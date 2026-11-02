#!/usr/bin/env python3
# SPDX-License-Identifier: FSL-1.1-MIT
# Copyright (c) 2026 Cua AI, Inc.
"""Bounded three-arm qualification inside the private desktop; never a full matrix."""

from __future__ import annotations

import argparse
import json
import math
import os
import subprocess
import sys
from collections.abc import Mapping, Sequence
from pathlib import Path

ARMS = ("cc-cua-driver-script", "cc-cua-driver-main", "cc-hermes-cua-jev")
TASKS = ("MB-10", "IR-01")


def accounted_trial_cost(rows: Sequence[Mapping[str, object]]) -> float:
    """Fail closed on missing/nonfinite usage or a trial+retry total reaching $6."""
    costs: list[float] = []
    for row in rows:
        value = row.get("cost_usd", row.get("total_cost_usd"))
        if (
            isinstance(value, bool)
            or not isinstance(value, (int, float))
            or not math.isfinite(value)
            or value < 0
        ):
            raise ValueError("unknown cost")
        costs.append(float(value))
    if not costs:
        raise ValueError("unknown cost")
    cost = sum(costs)
    if cost >= 6:
        raise ValueError("trial reached the $6 guard")
    return cost


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument(
        "--smoke",
        action="store_true",
        help="six paid trials, $6 per trial ($36 envelope); explicit credential mount required",
    )
    a = p.parse_args()
    if (
        os.environ.get("CDB_BENCH_DISPOSABLE") != "1"
        or not Path("/opt/cua-container/integration.json").is_file()
    ):
        raise SystemExit("requires integrated disposable container")
    # Import from the harness root, including when invoked as containers/qualify.py.
    sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
    import claude_credentials as credentials

    try:
        mode = credentials.billing_mode(required=a.smoke)
        if mode is not None:
            # Validate the file before doing any qualification or paid dispatch.
            with credentials.spawn([], {}):
                pass
    except ValueError as error:
        raise SystemExit(f"BLOCKED: {error}") from None
    ev = Path(os.environ["CDB_SESSION_DIR"])
    (ev / "credential-mode.json").write_text(json.dumps({"billing_mode": mode}) + "\n")
    binary = os.environ["CUA_DRIVER_BIN"]
    pins = ev / "pins-linux.json"
    subprocess.run(
        [
            sys.executable,
            "desktop_runtime.py",
            "doctor",
            "--cua-bin",
            binary,
            "--out",
            str(ev / "doctor"),
            "--json",
        ],
        check=True,
    )
    subprocess.run(
        [
            sys.executable,
            "tools/make_linux_pins.py",
            "--cua-bin",
            binary,
            "--cua-skills",
            "/opt/cua-skills",
            "--doctor-json",
            str(ev / "doctor/doctor.json"),
            "--out",
            str(pins),
        ],
        check=True,
    )
    common = [
        "--bench-v2",
        "--condition",
        "background",
        "--phase1-runs",
        "1",
        "--phase2-runs",
        "0",
        "--max-turns",
        "45",
        "--max-budget-usd",
        "6",
        "--model",
        "claude-sonnet-5-5",
        "--cua-bin",
        binary,
        "--recorder-bin",
        "/usr/local/bin/cua-driver-recorder",
        "--cua-skills",
        "/opt/cua-skills",
        "--pins",
        str(pins),
        "--claude-bin",
        "/usr/local/bin/claude",
        "--runs-dir",
        str(ev / "runs"),
        "--ledger",
        str(ev / "ledger.jsonl"),
        "--hermes-source",
        "/opt/hermes-core",
        "--hermes-python",
        "/opt/hermes-venv/bin/python",
        "--hermes-cua-jev",
        "/opt/cua-jev",
        "--hermes-allowlist",
        "/opt/cua-container/authorized-scopes.json",
    ]
    subprocess.run(
        [sys.executable, "run_bench.py", "dry-run", *common, "--arms", *ARMS, "--tasks", *TASKS],
        check=True,
    )
    subprocess.run(
        [
            sys.executable,
            "run_bench.py",
            "preflight",
            *common,
            "--run-id",
            "offline-preflight",
            "--offline",
            "--arms",
            *ARMS,
            "--tasks",
            *TASKS,
        ],
        check=True,
    )
    # Native GTK/AT-SPI parity contracts stay contained and credential-free.
    subprocess.run(
        [sys.executable, "linux/test_benchlab_atspi.py", "-v"],
        env={**os.environ, "CDB_BENCHLAB_CONTAINED": "1"},
        check=True,
        timeout=90,
    )
    # Same Hermes arm, no model call: prove real bounded input and exact policy
    # refusal through the adapter before any paid smoke can be dispatched.
    subprocess.run(
        [sys.executable, "containers/qualify_script.py"],
        check=True,
        timeout=180,
    )
    if not a.smoke:
        print(
            "PASS: offline three-arm preflight, six-row dry-run and native script policy; paid smoke not dispatched",
            flush=True,
        )
        return
    # Each task/arm gets a $6 trial+retry ledger, not a $6 series cap.
    # Serial dispatch stops the series BEFORE its next trial if any trial hits
    # that limit or lacks accounting. Six trials have a $36 planning envelope.
    for task in TASKS:
        for arm in ARMS:
            run_id = f"smoke-{task.lower()}-{arm}"
            subprocess.run(
                [
                    sys.executable,
                    "run_bench.py",
                    "run",
                    *common,
                    "--run-id",
                    run_id,
                    "--smoke",
                    "--no-model-preflight",
                    "--allow-unknown-quota",
                    "--ledger",
                    str(ev / f"{run_id}-ledger.jsonl"),
                    "--budget-cap-usd",
                    "6",
                    "--arms",
                    arm,
                    "--tasks",
                    task,
                ],
                check=True,
            )
            rows = list((ev / "runs" / run_id).rglob("*results*.jsonl"))
            # Do not continue when accounting cannot establish bounded spend.
            if not rows:
                raise SystemExit("STOP: no result accounting found; inspect run before resuming")
            parsed = [
                json.loads(line) for path in rows for line in path.read_text().splitlines() if line
            ]
            try:
                cost = accounted_trial_cost(parsed)
            except ValueError as error:
                raise SystemExit(f"STOP: {error}") from None
            print(json.dumps({"trial": run_id, "accounted_cost_usd": cost}), flush=True)


if __name__ == "__main__":
    main()
