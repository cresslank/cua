//! Linux process-level behavior of the AT-SPI listener against private buses:
//! readiness when the bus accepts a connection but stalls, and liveness of the
//! process-lifetime accessibility connection while applications come and go.

#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

use cua_driver_testkit::{
    driver_binary_required, spawn_in_job, ChildReaper, IsolatedStateRoot, REQUIRE_DRIVER_BIN_ENV,
};
use zbus::message::Header;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

/// `cua-driver serve` on the private session bus at `session_bus_address`,
/// with its per-user state under `state` (#4094). Keep `state` alive until the
/// child is reaped.
fn serve_command(
    daemon_socket: &Path,
    session_bus_address: &str,
    state: &IsolatedStateRoot,
) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cua-driver"));
    command
        .args([
            "serve",
            "--socket",
            daemon_socket.to_str().expect("UTF-8 daemon socket"),
            "--no-overlay",
            "--no-permissions-gate",
        ])
        .envs(state.env())
        .env("DBUS_SESSION_BUS_ADDRESS", session_bus_address)
        // A developer shell may export NO_AT_BRIDGE=1, which makes the driver
        // skip AT-SPI startup entirely; these tests exist to exercise it.
        .env_remove("NO_AT_BRIDGE")
        .env("CUA_DRIVER_RS_DISABLE_A11Y_ADVERTISE", "1")
        .env("CUA_DRIVER_RS_TELEMETRY_ENABLED", "false")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    command
}

fn process_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn hold_canonical_desktop_preparation_lock() -> std::fs::File {
    let effective_uid = unsafe { libc::geteuid() };
    let runtime_dir = std::path::Path::new("/run/user").join(effective_uid.to_string());
    let path = if runtime_dir.is_dir() {
        runtime_dir.join("cua-driver-desktop-preparation.lock")
    } else {
        std::path::Path::new("/tmp").join(format!(
            "cua-driver-desktop-preparation-{effective_uid}.lock"
        ))
    };
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .expect("open canonical desktop preparation lock");
    assert_eq!(
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0,
        "canonical desktop preparation lock was unexpectedly held"
    );
    file
}

struct AccessibilityBus {
    accessibility_address: String,
    get_address_called: Arc<AtomicBool>,
}

#[zbus::interface(name = "org.a11y.Bus")]
impl AccessibilityBus {
    fn get_address(&self) -> String {
        self.get_address_called.store(true, Ordering::SeqCst);
        self.accessibility_address.clone()
    }
}

struct RejectingRegistry {
    register_event_called: Arc<AtomicBool>,
}

#[zbus::interface(name = "org.a11y.atspi.Registry")]
impl RejectingRegistry {
    fn register_event(&self, _event: &str) -> zbus::fdo::Result<()> {
        self.register_event_called.store(true, Ordering::SeqCst);
        Err(zbus::fdo::Error::Failed(
            "intentional listener-registration refusal".to_string(),
        ))
    }
}

fn spawn_private_bus(path: &Path, reaper: &mut ChildReaper) -> String {
    let mut failures = Vec::new();
    for attempt in 1..=3 {
        let _ = std::fs::remove_file(path);
        let mut command = Command::new("dbus-daemon");
        command
            .args(["--nofork", "--nopidfile", "--print-address=1"])
            .arg(format!("--address=unix:path={}", path.display()))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(config) = std::env::var_os("CUA_DRIVER_TEST_DBUS_SESSION_CONFIG") {
            command.arg(format!("--config-file={}", config.to_string_lossy()));
        } else {
            command.arg("--session");
        }
        let mut child = spawn_in_job(&mut command).expect("spawn private dbus-daemon");
        let mut address = String::new();
        BufReader::new(child.stdout.take().expect("private bus stdout"))
            .read_line(&mut address)
            .expect("read private bus address");
        if !address.trim().is_empty() {
            reaper.push(child);
            return address.trim().to_string();
        }

        let status = child.try_wait().ok().flatten();
        if status.is_none() {
            let _ = child.kill();
        }
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .expect("private bus stderr")
            .read_to_string(&mut stderr)
            .expect("read private bus stderr");
        let status = status.or_else(|| child.wait().ok());
        failures.push(format!(
            "attempt {attempt}: status={status:?}, stderr={}",
            stderr.trim()
        ));
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!(
        "private bus printed no address for {}: {}",
        path.display(),
        failures.join("; ")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serve_refuses_admission_when_listener_registration_is_rejected() {
    let _process_test = process_test_lock();
    let directory = tempfile::Builder::new()
        .prefix("cua-atspi-startup-")
        .tempdir()
        .expect("temporary startup-test directory");
    let session_bus_path = directory.path().join("session-bus.sock");
    let accessibility_bus_path = directory.path().join("accessibility-bus.sock");
    let daemon_socket = directory.path().join("driver.sock");
    let mut reaper = ChildReaper::new();
    let session_bus_address = spawn_private_bus(&session_bus_path, &mut reaper);
    let accessibility_bus_address = spawn_private_bus(&accessibility_bus_path, &mut reaper);

    let get_address_called = Arc::new(AtomicBool::new(false));
    let register_event_called = Arc::new(AtomicBool::new(false));
    let _session_service = zbus::connection::Builder::address(session_bus_address.as_str())
        .expect("session service address")
        .name("org.a11y.Bus")
        .expect("session service name")
        .serve_at(
            "/org/a11y/bus",
            AccessibilityBus {
                accessibility_address: accessibility_bus_address.clone(),
                get_address_called: get_address_called.clone(),
            },
        )
        .expect("session accessibility interface")
        .build()
        .await
        .expect("connect session accessibility service");
    let _registry_service = zbus::connection::Builder::address(accessibility_bus_address.as_str())
        .expect("registry service address")
        .name("org.a11y.atspi.Registry")
        .expect("registry service name")
        .serve_at(
            "/org/a11y/atspi/registry",
            RejectingRegistry {
                register_event_called: register_event_called.clone(),
            },
        )
        .expect("registry interface")
        .build()
        .await
        .expect("connect accessibility registry service");

    let mut command = Command::new(env!("CARGO_BIN_EXE_cua-driver"));
    command
        .args([
            "serve",
            "--socket",
            daemon_socket.to_str().expect("UTF-8 daemon socket"),
            "--no-overlay",
            "--no-permissions-gate",
        ])
        .env("DBUS_SESSION_BUS_ADDRESS", &session_bus_address)
        .env("CUA_DRIVER_RS_DISABLE_A11Y_ADVERTISE", "1")
        .env("CUA_DRIVER_RS_TELEMETRY_ENABLED", "false")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    let mut child = spawn_in_job(&mut command).expect("spawn cua-driver serve");
    let readiness_deadline = Instant::now() + Duration::from_secs(8);
    let status = loop {
        if UnixStream::connect(&daemon_socket).is_ok() {
            panic!("serve bound before the complete AT-SPI contract was ready");
        }
        if let Some(status) = child.try_wait().expect("inspect cua-driver serve") {
            break status;
        }
        if Instant::now() >= readiness_deadline {
            let mut reaper = ChildReaper::new();
            reaper.push(child);
            panic!("serve did not fail within the bounded AT-SPI readiness deadline");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    };

    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("driver stderr")
        .read_to_string(&mut stderr)
        .expect("read driver stderr");
    reaper.push(child);
    assert!(
        get_address_called.load(Ordering::SeqCst),
        "driver never completed org.a11y.Bus.GetAddress on the session bus"
    );
    assert!(
        register_event_called.load(Ordering::SeqCst),
        "driver never reached org.a11y.atspi.Registry.RegisterEvent on the separate accessibility bus"
    );
    assert!(
        !status.success(),
        "serve unexpectedly admitted a degraded runtime"
    );
    assert!(
        !daemon_socket.exists(),
        "serve left a socket after refused admission"
    );
    assert!(
        stderr.contains("listener startup failed")
            || stderr.contains("object-event registration failed")
            || stderr.contains("intentional listener-registration refusal"),
        "driver error did not identify listener registration failure: {stderr}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serve_refuses_admission_when_reachable_accessibility_status_rejects_preparation() {
    let _process_test = process_test_lock();
    let directory = tempfile::Builder::new()
        .prefix("cua-atspi-preparation-")
        .tempdir()
        .expect("temporary startup-test directory");
    let session_bus_path = directory.path().join("session-bus.sock");
    let accessibility_bus_path = directory.path().join("accessibility-bus.sock");
    let daemon_socket = directory.path().join("driver.sock");
    let mut reaper = ChildReaper::new();
    let session_bus_address = spawn_private_bus(&session_bus_path, &mut reaper);
    let accessibility_bus_address = spawn_private_bus(&accessibility_bus_path, &mut reaper);
    let get_address_called = Arc::new(AtomicBool::new(false));

    // Deliberately export org.a11y.Bus without org.a11y.Status. The route is
    // reachable and GetAddress succeeds, but the default GNOME preparation
    // write must fail strict daemon admission rather than being downgraded.
    let _session_service = zbus::connection::Builder::address(session_bus_address.as_str())
        .expect("session service address")
        .name("org.a11y.Bus")
        .expect("session service name")
        .serve_at(
            "/org/a11y/bus",
            AccessibilityBus {
                accessibility_address: accessibility_bus_address,
                get_address_called: get_address_called.clone(),
            },
        )
        .expect("session accessibility interface")
        .build()
        .await
        .expect("connect session accessibility service");

    let mut command = Command::new(env!("CARGO_BIN_EXE_cua-driver"));
    command
        .args([
            "serve",
            "--socket",
            daemon_socket.to_str().expect("UTF-8 daemon socket"),
            "--no-overlay",
            "--no-permissions-gate",
        ])
        .env("DBUS_SESSION_BUS_ADDRESS", &session_bus_address)
        .env("XDG_CURRENT_DESKTOP", "GNOME")
        .env_remove("CUA_DRIVER_RS_DISABLE_A11Y_ADVERTISE")
        .env_remove("CUA_DRIVER_RS_A11Y_ADVERTISE_MODE")
        .env("CUA_DRIVER_RS_TELEMETRY_ENABLED", "false")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    let mut child = spawn_in_job(&mut command).expect("spawn cua-driver serve");
    let readiness_deadline = Instant::now() + Duration::from_secs(8);
    let status = loop {
        if UnixStream::connect(&daemon_socket).is_ok() {
            panic!("serve bound after accessibility preparation was rejected");
        }
        if let Some(status) = child.try_wait().expect("inspect cua-driver serve") {
            break status;
        }
        if Instant::now() >= readiness_deadline {
            let mut timed_out = ChildReaper::new();
            timed_out.push(child);
            panic!("serve did not fail within the accessibility preparation deadline");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    };

    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("driver stderr")
        .read_to_string(&mut stderr)
        .expect("read driver stderr");
    reaper.push(child);
    assert!(get_address_called.load(Ordering::SeqCst));
    assert!(
        !status.success(),
        "serve unexpectedly admitted a degraded runtime"
    );
    assert!(
        !daemon_socket.exists(),
        "serve left a socket after refused admission"
    );
    assert!(
        stderr.contains("session accessibility preparation failed")
            || stderr.contains("org.a11y.Status")
            || stderr.contains("UnknownInterface"),
        "driver error did not identify accessibility preparation failure: {stderr}"
    );
}

#[test]
fn direct_mcp_degrades_but_serve_refuses_when_desktop_preparation_lock_is_contended() {
    let _process_test = process_test_lock();
    let _held_lock = hold_canonical_desktop_preparation_lock();
    let directory = tempfile::Builder::new()
        .prefix("cua-atspi-lock-contention-")
        .tempdir()
        .expect("temporary lock-contention directory");
    let missing_bus = format!(
        "unix:path={}",
        directory.path().join("missing-session-bus.sock").display()
    );

    let mut direct_command = Command::new(env!("CARGO_BIN_EXE_cua-driver"));
    direct_command
        .args(["mcp", "--direct"])
        .env("DBUS_SESSION_BUS_ADDRESS", &missing_bus)
        .env("XDG_CURRENT_DESKTOP", "GNOME")
        .env("CUA_DRIVER_RS_TELEMETRY_ENABLED", "false")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut direct = spawn_in_job(&mut direct_command).expect("spawn direct MCP");
    {
        let mut stdin = direct.stdin.take().expect("direct MCP stdin");
        writeln!(
            stdin,
            "{}",
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}})
        )
        .unwrap();
        writeln!(
            stdin,
            "{}",
            serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})
        )
        .unwrap();
    }
    let direct_output = direct.wait_with_output().expect("wait for direct MCP");
    assert!(
        direct_output.status.success(),
        "direct MCP rejected best-effort lock contention: {}",
        String::from_utf8_lossy(&direct_output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&direct_output.stdout)
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .any(|response| response["id"] == 2 && response.get("result").is_some()),
        "direct MCP returned no tools/list response"
    );

    let daemon_socket = directory.path().join("driver.sock");
    let mut serve_command = Command::new(env!("CARGO_BIN_EXE_cua-driver"));
    serve_command
        .args([
            "serve",
            "--socket",
            daemon_socket.to_str().expect("UTF-8 daemon socket"),
            "--no-overlay",
            "--no-permissions-gate",
        ])
        .env("DBUS_SESSION_BUS_ADDRESS", &missing_bus)
        .env("XDG_CURRENT_DESKTOP", "GNOME")
        .env("CUA_DRIVER_RS_TELEMETRY_ENABLED", "false")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut serve = spawn_in_job(&mut serve_command).expect("spawn strict serve");
    let deadline = Instant::now() + Duration::from_secs(8);
    let status = loop {
        if UnixStream::connect(&daemon_socket).is_ok() {
            let mut reaper = ChildReaper::new();
            reaper.push(serve);
            panic!("strict serve bound while desktop preparation lock was unavailable");
        }
        if let Some(status) = serve.try_wait().expect("inspect strict serve") {
            break status;
        }
        if Instant::now() >= deadline {
            let mut reaper = ChildReaper::new();
            reaper.push(serve);
            panic!("strict serve did not reject lock contention before its deadline");
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    let mut stderr = String::new();
    serve
        .stderr
        .take()
        .expect("strict serve stderr")
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(!status.success());
    assert!(!daemon_socket.exists());
    assert!(
        stderr.contains("desktop preparation lock") && stderr.contains("unavailable"),
        "strict serve error did not identify lock contention: {stderr}"
    );
}

/// Start one private `dbus-daemon` in `directory`, owned by `reaper`, and
/// return its address. A missing binary skips the test, or panics under the
/// same `CUA_TEST_REQUIRE_DRIVER_BIN=1` strictness CI applies to the driver
/// binary. Explicit config: a sandbox (Nix) has no session.conf, and then the
/// daemon exits without printing an address.
fn start_private_bus(reaper: &mut ChildReaper, directory: &Path, name: &str) -> Option<String> {
    let config = directory.join(format!("{name}-bus.conf"));
    std::fs::write(
        &config,
        format!(
            "<busconfig><type>session</type>\
             <listen>unix:path={}</listen><auth>EXTERNAL</auth>\
             <policy context=\"default\"><allow send_destination=\"*\" eavesdrop=\"true\"/>\
             <allow eavesdrop=\"true\"/><allow own=\"*\"/></policy></busconfig>",
            directory.join(format!("{name}-bus.sock")).display()
        ),
    )
    .expect("write private bus config");
    let mut command = Command::new("dbus-daemon");
    command
        .arg(format!("--config-file={}", config.display()))
        .args(["--nofork", "--print-address=1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = match spawn_in_job(&mut command) {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            assert!(
                !driver_binary_required(),
                "dbus-daemon is not installed; {REQUIRE_DRIVER_BIN_ENV}=1 forbids skipping"
            );
            eprintln!("skipping: dbus-daemon is not installed");
            return None;
        }
        Err(error) => panic!("private test bus: {error}"),
    };
    let mut address = String::new();
    std::io::BufReader::new(child.stdout.take().expect("dbus-daemon stdout"))
        .read_line(&mut address)
        .expect("read private bus address");
    reaper.push(child);
    assert!(!address.trim().is_empty(), "dbus-daemon printed no address");
    Some(address.trim().to_owned())
}

/// `org.a11y.Bus` on the private session bus: tells the driver where the
/// private accessibility bus is.
struct AccessibilityBusLauncher {
    accessibility_bus_address: String,
}

#[zbus::interface(name = "org.a11y.Bus")]
impl AccessibilityBusLauncher {
    fn get_address(&self) -> String {
        self.accessibility_bus_address.clone()
    }
}

/// `org.a11y.atspi.Registry` on the private accessibility bus: remembers which
/// connection registered for which event.
struct Registry {
    registrations: Arc<Mutex<Vec<(String, String)>>>,
}

#[zbus::interface(name = "org.a11y.atspi.Registry")]
impl Registry {
    fn register_event(&self, event: String, #[zbus(header)] header: Header<'_>) {
        if let Some(sender) = header.sender() {
            self.registrations
                .lock()
                .unwrap()
                .push((sender.to_string(), event));
        }
    }
}

/// The registry's desktop root accessible, with no applications on it.
struct DesktopRoot;

#[zbus::interface(name = "org.a11y.atspi.Accessible")]
impl DesktopRoot {
    fn get_children(&self) -> Vec<(String, OwnedObjectPath)> {
        Vec::new()
    }
}

fn connect(address: &str) -> zbus::connection::Builder<'_> {
    zbus::connection::Builder::address(address).expect("private bus address")
}

async fn connection_stats(bus: &zbus::Connection, name: &str) -> HashMap<String, OwnedValue> {
    bus.call_method(
        Some("org.freedesktop.DBus"),
        "/org/freedesktop/DBus",
        Some("org.freedesktop.DBus.Debug.Stats"),
        "GetConnectionStats",
        &(name,),
    )
    .await
    .expect("GetConnectionStats (dbus-daemon built with statistics support)")
    .body()
    .deserialize()
    .expect("decode connection statistics")
}

fn queued_messages(stats: &HashMap<String, OwnedValue>) -> u32 {
    match stats.get("OutgoingMessages").map(|value| &**value) {
        Some(Value::U32(count)) => *count,
        other => panic!("OutgoingMessages missing from connection statistics: {other:?}"),
    }
}

async fn match_rules(bus: &zbus::Connection, name: &str) -> Vec<String> {
    let all: HashMap<String, Vec<String>> = bus
        .call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus.Debug.Stats"),
            "GetAllMatchRules",
            &(),
        )
        .await
        .expect("GetAllMatchRules (dbus-daemon built with statistics support)")
        .body()
        .deserialize()
        .expect("decode match rules");
    all.get(name).cloned().unwrap_or_default()
}

/// `object:state-changed:focused` for one accessible, as a toolkit emits it.
fn focus_event(index: usize) -> zbus::Message {
    let properties: HashMap<&str, Value<'_>> = HashMap::new();
    zbus::Message::signal(
        format!("/org/a11y/atspi/accessible/probe_{index}"),
        "org.a11y.atspi.Event.Object",
        "StateChanged",
    )
    .expect("focus event header")
    .build(&("focused", 1_i32, 0_i32, Value::I32(0), properties))
    .expect("focus event body")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accessibility_connection_keeps_draining_events_through_application_churn() {
    let _serial = process_test_lock();
    let directory = tempfile::Builder::new()
        .prefix("cua-atspi-churn-")
        .tempdir()
        .expect("temporary churn-test directory");
    let state = IsolatedStateRoot::new().expect("isolated driver state root");
    let mut reaper = ChildReaper::new();
    let Some(session_bus_address) = start_private_bus(&mut reaper, directory.path(), "session")
    else {
        return;
    };
    let accessibility_bus_address = start_private_bus(&mut reaper, directory.path(), "a11y")
        .expect("second private bus starts once the first did");

    let _session_bus = connect(&session_bus_address)
        .name("org.a11y.Bus")
        .expect("launcher name")
        .serve_at(
            "/org/a11y/bus",
            AccessibilityBusLauncher {
                accessibility_bus_address: accessibility_bus_address.clone(),
            },
        )
        .expect("serve launcher")
        .build()
        .await
        .expect("own org.a11y.Bus on the private session bus");
    let registrations = Arc::new(Mutex::new(Vec::new()));
    let accessibility_bus = connect(&accessibility_bus_address)
        .name("org.a11y.atspi.Registry")
        .expect("registry name")
        .serve_at(
            "/org/a11y/atspi/registry",
            Registry {
                registrations: registrations.clone(),
            },
        )
        .expect("serve registry")
        .serve_at("/org/a11y/atspi/accessible/root", DesktopRoot)
        .expect("serve desktop root")
        .build()
        .await
        .expect("own org.a11y.atspi.Registry on the private accessibility bus");

    let daemon_socket = directory.path().join("driver.sock");
    let child = spawn_in_job(&mut serve_command(
        &daemon_socket,
        &session_bus_address,
        &state,
    ))
    .expect("spawn cua-driver serve");
    reaper.push(child);

    // The driver's unique name on the accessibility bus is the sender of its
    // startup event registrations; its StateChanged match rule follows them.
    let startup_deadline = Instant::now() + Duration::from_secs(15);
    let driver = loop {
        if let Some((sender, _)) = registrations.lock().unwrap().first().cloned() {
            break sender;
        }
        assert!(
            Instant::now() < startup_deadline,
            "cua-driver never registered an AT-SPI event listener on the private accessibility bus"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    loop {
        let rules = match_rules(&accessibility_bus, &driver).await;
        if ["member='StateChanged'", "member='ActiveDescendantChanged'"]
            .iter()
            .all(|member| rules.iter().any(|rule| rule.contains(member)))
        {
            break;
        }
        assert!(
            Instant::now() < startup_deadline,
            "cua-driver never subscribed to both focus event types on the accessibility bus: {rules:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // An application that joins the bus after the driver and never answers a
    // method call (it serves no objects), then 100 applications appearing and
    // leaving, then a burst of focus events for the driver's subscription.
    let _silent_application = connect(&accessibility_bus_address)
        .build()
        .await
        .expect("connect silent application");
    for _ in 0..100 {
        drop(
            connect(&accessibility_bus_address)
                .build()
                .await
                .expect("connect transient application"),
        );
    }
    for index in 0..3000 {
        accessibility_bus
            .send(&focus_event(index))
            .await
            .expect("emit focus event");
    }

    let drain_deadline = Instant::now() + Duration::from_secs(5);
    let queued = loop {
        let queued = queued_messages(&connection_stats(&accessibility_bus, &driver).await);
        if queued == 0 || Instant::now() >= drain_deadline {
            break queued;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(
        queued, 0,
        "{queued} messages stayed queued at the bus for cua-driver's accessibility connection: it stopped reading after application churn"
    );
}
