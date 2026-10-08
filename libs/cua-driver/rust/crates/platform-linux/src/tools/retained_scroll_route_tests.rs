//! Headless I/O seam tests through ScrollTool::invoke, not a duplicate router.
//! Native dispatch absence is checked structurally too: no unused mock pointer
//! counter can silently pass when a direct input call bypasses the AX seam.
use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Copy)]
enum Outcome {
    Miss,
    Complete,
    StaleAfterOne,
    TimeoutAfterOne,
    UncertainWithoutAck,
}

pub(super) struct Backend {
    hyprland: bool,
    outcome: Outcome,
    ax: AtomicUsize,
}

impl Backend {
    pub(super) fn isolated_background(
        &self,
        delivery: crate::input::delivery::DeliveryMode,
    ) -> bool {
        self.hyprland && !delivery.is_foreground()
    }

    fn new(hyprland: bool, outcome: Outcome) -> Arc<Self> {
        Arc::new(Self {
            hyprland,
            outcome,
            ax: AtomicUsize::new(0),
        })
    }
}

impl ObservedScrollTarget for Arc<Backend> {
    fn semantic_scroll(
        &self,
        direction: &str,
        amount: usize,
        by: cua_driver_contract::ScrollBy,
    ) -> anyhow::Result<crate::atspi::ScrollProgress> {
        self.ax.fetch_add(1, Ordering::SeqCst);
        assert_eq!(direction, "down");
        assert_eq!(amount, 3);
        assert_eq!(by, cua_driver_contract::ScrollBy::Line);
        let (acknowledged, complete, detail) = match self.outcome {
            Outcome::Miss => {
                anyhow::bail!("no matching scroll Action or Value; no mutation attempted")
            }
            Outcome::Complete => (3, true, None),
            Outcome::StaleAfterOne => (
                1,
                false,
                Some("stale_element_token: object left frame".into()),
            ),
            Outcome::TimeoutAfterOne => (1, false, Some("scroll action timed out".into())),
            Outcome::UncertainWithoutAck => {
                (0, false, Some("attempted scroll outcome unknown".into()))
            }
        };
        Ok(crate::atspi::ScrollProgress {
            acknowledged,
            complete,
            detail,
        })
    }
}

/// Assert zero nonsemantic dispatch sites in the production element helper.
/// Combined with the real Tool route below and the integration branch guards,
/// this rejects even ignored-error input calls (not just changed return values).
fn assert_no_input_dispatch() {
    let source = include_str!("impl_.rs");
    let helper = source
        .rsplit_once(concat!("trait ObservedScroll", "Target: Send {"))
        .unwrap()
        .1
        .split_once(concat!(
            "#[cfg(test)]\n#[test]\nfn retained_scroll_",
            "chromium"
        ))
        .unwrap()
        .0;
    let compact = helper.split_whitespace().collect::<String>();
    for forbidden in [
        "send_",
        "pointer",
        "vptr",
        "xtest",
        "libei",
        "execute_foreground",
        "hyprland_input::",
        "foreground_hyprland_action(",
        "with_foreground",
        "with_x11_foreground",
        "activate",
        "grab_focus",
        "focus_observed",
        "hotkey(",
        "press_key(",
        "type_text(",
        "scroll_at(",
        "scroll_at_with_outcome(",
        "scroll_with_outcome(",
        "scroll_desktop(",
        "real_pointer_input_available(",
    ] {
        assert!(
            !compact.contains(forbidden),
            "nonsemantic dispatch/probe: {forbidden}"
        );
    }
    // MPX availability cannot admit a route that never queries input capabilities.
    assert!(!compact
        .replace("crate::input::delivery::", "")
        .contains("crate::input::"));
}

fn invoke(backend: Arc<Backend>, foreground: bool) -> ToolResult {
    cua_driver_core::tool::with_runtime_scope(
        format!("scroll-route-{}", uuid::Uuid::new_v4()),
        || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let mut state = ToolState::new();
                    Arc::get_mut(&mut state).unwrap().observed_scroll_backend = Some(backend);
                    let pid = std::process::id();
                    let node = crate::atspi::AtspiNode {
                        element_index: Some(7),
                        element_key: 41,
                        identity: Some(crate::atspi::AtspiIdentity {
                            bus_name: ":1.41".into(),
                            path: "/scroll".into(),
                            frame_bus_name: ":1.41".into(),
                            frame_path: "/frame".into(),
                        }),
                        role: "scroll pane".into(),
                        name: None,
                        value: None,
                        checked: None,
                        enabled: None,
                        selected: None,
                        description: None,
                        actions: vec![],
                        depth: 0,
                        parent_element_index: None,
                        in_web_content: false,
                        object_ref: None,
                    };
                    let id = state
                        .snapshots
                        .publish(state.snapshots.prepare(pid, 0x7f30_0106, &[node]).unwrap())
                        .unwrap();
                    ScrollTool { state }.invoke(json!({
                "pid":pid, "element_token":cua_driver_core::element_token::token_for(id, 7),
                "direction":"down", "amount":3,
                "delivery_mode":if foreground { "foreground" } else { "background" }
            })).await
                })
        },
    )
}

#[test]
fn foreground_ax_miss_refuses_without_input_dispatch() {
    for hyprland in [false, true] {
        let backend = Backend::new(hyprland, Outcome::Miss);
        let result = invoke(backend.clone(), true);
        assert_eq!(result.is_error, Some(true));
        let text = serde_json::to_string(&result.content).unwrap();
        assert!(text.contains("the element exposes no accessible scroll action"));
        assert!(text.contains("retry scroll with x,y pixel coordinates instead of element_token"));
        let public = result.structured_content.unwrap();
        assert_eq!(public["code"], "element_route_unqualified");
        assert_eq!(public["refusal"]["code"], "element_route_unqualified");
        assert_eq!(public["effect"], "refused");
        assert_eq!(backend.ax.load(Ordering::SeqCst), 1);
        assert_no_input_dispatch();
    }
}

#[test]
fn background_ax_miss_refuses_regardless_of_mpx_or_isolated_hyprland() {
    for hyprland in [false, true] {
        let backend = Backend::new(hyprland, Outcome::Miss);
        let result = invoke(backend.clone(), false);
        let expected = crate::input::delivery::background_unavailable_error(
            crate::input::delivery::BackgroundUnavailable::FocusedInputOnly,
        );
        assert_eq!(result.is_error, Some(true));
        assert_eq!(result.structured_content, expected.structured_content);
        assert_eq!(
            serde_json::to_value(&result.content).unwrap(),
            serde_json::to_value(&expected.content).unwrap()
        );
        assert_eq!(backend.ax.load(Ordering::SeqCst), 1);
        assert_no_input_dispatch();
    }
}

#[test]
fn semantic_scroll_still_allowed_in_both_delivery_modes() {
    for hyprland in [false, true] {
        for foreground in [false, true] {
            let backend = Backend::new(hyprland, Outcome::Complete);
            let result = invoke(backend.clone(), foreground);
            assert_ne!(result.is_error, Some(true));
            assert_eq!(
                result.structured_content.unwrap()["delivery"]["delivered_count"],
                3
            );
            assert_eq!(
                result.action_record.unwrap().transport,
                cua_driver_core::action_record::ActionTransport::LinuxAtSpiAction
            );
            assert_eq!(backend.ax.load(Ordering::SeqCst), 1);
            assert_no_input_dispatch();
        }
    }
}

#[test]
fn ax_partial_progress_then_failure_preserves_count_and_refuses_remainder() {
    for foreground in [false, true] {
        for outcome in [Outcome::StaleAfterOne, Outcome::TimeoutAfterOne] {
            let backend = Backend::new(false, outcome);
            let result = invoke(backend.clone(), foreground);
            assert_eq!(result.is_error, Some(true));
            let public = result.structured_content.unwrap();
            if matches!(outcome, Outcome::StaleAfterOne) {
                assert_eq!(public["refusal"]["code"], "stale_element_token");
            }
            assert_eq!(public["delivery"]["delivered_count"], 1);
            assert_eq!(public["delivery"]["mode"], "unknown");
            let record = result.action_record.unwrap();
            assert_eq!(
                record.effect,
                cua_driver_core::action_record::ActionEffect::Partial
            );
            assert_eq!(
                record.transport,
                cua_driver_core::action_record::ActionTransport::LinuxAtSpiAction
            );
            assert_eq!(
                backend.ax.load(Ordering::SeqCst),
                1,
                "never replay the remainder"
            );
            assert_no_input_dispatch();
        }
    }
}

#[test]
fn ax_attempt_without_ack_is_uncertain_not_an_action_miss() {
    for foreground in [false, true] {
        let backend = Backend::new(false, Outcome::UncertainWithoutAck);
        let result = invoke(backend.clone(), foreground);
        assert_eq!(result.is_error, Some(true));
        let record = result.action_record.unwrap();
        assert_eq!(
            record.effect,
            cua_driver_core::action_record::ActionEffect::Unverifiable
        );
        assert_eq!(
            record.transport,
            cua_driver_core::action_record::ActionTransport::LinuxAtSpiAction
        );
        assert_eq!(backend.ax.load(Ordering::SeqCst), 1);
        assert_no_input_dispatch();
    }
}
