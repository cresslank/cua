//! Admission tests run in fresh, env-cleared processes: no host display or bus.
#![cfg(target_os = "linux")]

use std::os::unix::fs::PermissionsExt;
use std::process::Command;

use cua_spacesd_desktop::CuaProviderBundle;
use cua_spacesd_provider_api::ProviderErrorCode;

fn run_child(mode: &str, session_bus: Option<String>) {
    let root = std::env::temp_dir().join(format!("spaces-admission-{}-{mode}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap());
    child
        .env_clear()
        .env("HOME", &root)
        .env("XDG_RUNTIME_DIR", &root)
        .env("TMPDIR", &root)
        .env("PATH", "/usr/bin:/bin")
        .env("CUA_DRIVER_RS_TELEMETRY_ENABLED", "false")
        .env("CUA_DRIVER_RS_UPDATE_CHECK", "false")
        .env("SPACES_ADMISSION_TEST_CHILD", mode)
        .args(["--exact", "admission_child", "--nocapture"]);
    if mode != "headless" {
        // A nonexistent private socket requests native admission without ever
        // connecting to a host compositor or capturing/injecting anything.
        child.env("WAYLAND_DISPLAY", root.join("no-compositor"));
    }
    child.env(
        "DBUS_SESSION_BUS_ADDRESS",
        session_bus.unwrap_or_else(|| format!("unix:path={}/missing-bus", root.display())),
    );
    child.env("GSETTINGS_BACKEND", "memory");
    let output = child.output().unwrap();
    std::fs::remove_dir_all(root).unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn admission_child() {
    let Ok(mode) = std::env::var("SPACES_ADMISSION_TEST_CHILD") else {
        return;
    };
    assert!(platform_linux::a11y::trusted_accessibility_bus_address().is_err());
    let result = CuaProviderBundle::new();
    match mode.as_str() {
        "headless" => {
            assert!(result.is_ok());
            assert_eq!(
                platform_linux::a11y::trusted_accessibility_bus_address()
                    .unwrap_err()
                    .to_string(),
                "trusted accessibility bus was not initialized"
            );
        }
        "refused" => {
            let error = result
                .err()
                .expect("unavailable private bus must deny native admission");
            assert_eq!(error.code, ProviderErrorCode::DeliveryFailed);
            assert!(error.message.starts_with(
                "runtime_unavailable: session accessibility preparation failed: could not prepare session accessibility:"), "{}", error.message);
            assert!(platform_linux::a11y::trusted_accessibility_bus_address().is_err());
        }
        "private" => {
            assert!(result.is_ok(), "{}", result.err().unwrap());
            assert!(platform_linux::a11y::trusted_accessibility_bus_address().is_ok());
            platform_linux::atspi::ensure_listener_active().unwrap();
        }
        _ => panic!("unknown test mode"),
    }
}

#[test]
fn headless_construction_does_not_claim_native_accessibility() {
    run_child("headless", None);
}

#[test]
#[ignore = "exercises the canonical per-UID preparation lock; run in the private accessibility harness"]
fn native_construction_propagates_preparation_failure() {
    run_child("refused", None);
}

#[test]
#[ignore = "requires an explicitly owned private session bus and AT-SPI registry; run via with-private-a11y.py"]
fn private_session_initializes_trusted_accessibility() {
    run_child(
        "private",
        Some(
            std::env::var("DBUS_SESSION_BUS_ADDRESS")
                .expect("run only inside the reviewed private accessibility helper"),
        ),
    );
}
