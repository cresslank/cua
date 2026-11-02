#!/usr/bin/env python3
# SPDX-License-Identifier: FSL-1.1-MIT
# Copyright (c) 2026 Cua AI, Inc.
"""Headless regression in a writable synthetic checkout, never the host checkout."""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
from pathlib import Path


def main() -> None:
    if os.environ.get("CDB_BENCH_DISPOSABLE") != "1":
        raise SystemExit("requires disposable container")
    root = Path("/home/bench/regression")
    bench = root / "libs/cua-driver-bench"
    shutil.copytree(
        "/opt/cua-bench/automated-eval/macos_bench", bench / "automated-eval/macos_bench"
    )
    shutil.copytree("/opt/cua-bench/tests", bench / "tests")
    workflows = root / ".github/workflows"
    workflows.mkdir(parents=True)
    shutil.copyfile(
        "/opt/cua-container/ci-cua-driver-bench.yml", workflows / "ci-cua-driver-bench.yml"
    )
    env = {
        k: v
        for k, v in os.environ.items()
        if k
        not in (
            "DISPLAY",
            "WAYLAND_DISPLAY",
            "XDG_SESSION_TYPE",
            "DBUS_SESSION_BUS_ADDRESS",
            "XAUTHORITY",
            "AT_SPI_BUS_ADDRESS",
            "CDB_CLAUDE_TOKEN_FILE",
            "CDB_ANTHROPIC_API_KEY_FILE",
            "CDB_BENCH_DISPOSABLE",
        )
    }
    env.update(
        HERMES_BENCH_PYTHON="/opt/hermes-venv/bin/python",
        HERMES_BENCH_SOURCE="/opt/hermes-core",
        HERMES_BENCH_CUA_JEV="/opt/cua-jev",
    )
    subprocess.run(["git", "init", "-q", str(root)], env=env, check=True)
    harness = bench / "automated-eval/macos_bench"
    # Whole suite, not a hand-maintained file list: new credential contracts must
    # not silently disappear from the container's offline qualification.
    subprocess.run(
        [
            sys.executable,
            "-m",
            "pytest",
            "-q",
            "-p",
            "no:cacheprovider",
            "tests",
            str(bench / "tests/test_hermes_mcp.py"),
        ],
        cwd=harness,
        env=env,
        check=True,
    )
    # Probes have their own top-level `common` module. Collect independently.
    subprocess.run(
        [
            sys.executable,
            "-m",
            "pytest",
            "-q",
            "-p",
            "no:cacheprovider",
            "probes/tests",
            "swift/tests",
        ],
        cwd=harness,
        env=env,
        check=True,
    )


if __name__ == "__main__":
    main()
