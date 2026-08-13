//! Background mouse injection via PostMessage.
//!
//! All clicks target the **deepest child** HWND at the click point
//! (via ChildWindowFromPointEx), so the message never reaches the top-level
//! chrome that would call SetForegroundWindow in response to WM_LBUTTONDOWN.

use anyhow::{bail, Result};
use std::thread::sleep;
use std::time::Duration;
use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK,
    MOUSEEVENTF_WHEEL, MOUSEINPUT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    ChildWindowFromPointEx, GetAncestor, GetClassLongPtrW, GetCursorPos, GetForegroundWindow,
    GetSystemMetrics, PostMessageW, SetCursorPos, WindowFromPoint, CS_DBLCLKS, CWP_SKIPDISABLED,
    CWP_SKIPINVISIBLE, CWP_SKIPTRANSPARENT, GA_ROOT, GCL_STYLE, SM_CXVIRTUALSCREEN,
    SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MBUTTONDBLCLK, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEMOVE, WM_RBUTTONDBLCLK,
    WM_RBUTTONDOWN, WM_RBUTTONUP,
};

const MK_LBUTTON: u32 = 0x0001;
const MK_MBUTTON: u32 = 0x0010;
const MK_RBUTTON: u32 = 0x0002;

const CLICK_DELAY_MS: u64 = 35;

fn exact_target_receives_point(target: u64, expected_pid: u32, sx: i32, sy: i32) -> bool {
    if crate::win32::capture_foreground_target(target, Some(expected_pid)).is_none() {
        return false;
    }
    let target = HWND(target as *mut _);
    let actual = unsafe { WindowFromPoint(POINT { x: sx, y: sy }) };
    if actual.0.is_null() {
        return false;
    }
    let target_root = unsafe { GetAncestor(target, GA_ROOT) };
    let actual_root = unsafe { GetAncestor(actual, GA_ROOT) };
    !target_root.0.is_null()
        && target_root == actual_root
        && crate::win32::window_owner_pid(actual_root.0 as usize as u64) == Some(expected_pid)
}

fn posted_press_message(down: u32, double: u32, click_index: usize, wants_double: bool) -> u32 {
    if wants_double && click_index % 2 == 1 {
        double
    } else {
        down
    }
}

/// Walk from `root` down to the deepest visible child that contains
/// `screen_pt`, mirroring trope-cua's DeepestChildFromScreenPoint.
///
/// Posting to the deepest child avoids the top-level window responding to
/// WM_LBUTTONDOWN by activating itself (focus-steal).
fn deepest_child(root: HWND, screen_pt: POINT) -> (HWND, POINT) {
    let mut current = root;
    for _ in 0..16 {
        let mut client = screen_pt;
        unsafe {
            let _ = ScreenToClient(current, &mut client);
        }
        let child = unsafe {
            ChildWindowFromPointEx(
                current,
                client,
                CWP_SKIPINVISIBLE | CWP_SKIPDISABLED | CWP_SKIPTRANSPARENT,
            )
        };
        // No deeper child, or same window, or outside root's subtree.
        if child.is_invalid() || child == current {
            break;
        }
        // Verify the child is actually a descendant of root.
        let is_child = unsafe { windows::Win32::UI::WindowsAndMessaging::IsChild(root, child) };
        if !is_child.as_bool() && child != root {
            break;
        }
        current = child;
    }
    // Return child-local client coordinates for `current`.
    let mut client = screen_pt;
    unsafe {
        let _ = ScreenToClient(current, &mut client);
    }
    (current, client)
}

fn exact_owned_root(root: u64, expected_pid: u32, operation: &str) -> Result<HWND> {
    let hwnd = HWND(root as *mut _);
    if hwnd.0.is_null() {
        bail!("{operation}: target HWND is null");
    }
    let actual_root = unsafe { GetAncestor(hwnd, GA_ROOT) };
    if actual_root.0.is_null() || actual_root != hwnd {
        bail!("{operation}: approved HWND is no longer the exact top-level root");
    }
    if crate::win32::window_owner_pid(root) != Some(expected_pid) {
        bail!("{operation}: approved HWND changed ownership");
    }
    Ok(hwnd)
}

fn prove_post_recipient(
    root: HWND,
    target: HWND,
    expected_pid: u32,
    operation: &str,
) -> Result<()> {
    if crate::win32::window_owner_pid(root.0 as usize as u64) != Some(expected_pid) {
        bail!("{operation}: approved root HWND changed ownership");
    }
    if crate::win32::window_owner_pid(target.0 as usize as u64) != Some(expected_pid) {
        bail!("{operation}: resolved child HWND changed ownership");
    }
    let actual_root = unsafe { GetAncestor(target, GA_ROOT) };
    if actual_root.0.is_null() || actual_root != root {
        bail!("{operation}: resolved child no longer belongs to the approved root");
    }
    Ok(())
}

/// Post a click at **screen** coordinates, resolving the deepest child of
/// `root_hwnd` at that point.  Call this when you already have screen coords.
pub fn post_click_screen(
    root: u64,
    expected_pid: u32,
    sx: i32,
    sy: i32,
    count: usize,
    button: &str,
) -> Result<()> {
    let root_hwnd = exact_owned_root(root, expected_pid, "posted click resolution")?;
    let screen_pt = POINT { x: sx, y: sy };
    let (target, client) = deepest_child(root_hwnd, screen_pt);
    prove_post_recipient(root_hwnd, target, expected_pid, "posted click resolution")?;
    post_click_on(
        root_hwnd,
        target,
        expected_pid,
        client.x,
        client.y,
        count,
        button,
    )
}

/// Internal: post click messages to `hwnd` using its own client coordinates.
fn post_click_on(
    root: HWND,
    hwnd: HWND,
    expected_pid: u32,
    x: i32,
    y: i32,
    count: usize,
    button: &str,
) -> Result<()> {
    prove_post_recipient(root, hwnd, expected_pid, "posted click preflight")?;
    // UIPI check — Medium-IL daemon → High-IL target silently drops mouse
    // messages just like keyboard ones. Surface an actionable error before
    // PostMessage returns its misleading TRUE. See post_message_blocked_by_uipi.
    if let Some(msg) = crate::input::post_message_blocked_by_uipi(hwnd.0 as u64) {
        anyhow::bail!(msg);
    }

    let (down_msg, double_msg, up_msg, mk_flag) = match button {
        "right" => (WM_RBUTTONDOWN, WM_RBUTTONDBLCLK, WM_RBUTTONUP, MK_RBUTTON),
        "middle" => (WM_MBUTTONDOWN, WM_MBUTTONDBLCLK, WM_MBUTTONUP, MK_MBUTTON),
        _ => (WM_LBUTTONDOWN, WM_LBUTTONDBLCLK, WM_LBUTTONUP, MK_LBUTTON),
    };
    let lparam = make_lparam(x, y);
    let wdown = WPARAM(mk_flag as usize);
    let wup = WPARAM(0);
    let wants_double = unsafe { (GetClassLongPtrW(hwnd, GCL_STYLE) as u32 & CS_DBLCLKS.0) != 0 };
    let displaced_target =
        crate::win32::capture_foreground_target(root.0 as usize as u64, Some(expected_pid))
            .ok_or_else(|| anyhow::anyhow!("posted click target identity is stale"))?;
    let prev_fg_target = crate::win32::capture_current_foreground_target()
        .filter(|previous| previous.hwnd() != root.0 as usize as u64);

    // Posted pointer messages are normally non-activating, but WebView hosts can
    // call SetForegroundWindow from their event handlers. Keep the top-level
    // categorically non-activatable until the complete burst has settled.
    prove_post_recipient(root, hwnd, expected_pid, "posted click guard")?;
    let mut noact = crate::input::NoActivateGuard::arm_for_pid(root, expected_pid)?;
    let mut button_down_posted = false;
    let mut result = (|| -> Result<()> {
        for i in 0..count {
            let press_msg = posted_press_message(down_msg, double_msg, i, wants_double);
            unsafe {
                // WM_MOUSEMOVE first so hover state is correct before the click.
                prove_post_recipient(root, hwnd, expected_pid, "posted click mouse-move")?;
                PostMessageW(hwnd, WM_MOUSEMOVE, WPARAM(0), lparam)?;
                // Win32 controls do not infer a double-click from two posted DOWN
                // messages. The second press must use WM_*BUTTONDBLCLK.
                prove_post_recipient(root, hwnd, expected_pid, "posted click button-down")?;
                PostMessageW(hwnd, press_msg, wdown, lparam)?;
                button_down_posted = true;
                sleep(Duration::from_millis(CLICK_DELAY_MS));
                prove_post_recipient(root, hwnd, expected_pid, "posted click button-up")?;
                PostMessageW(hwnd, up_msg, wup, lparam)?;
                button_down_posted = false;
            }
            if i + 1 < count {
                sleep(Duration::from_millis(80));
            }
        }
        Ok(())
    })();

    if let Err(primary) = result {
        result = if button_down_posted {
            let cleanup =
                prove_post_recipient(root, hwnd, expected_pid, "posted click release cleanup")
                    .and_then(|()| unsafe {
                        PostMessageW(hwnd, up_msg, wup, lparam).map_err(Into::into)
                    });
            match cleanup {
                Ok(()) => Err(primary),
                Err(cleanup_error) => Err(anyhow::anyhow!(
                    "{primary}; cleanup failure: release-only posted button-up failed: {cleanup_error}"
                )),
            }
        } else {
            Err(primary)
        };
    }
    if let Err(restoration) = noact.finish() {
        result = Err(match result {
            Ok(()) => restoration,
            Err(primary) => anyhow::anyhow!("{primary}; cleanup failure: {restoration}"),
        });
    }
    sleep(Duration::from_millis(50));
    if let Some(previous) = prev_fg_target {
        let restored = crate::win32::restore_foreground_target_if_still_displaced(
            previous,
            displaced_target,
            Duration::from_millis(500),
        );
        if restored == crate::win32::ForegroundRestoreOutcome::Failed {
            let restoration = anyhow::anyhow!("foreground_restore_failed after posted click");
            result = Err(match result {
                Ok(()) => restoration,
                Err(primary) => anyhow::anyhow!("{primary}; cleanup failure: {restoration}"),
            });
        }
    }
    result
}

/// Press-drag-release via PostMessage, resolving the **deepest child** at the
/// drag-start screen point and posting in that child's client coordinates.
///
/// `post_drag` (above) posts to the top-level frame, so a child-windowed
/// control (a WinForms `Panel`, a Win32 child canvas, …) never sees the drag —
/// the frame gets messages over a region it doesn't own and ignores them. This
/// variant mirrors `post_click`: it hit-tests down to the deepest descendant
/// under the start point and targets that HWND for the whole gesture (a drag
/// stays within one control), with each point converted to the child's own
/// client space. Endpoints are given in **screen** coordinates.
pub fn post_drag_screen(
    root: u64,
    expected_pid: u32,
    sx_from: i32,
    sy_from: i32,
    sx_to: i32,
    sy_to: i32,
    duration_ms: u64,
    steps: usize,
    button: &str,
) -> Result<()> {
    let root_hwnd = exact_owned_root(root, expected_pid, "posted drag resolution")?;
    let (target, c_from) = deepest_child(
        root_hwnd,
        POINT {
            x: sx_from,
            y: sy_from,
        },
    );
    let mut c_to = POINT { x: sx_to, y: sy_to };
    unsafe {
        let _ = ScreenToClient(target, &mut c_to);
    }
    prove_post_recipient(root_hwnd, target, expected_pid, "posted drag resolution")?;
    if let Some(msg) = crate::input::post_message_blocked_by_uipi(target.0 as u64) {
        anyhow::bail!(msg);
    }
    let (down_msg, up_msg, mk_flag) = match button {
        "right" => (WM_RBUTTONDOWN, WM_RBUTTONUP, MK_RBUTTON),
        "middle" => (WM_MBUTTONDOWN, WM_MBUTTONUP, MK_MBUTTON),
        _ => (WM_LBUTTONDOWN, WM_LBUTTONUP, MK_LBUTTON),
    };
    let wparam = WPARAM(mk_flag as usize);
    let steps = steps.max(1);
    let step_delay_ms = if steps > 1 {
        duration_ms / steps as u64
    } else {
        duration_ms
    };
    prove_post_recipient(root_hwnd, target, expected_pid, "posted drag guard")?;
    let mut noact = crate::input::NoActivateGuard::arm_for_pid(root_hwnd, expected_pid)?;
    let mut button_down_posted = false;
    let mut last_point = c_from;
    let gesture = (|| -> Result<()> {
        unsafe {
            // Pre-drag MOUSEMOVE (wParam=0, no buttons down yet) then DOWN at from.
            prove_post_recipient(root_hwnd, target, expected_pid, "posted drag pre-move")?;
            PostMessageW(
                target,
                WM_MOUSEMOVE,
                WPARAM(0),
                make_lparam(c_from.x, c_from.y),
            )?;
            prove_post_recipient(root_hwnd, target, expected_pid, "posted drag button-down")?;
            PostMessageW(target, down_msg, wparam, make_lparam(c_from.x, c_from.y))?;
            button_down_posted = true;
        }
        sleep(Duration::from_millis(CLICK_DELAY_MS));
        for i in 1..=steps {
            let t = i as f64 / steps as f64;
            let ix = c_from.x + ((c_to.x - c_from.x) as f64 * t).round() as i32;
            let iy = c_from.y + ((c_to.y - c_from.y) as f64 * t).round() as i32;
            prove_post_recipient(root_hwnd, target, expected_pid, "posted drag move")?;
            unsafe {
                PostMessageW(target, WM_MOUSEMOVE, wparam, make_lparam(ix, iy))?;
            }
            last_point = POINT { x: ix, y: iy };
            if step_delay_ms > 0 {
                sleep(Duration::from_millis(step_delay_ms));
            }
        }
        prove_post_recipient(root_hwnd, target, expected_pid, "posted drag button-up")?;
        unsafe {
            PostMessageW(target, up_msg, WPARAM(0), make_lparam(c_to.x, c_to.y))?;
        }
        button_down_posted = false;
        Ok(())
    })();

    let mut result = if let Err(primary) = gesture {
        if button_down_posted {
            let cleanup = prove_post_recipient(
                root_hwnd,
                target,
                expected_pid,
                "posted drag release cleanup",
            )
            .and_then(|()| unsafe {
                PostMessageW(
                    target,
                    up_msg,
                    WPARAM(0),
                    make_lparam(last_point.x, last_point.y),
                )
                .map_err(Into::into)
            });
            match cleanup {
                Ok(()) => Err(primary),
                Err(cleanup_error) => Err(anyhow::anyhow!(
                    "{primary}; cleanup failure: release-only posted button-up failed: {cleanup_error}"
                )),
            }
        } else {
            Err(primary)
        }
    } else {
        Ok(())
    };
    if let Err(restoration) = noact.finish() {
        result = Err(match result {
            Ok(()) => restoration,
            Err(primary) => anyhow::anyhow!("{primary}; cleanup failure: {restoration}"),
        });
    }
    result
}

/// Pack two 16-bit integers into a LPARAM (low word = x, high word = y).
///
/// Delegates the bit-math to [`crate::lparam::pack_xy`] so the
/// receiver-side `GET_X_LPARAM` / `GET_Y_LPARAM` sign-extension contract is
/// covered by cross-platform unit tests (see #1979's audit: the PostMessage
/// path packing was a suspect for the multi-monitor wrong-screen symptom,
/// turned out to be correct, and now has regression coverage).
///
/// On the (currently unreachable) out-of-range path we log + clamp rather
/// than panic — every existing call site passes post-`ScreenToClient`
/// window-local coords that fit in `i16` by construction, but if a future
/// caller passes a raw screen coord on a >32k-px virtual desktop, clamping
/// is at least visible in the log instead of silently wrapping.
fn make_lparam(x: i32, y: i32) -> LPARAM {
    match crate::lparam::pack_xy(x, y) {
        Ok(packed) => LPARAM(packed as isize),
        Err(err) => {
            tracing::warn!(
                target: "click",
                "make_lparam: {err}; clamping to i16 range. \
                 If you see this, the caller is passing non-window-local coords."
            );
            let clamp = |v: i32| v.clamp(i16::MIN as i32, i16::MAX as i32);
            let cx = clamp(x);
            let cy = clamp(y);
            let packed =
                crate::lparam::pack_xy(cx, cy).expect("clamped values always fit in i16 range");
            LPARAM(packed as isize)
        }
    }
}

/// Returns `true` when `hwnd` is a top-level frame of a Chromium-based browser
/// — Edge, Chrome, Brave, Vivaldi, Opera, Chromium, Arc, Thorium, Iridium,
/// Yandex, or any other Chromium-derivative. Matches by window class name,
/// which is stable across versions and consistent across Chromium forks.
///
/// Chromium uses the window class `Chrome_WidgetWin_1` (or `Chrome_WidgetWin_0`
/// for in-process child frames; both should be treated the same way). Electron
/// apps that embed Chromium also use this class, so Electron app coord clicks
/// will route through the SendInput path too — that's intentional, same root
/// cause (#1623).
///
/// Cheap call: one `GetClassNameW` to a 32-char buffer + a `matches!` against
/// the known prefixes. Suitable to call inline in the click dispatch hot path.
pub fn is_chromium_target_window(hwnd: u64) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::GetClassNameW;
    if hwnd == 0 {
        tracing::debug!(target: "click", "is_chromium_target_window: hwnd=0 short-circuit");
        return false;
    }
    let mut buf = [0u16; 64];
    let n = unsafe { GetClassNameW(HWND(hwnd as *mut _), &mut buf) };
    if n <= 0 {
        tracing::debug!(
            target: "click",
            "is_chromium_target_window: GetClassNameW returned {n} for hwnd=0x{hwnd:x}"
        );
        return false;
    }
    let class_name = String::from_utf16_lossy(&buf[..n as usize]);
    // Chromium-family classes. Match by prefix because Chromium suffixes a
    // 0/1 digit; future Chromium forks may use other suffixes.
    let is_chromium = class_name.starts_with("Chrome_WidgetWin_")
        // Electron sometimes uses CefBrowserWindow or similar — be permissive.
        || class_name.starts_with("CefBrowser");
    tracing::debug!(
        target: "click",
        "is_chromium_target_window: hwnd=0x{hwnd:x} class={class_name:?} → {is_chromium}"
    );
    is_chromium
}

/// Return true when `hwnd` hosts a Chromium/WebView2 renderer child even if
/// its own top-level class is framework-specific (for example a Tauri host).
/// Keep this separate from [`is_chromium_target_window`]: embedded WebView2
/// surfaces support some UIA/top-level background routes that direct Chromium
/// frames do not, so delivery policy needs to distinguish the two shapes.
pub fn has_chromium_descendant(hwnd: u64) -> bool {
    use windows::Win32::Foundation::{BOOL, FALSE, LPARAM, TRUE};
    use windows::Win32::UI::WindowsAndMessaging::{EnumChildWindows, GetClassNameW};

    if hwnd == 0 {
        return false;
    }
    struct Scan {
        found: bool,
    }
    unsafe extern "system" fn child_cb(child: HWND, lparam: LPARAM) -> BOOL {
        let scan = &mut *(lparam.0 as *mut Scan);
        let mut buf = [0u16; 64];
        let n = GetClassNameW(child, &mut buf);
        if n > 0 {
            let class = String::from_utf16_lossy(&buf[..n as usize]);
            if class.starts_with("Chrome_WidgetWin_") || class.starts_with("CefBrowser") {
                scan.found = true;
                return FALSE;
            }
        }
        TRUE
    }

    let mut scan = Scan { found: false };
    unsafe {
        let _ = EnumChildWindows(
            HWND(hwnd as *mut _),
            Some(child_cb),
            LPARAM(&mut scan as *mut Scan as isize),
        );
    }
    scan.found
}

/// Click at **screen** coordinates `(sx, sy)` via `SendInput` against the
/// system input queue, briefly focusing `target` so the click lands there.
///
/// Why this exists alongside `post_click_screen`: PostMessage(WM_LBUTTONDOWN)
/// to Chromium-based browsers' top-level frame HWND (or Chrome_RenderWidgetHostHWND
/// descendant) doesn't fire DOM `onclick` / `mousedown` handlers. Chromium's
/// input thread architecture requires events with `SendInput`-queue origin —
/// the same constraint that broke modifier-state hotkey delivery (#1614/#1618)
/// applies to coord clicks on Chromium content (#1623).
///
/// `SendInput` puts the synthetic mouse events on the **system input queue**,
/// where Chromium's input filter accepts them. The trade-off is a brief
/// foreground swap + visible cursor jump (mitigated by saving/restoring the
/// previous foreground HWND and previous cursor position after the click).
///
/// UIAccess constraint: `SetForegroundWindow` is restricted from non-UIAccess
/// processes when not driven by user input. The `cua-driver-uia` worker runs
/// at UIAccess integrity precisely so this restriction is lifted; outside the
/// worker, the foreground swap may silently fail and SendInput land on the
/// wrong window. Callers should funnel Chromium coord clicks through the
/// uia worker (the MCP proxy already prefers the uia pipe over the regular
/// pipe when both are running).
pub fn send_click_synthesized(
    target: u64,
    sx: i32,
    sy: i32,
    count: usize,
    button: &str,
) -> Result<()> {
    send_click_synthesized_mods(target, sx, sy, count, button, &[])
}

/// Like [`send_click_synthesized`] but HOLDS the named modifier keys
/// (cmd/shift/option/ctrl) for the duration of the click: modifier-down via
/// SendInput before the click sequence, modifier-up after. Mirrors the macOS
/// click `modifier` surface. Only this SendInput (foreground / desktop-scope)
/// path can carry modifiers — the background UIA-Invoke and PostMessage paths
/// have no keyboard state to hold them, so a `modifier` passed to a background
/// pixel/element click is necessarily ignored on those rungs.
pub fn send_click_synthesized_mods(
    target: u64,
    sx: i32,
    sy: i32,
    count: usize,
    button: &str,
    modifiers: &[&str],
) -> Result<()> {
    send_click_synthesized_mods_impl(target, None, sx, sy, count, button, modifiers, false)
}

pub fn send_click_synthesized_mods_for_pid(
    target: u64,
    expected_pid: u32,
    sx: i32,
    sy: i32,
    count: usize,
    button: &str,
    modifiers: &[&str],
) -> Result<()> {
    send_click_synthesized_mods_impl(
        target,
        Some(expected_pid),
        sx,
        sy,
        count,
        button,
        modifiers,
        false,
    )
}

/// SendInput click for an explicit foreground request. Unlike the historical
/// z-order-assisted path, this activates the target and does not add
/// `WS_EX_NOACTIVATE`, so retained-mode frameworks such as WPF process the
/// system-queue pointer event. The prior foreground is restored and confirmed
/// before the call returns.
pub fn send_click_synthesized_active_mods(
    target: u64,
    sx: i32,
    sy: i32,
    count: usize,
    button: &str,
    modifiers: &[&str],
) -> Result<()> {
    send_click_synthesized_active_mods_for_pid(target, None, sx, sy, count, button, modifiers)
}

pub fn send_click_synthesized_active_mods_for_pid(
    target: u64,
    expected_pid: Option<u32>,
    sx: i32,
    sy: i32,
    count: usize,
    button: &str,
    modifiers: &[&str],
) -> Result<()> {
    send_click_synthesized_mods_impl(target, expected_pid, sx, sy, count, button, modifiers, true)
}

fn send_click_synthesized_mods_impl(
    target: u64,
    expected_pid: Option<u32>,
    sx: i32,
    sy: i32,
    count: usize,
    button: &str,
    modifiers: &[&str],
    activate: bool,
) -> Result<()> {
    let target = HWND(target as *mut _);
    if target.0.is_null() {
        bail!("invalid target hwnd");
    }
    if let Some(msg) = crate::input::post_message_blocked_by_uipi(target.0 as u64) {
        // Same UIPI defense as PostMessage path — SendInput from non-UIAccess
        // would fail just as silently as PostMessage when target is at higher
        // integrity. Surface the diagnostic early.
        bail!(msg);
    }

    let (down_flag, up_flag) = match button {
        "right" => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
        "middle" => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
        _ => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
    };

    // Convert screen pixel coords to normalized absolute coords spanning the
    // virtual desktop (0..65535 across the union of all monitors). This is
    // what `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK` expects.
    //
    // Without VIRTUALDESK the coords are relative to the primary monitor only;
    // multi-monitor setups would misroute. Better to always use VIRTUALDESK.
    //
    // Math lives in `crate::virtualdesk` so it can be unit-tested cross-platform
    // (no Win32 runtime required) — see issue #1979 for the negative-offset
    // multi-monitor case the tests there pin down.
    let (vd_x, vd_y) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
        )
    };
    let (vd_w, vd_h) = unsafe {
        (
            GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1),
            GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1),
        )
    };
    let (norm_x, norm_y) =
        crate::virtualdesk::to_virtualdesk_absolute(sx, sy, vd_x, vd_y, vd_w, vd_h);

    let move_input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: norm_x,
                dy: norm_y,
                mouseData: 0,
                dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let down_input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: 0,
                dwFlags: down_flag,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let up_input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: 0,
                dwFlags: up_flag,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };

    unsafe {
        // Save previous foreground + cursor position so we can restore.
        let prev_fg_target = match crate::win32::capture_current_foreground_target() {
            Some(previous) if previous.hwnd() != target.0 as usize as u64 => Some(previous),
            Some(_) => None,
            None => {
                bail!(
                    "foreground_restore_unavailable: a stable prior foreground identity could not be captured; no mouse input was sent"
                )
            }
        };
        let target_before = if activate {
            Some(
                crate::win32::capture_foreground_target(target.0 as usize as u64, expected_pid)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "foreground_unavailable: exact target HWND {:?} changed ownership before mouse targeting; no window or input mutation was attempted",
                            target.0
                        )
                    })?,
            )
        } else {
            None
        };
        let cursor_target = if expected_pid.is_some() {
            Some(crate::win32::capture_current_cursor_target().ok_or_else(|| {
                anyhow::anyhow!(
                    "cursor_restore_unavailable: stable prior cursor recipient could not be captured; no mouse input was sent"
                )
            })?)
        } else {
            None
        };
        let mut prev_cursor = POINT::default();
        if cursor_target.is_none() {
            let _ = GetCursorPos(&mut prev_cursor);
        }

        if activate
            && !match expected_pid {
                Some(pid) => crate::input::force_foreground_assisted_for_pid(target, pid),
                None => crate::input::force_foreground_assisted(target),
            }
            .0
        {
            let actual = GetForegroundWindow();
            let restore_failed = match (prev_fg_target, target_before) {
                (Some(previous), Some(displaced)) => {
                    crate::win32::restore_foreground_target_if_still_displaced(
                        previous,
                        displaced,
                        Duration::from_millis(500),
                    ) == crate::win32::ForegroundRestoreOutcome::Failed
                }
                _ => false,
            };
            bail!(
                "foreground_unavailable: Windows did not activate exact target HWND {:?} \
                 (actual foreground HWND {:?}); no mouse input was sent{}",
                target.0,
                actual.0,
                if restore_failed {
                    "; cleanup failure: foreground restoration was not confirmed"
                } else {
                    ""
                }
            );
        }
        let foreground_target = if activate {
            match crate::win32::capture_foreground_target(target.0 as usize as u64, expected_pid) {
                Some(target) => Some(target),
                None => {
                    let restore_failed = match (prev_fg_target, target_before) {
                        (Some(previous), Some(displaced)) => {
                            crate::win32::restore_foreground_target_if_still_displaced(
                                previous,
                                displaced,
                                Duration::from_millis(500),
                            ) == crate::win32::ForegroundRestoreOutcome::Failed
                        }
                        _ => false,
                    };
                    bail!(
                        "foreground_unavailable: exact target HWND {:?} disappeared or changed ownership before mouse input could be sent{}",
                        target.0,
                        if restore_failed {
                            "; cleanup failure: foreground restoration was not confirmed"
                        } else {
                            ""
                        }
                    )
                }
            }
        } else {
            None
        };
        let mut noactivate = if !activate {
            match expected_pid {
                Some(pid) => Some(crate::input::NoActivateGuard::arm_for_pid(target, pid)?),
                None => Some(crate::input::NoActivateGuard::arm(target)),
            }
        } else {
            None
        };
        // Move the cursor so the OS hover state matches before the click; the
        // MOUSEEVENTF_MOVE input ensures Chromium's input filter sees a
        // coordinated move event.
        let _ = SetCursorPos(sx, sy);

        // Hold modifier keys (ctrl/shift/alt/win) across the click — the macOS
        // `modifier` surface. Pressed via the system input queue so apps that
        // poll GetKeyState (WPF, Chromium) observe the held state, then released
        // after the click loop below.
        let (mod_downs, mod_ups) = crate::input::keyboard::modifier_hold_inputs(modifiers);
        if expected_pid.is_some()
            && crate::win32::capture_foreground_target(target.0 as usize as u64, expected_pid)
                .is_none()
        {
            let restore_failed = match (prev_fg_target, foreground_target) {
                (Some(previous), Some(displaced)) => {
                    crate::win32::restore_foreground_target_if_still_displaced(
                        previous,
                        displaced,
                        Duration::from_millis(500),
                    ) == crate::win32::ForegroundRestoreOutcome::Failed
                }
                _ => false,
            };
            bail!(
                "foreground_unavailable: exact target HWND {:?} changed ownership at the mouse-input boundary; no input was sent{}",
                target.0,
                if restore_failed {
                    "; cleanup failure: foreground restoration was not confirmed"
                } else {
                    ""
                }
            );
        }
        let mut sent_ok = true;
        let mut cleanup_failures = Vec::new();
        let mut inserted_modifier_downs = 0usize;
        let point_authorized = expected_pid
            .is_none_or(|pid| exact_target_receives_point(target.0 as usize as u64, pid, sx, sy));
        let foreground_authorized = !activate
            || foreground_target.is_some_and(|approved| {
                crate::win32::foreground_matches_target_or_owned_window(
                    approved,
                    GetForegroundWindow().0 as usize as u64,
                )
            });
        if !point_authorized || !foreground_authorized {
            let restore_failed = match (prev_fg_target, foreground_target) {
                (Some(previous), Some(displaced)) => {
                    crate::win32::restore_foreground_target_if_still_displaced(
                        previous,
                        displaced,
                        Duration::from_millis(500),
                    ) == crate::win32::ForegroundRestoreOutcome::Failed
                }
                _ => false,
            };
            bail!(
                "foreground_unavailable: exact target no longer owns the click point or foreground at the SendInput boundary; no input was sent{}",
                if restore_failed {
                    "; cleanup failure: foreground restoration was not confirmed"
                } else {
                    ""
                }
            );
        }
        if !mod_downs.is_empty() {
            let sent = SendInput(&mod_downs, std::mem::size_of::<INPUT>() as i32);
            inserted_modifier_downs = (sent as usize).min(mod_downs.len());
            sent_ok = sent as usize == mod_downs.len();
            sleep(Duration::from_millis(5));
        }

        let count = count.max(1);
        for i in 0..count {
            if !sent_ok {
                break;
            }
            if expected_pid.is_some()
                && crate::win32::capture_foreground_target(target.0 as usize as u64, expected_pid)
                    .is_none()
            {
                sent_ok = false;
                break;
            }
            if expected_pid.is_some_and(|pid| {
                !exact_target_receives_point(target.0 as usize as u64, pid, sx, sy)
            }) {
                sent_ok = false;
                break;
            }
            if activate
                && !foreground_target.is_some_and(|approved| {
                    crate::win32::foreground_matches_target_or_owned_window(
                        approved,
                        GetForegroundWindow().0 as usize as u64,
                    )
                })
            {
                sent_ok = false;
                break;
            }
            // Only the move record carries absolute coordinates. Button-only
            // records act at the current pointer position; adding ABSOLUTE to
            // them can prevent retained-mode controls from seeing the press.
            let events = [move_input, down_input, up_input];
            let sent = SendInput(&events, std::mem::size_of::<INPUT>() as i32);
            if sent as usize != events.len() {
                // A partial batch may have inserted button-down without
                // button-up. Release only; do not encode another movement.
                let cleanup = [up_input];
                let cleanup_authorized = sent as usize >= 2
                    && expected_pid.is_none_or(|pid| {
                        exact_target_receives_point(target.0 as usize as u64, pid, sx, sy)
                            && (!activate
                                || foreground_target.is_some_and(|approved| {
                                    crate::win32::foreground_matches_target_or_owned_window(
                                        approved,
                                        GetForegroundWindow().0 as usize as u64,
                                    )
                                }))
                    });
                let cleanup_sent = if cleanup_authorized {
                    SendInput(&cleanup, std::mem::size_of::<INPUT>() as i32)
                } else {
                    0
                };
                cleanup_failures.push(format!(
                    "click insertion {sent}/{}; release-only cleanup {} and inserted {cleanup_sent}/{}",
                    events.len(),
                    if cleanup_authorized { "authorized" } else { "withheld because recipient authority was lost" },
                    if cleanup_authorized { cleanup.len() } else { 0 }
                ));
                sent_ok = false;
                break;
            }
            if i + 1 < count {
                sleep(Duration::from_millis(80));
            }
        }

        // Release any held modifiers (reverse order) before restoring z-order.
        let held_modifier_ups = &mod_ups[mod_ups.len() - inserted_modifier_downs..];
        if !held_modifier_ups.is_empty() {
            let release_authorized = expected_pid.is_none_or(|pid| {
                exact_target_receives_point(target.0 as usize as u64, pid, sx, sy)
                    && (!activate
                        || foreground_target.is_some_and(|approved| {
                            crate::win32::foreground_matches_target_or_owned_window(
                                approved,
                                GetForegroundWindow().0 as usize as u64,
                            )
                        }))
            });
            let released = if release_authorized {
                SendInput(held_modifier_ups, std::mem::size_of::<INPUT>() as i32)
            } else {
                0
            };
            if released as usize != held_modifier_ups.len() {
                let retry_authorized = release_authorized
                    && expected_pid.is_none_or(|pid| {
                        exact_target_receives_point(target.0 as usize as u64, pid, sx, sy)
                    });
                let retry = if retry_authorized {
                    SendInput(held_modifier_ups, std::mem::size_of::<INPUT>() as i32)
                } else {
                    0
                };
                cleanup_failures.push(format!(
                    "modifier release {released}/{expected}; retry {} and inserted {retry}/{expected}",
                    if retry_authorized { "authorized" } else { "withheld because recipient authority was lost" },
                    expected = held_modifier_ups.len()
                ));
                sent_ok = false;
            }
        }

        // Let the target process mouse-up before any background-route restore.
        // Retained-mode frameworks establish capture/focus on mouse-down and can
        // lose the click if the real cursor is warped away while those queued
        // messages are still being dispatched.
        sleep(Duration::from_millis(if activate { 120 } else { 40 }));
        let cursor_restored = if !activate {
            if let Some(cursor_target) = cursor_target {
                crate::win32::restore_cursor_target(cursor_target)
            } else {
                SetCursorPos(prev_cursor.x, prev_cursor.y).is_ok()
            }
        } else {
            true
        };
        let mut action_result = if !sent_ok {
            Err(anyhow::anyhow!(
                "SendInput inserted fewer modifier or mouse events than expected for the foreground click{}",
                if cleanup_failures.is_empty() {
                    String::new()
                } else {
                    format!("; cleanup evidence: {}", cleanup_failures.join("; "))
                }
            ))
        } else if activate {
            let actual = GetForegroundWindow();
            if !crate::win32::foreground_matches_target_or_owned_window(
                foreground_target.expect("foreground target captured before input"),
                actual.0 as usize as u64,
            ) {
                Err(anyhow::anyhow!(
                    "foreground_unavailable: exact target HWND {:?} or a verified same-process \
                     post-action window was not foreground after the click \
                     (actual foreground HWND {:?})",
                    target.0,
                    actual.0
                ))
            } else {
                Ok(())
            }
        } else {
            Ok(())
        };
        if expected_pid.is_some() {
            if let Some(noactivate) = noactivate.as_mut() {
                if let Err(restoration) = noactivate.finish() {
                    action_result = Err(match action_result {
                        Ok(()) => restoration,
                        Err(primary) => {
                            anyhow::anyhow!("{primary}; cleanup failure: {restoration}")
                        }
                    });
                }
            }
        }
        drop(noactivate);
        if !cursor_restored {
            let restore_error = anyhow::anyhow!(
                "cursor_restore_failed: stable prior cursor destination was not confirmed after mouse input"
            );
            action_result = Err(match action_result {
                Ok(_) => restore_error,
                Err(error) => anyhow::anyhow!("{error}; cleanup failure: {restore_error}"),
            });
        }
        if let Some(previous) = prev_fg_target {
            let restored = crate::win32::restore_foreground_target_if_still_displaced(
                previous,
                foreground_target.expect("foreground target captured before restoration"),
                Duration::from_millis(500),
            );
            if restored == crate::win32::ForegroundRestoreOutcome::Failed {
                let restore_error = anyhow::anyhow!(
                    "foreground_restore_failed: Windows did not confirm restoration of the prior foreground window after mouse input"
                );
                action_result = Err(match action_result {
                    Ok(_) => restore_error,
                    Err(error) => anyhow::anyhow!("{error}; cleanup failure: {restore_error}"),
                });
            }
        }
        action_result?;
    }

    Ok(())
}

/// Press-hold-move-release drag via `SendInput`. Companion to
/// [`send_click_synthesized`] for the `drag` tool's `delivery_mode:"foreground"`
/// path.
///
/// Why a SendInput drag is needed at all: the PostMessage drag path posts
/// `WM_LBUTTONDOWN` + `WM_MOUSEMOVE`s + `WM_LBUTTONUP` to the target HWND.
/// PostMessage does NOT update the per-thread keyboard state that
/// `GetKeyState(VK_LBUTTON)` reads, so frameworks that poll the button-held
/// state during their drag handler (WPF's Thumb.IsDragging logic does this
/// via Mouse.LeftButton, which polls GetKeyState) never observe the button
/// as down and the drag is a no-op. SendInput goes through the system
/// input queue and DOES update GetKeyState, so a WPF Slider thumb actually
/// tracks the drag.
///
/// Same UIAccess constraints as [`send_click_synthesized`] — the
/// `SetForegroundWindow` swap is rejected from non-UIAccess processes
/// when foreground-lock is active; route through `cua-driver-uia.exe`
/// for reliable operation.
pub fn send_drag_synthesized(
    target: u64,
    expected_pid: Option<u32>,
    sx_from: i32,
    sy_from: i32,
    sx_to: i32,
    sy_to: i32,
    duration_ms: u64,
    steps: usize,
    button: &str,
) -> Result<()> {
    let target = HWND(target as *mut _);
    if target.0.is_null() {
        bail!("invalid target hwnd");
    }
    if let Some(msg) = crate::input::post_message_blocked_by_uipi(target.0 as u64) {
        bail!(msg);
    }

    let (down_flag, up_flag) = match button {
        "right" => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
        "middle" => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
        _ => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
    };

    let (vd_x, vd_y, vd_w, vd_h) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1),
            GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1),
        )
    };
    // Same VIRTUALDESK normalization as `send_click_synthesized`; see
    // `crate::virtualdesk` for the math + the cross-platform unit tests.
    let norm = |sx: i32, sy: i32| -> (i32, i32) {
        crate::virtualdesk::to_virtualdesk_absolute(sx, sy, vd_x, vd_y, vd_w, vd_h)
    };
    let make_input = |dx: i32, dy: i32, flags| INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: 0,
                dwFlags: flags | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };

    let steps = steps.max(1);
    let step_delay_ms = if steps > 1 {
        duration_ms / steps as u64
    } else {
        0
    };

    unsafe {
        let target_addr = target.0 as usize as u64;
        let approved_pid = expected_pid
            .or_else(|| crate::win32::window_owner_pid(target_addr))
            .ok_or_else(|| anyhow::anyhow!("invalid or stale target hwnd"))?;
        let target_before = crate::win32::capture_foreground_target(target_addr, Some(approved_pid))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "foreground_unavailable: exact target HWND {:?} changed ownership before drag targeting; no window or input mutation was attempted",
                    target.0
                )
            })?;
        let prev_fg_target = match crate::win32::capture_current_foreground_target() {
            Some(previous) if previous.hwnd() != target_addr => Some(previous),
            Some(_) => None,
            None => {
                bail!(
                    "foreground_restore_unavailable: a stable prior foreground identity could not be captured; no drag mutation was attempted"
                )
            }
        };
        let cursor_target = crate::win32::capture_current_cursor_target().ok_or_else(|| {
            anyhow::anyhow!(
                "cursor_restore_unavailable: stable prior cursor recipient could not be captured; no drag mutation was attempted"
            )
        })?;

        let activated = crate::input::force_foreground_assisted_for_pid(target, approved_pid).0;
        let foreground_target =
            crate::win32::capture_foreground_target(target_addr, Some(approved_pid));
        let exact_foreground = foreground_target.is_some_and(|approved| {
            crate::win32::foreground_matches_target_or_owned_window(
                approved,
                GetForegroundWindow().0 as usize as u64,
            )
        });
        if !activated || !exact_foreground {
            let restore_failed = prev_fg_target.is_some_and(|previous| {
                crate::win32::restore_foreground_target_if_still_displaced(
                    previous,
                    target_before,
                    Duration::from_millis(500),
                ) == crate::win32::ForegroundRestoreOutcome::Failed
            });
            bail!(
                "foreground_unavailable: Windows did not confirm exact target HWND {:?} before foreground drag; no input was sent{}",
                target.0,
                if restore_failed {
                    "; cleanup failure: foreground restoration was not confirmed"
                } else {
                    ""
                }
            );
        }
        let foreground_target = foreground_target.expect("exact foreground target checked above");

        let mut drag_error = None;
        let mut button_down_maybe = false;
        let mut last_authorized = (sx_from, sy_from);
        if !exact_target_receives_point(target_addr, approved_pid, sx_from, sy_from) {
            drag_error = Some(anyhow::anyhow!(
                "foreground_unavailable: exact target does not own the drag start point; no input was sent"
            ));
        }

        if drag_error.is_none() {
            let (nfx, nfy) = norm(sx_from, sy_from);
            let _ = SetCursorPos(sx_from, sy_from);
            let still_authorized =
                exact_target_receives_point(target_addr, approved_pid, sx_from, sy_from)
                    && crate::win32::foreground_matches_target_or_owned_window(
                        foreground_target,
                        GetForegroundWindow().0 as usize as u64,
                    );
            if !still_authorized {
                drag_error = Some(anyhow::anyhow!(
                    "foreground_unavailable: exact target lost foreground or drag-start recipient authority before SendInput; no input was sent"
                ));
            } else {
                let prelude = [
                    make_input(nfx, nfy, MOUSEEVENTF_MOVE),
                    make_input(nfx, nfy, down_flag),
                ];
                let sent = SendInput(&prelude, std::mem::size_of::<INPUT>() as i32);
                button_down_maybe = sent as usize >= prelude.len();
                if sent as usize != prelude.len() {
                    let cleanup = [make_input(0, 0, up_flag)];
                    let cleanup_authorized = sent as usize >= 2
                        && exact_target_receives_point(
                            target_addr,
                            approved_pid,
                            last_authorized.0,
                            last_authorized.1,
                        )
                        && crate::win32::foreground_matches_target_or_owned_window(
                            foreground_target,
                            GetForegroundWindow().0 as usize as u64,
                        );
                    let cleanup_sent = if cleanup_authorized {
                        SendInput(&cleanup, std::mem::size_of::<INPUT>() as i32)
                    } else {
                        0
                    };
                    drag_error = Some(anyhow::anyhow!(
                        "SendInput drag-prelude inserted {sent}/{} events; release cleanup {} and inserted {cleanup_sent}/{}",
                        prelude.len(),
                        if cleanup_authorized { "was authorized" } else { "was withheld because recipient authority was lost" },
                        if cleanup_authorized { cleanup.len() } else { 0 }
                    ));
                    button_down_maybe = false;
                }
            }
        }

        if drag_error.is_none() {
            for i in 1..=steps {
                let t = i as f64 / steps as f64;
                let x = sx_from + ((sx_to - sx_from) as f64 * t).round() as i32;
                let y = sy_from + ((sy_to - sy_from) as f64 * t).round() as i32;
                let authorized_before_move =
                    exact_target_receives_point(target_addr, approved_pid, x, y)
                        && crate::win32::foreground_matches_target_or_owned_window(
                            foreground_target,
                            GetForegroundWindow().0 as usize as u64,
                        );
                if !authorized_before_move {
                    drag_error = Some(anyhow::anyhow!(
                        "foreground_unavailable: exact target lost foreground or recipient authority during drag; no further movement was sent"
                    ));
                    break;
                }
                let (nx, ny) = norm(x, y);
                let _ = SetCursorPos(x, y);
                if !exact_target_receives_point(target_addr, approved_pid, x, y) {
                    drag_error = Some(anyhow::anyhow!(
                        "foreground_unavailable: drag recipient changed after cursor placement; no further SendInput movement was sent"
                    ));
                    break;
                }
                let mv = [make_input(nx, ny, MOUSEEVENTF_MOVE)];
                let moved = SendInput(&mv, std::mem::size_of::<INPUT>() as i32);
                if moved as usize != mv.len() {
                    drag_error = Some(anyhow::anyhow!(
                        "SendInput drag movement inserted {moved}/{} events",
                        mv.len()
                    ));
                    break;
                }
                last_authorized = (x, y);
                if step_delay_ms > 0 {
                    sleep(Duration::from_millis(step_delay_ms));
                }
            }
        }

        if drag_error.is_none()
            && (!exact_target_receives_point(
                target_addr,
                approved_pid,
                last_authorized.0,
                last_authorized.1,
            ) || !crate::win32::foreground_matches_target_or_owned_window(
                foreground_target,
                GetForegroundWindow().0 as usize as u64,
            ))
        {
            drag_error = Some(anyhow::anyhow!(
                "foreground_unavailable: exact target lost foreground or recipient authority before drag release"
            ));
        }
        if button_down_maybe {
            let release = [make_input(0, 0, up_flag)];
            let release_authorized = exact_target_receives_point(
                target_addr,
                approved_pid,
                last_authorized.0,
                last_authorized.1,
            ) && crate::win32::foreground_matches_target_or_owned_window(
                foreground_target,
                GetForegroundWindow().0 as usize as u64,
            );
            let released = if release_authorized {
                SendInput(&release, std::mem::size_of::<INPUT>() as i32)
            } else {
                0
            };
            if released as usize != release.len() {
                let release_error = anyhow::anyhow!(
                    "SendInput drag release {} and inserted {released}/{} events",
                    if release_authorized {
                        "was authorized"
                    } else {
                        "was withheld because recipient authority was lost"
                    },
                    release.len()
                );
                drag_error = Some(match drag_error {
                    Some(error) => anyhow::anyhow!("{error}; cleanup failure: {release_error}"),
                    None => release_error,
                });
            }
        }

        sleep(Duration::from_millis(40));
        if !crate::win32::restore_cursor_target(cursor_target) {
            let restore_error = anyhow::anyhow!(
                "cursor_restore_failed: stable prior cursor destination was not confirmed after drag"
            );
            drag_error = Some(match drag_error {
                Some(error) => anyhow::anyhow!("{error}; {restore_error}"),
                None => restore_error,
            });
        }
        let restore_outcome = prev_fg_target.map(|previous| {
            crate::win32::restore_foreground_target_if_still_displaced(
                previous,
                foreground_target,
                Duration::from_millis(500),
            )
        });
        if restore_outcome == Some(crate::win32::ForegroundRestoreOutcome::Failed) {
            let restore_error = anyhow::anyhow!(
                "foreground_restore_failed: drag ended, but Windows did not confirm restoration of the prior foreground window"
            );
            drag_error = Some(match drag_error {
                Some(error) => anyhow::anyhow!("{error}; {restore_error}"),
                None => restore_error,
            });
        }
        if let Some(error) = drag_error {
            return Err(error);
        }
    }

    Ok(())
}

/// Standard wheel notch delta. A `mouseData` value of `±WHEEL_DELTA` is one
/// detent of the physical mouse wheel.
const WHEEL_DELTA: i32 = 120;

/// Compute the `MOUSEINPUT::mouseData` value for a wheel event of `ticks`
/// detents. Positive ticks = wheel forward/up (vertical) or right (horizontal);
/// negative = down / left. `mouseData` is a `u32` field carrying a signed
/// 32-bit delta, so we compute as `i32` then bit-cast to `u32` (this is what
/// the Win32 docs mean by "the value is a multiple of WHEEL_DELTA").
///
/// Factored out of [`send_wheel_synthesized`] so the sign/magnitude encoding is
/// unit-testable without a live display / `SendInput`.
fn wheel_mouse_data(ticks: i32) -> u32 {
    (WHEEL_DELTA * ticks) as u32
}

/// Synthesize a single mouse-wheel event at screen coordinates `(sx, sy)` via
/// `SendInput`.
///
/// The OS routes wheel input to the window **under the cursor**, not the
/// foreground window, so we `SetCursorPos(sx, sy)` first to place the wheel
/// over the intended target. `ticks` encodes both magnitude and direction:
/// positive scrolls up (vertical) / right (horizontal), negative scrolls down /
/// left — matching the `MOUSEEVENTF_WHEEL` / `MOUSEEVENTF_HWHEEL` convention
/// where `mouseData = WHEEL_DELTA * ticks`.
///
/// Unlike [`send_click_synthesized`] this does NOT do a foreground swap: wheel
/// delivery follows the cursor, so positioning the cursor is sufficient. The
/// cursor is restored to its previous position afterward.
pub fn send_wheel_synthesized(sx: i32, sy: i32, ticks: i32, horizontal: bool) -> Result<()> {
    send_wheel_synthesized_for_target(None, sx, sy, ticks, horizontal)
}

pub fn send_wheel_synthesized_for_target(
    target: Option<(u64, u32)>,
    sx: i32,
    sy: i32,
    ticks: i32,
    horizontal: bool,
) -> Result<()> {
    let flag = if horizontal {
        MOUSEEVENTF_HWHEEL
    } else {
        MOUSEEVENTF_WHEEL
    };
    let mouse_data = wheel_mouse_data(ticks);

    let wheel_input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: mouse_data,
                dwFlags: flag,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };

    unsafe {
        let cursor_target = target
            .map(|_| {
                crate::win32::capture_current_cursor_target().ok_or_else(|| {
                    anyhow::anyhow!(
                        "cursor_restore_unavailable: stable prior cursor recipient could not be captured; no wheel input was sent"
                    )
                })
            })
            .transpose()?;
        let mut prev_cursor = POINT::default();
        if cursor_target.is_none() {
            let _ = GetCursorPos(&mut prev_cursor);
        }

        if let Some((hwnd, pid)) = target {
            if crate::win32::capture_foreground_target(hwnd, Some(pid)).is_none() {
                bail!(
                    "foreground_unavailable: exact target HWND {hwnd:#x} changed ownership at the wheel-input boundary; no input was sent"
                );
            }
        }

        // Wheel routes to the window under the cursor — place it on the target.
        let _ = SetCursorPos(sx, sy);

        if let Some((hwnd, pid)) = target {
            if !exact_target_receives_point(hwnd, pid, sx, sy) {
                let restored = cursor_target.is_some_and(crate::win32::restore_cursor_target);
                bail!(
                    "foreground_unavailable: exact target no longer owns the wheel recipient at ({sx},{sy}); no wheel input was sent{}",
                    if restored { "" } else { "; cleanup failure: stable prior cursor destination was not restored" }
                );
            }
        }

        let events = [wheel_input];
        let sent = SendInput(&events, std::mem::size_of::<INPUT>() as i32);
        if sent as usize != events.len() {
            let restored = if let Some(cursor_target) = cursor_target {
                crate::win32::restore_cursor_target(cursor_target)
            } else {
                SetCursorPos(prev_cursor.x, prev_cursor.y).is_ok()
            };
            bail!(
                "SendInput inserted {sent}/{} wheel events{}",
                events.len(),
                if restored {
                    ""
                } else {
                    "; cleanup failure: stable prior cursor destination was not restored"
                }
            );
        }

        // Brief settle, then restore the cursor.
        sleep(Duration::from_millis(20));
        let restored = if let Some(cursor_target) = cursor_target {
            crate::win32::restore_cursor_target(cursor_target)
        } else {
            SetCursorPos(prev_cursor.x, prev_cursor.y).is_ok()
        };
        if !restored {
            bail!("cursor_restore_failed: stable prior cursor destination was not confirmed after wheel input");
        }
    }

    Ok(())
}

/// Return the current foreground window for desktop-scope keyboard and drag
/// operations, which intentionally target whatever the user can currently see.
pub fn foreground_window() -> Result<u64> {
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.0.is_null() {
        bail!("no foreground window is available");
    }
    Ok(hwnd.0 as u64)
}

/// Move the real OS pointer to a desktop coordinate.
pub fn move_cursor_desktop(x: i32, y: i32) -> Result<()> {
    unsafe { SetCursorPos(x, y) }
        .map_err(|error| anyhow::anyhow!("SetCursorPos({x}, {y}) failed: {error}"))
}

/// Classify a `WM_NCHITTEST` result into the non-client regions where a
/// posted-message drag can NEVER work: the OS handles caption and border
/// drags with its interactive move/resize modal loop
/// (`WM_NCLBUTTONDOWN` → `DefWindowProc` → `WM_SYSCOMMAND SC_MOVE/SC_SIZE`),
/// which only real pointer input off the system input queue can drive.
/// Posted `WM_MOUSEMOVE` streams are simply discarded, so background
/// delivery must refuse with `background_unavailable` instead of reporting
/// a success that moved nothing.
pub fn classify_nc_move_resize_hit(hit: isize) -> Option<&'static str> {
    // Constant values per winuser.h; kept as literals because the windows
    // crate exposes hit-test codes as u32 while SendMessage returns LRESULT.
    match hit {
        2 => Some("caption / title bar"), // HTCAPTION
        4 => Some("size box"),            // HTGROWBOX / HTSIZE
        10..=17 => Some("resize border"), // HTLEFT..HTBOTTOMRIGHT
        _ => None,
    }
}

/// Screen-coordinate `WM_NCHITTEST` against the drag target's top-level
/// window. Returns the human-readable region name when the point falls on a
/// caption / resize border (the regions a posted drag cannot actuate), else
/// `None`. Uses `SendMessageTimeoutW(SMTO_ABORTIFHUNG)` so a hung target
/// cannot wedge the daemon; timeouts fail open (`None`) — the posted drag
/// then proceeds exactly as before this check existed.
pub fn non_client_move_resize_hit(root: u64, sx: i32, sy: i32) -> Option<&'static str> {
    use windows::Win32::UI::WindowsAndMessaging::{
        SendMessageTimeoutW, SMTO_ABORTIFHUNG, WM_NCHITTEST,
    };
    if root == 0 {
        return None;
    }
    let hwnd = HWND(root as *mut _);
    // Hit-test the top-level frame: the caption belongs to the root window
    // even when the caller resolved a child HWND as the drag target.
    let top = unsafe { GetAncestor(hwnd, GA_ROOT) };
    let top = if top.0.is_null() { hwnd } else { top };
    // WM_NCHITTEST takes SCREEN coordinates packed in LPARAM (signed 16-bit
    // each — correct for any virtual-desktop position within ±32767).
    let lp = LPARAM((((sy as i16 as u16 as u32) << 16) | (sx as i16 as u16 as u32)) as isize);
    let mut result: usize = 0;
    let ok = unsafe {
        SendMessageTimeoutW(
            top,
            WM_NCHITTEST,
            WPARAM(0),
            lp,
            SMTO_ABORTIFHUNG,
            200, // ms — a live window answers this in microseconds
            Some(&mut result),
        )
    };
    if ok.0 == 0 {
        return None; // timed out / hung target: fail open, post as before
    }
    classify_nc_move_resize_hit(result as isize)
}

#[cfg(test)]
mod nc_hit_tests {
    use super::classify_nc_move_resize_hit;

    #[test]
    fn caption_and_borders_refuse_posted_drags_client_area_does_not() {
        assert_eq!(
            classify_nc_move_resize_hit(2), // HTCAPTION
            Some("caption / title bar")
        );
        assert_eq!(classify_nc_move_resize_hit(4), Some("size box")); // HTGROWBOX
        for border in 10..=17 {
            // HTLEFT..HTBOTTOMRIGHT
            assert_eq!(classify_nc_move_resize_hit(border), Some("resize border"));
        }
        assert_eq!(classify_nc_move_resize_hit(1), None); // HTCLIENT
        assert_eq!(classify_nc_move_resize_hit(0), None); // HTNOWHERE
        assert_eq!(classify_nc_move_resize_hit(3), None); // HTSYSMENU (click, not drag-move)
        assert_eq!(classify_nc_move_resize_hit(-1), None); // HTERROR
        assert_eq!(classify_nc_move_resize_hit(18), None); // HTBORDER (non-resizable edge)
    }
}

#[cfg(test)]
mod wheel_tests {
    use super::{posted_press_message, wheel_mouse_data, WHEEL_DELTA};
    use windows::Win32::UI::WindowsAndMessaging::{WM_LBUTTONDBLCLK, WM_LBUTTONDOWN};

    #[test]
    fn posted_double_click_uses_the_win32_double_click_message() {
        assert_eq!(
            posted_press_message(WM_LBUTTONDOWN, WM_LBUTTONDBLCLK, 0, true),
            WM_LBUTTONDOWN
        );
        assert_eq!(
            posted_press_message(WM_LBUTTONDOWN, WM_LBUTTONDBLCLK, 1, true),
            WM_LBUTTONDBLCLK
        );
        assert_eq!(
            posted_press_message(WM_LBUTTONDOWN, WM_LBUTTONDBLCLK, 2, true),
            WM_LBUTTONDOWN
        );
        assert_eq!(
            posted_press_message(WM_LBUTTONDOWN, WM_LBUTTONDBLCLK, 1, false),
            WM_LBUTTONDOWN
        );
    }

    #[test]
    fn wheel_data_up_is_positive_one_notch() {
        // +1 tick (up / right) → +WHEEL_DELTA, bit-cast to u32.
        assert_eq!(wheel_mouse_data(1), WHEEL_DELTA as u32);
        assert_eq!(wheel_mouse_data(1), 120u32);
    }

    #[test]
    fn wheel_data_down_is_negative_one_notch() {
        // -1 tick (down / left) → -WHEEL_DELTA, bit-cast: 0xFFFFFF88.
        assert_eq!(wheel_mouse_data(-1), (-WHEEL_DELTA) as u32);
        assert_eq!(wheel_mouse_data(-1), 0xFFFF_FF88);
    }

    #[test]
    fn wheel_data_scales_with_ticks() {
        assert_eq!(wheel_mouse_data(3), (3 * WHEEL_DELTA) as u32);
        assert_eq!(wheel_mouse_data(3), 360u32);
        assert_eq!(wheel_mouse_data(-3) as i32, -360);
    }

    #[test]
    fn wheel_data_zero_is_zero() {
        assert_eq!(wheel_mouse_data(0), 0);
    }
}
