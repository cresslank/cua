//! Client for the bundled **cua WinRects** GNOME Shell extension
//! (`org.cua.WinRects`, see `wayland-helper/winrects@cua/`).
//!
//! On GNOME Mutter (and other non-wlroots compositors) a normal client cannot
//! query a window's on-screen origin (`org.gnome.Shell.Introspect.GetWindows`
//! is privacy-denied; Wayland exposes no global coordinates) nor position an
//! overlay surface at a screen coordinate (no `zwlr_layer_shell_v1`). Both are
//! solvable only from *inside* the compositor — which is what the extension is
//! for. It runs in the shell's privileged context and exposes:
//!
//! - `GetRects() -> json` — every window's `meta_window.get_frame_rect()` (screen
//!   geometry), so `screen_xy = window_origin + AT-SPI CoordType::Window xy`
//!   (the GNOME analogue of the X11 `_GTK_FRAME_EXTENTS` reconstruction).
//! - `MoveCursor(x,y)` / `ClickPulse(x,y)` / `HideCursor()` — draw the agent
//!   cursor as a Clutter actor on the compositor stage.
//!
//! Everything here is **best-effort**: if the extension isn't installed/enabled
//! the calls return `None` / no-op and callers keep the prior behaviour (no
//! screen coords, no Wayland cursor). Most calls use a short-lived `gdbus`
//! subprocess. Cursor calls need one persistent D-Bus connection so Shell can
//! bind actor lifetime to its unique name; a dedicated non-Tokio thread owns
//! that blocking zbus connection and serializes those calls.

use std::collections::{HashMap, HashSet};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::x11::WindowInfo;

const DEST: &str = "org.cua.WinRects";
const PATH: &str = "/org/cua/WinRects";
const IFACE: &str = "org.cua.WinRects";
const DBUS_DEST: &str = "org.freedesktop.DBus";
const DBUS_PATH: &str = "/org/freedesktop/DBus";
const DBUS_IFACE: &str = "org.freedesktop.DBus";
const INTROSPECT_DEST: &str = "org.gnome.Shell.Introspect";
const INTROSPECT_PATH: &str = "/org/gnome/Shell/Introspect";
const INTROSPECT_IFACE: &str = "org.gnome.Shell.Introspect";
/// Public helper API carried by `GetVersion`; follows the upstream cursor and
/// session-badge contract and is intentionally independent from the exact-
/// target capability protocol advertised by `GetCapabilities`.
const REQUIRED_HELPER_API_VERSION: u32 = 8;
const REQUIRED_EXACT_TARGET_PROTOCOL: u64 = 4;
static OVERLAY_DISPATCH_TX: OnceLock<Option<std::sync::mpsc::SyncSender<OverlayDispatchRequest>>> =
    OnceLock::new();
static FOREGROUND_SEQUENCE: AtomicU64 = AtomicU64::new(1);
static PENDING_FOREGROUND: Mutex<Option<String>> = Mutex::new(None);

const OVERLAY_DISPATCH_CAPACITY: usize = 4096;
const OVERLAY_HELPER_PROBE_TTL: Duration = Duration::from_millis(250);

#[derive(Debug)]
enum OverlayDispatchRequest {
    Pin {
        owner: String,
        window_id: u64,
    },
    Move {
        owner: String,
        x: i32,
        y: i32,
    },
    ClickPulse {
        owner: String,
        x: i32,
        y: i32,
    },
    SetColor {
        owner: String,
        fill_color: String,
    },
    SetState {
        owner: String,
        action: String,
        delivery: String,
        target: String,
        active: bool,
    },
    SetSessionLabel {
        owner: String,
        label: String,
    },
    Hide {
        owner: String,
    },
    Remove {
        owner: String,
    },
}

impl OverlayDispatchRequest {
    const fn kind(&self) -> &'static str {
        match self {
            Self::Pin { .. } => "pin",
            Self::Move { .. } => "move",
            Self::ClickPulse { .. } => "click_pulse",
            Self::SetColor { .. } => "set_color",
            Self::SetState { .. } => "set_state",
            Self::SetSessionLabel { .. } => "set_session_label",
            Self::Hide { .. } => "hide",
            Self::Remove { .. } => "remove",
        }
    }
}

fn spawn_overlay_dispatcher<F>(
    mut dispatch: F,
) -> Option<std::sync::mpsc::SyncSender<OverlayDispatchRequest>>
where
    F: FnMut(OverlayDispatchRequest) + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::sync_channel(OVERLAY_DISPATCH_CAPACITY);
    std::thread::Builder::new()
        .name("cua-gnome-cursor-dbus".to_owned())
        .spawn(move || {
            while let Ok(request) = rx.recv() {
                dispatch(request);
            }
        })
        .ok()?;
    Some(tx)
}

fn dispatch_overlay_request(
    connection: &zbus::blocking::Connection,
    targets: &HashMap<String, u64>,
    request: OverlayDispatchRequest,
) {
    let Some(destination) = shell_owner(false) else {
        return;
    };
    if matches!(
        request,
        OverlayDispatchRequest::SetColor { .. }
            | OverlayDispatchRequest::SetState { .. }
            | OverlayDispatchRequest::SetSessionLabel { .. }
    ) && shell_owner(true).is_none()
    {
        // The exact-target protocol is intentionally independent from the
        // semantic-cursor API.  A protocol-v4 helper already loaded by GNOME
        // remains usable until the next login, but it must not receive v8-only
        // cursor badge methods.
        return;
    }
    match request {
        OverlayDispatchRequest::Pin { .. } => {}
        OverlayDispatchRequest::Move { owner, x, y } => {
            let Some(window_id) = targets.get(&owner).copied() else {
                return;
            };
            let Some(target) = resolve_target(window_id) else {
                return;
            };
            let _ = connection.call_method(
                Some(destination.as_str()),
                PATH,
                Some(IFACE),
                "MoveCursorFor",
                &(owner.as_str(), target.target_id.as_str(), x, y),
            );
        }
        OverlayDispatchRequest::ClickPulse { owner, x, y } => {
            let Some(window_id) = targets.get(&owner).copied() else {
                return;
            };
            let Some(target) = resolve_target(window_id) else {
                return;
            };
            let _ = connection.call_method(
                Some(destination.as_str()),
                PATH,
                Some(IFACE),
                "ClickPulseFor",
                &(owner.as_str(), target.target_id.as_str(), x, y),
            );
        }
        OverlayDispatchRequest::SetColor { owner, fill_color } => {
            let _ = connection.call_method(
                Some(destination.as_str()),
                PATH,
                Some(IFACE),
                "SetCursorColorFor",
                &(owner.as_str(), fill_color.as_str()),
            );
        }
        OverlayDispatchRequest::SetState {
            owner,
            action,
            delivery,
            target,
            active,
        } => {
            let _ = connection.call_method(
                Some(destination.as_str()),
                PATH,
                Some(IFACE),
                "SetCursorStateFor",
                &(
                    owner.as_str(),
                    action.as_str(),
                    delivery.as_str(),
                    target.as_str(),
                    active,
                ),
            );
        }
        OverlayDispatchRequest::SetSessionLabel { owner, label } => {
            let _ = connection.call_method(
                Some(destination.as_str()),
                PATH,
                Some(IFACE),
                "SetSessionLabelFor",
                &(owner.as_str(), label.as_str()),
            );
        }
        OverlayDispatchRequest::Hide { owner } => {
            let _ = connection.call_method(
                Some(destination.as_str()),
                PATH,
                Some(IFACE),
                "HideCursorFor",
                &(owner.as_str(),),
            );
        }
        OverlayDispatchRequest::Remove { owner } => {
            let _ = connection.call_method(
                Some(destination.as_str()),
                PATH,
                Some(IFACE),
                "RemoveCursor",
                &(owner.as_str(),),
            );
        }
    }
}

fn prepare_overlay_dispatch(
    targets: &mut HashMap<String, u64>,
    request: &OverlayDispatchRequest,
) -> bool {
    match request {
        OverlayDispatchRequest::Pin { owner, window_id } => {
            targets.insert(owner.clone(), *window_id);
            false
        }
        OverlayDispatchRequest::Move { owner, .. }
        | OverlayDispatchRequest::ClickPulse { owner, .. }
        | OverlayDispatchRequest::SetColor { owner, .. }
        | OverlayDispatchRequest::SetState { owner, .. }
        | OverlayDispatchRequest::SetSessionLabel { owner, .. }
        | OverlayDispatchRequest::Hide { owner } => targets.contains_key(owner),
        OverlayDispatchRequest::Remove { owner } => {
            targets.remove(owner);
            true
        }
    }
}

pub fn start_overlay_dispatcher() {
    let _ = OVERLAY_DISPATCH_TX.get_or_init(|| {
        let mut connection: Option<zbus::blocking::Connection> = None;
        let mut helper_compatible: Option<(Instant, bool)> = None;
        let mut targets = HashMap::new();
        spawn_overlay_dispatcher(move |request| {
            if !prepare_overlay_dispatch(&mut targets, &request) {
                return;
            }
            if tokio::runtime::Handle::try_current().is_ok() {
                tracing::error!(
                    request = request.kind(),
                    "refusing blocking GNOME cursor dispatch on a Tokio runtime"
                );
                return;
            }
            let now = Instant::now();
            let compatible = match helper_compatible {
                Some((checked_at, compatible))
                    if now.duration_since(checked_at) < OVERLAY_HELPER_PROBE_TTL =>
                {
                    compatible
                }
                _ => {
                    let compatible = available();
                    helper_compatible = Some((now, compatible));
                    compatible
                }
            };
            if !compatible {
                return;
            }
            if connection.is_none() {
                connection = zbus::blocking::Connection::session().ok();
            }
            if let Some(connection) = connection.as_ref() {
                dispatch_overlay_request(connection, &targets, request);
            }
        })
    });
}

fn try_enqueue_overlay_request(
    sender: &std::sync::mpsc::SyncSender<OverlayDispatchRequest>,
    request: OverlayDispatchRequest,
) -> bool {
    match sender.try_send(request) {
        Ok(()) => true,
        Err(std::sync::mpsc::TrySendError::Full(request)) => {
            tracing::warn!(
                request = request.kind(),
                capacity = OVERLAY_DISPATCH_CAPACITY,
                "GNOME cursor dispatcher queue is full; dropping best-effort visual command"
            );
            false
        }
        Err(std::sync::mpsc::TrySendError::Disconnected(request)) => {
            tracing::warn!(
                request = request.kind(),
                "GNOME cursor dispatcher disconnected; dropping best-effort visual command"
            );
            false
        }
    }
}

fn enqueue_overlay_request(request: OverlayDispatchRequest) -> bool {
    start_overlay_dispatcher();
    let Some(sender) = OVERLAY_DISPATCH_TX.get().and_then(Option::as_ref) else {
        tracing::warn!(
            request = request.kind(),
            "GNOME cursor dispatcher is unavailable"
        );
        return false;
    };
    try_enqueue_overlay_request(sender, request)
}

#[derive(Debug, Clone, serde::Deserialize)]
struct Capabilities {
    protocol_version: u64,
    epoch: String,
    #[serde(default)]
    capabilities: Vec<String>,
}

#[derive(Debug, Clone)]
struct ShellWindow {
    public_id: u64,
    native_id: u64,
    target_id: String,
    helper_epoch: String,
    transient_for_window_id: Option<u64>,
    transient_for_target_id: Option<String>,
    is_attached_dialog: Option<bool>,
    is_modal: Option<bool>,
    window_type: Option<u32>,
    pid: u32,
    app_id: String,
    title: String,
    x: i32,
    y: i32,
    buffer_x: i32,
    buffer_y: i32,
    width: u32,
    height: u32,
    visible: bool,
    capture_current: bool,
    minimized: bool,
    z_index: Option<usize>,
    workspace_index: Option<i32>,
    workspace_active: Option<bool>,
    sticky: Option<bool>,
    monitor: Option<i32>,
}

#[derive(Debug, serde::Deserialize)]
struct CaptureRect {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

#[derive(Debug, serde::Deserialize)]
struct CaptureLogicalSize {
    width: u32,
    height: u32,
}

#[derive(Debug, serde::Deserialize)]
struct CapturePayload {
    protocol_version: u64,
    target: String,
    rect: CaptureRect,
    logical_size: CaptureLogicalSize,
    png_base64: String,
}

#[derive(Debug)]
pub struct ForegroundTransaction {
    token: String,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ForegroundTerminalOutcome {
    pub terminal: bool,
    pub state: String,
    #[serde(default)]
    pub activation_required: Option<bool>,
    #[serde(default)]
    pub target_activation_verified: Option<bool>,
    #[serde(default)]
    pub restoration_attempted: Option<bool>,
    #[serde(default)]
    pub restoration_succeeded: Option<bool>,
    #[serde(default)]
    pub outcome: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

impl ForegroundTransaction {
    pub fn validate(&self) -> anyhow::Result<()> {
        let raw = gdbus_call_with_timeout(
            "ValidateForeground",
            &[gvariant_string(&self.token)],
            Duration::from_secs(2),
        )
        .ok_or_else(|| anyhow::anyhow!("foreground_unavailable: validation timed out"))?;
        let valid = extract_json_object(&raw)
            .and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok())
            .and_then(|value| value.get("valid").and_then(serde_json::Value::as_bool))
            .unwrap_or(false);
        if !valid {
            anyhow::bail!("stale_transaction: WinRects rejected foreground validation");
        }
        Ok(())
    }

    pub fn finish(mut self) -> anyhow::Result<ForegroundTerminalOutcome> {
        let token = std::mem::take(&mut self.token);
        set_pending_foreground(Some(token.clone()));
        let outcome = terminalize_foreground("EndForeground", &token)?;
        set_pending_foreground(None);
        Ok(outcome)
    }

    pub fn commit(mut self) -> anyhow::Result<()> {
        let token = std::mem::take(&mut self.token);
        set_pending_foreground(Some(token.clone()));
        let raw = gdbus_call_with_timeout(
            "CommitForeground",
            &[gvariant_string(&token)],
            Duration::from_secs(2),
        )
        .ok_or_else(|| anyhow::anyhow!("foreground_unavailable: commit timed out"))?;
        let committed = extract_json_object(&raw)
            .and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok())
            .and_then(|value| value.get("committed").and_then(serde_json::Value::as_bool))
            .unwrap_or(false);
        if !committed {
            anyhow::bail!("stale_transaction: WinRects rejected foreground commit");
        }
        set_pending_foreground(None);
        Ok(())
    }
}

impl Drop for ForegroundTransaction {
    fn drop(&mut self) {
        if !self.token.is_empty() {
            let token = std::mem::take(&mut self.token);
            set_pending_foreground(Some(token.clone()));
            if terminalize_foreground("AbortForeground", &token).is_ok() {
                set_pending_foreground(None);
            }
        }
    }
}

pub fn available() -> bool {
    // GetCapabilities is the exact-target contract. Do not require the v8
    // semantic-cursor GetVersion here: GNOME cannot safely reload extensions
    // in place, so a still-loaded protocol-v4 helper must remain usable until
    // the next login after updated files are staged.
    shell_owner(false).is_some()
        && capabilities().is_some_and(|capabilities| {
            capabilities.protocol_version == REQUIRED_EXACT_TARGET_PROTOCOL
                && !capabilities.epoch.is_empty()
                && capabilities
                    .capabilities
                    .iter()
                    .any(|capability| capability == "exact-target-v2")
                && capabilities
                    .capabilities
                    .iter()
                    .any(|capability| capability == "transient-parent-v1")
                && capabilities
                    .capabilities
                    .iter()
                    .any(|capability| capability == "foreground-revalidate-v1")
                && capabilities
                    .capabilities
                    .iter()
                    .any(|capability| capability == "foreground-reconcile-v1")
                && capabilities
                    .capabilities
                    .iter()
                    .any(|capability| capability == "unoccluded-target-v1")
                && capabilities
                    .capabilities
                    .iter()
                    .any(|capability| capability == "trusted-cursor-overlay-v1")
                && capabilities
                    .capabilities
                    .iter()
                    .any(|capability| capability == "exact-target-activation-v1")
                && capabilities
                    .capabilities
                    .iter()
                    .any(|capability| capability == "shell-grab-classification-v1")
                && capabilities
                    .capabilities
                    .iter()
                    .any(|capability| capability == "atomic-target-capture-v1")
                && capabilities
                    .capabilities
                    .iter()
                    .any(|capability| capability == "connection-owned-cursors-v1")
        })
}

fn exact_identity_capabilities() -> Option<Vec<String>> {
    Some(
        [
            "exact-target-v2",
            "transient-parent-v1",
            "foreground-revalidate-v1",
            "foreground-reconcile-v1",
            "unoccluded-target-v1",
            "trusted-cursor-overlay-v1",
            "exact-target-activation-v1",
            "shell-grab-classification-v1",
            "atomic-target-capture-v1",
            "connection-owned-cursors-v1",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
    )
}

/// Whether a WinRects D-Bus object is present at all, including an older or
/// otherwise incompatible protocol. This lets callers fail closed on skew
/// instead of silently dropping to non-incarnation-aware GNOME behavior.
pub fn present() -> bool {
    gdbus_call("GetRects", &[]).is_some()
}

fn capabilities() -> Option<Capabilities> {
    let raw = gdbus_call("GetCapabilities", &[])?;
    let json = extract_json_object(&raw)?;
    serde_json::from_str(json).ok()
}

fn extract_json_object(raw: &str) -> Option<&str> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    (end >= start).then_some(&raw[start..=end])
}

fn gvariant_string(value: &str) -> String {
    serde_json::to_string(value).expect("serializing a string cannot fail")
}

/// Stable public Linux window ID derived from the full helper-incarnation
/// target. FNV-1a keeps the existing integer contract while ensuring a helper
/// restart maps the same native stable sequence into a different ID.
fn public_window_id(target_id: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in target_id.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash.max(1)
}

fn gdbus_call(method: &str, args: &[String]) -> Option<String> {
    gdbus_call_with_timeout(method, args, Duration::from_millis(800))
}

fn gdbus_call_with_timeout(method: &str, args: &[String], timeout: Duration) -> Option<String> {
    let owner = shell_owner(false)?;
    gdbus_call_target(&owner, PATH, &format!("{IFACE}.{method}"), args, timeout)
}

/// Resolve the helper's immutable unique bus name and prove that it is hosted
/// by this user's system-installed GNOME Shell process. Protocol-sensitive
/// callers can additionally require the extension's current API. Addressing
/// the unique name closes the race where another process replaces the public
/// name after ownership is checked.
fn shell_owner(require_helper_api: bool) -> Option<String> {
    let owner_raw = gdbus_call_target(
        DBUS_DEST,
        DBUS_PATH,
        &format!("{DBUS_IFACE}.GetNameOwner"),
        &[DEST.to_owned()],
        Duration::from_millis(800),
    )?;
    let owner = parse_quoted_string(&owner_raw)?;
    if !owner.starts_with(':') {
        return None;
    }

    let pid_raw = gdbus_call_target(
        DBUS_DEST,
        DBUS_PATH,
        &format!("{DBUS_IFACE}.GetConnectionUnixProcessID"),
        &[owner.clone()],
        Duration::from_millis(800),
    )?;
    let uid_raw = gdbus_call_target(
        DBUS_DEST,
        DBUS_PATH,
        &format!("{DBUS_IFACE}.GetConnectionUnixUser"),
        &[owner.clone()],
        Duration::from_millis(800),
    )?;
    let pid = parse_first_u32(&pid_raw)?;
    let uid = parse_first_u32(&uid_raw)?;
    if uid != current_uid() || !is_trusted_gnome_shell(pid) {
        return None;
    }

    if require_helper_api {
        let version_raw = gdbus_call_target(
            &owner,
            PATH,
            &format!("{IFACE}.GetVersion"),
            &[],
            Duration::from_millis(800),
        )?;
        if parse_first_u32(&version_raw)? < REQUIRED_HELPER_API_VERSION {
            return None;
        }
    }
    Some(owner)
}

fn parse_quoted_string(raw: &str) -> Option<String> {
    let start = raw.find('\'')? + 1;
    let end = raw[start..].find('\'')? + start;
    (end > start).then(|| raw[start..end].to_owned())
}

fn parse_first_u32(raw: &str) -> Option<u32> {
    // `gdbus call` renders typed scalars as `(uint32 6079,)`. Searching the
    // whole string would incorrectly return the `32` in the type annotation.
    let payload = raw.split_once("uint32").map_or(raw, |(_, payload)| payload);
    payload
        .split(|character: char| !character.is_ascii_digit())
        .find(|part| !part.is_empty())?
        .parse()
        .ok()
}

fn current_uid() -> u32 {
    std::fs::metadata("/proc/self")
        .map(|meta| meta.uid())
        .unwrap_or(u32::MAX)
}

fn is_trusted_gnome_shell(pid: u32) -> bool {
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok();
    if comm.as_deref().map(str::trim) != Some("gnome-shell") {
        return false;
    }
    let executable = std::fs::read_link(format!("/proc/{pid}/exe")).ok();
    let metadata = executable
        .as_ref()
        .and_then(|path| std::fs::metadata(path).ok());
    executable
        .as_ref()
        .and_then(|path| path.file_name())
        .and_then(|name| name.to_str())
        == Some("gnome-shell")
        && metadata
            .as_ref()
            .is_some_and(|meta| meta.uid() == 0 && meta.permissions().mode() & 0o022 == 0)
}

fn gdbus_call_target(
    dest: &str,
    path: &str,
    method: &str,
    args: &[String],
    timeout: Duration,
) -> Option<String> {
    let mut cmd = Command::new("gdbus");
    cmd.arg("call")
        .arg("--session")
        .arg("--dest")
        .arg(dest)
        .arg("--object-path")
        .arg(path)
        .arg("--method")
        .arg(method);
    for a in args {
        cmd.arg(a);
    }
    // gdbus is local IPC; cap it so a wedged shell can't stall the caller.
    let child = cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let out = wait_timeout(child, timeout)?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Capture one exact GNOME target using the rectangle and logical display
/// dimensions returned by the same Shell transaction as the target-only pixels.
pub fn screenshot_window(window_id: u64) -> anyhow::Result<Vec<u8>> {
    let target = resolve_target(window_id).ok_or_else(|| {
        anyhow::anyhow!(
            "stale_target: GNOME window {window_id} belongs to another helper incarnation or no longer exists"
        )
    })?;
    if !target.capture_current {
        anyhow::bail!(
            "capture_foreground_required: GNOME window {window_id} is not currently painted on the active stage"
        );
    }
    let raw = gdbus_call_with_timeout(
        "CaptureTarget",
        &[gvariant_string(&target.target_id)],
        Duration::from_secs(5),
    )
    .ok_or_else(|| anyhow::anyhow!("GNOME exact-target capture failed for window {window_id}"))?;
    decode_capture_payload(&raw, &target.target_id)
}

fn decode_capture_payload(raw: &str, expected_target: &str) -> anyhow::Result<Vec<u8>> {
    use base64::{engine::general_purpose::STANDARD as B64, Engine as _};

    let payload = extract_json_object(&raw)
        .and_then(|json| serde_json::from_str::<CapturePayload>(json).ok())
        .ok_or_else(|| anyhow::anyhow!("GNOME capture returned an invalid atomic payload"))?;
    if payload.protocol_version != REQUIRED_EXACT_TARGET_PROTOCOL
        || payload.target != expected_target
    {
        anyhow::bail!("target_changed_during_capture: GNOME capture proof did not match request");
    }
    if payload.rect.width == 0
        || payload.rect.height == 0
        || payload.logical_size.width == 0
        || payload.logical_size.height == 0
        || payload.rect.x < 0
        || payload.rect.y < 0
        || u32::try_from(payload.rect.x)
            .ok()
            .and_then(|x| x.checked_add(payload.rect.width))
            .is_none_or(|right| right > payload.logical_size.width)
        || u32::try_from(payload.rect.y)
            .ok()
            .and_then(|y| y.checked_add(payload.rect.height))
            .is_none_or(|bottom| bottom > payload.logical_size.height)
    {
        anyhow::bail!("capture_geometry_invalid: GNOME returned an unsafe target rectangle");
    }
    let target_png = B64
        .decode(payload.png_base64)
        .map_err(|error| anyhow::anyhow!("GNOME capture returned invalid base64: {error}"))?;
    let image = image::load_from_memory(&target_png)
        .map_err(|error| anyhow::anyhow!("GNOME exact-target PNG is invalid: {error}"))?;
    if image.width() < payload.rect.width
        || image.height() < payload.rect.height
        || image.width() > payload.rect.width.saturating_mul(8)
        || image.height() > payload.rect.height.saturating_mul(8)
    {
        anyhow::bail!(
            "capture_geometry_invalid: GNOME target PNG dimensions {}x{} do not match logical rectangle {}x{}",
            image.width(),
            image.height(),
            payload.rect.width,
            payload.rect.height
        );
    }
    Ok(target_png)
}

/// GNOME's logical desktop size. Shell screenshots are encoded in physical
/// pixels, while MetaWindow frame rectangles use these logical coordinates.
pub fn logical_screen_size() -> Option<(u32, u32)> {
    let raw = gdbus_call_target(
        INTROSPECT_DEST,
        INTROSPECT_PATH,
        "org.freedesktop.DBus.Properties.Get",
        &[INTROSPECT_IFACE.to_owned(), "ScreenSize".to_owned()],
        Duration::from_millis(800),
    )?;
    parse_screen_size(&raw)
}

fn parse_screen_size(raw: &str) -> Option<(u32, u32)> {
    let mut values = raw
        .split(|character: char| !character.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse::<u32>().ok());
    let width = values.next()?;
    let height = values.next()?;
    (width > 0 && height > 0).then_some((width, height))
}

/// `Child::wait` with a deadline (no extra crates). Kills + reaps on timeout.
fn wait_timeout(mut child: std::process::Child, dur: Duration) -> Option<std::process::Output> {
    use std::io::Read;

    // Drain stdout while the child is running. Capture() returns a base64 PNG
    // that readily exceeds a pipe's ~64 KiB capacity; waiting for exit before
    // reading deadlocks the child on a full pipe and turns a healthy Shell
    // response into a false timeout.
    let stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut stdout = stdout;
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).ok()?;
        Some(bytes)
    });
    let deadline = std::time::Instant::now() + dur;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let status = child.wait().ok()?;
                    let _ = reader.join();
                    if !status.success() {
                        return None;
                    }
                    return None;
                }
                std::thread::sleep(Duration::from_millis(15));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return None;
            }
        }
    };
    let stdout = reader.join().ok().flatten()?;
    Some(std::process::Output {
        status,
        stdout,
        stderr: Vec::new(),
    })
}

/// Screen origin of the Wayland surface buffer backing `pid`.
///
/// GTK's AT-SPI `CoordType::Window` includes client-side shadow extents, while
/// Mutter's frame rectangle excludes them. The buffer origin preserves those
/// extents so accessibility frames line up with pixels. Older helpers omit the
/// buffer fields and fall back to the frame origin.
pub fn window_origin_for_pid(pid: u32) -> Option<(i32, i32)> {
    let matches = read_shell_windows()?
        .into_iter()
        .filter(|window| window.pid == pid)
        .collect::<Vec<_>>();
    (matches.len() == 1).then(|| (matches[0].buffer_x, matches[0].buffer_y))
}

/// Enumerate GNOME Shell toplevels when the compositor helper is available.
///
/// AT-SPI remains the source of accessibility elements, but it is a poor
/// source of truth for desktop window discovery: one unresponsive application
/// can exhaust the bounded registry walk and hide every healthy toplevel. The
/// shell already owns the authoritative stacking list, geometry, visibility,
/// title, and PID, so use that metadata directly for `list_windows`.
pub fn list_windows(filter_pid: Option<u32>) -> Option<Vec<WindowInfo>> {
    let windows = read_shell_windows()?;
    Some(
        windows
            .into_iter()
            .filter(|window| filter_pid.is_none_or(|wanted| wanted == window.pid))
            .map(|window| WindowInfo {
                xid: window.public_id,
                pid: Some(window.pid),
                app_name: window.app_id,
                title: window.title,
                is_on_screen: window.visible
                    && !window.minimized
                    && window.width > 0
                    && window.height > 0,
                z_index: window.z_index,
                x: window.x,
                y: window.y,
                width: window.width,
                height: window.height,
                native_window_id: Some(window.native_id),
                target_id: Some(window.target_id),
                helper_epoch: Some(window.helper_epoch),
                transient_for_window_id: window.transient_for_window_id,
                transient_for_target_id: window.transient_for_target_id,
                is_attached_dialog: window.is_attached_dialog,
                is_modal: window.is_modal,
                window_type: window.window_type,
                workspace_index: window.workspace_index,
                workspace_active: window.workspace_active,
                sticky: window.sticky,
                monitor: window.monitor,
                capture_current: Some(window.capture_current),
                identity_capabilities: exact_identity_capabilities(),
            })
            .collect(),
    )
}

/// Return one compositor-attested GNOME window only when the exact public id
/// still belongs to the approved process.
pub fn trusted_window_for_id(pid: u32, window_id: u64) -> Option<WindowInfo> {
    list_windows(Some(pid))?
        .into_iter()
        .find(|window| window.xid == window_id)
}

/// Enumerate incarnation-qualified window ids for one approved process.
pub fn trusted_window_ids_for_pid(pid: u32) -> Option<Vec<u64>> {
    Some(
        list_windows(Some(pid))?
            .into_iter()
            .map(|window| window.xid)
            .collect(),
    )
}

/// Execute one bounded operation against an exact GNOME target, then restore
/// and verify the prior compositor context even when the operation fails.
pub fn with_focused_window<T>(
    pid: u32,
    window_id: u64,
    body: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    trusted_window_for_id(pid, window_id)
        .ok_or_else(|| anyhow::anyhow!("no exact GNOME Shell window owns the approved target"))?;
    let transaction = begin_foreground(window_id)?;
    let action = transaction.validate().and_then(|()| body());
    let restoration = transaction.finish();
    match (action, restoration) {
        (Ok(value), Ok(_)) => Ok(value),
        (Err(action_error), Ok(_)) => Err(action_error),
        (Ok(_), Err(restoration_error)) => Err(restoration_error),
        (Err(action_error), Err(restoration_error)) => Err(anyhow::anyhow!(
            "{action_error}; foreground reconciliation also failed: {restoration_error}"
        )),
    }
}

/// The local-hardened helper never exports compositor-wide pixels. Upstream's
/// video backend treats `None` as an instruction to use the platform recorder;
/// preserving that fallback avoids weakening exact-target capture policy.
pub fn trusted_screenshot_display() -> Option<Vec<u8>> {
    None
}

pub fn begin_foreground(window_id: u64) -> anyhow::Result<ForegroundTransaction> {
    let target = resolve_target(window_id).ok_or_else(|| {
        anyhow::anyhow!(
            "stale_target: GNOME window {window_id} belongs to another helper incarnation or no longer exists"
        )
    })?;
    let token = new_foreground_token();
    set_pending_foreground(Some(token.clone()));
    let raw = gdbus_call_with_timeout(
        "BeginForeground",
        &[gvariant_string(&token), gvariant_string(&target.target_id)],
        Duration::from_secs(2),
    );
    let Some(raw) = raw else {
        let recovery = terminalize_foreground("AbortForeground", &token);
        if recovery.is_ok() {
            set_pending_foreground(None);
        }
        anyhow::bail!(
            "foreground_unavailable: GNOME BeginForeground timed out for exact window {window_id}; reconciliation {}",
            if recovery.is_ok() { "reached a terminal state" } else { "remains required" }
        );
    };
    let payload = extract_json_object(&raw)
        .and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok())
        .ok_or_else(|| anyhow::anyhow!("foreground_unavailable: invalid WinRects response"))?;
    if payload
        .get("activated")
        .and_then(serde_json::Value::as_bool)
        != Some(true)
    {
        let reason = payload
            .get("reason")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("activation_not_confirmed");
        let recovery = terminalize_foreground("AbortForeground", &token);
        if recovery.is_ok() {
            set_pending_foreground(None);
        }
        anyhow::bail!(
            "foreground_unavailable: WinRects did not confirm exact window {window_id} activation ({reason}); reconciliation {}",
            if recovery.is_ok() { "reached a terminal state" } else { "remains required" }
        );
    }
    let returned_token = payload
        .get("transaction")
        .and_then(serde_json::Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| anyhow::anyhow!("foreground_unavailable: missing WinRects transaction"))?;
    if returned_token != token {
        anyhow::bail!("foreground_unavailable: WinRects returned a mismatched transaction ID");
    }
    set_pending_foreground(None);
    Ok(ForegroundTransaction { token })
}

fn new_foreground_token() -> String {
    let sequence = FOREGROUND_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("cua-fg-{}-{nanos:x}-{sequence:x}", std::process::id())
}

fn set_pending_foreground(token: Option<String>) {
    if let Ok(mut pending) = PENDING_FOREGROUND.lock() {
        *pending = token;
    }
}

fn parse_foreground_outcome(raw: &str) -> anyhow::Result<ForegroundTerminalOutcome> {
    let outcome = extract_json_object(raw)
        .and_then(|json| serde_json::from_str::<ForegroundTerminalOutcome>(json).ok())
        .ok_or_else(|| {
            anyhow::anyhow!("foreground_recovery_required: invalid terminal response")
        })?;
    if !outcome.terminal || outcome.state != "terminal" {
        anyhow::bail!("foreground_recovery_required: helper restoration is not terminal");
    }
    if outcome.outcome.as_deref() == Some("restoration_unresolved") {
        anyhow::bail!("foreground_recovery_required: helper could not verify restoration");
    }
    Ok(outcome)
}

fn terminalize_foreground(method: &str, token: &str) -> anyhow::Result<ForegroundTerminalOutcome> {
    if token.is_empty() {
        anyhow::bail!("foreground_recovery_required: missing transaction ID");
    }
    let raw = gdbus_call_with_timeout(method, &[gvariant_string(token)], Duration::from_secs(2))
        .ok_or_else(|| anyhow::anyhow!("foreground_recovery_required: {method} timed out"))?;
    parse_foreground_outcome(&raw)
}

pub fn reconcile_orphaned_foreground() -> anyhow::Result<()> {
    let local_pending = PENDING_FOREGROUND
        .lock()
        .ok()
        .and_then(|pending| pending.clone());
    let raw = gdbus_call_with_timeout(
        "QueryForeground",
        &[gvariant_string(local_pending.as_deref().unwrap_or(""))],
        Duration::from_secs(2),
    )
    .ok_or_else(|| anyhow::anyhow!("foreground_recovery_required: QueryForeground timed out"))?;
    let payload = extract_json_object(&raw)
        .and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok())
        .ok_or_else(|| {
            anyhow::anyhow!("foreground_recovery_required: invalid QueryForeground response")
        })?;
    if payload.get("terminal").and_then(serde_json::Value::as_bool) == Some(true) {
        set_pending_foreground(None);
        return Ok(());
    }
    let token = payload
        .get("transaction")
        .and_then(serde_json::Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("foreground_recovery_required: active helper transaction has no ID")
        })?;
    terminalize_foreground("AbortForeground", token)?;
    set_pending_foreground(None);
    Ok(())
}

#[cfg(test)]
fn parse_windows(raw: &str, filter_pid: Option<u32>) -> Option<Vec<WindowInfo>> {
    let windows = parse_shell_windows(raw)?;
    Some(
        windows
            .into_iter()
            .filter(|window| filter_pid.is_none_or(|wanted| wanted == window.pid))
            .map(|window| WindowInfo {
                xid: window.public_id,
                pid: Some(window.pid),
                app_name: window.app_id,
                title: window.title,
                is_on_screen: window.visible
                    && !window.minimized
                    && window.width > 0
                    && window.height > 0,
                z_index: window.z_index,
                x: window.x,
                y: window.y,
                width: window.width,
                height: window.height,
                native_window_id: Some(window.native_id),
                target_id: Some(window.target_id),
                helper_epoch: Some(window.helper_epoch),
                transient_for_window_id: window.transient_for_window_id,
                transient_for_target_id: window.transient_for_target_id,
                is_attached_dialog: window.is_attached_dialog,
                is_modal: window.is_modal,
                window_type: window.window_type,
                workspace_index: window.workspace_index,
                workspace_active: window.workspace_active,
                sticky: window.sticky,
                monitor: window.monitor,
                capture_current: Some(window.capture_current),
                identity_capabilities: exact_identity_capabilities(),
            })
            .collect(),
    )
}

fn read_shell_windows() -> Option<Vec<ShellWindow>> {
    if !available() {
        return None;
    }
    parse_shell_windows(&gdbus_call("GetRects", &[])?)
}

fn resolve_target(window_id: u64) -> Option<ShellWindow> {
    read_shell_windows()?
        .into_iter()
        .find(|window| window.public_id == window_id)
}

fn parse_shell_windows(raw: &str) -> Option<Vec<ShellWindow>> {
    let start = raw.find('[')?;
    let end = raw.rfind(']')?;
    let windows: Vec<serde_json::Value> = serde_json::from_str(&raw[start..=end]).ok()?;
    let mut ids = HashMap::<u64, String>::new();
    let mut parsed = Vec::with_capacity(windows.len());
    for window in windows {
        let protocol = window.get("protocol_version")?.as_u64()?;
        if protocol != REQUIRED_EXACT_TARGET_PROTOCOL {
            return None;
        }
        let native_id = window.get("id")?.as_u64()?;
        if native_id == 0 {
            return None;
        }
        let helper_epoch = window.get("helper_epoch")?.as_str()?.to_owned();
        let target_id = window.get("target_id")?.as_str()?.to_owned();
        if helper_epoch.is_empty() || target_id != format!("{helper_epoch}:{native_id}") {
            return None;
        }
        let public_id = public_window_id(&target_id);
        if ids
            .insert(public_id, target_id.clone())
            .is_some_and(|previous| previous != target_id)
        {
            return None;
        }
        let x = i32::try_from(window.get("x")?.as_i64()?).ok()?;
        let y = i32::try_from(window.get("y")?.as_i64()?).ok()?;
        let buffer_x = window
            .get("buffer_x")
            .and_then(serde_json::Value::as_i64)
            .and_then(|value| i32::try_from(value).ok())
            .unwrap_or(x);
        let buffer_y = window
            .get("buffer_y")
            .and_then(serde_json::Value::as_i64)
            .and_then(|value| i32::try_from(value).ok())
            .unwrap_or(y);
        let width = u32::try_from(window.get("w")?.as_u64()?).ok()?;
        let height = u32::try_from(window.get("h")?.as_u64()?).ok()?;
        let transient_for_target_id = match window.get("transient_for_target_id") {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::String(value)) if !value.is_empty() => Some(value.clone()),
            _ => return None,
        };
        if transient_for_target_id.as_deref() == Some(target_id.as_str()) {
            return None;
        }
        let transient_for_window_id = transient_for_target_id.as_deref().map(public_window_id);
        parsed.push(ShellWindow {
            public_id,
            native_id,
            target_id,
            helper_epoch,
            transient_for_window_id,
            transient_for_target_id,
            is_attached_dialog: window
                .get("is_attached_dialog")
                .and_then(serde_json::Value::as_bool),
            is_modal: window.get("is_modal").and_then(serde_json::Value::as_bool),
            window_type: window
                .get("window_type")
                .and_then(serde_json::Value::as_u64)
                .and_then(|value| u32::try_from(value).ok()),
            pid: u32::try_from(window.get("pid")?.as_u64()?).ok()?,
            app_id: window
                .get("app_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            title: window
                .get("title")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            x,
            y,
            buffer_x,
            buffer_y,
            width,
            height,
            visible: window
                .get("visible")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            capture_current: window
                .get("capture_current")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            minimized: window
                .get("minimized")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            z_index: window
                .get("stacking")
                .and_then(serde_json::Value::as_u64)
                .and_then(|value| usize::try_from(value).ok()),
            workspace_index: window
                .get("workspace_index")
                .and_then(serde_json::Value::as_i64)
                .and_then(|value| i32::try_from(value).ok()),
            workspace_active: window
                .get("workspace_active")
                .and_then(serde_json::Value::as_bool),
            sticky: window.get("sticky").and_then(serde_json::Value::as_bool),
            monitor: window
                .get("monitor")
                .and_then(serde_json::Value::as_i64)
                .and_then(|value| i32::try_from(value).ok()),
        });
    }
    let target_ids: HashSet<&str> = parsed
        .iter()
        .map(|window| window.target_id.as_str())
        .collect();
    if parsed.iter().any(|window| {
        window
            .transient_for_target_id
            .as_deref()
            .is_some_and(|parent| {
                !parent.starts_with(&format!("{}:", window.helper_epoch))
                    || !target_ids.contains(parent)
            })
    }) {
        return None;
    }
    Some(parsed)
}

pub fn pin_cursor(owner: &str, window_id: u64) -> bool {
    enqueue_overlay_request(OverlayDispatchRequest::Pin {
        owner: owner.to_owned(),
        window_id,
    })
}

pub fn move_cursor(owner: &str, x: i32, y: i32) -> bool {
    enqueue_overlay_request(OverlayDispatchRequest::Move {
        owner: owner.to_owned(),
        x,
        y,
    })
}

pub fn click_pulse(owner: &str, x: i32, y: i32) -> bool {
    enqueue_overlay_request(OverlayDispatchRequest::ClickPulse {
        owner: owner.to_owned(),
        x,
        y,
    })
}

pub fn set_cursor_color(owner: &str, fill_color: &str) -> bool {
    enqueue_overlay_request(OverlayDispatchRequest::SetColor {
        owner: owner.to_owned(),
        fill_color: fill_color.to_owned(),
    })
}

pub fn set_cursor_state(
    owner: &str,
    action: &str,
    delivery: &str,
    target: &str,
    active: bool,
) -> bool {
    enqueue_overlay_request(OverlayDispatchRequest::SetState {
        owner: owner.to_owned(),
        action: action.to_owned(),
        delivery: delivery.to_owned(),
        target: target.to_owned(),
        active,
    })
}

pub fn set_session_label(owner: &str, label: &str) -> bool {
    enqueue_overlay_request(OverlayDispatchRequest::SetSessionLabel {
        owner: owner.to_owned(),
        label: label.to_owned(),
    })
}

pub fn hide_cursor(owner: &str) {
    let _ = enqueue_overlay_request(OverlayDispatchRequest::Hide {
        owner: owner.to_owned(),
    });
}

pub fn remove_cursor(owner: &str) {
    let _ = enqueue_overlay_request(OverlayDispatchRequest::Remove {
        owner: owner.to_owned(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dbus_owner_and_numeric_identity() {
        assert_eq!(
            parse_quoted_string("(':1.204',)"),
            Some(":1.204".to_owned())
        );
        assert_eq!(parse_first_u32("(uint32 6079,)"), Some(6079));
        assert_eq!(parse_first_u32("(uint32 4,)"), Some(4));
        assert_eq!(parse_first_u32("(6079,)"), Some(6079));
        assert_eq!(parse_quoted_string("(nothing,)"), None);
        assert_eq!(parse_first_u32("(nothing,)"), None);
    }

    #[test]
    fn exact_identity_records_advertise_all_delivery_invariants() {
        assert_eq!(
            exact_identity_capabilities().unwrap(),
            vec![
                "exact-target-v2".to_owned(),
                "transient-parent-v1".to_owned(),
                "foreground-revalidate-v1".to_owned(),
                "foreground-reconcile-v1".to_owned(),
                "unoccluded-target-v1".to_owned(),
                "trusted-cursor-overlay-v1".to_owned(),
                "exact-target-activation-v1".to_owned(),
                "shell-grab-classification-v1".to_owned(),
                "atomic-target-capture-v1".to_owned(),
                "connection-owned-cursors-v1".to_owned(),
            ]
        );
    }

    #[test]
    fn overlay_dispatcher_runs_outside_the_callers_tokio_runtime() {
        let (observed_tx, observed_rx) = std::sync::mpsc::sync_channel(1);
        let sender = spawn_overlay_dispatcher(move |_request| {
            let _ = observed_tx.send(tokio::runtime::Handle::try_current().is_err());
        })
        .expect("dispatcher thread");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");

        runtime.block_on(async {
            sender
                .try_send(OverlayDispatchRequest::Hide {
                    owner: "test-owner".to_owned(),
                })
                .expect("queue cursor request");
        });

        assert_eq!(observed_rx.recv_timeout(Duration::from_secs(1)), Ok(true));
    }

    #[test]
    fn public_overlay_enqueue_returns_from_a_current_thread_tokio_runtime() {
        let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime");
            runtime.block_on(async {
                let owner = "tokio-public-route";
                assert!(pin_cursor(owner, 1));
            });
            done_tx.send(()).expect("report completion");
        });

        assert_eq!(done_rx.recv_timeout(Duration::from_secs(1)), Ok(()));
    }

    #[test]
    fn overlay_dispatcher_preserves_fifo_order() {
        let (observed_tx, observed_rx) = std::sync::mpsc::sync_channel(5);
        let sender = spawn_overlay_dispatcher(move |request| {
            observed_tx.send(request.kind()).expect("record request");
        })
        .expect("dispatcher thread");

        for request in [
            OverlayDispatchRequest::Pin {
                owner: "test-owner".to_owned(),
                window_id: 1,
            },
            OverlayDispatchRequest::Move {
                owner: "test-owner".to_owned(),
                x: 10,
                y: 20,
            },
            OverlayDispatchRequest::ClickPulse {
                owner: "test-owner".to_owned(),
                x: 10,
                y: 20,
            },
            OverlayDispatchRequest::Hide {
                owner: "test-owner".to_owned(),
            },
            OverlayDispatchRequest::Remove {
                owner: "test-owner".to_owned(),
            },
        ] {
            sender.try_send(request).expect("queue cursor request");
        }

        let observed: Vec<_> = (0..5)
            .map(|_| {
                observed_rx
                    .recv_timeout(Duration::from_secs(1))
                    .expect("receive cursor request")
            })
            .collect();
        assert_eq!(observed, ["pin", "move", "click_pulse", "hide", "remove"]);
    }

    #[test]
    fn overlay_dispatcher_owns_ordered_target_lifecycle() {
        let owner = "test-owner".to_owned();
        let mut targets = HashMap::new();

        assert!(!prepare_overlay_dispatch(
            &mut targets,
            &OverlayDispatchRequest::Pin {
                owner: owner.clone(),
                window_id: 1,
            }
        ));
        assert_eq!(targets.get(&owner), Some(&1));
        assert!(prepare_overlay_dispatch(
            &mut targets,
            &OverlayDispatchRequest::Move {
                owner: owner.clone(),
                x: 10,
                y: 20,
            }
        ));

        assert!(!prepare_overlay_dispatch(
            &mut targets,
            &OverlayDispatchRequest::Pin {
                owner: owner.clone(),
                window_id: 2,
            }
        ));
        assert_eq!(targets.get(&owner), Some(&2));
        assert!(prepare_overlay_dispatch(
            &mut targets,
            &OverlayDispatchRequest::Remove {
                owner: owner.clone(),
            }
        ));
        assert!(!prepare_overlay_dispatch(
            &mut targets,
            &OverlayDispatchRequest::Move {
                owner,
                x: 30,
                y: 40,
            }
        ));
    }

    #[test]
    fn overlay_dispatcher_queue_failures_are_nonblocking_and_explicit() {
        let (full_tx, _full_rx) = std::sync::mpsc::sync_channel(1);
        assert!(try_enqueue_overlay_request(
            &full_tx,
            OverlayDispatchRequest::Hide {
                owner: "test-owner".to_owned(),
            }
        ));
        assert!(!try_enqueue_overlay_request(
            &full_tx,
            OverlayDispatchRequest::Remove {
                owner: "test-owner".to_owned(),
            }
        ));

        let (disconnected_tx, disconnected_rx) = std::sync::mpsc::sync_channel(1);
        drop(disconnected_rx);
        assert!(!try_enqueue_overlay_request(
            &disconnected_tx,
            OverlayDispatchRequest::Remove {
                owner: "test-owner".to_owned(),
            }
        ));
    }

    #[test]
    fn parses_shell_logical_screen_size_property() {
        assert_eq!(parse_screen_size("(<(4096, 1728)>,)"), Some((4096, 1728)));
        assert_eq!(parse_screen_size("(<(0, 1728)>,)"), None);
    }

    #[test]
    fn parses_and_filters_shell_windows() {
        let raw = r#"('[{"id":46,"target_id":"epoch-a:46","helper_epoch":"epoch-a","protocol_version":4,"pid":6079,"app_id":"org.example.Editor","title":"Sentinel's window","x":66,"y":32,"buffer_x":60,"buffer_y":28,"w":958,"h":736,"focused":true,"minimized":false,"visible":true,"capture_current":true,"workspace_index":7,"workspace_active":true,"sticky":false,"monitor":1,"stacking":2},{"id":47,"target_id":"epoch-a:47","helper_epoch":"epoch-a","protocol_version":4,"pid":6080,"app_id":"org.example.Hidden","title":"Hidden","x":0,"y":0,"w":100,"h":100,"minimized":true,"visible":false,"capture_current":false,"workspace_index":11,"workspace_active":false,"sticky":false,"monitor":0,"stacking":1}]',)"#;
        let windows = parse_windows(raw, Some(6079)).expect("valid helper response");
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].xid, public_window_id("epoch-a:46"));
        assert_ne!(windows[0].xid, 46);
        assert_eq!(windows[0].native_window_id, Some(46));
        assert_eq!(windows[0].target_id.as_deref(), Some("epoch-a:46"));
        assert_eq!(windows[0].helper_epoch.as_deref(), Some("epoch-a"));
        assert_eq!(windows[0].pid, Some(6079));
        assert_eq!(windows[0].app_name, "org.example.Editor");
        assert_eq!(windows[0].title, "Sentinel's window");
        assert_eq!((windows[0].x, windows[0].y), (66, 32));
        assert_eq!((windows[0].width, windows[0].height), (958, 736));
        assert!(windows[0].is_on_screen);
        assert_eq!(windows[0].capture_current, Some(true));
        assert_eq!(windows[0].workspace_index, Some(7));
        assert_eq!(windows[0].workspace_active, Some(true));
        assert_eq!(windows[0].monitor, Some(1));
        assert_eq!(windows[0].z_index, Some(2));
    }

    #[test]
    fn marks_minimized_shell_windows_off_screen() {
        let raw = r#"('[{"id":47,"target_id":"epoch-a:47","helper_epoch":"epoch-a","protocol_version":4,"pid":6080,"app_id":"org.example.Hidden","title":"Hidden","x":0,"y":0,"w":100,"h":100,"minimized":true,"visible":false,"capture_current":false,"stacking":1}]',)"#;
        let windows = parse_windows(raw, None).expect("valid helper response");
        assert_eq!(windows.len(), 1);
        assert!(!windows[0].is_on_screen);
        assert_eq!(windows[0].capture_current, Some(false));
    }

    #[test]
    fn helper_epoch_changes_public_window_identity() {
        assert_ne!(
            public_window_id("epoch-a:46"),
            public_window_id("epoch-b:46")
        );
    }

    #[test]
    fn preserves_only_proven_transient_parent_relationships() {
        let valid = r#"('[{"id":46,"target_id":"epoch-a:46","helper_epoch":"epoch-a","protocol_version":4,"pid":6079,"app_id":"org.example.Editor","title":"Parent","x":0,"y":0,"w":100,"h":100},{"id":47,"target_id":"epoch-a:47","helper_epoch":"epoch-a","protocol_version":4,"pid":6080,"app_id":"org.example.Dialog","title":"Chooser","x":10,"y":10,"w":80,"h":80,"transient_for_target_id":"epoch-a:46","is_attached_dialog":true,"is_modal":true,"window_type":4}]',)"#;
        let windows = parse_windows(valid, Some(6080)).expect("valid transient relationship");
        assert_eq!(windows.len(), 1);
        assert_eq!(
            windows[0].transient_for_window_id,
            Some(public_window_id("epoch-a:46"))
        );
        assert_eq!(
            windows[0].transient_for_target_id.as_deref(),
            Some("epoch-a:46")
        );
        assert_eq!(windows[0].is_attached_dialog, Some(true));
        assert_eq!(windows[0].is_modal, Some(true));
        assert_eq!(windows[0].window_type, Some(4));

        let missing_parent =
            valid.replace("epoch-a:46\",\"is_attached", "epoch-a:99\",\"is_attached");
        assert!(parse_windows(&missing_parent, None).is_none());

        let self_parent = valid.replace("epoch-a:46\",\"is_attached", "epoch-a:47\",\"is_attached");
        assert!(parse_windows(&self_parent, None).is_none());
    }

    #[test]
    fn rejects_unversioned_or_inconsistent_targets() {
        let unversioned = r#"('[{"id":46,"pid":6079,"title":"Old","x":0,"y":0,"w":1,"h":1}]',)"#;
        assert!(parse_windows(unversioned, None).is_none());

        let mismatched = r#"('[{"id":46,"target_id":"other:46","helper_epoch":"epoch-a","protocol_version":4,"pid":6079,"title":"Bad","x":0,"y":0,"w":1,"h":1}]',)"#;
        assert!(parse_windows(mismatched, None).is_none());

        let trailing = r#"('[{"id":46,"target_id":"epoch-a:garbage:46","helper_epoch":"epoch-a","protocol_version":4,"pid":6079,"title":"Bad","x":0,"y":0,"w":1,"h":1}]',)"#;
        assert!(parse_windows(trailing, None).is_none());
    }

    #[test]
    fn atomic_capture_payload_is_bound_to_target_geometry_and_png() {
        let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";
        let valid = format!(
            r#"{{"protocol_version":4,"target":"epoch-a:46","rect":{{"x":4,"y":5,"width":1,"height":1}},"logical_size":{{"width":100,"height":100}},"png_base64":"{png}"}}"#
        );

        assert!(decode_capture_payload(&valid, "epoch-a:46").is_ok());
        assert!(decode_capture_payload(&valid, "epoch-a:47")
            .unwrap_err()
            .to_string()
            .contains("target_changed_during_capture"));

        let escaped = valid.replace("\"x\":4", "\"x\":100");
        assert!(decode_capture_payload(&escaped, "epoch-a:46")
            .unwrap_err()
            .to_string()
            .contains("capture_geometry_invalid"));
    }

    #[test]
    fn foreground_tokens_are_caller_allocated_and_unique() {
        let first = new_foreground_token();
        let second = new_foreground_token();
        assert!(first.starts_with("cua-fg-"));
        assert_ne!(first, second);
        assert!(first.len() <= 167);
    }

    #[test]
    fn foreground_restoration_requires_a_terminal_resolved_outcome() {
        let restored = r#"('{"terminal":true,"state":"terminal","activation_required":true,"target_activation_verified":true,"restoration_attempted":true,"restoration_succeeded":true,"outcome":"restored_prior_context","reason":"complete"}',)"#;
        let outcome = parse_foreground_outcome(restored).expect("terminal restoration");
        assert_eq!(outcome.restoration_succeeded, Some(true));

        let unresolved = r#"('{"terminal":true,"state":"terminal","restoration_attempted":true,"restoration_succeeded":false,"outcome":"restoration_unresolved","reason":"prior_window_not_confirmed"}',)"#;
        assert!(parse_foreground_outcome(unresolved)
            .unwrap_err()
            .to_string()
            .contains("could not verify restoration"));

        let in_progress = r#"('{"terminal":false,"state":"restoring"}',)"#;
        assert!(parse_foreground_outcome(in_progress)
            .unwrap_err()
            .to_string()
            .contains("not terminal"));
    }
}
