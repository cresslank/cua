#!/usr/bin/env bash
#
# cua-driver-rs local/debug installer (macOS + Linux). Builds an exact clean
# Git commit into a durable, separate local-product namespace.
#
# Private helper — invoked by install-local.sh (the multi-backend
# dispatcher) when the user picks --backend=rust / --experimental-rust
# or runs on a non-macOS host. Do not invoke directly; flag parity with
# the dispatcher's argv shape is maintained from there.
#
# Rust local installer (dev-only helper for libs/cua-driver/rust):
#   --release    build the release configuration (default: debug)
#                Linux builds include portal-input, matching release artifacts.
#   --autostart  register an auto-start daemon (macOS: LaunchAgent;
#                Linux: systemd user unit). Default off; the post-install
#                message prints the registration command for the platform.
#
# Not for end-users — scripts/install.sh fetches a built release from
# GitHub. This script is for the developer loop (rapid edit/build/test
# on a Linux or macOS host).
#
# Linux layout produced (matches install.sh):
#
#   ${CUA_DRIVER_LOCAL_HOME:-$HOME/.cua-driver-local}/packages/
#       releases/<version>-local-<config>-<target>/cua-driver-local
#       current/cua-driver-local
#   ${CUA_DRIVER_LOCAL_INSTALL_DIR:-$HOME/.local/bin}/cua-driver-local
#
# macOS layout produced:
#   /Applications/CuaDriverLocal.app/Contents/MacOS/cua-driver-local
#   $HOME/.local/bin/cua-driver-local -> .../CuaDriverLocal.app/Contents/MacOS/cua-driver-local
#
# The version string carries `-local-debug` / `-local-release` so it
# never collides with a real release dir and is trivial to GC.
set -euo pipefail

CUA_DRIVER_LOCAL_PROVENANCE_CONTRACT="v4-linux-bwrap-scoped-host-provenance"

# The immutable source/build path must not select release-critical host tools
# from caller-controlled PATH entries. These binaries are supplied by the
# root-owned system image and are also included in the recorded system-input
# identity below.
GIT_BIN=/usr/bin/git
BWRAP_BIN=/usr/bin/bwrap
CAPSH_BIN=
UNSHARE_BIN=/usr/bin/unshare
MOUNT_BIN=/usr/bin/mount
READLINK_BIN=/usr/bin/readlink
SHA256SUM_BIN=/usr/bin/sha256sum
CUT_BIN=/usr/bin/cut
TAR_BIN=/usr/bin/tar
PYTHON_BIN=/usr/bin/python3
RPM_BIN=/usr/bin/rpm
DPKG_QUERY_BIN=/usr/bin/dpkg-query
SORT_BIN=/usr/bin/sort
UNAME_BIN=/usr/bin/uname

SCRIPT_DIR="$(cd "$(/usr/bin/dirname "$0")" && /usr/bin/pwd)"
INSTALL_TRANSACTION_LOCK_HELPER="$SCRIPT_DIR/_install-transaction-lock.py"
LOCK_PYTHON="$PYTHON_BIN"
if [ -z "$LOCK_PYTHON" ] || [ ! -f "$INSTALL_TRANSACTION_LOCK_HELPER" ]; then
    echo "error: Python and $INSTALL_TRANSACTION_LOCK_HELPER are required for safe local installation" >&2
    exit 1
fi
# Acquire before inspecting mutable install state. A managed wrapper holds this
# same fixed host lock through acceptance/rollback and passes its locked fd to
# this child; validation avoids both deadlock and caller-forged inheritance.
if [ -n "${CUA_DRIVER_INSTALL_TRANSACTION_LOCK_FD:-}" ]; then
    "$LOCK_PYTHON" "$INSTALL_TRANSACTION_LOCK_HELPER" --validate
else
    exec "$LOCK_PYTHON" "$INSTALL_TRANSACTION_LOCK_HELPER" --acquire \
        /bin/bash "$SCRIPT_DIR/_install-local-rust.sh" "$@"
fi
# Rust workspace root: scripts/ is the cross-cutting installer dir at
# libs/cua-driver/scripts/; the Cargo workspace lives one level deeper
# under libs/cua-driver/rust/.
REPO_ROOT="$(cd "$SCRIPT_DIR/../rust" && pwd)"

# Never let caller Git variables, replacement refs, or global/system config
# redefine the commit selected for an attested build. Repository-local config
# remains available only for worktree discovery/status; the archive itself is
# produced later through a separate config-free object-database view.
source_git() {
    /usr/bin/env -i \
        PATH=/usr/bin:/bin \
        HOME=/nonexistent \
        LC_ALL=C \
        GIT_CONFIG_NOSYSTEM=1 \
        GIT_CONFIG_GLOBAL=/dev/null \
        GIT_NO_REPLACE_OBJECTS=1 \
        "$GIT_BIN" \
        -c core.attributesFile=/dev/null \
        -c core.hooksPath=/dev/null \
        "$@"
}

# Immutable local installation is supported only from an exact clean Git
# commit. Source snapshots, dirty labels, and caller opt-outs cannot establish
# the provenance required by the content-addressed release store.
SOURCE_GIT_ROOT="$(source_git -C "$REPO_ROOT" rev-parse --show-toplevel 2>/dev/null || true)"
if [ -z "$SOURCE_GIT_ROOT" ]; then
    echo "error: immutable local promotion requires a Git checkout" >&2
    exit 1
fi
SOURCE_HEAD="$(source_git -C "$SOURCE_GIT_ROOT" rev-parse --verify 'HEAD^{commit}' 2>/dev/null || true)"
if ! [[ "$SOURCE_HEAD" =~ ^[0-9a-f]{40}$ ]]; then
    echo "error: could not determine an exact 40-character Git OID for $SOURCE_GIT_ROOT" >&2
    exit 1
fi
if [ -z "${CUA_DRIVER_SOURCE_SHA:-}" ]; then
    CUA_DRIVER_SOURCE_SHA="$SOURCE_HEAD"
fi
if ! [[ "$CUA_DRIVER_SOURCE_SHA" =~ ^[0-9a-f]{40}$ ]]; then
    echo "error: CUA_DRIVER_SOURCE_SHA must be an exact 40-character Git OID" >&2
    exit 2
fi
case "${CUA_DRIVER_REQUIRE_CLEAN_SOURCE:-1}" in
    1|true|yes|"") CUA_DRIVER_REQUIRE_CLEAN_SOURCE=1 ;;
    *)
        echo "error: immutable local promotion cannot disable clean-source enforcement" >&2
        exit 2
        ;;
esac
export CUA_DRIVER_SOURCE_SHA CUA_DRIVER_REQUIRE_CLEAN_SOURCE

assert_clean_source() {
    local current_sha
    current_sha="$(source_git -C "$SOURCE_GIT_ROOT" rev-parse --verify 'HEAD^{commit}')"
    if [ "$CUA_DRIVER_SOURCE_SHA" != "$current_sha" ]; then
        echo "error: CUA_DRIVER_SOURCE_SHA does not match the checked-out commit" >&2
        exit 1
    fi
    if [ -n "$(source_git -C "$SOURCE_GIT_ROOT" status --porcelain=v1 --untracked-files=normal)" ]; then
        echo "error: refusing promotion from a dirty source tree" >&2
        exit 1
    fi
}
assert_clean_source

BOLD=$(tput bold 2>/dev/null || true)
NORMAL=$(tput sgr0 2>/dev/null || true)
RED=$(tput setaf 1 2>/dev/null || true)
GREEN=$(tput setaf 2 2>/dev/null || true)
BLUE=$(tput setaf 4 2>/dev/null || true)
YELLOW=$(tput setaf 3 2>/dev/null || true)

if [ "$(id -u)" -eq 0 ] || [ -n "${SUDO_USER:-}" ]; then
    echo "${RED}Error: do not run this script with sudo or as root.${NORMAL}"
    echo "It prompts for sudo on the specific operations that need it."
    exit 1
fi

# --- Parse arguments ----------------------------------------------------

BUILD_CONFIG="debug"
INSTALL_AUTOSTART=false
case "${CUA_DRIVER_REQUIRE_STABLE_SIGNING:-0}" in
    0|false|no|"") CUA_DRIVER_REQUIRE_STABLE_SIGNING=0 ;;
    1|true|yes) CUA_DRIVER_REQUIRE_STABLE_SIGNING=1 ;;
    *)
        echo "${RED}Error: CUA_DRIVER_REQUIRE_STABLE_SIGNING must be 0 or 1.${NORMAL}" >&2
        exit 2
        ;;
esac
export CUA_DRIVER_REQUIRE_STABLE_SIGNING

while [ "$#" -gt 0 ]; do
    case "$1" in
        --release)
            BUILD_CONFIG="release"
            ;;
        --autostart)
            INSTALL_AUTOSTART=true
            ;;
        --require-stable-signing)
            CUA_DRIVER_REQUIRE_STABLE_SIGNING=1
            export CUA_DRIVER_REQUIRE_STABLE_SIGNING
            ;;
        --help|-h)
            echo "${BOLD}${BLUE}cua-driver-rs local installer${NORMAL}"
            echo "Usage: $0 [OPTIONS]"
            echo ""
            echo "Options:"
            echo "  --release     Build the release configuration (default: debug)."
            echo "  --autostart   Also register a logon-time daemon:"
            echo "                  macOS: LaunchAgent under ~/Library/LaunchAgents"
            echo "                  Linux: systemd --user unit"
            echo "                On macOS this also fixes TCC: a launchd-started daemon"
            echo "                is attributed to com.trycua.driver.local (not your terminal),"
            echo "                so you grant Accessibility + Screen Recording once and"
            echo "                every cua-driver-local call/mcp routes through it correctly."
            echo "  --require-stable-signing"
            echo "                On macOS, stop before replacing the installed app unless"
            echo "                a certificate-backed identity is available. Recommended"
            echo "                for behavior and E2E verification."
            echo "  --help        Show this help."
            echo ""
            echo "Examples:"
            echo "  $0                       # debug build, install junction layout"
            echo "  $0 --release             # release build"
            echo "  $0 --release --autostart # release + daemon at logon"
            exit 0
            ;;
        *)
            echo "${RED}Unknown option: $1${NORMAL}"
            echo "Use --help for usage."
            exit 1
            ;;
    esac
    shift
done

OS="$("$UNAME_BIN" -s)"
ARCH="$("$UNAME_BIN" -m)"
if [ "${CUA_DRIVER_INSTALL_TRANSACTION_LOCK_TESTING:-}" = 1 ]; then
    OS="${CUA_DRIVER_TEST_OS:-$OS}"
    ARCH="${CUA_DRIVER_TEST_ARCH:-$ARCH}"
elif [ -n "${CUA_DRIVER_TEST_OS:-}${CUA_DRIVER_TEST_ARCH:-}" ]; then
    echo "error: test-only platform overrides require transaction-lock testing mode" >&2
    exit 2
fi
case "$OS" in
    Darwin)
        echo "${RED}Error: hardened local promotion is temporarily unavailable on macOS until signed app publication is fully content-addressed and crash-atomic.${NORMAL}" >&2
        exit 1
        ;;
    Linux)  TARGET_TRIPLE="${ARCH}-unknown-linux-gnu" ;;
    *)      echo "${RED}Unsupported OS: $OS${NORMAL}"; exit 1 ;;
esac

if [ -f /var/lib/dpkg/status ] && [ -x "$DPKG_QUERY_BIN" ]; then
    PACKAGE_DATABASE_KIND=dpkg
    PACKAGE_QUERY_BIN="$DPKG_QUERY_BIN"
elif [ -x "$RPM_BIN" ]; then
    PACKAGE_DATABASE_KIND=rpm
    PACKAGE_QUERY_BIN="$RPM_BIN"
else
    echo "error: immutable local promotion requires an RPM or dpkg package database" >&2
    exit 1
fi

for capsh_candidate in /usr/bin/capsh /usr/sbin/capsh; do
    if [ -x "$capsh_candidate" ]; then
        CAPSH_BIN="$capsh_candidate"
        break
    fi
done
if [ -z "$CAPSH_BIN" ]; then
    echo "error: immutable local promotion requires the libcap capsh utility" >&2
    exit 1
fi

for trusted_tool in "$GIT_BIN" "$BWRAP_BIN" "$CAPSH_BIN" "$UNSHARE_BIN" "$MOUNT_BIN" "$READLINK_BIN" "$SHA256SUM_BIN" "$CUT_BIN" "$TAR_BIN" "$PYTHON_BIN" "$PACKAGE_QUERY_BIN" "$SORT_BIN" "$UNAME_BIN"; do
    if [ ! -f "$trusted_tool" ] || [ ! -x "$trusted_tool" ] || [ "$(/usr/bin/stat -Lc '%u' "$trusted_tool")" != 0 ]; then
        echo "error: immutable local promotion requires trusted system tool $trusted_tool" >&2
        exit 1
    fi
    trusted_mode="$(/usr/bin/stat -Lc '%a' "$trusted_tool")"
    if (( (8#$trusted_mode & 8#022) != 0 )); then
        echo "error: trusted system tool is group/world writable: $trusted_tool" >&2
        exit 1
    fi
done

CARGO_FEATURE_ARGS=()
if [ "$OS" = "Linux" ]; then
    # Ordinary Linux release artifacts include RemoteDesktop/libei input for
    # GNOME and KDE. Keep local installs behaviorally equivalent instead of
    # silently producing a version-identical binary with no native input path.
    CARGO_FEATURE_ARGS=(--features portal-input)
else
    echo "${RED}Error: immutable local source promotion is currently supported only on Linux.${NORMAL}" >&2
    echo "Use the published macOS installer until a read-only APFS/DMG build transaction is available." >&2
    exit 1
fi

HOME_DIR="${CUA_DRIVER_LOCAL_HOME:-$HOME/.cua-driver-local}"
BIN_DIR="${CUA_DRIVER_LOCAL_INSTALL_DIR:-$HOME/.local/bin}"
RELEASES_DIR="$HOME_DIR/packages/releases"
CURRENT_LINK="$HOME_DIR/packages/current"
if { [ -e "$HOME_DIR/packages" ] || [ -L "$HOME_DIR/packages" ]; } \
   && { [ ! -d "$HOME_DIR/packages" ] || [ -L "$HOME_DIR/packages" ]; }; then
    echo "error: package root is not a real directory: $HOME_DIR/packages" >&2
    exit 1
fi
mkdir -p "$HOME_DIR/packages"
chmod 700 "$HOME_DIR/packages"
if { [ -e "$CURRENT_LINK" ] || [ -L "$CURRENT_LINK" ]; } && [ ! -L "$CURRENT_LINK" ]; then
    echo "error: release selector exists but is not a symlink: $CURRENT_LINK" >&2
    exit 1
fi
if [ -e "$BIN_DIR/cua-driver-local" ] && [ -d "$BIN_DIR/cua-driver-local" ] \
   && [ ! -L "$BIN_DIR/cua-driver-local" ]; then
    echo "error: visible binary path exists as a real directory: $BIN_DIR/cua-driver-local" >&2
    exit 1
fi

STAGING_VERSIONED_DIR=""
CURRENT_LINK_TMP=""
BIN_LINK_TMP=""
BUILD_EXPORT_DIR=""
BUILD_REPO_ROOT=""
BUILD_SCRIPT_DIR=""
BUILD_TARGET_DIR=""
SOURCE_IDENTITY_GIT_DIR=""
APP_STAGE=""
APP_DEST=""
APP_BACKUP=""
APP_TRANSACTION_ACTIVE=false
cleanup_install_transaction() {
    if [ -n "${CURRENT_LINK_TMP:-}" ]; then
        rm -f "$CURRENT_LINK_TMP"
    fi
    if [ -n "${BIN_LINK_TMP:-}" ]; then
        rm -f "$BIN_LINK_TMP"
    fi
    if [ "${APP_TRANSACTION_ACTIVE:-false}" = true ]; then
        rm -rf "${APP_DEST:-}"
        if [ -n "${APP_BACKUP:-}" ] && [ -d "$APP_BACKUP" ]; then
            mv "$APP_BACKUP" "$APP_DEST"
        fi
    fi
    if [ -n "${STAGING_VERSIONED_DIR:-}" ] && [ -d "$STAGING_VERSIONED_DIR" ]; then
        chmod -R u+w "$STAGING_VERSIONED_DIR" 2>/dev/null || true
        rm -rf "$STAGING_VERSIONED_DIR"
    fi
    if [ -n "${BUILD_EXPORT_DIR:-}" ] && [ -d "$BUILD_EXPORT_DIR" ]; then
        rm -rf "$BUILD_EXPORT_DIR"
    fi
    if [ -n "${BUILD_TARGET_DIR:-}" ] && [ -d "$BUILD_TARGET_DIR" ]; then
        rm -rf "$BUILD_TARGET_DIR"
    fi
    if [ -n "${SOURCE_IDENTITY_GIT_DIR:-}" ] && [ -d "$SOURCE_IDENTITY_GIT_DIR" ]; then
        rm -rf "$SOURCE_IDENTITY_GIT_DIR"
    fi
    if [ -n "${APP_STAGE:-}" ] && [ -d "$APP_STAGE" ]; then
        rm -rf "$APP_STAGE"
    fi
}
trap cleanup_install_transaction EXIT

VERSION_TAG="0.0.0-local-$BUILD_CONFIG"
# Stage away from every installed release on both platforms. Promotion below
# is content-addressed and never overwrites a path reachable through `current`.
VERSIONED_DIR="$RELEASES_DIR/.staging-$VERSION_TAG-$TARGET_TRIPLE-$$"
STAGING_VERSIONED_DIR="$VERSIONED_DIR"
rm -rf "$VERSIONED_DIR"

echo "${BOLD}${BLUE}cua-driver-rs local installer${NORMAL}"
echo "  source:  ${BOLD}$REPO_ROOT${NORMAL}"
echo "  sha:     ${BOLD}$CUA_DRIVER_SOURCE_SHA${NORMAL}"
echo "  config:  ${BOLD}$BUILD_CONFIG${NORMAL}"
echo "  target:  ${BOLD}$TARGET_TRIPLE${NORMAL}"
if [ "${#CARGO_FEATURE_ARGS[@]}" -gt 0 ]; then
    echo "  features: ${BOLD}portal-input${NORMAL}"
fi
echo "  bin:     ${BOLD}$BIN_DIR/cua-driver-local${NORMAL}"
echo "  current: ${BOLD}$CURRENT_LINK${NORMAL}"
echo ""

# --- Prerequisites ------------------------------------------------------

# Resolve the two explicitly attested user-managed inputs before restricting
# all remaining release-critical command lookup to the root-owned system PATH.
CALLER_PATH="$PATH"
SFW_COMMAND="$(PATH="$CALLER_PATH" command -v sfw 2>/dev/null || true)"
RUSTUP_COMMAND="$(PATH="$CALLER_PATH" command -v rustup 2>/dev/null || true)"
if [ -z "$SFW_COMMAND" ]; then
    echo "${RED}Error: sfw is required for the immutable Cargo dependency fetch.${NORMAL}" >&2
    exit 1
fi
if [ -z "$RUSTUP_COMMAND" ] || [ ! -x "$RUSTUP_COMMAND" ]; then
    echo "${RED}Error: rustup is required to select the attested Rust toolchain.${NORMAL}" >&2
    exit 1
fi
PATH=/usr/bin:/bin
export PATH
LC_ALL=C
LANG=C
TZ=UTC
export LC_ALL LANG TZ

# --- Build --------------------------------------------------------------

# Derive archive-relative paths before entering the isolated build namespace.
REPO_ROOT_RELATIVE="${REPO_ROOT#"$SOURCE_GIT_ROOT"/}"
SCRIPT_DIR_RELATIVE="${SCRIPT_DIR#"$SOURCE_GIT_ROOT"/}"
if [ "$REPO_ROOT_RELATIVE" = "$REPO_ROOT" ] || [ "$SCRIPT_DIR_RELATIVE" = "$SCRIPT_DIR" ]; then
    echo "${RED}Error: installer source paths are outside the attested Git checkout.${NORMAL}" >&2
    exit 1
fi

# Never reuse caller-controlled Cargo artifacts for an attested release. Use a
# fresh owned subdirectory beneath the requested target root, then remove it at
# exit. This retains target-root placement without trusting cached build bytes.
BUILD_TARGET_ROOT="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"
case "$BUILD_TARGET_ROOT" in
    /*) ;;
    *) BUILD_TARGET_ROOT="$REPO_ROOT/$BUILD_TARGET_ROOT" ;;
esac
mkdir -p "$BUILD_TARGET_ROOT"
BUILD_TARGET_DIR="$(mktemp -d "$BUILD_TARGET_ROOT/.cua-immutable-$CUA_DRIVER_SOURCE_SHA-XXXXXX")"
BUILD_EXPORT_DIR="$(mktemp -d "$HOME_DIR/packages/.cua-build-export-$CUA_DRIVER_SOURCE_SHA-XXXXXX")"
BUILD_REPO_ROOT="$BUILD_EXPORT_DIR/repo"
BUILD_SCRIPT_DIR="$BUILD_EXPORT_DIR/scripts"

# Resolve source objects with replacements disabled, then expose only that
# content-addressed object database to a fresh bare Git directory. The fresh
# directory has no refs/replace, local config, or info/attributes, so mutable
# checkout metadata cannot alter the commit tree or archive membership while
# the original object hashes remain authoritative.
SOURCE_OBJECTS_DIR="$(source_git -C "$SOURCE_GIT_ROOT" rev-parse --path-format=absolute --git-path objects)"
if [ ! -d "$SOURCE_OBJECTS_DIR" ]; then
    echo "error: attested Git object database is unavailable: $SOURCE_OBJECTS_DIR" >&2
    exit 1
fi
SOURCE_IDENTITY_GIT_DIR="$(mktemp -d "$HOME_DIR/packages/.cua-source-identity-$CUA_DRIVER_SOURCE_SHA-XXXXXX")"
mkdir -p "$SOURCE_IDENTITY_GIT_DIR/objects/info" "$SOURCE_IDENTITY_GIT_DIR/refs"
printf '%s\n' "$SOURCE_OBJECTS_DIR" > "$SOURCE_IDENTITY_GIT_DIR/objects/info/alternates"
printf 'ref: refs/heads/attested\n' > "$SOURCE_IDENTITY_GIT_DIR/HEAD"
cat > "$SOURCE_IDENTITY_GIT_DIR/config" <<'GIT_CONFIG'
[core]
    bare = true
    attributesFile = /dev/null
GIT_CONFIG
identity_git() {
    /usr/bin/env -i \
        PATH=/usr/bin:/bin \
        HOME=/nonexistent \
        LC_ALL=C \
        GIT_CONFIG_NOSYSTEM=1 \
        GIT_CONFIG_GLOBAL=/dev/null \
        GIT_NO_REPLACE_OBJECTS=1 \
        "$GIT_BIN" --git-dir="$SOURCE_IDENTITY_GIT_DIR" "$@"
}
if ! identity_git cat-file -e "$CUA_DRIVER_SOURCE_SHA^{commit}"; then
    echo "error: selected source commit is unavailable in the isolated object database" >&2
    exit 1
fi

# The workstation package-manager policy is part of the fetch boundary. `/run`
# is deliberately replaced inside Bubblewrap, so attest and copy only the
# resolved native SFW executable into private tmpfs. Never set SFW_BYPASS here.
SFW_SOURCE="$("$READLINK_BIN" -f "$SFW_COMMAND")"
case "$SFW_SOURCE" in
    *.mjs)
        SFW_INSTALL_ROOT="$(dirname "$(dirname "$SFW_SOURCE")")"
        SFW_SOURCE="$("$READLINK_BIN" -f "$SFW_INSTALL_ROOT/.sfw-cache/latest" 2>/dev/null || true)"
        ;;
esac
if [ ! -f "$SFW_SOURCE" ] || [ ! -x "$SFW_SOURCE" ]; then
    echo "${RED}Error: active sfw policy has no cached native executable to pin.${NORMAL}" >&2
    exit 1
fi
SFW_SOURCE_SHA256="$("$SHA256SUM_BIN" "$SFW_SOURCE" | "$CUT_BIN" -d' ' -f1)"

# Rustup toolchains are user-writable and therefore cannot be consumed through
# the host's ambient PATH while claiming an immutable build. Snapshot the
# complete selected toolchain into namespace-private tmpfs, attest the copied
# tree, and expose only that copy plus system-owned compiler utilities.
TOOLCHAIN_CARGO="$(PATH="$CALLER_PATH" "$RUSTUP_COMMAND" which cargo 2>/dev/null || true)"
case "$TOOLCHAIN_CARGO" in
    /*) ;;
    *)
        echo "${RED}Error: rustup did not return an absolute Cargo path.${NORMAL}" >&2
        exit 1
        ;;
esac
TOOLCHAIN_ROOT="$(dirname "$(dirname "$TOOLCHAIN_CARGO")")"
for tool in cargo rustc rustdoc; do
    if [ ! -x "$TOOLCHAIN_ROOT/bin/$tool" ]; then
        echo "${RED}Error: selected Rust toolchain is incomplete: $TOOLCHAIN_ROOT/bin/$tool${NORMAL}" >&2
        exit 1
    fi
done
hash_toolchain_tree() {
    "$TAR_BIN" --sort=name --mtime='UTC 1970-01-01' --owner=0 --group=0 \
        --numeric-owner --format=posix \
        --pax-option=delete=atime,delete=ctime \
        -C "$1" -cf - . | "$SHA256SUM_BIN" | "$CUT_BIN" -d' ' -f1
}
TOOLCHAIN_TREE_SHA256="$(hash_toolchain_tree "$TOOLCHAIN_ROOT")"

hash_system_build_inputs() {
    {
        printf 'kernel\t'
        "$UNAME_BIN" -srvmo
        if [ "$PACKAGE_DATABASE_KIND" = rpm ]; then
            /usr/bin/env -i PATH=/usr/bin:/bin LC_ALL=C HOME=/nonexistent \
                "$PACKAGE_QUERY_BIN" -qa --qf 'rpm\t%{NAME}\t%{EPOCHNUM}:%{VERSION}-%{RELEASE}.%{ARCH}\n' \
                | "$SORT_BIN"
        else
            /usr/bin/env -i PATH=/usr/bin:/bin LC_ALL=C HOME=/nonexistent \
                "$PACKAGE_QUERY_BIN" -W -f='dpkg\t${binary:Package}\t${Version}\t${Architecture}\n' \
                | "$SORT_BIN"
        fi
        for system_input in \
            "$GIT_BIN" "$BWRAP_BIN" "$SHA256SUM_BIN" "$CUT_BIN" \
            "$TAR_BIN" "$PYTHON_BIN" "$PACKAGE_QUERY_BIN" "$SORT_BIN" \
            /usr/bin/bash "$CAPSH_BIN" "$UNSHARE_BIN" "$MOUNT_BIN" "$READLINK_BIN" \
            /usr/bin/cc /usr/bin/c++ /usr/bin/pkg-config \
            /etc/ld.so.cache /etc/nsswitch.conf; do
            if [ -e "$system_input" ]; then
                printf 'file\t%s\t' "$system_input"
                "$SHA256SUM_BIN" "$system_input" | "$CUT_BIN" -d' ' -f1
            fi
        done
    } | "$SHA256SUM_BIN" | "$CUT_BIN" -d' ' -f1
}
SYSTEM_BUILD_INPUTS_SHA256="$(hash_system_build_inputs)"

# `/etc/resolv.conf` points into `/run` on the supported Linux host, which is
# hidden below. Bind back only the resolver file—not the runtime tree or its
# Unix sockets.
RESOLV_SOURCE="$("$READLINK_BIN" -f /etc/resolv.conf)"
case "$RESOLV_SOURCE" in
    /etc/resolv.conf|/run/*) ;;
    *)
        echo "${RED}Error: unsupported resolver target for the private build namespace: $RESOLV_SOURCE${NORMAL}" >&2
        exit 1
        ;;
esac
if [ ! -f "$RESOLV_SOURCE" ]; then
    echo "${RED}Error: resolver target is not a regular file: $RESOLV_SOURCE${NORMAL}" >&2
    exit 1
fi
resolver_mode="$(/usr/bin/stat -Lc '%a' "$RESOLV_SOURCE")"
if (( (8#$resolver_mode & 8#022) != 0 )); then
    echo "${RED}Error: resolver target is group/world writable: $RESOLV_SOURCE${NORMAL}" >&2
    exit 1
fi

BWRAP_SYSTEM_ARGS=(
    --ro-bind /usr /usr
    --symlink usr/bin /bin
    --symlink usr/sbin /sbin
    --symlink usr/lib /lib
    --symlink usr/lib64 /lib64
    --dir /etc
)
for system_path in /etc/ssl /etc/pki /etc/hosts /etc/passwd /etc/group /etc/ld.so.cache /etc/nsswitch.conf; do
    if [ -e "$system_path" ]; then
        BWRAP_SYSTEM_ARGS+=(--ro-bind "$system_path" "$system_path")
    fi
done

echo "${BOLD}Building cua-driver ($BUILD_CONFIG)...${NORMAL}"
# `git archive` authenticates every source blob against the attested commit.
# The archive and a freshly fetched locked dependency graph are materialized in
# namespace-private tmpfs, bind-remounted read-only, and then built offline in a
# second network namespace after CAP_SYS_ADMIN is dropped. Build scripts and
# concurrent host processes therefore cannot change or fetch any consumed
# source, Skills, signing helper, bundle skeleton, or post-install hints.
identity_git archive --format=tar "$CUA_DRIVER_SOURCE_SHA" \
    | "$BWRAP_BIN" \
        --unshare-all \
        --share-net \
        --uid 0 \
        --gid 0 \
        --die-with-parent \
        "${BWRAP_SYSTEM_ARGS[@]}" \
        --ro-bind "$RESOLV_SOURCE" /etc/resolv.conf \
        --dev /dev \
        --proc /proc \
        --tmpfs /run \
        --dir /run/cua-input \
        --dir /run/cua-source \
        --dir /run/cua-output \
        --dir /run/cua-export \
        --dir /run/cua-tmp \
        --dir /run/cua-cargo-home \
        --dir /run/cua-vendor \
        --dir /run/cua-tools \
        --dir /run/cua-toolchain \
        --ro-bind "$SFW_SOURCE" /run/cua-input/sfw-source \
        --ro-bind "$TOOLCHAIN_ROOT" /run/cua-input/toolchain-source \
        --bind "$BUILD_TARGET_DIR" /run/cua-output \
        --bind "$BUILD_EXPORT_DIR" /run/cua-export \
        --clearenv \
        --setenv CARGO_HOME /run/cua-cargo-home \
        --setenv CARGO_TARGET_DIR /run/cua-output \
        --setenv TMPDIR /run/cua-tmp \
        --setenv SFW_BIN /run/cua-tools/sfw \
        --setenv CARGO /run/cua-toolchain/bin/cargo \
        --setenv RUSTC /run/cua-toolchain/bin/rustc \
        --setenv RUSTDOC /run/cua-toolchain/bin/rustdoc \
        --setenv CAPSH_BIN "$CAPSH_BIN" \
        --setenv UNSHARE_BIN "$UNSHARE_BIN" \
        --setenv MOUNT_BIN "$MOUNT_BIN" \
        --setenv CC /usr/bin/cc \
        --setenv CXX /usr/bin/c++ \
        --setenv CCACHE_DISABLE 1 \
        --setenv CUA_DRIVER_SOURCE_SHA "$CUA_DRIVER_SOURCE_SHA" \
        --setenv PATH /run/cua-toolchain/bin:/usr/sbin:/usr/bin:/sbin:/bin \
        --cap-add CAP_SYS_ADMIN \
        --cap-add CAP_SETPCAP \
        /bin/bash -c '
            set -euo pipefail
            repo_relative="$1"
            script_relative="$2"
            build_config="$3"
            sfw_sha256="$4"
            toolchain_sha256="$5"
            hash_toolchain_tree() {
                tar --sort=name --mtime="UTC 1970-01-01" --owner=0 --group=0 \
                    --numeric-owner --format=posix \
                    --pax-option=delete=atime,delete=ctime \
                    -C "$1" -cf - . | sha256sum | cut -d" " -f1
            }
            tar -xf - -C /run/cua-source
            "$MOUNT_BIN" --bind /run/cua-source /run/cua-source
            "$MOUNT_BIN" -o remount,bind,ro /run/cua-source
            cp /run/cua-input/sfw-source /run/cua-tools/sfw
            printf "%s  %s\n" "$sfw_sha256" /run/cua-tools/sfw | sha256sum -c -
            chmod 0555 /run/cua-tools/sfw
            "$MOUNT_BIN" --bind /run/cua-tools /run/cua-tools
            "$MOUNT_BIN" -o remount,bind,ro /run/cua-tools
            cp -a /run/cua-input/toolchain-source/. /run/cua-toolchain/
            copied_toolchain_sha256="$(hash_toolchain_tree /run/cua-toolchain)"
            if [ "$copied_toolchain_sha256" != "$toolchain_sha256" ]; then
                echo "immutable Rust toolchain changed during capture" >&2
                exit 1
            fi
            "$MOUNT_BIN" --bind /run/cua-toolchain /run/cua-toolchain
            "$MOUNT_BIN" -o remount,bind,ro /run/cua-toolchain
            "$UNSHARE_BIN" --pid --fork --kill-child \
                "$CAPSH_BIN" --secbits=3 --drop=all --caps="" --no-new-privs -- -c '\''
                    set -euo pipefail
                    cd "/run/cua-source/$1"
                    exec /run/cua-tools/sfw cargo fetch --locked
                '\'' immutable-fetch "$repo_relative"
            # The policy fetcher is an attested input, but it is not trusted to
            # materialize build bytes. Discard anything it could plant in the
            # output or unpacked registry, then let the attested Cargo verify
            # Cargo.lock checksums while constructing the only dependency tree
            # consumed by the offline build.
            find /run/cua-output /run/cua-vendor -mindepth 1 -delete
            rm -rf /run/cua-cargo-home/registry/src /run/cua-cargo-home/git/checkouts
            rm -f /run/cua-cargo-home/config /run/cua-cargo-home/config.toml
            "$UNSHARE_BIN" --net --pid --fork --kill-child \
                "$CAPSH_BIN" --secbits=3 --drop=all --caps="" --no-new-privs -- -c '\''
                    set -euo pipefail
                    cd "/run/cua-source/$1"
                    cargo vendor --locked --offline /run/cua-vendor >/dev/null
                '\'' immutable-vendor "$repo_relative"
            cat > /run/cua-cargo-home/config.toml <<"CONFIG"
[source.crates-io]
replace-with = "cua-vendored"

[source.cua-vendored]
directory = "/run/cua-vendor"
CONFIG
            "$MOUNT_BIN" --bind /run/cua-vendor /run/cua-vendor
            "$MOUNT_BIN" -o remount,bind,ro /run/cua-vendor
            touch /run/cua-cargo-home/.package-cache /run/cua-package-cache
            "$MOUNT_BIN" --bind /run/cua-cargo-home /run/cua-cargo-home
            "$MOUNT_BIN" -o remount,bind,ro /run/cua-cargo-home
            "$MOUNT_BIN" --bind /run/cua-package-cache /run/cua-cargo-home/.package-cache
            exec "$UNSHARE_BIN" --net --pid --fork --kill-child \
                "$CAPSH_BIN" --secbits=3 --drop=all --caps="" --no-new-privs -- -c '\''
                    set -euo pipefail
                    repo_relative="$1"
                    script_relative="$2"
                    build_config="$3"
                    cd "/run/cua-source/$repo_relative"
                    if [ "$build_config" = release ]; then
                        cargo build --locked --offline --release -p cua-driver -p cursor-theme-cli --features portal-input
                    else
                        cargo build --locked --offline -p cua-driver -p cursor-theme-cli --features portal-input
                    fi
                    mkdir -p /run/cua-export/repo/Skills /run/cua-export/scripts
                    cp -R Skills/cua-driver /run/cua-export/repo/Skills/cua-driver
                    cp "/run/cua-source/$script_relative/_local-signing.sh" /run/cua-export/scripts/
                    cp "/run/cua-source/$script_relative/_install-transaction-lock.py" /run/cua-export/scripts/
                    if [ -f "/run/cua-source/$script_relative/post-install-hints.txt" ]; then
                        cp "/run/cua-source/$script_relative/post-install-hints.txt" /run/cua-export/scripts/
                    fi
                '\'' immutable-build "$repo_relative" "$script_relative" "$build_config"
        ' immutable-setup "$REPO_ROOT_RELATIVE" "$SCRIPT_DIR_RELATIVE" "$BUILD_CONFIG" \
          "$SFW_SOURCE_SHA256" "$TOOLCHAIN_TREE_SHA256"

if [ "$(hash_system_build_inputs)" != "$SYSTEM_BUILD_INPUTS_SHA256" ]; then
    echo "${RED}Error: root-owned system build inputs changed during the isolated build.${NORMAL}" >&2
    exit 1
fi

BUILT_BINARY="$BUILD_TARGET_DIR/$BUILD_CONFIG/cua-driver"
BUILT_THEME_BINARY="$BUILD_TARGET_DIR/$BUILD_CONFIG/cua-cursor-theme"
if [ ! -x "$BUILT_BINARY" ]; then
    echo "${RED}Error: build produced no binary at $BUILT_BINARY${NORMAL}"
    exit 1
fi
if [ ! -x "$BUILT_THEME_BINARY" ]; then
    echo "${RED}Error: build produced no cursor-theme compiler at $BUILT_THEME_BINARY${NORMAL}"
    exit 1
fi
echo ""

# --- Stage into versioned release dir + repoint `current` --------------

echo "${BOLD}Staging into $VERSIONED_DIR${NORMAL}"
mkdir -p "$VERSIONED_DIR"
STAGED_BINARY="$VERSIONED_DIR/cua-driver-local"
STAGED_THEME_BINARY="$VERSIONED_DIR/cua-cursor-theme"
STAGED_BINARY_TMP="$VERSIONED_DIR/.cua-driver-local.tmp.$$"
STAGED_THEME_BINARY_TMP="$VERSIONED_DIR/.cua-cursor-theme.tmp.$$"
rm -f "$STAGED_BINARY_TMP" "$STAGED_THEME_BINARY_TMP"
cp "$BUILT_BINARY" "$STAGED_BINARY_TMP"
cp "$BUILT_THEME_BINARY" "$STAGED_THEME_BINARY_TMP"
chmod +x "$STAGED_BINARY_TMP" "$STAGED_THEME_BINARY_TMP"
# Replacing a running executable in place can fail with ETXTBSY on Linux.
# Rename complete sibling files over their destinations instead: existing
# processes retain old inodes while new launches atomically receive this build.
mv -f "$STAGED_BINARY_TMP" "$STAGED_BINARY"
mv -f "$STAGED_THEME_BINARY_TMP" "$STAGED_THEME_BINARY"

# Re-sign with a fresh ad-hoc signature.
#
# macOS 26+ Taskgated rejects the linker-emitted ad-hoc signature once
# the binary has been copied (the kernel's cached signature for the new
# inode doesn't match the embedded one strictly enough for the newer
# CODESIGNING namespace). Result is `SIGKILL (Code Signature Invalid)
# — Taskgated Invalid Signature` on first run, no stderr output, exit
# code 137 — extremely confusing without a diagnostic-report dig. The
# fix: re-sign in place. `codesign --force --sign -` emits a fresh
# ad-hoc signature keyed to the new on-disk bytes, which Taskgated
# accepts. Cheap (~50ms on a 40MB binary). macOS-only — no-op on Linux.
if [ "$OS" = "Darwin" ]; then
    if command -v codesign >/dev/null 2>&1; then
        codesign --force --sign - "$VERSIONED_DIR/cua-driver-local" 2>/dev/null \
            || echo "${YELLOW}warning: codesign --force --sign - failed; first run may fail with SIGKILL on macOS 26+${NORMAL}" >&2
        codesign --force --sign - "$VERSIONED_DIR/cua-cursor-theme" 2>/dev/null \
            || echo "${YELLOW}warning: cursor-theme sidecar signing failed${NORMAL}" >&2
    fi
fi

# Skill pack — stage from the repo so the `current` symlink below
# transparently exposes it to agents. Mirrors what install.sh does
# from a release tarball.
SOURCE_SKILLS="$BUILD_REPO_ROOT/Skills/cua-driver"
if [ -d "$SOURCE_SKILLS" ]; then
    STAGED_SKILLS="$VERSIONED_DIR/Skills/cua-driver"
    rm -rf "$STAGED_SKILLS"
    mkdir -p "$(dirname "$STAGED_SKILLS")"
    cp -R "$SOURCE_SKILLS" "$STAGED_SKILLS"
    echo "${GREEN}staged skill pack at $STAGED_SKILLS${NORMAL}"
fi

assert_clean_source

sha256_file() {
    "$SHA256SUM_BIN" "$1" | "$CUT_BIN" -d' ' -f1
}
skill_tree_sha256() {
    "$PYTHON_BIN" - "$1" <<'PY'
import hashlib
import os
from pathlib import Path
import stat
import sys

root = Path(sys.argv[1])
digest = hashlib.sha256()

def field(value: bytes) -> None:
    digest.update(len(value).to_bytes(8, "big"))
    digest.update(value)

if root.is_dir():
    for path in sorted(root.rglob("*"), key=lambda item: item.relative_to(root).as_posix()):
        relative = os.fsencode(path.relative_to(root).as_posix())
        entry_stat = path.lstat()
        # Promotion removes write bits. Hash executable/traversal semantics so
        # the pre-promotion digest equals the installed immutable tree.
        mode = (stat.S_IMODE(entry_stat.st_mode) & 0o555).to_bytes(4, "big")
        if path.is_symlink():
            raise SystemExit(f"symlink is not allowed in staged skill pack: {path}")
        elif path.is_dir():
            entry_type = b"D"
            payload = b""
        elif path.is_file():
            entry_type = b"F"
            payload = path.read_bytes()
        else:
            raise SystemExit(f"unsupported staged skill entry: {path}")
        # Type, path, executable permissions, and payload are length-framed.
        for value in (entry_type, relative, mode, payload):
            field(value)
print(digest.hexdigest())
PY
}

release_identity_sha256() {
    "$PYTHON_BIN" - "$@" <<'PY'
import hashlib
import sys

digest = hashlib.sha256()
for argument in sys.argv[1:]:
    value = argument.encode("utf-8")
    digest.update(len(value).to_bytes(8, "big"))
    digest.update(value)
print(digest.hexdigest())
PY
}

# Skill modes are part of provenance. Freeze the staged tree before hashing so
# the digest describes the exact immutable modes later reused from the store.
if [ -d "$VERSIONED_DIR/Skills" ]; then
    chmod -R a-w "$VERSIONED_DIR/Skills"
fi
BINARY_SHA256="$(sha256_file "$STAGED_BINARY")"
THEME_BINARY_SHA256="$(sha256_file "$STAGED_THEME_BINARY")"
SKILLS_SHA256="$(skill_tree_sha256 "$VERSIONED_DIR/Skills")"
if ! printf '%s' "$BINARY_SHA256" | grep -Eq '^[0-9a-f]{64}$'; then
    echo "${RED}Error: could not determine the staged binary SHA-256.${NORMAL}" >&2
    exit 1
fi
if ! printf '%s' "$THEME_BINARY_SHA256" | grep -Eq '^[0-9a-f]{64}$'; then
    echo "${RED}Error: could not determine the staged cursor-theme binary SHA-256.${NORMAL}" >&2
    exit 1
fi
if ! printf '%s' "$SKILLS_SHA256" | grep -Eq '^[0-9a-f]{64}$'; then
    echo "${RED}Error: could not determine the staged skill-pack SHA-256.${NORMAL}" >&2
    exit 1
fi
if [ "$OS" = "Linux" ]; then
    FEATURES_JSON='["portal-input"]'
else
    FEATURES_JSON='[]'
fi
PROVENANCE_SCHEMA="cua-driver-local-provenance-v4"
RELEASE_IDENTITY_SHA256="$(release_identity_sha256 \
    "$PROVENANCE_SCHEMA" \
    "$CUA_DRIVER_SOURCE_SHA" \
    "$BINARY_SHA256" \
    "$THEME_BINARY_SHA256" \
    "$SKILLS_SHA256" \
    "$BUILD_CONFIG" \
    "$TARGET_TRIPLE" \
    "$FEATURES_JSON" \
    "$SFW_SOURCE_SHA256" \
    "$TOOLCHAIN_TREE_SHA256" \
    "$SYSTEM_BUILD_INPUTS_SHA256")"
if ! printf '%s' "$RELEASE_IDENTITY_SHA256" | grep -Eq '^[0-9a-f]{64}$'; then
    echo "${RED}Error: could not determine the canonical release identity SHA-256.${NORMAL}" >&2
    exit 1
fi
# Keep each filesystem component below NAME_MAX while binding the full source,
# binary, cursor-theme sidecar, skill tree, build mode, target, and feature set.
FINAL_VERSIONED_DIR="$RELEASES_DIR/$VERSION_TAG-v4-$CUA_DRIVER_SOURCE_SHA-$RELEASE_IDENTITY_SHA256-$TARGET_TRIPLE"
PROVENANCE_TMP="$VERSIONED_DIR/.provenance.json.tmp.$$"
printf '{\n  "schema": "%s",\n  "source_sha": "%s",\n  "binary_sha256": "%s",\n  "cursor_theme_binary_sha256": "%s",\n  "skills_sha256": "%s",\n  "sfw_sha256": "%s",\n  "rust_toolchain_sha256": "%s",\n  "system_build_inputs_sha256": "%s",\n  "release_identity_sha256": "%s",\n  "build_config": "%s",\n  "target": "%s",\n  "features": %s\n}\n' \
    "$PROVENANCE_SCHEMA" "$CUA_DRIVER_SOURCE_SHA" "$BINARY_SHA256" "$THEME_BINARY_SHA256" "$SKILLS_SHA256" "$SFW_SOURCE_SHA256" "$TOOLCHAIN_TREE_SHA256" "$SYSTEM_BUILD_INPUTS_SHA256" "$RELEASE_IDENTITY_SHA256" "$BUILD_CONFIG" "$TARGET_TRIPLE" "$FEATURES_JSON" \
    >"$PROVENANCE_TMP"
mv "$PROVENANCE_TMP" "$VERSIONED_DIR/provenance.json"

CUA_DRIVER_RS_TELEMETRY_ENABLED=false "$PYTHON_BIN" - "$STAGED_BINARY" "$CUA_DRIVER_SOURCE_SHA" <<'PY'
import json
import subprocess
import sys

binary, expected_source = sys.argv[1:]
requests = [
    {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}},
    {
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {"name": "get_config", "arguments": {}},
    },
]
completed = subprocess.run(
    [binary, "mcp", "--direct"],
    input="".join(json.dumps(request) + "\n" for request in requests),
    text=True,
    capture_output=True,
    timeout=30,
    check=True,
)
responses = [json.loads(line) for line in completed.stdout.splitlines() if line.strip()]
response = next((item for item in responses if item.get("id") == 2), None)
if response is None or "error" in response:
    raise SystemExit("staged direct get_config probe failed")
structured = response.get("result", {}).get("structuredContent")
if not isinstance(structured, dict) or structured.get("source_sha") != expected_source:
    raise SystemExit("staged binary embedded source does not match the promotion source")
PY
assert_clean_source

mkdir -p "$HOME_DIR/packages"
# Revalidate immediately before the first release-store mutation. The managed
# updater retains the same inherited descriptor through acceptance and rollback.
if ! "$LOCK_PYTHON" "$INSTALL_TRANSACTION_LOCK_HELPER" --validate; then
    echo "${RED}Error: install transaction lock identity changed before promotion.${NORMAL}" >&2
    exit 1
fi

if [ -e "$FINAL_VERSIONED_DIR" ] || [ -L "$FINAL_VERSIONED_DIR" ]; then
    if [ ! -d "$FINAL_VERSIONED_DIR" ] || [ -L "$FINAL_VERSIONED_DIR" ]; then
        echo "${RED}Error: immutable release path exists but is not a real directory.${NORMAL}" >&2
        exit 1
    fi
    "$PYTHON_BIN" - "$FINAL_VERSIONED_DIR" <<'PY'
import os
from pathlib import Path
import sys

root = Path(sys.argv[1])
for path in [root, *root.rglob("*")]:
    if os.lstat(path).st_mode & 0o222:
        raise SystemExit(f"immutable release contains writable entry: {path}")
PY
    EXISTING_SHA256="$(sha256_file "$FINAL_VERSIONED_DIR/cua-driver-local")"
    EXISTING_THEME_SHA256="$(sha256_file "$FINAL_VERSIONED_DIR/cua-cursor-theme")"
    EXISTING_SKILLS_SHA256="$(skill_tree_sha256 "$FINAL_VERSIONED_DIR/Skills")"
    if [ "$EXISTING_SHA256" != "$BINARY_SHA256" ] \
       || [ "$EXISTING_THEME_SHA256" != "$THEME_BINARY_SHA256" ] \
       || [ "$EXISTING_SKILLS_SHA256" != "$SKILLS_SHA256" ] \
       || ! cmp -s "$FINAL_VERSIONED_DIR/provenance.json" "$VERSIONED_DIR/provenance.json" \
       || ! diff -qr "$FINAL_VERSIONED_DIR" "$VERSIONED_DIR" >/dev/null; then
        echo "${RED}Error: immutable release directory exists with different content or provenance.${NORMAL}" >&2
        exit 1
    fi
    chmod -R u+w "$VERSIONED_DIR" 2>/dev/null || true
    rm -rf "$VERSIONED_DIR"
else
    chmod -R a-w "$VERSIONED_DIR"
    mv "$VERSIONED_DIR" "$FINAL_VERSIONED_DIR"
fi
VERSIONED_DIR="$FINAL_VERSIONED_DIR"
STAGING_VERSIONED_DIR=""
STAGED_BINARY="$VERSIONED_DIR/cua-driver-local"
assert_clean_source

# --- macOS: stable local code-signing identity (so TCC grants survive rebuilds) ---
#
# Keep policy in a sourceable helper so strict/fallback behavior can be tested
# without building or installing the app.
# shellcheck source-path=SCRIPTDIR
# shellcheck source=_local-signing.sh
. "$BUILD_SCRIPT_DIR/_local-signing.sh"

# --- macOS: wrap the binary in CuaDriverLocal.app for a stable TCC identity ---
#
# TCC keys Accessibility / Screen-Recording grants on the bundle
# identifier (com.trycua.driver.local), not the bare executable path. A loose
# binary gets grants attributed to its ad-hoc cdhash, which changes on
# every rebuild — so permissions silently reset and never appear cleanly
# under System Settings. Mirror the production path (install.sh) + the CD
# bundle-assembly step: drop the freshly built binary into the checked-in
# CuaDriverBundle skeleton, install the bundle to /Applications, and point
# the visible bin at the binary INSIDE the bundle. Linux/Windows have no
# .app concept and keep the bare-binary symlink below.
APP_DEST="/Applications/CuaDriverLocal.app"
if [ "$OS" = "Darwin" ]; then
    SKELETON="$BUILD_REPO_ROOT/scripts/CuaDriverBundle"
    if [ ! -d "$SKELETON/Contents" ]; then
        echo "${RED}Error: bundle skeleton missing at $SKELETON${NORMAL}" >&2
        exit 1
    fi
    APP_STAGE="$HOME_DIR/packages/.app-staging-$VERSION_TAG-$TARGET_TRIPLE-$$"
    rm -rf "$APP_STAGE"
    mkdir -p "$APP_STAGE/Contents/MacOS"
    cp -R "$SKELETON/Contents/." "$APP_STAGE/Contents/"
    cp "$VERSIONED_DIR/cua-driver-local" "$APP_STAGE/Contents/MacOS/cua-driver-local"
    cp "$VERSIONED_DIR/cua-cursor-theme" "$APP_STAGE/Contents/MacOS/cua-cursor-theme"
    chmod +x "$APP_STAGE/Contents/MacOS/cua-driver-local"
    chmod +x "$APP_STAGE/Contents/MacOS/cua-cursor-theme"
    rm -f "$APP_STAGE/Contents/MacOS/.gitkeep"
    # Stamp the local build version so the bundle reports something sane.
    if command -v plutil >/dev/null 2>&1; then
        plutil -replace CFBundleShortVersionString -string "$VERSION_TAG" \
            "$APP_STAGE/Contents/Info.plist" 2>/dev/null || true
        plutil -replace CFBundleVersion -string "$VERSION_TAG" \
            "$APP_STAGE/Contents/Info.plist" 2>/dev/null || true
        plutil -replace CFBundleExecutable -string "cua-driver-local" \
            "$APP_STAGE/Contents/Info.plist"
        plutil -replace CFBundleIdentifier -string "com.trycua.driver.local" \
            "$APP_STAGE/Contents/Info.plist"
        plutil -replace CFBundleName -string "Cua Driver Local" \
            "$APP_STAGE/Contents/Info.plist"
        plutil -replace CFBundleDisplayName -string "Cua Driver Local" \
            "$APP_STAGE/Contents/Info.plist"
    fi
    # Sign the staged bundle before touching the live installation. Required on
    # macOS 26+ where Taskgated rejects a copied binary's stale signature.
    # Prefer the STABLE self-signed identity so TCC grants survive rebuilds;
    # never downgrade an existing certificate-signed installation to ad-hoc,
    # because that would invalidate its working TCC grants.
    if command -v codesign >/dev/null 2>&1; then
        if ! sign_staged_local_app "$APP_STAGE" "$APP_DEST"; then
            exit 1
        fi
        if ! codesign --verify --deep --strict "$APP_STAGE" 2>/dev/null; then
            echo "${RED}Error: staged CuaDriverLocal.app failed signature verification; live installation was not changed.${NORMAL}" >&2
            exit 1
        fi
        STAGED_REQUIREMENT="$(designated_requirement "$APP_STAGE")"
        STAGED_SIGNING_CLASS="$(classify_designated_requirement "$STAGED_REQUIREMENT")"
    else
        echo "${RED}Error: codesign is required to install CuaDriverLocal.app safely.${NORMAL}" >&2
        exit 1
    fi

    # Install to /Applications (user-writable for admins; no sudo — same as
    # install.sh). Keep the prior bundle available until the copy completes so
    # an interrupted install cannot leave a corrupt live app.
    assert_clean_source
    APP_BACKUP="${APP_DEST}.install-backup.$$"
    rm -rf "$APP_BACKUP"
    if [ -d "$APP_DEST" ]; then
        mv "$APP_DEST" "$APP_BACKUP"
    fi
    APP_TRANSACTION_ACTIVE=true
    install_valid=false
    if ditto "$APP_STAGE" "$APP_DEST" \
       && codesign --verify --deep --strict "$APP_DEST" 2>/dev/null; then
        INSTALLED_REQUIREMENT="$(designated_requirement "$APP_DEST")"
        INSTALLED_SIGNING_CLASS="$(classify_designated_requirement "$INSTALLED_REQUIREMENT")"
        if [ "$INSTALLED_REQUIREMENT" = "$STAGED_REQUIREMENT" ] \
           && [ "$INSTALLED_SIGNING_CLASS" = "$STAGED_SIGNING_CLASS" ] \
           && [ "$INSTALLED_SIGNING_CLASS" != "unknown" ]; then
            install_valid=true
        fi
    fi
    if [ "$install_valid" = true ]; then
        : # Keep the backup until the selector transaction commits below.
    else
        rm -rf "$APP_DEST"
        if [ -d "$APP_BACKUP" ]; then
            mv "$APP_BACKUP" "$APP_DEST"
        fi
        APP_TRANSACTION_ACTIVE=false
        echo "${RED}Error: installed CuaDriverLocal.app did not preserve its verified signing identity; restored the previous bundle.${NORMAL}" >&2
        exit 1
    fi
    echo "${GREEN}installed $APP_DEST${NORMAL}"
    if [ "$INSTALLED_SIGNING_CLASS" = "certificate-backed" ]; then
        echo "${GREEN}verified installed designated requirement: certificate-backed (stable across rebuilds)${NORMAL}"
    else
        echo "${YELLOW}verified installed designated requirement: ad-hoc cdhash (changes on rebuild)${NORMAL}" >&2
    fi
    rm -rf "$APP_STAGE"
    APP_STAGE=""

    # --- Force LaunchServices registration of the freshly-copied bundle ----
    #
    # `ditto` drops the bundle on disk, but LaunchServices registers the new
    # com.trycua.driver.local identity ASYNCHRONOUSLY (seconds later). Until it
    # does, `open -n -g -a CuaDriverLocal` (what `permissions grant` / MCP use to
    # launch the daemon) fails with -1728. A synchronous `lsregister -f` closes
    # that race so both the reset and the first launch resolve the bundle id.
    LSREGISTER="/System/Library/Frameworks/CoreServices.framework/Versions/A/Frameworks/LaunchServices.framework/Versions/A/Support/lsregister"
    if [ -x "$LSREGISTER" ]; then
        "$LSREGISTER" -f "$APP_DEST" >/dev/null 2>&1 || true
    fi

fi

# --- Visible-bin symlink ------------------------------------------------
#
# On macOS point at the binary INSIDE the installed bundle so the process
# that actually runs carries the com.trycua.driver.local identity (TCC keys
# grants on it). On Linux/Windows point at the versioned-store binary.
mkdir -p "$BIN_DIR"
if [ "$OS" = "Darwin" ]; then
    BIN_TARGET="$APP_DEST/Contents/MacOS/cua-driver-local"
else
    BIN_TARGET="$CURRENT_LINK/cua-driver-local"
fi
BIN_LINK="$BIN_DIR/cua-driver-local"
if [ -e "$BIN_LINK" ] && [ -d "$BIN_LINK" ] && [ ! -L "$BIN_LINK" ]; then
    echo "${RED}Error: visible binary path exists as a real directory: $BIN_LINK.${NORMAL}" >&2
    exit 1
fi
BIN_LINK_TMP="${BIN_LINK}.new.$$"
rm -f "$BIN_LINK_TMP"
ln -s "$BIN_TARGET" "$BIN_LINK_TMP"
if [ "$OS" = "Linux" ]; then
    mv -Tf "$BIN_LINK_TMP" "$BIN_LINK"
else
    mv -fh "$BIN_LINK_TMP" "$BIN_LINK"
fi
BIN_LINK_TMP=""
echo "${GREEN}$BIN_DIR/cua-driver-local -> $BIN_TARGET${NORMAL}"
echo ""

# Replace the release selector with a single rename. The temp symlink is a
# sibling, so the rename is atomic and either the old or new selector remains
# visible across interruption. The host transaction lock remains held here.
assert_clean_source
mkdir -p "$HOME_DIR/packages"
if { [ -e "$CURRENT_LINK" ] || [ -L "$CURRENT_LINK" ]; } && [ ! -L "$CURRENT_LINK" ]; then
    echo "${RED}Error: release selector exists but is not a symlink: $CURRENT_LINK.${NORMAL}" >&2
    exit 1
fi
CURRENT_LINK_TMP="${CURRENT_LINK}.new.$$"
rm -f "$CURRENT_LINK_TMP"
ln -s "$VERSIONED_DIR" "$CURRENT_LINK_TMP"
if [ "$OS" = "Linux" ]; then
    mv -Tf "$CURRENT_LINK_TMP" "$CURRENT_LINK"
else
    # BSD mv follows symlink-to-directory destinations unless -h is supplied.
    mv -fh "$CURRENT_LINK_TMP" "$CURRENT_LINK"
fi
CURRENT_LINK_TMP=""
if [ "$APP_TRANSACTION_ACTIVE" = true ]; then
    APP_TRANSACTION_ACTIVE=false
    rm -rf "$APP_BACKUP"
fi
echo "${GREEN}current -> $VERSIONED_DIR${NORMAL}"
echo ""

INSTALLED_BIN="$BIN_DIR/cua-driver-local"

# Release-critical source, build, provenance, and selector work is complete.
# Restore the caller PATH only for optional host service integration below.
PATH="$CALLER_PATH"
export PATH

# --- Stop any pre-swap cua-driver daemons ------------------------------
#
# Stop only the managed daemon. Direct MCP and private SDK workers retain their
# old immutable inode and are allowed to drain; killing by executable name can
# abort an unrelated in-flight action.
if [ "$OS" = "Darwin" ]; then
    launchctl unload "$HOME/Library/LaunchAgents/com.trycua.cua-driver-local.plist" 2>/dev/null || true
elif [ "$OS" = "Linux" ] && command -v systemctl >/dev/null 2>&1; then
    systemctl --user stop cua-driver-local.service >/dev/null 2>&1 || true
fi

# Agent skill pack symlinks: NOT auto-created. Run
# `cua-driver skills install --local` to symlink agent dirs to the
# staged copy at $VERSIONED_DIR/Skills/cua-driver-rs above.
echo ""

# --- Autostart (optional) ----------------------------------------------

if [ "$INSTALL_AUTOSTART" = true ]; then
    if [ "$OS" = "Darwin" ]; then
        PLIST_PATH="$HOME/Library/LaunchAgents/com.trycua.cua-driver-local.plist"
        echo "${BOLD}Writing LaunchAgent → $PLIST_PATH${NORMAL}"
        mkdir -p "$(dirname "$PLIST_PATH")"
        cat >"$PLIST_PATH" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>com.trycua.cua-driver-local</string>
  <key>ProgramArguments</key>
  <array>
    <string>$INSTALLED_BIN</string>
    <string>serve</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>$HOME_DIR/serve.out.log</string>
  <key>StandardErrorPath</key><string>$HOME_DIR/serve.err.log</string>
</dict>
</plist>
EOF
        launchctl unload "$PLIST_PATH" 2>/dev/null || true
        launchctl load "$PLIST_PATH"
        echo "${GREEN}Loaded.${NORMAL} Manage with launchctl load / unload \"$PLIST_PATH\"."
    elif [ "$OS" = "Linux" ]; then
        UNIT_PATH="$HOME/.config/systemd/user/cua-driver-local.service"
        echo "${BOLD}Writing systemd user unit → $UNIT_PATH${NORMAL}"
        mkdir -p "$(dirname "$UNIT_PATH")"
        cat >"$UNIT_PATH" <<EOF
[Unit]
Description=cua-driver-local serve daemon
After=graphical-session.target

[Service]
ExecStart=$INSTALLED_BIN serve
Restart=on-failure
RestartSec=2

[Install]
WantedBy=default.target
EOF
        systemctl --user daemon-reload
        systemctl --user enable --now cua-driver-local.service
        echo "${GREEN}Enabled.${NORMAL} Manage with systemctl --user {start|stop|status} cua-driver-local."
    fi
    echo ""
fi

# --- Done ---------------------------------------------------------------

echo "${BOLD}${GREEN}Installed.${NORMAL}"
echo "  ${BOLD}$INSTALLED_BIN${NORMAL}"
echo ""

# Unified post-install hints come from a single shared text file so the
# 4 Rust installers (this script + install-local.ps1 + _install-rust.sh +
# install.ps1) never drift. The .txt holds the OS-agnostic bulk
# (Try-it / skill pack / MCP setup / docs link) with {{BINARY}}
# placeholders; OS-specific bits stay inline below.
HINTS_TXT="$BUILD_SCRIPT_DIR/post-install-hints.txt"
if [ -f "$HINTS_TXT" ]; then
    sed "s|{{BINARY}}|$INSTALLED_BIN|g" "$HINTS_TXT"
else
    # Repo layout changed or running from an unexpected location — fall
    # back to one-line essentials so users still know what to do next.
    echo "Next steps: $INSTALLED_BIN --version  |  $INSTALLED_BIN mcp-config  |  $INSTALLED_BIN skills install"
    echo "Docs: https://github.com/trycua/cua/tree/main/libs/cua-driver/rust"
fi

# The local/release identity split deliberately stopped source installs from
# creating or repairing the published `cua-driver` name. Make the resulting
# migration state explicit when only the local product is present: otherwise
# an existing MCP client can keep launching a now-missing release path even
# though this install completed successfully. Do not create a compatibility
# symlink here; that would collapse the separate product identities again.
RELEASE_BIN="$BIN_DIR/cua-driver"
if [ ! -e "$RELEASE_BIN" ]; then
    echo ""
    echo "${YELLOW}Migration note: the published cua-driver CLI is not installed at $RELEASE_BIN.${NORMAL}" >&2
    echo "  Existing MCP clients configured for 'cua-driver' will not use this local build." >&2
    echo "  To configure Codex for the local build, run:" >&2
    echo "    $INSTALLED_BIN mcp-config --client codex" >&2
    echo "  To restore the published product instead, run:" >&2
    echo '    /bin/bash -c "$(curl -fsSL https://cua.ai/driver/install.sh)"' >&2
fi

# OS-specific autostart hint (kept inline; per-shell natural location).
if [ "$INSTALL_AUTOSTART" != true ]; then
    echo ""
    if [ "$OS" = "Darwin" ]; then
        echo "Auto-start (recommended on macOS): re-run with --autostart to register a LaunchAgent."
        echo "  A launchd-started daemon is attributed to com.trycua.driver.local (not your terminal),"
        echo "  so permission prompts say \"Cua Driver Local\" and grants stick — grant Accessibility +"
        echo "  Screen Recording once and every cua-driver-local call/mcp routes through it correctly."
        echo "  (Without it, a prompt raised from a terminal attributes to the terminal instead.)"
    else
        echo "Auto-start (optional): re-run with --autostart to register a systemd user unit."
    fi
    echo ""
fi
