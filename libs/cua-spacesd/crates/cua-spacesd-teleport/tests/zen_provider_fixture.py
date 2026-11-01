# SPDX-License-Identifier: FSL-1.1-MIT
# Copyright (c) 2026 Cua AI, Inc.

"""Host-only preparation of an explicitly approved retained synthetic Zen fixture.

No SQLite connection, browser, installed CLI/daemon, vault or owner root is used.
The receiver gets only bundles emitted by the real provider (or a typed subset
of those records), never payloads reconstructed from the old browser receipt.
"""
import hashlib
import json
import os
import pathlib
import shutil
import stat
import subprocess

LOCKS = {"lock", "parent.lock"}


CHECKS = {
    "real_http_logins", "real_saved_login", "two_real_account_tabs",
    "browser_persisted_cookies", "browser_persisted_localstorage",
    "real_sessionstore", "inspection_source_unchanged",
}
CLEANUP = {"browser_exited", "http_server_closed", "server_thread_stopped"}


def validate_receipt(receipt, profile):
    """Validate the complete closed-browser evidence before source inventory/copy.

    This is consistency checking of an explicitly approved receipt, not an
    authentication/signature mechanism or permission to discover other profiles.
    """
    try:
        caps = receipt["session"]["capabilities"]
        for field, keys in (("checks", CHECKS), ("cleanup", CLEANUP)):
            values = receipt[field]
            if type(values) is not dict or set(values) != keys or any(v is not True for v in values.values()):
                raise ValueError(field)
        if (receipt["profile"] != str(profile) or caps["moz:profile"] != str(profile)
                or type(receipt["browser_returncode"]) is not int or receipt["browser_returncode"] != 0
                or caps["browserName"] != "zen" or caps["platformName"] != "linux"
                or caps["moz:headless"] is not True or not caps["browserVersion"]
                or receipt["handshake"]["applicationType"] != "gecko"
                or type(receipt["handshake"]["marionetteProtocol"]) is not int
                or receipt["handshake"]["marionetteProtocol"] != 3):
            raise ValueError("source browser identity/exit")
        from urllib.parse import urlsplit
        origins, nonce = receipt["origins"], receipt["nonce"]
        if type(origins) is not list or len(origins) != 2 or type(nonce) is not str or not nonce:
            raise ValueError("origins/nonce")
        urls = [urlsplit(o) for o in origins]
        if ({u.hostname for u in urls} != {"localhost", "127.0.0.1"}
                or urls[0].port is None or urls[0].port != urls[1].port
                or any(o != "http://" + u.netloc or u.username or u.password for o, u in zip(origins, urls))):
            raise ValueError("loopback origins")
        account_urls = [o + "/account" for o in origins]
        states = receipt["page_states"]
        if ([s["url"] for s in states] != account_urls
                or any(s["storage"] != nonce or s["text"] != "Signed in\n" + nonce for s in states)
                or [t["url"] for t in receipt["chrome_tabs"]] != account_urls):
            raise ValueError("browser account readback")
        events = receipt["events"]
        hosts = {u.netloc for u in urls}
        if ({e["host"] for e in events if e["kind"] == "login-post" and e["credentials_correct"] is True} != hosts
                or {e["host"] for e in events if e["kind"] == "page-callback"
                    and e["query"].get("storage") == [nonce] and e["cookie"] == "session=" + nonce} != hosts):
            raise ValueError("HTTP evidence")
        logins = receipt["saved_login_service"]
        if (len(logins) != 1 or logins[0]["hostname"] != origins[0]
                or logins[0]["passwordCorrect"] is not True):
            raise ValueError("saved login readback")
    except (KeyError, TypeError, ValueError, AttributeError, IndexError) as error:
        raise RuntimeError("receipt does not attest a closed real synthetic Zen fixture") from error


def inventory(profile):
    result = {}
    for p in sorted(profile.rglob("*")):
        rel = str(p.relative_to(profile))
        mode = p.lstat().st_mode
        if stat.S_ISLNK(mode):
            result[rel] = {"symlink": os.readlink(p)}
        elif stat.S_ISREG(mode):
            result[rel] = {"sha256": hashlib.sha256(p.read_bytes()).hexdigest()}
        elif not stat.S_ISDIR(mode):
            raise RuntimeError("special file in synthetic profile: " + rel)
    return result


def prepare(args):
    profile = args.source_profile.absolute()
    scratch = pathlib.Path(os.environ["TMPDIR"]).resolve(strict=True)
    # Refuse before enumerating any source. This opt-in lane is scratch-only.
    if not profile.is_relative_to(scratch) or profile.is_symlink():
        raise RuntimeError("source must be an explicit non-symlink retained scratch fixture")
    profile = profile.resolve(strict=True)
    if not profile.is_relative_to(scratch):
        raise RuntimeError("source escaped scratch")
    receipt = json.loads(args.source_receipt.read_text())
    validate_receipt(receipt, profile)
    expected = json.loads(args.source_sha256.read_text())
    before = inventory(profile)
    if {k: v["sha256"] for k, v in before.items() if "sha256" in v} != expected:
        raise RuntimeError("retained fixture differs from its closed-profile hashes")
    home = args.evidence / "source-home"
    staged = home / ".zen/Profiles/retained.synthetic"
    staged.mkdir(parents=True, mode=0o700)
    # Preserve the cleanly exited fixture's ordinary .parentlock file: the
    # provider must check its POSIX lock, not misclassify a retained file as a
    # running browser. Omit only stale symlink markers from this new copy;
    # never delete/unlock an original source.
    omitted = []
    for rel, value in before.items():
        src, dst = profile / rel, staged / rel
        if rel in LOCKS:
            omitted.append(rel)
            continue
        if "symlink" in value:
            raise RuntimeError("non-lock symlink in retained fixture")
        dst.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        shutil.copyfile(src, dst)
        dst.chmod(0o600)
    (home / ".zen/profiles.ini").write_text("[Profile0]\nName=retained-synthetic\nIsRelative=1\nPath=Profiles/retained.synthetic\nDefault=1\n")
    (home / ".cua-synthetic-zen-fixture").write_text("offline synthetic format-boundary test only\n")
    staged_before = inventory(staged)
    output = args.evidence / "provider-bundles"
    env = {"PATH": "/usr/bin:/bin", "HOME": str(home), "TMPDIR": str(args.evidence),
           "CUA_ENV_TEST_SANDBOX": "1", "DO_NOT_TRACK": "1", "CUA_TELEMETRY": "0",
           "CUA_SPACESD_TELEMETRY": "0", "DBUS_SESSION_BUS_ADDRESS": "unix:path=/no-test-bus"}
    command = [str(args.exporter_bin.resolve(strict=True)), "--allow-synthetic-capture", str(home), str(staged), str(output)]
    completed = None
    try:
        completed = subprocess.run(command, env=env, capture_output=True, timeout=90)
        (args.evidence / "provider.stdout").write_bytes(completed.stdout)
        (args.evidence / "provider.stderr").write_bytes(completed.stderr)
    finally:
        unchanged = before == inventory(profile)
        stage_unchanged = staged_before == inventory(staged)
        (args.evidence / "source-integrity.json").write_text(json.dumps({
            "original": str(profile), "original_unchanged": unchanged,
            "staged_unchanged": stage_unchanged, "omitted_copy_only_locks": omitted,
            "source_hashes": before, "staged_hashes": staged_before,
            "exporter_sha256": hashlib.sha256(args.exporter_bin.read_bytes()).hexdigest(),
            "provider_exit": None if completed is None else completed.returncode,
        }, indent=2) + "\n")
        if not unchanged or not stage_unchanged:
            raise RuntimeError("provider modified original or staged closed source")
    assert completed is not None
    completed.check_returncode()
    summary = json.loads(completed.stdout)
    origins = receipt["origins"]
    if summary["tabs"] != [o + "/account" for o in origins]:
        raise RuntimeError("provider tabs differ from real source browser tabs")
    # Values here configure the local website oracle; they are NEVER used to
    # manufacture the transfer's cookies/storage/logins/tabs.
    config = {"nonce": receipt["nonce"], "origins": origins, "provider": summary,
              "source_capabilities": receipt["session"]["capabilities"]}
    (output / "fixture.json").write_text(json.dumps(config, indent=2) + "\n")
    return output
