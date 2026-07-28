#!/usr/bin/env python3
"""Descriptor-relative immutable release promotion and selector commit (Linux)."""
from __future__ import annotations

import ctypes
import errno
import hashlib
import json
import os
from pathlib import Path
import shutil
import stat
import sys

(
    raw_package, raw_releases, raw_bin, package_path, releases_path, bin_path,
    stage, final, binary_sha, skills_sha, source_sha, signing_sha, hints_sha,
    current_target, bin_target,
) = sys.argv[1:]
package_fd, releases_fd, bin_fd = map(int, (raw_package, raw_releases, raw_bin))

def same_path(fd: int, path: str, label: str) -> None:
    opened = os.fstat(fd)
    linked = os.stat(path, follow_symlinks=False)
    if not stat.S_ISDIR(linked.st_mode) or (opened.st_dev, opened.st_ino) != (linked.st_dev, linked.st_ino):
        raise SystemExit(f"{label} pathname was replaced")

def field(digest: "hashlib._Hash", value: bytes) -> None:
    digest.update(len(value).to_bytes(8, "big")); digest.update(value)

def skill_digest(root: Path) -> str:
    digest = hashlib.sha256()
    if root.is_dir():
        for path in sorted(root.rglob("*"), key=lambda item: item.relative_to(root).as_posix()):
            item = path.lstat()
            if path.is_symlink():
                raise SystemExit(f"symlink is not allowed in skill pack: {path}")
            relative = os.fsencode(path.relative_to(root).as_posix())
            mode = (stat.S_IMODE(item.st_mode) & 0o555).to_bytes(4, "big")
            if path.is_dir(): kind, payload = b"D", b""
            elif path.is_file(): kind, payload = b"F", path.read_bytes()
            else: raise SystemExit(f"unsupported release entry: {path}")
            for value in (kind, relative, mode, payload): field(digest, value)
    return digest.hexdigest()

def validate(name: str) -> Path:
    root = Path(f"/proc/self/fd/{releases_fd}/{name}")
    root_stat = root.lstat()
    if not root.is_dir() or stat.S_ISLNK(root_stat.st_mode):
        raise SystemExit("immutable release path is not a real directory")
    for path in (root, *root.rglob("*")):
        item = path.lstat()
        if stat.S_ISLNK(item.st_mode) or item.st_mode & 0o222:
            raise SystemExit(f"immutable release contains unsafe entry: {path}")
    binary = root / "cua-driver-local"
    if not binary.is_file() or hashlib.sha256(binary.read_bytes()).hexdigest() != binary_sha:
        raise SystemExit("immutable release binary digest mismatch")
    if skill_digest(root / "Skills") != skills_sha:
        raise SystemExit("immutable release skill digest mismatch")
    allowed = {"cua-driver-local", "provenance.json", "scripts", "scripts/_local-signing.sh", "Skills"}
    if hints_sha != "-":
        allowed.add("scripts/post-install-hints.txt")
    for path in root.rglob("*"):
        relative = path.relative_to(root).as_posix()
        if not (relative in allowed or relative.startswith("Skills/")):
            raise SystemExit(f"unexpected immutable release entry: {relative}")
    signing = root / "scripts/_local-signing.sh"
    if hashlib.sha256(signing.read_bytes()).hexdigest() != signing_sha:
        raise SystemExit("immutable signing-helper digest mismatch")
    hints = root / "scripts/post-install-hints.txt"
    if hints_sha == "-":
        if hints.exists(): raise SystemExit("unexpected post-install hints")
    elif not hints.is_file() or hashlib.sha256(hints.read_bytes()).hexdigest() != hints_sha:
        raise SystemExit("immutable post-install-hints digest mismatch")
    provenance = json.loads((root / "provenance.json").read_text())
    if provenance.get("source_sha") != source_sha or provenance.get("binary_sha256") != binary_sha or provenance.get("skills_sha256") != skills_sha:
        raise SystemExit("immutable provenance mismatch")
    return root

def freeze(root: Path) -> None:
    paths = [root, *root.rglob("*")]
    for path in reversed(paths):
        if not path.is_symlink():
            os.chmod(path, stat.S_IMODE(path.lstat().st_mode) & ~0o222, follow_symlinks=False)

def discard_staging(root: Path) -> None:
    for path in (root, *root.rglob("*")):
        if not path.is_symlink():
            os.chmod(path, stat.S_IMODE(path.lstat().st_mode) | 0o700, follow_symlinks=False)
    shutil.rmtree(root)

same_path(package_fd, package_path, "package root")
same_path(releases_fd, releases_path, "release store")
same_path(bin_fd, bin_path, "binary directory")
staged = Path(f"/proc/self/fd/{releases_fd}/{stage}")
freeze(staged)
validate(stage)
libc = ctypes.CDLL(None, use_errno=True)
renameat2 = libc.renameat2
renameat2.argtypes = [ctypes.c_int, ctypes.c_char_p, ctypes.c_int, ctypes.c_char_p, ctypes.c_uint]
if renameat2(releases_fd, os.fsencode(stage), releases_fd, os.fsencode(final), 1) != 0:
    error = ctypes.get_errno()
    if error != errno.EEXIST:
        raise OSError(error, os.strerror(error))
    validate(final)
    discard_staging(staged)
else:
    validate(final)
# Prepare both selector objects before replacing either one. Destination
# directories cannot be redirected because all operations use retained FDs.
current_tmp = f".current.new-{os.getpid()}"
bin_tmp = f".cua-driver-local.new-{os.getpid()}"
for fd, name in ((package_fd, "current"), (bin_fd, "cua-driver-local")):
    try:
        item = os.stat(name, dir_fd=fd, follow_symlinks=False)
    except FileNotFoundError:
        continue
    if not stat.S_ISLNK(item.st_mode):
        raise SystemExit(f"selector is not a symlink: {name}")
try:
    os.symlink(current_target, current_tmp, dir_fd=package_fd)
    os.symlink(bin_target, bin_tmp, dir_fd=bin_fd)
    os.replace(bin_tmp, "cua-driver-local", src_dir_fd=bin_fd, dst_dir_fd=bin_fd)
    os.replace(current_tmp, "current", src_dir_fd=package_fd, dst_dir_fd=package_fd)
finally:
    for fd, name in ((package_fd, current_tmp), (bin_fd, bin_tmp)):
        try: os.unlink(name, dir_fd=fd)
        except FileNotFoundError: pass
same_path(package_fd, package_path, "package root")
same_path(releases_fd, releases_path, "release store")
same_path(bin_fd, bin_path, "binary directory")
print(f"/proc/self/fd/{releases_fd}/{final}")
