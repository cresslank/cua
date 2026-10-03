//! Adapted from upstream PR #3490 by hyprcat; this fork retains the requested
//! `CUA_DRIVER_RS_*` launch names.
//!
//! Host-chosen input pacing and AX read-back bounds (macOS only).
//!
//! The macOS input adapter waits fixed intervals around each synthesized
//! click; text input also has WebKit focus settling and a default per-character
//! delay. An embedding host
//! whose targets accept faster input can shorten them at trusted launch
//! through the daemon environment:
//!
//! - [`MOUSE_PRIMER`] (`CUA_DRIVER_RS_MOUSE_PRIMER_MS`): settle after the
//!   mouseMoved primer that precedes a click, drag, or scroll.
//! - [`CLICK_GAP`] (`CUA_DRIVER_RS_CLICK_GAP_MS`): mouse down→up gap.
//! - [`MULTI_CLICK_GAP`] (`CUA_DRIVER_RS_MULTI_CLICK_GAP_MS`): gap between the
//!   down/up pairs of a double or triple click.
//! - [`WEBKIT_SETTLE`] (`CUA_DRIVER_RS_WEBKIT_SETTLE_MS`): wait after an AX press
//!   on a WebKit text input. `0` skips it.
//! - [`TYPE_TEXT_DELAY`] (`CUA_DRIVER_RS_TYPE_TEXT_DELAY_MS`): `type_text`'s
//!   default `delay_ms` when the caller omits it.
//! - [`AX_READBACK_TIMEOUT`] (`CUA_DRIVER_RS_AX_READBACK_TIMEOUT_MS`): bound for
//!   asynchronous AX insertion read-back. `0` disables waiting, retaining a single
//!   immediate read capped at 5ms.
//!
//! Unset, empty, or unparsable values keep each adapter default, so a daemon
//! launched without these variables behaves exactly as before. Values outside
//! a knob's range are clamped. They are read from the daemon process
//! environment only; no tool argument can change them. Linux and Windows keep
//! their own fixed pacing and do not read them. See
//! `Skills/cua-driver/EMBEDDING.md` for what a shorter value costs.

use std::time::Duration;

/// One launch-time pacing variable and its accepted range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacingKnob {
    /// Environment variable name.
    pub env: &'static str,
    /// Smallest accepted value; smaller values are raised.
    pub min: Duration,
    /// Largest accepted value; larger values are clamped.
    pub max: Duration,
}

const fn knob(env: &'static str, min_ms: u64, max_ms: u64) -> PacingKnob {
    PacingKnob {
        env,
        min: Duration::from_millis(min_ms),
        max: Duration::from_millis(max_ms),
    }
}

/// Settle after the mouseMoved primer. macOS default 12 ms.
pub const MOUSE_PRIMER: PacingKnob = knob("CUA_DRIVER_RS_MOUSE_PRIMER_MS", 2, 100);
/// Mouse down→up gap within one click. macOS default 28 ms.
pub const CLICK_GAP: PacingKnob = knob("CUA_DRIVER_RS_CLICK_GAP_MS", 2, 200);
/// Gap between the pairs of a multi-click. macOS default 80 ms. The ceiling
/// stays well under the system double-click interval so pairs still coalesce.
pub const MULTI_CLICK_GAP: PacingKnob = knob("CUA_DRIVER_RS_MULTI_CLICK_GAP_MS", 20, 300);
/// WebKit focus settle after an AX press on a text input. macOS default 800 ms.
pub const WEBKIT_SETTLE: PacingKnob = knob("CUA_DRIVER_RS_WEBKIT_SETTLE_MS", 0, 2_000);
/// `type_text` default `delay_ms`. macOS default 30 ms. The ceiling matches
/// the tool schema's `delay_ms` maximum.
pub const TYPE_TEXT_DELAY: PacingKnob = knob("CUA_DRIVER_RS_TYPE_TEXT_DELAY_MS", 0, 200);

/// Background AX insertion read-back bound. macOS default 250 ms.
pub const AX_READBACK_TIMEOUT: PacingKnob = knob("CUA_DRIVER_RS_AX_READBACK_TIMEOUT_MS", 0, 2_000);

/// Every knob, for allowlists and docs.
pub const ALL: [PacingKnob; 6] = [
    MOUSE_PRIMER,
    CLICK_GAP,
    MULTI_CLICK_GAP,
    WEBKIT_SETTLE,
    TYPE_TEXT_DELAY,
    AX_READBACK_TIMEOUT,
];

impl PacingKnob {
    /// Resolve from a raw environment value. Pure, so it can be tested
    /// without touching the process environment.
    pub fn from_raw(&self, raw: Option<&str>, default: Duration) -> Duration {
        raw.and_then(|r| r.trim().parse::<u64>().ok())
            .map(Duration::from_millis)
            .unwrap_or(default)
            .clamp(self.min, self.max)
    }

    /// Resolve from the daemon environment, falling back to the adapter's
    /// default when unset or unparsable.
    pub fn from_env(&self, default: Duration) -> Duration {
        self.from_raw(std::env::var(self.env).ok().as_deref(), default)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
    }

    #[test]
    fn unset_keeps_adapter_defaults() {
        for (k, d) in [
            (MOUSE_PRIMER, 12),
            (CLICK_GAP, 28),
            (MULTI_CLICK_GAP, 80),
            (WEBKIT_SETTLE, 800),
            (TYPE_TEXT_DELAY, 30),
            (AX_READBACK_TIMEOUT, 250),
        ] {
            assert_eq!(k.from_raw(None, ms(d)), ms(d), "{}", k.env);
        }
    }

    #[test]
    fn valid_values_are_honored() {
        assert_eq!(AX_READBACK_TIMEOUT.from_raw(Some("0"), ms(250)), ms(0));
        assert_eq!(
            AX_READBACK_TIMEOUT.from_raw(Some(" 125 "), ms(250)),
            ms(125)
        );
        assert_eq!(CLICK_GAP.from_raw(Some("4"), ms(28)), ms(4));
        assert_eq!(MULTI_CLICK_GAP.from_raw(Some(" 40\n"), ms(80)), ms(40));
        assert_eq!(WEBKIT_SETTLE.from_raw(Some("0"), ms(800)), ms(0));
        assert_eq!(TYPE_TEXT_DELAY.from_raw(Some("0"), ms(30)), ms(0));
    }

    #[test]
    fn invalid_values_fall_back_to_defaults() {
        for raw in ["", "  ", "abc", "-1", "1.5", "8ms", "18446744073709551616"] {
            for k in ALL {
                assert_eq!(k.from_raw(Some(raw), ms(50)), ms(50), "{} {raw:?}", k.env);
            }
        }
    }

    #[test]
    fn out_of_range_values_are_clamped() {
        for k in ALL {
            assert_eq!(k.from_raw(Some("0"), ms(50)), k.min, "{}", k.env);
            assert_eq!(k.from_raw(Some("600000"), ms(50)), k.max, "{}", k.env);
            assert_eq!(k.from_raw(Some(&u64::MAX.to_string()), ms(50)), k.max);
        }
    }

    #[test]
    fn names_follow_the_embedding_convention() {
        // This fork's launch contract explicitly retains the original RS names.
        assert_eq!(
            ALL.map(|k| k.env),
            [
                "CUA_DRIVER_RS_MOUSE_PRIMER_MS",
                "CUA_DRIVER_RS_CLICK_GAP_MS",
                "CUA_DRIVER_RS_MULTI_CLICK_GAP_MS",
                "CUA_DRIVER_RS_WEBKIT_SETTLE_MS",
                "CUA_DRIVER_RS_TYPE_TEXT_DELAY_MS",
                "CUA_DRIVER_RS_AX_READBACK_TIMEOUT_MS",
            ]
        );
    }
}
