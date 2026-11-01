// SPDX-License-Identifier: FSL-1.1-MIT
// Copyright (c) 2026 Cua AI, Inc.

use super::*;
use crate::firefox_store::testing as fixture;
use crate::{FakeHost, Platform};
use rusqlite::params;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

fn profile() -> (tempfile::TempDir, ResolvedGeckoProfile) {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join(".zen/Profiles/synthetic");
    fs::create_dir_all(&dir).unwrap();
    let host = FakeHost::new().with_home(temp.path());
    let p = crate::gecko_profile::resolve_profile(&host, Platform::Linux, Some(&dir)).unwrap();
    (temp, p)
}
fn code<T: std::fmt::Debug>(r: Result<T>, expected: &str) {
    let crate::TeleportError::Provider(s) = r.unwrap_err() else {
        panic!("not provider error")
    };
    assert_eq!(s.split(':').next().unwrap(), expected);
}
fn hashes(root: &Path) -> BTreeMap<String, String> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
        for e in fs::read_dir(dir).unwrap() {
            let e = e.unwrap();
            let p = e.path();
            if e.file_type().unwrap().is_dir() {
                walk(root, &p, out);
            } else {
                out.insert(
                    p.strip_prefix(root).unwrap().to_string_lossy().into_owned(),
                    hex::encode(Sha256::digest(fs::read(p).unwrap())),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}
fn literal(bytes: &[u8]) -> Vec<u8> {
    let mut out = b"mozLz40\0".to_vec();
    out.extend((bytes.len() as u32).to_le_bytes());
    if bytes.len() < 15 {
        out.push((bytes.len() as u8) << 4);
    } else {
        out.push(0xf0);
        let mut n = bytes.len() - 15;
        while n >= 255 {
            out.push(255);
            n -= 255;
        }
        out.push(n as u8);
    }
    out.extend(bytes);
    out
}
fn invariant(conn: &Connection) -> (i64, i64) {
    conn.query_row(
        "SELECT (SELECT n FROM counter), (SELECT count(*) FROM rows)",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .unwrap()
}
fn transaction_db(path: &Path, mode: &str) -> Connection {
    let c = Connection::open(path).unwrap();
    c.execute_batch(&format!("PRAGMA journal_mode={mode}; PRAGMA wal_autocheckpoint=0; CREATE TABLE counter(n INTEGER); INSERT INTO counter VALUES(0); CREATE TABLE rows(id INTEGER PRIMARY KEY, data BLOB); PRAGMA wal_checkpoint(TRUNCATE);")).unwrap();
    c
}
fn add(c: &Connection, id: i64) {
    c.execute("INSERT INTO rows VALUES(?1,zeroblob(8192))", [id])
        .unwrap();
    c.execute("UPDATE counter SET n=(SELECT count(*) FROM rows)", [])
        .unwrap();
}
#[test]
fn schema16_and17_cookie_semantics_and_source_hashes() {
    for schema in [16, 17] {
        let (_temp, p) = profile();
        fixture::write_cookies(
            &p.dir,
            &[
                (
                    ".a.example",
                    "persistent",
                    "secret",
                    "/path",
                    1_900_000_000_123,
                    "",
                ),
                ("b.example", "session", "only-b", "/", 0, ""),
            ],
        );
        Connection::open(p.dir.join("cookies.sqlite"))
            .unwrap()
            .pragma_update(None, "user_version", schema)
            .unwrap();
        let before = hashes(&p.dir);
        let rows = cookies(&p).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].expires_utc, 13_544_473_600_123_000);
        assert_eq!(rows[1].expires_utc, 0);
        assert_eq!(rows[0].host_key, ".a.example");
        assert_eq!(rows[0].path, "/path");
        assert!(rows[0].is_secure && rows[0].is_httponly);
        assert_eq!(rows[0].samesite, 1);
        assert_eq!(rows[0].extra.creation_utc, Some(13_334_473_600_000_000));
        assert_eq!(before, hashes(&p.dir));
    }
}
#[test]
fn cookie_partition_container_schema_overflow_and_corruption_refuse() {
    for attrs in [
        "^userContextId=1",
        "^partitionKey=%28https%2Ca.example%29",
        "^privateBrowsingId=1",
    ] {
        let (_temp, p) = profile();
        fixture::write_cookies(&p.dir, &[("a.example", "x", "v", "/", 0, attrs)]);
        code(cookies(&p), "zen_format_unsupported");
    }
    let (_temp, p) = profile();
    fixture::write_cookies(&p.dir, &[("a.example", "x", "v", "/", i64::MAX, "")]);
    code(cookies(&p), "zen_format_invalid");
    let c = Connection::open(p.dir.join("cookies.sqlite")).unwrap();
    c.pragma_update(None, "user_version", 18).unwrap();
    drop(c);
    code(cookies(&p), "zen_format_unsupported");
    fs::write(p.dir.join("cookies.sqlite"), b"invalid SQLite").unwrap();
    code(cookies(&p), "zen_format_invalid");
}
#[test]
fn no_source_shm_created_for_exited_wal_without_shm() {
    let (_temp, p) = profile();
    let db = p.dir.join("cookies.sqlite");
    fixture::write_cookies(&p.dir, &[("a.example", "x", "main", "/", 0, "")]);
    let c = Connection::open(&db).unwrap();
    c.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; UPDATE moz_cookies SET value='wal-only'").unwrap();
    let mut persist: i32 = 1;
    let result = unsafe {
        rusqlite::ffi::sqlite3_file_control(
            c.handle(),
            c"main".as_ptr(),
            rusqlite::ffi::SQLITE_FCNTL_PERSIST_WAL,
            (&mut persist as *mut i32).cast(),
        )
    };
    assert_eq!(result, 0);
    drop(c);
    fs::remove_file(p.dir.join("cookies.sqlite-shm")).unwrap();
    assert!(p.dir.join("cookies.sqlite-wal").exists());
    let before = hashes(&p.dir);
    assert_eq!(cookies(&p).unwrap()[0].value, b"wal-only");
    assert_eq!(before, hashes(&p.dir));
    assert!(!p.dir.join("cookies.sqlite-shm").exists());
}
#[test]
fn open_wal_transaction_snapshot_recovers_committed_not_uncommitted_rows() {
    let (_temp, p) = profile();
    let c = transaction_db(&p.dir.join("test.sqlite"), "WAL");
    c.execute_batch("BEGIN IMMEDIATE").unwrap();
    add(&c, 1);
    c.execute_batch("COMMIT; BEGIN IMMEDIATE").unwrap();
    add(&c, 2);
    let before = hashes(&p.dir);
    let snap = snapshot(&p, Path::new("test.sqlite")).unwrap().unwrap();
    let private = open(&snap).unwrap();
    assert_eq!(invariant(&private), (1, 1));
    assert_eq!(before, hashes(&p.dir));
    c.execute_batch("ROLLBACK").unwrap();
}
#[test]
fn rollback_journal_spill_is_recovered_only_on_private_copy() {
    let (_temp, p) = profile();
    let c = transaction_db(&p.dir.join("test.sqlite"), "DELETE");
    c.execute_batch("PRAGMA cache_size=10; BEGIN IMMEDIATE")
        .unwrap();
    for i in 1..=80 {
        add(&c, i);
    }
    c.execute_batch("COMMIT").unwrap();
    let committed = fs::read(p.dir.join("test.sqlite")).unwrap();
    c.execute_batch("BEGIN IMMEDIATE; UPDATE rows SET data=zeroblob(16384); DELETE FROM rows WHERE id>40; UPDATE counter SET n=40").unwrap();
    assert!(p.dir.join("test.sqlite-journal").exists());
    assert_ne!(
        committed,
        fs::read(p.dir.join("test.sqlite")).unwrap(),
        "must exercise actual page spill"
    );
    let before = hashes(&p.dir);
    let snap = snapshot(&p, Path::new("test.sqlite")).unwrap().unwrap();
    assert_eq!(invariant(&open(&snap).unwrap()), (80, 80));
    assert_eq!(before, hashes(&p.dir));
    c.execute_batch("ROLLBACK").unwrap();
}
#[test]
fn source_symlink_db_and_sidecar_refuse() {
    let (_temp, p) = profile();
    fixture::write_cookies(&p.dir, &[]);
    std::os::unix::fs::symlink("cookies.sqlite", p.dir.join("cookies.sqlite-wal")).unwrap();
    code(cookies(&p), "zen_profile_unsafe");
    fs::remove_file(p.dir.join("cookies.sqlite-wal")).unwrap();
    fs::rename(p.dir.join("cookies.sqlite"), p.dir.join("elsewhere")).unwrap();
    std::os::unix::fs::symlink("elsewhere", p.dir.join("cookies.sqlite")).unwrap();
    code(cookies(&p), "zen_profile_unsafe");
}
#[test]
fn sqlite_writer_stress_never_returns_torn_transaction() {
    for mode in ["WAL", "DELETE"] {
        let (_temp, p) = profile();
        let db = p.dir.join("test.sqlite");
        drop(transaction_db(&db, mode));
        let stop = Arc::new(AtomicBool::new(false));
        let done = stop.clone();
        let (started, ready) = std::sync::mpsc::channel();
        let writer = std::thread::spawn(move || {
            let c = Connection::open(db).unwrap();
            c.execute_batch("PRAGMA cache_size=10; PRAGMA wal_autocheckpoint=0")
                .unwrap();
            started.send(()).unwrap();
            let mut i = 0;
            while !done.load(Ordering::Relaxed) {
                i += 1;
                c.execute_batch("BEGIN IMMEDIATE").unwrap();
                add(&c, i);
                c.execute_batch("COMMIT").unwrap();
            }
            i
        });
        ready.recv().unwrap();
        let mut valid = 0;
        let mut busy = 0;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            for _ in 0..60 {
                match snapshot(&p, Path::new("test.sqlite")) {
                    Ok(Some(snap)) => {
                        let (n, count) = invariant(&open(&snap).unwrap());
                        assert_eq!(n, count);
                        valid += 1;
                    }
                    Err(crate::TeleportError::Provider(e)) if e.starts_with("zen_source_busy:") => {
                        busy += 1
                    }
                    r => panic!("unexpected snapshot result: {:?}", r.map(|v| v.is_some())),
                }
            }
        }));
        stop.store(true, Ordering::Relaxed);
        let transactions = writer.join().unwrap();
        result.unwrap();
        assert!(transactions > 0);
        assert_eq!(valid + busy, 60);
        let snap = snapshot(&p, Path::new("test.sqlite")).unwrap().unwrap();
        let (n, count) = invariant(&open(&snap).unwrap());
        assert_eq!(n, count);
    }
}
#[test]
fn local_storage_utf8_utf16_and_snappy_round_trip_without_source_changes() {
    let (_temp, p) = profile();
    fixture::write_local_storage(
        &p.dir,
        "https+++a.example",
        "https://a.example",
        &[
            ("plain", "hello 😀", false),
            (
                "compressed",
                "long text and repeated long text and repeated",
                true,
            ),
        ],
    );
    let conn = Connection::open(
        p.dir
            .join("storage/default/https+++a.example/ls/data.sqlite"),
    )
    .unwrap();
    let utf16 = "Latin + 日本語 😀";
    let blob: Vec<u8> = utf16.encode_utf16().flat_map(u16::to_le_bytes).collect();
    conn.execute("INSERT INTO data(key,utf16_length,conversion_type,compression_type,value) VALUES('utf16',?1,0,0,?2)", params![utf16.encode_utf16().count() as i64, blob]).unwrap();
    drop(conn);
    let before = hashes(&p.dir);
    let items = storage(&p).unwrap();
    assert_eq!(items.len(), 3);
    assert_eq!(
        items.iter().find(|i| i.key == "utf16").unwrap().value,
        utf16
    );
    assert_eq!(before, hashes(&p.dir));
}
#[test]
fn invalid_storage_encodings_do_not_silently_replace_or_truncate() {
    for (conv, comp, len, blob, expected) in [
        (1, 0, 1, vec![255], "zen_format_invalid"),
        (0, 0, 1, vec![0], "zen_format_invalid"),
        (0, 0, 1, vec![0, 216], "zen_format_invalid"),
        (2, 0, 1, vec![1], "zen_format_unsupported"),
        (1, 2, 1, vec![1], "zen_format_unsupported"),
        (1, 1, 1, vec![255], "zen_format_invalid"),
        (1, 0, 2, b"x".to_vec(), "zen_format_invalid"),
    ] {
        code(decode(conv, comp, len, &blob), expected);
    }
    // Snappy's length prefix must be bounded before allocating its output.
    code(
        decode(1, 1, 1, &[0xff, 0xff, 0xff, 0x7f]),
        "zen_format_unsupported",
    );
}
#[test]
fn storage_attributes_and_mismatched_origins_refuse_extensions_are_withheld() {
    let (_temp, p) = profile();
    fixture::write_local_storage(
        &p.dir,
        "moz-extension+++synthetic",
        "moz-extension://synthetic",
        &[("private", "not exported", false)],
    );
    assert!(storage(&p).unwrap().is_empty());
    fixture::write_local_storage(
        &p.dir,
        "https+++a.example^userContextId=1",
        "https://a.example^userContextId=1",
        &[("k", "v", false)],
    );
    code(storage(&p), "zen_format_unsupported");
    fs::remove_dir_all(
        p.dir
            .join("storage/default/https+++a.example^userContextId=1"),
    )
    .unwrap();
    fixture::write_local_storage(
        &p.dir,
        "https+++a.example",
        "https://b.example",
        &[("k", "v", false)],
    );
    code(storage(&p), "zen_format_invalid");
}
#[test]
fn nss_legacy_and_pbes2_decrypt_only_private_copies() {
    for pbes2 in [false, true] {
        let (_temp, p) = profile();
        fixture::write_logins(
            &p.dir,
            &[
                ("https://a.example", "synthetic", "secret"),
                ("https://b.example", "other", "withheld"),
            ],
            pbes2,
            b"",
        );
        let before = hashes(&p.dir);
        assert_eq!(
            login_hosts(&p).unwrap(),
            ["https://a.example", "https://b.example"]
        );
        let items = logins(&p).unwrap().logins;
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].password, b"secret");
        assert_eq!(before, hashes(&p.dir));
    }
}
#[test]
fn primary_password_and_corrupt_login_records_are_explicit_refusals() {
    let (_temp, p) = profile();
    fixture::write_logins(
        &p.dir,
        &[("https://a.example", "u", "p")],
        true,
        b"not-empty",
    );
    assert_eq!(login_hosts(&p).unwrap(), ["https://a.example"]);
    code(logins(&p), "zen_primary_password");
    fs::write(p.dir.join("logins.json"), b"[]").unwrap();
    code(login_hosts(&p), "zen_format_unsupported");
    fs::write(p.dir.join("logins.json"), b"{bad").unwrap();
    code(login_hosts(&p), "zen_format_invalid");
}
#[test]
fn tabs_current_http_urls_only_and_no_raw_form_state() {
    let (_temp, p) = profile();
    let bytes = serde_json::to_vec(&serde_json::json!({"windows":[{"tabs":[
        {"index":2,"entries":[{"url":"https://old.example"},{"url":"https://a.example/account","postdata_b64":"secret"}]},
        {"index":1,"entries":[{"url":"file:///secret"}]}, {"index":1,"entries":[{"url":"javascript:alert(1)"}]},
        {"index":1,"entries":[{"url":"about:blank"}]}, {"index":1,"entries":[{"url":"https://u:p@a.example"}]}
    ]}]})).unwrap();
    fs::write(p.dir.join("sessionstore.jsonlz4"), literal(&bytes)).unwrap();
    let before = hashes(&p.dir);
    assert_eq!(tabs(&p).unwrap(), ["https://a.example/account"]);
    assert_eq!(before, hashes(&p.dir));
}
#[test]
fn sessionstore_corrupt_indices_context_and_zen_specific_variants_refuse() {
    for doc in [
        serde_json::json!([]),
        serde_json::json!({"tabs":[],"spaces":[]}),
        serde_json::json!({"windows":[{"tabs":[{"index":0,"entries":[{"url":"https://a.example"}]}]}]}),
    ] {
        code(
            tabs_json(&serde_json::to_vec(&doc).unwrap()),
            "zen_format_invalid",
        );
    }
    code(tabs_json(br#"{"windows":[{"tabs":[{"userContextId":1,"index":1,"entries":[{"url":"https://a.example"}]}]}]}"#), "zen_format_unsupported");
}
#[test]
fn mozlz4_strict_length_bounds_offsets_truncation_and_output_size() {
    assert_eq!(mozlz4(&literal(b"hello")).unwrap(), b"hello");
    for bytes in [
        vec![],
        b"mozLz40\0".to_vec(),
        b"mozLz40\0\x05\0\0\0\xf0".to_vec(),
        b"mozLz40\0\x05\0\0\0\x10a\0\0".to_vec(),
        b"mozLz40\0\x01\0\0\0\x20ab".to_vec(),
        b"mozLz40\0\x03\0\0\0\x10a".to_vec(),
    ] {
        code(mozlz4(&bytes), "zen_format_invalid");
    }
    let mut huge = b"mozLz40\0".to_vec();
    huge.extend(u32::MAX.to_le_bytes());
    code(mozlz4(&huge), "zen_format_unsupported");
    // overlapping match would amplify beyond the declared output.
    code(
        mozlz4(b"mozLz40\0\x01\0\0\0\x1fa\x01\0\xff\xff\0"),
        "zen_format_invalid",
    );
}
#[test]
fn snapshot_ctime_detects_same_size_rewrite_with_restored_mtime() {
    use std::os::unix::fs::MetadataExt;
    let (_temp, p) = profile();
    let path = p.dir.join("stable.json");
    fs::write(&path, "old").unwrap();
    let before = fs::metadata(&path).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(10));
    fs::write(&path, "new").unwrap();
    let times = [
        libc::timespec {
            tv_sec: before.atime(),
            tv_nsec: before.atime_nsec(),
        },
        libc::timespec {
            tv_sec: before.mtime(),
            tv_nsec: before.mtime_nsec(),
        },
    ];
    use std::os::unix::ffi::OsStrExt;
    let cpath = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    assert_eq!(
        unsafe { libc::utimensat(libc::AT_FDCWD, cpath.as_ptr(), times.as_ptr(), 0) },
        0
    );
    let after = fs::metadata(path).unwrap();
    assert_eq!(
        (
            before.ino(),
            before.len(),
            before.mtime(),
            before.mtime_nsec()
        ),
        (after.ino(), after.len(), after.mtime(), after.mtime_nsec())
    );
    assert_ne!(
        stamp(&before),
        stamp(&after),
        "ctime must distinguish a same-size rewritten source"
    );
}
#[test]
fn actively_changing_source_has_bounded_busy_refusal() {
    let (_temp, p) = profile();
    let c = transaction_db(&p.dir.join("changing.sqlite"), "DELETE");
    let mut attempts = 0;
    let start = std::time::Instant::now();
    // Exercise the real copy/publish/retry phases. Force a real committed source
    // change between those phases, rather than assuming a writer was scheduled.
    let result = retry_snapshot(|| {
        attempts += 1;
        let candidate = SnapshotAttempt::capture(&p, Path::new("changing.sqlite"))?.unwrap();
        c.execute_batch("UPDATE counter SET n=n+1").unwrap();
        candidate
            .publish(&p, Path::new("changing.sqlite"))
            .map(Some)
    });
    code(result.map(|s| s.is_some()), "zen_source_busy");
    assert_eq!(attempts, 3, "retry budget must stay bounded");
    assert!(start.elapsed() < std::time::Duration::from_secs(30));
}
#[test]
fn snapshot_sidecar_appearance_and_disappearance_invalidate_capture() {
    for suffix in ["-wal", "-journal"] {
        let (_temp, p) = profile();
        fixture::write_cookies(&p.dir, &[]);
        let rel = Path::new("cookies.sqlite");
        let side = p.dir.join(format!("cookies.sqlite{suffix}"));
        let captured = SnapshotAttempt::capture(&p, rel).unwrap().unwrap();
        fs::write(&side, b"synthetic sidecar").unwrap();
        code(captured.publish(&p, rel).map(|_| ()), "zen_source_busy");
        let captured = SnapshotAttempt::capture(&p, rel).unwrap().unwrap();
        fs::remove_file(side).unwrap();
        code(captured.publish(&p, rel).map(|_| ()), "zen_source_busy");
    }
}
#[test]
fn snapshot_files_and_directory_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let (_temp, p) = profile();
    fixture::write_cookies(&p.dir, &[]);
    let snap = snapshot(&p, Path::new("cookies.sqlite")).unwrap().unwrap();
    assert_eq!(
        fs::metadata(snap.db.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(&snap.db).unwrap().permissions().mode() & 0o777,
        0o600
    );
}
#[test]
fn nss_kdf_parameters_are_bounded_before_key_derivation() {
    code(
        bounded_nss_der(&[0x30, 6, 2, 4, 0x7f, 0xff, 0xff, 0xff], 0),
        "zen_format_unsupported",
    );
    code(bounded_nss_der(&[0x30, 0x80], 0), "zen_format_invalid");
    code(bounded_nss_der(&[2, 0], 0), "zen_format_invalid");
    code(bounded_nss_der(&[2, 1, 1], 13), "zen_format_invalid");
    bounded_nss_der(&[0x30, 3, 2, 1, 1], 0).unwrap();
}
#[test]
fn all_content_readers_use_same_closed_profile_guard() {
    let (_temp, p) = profile();
    fs::write(p.dir.join("parent.lock"), "held").unwrap();
    code(cookies(&p), "zen_profile_locked");
    code(storage(&p), "zen_profile_locked");
    code(logins(&p), "zen_profile_locked");
    code(login_hosts(&p), "zen_profile_locked");
    code(tabs(&p), "zen_profile_locked");
    assert_eq!(
        fs::read_to_string(p.dir.join("parent.lock")).unwrap(),
        "held"
    );
}
