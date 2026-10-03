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
            //
            // Exit waits for the fixed set of guards already handed off, i.e.
            // actions that had returned before exit began; synchronous mode
            // finished those guards before returning, so none is shortened.
            // An action still running when exit begins loses its guard at
            // termination, exactly as the synchronous wait did. Exit must not
            // wait for running actions: their futures may need the executor
            // thread that called exit (current-thread hosts), which deadlocks.
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

/// Drain the leases present at entry without shortening any guard. Concurrent
/// runtimes may keep admitting work; their later leases cannot extend this wait.
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
    last_id: u64,
    leases: Vec<Held<T>>,
    stopped: bool,
}

struct Held<T> {
    id: u64,
    deadline: Instant,
    _lease: T,
}

impl<T> State<T> {
    fn hold(
        &mut self,
        lease: T,
        deadline: Instant,
        generation: u64,
        now: Instant,
    ) -> Result<(), T> {
        if generation != self.generation || deadline <= now {
            drop(lease);
            return Ok(());
        }
        if self.leases.len() == MAX_HELD {
            return Err(lease);
        }
        self.last_id = self
            .last_id
            .checked_add(1)
            .expect("focus lease ID exhausted");
        self.leases.push(Held {
            id: self.last_id,
            deadline,
            _lease: lease,
        });
        Ok(())
    }

    fn reap(&mut self, now: Instant) {
        self.leases.retain(|held| held.deadline > now);
    }

    fn pending_through(&self, last_id: u64) -> bool {
        self.leases.iter().any(|held| held.id <= last_id)
    }
}

struct Holder<T> {
    state: Arc<(Mutex<State<T>>, Condvar)>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// A shutdown barrier covers a fixed set, even while another runtime admits
/// new work. Capturing and waiting are separate so the boundary is explicit.
struct Drain<T> {
    state: Arc<(Mutex<State<T>>, Condvar)>,
    last_id: u64,
}

impl<T> Drain<T> {
    fn wait(self) {
        let mut state = self.state.0.lock().unwrap();
        while state.pending_through(self.last_id) {
            state = self.state.1.wait(state).unwrap();
        }
    }
}

impl<T: Send + 'static> Holder<T> {
    fn new() -> Self {
        let state = Arc::new((
            Mutex::new(State {
                generation: 0,
                last_id: 0,
                leases: Vec::new(),
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
                    state.reap(now);
                    wake.notify_all();
                    if state.stopped {
                        return;
                    }
                    state = match state.leases.iter().map(|held| held.deadline).min() {
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
        let result = state.hold(lease, deadline, generation, Instant::now());
        self.state.1.notify_all();
        result
    }

    fn cancel(&self) -> u64 {
        let mut state = self.state.0.lock().unwrap();
        state.generation = state.generation.wrapping_add(1);
        // Drop while locked: lease removal also joins any in-flight restore,
        // so cancellation returns before an intentional activation can begin.
        state.leases.clear();
        self.state.1.notify_all();
        state.generation
    }

    fn drain(&self) {
        self.begin_drain().wait();
    }

    fn begin_drain(&self) -> Drain<T> {
        Drain {
            state: self.state.clone(),
            last_id: self.state.0.lock().unwrap().last_id,
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
    deadlines: Mutex<std::collections::HashMap<i32, tokio::time::Instant>>,
}

static TEXT_FOCUS: OnceLock<TextFocus> = OnceLock::new();

impl TextFocus {
    /// PR #3490 (hyprcat): apply the configured WebKit settle to deferred input.
    fn record_settle(&self, pid: i32, settle: Duration) -> bool {
        settle.is_zero() || self.record(pid, tokio::time::Instant::now() + settle)
    }

    // A full map falls back to settling synchronously in the click. Never evict
    // another PID's pending settle or force unrelated PIDs to wait.
    fn record(&self, pid: i32, deadline: tokio::time::Instant) -> bool {
        let mut deadlines = self.deadlines.lock().unwrap();
        deadlines.retain(|_, at| *at > tokio::time::Instant::now());
        if deadlines.len() == MAX_HELD && !deadlines.contains_key(&pid) {
            return false;
        }
        deadlines
            .entry(pid)
            .and_modify(|at| *at = (*at).max(deadline))
            .or_insert(deadline);
        true
    }

    fn pending(&self, pid: Option<i32>) -> Option<tokio::time::Instant> {
        let mut deadlines = self.deadlines.lock().unwrap();
        deadlines.retain(|_, at| *at > tokio::time::Instant::now());
        match pid {
            Some(pid) => deadlines.get(&pid).copied(),
            None => deadlines.values().copied().max(),
        }
    }

    async fn wait(&self, pid: Option<i32>) {
        // Re-read after waking so a later click cannot have its entry removed
        // or its settle skipped by an older waiter.
        while let Some(deadline) = self.pending(pid) {
            tokio::time::sleep_until(deadline).await;
        }
    }
}

/// Called immediately after successful text-field AXPress. False requests the
/// configured synchronous settle (finite mode or bounded-map saturation).
pub(crate) fn defer_text_focus(pid: i32) -> bool {
    let settle = crate::input::pacing::webkit_settle();
    settle.is_zero()
        || (background()
            && TEXT_FOCUS
                .get_or_init(TextFocus::default)
                .record_settle(pid, settle))
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
        {
            // Supply time at the queue seam and hold the reaper's lock. Neither
            // admission nor assertions depend on scheduling before a deadline.
            let mut state = holder.state.0.lock().unwrap();
            let now = Instant::now();
            let deadline = now + Duration::from_secs(1);
            // A late action from before cancel is dropped, never reinserted.
            assert!(state.hold(Lease(drops.clone()), deadline, old, now).is_ok());
            assert_eq!(drops.load(Ordering::SeqCst), 3);
            let generation = state.generation;
            assert!(state
                .hold(Lease(drops.clone()), deadline, generation, now)
                .is_ok());
            state.reap(now + Duration::from_millis(40));
            assert_eq!(drops.load(Ordering::SeqCst), 3);
        }
        holder.cancel();
        assert_eq!(drops.load(Ordering::SeqCst), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn webkit_knob_drives_deferred_deadline_and_zero_skips_settle() {
        let focus = TextFocus::default();
        for raw in [None, Some("37"), Some("9000"), Some("0")] {
            let settle = cua_driver_core::input_pacing::WEBKIT_SETTLE
                .from_raw(raw, Duration::from_millis(800));
            let now = tokio::time::Instant::now();
            assert!(focus.record_settle(7, settle));
            assert_eq!(
                focus.pending(Some(7)),
                (!settle.is_zero()).then_some(now + settle)
            );
            tokio::time::advance(settle).await;
            focus.wait(Some(7)).await;
            assert_eq!(tokio::time::Instant::now(), now + settle);
            assert!(focus.deadlines.lock().unwrap().is_empty());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn text_focus_waits_only_for_the_addressed_pid_or_latest_global_deadline() {
        let focus = TextFocus::default();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(40);
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
        assert!(tokio::time::Instant::now() >= deadline);
        assert_eq!(focus.pending(Some(1)), None);
        focus.wait(None).await;
        assert!(tokio::time::Instant::now() >= deadline + Duration::from_millis(20));
        assert_eq!(focus.pending(None), None);
        assert!(focus.record(4, tokio::time::Instant::now() - Duration::from_millis(1)));
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
            assert!(focus.record(pid, deadline.into()));
        }
        let overflow = holder.hold(Lease(drops.clone()), deadline, holder.generation());
        assert!(
            overflow.is_err(),
            "caller must retain the excess lease synchronously"
        );
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        assert!(!focus.record(MAX_HELD as i32, deadline.into()));
        assert!(focus.record(0, (deadline + Duration::from_secs(1)).into()));
        assert_eq!(
            focus.pending(Some(0)),
            Some((deadline + Duration::from_secs(1)).into())
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
        let mut state = holder.state.0.lock().unwrap();
        let now = Instant::now();
        let deadline = now + Duration::from_millis(30);
        let generation = state.generation;
        assert!(state.hold(lease, deadline, generation, now).is_ok());
        assert!(active());
        // Exercise the worker's exact reaping operation with supplied time,
        // while excluding the real worker until both boundary checks finish.
        state.reap(deadline - Duration::from_nanos(1));
        assert!(active());
        state.reap(deadline);
        assert!(!active());
    }

    #[test]
    fn shutdown_drain_finishes_while_another_runtime_keeps_producing_leases() {
        // Drive the queue's clock explicitly, without a real-time reaper.
        // Admission, capture, waiting, and wake-up use the production paths.
        let holder = Holder {
            state: Arc::new((
                Mutex::new(State {
                    generation: 0,
                    last_id: 0,
                    leases: Vec::new(),
                    stopped: false,
                }),
                Condvar::new(),
            )),
            thread: None,
        };
        let drops = Arc::new(AtomicUsize::new(0));
        let now = Instant::now();
        let first_deadline = now + Duration::from_secs(1);
        assert!(holder
            .state
            .0
            .lock()
            .unwrap()
            .hold(Lease(drops.clone()), first_deadline, 0, now)
            .is_ok());
        // This is the boundary taken by the shutting-down runtime, after its
        // own admission closes. The second runtime remains active throughout.
        let drain = holder.begin_drain();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let (added_tx, added_rx) = std::sync::mpsc::channel();
        let (next_tx, next_rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let shutdown = scope.spawn(move || {
                drain.wait();
                done_tx.send(()).unwrap();
            });
            let holder = &holder;
            let drops = &drops;
            let producer = scope.spawn(move || {
                for _ in 0..8 {
                    assert!(holder
                        .state
                        .0
                        .lock()
                        .unwrap()
                        .hold(
                            Lease(drops.clone()),
                            first_deadline + Duration::from_secs(1),
                            0,
                            now
                        )
                        .is_ok());
                    added_tx.send(()).unwrap();
                    next_rx.recv().unwrap();
                }
            });
            added_rx.recv().unwrap();
            {
                let mut state = holder.state.0.lock().unwrap();
                state.reap(first_deadline);
                assert_eq!(drops.load(Ordering::SeqCst), 1);
                assert_eq!(state.leases.len(), 1);
                holder.state.1.notify_all();
            }
            // No later lease expires or is cancelled before this completes.
            let result = done_rx.recv_timeout(Duration::from_secs(5));
            for _ in 1..8 {
                next_tx.send(()).unwrap();
                added_rx.recv().unwrap();
            }
            next_tx.send(()).unwrap();
            producer.join().unwrap();
            assert_eq!(holder.state.0.lock().unwrap().leases.len(), 8);
            // Clean up before asserting so a broken drain cannot hang the test.
            holder.cancel();
            shutdown.join().unwrap();
            assert!(result.is_ok(), "new leases extended shutdown: {result:?}");
        });
    }
}
