// SPDX-License-Identifier: FSL-1.1-MIT
// Copyright (c) 2026 Cua AI, Inc.

use base64::Engine as _;
use cua_keyvault::Error;
use cua_keyvault::model::{ItemPayload, PayloadEntry};
use cua_keyvault::record::{self, ExportOptions};

fn entry(path: &str, json: serde_json::Value) -> PayloadEntry {
    PayloadEntry {
        rel_path: path.into(),
        mode: 0o600,
        data: base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&json).unwrap()),
    }
}
fn cookie() -> serde_json::Value {
    serde_json::json!({"host_key":".a.example.test","name":"session","value":"c2VjcmV0","path":"/account","expires_utc":0,"is_secure":true,"is_httponly":true,"samesite":1,"creation_utc":13334473600000000i64})
}
fn options() -> ExportOptions {
    ExportOptions {
        opted_in: vec![
            "zen/cookies.sqlite".into(),
            "zen/storage".into(),
            "zen/logins.json".into(),
        ],
    }
}

#[test]
fn zen_codec_structures_all_sensitive_types_and_preserves_every_supported_attribute() {
    let codec = record::codec_for("zen");
    assert_eq!(codec.id(), "normalized-gecko-v1");
    let entries = vec![
        entry("cookies.json", serde_json::json!([cookie()])),
        entry(
            "localstorage.json",
            serde_json::json!([{"origin":"https://a.example.test","key":"token","value":"秘密🔒"}]),
        ),
        entry(
            "logins.json",
            serde_json::json!([{"origin":"https://a.example.test","username":"u","password":"c2VjcmV0","signon_realm":"https://a.example.test/"}]),
        ),
        entry(
            "zen/tabs.json",
            serde_json::json!(["https://a.example.test/account"]),
        ),
    ];
    let exported = codec.export(entries, &options()).unwrap();
    let payloads: Vec<_> = exported
        .records
        .into_iter()
        .map(|n| n.into_item("zen", "Zen", "selected", "full").1)
        .collect();
    assert_eq!(
        payloads
            .iter()
            .map(|p| p.schema.as_str())
            .collect::<Vec<_>>(),
        ["cookie@1", "local_storage@1", "password@1", "file@1"]
    );
    let back = codec.import(&payloads).unwrap();
    let cookies: Vec<serde_json::Value> = serde_json::from_slice(
        &base64::engine::general_purpose::STANDARD
            .decode(
                &back
                    .iter()
                    .find(|e| e.rel_path == "cookies.json")
                    .unwrap()
                    .data,
            )
            .unwrap(),
    )
    .unwrap();
    assert_eq!(cookies, [cookie()]);
    let round = codec.export(back, &options()).unwrap();
    assert_eq!(
        round
            .records
            .iter()
            .map(|r| (r.kind, &r.record))
            .collect::<std::collections::BTreeMap<_, _>>(),
        payloads
            .iter()
            .map(|p| (record::kind_of_schema(&p.schema).unwrap(), &p.record))
            .collect::<std::collections::BTreeMap<_, _>>()
    );
}

#[test]
fn zen_codec_rejects_missing_exact_optins_raw_source_keys_and_opaque_reserved_files() {
    let codec = record::codec_for("zen");
    for (path, value) in [
        ("cookies.json", serde_json::json!([cookie()])),
        ("localstorage.json", serde_json::json!([])),
        ("logins.json", serde_json::json!([])),
    ] {
        assert!(matches!(
            codec.export(
                vec![entry(path, value)],
                &ExportOptions {
                    opted_in: vec!["firefox/logins.json".into()]
                }
            ),
            Err(Error::Invalid(_))
        ));
    }
    for path in [
        "zen/key4.db",
        "key4.db",
        "zen/logins.json",
        "zen/cookies.sqlite",
        "zen/storage",
        "zen/sessionstore.jsonlz4",
    ] {
        assert!(matches!(
            codec.export(vec![entry(path, serde_json::json!([]))], &options()),
            Err(Error::Unsupported(_))
        ));
        let p = record::file_record(path, 0o600, b"[]")
            .unwrap()
            .into_item("zen", "Zen", "p", "full")
            .1;
        assert!(matches!(codec.import(&[p]), Err(Error::Unsupported(_))));
    }
    for path in ["cookies.json", "localstorage.json", "logins.json"] {
        let p = record::file_record(path, 0o600, b"[]")
            .unwrap()
            .into_item("zen", "Zen", "p", "full")
            .1;
        assert!(
            matches!(codec.import(&[p]), Err(Error::Unsupported(_))),
            "reserved payload cannot masquerade as a deliverable file"
        );
    }
}

#[test]
fn zen_codec_refuses_context_flattening_lossy_passwords_corruption_and_wrong_identity() {
    let codec = record::codec_for("zen");
    for (key, value) in [
        ("originAttributes", serde_json::json!("^userContextId=1")),
        ("partition_key", serde_json::json!("https://top.test")),
    ] {
        let mut c = cookie();
        c[key] = value;
        assert!(matches!(
            codec.export(
                vec![entry("cookies.json", serde_json::json!([c]))],
                &options()
            ),
            Err(Error::Unsupported(_))
        ));
    }
    let login = entry(
        "logins.json",
        serde_json::json!([{"origin":"https://a.test","username":"u","password":"/w==","signon_realm":"https://a.test/"}]),
    );
    assert!(matches!(
        codec.export(vec![login], &options()),
        Err(Error::Corrupt(_))
    ));
    let invalid = PayloadEntry {
        rel_path: "cookies.json".into(),
        mode: 0o600,
        data: "not-base64".into(),
    };
    assert!(matches!(
        codec.export(vec![invalid], &options()),
        Err(Error::Corrupt(_))
    ));
    let p = record::file_record("zen/tabs.json", 0o600, b"[]")
        .unwrap()
        .into_item("firefox", "Firefox", "p", "full")
        .1;
    assert!(matches!(codec.import(&[p]), Err(Error::Invalid(_))));
    let p = ItemPayload {
        provider_id: "zen".into(),
        scope: "full".into(),
        schema: "cookie@1".into(),
        record: serde_json::json!({"host_key":"a.test","originAttributes":"^userContextId=1"})
            .to_string(),
    };
    assert!(matches!(codec.import(&[p]), Err(Error::Unsupported(_))));
}

#[test]
fn exact_sites_are_never_psl_parent_sibling_scheme_or_port_widened() {
    assert!(record::exact_site_matches(
        ".a.example.test",
        "a.example.test",
        true
    ));
    for wrong in [".example.test", "child.a.example.test", "b.example.test"] {
        assert!(!record::exact_site_matches(wrong, "a.example.test", true));
    }
    assert!(record::exact_site_matches(
        "https://a.example.test",
        "https://a.example.test",
        false
    ));
    for wrong in [
        "http://a.example.test",
        "https://a.example.test:8443",
        "https://b.example.test",
        "https://child.a.example.test",
    ] {
        assert!(!record::exact_site_matches(
            wrong,
            "https://a.example.test",
            false
        ));
    }
    for invalid in [
        "https://user:pw@a.test",
        "a.test/path",
        "https://a.test/path",
        "a.test^userContextId=1",
        " a.test",
        "a.test?x",
        "https://a.test/#x",
    ] {
        assert!(matches!(
            record::exact_site_selection(invalid),
            Err(Error::Invalid(_))
        ));
    }
}
