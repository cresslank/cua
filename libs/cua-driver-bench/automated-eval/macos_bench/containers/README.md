<!-- SPDX-License-Identifier: FSL-1.1-MIT -->
<!-- Copyright (c) 2026 Cua AI, Inc. -->
# Disposable Linux Bench v2 desktop

This image owns its Xvfb, openbox, picom, session D-Bus, AT-SPI registry,
applications and private driver processes. It is not a client of the host desktop.
No host `/tmp`, home, GUI/runtime socket, Docker socket or device is mounted.
The only default bind mount is a newly allocated evidence directory. Explicit
optional readonly mounts are `CDB_CUA_ALLOWLIST` (authorized `cua:*` JSON scopes)
and exactly one of `CDB_CLAUDE_TOKEN_FILE` (setup-token billing) or
`CDB_ANTHROPIC_API_KEY_FILE` (Console API-key billing). Both credential inputs
must be owned, nonsymlink, regular 0600 files and are mounted read-only. Setting
both refuses with `Exactly one Claude credential mode is allowed; both credential files are set`.
No login profile is discovered or copied. See [the Hermes arm guide](../HERMES_LINUX.md)
for the integration layer, offline checks and bounded six-trial smoke.

## Build and run

From the repository root (Docker access and host Python 3 are the only build
prerequisites):

```sh
python3 libs/cua-driver-bench/automated-eval/macos_bench/containers/build.py \
  --driver /home/lantzk/.local/bin/cua-driver-local \
  --tag cua-bench-linux:disposable \
  --scratch /home/lantzk/.hermes/cache/scratch/bench

# EVIDENCE_DIR must not exist; its parent must exist and be outside the checkout.
bash libs/cua-driver-bench/automated-eval/macos_bench/containers/run.sh \
  /home/lantzk/.hermes/cache/scratch/bench/container-smoke-new

python3 libs/cua-driver-bench/automated-eval/macos_bench/containers/test_launcher.py \
  --scratch /home/lantzk/.hermes/cache/scratch/bench
```

The default command checks the copied driver's hash and private daemon health,
runs upstream `tests/test_pilot_runner.py` (including real GTK actions, screenshots,
sentinel and driver recording), then resets and starts all three unmodified Linux
pack descriptors and witnesses each named window through X11 and AT-SPI. It also
checks that LibreOffice's welcome and GnuCash's tip dialogs do not obstruct them.
These are infrastructure checks, not scored agent trials.

To run another command in **the same private desktop/session**:

```sh
bash libs/cua-driver-bench/automated-eval/macos_bench/containers/run.sh \
  /home/lantzk/.hermes/cache/scratch/bench/container-doctor-new -- \
  bash -c 'python3 desktop_runtime.py doctor --cua-bin "$CUA_DRIVER_BIN" --out "$CDB_SESSION_DIR" --json'
```

`build.py` copies only tracked harness/taskpack/skill bytes, plus its explicit
container-file allowlist and the single resolved driver binary, into a fresh
`container-build-*` context. It does not copy `.git`, ignored dependencies,
installed application profiles, host skills, or credentials. New untracked
integration modules are intentionally absent: stage their exact public paths or
extend the allowlist before rebuilding. The recipe records staged file digests in
`/opt/cua-container/snapshot.json`, package versions in `packages.tsv`, and the
selected driver hash in `driver.sha256`. `receipt.json` and `image.id` remain in
the build context. HEAD identifies the base; the snapshot identifies dirty bytes.

The raw build command executed by the wrapper is:

```sh
docker build --progress=plain --iidfile "$CONTEXT/image.id" \
  --build-arg DRIVER_SHA256="$DRIVER_SHA256" \
  --build-arg BENCH_SOURCE_REVISION="$BENCH_SOURCE_REVISION" \
  -t cua-bench-linux:disposable -f "$CONTEXT/containers/Dockerfile" "$CONTEXT"
```

`run.sh` resolves the image to its immutable ID, then executes the following
Docker specification (see the script for its signal/status/cleanup supervision):

```sh
docker run --detach --name "$NAME" --hostname cua-bench-private \
  --cidfile "$EVIDENCE/container.id" \
  --network none --ipc private --shm-size 1g \
  --cap-drop ALL --security-opt no-new-privileges --read-only \
  --user "$(id -u):$(id -g)" --pids-limit 2048 --memory 12g --cpus 6 \
  --tmpfs /tmp:rw,noexec,nosuid,nodev,mode=1777 \
  --tmpfs /run:rw,noexec,nosuid,nodev,mode=755 \
  --tmpfs "/home/bench:rw,exec,nosuid,nodev,mode=700,uid=$(id -u),gid=$(id -g)" \
  --mount "type=bind,src=$EVIDENCE,dst=/evidence" \
  --env CDB_SESSION_TIMEOUT=600 --env CDB_SESSION_GRACE=10 \
  "$IMAGE_ID" [COMMAND ARG...]
```

The image entrypoint is `tini -- bench-container-entrypoint`; it clears the
environment and invokes upstream `launch/linux_session.py`. The launcher retains
`container-inspect.json`, container ID/status and `docker-exit`, waits for the
container's actual status, and verifies exact-target removal on success, failure
and cancellation. It deliberately does not rely on killing an attached Docker
client to stop daemon-owned work. Evidence remains; the container is removed.

## Dependencies and environment contract

- Ubuntu 26.04 is digest-pinned. The selected local driver requires **GLIBC_2.43**;
  older Ubuntu 24.04 and Debian trixie are not suitable for these exact bytes.
- Desktop packages: Xvfb, xauth, dbus-x11, at-spi2-core, openbox, picom,
  python3-gi, GTK3/AT-SPI GIR packages, xdotool, x11-utils, libxss1,
  libglib2.0-bin, ffmpeg with x11grab, lsof, procps, fonts and Chromium/Electron
  runtime libraries. Test support: system pytest and Pillow.
- Pack requirements are Python >=3.11, Node >=22, Electron 43.8.1 from each
  lockfile, LibreOffice >=25, GnuCash >=5.16 and a Chromium-family browser named
  `google-chrome`. The four Electron app directories are installed at build time.
- Ubuntu supplies only GnuCash 5.14. A separate build stage compiles upstream
  **5.16**, verifies its published source SHA256, and installs it under
  `/opt/gnucash`. The final image retains the distro package for its runtime
  dependency closure, but PATH selects 5.16. Its library directory is registered
  with ldconfig so Guile's `dlopen` can resolve `libgnc-expressions-guile.so`.
  The GnuCash shim selects the 5.16 schema directory. No build toolchain is
  copied into the final image.
- Ubuntu's Chromium package is a snap launcher. This image instead installs
  Chrome from Google's signed apt repository. `google-chrome` and `npx electron`
  shims add `--no-sandbox`: Docker's default seccomp/no-new-privileges prevents
  Chromium's inner sandbox. This exception is only for the disposable outer
  container; do not use these shims as host launchers.
- First-run/tip suppression changes application defaults in the image, not task
  data or runner Python. Both `Common/Misc` and `Setup/Product` carry a WhatsNew
  boolean; both must be disabled to suppress LibreOffice's new welcome dialog.

The entrypoint supplies these fixed values:

```text
HOME=/home/bench USER=bench LOGNAME=bench LANG=C.UTF-8
PATH=/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin
TMPDIR=/home/bench/tmp XDG_RUNTIME_DIR=/home/bench/runtime
XDG_SESSION_TYPE=x11 GDK_BACKEND=x11 GTK_A11Y=always NO_AT_BRIDGE=0
QT_ACCESSIBILITY=1 QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1
CUA_DRIVER_RS_TELEMETRY_ENABLED=false DO_NOT_TRACK=1
CUA_DRIVER_BIN=/usr/local/bin/cua-driver-local CUA_SKILLS=/opt/cua-skills
CDB_BENCH_DISPOSABLE=1 CDB_BENCH_WORK=/evidence/work
CDB_TASKPACK=/opt/cua-bench/taskpack PYTHONDONTWRITEBYTECODE=1
```

Upstream's session supervisor generates `DISPLAY`, `XAUTHORITY`,
`DBUS_SESSION_BUS_ADDRESS`, `CDB_SESSION_DIR=/evidence/session` and
`CDB_BENCH_PID_LOG` privately. Do **not** forward host equivalents or a host
`WAYLAND_DISPLAY`. Use `/usr/bin/python3` for GTK/AT-SPI. A managed interpreter
must be a child of COMMAND inside this session; `docker exec` alone does not
inherit its display/bus. The image does not include a Hermes interpreter or
model CLI/authentication; those are a separate integration layer.

Host-side knobs are only `CDB_CONTAINER_IMAGE`, `CDB_CONTAINER_NETWORK` (`none`
by default, or explicit `bridge`), `CDB_SESSION_TIMEOUT` (600 seconds by default)
and `CDB_SESSION_GRACE` (10 seconds). No model credentials are forwarded. The
network-none default still allows the pack's container-local loopback service;
model/API integration needs an explicitly designed network/auth layer.

The Ubuntu and Chrome apt package versions are recorded, not fully locked;
future rebuilds may differ. Use the immutable built image ID for repeat trials.
Source builds/downloads happen only during build, never on normal launch.

## Initial artifact (historical)

The artifact below predates the certificate-block/release-driver refresh. Use
[HERMES_LINUX.md](../HERMES_LINUX.md) for the current source-stamped release,
rebuilt immutable image and requalification commands.

On the initial verified image:

```text
image: cua-bench-linux:disposable
ID: sha256:b5987ee8471b717004c236f094680ff6efa451718074ac794b8d96535e2c5e96
base revision: c1df825cc7ad1257f73eef13f019763ab672bbc7
staged snapshot: 9bf1f01655923aca96dbd70582bd40a71484f95636784de2ea987ec8af22b560
driver version: 0.38.0
driver SHA256: 32ec15b834a66f50b779aa98a50870e682ec52c1c76eedff0b20b55c2c581a19
Python: 3.14.4; Node: 22.22.1; npm: 9.2.0; Electron: 43.8.1
LibreOffice: 26.2.6.3; GnuCash: 5.16; Chrome: 156.0.8078.79
```

`container-smoke-final` exited 0: four doctor checks passed; upstream pilot suite
reported **28 passed, 14 subtests passed**, no skips; all three pack desktop/app
checks passed. `container-check-eu040_ko` passed environment isolation, exit-7
propagation, timeout-124, deliberate orphan detection and SIGTERM cancellation,
including exact container-removal readback. Ruff 0.14.10 check/format passed.

**Original release-artifact provenance gate:** the copied installed driver's real health report
has `build.git_sha=""`. Its executable hash is verified end-to-end, but upstream
`make_linux_pins.py` requires a nonempty Git identity. The real
`container-pins-refusal` invocation exited **1** with `doctor build has no git_sha`.
Do not invent one from its release directory name. The copied upstream skill
also is not a claim that it matches the local hardened driver's source. No
scored agent run, model request, host GUI access or installed-driver deployment
was performed by this base-image check. The integrated qualification described
in [HERMES_LINUX.md](../HERMES_LINUX.md) resolves the source-identity gate with a
genuine source-stamped release artifact and matching archived skills, without
weakening the pins command or relabeling the installed release. The initial
integration used debug bytes; its timing is not a release baseline.
