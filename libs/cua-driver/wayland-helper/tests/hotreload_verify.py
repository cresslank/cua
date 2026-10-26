#!/usr/bin/env python3
"""Offline verifier for hotreload_private_lab.py receipts and staged bytes."""
import argparse
import hashlib
import json
from pathlib import Path
import sys


def digest(data):
    return hashlib.sha256(data).hexdigest()


def verify(output, expected_failure=None, error_contains=None):
    output = output.resolve(strict=True)
    rows = [json.loads(line) for line in (output / 'results.jsonl').read_text().splitlines() if line.strip()]
    starts = [row for row in rows if row['event'] == 'run_started']
    cleanup = [row for row in rows if row['event'] == 'cleanup_verified']
    failures = [row for row in rows if row['event'] == 'run_failed']
    successes = [row for row in rows if row['event'] == 'all_tests_passed']
    assert len(starts) == 1 and len(cleanup) == 1, 'missing/duplicate start or cleanup receipt'
    assert cleanup[0]['remaining_marked_pids'] == [] and cleanup[0]['matching_mounts'] == [], 'incomplete cleanup'
    root = Path(starts[0]['root'])
    assert output == root / 'out' and Path(starts[0]['evidence']) == output, 'receipt root mismatch'
    ext = root / 'data/gnome-shell/extensions/winrects@cua'
    source = json.loads((output / 'source-manifest.json').read_text())
    for name, sha in source['files'].items():
        if not name.startswith('impl/'):
            assert digest((ext / name).read_bytes()) == sha, f'stable bootstrap bytes changed: {name}'
    closures = [row for row in rows if row['event'] == 'closure_staged']
    for closure in closures:
        directory = ext / 'impl' / closure['digest']
        raw = (directory / 'manifest.json').read_bytes()
        assert digest(raw) == closure['digest'] == closure['manifest_sha256'], 'manifest digest mismatch'
        manifest = json.loads(raw)
        assert manifest == closure['manifest'], 'manifest receipt differs'
        assert {p.name for p in directory.iterdir()} == {'manifest.json'} | {f['path'] for f in manifest['files']}, 'closure inventory mismatch'
        for entry in manifest['files']:
            path = directory / entry['path']
            assert not path.is_symlink() and path.is_file(), 'non-regular closure member'
            data = path.read_bytes()
            assert len(data) == entry['size'] and digest(data) == entry['sha256'], 'closure member content mismatch'
    guards = [row for row in rows if row['event'] == 'dconf_guard_verified_before_settings']
    assert len(guards) == 1, 'private dconf was not verified before settings'
    env = json.loads((output / 'private-environment.json').read_text())
    for key in ('HOME', 'XDG_RUNTIME_DIR', 'XDG_CONFIG_HOME', 'XDG_DATA_HOME', 'XDG_CACHE_HOME', 'XDG_STATE_HOME', 'TMPDIR'):
        assert Path(env[key]).is_relative_to(root), f'environment escaped root: {key}'
        assert guards[0]['environment'][key] == env[key], f'dconf environment mismatch: {key}'
    assert env['DBUS_SESSION_BUS_ADDRESS'] == f'unix:path={root}/run/bus'
    assert env['DBUS_SYSTEM_BUS_ADDRESS'] == f'unix:path={root}/run/sysbus'
    assert 'DISPLAY' not in env and 'AT_SPI_BUS_ADDRESS' not in env, 'inherited desktop environment'
    passed_cells = [row['cell'] for row in rows if row['event'] == 'cell_passed']
    if expected_failure:
        assert not successes and len(failures) == 1, 'mutation did not fail exactly once'
        assert failures[0]['cell'] == expected_failure, f'mutation failed in unrelated cell: {failures}'
        assert error_contains and error_contains in failures[0]['error'], f'wrong behavioral mutation assertion: {failures}'
        assert any(row['event'] == 'scratch_only_mutation' for row in rows), 'missing copied-source mutation receipt'
        return dict(verdict='expected-behavioral-failure', cell=expected_failure,
                    error=failures[0]['error'], output=str(output), cleanup=True)
    assert not failures and len(successes) == 1, f'run did not pass: {failures}'
    expected = successes[0]['cases']
    assert passed_cells == expected and len(set(expected)) == len(expected), 'declared/executed cell inventory mismatch'
    ready = [row for row in rows if row['event'] == 'ready']
    epochs = [row['capabilities']['epoch'] for row in ready]
    assert len(set(epochs)) == len(epochs), 'reused real runtime epoch'
    shell_pids = {row['shell_pid'] for row in ready}
    assert shell_pids == {successes[0]['shell_pid']}, 'Shell restarted across reload'
    for row in ready:
        assert row['loader']['loaded_digest'] == row['loader']['selected_digest']
        assert row['loader']['epoch'] == row['capabilities']['epoch']
    if 'capture' in expected:
        hold = [row for row in rows if row['event'] == 'capture_retirement_rendezvous']
        refused = [row for row in rows if row['event'] == 'retired_capture_refused']
        assert len(hold) == len(refused) == 1, 'missing real capture rendezvous/refusal'
        assert hold[0]['observer']['capture']['nativeCompleted'] and refused[0]['native_png_bytes'] > 0
        assert hold[0]['raw_input_held'], 'retirement did not reserve the canonical raw-input lease'
    if 'foreground' in expected:
        busy = [row for row in rows if row['event'] == 'maintenance_busy_refused']
        assert [row['kind'] for row in busy] == ['foreground', 'raw-input'], 'missing idle-only refusal boundary'
        assert all('org.cua.WinRects.Loader.Busy' in row['error'] for row in busy)
        assert busy[0]['epoch'] == busy[1]['epoch'], 'Busy refusal changed helper epoch'
        assert not any(row['event'] == 'open_foreground_retired' for row in rows), 'obsolete foreground-abort swap claim'
    for case, event in [('timeout', 'retirement_timeout_poison_verified'), ('uncertain', 'uncertain_retirement_quarantined')]:
        if case in expected:
            poison = [row for row in rows if row['event'] == event]
            assert len(poison) == 1 and poison[0]['raw_input_held'] and poison[0]['loader']['quarantined']
            assert poison[0]['loader']['state'] == 'error', 'uncertain retirement was certified ready'
    memory = [row for row in rows if row['event'] == 'real_closure_memory_summary']
    if 'memory' in expected:
        assert len(memory) == 1
        samples = memory[0]['samples']
        assert len(samples) == len({s['digest'] for s in samples}) == memory[0]['distinct_real_closures'] == 20
        assert [s['index'] for s in samples] == list(range(1, 21))
        assert memory[0]['rss_delta_kib'] == memory[0]['final']['VmRSS'] - memory[0]['baseline']['VmRSS']
    return dict(verdict='pass', cells=passed_cells, real_epochs=len(epochs), shell_pid=next(iter(shell_pids)),
                distinct_real_closures=memory[0]['distinct_real_closures'] if memory else None,
                rss_delta_kib=memory[0]['rss_delta_kib'] if memory else None, cleanup=True, output=str(output))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('output', type=Path)
    parser.add_argument('--expect-failure')
    parser.add_argument('--error-contains')
    args = parser.parse_args()
    result = verify(args.output, args.expect_failure, args.error_contains)
    print(json.dumps(result, indent=2, sort_keys=True))
    return 0


if __name__ == '__main__':
    sys.exit(main())
