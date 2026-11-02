# Native keyboard spelling audit

Scope: the `press_key`/`hotkey` parsers reached after common dispatch. This is a
source audit, not native desktop qualification. No native parser or input
transport is changed by the session-policy spelling fix.

## Enforcement choice

Native names are not one portable, closed vocabulary. The common policy gate
therefore **rejects unprovable spellings before native dispatch** when any key
rule exists. It does not rewrite an allowed call to a different physical key.
This deliberately includes punctuation and arbitrary Unicode: Windows resolves
characters through a live layout and truncates the fallback character to UTF-16;
macOS also has a special `plus`/`+` → Shift+Equals path. Guessing the physical key
from those strings would leave another alias bypass.

The exact admitted call vocabulary and unchanged `input_policy_blocked` refusal
are documented in [session input policy](session-input-policy.md#input-coverage-and-error-shape).
Text-only/no-key-policy sessions retain their previous native vocabulary.
Unknown rule identities fail atomically with `invalid_input_policy`; known
policy aliases canonicalize even when the same spelling is refused as a call.
A rule alias is not a promise that a platform supports that spelling for input.

The rule canonicalizer retains single-character `+`, `-` and `_`, folds
spaces/underscores/hyphens in multi-character names before considering chord
separators, and normalizes the following equivalence families. Deny matching is
conservative; keypad Enter is grouped with Return, and left/right modifier names
are grouped with the corresponding modifier rather than granting a distinction.

- Command: `cmd`, `command`, `win`, `windows`, `super`, `meta`, `mod4`,
  `Super_L/R`, `Meta_L/R`, `⌘`.
- Control: `ctrl`, `control`, `ctl`, `Control_L/R`, `⌃`.
- Option: `alt`, `option`, `opt`, `mod1`, `menu`, `Alt_L/R`, `⌥`.
- Shift: `shift`, `Shift_L/R`, `⇧`; `fn` remains its own identity.
- Backspace: `backspace`, `bs`, `bksp`; macOS additionally `delete`.
- Forward Delete: `del`, `forwarddelete`, `fwddelete`, `deleteforward`;
  Linux/Windows additionally `delete`.
- Return: `return`, `enter`, `kpenter`, `numpadenter`; Escape: `escape`, `esc`.
- Page Up: `pageup`, `pgup`, `prior`; Page Down: `pagedown`, `pgdn`, `pgdown`,
  `pagedn`, `next`; direction names also accept `arrow<direction>` and
  `<direction>arrow`; Insert accepts `ins`; Space accepts `spacebar`/literal space.
- Function names `f1`–`f35` canonicalize numeric spellings accepted by the
  targeted X11 parser (such as `f01`, `f+1`); calls under key rules are limited
  to exact `f1`–`f12`, ignoring ASCII case.
- Punctuation rule aliases: `plus`; `minus`/`dash`; `equal`/`equals`;
  `asterisk`/`star`; `slash`; `backslash`; `period`/`dot`; `comma`;
  `semicolon`; `colon`; `underscore`; `parenleft/right`; `bracketleft/right`;
  `apostrophe`/`quoteright`; `grave`/`quoteleft`. All punctuation calls are
  refused under key rules rather than guessing a layout-dependent keycode.

## Shared ingress and composites

[`tool.rs`](../rust/crates/cua-driver-core/src/tool.rs), `canonical_key_name` and
`normalize_key_args`, already normalize Page Up/Down (`pgdn`, `pgdown`,
`pagedn`, `pgup`), Arrow directions, and Forward Delete aliases after dropping
spaces/underscores/hyphens and folding Unicode case. A press-key `ctrl+s`
becomes a base key plus modifiers; a hotkey string splits on `+`. The policy
checks the resulting arguments both before waits and immediately before adapter
invocation. Its own legacy `+`/`-` comparison does not authorize an opaque native
spelling.

[`batch_tools.rs`](../rust/crates/cua-driver-core/src/batch_tools.rs),
`rewrite_step`, additionally turns single-key hotkeys into press-key steps, key
names used as tool names into press-key steps, and unmodified printable symbol
steps into `type_text`. The last case is a text action, not key injection: it
retains text-pattern enforcement and is outside key-combo enforcement, like an
explicit `type_text` call. Regression key-spelling cases use an explicit
modifier when necessary to retain the keyboard route. QuickJS calls re-enter
common dispatch without a separate native key parser.

## macOS

[`input/keyboard.rs`](../rust/crates/platform-macos/src/input/keyboard.rs):
`canonical_key_name`, `key_name_to_code`, `modifier_flags`,
`modifier_key_code_and_flag`, `press_key`.

- Named keys trim, Unicode-lowercase, and remove all ASCII spaces, underscores
  and hyphens from multi-character names. This includes arbitrary separator
  placement (`back_space`, `b-a-c-k-s-p-a-c-e`, `c_m_d`, `f_1`). Single characters
  remain single keys; Unicode case equivalence such as Kelvin sign → `k` also
  reaches the native table. The key-policy call fence rejects those noncanonical
  spellings unless earlier common navigation normalization made them canonical.
- Native aliases: Return/Enter → 36; Delete/Backspace/Bksp → 51;
  Escape/Esc → 53; Command/Cmd/Super/Meta/Win → 55; Option/Alt → 58;
  Control/Ctrl → 59; Fn → 63; Page Up/PgUp and Page Down/PgDn/PgDown;
  Arrow directions in either order; Del/ForwardDelete/FwdDelete/DeleteForward
  → 117; KPEnter/NumpadEnter → main Return, **not** a separate keypad code.
- Other table entries are Tab, Space, Shift, Caps Lock, Home/End, F1–F12,
  ASCII letters/digits and unshifted US punctuation. `bs`, `ctl`, `opt`,
  `windows`, left/right modifier names and modifier glyphs are not extra native
  keyboard aliases, even though the policy accepts them as rule aliases.
- Modifier parsing is a separate case-folded exact table: Cmd/Command/Super/
  Meta/Win, Shift, Option/Alt, Ctrl/Control, Fn. It does not compact separators.
  Tool hotkey partitioning uses its own exact modifier classifier; the policy
  conservatively counts all supplied keys, so ignored extra keys cannot evade a
  blocked subset.
- `press_key` and the tool wrapper special-case `plus`/`+` as Shift+Equals;
  other native helper routes do not all share that special case. Both spellings
  are refused under key rules. Menu-equivalent matching uppercases one Unicode
  base character; non-ASCII calls are likewise refused under key rules.

## Windows

[`input/keyboard.rs`](../rust/crates/platform-windows/src/input/keyboard.rs):
`key_name_to_vk`, `resolve_press`, `modifier_vk`.

- ASCII letters/digits resolve directly to their virtual-key codes for chords.
  Unmodified printable single characters instead use the target thread's layout
  and can fall back to Unicode text.
- The Unicode-lowercased named table aliases Enter/Return, Escape/Esc,
  Space/literal space, Delete/Del, Insert/Ins, PageUp/PgUp, PageDown/PgDn,
  Ctrl/Control, and Win/Windows/Meta/Command/Cmd. Other named entries: Tab,
  Backspace, Home/End, arrows, F1–F12, Shift, Alt, CapsLock and NumLock.
- **The fallback does not require one character.** Any unknown string takes its
  first original character, truncates it to `u16`, and calls `VkKeyScanW`, then
  uses the low-byte VK. Thus `lorem` can mean `l`; `super`, `option`, `fn`
  can mean `s`, `o`, `f`; a non-BMP character can alias an unrelated BMP value.
  These are rejected, not normalized to the name the caller may have intended.
  The same gate rejects separator variants, arbitrary Unicode and punctuation
  whose physical identity depends on the target/daemon keyboard layout.
- The XAML/UIA accelerator route in
  [`uia/windows_enum.rs`](../rust/crates/platform-windows/src/uia/windows_enum.rs),
  `canonical_accelerator_token`, separately trims, ASCII-folds and drops spaces
  and hyphens (not underscores). Its aliases are Control → Ctrl,
  Windows/Window/Meta/Cmd/Command → Win, Return → Enter, Esc → Escape,
  Del → Delete, Ins → Insert, PgUp/PageUp, PgDn/PageDown and Spacebar/Space.
  It splits `+`, sorts/deduplicates Ctrl/Shift/Alt/Win, and compares provider
  accelerator strings or parenthesized name hints. `window` and opaque token
  spellings are rejected by the policy call fence before this semantic route.
- Modifier aliases are Ctrl/Control, Shift, Alt/Menu/Option, and
  Win/Meta/Windows/Cmd/Command. The Windows tool's hotkey partitioner only
  recognizes Ctrl/Control/Shift/Alt/Win/Windows/Cmd/Command; it does not treat
  Meta/Menu/Option as modifiers in a hotkey array. Unknown modifiers are skipped.
  Super and Fn are
  not Windows native modifier aliases; allowing those modifier-array entries
  cannot add a hidden key, and the policy conservatively counts them.

## Linux X11

[`input/mod.rs`](../rust/crates/platform-linux/src/input/mod.rs),
`key_name_to_keysym`, is shared by ordinary X11 and MPX injection. It folds
Unicode case, aliases Return/Enter, Escape/Esc, Space/literal space, Delete/Del,
Insert/Ins, PageUp/PgUp/Page_Up, PageDown/PgDn/Page_Down, Ctrl/Control, and
Super/Meta/Win/Cmd. Other named entries: Tab, Backspace, Home/End, arrows,
F1–F12, Shift, Alt, CapsLock, NumLock. Punctuation names are Plus, Minus/Dash,
Equal/Equals, Asterisk/Star, Slash, Backslash, Period/Dot, Comma, Semicolon,
Colon, Underscore and ParenLeft/Right. Its fallback accepts one **byte**, not
arbitrary Unicode. Punctuation is refused by key policy regardless of X11
layout/remapping.

[`input/targeted.rs`](../rust/crates/platform-linux/src/input/targeted.rs):
`x_key_name` adds Arrow directions, Command/Cmd → Super, Option → Alt.
`named_key_keysym` additionally accepts Spacebar, Option and Command and
function-key numbers 1–35 through Rust integer parsing (including leading zeros
and `+`). `resolve_key_keysym` also accepts raw `keysym:0x<hex>` and any single
Unicode character (Latin-1 or Unicode keysym encoding). Those raw/opaque call
spellings are refused by the common gate. Targeted modifier aliases are Shift,
Control/Ctrl, Alt/Option, Command/Cmd/Super/Win/Meta.

## Linux Wayland

[`wayland/mod.rs`](../rust/crates/platform-linux/src/wayland/mod.rs):

- `partition_modifiers`: Ctrl/Control, Alt, Shift, Super/Meta/Cmd/Command/Win/
  Windows; last remaining non-modifier is the key.
- `key_to_keysym` maps those modifiers plus Return/Enter, Tab, Esc/Escape,
  Space, Backspace, Delete/Del, arrows, Home/End, PageUp/Page_Up → Prior,
  PageDown/Page_Down → Next. **Other names pass unchanged to `wtype`**, exposing
  XKB's open-ended keysym vocabulary (including left/right modifiers, keypad,
  ISO, XF86, Unicode and numeric spellings). A partial alias table cannot fence
  that vocabulary; everything outside the common admitted spelling list is
  refused with key rules installed.
- `key_to_evdev`, used by portal/libei, native virtual-keyboard and Hyprland
  injection, has a finite lowercase table: the same modifier/navigation aliases,
  ASCII letters/digits and F1–F12. Punctuation aliases are Minus/Dash,
  Equal/Equals, BracketLeft/Right, Semicolon, Apostrophe/QuoteRight,
  Grave/QuoteLeft, Backslash, Comma, Period/Dot and Slash (plus literal glyphs).
  Shifted glyphs require explicit Shift; there is no NumPad/Fn/Unicode fallback.
- `validate_injectable_key`/`validate_injectable_hotkey` for the nested compositor
  trim and ASCII-fold an explicit whitelist: Enter/Return, Tab, Escape/Esc,
  Backspace, Space, arrows and F1–F12; hotkeys also allow one ASCII base byte.
  Modifier aliases are Ctrl/Control, Shift, Alt/Option, Meta/Super/Win/Cmd.
  The wire/compositor has no wider key alias parser admitted by these functions.
- [`wayland/hyprland_input.rs`](../rust/crates/platform-linux/src/wayland/hyprland_input.rs),
  `Action::Key`, uses `key_to_evdev` plus a distinct ASCII-folded modifier
  table: Shift, Ctrl/Control, Alt/Option, Super/Meta/Cmd/Command/Win. Native
  virtual-keyboard uses the already-partitioned Ctrl/Shift/Alt/Logo names with
  `key_to_evdev`. Portal/libei accepts the same partitioned modifier vocabulary.
  Neither adds an open-ended key-name resolver.

## Owner coverage and limits

`cua-driver-core/tests/input_policy_dispatch.rs` owns spelling policy at the
public registry boundary, using a semantic receiver rather than a native input
adapter. It covers rule/call normalization, exact refusals, direct calls, real
`run_steps`, real QuickJS, zero receiver effects, permitted-key controls,
text-only compatibility and atomic invalid-rule updates. Deliberate old-source
and narrow-mutant assertion failures qualify the keeper.

This audit does not claim native compilation or desktop delivery on macOS,
Windows or any compositor. Policy identity is per call, not a sandbox against
text construction, layout remapping, pointer/menu equivalents or alternate
workflows with the same effect. Those remain the documented policy non-goals.
