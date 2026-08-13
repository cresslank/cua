//! Foreground-steal bypass for UWP / XAML / WinUI UIA activation calls.
//!
//! UWP/XAML/WinUI apps self-foreground during UIA `InvokePattern.Invoke`
//! (and `ExpandCollapse.Expand`, `Toggle.Toggle`, `SelectionItem.Select` —
//! anything the XAML message-loop processes as an input-like event). The
//! XAML host unconditionally calls `SetForegroundWindow(self)` when handling
//! those, stealing focus from whatever the user had on top.
//!
//! Wrapping the call in `EnableWindow(host, FALSE) / call / EnableWindow(host, TRUE)`
//! silently suppresses the self-activation while letting the UIA pattern call
//! still execute — UIA pattern delivery uses the kernel accessibility channel,
//! not the input queue gated by `EnableWindow`.
//!
//! Empirical evidence (`flash-repro/14-multi-uwp-v3.ps1`, 2026-05-24):
//!   - Baseline UIA Invoke against UWP Calculator num5 button: user's
//!     foreground window dropped below the target in 91% of poller samples.
//!   - With this bypass: 0/507 z-drops across Calculator, Clock, Settings.
//!
//! Chromium / Electron hosts (`Chrome_WidgetWin_*`) exhibit the identical
//! self-foreground during UIA `Invoke` and are covered by the same shield (the
//! gate also accepts `is_chromium_target_window`). On Windows their content
//! window calls `SetForegroundWindow(self)` from the Invoke handler — measured
//! 7/8 background ax-bg actions stole focus before this; the `EnableWindow`
//! shield blocks it because a disabled top-level cannot become foreground while
//! the Invoke still lands over the a11y channel.
//!
//! Non-UWP / non-Chromium classic Win32 apps don't exhibit the bug
//! (`flash-repro/15-non-uwp.ps1`, Notepad: 0/45 baseline z-drops). The bypass
//! is therefore gated on `is_xaml_host_hwnd || is_chromium_target_window` and is
//! a no-op for other hosts.

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, IsWindowEnabled};
use windows::Win32::UI::WindowsAndMessaging::{GetAncestor, GA_ROOT};

/// RAII guard that disables a window on construction and restores its
/// previous enabled-state on Drop. Always re-arms on Drop even if the
/// wrapped action panics.
pub struct DisabledHwndGuard {
    hwnd: HWND,
    was_enabled: bool,
    armed: bool,
    expected_pid: Option<u32>,
}

impl DisabledHwndGuard {
    /// Disable `hwnd` for the lifetime of the guard. No-op for null HWND.
    pub fn disable(hwnd: HWND) -> Self {
        if hwnd.0.is_null() {
            return Self {
                hwnd,
                was_enabled: false,
                armed: false,
                expected_pid: None,
            };
        }
        // `EnableWindow` returns nonzero iff the window was *previously
        // disabled* — invert to get the "was enabled" state we want to
        // restore at Drop time.
        let was_disabled = unsafe { EnableWindow(hwnd, false).as_bool() };
        Self {
            hwnd,
            was_enabled: !was_disabled,
            armed: true,
            expected_pid: None,
        }
    }

    fn disable_for_pid(hwnd: HWND, expected_pid: u32) -> anyhow::Result<Self> {
        if hwnd.0.is_null()
            || crate::win32::window_owner_pid(hwnd.0 as usize as u64) != Some(expected_pid)
        {
            anyhow::bail!("foreground-bypass host changed ownership before disable");
        }
        let was_disabled = unsafe { EnableWindow(hwnd, false).as_bool() };
        if crate::win32::window_owner_pid(hwnd.0 as usize as u64) != Some(expected_pid) {
            anyhow::bail!("foreground-bypass host changed ownership at enabled-state mutation");
        }
        if unsafe { IsWindowEnabled(hwnd).as_bool() } {
            anyhow::bail!("foreground-bypass enabled-state mutation did not read back");
        }
        Ok(Self {
            hwnd,
            was_enabled: !was_disabled,
            armed: true,
            expected_pid: Some(expected_pid),
        })
    }

    fn finish(mut self) -> anyhow::Result<()> {
        if !self.armed {
            return Ok(());
        }
        if let Some(expected_pid) = self.expected_pid {
            if crate::win32::window_owner_pid(self.hwnd.0 as usize as u64) != Some(expected_pid) {
                self.armed = false;
                anyhow::bail!(
                    "foreground-bypass host changed ownership; enabled-state restoration refused"
                );
            }
        }
        unsafe {
            let _ = EnableWindow(self.hwnd, self.was_enabled);
        }
        self.armed = false;
        if unsafe { IsWindowEnabled(self.hwnd).as_bool() } != self.was_enabled {
            anyhow::bail!("foreground-bypass enabled-state restoration did not read back");
        }
        Ok(())
    }
}

impl Drop for DisabledHwndGuard {
    fn drop(&mut self) {
        if self.armed {
            let owner_matches = self.expected_pid.is_none_or(|expected_pid| {
                crate::win32::window_owner_pid(self.hwnd.0 as usize as u64) == Some(expected_pid)
            });
            if owner_matches {
                unsafe {
                    let _ = EnableWindow(self.hwnd, self.was_enabled);
                }
            }
        }
    }
}

/// Wrap an activation closure (Invoke / Expand / Toggle / SelectionItem.Select)
/// in a UWP foreground-steal bypass.
///
/// `host_hwnd` is the top-level HWND of the window that contains the target
/// UIA element. When it identifies as an XAML host (`is_xaml_host_hwnd`), the
/// HWND is disabled for the duration of `action`. For non-XAML / classic
/// Win32 hosts the closure runs unmodified — empirically those don't
/// self-foreground via the input-queue path that EnableWindow gates.
///
/// **Known limitation, WPF Buttons / TextBoxes:** WPF's automation peers
/// call `UIElement.Focus()` synchronously during the UIA Invoke /
/// ValuePattern.SetValue handler. `Focus()` routes through
/// `SetForegroundWindow`, which is NOT gated by EnableWindow. The bypass
/// has no effect there, and the daemon WILL transiently steal foreground
/// from the user. Restoring foreground from a non-UIAccess process is
/// blocked by the foreground-lock, so the only mitigation is to spawn
/// cua-driver-uia.exe (UIAccess-manifested worker) and route UIA
/// activations through it. See PR #1699 bg-modality tests for the
/// regression guards.
pub fn run_with_uwp_bypass<T>(host_hwnd: isize, action: impl FnOnce() -> T) -> T {
    let _guard = make_guard(host_hwnd);
    action()
}

pub fn run_with_uwp_bypass_for_pid<T, E>(
    host_hwnd: isize,
    expected_pid: u32,
    action: impl FnOnce() -> Result<T, E>,
) -> anyhow::Result<T>
where
    E: std::fmt::Display,
{
    if host_hwnd == 0
        || crate::win32::window_owner_pid(host_hwnd as usize as u64) != Some(expected_pid)
    {
        anyhow::bail!("foreground-bypass host changed ownership before UIA action");
    }
    let mut noactivate =
        crate::input::NoActivateGuard::arm_for_pid(HWND(host_hwnd as *mut _), expected_pid)?;
    let guard = match make_guard_for_pid(host_hwnd, expected_pid) {
        Ok(guard) => guard,
        Err(setup) => {
            return match noactivate.finish() {
                Ok(()) => Err(setup),
                Err(cleanup) => Err(anyhow::anyhow!(
                    "{setup}; UIA shield setup cleanup: {cleanup}"
                )),
            };
        }
    };
    if crate::win32::window_owner_pid(host_hwnd as usize as u64) != Some(expected_pid) {
        let enabled_restore = match guard {
            Some(guard) => guard.finish(),
            None => Ok(()),
        };
        let noactivate_restore = noactivate.finish();
        let cleanup = match (enabled_restore, noactivate_restore) {
            (Ok(()), Ok(())) => String::new(),
            (Err(enabled), Ok(())) => format!("; enabled-state cleanup: {enabled}"),
            (Ok(()), Err(noactivate)) => format!("; no-activate cleanup: {noactivate}"),
            (Err(enabled), Err(noactivate)) => {
                format!("; enabled-state cleanup: {enabled}; no-activate cleanup: {noactivate}")
            }
        };
        anyhow::bail!(
            "foreground-bypass host changed ownership immediately before UIA action{cleanup}"
        );
    }
    let action_result = action().map_err(|error| anyhow::anyhow!(error.to_string()));
    let enabled_restore = match guard {
        Some(guard) => guard.finish(),
        None => Ok(()),
    };
    let noactivate_restore = noactivate.finish();
    let cleanup_result = match (enabled_restore, noactivate_restore) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(enabled), Ok(())) => Err(enabled),
        (Ok(()), Err(noactivate)) => Err(noactivate),
        (Err(enabled), Err(noactivate)) => Err(anyhow::anyhow!(
            "enabled-state restoration: {enabled}; no-activate restoration: {noactivate}"
        )),
    };
    match (action_result, cleanup_result) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(action), Ok(())) => Err(action),
        (Ok(_), Err(cleanup)) => Err(cleanup),
        (Err(action), Err(cleanup)) => {
            Err(anyhow::anyhow!("{action}; UIA host cleanup: {cleanup}"))
        }
    }
}

fn make_guard(host_hwnd: isize) -> Option<DisabledHwndGuard> {
    if host_hwnd == 0 {
        return None;
    }
    // XAML/UWP/WinUI hosts self-foreground during UIA pattern handling (the
    // original case). Chromium/Electron hosts (`Chrome_WidgetWin_*`) exhibit the
    // SAME bug: their UIA `InvokePattern.Invoke` handler reaches the browser's
    // focus path and calls `SetForegroundWindow(self)`, stealing focus from the
    // user's window on a *background* click (measured 7/8 ax-bg actions stole on
    // Windows — Chromium-specific; macOS WKWebView / Linux Electron hold).
    // `WS_EX_NOACTIVATE` (the injection path's `NoActivateGuard`) does NOT stop
    // an explicit self-`SetForegroundWindow`, but the `EnableWindow` shield does:
    // a *disabled* top-level cannot be made the foreground window, while the UIA
    // Invoke still lands (it's delivered over the kernel accessibility channel,
    // not the input queue `EnableWindow` gates). Same mechanism, same 0-z-drop
    // result as UWP — so gate the shield on Chromium too.
    let shielded = crate::input::is_xaml_host_hwnd(host_hwnd as u64)
        || crate::input::is_chromium_target_window(host_hwnd as u64);
    if !shielded {
        return None;
    }
    let h = HWND(host_hwnd as *mut _);
    // Defensive: walk up to the root in case the caller handed us a child
    // HWND inside the XAML host's HWND tree. UWP elements always disable
    // cleanly via the AppFrame root.
    let root = unsafe { GetAncestor(h, GA_ROOT) };
    let target = if root.0.is_null() { h } else { root };
    Some(DisabledHwndGuard::disable(target))
}

fn make_guard_for_pid(
    host_hwnd: isize,
    expected_pid: u32,
) -> anyhow::Result<Option<DisabledHwndGuard>> {
    let shielded = crate::input::is_xaml_host_hwnd(host_hwnd as u64)
        || crate::input::is_chromium_target_window(host_hwnd as u64);
    if !shielded {
        return Ok(None);
    }
    let h = HWND(host_hwnd as *mut _);
    let root = unsafe { GetAncestor(h, GA_ROOT) };
    let target = if root.0.is_null() { h } else { root };
    DisabledHwndGuard::disable_for_pid(target, expected_pid).map(Some)
}
