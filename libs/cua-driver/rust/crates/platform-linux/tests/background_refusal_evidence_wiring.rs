//! Ordering/inventory owner for request-wide non-delivery proof. These guards
//! complement the real Tool::invoke serialization test; they do not prove
//! compositor delivery. Split anchors cannot match their own source text.
const TOOLS: &str = include_str!("../src/tools/impl_.rs");
const RETAINED: &str = include_str!("../src/tools/retained_click.rs");

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
fn background_refusal_evidence_stays_in_pre_input_admission() {
    // Every converted tool's actual impl block, not an unbounded source search.
    // Desktop, retained-element and isolated branches above some guards return
    // their result; they cannot fall through and claim zero after dispatch.
    for (tool, end, boundary) in [
        (
            "ClickTool",
            concat!("// ── px-focus ", "helper"),
            "// Coordinate-based path.",
        ),
        (
            "TypeTextTool",
            concat!("// ── press_", "key"),
            "let px = args.get(\"x\")",
        ),
        (
            "PressKeyTool",
            concat!("// ── hot", "key"),
            "let px = args.get(\"x\")",
        ),
        (
            "HotkeyTool",
            concat!("// ── set_", "value"),
            "let px = args.get(\"x\")",
        ),
        (
            "ScrollTool",
            concat!("// `Screenshot", "Tool`"),
            "if crate::wayland::is_gnome_wayland_session() && xid_opt.is_none()",
        ),
        (
            "DoubleClickTool",
            concat!("// ── right_", "click"),
            "if args.get(\"element_index\").is_some()",
        ),
        (
            "RightClickTool",
            concat!("// ── ", "drag"),
            "if args.get(\"element_index\").is_some()",
        ),
        (
            "DragTool",
            concat!("// ── mouse_button_", "down"),
            "let coerce = |key: &str|",
        ),
    ] {
        let start = format!("{}{} {{", concat!("impl Tool ", "for "), tool);
        let body = section(TOOLS, &start, end);
        let first = concat!("unavailable_chromium_", "background(pid, delivery)");
        assert_eq!(body.matches(first).count(), 1, "{tool}: guard inventory");
        ordered(body, &[first, "return refusal", boundary]);
        // Native side-effect paths above that admission guard must return,
        // never rejoin the guard with a possibly delivered request.
        if matches!(tool, "TypeTextTool" | "PressKeyTool" | "HotkeyTool") {
            let prefix = body.split_once(first).unwrap().0;
            ordered(
                prefix,
                &["if isolated_hyprland_background(delivery)", "return"],
            );
        }
        if matches!(tool, "ClickTool" | "ScrollTool" | "DragTool") {
            let prefix = body.split_once(first).unwrap().0;
            assert!(
                prefix.contains(if tool == "ClickTool" {
                    "return match r {"
                } else {
                    "return match result"
                }),
                "{tool}: desktop path must return"
            );
        }
    }
    // Guard-specific secondary sites have no preceding mutation in the same
    // route. Sibling routes return before these slices are reached.
    for (tool, end, first, last, expected) in [
        (
            "TypeTextTool",
            concat!("// ── press_", "key"),
            "if let (Some(cx), Some(cy)) = (px, py)",
            "focus_by_pixel(",
            "unavailable_webkit_keyboard_background(pid, delivery)",
        ),
        (
            "ScrollTool",
            concat!("// `Screenshot", "Tool`"),
            "let native_refusal = if isolated_background",
            "coordinate_scroll_scale(native_refusal",
            "unavailable_wayland_focused_input_background(delivery, true)",
        ),
        (
            "ScrollTool",
            concat!("// `Screenshot", "Tool`"),
            "if crate::wayland::wayland_input_enabled() {\n            if let Some(refusal)",
            "let direction_for_wayland",
            "unavailable_wayland_focused_input_background(delivery, false)",
        ),
        (
            "ScrollTool",
            concat!("// `Screenshot", "Tool`"),
            "if let Some(refusal) = unavailable_webkit_background",
            "let element_point",
            "unavailable_gtk_pointer_background(pid, delivery)",
        ),
    ] {
        let start = format!("{}{} {{", concat!("impl Tool ", "for "), tool);
        let body = section(TOOLS, &start, end);
        let admission = section(body, first, last);
        assert!(admission.contains(expected), "{tool}: {expected}");
        for forbidden in [
            "send_",
            ".focus(",
            ".perform_action(",
            "semantic_scroll(",
            "with_x11_foreground(",
            "with_target_foreground(",
        ] {
            assert!(
                !admission.contains(forbidden),
                "{tool}: input before refusal: {forbidden}"
            );
        }
    }
    let scroll = section(
        TOOLS,
        concat!("impl Tool for ", "ScrollTool {"),
        concat!("// `Screenshot", "Tool`"),
    );
    ordered(
        scroll,
        &[
            "if crate::wayland::is_inject_mode() {",
            "let Some((x, y)) = pixel_target else",
            "background_unavailable_before_input_error(",
            "crate::wayland::scroll_at(",
        ],
    );
    let qualify = section(
        RETAINED,
        concat!("fn qualify_on_", "platform("),
        concat!("#[derive(Clone, Copy", ", Debug, PartialEq, Eq)]"),
    );
    ordered(
        qualify,
        &[
            "if !plain && !delivery.is_foreground()",
            "background_unavailable_before_input_error(",
            "Ok(())",
        ],
    );
    for forbidden in [
        ".focus(",
        ".perform_action(",
        "send_",
        "activate",
        "scroll_into_view",
    ] {
        assert!(
            !qualify.contains(forbidden),
            "qualification mutates: {forbidden}"
        );
    }

    // Post-attempt sites must NOT gain the helper or a proof-producing guard.
    // These are separate real production seams, not blanket string matching.
    for (start, end) in [
        (
            concat!("async fn click_", "indexed_x11("),
            concat!("impl Tool for ", "ClickTool {"),
        ),
        (
            concat!("async fn invoke_observed_", "keyboard("),
            concat!("/// px-focus for ", "the keyboard family"),
        ),
        (
            concat!("async fn invoke_observed_", "scroll("),
            concat!("#[cfg(test)]\n#[test]\nfn retained_scroll_", "chromium"),
        ),
        (
            concat!("fn synthetic_pointer_", "fallback_refusal("),
            concat!("fn synthetic_pointer_fallback_", "refusal_for("),
        ),
        (
            concat!("fn background_keyboard_", "refusal("),
            concat!("fn background_keyboard_refusal_", "with_bus_hint("),
        ),
    ] {
        let body = section(TOOLS, start, end);
        assert!(
            body.contains("background_unavailable_error"),
            "legacy refusal must remain: {start}"
        );
        for forbidden in [
            "background_unavailable_before_input",
            "unavailable_chromium_background(",
        ] {
            assert!(
                !body.contains(forbidden),
                "post-attempt evidence: {start}: {forbidden}"
            );
        }
    }
    // Admission helpers alone select the opt-in evidence constructor.
    for (start, end) in [
        (
            concat!("fn unavailable_webkit_", "background("),
            concat!("fn unavailable_webkit_keyboard_", "background("),
        ),
        (
            concat!("fn unavailable_webkit_keyboard_", "background("),
            concat!("/// Add accessibility ", "setup details"),
        ),
        (
            concat!("fn unavailable_gtk_keyboard_", "background("),
            concat!("/// Refusal for a background ", "element click"),
        ),
        (
            concat!("fn unavailable_gtk_pointer_", "background("),
            concat!("fn unavailable_wayland_focused_input_", "background("),
        ),
        (
            concat!("fn unavailable_wayland_focused_input_", "background("),
            concat!("/// Chromium's X11 ", "renderer"),
        ),
        (
            concat!("fn unavailable_chromium_", "background("),
            concat!("fn chromium_background_must_", "refuse("),
        ),
    ] {
        let body = section(TOOLS, start, end);
        assert_eq!(
            body.matches("background_unavailable_before_input_error(")
                .count(),
            1,
            "{start}"
        );
    }
}
