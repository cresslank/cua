//! Bounded AX insertion observation, independent of a native run loop or clock.
//!
//! Gecko updates AXValue asynchronously after an AXSelectedText write. Waiting
//! for that update avoids synthesizing the same payload a second time.

use cua_driver_contract::AxReadback;
use std::time::Duration;

/// Backstop cadence only; notifications wake the observer immediately.
pub const READBACK_SLICE: Duration = Duration::from_millis(5);

/// Disabling asynchronous waiting preserves one immediate read, capped at one
/// polling slice. Callers must reserve this allowance in synthesis preflight.
pub fn readback_budget(timeout: Duration) -> Duration {
    if timeout.is_zero() {
        READBACK_SLICE
    } else {
        timeout
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadbackEvent {
    Notified,
    Tick,
}

/// A successful write without an admitted read may already have delivered text.
/// Keep it distinct from rejection so callers cannot replay it as a failed write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AxReadbackResult<T> {
    Rejected,
    Unobserved,
    Observed(T),
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
/// argument, returning early on a notification. `read` must cap native work to
/// its remaining-budget argument. Zero disables waiting, but keeps a single
/// immediate read bounded by `readback_budget`.
/// `settled` accepts both complete
/// and partial delivery: either must stop the ladder before any replay.
pub fn await_readback<T>(
    write_succeeded: bool,
    timeout: Duration,
    mut read: impl FnMut(Duration) -> T,
    settled: impl Fn(&T) -> bool,
    elapsed: impl Fn() -> Duration,
    mut wait: impl FnMut(Duration) -> ReadbackEvent,
) -> (AxReadbackResult<T>, AxReadbackMeasurement) {
    if !write_succeeded {
        return (AxReadbackResult::Rejected, AxReadbackMeasurement::default());
    }
    let budget = readback_budget(timeout);
    let mut value = None;
    let mut cause = AxReadback::Immediate;
    loop {
        let duration = elapsed();
        if duration >= budget || (timeout.is_zero() && value.is_some()) {
            return (
                value.map_or(AxReadbackResult::Unobserved, AxReadbackResult::Observed),
                AxReadbackMeasurement {
                    elapsed_ms: duration.as_millis().min(u64::MAX as u128) as u64,
                    outcome: if timeout.is_zero() {
                        AxReadback::Skipped
                    } else {
                        AxReadback::TimedOut
                    },
                },
            );
        }
        value = Some(read(budget - duration));
        let duration = elapsed();
        // Keep even a late positive/partial observation: losing it could cause
        // the non-idempotent typing ladder to replay text already delivered.
        if value.as_ref().is_some_and(&settled) {
            return (
                AxReadbackResult::Observed(value.expect("settled read has a value")),
                AxReadbackMeasurement {
                    elapsed_ms: duration.as_millis().min(u64::MAX as u128) as u64,
                    outcome: cause,
                },
            );
        }
        if !timeout.is_zero() && duration < budget {
            cause = match wait(READBACK_SLICE.min(budget - duration)) {
                ReadbackEvent::Notified => AxReadback::Notification,
                ReadbackEvent::Tick => AxReadback::Poll,
            };
        }
        // Recheck expiry after every read and wait, before admitting more work.
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

    fn run(
        value: Progress,
        event: ReadbackEvent,
    ) -> (AxReadbackResult<Progress>, AxReadbackMeasurement) {
        let clock = Cell::new(Duration::ZERO);
        let mut reads = 0;
        let result = await_readback(
            true,
            Duration::from_millis(12),
            |_| {
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
                AxReadbackResult::Observed(Progress::Complete),
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
                AxReadbackResult::Observed(Progress::Complete),
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
                    AxReadbackResult::Observed(progress),
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
                AxReadbackResult::Observed(Progress::Partial),
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
            |_| panic!("read after rejected write"),
            |_| false,
            || panic!("clock after rejected write"),
            |_| panic!("wait after rejected write"),
        );
        assert_eq!(
            result,
            (AxReadbackResult::Rejected, AxReadbackMeasurement::default())
        );
    }

    #[test]
    fn zero_knob_preserves_immediate_read_but_never_waits() {
        let result = await_readback(
            true,
            Duration::ZERO,
            |remaining| {
                assert_eq!(remaining, Duration::from_millis(4));
                Progress::Unchanged
            },
            |_| false,
            || Duration::from_millis(1),
            |_| panic!("disabled wait"),
        );
        assert_eq!(
            result,
            (
                AxReadbackResult::Observed(Progress::Unchanged),
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
            |_| Progress::Complete,
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

    #[test]
    fn slow_reads_receive_only_the_remaining_budget() {
        let clock = Cell::new(Duration::ZERO);
        let mut budgets = Vec::new();
        let result = await_readback(
            true,
            Duration::from_millis(250),
            |remaining| {
                budgets.push(remaining);
                clock.set(clock.get() + remaining.min(Duration::from_millis(200)));
                Progress::Unreadable
            },
            |_| false,
            || clock.get(),
            |slice| {
                clock.set(clock.get() + slice);
                ReadbackEvent::Tick
            },
        );
        assert_eq!(
            budgets,
            [Duration::from_millis(250), Duration::from_millis(45)]
        );
        assert_eq!(result.0, AxReadbackResult::Observed(Progress::Unreadable));
        assert_eq!(
            result.1,
            AxReadbackMeasurement {
                elapsed_ms: 250,
                outcome: AxReadback::TimedOut
            }
        );
    }

    #[test]
    fn expiry_before_first_read_or_after_wait_never_admits_another_read() {
        for initially_expired in [false, true] {
            let timeout = Duration::from_millis(5);
            let clock = Cell::new(if initially_expired {
                timeout
            } else {
                Duration::ZERO
            });
            let mut reads = 0;
            let result = await_readback(
                true,
                timeout,
                |remaining| {
                    assert_eq!(remaining, timeout);
                    reads += 1;
                    Progress::Unchanged
                },
                |_| false,
                || clock.get(),
                |slice| {
                    clock.set(clock.get() + slice);
                    ReadbackEvent::Notified
                },
            );
            assert_eq!(reads, usize::from(!initially_expired));
            assert_eq!(
                result.0,
                if initially_expired {
                    AxReadbackResult::Unobserved
                } else {
                    AxReadbackResult::Observed(Progress::Unchanged)
                }
            );
            assert_eq!(result.1.outcome, AxReadback::TimedOut);
            assert_eq!(result.1.elapsed_ms, 5);
        }
    }
}
