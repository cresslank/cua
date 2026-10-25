//! Pure opt-in guard for the ignored desktop test's temporary CI OS policy.

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PolicyOptIn {
    Unchanged,
    HostedCi,
    Refuse,
}

pub(crate) fn policy_opt_in(
    allow: Option<&str>,
    actions: Option<&str>,
    runner: Option<&str>,
) -> PolicyOptIn {
    if allow != Some("1") {
        PolicyOptIn::Unchanged
    } else if actions == Some("true") && runner == Some("github-hosted") {
        PolicyOptIn::HostedCi
    } else {
        PolicyOptIn::Refuse
    }
}

#[cfg(test)]
mod tests {
    use super::{policy_opt_in, PolicyOptIn};

    #[test]
    fn policy_requires_explicit_one_even_on_a_hosted_runner() {
        for allow in [None, Some(""), Some("0"), Some("true"), Some("yes")] {
            assert_eq!(
                policy_opt_in(allow, Some("true"), Some("github-hosted")),
                PolicyOptIn::Unchanged
            );
        }
    }

    #[test]
    fn policy_opt_in_is_refused_outside_a_hosted_actions_runner() {
        for actions in [None, Some("false"), Some("true")] {
            for runner in [None, Some("self-hosted"), Some("github-hosted")] {
                if actions == Some("true") && runner == Some("github-hosted") {
                    continue;
                }
                assert_eq!(
                    policy_opt_in(Some("1"), actions, runner),
                    PolicyOptIn::Refuse
                );
            }
        }
    }

    #[test]
    fn policy_allows_only_the_explicit_hosted_ci_combination() {
        assert_eq!(
            policy_opt_in(Some("1"), Some("true"), Some("github-hosted")),
            PolicyOptIn::HostedCi
        );
    }
}
