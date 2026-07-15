#!/usr/bin/env bash
# Install the cua WinRects GNOME Shell extension — supplies window screen
# geometry (for AT-SPI coordinate reconstruction) and renders the agent cursor
# on GNOME Mutter Wayland, where a normal client can do neither. Best-effort:
# cua-driver works without it (just no screen coords / no cursor on Mutter).
set -euo pipefail
UUID="winrects@cua"
SRC="$(cd "$(dirname "$0")" && pwd)/$UUID"
DEST="${XDG_DATA_HOME:-$HOME/.local/share}/gnome-shell/extensions/$UUID"
ENABLE=false
if [[ "${1:-}" == "--enable" ]]; then
  ENABLE=true
elif [[ $# -gt 0 ]]; then
  echo "usage: $0 [--enable]" >&2
  exit 2
fi
mkdir -p "$DEST"
cp -f "$SRC/metadata.json" "$SRC/extension.js" "$SRC/policy.js" "$DEST/"
echo "Staged $UUID protocol v6 to $DEST."

if ! $ENABLE; then
  echo "Not enabling it automatically. Re-run with --enable after warning the user."
  echo "On GNOME Wayland, loading this new extension code requires ONE logout/login."
  exit 0
fi

# Explicit --enable: add to the enabled set while preserving existing entries.
cur=$(gsettings get org.gnome.shell enabled-extensions 2>/dev/null || echo "@as []")
python3 - "$cur" "$UUID" <<'PY'
import sys, ast
try: l = ast.literal_eval(sys.argv[1])
except Exception: l = []
if sys.argv[2] not in l: l.append(sys.argv[2])
import subprocess
subprocess.run(["gsettings","set","org.gnome.shell","enabled-extensions",str(l)])
print("enabled-extensions ->", l)
PY
echo "Enabled $UUID in settings."
echo "LOGOUT/LOGIN REQUIRED ONCE: GNOME Wayland cannot reload Shell in place safely."
echo "After login: gnome-extensions info $UUID should show State: ACTIVE."
