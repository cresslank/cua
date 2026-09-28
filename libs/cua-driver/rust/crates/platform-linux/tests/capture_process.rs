//! Headless pipe/process fixtures; never invoke a capture backend or desktop.
#![cfg(unix)]

#[path = "../src/capture_process.rs"]
mod capture_process;

use std::os::unix::process::ExitStatusExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn fixture(script: &str) -> Child {
    Command::new("/bin/sh")
        .args(["-c", script])
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

#[test]
fn drains_both_pipes_beyond_pipe_capacity_and_preserves_exit_status() {
    let mut child = fixture("i=0; while [ $i -lt 20000 ]; do printf abcdefgh; printf ijklmnop >&2; i=$((i+1)); done; exit 7");
    let output = capture_process::wait_with_output(&mut child, Duration::from_secs(10)).unwrap();
    assert_eq!(output.status.code(), Some(7));
    assert_eq!(output.stdout, b"abcdefgh".repeat(20000));
    assert_eq!(output.stderr, b"ijklmnop".repeat(20000));
    assert_eq!(child.try_wait().unwrap(), Some(output.status));
}

#[test]
fn timeout_terminates_and_reaps_the_owned_child() {
    let mut child = fixture("exec /bin/sleep 30");
    let started = Instant::now();
    let error =
        capture_process::wait_with_output(&mut child, Duration::from_millis(50)).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    assert_eq!(
        child.try_wait().unwrap().unwrap().signal(),
        Some(libc::SIGKILL)
    );
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn reaped_child_with_open_output_pipe_times_out_without_signalling_again() {
    // Retain a duplicate write end without a descendant process: supply it as
    // stdout to the child, keeping our own writer alive after that child exits.
    let (reader, writer) = std::os::unix::net::UnixStream::pair().unwrap();
    let mut child = Command::new("/bin/sh")
        .args(["-c", "exit 0"])
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::from(std::os::fd::OwnedFd::from(
            writer.try_clone().unwrap(),
        )))
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // ChildStdout accepts an owned read descriptor; no raw-PID signal or race
    // injector is involved. This deterministically models delayed pipe EOF.
    child.stdout = Some(std::process::ChildStdout::from(std::os::fd::OwnedFd::from(
        reader,
    )));
    let status = child.wait().unwrap();
    let error =
        capture_process::wait_with_output(&mut child, Duration::from_millis(50)).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    assert_eq!(status.code(), Some(0));
    assert_eq!(child.try_wait().unwrap(), Some(status));
    drop(writer);
}

#[test]
fn exited_child_output_is_collected_without_a_timeout_kill() {
    let mut child = fixture("printf output; printf diagnostic >&2");
    let status = child.wait().unwrap();
    let output = capture_process::wait_with_output(&mut child, Duration::from_secs(1)).unwrap();
    assert_eq!(output.status, status);
    assert_eq!(output.stdout, b"output");
    assert_eq!(output.stderr, b"diagnostic");
}

#[test]
fn capture_keeps_wait_and_termination_in_one_supervisor() {
    let capture = include_str!("../src/capture.rs");
    let import = capture
        .split_once("fn capture_via_import(")
        .unwrap()
        .1
        .split_once("// ── shared pixel conversion")
        .unwrap()
        .0;
    assert!(import.contains("process::wait_with_output(&mut child, IMPORT_TIMEOUT)"));
    assert!(!import.contains("child.id()"));
    assert!(!import.contains("libc::kill"));
    assert!(!import.contains("thread::spawn"));
    let supervisor = include_str!("../src/capture_process.rs");
    assert!(!supervisor.contains("libc::kill"));
    assert!(!supervisor.contains("thread::spawn"));
}
