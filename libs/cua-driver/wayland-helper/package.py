#!/usr/bin/env python3
"""Build a WinRects extension tree; never installs or contacts a session bus."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import stat


def sha(data):
    return hashlib.sha256(data).hexdigest()


def package(source, output):
    source = source.resolve(strict=True)
    if output.exists() or output.is_symlink():
        raise ValueError('output must not exist')
    files = {}
    for path in sorted((source / 'impl').rglob('*')):
        if path.is_symlink():
            raise ValueError('symlink in implementation')
        if path.is_dir():
            continue
        if not stat.S_ISREG(path.stat().st_mode):
            raise ValueError('nonregular implementation file')
        files[path.relative_to(source / 'impl').as_posix()] = path.read_bytes()
    if not files or len(files) > 64 or sum(map(len, files.values())) > 2 * 1024 * 1024:
        raise ValueError('implementation budget exceeded')
    manifest = json.dumps(dict(loader_abi=1, entrypoint='main.js', helper_api=18, protocol=4,
        files=[dict(path=name, size=len(data), sha256=sha(data)) for name, data in files.items()]),
        sort_keys=True, separators=(',', ':')).encode() + b'\n'
    digest = sha(manifest)
    # Publication must satisfy the loader's owner-only mutation predicate even
    # when the caller uses umask 0002 (or a more restrictive mask). This builder
    # is single-threaded; restore the caller's mask even on partial failure.
    previous_umask = os.umask(0o077)
    try:
        output.mkdir(parents=True)
        try:
            for name in ['extension.js', 'metadata.json']:
                if (source / name).is_symlink():
                    raise ValueError('symlink in bootstrap')
                shutil.copyfile(source / name, output / name)
            directory = output / 'impl' / digest
            directory.mkdir(parents=True)
            (directory / 'manifest.json').write_bytes(manifest)
            for name, data in files.items():
                target = directory / name
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes(data)
            (output / 'selected.json').write_text(json.dumps(dict(loader_abi=1, selected=digest, previous=None)) + '\n')
        except BaseException:
            shutil.rmtree(output)
            raise
    finally:
        os.umask(previous_umask)
    return digest


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('output', type=Path, help='new output directory (not an installed extension)')
    parser.add_argument('--source', type=Path, default=Path(__file__).parent / 'winrects@cua')
    args = parser.parse_args()
    print(package(args.source, args.output))


if __name__ == '__main__':
    main()
