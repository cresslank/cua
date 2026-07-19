#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
HELPER="$ROOT/wayland-helper"
DEVKIT=/usr/libexec/mutter-devkit

if [[ ! -x "$DEVKIT" ]]; then
  printf 'BLOCKED: %s is not installed; deterministic policy/Rust gates remain available.\n' "$DEVKIT" >&2
  exit 77
fi

work=$(mktemp -d "${TMPDIR:-/tmp}/cua-gnome-devkit.XXXXXX")
cleanup() {
  rm -rf -- "$work"
  rm -f -- "${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/gnome-shell-disable-extensions"
}
trap cleanup EXIT INT TERM

mkdir -p "$work/extension"
gnome-extensions pack -f \
  -o "$work/extension" \
  --extra-source=policy.js \
  "$HELPER/winrects@cua"

XDG_CURRENT_DESKTOP=GNOME \
XDG_SESSION_DESKTOP=gnome \
dbus-run-session -- \
  gnome-shell-test-tool \
    "$HELPER/tests/gnome_devkit.mjs" \
    --devkit \
    --disable-animations \
    --extension "$work/extension/winrects@cua.shell-extension.zip"
