#!/usr/bin/env python3
# SPDX-License-Identifier: FSL-1.1-MIT
# Copyright (c) 2026 Cua AI, Inc.
"""GUI-only stdio MCP bridge for the cc-hermes-cua-jev benchmark arm.

Run with a Hermes dependency interpreter and PYTHONPATH pointing at Hermes source::

    env HOME=<disposable-home> HERMES_HOME=<private-profile> \
        PYTHONPATH=<hermes-source> PYTHONDONTWRITEBYTECODE=1 \
        HERMES_CUA_DRIVER_CMD=<absolute-driver-binary> \
        <hermes-python> -B hermes_mcp.py [--session-id <trial-id>] [--list-tools]

The caller provisions config.yaml and plugins/cua-jev in that private profile.
computer_use.backend MUST be cua-jev; daemon_socket MUST be an explicit absolute
private socket. Copy only operator-authorized cua:* command_allowlist entries,
never the live profile, credentials or approval bypasses. Provider credentials
needed by Jev/cua_task are optional, explicit environment inputs, not CLI args.
No environment is changed by this module, and importing it does not load Hermes.

Only the eight names below can be listed or dispatched. Schemas come from the
real core/plugin registrations, narrowed to a closed GUI surface: no target
switch, browser file transfer/service restart, or app-launch arguments/files.
This is a tool host, not a Hermes agent or messaging bridge: it loads ONLY
cua-jev through the real plugin loader and invokes native tool handlers with
stable approval/session identity. Native approvals, hard blocks, desktop lease,
presence checks and redaction remain in those handlers; no gate is replaced.
Unapproved actions fail closed (there is no interactive approval UI). Set
approvals.mode=manual and security.allow_lazy_installs=false. The caller owns
driver startup and trial timeout. List/import never start a driver. Execution
currently requires Linux: this cua-jev revision ignores daemon_socket for the
macOS computer_use backend, so non-Linux calls explicitly refuse instead of
quietly attaching to the user's app service.

Transport: JSON-RPC 2.0, MCP newline JSON stdio, also accepting Content-Length
frames for older clients. stdout is protocol-only. --list-tools emits the same
MCP tools/list result then exits, without availability probes or desktop reads.
"""

from __future__ import annotations

import argparse
import base64
import contextlib
import copy
import json
import os
import re
import sys
import uuid
from pathlib import Path
from typing import Any, BinaryIO

SERVER_NAME = "hermes-cua-jev"
VERSION = "1.0.0"
TOOLS = (
    "computer_use",
    "cua_steps",
    "cua_script",
    "cua_task",
    "cua_window",
    "cua_browser",
    "cua_verify",
    "cua_zoom",
)
COMPUTER_ACTIONS = (
    "capture",
    "click",
    "double_click",
    "right_click",
    "middle_click",
    "drag",
    "scroll",
    "type",
    "key",
    "set_value",
    "wait",
    "list_apps",
    "list_windows",
    "focus_app",
)
BROWSER_ACTIONS = (
    "prepare",
    "open",
    "snapshot",
    "navigate",
    "click",
    "type",
    "pointer",
    "dialog",
    "release",
)
MAX_REQUEST_BYTES = 1024 * 1024
MAX_IMAGE_BYTES = 16 * 1024 * 1024


class AdapterError(RuntimeError):
    """Value-free operator diagnostic; never interpolate config or tool inputs."""


def _private_profile() -> Path:
    raw = os.environ.get("HERMES_HOME", "")
    home = os.environ.get("HOME", "")
    if not raw or not home or not Path(raw).is_absolute() or not Path(home).is_absolute():
        raise AdapterError("explicit absolute HOME and HERMES_HOME are required")
    path = Path(raw)
    if path.is_symlink() or not path.is_dir():
        raise AdapterError("HERMES_HOME must be an existing private directory")
    info = path.stat()
    if info.st_uid != os.getuid() or info.st_mode & 0o077:
        raise AdapterError("HERMES_HOME must be owned by this user and mode 0700")
    # An inherited agent/gateway approval context is not this stdio client's authority.
    for key in (
        "HERMES_YOLO_MODE",
        "HERMES_INTERACTIVE",
        "HERMES_GATEWAY_SESSION",
        "HERMES_SESSION_PLATFORM",
        "HERMES_SINGLE_QUERY_SESSION",
        "HERMES_CRON_SESSION",
    ):
        if os.environ.get(key, "").lower() not in ("", "0", "false", "off"):
            raise AdapterError("inherited approval contexts and bypasses are forbidden")
    return path.resolve()


def gui_schema(name: str, original: dict[str, Any]) -> dict[str, Any]:
    """Narrow a copy; never mutate Hermes' shared schema or relax nested guards."""
    schema = copy.deepcopy(original["parameters"])
    schema["additionalProperties"] = False
    props = schema["properties"]
    props.pop("target", None)  # one private desktop, no alternate connection/lease namespace
    if name == "computer_use":
        props["action"]["enum"] = [a for a in COMPUTER_ACTIONS if a in props["action"]["enum"]]
    if name == "cua_browser":
        props["action"]["enum"] = [a for a in BROWSER_ACTIONS if a in props["action"]["enum"]]
        for key in ("files", "destination", "restart_driver"):
            props.pop(key, None)
    if name == "cua_window":
        for key in ("arguments", "urls"):
            props.pop(key, None)
    return {
        "name": name,
        "description": original["description"]
        + " Benchmark GUI-only scope: no target switching, file-transfer APIs, service restart or launch arguments.",
        "inputSchema": schema,
    }


def _failed(value: Any) -> bool:
    return isinstance(value, dict) and (
        bool(value.get("error"))
        or value.get("isError") is True
        or value.get("ok") is False
        or value.get("success") is False
        or value.get("status") in {"error", "refused", "failed", "denied", "stopped"}
        or value.get("effect") == "refused"
    )


def mcp_result(raw: Any) -> dict[str, Any]:
    """Translate Hermes text/data-URL images, never dereference a file or URL."""
    value = raw
    if isinstance(raw, str):
        try:
            value = json.loads(raw)
        except ValueError:
            pass
    if isinstance(value, dict) and value.get("_multimodal") is True:
        content = []
        failed = False
        for part in value.get("content", []):
            if part.get("type") == "text":
                text = part["text"]
                content.append({"type": "text", "text": text})
                try:
                    failed = failed or _failed(json.loads(text))
                except ValueError:
                    pass
            elif part.get("type") == "image_url":
                url = part["image_url"]["url"]
                match = re.fullmatch(
                    r"data:(image/(?:png|jpeg|webp));base64,([A-Za-z0-9+/=]+)", url
                )
                if not match or len(match[2]) > MAX_IMAGE_BYTES * 4 // 3 + 4:
                    raise AdapterError("invalid inline image result")
                base64.b64decode(match[2], validate=True)
                content.append({"type": "image", "mimeType": match[1], "data": match[2]})
            else:
                raise AdapterError("unsupported multimodal content")
        if not content:
            raise AdapterError("empty multimodal result")
        return {"content": content, "isError": failed or _failed(value)}
    text = raw if isinstance(raw, str) else json.dumps(raw, ensure_ascii=False, allow_nan=False)
    return {"content": [{"type": "text", "text": text}], "isError": _failed(value)}


def refusal(code: str) -> dict[str, Any]:
    return mcp_result({"status": "refused", "code": code})


class HermesTools:
    """Real installed core/plugin, lazy native desktop connection, one trial identity."""

    def __init__(self, session_id: str | None = None):
        self.home = _private_profile()
        self.session_id = session_id or "bench-" + uuid.uuid4().hex
        self.manager = None
        with contextlib.redirect_stdout(sys.stderr):
            # Parse the actual file, not defaults returned after a malformed config.
            from utils import fast_safe_load

            config = fast_safe_load((self.home / "config.yaml").read_text())
            self._check_config(config)
            self.config = copy.deepcopy(config)
            from hermes_cli.plugins import PluginManager, parse_manifest_file
            from tools.computer_use import tool
            from tools.registry import registry
            from jsonschema import Draft7Validator

            plugin_dir = self.home / "plugins" / "cua-jev"
            manifest = parse_manifest_file(plugin_dir / "plugin.yaml", plugin_dir, "user", "")
            if manifest is None or manifest.name != "cua-jev":
                raise AdapterError("cua-jev manifest is missing or incompatible")
            self.manager = PluginManager()
            try:
                # Narrow load, not discover_plugins(): no project/pip/bundled extras.
                self.manager._load_plugin(manifest)
                loaded = self.manager._plugins.get("cua-jev")
                if loaded is None or not loaded.enabled or loaded.error:
                    raise AdapterError("cua-jev registration failed")
                self.registry = registry
                self.computer = tool
                self.schemas = {}
                self.validators = {}
                for name in TOOLS:
                    original = (
                        tool.get_computer_use_schema()
                        if name == "computer_use"
                        else registry.get_schema(name)
                    )
                    if not isinstance(original, dict):
                        raise AdapterError("required GUI tool schema is missing")
                    projected = gui_schema(name, original)
                    Draft7Validator.check_schema(projected["inputSchema"])
                    self.schemas[name] = projected
                    self.validators[name] = Draft7Validator(projected["inputSchema"])
            except BaseException:
                self.close()
                raise

    @staticmethod
    def _check_config(config: Any) -> None:
        if not isinstance(config, dict):
            raise AdapterError("config.yaml must be an object")
        cua = config.get("computer_use", {})
        if not isinstance(cua, dict) or cua.get("backend") != "cua-jev":
            raise AdapterError("computer_use.backend must be cua-jev")
        socket = cua.get("daemon_socket")
        if (
            not isinstance(socket, str)
            or not socket
            or not Path(socket).is_absolute()
            or socket != socket.strip()
        ):
            raise AdapterError("an explicit absolute private daemon_socket is required")
        if cua.get("permission_mode", "standard") != "standard":
            raise AdapterError("only standard driver permission mode is supported")
        security = config.get("security", {})
        if not isinstance(security, dict) or security.get("allow_lazy_installs") is not False:
            raise AdapterError("security.allow_lazy_installs must be false")
        if not os.environ.get("HERMES_CUA_DRIVER_CMD", "").strip():
            raise AdapterError(
                "an explicit HERMES_CUA_DRIVER_CMD is required; acquisition is forbidden"
            )
        approvals = config.get("approvals", {})
        if (
            not isinstance(approvals, dict)
            or approvals.get("mode") != "manual"
            or any(
                approvals.get(key, "deny") != "deny"
                for key in ("single_query_mode", "unattended_mode", "cron_mode")
            )
        ):
            raise AdapterError("approval bypass/auto-approval policies are forbidden")
        allow = config.get("command_allowlist", [])
        if not isinstance(allow, list) or any(
            not isinstance(x, str) or not x.startswith("cua:") for x in allow
        ):
            raise AdapterError("command_allowlist may contain only authorized cua:* entries")
        plugins = config.get("plugins", {})
        if not isinstance(plugins, dict) or plugins.get("isolation", "inprocess") == "host":
            raise AdapterError("cua-jev requires in-process plugin loading")
        # A distinct socket also disables the plugin's automatic host-service restart.
        if config.get("mcp_servers") or config.get("hooks"):
            raise AdapterError("external MCP servers and shell hooks are outside this tool host")

    def list_tools(self) -> dict[str, Any]:
        return {"tools": list(self.schemas.values())}

    def call(self, name: Any, arguments: Any) -> dict[str, Any]:
        if not isinstance(name, str) or name not in self.schemas:
            return refusal("tool_not_allowed")
        if not isinstance(arguments, dict) or not self.validators[name].is_valid(arguments):
            return refusal("invalid_gui_arguments")
        with contextlib.redirect_stdout(sys.stderr):
            from tools import approval_context
            from tools.approval import is_approval_bypass_active_for_session

            # Re-read operator config at every boundary: a changed backend/socket or
            # approval policy must not silently broaden the startup contract.
            from utils import fast_safe_load
            from hermes_cli.config import load_config_readonly

            try:
                current = fast_safe_load((self.home / "config.yaml").read_text())
                self._check_config(current)
                effective = load_config_readonly()
                if (
                    current != self.config
                    or getattr(effective, "read_error", None) is not None
                    or any(
                        effective.get("computer_use", {}).get(key) != value
                        for key, value in current["computer_use"].items()
                    )
                ):
                    return refusal("configuration_changed")
                if sys.platform != "linux":
                    # Current cua-jev binds computer_use.daemon_socket only on Linux.
                    # Do not silently route computer_use to the macOS app service.
                    return refusal("private_socket_routing_unsupported")
                token = approval_context.set_current_session_key(self.session_id)
                ids = approval_context.set_current_observability_context(session_id=self.session_id)
                try:
                    if is_approval_bypass_active_for_session(self.session_id):
                        return refusal("approval_bypass_forbidden")
                    # No model_tools discovery or substitute safety callback. The real
                    # computer_use / plugin handlers retain all native tool-level gates.
                    raw = (
                        self.computer.handle_computer_use(arguments, session_id=self.session_id)
                        if name == "computer_use"
                        else self.registry.dispatch(name, arguments, session_id=self.session_id)
                    )
                    return mcp_result(raw)
                finally:
                    approval_context.reset_current_observability_context(ids)
                    approval_context.reset_current_session_key(token)
            except Exception:
                return refusal("hermes_tool_failed")

    def close(self) -> None:
        with contextlib.redirect_stdout(sys.stderr):
            if self.manager is not None:
                self.manager.unload()
                self.manager = None
            if hasattr(self, "computer"):
                self.computer.release_computer_use_session(self.session_id)


def _loads(data: bytes) -> Any:
    def invalid_constant(_: str) -> None:
        raise ValueError("non-finite JSON")

    return json.loads(data, parse_constant=invalid_constant)


def read_message(stream: BinaryIO) -> tuple[Any, bool] | None:
    line = stream.readline(MAX_REQUEST_BYTES + 1)
    if not line:
        return None
    if len(line) > MAX_REQUEST_BYTES:
        raise AdapterError("request too large")
    framed = line.lower().startswith(b"content-length:")
    if framed:
        size = int(line.split(b":", 1)[1])
        if not 0 < size <= MAX_REQUEST_BYTES:
            raise AdapterError("invalid content length")
        budget = MAX_REQUEST_BYTES
        while True:
            header = stream.readline(budget + 1)
            budget -= len(header)
            if not header or budget < 0:
                raise AdapterError("incomplete headers")
            if header in (b"\r\n", b"\n"):
                break
        line = stream.read(size)
        if len(line) != size:
            raise AdapterError("incomplete request")
    return _loads(line), framed


def handle_message(tools: HermesTools, request: Any) -> dict[str, Any] | None:
    if not isinstance(request, dict) or request.get("jsonrpc") != "2.0":
        return {
            "jsonrpc": "2.0",
            "id": None,
            "error": {"code": -32600, "message": "Invalid Request"},
        }
    if "id" not in request:
        return None
    ident = request["id"]
    method = request.get("method")
    params = request.get("params", {})
    if not isinstance(params, dict):
        return {
            "jsonrpc": "2.0",
            "id": ident,
            "error": {"code": -32602, "message": "Invalid params"},
        }
    if method == "initialize":
        requested = params.get("protocolVersion")
        version = (
            requested if requested in {"2024-11-05", "2025-03-26", "2025-06-18"} else "2025-06-18"
        )
        result = {
            "protocolVersion": version,
            "capabilities": {"tools": {"listChanged": False}},
            "serverInfo": {"name": SERVER_NAME, "version": VERSION},
        }
    elif method == "ping":
        result = {}
    elif method == "tools/list":
        result = tools.list_tools()
    elif method == "tools/call":
        result = tools.call(params.get("name"), params.get("arguments", {}))
    else:
        return {
            "jsonrpc": "2.0",
            "id": ident,
            "error": {"code": -32601, "message": "Method not found"},
        }
    return {"jsonrpc": "2.0", "id": ident, "result": result}


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--list-tools", action="store_true", help="List schemas and exit; no desktop probe"
    )
    parser.add_argument("--session-id", help="Stable trial identity (default: random per process)")
    args = parser.parse_args(argv)
    tools = None
    try:
        tools = HermesTools(args.session_id)
        if args.list_tools:
            print(json.dumps(tools.list_tools(), ensure_ascii=False))
            return 0
        while message := read_message(sys.stdin.buffer):
            request, framed = message
            response = handle_message(tools, request)
            if response is not None:
                data = json.dumps(response, ensure_ascii=False, allow_nan=False).encode("utf-8")
                wire = (
                    b"Content-Length: " + str(len(data)).encode() + b"\r\n\r\n" + data
                    if framed
                    else data + b"\n"
                )
                sys.stdout.buffer.write(wire)
                sys.stdout.buffer.flush()
        return 0
    except (Exception, KeyboardInterrupt):
        # Do not emit exceptions containing typed text, paths or provider secrets.
        print(
            "hermes-cua-jev: adapter failed; check isolated config and dependencies",
            file=sys.stderr,
        )
        return 2
    finally:
        if tools is not None:
            tools.close()


if __name__ == "__main__":
    raise SystemExit(main())
