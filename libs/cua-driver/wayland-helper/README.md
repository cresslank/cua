# cua WinRects — GNOME Shell helper extension (Wayland)

A small GNOME Shell extension that lets cua-driver get **pixel coordinates**,
address an incarnation-qualified target, perform a bounded foreground
transaction, capture only when the target is painted and unoccluded on the
current stage with no Shell input grab, and draw session-owned **agent cursors**
on GNOME Mutter Wayland. A
normal Wayland client cannot do these things globally.

It exposes `org.cua.WinRects` on the session bus:

- `GetCapabilities() -> json` — protocol version, helper epoch, and exact-target
  capabilities. Driver/helper version skew fails closed.
- `GetRects() -> json` — every window's epoch-qualified target id, workspace,
  monitor, sticky/visibility state, frame geometry, and surface-buffer
  origin. cua-driver combines the buffer origin with AT-SPI
  `CoordType::Window` per-widget coords: `screen = origin + window_xy`. This is
  the GNOME analogue of the X11 `_GTK_FRAME_EXTENTS` reconstruction (AT-SPI's
  `CoordType::Screen` is `(0,0)` for every widget on Mutter). Keeping the frame
  and buffer origins separate accounts for GTK client-side shadows.
- `BeginForeground(target) -> json` / `EndForeground(transaction) -> json` —
  activate one exact current-workspace target even when another ordinary window
  overlaps it, then require confirmed focus, visibility, no modal child, and no
  remaining occlusion. Restore the prior workspace/window only if focus still
  belongs to the transaction. `CommitForeground` is reserved for an explicit
  keep-focused action. A Shell input grab means an active Shell modal count or
  key focus inside `Main.uiGroup`; ordinary application-surface key focus is not
  misclassified as Shell chrome.
- `CaptureTarget(target) -> png_base64` — capture the compositor stage only when
  the exact target is currently painted, unoccluded by higher-stacked windows,
  and free of Shell modal/input grabs. Cua's click-through cursor overlay is
  non-occluding only after exact title/type/sticky state and `/proc/<pid>/exe`
  proof. Unsafe requests fail closed; they are never cropped from unrelated
  pixels.
- `MoveCursorFor(owner,target,x,y)` / `ClickPulseFor` / `HideCursorFor` /
  `RemoveCursor` — render independently owned cursors and hide them whenever
  their target is not actually visible on the active workspace.

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

wlroots compositors such as Sway and labwc do not need it: cua-driver uses
foreign-toplevel activation, virtual-pointer input, and layer-shell there.

KDE Plasma Wayland needs an equivalent target-addressable KWin activation
adapter; it is not yet provided. Portal reachability alone is insufficient
because RemoteDesktop/libei input is global to the compositor focus.
