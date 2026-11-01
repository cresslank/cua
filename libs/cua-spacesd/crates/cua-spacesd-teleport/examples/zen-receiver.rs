// SPDX-License-Identifier: FSL-1.1-MIT
// Copyright (c) 2026 Cua AI, Inc.
//! Explicit disposable-harness entry point, not a Keyvault or consent bypass.
//! Does not discover HOME, source browsers, vaults or installed daemons. Reads a
//! caller-supplied normalized tar from stdin; imports into the explicit home.
use cua_spacesd_teleport::{bundle::BundleReader, FakeHost, Ledger, LedgerStore, Receiver};
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 || !matches!(args[1].as_str(), "import" | "wipe") {
        return Err(
            "usage: zen-receiver import|wipe EXPLICIT_DISPOSABLE_HOME (bundle on stdin)".into(),
        );
    }
    let home = PathBuf::from(&args[2]).canonicalize()?;
    let r = Receiver::with_host(home.clone(), Arc::new(FakeHost::new()));
    let store = LedgerStore::new(home.join(".cua-harness-ledger"));
    if args[1] == "wipe" {
        let l = store.read("synthetic-zen")?.ok_or("no harness ledger")?;
        let report = r.wipe(&l);
        if !report.complete() || !report.refused.is_empty() {
            return Err(format!("wipe failed: {report:?}").into());
        }
        store.remove("synthetic-zen")?;
        println!("{{\"wiped\":true}}");
        return Ok(());
    }
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(40 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 40 * 1024 * 1024 {
        return Err("bundle too large".into());
    }
    let outcome = r.import_bundle_with_commit(
        std::io::Cursor::new(&bytes),
        "zen",
        false,
        |provider, record| {
            let mut record = record.clone();
            if let Some(previous) = store.read("synthetic-zen").map_err(|e| e.to_string())? {
                record.merge(&previous.record());
            }
            store
                .write(&Ledger::new("synthetic-zen", provider, &record, 0, 1))
                .map_err(|e| e.to_string())
        },
    )?;
    let mut reader = BundleReader::open(bytes.as_slice())?;
    let mut tabs: Vec<String> = Vec::new();
    while let Some(e) = reader.next_entry()? {
        if e.rel_path == "zen/tabs.json" {
            tabs = serde_json::from_slice(&e.bytes)?;
        }
    }
    println!(
        "{}",
        serde_json::json!({"provider":"zen", "program":"firefox", "profile":home.join(".mozilla/firefox/Profiles/cua.zen-import"), "tabs":tabs, "notices":outcome.notices, "launched":false, "boundary":"normalized receiver only; not Keyvault/polkit push"})
    );
    Ok(())
}
