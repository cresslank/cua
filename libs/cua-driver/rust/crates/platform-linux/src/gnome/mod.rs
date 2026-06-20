//! Optional GNOME Shell / Mutter backend.
//!
//! The regular Linux backend sees X11/XWayland windows through EWMH. Native
//! GNOME Wayland windows are owned by Mutter and are intentionally absent from
//! X11's `_NET_CLIENT_LIST`. This module talks to a small, local GNOME Shell
//! extension over the session bus so Cua can enumerate and safely activate /
//! move Mutter windows without pretending GNOME exposes wlroots protocols.
//!
//! User-facing opt-in is `CUA_DRIVER_GNOME_SHELL=1`. The extension is installed
//! separately; ordinary `cua-driver serve` does not install or enable Shell code.

use std::collections::HashSet;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;

use crate::x11::WindowInfo;

pub const ENABLE_GNOME_ENV: &str = "CUA_DRIVER_GNOME_SHELL";

const DBUS_NAME: &str = "org.trycua.Driver.GnomeShell";
const DBUS_PATH: &str = "/org/trycua/Driver/GnomeShell";
const DBUS_IFACE: &str = "org.trycua.Driver.GnomeShell";
const CALL_TIMEOUT: Duration = Duration::from_secs(3);

/// GNOME ids are prefixed before they enter the cross-platform `window_id`
/// field so they cannot collide with X11 XIDs. Keep this below 2^53 so JSON / JS
/// clients can round-trip it as an integer without precision loss.
pub const GNOME_WINDOW_ID_PREFIX: u64 = 1_u64 << 40;
const GNOME_LOCAL_ID_MASK: u64 = GNOME_WINDOW_ID_PREFIX - 1;

#[derive(Clone, Debug, Default, Deserialize)]
pub struct GnomeWindowRecord {
    pub id: u32,
    pub title: Option<String>,
    pub app_id: Option<String>,
    pub wm_class: Option<String>,
    pub pid: Option<u32>,
    pub workspace: Option<i32>,
    pub focused: Option<bool>,
    pub minimized: Option<bool>,
    pub visible: Option<bool>,
    pub window_type: Option<String>,
    pub x: Option<i32>,
    pub y: Option<i32>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

#[derive(Clone, Debug, Default)]
pub struct GnomeDiagnostic {
    pub enabled: bool,
    pub reachable: bool,
    pub window_count: usize,
    pub error: Option<String>,
}

fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build GNOME D-Bus tokio runtime")
    })
}

fn truthy_env(name: &str) -> bool {
    match std::env::var(name) {
        Ok(v) => {
            let v = v.trim();
            !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false")
        }
        Err(_) => false,
    }
}

pub fn gnome_enabled() -> bool {
    truthy_env(ENABLE_GNOME_ENV)
}

pub fn is_gnome_window_id(window_id: u64) -> bool {
    (window_id & GNOME_WINDOW_ID_PREFIX) == GNOME_WINDOW_ID_PREFIX
}

fn encode_window_id(local_id: u32) -> u64 {
    GNOME_WINDOW_ID_PREFIX | u64::from(local_id)
}

fn decode_window_id(window_id: u64) -> Result<u32> {
    if !is_gnome_window_id(window_id) {
        return Err(anyhow!(
            "window_id {window_id} is not a GNOME backend window id"
        ));
    }
    let local = window_id & GNOME_LOCAL_ID_MASK;
    u32::try_from(local)
        .map_err(|_| anyhow!("GNOME backend window id is out of range: {window_id}"))
}

async fn proxy<'a>(conn: &'a atspi::zbus::Connection) -> Result<atspi::zbus::Proxy<'a>> {
    atspi::zbus::Proxy::new(conn, DBUS_NAME, DBUS_PATH, DBUS_IFACE)
        .await
        .with_context(|| format!("GNOME Shell D-Bus extension unavailable at {DBUS_NAME}"))
}

fn run_bounded<T: Send + 'static>(
    work: impl std::future::Future<Output = Result<T>> + Send + 'static,
) -> Result<T> {
    runtime().block_on(async move {
        match tokio::time::timeout(CALL_TIMEOUT, work).await {
            Ok(r) => r,
            Err(_) => Err(anyhow!(
                "GNOME Shell D-Bus call timed out after {}s",
                CALL_TIMEOUT.as_secs()
            )),
        }
    })
}

async fn list_records_async() -> Result<Vec<GnomeWindowRecord>> {
    let conn = atspi::zbus::Connection::session()
        .await
        .context("connect to session bus")?;
    let proxy = proxy(&conn).await?;
    let raw: String = proxy
        .call("ListWindowsJson", &())
        .await
        .context("call ListWindowsJson")?;
    serde_json::from_str(&raw).context("parse GNOME window JSON")
}

fn list_records() -> Result<Vec<GnomeWindowRecord>> {
    run_bounded(list_records_async())
}

fn call_bool_method(
    method: &'static str,
    body: impl serde::Serialize + atspi::zbus::zvariant::DynamicType + Send + Sync + 'static,
) -> Result<()> {
    run_bounded(async move {
        let conn = atspi::zbus::Connection::session()
            .await
            .context("connect to session bus")?;
        let proxy = proxy(&conn).await?;
        let ok: bool = proxy
            .call(method, &body)
            .await
            .with_context(|| format!("call {method}"))?;
        if ok {
            Ok(())
        } else {
            Err(anyhow!("GNOME Shell extension returned false for {method}"))
        }
    })
}

fn display_title(record: &GnomeWindowRecord) -> String {
    let title = record.title.as_deref().unwrap_or("").trim();
    let app = record
        .app_id
        .as_deref()
        .or(record.wm_class.as_deref())
        .unwrap_or("")
        .trim();
    match (title.is_empty(), app.is_empty()) {
        (true, true) => "GNOME window".to_owned(),
        (false, true) => title.to_owned(),
        (true, false) => format!("[{app}]"),
        (false, false) if title.contains(app) => title.to_owned(),
        (false, false) => format!("{title} [{app}]"),
    }
}

fn record_to_window(record: GnomeWindowRecord) -> WindowInfo {
    WindowInfo {
        xid: encode_window_id(record.id),
        pid: record.pid.filter(|p| *p > 0),
        title: display_title(&record),
        x: record.x.unwrap_or_default(),
        y: record.y.unwrap_or_default(),
        width: record.width.unwrap_or_default(),
        height: record.height.unwrap_or_default(),
    }
}

pub fn list_windows(filter_pid: Option<u32>) -> Vec<WindowInfo> {
    if !gnome_enabled() {
        return Vec::new();
    }
    match list_records() {
        Ok(records) => records
            .into_iter()
            .filter(|record| {
                if let Some(pid) = filter_pid {
                    record.pid == Some(pid)
                } else {
                    true
                }
            })
            .map(record_to_window)
            .collect(),
        Err(e) => {
            tracing::warn!("GNOME Shell list_windows failed: {e}");
            Vec::new()
        }
    }
}

pub fn diagnostic() -> GnomeDiagnostic {
    let enabled = gnome_enabled();
    match list_records() {
        Ok(records) => GnomeDiagnostic {
            enabled,
            reachable: true,
            window_count: records.len(),
            error: None,
        },
        Err(e) => GnomeDiagnostic {
            enabled,
            reachable: false,
            window_count: 0,
            error: Some(e.to_string()),
        },
    }
}

pub fn available() -> bool {
    diagnostic().reachable
}

pub fn activate_window(window_id: u64) -> Result<()> {
    let id = decode_window_id(window_id)?;
    call_bool_method("ActivateWindow", (id,))
}

pub fn move_resize_window(window_id: u64, x: i32, y: i32, width: u32, height: u32) -> Result<()> {
    let id = decode_window_id(window_id)?;
    call_bool_method("MoveResizeWindow", (id, x, y, width as i32, height as i32))
}

pub fn move_window_to_workspace(window_id: u64, workspace_index: i32) -> Result<()> {
    let id = decode_window_id(window_id)?;
    call_bool_method("MoveWindowToWorkspace", (id, workspace_index))
}

pub fn switch_workspace(workspace_index: i32) -> Result<()> {
    call_bool_method("SwitchWorkspace", (workspace_index,))
}

/// Merge GNOME windows into an X11-first list. GNOME Shell also reports
/// XWayland windows, so prefer the X11 record when pid+title match.
pub fn append_deduped(existing: &mut Vec<WindowInfo>, gnome_windows: Vec<WindowInfo>) {
    let mut seen_ids: HashSet<u64> = existing.iter().map(|w| w.xid).collect();
    let mut seen_pid_titles: HashSet<(u32, String)> = existing
        .iter()
        .filter_map(|w| w.pid.map(|pid| (pid, w.title.clone())))
        .collect();

    for w in gnome_windows {
        if seen_ids.contains(&w.xid) {
            continue;
        }
        if let Some(pid) = w.pid {
            let key = (pid, w.title.clone());
            if seen_pid_titles.contains(&key) {
                continue;
            }
            seen_pid_titles.insert(key);
        }
        seen_ids.insert(w.xid);
        existing.push(w);
    }
}

pub fn unsupported_input_message(tool: &str, window_id: u64) -> String {
    format!(
        "{tool}: window_id {window_id} is a GNOME Shell backend window. Cua can list and activate GNOME native Wayland windows, but click/type/key input is not wired for this backend yet. Use bring_to_front to focus it, or relaunch the target under XWayland for X11 background input."
    )
}
