#!/usr/bin/env bash

# Print current-user rsid PIDs that are still live. pgrep also reports zombies;
# the process has exited, so it cannot be stopped or indicate an active daemon.
rsid_running_pids() {
    local pid state
    while IFS= read -r pid; do
        [[ -n "$pid" ]] || continue
        state="$(ps -o stat= -p "$pid" 2>/dev/null || true)"
        [[ "$state" == Z* ]] && continue
        [[ -n "$state" ]] && printf '%s\n' "$pid"
    done < <(pgrep -x -u "$(id -u)" rsid || true)
    return 0
}

# Snapshot only the known launcher ancestors of these daemon PIDs. They can
# outlive rsid while the supervisor drains/reaps and the holder removes its
# socket and releases its lease. Never stop or signal an ancestor here.
rsid_launcher_pids() {
    local pid parent command
    for pid in "$@"; do
        while :; do
            parent="$(ps -o ppid= -p "$pid" 2>/dev/null || true)"
            parent="${parent//[[:space:]]/}"
            [[ "$parent" =~ ^[0-9]+$ && "$parent" -gt 1 ]] || break
            command="$(ps -o args= -p "$parent" 2>/dev/null || true)"
            case "$command" in
                */rsid-supervisor.sh\ *|*/rsid-supervisor.sh.last-good\ *|*/rsi-socket-hold\ *|rsi-socket-hold\ *)
                    printf '%s\n' "$parent"
                    pid="$parent" ;;
                *) break ;;
            esac
        done
    done
}

rsid_pids_still_running() {
    local pid state
    for pid in "$@"; do
        state="$(ps -o stat= -p "$pid" 2>/dev/null || true)"
        [[ -n "$state" && "$state" != Z* ]] && printf '%s\n' "$pid"
    done
    return 0
}
