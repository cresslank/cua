#!/usr/bin/env python3
# SPDX-License-Identifier: FSL-1.1-MIT
# Copyright (c) 2026 Cua AI, Inc.
"""Stage public tracked sources plus one copied driver; never send HOME to Docker."""

from __future__ import annotations

import argparse
import hashlib
import json
import shutil
import subprocess
import tempfile
from pathlib import Path


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--driver", type=Path, required=True)
    parser.add_argument("--tag", default="cua-bench-linux:disposable")
    parser.add_argument("--scratch", type=Path, required=True)
    args = parser.parse_args()
    here = Path(__file__).resolve().parent
    repo = here.parents[4]
    bench = repo / "libs/cua-driver-bench"
    assert (repo / "AGENTS.md").is_file(), repo
    args.scratch.mkdir(parents=True, exist_ok=True)
    context = Path(tempfile.mkdtemp(prefix="container-build-", dir=args.scratch))
    source = args.driver.resolve(strict=True)
    if not source.is_file():
        raise SystemExit("--driver must resolve to one regular binary")
    shutil.copyfile(source, context / "cua-driver-local")
    digest = hashlib.sha256((context / "cua-driver-local").read_bytes()).hexdigest()
    # Copy current tracked bytes (including deliberate worktree edits), not ignored
    # node_modules, local-work, .git metadata, credentials or other untracked files.
    selected = [
        "libs/cua-driver-bench/automated-eval/macos_bench",
        "libs/cua-driver-bench/taskpack",
        "libs/cua-driver/rust/Skills/cua-driver",
    ]
    names = (
        subprocess.check_output(["git", "ls-files", "-z", "--", *selected], cwd=repo)
        .decode()
        .split("\0")
    )
    for name in filter(None, names):
        src = repo / name
        if src.is_symlink():
            raise SystemExit(f"refusing source symlink: {name}")
        if name.startswith("libs/cua-driver-bench/"):
            dest = context / "bench" / src.relative_to(bench)
        else:
            dest = context / "skills" / src.relative_to(repo / selected[2])
        dest.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(src, dest)
        dest.chmod(0o755 if src.stat().st_mode & 0o111 else 0o644)
    # This directory is this worker's owned new code; enumerate public files only.
    for name in (
        "Dockerfile",
        "entrypoint.sh",
        "npx.sh",
        "google-chrome.sh",
        "gnucash.sh",
        "smoke.py",
        "pack_smoke.py",
        "app_defaults.py",
    ):
        dest = context / "containers" / name
        dest.parent.mkdir(exist_ok=True)
        shutil.copyfile(here / name, dest)
        dest.chmod(0o644)
    revision = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip()
    receipt = {
        "source_revision": revision,
        "driver_source": str(source),
        "driver_sha256": digest,
        "context": str(context),
        "tag": args.tag,
    }
    snapshot = {}
    for file in sorted(p for p in context.rglob("*") if p.is_file()):
        snapshot[str(file.relative_to(context))] = hashlib.sha256(file.read_bytes()).hexdigest()
    fingerprint = hashlib.sha256(json.dumps(snapshot, sort_keys=True).encode()).hexdigest()
    receipt["source_snapshot_sha256"] = fingerprint
    (context / "snapshot.json").write_text(
        json.dumps(
            {"source_revision": revision, "source_snapshot_sha256": fingerprint, "files": snapshot},
            indent=2,
        )
        + "\n"
    )
    (context / "receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
    print(json.dumps(receipt, indent=2), flush=True)
    subprocess.run(
        [
            "docker",
            "build",
            "--progress=plain",
            "--iidfile",
            str(context / "image.id"),
            "--build-arg",
            f"DRIVER_SHA256={digest}",
            "--build-arg",
            f"BENCH_SOURCE_REVISION={revision}",
            "-t",
            args.tag,
            "-f",
            str(context / "containers/Dockerfile"),
            str(context),
        ],
        check=True,
    )
    receipt["image_id"] = (context / "image.id").read_text().strip()
    # Read back daemon-owned state rather than trusting only build's exit status.
    actual = subprocess.check_output(
        ["docker", "image", "inspect", args.tag, "--format", "{{.Id}}"], text=True
    ).strip()
    if actual != receipt["image_id"]:
        raise SystemExit("image tag changed during build")
    (context / "receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
    print(json.dumps(receipt, indent=2))


if __name__ == "__main__":
    main()
