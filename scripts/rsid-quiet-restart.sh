#!/usr/bin/env bash
# Sourced by install-release.sh (#1122): ask the running daemon to restart onto
# the freshly built binaries at a quiet point instead of cutting off running
# turns. It is the operator front of the AgentRequestDeploy runner.

# shellcheck source=scripts/repo-guard-bin.sh
source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/repo-guard-bin.sh"

# request_quiet_restart <rsi-rpc> <socket> <binaries-dir> [max-wait-secs]
#   0  a quiet-point restart is pending (status printed); the daemon restarts
#      itself and verifies the running build, so the caller must not restart
#   10 a direct restart is safe: the daemon is positively down (its socket is
#      absent or refusing AND no rsid process exists), or it answered that it
#      is not under the supervisor
#   1  anything else (a refusal, a timeout, a malformed reply, a socket that is
#      down while an rsid process lives): the daemon may still be running
#      turns, so nothing is restarted; the caller stops and names NOW=1
# `rsid_running_pids` (rsid-processes.sh) must be defined by the caller.
request_quiet_restart() {
    local rpc="$1" socket="$2" dir="$3" wait="${4:-}" status=0
    if rsid_resolve_repo_guard "$rpc" rsid-quiet-restart; then
        # The request itself is `rsi-repo-guard rsid-quiet-restart` (#465).
        "$RSI_GUARD_BIN" rsid-quiet-restart "$rpc" "$socket" "$dir" "$wait" || status=$?
    else
        echo "quiet-point restart not requested: the request helper is missing." >&2
        echo "Retry later, or force an immediate restart with: make release-install NOW=1" >&2
        status=1
    fi
    case "$status" in
        0) return 0 ;;
        10) return 10 ;;
        11)
            if ! declare -F rsid_running_pids >/dev/null; then
                echo "quiet-point restart not requested: cannot check for an rsid process." >&2
                echo "Retry later, or force an immediate restart with: make release-install NOW=1" >&2
                return 1
            fi
            if [[ -z "$(rsid_running_pids)" ]]; then
                echo "rsid is not running; restarting it directly." >&2
                return 10
            fi
            echo "rsid's socket is not accepting connections but an rsid process is running." >&2
            echo "The daemon may still be running turns, so it was not restarted." >&2
            echo "Retry later, or force an immediate restart with: make release-install NOW=1" >&2
            return 1
            ;;
        *) return 1 ;;
    esac
}

# The deployable binaries, as AgentRequestDeploy names them (DEPLOY_BINARIES).
RSID_DEPLOY_BINARIES=(rsid rsi rsi-rpc rsi-agent-mcp rsi-build-rustc rsi-contract-validate rsi-known-failure rsi-rolling-land rsi-repo-guard rsi-remote rsi-turn-shim rsi-socket-hold rsi-cloud rsi-eval rsid-supervisor.sh)
# Keep the canonical literal inventory readable by its managed-set parity test.
# Shell scripts are staged from source; only Cargo binaries need build coverage.
RSID_CARGO_BINARIES=()
for rsid_deploy_member in "${RSID_DEPLOY_BINARIES[@]}"; do
    case "$rsid_deploy_member" in
        *.sh) ;;
        *) RSID_CARGO_BINARIES+=("$rsid_deploy_member") ;;
    esac
done
unset rsid_deploy_member

# stabilize_installed_binaries <bin-dir> <cargo-release-dir> <stable-dir>
#
# Run BEFORE the build. A quiet-point deploy renames the installed executable
# to `<name>.prev` and puts the new one in its place; startup rollback restores
# that `.prev`. If the install path is a symlink into Cargo's output directory,
# the build has already overwritten the file it points at, so `.prev` would hold
# the NEW executable and a failed build could be restored over itself. Copy each
# installed executable that currently resolves into the Cargo output directory
# to <stable-dir> and point the install path there, so the prior executable
# survives the build and the daemon's `.prev` is genuinely the old one.
stabilize_installed_binaries() {
    local bin_dir="$1" release_dir="$2" stable_dir="$3" name path real release_real temp
    release_real="$(readlink -f "$release_dir" 2>/dev/null || true)"
    [[ -n "$release_real" ]] || return 0
    mkdir -p "$stable_dir"
    for name in "${RSID_DEPLOY_BINARIES[@]}"; do
        path="$bin_dir/$name"
        [[ -e "$path" ]] || continue
        real="$(readlink -f "$path")"
        [[ "$real" == "$release_real"/* ]] || continue
        temp="$(mktemp "$stable_dir/.$name.XXXXXX")"
        cp -p "$real" "$temp"
        chmod 755 "$temp"
        mv -f "$temp" "$stable_dir/$name"
        ln -sfn "$stable_dir/$name" "$path"
    done
}

# install_built_binaries <cargo-release-dir> <install-dir>   (#1164)
#
# Copy each built deployable binary that exists into <install-dir>, one atomic
# rename per file (a running executable is replaced, never overwritten in
# place). The installed copy is the single "rsid the supervisor runs".
install_built_binaries() {
    local release_dir="$1" install_dir="$2" name temp
    mkdir -p "$install_dir"
    for name in "${RSID_DEPLOY_BINARIES[@]}"; do
        [[ -x "$release_dir/$name" ]] || continue
        temp="$(mktemp "$install_dir/.$name.XXXXXX")"
        cp -p "$release_dir/$name" "$temp"
        chmod 755 "$temp"
        mv -f "$temp" "$install_dir/$name"
    done
}

# install_supervisor_script <install-dir>   (#1217)
#
# Copy rsid-supervisor.sh next to the installed rsid, atomically. Everything
# that launches the daemon under the supervisor (install-release's restart and
# the TUI's auto-start) runs this installed copy, never a script from a sandbox
# or the operator checkout. A running supervisor refreshes between lifetimes.
install_supervisor_script() {
    local install_dir="$1" src temp
    src="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/rsid-supervisor.sh"
    [[ -f "$src" ]] || return 1
    mkdir -p "$install_dir"
    temp="$(mktemp "$install_dir/.rsid-supervisor.sh.XXXXXX")"
    cp -p "$src" "$temp"
    bash -n "$temp" || { rm -f "$temp"; return 1; }
    chmod 755 "$temp"
    mv -f "$temp" "$install_dir/rsid-supervisor.sh"
}
