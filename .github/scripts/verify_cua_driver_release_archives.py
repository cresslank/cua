#!/usr/bin/env python3
"""Verify that Cua Driver release archives satisfy the installer contract."""

from __future__ import annotations

import argparse
from dataclasses import dataclass
from pathlib import Path
import stat
import tarfile
import unicodedata
import zipfile


class ContractError(RuntimeError):
    """Raised when a release archive is missing or malformed."""


@dataclass(frozen=True)
class ArchiveContract:
    filename: str
    members: tuple[str, ...]
    executable_members: tuple[str, ...] = ()


def _wayland_helper_members(source: Path) -> tuple[str, ...]:
    """Return the exact helper payload produced from *source*."""

    if not source.is_dir() or source.is_symlink():
        raise ContractError(f"invalid Wayland helper source: {source}")

    members: list[str] = []
    for path in sorted(source.rglob("*")):
        if path.is_symlink():
            raise ContractError(f"Wayland helper source contains symlink: {path}")
        if path.is_dir():
            continue
        if not path.is_file():
            raise ContractError(f"Wayland helper source contains unsupported entry: {path}")
        relative = path.relative_to(source).as_posix()
        members.append(f"wayland-helper/{relative}")

    required = {
        "wayland-helper/install.sh",
        "wayland-helper/winrects@cua/extension.js",
        "wayland-helper/winrects@cua/metadata.json",
    }
    missing = sorted(required.difference(members))
    if missing:
        raise ContractError(
            f"Wayland helper source is missing required members: {', '.join(missing)}"
        )
    return tuple(members)


def release_contracts(
    version: str, wayland_helper_members: tuple[str, ...]
) -> tuple[ArchiveContract, ...]:
    """Return every archive and member required for a driver release."""

    contracts: list[ArchiveContract] = []

    linux_payload = (
        "cua-driver",
        "cua-cursor-theme",
        "libcua_driver_sdk.so",
        "cua_driver_node_runtime.node",
        "cua_driver_abi.h",
        *wayland_helper_members,
    )
    for arch in ("x86_64", "arm64"):
        stage = f"cua-driver-rs-{version}-linux-{arch}"
        contracts.extend(
            (
                ArchiveContract(
                    f"{stage}.tar.gz",
                    tuple(f"{stage}/{member}" for member in linux_payload),
                    (
                        f"{stage}/cua-driver",
                        f"{stage}/cua-cursor-theme",
                        f"{stage}/wayland-helper/install.sh",
                    ),
                ),
                ArchiveContract(
                    f"{stage}-binary.tar.gz",
                    linux_payload,
                    (
                        "cua-driver",
                        "cua-cursor-theme",
                        "wayland-helper/install.sh",
                    ),
                ),
            )
        )

    windows_payload = (
        "cua-driver.exe",
        "cua-cursor-theme.exe",
        "cua-driver-uia.exe",
        "cua_driver_sdk.dll",
        "cua_driver_node_runtime.node",
        "cua_driver_abi.h",
    )
    for arch in ("x86_64", "arm64"):
        stage = f"cua-driver-rs-{version}-windows-{arch}"
        contracts.extend(
            (
                ArchiveContract(
                    f"{stage}.zip",
                    tuple(f"{stage}/{member}" for member in windows_payload),
                ),
                ArchiveContract(f"{stage}-binary.zip", windows_payload),
            )
        )

    macos_payload = (
        "cua-driver",
        "cua-cursor-theme",
        "libcua_driver_sdk.dylib",
        "cua_driver_node_runtime.node",
        "cua_driver_abi.h",
        "CuaDriver.app/Contents/Info.plist",
        "CuaDriver.app/Contents/MacOS/cua-driver",
        "CuaDriver.app/Contents/MacOS/cua-cursor-theme",
    )
    for label in ("darwin-arm64", "darwin-x86_64", "darwin-universal"):
        stage = f"cua-driver-rs-{version}-{label}"
        contracts.append(
            ArchiveContract(
                f"{stage}.tar.gz",
                tuple(f"{stage}/{member}" for member in macos_payload),
                (
                    f"{stage}/cua-driver",
                    f"{stage}/cua-cursor-theme",
                    f"{stage}/CuaDriver.app/Contents/MacOS/cua-driver",
                    f"{stage}/CuaDriver.app/Contents/MacOS/cua-cursor-theme",
                ),
            )
        )

    contracts.append(
        ArchiveContract(
            f"cua-driver-rs-{version}-darwin-universal-binary.tar.gz",
            (
                "cua-driver",
                "cua-cursor-theme",
                "libcua_driver_sdk.dylib",
                "cua_driver_node_runtime.node",
                "cua_driver_abi.h",
            ),
            ("cua-driver", "cua-cursor-theme"),
        )
    )
    return tuple(contracts)


def _canonical_member(name: str, *, is_directory: bool) -> str:
    """Validate and return an extraction-faithful archive member name."""

    if (
        not name
        or "\x00" in name
        or "\\" in name
        or any(ord(character) < 32 or ord(character) == 127 for character in name)
    ):
        raise ContractError(f"unsafe archive member name: {name!r}")
    raw = name[:-1] if is_directory and name.endswith("/") else name
    if not raw or raw.startswith("/"):
        raise ContractError(f"unsafe archive member name: {name!r}")
    parts = raw.split("/")
    if any(part in ("", ".", "..") for part in parts):
        raise ContractError(f"non-canonical archive member name: {name!r}")
    if len(parts[0]) >= 2 and parts[0][0].isalpha() and parts[0][1] == ":":
        raise ContractError(f"unsafe archive member name: {name!r}")
    return "/".join(parts)


def _record_member(
    seen: dict[str, bool], name: str, *, is_directory: bool, archive_name: str
) -> str:
    canonical = _canonical_member(name, is_directory=is_directory)
    if canonical in seen:
        raise ContractError(f"{archive_name} contains duplicate member {canonical}")
    parts = canonical.split("/")
    for index in range(1, len(parts)):
        ancestor = "/".join(parts[:index])
        if ancestor in seen and not seen[ancestor]:
            raise ContractError(
                f"{archive_name} contains file/directory path collision "
                f"between {ancestor} and {canonical}"
            )
    if not is_directory:
        descendant_prefix = f"{canonical}/"
        descendant = next(
            (member for member in seen if member.startswith(descendant_prefix)), None
        )
        if descendant is not None:
            raise ContractError(
                f"{archive_name} contains file/directory path collision "
                f"between {canonical} and {descendant}"
            )
    seen[canonical] = is_directory
    return canonical


_WINDOWS_RESERVED_COMPONENTS = {
    "con",
    "prn",
    "aux",
    "nul",
    "conin$",
    "conout$",
    "clock$",
    *(f"com{index}" for index in range(1, 10)),
    *(f"lpt{index}" for index in range(1, 10)),
    *(f"com{index}" for index in ("¹", "²", "³")),
    *(f"lpt{index}" for index in ("¹", "²", "³")),
}


def _target_path_key(canonical: str, target: str) -> str:
    components = canonical.split("/")
    if target == "windows":
        for component in components:
            if (
                component.endswith((".", " "))
                or ":" in component
                or any(character in '<>"|?*' for character in component)
                # Fail closed on volume-dependent DOS 8.3 aliases.
                or "~" in component
            ):
                raise ContractError(
                    f"unsafe Windows archive member name: {canonical!r}"
                )
            stem = component.split(".", 1)[0].casefold()
            if stem in _WINDOWS_RESERVED_COMPONENTS:
                raise ContractError(
                    f"unsafe Windows archive member name: {canonical!r}"
                )
        # NTFS/Win32 comparisons are driven by an invariant uppercase table,
        # not full Unicode case-folding (for example, I aliases dotless ı).
        return "/".join(component.upper() for component in components)
    if target == "darwin":
        return unicodedata.normalize("NFD", canonical).casefold()
    return canonical


def _record_target_member(
    seen: dict[str, tuple[str, bool]],
    canonical: str,
    *,
    is_directory: bool,
    archive_name: str,
    target: str,
) -> None:
    key = _target_path_key(canonical, target)
    if key in seen:
        previous = seen[key][0]
        raise ContractError(
            f"{archive_name} contains target-filesystem path collision "
            f"between {previous} and {canonical}"
        )
    parts = key.split("/")
    for index in range(1, len(parts)):
        ancestor_key = "/".join(parts[:index])
        if ancestor_key in seen and not seen[ancestor_key][1]:
            raise ContractError(
                f"{archive_name} contains target-filesystem path collision "
                f"between {seen[ancestor_key][0]} and {canonical}"
            )
    if not is_directory:
        descendant_prefix = f"{key}/"
        descendant = next(
            (
                original
                for existing_key, (original, _) in seen.items()
                if existing_key.startswith(descendant_prefix)
            ),
            None,
        )
        if descendant is not None:
            raise ContractError(
                f"{archive_name} contains target-filesystem path collision "
                f"between {canonical} and {descendant}"
            )
    seen[key] = (canonical, is_directory)


def _find_archive(root: Path, filename: str) -> Path:
    matches = sorted(path for path in root.rglob(filename) if path.is_file())
    if not matches:
        raise ContractError(f"missing release archive: {filename}")
    if len(matches) != 1:
        rendered = ", ".join(str(path) for path in matches)
        raise ContractError(f"duplicate release archive {filename}: {rendered}")
    return matches[0]


def _verify_helper_closure(
    archive_name: str, members: set[str], contract: ArchiveContract
) -> None:
    marker = "wayland-helper/"
    helper_roots = {
        expected[: expected.index(marker) + len(marker)]
        for expected in contract.members
        if marker in expected
    }
    expected_members = set(contract.members)
    allowed_members = set(expected_members)
    for expected in expected_members:
        parts = expected.split("/")
        allowed_members.update("/".join(parts[:index]) for index in range(1, len(parts)))
    unexpected = sorted(
        member
        for member in members.difference(allowed_members)
        if any(member.startswith(root) for root in helper_roots)
    )
    if unexpected:
        raise ContractError(
            f"{archive_name} contains unexpected Wayland helper member "
            f"{unexpected[0]}"
        )


def _verify_tar(path: Path, contract: ArchiveContract) -> None:
    with tarfile.open(path, "r:gz") as archive:
        members: dict[str, tarfile.TarInfo] = {}
        directories: dict[str, tarfile.TarInfo] = {}
        seen: dict[str, bool] = {}
        target_seen: dict[str, tuple[str, bool]] = {}
        target = "darwin" if "-darwin-" in path.name else "posix"
        for member in archive.getmembers():
            canonical = _record_member(
                seen,
                member.name,
                is_directory=member.isdir(),
                archive_name=path.name,
            )
            _record_target_member(
                target_seen,
                canonical,
                is_directory=member.isdir(),
                archive_name=path.name,
                target=target,
            )
            if member.isdir():
                directories[canonical] = member
                continue
            if not member.isfile():
                raise ContractError(
                    f"{path.name} contains unsupported member type {canonical}"
                )
            members[canonical] = member

        _verify_helper_closure(path.name, set(seen), contract)
        for expected in contract.members:
            parts = expected.split("/")
            for index in range(1, len(parts)):
                ancestor = "/".join(parts[:index])
                directory = directories.get(ancestor)
                if directory is not None and directory.mode & 0o500 != 0o500:
                    raise ContractError(
                        f"{path.name} contains owner-inaccessible directory {ancestor}"
                    )
            member = members.get(expected)
            if member is None:
                raise ContractError(f"{path.name} is missing {expected}")
            if member.size <= 0:
                raise ContractError(f"{path.name} contains empty member {expected}")
            if member.mode & 0o400 == 0:
                raise ContractError(
                    f"{path.name} contains owner-unreadable member {expected}"
                )

        for expected in contract.executable_members:
            member = members.get(expected)
            if member is None or member.mode & 0o500 != 0o500:
                raise ContractError(f"{path.name} contains non-executable member {expected}")


def _verify_zip(path: Path, contract: ArchiveContract) -> None:
    with zipfile.ZipFile(path) as archive:
        members: dict[str, zipfile.ZipInfo] = {}
        seen: dict[str, bool] = {}
        target_seen: dict[str, tuple[str, bool]] = {}
        for info in archive.infolist():
            raw_name = info.orig_filename
            canonical = _record_member(
                seen,
                raw_name,
                is_directory=info.is_dir(),
                archive_name=path.name,
            )
            _record_target_member(
                target_seen,
                canonical,
                is_directory=info.is_dir(),
                archive_name=path.name,
                target="windows",
            )
            unix_mode = (info.external_attr >> 16) & 0xFFFF
            file_type = stat.S_IFMT(unix_mode)
            if info.is_dir():
                if file_type not in (0, stat.S_IFDIR):
                    raise ContractError(
                        f"{path.name} contains unsupported member type {canonical}"
                    )
                continue
            if file_type not in (0, stat.S_IFREG):
                raise ContractError(
                    f"{path.name} contains unsupported member type {canonical}"
                )
            members[canonical] = info

        _verify_helper_closure(path.name, set(seen), contract)
        try:
            corrupt_member = archive.testzip()
        except (OSError, RuntimeError, zipfile.BadZipFile) as error:
            raise ContractError(f"{path.name} failed ZIP integrity validation") from error
        if corrupt_member is not None:
            raise ContractError(
                f"{path.name} contains corrupt ZIP member {corrupt_member}"
            )
        for expected in contract.members:
            member = members.get(expected)
            if member is None:
                raise ContractError(f"{path.name} is missing {expected}")
            if member.file_size <= 0:
                raise ContractError(f"{path.name} contains empty member {expected}")


def verify_release_archives(
    root: Path, version: str, wayland_helper_source: Path
) -> tuple[Path, ...]:
    """Verify all archives for *version* below *root*."""

    verified: list[Path] = []
    helper_members = _wayland_helper_members(wayland_helper_source)
    for contract in release_contracts(version, helper_members):
        path = _find_archive(root, contract.filename)
        if path.name.endswith(".tar.gz"):
            _verify_tar(path, contract)
        elif path.suffix == ".zip":
            _verify_zip(path, contract)
        else:  # pragma: no cover - contracts above define supported formats.
            raise ContractError(f"unsupported archive format: {path}")
        verified.append(path)
    return tuple(verified)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--artifacts", type=Path, required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--wayland-helper-source", type=Path, required=True)
    args = parser.parse_args()

    try:
        verified = verify_release_archives(
            args.artifacts, args.version, args.wayland_helper_source
        )
    except (ContractError, tarfile.TarError, zipfile.BadZipFile) as error:
        parser.error(str(error))

    print(f"Verified {len(verified)} Cua Driver release archives:")
    for path in verified:
        print(f"  - {path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
