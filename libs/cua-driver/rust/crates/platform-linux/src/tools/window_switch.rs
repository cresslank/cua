//! Persistent Linux focus changes; never a temporary input transaction.
use async_trait::async_trait;
use cua_driver_core::window_switch::{self as contract, Attempt, SwitchArgs, SwitchMethod};
use cua_driver_core::{
    protocol::ToolResult,
    tool::{Tool, ToolDef},
};
use serde_json::Value;
use std::time::{Duration, Instant};

pub struct SwitchWindowTool;

#[async_trait]
impl Tool for SwitchWindowTool {
    fn def(&self) -> &ToolDef {
        contract::definition()
    }

    async fn invoke(&self, value: Value) -> ToolResult {
        let args = match SwitchArgs::parse(&value) {
            Ok(args) => std::sync::Arc::new(args),
            Err(error) => return error,
        };
        let work = args.clone();
        match cua_driver_core::blocking::spawn(move || run(&work)).await {
            Ok(result) => result,
            Err(error) => failure(
                &args,
                None,
                session(),
                requested_route(&args),
                "switch_unverified",
                &error.to_string(),
            ),
        }
    }
}

fn session() -> &'static str {
    // A disabled native backend must not turn a Wayland session into X11 via
    // XWayland's DISPLAY. Only real X11 sessions enter the XID activation path.
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        "wayland"
    } else {
        "x11"
    }
}

fn requested_route(args: &SwitchArgs) -> &'static str {
    if args.method == SwitchMethod::Keyboard {
        "keyboard"
    } else {
        "native"
    }
}

fn failure(
    args: &SwitchArgs,
    id: Option<u64>,
    session: &str,
    route: &str,
    code: &str,
    reason: &str,
) -> ToolResult {
    contract::result(
        args,
        id,
        "linux",
        Some(session),
        route,
        false,
        vec![if session == "wayland" && route == "keyboard" {
            Attempt::refused(route, reason).with_delivery("shell_search")
        } else {
            Attempt::refused(route, reason)
        }],
        Some(code),
    )
}

fn run(args: &SwitchArgs) -> ToolResult {
    let session = session();
    // One host-wide lease covers resolution, native activation, all keyboard
    // fallback input, readback and cleanup. Detached native workers retain the
    // core dispatch lease through cua_driver_core::blocking::spawn.
    let _lease = match crate::wayland::acquire_host_raw_input_lease() {
        Ok(lease) => lease,
        Err(error) => {
            return failure(
                args,
                args.window_id,
                session,
                requested_route(args),
                "shell_not_ready",
                &error.to_string(),
            )
        }
    };
    if session == "wayland" {
        wayland(args)
    } else {
        x11(args)
    }
}

fn select_index(ids: &[u64], wanted: Option<u64>) -> Result<usize, &'static str> {
    if let Some(wanted) = wanted {
        let found: Vec<_> = ids
            .iter()
            .enumerate()
            .filter(|(_, id)| **id == wanted)
            .collect();
        return match found.as_slice() {
            [(index, _)] => Ok(*index),
            [] => Err("window_not_found"),
            _ => Err("target_ambiguous"),
        };
    }
    match ids.len() {
        0 => Err("window_not_found"),
        1 => Ok(0),
        _ => Err("target_ambiguous"), // Never choose a sibling by stacking/enumeration order.
    }
}

fn code(error: &anyhow::Error, default: &'static str) -> &'static str {
    match error.to_string().split(':').next().unwrap_or("").trim() {
        "session_locked" => "session_locked",
        "window_not_found" => "window_not_found",
        "target_ambiguous" => "target_ambiguous",
        "keyboard_unavailable" => "keyboard_unavailable",
        "shell_not_ready" => "shell_not_ready",
        "unsupported" => "unsupported",
        "switch_unverified" => "switch_unverified",
        _ => default,
    }
}

fn wayland(args: &SwitchArgs) -> ToolResult {
    use crate::wayland::shell_helper::window_switch::SwitchPeer;
    let route = requested_route(args);
    if !crate::wayland::shell_helper::gnome_shell_session() {
        return failure(
            args,
            args.window_id,
            "wayland",
            route,
            "unsupported",
            "switch_window currently requires the GNOME exact-target helper on Wayland",
        );
    }
    let peer = match SwitchPeer::connect() {
        Ok(peer) => peer,
        Err(error) => {
            return failure(
                args,
                args.window_id,
                "wayland",
                route,
                code(&error, "shell_not_ready"),
                &error.to_string(),
            )
        }
    };
    // Lock refusal precedes window absence/ambiguity and capability diagnostics.
    if peer.state().is_ok_and(|state| state.session_locked) {
        return failure(
            args,
            args.window_id,
            "wayland",
            route,
            "session_locked",
            "session_locked",
        );
    }
    let targets = match peer.targets(args.pid as u32) {
        Ok(targets) => targets,
        Err(error) => {
            return failure(
                args,
                args.window_id,
                "wayland",
                route,
                code(&error, "shell_not_ready"),
                &error.to_string(),
            )
        }
    };
    let ids: Vec<_> = targets.iter().map(|target| target.window_id).collect();
    let index = match select_index(&ids, args.window_id) {
        Ok(index) => index,
        Err(code) => return failure(args, args.window_id, "wayland", route, code, code),
    };
    let target = &targets[index];
    let id = Some(target.window_id);
    if args.method == SwitchMethod::Native && !peer.has("persistent-switch-v1") {
        return failure(
            args,
            id,
            "wayland",
            "native",
            "unsupported",
            "helper lacks persistent-switch-v1",
        );
    }
    // Older helpers may have exact enumeration without either new capability.
    // Do not weaken the existing activation/capture guards to accommodate them.
    if let Ok(state) = peer.state() {
        if state.session_locked {
            return failure(
                args,
                id,
                "wayland",
                route,
                "session_locked",
                "session_locked",
            );
        }
        if state.target_focused(target) {
            return contract::result(
                args,
                id,
                "linux",
                Some("wayland"),
                "already_focused",
                true,
                vec![],
                None,
            );
        }
    }
    let mut attempts = Vec::new();
    if args.method != SwitchMethod::Keyboard {
        match peer.native(target) {
            Ok(()) => {
                return contract::result(
                    args,
                    id,
                    "linux",
                    Some("wayland"),
                    "native",
                    true,
                    vec![Attempt::focused("native")],
                    None,
                )
            }
            Err(error) => {
                let refusal = code(&error, "switch_unverified");
                let attempt = if matches!(
                    refusal,
                    "unsupported" | "session_locked" | "window_not_found" | "shell_not_ready"
                ) {
                    Attempt::refused("native", error.to_string())
                } else {
                    Attempt::unverified("native", error.to_string())
                };
                let fallback = contract::should_fallback(args.method, &attempt)
                    && !matches!(
                        refusal,
                        "session_locked" | "window_not_found" | "target_ambiguous"
                    );
                attempts.push(attempt);
                if !fallback {
                    return contract::result(
                        args,
                        id,
                        "linux",
                        Some("wayland"),
                        "native",
                        false,
                        attempts,
                        Some(refusal),
                    );
                }
            }
        }
    } else {
        attempts.push(Attempt::skipped("native", "explicit keyboard method"));
    }
    let query: String = match args.query.clone().or_else(|| desktop_name(&target.app_id)) {
        Some(query) => query,
        None => {
            attempts.push(Attempt::refused("keyboard", "keyboard_unavailable: no unique desktop Name for target app identity; provide query").with_delivery("shell_search"));
            return contract::result(
                args,
                id,
                "linux",
                Some("wayland"),
                "keyboard",
                false,
                attempts,
                Some("keyboard_unavailable"),
            );
        }
    };
    match crate::wayland::window_switch::keyboard(&peer, target, &query) {
        Ok(()) => {
            attempts.push(Attempt::focused("keyboard").with_delivery("shell_search"));
            contract::result(
                args,
                id,
                "linux",
                Some("wayland"),
                "keyboard",
                true,
                attempts,
                None,
            )
        }
        Err(error) => {
            let refusal = code(&error, "switch_unverified");
            attempts.push(
                if refusal == "switch_unverified" {
                    Attempt::unverified("keyboard", error.to_string())
                } else {
                    Attempt::refused("keyboard", error.to_string())
                }
                .with_delivery("shell_search"),
            );
            contract::result(
                args,
                id,
                "linux",
                Some("wayland"),
                "keyboard",
                false,
                attempts,
                Some(refusal),
            )
        }
    }
}

fn desktop_name(app_id: &str) -> Option<String> {
    let matches: Vec<_> = crate::installed_apps::list_installed_apps()
        .into_iter()
        .filter(|app| app.owns_window_class(app_id))
        .collect();
    match matches.as_slice() {
        [app] if !app.name.trim().is_empty() => Some(app.name.clone()),
        _ => None,
    }
}

fn x11(args: &SwitchArgs) -> ToolResult {
    let route = requested_route(args);
    if let Err(error) = crate::wayland::shell_helper::require_unlocked_x11_session() {
        return failure(
            args,
            args.window_id,
            "x11",
            route,
            code(&error, "shell_not_ready"),
            &error.to_string(),
        );
    }
    let windows = crate::x11::list_windows(Some(args.pid as u32));
    let ids: Vec<_> = windows.iter().map(|window| window.xid).collect();
    let index = match select_index(&ids, args.window_id) {
        Ok(index) => index,
        Err(code) => return failure(args, args.window_id, "x11", route, code, code),
    };
    let id = windows[index].xid;
    let focused = || {
        crate::x11::window_belongs_to_pid(id, args.pid as u32)
            && crate::x11::focused_window_among(&[id]) == Some(id)
    };
    if focused() {
        return contract::result(
            args,
            Some(id),
            "linux",
            Some("x11"),
            "already_focused",
            true,
            vec![],
            None,
        );
    }
    if args.method == SwitchMethod::Keyboard {
        return failure(
            args,
            Some(id),
            "x11",
            "keyboard",
            "unsupported",
            "X11 keyboard switching is unsupported; use native",
        );
    }
    let native = || -> anyhow::Result<()> {
        cua_driver_core::blocking::check_cancelled()?;
        anyhow::ensure!(
            crate::x11::window_belongs_to_pid(id, args.pid as u32),
            "window_not_found"
        );
        crate::x11::unminimize_for_switch(id)?;
        crate::wayland::shell_helper::require_unlocked_x11_session()?;
        anyhow::ensure!(
            crate::x11::window_belongs_to_pid(id, args.pid as u32),
            "window_not_found"
        );
        crate::input::x11_activate_window_persistent(id)?;
        let until = Instant::now() + Duration::from_secs(1);
        loop {
            crate::wayland::shell_helper::require_unlocked_x11_session()?;
            if focused() {
                return Ok(());
            }
            anyhow::ensure!(
                Instant::now() < until,
                "switch_unverified: X11 keyboard focus did not reach exact target"
            );
            cua_driver_core::blocking::check_cancelled()?;
            std::thread::sleep(Duration::from_millis(25));
        }
    };
    match native() {
        Ok(()) => contract::result(
            args,
            Some(id),
            "linux",
            Some("x11"),
            "native",
            true,
            vec![Attempt::focused("native")],
            None,
        ),
        Err(error) => {
            let refusal = code(&error, "switch_unverified");
            let mut attempts = vec![if refusal == "switch_unverified" {
                Attempt::unverified("native", error.to_string())
            } else {
                Attempt::refused("native", error.to_string())
            }];
            if args.method == SwitchMethod::Auto {
                attempts.push(Attempt::skipped(
                    "keyboard",
                    "X11 keyboard switching is unsupported",
                ));
            }
            contract::result(
                args,
                Some(id),
                "linux",
                Some("x11"),
                "native",
                false,
                attempts,
                Some(refusal),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_selection_never_guesses_wayland_toplevels_or_aliases() {
        assert_eq!(select_index(&[], None), Err("window_not_found"));
        assert_eq!(select_index(&[7], None), Ok(0));
        assert_eq!(select_index(&[7, 8], None), Err("target_ambiguous"));
        assert_eq!(select_index(&[7, 8], Some(8)), Ok(1));
        assert_eq!(select_index(&[7, 8], Some(9)), Err("window_not_found"));
        assert_eq!(select_index(&[7, 7], Some(7)), Err("target_ambiguous"));
    }
    #[test]
    fn errors_preserve_lock_and_target_refusals_without_inventing_success() {
        for refusal in [
            "session_locked",
            "window_not_found",
            "target_ambiguous",
            "keyboard_unavailable",
            "shell_not_ready",
            "unsupported",
            "switch_unverified",
        ] {
            assert_eq!(
                code(&anyhow::anyhow!("{refusal}: detail"), "switch_unverified"),
                refusal
            );
        }
        assert_eq!(
            code(
                &anyhow::anyhow!("untrusted error mentions session_locked"),
                "switch_unverified"
            ),
            "switch_unverified"
        );
    }
    #[tokio::test]
    async fn invalid_arguments_are_rejected_before_any_desktop_io() {
        for value in [
            serde_json::json!({}),
            serde_json::json!({"pid":0}),
            serde_json::json!({"pid":1,"method":"guess"}),
            serde_json::json!({"pid":1,"window_id":0}),
            serde_json::json!({"pid":1,"query":""}),
        ] {
            let refused = SwitchWindowTool.invoke(value).await;
            assert_eq!(refused.is_error, Some(true));
            assert_eq!(
                refused.structured_content.unwrap()["code"],
                "invalid_arguments"
            );
        }
    }
}
