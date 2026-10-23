use super::*;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

struct StoppedOwner(Arc<Owner>);
impl Drop for StoppedOwner {
    fn drop(&mut self) {
        let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
        self.0.stop(deadline);
        let _ = self.0.join(deadline);
    }
}

struct PrivateBus(Child);
impl Drop for PrivateBus {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn private_bus() -> (tempfile::TempDir, PrivateBus, String) {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("bus");
    let config = dir.path().join("bus.conf");
    std::fs::write(&config, format!(
        "<busconfig><type>session</type><listen>unix:path={}</listen><auth>EXTERNAL</auth>\
         <policy context=\"default\"><allow send_destination=\"*\"/><allow receive_sender=\"*\"/><allow own=\"*\"/></policy></busconfig>",
        socket.display()
    )).unwrap();
    let mut child = Command::new("dbus-daemon")
        .arg(format!("--config-file={}", config.display()))
        .args(["--nofork", "--print-address=1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("private lifecycle tests require dbus-daemon (no desktop bus is used)");
    let mut address = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut address)
        .unwrap();
    assert!(!address.trim().is_empty());
    (dir, PrivateBus(child), address.trim().to_owned())
}

struct Registry {
    entered: std::sync::mpsc::Sender<()>,
    stall: Arc<AtomicBool>,
    release: Arc<tokio::sync::Notify>,
}
#[zbus::interface(name = "org.a11y.atspi.Registry")]
impl Registry {
    async fn register_event(&self, _event: &str) {
        let _ = self.entered.send(());
        if self.stall.load(Ordering::SeqCst) {
            self.release.notified().await;
        }
    }
}

fn fixture_registry(
    rt: &tokio::runtime::Runtime,
    address: &str,
    stall: bool,
) -> (
    zbus::Connection,
    std::sync::mpsc::Receiver<()>,
    Arc<AtomicBool>,
    Arc<tokio::sync::Notify>,
) {
    let (entered, receiver) = std::sync::mpsc::channel();
    let stall = Arc::new(AtomicBool::new(stall));
    let release = Arc::new(tokio::sync::Notify::new());
    let conn = rt.block_on(async {
        zbus::connection::Builder::address(address)
            .unwrap()
            .name("org.a11y.atspi.Registry")
            .unwrap()
            .serve_at(
                "/org/a11y/atspi/registry",
                Registry {
                    entered,
                    stall: stall.clone(),
                    release: release.clone(),
                },
            )
            .unwrap()
            .build()
            .await
            .unwrap()
    });
    (conn, receiver, stall, release)
}

fn install_connection_attempt(owner: &Arc<Owner>, address: &str) {
    let worker = owner.clone();
    let address = address.parse().unwrap();
    let task = owner
        .handle
        .spawn(async move { worker.connect(address).await });
    owner.resources.lock().unwrap().initialization = Some(task.abort_handle());
    *owner.initialization.blocking_lock() = Some(task);
}

#[test]
fn private_bus_shutdown_closes_retained_connection_and_joins_focus_tracker() {
    let (_dir, _bus, address) = private_bus();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let (registry, _, _, _) = fixture_registry(&rt, &address, false);
    let owner = StoppedOwner(Owner::new().unwrap());
    install_connection_attempt(&owner.0, &address);
    owner.0.ensure_listener(Duration::from_secs(3)).unwrap();
    let conn = owner.0.block_on(shared_connection()).unwrap();
    let retained_raw = conn.connection().clone();
    let name = retained_raw.unique_name().unwrap().to_owned();
    let dbus = rt.block_on(zbus::fdo::DBusProxy::new(&registry)).unwrap();
    assert!(rt
        .block_on(dbus.name_has_owner(name.clone().into()))
        .unwrap());
    owner
        .0
        .focus
        .lock()
        .unwrap()
        .insert(":1.fixture".into(), "/focus".into());
    owner
        .0
        .active_descendant
        .lock()
        .unwrap()
        .insert(":1.fixture".into(), "/cell".into());

    let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
    owner.0.stop(deadline);
    owner.0.join(deadline).unwrap();
    assert!(owner.0.thread.lock().unwrap().is_none());
    assert!(owner.0.resources.lock().unwrap().tracker.is_none());
    assert!(owner.0.initialization.blocking_lock().is_none());
    assert!(owner.0.focus.lock().unwrap().is_empty());
    assert!(owner.0.active_descendant.lock().unwrap().is_empty());
    assert!(rt
        .block_on(retained_raw.emit_signal(
            None::<&str>,
            "/test",
            "org.cua.Test",
            "AfterShutdown",
            &()
        ))
        .is_err());
    // The server also sees the client disappear, even while a proxy clone is retained.
    let deadline = Instant::now() + Duration::from_secs(1);
    while rt
        .block_on(dbus.name_has_owner(name.clone().into()))
        .unwrap()
    {
        assert!(
            Instant::now() < deadline,
            "AT-SPI client remained on private bus"
        );
        std::thread::yield_now();
    }
    rt.block_on(registry.close()).unwrap();
}

#[test]
fn stalled_registration_readiness_times_out_but_shutdown_awaits_initialization() {
    let (_dir, _bus, address) = private_bus();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let (registry, entered, _, _) = fixture_registry(&rt, &address, true);
    let owner = StoppedOwner(Owner::new().unwrap());
    install_connection_attempt(&owner.0, &address);
    entered.recv_timeout(Duration::from_secs(3)).unwrap();
    let raw = owner
        .0
        .resources
        .lock()
        .unwrap()
        .raw
        .clone()
        .expect("partial connection must be retained before registration");
    let started = Instant::now();
    let error = owner
        .0
        .ensure_listener(Duration::from_millis(30))
        .unwrap_err();
    assert!(error
        .to_string()
        .contains("listener initialization did not complete"));
    assert!(started.elapsed() < Duration::from_secs(1));
    let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
    owner.0.stop(deadline);
    owner.0.join(deadline).unwrap();
    assert!(owner.0.initialization.blocking_lock().is_none());
    assert!(owner.0.resources.lock().unwrap().raw.is_none());
    assert!(rt
        .block_on(raw.emit_signal(None::<&str>, "/test", "org.cua.Test", "AfterShutdown", &()))
        .is_err());
    rt.block_on(registry.close()).unwrap();
}

#[test]
fn late_registration_preserves_listener_readiness() {
    let (_dir, _bus, address) = private_bus();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let (registry, entered, stall, release) = fixture_registry(&rt, &address, true);
    let owner = StoppedOwner(Owner::new().unwrap());
    install_connection_attempt(&owner.0, &address);
    entered.recv_timeout(Duration::from_secs(3)).unwrap();
    assert!(owner.0.ensure_listener(Duration::from_millis(30)).is_err());
    stall.store(false, Ordering::SeqCst);
    release.notify_one();
    owner.0.ensure_listener(Duration::from_secs(3)).unwrap();
    assert!(owner.0.block_on(shared_connection()).is_ok());
    drop(owner);
    rt.block_on(registry.close()).unwrap();
}

#[test]
fn failed_registration_closes_its_socket_and_remains_retryable() {
    let (_dir, _bus, address) = private_bus();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let owner = StoppedOwner(Owner::new().unwrap());
    let error = owner
        .0
        .block_on(owner.0.connect(address.parse().unwrap()))
        .unwrap_err();
    assert!(error
        .to_string()
        .contains("AT-SPI object-event registration failed"));
    assert!(owner.0.resources.lock().unwrap().raw.is_none());
    assert!(owner.0.resources.lock().unwrap().connection.is_none());
    let (registry, _, _, _) = fixture_registry(&rt, &address, false);
    install_connection_attempt(&owner.0, &address);
    owner.0.ensure_listener(Duration::from_secs(3)).unwrap();
    let first = owner.0.block_on(shared_connection()).unwrap();
    let second = owner.0.block_on(shared_connection()).unwrap();
    assert_eq!(
        first.connection().unique_name(),
        second.connection().unique_name()
    );
    drop(owner);
    rt.block_on(registry.close()).unwrap();
}

#[test]
fn in_flight_calls_cancel_cleanly_and_double_shutdown_is_safe() {
    let owner = StoppedOwner(Owner::new().unwrap());
    let worker = owner.0.clone();
    let (entered, ready) = std::sync::mpsc::sync_channel(1);
    let call = std::thread::spawn(move || {
        worker.block_on(async {
            entered.send(()).unwrap();
            std::future::pending::<Result<()>>().await
        })
    });
    ready.recv_timeout(Duration::from_secs(1)).unwrap();
    let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
    owner.0.stop(deadline);
    owner.0.join(deadline).unwrap();
    assert_eq!(
        call.join().unwrap().unwrap_err().to_string(),
        "atspi_shutdown: AT-SPI runtime is shut down"
    );
    assert_eq!(
        owner.0.block_on(async { Ok(()) }).unwrap_err().to_string(),
        "atspi_shutdown: AT-SPI runtime is shut down"
    );
    assert_eq!(owner.0.resources.lock().unwrap().active, 0);
    owner.0.stop(deadline);
    owner.0.join(deadline).unwrap();
}

#[test]
fn shutdown_fences_a_native_call_that_finishes_in_the_same_poll() {
    let owner = StoppedOwner(Owner::new().unwrap());
    let worker = owner.0.clone();
    let (entered, ready) = std::sync::mpsc::sync_channel(1);
    let (release, gate) = std::sync::mpsc::sync_channel(1);
    let call = std::thread::spawn(move || {
        worker.block_on(async {
            entered.send(()).unwrap();
            gate.recv_timeout(SHUTDOWN_TIMEOUT).unwrap();
            Ok(())
        })
    });
    ready.recv_timeout(Duration::from_secs(1)).unwrap();
    let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
    owner.0.stop(deadline);
    release.send(()).unwrap();
    assert_eq!(
        call.join().unwrap().unwrap_err().to_string(),
        "atspi_shutdown: AT-SPI runtime is shut down"
    );
    owner.0.join(deadline).unwrap();
}

#[test]
fn non_yielding_native_call_does_not_make_shutdown_unbounded() {
    let owner = StoppedOwner(Owner::new().unwrap());
    let worker = owner.0.clone();
    let (entered, ready) = std::sync::mpsc::sync_channel(1);
    let (release, gate) = std::sync::mpsc::sync_channel(1);
    let call = std::thread::spawn(move || {
        worker.block_on(async {
            entered.send(()).unwrap();
            gate.recv_timeout(SHUTDOWN_TIMEOUT + Duration::from_secs(1))
                .unwrap();
            Ok(())
        })
    });
    ready.recv_timeout(Duration::from_secs(1)).unwrap();
    let started = Instant::now();
    let deadline = started + SHUTDOWN_TIMEOUT;
    owner.0.stop(deadline);
    let result = owner.0.join(deadline);
    // Always release/join the owned call, including on a failed assertion.
    release.send(()).unwrap();
    let call_result = call.join().unwrap();
    assert_eq!(
        result.unwrap_err().to_string(),
        "AT-SPI in-flight calls did not drain before shutdown"
    );
    assert!(started.elapsed() < SHUTDOWN_TIMEOUT);
    assert_eq!(
        call_result.unwrap_err().to_string(),
        "atspi_shutdown: AT-SPI runtime is shut down"
    );
}

fn await_retirement(owner: &Owner) {
    let done = owner.done.lock().unwrap();
    let (done, _) = owner
        .done_changed
        .wait_timeout_while(done, SHUTDOWN_TIMEOUT, |done| done.is_none())
        .unwrap();
    assert!(done.is_some(), "released AT-SPI generation did not retire");
}

#[test]
fn area_a_late_timer_after_teardown_returns_error_not_panic() {
    let owner = StoppedOwner(Owner::new().unwrap());
    let (entered, ready) = std::sync::mpsc::sync_channel(1);
    let (release, gate) = std::sync::mpsc::channel::<()>();
    let worker = owner.0.clone();
    let call = std::thread::spawn(move || {
        worker.block_on(async {
            entered.send(()).unwrap();
            // Model an admitted native synchronous round trip that outlives
            // the drain, then resumes polling Tokio/zbus on the same thread.
            let _ = gate.recv_timeout(Duration::from_secs(10));
            let _ =
                tokio::time::timeout(Duration::from_millis(1), std::future::pending::<()>()).await;
            Ok(())
        })
    });
    ready.recv_timeout(Duration::from_secs(3)).unwrap();
    let started = Instant::now();
    owner.0.stop(started + SHUTDOWN_TIMEOUT);
    let result = owner.0.join(started + SHUTDOWN_TIMEOUT);
    let shutdown_elapsed = started.elapsed();
    let completed_before_release = owner.0.done.lock().unwrap().is_some();
    // Always release and join the owned probe, including on a red assertion.
    drop(release);
    let joined = call.join();
    assert!(
        joined.is_ok(),
        "in-flight caller panicked after native teardown"
    );
    assert_eq!(
        joined.unwrap().unwrap_err().to_string(),
        "atspi_shutdown: AT-SPI runtime is shut down"
    );
    assert_eq!(
        result.unwrap_err().to_string(),
        "AT-SPI in-flight calls did not drain before shutdown"
    );
    assert!(shutdown_elapsed < SHUTDOWN_TIMEOUT);
    assert!(
        !completed_before_release,
        "runtime destroyed under an active caller"
    );
    await_retirement(&owner.0);
    let _ = owner.0.join(Instant::now() + SHUTDOWN_TIMEOUT);
    assert!(owner.0.thread.lock().unwrap().is_none());
}

#[test]
fn area_a_completed_late_call_does_not_permanently_fence_restart() {
    const CHILD: &str = "CUA_NATIVE_LATE_RESTART_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let name = format!(
            "{}::area_a_completed_late_call_does_not_permanently_fence_restart",
            module_path!().split_once("::").unwrap().1
        );
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &name, "--nocapture", "--test-threads=1"])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        return;
    }
    restart().unwrap();
    let owner = acquire().unwrap();
    let (entered, ready) = std::sync::mpsc::sync_channel(1);
    let (release, gate) = std::sync::mpsc::channel::<()>();
    let worker = owner.clone();
    let call = std::thread::spawn(move || {
        worker.block_on(async {
            entered.send(()).unwrap();
            let _ = gate.recv_timeout(Duration::from_secs(10));
            Ok(())
        })
    });
    ready.recv_timeout(Duration::from_secs(3)).unwrap();
    let started = Instant::now();
    let result = shutdown();
    let shutdown_elapsed = started.elapsed();
    // Assert the transient fence itself, not just eventual readmission. Capture
    // every outcome before releasing native work; assert after joining the probe.
    let restart_started = Instant::now();
    let restart_error = restart().err().map(|error| error.to_string());
    let restart_elapsed = restart_started.elapsed();
    let admission_started = Instant::now();
    let admission_error = crate::lifecycle::RuntimeLease::acquire()
        .err()
        .map(|error| error.to_string());
    let admission_elapsed = admission_started.elapsed();
    let owner_unchanged_while_active = GLOBAL
        .lock()
        .unwrap()
        .owner
        .as_ref()
        .is_some_and(|current| Arc::ptr_eq(current, &owner));
    let retired_while_active = owner.done.lock().unwrap().is_some();
    drop(release);
    let call_result = call.join().unwrap();
    assert_eq!(
        result.unwrap_err().to_string(),
        "AT-SPI in-flight calls did not drain before shutdown"
    );
    assert!(shutdown_elapsed < SHUTDOWN_TIMEOUT);
    assert_eq!(
        restart_error.as_deref(),
        Some("AT-SPI shutdown is still in progress")
    );
    assert_eq!(
        admission_error.as_deref(),
        Some("AT-SPI shutdown is still in progress")
    );
    assert!(
        restart_elapsed < Duration::from_millis(500),
        "restart waited for old work"
    );
    assert!(
        admission_elapsed < Duration::from_millis(500),
        "SDK admission waited for old work"
    );
    assert!(
        owner_unchanged_while_active,
        "restart replaced the active retired owner"
    );
    assert!(
        !retired_while_active,
        "active generation advertised true retirement"
    );
    assert_eq!(
        call_result.unwrap_err().to_string(),
        "atspi_shutdown: AT-SPI runtime is shut down"
    );
    // The late caller has now exited, but its reaper may not yet have published
    // retirement. An immediate restart may succeed or hit this transient fence;
    // it must not permanently refuse the next SDK after retirement completes.
    if let Err(error) = restart() {
        assert_eq!(error.to_string(), "AT-SPI shutdown is still in progress");
    }
    await_retirement(&owner);
    let lease = crate::lifecycle::RuntimeLease::acquire()
        .expect("completed generation permanently blocks embedded SDK admission");
    let replacement = acquire().unwrap();
    assert!(!Arc::ptr_eq(&owner, &replacement));
    assert!(owner.thread.lock().unwrap().is_none());
    assert_eq!(
        owner.block_on(async { Ok(()) }).unwrap_err().to_string(),
        "atspi_shutdown: AT-SPI runtime is shut down"
    );
    // A stale owner cannot retire or clear the replacement generation.
    owner.stop(Instant::now() + SHUTDOWN_TIMEOUT);
    let _ = owner.join(Instant::now() + SHUTDOWN_TIMEOUT);
    assert!(Arc::ptr_eq(&acquire().unwrap(), &replacement));
    runtime().block_on(async { Ok(()) }).unwrap();
    drop(lease);
    assert!(replacement.thread.lock().unwrap().is_none());
}

#[test]
fn public_shutdown_and_restart_are_safe_inside_an_ambient_runtime() {
    // Isolate the process-global API from unrelated parallel native unit tests.
    const CHILD: &str = "CUA_NATIVE_LIFECYCLE_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let name = format!(
            "{}::public_shutdown_and_restart_are_safe_inside_an_ambient_runtime",
            module_path!().split_once("::").unwrap().1
        );
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &name, "--nocapture", "--test-threads=1"])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        return;
    }
    let (_dir, _bus, address) = private_bus();
    let fixture_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let (registry, _, _, _) = fixture_registry(&fixture_rt, &address, false);
    crate::a11y::initialize_private_accessibility_bus(&address).unwrap();
    let mut previous_name = None;
    for _ in 0..2 {
        restart().unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            ensure_listener_active().unwrap();
        });
        let conn = runtime().block_on(shared_connection()).unwrap();
        let name = conn.connection().unique_name().unwrap().to_string();
        assert_ne!(previous_name.as_ref(), Some(&name));
        previous_name = Some(name);
        rt.block_on(async {
            shutdown().unwrap();
            shutdown().unwrap();
        });
        assert_eq!(
            runtime()
                .block_on(async { Ok(()) })
                .unwrap_err()
                .to_string(),
            "atspi_shutdown: AT-SPI runtime is shut down"
        );
    }
    fixture_rt.block_on(registry.close()).unwrap();
}
