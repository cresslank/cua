//! Deny-only, session-owned input policy. Updates are an atomic set union, never
//! replacement. State survives end/revival of the same lifecycle identity and
//! is released only with this registry, so restarting a label cannot clear it.
use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use regex::{Regex, RegexBuilder};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::protocol::ToolResult;
use crate::tool::{Tool, ToolDef};

pub const CAPABILITY: &str = "session.input_policy.v1";
pub const TOOL: &str = "set_input_policy";
const MAX_RULES: usize = 256;
const MAX_PATTERN_BYTES: usize = 4096;
const MAX_SESSIONS: usize = 1024;

#[derive(Default)]
struct Policy {
    patterns: Vec<(String, Regex)>,
    combos: BTreeSet<BTreeSet<String>>,
    denied: BTreeSet<String>,
}

#[derive(Default)]
pub struct InputPolicies(Mutex<HashMap<String, Policy>>);

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Update {
    #[serde(default)]
    blocked_text_patterns: Vec<String>,
    #[serde(default)]
    blocked_key_combos: Vec<Vec<String>>,
    #[serde(default)]
    denied_tools: Vec<String>,
}

pub fn refusal(code: &str, message: &str) -> ToolResult {
    ToolResult::error(message).with_structured(json!({"ok": false, "code": code}))
}

fn invalid() -> ToolResult {
    refusal(
        "invalid_input_policy",
        "invalid or unsupported input policy; no policy was changed",
    )
}

fn canonical_tool(name: &str) -> &str {
    match name {
        "type_text_chars" => "type_text",
        other => crate::tool::renamed_tool(other).unwrap_or(other),
    }
}

fn canonical_key(key: &str) -> String {
    // Policy comparison is deliberately conservative. Native macOS names fold
    // separators, while common argument normalization also folds navigation
    // aliases on every OS. Never split a named key such as back-space first.
    let lower = if key == " " {
        "space".to_owned()
    } else {
        key.trim().to_lowercase()
    };
    let key = if lower.chars().count() > 1 {
        lower.replace(['_', '-', ' '], "")
    } else {
        lower
    };
    match key.as_str() {
        "command" | "cmd" | "⌘" | "windows" | "super" | "meta" | "win" | "mod4" | "superl"
        | "superr" | "metal" | "metar" => {
            if cfg!(target_os = "macos") {
                "cmd"
            } else {
                "win"
            }
        }
        "control" | "ctrl" | "ctl" | "⌃" | "controll" | "controlr" => "ctrl",
        "option" | "alt" | "opt" | "⌥" | "mod1" | "menu" | "altl" | "altr" => "option",
        "shift" | "⇧" | "shiftl" | "shiftr" => "shift",
        "del" | "forwarddelete" | "fwddelete" | "deleteforward" => "delete",
        "bs" | "bksp" => "backspace",
        "esc" => "escape",
        "enter" | "kpenter" | "numpadenter" => "return",
        "pgup" | "prior" => "pageup",
        "pgdn" | "pgdown" | "pagedn" | "next" => "pagedown",
        "arrowleft" | "leftarrow" => "left",
        "arrowright" | "rightarrow" => "right",
        "arrowup" | "uparrow" => "up",
        "arrowdown" | "downarrow" => "down",
        "ins" => "insert",
        "spacebar" => "space",
        "plus" => "+",
        "minus" | "dash" => "-",
        "equal" | "equals" => "=",
        "asterisk" | "star" => "*",
        "slash" => "/",
        "backslash" => "\\",
        "period" | "dot" => ".",
        "comma" => ",",
        "semicolon" => ";",
        "colon" => ":",
        "underscore" => "_",
        "parenleft" => "(",
        "parenright" => ")",
        "bracketleft" => "[",
        "bracketright" => "]",
        "apostrophe" | "quoteright" => "'",
        "grave" | "quoteleft" => "`",
        // macOS's delete key is backspace, unlike Linux/Windows forward delete.
        "delete" if cfg!(target_os = "macos") => "backspace",
        other => {
            if let Some(number) = other.strip_prefix('f') {
                if let Ok(n @ 1..=35) = number.parse::<u8>() {
                    return format!("f{n}");
                }
            }
            return other.to_owned();
        }
    }
    .to_owned()
}

fn keys<'a>(parts: impl IntoIterator<Item = &'a str>) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    for part in parts {
        let canonical = canonical_key(part);
        // Preserve literal +/- and hyphenated native names. Legacy combination
        // strings still split when the whole token is not a known single key.
        if comparable_key(&canonical) {
            keys.insert(canonical);
        } else {
            keys.extend(
                part.split(['+', '-'])
                    .map(canonical_key)
                    .filter(|key| !key.is_empty()),
            );
        }
    }
    keys
}

fn comparable_key(key: &str) -> bool {
    (key.len() == 1 && key.as_bytes()[0].is_ascii_graphic())
        || matches!(
            key,
            "cmd"
                | "win"
                | "ctrl"
                | "option"
                | "shift"
                | "fn"
                | "return"
                | "tab"
                | "space"
                | "backspace"
                | "delete"
                | "escape"
                | "insert"
                | "home"
                | "end"
                | "pageup"
                | "pagedown"
                | "up"
                | "down"
                | "left"
                | "right"
                | "capslock"
                | "numlock"
        )
        || key
            .strip_prefix('f')
            .is_some_and(|number| number.parse::<u8>().is_ok_and(|n| (1..=35).contains(&n)))
}

// Native parsers are not closed vocabularies: Windows falls back to the first
// UTF-16 character + VkKeyScan, and Wayland/wtype accepts arbitrary XKB names.
// Do not guess their meaning or silently rewrite an allowed call's effect.
// With key rules installed, admit only these audited wire spellings; everything
// else fails closed before the native adapter, even if it is a policy alias.
fn auditable_key_spelling(key: &str, modifier: bool) -> bool {
    let lower = key.to_ascii_lowercase();
    if modifier {
        return matches!(
            lower.as_str(),
            "ctrl"
                | "control"
                | "shift"
                | "alt"
                | "cmd"
                | "command"
                | "meta"
                | "win"
                | "windows"
                | "super"
                | "option"
                | "menu"
                | "fn"
        );
    }
    if lower.len() == 1 && lower.as_bytes()[0].is_ascii_alphanumeric() {
        return true;
    }
    if matches!(
        lower.as_str(),
        "return"
            | "enter"
            | "tab"
            | "escape"
            | "esc"
            | "space"
            | "backspace"
            | "delete"
            | "del"
            | "insert"
            | "ins"
            | "home"
            | "end"
            | "pageup"
            | "pgup"
            | "pagedown"
            | "pgdn"
            | "up"
            | "down"
            | "left"
            | "right"
            | "capslock"
            | "numlock"
            | "ctrl"
            | "control"
            | "shift"
            | "alt"
            | "cmd"
            | "command"
            | "meta"
            | "win"
            | "windows"
    ) {
        return true;
    }
    if matches!(lower.as_str(), "super" | "option" | "fn") {
        // Windows treats these as s/o/f through its first-character fallback.
        return !cfg!(target_os = "windows");
    }
    lower.strip_prefix('f').is_some_and(|number| {
        number
            .parse::<u8>()
            .is_ok_and(|n| (1..=12).contains(&n) && number == n.to_string())
    })
}

fn auditable_call_keys(tool: &str, args: &Value) -> bool {
    match tool {
        "hotkey" => args
            .get("keys")
            .and_then(Value::as_array)
            .is_some_and(|keys| {
                keys.iter().all(|key| {
                    key.as_str()
                        .is_some_and(|key| auditable_key_spelling(key, false))
                })
            }),
        "press_key" => {
            args.get("key")
                .and_then(Value::as_str)
                .is_some_and(|key| auditable_key_spelling(key, false))
                && args.get("modifiers").is_none_or(|modifiers| {
                    modifiers.as_array().is_some_and(|modifiers| {
                        modifiers.iter().all(|key| {
                            key.as_str()
                                .is_some_and(|key| auditable_key_spelling(key, true))
                        })
                    })
                })
        }
        _ => true,
    }
}

fn call_keys(tool: &str, args: &Value) -> BTreeSet<String> {
    let mut parts = Vec::new();
    if tool == "hotkey" {
        match args.get("keys") {
            Some(Value::Array(values)) => parts.extend(values.iter().filter_map(Value::as_str)),
            Some(Value::String(value)) => parts.push(value.as_str()),
            _ => {}
        }
    } else if tool == "press_key" {
        if let Some(key) = args.get("key").and_then(Value::as_str) {
            parts.push(key);
        }
        if let Some(modifiers) = args.get("modifiers").and_then(Value::as_array) {
            parts.extend(modifiers.iter().filter_map(Value::as_str));
        }
    }
    keys(parts)
}

impl InputPolicies {
    fn tighten(&self, session: &str, update: Update) -> ToolResult {
        if update.blocked_text_patterns.len() > MAX_RULES
            || update.blocked_key_combos.len() > MAX_RULES
            || update.denied_tools.len() > MAX_RULES
        {
            return invalid();
        }
        // Compile everything before taking the lock or changing any live rule.
        // Rust regex is linear-time and has no backreferences/lookaround.
        let mut patterns = Vec::new();
        for pattern in update.blocked_text_patterns {
            if pattern.is_empty() || pattern.len() > MAX_PATTERN_BYTES {
                return invalid();
            }
            let regex = match RegexBuilder::new(&pattern)
                .case_insensitive(true)
                .size_limit(256 * 1024)
                .dfa_size_limit(256 * 1024)
                .build()
            {
                Ok(regex) => regex,
                Err(_) => return invalid(), // Never echo parser diagnostics or policy text.
            };
            patterns.push((pattern, regex));
        }
        let mut combos = BTreeSet::new();
        for combo in update.blocked_key_combos {
            if combo.is_empty() || combo.len() > 16 || combo.iter().any(|key| key.len() > 64) {
                return invalid();
            }
            let combo = keys(combo.iter().map(String::as_str));
            if combo.is_empty() || !combo.iter().all(|key| comparable_key(key)) {
                return invalid();
            }
            combos.insert(combo);
        }
        let mut denied = BTreeSet::new();
        for tool in update.denied_tools {
            if tool.is_empty()
                || tool.len() > 128
                || !tool
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            {
                return invalid();
            }
            denied.insert(canonical_tool(&tool).to_owned());
        }
        let mut policies = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if !policies.contains_key(session) && policies.len() >= MAX_SESSIONS {
            return refusal(
                "input_policy_capacity",
                "input policy session capacity reached; no policy was changed",
            );
        }
        let current = policies.entry(session.to_owned()).or_default();
        patterns.retain(|(pattern, _)| !current.patterns.iter().any(|(old, _)| old == pattern));
        patterns.sort_by(|a, b| a.0.cmp(&b.0));
        patterns.dedup_by(|a, b| a.0 == b.0);
        combos.extend(current.combos.iter().cloned());
        denied.extend(current.denied.iter().cloned());
        if current.patterns.len() + patterns.len() > MAX_RULES
            || combos.len() > MAX_RULES
            || denied.len() > MAX_RULES
        {
            return invalid();
        }
        current.patterns.extend(patterns);
        current.combos = combos;
        current.denied = denied;
        ToolResult::text("Session input policy tightened.").with_structured(json!({
            "ok": true, "version": 1, "tighten_only": true,
            "blocked_text_patterns": current.patterns.len(),
            "blocked_key_combos": current.combos.len(),
            "denied_tools": current.denied.len()
        }))
    }

    pub(crate) fn enforce(
        &self,
        session: Option<&str>,
        tool: &str,
        args: &Value,
    ) -> Result<(), ToolResult> {
        let Some(session) = session else {
            return Ok(());
        };
        let policies = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let Some(policy) = policies.get(session) else {
            return Ok(());
        };
        let tool = canonical_tool(tool);
        let text = match tool {
            "type_text" | "clipboard_write" | "browser_type" => args.get("text"),
            "set_value" => args.get("value"),
            "browser_dialog" => args.get("prompt_text"),
            "page"
                if matches!(
                    args.get("action").and_then(Value::as_str),
                    Some("insert_text" | "type_keystrokes")
                ) =>
            {
                args.get("text")
            }
            _ => None,
        };
        let text = text.filter(|value| !value.is_null()).map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| value.to_string())
        });
        let blocked_text = text.is_some_and(|text| {
            let sanitized = crate::text_sanitize::strip_trailing_agent_protocol_tags(&text);
            policy
                .patterns
                .iter()
                .any(|(_, regex)| regex.is_match(&text) || regex.is_match(&sanitized))
        });
        let call_keys = call_keys(tool, args);
        if policy.denied.contains(tool)
            || blocked_text
            || (!policy.combos.is_empty() && !auditable_call_keys(tool, args))
            || policy
                .combos
                .iter()
                .any(|combo| combo.is_subset(&call_keys))
        {
            return Err(refusal(
                "input_policy_blocked",
                "the session input policy blocked this call",
            ));
        }
        Ok(())
    }
}

pub struct SetInputPolicyTool(pub Arc<InputPolicies>);

#[async_trait]
impl Tool for SetInputPolicyTool {
    fn def(&self) -> &ToolDef {
        static DEF: OnceLock<ToolDef> = OnceLock::new();
        DEF.get_or_init(|| ToolDef {
            name: TOOL.into(),
            description: "Tighten this session's deny-only input policy. Updates atomically UNION rules; omitted/empty arrays never remove rules. Rules survive end_session/start_session revival until this driver registry exits. Text patterns use case-insensitive Rust regex search (no lookaround/backreferences). Key combos are unordered subsets with case/separator and physical modifier aliases normalized. With key rules installed, unrecognized/opaque native key spellings fail closed; use ASCII letters/digits, named keys and f1..f12 (see session-input-policy.md). Applies at common dispatch including nested run_steps/run_script. No approval bypass. Set and verify this before exposing actions; repeat after reconnect/new session. Returns only effective rule counts, never pattern/input text.".into(),
            input_schema: json!({
                "type": "object", "additionalProperties": false,
                "properties": {
                    "blocked_text_patterns": {"type": "array", "description": "Additional case-insensitive Rust regex search rules. Atomically unioned; empty/omitted never clears existing rules.", "maxItems": MAX_RULES, "items": {"type": "string", "minLength": 1, "maxLength": MAX_PATTERN_BYTES}},
                    "blocked_key_combos": {"type": "array", "description": "Additional unordered key-combo subsets. Case, named-key separators, modifier aliases, order and +/- combinations normalize. Unknown key identities are invalid; opaque call spellings fail closed when key rules exist. Additive only.", "maxItems": MAX_RULES, "items": {"type": "array", "minItems": 1, "maxItems": 16, "items": {"type": "string", "maxLength": 64}}},
                    "denied_tools": {"type": "array", "description": "Additional exact canonical tool names to deny. Aliases normalize; unknown future tool names are accepted. Additive only.", "maxItems": MAX_RULES, "items": {"type": "string", "minLength": 1, "maxLength": 128, "pattern": "^[a-z0-9_]+$"}}
                }
            }),
            read_only: false, destructive: false, idempotent: true, open_world: false,
        })
    }

    async fn invoke(&self, mut args: Value) -> ToolResult {
        let session = args
            .get("_session_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let Some(session) = session else {
            return refusal(
                "session_required",
                "input policy requires an admitted session",
            );
        };
        let Some(object) = args.as_object_mut() else {
            return invalid();
        };
        object.retain(|key, _| key != "session" && !key.starts_with('_'));
        let update = match serde_json::from_value(args) {
            Ok(update) => update,
            Err(_) => return invalid(),
        };
        self.0.tighten(&session, update)
    }
}
