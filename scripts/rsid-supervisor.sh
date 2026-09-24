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
        exit "$status"
    fi
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
