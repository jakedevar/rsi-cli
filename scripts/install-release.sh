#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
source "$ROOT/scripts/rsid-processes.sh"
source "$ROOT/scripts/rsid-health.sh"
source "$ROOT/scripts/rsid-quiet-restart.sh"
source "$ROOT/scripts/rsid-scope.sh"
source "$ROOT/scripts/rsid-launchd.sh"
source "$ROOT/scripts/release-target-guard.sh"
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
# #136: --stage-dir DIR writes the built set into DIR and touches nothing live.
STAGE_DIR=""
# #938: --link-only links only binaries this worktree built at its HEAD;
# --allow-foreign-build links whatever the target holds.
ALLOW_FOREIGN_BUILD=0
# Restart rsid immediately (today's behaviour) instead of at a quiet point:
# --now, or NOW=1 in the environment (make release-install NOW=1).
RESTART_NOW="${NOW:-0}"

usage() {
    cat <<USAGE
Usage: $0 [--link-only [--allow-foreign-build]] [--no-restart] [--tui-only] [--now]
       $0 --stage-dir DIR [--link-only [--allow-foreign-build]]

  (default)         Build, install into ~/.rsi/install (plus the ~/.rsi/bin and
                    ~/.rsi/lib tools), link ~/.local/bin and restart rsid at a
                    quiet point (--now: immediately).
  --no-restart      Install and link as usual but do NOT restart rsid. This is
                    not staging: the live install (~/.rsi/install, the links, the
                    tools) is still overwritten.
  --link-only       Skip the build; install and link what this worktree built.
  --tui-only        Build, install and link only the rsi TUI; never restarts rsid.
  --stage-dir DIR   Staging only (#136). Build (or, with --link-only, reuse) the
                    release set and write it into DIR, the binaries_dir of
                    AgentRequestDeploy: the binaries, rsid-supervisor.sh,
                    cargo-slot (the full scripts/cargo-slot), target-pool and rsi-spill,
                    all at DIR's top level. Touches nothing live: no
                    ~/.rsi/install, bin or lib, no ~/.local/bin links, no
                    supervisor, no restart. Prints a manifest with the sha256
                    of each staged file (also written to DIR/MANIFEST.sha256).
                    DIR must not overlap a live path and must be new or an
                    empty directory; the set is written to a private tree
                    beside DIR and renamed into place. A build refuses while a
                    live link resolves into the cargo release directory.
USAGE
}

while (($#)); do
    case "$1" in
        -h|--help)
            usage
            exit 0
            ;;
        --stage-dir)
            if (($# < 2)) || [[ -z "$2" ]]; then
                echo "--stage-dir needs a directory" >&2
                exit 1
            fi
            STAGE_DIR="$2"
            shift 2
            ;;
        --stage-dir=*)
            STAGE_DIR="${1#--stage-dir=}"
            if [[ -z "$STAGE_DIR" ]]; then
                echo "--stage-dir needs a directory" >&2
                exit 1
            fi
            shift
            ;;
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
        --allow-foreign-build)
            ALLOW_FOREIGN_BUILD=1
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
            usage >&2
            exit 1
            ;;
    esac
done

# #136: staging never restarts and never touches the live install, so it cannot
# combine with the mode whose point is the live TUI install.
if [[ -n "$STAGE_DIR" ]]; then
    if [[ "$TUI_ONLY" -eq 1 ]]; then
        echo "--stage-dir cannot be combined with --tui-only." >&2
        exit 1
    fi
    NO_RESTART=1
fi

# canonical_path <path>: the physical path, every existing symlink resolved and
# missing trailing components kept. Portable (#262, hub #2802): no GNU-only
# `readlink -m`.
canonical_path() {
    python3 -c 'import os, sys; print(os.path.realpath(sys.argv[1]))' "$1"
}

# paths_overlap <a> <b>: whether one canonical path is, or lies under, the other.
paths_overlap() {
    [[ "$1" == "$2" || "$1" == "$2"/* || "$2" == "$1"/* ]]
}

# stage_refuse_overlap <stage>: exit when <stage> overlaps a live path (#136).
stage_refuse_overlap() {
    local stage live resolved
    stage="$(canonical_path "$1")"
    for live in "$INSTALL_DIR" "$RSI_HOME_DIR/bin" "$RSI_HOME_DIR/lib" "$RSI_HOME_DIR/install-kept" "$BIN_DIR"; do
        resolved="$(canonical_path "$live")"
        paths_overlap "$stage" "$resolved" || continue
        echo "Refusing to stage into $stage: it overlaps the live path $resolved (#136)." >&2
        exit 1
    done
}

# stage_refuse_existing <stage>: exit unless <stage> is new or an empty
# directory. The staged set is renamed into place as one fresh tree (#262, hub
# #2800), so nothing already in DIR (a symlink into a live path) is written through.
stage_refuse_existing() {
    local stage
    stage="$(canonical_path "$1")"
    [[ -e "$stage" ]] || return 0
    if [[ -d "$stage" && -z "$(ls -A "$stage")" ]]; then
        return 0
    fi
    echo "Refusing to stage into $stage: it already exists and is not an empty directory; stage into a new directory (#262)." >&2
    exit 1
}

# stage_refuse_live_links_into_build: before a staging build, exit when a live
# link resolves into the cargo release directory (#262, hub #2801). The build
# overwrites those files and its #938 cleanup removes them, and staging never
# repoints live links, so such a link would change or dangle.
stage_refuse_live_links_into_build() {
    local dir links
    local -a dirs=()
    [[ "$LINK_ONLY" -eq 0 ]] || return 0
    while IFS= read -r dir; do
        dirs+=("$dir")
    done < <(release_guard_link_dir_list "$ROOT" "$BIN_DIR")
    links="$(release_guard_links_into "$TARGET_DIR/release" "${dirs[@]}" \
        "$INSTALL_DIR" "$RSI_HOME_DIR/bin" "$RSI_HOME_DIR/lib")"
    [[ -n "$links" ]] || return 0
    echo "Refusing to stage: these live links resolve into the build output $TARGET_DIR/release, which a staging build would overwrite or clean (#262):" >&2
    sed 's/^/  /' <<<"$links" >&2
    echo "Repoint them first (make release-install links ~/.rsi/install), or stage an existing build with --link-only." >&2
    exit 1
}

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
# repo script (a thin shim over `rsi-rpc spill`). hub #2958: the live link points
# at a stable copy under ~/.rsi/bin (like cargo-slot and target-pool), never into
# the checkout that ran the installer, which may be removed later.
RSI_SPILL_SOURCE="$ROOT/scripts/rsi-spill"
RSI_SPILL_SCRIPT="$RSI_HOME_DIR/bin/rsi-spill"
COMPAT_RELEASE_DIR="$ROOT/target/release"
# #1164: one source of truth for "the rsid the supervisor runs". Cargo's output
# is only a build product. The daemon, the supervisor and the ~/.local/bin links
# all use the copy under INSTALL_DIR, which is where AgentRequestDeploy and
# RequestOperatorRestart swap new binaries in, so a deploy restarts the very
# binary it installed.
INSTALL_DIR="$RSI_HOME_DIR/install"

# #136/#262: refuse a bad staging DIR before spending a build on it.
if [[ -n "$STAGE_DIR" ]]; then
    stage_refuse_overlap "$STAGE_DIR"
    stage_refuse_existing "$STAGE_DIR"
fi

if [[ "$LINK_ONLY" -eq 0 ]]; then
    # The test seam (fake capabilities, route-validation bypasses) must never
    # ship; refuse before spending a build on it (#1021 S4).
    "$ROOT/scripts/check-release-seam.sh"
fi

# #938: one release build or link per target at a time, and never another
# worktree's artifacts (scripts/release-target-guard.sh). The lock is released
# before any daemon restart, so a launched daemon never holds it.
release_guard_lock "$TARGET_DIR"
# #2180: no cleanup may remove a file an installed link resolves to.
# Staging has no installed links to protect, and the guard's repoint step would
# otherwise rewrite live links in $BIN_DIR (#136).
# A staging build instead refuses while a live link resolves into its output (#262).
if [[ -z "$STAGE_DIR" ]]; then
    release_guard_set_link_dirs "$ROOT" "$BIN_DIR" "$RSI_HOME_DIR/install-kept"
else
    stage_refuse_live_links_into_build
fi
if [[ "$LINK_ONLY" -eq 1 ]]; then
    if [[ "$ALLOW_FOREIGN_BUILD" -eq 1 ]]; then
        echo "warning: --allow-foreign-build: linking $TARGET_DIR/release without checking which worktree built it" >&2
    elif [[ "$TUI_ONLY" -eq 1 ]]; then
        release_guard_check_link "$ROOT" "$TARGET_DIR" || exit 1
    else
        release_guard_check_link "$ROOT" "$TARGET_DIR" "$RSID_BIN" || exit 1
    fi
fi

if [[ "$TUI_ONLY" -eq 1 ]]; then
    if [[ "$LINK_ONLY" -eq 0 ]]; then
        release_guard_prepare "$ROOT" "$TARGET_DIR"
        cargo build --release --manifest-path "$ROOT/Cargo.toml" --bin rsi 8>&-
        release_guard_finish "$ROOT" "$TARGET_DIR"
    fi
    if [[ ! -x "$RSI_BIN" ]]; then
        echo "Release rsi binary not found at $RSI_BIN" >&2
        exit 1
    fi
    # #2180: link a stable copy, never Cargo's output, which another worktree's
    # build into a shared target may clean. Same atomic copy as the full install.
    mkdir -p "$INSTALL_DIR"
    rsi_temp="$(mktemp "$INSTALL_DIR/.rsi.XXXXXX")"
    cp -p "$RSI_BIN" "$rsi_temp"
    chmod 755 "$rsi_temp"
    mv -f "$rsi_temp" "$INSTALL_DIR/rsi"
    RSI_BIN="$INSTALL_DIR/rsi"
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
    # #2188: links now point at ~/.rsi/install; drop kept copies none uses.
    release_guard_prune_kept
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

build_release_binaries() {
    # `8>&-`: no cargo child (an sccache server) inherits the target lock.
    cargo build --release --manifest-path "$ROOT/Cargo.toml" --bin rsi --bin rsid --bin rsi-rpc --bin rsi-agent-mcp --bin rsi-build-rustc --bin rsi-contract-validate --bin rsi-rolling-land --bin rsi-remote --bin rsi-turn-shim --bin rsi-socket-hold 8>&-
}

if [[ "$LINK_ONLY" -eq 0 ]]; then
    release_guard_prepare "$ROOT" "$TARGET_DIR"
    build_release_binaries
    # #938: rsid embeds the commit it was built from. A different commit means
    # Cargo reused artifacts this checkout did not compile; rebuild this
    # workspace's crates once, and never install a binary that still differs.
    if [[ -x "$RSID_BIN" ]] && ! release_guard_check_sha "$ROOT" "$RSID_BIN"; then
        echo "Rebuilding this workspace's release crates from $ROOT once (#938) ..." >&2
        release_guard_clean_workspace "$ROOT" "$TARGET_DIR"
        build_release_binaries
        if ! release_guard_check_sha "$ROOT" "$RSID_BIN"; then
            echo "Refusing to install: the rebuilt rsid still does not embed this checkout's commit (#938)." >&2
            exit 1
        fi
    fi
    release_guard_finish "$ROOT" "$TARGET_DIR"
fi

if [[ ! -x "$RSI_BIN" || ! -x "$RSID_BIN" || ! -x "$RSI_RPC_BIN" || ! -x "$RSI_AGENT_MCP_BIN" || ! -x "$BUILD_RUSTC_BIN" || ! -x "$CONTRACT_VALIDATE_BIN" ]]; then
    echo "Release binaries not found at $TARGET_DIR/release" >&2
    exit 1
fi

# #136: staging-only. Write the built set into $STAGE_DIR and stop; nothing below
# this block (live install, links, tools, supervisor, restart) runs.
# #262 (hub #2800): the set is written into a fresh private tree beside DIR
# (mktemp -d, mode 0700) and renamed onto DIR, so no path inside DIR that
# existed before (a `tools` or `MANIFEST.sha256` symlink into ~/.rsi/bin or
# ~/.rsi/lib) is ever written through.
STAGE_TREE=""
stage_release() {
    local stage parent name final
    stage="$(canonical_path "$1")"
    stage_refuse_overlap "$stage"
    stage_refuse_existing "$stage"
    parent="$(dirname -- "$stage")"
    name="$(basename -- "$stage")"
    mkdir -p "$parent"
    final="$(canonical_path "$parent")/$name"
    stage_refuse_overlap "$final"
    STAGE_TREE="$(mktemp -d "$parent/.$name.stage.XXXXXX")"
    trap 'rm -rf "$STAGE_TREE"' EXIT
    # #262 (hub #2799): AgentRequestDeploy reads every member of DEPLOY_BINARIES
    # as a top-level file of binaries_dir (rsi-common agent_deploy.rs, rsid
    # deploy.rs stage_all). So the binaries, rsid-supervisor.sh, the FULL
    # scripts/cargo-slot as `cargo-slot` (never the ~/.rsi/bin wrapper, hub
    # #2777), target-pool and rsi-spill (hub #2958) all sit at the top level.
    install_built_binaries "$TARGET_DIR/release" "$STAGE_TREE"
    install_supervisor_script "$STAGE_TREE"
    cp "$ROOT/scripts/cargo-slot" "$STAGE_TREE/cargo-slot"
    cp "$ROOT/scripts/target-pool" "$STAGE_TREE/target-pool"
    cp "$RSI_SPILL_SOURCE" "$STAGE_TREE/rsi-spill"
    chmod 755 "$STAGE_TREE/cargo-slot" "$STAGE_TREE/target-pool" "$STAGE_TREE/rsi-spill" "$STAGE_TREE"
    python3 - "$STAGE_TREE" "$final" <<'PY'
import hashlib
import os
import sys

tree, final = sys.argv[1:]
manifest = "MANIFEST.sha256"
lines = []
for top, dirs, files in os.walk(tree):
    for name in files:
        path = os.path.join(top, name)
        rel = os.path.relpath(path, tree)
        if rel == manifest:
            continue
        digest = hashlib.sha256()
        with open(path, "rb") as handle:
            for chunk in iter(lambda: handle.read(1 << 20), b""):
                digest.update(chunk)
        lines.append((rel, f"{digest.hexdigest()}  {rel}\n"))
lines = [line for _, line in sorted(lines)]
with open(os.path.join(tree, manifest), "w") as out:
    out.writelines(lines)
# POSIX rename: replaces a missing or empty directory, fails on anything else.
os.rename(tree, final)
print(f"Staged (nothing live touched) into {final}:")
sys.stdout.writelines(lines)
PY
    STAGE_TREE=""
}

if [[ -n "$STAGE_DIR" ]]; then
    stage_release "$STAGE_DIR"
    release_guard_unlock
    exit 0
fi

# #1592: carry the supervisor through the same manifest as the binaries.
# Direct installs below copy it too; quiet installs leave the running path
# alone until the daemon swaps the verified set.
install_supervisor_script "$TARGET_DIR/release"

# Daemon jobs execute these copies directly (cargo-slot; target-pool seeds
# each new worker sandbox's target, #1741). Publish them before either install
# path returns, keeping any running copy on its existing inode.
(
    mkdir -p "$RSI_HOME_DIR/bin"
    mkdir -p "$RSI_HOME_DIR/lib"
    script_temp="$(mktemp "$RSI_HOME_DIR/lib/.cargo-slot.XXXXXX")"
    trap 'rm -f "$script_temp"' EXIT
    cp "$ROOT/scripts/cargo-slot" "$script_temp"
    chmod 755 "$script_temp"
    mv -f "$script_temp" "$RSI_HOME_DIR/lib/cargo-slot"
    wrapper_temp="$(mktemp "$RSI_HOME_DIR/bin/.cargo-slot.XXXXXX")"
    trap 'rm -f "$script_temp" "$wrapper_temp"' EXIT
    cp "$ROOT/scripts/cargo-slot-wrapper" "$wrapper_temp"
    chmod 755 "$wrapper_temp"
    mv -f "$wrapper_temp" "$RSI_HOME_DIR/bin/cargo-slot"
)
(
    mkdir -p "$RSI_HOME_DIR/bin"
    script_temp="$(mktemp "$RSI_HOME_DIR/bin/.target-pool.XXXXXX")"
    trap 'rm -f "$script_temp"' EXIT
    cp "$ROOT/scripts/target-pool" "$script_temp"
    chmod 755 "$script_temp"
    mv -f "$script_temp" "$RSI_HOME_DIR/bin/target-pool"
)
# hub #2958: ~/.local/bin/rsi-spill links this stable copy, never the checkout.
(
    mkdir -p "$RSI_HOME_DIR/bin"
    script_temp="$(mktemp "$RSI_HOME_DIR/bin/.rsi-spill.XXXXXX")"
    trap 'rm -f "$script_temp"' EXIT
    cp "$RSI_SPILL_SOURCE" "$script_temp"
    chmod 755 "$script_temp"
    mv -f "$script_temp" "$RSI_SPILL_SCRIPT"
    mkdir -p "$BIN_DIR"
    ln -sfn "$RSI_SPILL_SCRIPT" "$BIN_DIR/rsi-spill"
)

# #2936: on macOS rsid runs as the launchd user agent dev.rsi.rsid
# (scripts/rsid-launchd.sh). Every live install refreshes its plist, so the next
# load (a restart below, a reboot or a login) starts what is installed here. A
# quiet restart keeps the running job: rsid restarts inside it.
# RSI_LAUNCHD_AGENT=0 keeps the detached holder chain instead.
LAUNCHD_AGENT=0
if [[ "$(uname -s)" == Darwin ]] && rsid_launchd_available; then
    LAUNCHD_AGENT=1
    (
        umask 077
        mkdir -p "$RSI_HOME_DIR"
        : >>"$DAEMON_LOG"
    )
    launchd_plist_state="$(rsid_launchd_write_plist "$ROOT/scripts/launchd/$RSID_LAUNCHD_LABEL.plist.in" \
        "$INSTALL_DIR" "$(daemon_socket_path)" "$DAEMON_LOG")"
    echo "launchd agent $(rsid_launchd_plist): $launchd_plist_state"
    if [[ "$NO_RESTART" -eq 0 && "$QUIET_RESTART" -eq 1 ]]; then
        if ! rsid_launchd_loaded && [[ -n "$(rsid_running_pids)" ]]; then
            echo "note: rsid runs outside launchd; a quiet restart keeps that chain. Move it under $RSID_LAUNCHD_LABEL with: make release-install NOW=1" >&2
        elif [[ "$launchd_plist_state" == changed ]] && rsid_launchd_loaded; then
            echo "note: the changed plist takes effect when the agent is next loaded (make release-install NOW=1, a reboot or a login)." >&2
        fi
    fi
fi

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
# (rsi-spill was linked to its stable ~/.rsi/bin copy with the tool scripts above.)
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

installer_daemon_environment() {
    rsid_daemon_environment
}

# Create the data directory (0700) and the daemon log (0600) under umask 077
# before anything redirects output into them (#1852): a log first created by
# the redirect would take the caller's umask (0644) inside a possibly 0755
# directory. Existing paths keep their modes; rsid tightens those at startup.
ensure_private_daemon_log() {
    (
        umask 077
        mkdir -p "$RSI_HOME_DIR"
        : >>"$DAEMON_LOG"
    )
}

# #2936: restart rsid as the launchd user agent. The loaded job is booted out
# first (KeepAlive would relaunch the chain this stop ends); a chain started
# outside launchd by an older install is then stopped the legacy way, so no
# second holder ever runs. The start waits for the job's holder and then for a
# GetHealthStatus answered from inside its process tree.
restart_rsid_launchd() {
    local old_pids="$1" old_launchers="" waited=0 limit="${RSI_INSTALL_HEALTH_WAIT_SECS:-90}"
    if [[ -z "$old_pids" ]] && ! rsid_launchd_loaded; then
        echo "rsid is not currently running -- starting it as launchd agent $(rsid_launchd_service)."
    else
        run_restart_drain_hook
        if [[ -n "$old_pids" ]]; then
            echo "Restarting rsid (was running as PID(s): $(echo "$old_pids" | tr '\n' ' ')) as launchd agent $(rsid_launchd_service) ..."
            # shellcheck disable=SC2086
            old_launchers="$(rsid_launcher_pids $old_pids)"
        fi
        rsid_launchd_stop 90 || return 1
        local legacy_pids
        legacy_pids="$(rsid_running_pids)"
        if [[ -n "$legacy_pids" ]]; then
            echo "Stopping rsid started outside launchd (PID(s): $(echo "$legacy_pids" | tr '\n' ' ')) ..."
            # shellcheck disable=SC2086
            kill -INT $legacy_pids 2>/dev/null || true
        fi
        while [[ "$waited" -lt 900 ]]; do
            # shellcheck disable=SC2086
            if [[ -z "$(rsid_running_pids)" && -z "$(rsid_pids_still_running $old_launchers)" ]]; then
                break
            fi
            sleep 0.1
            waited=$((waited + 1))
        done
        # shellcheck disable=SC2086
        if [[ -n "$(rsid_running_pids)" || -n "$(rsid_pids_still_running $old_launchers)" ]]; then
            echo "rsid or its launcher did not stop within 90s; refusing a second holder or daemon." >&2
            return 1
        fi
    fi

    ensure_private_daemon_log
    local socket_path
    socket_path="$(daemon_socket_path)"
    if ! rsid_launchd_start "$limit" \
        || ! wait_for_rsid_health "$RSI_RPC_BIN" "$socket_path" "$limit" "" "${RSID_LAUNCHD_PID:-0}"; then
        echo "No healthy daemon from this launch answered GetHealthStatus on $socket_path -- NO DAEMON IS RUNNING from this launch; check $DAEMON_LOG and \`launchctl print $(rsid_launchd_service)\` (docs/satellites.md, \"macOS satellites\")." >&2
        exit 1
    fi
    echo "rsid restarted as launchd agent $(rsid_launchd_service), logging to $DAEMON_LOG"
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
        persist_worker_slice_limits
        systemctl --user set-property rsid.service \
            "MemoryHigh=${RSID_SCOPE_MEMORY_HIGH_MIB}M" \
            "MemoryMax=${RSID_SCOPE_MEMORY_MAX_MIB}M" \
            "MemorySwapMax=${RSID_SCOPE_MEMORY_SWAP_MAX_MIB}M" \
            "CPUWeight=${RSID_SCOPE_CPU_WEIGHT}"
        echo "Restarting managed rsid.service ..."
        systemctl --user restart rsid.service
        local managed_pid
        managed_pid="$(systemctl --user show rsid.service --property=MainPID --value)"
        wait_for_rsid_health "$RSI_RPC_BIN" "$(daemon_socket_path)" "${RSI_INSTALL_HEALTH_WAIT_SECS:-90}" rsid.service "${managed_pid:-0}"
        verify_rsid_scope rsid.service
        return
    fi

    if [[ ! -x "$SOCKET_HOLD_BIN" ]]; then
        echo "Socket holder not installed at $SOCKET_HOLD_BIN; refusing to start or restart without the front door." >&2
        return 1
    fi

    if [[ "$platform" == Darwin && "${LAUNCHD_AGENT:-0}" -eq 1 ]]; then
        restart_rsid_launchd "$old_pids"
        return
    fi

    # #1862: a down daemon is started, never skipped. Exiting 0 with no daemon
    # left the hub dark for 22.6 h after a holder swap. Below, every start or
    # restart ends in a bounded health wait that exits nonzero without an answer.
    if [[ -z "$old_pids" ]]; then
        echo "rsid is not currently running -- starting it through the holder/supervisor path."
    else
        run_restart_drain_hook

        echo "Restarting rsid (was running as PID(s): $(echo "$old_pids" | tr '\n' ' ')) ..."

        # Snapshot before stopping rsid: its PPID disappears when it exits.
        local old_launchers
        # shellcheck disable=SC2086
        old_launchers="$(rsid_launcher_pids $old_pids)"

        # Legacy supervisor path. The managed service uses systemd's stop signal.
        # shellcheck disable=SC2086
        kill -INT $old_pids 2>/dev/null || true

        local waited=0
        while [[ "$waited" -lt 900 ]]; do
            # shellcheck disable=SC2086
            if [[ -z "$(rsid_running_pids)" && -z "$(rsid_pids_still_running $old_launchers)" ]]; then
                break
            fi
            sleep 0.1
            waited=$((waited + 1))
        done

        # shellcheck disable=SC2086
        if [[ -n "$(rsid_running_pids)" || -n "$(rsid_pids_still_running $old_launchers)" ]]; then
            echo "rsid or its launcher did not stop within 90s; refusing a second holder or daemon." >&2
            return 1
        fi
    fi

    if [[ "$platform" == Linux ]]; then
        provision_worker_slice
    fi

    ensure_private_daemon_log
    local scope_unit="" launcher_pid
    if [[ "$platform" == Linux ]]; then
        # A collected service survives the installer and the agent process tree.
        # The holder and supervisor retain the existing aggregate limits.
        installer_daemon_environment
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
            "${DAEMON_ENV_ARGS[@]}" \
            --setenv="RSI_SOCKET=$(daemon_socket_path)" \
            --setenv="RSI_DAEMON_SOCKET_PATH=$(daemon_socket_path)" \
            -- "$SOCKET_HOLD_BIN" "$(daemon_socket_path)" -- \
            "$INSTALL_DIR/rsid-supervisor.sh" "$RSID_BIN"
        launcher_pid="$(systemctl --user show "$scope_unit" --property=MainPID --value)"
    else
        RSI_SOCKET="$(daemon_socket_path)" RSI_DAEMON_SOCKET_PATH="$(daemon_socket_path)" \
        nohup "$SOCKET_HOLD_BIN" "$(daemon_socket_path)" -- \
            "$INSTALL_DIR/rsid-supervisor.sh" "$RSID_BIN" \
            </dev/null >>"$DAEMON_LOG" 2>&1 &
        launcher_pid=$!
        disown "$launcher_pid"
    fi

    local socket_path
    socket_path="$(daemon_socket_path)"
    if ! wait_for_rsid_health "$RSI_RPC_BIN" "$socket_path" "${RSI_INSTALL_HEALTH_WAIT_SECS:-90}" "$scope_unit" "${launcher_pid:-0}"; then
        echo "No healthy daemon from this launch answered GetHealthStatus on $socket_path -- NO DAEMON IS RUNNING from this launch; check $DAEMON_LOG, then start it directly (scripts/systemd/README.md, \"Recovery\")." >&2
        exit 1
    fi
    if [[ "$platform" == Linux ]]; then
        verify_rsid_scope "$scope_unit"
        echo "rsid supervisor restarted in $scope_unit, logging to $DAEMON_LOG"
    else
        echo "rsid restarted on macOS, logging to $DAEMON_LOG"
    fi
}

# #2188: the links now point at INSTALL_DIR; drop kept copies none uses.
release_guard_prune_kept

# Every copy out of the target is done: release the build lock (#938) so the
# daemon launched below never inherits it.
release_guard_unlock

if [[ "$NO_RESTART" -eq 0 ]]; then
    restart_rsid
else
    echo "Skipping rsid restart (--no-restart)."
fi
