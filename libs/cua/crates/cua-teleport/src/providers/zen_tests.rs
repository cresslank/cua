// SPDX-License-Identifier: FSL-1.1-MIT
// Copyright (c) 2026 Cua AI, Inc.

use super::*;
use crate::bundle::{BundleReader, VerifiedEntry};
use crate::firefox_store::testing as fixtures;
use crate::{FakeHost, TeleportError};
use std::fs;
use std::io::Cursor;

fn app(id: &str) -> AppRef {
    AppRef {
        app_id: id.to_owned(),
        display_name: "not the browser name".to_owned(),
        platform: Platform::Linux,
    }
}
fn fixture(deny: bool) -> (tempfile::TempDir, Arc<FakeHost>, ZenProvider, PathBuf) {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join(".zen/Profiles/synthetic");
    fs::create_dir_all(&dir).unwrap();
    let mut host = FakeHost::new().with_home(home.path());
    if deny {
        host = host.denying_authorization();
    }
    let host = Arc::new(host);
    let p = ZenProvider::new()
        .with_host(host.clone())
        .with_profile(&dir);
    fixtures::write_cookies(
        &dir,
        &[
            ("a.example", "auth", "a-secret", "/", 0, ""),
            ("b.example", "auth", "b-secret", "/", 0, ""),
        ],
    );
    fixtures::write_local_storage(
        &dir,
        "https+++a.example",
        "https://a.example",
        &[("token", "ls-secret", false)],
    );
    fixtures::write_logins(
        &dir,
        &[("https://a.example", "u", "login-secret")],
        true,
        b"",
    );
    for name in [
        "places.sqlite",
        "prefs.js",
        "containers.json",
        "zen-sessions.jsonlz4",
        "zen-live-folders.jsonlz4",
    ] {
        fs::write(dir.join(name), "must not travel").unwrap();
    }
    (home, host, p, dir)
}
fn unpack(bytes: Vec<u8>) -> Vec<VerifiedEntry> {
    let reader = BundleReader::open(Cursor::new(bytes)).unwrap();
    assert_eq!(reader.header().provider_id, "zen");
    assert_eq!(reader.header().app_display_name, "Zen");
    assert!(reader.header().entries.iter().all(|e| e.mode == 0o600));
    reader.read_all().unwrap()
}
fn code(e: TeleportError, expected: &str) {
    let TeleportError::Provider(s) = e else {
        panic!("not provider error");
    };
    assert_eq!(s.split(':').next().unwrap(), expected);
}
#[test]
fn exact_aliases_linux_only_canonical_label_and_manifest_allowlist() {
    let (_home, _host, p, _dir) = fixture(false);
    for id in APP_IDS {
        assert!(p.matches(&app(id)));
    }
    for id in [
        "firefox",
        "firefox-zen",
        "zen-nightly",
        "app.zen_browser.zen.desktop",
        "other-zen",
    ] {
        assert!(!p.matches(&app(id)));
    }
    assert!(p.platform_supported(Platform::Linux));
    assert!(!p.platform_supported(Platform::MacOS));
    assert!(!p.platform_supported(Platform::Windows));
    let manifest = p
        .manifest(&app("zen"), None, TransferScope::FullProfile)
        .unwrap();
    assert_eq!(manifest.app_display_name, "Zen");
    assert_eq!(manifest.provider_id, "zen");
    assert_eq!(
        manifest
            .items
            .iter()
            .map(|i| i.rel_path.as_str())
            .collect::<Vec<_>>(),
        [TABS_JSON, COOKIES, STORAGE, LOGINS]
    );
    crate::ux::testing::assert_opt_ins_match("zen", &manifest);
    for i in &manifest.items {
        assert_eq!(i.sensitive, i.rel_path != TABS_JSON);
        assert_eq!(i.default_checked, i.rel_path == TABS_JSON);
    }
    assert_eq!(
        p.manifest(&app("zen"), None, TransferScope::TabsOnly)
            .unwrap()
            .items
            .len(),
        1
    );
}
#[test]
fn implicit_full_scope_and_tabs_only_never_pack_sensitive_state_or_raw_files() {
    let (_home, _host, p, _dir) = fixture(false);
    for scope in [TransferScope::FullProfile, TransferScope::TabsOnly] {
        let mut bytes = Vec::new();
        p.export(&app("zen"), scope, &mut bytes).unwrap();
        let entries = unpack(bytes);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].rel_path, TABS_JSON);
        assert_eq!(entries[0].bytes, b"[]");
    }
}
#[test]
fn explicit_sensitive_keys_produce_only_reserved_normalized_entries() {
    let (_home, host, p, _dir) = fixture(false);
    let include = [COOKIES, STORAGE, LOGINS]
        .into_iter()
        .map(str::to_owned)
        .collect();
    let mut bytes = Vec::new();
    p.export_selected(
        &app("zen-browser"),
        TransferScope::FullProfile,
        Some(&include),
        &mut bytes,
    )
    .unwrap();
    assert_eq!(host.authorizations().len(), 1);
    let entries = unpack(bytes);
    assert_eq!(
        entries
            .iter()
            .map(|e| e.rel_path.as_str())
            .collect::<Vec<_>>(),
        ["cookies.json", "localstorage.json", "logins.json"]
    );
    let cookies = cua_teleport_bundle::cookies::parse(&entries[0].bytes);
    assert_eq!(cookies.len(), 2);
    assert_eq!(
        cookies
            .iter()
            .map(|c| c.host_key.as_str())
            .collect::<Vec<_>>(),
        ["a.example", "b.example"]
    );
    assert_eq!(
        cua_teleport_bundle::logins::parse(&entries[2].bytes)[0].password,
        b"login-secret"
    );
    assert!(!entries.iter().any(|e| e.rel_path.contains("key4")));
}
#[test]
fn host_denial_happens_before_password_decryption_and_writes_zero_bytes() {
    let (_home, host, p, dir) = fixture(true);
    fs::write(dir.join("key4.db"), "would fail if read").unwrap();
    let include = [LOGINS.to_owned()].into_iter().collect();
    let mut bytes = Vec::new();
    let e = p
        .export_selected(
            &app("zen"),
            TransferScope::FullProfile,
            Some(&include),
            &mut bytes,
        )
        .unwrap_err();
    assert!(
        e.to_string()
            .contains("sensitive session export was not authorized")
    );
    assert_eq!(host.authorizations().len(), 1);
    assert!(bytes.is_empty());
    let tabs = [TABS_JSON.to_owned()].into_iter().collect();
    p.export_selected(
        &app("zen"),
        TransferScope::FullProfile,
        Some(&tabs),
        &mut bytes,
    )
    .unwrap();
    assert_eq!(host.authorizations().len(), 1);
    assert_eq!(unpack(bytes).len(), 1);
}
#[test]
fn unknown_raw_password_key_and_sensitive_tabs_scope_selections_refuse() {
    let (_home, _host, p, _dir) = fixture(false);
    for (scope, name) in [
        (TransferScope::FullProfile, "zen/key4.db"),
        (TransferScope::FullProfile, "zen/sessionstore.jsonlz4"),
        (TransferScope::FullProfile, "zen/places.sqlite"),
        (TransferScope::FullProfile, "zen/storage/default"),
        (TransferScope::TabsOnly, COOKIES),
        (TransferScope::TabsOnly, LOGINS),
    ] {
        let set = [name.to_owned()].into_iter().collect();
        let mut bytes = Vec::new();
        code(
            p.export_selected(&app("zen"), scope, Some(&set), &mut bytes)
                .unwrap_err(),
            "zen_selection_invalid",
        );
        assert!(bytes.is_empty());
    }
}
#[test]
fn corrupt_selected_sensitive_data_is_not_successful_empty_delivery() {
    let (_home, _host, p, dir) = fixture(false);
    fs::write(dir.join("cookies.sqlite"), "corrupt").unwrap();
    let set = [COOKIES.to_owned()].into_iter().collect();
    let mut bytes = Vec::new();
    code(
        p.export_selected(
            &app("zen"),
            TransferScope::FullProfile,
            Some(&set),
            &mut bytes,
        )
        .unwrap_err(),
        "zen_format_invalid",
    );
    assert!(bytes.is_empty());
}
#[test]
fn reviewed_provider_keeps_profile_incarnation_when_defaults_change() {
    let (_home, _host, p, dir) = fixture(false);
    p.manifest(&app("zen"), None, TransferScope::TabsOnly)
        .unwrap();
    fs::rename(&dir, dir.with_file_name("old")).unwrap();
    fs::create_dir_all(&dir).unwrap();
    let mut bytes = Vec::new();
    code(
        p.export(&app("zen"), TransferScope::TabsOnly, &mut bytes)
            .unwrap_err(),
        "zen_profile_unsafe",
    );
    assert!(bytes.is_empty());
}
#[test]
fn window_and_other_application_selections_refuse() {
    let (_home, _host, p, _dir) = fixture(false);
    code(
        p.manifest(&app("firefox"), None, TransferScope::TabsOnly)
            .unwrap_err(),
        "zen_selection_invalid",
    );
    let window = crate::WindowRef {
        title: "window".into(),
        native_id: None,
    };
    code(
        p.manifest(&app("zen"), Some(&window), TransferScope::TabsOnly)
            .unwrap_err(),
        "zen_selection_invalid",
    );
}
