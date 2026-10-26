//! Exercise the public Linux screenshot entry points in an isolated subprocess.
//! An advertised GNOME session without WinRects must never reach a capture
//! backend, even with a missing compositor and bus. No host display is used.
#![cfg(target_os = "linux")]

#[test]
fn gnome_capture_is_fail_closed_without_a_helper() {
    if std::env::var_os("CUA_GNOME_CAPTURE_CHILD").is_some() {
        for capture in [
            platform_linux::capture::screenshot_display_bytes,
            platform_linux::wayland::screenshot_display_dispatch,
            platform_linux::wayland::screenshot_bytes,
            platform_linux::wayland::ext_screencopy::screenshot_via_ext_copy,
            platform_linux::wayland::portal_screenshot::screenshot_via_portal,
        ] {
            let error = capture().unwrap_err().to_string();
            assert!(error.starts_with("capture_scope_unsupported:"), "{error}");
        }
        for error in [
            platform_linux::capture::screenshot_root_region_png(0, 0, 128, 128).unwrap_err(),
            platform_linux::capture::screenshot_window_bytes(42).unwrap_err(),
        ] {
            assert!(
                error.to_string().starts_with("capture_scope_unsupported:"),
                "{error}"
            );
        }
        let error = platform_linux::wayland::screenshot_dispatch(42)
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("helper_unavailable:"), "{error}");
        return;
    }
    let home = tempfile::tempdir().unwrap();
    for opt_in in [None, Some(""), Some("0"), Some("false"), Some("1")] {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "gnome_capture_is_fail_closed_without_a_helper",
                "--nocapture",
            ])
            .env_clear()
            .env("HOME", home.path())
            .env("XDG_CONFIG_HOME", home.path())
            .env("XDG_RUNTIME_DIR", home.path())
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path={}/no-bus", home.path().display()),
            )
            .env(
                "DBUS_SYSTEM_BUS_ADDRESS",
                format!("unix:path={}/no-system-bus", home.path().display()),
            )
            .env("XDG_CURRENT_DESKTOP", "ubuntu:GNOME")
            .env("XDG_SESSION_TYPE", "wayland")
            .env("WAYLAND_DISPLAY", "no-compositor")
            .env("CUA_GNOME_CAPTURE_CHILD", "1");
        if let Some(value) = opt_in {
            command.env("CUA_DRIVER_RS_ENABLE_WAYLAND", value);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
