//! Universal background mouse actuator: coordinate-routed pointer/touch
//! injection that delivers a click to whatever window sits under a screen
//! point **without** `SetForegroundWindow` and **without** moving the user's
//! mouse cursor.
//!
//! Why this exists: the PostMessage path (`mouse::post_click`) is invisible and
//! never raises, but Chromium/Electron/GTK/WPF content silently ignore posted
//! synthetic `WM_*BUTTON` messages — their input arrives through the *system
//! input queue*, not the per-window message queue. The historical fallback for
//! those was `send_click_synthesized` (SendInput + a `SetForegroundWindow`
//! swap), which is exactly the visible "flash" we want to eliminate.
//!
//! Touch injection routes by coordinate through the system input queue (so
//! Chromium et al. accept it; the OS promotes it to `WM_*BUTTON` for legacy
//! Win32 windows that don't consume `WM_POINTER`), and — per the RE in
//! `docs/windows-background-input-re-plan.md` §4.4 — the kernel injection path
//! (`NtUserInjectMouseInput`/`NtUserInjectTouchInput`) gates only on a
//! per-process injection-enable, NOT on the target being foreground. The one
//! residual is that a tap on an *inactive* top-level window can still trigger
//! click-activation; we contain that with `NoActivateGuard` (WS_EX_NOACTIVATE)
//! plus conditional exact-identity restoration of the USER's foreground
//! afterward. Per the macOS-aligned contract a background actuation
//! never raises/restacks the target: when coordinate-routed input can't reach
//! it (occluded at the point), the caller returns `background_unavailable`
//! rather than raising it (see `target_visible_at_point`).
//!
//! Scope: left-button taps (single/double/triple). Right/middle have no clean
//! touch mapping; callers fall back to their existing routing for those.

use anyhow::{bail, Result};
use core::ffi::c_void;
use std::sync::Mutex;
use std::thread::sleep;
use std::time::Duration;

use windows::Win32::Foundation::{HANDLE, HWND, POINT, RECT};
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Controls::{
    CreateSyntheticPointerDevice, DestroySyntheticPointerDevice, HSYNTHETICPOINTERDEVICE,
    POINTER_FEEDBACK_DEFAULT, POINTER_TYPE_INFO, POINTER_TYPE_INFO_0,
};
use windows::Win32::UI::Input::Pointer::{
    InjectSyntheticPointerInput, POINTER_FLAG_DOWN, POINTER_FLAG_INCONTACT, POINTER_FLAG_INRANGE,
    POINTER_FLAG_UP, POINTER_FLAG_UPDATE, POINTER_INFO, POINTER_PEN_INFO, POINTER_TOUCH_INFO,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetAncestor, GetForegroundWindow, GetWindowLongPtrW, GetWindowThreadProcessId,
    LockSetForegroundWindow, SetForegroundWindow, SetWindowLongPtrW, WindowFromPoint, GA_ROOT,
    GWL_EXSTYLE, LSFW_LOCK, LSFW_UNLOCK, PT_PEN, PT_TOUCH, WS_EX_NOACTIVATE,
};

#[derive(Default)]
struct ForegroundLockState {
    holders: usize,
    locked: bool,
}

static FOREGROUND_LOCK_STATE: Mutex<ForegroundLockState> = Mutex::new(ForegroundLockState {
    holders: 0,
    locked: false,
});

/// Prevent other processes from taking the foreground during a background
/// launch. Windows automatically clears this lock on genuine user input; Drop
/// still balances the documented unlock call and overlapping driver launches.
pub struct ForegroundLockGuard {
    held: bool,
}

impl ForegroundLockGuard {
    pub fn acquire() -> Self {
        let mut state = FOREGROUND_LOCK_STATE.lock().unwrap();
        if state.holders == 0 {
            state.locked = unsafe { LockSetForegroundWindow(LSFW_LOCK) }.is_ok();
            if state.locked {
                tracing::debug!(target: "launch_app.focus_lock", "locked foreground changes during background launch");
            } else {
                tracing::warn!(target: "launch_app.focus_lock", "could not lock foreground changes during background launch");
            }
        }
        if state.locked {
            state.holders += 1;
        }
        Self { held: state.locked }
    }

    pub fn acquired(&self) -> bool {
        self.held
    }
}

impl Drop for ForegroundLockGuard {
    fn drop(&mut self) {
        if !self.held {
            return;
        }
        let mut state = FOREGROUND_LOCK_STATE.lock().unwrap();
        state.holders = state.holders.saturating_sub(1);
        if state.holders == 0 {
            let _ = unsafe { LockSetForegroundWindow(LSFW_UNLOCK) };
            state.locked = false;
            tracing::debug!(target: "launch_app.focus_lock", "unlocked foreground changes after background launch");
        }
    }
}

/// Bring `target` to the foreground using the AttachThreadInput trick, which
/// inherits the current foreground thread's FG-lock token so the swap is
/// honored even on a foreground-locked session without UIAccess (mirrors the
/// `bring_to_front` tool). Single attach, no retry loop — bounded. Returns
/// whether `target` actually became foreground.
pub(crate) unsafe fn force_foreground_attached(target: HWND) -> bool {
    let cur = GetForegroundWindow();
    if cur == target {
        return true;
    }
    let my_tid = GetCurrentThreadId();
    let mut pid = 0u32;
    let cur_tid = GetWindowThreadProcessId(cur, Some(&mut pid));
    let attached = cur_tid != 0 && cur_tid != my_tid;
    if attached {
        let _ = AttachThreadInput(my_tid, cur_tid, true);
    }
    let _ = SetForegroundWindow(target);
    if attached {
        let _ = AttachThreadInput(my_tid, cur_tid, false);
    }
    GetForegroundWindow() == target
}

unsafe fn force_foreground_attached_for_pid(target: HWND, expected_pid: u32) -> bool {
    let target_addr = target.0 as usize as u64;
    if crate::win32::capture_foreground_target(target_addr, Some(expected_pid)).is_none() {
        return false;
    }
    let cur = GetForegroundWindow();
    if cur == target {
        return true;
    }
    let my_tid = GetCurrentThreadId();
    let mut foreground_pid = 0u32;
    let cur_tid = GetWindowThreadProcessId(cur, Some(&mut foreground_pid));
    let attached = cur_tid != 0 && cur_tid != my_tid;
    if attached {
        let _ = AttachThreadInput(my_tid, cur_tid, true);
    }
    let still_owned =
        crate::win32::capture_foreground_target(target_addr, Some(expected_pid)).is_some();
    let requested = still_owned && SetForegroundWindow(target).as_bool();
    if attached {
        let _ = AttachThreadInput(my_tid, cur_tid, false);
    }
    requested
        && crate::win32::capture_foreground_target(target_addr, Some(expected_pid)).is_some()
        && GetForegroundWindow() == target
}

/// Attempt one bounded foreground transition without emitting any synthetic
/// input. Foreground-lock denial is a fail-closed refusal.
pub(crate) unsafe fn force_foreground_assisted(target: HWND) -> (bool, bool) {
    (unsafe { force_foreground_attached(target) }, false)
}

/// PID-bound foreground transition for exact-window transactions. Ownership is
/// re-proved before every activation retry. Unlike the desktop-scoped helper,
/// this variant never emits a reserved global key while another application is
/// foreground; foreground-lock denial is a refusal.
pub(crate) unsafe fn force_foreground_assisted_for_pid(
    target: HWND,
    expected_pid: u32,
) -> (bool, bool) {
    let target_addr = target.0 as usize as u64;
    for _ in 0..3 {
        if crate::win32::capture_foreground_target(target_addr, Some(expected_pid)).is_none() {
            return (false, false);
        }
        if unsafe { force_foreground_attached_for_pid(target, expected_pid) } {
            return (true, false);
        }
        sleep(Duration::from_millis(25));
    }
    (false, false)
}

/// RAII guard that makes a specific target window **unable to become the
/// foreground/active window** for the duration of an actuation, by adding the
/// `WS_EX_NOACTIVATE` extended style to its top-level window.
///
/// Why this and not a global foreground-lock: our own injected/posted input
/// (or a UIA-Invoke) legitimizes the target's foreground claim, so even a
/// maxed `SPI_*FOREGROUNDLOCKTIMEOUT` won't stop the steal. `WS_EX_NOACTIVATE`
/// is categorical — Windows refuses to activate the window *at all* (clicks,
/// `SetForegroundWindow(self)` from WPF/XAML/Tauri handlers, mouse-activate) —
/// while the window still RECEIVES the click/key. It is per-window (no session
/// side effects) and reversed on drop. Covers the self-activation that the
/// EnableWindow/UWP bypass cannot (WPF `UIElement.Focus()`→SetForegroundWindow).
pub struct NoActivateGuard {
    // Store the handle as an integer so the guard is `Send` and can be held
    // across `.await` in the async tools.
    root_addr: isize,
    expected_pid: Option<u32>,
    applied: bool,
}

impl NoActivateGuard {
    /// Arm on the top-level (GA_ROOT) ancestor of `hwnd`.
    pub fn arm(hwnd: HWND) -> Self {
        Self::arm_inner(hwnd, None)
            .expect("PID-less NoActivateGuard arming cannot reject ownership")
    }

    pub fn arm_for_pid(hwnd: HWND, expected_pid: u32) -> Result<Self> {
        Self::arm_inner(hwnd, Some(expected_pid))
    }

    fn arm_inner(hwnd: HWND, expected_pid: Option<u32>) -> Result<Self> {
        unsafe {
            let root = {
                let r = GetAncestor(hwnd, GA_ROOT);
                if r.0.is_null() {
                    hwnd
                } else {
                    r
                }
            };
            if expected_pid.is_some_and(|pid| {
                crate::win32::window_owner_pid(root.0 as usize as u64) != Some(pid)
            }) {
                bail!("exact target HWND changed ownership before NoActivateGuard mutation");
            }
            let prev = GetWindowLongPtrW(root, GWL_EXSTYLE);
            let want = WS_EX_NOACTIVATE.0 as isize;
            // Apply WS_EX_NOACTIVATE if not already set (prev can be 0, so don't
            // gate on it — we just need to check the bit and set it if absent).
            let was_present = (prev & want) != 0;
            if !was_present
                && expected_pid.is_some_and(|pid| {
                    crate::win32::window_owner_pid(root.0 as usize as u64) != Some(pid)
                })
            {
                bail!("exact target HWND changed ownership at NoActivateGuard mutation");
            }
            let applied = !was_present && {
                SetWindowLongPtrW(root, GWL_EXSTYLE, prev | want);
                // Confirm it took (cross-process SetWindowLongPtr can be denied
                // by UIPI on higher-integrity targets).
                (GetWindowLongPtrW(root, GWL_EXSTYLE) & want) != 0
            };
            if !was_present
                && expected_pid.is_some_and(|pid| {
                    crate::win32::window_owner_pid(root.0 as usize as u64) != Some(pid)
                })
            {
                bail!(
                    "exact target HWND changed ownership after NoActivateGuard mutation; restoration withheld"
                );
            }
            if expected_pid.is_some() && !was_present && !applied {
                bail!("NoActivateGuard mutation did not read back");
            }
            Ok(Self {
                root_addr: root.0 as isize,
                expected_pid,
                applied,
            })
        }
    }

    pub fn finish(&mut self) -> Result<()> {
        if !self.applied {
            return Ok(());
        }
        unsafe {
            let root = HWND(self.root_addr as *mut _);
            if self.expected_pid.is_some_and(|pid| {
                crate::win32::window_owner_pid(root.0 as usize as u64) != Some(pid)
            }) {
                bail!("exact target HWND changed ownership; NoActivateGuard restoration refused");
            }
            let current = GetWindowLongPtrW(root, GWL_EXSTYLE);
            let noactivate = WS_EX_NOACTIVATE.0 as isize;
            SetWindowLongPtrW(root, GWL_EXSTYLE, current & !noactivate);
            if GetWindowLongPtrW(root, GWL_EXSTYLE) & noactivate != 0 {
                bail!("NoActivateGuard restoration did not read back");
            }
            self.applied = false;
        }
        Ok(())
    }
}

impl Drop for NoActivateGuard {
    fn drop(&mut self) {
        if self.applied {
            let _ = self.finish();
        }
    }
}

pub fn run_with_noactivate_for_pid<T, E>(
    hwnd: HWND,
    expected_pid: u32,
    action: impl FnOnce() -> std::result::Result<T, E>,
) -> Result<T>
where
    E: std::fmt::Display,
{
    let mut guard = NoActivateGuard::arm_for_pid(hwnd, expected_pid)?;
    let action_result = action().map_err(|error| anyhow::anyhow!(error.to_string()));
    let restore_result = guard.finish();
    match (action_result, restore_result) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(action), Ok(())) => Err(action),
        (Ok(_), Err(restoration)) => Err(restoration),
        (Err(action), Err(restoration)) => Err(anyhow::anyhow!(
            "{action}; no-activate cleanup: {restoration}"
        )),
    }
}

/// PEN_FLAG_BARREL (winuser.h) — pen barrel button held == secondary (right)
/// button. `penFlags` is a raw u32 in the bindings, so use the literal.
const PEN_FLAG_BARREL: u32 = 0x0000_0001;

fn cleanup_release_is_authorized(down_may_have_landed: bool, recipient_authorized: bool) -> bool {
    down_may_have_landed && recipient_authorized
}

// (Removed ZorderGuard.) The macOS-aligned contract forbids a `background`
// actuation from raising/restacking the target: coordinate-routed pen/touch
// only lands on the topmost VISIBLE window, so an occluded target is reported
// as `background_unavailable` (see `target_visible_at_point`) and the agent
// escalates to `delivery_mode:"foreground"` instead of the driver raising it.

/// One down→up **pen** tap at screen `(sx, sy)`. When `barrel` is set the pen's
/// barrel button is held for the contact, which the system maps to a secondary
/// (right) click — both for `WM_POINTER`-aware apps (Chromium/WPF/UWP) and via
/// pen→mouse promotion for legacy Win32. A fresh synthetic pen device is
/// created and destroyed per tap (right/middle clicks are rare).
fn pen_taps(
    target: HWND,
    expected_pid: u32,
    sx: i32,
    sy: i32,
    barrel: bool,
    count: usize,
) -> Result<()> {
    unsafe {
        let dev = CreateSyntheticPointerDevice(PT_PEN, 1, POINTER_FEEDBACK_DEFAULT)
            .map_err(|e| anyhow::anyhow!("CreateSyntheticPointerDevice(PEN): {e}"))?;
        let pen_flags = if barrel { PEN_FLAG_BARREL } else { 0 };
        let mk = |flags| POINTER_TYPE_INFO {
            r#type: PT_PEN,
            Anonymous: POINTER_TYPE_INFO_0 {
                penInfo: POINTER_PEN_INFO {
                    pointerInfo: POINTER_INFO {
                        pointerType: PT_PEN,
                        pointerId: 0,
                        pointerFlags: flags,
                        sourceDevice: HANDLE::default(),
                        hwndTarget: HWND::default(),
                        ptPixelLocation: POINT { x: sx, y: sy },
                        ..Default::default()
                    },
                    penFlags: pen_flags,
                    penMask: 0,
                    pressure: 512,
                    rotation: 0,
                    tiltX: 0,
                    tiltY: 0,
                },
            },
        };
        let down = mk(POINTER_FLAG_DOWN | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT);
        let up = mk(POINTER_FLAG_UP);
        // Reuse the SAME synthetic device for every tap. A double/triple click
        // is two/three down-up cycles from one digitizer; creating a fresh
        // device per tap (the old loop) fails the next
        // CreateSyntheticPointerDevice in quick succession — which is exactly
        // why background double_click on Chromium errored. See #1984.
        let mut result: Result<()> = Ok(());
        let n = count.max(1);
        for i in 0..n {
            if !exact_target_visible_at_point(target, expected_pid, sx, sy) {
                result = Err(anyhow::anyhow!(
                    "exact target ownership or recipient changed before synthetic pen down"
                ));
                break;
            }
            let down_result = InjectSyntheticPointerInput(dev, &[down]);
            sleep(Duration::from_millis(25));
            let still_authorized = exact_target_visible_at_point(target, expected_pid, sx, sy);
            let cleanup_result = cleanup_release_is_authorized(true, still_authorized)
                .then(|| InjectSyntheticPointerInput(dev, &[up]));
            result = match (down_result, cleanup_result, still_authorized) {
                (Ok(()), Some(Ok(())), true) => Ok(()),
                (Ok(()), None, false) => Err(anyhow::anyhow!(
                    "exact target ownership or recipient changed after synthetic pen down; up cleanup was withheld and the transient device will be destroyed"
                )),
                (Ok(()), Some(Err(cleanup)), _) => Err(anyhow::anyhow!(
                    "synthetic pen down may have landed; mandatory up cleanup failed: {cleanup}"
                )),
                (Err(primary), Some(Ok(())), _) => Err(anyhow::anyhow!(
                    "synthetic pen down failed: {primary}; mandatory up cleanup succeeded"
                )),
                (Err(primary), Some(Err(cleanup)), _) => Err(anyhow::anyhow!(
                    "synthetic pen down failed: {primary}; mandatory up cleanup also failed: {cleanup}"
                )),
                (Err(primary), None, false) => Err(anyhow::anyhow!(
                    "synthetic pen down failed: {primary}; up cleanup was withheld because recipient authority was lost"
                )),
                (_, Some(_), false) => {
                    unreachable!("unauthorized pen cleanup is never injected")
                }
                (_, None, true) => unreachable!("authorized cleanup always produces a result"),
            };
            if result.is_err() {
                break;
            }
            if i + 1 < n {
                sleep(Duration::from_millis(70));
            }
        }
        let _ = DestroySyntheticPointerDevice(dev);
        result?;
    }
    Ok(())
}

/// Inject a click at **screen** coordinates `(sx, sy)`, routed by the system to
/// whatever window is under that point — without a foreground swap and without
/// moving the user's cursor. The target is cloaked for the duration so any
/// click-activation raise stays invisible, then the user's foreground is
/// restored.
///
/// - `left`  → synthetic-pen primary tap (proven path; promoted to a left click
///   for non-pointer-aware apps, and accepted directly by Chromium/WPF/UWP).
/// - `right` → synthetic-pen tap with the barrel button held (secondary click).
/// - `middle`→ unsupported (no clean pointer mapping); returns `Err` so the
///   caller can fall back to its existing routing / structured error.
///
/// We use the same `CreateSyntheticPointerDevice`/`InjectSyntheticPointerInput`
/// path for both buttons — `InjectTouchInput` proved unreliable for left-clicks
/// on Chromium content (returned errors), whereas synthetic-pen injection lands
/// reliably and routes by coordinate with no foreground dependency.
pub fn inject_click_screen(
    target: u64,
    expected_pid: u32,
    sx: i32,
    sy: i32,
    count: usize,
    button: &str,
) -> Result<()> {
    if target == 0 {
        bail!("inject_click_screen: null target window");
    }
    let target_h = HWND(target as *mut _);
    if crate::win32::capture_foreground_target(target, Some(expected_pid)).is_none() {
        bail!("inject_click_screen: target HWND changed ownership");
    }
    if let Some(msg) = crate::input::post_message_blocked_by_uipi(target) {
        // Higher-integrity target: injection into its queue is blocked too.
        bail!(msg);
    }

    let barrel = match button {
        "left" => false,
        "right" => true,
        other => bail!("background injection supports left/right buttons only (got {other:?})"),
    };

    // macOS-aligned contract: a `background` actuation NEVER fronts or raises
    // (mirrors macOS CGEvent-to-pid, which never touches z-order/focus). Synthetic
    // pen/touch is coordinate-routed to the TOP-MOST VISIBLE window at the point,
    // so if the target is occluded there we'd silently click the occluder. We do
    // NOT raise to win the hit-test (that's the foreground rung's job, which the
    // agent opts into). Instead bail — the caller surfaces `background_unavailable`
    // and the agent escalates to `delivery_mode:"foreground"`.
    if !exact_target_visible_at_point(target_h, expected_pid, sx, sy) {
        bail!(
            "background coordinate injection cannot reach this target at ({sx},{sy}) \
             — it is occluded by another window (raising it would break the \
             no-foreground contract). Escalate to delivery_mode:\"foreground\"."
        );
    }
    // Remember who the user had in front with a stable foreground/owner/
    // foreground snapshot so restoration can never target a mixed identity.
    let prev_fg_target = match crate::win32::capture_current_foreground_target() {
        Some(previous) if previous.hwnd() != target => Some(previous),
        Some(_) => None,
        None => {
            bail!(
                "foreground_restore_unavailable: a stable prior foreground identity could not be captured; no background click mutation was attempted"
            )
        }
    };
    let displaced = crate::win32::capture_foreground_target(target, Some(expected_pid))
        .ok_or_else(|| anyhow::anyhow!("inject_click_screen: target ownership changed"))?;
    // Make the target non-activatable for the click so click-activation can't
    // steal foreground. No z-order raise.
    let mut noact = NoActivateGuard::arm_for_pid(target_h, expected_pid)?;
    // One synthetic device does all `count` taps (single/double/triple click).
    let mut result = pen_taps(target_h, expected_pid, sx, sy, barrel, count);
    if let Err(restoration) = noact.finish() {
        result = Err(match result {
            Ok(()) => restoration,
            Err(primary) => anyhow::anyhow!("{primary}; cleanup failure: {restoration}"),
        });
    }
    // `WS_EX_NOACTIVATE` is NOT categorical against a Chromium/Electron content
    // window that calls `SetForegroundWindow(self)` from its (async) click
    // handler. Re-assert the USER's foreground (this restores focus where the
    // user left it — it never raises the target). Short settle + repeat to win
    // the race against the async self-activation.
    if let Some(previous) = prev_fg_target {
        let restored = crate::win32::restore_foreground_target_if_still_displaced(
            previous,
            displaced,
            Duration::from_millis(500),
        );
        if restored == crate::win32::ForegroundRestoreOutcome::Failed {
            let restore_error = anyhow::anyhow!("foreground_restore_failed after background click");
            result = Err(match result {
                Ok(_) => restore_error,
                Err(error) => anyhow::anyhow!("{error}; cleanup failure: {restore_error}"),
            });
        }
    }
    result
}

/// True when `target`'s top-level root is the window actually visible at screen
/// point `(sx, sy)` — i.e. coordinate-routed pen/touch there would land on it
/// rather than an occluder. Used to enforce the macOS-aligned rule that a
/// `background` actuation never raises: when this is false the injection bails
/// and the tool returns `background_unavailable` (escalate to foreground).
unsafe fn target_visible_at_point(target: HWND, sx: i32, sy: i32) -> bool {
    let top = WindowFromPoint(POINT { x: sx, y: sy });
    if top.0.is_null() {
        return false;
    }
    let top_root = GetAncestor(top, GA_ROOT);
    let target_root = GetAncestor(target, GA_ROOT);
    !top_root.0.is_null() && top_root == target_root
}

fn exact_target_visible_at_point(target: HWND, expected_pid: u32, sx: i32, sy: i32) -> bool {
    crate::win32::capture_foreground_target(target.0 as usize as u64, Some(expected_pid)).is_some()
        && unsafe { target_visible_at_point(target, sx, sy) }
}

/// True when screen point `(x, y)` lies within `hwnd`'s window rectangle.
///
/// Guards **element_index** clicks: an element's cached center can fall outside
/// its own window when the element is scrolled out of a ScrollViewer or pushed
/// off-screen (e.g. a tall form on a small display). Tapping the raw coordinate
/// then lands on whatever is actually there — the taskbar, the desktop, another
/// window — instead of the intended element. The click tool turns a `false`
/// here into a clear error rather than clicking the wrong target. Returns `true`
/// (fail-open) when the rect can't be read, so legitimate clicks are never
/// blocked by a transient query failure.
pub fn point_in_window_bounds(hwnd: u64, x: i32, y: i32) -> bool {
    use windows::Win32::Foundation::{HWND, RECT};
    use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;
    if hwnd == 0 {
        return true;
    }
    let mut r = RECT::default();
    let ok = unsafe { GetWindowRect(HWND(hwnd as *mut _), &mut r).is_ok() };
    if !ok {
        return true;
    }
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

fn point_in_rect(x: i32, y: i32, left: i32, top: i32, width: i32, height: i32) -> bool {
    if width <= 0 || height <= 0 {
        return false;
    }
    let (x, y, left, top, width, height) = (
        i64::from(x),
        i64::from(y),
        i64::from(left),
        i64::from(top),
        i64::from(width),
        i64::from(height),
    );
    x >= left && x < left + width && y >= top && y < top + height
}

/// True when `(x, y)` belongs to the live Windows virtual desktop.
///
/// Unlike a fixed negative-coordinate threshold, this admits every layout the
/// OS actually reports. It also rejects the iconic `(-32000, -32000)` region
/// when a transient `IsIconic` query races with UIA/GetWindowRect state.
pub fn point_on_virtual_desktop(x: i32, y: i32) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN,
    };
    unsafe {
        point_in_rect(
            x,
            y,
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    }
}

/// True when `hwnd` is minimized (iconic).
///
/// Authoritative counterpart to [`is_iconic_sentinel_point`]: the capture path
/// already refuses iconic windows outright (see `capture.rs`), and the input
/// path needs the same signal so an element action can fail loudly instead of
/// posting into the off-screen iconic region.
pub fn window_is_iconic(hwnd: u64) -> bool {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::IsIconic;
    if hwnd == 0 {
        return false;
    }
    unsafe { IsIconic(HWND(hwnd as *mut _)).as_bool() }
}

#[cfg(test)]
mod iconic_sentinel_tests {
    use super::*;

    /// The exact rect Win32 hands back for a minimized window, as documented on
    /// the capture-path guard in `capture.rs`.
    const ICONIC_RECT: (i32, i32, i32, i32) = (-32000, -32000, -31840, -31972);

    #[test]
    fn iconic_rect_passes_the_positive_extent_check_that_guards_the_bounds_path() {
        // This is why #2015 is silent rather than refused: the sentinel rect is
        // a *well-formed* rectangle, so validity checks phrased as
        // "right > left && bottom > top" admit it.
        let (left, top, right, bottom) = ICONIC_RECT;
        assert!(right > left, "sentinel width is positive: {right} > {left}");
        assert!(
            bottom > top,
            "sentinel height is positive: {bottom} > {top}"
        );
    }

    #[test]
    fn virtual_desktop_membership_has_no_magic_negative_ceiling() {
        assert!(point_in_rect(-31920, 50, -40000, 0, 50000, 2000));
    }

    #[test]
    fn iconic_center_is_outside_an_ordinary_virtual_desktop() {
        let (left, top, right, bottom) = ICONIC_RECT;
        let (cx, cy) = ((left + right) / 2, (top + bottom) / 2);
        assert!(!point_in_rect(cx, cy, -2560, -1080, 6400, 3240));
    }

    #[test]
    fn iconic_value_on_either_axis_is_outside_the_desktop() {
        assert!(!point_in_rect(640, -32000, -2560, -1080, 6400, 3240));
        assert!(!point_in_rect(-32000, 480, -2560, -1080, 6400, 3240));
    }

    #[test]
    fn real_negative_origin_monitor_points_remain_valid() {
        for (x, y) in [
            (-1920, 0),
            (-1795, 383),
            (-2560, 0),
            (0, -1080),
            (-1920, -1080),
            (0, 0),
            (1920, 1080),
        ] {
            assert!(point_in_rect(x, y, -2560, -1080, 6400, 3240));
        }
    }

    #[test]
    fn virtual_desktop_edges_and_invalid_extents_fail_closed() {
        assert!(point_in_rect(-1795, 383, -2560, -1080, 6400, 3240));
        assert!(!point_in_rect(-2561, 383, -2560, -1080, 6400, 3240));
        assert!(!point_in_rect(3840, 0, -2560, -1080, 6400, 3240));
        assert!(!point_in_rect(0, 0, 0, 0, 0, 1080));
        assert!(!point_in_rect(0, 0, 0, 0, 1920, -1));
    }
}

#[cfg(test)]
mod cleanup_authorization_tests {
    use super::cleanup_release_is_authorized;

    #[test]
    fn release_requires_possible_down_and_current_recipient_authority() {
        assert!(!cleanup_release_is_authorized(false, false));
        assert!(!cleanup_release_is_authorized(false, true));
        assert!(!cleanup_release_is_authorized(true, false));
        assert!(cleanup_release_is_authorized(true, true));
    }
}

/// One pen press-drag-release from screen `(sx0,sy0)` to `(sx1,sy1)`, with
/// `steps` interpolated in-contact UPDATE points between the down and the up.
/// A single synthetic pen device is created for the whole stroke. The barrel
/// button is held when `barrel` is set (secondary-button drag).
fn pen_drag(
    target: HWND,
    expected_pid: u32,
    sx0: i32,
    sy0: i32,
    sx1: i32,
    sy1: i32,
    steps: usize,
    barrel: bool,
) -> Result<()> {
    unsafe {
        let dev = CreateSyntheticPointerDevice(PT_PEN, 1, POINTER_FEEDBACK_DEFAULT)
            .map_err(|e| anyhow::anyhow!("CreateSyntheticPointerDevice(PEN): {e}"))?;
        let pen_flags = if barrel { PEN_FLAG_BARREL } else { 0 };
        let mk = |flags, x: i32, y: i32| POINTER_TYPE_INFO {
            r#type: PT_PEN,
            Anonymous: POINTER_TYPE_INFO_0 {
                penInfo: POINTER_PEN_INFO {
                    pointerInfo: POINTER_INFO {
                        pointerType: PT_PEN,
                        pointerId: 0,
                        pointerFlags: flags,
                        sourceDevice: HANDLE::default(),
                        hwndTarget: HWND::default(),
                        ptPixelLocation: POINT { x, y },
                        ..Default::default()
                    },
                    penFlags: pen_flags,
                    penMask: 0,
                    pressure: 512,
                    rotation: 0,
                    tiltX: 0,
                    tiltY: 0,
                },
            },
        };
        if !exact_target_visible_at_point(target, expected_pid, sx0, sy0) {
            let _ = DestroySyntheticPointerDevice(dev);
            bail!("exact target ownership or recipient changed before synthetic pen drag down");
        }
        let down = mk(
            POINTER_FLAG_DOWN | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT,
            sx0,
            sy0,
        );
        let mut result = InjectSyntheticPointerInput(dev, &[down]);
        let down_may_have_landed = true;
        let mut last_authorized = (sx0, sy0);
        let steps = steps.max(1);
        for i in 1..=steps {
            if result.is_err() {
                break;
            }
            sleep(Duration::from_millis(8));
            let t = i as f64 / steps as f64;
            let x = sx0 + ((sx1 - sx0) as f64 * t).round() as i32;
            let y = sy0 + ((sy1 - sy0) as f64 * t).round() as i32;
            if !exact_target_visible_at_point(target, expected_pid, x, y) {
                result = Err(windows::core::Error::new(
                    windows::core::HRESULT(0x80004005u32 as i32),
                    "exact target ownership or recipient changed during synthetic pen drag",
                ));
                break;
            }
            let mv = mk(
                POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT,
                x,
                y,
            );
            if let Err(error) = InjectSyntheticPointerInput(dev, &[mv]) {
                result = Err(error);
                break;
            }
            last_authorized = (x, y);
        }
        sleep(Duration::from_millis(8));
        let up = mk(POINTER_FLAG_UP, last_authorized.0, last_authorized.1);
        let cleanup_authorized = exact_target_visible_at_point(
            target,
            expected_pid,
            last_authorized.0,
            last_authorized.1,
        );
        let cleanup = cleanup_release_is_authorized(down_may_have_landed, cleanup_authorized)
            .then(|| InjectSyntheticPointerInput(dev, &[up]));
        let _ = DestroySyntheticPointerDevice(dev);
        match (result, cleanup, cleanup_authorized) {
            (Ok(()), Some(Ok(())), true) => {}
            (Ok(()), None, false) => {
                return Err(anyhow::anyhow!(
                    "synthetic pen drag release was withheld because exact recipient authority was lost; transient device destroyed"
                ));
            }
            (Ok(()), Some(Err(cleanup)), _) => {
                return Err(anyhow::anyhow!(
                    "InjectSyntheticPointerInput(pen drag cleanup): {cleanup}"
                ));
            }
            (Err(primary), Some(Err(cleanup)), _) => {
                return Err(anyhow::anyhow!(
                    "InjectSyntheticPointerInput(pen drag): {primary}; release cleanup also failed: {cleanup}"
                ));
            }
            (Err(primary), None, false) => {
                return Err(anyhow::anyhow!(
                    "InjectSyntheticPointerInput(pen drag): {primary}; release cleanup was withheld because recipient authority was lost; transient device destroyed"
                ));
            }
            (Err(primary), _, _) => {
                return Err(anyhow::anyhow!(
                    "InjectSyntheticPointerInput(pen drag): {primary}"
                ));
            }
            (_, Some(_), false) => {
                unreachable!("unauthorized pen drag cleanup is never injected")
            }
            (_, None, true) => unreachable!("authorized pen cleanup always produces a result"),
        }
    }
    Ok(())
}

/// A **persistent** synthetic touch digitizer, created once and never
/// destroyed. This is load-bearing: a transient (per-stroke) device is gone
/// before WPF's WISP stylus stack can bind to it, so the OS falls back to
/// legacy touch→mouse promotion — which drags the user's cursor to the
/// contact. A *standing* digitizer is enumerated as a real tablet, so WPF (and
/// other stylus/pointer-aware frameworks) consume the contact as touch/stylus
/// and promote it to mouse INTERNALLY, without the OS moving the system cursor.
static TOUCH_DEV: Mutex<isize> = Mutex::new(0);

/// One **touch** press-drag-release from screen `(sx0,sy0)` to `(sx1,sy1)` with
/// `steps` interpolated in-contact moves, via the persistent [`TOUCH_DEV`].
/// Unlike a pen (an absolute *cursor* device — injecting one drags the user's
/// mouse pointer along), a touch contact from a standing digitizer is consumed
/// as touch/stylus and does NOT move the user's cursor. Serialized on the
/// single shared device (one stroke at a time across all sessions).
fn touch_drag(
    target: HWND,
    expected_pid: u32,
    sx0: i32,
    sy0: i32,
    sx1: i32,
    sy1: i32,
    steps: usize,
) -> Result<()> {
    let mut dev_guard = TOUCH_DEV.lock().unwrap_or_else(|e| e.into_inner());
    unsafe {
        // A non-pointer-aware window (WPF) makes the OS promote the PRIMARY touch
        // contact to a mouse event, which drags the system cursor to the contact
        // — and the OS gates delivery on the cursor actually reaching it, so the
        // move can't be prevented from a background process (pinning/clipping the
        // cursor just drops the input). What we CAN do is snap the cursor back to
        // exactly where the user left it the instant the stroke ends, so the net
        // displacement is zero and (with a fast, few-step stroke) the excursion
        // is a brief flick rather than a sustained drag. Pointer-aware targets
        // (Chromium) never promote, so the cursor never moves and this restore is
        // a harmless no-op.
        let cursor_target = crate::win32::capture_current_cursor_target().ok_or_else(|| {
            anyhow::anyhow!(
                "cursor_restore_unavailable: stable pre-touch cursor recipient could not be captured; no touch mutation was attempted"
            )
        })?;
        let dev = if *dev_guard != 0 {
            HSYNTHETICPOINTERDEVICE(*dev_guard as *mut c_void)
        } else {
            let d = CreateSyntheticPointerDevice(PT_TOUCH, 1, POINTER_FEEDBACK_DEFAULT)
                .map_err(|e| anyhow::anyhow!("CreateSyntheticPointerDevice(TOUCH): {e}"))?;
            *dev_guard = d.0 as isize;
            d
        };
        let mk = |flags, x: i32, y: i32| POINTER_TYPE_INFO {
            r#type: PT_TOUCH,
            Anonymous: POINTER_TYPE_INFO_0 {
                touchInfo: POINTER_TOUCH_INFO {
                    pointerInfo: POINTER_INFO {
                        pointerType: PT_TOUCH,
                        pointerId: 0,
                        pointerFlags: flags,
                        sourceDevice: HANDLE::default(),
                        hwndTarget: HWND::default(),
                        ptPixelLocation: POINT { x, y },
                        ..Default::default()
                    },
                    touchFlags: 0,
                    touchMask: 0,
                    rcContact: RECT {
                        left: x - 2,
                        top: y - 2,
                        right: x + 2,
                        bottom: y + 2,
                    },
                    rcContactRaw: RECT {
                        left: x - 2,
                        top: y - 2,
                        right: x + 2,
                        bottom: y + 2,
                    },
                    orientation: 0,
                    pressure: 512,
                },
            },
        };
        // Fast stroke: line-tool canvases only need down→up (the segment is the
        // straight line between them), so a few in-contact frames with a tiny
        // dwell is plenty — and the shorter the stroke, the briefer the cursor
        // excursion before we snap it back.
        let down = mk(
            POINTER_FLAG_DOWN | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT,
            sx0,
            sy0,
        );
        if !exact_target_visible_at_point(target, expected_pid, sx0, sy0) {
            bail!("exact target ownership or recipient changed before synthetic touch drag down");
        }
        let mut res = InjectSyntheticPointerInput(dev, &[down]);
        let down_may_have_landed = true;
        let mut last_authorized = (sx0, sy0);
        let steps = steps.clamp(1, 3);
        for i in 1..=steps {
            if res.is_err() {
                break;
            }
            sleep(Duration::from_millis(2));
            let t = i as f64 / steps as f64;
            let x = sx0 + ((sx1 - sx0) as f64 * t).round() as i32;
            let y = sy0 + ((sy1 - sy0) as f64 * t).round() as i32;
            if !exact_target_visible_at_point(target, expected_pid, x, y) {
                res = Err(windows::core::Error::new(
                    windows::core::HRESULT(0x80004005u32 as i32),
                    "exact target ownership or recipient changed during synthetic touch drag",
                ));
                break;
            }
            let mv = mk(
                POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT,
                x,
                y,
            );
            if let Err(error) = InjectSyntheticPointerInput(dev, &[mv]) {
                res = Err(error);
                break;
            }
            last_authorized = (x, y);
        }
        sleep(Duration::from_millis(2));
        let up = mk(POINTER_FLAG_UP, last_authorized.0, last_authorized.1);
        let cleanup_authorized = exact_target_visible_at_point(
            target,
            expected_pid,
            last_authorized.0,
            last_authorized.1,
        );
        let cleanup = cleanup_release_is_authorized(down_may_have_landed, cleanup_authorized)
            .then(|| InjectSyntheticPointerInput(dev, &[up]));
        let cancel_device = down_may_have_landed
            && (!cleanup_authorized || cleanup.as_ref().is_some_and(Result::is_err));
        let cancel_result = cancel_device.then(|| DestroySyntheticPointerDevice(dev));
        if cancel_device {
            *dev_guard = 0;
        }
        // Snap the cursor back to where the user left it. The OS processes the
        // promoted mouse messages slightly after injection, so a single restore
        // right after the `up` can be overrun by that late move — settle briefly,
        // then restore, and restore once more to win the race. No-op for
        // pointer-aware targets (Chromium) that never moved the cursor.
        let cursor_restored = crate::win32::restore_cursor_target(cursor_target);
        sleep(Duration::from_millis(12));
        let cursor_restored_after_settle = crate::win32::restore_cursor_target(cursor_target);
        // device intentionally NOT destroyed — see TOUCH_DEV.
        match (res, cleanup, cleanup_authorized) {
            (Ok(()), Some(Ok(())), true) => {}
            (Ok(()), None, false) => {
                return Err(anyhow::anyhow!(
                    "synthetic touch drag release was withheld because exact recipient authority was lost; persistent device cancel result: {cancel_result:?}"
                ));
            }
            (Ok(()), Some(Err(cleanup)), _) => {
                return Err(anyhow::anyhow!(
                    "InjectSyntheticPointerInput(touch drag cleanup): {cleanup}; device cancel result: {cancel_result:?}"
                ));
            }
            (Err(primary), Some(Err(cleanup)), _) => {
                return Err(anyhow::anyhow!(
                    "InjectSyntheticPointerInput(touch drag): {primary}; release cleanup also failed: {cleanup}; device cancel result: {cancel_result:?}"
                ));
            }
            (Err(primary), None, false) => {
                return Err(anyhow::anyhow!(
                    "InjectSyntheticPointerInput(touch drag): {primary}; release cleanup was withheld because recipient authority was lost; device cancel result: {cancel_result:?}"
                ));
            }
            (Err(primary), _, _) => {
                return Err(anyhow::anyhow!(
                    "InjectSyntheticPointerInput(touch drag): {primary}"
                ));
            }
            (_, Some(_), false) => {
                unreachable!("unauthorized touch drag cleanup is never injected")
            }
            (_, None, true) => unreachable!("authorized touch cleanup always produces a result"),
        }
        if !cursor_restored || !cursor_restored_after_settle {
            bail!(
                "cursor_restore_failed after synthetic touch drag: stable prior cursor destination was not confirmed"
            );
        }
    }
    Ok(())
}

/// Inject a press-drag-release at **screen** coordinates, routed by the system
/// to whatever window is under the path — without a foreground swap and without
/// moving the user's cursor. This is the background fallback for canvases whose
/// content (Chromium/WPF/GTK) silently drops a PostMessage drag: synthetic-pen
/// input arrives through the system input queue and is accepted directly
/// (Chromium/WPF) or promoted to mouse for legacy Win32. `left` → primary
/// stroke, `right` → barrel-held secondary stroke; the target is held
/// non-activatable + cloaked so any transient raise stays invisible.
pub fn inject_drag_screen(
    target: u64,
    expected_pid: u32,
    sx0: i32,
    sy0: i32,
    sx1: i32,
    sy1: i32,
    steps: usize,
    button: &str,
) -> Result<()> {
    if target == 0 {
        bail!("inject_drag_screen: null target window");
    }
    let target_h = HWND(target as *mut _);
    if crate::win32::capture_foreground_target(target, Some(expected_pid)).is_none() {
        bail!("inject_drag_screen: target HWND changed ownership");
    }
    if let Some(msg) = crate::input::post_message_blocked_by_uipi(target) {
        bail!(msg);
    }
    let barrel = match button {
        "left" => false,
        "right" => true,
        other => bail!("background injection supports left/right buttons only (got {other:?})"),
    };
    // macOS-aligned contract: a `background` drag NEVER fronts/raises. WPF's
    // Wisp stylus stack only PROCESSES injected input while the window is the
    // ACTIVE foreground (verified by RE) — and background is not allowed to
    // force that — so a WPF drag is simply undeliverable in background. Bail so
    // the tool returns background_unavailable (escalate to foreground).
    if crate::input::delivery::is_wpf_target_window(target) {
        bail!(
            "background drag is not deliverable to a WPF target — its Wisp stylus \
             stack only processes injected input while the window is foreground, \
             which background must not force. Escalate to delivery_mode:\"foreground\"."
        );
    }
    // Coordinate-routed touch/pen lands on the topmost VISIBLE window at the
    // start point. If the target is occluded there, bail rather than raise it.
    if !exact_target_visible_at_point(target_h, expected_pid, sx0, sy0) {
        bail!(
            "background drag cannot reach this target at the start point ({sx0},{sy0}) \
             — it is occluded by another window. Escalate to delivery_mode:\"foreground\"."
        );
    }
    let prev_fg_target = match crate::win32::capture_current_foreground_target() {
        Some(previous) if previous.hwnd() != target => Some(previous),
        Some(_) => None,
        None => {
            bail!(
                "foreground_restore_unavailable: a stable prior foreground identity could not be captured; no background drag mutation was attempted"
            )
        }
    };
    let displaced = crate::win32::capture_foreground_target(target, Some(expected_pid))
        .ok_or_else(|| anyhow::anyhow!("inject_drag_screen: target ownership changed"))?;
    // Left drag → touch contact (coordinate-routed). Right/barrel drag has no
    // touch equivalent, so fall back to a pen (rare).
    let stroke = |()| {
        if barrel {
            pen_drag(target_h, expected_pid, sx0, sy0, sx1, sy1, steps, true)
        } else {
            touch_drag(target_h, expected_pid, sx0, sy0, sx1, sy1, steps)
        }
    };
    // Chromium/GTK: pointer-aware, process injection in the background — hold
    // non-activatable (no raise), inject, then re-assert the user's foreground.
    let mut noact = NoActivateGuard::arm_for_pid(target_h, expected_pid)?;
    let mut result = stroke(());
    if let Err(restoration) = noact.finish() {
        result = Err(match result {
            Ok(()) => restoration,
            Err(primary) => anyhow::anyhow!("{primary}; cleanup failure: {restoration}"),
        });
    }
    if let Some(previous) = prev_fg_target {
        let restored = crate::win32::restore_foreground_target_if_still_displaced(
            previous,
            displaced,
            Duration::from_millis(500),
        );
        if restored == crate::win32::ForegroundRestoreOutcome::Failed {
            let restore_error = anyhow::anyhow!("foreground_restore_failed after background drag");
            result = Err(match result {
                Ok(_) => restore_error,
                Err(error) => anyhow::anyhow!("{error}; cleanup failure: {restore_error}"),
            });
        }
    }
    result
}

// (Removed inject_key_cloaked / inject_text_cloaked.) The macOS-aligned
// contract forbids a `background` actuation from grabbing focus — even a
// *cloaked* (hidden) one. Keyboard/text that the target's input stack would
// drop in the background (VCL/classic-Win32 accelerators, Chromium key-combos,
// WPF/terminal text) is now reported as `background_unavailable`; the agent
// escalates to `delivery_mode:"foreground"`, which uses the explicit
// SetForegroundWindow path (send_key_synthesized / send_text_synthesized).
