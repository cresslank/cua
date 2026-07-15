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
//! screen coords, no Wayland cursor). Uses a short-lived `gdbus` subprocess so
//! there's no zbus blocking-feature or async-context coupling — the calls are
//! infrequent (once per `get_window_state`, a few per click).

use std::collections::HashMap;
use std::process::Command;
use std::time::Duration;

use crate::x11::WindowInfo;

const DEST: &str = "org.cua.WinRects";
const PATH: &str = "/org/cua/WinRects";
const IFACE: &str = "org.cua.WinRects";
const INTROSPECT_DEST: &str = "org.gnome.Shell.Introspect";
const INTROSPECT_PATH: &str = "/org/gnome/Shell/Introspect";
const INTROSPECT_IFACE: &str = "org.gnome.Shell.Introspect";
const REQUIRED_PROTOCOL: u64 = 2;

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

#[derive(Debug)]
pub struct ForegroundTransaction {
    token: String,
}

impl ForegroundTransaction {
    pub fn finish(mut self) {
        finish_foreground_token(&std::mem::take(&mut self.token));
    }

    pub fn commit(mut self) -> anyhow::Result<()> {
        let token = std::mem::take(&mut self.token);
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
        Ok(())
    }
}

impl Drop for ForegroundTransaction {
    fn drop(&mut self) {
        if !self.token.is_empty() {
            finish_foreground_token(&std::mem::take(&mut self.token));
        }
    }
}

pub fn available() -> bool {
    capabilities().is_some_and(|capabilities| {
        capabilities.protocol_version == REQUIRED_PROTOCOL
            && !capabilities.epoch.is_empty()
            && capabilities
                .capabilities
                .iter()
                .any(|capability| capability == "exact-target-v2")
    })
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
    gdbus_call_target(DEST, PATH, &format!("{IFACE}.{method}"), args, timeout)
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

/// Capture the GNOME stage only after WinRects v2 confirms that the exact
/// incarnation-qualified target is currently painted there.
pub fn screenshot_window(window_id: u64) -> anyhow::Result<Vec<u8>> {
    use base64::{engine::general_purpose::STANDARD as B64, Engine as _};

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
    let start = raw
        .find('\'')
        .ok_or_else(|| anyhow::anyhow!("GNOME capture returned an invalid payload"))?
        + 1;
    let end = raw
        .rfind('\'')
        .ok_or_else(|| anyhow::anyhow!("GNOME capture returned an invalid payload"))?;
    if end <= start {
        anyhow::bail!("GNOME capture returned an empty payload");
    }
    B64.decode(&raw[start..end])
        .map_err(|error| anyhow::anyhow!("GNOME capture returned invalid base64: {error}"))
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
                workspace_index: window.workspace_index,
                workspace_active: window.workspace_active,
                sticky: window.sticky,
                monitor: window.monitor,
                capture_current: Some(window.capture_current),
            })
            .collect(),
    )
}

pub fn begin_foreground(window_id: u64) -> anyhow::Result<ForegroundTransaction> {
    let target = resolve_target(window_id).ok_or_else(|| {
        anyhow::anyhow!(
            "stale_target: GNOME window {window_id} belongs to another helper incarnation or no longer exists"
        )
    })?;
    let raw = gdbus_call_with_timeout(
        "BeginForeground",
        &[gvariant_string(&target.target_id)],
        Duration::from_secs(2),
    )
    .ok_or_else(|| {
        anyhow::anyhow!(
            "foreground_unavailable: GNOME rejected or timed out activating exact window {window_id}"
        )
    })?;
    let payload = extract_json_object(&raw)
        .and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok())
        .ok_or_else(|| anyhow::anyhow!("foreground_unavailable: invalid WinRects response"))?;
    if payload.get("activated").and_then(serde_json::Value::as_bool) != Some(true) {
        anyhow::bail!(
            "foreground_unavailable: WinRects did not confirm exact window {window_id} activation"
        );
    }
    let token = payload
        .get("transaction")
        .and_then(serde_json::Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| anyhow::anyhow!("foreground_unavailable: missing WinRects transaction"))?
        .to_owned();
    Ok(ForegroundTransaction { token })
}

fn finish_foreground_token(token: &str) {
    if token.is_empty() {
        return;
    }
    let _ = gdbus_call_with_timeout(
        "EndForeground",
        &[gvariant_string(token)],
        Duration::from_secs(2),
    );
}

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
                workspace_index: window.workspace_index,
                workspace_active: window.workspace_active,
                sticky: window.sticky,
                monitor: window.monitor,
                capture_current: Some(window.capture_current),
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
        if protocol != REQUIRED_PROTOCOL {
            return None;
        }
        let native_id = window.get("id")?.as_u64()?.max(1);
        let helper_epoch = window.get("helper_epoch")?.as_str()?.to_owned();
        let target_id = window.get("target_id")?.as_str()?.to_owned();
        if helper_epoch.is_empty()
            || !target_id.starts_with(&format!("{helper_epoch}:"))
            || !target_id.ends_with(&format!(":{native_id}"))
        {
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
        parsed.push(ShellWindow {
            public_id,
            native_id,
            target_id,
            helper_epoch,
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
    Some(parsed)
}

pub fn move_cursor(owner: &str, window_id: u64, x: i32, y: i32) -> bool {
    let Some(target) = resolve_target(window_id) else {
        return false;
    };
    gdbus_call(
        "MoveCursorFor",
        &[
            gvariant_string(owner),
            gvariant_string(&target.target_id),
            x.to_string(),
            y.to_string(),
        ],
    )
    .is_some()
}

pub fn click_pulse(owner: &str, window_id: u64, x: i32, y: i32) -> bool {
    let Some(target) = resolve_target(window_id) else {
        return false;
    };
    gdbus_call(
        "ClickPulseFor",
        &[
            gvariant_string(owner),
            gvariant_string(&target.target_id),
            x.to_string(),
            y.to_string(),
        ],
    )
    .is_some()
}

pub fn hide_cursor(owner: &str) {
    let _ = gdbus_call("HideCursorFor", &[gvariant_string(owner)]);
}

pub fn remove_cursor(owner: &str) {
    let _ = gdbus_call("RemoveCursor", &[gvariant_string(owner)]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_shell_logical_screen_size_property() {
        assert_eq!(parse_screen_size("(<(4096, 1728)>,)"), Some((4096, 1728)));
        assert_eq!(parse_screen_size("(<(0, 1728)>,)"), None);
    }

    #[test]
    fn parses_and_filters_shell_windows() {
        let raw = r#"('[{"id":46,"target_id":"epoch-a:46","helper_epoch":"epoch-a","protocol_version":2,"pid":6079,"app_id":"org.example.Editor","title":"Sentinel's window","x":66,"y":32,"buffer_x":60,"buffer_y":28,"w":958,"h":736,"focused":true,"minimized":false,"visible":true,"capture_current":true,"workspace_index":7,"workspace_active":true,"sticky":false,"monitor":1,"stacking":2},{"id":47,"target_id":"epoch-a:47","helper_epoch":"epoch-a","protocol_version":2,"pid":6080,"app_id":"org.example.Hidden","title":"Hidden","x":0,"y":0,"w":100,"h":100,"minimized":true,"visible":false,"capture_current":false,"workspace_index":11,"workspace_active":false,"sticky":false,"monitor":0,"stacking":1}]',)"#;
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
        let raw = r#"('[{"id":47,"target_id":"epoch-a:47","helper_epoch":"epoch-a","protocol_version":2,"pid":6080,"app_id":"org.example.Hidden","title":"Hidden","x":0,"y":0,"w":100,"h":100,"minimized":true,"visible":false,"capture_current":false,"stacking":1}]',)"#;
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
    fn rejects_unversioned_or_inconsistent_targets() {
        let unversioned = r#"('[{"id":46,"pid":6079,"title":"Old","x":0,"y":0,"w":1,"h":1}]',)"#;
        assert!(parse_windows(unversioned, None).is_none());

        let mismatched = r#"('[{"id":46,"target_id":"other:46","helper_epoch":"epoch-a","protocol_version":2,"pid":6079,"title":"Bad","x":0,"y":0,"w":1,"h":1}]',)"#;
        assert!(parse_windows(mismatched, None).is_none());
    }
}
