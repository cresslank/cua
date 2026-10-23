//! Real OS-thread/TLS regressions; include as a #[cfg(test)] sibling module in
//! runtime/linux_shutdown.rs. No shipped injectors or private retirement types.
use super::{reap_finished, run_with_budget};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

const BUDGET: Duration = Duration::from_millis(100);
const RETURN_BOUND: Duration = Duration::from_secs(1);
const FIXTURE_BOUND: Duration = Duration::from_secs(8);

// Isolate process-global SDK ownership and /proc inventories from libtest peers.
// An outer process deadline also kills a broken synchronous join; Tokio timers
// cannot enforce that deadline when their current-thread executor is blocked.
fn child(test: &str) -> bool {
    const KEY: &str = "CUA_A4_TLS_REGRESSION_CHILD";
    let name = format!("{}::{test}", module_path!().split_once("::").unwrap().1);
    if std::env::var(KEY).ok().as_deref() == Some(&name) {
        return true;
    }
    let mut process = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &name, "--nocapture", "--test-threads=1"])
        .env(KEY, &name)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(25);
    while process.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            let _ = process.kill();
            let output = process.wait_with_output().unwrap();
            panic!(
                "TLS regression child hung: {name}\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let output = process.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "TLS regression child failed: {name}\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("1 passed"),
        "exact child filter ran no test: {name}"
    );
    false
}

fn timerless() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
}

fn options() -> crate::DriverHostOptions {
    crate::DriverHostOptions {
        cursor: cursor_overlay::CursorConfig {
            enabled: false,
            ..Default::default()
        },
        host_owns_permission_ux: false,
        host_bundle_id: None,
        claude_code_compatibility: false,
        prepare_desktop_environment: false,
        register_host_tools: None,
        authorization_host: None,
        activity_observer: None,
    }
}

// Kernel identities, not JoinHandle::is_finished or Rust ThreadId. The starttime
// distinguishes PID/TID reuse; stat comm is parenthesized and may contain spaces.
fn os_threads() -> BTreeMap<u32, (u64, String)> {
    let mut tasks = BTreeMap::new();
    for entry in std::fs::read_dir("/proc/self/task").expect("Linux /proc required") {
        let entry = entry.unwrap();
        let stat = match std::fs::read_to_string(entry.path().join("stat")) {
            Ok(stat) => stat,
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    || error.raw_os_error() == Some(libc::ESRCH) =>
            {
                continue;
            }
            Err(error) => panic!("read task stat: {error}"),
        };
        let tid = entry.file_name().to_string_lossy().parse().unwrap();
        let (prefix, fields) = stat.rsplit_once(") ").unwrap();
        let starttime = fields.split_whitespace().nth(19).unwrap().parse().unwrap();
        tasks.insert(
            tid,
            (starttime, prefix.split_once('(').unwrap().1.to_owned()),
        );
    }
    tasks
}

fn current_tid() -> u32 {
    std::fs::read_link("/proc/thread-self")
        .unwrap()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .parse()
        .unwrap()
}

fn await_no_new_threads(before: &BTreeMap<u32, (u64, String)>) {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        // This is only invoked AFTER every fixture gate has been released.
        reap_finished();
        let extra: BTreeMap<_, _> = os_threads()
            .into_iter()
            .filter(|(id, task)| before.get(id).map(|old| old.0) != Some(task.0))
            .collect();
        if extra.is_empty() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "shutdown left OS threads: {extra:?}"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

struct TlsHold {
    release: mpsc::Receiver<()>,
    entered: mpsc::Sender<u32>,
    exited: Arc<AtomicBool>,
    expired: Arc<AtomicBool>,
}
impl Drop for TlsHold {
    fn drop(&mut self) {
        let _ = self.entered.send(current_tid());
        if self.release.recv_timeout(FIXTURE_BOUND) == Err(mpsc::RecvTimeoutError::Timeout) {
            self.expired.store(true, Ordering::Release);
        }
        self.exited.store(true, Ordering::Release);
    }
}
thread_local! {
    static TLS_HOLD: std::cell::RefCell<Option<TlsHold>> = const { std::cell::RefCell::new(None) };
}

struct TlsGate {
    release: mpsc::Sender<()>,
    entered: mpsc::Receiver<u32>,
    exited: Arc<AtomicBool>,
    expired: Arc<AtomicBool>,
}
fn tls_gate() -> (TlsGate, TlsHold) {
    let (release, wait) = mpsc::channel();
    let (entered, ready) = mpsc::channel();
    let exited = Arc::new(AtomicBool::new(false));
    let expired = Arc::new(AtomicBool::new(false));
    (
        TlsGate {
            release,
            entered: ready,
            exited: exited.clone(),
            expired: expired.clone(),
        },
        TlsHold {
            release: wait,
            entered,
            exited,
            expired,
        },
    )
}
fn install_tls(hold: TlsHold) {
    TLS_HOLD.with(|slot| *slot.borrow_mut() = Some(hold));
}

#[test]
fn cleanup_shutdown_deadline_includes_tls_destruction() {
    if !child("cleanup_shutdown_deadline_includes_tls_destruction") {
        return;
    }
    let before = os_threads();
    let (gate, hold) = tls_gate();
    let (returned, result) = mpsc::channel();
    let caller = std::thread::spawn(move || {
        let rt = timerless();
        let started = Instant::now();
        rt.block_on(run_with_budget(move || install_tls(hold), BUDGET));
        returned.send(started.elapsed()).unwrap();
    });
    let tid = gate.entered.recv_timeout(FIXTURE_BOUND).unwrap();
    // Rendezvous is inside Drop, proving the closure has returned while the
    // actual kernel thread remains alive. No scheduling sleep establishes this.
    let bounded = result.recv_timeout(RETURN_BOUND);
    let still_in_tls = !gate.exited.load(Ordering::Acquire) && os_threads().contains_key(&tid);
    drop(gate.release); // Always release before asserting, including the red path.
    let elapsed = bounded
        .as_ref()
        .ok()
        .copied()
        .unwrap_or_else(|| result.recv_timeout(FIXTURE_BOUND).unwrap());
    caller.join().unwrap();
    await_no_new_threads(&before);
    assert!(
        !gate.expired.load(Ordering::Acquire),
        "TLS fixture timed out instead of being released"
    );
    assert!(
        bounded.is_ok(),
        "cleanup shutdown synchronously joined TLS past {RETURN_BOUND:?}: {elapsed:?}"
    );
    assert!(
        still_in_tls,
        "shutdown returned only after TLS/kernel retirement"
    );
    assert!(
        elapsed >= BUDGET,
        "closure completion was mistaken for true retirement: {elapsed:?}"
    );
    assert!(
        elapsed < RETURN_BOUND,
        "cleanup deadline was not bounded: {elapsed:?}"
    );
}

#[test]
fn cancelled_cleanup_does_not_join_tls_on_the_executor() {
    if !child("cancelled_cleanup_does_not_join_tls_on_the_executor") {
        return;
    }
    let before = os_threads();
    let (gate, hold) = tls_gate();
    let (cancel, cancelled) = mpsc::channel();
    let (release_body, body) = mpsc::channel::<()>();
    let (polled, ready) = mpsc::channel();
    let (returned, result) = mpsc::channel();
    let caller = std::thread::spawn(move || {
        use std::future::Future;
        let rt = timerless();
        let _entered = rt.enter();
        let mut work = Box::pin(run_with_budget(
            move || {
                install_tls(hold);
                let _ = body.recv_timeout(FIXTURE_BOUND);
            },
            Duration::from_secs(30),
        ));
        // Park after the first Pending poll. The closure body remains gated so
        // completion cannot race this poll into the normal-return Drop path.
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(work.as_mut().poll(&mut context).is_pending());
        polled.send(()).unwrap();
        cancelled.recv_timeout(FIXTURE_BOUND).unwrap();
        let started = Instant::now();
        drop(work); // The exact cancellation boundary, without another poll.
        returned.send(started.elapsed()).unwrap();
    });
    ready.recv_timeout(FIXTURE_BOUND).unwrap();
    drop(release_body);
    let tid = gate.entered.recv_timeout(FIXTURE_BOUND).unwrap();
    cancel.send(()).unwrap();
    let bounded = result.recv_timeout(RETURN_BOUND);
    let still_in_tls = !gate.exited.load(Ordering::Acquire) && os_threads().contains_key(&tid);
    drop(gate.release);
    let elapsed = bounded
        .as_ref()
        .ok()
        .copied()
        .unwrap_or_else(|| result.recv_timeout(FIXTURE_BOUND).unwrap());
    caller.join().unwrap();
    await_no_new_threads(&before);
    assert!(!gate.expired.load(Ordering::Acquire));
    assert!(
        bounded.is_ok(),
        "cancelling cleanup synchronously joined a TLS destructor: {elapsed:?}"
    );
    assert!(
        still_in_tls,
        "cancellation did not return until the held TLS destructor exited"
    );
    assert!(elapsed < RETURN_BOUND);
}

// Put a still-executing closure through the genuine timeout/handoff, THEN let
// it return into parked TLS. This reaches reap_finished independently of the
// broken shutdown Drop branch, without depending on RETIRING's representation.
fn retiring_tls() -> (TlsGate, u32) {
    let (gate, hold) = tls_gate();
    let (release_body, body) = mpsc::channel::<()>();
    let (entered, ready) = mpsc::channel();
    timerless().block_on(run_with_budget(
        move || {
            install_tls(hold);
            entered.send(()).unwrap();
            let _ = body.recv_timeout(FIXTURE_BOUND);
        },
        BUDGET,
    ));
    ready.recv_timeout(FIXTURE_BOUND).unwrap();
    drop(release_body);
    let tid = gate.entered.recv_timeout(FIXTURE_BOUND).unwrap();
    (gate, tid)
}

#[test]
fn cleanup_reap_never_joins_a_thread_in_tls() {
    if !child("cleanup_reap_never_joins_a_thread_in_tls") {
        return;
    }
    let before = os_threads();
    let (gate, tid) = retiring_tls();
    let (returned, result) = mpsc::channel();
    let caller = std::thread::spawn(move || {
        reap_finished();
        returned.send(()).unwrap();
    });
    let bounded = result.recv_timeout(RETURN_BOUND);
    let still_in_tls = !gate.exited.load(Ordering::Acquire) && os_threads().contains_key(&tid);
    drop(gate.release);
    if bounded.is_err() {
        result.recv_timeout(FIXTURE_BOUND).unwrap();
    }
    caller.join().unwrap();
    await_no_new_threads(&before);
    assert!(!gate.expired.load(Ordering::Acquire));
    assert!(bounded.is_ok(), "reap_finished synchronously joined TLS");
    assert!(still_in_tls, "reaping only returned after TLS exited");
}

#[test]
fn sdk_construction_never_joins_prior_cleanup_tls() {
    if !child("sdk_construction_never_joins_prior_cleanup_tls") {
        return;
    }
    let before = os_threads();
    let (gate, tid) = retiring_tls();
    let (returned, result) = mpsc::channel();
    let caller = std::thread::spawn(move || {
        returned
            .send(crate::CuaDriver::try_create_for_host(options()))
            .unwrap();
    });
    let bounded = result.recv_timeout(RETURN_BOUND);
    let still_in_tls = !gate.exited.load(Ordering::Acquire) && os_threads().contains_key(&tid);
    drop(gate.release);
    let driver = match bounded {
        Ok(driver) => (true, driver),
        Err(_) => (false, result.recv_timeout(FIXTURE_BOUND).unwrap()),
    };
    caller.join().unwrap();
    // This fake-free cleanup closure owns no native lease; the normal public
    // constructor must succeed. Actual native-generation fencing is tested in
    // platform-linux's existing eventual-retirement test, not bypassed here.
    let (bounded, driver) = (driver.0, driver.1.unwrap());
    timerless().block_on(driver.shutdown()).unwrap();
    drop(driver);
    await_no_new_threads(&before);
    assert!(!gate.expired.load(Ordering::Acquire));
    assert!(
        bounded,
        "public SDK constructor synchronously reaped prior cleanup TLS"
    );
    assert!(
        still_in_tls,
        "construction only returned after old TLS exited"
    );
}

#[test]
fn ordinary_sdk_shutdown_leaves_no_new_os_threads() {
    if !child("ordinary_sdk_shutdown_leaves_no_new_os_threads") {
        return;
    }
    let before = os_threads();
    for cycle in 0..12 {
        // The construction/public-shutdown boundary runs on a timerless host.
        // Drop its lazily started blocking pool before counting SDK-owned leaks.
        let rt = timerless();
        let driver = crate::CuaDriver::try_create_for_host(options()).unwrap();
        rt.block_on(driver.shutdown()).unwrap();
        assert!(
            !driver.is_available(),
            "SDK still available after cycle {cycle}"
        );
        drop(driver);
        drop(rt);
        await_no_new_threads(&before);
    }
}
