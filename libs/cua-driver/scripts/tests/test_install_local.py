from __future__ import annotations

import fcntl
import hashlib
import json
import os
import shutil
import stat
import subprocess
from pathlib import Path

import pytest

INSTALL_LOCAL = Path(__file__).resolve().parents[1] / "_install-local-rust.sh"
LOCAL_SIGNING = INSTALL_LOCAL.with_name("_local-signing.sh")
TRANSACTION_LOCK = INSTALL_LOCAL.with_name("_install-transaction-lock.py")
DISPATCHER = INSTALL_LOCAL.with_name("install-local.sh")


def _write_executable(path: Path, body: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(f"#!/bin/sh\n{body}", encoding="utf-8")
    path.chmod(0o755)


def test_installer_keeps_content_addressed_atomic_promotion_without_global_kills() -> None:
    installer = INSTALL_LOCAL.read_text(encoding="utf-8")

    assert 'FINAL_VERSIONED_DIR="$RELEASES_DIR/$VERSION_TAG-v4-' in installer
    assert 'mv "$VERSIONED_DIR" "$FINAL_VERSIONED_DIR"' in installer
    assert 'mv -f "$STAGED_BINARY_TMP" "$STAGED_BINARY"' in installer
    assert "pkill" not in installer
    assert "killall" not in installer


def _copy_installer_fixture(
    scripts_dir: Path, lock_path: Path, host_os: str | None = None
) -> None:
    installer = scripts_dir / INSTALL_LOCAL.name
    lock_helper = scripts_dir / TRANSACTION_LOCK.name
    shutil.copy2(INSTALL_LOCAL, installer)
    shutil.copy2(TRANSACTION_LOCK, lock_helper)
    shutil.copy2(LOCAL_SIGNING, scripts_dir / LOCAL_SIGNING.name)
    shutil.copy2(DISPATCHER, scripts_dir / DISPATCHER.name)

    lock_text = lock_helper.read_text(encoding="utf-8")
    old_lock = 'DEFAULT_LOCK = f"/tmp/cua-driver-local-install-transaction-{os.geteuid()}.lock"'
    lock_text = lock_text.replace(old_lock, f"DEFAULT_LOCK = {str(lock_path)!r}")
    assert old_lock not in lock_text
    lock_helper.write_text(lock_text, encoding="utf-8")

    if host_os is not None:
        installer_text = installer.read_text(encoding="utf-8")
        old_platform = 'OS="$("$UNAME_BIN" -s)"\nARCH="$("$UNAME_BIN" -m)"'
        new_platform = f"OS={host_os!r}\nARCH='x86_64'"
        installer_text = installer_text.replace(old_platform, new_platform)
        assert old_platform not in installer_text
        installer.write_text(installer_text, encoding="utf-8")


def _install_fake_rust_toolchain(fake_bin: Path, tmp_path: Path) -> None:
    toolchain_bin = tmp_path / "fake-toolchain/bin"
    toolchain_bin.mkdir(parents=True, exist_ok=True)
    shutil.copy2(fake_bin / "cargo", toolchain_bin / "cargo")
    _write_executable(toolchain_bin / "rustc", "exit 0")
    _write_executable(toolchain_bin / "rustdoc", "exit 0")
    _write_executable(
        fake_bin / "rustup",
        f'test "$*" = "which cargo"\nprintf "%s\\n" {toolchain_bin / "cargo"!s}\n',
    )


@pytest.mark.parametrize("relative_target", [False, True], ids=["absolute", "relative"])
@pytest.mark.parametrize(
    "mutate_source_during_fetch", [False, True], ids=["honest-fetch", "mutating-fetch"]
)
@pytest.mark.parametrize(
    ("embedded_source_sha", "expect_success"),
    [("a" * 40, True), ("b" * 40, False)],
    ids=["matching-source", "mismatched-source"],
)
def test_installer_stages_binary_from_custom_cargo_target(
    tmp_path: Path,
    relative_target: bool,
    mutate_source_during_fetch: bool,
    embedded_source_sha: str,
    expect_success: bool,
) -> None:
    fixture_root = tmp_path / "cua-driver"
    scripts_dir = fixture_root / "scripts"
    rust_dir = fixture_root / "rust"
    scripts_dir.mkdir(parents=True)
    rust_dir.mkdir()
    _copy_installer_fixture(scripts_dir, tmp_path / "install-transaction.lock")

    skill_file = rust_dir / "Skills/cua-driver/SKILL.md"
    skill_file.parent.mkdir(parents=True)
    skill_file.write_text("initial skill\n")

    stale_binary = rust_dir / "target/release/cua-driver"
    _write_executable(stale_binary, "printf 'stale workspace target\\n'")

    (fixture_root / ".gitignore").write_text(
        "rust/target/\nrust/relative custom target/\n", encoding="utf-8"
    )
    subprocess.run(
        ["git", "init", "-q", "-b", "local-hardened"], cwd=fixture_root, check=True
    )
    subprocess.run(["git", "add", "."], cwd=fixture_root, check=True)
    subprocess.run(
        [
            "git",
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "fixture",
        ],
        cwd=fixture_root,
        check=True,
    )
    source_oid = subprocess.check_output(
        ["git", "rev-parse", "HEAD"], cwd=fixture_root, text=True
    ).strip()
    # Hostile mutable repository metadata must not redefine either the selected
    # commit or archive membership. Build a replacement commit with a changed
    # skill blob, then add an info/attributes export-ignore rule; neither is
    # part of the authenticated commit.
    replacement_blob = subprocess.check_output(
        ["git", "hash-object", "-w", "--stdin"],
        cwd=fixture_root,
        input="replacement skill\n",
        text=True,
    ).strip()
    subprocess.run(["git", "read-tree", "HEAD"], cwd=fixture_root, check=True)
    subprocess.run(
        [
            "git",
            "update-index",
            "--cacheinfo",
            f"100644,{replacement_blob},rust/Skills/cua-driver/SKILL.md",
        ],
        cwd=fixture_root,
        check=True,
    )
    replacement_tree = subprocess.check_output(
        ["git", "write-tree"], cwd=fixture_root, text=True
    ).strip()
    replacement_commit = subprocess.check_output(
        [
            "git",
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit-tree",
            replacement_tree,
            "-p",
            source_oid,
            "-m",
            "hostile replacement",
        ],
        cwd=fixture_root,
        text=True,
    ).strip()
    subprocess.run(["git", "read-tree", source_oid], cwd=fixture_root, check=True)
    subprocess.run(
        ["git", "replace", source_oid, replacement_commit], cwd=fixture_root, check=True
    )
    git_common_dir = Path(
        subprocess.check_output(
            ["git", "rev-parse", "--path-format=absolute", "--git-common-dir"],
            cwd=fixture_root,
            text=True,
        ).strip()
    )
    info_dir = git_common_dir / "info"
    info_dir.mkdir(parents=True, exist_ok=True)
    (info_dir / "attributes").write_text(
        "rust/Skills/cua-driver/SKILL.md export-ignore\n", encoding="utf-8"
    )
    if embedded_source_sha == "a" * 40:
        embedded_source_assignment = 'embedded_source_sha="${CUA_DRIVER_SOURCE_SHA:?}"'
    else:
        embedded_source_assignment = f'embedded_source_sha="{embedded_source_sha}"'

    custom_target = (
        rust_dir / "relative custom target" if relative_target else tmp_path / "custom target"
    )
    cargo_target_dir = (
        str(custom_target.relative_to(rust_dir)) if relative_target else str(custom_target)
    )
    fake_bin = tmp_path / "fake-bin"
    host_only_marker = tmp_path / "host-only-input"
    host_only_marker.write_text("must not be visible inside the build namespace\n")
    _write_executable(
        fake_bin / "cargo",
        """set -eu
test ! -e "__HOST_ONLY_MARKER__" || {
    printf 'unscoped host filesystem leaked through sandbox\\n' >&2
    exit 1
}
for name in RUSTFLAGS CARGO_ENCODED_RUSTFLAGS RUSTC_WRAPPER RUSTC_WORKSPACE_WRAPPER CARGO_BUILD_RUSTC_WRAPPER CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER PKG_CONFIG; do
    eval "value=\\${$name-}"
    test -z "$value" || {
        printf 'host build environment leaked through sandbox: %s=%s\\n' "$name" "$value" >&2
        exit 1
    }
done
if [ "$*" = "fetch --locked" ]; then
    exit 0
fi
if [ "$*" = "vendor --locked --offline /run/cua-vendor" ]; then
    mkdir -p /run/cua-vendor/fake-dependency
    printf 'verified vendored dependency\n' > /run/cua-vendor/fake-dependency/source
    exit 0
fi
test "$*" = "build --locked --offline --release -p cua-driver -p cursor-theme-cli --features portal-input" || {
    printf 'unexpected cargo arguments: %s\\n' "$*" >&2
    exit 1
}
mount -o remount,rw /run/cua-vendor >/dev/null 2>&1 || true
if { printf 'tamper\n' >> /run/cua-vendor/fake-dependency/source; } 2>/dev/null; then
    printf 'vendored dependency tree remained writable during build\\n' >&2
    exit 1
fi
test "$CARGO_TARGET_DIR" = "/run/cua-output" || {
    printf 'unexpected sandbox cargo target: %s\\n' "$CARGO_TARGET_DIR" >&2
    exit 1
}
mkdir -p "$CARGO_TARGET_DIR/release"
__EMBEDDED_SOURCE_ASSIGNMENT__
cat > "$CARGO_TARGET_DIR/release/cua-driver" <<'BINARY'
#!/bin/sh
if [ "${1:-}" = "mcp" ]; then
    read -r initialize
    read -r get_config
    printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{}}'
    printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"structuredContent":{"source_sha":"__BUILD_SOURCE_SHA__"}}}'
    exit 0
fi
printf 'fresh custom target\n'
BINARY
sed -i "s/__BUILD_SOURCE_SHA__/$embedded_source_sha/" "$CARGO_TARGET_DIR/release/cua-driver"
chmod +x "$CARGO_TARGET_DIR/release/cua-driver"
printf 'fresh cursor theme compiler\n' > "$CARGO_TARGET_DIR/release/cua-cursor-theme"
chmod +x "$CARGO_TARGET_DIR/release/cua-cursor-theme"
""".replace("__EMBEDDED_SOURCE_ASSIGNMENT__", embedded_source_assignment).replace(
            "__HOST_ONLY_MARKER__", str(host_only_marker)
        ),
    )
    _install_fake_rust_toolchain(fake_bin, tmp_path)
    for ambient_wrapper in (
        "git",
        "bwrap",
        "tar",
        "sha256sum",
        "cut",
        "python3",
        "rpm",
        "dpkg-query",
        "sort",
    ):
        _write_executable(
            fake_bin / ambient_wrapper,
            f"printf 'ambient {ambient_wrapper} wrapper executed\\n' >&2\nexit 97",
        )
    _write_executable(
        fake_bin / "uname",
        """case "${1:-}" in
    -s) printf 'Linux\n' ;;
    -m) printf 'x86_64\n' ;;
    *) exit 2 ;;
esac
""",
    )
    _write_executable(fake_bin / "systemctl", "exit 0")
    _write_executable(fake_bin / "pkill", "exit 0")
    sfw_body = "exit 0"
    if mutate_source_during_fetch:
        sfw_body = (
            "mount -o remount,rw /run/cua-source >/dev/null 2>&1 || true\n"
            "printf 'fetch mutation\\n' >> Skills/cua-driver/SKILL.md || exit 73\n"
            "exit 0"
        )
    _write_executable(fake_bin / "sfw", sfw_body)
    _write_executable(fake_bin / "node", "exit 0")

    local_home = tmp_path / "local-home"
    install_bin = tmp_path / "install-bin"
    transaction_lock = tmp_path / "install-transaction.lock"
    env = os.environ.copy()
    env.pop("SUDO_USER", None)
    env.update(
        {
            "HOME": str(tmp_path / "home"),
            "PATH": f"{fake_bin}:/usr/bin:/bin",
            "CARGO_TARGET_DIR": cargo_target_dir,
            "CUA_DRIVER_SOURCE_SHA": source_oid,
            "CUA_DRIVER_REQUIRE_CLEAN_SOURCE": "1",
            "CUA_DRIVER_LOCAL_HOME": str(local_home),
            "CUA_DRIVER_LOCAL_INSTALL_DIR": str(tmp_path / "env-install-bin"),
            "RUSTFLAGS": "--cfg cua_audit_injected",
            "CARGO_ENCODED_RUSTFLAGS": "--cfg\x1fcua_encoded_audit_injected",
            "RUSTC_WRAPPER": "/host/must-not-run-rustc-wrapper",
            "RUSTC_WORKSPACE_WRAPPER": "/host/must-not-run-workspace-wrapper",
            "CARGO_BUILD_RUSTC_WRAPPER": "/host/must-not-run-cargo-wrapper",
            "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER": "/host/must-not-run-linker",
            "PKG_CONFIG": "/host/must-not-run-pkg-config",
        }
    )

    # Cover both accepted --bin-dir spellings and prove CLI precedence over
    # CUA_DRIVER_LOCAL_INSTALL_DIR without introducing a production test seam.
    bin_dir_args = (
        ["--bin-dir", str(install_bin)]
        if relative_target
        else [f"--bin-dir={install_bin}"]
    )
    for missing_args in (["--bin-dir"], ["--bin-dir="], ["--bin-dir", "--release"]):
        missing_bin_dir = subprocess.run(
            ["/bin/bash", str(scripts_dir / DISPATCHER.name), *missing_args],
            cwd=fixture_root,
            env=env,
            text=True,
            capture_output=True,
            check=False,
        )
        assert missing_bin_dir.returncode == 2
        assert "--bin-dir requires a value" in missing_bin_dir.stderr
    for relative_args in (["--bin-dir", "relative/bin"], ["--bin-dir=relative/bin"]):
        relative_bin_dir = subprocess.run(
            ["/bin/bash", str(scripts_dir / DISPATCHER.name), *relative_args],
            cwd=fixture_root,
            env=env,
            text=True,
            capture_output=True,
            check=False,
        )
        assert relative_bin_dir.returncode == 2
        assert "absolute path" in relative_bin_dir.stderr

    opt_out_env = env.copy()
    opt_out_env["CUA_DRIVER_REQUIRE_CLEAN_SOURCE"] = "0"
    opted_out = subprocess.run(
        ["/bin/bash", str(scripts_dir / DISPATCHER.name), "--release", *bin_dir_args],
        cwd=fixture_root,
        env=opt_out_env,
        text=True,
        capture_output=True,
        check=False,
    )
    assert opted_out.returncode != 0
    assert "cannot disable clean-source enforcement" in opted_out.stderr
    assert not (install_bin / "cua-driver-local").exists()
    transaction_lock.unlink()

    transaction_lock.parent.mkdir(parents=True, exist_ok=True)
    unrelated = tmp_path / "unrelated-lock-target"
    unrelated.write_text("must not be opened or changed\n")
    transaction_lock.symlink_to(unrelated)
    unsafe_lock = subprocess.run(
        ["/bin/bash", str(scripts_dir / DISPATCHER.name), "--release", *bin_dir_args],
        cwd=fixture_root,
        env=env,
        text=True,
        capture_output=True,
        check=False,
    )
    assert unsafe_lock.returncode != 0
    assert "lock" in unsafe_lock.stderr
    assert unrelated.read_text() == "must not be opened or changed\n"
    transaction_lock.unlink()
    transaction_lock.touch(mode=0o600)
    with transaction_lock.open("w") as held_lock:
        fcntl.flock(held_lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        blocked = subprocess.run(
            ["/bin/bash", str(scripts_dir / DISPATCHER.name), "--release", *bin_dir_args],
            cwd=fixture_root,
            env=env,
            text=True,
            capture_output=True,
            check=False,
        )
    assert blocked.returncode != 0
    assert "another CUA install/acceptance transaction is active" in blocked.stderr
    assert not (install_bin / "cua-driver-local").exists()

    result = subprocess.run(
        ["/bin/bash", str(scripts_dir / DISPATCHER.name), "--release", *bin_dir_args],
        cwd=fixture_root,
        env=env,
        text=True,
        capture_output=True,
        check=False,
    )

    if mutate_source_during_fetch:
        assert result.returncode != 0
        assert "Read-only file system" in result.stderr
        assert skill_file.read_text() == "initial skill\n"
        assert not (install_bin / "cua-driver-local").exists()
        releases = local_home / "packages/releases"
        assert not releases.exists() or not any(releases.iterdir())
        return

    if not expect_success:
        assert result.returncode != 0
        assert "embedded source does not match" in result.stderr
        assert not (install_bin / "cua-driver-local").exists()
        assert not any((local_home / "packages/releases").iterdir())
        return

    assert result.returncode == 0, result.stdout + result.stderr
    assert not (tmp_path / "env-install-bin").exists()
    assert not any(custom_target.glob(".cua-immutable-*"))
    installed = install_bin / "cua-driver-local"
    assert "fresh custom target" in installed.read_text()
    resolved = installed.resolve(strict=True)
    theme_binary = resolved.parent / "cua-cursor-theme"
    assert theme_binary.read_text() == "fresh cursor theme compiler\n"
    assert (local_home / "packages/current/cua-cursor-theme").resolve(strict=True) == theme_binary
    binary_sha = hashlib.sha256(resolved.read_bytes()).hexdigest()
    theme_binary_sha = hashlib.sha256(theme_binary.read_bytes()).hexdigest()
    skill_digest = hashlib.sha256()

    def skill_field(value: bytes) -> None:
        skill_digest.update(len(value).to_bytes(8, "big"))
        skill_digest.update(value)

    for value in (b"D", b"cua-driver", (0o555).to_bytes(4, "big"), b""):
        skill_field(value)
    for value in (
        b"F",
        b"cua-driver/SKILL.md",
        (0o444).to_bytes(4, "big"),
        b"initial skill\n",
    ):
        skill_field(value)
    skills_sha = skill_digest.hexdigest()
    manifest = json.loads((resolved.parent / "provenance.json").read_text())
    sfw_sha = manifest["sfw_sha256"]
    toolchain_sha = manifest["rust_toolchain_sha256"]
    system_inputs_sha = manifest["system_build_inputs_sha256"]
    for digest in (sfw_sha, toolchain_sha, system_inputs_sha):
        assert len(digest) == 64
        int(digest, 16)
    release_digest = hashlib.sha256()
    for value in (
        "cua-driver-local-provenance-v4",
        source_oid,
        binary_sha,
        theme_binary_sha,
        skills_sha,
        "release",
        "x86_64-unknown-linux-gnu",
        '["portal-input"]',
        sfw_sha,
        toolchain_sha,
        system_inputs_sha,
    ):
        encoded = value.encode("utf-8")
        release_digest.update(len(encoded).to_bytes(8, "big"))
        release_digest.update(encoded)
    release_identity_sha = release_digest.hexdigest()
    assert manifest == {
        "schema": "cua-driver-local-provenance-v4",
        "source_sha": source_oid,
        "binary_sha256": binary_sha,
        "cursor_theme_binary_sha256": theme_binary_sha,
        "skills_sha256": skills_sha,
        "sfw_sha256": sfw_sha,
        "rust_toolchain_sha256": toolchain_sha,
        "system_build_inputs_sha256": system_inputs_sha,
        "release_identity_sha256": release_identity_sha,
        "build_config": "release",
        "target": "x86_64-unknown-linux-gnu",
        "features": ["portal-input"],
    }
    assert source_oid in resolved.parent.name
    assert release_identity_sha in resolved.parent.name
    assert not (resolved.parent.stat().st_mode & stat.S_IWUSR)
    assert not any(
        path.name.startswith(".staging-")
        for path in (local_home / "packages/releases").iterdir()
    )

    # Reusing an identical immutable release is accepted.
    repeated = subprocess.run(
        ["/bin/bash", str(scripts_dir / DISPATCHER.name), "--release", *bin_dir_args],
        cwd=fixture_root,
        env=env,
        text=True,
        capture_output=True,
        check=False,
    )
    assert repeated.returncode == 0, repeated.stdout + repeated.stderr
    assert installed.resolve(strict=True).parent == resolved.parent

    # A pre-existing symlink at a content-addressed release path is never
    # treated as a reusable immutable release.
    changed_release = resolved.parent
    for path in changed_release.rglob("*"):
        path.chmod(path.stat().st_mode | stat.S_IWUSR)
    changed_release.chmod(changed_release.stat().st_mode | stat.S_IWUSR)
    shutil.rmtree(changed_release)
    changed_release.symlink_to(tmp_path, target_is_directory=True)
    malformed = subprocess.run(
        ["/bin/bash", str(scripts_dir / DISPATCHER.name), "--release", *bin_dir_args],
        cwd=fixture_root,
        env=env,
        text=True,
        capture_output=True,
        check=False,
    )
    assert malformed.returncode != 0
    assert "not a real directory" in malformed.stderr


def test_stage_only_exits_before_selector_publication() -> None:
    text = INSTALL_LOCAL.read_text(encoding="utf-8")
    stage_guard = text.index(
        'if [ "$STAGE_ONLY" = true ]; then',
        text.index('STAGED_BINARY="$VERSIONED_DIR/cua-driver-local"'),
    )
    visible_publish = text.index('BIN_LINK_TMP="${BIN_LINK}.new.$$"')
    current_publish = text.index('CURRENT_LINK_TMP="${CURRENT_LINK}.new.$$"')

    assert stage_guard < visible_publish < current_publish
    assert "printf '%s\\n' \"$VERSIONED_DIR\" >&3" in text
    assert 'exec 3>&1\n    exec 1>&2' in text


def test_stage_only_rejects_autostart_combination() -> None:
    text = INSTALL_LOCAL.read_text(encoding="utf-8")
    assert "--stage-only cannot be combined with --autostart" in text


def test_publication_is_durable_and_has_no_environment_selected_lock_domain() -> None:
    text = INSTALL_LOCAL.read_text(encoding="utf-8")
    lock_text = TRANSACTION_LOCK.read_text(encoding="utf-8")

    immutable_flush = text.index('fsync_tree_and_parent "$VERSIONED_DIR"')
    release_publish = text.index('mv "$VERSIONED_DIR" "$FINAL_VERSIONED_DIR"')
    release_parent_flush = text.index('fd = os.open(sys.argv[1]', release_publish)
    visible_publish = text.index('mv -Tf "$BIN_LINK_TMP" "$BIN_LINK"')
    visible_parent_flush = text.index('fsync_directory "$BIN_DIR"', visible_publish)
    current_publish = text.index('mv -Tf "$CURRENT_LINK_TMP" "$CURRENT_LINK"')
    current_parent_flush = text.index(
        'fsync_directory "$HOME_DIR/packages"', current_publish
    )

    assert immutable_flush < release_publish < release_parent_flush < visible_publish
    assert visible_publish < visible_parent_flush < current_publish < current_parent_flush
    for name in (
        "CUA_DRIVER_INSTALL_TRANSACTION_LOCK_TESTING",
        "CUA_DRIVER_INSTALL_TRANSACTION_LOCK_TEST_PATH",
        "CUA_DRIVER_TEST_OS",
        "CUA_DRIVER_TEST_ARCH",
    ):
        assert name not in text
        assert name not in lock_text


def test_installer_refuses_dirty_source_before_build(tmp_path: Path) -> None:
    fixture_root = tmp_path / "repo"
    scripts_dir = fixture_root / "libs/cua-driver/scripts"
    rust_dir = fixture_root / "libs/cua-driver/rust"
    scripts_dir.mkdir(parents=True)
    rust_dir.mkdir()
    _copy_installer_fixture(scripts_dir, tmp_path / "install-transaction.lock")
    (rust_dir / "Cargo.toml").write_text("[workspace]\nmembers = []\n")

    fake_bin = tmp_path / "fake-bin"
    build_marker = tmp_path / "cargo-ran"
    _write_executable(fake_bin / "cargo", f"touch {build_marker!s}\nexit 1")
    _install_fake_rust_toolchain(fake_bin, tmp_path)
    _write_executable(
        fake_bin / "uname",
        """case "${1:-}" in
    -s) printf 'Linux\n' ;;
    -m) printf 'x86_64\n' ;;
    *) exit 2 ;;
esac
""",
    )

    subprocess.run(["git", "init", "-q", "-b", "local-hardened"], cwd=fixture_root, check=True)
    subprocess.run(["git", "add", "."], cwd=fixture_root, check=True)
    subprocess.run(
        [
            "git",
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "fixture",
        ],
        cwd=fixture_root,
        check=True,
    )
    source_oid = subprocess.check_output(
        ["git", "rev-parse", "HEAD"], cwd=fixture_root, text=True
    ).strip()
    (fixture_root / "untracked").touch()

    env = os.environ.copy()
    env.pop("SUDO_USER", None)
    env.update(
        {
            "HOME": str(tmp_path / "home"),
            "PATH": f"{fake_bin}:/usr/bin:/bin",
            "CUA_DRIVER_SOURCE_SHA": source_oid,
            "CUA_DRIVER_REQUIRE_CLEAN_SOURCE": "1",
            "CUA_DRIVER_LOCAL_HOME": str(tmp_path / "local-home"),
            "CUA_DRIVER_LOCAL_INSTALL_DIR": str(tmp_path / "install-bin"),
        }
    )
    result = subprocess.run(
        ["/bin/bash", str(scripts_dir / INSTALL_LOCAL.name), "--release"],
        cwd=fixture_root,
        env=env,
        text=True,
        capture_output=True,
        check=False,
    )

    assert result.returncode != 0
    assert "dirty source tree" in result.stderr
    assert not build_marker.exists()


@pytest.mark.parametrize("host_os", ["Linux", "Darwin"])
def test_clean_promotion_refuses_source_drift_during_build(
    tmp_path: Path, host_os: str
) -> None:
    fixture_root = tmp_path / "repo"
    scripts_dir = fixture_root / "libs/cua-driver/scripts"
    rust_dir = fixture_root / "libs/cua-driver/rust"
    scripts_dir.mkdir(parents=True)
    rust_dir.mkdir()
    _copy_installer_fixture(
        scripts_dir, tmp_path / "install-transaction.lock", host_os=host_os
    )
    cargo_toml = rust_dir / "Cargo.toml"
    cargo_toml.write_text("[workspace]\nmembers = []\n")
    (fixture_root / ".gitignore").write_text("target/\n")

    fake_bin = tmp_path / "fake-bin"
    _write_executable(
        fake_bin / "cargo",
        """set -eu
mkdir -p "$CARGO_TARGET_DIR/release"
cat > "$CARGO_TARGET_DIR/release/cua-driver" <<'BINARY'
#!/bin/sh
exit 0
BINARY
chmod +x "$CARGO_TARGET_DIR/release/cua-driver"
printf 'BUILD_CWD=%s\n' "$PWD" >&2
printf '# source drift\n' >> "$PWD/Cargo.toml"
""",
    )
    _install_fake_rust_toolchain(fake_bin, tmp_path)
    _write_executable(
        fake_bin / "uname",
        f"""case "${{1:-}}" in
    -s) printf '{host_os}\\n' ;;
    -m) printf 'x86_64\\n' ;;
    *) exit 2 ;;
esac
""",
    )
    _write_executable(fake_bin / "sfw", "exit 0")
    _write_executable(fake_bin / "node", "exit 0")

    subprocess.run(["git", "init", "-q", "-b", "local-hardened"], cwd=fixture_root, check=True)
    subprocess.run(["git", "add", "."], cwd=fixture_root, check=True)
    subprocess.run(
        [
            "git",
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "fixture",
        ],
        cwd=fixture_root,
        check=True,
    )
    source_oid = subprocess.check_output(
        ["git", "rev-parse", "HEAD"], cwd=fixture_root, text=True
    ).strip()

    env = os.environ.copy()
    env.pop("SUDO_USER", None)
    env.update(
        {
            "HOME": str(tmp_path / "home"),
            "PATH": f"{fake_bin}:/usr/bin:/bin",
            "CARGO_TARGET_DIR": str(rust_dir / "target"),
            "CUA_DRIVER_SOURCE_SHA": source_oid,
            "CUA_DRIVER_REQUIRE_CLEAN_SOURCE": "1",
            "CUA_DRIVER_LOCAL_HOME": str(tmp_path / "local-home"),
            "CUA_DRIVER_LOCAL_INSTALL_DIR": str(tmp_path / "install-bin"),
        }
    )
    result = subprocess.run(
        ["/bin/bash", str(scripts_dir / INSTALL_LOCAL.name), "--release"],
        cwd=fixture_root,
        env=env,
        text=True,
        capture_output=True,
        check=False,
    )

    assert result.returncode != 0
    if host_os == "Darwin":
        assert "hardened local promotion is temporarily unavailable on macOS" in result.stderr
        assert "BUILD_CWD=" not in result.stderr
    else:
        assert "BUILD_CWD=/run/cua-source/libs/cua-driver/rust" in result.stderr
        assert "Read-only file system" in result.stderr
        assert cargo_toml.read_text() == "[workspace]\nmembers = []\n"
    assert not (tmp_path / "install-bin/cua-driver-local").exists()
    assert not (tmp_path / "local-home/packages/current").exists()


def test_sha256_git_oid_is_refused(tmp_path: Path) -> None:
    fixture_root = tmp_path / "cua-driver"
    scripts_dir = fixture_root / "scripts"
    (fixture_root / "rust").mkdir(parents=True)
    scripts_dir.mkdir()
    _copy_installer_fixture(scripts_dir, tmp_path / "install-transaction.lock")

    non_git_env = os.environ.copy()
    non_git_env.pop("SUDO_USER", None)
    non_git = subprocess.run(
        ["/bin/bash", str(scripts_dir / INSTALL_LOCAL.name), "--help"],
        cwd=fixture_root,
        env=non_git_env,
        text=True,
        capture_output=True,
        check=False,
    )
    assert non_git.returncode != 0
    assert "requires a Git checkout" in non_git.stderr

    initialized = subprocess.run(
        ["git", "init", "-q", "--object-format=sha256", "-b", "local-hardened"],
        cwd=fixture_root,
        text=True,
        capture_output=True,
        check=False,
    )
    if initialized.returncode != 0:
        pytest.skip("installed Git does not support SHA-256 repositories")
    subprocess.run(["git", "add", "."], cwd=fixture_root, check=True)
    subprocess.run(
        [
            "git",
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "fixture",
        ],
        cwd=fixture_root,
        check=True,
    )
    source_oid = subprocess.check_output(
        ["git", "rev-parse", "HEAD"], cwd=fixture_root, text=True
    ).strip()
    assert len(source_oid) == 64

    env = os.environ.copy()
    env.pop("SUDO_USER", None)
    env["CUA_DRIVER_SOURCE_SHA"] = source_oid
    result = subprocess.run(
        ["/bin/bash", str(scripts_dir / INSTALL_LOCAL.name), "--help"],
        cwd=fixture_root,
        env=env,
        text=True,
        capture_output=True,
        check=False,
    )

    assert result.returncode != 0
    assert "exact 40-character Git OID" in result.stderr
