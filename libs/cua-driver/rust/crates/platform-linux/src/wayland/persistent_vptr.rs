//! Exact stateful Wayland pointer delivery for `mouse_button_down` / drag / up.
//!
//! Press through release is one transaction. Native Sway holds the host-global
//! raw-input lease and an exact `(pid, container)` focus guard for the whole
//! lifetime; nested cua-compositor delivery stays focus-free and surface-token
//! addressed. Other Wayland compositors fail closed rather than reopening a
//! generic foreign-toplevel session from a bare integer id.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::thread;

use crossbeam_channel::{bounded, Receiver, Sender};
use wayland_client::{protocol::wl_pointer::ButtonState, Connection};
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1;

use super::{evdev_pointer_button, open_vptr_session, ExactTargetProof};

#[derive(Clone)]
struct TargetPoint {
    target: ExactTargetProof,
    x: i32,
    y: i32,
}

enum Cmd {
    Press {
        cursor_id: String,
        point: TargetPoint,
        button: u8,
        reply: Sender<anyhow::Result<()>>,
    },
    MoveTo {
        cursor_id: String,
        x: i32,
        y: i32,
        reply: Sender<anyhow::Result<()>>,
    },
    Release {
        cursor_id: String,
        button: u8,
        reply: Sender<anyhow::Result<()>>,
    },
    Forget {
        cursor_id: String,
        reply: Sender<anyhow::Result<()>>,
    },
}

enum ActivePointer {
    Native {
        vptr: ZwlrVirtualPointerV1,
        button: u32,
        out_w: u32,
        out_h: u32,
        target: ExactTargetProof,
        focus: super::sway_ipc::StatefulFocus,
        // Declared last so focus restoration happens before the lease drops.
        _lease: super::HostRawInputLease,
    },
    Nested {
        target: ExactTargetProof,
        surface: String,
        cursor_index: u64,
        button: u32,
    },
}

static TX: OnceLock<Sender<Cmd>> = OnceLock::new();

fn tx() -> &'static Sender<Cmd> {
    TX.get_or_init(|| {
        let (tx, rx) = bounded::<Cmd>(32);
        thread::Builder::new()
            .name("cua-persistent-vptr".into())
            .spawn(move || owner_thread(rx))
            .expect("spawn cua-persistent-vptr thread");
        tx
    })
}

fn owner_thread(rx: Receiver<Cmd>) {
    let mut active = HashMap::<String, ActivePointer>::new();
    let mut next_nested_cursor = 1_u64;
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::Press {
                cursor_id,
                point,
                button,
                reply,
            } => {
                let result = handle_press(
                    &mut active,
                    &mut next_nested_cursor,
                    &cursor_id,
                    point,
                    button,
                );
                let _ = reply.send(result);
            }
            Cmd::MoveTo {
                cursor_id,
                x,
                y,
                reply,
            } => {
                let result = handle_move(&mut active, &cursor_id, x, y);
                let _ = reply.send(result);
            }
            Cmd::Release {
                cursor_id,
                button,
                reply,
            } => {
                let result = handle_release(&mut active, &cursor_id, button);
                let _ = reply.send(result);
            }
            Cmd::Forget { cursor_id, reply } => {
                let result = active
                    .remove(&cursor_id)
                    .map(|entry| terminalize(&cursor_id, entry, false))
                    .unwrap_or(Ok(()));
                let _ = reply.send(result);
            }
        }
    }
}

fn local_to_sway_output(
    window: &super::sway_ipc::Window,
    x: i32,
    y: i32,
) -> anyhow::Result<(i32, i32)> {
    let content_width = i32::try_from(window.width)?.saturating_sub(window.content_x.max(0));
    let content_height = i32::try_from(window.height)?.saturating_sub(window.content_y.max(0));
    if x < 0 || y < 0 || x >= content_width || y >= content_height {
        anyhow::bail!("exact_target_mismatch: stateful pointer point lies outside the exact Sway client surface");
    }
    Ok((
        window
            .x
            .checked_add(window.content_x)
            .and_then(|v| v.checked_add(x))
            .ok_or_else(|| anyhow::anyhow!("stateful pointer x coordinate overflowed"))?,
        window
            .y
            .checked_add(window.content_y)
            .and_then(|v| v.checked_add(y))
            .ok_or_else(|| anyhow::anyhow!("stateful pointer y coordinate overflowed"))?,
    ))
}

fn checked_nested_local(target: &ExactTargetProof, x: i32, y: i32) -> anyhow::Result<(f64, f64)> {
    super::validate_exact_target(target)?;
    let window = super::list_windows_dispatch(Some(target.pid()))
        .into_iter()
        .find(|window| window.xid == target.window_id())
        .ok_or_else(|| anyhow::anyhow!("stale_target: nested stateful pointer target changed"))?;
    if x < 0 || y < 0 || x >= i32::try_from(window.width)? || y >= i32::try_from(window.height)? {
        anyhow::bail!(
            "exact_target_mismatch: stateful pointer point lies outside the exact nested surface"
        );
    }
    Ok((f64::from(x), f64::from(y)))
}

fn nested_lines(surface: &str, cursor: u64, lines: &[String]) -> anyhow::Result<()> {
    if !surface.starts_with("surface:") {
        anyhow::bail!("exact_target_unavailable: nested stateful pointer lost its surface token");
    }
    let expected = format!(" {surface} {cursor} ");
    if lines
        .iter()
        .any(|line| !format!(" {line} ").contains(&expected))
    {
        anyhow::bail!("exact_target_mismatch: mixed target in nested stateful pointer batch");
    }
    super::inject_send(lines)
}

fn handle_press(
    active: &mut HashMap<String, ActivePointer>,
    next_nested_cursor: &mut u64,
    cursor_id: &str,
    point: TargetPoint,
    button: u8,
) -> anyhow::Result<()> {
    if active.contains_key(cursor_id) {
        anyhow::bail!("cursor {cursor_id:?} already owns a stateful Wayland pointer transaction");
    }
    super::validate_exact_target(&point.target)?;
    let btn = evdev_pointer_button(button);

    if super::is_inject_mode() {
        let (x, y) = checked_nested_local(&point.target, point.x, point.y)?;
        let surface = super::inject_target_for_window(point.target.window_id())?;
        let cursor_index = *next_nested_cursor;
        *next_nested_cursor = next_nested_cursor.checked_add(1).unwrap_or(1);
        nested_lines(
            &surface,
            cursor_index,
            &[
                format!("m {surface} {cursor_index} {x:.1} {y:.1}"),
                format!("b {surface} {cursor_index} {btn} 1"),
            ],
        )?;
        active.insert(
            cursor_id.to_owned(),
            ActivePointer::Nested {
                target: point.target,
                surface,
                cursor_index,
                button: btn,
            },
        );
        return Ok(());
    }

    // Only Sway currently exposes the typed id+pid focus/geometry adapter needed
    // by a stateful native virtual pointer. Never fall back to generic activation.
    let lease = super::acquire_host_raw_input_lease()?;
    super::validate_exact_target(&point.target)?;
    let focus =
        super::sway_ipc::StatefulFocus::begin(point.target.pid(), point.target.window_id())?;

    let mut sess = open_vptr_session(None)?;
    let (w, h) = (sess.output_w, sess.output_h);
    // Opening the protocol objects performs compositor round-trips. Revalidate
    // after those waits and derive coordinates from the same fresh Sway tree,
    // immediately before the first stateful event.
    super::validate_exact_target(&point.target)?;
    let window = focus.validate()?;
    let (px, py) = local_to_sway_output(&window, point.x, point.y)?;
    if px < 0 || py < 0 || px >= i32::try_from(w)? || py >= i32::try_from(h)? {
        anyhow::bail!(
            "exact_target_mismatch: exact Sway point lies outside the virtual-pointer output"
        );
    }
    sess.vptr.motion_absolute(0, px as u32, py as u32, w, h);
    sess.vptr.frame();
    sess.vptr.button(0, btn, ButtonState::Pressed);
    sess.vptr.frame();
    sess.queue.roundtrip(&mut sess.state)?;
    let vptr = sess.vptr.clone();
    persist_conn(cursor_id, sess.conn);
    active.insert(
        cursor_id.to_owned(),
        ActivePointer::Native {
            vptr,
            button: btn,
            out_w: w,
            out_h: h,
            target: point.target,
            focus,
            _lease: lease,
        },
    );
    Ok(())
}

fn handle_move(
    active: &mut HashMap<String, ActivePointer>,
    cursor_id: &str,
    x: i32,
    y: i32,
) -> anyhow::Result<()> {
    let result = (|| -> anyhow::Result<()> {
        match active.get_mut(cursor_id).ok_or_else(|| {
            anyhow::anyhow!(
                "no held mouse button for cursor '{cursor_id}'; call mouse_button_down first"
            )
        })? {
            ActivePointer::Native {
                vptr,
                out_w,
                out_h,
                target,
                focus,
                ..
            } => {
                super::validate_exact_target(target)?;
                let window = focus.validate()?;
                let (px, py) = local_to_sway_output(&window, x, y)?;
                if px < 0 || py < 0 || px >= i32::try_from(*out_w)? || py >= i32::try_from(*out_h)?
                {
                    anyhow::bail!("exact_target_mismatch: exact Sway point lies outside the virtual-pointer output");
                }
                vptr.motion_absolute(0, px as u32, py as u32, *out_w, *out_h);
                vptr.frame();
                roundtrip_on_persistent(cursor_id)
            }
            ActivePointer::Nested {
                target,
                surface,
                cursor_index,
                ..
            } => {
                let (x, y) = checked_nested_local(target, x, y)?;
                nested_lines(
                    surface,
                    *cursor_index,
                    &[format!("m {surface} {cursor_index} {x:.1} {y:.1}")],
                )
            }
        }
    })();
    if let Err(error) = result {
        if let Some(entry) = active.remove(cursor_id) {
            let cleanup = terminalize(cursor_id, entry, true);
            return match cleanup {
                Ok(()) => Err(error),
                Err(cleanup) => {
                    Err(error.context(format!("stateful pointer cleanup also failed: {cleanup}")))
                }
            };
        }
        return Err(error);
    }
    Ok(())
}

fn handle_release(
    active: &mut HashMap<String, ActivePointer>,
    cursor_id: &str,
    button: u8,
) -> anyhow::Result<()> {
    let entry = active
        .remove(cursor_id)
        .ok_or_else(|| anyhow::anyhow!("no held mouse button for cursor '{cursor_id}'"))?;
    let expected = evdev_pointer_button(button);
    let actual = match &entry {
        ActivePointer::Native { button, .. } | ActivePointer::Nested { button, .. } => *button,
    };
    if actual != expected {
        let cleanup = terminalize(cursor_id, entry, true);
        cleanup?;
        anyhow::bail!("stateful pointer release button did not match the held button");
    }
    terminalize(cursor_id, entry, true)
}

fn terminalize(cursor_id: &str, entry: ActivePointer, emit_release: bool) -> anyhow::Result<()> {
    match entry {
        ActivePointer::Native {
            vptr,
            button,
            target,
            focus,
            ..
        } => {
            let action = (|| {
                super::validate_exact_target(&target)?;
                focus.validate()?;
                if emit_release {
                    vptr.button(0, button, ButtonState::Released);
                    vptr.frame();
                    roundtrip_on_persistent(cursor_id)?;
                }
                vptr.destroy();
                let _ = roundtrip_on_persistent(cursor_id);
                Ok(())
            })();
            forget_conn(cursor_id);
            let restoration = focus.finish();
            match (action, restoration) {
                (Ok(()), Ok(())) => Ok(()),
                (Err(error), Ok(())) => Err(error),
                (Ok(()), Err(error)) => Err(error),
                (Err(error), Err(restore)) => {
                    Err(error.context(format!("Sway focus restoration also failed: {restore}")))
                }
            }
        }
        ActivePointer::Nested {
            target,
            surface,
            cursor_index,
            button,
        } => {
            super::validate_exact_target(&target)?;
            if emit_release {
                nested_lines(
                    &surface,
                    cursor_index,
                    &[format!("b {surface} {cursor_index} {button} 0")],
                )?;
            }
            Ok(())
        }
    }
}

thread_local! {
    static CONNS: std::cell::RefCell<HashMap<String, (Connection, wayland_client::EventQueue<super::State>)>>
        = std::cell::RefCell::new(HashMap::new());
}

fn persist_conn(cursor_id: &str, conn: Connection) {
    let queue = conn.new_event_queue::<super::State>();
    CONNS.with(|connections| {
        connections
            .borrow_mut()
            .insert(cursor_id.to_owned(), (conn, queue));
    });
}

fn forget_conn(cursor_id: &str) {
    CONNS.with(|connections| {
        connections.borrow_mut().remove(cursor_id);
    });
}

fn roundtrip_on_persistent(cursor_id: &str) -> anyhow::Result<()> {
    CONNS.with(|connections| {
        let mut connections = connections.borrow_mut();
        let (_, queue) = connections
            .get_mut(cursor_id)
            .ok_or_else(|| anyhow::anyhow!("no persistent connection for cursor '{cursor_id}'"))?;
        queue
            .roundtrip(&mut super::State::default())
            .map_err(|error| anyhow::anyhow!("compositor roundtrip failed: {error}"))?;
        Ok(())
    })
}

pub fn press_exact(
    cursor_id: &str,
    target: ExactTargetProof,
    x: i32,
    y: i32,
    button: u8,
) -> anyhow::Result<()> {
    let (reply, receive) = bounded(1);
    tx().send(Cmd::Press {
        cursor_id: cursor_id.to_owned(),
        point: TargetPoint { target, x, y },
        button,
        reply,
    })
    .map_err(|error| anyhow::anyhow!("cua-persistent-vptr thread is dead: {error}"))?;
    receive
        .recv()
        .map_err(|error| anyhow::anyhow!("reply channel closed: {error}"))?
}

/// Source-compatible wrapper for callers of this public module. Promote one
/// unique compositor-owned id to an immutable exact proof before entering the
/// stateful transaction; missing/ambiguous ids or unproven pids fail closed.
pub fn press(cursor_id: &str, window_id: u64, x: i32, y: i32, button: u8) -> anyhow::Result<()> {
    let matches = super::list_windows_dispatch(None)
        .into_iter()
        .filter(|window| window.xid == window_id)
        .collect::<Vec<_>>();
    let [window] = matches.as_slice() else {
        anyhow::bail!(
            "exact_target_unavailable: stateful pointer window id is missing or ambiguous"
        );
    };
    let pid = window.pid.ok_or_else(|| {
        anyhow::anyhow!(
            "exact_target_unavailable: stateful pointer target has no compositor-attested pid"
        )
    })?;
    let target = super::establish_exact_target(pid, window_id)?;
    press_exact(cursor_id, target, x, y, button)
}

pub fn move_to(cursor_id: &str, x: i32, y: i32) -> anyhow::Result<()> {
    let (reply, receive) = bounded(1);
    tx().send(Cmd::MoveTo {
        cursor_id: cursor_id.to_owned(),
        x,
        y,
        reply,
    })
    .map_err(|error| anyhow::anyhow!("cua-persistent-vptr thread is dead: {error}"))?;
    receive
        .recv()
        .map_err(|error| anyhow::anyhow!("reply channel closed: {error}"))?
}

pub fn release(cursor_id: &str, button: u8) -> anyhow::Result<()> {
    let (reply, receive) = bounded(1);
    tx().send(Cmd::Release {
        cursor_id: cursor_id.to_owned(),
        button,
        reply,
    })
    .map_err(|error| anyhow::anyhow!("cua-persistent-vptr thread is dead: {error}"))?;
    receive
        .recv()
        .map_err(|error| anyhow::anyhow!("reply channel closed: {error}"))?
}

pub fn forget(cursor_id: &str) -> anyhow::Result<()> {
    let (reply, receive) = bounded(1);
    tx().send(Cmd::Forget {
        cursor_id: cursor_id.to_owned(),
        reply,
    })
    .map_err(|error| anyhow::anyhow!("cua-persistent-vptr thread is dead: {error}"))?;
    receive
        .recv()
        .map_err(|error| anyhow::anyhow!("reply channel closed: {error}"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sway_local_coordinates_include_container_and_content_origins() {
        let window = super::super::sway_ipc::Window {
            id: 9,
            pid: 42,
            title: String::new(),
            app_id: String::new(),
            x: 100,
            y: 200,
            width: 800,
            height: 600,
            content_x: 3,
            content_y: 27,
            focused: true,
            visible: true,
            fullscreen: false,
        };
        assert_eq!(local_to_sway_output(&window, 10, 20).unwrap(), (113, 247));
        assert!(local_to_sway_output(&window, -1, 20).is_err());
        assert!(local_to_sway_output(&window, 10, 573).is_err());
    }

    #[test]
    fn nested_batches_reject_mixed_surface_or_cursor() {
        let line = "m surface:one 7 1.0 2.0".to_owned();
        assert!(nested_lines("not-a-surface", 7, &[line.clone()]).is_err());
        assert!(nested_lines("surface:one", 8, &[line]).is_err());
    }
}
