//! Best-effort Sway/i3-compatible compositor metadata.
//!
//! The wlroots foreign-toplevel protocol exposes titles and app ids, but not
//! process ids or geometry. Sway's IPC tree supplies those missing fields and
//! a compositor-stable container id. Other compositors simply return no data.

use std::process::{Command, Stdio};

use serde::Deserialize;

#[derive(Clone, Debug, Default, Deserialize)]
struct Rect {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct Node {
    id: u64,
    // Sway sends `null` for unnamed containers and for XWayland views'
    // `app_id`; `default` covers only a missing field, and one null used to
    // discard the whole tree.
    #[serde(default, deserialize_with = "nullable_string")]
    name: String,
    #[serde(default, deserialize_with = "nullable_string")]
    app_id: String,
    pid: Option<u32>,
    #[serde(default)]
    rect: Rect,
    #[serde(default)]
    window_rect: Rect,
    #[serde(default)]
    deco_rect: Rect,
    #[serde(default)]
    focused: bool,
    #[serde(default = "default_visible")]
    visible: bool,
    #[serde(default)]
    fullscreen_mode: i32,
    #[serde(default)]
    nodes: Vec<Node>,
    #[serde(default)]
    floating_nodes: Vec<Node>,
}

fn default_visible() -> bool {
    true
}

fn nullable_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Window {
    pub id: u64,
    pub pid: u32,
    pub title: String,
    pub app_id: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub content_x: i32,
    pub content_y: i32,
    pub focused: bool,
    pub visible: bool,
    pub fullscreen: bool,
}

fn collect(node: &Node, windows: &mut Vec<Window>) {
    if let Some(pid) = node.pid {
        if !node.name.is_empty() || !node.app_id.is_empty() {
            // Sway normally reports the client surface origin in
            // `window_rect`. Some server-decorated Wayland clients instead
            // leave that origin at zero and expose the title-bar inset only
            // through `deco_rect`; use it as the content origin in that shape.
            let inferred_top_inset = if node.window_rect.height > 0 {
                node.rect
                    .height
                    .saturating_sub(node.window_rect.height)
                    .max(0)
            } else {
                0
            };
            let content_y = if node.window_rect.y != 0 {
                node.window_rect.y
            } else {
                node.deco_rect
                    .y
                    .saturating_add(node.deco_rect.height.max(0))
                    .max(inferred_top_inset)
            };
            windows.push(Window {
                id: node.id,
                pid,
                title: node.name.clone(),
                app_id: node.app_id.clone(),
                x: node.rect.x,
                y: node.rect.y,
                width: node.rect.width.max(0) as u32,
                height: node.rect.height.max(0) as u32,
                content_x: node.window_rect.x,
                content_y,
                focused: node.focused,
                visible: node.visible,
                fullscreen: node.fullscreen_mode != 0,
            });
        }
    }
    for child in node.nodes.iter().chain(&node.floating_nodes) {
        collect(child, windows);
    }
}

fn parse_tree(bytes: &[u8]) -> Option<Vec<Window>> {
    let root: Node = serde_json::from_slice(bytes).ok()?;
    let mut windows = Vec::new();
    collect(&root, &mut windows);
    Some(windows)
}

pub fn list_windows() -> Option<Vec<Window>> {
    std::env::var_os("SWAYSOCK")?;
    let output = Command::new("swaymsg")
        .args(["-r", "-t", "get_tree"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_tree(&output.stdout)
}

/// Fail closed rather than use a first-output physical extent for logical
/// AT-SPI coordinates. Secondary element pointer input currently qualifies only
/// one unscaled, unrotated, origin-zero output.
pub(crate) fn retained_pointer_extent() -> anyhow::Result<(u32, u32)> {
    let output = Command::new("swaymsg")
        .args(["-r", "-t", "get_outputs"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()?;
    anyhow::ensure!(output.status.success(), "output geometry unavailable");
    retained_pointer_extent_from_json(&serde_json::from_slice::<serde_json::Value>(
        &output.stdout,
    )?)
}

fn retained_pointer_extent_from_json(value: &serde_json::Value) -> anyhow::Result<(u32, u32)> {
    let outputs = value
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("invalid output geometry"))?;
    let active: Vec<_> = outputs.iter().filter(|o| o["active"] == true).collect();
    anyhow::ensure!(
        active.len() == 1,
        "element_route_unqualified: multiple/no active outputs"
    );
    let output = active[0];
    anyhow::ensure!(
        output["scale"].as_f64() == Some(1.0)
            && output["transform"] == "normal"
            && output["rect"]["x"] == 0
            && output["rect"]["y"] == 0,
        "element_route_unqualified: scaled, transformed or offset output"
    );
    let w = output["rect"]["width"]
        .as_u64()
        .and_then(|w| u32::try_from(w).ok());
    let h = output["rect"]["height"]
        .as_u64()
        .and_then(|h| u32::try_from(h).ok());
    match (w, h) {
        (Some(w), Some(h)) if w > 0 && h > 0 => Ok((w, h)),
        _ => anyhow::bail!("output dimensions unavailable"),
    }
}

#[cfg(test)]
#[test]
fn secondary_pointer_output_geometry_is_fail_closed() {
    use serde_json::json;
    let output = json!({"active":true,"scale":1.0,"transform":"normal","rect":{"x":0,"y":0,"width":800,"height":600}});
    assert_eq!(
        retained_pointer_extent_from_json(&json!([output])).unwrap(),
        (800, 600)
    );
    assert!(retained_pointer_extent_from_json(&json!([output, output])).is_err());
    for (key, value) in [
        ("scale", json!(2.0)),
        ("transform", json!("90")),
        ("active", json!(false)),
    ] {
        let mut changed = output.clone();
        changed[key] = value;
        assert!(retained_pointer_extent_from_json(&json!([changed])).is_err());
    }
    let mut changed = output.clone();
    changed["rect"]["x"] = json!(10);
    assert!(retained_pointer_extent_from_json(&json!([changed])).is_err());
    assert!(retained_pointer_extent_from_json(&json!([])).is_err());
}

pub fn window_for_id(id: u64) -> Option<Window> {
    list_windows()?.into_iter().find(|window| window.id == id)
}

pub fn window_for_pid(pid: u32) -> Option<Window> {
    list_windows()?
        .into_iter()
        .filter(|window| window.pid == pid && window.width > 0 && window.height > 0)
        .max_by_key(|window| {
            (
                window.focused,
                window.visible,
                u64::from(window.width) * u64::from(window.height),
            )
        })
}

pub fn window_for_title(title: &str) -> Option<Window> {
    list_windows()?
        .into_iter()
        .filter(|window| {
            window.width > 0
                && window.height > 0
                && (window.title == title
                    || (!window.title.is_empty() && title.starts_with(&window.title))
                    || (!title.is_empty() && window.title.starts_with(title)))
        })
        .max_by_key(|window| (window.focused, window.visible))
}

pub fn window_for_app_id(app_id: &str) -> Option<Window> {
    if app_id.is_empty() {
        return None;
    }
    list_windows()?
        .into_iter()
        .filter(|window| window.width > 0 && window.height > 0 && window.app_id == app_id)
        .max_by_key(|window| (window.focused, window.visible))
}

fn focus_container(id: u64) -> bool {
    let selector = format!("[con_id={id}]");
    Command::new("swaymsg")
        .args([selector.as_str(), "focus"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn focus_container_exact(id: u64, pid: u32) -> bool {
    let selector = format!("[con_id={id} pid={pid}]");
    Command::new("swaymsg")
        .args([selector.as_str(), "focus"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn wait_for_exact_container_focus(id: u64, pid: u32, timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match window_for_id(id) {
            Some(window) if window.pid != pid => return false,
            Some(window) if window.focused => return true,
            Some(_) => {}
            None => return false,
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Exact Sway focus transaction for a stateful press/move/release sequence.
/// Its caller must hold the host raw-input lease for this value's lifetime.
pub struct StatefulFocus {
    id: u64,
    pid: u32,
    prior: Option<(u64, u32)>,
    finished: bool,
}

impl StatefulFocus {
    pub fn begin(pid: u32, id: u64) -> anyhow::Result<Self> {
        let windows =
            list_windows().ok_or_else(|| anyhow::anyhow!("Sway IPC tree is unavailable"))?;
        if !windows
            .iter()
            .any(|window| window.id == id && window.pid == pid)
        {
            anyhow::bail!("stale_target: Sway container {id} is no longer owned by pid {pid}");
        }
        let prior = windows
            .iter()
            .find(|window| window.focused)
            .map(|window| (window.id, window.pid));
        if !focus_container_exact(id, pid) {
            anyhow::bail!("Sway refused to focus exact container {id}");
        }
        let focus = Self {
            id,
            pid,
            prior,
            finished: false,
        };
        if !wait_for_exact_container_focus(id, pid, std::time::Duration::from_millis(500)) {
            anyhow::bail!(
                "stale_target: exact Sway container {id} for pid {pid} did not acquire focus"
            );
        }
        focus.validate()?;
        Ok(focus)
    }

    /// Re-read identity and focus immediately before every event, rejecting id
    /// reuse and focus theft rather than injecting into the active surface.
    pub fn validate(&self) -> anyhow::Result<Window> {
        window_for_id(self.id)
            .filter(|window| window.pid == self.pid && window.focused)
            .ok_or_else(|| {
                anyhow::anyhow!(
                "stale_target: exact Sway container {} for pid {} is not focused at action time",
                self.id,
                self.pid
            )
            })
    }

    pub fn finish(mut self) -> anyhow::Result<()> {
        let result = self.restore();
        self.finished = true;
        result
    }

    fn restore(&mut self) -> anyhow::Result<()> {
        let Some((prior, prior_pid)) = self.prior.filter(|(prior, _)| *prior != self.id) else {
            return Ok(());
        };
        if !window_for_id(prior).is_some_and(|window| window.pid == prior_pid) {
            anyhow::bail!("Sway prior container {prior} changed identity before restoration");
        }
        if !focus_container_exact(prior, prior_pid) {
            anyhow::bail!("Sway could not restore prior container {prior}");
        }
        if !wait_for_exact_container_focus(prior, prior_pid, std::time::Duration::from_millis(500))
        {
            anyhow::bail!("Sway did not confirm restored container {prior}");
        }
        Ok(())
    }
}

impl Drop for StatefulFocus {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.restore();
        }
    }
}

fn with_focused_container_using<T>(
    expected_pid: u32,
    id: u64,
    mut read_windows: impl FnMut() -> anyhow::Result<Vec<Window>>,
    mut focus: impl FnMut(u64) -> bool,
    mut wait_for_focus: impl FnMut(u64, u32) -> bool,
    body: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let prior = read_windows()?
        .into_iter()
        .find(|window| window.focused)
        .map(|window| (window.id, window.pid));
    if !focus(id) {
        anyhow::bail!("Sway refused to focus exact container {id}");
    }

    // Once focus succeeds, every later path restores the prior identity. Re-read
    // both id and PID before the action body to reject container-id replacement.
    let result = if !wait_for_focus(id, expected_pid) {
        Err(anyhow::anyhow!(
            "Sway did not confirm focus on exact container {id} for pid {expected_pid}"
        ))
    } else {
        match read_windows() {
        Ok(windows) => match windows.into_iter().find(|window| window.id == id) {
            Some(window) if window.pid != expected_pid => Err(anyhow::anyhow!(
                "stale_target: Sway container {id} changed from expected pid {expected_pid} to pid {} while acquiring focus",
                window.pid
            )),
            Some(window) if !window.focused => Err(anyhow::anyhow!(
                "Sway did not confirm focus on exact container {id} for pid {expected_pid}"
            )),
            Some(_) => body(),
            None => Err(anyhow::anyhow!(
                "stale_target: Sway container {id} for pid {expected_pid} disappeared while acquiring focus"
            )),
        },
        Err(error) => Err(error.context(format!(
            "Sway could not confirm exact container {id} for pid {expected_pid} after focusing it"
        ))),
        }
    };
    let restore = prior
        .filter(|(prior, _)| *prior != id)
        .map(|(prior, prior_pid)| {
            if !read_windows()?
                .into_iter()
                .any(|window| window.id == prior && window.pid == prior_pid)
            {
                anyhow::bail!("Sway prior container {prior} changed identity before restoration");
            }
            if !focus(prior) {
                anyhow::bail!("Sway could not restore prior container {prior}");
            }
            if !wait_for_focus(prior, prior_pid) {
                anyhow::bail!("Sway did not confirm restored container {prior}");
            }
            if !read_windows()?
                .into_iter()
                .any(|window| window.id == prior && window.pid == prior_pid && window.focused)
            {
                anyhow::bail!("Sway did not confirm restored container {prior}");
            }
            Ok(())
        })
        .transpose();
    match (result, restore) {
        (Ok(value), Ok(_)) => Ok(value),
        (Err(error), Ok(_)) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(restore)) => Err(error.context(format!(
            "the prior Sway focus also could not be restored: {restore}"
        ))),
    }
}

/// Briefly focus one compositor-attested container, run `body`, then restore
/// the previously focused identity. PID and id are revalidated together after
/// focus and immediately before `body` can run.
pub fn with_focused_container<T>(
    expected_pid: u32,
    id: u64,
    body: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    with_focused_container_using(
        expected_pid,
        id,
        || list_windows().ok_or_else(|| anyhow::anyhow!("Sway IPC tree is unavailable")),
        focus_container,
        |container, pid| {
            wait_for_exact_container_focus(container, pid, std::time::Duration::from_millis(500))
        },
        body,
    )
}

pub fn window_origin_for_pid(pid: u32) -> Option<(i32, i32)> {
    window_for_pid(pid).map(|window| (window.x, window.y))
}

/// Offset of the application content inside Sway's captured toplevel.
/// Server-side decorations are part of `rect`/window screenshots but not of
/// WebKitGTK's AT-SPI `CoordType::Window` descendants.
pub fn window_content_offset_for_pid(pid: u32) -> Option<(i32, i32)> {
    window_for_pid(pid).map(|window| (window.content_x, window.content_y))
}

pub fn window_origin_for_title(title: &str) -> Option<(i32, i32)> {
    let window = window_for_title(title)?;
    Some((window.x, window.y))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(id: u64, pid: u32, focused: bool) -> Window {
        Window {
            id,
            pid,
            title: String::new(),
            app_id: String::new(),
            x: 0,
            y: 0,
            width: 100,
            height: 100,
            content_x: 0,
            content_y: 0,
            focused,
            visible: true,
            fullscreen: false,
        }
    }

    #[test]
    fn pid_replacement_after_focus_is_refused_and_prior_focus_is_restored() {
        let reads = std::cell::RefCell::new(std::collections::VecDeque::from([
            Ok(vec![window(1, 10, true), window(2, 20, false)]),
            Ok(vec![window(1, 10, false), window(2, 99, true)]),
            Ok(vec![window(1, 10, false), window(2, 99, true)]),
            Ok(vec![window(1, 10, true), window(2, 99, false)]),
        ]));
        let focused = std::cell::RefCell::new(Vec::new());
        let body_ran = std::cell::Cell::new(false);

        let error = with_focused_container_using(
            20,
            2,
            || reads.borrow_mut().pop_front().expect("expected tree read"),
            |id| {
                focused.borrow_mut().push(id);
                true
            },
            |_, _| true,
            || {
                body_ran.set(true);
                Ok(())
            },
        )
        .expect_err("replacement pid must be refused");

        assert!(error
            .to_string()
            .contains("changed from expected pid 20 to pid 99"));
        assert!(!body_ran.get());
        assert_eq!(*focused.borrow(), [2, 1]);
    }

    #[test]
    fn failed_focus_confirmation_still_restores_prior_focus() {
        let reads = std::cell::RefCell::new(std::collections::VecDeque::from([
            Ok(vec![window(1, 10, true), window(2, 20, false)]),
            Ok(vec![window(1, 10, false), window(2, 20, false)]),
            Ok(vec![window(1, 10, true), window(2, 20, false)]),
        ]));
        let focused = std::cell::RefCell::new(Vec::new());
        let confirmations =
            std::cell::RefCell::new(std::collections::VecDeque::from([false, true]));
        let body_ran = std::cell::Cell::new(false);

        let error = with_focused_container_using(
            20,
            2,
            || reads.borrow_mut().pop_front().expect("expected tree read"),
            |id| {
                focused.borrow_mut().push(id);
                true
            },
            |_, _| {
                confirmations
                    .borrow_mut()
                    .pop_front()
                    .expect("expected focus confirmation")
            },
            || {
                body_ran.set(true);
                Ok(())
            },
        )
        .expect_err("unconfirmed focus must be refused");

        assert!(error.to_string().contains("did not confirm focus"));
        assert!(!body_ran.get());
        assert_eq!(*focused.borrow(), [2, 1]);
    }

    #[test]
    fn failed_post_focus_tree_read_still_restores_prior_focus() {
        let reads = std::cell::RefCell::new(std::collections::VecDeque::from([
            Ok(vec![window(1, 10, true), window(2, 20, false)]),
            Err(anyhow::anyhow!("transient get_tree failure")),
            Ok(vec![window(1, 10, false), window(2, 20, true)]),
            Ok(vec![window(1, 10, true), window(2, 20, false)]),
        ]));
        let focused = std::cell::RefCell::new(Vec::new());
        let body_ran = std::cell::Cell::new(false);

        let error = with_focused_container_using(
            20,
            2,
            || reads.borrow_mut().pop_front().expect("expected tree read"),
            |id| {
                focused.borrow_mut().push(id);
                true
            },
            |_, _| true,
            || {
                body_ran.set(true);
                Ok(())
            },
        )
        .expect_err("failed focus tree read must be refused");

        assert!(error
            .to_string()
            .contains("could not confirm exact container"));
        assert!(!body_ran.get());
        assert_eq!(*focused.borrow(), [2, 1]);
    }

    #[test]
    fn parses_nested_and_floating_windows() {
        let tree = br#"{
          "id": 1,
          "nodes": [{
            "id": 2,
            "nodes": [{
              "id": 10,
              "name": "Editor",
              "app_id": "org.example.Editor",
              "pid": 123,
              "rect": {"x": 20, "y": 30, "width": 800, "height": 600},
              "window_rect": {"x": 0, "y": 47, "width": 800, "height": 553},
              "focused": true,
              "visible": true,
              "fullscreen_mode": 1
            }],
            "floating_nodes": [{
              "id": 11,
              "name": "Dialog",
              "pid": 124,
              "rect": {"x": 100, "y": 120, "width": 300, "height": 200}
            }]
          }]
        }"#;
        let windows = parse_tree(tree).expect("parse Sway tree");
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].pid, 123);
        assert_eq!((windows[0].x, windows[0].y), (20, 30));
        assert_eq!((windows[0].content_x, windows[0].content_y), (0, 47));
        assert!(windows[0].focused);
        assert!(windows[0].fullscreen);
        assert_eq!(windows[1].title, "Dialog");
    }

    #[test]
    fn parses_xwayland_null_app_id_and_nullable_container_names() {
        // Sway 1.11 shape: an unnamed split container and an XWayland view
        // whose app_id is null, on a workspace that is not visible.
        let tree = br#"{
          "id": 1,
          "name": "root",
          "nodes": [{
            "id": 2,
            "name": null,
            "app_id": null,
            "nodes": [{
              "id": 10,
              "name": "Native",
              "app_id": "org.example.Native",
              "pid": 123,
              "rect": {"x": 0, "y": 0, "width": 400, "height": 300}
            }, {
              "id": 41,
              "name": "GTK3 XWayland",
              "app_id": null,
              "pid": 456,
              "visible": false,
              "rect": {"x": 400, "y": 0, "width": 400, "height": 300}
            }]
          }]
        }"#;
        let windows = parse_tree(tree).expect("null names and app ids must not discard the tree");
        assert_eq!(windows.len(), 2);
        let xwayland = windows.iter().find(|w| w.pid == 456).unwrap();
        assert_eq!(xwayland.id, 41);
        assert_eq!(xwayland.title, "GTK3 XWayland");
        assert_eq!(xwayland.app_id, "");
        assert!(!xwayland.visible);
        assert_eq!(windows.iter().find(|w| w.pid == 123).unwrap().id, 10);

        // Missing fields still default; malformed values still reject the tree.
        assert!(parse_tree(br#"{"id": 1, "nodes": [{"id": 3, "pid": 7, "name": "x"}]}"#).is_some());
        assert!(parse_tree(br#"{"id": 1, "name": 5}"#).is_none());
        assert!(parse_tree(br#"{"id": 1, "app_id": {"a": 1}}"#).is_none());
        assert!(parse_tree(br#"{"id": "1"}"#).is_none());
    }

    #[test]
    fn decoration_height_fills_zero_window_content_origin() {
        let tree = br#"{
          "id": 1,
          "nodes": [{
            "id": 10,
            "name": "Tauri",
            "app_id": "cua-test-harness",
            "pid": 123,
            "rect": {"x": 0, "y": 0, "width": 940, "height": 780},
            "window_rect": {"x": 0, "y": 0, "width": 940, "height": 733},
            "deco_rect": {"x": 0, "y": 0, "width": 940, "height": 47}
          }]
        }"#;
        let windows = parse_tree(tree).expect("parse decorated Sway tree");
        assert_eq!((windows[0].content_x, windows[0].content_y), (0, 47));
    }

    #[test]
    fn outer_client_height_delta_fills_missing_decoration_metadata() {
        let tree = br#"{
          "id": 1,
          "nodes": [{
            "id": 10,
            "name": "Tauri",
            "app_id": "cua-test-harness",
            "pid": 123,
            "rect": {"x": 0, "y": 0, "width": 940, "height": 780},
            "window_rect": {"x": 0, "y": 0, "width": 940, "height": 733},
            "deco_rect": {"x": 0, "y": 0, "width": 0, "height": 0}
          }]
        }"#;
        let windows = parse_tree(tree).expect("parse Sway tree with implicit decoration");
        assert_eq!((windows[0].content_x, windows[0].content_y), (0, 47));
    }
}
