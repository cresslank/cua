from __future__ import annotations

import importlib.util
import json
from pathlib import Path
from types import ModuleType, SimpleNamespace

import pytest


SCRIPT = Path(__file__).parents[1] / "cua-private-session.py"


def _load() -> ModuleType:
    spec = importlib.util.spec_from_file_location("cua_private_session_test", SCRIPT)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def test_private_environment_drops_human_desktop_endpoints(tmp_path, monkeypatch):
    module = _load()
    monkeypatch.setenv("HOME", "/home/human")
    monkeypatch.setenv("DISPLAY", ":99")
    monkeypatch.setenv("WAYLAND_DISPLAY", "wayland-human")
    monkeypatch.setenv("DBUS_SESSION_BUS_ADDRESS", "unix:path=/human")
    args = module._parser().parse_args(
        ["--name", "lane-a", "--state-dir", str(tmp_path / "state"), "--dry-run"]
    )
    env = module._base_environment(args, tmp_path / "runtime")

    assert "DISPLAY" not in env
    assert "WAYLAND_DISPLAY" not in env
    assert "DBUS_SESSION_BUS_ADDRESS" not in env
    assert env["HOME"] == str(tmp_path / "state/home")
    assert env["XDG_STATE_HOME"].endswith("instances/lane-a/state")
    assert env["CUA_INJECT_SOCKET"].endswith("/cua-inject-v2.sock")
    assert env["CUA_DRIVER_BROWSER_PROFILE_ROOT"].endswith(
        "instances/lane-a/browser-profile"
    )
    assert env["CUA_BROWSER_PROFILE_DIR"] == env["CUA_DRIVER_BROWSER_PROFILE_ROOT"]


def test_dry_run_has_no_compositor_side_effect(tmp_path, capsys):
    module = _load()
    state = tmp_path / "state"
    assert module.main(["--name", "lane-b", "--state-dir", str(state), "--dry-run"]) == 0
    payload = json.loads(capsys.readouterr().out)
    assert payload["CUA_PRIVATE_INSTANCE"] == "lane-b"
    assert not (state / "instances" / "lane-b" / "supervisor.lock").exists()


def test_private_atspi_address_is_propagated(monkeypatch):
    module = _load()
    calls = []

    def fake_run(command, **kwargs):
        calls.append((command, kwargs))
        return SimpleNamespace(stdout="('unix:path=/tmp/private-atspi',)\n")

    monkeypatch.setattr(module.subprocess, "run", fake_run)
    env = {"DBUS_SESSION_BUS_ADDRESS": "unix:path=/tmp/private-dbus"}

    module._configure_atspi(env)

    assert env["AT_SPI_BUS_ADDRESS"] == "unix:path=/tmp/private-atspi"
    assert (
        env["CUA_DRIVER_PRIVATE_AT_SPI_BUS_ADDRESS"]
        == "unix:path=/tmp/private-atspi"
    )
    assert calls[0][0][2] == "--session"
    assert calls[1][0][2:4] == ["--address", "unix:path=/tmp/private-atspi"]


@pytest.mark.parametrize("name", ["bad/name", "bad name", ""])
def test_instance_name_rejects_path_or_shell_characters(name):
    module = _load()
    with pytest.raises(SystemExit):
        module._validate_name(name)
