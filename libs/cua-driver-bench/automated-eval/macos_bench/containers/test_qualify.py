# SPDX-License-Identifier: FSL-1.1-MIT
# Copyright (c) 2026 Cua AI, Inc.
"""Pure cost-admission rules. Values below are test inputs, never trial results."""

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


if __name__ == "__main__":
    unittest.main()
