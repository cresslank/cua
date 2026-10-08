//! Real tool entrypoints and snapshot admissions, with only native I/O replaced.
use super::super::{ClickTool, DoubleClickTool, RightClickTool};
use super::*;
use crate::atspi::native::retained_pointer_tests::{menu_probe, MenuFault, MENU_FAILURES};
use cua_driver_core::tool::Tool;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

#[derive(Clone, Copy, Default)]
enum Fault {
    #[default]
    None,
    Geometry,
    GeometryLater,
    GeometryChanged,
    Outside,
    Moved,
    Removed,
    Cleanup,
    Restore,
    MenuUnknown,
    MenuNative(MenuFault),
    DelayedValidation,
}
#[derive(Default)]
pub(crate) struct Backend {
    wayland: bool,
    hyprland: bool,
    menu: bool,
    mpx: bool,
    fault: Fault,
    delivered: AtomicUsize,
    validations: AtomicUsize,
    activations: AtomicUsize,
    geometry_reads: AtomicUsize,
    menu_calls: AtomicUsize,
    sessions: AtomicUsize,
}
impl Target for Arc<Backend> {
    fn context_menu(&self) -> Result<bool> {
        self.menu_calls.fetch_add(1, SeqCst);
        anyhow::ensure!(
            !matches!(self.fault, Fault::MenuUnknown),
            "menu acknowledgement lost"
        );
        if let Fault::MenuNative(fault) = self.fault {
            return menu_probe(fault);
        }
        Ok(self.menu)
    }
    fn hyprland(&self) -> bool {
        self.hyprland
    }
    fn transport(&self) -> ActionTransport {
        if self.wayland {
            ActionTransport::LinuxWaylandVirtualPointer
        } else {
            ActionTransport::LinuxXTest
        }
    }
    fn foreground(
        &self,
        _permit: Arc<MutationPermit>,
        body: &mut dyn FnMut(&dyn Pointer) -> Result<()>,
    ) -> Result<()> {
        self.activations.fetch_add(1, SeqCst);
        let _available_but_irrelevant = self.mpx;
        body(self.as_ref())?;
        anyhow::ensure!(
            !matches!(self.fault, Fault::Restore),
            "focus restoration failed"
        );
        Ok(())
    }
}
impl Pointer for Backend {
    fn geometry(&self) -> Result<Geometry> {
        assert_eq!(
            self.activations.load(SeqCst),
            1,
            "must activate before geometry"
        );
        self.geometry_reads.fetch_add(1, SeqCst);
        anyhow::ensure!(
            !matches!(self.fault, Fault::Geometry),
            "geometry unavailable"
        );
        if self.delivered.load(SeqCst) > 0 {
            anyhow::ensure!(
                !matches!(self.fault, Fault::GeometryLater),
                "live geometry unavailable"
            );
            if matches!(self.fault, Fault::GeometryChanged) {
                return Ok(Geometry {
                    window: (11, 20, 600, 400),
                    content: (0, 0),
                    output: (800, 600),
                });
            }
        }
        Ok(Geometry {
            window: (10, 20, 600, 400),
            content: (0, 0),
            output: (800, 600),
        })
    }
    fn bounds(&self, g: Geometry) -> Result<Bounds> {
        assert_eq!(g.window, (10, 20, 600, 400));
        if matches!(self.fault, Fault::Outside) {
            return Ok((1000, 1000, 50, 50));
        }
        if matches!(self.fault, Fault::Moved) && self.delivered.load(SeqCst) > 0 {
            return Ok((300, 300, 20, 20));
        }
        Ok((100, 100, 50, 50))
    }
    fn verify(&self) -> Result<()> {
        self.validations.fetch_add(1, SeqCst);
        if matches!(self.fault, Fault::DelayedValidation) && self.delivered.load(SeqCst) > 0 {
            std::thread::sleep(Duration::from_millis(320));
        }
        anyhow::ensure!(
            !(matches!(self.fault, Fault::Removed) && self.delivered.load(SeqCst) > 0),
            "identity/ancestry vanished"
        );
        Ok(())
    }
    fn click(
        &self,
        point: Point,
        _: Geometry,
        button: u8,
        count: usize,
        before: &mut dyn FnMut() -> Result<()>,
        ack: &mut dyn FnMut(),
    ) -> Result<()> {
        self.sessions.fetch_add(1, SeqCst);
        for _ in 0..count {
            before()?;
            assert_eq!(point, (125, 125));
            assert!(button == 1 || button == 3);
            self.delivered.fetch_add(1, SeqCst);
            ack();
            anyhow::ensure!(
                !matches!(self.fault, Fault::Cleanup),
                "pointer cleanup failed"
            );
        }
        Ok(())
    }
}

fn invoke(backend: Arc<Backend>, tool: &str, extra: serde_json::Value) -> ToolResult {
    cua_driver_core::tool::with_runtime_scope(format!("secondary-{}", uuid::Uuid::new_v4()), || {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            let mut state=ToolState::new();
            Arc::get_mut(&mut state).unwrap().observed_click_backend=Some(backend);
            let pid=std::process::id();
            let node=crate::atspi::AtspiNode {
                element_index:Some(7),element_key:41,
                identity:Some(crate::atspi::AtspiIdentity { bus_name:":1.41".into(), path:"/retained".into(), frame_bus_name:":1.41".into(), frame_path:"/frame".into() }),
                role:"push button".into(),name:None,value:None,checked:None,enabled:Some(true),selected:None,description:None,
                actions:vec![],depth:0,parent_element_index:None,in_web_content:false,object_ref:None,
            };
            let id=state.snapshots.publish(state.snapshots.prepare(pid,0x7f30_0110,&[node]).unwrap()).unwrap();
            let mut args=json!({"pid":pid,"element_token":cua_driver_core::element_token::token_for(id,7),"delivery_mode":"foreground"});
            args.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            match tool { "click" => ClickTool {state}.invoke(args).await,
                "right_click" => RightClickTool {state}.invoke(args).await,
                "double_click" => DoubleClickTool {state}.invoke(args).await,
                _=>panic!("unknown test tool") }
        })
    })
}
fn code(result: &ToolResult) -> &str {
    result.structured_content.as_ref().unwrap()["code"]
        .as_str()
        .unwrap()
}
fn count(result: &ToolResult) -> u64 {
    result.structured_content.as_ref().unwrap()["delivery"]["delivered_count"]
        .as_u64()
        .unwrap()
}

#[test]
fn secondary_background_nonplain_never_activates_or_dispatches_even_with_mpx() {
    for wayland in [false, true] {
        for mpx in [false, true] {
            for (tool, args) in [
                ("click", json!({"button":"right"})),
                ("click", json!({"count":2})),
                ("click", json!({"modifier":["shift"]})),
                ("click", json!({"button":"middle"})),
                ("right_click", json!({})),
                ("double_click", json!({})),
            ] {
                let backend = Arc::new(Backend {
                    wayland,
                    mpx,
                    menu: true,
                    ..Default::default()
                });
                let mut args = args;
                args["delivery_mode"] = json!("background");
                let result = invoke(backend.clone(), tool, args);
                assert_eq!(code(&result), "background_unavailable");
                assert_eq!(backend.delivered.load(SeqCst), 0);
                assert_eq!(backend.activations.load(SeqCst), 0);
                assert_eq!(backend.menu_calls.load(SeqCst), 0);
            }
        }
    }
}
#[test]
fn secondary_foreground_unsupported_forms_refuse_before_platform_dispatch() {
    for wayland in [false, true] {
        for args in [
            json!({"button":"middle"}),
            json!({"count":3}),
            json!({"modifier":["shift"]}),
            json!({"button":"right","count":2}),
        ] {
            let backend = Arc::new(Backend {
                wayland,
                ..Default::default()
            });
            let result = invoke(backend.clone(), "click", args);
            assert_eq!(code(&result), "element_route_unqualified");
            assert_eq!(backend.activations.load(SeqCst), 0);
            assert_eq!(backend.delivered.load(SeqCst), 0);
        }
    }
}
#[test]
fn secondary_x11_and_wayland_generic_clicks_share_retained_route() {
    for wayland in [false, true] {
        for (tool, args, wanted) in [
            ("click", json!({"button":"right"}), 1),
            ("click", json!({"count":2}), 2),
            ("right_click", json!({}), 1),
            ("double_click", json!({}), 2),
        ] {
            let backend = Arc::new(Backend {
                wayland,
                ..Default::default()
            });
            let result = invoke(backend.clone(), tool, args);
            assert_ne!(result.is_error, Some(true), "{result:?}");
            assert_eq!(count(&result), wanted);
            assert_eq!(backend.delivered.load(SeqCst), wanted as usize);
            assert_eq!(backend.validations.load(SeqCst), wanted as usize);
            assert_eq!(backend.geometry_reads.load(SeqCst), 1 + wanted as usize);
        }
    }
}
#[test]
fn secondary_menu_action_uses_retained_semantic_route_without_pointer() {
    for hyprland in [false, true] {
        let backend = Arc::new(Backend {
            menu: true,
            hyprland,
            ..Default::default()
        });
        let result = invoke(backend.clone(), "right_click", json!({}));
        assert_eq!(count(&result), 1);
        assert_eq!(
            result.action_record.unwrap().transport,
            ActionTransport::LinuxAtSpiAction
        );
        assert_eq!(backend.menu_calls.load(SeqCst), 1);
        assert_eq!(backend.activations.load(SeqCst), 0);
        assert_eq!(backend.delivered.load(SeqCst), 0);
    }
}
#[test]
fn secondary_menu_unknown_never_replays_as_pointer() {
    let backend = Arc::new(Backend {
        fault: Fault::MenuUnknown,
        ..Default::default()
    });
    let result = invoke(backend.clone(), "right_click", json!({}));
    assert_eq!(code(&result), "stale_element_token");
    assert_eq!(backend.activations.load(SeqCst), 0);
}
#[test]
fn secondary_hyprland_pointer_refuses_before_foreground_plugin() {
    for tool in ["double_click", "right_click"] {
        let backend = Arc::new(Backend {
            hyprland: true,
            ..Default::default()
        });
        let result = invoke(backend.clone(), tool, json!({}));
        assert_eq!(code(&result), "element_route_unqualified");
        assert_eq!(backend.activations.load(SeqCst), 0);
        assert_eq!(backend.delivered.load(SeqCst), 0);
    }
}
#[test]
fn secondary_geometry_failure_and_outside_output_do_not_clamp_or_dispatch() {
    for fault in [Fault::Geometry, Fault::Outside] {
        for wayland in [false, true] {
            let backend = Arc::new(Backend {
                fault,
                wayland,
                ..Default::default()
            });
            let result = invoke(backend.clone(), "double_click", json!({}));
            assert_eq!(code(&result), "stale_element_token");
            assert_eq!(count(&result), 0);
            assert_eq!(backend.delivered.load(SeqCst), 0);
        }
    }
}
#[test]
fn secondary_per_click_bounds_and_identity_revalidation_preserves_one_ack() {
    for fault in [
        Fault::Moved,
        Fault::Removed,
        Fault::GeometryLater,
        Fault::GeometryChanged,
    ] {
        for wayland in [false, true] {
            let backend = Arc::new(Backend {
                fault,
                wayland,
                ..Default::default()
            });
            let result = invoke(backend.clone(), "double_click", json!({}));
            assert_eq!(code(&result), "stale_element_token");
            assert_eq!(count(&result), 1);
            assert_eq!(backend.delivered.load(SeqCst), 1);
            assert_eq!(result.action_record.unwrap().effect, ActionEffect::Partial);
        }
    }
}
#[test]
fn secondary_acknowledgement_precedes_pointer_and_focus_cleanup() {
    for (fault, wanted) in [(Fault::Cleanup, 1), (Fault::Restore, 2)] {
        for wayland in [false, true] {
            let backend = Arc::new(Backend {
                fault,
                wayland,
                ..Default::default()
            });
            let result = invoke(backend.clone(), "double_click", json!({}));
            assert_eq!(code(&result), "stale_element_token");
            assert_eq!(count(&result), wanted);
            assert_eq!(backend.delivered.load(SeqCst), wanted as usize);
        }
    }
}

#[test]
fn secondary_native_menu_failures_never_reach_pointer() {
    for fault in MENU_FAILURES {
        let backend = Arc::new(Backend {
            fault: Fault::MenuNative(fault),
            ..Default::default()
        });
        let result = invoke(backend.clone(), "right_click", json!({}));
        assert_eq!(code(&result), "stale_element_token", "{fault:?}");
        assert_eq!(backend.activations.load(SeqCst), 0, "{fault:?}");
        assert_eq!(backend.delivered.load(SeqCst), 0, "{fault:?}");
    }
}
#[test]
fn secondary_double_click_reuses_transport_and_stops_after_delayed_revalidation() {
    for wayland in [false, true] {
        for fault in [Fault::None, Fault::DelayedValidation] {
            let backend = Arc::new(Backend {
                wayland,
                fault,
                ..Default::default()
            });
            let result = invoke(backend.clone(), "double_click", json!({}));
            assert_eq!(backend.sessions.load(SeqCst), 1);
            assert_eq!(backend.validations.load(SeqCst), 2);
            if matches!(fault, Fault::DelayedValidation) {
                assert_eq!(code(&result), "stale_element_token");
                assert_eq!(count(&result), 1);
                assert_eq!(backend.delivered.load(SeqCst), 1);
                assert_eq!(result.action_record.unwrap().effect, ActionEffect::Partial);
            } else {
                assert_ne!(result.is_error, Some(true));
                assert_eq!(count(&result), 2);
            }
        }
    }
    // The native adapters open once outside their per-click loop too.
    let wayland = include_str!("../wayland/mod.rs");
    let primitive = section(
        wayland,
        concat!("pub(crate) fn ", "click_retained_point("),
        "/// Synthesize a vertical",
    );
    assert_eq!(primitive.matches("open_vptr_session(").count(), 1);
    assert!(primitive.find("open_vptr_session(").unwrap() < primitive.find("for step in").unwrap());
    let x11 = include_str!("../input/mod.rs");
    let primitive = section(
        x11,
        concat!("pub(crate) fn ", "send_click_xtest_acknowledged("),
        "/// Move the real X11 pointer",
    );
    assert_eq!(primitive.matches("connect_x11_for_input(").count(), 1);
    assert!(
        primitive.find("connect_x11_for_input(").unwrap()
            < primitive.find("for click_index in").unwrap()
    );
}
#[test]
fn secondary_wayland_modified_click_codes_and_upstream_phrase() {
    for (delivery, expected) in [
        (DeliveryMode::Background, "background_unavailable"),
        (DeliveryMode::Foreground, "element_route_unqualified"),
    ] {
        let result = qualify_on_platform(delivery, 1, 1, &["ctrl".into()], true).unwrap_err();
        assert_eq!(code(&result), expected);
        assert_eq!(result.is_error, Some(true));
        let json = serde_json::to_value(&result).unwrap();
        let text = json["content"][0]["text"].as_str().unwrap();
        assert!(text.starts_with("modified element clicks are unavailable on native Wayland: the pointer route cannot carry keyboard modifier state"));
    }
}

#[test]
fn secondary_point_refuses_output_overflow_and_preserves_real_origin() {
    let geometry = Geometry {
        window: (0, 0, 2000, 2000),
        content: (0, 0),
        output: (800, 600),
    };
    assert!(checked_point((900, 100, 20, 20), geometry).is_err());
    assert!(checked_point((100, 900, 20, 20), geometry).is_err());
    assert!(checked_point((i32::MAX, 100, u32::MAX, 20), geometry).is_err());
    assert_eq!(checked_point((0, 0, 1, 1), geometry).unwrap(), (0, 0));
}

fn section<'a>(source: &'a str, start: &str, end: &str) -> &'a str {
    source
        .rsplit_once(start)
        .unwrap()
        .1
        .split_once(end)
        .unwrap()
        .0
}
#[test]
fn secondary_native_dispatch_boundaries_and_no_ordinal_escape() {
    let wayland = include_str!("../wayland/mod.rs");
    let primitive = section(
        wayland,
        concat!("pub(crate) fn ", "click_retained_point("),
        concat!("/// Synthesize a vertical", " or horizontal scroll"),
    );
    assert!(primitive.find("before()?").unwrap() < primitive.find("ButtonState::Pressed").unwrap());
    assert!(
        primitive.find("acknowledged();").unwrap() < primitive.find("sess.vptr.destroy()").unwrap()
    );
    for forbidden in [
        "clamp(",
        "click_with_outcome(",
        "execute_foreground",
        "activate_window_for_input_target",
    ] {
        assert!(!primitive.contains(forbidden));
    }
    let x11 = include_str!("../input/mod.rs");
    let primitive = section(
        x11,
        concat!("pub(crate) fn ", "send_click_xtest_acknowledged("),
        concat!("/// Move the real X11 pointer", " to a screen-absolute"),
    );
    assert!(
        primitive
            .find("complete_motion_before_validation(")
            .unwrap()
            < primitive.find("BUTTON_PRESS_EVENT").unwrap()
    );
    let motion = primitive
        .split_once("MOTION_NOTIFY_EVENT")
        .unwrap()
        .1
        .split_once("let press")
        .unwrap()
        .0;
    assert!(
        motion.contains(".check()?"),
        "must sync the motion cookie on the injection connection"
    );
    assert!(
        primitive.find("acknowledged();").unwrap()
            < primitive.find("let completion_result").unwrap()
    );
    let tools = include_str!("impl_.rs");
    let click = section(
        tools,
        concat!("impl Tool for ", "ClickTool {"),
        concat!("// ──", " type_text"),
    );
    assert!(
        click.find("retained_click::qualify").unwrap() < click.find(".resolve_for_tool").unwrap()
    );
    assert!(
        click.find("retained_click::invoke").unwrap() < click.find(".click_indexed_x11").unwrap()
    );
    let compact = click.split_whitespace().collect::<String>();
    assert!(compact.contains(".click_indexed_x11(pid,idx,window_id_resolved,snapshot_identity,1,1,Vec::new(),delivery,cursor_id,"));
    for tool in ["DoubleClickTool", "RightClickTool"] {
        let start = format!("impl Tool for {tool} {{");
        let route = tools
            .rsplit_once(&start)
            .unwrap()
            .1
            .split("// ──")
            .next()
            .unwrap();
        assert!(!route.contains("resolve_element_local_coords"));
        assert!(!route.contains("element_screen_center"));
    }
    let native = include_str!("../atspi/native.rs");
    let menu = section(
        native,
        concat!("pub(crate) fn ", "context_menu(&self)"),
        concat!("pub(crate) fn ", "pointer_bounds("),
    );
    assert!(menu.contains("strict_context_menu("));
    assert!(!menu.contains("target.actions"));
    assert!(!menu.contains("live_action_names("));
    assert!(menu.contains("|| self.verify_live_at_mutation()"));
    assert!(!menu.contains(".nth("));
    let route = include_str!("retained_click.rs");
    for required in [
        "with_retained_mutation(permit.clone()",
        "blocking::spawn(move",
        "Some(permit)",
    ] {
        assert!(route.contains(required));
    }
}
