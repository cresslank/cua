#!/usr/bin/env python3
"""Owner-only fork preparation. stdout is one JSON receipt; holds exit 78.

Local prepare accepts already-fetched Git objects and frozen release JSON. The
workflow fetch command is the only network acquisition path. No conflict repair.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile

POLICY = '.github/kdesk-cua-driver-policy.json'
FEATURE_FILE = 'libs/cua-driver/rust/crates/platform-linux/src/tools/get_config.rs'
SHA = re.compile(r'[0-9a-f]{40}')
TAG = re.compile(r'cua-driver-rs-v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)')
REQUEST_FIELDS = {'request_key', 'upstream_tag', 'upstream_oid', 'policy_oid', 'carry_digest', 'build_image_digest'}
ALLOWED_OVERLAY = {
    POLICY, '.github/workflows/kdesk-cua-driver-prepare.yml',
    '.github/workflows/kdesk-cua-driver-build.yml',
    *('.github/scripts/' + n for n in (
        'kdesk-cua-driver.py', 'kdesk-cua-package.py', 'kdesk-cua-smoke.py',
        'kdesk-cua-build.sh', 'kdesk-cua-portal-input.patch')),
}

class Hold(Exception):
    pass

def require(ok, message):
    if not ok:
        raise Hold(message)

def run(args, cwd=None, data=None, env=None):
    p = subprocess.run(args, cwd=cwd, input=data, stdout=subprocess.PIPE,
                       stderr=subprocess.PIPE, env=env, timeout=120)
    if p.returncode:
        raise Hold((p.stdout + p.stderr).decode(errors='replace')[:32768])
    return p.stdout

def git(repo, *args, data=None, env=None):
    return run(['git', '-c', 'core.hooksPath=/dev/null', '-c', 'rerere.enabled=false',
                '-c', 'commit.gpgSign=false', '-c', 'tag.gpgSign=false', *args], repo, data, env)

def text(repo, *args):
    return git(repo, *args).decode().strip()

def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(',', ':')).encode()

def digest(data):
    return hashlib.sha256(data).hexdigest()

def policy_at(repo, oid):
    return json.loads(git(repo, 'show', f'{oid}:{POLICY}'))

def carry_digest(policy):
    return digest(canonical({k: policy[k] for k in ('carry_base', 'carry_head', 'carry')}))

def validate_request(request):
    require(set(request) == REQUEST_FIELDS, 'request fields differ from exact schema')
    require(all(isinstance(v, str) for v in request.values()), 'request values must be strings')
    require(bool(re.fullmatch(r'[a-zA-Z0-9_-]{16,80}', request['request_key'])), 'invalid request key')
    require(bool(TAG.fullmatch(request['upstream_tag'])), 'not a component release tag')
    for key in ('upstream_oid', 'policy_oid'):
        require(bool(SHA.fullmatch(request[key])), f'invalid {key}')
    require(bool(re.fullmatch(r'[0-9a-f]{64}', request['carry_digest'])), 'invalid carry digest')
    require(bool(re.fullmatch(r'sha256:[0-9a-f]{64}', request['build_image_digest'])), 'image must be digest pinned')

def source_tag(request):
    return 'kdesk-' + request['upstream_tag'] + '-' + request['carry_digest'] + '-' + request['policy_oid'] + '-' + request['upstream_oid']

def guard(request, phase, environ=None):
    validate_request(request)
    e = os.environ if environ is None else environ
    require(e.get('GITHUB_REPOSITORY') == 'cresslank/cua', 'wrong repository')
    require(e.get('GITHUB_ACTOR') == e.get('GITHUB_TRIGGERING_ACTOR') == 'cresslank', 'owner and triggering owner required')
    require(e.get('GITHUB_EVENT_NAME') == 'workflow_dispatch', 'dispatch only')
    require(e.get('PIPELINE_ENABLED') == 'true' and e.get('CI_RUNNER') == 'private-vm', 'pipeline dormant or wrong runner')
    require(e.get('BUILD_IMAGE_DIGEST') == request['build_image_digest'], 'runner image policy mismatch')
    if phase == 'prepare':
        require(e.get('GITHUB_REF') == 'refs/heads/main', 'prepare must use main')
        require(e.get('GITHUB_SHA') == request['policy_oid'], 'policy must be workflow source')
    else:
        require(e.get('GITHUB_REF') == 'refs/tags/' + source_tag(request), 'build must use generated source tag')

def fetch(repo, request, output):
    guard(request, 'prepare' if os.environ.get('GITHUB_REF') == 'refs/heads/main' else 'build')
    policy = policy_at(repo, request['policy_oid'])
    require(carry_digest(policy) == request['carry_digest'], 'carry definition mismatch')
    release = json.loads(run(['gh', 'api', 'repos/trycua/cua/releases/tags/' + request['upstream_tag']]))
    require(release['tag_name'] == request['upstream_tag'] and release['draft'] is False and bool(release.get('published_at')), 'not a published upstream release')
    git(repo, 'fetch', '--no-tags', 'https://github.com/trycua/cua.git', 'refs/tags/' + request['upstream_tag'])
    require(text(repo, 'rev-parse', 'FETCH_HEAD^{commit}') == request['upstream_oid'], 'upstream tag OID mismatch')
    local = 'refs/tags/' + request['upstream_tag']
    existing = subprocess.run(['git', 'rev-parse', '--verify', local], cwd=repo, capture_output=True)
    if existing.returncode == 0:
        require(text(repo, 'rev-parse', local + '^{commit}') == request['upstream_oid'], 'moved upstream tag')
    else:
        git(repo, 'update-ref', local, request['upstream_oid'], '0' * 40)
    for oid in (policy['carry_base'], policy['carry_head']):
        git(repo, 'fetch', '--no-tags', 'https://github.com/cresslank/cua.git', oid)
    Path(output).write_bytes(canonical(release) + b'\n')

def construction(repo, request, release, output):
    validate_request(request)
    require(release.get('tag_name') == request['upstream_tag'] and release.get('draft') is False and bool(release.get('published_at')), 'not a published release')
    policy_oid = request['policy_oid']
    policy = policy_at(repo, policy_oid)
    require(carry_digest(policy) == request['carry_digest'], 'carry definition mismatch')
    require(set(policy['overlay_files']) == ALLOWED_OVERLAY and len(policy['overlay_files']) == len(ALLOWED_OVERLAY), 'overlay allowlist changed')
    require(text(repo, 'rev-parse', 'refs/tags/' + request['upstream_tag'] + '^{commit}') == request['upstream_oid'], 'wrong or moved release tag')
    base, head = policy['carry_base'], policy['carry_head']
    originals = text(repo, 'rev-list', '--reverse', '--ancestry-path', f'{base}..{head}').splitlines()
    require(originals == [c['oid'] for c in policy['carry']] and bool(originals), 'missing or extra original carry')
    require(text(repo, 'rev-list', '--count', f'{base}..{head}') == str(len(originals)), 'nonlinear carry history')
    patches = []
    for entry in policy['carry']:
        oid = entry['oid']
        require(len(text(repo, 'rev-list', '--parents', '-n1', oid).split()) == 2, 'merge carry refused')
        patch = git(repo, 'show', '--format=', '--binary', '--no-ext-diff', '--no-textconv', oid)
        require(digest(patch) == entry['patch_sha256'], 'original carry patch changed')
        patches.append(patch)
    out = Path(output)
    out.mkdir(parents=True, exist_ok=True)
    overlay_blobs = {p: git(repo, 'show', f'{policy_oid}:{p}') for p in sorted(ALLOWED_OVERLAY)}
    overlay_digest = digest(canonical({p: digest(b) for p, b in overlay_blobs.items()}))
    with tempfile.TemporaryDirectory(prefix='kdesk-source-') as tmp:
        work = Path(tmp)
        git(work, 'init', '-q')
        git(work, 'fetch', '--no-tags', str(Path(repo).resolve()), base, head, request['upstream_oid'])
        git(work, 'checkout', '--detach', head)
        env = dict(os.environ, GIT_COMMITTER_NAME='kdesk source automation',
                   GIT_COMMITTER_EMAIL='9219265+cresslank@users.noreply.github.com',
                   GIT_AUTHOR_NAME='kdesk source automation',
                   GIT_AUTHOR_EMAIL='9219265+cresslank@users.noreply.github.com',
                   GIT_AUTHOR_DATE='2000-01-01T00:00:00Z', GIT_COMMITTER_DATE='2000-01-01T00:00:00Z',
                   GIT_CONFIG_NOSYSTEM='1', GIT_CONFIG_GLOBAL='/dev/null')
        try:
            git(work, 'rebase', '--no-autostash', '--reapply-cherry-picks', '--empty=stop',
                '--committer-date-is-author-date', '--onto', request['upstream_oid'], base, env=env)
            rebased = text(work, 'rev-list', '--reverse', request['upstream_oid'] + '..HEAD').splitlines()
            require(len(rebased) == len(originals), 'carry dropped or unexpectedly empty')
            mapping = []
            for original, new, patch in zip(originals, rebased, patches):
                author = text(repo, 'show', '-s', '--format=%an%n%ae%n%aI', original)
                require(author == text(work, 'show', '-s', '--format=%an%n%ae%n%aI', new), 'authorship changed')
                new_patch = git(work, 'show', '--format=', '--binary', '--no-ext-diff', '--no-textconv', new)
                patch_id = lambda b: run(['git', 'patch-id', '--stable'], data=b).decode().split()[0]
                mapping.append({'original': original, 'rebased': new, 'author': author,
                                'original_patch_sha256': digest(patch), 'rebased_patch_sha256': digest(new_patch),
                                'original_patch_id': patch_id(patch), 'rebased_patch_id': patch_id(new_patch),
                                'tree': text(work, 'rev-parse', new + '^{tree}')})
                (out / (original + '.original.patch')).write_bytes(patch)
                (out / (original + '.rebased.patch')).write_bytes(new_patch)
            # Import the original objects for range-diff without touching any source files.
            git(work, 'fetch', '--no-tags', str(Path(repo).resolve()), head)
            (out / 'range-diff.txt').write_bytes(git(work, 'range-diff', '--no-color', base + '..' + head, request['upstream_oid'] + '..' + rebased[-1]))
            for path, blob in overlay_blobs.items():
                require(not (work / path).exists(), 'overlay file already present: ' + path)
                (work / path).parent.mkdir(parents=True, exist_ok=True)
                (work / path).write_bytes(blob)
                git(work, 'add', '--', path)
            git(work, 'apply', '--check', data=overlay_blobs['.github/scripts/kdesk-cua-portal-input.patch'])
            git(work, 'apply', '--index', data=overlay_blobs['.github/scripts/kdesk-cua-portal-input.patch'])
            expected = ALLOWED_OVERLAY | {FEATURE_FILE}
            require(set(text(work, 'diff', '--cached', '--name-only').splitlines()) == expected, 'extra overlay edit')
            (out / 'overlay.patch').write_bytes(git(work, 'diff', '--cached', '--binary', '--no-ext-diff', '--no-textconv', '--no-renames'))
            tree = text(work, 'write-tree')
            message = f'build: pinned kdesk driver controls\n\nPolicy: {policy_oid}\nOverlay-SHA256: {overlay_digest}\n'
            source = git(work, 'commit-tree', tree, '-p', rebased[-1], data=message.encode(), env=env).decode().strip()
            git(work, 'update-ref', 'refs/heads/prepared', source)
            git(repo, 'fetch', '--no-tags', str(work), 'refs/heads/prepared')
        except Hold as error:
            diagnostic = {'schema': 1, 'status': 'owner-hold', 'request': request,
                          'message': str(error)[:32768],
                          'conflict_paths': text(work, 'diff', '--name-only', '--diff-filter=U').splitlines(),
                          'head': text(work, 'rev-parse', 'HEAD')}
            (out / 'hold.json').write_bytes(canonical(diagnostic) + b'\n')
            (out / 'conflict.diff').write_bytes(git(work, 'diff', '--no-ext-diff', '--no-textconv', '--no-color')[:65536])
            raise
    tag = source_tag(request)
    existing = subprocess.run(['git', 'rev-parse', '--verify', 'refs/tags/' + tag], cwd=repo, capture_output=True)
    if existing.returncode == 0:
        require(existing.stdout.decode().strip() == source, 'source tag collision or movement')
    else:
        git(repo, 'update-ref', 'refs/tags/' + tag, source, '0' * 40)
    receipt = {'schema': 1, 'request': request, 'source_tag': tag, 'source_sha': source,
               'source_tree': tree, 'carry': mapping, 'overlay_digest': overlay_digest,
               'overlay_files': {p: digest(b) for p, b in overlay_blobs.items()},
               'overlay_patch_sha256': digest((out / 'overlay.patch').read_bytes())}
    (out / 'source.json').write_bytes(canonical(receipt) + b'\n')
    (out / 'upstream-release.json').write_bytes(canonical(release) + b'\n')
    return receipt

def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('command', choices=['guard-prepare', 'guard-build', 'fetch', 'prepare', 'verify-source', 'tag-name'])
    p.add_argument('--repo', default='.')
    p.add_argument('--request', required=True, help='exact-schema JSON file')
    p.add_argument('--release', default='upstream-release.json')
    p.add_argument('--output', default='kdesk-evidence')
    args = p.parse_args()
    try:
        raw = Path(args.request).read_bytes()
        require(len(raw) <= 4096, 'request exceeds bounded schema')
        request = json.loads(raw)
        require(isinstance(request, dict), 'request must be an object')
        validate_request(request)
        if args.command.startswith('guard-'):
            guard(request, args.command.split('-')[1])
            result = {'status': 'accepted'}
        elif args.command == 'tag-name':
            result = {'source_tag': source_tag(request)}
        elif args.command == 'fetch':
            fetch(args.repo, request, args.release)
            result = {'status': 'fetched'}
        else:
            result = construction(args.repo, request, json.loads(Path(args.release).read_text()), args.output)
            if args.command == 'verify-source':
                require(result['source_sha'] == os.environ['GITHUB_SHA'] == text(args.repo, 'rev-parse', 'HEAD'), 'compiled source is not workflow source')
                require(not text(args.repo, 'status', '--porcelain', '--untracked-files=all'), 'dirty source')
        print(json.dumps(result, sort_keys=True))
    except (Hold, KeyError, ValueError, OSError, subprocess.TimeoutExpired) as error:
        Path(args.output).mkdir(parents=True, exist_ok=True)
        receipt = {'schema': 1, 'status': 'owner-hold', 'message': str(error)[:32768]}
        (Path(args.output) / 'failure.json').write_bytes(canonical(receipt) + b'\n')
        print(json.dumps(receipt), file=sys.stderr)
        return 78
    return 0

if __name__ == '__main__':
    sys.exit(main())
