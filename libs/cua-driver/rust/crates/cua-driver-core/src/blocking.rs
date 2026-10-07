//! Admission control for native blocking calls.
//!
//! Timed-out platform work cannot always be cancelled (AX, UIA, AT-SPI, and
//! compositor IPC can remain inside an OS call after their async caller has
//! returned). Admit only a fixed number across Tokio's blocking pool and
//! synchronous deadline workers. The permit is moved into the blocking closure,
//! so dropping or timing out the waiter does not release capacity before the native call
//! actually exits. Unrelated SDK supervision and service shutdown work retain
//! access to the rest of Tokio's blocking pool.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

const MAX_CONCURRENT_NATIVE_CALLS: usize = 32;

/// Ownership of the dispatch grant across a native thread or command queue.
/// Capture in the caller, before spawning/enqueuing, and retain through input,
/// payload destruction and cleanup. This is not admission for another action.
/// A long-lived worker must capture per command, never once for its lifetime.
#[derive(Clone, Default)]
pub struct ActionLeaseScope(Option<Arc<crate::action_lease::ActionLease>>);

impl ActionLeaseScope {
    pub fn capture() -> Self {
        Self(
            crate::tool::DISPATCH_ACTION_LEASE
                .try_with(Clone::clone)
                .ok()
                .flatten(),
        )
    }

    /// Propagate ownership to any further native continuations started here.
    pub fn run<R>(&self, function: impl FnOnce() -> R) -> R {
        crate::tool::DISPATCH_ACTION_LEASE.sync_scope(self.0.clone(), function)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum NativeDeadlineError {
    #[error("native blocking-call capacity is exhausted")]
    Busy,
    #[error("could not start native worker: {0}")]
    Spawn(std::io::Error),
    #[error("native worker deadline expired or its waiter was cancelled")]
    TimedOut,
    #[error("native worker panicked")]
    Panicked,
}

/// One absolute deadline shared by the waiter and native worker. Native calls
/// cannot be interrupted; check again after each blocking read and immediately
/// before each mutation to avoid starting more work after cancellation.
pub struct NativeDeadline {
    expires: std::time::Instant,
    cancelled: Arc<AtomicBool>,
}

impl NativeDeadline {
    pub fn check(&self) -> Result<(), NativeDeadlineError> {
        if self.cancelled.load(Ordering::Acquire) || std::time::Instant::now() >= self.expires {
            Err(NativeDeadlineError::TimedOut)
        } else {
            Ok(())
        }
    }
}

struct CancelNativeWaiter(Arc<AtomicBool>);

impl Drop for CancelNativeWaiter {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// Bound synchronous native work without releasing its admission or authority
/// when the waiter times out. `keepalive` must own any mutation permits needed
/// by the worker. Nested calls share the ordinary native-call capacity and
/// refuse immediately when full, rather than waiting on their parent's slot.
pub fn run_native_with_deadline<K, F, R>(
    timeout: std::time::Duration,
    keepalive: K,
    function: F,
) -> Result<R, NativeDeadlineError>
where
    K: Send + 'static,
    F: FnOnce(&NativeDeadline) -> R + Send + 'static,
    R: Send + 'static,
{
    run_native_with_limit(native_call_limit(), timeout, keepalive, function)
}

pub(crate) fn run_native_with_limit<K, F, R>(
    limit: Arc<tokio::sync::Semaphore>,
    timeout: std::time::Duration,
    keepalive: K,
    function: F,
) -> Result<R, NativeDeadlineError>
where
    K: Send + 'static,
    F: FnOnce(&NativeDeadline) -> R + Send + 'static,
    R: Send + 'static,
{
    let deadline = NativeDeadline {
        expires: std::time::Instant::now() + timeout,
        cancelled: Arc::new(AtomicBool::new(false)),
    };
    let waiter = CancelNativeWaiter(deadline.cancelled.clone());
    let remaining = deadline.expires;
    let admission = limit
        .try_acquire_owned()
        .map_err(|_| NativeDeadlineError::Busy)?;
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let action_lease = ActionLeaseScope::capture();
    std::thread::Builder::new()
        .name("cua-native-deadline".into())
        .spawn(move || {
            let outcome = action_lease.run(|| {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    deadline.check()?;
                    Ok(function(&deadline))
                }));
                // Payload Drop can start native cleanup; keep and propagate
                // execution ownership until that cleanup has been handed off.
                drop(keepalive);
                outcome
            });
            drop(action_lease);
            drop(admission);
            let _ = sender.send(outcome);
        })
        .map_err(NativeDeadlineError::Spawn)?;
    let outcome =
        receiver.recv_timeout(remaining.saturating_duration_since(std::time::Instant::now()));
    drop(waiter);
    // A ready result can be delivered after the deadline if this waiter was
    // descheduled. Do not start a caller's mutation body on that late result.
    if std::time::Instant::now() >= remaining {
        return Err(NativeDeadlineError::TimedOut);
    }
    match outcome {
        Ok(Ok(value)) => value,
        Ok(Err(_)) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            Err(NativeDeadlineError::Panicked)
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(NativeDeadlineError::TimedOut),
    }
}

pub(crate) fn native_call_limit() -> Arc<tokio::sync::Semaphore> {
    static LIMIT: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
    LIMIT
        .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_NATIVE_CALLS)))
        .clone()
}

/// Spawn one potentially uncancellable native call under process-wide
/// admission control. The returned handle has the same output shape as
/// `tokio::task::spawn_blocking`, so callers retain ordinary join and panic
/// semantics.
pub fn spawn<F, R>(function: F) -> tokio::task::JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    spawn_with_limit(native_call_limit(), function)
}

fn spawn_with_limit<F, R>(
    limit: Arc<tokio::sync::Semaphore>,
    function: F,
) -> tokio::task::JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    let action_lease = ActionLeaseScope::capture();
    tokio::spawn(async move {
        let permit = limit
            .acquire_owned()
            .await
            .expect("native blocking-call admission semaphore closed");
        match tokio::task::spawn_blocking(move || {
            let _permit = permit;
            action_lease.run(function)
        })
        .await
        {
            Ok(value) => value,
            Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
            Err(error) => panic!("native blocking task was cancelled by runtime shutdown: {error}"),
        }
    })
}

#[derive(Debug)]
pub enum BoundedSyncCallError {
    Busy,
    Spawn(std::io::Error),
    TimedOut,
    Panicked,
}

impl std::fmt::Display for BoundedSyncCallError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy => formatter.write_str("another bounded browser cleanup is still active"),
            Self::Spawn(error) => write!(
                formatter,
                "could not start bounded browser cleanup: {error}"
            ),
            Self::TimedOut => formatter.write_str("bounded browser cleanup timed out"),
            Self::Panicked => formatter.write_str("bounded browser cleanup worker panicked"),
        }
    }
}

impl std::error::Error for BoundedSyncCallError {}

struct SyncCallLease(&'static AtomicBool);

impl Drop for SyncCallLease {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

fn run_bounded_sync_with_gate<F, R>(
    gate: &'static AtomicBool,
    timeout: std::time::Duration,
    function: F,
) -> Result<R, BoundedSyncCallError>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    gate.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .map_err(|_| BoundedSyncCallError::Busy)?;
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let action_lease = ActionLeaseScope::capture();
    if let Err(error) = std::thread::Builder::new()
        .name("cua-browser-cleanup".into())
        .spawn(move || {
            let lease = SyncCallLease(gate);
            let outcome = action_lease
                .run(|| std::panic::catch_unwind(std::panic::AssertUnwindSafe(function)));
            drop(action_lease);
            drop(lease);
            let _ = sender.send(outcome);
        })
    {
        gate.store(false, Ordering::Release);
        return Err(BoundedSyncCallError::Spawn(error));
    }
    match receiver.recv_timeout(timeout) {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(_)) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            Err(BoundedSyncCallError::Panicked)
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(BoundedSyncCallError::TimedOut),
    }
}

/// Run synchronous browser cleanup on one lifetime-bounded worker. UIA and
/// AT-SPI calls may ignore cancellation; a timed-out worker therefore retains
/// the process-wide cleanup gate until it really exits. Callers fail quickly
/// instead of creating unbounded detached workers or blocking forever.
pub fn run_browser_cleanup<F, R>(
    timeout: std::time::Duration,
    function: F,
) -> Result<R, BoundedSyncCallError>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    static ACTIVE: AtomicBool = AtomicBool::new(false);
    run_bounded_sync_with_gate(&ACTIVE, timeout, function)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Condvar, Mutex,
    };

    #[tokio::test]
    async fn timed_out_native_continuations_retain_the_dispatch_lease() {
        use crate::action_lease::{ActionLeaseTable, LeaseRequest};
        use crate::tool::DISPATCH_ACTION_LEASE;
        use std::time::Duration;

        for cleanup in [false, true] {
            let table = ActionLeaseTable::new();
            let lease = table
                .acquire_for_dispatch(LeaseRequest::desktop_raw(None, Duration::ZERO))
                .await
                .unwrap();
            let (started, running) = std::sync::mpsc::channel();
            let (release, resume) = std::sync::mpsc::channel();
            DISPATCH_ACTION_LEASE
                .scope(Some(lease), async {
                    spawn(move || {
                        let work = move || {
                            started.send(()).unwrap();
                            let _ = resume.recv_timeout(Duration::from_secs(10));
                        };
                        if cleanup {
                            let gate = Box::leak(Box::new(AtomicBool::new(false)));
                            assert!(matches!(
                                run_bounded_sync_with_gate(gate, Duration::from_secs(1), work),
                                Err(BoundedSyncCallError::TimedOut)
                            ));
                        } else {
                            assert!(matches!(
                                run_native_with_deadline(
                                    Duration::from_secs(1),
                                    (),
                                    move |_| work()
                                ),
                                Err(NativeDeadlineError::TimedOut)
                            ));
                        }
                    })
                    .await
                    .unwrap();
                })
                .await;
            running.recv_timeout(Duration::from_secs(2)).unwrap();
            let blocked = table
                .acquire(LeaseRequest::desktop_raw(None, Duration::from_millis(25)))
                .await;
            release.send(()).unwrap();
            assert_eq!(
                blocked
                    .expect_err("nested worker retains input after both waiters exit")
                    .code(),
                "input_busy"
            );
            table
                .acquire(LeaseRequest::desktop_raw(None, Duration::from_secs(2)))
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn queued_native_work_retains_the_dispatch_lease_before_admission() {
        use crate::action_lease::{ActionLeaseTable, LeaseRequest};
        use crate::tool::DISPATCH_ACTION_LEASE;
        use std::time::Duration;
        let table = ActionLeaseTable::new();
        let lease = table
            .acquire_for_dispatch(LeaseRequest::desktop_raw(None, Duration::ZERO))
            .await
            .unwrap();
        let limit = Arc::new(tokio::sync::Semaphore::new(0));
        let worker = DISPATCH_ACTION_LEASE
            .scope(Some(lease), async {
                spawn_with_limit(limit.clone(), || ())
            })
            .await;
        let blocked = table
            .acquire(LeaseRequest::desktop_raw(None, Duration::from_millis(25)))
            .await;
        limit.add_permits(1);
        assert_eq!(
            blocked
                .expect_err("queued work already owns the action lease")
                .code(),
            "input_busy"
        );
        worker.await.unwrap();
        table
            .acquire(LeaseRequest::desktop_raw(None, Duration::ZERO))
            .await
            .unwrap();
    }

    #[test]
    fn deadline_workers_retain_capacity_across_repeated_timeouts() {
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        let limit = Arc::new(tokio::sync::Semaphore::new(2));
        let entered = Arc::new(AtomicUsize::new(0));
        let mut releases = Vec::new();
        for _ in 0..2 {
            let (release, resume) = mpsc::sync_channel(1);
            let (started, start) = mpsc::sync_channel(1);
            let entered = entered.clone();
            let result =
                run_native_with_limit(limit.clone(), Duration::from_millis(100), (), move |_| {
                    entered.fetch_add(1, Ordering::SeqCst);
                    started.send(()).unwrap();
                    resume.recv_timeout(Duration::from_secs(5)).unwrap();
                });
            assert!(matches!(result, Err(NativeDeadlineError::TimedOut)));
            start.recv_timeout(Duration::from_secs(1)).unwrap();
            releases.push(release);
        }
        for _ in 0..64 {
            let result =
                run_native_with_limit(limit.clone(), Duration::from_millis(100), (), |_| {
                    panic!("exhausted admission must not start a worker")
                });
            assert!(matches!(result, Err(NativeDeadlineError::Busy)));
        }
        assert_eq!(entered.load(Ordering::SeqCst), 2);
        assert_eq!(limit.available_permits(), 0);
        for release in releases {
            release.send(()).unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while limit.available_permits() != 2 && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert_eq!(limit.available_permits(), 2);
        assert_eq!(
            run_native_with_limit(limit, Duration::from_secs(1), (), |_| 7).unwrap(),
            7
        );
    }

    #[test]
    fn stalled_mutation_resuming_after_retirement_keeps_permit_and_refuses_activation() {
        use crate::element_token::{RegistryError, TokenRegistry};
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        let registry = TokenRegistry::default();
        let payload = Arc::new(17_u64);
        let weak_payload = Arc::downgrade(&payload);
        let candidate = registry
            .prepare_current(std::process::id() as i32, 1, 1, payload)
            .unwrap();
        let identity = candidate.identity();
        registry.publish(candidate, Duration::ZERO).unwrap();
        let permit = Arc::new(registry.try_acquire_mutation(identity).unwrap());
        let weak_permit = Arc::downgrade(&permit);
        let limit = Arc::new(tokio::sync::Semaphore::new(1));
        let activations = Arc::new(AtomicUsize::new(0));
        let worker_activations = activations.clone();
        let (release, resume) = mpsc::sync_channel(1);
        let (started, start) = mpsc::sync_channel(1);
        let (finished, finish) = mpsc::sync_channel(1);
        let result = run_native_with_limit(
            limit.clone(),
            Duration::from_millis(100),
            permit.clone(),
            move |deadline| {
                started.send(()).unwrap();
                // Model a native read (e.g. the X11 server timestamp) stalled
                // immediately before the activation mutation boundary.
                resume.recv_timeout(Duration::from_secs(5)).unwrap();
                let allowed = deadline.check();
                if allowed.is_ok() {
                    worker_activations.fetch_add(1, Ordering::SeqCst);
                }
                finished.send(allowed).unwrap();
            },
        );
        assert!(matches!(result, Err(NativeDeadlineError::TimedOut)));
        start.recv_timeout(Duration::from_secs(1)).unwrap();
        drop(permit); // Indexed-click caller has returned on timeout.
        assert!(weak_permit.upgrade().is_some());
        assert_eq!(limit.available_permits(), 0);
        assert!(
            !registry.retire_generation(identity),
            "admitted worker prevents reaping"
        );
        assert_eq!(
            registry.try_acquire_mutation(identity).unwrap_err(),
            RegistryError::Stale
        );
        assert!(weak_payload.upgrade().is_some());
        release.send(()).unwrap();
        assert!(matches!(
            finish.recv_timeout(Duration::from_secs(2)).unwrap(),
            Err(NativeDeadlineError::TimedOut)
        ));
        assert_eq!(activations.load(Ordering::SeqCst), 0);
        let deadline = Instant::now() + Duration::from_secs(2);
        while limit.available_permits() == 0 && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert_eq!(limit.available_permits(), 1);
        assert!(weak_permit.upgrade().is_none());
        assert!(registry.reap_stale_generation(identity));
        assert!(weak_payload.upgrade().is_none());
    }

    #[test]
    fn deadline_panic_and_expiry_release_admission() {
        use std::time::Duration;
        let limit = Arc::new(tokio::sync::Semaphore::new(1));
        assert!(matches!(
            run_native_with_limit(limit.clone(), Duration::from_secs(1), (), |_| panic!(
                "native failure"
            )),
            Err(NativeDeadlineError::Panicked)
        ));
        assert_eq!(limit.available_permits(), 1);
        let deadline = NativeDeadline {
            expires: std::time::Instant::now(),
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        assert!(matches!(
            deadline.check(),
            Err(NativeDeadlineError::TimedOut)
        ));
        let deadline = NativeDeadline {
            expires: std::time::Instant::now() + Duration::from_secs(60),
            cancelled: Arc::new(AtomicBool::new(true)),
        };
        assert!(matches!(
            deadline.check(),
            Err(NativeDeadlineError::TimedOut)
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn permit_is_held_until_the_blocking_closure_exits() {
        let limit = Arc::new(tokio::sync::Semaphore::new(1));
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let entered = Arc::new(AtomicUsize::new(0));

        let first_gate = gate.clone();
        let first_entered = entered.clone();
        let first = spawn_with_limit(limit.clone(), move || {
            first_entered.fetch_add(1, Ordering::SeqCst);
            let (lock, ready) = &*first_gate;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = ready.wait(released).unwrap();
            }
        });

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while entered.load(Ordering::SeqCst) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        let second_entered = entered.clone();
        let second = spawn_with_limit(limit, move || {
            second_entered.fetch_add(1, Ordering::SeqCst);
        });
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        assert_eq!(entered.load(Ordering::SeqCst), 1);

        let (lock, ready) = &*gate;
        *lock.lock().unwrap() = true;
        ready.notify_all();
        first.await.unwrap();
        second.await.unwrap();
        assert_eq!(entered.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn native_panic_remains_a_join_error() {
        let handle = spawn_with_limit(Arc::new(tokio::sync::Semaphore::new(1)), || {
            panic!("native failure")
        });
        assert!(handle.await.unwrap_err().is_panic());
    }

    #[test]
    fn timed_out_sync_cleanup_holds_admission_until_the_worker_exits() {
        let gate = Box::leak(Box::new(AtomicBool::new(false)));
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let first =
            run_bounded_sync_with_gate(gate, std::time::Duration::from_millis(20), move || {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                7
            });
        assert!(matches!(first, Err(BoundedSyncCallError::TimedOut)));
        started_rx.recv().unwrap();
        assert!(matches!(
            run_bounded_sync_with_gate(gate, std::time::Duration::ZERO, || 8),
            Err(BoundedSyncCallError::Busy)
        ));

        release_tx.send(()).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while gate.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(!gate.load(Ordering::Acquire));
        assert_eq!(
            run_bounded_sync_with_gate(gate, std::time::Duration::from_secs(1), || 9).unwrap(),
            9
        );
    }
}
