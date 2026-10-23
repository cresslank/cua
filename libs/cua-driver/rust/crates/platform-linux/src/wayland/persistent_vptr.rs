//! Exact stateful Wayland pointer delivery for `mouse_button_down` / drag / up.
//!
//! Press through release is one transaction. Native Sway holds the host-global
//! raw-input lease and an exact `(pid, container)` focus guard for the whole
//! lifetime; nested cua-compositor delivery stays focus-free and surface-token
//! addressed. Other Wayland compositors fail closed rather than reopening a
//! generic foreign-toplevel session from a bare integer id.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::thread;

use crossbeam_channel::{bounded, Receiver, Sender};
use wayland_client::{protocol::wl_pointer::ButtonState, Connection};
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1;

use super::overlay::{TrackedSocket, WorkerControl, WorkerRegistry, WorkerSender};
use super::{evdev_pointer_button, ExactTargetProof};

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
        // A failed sync is NOT proof that already-queued injection was cancelled.
        pending: bool,
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

static OWNER: OnceLock<WorkerRegistry<Cmd>> = OnceLock::new();

fn registry() -> &'static WorkerRegistry<Cmd> {
    OWNER.get_or_init(WorkerRegistry::new)
}

fn tx() -> anyhow::Result<WorkerSender<Cmd>> {
    registry().sender("cua-persistent-vptr", 32, owner_thread)
}

/// New SDK generation only. Does not create a thread or compositor connection.
pub fn restart() -> anyhow::Result<()> {
    registry().restart()
}

/// Fence admission and join within 1s. Unconfirmed input keeps the worker and
/// raw-input/focus lease owned; an error must not be reported as clean teardown.
pub fn shutdown() -> anyhow::Result<()> {
    registry().shutdown()
}

fn has_pending(active: &HashMap<String, ActivePointer>) -> bool {
    active
        .values()
        .any(|entry| matches!(entry, ActivePointer::Native { pending: true, .. }))
}

fn owner_thread(rx: Receiver<Cmd>, control: Arc<WorkerControl>) {
    let mut active = HashMap::<String, ActivePointer>::new();
    let mut quarantine = Vec::new();
    let mut next_nested_cursor = 1_u64;
    loop {
        let cmd = crossbeam_channel::select_biased! {
            recv(control.stop_rx) -> _ => break,
            recv(rx) -> cmd => match cmd { Ok(cmd) => cmd, Err(_) => break },
        };
        if control.stopped() {
            break;
        }
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
                    &control,
                );
                let _ = reply.try_send(result);
            }
            Cmd::MoveTo {
                cursor_id,
                x,
                y,
                reply,
            } => {
                let result = handle_move(&mut active, &cursor_id, x, y, &mut quarantine, &control);
                let _ = reply.try_send(result);
            }
            Cmd::Release {
                cursor_id,
                button,
                reply,
            } => {
                let result = handle_release(&mut active, &cursor_id, button, &mut quarantine);
                let _ = reply.try_send(result);
            }
            Cmd::Forget { cursor_id, reply } => {
                let result = active
                    .remove(&cursor_id)
                    .map(|entry| terminalize(&cursor_id, entry, false, &mut quarantine))
                    .unwrap_or(Ok(()));
                let _ = reply.try_send(result);
            }
        }
        if has_pending(&active) || !quarantine.is_empty() {
            control.stop();
            break;
        }
    }
    // Discard all queued requests (including their reply channels); none may
    // execute after retirement. Keep native leases until final sync is confirmed.
    drop(rx);
    for (cursor_id, entry) in active.drain() {
        if let Err(error) = terminalize(&cursor_id, entry, true, &mut quarantine) {
            tracing::warn!("stateful Wayland retirement: {error}");
        }
    }
    if !quarantine.is_empty() {
        control.stop();
        tracing::error!("Wayland input consumption unconfirmed; retaining owned worker, connections and focus/raw-input leases");
        // There is no public protocol to revoke events already in the server's
        // receive queue. Never release a lease or detach its thread on this path.
        // shutdown/restart report the retained owner instead of claiming success.
        loop {
            thread::park();
        }
    }
    CONNS.with(|connections| connections.borrow_mut().clear());
}

// The general opener hides initialization roundtrips. This local equivalent
// registers the socket BEFORE the first roundtrip, so shutdown can cancel setup
// without ever weakening a raw-input lease that protects queued injection.
fn open_owned_session(
    control: &Arc<WorkerControl>,
) -> anyhow::Result<(super::VptrSession, TrackedSocket)> {
    let (conn, socket) = super::overlay::connect_tracked(control)?;
    initialize_session(conn, socket)
}

fn initialize_session(
    conn: Connection,
    socket: TrackedSocket,
) -> anyhow::Result<(super::VptrSession, TrackedSocket)> {
    let mut queue = conn.new_event_queue::<super::State>();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());
    let mut state = super::State::default();
    for _ in 0..5 {
        queue.roundtrip(&mut state)?;
    }
    let mgr = state.vptr_manager.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "compositor does not expose zwlr_virtual_pointer_manager_v1 ({})",
            super::NO_VPTR_MARKER
        )
    })?;
    let seat = state.seats.selected().ok_or_else(|| {
        anyhow::anyhow!("compositor exposed no wl_seat for virtual-pointer input")
    })?;
    let vptr = mgr.create_virtual_pointer(Some(&seat), &qh, ());
    let (origin_x, origin_y, output_w, output_h) =
        super::virtual_pointer_space((state.output_w, state.output_h))?;
    Ok((
        super::VptrSession {
            conn,
            queue,
            state,
            seat,
            vptr,
            output_w,
            output_h,
            origin_x,
            origin_y,
        },
        socket,
    ))
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
    control: &Arc<WorkerControl>,
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
        anyhow::ensure!(!control.stopped(), "Wayland worker generation retired");
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

    let (mut sess, socket) = open_owned_session(control)?;
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
    socket.protect_input()?;
    persist_conn(cursor_id, sess.conn.clone(), socket);
    active.insert(
        cursor_id.to_owned(),
        ActivePointer::Native {
            vptr: sess.vptr.clone(),
            button: btn,
            out_w: w,
            out_h: h,
            target: point.target,
            focus,
            pending: true,
            _lease: lease,
        },
    );
    // Ownership precedes the first event. A failed or stuck sync cannot unwind
    // the focus/raw-input lease while the compositor may still consume input.
    sess.vptr.motion_absolute(0, px as u32, py as u32, w, h);
    sess.vptr.frame();
    sess.vptr.button(0, btn, ButtonState::Pressed);
    sess.vptr.frame();
    sess.queue.roundtrip(&mut sess.state)?;
    if let Some(ActivePointer::Native { pending, .. }) = active.get_mut(cursor_id) {
        *pending = false;
    }
    Ok(())
}

fn handle_move(
    active: &mut HashMap<String, ActivePointer>,
    cursor_id: &str,
    x: i32,
    y: i32,
    quarantine: &mut Vec<ActivePointer>,
    control: &WorkerControl,
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
                pending,
                ..
            } => {
                super::validate_exact_target(target)?;
                let window = focus.validate()?;
                let (px, py) = local_to_sway_output(&window, x, y)?;
                if px < 0 || py < 0 || px >= i32::try_from(*out_w)? || py >= i32::try_from(*out_h)?
                {
                    anyhow::bail!("exact_target_mismatch: exact Sway point lies outside the virtual-pointer output");
                }
                anyhow::ensure!(!control.stopped(), "Wayland worker generation retired");
                *pending = true;
                vptr.motion_absolute(0, px as u32, py as u32, *out_w, *out_h);
                vptr.frame();
                roundtrip_on_persistent(cursor_id)?;
                *pending = false;
                Ok(())
            }
            ActivePointer::Nested {
                target,
                surface,
                cursor_index,
                ..
            } => {
                let (x, y) = checked_nested_local(target, x, y)?;
                anyhow::ensure!(!control.stopped(), "Wayland worker generation retired");
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
            let cleanup = terminalize(cursor_id, entry, true, quarantine);
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
    quarantine: &mut Vec<ActivePointer>,
) -> anyhow::Result<()> {
    let entry = active
        .remove(cursor_id)
        .ok_or_else(|| anyhow::anyhow!("no held mouse button for cursor '{cursor_id}'"))?;
    let expected = evdev_pointer_button(button);
    let actual = match &entry {
        ActivePointer::Native { button, .. } | ActivePointer::Nested { button, .. } => *button,
    };
    if actual != expected {
        let cleanup = terminalize(cursor_id, entry, true, quarantine);
        cleanup?;
        anyhow::bail!("stateful pointer release button did not match the held button");
    }
    terminalize(cursor_id, entry, true, quarantine)
}

fn terminalize(
    cursor_id: &str,
    mut entry: ActivePointer,
    emit_release: bool,
    quarantine: &mut Vec<ActivePointer>,
) -> anyhow::Result<()> {
    if let ActivePointer::Native {
        vptr,
        button,
        target,
        focus,
        pending,
        ..
    } = &mut entry
    {
        if *pending {
            quarantine.push(entry);
            anyhow::bail!("input consumption unconfirmed; focus/raw-input lease retained");
        }
        let validation =
            super::validate_exact_target(target).and_then(|()| focus.validate().map(|_| ()));
        // Even a validation refusal must destroy and confirm the virtual device
        // before restoring focus. A disconnect alone cannot cancel queued input.
        *pending = true;
        if emit_release && validation.is_ok() {
            vptr.button(0, *button, ButtonState::Released);
            vptr.frame();
        }
        vptr.destroy();
        if let Err(error) = roundtrip_on_persistent(cursor_id) {
            quarantine.push(entry);
            return Err(error
                .context("virtual device retirement unconfirmed; focus/raw-input lease retained"));
        }
        *pending = false;
        forget_conn(cursor_id);
        if let ActivePointer::Native { focus, _lease, .. } = entry {
            let restoration = focus.finish();
            // Explicit binding keeps the lease through confirmed destruction
            // AND focus restoration, including any restoration error.
            drop(_lease);
            return match (validation, restoration) {
                (Ok(()), Ok(())) => Ok(()),
                (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
                (Err(error), Err(restore)) => {
                    Err(error.context(format!("Sway focus restoration also failed: {restore}")))
                }
            };
        }
        unreachable!();
    }
    if let ActivePointer::Nested {
        target,
        surface,
        cursor_index,
        button,
    } = entry
    {
        super::validate_exact_target(&target)?;
        if emit_release {
            nested_lines(
                &surface,
                cursor_index,
                &[format!("b {surface} {cursor_index} {button} 0")],
            )?;
        }
    }
    Ok(())
}

thread_local! {
    static CONNS: std::cell::RefCell<HashMap<String, (Connection, wayland_client::EventQueue<super::State>, TrackedSocket)>>
        = std::cell::RefCell::new(HashMap::new());
}

fn persist_conn(cursor_id: &str, conn: Connection, socket: TrackedSocket) {
    let queue = conn.new_event_queue::<super::State>();
    CONNS.with(|connections| {
        connections
            .borrow_mut()
            .insert(cursor_id.to_owned(), (conn, queue, socket));
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
        let (_, queue, _) = connections
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
    press_with_sender(tx()?, cursor_id, target, x, y, button)
}

fn press_with_sender(
    sender: WorkerSender<Cmd>,
    cursor_id: &str,
    target: ExactTargetProof,
    x: i32,
    y: i32,
    button: u8,
) -> anyhow::Result<()> {
    let (reply, receive) = bounded(1);
    sender
        .send(Cmd::Press {
            cursor_id: cursor_id.to_owned(),
            point: TargetPoint { target, x, y },
            button,
            reply,
        })
        .map_err(|error| anyhow::anyhow!("cua-persistent-vptr thread is dead: {error}"))?;
    sender.reply(receive)
}

/// Source-compatible wrapper for callers of this public module. Promote one
/// unique compositor-owned id to an immutable exact proof before entering the
/// stateful transaction; missing/ambiguous ids or unproven pids fail closed.
pub fn press(cursor_id: &str, window_id: u64, x: i32, y: i32, button: u8) -> anyhow::Result<()> {
    let sender = tx()?;
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
    press_with_sender(sender, cursor_id, target, x, y, button)
}

pub fn move_to(cursor_id: &str, x: i32, y: i32) -> anyhow::Result<()> {
    let sender = tx()?;
    let (reply, receive) = bounded(1);
    sender
        .send(Cmd::MoveTo {
            cursor_id: cursor_id.to_owned(),
            x,
            y,
            reply,
        })
        .map_err(|error| anyhow::anyhow!("cua-persistent-vptr thread is dead: {error}"))?;
    sender.reply(receive)
}

pub fn release(cursor_id: &str, button: u8) -> anyhow::Result<()> {
    let sender = tx()?;
    let (reply, receive) = bounded(1);
    sender
        .send(Cmd::Release {
            cursor_id: cursor_id.to_owned(),
            button,
            reply,
        })
        .map_err(|error| anyhow::anyhow!("cua-persistent-vptr thread is dead: {error}"))?;
    sender.reply(receive)
}

pub fn forget(cursor_id: &str) -> anyhow::Result<()> {
    let sender = tx()?;
    let (reply, receive) = bounded(1);
    sender
        .send(Cmd::Forget {
            cursor_id: cursor_id.to_owned(),
            reply,
        })
        .map_err(|error| anyhow::anyhow!("cua-persistent-vptr thread is dead: {error}"))?;
    sender.reply(receive)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owned_pointer_idle_retirement_rejects_old_requests_and_restarts() {
        let registry = WorkerRegistry::<Cmd>::new();
        registry.shutdown().unwrap();
        registry.shutdown().unwrap();
        registry.restart().unwrap();
        let old = registry
            .sender("pointer-test-idle", 2, owner_thread)
            .unwrap();
        let (reply, receive) = bounded(1);
        // Forget of an absent fixture performs no compositor or input operation.
        old.send(Cmd::Forget {
            cursor_id: "absent-fixture".into(),
            reply,
        })
        .unwrap();
        old.reply(receive).unwrap();
        registry.shutdown().unwrap();
        let (reply, _) = bounded(1);
        assert!(old
            .send(Cmd::Forget {
                cursor_id: "old-fixture".into(),
                reply
            })
            .is_err());
        assert!(registry
            .sender("pointer-test-denied", 2, owner_thread)
            .is_err());
        registry.restart().unwrap();
        let fresh = registry
            .sender("pointer-test-fresh", 2, owner_thread)
            .unwrap();
        assert!(!Arc::ptr_eq(&old.control, &fresh.control));
        let (reply, receive) = bounded(1);
        fresh
            .send(Cmd::Forget {
                cursor_id: "fresh-fixture".into(),
                reply,
            })
            .unwrap();
        fresh.reply(receive).unwrap();
        registry.shutdown().unwrap();
    }

    #[test]
    fn owned_pointer_initialization_roundtrip_is_cancelled_and_socket_closed() {
        use std::io::Read;
        use std::time::Duration;
        let registry = WorkerRegistry::<()>::new();
        let (client, mut peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let _sender = registry
            .sender("pointer-test-stalled-setup", 1, move |_, control| {
                let conn = Connection::from_socket(client).unwrap();
                let socket = control.track(&conn).unwrap();
                assert!(initialize_session(conn, socket).is_err());
            })
            .unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let mut requests = [0u8; 24];
        // get_registry and sync, with no advertised globals and no input device.
        peer.read_exact(&mut requests).unwrap();
        assert_eq!(
            u32::from_ne_bytes(requests[4..8].try_into().unwrap()),
            (12 << 16) | 1
        );
        assert_eq!(
            u32::from_ne_bytes(requests[16..20].try_into().unwrap()),
            12 << 16
        );
        registry.shutdown().unwrap();
        let mut byte = [0u8; 1];
        assert_eq!(peer.read(&mut byte).unwrap(), 0);
        registry.shutdown().unwrap();
    }

    // The tool passes local coordinates. Conversion belongs here, after exact
    // target/focus revalidation, rather than in the upstream tool-side helper.
    #[test]
    fn persistent_wayland_pointer_coordinates_include_window_origin() {
        let mut window = super::super::sway_ipc::Window {
            id: 7,
            pid: 42,
            title: String::new(),
            app_id: String::new(),
            x: 100,
            y: 200,
            width: 800,
            height: 600,
            content_x: 0,
            content_y: 0,
            focused: true,
            visible: true,
            fullscreen: false,
        };
        assert_eq!(local_to_sway_output(&window, 10, 20).unwrap(), (110, 220));
        // The next event must use the newly validated origin, not the origin
        // observed when the hold began.
        window.x = 300;
        window.y = 400;
        assert_eq!(local_to_sway_output(&window, 10, 20).unwrap(), (310, 420));
    }

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
