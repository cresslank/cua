//! Shared persistent-switch wire contract. Platform adapters own observation and input.
use std::sync::OnceLock;

use serde::Serialize;
use serde_json::{json, Value};

use crate::{protocol::ToolResult, tool::ToolDef};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchMethod {
    Auto,
    Native,
    Keyboard,
}

#[derive(Debug, Clone)]
pub struct SwitchArgs {
    pub pid: i32,
    pub window_id: Option<u64>,
    pub method: SwitchMethod,
    pub query: Option<String>,
}

impl SwitchArgs {
    pub fn parse(value: &Value) -> Result<Self, ToolResult> {
        let invalid = |message: &str| {
            ToolResult::error(format!("switch_window: {message}"))
                .with_structured(json!({"code":"invalid_arguments"}))
        };
        let object = value
            .as_object()
            .ok_or_else(|| invalid("expected an object"))?;
        if object.keys().any(|key| {
            !matches!(
                key.as_str(),
                "pid"
                    | "window_id"
                    | "method"
                    | "query"
                    | "session"
                    | "_session_id"
                    | "_transport_session_id"
                    | "_public_session_label"
                    | "_session_idle_ttl_ms"
                    | "_input_origin"
                    | "_protected_process_fingerprint"
            )
        }) {
            return Err(invalid("unknown argument"));
        }
        if object.get("session").is_some_and(|v| !v.is_string()) {
            return Err(invalid("session must be a string"));
        }
        let pid = object
            .get("pid")
            .and_then(Value::as_i64)
            .and_then(|pid| i32::try_from(pid).ok())
            .filter(|pid| *pid > 0)
            .ok_or_else(|| invalid("pid must be a positive process identifier"))?;
        let window_id = match object.get("window_id") {
            None => None,
            Some(value) => Some(
                value
                    .as_u64()
                    .filter(|id| *id > 0)
                    .ok_or_else(|| invalid("window_id must be a positive 64-bit integer"))?,
            ),
        };
        let method = match object.get("method") {
            None => SwitchMethod::Auto,
            Some(Value::String(method)) => match method.as_str() {
                "auto" => SwitchMethod::Auto,
                "native" => SwitchMethod::Native,
                "keyboard" => SwitchMethod::Keyboard,
                _ => return Err(invalid("method must be auto, native, or keyboard")),
            },
            _ => return Err(invalid("method must be auto, native, or keyboard")),
        };
        let query = match object.get("query") {
            None => None,
            Some(Value::String(query))
                if !query.trim().is_empty()
                    && !query
                        .chars()
                        .any(|ch| ch.is_control() || matches!(ch, '\u{2028}' | '\u{2029}')) =>
            {
                Some(query.clone())
            }
            _ => {
                return Err(invalid(
                    "query must be nonempty text without control characters",
                ))
            }
        };
        Ok(Self {
            pid,
            window_id,
            method,
            query,
        })
    }
}

pub fn definition() -> &'static ToolDef {
    static DEF: OnceLock<ToolDef> = OnceLock::new();
    DEF.get_or_init(|| ToolDef {
        name: "switch_window".into(),
        description: "Persistently switch to a running app's window. This changes foreground focus and does not restore prior focus. auto tries exact native activation first, then real Shell/Spotlight keyboard search only after refusal or unverified activation. Keyboard text and Return require proven launcher search-field focus. Success requires compositor/OS focus readback, never request acceptance. GNOME Wayland requires an unambiguous exact toplevel and compatible helper; X11 supports native activation only. macOS verifies the exact focused window. Locked sessions refuse.".into(),
        input_schema: json!({
            "type":"object", "required":["pid"],
            "properties": {
                "pid": {"type":"integer", "minimum":1, "maximum":2147483647, "description":"Process ID of the running target application."},
                "window_id": {"type":"integer", "minimum":1, "description":"Exact window ID from fresh list_windows discovery. Required for a Wayland PID with multiple toplevels."},
                "method": {"type":"string", "enum":["auto","native","keyboard"], "default":"auto", "description":"auto tries native then keyboard on refusal/unverified focus; native and keyboard use only the named route."},
                "query": {"type":"string", "minLength":1, "description":"Keyboard search text without control characters. Defaults to the target application's display name."}
            },
            "additionalProperties":false
        }),
        read_only:false, destructive:false, idempotent:true, open_world:false,
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct Attempt {
    pub route: String,
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivery: Option<String>,
}

impl Attempt {
    pub fn with_delivery(mut self, delivery: &str) -> Self {
        self.delivery = Some(delivery.into());
        self
    }

    pub fn focused(route: &str) -> Self {
        Self {
            route: route.into(),
            outcome: "focused".into(),
            reason: None,
            delivery: None,
        }
    }
    pub fn refused(route: &str, reason: impl Into<String>) -> Self {
        Self {
            route: route.into(),
            outcome: "refused".into(),
            reason: Some(reason.into()),
            delivery: None,
        }
    }
    pub fn unverified(route: &str, reason: impl Into<String>) -> Self {
        Self {
            route: route.into(),
            outcome: "unverified".into(),
            reason: Some(reason.into()),
            delivery: None,
        }
    }
    pub fn skipped(route: &str, reason: impl Into<String>) -> Self {
        Self {
            route: route.into(),
            outcome: "skipped".into(),
            reason: Some(reason.into()),
            delivery: None,
        }
    }
}

pub fn should_fallback(method: SwitchMethod, attempt: &Attempt) -> bool {
    let refusal = attempt
        .reason
        .as_deref()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("")
        .trim();
    method == SwitchMethod::Auto
        && attempt.route == "native"
        && matches!(attempt.outcome.as_str(), "refused" | "unverified")
        && !matches!(
            refusal,
            "session_locked" | "window_not_found" | "target_ambiguous"
        )
}

#[allow(clippy::too_many_arguments)]
pub fn result(
    args: &SwitchArgs,
    window_id: Option<u64>,
    platform: &str,
    session: Option<&str>,
    route: &str,
    focused: bool,
    attempts: Vec<Attempt>,
    code: Option<&str>,
) -> ToolResult {
    let effect = if focused {
        "verified"
    } else if attempts.iter().any(|a| a.outcome == "unverified") {
        "unverifiable"
    } else {
        "refused"
    };
    let mut structured = json!({
        "route":route, "pid":args.pid, "window_id":window_id.or(args.window_id),
        "focused":focused, "attempts":attempts, "platform":platform, "effect":effect
    });
    if let Some(session) = session {
        structured["session"] = json!(session);
    }
    let answer = if focused {
        ToolResult::text(format!(
            "Focused target pid {} window {:?} via {route}.",
            args.pid, window_id
        ))
    } else {
        let code = code.unwrap_or("switch_unverified");
        structured["code"] = json!(code);
        ToolResult::error(format!(
            "switch_window: {code}; target focus was not verified"
        ))
    };
    answer.with_structured(structured)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_reject_unknowns_wrong_types_ranges_and_control_queries() {
        for args in [
            json!(null),
            json!({}),
            json!({"pid":true}),
            json!({"pid":0}),
            json!({"pid":-1}),
            json!({"pid":2147483648_u64}),
            json!({"pid":1.5}),
            json!({"pid":1,"window_id":0}),
            json!({"pid":1,"window_id":-1}),
            json!({"pid":1,"window_id":1.5}),
            json!({"pid":1,"method":"guess"}),
            json!({"pid":1,"method":null}),
            json!({"pid":1,"query":"  "}),
            json!({"pid":1,"query":"app\n"}),
            json!({"pid":1,"query":false}),
            json!({"pid":1,"session":2}),
            json!({"pid":1,"extra":true}),
        ] {
            let error = SwitchArgs::parse(&args).unwrap_err();
            assert_eq!(error.is_error, Some(true));
            assert_eq!(
                error.structured_content.unwrap()["code"],
                "invalid_arguments"
            );
        }
        let args = SwitchArgs::parse(&json!({"pid":42,"session":"owned"})).unwrap();
        assert_eq!(args.method, SwitchMethod::Auto);
        // Existing GNOME public IDs are full-width hashes; never remap them.
        assert_eq!(
            SwitchArgs::parse(&json!({"pid":42,"window_id":u64::MAX}))
                .unwrap()
                .window_id,
            Some(u64::MAX)
        );
        assert_eq!(
            crate::action_lease::classify_tool("switch_window"),
            crate::action_lease::ActionClass::DesktopRaw
        );
        assert_eq!(
            crate::tool::default_capabilities_for("switch_window"),
            vec!["window.activate"]
        );
        assert_eq!(
            crate::authorization::advertised_risk_for("switch_window"),
            crate::authorization::advertised_risk_for("bring_to_front")
        );
        assert_eq!(args.window_id, None);
        for method in ["auto", "native", "keyboard"] {
            assert!(
                SwitchArgs::parse(&json!({"pid":42,"method":method,"query":"Éditeur"})).is_ok()
            );
        }
    }

    #[test]
    fn fallback_is_only_auto_after_native_refusal_or_unverified() {
        for code in ["session_locked", "window_not_found", "target_ambiguous"] {
            for reason in [
                code.to_string(),
                format!("{code}: exact target disappeared"),
            ] {
                for attempt in [
                    Attempt::refused("native", &reason),
                    Attempt::unverified("native", &reason),
                ] {
                    assert!(!should_fallback(SwitchMethod::Auto, &attempt), "{reason}");
                }
            }
        }
        for code in ["unsupported", "switch_unverified", "shell_not_ready"] {
            assert!(should_fallback(
                SwitchMethod::Auto,
                &Attempt::refused("native", code)
            ));
        }
        for method in [
            SwitchMethod::Auto,
            SwitchMethod::Native,
            SwitchMethod::Keyboard,
        ] {
            for route in ["native", "keyboard", "already_focused"] {
                for outcome in ["focused", "refused", "unverified", "skipped"] {
                    let attempt = Attempt {
                        route: route.into(),
                        outcome: outcome.into(),
                        reason: None,
                        delivery: None,
                    };
                    assert_eq!(
                        should_fallback(method, &attempt),
                        method == SwitchMethod::Auto
                            && route == "native"
                            && matches!(outcome, "refused" | "unverified")
                    );
                }
            }
        }
    }

    #[test]
    fn wire_shape_preserves_attempts_codes_and_verified_focus_only() {
        let args = SwitchArgs::parse(&json!({"pid":42,"window_id":7})).unwrap();
        let ok = result(
            &args,
            Some(7),
            "linux",
            Some("wayland"),
            "keyboard",
            true,
            vec![
                Attempt::refused("native", "unsupported"),
                Attempt::focused("keyboard"),
            ],
            None,
        );
        assert_eq!(ok.is_error, None);
        let data = ok.structured_content.unwrap();
        assert_eq!(
            data,
            json!({"route":"keyboard","pid":42,"window_id":7,"focused":true,
            "platform":"linux","session":"wayland","effect":"verified",
            "attempts":[{"route":"native","outcome":"refused","reason":"unsupported"},{"route":"keyboard","outcome":"focused"}]})
        );
        for code in [
            "session_locked",
            "window_not_found",
            "target_ambiguous",
            "switch_unverified",
            "keyboard_unavailable",
            "shell_not_ready",
            "unsupported",
        ] {
            let failed = result(
                &args,
                None,
                "macos",
                None,
                "native",
                false,
                vec![Attempt::refused("native", code)],
                Some(code),
            );
            assert_eq!(failed.is_error, Some(true));
            let data = failed.structured_content.unwrap();
            assert_eq!(data["code"], code);
            assert_eq!(data["effect"], "refused");
            assert_eq!(data["window_id"], 7);
            assert!(data.get("session").is_none());
        }
        let failed = result(
            &args,
            None,
            "macos",
            None,
            "native",
            false,
            vec![Attempt::unverified("native", "timeout")],
            None,
        );
        assert_eq!(failed.structured_content.unwrap()["effect"], "unverifiable");
        assert_eq!(definition().input_schema["additionalProperties"], false);
        assert!(!definition().read_only && !definition().destructive && definition().idempotent);
    }
}
