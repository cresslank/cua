#!/usr/bin/env bash
# Private VM only. Never invoke this script on the shared workstation.
set -euo pipefail
repo=$(pwd -P)
source_sha=$(git rev-parse HEAD)
test "$source_sha" = "$GITHUB_SHA"
test -z "$(git status --porcelain --untracked-files=all)"
scratch=$(mktemp -d "${RUNNER_TEMP:?}/kdesk-driver.XXXXXXXX")
output=${KDESK_OUTPUT:?}
mkdir -p "$output" "$scratch/home" "$scratch/wrapper" "$scratch/cargo" "$scratch/xdg-runtime"
chmod 700 "$scratch/xdg-runtime"
# The runner image must already contain this exact toolchain; no moving stable
# or automatic rustup installation is allowed.
toolchain=$(python3 -c 'import tomllib; print(tomllib.load(open("libs/cua-driver/rust/rust-toolchain.toml", "rb"))["toolchain"]["channel"])')
expected=$(python3 -c 'import json; print(json.load(open(".github/kdesk-cua-driver-policy.json"))["toolchain"])')
test "$toolchain" = "$expected"
rustup toolchain list | grep -Eq "^${toolchain//./\\.}(-x86_64-unknown-linux-gnu)?( |$)"
real_cargo=$(rustup which --toolchain "$toolchain" cargo)
real_rustc=$(rustup which --toolchain "$toolchain" rustc)
# Keep the installed toolchain outside isolated HOME while downloads and targets
# are per-job. The wrapper applies only to installer cargo build invocations.
cat >"$scratch/wrapper/cargo" <<'WRAPPER'
#!/usr/bin/env bash
set -euo pipefail
if [ "${1:-}" = build ]; then
    exec "$KDESK_REAL_CARGO" "$@" --locked --features cua-driver/portal-input
fi
exec "$KDESK_REAL_CARGO" "$@"
WRAPPER
chmod 755 "$scratch/wrapper/cargo"
# Start from an allowlist, not the runner's ambient environment. In particular
# no GitHub credential, desktop/session bus, proxy, or user socket enters Cargo.
env -i PATH="$scratch/wrapper:/usr/bin:/bin" HOME="$scratch/home" \
    CARGO_HOME="$scratch/cargo" CARGO_TARGET_DIR="$scratch/target" CARGO_BUILD_JOBS=4 \
    RUSTC="$real_rustc" RUSTDOC="$(dirname "$real_rustc")/rustdoc" \
    KDESK_REAL_CARGO="$real_cargo" CUA_DRIVER_SOURCE_SHA="$source_sha" \
    CUA_DRIVER_LOCAL_HOME="$scratch/package" CUA_DRIVER_LOCAL_INSTALL_DIR="$scratch/bin" \
    XDG_CONFIG_HOME="$scratch/config" XDG_CACHE_HOME="$scratch/cache" \
    XDG_DATA_HOME="$scratch/data" XDG_STATE_HOME="$scratch/state" XDG_RUNTIME_DIR="$scratch/xdg-runtime" \
    DO_NOT_TRACK=1 CUA_DRIVER_TELEMETRY_ENABLED=false CUA_DRIVER_RS_UPDATE_CHECK=false \
    bash libs/cua-driver/scripts/install-local.sh --release
# Last installer cargo command has finished. Package the complete real directory,
# never current/visible-bin links. A changed layout holds rather than stripping it.
mapfile -t releases < <(find "$scratch/package/packages/releases" -mindepth 1 -maxdepth 1 -type d)
test "${#releases[@]}" -eq 1
stage=${releases[0]}
python3 .github/scripts/kdesk-cua-smoke.py --binary "$stage/cua-driver-local" --source-sha "$source_sha" --output "$output/smoke.json"
{
    ldd "$stage/cua-driver-local"
    ldd "$stage/cua-cursor-theme"
} >"$output/elf-dependencies.txt"
! grep -q 'not found' "$output/elf-dependencies.txt"
# Source tests follow the Linux unit contract. GUI/uinput ignored tests are not
# claimed here: this VM has no owner's desktop or physical-input devices.
(
    cd libs/cua-driver/rust
    export HOME="$scratch/home" CARGO_HOME="$scratch/cargo" CARGO_TARGET_DIR="$scratch/target"
    export CARGO_BUILD_JOBS=4 RUSTC="$real_rustc" RUSTDOC="$(dirname "$real_rustc")/rustdoc" CUA_DRIVER_SOURCE_SHA="$source_sha"
    "$real_cargo" test --locked --features cua-driver/portal-input \
        -p cua-driver -p cua-driver-contract -p cua-driver-core -p cua-driver-testkit \
        -p cursor-overlay -p cursor-theme-cli -p platform-linux --all-targets
    # Feature-disabled public diagnostic, using a separate Cargo target.
    CARGO_TARGET_DIR="$scratch/no-feature-target" "$real_cargo" build --locked -p cua-driver
) 2>&1 | tee "$output/test-receipt.txt"
mkdir "$scratch/no-feature"
cp "$scratch/no-feature-target/debug/cua-driver" "$scratch/no-feature/cua-driver-local"
python3 .github/scripts/kdesk-cua-smoke.py --binary "$scratch/no-feature/cua-driver-local" --source-sha "$source_sha" --portal-input false --output "$output/smoke-no-feature.json"
# Deliberately break feature forwarding: the no-feature binary must fail the
# release smoke's true requirement, through the actual MCP process boundary.
if python3 .github/scripts/kdesk-cua-smoke.py --binary "$scratch/no-feature/cua-driver-local" --source-sha "$source_sha" --output "$scratch/must-not-pass.json" >"$scratch/negative.log" 2>&1; then
    echo 'feature-forwarding negative unexpectedly passed' >&2; exit 1
fi
grep -q 'source/feature diagnostic mismatch' "$scratch/negative.log"
printf '\nFeature-forwarding deliberate break: intended assertion failed; restored disabled assertion passed.\n' >>"$output/test-receipt.txt"
# Deliberately suppress real export at the process boundary, require the positive
# assertion to fail, then restore opt-in and prove actual records again.
python3 - "$stage/cua-driver-local" "$source_sha" <<'PY' >>"$output/test-receipt.txt"
import importlib.util
from pathlib import Path
import sys
spec = importlib.util.spec_from_file_location('smoke', '.github/scripts/kdesk-cua-smoke.py')
smoke = importlib.util.module_from_spec(spec)
spec.loader.exec_module(smoke)
collector = smoke.Collector()
try:
    binary, source = Path(sys.argv[1]), sys.argv[2]
    smoke.dispatch(binary, source, True, collector.endpoint, 'sdk-disabled')
    try:
        smoke.require(bool(smoke.decode(collector.requests, source)), 'no actual get_config OTLP export')
    except ValueError as error:
        assert str(error) == 'no actual get_config OTLP export'
    else:
        raise ValueError('OTLP deliberate break unexpectedly passed')
    smoke.dispatch(binary, source, True, collector.endpoint, 'on')
    smoke.require(bool(smoke.decode(collector.requests, source)), 'restored OTLP export failed')
    print('OTLP deliberate suppression: intended positive assertion failed; restored opt-in passed.')
finally:
    collector.close()
PY
{
    du -sk "$scratch"
    if [ -r /sys/fs/cgroup/memory.peak ]; then
        printf 'container_memory_peak_bytes='
        cat /sys/fs/cgroup/memory.peak
    fi
} >"$output/build-resource-usage.txt"
python3 .github/scripts/kdesk-cua-package.py assemble --repo "$repo" --stage "$stage" --output "$output" --source-sha "$source_sha" --toolchain "$toolchain"
# Installer/tests must not dirty any tracked source after verification.
test -z "$(git status --porcelain --untracked-files=all)"
