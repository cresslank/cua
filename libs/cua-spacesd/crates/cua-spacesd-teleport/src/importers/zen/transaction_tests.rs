// SPDX-License-Identifier: FSL-1.1-MIT
// Copyright (c) 2026 Cua AI, Inc.

use super::*;

#[test]
fn ledger_failure_and_unwind_restore_predecessor_and_never_launch() {
    let home = tempfile::tempdir().unwrap();
    let host = Arc::new(FakeHost::new());
    let r = Receiver::with_host(home.path().into(), host.clone());
    r.import_bytes(&cookies(&[cookie()]), "zen", false).unwrap();
    let old = fs::read(profile(home.path()).join("cookies.sqlite")).unwrap();
    let blocker = home.path().join("not-a-directory");
    fs::write(&blocker, b"block ledger creation").unwrap();
    let store = crate::LedgerStore::new(blocker.join("ledger"));
    let mut c = cookie();
    c.value = b"new-synthetic-value".to_vec();
    let error = r
        .import_bundle_with_commit(
            Cursor::new(cookies(&[c.clone()])),
            "zen",
            true,
            |provider, record| {
                store
                    .write(&Ledger::new("failed", provider, record, 0, 1))
                    .map_err(|e| e.to_string())
            },
        )
        .unwrap_err();
    assert!(error.to_string().contains("import rolled back"), "{error}");
    assert_eq!(
        fs::read(profile(home.path()).join("cookies.sqlite")).unwrap(),
        old
    );
    assert!(host.calls_of(crate::EffectKind::AppLaunch).is_empty());
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = r.import_bundle_with_commit(Cursor::new(cookies(&[c])), "zen", true, |_, _| {
            panic!("synthetic cancellation/unwind")
        });
    }));
    assert!(panicked.is_err());
    assert_eq!(
        fs::read(profile(home.path()).join("cookies.sqlite")).unwrap(),
        old
    );
    assert!(host.calls_of(crate::EffectKind::AppLaunch).is_empty());
}

#[test]
fn fresh_generation_is_removed_if_ledger_cannot_commit() {
    let home = tempfile::tempdir().unwrap();
    let r = receiver(home.path());
    let blocker = home.path().join("blocker");
    fs::write(&blocker, b"x").unwrap();
    let store = crate::LedgerStore::new(blocker.join("ledger"));
    let result =
        r.import_bundle_with_commit(Cursor::new(tabs()), "zen", false, |provider, record| {
            store
                .write(&Ledger::new("failed", provider, record, 0, 1))
                .map_err(|e| e.to_string())
        });
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("import rolled back"));
    assert!(!profile(home.path()).exists());
}

#[test]
fn primary_password_destination_refuses_and_preserves_key_and_stores() {
    let home = tempfile::tempdir().unwrap();
    let r = receiver(home.path());
    r.import_bytes(&tabs(), "zen", false).unwrap();
    let key = profile(home.path()).join("key4.db");
    let (salt, check, sealed) = cua_teleport_bundle::firefox_nss::testing::key4_blobs(
        b"0123456789abcdef01234567",
        b"synthetic-primary-password",
        true,
    );
    let db = rusqlite::Connection::open(&key).unwrap();
    db.execute_batch("CREATE TABLE metaData(id,item1,item2);CREATE TABLE nssPrivate(a11,a102);")
        .unwrap();
    db.execute(
        "INSERT INTO metaData VALUES('password',?1,?2)",
        rusqlite::params![salt, check],
    )
    .unwrap();
    db.execute(
        "INSERT INTO nssPrivate VALUES(?1,x'f8000000000000000000000000000001')",
        [sealed],
    )
    .unwrap();
    drop(db);
    let before = fs::read(&key).unwrap();
    let login = LoginItem {
        origin: "https://one.example".into(),
        username: "synthetic".into(),
        password: b"synthetic-password".to_vec(),
        signon_realm: "https://one.example/".into(),
    };
    let b = bundle(
        "zen",
        &[(
            LOGINS_ENTRY,
            0o600,
            &cua_teleport_bundle::logins::serialize(&[login]),
        )],
    );
    let error = r.import_bytes(&b, "zen", false).unwrap_err();
    assert!(error.to_string().contains("master password"), "{error}");
    assert_eq!(fs::read(&key).unwrap(), before);
    assert!(!profile(home.path()).join("logins.json").exists());
}

#[test]
fn corrupt_or_future_schema_destination_fails_without_losing_existing_data() {
    for version in [999, 0] {
        let home = tempfile::tempdir().unwrap();
        let r = receiver(home.path());
        r.import_bytes(&cookies(&[cookie()]), "zen", false).unwrap();
        let db = profile(home.path()).join("cookies.sqlite");
        rusqlite::Connection::open(&db)
            .unwrap()
            .pragma_update(None, "user_version", version)
            .unwrap();
        let before = fs::read(&db).unwrap();
        assert!(r
            .import_bytes(&cookies(&[cookie()]), "zen", false)
            .unwrap_err()
            .to_string()
            .contains("unsupported destination SQLite schema"));
        assert_eq!(fs::read(&db).unwrap(), before);
    }
}
