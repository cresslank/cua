from __future__ import annotations

import os
import runpy
import subprocess
from pathlib import Path

import pytest


HELPER = Path(__file__).resolve().parents[1] / "_install-transaction-lock.py"
DEFAULT_LITERAL = 'DEFAULT_LOCK = f"/tmp/cua-driver-local-install-transaction-{os.geteuid()}.lock"'


def fixture_helper(tmp_path: Path, lock: Path) -> Path:
    path = tmp_path / "install-transaction-lock.py"
    original = HELPER.read_text(encoding="utf-8")
    replacement = f"DEFAULT_LOCK = {str(lock)!r}"
    text = original.replace(DEFAULT_LITERAL, replacement)
    assert text != original
    path.write_text(text, encoding="utf-8")
    path.chmod(0o755)
    return path


def run_acquire(tmp_path: Path, lock: Path) -> subprocess.CompletedProcess[str]:
    helper = fixture_helper(tmp_path, lock)
    return subprocess.run(
        ["python3", str(helper), "--acquire", "/bin/true"],
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

    result = run_acquire(tmp_path, lock)

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

    result = run_acquire(tmp_path, lock)

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
    helper = fixture_helper(tmp_path, lock)
    command = f'python3 "{helper}" --validate'

    result = subprocess.run(
        ["python3", str(helper), "--acquire", "/bin/bash", "-c", command],
        text=True,
        capture_output=True,
        timeout=2,
        check=False,
    )

    assert result.returncode == 0, result.stderr


def test_production_helper_has_no_environment_selected_lock_domain() -> None:
    text = HELPER.read_text(encoding="utf-8")
    assert "CUA_DRIVER_INSTALL_TRANSACTION_LOCK_TESTING" not in text
    assert "CUA_DRIVER_INSTALL_TRANSACTION_LOCK_TEST_PATH" not in text
