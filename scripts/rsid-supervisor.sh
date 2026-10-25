#!/usr/bin/env bash
# Restart only when rsid's independent watchdog asks for a new incarnation, and
# end a daemon shutdown that wedges (#2561, see "Shutdown backstop" below).
set -u

if [[ $# -ne 1 || ! -x $1 ]]; then
    echo "usage: rsid-supervisor.sh /path/to/rsid" >&2
    exit 2
fi

rsid_bin=$1
# systemd addresses activation to this supervisor, not its daemon children.
# Normalize its single listener to the portable handoff before any refresh;
# each rsid still validates the fd, path, ownership and mode before Store open.
# Preserve an explicit holder handoff and never adopt another process's fds.
if [[ -z ${RSI_LISTEN_FD+x} && ${LISTEN_PID:-} == "$$" ]]; then
    case ${LISTEN_FDS:-0} in
        1)
            export RSI_LISTEN_FD=3
            unset LISTEN_PID LISTEN_FDS LISTEN_FDNAMES
            ;;
        0) ;;
        *) echo "rsid supervisor requires exactly one systemd listening descriptor" >&2; exit 2 ;;
    esac
fi
child_pid=
last_child_pid=
unannounced=0
stopping=0
restart_window_seconds=3600
if [[ -n ${HOME:-} ]]; then
    default_restart_budget_file=$HOME/.rsi/deploy-restart-budget
    default_shutdown_marker=$HOME/.rsi/shutdown-begun
    default_incident_file=$HOME/.rsi/forced-shutdown-incident
    default_intended_stop_file=$HOME/.rsi/intended-stop
else
    default_restart_budget_file=/tmp/rsi-deploy-restart-budget
    default_shutdown_marker=/tmp/rsi-shutdown-begun
    default_incident_file=/tmp/rsi-forced-shutdown-incident
    # #3507: never a predictable shared /tmp path. The per-user runtime directory
    # (0700, owner only) when there is one; otherwise no announcement is honoured
    # and the stop is judged unannounced (fail closed).
    if [[ -n ${XDG_RUNTIME_DIR:-} && -d $XDG_RUNTIME_DIR ]]; then
        default_intended_stop_file=$XDG_RUNTIME_DIR/rsi/intended-stop
    else
        default_intended_stop_file=
    fi
fi
restart_budget_file=${RSID_SUPERVISOR_RESTART_BUDGET_FILE:-$default_restart_budget_file}
# #3492: the restart history this supervisor holds, published beside the budget
# snapshot so the daemon's deploy admission, the health wake and the TUI judge
# the same budget (`limit=`, `window=`, one `restart=<epoch>` per restart,
# `backoff_until=<epoch>` while it waits out an exhausted budget).
# With no HOME (and no explicit budget file) there is no private directory to
# publish into, so nothing is published rather than writing a shared /tmp path.
restarts_file=${RSID_SUPERVISOR_RESTARTS_FILE:-}
if [[ -z $restarts_file && ( -n ${HOME:-} || -n ${RSID_SUPERVISOR_RESTART_BUDGET_FILE:-} ) ]]; then
    restarts_file=$(dirname "$restart_budget_file")/supervisor-restarts
fi
# An exhausted budget never ends the supervisor (that left no rsid at all): it
# waits for room, at most this long, then restarts the daemon.
exhausted_backoff_max_seconds=${RSID_SUPERVISOR_EXHAUSTED_BACKOFF_MAX_SECONDS:-900}
# Shutdown backstop (#2561). The daemon writes `pid=<its pid> reason=<code>` to
# the marker when a shutdown begins (its path reaches the daemon as
# RSID_SHUTDOWN_MARKER, which the daemon then removes from its environment so no
# child inherits it); the supervisor stopping counts as a shutdown beginning
# too. A marker whose pid is not this supervisor's child is foreign or stale: it
# is removed and starts no clock (#2594). The grace period is the operator
# setting `supervisor_shutdown_grace_secs`, published as line 2 of the restart
# budget snapshot. Past it the supervisor sends SIGTERM again, then SIGKILL to
# the daemon's process group, appends one line to the incident file (the next
# daemon start surfaces it to the manager), and takes the exit-75 path.
shutdown_marker=${RSID_SUPERVISOR_SHUTDOWN_MARKER:-$default_shutdown_marker}
incident_file=${RSID_SUPERVISOR_INCIDENT_FILE:-$default_incident_file}
# #3433: a daemon that ends on a SIGINT/SIGTERM nobody announced is restarted,
# not treated as an intended stop (a stop through this supervisor, a deploy, an
# operator drain or restart never gets there). Whoever stops the daemon on
# purpose by signalling it directly (scripts/install-release.sh) first appends
# `pid=<daemon pid> reason=<code>` to this file, which is the announcement. The
# settle window lets a `systemctl stop`, which signals the whole unit, deliver
# its SIGTERM to this supervisor too before the exit is judged.
intended_stop_file=${RSID_SUPERVISOR_INTENDED_STOP_FILE:-$default_intended_stop_file}
unannounced_settle_seconds=${RSID_SUPERVISOR_UNANNOUNCED_SETTLE_SECONDS:-2}
poll_seconds=${RSID_SUPERVISOR_POLL_SECONDS:-1}
term_wait_seconds=${RSID_SUPERVISOR_TERM_WAIT_SECONDS:-5}
default_shutdown_grace_seconds=120
# The process-group kill needs POSIX job control; Windows (Git Bash, MSYS,
# Cygwin) has no supervised daemon, so the backstop is explicitly unsupported.
case $(uname -s 2>/dev/null) in
    MINGW* | MSYS* | CYGWIN*) shutdown_backstop=0 ;;
    *) shutdown_backstop=1 ;;
esac
load_restart_budget() {
    local first_ok=0 grace_line=
    deploy_restarts_per_hour=2
    shutdown_grace_seconds=$default_shutdown_grace_seconds
    if [[ -r $restart_budget_file ]]; then
        # Line 1: deploy restarts per hour. Line 2 (optional, #2561): shutdown
        # grace seconds. A snapshot written before line 2 existed keeps the default.
        { IFS= read -r deploy_restarts_per_hour && first_ok=1; IFS= read -r grace_line; } \
            < "$restart_budget_file"
        if ((first_ok == 0)) \
            || [[ ! $deploy_restarts_per_hour =~ ^[0-9]+$ ]] \
            || ((deploy_restarts_per_hour < 1 || deploy_restarts_per_hour > 12)); then
            echo "invalid deploy restart budget in $restart_budget_file; using default 2" >&2
            deploy_restarts_per_hour=2
        fi
        if [[ -n $grace_line ]]; then
            if [[ $grace_line =~ ^[0-9]+$ ]] && ((grace_line >= 1 && grace_line <= 86400)); then
                shutdown_grace_seconds=$grace_line
            else
                echo "invalid shutdown grace in $restart_budget_file; using default $default_shutdown_grace_seconds" >&2
            fi
        fi
    fi
    # Reserve one restart for rollback/watchdog recovery.
    max_restarts_per_window=$((deploy_restarts_per_hour + 1))
}
load_restart_budget
restart_times=()
read -r -a restart_times <<< "${RSID_SUPERVISOR_RESTART_TIMES:-}"

# #3492: write the restart history (atomically) for the daemon and the TUI. A
# failure only costs them the shared view, never a restart.
publish_restarts() {
    local backoff_until=${1:-} tmp restarted_at
    [[ -n $restarts_file ]] || return 0
    mkdir -p "$(dirname "$restarts_file")" 2>/dev/null || return 0
    tmp=$restarts_file.tmp.$$
    {
        printf 'limit=%s\nwindow=%s\n' "$max_restarts_per_window" "$restart_window_seconds"
        for restarted_at in ${restart_times[@]+"${restart_times[@]}"}; do
            printf 'restart=%s\n' "$restarted_at"
        done
        if [[ -n $backoff_until ]]; then printf 'backoff_until=%s\n' "$backoff_until"; fi
    } > "$tmp" 2>/dev/null && mv -f "$tmp" "$restarts_file" 2>/dev/null || rm -f "$tmp" 2>/dev/null
    return 0
}
publish_restarts
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
    export RSID_SUPERVISOR_RESTART_TIMES="${restart_times[*]-}"
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

# #2355: true when this exit-75 follows a daemon-requested deploy restart: the
# deploy swap writes `<real path>.deploy-inflight` before the daemon exits, so a
# marker no older than this child's start means this child requested the exit.
# A watchdog exit 75 with no fresh marker keeps the crash backoff.
deploy_requested_exit() {
    local marker_mtime
    [[ $1 -eq 75 ]] || return 1
    real_bin=$(readlink -f "$rsid_bin" 2>/dev/null || echo "$rsid_bin")
    [[ -e "$real_bin.deploy-inflight" ]] || return 1
    marker_mtime=$(stat -c %Y "$real_bin.deploy-inflight" 2>/dev/null \
        || stat -f %m "$real_bin.deploy-inflight" 2>/dev/null) || return 1
    ((marker_mtime >= started_at))
}

# #2561: append one incident line for the next daemon start to surface.
record_forced_shutdown() {
    local trigger=$1 signal=$2
    mkdir -p "$(dirname "$incident_file")" 2>/dev/null || true
    printf 'forced_at=%s trigger=%s grace_seconds=%s pid=%s signal=%s\n' \
        "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$trigger" "$shutdown_grace_seconds" \
        "$child_pid" "$signal" >> "$incident_file" \
        || echo "could not record the forced shutdown in $incident_file" >&2
}

# #3433: append one line for the next daemon start to surface (the same file
# and surfacing as a forced shutdown, #2561).
record_incident() {
    local trigger=$1 signal=$2 pid=$3
    mkdir -p "$(dirname "$incident_file")" 2>/dev/null || true
    printf 'forced_at=%s trigger=%s grace_seconds=0 pid=%s signal=%s\n' \
        "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$trigger" "$pid" "$signal" >> "$incident_file" \
        || echo "could not record the incident in $incident_file" >&2
}

# #3433: true when this daemon announced a signal-driven shutdown nobody
# announced beforehand. The daemon writes its marker (`pid=<pid> reason=ctrl_c`
# or `reason=sigterm`, plus `sender_pid=`) when it receives SIGINT or SIGTERM;
# any other reason (an operator drain, ...) is an intended stop. An
# announcement in $intended_stop_file naming the daemon, written no earlier
# than this launch, makes the signal intended and is consumed here. Sets
# unannounced_signal and unannounced_sender.
unannounced_signal=
unannounced_sender=

# #3507: an announcement is honoured only from a regular file (never a symlink)
# that this uid owns with mode 0600 or stricter; anything else is ignored, so
# another user's or a planted file can never excuse an unannounced stop. An
# empty path (no private location on this host) honours nothing. Prints why when
# an existing file is refused. stat -c is GNU, stat -f is BSD/macOS.
intended_stop_file_trusted() {
    local file=$1 owner mode
    [[ -n $file ]] || return 1
    [[ -e $file || -L $file ]] || return 1
    if [[ -L $file || ! -f $file ]]; then
        echo "ignoring intended-stop announcement $file: not a regular file" >&2
        return 1
    fi
    owner=$(stat -c %u "$file" 2>/dev/null || stat -f %u "$file" 2>/dev/null) || owner=
    mode=$(stat -c %a "$file" 2>/dev/null || stat -f %Lp "$file" 2>/dev/null) || mode=
    if [[ $owner != "$(id -u)" ]]; then
        echo "ignoring intended-stop announcement $file: not owned by this user" >&2
        return 1
    fi
    if [[ ! $mode =~ ^[0-7]{3,4}$ ]] || (( (8#$mode & ~8#600) != 0 )); then
        echo "ignoring intended-stop announcement $file: mode $mode is looser than 0600" >&2
        return 1
    fi
    return 0
}
unannounced_signal_stop() {
    local line reason= pid= sender= marker_mtime intent_line intent_mtime
    unannounced_signal=
    unannounced_sender=
    [[ -e $shutdown_marker ]] || return 1
    IFS= read -r line < "$shutdown_marker" 2>/dev/null || true
    [[ $line =~ (^|[[:space:]])pid=([0-9]+)([[:space:]]|$) ]] && pid=${BASH_REMATCH[2]}
    [[ $line =~ (^|[[:space:]])reason=([a-z_]+)([[:space:]]|$) ]] && reason=${BASH_REMATCH[2]}
    [[ $line =~ (^|[[:space:]])sender_pid=([0-9]+)([[:space:]]|$) ]] && sender=${BASH_REMATCH[2]}
    [[ $pid == "$child_pid" ]] || return 1
    case $reason in
        ctrl_c) unannounced_signal=INT ;;
        sigterm) unannounced_signal=TERM ;;
        *) return 1 ;;
    esac
    unannounced_sender=${sender:-unknown}
    if intended_stop_file_trusted "$intended_stop_file"; then
        intent_mtime=$(stat -c %Y "$intended_stop_file" 2>/dev/null \
            || stat -f %m "$intended_stop_file" 2>/dev/null || echo 0)
        while IFS= read -r intent_line || [[ -n $intent_line ]]; do
            if [[ $intent_line =~ (^|[[:space:]])pid=${child_pid}([[:space:]]|$) ]] \
                && ((intent_mtime >= started_at)); then
                rm -f "$intended_stop_file"
                return 1
            fi
        done < "$intended_stop_file"
    fi
    return 0
}

# Let a stop that signals the whole unit reach this supervisor before judging
# a daemon exit; ends early once the supervisor is told to stop.
settle_unannounced() {
    local slices=$((${unannounced_settle_seconds%%.*} * 10)) i
    for ((i = 0; i < slices; i++)); do
        [[ $stopping -eq 0 ]] || return 0
        sleep 0.1 &
        wait $! 2>/dev/null || true
    done
}

# Sleep without delaying a trap: `wait` returns as soon as a signal arrives.
# Where bash has `wait -n` (4.3+) the nap also ends the moment the daemon exits,
# so a quick exit is never delayed by the poll interval.
if ((BASH_VERSINFO[0] > 4 || (BASH_VERSINFO[0] == 4 && BASH_VERSINFO[1] >= 3))); then
    nap() {
        local napper
        sleep "$1" &
        napper=$!
        wait -n 2>/dev/null || true
        kill "$napper" 2>/dev/null || true
        wait "$napper" 2>/dev/null || true
    }
else
    # No `wait -n` (bash 3.2, the macOS system bash): a single `sleep "$1"` would
    # always run its full interval after the daemon exited, adding the poll
    # interval to every restart (#136: 4 starts took 11.8 s instead of 7 s). Nap in
    # tenths of a second and stop as soon as the daemon is gone; `wait` on each
    # slice still returns the moment a signal arrives.
    nap() {
        local slices=$((${1%%.*} * 10)) i
        ((slices >= 1)) || slices=1
        for ((i = 0; i < slices; i++)); do
            [[ -z ${child_pid:-} ]] || kill -0 "$child_pid" 2>/dev/null || return 0
            sleep 0.1 &
            wait $! 2>/dev/null || true
        done
    }
fi

# SIGTERM again, then SIGKILL, to the daemon's process group (it was started
# with job control, so its group id is its pid). Sets forced_exit.
force_end_child() {
    local trigger=$1 signal=TERM waited_from
    echo "rsid shutdown ($trigger) did not finish within ${shutdown_grace_seconds}s; sending SIGTERM, then SIGKILL, to process group $child_pid" >&2
    kill -TERM -- "-$child_pid" 2>/dev/null || kill -TERM "$child_pid" 2>/dev/null || true
    waited_from=$SECONDS
    while kill -0 "$child_pid" 2>/dev/null && ((SECONDS - waited_from < term_wait_seconds)); do
        nap "$poll_seconds"
    done
    if kill -0 "$child_pid" 2>/dev/null; then
        signal=KILL
        kill -KILL -- "-$child_pid" 2>/dev/null || kill -KILL "$child_pid" 2>/dev/null || true
    fi
    # Anything the daemon left in its group goes with it.
    kill -KILL -- "-$child_pid" 2>/dev/null || true
    record_forced_shutdown "$trigger" "$signal"
    forced_exit=1
}

# #2594: true when the marker exists and names this daemon child as its writer.
# A marker naming another pid (a stranger's, a previous daemon's) is removed so
# it can never start the grace clock. An empty or unparseable one starts no
# clock either but is left alone: it may be a write in progress.
own_shutdown_marker() {
    local line pid=
    [[ -e $shutdown_marker ]] || return 1
    IFS= read -r line < "$shutdown_marker" 2>/dev/null || true
    if [[ $line =~ (^|[[:space:]])pid=([0-9]+)([[:space:]]|$) ]]; then
        pid=${BASH_REMATCH[2]}
    fi
    [[ -n $pid ]] || return 1
    if [[ $pid == "$child_pid" ]]; then
        return 0
    fi
    echo "ignoring shutdown marker $shutdown_marker written by pid $pid (daemon is $child_pid)" >&2
    rm -f "$shutdown_marker"
    return 1
}

# Wait for the daemon, bounding its shutdown. Polling (rather than a bare
# `wait`) is what lets the supervisor notice a shutdown that never finishes.
supervise_child() {
    local shutdown_began=
    # Without the backstop the caller's plain `wait` is the whole supervision.
    if ((shutdown_backstop == 0)); then
        return
    fi
    while kill -0 "$child_pid" 2>/dev/null; do
        if [[ -z $shutdown_began ]] && { [[ $stopping -eq 1 ]] || own_shutdown_marker; }; then
            shutdown_began=$SECONDS
            # The operator may change the setting while the daemon ran.
            load_restart_budget
            echo "rsid shutdown began; allowing ${shutdown_grace_seconds}s" >&2
        fi
        if [[ -n $shutdown_began ]] && ((SECONDS - shutdown_began >= shutdown_grace_seconds)); then
            if [[ $stopping -eq 1 ]]; then
                force_end_child supervisor_stop
            else
                force_end_child daemon_shutdown
            fi
            return
        fi
        nap "$poll_seconds"
    done
}

# Keep only the restarts still inside the window (needs $now).
prune_restart_times() {
    local restarted_at recent=()
    # #2936: `${a[@]+"${a[@]}"}` keeps an empty array legal under `set -u` on
    # bash 3.2 (macOS /bin/bash), where expanding an empty array is an error.
    for restarted_at in ${restart_times[@]+"${restart_times[@]}"}; do
        if ((now - restarted_at < restart_window_seconds)); then
            recent+=("$restarted_at")
        fi
    done
    restart_times=(${recent[@]+"${recent[@]}"})
}

# Wait out an exhausted budget; a stop signal ends the wait at once.
exhausted_backoff() {
    local waited_from=$SECONDS
    while ((SECONDS - waited_from < $1)) && [[ $stopping -eq 0 ]]; do
        sleep 1 &
        wait $! 2>/dev/null || true
    done
}

if ((shutdown_backstop == 0)); then
    echo "rsid shutdown backstop is unsupported on this platform (no process-group kill); a wedged shutdown needs a manual kill" >&2
fi
forced_exit=0
while :; do
    if [[ $stopping -eq 1 ]]; then
        exit 0
    fi
    recover_pending_database
    started_at=$(date +%s)
    rm -f "$shutdown_marker"
    forced_exit=0
    # Job control for the launch only: the daemon becomes its own process group
    # leader, so the backstop can SIGKILL exactly its group.
    if ((shutdown_backstop == 1)); then set -m; fi
    RSID_SHUTDOWN_MARKER=$shutdown_marker "$rsid_bin" &
    child_pid=$!
    set +m
    supervise_child
    wait "$child_pid"
    status=$?
    unannounced=0
    if [[ $stopping -eq 0 && $status -eq 0 && $forced_exit -eq 0 ]] && unannounced_signal_stop; then
        settle_unannounced
        if [[ $stopping -eq 0 ]]; then
            unannounced=1
            # The restart follows the exit-75 path and its bounded budget.
            status=75
            echo "rsid stopped by an unannounced SIG${unannounced_signal} (sender pid ${unannounced_sender}; see daemon.log); restarting it" >&2
            record_incident "unannounced_signal" "$unannounced_signal" "$child_pid"
        fi
    fi
    if [[ $stopping -eq 1 ]]; then
        # wait can return early when a signal fires; finish reaping the child.
        wait "$child_pid" 2>/dev/null || true
        exit 0
    fi
    if ((forced_exit == 1)); then
        # A forced end follows the exit-75 path: restart on the intended
        # binary. A just-restarted binary the daemon has not yet verified (its
        # deploy marker is still present) that wedges inside the fast-failure
        # window is a failed deploy, so it takes the rollback branch below.
        real_bin=$(readlink -f "$rsid_bin" 2>/dev/null || echo "$rsid_bin")
        if [[ $after_restart -eq 1 && -e "$real_bin.deploy-inflight" ]] \
            && (($(date +%s) - started_at < fast_failure_seconds)); then
            status=1
        else
            status=75
        fi
    fi
    last_child_pid=$child_pid
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
            # #1801: an inherited listener is only usable by a binary that reads
            # RSI_LISTEN_FD. Falling back onto one that predates it would bind
            # the socket path itself and fight the holder, so refuse by name
            # before touching the database, markers or binaries.
            if [[ -n ${RSI_LISTEN_FD+x} ]] && ! grep -aq RSI_LISTEN_FD "$real_bin.prev"; then
                echo "rsid fallback refused: $real_bin.prev does not understand RSI_LISTEN_FD while this supervisor holds an inherited listener; leaving $real_bin and the deploy marker in place" >&2
                exit "$status"
            fi
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
    # Operator settings can change while this supervisor is waiting for its
    # child. Read the daemon-published snapshot for every restart decision.
    load_restart_budget
    now=$(date +%s)
    prune_restart_times
    if ((${#restart_times[@]} >= max_restarts_per_window)); then
        # #3492: past the budget the supervisor WAITS for room (the oldest restart
        # leaving the window, at most the cap below) and then restarts the daemon;
        # exiting left the host with no rsid at all. The wait is published for the
        # TUI and deploy admission, and an incident tells the next daemon start to
        # notify the operator and the manager.
        frees_in=$((restart_times[${#restart_times[@]} - max_restarts_per_window] + restart_window_seconds - now))
        if ((frees_in < 1)); then frees_in=1; fi
        if ((frees_in > exhausted_backoff_max_seconds)); then frees_in=$exhausted_backoff_max_seconds; fi
        record_incident "restart_budget_exhausted" "${unannounced_signal:-none}" "$last_child_pid"
        echo "rsid watchdog restart budget exhausted ($max_restarts_per_window in $restart_window_seconds seconds); not exiting: restarting the daemon in ${frees_in}s" >&2
        publish_restarts $((now + frees_in))
        exhausted_backoff "$frees_in"
        if [[ $stopping -eq 1 ]]; then
            exit 0
        fi
        now=$(date +%s)
        prune_restart_times
    fi
    restart_times+=("$now")
    publish_restarts
    if deploy_requested_exit "$status"; then
        # #2355: the daemon asked for this exit-75 (it wrote the deploy marker
        # during this run), so there is no crash to back off from. The restart
        # still counts against the budget above.
        echo "rsid deploy restart ${#restart_times[@]}/$max_restarts_per_window; no backoff" >&2
    else
        backoff_seconds=$((1 << (${#restart_times[@]} - 1)))
        echo "rsid watchdog restart ${#restart_times[@]}/$max_restarts_per_window; waiting ${backoff_seconds}s" >&2
        sleep "$backoff_seconds"
    fi
    if [[ $stopping -eq 0 ]]; then
        refresh_supervisor
    fi
done
