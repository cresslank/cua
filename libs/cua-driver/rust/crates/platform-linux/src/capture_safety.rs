//! Admission shared by Linux screenshot producers, before native requests.

pub(crate) fn dimensions(width: f64, height: f64) -> anyhow::Result<()> {
    if !width.is_finite() || !height.is_finite() || width < 1.0 || height < 1.0 {
        anyhow::bail!("capture_geometry_invalid: screenshot dimensions must be finite and at least 1x1 (got {width}x{height})");
    }
    Ok(())
}

/// WinRects deliberately exposes only exact-target pixels. Do not recover a
/// missing desktop method by asking a portal (or grim/XWayland) to capture it.
/// Capture policy is independent of the experimental backend preference.
/// Explicit Xorg remains X11 even if an inherited Wayland variable is present.
pub(crate) fn gnome_session() -> bool {
    let session = std::env::var("XDG_SESSION_TYPE").unwrap_or_default();
    if session.eq_ignore_ascii_case("x11") || crate::wayland::is_inject_mode() {
        return false;
    }
    (session.eq_ignore_ascii_case("wayland") || std::env::var_os("WAYLAND_DISPLAY").is_some())
        && (std::env::var("XDG_CURRENT_DESKTOP")
            .ok()
            .is_some_and(|desktop| {
                desktop
                    .split(':')
                    .any(|part| part.eq_ignore_ascii_case("gnome"))
            })
            || crate::wayland::shell_helper::present())
}

pub(crate) fn refuse_gnome_display() -> anyhow::Result<()> {
    if gnome_session() {
        anyhow::bail!("capture_scope_unsupported: GNOME whole-screen capture is unavailable; use get_window_state with an exact pid and window_id through WinRects; grim, portal and XWayland fallbacks are disabled");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn screenshot_dimensions_must_be_finite_and_nonzero() {
        for (width, height) in [(1.0, 1.0), (1920.0, 1080.0), (3840.0, 2160.0)] {
            super::dimensions(width, height).unwrap();
        }
        for invalid in [0.0, -1.0, 0.5, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            for (width, height) in [(invalid, 1080.0), (1920.0, invalid)] {
                assert!(super::dimensions(width, height)
                    .unwrap_err()
                    .to_string()
                    .starts_with("capture_geometry_invalid:"));
            }
        }
    }
}
