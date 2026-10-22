# macOS native menu receiver regression

This opt-in host test uses an explicit existing driver socket and builds a new
throwaway AppKit app. It does not install or start a driver. It requests no
permissions, captures no images, sends no keystrokes, and operates only its own
receiver. It briefly fronts that receiver for each row. Run in an unlocked Aqua
session with an already Accessibility-authorized driver. Do not run a full desktop
E2E suite against a shared user session merely to select this test.

```sh
python3 libs/cua-driver/tests/macos-menu-receiver.py --run-gui \
  --socket "$HOME/Library/Caches/cua-driver/cua-driver.sock" \
  --output "$HOME/.hermes/cache/scratch/menu-receiver-installed"
```

Use a fresh output directory for every run. To qualify a **staged** driver, pass
its private socket instead. Verify that driver's signing identity, permissions and
`get_config.source_sha` separately before testing. The test stores the configuration
receipt in `report.json`; it never falls back to the installed daemon. Unsigned or
ad-hoc stages are not evidence for the installed driver's TCC identity.

## Oracle and failure

Window 0 starts key. Every `invoke_menu` explicitly targets window 1. The app
writes native key-window transitions and per-window action counters to a JSON
file, independently of AX and the driver result. Three commands, repeated three
times, cover a direct menu item, a nested item, and a responder-chain command that
only window 1 implements. The last command is ordinary AppKit responder dispatch,
not a fixture-side suppression rule. A final missing-path call must refuse with
`menu_path_unavailable` and change neither counter.

On the affected macOS driver, all nine calls return `effect: unverifiable` with no
error. Six commands run in window 0 instead of window 1. The three target-only
commands do nothing. The receiver journal shows why: window 1 becomes key, the
driver restores window 0, **then** AppKit dispatches the menu action. This is not
failure to activate the requested window or failure to resolve/open the menu.
`AXPress` success acknowledges enqueueing, not completed responder dispatch.

The fix leaves the explicitly requested window key on the already-foreground
route and serializes that focus transaction. It does not activate background
applications, add sleeps/retries, or upgrade native API acceptance to a confirmed
semantic effect. It rechecks that the app is still frontmost after the worker is
admitted, refusing if the user switched applications meanwhile. The normal result
remains `unverifiable`; callers must still check their command's semantic effect.

Expected: **9/9** receiver rows, only window 1 increments, and missing-path refusal
passes. The receiver exits in cleanup; all evidence stays in the output directory.
The test intentionally fails on the old driver even though its API receipts have
no error. Host-independent Rust unit tests cannot prove this asynchronous AppKit
ordering; this native receiver is the regression owner.
