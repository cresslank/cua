# Stable-loader private GNOME qualification

`hotreload_private_lab.py` is an **opt-in** native test, not part of plain unit
runs. It starts an owned headless GNOME Shell and two real GTK3 receiver windows.
It does not build Cargo artifacts or use the host session/system buses, display,
installed extension, portal consent, or live settings. The optional `--driver`
cell runs a supplied candidate binary against the owned private session.
Missing native prerequisites
are failures, not silently skipped coverage.

Prerequisites: GNOME Shell supporting `--headless --no-x11`, system Python with
PyGObject/GTK3, `dbus-daemon`, `gsettings`, `gnome-extensions`, and dconf-service
at `/usr/libexec/dconf-service`. Coordinate with other native/Cargo workers and
freeze the candidate first. The harness also refuses Cargo/rustc, test drivers,
and another headless Shell. Ordinary installed driver daemons are inventoried
but not contacted. With `--driver`, the candidate can take the driver's normal
per-UID preparation locks; serialize this cell with every other driver/native
suite as well as Cargo. The canonical per-UID raw-input flock is the single
intentionally shared filesystem resource: foreground refusal/capture-drain tests
reserve it and inspect contention, never inject host input. All buses, windows,
settings, extensions and accessibility peers remain private. No input action is
requested by the MCP cell. Hot reload also requires util-linux `flock` on Shell's
PATH; cold first-login loading does not depend on that maintenance utility.

From the repository root, pass an existing short **owned scratch** directory
(outside system `/tmp`) and the qualification flock shared with other workers:

```bash
nice -n 19 ionice -c3 /usr/bin/python3 -I -B \
  libs/cua-driver/wayland-helper/tests/hotreload_private_lab.py \
  --allow-private-shell \
  --scratch-root "$QUALIFICATION_SCRATCH" \
  --qualification-lock "$QUALIFICATION_LOCK"
```

The lock waits in the shared queue and is held only for one lab, including
teardown. Every child receives a new whitelisted
HOME/XDG/runtime environment, including **the bus daemons themselves**. Only
private dconf activation is enabled. Before the first settings setter the harness
activates dconf and checks its actual `/proc/<pid>/environ`. An empty private
system bus prevents Shell's host GDM/logind registration. Keep all logs, including
failed attempts. Each run prints `EVIDENCE=<run>/out`; cleanup checks run-token
processes and mounts without deleting evidence.

## Exercised boundaries

- `idle`: real v1 → v2 closure swap in one Shell, unchanged root bootstrap and
  runtime main bytes, changed relative policy closure, compatible API/protocol,
  fresh epoch, stale target capture/foreground refusal, positive native receiver
  click and absence of late receiver input.
- `capture`: call real public `CaptureTarget`; temporarily wrap only the external
  GI `Shell.Screenshot.composite_to_stream` provider in the lab observer. Its real
  implementation runs and produces PNG bytes before the returned promise is
  parked. Start real `ReloadSelected`, release the provider, and require the
  original source continuation to resume and refuse retired publication. No
  implementation method, generation predicate, or returned PNG is mocked.
- `foreground`: open a real public, **focus-only** transaction; require
  ReloadSelected Busy / foreground_transaction_open, unchanged focus and epoch,
  and a still-open transaction. No native input is submitted and no End/Abort
  result is used as a native consumption proof. Then hold the actual canonical
  raw-input flock and require Busy / raw_input_window_open. After both windows
  close normally, require an idle swap and stale old targets. Every successful
  swap checks that its raw-input FD lease is released.
- `cursor`: public target-bound cursor movement/pulse with actual painted actors
  and GLib animation sources; require destroyed/unparented actors, disconnected
  native GObject handlers and dead native GLib sources after reload.
- `disconnect`: close a distinct real D-Bus client while its cursor is live;
  require connection-owned cleanup, then a clean subsequent reload.
- `faults`: real import failure and a lab-only derived entrypoint that executes
  actual `super.enable()`, records its native resources, then throws. Require
  stable-loader error reporting without poisoning GNOME extension ACTIVE state,
  complete retirement/name release, and fresh-epoch rollback using the public
  reload endpoint. No fault hook is added to shipped code.
- `driver` (selected when `--driver /path/to/candidate` is supplied): real MCP
  enumeration/capture, reload while one driver process remains alive, stale
  old public window ID refusal, different rediscovered ID and fresh capture,
  then EOF/disconnect cleanup. The cell starts owned accessibility peers and
  records the exact driver SHA-256. Full acceptance includes this cell; a run
  without `--driver` does not qualify Rust rediscovery/disconnect.
- `control`: real invalid digest, unlisted-member, symlink and traversal selector
  refusals preserve the healthy epoch; simultaneous reload is Busy; disabling
  and re-enabling during an actual delayed import never constructs the late
  runtime.
- `timeout` (last): hold a native-completed capture past retirement's deadline;
  require helper unavailability and permanent successor-admission poison even
  after the capture's real continuation drains. The actual canonical lease stays
  held (quarantined), not merely a status bit.
- `uncertain` (run separately with `--cases uncertain`): a lab-derived real
  implementation declares an uncertain transaction after closing admission.
  Require post-drain refusal, no successor, unavailable helper and retained
  actual lease. This is an explicit defensive-state fixture, not a claim of a
  native input consumption/acknowledgement test.
- `memory`: import **20 distinct real runtime closures** in one Shell and perform
  real preview capture in each; record GC-assisted whole-process RSS samples.
  Relative policy comments distinguish closure bytes without replacing behavior.
  RSS includes renderer/allocator noise and observer-held references; it is not
  proof that ES modules unload, nor a long-session memory bound.

`--cases idle capture ...` selects a focused native run. Final disable checks both
helper and loader names disappear and inventories the final retired runtime.
The observer reads existing extension/runtime instances; observer and receiver
files are copied only into the owned private environment.

## Evidence verification and deliberate breaks

```bash
/usr/bin/python3 -I -B libs/cua-driver/wayland-helper/tests/hotreload_verify.py \
  "$EVIDENCE"
```

The offline verifier reconciles executed cells, unchanged staged root bytes,
exact manifest-byte digests and all closure member sizes/hashes, one Shell PID,
unique actual epochs, real capture-provider completion, all 20 memory samples,
private dconf/bus environment, and zero-process/mount cleanup.

For a negative control, `--mutation <json>` replaces one unique exact-text span
in **the copied `impl/main.js` or `extension.js` bytes only**, before closure
hashing/import. The JSON is
`{"name":"...","path":"impl/main.js","old":"...","new":"..."}`. Neither the
candidate checkout nor an installed helper is edited. Retain an unchanged full
positive before and after mutations. Count a kill only when `run_failed` names
the intended behavioral cell/assertion, never a startup/import/timeout failure:

```bash
/usr/bin/python3 -I -B libs/cua-driver/wayland-helper/tests/hotreload_verify.py \
  "$MUTANT_EVIDENCE" --expect-failure cursor \
  --error-contains 'retired runtime retains native GLib sources'
```

The scripts pytest owner delegates pure policy to the same `policy.test.mjs`
module. Its obsolete GI-mocked API-10/unsupported-Shell-50 lifecycle test is
retired: that is no longer the helper's contract. Real lifecycle assertions live
in the canonical `gnome_devkit.mjs` suite and this native lab, not fabricated GI
objects. Packager/installer ownership (including the caller-umask matrix) lives
in `scripts/tests/test_wayland_helper_install.py`.

These tests qualify the actual Shell helper and loader boundary. The optional
MCP cell also qualifies Rust epoch rediscovery/disconnect, but not libei/portal
permission-store behavior, host lock modes, or deployment.
Do not install the observer or receiver into a real user session.
