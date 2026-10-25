#!/usr/bin/env bash
# #1862: `install-release.sh --now` starts rsid when none is running and exits
# nonzero when no healthy daemon answers afterwards; #2820: `rsid-start-satellite`
# starts the deployed daemon on a freshly booted satellite. Shell port of the
# former test_install_release_start.py (#404, no-Python #358); every case keeps
# its name, intent and assertions.
#
# Hermetic: HOME is a temp dir, the build is a stub, and `pgrep`, `uname`,
# `systemctl`, `systemd-run` and `launchctl` are fakes first on PATH, so the live
# daemon, its holder unit or launchd agent and its supervisor are never seen,
# started or signalled (also when this runs on macOS, #2936). The fake holder
# runs a fake daemon child on a temp Unix socket; readiness checks the sender of
# that child's GetHealthStatus reply. Bash cannot bind a Unix socket, so the fake
# daemon is a short Perl socket server (no Python, #358).
#
# Child ownership (#232): the fake launchers detach from the installer, so the
# fixture cannot `wait` on them. Instead the fixture owns a lifeline FIFO: it
# keeps a read end (fd 5), and the write end (fd 6) is inherited by every fake
# launcher and daemon. End of file on the read end, after the fixture closes its
# own write end, proves every fixture-owned process has exited, including a child
# that has not yet published a PID marker. Cleanup asks the fake holder to stop;
# the holder owns the daemon child and stops and reaps it, so the parent-child
# shutdown order is preserved. Processes the fixture owns directly (stand-ins,
# foreign daemons) close the lifeline.
#
# Live rsid stand-ins (#3476, #399, #404): a process whose comm is `rsid`. On
# macOS it is a copy of nc re-signed ad hoc that binds a real Unix socket with an
# environment naming no socket, so only the socket it holds (lsof) scopes it, as
# a real daemon. On Linux it is bash run through a symlink named rsid, scoped by
# the environment the helper reads from /proc, recording each signal it gets.
#
# Bash 3.2 compatible (macOS /bin/bash): no empty "${a[@]}" under `set -u`.
set -euo pipefail

SCRIPTS="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
# Every case copies this directory: refuse anything but the repo's scripts/.
[[ -f "$SCRIPTS/install-release.sh" && -f "$SCRIPTS/rsid-start-satellite" ]] || {
    echo "test_install_release_start.sh: $SCRIPTS is not the repo scripts/ directory" >&2
    exit 1
}
HOST_OS="$(uname -s)"
REAL_PGREP="$(command -v pgrep || echo /usr/bin/pgrep)"
RELEASE_BINS="rsi rsid rsi-agent-mcp rsi-build-rustc rsi-contract-validate rsi-known-failure rsi-turn-shim"
# Bound of one installer run: allow 120 s for restart/health subprocesses on
# loaded test hosts (#3888). The watchdog still fails a hung installer.
INSTALL_TIMEOUT=120
NOT_LINUX="systemd launcher is Linux-only"
NATIVE_DARWIN_ONLY="the macOS health-client branch (LOCAL_PEERPID) runs natively on macOS only; faking it on Linux needs a Python shim (#358)"

needed="perl"
[[ "$HOST_OS" != Darwin ]] || needed="$needed nc lsof codesign"
for tool in $needed; do
    if ! command -v "$tool" >/dev/null 2>&1 && [[ ! -x "/usr/sbin/$tool" ]]; then
        printf 'test_install_release_start.sh needs %s on %s\n' "$tool" "$HOST_OS" >&2
        exit 1
    fi
done

# The readiness probe (#465) is the rsi-repo-guard `rsid-health-probe` subcommand:
# build it once through the repo shim and hand it to every installer run, so no case
# depends on a binary installed on the host.
RSI_REPO_GUARD_BIN="$("$SCRIPTS/repo-guard" --bin)" || {
    echo "test_install_release_start.sh: scripts/repo-guard could not build rsi-repo-guard" >&2
    exit 1
}
export RSI_REPO_GUARD_BIN

write_executable() {
    mkdir -p "$(dirname -- "$1")"
    cat >"$1"
    chmod 755 "$1"
}

# ---------------------------------------------------------------- assertions

fail() {
    printf 'FAIL %s: %s\n' "$CASE" "$*" >&2
    if [[ -n "${OUT:-}" && -f "$OUT" ]]; then
        printf -- '--- last run: exit %s\n--- stdout:\n' "${STATUS:-?}" >&2
        tail -n 40 "$OUT" >&2 || true
        printf -- '--- stderr:\n' >&2
        tail -n 40 "$ERR" >&2 || true
    fi
    exit 1
}

skip() {
    printf 'skip %s: %s\n' "$CASE" "$1"
    exit 77
}

linux_only() { [[ "$HOST_OS" == Linux ]] || skip "${1:-$NOT_LINUX}"; }
darwin_only() { [[ "$HOST_OS" == Darwin ]] || skip "$1"; }

assert_ok() { [[ "$STATUS" -eq 0 ]] || fail "expected exit 0${1:+ ($1)}"; }
assert_failed() { [[ "$STATUS" -ne 0 ]] || fail "expected a nonzero exit${1:+ ($1)}"; }
assert_in() { grep -qF -- "$2" "$1" 2>/dev/null || fail "expected '$2' in $(basename -- "$1")"; }
assert_not_in() { ! grep -qF -- "$2" "$1" 2>/dev/null || fail "unexpected '$2' in $(basename -- "$1")"; }
assert_line() { grep -qxF -- "$2" "$1" 2>/dev/null || fail "expected the line '$2' in $(basename -- "$1")"; }
assert_exists() { [[ -e "$1" ]] || fail "expected $1 to exist"; }
assert_missing() { [[ ! -e "$1" ]] || fail "expected $1 to be absent"; }
assert_eq() { [[ "$1" == "$2" ]] || fail "${3:-value}: expected '$2', got '$1'"; }

# Index (0-based) of the exact entry $1 among the remaining arguments, or -1.
index_of() {
    local want="$1" i=0 entry
    shift
    for entry in "$@"; do
        if [[ "$entry" == "$want" ]]; then
            echo "$i"
            return 0
        fi
        i=$((i + 1))
    done
    echo -1
}

# Field $2 of the rendered plist $1: a string, `true`/`false`, one array item
# per line, or `key=value` per dict entry (the template's one-tag-per-line form).
plist_field() {
    awk -v want="$2" -f "$FAKES/plist.awk" "$1"
}

running() {
    local state
    state="$(ps -o stat= -p "$1" 2>/dev/null || true)"
    [[ -n "$state" && "$state" != Z* ]]
}

# ---------------------------------------------------------------- fixture

setup_fixture() {
    BASE="$(mktemp -d /tmp/rsi-irs.XXXXXX)" # short: AF_UNIX paths are bounded (104 bytes on macOS)
    HOME_DIR="$BASE/home"
    ROOT="$BASE/repo"
    TARGET="$BASE/target"
    STATE="$BASE/state"
    FAKES="$BASE/fakes"
    SOCKET="$BASE/s/daemon.sock"
    SERVER="$BASE/server.pl"
    OUT="" ERR="" STATUS=""
    STAND_INS="" FOREIGN="" OWNED="" STAND_IN_COUNT=0
    mkdir -p "$STATE" "$BASE/s"
    mkfifo "$BASE/lifeline"
    exec 4<>"$BASE/lifeline" 6>"$BASE/lifeline" 5<"$BASE/lifeline" 4>&-
    LIFELINE_OPEN=1

    mkdir -p "$ROOT/scripts"
    cp -R "$SCRIPTS"/. "$ROOT/scripts/"
    find "$ROOT/scripts" -name tests -type d -prune -exec rm -rf {} +
    printf '[workspace]\n[package]\nname = "stub"\nversion = "0.0.0"\nedition = "2021"\n' >"$ROOT/Cargo.toml"
    : | write_executable "$ROOT/src/lib.rs"
    local name
    for name in $RELEASE_BINS; do
        printf '#!/bin/sh\necho "fake-%s $*"\n' "$name" | write_executable "$TARGET/release/$name"
    done
    # #938: --link-only links only what this checkout built.
    printf 'worktree=%s\nstate=built\nhead=unknown\n' "$(cd "$ROOT" && pwd -P)" >"$TARGET/release/.rsi-release-source"
    # Fake build tools keep a fresh HOME offline, even for metadata.
    write_executable "$FAKES/cargo" <<'EOF'
#!/bin/sh
printf '{"target_directory":"%s"}\n' "$CARGO_TARGET_DIR"
EOF
    write_executable "$FAKES/rustup" <<'EOF'
#!/bin/sh
echo "unexpected rustup" >&2
exit 99
EOF

    # The fake daemon: a real temp Unix socket, so the probe checks real kernel
    # PID credentials of the reply's sender. Like the real holder, it reclaims
    # only a stale socket path that refuses connections.
    cat >"$SERVER" <<'EOF'
use strict;
use warnings;
use Errno qw(EADDRINUSE ECONNREFUSED);
use Socket qw(PF_UNIX SOCK_STREAM SOMAXCONN pack_sockaddr_un);

my ($path) = @ARGV;
my $state = $ENV{FAKE_STATE};
if ($ENV{FAKE_SERVER_GATE}) {
    # Test hook: hold startup, before any PID marker, until the fixture writes.
    open(my $gate, '<', $ENV{FAKE_SERVER_GATE}) or die "gate: $!";
    sysread($gate, my $byte, 1);
    close($gate);
}
open(my $marker, '>', "$state/daemon-$$") or die "marker: $!";
print {$marker} $$;
close($marker);
socket(my $listener, PF_UNIX, SOCK_STREAM, 0) or die "socket: $!";
unless (bind($listener, pack_sockaddr_un($path))) {
    die "bind $path: $!" unless $! == EADDRINUSE;
    socket(my $probe, PF_UNIX, SOCK_STREAM, 0) or die "socket: $!";
    die "bind $path: a live socket" if connect($probe, pack_sockaddr_un($path));
    die "bind $path: $!" unless $! == ECONNREFUSED;
    unlink($path);
    bind($listener, pack_sockaddr_un($path)) or die "bind $path: $!";
}
listen($listener, SOMAXCONN) or die "listen: $!";
$SIG{PIPE} = 'IGNORE';
while (accept(my $conn, $listener)) {
    my $request = '';
    sysread($conn, $request, 65536);
    if (open(my $log, '>>', "$state/rpc")) {
        print {$log} $request;
        close($log);
    }
    my $result = '{}';
    unless ($ENV{FAKE_NEVER_HEALTHY}) {
        my @fields = ('"persistence_queue_capacity":1', '"project_cache_size":0');
        unless ($ENV{FAKE_LEGACY_HEALTH}) {
            my $pid = $ENV{FAKE_WRONG_PID} ? getppid() : $$;
            my $started = $ENV{FAKE_BAD_START} ? 'invalid' : '2026-10-12T00:00:00.000000000Z';
            push @fields, qq("daemon_pid":$pid), qq("daemon_started_at":"$started");
        }
        $result = '{' . join(',', @fields) . '}';
    }
    if (defined syswrite($conn, qq({"jsonrpc":"2.0","id":1,"result":$result}\n))) {
        sysread($conn, my $byte, 1); # until the client closes
    }
    close($conn);
}
EOF

    # The fake holder: publishes its PID, runs the daemon child and stops and
    # reaps it on SIGTERM, then removes the socket path on the way out.
    write_executable "$TARGET/release/rsi-socket-hold" <<'EOF'
#!/usr/bin/env bash
state="$FAKE_STATE" child="" stopping=0
if [[ -z "${FAKE_HOLDER_EXIT:-}${FAKE_HOLDER_IDLE:-}" ]]; then
    # The holder owns the daemon child: stop it, then exit once it is reaped.
    trap 'stopping=1; [[ -z "$child" ]] || kill -TERM "$child" 2>/dev/null' TERM # before the PID is published
fi
printf '%s' "$*" >"$state/started"
printf '%s' "$$" >"$state/holder"
[[ -z "${FAKE_HOLDER_EXIT:-}" ]] || exit 1
[[ -z "${FAKE_HOLDER_IDLE:-}" ]] || exec sleep 15
perl "$FAKE_SERVER" "$1" &
child=$!
[[ "$stopping" -eq 0 ]] || kill -TERM "$child" 2>/dev/null
status=0
while :; do
    wait "$child" && status=0 || status=$?
    kill -0 "$child" 2>/dev/null || break
done
rm -f "$1"
exit "$status"
EOF
    write_executable "$TARGET/release/rsi-rpc" <<'EOF'
#!/bin/sh
exit 99
EOF
    # No live rsid is ever visible; the platform and systemd are faked.
    write_executable "$FAKES/pgrep" <<'EOF'
#!/bin/sh
exit 1
EOF
    write_executable "$FAKES/uname" <<'EOF'
#!/bin/sh
[ "$1" = "-s" ] && echo "$FAKE_UNAME" || exec /usr/bin/uname "$@"
EOF
    write_executable "$FAKES/systemctl" <<'EOF'
#!/bin/sh
echo "$@" >>"$FAKE_STATE/systemctl"
case "$2" in
  cat) [ "$FAKE_MANAGED" = 1 ]; exit $? ;;
  restart) "$FAKE_RUN" -- "$HOME/.rsi/install/rsi-socket-hold" "$RSI_DAEMON_SOCKET_PATH" ;;
  start) [ "$3" = rsid.service ] && "$FAKE_RUN" -- "$HOME/.rsi/install/rsi-socket-hold" "$RSI_DAEMON_SOCKET_PATH" ;;
  is-active) exit 0 ;;
  show)
    case "$*" in
      *--property=MainPID*) cat "$FAKE_STATE/holder"; exit ;;
      *--property=ActiveState*--value) echo "$FAKE_UNIT_STATE"; exit ;;
    esac
    echo ActiveState=active
    echo "ControlGroup=/user.slice/x/$3"
    echo MemoryHigh=6442450944
    echo MemoryMax=8589934592
    echo MemorySwapMax=0
    echo CPUWeight=20 ;;
esac
exit 0
EOF
    write_executable "$FAKES/systemd-run" <<'EOF'
#!/usr/bin/env bash
printf '%s' "$*" >"$FAKE_STATE/systemd-run"
while [[ $# -gt 0 && "$1" != -- ]]; do shift; done
[[ $# -eq 0 ]] || shift
# Detached like a real unit; the inherited lifeline lets the fixture prove it ended.
"$@" </dev/null >/dev/null 2>&1 &
tries=0
until [[ -e "$FAKE_STATE/holder" ]]; do
    tries=$((tries + 1))
    [[ "$tries" -le 100 ]] || exit 1
    sleep 0.01
done
EOF

    # #2936: a fake launchd for the gui domain. It runs the loaded plist's
    # ProgramArguments detached (its EnvironmentVariables, WorkingDirectory and
    # log), reports `pid = N` only while that process lives, and, like launchd,
    # returns from bootout at once while the job keeps showing until its process
    # has exited. No real launchd job is ever touched.
    cat >"$FAKES/plist.awk" <<'EOF'
function unxml(s) {
    gsub(/&lt;/, "<", s); gsub(/&gt;/, ">", s); gsub(/&quot;/, "\"", s); gsub(/&apos;/, "'", s)
    gsub(/&amp;/, "\\&", s)
    return s
}
function inner(s) {
    sub(/^[ \t]*<[A-Za-z]+>/, "", s)
    sub(/<\/[A-Za-z]+>[ \t]*$/, "", s)
    return unxml(s)
}
/^[ \t]*<\/(dict|array)>/ { depth--; if (grab && depth == 1) exit; next }
/^[ \t]*<(dict|array)>/ { depth++; next }
depth == 1 && !grab && /^[ \t]*<key>/ { if (inner($0) == want) grab = 1; next }
grab && depth == 1 {
    if ($0 ~ /<true\/>/) print "true"
    else if ($0 ~ /<false\/>/) print "false"
    else print inner($0)
    exit
}
grab && depth == 2 {
    if ($0 ~ /^[ \t]*<key>/) key = inner($0)
    else if (key != "") { print key "=" inner($0); key = "" }
    else print inner($0)
}
EOF
    write_executable "$FAKES/launchctl" <<'EOF'
#!/usr/bin/env bash
set -u
state="$FAKE_STATE" jobs="$FAKE_STATE/launchd" awk_file="$(dirname -- "$0")/plist.awk"
mkdir -p "$jobs"
printf '%s\n' "$*" >>"$state/launchctl"
loaded="$jobs/loaded" stopping="$jobs/stopping" pid_file="$jobs/pid"
alive() {
    local pid process_state
    pid="$(cat "$pid_file" 2>/dev/null || true)"
    [[ "$pid" =~ ^[0-9]+$ ]] || return 1
    # A detached holder may remain as a zombie until the test host's init
    # reaps it. launchd reports a running process, not merely an allocated PID.
    process_state="$(ps -o stat= -p "$pid" 2>/dev/null || true)"
    [[ -n "$process_state" && "$process_state" != Z* ]] && printf '%s\n' "$pid"
}
field() { awk -v want="$1" -f "$awk_file" "$loaded"; }
spawn() {
    local line dir out
    local -a args=() envs=()
    while IFS= read -r line; do args+=("$line"); done < <(field ProgramArguments)
    while IFS= read -r line; do envs+=("$line"); done < <(field EnvironmentVariables)
    dir="$(field WorkingDirectory)"
    out="$(field StandardOutPath)"
    (cd "$dir" && exec env ${envs[@]+"${envs[@]}"} "${args[@]}") </dev/null >>"$out" 2>&1 &
    printf '%s\n' "$!" >"$pid_file"
}
if [[ -e "$stopping" ]] && ! alive >/dev/null; then
    rm -f "$stopping"
fi
case "${1:-}" in
    print)
        [[ -e "$loaded" || -e "$stopping" ]] || exit 113
        pid="$(alive || true)"
        echo "gui/0/dev.rsi.rsid = {"
        if [[ -n "$pid" ]]; then
            printf '\tstate = running\n\tpid = %s\n' "$pid"
        else
            printf '\tstate = not running\n'
        fi
        echo "}" ;;
    bootstrap)
        if [[ -e "$loaded" || -e "$stopping" ]]; then exit 5; fi
        cp "$3" "$loaded"
        spawn ;;
    bootout)
        if [[ -e "$loaded" ]]; then
            rm -f "$loaded"
            pid="$(alive || true)"
            if [[ -n "$pid" ]]; then
                touch "$stopping"
                kill -TERM "$pid"
            fi
        fi ;;
    kickstart)
        [[ -e "$loaded" ]] || exit 113
        alive >/dev/null || spawn ;;
esac
exit 0
EOF
}

holder_pid() {
    # The PID published by this fixture's fake holder; a half-written file is
    # read again on the next tick.
    local pid
    pid="$(cat "$STATE/holder" 2>/dev/null || true)"
    [[ "$pid" =~ ^[0-9]+$ ]] && echo "$pid"
    return 0
}

# Wait up to $1 seconds for EOF on the lifeline; a background reader exits on it.
lifeline_closed_within() {
    local deadline=$((SECONDS + $1))
    while kill -0 "$LIFELINE_READER" 2>/dev/null; do
        [[ "$SECONDS" -lt "$deadline" ]] || return 1
        sleep 0.02
    done
}

# Stop every fake launcher and daemon this fixture started. Returns only once
# the lifeline reports every owner gone, so no fixture process can still write
# into the temp directory.
stop_launchers() {
    [[ -n "${LIFELINE_OPEN:-}" ]] || return 0
    LIFELINE_OPEN=""
    exec 6>&-
    cat <&5 >/dev/null 5<&- &
    LIFELINE_READER=$!
    exec 5<&-
    local signalled="" pid deadline=$((SECONDS + 5))
    while kill -0 "$LIFELINE_READER" 2>/dev/null; do
        # The holder stops and reaps its own daemon child. A holder still
        # starting is asked as soon as it publishes its PID.
        pid="$(holder_pid)"
        if [[ -n "$pid" && " $signalled " != *" $pid "* ]]; then
            signalled="$signalled $pid"
            kill -TERM "$pid" 2>/dev/null || true
        fi
        if [[ "$SECONDS" -ge "$deadline" ]]; then
            # Last resort for a wedged fake holder.
            pid="$(holder_pid)"
            [[ -z "$pid" ]] || kill -KILL "$pid" 2>/dev/null || true
            if ! lifeline_closed_within 5; then
                kill -KILL "$LIFELINE_READER" 2>/dev/null || true
                echo "FAIL $CASE: fixture launchers outlived cleanup; a child never published its PID" >&2
                return 1
            fi
            break
        fi
        sleep 0.02
    done
    wait "$LIFELINE_READER" 2>/dev/null || true
}

cleanup_case() {
    local status=$? pid
    set +e
    for pid in ${STAND_INS:-} ${OWNED:-}; do
        kill -KILL "$pid" 2>/dev/null
        wait "$pid" 2>/dev/null
    done
    for pid in ${FOREIGN:-}; do
        kill -TERM "$pid" 2>/dev/null
        wait "$pid" 2>/dev/null
    done
    stop_launchers || status=1
    [[ -z "${BASE:-}" ]] || rm -rf "$BASE"
    if [[ "$status" -ne 0 && "$status" -ne 77 ]]; then
        printf 'FAIL %s (exit %s)\n' "$CASE" "$status" >&2
    fi
    exit "$status"
}

# A pgrep that reports only the given PIDs, and only while the real pgrep sees
# them under the name rsid. The host's own rsid (the hub) is therefore never
# visible to the script under test.
write_pgrep() {
    printf '%s\n' "$@" >"$STATE/pgrep-pids"
    printf '#!/bin/sh\n%s "$@" | grep -x -F -f "%s"\n' "$REAL_PGREP" "$STATE/pgrep-pids" | write_executable "$FAKES/pgrep"
}

# rsid_stand_in in_scope|other exit|ignore: start a harmless live process whose
# comm is `rsid` (#3476) and set STAND_IN_PID / STAND_IN_LOG. "exit" leaves on
# SIGINT like a daemon would; "ignore" keeps running. An in-scope stand-in
# serves this install's socket; the other one serves a socket under another RSI
# home. On Linux it records each signal in STAND_IN_LOG; on macOS (nc) SIGINT
# is ignored in "ignore" mode and ends it with status 130 in "exit" mode.
rsid_stand_in() {
    local scope="$1" mode="$2" dir socket other="$BASE/other-home"
    STAND_IN_COUNT=$((STAND_IN_COUNT + 1))
    STAND_IN_LOG="$STATE/stand-in-$STAND_IN_COUNT.log"
    dir="$BASE/decoys/$scope"
    mkdir -p "$dir" "$other/.rsi"
    if [[ "$scope" == in_scope ]]; then socket="$SOCKET"; else socket="$other/.rsi/daemon.sock"; fi
    if [[ "$HOST_OS" == Darwin ]]; then
        if [[ ! -x "$dir/rsid" ]]; then
            # A copy of nc named rsid (comm `rsid`), re-signed ad hoc so the copy may run.
            cp "$(command -v nc)" "$dir/rsid"
            codesign -f -s - "$dir/rsid" >/dev/null 2>&1
        fi
        # Job control keeps SIGINT at its default in "exit" mode; a plain
        # background job ignores it, which is what "ignore" mode wants.
        [[ "$mode" != exit ]] || set -m
        if [[ "$scope" == in_scope ]]; then
            # The environment names no socket: only the socket it holds scopes it (#399).
            env -i HOME=/nonexistent "$dir/rsid" -lU "$socket" </dev/null >/dev/null 2>&1 5<&- 6>&- &
        else
            env -i HOME="$other" RSI_SOCKET="$socket" "$dir/rsid" -lU "$socket" </dev/null >/dev/null 2>&1 5<&- 6>&- &
        fi
        STAND_IN_PID=$!
        set +m
        STAND_INS="$STAND_INS $STAND_IN_PID"
        local deadline=$((SECONDS + 5))
        until [[ -S "$socket" ]]; do
            running "$STAND_IN_PID" && [[ "$SECONDS" -lt "$deadline" ]] || fail "rsid stand-in failed to start"
            sleep 0.01
        done
        return 0
    fi
    [[ -e "$dir/rsid" ]] || ln -s "$(command -v bash)" "$dir/rsid"
    local script='log="$1" mode="$2"
on() { printf "%s\n" "$1" >>"$log"; [ "$mode" != exit ] || exit 0; }
trap "on SIGINT" INT; trap "on SIGTERM" TERM; trap "on SIGHUP" HUP
trap "on SIGUSR1" USR1; trap "on SIGUSR2" USR2; trap "on SIGQUIT" QUIT
: >"$log.ready"
while :; do sleep 1 & wait $!; done'
    set -m # job control: SIGINT and SIGQUIT are not ignored, so the traps take them
    # A noninteractive runner may itself inherit SIGINT ignored. Bash cannot
    # trap a signal ignored at entry, even with job control (#3888). Reset it
    # before exec using the Perl already required by this shell fixture.
    local reset_signals='$SIG{INT}="DEFAULT"; $SIG{QUIT}="DEFAULT"; exec @ARGV or die "exec: $!";'
    if [[ "$scope" == in_scope ]]; then
        env -i PATH="$PATH" HOME="$HOME_DIR" RSI_DAEMON_SOCKET_PATH="$socket" \
            perl -e "$reset_signals" "$dir/rsid" -c "$script" rsid "$STAND_IN_LOG" "$mode" </dev/null >/dev/null 2>&1 5<&- 6>&- &
    else
        env -i PATH="$PATH" HOME="$other" RSI_SOCKET="$socket" \
            perl -e "$reset_signals" "$dir/rsid" -c "$script" rsid "$STAND_IN_LOG" "$mode" </dev/null >/dev/null 2>&1 5<&- 6>&- &
    fi
    STAND_IN_PID=$!
    set +m
    STAND_INS="$STAND_INS $STAND_IN_PID"
    local deadline=$((SECONDS + 5))
    until [[ -e "$STAND_IN_LOG.ready" ]]; do
        running "$STAND_IN_PID" && [[ "$SECONDS" -lt "$deadline" ]] || fail "rsid stand-in failed to start"
        sleep 0.01
    done
}

# Signals the stand-in recorded (Linux), space-separated.
stand_in_signals() {
    [[ -f "$1" ]] || return 0
    tr '\n' ' ' <"$1" | sed 's/ $//'
}

# The stand-in $1 (log $2) got SIGINT, nothing else, and left.
assert_stopped_by_sigint() {
    local pid="$1" log="$2" status=0 deadline=$((SECONDS + 5))
    while running "$pid"; do
        [[ "$SECONDS" -lt "$deadline" ]] || fail "stand-in $pid did not exit"
        sleep 0.02
    done
    wait "$pid" || status=$?
    if [[ "$HOST_OS" == Darwin ]]; then
        assert_eq "$status" 130 "stand-in $pid exit status (128 + SIGINT)"
    else
        assert_eq "$(stand_in_signals "$log")" SIGINT "signals the stand-in $pid recorded"
        assert_eq "$status" 0 "stand-in $pid exit status"
    fi
}

# The stand-in $1 (log $2) was never signalled and still runs.
assert_untouched() {
    running "$1" || fail "stand-in $1 is no longer running"
    assert_eq "$(stand_in_signals "$2")" "" "signals the stand-in $1 recorded"
}

start_foreign_daemon() {
    # $1: extra environment (for example FAKE_LEGACY_HEALTH=1), may be empty.
    (cd "$BASE" && exec env FAKE_STATE="$STATE" ${1:+"$1"} perl "$SERVER" "$SOCKET") </dev/null >/dev/null 2>&1 5<&- 6>&- &
    local pid=$! deadline=$((SECONDS + 3))
    FOREIGN="$FOREIGN $pid"
    until [[ -S "$SOCKET" ]]; do
        running "$pid" && [[ "$SECONDS" -lt "$deadline" ]] || fail "foreign fixture failed to bind"
        sleep 0.01
    done
}

# Run "$@" under job control (SIGINT keeps its default, as for a foreground
# child) and stop it after INSTALL_TIMEOUT seconds. Sets STATUS.
run_bounded() {
    local pid dog
    set -m
    "$@" &
    pid=$!
    set +m
    (
        trap 'kill "$nap" 2>/dev/null; exit 0' TERM
        sleep "$INSTALL_TIMEOUT" &
        nap=$!
        wait "$nap"
        echo "timed out after ${INSTALL_TIMEOUT}s" >>"$STATE/timeouts"
        kill -TERM "$pid" 2>/dev/null
    ) </dev/null >/dev/null 2>&1 5<&- 6>&- &
    dog=$!
    STATUS=0
    wait "$pid" || STATUS=$?
    kill -TERM "$dog" 2>/dev/null || true
    wait "$dog" 2>/dev/null || true
    [[ ! -e "$STATE/timeouts" ]] || fail "$(cat "$STATE/timeouts")"
}

install_child() {
    export HOME="$HOME_DIR" CARGO_TARGET_DIR="$TARGET" PATH="$FAKES:$PATH" \
        FAKE_STATE="$STATE" FAKE_SERVER="$SERVER" FAKE_RUN="$FAKES/systemd-run" \
        FAKE_MANAGED="$I_MANAGED" FAKE_UNIT_STATE="$I_UNIT_STATE" \
        FAKE_HOLDER_EXIT="$I_HOLDER_EXIT" FAKE_HOLDER_IDLE="$I_HOLDER_IDLE" \
        FAKE_NEVER_HEALTHY="$I_NEVER_HEALTHY" FAKE_LEGACY_HEALTH="$I_LEGACY" \
        FAKE_WRONG_PID="$I_WRONG_PID" FAKE_BAD_START="$I_BAD_START" \
        FAKE_SERVER_GATE="$I_GATE" FAKE_UNAME="$I_UNAME" \
        RSI_INSTALL_HEALTH_WAIT_SECS="$I_WAIT" RSI_DAEMON_SOCKET_PATH="$SOCKET"
    local kv
    for kv in $I_EXTRA; do export "$kv"; done
    unset RSI_INSTALL_BIN_DIR RSI_SOCKET NOW
    cd "$BASE"
    exec bash "$ROOT/scripts/$I_SCRIPT" "$@" </dev/null >"$OUT" 2>"$ERR"
}

# install [OPTION ...] FLAG ...: run the installer (or script=NAME) in the
# fixture. Options: uname=NAME never_healthy holder_exit holder_idle managed
# unit_state=STATE legacy_health wrong_pid bad_start gate=PATH script=NAME
# running env:NAME=VALUE (no spaces). Sets STATUS, OUT and ERR.
install() {
    I_UNAME=Darwin I_NEVER_HEALTHY="" I_HOLDER_EXIT="" I_HOLDER_IDLE="" I_MANAGED=0
    I_UNIT_STATE=active I_LEGACY="" I_WRONG_PID="" I_BAD_START="" I_GATE=""
    I_SCRIPT=install-release.sh I_EXTRA=""
    local with_running=0
    while [[ $# -gt 0 ]]; do
        case "$1" in
            uname=*) I_UNAME="${1#uname=}" ;;
            never_healthy) I_NEVER_HEALTHY=1 ;;
            holder_exit) I_HOLDER_EXIT=1 ;;
            holder_idle) I_HOLDER_IDLE=1 ;;
            managed) I_MANAGED=1 ;;
            unit_state=*) I_UNIT_STATE="${1#unit_state=}" ;;
            legacy_health) I_LEGACY=1 ;;
            wrong_pid) I_WRONG_PID=1 ;;
            bad_start) I_BAD_START=1 ;;
            gate=*) I_GATE="${1#gate=}" ;;
            script=*) I_SCRIPT="${1#script=}" ;;
            running) with_running=1 ;;
            env:*) I_EXTRA="$I_EXTRA ${1#env:}" ;;
            *) break ;;
        esac
        shift
    done
    # A run that must come up healthy gets a bound that survives a busy host
    # (#140); a run that expects the wait to give up keeps it short.
    I_WAIT=20
    if [[ -n "$I_NEVER_HEALTHY$I_HOLDER_EXIT$I_HOLDER_IDLE$I_WRONG_PID$I_BAD_START" || "$I_UNIT_STATE" != active ]]; then
        I_WAIT=2
    fi
    if [[ "$with_running" -eq 1 ]]; then
        # A live process named rsid that serves this install's socket stands in for the daemon.
        rsid_stand_in in_scope ignore
        write_pgrep "$STAND_IN_PID"
    fi
    OUT="$BASE/out" ERR="$BASE/err"
    run_bounded install_child "$@"
}

# ---------------------------------------------------------------- InstallReleaseStartTest

test_now_link_only_starts_a_down_daemon_and_waits_for_health() {
    install --link-only --now
    assert_ok
    assert_in "$OUT" "starting it"
    assert_in "$OUT" "rsid healthy"
    assert_in "$STATE/started" "daemon.sock"
    assert_in "$STATE/started" "rsid-supervisor.sh"
    assert_in "$STATE/rpc" "GetHealthStatus"
}

test_no_daemon_after_start_exits_nonzero_with_a_clear_message() {
    install never_healthy --link-only --now
    assert_failed
    assert_in "$ERR" "NO DAEMON IS RUNNING"
    assert_in "$ERR" "GetHealthStatus"
}

test_every_direct_sigint_of_the_daemon_is_announced_first() {
    # #3433: rsid-supervisor.sh restarts a daemon that ends on a signal nobody
    # announced, so each direct `kill -INT` of the daemon follows an
    # `rsid_announce_intended_stop` for the same pids.
    local report
    report="$(awk '
        { line[NR] = $0 }
        /kill -INT/ { kills[++n] = NR }
        END {
            if (n != 2) { print "expected 2 kill -INT lines, got " n; exit 1 }
            for (k = 1; k <= n; k++) {
                i = kills[k]
                rest = substr(line[i], index(line[i], "kill -INT ") + 10)
                split(rest, words, /[ \t]+/)
                found = 0
                for (j = (i > 3 ? i - 3 : 1); j < i; j++)
                    if (index(line[j], "rsid_announce_intended_stop install_release " words[1])) found = 1
                if (!found) { print "no announcement before: " line[i]; bad = 1 }
            }
            exit bad
        }' "$SCRIPTS/install-release.sh")" || fail "$report"
}

test_no_restart_still_skips_the_start() {
    install --link-only --no-restart
    assert_ok
    assert_missing "$STATE/started"
}

test_linux_starts_a_down_daemon_in_a_collected_unit() {
    linux_only
    install uname=Linux --link-only --now
    assert_ok
    assert_in "$OUT" "starting it"
    assert_in "$STATE/systemd-run" "--collect"
    assert_in "$STATE/systemd-run" "rsi-socket-hold"
    assert_exists "$STATE/started"
}

test_linux_unhealthy_start_exits_nonzero() {
    linux_only
    install uname=Linux never_healthy --link-only --now
    assert_failed
    assert_in "$ERR" "NO DAEMON IS RUNNING"
}

test_foreign_daemon_cannot_mask_an_exited_holder() {
    local variant platform managed
    for variant in Darwin:0 Linux:0 Linux:1; do
        platform="${variant%:*}" managed="${variant#*:}"
        [[ "$platform" != Linux || "$HOST_OS" == Linux ]] || continue
        # Each iteration owns a separate fake launcher; the foreign daemon stays.
        [[ -S "$SOCKET" ]] || start_foreign_daemon ""
        if [[ "$managed" -eq 1 ]]; then
            install uname="$platform" holder_exit managed --link-only --now
        else
            install uname="$platform" holder_exit --link-only --now
        fi
        assert_failed "platform=$platform managed=$managed"
        assert_in "$ERR" "GetHealthStatus"
    done
}

test_foreign_daemon_cannot_mask_a_live_unrelated_holder() {
    start_foreign_daemon ""
    install holder_idle --link-only --now
    assert_failed
    assert_in "$ERR" "NO DAEMON IS RUNNING from this launch"
}

test_managed_service_identifies_its_daemon() {
    linux_only
    install uname=Linux managed --link-only --now
    assert_ok
    assert_in "$OUT" "rsid healthy"
}

test_legacy_health_keeps_kernel_identity_fallback() {
    linux_only "kernel fallback is Linux-only here"
    install uname=Linux managed legacy_health --link-only --now
    assert_ok
    assert_in "$OUT" "rsid healthy"
}

test_claimed_holder_pid_cannot_replace_replying_daemon_identity() {
    linux_only "checks Linux sender credentials"
    install uname=Linux managed wrong_pid --link-only --now
    assert_failed
    assert_in "$ERR" "identified GetHealthStatus"
}

test_invalid_startup_identity_cannot_report_readiness() {
    install bad_start --link-only --now
    assert_failed
    assert_in "$ERR" "identified GetHealthStatus"
}

# The three darwin identity cases ran the macOS health-client branch on Linux
# through a Python shim that faked sys.platform and LOCAL_PEERPID. Without
# Python they run the real branch, natively on macOS.

test_darwin_identity_branch_uses_the_daemon_reply_on_an_inherited_socket() {
    darwin_only "$NATIVE_DARWIN_ONLY"
    # The fake daemon's reply names its own PID, inside the launcher's tree.
    install uname=Darwin --link-only --now
    assert_ok
    assert_in "$OUT" "rsid healthy"
}

test_darwin_legacy_daemon_is_identified_by_local_peerpid() {
    # #1875: a daemon that predates daemon_pid in GetHealthStatus is identified
    # on macOS by LOCAL_PEERPID alone; it must be accepted when that PID is
    # inside the launcher's process tree. The reply names no PID, so only the
    # real LOCAL_PEERPID can have identified it.
    darwin_only "$NATIVE_DARWIN_ONLY"
    install uname=Darwin legacy_health --link-only --now
    assert_ok
    assert_in "$OUT" "rsid healthy"
    assert_in "$STATE/rpc" "GetHealthStatus"
}

test_darwin_legacy_daemon_outside_the_launcher_tree_is_refused() {
    # A legacy daemon outside the launcher's tree holds the socket and answers;
    # its real LOCAL_PEERPID is not in the tree, so it is refused.
    darwin_only "$NATIVE_DARWIN_ONLY"
    start_foreign_daemon FAKE_LEGACY_HEALTH=1
    install uname=Darwin holder_idle --link-only --now
    assert_failed
    assert_in "$STATE/rpc" "GetHealthStatus"
    assert_in "$ERR" "NO DAEMON IS RUNNING from this launch"
}

test_answering_process_that_exits_right_after_replying_is_named() {
    # #1875: the daemon replied and then died. The message must say so, not
    # that the launcher exited. A real exit races the probe's next `ps`, so a
    # fake `ps` reports the fixture daemon gone, deterministically.
    write_executable "$FAKES/ps" <<'EOF'
#!/bin/sh
previous=""
for arg in "$@"; do
  [ "$previous" = -p ] && [ -f "$FAKE_STATE/daemon-$arg" ] && exit 1
  previous="$arg"
done
PATH="${PATH#*:}" exec ps "$@"
EOF
    install uname=Darwin --link-only --now
    assert_failed
    assert_in "$ERR" "process that answered GetHealthStatus exited right after replying"
    assert_not_in "$ERR" "launcher exited"
}

launchd_plist_path() { echo "$HOME_DIR/Library/LaunchAgents/dev.rsi.rsid.plist"; }

test_darwin_starts_rsid_as_the_launchd_agent() {
    # #2936: macOS runs the holder chain as the user agent dev.rsi.rsid.
    install uname=Darwin --link-only --now
    assert_ok
    local uid plist service install_dir args keys entry
    uid="$(id -u)" plist="$(launchd_plist_path)" install_dir="$HOME_DIR/.rsi/install"
    service="gui/$uid/dev.rsi.rsid"
    assert_in "$OUT" "rsid restarted as launchd agent $service"
    assert_in "$OUT" "rsid healthy"
    assert_line "$STATE/launchctl" "bootstrap gui/$uid $plist"
    assert_eq "$(plist_field "$plist" Label)" dev.rsi.rsid Label
    args="$(plist_field "$plist" ProgramArguments | tr '\n' '|')"
    assert_eq "$args" "$install_dir/rsi-socket-hold|$SOCKET|--|$install_dir/rsid-supervisor.sh|$install_dir/rsid|" ProgramArguments
    assert_eq "$(plist_field "$plist" RunAtLoad)" true RunAtLoad
    assert_eq "$(plist_field "$plist" KeepAlive)" true KeepAlive
    assert_eq "$(plist_field "$plist" StandardOutPath)" "$HOME_DIR/.rsi/daemon.log" StandardOutPath
    assert_eq "$(plist_field "$plist" StandardErrorPath)" "$HOME_DIR/.rsi/daemon.log" StandardErrorPath
    assert_eq "$(plist_field "$plist" WorkingDirectory)" "$HOME_DIR" WorkingDirectory
    # Only the launch environment: provider secrets come from ~/.rsi/.env.
    keys="$(plist_field "$plist" EnvironmentVariables | sed 's/=.*//' | LC_ALL=C sort | tr '\n' ' ')"
    assert_eq "$keys" "HOME LANG PATH RSI_DAEMON_SOCKET_PATH RSI_SOCKET " "EnvironmentVariables keys"
    assert_eq "$(plist_field "$plist" EnvironmentVariables | sed -n 's/^HOME=//p')" "$HOME_DIR" "agent HOME"
    assert_eq "$(plist_field "$plist" EnvironmentVariables | sed -n 's/^RSI_DAEMON_SOCKET_PATH=//p')" "$SOCKET" "agent RSI_DAEMON_SOCKET_PATH"
    local -a entries=()
    IFS=: read -r -a entries <<<"$(plist_field "$plist" EnvironmentVariables | sed -n 's/^PATH=//p')"
    assert_eq "${entries[0]:-}:${entries[1]:-}" "$HOME_DIR/.local/bin:$HOME_DIR/.cargo/bin" "first agent PATH entries"
    for entry in /usr/bin /bin /usr/sbin /sbin; do
        [[ "$(index_of "$entry" "${entries[@]}")" -ge 0 ]] || fail "agent PATH lacks $entry"
    done
    # The agent's holder is the launcher the health probe identified.
    assert_in "$STATE/started" "$install_dir/rsid-supervisor.sh"
}

test_darwin_agent_path_keeps_the_newest_nvm_node_and_drops_temporary_entries() {
    local version newest
    for version in v9.11.2 v24.22.0; do
        mkdir -p "$HOME_DIR/.nvm/versions/node/$version/bin"
    done
    install uname=Darwin --link-only --now
    assert_ok
    local -a entries=()
    IFS=: read -r -a entries <<<"$(plist_field "$(launchd_plist_path)" EnvironmentVariables | sed -n 's/^PATH=//p')"
    newest="$HOME_DIR/.nvm/versions/node/v24.22.0/bin"
    [[ "$(index_of "$newest" "${entries[@]}")" -ge 0 ]] || fail "agent PATH lacks $newest"
    [[ "$(index_of "$newest" "${entries[@]}")" -lt "$(index_of /usr/bin "${entries[@]}")" ]] \
        || fail "the newest node is not before /usr/bin"
    # The caller's temporary fakes directory does not outlive this run.
    assert_eq "$(index_of "$FAKES" "${entries[@]}")" -1 "fakes directory in the agent PATH"
}

test_darwin_restart_boots_the_loaded_agent_out_and_loads_it_again() {
    install uname=Darwin --link-only --now
    assert_ok "first install"
    local first_holder service bootout bootstraps
    first_holder="$(holder_pid)"
    install uname=Darwin --link-only --now
    assert_ok "second install"
    assert_in "$OUT" "Stopping launchd agent"
    assert_in "$OUT" "rsid healthy"
    service="gui/$(id -u)/dev.rsi.rsid"
    bootout="$(grep -n -x -F "bootout $service" "$STATE/launchctl" | head -n 1 | cut -d: -f1)"
    bootstraps="$(grep -n '^bootstrap ' "$STATE/launchctl" | cut -d: -f1 | tr '\n' ' ')"
    [[ -n "$bootout" ]] || fail "no 'bootout $service' call"
    set -- $bootstraps
    assert_eq "$#" 2 "bootstrap calls"
    [[ "$bootout" -lt "$2" ]] || fail "bootout did not precede the second bootstrap"
    [[ "$(holder_pid)" != "$first_holder" ]] || fail "the holder was not replaced"
}

test_darwin_no_restart_refreshes_the_agent_plist_without_loading_it() {
    install uname=Darwin --link-only --no-restart
    assert_ok
    assert_eq "$(plist_field "$(launchd_plist_path)" Label)" dev.rsi.rsid Label
    assert_in "$OUT" "launchd agent"
    assert_missing "$STATE/started"
}

test_darwin_opt_out_keeps_the_detached_holder_chain() {
    install uname=Darwin env:RSI_LAUNCHD_AGENT=0 --link-only --now
    assert_ok
    assert_in "$OUT" "rsid restarted on macOS"
    assert_exists "$STATE/started"
    assert_missing "$(launchd_plist_path)"
}

test_managed_service_rejects_a_live_foreign_responder() {
    linux_only
    start_foreign_daemon ""
    install uname=Linux managed holder_idle --link-only --now
    assert_failed
    assert_in "$ERR" "identified GetHealthStatus"
}

test_inactive_launcher_unit_cannot_report_success() {
    linux_only
    install uname=Linux unit_state=inactive --link-only --now
    assert_failed
    assert_in "$ERR" "exited before rsid became healthy"
}

test_cleanup_stops_a_daemon_that_has_not_published_its_pid() {
    # The installer returns while the fake daemon is held before its PID
    # marker (the late-start race of #232). Cleanup must still stop and reap
    # it, so nothing can write into the temp directory afterwards.
    linux_only
    local gate="$STATE/gate" deadline
    mkfifo "$gate"
    install uname=Linux unit_state=inactive gate="$gate" --link-only --now
    assert_failed
    assert_in "$ERR" "exited before rsid became healthy"
    # Opening the write end completes only once the daemon is a reader, i.e.
    # it is alive and blocked before writing daemon-<pid>. The writer keeps it
    # open, so the daemon stays blocked.
    (exec 3>"$gate" && : >"$STATE/gate-opened" && exec sleep 30) </dev/null >/dev/null 2>&1 5<&- 6>&- &
    OWNED="$OWNED $!"
    deadline=$((SECONDS + 5))
    until [[ -e "$STATE/gate-opened" ]]; do
        [[ "$SECONDS" -lt "$deadline" ]] || fail "no daemon ever read the gate"
        sleep 0.005
    done
    [[ -z "$(ls "$STATE" | grep '^daemon-' || true)" ]] || fail "the held daemon published its PID"
    stop_launchers || fail "cleanup did not stop the held daemon"
    # With the daemon gone nothing still reads the gate: a new writer stays blocked.
    (exec 3>"$gate" && : >"$STATE/gate-reopened") </dev/null >/dev/null 2>&1 5<&- 6>&- &
    OWNED="$OWNED $!"
    sleep 0.3
    assert_missing "$STATE/gate-reopened"
    [[ -z "$(ls "$STATE" | grep '^daemon-' || true)" ]] || fail "a daemon published its PID after cleanup"
}

assert_restart_ignores_an_rsid_of_another_home() {
    # #3476: the restart path found daemons with a host-wide pgrep and sent
    # SIGINT to every rsid of the user, including the hub's. This runs the real
    # restart path (--now) with a live process named rsid that serves a socket
    # under another RSI home and that ignores signals.
    rsid_stand_in other ignore
    local decoy="$STAND_IN_PID" log="$STAND_IN_LOG"
    write_pgrep "$decoy"
    install uname="$1" --link-only --now
    assert_ok
    # Nothing was stopped, so nothing was waited on: a wait would last 90 s.
    assert_not_in "$OUT" "was running as PID"
    assert_not_in "$ERR" "did not stop"
    assert_in "$OUT" "starting it"
    assert_untouched "$decoy" "$log"
}

test_linux_restart_never_signals_or_waits_on_an_rsid_of_another_home() {
    assert_restart_ignores_an_rsid_of_another_home Linux
}

test_launchd_restart_never_signals_or_waits_on_an_rsid_of_another_home() {
    assert_restart_ignores_an_rsid_of_another_home Darwin
}

test_restart_stops_only_the_daemon_that_serves_this_installs_socket() {
    rsid_stand_in in_scope exit
    local ours="$STAND_IN_PID" ours_log="$STAND_IN_LOG"
    rsid_stand_in other ignore
    local other="$STAND_IN_PID" other_log="$STAND_IN_LOG"
    write_pgrep "$ours" "$other"
    install uname=Linux --link-only --now
    assert_ok
    assert_in "$OUT" "was running as PID(s): $ours "
    assert_stopped_by_sigint "$ours" "$ours_log"
    assert_untouched "$other" "$other_log"
}

# ---------------------------------------------------------------- RsidStartSatelliteTest
# #2820: `scripts/rsid-start-satellite` starts the deployed daemon on a freshly
# booted satellite and persists the worker slice limits. The deployed set is
# populated by a real `install-release.sh --link-only --no-restart`.

deploy() {
    install uname=Linux --link-only --no-restart
    assert_ok "deploy"
    assert_exists "$HOME_DIR/.rsi/install/rsid-supervisor.sh"
}

start() { install script=rsid-start-satellite uname=Linux "$@"; }

test_starts_the_deployed_daemon_in_a_detached_unit_not_a_scope() {
    deploy
    start
    assert_ok
    assert_in "$OUT" "rsid started from"
    local install_dir="$HOME_DIR/.rsi/install"
    assert_in "$STATE/systemd-run" "--collect"
    assert_not_in "$STATE/systemd-run" "--scope"
    assert_in "$STATE/systemd-run" "$install_dir/rsid-supervisor.sh $install_dir/rsid"
    assert_not_in "$STATE/systemd-run" "$ROOT"
    assert_in "$STATE/started" "daemon.sock"
    assert_in "$STATE/started" "$install_dir/rsid"
    assert_in "$STATE/rpc" "GetHealthStatus"
}

test_persists_the_worker_slice_limits_from_the_scope_env() {
    deploy
    printf '%s\n' rsid_scope_memory_high_mib=6144 rsid_scope_memory_max_mib=8192 \
        rsid_scope_memory_swap_max_mib=0 rsid_scope_cpu_weight=20 \
        worker_scope_memory_high_mib=6144 worker_scope_memory_max_mib=8192 \
        worker_scope_memory_swap_max_mib=0 worker_scope_cpu_weight=20 >"$HOME_DIR/.rsi/rsid-scope.env"
    start
    assert_ok
    assert_line "$STATE/systemctl" "--user set-property rsi-workers.slice MemoryHigh=6144M MemoryMax=8192M MemorySwapMax=0M CPUWeight=20"
    assert_line "$STATE/systemctl" "--user set-property --runtime rsi-workers.slice MemoryHigh=6144M MemoryMax=8192M MemorySwapMax=0M CPUWeight=20"
    # Applied to the slice before the daemon unit is launched.
    assert_exists "$STATE/systemd-run"
}

# The --setenv=PATH= value of the daemon unit, split on ':' into PATH_ENTRIES.
unit_path_entries() {
    local word count=0 value=""
    local -a words=()
    read -r -a words <"$STATE/systemd-run" || true
    for word in "${words[@]}"; do
        case "$word" in
            --setenv=PATH=*) count=$((count + 1)) value="${word#--setenv=PATH=}" ;;
        esac
    done
    assert_eq "$count" 1 "--setenv=PATH= words"
    PATH_ENTRIES=()
    IFS=: read -r -a PATH_ENTRIES <<<"$value"
}

test_the_daemon_unit_carries_cargo_bin_on_path() {
    # #2823: the unit builds with the pinned toolchain, not the system rustc.
    deploy
    start
    assert_ok
    unit_path_entries
    local cargo="$HOME_DIR/.cargo/bin"
    [[ "$(index_of "$cargo" "${PATH_ENTRIES[@]}")" -ge 0 ]] || fail "unit PATH lacks $cargo"
    [[ "$(index_of "$HOME_DIR/.local/bin" "${PATH_ENTRIES[@]}")" -ge 0 ]] || fail "unit PATH lacks ~/.local/bin"
    # The caller's own entries stay, after the prepended ones.
    [[ "$(index_of "$FAKES" "${PATH_ENTRIES[@]}")" -ge 0 ]] || fail "unit PATH lacks the caller's $FAKES"
    [[ "$(index_of "$cargo" "${PATH_ENTRIES[@]}")" -lt "$(index_of "$FAKES" "${PATH_ENTRIES[@]}")" ]] \
        || fail "~/.cargo/bin is not before the caller's entries"
}

test_an_existing_cargo_bin_entry_is_not_duplicated() {
    deploy
    local cargo="$HOME_DIR/.cargo/bin" entry count=0
    start "env:PATH=$cargo:$FAKES:$PATH"
    assert_ok
    unit_path_entries
    for entry in "${PATH_ENTRIES[@]}"; do
        [[ "$entry" != "$cargo" ]] || count=$((count + 1))
    done
    assert_eq "$count" 1 "~/.cargo/bin entries in the unit PATH"
}

test_a_running_daemon_is_left_alone() {
    deploy
    start running
    assert_ok
    assert_in "$OUT" "already running"
    assert_missing "$STATE/systemd-run"
    assert_missing "$STATE/started"
}

test_managed_service_is_started_after_the_slice_is_persisted() {
    deploy
    start managed
    assert_ok
    assert_in "$OUT" "rsid.service started"
    local persist begin
    persist="$(grep -n '^--user set-property rsi-workers.slice' "$STATE/systemctl" | head -n 1 | cut -d: -f1)"
    begin="$(grep -n -x -F -- '--user start rsid.service' "$STATE/systemctl" | head -n 1 | cut -d: -f1)"
    [[ -n "$persist" && -n "$begin" ]] || fail "missing the slice persist or the rsid.service start"
    [[ "$persist" -lt "$begin" ]] || fail "rsid.service started before the slice was persisted"
}

test_unhealthy_start_exits_nonzero() {
    deploy
    start never_healthy
    assert_failed
    assert_in "$ERR" "NO DAEMON IS RUNNING"
}

test_refuses_when_nothing_is_deployed() {
    start
    assert_failed
    assert_in "$ERR" "Not deployed"
    assert_missing "$STATE/systemd-run"
}

test_refuses_an_unsupported_platform() {
    install script=rsid-start-satellite uname=FreeBSD
    assert_failed
    assert_in "$ERR" "supports Linux (systemd user units) and macOS"
}

test_macos_starts_the_deployed_daemon_as_the_launchd_agent() {
    # #2936: on macOS the deployed chain is loaded as dev.rsi.rsid.
    deploy
    install script=rsid-start-satellite uname=Darwin
    assert_ok
    local uid install_dir="$HOME_DIR/.rsi/install"
    uid="$(id -u)"
    assert_in "$OUT" "as launchd agent gui/$uid/dev.rsi.rsid"
    assert_line "$STATE/launchctl" "bootstrap gui/$uid $HOME_DIR/Library/LaunchAgents/dev.rsi.rsid.plist"
    assert_in "$STATE/started" "$install_dir/rsid-supervisor.sh $install_dir/rsid"
    assert_in "$STATE/rpc" "GetHealthStatus"
}

test_macos_a_loaded_agent_with_no_process_is_kickstarted() {
    deploy
    install script=rsid-start-satellite uname=Darwin
    assert_ok "first start"
    # The holder chain ends while launchd keeps the job loaded.
    local holder deadline
    holder="$(holder_pid)"
    kill -TERM "$holder"
    deadline=$((SECONDS + 5))
    while running "$holder"; do
        [[ "$SECONDS" -lt "$deadline" ]] || fail "fake holder did not stop"
        sleep 0.02
    done
    install script=rsid-start-satellite uname=Darwin
    assert_ok "second start"
    assert_line "$STATE/launchctl" "kickstart gui/$(id -u)/dev.rsi.rsid"
    [[ "$(holder_pid)" != "$holder" ]] || fail "the holder was not restarted"
    assert_in "$OUT" "rsid healthy"
}

# ---------------------------------------------------------------- repo guard (#451)

# The lander and the queue probe run `rsi-repo-guard` installed beside their own
# executable and fail closed without it, so the installer ships it in the same
# directory as rsi-rolling-land and links it next to the lander's link.
test_link_only_install_ships_the_repo_guard_beside_the_lander() {
    local name install_dir="$HOME_DIR/.rsi/install" bin_dir="$HOME_DIR/.local/bin"
    for name in rsi-rolling-land rsi-repo-guard; do
        printf '#!/bin/sh\necho "fake-%s $*"\n' "$name" | write_executable "$TARGET/release/$name"
    done
    install --link-only --no-restart
    assert_ok
    for name in rsi-rolling-land rsi-repo-guard; do
        [[ -x "$install_dir/$name" && ! -L "$install_dir/$name" ]] \
            || fail "$name is not an installed executable in $install_dir"
        assert_eq "$(readlink "$bin_dir/$name")" "$install_dir/$name" "$name link"
    done
    assert_in "$OUT" "$bin_dir/rsi-repo-guard -> $install_dir/rsi-repo-guard"
    assert_not_in "$ERR" "rsi-repo-guard not built"
}

test_link_only_install_without_a_repo_guard_warns_and_installs_the_rest() {
    printf '#!/bin/sh\necho fake-lander\n' | write_executable "$TARGET/release/rsi-rolling-land"
    install --link-only --no-restart
    assert_ok
    assert_exists "$HOME_DIR/.rsi/install/rsi-rolling-land"
    assert_missing "$HOME_DIR/.rsi/install/rsi-repo-guard"
    assert_missing "$HOME_DIR/.local/bin/rsi-repo-guard"
    assert_in "$ERR" "rsi-repo-guard not built"
    assert_in "$ERR" "lander refuses landings that touch protected migration paths"
}

# The staged tree (--stage-dir) and the quiet-point deploy both copy through
# install_built_binaries; it must carry the guard as a top-level member.
test_install_built_binaries_copies_the_repo_guard_next_to_the_lander() {
    local name release="$BASE/release-out" stage="$BASE/stage-out"
    for name in rsid rsi-rolling-land rsi-repo-guard; do
        printf '#!/bin/sh\necho "built-%s"\n' "$name" | write_executable "$release/$name"
    done
    (
        # shellcheck source=/dev/null
        source "$ROOT/scripts/rsid-quiet-restart.sh"
        install_built_binaries "$release" "$stage"
    )
    for name in rsid rsi-rolling-land rsi-repo-guard; do
        [[ -x "$stage/$name" ]] || fail "install_built_binaries did not copy $name"
        assert_eq "$("$stage/$name")" "built-$name" "$name content"
    done
}

# The guard must never share a cargo invocation with rsid: crates/rsi-repo-guards
# enables serde_json/preserve_order and Cargo unifies features across one build.
test_the_installer_builds_the_repo_guard_in_its_own_cargo_invocation() {
    local script="$ROOT/scripts/install-release.sh" builds daemon guard
    builds="$(awk '/^build_release_binaries\(\) \{/{f=1} f&&/^}/{exit} f&&/^    cargo build/{print}' "$script")"
    [[ "$(printf '%s\n' "$builds" | grep -c .)" -eq 2 ]] || fail "expected two cargo build lines, got: $builds"
    daemon="$(printf '%s\n' "$builds" | grep -- '--bin rsid ')"
    guard="$(printf '%s\n' "$builds" | grep -v -- '--bin rsid ')"
    case "$daemon" in *repo-guard*) fail "the rsid build line names the repo guard: $daemon" ;; esac
    case "$daemon" in *"--bin rsi-rolling-land"*) ;; *) fail "the lander left the rsid build line: $daemon" ;; esac
    case "$guard" in *"-p rsi-repo-guards --bin rsi-repo-guard"*) ;; *) fail "guard build line: $guard" ;; esac
    case "$guard" in *"--bin rsid "* | *--workspace*) fail "guard build line shares a build: $guard" ;; esac
}

# ---------------------------------------------------------------- runner

CASES="
test_now_link_only_starts_a_down_daemon_and_waits_for_health
test_no_daemon_after_start_exits_nonzero_with_a_clear_message
test_every_direct_sigint_of_the_daemon_is_announced_first
test_no_restart_still_skips_the_start
test_linux_starts_a_down_daemon_in_a_collected_unit
test_linux_unhealthy_start_exits_nonzero
test_foreign_daemon_cannot_mask_an_exited_holder
test_foreign_daemon_cannot_mask_a_live_unrelated_holder
test_managed_service_identifies_its_daemon
test_legacy_health_keeps_kernel_identity_fallback
test_claimed_holder_pid_cannot_replace_replying_daemon_identity
test_invalid_startup_identity_cannot_report_readiness
test_darwin_identity_branch_uses_the_daemon_reply_on_an_inherited_socket
test_darwin_legacy_daemon_is_identified_by_local_peerpid
test_darwin_legacy_daemon_outside_the_launcher_tree_is_refused
test_answering_process_that_exits_right_after_replying_is_named
test_darwin_starts_rsid_as_the_launchd_agent
test_darwin_agent_path_keeps_the_newest_nvm_node_and_drops_temporary_entries
test_darwin_restart_boots_the_loaded_agent_out_and_loads_it_again
test_darwin_no_restart_refreshes_the_agent_plist_without_loading_it
test_darwin_opt_out_keeps_the_detached_holder_chain
test_managed_service_rejects_a_live_foreign_responder
test_inactive_launcher_unit_cannot_report_success
test_cleanup_stops_a_daemon_that_has_not_published_its_pid
test_linux_restart_never_signals_or_waits_on_an_rsid_of_another_home
test_launchd_restart_never_signals_or_waits_on_an_rsid_of_another_home
test_restart_stops_only_the_daemon_that_serves_this_installs_socket
test_starts_the_deployed_daemon_in_a_detached_unit_not_a_scope
test_persists_the_worker_slice_limits_from_the_scope_env
test_the_daemon_unit_carries_cargo_bin_on_path
test_an_existing_cargo_bin_entry_is_not_duplicated
test_a_running_daemon_is_left_alone
test_managed_service_is_started_after_the_slice_is_persisted
test_unhealthy_start_exits_nonzero
test_refuses_when_nothing_is_deployed
test_refuses_an_unsupported_platform
test_macos_starts_the_deployed_daemon_as_the_launchd_agent
test_macos_a_loaded_agent_with_no_process_is_kickstarted
test_link_only_install_ships_the_repo_guard_beside_the_lander
test_link_only_install_without_a_repo_guard_warns_and_installs_the_rest
test_install_built_binaries_copies_the_repo_guard_next_to_the_lander
test_the_installer_builds_the_repo_guard_in_its_own_cargo_invocation
"

# `test_install_release_start.sh [CASE ...]` runs the named cases only.
selected="${*:-$CASES}"
passed=0 skipped=0 failed=""
for CASE in $selected; do
    declare -F "$CASE" >/dev/null || { echo "unknown case: $CASE" >&2; exit 2; }
    set +e
    (
        set -euo pipefail
        trap cleanup_case EXIT
        setup_fixture
        "$CASE"
    )
    status=$?
    set -e
    case "$status" in
        0) passed=$((passed + 1)); echo "ok $CASE" ;;
        77) skipped=$((skipped + 1)) ;;
        *) failed="$failed $CASE" ;;
    esac
done
echo "install-release start: $passed passed, $skipped skipped${failed:+, failed:$failed}"
[[ -z "$failed" ]]
