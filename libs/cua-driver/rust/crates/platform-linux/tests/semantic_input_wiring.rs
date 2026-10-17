//! Pin the shared admission/dispatch decision and the retained-route boundaries.
#[test]
fn semantic_input_selector_and_dispatch_share_the_policy() {
    let core = include_str!("../../cua-driver-core/src/tool.rs");
    assert!(core.contains(
        "let input_route = crate::action_lease::input_route_for_tool(resolved_name, &args)"
    ));
    assert!(core.contains("admit_action_lease(resolved_name, &args, input_route)"));
    assert!(core.contains("input_route.scope("));
    let hooks = include_str!("../src/tools/mod.rs");
    assert!(hooks.contains("install_input_route_hook(crate::wayland::input_route_for_tool)"));
    let tools = include_str!("../src/tools/impl_.rs");
    for tool in ["click", "type_text", "set_value", "scroll"] {
        assert!(tools.contains(&format!(
            "semantic_input::validate_dispatch(\"{tool}\", &args)"
        )));
    }
    let start = tools
        .find(concat!("async fn ", "invoke_observed_scroll("))
        .unwrap();
    let end = tools[start..].find("#[cfg(test)]\n#[test]").unwrap() + start;
    let scroll = &tools[start..end];
    assert!(scroll.contains("target.semantic_scroll("));
    assert!(!scroll.contains("with_target_foreground"));
    assert!(!scroll.contains("send_wayland"));
    assert!(!scroll.contains("libei::"));
}
