<!-- SPDX-License-Identifier: FSL-1.1-MIT -->
<!-- Copyright (c) 2026 Cua AI, Inc. -->
# Experimental Linux Hermes tool-layer comparison

This is a local, isolated extension of Bench v2, not a registered or published
result. It does **not** change the macOS protocol or the default arms. It compares
`cc-cua-driver-script` (stock MCP, run_script on), `cc-cua-driver-main` (stock MCP,
run_script off), and `cc-hermes-cua-jev` under the same `claude -p` invocation:
`claude-sonnet-5-5`, 45 turns, 360 seconds, and a $6 per-trial guard. The existing
script-arm prompt addendum is retained and must be accounted for in comparisons.

## Safety and tool surface

Use only the [disposable container launcher](containers/README.md), never the
operator's desktop. The image owns Xvfb, openbox, picom, D-Bus, AT-SPI, apps, all
private driver daemons, sockets and writable homes. The default network is
`none`. No host home, desktop/runtime socket, device, host `/tmp`, Docker socket
or login profile is mounted. Evidence is the only default bind mount. Optional,
explicit inputs are a read-only authorized-scope JSON file and a read-only,
owned, nonsymlink 0600 setup-token or API-key file (exactly one). Bridge networking is opt-in for paid
model trials. The container is a practical isolation boundary, not a hostile-code
VM; do not expose it to untrusted data or reuse its Chrome `--no-sandbox` shims
on the host.

The new stdio adapter exposes **only** `computer_use`, `cua_steps`, `cua_task`,
`cua_window`, `cua_browser`, `cua_verify`, and `cua_zoom`. It imports the real
Hermes core and cua-jev plugin, narrows their schemas to GUI operations, and
calls their native handlers. Target switching, browser transfer/restart APIs,
launch arguments, shell/file tools and messaging are not exposed. Native manual
approval, hard blocks, desktop leases, presence checks and redaction remain live.
Only operator-authorized `cua:*` command scopes enter the disposable config.
The private daemon uses the upstream bench's existing approval-bypass flag;
this does not bypass the Hermes-side gates. No host driver service is involved.

The MCP subprocess runs through `env -i`, with a disposable HOME/HERMES_HOME,
explicit private socket and driver command. No provider keys are forwarded.
Screenshots are configured for Claude's **native** image input, not auxiliary
vision inference. `cua_steps` can resolve labels without TYPESAFE_API_KEY.
Provider-backed Jev/task planning is unqualified and may fail closed without its
separate credentials. The arm currently refuses non-Linux execution because
this plugin revision's private-socket route is Linux-specific.

The catalog-pinned Hermes `mcp-serve` plugin was inspected, not installed. Its
curated `--tools` surface is additive to messaging/history/approval tools, so it
cannot provide this GUI-only benchmark surface. The thin adapter is intentional.

## Build

Build the desktop image first, following `containers/README.md`. Then add explicit
software snapshots. These commands run from the repository root. Every source
path below is an explicit operator input, not a discovery of credentials.

```sh
C=libs/cua-driver-bench/automated-eval/macos_bench/containers
python3 "$C/integrate.py" \
  --base cua-bench-linux:disposable --tag cua-bench-linux:hermes \
  --driver "$SOURCE_STAMPED_DRIVER" --skills "$MATCHING_SKILLS" \
  --recorder "$RELEASE_DRIVER" --claude "$REAL_CLAUDE_BINARY" \
  --hermes-source "$HERMES_SOURCE" --hermes-python "$MANAGED_HERMES_PYTHON" \
  --plugin-repo "$CUA_JEV_REPOSITORY" --scratch "$SCRATCH"
```

The core and plugin are `git archive HEAD` snapshots, not profiles. The managed
Python distribution/dependencies are copied and relocated; no Hermes installation
or plugin is modified. Bench files are current **tracked/indexed** worktree bytes.
Stage new public modules before building. The image and build context retain a
receipt with source revisions and executable hashes. Dependencies in the desktop
base are version-recorded rather than fully locked; reuse the immutable image ID
for repeat trials.

`make_linux_pins.py` remains fail-closed: the live private daemon must report its
real nonempty `build.git_sha`, version and executable hash. An installed 0.38.0
release with empty Git identity was rejected. The current driver under test is a
source-stamped **release** build of `origin/local-hardened` at
`16ee3a32b3b05f2f5cb67899076527f45d391eee`, version 0.38.0, SHA256
`e72b63396e680cf5ee1f67708b310473a4ddeb303cc17635505bb529a295a576`, with
skills archived from that exact source. It uses the repository's normal release
profile (opt-level 3, debug assertions off, no profile overrides), built with
`nice -n 19`, `cargo build --locked --release -p cua-driver --features
cua-driver/portal-input`, and the normal `CUA_DRIVER_GIT_SHA` build stamp.
It replaces the earlier debug qualification artifact; debug timing is not a
release baseline. Build provenance and logs are retained outside the checkout at
`/home/lantzk/.hermes/cache/scratch/bench/driver-release-scrbj7cc/`.
A separate private release recorder is supported with `--recorder-bin`; its
executable hash is recorded separately in versions. The current integration uses
the same source-stamped release bytes for recorder and driver under test.
The upstream macOS pin file remains unchanged for reference.

### Qualified release image and exact local rebuild

- Desktop image ID: `sha256:7fba7e7e11c6ce4658a6e4d7c67663b21492a2a6622998785086fd2c2fffb0fc`.
- Integrated image ID: `sha256:d7a7d9b44098cded4f63b96e12d4242b1853b98bb2a5a6d3f5d9b076e4fdb947`.
- Integration receipt: `/home/lantzk/.hermes/cache/scratch/bench/integration-build-szal1x47/receipt.json`.

These are local immutable Docker image IDs, not published registry RepoDigests.
The CUA-1948 block runs before all subsequent build HTTPS fetches. The final
image has neither root enabled in `ca-certificates.conf` nor linked into the
system CA directory. The image uses the same release bytes for the scored
driver and private recorder. Source-stamped identity, not its filename, is the
pin authority.

From `/home/lantzk/Development/cua-wt/bench-20261101`:

```sh
C=libs/cua-driver-bench/automated-eval/macos_bench/containers
# umask applies only to public image-build context, never to the token file.
(umask 022
 env -u DISPLAY -u WAYLAND_DISPLAY -u XDG_SESSION_TYPE \
  -u DBUS_SESSION_BUS_ADDRESS -u XAUTHORITY nice -n 19 python3 "$C/build.py" \
  --driver /home/lantzk/.hermes/cache/scratch/bench/driver-release-scrbj7cc/cua-driver \
  --tag cua-bench-linux:disposable --scratch /home/lantzk/.hermes/cache/scratch/bench &&
 env -u DISPLAY -u WAYLAND_DISPLAY -u XDG_SESSION_TYPE \
  -u DBUS_SESSION_BUS_ADDRESS -u XAUTHORITY nice -n 19 python3 "$C/integrate.py" \
  --base cua-bench-linux:disposable --tag cua-bench-linux:hermes \
  --driver /home/lantzk/.hermes/cache/scratch/bench/driver-release-scrbj7cc/cua-driver \
  --skills /home/lantzk/.hermes/cache/scratch/bench/driver-release-scrbj7cc/skills \
  --recorder /home/lantzk/.hermes/cache/scratch/bench/driver-release-scrbj7cc/cua-driver \
  --claude /home/lantzk/.local/share/claude/versions/2.1.316 \
  --hermes-source /home/lantzk/.hermes/hermes-agent \
  --hermes-python /home/lantzk/.hermes/installs/865c4168a0252f86/environments/818a0ad306dc4e258838adcf41c0d91b/venv/bin/python \
  --plugin-repo /home/lantzk/Development/hermes-cua-jev \
  --scratch /home/lantzk/.hermes/cache/scratch/bench)
```

Do not inherit restrictive `umask 077` into the public build context: directories
and generated metadata would become root-only inside the non-root image. A
failed prequalification bootstrap exposed that issue; it was rebuilt with the
scoped `022` above, not waived. Logs are `desktop-release-build-022.log` and
`integration-release-build-022.log` in scratch. All non-documentation tracked
harness files and both driver hashes were read back as the image's default
non-root user with no network, no host mounts and no desktop session. Documentation
records the image ID after build, so it is deliberately excluded from that byte
comparison.

## Offline qualification and regression

Create `AUTHORIZED_SCOPES_JSON` as a JSON array containing only the `cua:*`
entries the operator authorizes. Do not copy a live config into the image.
EVIDENCE_DIR must not exist, and its parent must be outside a Git checkout.

```sh
C=libs/cua-driver-bench/automated-eval/macos_bench/containers
SCRATCH=/home/lantzk/.hermes/cache/scratch/bench
env -u DISPLAY -u WAYLAND_DISPLAY -u XDG_SESSION_TYPE \
  -u DBUS_SESSION_BUS_ADDRESS -u XAUTHORITY -u CDB_CLAUDE_TOKEN_FILE \
  CDB_CONTAINER_IMAGE=sha256:d7a7d9b44098cded4f63b96e12d4242b1853b98bb2a5a6d3f5d9b076e4fdb947 \
  CDB_CONTAINER_NETWORK=none CDB_SESSION_TIMEOUT=900 \
  CDB_CUA_ALLOWLIST=/home/lantzk/.hermes/cache/scratch/bench/cua-allowlist.json \
  bash "$C/run.sh" /home/lantzk/.hermes/cache/scratch/bench/qualify-release-20261102-02 \
  -- python3 containers/qualify.py

env -u DISPLAY -u WAYLAND_DISPLAY -u XDG_SESSION_TYPE \
  -u DBUS_SESSION_BUS_ADDRESS -u XAUTHORITY -u CDB_CLAUDE_TOKEN_FILE \
  CDB_CONTAINER_IMAGE=sha256:d7a7d9b44098cded4f63b96e12d4242b1853b98bb2a5a6d3f5d9b076e4fdb947 \
  CDB_CONTAINER_NETWORK=none CDB_SESSION_TIMEOUT=900 \
  bash "$C/run.sh" /home/lantzk/.hermes/cache/scratch/bench/regression-release-20261102-02 \
  -- python3 containers/regression.py

env -u DISPLAY -u WAYLAND_DISPLAY -u XDG_SESSION_TYPE \
  CDB_CONTAINER_IMAGE=sha256:d7a7d9b44098cded4f63b96e12d4242b1853b98bb2a5a6d3f5d9b076e4fdb947 \
  python3 "$C/test_launcher.py" --scratch "$SCRATCH"
env -u DISPLAY -u WAYLAND_DISPLAY -u XDG_SESSION_TYPE \
  python3 -B -m unittest discover -s "$C" -p test_qualify.py -v
```

The retained successful qualification is `qualify-release-20261102-01`, with
`qualify-release-20261102-01.log` in scratch. The replay paths above are fresh;
never overwrite a retained evidence directory.

Qualification generates real Linux pins, prints a six-row schedule for MB-10 and
IR-01 across the three arms, and runs offline preflight. It checks exact tools,
script on/off, private daemon identity/capabilities, nonuniform captures through
each real MCP surface, and finalized video. Linux's short recording preflight is
10 seconds: the original 3-second path reproducibly timed out during ffmpeg
shutdown. A BenchLab target remains visible for the video checker.

Regression uses a writable synthetic checkout **inside** the container because
some upstream tests require Git detection and writable source. Display/session
variables are scrubbed before collection; tests can otherwise auto-enable GUI
work simply because DISPLAY exists. Missing opt-in native inputs are skips,
never reported as passes.

## Paid smoke: operator authentication prerequisite

No scored bench trial was dispatched during qualification. The operator selects
exactly one explicit credential file:

- `CDB_ANTHROPIC_API_KEY_FILE`: raw Console API key, billed to the linked Console
  organization, including any applicable promotional API credit.
- `CDB_CLAUDE_TOKEN_FILE`: raw `claude setup-token` output, billed to subscription
  limits instead. Run setup-token yourself if choosing that mode.

Create the private directory with mode 0700 and save the credential with an editor
in an owned nonsymlink regular 0600 file. Do not read/copy a login profile,
`~/.claude`, keychain, or helper settings. Never put a secret in chat, shell argv,
environment values, logs, an image, or the repository. Both variables set is an
error, not a preference order.

**Verified Claude 2.1.316 behavior:** `CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR` names
an inherited numeric fd, not a pathname; Claude reads the raw UTF-8 value, up to
65536 bytes, and trims surrounding whitespace. It supplies the X-Api-Key header.
In `-p`, ambient `ANTHROPIC_API_KEY` would take precedence, and saved keys/helpers
can still be consulted if an fd fails. A configured helper can execute even
with a valid fd. The harness therefore uses an allowlisted environment, a fresh
HOME plus Claude/XDG/Anthropic config roots for every spawn, and empty
`--setting-sources` in both explicit modes. It validates the opened descriptor
(owner, regular file, no symlink, exact 0600), rejects missing/empty/multiline or
oversize credentials before spawning, and never falls back to a login.
Do not use `--bare` or `CLAUDE_CODE_SIMPLE=1`: they bypass fd auth in this version.
Explicit modes reject debug files and explicit auth-setting overrides. Stdout,
stderr and elicitation data are redacted before persistence, including failed
children. Trial rows and `credential-mode.json` record only `api_key` or
`setup_token`. No provider credential is supplied to Hermes.

The installed and original integrated-image Claude binaries were byte-identical
2.1.316. Static-source inspection and 41 network-none fake-401 probes established
these semantics without upstream calls. Linux keychain behavior is the scope;
this is not native macOS Keychain qualification.

The API-key-qualified local image is
`sha256:3f75700841e188605adaadb7f7b1569af52fb03e177591cc21f23e120cfa2679`.
It layers the updated public harness and canonical CI wiring fixture over the
historical release image, without replacing any executable or dependency.
Do not use the historical setup-token-only image for API-key trials. Once the
operator has created `/home/lantzk/.config/cua-driver-bench/anthropic-api-key`,
this is the exact API-key smoke command. It is documented, **not run**:

```sh
cd /home/lantzk/Development/cua-wt/bench-20261101 && \
env -u DISPLAY -u WAYLAND_DISPLAY -u XDG_SESSION_TYPE \
  -u DBUS_SESSION_BUS_ADDRESS -u XAUTHORITY -u CDB_CLAUDE_TOKEN_FILE \
  CDB_CONTAINER_IMAGE=sha256:3f75700841e188605adaadb7f7b1569af52fb03e177591cc21f23e120cfa2679 \
  CDB_CONTAINER_NETWORK=bridge CDB_SESSION_TIMEOUT=3600 \
  CDB_CUA_ALLOWLIST=/home/lantzk/.hermes/cache/scratch/bench/cua-allowlist.json \
  CDB_ANTHROPIC_API_KEY_FILE=/home/lantzk/.config/cua-driver-bench/anthropic-api-key \
  bash libs/cua-driver-bench/automated-eval/macos_bench/containers/run.sh \
  /home/lantzk/.hermes/cache/scratch/bench/smoke-apikey-20261102-01 \
  -- python3 containers/qualify.py --smoke
```

This executes **only six** serial background-condition trials, MB-10 and IR-01,
three arms, n=1. Each task/arm has a separate $6 trial+retry ledger, not a $6 cap
for the whole series. It stops before the next trial on missing/invalid cost or
a trial's accumulated cost reaching $6. The six-trial planning envelope is $36;
CLI dollar guards are not billing guarantees for a request already in flight.
Model/init/quota-probe calls are disabled except the requested trial dispatches;
unknown subscription quota is explicitly admitted only for this bounded smoke.
Inspect failed attempts before resuming. Do not reuse an existing evidence path
or blindly retry authentication/model failures. The credential is mounted read-only
and passed through a freshly opened fd to each Claude trial or quota probe.
The adapter does not receive provider keys or that descriptor.

## API-key revision qualification

All checks used fake credentials and network-none containers. No paid bench call
or real key access was performed by this revision's implementation work.

- Full container offline suites: **344 passed, 24 skipped, 102 subtests passed**;
  independent probe/Swift suites: **55 passed, 2 skipped**. Native opt-ins and
  platform limitations remain skips. One non-failing PyGI deprecation warning.
- The two wrong-owner descriptor tests additionally passed as isolated root with
  CHOWN/DAC_OVERRIDE, so permission denial cannot mask a missing owner guard.
- Host offline suites also passed: **312 passed, 44 skipped, 95 subtests passed**,
  plus **62 passed, 10 skipped, 9 subtests passed** for ancillary suites.
- **14 real Docker launcher cases** passed, including both credential mount modes,
  owner/mode/symlink and both-set refusals, environment isolation, readonly mount,
  failure/timeout/cancellation propagation and exact-container-removal readback.
- **18 targeted mutations killed** through the retained credential/launcher tests:
  descriptor plumbing, billing evidence, redaction, settings and ambient-auth
  isolation, required/exclusive modes, opened-file guards, quota-probe fd, override
  refusal and readonly mounting. Source restoration was byte-verified.
- Three-arm offline qualification with a fake API-key file: **40 pass, 2 skips,
  1 unknown-quota warning, 0 failures**, six-row dry-run, real captures and video;
  `credential-mode.json` read back as `api_key`. No fake key appeared in retained
  qualification/regression/launcher evidence.
- Evidence root: `/home/lantzk/.hermes/cache/scratch/bench-apikey-20261102/`.
  `REPORT.md` documents the 41 Claude fd-semantics probes; `mutation-results.json`
  and `offline-container-results.json` identify execution receipts.

The initial ad-hoc overlay refused at startup because its copied entrypoint was
not executable; the corrected overlay uses `COPY --chmod=755`. Expanding offline
regression first exposed a missing canonical CI-workflow fixture. The integration
builder now copies that real public source fixture rather than weakening its test.
These failed attempts are retained, not counted as passing qualification.

## Release verification and limits

The release-driver refresh reran the checks below on the immutable artifacts
listed above. Evidence directories are outside the checkout; no scored trials
were dispatched.

- All three arms: real private-daemon identity checks, exact tool-surface checks,
  nonuniform captures and six-row dry-run passed.
- Offline preflight: **40 pass, 2 not-applicable skips, 1 unknown-quota warning,
  0 failures**; real 10-second recorded clip decoded successfully.
- Focused regression: **127 passed, 3 skipped, 82 subtests passed**.
- Launcher: environment isolation, readonly token mounting and mode/symlink
  refusals, exit propagation, timeout, intentional orphan detection and SIGTERM
  cleanup passed with exact-container-removal readback.
- Desktop-base infrastructure smoke: **28 passed, 14 subtests passed**, one
  non-failing PyGI `GLib.unix_signal_add_full` deprecation warning; actual Linux
  pack Electron/Calc/GnuCash/Chrome windows were witnessed. These are not agent
  task-success results.
- Scored MB-10/IR-01 rows: **not run, authentication required** for all three arms.
  Success, background completion, turns, wall time and per-trial model cost are
  unknown. Benchmark model spend to date: $0, no requests dispatched.

The Linux sentinel cannot establish every macOS Amendment-22 disturbance
criterion. Any unmeasured criterion remains a disturbance under the registered
rule. Do not equate ordinary task completion with background completion or
interpret the infrastructure smoke as comparative scores.

For ten tasks, three arms and two conditions: an n=1 pilot is **60 trials**;
the method's n=5 is **300 trials**. At the configured $6 guard the planning
envelopes are **$360** and **$1,800**, excluding retries, other preflight calls
and any separately enabled provider. These are conservative guard-based budgets,
not observed cost forecasts. Measure a paid pilot before estimating actual
spend. **Neither matrix was run.**

## Later experimental arms (not built or enabled)

Keep the qualified `cc-hermes-cua-jev` arm unchanged as the script-off control.
Two independent follow-ups can reuse the explicit binary, skills, source,
interpreter and plugin inputs above:

- **Hermes/cua-jev plus `cua_script`:** wait for the plugin implementation that
  routes `cua_script` through driver `run_script` with the driver's input policy.
  Pin that plugin commit, then add a separately named opt-in arm in `arms.py` and
  `claude_arms.CUA_BUILDS`, with its own daemon/socket/state, script enabled, and
  a separate exact tool allowlist in `hermes_mcp.py`. Do not simply add shell or
  unrestricted script tools to the current seven-tool adapter. Preserve Hermes
  approvals, leases, hard blocks and driver input-policy refusals. Extend
  `tests/test_hermes_arm.py` and preflight expected tools to prove the real
  `cua_script` route, allowed input and exact policy refusal through MCP before
  adding the new arm to a separately reviewed qualification/smoke schedule.
  Record prompt/addendum differences; do not relabel existing three-arm evidence.
- **Driver from [fork PR #41](https://github.com/cresslank/cua/pull/41):** the
  requested candidate is `dd8996f94e88e65c42acb2c87b35ac2355f9406d`.
  Fetch and verify that exact source separately, then build with the same normal
  release profile, `nice -n 19`, source stamp and matching archived skills.
  Reuse `build.py --driver`, `integrate.py --driver/--skills/--recorder`, and
  `tools/make_linux_pins.py` to make a **separate immutable image and pins file**.
  Use a fresh evidence directory and offline qualification before any authorized
  paid smoke. This is a binary variant, not a silent replacement of the current
  control; compare separate image runs unless distinct per-arm build/pin inputs
  are implemented and tested. Do not weaken identity checks or touch the live
  driver service.

Neither follow-up is built, installed, qualified or included in `qualify.ARMS`.
Adding an arm changes trial counts and cost envelopes and requires explicit paid
run authorization. The default arm set and upstream macOS pins remain unchanged.
