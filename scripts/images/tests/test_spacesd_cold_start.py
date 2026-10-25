#!/usr/bin/env python3
"""Exercise the guest launcher with a private real D-Bus, never a desktop."""

from pathlib import Path
import shutil
import socket
import subprocess
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[3]
LAUNCHER = ROOT / "libs/images/linux/files/start-spacesd.sh"


class SpacesdColdStartTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="spacesd-start-")
        self.addCleanup(self.tmp.cleanup)
        self.runtime = Path(self.tmp.name)
        self.address = f"unix:path={self.runtime}/bus"
        self.env = {
            "PATH": "/usr/bin:/bin",
            "HOME": self.tmp.name,
            "CUA_DESKTOP_RUNTIME_DIR": self.tmp.name,
            "CUA_ENV_TOKEN": "synthetic-startup-token",
            # The external executable edge records the environment at exec.
            "CUA_SPACESD_BIN": shutil.which("env"),
        }

    def launch(self):
        proc = subprocess.Popen(
            ["bash", str(LAUNCHER)],
            env=self.env,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
        )
        self.addCleanup(self.stop, proc)
        return proc

    @staticmethod
    def stop(proc):
        if proc.poll() is None:
            proc.terminate()
        proc.communicate(timeout=5)

    def write_env(self, display):
        (self.runtime / "desktop.env").write_text(
            f"DISPLAY={display}\nDBUS_SESSION_BUS_ADDRESS={self.address}\n"
        )

    def test_retained_env_and_socket_do_not_launch_until_bus_answers(self):
        self.write_env(":66")
        proc = self.launch()
        time.sleep(0.3)
        self.assertIsNone(proc.poll(), "retained desktop.env launched before the bus existed")

        # A socket path (even a listening one) is not a ready D-Bus daemon.
        with socket.socket(socket.AF_UNIX) as stalled:
            stalled.bind(str(self.runtime / "bus"))
            stalled.listen()
            time.sleep(1.5)
            self.assertIsNone(proc.poll(), "a nonresponsive bus socket was accepted")
        (self.runtime / "bus").unlink()
        self.write_env(":77")
        bus = subprocess.Popen(
            ["dbus-daemon", "--session", f"--address={self.address}", "--nofork", "--nopidfile"],
            env=self.env,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
        )
        self.addCleanup(self.stop, bus)
        output, _ = proc.communicate(timeout=10)
        self.assertEqual(proc.returncode, 0, output)
        self.assertIn("DISPLAY=:77\n", output)
        self.assertIn(f"DBUS_SESSION_BUS_ADDRESS={self.address}\n", output)

    def test_unavailable_desktop_refuses_with_a_bounded_wait(self):
        self.write_env(":66")
        started = time.monotonic()
        proc = self.launch()
        output, _ = proc.communicate(timeout=70)
        self.assertEqual(proc.returncode, 1, output)
        self.assertIn("desktop session not ready after 60s; refusing to start", output)
        self.assertNotIn("starting ", output)
        self.assertLess(time.monotonic() - started, 65)


if __name__ == "__main__":
    unittest.main()
