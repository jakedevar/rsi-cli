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
# #1045: a deploy swaps the new binary in, leaves the old one as
# `<real path>.prev`, writes `<real path>.deploy-inflight` and exits 75. Only
# with that marker present, if the binary started after the restart dies within
# this many seconds, put `.prev` back and start it once. The daemon removes the
# marker (and `.prev`) once it verifies a deploy; a bare `.prev` never triggers
# a fallback.
fast_failure_seconds=${RSID_SUPERVISOR_FAST_FAILURE_SECONDS:-60}
after_restart=0
last_fallback=

forward_stop() {
    stopping=1
    if [[ -n $child_pid ]]; then
        kill -TERM "$child_pid" 2>/dev/null || true
    fi
}
trap forward_stop INT TERM

while :; do
    if [[ $stopping -eq 1 ]]; then
        exit 0
    fi
    started_at=$SECONDS
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
    if [[ $status -ne 75 ]]; then
        # A binary that cannot start after a restart: restore the previous one
        # (atomic renames; the failed one is kept) and go through the normal
        # restart budget, once per window.
        real_bin=$(readlink -f "$rsid_bin" 2>/dev/null || echo "$rsid_bin")
        if [[ $after_restart -eq 1 ]] \
            && ((SECONDS - started_at < fast_failure_seconds)) \
            && [[ -e "$real_bin.deploy-inflight" ]] \
            && [[ -e "$real_bin.prev" ]] \
            && { [[ -z $last_fallback ]] || ((SECONDS - last_fallback >= restart_window_seconds)); }; then
            last_fallback=$SECONDS
            echo "rsid exited $status within ${fast_failure_seconds}s of a restart; restoring $real_bin.prev (failed binary kept as $real_bin.failed)" >&2
            rm -f "$real_bin.deploy-inflight"
            mv -f "$real_bin" "$real_bin.failed" \
                && mv -f "$real_bin.prev" "$real_bin" \
                || { echo "rsid fallback rename failed" >&2; exit "$status"; }
        else
            exit "$status"
        fi
    fi
    after_restart=1
    now=$SECONDS
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
done
