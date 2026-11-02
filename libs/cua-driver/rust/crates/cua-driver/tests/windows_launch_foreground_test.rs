#![cfg(target_os = "windows")]

//! Public-MCP owner for app-identifying desktop foreground preservation,
//! including a separately launched URL handler. Run only on a disposable native
//! Windows desktop; build the unshipped windows_launch_receiver example first.

use cua_driver_testkit::{Driver, McpDriver};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;

#[path = "support/windows_foreground_fixture.rs"]
mod windows_foreground_fixture;
#[path = "support/windows_foreground_policy.rs"]
mod windows_foreground_policy;
#[path = "support/windows_launch_receiver.rs"]
mod windows_launch_receiver;
#[path = "support/windows_url_protocol.rs"]
mod windows_url_protocol;

use windows_foreground_fixture::ForegroundFixture;
use windows_launch_receiver::ReceiverLease;
use windows_url_protocol::UrlProtocol;

#[test]
#[ignore = "launches owned desktop windows and temporarily registers a unique HKCU URL scheme"]
fn app_and_secondary_url_handler_preserve_exact_prior_foreground() {
    // The executable must be from THIS build, including custom target dirs.
    let image = std::env::current_exe()
        .expect("test executable")
        .parent()
        .expect("deps directory")
        .parent()
        .expect("profile directory")
        .join("examples/windows_launch_receiver.exe");
    assert!(image.is_file(), "build cargo build --locked -p cua-driver-testkit --example windows_launch_receiver first: {image:?}");

    // Keep a responsive, exact prior HWND/PID alive. Only this test daemon gets
    // foreground permission; no synthetic input or ASFW_ANY/global unlock.
    let foreground = ForegroundFixture::new();
    let mut driver = McpDriver::spawn_with_env(&[
        ("CUA_DRIVER_PERMISSION_MODE", "unrestricted"),
        ("CUA_DRIVER_DANGEROUSLY_BYPASS_APPROVALS", "1"),
    ])
    .expect("test Driver failed to start");

    // Both listeners/custodians precede the single public call. The handler's
    // UUID scheme is per-user and freshly created, never a browser default.
    let primary = ReceiverLease::new(&image);
    let secondary = ReceiverLease::new(&image);
    let protocol = UrlProtocol::new(&image, &secondary);
    foreground.grant_to(driver.daemon_pid().expect("owned daemon PID"));
    let launch = driver.call(
        "launch_app",
        serde_json::json!({
            "path": image,
            "additional_arguments": [primary.address, primary.nonce],
            "urls": [protocol.url],
            // This is the formerly unprotected route, NOT minimized launch.
            "start_minimized": false,
        }),
    );
    eprintln!("[launch_app_foreground] full MCP response: {}", launch.raw);
    assert!(!launch.is_error(), "desktop launch failed: {}", launch.raw);
    let reported_pid = launch.structured()["pid"]
        .as_u64()
        .and_then(|pid| u32::try_from(pid).ok())
        .filter(|pid| *pid != 0)
        .expect("nonzero public launch PID");
    let primary_ready = primary.ready();
    let secondary_ready = secondary.ready();
    assert_eq!(
        reported_pid, primary_ready.pid,
        "public primary receiver identity"
    );
    assert_ne!(
        primary_ready.pid, secondary_ready.pid,
        "separate URL process"
    );
    assert!(primary_ready.process.is_alive() && secondary_ready.process.is_alive());
    assert!(
        primary_ready.activation.is_none(),
        "primary must not activate"
    );
    let attempt = secondary_ready.activation.as_ref().expect(
        "registered URL handler must execute its explicit native SetForegroundWindow attempt",
    );
    assert_ne!(
        attempt.foreground_hwnd, 0,
        "native activation readback HWND"
    );
    assert_ne!(attempt.foreground_pid, 0, "native activation readback PID");
    eprintln!(
        "[launch_app_foreground] primary PID={} HWND={:#x} identity={:?}; secondary PID={} HWND={:#x} identity={:?}; native activation={attempt:?}",
        primary_ready.pid, primary_ready.hwnd, primary_ready.process.identity().unwrap(),
        secondary_ready.pid, secondary_ready.hwnd, secondary_ready.process.identity().unwrap(),
    );
    // Under the deliberately broken old lock condition this proves a REAL
    // successful activation before the exact-prior oracle fails, not a missing
    // association, command-quoting error, refusal or zero-selected test.
    if attempt.accepted {
        assert_eq!(
            attempt.foreground_hwnd, secondary_ready.hwnd,
            "accepted native activation HWND"
        );
        assert_eq!(
            attempt.foreground_pid, secondary_ready.pid,
            "accepted native activation PID"
        );
    }
    let windows = launch.structured()["windows"]
        .as_array()
        .expect("public native windows");
    assert!(!windows.is_empty(), "primary native window must resolve");
    for window in windows {
        let hwnd = window["window_id"].as_u64().expect("native window ID") as usize;
        let mut owner = 0;
        unsafe { GetWindowThreadProcessId(HWND(hwnd as *mut _), Some(&mut owner)) };
        assert_eq!(hwnd, primary_ready.hwnd, "reported primary HWND");
        assert_eq!(owner, primary_ready.pid, "reported primary native owner");
    }

    // No reactivation: the native result, not a success envelope or token facts,
    // owns this contract. The existing Edge gate keeps its original oracle too.
    foreground.assert_foreground();
    assert!(primary_ready.process.is_alive() && secondary_ready.process.is_alive());
    drop(protocol); // native deletion + absence readback, on unwind as well
    drop(secondary); // only authenticated retained handles can clean receivers
    drop(primary);
}
