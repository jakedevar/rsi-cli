#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
BIN_DIR="${RSI_INSTALL_BIN_DIR:-$HOME/.local/bin}"
RSI_HOME_DIR="$HOME/.rsi"
DAEMON_LOG="$RSI_HOME_DIR/daemon.log"
RSID_SCOPE_SETTINGS="$RSI_HOME_DIR/rsid-scope.env"
LINK_ONLY=0
NO_RESTART=0
TUI_ONLY=0

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
        --tui-only)
            # Build and link only the `rsi` TUI binary. The daemon is left
            # untouched, so this never restarts rsid.
            TUI_ONLY=1
            NO_RESTART=1
            shift
            ;;
        *)
            echo "Unknown argument: $1" >&2
            echo "Usage: $0 [--link-only] [--no-restart] [--tui-only]" >&2
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
RSI_RPC_BIN="$TARGET_DIR/release/rsi-rpc"
RSI_AGENT_MCP_BIN="$TARGET_DIR/release/rsi-agent-mcp"
COMPAT_RELEASE_DIR="$ROOT/target/release"

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

if [[ "$LINK_ONLY" -eq 0 ]]; then
    cargo build --release --manifest-path "$ROOT/Cargo.toml" --bin rsi --bin rsid --bin rsi-rpc --bin rsi-agent-mcp
fi

if [[ ! -x "$RSI_BIN" || ! -x "$RSID_BIN" || ! -x "$RSI_RPC_BIN" || ! -x "$RSI_AGENT_MCP_BIN" ]]; then
    echo "Release binaries not found at $TARGET_DIR/release" >&2
    exit 1
fi

mkdir -p "$BIN_DIR"
ln -sfn "$RSI_BIN" "$BIN_DIR/rsi"
ln -sfn "$RSID_BIN" "$BIN_DIR/rsid"
ln -sfn "$RSI_RPC_BIN" "$BIN_DIR/rsi-rpc"
ln -sfn "$RSI_AGENT_MCP_BIN" "$BIN_DIR/rsi-agent-mcp"

if [[ "$TARGET_DIR" != "$ROOT/target" ]]; then
    mkdir -p "$COMPAT_RELEASE_DIR"
    ln -sfn "$RSI_BIN" "$COMPAT_RELEASE_DIR/rsi"
    ln -sfn "$RSID_BIN" "$COMPAT_RELEASE_DIR/rsid"
    ln -sfn "$RSI_RPC_BIN" "$COMPAT_RELEASE_DIR/rsi-rpc"
    ln -sfn "$RSI_AGENT_MCP_BIN" "$COMPAT_RELEASE_DIR/rsi-agent-mcp"
fi

echo "Linked:"
echo "  $BIN_DIR/rsi     -> $RSI_BIN"
echo "  $BIN_DIR/rsid    -> $RSID_BIN"
echo "  $BIN_DIR/rsi-rpc -> $RSI_RPC_BIN"
echo "  $BIN_DIR/rsi-agent-mcp -> $RSI_AGENT_MCP_BIN"
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
restart_rsid() {
    # Match by exact name, current user only -- never touch another user's
    # process, and never match on a substring (e.g. a path containing "rsid").
    local old_pids
    old_pids="$(pgrep -x -u "$(id -u)" rsid || true)"

    if [[ -z "$old_pids" ]]; then
        echo "rsid is not currently running -- skipping restart."
        return 0
    fi

    load_rsid_scope_settings

    echo "Restarting rsid (was running as PID(s): $(echo "$old_pids" | tr '\n' ' ')) ..."

    # SIGINT, not SIGTERM: rsid's shutdown listener is tokio::signal::ctrl_c(),
    # which only observes SIGINT. SIGTERM would skip that handler entirely and
    # fall straight to the OS default action.
    # shellcheck disable=SC2086
    kill -INT $old_pids 2>/dev/null || true

    local waited=0
    while [[ "$waited" -lt 100 ]]; do
        if ! kill -0 $old_pids 2>/dev/null; then
            break
        fi
        sleep 0.1
        waited=$((waited + 1))
    done

    # shellcheck disable=SC2086
    if kill -0 $old_pids 2>/dev/null; then
        echo "rsid did not exit within 10s of SIGINT -- sending SIGKILL." >&2
        kill -KILL $old_pids 2>/dev/null || true
        sleep 0.2
    fi

    if pgrep -x -u "$(id -u)" rsid >/dev/null 2>&1; then
        echo "rsid did not stop; refusing to start a second daemon." >&2
        exit 1
    fi

    mkdir -p "$RSI_HOME_DIR"
    nohup systemd-run --user --scope --collect --unit=rsid.scope \
        --slice=user.slice \
        --property="MemoryHigh=${RSID_SCOPE_MEMORY_HIGH_MIB}M" \
        --property="MemoryMax=${RSID_SCOPE_MEMORY_MAX_MIB}M" \
        --property="MemorySwapMax=${RSID_SCOPE_MEMORY_SWAP_MAX_MIB}M" \
        --property="CPUWeight=${RSID_SCOPE_CPU_WEIGHT}" \
        -- "$ROOT/scripts/rsid-supervisor.sh" "$RSID_BIN" \
        </dev/null >>"$DAEMON_LOG" 2>&1 &
    local launcher_pid=$!
    disown "$launcher_pid"

    local started=0
    for _ in {1..100}; do
        if systemctl --user is-active --quiet rsid.scope \
            && pgrep -x -u "$(id -u)" rsid >/dev/null 2>&1; then
            started=1
            break
        fi
        if ! kill -0 "$launcher_pid" 2>/dev/null; then
            break
        fi
        sleep 0.1
    done
    if [[ "$started" -ne 1 ]]; then
        echo "rsid did not become active in rsid.scope -- check $DAEMON_LOG" >&2
        exit 1
    fi
    verify_rsid_scope
    echo "rsid supervisor restarted in rsid.scope, logging to $DAEMON_LOG"
}

load_rsid_scope_settings() {
    # Seed the shared launcher snapshot once. UpdateDaemonConfig replaces it
    # after the operator changes any scope limit.
    mkdir -p "$RSI_HOME_DIR"
    if [[ ! -e "$RSID_SCOPE_SETTINGS" ]]; then
        umask 077
        cat >"$RSID_SCOPE_SETTINGS" <<'EOF'
rsid_scope_memory_high_mib=6144
rsid_scope_memory_max_mib=8192
rsid_scope_memory_swap_max_mib=0
rsid_scope_cpu_weight=20
EOF
    fi
    if [[ ! -r "$RSID_SCOPE_SETTINGS" ]]; then
        echo "Cannot read rsid scope settings at $RSID_SCOPE_SETTINGS" >&2
        exit 1
    fi

    local seen_high=0 seen_max=0 seen_swap=0 seen_weight=0 key value
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
}

verify_rsid_scope() {
    local properties control_group active_state memory_high memory_max memory_swap_max cpu_weight
    properties="$(systemctl --user show rsid.scope \
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
        || "$control_group" != */rsid.scope || ! "$memory_high" =~ ^[0-9]+$ \
        || ! "$memory_max" =~ ^[0-9]+$ || ! "$memory_swap_max" =~ ^[0-9]+$ \
        || ! "$cpu_weight" =~ ^[0-9]+$ \
        || "$memory_high" -ne $((RSID_SCOPE_MEMORY_HIGH_MIB * 1024 * 1024)) \
        || "$memory_max" -ne $((RSID_SCOPE_MEMORY_MAX_MIB * 1024 * 1024)) \
        || "$memory_swap_max" -ne $((RSID_SCOPE_MEMORY_SWAP_MAX_MIB * 1024 * 1024)) \
        || "$cpu_weight" -ne "$RSID_SCOPE_CPU_WEIGHT" ]]; then
        echo "rsid.scope is active without the configured effective limits or user-slice placement." >&2
        systemctl --user stop rsid.scope >/dev/null 2>&1 || true

        exit 1
    fi
}

if [[ "$NO_RESTART" -eq 0 ]]; then
    restart_rsid
else
    echo "Skipping rsid restart (--no-restart)."
fi
