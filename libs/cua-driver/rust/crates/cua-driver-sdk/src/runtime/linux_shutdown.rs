//! Linux platform retirement must not queue behind the Tokio blocking pool
//! that native calls being retired may already have saturated.
use std::future::Future;
use std::sync::{mpsc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// An embedding may supply a Tokio runtime without time enabled. Keep shutdown
/// deadlines independent of that runtime (and its possibly saturated blocking
/// pool). Closing the channel interrupts the watchdog immediately on completion
/// or cancellation; joining it never waits for the original deadline.
struct DeadlineThread {
    cancel: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for DeadlineThread {
    fn drop(&mut self) {
        drop(self.cancel.take());
        if self.thread.take().unwrap().join().is_err() {
            tracing::warn!("Linux shutdown deadline thread panicked");
        }
    }
}

pub(super) async fn timeout<F: Future>(budget: Duration, work: F) -> Result<F::Output, ()> {
    let started = Instant::now();
    let (cancel, cancelled) = mpsc::channel();
    let (expired, deadline) = tokio::sync::oneshot::channel();
    let thread = match std::thread::Builder::new()
        .name("cua-shutdown-deadline".into())
        .spawn(move || {
            let remaining = budget.saturating_sub(started.elapsed());
            if matches!(
                cancelled.recv_timeout(remaining),
                Err(mpsc::RecvTimeoutError::Timeout)
            ) {
                let _ = expired.send(());
            }
        }) {
        Ok(thread) => thread,
        Err(error) => {
            // Fail closed rather than waiting without a deadline.
            tracing::warn!(%error, "could not spawn Linux shutdown deadline watchdog");
            return Err(());
        }
    };
    let _watchdog = DeadlineThread {
        cancel: Some(cancel),
        thread: Some(thread),
    };
    tokio::select! {
        biased;
        result = work => Ok(result),
        _ = deadline => Err(()),
    }
}

const CLEANUP_BUDGET: Duration = Duration::from_secs(20);

/// Only this independently owned reaper joins the cleanup worker. Rust can mark
/// a JoinHandle finished before the OS runs its TLS destructors, so neither
/// shutdown/cancellation nor SDK construction may join it synchronously.
struct Retirement {
    #[cfg(test)]
    worker_id: std::sync::Arc<std::sync::OnceLock<std::thread::ThreadId>>,
    retired: std::sync::Arc<std::sync::atomic::AtomicBool>,
    _reaper: JoinHandle<()>,
}

static RETIRING: Mutex<Vec<Retirement>> = Mutex::new(Vec::new());

pub(super) fn reap_finished() {
    // Prune bookkeeping only. Even joining an apparently finished reaper could
    // wait for TLS destruction and exceed the three-second admission budget.
    let mut pending = RETIRING.lock().unwrap_or_else(|error| error.into_inner());
    pending.retain(|owner| !owner.retired.load(std::sync::atomic::Ordering::Acquire));
    if pending.is_empty() {
        *pending = Vec::new();
    }
}

/// Cancellation/timeout hands the independent joining reaper to the process
/// owner. Dropping a public waiter must never perform a potentially blocking join.
struct CleanupThread(Option<Retirement>);
impl Drop for CleanupThread {
    fn drop(&mut self) {
        let Some(owner) = self.0.take() else { return };
        if !owner.retired.load(std::sync::atomic::Ordering::Acquire) {
            RETIRING
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(owner);
        }
        // Dropping the reaper handle detaches only the reaper, not the worker it
        // owns and joins. Its remaining epilogue never runs on this caller.
    }
}

pub(super) async fn run(cleanup: impl FnOnce() + Send + 'static) {
    run_with_budget(cleanup, CLEANUP_BUDGET).await;
}

async fn run_with_budget(cleanup: impl FnOnce() + Send + 'static, budget: Duration) {
    let started = Instant::now();
    reap_finished();
    let (done, completion) = tokio::sync::oneshot::channel();
    let retired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let completed = retired.clone();
    #[cfg(test)]
    let worker_id = std::sync::Arc::new(std::sync::OnceLock::new());
    #[cfg(test)]
    let identity = worker_id.clone();
    let reaper = match std::thread::Builder::new()
        .name("cua-cleanup-reaper".into())
        .spawn(move || {
            let joined = match std::thread::Builder::new()
                .name("cua-linux-shutdown".into())
                .spawn(move || {
                    #[cfg(test)]
                    let _ = identity.set(std::thread::current().id());
                    cleanup();
                }) {
                Ok(worker) => {
                    let result = worker.join();
                    if result.is_err() {
                        tracing::warn!("Linux platform cleanup thread panicked");
                    }
                    result.is_ok()
                }
                Err(error) => {
                    // The owned closure/lease is dropped on this independent
                    // reaper, never by a caller waiting under the watchdog.
                    tracing::warn!(%error, "could not spawn Linux platform cleanup owner");
                    false
                }
            };
            // Publish completion ONLY after join returns, including the worker's
            // TLS destruction. Closure return/is_finished are not retirement.
            completed.store(true, std::sync::atomic::Ordering::Release);
            let _ = done.send(joined);
        }) {
        Ok(reaper) => reaper,
        Err(error) => {
            tracing::warn!(%error, "could not spawn Linux platform cleanup reaper; retirement incomplete");
            return;
        }
    };
    let owner = CleanupThread(Some(Retirement {
        #[cfg(test)]
        worker_id,
        retired,
        _reaper: reaper,
    }));
    match timeout(budget.saturating_sub(started.elapsed()), completion).await {
        Ok(Ok(true)) => {}
        Ok(_) => tracing::warn!("Linux platform cleanup failed; retirement incomplete"),
        Err(()) => tracing::warn!(
            ?budget,
            "Linux platform cleanup deadline exceeded; retirement incomplete, independent reaper retains owner"
        ),
    }
    drop(owner);
    reap_finished();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    fn timerless_driver(
        register: Option<fn(&mut cua_driver_core::tool::ToolRegistry)>,
    ) -> Arc<crate::CuaDriver> {
        crate::CuaDriver::try_create_for_host(crate::DriverHostOptions {
            cursor: cursor_overlay::CursorConfig {
                enabled: false,
                ..Default::default()
            },
            host_owns_permission_ux: false,
            host_bundle_id: None,
            claude_code_compatibility: false,
            prepare_desktop_environment: false,
            register_host_tools: register,
            authorization_host: None,
            activity_observer: None,
        })
        .unwrap()
    }

    struct InvocationProbe {
        started: tokio::sync::Notify,
        dropped: AtomicBool,
    }

    thread_local! {
        static NEXT_PROBE: std::cell::RefCell<Option<Arc<InvocationProbe>>> = const { std::cell::RefCell::new(None) };
    }

    struct PendingTool {
        def: cua_driver_core::tool::ToolDef,
        probe: Arc<InvocationProbe>,
    }

    struct InvocationLifetime(Arc<InvocationProbe>);
    impl Drop for InvocationLifetime {
        fn drop(&mut self) {
            self.0.dropped.store(true, Ordering::Release);
        }
    }

    #[async_trait::async_trait]
    impl cua_driver_core::tool::Tool for PendingTool {
        fn def(&self) -> &cua_driver_core::tool::ToolDef {
            &self.def
        }

        async fn invoke(&self, _: serde_json::Value) -> cua_driver_core::protocol::ToolResult {
            let _lifetime = InvocationLifetime(self.probe.clone());
            self.probe.started.notify_one();
            std::future::pending().await
        }
    }

    fn register_pending_tool(registry: &mut cua_driver_core::tool::ToolRegistry) {
        registry.register(Box::new(PendingTool {
            def: cua_driver_core::tool::ToolDef {
                name: "health_report".into(),
                description: "shutdown invocation-drain probe".into(),
                input_schema: serde_json::json!({"type": "object"}),
                read_only: true,
                destructive: false,
                idempotent: false,
                open_world: false,
            },
            probe: NEXT_PROBE.with(|next| next.borrow_mut().take().unwrap()),
        }));
    }

    #[test]
    fn timerless_sdk_shutdown_without_in_flight_calls() {
        let _serial = super::super::TEST_RUNTIME_LOCK.blocking_lock();
        let driver = timerless_driver(None);
        // Deliberately omit enable_time/enable_all: embeddings need not run timers.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let started = std::time::Instant::now();
        runtime.block_on(driver.shutdown()).unwrap();
        assert!(!driver.is_available());
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn timerless_sdk_shutdown_cancels_stalled_invocation_after_grace_period() {
        let _serial = super::super::TEST_RUNTIME_LOCK.blocking_lock();
        let probe = Arc::new(InvocationProbe {
            started: tokio::sync::Notify::new(),
            dropped: AtomicBool::new(false),
        });
        NEXT_PROBE.with(|next| *next.borrow_mut() = Some(probe.clone()));
        let driver = timerless_driver(Some(register_pending_tool));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let caller = driver.clone();
            let mut action =
                tokio::spawn(
                    async move { caller.call_tool("health_report".into(), "{}".into()).await },
                );
            tokio::select! {
                _ = probe.started.notified() => {},
                result = &mut action => panic!("probe was not admitted: {result:?}"),
            }
            let started = std::time::Instant::now();
            driver.shutdown().await.unwrap();
            assert!(started.elapsed() >= Duration::from_millis(250));
            assert!(started.elapsed() < Duration::from_secs(5));
            assert!(
                probe.dropped.load(Ordering::Acquire),
                "shutdown did not cancel the invocation"
            );
            assert!(matches!(
                action.await.unwrap(),
                Err(crate::DriverError::Shutdown)
            ));
            assert!(matches!(
                driver.call_tool("health_report".into(), "{}".into()).await,
                Err(crate::DriverError::Shutdown)
            ));
        });
    }

    #[test]
    fn timerless_cleanup_deadline_retains_and_reaps_owner() {
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let (identity, id) = tokio::sync::oneshot::channel();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let started = std::time::Instant::now();
        runtime.block_on(run_with_budget(
            move || {
                let _ = identity.send(std::thread::current().id());
                // Disconnection also releases this owner if the test panics.
                let _ = wait.recv_timeout(Duration::from_secs(5));
            },
            Duration::from_millis(30),
        ));
        let id = runtime.block_on(id).unwrap();
        assert!(started.elapsed() >= Duration::from_millis(30));
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(RETIRING
            .lock()
            .unwrap()
            .iter()
            .any(|thread| thread.worker_id.get() == Some(&id)));
        drop(release);
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            reap_finished();
            if !RETIRING
                .lock()
                .unwrap()
                .iter()
                .any(|thread| thread.worker_id.get() == Some(&id))
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "released cleanup owner was not joined"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn sdk_shutdown_does_not_wait_for_saturated_tokio_blocking_pool() {
        let _serial = super::super::TEST_RUNTIME_LOCK.blocking_lock();
        let mut options = super::super::RuntimeOptions::embedded(false);
        options.prepare_desktop_environment = false;
        let driver = super::super::DriverRuntime::create(options).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let (release, wait) = std::sync::mpsc::channel();
        let started = Arc::new(AtomicBool::new(false));
        let worker_started = started.clone();
        let result = runtime.block_on(async {
            let blocked = tokio::task::spawn_blocking(move || {
                worker_started.store(true, Ordering::Release);
                let _ = wait.recv();
            });
            while !started.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
            let result = tokio::time::timeout(Duration::from_secs(4), driver.shutdown()).await;
            // Always release/join the parked worker, even when the assertion fails.
            release.send(()).unwrap();
            blocked.await.unwrap();
            result
        });
        runtime.shutdown_timeout(Duration::from_secs(1));
        result.expect("platform cleanup queued behind native blocking work");
        assert!(!driver.is_running());
        #[cfg(not(target_env = "musl"))]
        assert!(driver.platform_lease.lock().unwrap().is_none());
    }

    async fn wait_until_reaped(id: std::thread::ThreadId) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                reap_finished();
                if !RETIRING
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|thread| thread.worker_id.get() == Some(&id))
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("released cleanup thread was not joined");
    }

    #[tokio::test]
    async fn timeout_retains_owned_thread_until_work_releases() {
        let (release, wait) = std::sync::mpsc::channel();
        let (identity, id) = tokio::sync::oneshot::channel();
        let start = std::time::Instant::now();
        run_with_budget(
            move || {
                let _ = identity.send(std::thread::current().id());
                let _ = wait.recv();
            },
            Duration::from_millis(30),
        )
        .await;
        let id = id.await.unwrap();
        let retained = RETIRING
            .lock()
            .unwrap()
            .iter()
            .any(|thread| thread.worker_id.get() == Some(&id));
        release.send(()).unwrap();
        wait_until_reaped(id).await;
        assert!(retained, "timed-out cleanup lost its join handle");
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn cancelled_shutdown_keeps_cleanup_owned_and_reapable() {
        let (release, wait) = std::sync::mpsc::channel();
        let (identity, id) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(run(move || {
            let _ = identity.send(std::thread::current().id());
            let _ = wait.recv();
        }));
        let id = id.await.unwrap();
        let cancelled = Instant::now();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(
            cancelled.elapsed() < Duration::from_secs(2),
            "cancellation waited for the cleanup deadline watchdog"
        );
        let retained = RETIRING
            .lock()
            .unwrap()
            .iter()
            .any(|thread| thread.worker_id.get() == Some(&id));
        release.send(()).unwrap();
        wait_until_reaped(id).await;
        assert!(retained, "cancelled shutdown detached its cleanup owner");
    }
}

#[cfg(test)]
#[path = "a4_tls_regressions.rs"]
mod a4_tls_regressions;
