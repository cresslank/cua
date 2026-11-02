#!/usr/bin/env python3
# SPDX-License-Identifier: FSL-1.1-MIT
# Copyright (c) 2026 Cua AI, Inc.
"""Add explicit public software snapshots to an already-built disposable image.

No profile, credential, host socket or source .git directory enters this image.
The dependency interpreter is copied from a managed Hermes environment, not installed.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import tarfile
import tempfile
from pathlib import Path


def archive(repo: Path, dest: Path, paths: list[str] | None = None) -> str:
    sha = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip()
    dest.mkdir(parents=True)
    proc = subprocess.Popen(
        ["git", "archive", "HEAD", *(paths or [])], cwd=repo, stdout=subprocess.PIPE
    )
    assert proc.stdout is not None
    with tarfile.open(fileobj=proc.stdout, mode="r|") as tar:
        tar.extractall(dest, filter="data")
    if proc.wait() != 0:
        raise RuntimeError("git archive failed")
    return sha


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--base", default="cua-bench-linux:disposable")
    p.add_argument("--tag", default="cua-bench-linux:hermes")
    p.add_argument("--driver", type=Path, required=True)
    p.add_argument("--skills", type=Path, required=True)
    p.add_argument("--claude", type=Path, required=True)
    p.add_argument("--recorder", type=Path, help="optional independent release driver recorder")
    p.add_argument("--hermes-source", type=Path, required=True)
    p.add_argument("--hermes-python", type=Path, required=True)
    p.add_argument("--plugin-repo", type=Path, required=True)
    p.add_argument("--scratch", type=Path, required=True)
    a = p.parse_args()
    here = Path(__file__).resolve().parent
    repo = here.parents[4]
    if not (repo / "AGENTS.md").is_file():
        raise SystemExit("cannot locate repository root")
    context = Path(tempfile.mkdtemp(prefix="integration-build-", dir=a.scratch))
    base = subprocess.check_output(
        ["docker", "image", "inspect", a.base, "--format", "{{.Id}}"], text=True
    ).strip()
    paths = ["libs/cua-driver-bench/automated-eval/macos_bench", "libs/cua-driver-bench/tests"]
    names = (
        subprocess.check_output(["git", "ls-files", "-z", "--", *paths], cwd=repo)
        .decode()
        .split("\0")
    )
    for name in filter(None, names):
        src = repo / name
        if src.is_symlink():
            raise SystemExit(f"source symlink refused: {name}")
        dest = context / "bench" / src.relative_to(repo / "libs/cua-driver-bench")
        dest.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(src, dest)
        dest.chmod(0o755 if src.stat().st_mode & 0o111 else 0o644)
    # Public source fixture consumed by the full offline wiring suite.
    shutil.copyfile(
        repo / ".github/workflows/ci-cua-driver-bench.yml", context / "ci-cua-driver-bench.yml"
    )
    # Dependency-only environment. Drop editable source mapping to host paths.
    info = json.loads(
        subprocess.check_output(
            [
                str(a.hermes_python),
                "-c",
                "import sys,json; print(json.dumps([sys.prefix,sys.base_prefix,f'{sys.version_info.major}.{sys.version_info.minor}']))",
            ],
            env={"PATH": "/usr/bin:/bin"},
            text=True,
        )
    )
    shutil.copytree(info[1], context / "python", symlinks=True)
    shutil.copytree(Path(info[0]) / "lib", context / "venv/lib", symlinks=False)
    for path in (context / "venv/lib").rglob("__editable__*"):
        if path.is_file():
            path.unlink()
    (context / "venv/bin").mkdir()
    (context / "venv/bin/python").symlink_to(f"/opt/hermes-python/bin/python{info[2]}")
    (context / "venv/pyvenv.cfg").write_text(
        f"home = /opt/hermes-python/bin\ninclude-system-site-packages = false\nversion = {info[2]}\n"
    )
    source_sha = archive(a.hermes_source, context / "hermes")
    plugin_sha = archive(a.plugin_repo, context / "plugin", ["cua-jev"])
    for src, name in (
        (a.driver, "driver"),
        (a.claude, "claude"),
        (a.recorder or a.driver, "recorder"),
    ):
        shutil.copyfile(src.resolve(strict=True), context / name)
        (context / name).chmod(0o755)
    shutil.copytree(a.skills, context / "skills")
    receipt = {
        "base_image": base,
        "hermes_source": source_sha,
        "plugin_source": plugin_sha,
        "managed_interpreter": str(a.hermes_python),
        "driver_sha256": hashlib.sha256((context / "driver").read_bytes()).hexdigest(),
        "claude_sha256": hashlib.sha256((context / "claude").read_bytes()).hexdigest(),
    }
    base_tag = f"cua-bench-source:{base.removeprefix('sha256:')}"
    subprocess.run(["docker", "tag", base, base_tag], check=True)
    dockerfile = "\n".join(
        [
            f"FROM {base_tag}",
            "USER root",
            "COPY bench /opt/cua-bench",
            "COPY driver /usr/local/bin/cua-driver-local",
            "COPY recorder /usr/local/bin/cua-driver-recorder",
            "COPY skills /opt/cua-skills",
            "COPY claude /usr/local/bin/claude",
            "COPY python /opt/hermes-python",
            "COPY venv /opt/hermes-venv",
            "COPY hermes /opt/hermes-core",
            "COPY plugin/cua-jev /opt/cua-jev",
            "COPY bench/automated-eval/macos_bench/containers/entrypoint.sh /usr/local/bin/bench-container-entrypoint",
            "COPY integration.json /opt/cua-container/integration.json",
            "COPY ci-cua-driver-bench.yml /opt/cua-container/ci-cua-driver-bench.yml",
            "RUN chmod 755 /usr/local/bin/bench-container-entrypoint /usr/local/bin/claude /usr/local/bin/cua-driver-local",
            "USER 1000:1000",
            "",
        ]
    )
    (context / "Dockerfile").write_text(dockerfile)
    (context / "integration.json").write_text(json.dumps(receipt, indent=2) + "\n")
    subprocess.run(
        [
            "docker",
            "build",
            "--progress=plain",
            "--iidfile",
            str(context / "image.id"),
            "-t",
            a.tag,
            str(context),
        ],
        check=True,
    )
    receipt["image_id"] = subprocess.check_output(
        ["docker", "image", "inspect", a.tag, "--format", "{{.Id}}"], text=True
    ).strip()
    if receipt["image_id"] != (context / "image.id").read_text().strip():
        raise RuntimeError("image tag changed")
    (context / "receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
    print(json.dumps({"context": str(context), **receipt}, indent=2))


if __name__ == "__main__":
    main()
