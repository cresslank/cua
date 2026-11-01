//! Persistent-switch certification on a disposable GNOME/Mutter seat only.
//! No installed driver, helper or host session is ever selected.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "needs the explicit private GNOME Shell lab runner and portal-input candidate"]
fn private_gnome_persistent_switch_native_keyboard_and_refusal() {
    let runner = std::env::var_os("CUA_GNOME_SHELL_LAB_RUNNER")
        .expect("set CUA_GNOME_SHELL_LAB_RUNNER to the reviewed run_shell_lab.sh");
    let driver =
        std::env::var_os("CUA_TEST_DRIVER_BIN").expect("set exact candidate CUA_TEST_DRIVER_BIN");
    let scratch = std::env::var_os("TMPDIR").expect("set an owned scratch TMPDIR, outside /tmp");
    let fixture =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/gnome-switch/run.py");
    let helper = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../wayland-helper/winrects@cua");
    let status = std::process::Command::new("python3")
        .arg(fixture)
        .arg(runner)
        .arg(helper)
        .arg(driver)
        .arg(scratch)
        .status()
        .expect("start private GNOME test harness");
    assert!(
        status.success(),
        "private GNOME persistent-switch certification failed: {status}"
    );
}
