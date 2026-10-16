//! Accessibility trees of Flatpak apps.
//!
//! A Flatpak app reaches the accessibility bus through its sandbox's own
//! `xdg-dbus-proxy`, so the AT-SPI registry reports the proxy's pid for the
//! app, never the app's own. Matching registry entries by exact pid therefore
//! finds no tree for any Flatpak app (Zen, Slack, Signal, ...), and every read
//! falls back to window metadata and screenshots.
//!
//! This module proves, from file identities alone, that a registry peer is the
//! proxy serving one target process's sandbox: the socket the target sees at
//! Flatpak's fixed accessibility-bus path inside its own root is the same file
//! (device and inode) as a socket the peer is listening on. Every client in
//! that sandbox connects to the bus through that listener, so an application
//! the peer registered belongs to the sandbox. No name, title or command line
//! is trusted. Paths are walked without following symlinks (a sandboxed app
//! can create symlinks inside its own root, and an absolute one would resolve
//! in the driver's root), and both processes' start times bracket the reads
//! so a reused pid cannot pass.

use std::collections::HashSet;
use std::ffi::{CString, OsStr};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path};

/// Where Flatpak mounts the accessibility-bus proxy socket in every sandbox,
/// relative to the sandbox root (`AT_SPI_BUS_ADDRESS=unix:path=/run/flatpak/at-spi-bus`).
const FLATPAK_A11Y_BUS: &str = "run/flatpak/at-spi-bus";

/// A filesystem object's identity: `(st_dev, st_ino)`.
pub(crate) type FileKey = (u64, u64);

/// What a registry peer must match to stand in for one target process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SandboxPeerProof {
    target_pid: u32,
    target_instance: u64,
    socket: FileKey,
}

impl SandboxPeerProof {
    /// The accessibility-bus socket `pid` sees inside its Flatpak sandbox, or
    /// `None` when `pid` is not in one (or its root cannot be read).
    pub(crate) fn for_target(pid: u32) -> Option<Self> {
        let before = crate::proc_fs::process_instance_id(pid)?;
        let root = format!("/proc/{pid}/root");
        let socket = socket_key_beneath(Path::new(&root), Path::new(FLATPAK_A11Y_BUS))?;
        (crate::proc_fs::process_instance_id(pid)? == before).then_some(Self {
            target_pid: pid,
            target_instance: before,
            socket,
        })
    }

    /// Whether registry peer `candidate` listens on the target sandbox's
    /// accessibility-bus socket. The target's own pid is not a proxy and is
    /// matched by the caller, so it never passes here.
    pub(crate) fn admits(&self, candidate: u32) -> bool {
        if candidate == self.target_pid {
            return false;
        }
        let Some(before) = crate::proc_fs::process_instance_id(candidate) else {
            return false;
        };
        listens_on(candidate, self.socket)
            && crate::proc_fs::process_instance_id(candidate) == Some(before)
            && crate::proc_fs::process_instance_id(self.target_pid) == Some(self.target_instance)
    }

    /// Whether the target owns every window that belongs to its sandbox,
    /// given the owner of every window on the desktop. Any process in the
    /// sandbox can register through the shared proxy, so a registration can be
    /// attributed to the target only when no other process there has a
    /// window. Fails closed: the target must own a listed window (an empty or
    /// failed listing proves nothing), a window with an unknown owner could be
    /// anyone's, and a process whose root cannot be read cannot be ruled out.
    pub(crate) fn owns_its_sandbox_windows(
        &self,
        owners: impl IntoIterator<Item = Option<u32>>,
    ) -> bool {
        let mut owns_a_window = false;
        let mut others = HashSet::new();
        for owner in owners {
            match owner {
                None => return false,
                Some(pid) if pid == self.target_pid => owns_a_window = true,
                Some(pid) => {
                    others.insert(pid);
                }
            }
        }
        owns_a_window
            && others.into_iter().all(|pid| {
                let root = format!("/proc/{pid}/root");
                self.outside_sandbox(socket_key_state(
                    Path::new(&root),
                    Path::new(FLATPAK_A11Y_BUS),
                ))
            })
    }

    /// Whether a process whose accessibility-bus socket lookup gave `bus` is
    /// provably outside the target's sandbox.
    fn outside_sandbox(&self, bus: std::io::Result<Option<FileKey>>) -> bool {
        matches!(bus, Ok(key) if key != Some(self.socket))
    }
}

/// Whether a walk confined to sandbox `proxy` may address an object on bus
/// `name`, whose connection's owner pid is `owner`. Only a unique name (a
/// well-known name can move to another connection between a check and its
/// use) owned by the proxy itself passes; an unknown owner never does.
pub(crate) fn addressable(name: &str, owner: Option<u32>, proxy: u32) -> bool {
    name.starts_with(':') && owner == Some(proxy)
}

/// Whether process `pid` holds a listening Unix socket bound at a path that,
/// resolved inside that process's own root, is the file `socket`.
fn listens_on(pid: u32, socket: FileKey) -> bool {
    let inodes = socket_inodes(pid);
    if inodes.is_empty() {
        return false;
    }
    let Ok(table) = std::fs::read_to_string(format!("/proc/{pid}/net/unix")) else {
        return false;
    };
    let root = format!("/proc/{pid}/root");
    listening_paths(&table, &inodes)
        .into_iter()
        .any(|path| socket_key_beneath(Path::new(&root), Path::new(path)) == Some(socket))
}

/// Socket inodes of `pid`'s open descriptors (`socket:[N]` links in
/// `/proc/<pid>/fd`).
fn socket_inodes(pid: u32) -> HashSet<u64> {
    let Ok(entries) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
        return HashSet::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| std::fs::read_link(entry.path()).ok())
        .filter_map(|link| {
            link.to_str()?
                .strip_prefix("socket:[")?
                .strip_suffix(']')?
                .parse()
                .ok()
        })
        .collect()
}

/// Filesystem paths of the listening sockets in a `/proc/<pid>/net/unix`
/// table whose socket inode is one of `inodes`. Abstract and unnamed sockets
/// have no file to compare and are skipped.
fn listening_paths<'t>(table: &'t str, inodes: &HashSet<u64>) -> Vec<&'t str> {
    /// `__SO_ACCEPTCON` in the table's `Flags` column: the socket listens.
    const SO_ACCEPTCON: u32 = 0x0001_0000;
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            // Num RefCount Protocol Flags Type St Inode [Path]; the inode is
            // space-padded, so split fields by runs of whitespace and keep the
            // path as the untouched rest of the line.
            let mut rest = line;
            let mut fields = [""; 7];
            for field in &mut fields {
                rest = rest.trim_start();
                let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
                *field = &rest[..end];
                rest = &rest[end..];
            }
            let flags = u32::from_str_radix(fields[3], 16).ok()?;
            let inode: u64 = fields[6].parse().ok()?;
            let path = rest.trim_start();
            (flags & SO_ACCEPTCON != 0 && inodes.contains(&inode) && path.starts_with('/'))
                .then_some(path)
        })
        .collect()
}

/// The identity of the socket at `relative` beneath `root`, walking every
/// component without following symlinks. `root` itself is followed once, as
/// `/proc/<pid>/root` is a link to the process's root directory. `None` for
/// anything but a socket, any symlink on the way, a non-normal component, or
/// a root that cannot be read.
fn socket_key_beneath(root: &Path, relative: &Path) -> Option<FileKey> {
    socket_key_state(root, relative).ok().flatten()
}

/// [`socket_key_beneath`], telling "nothing there" (`Ok(None)`: a missing
/// component, a symlink or something other than a socket) apart from "could
/// not look" (`Err`: the root or a directory on the way could not be read).
fn socket_key_state(root: &Path, relative: &Path) -> std::io::Result<Option<FileKey>> {
    let mut names = Vec::new();
    for component in relative.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => names.push(name),
            Component::CurDir | Component::ParentDir | Component::Prefix(_) => return Ok(None),
        }
    }
    let Some((last, dirs)) = names.split_last() else {
        return Ok(None);
    };
    let absent = |error: std::io::Error| match error.raw_os_error() {
        Some(libc::ENOENT | libc::ENOTDIR | libc::ELOOP) => Ok(None),
        _ => Err(error),
    };
    let mut dir = open_path(None, root.as_os_str(), libc::O_DIRECTORY)?;
    for name in dirs {
        dir = match open_path(
            Some(dir.as_raw_fd()),
            name,
            libc::O_DIRECTORY | libc::O_NOFOLLOW,
        ) {
            Ok(next) => next,
            Err(error) => return absent(error),
        };
    }
    let stat = match stat_nofollow(dir.as_raw_fd(), last) {
        Ok(stat) => stat,
        Err(error) => return absent(error),
    };
    Ok((stat.st_mode & libc::S_IFMT == libc::S_IFSOCK)
        .then_some((stat.st_dev as u64, stat.st_ino as u64)))
}

fn open_path(dir: Option<RawFd>, name: &OsStr, flags: libc::c_int) -> std::io::Result<OwnedFd> {
    let name = CString::new(name.as_bytes())
        .map_err(|_| std::io::Error::from_raw_os_error(libc::EINVAL))?;
    // SAFETY: `name` is a valid NUL-terminated string and `dir`, when given,
    // is an open descriptor owned by the caller for the duration of the call.
    let fd = unsafe {
        libc::openat(
            dir.unwrap_or(libc::AT_FDCWD),
            name.as_ptr(),
            libc::O_PATH | libc::O_CLOEXEC | flags,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: a non-negative return is a fresh descriptor this call owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn stat_nofollow(dir: RawFd, name: &OsStr) -> std::io::Result<libc::stat> {
    let name = CString::new(name.as_bytes())
        .map_err(|_| std::io::Error::from_raw_os_error(libc::EINVAL))?;
    // SAFETY: zeroed `stat` is a valid out-parameter; `dir` is open.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::fstatat(dir, name.as_ptr(), &mut stat, libc::AT_SYMLINK_NOFOLLOW) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(stat)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A short private scratch directory (socket paths are length-limited).
    fn scratch() -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "sp{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A listening socket (the proxy's) and a sandbox root whose Flatpak bus
    /// path is a hard link to it, as Flatpak's bind mount makes it.
    struct Fixture {
        dir: PathBuf,
        host: PathBuf,
        sandbox: PathBuf,
        _listener: UnixListener,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = scratch();
            let host = dir.join("a11y");
            let listener = UnixListener::bind(&host).unwrap();
            let sandbox = dir.join("root");
            std::fs::create_dir_all(sandbox.join("run/flatpak")).unwrap();
            std::fs::hard_link(&host, sandbox.join(FLATPAK_A11Y_BUS)).unwrap();
            Self {
                dir,
                host,
                sandbox,
                _listener: listener,
            }
        }

        fn host_key(&self) -> FileKey {
            let meta = std::fs::symlink_metadata(&self.host).unwrap();
            (meta.dev(), meta.ino())
        }

        /// A proof for the test's parent process (a live process that is not
        /// this one) bound to the fixture's sandbox socket.
        fn proof(&self) -> SandboxPeerProof {
            let target_pid = std::os::unix::process::parent_id();
            SandboxPeerProof {
                target_pid,
                target_instance: crate::proc_fs::process_instance_id(target_pid).unwrap(),
                socket: socket_key_beneath(&self.sandbox, Path::new(FLATPAK_A11Y_BUS)).unwrap(),
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn sandbox_bus_socket_is_the_listeners_file() {
        let fixture = Fixture::new();
        assert_eq!(
            socket_key_beneath(&fixture.sandbox, Path::new(FLATPAK_A11Y_BUS)),
            Some(fixture.host_key())
        );
        assert!(listens_on(std::process::id(), fixture.host_key()));
    }

    #[test]
    fn listening_peer_on_the_sandbox_socket_is_admitted() {
        let fixture = Fixture::new();
        assert!(fixture.proof().admits(std::process::id()));
    }

    #[test]
    fn the_target_itself_is_never_admitted_as_its_proxy() {
        let fixture = Fixture::new();
        let mut proof = fixture.proof();
        proof.target_pid = std::process::id();
        proof.target_instance = crate::proc_fs::process_instance_id(proof.target_pid).unwrap();
        assert!(!proof.admits(std::process::id()));
    }

    #[test]
    fn a_target_whose_pid_was_reused_admits_nothing() {
        let fixture = Fixture::new();
        let mut proof = fixture.proof();
        proof.target_instance += 1;
        assert!(!proof.admits(std::process::id()));
    }

    #[test]
    fn a_peer_not_listening_on_the_socket_is_refused() {
        let fixture = Fixture::new();
        let other = Fixture::new();
        let mut proof = fixture.proof();
        proof.socket = other.host_key();
        drop(other);
        assert!(!proof.admits(std::process::id()));
    }

    #[test]
    fn a_closed_listener_no_longer_proves_anything() {
        let dir = scratch();
        let host = dir.join("gone");
        drop(UnixListener::bind(&host).unwrap());
        let meta = std::fs::symlink_metadata(&host).unwrap();
        assert!(!listens_on(std::process::id(), (meta.dev(), meta.ino())));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_symlinked_bus_socket_is_refused() {
        let fixture = Fixture::new();
        let bus = fixture.sandbox.join(FLATPAK_A11Y_BUS);
        std::fs::remove_file(&bus).unwrap();
        std::os::unix::fs::symlink(&fixture.host, &bus).unwrap();
        assert_eq!(
            socket_key_beneath(&fixture.sandbox, Path::new(FLATPAK_A11Y_BUS)),
            None
        );
    }

    #[test]
    fn a_symlinked_directory_on_the_way_is_refused() {
        let fixture = Fixture::new();
        let elsewhere = fixture.dir.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::hard_link(&fixture.host, elsewhere.join("at-spi-bus")).unwrap();
        let flatpak = fixture.sandbox.join("run/flatpak");
        std::fs::remove_dir_all(&flatpak).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &flatpak).unwrap();
        assert_eq!(
            socket_key_beneath(&fixture.sandbox, Path::new(FLATPAK_A11Y_BUS)),
            None
        );
    }

    #[test]
    fn only_sockets_and_normal_components_resolve() {
        let fixture = Fixture::new();
        let bus = fixture.sandbox.join(FLATPAK_A11Y_BUS);
        std::fs::remove_file(&bus).unwrap();
        std::fs::write(&bus, b"not a socket").unwrap();
        assert_eq!(
            socket_key_beneath(&fixture.sandbox, Path::new(FLATPAK_A11Y_BUS)),
            None
        );
        assert_eq!(
            socket_key_beneath(
                &fixture.dir,
                Path::new("root/run/../run/flatpak/at-spi-bus")
            ),
            None
        );
        assert_eq!(socket_key_beneath(&fixture.dir, Path::new("")), None);
    }

    #[test]
    fn application_selection_takes_the_proven_proxys_registration() {
        let fixture = Fixture::new();
        let proof = fixture.proof();
        let proxy = std::process::id();
        // u32::MAX is above any kernel pid_max, so it is never a live process.
        let candidates = [
            (Some(u32::MAX), "someone-else", true),
            (None, "unknown-peer", true),
            (Some(proxy), "sandboxed-app", true),
        ];
        let select = |sandbox| {
            let mut selection =
                super::super::ApplicationSelection::new(proof.target_pid).with_sandbox(sandbox);
            for (pid, app, has_children) in candidates {
                if selection.matches_pid(pid) {
                    selection.consider_matching(app, has_children);
                }
            }
            selection.into_selected()
        };
        assert_eq!(select(Some(proof.clone())), Ok(Some("sandboxed-app")));
        assert_eq!(select(None), Ok(None));
    }

    #[test]
    fn a_proxy_registration_must_be_the_only_registration() {
        let fixture = Fixture::new();
        let proof = fixture.proof();
        let proxy = Some(std::process::id());
        let select = |registrations: &[(Option<u32>, &'static str, bool)]| {
            let mut selection = super::super::ApplicationSelection::new(proof.target_pid)
                .with_sandbox(Some(proof.clone()));
            for (pid, app, has_children) in registrations {
                // A registration counts even when its application never answers.
                if selection.admit(*pid).is_some() && !app.is_empty() {
                    selection.consider_matching(*app, *has_children);
                }
            }
            selection.into_selected()
        };
        // One registration through the proxy, populated or not, is the target's.
        assert_eq!(select(&[(proxy, "app", true)]), Ok(Some("app")));
        assert_eq!(select(&[(proxy, "app", false)]), Ok(Some("app")));
        // Two processes in one sandbox: never pick one, even when the other's
        // registration is empty or did not answer.
        assert_eq!(select(&[(proxy, "a", false), (proxy, "b", true)]), Err(2));
        assert_eq!(select(&[(proxy, "", false), (proxy, "b", true)]), Err(2));
        // The exact pid's duplicate-registration fallback is unchanged.
        let mut exact = super::super::ApplicationSelection::new(proof.target_pid);
        for (app, has_children) in [("empty", false), ("live", true)] {
            assert!(exact.admit(Some(proof.target_pid)).is_some());
            exact.consider_matching(app, has_children);
        }
        assert_eq!(exact.into_selected(), Ok(Some("live")));
    }

    #[test]
    fn the_target_must_own_every_window_in_its_sandbox() {
        let fixture = Fixture::new();
        let proof = fixture.proof();
        let target = proof.target_pid;
        // No other window owner, or only owners provably outside the sandbox
        // (this test process has no Flatpak bus socket in its root).
        assert!(proof.owns_its_sandbox_windows([Some(target)]));
        assert!(proof.owns_its_sandbox_windows([Some(target), Some(std::process::id())]));
        // A listing that does not show the target, or a window with an
        // unknown owner, proves nothing.
        assert!(!proof.owns_its_sandbox_windows([]));
        assert!(!proof.owns_its_sandbox_windows([Some(std::process::id())]));
        assert!(!proof.owns_its_sandbox_windows([Some(target), None]));
        // Another process seeing the same bus socket is in the sandbox; one
        // whose root could not be read cannot be ruled out.
        assert!(!proof.outside_sandbox(Ok(Some(proof.socket))));
        assert!(!proof.outside_sandbox(Err(std::io::Error::from_raw_os_error(libc::EACCES))));
        assert!(proof.outside_sandbox(Ok(None)));
        assert!(proof.outside_sandbox(Ok(Some((proof.socket.0, proof.socket.1 + 1)))));
    }

    #[test]
    fn only_unique_names_on_the_proxys_connection_are_addressable() {
        assert!(addressable(":1.42", Some(7), 7));
        assert!(!addressable(":1.43", Some(8), 7));
        assert!(!addressable(":1.44", None, 7));
        // A well-known name is refused even while the proxy owns it.
        assert!(!addressable("org.a11y.atspi.Registry", Some(7), 7));
        assert!(!addressable("org.webkit.WebProcess.abc", Some(7), 7));
        assert!(!addressable("", Some(7), 7));
    }

    #[test]
    fn an_unreadable_root_is_told_apart_from_a_missing_socket() {
        let fixture = Fixture::new();
        assert!(socket_key_state(
            &fixture.dir.join("no-such-root"),
            Path::new(FLATPAK_A11Y_BUS)
        )
        .is_err());
        assert!(matches!(
            socket_key_state(&fixture.dir, Path::new(FLATPAK_A11Y_BUS)),
            Ok(None)
        ));
    }

    #[test]
    fn only_listening_named_sockets_of_the_process_are_paths() {
        let table = "\
Num       RefCount Protocol Flags    Type St Inode Path
0000000000000000: 00000002 00000000 00010000 0001 01  4242 /run/user/1000/.dbus-proxy/a11y-bus-proxy-ABC
0000000000000000: 00000002 00000000 00010000 0001 01  4243 @/tmp/.X11-unix/X0
0000000000000000: 00000003 00000000 00000000 0001 03  4244 /run/user/1000/bus
0000000000000000: 00000002 00000000 00010000 0001 01  4245 /run/other proxy
0000000000000000: 00000002 00000000 00010000 0001 01  4246
";
        let ours: HashSet<u64> = [4242, 4243, 4244, 4245, 4246].into_iter().collect();
        assert_eq!(
            listening_paths(table, &ours),
            vec![
                "/run/user/1000/.dbus-proxy/a11y-bus-proxy-ABC",
                "/run/other proxy"
            ]
        );
        let theirs: HashSet<u64> = [9999].into_iter().collect();
        assert!(listening_paths(table, &theirs).is_empty());
    }
}
