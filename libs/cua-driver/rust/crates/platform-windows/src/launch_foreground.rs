//! Pure decisions for the bounded Win32 launch/foreground settling interval.
//!
//! Launch completion and input-idle do not mean all startup activations have
//! finished: a browser can activate again when the URL opened by the shell
//! arrives. Keep observing after restoration, but never overwrite unrelated
//! foreground changes or claim success while foreground is unconfirmed.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LaunchForeground {
    Prior,
    LaunchedApp,
    Unrelated,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SettleStep {
    Observe,
    Restore,
    Complete,
    Superseded,
    Failed,
}

pub(crate) fn settle_step(
    prior_identity_live: bool,
    foreground: LaunchForeground,
    deadline_reached: bool,
) -> SettleStep {
    if !prior_identity_live {
        return SettleStep::Failed;
    }
    match foreground {
        LaunchForeground::Unrelated => SettleStep::Superseded,
        LaunchForeground::Prior if deadline_reached => SettleStep::Complete,
        LaunchForeground::Prior => SettleStep::Observe,
        LaunchForeground::LaunchedApp if deadline_reached => SettleStep::Failed,
        LaunchForeground::LaunchedApp => SettleStep::Restore,
        LaunchForeground::Unavailable if deadline_reached => SettleStep::Failed,
        LaunchForeground::Unavailable => SettleStep::Observe,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn successful_restore_keeps_observing_and_late_activation_restores_again() {
        let timeline = [
            (LaunchForeground::LaunchedApp, false),
            (LaunchForeground::Prior, false),
            (LaunchForeground::LaunchedApp, false),
            (LaunchForeground::Prior, false),
            (LaunchForeground::Prior, true),
        ];
        let actual: Vec<_> = timeline
            .into_iter()
            .map(|(foreground, expired)| settle_step(true, foreground, expired))
            .collect();
        assert_eq!(
            actual,
            [
                SettleStep::Restore,
                SettleStep::Observe,
                SettleStep::Restore,
                SettleStep::Observe,
                SettleStep::Complete,
            ]
        );
    }

    #[test]
    fn transient_denial_can_retry_but_permanent_denial_is_an_error() {
        assert_eq!(
            settle_step(true, LaunchForeground::LaunchedApp, false),
            SettleStep::Restore
        );
        // A rejected native attempt leaves the launched app foreground.
        assert_eq!(
            settle_step(true, LaunchForeground::LaunchedApp, false),
            SettleStep::Restore
        );
        assert_eq!(
            settle_step(true, LaunchForeground::LaunchedApp, true),
            SettleStep::Failed
        );
    }

    #[test]
    fn only_exact_prior_identity_can_complete_the_interval() {
        for foreground in [
            LaunchForeground::Prior,
            LaunchForeground::LaunchedApp,
            LaunchForeground::Unavailable,
            LaunchForeground::Unrelated,
        ] {
            assert_eq!(settle_step(false, foreground, false), SettleStep::Failed);
            assert_eq!(settle_step(false, foreground, true), SettleStep::Failed);
        }
    }

    #[test]
    fn unrelated_foreground_supersedes_without_another_restore() {
        for expired in [false, true] {
            assert_eq!(
                settle_step(true, LaunchForeground::Unrelated, expired),
                SettleStep::Superseded
            );
        }
    }

    #[test]
    fn transient_null_foreground_can_settle_but_is_not_success_at_expiry() {
        assert_eq!(
            settle_step(true, LaunchForeground::Unavailable, false),
            SettleStep::Observe
        );
        assert_eq!(
            settle_step(true, LaunchForeground::Unavailable, true),
            SettleStep::Failed
        );
    }

    #[test]
    fn no_activation_still_observes_the_full_startup_interval() {
        assert_eq!(
            settle_step(true, LaunchForeground::Prior, false),
            SettleStep::Observe
        );
        assert_eq!(
            settle_step(true, LaunchForeground::Prior, true),
            SettleStep::Complete
        );
    }
}
