#!/usr/bin/env bash
# Sourced by install-release.sh; keep probes independent of the caller's env.

rsid_rpc_ready() {
    python3 - "$1" "$2" <<'PY'
import json
import subprocess
import sys

try:
    probe = subprocess.run(
        [sys.argv[1], "--socket", sys.argv[2], "GetHealthStatus"],
        capture_output=True,
        text=True,
        timeout=2,
    )
    reply = json.loads(probe.stdout) if probe.returncode == 0 else None
    result = reply.get("result") if isinstance(reply, dict) else None
    ready = (
        isinstance(reply, dict)
        and reply.get("jsonrpc") == "2.0"
        and reply.get("id") == 1
        and reply.get("error") is None
        and isinstance(result, dict)
        and type(result.get("persistence_queue_capacity")) is int
        and type(result.get("project_cache_size")) is int
    )
except (OSError, subprocess.TimeoutExpired, ValueError):
    ready = False
sys.exit(0 if ready else 1)
PY
}

wait_for_rsid_health() {
    local rpc="$1" socket="$2" limit="$3" unit="${4:-}"
    local started=$SECONDS next_progress=$SECONDS state="socket pending"
    while ((SECONDS - started < limit)); do
        if rsid_rpc_ready "$rpc" "$socket"; then
            echo "rsid healthy after $((SECONDS - started))s on $socket"
            return 0
        fi
        if [[ -n "$unit" ]]; then
            if systemctl --user is-failed --quiet "$unit"; then
                echo "$unit failed before rsid became healthy" >&2
                return 1
            fi
            state="$(systemctl --user show "$unit" --property=ActiveState --value 2>/dev/null || echo unknown)"
        fi
        if ((SECONDS >= next_progress)); then
            echo "Waiting for rsid health: ${state}, $((SECONDS - started))/${limit}s"
            next_progress=$((SECONDS + 5))
        fi
        sleep 1
    done
    echo "rsid did not answer GetHealthStatus on $socket within ${limit}s" >&2
    return 1
}
