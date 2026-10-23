//! Shared, caller-owned xdg-desktop-portal connections and bounded teardown.
//!
//! Never use ashpd's process-global connection cache: a cached connection can
//! retain a socket reader on an executor that the caller has already destroyed.

use std::future::Future;
#[cfg(any(feature = "portal-input", feature = "portal-capture"))]
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
#[cfg(any(test, feature = "portal-input", feature = "portal-capture"))]
use std::time::Instant;

pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
pub(crate) const CLOSE_TIMEOUT: Duration = Duration::from_millis(500);
pub(crate) const RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(250);
#[cfg(any(feature = "portal-input", feature = "portal-capture"))]
pub(crate) const OWNER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);

pub(crate) async fn fresh_session_connection() -> anyhow::Result<zbus::Connection> {
    tokio::time::timeout(PROBE_TIMEOUT, zbus::Connection::session())
        .await
        .map_err(|_| anyhow::anyhow!("xdg-desktop-portal session bus connection timed out"))?
        .map_err(|e| anyhow::anyhow!("xdg-desktop-portal session bus unreachable: {e}"))
}

pub(crate) async fn close_connection(connection: zbus::Connection) {
    match tokio::time::timeout(CLOSE_TIMEOUT, connection.close()).await {
        Ok(Ok(())) => (),
        Ok(Err(error)) => tracing::debug!("portal connection close failed: {error}"),
        Err(_) => tracing::warn!("portal connection close timed out"),
    }
}

#[cfg(any(feature = "portal-input", feature = "portal-capture"))]
pub(crate) async fn close_session<T: ashpd::desktop::SessionPortal>(
    session: &ashpd::desktop::Session<T>,
) {
    match tokio::time::timeout(CLOSE_TIMEOUT, session.close()).await {
        Ok(Ok(())) => (),
        Ok(Err(error)) => tracing::debug!("portal session close failed: {error}"),
        Err(_) => tracing::warn!("portal session close timed out; closing owned connection"),
    }
}

/// Keep the raw connection even when a proxy operation fails or times out.
/// Close it while its executor is still alive, including every early-return path.
pub(crate) async fn with_connection<T, F, Fut>(operation: F) -> anyhow::Result<T>
where
    F: FnOnce(zbus::Connection) -> Fut,
    Fut: Future<Output = anyhow::Result<T>>,
{
    let connection = fresh_session_connection().await?;
    let result = operation(connection.clone()).await;
    close_connection(connection).await;
    result
}

/// Cancellation does not require a command-queue slot or a lock held by consent.
#[cfg(any(feature = "portal-input", feature = "portal-capture"))]
pub(crate) async fn cancelled(stopped: &AtomicBool) {
    while !stopped.load(Ordering::Acquire) {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[cfg(any(feature = "portal-input", feature = "portal-capture"))]
pub(crate) async fn until_stopped<T>(
    stopped: &AtomicBool,
    future: impl Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    tokio::select! {
        biased;
        _ = cancelled(stopped) => anyhow::bail!("portal owner is shutting down"),
        result = future => result,
    }
}

/// Runtime::block_on/drop may not run inside a caller's Tokio task.
pub(crate) fn off_runtime<T: Send>(operation: impl FnOnce() -> T + Send) -> T {
    if tokio::runtime::Handle::try_current().is_ok() {
        std::thread::scope(|scope| {
            scope
                .spawn(operation)
                .join()
                .expect("portal cleanup panicked")
        })
    } else {
        operation()
    }
}

#[cfg(any(feature = "portal-input", feature = "portal-capture"))]
pub(crate) fn lock_until<T>(
    mutex: &std::sync::Mutex<T>,
    deadline: Instant,
) -> Option<std::sync::MutexGuard<'_, T>> {
    loop {
        match mutex.try_lock() {
            Ok(guard) => return Some(guard),
            Err(std::sync::TryLockError::Poisoned(error)) => return Some(error.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return None,
        }
    }
}

#[cfg(feature = "portal-input")]
pub(crate) fn wait_for_thread<T>(thread: &std::thread::JoinHandle<T>, deadline: Instant) -> bool {
    while !thread.is_finished() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        std::thread::sleep(remaining.min(Duration::from_millis(5)));
    }
    true
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    #[cfg(any(feature = "portal-input", feature = "portal-capture"))]
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    #[cfg(any(feature = "portal-input", feature = "portal-capture"))]
    use std::sync::mpsc;
    #[cfg(any(feature = "portal-input", feature = "portal-capture"))]
    use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

    /// Run only the exact child test on a newly spawned private bus. No test
    /// modifies the parent process environment or connects to its desktop bus.
    pub(crate) fn private_bus_test(name: &str, test: impl FnOnce()) {
        let name = name.split_once("::").unwrap().1;
        if std::env::var("CUA_PORTAL_OWNER_TEST").as_deref() == Ok(name) {
            test();
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join("bus.conf");
        std::fs::write(&config, format!(
            "<busconfig><type>session</type><listen>unix:tmpdir={}</listen><auth>EXTERNAL</auth>\
             <policy context=\"default\"><allow send_destination=\"*\"/><allow receive_sender=\"*\"/>\
             <allow own=\"*\"/></policy></busconfig>", home.path().display()
        )).unwrap();
        let mut bus = Command::new("dbus-daemon")
            .arg(format!("--config-file={}", config.display()))
            .args(["--nofork", "--print-address=1"])
            .env("HOME", home.path())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        struct Reap(std::process::Child);
        impl Drop for Reap {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut address = String::new();
        BufReader::new(bus.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        let _bus = Reap(bus);
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env("CUA_PORTAL_OWNER_TEST", name)
            .env("DBUS_SESSION_BUS_ADDRESS", address.trim())
            .env("HOME", home.path())
            .env("XDG_CONFIG_HOME", home.path())
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("LIBEI_SOCKET")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("private portal test timed out: {name}");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
    }

    pub(crate) async fn assert_name_gone(connection: &zbus::Connection, name: &str) {
        tokio::time::timeout(PROBE_TIMEOUT, async {
            let dbus = zbus::fdo::DBusProxy::new(connection).await.unwrap();
            while dbus.name_has_owner(name.try_into().unwrap()).await.unwrap() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("closed connection retained its bus name");
    }

    #[test]
    fn per_call_connection_closes_on_success_and_error() {
        private_bus_test(
            concat!(
                module_path!(),
                "::per_call_connection_closes_on_success_and_error"
            ),
            || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                rt.block_on(async {
                    let observer = fresh_session_connection().await.unwrap();
                    for fail in [false, true] {
                        let mut retained = None;
                        let saved = &mut retained;
                        let result = with_connection(|connection| async move {
                            *saved = Some(connection.clone());
                            if fail {
                                anyhow::bail!("provider refused");
                            }
                            Ok(())
                        })
                        .await;
                        assert_eq!(result.is_err(), fail);
                        let connection = retained.unwrap();
                        let name = connection.unique_name().unwrap().clone();
                        assert_name_gone(&observer, name.as_str()).await;
                    }
                    close_connection(observer).await;
                });
                rt.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT);
            },
        );
    }

    #[cfg(any(feature = "portal-input", feature = "portal-capture"))]
    #[derive(Clone)]
    pub(crate) struct FakePortal {
        pub(crate) entered: mpsc::Sender<String>,
        pub(crate) closed: mpsc::Sender<()>,
        pub(crate) stall_close: bool,
    }

    #[cfg(any(feature = "portal-input", feature = "portal-capture"))]
    impl FakePortal {
        pub(crate) async fn serve(&self) -> zbus::Connection {
            let connection = fresh_session_connection().await.unwrap();
            connection
                .request_name("org.freedesktop.portal.Desktop")
                .await
                .unwrap();
            #[cfg(feature = "portal-capture")]
            connection
                .object_server()
                .at("/org/freedesktop/portal/desktop", Screen(self.clone()))
                .await
                .unwrap();
            #[cfg(feature = "portal-input")]
            connection
                .object_server()
                .at("/org/freedesktop/portal/desktop", Remote(self.clone()))
                .await
                .unwrap();
            connection
        }

        async fn create(
            &self,
            options: HashMap<String, OwnedValue>,
            connection: &zbus::Connection,
            header: zbus::message::Header<'_>,
        ) -> zbus::fdo::Result<OwnedObjectPath> {
            let sender = header.sender().unwrap().as_str();
            let prefix = sender.trim_start_matches(':').replace('.', "_");
            let token = <&str>::try_from(options.get("handle_token").unwrap()).unwrap();
            let session_token =
                <&str>::try_from(options.get("session_handle_token").unwrap()).unwrap();
            let session_path =
                format!("/org/freedesktop/portal/desktop/session/{prefix}/{session_token}");
            connection
                .object_server()
                .at(session_path.as_str(), FakeSession(self.clone()))
                .await
                .unwrap();
            let request = OwnedObjectPath::try_from(format!(
                "/org/freedesktop/portal/desktop/request/{prefix}/{token}"
            ))
            .unwrap();
            let result = HashMap::from([("session_handle", Value::from(session_path.as_str()))]);
            connection
                .emit_signal(
                    Some(sender),
                    request.as_str(),
                    "org.freedesktop.portal.Request",
                    "Response",
                    &(0_u32, result),
                )
                .await
                .unwrap();
            Ok(request)
        }

        async fn select(
            &self,
            header: zbus::message::Header<'_>,
        ) -> zbus::fdo::Result<OwnedObjectPath> {
            self.entered
                .send(header.sender().unwrap().to_string())
                .unwrap();
            std::future::pending().await
        }
    }

    #[cfg(feature = "portal-capture")]
    struct Screen(FakePortal);
    #[cfg(feature = "portal-capture")]
    #[zbus::interface(name = "org.freedesktop.portal.ScreenCast")]
    impl Screen {
        #[zbus(property)]
        fn version(&self) -> u32 {
            5
        }
        async fn create_session(
            &self,
            options: HashMap<String, OwnedValue>,
            #[zbus(connection)] connection: &zbus::Connection,
            #[zbus(header)] header: zbus::message::Header<'_>,
        ) -> zbus::fdo::Result<OwnedObjectPath> {
            self.0.create(options, connection, header).await
        }
        async fn select_sources(
            &self,
            _session: OwnedObjectPath,
            _options: HashMap<String, OwnedValue>,
            #[zbus(header)] header: zbus::message::Header<'_>,
        ) -> zbus::fdo::Result<OwnedObjectPath> {
            self.0.select(header).await
        }
    }

    #[cfg(feature = "portal-input")]
    struct Remote(FakePortal);
    #[cfg(feature = "portal-input")]
    #[zbus::interface(name = "org.freedesktop.portal.RemoteDesktop")]
    impl Remote {
        #[zbus(property)]
        fn version(&self) -> u32 {
            2
        }
        async fn create_session(
            &self,
            options: HashMap<String, OwnedValue>,
            #[zbus(connection)] connection: &zbus::Connection,
            #[zbus(header)] header: zbus::message::Header<'_>,
        ) -> zbus::fdo::Result<OwnedObjectPath> {
            self.0.create(options, connection, header).await
        }
        async fn select_devices(
            &self,
            _session: OwnedObjectPath,
            _options: HashMap<String, OwnedValue>,
            #[zbus(header)] header: zbus::message::Header<'_>,
        ) -> zbus::fdo::Result<OwnedObjectPath> {
            self.0.select(header).await
        }
    }

    #[cfg(any(feature = "portal-input", feature = "portal-capture"))]
    struct FakeSession(FakePortal);
    #[cfg(any(feature = "portal-input", feature = "portal-capture"))]
    #[zbus::interface(name = "org.freedesktop.portal.Session")]
    impl FakeSession {
        #[zbus(property)]
        fn version(&self) -> u32 {
            1
        }
        async fn close(&self) {
            self.0.closed.send(()).unwrap();
            if self.0.stall_close {
                std::future::pending::<()>().await;
            }
        }
    }
}
