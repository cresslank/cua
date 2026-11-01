#!/usr/bin/env python3
# SPDX-License-Identifier: FSL-1.1-MIT
# Copyright (c) 2026 Cua AI, Inc.

"""Opt-in, networkless disposable Docker test of the normalized receiver.

Not a Keyvault/polkit push test. The default lane uses a new real Gecko source;
--source-profile opts into actual retained Zen -> ZenProvider -> receiver.
No host homes, desktop, sockets, installed cua or daemon are mounted.
"""
import argparse
import base64
import json
import os
import pathlib
import signal
import subprocess
import sys
import uuid


def run(args, **kwargs):
    return subprocess.run(args, check=True, timeout=kwargs.pop("timeout", 60), **kwargs)


def main():
    os.umask(0o077)
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--allow-disposable-docker", action="store_true", required=True)
    p.add_argument("--image", required=True, help="already-present image ID, no pull/build")
    p.add_argument("--receiver-bin", required=True, type=pathlib.Path)
    p.add_argument("--evidence", required=True, type=pathlib.Path)
    p.add_argument("--exporter-bin", type=pathlib.Path)
    p.add_argument("--source-profile", type=pathlib.Path)
    p.add_argument("--source-receipt", type=pathlib.Path)
    p.add_argument("--source-sha256", type=pathlib.Path)
    p.add_argument("--require-saved-login", action="store_true")
    a = p.parse_args()
    provider_args = [a.exporter_bin, a.source_profile, a.source_receipt, a.source_sha256]
    if any(provider_args) and not all(provider_args):
        p.error("provider mode needs exporter-bin, source-profile, source-receipt and source-sha256")
    if a.require_saved_login and not all(provider_args):
        p.error("require-saved-login requires actual provider mode")
    a.evidence = a.evidence.absolute()
    if not a.image.startswith("sha256:") or len(a.image) != 71:
        p.error("use an exact locally present sha256 image ID")
    if a.evidence.exists():
        p.error("evidence path must not exist")
    a.evidence.mkdir(mode=0o700, parents=True)
    binary = a.receiver_bin.resolve(strict=True)
    image = json.loads(run(["docker", "image", "inspect", a.image], capture_output=True).stdout)[0]
    if image["Id"] != a.image:
        raise RuntimeError("image identity mismatch")
    token = uuid.uuid4().hex
    container_name = "cua-zen-test-" + token
    cid = None
    result = {"boundary": "normalized receiver; synthetic real Gecko source, not Zen-provider/Keyvault push", "image": a.image}
    def cancel(signum, _frame):
        # InterruptedError is swallowed by subprocess/selectors as EINTR;
        # use a non-OSError so cancellation reaches the disposal finally block.
        raise RuntimeError(f"cancelled by signal {signum}")
    for sig in (signal.SIGTERM, signal.SIGINT):
        signal.signal(sig, cancel)
    try:
        bundles = None
        if all(provider_args):
            from zen_provider_fixture import prepare
            bundles = prepare(a)
            config = json.loads((bundles / "fixture.json").read_text())
            config["require_saved_login"] = a.require_saved_login
            (bundles / "fixture.json").write_text(json.dumps(config, indent=2) + "\n")
            result["boundary"] = "actual retained Zen -> ZenProvider.capture_selected -> receiver -> Firefox; NO Keyvault/polkit approval"
        collision = subprocess.run(["docker", "inspect", container_name], capture_output=True, timeout=15)
        if collision.returncode == 0 or b"no such object" not in collision.stderr.lower():
            raise RuntimeError("owned name is not proven absent before allocation")
        cid = run(["docker", "create", "--name", container_name,
                   "--pull", "never", "--cidfile", str(a.evidence / "container.cid"),
                   "--label", "cua.zen-test=" + token, "--network", "none", "--read-only",
                   "--user", "1000:1000", "--memory", "3g", "--memory-swap", "3g", "--cpus", "4",
                   "--pids-limit", "512", "--cap-drop", "ALL", "--security-opt", "no-new-privileges",
                   "--tmpfs", "/tmp:rw,exec,nosuid,nodev,mode=1777,size=768m",
                   "--tmpfs", "/home/fixture:rw,nosuid,nodev,mode=0700,uid=1000,gid=1000,size=512m",
                   "--shm-size", "256m", "--entrypoint", "/bin/sleep", a.image, "600"], capture_output=True).stdout.decode().strip()
        run(["docker", "start", cid], capture_output=True)
        inspection = json.loads(run(["docker", "inspect", cid], capture_output=True).stdout)[0]
        assert inspection["Config"]["Labels"]["cua.zen-test"] == token
        assert inspection["HostConfig"]["NetworkMode"] == "none"
        assert not inspection["HostConfig"]["Binds"]
        assert not inspection["HostConfig"]["PortBindings"]
        assert not inspection["Config"]["Volumes"], "image-declared volumes are not allowed"
        assert all(m["Type"] == "tmpfs" for m in inspection["Mounts"])
        (a.evidence / "container.json").write_text(json.dumps(inspection, indent=2) + "\n")
        files = [(binary, "/tmp/zen-receiver", 0o755),
                 (pathlib.Path(__file__).with_name("zen_guest_browser.py"), "/tmp/zen_guest_browser.py", 0o600)]
        guest_script = "zen_guest_browser.py"
        if bundles:
            guest_script = "zen_provider_browser.py"
            files.append((pathlib.Path(__file__).with_name(guest_script), "/tmp/" + guest_script, 0o600))
            run(["docker", "exec", cid, "mkdir", "-m", "700", "/tmp/provider-bundles"])
            files.extend((f, "/tmp/provider-bundles/" + f.name, 0o600) for f in bundles.iterdir() if f.is_file())
        for source, target, mode in files:
            run(["docker", "exec", "-i", cid, "python3", "-c",
                 "import pathlib,sys; p=pathlib.Path(sys.argv[1]); p.write_bytes(sys.stdin.buffer.read()); p.chmod(int(sys.argv[2]))", target, str(mode)], input=source.read_bytes())
        with (a.evidence / "run.log").open("wb") as log:
            completed = subprocess.run(["docker", "exec", cid, "python3", "-B", "/tmp/" + guest_script], stdout=log, stderr=subprocess.STDOUT, timeout=300)
        evidence = json.loads(run(["docker", "exec", cid, "python3", "-c",
            "import pathlib,json,base64; p=pathlib.Path('/home/fixture/evidence'); print(json.dumps({f.name:base64.b64encode(f.read_bytes()).decode() for f in p.iterdir() if f.is_file()}))"], capture_output=True).stdout)
        for name, value in evidence.items():
            if pathlib.Path(name).name != name:
                raise RuntimeError("invalid evidence filename")
            (a.evidence / name).write_bytes(base64.b64decode(value, validate=True))
        result["guest_exit"] = completed.returncode
        if completed.returncode:
            raise RuntimeError("guest assertions failed; see run.log and result.json")
    except BaseException as e:
        result["failure"] = str(e)
        raise
    finally:
        # Ignore repeated cancellation ONLY during bounded exact-ID disposal.
        for sig in (signal.SIGTERM, signal.SIGINT):
            signal.signal(sig, signal.SIG_IGN)
        cleanup_failure = None
        # A signal may interrupt docker create after daemon allocation but before
        # its client returns. Recover only this run's exact labelled identity.
        if cid is None:
            probe = subprocess.run(["docker", "inspect", container_name], capture_output=True, timeout=15)
            if probe.returncode == 0:
                owned = json.loads(probe.stdout)[0]
                if owned["Config"]["Labels"].get("cua.zen-test") == token:
                    cid = owned["Id"]
                else:
                    cleanup_failure = "refused cleanup of mismatched ownership label"
            elif b"no such object" not in probe.stderr.lower():
                cleanup_failure = "could not resolve interrupted allocation"
        if cid:
            try:
                run(["docker", "rm", "-fv", cid], capture_output=True, timeout=40)
                absent = subprocess.run(["docker", "inspect", cid], capture_output=True, timeout=15)
                result["container_absent"] = absent.returncode != 0 and b"no such object" in absent.stderr.lower()
                if not result["container_absent"]:
                    cleanup_failure = "exact owned container absence not verified"
            except BaseException as e:
                cleanup_failure = str(e)
            result["container_id"] = cid
        if cleanup_failure:
            result["cleanup_failure"] = cleanup_failure
        (a.evidence / "cleanup.json").write_text(json.dumps(result, indent=2) + "\n")
        if cleanup_failure and "failure" not in result:
            raise RuntimeError(cleanup_failure)
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
