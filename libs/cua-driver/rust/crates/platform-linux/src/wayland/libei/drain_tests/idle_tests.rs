//! Private D-Bus RemoteDesktop provider + real EIS protocol, public readiness,
//! input and shutdown. Exercise the real 20-second policy (no test clock/hook).
use super::*;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

// Unlike the shorter partial-portal tests, these need to cross the actual idle
// interval. The independent parent watchdog also bounds deadlocked worker joins.
fn isolated(name: &str, test: impl FnOnce()) {
    let name = name.split_once("::").unwrap().1;
    if std::env::var("CUA_LIBEI_IDLE_TEST").as_deref() == Ok(name) {
        test();
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join("bus.conf");
    std::fs::write(
        &config,
        format!(
            "<busconfig><type>session</type><listen>unix:tmpdir={}</listen><auth>EXTERNAL</auth>\
         <policy context=\"default\"><allow send_destination=\"*\"/><allow receive_sender=\"*\"/>\
         <allow own=\"*\"/></policy></busconfig>",
            home.path().display()
        ),
    )
    .unwrap();
    struct Reap(std::process::Child);
    impl Drop for Reap {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut bus = Reap(
        Command::new("dbus-daemon")
            .arg(format!("--config-file={}", config.display()))
            .args(["--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut address = String::new();
    BufReader::new(bus.0.stdout.take().unwrap())
        .read_line(&mut address)
        .unwrap();
    let mut child = Reap(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env("CUA_LIBEI_IDLE_TEST", name)
            .env("DBUS_SESSION_BUS_ADDRESS", address.trim())
            .env("HOME", home.path())
            .env("XDG_CONFIG_HOME", home.path())
            .env_remove("LIBEI_SOCKET")
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(90);
    while child.0.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "idle test timed out: {name}");
        thread::sleep(Duration::from_millis(10));
    }
    let output = child.0.stdout.take().unwrap();
    let errors = child.0.stderr.take().unwrap();
    let mut stdout = String::new();
    let mut stderr = String::new();
    BufReader::new(output).read_to_string(&mut stdout).unwrap();
    BufReader::new(errors).read_to_string(&mut stderr).unwrap();
    assert!(child.0.wait().unwrap().success(), "{stdout}\n{stderr}");
    assert!(stdout.contains("1 passed"), "child ran no test: {stdout}");
}

struct Peer {
    server: eis::Context,
    socket: UnixStream,
}
impl Peer {
    fn new(socket: UnixStream) -> Self {
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        Self {
            server: eis::Context::new(socket.try_clone().unwrap()).unwrap(),
            socket,
        }
    }
    fn until(&self, mut predicate: impl FnMut(&eis::Request) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(Instant::now() < deadline, "EIS peer stalled");
            assert!(readable(
                &self.socket,
                deadline.saturating_duration_since(Instant::now())
            ));
            self.server.read().unwrap();
            while let Some(result) = self.server.pending_request() {
                match result {
                    PendingRequestResult::Request(request) if predicate(&request) => return,
                    PendingRequestResult::Request(_) => (),
                    other => panic!("invalid EIS request: {other:?}"),
                }
            }
        }
    }
    fn negotiate(&self) {
        self.server.handshake().handshake_version(1);
        self.server.flush().unwrap();
        self.until(|r| {
            matches!(
                r,
                eis::Request::Handshake(_, eis::handshake::Request::Finish)
            )
        });
        let connection = self.server.handshake().connection(1, 1);
        let seat = connection.seat(1);
        seat.capability(1, "ei_keyboard");
        seat.capability(2, "ei_pointer_absolute");
        seat.capability(4, "ei_button");
        seat.done();
        self.server.flush().unwrap();
        self.until(|r| matches!(r, eis::Request::Seat(_, eis::seat::Request::Bind { .. })));
        let device = seat.device(1);
        device.device_type(eis::device::DeviceType::Virtual);
        let _: eis::Keyboard = device.interface(1);
        let _: eis::PointerAbsolute = device.interface(1);
        let _: eis::Button = device.interface(1);
        device.region(0, 0, 1024, 768, 1.0);
        device.done();
        device.resumed(2);
        self.server.flush().unwrap();
        self.until(|r| {
            matches!(
                r,
                eis::Request::Device(_, eis::device::Request::StartEmulating { .. })
            )
        });
    }
    fn read_key(&self) {
        use eis::keyboard::KeyState::{Press, Released};
        let mut keys = Vec::new();
        let mut frames = 0;
        self.until(|r| {
            match r {
                eis::Request::Keyboard(_, eis::keyboard::Request::Key { key, state }) => {
                    keys.push((*key, *state))
                }
                eis::Request::Device(_, eis::device::Request::Frame { .. }) => frames += 1,
                _ => (),
            }
            keys.len() == 2 && frames == 2
        });
        assert_eq!(keys, [(30, Press), (30, Released)]);
    }
    fn assert_eof(&mut self) {
        let mut bytes = Vec::new();
        self.socket.read_to_end(&mut bytes).unwrap();
        assert!(
            bytes.is_empty(),
            "input appeared during retirement: {bytes:?}"
        );
    }
}

#[derive(Debug)]
struct Closed {
    client: String,
    eis_closed: bool,
    raw_excluded: bool,
    browser_excluded: bool,
}
#[derive(Clone)]
struct Provider {
    peers: mpsc::Sender<Peer>,
    selected: mpsc::Sender<Option<String>>,
    closed: mpsc::Sender<Closed>,
    sessions: Arc<Mutex<HashMap<OwnedObjectPath, Arc<Mutex<Option<UnixStream>>>>>>,
    park_close: Arc<AtomicBool>,
    release_close: Arc<tokio::sync::Notify>,
}
impl Provider {
    async fn respond(
        &self,
        options: HashMap<String, OwnedValue>,
        connection: &zbus::Connection,
        header: zbus::message::Header<'_>,
        result: HashMap<&str, Value<'_>>,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        let sender = header.sender().unwrap().as_str();
        let prefix = sender.trim_start_matches(':').replace('.', "_");
        let token = <&str>::try_from(options.get("handle_token").unwrap()).unwrap();
        let request = OwnedObjectPath::try_from(format!(
            "/org/freedesktop/portal/desktop/request/{prefix}/{token}"
        ))
        .unwrap();
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
    fn assert_lease_free(&self) {
        drop(
            global()
                .acquire_blocking(LeaseRequest::desktop_raw(None, Duration::ZERO))
                .expect("portal opening held the raw-input lease"),
        );
    }
}
#[zbus::interface(name = "org.freedesktop.portal.RemoteDesktop")]
impl Provider {
    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        2
    }
    async fn create_session(
        &self,
        options: HashMap<String, OwnedValue>,
        #[zbus(connection)] connection: &zbus::Connection,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        self.assert_lease_free();
        let prefix = header
            .sender()
            .unwrap()
            .as_str()
            .trim_start_matches(':')
            .replace('.', "_");
        let token = <&str>::try_from(options.get("session_handle_token").unwrap()).unwrap();
        let path = OwnedObjectPath::try_from(format!(
            "/org/freedesktop/portal/desktop/session/{prefix}/{token}"
        ))
        .unwrap();
        let socket = Arc::new(Mutex::new(None));
        self.sessions
            .lock()
            .unwrap()
            .insert(path.clone(), socket.clone());
        connection
            .object_server()
            .at(
                path.clone(),
                Session {
                    provider: self.clone(),
                    socket,
                    client: header.sender().unwrap().to_string(),
                },
            )
            .await
            .unwrap();
        self.respond(
            options,
            connection,
            header,
            HashMap::from([("session_handle", Value::from(path.as_str()))]),
        )
        .await
    }
    async fn select_devices(
        &self,
        _session: OwnedObjectPath,
        options: HashMap<String, OwnedValue>,
        #[zbus(connection)] connection: &zbus::Connection,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        self.assert_lease_free();
        assert_eq!(
            u32::try_from(options.get("persist_mode").unwrap()).unwrap(),
            2
        );
        assert_eq!(u32::try_from(options.get("types").unwrap()).unwrap(), 3);
        self.selected
            .send(
                options
                    .get("restore_token")
                    .map(|v| <&str>::try_from(v).unwrap().to_owned()),
            )
            .unwrap();
        self.respond(options, connection, header, HashMap::new())
            .await
    }
    async fn start(
        &self,
        _session: OwnedObjectPath,
        parent: String,
        options: HashMap<String, OwnedValue>,
        #[zbus(connection)] connection: &zbus::Connection,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        self.assert_lease_free();
        assert_eq!(parent, "wayland:");
        self.respond(
            options,
            connection,
            header,
            HashMap::from([
                ("devices", Value::from(3_u32)),
                ("restore_token", Value::from("rotated-token")),
            ]),
        )
        .await
    }
    #[zbus(name = "ConnectToEIS")]
    fn connect_to_eis(
        &self,
        session: OwnedObjectPath,
        _options: HashMap<String, OwnedValue>,
    ) -> zbus::zvariant::OwnedFd {
        self.assert_lease_free();
        let (client, socket) = UnixStream::pair().unwrap();
        *self
            .sessions
            .lock()
            .unwrap()
            .get(&session)
            .unwrap()
            .lock()
            .unwrap() = Some(socket.try_clone().unwrap());
        self.peers.send(Peer::new(socket)).unwrap();
        zbus::zvariant::OwnedFd::from(std::os::fd::OwnedFd::from(client))
    }
}
struct Session {
    provider: Provider,
    socket: Arc<Mutex<Option<UnixStream>>>,
    client: String,
}
#[zbus::interface(name = "org.freedesktop.portal.Session")]
impl Session {
    async fn close(&self) {
        let eis_closed = {
            let socket = self.socket.lock().unwrap();
            let mut byte = 0_u8;
            socket.as_ref().is_none_or(|socket| unsafe {
                libc::recv(
                    socket.as_raw_fd(),
                    (&mut byte as *mut u8).cast(),
                    1,
                    libc::MSG_DONTWAIT | libc::MSG_PEEK,
                ) == 0
            })
        };
        let raw_excluded = global()
            .acquire_blocking(LeaseRequest::desktop_raw(None, Duration::ZERO))
            .is_err();
        let browser_excluded = global()
            .acquire_blocking(LeaseRequest::browser_profile(
                "independent-profile",
                Duration::ZERO,
            ))
            .is_err();
        let park = self.provider.park_close.swap(false, Ordering::AcqRel);
        self.provider
            .closed
            .send(Closed {
                client: self.client.clone(),
                eis_closed,
                raw_excluded,
                browser_excluded,
            })
            .unwrap();
        if park {
            self.provider.release_close.notified().await;
        }
    }
}

struct PortalFixture {
    rt: Option<tokio::runtime::Runtime>,
    service: Option<zbus::Connection>,
    peers: mpsc::Receiver<Peer>,
    selected: mpsc::Receiver<Option<String>>,
    closed: mpsc::Receiver<Closed>,
    provider: Provider,
}
impl PortalFixture {
    fn new() -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let (peers, receiver) = mpsc::channel();
        let (selected, selections) = mpsc::channel();
        let (closed, closures) = mpsc::channel();
        let provider = Provider {
            peers,
            selected,
            closed,
            sessions: Default::default(),
            park_close: Arc::new(AtomicBool::new(false)),
            release_close: Default::default(),
        };
        let service = rt.block_on(async {
            let connection = super::super::super::portal::fresh_session_connection()
                .await
                .unwrap();
            connection
                .request_name("org.freedesktop.portal.Desktop")
                .await
                .unwrap();
            connection
                .object_server()
                .at("/org/freedesktop/portal/desktop", provider.clone())
                .await
                .unwrap();
            connection
        });
        Self {
            rt: Some(rt),
            service: Some(service),
            peers: receiver,
            selected: selections,
            closed: closures,
            provider,
        }
    }
    fn open(&self) -> Peer {
        let opener = thread::spawn(|| reconnect_for_readiness("ei_keyboard"));
        let peer = match self.peers.recv_timeout(Duration::from_secs(5)) {
            Ok(peer) => peer,
            Err(error) => panic!("no EIS peer: {error}; opener: {:?}", opener.join().unwrap()),
        };
        peer.negotiate();
        opener.join().unwrap().unwrap();
        peer
    }
    fn closed(&self, timeout: Duration) -> Closed {
        self.closed
            .recv_timeout(timeout)
            .expect("portal session did not close within its idle bound")
    }
    fn assert_name_gone(&self, name: &str) {
        self.rt
            .as_ref()
            .unwrap()
            .block_on(super::super::super::portal::tests::assert_name_gone(
                self.service.as_ref().unwrap(),
                name,
            ));
    }
}
impl Drop for PortalFixture {
    fn drop(&mut self) {
        self.provider.release_close.notify_one();
        shutdown();
        let rt = self.rt.take().unwrap();
        rt.block_on(super::super::super::portal::close_connection(
            self.service.take().unwrap(),
        ));
        rt.shutdown_timeout(super::super::super::portal::RUNTIME_SHUTDOWN_TIMEOUT);
    }
}

#[test]
fn portal_idle_retires_in_order_and_reopens_from_persisted_token() {
    isolated(
        concat!(
            module_path!(),
            "::portal_idle_retires_in_order_and_reopens_from_persisted_token"
        ),
        || {
            assert_eq!(PORTAL_IDLE_TIMEOUT, Duration::from_secs(20));
            let fixture = PortalFixture::new();
            write_restore_token("initial-token").unwrap();
            let mut first = fixture.open();
            assert_eq!(
                fixture.selected.recv().unwrap().as_deref(),
                Some("initial-token")
            );
            assert_eq!(read_restore_token().as_deref(), Some("rotated-token"));
            let old_sender = tx().unwrap();
            let worker_id = TX.lock().unwrap().as_ref().unwrap().thread.thread().id();
            fixture.provider.park_close.store(true, Ordering::Release);
            let started = Instant::now();
            let closed = fixture.closed(Duration::from_secs(23));
            assert!(
                started.elapsed() >= Duration::from_millis(19_800),
                "portal retired early"
            );
            assert!(
                closed.eis_closed,
                "Session.Close raced the still-live EIS context"
            );
            assert!(
                closed.raw_excluded && closed.browser_excluded,
                "idle close did not exclude admitted raw/browser input"
            );
            first.assert_eof();
            // Start readiness while Session.Close is parked. No replacement session
            // may open until the old session/connection/runtime have been retired.
            write_restore_token("changed-on-disk").unwrap();
            let opener = thread::spawn(|| reconnect_for_readiness("ei_keyboard"));
            assert!(
                matches!(
                    fixture.peers.recv_timeout(Duration::from_millis(100)),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ),
                "reopen overlapped old portal close"
            );
            fixture.provider.release_close.notify_one();
            let second = fixture.peers.recv_timeout(Duration::from_secs(5)).unwrap();
            second.negotiate();
            opener.join().unwrap().unwrap();
            fixture.assert_name_gone(&closed.client);
            assert_eq!(
                fixture.selected.recv().unwrap().as_deref(),
                Some("changed-on-disk")
            );
            assert_eq!(
                TX.lock().unwrap().as_ref().unwrap().thread.thread().id(),
                worker_id,
                "idle retirement replaced/detached the worker"
            );
            press_key(30).unwrap();
            second.read_key();
            shutdown();
            assert!(
                TX.lock().unwrap().is_none(),
                "shutdown left an owned worker behind"
            );
            assert_eq!(
                old_sender
                    .wait_ready("ei_keyboard")
                    .unwrap_err()
                    .to_string(),
                "libei worker is shutting down"
            );
            restart();
            let third = fixture.open();
            press_key(30).unwrap();
            third.read_key();
            shutdown();
            assert!(TX.lock().unwrap().is_none());
        },
    );
}

struct ParkedInput {
    def: ToolDef,
    entered: mpsc::Sender<()>,
    resume: Receiver<()>,
}
#[async_trait::async_trait]
impl Tool for ParkedInput {
    fn def(&self) -> &ToolDef {
        &self.def
    }
    async fn invoke(&self, _: serde_json::Value) -> ToolResult {
        if self.def.name == "browser_prepare" {
            // Profile admission is real and must not be broadened to a seat
            // lease merely to defeat retirement during trusted confirmation.
            drop(
                global()
                    .acquire_blocking(LeaseRequest::desktop_raw(None, Duration::ZERO))
                    .unwrap(),
            );
            let error = global()
                .acquire_blocking(LeaseRequest::browser_profile("pid:937453", Duration::ZERO))
                .unwrap_err();
            assert_eq!(error.code(), "target_busy");
        }
        self.entered.send(()).unwrap();
        self.resume.recv_timeout(Duration::from_secs(25)).unwrap();
        match press_key(30) {
            Ok(()) => ToolResult::text("drained"),
            Err(error) => ToolResult::error(error.to_string()),
        }
    }
}
fn tokio_thread_ids() -> std::collections::HashSet<String> {
    std::fs::read_dir("/proc/self/task")
        .unwrap()
        .filter_map(|entry| {
            let entry = entry.unwrap();
            match std::fs::read_to_string(entry.path().join("comm")) {
                Ok(name) if name.trim() == "tokio-rt-worker" => {
                    Some(entry.file_name().to_string_lossy().into_owned())
                }
                Ok(_) => None,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => panic!("cannot inspect owned runtime thread: {error}"),
            }
        })
        .collect()
}

#[test]
fn portal_idle_cannot_close_admitted_action_and_input_renews_idle() {
    isolated(
        concat!(
            module_path!(),
            "::portal_idle_cannot_close_admitted_action_and_input_renews_idle"
        ),
        || admitted_action_renews_idle("press_key", serde_json::json!({})),
    );
}

#[test]
fn portal_idle_cannot_close_browser_profile_dispatch_during_confirmation() {
    isolated(
        concat!(
            module_path!(),
            "::portal_idle_cannot_close_browser_profile_dispatch_during_confirmation"
        ),
        || {
            admitted_action_renews_idle(
                "browser_prepare",
                serde_json::json!({
                    "pid": 937453, "strategy": {"kind":"existing_profile"}
                }),
            )
        },
    );
}

fn admitted_action_renews_idle(tool: &'static str, args: serde_json::Value) {
    let fixture = PortalFixture::new();
    let baseline_threads = tokio_thread_ids();
    let mut peer = fixture.open();
    assert!(
        tokio_thread_ids().len() > baseline_threads.len(),
        "portal open created no runtime worker"
    );
    let (entered, entering) = mpsc::channel();
    let (resume, resuming) = bounded(1);
    let action = thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(ParkedInput {
            def: ToolDef {
                name: tool.into(),
                description: "park after real dispatch admission".into(),
                input_schema: serde_json::json!({"type":"object"}),
                read_only: false,
                destructive: false,
                idempotent: false,
                open_world: false,
            },
            entered,
            resume: resuming,
        }));
        rt.block_on(registry.invoke(tool, args))
    });
    entering.recv_timeout(Duration::from_secs(3)).unwrap();
    // No command is queued yet: only the real registry dispatch grant closes
    // the readiness-to-first-input race. Release the action before asserting
    // so a deliberate-break failure cannot strand the fixture thread.
    let prematurely_closed = fixture
        .closed
        .recv_timeout(Duration::from_millis(20_500))
        .ok();
    resume.send(()).unwrap();
    let result = action.join().unwrap();
    assert!(
        prematurely_closed.is_none(),
        "idle retirement closed an admitted action: {prematurely_closed:?}"
    );
    assert_ne!(
        result.is_error,
        Some(true),
        "first input failed: {result:?}"
    );
    peer.read_key();
    let completed = Instant::now();
    let closed = fixture.closed(Duration::from_secs(23));
    assert!(
        completed.elapsed() >= Duration::from_millis(19_800),
        "input completion did not renew the idle interval"
    );
    assert!(closed.eis_closed && closed.raw_excluded);
    fixture.assert_name_gone(&closed.client);
    peer.assert_eof();
    assert_eq!(
        press_key(30).unwrap_err().to_string(),
        no_live_context().to_string()
    );
    assert_eq!(
        wait_keyboard_ready().unwrap_err().to_string(),
        no_live_context().to_string()
    );
    assert!(
        fixture.peers.try_recv().is_err(),
        "input reopened the portal after idle retirement"
    );
    assert!(!tx().unwrap().live.load(Ordering::Acquire));
    // The provider's own lazily-created blocking threads may expire too;
    // no thread added by the portal owner may survive its retirement.
    assert!(
        tokio_thread_ids().is_subset(&baseline_threads),
        "portal runtime worker survived retirement"
    );
}

#[test]
fn browser_cleanup_reopens_after_idle_and_retry_without_holding_the_seat() {
    isolated(
        concat!(
            module_path!(),
            "::browser_cleanup_reopens_after_idle_and_retry_without_holding_the_seat"
        ),
        || {
            // Pin this exercised boundary to the adapter used by both session
            // end and expired-grant retry, before any destructive cleanup.
            let source = include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/src/browser_platform.rs"
            ));
            let body = source
                .split_once("fn cleanup_existing_profile_setup(")
                .unwrap()
                .1
                .split_once("async fn abort_existing_profile_setup(")
                .unwrap()
                .0;
            assert!(
                body.find("with_existing_profile_cleanup_input(pid, window_id, || {")
                    .unwrap()
                    < body.find("crate::browser_consent_ui::dismiss").unwrap()
            );
            let abort = source
                .split_once("async fn abort_existing_profile_setup(")
                .unwrap()
                .1
                .split_once("async fn handle_existing_profile_consent(")
                .unwrap()
                .0;
            assert!(
                abort
                    .find("with_existing_profile_cleanup_input(pid, request.window_id, || {")
                    .unwrap()
                    < abort
                        .find("crate::browser_setup_ui::abort_pending")
                        .unwrap()
            );
            let fixture = PortalFixture::new();
            let mut first = fixture.open();
            fixture.closed(Duration::from_secs(23));
            first.assert_eof();
            // Discover only the isolated GNOME facts, never the user's compositor.
            std::env::set_var("CUA_DRIVER_RS_ENABLE_WAYLAND", "1");
            std::env::set_var("WAYLAND_DISPLAY", "private-fixture-no-socket");
            std::env::set_var("XDG_CURRENT_DESKTOP", "GNOME");
            let service = fixture.service.as_ref().unwrap();
            let rt = fixture.rt.as_ref().unwrap();
            rt.block_on(service.release_name("org.freedesktop.portal.Desktop"))
                .unwrap();
            let failed =
                crate::browser_platform::with_existing_profile_cleanup_input(937453, 21, || {
                    panic!("cleanup ran without transport readiness");
                    #[allow(unreachable_code)]
                    Ok(false)
                })
                .unwrap_err();
            assert_eq!(
                failed.code,
                cua_driver_core::browser::refusal::BrowserRefusalCode::BrowserRouteUnavailable
            );
            drop(
                global()
                    .acquire_blocking(LeaseRequest::desktop_raw(None, Duration::ZERO))
                    .unwrap(),
            );
            rt.block_on(service.request_name("org.freedesktop.portal.Desktop"))
                .unwrap();
            let (entered, entering) = mpsc::channel();
            let (resume, resuming) = bounded(1);
            let cleanup = thread::spawn(move || {
                crate::browser_platform::with_existing_profile_cleanup_input(937453, 21, || {
                    entered.send(()).unwrap();
                    resuming.recv_timeout(Duration::from_secs(25)).unwrap();
                    press_key(30).map_err(|e| cua_driver_core::browser::refusal::BrowserRefusal::new(
                cua_driver_core::browser::refusal::BrowserRefusalCode::BrowserRouteUnavailable, e.to_string()))?;
                    Ok(true)
                })
            });
            let mut second = fixture.peers.recv_timeout(Duration::from_secs(5)).unwrap();
            // The production cleanup boundary reopens before acquiring the seat.
            drop(
                global()
                    .acquire_blocking(LeaseRequest::desktop_raw(None, Duration::ZERO))
                    .unwrap(),
            );
            second.negotiate();
            entering.recv_timeout(Duration::from_secs(3)).unwrap();
            assert_eq!(
                global()
                    .acquire_blocking(LeaseRequest::desktop_raw(None, Duration::ZERO))
                    .unwrap_err()
                    .code(),
                "input_busy"
            );
            let premature = fixture
                .closed
                .recv_timeout(Duration::from_millis(20_500))
                .ok();
            resume.send(()).unwrap();
            let result = cleanup.join().unwrap();
            assert!(
                premature.is_none(),
                "cleanup transport retired: {premature:?}"
            );
            assert_eq!(result.unwrap(), true);
            second.read_key();
            let closed = fixture.closed(Duration::from_secs(23));
            assert!(closed.eis_closed && closed.raw_excluded && closed.browser_excluded);
            fixture.assert_name_gone(&closed.client);
            second.assert_eof();
        },
    );
}

#[test]
fn portal_warm_readiness_renews_idle_without_emitting_input() {
    isolated(
        concat!(
            module_path!(),
            "::portal_warm_readiness_renews_idle_without_emitting_input"
        ),
        || {
            let fixture = PortalFixture::new();
            let mut peer = fixture.open();
            thread::sleep(Duration::from_secs(10));
            reconnect_for_readiness("ei_keyboard").unwrap();
            assert!(
                fixture.peers.try_recv().is_err(),
                "warm readiness opened a second session"
            );
            assert!(
                !readable(&peer.socket, Duration::from_millis(20)),
                "readiness emitted input"
            );
            let refreshed = Instant::now();
            let closed = fixture.closed(Duration::from_secs(23));
            assert!(
                refreshed.elapsed() >= Duration::from_millis(19_800),
                "warm readiness did not renew idle"
            );
            assert!(closed.eis_closed && closed.raw_excluded);
            fixture.assert_name_gone(&closed.client);
            peer.assert_eof();
        },
    );
}

#[test]
fn direct_env_socket_survives_the_portal_idle_interval() {
    isolated(
        concat!(
            module_path!(),
            "::direct_env_socket_survives_the_portal_idle_interval"
        ),
        || {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("eis.sock");
            let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
            std::env::set_var("LIBEI_SOCKET", &path);
            let opener = thread::spawn(|| reconnect_for_readiness("ei_keyboard"));
            let mut peer = Peer::new(listener.accept().unwrap().0);
            peer.negotiate();
            opener.join().unwrap().unwrap();
            thread::sleep(Duration::from_millis(20_500));
            assert!(tx().unwrap().live.load(Ordering::Acquire));
            press_key(30).unwrap();
            peer.read_key();
            shutdown();
            peer.assert_eof();
            assert!(TX.lock().unwrap().is_none());
            std::env::remove_var("LIBEI_SOCKET");
        },
    );
}
