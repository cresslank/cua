import hashlib
import os
from pathlib import Path
import subprocess

import pytest


INSTALLER = Path(__file__).parents[1] / "_install-rust.sh"


def _write_executable(path: Path, body: str) -> None:
    path.write_text(f"#!/bin/sh\nset -eu\n{body}", encoding="utf-8")
    path.chmod(0o755)


@pytest.mark.parametrize("manifest", ["mismatch", "full-only", "valid"])
def test_release_checksum_mismatch_fails_before_extraction(
    tmp_path: Path, manifest: str
) -> None:
    fake_bin = tmp_path / "fake-bin"
    fake_bin.mkdir()
    tar_marker = tmp_path / "tar-called"
    requests = tmp_path / "requests"
    archive = "cua-driver-rs-0.13.1-linux-x86_64-binary.tar.gz"
    release_url = "https://github.com/trycua/cua/releases/download/cua-driver-rs-v0.13.1"
    payload = b"tampered release payload\n"
    digest = hashlib.sha256(payload).hexdigest() if manifest == "valid" else "0" * 64
    listed_archive = archive.replace("-binary", "") if manifest == "full-only" else archive

    # The checksum fixture describes a Linux x86_64 asset, regardless of the
    # host running this test. Otherwise Darwin refuses a missing entry before
    # reaching the checksum-mismatch boundary this regression must exercise.
    _write_executable(
        fake_bin / "uname",
        'case "$1" in -s) printf "Linux\\n" ;; -m) printf "x86_64\\n" ;; *) exit 2 ;; esac\n',
    )
    _write_executable(
        fake_bin / "curl",
        r'''
out=""
url=""
write_status=0
while [ "$#" -gt 0 ]; do
    case "$1" in
        -o)
            out="$2"
            shift 2
            ;;
        -w)
            write_status=1
            shift 2
            ;;
        -*)
            shift
            ;;
        *)
            url="$1"
            shift
            ;;
    esac
done
[ -n "$out" ]
printf '%s\n' "$url" >> "$FAKE_REQUESTS"
case "$url" in
    "$FAKE_RELEASE_URL/SHA256SUMS")
        printf '%s  %s\n' "$FAKE_DIGEST" "$FAKE_LISTED_ARCHIVE" > "$out"
        ;;
    "$FAKE_RELEASE_URL/$FAKE_ARCHIVE")
        printf 'tampered release payload\n' > "$out"
        ;;
    "$FAKE_RELEASE_URL/$FAKE_ARCHIVE.sigstore.json")
        printf '{}\n' > "$out"
        ;;
    *) exit 92 ;;
esac
if [ "$write_status" -eq 1 ]; then
    printf '200'
fi
''',
    )
    _write_executable(
        fake_bin / "tar",
        'printf "called\\n" > "$FAKE_TAR_MARKER"\nexit 91\n',
    )

    _write_executable(fake_bin / "cosign", "exit 0\n")

    home = tmp_path / "home"
    home.mkdir()
    install_dir = tmp_path / "bin"
    package_home = tmp_path / "packages"
    temporary = tmp_path / "temporary"
    temporary.mkdir()
    env = os.environ.copy()
    env.update(
        {
            "PATH": f"{fake_bin}:/usr/bin:/bin",
            "HOME": str(home),
            "CUA_DRIVER_RS_VERSION": "0.13.1",
            "CUA_DRIVER_RS_HOME": str(package_home),
            "TMPDIR": str(temporary),
            "FAKE_TAR_MARKER": str(tar_marker),
            "FAKE_REQUESTS": str(requests),
            "FAKE_RELEASE_URL": release_url,
            "FAKE_ARCHIVE": archive,
            "FAKE_DIGEST": digest,
            "FAKE_LISTED_ARCHIVE": listed_archive,
        }
    )

    result = subprocess.run(
        [
            "/bin/bash",
            str(INSTALLER),
            "--bin-dir",
            str(install_dir),
            "--no-modify-path",
        ],
        env=env,
        text=True,
        capture_output=True,
        check=False,
        timeout=15,
    )

    combined = result.stdout + result.stderr
    expected_requests = [f"{release_url}/{archive}", f"{release_url}/SHA256SUMS"]
    if manifest == "valid":
        expected_requests.append(f"{release_url}/{archive}.sigstore.json")
        # Stop at fake extraction: this control cannot install or run a driver.
        assert result.returncode == 91
        assert f"verified {archive} against SHA256SUMS" in combined
        assert tar_marker.read_text() == "called\n"
    else:
        assert result.returncode == 1
        if manifest == "mismatch":
            actual = hashlib.sha256(payload).hexdigest()
            assert (
                f"error: {archive} does not match cua-driver-rs-v0.13.1's "
                f"SHA256SUMS (expected {digest}, got {actual}); refusing to install it"
            ) in combined
        else:
            assert (
                f"error: {archive} is not listed in cua-driver-rs-v0.13.1's "
                "SHA256SUMS; refusing to install it"
            ) in combined
        assert not tar_marker.exists()
    # Exact asset requests also exclude any unintended API resolution.
    assert requests.read_text().splitlines() == expected_requests
    assert not install_dir.exists()
    assert not (package_home / "packages/.install.lock.d").exists()
    assert list(temporary.iterdir()) == []
