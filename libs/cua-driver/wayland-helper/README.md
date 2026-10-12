# cua WinRects: GNOME Shell helper extension (Wayland)

A small GNOME Shell extension that lets cua-driver get **pixel coordinates**,
address an incarnation-qualified target, perform a bounded foreground
transaction, capture only when the target is painted and unoccluded on the
current stage with no Shell input grab, and draw session-owned **agent cursors**
on GNOME Mutter Wayland. A
normal Wayland client cannot do these things globally.

It exposes `org.cua.WinRects` on the session bus:

- `GetVersion() -> uint` — this fork's helper API version (v14, with the v8
  semantic cursor/session-badge API). The driver accepts it only after resolving
  the helper's immutable D-Bus owner and proving that owner is the current
  user's system-installed `gnome-shell` process.
- `GetCapabilities() -> json` — an independent exact-target protocol version (4),
  helper epoch, and exact-target capabilities. Driver/helper version skew fails
  closed.
- `GetRects() -> json` — every window's epoch-qualified target id, workspace,
  monitor, sticky/visibility state, frame geometry, and surface-buffer
  origin. cua-driver combines the buffer origin with AT-SPI
  `CoordType::Window` per-widget coords: `screen = origin + window_xy`. This is
  the GNOME analogue of the X11 `_GTK_FRAME_EXTENTS` reconstruction (AT-SPI's
  `CoordType::Screen` is `(0,0)` for every widget on Mutter). Keeping the frame
  and buffer origins separate accounts for GTK client-side shadows.
- `BeginForeground(transaction,target) -> json` / `QueryForeground(transaction)` /
  `AbortForeground(transaction)` / `EndForeground(transaction)` — the caller
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
- `CaptureTarget(target) -> capture_json` — capture only the
  exact target rectangle when it is painted, unoccluded by higher-stacked
  windows, and free of Shell modal/input grabs. The JSON result binds target
  id, epoch, geometry, stage size, and target-only PNG bytes to one Shell-side
  operation; no caller receives a broader stage image. Cua's in-process cursor
  actor is explicitly non-occluding. Unsafe requests fail closed.
- `MoveCursorFor(owner,target,x,y)` / `ClickPulseFor` / `HideCursorFor` /
  `RemoveCursor` — render independently owned cursors and hide them whenever
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

On Shell 50, `Capture` uses the cursor-free `screenshot_area()` API. The older
stage-content path can fail on remote/headless pointer seats when Mutter exposes
a 0x0 real-cursor sprite; cua-driver renders its own agent cursor instead.

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

## Deferred upstream 0.34.1 changes

This merge retains the fork's v14/protocol-4 implementation. Upstream v9/v10
`CaptureWindow(id,pid)` and `CaptureWindowPreview(id,pid,max_dimension)` use
raw Shell ordinals rather than epoch-qualified targets. Actor capture of covered
windows, its expiring transparent clones, GPU preview downscaling, and PNG
`Creation Time` removal are deferred together. A future port must preserve
atomic target/epoch/geometry binding and the fork's refusal gates; previews
currently use the same exact-target capture route and resize in the driver.
The upstream enumeration reorder is also deferred: private-compositor routing
and refusal of a present incompatible helper retain their existing precedence.

The driver retains its authenticated, connection-owned, bounded transport and
fail-closed parser. The upstream `shell_helper/transport.rs` is not included;
adopting that transport requires preserving connection ownership, deadlines,
queue bounds, and epoch/native identity.

Upstream libei `ei_connection.sync` acknowledgement is deferred. Its pending
reply map and independent five-second timer do not carry this fork's shared
command deadline or action lease. Completion continues to mean native socket
drain or connection discard, not compositor acknowledgement. Any ACK port must
retain the lease through terminal completion even for cancelled callers, and
must not reconnect or replay input; only readiness may initialize a connection.

Upstream installers assume every higher helper version is an additive superset
and keep it based on the version integer alone. That assumption is incompatible
with this fork's exact-target protocol. The hardened, atomic, complete-runtime
installation and stage-only default remain; upstream automatic enablement,
version-only keep-newer behavior, and `--force` replacement are not adopted.

## Other compositors

wlroots compositors such as Sway and labwc do not need it: cua-driver uses
foreign-toplevel activation, virtual-pointer input, and layer-shell there.

KDE Plasma Wayland needs an equivalent target-addressable KWin activation
adapter; it is not yet provided. Portal reachability alone is insufficient
because RemoteDesktop/libei input is global to the compositor focus.
