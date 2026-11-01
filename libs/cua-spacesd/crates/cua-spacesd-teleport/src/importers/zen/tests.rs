// SPDX-License-Identifier: FSL-1.1-MIT
// Copyright (c) 2026 Cua AI, Inc.

use super::*;
use crate::importers::fixtures::bundle;
use crate::{FakeHost, Ledger, Receiver};
use std::io::Cursor;
use std::sync::Arc;

#[path = "transaction_tests.rs"]
mod transaction_tests;

fn receiver(home: &Path) -> Receiver {
    Receiver::with_host(home.into(), Arc::new(FakeHost::new()))
}
fn profile(home: &Path) -> PathBuf {
    home.join(".mozilla/firefox/Profiles").join(DEST_PROFILE)
}
fn tabs() -> Vec<u8> {
    bundle(
        "zen",
        &[(TABS_JSON, 0o600, br#"["https://one.example/account"]"#)],
    )
}
fn cookie() -> CookieItem {
    CookieItem {
        host_key: "one.example".into(),
        name: "session".into(),
        value: b"synthetic-only".to_vec(),
        path: "/".into(),
        expires_utc: CHROME_EPOCH + 1_900_000_000_000_000,
        is_secure: true,
        is_httponly: true,
        samesite: 1,
        extra: Default::default(),
    }
}
fn cookies(items: &[CookieItem]) -> Vec<u8> {
    bundle(
        "zen",
        &[(
            COOKIES_ENTRY,
            0o600,
            &cua_teleport_bundle::cookies::serialize(items),
        )],
    )
}

#[test]
fn normalized_registry_is_separate_and_does_not_change_firefox() {
    let home = tempfile::tempdir().unwrap();
    let r = receiver(home.path());
    assert!(r.provider_ids().contains(&"zen".into()));
    assert!(r.provider_ids().contains(&"firefox".into()));
    assert!(!r.provider_ids().contains(&"zen-browser".into()));
    let ini = home.path().join(".mozilla/firefox/profiles.ini");
    fs::create_dir_all(ini.parent().unwrap()).unwrap();
    fs::write(&ini, b"original default profile").unwrap();
    let result = r.import_bytes(&tabs(), "zen", false).unwrap();
    assert_eq!(fs::read(&ini).unwrap(), b"original default profile");
    assert_eq!(result.provider_id, "zen");
    assert!(!result.launched);
    assert!(profile(home.path()).join("user.js").is_file());
    let report = r.wipe(&Ledger::new("owned", "zen", &result.record, 0, 1));
    assert!(report.complete() && report.refused.is_empty(), "{report:?}");
    assert!(!profile(home.path()).exists());
    assert_eq!(fs::read(ini).unwrap(), b"original default profile");
}

#[test]
fn tabs_have_dedicated_explicit_firefox_launch_and_no_secret_stores() {
    let home = tempfile::tempdir().unwrap();
    let spec = ZenImporter::new()
        .import_to(&mut Cursor::new(tabs()), home.path(), Platform::Linux)
        .unwrap();
    assert_eq!(spec.program, "firefox");
    assert_eq!(
        spec.args,
        [
            "--profile",
            profile(home.path()).to_str().unwrap(),
            "--no-remote",
            "https://one.example/account"
        ]
    );
    for file in [
        "cookies.sqlite",
        "key4.db",
        "logins.json",
        "storage",
        "sessionstore.jsonlz4",
    ] {
        assert!(!profile(home.path()).join(file).exists(), "{file}");
    }
}

#[test]
fn cookies_and_storage_are_installed_losslessly_and_wiped() {
    let home = tempfile::tempdir().unwrap();
    let ls = cua_teleport_bundle::local_storage::serialize(&[LocalStorageItem {
        origin: "https://one.example:8443".into(),
        key: "token".into(),
        value: "café 🦊".into(),
        key_raw: None,
        value_raw: None,
    }]);
    let b = bundle(
        "zen",
        &[
            (
                COOKIES_ENTRY,
                0o600,
                &cua_teleport_bundle::cookies::serialize(&[cookie()]),
            ),
            (LOCAL_STORAGE_ENTRY, 0o600, &ls),
        ],
    );
    let r = receiver(home.path());
    let outcome = r.import_bytes(&b, "zen", false).unwrap();
    let db = rusqlite::Connection::open(profile(home.path()).join("cookies.sqlite")).unwrap();
    let row: (String, i64, i64, String) = db
        .query_row(
            "SELECT value, expiry, isHttpOnly, originAttributes FROM moz_cookies",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        row,
        ("synthetic-only".into(), 1_900_000_000_000, 1, "".into())
    );
    drop(db);
    let db = rusqlite::Connection::open(
        profile(home.path()).join("storage/default/https+++one.example+8443/ls/data.sqlite"),
    )
    .unwrap();
    let bytes: Vec<u8> = db
        .query_row("SELECT value FROM data WHERE key='token'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(bytes, "café 🦊".as_bytes());
    drop(db);
    let report = r.wipe(&Ledger::new("x", "zen", &outcome.record, 0, 1));
    assert!(report.complete() && report.refused.is_empty(), "{report:?}");
    assert!(!profile(home.path()).exists());
}

#[test]
fn every_unknown_raw_or_corrupt_entry_fails_before_destination_writes() {
    for (name, bytes) in [
        ("zen/key4.db", b"source-key".as_slice()),
        ("zen/cookies.sqlite", b"raw"),
        ("zen/storage", b"raw"),
        ("firefox/tabs.json", b"[]"),
        ("keychain.json", b"[]"),
        (TABS_JSON, b"not-json"),
        (COOKIES_ENTRY, b"[{}]"),
        (LOCAL_STORAGE_ENTRY, b"null"),
        (LOGINS_ENTRY, b"corrupt"),
        (TABS_JSON, br#"["file:///etc/passwd"]"#),
        (TABS_JSON, br#"["https://user:secret@example.com"]"#),
    ] {
        let home = tempfile::tempdir().unwrap();
        let err = receiver(home.path())
            .import_bytes(&bundle("zen", &[(name, 0o600, bytes)]), "zen", false)
            .unwrap_err();
        assert!(err.to_string().contains("Zen import:"), "{name}: {err}");
        assert_eq!(fs::read_dir(home.path()).unwrap().count(), 0, "{name}");
    }
}

#[test]
fn partition_container_raw_encoding_and_duplicate_fields_never_flatten() {
    let mut partitioned = cookie();
    partitioned.extra.partition_key = Some("https://top.example".into());
    let c = cookies(&[partitioned]);
    for b in [
        c,
        bundle(
            "zen",
            &[(
                COOKIES_ENTRY,
                0o600,
                br#"[{"host_key":"a","originAttributes":"^userContextId=1"}]"#,
            )],
        ),
        bundle(
            "zen",
            &[(
                LOCAL_STORAGE_ENTRY,
                0o600,
                br#"[{"origin":"https://a.example^userContextId=1","key":"x","value":"y"}]"#,
            )],
        ),
        bundle(
            "zen",
            &[(
                LOCAL_STORAGE_ENTRY,
                0o600,
                br#"[{"origin":"https://a.example","key":"x","value":"y","value_raw":"AAA="}]"#,
            )],
        ),
        bundle(
            "zen",
            &[(
                LOCAL_STORAGE_ENTRY,
                0o600,
                br#"[{"origin":"https://a.example","key":"x","key":"y","value":"y"}]"#,
            )],
        ),
    ] {
        let home = tempfile::tempdir().unwrap();
        assert!(receiver(home.path())
            .import_bytes(&b, "zen", false)
            .is_err());
        assert_eq!(fs::read_dir(home.path()).unwrap().count(), 0);
    }
}

#[test]
fn duplicate_entries_and_checksum_failure_leave_existing_profile_unchanged() {
    let home = tempfile::tempdir().unwrap();
    let r = receiver(home.path());
    r.import_bytes(&tabs(), "zen", false).unwrap();
    let old = fs::read(profile(home.path()).join("user.js")).unwrap();
    let mut corrupt = cookies(&[cookie()]);
    let needle = b"c3ludGhldGljLW9ubHk=";
    let pos = corrupt
        .windows(needle.len())
        .position(|x| x == needle)
        .unwrap();
    corrupt[pos] ^= 1;
    for b in [
        corrupt,
        bundle(
            "zen",
            &[(TABS_JSON, 0o600, b"[]"), (TABS_JSON, 0o600, b"[]")],
        ),
    ] {
        assert!(r.import_bytes(&b, "zen", false).is_err());
        assert_eq!(fs::read(profile(home.path()).join("user.js")).unwrap(), old);
        assert!(!profile(home.path()).join("cookies.sqlite").exists());
    }
}

#[test]
fn tabs_only_rejects_credentials_and_session_cookie_has_explicit_refusal() {
    let mut w = cua_teleport_bundle::bundle::BundleWriter::new(
        Vec::new(),
        "zen",
        "Zen",
        crate::TransferScope::TabsOnly,
    );
    w.add_bytes(COOKIES_ENTRY, 0o600, b"[]").unwrap();
    let home = tempfile::tempdir().unwrap();
    let r = receiver(home.path());
    assert!(r
        .import_bytes(&w.finish().unwrap(), "zen", false)
        .unwrap_err()
        .to_string()
        .contains("tabs-only"));
    let mut c = cookie();
    c.expires_utc = 0;
    assert!(r
        .import_bytes(&cookies(&[c]), "zen", false)
        .unwrap_err()
        .to_string()
        .contains("session-only cookies"));
    let mut c = cookie();
    c.extra.creation_utc = Some(i64::MIN);
    assert!(r
        .import_bytes(&cookies(&[c]), "zen", false)
        .unwrap_err()
        .to_string()
        .contains("invalid cookie timestamp"));
}

#[test]
fn same_site_outside_gecko_range_refuses_before_any_filesystem_mutation() {
    use std::os::unix::fs::MetadataExt;
    // Include directory identities/timestamps as well as file bytes: creating
    // and deleting temporary state is still a filesystem mutation.
    fn snapshot(root: &Path) -> Vec<(PathBuf, Vec<i128>, Vec<u8>)> {
        fn walk(root: &Path, p: &Path, out: &mut Vec<(PathBuf, Vec<i128>, Vec<u8>)>) {
            let m = fs::symlink_metadata(p).unwrap();
            out.push((
                p.strip_prefix(root).unwrap().into(),
                vec![
                    m.dev() as i128,
                    m.ino() as i128,
                    m.mode() as i128,
                    m.len() as i128,
                    m.mtime() as i128,
                    m.mtime_nsec() as i128,
                    m.ctime() as i128,
                    m.ctime_nsec() as i128,
                ],
                if m.is_file() {
                    fs::read(p).unwrap()
                } else {
                    vec![]
                },
            ));
            if m.is_dir() {
                let mut paths: Vec<_> = fs::read_dir(p)
                    .unwrap()
                    .map(|e| e.unwrap().path())
                    .collect();
                paths.sort();
                for path in paths {
                    walk(root, &path, out);
                }
            }
        }
        let mut out = vec![];
        walk(root, root, &mut out);
        out
    }
    for existing in [false, true] {
        let home = tempfile::tempdir().unwrap();
        let r = receiver(home.path());
        if existing {
            r.import_bytes(&cookies(&[cookie()]), "zen", false).unwrap();
        }
        fs::write(home.path().join("neighbour"), b"unchanged").unwrap();
        let before = snapshot(home.path());
        for value in [i64::MIN, -2, -1, 3, i64::MAX] {
            let mut c = cookie();
            c.samesite = value;
            let err = r.import_bytes(&cookies(&[c]), "zen", false).unwrap_err();
            assert_eq!(
                err.to_string(),
                "import failed: provider error: Zen import: invalid cookie expiry or SameSite"
            );
            assert_eq!(
                snapshot(home.path()),
                before,
                "SameSite={value}, existing={existing}"
            );
        }
    }
}

#[test]
fn same_site_supported_values_remain_lossless() {
    for value in 0..=2 {
        let home = tempfile::tempdir().unwrap();
        let mut c = cookie();
        c.samesite = value;
        receiver(home.path())
            .import_bytes(&cookies(&[c]), "zen", false)
            .unwrap();
        let db = rusqlite::Connection::open(profile(home.path()).join("cookies.sqlite")).unwrap();
        let actual: i64 = db
            .query_row("SELECT sameSite FROM moz_cookies", [], |r| r.get(0))
            .unwrap();
        assert_eq!(actual, value);
    }
}

#[test]
fn no_destination_nss_key_is_reported_as_not_sent() {
    let home = tempfile::tempdir().unwrap();
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
    let outcome = receiver(home.path())
        .import_bytes(&b, "zen", false)
        .unwrap();
    assert_eq!(outcome.notices.len(), 1);
    assert!(outcome.notices[0].contains("not sent"));
    assert!(!profile(home.path()).join("logins.json").exists());
}

#[test]
fn import_lock_is_nonblocking_and_retained_until_launch_admission_finishes() {
    let home = tempfile::tempdir().unwrap();
    let mut record = ImportRecord::default();
    let (_, lease) = ZenImporter::new()
        .import_with_lease(
            &mut Cursor::new(tabs()),
            home.path(),
            Platform::Linux,
            &mut record,
        )
        .unwrap();
    let error = receiver(home.path())
        .import_bytes(&tabs(), "zen", false)
        .unwrap_err();
    assert!(error.to_string().contains("import is busy"), "{error}");
    drop(lease);
    receiver(home.path())
        .import_bytes(&tabs(), "zen", false)
        .unwrap();
}

#[test]
fn foreign_existing_profile_and_symlink_ancestors_are_refused() {
    let home = tempfile::tempdir().unwrap();
    let p = profile(home.path());
    fs::create_dir_all(&p).unwrap();
    fs::write(p.join(MARKER), b"not ours").unwrap();
    fs::write(p.join("keep"), b"user data").unwrap();
    assert!(receiver(home.path())
        .import_bytes(&tabs(), "zen", false)
        .unwrap_err()
        .to_string()
        .contains("not created"));
    assert_eq!(fs::read(p.join("keep")).unwrap(), b"user data");
    let home = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), home.path().join(".mozilla")).unwrap();
    assert!(receiver(home.path())
        .import_bytes(&tabs(), "zen", false)
        .unwrap_err()
        .to_string()
        .contains("ancestor"));
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
}

#[test]
fn failure_during_staging_keeps_prior_generation_and_no_secret_staging() {
    let home = tempfile::tempdir().unwrap();
    let r = receiver(home.path());
    r.import_bytes(&tabs(), "zen", false).unwrap();
    fs::write(
        profile(home.path()).join("key4.db"),
        b"deliberately corrupt NSS database",
    )
    .unwrap();
    // An explicit symlink in an importer-owned profile is refused during clone,
    // before any incoming value is installed or the existing generation moved.
    std::os::unix::fs::symlink("/nonexistent", profile(home.path()).join("unsafe")).unwrap();
    assert!(r
        .import_bytes(&cookies(&[cookie()]), "zen", false)
        .unwrap_err()
        .to_string()
        .contains("symlink"));
    assert_eq!(
        fs::read(profile(home.path()).join("key4.db")).unwrap(),
        b"deliberately corrupt NSS database"
    );
    assert!(!profile(home.path()).join("cookies.sqlite").exists());
    assert!(!fs::read_dir(profile(home.path()).parent().unwrap())
        .unwrap()
        .any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".cua-zen-stage-")));
}
