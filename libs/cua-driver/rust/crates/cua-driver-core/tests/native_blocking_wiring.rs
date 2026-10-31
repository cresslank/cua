//! Source guards for native call sites that must retain bounded admission.
//! These run on every host, including for platform sources unavailable to its
//! compiler. Native behavior still needs the corresponding target's own gates.

const GOVERNED_SOURCES: &[(&str, &str)] = &[
    ("core clipboard", include_str!("../src/clipboard.rs")),
    (
        "core browser setup",
        include_str!("../src/browser/prepare.rs"),
    ),
    (
        "Linux browser platform",
        include_str!("../../platform-linux/src/browser_platform.rs"),
    ),
    (
        "Windows browser platform",
        include_str!("../../platform-windows/src/browser_platform.rs"),
    ),
    (
        "macOS browser platform",
        include_str!("../../platform-macos/src/browser/platform.rs"),
    ),
    (
        "macOS browser JavaScript",
        include_str!("../../platform-macos/src/browser/browser_js.rs"),
    ),
    (
        "Linux tools",
        include_str!("../../platform-linux/src/tools/impl_.rs"),
    ),
    (
        "Windows tools",
        include_str!("../../platform-windows/src/tools/impl_.rs"),
    ),
    (
        "macOS tools",
        include_str!("../../platform-macos/src/tools/mod.rs"),
    ),
];

#[test]
fn native_platform_and_browser_call_sites_keep_bounded_admission() {
    for (name, source) in GOVERNED_SOURCES {
        // Browser prepare's raw calls belong only to its final test module.
        // Other governed files have no raw calls, including in inline tests.
        let production = if *name == "core browser setup" {
            source.split("#[cfg(test)]").next().unwrap()
        } else {
            source
        };
        assert!(
            !production.contains("task::spawn_blocking"),
            "{name} bypasses bounded native-call admission; use cua_driver_core::blocking::spawn"
        );
    }
}
