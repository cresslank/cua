# cua WinRects: GNOME Shell helper extension (Wayland)

A small GNOME Shell extension that lets cua-driver get **pixel coordinates**,
address an incarnation-qualified target, perform a bounded foreground
transaction, capture only when the target is painted and unoccluded on the
current stage with no Shell input grab, and draw session-owned **agent cursors**
on GNOME Mutter Wayland. A
normal Wayland client cannot do these things globally.

It exposes `org.cua.WinRects` on the session bus:

- `GetVersion() -> uint`: this fork's helper API version (v17, with the v8
  semantic cursor/session-badge API). The driver accepts it only after resolving
  the helper's immutable D-Bus owner and proving that owner is the current
  user's system-installed `gnome-shell` process.
- `GetCapabilities() -> json`: an independent exact-target protocol version (4),
  helper epoch, and exact-target capabilities. Driver/helper version skew fails
  closed.
- `GetRects() -> json`: every window's epoch-qualified target id, workspace,
  monitor, sticky/visibility state, frame geometry, and surface-buffer
  origin. cua-driver combines the buffer origin with AT-SPI
  `CoordType::Window` per-widget coords: `screen = origin + window_xy`. This is
  the GNOME analogue of the X11 `_GTK_FRAME_EXTENTS` reconstruction (AT-SPI's
  `CoordType::Screen` is `(0,0)` for every widget on Mutter). Keeping the frame
  and buffer origins separate accounts for GTK client-side shadows.
- `BeginForeground(transaction,target) -> json` / `QueryForeground(transaction)` /
  `AbortForeground(transaction)` / `EndForeground(transaction)`: the caller
  allocates the transaction ID before activation, so a lost or delayed reply can
  be queried and reconciled before the host-wide raw-input lease is released.
  Activate one exact current-workspace target even when another ordinary window
  overlaps it, then require confirmed focus, visibility, no modal child, and no
  remaining occlusion. Restoration is terminal only after readback confirms the
  prior workspace/window, or after a third-party focus change is preserved.
  `CommitForeground` is reserved for an explicit
  keep-focused action. A Shell input grab means an active Shell modal count or
  key focus inside `Main.uiGroup`; ordinary application-surface key focus is not
  misclassified as Shell chrome.
- `CaptureTarget(target) -> capture_json`: capture only the
  exact target rectangle when it is painted, unoccluded by higher-stacked
  windows, and free of Shell modal/input grabs. The JSON result binds target
  id, epoch, geometry, stage size, and target-only PNG bytes to one Shell-side
  operation; no caller receives a broader stage image. Cua's in-process cursor
  actor is explicitly non-occluding. Unsafe requests fail closed.
- `MoveCursorFor(owner,target,x,y)` / `ClickPulseFor` / `HideCursorFor` /
  `RemoveCursor`: render independently owned cursors and hide them whenever
  their target is not actually visible on the active workspace. The D-Bus
  connection's unique owner, not the caller label alone, owns those actors;
  `NameOwnerChanged` cleanup reaps them after normal or abnormal process exit.
- `SetCursorState` / `SetCursorColor` / `SetSessionLabel` retain the upstream v8
  semantic cursor and host-owned session-badge API. Exact-target callers use the
  corresponding `...For(owner, ...)` variants so each connection-owned cursor
  keeps independent action, color, and label state.

It runs in the shell's privileged context, so **no xdg-desktop-portal grant** is
needed (unlike libei/RemoteDesktop).

## Install

The `local-hardened` metadata also admits GNOME Shell 50. Static packaging
passes on Shell 50, but the extension still requires a real-session smoke after
the first logout/login because Shell-only resources cannot be loaded by a
standalone `gjs` process.

`CaptureTarget` and `CaptureTargetPreview` capture the exact target actor through
`paint_to_content` and cursor-free `composite_to_stream`. GPU previews retain
atomic full-size and encoded-size metadata. Both routes require the same
painted/current, unoccluded target and revalidate identity and geometry after
capture; covered or off-stage targets remain refused. Legacy `Capture` and
`Activate` fail closed.

```
./install.sh          # stage only; does not enable or alter the live session
./install.sh --enable # explicit enable after warning the user
# LOGOUT/LOGIN REQUIRED ONCE (GNOME Wayland cannot safely reload Shell in place)
gnome-extensions info winrects@cua   # -> State: ACTIVE
```

For a distributable zip, first materialize its content-addressed closure:

```bash
python3 package.py /owned/staging/winrects@cua
gnome-extensions pack --extra-source=impl --extra-source=selected.json /owned/staging/winrects@cua
```

The first cached-monolith → stable-bootstrap deployment still needs one
logout/login. Routine implementation updates then use the bounded loader API,
not an extension toggle or Shell's private reload API. See
[loader ABI, closure format, retirement and installer transaction](HOT-RELOAD.md).
`install.sh` deliberately preserves an already-installed bootstrap and rollback
closures; the separately reviewed managed transaction owns routine selection.

cua-driver auto-detects it at runtime (`wayland::shell_helper`). AX operations
still work when it is absent. With an incompatible installed helper, exact
enumeration/capture fail closed rather than silently dropping to PID/title or
active-stage guessing. Background mode never authorizes foreground input.

## Transport and rollout policy

General RPC uses the upstream persistent typed transport with exact-target
methods and finite deadlines. Cursor actors retain their separate connection
owner and bounded dispatcher; foreground input still uses explicit transaction
IDs, validation, reconciliation, and restoration. Transport ownership does not
replace either contract.

Upstream libei `ei_connection.sync` acknowledgement is deferred. Its pending
reply map and independent five-second timer do not carry this fork's shared
command deadline or action lease. Completion continues to mean native socket
drain or connection discard, not compositor acknowledgement. Any ACK port must
retain the lease through terminal completion even for cancelled callers, and
must not reconnect or replay input; only readiness may initialize a connection.

The explicit rollout installer preserves a newer installed helper without
assuming protocol compatibility. The client still requires the exact-target
capability gate. Atomic complete-runtime installation, the stage-only default,
and refusal of malformed version metadata remain; automatic enablement and
`--force` replacement are not adopted.

## Workspace occlusion and reply compatibility (v15)

A higher-stacked window blocks capture only when its actor is visible, it is not
minimized, Mutter says it is showing on its workspace, and it is sticky or on the
active workspace. `showing_on_its_workspace()` alone is hypothetical visibility:
it can be true for a window on an inactive workspace. The same stage predicate
controls target discovery, capture, and target-bound cursor/badge visibility.
Activation does not require an already-painted actor, but remains restricted to
the active workspace (or sticky windows). A target's own non-minimized modal
child deliberately keeps the workspace-relative guard: activation must not
bypass that modal during mapping/workspace transitions.

The API bump identifies changed helper bytes; exact-target protocol 4 and
`unoccluded-target-v1` are unchanged because this repairs their existing promise.
The driver still accepts the v14 helper. v15 adds `target` and `transaction` to
negative foreground replies for an existing transaction. A stale transaction has
no target to echo. New drivers report correlated v14 BeginForeground reasons
when `target` is absent, but never accept missing identity as success and always
reject an explicitly different target/transaction. Legacy invalid validation
replies lacking both fields are diagnostic refusals only; valid replies require
both identities. Old drivers continue to accept v15 success replies and refuse
unsafe validation.

A row with `visible=true` and `capture_current=false` now yields
`capture_occluded`, rather than claiming that the window is not painted. This row
can also reflect an unsafe Shell context; the message does not assert overlap
as the only possible cause. Non-painted rows retain `capture_foreground_required`.
Neither code has a dedicated recovery-hint mapping or Rust E2E code assertion;
MCP capture errors remain `tool_invocation_failed` with the specific reason in
text. The downstream Hermes cua-jev plugin has no branch keyed on either code,
so no compatibility follow-up is required there.

## Native element coordinates (v16)

`GetRects.client_type` explicitly distinguishes native Wayland from XWayland.
For an exact native target, the driver uses `CoordType::Window` plus the buffer
origin even when GTK3 labels its surface-local coordinates as `Screen`. GTK4
instead reports widget-root Window coordinates: its attested application toolkit
metadata selects the frame origin rather than adding the surface shadow twice.
The same origin is used for snapshot, retained-object, and point-action bounds.
Public `elements[].frame` remains in screen coordinates; `screenshot_frame` subtracts
Mutter's frame origin and applies delivered pixels per logical frame unit,
including compositor resource scale and image resizing. For a floating GTK CSD
window this starts from `window_xy + buffer_origin - frame_origin` for GTK3.
The actual current origins are used in every window state: maximization can
remove the inset, while some GTK3 half-tiled surfaces retain a smaller buffer
margin. Equal origins naturally produce zero correction. The driver reports `frame_scale: 1`
when no image resize occurred. Element extents are not a scale measurement:
offscreen controls and shadows can extend beyond a screenshot at 1x.

The coordinate correction needs v16 metadata. Older helpers remain accepted for
existing exact-target operations, but cannot attest this native reconstruction;
missing/unknown client types, wrong PID/window pairs, and duplicate identities
never qualify it. XWayland keeps its genuine Screen extents. The initial stable
bootstrap still requires one logout/login; subsequent implementation-only
updates follow [the hot-reload transaction](HOT-RELOAD.md). Neither grants
background native input.

## Exact refusal diagnostics

The additive `refusal-reasons-v1` capability advertises exact diagnostic reasons,
not new input authority. `TargetNotActivatable` retains its D-Bus name and
`target_not_activatable` prefix; its message now includes `reason=...`.
`GetRects.capture_refusal_reason` and capture D-Bus errors distinguish
`overview_visible`, `session_locked`, `shell_modal_grab`, `shell_key_focus`,
`minimized`, `off_workspace`, and `modal_child`. Other visibility failures use
`not_showing`, `not_painted`, or `overlapped`. When several guards apply, Shell
context precedes window state. Reasons never dismiss Shell UI or move focus.

Required foreground and capture RPCs preserve the peer's exact D-Bus error name
and message through the driver refusal. A real deadline says `D-Bus call timed
out`; a method refusal no longer masquerades as a timeout or retires a healthy
connection. Refusal codes and existing reconciliation remain unchanged. Older
API15/API16 helpers remain usable; their missing diagnostic field is optional.
Updated helper bytes still require the approved helper activation boundary.

Foreground native right/double clicks remain refused. Mutter 50's EIS sync
acknowledges input-thread work, not necessarily final Clutter event routing;
closing a session does not join all already-enqueued virtual-input work. A safe
route needs a compositor-dispatch fence, exact transaction/batch correlation,
and a quarantine lease owner that survives driver loss and helper retirement.
End/Abort/watchdog restoration and transport close are not that proof. No new
input capability or native-terminal retirement hook is advertised here.

## Persistent window switching (v17)

`SwitchToTarget(transaction,target) -> json` advertises `persistent-switch-v1`.
It binds the transaction to the caller's unique bus owner and the exact helper
epoch/target. It refuses a locked/non-user session, concurrent foreground/switch
transactions, stale targets, and a target with a modal child. Only this explicit
persistent action may close overview, calendar/panel menus and a notification
banner, unminimize a window or activate another workspace through
`Main.activateWindow`. It never saves or restores prior focus. A bounded 1.2 s
poll requires the exact target to own compositor focus on the active workspace,
with no overview or Shell keyboard grab. Duplicates never replay activation.

`GetSwitchState() -> json` advertises `shell-search-focus-v1`. Readback includes
protocol/epoch, `overview_visible`, `session_locked`,
`search_entry_has_key_focus`, `key_focus_in_shell_ui`, `modal_count`,
`overlay_key_is_super` (the configured Mutter overlay binding), exact
`focus_window` token and `focus_pid`, plus foreground/persistent transaction-open
flags. An outstanding restoring/activating transaction refuses keyboard input.
The search-entry boolean is positive proof
that Shell's real overview entry owns Clutter key focus, not merely that the
overview is visible. `FocusSwitchSearch(target)` is a separate explicit action:
it may focus only the known mapped Shell search entry inside an unlocked,
visible overview with no extra modal or helper transaction. GNOME normally
forwards the first printable key to activate search; focusing the entry explicitly
avoids that unproven character. GetSwitchState remains read-only. Missing
API/state never authorizes keys.

`DeliverSwitchSearch(target,query)` and `CancelSwitchSearch(target)` advertise
`shell-search-delivery-v1`. Delivery rechecks the full Shell focus predicate,
exact target and matching running Shell application search result, then sets the
entry text and activates that result synchronously in the compositor main loop.
No match refuses `switch_unverified`; lost overview/focus refuses `shell_not_ready`.
Cancellation uses `Main.overview.hide()`. Neither method emits keyboard events.

The Linux `switch_window` keyboard route emits only the configured overlay Super
through a previously prepared RemoteDesktop/EIS transport (optional readiness
runs before the input lease for auto/keyboard). The worker rechecks the overlay
binding after queue/device waits. Query, Return and Escape never enter libei or a
virtual keyboard. The whole-action host-wide lease still serializes agents, while
receiver-owned synchronous delivery eliminates the physical-focus check/post
race for launcher contents. Attempts report `delivery: "shell_search"`; exact
PID/window focus readback remains mandatory for success. API 17 is unreleased;
the exact-target protocol and stable bootstrap bytes remain unchanged.
No other tool's GNOME desktop-input refusal or capture/foreground guard changes.

Implementation-only activation uses the existing reviewed hot-reload transaction;
the bootstrap bytes, loader ABI 1 and exact-target protocol 4 are unchanged.
The driver's existing full-width incarnation-qualified public IDs and collision
rejection remain unchanged. The helper's full epoch-qualified target token is
the exact-identity authority.

### Non-GNOME Wayland

wlroots compositors such as Sway and labwc do not need this helper: cua-driver
uses foreign-toplevel activation, virtual-pointer input, and layer-shell there.
KDE Plasma Wayland needs an equivalent target-addressable KWin activation
adapter; it is not yet provided. Portal reachability alone is insufficient
because RemoteDesktop/libei input is global to the compositor focus.

## Versions and other apps that bundle the helper

The UUID `winrects@cua` and session-bus name `org.cua.WinRects` are shared with
other apps, including T3 Code. This implementation reports helper API 17 in
`GetVersion()` and its closure manifest. `metadata.json` remains the stable
bootstrap release (16), not the selected implementation version. Exact-target authority is a separate
protocol 4 capability contract: a larger API version alone never proves it.
Legacy global capture/activation requests are refused, and clients must prove
the helper incarnation and exact target before requesting pixels or input.

The driver installers only bundle helper files for a later explicit rollout.
`install.sh` stages the first bootstrap atomically, refuses malformed installed
version metadata, never downgrades a newer helper, and enables only with
`--enable`. It never replaces an existing stable bootstrap or retained closure.
Bootstrap-byte changes require logout/login; implementation-only activation
uses the explicit transaction and bounded API in [HOT-RELOAD.md](HOT-RELOAD.md).
No binary installer/updater implicitly installs or reloads the helper.
