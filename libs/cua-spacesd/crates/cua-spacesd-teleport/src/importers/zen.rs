// SPDX-License-Identifier: FSL-1.1-MIT
// Copyright (c) 2026 Cua AI, Inc.

//! Linux Zen → Firefox interoperability, not a raw profile restore. Only the
//! normalized contract is accepted. Existing Firefox profiles and profiles.ini
//! are never selected or rewritten. Same-UID hostile filesystem/ptrace races
//! are outside this boundary; symlinks and non-owned destination profiles fail
//! closed. Imports cooperate via flock; Firefox's own fcntl profile lock is held
//! through ledger commit, then released only immediately before deliberate launch.

use std::collections::BTreeSet;
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use cua_teleport_bundle::bundle::BundleReader;
use cua_teleport_bundle::cookies::{CookieItem, COOKIES_ENTRY};
use cua_teleport_bundle::layout::firefox::{launch_program_for, ONBOARDING_USER_JS};
use cua_teleport_bundle::local_storage::{LocalStorageItem, LOCAL_STORAGE_ENTRY};
use cua_teleport_bundle::logins::{LoginItem, LOGINS_ENTRY};
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use zeroize::Zeroizing;

use crate::{
    ImportProvider, ImportRecord, LaunchSpec, Platform, Result, TeleportError, WindowRestore,
};

use cua_teleport_bundle::layout::zen::TABS_JSON;
pub use cua_teleport_bundle::layout::zen::{DEST_PROFILE, FORMAT, ID};
const MARKER: &str = ".cua-zen-import";
const MAX_BYTES: u64 = 32 * 1024 * 1024;
const MAX_PROFILE_BYTES: u64 = 256 * 1024 * 1024;
const CHROME_EPOCH: i64 = 11_644_473_600_000_000;

fn fail(message: &str) -> TeleportError {
    TeleportError::Provider(format!("Zen import: {message}"))
}

/// A normalized Zen session is delivered into Firefox, never into Zen.
#[derive(Default)]
pub struct ZenImporter;

impl ZenImporter {
    pub fn new() -> Self {
        Self
    }

    fn receive(
        &self,
        bundle: &mut dyn Read,
        home: &Path,
        platform: Platform,
        record: &mut ImportRecord,
    ) -> Result<crate::GuardedImport> {
        if platform != Platform::Linux {
            return Err(fail("normalized-gecko-v1 is qualified only on Linux"));
        }
        // Parse/checksum/validate the COMPLETE bundle before filesystem writes.
        let data = Normalized::read(bundle)?;
        let home = checked_home(home)?;
        let parent = home.join(".mozilla/firefox/Profiles");
        let mut parents = Vec::new();
        private_ancestors(&home, &parent, &mut parents)?;
        let dest = parent.join(DEST_PROFILE);
        let lock = open_private(&parent.join(".cua.zen-import.lock"))?;
        // Nonblocking: no unbounded blocking worker queue or cancellation wait.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(fail("destination import is busy; retry after it completes"));
        }
        let exists = match fs::symlink_metadata(&dest) {
            Ok(m) if m.is_dir() && !m.file_type().is_symlink() => true,
            Ok(_) => return Err(fail("destination profile is not a plain directory")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => return Err(e.into()),
        };
        let browser_lock = if exists {
            if fs::read(dest.join(MARKER))? != FORMAT.as_bytes() {
                return Err(fail(
                    "destination was not created by the Zen importer; refusing it",
                ));
            }
            // Older owned generations may not yet have a lock inode. Establish
            // it before reading/copying any profile state.
            Some(lock_browser(open_private(&dest.join(".parentlock"))?)?)
        } else {
            None
        };
        let mut lease = Lease {
            _import: lock,
            _predecessor: browser_lock,
            browser: None,
            pending: None,
        };
        let staging = tempfile::Builder::new()
            .prefix(".cua-zen-stage-")
            .tempdir_in(&parent)?;
        let stage = staging.path().join("profile");
        fs::DirBuilder::new().mode(0o700).create(&stage)?;
        if exists {
            let mut quota = (0u64, 0usize);
            copy_profile(&dest, &stage, &mut quota)?;
        }
        let mut staged_record = ImportRecord::default();
        let mut changed = BTreeSet::new();
        if !data.cookies.is_empty() {
            let version = store_version(&stage.join("cookies.sqlite"), &[16, 17])?;
            crate::firefox_items::install_cookies(&stage, &data.cookies, &mut staged_record)?;
            if version == Some(17) {
                rusqlite::Connection::open(stage.join("cookies.sqlite"))
                    .and_then(|db| db.pragma_update(None, "user_version", 17))
                    .map_err(|_| fail("cannot preserve destination cookie schema"))?;
            }
            changed.insert(PathBuf::from("cookies.sqlite"));
        }
        if !data.storage.is_empty() {
            for item in &data.storage {
                let (scheme, rest) = item.origin.split_once("://").expect("validated origin");
                store_version(
                    &stage.join(format!(
                        "storage/default/{scheme}+++{}/ls/data.sqlite",
                        rest.replace(':', "+")
                    )),
                    &[80],
                )?;
            }
            crate::firefox_items::install_local_storage(&stage, &data.storage, &mut staged_record)?;
            for item in &data.storage {
                let (scheme, rest) = item.origin.split_once("://").expect("validated origin");
                let rel = PathBuf::from(format!(
                    "storage/default/{scheme}+++{}/ls/data.sqlite",
                    rest.replace(':', "+")
                ));
                // Real Gecko 159 LS schema 5.0 encodes its version as 5 << 4.
                // Without this marker Firefox treats a populated DB as an
                // unversioned store and localStorage throws NS_ERROR_FAILURE.
                let db = rusqlite::Connection::open(stage.join(&rel))
                    .map_err(|_| fail("cannot validate staged localStorage"))?;
                db.pragma_update(None, "user_version", 80)
                    .map_err(|_| fail("cannot version staged localStorage"))?;
                changed.insert(rel);
            }
        }
        if !data.logins.is_empty() {
            if stage.join("key4.db").exists() {
                store_version(&stage.join("key4.db"), &[])?;
            }
            if stage.join("logins.json").exists() {
                let existing: Value = json(&fs::read(stage.join("logins.json"))?)?;
                if !existing["logins"].is_array()
                    || existing["nextId"]
                        .as_i64()
                        .is_none_or(|n| n <= 0 || n.checked_add(data.logins.len() as i64).is_none())
                    || existing["version"] != 3
                {
                    return Err(fail("corrupt or unsupported destination saved-login store"));
                }
            }
            let n = saved_logins::install(&stage, &data.logins, &mut staged_record)?;
            if n > 0 {
                changed.insert(PathBuf::from("logins.json"));
            }
        }
        for (name, bytes) in [
            ("user.js", ONBOARDING_USER_JS.as_bytes()),
            (MARKER, FORMAT.as_bytes()),
        ] {
            let mut f = File::create(stage.join(name))?;
            f.write_all(bytes)?;
            changed.insert(PathBuf::from(name));
        }
        // Each generation needs its OWN browser-lock inode: exchange moves the
        // predecessor inode to staging. Never copy that lock into the successor.
        drop(open_private(&stage.join(".parentlock"))?);
        changed.insert(PathBuf::from(".parentlock"));
        // Build the ledger before the atomic boundary. Only changed stores are
        // owned, never a copied NSS key or any browser-created neighbour.
        let mut committed = ImportRecord {
            created_dirs: parents,
            ..Default::default()
        };
        sync_tree_and_note_dirs(&stage, &stage, &dest, &mut committed)?;
        for rel in changed {
            committed.file_written(&dest.join(rel));
        }
        committed.notices = std::mem::take(&mut staged_record.notices);
        let flags = if exists {
            libc::RENAME_EXCHANGE
        } else {
            libc::RENAME_NOREPLACE
        };
        // POSIX locks are process-owned: closing ANY descriptor for this inode
        // releases them. Acquire only after sync_tree has closed its descriptors,
        // and do not reopen the inode while this lease protects it.
        lease.browser = Some(lock_browser(open_private(&stage.join(".parentlock"))?)?);
        rename(&stage, &dest, flags)?;
        if let Err(e) = File::open(&parent).and_then(|f| f.sync_all()) {
            // The old generation is still at stage on exchange, so an fsync
            // failure can restore it without overwriting unrelated content.
            let undo = if exists {
                rename(&stage, &dest, libc::RENAME_EXCHANGE)
            } else {
                fs::rename(&dest, &stage)
            };
            if undo.is_err() {
                // Retain both generations rather than deleting recovery data.
                let _ = staging.keep();
                return Err(fail(
                    "commit sync and rollback failed; retained private staging for recovery",
                ));
            }
            return Err(e.into());
        }
        // Retain the old generation until the receiver's durable ledger callback
        // succeeds. Unwind/cancellation before commit restores the predecessor.
        lease.pending = Some(Pending {
            staging,
            stage,
            dest: dest.clone(),
            existed: exists,
        });
        record.merge(&committed);
        record.notices.extend(committed.notices);
        let mut args = vec![
            "--profile".into(),
            dest.to_string_lossy().into_owned(),
            "--no-remote".into(),
        ];
        args.extend(data.tabs.iter().cloned());
        Ok((
            LaunchSpec {
                program: launch_program_for(platform),
                args,
                env: vec![],
                cwd: None,
                restore_windows: vec![WindowRestore {
                    title: None,
                    urls: data.tabs,
                }],
            },
            Some(Box::new(lease)),
        ))
    }
}

struct Lease {
    _import: File,
    _predecessor: Option<File>,
    browser: Option<File>,
    pending: Option<Pending>,
}

struct Pending {
    staging: tempfile::TempDir,
    stage: PathBuf,
    dest: PathBuf,
    existed: bool,
}

impl crate::ImportLease for Lease {
    fn commit(&mut self) {
        // The durable ledger already owns the new generation's changed stores.
        self.pending.take();
        self._predecessor.take();
    }
    fn prepare_launch(&mut self) {
        // The durable import is now immutable to us; let our own Firefox acquire
        // its lock. Keep import admission until the launch attempt returns.
        self.browser.take();
    }
    fn rollback(&mut self) -> Result<()> {
        let Some(pending) = self.pending.take() else {
            return Ok(());
        };
        let restored = if pending.existed {
            rename(&pending.stage, &pending.dest, libc::RENAME_EXCHANGE)
        } else {
            fs::rename(&pending.dest, &pending.stage)
        };
        if restored.is_err() {
            let _ = pending.staging.keep();
            return Err(fail(
                "rollback failed; retained private staging for recovery",
            ));
        }
        File::open(pending.dest.parent().expect("profile parent"))?.sync_all()?;
        self.browser.take();
        Ok(())
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let _ = crate::ImportLease::rollback(self);
        // Close process-owned browser locks BEFORE opening import admission to
        // a successor: closing a late predecessor FD could unlock that next
        // same-process import. Explicit flock unlock also retires copies briefly
        // inherited by another thread's fork before its CLOEXEC descriptors close.
        self.browser.take();
        self._predecessor.take();
        unsafe { libc::flock(self._import.as_raw_fd(), libc::LOCK_UN) };
    }
}

impl ImportProvider for ZenImporter {
    fn id(&self) -> &str {
        ID
    }
    fn display_name(&self) -> &str {
        "Zen → Firefox"
    }
    fn import_recorded(
        &self,
        bundle: &mut dyn Read,
        home: &Path,
        platform: Platform,
        record: &mut ImportRecord,
    ) -> Result<LaunchSpec> {
        self.receive(bundle, home, platform, record)
            .map(|(spec, mut lease)| {
                if let Some(lease) = lease.as_mut() {
                    lease.commit();
                }
                spec
            })
    }
    fn import_with_lease(
        &self,
        bundle: &mut dyn Read,
        home: &Path,
        platform: Platform,
        record: &mut ImportRecord,
    ) -> Result<crate::GuardedImport> {
        self.receive(bundle, home, platform, record)
    }
}

fn checked_home(home: &Path) -> Result<PathBuf> {
    if !home.is_absolute()
        || home
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(fail("destination home must be an absolute plain directory"));
    }
    let canonical = home.canonicalize()?;
    if canonical != home || !fs::symlink_metadata(home)?.is_dir() {
        return Err(fail("destination home may not cross symlinks"));
    }
    Ok(canonical)
}

fn private_ancestors(home: &Path, dest: &Path, created: &mut Vec<PathBuf>) -> Result<()> {
    let mut p = home.to_path_buf();
    for part in dest
        .strip_prefix(home)
        .map_err(|_| fail("destination outside home"))?
    {
        p.push(part);
        match fs::symlink_metadata(&p) {
            Ok(m) if m.is_dir() && !m.file_type().is_symlink() => (),
            Ok(_) => return Err(fail("destination ancestor is not a plain directory")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                fs::DirBuilder::new().mode(0o700).create(&p)?;
                created.push(p.clone());
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

fn browser_closed(dest: &Path) -> Result<Option<File>> {
    let file = match OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(dest.join(".parentlock"))
    {
        Ok(file) if file.metadata()?.is_file() => file,
        Ok(_) => return Err(fail("browser lock is not a regular file")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    lock_browser(file).map(Some)
}

fn lock_browser(file: File) -> Result<File> {
    let mut flock: libc::flock = unsafe { std::mem::zeroed() };
    flock.l_type = libc::F_WRLCK as _;
    flock.l_whence = libc::SEEK_SET as _;
    // Firefox uses a POSIX write lock on the profile's .parentlock.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &flock) } == -1 {
        return Err(fail(
            "destination Firefox is running; close this exact profile and retry",
        ));
    }
    Ok(file)
}

/// Wipes and TTL sweeps share the import admission lock and never edit a live
/// destination browser. Missing state is a successful no-op, not recreated.
pub(crate) fn wipe_lease(home: &Path) -> Result<Option<Box<dyn Send>>> {
    let home = checked_home(home)?;
    let mut parent = home.clone();
    for part in [".mozilla", "firefox", "Profiles"] {
        parent.push(part);
        match fs::symlink_metadata(&parent) {
            Ok(m) if m.is_dir() && !m.file_type().is_symlink() => (),
            Ok(_) => return Err(fail("wipe ancestor is not a plain directory")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        }
    }
    let lock = open_private(&parent.join(".cua.zen-import.lock"))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(fail("destination import is busy; retry wipe later"));
    }
    let dest = parent.join(DEST_PROFILE);
    let browser = match fs::symlink_metadata(&dest) {
        Ok(m) if m.is_dir() && !m.file_type().is_symlink() => browser_closed(&dest)?,
        Ok(_) => return Err(fail("wipe destination is not a plain directory")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    Ok(Some(Box::new(Lease {
        _import: lock,
        _predecessor: browser,
        browser: None,
        pending: None,
    })))
}

fn open_private(path: &Path) -> std::io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other("profile lock is not a regular file"));
    }
    Ok(file)
}

fn copy_profile(src: &Path, dest: &Path, quota: &mut (u64, usize)) -> Result<()> {
    for item in fs::read_dir(src)? {
        let item = item?;
        let name = item.file_name();
        if [".parentlock", "parent.lock", "lock"]
            .iter()
            .any(|n| name == *n)
        {
            continue;
        }
        quota.1 += 1;
        let m = fs::symlink_metadata(item.path())?;
        if quota.1 > 20_000 {
            return Err(fail("destination profile exceeds the staging file quota"));
        }
        let target = dest.join(name);
        if m.is_dir() {
            fs::DirBuilder::new().mode(0o700).create(&target)?;
            copy_profile(&item.path(), &target, quota)?;
        } else if m.is_file() && !m.file_type().is_symlink() {
            quota.0 = quota.0.saturating_add(m.len());
            if quota.0 > MAX_PROFILE_BYTES {
                return Err(fail("destination profile exceeds the staging byte quota"));
            }
            fs::copy(item.path(), &target)?;
            fs::set_permissions(&target, fs::Permissions::from_mode(0o600))?;
        } else {
            return Err(fail(
                "destination profile contains a symlink or special file",
            ));
        }
    }
    Ok(())
}

fn sync_tree_and_note_dirs(
    root: &Path,
    current: &Path,
    dest: &Path,
    record: &mut ImportRecord,
) -> Result<()> {
    let target = dest.join(current.strip_prefix(root).expect("stage descendant"));
    if !target.exists() {
        record.created_dirs.push(target);
    }
    for item in fs::read_dir(current)? {
        let item = item?;
        if item.file_type()?.is_dir() {
            sync_tree_and_note_dirs(root, &item.path(), dest, record)?;
        } else {
            fs::set_permissions(item.path(), fs::Permissions::from_mode(0o600))?;
            File::open(item.path())?.sync_all()?;
        }
    }
    File::open(current)?.sync_all()?;
    Ok(())
}

/// Validate only the private staged copy, never a source browser database.
fn store_version(path: &Path, supported: &[i64]) -> Result<Option<i64>> {
    if !path.exists() {
        return Ok(None);
    }
    let db =
        rusqlite::Connection::open(path).map_err(|_| fail("corrupt destination SQLite store"))?;
    let check: String = db
        .query_row("PRAGMA quick_check", [], |r| r.get(0))
        .map_err(|_| fail("corrupt destination SQLite store"))?;
    let version: i64 = db
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(|_| fail("corrupt destination SQLite store"))?;
    if check != "ok" || (!supported.is_empty() && !supported.contains(&version)) {
        return Err(fail("corrupt or unsupported destination SQLite schema"));
    }
    Ok(Some(version))
}

fn rename(from: &Path, to: &Path, flags: u32) -> std::io::Result<()> {
    let from = CString::new(from.as_os_str().as_bytes())?;
    let to = CString::new(to.as_os_str().as_bytes())?;
    // Both paths are beneath the checked private parent on the same filesystem.
    if unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            flags,
        )
    } == 0
    {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[derive(Default)]
struct Normalized {
    tabs: Vec<String>,
    cookies: Vec<CookieItem>,
    storage: Vec<LocalStorageItem>,
    logins: Vec<LoginItem>,
}

impl Normalized {
    fn read(bundle: &mut dyn Read) -> Result<Self> {
        let mut reader = BundleReader::open_with_limit(bundle, MAX_BYTES)?;
        if reader.header().provider_id != ID {
            return Err(fail("bundle provider must be zen"));
        }
        let declared = reader.header().entries.clone();
        if declared.len() > 4 {
            return Err(fail("unknown or duplicate bundle entries"));
        }
        let tabs_only = reader.header().scope == crate::TransferScope::TabsOnly;
        let mut seen = BTreeSet::new();
        let mut out = Self::default();
        while let Some(entry) = reader.next_entry()? {
            let index = seen.len();
            if !seen.insert(entry.rel_path.clone()) {
                return Err(fail("duplicate bundle entry"));
            }
            if entry.bytes.len() as u64 != declared[index].byte_len {
                return Err(fail("entry length differs from manifest"));
            }
            if tabs_only && entry.rel_path != TABS_JSON {
                return Err(fail("tabs-only bundle contains credential state"));
            }
            let bytes = Zeroizing::new(entry.bytes);
            match entry.rel_path.as_str() {
                TABS_JSON => {
                    out.tabs = json(&bytes)?;
                    if out.tabs.len() > 4096 {
                        return Err(fail("too many tabs"));
                    }
                    for tab in &out.tabs {
                        web_url(tab)?;
                    }
                }
                COOKIES_ENTRY => {
                    out.cookies = records(
                        &bytes,
                        &[
                            "host_key",
                            "name",
                            "value",
                            "path",
                            "expires_utc",
                            "is_secure",
                            "is_httponly",
                            "samesite",
                            "creation_utc",
                            "last_access_utc",
                            "last_update_utc",
                            "priority",
                            "source_scheme",
                            "source_port",
                            "source_type",
                            "has_cross_site_ancestor",
                            "partition_key",
                        ],
                    )?;
                    let mut keys = BTreeSet::new();
                    for c in &out.cookies {
                        if c.extra.partition_key.is_some()
                            || c.extra.has_cross_site_ancestor.unwrap_or(0) != 0
                        {
                            return Err(fail("partition/container cookies are unsupported; send default unpartitioned items only"));
                        }
                        let host = c.host_key.strip_prefix('.').unwrap_or(&c.host_key);
                        if host.is_empty()
                            || host.contains("..")
                            || !host
                                .bytes()
                                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
                        {
                            return Err(fail("invalid cookie host"));
                        }
                        let u = web_url(&format!("http://{host}/"))?;
                        if u.host_str() != Some(host) {
                            return Err(fail("noncanonical cookie host"));
                        }
                        if !c.path.starts_with('/')
                            || c.path.contains(['\0', '\r', '\n'])
                            || c.name.contains(['\0', '\r', '\n', ';', '='])
                            || std::str::from_utf8(&c.value).is_err()
                            || c.value.iter().any(|b| *b < 0x20 || *b == 0x7f)
                        {
                            return Err(fail("invalid or non-lossless cookie record"));
                        }
                        if [
                            c.extra.creation_utc,
                            c.extra.last_access_utc,
                            c.extra.last_update_utc,
                        ]
                        .into_iter()
                        .flatten()
                        .any(|v| v < CHROME_EPOCH)
                        {
                            return Err(fail("invalid cookie timestamp"));
                        }
                        if c.expires_utc == 0 {
                            return Err(fail("session-only cookies cannot be restored losslessly yet; send persistent cookies"));
                        }
                        if c.expires_utc <= CHROME_EPOCH || !(0..=2).contains(&c.samesite) {
                            return Err(fail("invalid cookie expiry or SameSite"));
                        }
                        if !keys.insert((&c.host_key, &c.name, &c.path)) {
                            return Err(fail("duplicate cookie identity"));
                        }
                    }
                }
                LOCAL_STORAGE_ENTRY => {
                    out.storage =
                        records(&bytes, &["origin", "key", "value", "key_raw", "value_raw"])?;
                    let mut keys = BTreeSet::new();
                    for item in &out.storage {
                        origin(&item.origin)?;
                        if item.key_raw.is_some() || item.value_raw.is_some() {
                            return Err(fail(
                                "non-lossless raw localStorage encoding is unsupported",
                            ));
                        }
                        if !keys.insert((&item.origin, &item.key)) {
                            return Err(fail("duplicate localStorage identity"));
                        }
                    }
                }
                LOGINS_ENTRY => {
                    out.logins =
                        records(&bytes, &["origin", "username", "password", "signon_realm"])?;
                    let mut keys = BTreeSet::new();
                    for item in &out.logins {
                        origin(&item.origin)?;
                        if item.signon_realm
                            != cua_teleport_bundle::logins::signon_realm(&item.origin)
                            || std::str::from_utf8(&item.password).is_err()
                        {
                            return Err(fail("unsupported login realm or password encoding"));
                        }
                        if !keys.insert((&item.origin, &item.username, &item.signon_realm)) {
                            return Err(fail("duplicate login identity"));
                        }
                    }
                }
                _ => {
                    return Err(fail(
                        "unknown bundle entry; raw profiles and source keys are forbidden",
                    ))
                }
            }
        }
        if seen.len() != declared.len() {
            return Err(fail("bundle is missing declared entries"));
        }
        Ok(out)
    }
}

fn web_url(text: &str) -> Result<url::Url> {
    let u = url::Url::parse(text).map_err(|_| fail("invalid HTTP(S) URL"))?;
    if text.len() > 16_384
        || text.chars().any(char::is_control)
        || !matches!(u.scheme(), "http" | "https")
        || u.host_str().is_none()
        || !u.username().is_empty()
        || u.password().is_some()
    {
        return Err(fail("only credential-free HTTP(S) URLs are supported"));
    }
    Ok(u)
}
fn origin(text: &str) -> Result<()> {
    let u = web_url(text)?;
    if u.origin().ascii_serialization() != text || text.contains(['^', '%', '\\']) {
        return Err(fail("only canonical default HTTP(S) origins are supported; containers/partitions are refused"));
    }
    Ok(())
}
fn json<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes).map_err(|_| fail("corrupt normalized JSON"))
}
fn records<T: serde::de::DeserializeOwned>(bytes: &[u8], fields: &[&str]) -> Result<Vec<T>> {
    let value: StrictValue = json(bytes)?;
    let values = value
        .0
        .as_array()
        .ok_or_else(|| fail("normalized records must be an array"))?;
    if values.len() > 100_000 {
        return Err(fail("too many normalized records"));
    }
    for item in values {
        let object = item
            .as_object()
            .ok_or_else(|| fail("normalized record must be an object"))?;
        if object.keys().any(|key| !fields.contains(&key.as_str())) {
            return Err(fail(
                "unknown normalized record fields (container/partition identity is not supported)",
            ));
        }
    }
    serde_json::from_value(value.0).map_err(|_| fail("corrupt normalized record"))
}

// Value normally silently replaces duplicate JSON keys. Refuse them before
// deserializing shared item types (CookieExtra uses serde flatten).
struct StrictValue(Value);
impl<'de> Deserialize<'de> for StrictValue {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = StrictValue;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("normalized JSON without duplicate fields")
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut a: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut map = serde_json::Map::new();
                while let Some((k, v)) = a.next_entry::<String, StrictValue>()? {
                    if map.insert(k, v.0).is_some() {
                        return Err(de::Error::custom("duplicate field"));
                    }
                }
                Ok(StrictValue(Value::Object(map)))
            }
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut a: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut out = Vec::new();
                while let Some(v) = a.next_element::<StrictValue>()? {
                    out.push(v.0);
                }
                Ok(StrictValue(Value::Array(out)))
            }
            fn visit_str<E: de::Error>(self, s: &str) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(Value::String(s.into())))
            }
            fn visit_string<E: de::Error>(self, s: String) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(Value::String(s)))
            }
            fn visit_bool<E: de::Error>(self, b: bool) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(Value::Bool(b)))
            }
            fn visit_i64<E: de::Error>(self, n: i64) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(Value::from(n)))
            }
            fn visit_u64<E: de::Error>(self, n: u64) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(Value::from(n)))
            }
            fn visit_f64<E: de::Error>(self, n: f64) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(Value::from(n)))
            }
            fn visit_unit<E: de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(Value::Null))
            }
        }
        d.deserialize_any(V)
    }
}

mod saved_logins;

#[cfg(test)]
mod tests;
