#!/usr/bin/env python3
"""Acquire or validate the process-inherited CUA local promotion lock."""

from __future__ import annotations

import fcntl
import os
import stat
import sys
from pathlib import Path

PROTOCOL = "1"
DEFAULT_LOCK = f"/tmp/cua-driver-local-install-transaction-{os.geteuid()}.lock"
FD_ENV = "CUA_DRIVER_INSTALL_TRANSACTION_LOCK_FD"
DEV_ENV = "CUA_DRIVER_INSTALL_TRANSACTION_LOCK_DEV"
INO_ENV = "CUA_DRIVER_INSTALL_TRANSACTION_LOCK_INO"
PROTOCOL_ENV = "CUA_DRIVER_INSTALL_TRANSACTION_LOCK_PROTOCOL"


def checked_open(path: str) -> int:
    flags = os.O_RDWR | os.O_CREAT | os.O_NONBLOCK
    flags |= getattr(os, "O_NOFOLLOW", 0)
    flags |= getattr(os, "O_CLOEXEC", 0)
    fd = os.open(path, flags, 0o600)
    try:
        opened = os.fstat(fd)
        visible = os.lstat(path)
        if not stat.S_ISREG(opened.st_mode):
            raise RuntimeError("transaction lock is not a regular file")
        if opened.st_uid != os.geteuid():
            raise RuntimeError("transaction lock is not owned by the current user")
        if opened.st_nlink != 1:
            raise RuntimeError("transaction lock has an unsafe link count")
        if stat.S_IMODE(opened.st_mode) != 0o600:
            raise RuntimeError("transaction lock permissions are not 0600")
        if (opened.st_dev, opened.st_ino) != (visible.st_dev, visible.st_ino):
            raise RuntimeError("transaction lock changed while it was opened")
        return fd
    except BaseException:
        os.close(fd)
        raise


def validate_inherited(path: str) -> None:
    if os.environ.get(PROTOCOL_ENV) != PROTOCOL:
        raise RuntimeError("unsupported inherited transaction lock protocol")
    try:
        fd = int(os.environ[FD_ENV])
        expected = (int(os.environ[DEV_ENV]), int(os.environ[INO_ENV]))
    except (KeyError, ValueError) as exc:
        raise RuntimeError("invalid inherited transaction lock identity") from exc
    if fd < 3:
        raise RuntimeError("invalid inherited transaction lock descriptor")

    inherited = os.fstat(fd)
    if not stat.S_ISREG(inherited.st_mode) or inherited.st_uid != os.geteuid():
        raise RuntimeError("inherited transaction lock has unsafe type or owner")
    if inherited.st_nlink != 1 or stat.S_IMODE(inherited.st_mode) != 0o600:
        raise RuntimeError("inherited transaction lock has unsafe metadata")
    if (inherited.st_dev, inherited.st_ino) != expected:
        raise RuntimeError("inherited transaction lock identity does not match")

    canonical = checked_open(path)
    try:
        canonical_info = os.fstat(canonical)
        if (canonical_info.st_dev, canonical_info.st_ino) != expected:
            raise RuntimeError("inherited transaction lock is not the host lock")
    finally:
        os.close(canonical)

    # Reasserting LOCK_EX succeeds for the same inherited open-file description.
    # A forged descriptor loses with EWOULDBLOCK when the real owner is active;
    # when uncontended this call safely acquires the lock itself.
    try:
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError as exc:
        raise RuntimeError("inherited descriptor does not own the transaction lock") from exc
    visible = os.lstat(path)
    if (visible.st_dev, visible.st_ino) != expected:
        raise RuntimeError("transaction lock changed during inherited validation")
    os.set_inheritable(fd, True)


def acquire_and_exec(path: str, command: list[str]) -> None:
    fd = checked_open(path)
    try:
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as exc:
            raise RuntimeError(
                "another CUA install/acceptance transaction is active"
            ) from exc
        info = os.fstat(fd)
        visible = os.lstat(path)
        if (info.st_dev, info.st_ino) != (visible.st_dev, visible.st_ino):
            raise RuntimeError("transaction lock changed while acquiring ownership")
        os.set_inheritable(fd, True)
        env = os.environ.copy()
        env.update(
            {
                FD_ENV: str(fd),
                DEV_ENV: str(info.st_dev),
                INO_ENV: str(info.st_ino),
                PROTOCOL_ENV: PROTOCOL,
            }
        )
        os.execve(command[0], command, env)
    finally:
        os.close(fd)


def main() -> int:
    try:
        # One fixed per-UID host path serializes every supported local promotion.
        # Tests that need isolation rewrite this literal in a disposable copy;
        # shipped execution has no environment-selected lock domain.
        path = DEFAULT_LOCK
        Path(path).parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        if len(sys.argv) == 2 and sys.argv[1] == "--validate":
            validate_inherited(path)
            return 0
        if len(sys.argv) >= 3 and sys.argv[1] == "--acquire":
            acquire_and_exec(path, sys.argv[2:])
            return 0
        raise RuntimeError("usage: lock-helper --validate | --acquire COMMAND [ARG ...]")
    except (OSError, RuntimeError) as exc:
        detail = exc.strerror if isinstance(exc, OSError) and exc.strerror else str(exc)
        print(f"error: refusing CUA promotion transaction lock: {detail}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
