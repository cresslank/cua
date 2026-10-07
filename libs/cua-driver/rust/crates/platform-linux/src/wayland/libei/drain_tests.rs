//! Real reis socketpair + production calloop/command/reply/lease boundary.
//! The peer negotiates a keyboard, then deliberately stops reading. No desktop,
//! portal, environment override or test branch in production code is involved.
use super::*;
use cua_driver_core::action_lease::{global, ExactWindow, LeaseRequest};
use cua_driver_core::protocol::ToolResult;
use cua_driver_core::tool::{Tool, ToolDef, ToolRegistry};
use reis::{eis, PendingRequestResult};
use std::io::Read;
use std::os::unix::net::UnixStream;

// These tests exercise the real process-global dispatch table. Serialize only
// this fixture's global lease assertions, not any production work or gate.
static LEASE_TEST: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
const TEXT_LEN: usize = 32_768;

mod readiness_tests;

fn readable(fd: &impl AsRawFd, timeout: Duration) -> bool {
    let mut pfd = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let result = unsafe { libc::poll(&mut pfd, 1, timeout.as_millis() as i32) };
    assert!(result >= 0, "poll: {}", std::io::Error::last_os_error());
    result > 0
}

struct Fixture {
    sender: CommandSender,
    server: eis::Context,
    peer: UnixStream,
    // Deliberately keep an extra backend reference: terminal discard must also
    // make it impossible for any stale Context clone to transmit the suffix.
    stale: reis::ei::Context,
    worker: Option<thread::JoinHandle<anyhow::Result<()>>>,
}

impl Fixture {
    fn disconnected() -> (Self, reis::ei::Context) {
        let (socket, peer) = UnixStream::pair().unwrap();
        let size: libc::c_int = 4096;
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    socket.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    (&size as *const libc::c_int).cast(),
                    std::mem::size_of_val(&size) as libc::socklen_t,
                )
            },
            0
        );
        let context = reis::ei::Context::new(socket).unwrap();
        let stale = context.clone();
        let server = eis::Context::new(peer.try_clone().unwrap()).unwrap();
        let (tx, rx) = bounded(64);
        let sender = CommandSender::new(tx);
        let live = sender.live.clone();
        let worker = thread::spawn(move || worker(rx, live));
        (
            Self {
                sender,
                server,
                peer,
                stale,
                worker: Some(worker),
            },
            context,
        )
    }

    fn start() -> Self {
        let (mut fixture, context) = Self::disconnected();
        let sender = fixture.sender.clone();
        let ready = thread::spawn(move || {
            sender.connect("ei_keyboard", || Ok((context, PortalKeepAlive::None)))
        });
        fixture.negotiate();
        ready.join().unwrap().unwrap();
        fixture.clear_setup();
        fixture
    }

    fn negotiate(&mut self) {
        self.negotiate_devices(false);
    }

    fn negotiate_devices(&mut self, pointer: bool) {
        self.server.handshake().handshake_version(1);
        self.server.flush().unwrap();
        self.until(|request| {
            matches!(
                request,
                eis::Request::Handshake(_, eis::handshake::Request::Finish)
            )
        });
        let connection = self.server.handshake().connection(1, 1);
        let seat = connection.seat(1);
        seat.capability(1, "ei_keyboard");
        if pointer {
            seat.capability(2, "ei_pointer_absolute");
            seat.capability(4, "ei_button");
            seat.capability(8, "ei_scroll");
        }
        seat.done();
        self.server.flush().unwrap();
        self.until(|request| {
            matches!(
                request,
                eis::Request::Seat(_, eis::seat::Request::Bind { .. })
            )
        });
        let device = seat.device(1);
        device.device_type(eis::device::DeviceType::Virtual);
        let _keyboard: eis::Keyboard = device.interface(1);
        if pointer {
            let _: eis::PointerAbsolute = device.interface(1);
            let _: eis::Button = device.interface(1);
            let _: eis::Scroll = device.interface(1);
            device.region(0, 0, 1024, 768, 1.0);
        }
        device.done();
        device.resumed(2);
        self.server.flush().unwrap();
    }

    fn clear_setup(&mut self) {
        self.server.read().unwrap();
        while self.server.pending_request().is_some() {}
    }

    fn until(&mut self, mut predicate: impl FnMut(&eis::Request) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(Instant::now() < deadline, "EIS negotiation stalled");
            assert!(readable(
                &self.server,
                deadline.saturating_duration_since(Instant::now())
            ));
            self.server.read().unwrap();
            while let Some(request) = self.server.pending_request() {
                match request {
                    PendingRequestResult::Request(request) if predicate(&request) => return,
                    PendingRequestResult::Request(_) => (),
                    other => panic!("invalid request: {other:?}"),
                }
            }
        }
    }

    fn assert_disconnected(&self) {
        assert_eq!(
            self.sender
                .wait_ready("ei_keyboard")
                .unwrap_err()
                .to_string(),
            no_live_context().to_string()
        );
        assert!(!self.sender.live.load(Ordering::Acquire));
    }

    fn read_to_eof(&mut self) -> usize {
        let mut bytes = Vec::new();
        // A disconnected transport is immediately readable through EOF. The
        // kernel may still contain a prefix written BEFORE ownership release.
        self.peer.read_to_end(&mut bytes).unwrap();
        bytes.len()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            let _ = self.sender.send(Cmd::Shutdown);
            let _ = worker.join();
        }
    }
}

impl CommandSender {
    async fn queue_owned_text(&self) -> (Receiver<anyhow::Result<()>>, Arc<CommandCompletion>) {
        struct QueueProbe {
            def: ToolDef,
            sender: CommandSender,
            receipt: Sender<(Receiver<anyhow::Result<()>>, Arc<CommandCompletion>)>,
        }
        #[async_trait::async_trait]
        impl Tool for QueueProbe {
            fn def(&self) -> &ToolDef {
                &self.def
            }
            async fn invoke(&self, _: serde_json::Value) -> ToolResult {
                let (reply, waiter) = bounded(1);
                let completion = self
                    .sender
                    .send(Cmd::TypeText {
                        text: "a".repeat(TEXT_LEN),
                        reply,
                    })
                    .unwrap();
                self.receipt.send((waiter, completion)).unwrap();
                // End the originating invoke scope; only the queued command now
                // owns the grant. No manually held/dropped stand-in lease.
                ToolResult::text("queued")
            }
        }
        let (receipt, receiver) = bounded(1);
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(QueueProbe {
            // A real DesktopRaw dispatch grant, propagated by CommandSender.
            // This unit fixture does not install a platform readiness hook.
            def: ToolDef {
                name: "type_text".into(),
                description: "hermetic libei drain".into(),
                input_schema: serde_json::json!({"type": "object"}),
                read_only: false,
                destructive: false,
                idempotent: false,
                open_world: false,
            },
            sender: self.clone(),
            receipt,
        }));
        let result = registry
            .invoke(
                "type_text",
                serde_json::json!({"pid": 937453, "window_id": 21}),
            )
            .await;
        assert_ne!(result.is_error, Some(true), "{result:?}");
        receiver.recv().unwrap()
    }
}

async fn assert_owned(waiter: &Receiver<anyhow::Result<()>>) {
    assert!(
        matches!(
            waiter.recv_timeout(Duration::from_millis(100)),
            Err(crossbeam_channel::RecvTimeoutError::Timeout)
        ),
        "command replied before drain"
    );
    assert_eq!(
        global()
            .acquire(LeaseRequest::desktop_raw(None, Duration::ZERO))
            .await
            .unwrap_err()
            .code(),
        "input_busy"
    );
}

async fn assert_released() {
    // The reply can wake its receiver just before the worker drops its scope.
    let lease = global()
        .acquire(LeaseRequest::desktop_raw(None, Duration::from_secs(2)))
        .await
        .unwrap();
    drop(lease);
    global()
        .acquire(LeaseRequest::window_semantic(
            ExactWindow {
                pid: 937453,
                window_id: 21,
            },
            Duration::ZERO,
        ))
        .await
        .unwrap();
}

fn read_text_batch(server: eis::Context) -> (usize, usize) {
    let mut keys = 0;
    let mut frames = 0;
    while keys < TEXT_LEN * 2 || frames < TEXT_LEN * 2 {
        assert!(readable(&server, Duration::from_secs(5)), "drain stalled");
        server.read().unwrap();
        while let Some(request) = server.pending_request() {
            match request {
                PendingRequestResult::Request(eis::Request::Keyboard(
                    _,
                    eis::keyboard::Request::Key { key, state },
                )) => {
                    assert_eq!(key, 30);
                    assert_eq!(
                        state,
                        if keys % 2 == 0 {
                            eis::keyboard::KeyState::Press
                        } else {
                            eis::keyboard::KeyState::Released
                        }
                    );
                    keys += 1;
                }
                PendingRequestResult::Request(eis::Request::Device(
                    _,
                    eis::device::Request::Frame { .. },
                )) => frames += 1,
                other => panic!("unexpected input: {other:?}"),
            }
        }
    }
    (keys, frames)
}

#[tokio::test]
async fn production_loop_backpressure_defers_reply_and_desktop_raw_admission() {
    let _serial = LEASE_TEST.lock().await;
    let fixture = Fixture::start();
    let (waiter, completion) = fixture.sender.queue_owned_text().await;
    assert!(
        readable(&fixture.peer, Duration::from_secs(5)),
        "no input queued"
    );
    assert_owned(&waiter).await;
    // The receiving compositor starts reading only after backpressure and the
    // exclusion assertions. Parse every key/frame, not merely a helper result.
    let server = fixture.server.clone();
    let reader = thread::spawn(move || read_text_batch(server));
    wait_for_reply(waiter, completion).unwrap();
    assert_eq!(reader.join().unwrap(), (TEXT_LEN * 2, TEXT_LEN * 2));
    assert_released().await;
    // Force another flush after release and give the production loop another
    // iteration. No retained input may appear after the proven complete batch.
    fixture.stale.flush().unwrap();
    assert!(!readable(&fixture.peer, Duration::from_millis(100)));
}

#[tokio::test]
async fn production_loop_deadline_discards_before_reply_focus_restore_and_release() {
    let _serial = LEASE_TEST.lock().await;
    let mut fixture = Fixture::start();
    let (waiter, completion) = fixture.sender.queue_owned_text().await;
    assert!(readable(&fixture.peer, Duration::from_secs(5)));
    assert_owned(&waiter).await;
    // Exercise the actual public reply waiter at its actual 20s deadline. It
    // represents the foreground wrapper: returning permits focus restoration.
    let error = wait_for_reply(waiter, completion).unwrap_err();
    assert_eq!(error.to_string(), "input_unavailable: libei output drain failed; EIS connection discarded: libei output drain deadline expired");
    assert_eq!(
        fixture.stale.flush().unwrap_err().raw_os_error(),
        libc::EPIPE
    );
    fixture.assert_disconnected();
    assert_released().await;
    let prefix = fixture.read_to_eof();
    assert!(
        prefix > 0 && prefix < TEXT_LEN * 2 * 24,
        "expected a partial batch, got {prefix}"
    );
    assert_eq!(
        fixture.stale.flush().unwrap_err().raw_os_error(),
        libc::EPIPE
    );
    assert_eq!(
        fixture.read_to_eof(),
        0,
        "input emitted after ownership release"
    );
}

#[tokio::test]
async fn production_loop_peer_failure_discards_before_error_and_release() {
    let _serial = LEASE_TEST.lock().await;
    let mut fixture = Fixture::start();
    let (waiter, completion) = fixture.sender.queue_owned_text().await;
    assert!(readable(&fixture.peer, Duration::from_secs(5)));
    assert_owned(&waiter).await;
    fixture.peer.shutdown(std::net::Shutdown::Both).unwrap();
    let error = wait_for_reply(waiter, completion).unwrap_err();
    assert_eq!(error.to_string(), "input_unavailable: libei output drain failed; EIS connection discarded: EIS socket closed while draining");
    assert_eq!(
        fixture.stale.flush().unwrap_err().raw_os_error(),
        libc::EPIPE
    );
    fixture.assert_disconnected();
    assert_released().await;
    fixture.read_to_eof();
    assert_eq!(
        fixture.stale.flush().unwrap_err().raw_os_error(),
        libc::EPIPE
    );
    assert_eq!(fixture.read_to_eof(), 0);
}

#[tokio::test]
async fn production_worker_refuses_input_during_lease_free_stalled_reconnect() {
    let _serial = LEASE_TEST.lock().await;
    let (mut fixture, context) = Fixture::disconnected();
    // A command arriving before readiness must not open anything or emit input.
    let (waiter, completion) = fixture.sender.queue_owned_text().await;
    let error = waiter
        .recv_timeout(Duration::from_millis(500))
        .expect("offline input was not refused promptly")
        .unwrap_err();
    assert_eq!(error.to_string(), no_live_context().to_string());
    // Retain the completion to prove refusal releases ownership, not just Drop.
    global()
        .acquire(LeaseRequest::desktop_raw(None, Duration::ZERO))
        .await
        .unwrap();
    drop(completion);
    assert!(!readable(&fixture.peer, Duration::from_millis(50)));

    let (entered, opening) = bounded(1);
    let (resume, stalled) = bounded(1);
    let sender = fixture.sender.clone();
    let readiness = thread::spawn(move || {
        sender.connect("ei_keyboard", || {
            entered.send(()).unwrap();
            stalled
                .recv_timeout(Duration::from_secs(5))
                .expect("test opener was not resumed");
            Ok((context, PortalKeepAlive::None))
        })
    });
    opening.recv_timeout(Duration::from_secs(2)).unwrap();
    // Consent can wait without a dispatch grant, and the command worker remains
    // responsive. This uses the production worker, not just run_calloop.
    global()
        .acquire(LeaseRequest::desktop_raw(None, Duration::ZERO))
        .await
        .unwrap();
    let (waiter, completion) = fixture.sender.queue_owned_text().await;
    let error = waiter
        .recv_timeout(Duration::from_millis(500))
        .expect("input waited behind connection opener")
        .unwrap_err();
    assert_eq!(error.to_string(), no_live_context().to_string());
    global()
        .acquire(LeaseRequest::desktop_raw(None, Duration::ZERO))
        .await
        .unwrap();
    drop(completion);
    assert!(!readable(&fixture.peer, Duration::from_millis(50)));
    resume.send(()).unwrap();
    fixture.negotiate();
    readiness.join().unwrap().unwrap();
    fixture.clear_setup();
    assert!(
        !readable(&fixture.peer, Duration::from_millis(100)),
        "refused command replayed after reconnect"
    );
    fixture
        .sender
        .connect("ei_keyboard", || {
            panic!("live readiness reopened a connection")
        })
        .unwrap();
}

#[tokio::test]
async fn cancelled_unstarted_command_releases_captured_lease_without_replay() {
    let _serial = LEASE_TEST.lock().await;
    let fixture = Fixture::start();
    let (tx, rx) = bounded(64);
    let queued_sender = CommandSender::new(tx);
    let scope = super::super::foreground_deadline::Scope::until(
        Instant::now() + Duration::from_millis(200),
    );
    let (waiter, completion) = queued_sender.queue_owned_text().await;
    assert_owned(&waiter).await;
    assert_eq!(
        wait_for_reply(waiter, completion).unwrap_err().to_string(),
        command_timeout().to_string()
    );
    // Still queued: cancellation, not worker progress or queued-payload Drop,
    // must release the captured dispatch grant.
    global()
        .acquire(LeaseRequest::desktop_raw(None, Duration::ZERO))
        .await
        .unwrap();
    drop(scope);
    fixture.sender.tx.send(rx.recv().unwrap()).unwrap();
    fixture.sender.wait_ready("ei_keyboard").unwrap();
    assert!(
        !readable(&fixture.peer, Duration::from_millis(100)),
        "cancelled input was replayed"
    );
}

#[tokio::test]
async fn foreground_commands_share_one_drain_deadline_under_backpressure() {
    let _serial = LEASE_TEST.lock().await;
    let mut fixture = Fixture::start();
    // Model a transaction whose Begin/validation already used 22s of its 25s
    // input budget. Both sends use the real thread-local scope and sender.
    let start = Instant::now() - Duration::from_secs(22);
    let scope = super::super::foreground_deadline::Scope::begin(start);
    let deadline = start + Duration::from_secs(25);
    let (waiter, first) = fixture.sender.queue_owned_text().await;
    assert_eq!(first.deadline, deadline);
    assert!(readable(&fixture.peer, Duration::from_secs(1)));
    assert_owned(&waiter).await;
    // Keep the peer stopped while the first command consumes most of the
    // remaining budget, then allow that command to complete successfully.
    thread::sleep(Duration::from_secs(2));
    let server = fixture.server.clone();
    let reader = thread::spawn(move || read_text_batch(server));
    wait_for_reply(waiter, first).unwrap();
    assert_eq!(reader.join().unwrap(), (TEXT_LEN * 2, TEXT_LEN * 2));
    assert_released().await;
    let (waiter, second) = fixture.sender.queue_owned_text().await;
    assert_eq!(
        second.deadline, deadline,
        "later command renewed the transaction budget"
    );
    assert!(readable(&fixture.peer, Duration::from_millis(500)));
    assert_owned(&waiter).await;
    let error = wait_for_reply(waiter, second).unwrap_err();
    assert_eq!(error.to_string(), "input_unavailable: libei output drain failed; EIS connection discarded: libei output drain deadline expired");
    assert!(
        Instant::now() < deadline + Duration::from_secs(1),
        "drain used a fresh 20s budget"
    );
    assert_eq!(
        fixture.stale.flush().unwrap_err().raw_os_error(),
        libc::EPIPE
    );
    // The transaction is over. Probe the worker with an ordinary readiness
    // budget: under the expired scope the probe's own deadline has already
    // elapsed, so its waiter can cancel it before the worker refuses it.
    drop(scope);
    fixture.assert_disconnected();
    assert_released().await;
    let prefix = fixture.read_to_eof();
    assert!(prefix > 0 && prefix < TEXT_LEN * 2 * 24);
    assert_eq!(
        fixture.stale.flush().unwrap_err().raw_os_error(),
        libc::EPIPE
    );
    assert_eq!(
        fixture.read_to_eof(),
        0,
        "input transmitted after transaction deadline"
    );
}

#[test]
fn queued_timeout_cannot_inject_after_waiter_restores_focus() {
    let fixture = Fixture::start();
    let (reply, waiter) = bounded(1);
    let completion = Arc::new(CommandCompletion {
        deadline: Instant::now(),
        execution: Mutex::new(CommandExecution::default()),
    });
    fixture
        .sender
        .tx
        .send(WorkerRequest::Command(QueuedCommand {
            command: Cmd::PressKey { keycode: 30, reply },
            completion: completion.clone(),
        }))
        .unwrap();
    assert_eq!(
        wait_for_reply(waiter, completion).unwrap_err().to_string(),
        command_timeout().to_string()
    );
    assert!(
        !readable(&fixture.peer, Duration::from_millis(100)),
        "expired command emitted input"
    );
}
