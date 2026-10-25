#!/usr/bin/env bash

# Scoping (#3476): `pgrep -x rsid` sees every rsid of the user on the host, so an
# installer that signalled or waited on all of them stopped daemons that belong
# to another RSI home (a test's temp home, a dev daemon, the hub itself). A
# process is "this install's daemon" only when the socket it serves is this
# install's socket. The socket is read from the process's own environment
# (RSI_DAEMON_SOCKET_PATH, RSI_SOCKET, the legacy names, else $HOME/.rsi/daemon.sock
# of that process's HOME). Linux reads /proc/PID/environ; macOS reads `ps eww`,
# split only at ` NAME=` boundaries so values with spaces survive. macOS 27 hides
# the environment of some binaries from `ps eww`, so there a process that holds a
# Unix socket bound at this install's socket path (lsof) is in scope too (#399).
# A process neither source places on this install's socket is out of scope: it
# is never signalled and never waited on.

# The socket this install serves: the caller's `daemon_socket_path` when defined
# (install-release.sh), else the same resolution order.
rsid_expected_socket() {
    if declare -F daemon_socket_path >/dev/null 2>&1; then
        daemon_socket_path
        return 0
    fi
    printf '%s\n' "${RSI_DAEMON_SOCKET_PATH:-${RSI_SOCKET:-${MOTHERSHIP_SOCKET:-${FLYWHEEL_SOCKET:-${RSI_HOME_DIR:-$HOME/.rsi}/daemon.sock}}}}"
}

# NAME=value lines for the variables that fix a daemon's socket. Prints nothing
# when the environment is unreadable.
rsid_process_environment() {
    local pid="$1" names='RSI_DAEMON_SOCKET_PATH|RSI_SOCKET|MOTHERSHIP_SOCKET|FLYWHEEL_SOCKET|HOME'
    if [[ -r "/proc/$pid/environ" ]]; then
        tr '\0' '\n' <"/proc/$pid/environ" 2>/dev/null | grep -E "^($names)=" || true
    else
        ps eww -o command= -p "$pid" 2>/dev/null | rsid_parse_ps_environment "$names" || true
    fi
}

# Reads one `ps eww` command line on stdin and prints NAME=value lines for the
# wanted names (a `|`-separated list). `ps eww` joins the environment with single
# spaces, so a value is cut only at the next ` IDENT=` token: a socket or HOME
# path containing spaces stays whole.
rsid_parse_ps_environment() {
    awk -v names="$1" '
        BEGIN { n = split(names, want, "|") }
        {
            for (i = 1; i <= n; i++) {
                if (match($0, "(^| )" want[i] "=")) {
                    rest = substr($0, RSTART + RLENGTH)
                    if (match(rest, / [A-Za-z_][A-Za-z0-9_]*=/)) rest = substr(rest, 1, RSTART - 1)
                    print want[i] "=" rest
                }
            }
        }'
}

# The socket a live process serves, empty when it cannot be determined.
rsid_process_socket() {
    local env name value socket="" home=""
    env="$(rsid_process_environment "$1")"
    [[ -n "$env" ]] || return 0
    for name in RSI_DAEMON_SOCKET_PATH RSI_SOCKET MOTHERSHIP_SOCKET FLYWHEEL_SOCKET; do
        value="$(printf '%s\n' "$env" | sed -n "s/^$name=//p" | head -n 1)"
        if [[ -n "$value" ]]; then
            socket="$value"
            break
        fi
    done
    if [[ -z "$socket" ]]; then
        home="$(printf '%s\n' "$env" | sed -n 's/^HOME=//p' | head -n 1)"
        [[ -n "$home" ]] && socket="${home%/}/.rsi/daemon.sock"
    fi
    printf '%s\n' "$socket"
}

# Succeeds when PID holds a Unix socket bound at SOCKET (#399). This reads what
# the process serves, not what its environment says, so it also works where the
# environment is hidden: macOS 27 `ps eww` prints no environment for an
# Apple-signed binary. Needs lsof (stock on macOS); without it nothing matches.
rsid_pid_serves_socket() {
    local pid="$1" socket="$2" lsof_bin name
    [[ -n "$socket" ]] || return 1
    lsof_bin="$(command -v lsof 2>/dev/null || true)"
    [[ -z "$lsof_bin" && -x /usr/sbin/lsof ]] && lsof_bin=/usr/sbin/lsof
    [[ -n "$lsof_bin" ]] || return 1
    while IFS= read -r name; do
        [[ "$name" == "n$socket" ]] && return 0
    done < <("$lsof_bin" -w -n -a -p "$pid" -U -Fn 2>/dev/null || true)
    return 1
}

# Succeeds when PID serves this install's socket. Where /proc/PID/environ is
# readable (Linux) only the environment decides, as before. Elsewhere (macOS) a
# process that holds this install's socket is in scope even when its environment
# cannot be read (#399); the environment read stays as the second source, so a
# daemon still draining after it closed its listener is still waited on.
rsid_pid_in_scope() {
    local theirs expected
    expected="$(rsid_expected_socket)"
    if [[ ! -r "/proc/$1/environ" ]] && rsid_pid_serves_socket "$1" "$expected"; then
        return 0
    fi
    theirs="$(rsid_process_socket "$1")"
    [[ -n "$theirs" && "$theirs" == "$expected" ]]
}

# Print this install's live rsid PIDs. pgrep also reports zombies; the process
# has exited, so it cannot be stopped or indicate an active daemon. An rsid that
# serves another socket is ignored (see Scoping above).
rsid_running_pids() {
    local pid state
    while IFS= read -r pid; do
        [[ -n "$pid" ]] || continue
        state="$(ps -o stat= -p "$pid" 2>/dev/null || true)"
        [[ "$state" == Z* ]] && continue
        [[ -n "$state" ]] || continue
        rsid_pid_in_scope "$pid" || continue
        printf '%s\n' "$pid"
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

# #3433: announce that the caller is about to stop the daemon on purpose by
# signalling it directly (not through its supervisor). rsid-supervisor.sh
# restarts a daemon that ends on a SIGINT/SIGTERM nobody announced; one line
# `pid=<daemon pid> reason=<reason>` per pid in $RSI_HOME_DIR/intended-stop
# (the supervisor's RSID_SUPERVISOR_INTENDED_STOP_FILE) makes this one intended;
# the file is owner-private (0600) or the supervisor ignores it (#3507).
rsid_announce_intended_stop() {
    local reason="$1" file pid
    shift
    file="${RSID_SUPERVISOR_INTENDED_STOP_FILE:-}"
    if [[ -z "$file" ]]; then
        # #3507: the same owner-private locations the supervisor reads; never a
        # shared /tmp path. Without one the announcement is not made and the
        # supervisor judges the stop unannounced (fail closed).
        if [[ -n "${RSI_HOME_DIR:-}" ]]; then
            file="$RSI_HOME_DIR/intended-stop"
        elif [[ -n "${HOME:-}" ]]; then
            file="$HOME/.rsi/intended-stop"
        elif [[ -n "${XDG_RUNTIME_DIR:-}" && -d "$XDG_RUNTIME_DIR" ]]; then
            file="$XDG_RUNTIME_DIR/rsi/intended-stop"
        else
            echo "cannot announce the intended stop: no private per-user directory" >&2
            return 0
        fi
    fi
    if [[ -L "$file" ]]; then
        echo "cannot announce the intended stop: $file is a symlink" >&2
        return 0
    fi
    # The supervisor honours only an owner-owned regular file of mode 0600.
    (
        umask 077
        mkdir -p "$(dirname "$file")" 2>/dev/null || true
        for pid in "$@"; do
            printf 'pid=%s reason=%s\n' "$pid" "$reason" >>"$file" 2>/dev/null || true
        done
    )
    chmod 600 "$file" 2>/dev/null || true
}

rsid_pids_still_running() {
    local pid state
    for pid in "$@"; do
        state="$(ps -o stat= -p "$pid" 2>/dev/null || true)"
        [[ -n "$state" && "$state" != Z* ]] && printf '%s\n' "$pid"
    done
    return 0
}
