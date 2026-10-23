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
    call_on(owner(), destination, path, method, args, timeout)
}

fn call_on(
    owner: &runtime::Owner,
    destination: &str,
    path: &str,
    method: &str,
    args: &[String],
    timeout: Duration,
) -> Option<String> {
    let destination = destination.to_owned();
    let path = path.to_owned();
    let method = method.to_owned();
    let args = args.to_vec();
    let action_lease = cua_driver_core::blocking::ActionLeaseScope::capture();
    let (tx, rx) = mpsc::sync_channel(1);
    owner.spawn(move |generation| async move {
        let mut used = None;
        let result = tokio::time::timeout(timeout, async {
            let connection = {
                let mut shared = generation.connection.lock().await;
                if shared.is_none() {
                    *shared = Some(
                        generation.session(
                            Connection::session()
                                .await
                                .map_err(|_| Failure::Transport)?,
                        ),
                    );
                }
                shared.as_ref().ok_or(Failure::Transport)?.clone()
            };
            used = Some(connection.clone());
            // A connection wait may outlive the native caller. Do not begin a
            // new focus mutation after cancellation; terminal reconciliation
            // methods must remain available for unconditional restoration.
            if method.ends_with("BeginForeground") {
                action_lease
                    .run(cua_driver_core::blocking::check_cancelled)
                    .map_err(|_| Failure::Reply)?;
            }
            invoke(&connection.connection, &destination, &path, &method, &args).await
        })
        .await
        .unwrap_or(Err(Failure::Transport));
        // Never replay a possibly applied mutation. Retire only the connection
        // this request used: an older failure cannot evict its replacement.
        // Its last user schedules a bounded close without interrupting peers.
        if matches!(result, Err(Failure::Transport)) {
            if let Some(connection) = used.as_ref() {
                generation.retire(connection).await;
            }
        }
        drop(used);
        let _ = tx.send(result.ok());
    })?;
    rx.recv_timeout(timeout + Duration::from_millis(100))
        .ok()
        .flatten()
}

#[derive(Debug, PartialEq, Eq)]
enum Failure {
    /// The peer answered with an error, or the call or reply had an
    /// unexpected shape. The connection is healthy.
    Reply,
    /// The connection failed or the call timed out.
    Transport,
}

fn classify(error: zbus::Error) -> Failure {
    match error {
        zbus::Error::MethodError(..) | zbus::Error::FDO(_) => Failure::Reply,
        _ => Failure::Transport,
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
            | "QueryForeground"
            | "AbortForeground"
            | "ValidateForeground"
            | "EndForeground"
            | "CommitForeground",
            [value],
        ) => send!((value.as_str(),)),
        ("GetRects" | "GetVersion" | "GetCapabilities", []) => send!(()),
        ("BeginForeground", [transaction, target]) => {
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
        fn begin_foreground(&self, transaction: &str, target: &str) -> String {
            format!("{transaction}:{target}")
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
                Some("(uint32 14,)")
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
        assert_eq!(request.await.unwrap(), None);
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
            Err(Failure::Reply)
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
