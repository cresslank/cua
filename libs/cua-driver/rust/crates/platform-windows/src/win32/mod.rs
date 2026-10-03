//! Win32 API wrappers for window/process enumeration.

pub mod apps;
pub mod installed_apps;
pub mod windows;

pub use apps::{list_descendants, list_processes, related_processes, ProcessInfo};
pub use installed_apps::{list_installed_apps, InstalledApp};
// Upstream PR #4505 lookup helpers alongside the fork's identity-safe restoration.
pub(crate) use windows::{
    capture_current_cursor_target, capture_current_foreground_target, capture_foreground_target,
    capture_point_target, find_window_by_pid_and_handle, foreground_matches_target_or_owned_window,
    list_windows_via_win32, list_windows_win32_first, lookup_window_for_pid, resolve_uwp_app_pid,
    restore_cursor_target, restore_foreground_target_if_still_displaced, window_owner_pid,
    ForegroundRestoreOutcome, ForegroundTarget, PidWindowLookup,
};
pub use windows::{list_windows, resolve_uwp_host_window, WindowInfo};
