from __future__ import annotations

from io import BytesIO
from pathlib import Path
import shutil
import stat
import subprocess
import tarfile
import zipfile

import pytest

from verify_cua_driver_release_archives import (
    ArchiveContract,
    ContractError,
    _wayland_helper_members,
    _verify_tar,
    _verify_zip,
    release_contracts,
    verify_release_archives,
)


VERSION = "9.8.7"
CURRENT_HELPER_MEMBERS = (
    "wayland-helper/README.md",
    "wayland-helper/install.sh",
    "wayland-helper/tests/gnome_devkit.mjs",
    "wayland-helper/winrects@cua/extension.js",
    "wayland-helper/winrects@cua/metadata.json",
    "wayland-helper/winrects@cua/policy.js",
)
HISTORICAL_0_14_HELPER_MEMBERS = (
    "wayland-helper/README.md",
    "wayland-helper/install.sh",
    "wayland-helper/winrects@cua/extension.js",
    "wayland-helper/winrects@cua/metadata.json",
)


def _write_tar(
    path: Path,
    contract: ArchiveContract,
    *,
    transform=lambda name: name,
) -> None:
    with tarfile.open(path, "w:gz") as archive:
        for name in contract.members:
            payload = f"payload for {name}".encode()
            info = tarfile.TarInfo(transform(name))
            info.size = len(payload)
            info.mode = 0o755 if name in contract.executable_members else 0o644
            archive.addfile(info, BytesIO(payload))


def _write_zip(
    path: Path,
    contract: ArchiveContract,
    *,
    transform=lambda name: name,
) -> None:
    with zipfile.ZipFile(path, "w") as archive:
        for name in contract.members:
            archive.writestr(transform(name), f"payload for {name}")


def _write_helper_source(root: Path, members: tuple[str, ...]) -> Path:
    source = root / "helper-source"
    for member in members:
        relative = member.removeprefix("wayland-helper/")
        path = source / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(f"payload for {member}\n", encoding="utf-8")
        if relative == "install.sh":
            path.chmod(0o755)
    return source


def _write_valid_release(root: Path) -> tuple[ArchiveContract, ...]:
    _write_helper_source(root, CURRENT_HELPER_MEMBERS)
    contracts = release_contracts(VERSION, CURRENT_HELPER_MEMBERS)
    for contract in contracts:
        path = root / contract.filename
        if path.name.endswith(".tar.gz"):
            _write_tar(path, contract)
        else:
            _write_zip(path, contract)
    return contracts


def _verify(root: Path) -> tuple[Path, ...]:
    return verify_release_archives(root, VERSION, root / "helper-source")


def _write_linux_archives_like_workflow(
    root: Path, helper_source: Path, helper_members: tuple[str, ...]
) -> tuple[ArchiveContract, ...]:
    release = root / "release"
    stage_name = f"cua-driver-rs-{VERSION}-linux-x86_64"
    stage = release / stage_name
    stage.mkdir(parents=True)
    for name in (
        "cua-driver",
        "cua-cursor-theme",
        "libcua_driver_sdk.so",
        "cua_driver_node_runtime.node",
        "cua_driver_abi.h",
    ):
        path = stage / name
        path.write_text(f"payload for {name}\n", encoding="utf-8")
        path.chmod(0o755 if name in {"cua-driver", "cua-cursor-theme"} else 0o644)
    shutil.copytree(helper_source, stage / "wayland-helper")

    subprocess.run(
        ["tar", "-czf", f"{stage_name}.tar.gz", stage_name],
        cwd=release,
        check=True,
    )
    subprocess.run(
        [
            "tar",
            "-czf",
            f"{stage_name}-binary.tar.gz",
            "-C",
            stage_name,
            "cua-driver",
            "cua-cursor-theme",
            "libcua_driver_sdk.so",
            "cua_driver_node_runtime.node",
            "cua_driver_abi.h",
            "wayland-helper",
        ],
        cwd=release,
        check=True,
    )
    return tuple(
        contract
        for contract in release_contracts(VERSION, helper_members)
        if contract.filename
        in {f"{stage_name}.tar.gz", f"{stage_name}-binary.tar.gz"}
    )


def test_complete_release_archive_set_passes(tmp_path: Path) -> None:
    contracts = _write_valid_release(tmp_path)

    verified = _verify(tmp_path)

    assert len(verified) == len(contracts) == 12


def test_linux_wayland_helper_contract_matches_candidate_source() -> None:
    linux_contracts = [
        contract
        for contract in release_contracts(VERSION, CURRENT_HELPER_MEMBERS)
        if "-linux-" in contract.filename
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
        assert helper_members == set(CURRENT_HELPER_MEMBERS)
        assert f"{helper_prefix}wayland-helper/install.sh" in contract.executable_members


def test_historical_0_14_contract_comes_from_exact_candidate_source(
    tmp_path: Path,
) -> None:
    source = _write_helper_source(tmp_path, HISTORICAL_0_14_HELPER_MEMBERS)
    contracts = _write_linux_archives_like_workflow(
        tmp_path, source, HISTORICAL_0_14_HELPER_MEMBERS
    )
    for contract in contracts:
        _verify_tar(tmp_path / "release" / contract.filename, contract)

    assert len(contracts) == 2
    assert all(
        not member.endswith("policy.js")
        for contract in contracts
        for member in contract.members
    )


def test_current_linux_producer_and_source_contract_are_closed(tmp_path: Path) -> None:
    source = Path(__file__).resolve().parents[3] / "libs/cua-driver/wayland-helper"
    helper_members = _wayland_helper_members(source)
    contracts = _write_linux_archives_like_workflow(tmp_path, source, helper_members)

    for contract in contracts:
        _verify_tar(tmp_path / "release" / contract.filename, contract)

    assert len(contracts) == 2


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
        _verify(tmp_path)


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
        _verify(tmp_path)


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
        _verify(tmp_path)


def test_missing_archive_fails_closed(tmp_path: Path) -> None:
    contracts = _write_valid_release(tmp_path)
    missing = contracts[0].filename
    (tmp_path / missing).unlink()

    with pytest.raises(ContractError, match=rf"missing release archive: {missing}"):
        _verify(tmp_path)


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
        _verify(tmp_path)


def test_owner_unreadable_required_tar_member_fails_closed(tmp_path: Path) -> None:
    contract = ArchiveContract("probe.tar.gz", ("payload",))
    path = tmp_path / contract.filename
    with tarfile.open(path, "w:gz") as archive:
        payload = b"payload"
        info = tarfile.TarInfo("payload")
        info.size = len(payload)
        info.mode = 0o044
        archive.addfile(info, BytesIO(payload))

    with pytest.raises(ContractError, match="owner-unreadable member payload"):
        _verify_tar(path, contract)


def test_owner_inaccessible_required_tar_directory_fails_closed(
    tmp_path: Path,
) -> None:
    contract = ArchiveContract("probe.tar.gz", ("safe/member",))
    path = tmp_path / contract.filename
    with tarfile.open(path, "w:gz") as archive:
        directory = tarfile.TarInfo("safe/")
        directory.type = tarfile.DIRTYPE
        directory.mode = 0o000
        archive.addfile(directory)
        payload = b"payload"
        member = tarfile.TarInfo("safe/member")
        member.size = len(payload)
        member.mode = 0o644
        archive.addfile(member, BytesIO(payload))

    with pytest.raises(ContractError, match="owner-inaccessible directory safe"):
        _verify_tar(path, contract)


def test_other_execute_bit_does_not_satisfy_tar_executable_contract(
    tmp_path: Path,
) -> None:
    contract = ArchiveContract("probe.tar.gz", ("payload",), ("payload",))
    path = tmp_path / contract.filename
    with tarfile.open(path, "w:gz") as archive:
        payload = b"payload"
        info = tarfile.TarInfo("payload")
        info.size = len(payload)
        info.mode = 0o401
        archive.addfile(info, BytesIO(payload))

    with pytest.raises(ContractError, match="non-executable member payload"):
        _verify_tar(path, contract)


@pytest.mark.parametrize("archive_kind", ("tar", "zip"))
def test_backslash_member_names_are_rejected(
    tmp_path: Path, archive_kind: str
) -> None:
    contract = ArchiveContract(f"probe.{archive_kind}", ("safe/member",))
    path = tmp_path / contract.filename
    if archive_kind == "tar":
        _write_tar(path, contract, transform=lambda name: name.replace("/", "\\"))
        verify = _verify_tar
    else:
        _write_zip(path, contract, transform=lambda name: name.replace("/", "\\"))
        verify = _verify_zip

    with pytest.raises(ContractError, match="unsafe archive member name"):
        verify(path, contract)


@pytest.mark.parametrize("archive_kind", ("tar", "zip"))
@pytest.mark.parametrize(
    "unsafe_name",
    ("/absolute", "../escape", "safe/../escape", "C:/escape", "safe//member", "./safe"),
)
def test_unsafe_extra_members_are_rejected(
    tmp_path: Path, archive_kind: str, unsafe_name: str
) -> None:
    contract = ArchiveContract(f"probe.{archive_kind}", ("safe/member",))
    path = tmp_path / contract.filename
    if archive_kind == "tar":
        with tarfile.open(path, "w:gz") as archive:
            for name in ("safe/member", unsafe_name):
                payload = b"payload"
                info = tarfile.TarInfo(name)
                info.size = len(payload)
                archive.addfile(info, BytesIO(payload))
        verify = _verify_tar
    else:
        with zipfile.ZipFile(path, "w") as archive:
            archive.writestr("safe/member", "payload")
            archive.writestr(unsafe_name, "payload")
        verify = _verify_zip

    with pytest.raises(ContractError, match="archive member name"):
        verify(path, contract)


@pytest.mark.parametrize("archive_kind", ("tar", "zip"))
@pytest.mark.parametrize("extra_is_directory", (True, False))
def test_unexpected_helper_members_are_rejected(
    tmp_path: Path, archive_kind: str, extra_is_directory: bool
) -> None:
    contract = ArchiveContract(
        f"probe.{archive_kind}", ("wayland-helper/install.sh",)
    )
    path = tmp_path / contract.filename
    extra = (
        "wayland-helper/unexpected/"
        if extra_is_directory
        else "wayland-helper/unexpected.js"
    )
    names = ("wayland-helper/install.sh", extra)
    if archive_kind == "tar":
        with tarfile.open(path, "w:gz") as archive:
            for name in names:
                payload = b"payload"
                info = tarfile.TarInfo(name)
                if name.endswith("/"):
                    info.type = tarfile.DIRTYPE
                    archive.addfile(info)
                else:
                    info.size = len(payload)
                    archive.addfile(info, BytesIO(payload))
        verify = _verify_tar
    else:
        with zipfile.ZipFile(path, "w") as archive:
            for name in names:
                archive.writestr(name, "payload")
        verify = _verify_zip

    with pytest.raises(ContractError, match="unexpected Wayland helper member"):
        verify(path, contract)


def test_zip_slash_suffixed_symlink_is_rejected(tmp_path: Path) -> None:
    contract = ArchiveContract("probe.zip", ("safe/member",))
    path = tmp_path / contract.filename
    symlink = zipfile.ZipInfo("link/")
    symlink.create_system = 3
    symlink.external_attr = (stat.S_IFLNK | 0o777) << 16
    with zipfile.ZipFile(path, "w") as archive:
        archive.writestr("safe/member", "payload")
        archive.writestr(symlink, "elsewhere")

    with pytest.raises(ContractError, match="unsupported member type link"):
        _verify_zip(path, contract)


@pytest.mark.parametrize("archive_kind", ("tar", "zip"))
@pytest.mark.parametrize("ancestor_first", (True, False))
def test_file_directory_path_collisions_are_rejected(
    tmp_path: Path, archive_kind: str, ancestor_first: bool
) -> None:
    contract = ArchiveContract(f"probe.{archive_kind}", ("safe/member",))
    path = tmp_path / contract.filename
    names = ("safe", "safe/member")
    if not ancestor_first:
        names = tuple(reversed(names))
    if archive_kind == "tar":
        with tarfile.open(path, "w:gz") as archive:
            for name in names:
                payload = b"payload"
                info = tarfile.TarInfo(name)
                info.size = len(payload)
                archive.addfile(info, BytesIO(payload))
        verify = _verify_tar
    else:
        with zipfile.ZipFile(path, "w") as archive:
            for name in names:
                archive.writestr(name, "payload")
        verify = _verify_zip

    with pytest.raises(ContractError, match="file/directory path collision"):
        verify(path, contract)


@pytest.mark.parametrize("archive_kind", ("tar", "zip"))
def test_duplicate_members_are_rejected(
    tmp_path: Path, archive_kind: str
) -> None:
    contract = ArchiveContract(f"probe.{archive_kind}", ("safe/member",))
    path = tmp_path / contract.filename
    if archive_kind == "tar":
        with tarfile.open(path, "w:gz") as archive:
            for _ in range(2):
                payload = b"payload"
                info = tarfile.TarInfo("safe/member")
                info.size = len(payload)
                archive.addfile(info, BytesIO(payload))
        verify = _verify_tar
    else:
        with pytest.warns(UserWarning):
            with zipfile.ZipFile(path, "w") as archive:
                archive.writestr("safe/member", "payload")
                archive.writestr("safe/member", "payload")
        verify = _verify_zip

    with pytest.raises(ContractError, match="duplicate member safe/member"):
        verify(path, contract)


@pytest.mark.parametrize("archive_kind", ("tar", "zip"))
def test_symlink_members_are_rejected(tmp_path: Path, archive_kind: str) -> None:
    contract = ArchiveContract(f"probe.{archive_kind}", ("safe/member",))
    path = tmp_path / contract.filename
    if archive_kind == "tar":
        with tarfile.open(path, "w:gz") as archive:
            info = tarfile.TarInfo("safe/member")
            info.type = tarfile.SYMTYPE
            info.linkname = "elsewhere"
            archive.addfile(info)
        verify = _verify_tar
    else:
        info = zipfile.ZipInfo("safe/member")
        info.create_system = 3
        info.external_attr = (stat.S_IFLNK | 0o777) << 16
        with zipfile.ZipFile(path, "w") as archive:
            archive.writestr(info, "elsewhere")
        verify = _verify_zip

    with pytest.raises(ContractError, match="unsupported member type safe/member"):
        verify(path, contract)


def test_zip_rejects_corrupt_payload_crc(tmp_path: Path) -> None:
    contract = ArchiveContract("probe.zip", ("safe/member",))
    path = tmp_path / contract.filename
    with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_STORED) as archive:
        archive.writestr("safe/member", b"payload")
    content = path.read_bytes()
    assert content.count(b"payload") == 1
    path.write_bytes(content.replace(b"payload", b"qayload", 1))

    with pytest.raises(ContractError, match="ZIP integrity|corrupt ZIP"):
        _verify_zip(path, contract)


def test_zip_rejects_corrupt_unexpected_payload_crc(tmp_path: Path) -> None:
    contract = ArchiveContract("probe.zip", ("safe/member",))
    path = tmp_path / contract.filename
    with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_STORED) as archive:
        archive.writestr("safe/member", b"expected")
        archive.writestr("safe/extra", b"corrupt-extra")
    content = path.read_bytes()
    assert content.count(b"corrupt-extra") == 1
    path.write_bytes(content.replace(b"corrupt-extra", b"dorrupt-extra", 1))

    with pytest.raises(ContractError, match="ZIP integrity|corrupt ZIP"):
        _verify_zip(path, contract)


def test_zip_rejects_raw_nul_name_before_python_truncation(tmp_path: Path) -> None:
    contract = ArchiveContract("probe.zip", ("safe/member",))
    path = tmp_path / contract.filename
    placeholder = b"safe/memberXevil"
    raw_name = b"safe/member\x00evil"
    assert len(placeholder) == len(raw_name)
    with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_STORED) as archive:
        archive.writestr(placeholder.decode("ascii"), b"payload")
    content = path.read_bytes()
    assert content.count(placeholder) == 2
    path.write_bytes(content.replace(placeholder, raw_name))

    with zipfile.ZipFile(path) as archive:
        info = archive.infolist()[0]
        assert info.filename == "safe/member"
        assert info.orig_filename == "safe/member\x00evil"
    with pytest.raises(ContractError, match="unsafe archive member name"):
        _verify_zip(path, contract)


@pytest.mark.parametrize(
    ("expected_name", "alias_name"),
    (
        ("stage/cua-driver.exe", "stage/CUA-DRIVER.EXE"),
        ("stage/I.exe", "stage/ı.exe"),
    ),
)
def test_zip_rejects_windows_case_alias(
    tmp_path: Path,
    expected_name: str,
    alias_name: str,
) -> None:
    contract = ArchiveContract("probe.zip", (expected_name,))
    path = tmp_path / contract.filename
    with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_STORED) as archive:
        archive.writestr(expected_name, b"expected")
        archive.writestr(alias_name, b"alias")

    with pytest.raises(ContractError, match="target-filesystem path collision"):
        _verify_zip(path, contract)


@pytest.mark.parametrize(
    "unsafe_name",
    (
        "safe/trailing.",
        "safe/trailing ",
        "safe/name:stream",
        "safe/name?.txt",
        "stage/CUA-DR~1.EXE",
        "safe/CON.txt",
        "safe/CONOUT$",
        "safe/com¹.log",
        "safe/lpt9.log",
    ),
)
def test_zip_rejects_windows_unsafe_components(
    tmp_path: Path,
    unsafe_name: str,
) -> None:
    contract = ArchiveContract("probe.zip", ("safe/member",))
    path = tmp_path / contract.filename
    with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_STORED) as archive:
        archive.writestr("safe/member", b"expected")
        archive.writestr(unsafe_name, b"unsafe")

    with pytest.raises(ContractError, match="unsafe Windows archive member"):
        _verify_zip(path, contract)


@pytest.mark.parametrize(
    ("expected_name", "alias_name"),
    (
        ("safe/member", "SAFE/MEMBER"),
        ("safe/caf\N{LATIN SMALL LETTER E WITH ACUTE}", "safe/cafe\N{COMBINING ACUTE ACCENT}"),
    ),
)
def test_darwin_tar_rejects_case_and_unicode_aliases(
    tmp_path: Path,
    expected_name: str,
    alias_name: str,
) -> None:
    contract = ArchiveContract("probe-darwin-x86_64.tar.gz", (expected_name,))
    path = tmp_path / contract.filename
    with tarfile.open(path, "w:gz") as archive:
        for name in (expected_name, alias_name):
            info = tarfile.TarInfo(name)
            info.mode = 0o644
            info.size = len(b"payload")
            archive.addfile(info, BytesIO(b"payload"))

    with pytest.raises(ContractError, match="target-filesystem path collision"):
        _verify_tar(path, contract)
