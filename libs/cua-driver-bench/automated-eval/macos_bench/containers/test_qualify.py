# SPDX-License-Identifier: FSL-1.1-MIT
# Copyright (c) 2026 Cua AI, Inc.
"""Cost/CLI contracts and opt-in private-desktop qualification; never paid calls."""

import os
import subprocess
import sys
import unittest
from pathlib import Path

from qualify import accounted_trial_cost


class CostTests(unittest.TestCase):
    def test_known_usage_including_retry_is_summed(self):
        self.assertEqual(accounted_trial_cost([{"cost_usd": 1.25}, {"cost_usd": 0.5}]), 1.75)

    def test_unknown_or_capped_trial_stops_series(self):
        for rows in (
            [],
            [{}],
            [{"cost_usd": None}],
            [{"cost_usd": True}],
            [{"cost_usd": float("nan")}],
            [{"cost_usd": float("inf")}],
            [{"cost_usd": -1}],
            [{"cost_usd": 6}],
            [{"cost_usd": 4}, {"cost_usd": 2}],
        ):
            with self.subTest(rows=rows), self.assertRaises(ValueError):
                accounted_trial_cost(rows)

    def test_builder_root_uses_containing_directory(self):
        here = Path(__file__).resolve().parent
        repo = here.parents[4]
        self.assertTrue((repo / "AGENTS.md").is_file())
        self.assertEqual(here, repo / "libs/cua-driver-bench/automated-eval/macos_bench/containers")

    def test_script_qualification_refuses_before_importing_native_tools_on_host(self):
        # Execute the actual CLI guard, not an imitation of its policy.
        env = {"PATH": "/usr/bin:/bin", "CDB_BENCH_DISPOSABLE": "0"}
        result = subprocess.run(
            [sys.executable, "-I", "-B", str(Path(__file__).with_name("qualify_script.py"))],
            env=env,
            capture_output=True,
            text=True,
            timeout=10,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")
        self.assertEqual(
            result.stderr.strip(),
            "requires integrated disposable container; never run against a host desktop",
        )

    @unittest.skipUnless(
        os.environ.get("CDB_TEST_SCRIPT_QUALIFICATION") == "1"
        and os.environ.get("CDB_BENCH_DISPOSABLE") == "1"
        and Path("/opt/cua-container/integration.json").is_file(),
        "requires explicit integrated disposable desktop; no host fallback",
    )
    def test_real_script_policy_receiver_and_exact_refusals(self):
        subprocess.run(
            [sys.executable, "-B", str(Path(__file__).with_name("qualify_script.py"))],
            check=True,
            timeout=180,
        )


if __name__ == "__main__":
    unittest.main()
