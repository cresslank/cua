//! Reap children launched by Linux app tools without blocking later exits.

use std::{
    process::Child,
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError},
        Arc, OnceLock,
    },
    thread,
    time::Duration,
};

use anyhow::{anyhow, Context};

const REAP_INTERVAL: Duration = Duration::from_millis(100);
const REAPER_QUEUE_CAPACITY: usize = 256;
const REAPER_BATCH_SIZE: usize = 32;
static REAPER: OnceLock<Result<ReaperHandle, String>> = OnceLock::new();

struct ReaperHandle {
    sender: SyncSender<Child>,
    in_flight: Arc<AtomicUsize>,
}

/// Reserve one globally bounded child slot before causing any launch side
/// effect. Dropping an unused permit releases the slot.
pub(super) struct AppLaunchPermit {
    reserved: bool,
}

impl AppLaunchPermit {
    pub(super) fn reserve() -> anyhow::Result<Self> {
        let reaper = REAPER
            .get_or_init(start_reaper)
            .as_ref()
            .map_err(|error| anyhow!("app child reaper is unavailable: {error}"))?;
        if !reserve_slot(&reaper.in_flight) {
            return Err(anyhow!("app child reaper is at capacity"));
        }
        Ok(Self { reserved: true })
    }

    pub(super) fn guard(mut self, child: Child) -> AppChildGuard {
        self.reserved = false;
        AppChildGuard { child: Some(child) }
    }
}

impl Drop for AppLaunchPermit {
    fn drop(&mut self) {
        if self.reserved {
            if let Some(Ok(reaper)) = REAPER.get() {
                release_slot(&reaper.in_flight);
            }
        }
    }
}

/// Own a launched child until explicit handoff. Drop still transfers ownership,
/// so cancellation, unwinding, and early returns cannot lose the child handle.
pub(super) struct AppChildGuard {
    child: Option<Child>,
}

impl AppChildGuard {
    pub(super) fn id(&self) -> u32 {
        self.child.as_ref().expect("guard always owns child").id()
    }

    pub(super) fn handoff(mut self) -> anyhow::Result<u32> {
        submit_reserved(self.child.take().expect("guard always owns child"))
    }
}

impl Drop for AppChildGuard {
    fn drop(&mut self) {
        if let Some(child) = self.child.take() {
            let _ = submit_reserved(child);
        }
    }
}

/// Convenience path for tests and already-spawned callers. Production launch
/// paths reserve first via [`AppLaunchPermit`] so capacity fails before spawn.
#[cfg(test)]
pub(super) fn submit(mut child: Child) -> anyhow::Result<u32> {
    let permit = match AppLaunchPermit::reserve() {
        Ok(permit) => permit,
        Err(error) => {
            terminate_and_wait(&mut child);
            return Err(error);
        }
    };
    permit.guard(child).handoff()
}

fn submit_reserved(child: Child) -> anyhow::Result<u32> {
    let pid = child.id();
    let reaper = REAPER
        .get()
        .and_then(|result| result.as_ref().ok())
        .expect("reserved slot requires initialized app reaper");
    match reaper.sender.try_send(child) {
        Ok(()) => Ok(pid),
        Err(TrySendError::Full(mut child)) => {
            release_slot(&reaper.in_flight);
            terminate_and_wait(&mut child);
            Err(anyhow!(
                "app child reaper queue is full; terminated launched child"
            ))
        }
        Err(TrySendError::Disconnected(mut child)) => {
            release_slot(&reaper.in_flight);
            terminate_and_wait(&mut child);
            Err(anyhow!("app child reaper stopped unexpectedly"))
        }
    }
}

fn start_reaper() -> Result<ReaperHandle, String> {
    let (sender, receiver) = mpsc::sync_channel(REAPER_QUEUE_CAPACITY);
    let in_flight = Arc::new(AtomicUsize::new(0));
    let reaper_in_flight = Arc::clone(&in_flight);
    thread::Builder::new()
        .name("cua-app-reaper".to_owned())
        .spawn(move || run_reaper(receiver, reaper_in_flight))
        .context("spawning app child reaper")
        .map_err(|error| format!("{error:#}"))?;
    Ok(ReaperHandle { sender, in_flight })
}

#[allow(
    clippy::zombie_processes,
    reason = "ECHILD proves an externally consumed child must be dropped without another numeric-PID wait"
)]
fn run_reaper(receiver: Receiver<Child>, in_flight: Arc<AtomicUsize>) {
    let mut children = Vec::new();

    loop {
        let received = if children.is_empty() {
            receiver.recv().map_err(|_| RecvTimeoutError::Disconnected)
        } else {
            receiver.recv_timeout(REAP_INTERVAL)
        };

        match received {
            Ok(child) => children.push(child),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        for _ in 1..REAPER_BATCH_SIZE {
            match receiver.try_recv() {
                Ok(child) => children.push(child),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }

        let mut index = 0;
        while index < children.len() {
            let pid = children[index].id();
            match children[index].try_wait() {
                Ok(Some(status)) => {
                    tracing::debug!(pid, %status, "reaped launched app child");
                    let mut child = children.swap_remove(index);
                    let _ = child.wait();
                    release_slot(&in_flight);
                }
                Ok(None) => index += 1,
                Err(error) if error.raw_os_error() == Some(libc::ECHILD) => {
                    tracing::debug!(pid, "launched app child is no longer waitable");
                    drop(children.swap_remove(index));
                    release_slot(&in_flight);
                }
                Err(error) => {
                    tracing::warn!(pid, %error, "could not poll launched app child");
                    index += 1;
                }
            }
        }
    }
}

fn reserve_slot(in_flight: &AtomicUsize) -> bool {
    in_flight
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
            (count < REAPER_QUEUE_CAPACITY).then_some(count + 1)
        })
        .is_ok()
}

fn release_slot(in_flight: &AtomicUsize) {
    let previous = in_flight.fetch_sub(1, Ordering::AcqRel);
    debug_assert!(previous > 0, "app child reaper slot underflow");
}

fn terminate_and_wait(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{path::PathBuf, process::Command, time::Instant};

    const AUTO_REAP_MODE: &str = "CUA_APP_REAPER_TEST_AUTO_REAP_MODE";

    fn proc_path(pid: u32) -> PathBuf {
        PathBuf::from(format!("/proc/{pid}"))
    }

    fn wait_until_reaped(pid: u32) -> bool {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if !proc_path(pid).exists() {
                return true;
            }
            thread::sleep(Duration::from_millis(25));
        }
        !proc_path(pid).exists()
    }

    #[test]
    fn capacity_reservation_is_globally_bounded() {
        let count = AtomicUsize::new(REAPER_QUEUE_CAPACITY);
        assert!(!reserve_slot(&count));
        assert_eq!(count.load(Ordering::Acquire), REAPER_QUEUE_CAPACITY);
    }

    #[test]
    fn dropped_guard_hands_child_to_reaper() {
        let permit = AppLaunchPermit::reserve().expect("reserve guarded child slot");
        let child = Command::new("true").spawn().expect("spawn guarded child");
        let pid = child.id();
        drop(permit.guard(child));
        assert!(
            wait_until_reaped(pid),
            "dropped guard did not reap child {pid}"
        );
    }

    #[test]
    fn auto_reap_subprocess_helper() {
        let Some(mode) = std::env::var_os(AUTO_REAP_MODE) else {
            return;
        };
        let mode = mode.to_string_lossy();
        if mode == "ignore" {
            unsafe {
                libc::signal(libc::SIGCHLD, libc::SIG_IGN);
            }
        } else if mode == "no-cldwait" {
            let mut action = unsafe { std::mem::zeroed::<libc::sigaction>() };
            action.sa_sigaction = libc::SIG_DFL;
            action.sa_flags = libc::SA_NOCLDWAIT;
            unsafe {
                libc::sigemptyset(&mut action.sa_mask);
                assert_eq!(
                    libc::sigaction(libc::SIGCHLD, &action, std::ptr::null_mut()),
                    0
                );
            }
        }

        let child = Command::new("true")
            .spawn()
            .expect("spawn auto-reaped child");
        let pid = child.id();
        if mode == "prewait" {
            let mut status = 0;
            assert_eq!(
                unsafe { libc::waitpid(pid as i32, &mut status, 0) },
                pid as i32
            );
        }
        submit(child).expect("register auto-reaped child");

        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let in_flight = REAPER
                .get()
                .and_then(|result| result.as_ref().ok())
                .map(|reaper| reaper.in_flight.load(Ordering::Acquire))
                .unwrap_or(usize::MAX);
            if in_flight == 0 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "auto-reaped child {pid} retained a registry slot in mode {mode}"
            );
            thread::sleep(Duration::from_millis(25));
        }
        assert!(!proc_path(pid).exists());
    }

    #[test]
    fn auto_reaped_children_release_slots_without_second_wait() {
        let executable = std::env::current_exe().expect("current test executable");
        for mode in ["ignore", "no-cldwait", "prewait"] {
            let status = Command::new(&executable)
                .arg("--exact")
                .arg("tools::child_reaper::tests::auto_reap_subprocess_helper")
                .arg("--nocapture")
                .env(AUTO_REAP_MODE, mode)
                .status()
                .expect("run auto-reap helper subprocess");
            assert!(status.success(), "auto-reap mode {mode} failed");
        }
    }

    #[test]
    fn later_exit_is_reaped_while_earlier_child_is_running() {
        let long_lived = Command::new("sh")
            .args(["-c", "exec sleep 30"])
            .spawn()
            .expect("spawn long-lived child");
        let long_pid = submit(long_lived).expect("register long-lived child");

        let short_lived = Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("spawn short-lived child");
        let short_pid = submit(short_lived).expect("register short-lived child");

        assert!(
            wait_until_reaped(short_pid),
            "short-lived child {short_pid} was not reaped while {long_pid} was running"
        );

        // The registry owns the Child handle, so terminate by pid and prove it
        // also performs the final wait rather than leaving the test fixture.
        let killed = unsafe { libc::kill(long_pid as i32, libc::SIGKILL) };
        assert_eq!(killed, 0, "kill long-lived child {long_pid}");
        assert!(
            wait_until_reaped(long_pid),
            "long-lived child {long_pid} was not reaped after SIGKILL"
        );
    }
}
