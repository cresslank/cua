//! Portable source qualification for the deferred macOS background attach route.
//! Runtime refusal/foreground behavior is owned by browser::v2_tests. This test
//! prevents native-only code from silently opting back into an unqualified route.

#[test]
fn macos_background_attach_keeps_the_default_refusal() {
    let platform = include_str!("../../platform-macos/src/browser/platform.rs");
    let setup = include_str!("../../platform-macos/src/browser/setup_ui.rs");
    let skylight = include_str!("../../platform-macos/src/input/skylight.rs");
    let key_guard = include_str!("../../platform-macos/src/key_focus_guard.rs");
    assert!(!platform.contains("fn guard_background_attach("));
    let core = include_str!("../src/browser/platform.rs");
    let default_guard = core
        .split("fn guard_background_attach(")
        .nth(1)
        .expect("keep the shared platform extension point")
        .split('}')
        .next()
        .unwrap();
    assert!(default_guard.ends_with("{\n        None\n    "));

    for source in [platform, setup, skylight, key_guard] {
        // The existing joined-thread serialization unit test is not a native
        // production watcher and must remain available to native qualification.
        let production = source.split("\n#[cfg(test)]\nmod tests").next().unwrap();
        let compact: String = production.chars().filter(|c| !c.is_whitespace()).collect();
        for forbidden in [
            "SelfActivationGuard",
            "GUARDED_ATTACHES",
            "BACKGROUND_KEY_TURN",
            "guarded_attach_active",
            "with_background_key_window",
            "foreground_turn_active",
            "std::thread::spawn",
        ] {
            assert!(!compact.contains(forbidden), "deferred route: {forbidden}");
        }
    }
    let setup: String = setup.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(
        !setup.contains("walk_tree(pid,None,"),
        "setup must retain its exact window, never fall back to the app tree"
    );
}
