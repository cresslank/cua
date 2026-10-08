//! Secondary element clicks never rejoin a pixel/ordinal ladder. Native I/O
//! seams leave qualification, per-click validation and accounting in production.
use super::{ToolResult, ToolState};
use crate::input::delivery::{background_unavailable_error, BackgroundUnavailable, DeliveryMode};
use anyhow::{Context, Result};
use cua_driver_core::element_token::MutationPermit;
use cua_driver_core::{action_record::*, element_token::SnapshotIdentity};
use serde_json::json;
use std::{
    cell::Cell,
    sync::Arc,
    time::{Duration, Instant},
};

type Bounds = (i32, i32, u32, u32);
type Point = (i32, i32);

pub(super) fn unqualified(detail: &str) -> ToolResult {
    ToolResult::error(format!("element_route_unqualified: {detail}; retry with x,y pixel coordinates instead of element_token"))
        .with_structured(json!({"code":"element_route_unqualified", "effect":"refused",
            "refusal":{"code":"element_route_unqualified"}}))
}

pub(super) fn qualify_args(
    args: &serde_json::Value,
    delivery: DeliveryMode,
    button: u8,
    count: usize,
) -> Result<(), ToolResult> {
    use cua_driver_core::tool_args::ArgsExt;
    if args.get("element_token").is_some() || args.get("element_index").is_some() {
        qualify(delivery, button, count, &args.str_array("modifier"))
    } else {
        Ok(())
    }
}

pub(super) fn qualify(
    delivery: DeliveryMode,
    button: u8,
    count: usize,
    modifiers: &[String],
) -> Result<(), ToolResult> {
    qualify_on_platform(
        delivery,
        button,
        count,
        modifiers,
        crate::wayland::wayland_input_enabled(),
    )
}

fn qualify_on_platform(
    delivery: DeliveryMode,
    button: u8,
    count: usize,
    modifiers: &[String],
    wayland: bool,
) -> Result<(), ToolResult> {
    if wayland && !modifiers.is_empty() {
        let detail = "modified element clicks are unavailable on native Wayland: the pointer route cannot carry keyboard modifier state";
        let code = if delivery.is_foreground() {
            "element_route_unqualified"
        } else {
            "background_unavailable"
        };
        return Err(ToolResult::error(detail).with_structured(json!({
            "code":code,"effect":"refused","refusal":{"code":code}
        })));
    }
    let plain = button == 1 && count == 1 && modifiers.is_empty();
    if !plain && !delivery.is_foreground() {
        return Err(background_unavailable_error(
            BackgroundUnavailable::FocusedInputOnly,
        ));
    }
    if !plain && !(modifiers.is_empty() && matches!((button, count), (3, 1) | (1, 2))) {
        return Err(unqualified(
            "this element click form cannot fall through to pointer injection",
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Geometry {
    window: Bounds,
    content: Point,
    output: (u32, u32),
}

fn contains((x, y, w, h): Bounds, point: Point) -> bool {
    w > 0
        && h > 0
        && i64::from(point.0) >= i64::from(x)
        && i64::from(point.1) >= i64::from(y)
        && i64::from(point.0) < i64::from(x) + i64::from(w)
        && i64::from(point.1) < i64::from(y) + i64::from(h)
}

fn checked_point(bounds: Bounds, geometry: Geometry) -> Result<Point> {
    anyhow::ensure!(bounds.2 > 0 && bounds.3 > 0, "empty retained bounds");
    let point = (
        i32::try_from(i64::from(bounds.0) + i64::from(bounds.2 / 2))?,
        i32::try_from(i64::from(bounds.1) + i64::from(bounds.3 / 2))?,
    );
    anyhow::ensure!(
        contains(bounds, point)
            && contains(geometry.window, point)
            && contains((0, 0, geometry.output.0, geometry.output.1), point),
        "retained point outside window/output"
    );
    Ok(point)
}

trait Pointer {
    fn geometry(&self) -> Result<Geometry>;
    fn bounds(&self, geometry: Geometry) -> Result<Bounds>;
    fn verify(&self) -> Result<()>;
    /// Reuses ONE native transport for the sequence. Calls `before` after
    /// completed setup/motion, directly before EACH press. Acknowledges each
    /// release BEFORE any fallible cleanup, even if this call then returns Err.
    fn click(
        &self,
        point: Point,
        geometry: Geometry,
        button: u8,
        count: usize,
        before: &mut dyn FnMut() -> Result<()>,
        ack: &mut dyn FnMut(),
    ) -> Result<()>;
}

trait Target: Send {
    fn context_menu(&self) -> Result<bool>;
    fn hyprland(&self) -> bool;
    fn transport(&self) -> ActionTransport;
    fn foreground(
        &self,
        permit: Arc<MutationPermit>,
        body: &mut dyn FnMut(&dyn Pointer) -> Result<()>,
    ) -> Result<()>;
}

fn pointer_sequence(
    io: &dyn Pointer,
    button: u8,
    count: usize,
    acknowledged: &Cell<u32>,
) -> Result<()> {
    let geometry = io.geometry()?;
    let point = checked_point(io.bounds(geometry)?, geometry)?;
    let first_ack = Cell::new(None::<Instant>);
    io.click(
        point,
        geometry,
        button,
        count,
        &mut || {
            // Re-read one authoritative geometry, use that SAME geometry for
            // live retained bounds, and dispatch only the original screen point.
            let live_geometry = io.geometry()?;
            anyhow::ensure!(live_geometry == geometry, "window/output geometry changed");
            anyhow::ensure!(
                contains(io.bounds(live_geometry)?, point),
                "retained bounds moved"
            );
            io.verify()?;
            // Revalidation can block. Check AFTER it, at the native press
            // boundary, so a late second click is refused, never dispatched.
            anyhow::ensure!(
                first_ack
                    .get()
                    .is_none_or(|at| at.elapsed() < Duration::from_millis(300)),
                "inter-click deadline expired; refusing late second click"
            );
            Ok(())
        },
        &mut || {
            if first_ack.get().is_none() {
                first_ack.set(Some(Instant::now()));
            }
            acknowledged.set(acknowledged.get() + 1);
        },
    )
}

fn outcome(result: Result<()>, acknowledged: u32, transport: ActionTransport) -> ToolResult {
    let complete = result.is_ok();
    let detail = match result {
        Ok(()) => format!("Acknowledged {acknowledged} click(s); effect remains unverified."),
        Err(error) => format!("Stopped after {acknowledged} acknowledged click(s): {error:#}. Refresh state; never replay this request."),
    };
    let mut record = ActionExecutionRecord::builder(
        if complete {
            ActionEffect::Unverifiable
        } else if acknowledged > 0 {
            ActionEffect::Partial
        } else {
            ActionEffect::Unverifiable
        },
        transport,
        RequestedDelivery::Foreground,
    )
    .actual_delivery(if complete {
        ActualDelivery::Foreground
    } else {
        ActualDelivery::Unknown
    })
    .detail(detail.clone());
    if acknowledged > 0 {
        record = record.delivered_count(acknowledged);
    }
    let record = record.build().expect("secondary click progress");
    let mut public = serde_json::to_value(record.public_result().unwrap()).unwrap();
    if !complete {
        public["code"] = json!("stale_element_token");
        public["refusal"] = json!({"code":"stale_element_token"});
    }
    // Explicit zero is useful to consumers distinguishing a failed admission
    // from indeterminate dispatch; never turn it into a claim of no attempt.
    public["delivery"]["delivered_count"] = json!(acknowledged);
    (if complete {
        ToolResult::text(detail)
    } else {
        ToolResult::error(detail)
    })
    .with_structured(public)
    .with_action_record(record)
}

fn run(target: &dyn Target, permit: Arc<MutationPermit>, button: u8, count: usize) -> ToolResult {
    if button == 3 {
        match target.context_menu() {
            Ok(true) => return outcome(Ok(()), 1, ActionTransport::LinuxAtSpiAction),
            Ok(false) => {}
            Err(error) => return outcome(Err(error), 0, ActionTransport::LinuxAtSpiAction),
        }
    }
    if target.hyprland() {
        return unqualified("Hyprland pointer transport can reactivate after validation");
    }
    let acknowledged = Cell::new(0);
    let result = target.foreground(permit, &mut |io| {
        pointer_sequence(io, button, count, &acknowledged)
    });
    if acknowledged.get() == 0
        && result
            .as_ref()
            .err()
            .is_some_and(|error| error.to_string().contains("element_route_unqualified"))
    {
        return unqualified(&result.unwrap_err().to_string());
    }
    outcome(result, acknowledged.get(), target.transport())
}

pub(super) async fn invoke(
    state: &Arc<ToolState>,
    pid: u32,
    xid: u64,
    index: usize,
    identity: SnapshotIdentity,
    delivery: DeliveryMode,
    button: u8,
    count: usize,
) -> ToolResult {
    if let Err(refusal) = qualify(delivery, button, count, &[]) {
        return refusal;
    }
    let snapshots = state.snapshots.clone();
    #[cfg(test)]
    let backend = state.observed_click_backend.clone();
    // The native worker owns the mutation permit AND the blocking framework's
    // retained core ActionLease through activation, dispatch and all cleanup.
    let task = cua_driver_core::blocking::spawn(move || -> Result<ToolResult> {
        let (permit, identity) = snapshots
            .acquire_observed_mutation(identity, index)
            .map_err(|error| anyhow::anyhow!("stale_element_token: {error}"))?;
        let permit = Arc::new(permit);
        crate::atspi::snapshot::with_retained_mutation(permit.clone(), || {
            #[cfg(test)]
            if let Some(backend) = backend {
                return Ok(run(&backend, permit, button, count));
            }
            let wayland = crate::wayland::wayland_input_enabled();
            let proof = if wayland {
                Some(crate::wayland::establish_exact_target(pid, xid)?)
            } else {
                None
            };
            let target =
                crate::atspi::native::resolve_observed_target(pid, index, xid, &identity, proof)?;
            Ok(run(
                &Native {
                    target,
                    pid,
                    xid,
                    wayland,
                },
                permit,
                button,
                count,
            ))
        })
    })
    .await;
    match task {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => outcome(Err(error), 0, ActionTransport::LinuxAtSpiAction),
        Err(error) => outcome(Err(error.into()), 0, ActionTransport::LinuxAtSpiAction),
    }
}

struct Native {
    target: crate::atspi::ObservedClickTarget,
    pid: u32,
    xid: u64,
    wayland: bool,
}

impl Target for Native {
    fn context_menu(&self) -> Result<bool> {
        self.target.context_menu()
    }
    fn hyprland(&self) -> bool {
        crate::wayland::hyprland::is_session()
    }
    fn transport(&self) -> ActionTransport {
        if self.wayland {
            ActionTransport::LinuxWaylandVirtualPointer
        } else {
            ActionTransport::LinuxXTest
        }
    }
    fn foreground(
        &self,
        permit: Arc<MutationPermit>,
        body: &mut dyn FnMut(&dyn Pointer) -> Result<()>,
    ) -> Result<()> {
        if self.wayland {
            // No plugin/inject fallback: only Sway's operation-bound focus guard
            // and the single-output virtual pointer are qualified in this slice.
            anyhow::ensure!(
                crate::wayland::sway_ipc::window_for_id(self.xid).is_some(),
                "element_route_unqualified: no qualified retained pointer adapter"
            );
            crate::wayland::with_target_foreground(self.pid, self.xid, |guard| {
                body(&NativePointer {
                    native: self,
                    focus: &|| guard.validate(),
                })
            })
        } else {
            crate::input::foreground::with_x11_foreground_permit(
                self.xid,
                crate::input::ForegroundOptions::from_settle_hint(80),
                Some(permit),
                || {
                    body(&NativePointer {
                        native: self,
                        focus: &|| {
                            anyhow::ensure!(
                                crate::x11::active_window() == Some(self.xid),
                                "exact X11 window lost focus"
                            );
                            Ok(())
                        },
                    })
                },
            )
            .map(|_| ())
        }
    }
}

struct NativePointer<'a> {
    native: &'a Native,
    focus: &'a dyn Fn() -> Result<()>,
}
impl Pointer for NativePointer<'_> {
    fn geometry(&self) -> Result<Geometry> {
        let n = self.native;
        if n.wayland {
            let w = crate::wayland::sway_ipc::window_for_id(n.xid)
                .filter(|w| w.pid == n.pid && w.visible)
                .context("exact geometry unavailable")?;
            Ok(Geometry {
                window: (w.x, w.y, w.width, w.height),
                content: (w.content_x, w.content_y),
                output: crate::wayland::sway_ipc::retained_pointer_extent()?,
            })
        } else {
            use x11rb::{
                connection::Connection, protocol::xproto::ConnectionExt,
                rust_connection::RustConnection,
            };
            let (conn, screen) = RustConnection::connect(None)?;
            let root = &conn.setup().roots[screen];
            let xid = u32::try_from(n.xid)?;
            let geometry = conn.get_geometry(xid)?.reply()?;
            anyhow::ensure!(
                geometry.root == root.root,
                "window moved to another X11 screen"
            );
            let translation = conn.translate_coordinates(xid, root.root, 0, 0)?.reply()?;
            anyhow::ensure!(
                translation.same_screen,
                "X11 coordinate conversion unavailable"
            );
            Ok(Geometry {
                window: (
                    i32::from(translation.dst_x),
                    i32::from(translation.dst_y),
                    u32::from(geometry.width),
                    u32::from(geometry.height),
                ),
                content: (0, 0),
                output: (
                    u32::from(root.width_in_pixels),
                    u32::from(root.height_in_pixels),
                ),
            })
        }
    }
    fn bounds(&self, g: Geometry) -> Result<Bounds> {
        self.native
            .target
            .pointer_bounds((g.window.0, g.window.1), g.content, self.native.wayland)
    }
    fn verify(&self) -> Result<()> {
        self.native.target.verify_live()?;
        (self.focus)()
    }
    fn click(
        &self,
        point: Point,
        geometry: Geometry,
        button: u8,
        count: usize,
        before: &mut dyn FnMut() -> Result<()>,
        ack: &mut dyn FnMut(),
    ) -> Result<()> {
        if self.native.wayland {
            crate::wayland::click_retained_point(point, geometry.output, button, count, before, ack)
        } else {
            // XTest coordinates are signed 16-bit. Never truncate/wrap them.
            i16::try_from(point.0)?;
            i16::try_from(point.1)?;
            crate::input::send_click_xtest_acknowledged(
                point.0,
                point.1,
                button,
                count,
                &[],
                before,
                ack,
            )
        }
    }
}

#[cfg(test)]
#[path = "retained_click_tests.rs"]
pub(super) mod tests;
