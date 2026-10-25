#!/usr/bin/env bash
# Sourced by rsid-health.sh, rsid-quiet-restart.sh and release-target-guard.sh (#465):
# find the rsi-repo-guard binary that runs the Rust helpers which replaced the
# inline interpreter heredocs of the product shell scripts (health probe, quiet
# restart request, release-target locking and link keeping, receipt checks).
#
# Bash 3.2 compatible (macOS /bin/bash).

RSI_GUARD_SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# Set by rsid_resolve_repo_guard.
RSI_GUARD_BIN=""

# rsid_guard_has <binary> <subcommand>: does that build know the subcommand? An
# installed copy older than the helper is skipped, never run into "unknown guard".
rsid_guard_has() {
    "$1" --list 2>/dev/null | awk -F'\t' -v name="$2" '$1 == name { found = 1 } END { exit !found }'
}

# rsid_resolve_repo_guard [<rsi-rpc> [<subcommand>]]: set RSI_GUARD_BIN. Order:
# RSI_REPO_GUARD_BIN, beside <rsi-rpc> (the install dir), the installer's copy in
# $RSI_HOME_DIR/bin (default ~/.rsi/bin), then a build through scripts/repo-guard.
# With <subcommand>, a binary that lacks it is passed over. Returns 1 with a message
# when none exists: a helper that cannot run is never read as a pass.
rsid_resolve_repo_guard() {
    local rpc="${1:-}" need="${2:-}" candidate
    if [[ -n "$RSI_GUARD_BIN" && -x "$RSI_GUARD_BIN" ]] \
        && { [[ -z "$need" ]] || rsid_guard_has "$RSI_GUARD_BIN" "$need"; }; then
        return 0
    fi
    RSI_GUARD_BIN=""
    for candidate in "${RSI_REPO_GUARD_BIN:-}" "${rpc:+$(dirname -- "$rpc")/rsi-repo-guard}" \
        "${RSI_HOME_DIR:-$HOME/.rsi}/bin/rsi-repo-guard"; do
        if [[ -n "$candidate" && -f "$candidate" && -x "$candidate" ]] \
            && { [[ -z "$need" ]] || rsid_guard_has "$candidate" "$need"; }; then
            RSI_GUARD_BIN="$candidate"
            return 0
        fi
    done
    if [[ -x "$RSI_GUARD_SCRIPT_DIR/repo-guard" ]] \
        && candidate="$("$RSI_GUARD_SCRIPT_DIR/repo-guard" --bin 2>/dev/null)" \
        && [[ -x "$candidate" ]] \
        && { [[ -z "$need" ]] || rsid_guard_has "$candidate" "$need"; }; then
        RSI_GUARD_BIN="$candidate"
        return 0
    fi
    echo "rsi-repo-guard${need:+ with $need} not found (RSI_REPO_GUARD_BIN, beside ${rpc:-the install dir}, or in ~/.rsi/bin); cannot run its helper" >&2
    return 1
}
