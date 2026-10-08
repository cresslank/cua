//! Retained scroll wiring complements identity/sequence and permit tests.
//! These source guards do not claim D-Bus or compositor delivery.
const TOOLS: &str = include_str!("../src/tools/impl_.rs");
const NATIVE: &str = include_str!("../src/atspi/native.rs");
fn section<'a>(source: &'a str, start: &str, end: &str) -> &'a str {
    source
        .rsplit_once(start)
        .unwrap()
        .1
        .split_once(end)
        .unwrap()
        .0
}
fn ordered(source: &str, needles: &[&str]) {
    let compact = source.split_whitespace().collect::<String>();
    let mut rest = compact.as_str();
    for needle in needles {
        let needle = needle.split_whitespace().collect::<String>();
        rest = rest
            .split_once(&needle)
            .unwrap_or_else(|| panic!("missing/out of order: {needle}"))
            .1;
    }
}

#[test]
fn retained_scroll_routes_never_use_ordinals_or_skip_background_refusal() {
    let route = section(
        TOOLS,
        concat!("impl Tool for ", "ScrollTool {"),
        concat!("// `Screenshot", "Tool`"),
    );
    let helper = section(
        TOOLS,
        concat!("trait ObservedScroll", "Target: Send {"),
        concat!("#[cfg(test)]\n#[test]\nfn retained_scroll_", "chromium"),
    );
    for source in [route, helper] {
        for forbidden in [
            concat!("scroll_", "element("),
            concat!("focus_", "element("),
            concat!("type_into_editable", "_at("),
            concat!("get_element_", "bounds("),
            concat!("get_element_bounds_", "for_window("),
            concat!("resolve_element_", "local_coords("),
            concat!("element_screen_", "center("),
            ".nth(",
        ] {
            assert!(!source.contains(forbidden), "ordinal route: {forbidden}");
        }
    }
    ordered(
        route,
        &[
            "if !isolated_background",
            "unavailable_chromium_background(pid, delivery)",
            "return refusal",
            "establish_exact_target(pid, exact_window_id)",
            "return invoke_observed_scroll(",
            "explicit_keyboard_cursor_target(pid, xid, None, pixel_target)",
        ],
    );
    assert!(!route.contains("resolved_element_index"));
    ordered(
        helper,
        &[
            "acquire_observed_mutation(snapshot_identity, index)",
            "Some(crate::wayland::establish_exact_target(pid, xid)?)",
            "resolve_observed_target(pid, index, xid, &identity, proof)",
            "Box::new(target) as Box<dyn ObservedScrollTarget>",
            "with_retained_mutation(permit.clone()",
            "target.semantic_scroll(&direction_for_ax, amount, by)",
            "contains(\"stale_element_token\")",
            "return observed_keyboard_error(error)",
        ],
    );
    assert!(helper.contains("scroll_observed_element(self, direction, amount, by)"));
}

#[test]
fn retained_scroll_native_uses_original_object_and_fixed_page_probes() {
    let scroll = section(
        NATIVE,
        concat!("pub fn scroll_observed_", "element("),
        concat!("#[cfg(test)]\nmod retained_", "scroll_tests"),
    );
    for forbidden in [
        "collect_visited",
        ".nth(",
        "get_element_bounds",
        "resolve_observed_target",
    ] {
        assert!(
            !scroll.contains(forbidden),
            "native scroll re-resolved: {forbidden}"
        );
    }
    ordered(
        scroll,
        &[
            "let target = &visited[observed.target_position]",
            "observed.verify_live_at_mutation().await?",
            "target.acc.proxies()",
            "action.get_name(action_index)",
            "let target_index = observed.target_position",
            "Fix probe identities before dispatch; never replace vanished descendants",
            "descendant_indices(",
            "scroll_sequence(",
            "observed.verify_live_at_mutation().await?",
            "action.do_action(action_index)",
            "observed.verify_live_at_mutation().await?",
            "attempted.set(true)",
            "call(value.set_current_value(next))",
            "finish_scroll(",
        ],
    );
    let resolve = section(
        NATIVE,
        concat!("pub fn resolve_observed_", "target("),
        concat!("/// A mutation was ", "attempted;"),
    );
    ordered(
        resolve,
        &[
            "validate_exact_target(proof)",
            "unique_observed_identity_position(",
            "identity,",
            "frame_ordinal,",
            "Ok(ObservedClickTarget",
        ],
    );
}

#[test]
fn retained_scroll_element_branch_has_no_nonsemantic_dispatch() {
    let route = section(
        TOOLS,
        concat!("impl Tool for ", "ScrollTool {"),
        concat!("// `Screenshot", "Tool`"),
    );
    let branch = section(
        route,
        concat!(
            "if let cua_driver_core::element_token::ResolvedElement::",
            "Element {"
        ),
        concat!("if named_session_cursor_key", "(&args).is_some() {"),
    );
    let helper = section(
        TOOLS,
        concat!("trait ObservedScroll", "Target: Send {"),
        concat!("#[cfg(test)]\n#[test]\nfn retained_scroll_", "chromium"),
    );
    let native = section(
        NATIVE,
        concat!("pub fn scroll_observed_", "element("),
        concat!("#[cfg(test)]\nmod retained_", "scroll_tests"),
    );
    for source in [branch, helper, native] {
        let compact = source.split_whitespace().collect::<String>();
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
        assert!(!compact
            .replace("crate::input::delivery::", "")
            .contains("crate::input::"));
    }
    ordered(branch, &["return invoke_observed_scroll(", ".await;"]);
    ordered(helper, &[
        "with_retained_mutation(permit.clone()",
        "target.semantic_scroll(&direction_for_ax, amount, by)",
        "return observed_scroll_progress(progress, foreground)",
        "if !foreground",
        "background_unavailable_error(",
        "element_route_unqualified: the element exposes no accessible scroll action; retry scroll with x,y pixel coordinates",
    ]);
    for removed in [
        concat!("observed_scroll_", "pointer("),
        concat!("observed_scroll_checked_", "point("),
        concat!("fn pointer_", "step("),
    ] {
        assert!(!TOOLS.contains(removed), "dead fallback: {removed}");
    }
    let snapshot = include_str!("../src/atspi/snapshot.rs");
    let hold = section(
        snapshot,
        concat!("pub(crate) fn with_retained_", "mutation<R>("),
        concat!("impl Default for ", "Snapshots {"),
    );
    ordered(hold, &["let result = mutation()", "drop(permit)", "result"]);
    let wayland = include_str!("../src/wayland/mod.rs");
    assert!(!wayland.contains(concat!("scroll_at_with_observed_", "point(")));
}
