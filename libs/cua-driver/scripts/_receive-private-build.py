#!/usr/bin/env python3
"""Receive the private build tar through a pipe and attest accepted bytes."""
from __future__ import annotations

import hashlib
import os
import sys
import tarfile

release_fd, stage = int(sys.argv[1]), sys.argv[2]
if "/" in stage or stage in ("", ".", ".."):
    raise SystemExit("unsafe staging name")
os.mkdir(stage, 0o700, dir_fd=release_fd)
stage_fd = os.open(stage, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=release_fd)
files: dict[str, tuple[str, int]] = {}
skill_entries: list[tuple[bytes, bytes, bytes, bytes]] = []
skill_dirs: list[tuple[int, int]] = []
try:
    with tarfile.open(fileobj=sys.stdin.buffer, mode="r|*") as archive:
        for member in archive:
            name = member.name.removeprefix("./").rstrip("/")
            if not name or name == ".":
                continue
            parts = name.split("/")
            if name.startswith("/") or any(part in ("", ".", "..") for part in parts):
                raise SystemExit("unsafe export path")
            if member.issym() or member.islnk():
                raise SystemExit(f"symlink is not allowed in build export: {name}")
            parent_fd = stage_fd
            opened: list[int] = []
            try:
                for component in parts[:-1]:
                    try:
                        os.mkdir(component, 0o700, dir_fd=parent_fd)
                    except FileExistsError:
                        pass
                    child = os.open(
                        component, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW,
                        dir_fd=parent_fd,
                    )
                    opened.append(child)
                    parent_fd = child
                leaf = parts[-1]
                if member.isdir():
                    try:
                        os.mkdir(leaf, 0o700, dir_fd=parent_fd)
                    except FileExistsError:
                        pass
                    directory_fd = os.open(
                        leaf, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW,
                        dir_fd=parent_fd,
                    )
                    skill_dirs.append((directory_fd, member.mode & 0o555))
                    if name.startswith("Skills/"):
                        relative = name.removeprefix("Skills/").encode()
                        skill_entries.append((b"D", relative, (member.mode & 0o555).to_bytes(4, "big"), b""))
                    continue
                if not member.isfile() or name in files:
                    raise SystemExit(f"unsupported or duplicate export entry: {name}")
                source = archive.extractfile(member)
                if source is None:
                    raise SystemExit("missing export payload")
                fd = os.open(
                    leaf,
                    os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
                    0o600,
                    dir_fd=parent_fd,
                )
                digest = hashlib.sha256()
                size = 0
                payload = bytearray()
                with os.fdopen(fd, "wb") as output:
                    while chunk := source.read(1024 * 1024):
                        digest.update(chunk)
                        size += len(chunk)
                        if name.startswith("Skills/"):
                            payload.extend(chunk)
                        output.write(chunk)
                    output.flush()
                    os.fsync(output.fileno())
                    final_mode = 0o555 if name == "cua-driver-local" else member.mode & 0o555
                    os.fchmod(output.fileno(), final_mode)
                if size != member.size:
                    raise SystemExit("truncated export payload")
                files[name] = (digest.hexdigest(), size)
                if name.startswith("Skills/"):
                    relative = name.removeprefix("Skills/").encode()
                    skill_entries.append((b"F", relative, final_mode.to_bytes(4, "big"), bytes(payload)))
            finally:
                for fd in reversed(opened):
                    os.close(fd)
    required = {"cua-driver-local", "scripts/_local-signing.sh", "Skills/cua-driver/SKILL.md"}
    if missing := required - files.keys():
        raise SystemExit(f"build export missing required entries: {sorted(missing)}")
    skill_digest = hashlib.sha256()
    for entry in sorted(skill_entries, key=lambda item: item[1]):
        for field in entry:
            skill_digest.update(len(field).to_bytes(8, "big"))
            skill_digest.update(field)
    print(files["cua-driver-local"][0], skill_digest.hexdigest())
finally:
    for fd, mode in reversed(skill_dirs):
        os.fchmod(fd, mode)
        os.close(fd)
    os.close(stage_fd)
