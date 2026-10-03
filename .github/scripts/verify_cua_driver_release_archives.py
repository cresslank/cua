#!/usr/bin/env python3
"""Verify that Cua Driver release archives satisfy the installer contract."""

from __future__ import annotations

import argparse
from dataclasses import dataclass
import gzip
from pathlib import Path, PurePosixPath
import stat
import tarfile
import unicodedata
import zipfile


FORBIDDEN_MODEL_SUFFIXES = (".onnx", ".ort", ".gguf", ".safetensors")
AGPL_MARKERS = (b"agpl-", b"agpl ", b"gnu affero general public license")
NOTICE_NAMES = ("license", "notice", "copying")
FORBIDDEN_BINARY_MARKERS = (
    b"onnxruntime",
    b"OrtGetApiBase",
    b"onnxruntime_providers",
)
MAX_BINARY_SIZE = 128 * 1024 * 1024
MAX_ARCHIVE_UNCOMPRESSED_SIZE = 512 * 1024 * 1024

BINARY_MAGIC = {
    "elf": (b"\x7fELF",),
    "pe": (b"MZ",),
    "mach-o": (
        b"\xca\xfe\xba\xbe",
        b"\xca\xfe\xba\xbf",
        b"\xce\xfa\xed\xfe",
        b"\xcf\xfa\xed\xfe",
        b"\xfe\xed\xfa\xce",
        b"\xfe\xed\xfa\xcf",
    ),
}


class ContractError(RuntimeError):
    """Raised when a release archive is missing or malformed."""


_MAX_UNCOMPRESSED_TAR_BYTES = 1 << 30


@dataclass(frozen=True)
class ArchiveContract:
    filename: str
    members: tuple[str, ...]
    executable_members: tuple[str, ...] = ()
    optional_members: tuple[str, ...] = ()
    binary_members: tuple[str, ...] = ()
    binary_format: str = ""


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

    linux_runtime = (
        "cua-driver",
        "cua-cursor-theme",
        "libcua_driver_sdk.so",
        "cua_driver_node_runtime.node",
        "cua_driver_abi.h",
    )
    linux_payload = linux_runtime + wayland_helper_members
    linux_binaries = linux_runtime[:-1]
    for arch in ("x86_64", "arm64"):
        stage = f"cua-driver-rs-{version}-linux-{arch}"
        contracts.extend(
            (
                ArchiveContract(
                    f"{stage}.tar.gz",
                    tuple(f"{stage}/{member}" for member in (*linux_payload, "LICENSE", "THIRD_PARTY_NOTICES.md")),
                    (
                        f"{stage}/cua-driver",
                        f"{stage}/cua-cursor-theme",
                        f"{stage}/wayland-helper/install.sh",
                    ),
                    binary_members=tuple(
                        f"{stage}/{member}" for member in linux_binaries
                    ),
                    binary_format="elf",
                ),
                ArchiveContract(
                    f"{stage}-binary.tar.gz",
                    linux_payload,
                    (
                        "cua-driver",
                        "cua-cursor-theme",
                        "wayland-helper/install.sh",
                    ),
                    binary_members=linux_binaries,
                    binary_format="elf",
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
                    tuple(f"{stage}/{member}" for member in (*windows_payload, "LICENSE", "THIRD_PARTY_NOTICES.md")),
                    binary_members=tuple(
                        f"{stage}/{member}" for member in windows_payload[:-1]
                    ),
                    binary_format="pe",
                ),
                ArchiveContract(
                    f"{stage}-binary.zip",
                    windows_payload,
                    binary_members=windows_payload[:-1],
                    binary_format="pe",
                ),
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
        "CuaDriver.app/Contents/Resources/AppIcon.icns",
    )
    macos_optional = (
        "CuaDriver.app/Contents/_CodeSignature/CodeResources",
        "CuaDriver.app/Contents/CodeResources",
        "CuaDriver.app/Contents/embedded.provisionprofile",
    )
    macos_binaries = (
        "cua-driver",
        "cua-cursor-theme",
        "libcua_driver_sdk.dylib",
        "cua_driver_node_runtime.node",
        "CuaDriver.app/Contents/MacOS/cua-driver",
        "CuaDriver.app/Contents/MacOS/cua-cursor-theme",
    )
    for label in ("darwin-arm64", "darwin-x86_64", "darwin-universal"):
        stage = f"cua-driver-rs-{version}-{label}"
        contracts.append(
            ArchiveContract(
                f"{stage}.tar.gz",
                tuple(f"{stage}/{member}" for member in (*macos_payload, "LICENSE", "THIRD_PARTY_NOTICES.md")),
                (
                    f"{stage}/cua-driver",
                    f"{stage}/cua-cursor-theme",
                    f"{stage}/CuaDriver.app/Contents/MacOS/cua-driver",
                    f"{stage}/CuaDriver.app/Contents/MacOS/cua-cursor-theme",
                ),
                tuple(f"{stage}/{member}" for member in macos_optional),
                tuple(f"{stage}/{member}" for member in macos_binaries),
                "mach-o",
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
            binary_members=(
                "cua-driver",
                "cua-cursor-theme",
                "libcua_driver_sdk.dylib",
                "cua_driver_node_runtime.node",
            ),
            binary_format="mach-o",
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


def _forbidden_member_reason(name: str) -> str | None:
    normalized = name.lower()
    path = f"/{normalized.strip('/')}"
    basename = PurePosixPath(normalized).name
    if "cua-perception" in normalized:
        return "cua-perception worker"
    if "/models/" in f"{path}/" or basename == "models":
        return "model directory"
    if basename.endswith(FORBIDDEN_MODEL_SUFFIXES):
        return "model payload"
    if "onnxruntime" in normalized:
        return "ONNX Runtime"
    if "catalog" in basename:
        return "extension catalog"
    if "agpl" in normalized:
        return "AGPL notice"
    return None


def _verify_member_names(path: Path, names: tuple[str, ...]) -> None:
    for name in names:
        if reason := _forbidden_member_reason(name):
            raise ContractError(
                f"{path.name} contains forbidden optional perception payload "
                f"({reason}): {name}"
            )


def _verify_notice_content(path: Path, name: str, payload: bytes) -> None:
    basename = PurePosixPath(name).name.lower()
    if not any(token in basename for token in NOTICE_NAMES):
        return
    lowered = payload.lower()
    if any(marker in lowered for marker in AGPL_MARKERS):
        raise ContractError(
            f"{path.name} contains forbidden optional perception payload "
            f"(AGPL notice): {name}"
        )


def _allowed_directories(names: set[str]) -> set[str]:
    directories: set[str] = set()
    for name in names:
        parent = PurePosixPath(name).parent
        while str(parent) != ".":
            directories.add(str(parent))
            parent = parent.parent
    return directories


def _verify_exact_members(
    path: Path,
    contract: ArchiveContract,
    file_names: tuple[str, ...],
    directory_names: tuple[str, ...],
) -> None:
    required = set(contract.members)
    allowed = required | set(contract.optional_members)
    actual = set(file_names)
    missing = sorted(required - actual)
    if missing:
        raise ContractError(f"{path.name} is missing {missing[0]}")
    unexpected = sorted(actual - allowed)
    if unexpected:
        raise ContractError(f"{path.name} contains unexpected member {unexpected[0]}")
    unexpected_directories = sorted(set(directory_names) - _allowed_directories(allowed))
    if unexpected_directories:
        raise ContractError(
            f"{path.name} contains unexpected directory {unexpected_directories[0]}"
        )
    if len(file_names) != len(actual):
        raise ContractError(f"{path.name} contains duplicate file members")


def _verify_binary(path: Path, contract: ArchiveContract, name: str, payload: bytes) -> None:
    if len(payload) > MAX_BINARY_SIZE:
        raise ContractError(
            f"{path.name} binary exceeds {MAX_BINARY_SIZE} bytes: {name}"
        )
    magic = BINARY_MAGIC[contract.binary_format]
    if not payload.startswith(magic):
        raise ContractError(
            f"{path.name} member is not {contract.binary_format} binary: {name}"
        )
    lowered = payload.lower()
    for marker in FORBIDDEN_BINARY_MARKERS:
        if marker.lower() in lowered:
            raise ContractError(
                f"{path.name} binary links or vendors optional perception runtime "
                f"({marker.decode()}): {name}"
            )




def _verify_tar(path: Path, contract: ArchiveContract) -> None:
    total = 0
    try:
        with gzip.open(path, "rb") as compressed:
            while True:
                chunk = compressed.read(1024 * 1024)
                if not chunk:
                    break
                total += len(chunk)
                if total > _MAX_UNCOMPRESSED_TAR_BYTES:
                    raise ContractError(
                        f"{path.name} exceeds the bounded uncompressed TAR size"
                    )
    except ContractError:
        raise
    except (gzip.BadGzipFile, EOFError, OSError) as error:
        raise ContractError(f"{path.name} failed gzip integrity verification") from error

    with tarfile.open(path, "r:gz") as archive:
        members: dict[str, tarfile.TarInfo] = {}
        directories: dict[str, tarfile.TarInfo] = {}
        seen: dict[str, bool] = {}
        target_seen: dict[str, tuple[str, bool]] = {}
        target = "darwin" if "-darwin-" in path.name else "posix"
        archive_members = archive.getmembers()
        _verify_member_names(path, tuple(member.name for member in archive_members))
        for member in archive_members:
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
        _verify_exact_members(path, contract, tuple(members), tuple(directories))
        total_size = sum(member.size for member in members.values())
        if total_size > MAX_ARCHIVE_UNCOMPRESSED_SIZE:
            raise ContractError(f"{path.name} uncompressed payload is too large: {total_size}")

        for name, member in members.items():
            if member.size <= 0:
                raise ContractError(f"{path.name} contains empty member {name}")
            if any(token in PurePosixPath(name).name.lower() for token in NOTICE_NAMES):
                extracted = archive.extractfile(member)
                if extracted is not None:
                    _verify_notice_content(path, name, extracted.read())

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

        for expected in contract.binary_members:
            extracted = archive.extractfile(members[expected])
            if extracted is None:  # pragma: no cover - member is already a regular file.
                raise ContractError(f"{path.name} cannot read binary member {expected}")
            _verify_binary(path, contract, expected, extracted.read())


def _verify_zip(path: Path, contract: ArchiveContract) -> None:
    with zipfile.ZipFile(path) as archive:
        members: dict[str, zipfile.ZipInfo] = {}
        seen: dict[str, bool] = {}
        target_seen: dict[str, tuple[str, bool]] = {}
        archive_members = archive.infolist()
        _verify_member_names(path, tuple(info.orig_filename for info in archive_members))
        for info in archive_members:
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
        total_size = sum(member.file_size for member in members.values())
        if total_size > MAX_ARCHIVE_UNCOMPRESSED_SIZE:
            raise ContractError(f"{path.name} uncompressed payload is too large: {total_size}")
        try:
            corrupt_member = archive.testzip()
        except (OSError, RuntimeError, zipfile.BadZipFile) as error:
            raise ContractError(f"{path.name} failed ZIP integrity validation") from error
        if corrupt_member is not None:
            raise ContractError(
                f"{path.name} contains corrupt ZIP member {corrupt_member}"
            )
        _verify_exact_members(path, contract, tuple(members), tuple(name for name, is_dir in seen.items() if is_dir))
        for name, member in members.items():
            if member.file_size <= 0:
                raise ContractError(f"{path.name} contains empty member {name}")
            if any(token in PurePosixPath(name).name.lower() for token in NOTICE_NAMES):
                _verify_notice_content(path, name, archive.read(member))

        for expected in contract.members:
            member = members.get(expected)
            if member is None:
                raise ContractError(f"{path.name} is missing {expected}")

        for expected in contract.binary_members:
            _verify_binary(path, contract, expected, archive.read(members[expected]))


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
