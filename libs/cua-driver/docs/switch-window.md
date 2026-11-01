# Persistent window switching

`switch_window` (Linux and macOS) deliberately changes foreground focus and
**does not restore** the previously active app or workspace. Use it only when a
persistent switch is intended; ordinary background input remains background-first.
It is reversible, non-destructive, idempotent, and advertises `window.activate`.

```json
{"pid":1234,"window_id":5678,"method":"auto","query":"Application Name"}
```

- `pid`: required positive process identifier.
- `window_id`: optional positive window ID from fresh `list_windows` discovery.
  GNOME requires it when the PID has multiple toplevels. Existing full-width
  GNOME IDs are preserved, not remapped or truncated.
- `method`: `auto` (default), `native`, or `keyboard`. `auto` tries native first
  and falls back only after refusal or unverified focus. Lock/vanished-target
  refusals never authorize keyboard input. Explicit methods use only that route.
- `query`: optional, nonempty search text without control characters. Defaults
  to the target app's display name (desktop-entry Name on Linux, localized name
  on macOS). GNOME keyboard queries are limited to 256 Unicode characters;
  Shell-owned delivery does not depend on the active keyboard layout.
- `session`: the driver's standard optional session label. Other public fields
  are rejected (`additionalProperties: false`).

Successful structured content:

```json
{"route":"native","pid":1234,"window_id":5678,"focused":true,"attempts":[{"route":"native","outcome":"focused"}],"platform":"linux","session":"wayland","effect":"verified"}
```

`route` is `native`, `keyboard`, or `already_focused`. Attempts retain their
ordered `route`, `outcome` (`focused`, `refused`, `unverified`, `skipped`), and
optional `reason` and `delivery` (`shell_search` for GNOME keyboard attempts). `session` is present only on Linux (`wayland` or `x11`).
`effect` is `verified`, `unverifiable`, or `refused`. `focused` is compositor/OS
readback, never an activation-request receipt. No verified focus means an MCP
error with structured `code`: `session_locked`, `window_not_found`,
`target_ambiguous`, `switch_unverified`, `keyboard_unavailable`, `shell_not_ready`,
or `unsupported`. Malformed arguments report `invalid_arguments` before OS work.

## Routes and safety boundaries

GNOME Wayland native switching requires helper capability `persistent-switch-v1`
and readback `shell-search-focus-v1`, with protocol 4 and the same attested helper
owner/epoch throughout. `SwitchToTarget(transaction,target)` dismisses Shell
chrome, unminimizes, activates the target workspace/window, and checks exact
compositor focus. It is separate from temporary foreground transactions and
never restores focus. `GetSwitchState()` reports lock, overview/search focus,
Shell focus, current exact target/PID, overlay-key configuration and busy state.
`FocusSwitchSearch(target)` explicitly focuses Shell's known search entry only
inside an unlocked, visible overview with no additional modal or transaction.
GNOME otherwise focuses search by forwarding the first printable key; this
explicit Shell-only step avoids sending an unproven first character.
`DeliverSwitchSearch(target,query)` revalidates the full Shell predicate and
sets search text plus activates the matching running application result in one
synchronous compositor-main-loop operation. It uses the Shell application search
for the exact query and the target window's tracked application identity; no
matching application means `switch_unverified`, never an arbitrary first result.
`CancelSwitchSearch(target)` hides overview with the Shell API, never Escape.
Capability `shell-search-delivery-v1` is mandatory for the keyboard route.
Helper API 17 adds these capabilities; the loader ABI and protocol stay unchanged,
and the stable `extension.js` bootstrap is unchanged. A separately approved
helper rollout is required; updating the binary never updates the live helper.

Keyboard switching opens the launcher with real Mutter RemoteDesktop/libei
Super or macOS HID Cmd+Space. Query, commit and cancel are receiver-routed,
not global input. GNOME prepares an optional keyboard
transport before acquiring input leases for `auto`/`keyboard`; failure does not
block native switching. Explicit `native` never opens an input session. No
reconnection occurs while switching owns the seat. The host-wide GNOME lease
covers resolution, activation, the entire keyboard sequence and cleanup.

GNOME's only libei key is Super, permitted only with the default `Super` or
explicit `Super_L` Mutter overlay binding (also checked in the command worker).
Query, Return and Escape never enter the virtual keyboard. A physical focus
change after proof is either processed before the synchronous Shell RPC and
refused, or after receiver-owned delivery; it cannot redirect text into a client.

macOS reads the effective `com.apple.symbolichotkeys` preference domain. An absent
`AppleSymbolicHotKeys` dictionary (including a missing plist), an empty dictionary,
or absent entry 64 uses macOS's enabled Command-Space default. A present entry 64
must be explicitly enabled with standard parameters `[32,49,1048576]`; disabled,
remapped or malformed settings and preference-synchronization failures refuse
with `keyboard_unavailable` and zero input. The shortcut consists of back-to-back
Command-flagged Space down/up events, with no separate modifier transitions or
sleeps. All later query/select-all/Return/Escape events use `CGEventPostToPid` to the retained
Spotlight process. Every delivery revalidates bundle, process incarnation,
AX search-field identity and WindowServer focus; focus loss cannot redirect
already-authorized input into the newly focused client. Exact focus readback
remains the success authority. The preference-check/shortcut and numeric-PID
incarnation-check/post boundaries are not OS-atomic locks. All other GNOME
desktop-global input refusals remain in force.

X11 supports native unminimize/`_NET_ACTIVE_WINDOW` with actual keyboard-focus
readback. Its keyboard route is unsupported. Other Wayland compositors and
Windows do not expose this tool's new switching routes.

## Private GNOME qualification

The opt-in fixture lives in
`rust/crates/cua-driver-e2e/fixtures/gnome-switch`. It takes the reviewed
`run_shell_lab.sh` runner explicitly, copies its fixtures into owned scratch,
and preserves its private session/system buses, dconf guard, AT-SPI and Mutter
EIS relay. It never targets the host display or installed helper/driver.

```sh
python3 rust/crates/cua-driver-e2e/fixtures/gnome-switch/run.py \
  /path/to/reviewed/run_shell_lab.sh wayland-helper/winrects@cua \
  /path/to/candidate/cua-driver "$TMPDIR"
```

Assertions cover minimized, off-workspace and overview native activation, a cold
keyboard route, pre-proof and deterministic post-proof/pre-delivery Shell-focus
loss refusal, exact compositor PID/window readback,
and zero query/Return/Escape key presses at both GTK client receivers. Mutter
may forward the initial Super press before consuming its release; this explicit
contract exception is counted separately in every receipt. Full MCP receipts and Shell,
receiver, relay and private-session logs are retained in `SWITCH_LAB_EVIDENCE`.
The matching ignored `switch_window_gnome_test` is in the manual E2E allowlist.
The fixture's copied runner checks actual Cua Cargo/lane processes, not unrelated
Rust repositories, and records any contender's cwd. On a shared qualification
host, hold the coordinator's `native.lock` for the entire invocation (and builds
and lanes); a process waiting on that lock is not an executing contender.
Compile it with `cargo test --no-run`, then run the emitted test executable
with `--ignored` directly: the private runner intentionally refuses while any
Cargo/gate/E2E job is active. macOS native/input acceptance requires a separate
supervised host run; pure tests and compilation do not establish TCC or GUI acceptance.
