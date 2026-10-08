#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
source "$ROOT/scripts/rsid-processes.sh"
source "$ROOT/scripts/rsid-health.sh"
source "$ROOT/scripts/rsid-quiet-restart.sh"
BIN_DIR="${RSI_INSTALL_BIN_DIR:-$HOME/.local/bin}"
RSI_HOME_DIR="$HOME/.rsi"
DAEMON_LOG="$RSI_HOME_DIR/daemon.log"
RSID_SCOPE_SETTINGS="$RSI_HOME_DIR/rsid-scope.env"

daemon_socket_path() {
    printf '%s\n' "${RSI_DAEMON_SOCKET_PATH:-${RSI_SOCKET:-${MOTHERSHIP_SOCKET:-${FLYWHEEL_SOCKET:-$RSI_HOME_DIR/daemon.sock}}}}"
}
LINK_ONLY=0
NO_RESTART=0
TUI_ONLY=0
# Restart rsid immediately (today's behaviour) instead of at a quiet point:
# --now, or NOW=1 in the environment (make release-install NOW=1).
RESTART_NOW="${NOW:-0}"

while (($#)); do
    case "$1" in
        --link-only)
            LINK_ONLY=1
            shift
            ;;
        --no-restart)
            NO_RESTART=1
            shift
            ;;
        --now)
            RESTART_NOW=1
            shift
            ;;
        --tui-only)
            # Build and link only the `rsi` TUI binary. The daemon is left
            # untouched, so this never restarts rsid.
            TUI_ONLY=1
            NO_RESTART=1
            shift
            ;;
        *)
            echo "Unknown argument: $1" >&2
            echo "Usage: $0 [--link-only] [--no-restart] [--tui-only] [--now]" >&2
            exit 1
            ;;
    esac
done

target_dir() {
    cargo metadata --format-version 1 --no-deps --manifest-path "$ROOT/Cargo.toml" \
        | python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])'
}

TARGET_DIR="$(target_dir)"
RSI_BIN="$TARGET_DIR/release/rsi"
RSID_BIN="$TARGET_DIR/release/rsid"
BUILD_RUSTC_BIN="$TARGET_DIR/release/rsi-build-rustc"
RSI_RPC_BIN="$TARGET_DIR/release/rsi-rpc"
ROLLING_LAND_BIN="$TARGET_DIR/release/rsi-rolling-land"
RSI_AGENT_MCP_BIN="$TARGET_DIR/release/rsi-agent-mcp"
# The worker preamble tells agents `rsi-contract-validate` parses their handoff.
CONTRACT_VALIDATE_BIN="$TARGET_DIR/release/rsi-contract-validate"
# RSI Remote gateway (#1096): rsid's managed systemd user unit runs it from here.
RSI_REMOTE_BIN="$TARGET_DIR/release/rsi-remote"
TURN_SHIM_BIN="$TARGET_DIR/release/rsi-turn-shim"
SOCKET_HOLD_BIN="$TARGET_DIR/release/rsi-socket-hold"
# #1137: the spill stub tells workers to run a bare `rsi-spill show ...`. It is a
# repo script (a thin shim over `rsi-rpc spill`), so link it next to rsi-rpc.
RSI_SPILL_SCRIPT="$ROOT/scripts/rsi-spill"
COMPAT_RELEASE_DIR="$ROOT/target/release"
# #1164: one source of truth for "the rsid the supervisor runs". Cargo's output
# is only a build product. The daemon, the supervisor and the ~/.local/bin links
# all use the copy under INSTALL_DIR, which is where AgentRequestDeploy and
# RequestOperatorRestart swap new binaries in, so a deploy restarts the very
# binary it installed.
INSTALL_DIR="$RSI_HOME_DIR/install"

if [[ "$LINK_ONLY" -eq 0 ]]; then
    # The test seam (fake capabilities, route-validation bypasses) must never
    # ship; refuse before spending a build on it (#1021 S4).
    "$ROOT/scripts/check-release-seam.sh"
fi

if [[ "$TUI_ONLY" -eq 1 ]]; then
    if [[ "$LINK_ONLY" -eq 0 ]]; then
        cargo build --release --manifest-path "$ROOT/Cargo.toml" --bin rsi
    fi
    if [[ ! -x "$RSI_BIN" ]]; then
        echo "Release rsi binary not found at $RSI_BIN" >&2
        exit 1
    fi
    mkdir -p "$BIN_DIR"
    ln -sfn "$RSI_BIN" "$BIN_DIR/rsi"
    if [[ "$TARGET_DIR" != "$ROOT/target" ]]; then
        mkdir -p "$COMPAT_RELEASE_DIR"
        ln -sfn "$RSI_BIN" "$COMPAT_RELEASE_DIR/rsi"
    fi
    echo "Linked:"
    echo "  $BIN_DIR/rsi     -> $RSI_BIN"
    if [[ "$TARGET_DIR" != "$ROOT/target" ]]; then
        echo "Compatibility links:"
        echo "  $COMPAT_RELEASE_DIR/rsi     -> $RSI_BIN"
    fi
    echo "TUI-only install: rsid, rsi-rpc, and rsi-agent-mcp untouched; no daemon restart."
    exit 0
fi

# #1122: by default ask the daemon to restart at a quiet point, so an operator
# rebuild does not cut off running manager and worker turns. NOW=1 / --now
# restarts immediately; a daemon that is positively down (or not under the
# supervisor) is restarted directly.
QUIET_RESTART=0
if [[ "$LINK_ONLY" -eq 0 && "$NO_RESTART" -eq 0 && "$RESTART_NOW" != 1 ]]; then
    QUIET_RESTART=1
    # Keep the prior installed executables out of Cargo's output directory, so
    # the build cannot overwrite what the daemon keeps as `.prev` for rollback.
    stabilize_installed_binaries "$BIN_DIR" "$TARGET_DIR/release" "$RSI_HOME_DIR/install"
fi

if [[ "$LINK_ONLY" -eq 0 ]]; then
    cargo build --release --manifest-path "$ROOT/Cargo.toml" --bin rsi --bin rsid --bin rsi-rpc --bin rsi-agent-mcp --bin rsi-build-rustc --bin rsi-contract-validate --bin rsi-rolling-land --bin rsi-remote --bin rsi-turn-shim --bin rsi-socket-hold
fi

if [[ ! -x "$RSI_BIN" || ! -x "$RSID_BIN" || ! -x "$RSI_RPC_BIN" || ! -x "$RSI_AGENT_MCP_BIN" || ! -x "$BUILD_RUSTC_BIN" || ! -x "$CONTRACT_VALIDATE_BIN" ]]; then
    echo "Release binaries not found at $TARGET_DIR/release" >&2
    exit 1
fi

# #1592: carry the supervisor through the same manifest as the binaries.
# Direct installs below copy it too; quiet installs leave the running path
# alone until the daemon swaps the verified set.
install_supervisor_script "$TARGET_DIR/release"

# Daemon jobs execute this copy directly. Publish it before either install
# path returns, keeping any running cargo-slot on its existing inode.
(
    mkdir -p "$RSI_HOME_DIR/bin"
    cargo_slot_temp="$(mktemp "$RSI_HOME_DIR/bin/.cargo-slot.XXXXXX")"
    trap 'rm -f "$cargo_slot_temp"' EXIT
    cp "$ROOT/scripts/cargo-slot" "$cargo_slot_temp"
    chmod 755 "$cargo_slot_temp"
    mv -f "$cargo_slot_temp" "$RSI_HOME_DIR/bin/cargo-slot"
)

if [[ "$QUIET_RESTART" -eq 1 ]]; then
    # The daemon stages, verifies and swaps the new binaries in at a quiet point
    # (nothing is relinked here: the installed set stays the previous, working
    # one until then) and verifies the running build after the restart.
    quiet_status=0
    request_quiet_restart "$RSI_RPC_BIN" "$(daemon_socket_path)" "$TARGET_DIR/release" \
        || quiet_status=$?
    case "$quiet_status" in
        0) exit 0 ;;
        10) ;;
        *) exit 1 ;;
    esac
fi

# Direct (non-quiet) install: copy the built binaries into INSTALL_DIR
# (atomically, per file) and point every link and the supervisor at that copy.
install_built_binaries "$TARGET_DIR/release" "$INSTALL_DIR"
RSI_BIN="$INSTALL_DIR/rsi"
RSID_BIN="$INSTALL_DIR/rsid"
BUILD_RUSTC_BIN="$INSTALL_DIR/rsi-build-rustc"
RSI_RPC_BIN="$INSTALL_DIR/rsi-rpc"
ROLLING_LAND_BIN="$INSTALL_DIR/rsi-rolling-land"
RSI_AGENT_MCP_BIN="$INSTALL_DIR/rsi-agent-mcp"
CONTRACT_VALIDATE_BIN="$INSTALL_DIR/rsi-contract-validate"
RSI_REMOTE_BIN="$INSTALL_DIR/rsi-remote"
TURN_SHIM_BIN="$INSTALL_DIR/rsi-turn-shim"
SOCKET_HOLD_BIN="$INSTALL_DIR/rsi-socket-hold"

mkdir -p "$BIN_DIR"
ln -sfn "$RSI_BIN" "$BIN_DIR/rsi"
ln -sfn "$RSID_BIN" "$BIN_DIR/rsid"
ln -sfn "$BUILD_RUSTC_BIN" "$BIN_DIR/rsi-build-rustc"
ln -sfn "$RSI_RPC_BIN" "$BIN_DIR/rsi-rpc"
ln -sfn "$RSI_AGENT_MCP_BIN" "$BIN_DIR/rsi-agent-mcp"
ln -sfn "$CONTRACT_VALIDATE_BIN" "$BIN_DIR/rsi-contract-validate"
[[ -x "$RSI_REMOTE_BIN" ]] && ln -sfn "$RSI_REMOTE_BIN" "$BIN_DIR/rsi-remote"
[[ -x "$TURN_SHIM_BIN" ]] && ln -sfn "$TURN_SHIM_BIN" "$BIN_DIR/rsi-turn-shim"
[[ -x "$SOCKET_HOLD_BIN" ]] && ln -sfn "$SOCKET_HOLD_BIN" "$BIN_DIR/rsi-socket-hold"
ln -sfn "$RSI_SPILL_SCRIPT" "$BIN_DIR/rsi-spill"
# The daemon rolling merge queue spawns the lander: it looks next to rsid first,
# then on PATH (rolling_queue.rs LanderLauncher::discover).
if [[ -x "$ROLLING_LAND_BIN" ]]; then
    ln -sfn "$ROLLING_LAND_BIN" "$BIN_DIR/rsi-rolling-land"
else
    echo "warning: $ROLLING_LAND_BIN not built; the daemon merge queue cannot land until it is" >&2
fi

if [[ "$TARGET_DIR" != "$ROOT/target" ]]; then
    mkdir -p "$COMPAT_RELEASE_DIR"
    ln -sfn "$RSI_BIN" "$COMPAT_RELEASE_DIR/rsi"
    ln -sfn "$RSID_BIN" "$COMPAT_RELEASE_DIR/rsid"
    ln -sfn "$BUILD_RUSTC_BIN" "$COMPAT_RELEASE_DIR/rsi-build-rustc"
    ln -sfn "$RSI_RPC_BIN" "$COMPAT_RELEASE_DIR/rsi-rpc"
    ln -sfn "$RSI_AGENT_MCP_BIN" "$COMPAT_RELEASE_DIR/rsi-agent-mcp"
fi

echo "Linked:"
echo "  $BIN_DIR/rsi     -> $RSI_BIN"
echo "  $BIN_DIR/rsid    -> $RSID_BIN"
echo "  $BIN_DIR/rsi-build-rustc -> $BUILD_RUSTC_BIN"
echo "  $BIN_DIR/rsi-rpc -> $RSI_RPC_BIN"
echo "  $BIN_DIR/rsi-agent-mcp -> $RSI_AGENT_MCP_BIN"
echo "  $BIN_DIR/rsi-contract-validate -> $CONTRACT_VALIDATE_BIN"
echo "  $BIN_DIR/rsi-rolling-land -> $ROLLING_LAND_BIN"
echo "  $BIN_DIR/rsi-spill -> $RSI_SPILL_SCRIPT"
if [[ "$TARGET_DIR" != "$ROOT/target" ]]; then
    echo "Compatibility links:"
    echo "  $COMPAT_RELEASE_DIR/rsi     -> $RSI_BIN"
    echo "  $COMPAT_RELEASE_DIR/rsid    -> $RSID_BIN"
    echo "  $COMPAT_RELEASE_DIR/rsi-rpc -> $RSI_RPC_BIN"
    echo "  $COMPAT_RELEASE_DIR/rsi-agent-mcp -> $RSI_AGENT_MCP_BIN"
fi

# A rebuilt binary on disk has no effect on a daemon already running from the
# old one -- replacing the file a symlink points at (or the file itself)
# never touches an already-exec'd process's in-memory code. Restart the live
# daemon here so `make release-install` always leaves the running process
# matching what it just built, instead of silently leaving a stale rsid
# serving requests from before this build.
run_restart_drain_hook() {
    # #929 can install this operator-owned hook to request a safe daemon drain.
    # Its failure prevents the restart. No provider environment is printed.
    local hook="$HOME/.config/rsi/pre-restart-drain"
    if [[ -e "$hook" ]]; then
        if [[ ! -x "$hook" ]]; then
            echo "rsid drain hook is not executable: $hook" >&2
            return 1
        fi
        "$hook" "$(daemon_socket_path)"
    fi
}

restart_rsid() {
    local platform
    platform="$(uname -s)"
    if [[ "$platform" != Linux && "$platform" != Darwin ]]; then
        echo "Automatic rsid restart is unsupported on $platform; use --no-restart." >&2
        return 1
    fi

    # Match by exact name, current user only -- never touch another user's
    # process, and never match on a substring (e.g. a path containing "rsid").
    local old_pids
    old_pids="$(rsid_running_pids)"

    if [[ "$platform" == Linux ]]; then
        load_rsid_scope_settings
    fi

    if [[ "$platform" == Linux ]] && systemctl --user cat rsid.service >/dev/null 2>&1; then
        if [[ -n "$old_pids" ]] && ! systemctl --user is-active --quiet rsid.service; then
            echo "rsid.service is installed but a legacy daemon is running; stop the legacy daemon before switching launchers." >&2
            return 1
        fi
        run_restart_drain_hook
        provision_worker_slice
        # Keep verified aggregate limits across a reboot of the managed unit.
        # A busy slice with conflicting limits is left untouched by
        # provision_worker_slice and is not persisted here.
        if verify_worker_slice; then
            systemctl --user set-property rsi-workers.slice \
                "MemoryHigh=${WORKER_SCOPE_MEMORY_HIGH_MIB}M" \
                "MemoryMax=${WORKER_SCOPE_MEMORY_MAX_MIB}M" \
                "MemorySwapMax=${WORKER_SCOPE_MEMORY_SWAP_MAX_MIB}M" \
                "CPUWeight=${WORKER_SCOPE_CPU_WEIGHT}"
        fi
        systemctl --user set-property rsid.service \
            "MemoryHigh=${RSID_SCOPE_MEMORY_HIGH_MIB}M" \
            "MemoryMax=${RSID_SCOPE_MEMORY_MAX_MIB}M" \
            "MemorySwapMax=${RSID_SCOPE_MEMORY_SWAP_MAX_MIB}M" \
            "CPUWeight=${RSID_SCOPE_CPU_WEIGHT}"
        echo "Restarting managed rsid.service ..."
        systemctl --user restart rsid.service
        wait_for_rsid_health "$RSI_RPC_BIN" "$(daemon_socket_path)" 90 rsid.service
        verify_rsid_scope rsid.service
        return
    fi

    if [[ -z "$old_pids" ]]; then
        echo "rsid is not currently running -- skipping restart."
        return 0
    fi

    if [[ ! -x "$SOCKET_HOLD_BIN" ]]; then
        echo "Socket holder not installed at $SOCKET_HOLD_BIN; refusing a restart without the front door." >&2
        return 1
    fi
    run_restart_drain_hook

    echo "Restarting rsid (was running as PID(s): $(echo "$old_pids" | tr '\n' ' ')) ..."

    # Legacy supervisor path. The managed service uses systemd's stop signal.
    # shellcheck disable=SC2086
    kill -INT $old_pids 2>/dev/null || true

    local waited=0
    while [[ "$waited" -lt 900 ]]; do
        if [[ -z "$(rsid_running_pids)" ]]; then
            break
        fi
        sleep 0.1
        waited=$((waited + 1))
    done

    if [[ -n "$(rsid_running_pids)" ]]; then
        echo "rsid did not stop within 90s; leaving it running and refusing a second daemon." >&2
        return 1
    fi

    if [[ "$platform" == Linux ]]; then
        provision_worker_slice
    fi

    mkdir -p "$RSI_HOME_DIR"
    local scope_unit=""
    if [[ "$platform" == Linux ]]; then
        # A collected service survives the installer and the agent process tree.
        # The holder and supervisor retain the existing aggregate limits.
        scope_unit="rsid-install-$(date +%s)-$$-$RANDOM.service"
        systemd-run --user --collect --service-type=exec --unit="$scope_unit" \
            --slice=user.slice \
            --property="MemoryHigh=${RSID_SCOPE_MEMORY_HIGH_MIB}M" \
            --property="MemoryMax=${RSID_SCOPE_MEMORY_MAX_MIB}M" \
            --property="MemorySwapMax=${RSID_SCOPE_MEMORY_SWAP_MAX_MIB}M" \
            --property="CPUWeight=${RSID_SCOPE_CPU_WEIGHT}" \
            --property=StandardInput=null \
            --property="StandardOutput=append:$DAEMON_LOG" \
            --property="StandardError=append:$DAEMON_LOG" \
            --setenv="PATH=$PATH" \
            --setenv="RSI_SOCKET=$(daemon_socket_path)" \
            --setenv="RSI_DAEMON_SOCKET_PATH=$(daemon_socket_path)" \
            -- "$SOCKET_HOLD_BIN" "$(daemon_socket_path)" -- \
            "$INSTALL_DIR/rsid-supervisor.sh" "$RSID_BIN"
    else
        RSI_SOCKET="$(daemon_socket_path)" RSI_DAEMON_SOCKET_PATH="$(daemon_socket_path)" \
        nohup "$SOCKET_HOLD_BIN" "$(daemon_socket_path)" -- \
            "$INSTALL_DIR/rsid-supervisor.sh" "$RSID_BIN" \
            </dev/null >>"$DAEMON_LOG" 2>&1 &
        disown "$!"
    fi

    local socket_path
    socket_path="$(daemon_socket_path)"
    if ! wait_for_rsid_health "$RSI_RPC_BIN" "$socket_path" 90; then
        echo "rsid did not answer GetHealthStatus on $socket_path after restart -- check $DAEMON_LOG" >&2
        exit 1
    fi
    if [[ "$platform" == Linux ]]; then
        verify_rsid_scope "$scope_unit"
        echo "rsid supervisor restarted in $scope_unit, logging to $DAEMON_LOG"
    else
        echo "rsid restarted on macOS, logging to $DAEMON_LOG"
    fi
}

load_rsid_scope_settings() {
    # Seed the shared launcher snapshot once. UpdateDaemonConfig replaces it
    # after the operator changes any scope limit.
    mkdir -p "$RSI_HOME_DIR"
    if [[ ! -e "$RSID_SCOPE_SETTINGS" ]]; then
        local temp_settings
        temp_settings="$(mktemp "$RSI_HOME_DIR/.rsid-scope.XXXXXXXX")"
        cat >"$temp_settings" <<'EOF'
rsid_scope_memory_high_mib=6144
rsid_scope_memory_max_mib=8192
rsid_scope_memory_swap_max_mib=0
rsid_scope_cpu_weight=20
worker_scope_memory_high_mib=6144
worker_scope_memory_max_mib=8192
worker_scope_memory_swap_max_mib=0
worker_scope_cpu_weight=20
EOF
        # Publish a complete file without replacing a concurrent TUI launch
        # or an operator-updated snapshot.
        if ! ln "$temp_settings" "$RSID_SCOPE_SETTINGS" 2>/dev/null \
            && [[ ! -e "$RSID_SCOPE_SETTINGS" ]]; then
            rm -f "$temp_settings"
            echo "Cannot create rsid scope settings at $RSID_SCOPE_SETTINGS" >&2
            exit 1
        fi
        rm -f "$temp_settings"
    fi
    if [[ ! -r "$RSID_SCOPE_SETTINGS" ]]; then
        echo "Cannot read rsid scope settings at $RSID_SCOPE_SETTINGS" >&2
        exit 1
    fi

    local seen_high=0 seen_max=0 seen_swap=0 seen_weight=0 key value
    local seen_worker_high=0 seen_worker_max=0 seen_worker_swap=0 seen_worker_weight=0
    WORKER_SCOPE_MEMORY_HIGH_MIB=6144
    WORKER_SCOPE_MEMORY_MAX_MIB=8192
    WORKER_SCOPE_MEMORY_SWAP_MAX_MIB=0
    WORKER_SCOPE_CPU_WEIGHT=20
    while IFS='=' read -r key value || [[ -n "${key:-}${value:-}" ]]; do
        [[ -z "${key//[[:space:]]/}" || "$key" == \#* ]] && continue
        value="${value//[[:space:]]/}"
        if [[ ! "$value" =~ ^[0-9]+$ ]]; then
            echo "Invalid numeric value in $RSID_SCOPE_SETTINGS for $key" >&2
            exit 1
        fi
        case "$key" in
            rsid_scope_memory_high_mib)
                (( seen_high == 0 )) || { echo "Duplicate $key in $RSID_SCOPE_SETTINGS" >&2; exit 1; }
                seen_high=1; RSID_SCOPE_MEMORY_HIGH_MIB="$value" ;;
            rsid_scope_memory_max_mib)
                (( seen_max == 0 )) || { echo "Duplicate $key in $RSID_SCOPE_SETTINGS" >&2; exit 1; }
                seen_max=1; RSID_SCOPE_MEMORY_MAX_MIB="$value" ;;
            rsid_scope_memory_swap_max_mib)
                (( seen_swap == 0 )) || { echo "Duplicate $key in $RSID_SCOPE_SETTINGS" >&2; exit 1; }
                seen_swap=1; RSID_SCOPE_MEMORY_SWAP_MAX_MIB="$value" ;;
            rsid_scope_cpu_weight)
                (( seen_weight == 0 )) || { echo "Duplicate $key in $RSID_SCOPE_SETTINGS" >&2; exit 1; }
                seen_weight=1; RSID_SCOPE_CPU_WEIGHT="$value" ;;
            worker_scope_memory_high_mib)
                (( seen_worker_high == 0 )) || { echo "Duplicate $key in $RSID_SCOPE_SETTINGS" >&2; exit 1; }
                seen_worker_high=1; WORKER_SCOPE_MEMORY_HIGH_MIB="$value" ;;
            worker_scope_memory_max_mib)
                (( seen_worker_max == 0 )) || { echo "Duplicate $key in $RSID_SCOPE_SETTINGS" >&2; exit 1; }
                seen_worker_max=1; WORKER_SCOPE_MEMORY_MAX_MIB="$value" ;;
            worker_scope_memory_swap_max_mib)
                (( seen_worker_swap == 0 )) || { echo "Duplicate $key in $RSID_SCOPE_SETTINGS" >&2; exit 1; }
                seen_worker_swap=1; WORKER_SCOPE_MEMORY_SWAP_MAX_MIB="$value" ;;
            worker_scope_cpu_weight)
                (( seen_worker_weight == 0 )) || { echo "Duplicate $key in $RSID_SCOPE_SETTINGS" >&2; exit 1; }
                seen_worker_weight=1; WORKER_SCOPE_CPU_WEIGHT="$value" ;;
            *) echo "Unknown rsid scope setting $key in $RSID_SCOPE_SETTINGS" >&2; exit 1 ;;
        esac
    done <"$RSID_SCOPE_SETTINGS"

    if [[ "$seen_high" -ne 1 || "$seen_max" -ne 1 || "$seen_swap" -ne 1 || "$seen_weight" -ne 1 \
        || "$RSID_SCOPE_MEMORY_HIGH_MIB" -lt 256 || "$RSID_SCOPE_MEMORY_HIGH_MIB" -gt 1048576 \
        || "$RSID_SCOPE_MEMORY_MAX_MIB" -lt 256 || "$RSID_SCOPE_MEMORY_MAX_MIB" -gt 1048576 \
        || "$RSID_SCOPE_MEMORY_HIGH_MIB" -ge "$RSID_SCOPE_MEMORY_MAX_MIB" \
        || "$RSID_SCOPE_MEMORY_SWAP_MAX_MIB" -gt 1048576 \
        || "$RSID_SCOPE_CPU_WEIGHT" -lt 1 || "$RSID_SCOPE_CPU_WEIGHT" -gt 10000 ]]; then
        echo "Invalid or incomplete rsid scope settings in $RSID_SCOPE_SETTINGS" >&2
        exit 1
    fi
    local worker_keys_seen=$((seen_worker_high + seen_worker_max + seen_worker_swap + seen_worker_weight))
    if [[ "$worker_keys_seen" -ne 0 && "$worker_keys_seen" -ne 4 ]]; then
        echo "Incomplete worker slice settings in $RSID_SCOPE_SETTINGS" >&2
        exit 1
    fi
    if [[ "$WORKER_SCOPE_MEMORY_HIGH_MIB" -lt 256 || "$WORKER_SCOPE_MEMORY_HIGH_MIB" -gt 1048576 \
        || "$WORKER_SCOPE_MEMORY_MAX_MIB" -lt 256 || "$WORKER_SCOPE_MEMORY_MAX_MIB" -gt 1048576 \
        || "$WORKER_SCOPE_MEMORY_HIGH_MIB" -ge "$WORKER_SCOPE_MEMORY_MAX_MIB" \
        || "$WORKER_SCOPE_MEMORY_SWAP_MAX_MIB" -gt 1048576 \
        || "$WORKER_SCOPE_CPU_WEIGHT" -lt 1 || "$WORKER_SCOPE_CPU_WEIGHT" -gt 10000 ]]; then
        echo "Invalid worker slice settings in $RSID_SCOPE_SETTINGS" >&2
        exit 1
    fi
}

verify_worker_slice() {
    local properties control_group active_state memory_high memory_max memory_swap_max cpu_weight
    properties="$(systemctl --user show rsi-workers.slice \
        --property=ActiveState --property=ControlGroup --property=MemoryHigh \
        --property=MemoryMax --property=MemorySwapMax --property=CPUWeight)" || return 1
    active_state="$(sed -n 's/^ActiveState=//p' <<<"$properties")"
    control_group="$(sed -n 's/^ControlGroup=//p' <<<"$properties")"
    memory_high="$(sed -n 's/^MemoryHigh=//p' <<<"$properties")"
    memory_max="$(sed -n 's/^MemoryMax=//p' <<<"$properties")"
    memory_swap_max="$(sed -n 's/^MemorySwapMax=//p' <<<"$properties")"
    cpu_weight="$(sed -n 's/^CPUWeight=//p' <<<"$properties")"
    [[ "$active_state" == active && "$control_group" == /user.slice/* \
        && "$control_group" == */rsi-workers.slice \
        && "$memory_high" =~ ^[0-9]+$ && "$memory_max" =~ ^[0-9]+$ \
        && "$memory_swap_max" =~ ^[0-9]+$ && "$cpu_weight" =~ ^[0-9]+$ \
        && "$memory_high" -eq $((WORKER_SCOPE_MEMORY_HIGH_MIB * 1024 * 1024)) \
        && "$memory_max" -eq $((WORKER_SCOPE_MEMORY_MAX_MIB * 1024 * 1024)) \
        && "$memory_swap_max" -eq $((WORKER_SCOPE_MEMORY_SWAP_MAX_MIB * 1024 * 1024)) \
        && "$cpu_weight" -eq "$WORKER_SCOPE_CPU_WEIGHT" ]]
}

worker_slice_is_empty() {
    local properties control_group populated descendants
    properties="$(systemctl --user show rsi-workers.slice --property=ControlGroup)" || return 1
    control_group="$(sed -n 's/^ControlGroup=//p' <<<"$properties")"
    [[ "$control_group" == "/user.slice/user-${EUID}.slice/user@${EUID}.service/rsi.slice/rsi-workers.slice" ]] || return 1
    populated="$(sed -n 's/^populated \([0-9][0-9]*\)$/\1/p' "/sys/fs/cgroup${control_group}/cgroup.events")" || return 1
    descendants="$(sed -n 's/^nr_descendants \([0-9][0-9]*\)$/\1/p' "/sys/fs/cgroup${control_group}/cgroup.stat")" || return 1
    [[ "$populated" == 0 && "$descendants" == 0 ]]
}

provision_worker_slice() {
    local before
    before="$(systemctl --user show rsi-workers.slice --property=ActiveState)" || {
        echo "Cannot inspect rsi-workers.slice." >&2
        exit 1
    }
    if [[ "$before" == ActiveState=active ]]; then
        if verify_worker_slice; then
            return 0
        fi
        if ! worker_slice_is_empty; then
            echo "Active rsi-workers.slice differs from configured limits and may contain workers; daemon will restart to reap owned workers, while new worker admission stays closed." >&2
            return 0
        fi
        echo "Reconciling empty rsi-workers.slice with configured limits." >&2
    fi
    systemctl --user set-property --runtime rsi-workers.slice \
        "MemoryHigh=${WORKER_SCOPE_MEMORY_HIGH_MIB}M" \
        "MemoryMax=${WORKER_SCOPE_MEMORY_MAX_MIB}M" \
        "MemorySwapMax=${WORKER_SCOPE_MEMORY_SWAP_MAX_MIB}M" \
        "CPUWeight=${WORKER_SCOPE_CPU_WEIGHT}" || exit 1
    systemctl --user start rsi-workers.slice || exit 1
    if ! verify_worker_slice; then
        echo "rsi-workers.slice did not acquire configured aggregate limits; refusing daemon start." >&2
        exit 1
    fi
}

verify_rsid_scope() {
    local scope_unit="$1"
    local properties control_group active_state memory_high memory_max memory_swap_max cpu_weight
    properties="$(systemctl --user show "$scope_unit" \
        --property=ActiveState --property=ControlGroup --property=MemoryHigh \
        --property=MemoryMax --property=MemorySwapMax --property=CPUWeight)" || {
        echo "Cannot inspect effective rsid scope limits." >&2
        exit 1
    }
    active_state="$(sed -n 's/^ActiveState=//p' <<<"$properties")"
    control_group="$(sed -n 's/^ControlGroup=//p' <<<"$properties")"
    memory_high="$(sed -n 's/^MemoryHigh=//p' <<<"$properties")"
    memory_max="$(sed -n 's/^MemoryMax=//p' <<<"$properties")"
    memory_swap_max="$(sed -n 's/^MemorySwapMax=//p' <<<"$properties")"
    cpu_weight="$(sed -n 's/^CPUWeight=//p' <<<"$properties")"
    if [[ "$active_state" != active || "$control_group" != /user.slice/* \
        || "$control_group" != */"$scope_unit" || ! "$memory_high" =~ ^[0-9]+$ \
        || ! "$memory_max" =~ ^[0-9]+$ || ! "$memory_swap_max" =~ ^[0-9]+$ \
        || ! "$cpu_weight" =~ ^[0-9]+$ \
        || "$memory_high" -ne $((RSID_SCOPE_MEMORY_HIGH_MIB * 1024 * 1024)) \
        || "$memory_max" -ne $((RSID_SCOPE_MEMORY_MAX_MIB * 1024 * 1024)) \
        || "$memory_swap_max" -ne $((RSID_SCOPE_MEMORY_SWAP_MAX_MIB * 1024 * 1024)) \
        || "$cpu_weight" -ne "$RSID_SCOPE_CPU_WEIGHT" ]]; then
        echo "$scope_unit is active without the configured effective limits or user-slice placement." >&2
        systemctl --user stop "$scope_unit" >/dev/null 2>&1 || true

        exit 1
    fi
}

if [[ "$NO_RESTART" -eq 0 ]]; then
    restart_rsid
else
    echo "Skipping rsid restart (--no-restart)."
fi
