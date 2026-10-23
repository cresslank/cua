//! One owned AT-SPI generation. The listener thread owns (and destroys) Tokio;
//! synchronous callers borrow only its Handle, never a process-lifetime Runtime.
use super::*;
use std::sync::{Arc, Condvar};
use std::time::Instant;

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);
const DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(1);
type FocusMap = Arc<Mutex<std::collections::HashMap<String, String>>>;
type Initialization = tokio::task::JoinHandle<Result<AccessibilityConnection>>;
type Completion = std::result::Result<(), String>;

enum ListenerStartup {
    Idle,
    Starting,
    Complete(Completion),
}

struct Global {
    enabled: bool,
    owner: Option<Arc<Owner>>,
}
static GLOBAL: Mutex<Global> = Mutex::new(Global {
    enabled: true,
    owner: None,
});

tokio::task_local! {
    static CURRENT_OWNER: Arc<Owner>;
}

#[derive(Default)]
struct Resources {
    closed: bool,
    active: usize,
    connection: Option<AccessibilityConnection>,
    // Published immediately after from_address, BEFORE registry registration.
    // Even aborting initialization must leave its socket available to close.
    raw: Option<atspi::zbus::Connection>,
    initialization: Option<tokio::task::AbortHandle>,
    tracker: Option<tokio::task::JoinHandle<()>>,
}

struct Owner {
    handle: tokio::runtime::Handle,
    resources: Mutex<Resources>,
    drained: Condvar,
    initialization: tokio::sync::Mutex<Option<Initialization>>,
    stop: tokio::sync::watch::Sender<Option<Instant>>,
    listener_request: tokio::sync::Notify,
    startup: Mutex<ListenerStartup>,
    startup_done: Condvar,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    // Public completion can report a bounded incomplete drain while this
    // thread remains the fenced runtime reaper. `done` means retirement really
    // finished, and only then is it safe to join/admit a replacement.
    report: Mutex<Option<Completion>>,
    report_changed: Condvar,
    done: Mutex<Option<Completion>>,
    done_changed: Condvar,
    focus: FocusMap,
    active_descendant: FocusMap,
}

impl Owner {
    fn new() -> Result<Arc<Self>> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .context("build AT-SPI tokio runtime")?;
        let (stop, _) = tokio::sync::watch::channel(None);
        let owner = Arc::new(Self {
            handle: rt.handle().clone(),
            resources: Mutex::new(Resources::default()),
            drained: Condvar::new(),
            initialization: tokio::sync::Mutex::new(None),
            stop,
            listener_request: tokio::sync::Notify::new(),
            startup: Mutex::new(ListenerStartup::Idle),
            startup_done: Condvar::new(),
            thread: Mutex::new(None),
            report: Mutex::new(None),
            report_changed: Condvar::new(),
            done: Mutex::new(None),
            done_changed: Condvar::new(),
            focus: Arc::default(),
            active_descendant: Arc::default(),
        });
        let worker = owner.clone();
        let thread = std::thread::Builder::new()
            .name("cua-atspi-listener".into())
            .spawn(move || worker.listen(rt))
            .context("could not spawn AT-SPI listener owner")?;
        *owner.thread.lock().unwrap() = Some(thread);
        Ok(owner)
    }

    fn listen(self: Arc<Self>, rt: tokio::runtime::Runtime) {
        let mut stop = self.stop.subscribe();
        let deadline = rt.block_on(async {
            tokio::select! {
                biased;
                deadline = stop.wait_for(|deadline| deadline.is_some()) => {
                    deadline.expect("AT-SPI owner retains shutdown sender").unwrap()
                }
                () = async {
                    loop {
                        self.listener_request.notified().await;
                        let result = self.connection().await.map(|_| ()).map_err(|e| e.to_string());
                        *self.startup.lock().unwrap() = ListenerStartup::Complete(result);
                        self.startup_done.notify_all();
                    }
                } => unreachable!(),
            }
        });

        // Stop admission and wake external Handle::block_on callers first.
        // Never hold resources while waiting for a task or a remote close.
        let drain_deadline = deadline.min(Instant::now() + DRAIN_TIMEOUT);
        let resources = self.resources.lock().unwrap();
        let (resources, _) = self
            .drained
            .wait_timeout_while(
                resources,
                drain_deadline.saturating_duration_since(Instant::now()),
                |resources| resources.active != 0,
            )
            .unwrap();
        let drained = resources.active == 0;
        let deadline = if drained {
            drop(resources);
            deadline
        } else {
            // Handle::block_on runs on external caller threads. A caller can
            // be parked inside a synchronous poll and later register a timer
            // or zbus I/O before observing cancellation. Destroying Tokio here
            // would panic underneath it. Report the bounded public failure,
            // then keep this owned thread/runtime as the fenced reaper until
            // every admitted caller has actually left Handle::block_on.
            self.publish_report(Err(
                "AT-SPI in-flight calls did not drain before shutdown".into()
            ));
            let mut resources = resources;
            while resources.active != 0 {
                resources = self.drained.wait(resources).unwrap();
            }
            drop(resources);
            // This is background retirement, NOT an extension of shutdown's
            // public deadline. Old work and new admission cannot overlap.
            Instant::now() + SHUTDOWN_TIMEOUT
        };

        let close_deadline = deadline.min(Instant::now() + CLOSE_TIMEOUT);
        let closed = rt.block_on(async {
            tokio::time::timeout_at(tokio::time::Instant::from_std(close_deadline), async {
                // Abort AND await, including initialization cancelled while a
                // caller held the async initialization lock.
                if let Some(task) = self.initialization.lock().await.take() {
                    task.abort();
                    let _ = task.await;
                }
                let (tracker, raw) = {
                    let mut resources = self.resources.lock().unwrap();
                    resources.initialization = None;
                    resources.connection = None;
                    (resources.tracker.take(), resources.raw.take())
                };
                if let Some(task) = tracker {
                    task.abort();
                    let _ = task.await;
                }
                if let Some(raw) = raw {
                    // atspi-connection 0.14 stores proxies, not a second bus.
                    // connection().clone().close() closes every proxy's socket.
                    raw.close().await.context("close AT-SPI connection")?;
                }
                Ok::<_, anyhow::Error>(())
            })
            .await
        });
        self.focus.lock().unwrap().clear();
        self.active_descendant.lock().unwrap().clear();
        rt.shutdown_timeout(deadline.saturating_duration_since(Instant::now()));
        let result = match closed {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(error.to_string()),
            Err(_) => Err("AT-SPI task/connection cleanup exceeded its shutdown budget".into()),
        };
        *self.done.lock().unwrap() = Some(result.clone());
        self.done_changed.notify_all();
        self.publish_report(result);
    }

    fn publish_report(&self, result: Completion) {
        let mut report = self.report.lock().unwrap();
        if report.is_none() {
            *report = Some(result);
            self.report_changed.notify_all();
        }
    }

    fn block_on<T>(
        self: &Arc<Self>,
        work: impl std::future::Future<Output = Result<T>>,
    ) -> Result<T> {
        {
            let mut resources = self.resources.lock().unwrap();
            anyhow::ensure!(
                !resources.closed,
                "atspi_shutdown: AT-SPI runtime is shut down"
            );
            resources.active += 1;
        }
        let _call = ActiveCall(self.clone());
        let mut stop = self.stop.subscribe();
        self.handle
            .block_on(CURRENT_OWNER.scope(self.clone(), async {
                tokio::select! {
                    biased;
                    _ = stop.wait_for(|deadline| deadline.is_some()) => {
                        Err(anyhow!("atspi_shutdown: AT-SPI runtime is shut down"))
                    }
                    result = work => {
                        // A synchronous native round trip can finish in the
                        // same poll that shutdown was signalled. Do not publish
                        // its late success as an admitted operation.
                        anyhow::ensure!(
                            self.stop.borrow().is_none(),
                            "atspi_shutdown: AT-SPI runtime is shut down"
                        );
                        result
                    },
                }
            }))
    }

    async fn connection(self: &Arc<Self>) -> Result<AccessibilityConnection> {
        // A cancelled caller releases this lock, but leaves the owned JoinHandle
        // here. The next caller awaits it; shutdown can abort it independently.
        let mut initialization = self.initialization.lock().await;
        {
            let resources = self.resources.lock().unwrap();
            anyhow::ensure!(
                !resources.closed,
                "atspi_shutdown: AT-SPI runtime is shut down"
            );
            if let Some(conn) = &resources.connection {
                return Ok(conn.clone());
            }
        }
        if initialization.is_none() {
            let mut resources = self.resources.lock().unwrap();
            anyhow::ensure!(
                !resources.closed,
                "atspi_shutdown: AT-SPI runtime is shut down"
            );
            let owner = self.clone();
            let task = self.handle.spawn(async move { owner.initialize().await });
            resources.initialization = Some(task.abort_handle());
            *initialization = Some(task);
        }
        let result = initialization.as_mut().unwrap().await;
        *initialization = None;
        self.resources.lock().unwrap().initialization = None;
        result.context("AT-SPI initialization task cancelled")?
    }

    async fn initialize(self: &Arc<Self>) -> Result<AccessibilityConnection> {
        let address = crate::a11y::trusted_accessibility_bus_address()?
            .parse()
            .map_err(|error| anyhow!("invalid trusted AT-SPI bus address: {error}"))?;
        self.connect(address).await
    }

    async fn connect(
        self: &Arc<Self>,
        address: atspi::zbus::Address,
    ) -> Result<AccessibilityConnection> {
        let conn = AccessibilityConnection::from_address(address)
            .await
            .map_err(|error| anyhow!("AT-SPI connect failed: {error}"))?;
        {
            let mut resources = self.resources.lock().unwrap();
            resources.raw = Some(conn.connection().clone());
            anyhow::ensure!(
                !resources.closed,
                "atspi_shutdown: AT-SPI runtime is shut down"
            );
        }
        let registration = async {
            conn.add_registry_event::<atspi::ObjectEvents>()
                .await
                .map_err(|e| anyhow!("AT-SPI object-event registration failed: {e}"))?;
            conn.register_event::<atspi::events::object::StateChangedEvent>()
                .await
                .map_err(|e| anyhow!("AT-SPI state-changed subscription failed: {e}"))?;
            conn.register_event::<atspi::events::object::ActiveDescendantChangedEvent>()
                .await
                .map_err(|e| anyhow!("AT-SPI active-descendant subscription failed: {e}"))?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if let Err(error) = registration {
            // Failed attempts remain retryable without keeping the old socket.
            conn.connection().clone().close().await?;
            self.resources.lock().unwrap().raw = None;
            return Err(error);
        }
        let mut resources = self.resources.lock().unwrap();
        anyhow::ensure!(
            !resources.closed,
            "atspi_shutdown: AT-SPI runtime is shut down"
        );
        resources.tracker = Some(self.handle.spawn(track_focus(
            conn.clone(),
            self.focus.clone(),
            self.active_descendant.clone(),
        )));
        resources.connection = Some(conn.clone());
        Ok(conn)
    }

    fn stop(&self, deadline: Instant) {
        let mut resources = self.resources.lock().unwrap();
        if !resources.closed {
            resources.closed = true;
            self.stop.send_replace(Some(deadline));
            if let Some(task) = &resources.initialization {
                task.abort();
            }
            if let Some(task) = &resources.tracker {
                task.abort();
            }
        }
        drop(resources);
        // Pair the shutdown predicate with the readiness mutex so a waiter
        // cannot miss this notification between checking stop and sleeping.
        let _startup = self.startup.lock().unwrap();
        self.startup_done.notify_all();
    }

    fn join(&self, deadline: Instant) -> Result<()> {
        let report = self.report.lock().unwrap();
        let (report, _) = self
            .report_changed
            .wait_timeout_while(
                report,
                deadline.saturating_duration_since(Instant::now()),
                |report| report.is_none(),
            )
            .unwrap();
        let result = report
            .clone()
            .ok_or_else(|| anyhow!("AT-SPI listener shutdown timed out"))?;
        drop(report);
        // An incomplete public report is not permission to join the reaper:
        // its runtime must stay alive beneath outstanding external callers.
        if self.done.lock().unwrap().is_some() {
            if let Some(thread) = self.thread.lock().unwrap().take() {
                thread
                    .join()
                    .map_err(|_| anyhow!("AT-SPI listener owner panicked"))?;
            }
        }
        result.map_err(anyhow::Error::msg)
    }

    fn ensure_listener(&self, timeout: Duration) -> Result<()> {
        anyhow::ensure!(
            !self.resources.lock().unwrap().closed,
            "atspi_shutdown: AT-SPI runtime is shut down"
        );
        let mut startup = self.startup.lock().unwrap();
        if matches!(
            &*startup,
            ListenerStartup::Idle | ListenerStartup::Complete(Err(_))
        ) {
            *startup = ListenerStartup::Starting;
            self.listener_request.notify_one();
        }
        let (startup, _) = self
            .startup_done
            .wait_timeout_while(startup, timeout, |state| {
                matches!(state, ListenerStartup::Starting) && self.stop.borrow().is_none()
            })
            .unwrap();
        anyhow::ensure!(
            self.stop.borrow().is_none(),
            "atspi_shutdown: AT-SPI runtime is shut down"
        );
        match &*startup {
            ListenerStartup::Complete(Ok(())) => Ok(()),
            ListenerStartup::Complete(Err(error)) => Err(anyhow!(error.clone())),
            ListenerStartup::Idle | ListenerStartup::Starting => Err(anyhow!(
                "AT-SPI listener initialization did not complete within {} ms",
                timeout.as_millis()
            )),
        }
    }
}

struct ActiveCall(Arc<Owner>);
impl Drop for ActiveCall {
    fn drop(&mut self) {
        self.0.resources.lock().unwrap().active -= 1;
        self.0.drained.notify_all();
    }
}

fn acquire() -> Result<Arc<Owner>> {
    let mut global = GLOBAL.lock().unwrap();
    anyhow::ensure!(
        global.enabled,
        "atspi_shutdown: AT-SPI runtime is shut down"
    );
    if global.owner.is_none() {
        global.owner = Some(Owner::new()?);
    }
    Ok(global.owner.as_ref().unwrap().clone())
}

/// Explicitly admit a new AT-SPI generation after the previous SDK lifetime
/// shut down. Lazy first use remains supported; calls never restart shutdown.
pub fn restart() -> Result<()> {
    let mut global = GLOBAL.lock().unwrap();
    // A public shutdown can return before an external caller leaves its
    // synchronous poll. Even immediately after the last late caller exits,
    // restart may transiently report "AT-SPI shutdown is still in progress"
    // until the owned reaper completes retirement. This is a retirement fence,
    // not permanent refusal: reap that exact completed generation here.
    if let Some(owner) = global.owner.as_ref() {
        if owner.done.lock().unwrap().is_some() && owner.resources.lock().unwrap().active == 0 {
            let _ = owner.join(Instant::now());
            global.owner = None;
        }
    }
    anyhow::ensure!(
        global.enabled || global.owner.is_none(),
        "AT-SPI shutdown is still in progress"
    );
    global.enabled = true;
    Ok(())
}

/// Stop admission, cancel/drain calls and owned tasks, close the raw zbus socket,
/// shut down Tokio and join its listener thread. Safe from an ambient runtime:
/// all async cleanup and Runtime destruction happen on the owned OS thread.
/// The shared three-second budget is not a separate timeout per resource.
/// Repeated calls are safe; an incomplete cleanup is returned, never hidden.
pub fn shutdown() -> Result<()> {
    let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
    let owner = {
        let mut global = GLOBAL.lock().unwrap();
        global.enabled = false;
        global.owner.clone()
    };
    let Some(owner) = owner else {
        return Ok(());
    };
    owner.stop(deadline);
    let result = owner.join(deadline);
    if owner.done.lock().unwrap().is_some() && owner.resources.lock().unwrap().active == 0 {
        let mut global = GLOBAL.lock().unwrap();
        if global
            .owner
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &owner))
        {
            global.owner = None;
        }
    }
    result
}

pub(super) struct RuntimeAccess;
pub(super) fn runtime() -> RuntimeAccess {
    RuntimeAccess
}
impl RuntimeAccess {
    pub(super) fn block_on<T>(
        &self,
        work: impl std::future::Future<Output = Result<T>>,
    ) -> Result<T> {
        acquire()?.block_on(work)
    }
}

pub(super) async fn shared_connection() -> Result<AccessibilityConnection> {
    let owner = CURRENT_OWNER
        .try_with(Arc::clone)
        .context("AT-SPI connection requested outside an admitted operation")?;
    owner.connection().await
}

pub fn ensure_listener_active() -> Result<()> {
    crate::a11y::trusted_accessibility_bus_address()?;
    acquire()?.ensure_listener(LISTENER_STARTUP_TIMEOUT)
}

pub(super) fn focus_map() -> FocusMap {
    CURRENT_OWNER.with(|owner| owner.focus.clone())
}
pub(super) fn active_descendant_map() -> FocusMap {
    CURRENT_OWNER.with(|owner| owner.active_descendant.clone())
}

pub(super) fn note_focus_event(
    map: &Mutex<std::collections::HashMap<String, String>>,
    bus: &str,
    path: &str,
    enabled: bool,
) {
    let mut map = map.lock().unwrap();
    if enabled {
        map.insert(bus.to_owned(), path.to_owned());
    } else if map.get(bus).is_some_and(|current| current == path) {
        map.remove(bus);
    }
}

#[cfg(test)]
mod tests;

async fn track_focus(conn: AccessibilityConnection, focus: FocusMap, active_descendant: FocusMap) {
    use futures_util::StreamExt;
    let stream = conn.event_stream();
    futures_util::pin_mut!(stream);
    while let Some(event) = stream.next().await {
        if let Ok(atspi::Event::Object(atspi::ObjectEvents::ActiveDescendantChanged(active))) =
            &event
        {
            if let Some(bus) = active.item.name_as_str() {
                active_descendant
                    .lock()
                    .unwrap()
                    .insert(bus.to_owned(), active.descendant.path_as_str().to_owned());
            }
            continue;
        }
        let Ok(atspi::Event::Object(atspi::ObjectEvents::StateChanged(changed))) = event else {
            continue;
        };
        if changed.state == State::Focused {
            if let Some(bus) = changed.item.name_as_str() {
                note_focus_event(&focus, bus, changed.item.path_as_str(), changed.enabled);
            }
        }
    }
}
