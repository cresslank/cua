//! Linux process-level readiness when AT-SPI registry registration fails.

#![cfg(target_os = "linux")]

use std::io::{BufRead, BufReader, Read};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

use cua_driver_testkit::{spawn_in_job, ChildReaper};

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
    let mut command = Command::new("dbus-daemon");
    command
        .args(["--session", "--nofork", "--nopidfile", "--print-address=1"])
        .arg(format!("--address=unix:path={}", path.display()))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = spawn_in_job(&mut command).expect("spawn private dbus-daemon");
    let mut address = String::new();
    BufReader::new(child.stdout.take().expect("private bus stdout"))
        .read_line(&mut address)
        .expect("read private bus address");
    assert!(!address.trim().is_empty(), "private bus printed no address");
    reaper.push(child);
    address.trim().to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serve_refuses_admission_when_listener_registration_is_rejected() {
    let directory = tempfile::Builder::new()
        .prefix("cua-atspi-startup-")
        .tempdir_in("/tmp")
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
