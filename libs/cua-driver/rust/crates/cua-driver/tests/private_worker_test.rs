use cua_driver_sdk::worker::{
    read_private_worker_message, ActionCompletion, ChannelResponse,
    PRIVATE_WORKER_STARTUP_ERROR_REQUEST_ID,
};
use cua_driver_sdk::{
    ConfiguredDriverOptions, DriverError, DriverExecutionMode, EmbeddedDriverHostOptions,
    EmbeddedEnvironmentVariable, EmbeddedPermissionMode, PrivateWorkerOptions,
    RuntimeAuthorizationOptions, SessionPermissionMode, TrustedSessionOptions,
};

#[cfg(target_os = "linux")]
use cua_driver_sdk::worker::{
    encode_private_worker_message, ChannelRequest, PrivateWorkerBinding, WorkerInitialization,
    PRIVATE_WORKER_INITIALIZATION_REQUEST_ID, PRIVATE_WORKER_PROTOCOL_VERSION,
};

fn worker_options() -> PrivateWorkerOptions {
    PrivateWorkerOptions {
        binary_path: env!("CARGO_BIN_EXE_cua-driver").to_owned(),
        host_bundle_id: "com.trycua.private-worker-test".into(),
        startup_timeout_ms: Some(10_000),
        shutdown_timeout_ms: Some(2_000),
        configured_driver: ConfiguredDriverOptions {
            claude_code_compatibility: false,
            authorization: RuntimeAuthorizationOptions {
                allowed_modes: vec![
                    SessionPermissionMode::Standard,
                    SessionPermissionMode::Unrestricted,
                ],
                compatibility_mode: SessionPermissionMode::Standard,
                compatibility_capability_manifest_path: None,
                compatibility_bounded_manifest_path: None,
                unrestricted_acknowledged: true,
                max_session_ttl_seconds: 60,
                max_idle_ttl_seconds: 30,
            },
        },
        environment: Vec::new(),
        inherit_stderr: true,
    }
}

#[test]
fn child_reports_a_structured_error_before_runtime_creation() {
    use std::io::BufReader;
    use std::process::{Command, Stdio};

    let mut child = Command::new(env!("CARGO_BIN_EXE_cua-driver"))
        .args(["__private-worker", "--generation", "", "--host-pid", "1"])
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let line = read_private_worker_message(&mut stdout)
        .unwrap()
        .expect("private worker must report its pre-runtime startup failure");
    let response: ChannelResponse = serde_json::from_str(&line).unwrap();
    assert!(!response.ok);
    assert_eq!(response.request_id, PRIVATE_WORKER_STARTUP_ERROR_REQUEST_ID);
    assert_eq!(response.generation, "");
    assert_eq!(response.error_code.as_deref(), Some("invalid_startup"));
    assert_eq!(response.completion, ActionCompletion::NotStarted);
    assert!(child.wait().unwrap().success());
}

#[test]
fn private_worker_constructor_rejects_an_unbounded_environment() {
    let mut options = worker_options();
    options.startup_timeout_ms = Some(1);
    options.environment = (0..4_097)
        .map(|_| EmbeddedEnvironmentVariable {
            name: "LANG".into(),
            value: "C.UTF-8".into(),
        })
        .collect();

    assert!(matches!(
        cua_driver_sdk::CuaDriver::create_private_worker(options),
        Err(DriverError::Configuration { reason }) if reason.contains("4096 entries")
    ));
}

#[cfg(target_os = "linux")]
#[test]
fn child_reports_a_structured_error_when_the_attested_atspi_route_is_missing() {
    use std::io::{BufReader, Write as _};
    use std::process::{Command, Stdio};

    let generation = "structured-startup-error";
    let host_pid = std::process::id().to_string();
    let mut child = Command::new(env!("CARGO_BIN_EXE_cua-driver"))
        .args([
            "__private-worker",
            "--generation",
            generation,
            "--host-pid",
            &host_pid,
        ])
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let binding = PrivateWorkerBinding {
        protocol_version: PRIVATE_WORKER_PROTOCOL_VERSION,
        generation: generation.into(),
        nonce: "structured-startup-error-binding".into(),
        worker_pid: child.id(),
    };
    let request = ChannelRequest {
        protocol_version: PRIVATE_WORKER_PROTOCOL_VERSION,
        request_id: PRIVATE_WORKER_INITIALIZATION_REQUEST_ID,
        generation: generation.into(),
        operation: "initialize".into(),
        name: None,
        arguments: Some(
            serde_json::to_value(WorkerInitialization {
                configured_driver: worker_options().configured_driver,
                host_bundle_id: "com.trycua.structured-startup-error".into(),
                environment_attestation: Vec::new(),
            })
            .unwrap(),
        ),
        session_handle: None,
    };
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    stdin
        .write_all(&encode_private_worker_message(&binding).unwrap())
        .unwrap();
    stdin.write_all(b"\n").unwrap();
    stdin.flush().unwrap();
    let binding_line = read_private_worker_message(&mut stdout)
        .unwrap()
        .expect("private worker must acknowledge the process binding");
    let acknowledged: PrivateWorkerBinding = serde_json::from_str(&binding_line).unwrap();
    assert_eq!(acknowledged, binding);
    stdin
        .write_all(&encode_private_worker_message(&request).unwrap())
        .unwrap();
    stdin.write_all(b"\n").unwrap();
    stdin.flush().unwrap();

    let line = read_private_worker_message(&mut stdout)
        .unwrap()
        .expect("private worker must report its startup failure");
    let response: ChannelResponse = serde_json::from_str(&line).unwrap();
    assert!(!response.ok);
    assert_eq!(response.request_id, PRIVATE_WORKER_STARTUP_ERROR_REQUEST_ID);
    assert_eq!(response.generation, generation);
    assert_eq!(
        response.error_code.as_deref(),
        Some("runtime_initialization_failed")
    );
    assert_eq!(response.completion, ActionCompletion::NotStarted);
    assert!(response
        .error
        .as_deref()
        .is_some_and(|error| error.contains("missing AT_SPI_BUS_ADDRESS")));
    assert!(child.wait().unwrap().success());
}

#[tokio::test]
async fn private_worker_owns_one_runtime_without_a_reconnect_endpoint() {
    let driver = cua_driver_sdk::CuaDriver::create_private_worker(worker_options()).unwrap();
    assert_eq!(driver.execution_mode(), DriverExecutionMode::PrivateWorker);
    assert!(driver.socket_path().is_empty());
    assert!(driver.is_available());

    let metadata = driver.metadata().await.unwrap();
    assert_ne!(metadata.pid, std::process::id());
    let tools: serde_json::Value =
        serde_json::from_str(&driver.list_tools_json().await.unwrap()).unwrap();
    assert!(tools["tools"]
        .as_array()
        .is_some_and(|tools| !tools.is_empty()));

    let session = driver
        .create_trusted_session(TrustedSessionOptions {
            public_session: "worker-trusted".into(),
            mode: SessionPermissionMode::Standard,
            ttl_seconds: 60,
            idle_ttl_seconds: 30,
            capability_manifest_path: None,
            bounded_manifest_path: None,
        })
        .unwrap();
    let healthy = session
        .call_tool("health_report".into(), "{}".into())
        .await
        .unwrap();
    assert_ne!(healthy.error_code.as_deref(), Some("permission_denied"));

    let substituted = session
        .call_tool(
            "health_report".into(),
            serde_json::json!({"session": "other"}).to_string(),
        )
        .await
        .unwrap();
    assert_eq!(substituted.error_code.as_deref(), Some("permission_denied"));

    session.close();
    driver.shutdown().await.unwrap();
    assert!(!driver.is_available());
}

#[cfg(target_os = "linux")]
#[test]
fn private_worker_constructor_does_not_mutate_session_bus_environment() {
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "private_worker_session_bus_environment_probe",
            "--nocapture",
        ])
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .env_remove("CUA_DRIVER_PRIVATE_AT_SPI_BUS_ADDRESS")
        .status()
        .unwrap();
    assert!(
        status.success(),
        "the public private-worker constructor must not mutate process-global session-bus state"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "subprocess-only process-environment isolation probe"]
async fn private_worker_session_bus_environment_probe() {
    assert!(std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none());
    let driver = cua_driver_sdk::CuaDriver::create_private_worker(worker_options()).unwrap();
    assert!(std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none());
    driver.shutdown().await.unwrap();
}

#[tokio::test]
async fn concurrent_private_workers_initialize_independently() {
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let spawn = |barrier: std::sync::Arc<std::sync::Barrier>| {
        std::thread::spawn(move || {
            barrier.wait();
            cua_driver_sdk::CuaDriver::create_private_worker(worker_options())
        })
    };
    let first = spawn(barrier.clone());
    let second = spawn(barrier.clone());
    barrier.wait();

    let first = first.join().unwrap().unwrap();
    let second = second.join().unwrap().unwrap();
    let (first_metadata, second_metadata) = tokio::join!(first.metadata(), second.metadata());
    assert_ne!(first_metadata.unwrap().pid, second_metadata.unwrap().pid);
    let (first_shutdown, second_shutdown) = tokio::join!(first.shutdown(), second.shutdown());
    first_shutdown.unwrap();
    second_shutdown.unwrap();
}

#[tokio::test]
async fn private_worker_attests_the_post_policy_child_environment() {
    let mut options = worker_options();
    options.environment = vec![EmbeddedEnvironmentVariable {
        name: "LANG".into(),
        value: "C.UTF-8".into(),
    }];
    let driver = cua_driver_sdk::CuaDriver::create_private_worker(options).unwrap();
    assert!(driver.is_available());
    driver.shutdown().await.unwrap();

    // Generic embedded callers may request these conventional names, but the
    // private-worker merge ignores them in favor of trusted inherited routing.
    // Successful readiness proves the resulting env-cleared child received the
    // post-policy values attested by the parent/child handshake.
    for ignored_private_override in ["HOME", "home"] {
        let mut options = worker_options();
        options.environment = vec![EmbeddedEnvironmentVariable {
            name: ignored_private_override.into(),
            value: "/tmp/forged-private-worker-route".into(),
        }];
        let driver = cua_driver_sdk::CuaDriver::create_private_worker(options).unwrap();
        driver.shutdown().await.unwrap();
    }

    for reserved_name in [
        "AT_SPI_BUS_ADDRESS",
        "CUA_DRIVER_PRIVATE_AT_SPI_BUS_ADDRESS",
        "CUA_DRIVER_RS_SESSION_IDLE_TTL_SECS",
    ] {
        let mut options = worker_options();
        options.environment = vec![EmbeddedEnvironmentVariable {
            name: reserved_name.into(),
            value: "/tmp/forged-private-worker-route".into(),
        }];
        assert!(cua_driver_sdk::CuaDriver::create_private_worker(options).is_err());
    }
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn private_worker_owns_the_macos_cursor_overlay_facility() {
    let driver = cua_driver_sdk::CuaDriver::create_private_worker(worker_options()).unwrap();
    let result = driver
        .call_tool(
            "get_agent_cursor_state".into(),
            serde_json::json!({"session": "worker-overlay"}).to_string(),
        )
        .await
        .unwrap();
    assert_ne!(
        result.error_code.as_deref(),
        Some("facility_unavailable"),
        "private worker did not install its AppKit main-thread adapter"
    );
    let permissions = driver
        .call_tool(
            "check_permissions".into(),
            serde_json::json!({"prompt": true}).to_string(),
        )
        .await
        .unwrap();
    let structured: serde_json::Value =
        serde_json::from_str(permissions.structured_json.as_deref().unwrap()).unwrap();
    assert_eq!(structured["direct_capture_status"], "not_checked");
    assert_eq!(structured["source"]["attribution"], "host");
    assert_eq!(
        structured["source"]["host_bundle_id"],
        "com.trycua.private-worker-test"
    );
    driver.shutdown().await.unwrap();
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn private_worker_inherits_the_interactive_linux_display_scope() {
    if std::env::var("CUA_REQUIRE_GUI").as_deref() != Ok("1") {
        return;
    }
    let has_x11 = std::env::var("DISPLAY").is_ok_and(|display| !display.is_empty());
    let has_wayland = std::env::var("WAYLAND_DISPLAY").is_ok_and(|display| !display.is_empty());
    assert!(
        has_x11 || has_wayland,
        "canonical GUI E2E requires DISPLAY or WAYLAND_DISPLAY"
    );

    let driver = cua_driver_sdk::CuaDriver::create_private_worker(worker_options()).unwrap();
    let standard = driver
        .create_trusted_session(TrustedSessionOptions {
            public_session: "worker-standard-display-scope".into(),
            mode: SessionPermissionMode::Standard,
            ttl_seconds: 60,
            idle_ttl_seconds: 30,
            capability_manifest_path: None,
            bounded_manifest_path: None,
        })
        .unwrap();
    let observed = standard
        .call_tool("list_windows".into(), "{}".into())
        .await
        .unwrap();
    assert!(
        !observed.is_error,
        "routine standard-mode observation must not require an authorization host: {}",
        observed.text
    );
    standard.close();

    let session = driver
        .create_trusted_session(TrustedSessionOptions {
            public_session: "worker-display-scope".into(),
            mode: SessionPermissionMode::Unrestricted,
            ttl_seconds: 60,
            idle_ttl_seconds: 30,
            capability_manifest_path: None,
            bounded_manifest_path: None,
        })
        .unwrap();
    let started = session
        .call_tool(
            "start_session".into(),
            serde_json::json!({
                "session": "worker-display-scope",
                "capture_scope": "desktop"
            })
            .to_string(),
        )
        .await
        .unwrap();
    assert!(
        !started.is_error,
        "worker could not declare the trusted desktop capture scope: {}",
        started.text
    );
    let started_structured: serde_json::Value = serde_json::from_str(
        started
            .structured_json
            .as_deref()
            .expect("start_session omitted structuredContent"),
    )
    .unwrap();
    assert_eq!(started_structured["capture_scope"], "desktop");
    assert_eq!(started_structured["effective_scope"], "desktop");
    if has_x11 {
        let desktop = session
            .call_tool("get_desktop_state".into(), "{}".into())
            .await
            .unwrap();
        assert!(
            !desktop.images.is_empty(),
            "worker inherited no usable X11 capture scope: {}",
            desktop.text
        );
    } else {
        let windows = session
            .call_tool("list_windows".into(), "{}".into())
            .await
            .unwrap();
        assert!(
            windows.error_code.is_none(),
            "worker inherited no usable native Wayland session scope: {}",
            windows.text
        );
    }
    let ended = session
        .call_tool(
            "end_session".into(),
            serde_json::json!({"session": "worker-display-scope"}).to_string(),
        )
        .await
        .unwrap();
    assert!(
        !ended.is_error,
        "worker could not end session: {}",
        ended.text
    );
    session.close();
    driver.shutdown().await.unwrap();
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn private_worker_descriptor_boundary_helper() {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    if std::env::var_os("CUA_DRIVER_TEST_DESCRIPTOR_BOUNDARY_HELPER").is_none() {
        return;
    }
    let mut descriptors = [-1; 2];
    // Deliberately omit O_CLOEXEC: the worker spawn boundary must seal this
    // ambient writer rather than relying on well-behaved host descriptors.
    assert_eq!(unsafe { libc::pipe(descriptors.as_mut_ptr()) }, 0);
    // SAFETY: pipe returned two fresh descriptors, transferred exactly once.
    let read_end = unsafe { OwnedFd::from_raw_fd(descriptors[0]) };
    let write_end = unsafe { OwnedFd::from_raw_fd(descriptors[1]) };
    let high_descriptor = unsafe { libc::fcntl(write_end.as_raw_fd(), libc::F_DUPFD, 256) };
    assert!(
        high_descriptor >= 256,
        "could not create a high inheritable descriptor: {}",
        std::io::Error::last_os_error()
    );
    let high_write_end = unsafe { OwnedFd::from_raw_fd(high_descriptor) };
    drop(write_end);

    let mut original_limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut original_limit) },
        0
    );
    assert!(original_limit.rlim_max > high_descriptor as libc::rlim_t);
    let lowered_limit = libc::rlimit {
        rlim_cur: high_descriptor as libc::rlim_t,
        rlim_max: original_limit.rlim_max,
    };
    assert_eq!(
        unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lowered_limit) },
        0
    );

    let driver = cua_driver_sdk::CuaDriver::create_private_worker(worker_options()).unwrap();
    assert_eq!(
        unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &original_limit) },
        0
    );
    drop(high_write_end);
    let flags = unsafe { libc::fcntl(read_end.as_raw_fd(), libc::F_GETFL) };
    assert!(flags >= 0);
    assert_eq!(
        unsafe {
            libc::fcntl(
                read_end.as_raw_fd(),
                libc::F_SETFL,
                flags | libc::O_NONBLOCK,
            )
        },
        0
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    loop {
        let mut byte = 0_u8;
        let result = unsafe { libc::read(read_end.as_raw_fd(), (&mut byte as *mut u8).cast(), 1) };
        if result == 0 {
            break;
        }
        let error = std::io::Error::last_os_error();
        assert!(
            result < 0 && error.kind() == std::io::ErrorKind::WouldBlock,
            "unexpected ambient-descriptor probe result: {result}, {error}"
        );
        assert!(
            std::time::Instant::now() < deadline,
            "private worker retained an ambient host descriptor across exec"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(driver.is_available());
    driver.shutdown().await.unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn private_worker_closes_ambient_host_descriptors() {
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "private_worker_descriptor_boundary_helper",
            "--nocapture",
        ])
        .env("CUA_DRIVER_TEST_DESCRIPTOR_BOUNDARY_HELPER", "1")
        .status()
        .unwrap();
    assert!(status.success());
}

#[tokio::test]
async fn dropping_the_host_closes_and_terminates_the_private_worker() {
    let driver = cua_driver_sdk::CuaDriver::create_private_worker(worker_options()).unwrap();
    let pid = driver.metadata().await.unwrap().pid;
    drop(driver);

    #[cfg(unix)]
    {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while unsafe { libc::kill(pid as i32, 0) } == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_ne!(
            unsafe { libc::kill(pid as i32, 0) },
            0,
            "private worker must not outlive its owning SDK object"
        );
    }
}

#[tokio::test]
async fn embedded_service_binds_authority_to_the_original_host_connection() {
    let host = cua_driver_sdk::EmbeddedCuaDriverHost::with_options(EmbeddedDriverHostOptions {
        binary_path: env!("CARGO_BIN_EXE_cua-driver").to_owned(),
        host_bundle_id: "com.trycua.trusted-service-test".into(),
        socket_path: None,
        startup_timeout_ms: Some(10_000),
        shutdown_timeout_ms: Some(2_000),
        permission_mode: Some(EmbeddedPermissionMode::Standard),
        capability_manifest_path: None,
        approve_capability_manifest: false,
        session_policy_path: None,
        approve_session_policy: false,
        dangerously_bypass_approvals: false,
        environment: Vec::<EmbeddedEnvironmentVariable>::new(),
        inherit_stderr: true,
    })
    .unwrap();
    let connection = host.clone().start().await.unwrap();
    let driver = cua_driver_sdk::CuaDriver::connect(Some(connection.socket_path));
    let driver = driver.unwrap();
    let session = driver
        .create_trusted_session(TrustedSessionOptions {
            public_session: "service-trusted".into(),
            mode: SessionPermissionMode::Standard,
            ttl_seconds: 60,
            idle_ttl_seconds: 30,
            capability_manifest_path: None,
            bounded_manifest_path: None,
        })
        .unwrap();
    let result = session
        .call_tool("health_report".into(), "{}".into())
        .await
        .unwrap();
    assert_ne!(result.error_code.as_deref(), Some("permission_denied"));

    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "service_untrusted_child_probe",
            "--nocapture",
        ])
        .env(
            "CUA_DRIVER_TEST_TRUSTED_SERVICE_SOCKET",
            driver.socket_path(),
        )
        .status()
        .unwrap();
    assert!(
        child.success(),
        "a non-parent same-user process must be refused before delegated authority is created"
    );

    session.close();
    host.stop().await.unwrap();
}

#[test]
#[ignore = "subprocess-only trusted-service authentication probe"]
fn service_untrusted_child_probe() {
    let socket = std::env::var("CUA_DRIVER_TEST_TRUSTED_SERVICE_SOCKET").unwrap();
    let driver = cua_driver_sdk::CuaDriver::connect(Some(socket)).unwrap();
    let result = driver.create_trusted_session(TrustedSessionOptions {
        public_session: "forged-service-session".into(),
        mode: SessionPermissionMode::Standard,
        ttl_seconds: 60,
        idle_ttl_seconds: 30,
        capability_manifest_path: None,
        bounded_manifest_path: None,
    });
    assert!(
        result.is_err(),
        "same-user reachability without embedded-host process identity is not authority"
    );
}
