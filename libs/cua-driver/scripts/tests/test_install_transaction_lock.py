from __future__ import annotations

import os
import runpy
import subprocess
from pathlib import Path

import pytest


HELPER = Path(__file__).resolve().parents[1] / "_install-transaction-lock.py"


def lock_env(path: Path) -> dict[str, str]:
    env = os.environ.copy()
    env.update(
        {
            "CUA_DRIVER_INSTALL_TRANSACTION_LOCK_TESTING": "1",
            "CUA_DRIVER_INSTALL_TRANSACTION_LOCK_TEST_PATH": str(path),
        }
    )
    return env


def run_acquire(path: Path) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["python3", str(HELPER), "--acquire", "/bin/true"],
        env=lock_env(path),
        text=True,
        capture_output=True,
        timeout=2,
        check=False,
    )


def test_lock_open_refuses_symlink_without_touching_target(tmp_path: Path) -> None:
    victim = tmp_path / "victim"
    victim.write_text("unchanged", encoding="utf-8")
    lock = tmp_path / "lock"
    lock.symlink_to(victim)

    result = run_acquire(lock)

    assert result.returncode != 0
    assert victim.read_text(encoding="utf-8") == "unchanged"


@pytest.mark.parametrize("kind", ["fifo", "directory", "permissive-file"])
def test_lock_open_refuses_unsafe_type_or_mode(tmp_path: Path, kind: str) -> None:
    lock = tmp_path / "lock"
    if kind == "fifo":
        os.mkfifo(lock)
    elif kind == "directory":
        lock.mkdir()
    else:
        lock.touch(mode=0o644)
        lock.chmod(0o644)

    result = run_acquire(lock)

    assert result.returncode != 0
    assert "refusing CUA promotion transaction lock" in result.stderr


def test_lock_open_refuses_wrong_owner(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    lock = tmp_path / "lock"
    lock.touch(mode=0o600)
    namespace = runpy.run_path(str(HELPER))
    actual_uid = os.geteuid()
    monkeypatch.setattr(namespace["os"], "geteuid", lambda: actual_uid + 1)

    with pytest.raises(RuntimeError, match="not owned by the current user"):
        namespace["checked_open"](str(lock))


def test_locked_descriptor_is_safely_inherited_and_validated(tmp_path: Path) -> None:
    lock = tmp_path / "lock"
    command = (
        f'python3 "{HELPER}" --validate'
    )

    result = subprocess.run(
        ["python3", str(HELPER), "--acquire", "/bin/bash", "-c", command],
        env=lock_env(lock),
        text=True,
        capture_output=True,
        timeout=2,
        check=False,
    )

    assert result.returncode == 0, result.stderr
