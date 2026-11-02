#!/usr/bin/env python3
# SPDX-License-Identifier: FSL-1.1-MIT
# Copyright (c) 2026 Cua AI, Inc.
"""Real private-desktop/dependency smoke; no model calls or host desktop access."""

from __future__ import annotations

import hashlib
import json
import os
import subprocess
import sys
from pathlib import Path


def main() -> None:
    bench = Path.cwd()
    ev = Path(os.environ["CDB_SESSION_DIR"])
    sys.path.insert(0, str(bench))
    import gi

    gi.require_version("Atspi", "2.0")
    from gi.repository import Atspi

    assert os.environ["XDG_SESSION_TYPE"] == "x11"
    assert "WAYLAND_DISPLAY" not in os.environ
    assert os.environ["HOME"] == "/home/bench"
    assert os.environ["DBUS_SESSION_BUS_ADDRESS"].startswith("unix:")
    assert str(Path(os.environ["XAUTHORITY"]).resolve()).startswith("/home/bench/")
    assert not Path("/run/user/1000/bus").exists()
    assert not Path("/var/run/docker.sock").exists()
    for name in ("ANTHROPIC_API_KEY", "OPENAI_API_KEY", "CLAUDE_CODE_OAUTH_TOKEN", "SSH_AUTH_SOCK"):
        assert name not in os.environ, name
    binary = Path(os.environ["CUA_DRIVER_BIN"])
    digest = hashlib.sha256(binary.read_bytes()).hexdigest()
    assert digest == Path("/opt/cua-container/driver.sha256").read_text().split()[0]
    print("driver-copy-sha256:", digest, flush=True)
    subprocess.run([str(binary), "--version"], check=True)
    subprocess.run(
        [
            sys.executable,
            "desktop_runtime.py",
            "doctor",
            "--cua-bin",
            str(binary),
            "--out",
            str(ev),
            "--json",
        ],
        check=True,
    )
    report = json.loads((ev / "doctor.json").read_text())
    assert report["ok"] and report["daemon"] == "private"
    assert report["driver_sha256"] == digest
    assert report["build"]["exe_sha256"] == digest
    print("at-spi-desktop:", Atspi.get_desktop(0).get_child_count(), flush=True)
    # Upstream black-box interactions cover all seven Linux BenchLab modes,
    # sentinel witnessing, screenshot evidence and real driver recording.
    subprocess.run(
        [
            sys.executable,
            "-m",
            "pytest",
            "-q",
            "-p",
            "no:cacheprovider",
            "tests/test_pilot_runner.py",
        ],
        check=True,
    )
    subprocess.run([sys.executable, "/opt/cua-container/pack_smoke.py"], check=True)
    versions = {}
    for name, argv in {
        "python": [sys.executable, "--version"],
        "node": ["node", "--version"],
        "npm": ["npm", "--version"],
        "libreoffice": ["libreoffice", "--version"],
        "gnucash": ["gnucash", "--version"],
        "chrome": ["google-chrome", "--version"],
    }.items():
        done = subprocess.run(argv, capture_output=True, text=True, check=True, timeout=30)
        versions[name] = done.stdout.strip()
    (ev / "dependency-versions.json").write_text(json.dumps(versions, indent=2) + "\n")
    print(json.dumps(versions, indent=2), flush=True)


if __name__ == "__main__":
    main()
