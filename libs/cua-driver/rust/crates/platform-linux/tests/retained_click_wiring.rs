//! Headless source-wiring guards for OS-gated mutation boundaries.
//! These complement the real identity/cache unit tests; they do NOT execute
//! D-Bus, X11, a compositor, or the native tool handlers.

fn section<'a>(source: &'a str, start: &str, end: &str) -> &'a str {
    source
        .split_once(start)
        .unwrap()
        .1
        .split_once(end)
        .unwrap()
        .0
}

fn ordered(source: &str, needles: &[&str]) {
    // Formatting can split a call across lines without changing its ordering.
    let source = source.split_whitespace().collect::<String>();
    let mut rest = source.as_str();
    for needle in needles {
        let compact = needle.split_whitespace().collect::<String>();
        rest = rest
            .split_once(compact.as_str())
            .unwrap_or_else(|| panic!("missing or out-of-order boundary: {needle}"))
            .1;
    }
}

const NATIVE: &str = include_str!("../src/atspi/native.rs");
const TOOLS: &str = include_str!("../src/tools/impl_.rs");

#[test]
fn action_revalidates_retained_ancestry_after_action_vector_before_dispatch() {
    let target = section(
        NATIVE,
        "impl ObservedClickTarget {",
        "pub fn resolve_observed_click_target(",
    );
    ordered(
        target,
        &[
            "async fn verify_live_at_mutation(&self)",
            "target.acc.get_state()",
            "verify_observed_frame_ancestry(identity,",
            "window_belongs_to_pid(self.xid, self.pid)",
            "pub fn verify_live(&self)",
            "self.verify_live_at_mutation()",
            "pub fn perform_action(&self",
            "live_action_names(&action).await?",
            "self.verify_live_at_mutation().await?",
            "action.do_action(chosen as i32).await?",
            "require_affirmative_ack",
        ],
    );
    assert!(!target.contains("collect_visited"));
    assert!(!target.contains("get_element_bounds("));
    assert!(!target.contains("exact_window_element_index("));
    let parent = section(
        NATIVE,
        "async fn live_parent_address(",
        "impl ObservedClickTarget {",
    );
    ordered(
        parent,
        &[
            "call(async",
            "accessible_for(",
            "get_property(\"Parent\")",
            "identity_ref(",
        ],
    );
    assert!(NATIVE.contains(".cache_properties(atspi::zbus::proxy::CacheProperties::No)"));
}

#[test]
fn pointer_delivery_revalidates_after_overlay_geometry_and_foreground_activation() {
    let route = section(
        TOOLS,
        "async fn click_indexed_x11(",
        "impl Tool for ClickTool",
    );
    ordered(
        route,
        &[
            "acquire_observed_mutation",
            "resolve_observed_click_target",
            "Ok((permit, target, center))",
            "reveal_pointer_action_for",
            "let _permit = permit",
            "target.verify_live()?",
            "target.perform_action(",
            "click_error_allows_pointer_fallback",
            "target.screen_bounds()?",
            "with_x11_foreground(xid, 80, ||",
            "let (lx, ly) = local_center()?",
            "window_local_to_screen(xid, lx, ly)?",
            "target.verify_live()?",
            "send_click_xtest_desktop_with_modifiers(",
            "target.verify_live()?",
            "send_click_with_modifiers(",
        ],
    );
    assert_eq!(
        route.matches("cua_driver_core::blocking::spawn(").count(),
        2
    );
    assert!(!route.contains("tokio::task::spawn_blocking"));
    assert!(!route.contains("resolve_element_local_coords("));
    assert!(!route.contains("element_screen_center("));
}

#[test]
fn isolated_semantic_click_carries_original_proof_and_only_miss_can_fall_back() {
    let click = section(TOOLS, "impl Tool for ClickTool", "async fn focus_by_pixel");
    let semantic = section(
        click,
        "let Some(proof) = exact_target_proof.clone()",
        "return isolated_hyprland_action(",
    );
    ordered(
        semantic,
        &[
            "cua_driver_core::blocking::spawn(",
            "exact_point_action_may_fallback(state_for_semantic.point_action(",
            "&proof, output_x, output_y",
            "Ok(Ok(false))",
            "Ok(Ok(true)) => {}",
            "Ok(Err(error))",
            "return ToolResult::error",
            "Err(error) => return ToolResult::error",
        ],
    );
    assert!(!semantic.contains("establish_exact_target("));
    assert!(!semantic.contains("tokio::task::spawn_blocking"));
}

#[test]
fn restored_capture_dispatch_keeps_private_and_gnome_exact_capture_gates() {
    let wayland = include_str!("../src/wayland/mod.rs");
    let route = section(
        wayland,
        "fn screenshot_dispatch_for_pid(",
        "fn isolated_private_window_capture_with_dispatch(",
    );
    ordered(
        route,
        &[
            "if !is_wayland()",
            "if is_inject_mode()",
            "inject_capture_lease(xid)?",
            "isolated_private_window_capture_with_dispatch(",
            "if hyprland::is_session()",
            "return hyprland::capture(xid, pid).map_err(",
            "surface_identity_unproven(",
            "if shell_helper::present()",
            "if !shell_helper::available()",
            "return shell_helper::screenshot_window(xid)",
            "screenshot_window_bytes_with_dispatch(",
        ],
    );
}

#[test]
fn desktop_click_uses_released_flat_parser_not_sdk_sum_type() {
    let desktop = section(
        TOOLS,
        "if has_xy && !has_pid && !has_window_id",
        "// Surface 5:",
    );
    assert!(desktop.contains("cua_driver_core::tool_args::parse_legacy_click_input(&args)"));
    assert!(!desktop.contains("parse_typed_projection::<ClickInput>"));
    let parsed = cua_driver_core::tool_args::parse_legacy_click_input(&serde_json::json!({
        "scope": "desktop", "x": 3.0, "y": 7.0, "button": "left", "count": 1,
    }))
    .unwrap();
    assert_eq!((parsed.x, parsed.y), (3.0, 7.0));
}
