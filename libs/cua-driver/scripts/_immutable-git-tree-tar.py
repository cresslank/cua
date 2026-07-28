#!/usr/bin/env python3
"""Emit a canonical tar stream directly from exact Git tree objects."""
from __future__ import annotations

import io
import os
import subprocess
import sys
import tarfile

repo, commit = sys.argv[1:]
rows = subprocess.check_output(
    ["git", "-C", repo, "ls-tree", "-rz", "--full-tree", "-r", commit]
).split(b"\0")
with tarfile.open(fileobj=sys.stdout.buffer, mode="w|") as archive:
    for row in rows:
        if not row:
            continue
        meta, raw_name = row.split(b"\t", 1)
        mode, kind, oid = meta.decode("ascii").split()
        if kind != "blob" or mode not in {"100644", "100755", "120000"}:
            raise SystemExit(f"unsupported Git tree entry: {mode} {kind}")
        name = os.fsdecode(raw_name)
        if name.startswith("/") or ".." in name.split("/"):
            raise SystemExit("unsafe Git path")
        payload = subprocess.check_output(["git", "-C", repo, "cat-file", "blob", oid])
        info = tarfile.TarInfo(name)
        info.uid = info.gid = info.mtime = 0
        info.uname = info.gname = ""
        if mode == "120000":
            info.type = tarfile.SYMTYPE
            info.mode = 0o777
            info.linkname = os.fsdecode(payload)
            info.size = 0
            archive.addfile(info)
        else:
            info.mode = 0o755 if mode == "100755" else 0o644
            info.size = len(payload)
            archive.addfile(info, io.BytesIO(payload))
