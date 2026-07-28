import os
from pathlib import Path
import subprocess


INSTALLER = Path(__file__).parents[1] / "_install-rust.sh"


def _write_executable(path: Path, body: str) -> None:
    path.write_text(f"#!/bin/sh\nset -eu\n{body}", encoding="utf-8")
    path.chmod(0o755)


def test_release_checksum_mismatch_fails_before_extraction(tmp_path: Path) -> None:
    fake_bin = tmp_path / "fake-bin"
    fake_bin.mkdir()
    tar_marker = tmp_path / "tar-called"

    _write_executable(
        fake_bin / "curl",
        r'''
out=""
url=""
while [ "$#" -gt 0 ]; do
    case "$1" in
        -o)
            out="$2"
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
case "$url" in
    */checksums.txt)
        archive="cua-driver-rs-0.13.1-linux-x86_64-binary.tar.gz"
        printf '%064d  %s\n' 0 "$archive" > "$out"
        ;;
    *)
        printf 'tampered release payload\n' > "$out"
        ;;
esac
''',
    )
    _write_executable(
        fake_bin / "tar",
        'printf "called\\n" > "$FAKE_TAR_MARKER"\nexit 91\n',
    )

    home = tmp_path / "home"
    home.mkdir()
    install_dir = tmp_path / "bin"
    package_home = tmp_path / "packages"
    env = os.environ.copy()
    env.update(
        {
            "PATH": f"{fake_bin}:/usr/bin:/bin",
            "HOME": str(home),
            "CUA_DRIVER_RS_VERSION": "0.13.1",
            "CUA_DRIVER_RS_HOME": str(package_home),
            "FAKE_TAR_MARKER": str(tar_marker),
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
    )

    combined = result.stdout + result.stderr
    assert result.returncode != 0
    assert "checksum mismatch" in combined
    assert not tar_marker.exists()
    assert not install_dir.exists()
