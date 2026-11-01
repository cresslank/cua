// SPDX-License-Identifier: FSL-1.1-MIT
// Copyright (c) 2026 Cua AI, Inc.
#![cfg(target_os = "linux")]

use cua_proto::env::v1::teleport_service_server::TeleportService;
use cua_proto::env::v1::{ImportOptions, ImportSessionRequest};
use cua_spacesd_server::{
    config::ServerConfig, context::ServerContext, services::teleport::TeleportServiceImpl,
};
use cua_spacesd_teleport::{
    bundle::BundleWriter, importers::zen::ZenImporter, FakeHost, GuardedImport, ImportLease,
    ImportProvider, ImportRecord, ImportRegistry, LaunchSpec, Platform, Receiver, TransferScope,
};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Cursor, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;
use tokio::sync::oneshot;
use tonic::Request;

const DEADLINE: Duration = Duration::from_secs(15);
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pause {
    BeforeDecision,
    AfterDurable,
}

// An ordinary public importer/lease adapter controls only owned test work. The
// real Zen parser, staging, locks, ledger callback and rollback all run unchanged.
struct Gate {
    pause: Pause,
    arrived: Mutex<Option<oneshot::Sender<()>>>,
    resume: Mutex<mpsc::Receiver<()>>,
    finished: Mutex<Option<oneshot::Sender<()>>>,
}
impl Gate {
    fn wait(&self) {
        self.arrived
            .lock()
            .unwrap()
            .take()
            .unwrap()
            .send(())
            .unwrap();
        self.resume
            .lock()
            .unwrap()
            .recv_timeout(DEADLINE)
            .expect("owned barrier release");
    }
}
struct Release(Option<mpsc::Sender<()>>);
impl Drop for Release {
    fn drop(&mut self) {
        if let Some(tx) = self.0.take() {
            let _ = tx.send(());
        }
    }
}
struct GatedImporter(Arc<Gate>);
impl ImportProvider for GatedImporter {
    fn id(&self) -> &str {
        "zen"
    }
    fn display_name(&self) -> &str {
        "Zen"
    }
    fn import_recorded(
        &self,
        bundle: &mut dyn Read,
        home: &Path,
        platform: Platform,
        record: &mut ImportRecord,
    ) -> cua_spacesd_teleport::Result<LaunchSpec> {
        ZenImporter::new().import_recorded(bundle, home, platform, record)
    }
    fn import_with_lease(
        &self,
        bundle: &mut dyn Read,
        home: &Path,
        platform: Platform,
        record: &mut ImportRecord,
    ) -> cua_spacesd_teleport::Result<GuardedImport> {
        let (spec, lease) = ZenImporter::new().import_with_lease(bundle, home, platform, record)?;
        let lease = GatedLease {
            inner: lease,
            gate: self.0.clone(),
        };
        if self.0.pause == Pause::BeforeDecision {
            self.0.wait();
        }
        Ok((spec, Some(Box::new(lease))))
    }
}
struct GatedLease {
    inner: Option<Box<dyn ImportLease>>,
    gate: Arc<Gate>,
}
impl ImportLease for GatedLease {
    fn commit(&mut self) {
        self.inner.as_mut().unwrap().commit();
        if self.gate.pause == Pause::AfterDurable {
            self.gate.wait();
        }
    }
    fn rollback(&mut self) -> cua_spacesd_teleport::Result<()> {
        self.inner.as_mut().unwrap().rollback()
    }
    fn prepare_launch(&mut self) {
        self.inner.as_mut().unwrap().prepare_launch();
    }
}
impl Drop for GatedLease {
    fn drop(&mut self) {
        self.inner.take(); // Finish native rollback/FD release before notification.
        if let Some(tx) = self.gate.finished.lock().unwrap().take() {
            let _ = tx.send(());
        }
    }
}
fn tabs() -> Vec<u8> {
    let mut w = BundleWriter::new(Vec::new(), "zen", "Zen", TransferScope::TabsOnly);
    w.add_bytes("zen/tabs.json", 0o600, b"[]").unwrap();
    w.finish().unwrap()
}
fn browser_can_lock(path: &Path) -> bool {
    let output = Command::new("python3")
        .args([
            "-c",
            r#"
import errno, fcntl, sys
with open(sys.argv[1], 'r+') as f:
    try: fcntl.lockf(f, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except OSError as e:
        if e.errno not in (errno.EACCES, errno.EAGAIN): raise
        print('busy')
    else: print('available')
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
    assert!(matches!(
        output.stdout.as_slice(),
        b"available\n" | b"busy\n"
    ));
    output.stdout == b"available\n"
}

async fn cancel_at(pause: Pause, existing: bool, sealed: bool) {
    let home = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let profile = home.path().join(".mozilla/firefox/Profiles/cua.zen-import");
    let lock = profile.join(".parentlock");
    let host = Arc::new(FakeHost::new());
    let mut receiver = Receiver::with_host(home.path().into(), host.clone());
    let old_inode = if existing {
        receiver
            .import_bundle(Cursor::new(tabs()), "zen", false)
            .unwrap();
        fs::write(profile.join("user.js"), b"predecessor bytes").unwrap();
        fs::write(profile.join("neighbour"), b"do not remove").unwrap();
        Some(fs::metadata(&lock).unwrap().ino())
    } else {
        None
    };
    let (arrived_tx, arrived) = oneshot::channel();
    let (resume_tx, resume) = mpsc::channel();
    let release = Release(Some(resume_tx));
    let (finished_tx, finished) = oneshot::channel();
    let gate = Arc::new(Gate {
        pause,
        arrived: Mutex::new(Some(arrived_tx)),
        resume: Mutex::new(resume),
        finished: Mutex::new(Some(finished_tx)),
    });
    receiver.registry = ImportRegistry::new();
    receiver.registry.register(Box::new(GatedImporter(gate)));
    let ctx = ServerContext::new(
        ServerConfig {
            data_dir: data.path().into(),
            teleport_home: Some(home.path().into()),
            teleport_ledger_dir: Some(data.path().join("ledger")),
            ..ServerConfig::default()
        },
        None,
    );
    // No periodic sweep participates in the deterministic handler experiment.
    ctx.shutdown_token().cancel();
    let service = TeleportServiceImpl::new(ctx.clone(), Arc::new(receiver));
    let wire = if sealed {
        cua_machine_seal::seal(
            ctx.machine_seal().unwrap().public(),
            "owned-cancel",
            cua_machine_seal::purpose::TELEPORT_BUNDLE,
            &tabs(),
        )
        .unwrap()
        .to_bytes()
    } else {
        tabs()
    };
    let request = ImportSessionRequest {
        import_id: "owned-cancel".into(),
        app: "zen".into(),
        sha256: hex::encode(Sha256::digest(&wire)),
        data: wire,
        commit: true,
        options: Some(ImportOptions {
            launch_after: true,
            ..Default::default()
        }),
        ..Default::default()
    };
    let task = tokio::spawn({
        let service = service.clone();
        async move { service.import_session(Request::new(request)).await }
    });
    tokio::time::timeout(DEADLINE, arrived)
        .await
        .unwrap()
        .unwrap();
    // The successor is visible, but an external browser cannot acquire it.
    assert!(!browser_can_lock(&lock));
    if let Some(inode) = old_inode {
        assert_ne!(fs::metadata(&lock).unwrap().ino(), inode);
    }
    assert_eq!(
        service
            .ledger_store()
            .read("owned-cancel")
            .unwrap()
            .is_some(),
        pause == Pause::AfterDurable
    );
    task.abort();
    assert!(tokio::time::timeout(DEADLINE, task)
        .await
        .unwrap()
        .unwrap_err()
        .is_cancelled());
    assert!(
        !browser_can_lock(&lock),
        "aborting the future cannot release a worker's lock early"
    );
    drop(release);
    tokio::time::timeout(DEADLINE, finished)
        .await
        .unwrap()
        .unwrap();
    // Wait for the blocking owner, not the cancelled JoinHandle, to retire its
    // upload. This bounded readback also asserts async cancellation cleanup.
    tokio::time::timeout(DEADLINE, async {
        loop {
            if fs::read_dir(data.path().join("teleport"))
                .unwrap()
                .next()
                .is_none()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let ledger = service.ledger_store().read("owned-cancel").unwrap();
    if pause == Pause::BeforeDecision {
        assert!(
            ledger.is_none(),
            "cancelled precommit work must never write its ledger"
        );
        if let Some(inode) = old_inode {
            assert_eq!(fs::metadata(&lock).unwrap().ino(), inode);
            assert_eq!(
                fs::read(profile.join("user.js")).unwrap(),
                b"predecessor bytes"
            );
            assert_eq!(
                fs::read(profile.join("neighbour")).unwrap(),
                b"do not remove"
            );
            assert!(browser_can_lock(&lock));
        } else {
            assert!(!profile.exists());
        }
    } else {
        assert!(
            ledger.is_some(),
            "postcommit cancellation is committed, never advertised as rollback"
        );
        assert_ne!(
            fs::read(profile.join("user.js")).unwrap(),
            b"predecessor bytes"
        );
        assert!(browser_can_lock(&lock));
    }
    assert!(
        host.calls_of(cua_spacesd_teleport::EffectKind::AppLaunch)
            .is_empty(),
        "no launch after either cancellation boundary"
    );
    let parent: PathBuf = profile.parent().unwrap().into();
    assert!(!fs::read_dir(parent).unwrap().any(|e| e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".cua-zen-stage-")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_public_service_handler_before_commit_restores_predecessor_and_never_ledgers_or_launches(
) {
    for existing in [false, true] {
        for sealed in [false, true] {
            cancel_at(Pause::BeforeDecision, existing, sealed).await;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_public_service_handler_after_durable_commit_keeps_ledger_and_suppresses_unstarted_launch(
) {
    for existing in [false, true] {
        for sealed in [false, true] {
            cancel_at(Pause::AfterDurable, existing, sealed).await;
        }
    }
}
