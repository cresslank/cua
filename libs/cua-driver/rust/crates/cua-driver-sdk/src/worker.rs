//! Private supervised-worker transport for the typed SDK.
//!
//! The worker owns one runtime in a directly spawned child process. Its only
//! request/control surface is the child's inherited stdin/stdout pair: there
//! is no discovery, reconnect, or reattachment protocol. Private workers are
//! available only through the directly held child and platform containment
//! capabilities; there is no process discovery or PID-based reattachment.

use crate::{
    embedded::{
        allowed_environment_name, private_worker_environment, PrivateWorkerEnvironmentError,
        PRIVATE_WORKER_MAX_ENVIRONMENT_BYTES, PRIVATE_WORKER_MAX_ENVIRONMENT_ENTRIES,
        PRIVATE_WORKER_MAX_ENVIRONMENT_NAME_BYTES, PRIVATE_WORKER_MAX_ENVIRONMENT_VALUE_BYTES,
    },
    ConfiguredDriverOptions, DriverError, DriverMetadata, EmbeddedEnvironmentVariable,
    TrustedSessionOptions,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(target_os = "linux")]
use std::ffi::CString;
use std::io::{self, BufRead, BufReader, Write};
#[cfg(all(not(target_os = "linux"), not(target_os = "windows")))]
use std::process::{Child, ChildStdin, ChildStdout};
#[cfg(not(target_os = "linux"))]
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
#[cfg(target_os = "linux")]
use std::sync::OnceLock;
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};
use uuid::Uuid;

#[cfg(target_os = "linux")]
use std::os::fd::{FromRawFd, OwnedFd};

#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(all(unix, not(target_os = "linux")))]
use std::os::unix::process::CommandExt;

#[cfg(target_os = "macos")]
use std::net::Shutdown;
#[cfg(target_os = "macos")]
use std::os::unix::{
    fs::DirBuilderExt,
    net::{UnixListener, UnixStream},
};
#[cfg(target_os = "macos")]
use std::path::PathBuf;

pub const PRIVATE_WORKER_PROTOCOL_VERSION: u32 = 1;
/// Reserved identity used only when startup fails before initialization can
/// produce a correlated response.
pub const PRIVATE_WORKER_STARTUP_ERROR_REQUEST_ID: u64 = 0;
/// Correlated identity for the private-worker initialization exchange. Normal
/// host request IDs start at two after initialization succeeds.
pub const PRIVATE_WORKER_INITIALIZATION_REQUEST_ID: u64 = 1;
pub const PRIVATE_WORKER_MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

#[cfg(all(not(target_os = "linux"), not(target_os = "windows")))]
type WorkerChild = Child;
const DEFAULT_STARTUP_TIMEOUT_MS: u64 = 10_000;
const DEFAULT_SHUTDOWN_TIMEOUT_MS: u64 = 2_000;
const PRIVATE_WORKER_MAX_PATH_BYTES: usize = 4_096;
const PRIVATE_WORKER_MAX_AUTHORIZATION_MODES: usize = 3;
const PRIVATE_WORKER_MAX_READINESS_BYTES: usize = 64 * 1024;
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
#[cfg(target_os = "linux")]
const LINUX_EXACT_REAP_GRACE: Duration = Duration::from_millis(100);
#[cfg(target_os = "linux")]
const LINUX_REAP_REGISTRY_CAPACITY: usize = 1_024;

struct BoundedMessageWriter {
    bytes: Vec<u8>,
    limit: usize,
    deadline: Option<Instant>,
}

struct DeadlineReader<'a> {
    remaining: &'a [u8],
    deadline: Instant,
}

impl io::Read for DeadlineReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if Instant::now() >= self.deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "private-worker response parsing exceeded its deadline",
            ));
        }
        let count = buffer.len().min(self.remaining.len()).min(4 * 1024);
        buffer[..count].copy_from_slice(&self.remaining[..count]);
        self.remaining = &self.remaining[count..];
        Ok(count)
    }
}

impl Write for BoundedMessageWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "private-worker request serialization exceeded its deadline",
            ));
        }
        if buffer.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("private worker message exceeds {} bytes", self.limit),
            ));
        }
        // Bound each copy so serde_json's write_all loop rechecks the
        // absolute deadline while emitting a single large string value.
        let count = buffer.len().min(4 * 1024);
        self.bytes.extend_from_slice(&buffer[..count]);
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn encode_private_worker_message_with_limit<T: Serialize>(
    value: &T,
    limit: usize,
    deadline: Option<Instant>,
) -> io::Result<Vec<u8>> {
    let mut writer = BoundedMessageWriter {
        bytes: Vec::new(),
        limit,
        deadline,
    };
    serde_json::to_writer(&mut writer, value).map_err(io::Error::other)?;
    Ok(writer.bytes)
}

pub fn encode_private_worker_message<T: Serialize>(value: &T) -> io::Result<Vec<u8>> {
    encode_private_worker_message_with_limit(value, PRIVATE_WORKER_MAX_MESSAGE_BYTES, None)
}

fn encode_private_worker_message_until<T: Serialize>(
    value: &T,
    deadline: Instant,
) -> io::Result<Vec<u8>> {
    encode_private_worker_message_with_limit(
        value,
        PRIVATE_WORKER_MAX_MESSAGE_BYTES,
        Some(deadline),
    )
}

fn read_private_worker_message_with_limit<R: BufRead>(
    reader: &mut R,
    limit: usize,
) -> io::Result<Option<String>> {
    let mut message = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            if message.is_empty() {
                return Ok(None);
            }
            break;
        }
        if let Some(newline) = available.iter().position(|byte| *byte == b'\n') {
            if newline > limit.saturating_sub(message.len()) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("private worker message exceeds {limit} bytes"),
                ));
            }
            message.extend_from_slice(&available[..newline]);
            reader.consume(newline + 1);
            if message.last() == Some(&b'\r') {
                message.pop();
            }
            return String::from_utf8(message)
                .map(Some)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error));
        }
        if available.len() > limit.saturating_sub(message.len()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("private worker message exceeds {limit} bytes"),
            ));
        }
        message.extend_from_slice(available);
        let consumed = available.len();
        reader.consume(consumed);
    }
    String::from_utf8(message)
        .map(Some)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

pub fn read_private_worker_message<R: BufRead>(reader: &mut R) -> io::Result<Option<String>> {
    read_private_worker_message_with_limit(reader, PRIVATE_WORKER_MAX_MESSAGE_BYTES)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum ActionCompletion {
    NotStarted,
    Completed,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelRequest {
    pub protocol_version: u32,
    pub request_id: u64,
    pub generation: String,
    pub operation: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_handle: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivateWorkerBinding {
    pub protocol_version: u32,
    pub generation: String,
    pub nonce: String,
    pub worker_pid: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelResponse {
    pub protocol_version: u32,
    pub request_id: u64,
    pub generation: String,
    pub ok: bool,
    pub completion: ActionCompletion,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}

impl ChannelResponse {
    pub fn ok(request_id: u64, generation: impl Into<String>, result: Value) -> ChannelResponse {
        ChannelResponse {
            protocol_version: PRIVATE_WORKER_PROTOCOL_VERSION,
            request_id,
            generation: generation.into(),
            ok: true,
            completion: ActionCompletion::Completed,
            result: Some(result),
            error: None,
            error_code: None,
        }
    }

    pub fn error(
        request_id: u64,
        generation: impl Into<String>,
        error_code: impl Into<String>,
        error: impl Into<String>,
        completion: ActionCompletion,
    ) -> ChannelResponse {
        ChannelResponse {
            protocol_version: PRIVATE_WORKER_PROTOCOL_VERSION,
            request_id,
            generation: generation.into(),
            ok: false,
            completion,
            result: None,
            error: Some(error.into()),
            error_code: Some(error_code.into()),
        }
    }
}

fn response_identity_matches(
    request: &ChannelRequest,
    response: &ChannelResponse,
    generation: &str,
) -> bool {
    let correlated = response.request_id == request.request_id;
    let structured_startup_error = request.operation == "initialize"
        && response.request_id == PRIVATE_WORKER_STARTUP_ERROR_REQUEST_ID
        && !response.ok
        && response.completion == ActionCompletion::NotStarted
        && response.error_code.is_some()
        && response.error.is_some();
    response.protocol_version == PRIVATE_WORKER_PROTOCOL_VERSION
        && response.generation == generation
        && (correlated || structured_startup_error)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerEnvironmentVariable {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerInitialization {
    pub configured_driver: ConfiguredDriverOptions,
    pub host_bundle_id: String,
    /// Exact post-policy environment expected in the env-cleared child. The
    /// worker confirms these values before runtime initialization and returns
    /// only a boolean attestation, never the values.
    pub environment_attestation: Vec<WorkerEnvironmentVariable>,
}

#[derive(Debug, Clone)]
pub(crate) struct ValidatedWorkerOptions {
    pub binary_path: String,
    pub host_bundle_id: String,
    pub startup_timeout: Duration,
    pub shutdown_timeout: Duration,
    pub configured_driver: ConfiguredDriverOptions,
    pub environment: Vec<EmbeddedEnvironmentVariable>,
    pub inherit_stderr: bool,
}

#[cfg(target_os = "windows")]
struct HostDeathGuard(usize);

#[cfg(target_os = "linux")]
#[derive(Debug)]
struct HostDeathGuard {
    finish: Option<std::sync::mpsc::SyncSender<()>>,
    parent_thread: Option<std::thread::JoinHandle<()>>,
    pidfd: Option<OwnedFd>,
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
type HostDeathGuard = ();

#[cfg(target_os = "linux")]
impl Drop for HostDeathGuard {
    fn drop(&mut self) {
        self.finish.take();
        // Dropping the handle releases the lifetime parent. Its direct cleanup
        // is deadline-bounded; a resistant exact pidfd is transferred into the
        // capacity-bounded process-wide nonblocking reap registry.
        self.parent_thread.take();
    }
}

#[cfg(target_os = "linux")]
impl HostDeathGuard {
    fn take_pidfd(&mut self) -> io::Result<OwnedFd> {
        self.pidfd.take().ok_or_else(|| {
            io::Error::other("private-worker spawn did not return its atomic pidfd capability")
        })
    }
}

#[cfg(target_os = "windows")]
impl Drop for HostDeathGuard {
    fn drop(&mut self) {
        use core::ffi::c_void;
        use windows::Win32::Foundation::{CloseHandle, HANDLE};
        unsafe {
            let _ = CloseHandle(HANDLE(self.0 as *mut c_void));
        }
    }
}

#[cfg(target_os = "windows")]
impl HostDeathGuard {
    fn terminate(&self) {
        use core::ffi::c_void;
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::System::JobObjects::TerminateJobObject;
        unsafe {
            let _ = TerminateJobObject(HANDLE(self.0 as *mut c_void), 1);
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn configure_host_death_containment(_command: &mut Command, _host_pid: u32) {}

#[cfg(all(unix, not(target_os = "linux")))]
fn configure_child_descriptor_boundary(command: &mut Command) -> io::Result<()> {
    let mut limits = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limits) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let descriptor_ceiling = limits.rlim_cur.min(i32::MAX as libc::rlim_t) as libc::c_int;
    unsafe {
        command.pre_exec(move || {
            for descriptor in 3..descriptor_ceiling {
                let flags = libc::fcntl(descriptor, libc::F_GETFD);
                if flags < 0 {
                    let error = io::Error::last_os_error();
                    if error.raw_os_error() == Some(libc::EBADF) {
                        continue;
                    }
                    return Err(error);
                }
                if libc::fcntl(descriptor, libc::F_SETFD, flags | libc::FD_CLOEXEC) < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    Ok(())
}

#[cfg(not(unix))]
fn configure_child_descriptor_boundary(_command: &mut Command) -> io::Result<()> {
    Ok(())
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
struct WorkerChild {
    process_id: u32,
    stdin: Option<std::fs::File>,
    stdout: Option<std::fs::File>,
}

#[cfg(target_os = "linux")]
impl WorkerChild {
    fn id(&self) -> u32 {
        self.process_id
    }
}

#[cfg(target_os = "windows")]
struct WorkerChild {
    process: usize,
    process_id: u32,
    stdin: Option<std::fs::File>,
    stdout: Option<std::fs::File>,
}

#[cfg(target_os = "windows")]
impl WorkerChild {
    fn id(&self) -> u32 {
        self.process_id
    }

    fn try_wait(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
        use core::ffi::c_void;
        use std::os::windows::process::ExitStatusExt;
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::System::Threading::GetExitCodeProcess;

        let mut code = 0_u32;
        // SAFETY: process is a live owned process handle until Drop.
        unsafe {
            GetExitCodeProcess(HANDLE(self.process as *mut c_void), &mut code)
                .map_err(io::Error::other)?;
        }
        if code == 259 {
            Ok(None)
        } else {
            Ok(Some(std::process::ExitStatus::from_raw(code)))
        }
    }

    fn wait(&mut self) -> io::Result<std::process::ExitStatus> {
        use core::ffi::c_void;
        use windows::Win32::Foundation::{HANDLE, WAIT_FAILED};
        use windows::Win32::System::Threading::{WaitForSingleObject, INFINITE};

        // SAFETY: process is a live owned process handle until Drop.
        let wait = unsafe { WaitForSingleObject(HANDLE(self.process as *mut c_void), INFINITE) };
        if wait == WAIT_FAILED {
            return Err(io::Error::last_os_error());
        }
        self.try_wait()?.ok_or_else(|| {
            io::Error::other(
                "Windows process remained active after its process handle was signaled",
            )
        })
    }

    fn kill(&mut self) -> io::Result<()> {
        use core::ffi::c_void;
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::System::Threading::TerminateProcess;

        // SAFETY: process is a live owned process handle until Drop.
        unsafe { TerminateProcess(HANDLE(self.process as *mut c_void), 1) }
            .map_err(io::Error::other)
    }
}

#[cfg(target_os = "windows")]
impl Drop for WorkerChild {
    fn drop(&mut self) {
        use core::ffi::c_void;
        use windows::Win32::Foundation::{CloseHandle, HANDLE};
        // SAFETY: WorkerChild uniquely owns this process handle.
        unsafe {
            let _ = CloseHandle(HANDLE(self.process as *mut c_void));
        }
    }
}

#[cfg(target_os = "windows")]
fn create_host_death_job() -> io::Result<HostDeathGuard> {
    use core::ffi::c_void;
    use windows::Win32::System::JobObjects::{
        CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    // SAFETY: all pointers refer to initialized storage for the duration of the calls.
    unsafe {
        let job =
            CreateJobObjectW(None, windows::core::PCWSTR::null()).map_err(io::Error::other)?;
        let guard = HostDeathGuard(job.0 as usize);
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const c_void,
            std::mem::size_of_val(&info) as u32,
        )
        .map_err(io::Error::other)?;
        Ok(guard)
    }
}

#[cfg(target_os = "windows")]
fn quote_windows_argument(argument: &std::ffi::OsStr, output: &mut Vec<u16>) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    let argument = argument.encode_wide().collect::<Vec<_>>();
    if argument.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "private-worker argument contains NUL",
        ));
    }
    let requires_quotes = argument.is_empty()
        || argument
            .iter()
            .any(|unit| *unit == b' ' as u16 || *unit == b'\t' as u16 || *unit == b'"' as u16);
    if !requires_quotes {
        output.extend_from_slice(&argument);
        return Ok(());
    }
    output.push(b'"' as u16);
    let mut backslashes = 0_usize;
    for unit in argument {
        if unit == b'\\' as u16 {
            backslashes += 1;
        } else if unit == b'"' as u16 {
            output.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2 + 1));
            output.push(unit);
            backslashes = 0;
        } else {
            output.extend(std::iter::repeat_n(b'\\' as u16, backslashes));
            backslashes = 0;
            output.push(unit);
        }
    }
    output.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2));
    output.push(b'"' as u16);
    Ok(())
}

#[cfg(target_os = "linux")]
const LINUX_CLOSE_RANGE_CLOEXEC: libc::c_uint = 1 << 2;

#[cfg(target_os = "linux")]
const LINUX_CHILD_STAGE_PDEATHSIG: i32 = 1;
#[cfg(target_os = "linux")]
const LINUX_CHILD_STAGE_PARENT: i32 = 2;
#[cfg(target_os = "linux")]
const LINUX_CHILD_STAGE_STDIN: i32 = 3;
#[cfg(target_os = "linux")]
const LINUX_CHILD_STAGE_STDOUT: i32 = 4;
#[cfg(target_os = "linux")]
const LINUX_CHILD_STAGE_STDERR: i32 = 5;
#[cfg(target_os = "linux")]
const LINUX_CHILD_STAGE_CLOSE_RANGE: i32 = 6;
#[cfg(target_os = "linux")]
const LINUX_CHILD_STAGE_SIGPIPE: i32 = 7;
#[cfg(target_os = "linux")]
const LINUX_CHILD_STAGE_EXECVE: i32 = 8;
#[cfg(target_os = "linux")]
const LINUX_CHILD_STAGE_PARENT_GATE: i32 = 9;
#[cfg(target_os = "linux")]
const LINUX_CHILD_STAGE_NO_NEW_PRIVS: i32 = 10;
#[cfg(target_os = "linux")]
const LINUX_CHILD_STAGE_EXECUTABLE_PRIVILEGE: i32 = 11;

#[cfg(target_os = "linux")]
#[repr(C)]
struct LinuxCloneArgs {
    flags: u64,
    pidfd: u64,
    child_tid: u64,
    parent_tid: u64,
    exit_signal: u64,
    stack: u64,
    stack_size: u64,
    tls: u64,
    set_tid: u64,
    set_tid_size: u64,
    cgroup: u64,
}

#[cfg(target_os = "linux")]
#[repr(C)]
#[derive(Clone, Copy)]
struct LinuxChildError {
    stage: i32,
    errno: i32,
}

#[cfg(all(target_os = "linux", test))]
#[derive(Clone, Copy, Debug, Default)]
enum LinuxChildTestAction {
    #[default]
    None,
    KillBeforeExec,
    StopAfterPdeathsig,
    CloseRangeUnsupported,
}

#[cfg(target_os = "linux")]
struct LinuxExecImage {
    executable: OwnedFd,
    _argv: Vec<CString>,
    _env: Vec<CString>,
    argvp: Vec<usize>,
    envp: Vec<usize>,
}

#[cfg(all(target_os = "linux", test))]
struct LinuxCleanupTestControl {
    started: std::sync::mpsc::SyncSender<libc::c_int>,
    finished: std::sync::mpsc::SyncSender<bool>,
    reaped: std::sync::mpsc::SyncSender<()>,
    force_registry: bool,
}

#[cfg(target_os = "linux")]
struct LinuxSpawnPlan {
    image: LinuxExecImage,
    stdin_child: OwnedFd,
    stdin_parent: OwnedFd,
    stdout_child: OwnedFd,
    stdout_parent: OwnedFd,
    stderr_child: OwnedFd,
    parent_gate_child: OwnedFd,
    parent_gate_parent: OwnedFd,
    exec_error_read: OwnedFd,
    exec_error_write: OwnedFd,
    expected_parent_pid: libc::pid_t,
    #[cfg(test)]
    test_action: LinuxChildTestAction,
    #[cfg(test)]
    test_clone3_errno: Option<i32>,
    #[cfg(test)]
    test_abandon_handoff: bool,
    #[cfg(test)]
    test_cleanup_control: Option<LinuxCleanupTestControl>,
}

#[cfg(target_os = "linux")]
struct LinuxSpawnFailure {
    error: io::Error,
    cleanup_pidfd: Option<OwnedFd>,
}

#[cfg(target_os = "linux")]
impl LinuxSpawnFailure {
    fn before_clone(error: io::Error) -> Self {
        Self {
            error,
            cleanup_pidfd: None,
        }
    }

    fn after_clone(error: io::Error, cleanup_pidfd: OwnedFd) -> Self {
        Self {
            error,
            cleanup_pidfd: Some(cleanup_pidfd),
        }
    }
}

#[cfg(target_os = "linux")]
struct LinuxReapPermit;

#[cfg(target_os = "linux")]
struct LinuxPendingReap {
    pidfd: OwnedFd,
    _permit: LinuxReapPermit,
    completion: Option<std::sync::mpsc::SyncSender<()>>,
}

#[cfg(target_os = "linux")]
static LINUX_REAP_PERMITS: AtomicUsize = AtomicUsize::new(0);

#[cfg(target_os = "linux")]
struct LinuxReapRegistry {
    pending: Mutex<Vec<LinuxPendingReap>>,
    state: AtomicU8,
}

#[cfg(target_os = "linux")]
static LINUX_REAP_REGISTRY: OnceLock<LinuxReapRegistry> = OnceLock::new();

#[cfg(target_os = "linux")]
impl Drop for LinuxReapPermit {
    fn drop(&mut self) {
        let previous = LINUX_REAP_PERMITS.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "Linux reap permit count underflowed");
    }
}

fn linux_reap_registry() -> &'static LinuxReapRegistry {
    LINUX_REAP_REGISTRY.get_or_init(|| LinuxReapRegistry {
        pending: Mutex::new(Vec::with_capacity(LINUX_REAP_REGISTRY_CAPACITY)),
        state: AtomicU8::new(0),
    })
}

#[cfg(target_os = "linux")]
fn linux_ensure_reap_registry_until(deadline: Instant) -> io::Result<()> {
    let registry = linux_reap_registry();
    loop {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "private-worker reap registry startup exceeded its deadline",
            ));
        }
        match registry.state.load(Ordering::Acquire) {
            2 => return Ok(()),
            1 => std::thread::sleep(Duration::from_millis(1)),
            _ => {
                if registry
                    .state
                    .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                    .is_err()
                {
                    continue;
                }
                match std::thread::Builder::new()
                    .name("cua-private-worker-pidfd-reaper".into())
                    .spawn(move || loop {
                        // Keep exact pidfds in process-global storage. A panic
                        // poisons but does not discard that inventory, and the
                        // next iteration recovers the guard.
                        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            let mut pending = registry
                                .pending
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            let mut index = 0;
                            while index < pending.len() {
                                match linux_try_reap_pidfd(&pending[index].pidfd) {
                                    Ok(true) => {
                                        let mut completed = pending.swap_remove(index);
                                        if let Some(completion) = completed.completion.take() {
                                            let _ = completion.try_send(());
                                        }
                                    }
                                    Ok(false) | Err(_) => index += 1,
                                }
                            }
                        }));
                        std::thread::sleep(Duration::from_millis(100));
                    }) {
                    Ok(_) => registry.state.store(2, Ordering::Release),
                    Err(error) => {
                        registry.state.store(0, Ordering::Release);
                        return Err(io::Error::other(format!(
                            "start bounded pidfd reap registry: {error}"
                        )));
                    }
                }
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn linux_reserve_reap_permit(deadline: Instant) -> io::Result<LinuxReapPermit> {
    linux_ensure_reap_registry_until(deadline)?;
    loop {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "private-worker reap permit reservation exceeded its deadline",
            ));
        }
        let current = LINUX_REAP_PERMITS.load(Ordering::Acquire);
        if current >= LINUX_REAP_REGISTRY_CAPACITY {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "private-worker exact reap registry is at capacity",
            ));
        }
        if LINUX_REAP_PERMITS
            .compare_exchange_weak(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return Ok(LinuxReapPermit);
        }
    }
}

#[cfg(target_os = "linux")]
fn linux_register_pending_reap(item: LinuxPendingReap) {
    // Direct cleanup already sent SIGKILL. Retain the exact pidfd and permit
    // in process-global storage until waitid(P_PIDFD) proves exact reaping.
    linux_reap_registry()
        .pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(item);
}

#[cfg(target_os = "linux")]
fn linux_preflight_close_range() -> io::Result<()> {
    let descriptor = linux_open_dev_null()?;
    // Marking an already-CLOEXEC fresh descriptor is an isolated support/policy
    // probe and cannot alter any descriptor supplied by the embedding host.
    let result = unsafe {
        libc::syscall(
            libc::SYS_close_range,
            descriptor.as_raw_fd() as libc::c_uint,
            descriptor.as_raw_fd() as libc::c_uint,
            LINUX_CLOSE_RANGE_CLOEXEC,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(all(target_os = "linux", test))]
fn linux_preflight_close_range_with(injected_errno: Option<i32>) -> io::Result<()> {
    match injected_errno {
        Some(errno) => Err(io::Error::from_raw_os_error(errno)),
        None => linux_preflight_close_range(),
    }
}

#[cfg(target_os = "linux")]
fn linux_open_dev_null() -> io::Result<OwnedFd> {
    let descriptor =
        unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC, 0) };
    if descriptor < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(descriptor) })
    }
}

#[cfg(target_os = "linux")]
fn linux_fd_above_stdio(descriptor: OwnedFd) -> io::Result<OwnedFd> {
    if descriptor.as_raw_fd() >= 3 {
        return Ok(descriptor);
    }
    let duplicate = unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    if duplicate < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(duplicate) })
    }
}

#[cfg(target_os = "linux")]
fn linux_pipe(flags: libc::c_int) -> io::Result<(OwnedFd, OwnedFd)> {
    let mut descriptors = [-1; 2];
    if unsafe { libc::pipe2(descriptors.as_mut_ptr(), flags) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let first = linux_fd_above_stdio(unsafe { OwnedFd::from_raw_fd(descriptors[0]) })?;
    let second = linux_fd_above_stdio(unsafe { OwnedFd::from_raw_fd(descriptors[1]) })?;
    Ok((first, second))
}

#[cfg(target_os = "linux")]
fn linux_socketpair(flags: libc::c_int) -> io::Result<(OwnedFd, OwnedFd)> {
    let mut descriptors = [-1; 2];
    if unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_STREAM | flags,
            0,
            descriptors.as_mut_ptr(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    let first = linux_fd_above_stdio(unsafe { OwnedFd::from_raw_fd(descriptors[0]) })?;
    let second = linux_fd_above_stdio(unsafe { OwnedFd::from_raw_fd(descriptors[1]) })?;
    Ok((first, second))
}

#[cfg(target_os = "linux")]
fn linux_duplicate_stderr(inherit_stderr: bool) -> io::Result<OwnedFd> {
    if inherit_stderr {
        let duplicate = unsafe { libc::fcntl(2, libc::F_DUPFD_CLOEXEC, 3) };
        if duplicate >= 0 {
            return Ok(unsafe { OwnedFd::from_raw_fd(duplicate) });
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EBADF) {
            return Err(error);
        }
    }
    linux_fd_above_stdio(linux_open_dev_null()?)
}

#[cfg(target_os = "linux")]
fn linux_open_admissible_executable(program: &CString) -> io::Result<OwnedFd> {
    // Open first so every admission check and the eventual execveat refer to
    // one immutable file description even if the pathname is replaced.
    let descriptor = unsafe { libc::open(program.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC, 0) };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: open returned one fresh descriptor, transferred exactly once.
    let executable =
        linux_fd_above_stdio(unsafe { OwnedFd::from_raw_fd(descriptor) }).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("relocate private-worker executable above stdio: {error}"),
            )
        })?;

    let mut metadata = unsafe { std::mem::zeroed::<libc::stat>() };
    // SAFETY: fstat writes one stat record and retains no pointer.
    if unsafe { libc::fstat(executable.as_raw_fd(), &mut metadata) } != 0 {
        let error = io::Error::last_os_error();
        return Err(io::Error::new(
            error.kind(),
            format!("inspect private-worker executable metadata: {error}"),
        ));
    }
    if metadata.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "private-worker executable must be a regular file",
        ));
    }
    if metadata.st_mode & (libc::S_ISUID | libc::S_ISGID) != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "private-worker executable must not have setuid or setgid mode bits",
        ));
    }
    if metadata.st_mode & 0o111 == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "private-worker executable has no executable mode bits",
        ));
    }

    // The existence of security.capability is disqualifying. ENODATA is the
    // only admissible negative result; unsupported or denied inspection fails
    // closed because it cannot prove the object is unprivileged.
    let capability_size = unsafe {
        libc::fgetxattr(
            executable.as_raw_fd(),
            c"security.capability".as_ptr(),
            std::ptr::null_mut(),
            0,
        )
    };
    if capability_size >= 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "private-worker executable must not have file capabilities",
        ));
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() != Some(libc::ENODATA) {
        return Err(io::Error::new(
            error.kind(),
            format!("inspect private-worker executable security.capability: {error}"),
        ));
    }

    let mut magic = [0_u8; 4];
    let mut filled = 0;
    while filled < magic.len() {
        // SAFETY: pread writes only the unfilled portion of the live stack
        // buffer and does not alter the shared file-description offset.
        let count = unsafe {
            libc::pread(
                executable.as_raw_fd(),
                magic[filled..].as_mut_ptr().cast(),
                magic.len() - filled,
                filled as libc::off_t,
            )
        };
        if count > 0 {
            filled += count as usize;
            continue;
        }
        if count < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(io::Error::new(
                error.kind(),
                format!("inspect private-worker executable header: {error}"),
            ));
        }
        break;
    }
    if magic != *b"\x7fELF" {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "private-worker executable must be an ELF binary; scripts are not admitted",
        ));
    }
    Ok(executable)
}

#[cfg(target_os = "linux")]
fn prepare_linux_spawn_plan_until(
    program: &str,
    arguments: &[String],
    environment: &[EmbeddedEnvironmentVariable],
    expected_parent_pid: libc::pid_t,
    inherit_stderr: bool,
    deadline: Instant,
) -> io::Result<LinuxSpawnPlan> {
    let ensure_deadline = || {
        if Instant::now() >= deadline {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "private-worker spawn preparation exceeded its startup deadline",
            ))
        } else {
            Ok(())
        }
    };
    ensure_deadline()?;
    let program = CString::new(program).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "private-worker executable path contains NUL",
        )
    })?;
    let executable = linux_open_admissible_executable(&program)?;
    ensure_deadline()?;
    let mut argv = Vec::with_capacity(arguments.len() + 1);
    argv.push(program.clone());
    for argument in arguments {
        argv.push(CString::new(argument.as_str()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "private-worker argument contains NUL",
            )
        })?);
    }
    let mut env = Vec::with_capacity(environment.len());
    for variable in environment {
        env.push(
            CString::new(format!("{}={}", variable.name, variable.value)).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "private-worker environment contains NUL",
                )
            })?,
        );
    }
    let mut argvp = argv
        .iter()
        .map(|argument| argument.as_ptr() as usize)
        .collect::<Vec<_>>();
    argvp.push(0);
    let mut envp = env
        .iter()
        .map(|variable| variable.as_ptr() as usize)
        .collect::<Vec<_>>();
    envp.push(0);

    ensure_deadline()?;
    let (stdin_child, stdin_parent) = linux_pipe(libc::O_CLOEXEC)?;
    let (stdout_parent, stdout_child) = linux_pipe(libc::O_CLOEXEC)?;
    let (parent_gate_parent, parent_gate_child) = linux_socketpair(libc::SOCK_CLOEXEC)?;
    let (exec_error_read, exec_error_write) = linux_pipe(libc::O_CLOEXEC | libc::O_NONBLOCK)?;
    ensure_deadline()?;
    Ok(LinuxSpawnPlan {
        image: LinuxExecImage {
            executable,
            _argv: argv,
            _env: env,
            argvp,
            envp,
        },
        stdin_child,
        stdin_parent,
        stdout_child,
        stdout_parent,
        stderr_child: linux_duplicate_stderr(inherit_stderr)?,
        parent_gate_child,
        parent_gate_parent,
        exec_error_read,
        exec_error_write,
        expected_parent_pid,
        #[cfg(test)]
        test_action: LinuxChildTestAction::None,
        #[cfg(test)]
        test_clone3_errno: None,
        #[cfg(test)]
        test_abandon_handoff: false,
        #[cfg(test)]
        test_cleanup_control: None,
    })
}

#[cfg(all(target_os = "linux", test))]
fn prepare_linux_spawn_plan(
    program: &str,
    arguments: &[String],
    environment: &[EmbeddedEnvironmentVariable],
    expected_parent_pid: libc::pid_t,
    inherit_stderr: bool,
) -> io::Result<LinuxSpawnPlan> {
    prepare_linux_spawn_plan_until(
        program,
        arguments,
        environment,
        expected_parent_pid,
        inherit_stderr,
        Instant::now() + Duration::from_secs(30),
    )
}

#[cfg(target_os = "linux")]
fn linux_duplicate_pidfd(pidfd: &OwnedFd) -> io::Result<OwnedFd> {
    let descriptor = unsafe { libc::fcntl(pidfd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    if descriptor < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(descriptor) })
    }
}

#[cfg(target_os = "linux")]
unsafe fn linux_child_errno() -> i32 {
    *libc::__errno_location()
}

#[cfg(target_os = "linux")]
unsafe fn linux_child_fail(error_fd: libc::c_int, stage: i32, errno: i32) -> ! {
    let error = LinuxChildError { stage, errno };
    loop {
        let written = libc::syscall(
            libc::SYS_write,
            error_fd,
            (&error as *const LinuxChildError).cast::<libc::c_void>(),
            std::mem::size_of::<LinuxChildError>(),
        );
        if written == std::mem::size_of::<LinuxChildError>() as libc::c_long {
            break;
        }
        if written < 0 && linux_child_errno() == libc::EINTR {
            continue;
        }
        break;
    }
    libc::_exit(127)
}

#[cfg(target_os = "linux")]
unsafe fn linux_child_recheck_executable_privilege(plan: &LinuxSpawnPlan) {
    let mut metadata = std::mem::zeroed::<libc::stat>();
    // fstat is async-signal-safe and lets libc select the architecture-correct
    // kernel ABI instead of pairing SYS_fstat with a potentially mismatched
    // userspace stat layout on 32-bit Linux.
    if libc::fstat(plan.image.executable.as_raw_fd(), &mut metadata) != 0 {
        linux_child_fail(
            plan.exec_error_write.as_raw_fd(),
            LINUX_CHILD_STAGE_EXECUTABLE_PRIVILEGE,
            linux_child_errno(),
        );
    }
    if metadata.st_mode & libc::S_IFMT != libc::S_IFREG
        || metadata.st_mode & (libc::S_ISUID | libc::S_ISGID) != 0
    {
        linux_child_fail(
            plan.exec_error_write.as_raw_fd(),
            LINUX_CHILD_STAGE_EXECUTABLE_PRIVILEGE,
            libc::EPERM,
        );
    }
    let capability_size = libc::syscall(
        libc::SYS_fgetxattr,
        plan.image.executable.as_raw_fd(),
        c"security.capability".as_ptr(),
        std::ptr::null_mut::<libc::c_void>(),
        0,
    );
    if capability_size >= 0 {
        linux_child_fail(
            plan.exec_error_write.as_raw_fd(),
            LINUX_CHILD_STAGE_EXECUTABLE_PRIVILEGE,
            libc::EPERM,
        );
    }
    let error = linux_child_errno();
    if error != libc::ENODATA {
        linux_child_fail(
            plan.exec_error_write.as_raw_fd(),
            LINUX_CHILD_STAGE_EXECUTABLE_PRIVILEGE,
            error,
        );
    }
}

#[cfg(target_os = "linux")]
unsafe fn linux_child_exec(plan: &LinuxSpawnPlan) -> ! {
    if libc::syscall(
        libc::SYS_prctl,
        libc::PR_SET_PDEATHSIG,
        libc::SIGKILL,
        0,
        0,
        0,
    ) != 0
    {
        linux_child_fail(
            plan.exec_error_write.as_raw_fd(),
            LINUX_CHILD_STAGE_PDEATHSIG,
            linux_child_errno(),
        );
    }
    if libc::syscall(libc::SYS_prctl, libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
        linux_child_fail(
            plan.exec_error_write.as_raw_fd(),
            LINUX_CHILD_STAGE_NO_NEW_PRIVS,
            linux_child_errno(),
        );
    }

    #[cfg(test)]
    if matches!(plan.test_action, LinuxChildTestAction::StopAfterPdeathsig) {
        let pid = libc::syscall(libc::SYS_getpid);
        let _ = libc::syscall(libc::SYS_kill, pid, libc::SIGSTOP);
    }

    if libc::syscall(libc::SYS_getppid) != plan.expected_parent_pid as libc::c_long {
        linux_child_fail(
            plan.exec_error_write.as_raw_fd(),
            LINUX_CHILD_STAGE_PARENT,
            libc::ECHILD,
        );
    }

    let _ = libc::syscall(libc::SYS_close, plan.parent_gate_parent.as_raw_fd());
    let mut permission = 0_u8;
    loop {
        let count = libc::syscall(
            libc::SYS_read,
            plan.parent_gate_child.as_raw_fd(),
            (&mut permission as *mut u8).cast::<libc::c_void>(),
            1,
        );
        if count == 1 && permission == 1 {
            break;
        }
        if count < 0 && linux_child_errno() == libc::EINTR {
            continue;
        }
        linux_child_fail(
            plan.exec_error_write.as_raw_fd(),
            LINUX_CHILD_STAGE_PARENT_GATE,
            if count < 0 {
                linux_child_errno()
            } else {
                libc::ECANCELED
            },
        );
    }

    #[cfg(test)]
    if matches!(plan.test_action, LinuxChildTestAction::KillBeforeExec) {
        let pid = libc::syscall(libc::SYS_getpid);
        let _ = libc::syscall(libc::SYS_kill, pid, libc::SIGKILL);
        libc::_exit(127);
    }

    // fstat and fgetxattr are issued as raw syscalls over the already-opened
    // executable. This allocation-free recheck closes the pre-clone metadata
    // window while no_new_privs protects the remaining syscall boundary.
    linux_child_recheck_executable_privilege(plan);

    for (source, target, stage) in [
        (plan.stdin_child.as_raw_fd(), 0, LINUX_CHILD_STAGE_STDIN),
        (plan.stdout_child.as_raw_fd(), 1, LINUX_CHILD_STAGE_STDOUT),
        (plan.stderr_child.as_raw_fd(), 2, LINUX_CHILD_STAGE_STDERR),
    ] {
        // Every source was relocated above stdio in the parent, so dup3's
        // source!=target rule is guaranteed and flags=0 clears CLOEXEC.
        if libc::syscall(libc::SYS_dup3, source, target, 0) < 0 {
            linux_child_fail(
                plan.exec_error_write.as_raw_fd(),
                stage,
                linux_child_errno(),
            );
        }
    }

    #[cfg(test)]
    if matches!(
        plan.test_action,
        LinuxChildTestAction::CloseRangeUnsupported
    ) {
        linux_child_fail(
            plan.exec_error_write.as_raw_fd(),
            LINUX_CHILD_STAGE_CLOSE_RANGE,
            libc::ENOSYS,
        );
    }

    if libc::syscall(
        libc::SYS_close_range,
        3_u32,
        u32::MAX,
        LINUX_CLOSE_RANGE_CLOEXEC,
    ) != 0
    {
        linux_child_fail(
            plan.exec_error_write.as_raw_fd(),
            LINUX_CHILD_STAGE_CLOSE_RANGE,
            linux_child_errno(),
        );
    }

    let mut sigpipe = std::mem::zeroed::<libc::sigaction>();
    sigpipe.sa_sigaction = libc::SIG_DFL;
    if libc::sigemptyset(&mut sigpipe.sa_mask) != 0
        || libc::sigaction(libc::SIGPIPE, &sigpipe, std::ptr::null_mut()) != 0
    {
        linux_child_fail(
            plan.exec_error_write.as_raw_fd(),
            LINUX_CHILD_STAGE_SIGPIPE,
            linux_child_errno(),
        );
    }

    // The executable fd, argv/env strings, and pointer vectors were all
    // allocated before clone3 and remain owned by `plan` in this child memory
    // image. AT_EMPTY_PATH binds execution to the exact admitted object and
    // performs no second pathname lookup.
    libc::syscall(
        libc::SYS_execveat,
        plan.image.executable.as_raw_fd(),
        c"".as_ptr(),
        plan.image.argvp.as_ptr() as *const *const libc::c_char,
        plan.image.envp.as_ptr() as *const *const libc::c_char,
        libc::AT_EMPTY_PATH,
    );
    linux_child_fail(
        plan.exec_error_write.as_raw_fd(),
        LINUX_CHILD_STAGE_EXECVE,
        linux_child_errno(),
    )
}

#[cfg(target_os = "linux")]
fn linux_child_stage_name(stage: i32) -> &'static str {
    match stage {
        LINUX_CHILD_STAGE_PDEATHSIG => "PR_SET_PDEATHSIG",
        LINUX_CHILD_STAGE_PARENT => "parent verification",
        LINUX_CHILD_STAGE_STDIN => "stdin wiring",
        LINUX_CHILD_STAGE_STDOUT => "stdout wiring",
        LINUX_CHILD_STAGE_STDERR => "stderr wiring",
        LINUX_CHILD_STAGE_CLOSE_RANGE => "close_range",
        LINUX_CHILD_STAGE_SIGPIPE => "SIGPIPE reset",
        LINUX_CHILD_STAGE_EXECVE => "execveat",
        LINUX_CHILD_STAGE_PARENT_GATE => "parent pidfd ownership gate",
        LINUX_CHILD_STAGE_NO_NEW_PRIVS => "PR_SET_NO_NEW_PRIVS",
        LINUX_CHILD_STAGE_EXECUTABLE_PRIVILEGE => "executable privilege recheck",
        _ => "unknown child stage",
    }
}

#[cfg(target_os = "linux")]
fn linux_poll_timeout(deadline: Instant) -> libc::c_int {
    deadline
        .saturating_duration_since(Instant::now())
        .as_millis()
        .max(1)
        .min(libc::c_int::MAX as u128) as libc::c_int
}

#[cfg(target_os = "linux")]
fn linux_wait_exec_result_until(
    error_fd: &OwnedFd,
    pidfd: &OwnedFd,
    deadline: Instant,
) -> io::Result<()> {
    let mut bytes = [0_u8; std::mem::size_of::<LinuxChildError>()];
    let mut filled = 0_usize;
    loop {
        loop {
            let count = unsafe {
                libc::read(
                    error_fd.as_raw_fd(),
                    bytes[filled..].as_mut_ptr().cast(),
                    bytes.len() - filled,
                )
            };
            if count > 0 {
                filled += count as usize;
                if filled == bytes.len() {
                    let error = LinuxChildError {
                        stage: i32::from_ne_bytes(bytes[..4].try_into().unwrap()),
                        errno: i32::from_ne_bytes(bytes[4..].try_into().unwrap()),
                    };
                    return Err(io::Error::other(format!(
                        "private-worker child failed at {}: {}",
                        linux_child_stage_name(error.stage),
                        io::Error::from_raw_os_error(error.errno)
                    )));
                }
                continue;
            }
            if count == 0 {
                if filled != 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "private-worker child returned a partial exec-error record",
                    ));
                }
                let observation_deadline = deadline.min(Instant::now() + Duration::from_millis(5));
                let mut process = libc::pollfd {
                    fd: pidfd.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                loop {
                    let result = unsafe {
                        libc::poll(&mut process, 1, linux_poll_timeout(observation_deadline))
                    };
                    if result >= 0 {
                        break;
                    }
                    if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                        break;
                    }
                    if Instant::now() >= observation_deadline {
                        break;
                    }
                }
                if linux_try_reap_pidfd(pidfd)? {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "private worker exited during launch",
                    ));
                }
                return Ok(());
            }
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            if error.kind() == io::ErrorKind::WouldBlock {
                break;
            }
            return Err(error);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "private-worker exec exceeded its startup deadline",
            ));
        }
        let mut poll_fds = [
            libc::pollfd {
                fd: error_fd.as_raw_fd(),
                events: libc::POLLIN | libc::POLLHUP,
                revents: 0,
            },
            libc::pollfd {
                fd: pidfd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let result = unsafe {
            libc::poll(
                poll_fds.as_mut_ptr(),
                poll_fds.len() as _,
                linux_poll_timeout(deadline),
            )
        };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if result == 0 {
            continue;
        }
        if poll_fds[1].revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) != 0
            && poll_fds[0].revents == 0
        {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "private worker exited before exec confirmation",
            ));
        }
    }
}

#[cfg(target_os = "linux")]
fn linux_clone3_exec_until(
    plan: LinuxSpawnPlan,
    exec_deadline: Instant,
) -> Result<(WorkerChild, OwnedFd, OwnedFd), LinuxSpawnFailure> {
    if Instant::now() >= exec_deadline {
        return Err(LinuxSpawnFailure::before_clone(io::Error::new(
            io::ErrorKind::TimedOut,
            "private-worker clone3 admission exceeded its startup deadline",
        )));
    }
    #[cfg(test)]
    if let Some(errno) = plan.test_clone3_errno {
        return Err(LinuxSpawnFailure::before_clone(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "Linux clone3(CLONE_PIDFD) is unavailable or denied: {}",
                io::Error::from_raw_os_error(errno)
            ),
        )));
    }
    let mut pidfd_descriptor = -1_i32;
    let mut arguments = LinuxCloneArgs {
        flags: libc::CLONE_PIDFD as u64,
        pidfd: (&mut pidfd_descriptor as *mut i32) as u64,
        child_tid: 0,
        parent_tid: 0,
        exit_signal: libc::SIGCHLD as u64,
        stack: 0,
        stack_size: 0,
        tls: 0,
        set_tid: 0,
        set_tid_size: 0,
        cgroup: 0,
    };
    let child_pid = unsafe {
        libc::syscall(
            libc::SYS_clone3,
            &mut arguments as *mut LinuxCloneArgs,
            std::mem::size_of::<LinuxCloneArgs>(),
        )
    };
    if child_pid == 0 {
        unsafe { linux_child_exec(&plan) }
    }
    if child_pid < 0 {
        let error = io::Error::last_os_error();
        let reason = if matches!(
            error.raw_os_error(),
            Some(libc::ENOSYS) | Some(libc::EINVAL) | Some(libc::EPERM)
        ) {
            format!("Linux clone3(CLONE_PIDFD) is unavailable or denied: {error}")
        } else {
            format!("clone3(CLONE_PIDFD) failed: {error}")
        };
        return Err(LinuxSpawnFailure::before_clone(io::Error::new(
            error.kind(),
            reason,
        )));
    }
    if pidfd_descriptor < 0 {
        return Err(LinuxSpawnFailure::before_clone(io::Error::other(
            "clone3(CLONE_PIDFD) returned an invalid child identity",
        )));
    }
    // SAFETY: successful CLONE_PIDFD installed one fresh descriptor into the
    // supplied integer. If the embedding host started with closed stdio, the
    // kernel may select 0, 1, or 2. That temporarily occupies an already-free
    // slot without replacing any host descriptor; duplicate it above stdio
    // and close only this SDK-owned original. Never reserve and later close a
    // numeric stdio slot, because another host thread could reuse it first.
    let original_pidfd = unsafe { OwnedFd::from_raw_fd(pidfd_descriptor) };
    let pidfd = match linux_duplicate_pidfd(&original_pidfd) {
        Ok(relocated) => {
            drop(original_pidfd);
            relocated
        }
        Err(error) => {
            return Err(LinuxSpawnFailure::after_clone(
                io::Error::new(
                    error.kind(),
                    format!("relocate atomic private-worker pidfd above stdio: {error}"),
                ),
                original_pidfd,
            ));
        }
    };
    if child_pid > u32::MAX as libc::c_long {
        return Err(LinuxSpawnFailure::after_clone(
            io::Error::other("clone3(CLONE_PIDFD) returned an invalid numeric child identity"),
            pidfd,
        ));
    }
    let cleanup_pidfd = match linux_duplicate_pidfd(&pidfd) {
        Ok(duplicate) => duplicate,
        Err(error) => {
            return Err(LinuxSpawnFailure::after_clone(
                io::Error::new(
                    error.kind(),
                    format!("duplicate atomic private-worker pidfd: {error}"),
                ),
                pidfd,
            ));
        }
    };
    match linux_try_reap_pidfd(&pidfd) {
        Ok(false) => {}
        Ok(true) => {
            return Err(LinuxSpawnFailure::after_clone(
                io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "private-worker child exited before pidfd lifecycle admission",
                ),
                cleanup_pidfd,
            ));
        }
        Err(error) => {
            drop(plan.parent_gate_parent);
            return Err(LinuxSpawnFailure::after_clone(
                io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("Linux waitid(P_PIDFD) support or policy access is required: {error}"),
                ),
                cleanup_pidfd,
            ));
        }
    }
    if let Err(error) = linux_signal_process(&pidfd, 0) {
        drop(plan.parent_gate_parent);
        return Err(LinuxSpawnFailure::after_clone(
            io::Error::new(
                io::ErrorKind::Unsupported,
                format!("Linux pidfd_send_signal support or policy access is required: {error}"),
            ),
            cleanup_pidfd,
        ));
    }
    drop(plan.parent_gate_child);
    let permission = 1_u8;
    let permitted = unsafe {
        libc::send(
            plan.parent_gate_parent.as_raw_fd(),
            (&permission as *const u8).cast(),
            1,
            libc::MSG_NOSIGNAL,
        )
    };
    if permitted != 1 {
        let error = if permitted < 0 {
            io::Error::last_os_error()
        } else {
            io::Error::from_raw_os_error(libc::EIO)
        };
        return Err(LinuxSpawnFailure::after_clone(
            io::Error::new(
                error.kind(),
                format!("release private-worker atomic pidfd ownership gate: {error}"),
            ),
            cleanup_pidfd,
        ));
    }
    drop(plan.parent_gate_parent);

    drop(plan.stdin_child);
    drop(plan.stdout_child);
    drop(plan.stderr_child);
    drop(plan.exec_error_write);
    if let Err(error) = linux_wait_exec_result_until(&plan.exec_error_read, &pidfd, exec_deadline) {
        return Err(LinuxSpawnFailure::after_clone(error, cleanup_pidfd));
    }
    drop(plan.exec_error_read);
    Ok((
        WorkerChild {
            process_id: child_pid as u32,
            stdin: Some(std::fs::File::from(plan.stdin_parent)),
            stdout: Some(std::fs::File::from(plan.stdout_parent)),
        },
        pidfd,
        cleanup_pidfd,
    ))
}

#[cfg(target_os = "linux")]
fn linux_send_until<T>(
    sender: &std::sync::mpsc::SyncSender<T>,
    mut value: T,
    deadline: Instant,
) -> Result<(), T> {
    loop {
        if Instant::now() >= deadline {
            return Err(value);
        }
        match sender.try_send(value) {
            Ok(()) => return Ok(()),
            Err(std::sync::mpsc::TrySendError::Full(returned)) => {
                value = returned;
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(value);
                }
                std::thread::sleep(Duration::from_millis(1).min(remaining));
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(returned)) => return Err(returned),
        }
    }
}

#[cfg(all(target_os = "linux", test))]
fn linux_finish_exact_cleanup(
    cleanup_pidfd: OwnedFd,
    permit: LinuxReapPermit,
    control: Option<LinuxCleanupTestControl>,
) {
    if let Some(control) = control {
        let _ = control.started.send(cleanup_pidfd.as_raw_fd());
        let deadline = if control.force_registry {
            Instant::now()
        } else {
            Instant::now() + LINUX_EXACT_REAP_GRACE
        };
        let reaped = linux_finish_exact_cleanup_bounded(
            cleanup_pidfd,
            permit,
            deadline,
            Some(control.reaped),
        );
        let _ = control.finished.send(reaped);
        return;
    }
    let _ = linux_finish_exact_cleanup_bounded(
        cleanup_pidfd,
        permit,
        Instant::now() + LINUX_EXACT_REAP_GRACE,
        None,
    );
}

#[cfg(all(target_os = "linux", not(test)))]
fn linux_finish_exact_cleanup(cleanup_pidfd: OwnedFd, permit: LinuxReapPermit) {
    let _ = linux_finish_exact_cleanup_bounded(
        cleanup_pidfd,
        permit,
        Instant::now() + LINUX_EXACT_REAP_GRACE,
        None,
    );
}

#[cfg(target_os = "linux")]
fn linux_finish_exact_cleanup_bounded(
    cleanup_pidfd: OwnedFd,
    permit: LinuxReapPermit,
    deadline: Instant,
    completion: Option<std::sync::mpsc::SyncSender<()>>,
) -> bool {
    if linux_terminate_and_reap_until(&cleanup_pidfd, deadline).unwrap_or(false) {
        if let Some(sender) = completion {
            let _ = sender.send(());
        }
        return true;
    }
    linux_register_pending_reap(LinuxPendingReap {
        pidfd: cleanup_pidfd,
        _permit: permit,
        completion,
    });
    false
}

#[cfg(target_os = "linux")]
#[allow(unused_mut)]
fn spawn_contained_worker_until(
    mut plan: LinuxSpawnPlan,
    deadline: Instant,
) -> io::Result<(WorkerChild, HostDeathGuard)> {
    #[cfg(test)]
    let abandon_handoff = plan.test_abandon_handoff;
    #[cfg(test)]
    let mut cleanup_control = plan.test_cleanup_control.take();
    if Instant::now() >= deadline {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "private-worker spawn exceeded its startup deadline",
        ));
    }
    let reap_permit = linux_reserve_reap_permit(deadline)?;
    let (result_tx, result_rx) = std::sync::mpsc::sync_channel(0);
    let (ack_tx, ack_rx) = std::sync::mpsc::sync_channel(0);
    let (finish_tx, finish_rx) = std::sync::mpsc::sync_channel(0);
    let parent_thread = std::thread::Builder::new()
        .name("cua-private-worker-parent".into())
        .spawn(move || {
            match linux_clone3_exec_until(plan, deadline) {
                Ok((child, pidfd, cleanup_pidfd)) => {
                    match linux_send_until(&result_tx, Ok((child, pidfd)), deadline) {
                        Ok(()) => {
                            let remaining = deadline.saturating_duration_since(Instant::now());
                            if !remaining.is_zero() && ack_rx.recv_timeout(remaining).is_ok() {
                                // PDEATHSIG ownership is thread-scoped, so this parent
                                // remains for the admitted worker lifetime. Release
                                // starts bounded exact cleanup; it never enters a
                                // blocking waitid or an unbounded cleanup loop.
                                let _ = finish_rx.recv();
                            }
                        }
                        Err(Ok((mut child, _pidfd))) => {
                            child.stdin.take();
                            child.stdout.take();
                        }
                        Err(Err(_)) => unreachable!("successful launch result changed variant"),
                    }
                    #[cfg(test)]
                    linux_finish_exact_cleanup(cleanup_pidfd, reap_permit, cleanup_control.take());
                    #[cfg(not(test))]
                    linux_finish_exact_cleanup(cleanup_pidfd, reap_permit);
                }
                Err(mut failure) => {
                    // Report the bounded launch result before attempting the
                    // finite exact-child cleanup phase.
                    let _ = linux_send_until(&result_tx, Err(failure.error), deadline);
                    if let Some(cleanup_pidfd) = failure.cleanup_pidfd.take() {
                        #[cfg(test)]
                        linux_finish_exact_cleanup(
                            cleanup_pidfd,
                            reap_permit,
                            cleanup_control.take(),
                        );
                        #[cfg(not(test))]
                        linux_finish_exact_cleanup(cleanup_pidfd, reap_permit);
                    }
                }
            }
        })?;

    #[cfg(test)]
    if abandon_handoff {
        drop(result_rx);
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "private-worker ownership handoff was abandoned by test",
        ));
    }
    let remaining = deadline.saturating_duration_since(Instant::now());
    let result = if remaining.is_zero() {
        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
    } else {
        result_rx.recv_timeout(remaining)
    };
    let (mut child, pidfd) = match result {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => return Err(error),
        Err(error) => {
            drop(result_rx);
            return Err(match error {
                std::sync::mpsc::RecvTimeoutError::Timeout => io::Error::new(
                    io::ErrorKind::TimedOut,
                    "private-worker spawn exceeded its startup deadline",
                ),
                std::sync::mpsc::RecvTimeoutError::Disconnected => {
                    io::Error::other("private-worker parent thread exited without a result")
                }
            });
        }
    };
    let guard = HostDeathGuard {
        finish: Some(finish_tx),
        parent_thread: Some(parent_thread),
        pidfd: Some(pidfd),
    };
    if linux_send_until(&ack_tx, (), deadline).is_err() {
        child.stdin.take();
        child.stdout.take();
        drop(guard);
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "private-worker ownership acknowledgement exceeded its startup deadline",
        ));
    }
    if Instant::now() >= deadline {
        child.stdin.take();
        child.stdout.take();
        drop(guard);
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "private-worker spawn completed after its startup deadline",
        ));
    }
    Ok((child, guard))
}

#[cfg(target_os = "windows")]
fn spawn_contained_worker(
    command: Command,
    inherit_stderr: bool,
) -> std::io::Result<(WorkerChild, HostDeathGuard)> {
    use core::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use windows::Win32::Foundation::{
        CloseHandle, DuplicateHandle, SetHandleInformation, BOOL, DUPLICATE_SAME_ACCESS, HANDLE,
        HANDLE_FLAG_INHERIT, TRUE,
    };
    use windows::Win32::Security::SECURITY_ATTRIBUTES;
    use windows::Win32::System::Pipes::CreatePipe;
    use windows::Win32::System::Threading::{
        CreateProcessW, DeleteProcThreadAttributeList, GetCurrentProcess,
        InitializeProcThreadAttributeList, UpdateProcThreadAttribute, CREATE_UNICODE_ENVIRONMENT,
        EXTENDED_STARTUPINFO_PRESENT, LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION,
        PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_JOB_LIST, STARTF_USESTDHANDLES,
        STARTUPINFOEXW,
    };

    struct TemporaryHandle(HANDLE);
    impl Drop for TemporaryHandle {
        fn drop(&mut self) {
            if !self.0.is_invalid() {
                // SAFETY: TemporaryHandle uniquely owns this handle.
                unsafe {
                    let _ = CloseHandle(self.0);
                }
            }
        }
    }
    impl TemporaryHandle {
        fn into_file(mut self) -> std::fs::File {
            let raw = self.0 .0;
            self.0 = HANDLE::default();
            // SAFETY: ownership transfers from TemporaryHandle to File.
            unsafe { std::fs::File::from_raw_handle(raw) }
        }
    }

    struct AttributeList(LPPROC_THREAD_ATTRIBUTE_LIST);
    impl Drop for AttributeList {
        fn drop(&mut self) {
            if !self.0 .0.is_null() {
                // SAFETY: this list was initialized once and remains backed by
                // attribute_storage until AttributeList is dropped.
                unsafe { DeleteProcThreadAttributeList(self.0) };
            }
        }
    }

    let mut security = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: TRUE,
    };
    let mut stdin_read = HANDLE::default();
    let mut stdin_write = HANDLE::default();
    let mut stdout_read = HANDLE::default();
    let mut stdout_write = HANDLE::default();
    // SAFETY: all output pointers and SECURITY_ATTRIBUTES are valid.
    unsafe {
        CreatePipe(&mut stdin_read, &mut stdin_write, Some(&mut security), 0)
            .map_err(io::Error::other)?;
    }
    let stdin_read = TemporaryHandle(stdin_read);
    let stdin_write = TemporaryHandle(stdin_write);
    // SAFETY: all output pointers and SECURITY_ATTRIBUTES are valid.
    unsafe {
        CreatePipe(&mut stdout_read, &mut stdout_write, Some(&mut security), 0)
            .map_err(io::Error::other)?;
    }
    let stdout_read = TemporaryHandle(stdout_read);
    let stdout_write = TemporaryHandle(stdout_write);
    // Parent-side pipe ends must never be inherited by the child.
    unsafe {
        SetHandleInformation(stdin_write.0, HANDLE_FLAG_INHERIT.0, Default::default())
            .map_err(io::Error::other)?;
        SetHandleInformation(stdout_read.0, HANDLE_FLAG_INHERIT.0, Default::default())
            .map_err(io::Error::other)?;
    }

    let stderr_file = if inherit_stderr {
        None
    } else {
        Some(
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("NUL")?,
        )
    };
    let stderr_source = if let Some(file) = stderr_file.as_ref() {
        HANDLE(file.as_raw_handle())
    } else {
        HANDLE(std::io::stderr().as_raw_handle())
    };
    let current_process = unsafe { GetCurrentProcess() };
    let mut stderr_duplicate = HANDLE::default();
    // Duplicate instead of changing inheritability on the host's shared stderr.
    unsafe {
        DuplicateHandle(
            current_process,
            stderr_source,
            current_process,
            &mut stderr_duplicate,
            0,
            TRUE,
            DUPLICATE_SAME_ACCESS,
        )
        .map_err(io::Error::other)?;
    }
    let stderr_duplicate = TemporaryHandle(stderr_duplicate);

    let guard = create_host_death_job()?;
    let job = HANDLE(guard.0 as *mut c_void);
    let inherited_handles = [stdin_read.0, stdout_write.0, stderr_duplicate.0];
    let jobs = [job];
    let mut attribute_bytes = 0_usize;
    // The sizing call intentionally fails with ERROR_INSUFFICIENT_BUFFER while
    // returning the required opaque-list size.
    unsafe {
        let _ = InitializeProcThreadAttributeList(
            LPPROC_THREAD_ATTRIBUTE_LIST(std::ptr::null_mut()),
            2,
            0,
            &mut attribute_bytes,
        );
    }
    if attribute_bytes == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut attribute_storage =
        vec![0_usize; attribute_bytes.div_ceil(std::mem::size_of::<usize>())];
    let attribute_list = LPPROC_THREAD_ATTRIBUTE_LIST(attribute_storage.as_mut_ptr().cast());
    unsafe {
        InitializeProcThreadAttributeList(attribute_list, 2, 0, &mut attribute_bytes)
            .map_err(io::Error::other)?;
    }
    let attribute_list = AttributeList(attribute_list);
    unsafe {
        UpdateProcThreadAttribute(
            attribute_list.0,
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            Some(inherited_handles.as_ptr().cast()),
            std::mem::size_of_val(&inherited_handles),
            None,
            None,
        )
        .map_err(io::Error::other)?;
        UpdateProcThreadAttribute(
            attribute_list.0,
            0,
            PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
            Some(jobs.as_ptr().cast()),
            std::mem::size_of_val(&jobs),
            None,
            None,
        )
        .map_err(io::Error::other)?;
    }

    let mut application = command.get_program().encode_wide().collect::<Vec<_>>();
    if application.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "private-worker executable path contains NUL",
        ));
    }
    application.push(0);
    let mut command_line = Vec::new();
    quote_windows_argument(command.get_program(), &mut command_line)?;
    for argument in command.get_args() {
        command_line.push(b' ' as u16);
        quote_windows_argument(argument, &mut command_line)?;
    }
    command_line.push(0);

    let mut environment = command
        .get_envs()
        .filter_map(|(name, value)| value.map(|value| (name, value)))
        .map(|(name, value)| {
            let name = name.encode_wide().collect::<Vec<_>>();
            let value = value.encode_wide().collect::<Vec<_>>();
            if name.contains(&0) || value.contains(&0) || name.contains(&(b'=' as u16)) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "private-worker environment contains an invalid Windows entry",
                ));
            }
            Ok((name, value))
        })
        .collect::<io::Result<Vec<_>>>()?;
    environment.sort_by(|(left, _), (right, _)| {
        left.iter()
            .copied()
            .map(|unit| {
                if (b'A' as u16..=b'Z' as u16).contains(&unit) {
                    unit + (b'a' - b'A') as u16
                } else {
                    unit
                }
            })
            .cmp(right.iter().copied().map(|unit| {
                if (b'A' as u16..=b'Z' as u16).contains(&unit) {
                    unit + (b'a' - b'A') as u16
                } else {
                    unit
                }
            }))
    });
    let mut environment_block = Vec::new();
    for (name, value) in environment {
        environment_block.extend(name);
        environment_block.push(b'=' as u16);
        environment_block.extend(value);
        environment_block.push(0);
    }
    environment_block.push(0);
    if environment_block.len() == 1 {
        environment_block.push(0);
    }

    let mut startup = STARTUPINFOEXW::default();
    startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = stdin_read.0;
    startup.StartupInfo.hStdOutput = stdout_write.0;
    startup.StartupInfo.hStdError = stderr_duplicate.0;
    startup.lpAttributeList = attribute_list.0;
    let mut process = PROCESS_INFORMATION::default();
    let created = unsafe {
        CreateProcessW(
            windows::core::PCWSTR(application.as_ptr()),
            windows::core::PWSTR(command_line.as_mut_ptr()),
            None,
            None,
            BOOL(1),
            EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
            Some(environment_block.as_ptr().cast()),
            windows::core::PCWSTR::null(),
            &startup.StartupInfo,
            &mut process,
        )
    };
    created.map_err(io::Error::other)?;
    // The process handle is retained; the initial thread handle is not needed.
    unsafe {
        let _ = CloseHandle(process.hThread);
    }
    drop(stdin_read);
    drop(stdout_write);
    drop(stderr_duplicate);
    Ok((
        WorkerChild {
            process: process.hProcess.0 as usize,
            process_id: process.dwProcessId,
            stdin: Some(stdin_write.into_file()),
            stdout: Some(stdout_read.into_file()),
        },
        guard,
    ))
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn spawn_contained_worker(
    mut command: Command,
    _inherit_stderr: bool,
) -> std::io::Result<(Child, HostDeathGuard)> {
    command.spawn().map(|child| (child, ()))
}

#[cfg(not(target_os = "linux"))]
fn spawn_contained_worker_until(
    command: Command,
    inherit_stderr: bool,
    deadline: Instant,
) -> std::io::Result<(WorkerChild, HostDeathGuard)> {
    if Instant::now() >= deadline {
        return Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "private-worker spawn deadline expired",
        ));
    }
    let (sender, receiver) = std::sync::mpsc::sync_channel(0);
    std::thread::Builder::new()
        .name("cua-private-worker-spawn".into())
        .spawn(move || {
            let result = spawn_contained_worker(command, inherit_stderr);
            if let Err(error) = sender.send(result) {
                if let Ok((mut child, guard)) = error.0 {
                    child.stdin.take();
                    child.stdout.take();
                    // On Windows this closes the kill-on-close Job before
                    // waiting for the now-contained process.
                    #[cfg(target_os = "windows")]
                    drop(guard);
                    #[cfg(not(target_os = "windows"))]
                    let _ = guard;
                    let _ = child.wait();
                }
            }
        })?;
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "private-worker spawn exceeded its startup deadline",
        ));
    }
    receiver
        .recv_timeout(remaining)
        .map_err(|error| match error {
            std::sync::mpsc::RecvTimeoutError::Timeout => std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "private-worker spawn exceeded its startup deadline",
            ),
            std::sync::mpsc::RecvTimeoutError::Disconnected => {
                std::io::Error::other("private-worker spawn thread exited without a result")
            }
        })?
}

struct WorkerProcess {
    child: Option<WorkerChild>,
    #[cfg(all(not(target_os = "linux"), not(target_os = "windows")))]
    stdin: Option<ChildStdin>,
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    stdin: Option<std::fs::File>,
    #[cfg(all(not(target_os = "linux"), not(target_os = "windows")))]
    stdout: Option<BufReader<ChildStdout>>,
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    stdout: Option<BufReader<std::fs::File>>,
    #[cfg(target_os = "linux")]
    worker_pidfd: Arc<OwnedFd>,
    #[cfg(target_os = "macos")]
    worker_termination: Arc<MacosWorkerTermination>,
    stopped: bool,
}

#[cfg(target_os = "macos")]
struct MacosWorkerTermination {
    stream: Mutex<Option<UnixStream>>,
}

#[cfg(target_os = "macos")]
struct MacosWorkerTerminationEndpoint {
    listener: UnixListener,
    socket_path: PathBuf,
    directory: PathBuf,
}

#[cfg(target_os = "macos")]
impl MacosWorkerTermination {
    fn terminate(&self) {
        // Closing this SDK-owned capability wakes the worker's dedicated
        // watchdog, which exits that exact process. No numeric PID is used.
        if let Some(stream) = self
            .stream
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }
}

#[cfg(target_os = "macos")]
impl MacosWorkerTerminationEndpoint {
    fn bind(generation: &str) -> io::Result<Self> {
        let directory = PathBuf::from(format!("/tmp/cua-private-worker-{generation}"));
        std::fs::DirBuilder::new().mode(0o700).create(&directory)?;
        let socket_path = directory.join("terminate.sock");
        let listener = match UnixListener::bind(&socket_path) {
            Ok(listener) => listener,
            Err(error) => {
                let _ = std::fs::remove_dir(&directory);
                return Err(error);
            }
        };
        if let Err(error) = listener.set_nonblocking(true) {
            let _ = std::fs::remove_file(&socket_path);
            let _ = std::fs::remove_dir(&directory);
            return Err(error);
        }
        Ok(Self {
            listener,
            socket_path,
            directory,
        })
    }

    fn socket_path(&self) -> &std::path::Path {
        &self.socket_path
    }

    fn accept_until(
        self,
        deadline: Instant,
        generation: &str,
        expected_worker_pid: u32,
    ) -> io::Result<Arc<MacosWorkerTermination>> {
        let (mut stream, _) = loop {
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "private-worker termination capability handshake timed out",
                ));
            }
            match self.listener.accept() {
                Ok((stream, address)) => {
                    let mut peer_pid: libc::pid_t = 0;
                    let mut length = std::mem::size_of_val(&peer_pid) as libc::socklen_t;
                    // SAFETY: getsockopt writes at most `length` bytes into the
                    // live pid buffer and retains no pointer.
                    let result = unsafe {
                        libc::getsockopt(
                            stream.as_raw_fd(),
                            libc::SOL_LOCAL,
                            libc::LOCAL_PEERPID,
                            (&mut peer_pid as *mut libc::pid_t).cast(),
                            &mut length,
                        )
                    };
                    if result < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    if peer_pid > 1 && peer_pid as u32 == expected_worker_pid {
                        break (stream, address);
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "private-worker termination capability handshake timed out",
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(5).min(remaining));
                }
                Err(error) => return Err(error),
            }
        };

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "private-worker termination capability attestation timed out",
            ));
        }
        let mut attestation = Vec::with_capacity(generation.len() + 1);
        loop {
            if attestation.len() > generation.len() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "private-worker termination capability attestation was invalid",
                ));
            }
            // A socket read timeout is per operation. Recompute it before
            // every byte so a peer cannot extend the absolute startup budget
            // indefinitely by trickling an otherwise valid attestation.
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "private-worker termination capability attestation timed out",
                ));
            }
            stream.set_read_timeout(Some(remaining))?;
            let mut byte = [0_u8; 1];
            std::io::Read::read_exact(&mut stream, &mut byte)?;
            if byte[0] == b'\n' {
                break;
            }
            attestation.push(byte[0]);
        }
        if attestation != generation.as_bytes() || Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "private-worker termination capability attestation was invalid or late",
            ));
        }
        stream.set_read_timeout(None)?;
        Ok(Arc::new(MacosWorkerTermination {
            stream: Mutex::new(Some(stream)),
        }))
    }
}

#[cfg(target_os = "macos")]
impl Drop for MacosWorkerTerminationEndpoint {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket_path);
        let _ = std::fs::remove_dir(&self.directory);
    }
}

enum WorkerExchangeError {
    Write(std::io::Error),
    Read(std::io::Error),
}

fn forced_shutdown_result(
    response: Result<Value, DriverError>,
    reason: impl Into<String>,
) -> Result<(), DriverError> {
    match response {
        Err(error) => Err(error),
        Ok(_) => Err(DriverError::ActionInterrupted {
            completion: ActionCompletion::Unknown,
            reason: reason.into(),
        }),
    }
}

type ReapTask = Box<dyn FnOnce() + Send + 'static>;

impl WorkerProcess {
    fn try_reap(&mut self) -> io::Result<bool> {
        if self.child.is_none() {
            return Ok(true);
        }
        #[cfg(target_os = "linux")]
        {
            linux_try_reap_pidfd(&self.worker_pidfd)
        }
        #[cfg(not(target_os = "linux"))]
        {
            self.child
                .as_mut()
                .expect("child presence checked above")
                .try_wait()
                .map(|status| status.is_some())
        }
    }

    fn stop_and_reap_until(&mut self, deadline: Instant) -> bool {
        self.stdin.take();
        self.stdout.take();
        if self.child.is_none() {
            self.stopped = true;
            return true;
        }
        match self.try_reap() {
            Ok(true) => {
                self.child.take();
                self.stopped = true;
                return true;
            }
            Err(_) => {
                self.child.take();
                self.stopped = true;
                return true;
            }
            Ok(false) => {}
        }
        #[cfg(target_os = "linux")]
        let _ = linux_kill_process(&self.worker_pidfd);
        #[cfg(target_os = "macos")]
        self.worker_termination.terminate();
        #[cfg(any(target_os = "windows", not(unix)))]
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
        }
        loop {
            match self.try_reap() {
                Ok(true) => {
                    self.child.take();
                    self.stopped = true;
                    return true;
                }
                Err(_) => {
                    self.child.take();
                    self.stopped = true;
                    return true;
                }
                Ok(false) if Instant::now() < deadline => {
                    std::thread::sleep(
                        Duration::from_millis(5)
                            .min(deadline.saturating_duration_since(Instant::now())),
                    );
                }
                Ok(false) => return false,
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn stop_and_reap_blocking(&mut self) {
        self.stdin.take();
        self.stdout.take();
        if let Some(mut child) = self.child.take() {
            match child.try_wait() {
                Ok(None) => {
                    #[cfg(target_os = "macos")]
                    self.worker_termination.terminate();
                    #[cfg(any(target_os = "windows", not(unix)))]
                    let _ = child.kill();
                }
                Ok(Some(_)) | Err(_) => {}
            }
            let _ = child.wait();
        }
        self.stopped = true;
    }
}

fn validate_readiness(
    ready: &Value,
    worker_pid: u32,
    host_bundle_id: &str,
    deadline: Instant,
) -> Result<(), DriverError> {
    ensure_startup_deadline(deadline)?;
    if ready.get("ready").and_then(Value::as_bool) != Some(true)
        || ready.get("pid").and_then(Value::as_u64) != Some(worker_pid as u64)
        || ready.get("host_bundle_id").and_then(Value::as_str) != Some(host_bundle_id)
        || ready.get("environment_verified").and_then(Value::as_bool) != Some(true)
    {
        return Err(DriverError::Protocol {
            reason: "private worker readiness proof did not match the spawned host generation"
                .into(),
        });
    }
    // The channel response has already been parsed under the request deadline.
    // Inspect the metadata object in place: cloning/deserializing a worker-
    // controlled subtree here could perform another near-64-MiB traversal
    // after the startup budget had expired.
    let metadata = ready
        .get("metadata")
        .and_then(Value::as_object)
        .ok_or_else(|| DriverError::Protocol {
            reason: "private worker readiness proof omitted compatibility metadata".into(),
        })?;
    let metadata_pid = metadata.get("pid").and_then(Value::as_u64);
    let mismatch = if metadata
        .get("driver_version")
        .and_then(Value::as_str)
        .is_none()
    {
        Some("metadata omitted a string driver version".into())
    } else if metadata_pid != Some(worker_pid as u64) {
        Some(format!(
            "metadata PID {metadata_pid:?} does not match spawned worker {worker_pid}"
        ))
    } else if metadata.get("embedded").and_then(Value::as_bool) != Some(true) {
        Some("private worker metadata did not identify an embedded runtime".into())
    } else if metadata.get("host_bundle_id").and_then(Value::as_str) != Some(host_bundle_id) {
        Some(format!(
            "metadata host bundle id {:?} does not match {host_bundle_id}",
            metadata.get("host_bundle_id")
        ))
    } else if metadata.get("contract_version").and_then(Value::as_str)
        != Some(cua_driver_contract::CONTRACT_VERSION)
    {
        Some(format!(
            "contract version {:?} does not match SDK {}",
            metadata.get("contract_version"),
            cua_driver_contract::CONTRACT_VERSION
        ))
    } else if metadata
        .get("tools_list_schema_version")
        .and_then(Value::as_str)
        != Some(cua_driver_contract::TOOLS_LIST_SCHEMA_VERSION)
    {
        Some(format!(
            "tools-list schema version {:?} does not match SDK {}",
            metadata.get("tools_list_schema_version"),
            cua_driver_contract::TOOLS_LIST_SCHEMA_VERSION
        ))
    } else if metadata.get("capability_version").and_then(Value::as_str)
        != Some(cua_driver_contract::CAPABILITY_VERSION)
    {
        Some(format!(
            "capability version {:?} does not match SDK {}",
            metadata.get("capability_version"),
            cua_driver_contract::CAPABILITY_VERSION
        ))
    } else if metadata.get("mcp_protocol_version").and_then(Value::as_str)
        != Some(cua_driver_contract::MCP_PROTOCOL_VERSION)
    {
        Some(format!(
            "MCP protocol version {:?} does not match SDK {}",
            metadata.get("mcp_protocol_version"),
            cua_driver_contract::MCP_PROTOCOL_VERSION
        ))
    } else {
        None
    };
    ensure_startup_deadline(deadline)?;
    if let Some(reason) = mismatch {
        Err(DriverError::Protocol {
            reason: format!("incompatible private worker readiness metadata: {reason}"),
        })
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn exchange_process_binding_until(
    child: &mut WorkerChild,
    generation: &str,
    deadline: Instant,
) -> io::Result<String> {
    let nonce = Uuid::new_v4().to_string();
    let challenge = PrivateWorkerBinding {
        protocol_version: PRIVATE_WORKER_PROTOCOL_VERSION,
        generation: generation.to_owned(),
        nonce: nonce.clone(),
        worker_pid: child.id(),
    };
    let line = encode_private_worker_message_until(&challenge, deadline)?;
    let stdin = child.stdin.as_mut().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::BrokenPipe,
            "private worker stdin was not piped",
        )
    })?;
    stdin.write_all(&line)?;
    stdin.write_all(b"\n")?;
    stdin.flush()?;

    let stdout = child.stdout.as_ref().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::BrokenPipe,
            "private worker stdout was not piped",
        )
    })?;
    let descriptor = stdout.as_raw_fd();
    let mut response = Vec::with_capacity(256);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "private-worker process binding timed out",
            ));
        }
        let timeout_ms = remaining.as_millis().max(1).min(i32::MAX as u128) as i32;
        let mut poll_fd = libc::pollfd {
            fd: descriptor,
            events: libc::POLLIN | libc::POLLHUP,
            revents: 0,
        };
        // SAFETY: poll receives one live pollfd for the duration of the call.
        let polled = unsafe { libc::poll(&mut poll_fd, 1, timeout_ms) };
        if polled < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if polled == 0 {
            continue;
        }
        let mut buffer = [0_u8; 256];
        // SAFETY: read writes at most buffer.len() bytes to this live buffer.
        let count = unsafe { libc::read(descriptor, buffer.as_mut_ptr().cast(), buffer.len()) };
        if count < 0 {
            let error = io::Error::last_os_error();
            if matches!(
                error.kind(),
                io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
            ) {
                continue;
            }
            return Err(error);
        }
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "private worker exited before process binding",
            ));
        }
        response.extend_from_slice(&buffer[..count as usize]);
        if response.len() > 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "private-worker process binding exceeded 1024 bytes",
            ));
        }
        let Some(newline) = response.iter().position(|byte| *byte == b'\n') else {
            continue;
        };
        if newline + 1 != response.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "private worker wrote data beyond its process binding proof",
            ));
        }
        response.truncate(newline);
        let attestation: PrivateWorkerBinding = serde_json::from_slice(&response)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if attestation.protocol_version != PRIVATE_WORKER_PROTOCOL_VERSION
            || attestation.generation != generation
            || attestation.nonce != nonce
            || attestation.worker_pid != child.id()
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "private-worker process binding proof did not match its private channel challenge",
            ));
        }
        return Ok(nonce);
    }
}

#[cfg(not(target_os = "linux"))]
fn send_child_to_reaper(
    reap_sender: &std::sync::mpsc::SyncSender<ReapTask>,
    mut child: WorkerChild,
) {
    child.stdin.take();
    child.stdout.take();
    let _ = reap_sender.send(Box::new(move || {
        let _ = child.wait();
    }));
}

#[cfg(target_os = "linux")]
fn close_and_reap_unbound_worker(
    _reap_sender: &std::sync::mpsc::SyncSender<ReapTask>,
    mut child: WorkerChild,
) {
    child.stdin.take();
    child.stdout.take();
    drop(child);
}

#[cfg(target_os = "macos")]
fn close_and_reap_unbound_worker(
    _reap_sender: &std::sync::mpsc::SyncSender<ReapTask>,
    mut child: Child,
) {
    child.stdin.take();
    child.stdout.take();
    drop(child);
}

#[cfg(target_os = "linux")]
fn close_and_reap_bound_worker(
    _reap_sender: &std::sync::mpsc::SyncSender<ReapTask>,
    mut child: WorkerChild,
    handle: &Arc<OwnedFd>,
) {
    child.stdin.take();
    child.stdout.take();
    let _ = linux_kill_process(handle);
    drop(child);
    // The creator thread retains a duplicate pidfd and a pre-reserved registry
    // permit. Dropping HostDeathGuard releases that owner to bounded cleanup.
    let _ = linux_try_reap_pidfd(handle);
}

#[cfg(target_os = "macos")]
fn close_and_reap_bound_worker(
    reap_sender: &std::sync::mpsc::SyncSender<ReapTask>,
    child: Child,
    handle: &Arc<MacosWorkerTermination>,
) {
    handle.terminate();
    send_child_to_reaper(reap_sender, child);
}

#[cfg(target_os = "windows")]
fn close_and_reap_bound_worker(
    reap_sender: &std::sync::mpsc::SyncSender<ReapTask>,
    mut child: WorkerChild,
    _handle: &(),
) {
    let _ = child.kill();
    send_child_to_reaper(reap_sender, child);
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn close_and_reap_bound_worker(
    reap_sender: &std::sync::mpsc::SyncSender<ReapTask>,
    child: Child,
    _handle: &(),
) {
    send_child_to_reaper(reap_sender, child);
}

#[cfg(not(any(unix, target_os = "windows")))]
fn close_and_reap_bound_worker(
    reap_sender: &std::sync::mpsc::SyncSender<ReapTask>,
    mut child: Child,
    _handle: &(),
) {
    let _ = child.kill();
    send_child_to_reaper(reap_sender, child);
}

#[cfg(target_os = "linux")]
fn linux_try_reap_pidfd(handle: &OwnedFd) -> io::Result<bool> {
    loop {
        // SAFETY: siginfo is plain output storage initialized to zero; waitid
        // retains no pointers after returning. P_PIDFD binds waiting/reaping to
        // the immutable process object rather than a recyclable numeric PID.
        let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
        let result = unsafe {
            libc::waitid(
                libc::P_PIDFD,
                handle.as_raw_fd() as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG,
            )
        };
        if result == 0 {
            // si_pid is zero only for a successful WNOHANG query whose target
            // has not exited.
            return Ok(unsafe { info.si_pid() } != 0);
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        if error.raw_os_error() == Some(libc::ECHILD) {
            // A process-global waiter already reaped this exact pidfd target.
            return Ok(true);
        }
        return Err(error);
    }
}

#[cfg(target_os = "linux")]
fn linux_reap_pidfd_until(handle: &OwnedFd, deadline: Instant) -> io::Result<bool> {
    loop {
        if linux_try_reap_pidfd(handle)? {
            return Ok(true);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(false);
        }
        let mut descriptor = libc::pollfd {
            fd: handle.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let timeout = remaining
            .min(Duration::from_millis(10))
            .as_millis()
            .max(1)
            .min(libc::c_int::MAX as u128) as libc::c_int;
        let result = unsafe { libc::poll(&mut descriptor, 1, timeout) };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
    }
}

#[cfg(target_os = "linux")]
fn linux_terminate_and_reap_until(handle: &OwnedFd, deadline: Instant) -> io::Result<bool> {
    if let Err(error) = linux_kill_process(handle) {
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error);
        }
    }
    linux_reap_pidfd_until(handle, deadline)
}

#[cfg(target_os = "linux")]
fn linux_signal_process(handle: &OwnedFd, signal: libc::c_int) -> std::io::Result<()> {
    // pidfd_send_signal binds delivery to the opened process object. Unlike a
    // /proc start-time check followed by kill(pid), it cannot target a process
    // that later reuses the worker's numeric PID.
    // SAFETY: the pidfd is live, the null siginfo requests ordinary kill
    // semantics, and the kernel does not retain pointer arguments.
    let result = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            handle.as_raw_fd(),
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if result < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn linux_kill_process(handle: &OwnedFd) -> std::io::Result<()> {
    linux_signal_process(handle, libc::SIGKILL)
}

pub(crate) struct PrivateWorkerClient {
    generation: String,
    #[cfg(target_os = "linux")]
    worker_pid: u32,
    #[cfg(target_os = "linux")]
    worker_pidfd: Arc<OwnedFd>,
    #[cfg(target_os = "macos")]
    worker_termination: Arc<MacosWorkerTermination>,
    host_death_guard: Mutex<Option<HostDeathGuard>>,
    next_request_id: AtomicU64,
    request: Mutex<()>,
    #[cfg(test)]
    request_lock_signal: Mutex<Option<std::sync::mpsc::SyncSender<()>>>,
    process: Arc<Mutex<WorkerProcess>>,
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    reap_sender: std::sync::mpsc::SyncSender<ReapTask>,
    reap_scheduled: AtomicBool,
    shutdown_timeout: Duration,
}

impl PrivateWorkerClient {
    fn lock_process_until(&self, deadline: Instant) -> Option<MutexGuard<'_, WorkerProcess>> {
        loop {
            match self.process.try_lock() {
                Ok(process) => return Some(process),
                Err(TryLockError::Poisoned(error)) => return Some(error.into_inner()),
                Err(TryLockError::WouldBlock) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return None;
                    }
                    std::thread::sleep(Duration::from_millis(5).min(remaining));
                }
            }
        }
    }

    fn schedule_reap(&self) {
        if self.reap_scheduled.swap(true, Ordering::AcqRel) {
            return;
        }
        #[cfg(target_os = "linux")]
        {
            // HostDeathGuard releases the creator thread immediately after
            // this path; that thread owns bounded cleanup and registry fallback.
            let _ = linux_try_reap_pidfd(&self.worker_pidfd);
        }
        #[cfg(not(target_os = "linux"))]
        {
            let process = Arc::clone(&self.process);
            let task: ReapTask = Box::new(move || {
                process
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .stop_and_reap_blocking();
            });
            if let Err(error) = self.reap_sender.send(task) {
                drop(error.0);
            }
        }
    }

    fn discharge_host_death_containment(&self) {
        let mut guard = match self.host_death_guard.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::Poisoned(error)) => error.into_inner(),
            Err(TryLockError::WouldBlock) => return,
        };
        #[cfg(target_os = "windows")]
        if let Some(guard) = guard.as_ref() {
            guard.terminate();
        }
        guard.take();
    }

    fn force_terminate_worker(&self) {
        #[cfg(target_os = "linux")]
        {
            if let Err(error) = linux_kill_process(&self.worker_pidfd) {
                tracing::debug!(
                    worker_pid = self.worker_pid,
                    %error,
                    "private worker pidfd termination did not signal a live process"
                );
            }
        }
        #[cfg(target_os = "macos")]
        self.worker_termination.terminate();
        #[cfg(target_os = "windows")]
        {
            let guard = match self.host_death_guard.try_lock() {
                Ok(guard) => guard,
                Err(TryLockError::Poisoned(error)) => error.into_inner(),
                Err(TryLockError::WouldBlock) => return,
            };
            if let Some(guard) = guard.as_ref() {
                guard.terminate();
            }
        }
        #[cfg(not(any(unix, target_os = "windows")))]
        if let Ok(mut process) = self.process.try_lock() {
            if let Some(child) = process.child.as_mut() {
                let _ = child.kill();
            }
        }
    }

    fn terminate_process_until(&self, deadline: Instant) {
        self.force_terminate_worker();
        if let Some(mut process) = self.lock_process_until(deadline) {
            if !process.stop_and_reap_until(deadline) {
                drop(process);
                self.schedule_reap();
            }
        } else {
            self.schedule_reap();
        }
        self.discharge_host_death_containment();
    }

    pub(crate) fn spawn(
        options: ValidatedWorkerOptions,
        startup_deadline: Instant,
    ) -> Result<Arc<Self>, DriverError> {
        ensure_startup_deadline(startup_deadline)?;
        #[cfg(target_os = "linux")]
        linux_preflight_close_range().map_err(|error| DriverError::Configuration {
            reason: format!(
                "private workers require Linux close_range(CLOSE_RANGE_CLOEXEC) support and policy access: {error}"
            ),
        })?;
        let (reap_sender, reap_receiver) = std::sync::mpsc::sync_channel::<ReapTask>(1);
        #[cfg(not(target_os = "linux"))]
        std::thread::Builder::new()
            .name("cua-private-worker-reaper".into())
            .spawn(move || {
                if let Ok(task) = reap_receiver.recv() {
                    task();
                }
            })
            .map_err(|error| DriverError::Worker {
                reason: format!("spawn private-worker reaper before child creation: {error}"),
            })?;
        #[cfg(target_os = "linux")]
        drop(reap_receiver);
        let generation = Uuid::new_v4().to_string();
        let host_pid = std::process::id();
        #[cfg(target_os = "macos")]
        let termination_endpoint =
            MacosWorkerTerminationEndpoint::bind(&generation).map_err(|error| {
                DriverError::Worker {
                    reason: format!("create private-worker termination capability: {error}"),
                }
            })?;
        #[cfg(not(target_os = "linux"))]
        let mut command = Command::new(&options.binary_path);
        #[cfg(not(target_os = "linux"))]
        command
            .arg("__private-worker")
            .arg("--generation")
            .arg(&generation)
            .arg("--host-pid")
            .arg(host_pid.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(if options.inherit_stderr {
                Stdio::inherit()
            } else {
                Stdio::null()
            })
            .env_clear();
        #[cfg(target_os = "macos")]
        {
            command
                .arg("--termination-socket")
                .arg(termination_endpoint.socket_path());
        }
        #[cfg(not(target_os = "linux"))]
        {
            configure_host_death_containment(&mut command, host_pid);
            configure_child_descriptor_boundary(&mut command).map_err(|error| {
                DriverError::Worker {
                    reason: format!("seal private-worker inherited descriptor boundary: {error}"),
                }
            })?;
        }
        #[cfg(target_os = "linux")]
        let trusted_accessibility_bus = Some(
            crate::runtime::prepare_private_worker_accessibility_route(startup_deadline).map_err(
                |error| DriverError::Worker {
                    reason: format!("prepare private-worker desktop route: {error}"),
                },
            )?,
        );
        #[cfg(not(target_os = "linux"))]
        let trusted_accessibility_bus: Option<String> = None;
        ensure_startup_deadline(startup_deadline)?;
        let worker_environment = private_worker_environment(
            &options.environment,
            trusted_accessibility_bus.as_deref(),
            startup_deadline,
        )
        .map_err(|error| match error {
            PrivateWorkerEnvironmentError::DeadlineExpired => startup_timeout_error(),
            PrivateWorkerEnvironmentError::Invalid(reason) => DriverError::Configuration { reason },
        })?;
        #[cfg(not(target_os = "linux"))]
        for variable in &worker_environment {
            ensure_startup_deadline(startup_deadline)?;
            command.env(&variable.name, &variable.value);
        }
        ensure_startup_deadline(startup_deadline)?;

        #[cfg(target_os = "linux")]
        let linux_plan = prepare_linux_spawn_plan_until(
            &options.binary_path,
            &[
                "__private-worker".into(),
                "--generation".into(),
                generation.clone(),
                "--host-pid".into(),
                host_pid.to_string(),
            ],
            &worker_environment,
            host_pid as libc::pid_t,
            options.inherit_stderr,
            startup_deadline,
        )
        .map_err(|error| {
            if error.kind() == io::ErrorKind::TimedOut {
                startup_timeout_error()
            } else {
                DriverError::Worker {
                    reason: format!("prepare private-worker clone3 exec image: {error}"),
                }
            }
        })?;
        #[cfg(target_os = "linux")]
        let spawn_result = spawn_contained_worker_until(linux_plan, startup_deadline);
        #[cfg(not(target_os = "linux"))]
        let spawn_result =
            spawn_contained_worker_until(command, options.inherit_stderr, startup_deadline);
        let (mut child, host_death_guard) = spawn_result.map_err(|error| DriverError::Worker {
            reason: format!(
                "spawn and contain private worker {}: {error}",
                options.binary_path
            ),
        })?;
        let worker_pid = child.id();
        #[cfg(target_os = "linux")]
        let (host_death_guard, worker_termination) = {
            let mut host_death_guard = host_death_guard;
            let candidate_pidfd = match host_death_guard.take_pidfd() {
                Ok(handle) => handle,
                Err(error) => {
                    close_and_reap_unbound_worker(&reap_sender, child);
                    return Err(DriverError::Worker {
                        reason: format!(
                            "spawn did not return an immutable handle for private worker {worker_pid}: {error}"
                        ),
                    });
                }
            };
            (host_death_guard, Arc::new(candidate_pidfd))
        };
        #[cfg(unix)]
        let binding_nonce =
            match exchange_process_binding_until(&mut child, &generation, startup_deadline) {
                Ok(nonce) => nonce,
                Err(error) => {
                    #[cfg(target_os = "linux")]
                    close_and_reap_bound_worker(&reap_sender, child, &worker_termination);
                    #[cfg(not(target_os = "linux"))]
                    close_and_reap_unbound_worker(&reap_sender, child);
                    return Err(DriverError::Worker {
                        reason: format!(
                        "could not prove the spawned private worker on its private channel: {error}"
                    ),
                    });
                }
            };
        #[cfg(target_os = "linux")]
        let _ = &binding_nonce;
        #[cfg(target_os = "macos")]
        let worker_termination =
            match termination_endpoint.accept_until(startup_deadline, &binding_nonce, worker_pid) {
                Ok(handle) => handle,
                Err(error) => {
                    close_and_reap_unbound_worker(&reap_sender, child);
                    return Err(DriverError::Worker {
                        reason: format!(
                            "could not bind the private-worker termination capability: {error}"
                        ),
                    });
                }
            };

        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let worker_termination = ();
        let Some(stdin) = child.stdin.take() else {
            close_and_reap_bound_worker(&reap_sender, child, &worker_termination);
            return Err(DriverError::Worker {
                reason: "private worker stdin was not piped".into(),
            });
        };
        let Some(stdout) = child.stdout.take() else {
            close_and_reap_bound_worker(&reap_sender, child, &worker_termination);
            return Err(DriverError::Worker {
                reason: "private worker stdout was not piped".into(),
            });
        };
        let process = Arc::new(Mutex::new(WorkerProcess {
            child: Some(child),
            stdin: Some(stdin),
            stdout: Some(BufReader::new(stdout)),
            #[cfg(target_os = "linux")]
            worker_pidfd: Arc::clone(&worker_termination),
            #[cfg(target_os = "macos")]
            worker_termination: Arc::clone(&worker_termination),
            stopped: false,
        }));
        let client = Arc::new(Self {
            generation,
            #[cfg(target_os = "linux")]
            worker_pid,
            #[cfg(target_os = "linux")]
            worker_pidfd: worker_termination,
            #[cfg(target_os = "macos")]
            worker_termination,
            host_death_guard: Mutex::new(Some(host_death_guard)),
            next_request_id: AtomicU64::new(2),
            request: Mutex::new(()),
            #[cfg(test)]
            request_lock_signal: Mutex::new(None),
            process,
            reap_sender,
            reap_scheduled: AtomicBool::new(false),
            shutdown_timeout: options.shutdown_timeout,
        });

        let host_bundle_id = options.host_bundle_id.clone();
        let initialization = serde_json::to_value(WorkerInitialization {
            configured_driver: options.configured_driver,
            host_bundle_id: options.host_bundle_id,
            environment_attestation: worker_environment
                .into_iter()
                .map(|variable| WorkerEnvironmentVariable {
                    name: variable.name,
                    value: variable.value,
                })
                .collect(),
        })
        .map_err(|error| DriverError::Protocol {
            reason: format!("serialize private worker initialization: {error}"),
        })?;
        let request = ChannelRequest {
            protocol_version: PRIVATE_WORKER_PROTOCOL_VERSION,
            request_id: PRIVATE_WORKER_INITIALIZATION_REQUEST_ID,
            generation: client.generation.clone(),
            operation: "initialize".into(),
            name: None,
            arguments: Some(initialization),
            session_handle: None,
        };
        let remaining = startup_deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            client.terminate_process_until(startup_deadline);
            return Err(startup_timeout_error());
        }
        let ready = client.request_until(request, startup_deadline, options.startup_timeout)?;
        validate_readiness(&ready, worker_pid, &host_bundle_id, startup_deadline)?;
        Ok(client)
    }

    pub(crate) fn is_available(&self) -> bool {
        if self.reap_scheduled.load(Ordering::Acquire) {
            return false;
        }
        let Some(mut process) = self.lock_process_until(Instant::now()) else {
            return false;
        };
        if process.stopped {
            return false;
        }
        if process.child.is_none() {
            process.stopped = true;
            return false;
        }
        match process.try_reap() {
            Ok(false) => true,
            Ok(true) | Err(_) => {
                process.stopped = true;
                false
            }
        }
    }

    pub(crate) async fn metadata(self: &Arc<Self>) -> Result<DriverMetadata, DriverError> {
        let response = self.request_async("metadata", None, None, None).await?;
        serde_json::from_value(response).map_err(|error| DriverError::Protocol {
            reason: format!("private worker returned invalid metadata: {error}"),
        })
    }

    pub(crate) async fn list_tools(self: &Arc<Self>) -> Result<Value, DriverError> {
        self.request_async("list", None, None, None).await
    }

    pub(crate) async fn invoke(
        self: &Arc<Self>,
        name: &str,
        arguments: Value,
        session_handle: Option<String>,
    ) -> Result<Value, DriverError> {
        self.request_async(
            "call",
            Some(name.to_owned()),
            Some(arguments),
            session_handle,
        )
        .await
    }

    pub(crate) fn bind_session(
        self: &Arc<Self>,
        options: TrustedSessionOptions,
    ) -> Result<String, DriverError> {
        let arguments = serde_json::to_value(options).map_err(|error| DriverError::Protocol {
            reason: format!("serialize private worker session options: {error}"),
        })?;
        let response = self.request_sync("bind_session", None, Some(arguments), None)?;
        response
            .get("session_handle")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| DriverError::Protocol {
                reason: "private worker bind response omitted session_handle".into(),
            })
    }

    pub(crate) fn close_session(&self, session_handle: &str) {
        let _ = self.request_sync("close_session", None, None, Some(session_handle.to_owned()));
    }

    pub(crate) async fn shutdown(self: &Arc<Self>) -> Result<(), DriverError> {
        let deadline = Instant::now()
            .checked_add(self.shutdown_timeout)
            .ok_or_else(|| DriverError::Configuration {
                reason: "private-worker shutdown timeout exceeds the platform clock range".into(),
            })?;
        let client = self.clone();
        let may_have_started = Arc::new(AtomicBool::new(false));
        let may_have_started_in_task = Arc::clone(&may_have_started);
        let task = tokio::task::spawn_blocking(move || {
            client.shutdown_sync_until(deadline, &may_have_started_in_task)
        });
        match tokio::time::timeout(deadline.saturating_duration_since(Instant::now()), task).await {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => Err(DriverError::Worker {
                reason: format!("join private worker shutdown: {error}"),
            }),
            Err(_) => {
                self.force_terminate_worker();
                self.schedule_reap();
                self.discharge_host_death_containment();
                if may_have_started.load(Ordering::Acquire) {
                    Err(DriverError::ActionInterrupted {
                        completion: ActionCompletion::Unknown,
                        reason: format!(
                            "private worker shutdown exceeded its total {}ms deadline after transmission may have started",
                            self.shutdown_timeout.as_millis()
                        ),
                    })
                } else {
                    Err(self.shutdown_not_started_error())
                }
            }
        }
    }

    #[cfg(test)]
    fn shutdown_sync(&self) -> Result<(), DriverError> {
        self.shutdown_sync_until(
            Instant::now() + self.shutdown_timeout,
            &Arc::new(AtomicBool::new(false)),
        )
    }

    fn shutdown_sync_until(
        &self,
        deadline: Instant,
        may_have_started: &Arc<AtomicBool>,
    ) -> Result<(), DriverError> {
        let result = self.shutdown_sync_inner_until(deadline, may_have_started);
        // On Windows this terminates every remaining Job member before a
        // successful shutdown can be reported. On Linux it releases the
        // parent-death supervision thread only after direct-worker shutdown.
        self.discharge_host_death_containment();
        result
    }

    fn shutdown_sync_inner_until(
        &self,
        deadline: Instant,
        may_have_started: &Arc<AtomicBool>,
    ) -> Result<(), DriverError> {
        // Start the deadline before waiting for the request serializer. An
        // in-flight request may be blocked on the worker for its full request
        // timeout, but it must not extend this configured shutdown budget.
        let request = self.lock_request_until(deadline);
        let response = match request.as_ref() {
            Some(_) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    Err(self.shutdown_not_started_error())
                } else {
                    self.request_sync_locked_until(
                        "shutdown",
                        None,
                        None,
                        None,
                        deadline,
                        self.shutdown_timeout,
                        Some(Arc::clone(may_have_started)),
                    )
                }
            }
            None => Err(self.shutdown_not_started_error()),
        };
        drop(request);

        let Some(mut process) = self.lock_process_until(deadline) else {
            self.force_terminate_worker();
            self.schedule_reap();
            return forced_shutdown_result(
                response,
                "private worker acknowledged shutdown but its process channel could not be acquired before forced termination",
            );
        };
        process.stdin.take();
        loop {
            if process.child.is_none() {
                process.stopped = true;
                return response.map(|_| ());
            }
            match process.try_reap() {
                Ok(true) => {
                    process.child.take();
                    process.stopped = true;
                    return response.map(|_| ());
                }
                Ok(false) if Instant::now() < deadline => {
                    drop(process);
                    std::thread::sleep(
                        Duration::from_millis(20)
                            .min(deadline.saturating_duration_since(Instant::now())),
                    );
                    let Some(relocked) = self.lock_process_until(deadline) else {
                        self.force_terminate_worker();
                        self.schedule_reap();
                        return forced_shutdown_result(
                            response,
                            "private worker acknowledged shutdown but process exit could not be observed before forced termination",
                        );
                    };
                    process = relocked;
                }
                Ok(false) => {
                    self.force_terminate_worker();
                    let reaped = process.stop_and_reap_until(deadline);
                    drop(process);
                    if !reaped {
                        self.schedule_reap();
                    }
                    return forced_shutdown_result(
                        response,
                        "private worker acknowledged shutdown but did not exit before deadline-forced termination",
                    );
                }
                Err(error) => {
                    self.force_terminate_worker();
                    let reaped = process.stop_and_reap_until(deadline);
                    drop(process);
                    if !reaped {
                        self.schedule_reap();
                    }
                    return forced_shutdown_result(
                        response,
                        format!(
                            "private worker process exit could not be observed ({error}) before forced termination"
                        ),
                    );
                }
            }
        }
    }

    async fn request_async(
        self: &Arc<Self>,
        operation: &str,
        name: Option<String>,
        arguments: Option<Value>,
        session_handle: Option<String>,
    ) -> Result<Value, DriverError> {
        self.request_async_with_timeout(
            operation,
            name,
            arguments,
            session_handle,
            DEFAULT_REQUEST_TIMEOUT,
        )
        .await
    }

    async fn request_async_with_timeout(
        self: &Arc<Self>,
        operation: &str,
        name: Option<String>,
        arguments: Option<Value>,
        session_handle: Option<String>,
        timeout: Duration,
    ) -> Result<Value, DriverError> {
        let deadline =
            Instant::now()
                .checked_add(timeout)
                .ok_or_else(|| DriverError::Configuration {
                    reason: "private-worker request timeout exceeds the platform clock range"
                        .into(),
                })?;
        let client = self.clone();
        let operation = operation.to_owned();
        let operation_for_error = operation.clone();
        let io_may_have_started = Arc::new(AtomicBool::new(false));
        let io_phase = Arc::clone(&io_may_have_started);
        let task = tokio::task::spawn_blocking(move || {
            client.request_sync_with_deadline(
                &operation,
                name,
                arguments,
                session_handle,
                deadline,
                timeout,
                Some(io_phase),
            )
        });
        match tokio::time::timeout(deadline.saturating_duration_since(Instant::now()), task).await {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => Err(DriverError::Worker {
                reason: format!("join private worker request: {error}"),
            }),
            Err(_) => {
                self.force_terminate_worker();
                self.schedule_reap();
                Err(DriverError::ActionInterrupted {
                    completion: if io_may_have_started.load(Ordering::Acquire) {
                        ActionCompletion::Unknown
                    } else {
                        ActionCompletion::NotStarted
                    },
                    reason: format!(
                        "private worker request {operation_for_error} exceeded its total {}ms admission and execution deadline",
                        timeout.as_millis()
                    ),
                })
            }
        }
    }

    fn request_sync(
        &self,
        operation: &str,
        name: Option<String>,
        arguments: Option<Value>,
        session_handle: Option<String>,
    ) -> Result<Value, DriverError> {
        self.request_sync_with_timeout(
            operation,
            name,
            arguments,
            session_handle,
            DEFAULT_REQUEST_TIMEOUT,
        )
    }

    fn request_sync_with_timeout(
        &self,
        operation: &str,
        name: Option<String>,
        arguments: Option<Value>,
        session_handle: Option<String>,
        timeout: Duration,
    ) -> Result<Value, DriverError> {
        let deadline =
            Instant::now()
                .checked_add(timeout)
                .ok_or_else(|| DriverError::Configuration {
                    reason: "private-worker request timeout exceeds the platform clock range"
                        .into(),
                })?;
        self.request_sync_with_deadline(
            operation,
            name,
            arguments,
            session_handle,
            deadline,
            timeout,
            None,
        )
    }

    fn request_sync_with_deadline(
        &self,
        operation: &str,
        name: Option<String>,
        arguments: Option<Value>,
        session_handle: Option<String>,
        deadline: Instant,
        timeout: Duration,
        io_may_have_started: Option<Arc<AtomicBool>>,
    ) -> Result<Value, DriverError> {
        let Some(_request) = self.lock_request_until(deadline) else {
            return Err(DriverError::ActionInterrupted {
                completion: ActionCompletion::NotStarted,
                reason: format!(
                    "private worker request {operation} could not enter the channel within {}ms",
                    timeout.as_millis()
                ),
            });
        };
        #[cfg(test)]
        if let Some(signal) = self
            .request_lock_signal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = signal.try_send(());
        }
        self.request_sync_locked_until(
            operation,
            name,
            arguments,
            session_handle,
            deadline,
            timeout,
            io_may_have_started,
        )
    }

    fn request_sync_locked_until(
        &self,
        operation: &str,
        name: Option<String>,
        arguments: Option<Value>,
        session_handle: Option<String>,
        deadline: Instant,
        timeout: Duration,
        io_may_have_started: Option<Arc<AtomicBool>>,
    ) -> Result<Value, DriverError> {
        let request_id = self
            .next_request_id
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .map_err(|_| DriverError::Protocol {
                reason: "private worker request ID space exhausted".into(),
            })?;
        self.request_until_locked(
            ChannelRequest {
                protocol_version: PRIVATE_WORKER_PROTOCOL_VERSION,
                request_id,
                generation: self.generation.clone(),
                operation: operation.into(),
                name,
                arguments,
                session_handle,
            },
            deadline,
            timeout,
            io_may_have_started,
        )
    }

    fn request_until(
        &self,
        request: ChannelRequest,
        deadline: Instant,
        timeout: Duration,
    ) -> Result<Value, DriverError> {
        let Some(_request) = self.lock_request_until(deadline) else {
            return Err(DriverError::ActionInterrupted {
                completion: ActionCompletion::NotStarted,
                reason: format!(
                    "private worker request {} could not enter the channel within {}ms",
                    request.request_id,
                    timeout.as_millis()
                ),
            });
        };
        self.request_until_locked(request, deadline, timeout, None)
    }

    fn request_until_locked(
        &self,
        request: ChannelRequest,
        deadline: Instant,
        timeout: Duration,
        io_may_have_started: Option<Arc<AtomicBool>>,
    ) -> Result<Value, DriverError> {
        let line = match encode_private_worker_message_until(&request, deadline) {
            Ok(line) => line,
            Err(_) if Instant::now() >= deadline => {
                return Err(DriverError::ActionInterrupted {
                    completion: ActionCompletion::NotStarted,
                    reason: format!(
                        "private worker request {} exceeded its deadline during serialization",
                        request.request_id
                    ),
                });
            }
            Err(error) => {
                return Err(DriverError::Protocol {
                    reason: format!("serialize bounded private worker request: {error}"),
                });
            }
        };
        if Instant::now() >= deadline {
            return Err(DriverError::ActionInterrupted {
                completion: ActionCompletion::NotStarted,
                reason: format!(
                    "private worker request {} exceeded its deadline before channel I/O",
                    request.request_id
                ),
            });
        }
        let Some(mut process) = self.lock_process_until(deadline) else {
            return Err(DriverError::ActionInterrupted {
                completion: ActionCompletion::NotStarted,
                reason: format!(
                    "private worker request {} could not acquire the process channel within {}ms",
                    request.request_id,
                    timeout.as_millis()
                ),
            });
        };
        if process.stopped {
            return Err(DriverError::Shutdown);
        }
        if process.child.is_none() {
            process.stopped = true;
            return Err(DriverError::Shutdown);
        }
        match process.try_reap() {
            Ok(false) => {}
            Ok(true) | Err(_) => {
                process.child.take();
                process.stopped = true;
                return Err(DriverError::ActionInterrupted {
                    completion: ActionCompletion::NotStarted,
                    reason: "private worker exited or was no longer waitable before the request was written".into(),
                });
            }
        }

        let mut stdin = process.stdin.take().ok_or(DriverError::Shutdown)?;
        let stdout = match process.stdout.take() {
            Some(stdout) => stdout,
            None => {
                process.stdin = Some(stdin);
                return Err(DriverError::Worker {
                    reason: "private worker response reader is unavailable".into(),
                });
            }
        };

        // Anonymous pipes do not expose portable I/O timeouts. A helper thread
        // owns the complete bounded write/read exchange and returns both
        // handles. The process lock stays available so shutdown can terminate
        // this SDK-owned child if either half of the exchange wedges.
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let io_may_have_started =
            io_may_have_started.unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
        let io_may_have_started_in_thread = Arc::clone(&io_may_have_started);
        let response_limit = if request.request_id == PRIVATE_WORKER_INITIALIZATION_REQUEST_ID {
            PRIVATE_WORKER_MAX_READINESS_BYTES
        } else {
            PRIVATE_WORKER_MAX_MESSAGE_BYTES
        };
        let exchange_thread = std::thread::Builder::new()
            .name("cua-private-worker-exchange".into())
            .spawn(move || {
                let mut stdout = stdout;
                io_may_have_started_in_thread.store(true, Ordering::Release);
                let exchange = stdin
                    .write_all(&line)
                    .and_then(|()| stdin.write_all(b"\n"))
                    .and_then(|()| stdin.flush())
                    .map_err(WorkerExchangeError::Write)
                    .and_then(|()| {
                        read_private_worker_message_with_limit(&mut stdout, response_limit)
                            .map_err(WorkerExchangeError::Read)
                    });
                let _ = tx.send((stdin, stdout, exchange));
            });
        drop(process);
        if let Err(error) = exchange_thread {
            self.terminate_process_until(deadline);
            return Err(DriverError::ActionInterrupted {
                completion: ActionCompletion::NotStarted,
                reason: format!("start private worker exchange: {error}"),
            });
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        let (stdin, stdout, exchange) = match rx.recv_timeout(remaining) {
            Ok(response) => response,
            Err(_) => {
                self.terminate_process_until(deadline);
                return Err(DriverError::ActionInterrupted {
                    completion: if io_may_have_started.load(Ordering::Acquire) {
                        ActionCompletion::Unknown
                    } else {
                        ActionCompletion::NotStarted
                    },
                    reason: format!(
                        "private worker did not answer request {} within {}ms",
                        request.request_id,
                        timeout.as_millis()
                    ),
                });
            }
        };
        let Some(mut process) = self.lock_process_until(deadline) else {
            self.terminate_process_until(deadline);
            return Err(DriverError::ActionInterrupted {
                completion: ActionCompletion::Unknown,
                reason: "private worker process channel was unavailable after response I/O".into(),
            });
        };
        if !process.stopped {
            process.stdin = Some(stdin);
            process.stdout = Some(stdout);
        }
        drop(process);

        let response_line = match exchange {
            Ok(Some(response)) => response,
            Ok(None) => {
                self.terminate_process_until(deadline);
                return Err(DriverError::ActionInterrupted {
                    completion: ActionCompletion::Unknown,
                    reason: "private worker channel closed before completion was reported".into(),
                });
            }
            Err(WorkerExchangeError::Write(error)) => {
                self.terminate_process_until(deadline);
                return Err(DriverError::ActionInterrupted {
                    completion: ActionCompletion::Unknown,
                    reason: format!("write private worker request: {error}"),
                });
            }
            Err(WorkerExchangeError::Read(error)) => {
                self.terminate_process_until(deadline);
                return Err(DriverError::ActionInterrupted {
                    completion: ActionCompletion::Unknown,
                    reason: format!("read private worker response: {error}"),
                });
            }
        };
        let response_reader = DeadlineReader {
            remaining: response_line.as_bytes(),
            deadline,
        };
        let response: ChannelResponse = match serde_json::from_reader(response_reader) {
            Ok(response) => response,
            Err(_) if Instant::now() >= deadline => {
                self.terminate_process_until(deadline);
                return Err(DriverError::ActionInterrupted {
                    completion: ActionCompletion::Unknown,
                    reason: format!(
                        "private worker response parsing exceeded the {}ms request deadline",
                        timeout.as_millis()
                    ),
                });
            }
            Err(error) => {
                self.terminate_process_until(deadline);
                return Err(DriverError::Protocol {
                    reason: format!("parse private worker response: {error}"),
                });
            }
        };
        if Instant::now() >= deadline {
            self.terminate_process_until(deadline);
            return Err(DriverError::ActionInterrupted {
                completion: ActionCompletion::Unknown,
                reason: format!(
                    "private worker response validation exceeded the {}ms request deadline",
                    timeout.as_millis()
                ),
            });
        }
        if !response_identity_matches(&request, &response, &self.generation) {
            self.terminate_process_until(deadline);
            return Err(DriverError::Protocol {
                reason: "private worker response identity mismatch".into(),
            });
        }
        if response.completion == ActionCompletion::Unknown {
            return Err(DriverError::ActionInterrupted {
                completion: ActionCompletion::Unknown,
                reason: response
                    .error
                    .unwrap_or_else(|| "private worker completion is unknown".into()),
            });
        }
        if response.ok && response.completion != ActionCompletion::Completed {
            self.terminate_process_until(deadline);
            return Err(DriverError::Protocol {
                reason: "private worker reported success without completed execution".into(),
            });
        }
        if !response.ok {
            return Err(DriverError::Worker {
                reason: format!(
                    "{}: {}",
                    response.error_code.as_deref().unwrap_or("worker_error"),
                    response.error.as_deref().unwrap_or("request failed")
                ),
            });
        }
        Ok(response.result.unwrap_or(Value::Null))
    }

    fn lock_request_until(&self, deadline: Instant) -> Option<MutexGuard<'_, ()>> {
        loop {
            match self.request.try_lock() {
                Ok(request) => return Some(request),
                Err(TryLockError::Poisoned(error)) => return Some(error.into_inner()),
                Err(TryLockError::WouldBlock) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return None;
                    }
                    std::thread::sleep(Duration::from_millis(5).min(remaining));
                }
            }
        }
    }

    fn shutdown_not_started_error(&self) -> DriverError {
        DriverError::ActionInterrupted {
            completion: ActionCompletion::NotStarted,
            reason: format!(
                "private worker shutdown could not start within {}ms",
                self.shutdown_timeout.as_millis()
            ),
        }
    }
}

impl Drop for PrivateWorkerClient {
    fn drop(&mut self) {
        self.force_terminate_worker();
        self.schedule_reap();
        self.discharge_host_death_containment();
    }
}

fn startup_timeout_error() -> DriverError {
    DriverError::ActionInterrupted {
        reason: "private-worker startup exceeded its configured deadline".into(),
        completion: ActionCompletion::NotStarted,
    }
}

fn ensure_startup_deadline(deadline: Instant) -> Result<(), DriverError> {
    if Instant::now() >= deadline {
        Err(startup_timeout_error())
    } else {
        Ok(())
    }
}

pub(crate) fn private_worker_startup_timing(
    startup_timeout_ms: Option<u64>,
) -> Result<(Duration, Instant), DriverError> {
    let startup_timeout_ms = startup_timeout_ms.unwrap_or(DEFAULT_STARTUP_TIMEOUT_MS);
    if startup_timeout_ms == 0 {
        return Err(DriverError::Configuration {
            reason: "private worker startup timeout must be positive".into(),
        });
    }
    let startup_timeout = Duration::from_millis(startup_timeout_ms);
    let startup_deadline =
        Instant::now()
            .checked_add(startup_timeout)
            .ok_or_else(|| DriverError::Configuration {
                reason: "private-worker startup timeout exceeds the platform clock range".into(),
            })?;
    Ok((startup_timeout, startup_deadline))
}

fn validate_worker_environment(
    environment: &[EmbeddedEnvironmentVariable],
    startup_deadline: Instant,
) -> Result<(), DriverError> {
    ensure_startup_deadline(startup_deadline)?;
    if environment.len() > PRIVATE_WORKER_MAX_ENVIRONMENT_ENTRIES {
        return Err(DriverError::Configuration {
            reason: format!(
                "private worker environment exceeds {PRIVATE_WORKER_MAX_ENVIRONMENT_ENTRIES} entries"
            ),
        });
    }
    let mut environment_bytes = 0usize;
    for variable in environment {
        ensure_startup_deadline(startup_deadline)?;
        if variable.name.len() > PRIVATE_WORKER_MAX_ENVIRONMENT_NAME_BYTES {
            return Err(DriverError::Configuration {
                reason: format!(
                    "environment variable name exceeds {PRIVATE_WORKER_MAX_ENVIRONMENT_NAME_BYTES} bytes"
                ),
            });
        }
        if variable.value.len() > PRIVATE_WORKER_MAX_ENVIRONMENT_VALUE_BYTES {
            return Err(DriverError::Configuration {
                reason: format!(
                    "environment variable {} exceeds {PRIVATE_WORKER_MAX_ENVIRONMENT_VALUE_BYTES} bytes",
                    variable.name
                ),
            });
        }
        environment_bytes = environment_bytes
            .checked_add(variable.name.len())
            .and_then(|bytes| bytes.checked_add(variable.value.len()))
            .ok_or_else(|| DriverError::Configuration {
                reason: "private worker environment size overflowed".into(),
            })?;
        if environment_bytes > PRIVATE_WORKER_MAX_ENVIRONMENT_BYTES {
            return Err(DriverError::Configuration {
                reason: format!(
                    "private worker environment exceeds {PRIVATE_WORKER_MAX_ENVIRONMENT_BYTES} bytes"
                ),
            });
        }
        if !allowed_environment_name(&variable.name)
            || variable.name.contains('=')
            || variable.name.contains('\0')
            || variable.value.contains('\0')
        {
            return Err(DriverError::Configuration {
                reason: format!(
                    "environment variable {} is not in the private-worker safe allowlist",
                    variable.name
                ),
            });
        }
        ensure_startup_deadline(startup_deadline)?;
    }
    Ok(())
}

pub(crate) fn validate_worker_options(
    binary_path: String,
    host_bundle_id: String,
    startup_timeout: Duration,
    startup_deadline: Instant,
    shutdown_timeout_ms: Option<u64>,
    configured_driver: ConfiguredDriverOptions,
    environment: Vec<EmbeddedEnvironmentVariable>,
    inherit_stderr: bool,
) -> Result<ValidatedWorkerOptions, DriverError> {
    ensure_startup_deadline(startup_deadline)?;
    if binary_path.is_empty()
        || binary_path.len() > PRIVATE_WORKER_MAX_PATH_BYTES
        || binary_path.trim().is_empty()
        || !std::path::Path::new(&binary_path).is_absolute()
    {
        return Err(DriverError::Configuration {
            reason: format!(
                "private worker binary_path must be absolute, non-empty, and no more than {PRIVATE_WORKER_MAX_PATH_BYTES} bytes"
            ),
        });
    }
    if host_bundle_id.is_empty()
        || host_bundle_id.len() > 255
        || host_bundle_id.trim().is_empty()
        || host_bundle_id
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err(DriverError::Configuration {
            reason:
                "private worker host_bundle_id must be 1-255 non-whitespace, non-control characters"
                    .into(),
        });
    }
    let shutdown_timeout_ms = shutdown_timeout_ms.unwrap_or(DEFAULT_SHUTDOWN_TIMEOUT_MS);
    if shutdown_timeout_ms == 0 {
        return Err(DriverError::Configuration {
            reason: "private worker shutdown timeout must be positive".into(),
        });
    }
    ensure_startup_deadline(startup_deadline)?;
    if configured_driver.authorization.allowed_modes.len() > PRIVATE_WORKER_MAX_AUTHORIZATION_MODES
    {
        return Err(DriverError::Configuration {
            reason: format!(
                "private worker authorization exceeds {PRIVATE_WORKER_MAX_AUTHORIZATION_MODES} modes"
            ),
        });
    }
    if configured_driver
        .authorization
        .compatibility_bounded_manifest_path
        .as_ref()
        .is_some_and(|path| path.len() > PRIVATE_WORKER_MAX_PATH_BYTES)
    {
        return Err(DriverError::Configuration {
            reason: format!(
                "private worker bounded manifest path exceeds {PRIVATE_WORKER_MAX_PATH_BYTES} bytes"
            ),
        });
    }
    ensure_startup_deadline(startup_deadline)?;
    validate_worker_environment(&environment, startup_deadline)?;
    Ok(ValidatedWorkerOptions {
        binary_path,
        host_bundle_id,
        startup_timeout,
        shutdown_timeout: Duration::from_millis(shutdown_timeout_ms),
        configured_driver,
        environment,
        inherit_stderr,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        encode_private_worker_message_with_limit, read_private_worker_message_with_limit,
        response_identity_matches, validate_readiness, validate_worker_environment,
        ActionCompletion, AtomicU64, ChannelRequest, ChannelResponse, DriverError,
        PrivateWorkerClient, WorkerProcess, PRIVATE_WORKER_INITIALIZATION_REQUEST_ID,
        PRIVATE_WORKER_MAX_ENVIRONMENT_ENTRIES, PRIVATE_WORKER_MAX_ENVIRONMENT_VALUE_BYTES,
        PRIVATE_WORKER_PROTOCOL_VERSION, PRIVATE_WORKER_STARTUP_ERROR_REQUEST_ID,
    };
    use crate::embedded::{
        allowed_environment_name, inherited_managed_environment_name,
        inherited_runtime_environment_name, EmbeddedEnvironmentVariable,
    };
    use std::io::{BufRead, BufReader, Cursor, Write};
    use std::process::{Command, Stdio};
    use std::sync::{atomic::AtomicBool, Arc, Mutex};
    use std::time::{Duration, Instant};

    const WEDGE_HELPER_ENV: &str = "CUA_DRIVER_SDK_WEDGE_HELPER";
    #[cfg(target_os = "macos")]
    const WEDGE_TERMINATION_SOCKET_ENV: &str = "CUA_DRIVER_SDK_WEDGE_TERMINATION_SOCKET";
    #[cfg(target_os = "macos")]
    const WEDGE_GENERATION: &str = "shutdown-timeout-test";
    const WEDGE_HELPER_READY: &str = "private-worker-wedge-ready";
    const WEDGE_HELPER_REQUEST: &str = "private-worker-wedge-request";
    #[cfg(target_os = "linux")]
    const PDEATH_HELPER_ENV: &str = "CUA_DRIVER_SDK_PDEATH_HELPER";
    #[cfg(target_os = "linux")]
    const AUTO_REAP_HELPER_ENV: &str = "CUA_DRIVER_SDK_AUTO_REAP_HELPER";
    #[cfg(target_os = "linux")]
    const FOREIGN_WAITER_HELPER_ENV: &str = "CUA_DRIVER_SDK_FOREIGN_WAITER_HELPER";
    #[cfg(target_os = "linux")]
    const CLOSED_STDIO_HELPER_ENV: &str = "CUA_DRIVER_SDK_CLOSED_STDIO_HELPER";
    #[cfg(target_os = "linux")]
    const LOW_FD_REUSE_HELPER_ENV: &str = "CUA_DRIVER_SDK_LOW_FD_REUSE_HELPER";
    #[cfg(target_os = "linux")]
    const HIGH_FD_HELPER_ENV: &str = "CUA_DRIVER_SDK_HIGH_FD_HELPER";

    #[test]
    fn startup_error_identity_is_accepted_only_for_initialization_failures() {
        let initialization = ChannelRequest {
            protocol_version: PRIVATE_WORKER_PROTOCOL_VERSION,
            request_id: PRIVATE_WORKER_INITIALIZATION_REQUEST_ID,
            generation: "generation".into(),
            operation: "initialize".into(),
            name: None,
            arguments: None,
            session_handle: None,
        };
        let startup_error = ChannelResponse::error(
            PRIVATE_WORKER_STARTUP_ERROR_REQUEST_ID,
            "generation",
            "worker_setup_failed",
            "failed before initialization",
            ActionCompletion::NotStarted,
        );
        assert!(response_identity_matches(
            &initialization,
            &startup_error,
            "generation"
        ));

        let initialization_ready = ChannelResponse::ok(
            PRIVATE_WORKER_INITIALIZATION_REQUEST_ID,
            "generation",
            serde_json::json!({"ready": true}),
        );
        assert!(response_identity_matches(
            &initialization,
            &initialization_ready,
            "generation"
        ));

        let mut ordinary = initialization.clone();
        ordinary.request_id = 2;
        ordinary.operation = "metadata".into();
        assert!(!response_identity_matches(
            &ordinary,
            &startup_error,
            "generation"
        ));

        let mut invalid_startup_success = startup_error.clone();
        invalid_startup_success.ok = true;
        invalid_startup_success.completion = ActionCompletion::Completed;
        assert!(!response_identity_matches(
            &initialization,
            &invalid_startup_success,
            "generation"
        ));
        let mut unstructured_startup_error = startup_error.clone();
        unstructured_startup_error.error_code = None;
        assert!(!response_identity_matches(
            &initialization,
            &unstructured_startup_error,
            "generation"
        ));
        assert!(!response_identity_matches(
            &initialization,
            &startup_error,
            "other-generation"
        ));
    }

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    type WedgeStderr = ();
    #[cfg(target_os = "macos")]
    type WedgeStderr = BufReader<std::process::ChildStderr>;

    #[cfg(target_os = "linux")]
    fn spawn_wedged_client(shutdown_timeout: Duration) -> (Arc<PrivateWorkerClient>, WedgeStderr) {
        let executable = std::env::current_exe().unwrap();
        let environment = [EmbeddedEnvironmentVariable {
            name: WEDGE_HELPER_ENV.into(),
            value: "1".into(),
        }];
        let plan = super::prepare_linux_spawn_plan(
            executable.to_str().unwrap(),
            &[
                "--exact".into(),
                "worker::tests::private_worker_wedge_helper".into(),
                "--nocapture".into(),
            ],
            &environment,
            std::process::id() as libc::pid_t,
            false,
        )
        .unwrap();
        let (mut child, mut host_death_guard) =
            super::spawn_contained_worker_until(plan, Instant::now() + Duration::from_secs(2))
                .unwrap();
        let worker_pidfd = Arc::new(host_death_guard.take_pidfd().unwrap());
        let worker_pid = child.id();
        let stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        loop {
            line.clear();
            assert_ne!(stdout.read_line(&mut line).unwrap(), 0);
            if line.trim() == WEDGE_HELPER_READY {
                break;
            }
        }
        let process = Arc::new(Mutex::new(WorkerProcess {
            child: Some(child),
            stdin: Some(stdin),
            stdout: Some(stdout),
            worker_pidfd: Arc::clone(&worker_pidfd),
            stopped: false,
        }));
        let (reap_sender, reap_receiver) = std::sync::mpsc::sync_channel::<super::ReapTask>(1);
        std::thread::spawn(move || {
            if let Ok(task) = reap_receiver.recv() {
                task();
            }
        });
        (
            Arc::new(PrivateWorkerClient {
                generation: "shutdown-timeout-test".into(),
                worker_pid,
                worker_pidfd,
                host_death_guard: Mutex::new(Some(host_death_guard)),
                next_request_id: AtomicU64::new(2),
                request: Mutex::new(()),
                request_lock_signal: Mutex::new(None),
                process,
                reap_sender,
                reap_scheduled: AtomicBool::new(false),
                shutdown_timeout,
            }),
            (),
        )
    }

    #[cfg(target_os = "windows")]
    fn spawn_wedged_client(shutdown_timeout: Duration) -> (Arc<PrivateWorkerClient>, WedgeStderr) {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .arg("--exact")
            .arg("worker::tests::private_worker_wedge_helper")
            .arg("--nocapture")
            .env(WEDGE_HELPER_ENV, "1");
        let (mut child, host_death_guard) = super::spawn_contained_worker(command, false).unwrap();
        let stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        loop {
            line.clear();
            assert_ne!(stdout.read_line(&mut line).unwrap(), 0);
            if line.trim() == WEDGE_HELPER_READY {
                break;
            }
        }
        let process = Arc::new(Mutex::new(WorkerProcess {
            child: Some(child),
            stdin: Some(stdin),
            stdout: Some(stdout),
            stopped: false,
        }));
        let (reap_sender, reap_receiver) = std::sync::mpsc::sync_channel::<super::ReapTask>(1);
        std::thread::spawn(move || {
            if let Ok(task) = reap_receiver.recv() {
                task();
            }
        });

        (
            Arc::new(PrivateWorkerClient {
                generation: "shutdown-timeout-test".into(),
                host_death_guard: Mutex::new(Some(host_death_guard)),
                next_request_id: AtomicU64::new(2),
                request: Mutex::new(()),
                request_lock_signal: Mutex::new(None),
                process,
                reap_sender,
                reap_scheduled: AtomicBool::new(false),
                shutdown_timeout,
            }),
            (),
        )
    }

    #[cfg(target_os = "macos")]
    fn spawn_wedged_client(shutdown_timeout: Duration) -> (Arc<PrivateWorkerClient>, WedgeStderr) {
        let termination_endpoint =
            super::MacosWorkerTerminationEndpoint::bind(WEDGE_GENERATION).unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .arg("--exact")
            .arg("worker::tests::private_worker_wedge_helper")
            .arg("--nocapture")
            .env(WEDGE_HELPER_ENV, "1")
            .env(
                WEDGE_TERMINATION_SOCKET_ENV,
                termination_endpoint.socket_path(),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        let worker_termination = termination_endpoint
            .accept_until(
                Instant::now() + Duration::from_secs(2),
                WEDGE_GENERATION,
                child.id(),
            )
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let stderr = BufReader::new(child.stderr.take().unwrap());
        let mut line = String::new();
        loop {
            line.clear();
            assert_ne!(stdout.read_line(&mut line).unwrap(), 0);
            if line.trim() == WEDGE_HELPER_READY {
                break;
            }
        }
        let process = Arc::new(Mutex::new(WorkerProcess {
            child: Some(child),
            stdin: Some(stdin),
            stdout: Some(stdout),
            worker_termination: Arc::clone(&worker_termination),
            stopped: false,
        }));
        let (reap_sender, reap_receiver) = std::sync::mpsc::sync_channel::<super::ReapTask>(1);
        std::thread::spawn(move || {
            if let Ok(task) = reap_receiver.recv() {
                task();
            }
        });
        (
            Arc::new(PrivateWorkerClient {
                generation: WEDGE_GENERATION.into(),
                worker_termination,
                host_death_guard: Mutex::new(None),
                next_request_id: AtomicU64::new(2),
                request: Mutex::new(()),
                request_lock_signal: Mutex::new(None),
                process,
                reap_sender,
                reap_scheduled: AtomicBool::new(false),
                shutdown_timeout,
            }),
            stderr,
        )
    }

    fn assert_bounded(elapsed: Duration, timeout: Duration) {
        assert!(
            elapsed >= timeout / 2,
            "shutdown returned before exercising the timeout: {elapsed:?}"
        );
        assert!(
            elapsed < timeout + Duration::from_millis(250),
            "short private-worker timeout was not a full-path bound: {elapsed:?}"
        );
    }

    #[test]
    fn protocol_reader_rejects_a_message_over_the_byte_limit() {
        let mut reader = Cursor::new(b"123456789\n".to_vec());
        let error = read_private_worker_message_with_limit(&mut reader, 8).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);

        let mut reader = Cursor::new(b"12345678\n".to_vec());
        assert_eq!(
            read_private_worker_message_with_limit(&mut reader, 8).unwrap(),
            Some("12345678".into())
        );
    }

    #[test]
    fn response_parser_rejects_work_after_the_request_deadline() {
        let reader = super::DeadlineReader {
            remaining: br#"{"ok":true}"#,
            deadline: Instant::now(),
        };
        assert!(serde_json::from_reader::<_, serde_json::Value>(reader).is_err());
    }

    #[test]
    fn protocol_encoder_rejects_a_message_over_the_byte_limit() {
        let value = serde_json::json!({"payload": "x".repeat(64)});
        let error = encode_private_worker_message_with_limit(&value, 32, None).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::Other);
        assert!(error.to_string().contains("exceeds 32 bytes"));
    }

    #[test]
    fn readiness_requires_the_exact_spawned_pid() {
        let mut ready = serde_json::json!({
            "ready": true,
            "pid": 41,
            "host_bundle_id": "com.example.host",
            "environment_verified": true,
            "metadata": {
                "driver_version": env!("CARGO_PKG_VERSION"),
                "contract_version": cua_driver_contract::CONTRACT_VERSION,
                "tools_list_schema_version": cua_driver_contract::TOOLS_LIST_SCHEMA_VERSION,
                "capability_version": cua_driver_contract::CAPABILITY_VERSION,
                "mcp_protocol_version": cua_driver_contract::MCP_PROTOCOL_VERSION,
                "pid": 41,
                "embedded": true,
                "host_bundle_id": "com.example.host",
            },
        });
        let deadline = Instant::now() + Duration::from_secs(1);
        assert!(validate_readiness(&ready, 41, "com.example.host", deadline).is_ok());
        assert!(validate_readiness(&ready, 42, "com.example.host", deadline).is_err());
        assert!(validate_readiness(&ready, 41, "com.example.other", deadline).is_err());

        ready["metadata"]["host_bundle_id"] = serde_json::Value::Null;
        assert!(validate_readiness(&ready, 41, "com.example.host", deadline).is_err());
        ready["metadata"]["host_bundle_id"] = serde_json::json!("com.example.other");
        assert!(matches!(
            validate_readiness(&ready, 41, "com.example.host", deadline),
            Err(DriverError::Protocol { reason })
                if reason.contains("metadata host bundle id")
        ));
        ready["metadata"]["host_bundle_id"] = serde_json::json!("com.example.host");
        ready["metadata"]["contract_version"] = serde_json::json!("incompatible");
        assert!(matches!(
            validate_readiness(&ready, 41, "com.example.host", deadline),
            Err(DriverError::Protocol { reason })
                if reason.contains("contract version")
        ));
    }

    #[test]
    fn private_worker_environment_validation_is_bounded_by_size_and_deadline() {
        let variable = EmbeddedEnvironmentVariable {
            name: "LANG".into(),
            value: "C.UTF-8".into(),
        };
        let too_many = vec![variable.clone(); PRIVATE_WORKER_MAX_ENVIRONMENT_ENTRIES + 1];
        assert!(matches!(
            validate_worker_environment(
                &too_many,
                Instant::now() + Duration::from_secs(1)
            ),
            Err(DriverError::Configuration { reason }) if reason.contains("entries")
        ));

        let oversized = [EmbeddedEnvironmentVariable {
            name: "LANG".into(),
            value: "x".repeat(PRIVATE_WORKER_MAX_ENVIRONMENT_VALUE_BYTES + 1),
        }];
        assert!(matches!(
            validate_worker_environment(
                &oversized,
                Instant::now() + Duration::from_secs(1)
            ),
            Err(DriverError::Configuration { reason }) if reason.contains("bytes")
        ));

        assert!(matches!(
            validate_worker_environment(&[variable], Instant::now()),
            Err(DriverError::ActionInterrupted {
                completion: ActionCompletion::NotStarted,
                ..
            })
        ));
    }

    #[cfg(target_os = "linux")]
    fn linux_shell_plan(script: &str) -> super::LinuxSpawnPlan {
        super::prepare_linux_spawn_plan(
            "/bin/sh",
            &["-c".into(), script.into()],
            &[],
            std::process::id() as libc::pid_t,
            false,
        )
        .unwrap()
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_clone3_spawn_returns_atomic_pidfd_and_private_stdio() {
        let plan = linux_shell_plan("IFS= read -r line; printf '%s\\n' \"$line\"; exec sleep 30");
        let (mut child, mut guard) =
            super::spawn_contained_worker_until(plan, Instant::now() + Duration::from_secs(2))
                .unwrap();
        super::exchange_process_binding_until(
            &mut child,
            "atomic-generation",
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap();
        let pidfd = guard.take_pidfd().unwrap();
        assert!(super::linux_terminate_and_reap_until(
            &pidfd,
            Instant::now() + Duration::from_secs(1)
        )
        .unwrap());
        drop(child);
        drop(guard);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_pre_exec_death_and_stop_are_bounded_and_reaped() {
        for action in [
            super::LinuxChildTestAction::KillBeforeExec,
            super::LinuxChildTestAction::StopAfterPdeathsig,
        ] {
            let mut plan = linux_shell_plan("exec sleep 30");
            plan.test_action = action;
            let (cleanup_started_tx, cleanup_started_rx) = std::sync::mpsc::sync_channel(1);
            let (cleanup_finished_tx, cleanup_finished_rx) = std::sync::mpsc::sync_channel(1);
            let (reaped_tx, reaped_rx) = std::sync::mpsc::sync_channel(1);
            plan.test_cleanup_control = Some(super::LinuxCleanupTestControl {
                started: cleanup_started_tx,
                finished: cleanup_finished_tx,
                force_registry: false,
                reaped: reaped_tx,
            });
            let started = Instant::now();
            let result = super::spawn_contained_worker_until(
                plan,
                Instant::now() + Duration::from_millis(350),
            );
            assert!(
                result.is_err(),
                "pre-exec action {action:?} unexpectedly launched"
            );
            assert!(
                started.elapsed() < Duration::from_secs(1),
                "pre-exec failure escaped its public deadline"
            );
            cleanup_started_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("exact cleanup owner did not start");
            assert!(
                cleanup_finished_rx
                    .recv_timeout(Duration::from_secs(1))
                    .expect("exact cleanup owner did not finish"),
                "ordinary pre-exec cleanup unexpectedly required registry fallback"
            );
            reaped_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("exact pidfd target was not reaped");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_executable_open_and_execveat_report_structured_enoent_and_eacces() {
        use std::os::unix::fs::PermissionsExt;

        let missing_error = super::prepare_linux_spawn_plan(
            "/definitely/missing/cua-private-worker",
            &[],
            &[],
            std::process::id() as libc::pid_t,
            false,
        )
        .err()
        .expect("missing executable unexpectedly opened");
        assert!(missing_error.to_string().contains("os error 2"));

        let directory = tempfile::tempdir().unwrap();
        let denied_path = directory.path().join("denied-worker");
        std::fs::copy("/bin/sh", &denied_path).unwrap();
        let denied = super::prepare_linux_spawn_plan(
            denied_path.to_str().unwrap(),
            &[],
            &[],
            std::process::id() as libc::pid_t,
            false,
        )
        .unwrap();
        std::fs::set_permissions(&denied_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let denied_error =
            super::spawn_contained_worker_until(denied, Instant::now() + Duration::from_secs(1))
                .unwrap_err();
        assert!(denied_error.to_string().contains("execveat"));
        assert!(denied_error.to_string().contains("os error 13"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_rejects_privileged_and_script_executables_before_clone() {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        for (name, privileged_bit) in [
            ("setuid-worker", libc::S_ISUID),
            ("setgid-worker", libc::S_ISGID),
        ] {
            let path = directory.path().join(name);
            std::fs::copy("/bin/true", &path).unwrap();
            std::fs::set_permissions(
                &path,
                std::fs::Permissions::from_mode(0o755 | privileged_bit),
            )
            .unwrap();
            let error = super::prepare_linux_spawn_plan(
                path.to_str().unwrap(),
                &[],
                &[],
                std::process::id() as libc::pid_t,
                false,
            )
            .err()
            .expect("privileged executable unexpectedly admitted");
            assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
            assert!(error.to_string().contains("setuid or setgid"));
        }

        let changed_path = directory.path().join("post-admission-setuid-worker");
        std::fs::copy("/bin/true", &changed_path).unwrap();
        std::fs::set_permissions(&changed_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let changed_plan = super::prepare_linux_spawn_plan(
            changed_path.to_str().unwrap(),
            &[],
            &[],
            std::process::id() as libc::pid_t,
            false,
        )
        .unwrap();
        std::fs::set_permissions(&changed_path, std::fs::Permissions::from_mode(0o4755)).unwrap();
        let error = super::spawn_contained_worker_until(
            changed_plan,
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap_err();
        assert!(error.to_string().contains("executable privilege recheck"));

        let script = directory.path().join("script-worker");
        std::fs::write(&script, b"#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let error = super::prepare_linux_spawn_plan(
            script.to_str().unwrap(),
            &[],
            &[],
            std::process::id() as libc::pid_t,
            false,
        )
        .err()
        .expect("script executable unexpectedly admitted");
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("ELF"));

        let capability_path = directory.path().join("capability-worker");
        std::fs::copy("/bin/true", &capability_path).unwrap();
        let capability_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&capability_path)
            .unwrap();
        // VFS_CAP_REVISION_2 with the effective flag and
        // CAP_NET_BIND_SERVICE in the low permitted word.
        let mut capability = [0_u8; 20];
        capability[..4].copy_from_slice(&0x0200_0001_u32.to_le_bytes());
        capability[4..8].copy_from_slice(&(1_u32 << 10).to_le_bytes());
        let set_result = unsafe {
            libc::fsetxattr(
                capability_file.as_raw_fd(),
                c"security.capability".as_ptr(),
                capability.as_ptr().cast(),
                capability.len(),
                0,
            )
        };
        if set_result == 0 {
            drop(capability_file);
            let error = super::prepare_linux_spawn_plan(
                capability_path.to_str().unwrap(),
                &[],
                &[],
                std::process::id() as libc::pid_t,
                false,
            )
            .err()
            .expect("file-capability executable unexpectedly admitted");
            assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
            assert!(error.to_string().contains("file capabilities"));
        } else {
            assert!(matches!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::EPERM) | Some(libc::EACCES) | Some(libc::EOPNOTSUPP)
            ));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_executes_the_opened_object_after_its_path_is_substituted() {
        use std::os::unix::fs::symlink;
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let executable_path = directory.path().join("worker");
        let admitted_path = directory.path().join("admitted-worker");
        // Admit the stable system executable inode directly. Copying an ELF
        // into a fresh writable inode can race Linux's ETXTBSY accounting when
        // this test runs alongside the default parallel harness, obscuring the
        // pathname-substitution property this test is meant to prove.
        symlink("/bin/sh", &executable_path).unwrap();
        let plan = super::prepare_linux_spawn_plan(
            executable_path.to_str().unwrap(),
            &[
                "-c".into(),
                "printf 'opened-object\\n'; exec sleep 30".into(),
            ],
            &[],
            std::process::id() as libc::pid_t,
            false,
        )
        .unwrap();
        std::fs::rename(&executable_path, &admitted_path).unwrap();
        std::fs::copy("/bin/false", &executable_path).unwrap();
        std::fs::set_permissions(&executable_path, std::fs::Permissions::from_mode(0o755)).unwrap();

        let (mut child, mut guard) =
            super::spawn_contained_worker_until(plan, Instant::now() + Duration::from_secs(2))
                .unwrap();
        let mut output = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut output)
            .unwrap();
        assert_eq!(output, "opened-object\n");
        let pidfd = guard.take_pidfd().unwrap();
        assert!(super::linux_terminate_and_reap_until(
            &pidfd,
            Instant::now() + Duration::from_secs(1)
        )
        .unwrap());
        drop(child);
        drop(guard);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_exec_sets_no_new_privileges_in_the_real_child() {
        let plan = linux_shell_plan(
            "grep -q '^NoNewPrivs:[[:space:]]*1$' /proc/self/status; \
             printf 'no-new-privs\\n'; exec sleep 30",
        );
        let (mut child, mut guard) =
            super::spawn_contained_worker_until(plan, Instant::now() + Duration::from_secs(2))
                .unwrap();
        let mut output = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut output)
            .unwrap();
        assert_eq!(output, "no-new-privs\n");
        let pidfd = guard.take_pidfd().unwrap();
        assert!(super::linux_terminate_and_reap_until(
            &pidfd,
            Instant::now() + Duration::from_secs(1)
        )
        .unwrap());
        drop(child);
        drop(guard);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_unsupported_syscalls_fail_closed_without_fallback() {
        for errno in [libc::ENOSYS, libc::EINVAL, libc::EPERM] {
            let mut plan = linux_shell_plan("exit 99");
            plan.test_clone3_errno = Some(errno);
            let error =
                super::spawn_contained_worker_until(plan, Instant::now() + Duration::from_secs(1))
                    .unwrap_err();
            assert!(error.to_string().contains("clone3(CLONE_PIDFD)"));
        }
        let preflight = super::linux_preflight_close_range_with(Some(libc::ENOSYS)).unwrap_err();
        assert_eq!(preflight.raw_os_error(), Some(libc::ENOSYS));

        let mut plan = linux_shell_plan("exit 99");
        plan.test_action = super::LinuxChildTestAction::CloseRangeUnsupported;
        let error =
            super::spawn_contained_worker_until(plan, Instant::now() + Duration::from_secs(1))
                .unwrap_err();
        assert!(error.to_string().contains("close_range"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_abandoned_result_handoff_retains_an_exact_cleanup_owner() {
        let mut plan = linux_shell_plan("exec sleep 30");
        plan.test_abandon_handoff = true;
        let started = Instant::now();
        let error =
            super::spawn_contained_worker_until(plan, Instant::now() + Duration::from_millis(500))
                .unwrap_err();
        assert!(error.to_string().contains("handoff was abandoned"));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_public_timeout_transfers_to_bounded_registry_and_reaps() {
        let (cleanup_started_tx, cleanup_started_rx) = std::sync::mpsc::sync_channel(1);
        let (cleanup_finished_tx, cleanup_finished_rx) = std::sync::mpsc::sync_channel(1);
        let (reaped_tx, reaped_rx) = std::sync::mpsc::sync_channel(1);
        let mut plan = linux_shell_plan("exec sleep 30");
        plan.test_action = super::LinuxChildTestAction::StopAfterPdeathsig;
        plan.test_cleanup_control = Some(super::LinuxCleanupTestControl {
            started: cleanup_started_tx,
            finished: cleanup_finished_tx,
            reaped: reaped_tx,
            force_registry: true,
        });

        let public_started = Instant::now();
        let error =
            super::spawn_contained_worker_until(plan, Instant::now() + Duration::from_millis(150))
                .unwrap_err();
        assert!(
            error.kind() == std::io::ErrorKind::TimedOut
                || error.to_string().contains("startup deadline")
        );
        assert!(public_started.elapsed() < Duration::from_secs(1));

        let _retained_pidfd = cleanup_started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("bounded parent helper did not retain the cleanup pidfd");
        assert!(
            !cleanup_finished_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("bounded parent helper did not finish cleanup"),
            "test did not exercise registry fallback"
        );
        reaped_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("bounded registry did not complete exact waitid reaping");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn private_worker_closed_stdio_helper() {
        if std::env::var_os(CLOSED_STDIO_HELPER_ENV).is_none() {
            return;
        }
        unsafe {
            libc::close(0);
            libc::close(1);
            libc::close(2);
        }
        let plan = linux_shell_plan("IFS= read -r line; printf '%s\\n' \"$line\"; exec sleep 30");
        let (mut child, mut guard) =
            super::spawn_contained_worker_until(plan, Instant::now() + Duration::from_secs(2))
                .unwrap();
        for descriptor in 0..=2 {
            assert_eq!(unsafe { libc::fcntl(descriptor, libc::F_GETFD) }, -1);
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::EBADF)
            );
        }
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(b"private-stdio\n")
            .unwrap();
        child.stdin.as_mut().unwrap().flush().unwrap();
        let mut output = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut output)
            .unwrap();
        assert_eq!(output, "private-stdio\n");
        let pidfd = guard.take_pidfd().unwrap();
        assert!(super::linux_terminate_and_reap_until(
            &pidfd,
            Instant::now() + Duration::from_secs(1)
        )
        .unwrap());
        drop(child);
        drop(guard);
        for descriptor in 0..=2 {
            assert_eq!(unsafe { libc::fcntl(descriptor, libc::F_GETFD) }, -1);
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::EBADF)
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_spawn_wires_stdio_when_the_host_descriptors_started_closed() {
        let status = Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("worker::tests::private_worker_closed_stdio_helper")
            .arg("--nocapture")
            .env(CLOSED_STDIO_HELPER_ENV, "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(
            status.success(),
            "closed-stdio helper failed with {status:?}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn private_worker_low_fd_reuse_helper() {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

        if std::env::var_os(LOW_FD_REUSE_HELPER_ENV).is_none() {
            return;
        }
        unsafe { libc::close(0) };
        let descriptor = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) };
        assert_eq!(descriptor, 0);
        let moved = super::linux_fd_above_stdio(unsafe { OwnedFd::from_raw_fd(descriptor) })
            .expect("move low descriptor above stdio");
        assert!(moved.as_raw_fd() >= 3);

        let (opened_tx, opened_rx) = std::sync::mpsc::sync_channel(0);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(0);
        let opener = std::thread::spawn(move || {
            let raw =
                unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC, 0) };
            assert!(
                raw >= 0,
                "open reused stdio slot: {}",
                std::io::Error::last_os_error()
            );
            let reused = unsafe { OwnedFd::from_raw_fd(raw) };
            assert_eq!(reused.as_raw_fd(), 0);
            opened_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            assert!(unsafe { libc::fcntl(reused.as_raw_fd(), libc::F_GETFD) } >= 0);
        });
        opened_rx.recv().unwrap();
        drop(moved);
        assert!(
            unsafe { libc::fcntl(0, libc::F_GETFD) } >= 0,
            "dropping the moved capability closed another thread's reused fd"
        );
        release_tx.send(()).unwrap();
        opener.join().unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_low_fd_move_never_closes_a_concurrent_reuse() {
        let status = Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("worker::tests::private_worker_low_fd_reuse_helper")
            .arg("--nocapture")
            .env(LOW_FD_REUSE_HELPER_ENV, "1")
            .stdin(Stdio::null())
            .status()
            .unwrap();
        assert!(
            status.success(),
            "low-fd reuse helper failed with {status:?}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn private_worker_high_fd_helper() {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

        if std::env::var_os(HIGH_FD_HELPER_ENV).is_none() {
            return;
        }
        let mut descriptors = [-1; 2];
        assert_eq!(unsafe { libc::pipe(descriptors.as_mut_ptr()) }, 0);
        let read_end = unsafe { OwnedFd::from_raw_fd(descriptors[0]) };
        let write_end = unsafe { OwnedFd::from_raw_fd(descriptors[1]) };
        let high_descriptor = unsafe { libc::fcntl(write_end.as_raw_fd(), libc::F_DUPFD, 256) };
        assert!(high_descriptor >= 256);
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
        let lowered_limit = libc::rlimit {
            rlim_cur: high_descriptor as libc::rlim_t,
            rlim_max: original_limit.rlim_max,
        };
        assert_eq!(
            unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lowered_limit) },
            0
        );
        let plan = linux_shell_plan(&format!(
            "test ! -e /proc/self/fd/{high_descriptor}; exec sleep 30"
        ));
        let (child, mut guard) =
            super::spawn_contained_worker_until(plan, Instant::now() + Duration::from_secs(2))
                .unwrap();
        assert_eq!(
            unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &original_limit) },
            0
        );
        drop(high_write_end);
        drop(read_end);
        let pidfd = guard.take_pidfd().unwrap();
        assert!(super::linux_terminate_and_reap_until(
            &pidfd,
            Instant::now() + Duration::from_secs(1)
        )
        .unwrap());
        drop(child);
        drop(guard);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_exec_closes_high_inheritable_fd_above_lowered_soft_limit() {
        let status = Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("worker::tests::private_worker_high_fd_helper")
            .arg("--nocapture")
            .env(HIGH_FD_HELPER_ENV, "1")
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn private_worker_pidfd_auto_reap_helper() {
        let Some(mode) = std::env::var_os(AUTO_REAP_HELPER_ENV) else {
            return;
        };
        // SAFETY: this test runs in its own subprocess and changes only that
        // disposable process's SIGCHLD disposition.
        if mode == "ignore" {
            unsafe {
                libc::signal(libc::SIGCHLD, libc::SIG_IGN);
            }
        } else {
            let mut action = unsafe { std::mem::zeroed::<libc::sigaction>() };
            action.sa_sigaction = libc::SIG_DFL;
            action.sa_flags = libc::SA_NOCLDWAIT;
            unsafe {
                libc::sigemptyset(&mut action.sa_mask);
                assert_eq!(
                    libc::sigaction(libc::SIGCHLD, &action, std::ptr::null_mut()),
                    0
                );
            }
        }
        let plan = linux_shell_plan("exec sleep 30");
        let (child, mut guard) =
            super::spawn_contained_worker_until(plan, Instant::now() + Duration::from_secs(1))
                .unwrap();
        let pidfd = guard.take_pidfd().unwrap();
        assert!(super::linux_terminate_and_reap_until(
            &pidfd,
            Instant::now() + Duration::from_secs(1)
        )
        .unwrap());
        drop(child);
        drop(guard);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pidfd_capture_remains_identity_safe_when_sigchld_auto_reaps_the_child() {
        for mode in ["ignore", "no_cldwait"] {
            let status = Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg("worker::tests::private_worker_pidfd_auto_reap_helper")
                .arg("--nocapture")
                .env(AUTO_REAP_HELPER_ENV, mode)
                .status()
                .unwrap();
            assert!(status.success(), "SIGCHLD mode {mode} failed");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn private_worker_foreign_waiter_helper() {
        if std::env::var_os(FOREIGN_WAITER_HELPER_ENV).is_none() {
            return;
        }
        let plan = linux_shell_plan("exec sleep 30");
        let (child, mut guard) =
            super::spawn_contained_worker_until(plan, Instant::now() + Duration::from_secs(2))
                .unwrap();
        let pidfd = guard.take_pidfd().unwrap();
        let mut sentinel = Command::new("/bin/sh")
            .args(["-c", "exec sleep 30"])
            .spawn()
            .unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let waiter_stop = Arc::clone(&stop);
        let waiter = std::thread::spawn(move || {
            while !waiter_stop.load(std::sync::atomic::Ordering::Acquire) {
                let mut status = 0;
                unsafe {
                    libc::waitpid(-1, &mut status, libc::WNOHANG);
                }
                std::thread::yield_now();
            }
        });
        assert!(super::linux_terminate_and_reap_until(
            &pidfd,
            Instant::now() + Duration::from_secs(1)
        )
        .unwrap());
        stop.store(true, std::sync::atomic::Ordering::Release);
        waiter.join().unwrap();
        assert!(
            sentinel.try_wait().unwrap().is_none(),
            "exact pidfd cleanup affected the unrelated sentinel child"
        );
        sentinel.kill().unwrap();
        sentinel.wait().unwrap();
        drop(child);
        drop(guard);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn foreign_waitpid_cannot_redirect_private_worker_termination() {
        let status = Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("worker::tests::private_worker_foreign_waiter_helper")
            .arg("--nocapture")
            .env(FOREIGN_WAITER_HELPER_ENV, "1")
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn private_worker_pdeath_helper() {
        if std::env::var_os(PDEATH_HELPER_ENV).is_none() {
            return;
        }
        let stopped = std::env::var(PDEATH_HELPER_ENV).as_deref() == Ok("stop");
        let mut plan = linux_shell_plan("echo $$; exec sleep 30");
        if stopped {
            plan.test_action = super::LinuxChildTestAction::StopAfterPdeathsig;
        }
        let (mut child, _guard) = super::spawn_contained_worker_until(
            plan,
            Instant::now()
                + if stopped {
                    Duration::from_secs(30)
                } else {
                    Duration::from_secs(2)
                },
        )
        .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        println!("pdeath-child:{}", line.trim());
        std::io::stdout().flush().unwrap();
        std::thread::sleep(Duration::from_secs(30));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_worker_dies_when_spawning_host_is_sigkilled() {
        let mut host = Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("worker::tests::private_worker_pdeath_helper")
            .arg("--nocapture")
            .env(PDEATH_HELPER_ENV, "1")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut output = BufReader::new(host.stdout.take().unwrap());
        let child_pid = loop {
            let mut line = String::new();
            assert_ne!(output.read_line(&mut line).unwrap(), 0);
            if let Some(pid) = line.trim().strip_prefix("pdeath-child:") {
                break pid.parse::<libc::pid_t>().unwrap();
            }
        };
        host.kill().unwrap();
        host.wait().unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let alive = unsafe { libc::kill(child_pid, 0) } == 0;
            if !alive {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "contained worker survived host SIGKILL"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_stopped_worker_dies_when_spawning_host_is_sigkilled() {
        let mut host = Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("worker::tests::private_worker_pdeath_helper")
            .arg("--nocapture")
            .env(PDEATH_HELPER_ENV, "stop")
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let worker_pid = loop {
            let task_directory = format!("/proc/{}/task", host.id());
            let mut discovered = None;
            if let Ok(tasks) = std::fs::read_dir(task_directory) {
                for task in tasks.flatten() {
                    let children = task.path().join("children");
                    if let Ok(contents) = std::fs::read_to_string(children) {
                        if let Some(pid) = contents
                            .split_whitespace()
                            .find_map(|pid| pid.parse::<libc::pid_t>().ok())
                        {
                            discovered = Some(pid);
                            break;
                        }
                    }
                }
            }
            if let Some(pid) = discovered {
                break pid;
            }
            assert!(
                Instant::now() < deadline,
                "did not observe the stopped post-PDEATHSIG worker"
            );
            std::thread::sleep(Duration::from_millis(5));
        };
        host.kill().unwrap();
        host.wait().unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while unsafe { libc::kill(worker_pid, 0) } == 0 {
            assert!(
                Instant::now() < deadline,
                "worker stopped after PDEATHSIG arm survived host SIGKILL"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn private_worker_wedge_helper() {
        if std::env::var_os(WEDGE_HELPER_ENV).is_none() {
            return;
        }

        #[cfg(target_os = "macos")]
        {
            let mut stream = std::os::unix::net::UnixStream::connect(
                std::env::var(WEDGE_TERMINATION_SOCKET_ENV).unwrap(),
            )
            .unwrap();
            stream.write_all(WEDGE_GENERATION.as_bytes()).unwrap();
            stream.write_all(b"\n").unwrap();
            stream.flush().unwrap();
            std::thread::spawn(move || {
                let mut byte = 0_u8;
                let _ = std::io::Read::read(&mut stream, std::slice::from_mut(&mut byte));
                unsafe { libc::_exit(137) }
            });
        }

        println!("{WEDGE_HELPER_READY}");
        std::io::stdout().flush().unwrap();
        let mut request = String::new();
        std::io::stdin().read_line(&mut request).unwrap();
        eprintln!("{WEDGE_HELPER_REQUEST}");
        std::io::stderr().flush().unwrap();
        // Long relative to the 75ms test budget, but short enough that a
        // regression fails promptly instead of waiting the production 120s.
        std::thread::sleep(Duration::from_secs(2));
    }

    #[test]
    fn ordinary_request_timeout_bounds_wait_behind_request_lock() {
        let timeout = Duration::from_millis(75);
        let (client, _stderr) = spawn_wedged_client(timeout);
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let holder = {
            let client = client.clone();
            std::thread::spawn(move || {
                let _request = client.request.lock().unwrap();
                ready_tx.send(()).unwrap();
                std::thread::sleep(Duration::from_millis(500));
            })
        };
        ready_rx.recv().unwrap();

        let started = Instant::now();
        let result = client.request_sync_with_timeout("locked", None, None, None, timeout);
        let elapsed = started.elapsed();

        assert_bounded(elapsed, timeout);
        assert!(matches!(
            result,
            Err(DriverError::ActionInterrupted {
                completion: ActionCompletion::NotStarted,
                ..
            })
        ));
        holder.join().unwrap();
        assert!(client.is_available());
        drop(client);
    }

    #[test]
    fn ordinary_request_timeout_includes_blocking_pool_queue_time() {
        let timeout = Duration::from_millis(75);
        let (client, _stderr) = spawn_wedged_client(timeout);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(1)
            .enable_time()
            .build()
            .unwrap();
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let blocker = runtime.spawn_blocking(move || {
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        entered_rx.recv().unwrap();

        let started = Instant::now();
        let result = runtime
            .block_on(client.request_async_with_timeout("queued", None, None, None, timeout));
        let elapsed = started.elapsed();

        assert_bounded(elapsed, timeout);
        assert!(matches!(
            result,
            Err(DriverError::ActionInterrupted {
                completion: ActionCompletion::NotStarted,
                ..
            })
        ));
        assert!(!client.is_available());
        release_tx.send(()).unwrap();
        runtime.block_on(blocker).unwrap();
    }

    #[test]
    fn shutdown_timeout_bounds_wedged_handshake() {
        let timeout = Duration::from_millis(75);
        let (client, _stderr) = spawn_wedged_client(timeout);

        let started = Instant::now();
        let result = client.shutdown_sync();
        let elapsed = started.elapsed();

        assert_bounded(elapsed, timeout);
        assert!(matches!(
            result,
            Err(DriverError::ActionInterrupted {
                completion: ActionCompletion::Unknown,
                ..
            })
        ));
        assert!(!client.is_available());
    }

    #[test]
    fn async_shutdown_timeout_reports_unknown_after_wedged_transmission() {
        let timeout = Duration::from_millis(75);
        let (client, _stderr) = spawn_wedged_client(timeout);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();

        let started = Instant::now();
        let result = runtime.block_on(client.shutdown());
        let elapsed = started.elapsed();

        assert_bounded(elapsed, timeout);
        assert!(matches!(
            result,
            Err(DriverError::ActionInterrupted {
                completion: ActionCompletion::Unknown,
                ..
            })
        ));
        assert!(!client.is_available());
    }

    #[test]
    fn shutdown_timeout_includes_blocking_pool_queue_time() {
        let timeout = Duration::from_millis(75);
        let (client, _stderr) = spawn_wedged_client(timeout);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(1)
            .enable_time()
            .build()
            .unwrap();
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let blocker = runtime.spawn_blocking(move || {
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        entered_rx.recv().unwrap();

        let started = Instant::now();
        let result = runtime.block_on(client.shutdown());
        let elapsed = started.elapsed();

        assert_bounded(elapsed, timeout);
        assert!(matches!(
            result,
            Err(DriverError::ActionInterrupted {
                completion: ActionCompletion::NotStarted,
                ..
            })
        ));
        assert!(!client.is_available());
        release_tx.send(()).unwrap();
        runtime.block_on(blocker).unwrap();
    }

    #[test]
    fn shutdown_timeout_bounds_wait_behind_in_flight_request() {
        let timeout = Duration::from_millis(75);
        let (client, _stderr) = spawn_wedged_client(timeout);
        let (lock_held_tx, lock_held_rx) = std::sync::mpsc::sync_channel(1);
        *client.request_lock_signal.lock().unwrap() = Some(lock_held_tx);
        let requester = {
            let client = client.clone();
            std::thread::spawn(move || client.request_sync("wedged", None, None, None))
        };
        lock_held_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("request did not prove it held the exchange lock");

        let started = Instant::now();
        let result = client.shutdown_sync();
        let elapsed = started.elapsed();

        assert_bounded(elapsed, timeout);
        assert!(matches!(
            result,
            Err(DriverError::ActionInterrupted {
                completion: ActionCompletion::NotStarted,
                ..
            })
        ));
        assert!(matches!(
            requester.join().unwrap(),
            Err(DriverError::ActionInterrupted {
                completion: ActionCompletion::Unknown,
                ..
            })
        ));
        assert!(!client.is_available());
    }

    #[test]
    fn shutdown_timeout_bounds_wait_behind_process_lock() {
        let timeout = Duration::from_millis(75);
        let (client, _stderr) = spawn_wedged_client(timeout);
        #[cfg(target_os = "linux")]
        let worker_pidfd = super::linux_duplicate_pidfd(&client.worker_pidfd).unwrap();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let holder = {
            let client = client.clone();
            std::thread::spawn(move || {
                let _process = client.process.lock().unwrap();
                ready_tx.send(()).unwrap();
                std::thread::sleep(Duration::from_millis(500));
            })
        };
        ready_rx.recv().unwrap();

        let started = Instant::now();
        let result = client.shutdown_sync();
        let elapsed = started.elapsed();

        assert_bounded(elapsed, timeout);
        assert!(matches!(
            result,
            Err(DriverError::ActionInterrupted {
                completion: ActionCompletion::NotStarted,
                ..
            })
        ));
        holder.join().unwrap();
        #[cfg(target_os = "linux")]
        {
            assert!(super::linux_reap_pidfd_until(
                &worker_pidfd,
                Instant::now() + Duration::from_secs(1)
            )
            .unwrap());
        }
        assert!(!client.is_available());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn drop_recovers_a_poisoned_process_lock_and_terminates_the_worker() {
        let (client, _stderr) = spawn_wedged_client(Duration::from_millis(75));
        let worker_pidfd = super::linux_duplicate_pidfd(&client.worker_pidfd).unwrap();
        let poisoner = {
            let client = client.clone();
            std::thread::spawn(move || {
                let _process = client.process.lock().unwrap();
                panic!("poison private-worker process lock");
            })
        };
        assert!(poisoner.join().is_err());

        drop(client);
        assert!(super::linux_reap_pidfd_until(
            &worker_pidfd,
            Instant::now() + Duration::from_secs(1)
        )
        .unwrap());
    }

    #[test]
    fn managed_authorization_is_inherited_but_cannot_be_overridden() {
        for name in [
            "CUA_DRIVER_DISABLE_UNRESTRICTED",
            "CUA_DRIVER_MANAGED_POLICY_FILE",
            "CUA_DRIVER_SESSION_POLICY_FILE",
        ] {
            assert!(inherited_managed_environment_name(name));
            assert!(
                !allowed_environment_name(name),
                "caller-provided worker environment must not override {name}"
            );
        }
    }

    #[test]
    fn runtime_isolation_is_inherited_but_cannot_be_overridden() {
        for name in [
            "CUA_BROWSER_PROFILE_DIR",
            "CUA_DRIVER_BROWSER_PROFILE_ROOT",
            "CUA_DRIVER_RS_DISABLE_A11Y_ADVERTISE",
            "CUA_DRIVER_RS_ENABLE_WAYLAND",
            "CUA_DRIVER_RS_RECORDING_IDLE_TTL_SECS",
            "CUA_DRIVER_RS_SESSION_IDLE_TTL_SECS",
            "CUA_DRIVER_RS_TELEMETRY_ENABLED",
            "CUA_INJECT_SOCKET",
            "XDG_CONFIG_HOME",
        ] {
            assert!(inherited_runtime_environment_name(name));
            assert!(
                !allowed_environment_name(name),
                "caller-provided worker environment must not override {name}"
            );
        }
        assert!(!inherited_runtime_environment_name("AT_SPI_BUS_ADDRESS"));
    }
}
