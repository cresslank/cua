//! GNOME routes which can only mutate through retained AT-SPI, or refuse.
//! This conservative decision is shared by admission and dispatch validation.
use cua_driver_core::action_lease::{current_input_route, InputRoute};
use serde_json::Value;

pub(super) fn route(tool: &str, args: &Value, gnome: bool) -> InputRoute {
    // Do not exempt coordinate/mixed forms, legacy ordinals, foreground,
    // malformed policy fields, or other compositors, even if some refuse early.
    let background = match args.get("delivery_mode") {
        None => true,
        Some(Value::String(mode)) => mode == "background",
        _ => false,
    };
    let token = args
        .get("element_token")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty());
    let coordinates = ["x", "y", "capture_id"]
        .iter()
        .any(|key| args.get(*key).is_some())
        || args
            .get("from_zoom")
            .is_some_and(|v| v.as_bool() != Some(false))
        || ["scope", "coordinate_frame"]
            .iter()
            .any(|key| args.get(*key).is_some_and(|v| v.as_str() != Some("window")));
    // Typed window targets normalize to scope=window before this decision;
    // that is an exact element target, not a coordinate action.
    if !gnome || !background || !token || coordinates {
        return InputRoute::Unrestricted;
    }
    let only_atspi = match tool {
        "click" => {
            let left = args
                .get("button")
                .is_none_or(|v| v.as_str() == Some("left"));
            let single = args.get("count").is_none_or(|v| v.as_u64() == Some(1));
            let unmodified = ["modifier", "modifiers"].iter().all(|key| {
                args.get(*key)
                    .is_none_or(|v| v.as_array().is_some_and(Vec::is_empty))
            });
            left && single && unmodified
        }
        // Addressed background text is EditableText-or-refuse, never keys.
        // Retained scroll and set_value likewise never rejoin input ladders.
        "type_text" | "set_value" | "scroll" => true,
        _ => false,
    };
    if only_atspi {
        InputRoute::AccessibilityOnly
    } else {
        InputRoute::Unrestricted
    }
}

pub fn input_route_for_tool(tool: &str, args: &Value) -> InputRoute {
    // Most tools cannot use this route. Do not add a Shell/helper probe to
    // observations or raw-input forms just to rediscover that fact.
    if route(tool, args, true) == InputRoute::Unrestricted {
        return InputRoute::Unrestricted;
    }
    route(
        tool,
        args,
        !super::is_inject_mode() && super::is_gnome_wayland_session(),
    )
}

/// The dispatcher consumes the admission decision, not a second exemption
/// list. Refuse changed arguments/session facts rather than selecting a raw
/// fallback under a grant whose transport readiness was skipped.
pub(crate) fn validate_dispatch(
    tool: &str,
    args: &Value,
) -> Result<(), cua_driver_core::protocol::ToolResult> {
    if current_input_route() == InputRoute::AccessibilityOnly
        && input_route_for_tool(tool, args) != InputRoute::AccessibilityOnly
    {
        return Err(cua_driver_core::protocol::ToolResult::error(
            "input_unavailable: accessibility-only route changed after admission",
        )
        .with_structured(serde_json::json!({"code":"input_unavailable", "effect":"refused"})));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn semantic_input_positive_routes() {
        for tool in ["click", "type_text", "set_value", "scroll"] {
            for extra in [
                json!({}),
                json!({"delivery_mode":"background"}),
                json!({"scope":"window", "coordinate_frame":"window", "from_zoom":false}),
                json!({"pid":123,"window_id":456,"element_index":7}),
            ] {
                let mut args = json!({"element_token":"s00000001:7"});
                args.as_object_mut()
                    .unwrap()
                    .extend(extra.as_object().unwrap().clone());
                assert_eq!(
                    route(tool, &args, true),
                    InputRoute::AccessibilityOnly,
                    "{tool} {args}"
                );
                assert_eq!(
                    route(tool, &args, false),
                    InputRoute::Unrestricted,
                    "X11/non-GNOME {tool}"
                );
            }
        }
        assert_eq!(
            route(
                "click",
                &json!({"element_token":"s:1","button":"left","count":1,"modifier":[]}),
                true
            ),
            InputRoute::AccessibilityOnly
        );
    }

    #[test]
    fn semantic_input_typed_window_normalization_preserves_the_exemption() {
        for tool in ["click", "type_text", "scroll"] {
            let mut args = json!({"element_token":"s00000001:7", "delivery_mode":"background",
                "target":{"kind":"window", "pid":123, "window_id":456}});
            cua_driver_core::action_target::normalize_action_target(tool, &mut args).unwrap();
            assert_eq!(args["scope"], "window");
            assert_eq!(route(tool, &args, true), InputRoute::AccessibilityOnly);
            args["x"] = json!(1);
            args["y"] = json!(2);
            assert_eq!(route(tool, &args, true), InputRoute::Unrestricted);
        }
    }

    #[test]
    fn semantic_input_raw_and_ambiguous_forms_keep_gate() {
        for tool in ["click", "type_text", "set_value", "scroll"] {
            for args in [
                json!({}),
                json!({"element_index":1}),
                json!({"element_token":null}),
                json!({"element_token":1}),
                json!({"element_token":""}),
            ] {
                assert_eq!(
                    route(tool, &args, true),
                    InputRoute::Unrestricted,
                    "{tool} {args}"
                );
            }
            for extra in [
                json!({"delivery_mode":"foreground"}),
                json!({"delivery_mode":"Foreground"}),
                json!({"delivery_mode":"BACKGROUND"}),
                json!({"delivery_mode":null}),
                json!({"delivery_mode":"auto"}),
                json!({"x":2}),
                json!({"y":2}),
                json!({"x":2,"y":3}),
                json!({"x":null}),
                json!({"capture_id":"c"}),
                json!({"from_zoom":true}),
                json!({"from_zoom":null}),
                json!({"scope":"desktop"}),
                json!({"coordinate_frame":"desktop"}),
            ] {
                let mut args = json!({"element_token":"s:1"});
                args.as_object_mut()
                    .unwrap()
                    .extend(extra.as_object().unwrap().clone());
                assert_eq!(
                    route(tool, &args, true),
                    InputRoute::Unrestricted,
                    "{tool} {args}"
                );
            }
        }
        for extra in [
            json!({"button":"right"}),
            json!({"button":"middle"}),
            json!({"button":"LEFT"}),
            json!({"button":""}),
            json!({"button":null}),
            json!({"count":2}),
            json!({"count":3}),
            json!({"count":0}),
            json!({"count":"1"}),
            json!({"modifier":["shift"]}),
            json!({"modifier":null}),
            json!({"modifiers":["ctrl"]}),
            json!({"modifier":[1]}),
        ] {
            let mut args = json!({"element_token":"s:1"});
            args.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            assert_eq!(
                route("click", &args, true),
                InputRoute::Unrestricted,
                "{args}"
            );
        }
        for tool in [
            "double_click",
            "right_click",
            "press_key",
            "hotkey",
            "drag",
            "mouse_drag",
            "parallel_mouse_drag",
            "move_cursor",
            "mouse_button_down",
            "mouse_button_up",
            "bring_to_front",
            "set_window_frame",
            "invoke_menu",
            "browser_prepare",
            "unknown",
        ] {
            assert_eq!(
                route(
                    tool,
                    &json!({"element_token":"s:1","delivery_mode":"background"}),
                    true
                ),
                InputRoute::Unrestricted,
                "{tool}"
            );
        }
    }
}

#[cfg(test)]
mod dispatch_revalidation_tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn changed_arguments_refuse_under_accessibility_only_route() {
        InputRoute::AccessibilityOnly
            .scope(async {
                for tool in ["click", "type_text", "set_value", "scroll"] {
                    for args in [
                        json!({}),
                        json!({"element_token":"s:1","x":1}),
                        json!({"element_token":"s:1","capture_id":"c"}),
                        json!({"element_token":"s:1","scope":"desktop"}),
                        json!({"element_token":"s:1","from_zoom":true}),
                        json!({"element_token":"s:1","delivery_mode":"foreground"}),
                    ] {
                        let result = validate_dispatch(tool, &args)
                            .expect_err("changed dispatch must refuse");
                        assert_eq!(
                            result.structured_content.unwrap()["code"],
                            "input_unavailable"
                        );
                        assert_eq!(current_input_route(), InputRoute::AccessibilityOnly);
                    }
                }
                for args in [
                    json!({"element_token":"s:1","count":2}),
                    json!({"element_token":"s:1","button":"right"}),
                    json!({"element_token":"s:1","modifier":["shift"]}),
                ] {
                    assert!(validate_dispatch("click", &args).is_err());
                }
            })
            .await;
        assert_eq!(current_input_route(), InputRoute::Unrestricted);
        assert!(validate_dispatch("click", &json!({"x":1,"y":2})).is_ok());
    }
}
