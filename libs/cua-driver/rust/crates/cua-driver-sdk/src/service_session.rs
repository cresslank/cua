//! Authenticated, connection-bound session client for an explicit service.
//!
//! The accepted service connection owns exactly one trusted session. Authority
//! is never returned as a bearer value and cannot move to another connection.

use crate::{DriverError, TrustedSessionOptions};
use cua_driver_core::daemon::{DaemonClientKind, DaemonRequest, DaemonResponse};
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
#[cfg(target_os = "windows")]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

#[cfg(unix)]
const SERVICE_REQUEST_DEADLINE: Duration = Duration::from_secs(120);
const RESUME_REGISTRATION_RETRIES: usize = 20;
const RESUME_REGISTRATION_DELAY: Duration = Duration::from_millis(10);
const SERVICE_CLOSE_DEADLINE: Duration = Duration::from_secs(2);
const SERVICE_CLOSE_LOCK_POLL: Duration = Duration::from_millis(5);

#[cfg(unix)]
type ServiceStream = std::os::unix::net::UnixStream;
#[cfg(target_os = "windows")]
type ServiceStream = std::fs::File;

struct ServiceConnection {
    reader: BufReader<ServiceStream>,
    writer: ServiceStream,
    closed: bool,
    resume_credential: String,
}

#[cfg(unix)]
struct ActiveIoGuard<'a> {
    slot: &'a Mutex<Option<ServiceStream>>,
}

#[cfg(unix)]
impl Drop for ActiveIoGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut slot) = self.slot.lock() {
            slot.take();
        }
    }
}

#[cfg(target_os = "windows")]
struct ActiveIoGuard<'a> {
    slot: &'a AtomicU64,
}

#[cfg(target_os = "windows")]
impl Drop for ActiveIoGuard<'_> {
    fn drop(&mut self) {
        self.slot.store(0, Ordering::Release);
    }
}

pub(crate) struct ServiceSessionClient {
    socket_path: String,
    connection: Mutex<ServiceConnection>,
    closing: AtomicBool,
    #[cfg(unix)]
    active_io: Mutex<Option<ServiceStream>>,
    #[cfg(target_os = "windows")]
    active_io_thread: AtomicU64,
}

impl ServiceSessionClient {
    pub(crate) fn connect_and_bind(
        socket_path: String,
        options: TrustedSessionOptions,
        client_kind: DaemonClientKind,
    ) -> Result<Arc<Self>, DriverError> {
        let metadata =
            cua_driver_core::daemon::request_daemon_metadata(&socket_path).map_err(|error| {
                DriverError::Transport {
                    socket_path: socket_path.clone(),
                    reason: format!("read service compatibility metadata: {error}"),
                }
            })?;
        crate::validate_daemon_metadata(&metadata)?;
        let stream = connect(&socket_path)?;
        let writer = stream.try_clone().map_err(|error| DriverError::Transport {
            socket_path: socket_path.clone(),
            reason: format!("clone trusted service connection: {error}"),
        })?;
        let client = Arc::new(Self {
            socket_path,
            connection: Mutex::new(ServiceConnection {
                reader: BufReader::new(stream),
                writer,
                closed: false,
                resume_credential: String::new(),
            }),
            closing: AtomicBool::new(false),
            #[cfg(unix)]
            active_io: Mutex::new(None),
            #[cfg(target_os = "windows")]
            active_io_thread: AtomicU64::new(0),
        });
        let arguments = serde_json::to_value(options).map_err(|error| DriverError::Protocol {
            reason: format!("serialize trusted service session options: {error}"),
        })?;
        let bound = client.request(DaemonRequest {
            method: "trusted_session_begin".into(),
            name: None,
            args: Some(arguments),
            session_id: None,
            observation_origin: None,
            client_kind: Some(client_kind),
        })?;
        let resume_credential = bound
            .get("resume_credential")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| DriverError::Protocol {
                reason: "trusted service bind omitted resume credential".into(),
            })?;
        client.connection.lock().unwrap().resume_credential = resume_credential.to_owned();
        Ok(client)
    }

    pub(crate) async fn invoke(
        self: &Arc<Self>,
        name: &str,
        arguments: Value,
    ) -> Result<Value, DriverError> {
        let client = self.clone();
        let name = name.to_owned();
        tokio::task::spawn_blocking(move || {
            client.request(DaemonRequest {
                method: "trusted_session_call".into(),
                name: Some(name),
                args: Some(arguments),
                session_id: None,
                observation_origin: None,
                client_kind: None,
            })
        })
        .await
        .map_err(|error| DriverError::Protocol {
            reason: format!("join trusted service request: {error}"),
        })?
    }

    pub(crate) fn close(&self) {
        if let Err(error) = self.close_until(Instant::now() + SERVICE_CLOSE_DEADLINE) {
            tracing::warn!(error = %error, "trusted service session cleanup did not complete before its deadline");
        }
    }

    fn close_until(&self, deadline: Instant) -> Result<(), DriverError> {
        self.closing.store(true, Ordering::Release);
        self.interrupt_active_io();
        let mut connection = self.lock_connection_until(deadline)?;
        if connection.closed {
            self.resume_connection_until(&mut connection, deadline)?;
        }
        let request = DaemonRequest {
            method: "trusted_session_end".into(),
            name: None,
            args: None,
            session_id: None,
            observation_origin: None,
            client_kind: None,
        };
        let close_result =
            close_request_until(&mut connection, &request, deadline).map_err(|error| {
                DriverError::Transport {
                    socket_path: self.socket_path.clone(),
                    reason: format!("bounded trusted service close: {error}"),
                }
            });
        connection.closed = true;
        close_result.map(|_| ())
    }

    fn lock_connection_until(
        &self,
        deadline: Instant,
    ) -> Result<MutexGuard<'_, ServiceConnection>, DriverError> {
        loop {
            match self.connection.try_lock() {
                Ok(connection) => return Ok(connection),
                Err(TryLockError::Poisoned(_)) => {
                    return Err(DriverError::Protocol {
                        reason: "trusted service connection lock is poisoned".into(),
                    })
                }
                Err(TryLockError::WouldBlock) => {
                    let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                        return Err(DriverError::Shutdown);
                    };
                    std::thread::sleep(remaining.min(SERVICE_CLOSE_LOCK_POLL));
                }
            }
        }
    }

    pub(crate) fn abandon(&self) {
        self.close();
    }

    #[cfg(unix)]
    fn request_response_active(
        &self,
        connection: &mut ServiceConnection,
        request: &DaemonRequest,
    ) -> std::io::Result<Option<String>> {
        let interrupt = connection.reader.get_ref().try_clone()?;
        *self
            .active_io
            .lock()
            .map_err(|_| std::io::Error::other("trusted service active-I/O slot is poisoned"))? =
            Some(interrupt);
        let _active = ActiveIoGuard {
            slot: &self.active_io,
        };
        if self.closing.load(Ordering::Acquire) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "trusted service session is closing",
            ));
        }
        write_request(&mut connection.writer, request)?;
        read_response_line(&mut connection.reader)
    }

    #[cfg(target_os = "windows")]
    fn request_response_active(
        &self,
        connection: &mut ServiceConnection,
        request: &DaemonRequest,
    ) -> std::io::Result<Option<String>> {
        use windows::Win32::System::Threading::GetCurrentThreadId;
        self.active_io_thread
            .store(unsafe { GetCurrentThreadId() } as u64, Ordering::Release);
        let _active = ActiveIoGuard {
            slot: &self.active_io_thread,
        };
        if self.closing.load(Ordering::Acquire) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "trusted service session is closing",
            ));
        }
        write_request(&mut connection.writer, request)?;
        read_response_line(&mut connection.reader)
    }

    #[cfg(unix)]
    fn interrupt_active_io(&self) {
        use std::net::Shutdown;
        if let Ok(mut active) = self.active_io.lock() {
            if let Some(stream) = active.take() {
                let _ = stream.shutdown(Shutdown::Both);
            }
        }
    }

    #[cfg(target_os = "windows")]
    fn interrupt_active_io(&self) {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Threading::{OpenThread, THREAD_TERMINATE};
        use windows::Win32::System::IO::CancelSynchronousIo;
        let thread_id = self.active_io_thread.load(Ordering::Acquire) as u32;
        if thread_id == 0 {
            return;
        }
        if let Ok(thread) = unsafe { OpenThread(THREAD_TERMINATE, false, thread_id) } {
            let _ = unsafe { CancelSynchronousIo(thread) };
            let _ = unsafe { CloseHandle(thread) };
        }
    }

    fn request(&self, request: DaemonRequest) -> Result<Value, DriverError> {
        if self.closing.load(Ordering::Acquire) {
            return Err(DriverError::Shutdown);
        }
        let mut connection = self.connection.lock().unwrap();
        if self.closing.load(Ordering::Acquire) {
            return Err(DriverError::Shutdown);
        }
        if connection.closed {
            self.resume_connection(&mut connection)?;
        }
        let action_call = request.method == "trusted_session_call";
        let line = self
            .request_response_active(&mut connection, &request)
            .map_err(|error| {
                connection.closed = true;
                self.connection_failure(
                    action_call,
                    format!("trusted service request interrupted: {error}"),
                )
            })?
            .ok_or_else(|| {
                connection.closed = true;
                self.connection_failure(action_call, "trusted service connection closed".into())
            })?;
        let response: DaemonResponse = serde_json::from_str(&line).map_err(|error| {
            connection.closed = true;
            if action_call {
                DriverError::ActionInterrupted {
                    completion: crate::worker::ActionCompletion::Unknown,
                    reason: format!("parse trusted service response: {error}"),
                }
            } else {
                DriverError::Protocol {
                    reason: format!("parse trusted service response: {error}"),
                }
            }
        })?;
        if !response.ok {
            return Err(DriverError::Tool {
                tool: request.name.unwrap_or_else(|| request.method.clone()),
                message: response
                    .error
                    .unwrap_or_else(|| "trusted service request failed".into()),
                error_code: response
                    .exit_code
                    .map(|code| code.to_string())
                    .unwrap_or_default(),
            });
        }
        Ok(response.result.unwrap_or(Value::Null))
    }

    fn resume_connection(&self, connection: &mut ServiceConnection) -> Result<(), DriverError> {
        self.resume_connection_inner(connection, None)
    }

    fn resume_connection_until(
        &self,
        connection: &mut ServiceConnection,
        deadline: Instant,
    ) -> Result<(), DriverError> {
        self.resume_connection_inner(connection, Some(deadline))
    }

    fn resume_connection_inner(
        &self,
        connection: &mut ServiceConnection,
        deadline: Option<Instant>,
    ) -> Result<(), DriverError> {
        if connection.resume_credential.is_empty() {
            return Err(DriverError::Shutdown);
        }
        let credential = connection.resume_credential.clone();
        for attempt in 0..=RESUME_REGISTRATION_RETRIES {
            let stream = match deadline {
                Some(deadline) => connect_until(&self.socket_path, deadline),
                None => connect(&self.socket_path),
            }?;
            let writer = stream.try_clone().map_err(|error| DriverError::Transport {
                socket_path: self.socket_path.clone(),
                reason: format!("clone resumed trusted service connection: {error}"),
            })?;

            // Replace both handles before presenting the credential. Dropping
            // the old client handles makes the daemon observe EOF and publish
            // the detachable lease on Unix sockets and Windows named pipes.
            connection.reader = BufReader::new(stream);
            connection.writer = writer;
            let request = DaemonRequest {
                method: "trusted_session_resume".into(),
                name: None,
                args: Some(serde_json::json!({
                    "resume_credential": credential,
                })),
                session_id: None,
                observation_origin: None,
                client_kind: None,
            };
            let line = match deadline {
                Some(deadline) => request_response_until(connection, &request, deadline),
                None => self.request_response_active(connection, &request),
            }
            .map_err(|error| DriverError::Transport {
                socket_path: self.socket_path.clone(),
                reason: format!("resume trusted service session: {error}"),
            })?
            .ok_or_else(|| DriverError::Transport {
                socket_path: self.socket_path.clone(),
                reason: "trusted service closed during resume".into(),
            })?;
            let response: DaemonResponse =
                serde_json::from_str(&line).map_err(|error| DriverError::Protocol {
                    reason: format!("parse trusted session resume response: {error}"),
                })?;
            if response.ok {
                let rotated = response
                    .result
                    .as_ref()
                    .and_then(|value| value.get("resume_credential"))
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| DriverError::Protocol {
                        reason: "trusted session resume omitted rotated credential".into(),
                    })?;
                connection.resume_credential = rotated.to_owned();
                connection.closed = false;
                return Ok(());
            }

            let reason = response
                .error
                .unwrap_or_else(|| "trusted session resume failed".into());
            let registration_race = reason.contains("unavailable or already used")
                && attempt < RESUME_REGISTRATION_RETRIES;
            if registration_race {
                if let Some(deadline) = deadline {
                    let remaining = remaining_until(deadline).map_err(|_| DriverError::Shutdown)?;
                    std::thread::sleep(remaining.min(RESUME_REGISTRATION_DELAY));
                } else {
                    std::thread::sleep(RESUME_REGISTRATION_DELAY);
                }
                continue;
            }
            connection.resume_credential.clear();
            return Err(DriverError::Transport {
                socket_path: self.socket_path.clone(),
                reason,
            });
        }
        unreachable!("bounded resume loop returns on its final attempt")
    }

    fn connection_failure(&self, action_call: bool, reason: String) -> DriverError {
        if action_call {
            DriverError::ActionInterrupted {
                completion: crate::worker::ActionCompletion::Unknown,
                reason,
            }
        } else {
            DriverError::Transport {
                socket_path: self.socket_path.clone(),
                reason,
            }
        }
    }
}

impl Drop for ServiceSessionClient {
    fn drop(&mut self) {
        self.abandon();
    }
}

fn remaining_until(deadline: Instant) -> std::io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "trusted service cleanup deadline elapsed",
            )
        })
}

fn close_request_until(
    connection: &mut ServiceConnection,
    request: &DaemonRequest,
    deadline: Instant,
) -> std::io::Result<Option<String>> {
    request_response_until(connection, request, deadline)
}

#[cfg(unix)]
fn request_response_until(
    connection: &mut ServiceConnection,
    request: &DaemonRequest,
    deadline: Instant,
) -> std::io::Result<Option<String>> {
    write_request_until(&mut connection.writer, request, deadline)?;
    read_response_line_until(&mut connection.reader, deadline)
}

#[cfg(target_os = "windows")]
fn request_response_until(
    connection: &mut ServiceConnection,
    request: &DaemonRequest,
    deadline: Instant,
) -> std::io::Result<Option<String>> {
    let remaining = remaining_until(deadline)?;
    close_request_bounded(
        &mut connection.writer,
        &mut connection.reader,
        request,
        remaining,
    )
}

#[cfg(unix)]
fn write_request_until(
    writer: &mut ServiceStream,
    request: &DaemonRequest,
    deadline: Instant,
) -> std::io::Result<()> {
    writer.set_write_timeout(Some(remaining_until(deadline)?))?;
    let mut frame = serde_json::to_vec(request).map_err(std::io::Error::other)?;
    frame.push(b'\n');
    cua_driver_core::socket_io::write_all_with_retry(writer, &frame, deadline)
}

#[cfg(unix)]
fn read_response_line_until(
    reader: &mut BufReader<ServiceStream>,
    deadline: Instant,
) -> std::io::Result<Option<String>> {
    let mut bytes = Vec::new();
    loop {
        let remaining = remaining_until(deadline)?;
        reader
            .get_ref()
            .set_read_timeout(Some(remaining.min(Duration::from_millis(50))))?;
        let available = reader.fill_buf();
        match available {
            Ok([]) => return Ok(None),
            Ok(buffer) => {
                if let Some(newline) = buffer.iter().position(|byte| *byte == b'\n') {
                    let length = newline + 1;
                    bytes.extend_from_slice(&buffer[..length]);
                    reader.consume(length);
                    return String::from_utf8(bytes).map(Some).map_err(|error| {
                        std::io::Error::new(std::io::ErrorKind::InvalidData, error)
                    });
                }
                let length = buffer.len();
                bytes.extend_from_slice(buffer);
                reader.consume(length);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                remaining_until(deadline)?;
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(unix)]
fn write_request(writer: &mut impl Write, request: &DaemonRequest) -> std::io::Result<()> {
    let mut frame = serde_json::to_vec(request).map_err(std::io::Error::other)?;
    frame.push(b'\n');
    cua_driver_core::socket_io::write_all_with_retry(
        writer,
        &frame,
        Instant::now() + SERVICE_REQUEST_DEADLINE,
    )
}

#[cfg(target_os = "windows")]
fn write_request(writer: &mut impl Write, request: &DaemonRequest) -> std::io::Result<()> {
    serde_json::to_writer(&mut *writer, request).map_err(std::io::Error::other)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

#[cfg(unix)]
fn read_response_line(reader: &mut BufReader<ServiceStream>) -> std::io::Result<Option<String>> {
    let deadline = Instant::now() + SERVICE_REQUEST_DEADLINE;
    let mut bytes = Vec::new();
    loop {
        match reader.read_until(b'\n', &mut bytes) {
            Ok(0) if bytes.is_empty() => return Ok(None),
            Ok(0) | Ok(_) => {
                return String::from_utf8(bytes)
                    .map(Some)
                    .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error));
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                if Instant::now() >= deadline {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "timed out after 120s waiting for trusted service response",
                    ));
                }
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(target_os = "windows")]
fn read_response_line(reader: &mut BufReader<ServiceStream>) -> std::io::Result<Option<String>> {
    let mut line = String::new();
    match reader.read_line(&mut line)? {
        0 => Ok(None),
        _ => Ok(Some(line)),
    }
}

#[cfg(target_os = "windows")]
fn close_request_bounded(
    writer: &mut ServiceStream,
    reader: &mut BufReader<ServiceStream>,
    request: &DaemonRequest,
    deadline: Duration,
) -> std::io::Result<Option<String>> {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{GetCurrentThreadId, OpenThread, THREAD_TERMINATE};
    use windows::Win32::System::IO::CancelSynchronousIo;

    let thread = unsafe { OpenThread(THREAD_TERMINATE, false, GetCurrentThreadId()) }
        .map_err(std::io::Error::other)?;
    let thread_addr = thread.0 as usize;
    let timed_out = Arc::new(AtomicBool::new(false));
    let watchdog_timed_out = timed_out.clone();
    let (disarm, armed) = mpsc::sync_channel::<()>(1);
    let watchdog = std::thread::spawn(move || {
        let thread = windows::Win32::Foundation::HANDLE(thread_addr as *mut _);
        if armed.recv_timeout(deadline).is_err() {
            watchdog_timed_out.store(true, Ordering::Release);
            let _ = unsafe { CancelSynchronousIo(thread) };
        }
        let _ = unsafe { CloseHandle(thread) };
    });

    let result = write_request(writer, request).and_then(|()| read_response_line(reader));
    let _ = disarm.send(());
    let _ = watchdog.join();
    if result.is_err() && timed_out.load(Ordering::Acquire) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "timed out waiting for trusted service close acknowledgement",
        ));
    }
    result
}

#[cfg(unix)]
fn connect_until(socket_path: &str, deadline: Instant) -> Result<ServiceStream, DriverError> {
    use socket2::{Domain, SockAddr, Socket, Type};
    use std::os::fd::{FromRawFd, IntoRawFd};
    use std::path::Path;

    let socket =
        Socket::new(Domain::UNIX, Type::STREAM, None).map_err(|error| DriverError::Transport {
            socket_path: socket_path.to_owned(),
            reason: format!("create bounded trusted service socket: {error}"),
        })?;
    let address =
        SockAddr::unix(Path::new(socket_path)).map_err(|error| DriverError::Transport {
            socket_path: socket_path.to_owned(),
            reason: format!("resolve trusted service socket address: {error}"),
        })?;
    socket
        .connect_timeout(
            &address,
            remaining_until(deadline).map_err(|error| DriverError::Transport {
                socket_path: socket_path.to_owned(),
                reason: error.to_string(),
            })?,
        )
        .map_err(|error| DriverError::Transport {
            socket_path: socket_path.to_owned(),
            reason: format!("bounded connect trusted service session: {error}"),
        })?;
    let stream = unsafe { ServiceStream::from_raw_fd(socket.into_raw_fd()) };
    let remaining = remaining_until(deadline).map_err(|error| DriverError::Transport {
        socket_path: socket_path.to_owned(),
        reason: error.to_string(),
    })?;
    stream
        .set_read_timeout(Some(remaining))
        .and_then(|()| stream.set_write_timeout(Some(remaining)))
        .map_err(|error| DriverError::Transport {
            socket_path: socket_path.to_owned(),
            reason: format!("configure bounded trusted service timeout: {error}"),
        })?;
    Ok(stream)
}

#[cfg(target_os = "windows")]
fn connect_until(socket_path: &str, deadline: Instant) -> Result<ServiceStream, DriverError> {
    loop {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(socket_path)
        {
            Ok(pipe) => return Ok(pipe),
            Err(error) => {
                let remaining =
                    deadline
                        .checked_duration_since(Instant::now())
                        .ok_or_else(|| DriverError::Transport {
                            socket_path: socket_path.to_owned(),
                            reason: format!("bounded connect trusted service named pipe: {error}"),
                        })?;
                std::thread::sleep(remaining.min(Duration::from_millis(50)));
            }
        }
    }
}

#[cfg(unix)]
fn connect(socket_path: &str) -> Result<ServiceStream, DriverError> {
    let stream = std::os::unix::net::UnixStream::connect(socket_path).map_err(|error| {
        DriverError::Transport {
            socket_path: socket_path.to_owned(),
            reason: format!("connect trusted service session: {error}"),
        }
    })?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .and_then(|()| stream.set_write_timeout(Some(Duration::from_secs(5))))
        .map_err(|error| DriverError::Transport {
            socket_path: socket_path.to_owned(),
            reason: format!("configure trusted service session timeout: {error}"),
        })?;
    Ok(stream)
}

#[cfg(target_os = "windows")]
fn connect(socket_path: &str) -> Result<ServiceStream, DriverError> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(socket_path)
        {
            Ok(pipe) => return Ok(pipe),
            Err(_) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(error) => {
                return Err(DriverError::Transport {
                    socket_path: socket_path.to_owned(),
                    reason: format!("connect trusted service named pipe: {error}"),
                });
            }
        }
    }
}

#[cfg(not(any(unix, target_os = "windows")))]
fn connect(socket_path: &str) -> Result<ServiceStream, DriverError> {
    Err(DriverError::Transport {
        socket_path: socket_path.to_owned(),
        reason: "trusted service sessions are unsupported on this platform".into(),
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::{SessionPermissionMode, TrustedSessionOptions};
    use std::os::unix::net::UnixListener;

    fn serve_compatible_metadata(listener: &UnixListener) {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let request: DaemonRequest = serde_json::from_str(&line).unwrap();
        assert_eq!(request.method, "metadata");
        let mut writer = stream;
        writeln!(
            writer,
            "{}",
            serde_json::to_string(&DaemonResponse::ok(
                serde_json::to_value(cua_driver_core::daemon::current_daemon_metadata()).unwrap()
            ))
            .unwrap()
        )
        .unwrap();
    }

    #[test]
    fn response_reader_preserves_partial_utf8_across_poll_timeouts() {
        let (reader, mut writer) = std::os::unix::net::UnixStream::pair().unwrap();
        reader
            .set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        let server = std::thread::spawn(move || {
            writer.write_all(&[0xe2]).unwrap();
            std::thread::sleep(Duration::from_millis(80));
            writer.write_all(&[0x82, 0xac, b'\n']).unwrap();
        });

        let mut reader = BufReader::new(reader);
        assert_eq!(
            read_response_line(&mut reader).unwrap().as_deref(),
            Some("€\n")
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn lost_action_response_is_reported_with_unknown_completion() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("service.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            serve_compatible_metadata(&listener);
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;

            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let begin: DaemonRequest = serde_json::from_str(&line).unwrap();
            assert_eq!(begin.method, "trusted_session_begin");
            writeln!(
                writer,
                "{}",
                serde_json::to_string(&DaemonResponse::ok(serde_json::json!({
                    "resume_credential": "resume-test-1"
                })))
                .unwrap()
            )
            .unwrap();

            line.clear();
            reader.read_line(&mut line).unwrap();
            let call: DaemonRequest = serde_json::from_str(&line).unwrap();
            assert_eq!(call.method, "trusted_session_call");
            // Closing the accepted connection after reading the action leaves
            // its completion unknown to the client.
        });

        let client = ServiceSessionClient::connect_and_bind(
            socket.to_string_lossy().into_owned(),
            TrustedSessionOptions {
                public_session: "lost-response".into(),
                mode: SessionPermissionMode::Standard,
                ttl_seconds: 60,
                idle_ttl_seconds: 30,
                capability_manifest_path: None,
                bounded_manifest_path: None,
            },
            DaemonClientKind::Unknown,
        )
        .unwrap();
        assert!(matches!(
            client
                .invoke("click", serde_json::json!({"x": 1, "y": 1}))
                .await,
            Err(DriverError::ActionInterrupted {
                completion: crate::worker::ActionCompletion::Unknown,
                ..
            })
        ));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn next_call_resumes_with_single_use_host_credential_after_disconnect() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("resumable-service.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            serve_compatible_metadata(&listener);
            {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut writer = stream;
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let begin: DaemonRequest = serde_json::from_str(&line).unwrap();
                assert_eq!(begin.method, "trusted_session_begin");
                writeln!(
                    writer,
                    "{}",
                    serde_json::to_string(&DaemonResponse::ok(serde_json::json!({
                        "resume_credential": "resume-original"
                    })))
                    .unwrap()
                )
                .unwrap();

                line.clear();
                reader.read_line(&mut line).unwrap();
                let call: DaemonRequest = serde_json::from_str(&line).unwrap();
                assert_eq!(call.method, "trusted_session_call");
                // Drop without a response. The SDK must not retry this action.
            }

            // The reconnect can beat the daemon task that records EOF from the
            // previous connection. A transient unavailable response must keep
            // the single-use credential intact and retry on a fresh socket.
            {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut writer = stream;
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let resume: DaemonRequest = serde_json::from_str(&line).unwrap();
                assert_eq!(resume.method, "trusted_session_resume");
                writeln!(
                    writer,
                    "{}",
                    serde_json::to_string(&DaemonResponse::err(
                        "trusted session resume credential is unavailable or already used",
                        77,
                    ))
                    .unwrap()
                )
                .unwrap();
            }

            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let resume: DaemonRequest = serde_json::from_str(&line).unwrap();
            assert_eq!(resume.method, "trusted_session_resume");
            assert_eq!(resume.args.unwrap()["resume_credential"], "resume-original");
            writeln!(
                writer,
                "{}",
                serde_json::to_string(&DaemonResponse::ok(serde_json::json!({
                    "resume_credential": "resume-rotated"
                })))
                .unwrap()
            )
            .unwrap();

            line.clear();
            reader.read_line(&mut line).unwrap();
            let call: DaemonRequest = serde_json::from_str(&line).unwrap();
            assert_eq!(call.method, "trusted_session_call");
            writeln!(
                writer,
                "{}",
                serde_json::to_string(&DaemonResponse::ok(serde_json::json!({
                    "resumed": true
                })))
                .unwrap()
            )
            .unwrap();
        });

        let client = ServiceSessionClient::connect_and_bind(
            socket.to_string_lossy().into_owned(),
            TrustedSessionOptions {
                public_session: "resume-test".into(),
                mode: SessionPermissionMode::Standard,
                ttl_seconds: 60,
                idle_ttl_seconds: 30,
                capability_manifest_path: None,
                bounded_manifest_path: None,
            },
            DaemonClientKind::Unknown,
        )
        .unwrap();
        assert!(matches!(
            client.invoke("click", serde_json::json!({})).await,
            Err(DriverError::ActionInterrupted {
                completion: crate::worker::ActionCompletion::Unknown,
                ..
            })
        ));
        let resumed = client
            .invoke("get_session", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(resumed["resumed"], true);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn transient_read_timeouts_are_poll_intervals_not_action_deadlines() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("delayed-service.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            serve_compatible_metadata(&listener);
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;

            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            writeln!(
                writer,
                "{}",
                serde_json::to_string(&DaemonResponse::ok(serde_json::json!({
                    "resume_credential": "resume-test-2"
                })))
                .unwrap()
            )
            .unwrap();

            line.clear();
            reader.read_line(&mut line).unwrap();
            let call: DaemonRequest = serde_json::from_str(&line).unwrap();
            assert_eq!(call.method, "trusted_session_call");
            std::thread::sleep(Duration::from_millis(80));
            writeln!(
                writer,
                "{}",
                serde_json::to_string(&DaemonResponse::ok(serde_json::json!({
                    "content": [{"type": "text", "text": "completed"}],
                    "isError": false,
                })))
                .unwrap()
            )
            .unwrap();
        });

        let client = ServiceSessionClient::connect_and_bind(
            socket.to_string_lossy().into_owned(),
            TrustedSessionOptions {
                public_session: "delayed-response".into(),
                mode: SessionPermissionMode::Standard,
                ttl_seconds: 60,
                idle_ttl_seconds: 30,
                capability_manifest_path: None,
                bounded_manifest_path: None,
            },
            DaemonClientKind::Unknown,
        )
        .unwrap();
        client
            .connection
            .lock()
            .unwrap()
            .reader
            .get_ref()
            .set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();

        let result = client
            .invoke("health_report", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(result["content"][0]["text"], "completed");
        server.join().unwrap();
    }

    fn connected_test_client(
        socket_path: String,
        stream: ServiceStream,
        closed: bool,
        resume_credential: &str,
    ) -> ServiceSessionClient {
        let writer = stream.try_clone().unwrap();
        ServiceSessionClient {
            socket_path,
            connection: Mutex::new(ServiceConnection {
                reader: BufReader::new(stream),
                writer,
                closed,
                resume_credential: resume_credential.to_owned(),
            }),
            closing: AtomicBool::new(false),
            active_io: Mutex::new(None),
        }
    }

    #[test]
    fn close_interrupts_stalled_resume_then_retries_and_explicitly_ends() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("interrupt-resume-retry-end.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let (stalled_tx, stalled_rx) = std::sync::mpsc::sync_channel(1);
        let server = std::thread::spawn(move || {
            let (stalled_stream, _) = listener.accept().unwrap();
            let mut stalled_reader = BufReader::new(stalled_stream);
            let mut line = String::new();
            stalled_reader.read_line(&mut line).unwrap();
            let stalled_resume: DaemonRequest = serde_json::from_str(&line).unwrap();
            assert_eq!(stalled_resume.method, "trusted_session_resume");
            stalled_tx.send(()).unwrap();
            line.clear();
            assert_eq!(stalled_reader.read_line(&mut line).unwrap(), 0);

            let (cleanup_stream, _) = listener.accept().unwrap();
            let mut cleanup_reader = BufReader::new(cleanup_stream.try_clone().unwrap());
            let mut cleanup_writer = cleanup_stream;
            cleanup_reader.read_line(&mut line).unwrap();
            let cleanup_resume: DaemonRequest = serde_json::from_str(&line).unwrap();
            assert_eq!(cleanup_resume.method, "trusted_session_resume");
            writeln!(
                cleanup_writer,
                "{}",
                serde_json::to_string(&DaemonResponse::ok(serde_json::json!({
                    "resume_credential": "resume-cleanup"
                })))
                .unwrap()
            )
            .unwrap();
            line.clear();
            cleanup_reader.read_line(&mut line).unwrap();
            let end: DaemonRequest = serde_json::from_str(&line).unwrap();
            assert_eq!(end.method, "trusted_session_end");
            writeln!(
                cleanup_writer,
                "{}",
                serde_json::to_string(&DaemonResponse::ok(serde_json::json!({
                    "closed": true
                })))
                .unwrap()
            )
            .unwrap();
        });
        let (stream, _peer) = ServiceStream::pair().unwrap();
        let client = Arc::new(connected_test_client(
            socket.to_string_lossy().into_owned(),
            stream,
            true,
            "resume-original",
        ));
        let caller = client.clone();
        let call = std::thread::spawn(move || {
            caller.request(DaemonRequest {
                method: "trusted_session_call".into(),
                name: Some("health_report".into()),
                args: Some(serde_json::json!({})),
                session_id: None,
                observation_origin: None,
                client_kind: None,
            })
        });
        stalled_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("server must observe the stalled resume");
        client
            .close_until(Instant::now() + Duration::from_millis(500))
            .expect("close must interrupt stalled resume, retry it, and explicitly end");
        assert!(matches!(
            call.join().unwrap(),
            Err(DriverError::Transport { .. })
        ));
        server.join().unwrap();
    }

    #[test]
    fn close_interrupts_wedged_call_then_resumes_and_explicitly_ends() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("interrupt-resume-end.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let initial = ServiceStream::connect(&socket).unwrap();
        let (call_seen_tx, call_seen_rx) = std::sync::mpsc::sync_channel(1);
        let server = std::thread::spawn(move || {
            let (initial_stream, _) = listener.accept().unwrap();
            let mut initial_reader = BufReader::new(initial_stream);
            let mut line = String::new();
            initial_reader.read_line(&mut line).unwrap();
            let call: DaemonRequest = serde_json::from_str(&line).unwrap();
            assert_eq!(call.method, "trusted_session_call");
            call_seen_tx.send(()).unwrap();
            line.clear();
            assert_eq!(initial_reader.read_line(&mut line).unwrap(), 0);

            let (resumed_stream, _) = listener.accept().unwrap();
            let mut resumed_reader = BufReader::new(resumed_stream.try_clone().unwrap());
            let mut resumed_writer = resumed_stream;
            resumed_reader.read_line(&mut line).unwrap();
            let resume: DaemonRequest = serde_json::from_str(&line).unwrap();
            assert_eq!(resume.method, "trusted_session_resume");
            writeln!(
                resumed_writer,
                "{}",
                serde_json::to_string(&DaemonResponse::ok(serde_json::json!({
                    "resume_credential": "resume-rotated"
                })))
                .unwrap()
            )
            .unwrap();

            line.clear();
            resumed_reader.read_line(&mut line).unwrap();
            let end: DaemonRequest = serde_json::from_str(&line).unwrap();
            assert_eq!(end.method, "trusted_session_end");
            writeln!(
                resumed_writer,
                "{}",
                serde_json::to_string(&DaemonResponse::ok(serde_json::json!({
                    "closed": true
                })))
                .unwrap()
            )
            .unwrap();
        });
        let client = Arc::new(connected_test_client(
            socket.to_string_lossy().into_owned(),
            initial,
            false,
            "resume-original",
        ));
        let caller = client.clone();
        let call = std::thread::spawn(move || {
            caller.request(DaemonRequest {
                method: "trusted_session_call".into(),
                name: Some("health_report".into()),
                args: Some(serde_json::json!({})),
                session_id: None,
                observation_origin: None,
                client_kind: None,
            })
        });
        call_seen_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("server must observe the wedged call");
        let started = Instant::now();
        client
            .close_until(Instant::now() + Duration::from_millis(500))
            .expect("close must interrupt, resume, and explicitly end the lease");
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(matches!(
            call.join().unwrap(),
            Err(DriverError::ActionInterrupted { .. })
        ));
        server.join().unwrap();
    }

    #[test]
    fn close_deadline_includes_connection_mutex_wait() {
        let (stream, _peer) = ServiceStream::pair().unwrap();
        let client = Arc::new(connected_test_client(
            "unused".into(),
            stream,
            false,
            "resume",
        ));
        let guard = client.connection.lock().unwrap();
        let worker = client.clone();
        let started = Instant::now();
        let closer = std::thread::spawn(move || {
            worker.close_until(Instant::now() + Duration::from_millis(80))
        });
        let error = closer
            .join()
            .unwrap()
            .expect_err("held mutex must consume close budget");
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(50),
            "lock wait returned too early: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_millis(500),
            "lock wait exceeded budget: {elapsed:?}"
        );
        assert!(matches!(error, DriverError::Shutdown));
        drop(guard);
    }

    #[test]
    fn close_deadline_bounds_stalled_resume_acknowledgement() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("stalled-resume.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let request: DaemonRequest = serde_json::from_str(&line).unwrap();
            assert_eq!(request.method, "trusted_session_resume");
            std::thread::sleep(Duration::from_millis(300));
        });
        let (stream, _peer) = ServiceStream::pair().unwrap();
        let client = connected_test_client(
            socket.to_string_lossy().into_owned(),
            stream,
            true,
            "resume-stalled",
        );
        let started = Instant::now();
        let error = client
            .close_until(Instant::now() + Duration::from_millis(80))
            .expect_err("stalled resume acknowledgement must time out");
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(50),
            "resume returned too early: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_millis(500),
            "resume exceeded budget: {elapsed:?}"
        );
        assert!(matches!(error, DriverError::Transport { .. }));
        server.join().unwrap();
    }

    #[test]
    fn close_deadline_bounds_resume_and_end_under_one_budget() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("resume-then-stalled-end.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let resume: DaemonRequest = serde_json::from_str(&line).unwrap();
            assert_eq!(resume.method, "trusted_session_resume");
            std::thread::sleep(Duration::from_millis(60));
            writeln!(
                writer,
                "{}",
                serde_json::to_string(&DaemonResponse::ok(serde_json::json!({
                    "resume_credential": "rotated"
                })))
                .unwrap()
            )
            .unwrap();
            line.clear();
            reader.read_line(&mut line).unwrap();
            let end: DaemonRequest = serde_json::from_str(&line).unwrap();
            assert_eq!(end.method, "trusted_session_end");
            std::thread::sleep(Duration::from_millis(300));
        });
        let (stream, _peer) = ServiceStream::pair().unwrap();
        let client = connected_test_client(
            socket.to_string_lossy().into_owned(),
            stream,
            true,
            "resume-original",
        );
        let started = Instant::now();
        let error = client
            .close_until(Instant::now() + Duration::from_millis(120))
            .expect_err("resume and close must share one absolute deadline");
        let elapsed = started.elapsed();
        assert!(elapsed >= Duration::from_millis(60));
        assert!(elapsed < Duration::from_millis(500));
        assert!(matches!(error, DriverError::Transport { .. }));
        server.join().unwrap();
    }

    #[test]
    fn close_deadline_bounds_stalled_end_acknowledgement() {
        let (client_stream, server_stream) = ServiceStream::pair().unwrap();
        let server = std::thread::spawn(move || {
            let mut reader = BufReader::new(server_stream);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let request: DaemonRequest = serde_json::from_str(&line).unwrap();
            assert_eq!(request.method, "trusted_session_end");
            std::thread::sleep(Duration::from_millis(300));
        });
        let client = connected_test_client("unused".into(), client_stream, false, "resume");
        let started = Instant::now();
        let error = client
            .close_until(Instant::now() + Duration::from_millis(80))
            .expect_err("stalled close acknowledgement must time out");
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(50),
            "close returned too early: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_millis(500),
            "close exceeded budget: {elapsed:?}"
        );
        assert!(matches!(error, DriverError::Transport { .. }));
        server.join().unwrap();
    }
}
