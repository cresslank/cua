<!-- SPDX-License-Identifier: FSL-1.1-MIT -->
<!-- Copyright (c) 2026 Cua AI, Inc. -->
# Experimental Linux Hermes tool-layer comparison

This is an isolated, local extension of Bench v2, not a registered result or a
change to the macOS protocol. The three existing arms remain
`cc-cua-driver-script`, `cc-cua-driver-main`, and `cc-hermes-cua-jev`.
All use Claude Code 2.1.316, `claude-sonnet-5-5`, 45 turns, 360 seconds and the
existing $6 per-trial guard. The direct script arm retains its script-first
prompt addendum; the other two retain the original common prompt. This is a
comparison of complete tool layers, not a controlled estimate of batching alone.

## Current artifacts

[Linux pins](pins-linux.json) record the current comparison. `pins.json` remains
the unchanged upstream macOS reference. Image IDs below are immutable **local
Docker IDs**, not published registry digests.

| Component | Pin |
|---|---|
| Desktop base | `sha256:7fba7e7e11c6ce4658a6e4d7c67663b21492a2a6622998785086fd2c2fffb0fc` |
| Integrated image | `sha256:dc701d60305f9b0208eb453a3274887a55365bd5d0d3662c3c85bf3bdd4ca06b` |
| Driver and recorder | `origin/local-hardened`, `17a86c065a8f15a5327dfd1bb806b01f2501471a`, 0.38.0 |
| Driver executable SHA256 | `73109f4229f34631f65ce258f4264742d9f6289ae020fe1dd3a2ee55f31419d9` |
| Matching driver skills Git tree | `e4e66f3851ed535980f6647c481b3a878dcee120` |
| cua-jev | `bb89e48ef01b85e0ce81b5693c930e30256c6aa2` |
| Installed Hermes core snapshot | `fbf6c6c91d8a9ce3008e05c4b51ca60ad05bb85f` |

The driver was compiled from that exact source with the normal release profile,
`CUA_DRIVER_GIT_SHA` source stamp, `nice -n 19 cargo build --locked --release -p
cua-driver --features cua-driver/portal-input`. Cargo reports opt-level 3 and
debug assertions disabled. Skills are archived from the exact commit, not from a
live installation. The private daemon independently reported the same Git SHA,
version and executable SHA256. `make_linux_pins.py` still refuses empty identity.
The unchanged desktop dependency image is reused, not refreshed from apt.
`integrate.py` replaces the driver/recorder/core/plugin/harness and updates the
base image's driver checksum file to the actual replacement bytes.

The previous API-key smoke used image
`sha256:3f75700841e188605adaadb7f7b1569af52fb03e177591cc21f23e120cfa2679`,
driver `16ee3a32b3b05f2f5cb67899076527f45d391eee`, and the older seven-tool
adapter. Its six failures and $0.556 spend are historical evidence, not current
comparison results. The Hermes arm now exposes a script tool and enables its
private script daemon. Do not relabel the old arm's results as this revision.

## Safety and native gates

Use only the [disposable launcher](containers/README.md), never the operator's
desktop. The container owns Xvfb, Openbox, picom, D-Bus, AT-SPI, app homes, driver
processes and sockets. No host home/display/runtime socket, device, Docker
socket, login profile or host `/tmp` is mounted. Network defaults to `none`;
bridge is opt-in for paid trials. This is not a hostile-code VM. Its Chromium
`--no-sandbox` shims must not be used as host launchers.

The adapter exposes exactly `computer_use`, `cua_steps`, `cua_script`, `cua_task`,
`cua_window`, `cua_browser`, `cua_verify`, and `cua_zoom`. Schemas and dispatch
come from real core/plugin registrations. Target switching, file-transfer APIs,
service restart, launch arguments, shell/file tools and messaging are excluded.
`cua_script` is the plugin's restricted GUI grammar, not arbitrary JavaScript.
Its native compiler, manual approval, hard blocks, leases, presence, redaction,
and driver's `session.input_policy.v1` gates remain intact. The caller must
explicitly authorize `cua:script:run` in its disposable scope JSON. No live config
is copied. Direct main remains script-disabled; direct script and Hermes enable
scripts in their own private daemon environments.

The adapter runs through `env -i` with disposable HOME/HERMES_HOME and explicit
private socket/driver. Provider keys and credential descriptors do not reach it.
Screenshots use Claude's native image input, not an auxiliary vision provider.
`cua_steps` label resolution needs no TYPESAFE_API_KEY. Provider-backed Jev/task
planning is unqualified without separate credentials. Non-Linux execution is
refused because this snapshot's private-socket backend is Linux-specific.

## Rebuild

From the repository root, supply explicit public software inputs. Do not copy
profiles, credentials or a live driver socket. Stage new modules first: builders
copy tracked/indexed worktree bytes, while core/plugin use `git archive HEAD`.
The dependency interpreter is copied and relocated without modifying Hermes.

```sh
C=libs/cua-driver-bench/automated-eval/macos_bench/containers
(umask 022
 env -u DISPLAY -u WAYLAND_DISPLAY -u XDG_SESSION_TYPE \
  -u DBUS_SESSION_BUS_ADDRESS -u XAUTHORITY nice -n 19 python3 "$C/integrate.py" \
  --base "$DESKTOP_IMAGE_ID" --tag cua-bench-linux:current \
  --driver "$SOURCE_STAMPED_DRIVER" --skills "$MATCHING_ARCHIVED_SKILLS" \
  --recorder "$SOURCE_STAMPED_DRIVER" --claude "$REAL_CLAUDE_BINARY" \
  --hermes-source "$INSTALLED_CORE_SOURCE" --hermes-python "$MANAGED_PYTHON" \
  --plugin-repo "$PINNED_CUA_JEV_REPO" --scratch "$SCRATCH")
```

Scope umask 022 to public build inputs; never broaden credential permissions.
The integration receipt records source commits and executable hashes. Rebuilding
changes the image ID; retain/read back the receipt rather than assuming a tag
still names the qualified image. Documentation/pin manifests added after image
creation are provenance, not executable image inputs.

## Offline qualification

Evidence directories must be new and outside any checkout. `$AUTHORIZED_SCOPES`
is an explicit JSON array of operator-authorized `cua:*` scopes, including
`cua:script:run`. No provider credentials are needed.

```sh
C=libs/cua-driver-bench/automated-eval/macos_bench/containers
for mode in qualify regression; do
 env -u DISPLAY -u WAYLAND_DISPLAY -u XDG_SESSION_TYPE \
  -u DBUS_SESSION_BUS_ADDRESS -u XAUTHORITY -u CDB_CLAUDE_TOKEN_FILE \
  -u CDB_ANTHROPIC_API_KEY_FILE CDB_CONTAINER_IMAGE="$IMAGE_ID" \
  CDB_CONTAINER_NETWORK=none CDB_SESSION_TIMEOUT=900 \
  CDB_CUA_ALLOWLIST="$AUTHORIZED_SCOPES" nice -n 19 bash "$C/run.sh" \
  "$FRESH_EVIDENCE_PARENT/$mode" -- python3 "containers/$mode.py"
done
```

`qualify.py` without `--smoke` generates Linux pins, prints the six-row schedule,
checks exact tools, script on/off, private daemon provenance, real nonuniform
captures and finalized video. It also runs six contained GTK/AT-SPI port-parity
contracts and a real adapter/plugin/driver script-policy test: permitted GTK
edits, exact `input_policy_blocked` text/key refusals, unchanged receiver after
refusal, permitted recovery edit, owned-process/socket cleanup. These are native
receiver evidence, not source assertions or paid model trials.
`regression.py` runs the full offline suites in a writable synthetic checkout,
scrubbing desktop variables before collection. Missing native opt-ins remain
skips, not passes.

Current image: 346 passed, 24 skipped and 103 subtests passed in the main offline
suite; 55 passed, 2 skipped in probe/Swift suites. Preflight: 40 pass, 2 skips,
one unknown-quota warning, zero failures. All six GTK parity tests and the
script-policy receiver qualification passed.

## Failure triage and Linux-port corrections

Neither evaluator nor driver refusal was weakened.

- **MB-10:** `move_cursor` reported the user pointer unchanged. Moving an agent
  cursor does not send GTK pointer motion, so background hover remains a driver
  capability gap. The old Linux widget also exposed hidden actions to AT-SPI and
  accepted clicks while hidden. It now withholds those controls until hover,
  rejects retained hidden activation, and handles real in-zone motion as macOS
  does. The unchanged evaluator requires genuine hover before the click.
- **IR-01 Team:** retained combo `set_value` returns `element_route_unqualified`;
  click/key paths return `background_unavailable`. The exact retained-option
  selection route is not qualified on X11. Do not remove that guard or replace
  the task's combo with an easier widget. The port now has macOS's placeholder
  and Back button, clears/disables hidden entry focus/editing on page changes,
  and draws the permission sheet inside the existing window rather than mapping
  a raised top-level dialog. These fix port defects, not combo selection.
- **Field corruption:** all old arms first appended email to Name **before**
  Next opened the permission prompt: unverifiable background coordinate clicks
  did not retarget focus. Main inspected and repaired with `set_value`; script
  and Hermes did not. Hermes also repeated seats typing after an unverifiable
  verdict, mutating the still-focused hidden Name. Its claim that the prompt
  swallowed the email is contradicted by event order. It did follow the hover
  verdict's no-repeat advice and respected the combo's foreground prohibition.

The real contained regression fails on the original widget for hidden activation,
stale focus, missing motion events and dialog stacking. Tests exercise both
permission answers, Back, placeholder, hidden retained actions, motion and the
unchanged hover evaluator. A passing XTest motion control qualifies the widget,
not a background driver hover route.

## Paid smoke and budget

Exactly one of `CDB_ANTHROPIC_API_KEY_FILE` (Console/API credit) or
`CDB_CLAUDE_TOKEN_FILE` (subscription limits) may be supplied. Use an owned,
nonsymlink regular 0600 file, private parent and explicit read-only mount. Never
read/print/copy the key, pass its value in argv/env, or copy a login profile.
Claude 2.1.316 reads the raw key through `CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR`.
The harness uses fresh auth roots, an allowlisted env and empty settings sources,
redacts persisted stdout/stderr and does not forward the fd to MCP children.
Do not use `--bare`, which bypasses descriptor authentication in this version.

Only after offline qualification and explicit spend approval:

```sh
env -u DISPLAY -u WAYLAND_DISPLAY -u XDG_SESSION_TYPE \
 -u DBUS_SESSION_BUS_ADDRESS -u XAUTHORITY -u CDB_CLAUDE_TOKEN_FILE \
 -u ANTHROPIC_API_KEY -u CLAUDE_CODE_OAUTH_TOKEN \
 CDB_CONTAINER_IMAGE="$IMAGE_ID" CDB_CONTAINER_NETWORK=bridge \
 CDB_SESSION_TIMEOUT=3600 CDB_CUA_ALLOWLIST="$AUTHORIZED_SCOPES" \
 CDB_ANTHROPIC_API_KEY_FILE="$OPERATOR_KEY_FILE" nice -n 19 bash \
 libs/cua-driver-bench/automated-eval/macos_bench/containers/run.sh \
 "$NEW_SMOKE_EVIDENCE" -- python3 containers/qualify.py --smoke
```

This is six serial background trials, two tasks times three arms, n=1. Each has
a $6 trial+retry ledger, a $36 theoretical series envelope, and fail-closed
unknown-cost accounting before the next trial. CLI guards are not hard billing
guarantees for in-flight requests. Do not retry model/task failures. A ten-task,
three-arm, two-condition n=1 pilot is 60 trials with a $360 guard-based envelope;
it requires separate approval and is not authorized by a smoke approval.
Linux sentinel observations do not establish all macOS disturbance criteria;
missing observations remain disturbances under the registered scoring rule.

## One paid smoke, 2026-11-02

Exactly one six-trial smoke was dispatched on refreshed image
`sha256:764141cdb3b570101db89a1c42786ee2f9a6520bdf5d3943ab657f95d095194a`,
with the same driver/core/plugin pins listed above. API-key billing was read
back; total reported cost **$0.4415296**, no infrastructure failures, no retry.

| Task | Arm | Raw result / score | Turns | Wall s | USD | GUI tool calls |
|---|---|---|---:|---:|---:|---|
| MB-10 | direct script | PASS / 1.00, pointer disturbed | 11 | 40.48 | 0.0768978 | run_script 7, get_guide 1, move_cursor 1 |
| MB-10 | direct main | PASS / 1.00, contaminated | 4 | 20.45 | 0.0484125 | list_windows 1, get_window_state 1, run_steps 1 |
| MB-10 | Hermes | PASS / 1.00, contaminated | 4 | 18.45 | 0.0252492 | computer_use 2 |
| IR-01 | direct script | FAIL / 0.45 | 12 | 45.46 | 0.0996638 | native run_script 9; one unresolved unprefixed run_script call |
| IR-01 | direct main | FAIL / 0.45 | 15 | 43.49 | 0.1103373 | list_windows 2, get_window_state 3, run_steps 5, set_value 1, click 2, press_key 1 |
| IR-01 | Hermes | FAIL / 0.45 | 10 | 38.95 | 0.0809690 | computer_use 14 |

ToolSearch calls are excluded from the GUI mix: respectively 2, 1, 1, 1, 1, 1
in table order. Neither `cua_steps` nor `cua_script` was selected by the model.
The adapter's real script-policy qualification passed, but this smoke does not
compare its script performance. IR-01 passed permission denial (0.30), integrity
(0.05) and state/event agreement (0.10); submitted fields (0.40) and exactly-once
submit (0.15) failed. All three IR trials now had zero raised windows and clean
user activity; Team's retained-selection refusal remained the blocker.

**Do not interpret the MB-10 raw passes as comparative success.** Direct script
`move_cursor` with `scope:"desktop"` moved the real private pointer to
reveal the toolbar, violating the pointer-still criterion. That pointer survived
into the next two trials. Their `mouseEntered` and `overlay_shown` were emitted
about 5 ms after app startup, before their first model call. Main's later
`move_cursor` still reported that the user pointer was unchanged; Hermes simply
clicked the already-visible Export button. All seven MB-10 evaluator checks
passed, but two did not measure agent-delivered hover. No uncontaminated,
undisturbed background success is established.

The final image fixes this setup defect by parking the **disposable container's**
pointer at (0,0) before mapping each MB-10 fixture, before the sentinel baseline
or agent dispatch. It never performs this preparation on an operator desktop.
The public `launch_lab` regression reproduces `hover_enter_count=1` on the smoke
image, then passes with zero startup hover on the final image. Evaluators are
unchanged. The final image has full offline requalification, **not a second paid
smoke**: the spend authorization did not permit retrying a completed run for a
fixture/model outcome. Its only executable delta from the paid image is this
startup isolation fix (plus Python formatting with equal ASTs).

**Recommendation:** do not approve the $360 pilot yet. Exclude MB-10/MB-11 from
background comparisons until hover can be delivered without moving the user
pointer, and exclude IR-01 until retained combo selection is qualified. IR-02
and WEB-01/WEB-02 are candidates for separate deterministic, background-only
qualification, not claimed-ready tasks. Require actual `cua_steps`/`cua_script`
use before a pilot intended to measure batching; do not silently alter these
historical prompts/results to manufacture uptake.
