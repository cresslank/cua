#!/usr/bin/env python3
"""Hermetic CLI refusal checks: no bus, Shell, windows or settings are started."""
from pathlib import Path
import subprocess
import unittest

HERE = Path(__file__).resolve().parent
PYTHON = '/usr/bin/python3'


class OptInContract(unittest.TestCase):
    def invoke(self, *arguments):
        # Neither the help nor opt-in-refusal paths may inspect these nonexistent
        # paths or create a lock. They must refuse before touching native state.
        return subprocess.run(
            [PYTHON, '-I', '-B', str(HERE / 'hotreload_private_lab.py'), *arguments],
            env={'PATH': '/usr/bin:/bin', 'LANG': 'C.UTF-8'},
            stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=10)

    def test_help_is_non_native(self):
        result = self.invoke('--help')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('--allow-private-shell', result.stdout)
        self.assertIn('--qualification-lock', result.stdout)
        self.assertNotIn('run_started', result.stdout)

    def test_missing_opt_in_refuses_before_allocating_native_state(self):
        result = self.invoke('--scratch-root', '/nonexistent-cua-hotreload-scratch',
                             '--qualification-lock', '/nonexistent-cua-hotreload-lock')
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn('native lab is opt-in', result.stderr)
        self.assertNotIn('run_started', result.stdout)
        self.assertFalse(Path('/nonexistent-cua-hotreload-scratch').exists())
        self.assertFalse(Path('/nonexistent-cua-hotreload-lock').exists())

    def test_receiver_requires_owned_environment(self):
        result = subprocess.run([PYTHON, '-I', '-B', str(HERE / 'hotreload_receiver.py')],
                                env={'PATH': '/usr/bin:/bin', 'LANG': 'C.UTF-8'},
                                stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=10)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('CUA_HOTRELOAD_ROOT', result.stderr)
        self.assertNotIn('cannot open display', result.stderr.lower())


if __name__ == '__main__':
    unittest.main()
