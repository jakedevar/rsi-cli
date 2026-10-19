#!/usr/bin/env bash
# Sourced by install-release.sh and the recovery runbook.

# A transient service does not inherit the installer's environment like a scope.
# Pass exported configuration by name: systemd-run reads the value from its own
# environment, so neither command arguments nor installer output contain it.
# Provider secrets continue to come from ~/.rsi/.env; agent custody and build
# metadata must not become the daemon's identity.
#
# #2823: the daemon's PATH decides which rustc every job unit builds with. A
# restart from a bare `systemd-run --user` (the hub's, after a reboot) carries
# /usr/local/bin:/usr/bin, so every job built with the system rustc instead of
# the rust-toolchain.toml pin and the warning gate failed on untouched code. The
# daemon PATH is the caller's PATH with ~/.cargo/bin (rustup) and ~/.local/bin
# in front when absent.
rsid_daemon_path() {
    local dir path=":${PATH:-}:" prefix=""
    for dir in "$HOME/.local/bin" "$HOME/.cargo/bin"; do
        case "$path" in
            *":$dir:"*) ;;
            *) prefix="$dir:$prefix" ;;
        esac
    done
    printf '%s%s\n' "$prefix" "${PATH:-/usr/local/bin:/usr/bin:/bin}"
}

rsid_daemon_environment() {
    local key
    DAEMON_ENV_ARGS=()
    while IFS= read -r key; do
        case "$key" in
            RSI_SESSION_*|RSI_PROCESS_OWNERSHIP_NAMESPACE|RSI_LISTEN_FD|RSI_CARGO_*|RSI_SCOPED_TEST_*|RSI_TEST_*|*KEY*|*TOKEN*|*SECRET*|*PASSWORD*|*CREDENTIAL*) continue ;;
            PATH) continue ;;
            HOME|USER|LOGNAME|SHELL|LANG|LC_*|TZ|TMPDIR|XDG_*|AWS_REGION|AWS_DEFAULT_REGION|AWS_PROFILE|RSI_*|MOTHERSHIP_*|FLYWHEEL_*)
                DAEMON_ENV_ARGS+=("--setenv=$key") ;;
        esac
    done < <(compgen -e | sort)
    DAEMON_ENV_ARGS+=("--setenv=PATH=$(rsid_daemon_path)")
}

rsid_rpc_ready() {
    python3 - "$1" "$2" "${3:-}" <<'PY'
import json
from datetime import datetime
import socket
import struct
import subprocess
import sys
import time

# Probe identity on the health connection. The reported PID names the daemon
# using an inherited listener; Linux credentials additionally bind its reply
# to the actual sender. Older daemons keep the kernel identity fallback.
deadline = time.monotonic() + 2


def remaining():
    return max(0.001, deadline - time.monotonic())


def process(pid):
    probe = subprocess.run(
        ["ps", "-o", "ppid=,stat=", "-p", str(pid)],
        capture_output=True, text=True, timeout=remaining(),
    )
    fields = probe.stdout.split()
    if probe.returncode or len(fields) != 2 or fields[1].startswith("Z"):
        raise ProcessLookupError("launcher or daemon exited")
    return int(fields[0])


class ResponderExited(ProcessLookupError):
    """The process that sent the health reply is gone (exit code 3)."""


def responder_parent(pid):
    try:
        return process(pid)
    except ProcessLookupError as exited:
        raise ResponderExited("process that answered exited") from exited


def in_launcher_tree(pid, launcher):
    seen = set()
    lookup = responder_parent  # only the first process is the one that answered
    while pid > 1 and pid not in seen and time.monotonic() < deadline:
        seen.add(pid)
        parent = lookup(pid)
        lookup = process
        if pid == launcher:
            return True
        pid = parent
    return False


try:
    launcher = int(sys.argv[3]) if sys.argv[3] else None
    if launcher is not None:
        if launcher <= 1:
            raise ProcessLookupError("launcher PID missing")
        process(launcher)
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as conn:
            conn.settimeout(remaining())
            if sys.platform.startswith("linux"):
                conn.setsockopt(socket.SOL_SOCKET, socket.SO_PASSCRED, 1)
            elif sys.platform != "darwin":
                raise OSError("daemon identity probe unsupported on this platform")
            conn.connect(sys.argv[2])
            conn.sendall(b'{"jsonrpc":"2.0","id":1,"method":"GetHealthStatus","params":null}\n')
            data = b""
            senders = set()
            while b"\n" not in data and len(data) < 1024 * 1024:
                conn.settimeout(remaining())
                if sys.platform.startswith("linux"):
                    chunk, ancillary, flags, _ = conn.recvmsg(65536, socket.CMSG_SPACE(12))
                    if flags & socket.MSG_CTRUNC:
                        raise ValueError("truncated credentials")
                    credentials = [struct.unpack("3i", value)[0]
                                   for level, kind, value in ancillary
                                   if level == socket.SOL_SOCKET and kind == socket.SCM_CREDENTIALS]
                    if not credentials:
                        raise ValueError("health reply has no sender credentials")
                    senders.update(credentials)
                else:
                    chunk = conn.recv(65536)
                if not chunk:
                    raise ValueError("incomplete health reply")
                data += chunk
            if b"\n" not in data:
                raise ValueError("oversized health reply")
            reply = json.loads(data.split(b"\n", 1)[0])
            result = reply.get("result") if isinstance(reply, dict) else None
            pid = result.get("daemon_pid") if isinstance(result, dict) else None
            started = result.get("daemon_started_at") if isinstance(result, dict) else None
            if pid is not None or started is not None:
                if type(pid) is not int or pid <= 1 or not isinstance(started, str):
                    raise ValueError("invalid daemon identity")
                if datetime.fromisoformat(started.replace("Z", "+00:00")).tzinfo is None:
                    raise ValueError("daemon startup time has no timezone")
                if sys.platform.startswith("linux") and senders != {pid}:
                    raise ValueError("reported daemon PID differs from health sender")
                senders = {pid}
            elif sys.platform == "darwin":
                # Legacy Darwin SOL_LOCAL=0 / LOCAL_PEERPID=2 fallback.
                senders.add(conn.getsockopt(0, 2))
            if len(senders) != 1 or not in_launcher_tree(senders.pop(), launcher):
                raise ValueError("foreign daemon answered GetHealthStatus")
        process(launcher)
    else:
        probe = subprocess.run(
            [sys.argv[1], "--socket", sys.argv[2], "GetHealthStatus"],
            capture_output=True, text=True, timeout=remaining(),
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
except ResponderExited:
    sys.exit(3)
except ProcessLookupError:
    sys.exit(2)
except (OSError, subprocess.TimeoutExpired, ValueError):
    ready = False
sys.exit(0 if ready else 1)
PY
}

wait_for_rsid_health() {
    local rpc="$1" socket="$2" limit="$3" unit="${4:-}" launcher="${5:-}"
    local started=$SECONDS next_progress=$SECONDS state="socket pending"
    while ((SECONDS - started < limit)); do
        if [[ -n "$unit" ]]; then
            state="$(systemctl --user show "$unit" --property=ActiveState --value 2>/dev/null || echo unknown)"
            if [[ "$state" != active && "$state" != activating ]]; then
                echo "$unit exited before rsid became healthy ($state)" >&2
                return 1
            fi
            if [[ "$(systemctl --user show "$unit" --property=MainPID --value 2>/dev/null)" != "$launcher" ]]; then
                echo "$unit launcher changed before rsid became healthy" >&2
                return 1
            fi
        fi
        local probe_status=0
        rsid_rpc_ready "$rpc" "$socket" "$launcher" || probe_status=$?
        if ((probe_status == 0)); then
            # The probe also checks the launcher after reading the response.
            echo "rsid healthy after $((SECONDS - started))s on $socket"
            return 0
        fi
        if ((probe_status == 2)); then
            echo "rsid launcher exited before an identified GetHealthStatus reply" >&2
            return 1
        fi
        if ((probe_status == 3)); then
            echo "the rsid process that answered GetHealthStatus exited right after replying; the daemon is not staying up" >&2
            return 1
        fi
        if ((SECONDS >= next_progress)); then
            echo "Waiting for rsid health: ${state}, $((SECONDS - started))/${limit}s"
            next_progress=$((SECONDS + 5))
        fi
        sleep 1
    done
    echo "rsid did not answer an identified GetHealthStatus on $socket within ${limit}s" >&2
    return 1
}
