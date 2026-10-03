//! Bounded AX insertion observation, independent of a native run loop or clock.
//!
//! Gecko updates AXValue asynchronously after an AXSelectedText write. Waiting
//! for that update avoids synthesizing the same payload a second time.

use cua_driver_contract::AxReadback;
use std::time::Duration;

/// Backstop cadence only; notifications wake the observer immediately.
pub const READBACK_SLICE: Duration = Duration::from_millis(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadbackEvent {
    Notified,
    Tick,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AxReadbackMeasurement {
    pub elapsed_ms: u64,
    pub outcome: AxReadback,
}

impl Default for AxReadbackMeasurement {
    fn default() -> Self {
        Self {
            elapsed_ms: 0,
            outcome: AxReadback::Skipped,
        }
    }
}

/// `elapsed` starts when the write returns. `wait` must wait no longer than its
/// argument, returning early on a notification. `settled` accepts both complete
/// and partial delivery: either must stop the ladder before any replay.
pub fn await_readback<T>(
    write_succeeded: bool,
    timeout: Duration,
    mut read: impl FnMut() -> T,
    settled: impl Fn(&T) -> bool,
    elapsed: impl Fn() -> Duration,
    mut wait: impl FnMut(Duration) -> ReadbackEvent,
) -> (Option<T>, AxReadbackMeasurement) {
    if !write_succeeded {
        return (None, AxReadbackMeasurement::default());
    }
    let mut value = read();
    let mut cause = AxReadback::Immediate;
    loop {
        let duration = elapsed();
        let outcome = if settled(&value) {
            Some(cause)
        } else if timeout.is_zero() {
            Some(AxReadback::Skipped)
        } else if duration >= timeout {
            Some(AxReadback::TimedOut)
        } else {
            None
        };
        if let Some(outcome) = outcome {
            return (
                Some(value),
                AxReadbackMeasurement {
                    elapsed_ms: duration.as_millis().min(u64::MAX as u128) as u64,
                    outcome,
                },
            );
        }
        cause = match wait(READBACK_SLICE.min(timeout.saturating_sub(duration))) {
            ReadbackEvent::Notified => AxReadback::Notification,
            ReadbackEvent::Tick => AxReadback::Poll,
        };
        value = read();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Progress {
        Complete,
        Partial,
        Unchanged,
        Unreadable,
    }

    fn run(value: Progress, event: ReadbackEvent) -> (Option<Progress>, AxReadbackMeasurement) {
        let clock = Cell::new(Duration::ZERO);
        let mut reads = 0;
        let result = await_readback(
            true,
            Duration::from_millis(12),
            || {
                reads += 1;
                if clock.get().is_zero() {
                    Progress::Unchanged
                } else {
                    value
                }
            },
            |p| matches!(p, Progress::Complete | Progress::Partial),
            || clock.get(),
            |slice| {
                assert!(slice <= READBACK_SLICE);
                clock.set(
                    clock.get()
                        + if event == ReadbackEvent::Notified {
                            Duration::from_millis(2)
                        } else {
                            slice
                        },
                );
                event
            },
        );
        if matches!(value, Progress::Complete | Progress::Partial) {
            assert_eq!(
                reads, 2,
                "settled delivery must never reach another rung/read"
            );
        }
        result
    }

    #[test]
    fn notification_settles_before_the_poll_slice() {
        assert_eq!(
            run(Progress::Complete, ReadbackEvent::Notified),
            (
                Some(Progress::Complete),
                AxReadbackMeasurement {
                    elapsed_ms: 2,
                    outcome: AxReadback::Notification
                }
            )
        );
    }

    #[test]
    fn backstop_tick_settles_without_notifications() {
        assert_eq!(
            run(Progress::Complete, ReadbackEvent::Tick),
            (
                Some(Progress::Complete),
                AxReadbackMeasurement {
                    elapsed_ms: 5,
                    outcome: AxReadback::Poll
                }
            )
        );
    }

    #[test]
    fn unchanged_or_unreadable_exhausts_bound_and_allows_fallback() {
        for progress in [Progress::Unchanged, Progress::Unreadable] {
            assert_eq!(
                run(progress, ReadbackEvent::Tick),
                (
                    Some(progress),
                    AxReadbackMeasurement {
                        elapsed_ms: 12,
                        outcome: AxReadback::TimedOut
                    }
                )
            );
        }
    }

    #[test]
    fn partial_stops_without_replay() {
        assert_eq!(
            run(Progress::Partial, ReadbackEvent::Notified),
            (
                Some(Progress::Partial),
                AxReadbackMeasurement {
                    elapsed_ms: 2,
                    outcome: AxReadback::Notification
                }
            )
        );
    }

    #[test]
    fn rejected_write_skips_reads_and_waits() {
        let result = await_readback::<Progress>(
            false,
            Duration::from_millis(250),
            || panic!("read after rejected write"),
            |_| false,
            || panic!("clock after rejected write"),
            |_| panic!("wait after rejected write"),
        );
        assert_eq!(result, (None, AxReadbackMeasurement::default()));
    }

    #[test]
    fn zero_knob_preserves_immediate_read_but_never_waits() {
        let result = await_readback(
            true,
            Duration::ZERO,
            || Progress::Unchanged,
            |_| false,
            || Duration::from_millis(1),
            |_| panic!("disabled wait"),
        );
        assert_eq!(
            result,
            (
                Some(Progress::Unchanged),
                AxReadbackMeasurement {
                    elapsed_ms: 1,
                    outcome: AxReadback::Skipped,
                }
            )
        );
    }

    #[test]
    fn immediate_success_includes_read_latency() {
        let result = await_readback(
            true,
            Duration::from_millis(250),
            || Progress::Complete,
            |_| true,
            || Duration::from_millis(3),
            |_| panic!("already settled"),
        );
        assert_eq!(
            result.1,
            AxReadbackMeasurement {
                elapsed_ms: 3,
                outcome: AxReadback::Immediate
            }
        );
    }
}
