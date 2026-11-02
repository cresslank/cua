#!/usr/bin/env python3
# SPDX-License-Identifier: FSL-1.1-MIT
# Copyright (c) 2026 Cua AI, Inc.
"""Launch each unmodified Linux pack descriptor and witness its GUI/AT-SPI windows."""

from __future__ import annotations

import json
import os
import subprocess
import sys
import time
from pathlib import Path


def windows() -> list[str]:
    done = subprocess.run(
        ["xprop", "-root", "_NET_CLIENT_LIST"], capture_output=True, text=True, check=True
    )
    import re

    names = []
    for xid in re.findall(r"0x[0-9a-fA-F]+", done.stdout):
        name = subprocess.run(
            ["xdotool", "getwindowname", xid], capture_output=True, text=True
        ).stdout.strip()
        names.append(name)
    return names


def ax_windows() -> list[str]:
    import gi

    gi.require_version("Atspi", "2.0")
    from gi.repository import Atspi

    desktop = Atspi.get_desktop(0)
    names = []
    for app in desktop:
        for window in app:
            names.append(window.get_name())
    return names


def main() -> None:
    sys.path.insert(0, str(Path.cwd()))
    import cdb_adapter
    import desktop_runtime

    root = Path(os.environ["CDB_SESSION_DIR"]) / "pack"
    reports = []
    for ident in ("cdb-s02", "cdb-s03", "cdb-s04"):
        artifacts = root / ident
        artifacts.mkdir(parents=True)
        task = cdb_adapter.CdbTask({"pack_task": f"shared/{ident}"}, artifacts)
        try:
            task.reset()
            task.start_apps(force_accessibility=True)
            wanted = [
                (
                    f"{app['window']['title_contains']}.gnucash"
                    if app["id"] == "gnucash"
                    else app["window"].get("title", app["window"].get("title_contains"))
                )
                for app in task.apps()
                if app.get("window")
            ]
            end = time.monotonic() + 45
            while True:
                titles = windows()
                accessible = ax_windows()
                missing_x = [name for name in wanted if not any(name in t for t in titles)]
                missing_ax = [name for name in wanted if not any(name in t for t in accessible)]
                if not missing_x and not missing_ax:
                    break
                if time.monotonic() > end:
                    raise AssertionError(
                        {
                            "task": ident,
                            "missing_x11": missing_x,
                            "missing_atspi": missing_ax,
                            "x11": titles,
                            "atspi": accessible,
                        }
                    )
                time.sleep(0.5)
            assert not any(
                "Welcome to LibreOffice" in t or "GnuCash Tip Of The Day" in t for t in titles
            ), titles
            row = {"task": ident, "wanted": wanted, "x11": titles, "atspi": accessible}
            (artifacts / "windows.json").write_text(json.dumps(row, indent=2) + "\n")
            desktop_runtime.capture_screenshot(os.environ["DISPLAY"], artifacts / "desktop.png")
            reports.append(row)
            print(json.dumps(row), flush=True)
        finally:
            task.stop_apps()
            task.clean_workspace()
    (root / "summary.json").write_text(json.dumps(reports, indent=2) + "\n")


if __name__ == "__main__":
    main()
