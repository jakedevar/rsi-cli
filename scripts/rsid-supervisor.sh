#!/usr/bin/env bash
# Restart only when rsid's independent watchdog asks for a new incarnation.
set -u

if [[ $# -ne 1 || ! -x $1 ]]; then
    echo "usage: rsid-supervisor.sh /path/to/rsid" >&2
    exit 2
fi

rsid_bin=$1
child_pid=
stopping=0
restart_window_seconds=3600
max_restarts_per_window=3
restart_times=()
read -r -a restart_times <<< "${RSID_SUPERVISOR_RESTART_TIMES:-}"
# #1045: a deploy swaps the new binary in, leaves the old one as
# `<real path>.prev`, writes `<real path>.deploy-inflight` and exits 75. Only
# with that marker present, if the binary started after the restart dies within
# this many seconds, put `.prev` back and start it once. The daemon removes the
# marker (and `.prev`) once it verifies a deploy; a bare `.prev` never triggers
# a fallback.
fast_failure_seconds=${RSID_SUPERVISOR_FAST_FAILURE_SECONDS:-60}
after_restart=${RSID_SUPERVISOR_AFTER_RESTART:-0}
last_fallback=${RSID_SUPERVISOR_LAST_FALLBACK:-}

# Pin the running contents before a deploy replaces the path. A refresh uses a
# private snapshot, so validation and exec see the same file even if another
# install replaces the destination in between. Keep $0's basename for rsid's
# supervisor detection, and remember the installed path across snapshot execs.
supervisor_path=${RSID_SUPERVISOR_PATH:-$0}
active_script=$(cat "$0")
if [[ ${RSID_SUPERVISOR_SNAPSHOT:-} == "$0" ]]; then
    rm -f "$0"
    rmdir "$(dirname "$0")" 2>/dev/null || true
fi
unset RSID_SUPERVISOR_RESTART_TIMES RSID_SUPERVISOR_AFTER_RESTART \
    RSID_SUPERVISOR_LAST_FALLBACK RSID_SUPERVISOR_SNAPSHOT

refresh_supervisor() {
    local snapshot_dir snapshot candidate
    [[ -f $supervisor_path ]] || return 0
    candidate=$(cat "$supervisor_path") || return 0
    [[ $candidate != "$active_script" ]] || return 0
    snapshot_dir=$(mktemp -d "$(dirname "$supervisor_path")/.rsid-supervisor.XXXXXX") || return 0
    snapshot=$snapshot_dir/rsid-supervisor.sh
    if ! cp "$supervisor_path" "$snapshot" || ! bash -n "$snapshot"; then
        echo "supervisor refresh refused; keeping the running script" >&2
        rm -f "$snapshot"
        rmdir "$snapshot_dir"
        return 0
    fi
    # Retain the running script independently of the deploy's .prev lifecycle.
    if ! printf '%s\n' "$active_script" > "$snapshot_dir/previous" \
        || ! mv -f "$snapshot_dir/previous" "$supervisor_path.last-good"; then
        rm -f "$snapshot" "$snapshot_dir/previous"
        rmdir "$snapshot_dir"
        return 0
    fi
    if [[ $stopping -ne 0 ]]; then
        rm -f "$snapshot"
        rmdir "$snapshot_dir"
        return 0
    fi
    export RSID_SUPERVISOR_PATH=$supervisor_path
    export RSID_SUPERVISOR_SNAPSHOT=$snapshot
    export RSID_SUPERVISOR_RESTART_TIMES="${restart_times[*]}"
    export RSID_SUPERVISOR_AFTER_RESTART=$after_restart
    export RSID_SUPERVISOR_LAST_FALLBACK=$last_fallback
    echo "refreshing rsid supervisor between daemon lifetimes" >&2
    # A failed exec must return to this loop instead of terminating the hub.
    shopt -s execfail
    exec bash "$snapshot" "$rsid_bin"
    unset RSID_SUPERVISOR_SNAPSHOT RSID_SUPERVISOR_RESTART_TIMES \
        RSID_SUPERVISOR_AFTER_RESTART RSID_SUPERVISOR_LAST_FALLBACK
    rm -f "$snapshot"
    rmdir "$snapshot_dir"
    echo "supervisor exec failed; keeping the running script" >&2
}

forward_stop() {
    stopping=1
    if [[ -n $child_pid ]]; then
        kill -TERM "$child_pid" 2>/dev/null || true
    fi
}
trap forward_stop INT TERM

# The verifier restores binaries while its Store is open. Handle the request
# after reaping rsid, and before the first launch if the supervisor restarted.
recover_pending_database() {
    real_bin=$(readlink -f "$rsid_bin" 2>/dev/null || echo "$rsid_bin")
    if [[ -e "$real_bin.db-rollback" ]]; then
        database=$(cat "$real_bin.db-rollback")
        "$real_bin.failed" --restore-pre-migration-db "$database" "$real_bin" \
            || { echo "offline database rollback failed; refusing old daemon startup" >&2; exit 1; }
        rm -f "$real_bin.db-rollback"
    fi
}

while :; do
    if [[ $stopping -eq 1 ]]; then
        exit 0
    fi
    recover_pending_database
    started_at=$(date +%s)
    "$rsid_bin" &
    child_pid=$!
    wait "$child_pid"
    status=$?
    if [[ $stopping -eq 1 ]]; then
        # wait can return early when a signal fires; finish reaping the child.
        wait "$child_pid" 2>/dev/null || true
        exit 0
    fi
    child_pid=
    recover_pending_database
    if [[ $status -ne 75 ]]; then
        # A binary that cannot start after a restart: restore the previous one
        # (atomic renames; the failed one is kept) and go through the normal
        # restart budget, once per window.
        real_bin=$(readlink -f "$rsid_bin" 2>/dev/null || echo "$rsid_bin")
        if [[ $after_restart -eq 1 ]] \
            && (($(date +%s) - started_at < fast_failure_seconds)) \
            && [[ -e "$real_bin.deploy-inflight" ]] \
            && [[ -e "$real_bin.prev" ]] \
            && { [[ -z $last_fallback ]] || (($(date +%s) - last_fallback >= restart_window_seconds)); }; then
            last_fallback=$(date +%s)
            echo "rsid exited $status within ${fast_failure_seconds}s of a restart; restoring $real_bin.prev (failed binary kept as $real_bin.failed)" >&2
            # New deploy markers carry the exact Store path. Legacy markers
            # predate automatic backups and retain their existing behavior.
            marker=$(cat "$real_bin.deploy-inflight")
            if [[ $marker == database=* ]]; then
                "$real_bin" --restore-pre-migration-db "${marker#database=}" "$real_bin.prev" \
                    || { echo "database rollback failed; refusing old binary fallback" >&2; exit 1; }
            fi
            rm -f "$real_bin.deploy-inflight"
            mv -f "$real_bin" "$real_bin.failed" \
                && mv -f "$real_bin.prev" "$real_bin" \
                || { echo "rsid fallback rename failed" >&2; exit "$status"; }
        else
            exit "$status"
        fi
    fi
    after_restart=1
    now=$(date +%s)
    recent=()
    for restarted_at in "${restart_times[@]}"; do
        if ((now - restarted_at < restart_window_seconds)); then
            recent+=("$restarted_at")
        fi
    done
    restart_times=("${recent[@]}")
    if ((${#restart_times[@]} >= max_restarts_per_window)); then
        echo "rsid watchdog restart budget exhausted ($max_restarts_per_window in $restart_window_seconds seconds)" >&2
        exit 75
    fi
    restart_times+=("$now")
    backoff_seconds=$((1 << (${#restart_times[@]} - 1)))
    echo "rsid watchdog restart ${#restart_times[@]}/$max_restarts_per_window; waiting ${backoff_seconds}s" >&2
    sleep "$backoff_seconds"
    if [[ $stopping -eq 0 ]]; then
        refresh_supervisor
    fi
done
