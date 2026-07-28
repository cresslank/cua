# _install-common.sh — shared helpers for the .sh installers.
#
# Bash counterpart to CuaDriverInstall.psm1. Both files have to stay in
# lockstep on the kill / probe behaviour so a Mac/Linux dev fix matches
# what install.ps1 does on Windows. Function names mirror the
# PowerShell side:
#
#   stop_cua_driver_daemons          ↔  Stop-CuaDriverDaemons
#   show_cua_driver_daemon_survivors ↔  Show-CuaDriverDaemonSurvivors
#
# This script is sourced (not exec'd) by the Rust install helpers:
#
#   * _install-rust.sh           (production Rust delegate)
#   * _install-local-rust.sh     (dev Rust installer)
#
# Loaders: on-disk first when run from a checked-out tree, else fetched
# from GitHub raw via `curl` (mirrors the irm | iex path that
# install.ps1 uses to import the .psm1 over the network). The inline
# loader is duplicated in each consumer because `source` doesn't have a
# bootstrap function the way PowerShell's `Import-Module` does — kept
# minimal so the duplication cost stays low.
#
# Keep this file narrow on purpose: every byte gets curl'd on every
# `curl ... | bash` install on non-macOS hosts (where install.sh
# auto-delegates to the Rust path) AND on dev installs. Things that DO
# belong here: kill / wait / probe helpers that multiple consumers
# need. Things that DON'T: anything only one script uses (leave it
# inline there).
#
# Style:
#   * No `set -e` — sourced helpers shouldn't change the caller's shell
#     options. Caller scripts are already `set -euo pipefail`.
#   * Best-effort everywhere: every external command is suffixed with
#     `|| true` (or wrapped in a subshell) so a kill failure never
#     aborts the surrounding install.
#   * Bash 3.2 compatible (macOS default). No associative arrays, no
#     `[[ =~ ]]` patterns that need bash 4+.

# Stop only supervisor-owned daemons. Installers must never signal ambient
# `cua-driver mcp`, `__private-worker`, or manually launched processes: those
# may own unrelated in-flight actions and drain independently from an immutable
# release selector swap. A managed service can be restarted explicitly after
# the caller has accepted the new release's provenance.
#
# Returns: always 0. Supervisor cleanup is best-effort and never broadens into
# process-name matching.
stop_cua_driver_daemons() {
    printf '==> stopping any running cua-driver daemons before swap\n'

    # Wrap the whole thing in a subshell so supervisor-specific failures never
    # escape into the caller's `set -e` installation transaction.
    (
        case "$(uname -s 2>/dev/null || echo unknown)" in
            Darwin)
                # Both known LaunchAgent plists. `launchctl unload` is a
                # no-op-with-warning when the plist doesn't exist; swallow
                # stderr to keep the install log clean.
                local plist
                # Rust LaunchAgent plist.
                plist="$HOME/Library/LaunchAgents/com.trycua.cua-driver-rs.plist"
                if [ -f "$plist" ]; then
                    launchctl unload "$plist" >/dev/null 2>&1 || true
                fi
                ;;
            Linux)
                # systemctl --user is the only supported supervisor on
                # Linux today (install-local-rust.sh --autostart writes
                # ~/.config/systemd/user/cua-driver-rs.service). `command
                # -v` so we don't error on systemd-less hosts (musl
                # containers, NixOS without user services, etc.).
                if command -v systemctl >/dev/null 2>&1; then
                    systemctl --user stop cua-driver-rs.service >/dev/null 2>&1 || true
                fi
                ;;
            *)
                # Other Unixes: no supervisor we can safely identify.
                ;;
        esac
    ) || true

    return 0
}

# Print a yellow warning if cua-driver processes are still running after
# Report direct/manual processes that were intentionally left to drain on their
# already-open immutable executable. This is informational; never suggest or
# perform a process-name kill.
show_cua_driver_daemon_survivors() {
    # Exact matching avoids unrelated cua-driver-* programs.
    if ! command -v pgrep >/dev/null 2>&1; then
        # No pgrep, no way to check. Bail quietly — the install can
        # still succeed; worst case the user notices a stale binary
        # and reboots.
        return 0
    fi

    local survivor_pids
    survivor_pids=$(pgrep -x cua-driver 2>/dev/null || true)
    if [ -z "$survivor_pids" ]; then
        return 0
    fi

    # `tput` may not be available (CI, agent sandboxes without TERM);
    # guard so we don't crash before printing the actual warning. The
    # color is decoration — the text is the load-bearing part.
    local yellow normal
    yellow=$(tput setaf 3 2>/dev/null || true)
    normal=$(tput sgr0 2>/dev/null || true)

    # `wc -w` is portable across BSD and GNU; pgrep prints one pid per
    # line so we'd get the same count from `wc -l`, but `wc -w` is
    # robust to a missing trailing newline.
    local count
    count=$(printf '%s\n' "$survivor_pids" | wc -w | tr -d ' ')
    local pid_csv
    pid_csv=$(printf '%s\n' "$survivor_pids" | tr '\n' ',' | sed 's/,$//' | sed 's/,/, /g')

    printf '%sNote: %s existing cua-driver process(es) were intentionally left running (pid: %s).%s\n' \
        "$yellow" "$count" "$pid_csv" "$normal"
    printf '%s      They continue on their current executable and should drain independently.%s\n' \
        "$yellow" "$normal"
    printf '%s      Fresh supervised/invoked workers use the newly selected release.%s\n' \
        "$yellow" "$normal"
    return 0
}
