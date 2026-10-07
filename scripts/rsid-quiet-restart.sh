#!/usr/bin/env bash
# Sourced by install-release.sh (#1122): ask the running daemon to restart onto
# the freshly built binaries at a quiet point instead of cutting off running
# turns. It is the operator front of the AgentRequestDeploy runner.

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
    python3 - "$rpc" "$socket" "$dir" "$wait" <<'PY' || status=$?
import json
import socket as socket_module
import subprocess
import sys

rpc, socket_path, directory, wait = sys.argv[1:5]
NOW_HINT = "Retry later, or force an immediate restart with: make release-install NOW=1"


def ambiguous(reason):
    print(f"quiet-point restart not requested: {reason}", file=sys.stderr)
    print(
        "The daemon may still be running turns, so it was not restarted. " + NOW_HINT,
        file=sys.stderr,
    )
    sys.exit(1)


# Positive evidence that the daemon is down: its socket is absent or refuses.
probe = socket_module.socket(socket_module.AF_UNIX, socket_module.SOCK_STREAM)
probe.settimeout(5)
try:
    probe.connect(socket_path)
except (FileNotFoundError, ConnectionRefusedError):
    sys.exit(11)
except OSError as error:
    ambiguous(f"cannot probe the daemon socket ({error})")
finally:
    probe.close()

params = {"binaries_dir": directory}
if wait:
    params["max_wait_secs"] = int(wait)
try:
    run = subprocess.run(
        [rpc, "--socket", socket_path, "RequestOperatorRestart", "--params", json.dumps(params)],
        capture_output=True,
        text=True,
        timeout=120,
    )
except subprocess.TimeoutExpired:
    ambiguous("the daemon did not answer RequestOperatorRestart in time")
except OSError as error:
    ambiguous(f"cannot run {rpc} ({error})")
try:
    reply = json.loads(run.stdout)
except ValueError:
    reply = None
if not isinstance(reply, dict) or reply.get("jsonrpc") != "2.0":
    ambiguous("the daemon's reply to RequestOperatorRestart was not understood")
error = reply.get("error")
if error:
    message = str(error.get("message", error) if isinstance(error, dict) else error)
    if "deploy_needs_supervisor" in message:
        print("rsid is not under rsid-supervisor.sh; restarting it directly.", file=sys.stderr)
        sys.exit(10)
    print(f"quiet-point restart refused: {message}", file=sys.stderr)
    print(NOW_HINT, file=sys.stderr)
    sys.exit(1)
status = reply.get("result") or {}
print(
    "Restart pending: rsid swaps in the new binaries and restarts at the next quiet "
    f"point (sha {status.get('sha')}, release by {status.get('release_by')})."
)
print("  Waiting for running turns, jobs and landings to finish; the TUI shows the countdown.")
print("  Force it now:  rsi-rpc ForceOperatorRestart    Cancel it:  rsi-rpc CancelOperatorRestart")
sys.exit(0)
PY
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
RSID_DEPLOY_BINARIES=(rsid rsi rsi-rpc rsi-agent-mcp rsi-build-rustc rsi-contract-validate rsi-rolling-land rsi-remote rsid-supervisor.sh)

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
