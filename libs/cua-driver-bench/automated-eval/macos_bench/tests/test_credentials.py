# SPDX-License-Identifier: FSL-1.1-MIT
# Copyright (c) 2026 Cua AI, Inc.
"""Credential contracts through actual child processes; fake files, no network."""

import json
import os
import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import claude_credentials as cred
import claude_driver


@pytest.fixture
def key_file(tmp_path, monkeypatch):
    monkeypatch.delenv(cred.TOKEN_ENV, raising=False)
    monkeypatch.delenv(cred.API_ENV, raising=False)
    monkeypatch.setenv("CDB_BENCH_DISPOSABLE", "1")
    key = tmp_path / "credential"
    key.write_text("sk-ant-synthetic-credential-canary\n")
    key.chmod(0o600)
    return key


def trial(tmp_path, argv, env=None):
    return claude_driver.run_claude(
        argv,
        env or {"PATH": "/usr/bin:/bin"},
        tmp_path,
        "test",
        set(),
        5,
        tmp_path / "stream.tsv",
        tmp_path / "stderr",
        tmp_path / "elicitation.jsonl",
        post_result_grace_s=0.1,
    )


@pytest.mark.parametrize(
    "mode,variable,fd_env",
    [
        ("api_key", cred.API_ENV, "CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR"),
        ("setup_token", cred.TOKEN_ENV, "CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR"),
    ],
)
@pytest.mark.parametrize("failure", [False, True])
def test_child_fd_private_profile_redaction_and_billing(
    tmp_path, key_file, monkeypatch, mode, variable, fd_env, failure
):
    monkeypatch.setenv(variable, str(key_file))
    # This child is the external credential consumer boundary. It deliberately
    # echoes the fake credential in all output paths, including failure output.
    script = tmp_path / "consumer.py"
    script.write_text("""import json, os, sys, pathlib, time
fdname = sys.argv[1]
assert fdname in os.environ
assert [k for k in os.environ if k.endswith('FILE_DESCRIPTOR')] == [fdname]
key = os.read(int(os.environ[fdname]), 65536).decode().strip()
assert key and key not in json.dumps(dict(os.environ)) and key not in repr(sys.argv)
assert sys.argv[sys.argv.index('--setting-sources') + 1] == ''
for k in ('ANTHROPIC_API_KEY','ANTHROPIC_AUTH_TOKEN','CLAUDE_CODE_OAUTH_TOKEN',
          'ANTHROPIC_PROFILE','CLAUDE_CODE_REMOTE'):
    assert k not in os.environ, k
roots = [os.environ[k] for k in ('HOME','CLAUDE_CONFIG_DIR','XDG_CONFIG_HOME','ANTHROPIC_CONFIG_DIR')]
assert not any(pathlib.Path(p).exists() and list(pathlib.Path(p).iterdir()) for p in roots)
print(json.dumps({'type':'control_request','request_id':'1','request':{'subtype':'elicitation','message':key}}), flush=True)
sys.stdin.readline()
sys.stderr.write(key[:9]); sys.stderr.flush(); time.sleep(.01)
sys.stderr.write(key[9:] + '\\n'); sys.stderr.flush()
print(json.dumps({'type':'result','is_error':sys.argv[2] == '1','result':key, 'roots':roots}), flush=True)
sys.exit(int(sys.argv[2]))
""")
    poisoned = {
        "PATH": "/usr/bin:/bin",
        "HOME": str(tmp_path / "login"),
        "ANTHROPIC_API_KEY": "ambient-must-not-win",
        "ANTHROPIC_AUTH_TOKEN": "ambient-must-not-win",
        "CLAUDE_CODE_OAUTH_TOKEN": "ambient-must-not-win",
        "CLAUDE_CONFIG_DIR": str(tmp_path / "login"),
        "ANTHROPIC_PROFILE": "saved",
        "CLAUDE_CODE_REMOTE": "1",
    }
    homes = []
    for _ in range(2):  # A consumed first fd must not starve the next spawn.
        result = trial(
            tmp_path,
            [
                sys.executable,
                str(script),
                fd_env,
                str(int(failure)),
                "--setting-sources",
                "project",
            ],
            poisoned,
        )
        assert result["returncode"] == int(failure)
        assert result["billing_mode"] == mode
        assert result["result_seen"]
        assert result["elicitations"][0]["message"] == "[REDACTED]"
        rows = (tmp_path / "stream.tsv").read_text().splitlines()
        last = json.loads(rows[-1].split("\t", 1)[1])
        homes.append(last["roots"][0])
        assert all(not Path(p).exists() for p in last["roots"])
        (tmp_path / "evidence.json").write_text(json.dumps(result))
        secret = key_file.read_bytes().strip()
        for path in tmp_path.rglob("*"):
            if path.is_file() and path != key_file:
                assert secret not in path.read_bytes(), path
    assert homes[0] != homes[1]
    assert (tmp_path / "stderr").read_text() == "[REDACTED]\n"


@pytest.mark.parametrize("variable", [cred.API_ENV, cred.TOKEN_ENV])
@pytest.mark.parametrize(
    "fault,error",
    [
        ("mode", cred.FILE_ERROR),
        ("symlink", cred.FILE_ERROR),
        ("absent", cred.FILE_ERROR),
        ("directory", cred.FILE_ERROR),
        ("empty", cred.VALUE_ERROR),
        ("multiline", cred.VALUE_ERROR),
        ("oversize", cred.VALUE_ERROR),
    ],
)
def test_unsafe_file_never_spawns(tmp_path, key_file, monkeypatch, variable, fault, error):
    selected = key_file
    if fault == "mode":
        key_file.chmod(0o640)
    elif fault == "symlink":
        selected = tmp_path / "link"
        selected.symlink_to(key_file)
    elif fault == "absent":
        selected = tmp_path / "missing"
    elif fault == "directory":
        selected = tmp_path
    else:
        key_file.write_bytes(
            {"empty": b" \n", "multiline": b"first\nsecond", "oversize": b"x" * 65537}[fault]
        )
    monkeypatch.setenv(variable, str(selected))
    sentinel = tmp_path / "spawned"
    with pytest.raises(ValueError) as caught:
        trial(tmp_path, [sys.executable, "-c", f"open({str(sentinel)!r},'w').close()"])
    assert str(caught.value) == error
    assert not sentinel.exists()


def test_exclusive_mode_and_required_file(tmp_path, key_file, monkeypatch):
    for variable in (cred.API_ENV, cred.TOKEN_ENV):
        monkeypatch.setenv(variable, str(key_file))
    with pytest.raises(ValueError) as caught:
        trial(tmp_path, ["/bin/true"])
    assert str(caught.value) == cred.BOTH_ERROR
    monkeypatch.delenv(cred.API_ENV)
    monkeypatch.delenv(cred.TOKEN_ENV)
    with pytest.raises(ValueError) as caught:
        trial(tmp_path, ["/bin/true"])
    assert str(caught.value) == cred.REQUIRED_ERROR


@pytest.mark.skipif(
    os.geteuid() != 0, reason="real wrong-owner check requires isolated root container"
)
@pytest.mark.parametrize("variable", [cred.API_ENV, cred.TOKEN_ENV])
def test_open_descriptor_owner_rechecked(tmp_path, key_file, monkeypatch, variable):
    monkeypatch.setenv(variable, str(key_file))
    os.chown(key_file, 12345, -1)
    try:
        with pytest.raises(ValueError) as caught:
            trial(tmp_path, ["/bin/true"])
        assert str(caught.value) == cred.FILE_ERROR
    finally:
        os.chown(key_file, os.getuid(), -1)


@pytest.mark.parametrize(
    "mode,variable,fd_env",
    [
        ("api_key", cred.API_ENV, "CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR"),
        ("setup_token", cred.TOKEN_ENV, "CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR"),
    ],
)
def test_quota_probe_uses_same_fd_boundary(tmp_path, key_file, monkeypatch, mode, variable, fd_env):
    import argparse
    import bench_core
    import run_bench as rb
    from types import SimpleNamespace

    monkeypatch.setenv(variable, str(key_file))
    binary = tmp_path / "fake-claude"
    binary.write_text(f"""#!{sys.executable}
import os,json,pathlib
name = {fd_env!r}
key = os.read(int(os.environ[name]),65536).decode().strip()
assert key and 'ANTHROPIC_API_KEY' not in os.environ
assert pathlib.Path(os.environ['HOME']).is_dir()
pathlib.Path('fd-read').write_text('ok')
print(json.dumps({{'type':'result','is_error':True,'result':'Invalid API key '+key}}))
""")
    binary.chmod(0o755)
    monkeypatch.setattr(rb.ca, "CLAUDE_BIN", binary)
    monkeypatch.setattr(rb.ca, "CWD_ROOT", tmp_path / "cwd")
    ctx = SimpleNamespace(
        args=argparse.Namespace(ledger_who="test"),
        quota=None,
        ledger=bench_core.Ledger(tmp_path / "ledger.jsonl"),
    )
    ok, message = rb.quota_probe(ctx)
    assert (tmp_path / "cwd/fd-read").read_text() == "ok"
    assert (ok, message) == (False, "no rate_limit_event in the stream; utilisation unknown")
    assert key_file.read_bytes().strip() not in (tmp_path / "ledger.jsonl").read_bytes()


@pytest.mark.parametrize("flag", ["--debug-file", "--settings", "--bare"])
def test_explicit_mode_refuses_auth_or_log_overrides(tmp_path, key_file, monkeypatch, flag):
    monkeypatch.setenv(cred.API_ENV, str(key_file))
    with pytest.raises(ValueError) as caught:
        trial(tmp_path, ["/bin/true", flag, "untrusted"])
    assert (
        str(caught.value)
        == "Explicit credential mode refuses debug files and auth-setting overrides"
    )
