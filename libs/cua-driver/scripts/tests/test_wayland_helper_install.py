import ast
import json
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
    shutil.copyfile(DRIVER_ROOT / 'wayland-helper/package.py', source.parent / 'package.py')
    return source


def _write_pre_policy_destination(tmp_path: Path) -> Path:
    destination = tmp_path / "data/gnome-shell/extensions" / UUID
    destination.mkdir(parents=True)
    (destination / "metadata.json").write_text('{"version": 8}\n', encoding="utf-8")
    (destination / "extension.js").write_text("old extension\n", encoding="utf-8")
    return destination


def _run_installer(tmp_path: Path, *, umask: int = 0o022) -> subprocess.CompletedProcess[str]:
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
        preexec_fn=lambda: os.umask(umask),
    )


def _source_files(tmp_path: Path, source: Path = HELPER_SOURCE) -> dict[Path, bytes]:
    package = tmp_path / 'expected-package'
    result = subprocess.run([sys.executable, str(source.parent / 'package.py'),
                             '--source', str(source), str(package)], capture_output=True, text=True)
    assert result.returncode == 0, result.stderr
    return _source_files_from(package)


def test_pre_policy_helper_is_atomically_replaced_with_complete_runtime(
    tmp_path: Path,
) -> None:
    destination = tmp_path / "data/gnome-shell/extensions" / UUID
    destination.mkdir(parents=True)
    (destination / "metadata.json").write_text('{"version": 8}\n', encoding="utf-8")
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
    assert installed == _source_files(tmp_path)
    selected = json.loads((destination / 'selected.json').read_text())['selected']
    assert (destination / 'impl' / selected / 'policy.js').is_file()
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
    assert installed == _source_files(tmp_path)


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


def test_release_installer_leaves_helper_publication_to_explicit_rollout() -> None:
    installer = RELEASE_INSTALLER.read_text(encoding="utf-8")

    assert '/bin/bash "$SRC_WAYLAND_HELPER/install.sh"' not in installer
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

    assert _source_files_from(destination) == _source_files(tmp_path, source)
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

    assert _source_files_from(destination) == _source_files(tmp_path, source)
    assert "could not fsync helper parent" in capsys.readouterr().err


def _source_files_from(root: Path) -> dict[Path, bytes]:
    return {
        path.relative_to(root): path.read_bytes()
        for path in root.rglob("*")
        if path.is_file()
    }


def test_explicit_rollout_never_downgrades_newer_helper(tmp_path: Path) -> None:
    destination = _write_pre_policy_destination(tmp_path)
    version = json.loads((HELPER_SOURCE / "metadata.json").read_text())["version"] + 1
    (destination / "metadata.json").write_text(json.dumps({"version": version}))
    before = _source_files_from(destination)
    result = _run_installer(tmp_path)
    assert result.returncode == 0, result.stderr
    assert _source_files_from(destination) == before
    assert f"Keeping the installed winrects@cua v{version}" in result.stdout
    assert "Not enabling it automatically" in result.stdout


def test_unknown_installed_version_refuses_replacement(tmp_path: Path) -> None:
    destination = _write_pre_policy_destination(tmp_path)
    (destination / "metadata.json").write_text("invalid")
    before = _source_files_from(destination)
    result = _run_installer(tmp_path)
    assert result.returncode != 0
    assert "cannot establish helper version before replacement" in result.stderr
    assert _source_files_from(destination) == before


def test_existing_bootstrap_and_retained_closures_are_never_replaced(tmp_path: Path) -> None:
    destination = tmp_path / 'data/gnome-shell/extensions' / UUID
    first = _run_installer(tmp_path)
    assert first.returncode == 0, first.stderr
    (destination / 'retained-rollback').write_text('retain across routine updates')
    before = _source_files_from(destination)
    inode = destination.stat().st_ino
    second = _run_installer(tmp_path)
    assert second.returncode == 0, second.stderr
    assert 'use the reviewed selector transaction' in second.stdout
    assert _source_files_from(destination) == before
    assert destination.stat().st_ino == inode


def test_packaged_manifest_binds_the_complete_implementation(tmp_path: Path) -> None:
    import hashlib
    files = _source_files(tmp_path)
    selector = json.loads(files[Path('selected.json')])
    digest = selector['selected']
    manifest_bytes = files[Path('impl') / digest / 'manifest.json']
    assert hashlib.sha256(manifest_bytes).hexdigest() == digest
    manifest = json.loads(manifest_bytes)
    prefix = Path('impl') / digest
    assert {prefix / entry['path'] for entry in manifest['files']} == {
        path for path in files if path.parts[:2] == prefix.parts and path.name != 'manifest.json'}
    for entry in manifest['files']:
        content = files[prefix / entry['path']]
        assert len(content) == entry['size']
        assert hashlib.sha256(content).hexdigest() == entry['sha256']


@pytest.mark.parametrize('umask', [0o000, 0o002, 0o022, 0o077])
def test_install_publishes_loader_admissible_modes_under_caller_umask(tmp_path: Path, umask: int) -> None:
    result = _run_installer(tmp_path, umask=umask)
    assert result.returncode == 0, result.stderr
    destination = tmp_path / 'data/gnome-shell/extensions' / UUID
    for path in [destination, *destination.rglob('*')]:
        assert not path.is_symlink()
        assert path.stat().st_uid == os.getuid()
        assert path.stat().st_mode & 0o777 == (0o700 if path.is_dir() else 0o600), path
