# Scratch Windows Edge foreground diagnostic

**Do not merge this branch.** No Cargo build, installation, service registration,
input synthesis, foreground bypass, driver patch, PR, or production deployment.
Native execution is guarded to GitHub-hosted Windows in `cresslank/cua`.

The scratch branch repurposes the **already registered**
`.github/workflows/ci-driver-mcp-candidate.yml` (workflow ID `384010651`). Reusing
that path avoids depending on default-branch registration of a new workflow file.
The workflow is manual-only and refuses refs outside `scratch/windows-edge-*`.
The normal candidate producer remains unchanged on all other branches.

## Exercise after separately authorized branch publication

The local implementation is not pushed. After review and a separately authorized
commit/push of `scratch/windows-edge-diagnostics-20261025`:

```sh
gh workflow run 384010651 -R cresslank/cua \
  --ref scratch/windows-edge-diagnostics-20261025 \
  -f candidate_run=40173066492 \
  -f candidate_sha=8871472ea121943ff2ce12ac81c6ab7fcab8d915 \
  -f lock_timeout_control=false

gh run list -R cresslank/cua --workflow ci-driver-mcp-candidate.yml \
  --branch scratch/windows-edge-diagnostics-20261025 --event workflow_dispatch \
  --limit 3 --json databaseId,headSha,status,conclusion,url
```

Use `-f lock_timeout_control=true` to include the optional third control. It uses
`SPI_SETFOREGROUNDLOCKTIMEOUT`, does not persist the setting, and restores and
reads back the original API value in `finally`.

Once the new run appears, verify its `headSha` against the published harness
commit, then substitute its run ID below (not the candidate build's ID):

```sh
gh run watch RUN_ID -R cresslank/cua --exit-status
gh run download RUN_ID -R cresslank/cua \
  --name edge-foreground-diagnostic-RUN_ID --dir edge-foreground-evidence-RUN_ID
```

A red run is expected when any original driver response or native oracle fails.
All variants still run, evidence uploads with `always()`, and no error is converted
to a successful qualification. Missing/expired artifacts or setup failures are
explicitly **BLOCKED**, not rebuilt or replaced.

## Controls and observations

One real `launch_app` call per variant, no relaunch/ambiguous-outcome retry:

1. Original runner Edge policy, `name: "Microsoft Edge"`,
   `urls: ["https://example.com"]`, and arguments `--no-first-run` and
   `--no-default-browser-check`.
2. Restart disposable Edge with explicit HKLM
   `SOFTWARE\Policies\Microsoft\Edge\HideFirstRunExperience=1` (DWORD).
3. Optional: restore the original Edge policy and repeat with the supported
   foreground lock timeout temporarily set to zero.

Policy and timeout originals, mutations, restorations and readbacks are retained.
These sequential controls share the runner's Edge profile; they do **not** prove a
pristine-profile causal A/B comparison. Registry readback proves the configured
policy, not that the browser consumed it. Actual browser command lines show
whether the shell preserved onboarding arguments. URL dispatch intentionally
remains the original separate default-browser operation; its handler is recorded.

Each variant has a fresh private daemon and stdio proxy using actual source
contracts: `serve --socket ... --pid-file ... --no-overlay
--no-permissions-gate` and `mcp --socket ...`. The daemon's source-reviewed
unrestricted startup variables are explicit. `--no-permissions-gate` matches the
Windows testkit (the gate is macOS-specific); it is **not** a Windows foreground
permission bypass. Driver state uses testkit's real `HOME` plus
`CUA_DRIVER_RS_HOME` overrides. There is no `CUA_DRIVER_STATE_ROOT` product
variable in this source; `USERPROFILE`, `APPDATA` and `LOCALAPPDATA` must remain
unchanged for AppsFolder discovery. Readiness completes the real named-pipe
`{"method":"list"}` / `ok:true` exchange, not merely PID-file existence.

A responsive owned Win32 `STATIC` window stays alive on its own pumping thread.
Only while it is foreground does the harness call
`AllowSetForegroundWindow(exact_daemon_pid)`, never the proxy or `ASFW_ANY`.
The independent oracle requires the same fixture HWND **and owner PID**, alive
and responsive, at tool return and after a passive 500 ms readback. Driver success
also requires a non-error tool result and positive process ID. Failure of the
follow-up `list_windows` read is reported separately and fails the run.

## Evidence

- `candidate-run.json`, `artifact-verification.json`, `provenance.json`: successful
  producer run, exact candidate source, archive hash, and executable hashes.
  Archive inventory/type/size and identical inner/outer provenance are checked
  before any execution; the harness rechecks all executable bytes.
- `native-timeline.jsonl`: timestamped foreground WinEvents, top-level window
  lifecycle/name events, 100 ms foreground samples, phase boundaries, HWND,
  PID/TID, title, class, image, ownership, GUI-thread active/focus/menu state.
- Per variant: exact RPC requests/responses; daemon/proxy stdout and stderr;
  process executable paths/arguments/PIDs/sessions before, during and after
  launch; visible native windows; original fixture identity and ASFW result;
  immutable `oracle-before-diagnostic-controls.json`; result/exception details.
- `harness-sfw-control-NOT-driver-success.json`: **post-oracle** plain native
  `SetForegroundWindow` boolean, cleared/read-back last error, immediate/100 ms
  foreground readbacks, caller PID/thread. It cannot change the recorded verdict.
  SetForegroundWindow does not document extended last-error semantics; zero is
  not proof of permission or an explanation for failure.
- `summary.json` / `summary.md`: per-variant driver/native results, expected vs
  completed inventory, blocked/failure and cleanup/infrastructure errors.

**Important observability limit:** the unchanged prebuilt driver does not expose
its internal SetForegroundWindow return/error values. External WinEvents and
native readbacks show actual transitions, but not why Windows rejected a driver's
request. The separately labeled harness control is from a different caller and
cannot establish the driver's foreground rights. Investigating internal return
values requires a separately authorized instrumented artifact; this harness
neither injects into the driver nor invents them.

Every owned driver/proxy and fixture is reaped. Disposable Edge processes are
stopped between variants and at teardown. Registry and timeout restoration are
attempted independently even if another cleanup fails; cleanup errors fail the
run. Private state/evidence remain only on the disposable runner and in its
seven-day diagnostic artifact; no browser profile is uploaded.

## Portable checks (not Windows qualification)

```sh
pwsh -NoLogo -NoProfile -File scripts/ci/windows-edge/test-contracts.ps1
actionlint .github/workflows/ci-driver-mcp-candidate.yml
git diff --check
```

The first command parses PowerShell, compiles C# without calling Windows APIs,
tests pure verdict rules (including `isError=true` with no structured PID), and
exercises actual subprocess stdin/stdout/stderr and cleanup. Native foreground,
AppsFolder resolution, policy access and artifact execution still require the
manual Windows run above.
