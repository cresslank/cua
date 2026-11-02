#!/usr/bin/env python3
# SPDX-License-Identifier: FSL-1.1-MIT
# Copyright (c) 2026 Cua AI, Inc.
"""Opt-in real-Docker launcher boundary, status, timeout, leak and cancellation checks."""

from __future__ import annotations

import argparse
import json
import os
import signal
import subprocess
import tempfile
import time
from pathlib import Path


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--scratch", type=Path, required=True)
    args = parser.parse_args()
    args.scratch.mkdir(parents=True, exist_ok=True)
    root = Path(tempfile.mkdtemp(prefix="container-check-", dir=args.scratch))
    launcher = Path(__file__).resolve().parent / "run.sh"

    def start(name: str, command: list[str], **extra: str) -> tuple[Path, subprocess.Popen]:
        evidence = root / name
        env = {
            k: v
            for k, v in os.environ.items()
            if k not in ("CDB_CLAUDE_TOKEN_FILE", "CDB_ANTHROPIC_API_KEY_FILE", "CDB_CUA_ALLOWLIST")
        }
        env.update(extra)
        env.update(
            DISPLAY=":9876",
            WAYLAND_DISPLAY="host-do-not-use",
            HOST_ENV_CANARY="must-not-leak",
            DBUS_SESSION_BUS_ADDRESS="unix:path=/host-forbidden",
            SSH_AUTH_SOCK="/host-forbidden",
        )
        proc = subprocess.Popen(
            ["bash", str(launcher), str(evidence), "--", *command],
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
        )
        return evidence, proc

    def finish(evidence: Path, proc: subprocess.Popen, expected: int) -> str:
        try:
            out, _ = proc.communicate(timeout=80)
        except subprocess.TimeoutExpired:
            proc.send_signal(signal.SIGTERM)
            out, _ = proc.communicate(timeout=40)
            raise AssertionError(f"launcher exceeded timeout: {out}") from None
        assert proc.returncode == expected, (evidence, expected, proc.returncode, out)
        assert int((evidence / "docker-exit").read_text()) == expected
        cid = (evidence / "container.id").read_text().strip()
        assert subprocess.run(["docker", "inspect", cid], capture_output=True).returncode != 0
        return out

    ev, proc = start(
        "environment",
        [
            "python3",
            "-c",
            "import os,json,pathlib; assert 'HOST_ENV_CANARY' not in os.environ; assert 'WAYLAND_DISPLAY' not in os.environ; assert 'SSH_AUTH_SOCK' not in os.environ; assert os.environ['DISPLAY'] != ':9876'; assert not pathlib.Path('/run/user/1000/bus').exists(); pathlib.Path('/evidence/environment.json').write_text(json.dumps(dict(os.environ)))",
        ],
    )
    finish(ev, proc, 0)
    spec = json.loads((ev / "container-inspect.json").read_text())[0]
    hc = spec["HostConfig"]
    assert not hc["Privileged"] and hc["ReadonlyRootfs"]
    assert hc["CapDrop"] == ["ALL"]
    assert hc["NetworkMode"] == "none" and hc["IpcMode"] == "private"
    assert hc["PidMode"] == "" and not hc["Devices"] and not hc["PortBindings"]
    assert "no-new-privileges" in hc["SecurityOpt"]
    binds = [m for m in spec["Mounts"] if m["Type"] == "bind"]
    assert (
        len(binds) == 1 and binds[0]["Source"] == str(ev) and binds[0]["Destination"] == "/evidence"
    )
    assert set(hc["Tmpfs"]) == {"/tmp", "/run", "/home/bench"}
    assert "exec" in hc["Tmpfs"]["/home/bench"].split(",")
    assert (ev / "session/exit").read_text().strip() == "command=0 evidence=0"

    # Synthetic file canary only, never used by a model or credential client.
    token = root / "token-canary"
    token.write_text("launcher-permission-canary\n")
    token.chmod(0o644)
    ev, proc = start("token-mode-refusal", ["true"], CDB_CLAUDE_TOKEN_FILE=str(token))
    out, _ = proc.communicate(timeout=10)
    assert (
        proc.returncode == 2
        and out == "Token must be an owned regular 0600 file\n"
        and not ev.exists()
    ), out
    token.chmod(0o600)
    link = root / "token-link"
    link.symlink_to(token)
    ev, proc = start("token-link-refusal", ["true"], CDB_CLAUDE_TOKEN_FILE=str(link))
    out, _ = proc.communicate(timeout=10)
    assert proc.returncode == 2 and out == "Token symlink refused\n" and not ev.exists(), out
    ev, proc = start("token-mount", ["true"], CDB_CLAUDE_TOKEN_FILE=str(token))
    finish(ev, proc, 0)
    spec = json.loads((ev / "container-inspect.json").read_text())[0]
    mount = next(m for m in spec["Mounts"] if m["Destination"] == "/opt/cua-container/claude-token")
    assert mount["Source"] == str(token) and not mount["RW"]

    extra_cases = []
    api = root / "api-canary"
    api.write_text("sk-ant-launcher-canary\n")
    api.chmod(0o644)
    for name, path, error, token_value in (
        ("api-mode-refusal", api, "API key must be an owned regular 0600 file", ""),
        ("api-link-refusal", link, "API key symlink refused", ""),
        (
            "both-refusal",
            api,
            "Exactly one Claude credential mode is allowed; both credential files are set",
            str(token),
        ),
    ):
        ev, proc = start(
            name, ["true"], CDB_ANTHROPIC_API_KEY_FILE=str(path), CDB_CLAUDE_TOKEN_FILE=token_value
        )
        out, _ = proc.communicate(timeout=10)
        assert proc.returncode == 2 and out == error + "\n" and not ev.exists(), out
        extra_cases.append(name)
    api.chmod(0o600)
    # Real wrong-owner file, not a mocked stat result. Only this synthetic file
    # is chowned, in a network-none container; restore it even on failure.
    image = os.environ["CDB_CONTAINER_IMAGE"]

    def chown(uid: int) -> None:
        subprocess.run(
            [
                "docker",
                "run",
                "--rm",
                "--network",
                "none",
                "--user",
                "0",
                "--entrypoint",
                "/bin/chown",
                "--mount",
                f"type=bind,src={api},dst=/canary",
                image,
                str(uid),
                "/canary",
            ],
            check=True,
        )
        assert api.stat().st_uid == uid

    try:
        chown(12345)
        for variable, label in (
            ("CDB_ANTHROPIC_API_KEY_FILE", "API key"),
            ("CDB_CLAUDE_TOKEN_FILE", "Token"),
        ):
            name = label.lower().replace(" ", "-") + "-owner-refusal"
            ev, proc = start(name, ["true"], **{variable: str(api)})
            out, _ = proc.communicate(timeout=10)
            assert (
                proc.returncode == 2
                and out == f"{label} must be an owned regular 0600 file\n"
                and not ev.exists()
            ), out
            extra_cases.append(name)
    finally:
        chown(os.getuid())
    ev, proc = start(
        "api-mount",
        [
            "python3",
            "-c",
            "import os,pathlib; assert os.environ['CDB_ANTHROPIC_API_KEY_FILE']=='/opt/cua-container/anthropic-api-key'; assert 'CDB_CLAUDE_TOKEN_FILE' not in os.environ; assert pathlib.Path(os.environ['CDB_ANTHROPIC_API_KEY_FILE']).stat().st_mode & 0o777 == 0o600",
        ],
        CDB_ANTHROPIC_API_KEY_FILE=str(api),
    )
    output = finish(ev, proc, 0)
    spec = json.loads((ev / "container-inspect.json").read_text())[0]
    mount = next(
        m for m in spec["Mounts"] if m["Destination"] == "/opt/cua-container/anthropic-api-key"
    )
    assert mount["Source"] == str(api) and not mount["RW"]
    secret = api.read_bytes().strip()
    assert secret.decode() not in output
    assert all(secret not in p.read_bytes() for p in ev.rglob("*") if p.is_file())
    extra_cases.append("api-mount")

    ev, proc = start("status", ["bash", "-c", "exit 7"])
    finish(ev, proc, 7)
    assert "command=7 evidence=0" in (ev / "session/exit").read_text()

    ev, proc = start("timeout", ["sleep", "30"], CDB_SESSION_TIMEOUT="2", CDB_SESSION_GRACE="1")
    finish(ev, proc, 124)
    assert "command=124" in (ev / "session/exit").read_text()

    ev, proc = start("leak", ["bash", "-c", "sleep 30 &"])
    finish(ev, proc, 1)
    assert "command=0 evidence=1" in (ev / "session/exit").read_text()

    ev, proc = start("cancel", ["sleep", "30"])
    end = time.monotonic() + 20
    while not (ev / "session/command.pid").exists():
        assert proc.poll() is None, proc.communicate()[0]
        assert time.monotonic() < end, "command never started"
        time.sleep(0.1)
    proc.send_signal(signal.SIGTERM)
    finish(ev, proc, 143)
    assert "command=143" in (ev / "session/exit").read_text()
    print(
        json.dumps(
            {
                "ok": True,
                "cases": [
                    "environment",
                    "token-mode-refusal",
                    "token-link-refusal",
                    "token-mount",
                    "status",
                    "timeout",
                    "leak",
                    "cancel",
                    *extra_cases,
                ],
                "evidence": str(root),
            },
            indent=2,
        )
    )


if __name__ == "__main__":
    main()
