# SPDX-License-Identifier: FSL-1.1-MIT
# Copyright (c) 2026 Cua AI, Inc.
"""Hermes runner contract: real CLI/config/stdio/lifecycle, no host GUI or model.

Native driver/MCP edges use owned protocol fixtures; optional --list-tools uses
real Hermes when HERMES_BENCH_PYTHON/SOURCE/CUA_JEV are explicitly supplied.
"""

from __future__ import annotations

import argparse
import base64
import json
import os
import subprocess
import sys
import tempfile
import unittest
from dataclasses import replace
from pathlib import Path
from unittest import mock

HERE = Path(__file__).resolve().parent
BENCH = HERE.parent
sys.path.insert(0, str(BENCH))

import arms
import bench_core as core
import claude_arms as ca
import desktop_runtime as desktop
import run_bench as rb

ARM = "cc-hermes-cua-jev"
EXPECTED_TOOLS = (
    "computer_use",
    "cua_steps",
    "cua_script",
    "cua_task",
    "cua_window",
    "cua_browser",
    "cua_verify",
    "cua_zoom",
)


@unittest.skipUnless(sys.platform.startswith("linux"), "Hermes private-socket runner is Linux-only")
class HermesArmTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="hermes-arm-", dir=os.environ.get("TMPDIR"))
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.saved_builds = dict(ca.CUA_BUILDS)
        self.addCleanup(self.restore_builds)
        self.source = self.root / "read only core"
        (self.source / "tools/computer_use").mkdir(parents=True)
        (self.source / "tools/computer_use/tool.py").write_text(
            "# static config fixture; never imported\n"
        )
        self.plugin = self.root / "read only plugin"
        self.plugin.mkdir()
        (self.plugin / "plugin.yaml").write_text("name: cua-jev\n")
        self.allowlist = self.root / "authorized.json"
        self.allowlist.write_text('["cua:click:background", "cua:steps:run"]')
        self.options = ca.HermesOptions(
            self.source, Path(sys.executable), self.plugin, self.allowlist
        )
        self.run_dir = self.root / "run"
        self.run_dir.mkdir()

    def restore_builds(self):
        ca.CUA_BUILDS.clear()
        ca.CUA_BUILDS.update(self.saved_builds)

    def config(self):
        path, server = ca.mcp_config_for(ARM, self.run_dir, hermes=self.options)
        return path, server, json.loads(path.read_text())["mcpServers"][server]

    def test_registration_shared_prompt_and_no_driver_skill(self):
        self.assertIn(ARM, arms.CLAUDE_ARMS)
        self.assertNotIn(ARM, arms.DEFAULT_CLAUDE_ARMS)
        self.assertIn(ARM, ca.DRIVER_ARMS)
        self.assertNotIn(ARM, ca.CUA_ARMS)
        self.assertEqual(tuple(ca.HERMES_TOOLS), EXPECTED_TOOLS)
        self.assertEqual(ca.system_prompt_for(ARM), ca.SYSTEM_PROMPT)
        self.assertEqual(list(ca.prepare_cwd(ARM, self.root / "cwd").iterdir()), [])
        self.assertIsNone(desktop.arm_platform_error(ARM, ca.DRIVER_ARMS))
        with mock.patch.object(desktop, "detect_platform", return_value="macos"):
            reason = desktop.arm_platform_error(ARM, ca.DRIVER_ARMS)
            assert reason is not None
            self.assertIn("requires Linux", reason)

    def test_config_is_private_explicit_and_env_i_actually_scrubs(self):
        path, server, entry = self.config()
        self.assertEqual(server, "hermes-cua-jev")
        self.assertEqual(entry["command"], "/usr/bin/env")
        self.assertEqual(entry["env"], {})
        self.assertEqual(
            entry["args"][-4:], [str(self.options.python), "-s", "-B", str(BENCH / "hermes_mcp.py")]
        )
        assignments = dict(arg.split("=", 1) for arg in entry["args"][1:-4])
        profile = Path(assignments["HERMES_HOME"])
        home = Path(assignments["HOME"])
        self.assertTrue(profile.is_relative_to(self.run_dir))
        self.assertNotEqual(home, Path.home())
        self.assertEqual(assignments["PYTHONPATH"], str(self.source))
        for directory in (profile, home, profile.parent.parent, profile / "plugins"):
            self.assertEqual(directory.stat().st_mode & 0o777, 0o700)
        self.assertEqual((profile / "plugins/cua-jev").resolve(), self.plugin)
        config = json.loads((profile / "config.yaml").read_text())
        build = ca.CUA_BUILDS[ARM]
        self.assertEqual(config["computer_use"]["daemon_socket"], build.socket)
        self.assertNotIn(build.socket, (ca.AGENT_SOCKET, ca.MAIN_SOCKET, ca.SCRIPT_SOCKET))
        self.assertEqual(config["approvals"]["mode"], "manual")
        self.assertEqual(config["model"], {"provider": "anthropic", "default": "claude-sonnet-5-5"})
        self.assertEqual(config["agent"]["image_input_mode"], "native")
        self.assertEqual(config["command_allowlist"], json.loads(self.allowlist.read_text()))
        self.assertIs(config["security"]["allow_lazy_installs"], False)
        self.assertEqual(build.daemon_env, (ca.SCRIPT_FLAG,))
        self.assertEqual(ca.CUA_BUILDS["cc-cua-driver-main"].daemon_env, (ca.NO_SCRIPT_FLAG,))
        self.assertEqual(ca.CUA_BUILDS["cc-cua-driver-script"].daemon_env, (ca.SCRIPT_FLAG,))
        self.assertTrue(build.home.is_relative_to(self.run_dir))
        self.assertNotIn("mcp_servers", config)
        poison = {
            "HOME": "/do-not-read",
            "HERMES_HOME": "/do-not-read",
            "PYTHONPATH": "/do-not-import",
            "HERMES_YOLO_MODE": "1",
            "HERMES_INTERACTIVE": "1",
            "ANTHROPIC_API_KEY": "synthetic-secret",
            "OPENAI_API_KEY": "synthetic-secret",
            "CLAUDE_CODE_OAUTH_TOKEN": "synthetic-secret",
        }
        # Exercise the generated env -i launcher, replacing only the adapter program
        # with an environment reporter; no Hermes or desktop import is involved.
        argv = [
            entry["command"],
            *entry["args"][:-1],
            "-c",
            "import json,os;print(json.dumps(dict(os.environ)))",
        ]
        result = subprocess.run(argv, env=poison, capture_output=True, text=True, timeout=15)
        self.assertEqual(result.returncode, 0, result.stderr)
        observed = json.loads(result.stdout)
        for key in poison:
            if key in {"HOME", "HERMES_HOME", "PYTHONPATH"}:
                self.assertEqual(observed[key], assignments[key])
            else:
                self.assertNotIn(key, observed)
        self.assertEqual(observed["CUA_DRIVER_EXPERIMENTAL_SCRIPT"], "1")
        self.assertNotIn("CUA_DRIVER_DISABLE_RUN_SCRIPT", observed)
        argv = ca.claude_argv(
            mcp_config=path, server=server, model="test-model", max_turns=1, max_budget_usd=1
        )
        self.assertEqual(argv[argv.index("--allowedTools") + 1], "mcp__hermes-cua-jev")
        self.assertFalse(
            set(argv[argv.index("--tools") + 1].split(",")) & set(rb.v2.FORBIDDEN_BUILTINS)
        )

    def test_refuses_non_cua_allowlist_and_unsafe_reused_runtime(self):
        self.allowlist.write_text('["terminal:*"]')
        with self.assertRaisesRegex(ValueError, "only authorized cua:"):
            self.config()
        self.assertFalse((self.run_dir / "hermes-runtime").exists())
        self.allowlist.write_text("[]")
        root = self.run_dir / "hermes-runtime"
        root.mkdir(mode=0o755)
        root.chmod(0o755)  # qualify unsafe permissions even under the launcher's umask 077
        with self.assertRaisesRegex(ValueError, "owned private directories"):
            self.config()
        root.rmdir()
        root.symlink_to(self.root, target_is_directory=True)
        with self.assertRaisesRegex(ValueError, "owned private directories"):
            self.config()
        self.assertFalse((self.root / "home").exists())

    def test_live_host_guard_runs_before_any_allocation_or_binary(self):
        args = rb.build_parser().parse_args(["preflight", "--arms", ARM, "--bench-v2"])
        args.runs_dir = self.root / "must-not-create"
        with mock.patch.dict(os.environ, {"CDB_BENCH_DISPOSABLE": "0"}):
            with self.assertRaisesRegex(SystemExit, "CDB_BENCH_DISPOSABLE=1"):
                rb.make_ctx(args)
        self.assertFalse(args.runs_dir.exists())

    def cli_args(self):
        return [
            "dry-run",
            "--bench-v2",
            "--arms",
            ARM,
            "--tasks",
            "MB-09",
            "--phase1-runs",
            "1",
            "--phase2-runs",
            "0",
            "--ledger",
            str(self.root / "unused-ledger.jsonl"),
        ]

    def test_real_cli_dry_run_uses_env_defaults_without_child_or_network(self):
        child_tmp = self.root / "tmp"
        child_tmp.mkdir()
        user_home = self.root / "user"
        user_home.mkdir()
        env = {
            "PATH": "/usr/bin:/bin",
            "HOME": str(user_home),
            "TMPDIR": str(child_tmp),
            "CDB_BENCH_WORK": str(self.root / "work"),
            "CDB_HERMES_SOURCE": str(self.source),
            "CDB_HERMES_PYTHON": sys.executable,
            "CDB_HERMES_CUA_JEV": str(self.plugin),
            "CDB_HERMES_ALLOWLIST": str(self.allowlist),
        }
        code = """import runpy,sys
from pathlib import Path
marker=Path(sys.argv[1]); script=sys.argv[2]; sys.argv=sys.argv[2:]
def audit(event,args):
    if event in {'subprocess.Popen','os.system','os.posix_spawn','socket.connect','socket.getaddrinfo'}:
        marker.write_text(event); raise RuntimeError('dry-run I/O forbidden')
sys.addaudithook(audit)
runpy.run_path(script,run_name='__main__')
"""
        marker = self.root / "unexpected-io"
        result = subprocess.run(
            [
                sys.executable,
                "-s",
                "-B",
                "-c",
                code,
                str(marker),
                str(BENCH / "run_bench.py"),
                *self.cli_args(),
            ],
            env=env,
            cwd=user_home,
            capture_output=True,
            text=True,
            timeout=30,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(marker.exists(), marker.read_text() if marker.exists() else "")
        self.assertIn("1 trials in 1 block(s)", result.stdout)
        self.assertIn("MCP (planned, not executed)", result.stdout)
        self.assertNotIn("UNAVAILABLE", result.stdout)
        configs = list(child_tmp.glob(f"cdb-dry-*/mcp-{ARM}.json"))
        self.assertEqual(len(configs), 1)
        entry = json.loads(configs[0].read_text())["mcpServers"][ca.HERMES_SERVER]
        self.assertIn(str(self.options.python), entry["args"])
        self.assertFalse((self.root / "unused-ledger.jsonl").exists())

    def owned_driver(self):
        driver = self.root / "protocol-driver"
        driver.write_text(
            f"#!{sys.executable}\n"
            + """import json,os,signal,sys
from pathlib import Path
record=Path(os.environ['HOME']).parent/'served.json'
if sys.argv[1]=='serve':
    record.write_text(json.dumps({'argv':sys.argv[1:],'env':dict(os.environ),'pid':os.getpid()}))
    signal.pause()
elif sys.argv[1]=='status':
    raise SystemExit(0 if record.exists() else 1)
"""
        )
        driver.chmod(0o755)
        self.config()
        build = replace(ca.CUA_BUILDS[ARM], executable=driver)
        ca.CUA_BUILDS.clear()
        ca.CUA_BUILDS[ARM] = build
        ctx = rb.Ctx(
            argparse.Namespace(), self.run_dir, core.Ledger(self.root / "ledger"), {}, [], [ARM]
        )
        self.addCleanup(rb.shutdown, ctx)
        return ctx, build

    def test_private_daemon_lifecycle_is_real_and_script_enabled(self):
        ctx, build = self.owned_driver()
        rb.ensure_agent_daemon(ctx, ARM)
        proc = ctx.daemon
        self.assertIsNotNone(proc)
        assert proc is not None
        record = json.loads((build.state / "served.json").read_text())
        self.assertEqual(record["argv"][:3], ["serve", "--socket", build.socket])
        self.assertEqual(record["env"]["HOME"], str(build.home))
        self.assertEqual(record["env"].get("CUA_DRIVER_EXPERIMENTAL_SCRIPT"), "1")
        self.assertNotIn("CUA_DRIVER_DISABLE_RUN_SCRIPT", record["env"])
        rb.ensure_agent_daemon(ctx, ARM)
        assert ctx.daemon is not None
        self.assertEqual(ctx.daemon.pid, proc.pid)
        rb.shutdown(ctx)
        self.assertIsNotNone(proc.poll())
        with self.assertRaises(ProcessLookupError):
            os.kill(proc.pid, 0)
        ctx.daemon = None

    def test_real_stdio_capture_branch_validates_eight_tools_images_and_refusals(self):
        ctx, _ = self.owned_driver()
        server = self.root / "capture-edge.py"
        fixture = self.root / "fixture.json"
        call_log = self.root / "calls.jsonl"
        server.write_text("""import json,sys
from pathlib import Path
fixture=json.loads(Path(sys.argv[1]).read_text()); log=Path(sys.argv[2])
for line in sys.stdin:
    req=json.loads(line)
    if 'id' not in req: continue
    if req['method']=='initialize': result={}
    elif req['method']=='tools/list': result={'tools':[{'name':n} for n in fixture['tools']]}
    elif req['method']=='tools/call':
        with log.open('a') as output: output.write(json.dumps(req['params'])+'\\n')
        result=fixture['capture']
    else: raise RuntimeError('unexpected MCP method')
    print(json.dumps({'jsonrpc':'2.0','id':req['id'],'result':result}),flush=True)
""")
        config = self.root / "capture-mcp.json"
        config.write_text(
            json.dumps(
                {
                    "mcpServers": {
                        ca.HERMES_SERVER: {
                            "command": sys.executable,
                            "args": ["-s", "-B", str(server), str(fixture), str(call_log)],
                            "env": {"PATH": "/usr/bin:/bin"},
                        }
                    }
                }
            )
        )
        ctx.mcp[ARM] = (config, ca.HERMES_SERVER)
        colorful = b"P6\n2 1\n255\n\x00\x00\x00\xff\xff\xff"
        blank = b"P6\n2 1\n255\n" + b"\x00" * 6
        for names, image, refusal, expected in (
            (EXPECTED_TOOLS, colorful, False, None),
            ((*EXPECTED_TOOLS, "terminal"), colorful, False, "expected exact eight"),
            ((*EXPECTED_TOOLS, EXPECTED_TOOLS[0]), colorful, False, "expected exact eight"),
            (EXPECTED_TOOLS[:-1], colorful, False, "expected exact eight"),
            (EXPECTED_TOOLS, blank, False, "blank image"),
            (EXPECTED_TOOLS, b"not an image", False, "decode failed"),
            (EXPECTED_TOOLS, None, False, "no image block"),
            (EXPECTED_TOOLS, None, True, "native capture denied"),
        ):
            with self.subTest(expected=expected, tools=names):
                call_log.unlink(missing_ok=True)
                capture = {
                    "isError": refusal,
                    "content": (
                        [{"type": "text", "text": "native capture denied"}]
                        if refusal
                        else [
                            {
                                "type": "image",
                                "mimeType": "image/png",
                                "data": base64.b64encode(image).decode(),
                            }
                        ]
                        if image is not None
                        else []
                    ),
                }
                fixture.write_text(json.dumps({"tools": names, "capture": capture}))
                name, status, detail = rb.real_capture_check(ctx, ARM)
                self.assertEqual(name, f"{ARM} real capture")
                self.assertEqual(status, "fail" if expected else "pass", detail)
                self.assertIn(expected or "2x1 bytes=", detail)
                if "expected exact eight" not in (expected or ""):
                    calls = [json.loads(line) for line in call_log.read_text().splitlines()]
                    self.assertEqual(
                        calls,
                        [
                            {
                                "name": "computer_use",
                                "arguments": {
                                    "action": "capture",
                                    "mode": "som",
                                    "app": "BenchLab",
                                },
                            }
                        ],
                    )
                else:
                    self.assertFalse(call_log.exists())

    @unittest.skipUnless(
        all(
            os.environ.get(k)
            for k in (
                "HERMES_BENCH_PYTHON",
                "HERMES_BENCH_SOURCE",
                "HERMES_BENCH_CUA_JEV",
            )
        ),
        "explicit real Hermes paths required; import/list only, no desktop",
    )
    def test_generated_launcher_lists_real_hermes_tools_without_desktop(self):
        self.allowlist.write_text("[]")
        self.options = ca.HermesOptions(
            Path(os.environ["HERMES_BENCH_SOURCE"]),
            Path(os.environ["HERMES_BENCH_PYTHON"]),
            Path(os.environ["HERMES_BENCH_CUA_JEV"]),
            self.allowlist,
        )
        _, _, entry = self.config()
        result = subprocess.run(
            [entry["command"], *entry["args"], "--list-tools"],
            env={"HERMES_YOLO_MODE": "1", "ANTHROPIC_API_KEY": "synthetic-poison"},
            cwd=self.root,
            capture_output=True,
            text=True,
            timeout=60,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            [tool["name"] for tool in json.loads(result.stdout)["tools"]], list(EXPECTED_TOOLS)
        )


if __name__ == "__main__":
    unittest.main()
