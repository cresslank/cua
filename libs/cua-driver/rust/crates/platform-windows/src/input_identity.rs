//! Pure identity decisions used by Windows global-input transactions.
//!
//! This module intentionally contains no Win32 calls so stale/recycled identity
//! and foreground-restoration policy remain testable on every build host.

pub(crate) fn recipient_identity_matches(
    expected_root: u64,
    expected_pid: u32,
    actual_root: u64,
    actual_pid: Option<u32>,
) -> bool {
    expected_root == actual_root && actual_pid == Some(expected_pid)
}

pub(crate) fn restoration_is_permitted(
    previous_is_current: bool,
    displaced_is_current: bool,
) -> bool {
    previous_is_current || displaced_is_current
}

#[cfg(test)]
mod tests {
    use super::{recipient_identity_matches, restoration_is_permitted};

    #[test]
    fn stale_or_recycled_recipient_identity_is_rejected() {
        assert!(recipient_identity_matches(0x10, 41, 0x10, Some(41)));
        assert!(!recipient_identity_matches(0x10, 41, 0x11, Some(41)));
        assert!(!recipient_identity_matches(0x10, 41, 0x10, Some(42)));
        assert!(!recipient_identity_matches(0x10, 41, 0x10, None));
    }

    #[test]
    fn unrelated_user_foreground_supersedes_restoration() {
        assert!(restoration_is_permitted(true, false));
        assert!(restoration_is_permitted(false, true));
        assert!(restoration_is_permitted(true, true));
        assert!(!restoration_is_permitted(false, false));
    }
}
