// SPDX-License-Identifier: FSL-1.1-MIT
// Copyright (c) 2026 Cua AI, Inc.

#![cfg(target_os = "linux")]
use cua_teleport::{FakeHost, Platform, TeleportError, gecko_profile::resolve_profile};
use std::{fs, path::PathBuf};
fn fixture() -> (tempfile::TempDir, FakeHost, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join(".zen");
    fs::create_dir_all(root.join("Profiles/synthetic")).unwrap();
    fs::write(
        root.join("profiles.ini"),
        "[Profile0]\nName=synthetic\nIsRelative=1\nPath=Profiles/synthetic\nDefault=1\n",
    )
    .unwrap();
    let host = FakeHost::new().with_home(temp.path());
    (temp, host, root)
}
fn code(r: Result<(), TeleportError>, expected: &str) {
    match r.unwrap_err() {
        TeleportError::Provider(s) => assert_eq!(s.split(':').next().unwrap(), expected),
        e => panic!("wrong error: {e}"),
    }
}
#[test]
fn posix_lock_child() {
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    let Some(path) = std::env::var_os("CUA_ZEN_TEST_LOCK_PATH") else {
        return;
    };
    let f = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    let mut lock: libc::flock = unsafe { std::mem::zeroed() };
    lock.l_type = libc::F_WRLCK as _;
    lock.l_whence = libc::SEEK_SET as _;
    assert_ne!(
        unsafe { libc::fcntl(f.as_raw_fd(), libc::F_SETLK, &lock) },
        -1
    );
    println!("ZEN_LOCK_READY");
    std::io::stdout().flush().unwrap();
    let _ = std::io::stdin().read(&mut [0]);
}

#[test]
fn actual_posix_lock_refuses_but_clean_retained_parentlock_is_not_deleted() {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    let (_temp, host, _root) = fixture();
    let p = resolve_profile(&host, Platform::Linux, None).unwrap();
    let lock = p.dir.join(".parentlock");
    fs::write(&lock, "").unwrap();
    p.ensure_closed().unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "posix_lock_child", "--nocapture"])
        .env("CUA_ZEN_TEST_LOCK_PATH", &lock)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut ready = false;
    for _ in 0..8 {
        let mut line = String::new();
        if output.read_line(&mut line).unwrap() == 0 {
            break;
        }
        if line.contains("ZEN_LOCK_READY") {
            ready = true;
            break;
        }
    }
    // Always release/join the owned lock holder before assertions can unwind.
    let result = p.ensure_closed();
    drop(child.stdin.take());
    let exited = child.wait().unwrap();
    assert!(ready && exited.success());
    code(result, "zen_profile_locked");
    p.ensure_closed().unwrap();
    assert_eq!(fs::read(lock).unwrap(), b"");
}
