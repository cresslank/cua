#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
HELPER="$ROOT/wayland-helper"
DEVKIT=/usr/libexec/mutter-devkit
BUS_CONFIG="$HELPER/tests/gnome_session_bus.conf"
NO_X11_WRAPPER="$HELPER/tests/gnome_no_x11.sh"

if [[ ! -x "$DEVKIT" ]]; then
  printf 'BLOCKED: %s is not installed; deterministic policy/Rust gates remain available.\n' "$DEVKIT" >&2
  exit 77
fi

test_root=${XDG_RUNTIME_DIR:-${TMPDIR:-/tmp}}
work=$(mktemp -d "$test_root/cua-gnome-devkit.XXXXXX")
disable_marker="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/gnome-shell-disable-extensions"
marker_preexisting=false
if [[ -e "$disable_marker" ]]; then
  marker_preexisting=true
fi
cleanup() {
  rm -rf -- "$work"
  if [[ "$marker_preexisting" == false ]]; then
    rm -f -- "$disable_marker"
  fi
}
trap cleanup EXIT INT TERM

mkdir -p "$work/extension"
gnome-extensions pack -f \
  -o "$work/extension" \
  --extra-source=policy.js \
  "$HELPER/winrects@cua"

GDK_BACKEND=wayland \
GIO_USE_VFS=local \
GTK_A11Y=none \
NO_AT_BRIDGE=1 \
XDG_CURRENT_DESKTOP=GNOME \
XDG_SESSION_DESKTOP=gnome \
timeout --kill-after=10s 90s \
dbus-run-session --config-file="$BUS_CONFIG" -- \
  gnome-shell-test-tool \
    "$HELPER/tests/gnome_devkit.mjs" \
    --devkit \
    --disable-animations \
    --wrap "$NO_X11_WRAPPER" \
    --extension "$work/extension/winrects@cua.shell-extension.zip"
