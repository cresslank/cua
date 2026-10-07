//! Cold workers admitted by the production registry and platform readiness
//! routing seam. Only compositor facts / the EIS opener are fixture dependencies.
use super::*;
use crate::wayland::{ensure_input_ready_with, InputReadinessSession, NativeInputProtocols};

struct Readiness {
    session: InputReadinessSession,
    protocols: NativeInputProtocols,
    sender: CommandSender,
    context: Option<reis::ei::Context>,
    interfaces: Vec<&'static str>,
    calls: usize,
    opens: usize,
}
static READINESS: Mutex<Option<Readiness>> = Mutex::new(None);

fn ready_hook(tool: &str, args: &serde_json::Value) -> Result<(), String> {
    let mut slot = READINESS.lock().unwrap();
    let ready = slot.as_mut().unwrap();
    ready.calls += 1;
    let protocols = ready.protocols;
    ensure_input_ready_with(
        tool,
        args,
        ready.session,
        || Ok(protocols),
        |interface| {
            // Readiness must precede BOTH the browser dispatch grant and the raw
            // foreground transaction. Assert on every required interface, not only
            // the first open; a second wait must not move into the action either.
            drop(
                global()
                    .acquire_blocking(LeaseRequest::desktop_raw(None, Duration::ZERO))
                    .expect("readiness held the desktop lease"),
            );
            drop(
                global()
                    .acquire_blocking(LeaseRequest::browser_profile("pid:937453", Duration::ZERO))
                    .expect("readiness held the browser profile lease"),
            );
            ready.interfaces.push(interface);
            ready.sender.connect(interface, || {
                ready.opens += 1;
                Ok((
                    ready.context.take().expect("opened more than once"),
                    PortalKeepAlive::None,
                ))
            })
        },
    )
    .map_err(|error| error.to_string())
}

// Isolate the process-wide hook/table from unrelated parallel unit tests. This
// environment marker exists only in cfg(test); production has no test switch.
fn isolated(name: &str, test: impl FnOnce()) {
    if std::env::var("CUA_READINESS_TEST").as_deref() == Ok(name) {
        test();
        return;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            &format!("{}::{name}", module_path!().split_once("::").unwrap().1),
            "--nocapture",
        ])
        .env("CUA_READINESS_TEST", name)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("1 passed"),
        "child did not run the test"
    );
}

struct FirstInput {
    def: ToolDef,
    sender: CommandSender,
    inject: bool,
}
#[async_trait::async_trait]
impl Tool for FirstInput {
    fn def(&self) -> &ToolDef {
        &self.def
    }
    async fn invoke(&self, _: serde_json::Value) -> ToolResult {
        if !self.inject {
            return ToolResult::text("native route unchanged");
        }
        let browser = self.def.name == "browser_prepare";
        let request = if browser {
            LeaseRequest::browser_profile("pid:937453", Duration::ZERO)
        } else {
            LeaseRequest::desktop_raw(None, Duration::ZERO)
        };
        assert!(
            global().acquire_blocking(request).is_err(),
            "dispatch grant missing"
        );
        // Browser setup starts its native foreground transaction after admission.
        let _foreground = browser.then(|| {
            global()
                .acquire_blocking(LeaseRequest::desktop_raw(None, Duration::ZERO))
                .unwrap()
        });
        let (reply, waiter) = bounded(1);
        let command = match self.def.name.as_str() {
            "click" => Cmd::Click {
                x: 10.0,
                y: 20.0,
                button: Button::Left,
                reply,
            },
            "scroll" => Cmd::Scroll {
                dx: 0.0,
                dy: 12.0,
                reply,
            },
            // The first trusted setup navigation input is Ctrl+T.
            _ => Cmd::KeySequence {
                transitions: vec![
                    KeyTransition::Press(29),
                    KeyTransition::Press(20),
                    KeyTransition::Release(20),
                    KeyTransition::Release(29),
                ],
                reply,
            },
        };
        let completion = self.sender.send(command).unwrap();
        match wait_for_reply(waiter, completion) {
            Ok(()) => ToolResult::text("first input drained"),
            Err(error) => ToolResult::error(error.to_string()),
        }
    }
}

fn registry(sender: CommandSender, tool: &str, inject: bool) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(FirstInput {
        def: ToolDef {
            name: tool.into(),
            description: "readiness fixture".into(),
            input_schema: serde_json::json!({"type": "object"}),
            read_only: false,
            destructive: false,
            idempotent: false,
            open_world: false,
        },
        sender,
        inject,
    }));
    registry
}

fn cold_action(tool: &str, session: InputReadinessSession, expected: &[&str]) {
    cold_action_with_protocols(
        tool,
        session,
        NativeInputProtocols {
            pointer: false,
            keyboard: false,
            wtype: false,
        },
        expected,
    );
}

fn cold_action_with_protocols(
    tool: &str,
    session: InputReadinessSession,
    protocols: NativeInputProtocols,
    expected: &[&str],
) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let (mut fixture, context) = Fixture::disconnected();
    fixture.assert_disconnected();
    let sender = fixture.sender.clone();
    *READINESS.lock().unwrap() = Some(Readiness {
        session,
        protocols,
        sender: sender.clone(),
        context: Some(context),
        interfaces: vec![],
        calls: 0,
        opens: 0,
    });
    cua_driver_core::action_lease::install_raw_input_ready_hook(ready_hook);
    let peer = thread::spawn(move || {
        fixture.negotiate_devices(true);
        fixture
    });
    let result = runtime.block_on(registry(sender, tool, true).invoke(
        tool,
        serde_json::json!({
            "pid": 937453, "window_id": 21, "strategy": {"kind": "existing_profile"}
        }),
    ));
    assert_ne!(
        result.is_error,
        Some(true),
        "cold action failed: {result:?}"
    );
    let mut fixture = peer.join().unwrap();
    let ready = READINESS.lock().unwrap().take().unwrap();
    assert_eq!(ready.calls, 1);
    assert_eq!(ready.opens, 1);
    assert_eq!(ready.interfaces, expected);
    // Decode input from the actual peer, proving the first action ran rather
    // than just observing a mocked successful readiness result.
    assert!(readable(&fixture.peer, Duration::from_secs(2)));
    fixture.server.read().unwrap();
    let mut keys = Vec::new();
    let mut buttons = 0;
    let mut scrolls = 0;
    while let Some(request) = fixture.server.pending_request() {
        match request {
            PendingRequestResult::Request(eis::Request::Keyboard(
                _,
                eis::keyboard::Request::Key { key, state },
            )) => keys.push((key, state)),
            PendingRequestResult::Request(eis::Request::Button(
                _,
                eis::button::Request::Button { .. },
            )) => buttons += 1,
            PendingRequestResult::Request(eis::Request::Scroll(
                _,
                eis::scroll::Request::Scroll { .. },
            )) => scrolls += 1,
            _ => (),
        }
    }
    match tool {
        "click" => assert_eq!((keys.len(), buttons, scrolls), (0, 2, 0)),
        "scroll" => assert_eq!((keys.len(), buttons, scrolls), (0, 0, 1)),
        _ => {
            use eis::keyboard::KeyState::{Press, Released};
            assert_eq!(
                keys,
                [(29, Press), (20, Press), (20, Released), (29, Released)]
            );
            assert_eq!((buttons, scrolls), (0, 0));
        }
    }
    runtime.block_on(assert_released());
    fixture.clear_setup();
    assert!(
        !readable(&fixture.peer, Duration::from_millis(50)),
        "unexpected replay"
    );
}

#[test]
fn cold_gnome_existing_profile_admission_prepares_keyboard_and_pointer() {
    isolated(
        "cold_gnome_existing_profile_admission_prepares_keyboard_and_pointer",
        || {
            cold_action(
                "browser_prepare",
                InputReadinessSession::Gnome,
                &["ei_keyboard", "ei_pointer_absolute"],
            )
        },
    );
}

#[test]
fn cold_non_gnome_click_admission_prepares_portal_fallback() {
    isolated(
        "cold_non_gnome_click_admission_prepares_portal_fallback",
        || {
            cold_action(
                "click",
                InputReadinessSession::NativeWayland,
                &["ei_pointer_absolute"],
            )
        },
    );
}

#[test]
fn cold_non_gnome_scroll_admission_prepares_pointer_and_scroll() {
    isolated(
        "cold_non_gnome_scroll_admission_prepares_pointer_and_scroll",
        || {
            cold_action(
                "scroll",
                InputReadinessSession::NativeWayland,
                &["ei_pointer_absolute", "ei_scroll"],
            )
        },
    );
}

#[test]
fn cold_non_gnome_keyboard_admission_prepares_portal_fallback() {
    isolated(
        "cold_non_gnome_keyboard_admission_prepares_portal_fallback",
        || {
            cold_action(
                "hotkey",
                InputReadinessSession::NativeWayland,
                &["ei_keyboard"],
            )
        },
    );
}

#[test]
fn cold_native_keyboard_without_wtype_prepares_text_and_key_fallback() {
    isolated(
        "cold_native_keyboard_without_wtype_prepares_text_and_key_fallback",
        || {
            for tool in ["type_text", "press_key"] {
                cold_action_with_protocols(
                    tool,
                    InputReadinessSession::NativeWayland,
                    NativeInputProtocols {
                        pointer: true,
                        keyboard: true,
                        wtype: false,
                    },
                    &["ei_keyboard"],
                );
            }
        },
    );
}

#[test]
fn unavailable_fallback_refuses_before_dispatch() {
    isolated("unavailable_fallback_refuses_before_dispatch", || {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let (fixture, _) = Fixture::disconnected();
        cua_driver_core::action_lease::install_raw_input_ready_hook(|tool, args| {
            ensure_input_ready_with(
                tool,
                args,
                InputReadinessSession::NativeWayland,
                || {
                    Ok(NativeInputProtocols {
                        pointer: false,
                        keyboard: false,
                        wtype: false,
                    })
                },
                |_| anyhow::bail!("fixture portal unavailable"),
            )
            .map_err(|error| error.to_string())
        });
        let result = runtime.block_on(
            registry(fixture.sender.clone(), "click", false).invoke("click", serde_json::json!({})),
        );
        assert_eq!(result.is_error, Some(true));
        assert_eq!(
            result.structured_content.unwrap()["refusal"]["code"],
            "input_unavailable"
        );
        runtime.block_on(assert_released());
        fixture.assert_disconnected();
        assert!(!readable(&fixture.peer, Duration::from_millis(50)));
    });
}

#[test]
fn native_and_inject_admission_never_open_libei() {
    isolated("native_and_inject_admission_never_open_libei", || {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let (fixture, _context) = Fixture::disconnected();
        *READINESS.lock().unwrap() = Some(Readiness {
            session: InputReadinessSession::NativeWayland,
            protocols: NativeInputProtocols {
                pointer: true,
                keyboard: true,
                wtype: true,
            },
            sender: fixture.sender.clone(),
            context: None,
            interfaces: vec![],
            calls: 0,
            opens: 0,
        });
        cua_driver_core::action_lease::install_raw_input_ready_hook(ready_hook);
        for session in [
            InputReadinessSession::NativeWayland,
            InputReadinessSession::Bypass,
        ] {
            READINESS.lock().unwrap().as_mut().unwrap().session = session;
            for tool in ["click", "scroll", "hotkey", "browser_prepare"] {
                let result = runtime
                    .block_on(registry(fixture.sender.clone(), tool, false).invoke(
                    tool,
                    serde_json::json!({"pid": 937453, "strategy": {"kind": "existing_profile"}}),
                ));
                assert_ne!(result.is_error, Some(true), "{result:?}");
            }
        }
        let ready = READINESS.lock().unwrap().take().unwrap();
        assert_eq!(ready.calls, 8);
        assert_eq!(ready.opens, 0);
        assert!(ready.interfaces.is_empty());
        fixture.assert_disconnected();
        assert!(!readable(&fixture.peer, Duration::from_millis(50)));
    });
}

#[test]
fn isolated_browser_and_launch_admission_skip_readiness() {
    isolated(
        "isolated_browser_and_launch_admission_skip_readiness",
        || {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            let (fixture, _) = Fixture::disconnected();
            cua_driver_core::action_lease::install_raw_input_ready_hook(|_, _| {
                panic!("unexpected transport readiness")
            });
            for tool in ["browser_prepare", "launch_app"] {
                let result = runtime.block_on(registry(fixture.sender.clone(), tool, false).invoke(tool,
                serde_json::json!({"profile_name": "fixture", "profile": {"mode": "isolated_new"}})));
                assert_ne!(result.is_error, Some(true), "{result:?}");
            }
            fixture.assert_disconnected();
        },
    );
}

#[test]
fn native_chord_routes_without_wtype_never_open_libei() {
    isolated("native_chord_routes_without_wtype_never_open_libei", || {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let (fixture, _context) = Fixture::disconnected();
        *READINESS.lock().unwrap() = Some(Readiness {
            session: InputReadinessSession::NativeWayland,
            protocols: NativeInputProtocols {
                pointer: true,
                keyboard: true,
                wtype: false,
            },
            sender: fixture.sender.clone(),
            context: None,
            interfaces: vec![],
            calls: 0,
            opens: 0,
        });
        cua_driver_core::action_lease::install_raw_input_ready_hook(ready_hook);
        // Both native protocols are present, so these chord routes dispatch
        // through the in-process virtual keyboard; the missing wtype helper
        // must not open a portal. Unmodified press_key and type_text still do
        // (cold_native_keyboard_without_wtype_prepares_text_and_key_fallback).
        for (tool, args) in [
            ("hotkey", serde_json::json!({"keys": ["ctrl", "s"]})),
            (
                "press_key",
                serde_json::json!({"key": "s", "modifiers": ["ctrl"]}),
            ),
            (
                "browser_prepare",
                serde_json::json!({"pid": 937453, "strategy": {"kind": "existing_profile"}}),
            ),
        ] {
            let result =
                runtime.block_on(registry(fixture.sender.clone(), tool, false).invoke(tool, args));
            assert_ne!(result.is_error, Some(true), "{tool}: {result:?}");
        }
        let ready = READINESS.lock().unwrap().take().unwrap();
        assert_eq!(ready.calls, 3);
        assert_eq!(ready.opens, 0);
        assert!(ready.interfaces.is_empty(), "{:?}", ready.interfaces);
        fixture.assert_disconnected();
        assert!(!readable(&fixture.peer, Duration::from_millis(50)));
    });
}
