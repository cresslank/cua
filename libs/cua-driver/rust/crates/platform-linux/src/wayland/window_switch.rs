//! GNOME launcher fallback: only overlay input is global; search delivery is Shell-owned.
use super::shell_helper::window_switch::{SwitchPeer, SwitchTarget};
use std::time::{Duration, Instant};

pub(crate) fn keyboard(
    peer: &SwitchPeer,
    target: &SwitchTarget,
    query: &str,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        peer.has("shell-search-focus-v1") && peer.has("shell-search-delivery-v1"),
        "keyboard_unavailable: helper lacks receiver-routed Shell search"
    );
    anyhow::ensure!(
        safe_query(query),
        "keyboard_unavailable: query must be printable nonempty text (at most 256 characters)"
    );
    let _deadline =
        super::foreground_deadline::Scope::until(Instant::now() + Duration::from_secs(8));
    let action = || -> anyhow::Result<()> {
        peer.validate_target(target)?;
        let state = peer.state()?;
        anyhow::ensure!(!state.session_locked, "session_locked");
        anyhow::ensure!(
            state.idle(),
            "shell_not_ready: another helper focus transaction remains open"
        );
        if !state.search_ready() {
            if !state.overview_visible {
                anyhow::ensure!(
                    state.overlay_key_is_super,
                    "keyboard_unavailable: Mutter overlay-key is not Super_L"
                );
                // No portal open/reconnect while this action owns the raw-input lease.
                super::require_gnome_keyboard_transport_ready()
                    .map_err(|error| anyhow::anyhow!("keyboard_unavailable: {error}"))?;
                overlay(peer, target)?;
            }
            poll_search(peer, target, Duration::from_secs(2))?;
        }
        // An earlier proof authorizes no keystrokes. This RPC rechecks focus and
        // delivers text + matched-app activation synchronously inside Shell.
        peer.deliver_search(target, query)?;
        let until = Instant::now() + Duration::from_secs(2);
        loop {
            let state = peer.state()?;
            anyhow::ensure!(!state.session_locked, "session_locked");
            if state.target_focused(target) {
                return Ok(());
            }
            anyhow::ensure!(
                Instant::now() < until,
                "switch_unverified: Shell search did not focus the exact PID/window"
            );
            cua_driver_core::blocking::check_cancelled()?;
            std::thread::sleep(Duration::from_millis(25));
        }
    };
    let outcome = action();
    if outcome.is_err() {
        // Shell API only. Focus loss between any reads cannot redirect Escape.
        let _ = peer.cancel_search(target);
    }
    outcome
}

#[cfg(feature = "portal-input")]
fn overlay(peer: &SwitchPeer, target: &SwitchTarget) -> anyhow::Result<()> {
    let peer = peer.clone();
    let target = target.clone();
    super::libei::switch_overlay(std::sync::Arc::new(move || {
        peer.validate_target(&target)?;
        let state = peer.state()?;
        anyhow::ensure!(!state.session_locked, "session_locked");
        anyhow::ensure!(
            state.idle() && state.overlay_key_is_super,
            "shell_not_ready: overlay binding or helper state changed before dispatch"
        );
        Ok(())
    }))
}

#[cfg(not(feature = "portal-input"))]
fn overlay(_peer: &SwitchPeer, _target: &SwitchTarget) -> anyhow::Result<()> {
    anyhow::bail!("keyboard_unavailable: switch overlay requires portal-input/libei")
}

fn safe_query(query: &str) -> bool {
    !query.trim().is_empty() && query.chars().count() <= 256 && !query.chars().any(char::is_control)
}

fn poll_search(peer: &SwitchPeer, target: &SwitchTarget, timeout: Duration) -> anyhow::Result<()> {
    let until = Instant::now() + timeout;
    loop {
        let state = peer.state()?;
        anyhow::ensure!(!state.session_locked, "session_locked");
        if state.search_ready() {
            return Ok(());
        }
        if state.overview_visible {
            match peer.focus_search(target) {
                Ok(state) if state.search_ready() => return Ok(()),
                // Animation/mapping can temporarily refuse. Retry only the
                // explicit shell_not_ready refusal, never lock/target/epoch errors.
                Err(error) if !transient_search_refusal(&error) => return Err(error),
                _ => {}
            }
        }
        anyhow::ensure!(
            Instant::now() < until,
            "shell_not_ready: overview search entry did not become ready"
        );
        cua_driver_core::blocking::check_cancelled()?;
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn transient_search_refusal(error: &anyhow::Error) -> bool {
    error.to_string() == "shell_not_ready: Shell search focus could not be established"
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn keyboard_queries_cannot_embed_input_controls() {
        for query in ["", " ", "app\n", "app\r", "\tapp", "\u{1b}", "a\u{7f}"] {
            assert!(!safe_query(query), "{query:?}");
        }
        for query in ["Terminal", "Firefox", "文件", "App's Name"] {
            assert!(safe_query(query));
        }
        assert!(!safe_query(&"a".repeat(257)));
        assert!(safe_query(&"a".repeat(256)));
    }

    #[test]
    fn only_transient_search_refusal_is_retryable() {
        assert!(transient_search_refusal(&anyhow::anyhow!(
            "shell_not_ready: Shell search focus could not be established"
        )));
        for reason in [
            "session_locked: Shell search focus could not be established",
            "window_not_found",
            "shell_not_ready: invalid search-focus epoch/protocol",
            "shell_not_ready: helper owner changed",
        ] {
            assert!(
                !transient_search_refusal(&anyhow::anyhow!("{reason}")),
                "{reason}"
            );
        }
    }
}
