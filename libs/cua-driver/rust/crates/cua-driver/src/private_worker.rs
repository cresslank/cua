//! Hidden process-isolated SDK runtime entry point.
//!
//! This is intentionally not a CLI product surface. A trusted SDK host starts
//! it directly and owns the inherited stdin/stdout channel for the lifetime of
//! one runtime generation.

#[cfg(unix)]
use cua_driver_sdk::worker::PrivateWorkerBinding;
use cua_driver_sdk::worker::{
    encode_private_worker_message, read_private_worker_message, ActionCompletion, ChannelRequest,
    ChannelResponse, WorkerEnvironmentVariable, WorkerInitialization,
    PRIVATE_WORKER_INITIALIZATION_REQUEST_ID, PRIVATE_WORKER_MAX_MESSAGE_BYTES,
    PRIVATE_WORKER_PROTOCOL_VERSION, PRIVATE_WORKER_STARTUP_ERROR_REQUEST_ID,
};
use cua_driver_sdk::{CuaDriver, CuaDriverSession, DriverHostOptions};
use serde_json::Value;
use std::collections::HashMap;
#[cfg(unix)]
use std::io;
use std::io::Write;
use std::sync::Arc;

struct InitializationSignal(Option<std::sync::mpsc::SyncSender<bool>>);

impl InitializationSignal {
    fn ready(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(true);
        }
    }
}

impl Drop for InitializationSignal {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(false);
        }
    }
}

pub fn requested_generation() -> Option<String> {
    let args = std::env::args().collect::<Vec<_>>();
    if args.get(1).map(String::as_str) != Some("__private-worker") {
        return None;
    }
    let Some(generation) = args.get(3) else {
        return Some(String::new());
    };
    let valid_base = args.get(2).map(String::as_str) == Some("--generation")
        && args.get(4).map(String::as_str) == Some("--host-pid")
        && args
            .get(5)
            .is_some_and(|host_pid| host_pid.parse::<u32>().is_ok_and(|pid| pid > 1))
        && generation.len() <= 128
        && !generation.is_empty()
        && generation
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-');
    #[cfg(target_os = "macos")]
    let valid_platform = args.len() == 8
        && args.get(6).map(String::as_str) == Some("--termination-socket")
        && args
            .get(7)
            .is_some_and(|path| !path.is_empty() && path.len() <= 103);
    #[cfg(not(target_os = "macos"))]
    let valid_platform = args.len() == 6;

    if valid_base && valid_platform {
        Some(generation.clone())
    } else {
        Some(String::new())
    }
}

#[cfg(unix)]
fn prove_private_channel_binding(generation: &str) -> Result<String, String> {
    let mut stdin = io::stdin().lock();
    let Some(line) = read_private_worker_message(&mut stdin)
        .map_err(|error| format!("read private-worker process binding: {error}"))?
    else {
        return Err("private-worker process binding channel closed".into());
    };
    let binding: PrivateWorkerBinding = serde_json::from_str(&line)
        .map_err(|error| format!("decode private-worker process binding: {error}"))?;
    if binding.protocol_version != PRIVATE_WORKER_PROTOCOL_VERSION
        || binding.generation != generation
        || binding.nonce.is_empty()
        || binding.nonce.len() > 128
        || binding.worker_pid != std::process::id()
    {
        return Err("private-worker process binding challenge is invalid".into());
    }
    let response = encode_private_worker_message(&binding)
        .map_err(|error| format!("encode private-worker process binding: {error}"))?;
    let mut stdout = io::stdout().lock();
    stdout
        .write_all(&response)
        .and_then(|()| stdout.write_all(b"\n"))
        .and_then(|()| stdout.flush())
        .map_err(|error| format!("write private-worker process binding: {error}"))?;
    Ok(binding.nonce)
}

#[cfg(not(unix))]
fn prove_private_channel_binding(_generation: &str) -> Result<String, String> {
    Ok(String::new())
}

#[cfg(target_os = "macos")]
fn arm_parent_watchdog(binding_nonce: &str) -> anyhow::Result<()> {
    let args = std::env::args().collect::<Vec<_>>();
    let expected = args
        .windows(2)
        .find(|pair| pair[0] == "--host-pid")
        .and_then(|pair| pair[1].parse::<libc::pid_t>().ok())
        .filter(|pid| *pid > 1)
        .ok_or_else(|| anyhow::anyhow!("private worker requires a valid host PID"))?;
    let termination_socket = args
        .windows(2)
        .find(|pair| pair[0] == "--termination-socket")
        .map(|pair| pair[1].clone())
        .ok_or_else(|| anyhow::anyhow!("private worker requires a termination capability"))?;

    if unsafe { libc::getppid() } != expected {
        anyhow::bail!("private-worker parent changed before watchdog startup");
    }
    let mut termination_stream = std::os::unix::net::UnixStream::connect(termination_socket)?;
    termination_stream.write_all(binding_nonce.as_bytes())?;
    termination_stream.write_all(b"\n")?;
    termination_stream.flush()?;
    std::thread::Builder::new()
        .name("cua-private-worker-termination-watchdog".into())
        .spawn(move || {
            let mut byte = 0_u8;
            loop {
                match std::io::Read::read(&mut termination_stream, std::slice::from_mut(&mut byte))
                {
                    Ok(1) => continue,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    _ => {
                        // EOF or an unusable capability revokes worker authority
                        // even if the runtime is blocked in native work.
                        unsafe { libc::_exit(137) }
                    }
                }
            }
        })?;
    std::thread::Builder::new()
        .name("cua-private-worker-parent-watchdog".into())
        .spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_millis(100));
            if unsafe { libc::getppid() } != expected {
                // Terminate even while the runtime is blocked in native work; after
                // authority loss it is unsafe to wait for an async cleanup point.
                unsafe { libc::_exit(137) }
            }
        })?;
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn arm_parent_watchdog(_binding_nonce: &str) -> anyhow::Result<()> {
    Ok(())
}

fn environment_attestation_matches(expected: &[WorkerEnvironmentVariable]) -> bool {
    let mut expected_by_name = HashMap::new();
    for variable in expected {
        let normalized = variable.name.to_ascii_uppercase();
        if expected_by_name
            .insert(normalized, variable.value.clone())
            .is_some()
        {
            return false;
        }
    }

    let mut actual_by_name = HashMap::new();
    for (name, value) in std::env::vars_os() {
        let (Some(name), Some(value)) = (name.to_str(), value.to_str()) else {
            return false;
        };
        if actual_by_name
            .insert(name.to_ascii_uppercase(), value.to_owned())
            .is_some()
        {
            return false;
        }
    }
    actual_by_name == expected_by_name
}

#[cfg(unix)]
fn mark_control_pipe_close_on_exec(descriptor: libc::c_int) -> anyhow::Result<()> {
    let flags = loop {
        // SAFETY: the descriptor is process-owned and fcntl retains no pointer.
        let result = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
        if result >= 0 {
            break result;
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error.into());
        }
    };
    loop {
        // SAFETY: the descriptor is process-owned and fcntl retains no pointer.
        let result = unsafe { libc::fcntl(descriptor, libc::F_SETFD, flags | libc::FD_CLOEXEC) };
        if result >= 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error.into());
        }
    }
}

#[cfg(unix)]
fn seal_control_pipes_against_exec_descendants() -> anyhow::Result<()> {
    for descriptor in [libc::STDIN_FILENO, libc::STDOUT_FILENO] {
        mark_control_pipe_close_on_exec(descriptor)?;
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn seal_control_pipes_against_exec_descendants() -> anyhow::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::{SetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT};

    for handle in [
        HANDLE(std::io::stdin().as_raw_handle()),
        HANDLE(std::io::stdout().as_raw_handle()),
    ] {
        // SAFETY: stdin/stdout are live process control handles. Clearing their
        // inherit bits prevents worker-spawned descendants retaining the channel.
        unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT.0, Default::default()) }?;
    }
    Ok(())
}

#[cfg(not(any(unix, target_os = "windows")))]
fn seal_control_pipes_against_exec_descendants() -> anyhow::Result<()> {
    Ok(())
}

pub(crate) fn write_startup_error(
    generation: &str,
    error_code: &str,
    error: impl Into<String>,
) -> anyhow::Result<()> {
    let stdout = std::io::stdout();
    let mut writer = stdout.lock();
    write_response(
        &mut writer,
        &ChannelResponse::error(
            PRIVATE_WORKER_STARTUP_ERROR_REQUEST_ID,
            generation,
            error_code,
            error.into(),
            ActionCompletion::NotStarted,
        ),
    )
}

pub fn run(
    generation: String,
    initialized: Option<std::sync::mpsc::SyncSender<bool>>,
) -> anyhow::Result<()> {
    let initialized = InitializationSignal(initialized);
    if generation.is_empty() {
        write_startup_error(
            &generation,
            "invalid_startup",
            "private worker requires one valid --generation value",
        )?;
        return Ok(());
    }
    if let Err(error) = seal_control_pipes_against_exec_descendants() {
        write_startup_error(
            &generation,
            "worker_setup_failed",
            format!("seal private-worker control pipes: {error}"),
        )?;
        return Ok(());
    }
    let binding_nonce = match prove_private_channel_binding(&generation) {
        Ok(nonce) => nonce,
        Err(error) => {
            write_startup_error(&generation, "worker_setup_failed", error)?;
            return Ok(());
        }
    };
    if let Err(error) = arm_parent_watchdog(&binding_nonce) {
        write_startup_error(
            &generation,
            "worker_setup_failed",
            format!("arm private-worker parent watchdog: {error}"),
        )?;
        return Ok(());
    }
    // A supervised worker owns one serialized control channel. Letting every
    // worker inherit Tokio's host-CPU-sized default creates N×CPU scheduler
    // threads and can stall concurrent worker initialization under CPU quotas.
    // Keep the scheduler small, but retain Tokio's 512-thread blocking ceiling.
    // Some native AX/UI calls are uncancellable after timeout; lowering that
    // ceiling makes sequential timeouts permanently starve later operations.
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(512)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            write_startup_error(
                &generation,
                "worker_setup_failed",
                format!("create private-worker async runtime: {error}"),
            )?;
            return Ok(());
        }
    };
    runtime.block_on(run_async(generation, initialized))
}

async fn run_async(
    generation: String,
    mut initialized: InitializationSignal,
) -> anyhow::Result<()> {
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let stdout = std::io::stdout();
    let mut writer = stdout.lock();

    let first_line = match read_private_worker_message(&mut reader) {
        Ok(Some(line)) => line,
        Ok(None) => return Ok(()),
        Err(error) => {
            write_response(
                &mut writer,
                &ChannelResponse::error(
                    PRIVATE_WORKER_STARTUP_ERROR_REQUEST_ID,
                    &generation,
                    "invalid_initialization",
                    format!("read private worker initialization: {error}"),
                    ActionCompletion::NotStarted,
                ),
            )?;
            return Ok(());
        }
    };
    let initialization_request: ChannelRequest = match serde_json::from_str(&first_line) {
        Ok(request) => request,
        Err(error) => {
            write_response(
                &mut writer,
                &ChannelResponse::error(
                    PRIVATE_WORKER_STARTUP_ERROR_REQUEST_ID,
                    &generation,
                    "invalid_initialization",
                    format!("parse private worker initialization: {error}"),
                    ActionCompletion::NotStarted,
                ),
            )?;
            return Ok(());
        }
    };
    if initialization_request.protocol_version != PRIVATE_WORKER_PROTOCOL_VERSION
        || initialization_request.request_id != PRIVATE_WORKER_INITIALIZATION_REQUEST_ID
        || initialization_request.generation != generation
        || initialization_request.operation != "initialize"
    {
        write_response(
            &mut writer,
            &ChannelResponse::error(
                PRIVATE_WORKER_STARTUP_ERROR_REQUEST_ID,
                &generation,
                "invalid_initialization",
                "private worker initialization identity mismatch",
                ActionCompletion::NotStarted,
            ),
        )?;
        return Ok(());
    }
    let initialization: WorkerInitialization = match initialization_request
        .arguments
        .ok_or_else(|| anyhow::anyhow!("private worker initialization omitted options"))
        .and_then(|arguments| serde_json::from_value(arguments).map_err(Into::into))
    {
        Ok(initialization) => initialization,
        Err(error) => {
            write_response(
                &mut writer,
                &ChannelResponse::error(
                    PRIVATE_WORKER_STARTUP_ERROR_REQUEST_ID,
                    &generation,
                    "invalid_initialization",
                    error.to_string(),
                    ActionCompletion::NotStarted,
                ),
            )?;
            return Ok(());
        }
    };
    if initialization.host_bundle_id.trim().is_empty() {
        write_response(
            &mut writer,
            &ChannelResponse::error(
                PRIVATE_WORKER_STARTUP_ERROR_REQUEST_ID,
                &generation,
                "invalid_initialization",
                "private worker host identity is empty",
                ActionCompletion::NotStarted,
            ),
        )?;
        return Ok(());
    }
    if !environment_attestation_matches(&initialization.environment_attestation) {
        write_response(
            &mut writer,
            &ChannelResponse::error(
                PRIVATE_WORKER_STARTUP_ERROR_REQUEST_ID,
                &generation,
                "environment_attestation_failed",
                "private worker environment did not match the trusted post-policy launch scope",
                ActionCompletion::NotStarted,
            ),
        )?;
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    {
        let private_bus = std::env::var("AT_SPI_BUS_ADDRESS")
            .map_err(|_| anyhow::anyhow!("attested private worker is missing AT_SPI_BUS_ADDRESS"))
            .and_then(|address| {
                platform_linux::a11y::initialize_private_accessibility_bus(&address)
            });
        if let Err(error) = private_bus {
            write_response(
                &mut writer,
                &ChannelResponse::error(
                    PRIVATE_WORKER_STARTUP_ERROR_REQUEST_ID,
                    &generation,
                    "runtime_initialization_failed",
                    error.to_string(),
                    ActionCompletion::NotStarted,
                ),
            )?;
            return Ok(());
        }
    }
    let driver = match CuaDriver::try_create_configured_for_host(
        initialization.configured_driver,
        DriverHostOptions {
            cursor: cursor_overlay::CursorConfig::default(),
            host_owns_permission_ux: true,
            host_bundle_id: Some(initialization.host_bundle_id.clone()),
            claude_code_compatibility: false,
            // Child initialization must establish the complete Linux desktop
            // contract before reporting ready. Cross-process serialization and
            // bounded listener startup in the SDK prevent concurrent workers
            // from deadlocking while preserving Xauthority, session-bus, and
            // accessibility setup.
            prepare_desktop_environment: true,
            register_host_tools: Some(crate::check_update_tool::register_into),
            authorization_host: None,
            activity_observer: None,
        },
    ) {
        Ok(driver) => driver,
        Err(error) => {
            write_response(
                &mut writer,
                &ChannelResponse::error(
                    PRIVATE_WORKER_STARTUP_ERROR_REQUEST_ID,
                    &generation,
                    "runtime_initialization_failed",
                    error.to_string(),
                    ActionCompletion::NotStarted,
                ),
            )?;
            return Ok(());
        }
    };
    let metadata = match driver.metadata().await {
        Ok(metadata) => metadata,
        Err(error) => {
            write_response(
                &mut writer,
                &ChannelResponse::error(
                    PRIVATE_WORKER_STARTUP_ERROR_REQUEST_ID,
                    &generation,
                    "runtime_initialization_failed",
                    format!("read private-worker readiness metadata: {error}"),
                    ActionCompletion::NotStarted,
                ),
            )?;
            driver.shutdown().await?;
            return Ok(());
        }
    };
    initialized.ready();
    write_response(
        &mut writer,
        &ChannelResponse::ok(
            PRIVATE_WORKER_INITIALIZATION_REQUEST_ID,
            &generation,
            serde_json::json!({
                "ready": true,
                "pid": std::process::id(),
                "host_bundle_id": initialization.host_bundle_id,
                "environment_verified": true,
                "metadata": metadata,
            }),
        ),
    )?;

    let mut sessions: HashMap<String, Arc<CuaDriverSession>> = HashMap::new();
    loop {
        let line = match read_private_worker_message(&mut reader) {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(error) => {
                write_response(
                    &mut writer,
                    &ChannelResponse::error(
                        0,
                        &generation,
                        "request_too_large",
                        format!("read private worker request: {error}"),
                        ActionCompletion::NotStarted,
                    ),
                )?;
                break;
            }
        };
        let request: ChannelRequest = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(error) => {
                write_response(
                    &mut writer,
                    &ChannelResponse::error(
                        0,
                        &generation,
                        "invalid_request",
                        format!("parse private worker request: {error}"),
                        ActionCompletion::NotStarted,
                    ),
                )?;
                continue;
            }
        };
        if request.protocol_version != PRIVATE_WORKER_PROTOCOL_VERSION
            || request.generation != generation
            || request.request_id <= PRIVATE_WORKER_INITIALIZATION_REQUEST_ID
        {
            write_response(
                &mut writer,
                &ChannelResponse::error(
                    request.request_id,
                    &generation,
                    "invalid_request_identity",
                    "private worker request used a reserved identity or belonged to another runtime generation",
                    ActionCompletion::NotStarted,
                ),
            )?;
            continue;
        }

        let response = handle_request(&driver, &mut sessions, &generation, request).await;
        let shutdown = response
            .result
            .as_ref()
            .and_then(|value| value.get("shutdown"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        write_response(&mut writer, &response)?;
        if shutdown {
            break;
        }
    }

    sessions.clear();
    driver.shutdown().await?;
    Ok(())
}

async fn handle_request(
    driver: &Arc<CuaDriver>,
    sessions: &mut HashMap<String, Arc<CuaDriverSession>>,
    generation: &str,
    request: ChannelRequest,
) -> ChannelResponse {
    let request_id = request.request_id;
    let result: Result<Value, String> = match request.operation.as_str() {
        "metadata" => driver
            .metadata()
            .await
            .and_then(|metadata| {
                serde_json::to_value(metadata).map_err(|error| {
                    cua_driver_sdk::DriverError::Protocol {
                        reason: error.to_string(),
                    }
                })
            })
            .map_err(|error| error.to_string()),
        "list" => driver
            .list_tools_json()
            .await
            .map_err(|error| error.to_string())
            .and_then(|json| serde_json::from_str(&json).map_err(|error| error.to_string())),
        "sessions_list" => driver
            .list_host_sessions_json()
            .await
            .map_err(|error| error.to_string())
            .and_then(|json| serde_json::from_str(&json).map_err(|error| error.to_string())),
        "call" => {
            let name = request.name.as_deref().unwrap_or("");
            let arguments = request
                .arguments
                .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
            let invocation = if let Some(handle) = request.session_handle.as_deref() {
                match sessions.get(handle) {
                    Some(session) => {
                        session
                            .call_tool(name.to_owned(), arguments.to_string())
                            .await
                    }
                    None => {
                        return ChannelResponse::error(
                            request_id,
                            generation,
                            "session_not_bound",
                            "private worker session handle is not live on this channel",
                            ActionCompletion::NotStarted,
                        );
                    }
                }
            } else {
                driver.call_tool_from_trusted_adapter(name, arguments).await
            };
            invocation
                .map_err(|error| error.to_string())
                .and_then(|result| {
                    serde_json::from_str(&result.raw_json).map_err(|error| error.to_string())
                })
        }
        "bind_session" => {
            let options = request
                .arguments
                .ok_or_else(|| "bind_session omitted options".to_owned())
                .and_then(|value| serde_json::from_value(value).map_err(|error| error.to_string()));
            match options {
                Ok(options) => match driver.create_trusted_session(options) {
                    Ok(session) => {
                        let handle = uuid::Uuid::new_v4().to_string();
                        sessions.insert(handle.clone(), session);
                        Ok(serde_json::json!({"session_handle": handle}))
                    }
                    Err(error) => Err(error.to_string()),
                },
                Err(error) => Err(error),
            }
        }
        "close_session" => {
            let Some(handle) = request.session_handle.as_deref() else {
                return ChannelResponse::error(
                    request_id,
                    generation,
                    "invalid_request",
                    "close_session omitted session_handle",
                    ActionCompletion::NotStarted,
                );
            };
            if let Some(session) = sessions.remove(handle) {
                session.close();
            }
            Ok(serde_json::json!({"closed": true}))
        }
        "shutdown" => {
            sessions.clear();
            match driver.shutdown().await {
                Ok(()) => Ok(serde_json::json!({"shutdown": true})),
                Err(error) => Err(error.to_string()),
            }
        }
        other => Err(format!("unknown private worker operation: {other}")),
    };

    match result {
        Ok(value) => ChannelResponse::ok(request_id, generation, value),
        Err(error) => ChannelResponse::error(
            request_id,
            generation,
            "worker_request_failed",
            error,
            ActionCompletion::Completed,
        ),
    }
}

fn write_response(writer: &mut impl Write, response: &ChannelResponse) -> anyhow::Result<()> {
    let bytes = match encode_private_worker_message(response) {
        Ok(bytes) => bytes,
        Err(error) => encode_private_worker_message(&ChannelResponse::error(
            response.request_id,
            &response.generation,
            "response_too_large",
            format!(
                "private worker response exceeded the {} byte limit: {error}",
                PRIVATE_WORKER_MAX_MESSAGE_BYTES
            ),
            response.completion,
        ))?,
    };
    writer.write_all(&bytes)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::requested_generation;

    #[test]
    fn ordinary_process_is_not_a_private_worker() {
        assert!(requested_generation().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn control_pipe_descriptors_are_sealed_against_exec_descendants() {
        let mut descriptors = [-1; 2];
        // SAFETY: the output array has room for both descriptors.
        assert_eq!(unsafe { libc::pipe(descriptors.as_mut_ptr()) }, 0);
        for descriptor in descriptors {
            super::mark_control_pipe_close_on_exec(descriptor).unwrap();
            // SAFETY: the descriptor remains open through this query.
            let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
            assert_ne!(flags & libc::FD_CLOEXEC, 0);
            // SAFETY: each descriptor is closed exactly once.
            unsafe {
                libc::close(descriptor);
            }
        }
    }
}
