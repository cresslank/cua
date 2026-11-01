// SPDX-License-Identifier: FSL-1.1-MIT
// Copyright (c) 2026 Cua AI, Inc.
#![cfg(target_os = "linux")]

use cua_spacesd_teleport::{
    bundle::BundleWriter, importers::zen::ZenImporter, HostCommand, HostEffects, HostOutput,
    ImportProvider, ImportRecord, Ledger, LedgerStore, Platform, Receiver, TransferScope,
};
use std::fs;
use std::io::{self, Cursor};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

fn tabs() -> Vec<u8> {
    let mut w = BundleWriter::new(Vec::new(), "zen", "Zen", TransferScope::TabsOnly);
    w.add_bytes("zen/tabs.json", 0o600, b"[]").unwrap();
    w.finish().unwrap()
}
fn profile(home: &Path) -> PathBuf {
    home.join(".mozilla/firefox/Profiles/cua.zen-import")
}

// A distinct process is essential: POSIX locks are process-, not FD-owned.
fn browser_can_lock(path: &Path) -> bool {
    let output = Command::new("python3")
        .args([
            "-c",
            r#"
import errno, fcntl, sys
with open(sys.argv[1], 'r+') as f:
    try:
        fcntl.lockf(f, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except OSError as e:
        if e.errno not in (errno.EACCES, errno.EAGAIN): raise
        print('busy')
    else:
        print('available')
"#,
        ])
        .arg(path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    match output.stdout.as_slice() {
        b"available\n" => true,
        b"busy\n" => false,
        other => panic!("unexpected child response: {other:?}"),
    }
}

// The real child probes at our actual spawn seam; no browser/desktop is started.
#[derive(Default)]
struct LaunchProbe(Mutex<Vec<bool>>);
impl HostEffects for LaunchProbe {
    fn run(&self, _: &HostCommand) -> io::Result<HostOutput> {
        Err(io::Error::other("unexpected host effect"))
    }
    fn home_dir(&self) -> Option<PathBuf> {
        None
    }
    fn spawn(&self, c: &HostCommand) -> io::Result<u32> {
        assert_eq!(c.program, "firefox");
        assert_eq!(c.args[0], "--profile");
        self.0
            .lock()
            .unwrap()
            .push(browser_can_lock(&Path::new(&c.args[1]).join(".parentlock")));
        Err(io::Error::other("owned synthetic launch refusal"))
    }
}

#[test]
fn fresh_and_replaced_live_generations_remain_locked_through_durable_ledger_until_spawn() {
    let home = tempfile::tempdir().unwrap();
    let host = Arc::new(LaunchProbe::default());
    let r = Receiver::with_host(home.path().into(), host.clone());
    let store = LedgerStore::new(home.path().join("ledger"));
    let lock = profile(home.path()).join(".parentlock");
    let mut old_inode = None;
    for id in ["fresh", "replacement"] {
        let result = r
            .import_bundle_with_commit(Cursor::new(tabs()), "zen", true, |provider, record| {
                assert!(
                    !browser_can_lock(&lock),
                    "live generation must exclude an external browser during ledger"
                );
                if let Some(inode) = old_inode {
                    assert_ne!(fs::metadata(&lock).unwrap().ino(), inode);
                    let parent = profile(home.path()).parent().unwrap().to_owned();
                    let staging = fs::read_dir(parent)
                        .unwrap()
                        .map(|e| e.unwrap().path())
                        .find(|p| {
                            p.file_name()
                                .unwrap()
                                .to_string_lossy()
                                .starts_with(".cua-zen-stage-")
                        })
                        .unwrap();
                    let old = staging.join("profile/.parentlock");
                    assert_eq!(fs::metadata(&old).unwrap().ino(), inode);
                    assert!(
                        !browser_can_lock(&old),
                        "predecessor is separately locked until rollback decision"
                    );
                }
                store
                    .write(&Ledger::new(id, provider, record, 0, 1))
                    .map_err(|e| e.to_string())?;
                assert!(store.read(id).unwrap().is_some());
                assert!(
                    !browser_can_lock(&lock),
                    "durable callback must not release live lock"
                );
                Ok(())
            })
            .unwrap();
        assert!(!result.launched);
        assert!(result
            .launch_error
            .unwrap()
            .contains("owned synthetic launch refusal"));
        assert!(browser_can_lock(&lock));
        old_inode = Some(fs::metadata(&lock).unwrap().ino());
    }
    assert_eq!(
        *host.0.lock().unwrap(),
        [true, true],
        "only deliberate spawn releases browser lock"
    );
}

#[test]
fn drop_explicitly_releases_admission_even_if_a_child_inherited_its_flock_fd() {
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    struct Child {
        pid: libc::pid_t,
        release: UnixStream,
    }
    impl Drop for Child {
        fn drop(&mut self) {
            use std::io::Write;
            let _ = self.release.write_all(b"x");
            let mut status = 0;
            assert_eq!(unsafe { libc::waitpid(self.pid, &mut status, 0) }, self.pid);
            assert_eq!(status, 0);
        }
    }
    let home = tempfile::tempdir().unwrap();
    let r = Receiver::with_host(home.path().into(), Arc::new(LaunchProbe::default()));
    let mut child = None;
    let outcome = r
        .import_bundle_with_commit(Cursor::new(tabs()), "zen", false, |_, _| {
            let (parent, peer) = UnixStream::pair().unwrap();
            let pid = unsafe { libc::fork() };
            assert!(pid >= 0);
            if pid == 0 {
                // A fork in a multithreaded process may use only async-signal-safe
                // syscalls before exec/_exit. The child never touches Rust state.
                let mut byte = 0u8;
                let n = unsafe { libc::read(peer.as_raw_fd(), (&mut byte as *mut u8).cast(), 1) };
                unsafe { libc::_exit(if n == 1 { 0 } else { 1 }) };
            }
            drop(peer);
            child = Some(Child {
                pid,
                release: parent,
            });
            Ok(())
        })
        .unwrap();
    // The live child still owns the inherited flock open-file description.
    // Merely closing the parent's descriptor cannot release that description.
    let report = r.wipe(&Ledger::new("fork", "zen", &outcome.record, 0, 1));
    drop(child); // Always join our exact child before checking the result.
    assert!(report.complete() && report.refused.is_empty(), "{report:?}");
    assert!(!profile(home.path()).exists());
}

#[test]
fn wipe_unlinks_browser_lock_only_after_profile_data() {
    let home = tempfile::tempdir().unwrap();
    let r = Receiver::with_host(home.path().into(), Arc::new(LaunchProbe::default()));
    let outcome = r.import_bytes(&tabs(), "zen", false).unwrap();
    let lock = profile(home.path()).join(".parentlock");
    let data = profile(home.path()).join("user.js");
    let mut ledger = Ledger::new("wipe", "zen", &outcome.record, 0, 1);
    // Even a merged/older record that lists the lock first must defer it.
    ledger.files.sort_by_key(|path| path != &lock);
    assert_eq!(ledger.files[0], lock);
    let report = r.wipe(&ledger);
    assert!(report.complete() && report.refused.is_empty(), "{report:?}");
    let data_removed = report
        .removed_paths
        .iter()
        .position(|path| path == &data)
        .unwrap();
    let lock_removed = report
        .removed_paths
        .iter()
        .position(|path| path == &lock)
        .unwrap();
    assert!(data_removed < lock_removed);
    assert!(!profile(home.path()).exists());
}

#[test]
fn admitted_commit_callback_finishes_despite_cancellation_and_never_launches() {
    use cua_spacesd_teleport::ImportCancellation;
    use std::sync::mpsc;
    use std::time::Duration;
    let home = tempfile::tempdir().unwrap();
    let host = Arc::new(LaunchProbe::default());
    let receiver = Receiver::with_host(home.path().into(), host.clone());
    let control = ImportCancellation::default();
    let worker_control = control.clone();
    let store = LedgerStore::new(home.path().join("ledger"));
    let worker_store = store.clone();
    let lock = profile(home.path()).join(".parentlock");
    let (arrived_tx, arrived) = mpsc::channel();
    let (release, resume) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        receiver.import_bundle_cancellable(
            Cursor::new(tabs()),
            "zen",
            true,
            &worker_control,
            |provider, record| {
                arrived_tx.send(()).unwrap();
                resume.recv_timeout(Duration::from_secs(15)).unwrap();
                // Entry to this callback already won the atomic decision; stopping
                // while it runs cannot honestly promise that this write is undone.
                worker_store
                    .write(&Ledger::new("admitted", provider, record, 0, 1))
                    .map_err(|e| e.to_string())
            },
        )
    });
    let reached = arrived.recv_timeout(Duration::from_secs(15));
    control.cancel();
    let protected = lock.exists() && !browser_can_lock(&lock);
    let before = store.read("admitted").unwrap();
    // Release and join before assertions so a test failure cannot strand work.
    let _ = release.send(());
    let result = worker.join().unwrap().unwrap();
    reached.unwrap();
    assert!(protected);
    assert!(before.is_none());
    assert!(store.read("admitted").unwrap().is_some());
    assert!(!result.launched);
    assert_eq!(
        result.launch_error.as_deref(),
        Some("RPC cancelled after commit; import committed, launch suppressed")
    );
    assert!(host.0.lock().unwrap().is_empty());
    assert!(browser_can_lock(&lock));
}

#[test]
fn rollback_restores_the_locked_predecessor_then_drop_releases_it() {
    let home = tempfile::tempdir().unwrap();
    let r = Receiver::with_host(home.path().into(), Arc::new(LaunchProbe::default()));
    r.import_bytes(&tabs(), "zen", false).unwrap();
    let lock = profile(home.path()).join(".parentlock");
    let old = fs::metadata(&lock).unwrap().ino();
    let (_, mut lease) = ZenImporter::new()
        .import_with_lease(
            &mut Cursor::new(tabs()),
            home.path(),
            Platform::Linux,
            &mut ImportRecord::default(),
        )
        .unwrap();
    assert_ne!(fs::metadata(&lock).unwrap().ino(), old);
    assert!(!browser_can_lock(&lock));
    lease.as_mut().unwrap().rollback().unwrap();
    assert_eq!(fs::metadata(&lock).unwrap().ino(), old);
    assert!(
        !browser_can_lock(&lock),
        "rollback must restore predecessor while its exclusion is retained"
    );
    drop(lease);
    assert!(browser_can_lock(&lock));
    assert!(!fs::read_dir(profile(home.path()).parent().unwrap())
        .unwrap()
        .any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".cua-zen-stage-")));
}
