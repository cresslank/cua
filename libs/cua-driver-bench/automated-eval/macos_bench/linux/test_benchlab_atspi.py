# SPDX-License-Identifier: FSL-1.1-MIT
# Copyright (c) 2026 Cua AI, Inc.

"""Opt-in black-box BenchLab parity tests; never connect to a host desktop.

Run in the disposable bench image's private Xvfb/Openbox/AT-SPI session:
  env CDB_BENCHLAB_CONTAINED=1 nice -n19 /usr/bin/python3 -B linux/test_benchlab_atspi.py -v
Only this file and benchlab.py need read-only mounts over an existing bench image.
No driver, network, model, or credential is used. The container must not share host
PID/network namespaces or mount host display/bus sockets. Session logs and each
app's public state/events are retained beneath CDB_SESSION_DIR/benchlab-parity-*.
XTest moves affect ONLY the disposable Xvfb pointer, never the host pointer.
"""

from __future__ import annotations

import json
import os
import re
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
ENABLED = os.environ.get("CDB_BENCHLAB_CONTAINED") == "1"
if ENABLED:
    if not Path("/.dockerenv").is_file() or os.environ.get("CDB_BENCH_DISPOSABLE") != "1":
        raise RuntimeError("requires the disposable bench container, not a host session")
    import gi

    gi.require_version("Atspi", "2.0")
    from gi.repository import Atspi


def command(*args):
    return subprocess.run(args, check=True, capture_output=True, text=True, timeout=5).stdout


def wait_for(predicate, description, timeout=5):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        value = predicate()
        if value:
            return value
        time.sleep(0.03)
    raise AssertionError(f"timed out: {description}")


def tree(node):
    node.clear_cache()
    yield node
    for index in range(node.get_child_count()):
        child = node.get_child_at_index(index)
        if child is not None:
            yield from tree(child)


def showing(node):
    node.clear_cache()
    return node.get_state_set().contains(Atspi.StateType.SHOWING)


def action(node):
    interface = node.get_action_iface()
    if interface is None or interface.get_n_actions() == 0:
        raise AssertionError(f"no AT-SPI action on {node.get_name()}")
    return interface.do_action(0)


def text(node):
    return Atspi.Text.get_text(node.get_text_iface(), 0, -1)


def write(node, value):
    return node.get_editable_text_iface().set_text_contents(value)


@unittest.skipUnless(ENABLED, "opt in inside the disposable bench container only")
class BenchLabParityTest(unittest.TestCase):
    def setUp(self):
        Atspi.init()
        self.root = Path(
            tempfile.mkdtemp(prefix="benchlab-parity-", dir=os.environ["CDB_SESSION_DIR"])
        )
        self.children = []
        self.addCleanup(self.stop_children)
        command("xdotool", "mousemove", "1900", "1000")

    def stop_children(self):
        for proc, log in reversed(self.children):
            if proc.poll() is None:
                proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=5)
            log.close()

    def start(self, script, *args):
        log = (self.root / f"{script.stem}.log").open("w")
        proc = subprocess.Popen(
            [sys.executable, "-B", str(script), *map(str, args)],
            stdout=log,
            stderr=subprocess.STDOUT,
        )
        self.children.append((proc, log))
        return proc

    def lab(self, mode):
        self.state_path, self.events_path = self.root / "state.json", self.root / "events.jsonl"
        self.proc = self.start(
            HERE / "benchlab.py",
            "--mode",
            mode,
            "--seed",
            227159,
            "--state",
            self.state_path,
            "--events",
            self.events_path,
        )
        wait_for(lambda: self.state_path.exists(), "BenchLab state")
        self.app = wait_for(
            lambda: next(
                (
                    n
                    for n in tree(Atspi.get_desktop(0))
                    if n.get_role() == Atspi.Role.APPLICATION
                    and n.get_process_id() == self.proc.pid
                ),
                None,
            ),
            "BenchLab AT-SPI app",
        )
        self.xid = wait_for(lambda: self.windows(self.proc.pid), "BenchLab X window")[0]

    def state(self):
        return json.loads(self.state_path.read_text())

    def events(self, kind=None):
        rows = [json.loads(line) for line in self.events_path.read_text().splitlines()]
        return rows if kind is None else [row for row in rows if row["type"] == kind]

    def find(self, name, *, visible=True):
        return next(
            (
                n
                for n in tree(self.app)
                if n.get_name() == name
                and n.get_role() != Atspi.Role.LABEL
                and (not visible or showing(n))
            ),
            None,
        )

    def get(self, name):
        return wait_for(lambda: self.find(name), f"AT-SPI {name}")

    def windows(self, pid):
        ids = re.findall(r"0x[0-9a-fA-F]+", command("xprop", "-root", "_NET_CLIENT_LIST"))
        return [
            int(w, 16)
            for w in ids
            if re.search(rf"= {pid}\s*$", command("xprop", "-id", w, "_NET_WM_PID"))
        ]

    def zone(self):
        node = self.find("Actions")
        # Old-image negative control has no Actions accessible name.
        if node is None:
            label = next(
                n for n in tree(self.app) if n.get_name() == "Hover here to reveal actions"
            )
            node = label.get_parent().get_parent()
        return node.get_component_iface().get_extents(Atspi.CoordType.SCREEN)

    def move_inside(self):
        r = self.zone()
        command("xdotool", "mousemove", str(r.x + 20), str(r.y + 20))

    def test_trial_launch_clears_inherited_hover_before_agent(self):
        # Establish the target geometry, then reproduce a previous trial leaving
        # its real pointer over the next window's hot zone.
        self.lab("hover")
        r = self.zone()
        self.proc.terminate()
        self.proc.wait(timeout=5)
        command("xdotool", "mousemove", str(r.x + 20), str(r.y + 20))
        sys.path.insert(0, str(HERE.parent))
        import desktop_runtime as desktop
        import run_pilot as pilot

        artifact = self.root / "next-trial"
        artifact.mkdir()
        paths = {"state": artifact / "state.json", "events": artifact / "events.jsonl"}
        proc = pilot.launch_lab(
            pilot.load_tasks()["MB-10"],
            227159,
            paths,
            HERE.parent / "build/BenchLab.app",
            desktop.desktop_env(artifact / "home"),
        )
        self.addCleanup(lambda: (desktop.stop_group(proc.pid), proc.wait(timeout=5)))
        state = json.loads(paths["state"].read_text())
        self.assertEqual(state["hover_enter_count"], 0, "mapping inherited a prior trial's hover")
        self.assertEqual(state["clicks"], [])
        self.assertNotIn("overlay_shown", paths["events"].read_text())
        self.assertIn("X=0", command("xdotool", "getmouselocation", "--shell"))

    def test_hidden_actions_cannot_be_activated(self):
        self.lab("hover")
        hidden = self.find("Export", visible=False)
        if hidden is not None:
            self.assertFalse(showing(hidden), "Export showing before hover")
            action(hidden)
            time.sleep(0.15)
        self.assertEqual(self.state()["clicks"], [], "hidden AT-SPI action emitted a click")
        self.move_inside()
        export = self.get("Export")
        command("xdotool", "mousemove", "1900", "1000")
        wait_for(lambda: self.state()["hover_exit_count"] > 0, "leave")
        self.assertFalse(showing(export))
        action(export)  # retained handle after hiding, not just a fresh enumeration
        time.sleep(0.15)
        self.assertEqual(
            self.state()["clicks"], [], "retained hidden AT-SPI action emitted a click"
        )
        self.assertEqual(self.events("overlay_click"), [])

    def test_motion_is_recorded_and_passes_unchanged_evaluator(self):
        self.lab("hover")
        time.sleep(0.15)
        self.assertEqual(self.events("overlay_shown"), [], "actions revealed without hover")
        self.move_inside()
        wait_for(lambda: self.events("overlay_shown"), "hover reveals overlay")
        r = self.zone()
        command("xdotool", "mousemove", str(r.x + 30), str(r.y + 30))
        wait_for(
            lambda: any(e["details"]["phase"] == "mouseMoved" for e in self.events("mouse")),
            "motion-notify must emit mouseMoved, not just enter",
        )
        self.assertTrue(action(self.get("Export")))
        wait_for(lambda: len(self.events("overlay_click")) == 1, "visible action")
        self.assertTrue(self.events("overlay_click")[0]["details"]["overlay_visible"])
        self.evaluate_hover()

    def evaluate_hover(self):
        result = self.root / "evaluation.json"
        command(
            sys.executable,
            "-B",
            str(HERE.parent / "probes/MB-10/evaluate.py"),
            "--seed",
            "227159",
            "--state",
            str(self.state_path),
            "--events",
            str(self.events_path),
            "--result",
            str(result),
        )
        self.assertTrue(json.loads(result.read_text())["passed"], result.read_text())

    def page_roundtrip(self, answer):
        self.lab("irmodal")
        name, email = self.get("Name"), self.get("Email")
        self.assertTrue(write(name, "Jonas Lindqvist"))
        self.assertTrue(write(email, "jonas.lindqvist@example.com"))
        self.assertTrue(name.get_component_iface().grab_focus())
        wait_for(lambda: name.get_state_set().contains(Atspi.StateType.FOCUSED), "Name focused")
        action(self.get("Next"))
        action(self.get("Don’t Allow" if answer == "deny" else "Allow"))
        wait_for(lambda: self.state()["page"] == 2, "page 2")
        for state in (Atspi.StateType.FOCUSED, Atspi.StateType.FOCUSABLE, Atspi.StateType.EDITABLE):
            name.clear_cache()
            self.assertFalse(name.get_state_set().contains(state), f"hidden Name retains {state}")
        write(name, "CORRUPTED")
        time.sleep(0.1)
        self.assertEqual(text(name), "Jonas Lindqvist", "hidden Name accepted retained text write")
        self.assertEqual(self.state()["fields"]["name"], "Jonas Lindqvist")
        self.assertEqual(self.state()["fields"]["team"], "")
        combo = self.get("Team")
        self.assertEqual(combo.get_role(), Atspi.Role.COMBO_BOX)
        self.assertIn("Choose a team", [n.get_name() for n in tree(combo)])
        seats = self.get("Seats")
        self.assertTrue(write(seats, "16"))
        self.assertTrue(seats.get_component_iface().grab_focus())
        action(self.get("Back"))
        wait_for(lambda: self.state()["page"] == 1, "Back to page 1")
        self.assertEqual(text(self.get("Name")), "Jonas Lindqvist")
        self.assertEqual(text(self.get("Email")), "jonas.lindqvist@example.com")
        write(seats, "CORRUPTED")
        time.sleep(0.1)
        self.assertEqual(text(seats), "16", "hidden Seats accepted retained text write")
        self.assertTrue(write(self.get("Name"), "Repaired Name"))
        action(self.get("Next"))
        wait_for(lambda: self.state()["page"] == 2, "Next after Back")
        self.assertEqual(text(self.get("Seats")), "16")
        self.assertEqual(len(self.events("permission_prompt_shown")), 1)
        self.assertEqual(self.state()["prompt_answers"], [answer])
        self.assertEqual(
            [e["details"]["answer"] for e in self.events("permission_answer")], [answer]
        )
        action(self.get("Submit"))
        wait_for(lambda: self.state()["submit_count"] == 1, "submit exactly once")
        self.assertEqual(
            self.state()["submitted"],
            {
                "name": "Repaired Name",
                "email": "jonas.lindqvist@example.com",
                "team": "",
                "seats": "16",
            },
        )

    def test_deny_page_focus_back_and_placeholder(self):
        self.page_roundtrip("deny")

    def test_allow_page_focus_back_and_placeholder(self):
        self.page_roundtrip("allow")

    def test_permission_keeps_foreground_window_above_benchlab(self):
        self.lab("irmodal")
        sentinel = self.start(HERE / "sentinel.py", "--log", self.root / "sentinel.jsonl")
        other = wait_for(lambda: self.windows(sentinel.pid), "second ordinary window")[0]
        command("xdotool", "windowactivate", "--sync", str(other))
        time.sleep(0.2)
        before = command("xprop", "-root", "_NET_CLIENT_LIST")
        pointer = command("xdotool", "getmouselocation", "--shell")
        samples = []

        def sample():
            active = command("xprop", "-root", "_NET_ACTIVE_WINDOW")
            stack = re.findall(
                r"0x[0-9a-fA-F]+", command("xprop", "-root", "_NET_CLIENT_LIST_STACKING")
            )
            ids = [int(w, 16) for w in stack]
            samples.append({"active": active, "stack": ids})
            (self.root / "stacking.json").write_text(json.dumps(samples))
            match = re.search(r"0x[0-9a-fA-F]+", active)
            if match is None:
                self.fail("missing active window")
            self.assertEqual(int(match[0], 16), other, "permission prompt took key focus")
            above = ids[ids.index(other) + 1 :]
            self.assertFalse(
                set(above) & set(self.windows(self.proc.pid)),
                f"BenchLab-owned window raised above foreground: {above}",
            )

        sample()
        next_button = self.get("Next")
        action(next_button)  # background AT-SPI, without activating BenchLab
        wait_for(lambda: self.state()["prompt_state"] == "open", "permission prompt")
        for _ in range(12):
            sample()
            time.sleep(0.04)
        self.assertEqual(
            command("xprop", "-root", "_NET_CLIENT_LIST"),
            before,
            "permission prompt mapped another managed top-level",
        )
        self.assertEqual(command("xdotool", "getmouselocation", "--shell"), pointer)
        action(next_button)
        time.sleep(0.1)
        self.assertEqual(self.state()["next_count"], 1, "underlying Next active during prompt")
        action(self.get("Don’t Allow"))
        wait_for(lambda: self.state()["page"] == 2, "deny")
        sample()


if __name__ == "__main__":
    unittest.main()
