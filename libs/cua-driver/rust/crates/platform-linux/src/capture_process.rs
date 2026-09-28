//! Single-owner subprocess supervision. Pipe reads never own or reap the child.

use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::process::{Child, Output};
use std::time::{Duration, Instant};

fn nonblocking(pipe: &impl AsRawFd) -> io::Result<()> {
    // SAFETY: the borrowed pipe keeps its descriptor alive through both calls.
    let flags = unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_GETFL) };
    if flags < 0
        || unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Read at most 64 KiB per turn, so a continuously writing pipe cannot starve
/// the other pipe, child status checks, or the absolute deadline. True means EOF.
fn drain(pipe: &mut impl Read, output: &mut Vec<u8>) -> io::Result<bool> {
    let mut buffer = [0; 8192];
    for _ in 0..8 {
        match pipe.read(&mut buffer) {
            Ok(0) => return Ok(true),
            Ok(n) => output.extend_from_slice(&buffer[..n]),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(false)
}

/// Own all wait/kill operations through the same exclusive Child borrow. In
/// particular, after try_wait reaps an exited child, Child caches that status;
/// kill cannot signal its former numeric PID, even if a descendant holds a pipe
/// open until the deadline. No detached reader/waiter threads survive this call.
pub(super) fn wait_with_output(child: &mut Child, timeout: Duration) -> io::Result<Output> {
    let deadline = Instant::now() + timeout;
    let result = (|| {
        let mut stdout = child.stdout.take().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "capture stdout must be piped")
        })?;
        let mut stderr = child.stderr.take().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "capture stderr must be piped")
        })?;
        nonblocking(&stdout)?;
        nonblocking(&stderr)?;
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let (mut out_eof, mut err_eof) = (false, false);
        loop {
            let status = child.try_wait()?;
            if !out_eof {
                out_eof = drain(&mut stdout, &mut out)?;
            }
            if !err_eof {
                err_eof = drain(&mut stderr, &mut err)?;
            }
            if let Some(status) = status {
                if out_eof && err_eof {
                    return Ok(Output {
                        status,
                        stdout: out,
                        stderr: err,
                    });
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "capture process output deadline expired",
                ));
            }
            std::thread::sleep(remaining.min(Duration::from_millis(5)));
        }
    })();
    if result.is_err() {
        // No other owner can reap between kill and wait. If try_wait already
        // reaped it, both calls use Child's cached status and never signal a PID.
        child.kill()?;
        child.wait()?;
    }
    result
}
