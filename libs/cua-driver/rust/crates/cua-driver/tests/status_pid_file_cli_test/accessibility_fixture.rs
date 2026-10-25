//! A headless CLI fixture must prepare its own accessibility route. In
//! particular, Nix has neither a session bus nor a desktop to auto-activate one.
//! This is a real private D-Bus protocol peer, not a driver preparation bypass.
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::Duration;
use zbus::{self, zvariant::OwnedObjectPath};

struct Launcher(String);
#[zbus::interface(name = "org.a11y.Bus")]
impl Launcher {
    fn get_address(&self) -> &str {
        &self.0
    }
}

struct Status;
#[zbus::interface(name = "org.a11y.Status")]
impl Status {
    #[zbus(property)]
    fn is_enabled(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn screen_reader_enabled(&self) -> bool {
        true
    }
}

struct Registry;
#[zbus::interface(name = "org.a11y.atspi.Registry")]
impl Registry {
    fn register_event(&self, _event: &str) {}
    fn deregister_event(&self, _event: &str) {}
}

struct Desktop;
#[zbus::interface(name = "org.a11y.atspi.Accessible")]
impl Desktop {
    fn get_children(&self) -> Vec<(String, OwnedObjectPath)> {
        vec![]
    }
}

struct Bus(Child);
impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub(super) struct Session {
    _daemon: Bus,
    _dir: tempfile::TempDir,
    address: String,
    connection: Option<zbus::Connection>,
    runtime: Option<tokio::runtime::Runtime>,
}
impl Session {
    pub(super) fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("bus.conf");
        std::fs::write(&config, format!(
            "<busconfig><type>session</type><listen>unix:path={}</listen><auth>EXTERNAL</auth>\
             <policy context=\"default\"><allow send_destination=\"*\"/><allow receive_sender=\"*\"/><allow own=\"*\"/></policy></busconfig>",
            dir.path().join("bus").display()
        )).unwrap();
        let mut daemon = Bus(Command::new("dbus-daemon")
            .arg(format!("--config-file={}", config.display()))
            .args(["--nofork", "--print-address=1"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("status fixture requires a private dbus-daemon"));
        let mut address = String::new();
        BufReader::new(daemon.0.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        let address = address.trim().to_owned();
        assert!(
            !address.is_empty(),
            "private bus did not publish an address"
        );
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let connection = runtime.block_on(async {
            zbus::connection::Builder::address(address.as_str())
                .unwrap()
                .name("org.a11y.Bus")
                .unwrap()
                .name("org.a11y.atspi.Registry")
                .unwrap()
                .serve_at("/org/a11y/bus", Launcher(address.clone()))
                .unwrap()
                .serve_at("/org/a11y/bus", Status)
                .unwrap()
                .serve_at("/org/a11y/atspi/registry", Registry)
                .unwrap()
                .serve_at("/org/a11y/atspi/accessible/root", Desktop)
                .unwrap()
                .build()
                .await
                .unwrap()
        });
        Self {
            _dir: dir,
            _daemon: daemon,
            address,
            connection: Some(connection),
            runtime: Some(runtime),
        }
    }

    pub(super) fn apply(&self, command: &mut Command) {
        // Bind each child explicitly; never mutate the test process environment
        // or borrow a developer's desktop bus/advertisement settings.
        command
            .env("DBUS_SESSION_BUS_ADDRESS", &self.address)
            .env("AT_SPI_BUS_ADDRESS", &self.address)
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("CUA_DRIVER_RS_A11Y_ADVERTISE_MODE")
            .env_remove("CUA_DRIVER_RS_DISABLE_A11Y_ADVERTISE");
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        if let (Some(runtime), Some(connection)) = (self.runtime.as_ref(), self.connection.take()) {
            let _ = runtime.block_on(async move {
                tokio::time::timeout(Duration::from_millis(250), connection.close()).await
            });
        }
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_timeout(Duration::from_millis(250));
        }
    }
}
