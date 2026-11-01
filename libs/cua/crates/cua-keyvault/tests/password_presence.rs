// SPDX-License-Identifier: FSL-1.1-MIT
// Copyright (c) 2026 Cua AI, Inc.

//! Public broker flows over a real disposable sealed vault, with distinct fake
//! external presence/capture/delivery edges. This does not qualify OS consent.
use cua_keyvault::broker::*;
use cua_keyvault::model::{LoginRecord, PayloadEntry};
use cua_keyvault::{CallerIdentity, Error, Result, record};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Presence {
    passwords_allowed: AtomicBool,
    passwords_asked: AtomicUsize,
}
impl UserPresence for Presence {
    fn confirm(&self, _: &str) -> Result<()> {
        Ok(())
    }
    fn confirm_saved_passwords(&self, _: &str) -> Result<()> {
        self.passwords_asked.fetch_add(1, Ordering::SeqCst);
        if self.passwords_allowed.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(Error::PresenceFailed("password prompt declined".into()))
        }
    }
}
#[derive(Default)]
struct Edge {
    captures: AtomicUsize,
    deliveries: Mutex<Vec<Vec<PayloadEntry>>>,
}
impl Edge {
    fn password(&self, app: &str) -> Result<Vec<Captured>> {
        self.captures.fetch_add(1, Ordering::SeqCst);
        let n = record::password_record(&LoginRecord {
            origin: "https://synthetic.test".into(),
            username: "test".into(),
            password: "synthetic-only".into(),
        })?;
        let (meta, payload) = n.into_item(app, app, "selected", "full");
        Ok(vec![Captured { meta, payload }])
    }
}
#[async_trait::async_trait]
impl Backend for Edge {
    async fn preflight_target(&self, _: &str, _: &str) -> Result<()> {
        Ok(())
    }
    fn inventory(&self, _: &str, _: Option<&str>) -> Result<Inventory> {
        Ok(Inventory::default())
    }
    fn capture(&self, spec: &ImportSpec) -> Result<Vec<Captured>> {
        self.password(&spec.app)
    }
    fn capture_passwords(&self, spec: &PasswordImportSpec) -> Result<Vec<Captured>> {
        self.password(&spec.app)
    }
    async fn deliver(
        &self,
        _: &str,
        _: &str,
        _: &str,
        entries: Vec<PayloadEntry>,
        _: u64,
    ) -> Result<DeliveryOutcome> {
        self.deliveries.lock().unwrap().push(entries);
        Ok(DeliveryOutcome::default())
    }
    async fn wipe(&self, _: &str, _: &str) -> Result<Vec<String>> {
        Ok(vec![])
    }
}

#[tokio::test]
async fn password_import_approval_and_stored_delivery_cannot_reuse_generic_presence() {
    let temp = tempfile::tempdir().unwrap();
    let presence = Arc::new(Presence::default());
    let edge = Arc::new(Edge::default());
    let broker = Broker::new(
        BrokerConfig {
            dir: temp.path().join("vault"),
            keychain_path: None,
            os_protector: false,
        },
        edge.clone(),
        presence.clone(),
    )
    .unwrap();
    let cua = CallerIdentity::for_tests("com.trycua.cua", true);
    broker
        .init(
            &cua,
            InitRequest {
                os_protector: false,
                passphrase: Some("synthetic correct horse".into()),
                recovery_key: false,
            },
        )
        .await
        .unwrap();
    let spec = ImportSpec {
        app: "zen".into(),
        whole_app: true,
        paths: Some(vec!["zen/logins.json".into()]),
        confirm_passwords: true,
        ..Default::default()
    };
    let password_spec = PasswordImportSpec {
        app: "zen".into(),
        profile: Some("selected".into()),
        sites: vec![],
    };
    assert!(
        matches!(broker.import(&cua, spec.clone()).await, Err(Error::PresenceFailed(m)) if m == "password prompt declined")
    );
    assert!(
        matches!(broker.import_and_teleport(&cua, spec.clone(), "lab".into(), true).await, Err(Error::PresenceFailed(m)) if m == "password prompt declined")
    );
    assert!(
        matches!(broker.import_passwords(&cua, password_spec.clone()).await, Err(Error::PresenceFailed(m)) if m == "password prompt declined")
    );
    assert_eq!(
        edge.captures.load(Ordering::SeqCst),
        0,
        "no source read before password presence"
    );
    assert!(edge.deliveries.lock().unwrap().is_empty());
    assert_eq!(presence.passwords_asked.load(Ordering::SeqCst), 3);

    // Whole-category selection is legitimate with both explicit opt-in and the
    // separate presence gate; it must not be restricted to a named site.
    presence.passwords_allowed.store(true, Ordering::SeqCst);
    broker
        .import_and_teleport(&cua, spec, "lab".into(), true)
        .await
        .unwrap();
    assert_eq!(edge.captures.load(Ordering::SeqCst), 1);
    assert_eq!(edge.deliveries.lock().unwrap().len(), 1);
    assert_eq!(
        presence.passwords_asked.load(Ordering::SeqCst),
        4,
        "one password gate covers capture and delivery"
    );
    broker.browse(&cua).await.unwrap();
    let items = broker.list_items(&cua, 0, 100).await.unwrap().items;
    assert_eq!(items.len(), 1);
    let id = items[0].id.clone();

    presence.passwords_allowed.store(false, Ordering::SeqCst);
    let err = broker
        .teleport(
            &cua,
            TeleportRequest {
                items: vec![id.clone()],
                target: "lab".into(),
                include_passwords: true,
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, Error::PresenceFailed(m) if m == "password prompt declined"));
    assert_eq!(
        edge.deliveries.lock().unwrap().len(),
        1,
        "stored password delivery also needs its own gate"
    );

    let agent = CallerIdentity::for_tests("test.agent", false);
    let request = broker
        .request_access(
            &agent,
            AccessRequest {
                selectors: vec![Selector::Item { id }],
                actions: vec![cua_keyvault::Action::Login],
                targets: vec!["lab".into()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let err = broker
        .approve(&cua, &request.id, ApproveOptions::default())
        .await
        .unwrap_err();
    assert!(matches!(err, Error::PresenceFailed(m) if m == "password prompt declined"));
    assert_eq!(presence.passwords_asked.load(Ordering::SeqCst), 6);
}

#[test]
fn a_legacy_generic_presence_implementation_fails_closed_for_passwords() {
    struct Legacy;
    impl UserPresence for Legacy {
        fn confirm(&self, _: &str) -> Result<()> {
            Ok(())
        }
    }
    assert!(
        matches!(Legacy.confirm_saved_passwords("passwords"), Err(Error::PresenceFailed(m)) if m == "saved-password confirmation is unavailable")
    );
}
