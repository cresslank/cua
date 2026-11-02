#![cfg(target_os = "windows")]

//! Public-MCP owner for ordinary-app launch tokens (#3607). Observe the actual
//! nonce-bound executable, not launch_app's potentially substituted window PID.
//! The real receiver stays alive until release; all destructive cleanup is
//! restricted to its independently image/birth-qualified retained handle.
//!
//! Run on an explicitly selected disposable Windows desktop.
//! Build the real receiver with `cargo build --locked -p cua-driver-testkit
//! --example windows_launch_receiver` before running this ignored test.

use std::path::PathBuf;

use cua_driver_testkit::windows_process::{describe_process, ObservedProcess, TokenFacts};
use cua_driver_testkit::{Driver, McpDriver};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;

#[path = "support/windows_launch_receiver.rs"]
mod windows_launch_receiver;
use windows_launch_receiver::ReceiverLease;

fn receiver_image() -> PathBuf {
    // Resolve next to THIS test artifact, not a release-preferred driver or
    // another checkout. Works with custom Cargo target dirs and target triples.
    let test = std::env::current_exe().expect("current test executable");
    let image = test
        .parent()
        .expect("deps directory")
        .parent()
        .expect("profile directory")
        .join("examples/windows_launch_receiver.exe");
    assert!(image.is_file(), "missing {image:?}; build with cargo build --locked -p cua-driver-testkit --example windows_launch_receiver");
    image
}

fn assert_launch_token(route: &str, daemon: &TokenFacts, target: &TokenFacts) {
    assert_eq!(
        target.user_sid, daemon.user_sid,
        "{route}: launch changed user SID"
    );
    assert_eq!(
        target.authentication_id, daemon.authentication_id,
        "{route}: launch changed logon session"
    );
    assert_eq!(
        target.session_id, daemon.session_id,
        "{route}: launch changed Windows session"
    );
    if daemon.requires_deelevation() {
        assert!(
            target.integrity_rid <= 0x2000,
            "{route}: actual receiver launched from elevated daemon above Medium: {target:?}"
        );
        assert!(target.integrity_rid < daemon.integrity_rid,
            "{route}: receiver integrity is not below elevated daemon: daemon={daemon:?}, receiver={target:?}");
        assert!(
            target.administrators_deny_only_or_absent(),
            "{route}: actual receiver retains allow-capable Administrators: {target:?}"
        );
        // Independent expected security contract, not the product's validator.
        const STANDARD_USER_PRIVILEGES: &[&str] = &[
            "SeChangeNotifyPrivilege",
            "SeShutdownPrivilege",
            "SeUndockPrivilege",
            "SeIncreaseWorkingSetPrivilege",
            "SeTimeZonePrivilege",
        ];
        let extra: Vec<_> = target
            .privileges
            .iter()
            .filter(|name| !STANDARD_USER_PRIVILEGES.contains(&name.as_str()))
            .collect();
        assert!(
            extra.is_empty(),
            "{route}: actual receiver retains nonstandard privileges: {extra:?}"
        );
        // SAFER can inherit TokenElevation from the elevated source. Effective
        // authority is checked above; that flag is NOT a target rejection rule.
    } else {
        assert_eq!(
            target, daemon,
            "{route}: non-elevated daemon changed token security facts"
        );
    }
}

#[test]
#[ignore = "launches an owned process/window on the interactive desktop"]
fn elevated_driver_launches_apps_with_a_standard_user_token() {
    let image = receiver_image();
    let mut driver = McpDriver::spawn_with_env(&[
        ("CUA_DRIVER_PERMISSION_MODE", "unrestricted"),
        ("CUA_DRIVER_DANGEROUSLY_BYPASS_APPROVALS", "1"),
    ])
    .expect("test Driver failed to start");
    // The daemon executes launch_app. Neither this test process nor its stdio
    // proxy is authoritative evidence of the daemon's token posture.
    let daemon_pid = driver.daemon_pid().expect("owned daemon PID");
    let daemon = ObservedProcess::open(daemon_pid).expect("open actual daemon token oracle");
    let daemon_identity = daemon.identity().expect("actual daemon identity");
    let daemon_token = daemon.token().expect("actual daemon token");
    eprintln!("[launch_app_standard_user] actual daemon PID={daemon_pid}, identity={daemon_identity:?}, token={daemon_token:?}");
    if !daemon_token.requires_deelevation() {
        eprintln!("[launch_app_standard_user] non-elevated same-token control only; elevated de-elevation NOT exercised");
    }

    let mut exercised = Vec::new();
    for (route, start_minimized) in [("shell", false), ("direct", true)] {
        let receiver = ReceiverLease::new(&image);
        let launch = driver.call(
            "launch_app",
            serde_json::json!({
                "path": image,
                "additional_arguments": [receiver.address, receiver.nonce],
                "start_minimized": start_minimized,
            }),
        );
        eprintln!(
            "[launch_app_standard_user] {route} full MCP response: {}",
            launch.raw
        );
        if start_minimized && launch.structured()["code"] == "background_unavailable" {
            assert!(
                launch.is_error(),
                "background_unavailable must be a refusal"
            );
            eprintln!("[launch_app_standard_user] direct route NOT exercised (background_unavailable); not token coverage");
            continue;
        }
        assert!(!launch.is_error(), "{route} launch failed: {}", launch.raw);
        let reported_pid = launch.structured()["pid"]
            .as_u64()
            .and_then(|pid| u32::try_from(pid).ok())
            .filter(|pid| *pid != 0)
            .unwrap_or_else(|| panic!("{route}: invalid reported PID: {}", launch.raw));
        // Read-only diagnostics: no termination right, never cleanup authority.
        eprintln!(
            "[launch_app_standard_user] {route} public response {}",
            describe_process(reported_pid)
        );
        let ready = receiver.ready();
        let identity = ready
            .process
            .identity()
            .expect("retained receiver identity");
        let target = ready
            .process
            .token()
            .expect("actual receiver process token");
        assert!(
            ready.process.is_alive(),
            "{route}: receiver expired before oracle"
        );
        eprintln!("[launch_app_standard_user] {route} actual receiver PID={}, identity={identity:?}, token={target:?}", ready.pid);
        assert_launch_token(route, &daemon_token, &target);

        // This fixture has its OWN native top-level window and never spawns a
        // descendant. Public PID attribution is a separate assertion, not a
        // source of token facts. A substituted unrelated High PID fails HERE,
        // without mislabeling it as the receiver or terminating it.
        assert_eq!(
            reported_pid, ready.pid,
            "{route}: public launch identity does not identify nonce-bound receiver"
        );
        let reported = ObservedProcess::open(reported_pid).expect("read-only public PID identity");
        assert_eq!(
            reported.identity().expect("public identity"),
            identity,
            "{route}: public PID incarnation differs from retained receiver"
        );
        if let Some(windows) = launch.structured()["windows"].as_array() {
            for window in windows {
                let hwnd = window["window_id"]
                    .as_u64()
                    .and_then(|id| usize::try_from(id).ok())
                    .expect("reported native window identity");
                let mut owner = 0;
                unsafe { GetWindowThreadProcessId(HWND(hwnd as *mut _), Some(&mut owner)) };
                assert_eq!(
                    owner, ready.pid,
                    "{route}: public window belongs to another process"
                );
                assert_eq!(
                    hwnd, ready.hwnd,
                    "{route}: public window is not owned receiver HWND"
                );
            }
        }
        assert!(daemon.is_alive(), "actual daemon exited during token test");
        assert_eq!(
            daemon.identity().unwrap(),
            daemon_identity,
            "daemon incarnation changed"
        );
        assert!(
            ready.process.is_alive(),
            "{route}: receiver exited before explicit release"
        );
        drop(receiver); // joins custody worker and verifies exact receiver exit
        exercised.push(route);
    }
    eprintln!("[launch_app_standard_user] actual-token routes exercised: {exercised:?}; elevated daemon={}", daemon_token.requires_deelevation());
}
