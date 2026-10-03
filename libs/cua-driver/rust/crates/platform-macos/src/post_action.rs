//! Deferred focus protection for long-lived hosts. Finite hosts default to
//! synchronous observation. SDK runtimes enable background mode; finite
//! embedders can pin synchronous mode before SDK creation. CLI call is
//! service-backed. One reaper owns at most MAX_HELD leases, independently of Tokio.
//! Saturation falls back to the existing synchronous wait, never an early drop.

use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::focus_steal::SuppressionLease;

const MAX_HELD: usize = 256;
static BACKGROUND: OnceLock<bool> = OnceLock::new();
static HOLDER: OnceLock<Holder<SuppressionLease>> = OnceLock::new();

/// Pin finite in-process commands to synchronous mode, before SDK creation.
pub fn keep_synchronous() {
    BACKGROUND.get_or_init(|| false);
}

/// SDK runtimes, including serve and direct MCP, enable deferred protection.
/// The immutable process choice prevents one runtime changing another's mode.
pub fn enable_background() {
    if *BACKGROUND.get_or_init(|| true) {
        HOLDER.get_or_init(|| {
            let holder = Holder::new();
            // Normal return and process::exit both run atexit callbacks. The
            // reaper is independent of Tokio, which may already be torn down.
            extern "C" fn drain_before_exit() {
                drain();
            }
            assert_eq!(unsafe { libc::atexit(drain_before_exit) }, 0);
            holder
        });
        cua_driver_core::action_boundary::install_before_mutation_hook(cancel);
    }
}

pub(crate) fn background() -> bool {
    BACKGROUND.get().copied().unwrap_or(false)
}

pub(crate) fn generation() -> u64 {
    HOLDER.get().map_or(0, Holder::generation)
}

/// Cancel only deferred leases, never the guards of a currently running action.
pub(crate) fn cancel() {
    if let Some(holder) = HOLDER.get() {
        holder.cancel();
    }
}

/// Drain before an orderly host exit. This does not shorten any guard.
pub fn drain() {
    if let Some(holder) = HOLDER.get() {
        holder.drain();
    }
}

pub(crate) fn hold(
    lease: SuppressionLease,
    deadline: Instant,
    generation: u64,
) -> Result<(), SuppressionLease> {
    match HOLDER.get().filter(|_| background()) {
        Some(holder) => holder.hold(lease, deadline, generation),
        None => Err(lease),
    }
}

struct State<T> {
    generation: u64,
    leases: Vec<(Instant, T)>,
    stopped: bool,
}

struct Holder<T> {
    state: Arc<(Mutex<State<T>>, Condvar)>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl<T: Send + 'static> Holder<T> {
    fn new() -> Self {
        let state = Arc::new((
            Mutex::new(State {
                generation: 0,
                leases: Vec::<(Instant, T)>::new(),
                stopped: false,
            }),
            Condvar::new(),
        ));
        let worker = state.clone();
        let thread = std::thread::Builder::new()
            .name("cua-focus-guard-reaper".into())
            .spawn(move || {
                let (lock, wake) = &*worker;
                let mut state = lock.lock().unwrap();
                loop {
                    // Recompute from the current queue after every wake. No
                    // stale timer ever acts on a newer generation's leases.
                    let now = Instant::now();
                    state.leases.retain(|(deadline, _)| *deadline > now);
                    wake.notify_all();
                    if state.stopped {
                        return;
                    }
                    state = match state.leases.iter().map(|(at, _)| *at).min() {
                        Some(at) => {
                            wake.wait_timeout(state, at.saturating_duration_since(now))
                                .unwrap()
                                .0
                        }
                        None => wake.wait(state).unwrap(),
                    };
                }
            })
            .expect("focus guard reaper");
        Self {
            state,
            thread: Some(thread),
        }
    }

    fn generation(&self) -> u64 {
        self.state.0.lock().unwrap().generation
    }

    fn hold(&self, lease: T, deadline: Instant, generation: u64) -> Result<(), T> {
        let mut state = self.state.0.lock().unwrap();
        if generation != state.generation || deadline <= Instant::now() {
            drop(lease);
            return Ok(());
        }
        if state.leases.len() == MAX_HELD {
            return Err(lease);
        }
        state.leases.push((deadline, lease));
        self.state.1.notify_all();
        Ok(())
    }

    fn cancel(&self) -> u64 {
        let mut state = self.state.0.lock().unwrap();
        state.generation = state.generation.wrapping_add(1);
        // Drop while locked: cancel returns only after all leases are released.
        state.leases.clear();
        self.state.1.notify_all();
        state.generation
    }

    fn drain(&self) {
        let mut state = self.state.0.lock().unwrap();
        while !state.leases.is_empty() {
            state = self.state.1.wait(state).unwrap();
        }
    }
}

impl<T> Drop for Holder<T> {
    fn drop(&mut self) {
        {
            let mut state = self.state.0.lock().unwrap();
            state.stopped = true;
            state.leases.clear();
            self.state.1.notify_all();
        }
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

/// Pending WebKit focus settles are independent of guard cancellation. A
/// keyboard action must still wait after its own admission cancelled guards.
#[derive(Default)]
pub(crate) struct TextFocus {
    deadlines: Mutex<std::collections::HashMap<i32, Instant>>,
}

static TEXT_FOCUS: OnceLock<TextFocus> = OnceLock::new();

impl TextFocus {
    // A full map falls back to settling synchronously in the click. Never evict
    // another PID's pending settle or force unrelated PIDs to wait.
    fn record(&self, pid: i32, deadline: Instant) -> bool {
        let mut deadlines = self.deadlines.lock().unwrap();
        deadlines.retain(|_, at| *at > Instant::now());
        if deadlines.len() == MAX_HELD && !deadlines.contains_key(&pid) {
            return false;
        }
        deadlines
            .entry(pid)
            .and_modify(|at| *at = (*at).max(deadline))
            .or_insert(deadline);
        true
    }

    fn pending(&self, pid: Option<i32>) -> Option<Instant> {
        let mut deadlines = self.deadlines.lock().unwrap();
        deadlines.retain(|_, at| *at > Instant::now());
        match pid {
            Some(pid) => deadlines.get(&pid).copied(),
            None => deadlines.values().copied().max(),
        }
    }

    async fn wait(&self, pid: Option<i32>) {
        // Re-read after waking so a later click cannot have its entry removed
        // or its settle skipped by an older waiter.
        while let Some(deadline) = self.pending(pid) {
            tokio::time::sleep(deadline.saturating_duration_since(Instant::now())).await;
        }
    }
}

/// Called immediately after successful text-field AXPress. False requests the
/// original synchronous 800ms settle (finite mode or bounded-map saturation).
pub(crate) fn defer_text_focus(pid: i32) -> bool {
    background()
        && TEXT_FOCUS
            .get_or_init(TextFocus::default)
            .record(pid, Instant::now() + Duration::from_millis(800))
}

pub(crate) async fn wait_for_text_focus(pid: Option<i32>) {
    if let Some(focus) = TEXT_FOCUS.get() {
        focus.wait(pid).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Lease(Arc<AtomicUsize>);
    impl Drop for Lease {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn reaper_releases_without_another_call_and_cancel_cannot_reap_new_leases() {
        let holder = Holder::new();
        let drops = Arc::new(AtomicUsize::new(0));
        let old = holder.generation();
        assert!(holder
            .hold(
                Lease(drops.clone()),
                Instant::now() + Duration::from_millis(20),
                old
            )
            .is_ok());
        holder.drain();
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        let old_deadline = Instant::now() + Duration::from_millis(20);
        assert!(holder.hold(Lease(drops.clone()), old_deadline, old).is_ok());
        holder.cancel();
        assert_eq!(drops.load(Ordering::SeqCst), 2);
        // A late action from before cancel is dropped, never reinserted.
        assert!(holder.hold(Lease(drops.clone()), old_deadline, old).is_ok());
        assert_eq!(drops.load(Ordering::SeqCst), 3);
        assert!(holder
            .hold(
                Lease(drops.clone()),
                Instant::now() + Duration::from_secs(1),
                holder.generation()
            )
            .is_ok());
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(drops.load(Ordering::SeqCst), 3);
        holder.cancel();
        assert_eq!(drops.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn text_focus_waits_only_for_the_addressed_pid_or_latest_global_deadline() {
        let focus = TextFocus::default();
        let deadline = Instant::now() + Duration::from_millis(40);
        assert!(focus.record(1, deadline));
        assert!(focus.record(2, deadline + Duration::from_millis(20)));
        assert_eq!(
            focus.pending(None),
            Some(deadline + Duration::from_millis(20))
        );
        // Poll once: a different PID has no timer, while this PID is pending.
        use std::future::Future;
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        assert!(std::pin::pin!(focus.wait(Some(3))).poll(&mut cx).is_ready());
        assert!(std::pin::pin!(focus.wait(Some(1)))
            .poll(&mut cx)
            .is_pending());
        focus.wait(Some(1)).await;
        assert!(Instant::now() >= deadline);
        assert_eq!(focus.pending(Some(1)), None);
        focus.wait(None).await;
        assert!(Instant::now() >= deadline + Duration::from_millis(20));
        assert_eq!(focus.pending(None), None);
        assert!(focus.record(4, Instant::now() - Duration::from_millis(1)));
        assert!(std::pin::pin!(focus.wait(Some(4))).poll(&mut cx).is_ready());
    }

    #[test]
    fn bounded_holder_and_text_map_preserve_existing_entries_on_saturation() {
        let holder = Holder::new();
        let drops = Arc::new(AtomicUsize::new(0));
        let focus = TextFocus::default();
        let deadline = Instant::now() + Duration::from_secs(10);
        for pid in 0..MAX_HELD as i32 {
            assert!(holder
                .hold(Lease(drops.clone()), deadline, holder.generation())
                .is_ok());
            assert!(focus.record(pid, deadline));
        }
        let overflow = holder.hold(Lease(drops.clone()), deadline, holder.generation());
        assert!(
            overflow.is_err(),
            "caller must retain the excess lease synchronously"
        );
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        assert!(!focus.record(MAX_HELD as i32, deadline));
        assert!(focus.record(0, deadline + Duration::from_secs(1)));
        assert_eq!(
            focus.pending(Some(0)),
            Some(deadline + Duration::from_secs(1))
        );
        holder.cancel();
        assert_eq!(drops.load(Ordering::SeqCst), MAX_HELD);
        drop(overflow);
        assert_eq!(drops.load(Ordering::SeqCst), MAX_HELD + 1);
    }

    #[test]
    fn real_suppression_entry_survives_until_reaper_deadline() {
        let holder = Holder::new();
        let (lease, active) = SuppressionLease::isolated_for_test();
        let deadline = Instant::now() + Duration::from_millis(30);
        assert!(holder.hold(lease, deadline, holder.generation()).is_ok());
        assert!(active());
        holder.drain();
        assert!(Instant::now() >= deadline);
        assert!(!active());
    }
}
