//! Narrow source-edge keeper: receiver behavior lives in helper tests/private GNOME lab.
#[test]
fn switch_routes_keep_the_dedicated_gnome_keyboard_exception_contained() {
    let keyboard = include_str!("../src/wayland/window_switch.rs");
    let tool = include_str!("../src/tools/window_switch.rs");
    let worker = include_str!("../src/wayland/libei.rs");
    assert!(tool.contains("acquire_host_raw_input_lease()"));
    assert!(tool.contains("contract::should_fallback(args.method, &attempt)"));
    assert!(keyboard.contains("peer.deliver_search(target, query)?"));
    assert!(keyboard.contains("peer.cancel_search(target)"));
    assert!(keyboard.contains("super::libei::switch_overlay("));
    let overlay = worker
        .split("pub(crate) fn switch_overlay(")
        .nth(1)
        .unwrap()
        .split("/// Submit an ordered sequence")
        .next()
        .unwrap();
    assert!(overlay.contains("keycode: 125"));
    for forbidden in [
        "Cmd::TypeText",
        "Cmd::KeySequence",
        "SwitchInput",
        "libei::type_text",
        "libei::press_key",
    ] {
        assert!(!overlay.contains(forbidden), "{forbidden}");
        assert!(!keyboard.contains(forbidden), "{forbidden}");
    }
    assert!(!keyboard.contains("reconnect_for_readiness("));
    let ordinary = include_str!("../src/wayland/mod.rs");
    let guard = ordinary
        .split("fn reject_unsafe_gnome_desktop_input(operation: &str)")
        .nth(1)
        .unwrap()
        .split("fn with_operation_bound_validation")
        .next()
        .unwrap();
    assert!(guard.contains("exact_target_required:"));
    assert!(!guard.contains("switch_window"));
}
