#!/usr/bin/python3
"""Admission for this private fixture: Cua jobs share per-UID desktop locks.

Unrelated Cargo jobs in other repositories do not. Inspect actual cwd/manifest
paths instead of either ignoring contention or waiting on every Rust project on
a development host. This is test harness code, never a driver input authority.
"""
from pathlib import Path
import os
import sys

blocked = []
for entry in Path('/proc').iterdir():
    if not entry.name.isdigit() or int(entry.name) == os.getpid():
        continue
    try:
        if entry.stat().st_uid != os.getuid():
            continue
        argv = [part.decode(errors='replace') for part in (entry/'cmdline').read_bytes().split(b'\0') if part]
        command = ' '.join(argv)
        cwd = (entry/'cwd').resolve(strict=True)
        related = '/libs/cua-driver/' in str(cwd) or 'libs/cua-driver/' in command
        related = related or any((parent/'libs/cua-driver/rust/Cargo.toml').is_file()
                                 for parent in [cwd, *cwd.parents])
        # A coordinator waiting on native.lock may mention a future Cargo/lane
        # in bash -c text; it is not an executing contender. Inspect argv tokens.
        cargo = bool(argv) and Path(argv[0]).name == 'cargo' and any(a in ('test', 'build', 'check') for a in argv[1:])
        lane = any(Path(arg).name in ('slice_chain.sh', 'e2e_lane.sh', 'e2e_contained.sh') for arg in argv[:3])
        shell = bool(argv) and Path(argv[0]).name == 'gnome-shell' and '--headless' in argv
        if shell or related and (cargo or lane):
            blocked.append((entry.name, str(cwd), command))
    except (OSError, PermissionError):
        continue
for record in blocked:
    print('Cua desktop-lock contender:', record, flush=True)
sys.exit(0 if blocked else 1)
