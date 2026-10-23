//! Per-call input delivery mode and the shared `background_unavailable`
//! refusal envelope.
//!
//! Every platform accepts the same two `delivery_mode` rungs, parsed the same
//! way, and refuses an impossible background delivery with the same
//! foreground-escalation contract. Platform adapters keep only the
//! platform-specific reason text and fields.

use serde_json::{json, Value};

use crate::protocol::ToolResult;

/// Input delivery modality: the agent-selected rung of the
/// best-effort-background ladder, passed per call (never a stored or
/// configured setting).
///
/// - `Background` (default): inject without activating or raising the target.
///   When the platform cannot do that for the target, the tool refuses with
///   [`background_unavailable_result`] instead of fronting.
/// - `Foreground`: activate the target for the action. This is the agent's
///   explicit last resort.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum DeliveryMode {
    #[default]
    Background,
    Foreground,
}

impl DeliveryMode {
    /// Parse the per-call `delivery_mode` argument. Anything other than an
    /// explicit case-insensitive `"foreground"` resolves to `Background`, so
    /// an omitted, garbage, or removed legacy value (`"auto"`) never silently
    /// fronts.
    pub fn parse(arg: Option<&str>) -> Self {
        match arg {
            Some(s) if s.eq_ignore_ascii_case("foreground") => Self::Foreground,
            _ => Self::Background,
        }
    }

    /// Parse from a tool's JSON args, reading the `delivery_mode` field.
    pub fn from_args(args: &Value) -> Self {
        Self::parse(args.get("delivery_mode").and_then(Value::as_str))
    }

    pub fn is_foreground(self) -> bool {
        matches!(self, Self::Foreground)
    }
}

/// Environment variable an operator sets when starting the daemon to allow a
/// brief foreground step (trycua/cua#5478). Off by default.
pub const BRIEF_FOREGROUND_ENV: &str = "CUA_DRIVER_ALLOW_BRIEF_FOREGROUND";

/// Whether the operator allowed a brief foreground step for actions that
/// cannot finish in the background.
///
/// When it is on, a tool whose background attempt hits a known platform limit
/// (today: a macOS drag-and-drop session in a native window) repeats only that
/// step in the foreground, then hands the frontmost app and the pointer back,
/// and says so in its result. It is read from the daemon's environment, which
/// the agent can't change, and a call that passes `delivery_mode:"background"`
/// explicitly is never escalated.
pub fn brief_foreground_allowed() -> bool {
    brief_foreground_flag(std::env::var(BRIEF_FOREGROUND_ENV).ok().as_deref())
}

/// Parse [`BRIEF_FOREGROUND_ENV`]: only `1`, `true`, `yes` or `on`
/// (case-insensitive) turn it on.
pub fn brief_foreground_flag(value: Option<&str>) -> bool {
    matches!(
        value.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

/// Whether the call itself asked for background delivery by name, which opts
/// it out of the brief foreground step.
pub fn background_requested_explicitly(args: &Value) -> bool {
    args.get("delivery_mode")
        .and_then(Value::as_str)
        .is_some_and(|mode| mode.eq_ignore_ascii_case("background"))
}

/// The `suggestion` every background refusal carries.
pub const FOREGROUND_RETRY_SUGGESTION: &str =
    "Retry this action with delivery_mode:\"foreground\".";

/// Build a structured background-delivery refusal.
///
/// The result is an error with `message` as its text. Its structured content
/// holds `code`, the shared [`FOREGROUND_RETRY_SUGGESTION`], and an
/// `escalation` that recommends `foreground` for `escalation_reason`, plus
/// every field of the `fields` object. `fields` cannot override those
/// contract keys.
pub fn background_unavailable_result(
    message: impl Into<String>,
    code: &str,
    escalation_reason: impl Into<String>,
    fields: Value,
) -> ToolResult {
    background_unavailable_result_with_suggestion(
        message,
        code,
        escalation_reason,
        fields,
        FOREGROUND_RETRY_SUGGESTION,
    )
}

/// Like [`background_unavailable_result`], but `suggestion` replaces the
/// foreground retry text. Use it when a working background route exists, so the
/// refusal leads the caller to that route instead of implying the task is
/// blocked. The foreground `escalation` is still included.
pub fn background_unavailable_result_with_suggestion(
    message: impl Into<String>,
    code: &str,
    escalation_reason: impl Into<String>,
    fields: Value,
    suggestion: &str,
) -> ToolResult {
    let mut structured = match fields {
        Value::Object(fields) => fields,
        _ => serde_json::Map::new(),
    };
    structured.insert("code".to_owned(), json!(code));
    structured.insert("suggestion".to_owned(), json!(suggestion));
    structured.insert(
        "escalation".to_owned(),
        json!({
            "recommended": "foreground",
            "reason": escalation_reason.into(),
        }),
    );
    ToolResult::error(message).with_structured(Value::Object(structured))
}

/// Build a background refusal with explicit proof that this request sent no input.
///
/// Callers MUST guarantee that no input event, accessibility action,
/// scroll-into-view, or focus change has been performed for this request. A
/// failed/ambiguous actuator attempt does not satisfy this precondition. Use
/// [`background_unavailable_result`] when non-delivery has not been proven.
///
/// This remains the existing MCP error envelope, not the closed success-only
/// `ActionResult`. Its evidence uses that contract's public effect and delivery
/// types. `NotApplicable` describes an actuator that was never attempted; using
/// `Background` would misleadingly describe an actual delivery modality. The
/// internal record likewise represents an unattempted refusal with
/// `actual_delivery: None`, so no transport or action record is invented here.
/// The closed `ActionResult` refusal invariants are deliberately unchanged.
/// `fields` must contain diagnostics only, not other action/delivery evidence.
/// Supplied effect/delivery values cannot override the proof.
pub fn background_unavailable_before_input_result(
    message: impl Into<String>,
    code: &str,
    escalation_reason: impl Into<String>,
    fields: Value,
) -> ToolResult {
    use cua_driver_contract::{ActionDelivery, ActionDeliveryMode, ActionEffect};

    let mut result = background_unavailable_result(message, code, escalation_reason, fields);
    let structured = result
        .structured_content
        .as_mut()
        .expect("background refusal has structured content");
    structured["effect"] = json!(ActionEffect::Refused);
    structured["delivery"] = json!(ActionDelivery {
        mode: ActionDeliveryMode::NotApplicable,
        delivered_count: Some(0),
        ax_readback_ms: None,
        ax_readback: None,
    });
    result
}

#[cfg(test)]
mod tests {
    #[test]
    fn brief_foreground_is_off_unless_explicitly_enabled() {
        for off in [
            None,
            Some(""),
            Some("0"),
            Some("false"),
            Some("no"),
            Some("maybe"),
        ] {
            assert!(!super::brief_foreground_flag(off), "{off:?}");
        }
        for on in ["1", "true", "TRUE", " yes ", "on"] {
            assert!(super::brief_foreground_flag(Some(on)), "{on:?}");
        }
    }

    #[test]
    fn explicit_background_opts_out_of_the_brief_foreground_step() {
        use serde_json::json;
        assert!(super::background_requested_explicitly(
            &json!({"delivery_mode": "Background"})
        ));
        assert!(!super::background_requested_explicitly(&json!({})));
        assert!(!super::background_requested_explicitly(
            &json!({"delivery_mode": "foreground"})
        ));
    }

    use super::*;

    #[test]
    fn delivery_mode_parse_fronts_only_on_explicit_foreground() {
        for (args, expected) in [
            (
                json!({"delivery_mode": "background"}),
                DeliveryMode::Background,
            ),
            (
                json!({"delivery_mode": "foreground"}),
                DeliveryMode::Foreground,
            ),
            (
                json!({"delivery_mode": "Foreground"}),
                DeliveryMode::Foreground,
            ),
            (
                json!({"delivery_mode": "FOREGROUND"}),
                DeliveryMode::Foreground,
            ),
            (json!({}), DeliveryMode::Background),
            (
                json!({"delivery_mode": "garbage"}),
                DeliveryMode::Background,
            ),
            (json!({"delivery_mode": "auto"}), DeliveryMode::Background),
            (json!({"delivery_mode": null}), DeliveryMode::Background),
            (json!({"delivery_mode": 1}), DeliveryMode::Background),
        ] {
            let mode = DeliveryMode::from_args(&args);
            assert_eq!(mode, expected, "{args}");
            assert_eq!(mode.is_foreground(), expected == DeliveryMode::Foreground);
        }
    }

    #[test]
    fn background_unavailable_before_input_serializes_explicit_non_delivery() {
        let result = background_unavailable_before_input_result(
            "No addressed backend.",
            "background_unavailable",
            "focus-free route unavailable",
            json!({"detail": "no backend", "effect": "partial",
                "delivery": {"mode": "unknown", "delivered_count": 1}}),
        );
        assert!(
            result.action_record.is_none(),
            "no attempted transport to record"
        );
        assert_eq!(
            serde_json::to_value(result).unwrap(),
            json!({
                "content": [{"type": "text", "text": "No addressed backend."}],
                "isError": true,
                "structuredContent": {
                    "code": "background_unavailable",
                    "detail": "no backend",
                    "effect": "refused",
                    "delivery": {"mode": "not_applicable", "delivered_count": 0},
                    "suggestion": FOREGROUND_RETRY_SUGGESTION,
                    "escalation": {"recommended": "foreground", "reason": "focus-free route unavailable"}
                }
            })
        );
    }

    #[test]
    fn a_custom_suggestion_replaces_only_the_foreground_retry_text() {
        let result = background_unavailable_result_with_suggestion(
            "refused",
            "background_unavailable",
            "why",
            json!({}),
            "use get_window_state",
        );
        let structured = result.structured_content.unwrap();
        assert_eq!(structured["suggestion"], "use get_window_state");
        assert_eq!(structured["code"], "background_unavailable");
        assert_eq!(structured["escalation"]["recommended"], "foreground");
    }

    #[test]
    fn background_unavailable_result_carries_the_foreground_escalation() {
        let result = background_unavailable_result(
            "Background delivery is not available.",
            "background_occluded",
            "the target is occluded",
            json!({
                "event_kind": "mouse_click",
                "code": "forged",
                "suggestion": "forged",
            }),
        );
        assert_eq!(result.is_error, Some(true));
        assert_eq!(
            result.structured_content,
            Some(json!({
                "code": "background_occluded",
                "event_kind": "mouse_click",
                "suggestion": "Retry this action with delivery_mode:\"foreground\".",
                "escalation": {
                    "recommended": "foreground",
                    "reason": "the target is occluded",
                },
            }))
        );
        match &result.content[0] {
            crate::protocol::Content::Text { text, .. } => {
                assert_eq!(text, "Background delivery is not available.")
            }
            _ => panic!("expected text content"),
        }
    }
}
