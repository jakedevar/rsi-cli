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
