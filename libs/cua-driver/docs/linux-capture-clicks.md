# Linux capture-bound clicks

`click` can bind an explicit window pixel target to the exact screenshot returned
by `get_window_state`. This does not grant input permission or bypass session
policy, the desktop lease, exact-window checks, or delivery-mode restrictions.
Other input tools retain their existing `capture_route_unqualified` refusal.

## Probe before visual inference

1. Inspect MCP `tools/list`: find `click`, then
   `inputSchema.properties.capture_id.title == "cua.capture-click.v1"`.
   The equivalent CLI manifest schema carries the same marker. A schema merely
   containing `capture_id`, without this marker, is not proof of Linux support.
2. Capture the intended window on the same session/connection. Require
   `structuredContent.capture_click_supported == true` and a `capture_id`
   **before** paying for visual inference. Missing/false means this specific
   capture is not qualified. Display-only reads do not mint action authority.

```json
{
  "pid": 1234,
  "window_id": 5678,
  "x": 199.5,
  "y": 134.5,
  "capture_id": "capture_0123456789abcdef0123456789abcdef_0000000000000001",
  "delivery_mode": "foreground"
}
```

Pass this object to `click`. Copy the actual ID verbatim. `x` and `y` are finite
pixels of the **delivered PNG**, in `[0,width)` and `[0,height)`. Never pre-scale
them, add the screen origin, or combine them with element targets, `from_zoom`,
or desktop coordinates. Both target identifiers and both coordinates are
required. Existing button, count and modifier restrictions still apply. Mapping
rounds once in native action units. A rounded point outside the action frame, or
GNOME's absolute desktop `(0,0)` (a legacy native no-point sentinel), is refused
with `capture_coordinate_invalid`, never clamped or retargeted to the center.

## Freshness, mapping and delivery

The common bounded capture registry owns the exact PNG, runtime/session
binding, 60-second TTL, snapshot identity and screenshot revision. Linux opts
into a common optional frame witness containing origin, logical extent, source
pixel extent and a digest of decoded source pixels. Movement, resize, scale
changes and changed source pixels require a fresh capture, even if the new dimensions are
unchanged. PNG metadata such as GNOME's creation timestamp is not a repaint.
This deliberately strict check can refuse animations, caret blinks and other
benign changes while a model is running.

Each axis maps independently: `action_x = x * frame_width / screenshot_width`,
likewise Y. GNOME uses the atomic WinRects **frame rectangle**, not GTK's larger
client-side-decoration buffer/shadow rectangle; physical HiDPI pixels map to
logical native-input units. The retained frame origin supplies compositor
coordinates without the legacy missing-geometry fallback. X11 uses the exact
client drawable's root-relative origin and native extent, bracketed around the
capture. Qualified X11 captures also exclude the Driver's own cursor overlay;
source-pixel checks must not invalidate themselves as the agent cursor moves.
If exclusion leaves residual pixels that need save-under repair, the route
refuses rather than treating cached desktop pixels as a fresh witness.
Native workers retain the generation permit and recheck the frame
revision and native witness after cursor movement, and X11 checks again after
foreground activation. Before any X11 capture-bound press, the screen point
must resolve to the captured window or its WM frame. Same-PID sibling windows,
foreign covers and override-redirect popups are refused with
`capture_frame_mismatch`, rather than inheriting ordinary pointer retargeting.
Only Driver-owned cursor overlays are excluded from this recipient check.
Native delivery itself remains the existing route.

Admission consumes the ID atomically, **before** native delivery. Failed
lookup, target/frame or coordinate validation does not consume it. Once
admitted, it cannot be reused, including after cancellation, a later refusal or
an uncertain native outcome. Do not replay an uncertain action; observe the
receiver and capture again. A tiny compositor/application race after the last
check remains: this is not an atomic screenshot-and-input transaction or a
promise that application state cannot change during delivery.

`delivery_mode` still defaults to `background`; it never silently escalates.
GNOME background left-single clicks use the existing exact-window AT-SPI
point-action route. Other forms retain their native restrictions. X11
capture-bound clicks skip the legacy PID-wide semantic hit test because that
cannot prove a same-PID sibling window. They use the exact-XID native route;
GTK and other toolkits that drop synthetic background events may require an
explicit `foreground` request. Success is still `verified:false`,
`effect:"unverifiable"`, with the actual native route in `path`.

## Refusals

All capture-admission refusals expose the exact top-level `code` and
`effect:"refused"` in structured content:

| Code | Meaning / client response |
| --- | --- |
| `capture_route_unqualified` | Unsupported capture kind/backend, not stale. Do not spend another model call until the probe supports it. |
| `capture_id_invalid` | Malformed ID at capture admission (schema/type errors can be `invalid_arguments` earlier). |
| `capture_binding_failed` | No trusted runtime session binding. |
| `capture_not_found` | Consumed, evicted, retired or unknown ID. Capture again. |
| `capture_expired` | TTL expired (bounded expiry tombstones can later become not-found). |
| `capture_generation_mismatch` | Foreign session/runtime, replaced snapshot or refreshed screenshot revision. |
| `capture_target_mismatch` | Request target differs from the captured target. |
| `capture_frame_mismatch` | Window disappeared, became unviewable, moved, resized, changed scale or has different source pixels. |
| `capture_coordinate_invalid` | Screenshot point is non-finite/outside the delivered image. |
| `capture_action_refused` | Other capture-admission failure. |
| `invalid_arguments` | Ambiguous/incomplete capture request, including element/zoom combinations. |

Existing policy, input-transport and foreground-transaction refusals (including
`window_target_mismatch` for a nonexistent/foreign explicit window) can occur
independently; capture support does not promise delivery on every toolkit.

## Qualified scope and evidence boundary

Supported: exact ordinary **composited X11** window captures and GNOME Wayland WinRects atomic
target captures with the current capture-geometry safety capability. Composited
X11 popup/dialog captures, non-composited X11 captures (obscured drawable pixels
are undefined), desktop captures, other Wayland compositors, legacy
helpers and display-only previews remain unsupported. Their screenshots are
still usable for observation; they do not gain capture-click authority.
macOS and Windows retain their existing capture semantics: the new frame
witness is opt-in, not a silently universal repaint policy.

Native receiver qualification is performed only in a disposable Fedora/Xvfb
session with a compositor, including GDK_SCALE=2, downsampled PNG targeting, move refusal and
ordinary clicks. GNOME's atomic frame/CSD/HiDPI decoding and transform contracts
are covered hermetically; the active host desktop is not exercised or changed.
