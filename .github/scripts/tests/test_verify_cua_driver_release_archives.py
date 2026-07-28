from __future__ import annotations

from io import BytesIO
from pathlib import Path
import tarfile
import zipfile

import pytest

from verify_cua_driver_release_archives import (
    ArchiveContract,
    ContractError,
    release_contracts,
    verify_release_archives,
)


VERSION = "9.8.7"


def _write_tar(path: Path, contract: ArchiveContract) -> None:
    with tarfile.open(path, "w:gz") as archive:
        for name in contract.members:
            payload = f"payload for {name}".encode()
            info = tarfile.TarInfo(name)
            info.size = len(payload)
            info.mode = 0o755 if name in contract.executable_members else 0o644
            archive.addfile(info, BytesIO(payload))


def _write_zip(path: Path, contract: ArchiveContract) -> None:
    with zipfile.ZipFile(path, "w") as archive:
        for name in contract.members:
            archive.writestr(name, f"payload for {name}")


def _write_valid_release(root: Path) -> tuple[ArchiveContract, ...]:
    contracts = release_contracts(VERSION)
    for contract in contracts:
        path = root / contract.filename
        if path.name.endswith(".tar.gz"):
            _write_tar(path, contract)
        else:
            _write_zip(path, contract)
    return contracts


def test_complete_release_archive_set_passes(tmp_path: Path) -> None:
    contracts = _write_valid_release(tmp_path)

    verified = verify_release_archives(tmp_path, VERSION)

    assert len(verified) == len(contracts) == 12


def test_linux_wayland_helper_runtime_contract_is_complete() -> None:
    expected = {
        "wayland-helper/install.sh",
        "wayland-helper/winrects@cua/extension.js",
        "wayland-helper/winrects@cua/metadata.json",
        "wayland-helper/winrects@cua/policy.js",
    }
    linux_contracts = [
        contract for contract in release_contracts(VERSION) if "-linux-" in contract.filename
    ]

    assert len(linux_contracts) == 4
    for contract in linux_contracts:
        helper_prefix = (
            ""
            if contract.filename.endswith("-binary.tar.gz")
            else f"{contract.filename.removesuffix('.tar.gz')}/"
        )
        helper_members = {
            member.removeprefix(helper_prefix)
            for member in contract.members
            if "wayland-helper/" in member
        }
        assert helper_members == expected
        assert f"{helper_prefix}wayland-helper/install.sh" in contract.executable_members


def test_missing_cursor_theme_fails_with_archive_and_member(
    tmp_path: Path,
) -> None:
    contracts = _write_valid_release(tmp_path)
    target = next(
        contract for contract in contracts if contract.filename.endswith("darwin-universal.tar.gz")
    )
    missing = (
        f"cua-driver-rs-{VERSION}-darwin-universal/CuaDriver.app/Contents/MacOS/cua-cursor-theme"
    )
    broken = ArchiveContract(
        target.filename,
        tuple(member for member in target.members if member != missing),
        tuple(member for member in target.executable_members if member != missing),
    )
    _write_tar(tmp_path / target.filename, broken)

    with pytest.raises(ContractError, match=rf"{target.filename} is missing {missing}"):
        verify_release_archives(tmp_path, VERSION)


@pytest.mark.parametrize(
    "archive_suffix",
    (
        "linux-x86_64.tar.gz",
        "linux-x86_64-binary.tar.gz",
        "linux-arm64.tar.gz",
        "linux-arm64-binary.tar.gz",
    ),
)
@pytest.mark.parametrize(
    "helper_member",
    (
        "wayland-helper/install.sh",
        "wayland-helper/winrects@cua/extension.js",
        "wayland-helper/winrects@cua/metadata.json",
        "wayland-helper/winrects@cua/policy.js",
    ),
)
def test_missing_linux_wayland_helper_fails_with_archive_and_member(
    tmp_path: Path,
    archive_suffix: str,
    helper_member: str,
) -> None:
    contracts = _write_valid_release(tmp_path)
    target = next(
        contract for contract in contracts if contract.filename.endswith(archive_suffix)
    )
    prefix = (
        ""
        if target.filename.endswith("-binary.tar.gz")
        else f"{target.filename.removesuffix('.tar.gz')}/"
    )
    missing = f"{prefix}{helper_member}"
    broken = ArchiveContract(
        target.filename,
        tuple(member for member in target.members if member != missing),
        tuple(member for member in target.executable_members if member != missing),
    )
    _write_tar(tmp_path / target.filename, broken)

    with pytest.raises(ContractError, match=rf"{target.filename} is missing {missing}"):
        verify_release_archives(tmp_path, VERSION)


def test_non_executable_linux_wayland_helper_fails_closed(tmp_path: Path) -> None:
    contracts = _write_valid_release(tmp_path)
    target = next(
        contract
        for contract in contracts
        if contract.filename.endswith("linux-x86_64-binary.tar.gz")
    )
    helper = "wayland-helper/install.sh"
    broken = ArchiveContract(
        target.filename,
        target.members,
        tuple(member for member in target.executable_members if member != helper),
    )
    _write_tar(tmp_path / target.filename, broken)

    with pytest.raises(
        ContractError,
        match=rf"{target.filename} contains non-executable member {helper}",
    ):
        verify_release_archives(tmp_path, VERSION)


def test_missing_archive_fails_closed(tmp_path: Path) -> None:
    contracts = _write_valid_release(tmp_path)
    missing = contracts[0].filename
    (tmp_path / missing).unlink()

    with pytest.raises(ContractError, match=rf"missing release archive: {missing}"):
        verify_release_archives(tmp_path, VERSION)


def test_non_executable_unix_binary_fails_closed(tmp_path: Path) -> None:
    contracts = _write_valid_release(tmp_path)
    target = next(
        contract
        for contract in contracts
        if contract.filename.endswith("linux-x86_64-binary.tar.gz")
    )
    broken = ArchiveContract(target.filename, target.members)
    _write_tar(tmp_path / target.filename, broken)

    with pytest.raises(
        ContractError,
        match=rf"{target.filename} contains non-executable member cua-driver",
    ):
        verify_release_archives(tmp_path, VERSION)
