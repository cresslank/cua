//! Owned real receiver lifecycle for the single public-MCP launch token test.
//! Numeric PIDs from launch_app are never admitted to the cleanup authority.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use cua_driver_testkit::windows_process::{process_identity, ObservedProcess};
use serde::Deserialize;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, TerminateProcess, WaitForSingleObject,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
};
use windows::Win32::UI::WindowsAndMessaging::{GetWindowThreadProcessId, IsWindow};

#[derive(Deserialize)]
struct Receipt {
    nonce: String,
    pid: u32,
    created: u64,
    hwnd: usize,
}

pub struct ReadyReceiver {
    pub pid: u32,
    pub hwnd: usize,
    pub process: ObservedProcess,
}

struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Constructed only after nonce, image, birth and native window-owner checks.
/// Its handle pins the actual process object even if Windows later reuses a PID.
struct QualifiedReceiver {
    process: Handle,
    stream: TcpStream,
    nonce: String,
    released: bool,
}

impl QualifiedReceiver {
    fn release(&mut self) -> io::Result<()> {
        if self.released {
            return Ok(());
        }
        self.released = true;
        // EOF also releases the fixture. Always close the connection even if
        // the explicit release write fails (including assertion unwind).
        let _ = writeln!(self.stream, "release {}", self.nonce);
        let _ = self.stream.shutdown(Shutdown::Both);
        let wait = unsafe { WaitForSingleObject(self.process.0, 5000) };
        if wait == WAIT_TIMEOUT {
            // ONLY this authenticated, incarnation-bound retained handle can
            // reach TerminateProcess. Never reopen the tool-returned PID here.
            unsafe { TerminateProcess(self.process.0, 1) }.map_err(io::Error::other)?;
            if unsafe { WaitForSingleObject(self.process.0, 5000) } != WAIT_OBJECT_0 {
                return Err(io::Error::other(
                    "owned receiver did not exit after termination",
                ));
            }
            return Err(io::Error::other("owned receiver required forced cleanup"));
        }
        if wait != WAIT_OBJECT_0 {
            return Err(io::Error::other("owned receiver exit wait failed"));
        }
        let mut code = 0;
        unsafe { GetExitCodeProcess(self.process.0, &mut code) }.map_err(io::Error::other)?;
        if code != 0 {
            return Err(io::Error::other(format!(
                "owned receiver exited with {code}"
            )));
        }
        eprintln!("[launch_app_standard_user] authenticated receiver released and exit verified");
        Ok(())
    }
}

impl Drop for QualifiedReceiver {
    fn drop(&mut self) {
        if let Err(error) = self.release() {
            eprintln!("[launch_app_standard_user] receiver cleanup: {error}");
        }
    }
}

/// Listener + cleanup worker are armed BEFORE calling launch_app. They keep
/// custody during the synchronous MCP call and on any assertion panic.
pub struct ReceiverLease {
    pub address: String,
    pub nonce: String,
    ready: Receiver<io::Result<ReadyReceiver>>,
    release: Option<Sender<()>>,
    worker: Option<JoinHandle<io::Result<()>>>,
}

impl ReceiverLease {
    pub fn new(image: &Path) -> Self {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .expect("private readiness listener");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let address = listener.local_addr().unwrap().to_string();
        let image = image.canonicalize().expect("built launch receiver image");
        let nonce = uuid::Uuid::new_v4().to_string();
        let (ready_tx, ready) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let worker_nonce = nonce.clone();
        let worker = thread::spawn(move || {
            let result = receive(listener, &worker_nonce, image, &release_rx);
            match result {
                Ok(Some((mut owned, observation))) => {
                    // A dropped test controller still releases via recv's
                    // disconnection. Sending observations cannot lose custody.
                    let sent = ready_tx.send(Ok(observation));
                    let released = release_rx.recv_timeout(Duration::from_secs(80));
                    let cleanup = owned.release();
                    cleanup?;
                    sent.map_err(|_| io::Error::other("receiver observer disconnected"))?;
                    if matches!(released, Err(mpsc::RecvTimeoutError::Timeout)) {
                        return Err(io::Error::other("test did not release owned receiver"));
                    }
                    Ok(())
                }
                Ok(None) => Ok(()), // Refused launch / panic before readiness.
                Err(error) => {
                    let message = error.to_string();
                    let _ = ready_tx.send(Err(error));
                    Err(io::Error::other(message))
                }
            }
        });
        Self {
            address,
            nonce,
            ready,
            release: Some(release),
            worker: Some(worker),
        }
    }

    pub fn ready(&self) -> ReadyReceiver {
        self.ready
            .recv_timeout(Duration::from_secs(5))
            .expect("launch receiver did not announce readiness")
            .expect("launch receiver identity qualification failed")
    }

    fn finish(&mut self) -> io::Result<()> {
        self.release.take(); // disconnect rather than relying on send success
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| io::Error::other("receiver custodian panicked"))??;
        }
        Ok(())
    }
}

impl Drop for ReceiverLease {
    fn drop(&mut self) {
        if let Err(error) = self.finish() {
            eprintln!("[launch_app_standard_user] receiver custody failed: {error}");
            if !thread::panicking() {
                panic!("receiver cleanup failed: {error}");
            }
        }
    }
}

fn receive(
    listener: TcpListener,
    nonce: &str,
    image: PathBuf,
    release: &Receiver<()>,
) -> io::Result<Option<(QualifiedReceiver, ReadyReceiver)>> {
    // Includes the existing 25-second MCP call budget; no unbounded accepts or
    // reads. Dropping the listener/stream makes a not-yet-qualified fixture exit.
    let deadline = Instant::now() + Duration::from_secs(35);
    let mut stream = loop {
        match listener.accept() {
            Ok((stream, peer)) => {
                if !peer.ip().is_loopback() {
                    return Err(io::Error::other("non-loopback receiver"));
                }
                break stream;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error),
        }
        if release.try_recv() != Err(mpsc::TryRecvError::Empty) {
            return Ok(None);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other("receiver accept expired"));
        }
        thread::sleep(Duration::from_millis(10));
    };
    stream.set_nonblocking(true)?;
    let mut bytes = Vec::new();
    loop {
        let mut buffer = [0; 1024];
        match stream.read(&mut buffer) {
            Ok(0) => return Err(io::Error::other("receiver closed before readiness")),
            Ok(count) => bytes.extend_from_slice(&buffer[..count]),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error),
        }
        if bytes.len() > 4096 {
            return Err(io::Error::other("oversized receiver receipt"));
        }
        if bytes.contains(&b'\n') {
            break;
        }
        if release.try_recv() != Err(mpsc::TryRecvError::Empty) {
            return Ok(None);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other("receiver readiness expired"));
        }
        thread::sleep(Duration::from_millis(10));
    }
    let receipt: Receipt = serde_json::from_slice(&bytes)?;
    if receipt.nonce != nonce || receipt.pid == 0 || receipt.hwnd == 0 {
        return Err(io::Error::other("receiver nonce/PID/window mismatch"));
    }
    let process = Handle(
        unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE | PROCESS_TERMINATE,
                false,
                receipt.pid,
            )
        }
        .map_err(io::Error::other)?,
    );
    let identity = process_identity(process.0)?;
    let hwnd = HWND(receipt.hwnd as *mut _);
    let mut owner = 0;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut owner)) };
    if identity.image != image
        || identity.created != receipt.created
        || owner != receipt.pid
        || !unsafe { IsWindow(Some(hwnd)) }.as_bool()
        || unsafe { WaitForSingleObject(process.0, 0) } != WAIT_TIMEOUT
    {
        return Err(io::Error::other(format!(
            "receiver native identity mismatch: {identity:?}"
        )));
    }
    // A separate query-only retained handle goes to the test's independent
    // token oracle. Compare incarnations before admitting it to observation.
    let observation = ObservedProcess::open(receipt.pid)?;
    if observation.identity()? != identity || !observation.is_alive() {
        return Err(io::Error::other("receiver changed during qualification"));
    }
    stream.set_nonblocking(false)?;
    stream.set_write_timeout(Some(Duration::from_secs(1)))?;
    eprintln!(
        "[launch_app_standard_user] nonce-bound receiver PID={}, HWND={:#x}, identity={identity:?}",
        receipt.pid, receipt.hwnd
    );
    Ok(Some((
        QualifiedReceiver {
            process,
            stream,
            nonce: nonce.to_owned(),
            released: false,
        },
        ReadyReceiver {
            pid: receipt.pid,
            hwnd: receipt.hwnd,
            process: observation,
        },
    )))
}
