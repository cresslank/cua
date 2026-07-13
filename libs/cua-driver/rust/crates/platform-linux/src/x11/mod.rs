//! X11 window enumeration via x11rb.
//!
//! Uses _NET_CLIENT_LIST_STACKING to get the list of top-level windows,
//! then reads WM_NAME/_NET_WM_NAME, _NET_WM_PID, and geometry per window.

use anyhow::Result;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;
use x11rb::rust_connection::RustConnection;

#[derive(Debug, Clone)]
pub struct WindowInfo {
    /// X11 Window (XID) cast to u64.
    pub xid: u64,
    pub pid: Option<u32>,
    pub app_name: String,
    pub title: String,
    pub is_on_screen: bool,
    pub z_index: Option<usize>,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// Return the best available X11/XWayland desktop size.
///
/// GNOME/Mutter's rootless XWayland can report a 0x0 root screen even while
/// XWayland top-level windows are visible and automation is otherwise usable.
/// Prefer the X11 screen size when it is non-zero, then fall back to EWMH
/// workarea extents and finally the bounding box of visible X11 windows.
pub fn screen_size() -> Result<(u32, u32)> {
    let (conn, screen_num) = RustConnection::connect(None)?;
    let screen = &conn.setup().roots[screen_num];
    let root = screen.root;

    let width = screen.width_in_pixels as u32;
    let height = screen.height_in_pixels as u32;
    if width > 0 && height > 0 {
        return Ok((width, height));
    }

    if let Ok(Some(size)) = get_workarea_extent(&conn, root) {
        return Ok(size);
    }

    if let Ok(Some(size)) = get_window_bounds_extent(&conn, root) {
        return Ok(size);
    }

    Ok((width, height))
}

/// List top-level windows, optionally filtered by pid.
pub fn list_windows(filter_pid: Option<u32>) -> Vec<WindowInfo> {
    match list_windows_inner(filter_pid) {
        Ok(w) => w,
        Err(_) => Vec::new(),
    }
}

fn list_windows_inner(filter_pid: Option<u32>) -> Result<Vec<WindowInfo>> {
    let (conn, screen_num) = RustConnection::connect(None)?;
    let screen = &conn.setup().roots[screen_num];
    let root = screen.root;

    // Get _NET_CLIENT_LIST_STACKING (or fallback to _NET_CLIENT_LIST).
    let windows = get_window_list(&conn, root)?;

    let mut result = Vec::new();
    for (z_index, xid) in windows.into_iter().enumerate() {
        let pid = get_window_pid(&conn, xid).ok().flatten();
        if let Some(fp) = filter_pid {
            if pid != Some(fp) { continue; }
        }

        let title = get_window_title(&conn, xid).unwrap_or_default();
        if title.trim().is_empty() { continue; }
        let app_name = get_window_class(&conn, xid)
            .map(|(instance, class)| if class.is_empty() { instance } else { class })
            .unwrap_or_default();
        let is_on_screen = conn
            .get_window_attributes(xid)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .is_some_and(|attributes| attributes.map_state == MapState::VIEWABLE);

        let geom = conn.get_geometry(xid)?.reply().ok();
        let (x, y, w, h) = if let Some(g) = geom {
            // Translate to root coordinates.
            let trans = conn.translate_coordinates(xid, root, 0, 0)?.reply().ok();
            let (rx, ry) = trans.map(|t| (t.dst_x as i32, t.dst_y as i32)).unwrap_or((0, 0));
            (rx, ry, g.width as u32, g.height as u32)
        } else {
            (0, 0, 0, 0)
        };

        result.push(WindowInfo {
            xid: xid as u64,
            pid,
            app_name,
            title,
            is_on_screen,
            z_index: Some(z_index),
            x,
            y,
            width: w,
            height: h,
        });
    }

    Ok(result)
}

fn get_window_list(conn: &RustConnection, root: Window) -> Result<Vec<Window>> {
    let atom_names = ["_NET_CLIENT_LIST_STACKING", "_NET_CLIENT_LIST"];
    for name in &atom_names {
        if let Ok(atom) = get_atom(conn, name) {
            if let Ok(reply) = conn.get_property(false, root, atom, AtomEnum::WINDOW, 0, u32::MAX)?.reply() {
                let windows: Vec<Window> = reply.value32()
                    .map(|iter| iter.collect())
                    .unwrap_or_default();
                if client_list_property(reply.type_, windows.as_slice()).is_some() {
                    return Ok(windows);
                }
            }
        }
    }

    // No EWMH client-list property means there may be no window manager. In
    // that case only expose mapped root children; unmapped Electron children
    // can otherwise be reported before a late-starting WM reparents them.
    let tree = conn.query_tree(root)?.reply()?;
    Ok(tree
        .children
        .into_iter()
        .filter(|window| {
            conn.get_window_attributes(*window)
                .ok()
                .and_then(|cookie| cookie.reply().ok())
                .map(|attributes| fallback_window_is_listable(attributes.map_state))
                .unwrap_or(false)
        })
        .collect())
}

fn client_list_property(property_type: Atom, windows: &[Window]) -> Option<&[Window]> {
    (property_type != x11rb::NONE).then_some(windows)
}

fn fallback_window_is_listable(map_state: MapState) -> bool {
    map_state == MapState::VIEWABLE
}

fn get_workarea_extent(conn: &RustConnection, root: Window) -> Result<Option<(u32, u32)>> {
    let atom = get_atom(conn, "_NET_WORKAREA")?;
    let reply = conn.get_property(false, root, atom, AtomEnum::CARDINAL, 0, u32::MAX)?.reply()?;
    let values: Vec<u32> = reply.value32().map(|iter| iter.collect()).unwrap_or_default();

    let mut max_right = 0i64;
    let mut max_bottom = 0i64;
    for chunk in values.chunks_exact(4) {
        let x = chunk[0] as i64;
        let y = chunk[1] as i64;
        let width = chunk[2] as i64;
        let height = chunk[3] as i64;
        if width > 0 && height > 0 {
            max_right = max_right.max(x + width);
            max_bottom = max_bottom.max(y + height);
        }
    }

    if max_right > 0 && max_bottom > 0 {
        Ok(Some((max_right as u32, max_bottom as u32)))
    } else {
        Ok(None)
    }
}

fn get_window_bounds_extent(conn: &RustConnection, root: Window) -> Result<Option<(u32, u32)>> {
    let windows = get_window_list(conn, root)?;
    let mut min_x = i64::MAX;
    let mut min_y = i64::MAX;
    let mut max_x = i64::MIN;
    let mut max_y = i64::MIN;

    for xid in windows {
        let Ok(geom) = conn.get_geometry(xid)?.reply() else { continue };
        if geom.width == 0 || geom.height == 0 {
            continue;
        }
        let trans = conn.translate_coordinates(xid, root, 0, 0)?.reply().ok();
        let (x, y) = trans.map(|t| (t.dst_x as i64, t.dst_y as i64)).unwrap_or((0, 0));
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x + geom.width as i64);
        max_y = max_y.max(y + geom.height as i64);
    }

    if min_x != i64::MAX && min_y != i64::MAX && max_x > min_x && max_y > min_y {
        Ok(Some(((max_x - min_x) as u32, (max_y - min_y) as u32)))
    } else {
        Ok(None)
    }
}

fn get_atom(conn: &RustConnection, name: &str) -> Result<Atom> {
    Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
}

fn get_window_pid(conn: &RustConnection, window: Window) -> Result<Option<u32>> {
    let atom = get_atom(conn, "_NET_WM_PID")?;
    let reply = conn.get_property(false, window, atom, AtomEnum::CARDINAL, 0, 1)?.reply()?;
    Ok(reply.value32().and_then(|mut i| i.next()))
}

fn get_window_title(conn: &RustConnection, window: Window) -> Result<String> {
    // Try _NET_WM_NAME (UTF-8) first.
    if let Ok(atom) = get_atom(conn, "_NET_WM_NAME") {
        if let Ok(utf8_atom) = get_atom(conn, "UTF8_STRING") {
            if let Ok(reply) = conn.get_property(false, window, atom, utf8_atom, 0, 1024)?.reply() {
                if !reply.value.is_empty() {
                    return Ok(String::from_utf8_lossy(&reply.value).into_owned());
                }
            }
        }
    }
    // Fallback: WM_NAME (latin-1 / ASCII).
    let reply = conn.get_property(false, window, AtomEnum::WM_NAME, AtomEnum::STRING, 0, 1024)?.reply()?;
    Ok(String::from_utf8_lossy(&reply.value).into_owned())
}

/// Return the WM_CLASS pair for `xid` as `(instance, class)`.
///
/// X11's `WM_CLASS` property is two NUL-separated strings; the first is
/// the instance name, the second is the class name. Either field can
/// be empty. Used by [`crate::terminal::is_terminal_window`] to detect
/// terminal emulators that share a process tree with another GUI
/// (e.g. Ghostty's `WM_CLASS = "ghostty\0Ghostty\0"`).
///
/// Returns `None` when no X connection is available, the window has no
/// WM_CLASS atom set, or the property could not be read.
pub fn wm_class_for_window(xid: u64) -> Option<(String, String)> {
    let (conn, _) = RustConnection::connect(None).ok()?;
    get_window_class(&conn, xid as u32)
}

fn get_window_class(conn: &RustConnection, xid: Window) -> Option<(String, String)> {
    let reply = conn
        .get_property(false, xid as u32, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 512)
        .ok()?
        .reply()
        .ok()?;
    let raw = reply.value;
    let mut parts = raw.split(|&b| b == 0).filter(|s| !s.is_empty());
    let instance = parts.next().map(|s| String::from_utf8_lossy(s).into_owned()).unwrap_or_default();
    let class = parts.next().map(|s| String::from_utf8_lossy(s).into_owned()).unwrap_or_default();
    if instance.is_empty() && class.is_empty() {
        return None;
    }
    Some((instance, class))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_present_client_list_does_not_fall_back_to_query_tree() {
        assert_eq!(client_list_property(1, &[]), Some([].as_slice()));
    }

    #[test]
    fn absent_client_list_allows_query_tree_fallback() {
        assert_eq!(client_list_property(x11rb::NONE, &[]), None);
    }

    #[test]
    fn query_tree_fallback_only_lists_viewable_windows() {
        assert!(fallback_window_is_listable(MapState::VIEWABLE));
        assert!(!fallback_window_is_listable(MapState::UNMAPPED));
        assert!(!fallback_window_is_listable(MapState::UNVIEWABLE));
    }
}
