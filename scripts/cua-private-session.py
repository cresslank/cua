#!/usr/bin/env python3
"""Run one isolated, headless cua-compositor session.

The supervisor owns a private XDG runtime, D-Bus session, Wayland socket,
injection socket, config/data/cache roots, process group, pid lock, and log.
It never attaches to or changes the caller's desktop session.
"""

from __future__ import annotations

import argparse
import ast
import fcntl
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from typing import NoReturn


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--name", default="default", help="instance name")
    parser.add_argument(
        "--state-dir",
        type=Path,
        default=Path.home() / ".local" / "state" / "cua-private",
        help="durable agent-owned state root",
    )
    parser.add_argument("--width", type=int, default=1920)
    parser.add_argument("--height", type=int, default=1080)
    parser.add_argument("--startup-timeout", type=float, default=20.0)
    parser.add_argument("--keep-runtime", action="store_true")
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="print the isolated environment without starting D-Bus or a compositor",
    )
    parser.add_argument("command", nargs=argparse.REMAINDER)
    return parser


def _validate_name(name: str) -> str:
    if not name or any(not (ch.isalnum() or ch in "-_") for ch in name):
        raise SystemExit("--name must contain only letters, digits, '-' or '_'")
    return name


def _base_environment(args: argparse.Namespace, runtime: Path) -> dict[str, str]:
    state = args.state_dir.expanduser().resolve()
    instance = state / "instances" / args.name
    home = state / "home"
    env = os.environ.copy()
    env.update(
        {
            "XDG_RUNTIME_DIR": str(runtime),
            "XDG_SESSION_TYPE": "wayland",
            "XDG_CURRENT_DESKTOP": "cua-private",
            "XDG_SESSION_DESKTOP": "cua-private",
            "XDG_CONFIG_HOME": str(home / ".config"),
            "XDG_DATA_HOME": str(home / ".local" / "share"),
            "XDG_CACHE_HOME": str(instance / "cache"),
            "CUA_DRIVER_RS_ENABLE_WAYLAND": "1",
            "CUA_INJECT_SOCKET": str(runtime / "cua-inject-v2.sock"),
            "CUA_OUTW": str(args.width),
            "CUA_OUTH": str(args.height),
            "CUA_PRIVATE_INSTANCE": args.name,
            "CUA_PRIVATE_STATE_DIR": str(instance),
            "CUA_BROWSER_PROFILE_DIR": str(instance / "browser-profile"),
            "WLR_BACKENDS": "headless",
            "WLR_RENDERER": "pixman",
            "WLR_RENDERER_ALLOW_SOFTWARE": "1",
            "WLR_LIBINPUT_NO_DEVICES": "1",
            "WLR_HEADLESS_OUTPUTS": "1",
        }
    )
    # Never inherit display/session endpoints from the human desktop.
    for key in ("DISPLAY", "WAYLAND_DISPLAY", "DBUS_SESSION_BUS_ADDRESS", "AT_SPI_BUS_ADDRESS"):
        env.pop(key, None)
    return env


def _inside_dbus() -> bool:
    return os.environ.get("CUA_PRIVATE_INSIDE_DBUS") == "1"


def _reexec_inside_dbus(argv: list[str], env: dict[str, str]) -> NoReturn:
    env = env.copy()
    env["CUA_PRIVATE_INSIDE_DBUS"] = "1"
    os.execvpe(
        "dbus-run-session",
        ["dbus-run-session", "--", sys.executable, str(Path(__file__).resolve()), *argv],
        env,
    )


def _wait_for_socket(path: Path, process: subprocess.Popen[bytes], deadline: float) -> None:
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(
                f"cua-compositor exited during startup with status {process.returncode}"
            )
        if path.is_socket():
            return
        time.sleep(0.05)
    raise TimeoutError(f"timed out waiting for {path}")


def _configure_atspi(env: dict[str, str]) -> None:
    """Start the private AT-SPI bus through this instance's D-Bus session."""
    result = subprocess.run(
        [
            "gdbus",
            "call",
            "--session",
            "--dest",
            "org.a11y.Bus",
            "--object-path",
            "/org/a11y/bus",
            "--method",
            "org.a11y.Bus.GetAddress",
        ],
        env=env,
        check=True,
        capture_output=True,
        text=True,
        timeout=10,
    )
    parsed = ast.literal_eval(result.stdout.strip())
    address = str(parsed[0] if isinstance(parsed, tuple) else parsed)
    if not address.startswith("unix:"):
        raise RuntimeError("private AT-SPI bus returned an invalid address")
    env["AT_SPI_BUS_ADDRESS"] = address
    subprocess.run(
        [
            "gdbus",
            "call",
            "--address",
            address,
            "--dest",
            "org.a11y.Bus",
            "--object-path",
            "/org/a11y/bus",
            "--method",
            "org.freedesktop.DBus.Properties.Set",
            "org.a11y.Status",
            "IsEnabled",
            "<true>",
        ],
        env=env,
        check=True,
        capture_output=True,
        text=True,
        timeout=10,
    )


def _terminate(process: subprocess.Popen[bytes], timeout: float = 5.0) -> None:
    if process.poll() is not None:
        return
    try:
        os.killpg(process.pid, signal.SIGTERM)
        process.wait(timeout=timeout)
    except (ProcessLookupError, subprocess.TimeoutExpired):
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait(timeout=timeout)


def main(argv: list[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    args.name = _validate_name(args.name)
    if args.width < 320 or args.height < 240:
        raise SystemExit("private output must be at least 320x240")
    command = list(args.command)
    if command[:1] == ["--"]:
        command = command[1:]

    runtime_arg = os.environ.get("CUA_PRIVATE_RUNTIME_DIR")
    created_runtime = runtime_arg is None
    runtime = (
        Path(runtime_arg)
        if runtime_arg
        else Path(tempfile.mkdtemp(prefix=f"cua-private-{args.name}-"))
    )
    runtime.chmod(0o700)
    env = _base_environment(args, runtime)
    env["CUA_PRIVATE_RUNTIME_DIR"] = str(runtime)

    if args.dry_run:
        print(
            json.dumps(
                {key: env[key] for key in sorted(env) if key.startswith(("CUA_", "XDG_", "WLR_"))},
                indent=2,
            )
        )
        if created_runtime and not args.keep_runtime:
            shutil.rmtree(runtime)
        return 0

    if not command:
        raise SystemExit("a command is required after '--'")
    if not _inside_dbus():
        _reexec_inside_dbus(sys.argv[1:], env)

    state = args.state_dir.expanduser().resolve()
    instance = state / "instances" / args.name
    for directory in (
        instance,
        instance / "cache",
        instance / "browser-profile",
        state / "home" / ".config",
        state / "home" / ".local" / "share",
    ):
        directory.mkdir(parents=True, exist_ok=True, mode=0o700)
    lock_path = instance / "supervisor.lock"
    lock_file = lock_path.open("a+")
    try:
        fcntl.flock(lock_file, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError as exc:
        raise SystemExit(f"private instance {args.name!r} is already running") from exc

    _configure_atspi(env)

    log_path = instance / "compositor.log"
    with log_path.open("ab", buffering=0) as log:
        compositor = subprocess.Popen(
            ["cua-compositor"],
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=log,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
        try:
            deadline = time.monotonic() + args.startup_timeout
            inject_socket = Path(env["CUA_INJECT_SOCKET"])
            _wait_for_socket(inject_socket, compositor, deadline)
            wayland_sockets = sorted(path for path in runtime.glob("wayland-*") if path.is_socket())
            while not wayland_sockets and time.monotonic() < deadline:
                if compositor.poll() is not None:
                    raise RuntimeError(
                        f"cua-compositor exited during startup with status {compositor.returncode}"
                    )
                time.sleep(0.05)
                wayland_sockets = sorted(
                    path for path in runtime.glob("wayland-*") if path.is_socket()
                )
            if len(wayland_sockets) != 1:
                raise RuntimeError(
                    f"expected one private Wayland socket, found {[path.name for path in wayland_sockets]}"
                )
            child_env = env.copy()
            child_env["WAYLAND_DISPLAY"] = wayland_sockets[0].name
            child = subprocess.Popen(command, env=child_env, start_new_session=True)
            try:
                return child.wait()
            finally:
                _terminate(child)
        finally:
            _terminate(compositor)
            if created_runtime and not args.keep_runtime:
                shutil.rmtree(runtime, ignore_errors=True)


if __name__ == "__main__":
    raise SystemExit(main())
