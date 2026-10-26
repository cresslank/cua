"""Pure helper policy owner; native lifecycle belongs to the GNOME suites.

The former Node/GI mock asserted the retired API-10 monolith and rejected Shell
50, which is no longer the shipped contract. Exercise the canonical pure module
here; gnome_devkit.mjs and hotreload_private_lab.py own real native lifecycle,
resource cleanup, stale epochs, import races and failed-enable recovery.
"""
import shutil
import subprocess
from pathlib import Path

import pytest


def test_wayland_helper_policy():
    node = shutil.which("node")
    if node is None:
        pytest.fail("Node is required for the helper policy tests")
    policy = Path(__file__).resolve().parents[2] / "wayland-helper/tests/policy.test.mjs"
    subprocess.run([node, "--test", str(policy)], check=True)
