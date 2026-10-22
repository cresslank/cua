#!/usr/bin/env python3
"""Opt-in native menu regression. Only operates the receiver it builds/launches.

Requires an unlocked Aqua session and an already trusted driver on --socket.
Never launches/replaces a driver, requests permission, captures the desktop,
uses Apple Events, or operates an existing application. See macos-menu-receiver.md.
"""

import argparse
import json
import os
from pathlib import Path
import plistlib
import socket
import stat
import subprocess
import sys
import time
import uuid


def wait_for(read, predicate, timeout=4):
    deadline = time.monotonic() + timeout
    while True:
        value = read()
        if predicate(value):
            return value
        if time.monotonic() >= deadline:
            raise AssertionError(f"receiver did not settle: {value}")
        time.sleep(0.025)


class Driver:
    def __init__(self, path):
        info = path.lstat()
        assert stat.S_ISSOCK(info.st_mode) and info.st_uid == os.getuid()
        self.connection = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.connection.settimeout(25)
        self.connection.connect(str(path))
        self.stream = self.connection.makefile("rb")
        self.session = "menu-receiver-" + uuid.uuid4().hex
        assert self.send("session_begin")["ok"]

    def send(self, method, name=None, args=None):
        request = {"method": method, "session_id": self.session}
        if name is not None:
            request.update(name=name, args=args)
        self.connection.sendall(json.dumps(request).encode() + b"\n")
        line = self.stream.readline(4_000_001)
        assert line.endswith(b"\n") and len(line) <= 4_000_000
        return json.loads(line)

    def call(self, name, args):
        response = self.send("call", name, args)
        assert response.get("ok"), response
        return response["result"]

    def close(self):
        self.stream.close()
        self.connection.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--run-gui", action="store_true")
    parser.add_argument("--socket", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--repeats", type=int, default=3)
    args = parser.parse_args()
    if not args.run_gui:
        parser.error("explicit --run-gui is required; this test fronts only its owned receiver")
    if sys.platform != "darwin":
        parser.error("requires macOS")
    if not 1 <= args.repeats <= 20:
        parser.error("--repeats must be 1..20")
    root = args.output.resolve()
    root.mkdir(mode=0o700, parents=True, exist_ok=False)
    bundle = root / "MenuReceiver.app"
    executable = bundle / "Contents/MacOS/MenuReceiver"
    executable.parent.mkdir(parents=True)
    (bundle / "Contents/Info.plist").write_bytes(
        plistlib.dumps(
            {
                "CFBundleExecutable": "MenuReceiver",
                "CFBundleIdentifier": "local.cua.menu-receiver." + uuid.uuid4().hex,
                "CFBundleName": "Owned Menu Receiver",
                "CFBundlePackageType": "APPL",
                "NSPrincipalClass": "NSApplication",
            }
        )
    )
    source = Path(__file__).parent / "fixtures/apps/macos/menu-receiver/main.swift"
    subprocess.run(["xcrun", "swiftc", str(source), "-o", str(executable)], check=True)
    receiver_pid = None
    driver = None
    report = {"socket": str(args.socket), "rows": []}

    def state():
        try:
            return json.loads((root / "state.json").read_text())
        except FileNotFoundError:
            return {}

    def command(value):
        pending = root / "command.pending"
        pending.write_text(value)
        pending.replace(root / "command")

    def focus_own_window(index):
        assert driver is not None
        current = state()
        result = driver.call(
            "bring_to_front",
            {
                "pid": current["pid"],
                "window_id": current["windows"][index]["id"],
            },
        )
        assert not result.get("isError"), result
        return wait_for(state, lambda s: s.get("active") and s["windows"][index]["key"])

    try:
        driver = Driver(args.socket)
        permissions = driver.call("check_permissions", {"prompt": False})
        assert not permissions.get("isError"), permissions
        assert permissions["structuredContent"]["accessibility"], permissions
        report["config"] = driver.call("get_config", {})
        subprocess.run(
            ["open", "-n", "-g", "-a", str(bundle), "--env", f"CUA_MENU_RECEIVER_ROOT={root}"],
            check=True,
        )
        initial = wait_for(state, lambda s: len(s.get("windows", [])) == 2)
        receiver_pid = initial["pid"]
        report["initial"] = initial
        cases = [
            (["Probe", "Count"], "count"),
            (["Probe", "Nested", "Count Nested"], "nested_count"),
            (["Probe", "Nested", "Target Only"], "count"),
        ]
        for repetition in range(args.repeats):
            for path, counter in cases:
                # The app is already frontmost, but the requested sibling is
                # not key. AXPress must dispatch in window 1, never window 0.
                before = focus_own_window(0)
                assert before["pid"] == receiver_pid
                row = {"path": path, "repetition": repetition, "before": before}
                report["rows"].append(row)
                result = driver.call(
                    "invoke_menu",
                    {
                        "pid": receiver_pid,
                        "window_id": before["windows"][1]["id"],
                        "path": path,
                    },
                )
                row["result"] = result
                try:
                    assert not result.get("isError"), result
                    assert result["structuredContent"]["effect"] == "unverifiable", result
                    after = wait_for(
                        state,
                        lambda s: any(
                            s["windows"][i][counter] != before["windows"][i][counter]
                            for i in (0, 1)
                        ),
                    )
                    assert after["windows"][1][counter] == before["windows"][1][counter] + 1, after
                    assert after["windows"][0][counter] == before["windows"][0][counter], after
                    assert after["windows"][1]["key"], after
                    row["passed"] = True
                except AssertionError as error:
                    row.update(passed=False, error=str(error))
                row["after"] = state()
                (root / "report.json").write_text(json.dumps(report, indent=2) + "\n")
        # Missing paths must still refuse and must not increment either window.
        before = focus_own_window(1)
        refused = driver.call(
            "invoke_menu",
            {
                "pid": receiver_pid,
                "window_id": before["windows"][1]["id"],
                "path": ["Probe", "Nested", "Missing"],
            },
        )
        assert refused.get("isError"), refused
        assert refused["structuredContent"]["refusal"]["code"] == "menu_path_unavailable", refused
        report["missing_path_refusal"] = refused
        assert [[w["count"], w["nested_count"]] for w in state()["windows"]] == [
            [w["count"], w["nested_count"]] for w in before["windows"]
        ]
    finally:
        if receiver_pid is not None:
            command("quit")
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline:
                try:
                    os.kill(receiver_pid, 0)
                except ProcessLookupError:
                    report["receiver_exited"] = True
                    break
                time.sleep(0.05)
            # Never kill by name or close unrelated applications.
            assert report.get("receiver_exited"), "owned receiver did not exit"
        if driver is not None:
            driver.close()
        (root / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    passed = sum(row["passed"] for row in report["rows"])
    print(
        json.dumps(
            {"passed": passed, "total": len(report["rows"]), "report": str(root / "report.json")},
            indent=2,
        )
    )
    return 0 if passed == len(report["rows"]) else 1


if __name__ == "__main__":
    sys.exit(main())
