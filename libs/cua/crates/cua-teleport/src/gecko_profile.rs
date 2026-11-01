// SPDX-License-Identifier: FSL-1.1-MIT
// Copyright (c) 2026 Cua AI, Inc.

//! Constrained Zen source discovery. No Firefox fallback or ambient root override.
//! Paths are descriptive; content readers must use the retained directory handles.
//! All content reads require a closed profile (including refusal of stale locks).
//! This is not a security boundary against another process with this user's UID.

use crate::{HostEffects, Platform, Result, TeleportError};
use std::path::{Path, PathBuf};

pub(crate) fn fail(code: &str, reason: impl std::fmt::Display) -> TeleportError {
    TeleportError::Provider(format!("{code}: {reason}"))
}

/// One Zen profile, constrained to its browser root and pinned to directory FDs.
#[derive(Clone, Debug)]
pub struct ResolvedGeckoProfile {
    pub provider_id: &'static str,
    pub root: PathBuf,
    /// Stable path relative to `root`, not an arbitrary INI display name.
    pub profile_id: String,
    pub name: String,
    pub dir: PathBuf,
    pub is_default: bool,
    #[cfg(target_os = "linux")]
    pub(crate) root_fd: linux::Dir,
    #[cfg(target_os = "linux")]
    pub(crate) dir_fd: linux::Dir,
}

/// Enumerate only the two documented Linux Zen roots through the supplied host.
/// Inventory itself may enumerate a locked profile; reading its content refuses.
pub fn profiles(host: &dyn HostEffects, platform: Platform) -> Result<Vec<ResolvedGeckoProfile>> {
    #[cfg(target_os = "linux")]
    {
        linux::discover(host, platform)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (host, platform);
        Err(fail(
            "zen_platform_unsupported",
            "Zen export requires Linux",
        ))
    }
}

/// Select by profile name, root-relative path or a contained absolute path.
/// Multiple roots/defaults/installations never select the first entry implicitly.
pub fn resolve_profile(
    host: &dyn HostEffects,
    platform: Platform,
    selector: Option<&Path>,
) -> Result<ResolvedGeckoProfile> {
    #[cfg(target_os = "linux")]
    {
        linux::resolve(host, platform, selector)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (host, platform, selector);
        Err(fail(
            "zen_platform_unsupported",
            "Zen export requires Linux",
        ))
    }
}

#[cfg(target_os = "linux")]
pub(crate) mod linux {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};
    use std::ffi::{CStr, CString, OsString};
    use std::fs::{File, Metadata};
    use std::io::{self, Read};
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::os::unix::fs::MetadataExt;
    use std::path::Component;
    use std::sync::Arc;

    use crate::layout::zen::LINUX_SOURCE_ROOTS as ROOTS;
    pub(crate) const MAX_FILE: u64 = 256 * 1024 * 1024;

    #[derive(Clone, Debug)]
    pub(crate) struct Dir(Arc<File>);

    pub(crate) fn unsafe_path() -> TeleportError {
        fail(
            "zen_profile_unsafe",
            "profile path must remain beneath its Zen root, without symlinks",
        )
    }
    fn io_error(e: io::Error) -> TeleportError {
        if matches!(e.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)) {
            unsafe_path()
        } else {
            fail("zen_source_io", e)
        }
    }
    fn cpath(path: &Path) -> Result<CString> {
        CString::new(path.as_os_str().as_bytes()).map_err(|_| unsafe_path())
    }
    fn parts(path: &Path) -> Result<Vec<OsString>> {
        let mut out = Vec::new();
        for part in path.components() {
            match part {
                Component::Normal(p) => out.push(p.to_owned()),
                _ => return Err(unsafe_path()),
            }
        }
        if out.is_empty() {
            return Err(unsafe_path());
        }
        Ok(out)
    }
    impl Dir {
        fn from_file(file: File) -> Self {
            Self(Arc::new(file))
        }
        /// Every component is opened relative to the previously retained FD.
        pub(crate) fn absolute(path: &Path) -> Result<Option<Self>> {
            if !path.is_absolute() {
                return Err(unsafe_path());
            }
            let mut current = Self::from_file(File::open("/").map_err(io_error)?);
            for part in path.components().skip(1) {
                let Component::Normal(name) = part else {
                    return Err(unsafe_path());
                };
                let Some(next) = current.child(Path::new(name))? else {
                    return Ok(None);
                };
                current = next;
            }
            Ok(Some(current))
        }
        fn open_one(&self, path: &Path, directory: bool) -> Result<Option<File>> {
            let path = cpath(path)?;
            let flags = libc::O_RDONLY
                | libc::O_CLOEXEC
                | libc::O_NOFOLLOW
                | libc::O_NONBLOCK
                | if directory { libc::O_DIRECTORY } else { 0 };
            // SAFETY: valid owned FD and NUL-terminated name; returned FD is owned.
            let fd = unsafe { libc::openat(self.0.as_raw_fd(), path.as_ptr(), flags) };
            if fd < 0 {
                let e = io::Error::last_os_error();
                return if e.kind() == io::ErrorKind::NotFound {
                    Ok(None)
                } else {
                    Err(io_error(e))
                };
            }
            // SAFETY: openat returned a fresh owned descriptor.
            let f = unsafe { File::from_raw_fd(fd) };
            if !directory && !f.metadata().map_err(io_error)?.is_file() {
                return Err(unsafe_path());
            }
            Ok(Some(f))
        }
        pub(crate) fn child(&self, rel: &Path) -> Result<Option<Self>> {
            let mut current = self.clone();
            for name in parts(rel)? {
                let Some(f) = current.open_one(Path::new(&name), true)? else {
                    return Ok(None);
                };
                current = Self::from_file(f);
            }
            Ok(Some(current))
        }
        pub(crate) fn open(&self, rel: &Path) -> Result<Option<File>> {
            let names = parts(rel)?;
            let mut current = self.clone();
            for name in &names[..names.len() - 1] {
                let Some(next) = current.child(Path::new(name))? else {
                    return Ok(None);
                };
                current = next;
            }
            current.open_one(Path::new(names.last().unwrap()), false)
        }
        pub(crate) fn same(&self, other: &Self) -> Result<bool> {
            let a = self.0.metadata().map_err(io_error)?;
            let b = other.0.metadata().map_err(io_error)?;
            Ok((a.dev(), a.ino()) == (b.dev(), b.ino()))
        }
        /// Includes symlinks: a Gecko lock symlink is a lock, not permission to follow it.
        pub(crate) fn entry_exists(&self, name: &str) -> Result<bool> {
            let name = CString::new(name).map_err(|_| unsafe_path())?;
            let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
            // SAFETY: pointers are valid and fstatat initializes st on success.
            let r = unsafe {
                libc::fstatat(
                    self.0.as_raw_fd(),
                    name.as_ptr(),
                    st.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            };
            if r == 0 {
                return Ok(true);
            }
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::NotFound {
                Ok(false)
            } else {
                Err(io_error(e))
            }
        }
        pub(crate) fn names(&self) -> Result<Vec<OsString>> {
            let dot = CString::new(".").unwrap();
            // Open a separate file description, not dup's shared directory offset.
            let fd = unsafe {
                libc::openat(
                    self.0.as_raw_fd(),
                    dot.as_ptr(),
                    libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY,
                )
            };
            if fd < 0 {
                return Err(io_error(io::Error::last_os_error()));
            }
            let stream = unsafe { libc::fdopendir(fd) };
            if stream.is_null() {
                unsafe {
                    libc::close(fd);
                }
                return Err(io_error(io::Error::last_os_error()));
            }
            let mut names = Vec::new();
            let result = loop {
                // SAFETY: stream is exclusively owned here. readdir's borrowed name
                // is copied before the next call; errno distinguishes EOF from error.
                unsafe {
                    *libc::__errno_location() = 0;
                }
                let entry = unsafe { libc::readdir(stream) };
                if entry.is_null() {
                    let errno = io::Error::last_os_error();
                    break if errno.raw_os_error() == Some(0) {
                        Ok(())
                    } else {
                        Err(io_error(errno))
                    };
                }
                let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
                if name != b"." && name != b".." {
                    names.push(OsString::from_vec(name.to_vec()));
                }
                if names.len() > 10_000 {
                    break Err(fail("zen_format_unsupported", "too many storage origins"));
                }
            };
            unsafe {
                libc::closedir(stream);
            }
            result?;
            names.sort();
            Ok(names)
        }
        pub(crate) fn read(&self, rel: &Path, limit: u64) -> Result<Option<Vec<u8>>> {
            for _ in 0..3 {
                let Some(f) = self.open(rel)? else {
                    return Ok(None);
                };
                let before = stamp(&f.metadata().map_err(io_error)?);
                if before.2 > limit {
                    return Err(fail(
                        "zen_format_unsupported",
                        "source file exceeds size limit",
                    ));
                }
                let mut bytes = Vec::new();
                (&f).take(limit + 1)
                    .read_to_end(&mut bytes)
                    .map_err(io_error)?;
                if bytes.len() as u64 > limit {
                    return Err(fail(
                        "zen_format_unsupported",
                        "source file exceeds size limit",
                    ));
                }
                let after = stamp(&f.metadata().map_err(io_error)?);
                if before == after
                    && bytes.len() as u64 == before.2
                    && let Some(now) = self.open(rel)?
                    && stamp(&now.metadata().map_err(io_error)?) == before
                {
                    return Ok(Some(bytes));
                }
            }
            Err(fail(
                "zen_source_busy",
                "source file changed during capture; close Zen and retry",
            ))
        }
    }

    pub(crate) type Stamp = (u64, u64, u64, i64, i64, i64, i64);
    pub(crate) fn stamp(m: &Metadata) -> Stamp {
        (
            m.dev(),
            m.ino(),
            m.len(),
            m.mtime(),
            m.mtime_nsec(),
            m.ctime(),
            m.ctime_nsec(),
        )
    }

    fn ini(bytes: &[u8]) -> Result<Vec<(String, BTreeMap<String, String>)>> {
        let text = std::str::from_utf8(bytes)
            .map_err(|_| fail("zen_format_invalid", "INI is not UTF-8"))?;
        let mut out: Vec<(String, BTreeMap<String, String>)> = Vec::new();
        let mut seen = BTreeSet::new();
        for line in text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with([';', '#']))
        {
            if let Some(section) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                if !seen.insert(section.to_owned()) {
                    return Err(fail("zen_profile_ambiguous", "duplicate INI section"));
                }
                out.push((section.to_owned(), BTreeMap::new()));
            } else if let Some((key, value)) = line.split_once('=') {
                let kv = &mut out
                    .last_mut()
                    .ok_or_else(|| fail("zen_format_invalid", "INI has no section"))?
                    .1;
                if kv
                    .insert(key.trim().to_owned(), value.trim().to_owned())
                    .is_some()
                {
                    return Err(fail("zen_profile_ambiguous", "duplicate INI key"));
                }
            } else {
                return Err(fail("zen_format_invalid", "malformed INI line"));
            }
        }
        Ok(out)
    }
    fn relative(root: &Path, raw: &str, is_relative: bool) -> Result<PathBuf> {
        let path = Path::new(raw);
        // Do not normalize away a traversal or an absolute IsRelative=1 entry.
        let rel = if path.is_absolute() {
            if is_relative {
                return Err(unsafe_path());
            }
            path.strip_prefix(root)
                .map_err(|_| unsafe_path())?
                .to_path_buf()
        } else {
            path.to_path_buf()
        };
        parts(&rel)?;
        Ok(rel)
    }
    fn make(
        root: &Path,
        root_fd: &Dir,
        rel: &Path,
        name: String,
        default: bool,
    ) -> Result<ResolvedGeckoProfile> {
        let dir_fd = root_fd.child(rel)?.ok_or_else(|| {
            fail(
                "zen_profile_not_found",
                "selected profile directory is missing",
            )
        })?;
        Ok(ResolvedGeckoProfile {
            provider_id: "zen",
            root: root.to_owned(),
            profile_id: rel.to_str().ok_or_else(unsafe_path)?.to_owned(),
            name,
            dir: root.join(rel),
            is_default: default,
            root_fd: root_fd.clone(),
            dir_fd,
        })
    }
    fn roots(host: &dyn HostEffects, platform: Platform) -> Result<Vec<(PathBuf, Dir)>> {
        if platform != Platform::Linux {
            return Err(fail(
                "zen_platform_unsupported",
                "Zen export requires Linux",
            ));
        }
        let home = host
            .home_dir()
            .ok_or_else(|| fail("zen_profile_not_found", "host home is unavailable"))?;
        let mut roots = Vec::new();
        for rel in ROOTS {
            let path = home.join(rel);
            if let Some(fd) = Dir::absolute(&path)? {
                roots.push((path, fd));
            }
        }
        Ok(roots)
    }
    pub(super) fn discover(
        host: &dyn HostEffects,
        platform: Platform,
    ) -> Result<Vec<ResolvedGeckoProfile>> {
        let mut all = Vec::new();
        for (root, fd) in roots(host, platform)? {
            let mut defaults = BTreeSet::new();
            let mut candidates = Vec::new();
            if let Some(bytes) = fd.read(Path::new("profiles.ini"), 1024 * 1024)? {
                for (section, kv) in ini(&bytes)? {
                    if section.starts_with("Profile") {
                        let raw = kv
                            .get("Path")
                            .ok_or_else(|| fail("zen_format_invalid", "profile has no Path"))?;
                        let rel_flag = kv.get("IsRelative").map(String::as_str).unwrap_or("1");
                        if !matches!(rel_flag, "0" | "1") {
                            return Err(fail("zen_format_invalid", "invalid IsRelative"));
                        }
                        let rel = relative(&root, raw, rel_flag == "1")?;
                        if kv.get("Default").is_some_and(|v| v == "1") {
                            defaults.insert(rel.clone());
                        }
                        candidates.push((rel, kv.get("Name").cloned().unwrap_or_default()));
                    } else if section.starts_with("Install")
                        && let Some(raw) = kv.get("Default")
                    {
                        defaults.insert(relative(&root, raw, !Path::new(raw).is_absolute())?);
                    }
                }
            }
            if let Some(bytes) = fd.read(Path::new("installs.ini"), 1024 * 1024)? {
                for (_, kv) in ini(&bytes)? {
                    if let Some(raw) = kv.get("Default") {
                        defaults.insert(relative(&root, raw, !Path::new(raw).is_absolute())?);
                    }
                }
            }
            if defaults
                .iter()
                .any(|d| !candidates.iter().any(|(p, _)| p == d))
            {
                return Err(fail(
                    "zen_profile_unsafe",
                    "installation default is not a registered Zen profile",
                ));
            }
            let mut unique = BTreeSet::new();
            for (rel, name) in candidates {
                if !unique.insert(rel.clone()) {
                    return Err(fail("zen_profile_ambiguous", "duplicate profile path"));
                }
                all.push(make(&root, &fd, &rel, name, defaults.contains(&rel))?);
            }
        }
        Ok(all)
    }
    pub(super) fn resolve(
        host: &dyn HostEffects,
        platform: Platform,
        selector: Option<&Path>,
    ) -> Result<ResolvedGeckoProfile> {
        let all = discover(host, platform)?;
        if let Some(selector) = selector {
            if selector.is_absolute() {
                for (root, fd) in roots(host, platform)? {
                    if selector.starts_with(&root) {
                        let rel =
                            relative(&root, selector.to_str().ok_or_else(unsafe_path)?, false)?;
                        if let Some(p) = all.iter().find(|p| p.dir == selector) {
                            return Ok(p.clone());
                        }
                        return make(&root, &fd, &rel, rel.to_string_lossy().into_owned(), false);
                    }
                }
                return Err(unsafe_path());
            }
            parts(selector)?;
            let selected: Vec<_> = all
                .into_iter()
                .filter(|p| Path::new(&p.profile_id) == selector || Path::new(&p.name) == selector)
                .collect();
            return one(selected);
        }
        if all.len() == 1 {
            return Ok(all.into_iter().next().unwrap());
        }
        // A second installed root is an ambiguity even if one root has no default.
        let roots: BTreeSet<_> = all.iter().map(|p| &p.root).collect();
        if roots.len() > 1 {
            return Err(fail(
                "zen_profile_ambiguous",
                "multiple Zen installations; select a profile explicitly",
            ));
        }
        if all.len() > 1 && !all.iter().any(|p| p.is_default) {
            return Err(fail(
                "zen_profile_ambiguous",
                "multiple profiles without a default; select a profile explicitly",
            ));
        }
        one(all.into_iter().filter(|p| p.is_default).collect())
    }
    fn one(mut selected: Vec<ResolvedGeckoProfile>) -> Result<ResolvedGeckoProfile> {
        match selected.len() {
            0 => Err(fail(
                "zen_profile_not_found",
                "no uniquely selected Zen profile; choose a profile explicitly",
            )),
            1 => Ok(selected.remove(0)),
            _ => Err(fail(
                "zen_profile_ambiguous",
                "multiple Zen profiles/defaults; select a profile explicitly",
            )),
        }
    }
}

#[cfg(target_os = "linux")]
impl ResolvedGeckoProfile {
    /// Revalidate the pinned root/profile before and after every source operation.
    pub(crate) fn validate(&self) -> Result<()> {
        let root = linux::Dir::absolute(&self.root)?.ok_or_else(linux::unsafe_path)?;
        if !root.same(&self.root_fd)? {
            return Err(linux::unsafe_path());
        }
        let dir = root
            .child(Path::new(&self.profile_id))?
            .ok_or_else(linux::unsafe_path)?;
        if !dir.same(&self.dir_fd)? {
            return Err(linux::unsafe_path());
        }
        Ok(())
    }
    pub fn ensure_closed(&self) -> Result<()> {
        self.validate()?;
        let locked = || {
            fail(
                "zen_profile_locked",
                "close Zen and retry; lock artifacts are never removed automatically",
            )
        };
        for lock in ["parent.lock", "lock"] {
            if self.dir_fd.entry_exists(lock)? {
                return Err(locked());
            }
        }
        // Gecko intentionally leaves this empty regular file after clean exit.
        // Its existence alone does NOT mean the browser is running. Query the
        // real POSIX lock without creating, opening SQLite, writing, or acquiring
        // a source lock. Symlinks (including stale ones) still refuse.
        if self.dir_fd.entry_exists(".parentlock")? {
            use std::os::fd::AsRawFd;
            let file = self
                .dir_fd
                .open(Path::new(".parentlock"))
                .map_err(|_| locked())?
                .ok_or_else(locked)?;
            if file.metadata().map_err(|_| locked())?.len() != 0 {
                return Err(locked());
            }
            let mut query: libc::flock = unsafe { std::mem::zeroed() };
            query.l_type = libc::F_WRLCK as _;
            query.l_whence = libc::SEEK_SET as _;
            if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETLK, &mut query) } == -1
                || query.l_type != libc::F_UNLCK as libc::c_short
            {
                return Err(locked());
            }
        }
        Ok(())
    }
    pub(crate) fn read(&self, rel: &Path, limit: u64) -> Result<Option<Vec<u8>>> {
        self.ensure_closed()?;
        let bytes = self.dir_fd.read(rel, limit)?;
        self.ensure_closed()?;
        Ok(bytes)
    }
}

#[cfg(not(target_os = "linux"))]
impl ResolvedGeckoProfile {
    pub fn ensure_closed(&self) -> Result<()> {
        Err(fail(
            "zen_platform_unsupported",
            "Zen export requires Linux",
        ))
    }
}

#[cfg(all(test, target_os = "linux"))]
#[path = "gecko_profile_tests.rs"]
mod tests;
