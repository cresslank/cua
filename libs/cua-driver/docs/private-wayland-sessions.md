# Private Wayland Computer Use sessions

`cua-compositor` is the isolated high-throughput lane for Linux. It does not
attach to the human GNOME session. Each supervised instance owns its own:

- `XDG_RUNTIME_DIR`, Wayland socket, D-Bus session, AT-SPI endpoint, and
  `CUA_INJECT_SOCKET`;
- config/data/cache roots and agent-owned browser profile directory;
- compositor process group, pid lock, logs, and cleanup boundary.

Start it through `scripts/cua-private-session.py`, not by exporting socket names
into an existing desktop session:

```bash
scripts/cua-private-session.py --name lane-1 --state-dir ~/.local/state/cua-private -- \
  your-agent-command
```

`--dry-run` prints the isolated environment and starts nothing. Different names
can run in parallel. Reusing a live name fails rather than sharing sockets.

## Injection protocol v2

The private control socket requires the `cua-inject v2` handshake. Every mapped
toplevel receives a token containing the compositor-instance epoch and a
monotonic toplevel id:

```text
surface:<instance-epoch>:<toplevel-id>
```

Mutating commands reject PID, app-id, title, and newest-window selectors. The
foreign-toplevel adapter derives the public integer `window_id` from the full
surface token, so compositor restart and native-id reuse produce a different
public id.

Input sequences run inside an explicit connection-owned, exact-surface batch.
Another client targeting that surface receives `target-busy`; batches for
different surfaces can progress concurrently. Disconnect releases the target.
Commands stop at the first rejection and report a potentially partial failure
rather than continuing.

Exact capture uses a bounded compositor lease. While the lease is held, other
toplevel scene nodes are disabled, the target is captured through normal
screencopy and cropped with exact geometry, then the scene is restored. Explicit
release, socket close, and a compositor timeout all restore the scene.

## Browser profile ownership

The supervisor exports `CUA_DRIVER_BROWSER_PROFILE_ROOT`. One browser process
owns that profile; several agents must attach to the same process through a
browser broker.
Do not launch multiple browser processes against the directory or bypass browser
profile locks.

Hermes' `BrowserProfileBroker` uses exact CDP target ids, per-context locks, a
profile-global lock, and explicit compositor-surface bindings. A random temporary
title nonce may prove the initial CDP-target/surface join, but all later actions
use the sticky exact ids.

## Validation boundary

Building and unit testing this code does not launch a compositor. The native E2E
lane launches one only when explicitly invoked:

```bash
scripts/ci/linux/run-rust-e2e-inject.sh
```

That run requires `cua-compositor` to be built and available. It does not require
an OS logout/login or a portal grant. Do not point its environment at the human
Wayland or D-Bus session.
