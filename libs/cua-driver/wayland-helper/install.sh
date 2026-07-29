#!/usr/bin/env bash
# Install the cua WinRects GNOME Shell extension — supplies window screen
# geometry (for AT-SPI coordinate reconstruction) and renders the agent cursor
# on GNOME Mutter Wayland, where a normal client can do neither. Best-effort:
# cua-driver works without it (just no screen coords / no cursor on Mutter).
set -euo pipefail
UUID="winrects@cua"
SRC="$(cd "$(dirname "$0")" && pwd)/$UUID"
DEST="${XDG_DATA_HOME:-$HOME/.local/share}/gnome-shell/extensions/$UUID"
VERSION=$(python3 - "$SRC/metadata.json" <<'PY'
import json
import sys
with open(sys.argv[1], encoding="utf-8") as handle:
    print(json.load(handle)["version"])
PY
)
ENABLE=false
if [[ "${1:-}" == "--enable" ]]; then
  ENABLE=true
elif [[ $# -gt 0 ]]; then
  echo "usage: $0 [--enable]" >&2
  exit 2
fi
python3 - "$SRC" "$DEST" <<'PY'
import ctypes
import errno
import os
from pathlib import Path
import shutil
import stat
import sys
import tempfile


source = Path(sys.argv[1])
destination = Path(sys.argv[2])
if not source.is_dir() or source.is_symlink():
    raise SystemExit(f"invalid helper source directory: {source}")

source_files = set()
for path in source.rglob("*"):
    mode = path.lstat().st_mode
    if stat.S_ISLNK(mode):
        raise SystemExit(f"helper source contains symlink: {path}")
    if stat.S_ISREG(mode):
        source_files.add(path.relative_to(source))
    elif not stat.S_ISDIR(mode):
        raise SystemExit(f"helper source contains unsupported entry: {path}")
if not source_files:
    raise SystemExit("helper source contains no runtime files")

parent = destination.parent
parent.mkdir(parents=True, exist_ok=True)
if os.path.lexists(destination):
    mode = destination.lstat().st_mode
    if not stat.S_ISDIR(mode) or stat.S_ISLNK(mode):
        raise SystemExit(f"helper destination is not a real directory: {destination}")

stage = Path(tempfile.mkdtemp(prefix=f".{destination.name}.new-", dir=parent))
published = False
try:
    for child in source.iterdir():
        staged_child = stage / child.name
        if child.is_dir():
            shutil.copytree(str(child), str(staged_child), symlinks=True)
        else:
            shutil.copy2(str(child), str(staged_child), follow_symlinks=False)
    staged_files = set()
    for path in stage.rglob("*"):
        mode = path.lstat().st_mode
        if stat.S_ISLNK(mode):
            raise SystemExit(f"staged helper contains symlink: {path}")
        if stat.S_ISREG(mode):
            staged_files.add(path.relative_to(stage))
        elif not stat.S_ISDIR(mode):
            raise SystemExit(f"staged helper contains unsupported entry: {path}")
    if staged_files != source_files:
        raise SystemExit("staged helper runtime does not match the source closure")

    for path in sorted(stage.rglob("*"), reverse=True):
        if path.is_file():
            with path.open("rb") as handle:
                os.fsync(handle.fileno())
        elif path.is_dir():
            descriptor = os.open(path, os.O_RDONLY | os.O_DIRECTORY)
            try:
                os.fsync(descriptor)
            finally:
                os.close(descriptor)
    descriptor = os.open(stage, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)

    if not os.path.lexists(destination):
        os.rename(stage, destination)
        published = True
    else:
        libc = ctypes.CDLL(None, use_errno=True)
        renameat2 = getattr(libc, "renameat2", None)
        if renameat2 is None:
            raise SystemExit("atomic helper replacement requires renameat2")
        renameat2.argtypes = [
            ctypes.c_int,
            ctypes.c_char_p,
            ctypes.c_int,
            ctypes.c_char_p,
            ctypes.c_uint,
        ]
        renameat2.restype = ctypes.c_int
        at_fdcwd = -100
        rename_exchange = 2
        if renameat2(
            at_fdcwd,
            os.fsencode(stage),
            at_fdcwd,
            os.fsencode(destination),
            rename_exchange,
        ) != 0:
            error = ctypes.get_errno()
            if error in (errno.ENOSYS, errno.EINVAL, errno.ENOTSUP):
                raise SystemExit("atomic helper replacement is unavailable")
            raise OSError(error, os.strerror(error), destination)
        published = True
        try:
            shutil.rmtree(stage)
        except OSError as error:
            print(f"warning: could not remove previous helper tree: {error}", file=sys.stderr)

    try:
        descriptor = os.open(parent, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(descriptor)
        finally:
            os.close(descriptor)
    except OSError as error:
        print(f"warning: could not fsync helper parent: {error}", file=sys.stderr)
finally:
    if not published and stage.exists():
        try:
            shutil.rmtree(stage)
        except OSError as error:
            print(f"warning: could not remove staged helper tree: {error}", file=sys.stderr)
PY
echo "Staged $UUID protocol v$VERSION to $DEST."

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
