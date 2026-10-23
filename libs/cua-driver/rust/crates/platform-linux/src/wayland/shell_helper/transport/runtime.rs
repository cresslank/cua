//! Restartable ownership for the synchronous helper's Tokio executor.
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;
use tokio::runtime::{Handle, Runtime};
use tokio::sync::Notify;
use tokio::task::{AbortHandle, JoinHandle};
use zbus::Connection;

const DRAIN_TIMEOUT: Duration = Duration::from_millis(300);
const CLOSE_TIMEOUT: Duration = Duration::from_millis(200);
const RUNTIME_TIMEOUT: Duration = Duration::from_millis(200);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Default)]
pub(super) struct Owner {
    state: Arc<Mutex<State>>,
}

struct State {
    current: Option<OwnedRuntime>,
    stopping: bool,
    enabled: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            current: None,
            stopping: false,
            enabled: true,
        }
    }
}

struct OwnedRuntime {
    runtime: Runtime,
    generation: Arc<Generation>,
}

#[derive(Default)]
pub(super) struct Generation {
    pub(super) connection: tokio::sync::Mutex<Option<Arc<Session>>>,
    activity: Arc<Activity>,
    requests: Mutex<Vec<AbortHandle>>,
}

// Only handles/leases cross into futures. No future owns a Runtime, so task
// cancellation or the final lease's Drop can never drop one inside Tokio.
#[derive(Default)]
struct Activity {
    count: AtomicUsize,
    idle: Notify,
}

struct Lease(Arc<Activity>);

impl Activity {
    fn acquire(self: &Arc<Self>) -> Lease {
        self.count.fetch_add(1, Ordering::AcqRel);
        Lease(self.clone())
    }

    async fn drained(&self) {
        loop {
            let notified = self.idle.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.count.load(Ordering::Acquire) == 0 {
                return;
            }
            notified.await;
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if self.0.count.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0.idle.notify_waiters();
        }
    }
}

/// Last-user close: retiring a failed connection must not disconnect a second
/// request still using it. The close task itself also holds an activity lease.
pub(super) struct Session {
    pub(super) connection: Connection,
    runtime: Handle,
    activity: Arc<Activity>,
}

impl Generation {
    pub(super) fn session(&self, connection: Connection) -> Arc<Session> {
        Arc::new(Session {
            connection,
            runtime: Handle::current(),
            activity: self.activity.clone(),
        })
    }

    pub(super) async fn retire(&self, session: &Arc<Session>) {
        let mut shared = self.connection.lock().await;
        if shared
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, session))
        {
            shared.take();
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let connection = self.connection.clone();
        let lease = self.activity.acquire();
        self.runtime.spawn(async move {
            let _lease = lease;
            let _ = tokio::time::timeout(CLOSE_TIMEOUT, connection.close()).await;
        });
    }
}

impl Owner {
    pub(super) fn spawn<F, Fut>(&self, job: F) -> Option<JoinHandle<()>>
    where
        F: FnOnce(Arc<Generation>) -> Fut,
        Fut: Future<Output = ()> + Send + 'static,
    {
        // Admission and spawning share the retirement lock. Shutdown cannot
        // take the Runtime between acquiring a lease and publishing its task.
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.stopping || !state.enabled {
            return None;
        }
        if state.current.is_none() {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .thread_name("cua-shell-dbus")
                .enable_all()
                .build()
                .ok()?;
            state.current = Some(OwnedRuntime {
                runtime,
                generation: Arc::default(),
            });
        }
        let owned = state.current.as_ref()?;
        let generation = owned.generation.clone();
        let lease = generation.activity.acquire();
        let future = job(generation.clone());
        let task = owned.runtime.spawn(async move {
            let _lease = lease;
            future.await;
        });
        let mut requests = generation
            .requests
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        requests.retain(|request| !request.is_finished());
        requests.push(task.abort_handle());
        Some(task)
    }

    pub(super) fn restart(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.stopping {
            state.enabled = true;
        }
    }

    pub(super) fn shutdown(&self) {
        let owned = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.enabled = false;
            if state.stopping {
                return;
            }
            let Some(owned) = state.current.take() else {
                return;
            };
            state.stopping = true;
            owned
        };
        // shutdown_timeout must run outside *any* caller's Tokio context.
        // Keep ownership recoverable if the OS refuses to create the thread.
        let handoff = Arc::new(Mutex::new(Some(owned)));
        let worker_handoff = handoff.clone();
        let state = self.state.clone();
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        let spawned = std::thread::Builder::new()
            .name("cua-shell-dbus-stop".into())
            .spawn(move || {
                let owned = worker_handoff
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take();
                if let Some(owned) = owned {
                    owned.shutdown();
                }
                state.lock().unwrap_or_else(|e| e.into_inner()).stopping = false;
                let _ = done_tx.send(());
            });
        match spawned {
            Ok(worker) => {
                if done_rx.recv_timeout(SHUTDOWN_TIMEOUT).is_ok() {
                    let _ = worker.join();
                } else {
                    // Keep admission closed until the owned worker finishes.
                    tracing::warn!("compositor D-Bus teardown exceeded its deadline");
                }
            }
            Err(error) => {
                if let Some(owned) = handoff.lock().unwrap_or_else(|e| e.into_inner()).take() {
                    owned.runtime.shutdown_background();
                }
                self.state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .stopping = false;
                tracing::warn!(%error, "could not start compositor D-Bus teardown worker");
            }
        }
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl OwnedRuntime {
    fn shutdown(self) {
        let generation = self.generation;
        self.runtime.block_on(async {
            if tokio::time::timeout(DRAIN_TIMEOUT, generation.activity.drained())
                .await
                .is_err()
            {
                // Abort only requests, not their last-user connection close tasks.
                for request in generation
                    .requests
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .drain(..)
                {
                    request.abort();
                }
            }
            let _ = tokio::time::timeout(CLOSE_TIMEOUT, async {
                // Cancelling an initializer releases this async mutex, too.
                generation.connection.lock().await.take();
                generation.activity.drained().await;
            })
            .await;
        });
        self.runtime.shutdown_timeout(RUNTIME_TIMEOUT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_cancels_pending_work_and_allows_a_fresh_generation() {
        let owner = Owner::default();
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let task = owner
            .spawn(move |generation| async move {
                started_tx.send(Arc::downgrade(&generation)).unwrap();
                std::future::pending::<()>().await;
            })
            .unwrap();
        let old = started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let started = std::time::Instant::now();
        owner.shutdown();
        assert!(started.elapsed() < SHUTDOWN_TIMEOUT + Duration::from_secs(1));
        assert!(task.is_finished());
        assert!(old.upgrade().is_none(), "retired generation remained owned");
        owner.shutdown();
        assert!(owner.spawn(|_| async {}).is_none());
        owner.restart();
        let (tx, rx) = mpsc::sync_channel(1);
        owner
            .spawn(move |_| async move {
                tx.send(()).unwrap();
            })
            .unwrap();
        rx.recv_timeout(Duration::from_secs(2)).unwrap();
        owner.shutdown();
    }

    #[test]
    fn shutdown_drains_admitted_work_and_fences_concurrent_admission() {
        let owner = Arc::new(Owner::default());
        let release = Arc::new(Notify::new());
        let gate = release.clone();
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (finished_tx, finished_rx) = mpsc::sync_channel(1);
        owner
            .spawn(move |_| async move {
                started_tx.send(()).unwrap();
                gate.notified().await;
                finished_tx.send(()).unwrap();
            })
            .unwrap();
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let retiring = owner.clone();
        let worker = std::thread::spawn(move || retiring.shutdown());
        let deadline = std::time::Instant::now() + DRAIN_TIMEOUT;
        while !owner.state.lock().unwrap().stopping {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert!(owner.spawn(|_| async {}).is_none());
        owner.shutdown(); // Concurrent duplicate teardown is harmless.
        release.notify_one();
        finished_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        worker.join().unwrap();
        assert!(owner.state.lock().unwrap().current.is_none());
    }

    #[tokio::test]
    async fn abort_before_first_poll_releases_lease_and_async_shutdown_is_safe() {
        let owner = Owner::default();
        let (parked_tx, parked_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        owner
            .spawn(move |_| async move {
                parked_tx.send(()).unwrap();
                // Deliberately park the sole worker so the next future cannot poll.
                release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            })
            .unwrap();
        parked_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let (tx, rx) = mpsc::sync_channel(1);
        let task = owner
            .spawn(move |generation| {
                tx.send((Arc::downgrade(&generation), generation.activity.clone()))
                    .unwrap();
                async move { panic!("aborted future was polled") }
            })
            .unwrap();
        let (generation, activity) = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        task.abort();
        release_tx.send(()).unwrap();
        owner.shutdown();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(activity.count.load(Ordering::Acquire), 0);
        assert!(generation.upgrade().is_none());
        owner.shutdown();
    }
}
