//! Host-chosen bound for the post-action window-change observation.
//!
//! Long-lived macOS hosts (`serve`, direct MCP, SDK runtime) take one immediate
//! observation and retain focus protection in the background after returning.
//! Windows appearing later are not reported in that action result; inspect the
//! next screenshot or `list_windows`. Finite macOS hosts and Linux X11 foreground
//! transactions poll synchronously. Trusted host environment settings:
//!
//! - [`WINDOW_CHANGE_TIMEOUT_ENV`] (`CUA_DRIVER_WINDOW_CHANGE_TIMEOUT_MS`):
//!   post-action protection duration on long-lived macOS hosts, or synchronous
//!   observation deadline, in milliseconds. `0` skips observation and releases
//!   the macOS wildcard lease immediately.
//! - [`WINDOW_CHANGE_POLL_ENV`] (`CUA_DRIVER_WINDOW_CHANGE_POLL_MS`): interval
//!   between window-set reads in synchronous mode only, in milliseconds.
//!
//! Unset, empty, or unparsable values keep each adapter's default. Values are
//! read only from the host environment; no tool argument can change them.
//! See `Skills/cua-driver/EMBEDDING.md` for platform tradeoffs.

use std::time::Duration;

/// Post-action protection duration or synchronous observation deadline, in milliseconds.
pub const WINDOW_CHANGE_TIMEOUT_ENV: &str = "CUA_DRIVER_WINDOW_CHANGE_TIMEOUT_MS";

/// Interval between synchronous window-set reads, in milliseconds.
pub const WINDOW_CHANGE_POLL_ENV: &str = "CUA_DRIVER_WINDOW_CHANGE_POLL_MS";

/// Largest accepted deadline. Larger values are clamped so a typo cannot make
/// every action wait for minutes.
pub const MAX_WINDOW_CHANGE_TIMEOUT: Duration = Duration::from_millis(10_000);

/// Smallest accepted poll interval. Smaller values (including `0`) are raised
/// so the observation never spins.
pub const MIN_WINDOW_CHANGE_POLL: Duration = Duration::from_millis(5);

/// Largest accepted poll interval.
pub const MAX_WINDOW_CHANGE_POLL: Duration = Duration::from_millis(1_000);

/// Resolved observation bounds for one action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowObservationBounds {
    /// Protection duration or synchronous observation deadline. Zero skips observation.
    pub timeout: Duration,
    /// Interval between reads. At least [`MIN_WINDOW_CHANGE_POLL`], and never
    /// longer than a non-zero `timeout` that is itself above that minimum, so
    /// the observation cannot overshoot a short deadline by a whole poll.
    pub poll: Duration,
}

impl WindowObservationBounds {
    /// True when the host asked to skip the observation entirely.
    pub fn skips_observation(&self) -> bool {
        self.timeout.is_zero()
    }

    /// Resolve bounds from raw environment values. Pure, so it can be tested
    /// without touching the process environment.
    pub fn from_raw(
        timeout_raw: Option<&str>,
        poll_raw: Option<&str>,
        default_timeout: Duration,
        default_poll: Duration,
    ) -> Self {
        let timeout = parse_millis(timeout_raw)
            .unwrap_or(default_timeout)
            .min(MAX_WINDOW_CHANGE_TIMEOUT);
        let poll = parse_millis(poll_raw)
            .unwrap_or(default_poll)
            .min(timeout.max(MIN_WINDOW_CHANGE_POLL))
            .clamp(MIN_WINDOW_CHANGE_POLL, MAX_WINDOW_CHANGE_POLL);
        Self { timeout, poll }
    }

    /// Resolve bounds from the daemon environment, falling back to the
    /// adapter's own defaults for anything unset or unparsable.
    pub fn from_env(default_timeout: Duration, default_poll: Duration) -> Self {
        let timeout = std::env::var(WINDOW_CHANGE_TIMEOUT_ENV).ok();
        let poll = std::env::var(WINDOW_CHANGE_POLL_ENV).ok();
        Self::from_raw(
            timeout.as_deref(),
            poll.as_deref(),
            default_timeout,
            default_poll,
        )
    }
}

/// Parse a non-negative whole number of milliseconds. Anything else (empty,
/// negative, fractional, non-numeric, or overflowing `u64`) is `None`.
fn parse_millis(raw: Option<&str>) -> Option<Duration> {
    raw?.trim().parse::<u64>().ok().map(Duration::from_millis)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULT_TIMEOUT: Duration = Duration::from_millis(1000);
    const DEFAULT_POLL: Duration = Duration::from_millis(50);

    fn bounds(timeout: Option<&str>, poll: Option<&str>) -> WindowObservationBounds {
        WindowObservationBounds::from_raw(timeout, poll, DEFAULT_TIMEOUT, DEFAULT_POLL)
    }

    #[test]
    fn unset_keeps_adapter_defaults() {
        let b = bounds(None, None);
        assert_eq!(b.timeout, DEFAULT_TIMEOUT);
        assert_eq!(b.poll, DEFAULT_POLL);
        assert!(!b.skips_observation());
    }

    #[test]
    fn valid_values_are_honored() {
        let b = bounds(Some("200"), Some("20"));
        assert_eq!(b.timeout, Duration::from_millis(200));
        assert_eq!(b.poll, Duration::from_millis(20));
        let padded = bounds(Some(" 300 "), Some("\t25\n"));
        assert_eq!(padded.timeout, Duration::from_millis(300));
        assert_eq!(padded.poll, Duration::from_millis(25));
    }

    #[test]
    fn zero_timeout_skips_observation() {
        let b = bounds(Some("0"), None);
        assert_eq!(b.timeout, Duration::ZERO);
        assert!(b.skips_observation());
        // The poll interval stays non-zero even though it is unused.
        assert_eq!(b.poll, MIN_WINDOW_CHANGE_POLL);
    }

    #[test]
    fn invalid_values_fall_back_to_defaults() {
        for raw in [
            "",
            "  ",
            "abc",
            "-1",
            "1.5",
            "100ms",
            "18446744073709551616",
        ] {
            let b = bounds(Some(raw), Some(raw));
            assert_eq!(b.timeout, DEFAULT_TIMEOUT, "timeout for {raw:?}");
            assert_eq!(b.poll, DEFAULT_POLL, "poll for {raw:?}");
        }
    }

    #[test]
    fn too_large_values_are_clamped() {
        let b = bounds(Some("600000"), Some("5000"));
        assert_eq!(b.timeout, MAX_WINDOW_CHANGE_TIMEOUT);
        assert_eq!(b.poll, MAX_WINDOW_CHANGE_POLL);
        let max = bounds(Some(&u64::MAX.to_string()), None);
        assert_eq!(max.timeout, MAX_WINDOW_CHANGE_TIMEOUT);
    }

    #[test]
    fn zero_or_tiny_poll_is_raised_to_the_minimum() {
        assert_eq!(bounds(None, Some("0")).poll, MIN_WINDOW_CHANGE_POLL);
        assert_eq!(bounds(None, Some("1")).poll, MIN_WINDOW_CHANGE_POLL);
    }

    #[test]
    fn poll_never_exceeds_a_short_timeout() {
        let b = bounds(Some("30"), Some("200"));
        assert_eq!(b.timeout, Duration::from_millis(30));
        assert_eq!(b.poll, Duration::from_millis(30));
        // The adapter default poll is capped the same way.
        assert_eq!(bounds(Some("10"), None).poll, Duration::from_millis(10));
        // A timeout below the poll floor still polls at the floor.
        assert_eq!(bounds(Some("2"), None).poll, MIN_WINDOW_CHANGE_POLL);
    }

    #[test]
    fn adapter_defaults_pass_through_per_platform() {
        let linux = WindowObservationBounds::from_raw(
            None,
            None,
            Duration::from_millis(800),
            Duration::from_millis(50),
        );
        assert_eq!(linux.timeout, Duration::from_millis(800));
        assert_eq!(linux.poll, Duration::from_millis(50));
    }
}
