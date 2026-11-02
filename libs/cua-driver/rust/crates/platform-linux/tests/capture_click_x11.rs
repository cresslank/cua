// SPDX-License-Identifier: MIT

#![cfg(target_os = "linux")]

//! Native owner of the Linux capture-click adapter contract. Run only inside
//! a disposable container with Xvfb, a window manager, GTK3/PyGObject and XTEST:
//! CUA_CAPTURE_TEST_PRIVATE_X11=1 cargo test -p platform-linux \
//!   --test capture_click_x11 -- --ignored --test-threads=1
//! The guard is intentional: this test activates its window and moves a pointer.

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{ensure, Context, Result};
use cua_driver_core::authorization::PermissionMode;
use cua_driver_core::protocol::ToolResult;
use cua_driver_core::session_authorization::{
    EffectiveAuthorizationContext, SessionAuthorizationRegistry, SessionModeCeiling,
};
use cua_driver_core::tool::ToolRegistry;
use serde_json::{json, Value};
use std::sync::Arc;

const RECEIVER: &str = r#"
import gi, json, os, sys
from pathlib import Path
gi.require_version('Gtk', '3.0')
gi.require_version('GdkX11', '3.0')
from gi.repository import Gtk, GdkX11, GLib
state = Path(sys.argv[1])
count = 0
win = Gtk.Window(title='capture-click native contract')
win.set_decorated(False)
win.set_default_size(500, 300)
win.move(60, 60)
fixed = Gtk.Fixed()
button = Gtk.Button(label='CAPTURE RECEIVER')
button.set_size_request(120, 50)
fixed.put(button, 100, 70)
win.add(fixed)
other = Gtk.Window(title='capture-click sibling target')
other.set_decorated(False)
other.set_default_size(100, 100)
other.move(680, 60)
other.show_all()
def report():
    if win.get_window():
        value = dict(pid=os.getpid(), window_id=win.get_window().get_xid(),
                     count=count, scale=win.get_scale_factor(), other_window_id=other.get_window().get_xid())
        temporary = state.with_suffix('.next')
        temporary.write_text(json.dumps(value))
        temporary.replace(state)
    return True
def clicked(*unused):
    global count
    count += 1
    report()
button.connect('clicked', clicked)
win.show_all()
GLib.timeout_add(20, report)
Gtk.main()
"#;

struct Receiver(Child);
impl Drop for Receiver {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_state(path: &std::path::Path, count: u64) -> Result<Value> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(bytes) = std::fs::read(path) {
            if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
                if value["count"].as_u64() == Some(count) {
                    // Let release/prelight redraw settle before exact pixel checks.
                    std::thread::sleep(Duration::from_millis(100));
                    return Ok(value);
                }
            }
        }
        ensure!(
            Instant::now() < deadline,
            "receiver did not reach count {count}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

async fn invoke(
    registry: &ToolRegistry,
    context: &Arc<EffectiveAuthorizationContext>,
    name: &str,
    args: Value,
) -> ToolResult {
    registry
        .invoke_with_context(name, args, context.clone())
        .await
}

fn data(reply: ToolResult) -> Result<Value> {
    ensure!(reply.is_error != Some(true), "tool refused: {reply:?}");
    reply
        .structured_content
        .context("missing structured content")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicitly authorized disposable GTK/X11 desktop, never the host"]
async fn capture_click_preserves_dispatch_scope_maps_pixels_and_refuses_stale_frame() -> Result<()>
{
    ensure!(
        std::env::var("CUA_CAPTURE_TEST_PRIVATE_X11").as_deref() == Ok("1"),
        "set CUA_CAPTURE_TEST_PRIVATE_X11=1 only inside the disposable X11 container"
    );
    ensure!(
        std::env::var_os("DISPLAY").is_some() && std::env::var_os("WAYLAND_DISPLAY").is_none(),
        "private X11 display required"
    );
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("receiver.json");
    let _receiver = Receiver(
        Command::new("python3")
            .args(["-c", RECEIVER])
            .arg(&path)
            .env("GDK_BACKEND", "x11")
            .env("GDK_SCALE", "2")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?,
    );
    let receiver = wait_state(&path, 0)?;
    ensure!(
        receiver["scale"] == 2,
        "HiDPI GTK fixture did not initialize"
    );
    let pid = receiver["pid"].as_u64().unwrap();
    let xid = receiver["window_id"].as_u64().unwrap();
    let ceiling = SessionModeCeiling::for_trusted_sessions(
        [PermissionMode::Unrestricted],
        true,
        Duration::from_secs(60),
        Duration::from_secs(30),
    )
    .map_err(anyhow::Error::msg)?;
    let context = SessionAuthorizationRegistry::with_ceiling(ceiling)
        .compatibility_context(PermissionMode::Unrestricted, None)
        .map_err(anyhow::Error::msg)?;
    platform_linux::overlay::init(Default::default());
    std::thread::spawn(platform_linux::overlay::run_on_thread);
    let registry = cua_driver_core::tool::with_runtime_scope(context.runtime_scope_key(), || {
        platform_linux::tools::build_registry(false)
    });
    let definitions = registry.tools_list();
    let click = definitions["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "click")
        .unwrap();
    ensure!(click["inputSchema"]["properties"]["capture_id"]["title"] == "cua.capture-click.v1");
    let capture = |max| {
        json!({"pid":pid,"window_id":xid,"include_accessibility_tree":false,
        "include_screenshot":true,"max_image_dimension":max})
    };
    data(invoke(&registry, &context, "get_window_state", capture(0)).await)?;
    data(
        invoke(
            &registry,
            &context,
            "click",
            json!({"pid":pid,"window_id":xid,"x":300,"y":190,
        "delivery_mode":"foreground"}),
        )
        .await,
    )?;
    wait_state(&path, 1)?;
    let window = platform_linux::x11::list_windows(Some(pid as u32))
        .into_iter()
        .find(|window| window.xid == xid)
        .context("fixture window not listed")?;
    let overlay_key = "capture-contract-overlay".to_owned();
    platform_linux::overlay::send_command_for(
        overlay_key.clone(),
        cursor_overlay::OverlayCommand::SetEnabled(true),
    );
    platform_linux::overlay::send_command_for(
        overlay_key.clone(),
        cursor_overlay::OverlayCommand::PinAbove(xid),
    );
    platform_linux::overlay::send_command_for(
        overlay_key.clone(),
        cursor_overlay::track_pointer_command(f64::from(window.x + 300), f64::from(window.y + 190)),
    );
    std::thread::sleep(Duration::from_millis(100));
    ensure!(
        platform_linux::overlay::is_visible_for_session(&overlay_key),
        "overlay fixture was not visible"
    );
    let frame = data(invoke(&registry, &context, "get_window_state", capture(250)).await)?;
    ensure!(
        frame["capture_click_supported"] == true,
        "probe did not qualify window: {frame}"
    );
    ensure!(
        frame["screenshot_width"] == 250 && frame["screenshot_height"] == 150,
        "expected fourfold downsampling of scale-2 GTK window: {frame}"
    );
    let click = json!({"pid":pid,"window_id":xid,"x":80,"y":47.5,
        "capture_id":frame["capture_id"],"delivery_mode":"foreground"});
    let mut wrong_target = click.clone();
    wrong_target["window_id"] = receiver["other_window_id"].clone();
    let refused = invoke(&registry, &context, "click", wrong_target).await;
    ensure!(refused.is_error == Some(true));
    ensure!(refused.structured_content.unwrap()["code"] == "capture_target_mismatch");
    wait_state(&path, 1)?;
    // Move only the Driver-owned overlay between observation and admission.
    // That must not look like an application repaint or shift the target.
    platform_linux::overlay::send_command_for(
        overlay_key.clone(),
        cursor_overlay::track_pointer_command(f64::from(window.x + 360), f64::from(window.y + 210)),
    );
    std::thread::sleep(Duration::from_millis(100));
    data(invoke(&registry, &context, "click", click.clone()).await)?;
    wait_state(&path, 2)?;
    let replay = invoke(&registry, &context, "click", click).await;
    ensure!(replay.is_error == Some(true));
    ensure!(replay.structured_content.unwrap()["code"] == "capture_not_found");
    let frame = data(invoke(&registry, &context, "get_window_state", capture(250)).await)?;
    let moved = platform_linux::x11::set_window_frame(xid, pid as u32, 300, 250, 1000, 600)?;
    ensure!(
        moved.1,
        "private WM did not acknowledge movement: {moved:?}"
    );
    let stale = invoke(
        &registry,
        &context,
        "click",
        json!({"pid":pid,"window_id":xid,"x":80,"y":47.5,
        "capture_id":frame["capture_id"],"delivery_mode":"foreground"}),
    )
    .await;
    ensure!(stale.is_error == Some(true));
    ensure!(stale.structured_content.unwrap()["code"] == "capture_frame_mismatch");
    wait_state(&path, 2)?;
    data(invoke(&registry, &context, "get_window_state", capture(0)).await)?;
    data(
        invoke(
            &registry,
            &context,
            "click",
            json!({"pid":pid,"window_id":xid,"x":300,"y":190,
        "delivery_mode":"foreground"}),
        )
        .await,
    )?;
    wait_state(&path, 3)?;
    Ok(())
}

// This is a test-binary child, not the Driver authorization process. Keep the
// real X11 owner and advertised pid identical instead of bypassing self guards.
fn occlusion_receiver(path: &std::path::Path) -> Result<()> {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::*;
    use x11rb::wrapper::ConnectionExt as _;
    let (conn, screen_num) = x11rb::connect(None)?;
    let screen = &conn.setup().roots[screen_num];
    let pid = std::process::id();
    let wm_pid = conn.intern_atom(false, b"_NET_WM_PID")?.reply()?.atom;
    let create = |name: &str, popup: bool| -> Result<u32> {
        let xid = conn.generate_id()?;
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            xid,
            screen.root,
            80,
            80,
            400,
            300,
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            &CreateWindowAux::new()
                .background_pixel(screen.white_pixel)
                .override_redirect(u32::from(popup))
                .event_mask(EventMask::BUTTON_PRESS),
        )?
        .check()?;
        conn.change_property32(PropMode::REPLACE, xid, wm_pid, AtomEnum::CARDINAL, &[pid])?
            .check()?;
        conn.change_property8(
            PropMode::REPLACE,
            xid,
            AtomEnum::WM_NAME,
            AtomEnum::STRING,
            name.as_bytes(),
        )?
        .check()?;
        Ok(xid)
    };
    let xid = create("capture occlusion target", false)?;
    let sibling = create("capture occlusion sibling", false)?;
    let popup = create("capture occlusion popup", true)?;
    conn.map_window(xid)?.check()?;
    conn.flush()?;
    let mut count = 0;
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        while let Some(event) = conn.poll_for_event()? {
            if matches!(event, x11rb::protocol::Event::ButtonPress(_)) {
                count += 1;
            }
        }
        let temporary = path.with_extension("next");
        std::fs::write(
            &temporary,
            serde_json::to_vec(&json!({
                "pid":pid, "window_id":xid, "sibling":sibling, "popup":popup, "count":count,
            }))?,
        )?;
        std::fs::rename(temporary, path)?;
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

/// Owner of the capture-only X11 screen-recipient fence. Exercise it before
/// either MPX availability or XSendEvent fallback, so Xvfb proves the same
/// admission predicate without claiming /dev/uinput delivery qualification.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicitly authorized disposable GTK/X11 desktop, never the host"]
async fn capture_click_refuses_sibling_and_override_redirect_occlusion() -> Result<()> {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::*;
    ensure!(std::env::var("CUA_CAPTURE_TEST_PRIVATE_X11").as_deref() == Ok("1"));
    ensure!(std::env::var_os("DISPLAY").is_some() && std::env::var_os("WAYLAND_DISPLAY").is_none());
    if let Some(path) = std::env::var_os("CUA_CAPTURE_OCCLUSION_RECEIVER") {
        return occlusion_receiver(std::path::Path::new(&path));
    }
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("occlusion-receiver.json");
    let _receiver = Receiver(
        Command::new(std::env::current_exe()?)
            .args([
                "--ignored",
                "--exact",
                "capture_click_refuses_sibling_and_override_redirect_occlusion",
            ])
            .env("CUA_CAPTURE_OCCLUSION_RECEIVER", &path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?,
    );
    let receiver = wait_state(&path, 0)?;
    let pid = receiver["pid"].as_u64().unwrap() as u32;
    let xid = receiver["window_id"].as_u64().unwrap() as u32;
    let (conn, _) = x11rb::connect(None)?;
    let ceiling = SessionModeCeiling::for_trusted_sessions(
        [PermissionMode::Unrestricted],
        true,
        Duration::from_secs(60),
        Duration::from_secs(30),
    )
    .map_err(anyhow::Error::msg)?;
    let context = SessionAuthorizationRegistry::with_ceiling(ceiling)
        .compatibility_context(PermissionMode::Unrestricted, None)
        .map_err(anyhow::Error::msg)?;
    let registry = cua_driver_core::tool::with_runtime_scope(context.runtime_scope_key(), || {
        platform_linux::tools::build_registry(false)
    });
    for kind in ["sibling", "popup"] {
        // Cover the point after capture, without changing redirected pixels.
        let frame = data(
            invoke(
                &registry,
                &context,
                "get_window_state",
                json!({
                    "pid":pid, "window_id":xid, "include_accessibility_tree":false,
                    "include_screenshot":true, "max_image_dimension":0,
                }),
            )
            .await,
        )?;
        ensure!(
            frame["capture_click_supported"] == true,
            "unqualified fixture: {frame}"
        );
        let cover = receiver[kind].as_u64().unwrap() as u32;
        let target = platform_linux::x11::list_windows(Some(pid))
            .into_iter()
            .find(|w| w.xid == u64::from(xid))
            .context("target not mapped")?;
        conn.configure_window(cover, &ConfigureWindowAux::new().x(target.x).y(target.y))?
            .check()?;
        conn.map_window(cover)?.check()?;
        conn.configure_window(
            cover,
            &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
        )?
        .check()?;
        conn.flush()?;
        std::thread::sleep(Duration::from_millis(200));
        if kind == "sibling" {
            let placed = platform_linux::x11::set_window_frame(
                u64::from(cover),
                pid,
                target.x,
                target.y,
                400,
                300,
            )?;
            ensure!(placed.1, "cover move was not acknowledged: {placed:?}");
        }
        let refused = invoke(
            &registry,
            &context,
            "click",
            json!({
                "pid":pid, "window_id":xid, "x":100, "y":100,
                "capture_id":frame["capture_id"], "delivery_mode":"background",
            }),
        )
        .await;
        ensure!(
            refused.is_error == Some(true),
            "covered click accepted, {kind}: {refused:?}"
        );
        ensure!(
            refused
                .structured_content
                .as_ref()
                .and_then(|v| v["code"].as_str())
                == Some("capture_frame_mismatch"),
            "wrong refusal, {kind}: {refused:?}"
        );
        std::thread::sleep(Duration::from_millis(100));
        let observed: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
        ensure!(
            observed["count"] == 0,
            "input reached a real window despite refusal: {observed}"
        );
        conn.unmap_window(cover)?.check()?;
        conn.flush()?;
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}
