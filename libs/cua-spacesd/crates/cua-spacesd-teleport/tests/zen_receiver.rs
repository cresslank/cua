// SPDX-License-Identifier: FSL-1.1-MIT
// Copyright (c) 2026 Cua AI, Inc.
#![cfg(target_os = "linux")]
use cua_spacesd_teleport::{bundle::BundleWriter, FakeHost, Receiver, TransferScope};
use std::io::BufRead;
use std::process::{Command, Stdio};
use std::sync::Arc;

fn tabs() -> Vec<u8> {
    let mut w = BundleWriter::new(Vec::new(), "zen", "Zen", TransferScope::TabsOnly);
    w.add_bytes("zen/tabs.json", 0o600, b"[]").unwrap();
    w.finish().unwrap()
}

#[test]
fn browser_posix_lock_from_another_process_refuses_without_killing_it() {
    let home = tempfile::tempdir().unwrap();
    let r = Receiver::with_host(home.path().into(), Arc::new(FakeHost::new()));
    let first = r.import_bytes(&tabs(), "zen", false).unwrap();
    let profile = home.path().join(".mozilla/firefox/Profiles/cua.zen-import");
    let lock = profile.join(".parentlock");
    let mut child = Command::new("python3").args(["-c", "import fcntl,sys; f=open(sys.argv[1],'a'); fcntl.lockf(f,fcntl.LOCK_EX|fcntl.LOCK_NB); print('locked',flush=True); sys.stdin.read()", lock.to_str().unwrap()]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    let mut line = String::new();
    std::io::BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let result = r.import_bytes(&tabs(), "zen", false);
    let ledger = cua_spacesd_teleport::Ledger::new("owned", "zen", &first.record, 0, 1);
    let wipe = r.wipe(&ledger);
    drop(child.stdin.take());
    let status = child.wait().unwrap();
    assert!(status.success());
    assert_eq!(line.trim(), "locked");
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("Firefox is running"));
    assert!(!wipe.complete());
    assert!(wipe.errors.iter().any(|e| e.contains("Firefox is running")));
    assert!(profile.join("user.js").exists());
    r.import_bytes(&tabs(), "zen", false).unwrap();
}
