//! Persistent session-bus transport for synchronous compositor helpers.
//! A separate runtime also serves callers already inside Tokio without nested block_on.
mod runtime;

use std::sync::{mpsc, OnceLock};
use std::time::Duration;
use zbus::{Connection, Message};

fn owner() -> &'static runtime::Owner {
    static OWNER: OnceLock<runtime::Owner> = OnceLock::new();
    OWNER.get_or_init(runtime::Owner::default)
}

fn cursor_owner() -> &'static runtime::Owner {
    static OWNER: OnceLock<runtime::Owner> = OnceLock::new();
    OWNER.get_or_init(runtime::Owner::default)
}

pub(super) fn restart() {
    owner().restart();
    cursor_owner().restart();
}

pub(super) fn cursor_call(destination: &str, method: &str, args: &[String]) -> Option<String> {
    call_on(
        cursor_owner(),
        destination,
        super::PATH,
        method,
        args,
        Duration::from_secs(25),
    )
    .ok()
}

/// Retire the current transport generation without initializing a cold one.
/// A later lifecycle can lazily create a fresh executor and session connection.
pub(super) fn shutdown() {
    owner().shutdown();
    cursor_owner().shutdown();
}

pub(super) fn call(
    destination: &str,
    path: &str,
    method: &str,
    args: &[String],
    timeout: Duration,
) -> Option<String> {
    call_result(destination, path, method, args, timeout).ok()
}

/// Required operations retain the actual peer refusal; best-effort discovery
/// and cursor callers may deliberately discard it through `call`.
pub(super) fn call_result(
    destination: &str,
    path: &str,
    method: &str,
    args: &[String],
    timeout: Duration,
) -> Result<String, Failure> {
    call_on(owner(), destination, path, method, args, timeout)
}

fn call_on(
    owner: &runtime::Owner,
    destination: &str,
    path: &str,
    method: &str,
    args: &[String],
    timeout: Duration,
) -> Result<String, Failure> {
    let destination = destination.to_owned();
    let path = path.to_owned();
    let method = method.to_owned();
    let args = args.to_vec();
    let action_lease = cua_driver_core::blocking::ActionLeaseScope::capture();
    let (tx, rx) = mpsc::sync_channel(1);
    owner
        .spawn(move |generation| async move {
            let mut used = None;
            let result = tokio::time::timeout(timeout, async {
                let connection = {
                    let mut shared = generation.connection.lock().await;
                    if shared.is_none() {
                        *shared = Some(
                            generation.session(Connection::session().await.map_err(classify)?),
                        );
                    }
                    shared.as_ref().ok_or(Failure::Unavailable)?.clone()
                };
                used = Some(connection.clone());
                // A connection wait may outlive the native caller. Do not begin a
                // new focus mutation after cancellation; terminal reconciliation
                // methods must remain available for unconditional restoration.
                if method.ends_with("BeginForeground")
                    || method.ends_with("SwitchToTarget")
                    || method.ends_with("FocusSwitchSearch")
                    || method.ends_with("DeliverSwitchSearch")
                {
                    action_lease
                        .run(cua_driver_core::blocking::check_cancelled)
                        .map_err(|_| Failure::Cancelled)?;
                }
                invoke(&connection.connection, &destination, &path, &method, &args).await
            })
            .await
            .unwrap_or(Err(Failure::Timeout));
            // Never replay a possibly applied mutation. Retire only the connection
            // this request used: an older failure cannot evict its replacement.
            // Its last user schedules a bounded close without interrupting peers.
            if matches!(result, Err(Failure::Transport(_) | Failure::Timeout)) {
                if let Some(connection) = used.as_ref() {
                    generation.retire(connection).await;
                }
            }
            drop(used);
            let _ = tx.send(result);
        })
        .ok_or(Failure::Unavailable)?;
    rx.recv_timeout(timeout + Duration::from_millis(100))
        .map_err(|error| match error {
            mpsc::RecvTimeoutError::Timeout => Failure::Timeout,
            mpsc::RecvTimeoutError::Disconnected => Failure::Unavailable,
        })?
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Failure {
    Reply,
    /// A peer refusal is not a timeout and does not retire a healthy connection.
    Remote {
        name: String,
        message: String,
    },
    Transport(String),
    Timeout,
    Cancelled,
    Unavailable,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Reply => write!(f, "invalid WinRects call or reply shape"),
            Self::Remote { name, message } => write!(f, "D-Bus {name}: {message}"),
            Self::Transport(message) => write!(f, "D-Bus transport failed: {message}"),
            Self::Timeout => write!(f, "D-Bus call timed out"),
            Self::Cancelled => write!(f, "D-Bus call cancelled before dispatch"),
            Self::Unavailable => write!(f, "WinRects transport unavailable or shut down"),
        }
    }
}

impl std::error::Error for Failure {}

fn classify(error: zbus::Error) -> Failure {
    match error {
        zbus::Error::MethodError(name, message, _) => Failure::Remote {
            name: name.to_string(),
            message: message.unwrap_or_default(),
        },
        zbus::Error::FDO(error) => Failure::Remote {
            name: zbus::DBusError::name(error.as_ref()).to_string(),
            message: zbus::DBusError::description(error.as_ref())
                .unwrap_or_default()
                .to_owned(),
        },
        error => Failure::Transport(error.to_string()),
    }
}

// Preserve the small textual reply contract consumed by shell_helper's existing
// parsers. Bodies on the wire are typed; strings are never shell or GVariant code.
async fn invoke(
    connection: &Connection,
    destination: &str,
    path: &str,
    method: &str,
    args: &[String],
) -> Result<String, Failure> {
    let (interface, member) = method.rsplit_once('.').ok_or(Failure::Reply)?;
    let parse_u32 = |value: &String| value.parse::<u32>().map_err(|_| Failure::Reply);
    macro_rules! send {
        ($body:expr) => {
            connection
                .call_method(Some(destination), path, Some(interface), member, &$body)
                .await
                .map_err(classify)?
        };
    }
    let reply: Message = match (member, args) {
        (
            "GetNameOwner"
            | "GetConnectionUnixProcessID"
            | "GetConnectionUnixUser"
            | "CaptureTarget"
            | "FocusSwitchSearch"
            | "CancelSwitchSearch"
            | "QueryForeground"
            | "AbortForeground"
            | "ValidateForeground"
            | "EndForeground"
            | "CommitForeground",
            [value],
        ) => send!((value.as_str(),)),
        ("GetRects" | "GetVersion" | "GetCapabilities" | "GetSwitchState" | "GetActive", []) => {
            send!(())
        }
        ("BeginForeground" | "SwitchToTarget" | "DeliverSwitchSearch", [transaction, target]) => {
            send!((transaction.as_str(), target.as_str()))
        }
        ("CaptureTargetPreview", [target, max]) => send!((target.as_str(), parse_u32(max)?)),
        ("Get", [interface, property])
            if interface == "org.gnome.Shell.Introspect" && property == "ScreenSize" =>
        {
            send!((interface.as_str(), property.as_str()))
        }
        ("MoveCursorFor" | "ClickPulseFor", [owner, target, x, y]) => {
            let x = x.parse::<i32>().map_err(|_| Failure::Reply)?;
            let y = y.parse::<i32>().map_err(|_| Failure::Reply)?;
            send!((owner.as_str(), target.as_str(), x, y))
        }
        ("SetCursorColorFor" | "SetSessionLabelFor", [owner, value]) => {
            send!((owner.as_str(), value.as_str()))
        }
        ("SetCursorStateFor", [owner, action, delivery, target, active]) => {
            let active = active.parse::<bool>().map_err(|_| Failure::Reply)?;
            send!((
                owner.as_str(),
                action.as_str(),
                delivery.as_str(),
                target.as_str(),
                active
            ))
        }
        ("HideCursorFor" | "RemoveCursor", [owner]) => send!((owner.as_str(),)),
        // Cursor calls use a separate owned transport generation/unique name;
        // legacy global capture/activation are never routed by this transport.
        _ => return Err(Failure::Reply),
    };
    let body = reply.body();
    let decoded = match member {
        "GetActive" => body
            .deserialize::<bool>()
            .ok()
            .map(|value| value.to_string()),
        "MoveCursorFor" | "ClickPulseFor" | "SetCursorColorFor" | "SetCursorStateFor"
        | "SetSessionLabelFor" | "HideCursorFor" | "RemoveCursor" => {
            body.deserialize::<()>().ok().map(|()| String::new())
        }
        "GetNameOwner" => body
            .deserialize::<String>()
            .ok()
            .map(|value| format!("('{value}',)")),
        "GetVersion" | "GetConnectionUnixProcessID" | "GetConnectionUnixUser" => body
            .deserialize::<u32>()
            .ok()
            .map(|value| format!("(uint32 {value},)")),
        "Get" => body
            .deserialize::<zbus::zvariant::OwnedValue>()
            .ok()
            .and_then(|value| <(i32, i32)>::try_from(value).ok())
            .map(|(width, height)| format!("({width}, {height})")),
        _ => body.deserialize::<String>().ok(),
    };
    decoded.ok_or(Failure::Reply)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;
    use std::process::{Child, Command, Stdio};

    struct PrivateBus(Child);
    impl Drop for PrivateBus {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    // Titles gdbus's GVariant text would escape: an apostrophe, quotes, a
    // backslash, a newline and non-ASCII. A typed reply carries them intact.
    const RECTS: &str = r#"[{"id":3,"pid":41,"title":"Tomorrow's \"plan\" C:\\ 1\n2 飞书","x":0,"y":0,"w":10,"h":10},{"id":7,"pid":42,"title":"Target","x":14,"y":12,"w":800,"h":600}]"#;

    #[derive(Debug, zbus::DBusError)]
    #[zbus(prefix = "org.cua.WinRects")]
    enum Refusal {
        TargetNotActivatable(String),
    }

    struct Helper;
    #[zbus::interface(name = "org.cua.WinRects")]
    impl Helper {
        fn get_version(&self) -> u32 {
            14
        }
        fn get_rects(&self) -> String {
            RECTS.into()
        }
        fn get_capabilities(&self) -> String {
            "{\"protocol_version\":4}".into()
        }
        fn capture_target(&self, target: &str) -> zbus::fdo::Result<String> {
            if target != "epoch:7" {
                return Err(zbus::fdo::Error::Failed("stale_target".into()));
            }
            Ok("{\"target\":\"epoch:7\"}".into())
        }
        fn capture_target_preview(&self, target: &str, max: u32) -> zbus::fdo::Result<String> {
            if max != 800 {
                return Err(zbus::fdo::Error::InvalidArgs("wrong size".into()));
            }
            self.capture_target(target)
        }
        async fn begin_foreground(
            &self,
            transaction: &str,
            target: &str,
        ) -> Result<String, Refusal> {
            if target == "blocked" {
                return Err(Refusal::TargetNotActivatable(
                    "target_not_activatable: reason=shell_key_focus; user's Shell UI 飞书".into(),
                ));
            }
            if target == "stall" {
                return std::future::pending().await;
            }
            Ok(format!("{transaction}:{target}"))
        }
        fn query_foreground(&self, transaction: &str) -> String {
            transaction.into()
        }
    }

    fn private_bus() -> Option<(tempfile::TempDir, PrivateBus, String)> {
        // Use an explicit config: a sandbox (Nix) has no session.conf, and
        // then the daemon exits without printing an address.
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("bus.conf");
        std::fs::write(
            &config,
            format!(
                "<busconfig><type>session</type>\
                 <listen>unix:dir={}</listen><auth>EXTERNAL</auth>\
                 <policy context=\"default\"><allow send_destination=\"*\" eavesdrop=\"true\"/>\
                 <allow eavesdrop=\"true\"/><allow own=\"*\"/></policy></busconfig>",
                dir.path().display()
            ),
        )
        .unwrap();
        let spawned = Command::new("dbus-daemon")
            .arg(format!("--config-file={}", config.display()))
            .args(["--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
            .spawn();
        let mut bus = match spawned {
            Ok(child) => PrivateBus(child),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("skipping: dbus-daemon is not installed");
                return None;
            }
            Err(error) => panic!("private test bus: {error}"),
        };
        let mut address = String::new();
        std::io::BufReader::new(bus.0.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        assert!(!address.trim().is_empty(), "dbus-daemon printed no address");
        Some((dir, bus, address))
    }

    fn install_private_connection(owner: &runtime::Owner, address: String) -> String {
        let (tx, rx) = mpsc::sync_channel(1);
        owner
            .spawn(move |generation| async move {
                let connection = zbus::connection::Builder::address(address.trim())
                    .unwrap()
                    .build()
                    .await
                    .unwrap();
                let name = connection.unique_name().unwrap().to_string();
                *generation.connection.lock().await = Some(generation.session(connection));
                tx.send(name).unwrap();
            })
            .unwrap();
        rx.recv_timeout(Duration::from_secs(2)).unwrap()
    }

    async fn assert_name_gone(observer: &Connection, name: &str) {
        let proxy = zbus::fdo::DBusProxy::new(observer).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while proxy
                .name_has_owner(name.try_into().unwrap())
                .await
                .unwrap()
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("transport's bus name survived teardown");
    }

    #[tokio::test]
    async fn peer_refusal_survives_worker_and_tool_error_without_becoming_timeout() {
        let Some((_dir, _bus, address)) = private_bus() else {
            return;
        };
        let service = zbus::connection::Builder::address(address.trim())
            .unwrap()
            .name("org.cua.WinRects")
            .unwrap()
            .serve_at("/org/cua/WinRects", Helper)
            .unwrap()
            .build()
            .await
            .unwrap();
        let owner = std::sync::Arc::new(runtime::Owner::default());
        let name = install_private_connection(&owner, address);
        for (method, args, code, expected) in [
            ("BeginForeground", vec!["transaction".into(), "blocked".into()], "foreground_unavailable",
             "D-Bus org.cua.WinRects.TargetNotActivatable: target_not_activatable: reason=shell_key_focus; user's Shell UI 飞书"),
            ("CaptureTarget", vec!["stale:7".into()], "tool_invocation_failed",
             "D-Bus org.freedesktop.DBus.Error.Failed: stale_target"),
        ] {
            let caller = owner.clone();
            let error = tokio::task::spawn_blocking(move || call_on(
                &caller, "org.cua.WinRects", "/org/cua/WinRects",
                &format!("org.cua.WinRects.{method}"), &args, Duration::from_secs(2),
            )).await.unwrap().unwrap_err();
            let refusal = super::super::required_call_error(code, method, &error).to_string();
            assert_eq!(refusal, format!("{code}: GNOME {method} failed: {expected}"));
            assert!(!refusal.contains("timed out"));
        }
        // A method refusal must keep the same live connection, not retire it.
        let (tx, rx) = mpsc::sync_channel(1);
        owner
            .spawn(move |generation| async move {
                let current = generation.connection.lock().await;
                tx.send(
                    current
                        .as_ref()
                        .unwrap()
                        .connection
                        .unique_name()
                        .unwrap()
                        .to_string(),
                )
                .unwrap();
            })
            .unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), name);
        let caller = owner.clone();
        let error = tokio::task::spawn_blocking(move || {
            call_on(
                &caller,
                "org.cua.WinRects",
                "/org/cua/WinRects",
                "org.cua.WinRects.BeginForeground",
                &["transaction".into(), "stall".into()],
                Duration::from_millis(100),
            )
        })
        .await
        .unwrap()
        .unwrap_err();
        assert_eq!(error, Failure::Timeout);
        assert_eq!(error.to_string(), "D-Bus call timed out");
        assert_name_gone(&service, &name).await;
        owner.shutdown();
        service.close().await.unwrap();
    }

    #[tokio::test]
    async fn teardown_closes_private_connection_and_reopens() {
        let Some((_dir, _bus, address)) = private_bus() else {
            return;
        };
        let service = zbus::connection::Builder::address(address.trim())
            .unwrap()
            .name("org.cua.WinRects")
            .unwrap()
            .serve_at("/org/cua/WinRects", Helper)
            .unwrap()
            .build()
            .await
            .unwrap();
        let owner = std::sync::Arc::new(runtime::Owner::default());
        // Cold and repeated teardown do not initialize a runtime/connection.
        owner.shutdown();
        for _ in 0..2 {
            owner.restart();
            let name = install_private_connection(&owner, address.clone());
            let caller = owner.clone();
            assert_eq!(
                tokio::task::spawn_blocking(move || call_on(
                    &caller,
                    "org.cua.WinRects",
                    "/org/cua/WinRects",
                    "org.cua.WinRects.GetVersion",
                    &[],
                    Duration::from_secs(2),
                ))
                .await
                .unwrap()
                .as_deref(),
                Ok("(uint32 14,)")
            );
            // Exercise shutdown inside a different, current-thread Tokio runtime.
            owner.shutdown();
            owner.shutdown();
            assert_name_gone(&service, &name).await;
        }
        service.close().await.unwrap();
    }

    struct HoldingHelper(tokio::sync::mpsc::UnboundedSender<()>);

    #[zbus::interface(name = "org.cua.HoldingHelper")]
    impl HoldingHelper {
        async fn query_foreground(&self, _transaction: &str) -> String {
            self.0.send(()).unwrap();
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn teardown_cancels_stalled_rpc_and_closes_its_connection() {
        let Some((_dir, _bus, address)) = private_bus() else {
            return;
        };
        let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
        let service = zbus::connection::Builder::address(address.trim())
            .unwrap()
            .name("org.cua.HoldingHelper")
            .unwrap()
            .serve_at("/org/cua/HoldingHelper", HoldingHelper(started_tx))
            .unwrap()
            .build()
            .await
            .unwrap();
        let owner = std::sync::Arc::new(runtime::Owner::default());
        let name = install_private_connection(&owner, address);
        let caller = owner.clone();
        let request = tokio::task::spawn_blocking(move || {
            call_on(
                &caller,
                "org.cua.HoldingHelper",
                "/org/cua/HoldingHelper",
                "org.cua.HoldingHelper.QueryForeground",
                &["pending".into()],
                Duration::from_secs(10),
            )
        });
        tokio::time::timeout(Duration::from_secs(2), started_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let start = std::time::Instant::now();
        owner.shutdown();
        assert!(start.elapsed() < Duration::from_secs(2));
        assert_eq!(request.await.unwrap(), Err(Failure::Unavailable));
        assert_name_gone(&service, &name).await;
        service.close().await.unwrap();
    }

    #[tokio::test]
    async fn retired_connection_waits_for_peer_and_cannot_evict_replacement() {
        let Some((_dir, _bus, address)) = private_bus() else {
            return;
        };
        let observer = zbus::connection::Builder::address(address.trim())
            .unwrap()
            .build()
            .await
            .unwrap();
        let owner = runtime::Owner::default();
        let (tx, rx) = mpsc::sync_channel(1);
        owner
            .spawn(move |generation| async move {
                let connect = || {
                    zbus::connection::Builder::address(address.trim())
                        .unwrap()
                        .build()
                };
                let old = generation.session(connect().await.unwrap());
                let name = old.connection.unique_name().unwrap().to_string();
                *generation.connection.lock().await = Some(old.clone());
                let peer = old.clone();
                generation.retire(&old).await;
                let replacement = generation.session(connect().await.unwrap());
                *generation.connection.lock().await = Some(replacement.clone());
                // Simulate a second old request failing after reconnect completed.
                generation.retire(&old).await;
                assert!(std::sync::Arc::ptr_eq(
                    generation.connection.lock().await.as_ref().unwrap(),
                    &replacement,
                ));
                drop(old);
                assert_eq!(
                    invoke(
                        &peer.connection,
                        "org.freedesktop.DBus",
                        "/org/freedesktop/DBus",
                        "org.freedesktop.DBus.GetNameOwner",
                        &["org.freedesktop.DBus".into()],
                    )
                    .await
                    .as_deref(),
                    Ok("('org.freedesktop.DBus',)")
                );
                drop(peer);
                tx.send(name).unwrap();
            })
            .unwrap();
        let old_name = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_name_gone(&observer, &old_name).await;
        owner.shutdown();
        observer.close().await.unwrap();
    }

    #[tokio::test]
    async fn typed_roundtrips_on_one_private_connection() {
        let Some((_dir, _bus, address)) = private_bus() else {
            return;
        };
        let service = zbus::connection::Builder::address(address.trim())
            .unwrap()
            .name("org.cua.WinRects")
            .unwrap()
            .serve_at("/org/cua/WinRects", Helper)
            .unwrap()
            .build()
            .await
            .unwrap();
        let client = zbus::connection::Builder::address(address.trim())
            .unwrap()
            .build()
            .await
            .unwrap();
        let call = |method: &'static str, args: Vec<String>| {
            let client = client.clone();
            async move {
                invoke(
                    &client,
                    "org.cua.WinRects",
                    "/org/cua/WinRects",
                    &format!("org.cua.WinRects.{method}"),
                    &args,
                )
                .await
            }
        };
        for _ in 0..3 {
            assert_eq!(
                call("GetVersion", vec![]).await.as_deref(),
                Ok("(uint32 14,)")
            );
            assert_eq!(call("GetRects", vec![]).await.unwrap(), RECTS);
            assert_eq!(
                call("CaptureTarget", vec!["epoch:7".into()])
                    .await
                    .as_deref(),
                Ok("{\"target\":\"epoch:7\"}")
            );
            assert_eq!(
                call("CaptureTargetPreview", vec!["epoch:7".into(), "800".into()])
                    .await
                    .as_deref(),
                Ok("{\"target\":\"epoch:7\"}")
            );
            assert_eq!(
                call(
                    "BeginForeground",
                    vec!["transaction".into(), "epoch:7".into()]
                )
                .await
                .as_deref(),
                Ok("transaction:epoch:7")
            );
            assert_eq!(
                call("QueryForeground", vec!["transaction".into()])
                    .await
                    .as_deref(),
                Ok("transaction")
            );
        }
        assert_eq!(
            call("CaptureTarget", vec!["stale:7".into()]).await,
            Err(Failure::Remote {
                name: "org.freedesktop.DBus.Error.Failed".into(),
                message: "stale_target".into(),
            })
        );
        for method in [
            "Capture",
            "Activate",
            "CaptureWindow",
            "MoveCursor",
            "SetCursorState",
        ] {
            assert_eq!(call(method, vec![]).await, Err(Failure::Reply));
        }
        assert_eq!(
            call("GetVersion", vec![]).await.as_deref(),
            Ok("(uint32 14,)")
        );
        drop(service);
    }
}
