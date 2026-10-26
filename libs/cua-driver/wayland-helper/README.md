# cua WinRects: GNOME Shell helper extension (Wayland)

A small GNOME Shell extension that lets cua-driver get **pixel coordinates**,
address an incarnation-qualified target, perform a bounded foreground
transaction, capture only when the target is painted and unoccluded on the
current stage with no Shell input grab, and draw session-owned **agent cursors**
on GNOME Mutter Wayland. A
normal Wayland client cannot do these things globally.

It exposes `org.cua.WinRects` on the session bus:

- `GetVersion() -> uint`: this fork's helper API version (v16, with the v8
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

When producing a distributable bundle, include the pure policy module:

```bash
gnome-extensions pack --extra-source=policy.js winrects@cua
```

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
never qualify it. XWayland keeps its genuine Screen extents. New helper files
still require the normal logout/login boundary; this change does not reload the
live Shell or grant background native input.

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

## Other compositors

wlroots compositors such as Sway and labwc do not need this helper: cua-driver
uses foreign-toplevel activation, virtual-pointer input, and layer-shell there.
KDE Plasma Wayland needs an equivalent target-addressable KWin activation
adapter; it is not yet provided. Portal reachability alone is insufficient
because RemoteDesktop/libei input is global to the compositor focus.

## Versions and other apps that bundle the helper

The UUID `winrects@cua` and session-bus name `org.cua.WinRects` are shared with
other apps, including T3 Code. This bundle reports helper API 16 in both
`GetVersion()` and `metadata.json`. Exact-target authority is a separate
protocol 4 capability contract: a larger API version alone never proves it.
Legacy global capture/activation requests are refused, and clients must prove
the helper incarnation and exact target before requesting pixels or input.

The driver installers only bundle helper files for a later explicit rollout.
`install.sh` stages the complete runtime atomically, refuses malformed installed
version metadata, never downgrades a newer helper, and enables only with
`--enable`. Activation of changed helper bytes requires the normal logout/login
boundary. No installer or updater implicitly installs or reloads the helper.
