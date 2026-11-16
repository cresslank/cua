"""Offline CLI/Git and release-data tests. No Cargo or live runner required."""
import copy
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tarfile

import pytest
import yaml

SCRIPTS = Path(__file__).resolve().parents[1]
ROOT = SCRIPTS.parents[1]
CLI = SCRIPTS / 'kdesk-cua-driver.py'
PKG = SCRIPTS / 'kdesk-cua-package.py'
FEATURE = 'libs/cua-driver/rust/crates/platform-linux/src/tools/get_config.rs'
POLICY = '.github/kdesk-cua-driver-policy.json'

def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(',', ':')).encode()

def digest(data):
    return hashlib.sha256(data).hexdigest()

def git(repo, *args, data=None, author='Contributor <123+contributor@users.noreply.github.com>'):
    env = dict(os.environ, GIT_AUTHOR_NAME=author.split(' <')[0], GIT_AUTHOR_EMAIL=author.split('<')[1][:-1],
               GIT_COMMITTER_NAME='Fixture', GIT_COMMITTER_EMAIL='fixture@example.org',
               GIT_AUTHOR_DATE='2026-01-01T00:00:00Z', GIT_COMMITTER_DATE='2026-01-01T00:00:00Z',
               GIT_CONFIG_NOSYSTEM='1', GIT_CONFIG_GLOBAL='/dev/null')
    return subprocess.check_output(['git', '-c', 'core.hooksPath=/dev/null', '-c', 'commit.gpgSign=false', *args], cwd=repo, input=data, env=env, stderr=subprocess.PIPE).decode().strip()

def commit(repo, message):
    git(repo, 'add', '.')
    git(repo, 'commit', '-qm', message)
    return git(repo, 'rev-parse', 'HEAD')

def put(repo, path, content):
    p = repo / path
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(content)

@pytest.fixture
def repository(tmp_path):
    repo = tmp_path / 'repo'
    repo.mkdir()
    git(repo, 'init', '-q')
    put(repo, FEATURE, '        ToolResult::text("cua-driver-rs configuration").with_structured(json!({\n            "version": env!("CARGO_PKG_VERSION"),\n            "source_sha": option_env!("CUA_DRIVER_SOURCE_SHA"),\n            "platform": "linux",\n            "capture_mode": cfg.capture_mode,\n            "max_image_dimension": cfg.max_image_dimension,\n        }))\n')
    put(repo, 'runtime.txt', 'line1\nline2\noriginal\n')
    base = commit(repo, 'base')
    put(repo, 'runtime.txt', 'line1\nline2\ncarried\n')
    head = commit(repo, 'runtime carry\n\nCo-authored-by: Other <other@example.org>')
    patch = subprocess.check_output(['git', 'show', '--format=', '--binary', head], cwd=repo)
    git(repo, 'checkout', '--detach', base)
    put(repo, 'release.txt', 'release not main\n')
    upstream = commit(repo, 'published release')
    tag = 'cua-driver-rs-v1.2.3'
    git(repo, 'tag', tag)
    put(repo, 'main-only.txt', 'must never be carried\n')
    commit(repo, 'upstream main after release')
    policy = json.loads((ROOT / POLICY).read_text())
    policy.update(carry_base=base, carry_head=head, carry=[{'oid': head, 'patch_sha256': digest(patch)}])
    for path in policy['overlay_files']:
        put(repo, path, (ROOT / path).read_text())
    put(repo, POLICY, json.dumps(policy))
    control = commit(repo, 'reviewed control policy')
    request = {'request_key': 'fixture-request-0001', 'upstream_tag': tag, 'upstream_oid': upstream,
               'policy_oid': control, 'carry_digest': digest(canonical({k: policy[k] for k in ('carry_base', 'carry_head', 'carry')})),
               'build_image_digest': 'sha256:' + '1' * 64}
    reqfile = tmp_path / 'request.json'
    release = tmp_path / 'release.json'
    reqfile.write_text(json.dumps(request))
    release.write_text(json.dumps({'tag_name': tag, 'draft': False, 'published_at': '2026-01-01T00:00:00Z'}))
    return {'repo': repo, 'base': base, 'head': head, 'upstream': upstream, 'request': request,
            'reqfile': reqfile, 'release': release, 'out': tmp_path / 'evidence', 'policy': policy}

def invoke(f, command='prepare', request=None, env=None, output=None):
    f['reqfile'].write_text(json.dumps(f['request'] if request is None else request))
    return subprocess.run([sys.executable, str(CLI), command, '--repo', str(f['repo']),
                           '--request', str(f['reqfile']), '--release', str(f['release']),
                           '--output', str(output or f['out'])], env=env, text=True, capture_output=True)

def prepared(f):
    result = invoke(f)
    assert result.returncode == 0, result.stderr
    return json.loads(result.stdout)

def test_release_not_main_authorship_mapping_and_exact_overlay(repository):
    f = repository
    receipt = prepared(f)
    source = receipt['source_sha']
    assert git(f['repo'], 'show', source + ':release.txt') == 'release not main'
    assert subprocess.run(['git', 'cat-file', '-e', source + ':main-only.txt'], cwd=f['repo'], capture_output=True).returncode != 0
    assert git(f['repo'], 'show', source + ':runtime.txt') == 'line1\nline2\ncarried'
    carried = receipt['carry'][0]
    assert git(f['repo'], 'rev-parse', carried['rebased'] + '^') == f['upstream']
    assert git(f['repo'], 'show', '-s', '--format=%an%n%ae%n%aI%n%B', carried['rebased']) == git(f['repo'], 'show', '-s', '--format=%an%n%ae%n%aI%n%B', f['head'])
    assert carried['original_patch_id'] == carried['rebased_patch_id']
    assert (f['out'] / 'range-diff.txt').read_text().strip()
    assert set(git(f['repo'], 'diff-tree', '--no-commit-id', '--name-only', '-r', source).splitlines()) == set(f['policy']['overlay_files']) | {FEATURE}
    for path in f['policy']['overlay_files']:
        assert git(f['repo'], 'rev-parse', source + ':' + path) == git(f['repo'], 'rev-parse', f['request']['policy_oid'] + ':' + path)
    assert '"portal_input": cfg!(feature = "portal-input")' in git(f['repo'], 'show', source + ':' + FEATURE)

def test_deterministic_repeated_construction(repository):
    f = repository
    first = prepared(f)
    second = prepared(f)
    assert first == second
    assert git(f['repo'], 'rev-parse', 'refs/tags/' + first['source_tag']) == first['source_sha']

def test_verified_workflow_sha_and_untracked_edit_refusal(repository):
    f = repository
    receipt = prepared(f)
    git(f['repo'], 'checkout', '--detach', receipt['source_sha'])
    env = dict(os.environ, GITHUB_SHA=receipt['source_sha'])
    result = invoke(f, 'verify-source', env=env)
    assert result.returncode == 0, result.stderr
    put(f['repo'], 'unrecorded-source.rs', 'extra compiled input\n')
    result = invoke(f, 'verify-source', env=env)
    assert result.returncode == 78 and 'dirty source' in result.stderr


def test_context_changed_patch_evidence(repository):
    f = repository
    git(f['repo'], 'checkout', '--detach', f['upstream'])
    put(f['repo'], 'runtime.txt', 'context\nline1\nline2\noriginal\n')
    upstream = commit(f['repo'], 'changed patch context')
    git(f['repo'], 'tag', '-f', f['request']['upstream_tag'], upstream)
    f['request']['upstream_oid'] = upstream
    receipt = prepared(f)
    carry = receipt['carry'][0]
    assert carry['original_patch_sha256'] != carry['rebased_patch_sha256']
    assert 'context' in (f['out'] / (f['head'] + '.rebased.patch')).read_text()

@pytest.mark.parametrize('failure', ['conflict', 'empty', 'overlay-conflict', 'overlay-existing'])
def test_conflict_empty_and_overlay_holds(repository, failure):
    f = repository
    git(f['repo'], 'checkout', '--detach', f['upstream'])
    if failure in ('conflict', 'empty'):
        put(f['repo'], 'runtime.txt', 'conflicting\n' if failure == 'conflict' else 'line1\nline2\ncarried\n')
    elif failure == 'overlay-existing':
        put(f['repo'], '.github/scripts/kdesk-cua-driver.py', 'upstream-owned\n')
    else:
        put(f['repo'], FEATURE, 'incompatible new config implementation\n')
    upstream = commit(f['repo'], failure)
    git(f['repo'], 'tag', '-f', f['request']['upstream_tag'], upstream)
    f['request']['upstream_oid'] = upstream
    result = invoke(f)
    assert result.returncode == 78
    assert (f['out'] / 'hold.json').is_file()
    assert len((f['out'] / 'conflict.diff').read_bytes()) <= 65536
    assert not list((f['out']).glob('source.json'))

@pytest.mark.parametrize('failure', ['wrong-oid', 'moved-tag', 'missing-carry', 'changed-carry-definition', 'tag-collision', 'extra-source-edit'])
def test_identity_refusals(repository, failure):
    f = repository
    if failure == 'wrong-oid':
        f['request']['upstream_oid'] = f['base']
    elif failure == 'moved-tag':
        git(f['repo'], 'tag', '-f', f['request']['upstream_tag'], f['base'])
    elif failure == 'missing-carry':
        # A real absent original object, not a mocked rev-list response.
        policy = copy.deepcopy(f['policy'])
        policy['carry_head'] = 'f' * 40
        put(f['repo'], POLICY, json.dumps(policy))
        f['request']['policy_oid'] = commit(f['repo'], 'missing recorded original')
        f['request']['carry_digest'] = digest(canonical({k: policy[k] for k in ('carry_base', 'carry_head', 'carry')}))
    elif failure == 'changed-carry-definition':
        f['request']['carry_digest'] = 'f' * 64
    else:
        receipt = prepared(f)
        if failure == 'tag-collision':
            git(f['repo'], 'update-ref', 'refs/tags/' + receipt['source_tag'], f['base'])
        else:
            git(f['repo'], 'checkout', '--detach', receipt['source_sha'])
            put(f['repo'], 'extra.txt', 'unrecorded edit\n')
            extra = commit(f['repo'], 'extra compiled source')
            result = invoke(f, 'verify-source', env=dict(os.environ, GITHUB_SHA=extra))
            assert result.returncode == 78 and 'workflow source' in result.stderr
            return
    assert invoke(f).returncode == 78

def environment(f, phase='prepare'):
    request = f['request']
    tag = 'kdesk-' + request['upstream_tag'] + '-' + request['carry_digest'] + '-' + request['policy_oid'] + '-' + request['upstream_oid']
    return dict(os.environ, GITHUB_REPOSITORY='cresslank/cua', GITHUB_ACTOR='cresslank', GITHUB_TRIGGERING_ACTOR='cresslank',
                GITHUB_EVENT_NAME='workflow_dispatch', GITHUB_REF='refs/heads/main' if phase == 'prepare' else 'refs/tags/' + tag,
                GITHUB_SHA=request['policy_oid'], PIPELINE_ENABLED='true', CI_RUNNER='private-vm', BUILD_IMAGE_DIGEST=request['build_image_digest'])

@pytest.mark.parametrize('phase', ['prepare', 'build'])
def test_owner_dispatch_accepts(repository, phase):
    assert invoke(repository, 'guard-' + phase, env=environment(repository, phase)).returncode == 0

@pytest.mark.parametrize('phase', ['prepare', 'build'])
@pytest.mark.parametrize('key,value', [('GITHUB_REPOSITORY', 'trycua/cua'), ('GITHUB_ACTOR', 'outsider'),
    ('GITHUB_TRIGGERING_ACTOR', 'outsider'), ('GITHUB_EVENT_NAME', 'pull_request_target'),
    ('GITHUB_EVENT_NAME', 'push'), ('GITHUB_REF', 'refs/heads/other'), ('PIPELINE_ENABLED', ''),
    ('CI_RUNNER', 'ubuntu-latest'), ('BUILD_IMAGE_DIGEST', 'sha256:' + '2' * 64)])
def test_actor_rerun_ref_event_activation_guards(repository, phase, key, value):
    env = environment(repository, phase)
    env[key] = value
    assert invoke(repository, 'guard-' + phase, env=env).returncode == 78

@pytest.mark.parametrize('key,value', [('upstream_tag', 'nightly'), ('upstream_tag', 'cua-driver-rs-v01.2.3'),
    ('upstream_oid', 'HEAD'), ('policy_oid', 'main'), ('carry_digest', 'short'),
    ('build_image_digest', 'latest'), ('request_key', 'a; echo injected'), ('unknown', 'field')])
def test_exact_input_schema(repository, key, value):
    request = dict(repository['request'], **{key: value})
    assert invoke(repository, request=request).returncode == 78

@pytest.mark.parametrize('field,value', [('draft', True), ('tag_name', 'other'), ('published_at', None)])
def test_release_requires_published_component(repository, field, value):
    release = json.loads(repository['release'].read_text())
    release[field] = value
    repository['release'].write_text(json.dumps(release))
    assert invoke(repository).returncode == 78

def load_module(path, name):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module

@pytest.fixture
def package(tmp_path, monkeypatch):
    source = tmp_path / 'source'
    stage = tmp_path / 'stage'
    output = tmp_path / 'package'
    output.mkdir()
    for path, content in [('libs/cua-driver/rust/Skills/cua-driver/SKILL.md', 'skill'),
                          ('libs/cua-driver/wayland-helper/winrects@cua/extension.js', 'helper')]:
        put(source, path, content)
    for path, content in [('cua-driver-local', 'ELF-fixture-driver'), ('cua-cursor-theme', 'ELF-fixture-theme'),
                          ('Skills/cua-driver/SKILL.md', 'skill'), ('wayland-helper/winrects@cua/extension.js', 'helper')]:
        put(stage, path, content)
    (stage / 'cua-driver-local').chmod(0o755)
    (stage / 'cua-cursor-theme').chmod(0o755)
    mod = load_module(PKG, 'kdesk_package')
    sha = 'a' * 40
    receipt = {'source_sha': sha, 'source_tree': 'b' * 40, 'source_tag': 'kdesk-fixture', 'request': {'build_image_digest': 'sha256:' + '1' * 64, 'policy_oid': 'c' * 40}}
    for name in mod.EVIDENCE:
        (output / name).write_text('evidence\n')
    (output / 'source.json').write_text(json.dumps(receipt))
    (output / 'smoke.json').write_text(json.dumps({'status': 'passed', 'source_sha': sha, 'portal_input': True, 'binary_sha256': digest((stage / 'cua-driver-local').read_bytes())}))
    for key, value in {'GITHUB_REPOSITORY': 'cresslank/cua', 'GITHUB_RUN_ID': '1234', 'GITHUB_RUN_ATTEMPT': '1',
                       'GITHUB_WORKFLOW_REF': 'cresslank/cua/.github/workflows/kdesk-cua-driver-build.yml@refs/tags/kdesk-fixture',
                       'GITHUB_WORKFLOW_SHA': sha, 'GITHUB_SHA': sha, 'GITHUB_REF': 'refs/tags/kdesk-fixture',
                       'GITHUB_ACTOR': 'cresslank', 'GITHUB_TRIGGERING_ACTOR': 'cresslank', 'GITHUB_EVENT_NAME': 'workflow_dispatch'}.items():
        monkeypatch.setenv(key, value)
    def assemble():
        return subprocess.run([sys.executable, str(PKG), 'assemble', '--repo', str(source), '--stage', str(stage), '--output', str(output), '--source-sha', sha, '--toolchain', '1.97.1'], capture_output=True, text=True)
    return mod, stage, output, sha, assemble

def test_complete_installer_roster_archive_and_manifest(package):
    mod, stage, output, sha, assemble = package
    result = assemble()
    assert result.returncode == 0, result.stderr
    manifest = mod.validate(output, sha)
    assert {e['path'] for e in manifest['members']} == {p.relative_to(stage).as_posix() for p in stage.rglob('*')}
    assert manifest['binary_sha256']['cua-driver-local'] == digest((stage / 'cua-driver-local').read_bytes())

@pytest.mark.parametrize('failure', ['missing-skill', 'missing-theme', 'extra-member', 'symlink', 'source-mismatch', 'feature-lost'])
def test_assembly_refuses_incomplete_or_unsafe_installer(package, failure):
    mod, stage, output, sha, assemble = package
    if failure == 'missing-skill':
        (stage / 'Skills/cua-driver/SKILL.md').unlink()
    elif failure == 'missing-theme':
        (stage / 'cua-cursor-theme').unlink()
    elif failure == 'extra-member':
        put(stage, 'unexpected', 'extra')
    elif failure == 'symlink':
        (stage / 'link').symlink_to('/etc/passwd')
    else:
        smoke = json.loads((output / 'smoke.json').read_text())
        smoke['source_sha' if failure == 'source-mismatch' else 'portal_input'] = 'd' * 40 if failure == 'source-mismatch' else False
        (output / 'smoke.json').write_text(json.dumps(smoke))
    assert assemble().returncode == 78

@pytest.mark.parametrize('failure', ['mutated', 'duplicate-checksum', 'missing-checksum', 'unsafe-archive', 'duplicate-member', 'missing-member', 'source-mismatch'])
def test_release_static_admission_refuses(package, failure):
    mod, stage, output, sha, assemble = package
    assert assemble().returncode == 0
    if failure == 'mutated':
        (output / 'range-diff.txt').write_text('mutated')
    elif failure == 'duplicate-checksum':
        sums = output / 'SHA256SUMS'
        sums.write_text(sums.read_text() + sums.read_text().splitlines()[0] + '\n')
    elif failure == 'missing-checksum':
        sums = output / 'SHA256SUMS'
        sums.write_text('\n'.join(sums.read_text().splitlines()[1:]) + '\n')
    elif failure == 'source-mismatch':
        sha = 'd' * 40
    else:
        archive_path = output / mod.ARCHIVE
        with tarfile.open(archive_path) as archive:
            members = [(m, archive.extractfile(m).read() if m.isfile() else None) for m in archive]
        if failure == 'missing-member':
            members.pop()
        if failure == 'duplicate-member':
            members.append(members[-1])
        if failure == 'unsafe-archive':
            info = tarfile.TarInfo('../escape')
            info.size = 1
            members.append((info, b'x'))
        with tarfile.open(archive_path, 'w:gz') as archive:
            for member, data in members:
                archive.addfile(member, io.BytesIO(data) if data is not None else None)
    with pytest.raises(ValueError):
        mod.validate(output, sha)

def test_no_clobber_rerun_asset_identity(package):
    mod, stage, output, sha, assemble = package
    assert assemble().returncode == 0
    names = {p.name for p in output.iterdir()}
    release = {'assets': [{'name': n, 'size': (output / n).stat().st_size, 'digest': 'sha256:' + digest((output / n).read_bytes())} for n in names]}
    mod.release_equal(release, output, names)
    for failure in ('missing', 'duplicate', 'mutated'):
        broken = copy.deepcopy(release)
        if failure == 'missing':
            broken['assets'].pop()
        elif failure == 'duplicate':
            broken['assets'].append(broken['assets'][0])
        else:
            broken['assets'][0]['digest'] = 'sha256:' + '0' * 64
        with pytest.raises(ValueError):
            mod.release_equal(broken, output, names)

@pytest.mark.parametrize('name', ['prepare', 'build'])
def test_workflow_private_authority_and_pins(name):
    path = ROOT / f'.github/workflows/kdesk-cua-driver-{name}.yml'
    raw = path.read_text()
    workflow = yaml.safe_load(raw)
    assert set(workflow.get('on', workflow.get(True))) == {'workflow_dispatch'}
    assert workflow['concurrency']['cancel-in-progress'] is False
    for job in workflow['jobs'].values():
        assert job['runs-on'] == 'private-vm'
        for guard in ("github.repository == 'cresslank/cua'", "github.actor == 'cresslank'", "github.triggering_actor == 'cresslank'", "vars.KDESK_DRIVER_PIPELINE_ENABLED == 'true'", "vars.CI_RUNNER == 'private-vm'"):
            assert guard in job['if']
        for step in job['steps']:
            if 'uses' in step:
                assert re.fullmatch(r'[^@]+@[0-9a-f]{40}', step['uses'])
                if step['uses'].startswith('actions/checkout@'):
                    assert step['with']['persist-credentials'] is False
    if name == 'build':
        assert workflow['jobs']['build']['permissions'] == {'contents': 'read'}
        assert workflow['jobs']['publish']['permissions'] == {'contents': 'write', 'actions': 'read', 'id-token': 'write', 'attestations': 'write'}
        assert '--clobber' not in (SCRIPTS / 'kdesk-cua-package.py').read_text()
        assert workflow['jobs']['build']['steps'][0]['with'].get('ref') is None
        assert workflow['jobs']['publish']['steps'][0]['with']['ref'] == '${{ fromJSON(inputs.request).policy_oid }}'

def test_policy_matches_additive_feature_patch():
    policy = json.loads((ROOT / POLICY).read_text())
    assert policy['carry_head'] == '77e7659a110f1928a1cdb0f9cc40ef92832526a6'
    assert policy['features'] == ['cua-driver/portal-input']
    main_feature = ROOT / 'libs/cua-driver/rust/crates/platform-linux/src/tools/impl_.rs'
    assert '"build_features": { "portal_input": cfg!(feature = "portal-input") },' in main_feature.read_text()
    wrapper = (SCRIPTS / 'kdesk-cua-build.sh').read_text()
    assert '--locked --features cua-driver/portal-input' in wrapper
    assert 'env -i' in wrapper and 'install-local.sh --release' in wrapper
