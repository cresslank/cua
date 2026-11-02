#!/usr/bin/env python3
# SPDX-License-Identifier: FSL-1.1-MIT
# Copyright (c) 2026 Cua AI, Inc.
"""Image-build only: suppress first-run/tip dialogs, never modify task content."""

from __future__ import annotations

import re
import subprocess
from pathlib import Path


def main() -> None:
    registry = Path("/usr/lib/libreoffice/share/registry/main.xcd")
    text = registry.read_text()
    for key in ("FirstRun", "ShowTipOfTheDay", "WhatsNew", "WhatsNewDialog"):
        pattern = rf'(<prop oor:name="{key}"[^>]*>.*?<value>)true(</value>)'
        text, count = re.subn(pattern, r"\g<1>false\2", text, flags=re.S)
        # Common/Misc and Setup/Product both carry a WhatsNew default.
        assert count == (2 if key == "WhatsNew" else 1), (key, count)
    registry.write_text(text)
    schemas = Path("/opt/gnucash/share/glib-2.0/schemas")
    assert (schemas / "org.gnucash.GnuCash.dialogs.totd.gschema.xml").is_file()
    (schemas / "99-bench.gschema.override").write_text(
        "[org.gnucash.GnuCash.dialogs.totd]\nshow-at-startup=false\n"
    )
    subprocess.run(["glib-compile-schemas", "--strict", str(schemas)], check=True)


if __name__ == "__main__":
    main()
