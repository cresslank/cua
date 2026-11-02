# Session input policy and nested cancellation

The session input policy is an extra **deny-only** driver gate, not an approval
system, risk ceiling, command sandbox, or output privacy filter. Existing
permission modes, capability manifests, per-tool validation, consent and desktop
leases still apply to each nested action.

## Probe and configure before exposing actions

1. Connect to the intended driver. `mcp --socket <private-or-managed-service>`
   selects a daemon; Linux/Windows plain `mcp` owns a different direct runtime.
2. Perform MCP `initialize` and `tools/list`. Require a `set_input_policy` tool,
   its advertised schema, and capability `session.input_policy.v1` on that tool
   and every composite you expose (`run_steps`, `run_script`). The CLI `manifest`
   also advertises `features.session_input_policy_v1` and
   `features.nested_action_cancellation`, but the connected server's inventory is
   authoritative. If support is absent, do not expose composites.
3. Choose a session label owned by this connection. Call `set_input_policy` and
   await a successful acknowledgement before publishing any action tool:

```json
{
  "name": "set_input_policy",
  "arguments": {
    "session": "hermes-run-unique-id",
    "blocked_text_patterns": [
      "curl\\s+[^|]*\\|\\s*bash",
      "curl\\s+[^|]*\\|\\s*sh",
      "wget\\s+[^|]*\\|\\s*bash",
      "\\bsudo\\s+rm\\s+-[rf]",
      "\\brm\\s+-rf\\s+/\\s*$",
      ":\\s*\\(\\)\\s*\\{\\s*:\\|:\\s*&\\s*\\}"
    ],
    "blocked_key_combos": [
      ["cmd", "shift", "backspace"],
      ["cmd", "option", "backspace"],
      ["cmd", "ctrl", "q"],
      ["cmd", "shift", "q"],
      ["cmd", "option", "shift", "q"],
      ["win", "l"],
      ["ctrl", "option", "delete"],
      ["ctrl", "option", "del"],
      ["option", "f4"]
    ],
    "denied_tools": ["clipboard_read"]
  }
}
```

The example imports Hermes' current hard blocks; the client owns keeping them in
sync. This driver does not hardcode Hermes' rules or weaken them by approval.
It accepts only the three rule arrays plus optional `session`. All arrays are
optional. Text patterns are nonempty strings up to 4096 UTF-8 bytes, compiled by
Rust `regex` with case-insensitive search, including Unicode support. Lookaround
and backreferences are unsupported and refused. Compile-size bounds also apply.
Key combos are arrays of 1 to 16 strings, each at most 64 UTF-8 bytes. Denied tool
names are exact lowercase ASCII `[a-z0-9_]+`, at most 128 bytes; future names are
allowed. Tool aliases such as `type_text_chars` and `run_actions` canonicalize to
their current names. Each effective rule family is limited to 256 distinct rules.
There are at most 1024 policy-bearing lifecycle identities per driver registry.

A successful tool result has `isError` absent/false and `structuredContent`:

```json
{
  "ok": true,
  "version": 1,
  "tighten_only": true,
  "blocked_text_patterns": 6,
  "blocked_key_combos": 9,
  "denied_tools": 1
}
```

Counts are effective **distinct canonical** rules. For example `cmd` and `win`
are the same physical modifier on each target OS, and `del` and `delete` are the
same key on Linux/Windows, so the example's actual combo count can be lower.
Do not assume the illustrative count is an exact acknowledgement for every OS.
No raw rules, matched text, public/private session IDs or key values are echoed.

## Tighten-only and isolation

Updates atomically union all three sets with the existing policy. Empty/omitted
arrays are no-ops, duplicates are idempotent, and there is no clear/replace/relax
operation. Invalid updates leave every effective rule unchanged. Rules survive
`end_session` and subsequent `start_session` revival of the same private
lifecycle identity. They are freed only when its registry exits. A new runtime,
new session label, or new connection's implicit session does not inherit policy.
A client must therefore set and acknowledge policy on **every new session or
reconnection**, before admitting actions; it must not let model-written
arguments select arbitrary unconfigured session labels. Using no label binds
the policy to the connection's implicit session, and subsequent actions must
also omit it. Empty and `"default"` labels are legacy anonymous spellings and
resolve to that same connection's implicit scope, never a shared global bucket.
Named and implicit sessions are separate scopes.

The trusted runtime namespace prevents cross-runtime label collisions. Both
composites inherit their parent session and authorization context. Scripts and
steps cannot configure policy or change session; each actual nested driver call
re-enters the same common dispatch gate. Concurrent tightening closes admission
for actions not yet entered; it does not undo or forcibly revoke an already
admitted native action. Session policy changes should normally be serialized
with action publication by the embedding client.

## Input coverage and error shape

Text search covers `type_text.text` (including `type_text_chars`),
`set_value.value`, `clipboard_write.text`, `browser_type.text`,
`browser_dialog.prompt_text`, and `page.text` for `insert_text` and
`type_keystrokes`. Both the raw text and the platform's protocol-tag-stripped
text are checked. Non-string set values use their JSON representation. Rule
checks run before protected consent/recording and again after admission waits,
before invoking the platform adapter.

`hotkey.keys` and `press_key.key` + `modifiers` share one key guard. Key order and
duplicates do not matter. A blocked combo is an unordered **subset**: extra keys
cannot evade it. Policy rules fold case and spaces/underscores/hyphens within
named keys (`back_space`, `back-space`, `BACK SPACE` all mean `backspace`),
modifier aliases/symbols, navigation aliases and Return/Enter/Escape spellings.
Named keys take precedence over legacy `+`/`-` combination strings, so a hyphen
inside `back-space` is not a chord separator. Literal `+` and `-` remain keys.
On macOS `delete` is backspace; `del`/`forward_delete` is forward delete.
Unidentifiable rule keys are atomically refused with `invalid_input_policy`.

**Spelling restriction with key rules:** once at least one key rule exists,
keyboard calls must use audited named-key spellings or ASCII letters/digits.
Opaque native fallbacks, punctuation (literal or named), arbitrary Unicode,
raw/numeric X keysyms, keypad/XKB-specific names and unknown names are refused
with `input_policy_blocked`, even when no listed combo matches. A native parser
may accept these spellings, but the common gate cannot prove their physical
identity without the native layout. No key arguments are silently rewritten by
this gate. This restriction does not apply to sessions without key rules,
including text-only policies. It does not restrict `type_text` Unicode input.

The admitted call vocabulary is case-insensitive ASCII: `a`–`z`, `0`–`9`,
`f1`–`f12`, `return`/`enter`, `tab`, `escape`/`esc`, `space`, `backspace`,
`delete`/`del`, `insert`/`ins`, `home`, `end`, `pageup`/`pgup`,
`pagedown`/`pgdn`, `up`, `down`, `left`, `right`, `capslock`, `numlock`,
`ctrl`/`control`, `shift`, `alt`, `cmd`/`command`/`meta`/`win`/`windows`.
`super`, `option` and `fn` as keys are also admitted outside Windows; Windows
would otherwise interpret them as their first letter. Modifier arrays additionally
accept `super`, `option`, `menu` and `fn`. Native support still varies: admission
is not a claim that every OS supports every named key. The existing common
argument normalizer still accepts shapes such as `ctrl+s` and navigation names
such as `Page_Down`/`ArrowLeft` before this spelling check.

See [the native key alias audit](native-key-alias-audit.md) for parser-specific
aliases and the reason the common gate refuses open-ended native spellings.

A refused input or denied tool returns an MCP tool error, not a JSON-RPC
transport error:

```json
{
  "isError": true,
  "content": [{"type": "text", "text": "the session input policy blocked this call"}],
  "structuredContent": {"ok": false, "code": "input_policy_blocked"}
}
```

The normal common recovery layer may add a content-free `hint`. Invalid policies
return `invalid_input_policy`; capacity returns `input_policy_capacity`; no
admitted session returns `session_required`. Existing authorization/lifecycle
errors can take precedence. A batch stops at the failed action and publishes the
same code in `structuredContent.steps[i].code`. A failed script host call throws
`CuaError` with `.code === 'input_policy_blocked'`; uncaught errors publish that
code in `structuredContent.error.code`. Catching it permits other allowed work,
never the refused effect. Driver-generated refusal text and logs do not include
the matched input. Script-authored return/console/error strings are untrusted
output and can of course repeat their own source literals.

## Cancellation

Send the standard MCP notification while the request is pending:

```json
{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":17}}
```

IDs match exactly, including numeric versus string type, on that transport only.
Unknown/completed IDs do not cancel future ID reuse. Queued ordinary requests
remain FIFO and can be cancelled before starting. Matching cancellation returns
JSON-RPC error `-32800`, fixed message, `error.data.code = request_cancelled`.
Direct stdio and daemon proxy keep servicing cancellation/EOF during tool calls.
Proxy cancellation closes its exact async per-call socket; the daemon observes
that EOF during dispatch. Transport closure drops its in-flight future and ends
only its own sessions. HTTP and typed-envelope MCP carry the same scoped control.

Common dispatch checks cancellation between nested actions and after admission
waits. Composite execution observes session-end/request fences during sleeps,
locator waits and pure JS, on a 10 ms cooperative polling interval. Session end
(including pending teardown before the last dispatch drains) returns tool error
`session_ended`. Cooperative non-MCP request cancellation can return tool error
`request_cancelled`. Native input already admitted may finish; existing owned
worker/lease, key release, focus restoration and cleanup guards remain intact.
Native scopes also retain the original session admission through queued work,
native continuations, payload destruction and release/restoration. A cancelled
waiter's reply does not complete session teardown: session-end hooks stay deferred
until those owners drain, including after a native deadline expires.
Cancellation is not rollback and does not prove that an in-flight native action
had no effect. Verify receiver state before retrying an ambiguous action.

A Python MCP client must keep the read loop alive, send cancellation on its
existing connection, use the exact active request ID, and not merely abandon the
await while retaining the transport. On teardown close/drain the MCP transport;
a reconnect is a new admission boundary, not an automatic replay. Bound client
waits and reconfigure policy before restoring composite availability.

## Output privacy and non-goals

Input policy does not sanitize script results. A script can return arbitrary
JSON, console text and retained images; `getState()` exposes the underlying
`get_window_state` tree/diff, and queries can extract labels/values. Those trees
can include an application menu bar (for example Recent Items). Clipboard reads
are not on the script allowlist, but top-level clipboard access still exists
unless denied. The client must retain secret redaction and menu-bar privacy.
Post-hoc filtering arbitrary script JSON cannot reliably establish tree
provenance. Until a before-engine observation privacy filter is available, deny
`get_window_state` (which also fences locator/query reads) and other sensitive
read tools, or do not expose scripts that can observe those data to a third-party
model. Do not forward raw script output under an assumption that this input
policy makes it private.

Regexes inspect each supplied payload, not the receiver's accumulated document
or shell buffer. They do not prevent character-by-character construction,
pointer/menu equivalents, arbitrary page JavaScript, or alternative workflows
with the same effect. Deny unsuitable tools and retain the external plan/lease
and hard-block gates. This feature prevents nested payloads from skipping the
same per-call rules; it is not a malicious-program execution sandbox.
