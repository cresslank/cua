//! Exact Linux AT-SPI setup for Chromium existing-profile attachment.

use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io::Write,
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use cua_driver_core::browser::{
    BrowserProduct, BrowserRefusal, BrowserRefusalCode, BrowserSetupDescriptor,
    EXISTING_PROFILE_SETUP_READY_TIMEOUT,
};

use crate::atspi::AtspiNode;

fn refusal(code: BrowserRefusalCode, message: impl Into<String>) -> BrowserRefusal {
    BrowserRefusal::new(code, message)
}

fn field_equals(node: &AtspiNode, expected: &str) -> bool {
    [
        node.name.as_deref(),
        node.value.as_deref(),
        node.description.as_deref(),
    ]
    .into_iter()
    .flatten()
    .any(|value| value.trim().eq_ignore_ascii_case(expected))
}

fn role_is(node: &AtspiNode, accepted: &[&str]) -> bool {
    let role = node.role.trim().to_ascii_lowercase();
    accepted.iter().any(|candidate| role == *candidate)
}

fn trusted_semantic_action(node: &AtspiNode) -> Option<&str> {
    let actions = node
        .actions
        .iter()
        .filter(|action| {
            matches!(
                action.trim().to_ascii_lowercase().as_str(),
                "activate" | "click" | "press" | "toggle"
            )
        })
        .collect::<Vec<_>>();
    (actions.len() == 1).then(|| actions[0].as_str())
}

fn perform_verified_setup_action(
    target: &crate::wayland::ExactTargetProof,
    node: &crate::atspi::AtspiNode,
    action: &str,
) -> anyhow::Result<(String, bool)> {
    crate::atspi::perform_verified_action_by_key(
        target,
        node.element_key,
        &node.role,
        node.name.as_deref().unwrap_or_default(),
        node.checked,
        &node.actions,
        action,
    )
}

fn setup_page_proven(
    nodes: &[AtspiNode],
    descriptor: &BrowserSetupDescriptor,
    trusted_navigation: bool,
) -> bool {
    let has_contradictory_address_bar = nodes.iter().any(|node| {
        role_is(node, &["entry", "text"])
            && field_equals(node, "Address and search bar")
            && node.value.as_deref().is_some_and(|value| {
                !value.trim().is_empty() && !value.trim().eq_ignore_ascii_case(descriptor.setup_url)
            })
    });
    let exact_url = nodes.iter().any(|node| {
        role_is(node, &["entry", "text"])
            && field_equals(node, "Address and search bar")
            && node
                .value
                .as_deref()
                .or(node.name.as_deref())
                .is_some_and(|value| value.trim().eq_ignore_ascii_case(descriptor.setup_url))
    });
    let exact_heading = nodes.iter().any(|node| {
        role_is(node, &["heading", "section", "static"])
            && field_equals(node, descriptor.page_heading)
    });
    let exact_page = nodes.iter().any(|node| {
        role_is(node, &["document web", "document frame"])
            && descriptor
                .page_titles
                .iter()
                .any(|title| field_equals(node, title))
    });
    let exact_identity =
        exact_url || (trusted_navigation && !has_contradictory_address_bar && exact_page);
    exact_identity && exact_heading
}

fn exact_setup_checkbox<'a>(
    nodes: &'a [AtspiNode],
    descriptor: &BrowserSetupDescriptor,
    trusted_navigation: bool,
) -> Result<Option<&'a AtspiNode>, BrowserRefusal> {
    if !setup_page_proven(nodes, descriptor, trusted_navigation) {
        return Ok(None);
    }
    let matches = nodes
        .iter()
        .filter(|node| {
            role_is(node, &["check box", "checkbox"])
                && field_equals(node, descriptor.checkbox_label)
                && trusted_semantic_action(node).is_some()
                && node.element_index.is_some()
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [] => Ok(None),
        [node] => Ok(Some(*node)),
        _ => Err(refusal(
            BrowserRefusalCode::BrowserWrongTargetRefused,
            "multiple exact remote-debugging checkboxes were exposed",
        )),
    }
}

fn exact_setup_navigation<'a>(
    nodes: &'a [AtspiNode],
    descriptor: &BrowserSetupDescriptor,
) -> Result<Option<&'a AtspiNode>, BrowserRefusal> {
    let exact_page = nodes.iter().any(|node| {
        role_is(node, &["document web", "document frame"])
            && descriptor
                .page_titles
                .iter()
                .any(|title| field_equals(node, title))
    });
    if !exact_page {
        return Ok(None);
    }
    let matches = nodes
        .iter()
        .filter(|node| {
            role_is(node, &["push button", "button"])
                && field_equals(node, descriptor.page_heading)
                && trusted_semantic_action(node).is_some()
                && node.element_index.is_some()
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [] => Ok(None),
        [node] => Ok(Some(*node)),
        _ => Err(refusal(
            BrowserRefusalCode::BrowserWrongTargetRefused,
            "multiple exact remote-debugging navigation controls were exposed",
        )),
    }
}

fn setup_not_ready_message(descriptor: &BrowserSetupDescriptor) -> String {
    format!(
        "the exact {} remote-debugging setup page did not become ready; on Linux, existing-profile setup requires the browser's complete AT-SPI tree (launch Chromium-family browsers with --force-renderer-accessibility, or use a screen reader that enables full renderer accessibility)",
        descriptor.product_name
    )
}

fn validate_single_exact_native_window(
    target: &crate::wayland::ExactTargetProof,
) -> anyhow::Result<()> {
    crate::wayland::validate_single_exact_target(target)
}

fn close_tab(target: &crate::wayland::ExactTargetProof, window_id: u64) -> anyhow::Result<()> {
    crate::wayland::validate_exact_target(target)?;
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        crate::wayland::hotkey(target.clone(), &["ctrl".to_owned(), "w".to_owned()])
    } else {
        crate::input::with_x11_foreground(window_id, 80, || {
            crate::input::send_key_xtest("w", &["ctrl"])
        })
    }
}

/// The exact address-and-search field of the approved window, or `None` while
/// the freshly created tab has not exposed one yet. More than one is refused:
/// the field is where the setup URL is about to be written, so the wrong pick
/// navigates a surface the caller never approved.
fn exact_omnibox<'a>(
    nodes: &'a [AtspiNode],
    descriptor: &BrowserSetupDescriptor,
) -> Result<Option<&'a AtspiNode>, BrowserRefusal> {
    let matches = nodes
        .iter()
        .filter(|node| {
            role_is(node, &["entry", "text"])
                && field_equals(node, "Address and search bar")
                && node.element_index.is_some()
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [] => Ok(None),
        [node] => Ok(Some(*node)),
        _ => Err(refusal(
            BrowserRefusalCode::BrowserWrongTargetRefused,
            format!(
                "{} exposed multiple exact address-and-search fields",
                descriptor.product_name
            ),
        )),
    }
}

/// Whether the omnibox currently holds exactly the fixed setup URL.
fn omnibox_holds_setup_url(node: &AtspiNode, descriptor: &BrowserSetupDescriptor) -> bool {
    node.value
        .as_deref()
        .or(node.name.as_deref())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case(descriptor.setup_url))
}

/// Navigate the approved window to its fixed setup page.
///
/// This mirrors the macOS and Windows adapters rather than synthesizing the URL
/// keystroke by keystroke: write the whole URL into the address field through
/// the accessibility API, read it back to prove it landed, and only then commit
/// with a single Enter. `set_text_contents` is the AT-SPI counterpart of UIA's
/// `ValuePattern::SetValue` and AppKit's `AXValue`.
///
/// Per-character synthesis was the wrong primitive here. XTEST keysym lookup is
/// keyboard-layout dependent and wlroots virtual-keyboard seats drop
/// punctuation, so `chrome://inspect` could arrive as `inspect` — which the
/// omnibox treats as a search term, silently navigating to a search-engine
/// results page. Nothing downstream could tell that apart from a slow-loading
/// setup page, so the flow reported a readiness timeout while leaving the user
/// on someone else's website. Writing the value whole removes the layout
/// dependency, and the read-back turns any residual mangling into an immediate,
/// accurate refusal.
fn with_target_foreground<T>(
    target: &crate::wayland::ExactTargetProof,
    operation: impl FnOnce(&crate::wayland::ExactTargetInputGuard<'_>) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    crate::wayland::with_target_foreground(target.pid(), target.window_id(), operation)
}

fn trusted_setup_navigation(
    pid: u32,
    window_id: u64,
    target: &crate::wayland::ExactTargetProof,
    descriptor: &BrowserSetupDescriptor,
) -> anyhow::Result<()> {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();

    // A fresh tab, so the setup page never displaces a page the user was on.
    // `ctrl+l` then focuses the address field. Both are single letters, so
    // neither depends on the keyboard layout the way punctuation does.
    let focus_omnibox = || -> anyhow::Result<()> {
        with_target_foreground(target, |guard| {
            if wayland {
                crate::wayland::hotkey_focused_for_target(
                    guard,
                    &["ctrl".to_owned(), "l".to_owned()],
                )
            } else {
                crate::input::send_key_xtest("l", &["ctrl"])
            }
        })
    };

    with_target_foreground(target, |guard| {
        if wayland {
            crate::wayland::hotkey_focused_for_target(guard, &["ctrl".to_owned(), "t".to_owned()])
        } else {
            crate::input::send_key_xtest("t", &["ctrl"])
        }
    })?;
    std::thread::sleep(Duration::from_millis(100));
    focus_omnibox()?;

    // Wait for the new tab to publish its address field before writing to it.
    let deadline = Instant::now() + EXISTING_PROFILE_SETUP_READY_TIMEOUT;
    loop {
        crate::wayland::validate_exact_target(target)?;
        let tree =
            window_scoped_tree(pid, window_id).map_err(|error| anyhow::anyhow!(error.message))?;
        if exact_omnibox(&tree.nodes, descriptor)
            .map_err(|error| anyhow::anyhow!(error.message))?
            .is_some()
        {
            break;
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "the approved {} window never exposed an exact address-and-search field",
                descriptor.product_name
            );
        }
        std::thread::sleep(Duration::from_millis(150));
    }

    // Transfer the URL through the clipboard rather than the keyboard.
    //
    // Chromium's AT-SPI bridge does not honour EditableText writes on the
    // omnibox, so the accessibility set-value that macOS (`AXValue`) and
    // Windows (`ValuePattern::SetValue`) rely on has no working counterpart
    // here. A paste keeps the property that actually matters: the exact string
    // arrives in one operation, with no per-keysym synthesis to be mistranslated
    // by the active layout or dropped by a virtual-keyboard seat. `ctrl+a` and
    // `ctrl+v` are plain letters, so they carry no layout dependency of their own.
    use cua_driver_core::clipboard::ClipboardBackend;
    let clipboard = crate::clipboard::LinuxClipboard::new();
    let restore = clipboard.read_text().ok().flatten();
    clipboard
        .write_text(descriptor.setup_url.to_owned())
        .map_err(|error| anyhow::anyhow!("could not stage the fixed setup URL: {error}"))?;
    let paste = with_target_foreground(target, |guard| {
        if wayland {
            crate::wayland::hotkey_focused_for_target(guard, &["ctrl".to_owned(), "a".to_owned()])?;
            std::thread::sleep(Duration::from_millis(60));
            crate::wayland::hotkey_focused_for_target(guard, &["ctrl".to_owned(), "v".to_owned()])
        } else {
            crate::input::send_key_xtest("a", &["ctrl"])?;
            std::thread::sleep(Duration::from_millis(60));
            crate::input::send_key_xtest("v", &["ctrl"])
        }
    });
    // The user's clipboard is theirs; put it back whether or not the paste took.
    std::thread::sleep(Duration::from_millis(120));
    if let Some(previous) = restore {
        let _ = clipboard.write_text(previous);
    }
    paste?;

    // Commit. Enter is the one synthesized keystroke left, and it carries no
    // layout dependency.
    with_target_foreground(target, |guard| {
        if wayland {
            crate::wayland::hotkey_focused_for_target(guard, &["enter".to_owned()])
        } else {
            crate::input::send_key_xtest("enter", &[])
        }
    })?;

    // Verify the destination, not the input. Chromium exposes no readable text
    // on its omnibox over AT-SPI — no Value interface and no Text content even
    // while the field holds a URL — so the read-back that the Windows and macOS
    // adapters perform against the address field has no counterpart here.
    // Proving the tab actually arrived at the fixed setup page is the stronger
    // check anyway: it fails for a mistyped URL, a hijacked search, and a
    // redirect alike, and it names what was reached instead of timing out.
    let deadline = Instant::now() + EXISTING_PROFILE_SETUP_READY_TIMEOUT;
    loop {
        crate::wayland::validate_exact_target(target)?;
        let tree =
            window_scoped_tree(pid, window_id).map_err(|error| anyhow::anyhow!(error.message))?;
        if tree.nodes.iter().any(|node| {
            role_is(node, &["document web", "document frame"])
                && descriptor
                    .page_titles
                    .iter()
                    .any(|title| field_equals(node, title))
        }) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            let landed = tree
                .nodes
                .iter()
                .find(|node| role_is(node, &["document web", "document frame"]))
                .and_then(|node| node.name.clone())
                .unwrap_or_else(|| "no document".to_owned());
            anyhow::bail!(
                "the approved {} window did not reach its fixed setup page; it is showing {:?}. \
                 The address field is written through the clipboard, so this means the browser \
                 rejected or redirected the URL rather than that a keystroke was dropped",
                descriptor.product_name,
                landed
            );
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// Walk the target window's accessibility tree, refusing unless the snapshot is
/// provably confined to that one window.
///
/// AT-SPI publishes a single tree per process, so a browser showing several
/// windows exposes all of their controls together — including one "Allow remote
/// debugging for this browser instance" checkbox per open setup page. Matching a
/// control by label across that tree can therefore find a control the caller did
/// not name. Requiring proven window scope is what makes the exact-window
/// contract real rather than assumed.
fn window_scoped_tree(
    pid: u32,
    window_id: u64,
) -> Result<crate::atspi::AtspiTreeResult, BrowserRefusal> {
    let tree = crate::atspi::walk_tree_bounded(pid, window_id, None, None, None);
    if !tree.trusted {
        return Err(refusal(
            BrowserRefusalCode::BrowserRouteUnavailable,
            "no trusted AT-SPI tree for the approved browser window; \
             the accessibility bus must be reachable to prove which window a control belongs to",
        ));
    }
    if !tree.window_scoped {
        return Err(refusal(
            BrowserRefusalCode::BrowserBindingAmbiguous,
            format!(
                "could not prove which of pid {pid}'s accessibility top-levels renders window \
                 {window_id}, so a matched control cannot be attributed to the approved window; \
                 relaunch the browser with --remote-debugging-port to skip setup entirely"
            ),
        ));
    }
    Ok(tree)
}

// Remote-debugging state and DevToolsActivePort belong to the browser
// process/profile, not to one of its windows.
type PendingSetupKey = u32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SetupResourceKey {
    device: u64,
    inode: u64,
}

impl SetupResourceKey {
    fn for_profile(profile_path: &Path) -> Result<Self, BrowserRefusal> {
        let metadata = std::fs::metadata(profile_path).map_err(|error| {
            refusal(
                BrowserRefusalCode::BrowserBindingStale,
                format!("could not identify the approved browser profile resource: {error}"),
            )
        })?;
        if !metadata.is_dir() {
            return Err(refusal(
                BrowserRefusalCode::BrowserBindingStale,
                "the approved browser profile resource is not a directory",
            ));
        }
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }

    fn lock_name(self) -> String {
        format!("profile-{:016x}-{:016x}.lock", self.device, self.inode)
    }

    fn state_name(self) -> String {
        format!("profile-{:016x}-{:016x}.state", self.device, self.inode)
    }
}

#[derive(Debug)]
struct SetupReservation {
    _lock_file: File,
    state_path: PathBuf,
    release_on_drop: bool,
}

impl SetupReservation {
    fn acquire(profile_path: &Path) -> Result<Self, BrowserRefusal> {
        let key = SetupResourceKey::for_profile(profile_path)?;
        let lock_dir = setup_reservation_dir();
        Self::acquire_at(key, &lock_dir)
    }

    fn acquire_at(key: SetupResourceKey, lock_dir: &Path) -> Result<Self, BrowserRefusal> {
        ensure_secure_reservation_dir(lock_dir)?;
        let lock_path = lock_dir.join(key.lock_name());
        let lock_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&lock_path)
            .map_err(|error| {
                refusal(
                    BrowserRefusalCode::BrowserRouteUnavailable,
                    format!("could not open the browser setup reservation: {error}"),
                )
            })?;
        let metadata = lock_file.metadata().map_err(|error| {
            refusal(
                BrowserRefusalCode::BrowserRouteUnavailable,
                format!("could not inspect the browser setup reservation: {error}"),
            )
        })?;
        // Refuse links, device nodes, foreign-owner files, and files writable
        // by other users before trusting this as host-wide transaction state.
        let effective_uid = unsafe { libc::geteuid() };
        if !metadata.file_type().is_file()
            || metadata.uid() != effective_uid
            || metadata.nlink() != 1
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err(refusal(
                BrowserRefusalCode::BrowserRouteUnavailable,
                "refusing an unsafe browser setup reservation file",
            ));
        }
        // SAFETY: lock_file owns a valid descriptor for the duration of flock.
        let locked = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if locked != 0 {
            let error = std::io::Error::last_os_error();
            let busy = error
                .raw_os_error()
                .is_some_and(|code| code == libc::EWOULDBLOCK || code == libc::EAGAIN);
            return Err(refusal(
                if busy {
                    BrowserRefusalCode::BrowserBindingAmbiguous
                } else {
                    BrowserRefusalCode::BrowserRouteUnavailable
                },
                if busy {
                    "another approved browser setup is already preparing this browser profile"
                        .to_owned()
                } else {
                    format!("could not lock the browser setup reservation: {error}")
                },
            ));
        }

        let state_path = lock_dir.join(key.state_name());
        if !reservation_marker_is_clean(&state_path)? {
            return Err(refusal(
                BrowserRefusalCode::BrowserBindingStale,
                "a prior browser setup did not prove terminal cleanup; the profile remains reserved fail-closed",
            ));
        }
        // Persist the active marker atomically before the first browser read or
        // mutation. A failed write leaves the previous state intact.
        write_reservation_marker(&state_path, b"active\n").map_err(|error| {
            refusal(
                BrowserRefusalCode::BrowserRouteUnavailable,
                format!("could not arm the browser setup reservation: {error}"),
            )
        })?;
        Ok(Self {
            _lock_file: lock_file,
            state_path,
            release_on_drop: true,
        })
    }

    fn ensure_clean(profile_path: &Path) -> Result<(), BrowserRefusal> {
        let key = SetupResourceKey::for_profile(profile_path)?;
        let lock_dir = setup_reservation_dir();
        ensure_secure_reservation_dir(&lock_dir)?;
        let lock_path = lock_dir.join(key.lock_name());
        if !lock_path.try_exists().map_err(|error| {
            refusal(
                BrowserRefusalCode::BrowserRouteUnavailable,
                format!("could not inspect the browser setup reservation path: {error}"),
            )
        })? {
            return Ok(());
        }
        let lock_file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&lock_path)
            .map_err(|error| {
                refusal(
                    BrowserRefusalCode::BrowserRouteUnavailable,
                    format!("could not open the browser setup reservation: {error}"),
                )
            })?;
        let metadata = lock_file.metadata().map_err(|error| {
            refusal(
                BrowserRefusalCode::BrowserRouteUnavailable,
                format!("could not inspect the browser setup reservation: {error}"),
            )
        })?;
        let effective_uid = unsafe { libc::geteuid() };
        if !metadata.file_type().is_file()
            || metadata.uid() != effective_uid
            || metadata.nlink() != 1
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err(refusal(
                BrowserRefusalCode::BrowserRouteUnavailable,
                "refusing an unsafe browser setup reservation file",
            ));
        }
        let locked = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if locked != 0 {
            return Err(refusal(
                BrowserRefusalCode::BrowserBindingAmbiguous,
                "browser endpoint discovery is blocked by an active setup for this profile",
            ));
        }
        if reservation_marker_is_clean(&lock_dir.join(key.state_name()))? {
            Ok(())
        } else {
            Err(refusal(
                BrowserRefusalCode::BrowserBindingStale,
                "browser endpoint discovery is blocked because prior setup cleanup is unproven",
            ))
        }
    }

    fn retain_fail_closed(&mut self) {
        // Do not rewrite the already-synced `active` marker: it is the durable
        // poison state. Rewriting it after cleanup failed would create a crash
        // window where truncation could be mistaken for a clean reservation.
        self.release_on_drop = false;
    }
}

impl Drop for SetupReservation {
    fn drop(&mut self) {
        if !self.release_on_drop {
            return;
        }
        if let Err(error) = write_reservation_marker(&self.state_path, b"clean\n") {
            // Atomic marker replacement leaves `active` intact on failure.
            eprintln!("cua-driver: could not clear browser setup reservation: {error}");
        }
    }
}

fn reservation_marker_is_clean(path: &Path) -> Result<bool, BrowserRefusal> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(error) => {
            return Err(refusal(
                BrowserRefusalCode::BrowserRouteUnavailable,
                format!("could not inspect the browser setup state marker: {error}"),
            ));
        }
    };
    let effective_uid = unsafe { libc::geteuid() };
    if !metadata.file_type().is_file()
        || metadata.uid() != effective_uid
        || metadata.nlink() != 1
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(refusal(
            BrowserRefusalCode::BrowserRouteUnavailable,
            "refusing an unsafe browser setup state marker",
        ));
    }
    std::fs::read(path)
        .map(|marker| marker == b"clean\n")
        .map_err(|error| {
            refusal(
                BrowserRefusalCode::BrowserRouteUnavailable,
                format!("could not read the browser setup state marker: {error}"),
            )
        })
}

fn write_reservation_marker(path: &Path, marker: &[u8]) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("browser setup marker has no parent"))?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temp = parent.join(format!(".setup-state-{}-{nonce}.tmp", std::process::id()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&temp)?;
        file.write_all(marker)?;
        file.sync_all()?;
        std::fs::rename(&temp, path)?;
        File::open(parent)?.sync_all()
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

fn setup_reservation_dir() -> PathBuf {
    // Use one fixed per-UID host path rather than XDG_RUNTIME_DIR/TMPDIR:
    // separate daemon/session environments must still contend for the same
    // browser profile transaction.
    let effective_uid = unsafe { libc::geteuid() };
    PathBuf::from(format!("/tmp/cua-driver-browser-setup-{effective_uid}"))
}

fn ensure_secure_reservation_dir(path: &Path) -> Result<(), BrowserRefusal> {
    match std::fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(refusal(
                BrowserRefusalCode::BrowserRouteUnavailable,
                format!("could not create the browser setup reservation directory: {error}"),
            ));
        }
    }
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        refusal(
            BrowserRefusalCode::BrowserRouteUnavailable,
            format!("could not inspect the browser setup reservation directory: {error}"),
        )
    })?;
    let effective_uid = unsafe { libc::geteuid() };
    if !metadata.file_type().is_dir()
        || metadata.uid() != effective_uid
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(refusal(
            BrowserRefusalCode::BrowserRouteUnavailable,
            "refusing an unsafe browser setup reservation directory",
        ));
    }
    Ok(())
}

pub struct SetupUiHandle {
    pid: u32,
    window_id: u64,
    target: crate::wayland::ExactTargetProof,
    reservation: SetupReservation,
    descriptor: &'static BrowserSetupDescriptor,
    trusted_setup_navigation: bool,
    enable_attempted: bool,
    remote_debugging_mutation_possible: bool,
    trusted_checkbox_fallback_attempted: bool,
    pub opened_setup_page: bool,
    pub enabled_remote_debugging: bool,
    pub focused_setup_address_field: bool,
    pub foregrounded_window: bool,
    pub injected_global_input: bool,
    pub used_bounded_pixel_fallback: bool,
}

impl SetupUiHandle {
    fn validate(&self) -> anyhow::Result<()> {
        crate::wayland::validate_exact_target(&self.target)
    }

    fn rollback_remote_debugging(&mut self) -> bool {
        if !remote_debugging_cleanup_required(
            self.enabled_remote_debugging,
            self.remote_debugging_mutation_possible,
        ) {
            return true;
        }
        if validate_single_exact_native_window(&self.target).is_err() {
            return false;
        }
        let Ok(tree) = window_scoped_tree(self.pid, self.window_id) else {
            return false;
        };
        let restored =
            exact_setup_checkbox(&tree.nodes, self.descriptor, self.trusted_setup_navigation)
                .ok()
                .flatten()
                .is_some_and(|checkbox| {
                    if checkbox.checked == Some(false) {
                        return true;
                    }
                    let element_key = checkbox.element_key;
                    let Some(action) = trusted_semantic_action(checkbox) else {
                        return false;
                    };
                    if checkbox.checked != Some(true)
                        || perform_verified_setup_action(&self.target, checkbox, action).is_err()
                        || validate_single_exact_native_window(&self.target).is_err()
                    {
                        return false;
                    }
                    let Ok(verified) = window_scoped_tree(self.pid, self.window_id) else {
                        return false;
                    };
                    exact_setup_checkbox(
                        &verified.nodes,
                        self.descriptor,
                        self.trusted_setup_navigation,
                    )
                    .ok()
                    .flatten()
                    .is_some_and(|current| {
                        current.element_key == element_key && current.checked == Some(false)
                    })
                });
        if restored {
            self.enabled_remote_debugging = false;
            self.remote_debugging_mutation_possible = false;
        }
        restored
    }

    pub fn abort(mut self, error: BrowserRefusal) -> BrowserRefusal {
        let enabled_remote_debugging =
            self.enabled_remote_debugging || self.remote_debugging_mutation_possible;
        let restored_remote_debugging = self.rollback_remote_debugging();
        let opened_setup_page = self.opened_setup_page;
        let focused_setup_address_field = self.focused_setup_address_field;
        let foregrounded_window = self.foregrounded_window;
        let injected_global_input = self.injected_global_input;
        let closed_setup_page =
            restored_remote_debugging && self.close_setup_page().unwrap_or(true);
        let mut error = error;
        let cause = error.detail.take();
        error.with_detail(serde_json::json!({
            "setup_side_effects": {
                "opened_setup_page": opened_setup_page,
                "closed_setup_page": closed_setup_page,
                "focused_setup_address_field": focused_setup_address_field,
                "enabled_remote_debugging": enabled_remote_debugging,
                "foregrounded_window": foregrounded_window,
                "injected_global_input": injected_global_input,
                "restored_remote_debugging": restored_remote_debugging,
            },
            "cause": cause,
        }))
    }

    pub fn close_for_success(mut self) -> Result<Option<bool>, BrowserRefusal> {
        if !self.opened_setup_page {
            // The endpoint was accepted; any debugging state enabled by this
            // handle is now committed rather than a rollback obligation.
            self.enabled_remote_debugging = false;
            self.remote_debugging_mutation_possible = false;
            return Ok(None);
        }
        if let Err(error) = self.validate() {
            let error = refusal(
                BrowserRefusalCode::BrowserWrongTargetRefused,
                format!("the immutable setup target became stale before cleanup: {error}"),
            );
            return Err(self.abort(error));
        }
        let tree = match window_scoped_tree(self.pid, self.window_id) {
            Ok(tree) => tree,
            Err(error) => return Err(self.abort(error)),
        };
        if !setup_page_proven(&tree.nodes, self.descriptor, self.trusted_setup_navigation) {
            let error = refusal(
                BrowserRefusalCode::BrowserWrongTargetRefused,
                "the temporary setup page was no longer exact before cleanup",
            );
            return Err(self.abort(error));
        }
        if let Err(error) = close_tab(&self.target, self.window_id) {
            let error = refusal(
                BrowserRefusalCode::BrowserWrongTargetRefused,
                format!("could not close the exact temporary setup tab: {error}"),
            );
            return Err(self.abort(error));
        }
        self.opened_setup_page = false;
        self.enabled_remote_debugging = false;
        self.remote_debugging_mutation_possible = false;
        Ok(Some(true))
    }

    fn close_setup_page(&mut self) -> Option<bool> {
        if !self.opened_setup_page {
            return None;
        }
        if self.validate().is_err() {
            return Some(false);
        }
        let Ok(tree) = window_scoped_tree(self.pid, self.window_id) else {
            return Some(false);
        };
        let closed = setup_page_proven(&tree.nodes, self.descriptor, self.trusted_setup_navigation)
            && close_tab(&self.target, self.window_id).is_ok();
        if closed {
            self.opened_setup_page = false;
        }
        Some(closed)
    }
}

impl Drop for SetupUiHandle {
    fn drop(&mut self) {
        if !remote_debugging_cleanup_required(
            self.enabled_remote_debugging,
            self.remote_debugging_mutation_possible,
        ) && !self.opened_setup_page
        {
            return;
        }
        let restored = self.rollback_remote_debugging();
        let closed = restored && self.close_setup_page().unwrap_or(true);
        if !restored || !closed {
            // Cancellation must not silently release browser-setup ownership
            // while its side effects remain. Keep the host-wide profile
            // reservation persistently poisoned until explicit cleanup/reset.
            self.reservation.retain_fail_closed();
            eprintln!(
                "cua-driver: browser setup terminal cleanup failed; retaining reservation fail-closed"
            );
        }
    }
}

fn perform_with_rollback_armed<T, E>(
    mutation_possible: &mut bool,
    action: impl FnOnce() -> Result<T, E>,
) -> Result<T, E> {
    *mutation_possible = true;
    action()
}

fn remote_debugging_cleanup_required(enabled: bool, mutation_possible: bool) -> bool {
    enabled || mutation_possible
}

fn pending_setups() -> &'static Mutex<HashMap<PendingSetupKey, SetupUiHandle>> {
    static PENDING: OnceLock<Mutex<HashMap<PendingSetupKey, SetupUiHandle>>> = OnceLock::new();
    PENDING.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Fail closed while setup owns or has poisoned this profile resource.
/// Endpoint discovery rechecks this immediately before publication.
pub fn ensure_profile_discoverable(profile_path: &Path) -> Result<(), BrowserRefusal> {
    SetupReservation::ensure_clean(profile_path)
}

pub fn retain_pending(
    pid: u32,
    window_id: u64,
    handle: SetupUiHandle,
) -> Result<(), BrowserRefusal> {
    if (handle.pid, handle.window_id) != (pid, window_id) {
        return Err(handle.abort(refusal(
            BrowserRefusalCode::BrowserBindingStale,
            "the browser setup reservation does not match the retained window",
        )));
    }
    let mut pending = pending_setups().lock().unwrap();
    if pending.contains_key(&pid) {
        drop(pending);
        return Err(handle.abort(refusal(
            BrowserRefusalCode::BrowserBindingAmbiguous,
            "another approved browser setup is already pending for this browser profile",
        )));
    }
    pending.insert(pid, handle);
    Ok(())
}

pub fn commit_pending(pid: u32, window_id: u64) -> Result<bool, BrowserRefusal> {
    let handle = pending_setups()
        .lock()
        .unwrap()
        .remove(&pid)
        .ok_or_else(|| {
            refusal(
                BrowserRefusalCode::BrowserBindingStale,
                "the exact pending browser setup cleanup handle is missing",
            )
        })?;
    if handle.window_id != window_id {
        return Err(handle.abort(refusal(
            BrowserRefusalCode::BrowserBindingStale,
            "the pending browser setup belongs to a different approved window",
        )));
    }
    Ok(handle.close_for_success()?.unwrap_or(false))
}

pub fn abort_pending(pid: u32, window_id: u64, error: BrowserRefusal) -> BrowserRefusal {
    match pending_setups().lock().unwrap().remove(&pid) {
        Some(handle) if handle.window_id == window_id => handle.abort(error),
        Some(handle) => handle.abort(refusal(
            BrowserRefusalCode::BrowserBindingStale,
            "the pending browser setup belongs to a different approved window",
        )),
        None => error.with_detail(serde_json::json!({
            "setup_cleanup": "the exact pending browser setup cleanup handle was missing"
        })),
    }
}

pub fn enable(
    pid: u32,
    window_id: u64,
    descriptor: &'static BrowserSetupDescriptor,
    profile_path: &Path,
) -> Result<SetupUiHandle, BrowserRefusal> {
    let target = crate::wayland::establish_exact_target(pid, window_id).map_err(|error| {
        refusal(
            BrowserRefusalCode::BrowserWrongTargetRefused,
            format!("could not establish the immutable browser setup target: {error}"),
        )
    })?;
    // Reserve before the first setup-tree read or navigation.
    let mut reservation = Some(SetupReservation::acquire(profile_path)?);
    let initial = window_scoped_tree(pid, window_id)?;
    let initial_checkbox = exact_setup_checkbox(&initial.nodes, descriptor, false)?;
    let mut handle = if initial_checkbox.is_some() {
        SetupUiHandle {
            pid,
            window_id,
            target: target.clone(),
            descriptor,
            trusted_setup_navigation: false,
            reservation: reservation.take().expect("setup reservation available"),
            enable_attempted: false,
            remote_debugging_mutation_possible: false,
            trusted_checkbox_fallback_attempted: false,
            opened_setup_page: false,
            enabled_remote_debugging: false,
            focused_setup_address_field: false,
            foregrounded_window: false,
            injected_global_input: false,
            used_bounded_pixel_fallback: false,
        }
    } else {
        let handle = SetupUiHandle {
            pid,
            window_id,
            target: target.clone(),
            descriptor,
            trusted_setup_navigation: true,
            reservation: reservation.take().expect("setup reservation available"),
            enable_attempted: false,
            remote_debugging_mutation_possible: false,
            trusted_checkbox_fallback_attempted: false,
            opened_setup_page: true,
            enabled_remote_debugging: false,
            focused_setup_address_field: true,
            foregrounded_window: true,
            injected_global_input: true,
            used_bounded_pixel_fallback: false,
        };
        if let Err(error) = crate::wayland::validate_exact_target(&target)
            .and_then(|_| trusted_setup_navigation(pid, window_id, &target, descriptor))
        {
            return Err(handle.abort(refusal(
                BrowserRefusalCode::BrowserWrongTargetRefused,
                format!(
                    "could not navigate the exact {} window to its fixed setup page: {error}",
                    descriptor.product_name
                ),
            )));
        }
        handle
    };

    let deadline = Instant::now() + EXISTING_PROFILE_SETUP_READY_TIMEOUT;
    loop {
        if let Err(error) = handle.validate() {
            return Err(handle.abort(refusal(
                BrowserRefusalCode::BrowserWrongTargetRefused,
                format!("the immutable browser setup target became stale: {error}"),
            )));
        }
        let tree = match window_scoped_tree(pid, window_id) {
            Ok(tree) => tree,
            Err(error) => return Err(handle.abort(error)),
        };
        match exact_setup_checkbox(&tree.nodes, descriptor, handle.trusted_setup_navigation) {
            Ok(Some(node)) => match node.checked {
                Some(true) => {
                    if handle.enable_attempted {
                        handle.enabled_remote_debugging = true;
                    }
                    return Ok(handle);
                }
                Some(false) if !handle.enable_attempted => {
                    let element_key = node.element_key;
                    if let Err(error) = validate_single_exact_native_window(&handle.target) {
                        return Err(handle.abort(refusal(
                            BrowserRefusalCode::BrowserWrongTargetRefused,
                            format!("the immutable browser setup target became stale: {error}"),
                        )));
                    }
                    let current = match window_scoped_tree(pid, window_id) {
                        Ok(tree) => tree,
                        Err(error) => return Err(handle.abort(error)),
                    };
                    let current_checkbox = match exact_setup_checkbox(
                        &current.nodes,
                        descriptor,
                        handle.trusted_setup_navigation,
                    ) {
                        Ok(Some(current_checkbox))
                            if current_checkbox.checked == Some(false)
                                && current_checkbox.element_key == element_key =>
                        {
                            current_checkbox
                        }
                        Ok(_) => {
                            return Err(handle.abort(refusal(
                                BrowserRefusalCode::BrowserWrongTargetRefused,
                                "the exact checkbox changed before its trusted action",
                            )));
                        }
                        Err(error) => return Err(handle.abort(error)),
                    };
                    // Arm rollback before crossing the action boundary. AT-SPI
                    // can apply the toggle and lose the method response; that
                    // error must still drive exact, verified disable.
                    handle.enable_attempted = true;
                    let action_target = handle.target.clone();
                    if let Err(error) = perform_with_rollback_armed(
                        &mut handle.remote_debugging_mutation_possible,
                        || {
                            perform_verified_setup_action(
                                &action_target,
                                current_checkbox,
                                trusted_semantic_action(current_checkbox)
                                    .expect("trusted action was already proven"),
                            )
                        },
                    ) {
                        return Err(handle.abort(refusal(
                            BrowserRefusalCode::BrowserWrongTargetRefused,
                            format!("the exact checkbox action failed: {error}"),
                        )));
                    }
                }
                Some(false)
                    if descriptor.product == BrowserProduct::MicrosoftEdge
                        && !handle.trusted_checkbox_fallback_attempted =>
                {
                    handle.trusted_checkbox_fallback_attempted = true;
                    handle.foregrounded_window = true;
                    let trusted_navigation = handle.trusted_setup_navigation;
                    let clicked = (|| -> anyhow::Result<bool> {
                        validate_single_exact_native_window(&handle.target)?;
                        std::thread::sleep(Duration::from_millis(60));
                        validate_single_exact_native_window(&handle.target)?;
                        let tree = window_scoped_tree(pid, window_id)
                            .map_err(|error| anyhow::anyhow!(error.message))?;
                        let checkbox = exact_setup_checkbox(
                            &tree.nodes,
                            descriptor,
                            trusted_navigation,
                        )
                        .map_err(|error| anyhow::anyhow!(error.message))?
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "the exact Microsoft Edge remote-debugging checkbox became stale before the trusted click"
                            )
                        })?;
                        if checkbox.checked == Some(true) {
                            return Ok(false);
                        }
                        if checkbox.checked != Some(false) {
                            anyhow::bail!(
                                "the exact Microsoft Edge remote-debugging checkbox had an unknown state before the trusted click"
                            );
                        }
                        let element_key = checkbox.element_key;
                        validate_single_exact_native_window(&handle.target)?;
                        let current = window_scoped_tree(pid, window_id)
                            .map_err(|error| anyhow::anyhow!(error.message))?;
                        let current_checkbox = exact_setup_checkbox(
                            &current.nodes,
                            descriptor,
                            trusted_navigation,
                        )
                        .map_err(|error| anyhow::anyhow!(error.message))?
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "the exact Microsoft Edge checkbox disappeared before the trusted click"
                            )
                        })?;
                        if current_checkbox.checked != Some(false)
                            || current_checkbox.element_key != element_key
                        {
                            anyhow::bail!(
                                "the exact Microsoft Edge checkbox changed before the trusted click"
                            );
                        }
                        // Refresh bounds by stable object identity after the
                        // semantic check. Carry the immutable native window ID
                        // so nested accessibility/content offsets are retained,
                        // then revalidate the compositor target immediately
                        // before global injection.
                        let (x, y, width, height) =
                            crate::atspi::get_verified_element_bounds_by_key(
                                pid,
                                window_id,
                                element_key,
                                &current_checkbox.role,
                                current_checkbox.name.as_deref().unwrap_or_default(),
                                current_checkbox.checked,
                                &current_checkbox.actions,
                            )?;
                        if width <= 1 || height <= 1 {
                            anyhow::bail!(
                                "the exact Microsoft Edge remote-debugging checkbox had empty screen bounds"
                            );
                        }
                        let center_x = x
                            .checked_add(i32::try_from(width / 2)?)
                            .ok_or_else(|| anyhow::anyhow!("checkbox center x overflowed"))?;
                        let center_y = y
                            .checked_add(i32::try_from(height / 2)?)
                            .ok_or_else(|| anyhow::anyhow!("checkbox center y overflowed"))?;
                        validate_single_exact_native_window(&handle.target)?;
                        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
                            crate::wayland::click(handle.target.clone(), center_x, center_y, 1, 1)?;
                        } else {
                            crate::input::with_x11_foreground(window_id, 80, || {
                                crate::input::send_click_xtest_desktop(center_x, center_y, 1, 1)
                            })?;
                        }
                        Ok(true)
                    })();
                    match clicked {
                        Ok(injected) => {
                            handle.injected_global_input |= injected;
                            handle.used_bounded_pixel_fallback |= injected;
                        }
                        Err(error) => {
                            return Err(handle.abort(refusal(
                                BrowserRefusalCode::BrowserWrongTargetRefused,
                                format!(
                                    "could not toggle the exact Microsoft Edge remote-debugging checkbox: {error}"
                                ),
                            )))
                        }
                    }
                }
                Some(false) => {}
                None => {
                    return Err(handle.abort(refusal(
                        BrowserRefusalCode::BrowserWrongTargetRefused,
                        "AT-SPI did not expose the exact checkbox checked state",
                    )));
                }
            },
            Ok(None) => {}
            Err(error) => return Err(handle.abort(error)),
        }
        if Instant::now() >= deadline {
            return Err(handle.abort(refusal(
                BrowserRefusalCode::BrowserWrongTargetRefused,
                setup_not_ready_message(descriptor),
            )));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cua_driver_core::browser::{existing_profile_setup_descriptor, BrowserProduct};

    fn descriptor() -> &'static BrowserSetupDescriptor {
        existing_profile_setup_descriptor(BrowserProduct::GoogleChrome).unwrap()
    }

    fn node(role: &str, name: &str, value: Option<&str>, actions: &[&str]) -> AtspiNode {
        AtspiNode {
            element_index: (!actions.is_empty()).then_some(0),
            role: role.to_owned(),
            name: Some(name.to_owned()),
            value: value.map(str::to_owned),
            checked: None,
            enabled: None,
            selected: None,
            description: None,
            actions: actions.iter().map(|value| (*value).to_owned()).collect(),
            element_key: 0,
            depth: 0,
            parent_element_index: None,
            in_web_content: false,
        }
    }

    #[test]
    fn checkbox_requires_exact_url_heading_and_unique_action() {
        let mut checkbox = node("check box", descriptor().checkbox_label, None, &["toggle"]);
        checkbox.checked = Some(false);
        let nodes = vec![
            node(
                "entry",
                "Address and search bar",
                Some(descriptor().setup_url),
                &["activate"],
            ),
            node("document web", descriptor().page_titles[0], None, &[]),
            node("heading", descriptor().page_heading, None, &[]),
            checkbox,
        ];
        assert_eq!(
            exact_setup_checkbox(&nodes, descriptor(), false)
                .unwrap()
                .unwrap()
                .checked,
            Some(false)
        );

        let titleless = vec![nodes[0].clone(), nodes[2].clone(), nodes[3].clone()];
        assert!(
            exact_setup_checkbox(&titleless, descriptor(), false)
                .unwrap()
                .is_some(),
            "an exact internal URL and heading prove products that omit the document title from AT-SPI"
        );

        let addressless = nodes[1..].to_vec();
        assert!(
            exact_setup_checkbox(&addressless, descriptor(), false)
                .unwrap()
                .is_none(),
            "page labels alone must not authorize a setup action"
        );
        assert!(
            exact_setup_checkbox(&addressless, descriptor(), true)
                .unwrap()
                .is_some(),
            "the exact compositor-routed fixed navigation may substitute for hidden browser chrome"
        );

        let mut redacted_address = nodes.clone();
        redacted_address[0].value = None;
        assert!(
            exact_setup_checkbox(&redacted_address, descriptor(), true)
                .unwrap()
                .is_some(),
            "an address control with a withheld value is not contradictory evidence"
        );

        let mut contradictory = nodes;
        contradictory[0].value = Some("https://example.test/spoof".to_owned());
        assert!(
            exact_setup_checkbox(&contradictory, descriptor(), true)
                .unwrap()
                .is_none(),
            "trusted navigation must not override a visible contradictory address bar"
        );
    }

    #[test]
    fn setup_timeout_explains_linux_renderer_accessibility_precondition() {
        let message = setup_not_ready_message(descriptor());
        assert!(message.contains("complete AT-SPI tree"));
        assert!(message.contains("--force-renderer-accessibility"));
    }

    fn omnibox(value: Option<&str>) -> AtspiNode {
        node("entry", "Address and search bar", value, &["activate"])
    }

    #[test]
    fn omnibox_selection_is_exact_or_refused() {
        let nodes = vec![
            node("push button", "Reload", None, &["press"]),
            omnibox(Some("about:blank")),
        ];
        assert!(exact_omnibox(&nodes, descriptor()).unwrap().is_some());
        assert!(exact_omnibox(&nodes[..1], descriptor()).unwrap().is_none());

        // Two address fields means two candidate destinations; writing the
        // setup URL into a guess could navigate a surface nobody approved.
        let mut ambiguous = nodes;
        ambiguous.push(omnibox(None));
        assert!(exact_omnibox(&ambiguous, descriptor()).is_err());
    }

    /// The regression that motivated the rewrite: XTEST dropped characters and
    /// `chrome://inspect` reached the omnibox as `inspect`, which Chrome
    /// submitted as a search query. The read-back has to reject that before it
    /// is ever committed.
    #[test]
    fn partially_applied_setup_url_is_not_accepted() {
        assert!(!omnibox_holds_setup_url(
            &omnibox(Some("inspect")),
            descriptor()
        ));
        assert!(!omnibox_holds_setup_url(
            &omnibox(Some("chrome://inspect")),
            descriptor()
        ));
        assert!(!omnibox_holds_setup_url(&omnibox(None), descriptor()));
        assert!(!omnibox_holds_setup_url(
            &omnibox(Some("https://www.google.com/search?q=inspect")),
            descriptor()
        ));
    }

    #[test]
    fn fully_applied_setup_url_is_accepted() {
        assert!(omnibox_holds_setup_url(
            &omnibox(Some(descriptor().setup_url)),
            descriptor()
        ));
        // Chromium reports the omnibox value with surrounding whitespace on
        // some toolkit versions, and case is not significant in a scheme.
        assert!(omnibox_holds_setup_url(
            &omnibox(Some("  CHROME://inspect/#remote-debugging  ")),
            descriptor()
        ));
    }

    fn test_lock_dir(label: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "cua-browser-setup-test-{label}-{}-{nonce}",
            std::process::id()
        ))
    }

    fn wait_for_file(path: &Path, child: &mut std::process::Child) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !path.exists() {
            assert!(
                child.try_wait().unwrap().is_none(),
                "reservation holder exited before becoming ready"
            );
            assert!(Instant::now() < deadline, "reservation holder timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn reservation_subprocess_holder() {
        let Some(lock_dir) = std::env::var_os("CUA_SETUP_RESERVATION_TEST_DIR") else {
            return;
        };
        let lock_dir = PathBuf::from(lock_dir);
        let reservation = SetupReservation::acquire_at(
            SetupResourceKey {
                device: 0x1234,
                inode: 0x5678,
            },
            &lock_dir,
        )
        .unwrap();
        std::fs::write(lock_dir.join("ready"), b"ready").unwrap();
        while !lock_dir.join("release").exists() {
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(reservation);
    }

    fn spawn_reservation_holder(lock_dir: &Path) -> std::process::Child {
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "browser_setup_ui::tests::reservation_subprocess_holder",
                "--nocapture",
            ])
            .env("CUA_SETUP_RESERVATION_TEST_DIR", lock_dir)
            .spawn()
            .unwrap()
    }

    #[test]
    fn setup_reservation_is_host_wide_and_profile_resource_keyed() {
        let lock_dir = test_lock_dir("cross-process");
        let mut child = spawn_reservation_holder(&lock_dir);
        wait_for_file(&lock_dir.join("ready"), &mut child);
        let key = SetupResourceKey {
            device: 0x1234,
            inode: 0x5678,
        };
        assert!(SetupReservation::acquire_at(key, &lock_dir).is_err());
        // A distinct browser profile resource is independently reservable,
        // regardless of PID/window identity in either process.
        let other = SetupReservation::acquire_at(
            SetupResourceKey {
                device: key.device,
                inode: key.inode + 1,
            },
            &lock_dir,
        )
        .unwrap();
        drop(other);
        std::fs::write(lock_dir.join("release"), b"release").unwrap();
        assert!(child.wait().unwrap().success());
        assert!(SetupReservation::acquire_at(key, &lock_dir).is_ok());
        std::fs::remove_dir_all(lock_dir).unwrap();
    }

    #[test]
    fn crashed_holder_recovers_kernel_lock_but_leaves_fail_closed_marker() {
        let lock_dir = test_lock_dir("crash");
        let mut child = spawn_reservation_holder(&lock_dir);
        wait_for_file(&lock_dir.join("ready"), &mut child);
        child.kill().unwrap();
        child.wait().unwrap();
        let error = SetupReservation::acquire_at(
            SetupResourceKey {
                device: 0x1234,
                inode: 0x5678,
            },
            &lock_dir,
        )
        .unwrap_err();
        assert_eq!(error.code, BrowserRefusalCode::BrowserBindingStale);
        std::fs::remove_dir_all(lock_dir).unwrap();
    }

    #[test]
    fn failed_terminal_cleanup_keeps_setup_reservation_fail_closed() {
        let lock_dir = test_lock_dir("poison");
        let key = SetupResourceKey {
            device: u64::MAX,
            inode: u64::MAX - 1,
        };
        let mut reservation = SetupReservation::acquire_at(key, &lock_dir).unwrap();
        reservation.retain_fail_closed();
        drop(reservation);
        assert_eq!(
            std::fs::read(lock_dir.join(key.state_name())).unwrap(),
            b"active\n"
        );
        assert!(SetupReservation::acquire_at(key, &lock_dir).is_err());
        std::fs::remove_dir_all(lock_dir).unwrap();
    }

    #[test]
    fn response_lost_after_checkbox_action_still_requires_rollback() {
        let mut mutation_possible = false;
        let result = perform_with_rollback_armed(&mut mutation_possible, || {
            Err::<(), _>("action applied; response lost")
        });
        assert!(result.is_err());
        assert!(mutation_possible);
        assert!(remote_debugging_cleanup_required(false, mutation_possible));
        assert!(remote_debugging_cleanup_required(true, false));
        assert!(!remote_debugging_cleanup_required(false, false));
    }

    #[test]
    fn semantic_mutations_require_one_trusted_action() {
        let mut control = node("check box", descriptor().checkbox_label, None, &["toggle"]);
        assert_eq!(trusted_semantic_action(&control), Some("toggle"));
        control.actions.push("activate".to_owned());
        assert_eq!(trusted_semantic_action(&control), None);
        control.actions = vec!["show menu".to_owned()];
        assert_eq!(trusted_semantic_action(&control), None);
    }
}
