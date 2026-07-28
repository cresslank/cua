from __future__ import annotations

import io
import os
from pathlib import Path
import subprocess
import tarfile

SCRIPTS = Path(__file__).resolve().parents[1]
ARCHIVER = SCRIPTS / "_immutable-git-tree-tar.py"
RECEIVER = SCRIPTS / "_receive-private-build.py"
PUBLISHER = SCRIPTS / "_publish-linux-release.py"


def test_exact_tree_archiver_ignores_ambient_export_attributes(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    repo.mkdir()
    (repo / "kept").write_text("kept\n")
    (repo / "ambiently-ignored").write_text("must remain\n")
    subprocess.run(["git", "init", "-q"], cwd=repo, check=True)
    subprocess.run(["git", "add", "."], cwd=repo, check=True)
    subprocess.run(
        ["git", "-c", "user.name=t", "-c", "user.email=t@invalid", "commit", "-qm", "tree"],
        cwd=repo,
        check=True,
    )
    oid = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip()
    (repo / ".git/info/attributes").write_text("ambiently-ignored export-ignore\n")
    emitted = subprocess.check_output(["python3", str(ARCHIVER), str(repo), oid])
    with tarfile.open(fileobj=io.BytesIO(emitted)) as archive:
        assert set(archive.getnames()) == {"kept", "ambiently-ignored"}


def test_private_export_receiver_rejects_skill_symlinks(tmp_path: Path) -> None:
    releases = tmp_path / "releases"
    releases.mkdir()
    release_fd = os.open(releases, os.O_RDONLY | os.O_DIRECTORY)
    stream = io.BytesIO()
    with tarfile.open(fileobj=stream, mode="w") as archive:
        for name, payload in (
            ("cua-driver-local", b"binary"),
            ("scripts/_local-signing.sh", b"signing"),
            ("Skills/cua-driver/SKILL.md", b"skill"),
        ):
            info = tarfile.TarInfo(name)
            info.size = len(payload)
            archive.addfile(info, io.BytesIO(payload))
        link = tarfile.TarInfo("Skills/cua-driver/escape")
        link.type = tarfile.SYMTYPE
        link.linkname = "/tmp"
        archive.addfile(link)
    try:
        result = subprocess.run(
            ["python3", str(RECEIVER), str(release_fd), ".stage"],
            input=stream.getvalue(),
            capture_output=True,
            pass_fds=(release_fd,),
            check=False,
        )
    finally:
        os.close(release_fd)
    assert result.returncode != 0
    assert b"symlink is not allowed" in result.stderr


def test_publisher_rejects_replaced_destination_pathnames(tmp_path: Path) -> None:
    package = tmp_path / "packages"
    releases = package / "releases"
    binary_dir = tmp_path / "bin"
    releases.mkdir(parents=True)
    binary_dir.mkdir()
    package_fd = os.open(package, os.O_RDONLY | os.O_DIRECTORY)
    releases_fd = os.open(releases, os.O_RDONLY | os.O_DIRECTORY)
    bin_fd = os.open(binary_dir, os.O_RDONLY | os.O_DIRECTORY)
    held = tmp_path / "packages-held"
    package.rename(held)
    (package / "releases").mkdir(parents=True)
    try:
        result = subprocess.run(
            [
                "python3", str(PUBLISHER), str(package_fd), str(releases_fd), str(bin_fd),
                str(package), str(package / "releases"), str(binary_dir),
                ".stage", "final", "0" * 64, "1" * 64, "a" * 40,
                "2" * 64, "-", str(package / "releases/final"),
                str(package / "current/cua-driver-local"),
            ],
            text=True,
            capture_output=True,
            pass_fds=(package_fd, releases_fd, bin_fd),
            check=False,
        )
    finally:
        os.close(package_fd)
        os.close(releases_fd)
        os.close(bin_fd)
    assert result.returncode != 0
    assert "package root pathname was replaced" in result.stderr
    assert not (package / "current").exists()
