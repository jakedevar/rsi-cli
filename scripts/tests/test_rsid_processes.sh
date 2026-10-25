#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
source "$SCRIPT_DIR/rsid-processes.sh"

export HOME=/fixture/home
unset RSI_DAEMON_SOCKET_PATH RSI_SOCKET MOTHERSHIP_SOCKET FLYWHEEL_SOCKET RSI_HOME_DIR

pgrep() {
    printf '%s\n' 29712 474038 991
}

# Environment of each fake PID (the real helper reads /proc/PID/environ or `ps eww`).
rsid_process_environment() {
    case "$1" in
        29712 | 474038) printf 'HOME=/fixture/home\n' ;;
        991) ;;
        *) printf 'RSI_SOCKET=/fixture/other/daemon.sock\nHOME=/fixture/other\n' ;;
    esac
}

ps() {
    case "${!#}" in
        29712) printf 'Z\n' ;;
        474038) printf 'S\n' ;;
        991) return 1 ;; # exited between pgrep and ps
        *) return 2 ;;
    esac
}

actual="$(rsid_running_pids)"
if [[ "$actual" != 474038 ]]; then
    printf 'expected only the live rsid PID, got: %s\n' "$actual" >&2
    exit 1
fi

pgrep() { printf '%s\n' 29712; }
if [[ -n "$(rsid_running_pids)" ]]; then
    echo "expected a zombie-only rsid match to be treated as stopped" >&2
    exit 1
fi

# #3433: an intended direct stop is announced to the supervisor by pid.
announce_dir="$(mktemp -d)"
live_pids="" live_dir=""
cleanup() {
    local pid
    for pid in $live_pids; do
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    done
    rm -rf "$announce_dir"
    [[ -z "$live_dir" ]] || rm -rf "$live_dir"
}
trap cleanup EXIT
RSID_SUPERVISOR_INTENDED_STOP_FILE="$announce_dir/nested/intended-stop" \
    rsid_announce_intended_stop install_release 111 222
if [[ "$(cat "$announce_dir/nested/intended-stop")" != $'pid=111 reason=install_release\npid=222 reason=install_release' ]]; then
    echo "expected one announcement line per pid" >&2
    exit 1
fi
# #3507: the announcement is owner-private, and never goes through a symlink or a
# shared /tmp path when there is no HOME.
if [[ "$(stat -c %a "$announce_dir/nested/intended-stop" 2>/dev/null || stat -f %Lp "$announce_dir/nested/intended-stop")" != 600 ]]; then
    echo "expected the announcement file to be mode 0600" >&2
    exit 1
fi
ln -s "$announce_dir/elsewhere" "$announce_dir/link"
RSID_SUPERVISOR_INTENDED_STOP_FILE="$announce_dir/link" \
    rsid_announce_intended_stop install_release 444 2>/dev/null
if [[ -e "$announce_dir/elsewhere" ]]; then
    echo "expected no write through a symlinked announcement file" >&2
    exit 1
fi
mkdir -p "$announce_dir/runtime"
(
    unset HOME RSI_HOME_DIR RSID_SUPERVISOR_INTENDED_STOP_FILE
    XDG_RUNTIME_DIR="$announce_dir/runtime" rsid_announce_intended_stop install_release 555
    if ! grep -qx 'pid=555 reason=install_release' "$announce_dir/runtime/rsi/intended-stop"; then
        echo "expected the announcement under XDG_RUNTIME_DIR without HOME" >&2
        exit 1
    fi
    unset XDG_RUNTIME_DIR
    rsid_announce_intended_stop install_release 666 2>/dev/null
    if [[ -e /tmp/rsi-intended-stop ]] && grep -qx 'pid=666 reason=install_release' /tmp/rsi-intended-stop; then
        echo "expected no announcement through a shared /tmp path" >&2
        exit 1
    fi
) || exit 1
RSI_HOME_DIR="$announce_dir/home" rsid_announce_intended_stop install_release 333
if ! grep -qx 'pid=333 reason=install_release' "$announce_dir/home/intended-stop"; then
    echo "expected the announcement under RSI_HOME_DIR by default" >&2
    exit 1
fi

echo "rsid process selection ignores zombies and exited PIDs"

# #3476: an rsid serving another socket or home is not this install's daemon.
pgrep() { printf '%s\n' 474038 500 501 502 503; }
ps() { printf 'S\n'; }
rsid_process_environment() {
    case "$1" in
        474038) printf 'HOME=/fixture/home\n' ;;                                        # default socket of this home
        500) printf 'HOME=/fixture/other\n' ;;                                          # another home
        501) printf 'RSI_SOCKET=/fixture/other/daemon.sock\nHOME=/fixture/home\n' ;;    # another socket, same home
        502) ;;                                                                          # unreadable environment
        503) printf 'RSI_DAEMON_SOCKET_PATH=/fixture/home/.rsi/daemon.sock\nHOME=/x\n' ;; # explicit, same socket
    esac
}
actual="$(rsid_running_pids | tr '\n' ' ')"
if [[ "$actual" != "474038 503 " ]]; then
    printf 'expected only the daemons serving this install socket, got: %s\n' "$actual" >&2
    exit 1
fi

daemon_socket_path() { printf '%s\n' /fixture/other/daemon.sock; }
actual="$(rsid_running_pids | tr '\n' ' ')"
if [[ "$actual" != "501 " ]]; then
    printf 'expected the caller socket to scope the match, got: %s\n' "$actual" >&2
    exit 1
fi

echo "rsid process selection ignores daemons of another RSI home or socket"

# macOS `ps eww` fallback: values containing spaces stay whole.
unset -f daemon_socket_path
source "$SCRIPT_DIR/rsid-processes.sh" # restore the real rsid_process_environment
line='/usr/local/bin/rsid --foo PATH=/usr/bin HOME=/Users/Some User/My Home RSI_SOCKET=/Volumes/My Disk/rsi/daemon.sock TERM=xterm'
actual="$(printf '%s\n' "$line" | rsid_parse_ps_environment 'RSI_SOCKET|HOME')"
expected=$'RSI_SOCKET=/Volumes/My Disk/rsi/daemon.sock\nHOME=/Users/Some User/My Home'
if [[ "$actual" != "$expected" ]]; then
    printf 'expected spaced values to survive, got: %s\n' "$actual" >&2
    exit 1
fi

# A daemon whose HOME has spaces is in scope of that home's default socket. The
# fake PID has no /proc entry, so the helper takes the `ps eww` fallback.
line='rsid HOME=/Users/Some User/My Home TERM=xterm'
export HOME="/Users/Some User/My Home"
fake_pid=99999999
pgrep() { printf '%s\n' "$fake_pid"; }
ps() { case "$*" in *eww*) printf '%s\n' "$line" ;; *) printf 'S\n' ;; esac; }
actual="$(rsid_running_pids | tr '\n' ' ')"
if [[ "$actual" != "$fake_pid " ]]; then
    printf 'expected a spaced HOME to resolve to this install via the ps fallback, got: %s\n' "$actual" >&2
    exit 1
fi

echo "rsid ps eww fallback keeps values with spaces"

# #399: on macOS, scope by the socket the process holds, not only its environment.
# macOS 27 `ps eww` prints no environment for an Apple-signed binary, so a live
# daemon was never seen and rsid-start-satellite could start a second one. Two
# real processes named rsid each bind a real Unix socket; neither environment
# names this install's socket, so only the socket can tell them apart.
if [[ "$(uname -s)" != Darwin ]]; then
    echo "rsid socket-held scope: skipped (macOS only; Linux reads /proc/PID/environ)"
    exit 0
fi
for tool in nc lsof codesign pgrep; do
    if ! command -v "$tool" >/dev/null 2>&1 && [[ ! -x "/usr/sbin/$tool" ]]; then
        printf 'rsid socket-held scope needs %s on macOS\n' "$tool" >&2
        exit 1
    fi
done
unset -f ps pgrep rsid_process_environment daemon_socket_path
source "$SCRIPT_DIR/rsid-processes.sh"
live_dir="$(mktemp -d /tmp/rsid-proc.XXXXXX)" # short: AF_UNIX paths are bounded
# A copy of nc named rsid (comm `rsid`), re-signed ad hoc so the copy may run.
cp "$(command -v nc)" "$live_dir/rsid"
codesign -f -s - "$live_dir/rsid" >/dev/null 2>&1
start_listener() {
    env -i HOME=/nonexistent "$live_dir/rsid" -lU "$1" </dev/null >/dev/null 2>&1 &
    live_pids="$live_pids $!"
    last_pid=$!
}
start_listener "$live_dir/ours.sock"
ours=$last_pid
start_listener "$live_dir/other.sock"
other=$last_pid
tries=0
until [[ -S "$live_dir/ours.sock" && -S "$live_dir/other.sock" ]]; do
    tries=$((tries + 1))
    if [[ "$tries" -gt 100 ]]; then
        echo "rsid socket-held scope: listeners never bound" >&2
        exit 1
    fi
    sleep 0.05
done
# The real pgrep, narrowed to the two listeners so the host's own rsid stays out.
pgrep() { command pgrep "$@" | grep -x -e "$ours" -e "$other" || true; }
export HOME=/nonexistent
export RSI_DAEMON_SOCKET_PATH="$live_dir/ours.sock"
if [[ "$(pgrep -x -u "$(id -u)" rsid | sort | tr '\n' ' ')" != "$(printf '%s\n' "$ours" "$other" | sort | tr '\n' ' ')" ]]; then
    echo "rsid socket-held scope: pgrep does not see both listeners as rsid" >&2
    exit 1
fi
actual="$(rsid_running_pids | tr '\n' ' ')"
if [[ "$actual" != "$ours " ]]; then
    printf 'expected only the rsid holding this install socket (%s), got: %s\n' "$ours" "$actual" >&2
    exit 1
fi
export RSI_DAEMON_SOCKET_PATH="$live_dir/nobody.sock"
if [[ -n "$(rsid_running_pids)" ]]; then
    echo "expected no rsid in scope when none holds this install socket" >&2
    exit 1
fi

echo "rsid process selection finds the macOS daemon by the socket it holds"
