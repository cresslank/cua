// SPDX-License-Identifier: FSL-1.1-MIT
// Copyright (c) 2026 Cua AI, Inc.

//! Public daemon Backend and Broker boundaries over disposable real SQLite/NSS
//! stores. No real HOME, browser, vault, consent prompt or Space is accessed.
#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cua_keyvault::broker::{
    Backend, Broker, BrokerConfig, FakePresence, ImportSpec, InitRequest, SiteChoice,
};
use cua_keyvault::record::{self, CookieRecord, LocalStorageRecord};
use cua_keyvault::{CallerIdentity, Error, ItemKind};
use cua_spaces::Spaces;
use cua_spaces_ext::daemon::keyvault::DaemonBackend;
use cua_spaces_ext::teleport::AppSessions;
use cua_spacesd_client::testing::{MockAuth, MockServer};
use cua_teleport::providers::zen::{self, ZenProvider};
use cua_teleport::{ExportRegistry, FakeHost, Platform};
use rusqlite::Connection;
use sha2::{Digest, Sha256};

struct Rig {
    home: tempfile::TempDir,
    _registry: tempfile::TempDir,
    backend: DaemonBackend,
    profiles: Vec<PathBuf>,
}

fn seed(dir: &Path, marker: &str) {
    fs::create_dir_all(dir).unwrap();
    let cookies = Connection::open(dir.join("cookies.sqlite")).unwrap();
    cookies.execute_batch("PRAGMA user_version=17; CREATE TABLE moz_cookies (host TEXT,name TEXT,value TEXT,path TEXT,expiry INTEGER,isSecure INTEGER,isHttpOnly INTEGER,sameSite INTEGER,originAttributes TEXT,creationTime INTEGER,lastAccessed INTEGER,isPartitionedAttributeSet INTEGER,inBrowserElement INTEGER);").unwrap();
    for host in [
        ".a.example.test",
        "b.example.test",
        "child.a.example.test",
        ".example.test",
    ] {
        cookies.execute("INSERT INTO moz_cookies VALUES (?1,'session',?2,'/account',0,1,1,1,'',1700000000000000,1700000000000000,0,0)", (host, marker)).unwrap();
    }
    drop(cookies);
    for (encoded, origin) in [
        ("https+++a.example.test", "https://a.example.test"),
        ("http+++a.example.test", "http://a.example.test"),
        ("https+++a.example.test+8443", "https://a.example.test:8443"),
        ("https+++b.example.test", "https://b.example.test"),
    ] {
        let storage = dir.join("storage/default").join(encoded).join("ls");
        fs::create_dir_all(&storage).unwrap();
        let c = Connection::open(storage.join("data.sqlite")).unwrap();
        c.execute_batch("CREATE TABLE database(origin TEXT); CREATE TABLE data(key TEXT,utf16_length INTEGER,conversion_type INTEGER,compression_type INTEGER,value BLOB)").unwrap();
        c.execute("INSERT INTO database VALUES(?1)", [origin])
            .unwrap();
        let value = format!("{marker}:{origin}");
        c.execute(
            "INSERT INTO data VALUES('token',?1,1,0,?2)",
            (value.encode_utf16().count() as i64, value.as_bytes()),
        )
        .unwrap();
    }
    let master = *b"0123456789abcdef01234567";
    let (salt, check, key) =
        cua_teleport_bundle::firefox_nss::testing::key4_blobs(&master, b"", true);
    let c = Connection::open(dir.join("key4.db")).unwrap();
    c.execute_batch(
        "CREATE TABLE metaData(id TEXT,item1 BLOB,item2 BLOB); CREATE TABLE nssPrivate(a11 BLOB)",
    )
    .unwrap();
    c.execute(
        "INSERT INTO metaData VALUES('password',?1,?2)",
        (&salt, &check),
    )
    .unwrap();
    c.execute("INSERT INTO nssPrivate VALUES(?1)", [&key])
        .unwrap();
    let logins: Vec<_> = ["https://a.example.test", "http://a.example.test", "https://a.example.test:8443", "https://b.example.test"].iter().map(|origin| {
        let seal = |plain: &[u8]| cua_teleport_bundle::firefox_nss::testing::sealed_field(&master, b"synthetic-key-id", plain);
        serde_json::json!({"hostname":origin,"formSubmitURL":origin,"httpRealm":null,"encType":1,"encryptedUsername":seal(b"synthetic"),"encryptedPassword":seal(marker.as_bytes())})
    }).collect();
    fs::write(
        dir.join("logins.json"),
        serde_json::to_vec(&serde_json::json!({"version":3,"logins":logins})).unwrap(),
    )
    .unwrap();
}

fn rig() -> Rig {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join(".zen");
    let profiles = ["default", "selected"]
        .map(|p| root.join("Profiles").join(p))
        .to_vec();
    seed(&profiles[0], "default-only");
    seed(&profiles[1], "selected-only");
    fs::write(root.join("profiles.ini"), "[Profile0]\nName=default\nIsRelative=1\nPath=Profiles/default\nDefault=1\n[Profile1]\nName=selected\nIsRelative=1\nPath=Profiles/selected\n").unwrap();
    let host = Arc::new(FakeHost::new().with_home(home.path()));
    let mut registry = ExportRegistry::new();
    registry.register(Box::new(ZenProvider::new().with_host(host.clone())));
    let sessions = Arc::new(AppSessions::from_registry(registry));
    let registry = tempfile::tempdir().unwrap();
    let spaces = Spaces::builder()
        .home(registry.path())
        .operator_display(Arc::new(cua_spaces::operator::NoDisplay))
        .build();
    let backend = DaemonBackend::new(spaces, sessions)
        .with_host(host)
        .with_platform(Platform::Linux);
    Rig {
        home,
        _registry: registry,
        backend,
        profiles,
    }
}

fn hashes(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for e in fs::read_dir(dir).unwrap() {
            let e = e.unwrap();
            if e.file_type().unwrap().is_dir() {
                walk(root, &e.path(), out);
            } else {
                out.insert(
                    e.path().strip_prefix(root).unwrap().to_path_buf(),
                    Sha256::digest(fs::read(e.path()).unwrap()).to_vec(),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

fn spec(paths: &[&str]) -> ImportSpec {
    ImportSpec {
        app: "zen".into(),
        profile: Some("selected".into()),
        whole_app: true,
        paths: Some(paths.iter().map(|s| (*s).into()).collect()),
        ..Default::default()
    }
}

#[test]
fn zen_inventory_readiness_and_capture_use_the_same_explicit_profile_and_identity() {
    let r = rig();
    let before = hashes(r.home.path());
    for alias in zen::APP_IDS {
        let ready = r.backend.readiness(alias, Some("selected")).unwrap();
        let inventory = r.backend.inventory(alias, Some("selected")).unwrap();
        assert_eq!(ready.provider_id, "zen");
        assert_eq!(ready.app_display, "Zen");
        assert_eq!(ready.profile, inventory.profile);
        assert_eq!(
            ready.profile,
            Some(r.profiles[1].to_string_lossy().into_owned())
        );
        assert!(
            inventory
                .domains
                .iter()
                .any(|d| d.domain == "a.example.test" && d.cookies == 1)
        );
        assert!(
            inventory
                .domains
                .iter()
                .any(|d| d.domain == "https://a.example.test:8443"
                    && d.local_storage == 1
                    && d.passwords == 1)
        );
        let mut s = spec(&[zen::COOKIES]);
        s.app = (*alias).into();
        let out = r.backend.capture(&s).unwrap();
        assert_eq!(out.len(), 4);
        for c in out {
            assert_eq!(c.meta.provider_id, "zen");
            assert_eq!(c.payload.provider_id, "zen");
            assert_eq!(c.meta.source, r.profiles[1].to_string_lossy());
            let cookie: CookieRecord = serde_json::from_str(&c.payload.record).unwrap();
            assert_eq!(cookie.value, b"selected-only");
            assert_eq!(cookie.path, "/account");
            assert!(cookie.secure && cookie.http_only);
        }
    }
    assert_eq!(
        before,
        hashes(r.home.path()),
        "all source files and inventories stay unchanged"
    );
}

#[test]
fn zen_site_capture_keeps_only_the_exact_cookie_host_and_requested_storage_origin() {
    let r = rig();
    let before = hashes(r.home.path());
    let s = ImportSpec {
        app: "app.zen_browser.zen".into(),
        profile: Some("selected".into()),
        sites: vec![SiteChoice {
            site: "https://a.example.test".into(),
            include_storage: true,
            include_passwords: false,
        }],
        ..Default::default()
    };
    let out = r.backend.capture(&s).unwrap();
    assert_eq!(out.len(), 2);
    assert_eq!(
        out.iter()
            .filter(|c| c.meta.kind == ItemKind::Cookie)
            .count(),
        1
    );
    let storage = out
        .iter()
        .find(|c| c.meta.kind == ItemKind::LocalStorage)
        .unwrap();
    let v: LocalStorageRecord = serde_json::from_str(&storage.payload.record).unwrap();
    assert_eq!(v.origin, "https://a.example.test");
    assert_eq!(v.value, "selected-only:https://a.example.test");
    let mut whole = spec(&[zen::COOKIES, zen::STORAGE]);
    whole.domains = Some(vec!["https://a.example.test".into()]);
    let whole = r.backend.capture(&whole).unwrap();
    assert_eq!(whole.len(), 2);
    assert_eq!(before, hashes(r.home.path()));
}

#[test]
fn zen_saved_password_path_flags_and_site_choices_all_require_second_confirmation() {
    let r = rig();
    for mut s in [
        spec(&[zen::LOGINS]),
        ImportSpec {
            passwords: true,
            ..spec(&[])
        },
        ImportSpec {
            whole_app: false,
            sites: vec![SiteChoice {
                site: "https://a.example.test".into(),
                include_storage: false,
                include_passwords: true,
            }],
            ..spec(&[])
        },
    ] {
        assert!(
            matches!(r.backend.capture(&s), Err(Error::Invalid(m)) if m.contains("confirm_passwords"))
        );
        s.confirm_passwords = true;
        s.password_domains = Some(vec!["https://a.example.test".into()]);
        let out = r.backend.capture(&s).unwrap();
        let passwords: Vec<_> = out
            .iter()
            .filter(|c| c.meta.kind == ItemKind::Password)
            .collect();
        assert_eq!(passwords.len(), 1);
        let login: cua_keyvault::model::LoginRecord =
            serde_json::from_str(&passwords[0].payload.record).unwrap();
        assert_eq!(login.origin, "https://a.example.test");
        assert_eq!(login.password, "selected-only");
        assert!(out.iter().all(|c| c.payload.provider_id == "zen"));
    }
}

#[test]
fn zen_defaults_tabs_empty_unknown_keys_locked_and_missing_profiles_fail_closed() {
    let r = rig();
    let default = r
        .backend
        .capture(&ImportSpec {
            paths: None,
            ..spec(&[])
        })
        .unwrap();
    assert_eq!(default.len(), 1);
    assert_eq!(default[0].meta.key, zen::TABS_JSON);
    assert_eq!(
        r.backend.capture(&spec(&[zen::TABS_JSON])).unwrap().len(),
        1
    );
    assert!(r.backend.capture(&spec(&[])).unwrap().is_empty());
    for raw in [
        "zen/key4.db",
        "zen/cookies.sqlite-journal",
        "zen/prefs.js",
        "firefox/logins.json",
        "cookies.json",
    ] {
        assert!(
            matches!(r.backend.capture(&spec(&[raw])), Err(Error::Invalid(m)) if m.starts_with("zen_selection_invalid"))
        );
    }
    for app in [
        "not-zen",
        "zen-browser-other",
        "firefox",
        "org.mozilla.firefox",
    ] {
        assert!(matches!(
            r.backend.readiness(app, None),
            Err(Error::Unsupported(_))
        ));
    }
    for selected in ["missing", "../escape"] {
        let s = ImportSpec {
            profile: Some(selected.into()),
            ..spec(&[zen::COOKIES])
        };
        assert!(
            matches!(r.backend.capture(&s), Err(Error::Backend(m)) if m.contains("zen_profile_"))
        );
        assert!(
            matches!(r.backend.inventory("zen", Some(selected)), Err(Error::Backend(m)) if m.contains("zen_profile_"))
        );
        assert!(
            matches!(r.backend.readiness("zen", Some(selected)), Err(Error::Backend(m)) if m.contains("zen_profile_"))
        );
    }
    fs::write(r.profiles[1].join(".parentlock"), "locked").unwrap();
    assert!(
        matches!(r.backend.readiness("zen", Some("selected")), Err(Error::Backend(m)) if m.contains("zen_profile_locked"))
    );
    assert!(
        matches!(r.backend.inventory("zen", Some("selected")), Err(Error::Backend(m)) if m.contains("zen_profile_locked"))
    );
    assert!(
        matches!(r.backend.capture(&spec(&[zen::COOKIES])), Err(Error::Backend(m)) if m.contains("zen_profile_locked"))
    );
}

#[tokio::test]
async fn old_guest_refuses_before_presence_and_even_a_locked_source_is_not_read() {
    let r = rig();
    let mock = MockServer::start(MockAuth::default()).await;
    let registry = tempfile::tempdir().unwrap();
    let spaces = Spaces::builder()
        .home(registry.path())
        .operator_display(Arc::new(cua_spaces::operator::NoDisplay))
        .build();
    let target = spaces
        .add(&mock.url(), None, Some("old-guest".into()))
        .await
        .unwrap();
    let host = Arc::new(FakeHost::new().with_home(r.home.path()));
    let mut providers = ExportRegistry::new();
    providers.register(Box::new(ZenProvider::new().with_host(host.clone())));
    let backend = Arc::new(
        DaemonBackend::new(spaces, Arc::new(AppSessions::from_registry(providers))).with_host(host),
    );
    let presence = Arc::new(FakePresence::new(true));
    let vault = tempfile::tempdir().unwrap();
    let broker = Broker::new(
        BrokerConfig {
            dir: vault.path().join("vault"),
            keychain_path: None,
            os_protector: false,
        },
        backend,
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
    fs::write(r.profiles[1].join(".parentlock"), "locked").unwrap();
    for advertised in [false, true] {
        if advertised {
            mock.state.advertise(&["teleport.zen"]);
        }
        let prompts = presence.asked.lock().unwrap().len();
        let e = broker
            .import_and_teleport_launching(
                &cua,
                spec(&[zen::COOKIES]),
                target.id.clone(),
                false,
                false,
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(e, Error::Unsupported(m) if m.contains("normalized-gecko-v1")));
        assert_eq!(
            presence.asked.lock().unwrap().len(),
            prompts,
            "no presence before destination compatibility"
        );
        assert!(mock.state.teleport_imports().is_empty());
    }
}

#[test]
fn zen_password_domain_empty_and_primary_password_refuse_without_source_mutation() {
    let r = rig();
    let mut s = ImportSpec {
        confirm_passwords: true,
        password_domains: Some(Vec::new()),
        ..spec(&[zen::LOGINS])
    };
    assert!(r.backend.capture(&s).unwrap().is_empty());
    let master = *b"0123456789abcdef01234567";
    let (salt, check, key) =
        cua_teleport_bundle::firefox_nss::testing::key4_blobs(&master, b"synthetic-primary", true);
    let c = Connection::open(r.profiles[1].join("key4.db")).unwrap();
    c.execute("UPDATE metaData SET item1=?1,item2=?2", (&salt, &check))
        .unwrap();
    c.execute("UPDATE nssPrivate SET a11=?1", [&key]).unwrap();
    drop(c);
    s.password_domains = Some(vec!["https://a.example.test".into()]);
    let before = hashes(r.home.path());
    assert!(
        matches!(r.backend.capture(&s), Err(Error::Backend(m)) if m.contains("zen_primary_password"))
    );
    assert_eq!(before, hashes(r.home.path()));
}

#[test]
fn actual_zen_159_aes_login_fixture_is_read_without_truncating_the_nss_key() {
    use base64::Engine as _;
    let r = rig();
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/zen-modern-nss.json")).unwrap();
    let bytes = |key: &str| {
        base64::engine::general_purpose::STANDARD
            .decode(fixture[key].as_str().unwrap())
            .unwrap()
    };
    let (salt, check, a11) = (bytes("salt"), bytes("check"), bytes("a11"));
    let key = cua_teleport_bundle::firefox_nss::master_key(&salt, &check, &a11, b"").unwrap();
    assert_eq!(
        key.len(),
        32,
        "modern Zen uses AES-256 SDR, not the legacy 24-byte DES key"
    );
    let c = Connection::open(r.profiles[1].join("key4.db")).unwrap();
    c.execute("UPDATE metaData SET item1=?1,item2=?2", (&salt, &check))
        .unwrap();
    c.execute("UPDATE nssPrivate SET a11=?1", [&a11]).unwrap();
    drop(c);
    fs::write(
        r.profiles[1].join("logins.json"),
        serde_json::to_vec(&fixture["logins"]).unwrap(),
    )
    .unwrap();
    let before = hashes(r.home.path());
    let captured = r
        .backend
        .capture(&ImportSpec {
            confirm_passwords: true,
            password_domains: Some(vec!["http://localhost:38211".into()]),
            ..spec(&[zen::LOGINS])
        })
        .unwrap();
    assert_eq!(captured.len(), 1);
    let login: cua_keyvault::model::LoginRecord =
        serde_json::from_str(&captured[0].payload.record).unwrap();
    assert_eq!(login.origin, "http://localhost:38211");
    assert!(!login.username.is_empty() && !login.password.is_empty());
    assert_eq!(before, hashes(r.home.path()));
}

#[test]
fn zen_records_round_trip_structured_with_provider_and_scope_unchanged() {
    let r = rig();
    let mut s = spec(&[zen::COOKIES, zen::STORAGE, zen::LOGINS, zen::TABS_JSON]);
    s.confirm_passwords = true;
    s.domains = Some(vec!["https://a.example.test".into()]);
    let out = r.backend.capture(&s).unwrap();
    assert_eq!(out.len(), 4);
    let payloads: Vec<_> = out.into_iter().map(|c| c.payload).collect();
    let entries = record::codec_for("zen").import(&payloads).unwrap();
    let round = record::codec_for("zen")
        .export(
            entries,
            &record::ExportOptions {
                opted_in: vec![zen::COOKIES.into(), zen::STORAGE.into(), zen::LOGINS.into()],
            },
        )
        .unwrap();
    assert_eq!(round.records.len(), 4);
    for schema in ["cookie@1", "local_storage@1", "password@1", "file@1"] {
        assert_eq!(payloads.iter().filter(|p| p.schema == schema).count(), 1);
    }
    assert!(
        payloads
            .iter()
            .all(|p| p.provider_id == "zen" && p.scope == "full")
    );
}
