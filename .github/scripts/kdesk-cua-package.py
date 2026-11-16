#!/usr/bin/env python3
"""Assemble/validate installer cohorts; publish data without executing members."""
import argparse
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import stat
import subprocess
import tarfile
import sys

ARCHIVE = 'cua-driver-kdesk-x86_64-linux.tar.gz'
EVIDENCE = {'source.json', 'upstream-release.json', 'range-diff.txt', 'overlay.patch',
            'test-receipt.txt', 'elf-dependencies.txt', 'smoke.json'}
BUNDLES = {'package.sigstore.json', 'manifest.sigstore.json', 'checksums.sigstore.json'}

def require(ok, message):
    if not ok:
        raise ValueError(message)

def sha(data):
    return hashlib.sha256(data).hexdigest()

def canonical(obj):
    return json.dumps(obj, sort_keys=True, separators=(',', ':')).encode() + b'\n'

def execute(args):
    return subprocess.run(args, check=True, capture_output=True, timeout=120).stdout

def roster(source):
    paths = {'cua-driver-local', 'cua-cursor-theme'}
    for origin, target in [('libs/cua-driver/rust/Skills/cua-driver', 'Skills/cua-driver'),
                           ('libs/cua-driver/wayland-helper', 'wayland-helper')]:
        root = Path(source) / origin
        require(root.is_dir(), 'missing source roster: ' + origin)
        paths.add(target)
        for p in root.rglob('*'):
            require(not p.is_symlink(), 'source roster link refused')
            paths.add(target + '/' + p.relative_to(root).as_posix())
    paths.add('Skills')
    return paths

def inventory(stage):
    entries = []
    for p in sorted(Path(stage).rglob('*')):
        mode = p.lstat().st_mode
        require(stat.S_ISREG(mode) or stat.S_ISDIR(mode), 'links/special files refused')
        name = p.relative_to(stage).as_posix()
        require('\n' not in name and '\\' not in name, 'noncanonical member')
        data = p.read_bytes() if p.is_file() else b''
        entries.append({'path': name, 'type': 'file' if p.is_file() else 'directory',
                        'mode': stat.S_IMODE(mode), 'size': len(data), 'sha256': sha(data)})
    return entries

def assemble(args):
    output = Path(args.output)
    output.mkdir(parents=True, exist_ok=True)
    source = json.loads((output / 'source.json').read_text())
    require(source['source_sha'] == args.source_sha, 'source mismatch')
    smoke = json.loads((output / 'smoke.json').read_text())
    require(smoke['source_sha'] == args.source_sha and smoke['portal_input'] is True and smoke['status'] == 'passed', 'packaged smoke missing/failed')
    entries = inventory(args.stage)
    require({e['path'] for e in entries} == roster(args.repo), 'missing or unexpected installer roster member')
    for binary in ('cua-driver-local', 'cua-cursor-theme'):
        require(next(e for e in entries if e['path'] == binary)['mode'] & 0o111, 'binary is not executable')
    for name in EVIDENCE:
        require((output / name).is_file(), 'missing evidence: ' + name)
    with tarfile.open(output / ARCHIVE, 'w:gz', format=tarfile.PAX_FORMAT) as archive:
        for e in entries:
            info = tarfile.TarInfo(e['path'])
            info.mode = e['mode']
            info.size = e['size']
            info.mtime = 946684800
            info.type = tarfile.REGTYPE if e['type'] == 'file' else tarfile.DIRTYPE
            archive.addfile(info, io.BytesIO((Path(args.stage) / e['path']).read_bytes()) if e['type'] == 'file' else None)
    manifest = {'schema': 1, 'target': 'x86_64-unknown-linux-gnu', 'source_sha': args.source_sha,
                'source_tree': source['source_tree'], 'construction': source,
                'toolchain': args.toolchain, 'features': ['cua-driver/portal-input'],
                'build_image_digest': source['request']['build_image_digest'],
                'ci': {k: os.environ[k] for k in ('GITHUB_REPOSITORY', 'GITHUB_RUN_ID', 'GITHUB_RUN_ATTEMPT', 'GITHUB_WORKFLOW_REF', 'GITHUB_WORKFLOW_SHA', 'GITHUB_SHA', 'GITHUB_REF', 'GITHUB_ACTOR', 'GITHUB_TRIGGERING_ACTOR', 'GITHUB_EVENT_NAME')},
                'members': entries, 'smoke': smoke,
                'binary_sha256': {e['path']: e['sha256'] for e in entries if e['path'] in ('cua-driver-local', 'cua-cursor-theme')}}
    (output / 'package-manifest.json').write_bytes(canonical(manifest))
    files = sorted(p for p in output.iterdir() if p.is_file() and p.name != 'SHA256SUMS')
    (output / 'SHA256SUMS').write_text(''.join(f'{sha(p.read_bytes())}  {p.name}\n' for p in files))
    validate(output, args.source_sha)

def validate(output, source_sha):
    output = Path(output)
    manifest = json.loads((output / 'package-manifest.json').read_text())
    require(manifest['schema'] == 1 and manifest['source_sha'] == source_sha, 'source mismatch')
    require(manifest['target'] == 'x86_64-unknown-linux-gnu' and manifest['features'] == ['cua-driver/portal-input'], 'target/features mismatch')
    require(manifest['ci']['GITHUB_SHA'] == source_sha and manifest['ci']['GITHUB_REF'] == 'refs/tags/' + manifest['construction']['source_tag'], 'workflow/source mismatch')
    require(manifest['ci']['GITHUB_ACTOR'] == manifest['ci']['GITHUB_TRIGGERING_ACTOR'] == 'cresslank' and manifest['ci']['GITHUB_EVENT_NAME'] == 'workflow_dispatch', 'CI authority mismatch')
    entries = manifest['members']
    expected = {e['path']: e for e in entries}
    require(len(expected) == len(entries) and len(entries) <= 20000, 'duplicate/oversized inventory')
    require({'cua-driver-local', 'cua-cursor-theme', 'Skills/cua-driver/SKILL.md', 'wayland-helper/winrects@cua/extension.js'} <= expected.keys(), 'missing required roster')
    smoke = manifest['smoke']
    require(smoke['status'] == 'passed' and smoke['portal_input'] is True and smoke['source_sha'] == source_sha, 'smoke identity mismatch')
    require(smoke['binary_sha256'] == expected['cua-driver-local']['sha256'], 'smoke executable mismatch')
    require(manifest['binary_sha256'] == {n: expected[n]['sha256'] for n in ('cua-driver-local', 'cua-cursor-theme')}, 'binary identity mismatch')
    seen = set()
    total = 0
    with tarfile.open(output / ARCHIVE, 'r:gz') as archive:
        for member in archive:
            name = member.name
            require(name == PurePosixPath(name).as_posix() and not name.startswith('/') and '..' not in PurePosixPath(name).parts and '\\' not in name, 'unsafe archive path')
            require(name not in seen and name in expected, 'duplicate/unknown archive member')
            seen.add(name)
            require(member.isfile() or member.isdir(), 'unsafe archive member type')
            total += member.size
            require(0 <= member.size <= 512 * 1024 * 1024 and total <= 1024 * 1024 * 1024, 'archive size exceeded')
            e = expected[name]
            data = archive.extractfile(member).read() if member.isfile() else b''
            require(e == {'path': name, 'type': 'file' if member.isfile() else 'directory', 'mode': member.mode, 'size': len(data), 'sha256': sha(data)}, 'archive/manifest mismatch')
    require(seen == expected.keys(), 'incomplete archive')
    checksum_names = set()
    for line in (output / 'SHA256SUMS').read_text().splitlines():
        digest, name = line.split('  ', 1)
        require(name not in checksum_names and name == Path(name).name and name != 'SHA256SUMS', 'duplicate/unsafe checksum entry')
        checksum_names.add(name)
        require(sha((output / name).read_bytes()) == digest, 'mutated asset: ' + name)
    actual = {p.name for p in output.iterdir() if p.is_file()} - BUNDLES - {'SHA256SUMS'}
    require(checksum_names == actual and EVIDENCE | {ARCHIVE, 'package-manifest.json'} <= checksum_names, 'incomplete checksum/asset set')
    return manifest

def release_equal(release, output, names):
    assets = release['assets']
    require(len(assets) == len(names) and {a['name'] for a in assets} == names, 'existing release incomplete/conflicting')
    for a in assets:
        data = (output / a['name']).read_bytes()
        require(a['size'] == len(data) and a.get('digest') == 'sha256:' + sha(data), 'existing release bytes differ')

def publish(args):
    output = Path(args.output)
    manifest = validate(output, args.source_sha)
    require(BUNDLES <= {p.name for p in output.iterdir() if p.is_file()}, 'missing attestations')
    require(manifest['construction']['request']['policy_oid'] == args.policy_oid, 'unapproved control identity')
    require(manifest['ci']['GITHUB_RUN_ID'] == os.environ['GITHUB_RUN_ID'] and manifest['ci']['GITHUB_RUN_ATTEMPT'] == os.environ['GITHUB_RUN_ATTEMPT'], 'wrong build run/attempt')
    tag = manifest['construction']['source_tag']
    require(os.environ['GITHUB_REF'] == 'refs/tags/' + tag and os.environ['GITHUB_SHA'] == args.source_sha, 'publish/source mismatch')
    names = {p.name for p in output.iterdir() if p.is_file()}
    base = ['gh', 'api', 'repos/cresslank/cua']
    ref = json.loads(execute(base[:-1] + [base[-1] + '/git/ref/tags/' + tag]))
    require(ref['object']['type'] == 'commit' and ref['object']['sha'] == args.source_sha, 'tag moved')
    lookup = subprocess.run(['gh', 'api', 'repos/cresslank/cua/releases/tags/' + tag], capture_output=True)
    if lookup.returncode == 0:
        release = json.loads(lookup.stdout)
        release_equal(release, output, names)
        require(not release['draft'], 'existing draft requires owner inspection')
        print(json.dumps({'status': 'existing-complete', 'release_id': release['id'], 'url': release['html_url']}))
        return
    require(b'404' in lookup.stderr, 'release lookup failed')
    execute(['gh', 'release', 'create', tag, '--repo', 'cresslank/cua', '--verify-tag', '--draft', '--title', tag, '--notes', 'Source-qualified kdesk Linux package. See package-manifest.json and provenance bundles.'])
    execute(['gh', 'release', 'upload', tag, '--repo', 'cresslank/cua', *[str(output / n) for n in sorted(names)]])
    release = json.loads(execute(['gh', 'api', 'repos/cresslank/cua/releases/tags/' + tag]))
    require(release['draft'] is True, 'release was unexpectedly published')
    release_equal(release, output, names)
    execute(['gh', 'release', 'edit', tag, '--repo', 'cresslank/cua', '--draft=false', '--latest=false'])
    release = json.loads(execute(['gh', 'api', 'repos/cresslank/cua/releases/tags/' + tag]))
    release_equal(release, output, names)
    require(release['draft'] is False, 'publish readback failed')
    print(json.dumps({'status': 'published', 'release_id': release['id'], 'url': release['html_url']}))

def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('command', choices=['assemble', 'validate', 'publish'])
    p.add_argument('--output', required=True)
    p.add_argument('--source-sha', required=True)
    p.add_argument('--policy-oid')
    p.add_argument('--repo', default='.')
    p.add_argument('--stage')
    p.add_argument('--toolchain')
    args = p.parse_args()
    try:
        if args.command == 'assemble':
            assemble(args)
        elif args.command == 'validate':
            validate(args.output, args.source_sha)
        else:
            publish(args)
    except (ValueError, KeyError, OSError, tarfile.TarError, subprocess.SubprocessError) as error:
        print(json.dumps({'status': 'owner-hold', 'message': str(error)[:4096]}), file=sys.stderr)
        return 78
    return 0

if __name__ == '__main__':
    sys.exit(main())
