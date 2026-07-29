import ast
import ctypes
import os
from pathlib import Path
import shutil
import subprocess
import sys
from typing import Any

import pytest


SCRIPTS = Path(__file__).resolve().parents[1]
DRIVER_ROOT = SCRIPTS.parent
HELPER_INSTALLER = DRIVER_ROOT / "wayland-helper" / "install.sh"
HELPER_SOURCE = DRIVER_ROOT / "wayland-helper" / "winrects@cua"
RELEASE_INSTALLER = SCRIPTS / "_install-rust.sh"
UUID = "winrects@cua"


def _embedded_python() -> str:
    installer = HELPER_INSTALLER.read_text(encoding="utf-8")
    start = "python3 - \"$SRC\" \"$DEST\" <<'PY'\n"
    return installer.split(start, 1)[1].split("\nPY\n", 1)[0]


def _run_embedded(
    monkeypatch: pytest.MonkeyPatch,
    source: Path,
    destination: Path,
) -> None:
    monkeypatch.setattr(sys, "argv", ["helper-publisher", str(source), str(destination)])
    exec(compile(_embedded_python(), "helper-publisher", "exec"), {"__name__": "__main__"})


def _copy_helper_source(tmp_path: Path) -> Path:
    source = tmp_path / "source" / UUID
    shutil.copytree(HELPER_SOURCE, source)
    return source


def _write_pre_policy_destination(tmp_path: Path) -> Path:
    destination = tmp_path / "data/gnome-shell/extensions" / UUID
    destination.mkdir(parents=True)
    (destination / "metadata.json").write_text("old metadata\n", encoding="utf-8")
    (destination / "extension.js").write_text("old extension\n", encoding="utf-8")
    return destination


def _run_installer(tmp_path: Path) -> subprocess.CompletedProcess[str]:
    env = os.environ.copy()
    env.update(
        {
            "HOME": str(tmp_path / "home"),
            "PATH": "/usr/bin:/bin",
            "XDG_DATA_HOME": str(tmp_path / "data"),
        }
    )
    return subprocess.run(
        ["/bin/bash", str(HELPER_INSTALLER)],
        env=env,
        text=True,
        capture_output=True,
        check=False,
    )


def _source_files() -> dict[Path, bytes]:
    return {
        path.relative_to(HELPER_SOURCE): path.read_bytes()
        for path in HELPER_SOURCE.rglob("*")
        if path.is_file()
    }


def test_pre_policy_helper_is_atomically_replaced_with_complete_runtime(
    tmp_path: Path,
) -> None:
    destination = tmp_path / "data/gnome-shell/extensions" / UUID
    destination.mkdir(parents=True)
    (destination / "metadata.json").write_text("old metadata\n", encoding="utf-8")
    (destination / "extension.js").write_text("old extension\n", encoding="utf-8")
    (destination / "stale.js").write_text("stale\n", encoding="utf-8")
    old_inode = destination.stat().st_ino

    result = _run_installer(tmp_path)

    assert result.returncode == 0, result.stdout + result.stderr
    assert destination.stat().st_ino != old_inode
    installed = {
        path.relative_to(destination): path.read_bytes()
        for path in destination.rglob("*")
        if path.is_file()
    }
    assert installed == _source_files()
    assert (destination / "policy.js").is_file()
    assert not (destination / "stale.js").exists()
    assert not list(destination.parent.glob(f".{UUID}.new-*"))


def test_fresh_helper_install_publishes_complete_runtime(tmp_path: Path) -> None:
    destination = tmp_path / "data/gnome-shell/extensions" / UUID

    result = _run_installer(tmp_path)

    assert result.returncode == 0, result.stdout + result.stderr
    installed = {
        path.relative_to(destination): path.read_bytes()
        for path in destination.rglob("*")
        if path.is_file()
    }
    assert installed == _source_files()


def test_helper_install_refuses_symlink_destination(tmp_path: Path) -> None:
    parent = tmp_path / "data/gnome-shell/extensions"
    parent.mkdir(parents=True)
    victim = tmp_path / "victim"
    victim.mkdir()
    marker = victim / "marker"
    marker.write_text("preserve\n", encoding="utf-8")
    destination = parent / UUID
    destination.symlink_to(victim, target_is_directory=True)

    result = _run_installer(tmp_path)

    assert result.returncode != 0
    assert "not a real directory" in result.stdout + result.stderr
    assert destination.is_symlink()
    assert marker.read_text(encoding="utf-8") == "preserve\n"
    assert not (victim / "policy.js").exists()


def test_release_installer_delegates_complete_helper_publication() -> None:
    installer = RELEASE_INSTALLER.read_text(encoding="utf-8")

    assert '/bin/bash "$SRC_WAYLAND_HELPER/install.sh"' in installer
    assert 'cp "$SRC_WAYLAND_HELPER/winrects@cua/metadata.json"' not in installer
    assert '"$SRC_WAYLAND_HELPER/winrects@cua/extension.js"' not in installer


def test_embedded_helper_publisher_retains_python_36_grammar_and_apis() -> None:
    publisher = _embedded_python()

    ast.parse(publisher, feature_version=(3, 6))
    assert "set[Path]" not in publisher
    assert "dirs_exist_ok" not in publisher


def test_unavailable_renameat2_preserves_existing_helper(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    source = _copy_helper_source(tmp_path)
    destination = _write_pre_policy_destination(tmp_path)
    before = _source_files_from(destination)

    class LibcWithoutRenameat2:
        pass

    monkeypatch.setattr(ctypes, "CDLL", lambda *args, **kwargs: LibcWithoutRenameat2())
    with pytest.raises(SystemExit, match="atomic helper replacement"):
        _run_embedded(monkeypatch, source, destination)

    assert _source_files_from(destination) == before
    assert not list(destination.parent.glob(f".{UUID}.new-*"))


def test_post_exchange_cleanup_failure_keeps_new_helper_active(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    source = _copy_helper_source(tmp_path)
    destination = _write_pre_policy_destination(tmp_path)
    real_rmtree = shutil.rmtree

    def fail_old_tree_cleanup(
        path: str | os.PathLike[str], *args: Any, **kwargs: Any
    ) -> None:
        if Path(path).name.startswith(f".{UUID}.new-"):
            raise OSError("injected old-tree cleanup failure")
        real_rmtree(path, *args, **kwargs)

    monkeypatch.setattr(shutil, "rmtree", fail_old_tree_cleanup)
    _run_embedded(monkeypatch, source, destination)

    assert _source_files_from(destination) == _source_files_from(source)
    assert "could not remove previous helper tree" in capsys.readouterr().err
    assert list(destination.parent.glob(f".{UUID}.new-*"))


def test_post_publish_parent_fsync_failure_keeps_new_helper_active(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    source = _copy_helper_source(tmp_path)
    destination = tmp_path / "data/gnome-shell/extensions" / UUID
    real_fsync = os.fsync

    def fail_parent_fsync(descriptor: int) -> None:
        target = Path(os.readlink(f"/proc/self/fd/{descriptor}"))
        if target == destination.parent:
            raise OSError("injected parent fsync failure")
        real_fsync(descriptor)

    monkeypatch.setattr(os, "fsync", fail_parent_fsync)
    _run_embedded(monkeypatch, source, destination)

    assert _source_files_from(destination) == _source_files_from(source)
    assert "could not fsync helper parent" in capsys.readouterr().err


def _source_files_from(root: Path) -> dict[Path, bytes]:
    return {
        path.relative_to(root): path.read_bytes()
        for path in root.rglob("*")
        if path.is_file()
    }
