# SPDX-License-Identifier: FSL-1.1-MIT
# Copyright (c) 2026 Cua AI, Inc.
"""Explicit file credentials, private auth stores and pre-persistence output redaction.

Claude 2.1.316 reads raw UTF-8 from the inherited fd. Its own resolver can
fall back to saved keys/helpers, so an fd alone is NOT an isolation boundary.
"""

from __future__ import annotations

import contextlib
import json
import os
import stat
import tempfile
from collections.abc import Iterator, Mapping
from dataclasses import dataclass, field

TOKEN_ENV = "CDB_CLAUDE_TOKEN_FILE"
API_ENV = "CDB_ANTHROPIC_API_KEY_FILE"
BOTH_ERROR = "Exactly one Claude credential mode is allowed; both credential files are set"
REQUIRED_ERROR = "Explicit Claude credential file required; no login profile will be read"
FILE_ERROR = "Claude credential must be an owned non-symlink regular 0600 file"
VALUE_ERROR = "Claude credential must contain one nonempty ASCII credential (maximum 65536 bytes)"
FD_ENV = {
    "setup_token": "CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR",
    "api_key": "CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR",
}


def billing_mode(env: Mapping[str, str] | None = None, *, required: bool = False) -> str | None:
    env = os.environ if env is None else env
    token, api = bool(env.get(TOKEN_ENV)), bool(env.get(API_ENV))
    if token and api:
        raise ValueError(BOTH_ERROR)
    if not token and not api:
        if required:
            raise ValueError(REQUIRED_ERROR)
        return None
    return "api_key" if api else "setup_token"


@dataclass
class CredentialSpawn:
    argv: list[str]
    env: dict[str, str]
    fd: int | None = None
    mode: str | None = None
    # Never expose the credential through repr, evidence or exception messages.
    _secret: bytes = field(default=b"", repr=False)

    @property
    def pass_fds(self) -> tuple[int, ...]:
        return (self.fd,) if self.fd is not None else ()

    def redact_bytes(self, data: bytes) -> bytes:
        if not self._secret:
            return data
        escaped = json.dumps(self._secret.decode("ascii"))[1:-1].encode("ascii")
        return data.replace(escaped, b"[REDACTED]").replace(self._secret, b"[REDACTED]")

    def redact(self, text: str) -> str:
        return self.redact_bytes(text.encode("utf-8")).decode("utf-8")


@contextlib.contextmanager
def spawn(argv: list[str], env: dict[str, str]) -> Iterator[CredentialSpawn]:
    """Validate before spawn. Every explicit spawn gets a new fd and empty profile.

    Legacy non-container runs with no file retain their existing auth behavior.
    Disposable containers never implicitly authenticate with a login profile.
    """
    mode = billing_mode(required=os.environ.get("CDB_BENCH_DISPOSABLE") == "1")
    if mode is None:
        yield CredentialSpawn(argv, env)
        return
    path = os.environ[API_ENV if mode == "api_key" else TOKEN_ENV]
    fd = None
    try:
        try:
            fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
            info = os.fstat(fd)
            if (
                not stat.S_ISREG(info.st_mode)
                or info.st_uid != os.getuid()
                or stat.S_IMODE(info.st_mode) != 0o600
            ):
                raise ValueError(FILE_ERROR)
        except OSError:
            raise ValueError(FILE_ERROR) from None
        raw = os.read(fd, 65537)
        secret = raw.strip()
        if len(raw) > 65536 or not secret or any(b < 33 or b > 126 for b in secret):
            raise ValueError(VALUE_ERROR)
        os.lseek(fd, 0, os.SEEK_SET)
        # No inherited auth/provider/profile/remote overrides. Claude may consult
        # its global credential store even with --setting-sources '', hence all
        # auth roots are fresh and outside the retained evidence tree.
        clean = {
            k: v
            for k, v in env.items()
            if k
            in {
                "PATH",
                "USER",
                "LANG",
                "TMPDIR",
                "TERM",
                "CLAUDE_CODE_DISABLE_AUTO_MEMORY",
                "CLAUDE_CODE_DISABLE_CLAUDE_MDS",
                "DISABLE_AUTOUPDATER",
            }
        }
        args = list(argv)
        if "--debug-file" in args or "--settings" in args or "--bare" in args:
            raise ValueError(
                "Explicit credential mode refuses debug files and auth-setting overrides"
            )
        if "--setting-sources" in args:
            args[args.index("--setting-sources") + 1] = ""
        else:
            args += ["--setting-sources", ""]
        with tempfile.TemporaryDirectory(prefix="cdb-claude-auth-") as home:
            clean.update(
                HOME=home,
                CLAUDE_CONFIG_DIR=home + "/claude",
                XDG_CONFIG_HOME=home + "/config",
                ANTHROPIC_CONFIG_DIR=home + "/anthropic",
            )
            clean[FD_ENV[mode]] = str(fd)
            yield CredentialSpawn(args, clean, fd, mode, secret)
    finally:
        if fd is not None:
            os.close(fd)
