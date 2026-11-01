// SPDX-License-Identifier: FSL-1.1-MIT
// Copyright (c) 2026 Cua AI, Inc.

//! Normalized Gecko reads from a constrained, closed Zen source. SQLite only
//! opens private copies, never the original, and never consumes source SHM.
//! Snapshot stability is per-file-set, not an atomic cross-database transaction.

use crate::Result;
use crate::gecko_profile::{ResolvedGeckoProfile, fail};
use cua_teleport_bundle::cookies::CookieItem;
use cua_teleport_bundle::local_storage::LocalStorageItem;
use cua_teleport_bundle::logins::LoginItem;

#[derive(Debug, Default)]
pub struct GeckoLogins {
    pub logins: Vec<LoginItem>,
}

pub fn read_cookies(profile: &ResolvedGeckoProfile) -> Result<Vec<CookieItem>> {
    #[cfg(target_os = "linux")]
    {
        linux::cookies(profile)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = profile;
        Err(fail(
            "zen_platform_unsupported",
            "Zen export requires Linux",
        ))
    }
}
pub fn read_local_storage(profile: &ResolvedGeckoProfile) -> Result<Vec<LocalStorageItem>> {
    #[cfg(target_os = "linux")]
    {
        linux::storage(profile)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = profile;
        Err(fail(
            "zen_platform_unsupported",
            "Zen export requires Linux",
        ))
    }
}
pub fn read_logins(profile: &ResolvedGeckoProfile) -> Result<GeckoLogins> {
    #[cfg(target_os = "linux")]
    {
        linux::logins(profile)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = profile;
        Err(fail(
            "zen_platform_unsupported",
            "Zen export requires Linux",
        ))
    }
}
/// Plaintext origins only; does not open or decrypt the NSS key database.
pub fn login_hosts(profile: &ResolvedGeckoProfile) -> Result<Vec<String>> {
    #[cfg(target_os = "linux")]
    {
        linux::login_hosts(profile)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = profile;
        Err(fail(
            "zen_platform_unsupported",
            "Zen export requires Linux",
        ))
    }
}
pub fn read_tabs(profile: &ResolvedGeckoProfile) -> Result<Vec<String>> {
    #[cfg(target_os = "linux")]
    {
        linux::tabs(profile)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = profile;
        Err(fail(
            "zen_platform_unsupported",
            "Zen export requires Linux",
        ))
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use crate::gecko_profile::linux::{MAX_FILE, Stamp, stamp};
    use cua_teleport_bundle::cookies::CookieExtra;
    use rusqlite::Connection;
    use std::fs::{File, OpenOptions};
    use std::io::{Read, Write};
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::{Path, PathBuf};
    use zeroize::Zeroizing;

    const EPOCH: i64 = 11_644_473_600_000_000;
    const MAX_TEXT: usize = 32 * 1024 * 1024;
    fn invalid(m: impl std::fmt::Display) -> crate::TeleportError {
        fail("zen_format_invalid", m)
    }
    fn sql(e: rusqlite::Error) -> crate::TeleportError {
        invalid(format!("private Gecko database: {e}"))
    }
    fn io(e: std::io::Error) -> crate::TeleportError {
        fail("zen_source_io", e)
    }

    pub(super) struct Snapshot {
        _dir: tempfile::TempDir,
        pub(super) db: PathBuf,
    }
    type SourceComponent = (PathBuf, Option<(File, Stamp)>);
    fn components(profile: &ResolvedGeckoProfile, rel: &Path) -> Result<Vec<SourceComponent>> {
        let mut out = Vec::new();
        for suffix in ["", "-wal", "-journal"] {
            let mut name = rel.as_os_str().to_owned();
            name.push(suffix);
            let name = PathBuf::from(name);
            let f = profile.dir_fd.open(&name)?;
            let entry = if let Some(f) = f {
                let meta = stamp(&f.metadata().map_err(io)?);
                if meta.2 > MAX_FILE {
                    return Err(fail(
                        "zen_format_unsupported",
                        "database component exceeds size limit",
                    ));
                }
                Some((f, meta))
            } else {
                None
            };
            out.push((name, entry));
        }
        Ok(out)
    }
    /// Captured bytes are not publishable until the source file set is checked.
    /// Keeping the phases explicit makes it impossible to hand SQLite an
    /// unvalidated copy, and permits deterministic filesystem race tests.
    struct SnapshotAttempt {
        copy: Snapshot,
        before: Vec<SourceComponent>,
    }
    impl SnapshotAttempt {
        fn capture(profile: &ResolvedGeckoProfile, rel: &Path) -> Result<Option<Self>> {
            profile.ensure_closed()?;
            let before = components(profile, rel)?;
            if before[0].1.is_none() {
                if before.iter().any(|(_, e)| e.is_some()) {
                    return Err(invalid("database sidecar has no main file"));
                }
                return Ok(None);
            }
            let dir = tempfile::Builder::new()
                .prefix("cua-zen-")
                .tempdir()
                .map_err(io)?;
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
                .map_err(io)?;
            for (name, entry) in &before {
                if let Some((f, _)) = entry {
                    let destination = dir
                        .path()
                        .join(name.file_name().ok_or_else(|| invalid("database name"))?);
                    let mut target = OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .mode(0o600)
                        .open(destination)
                        .map_err(io)?;
                    let n = std::io::copy(&mut f.take(MAX_FILE + 1), &mut target).map_err(io)?;
                    if n > MAX_FILE {
                        return Err(fail(
                            "zen_format_unsupported",
                            "database grew beyond size limit",
                        ));
                    }
                    target.flush().map_err(io)?;
                }
            }
            let db = dir
                .path()
                .join(rel.file_name().ok_or_else(|| invalid("database name"))?);
            Ok(Some(Self {
                copy: Snapshot { _dir: dir, db },
                before,
            }))
        }
        fn publish(self, profile: &ResolvedGeckoProfile, rel: &Path) -> Result<Snapshot> {
            let after = components(profile, rel)?;
            profile.ensure_closed()?;
            let mut stable = true;
            for ((_, a), (_, b)) in self.before.iter().zip(after.iter()) {
                stable &= match (a, b) {
                    (None, None) => true,
                    (Some((f, s)), Some((_, t))) => {
                        s == t && stamp(&f.metadata().map_err(io)?) == *s
                    }
                    _ => false,
                };
            }
            if stable {
                return Ok(self.copy);
            }
            Err(fail(
                "zen_source_busy",
                "database changed during capture; close Zen and retry",
            ))
        }
    }
    fn retry_snapshot(
        mut capture: impl FnMut() -> Result<Option<Snapshot>>,
    ) -> Result<Option<Snapshot>> {
        for attempt in 0..3 {
            match capture() {
                Err(crate::TeleportError::Provider(message))
                    if message.starts_with("zen_source_busy:") && attempt < 2 =>
                {
                    continue;
                }
                result => return result,
            }
        }
        unreachable!("the last attempt returns its refusal")
    }
    /// Main first, then present WAL and rollback journal. Source SQLite is never
    /// opened: journal recovery and SHM creation are exclusively in this TempDir.
    pub(super) fn snapshot(profile: &ResolvedGeckoProfile, rel: &Path) -> Result<Option<Snapshot>> {
        retry_snapshot(|| {
            SnapshotAttempt::capture(profile, rel)?
                .map(|a| a.publish(profile, rel))
                .transpose()
        })
    }
    fn open(copy: &Snapshot) -> Result<Connection> {
        let conn = Connection::open_with_flags(
            &copy.db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(sql)?;
        conn.busy_timeout(std::time::Duration::from_millis(100))
            .map_err(sql)?;
        let check: String = conn
            .query_row("PRAGMA quick_check", [], |r| r.get(0))
            .map_err(sql)?;
        if check != "ok" {
            return Err(invalid("private database integrity check failed"));
        }
        Ok(conn)
    }
    fn time(us: i64) -> Result<Option<i64>> {
        if us < 0 {
            return Err(invalid("negative cookie timestamp"));
        }
        if us == 0 {
            return Ok(None);
        }
        Ok(Some(
            us.checked_add(EPOCH)
                .ok_or_else(|| invalid("cookie timestamp overflow"))?,
        ))
    }
    pub(super) fn cookies(profile: &ResolvedGeckoProfile) -> Result<Vec<CookieItem>> {
        let Some(copy) = snapshot(profile, Path::new("cookies.sqlite"))? else {
            return Ok(Vec::new());
        };
        let conn = open(&copy)?;
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(sql)?;
        if !matches!(version, 16 | 17) {
            return Err(fail(
                "zen_format_unsupported",
                "only Gecko cookie schemas 16 and 17 are supported",
            ));
        }
        let mut statement = conn.prepare("SELECT name,value,host,path,expiry,lastAccessed,creationTime,isSecure,isHttpOnly,sameSite,originAttributes,isPartitionedAttributeSet,inBrowserElement FROM moz_cookies").map_err(sql)?;
        let mut rows = statement.query([]).map_err(sql)?;
        let mut out = Vec::new();
        while let Some(r) = rows.next().map_err(sql)? {
            let attrs: String = r.get(10).map_err(sql)?;
            let partitioned: i64 = r.get(11).map_err(sql)?;
            let in_browser: i64 = r.get(12).map_err(sql)?;
            if !attrs.is_empty() || partitioned != 0 || in_browser != 0 {
                return Err(fail(
                    "zen_format_unsupported",
                    "cookies with container/partition attributes are withheld; choose a default-context site",
                ));
            }
            let expiry: i64 = r.get(4).map_err(sql)?;
            if expiry < 0 {
                return Err(invalid("negative cookie expiry"));
            }
            let expiry = if expiry == 0 {
                0
            } else {
                expiry
                    .checked_mul(1000)
                    .and_then(|v| v.checked_add(EPOCH))
                    .ok_or_else(|| invalid("cookie expiry overflow"))?
            };
            let secure: i64 = r.get(7).map_err(sql)?;
            let http: i64 = r.get(8).map_err(sql)?;
            let same: i64 = r.get(9).map_err(sql)?;
            if !matches!(secure, 0 | 1) || !matches!(http, 0 | 1) || !matches!(same, 0..=2) {
                return Err(invalid("invalid cookie flags"));
            }
            let host: String = r.get(2).map_err(sql)?;
            let path: String = r.get(3).map_err(sql)?;
            if host.is_empty() || !path.starts_with('/') || host.contains(['/', '\\', '^']) {
                return Err(invalid("invalid cookie host/path"));
            }
            let value: String = r.get(1).map_err(sql)?;
            out.push(CookieItem {
                host_key: host,
                name: r.get(0).map_err(sql)?,
                value: value.into_bytes(),
                path,
                expires_utc: expiry,
                is_secure: secure == 1,
                is_httponly: http == 1,
                samesite: same,
                extra: CookieExtra {
                    creation_utc: time(r.get(6).map_err(sql)?)?,
                    last_access_utc: time(r.get(5).map_err(sql)?)?,
                    ..Default::default()
                },
            });
            if out.len() > 100_000 {
                return Err(fail("zen_format_unsupported", "too many cookie rows"));
            }
        }
        profile.ensure_closed()?;
        Ok(out)
    }
    fn origin(raw: &str) -> Result<String> {
        if raw.contains('^') {
            return Err(fail(
                "zen_format_unsupported",
                "non-default origin attributes are withheld",
            ));
        }
        let u = url::Url::parse(raw).map_err(|_| invalid("invalid HTTP(S) origin"))?;
        if !matches!(u.scheme(), "http" | "https")
            || u.host_str().is_none()
            || !u.username().is_empty()
            || u.password().is_some()
            || !matches!(u.path(), "" | "/")
            || u.query().is_some()
            || u.fragment().is_some()
        {
            return Err(fail(
                "zen_format_unsupported",
                "only HTTP(S) origins without credentials are supported",
            ));
        }
        Ok(u.origin().ascii_serialization())
    }
    fn origin_dir(name: &str) -> Result<Option<String>> {
        if name.contains('^') {
            return Err(fail(
                "zen_format_unsupported",
                "container/partition localStorage is withheld",
            ));
        }
        let Some((scheme, rest)) = name.split_once("+++") else {
            return Ok(None);
        };
        if !matches!(scheme, "http" | "https") {
            return Ok(None);
        }
        origin(&format!("{scheme}://{}", rest.replace('+', ":"))).map(Some)
    }
    fn decode(conversion: i64, compression: i64, length: i64, blob: &[u8]) -> Result<String> {
        if !(0..=MAX_TEXT as i64).contains(&length) || blob.len() > MAX_TEXT {
            return Err(fail(
                "zen_format_unsupported",
                "localStorage value exceeds limit",
            ));
        }
        let bytes = Zeroizing::new(match compression {
            0 => blob.to_vec(),
            1 => {
                let n =
                    snap::raw::decompress_len(blob).map_err(|_| invalid("invalid Snappy value"))?;
                if n > MAX_TEXT {
                    return Err(fail(
                        "zen_format_unsupported",
                        "Snappy output exceeds limit",
                    ));
                }
                snap::raw::Decoder::new()
                    .decompress_vec(blob)
                    .map_err(|_| invalid("invalid Snappy value"))?
            }
            _ => {
                return Err(fail(
                    "zen_format_unsupported",
                    "unknown localStorage compression",
                ));
            }
        });
        let value = match conversion {
            1 => String::from_utf8(bytes.to_vec())
                .map_err(|_| invalid("localStorage is not lossless UTF-8"))?,
            0 => {
                if bytes.len() % 2 != 0 {
                    return Err(invalid("odd UTF-16 byte count"));
                }
                let units: Vec<u16> = bytes
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect();
                String::from_utf16(&units)
                    .map_err(|_| invalid("localStorage contains unpaired UTF-16 surrogate"))?
            }
            _ => {
                return Err(fail(
                    "zen_format_unsupported",
                    "unknown localStorage conversion",
                ));
            }
        };
        if value.encode_utf16().count() as i64 != length {
            return Err(invalid("localStorage UTF-16 length mismatch"));
        }
        Ok(value)
    }
    pub(super) fn storage(profile: &ResolvedGeckoProfile) -> Result<Vec<LocalStorageItem>> {
        profile.ensure_closed()?;
        let Some(root) = profile.dir_fd.child(Path::new("storage/default"))? else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        let mut total_text = 0usize;
        for name in root.names()? {
            let name = name
                .to_str()
                .ok_or_else(|| invalid("non-UTF-8 storage origin"))?;
            let Some(expected) = origin_dir(name)? else {
                continue;
            };
            let rel = Path::new("storage/default")
                .join(name)
                .join("ls/data.sqlite");
            let Some(copy) = snapshot(profile, &rel)? else {
                continue;
            };
            let conn = open(&copy)?;
            let mut st = conn.prepare("SELECT origin FROM database").map_err(sql)?;
            let origins = st
                .query_map([], |r| r.get::<_, String>(0))
                .map_err(sql)?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(sql)?;
            if origins.len() != 1 || origin(&origins[0])? != expected {
                return Err(invalid("localStorage origin does not match its directory"));
            }
            let mut st = conn
                .prepare("SELECT key,utf16_length,conversion_type,compression_type,value FROM data")
                .map_err(sql)?;
            let mut rows = st.query([]).map_err(sql)?;
            while let Some(r) = rows.next().map_err(sql)? {
                let blob = Zeroizing::new(r.get::<_, Vec<u8>>(4).map_err(sql)?);
                let key: String = r.get(0).map_err(sql)?;
                let value = decode(
                    r.get(2).map_err(sql)?,
                    r.get(3).map_err(sql)?,
                    r.get(1).map_err(sql)?,
                    &blob,
                )?;
                total_text = total_text
                    .checked_add(key.len())
                    .and_then(|n| n.checked_add(value.len()))
                    .and_then(|n| n.checked_add(expected.len()))
                    .ok_or_else(|| invalid("localStorage total size overflow"))?;
                if total_text > MAX_TEXT {
                    return Err(fail(
                        "zen_format_unsupported",
                        "normalized localStorage exceeds aggregate size limit",
                    ));
                }
                out.push(LocalStorageItem {
                    origin: expected.clone(),
                    key,
                    value,
                    key_raw: None,
                    value_raw: None,
                });
                if out.len() > 100_000 {
                    return Err(fail("zen_format_unsupported", "too many localStorage rows"));
                }
            }
        }
        profile.ensure_closed()?;
        Ok(out)
    }
    fn login_entries(profile: &ResolvedGeckoProfile) -> Result<Vec<serde_json::Value>> {
        let Some(bytes) = profile.read(Path::new("logins.json"), MAX_TEXT as u64)? else {
            return Ok(Vec::new());
        };
        let doc: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|_| invalid("invalid logins JSON"))?;
        if doc.get("version").and_then(|v| v.as_i64()) != Some(3) {
            return Err(fail(
                "zen_format_unsupported",
                "only logins version 3 is supported",
            ));
        }
        let entries = doc
            .get("logins")
            .and_then(|v| v.as_array())
            .ok_or_else(|| invalid("logins array is missing"))?;
        for entry in entries {
            let host = entry
                .get("hostname")
                .and_then(|v| v.as_str())
                .ok_or_else(|| invalid("login hostname is missing"))?;
            origin(host)?;
            if entry
                .get("originAttributes")
                .is_some_and(|v| v.as_str() != Some(""))
                || entry
                    .get("userContextId")
                    .is_some_and(|v| v.as_u64() != Some(0))
            {
                return Err(fail(
                    "zen_format_unsupported",
                    "non-default login context is withheld",
                ));
            }
            // The interchange cannot preserve HTTP-auth realm/form scope.
            if entry.get("httpRealm").is_some_and(|v| !v.is_null()) {
                return Err(fail(
                    "zen_format_unsupported",
                    "HTTP-auth login realms are not supported",
                ));
            }
            if entry.get("encType").and_then(|v| v.as_i64()) != Some(1) {
                return Err(fail(
                    "zen_format_unsupported",
                    "unsupported NSS login encryption",
                ));
            }
        }
        Ok(entries.clone())
    }
    pub(super) fn login_hosts(profile: &ResolvedGeckoProfile) -> Result<Vec<String>> {
        login_entries(profile)?
            .iter()
            .map(|e| origin(e["hostname"].as_str().unwrap()))
            .collect()
    }
    // Bound the existing pure NSS decoder's KDF work before invoking it. This
    // validates DER structure/depth and all integer parameters without changing
    // the historical Firefox decoder or attempting any Primary Password.
    fn bounded_nss_der(bytes: &[u8], depth: usize) -> Result<()> {
        if depth > 12 {
            return Err(invalid("NSS DER nesting exceeds limit"));
        }
        let mut at = 0;
        while at < bytes.len() {
            let tag = *bytes.get(at).ok_or_else(|| invalid("truncated NSS DER"))?;
            let first = *bytes
                .get(at + 1)
                .ok_or_else(|| invalid("truncated NSS DER"))?;
            at += 2;
            let len = if first < 128 {
                first as usize
            } else {
                let n = (first & 127) as usize;
                if n == 0 || n > 4 || n > bytes.len() - at {
                    return Err(invalid("invalid NSS DER length"));
                }
                let value = bytes[at..at + n]
                    .iter()
                    .fold(0usize, |a, &b| (a << 8) | b as usize);
                at += n;
                value
            };
            if len > bytes.len() - at {
                return Err(invalid("truncated NSS DER value"));
            }
            let value = &bytes[at..at + len];
            if tag == 0x30 {
                bounded_nss_der(value, depth + 1)?;
            }
            if tag == 0x02 {
                if value.is_empty() || value.len() > 4 || value[0] & 128 != 0 {
                    return Err(invalid("invalid NSS integer"));
                }
                let n = value.iter().fold(0u32, |a, &b| (a << 8) | b as u32);
                if n == 0 || n > 1_000_000 {
                    return Err(fail(
                        "zen_format_unsupported",
                        "NSS KDF parameter exceeds work limit",
                    ));
                }
            }
            at += len;
        }
        Ok(())
    }
    pub(super) fn logins(profile: &ResolvedGeckoProfile) -> Result<GeckoLogins> {
        let entries = login_entries(profile)?;
        if entries.is_empty() {
            return Ok(GeckoLogins::default());
        }
        let Some(copy) = snapshot(profile, Path::new("key4.db"))? else {
            return Err(invalid("selected logins have no key4.db"));
        };
        let conn = open(&copy)?;
        let (salt, check): (Vec<u8>, Vec<u8>) = conn
            .query_row(
                "SELECT item1,item2 FROM metaData WHERE id='password'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(sql)?;
        let keys = conn
            .prepare("SELECT a11 FROM nssPrivate WHERE a11 IS NOT NULL")
            .map_err(sql)?
            .query_map([], |r| r.get::<_, Vec<u8>>(0))
            .map_err(sql)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(sql)?;
        if keys.len() != 1 {
            return Err(fail("zen_format_unsupported", "ambiguous NSS login key"));
        }
        if salt.len() > 65536 || check.len() > 65536 || keys[0].len() > 65536 {
            return Err(fail(
                "zen_format_unsupported",
                "oversized NSS key parameters",
            ));
        }
        bounded_nss_der(&check, 0)?;
        bounded_nss_der(&keys[0], 0)?;
        let key = cua_teleport_bundle::firefox_nss::master_key(&salt, &check, &keys[0], b"").map_err(|_| fail("zen_primary_password", "NSS key is invalid or requires a Primary Password; no password is requested or guessed"))?;
        let mut out = GeckoLogins::default();
        for e in entries {
            let field = |name: &str| {
                e.get(name)
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| invalid("encrypted login field is missing"))
            };
            let user =
                cua_teleport_bundle::firefox_nss::decrypt_sdr(&key, field("encryptedUsername")?)
                    .map_err(|_| invalid("cannot decrypt selected username"))?;
            let password =
                cua_teleport_bundle::firefox_nss::decrypt_sdr(&key, field("encryptedPassword")?)
                    .map_err(|_| invalid("cannot decrypt selected password"))?;
            let username = String::from_utf8(user.to_vec())
                .map_err(|_| invalid("login username is not UTF-8"))?;
            let host = origin(field("hostname")?)?;
            // The wire format matches a form at the origin. Refuse narrower form
            // targets instead of silently broadening their saved credential scope.
            if let Some(form) = e.get("formSubmitURL").and_then(|v| v.as_str())
                && !form.is_empty()
                && origin(form)? != host
            {
                return Err(fail(
                    "zen_format_unsupported",
                    "cross-origin login form target is withheld",
                ));
            }
            out.logins.push(LoginItem {
                origin: host.clone(),
                username,
                password: password.to_vec(),
                signon_realm: cua_teleport_bundle::logins::signon_realm(&host),
            });
        }
        profile.ensure_closed()?;
        Ok(out)
    }
    pub(super) fn tabs(profile: &ResolvedGeckoProfile) -> Result<Vec<String>> {
        // Closed capture: clean shutdown session wins; recovery only if absent.
        // A malformed present file is an error, not a successful empty delivery.
        for path in [
            "sessionstore.jsonlz4",
            "sessionstore-backups/recovery.jsonlz4",
        ] {
            if let Some(bytes) = profile.read(Path::new(path), MAX_TEXT as u64)? {
                return tabs_json(&mozlz4(&bytes)?);
            }
        }
        Ok(Vec::new())
    }
    fn tabs_json(bytes: &[u8]) -> Result<Vec<String>> {
        let doc: serde_json::Value =
            serde_json::from_slice(bytes).map_err(|_| invalid("invalid sessionstore JSON"))?;
        let windows = doc
            .get("windows")
            .and_then(|v| v.as_array())
            .ok_or_else(|| invalid("Firefox-format sessionstore windows array is missing"))?;
        let mut out = Vec::new();
        for window in windows {
            for tab in window
                .get("tabs")
                .and_then(|v| v.as_array())
                .ok_or_else(|| invalid("sessionstore tabs array is missing"))?
            {
                if tab
                    .get("userContextId")
                    .is_some_and(|v| v.as_u64() != Some(0))
                    || tab
                        .get("originAttributes")
                        .is_some_and(|v| v.as_str() != Some(""))
                {
                    return Err(fail(
                        "zen_format_unsupported",
                        "non-default tab context is withheld",
                    ));
                }
                let entries = tab
                    .get("entries")
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| invalid("sessionstore entries array is missing"))?;
                if entries.is_empty() {
                    continue;
                }
                let index = tab
                    .get("index")
                    .and_then(|v| v.as_u64())
                    .ok_or_else(|| invalid("sessionstore current index is missing"))?;
                let entry = index
                    .checked_sub(1)
                    .and_then(|i| usize::try_from(i).ok())
                    .and_then(|i| entries.get(i))
                    .ok_or_else(|| invalid("sessionstore index is out of bounds"))?;
                let raw = entry
                    .get("url")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| invalid("sessionstore URL is missing"))?;
                let u = url::Url::parse(raw).map_err(|_| invalid("invalid sessionstore URL"))?;
                if matches!(u.scheme(), "http" | "https")
                    && u.host_str().is_some()
                    && u.username().is_empty()
                    && u.password().is_none()
                {
                    out.push(raw.to_owned());
                }
            }
        }
        Ok(out)
    }
    fn mozlz4(bytes: &[u8]) -> Result<Vec<u8>> {
        if bytes.len() < 12 || &bytes[..8] != b"mozLz40\0" {
            return Err(invalid("invalid mozLz4 header"));
        }
        let size = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
        if size > MAX_TEXT || bytes.len() > MAX_TEXT {
            return Err(fail("zen_format_unsupported", "mozLz4 exceeds size limit"));
        }
        let src = &bytes[12..];
        let mut out = Vec::with_capacity(size);
        let mut i = 0;
        fn length(src: &[u8], i: &mut usize, mut n: usize) -> Result<usize> {
            loop {
                let b = *src.get(*i).ok_or_else(|| invalid("truncated LZ4 length"))?;
                *i += 1;
                n = n
                    .checked_add(b as usize)
                    .filter(|n| *n <= MAX_TEXT)
                    .ok_or_else(|| invalid("LZ4 length overflow"))?;
                if b != 255 {
                    return Ok(n);
                }
            }
        }
        while i < src.len() {
            let token = src[i];
            i += 1;
            let lit = if token >> 4 == 15 {
                length(src, &mut i, 15)?
            } else {
                (token >> 4) as usize
            };
            if lit > src.len() - i || lit > size.saturating_sub(out.len()) {
                return Err(invalid("LZ4 literal exceeds bounds"));
            }
            out.extend_from_slice(&src[i..i + lit]);
            i += lit;
            if i == src.len() {
                break;
            }
            let offset = src
                .get(i..i + 2)
                .ok_or_else(|| invalid("truncated LZ4 offset"))?;
            let offset = u16::from_le_bytes(offset.try_into().unwrap()) as usize;
            i += 2;
            if offset == 0 || offset > out.len() {
                return Err(invalid("invalid LZ4 offset"));
            }
            let n = if token & 15 == 15 {
                length(src, &mut i, 15)?
            } else {
                (token & 15) as usize
            };
            let n = n + 4;
            if n > size.saturating_sub(out.len()) {
                return Err(invalid("LZ4 match exceeds bounds"));
            }
            for _ in 0..n {
                out.push(out[out.len() - offset]);
            }
        }
        if out.len() != size {
            return Err(invalid("mozLz4 decoded size mismatch"));
        }
        Ok(out)
    }
    #[cfg(test)]
    mod tests {
        include!("gecko_store_tests.rs");
    }
}
