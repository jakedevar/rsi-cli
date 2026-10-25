#!/usr/bin/env bash
# suite-gate: tap
# #1014, #255, #1854, #2430, #2773, #2778, #938, #465: `scripts/cargo-slot`, the resource
# governor client: admission deadlines and recovery against a real Unix-socket governor stub,
# the local FIFO ticket fallback and its disk/load/memory gates, the `check` class, the idle
# target-pool reclaim, `--status`, and the lander's TMPDIR and CARGO_TARGET_DIR. Port of the
# retired Python suite scripts/tests/test_cargo_slot.py (#366, no Python #358). The governor
# client, the probes and the status listing are `rsi-repo-guard cargo-slot ...` (#423, #465),
# found through RSI_REPO_GUARD_BIN, built once for the suite.
#
# Case map (legacy test -> case). Cases marked [linux] drive the local fallback (flock(1),
# /proc/loadavg, /proc/meminfo, GNU df) or /dev/shm, are SKIPPED on other hosts with the reason
# named (hub #3621 Linux checklist) and were NOT executed on the macOS host that ported them;
# every other case runs on every host through the governor stub.
#   test_local_gates_stop_admission_and_name_the_phase (4 sub-cases)
#       -> "[linux] the local load gate stops admission and names the phase" (and "... memory gate ...",
#          "... disk gate ...", "... slot gate ...")
#   test_local_slot_tickets_are_served_in_fifo_order  -> "[linux] local slot tickets are served in FIFO order"
#   test_local_queue_removes_a_dead_stale_pid_ticket  -> "[linux] the local queue removes a dead stale-pid ticket"
#   test_older_live_ticket_blocks_a_free_slot_until_it_goes_away
#       -> "[linux] an older live ticket blocks a free slot until it goes away"
#   test_higher_priority_ticket_goes_ahead_of_an_older_normal_ticket
#       -> "[linux] a higher-priority ticket goes ahead of an older normal ticket"
#   test_local_disk_guard_wait_keeps_a_ticket_and_cleans_it_on_timeout
#       -> "[linux] a local disk-guard wait keeps a ticket and cleans it on timeout"
#   test_local_disk_floor_compares_bytes_and_reports_the_rounded_boundary
#       -> "[linux] the local disk floor compares bytes and reports the rounded boundary"
#   test_local_status_lists_holders_and_the_ordered_queue_without_stale_tickets
#       -> "local status lists holders and the ordered queue without stale tickets"
#   test_cargo_check_uses_its_own_local_pool_and_ignores_the_load_gate
#       -> "[linux] cargo check uses its own local pool and ignores the load gate"
#   test_cargo_check_asks_the_governor_for_the_check_class
#       -> "cargo check asks the governor for the check class"
#   test_a_worker_waiting_on_the_disk_floor_reclaims_the_idle_pool_once
#       -> "a worker waiting on the disk floor reclaims the idle pool once"
#   test_other_waits_and_pool_builds_do_not_reclaim_the_pool
#       -> "other waits do not reclaim the pool"
#   test_the_local_disk_wait_reclaims_once_and_a_pool_build_never_does
#       -> "[linux] the local disk wait reclaims once and a pool build never does"
#   test_status_lists_current_governor_holders_and_queue_order
#       -> "status lists current governor holders and queue order"
#   test_status_shows_live_local_holders_and_waiters_beside_a_reachable_daemon
#       -> "status shows live local holders and waiters beside a reachable daemon"
#   test_status_lists_daemon_state_first_then_the_labelled_local_state
#       -> "status lists daemon state first, then the labelled local state"
#   test_forced_local_status_reports_the_local_backend_and_skips_the_daemon
#       -> "forced local status reports the local backend and skips the daemon"
#   test_memory_wait_survives_a_transient_governor_timeout_with_the_same_ticket
#       -> "a memory wait survives a transient governor timeout with the same ticket"
#   test_governor_queue_is_bounded_and_cancels_its_ticket
#       -> "the governor queue is bounded and cancels its ticket"
#   test_budgeted_caller_can_extend_the_interactive_admission_limit
#       -> "a budgeted caller can extend the interactive admission limit"
#   test_unavailable_governor_records_the_error_before_local_admission_timeout
#       -> "[linux] an unavailable governor records the error before the local admission timeout"
#   test_cargo_target_dir_reaches_the_command_unchanged
#       -> "CARGO_TARGET_DIR reaches the command unchanged" (through the governor stub, not the local
#          fallback: the environment handoff is shared by both paths)
#   test_lander_tmpdir_goes_to_tmpfs_only_with_room_for_this_user
#       -> "[linux] the lander TMPDIR goes to tmpfs only with room for this user"
#   test_lander_tmpdir_counts_the_user_quota_not_just_free_space
#       -> "[linux] the lander TMPDIR counts the user quota, not just free space" (the quota probe is
#          `cargo-slot free-gb`; the case checks the script follows it)
# Added with the port (#465), negative proofs for the helper-binary lookup that replaced the
# interpreter: "cargo-slot without rsi-repo-guard exits 127 and says so" (nothing is admitted or
# run) and "cargo-slot --status names the missing governor, never reads as healthy". Also: the
# child runs without ionice on a host that has none ("the command runs where ionice is absent").
# Added with #3893: the trusted-guard order and probe ("a stale guard is skipped and named ...",
# "the installed guard wins over a sandbox build", "when every guard is stale ... exits 127"), a
# failing governor client ("a failing governor client is visible and retried ...") and the
# fallback's re-ask ("[linux] a persistent client failure falls back, then the re-ask ...").
source "$(dirname "$0")/lib/tap.sh"

SCRIPT="$SCRIPTS_DIR/cargo-slot"
HOST_OS="$(uname -s)"

if ! RSI_REPO_GUARD_BIN="$("$SCRIPTS_DIR/repo-guard" --bin)"; then
    echo "Bail out! scripts/repo-guard could not build rsi-repo-guard" >&2
    exit 1
fi
export RSI_REPO_GUARD_BIN

# The governor stub: a Perl Unix-socket server (no Python, #358). One request per connection.
# Scenarios: grant | queue | queue-then-grant | disk | memory-stall | snapshot. Every request line
# is appended to the log file; STUB_MESSAGE, STUB_TICKET and STUB_SNAPSHOT shape the replies.
GOVERNOR_STUB='
use strict; use warnings;
use IO::Socket::UNIX; use JSON::PP;
$SIG{PIPE} = "IGNORE"; $| = 1;
my ($path, $scenario, $log) = @ARGV;
my $ticket = $ENV{STUB_TICKET} // "00000000-0000-0000-0000-000000000001";
my $message = $ENV{STUB_MESSAGE} // "all 4 build slots busy";
unlink $path;
my $server = IO::Socket::UNIX->new(Local => $path, Listen => 16) or die "bind $path: $!";
my $acquires = 0;
while (my $client = $server->accept) {
    my $line = <$client>;
    next unless defined $line;
    open my $fh, ">>", $log or die; print $fh $line; close $fh;
    my $request = eval { decode_json($line) } or next;
    my $result;
    if ($scenario eq "snapshot") {
        $result = decode_json($ENV{STUB_SNAPSHOT});
    } elsif ($request->{method} eq "ReleaseAdmission") {
        $result = { released => JSON::PP::true };
    } else {
        $acquires++;
        my $queued = { status => "queued", ticket_id => $ticket, position => 2, message => $message };
        my $granted = { status => "granted", lease_id => $ticket };
        if ($scenario eq "grant") { $result = $granted }
        elsif ($scenario eq "queue") { $result = $queued }
        elsif ($scenario eq "queue-then-grant") { $result = $acquires == 1 ? $queued : $granted }
        elsif ($scenario eq "disk") { $result = $acquires <= 3 ? $queued : $granted }
        elsif ($scenario eq "memory-stall") {
            if ($acquires == 1) { $result = $queued }
            else { select(undef, undef, undef, 5.2) if $acquires == 2; $result = $granted }
        }
    }
    print $client encode_json({ jsonrpc => "2.0", id => 1, result => $result }), "\n";
    close $client;
}
'

# fixture: BASE, HOME_DIR, the environment cargo-slot runs under (the local gates open wide, a
# one-second admission budget, the local fallback forced) and the bookkeeping for stubs.
fixture() {
    BASE="$(cd -- "$CASE_DIR" && pwd -P)/base"
    HOME_DIR="$BASE/home"
    mkdir -p "$HOME_DIR/.rsi"
    BG_PIDS=""
    SOCK_DIR=""
    ENV_TOKENS="HOME=$HOME_DIR
RSI_CARGO_SLOT_POLL_SEC=1
RSI_CARGO_MIN_FREE_GB=0
RSI_CARGO_MAX_LOAD=1000000
RSI_CARGO_MIN_AVAIL_GB=0
RSI_CARGO_MAX_SLICE_GB=1000000
RSI_CARGO_ADMISSION_MAX_SEC=1
RSI_CARGO_SLOT_LOCAL=1"
    # shellcheck disable=SC2064
    trap 'cleanup_case' EXIT
}

cleanup_case() {
    local pid
    for pid in $BG_PIDS; do
        kill "$pid" 2>/dev/null || true
    done
    [[ -z "$SOCK_DIR" ]] || rm -rf "$SOCK_DIR"
}

# set_env NAME=value...: change the environment of later invocations (a bare `-NAME` unsets).
set_env() {
    local pair name new
    for pair in "$@"; do
        case "$pair" in
            -*) name="${pair#-}"; new="-u $name" ;;
            *) name="${pair%%=*}"; new="$pair" ;;
        esac
        ENV_TOKENS="$(printf '%s\n' "$ENV_TOKENS" | grep -v -e "^$name=" -e "^-u $name\$" || true)
$new"
    done
}

# env_args: the tokens as one word-split line, `-u NAME` words first (env stops option parsing at
# the first assignment); no value holds a space.
env_args() { { printf '%s\n' "$ENV_TOKENS" | grep '^-u ' || true; printf '%s\n' "$ENV_TOKENS" | grep -v -e '^-u ' -e '^$' || true; } | tr '\n' ' '; }

# invoke <command...>: cargo-slot with that command under the fixture environment; sets RUN_CODE/RUN_OUT/RUN_ERR.
invoke() {
    # shellcheck disable=SC2086
    run_capture env $(env_args) bash "${SLOT_SCRIPT:-$SCRIPT}" "$@"
}

# slot_default: the stand-in command of the legacy suite (`/bin/sh -c "printf command-ran"`).
invoke_default() { invoke /bin/sh -c "printf command-ran"; }

# start_governor <scenario> [NAME=value ...]: a stub governor on a fresh socket; later invocations
# talk to it (the local fallback is switched off).
start_governor() {
    command -v perl >/dev/null 2>&1 || tap_fail "perl is needed for the governor stub"
    local scenario="$1" pair
    shift
    SOCK_DIR="$(mktemp -d /tmp/cgs.XXXXXX)" || tap_fail "no socket directory"
    GOV_SOCK="$SOCK_DIR/governor.sock"
    GOV_LOG="$SOCK_DIR/requests.log"
    : >"$GOV_LOG"
    printf '%s' "$GOVERNOR_STUB" >"$SOCK_DIR/stub.pl"
    (
        for pair in "$@"; do export "${pair?}"; done
        exec perl "$SOCK_DIR/stub.pl" "$GOV_SOCK" "$scenario" "$GOV_LOG" >/dev/null 2>&1
    ) &
    BG_PIDS="$BG_PIDS $!"
    disown "$!" 2>/dev/null || true
    local tries=0
    until [[ -S "$GOV_SOCK" ]]; do
        tries=$((tries + 1))
        [[ "$tries" -le 100 ]] || tap_fail "the governor stub never bound its socket"
        sleep 0.05
    done
    set_env -RSI_CARGO_SLOT_LOCAL "RSI_DAEMON_SOCKET_PATH=$GOV_SOCK"
}

# requests: the logged request lines; methods: their method names, in order.
methods() { sed -n 's/.*"method":"\([A-Za-z]*\)".*/\1/p' "$GOV_LOG"; }
acquire_classes() { grep '"AcquireAdmission"' "$GOV_LOG" | sed -n 's/.*"class":"\([a-z]*\)".*/\1/p'; }
last_request() { tail -n 1 "$GOV_LOG"; }

# wait_for <description> <command...>: poll up to five seconds.
wait_for() {
    local what="$1" tries=0
    shift
    until "$@"; do
        tries=$((tries + 1))
        [[ "$tries" -le 250 ]] || tap_fail "timed out waiting for $what"
        sleep 0.02
    done
}

# tickets_are <dir> <count>: the queue directory holds exactly that many tickets.
tickets_are() { [[ "$(ls "$1" 2>/dev/null | wc -l | tr -d ' ')" == "$2" ]]; }

# dead_pid: a pid that is not running (a child that already exited and was reaped).
dead_pid() {
    sh -c 'exit 0' &
    local pid=$!
    wait "$pid" 2>/dev/null
    printf '%s' "$pid"
}

# live_ticket <home> <name-seq> <pid> [pool]: a local queue ticket file.
live_ticket() {
    local tickets="$1/.rsi/cargo-slot-queue/${4:-build}/tickets"
    mkdir -p "$tickets"
    printf '%s 0\n' "$3" >"$tickets/$2.$3"
}

# sleeper: a live process (its pid in SLEEPER_PID), reaped with the case.
sleeper() {
    sleep 60 >/dev/null 2>&1 &
    SLEEPER_PID=$!
    BG_PIDS="$BG_PIDS $SLEEPER_PID"
}

# hold_slot_lock <home>: hold ~/.rsi/build-slot-0.lock for the case (the lock is taken by the same
# helper cargo-slot's status probes), with a holder note.
hold_slot_lock() {
    local lock="$1/.rsi/build-slot-0.lock"
    mkdir -p "$1/.rsi"
    (
        exec 9>>"$lock"
        "$RSI_REPO_GUARD_BIN" release-guard lock-fd 9 || exit 1
        exec sleep 60
    ) >/dev/null 2>&1 &
    BG_PIDS="$BG_PIDS $!"
    disown "$!" 2>/dev/null || true
    printf 'pid 1: %s\n' "$2" >"$lock.holder"
    wait_for "the slot holder lock" slot_lock_held "$1"
}

slot_lock_held() {
    env HOME="$1" RSI_CARGO_SLOT_LOCAL=1 "$RSI_REPO_GUARD_BIN" cargo-slot status 2>/dev/null | grep -q '^holder '
}

# status: `cargo-slot --status` under the fixture environment.
status() { run_capture env $(env_args) bash "$SCRIPT" --status; }

stdout_lines() { printf '%s\n' "$RUN_OUT"; }

linux_case() { # <name> <function> <what it needs>
    if [[ "$HOST_OS" == Linux ]]; then
        run_case "$1" "$2"
    else
        skip_linux_only "$1" "$3"
    fi
}

LOCAL_NEEDS="the local fallback (flock(1), /proc/loadavg, GNU df)"

# ---- local fallback [linux] -------------------------------------------------------

gate_stops_admission() { # <setting> <value> <phase>
    fixture
    set_env "$1=$2"
    local started=$SECONDS
    invoke_default
    assert_code 124 "admission with $1=$2"
    assert_contains "$RUN_ERR" "admission-timeout: $3" "the timeout phase"
    [[ $((SECONDS - started)) -lt 5 ]] || tap_fail "admission took $((SECONDS - started))s"
    assert_eq "" "$RUN_OUT" "stdout"
}
case_the_local_load_gate_stops_admission() { gate_stops_admission RSI_CARGO_MAX_LOAD 0 "local load wait"; }
case_the_local_memory_gate_stops_admission() { gate_stops_admission RSI_CARGO_MIN_AVAIL_GB 1000000 "memory wait (local)"; }
case_the_local_disk_gate_stops_admission() { gate_stops_admission RSI_CARGO_MIN_FREE_GB 1000000 "local disk wait"; }
case_the_local_slot_gate_stops_admission() { gate_stops_admission RSI_CARGO_SLOTS 0 "local slot wait"; }

case_local_slot_tickets_are_served_in_fifo_order() {
    fixture
    local home="$BASE/fifo-home" release="$BASE/release-first" order="$BASE/run-order" queue
    queue="$home/.rsi/cargo-slot-queue/build/tickets"
    mkdir -p "$home/.rsi"
    set_env "HOME=$home" RSI_CARGO_SLOTS=1 RSI_CARGO_ADMISSION_MAX_SEC=30
    # shellcheck disable=SC2086
    env $(env_args) bash "$SCRIPT" /bin/sh -c "printf '%s\\n' first >> $order; while [ ! -e $release ]; do sleep .02; done" >"$BASE/h.out" 2>&1 &
    local holder=$!
    BG_PIDS="$BG_PIDS $holder"
    wait_for "the first holder" test -e "$order"
    # shellcheck disable=SC2086
    env $(env_args) bash "$SCRIPT" /bin/sh -c "printf '%s\\n' second >> $order" >"$BASE/w1.out" 2>&1 &
    local first_waiter=$!
    BG_PIDS="$BG_PIDS $first_waiter"
    wait_for "the first ticket" tickets_are "$queue" 1
    # shellcheck disable=SC2086
    env $(env_args) bash "$SCRIPT" /bin/sh -c "printf '%s\\n' third >> $order" >"$BASE/w2.out" 2>&1 &
    local second_waiter=$!
    BG_PIDS="$BG_PIDS $second_waiter"
    wait_for "the second ticket" tickets_are "$queue" 2
    touch "$release"
    local pid
    for pid in $holder $first_waiter $second_waiter; do
        wait "$pid" || tap_fail "a cargo-slot run failed: $(cat "$BASE"/*.out)"
    done
    assert_eq $'first\nsecond\nthird' "$(cat "$order")" "run order"
    assert_eq "" "$(ls -A "$queue")" "leftover tickets"
}

case_the_local_queue_removes_a_dead_stale_pid_ticket() {
    fixture
    local home="$BASE/stale-home" dead stale
    dead="$(dead_pid)"
    live_ticket "$home" 00000000000000000001 "$dead"
    stale="$home/.rsi/cargo-slot-queue/build/tickets/00000000000000000001.$dead"
    set_env "HOME=$home" RSI_CARGO_SLOTS=1
    invoke /bin/sh -c "printf ran"
    assert_code 0 "run past a stale ticket"
    assert_eq "ran" "$RUN_OUT" "stdout"
    assert_no_file "$stale" "the stale ticket"
}

case_an_older_live_ticket_blocks_a_free_slot_until_it_goes_away() {
    fixture
    local home="$BASE/blocked-home" ticket
    sleeper
    local older=$SLEEPER_PID
    live_ticket "$home" "0$(printf '%020d' 1)" "$older"
    ticket="$home/.rsi/cargo-slot-queue/build/tickets/0$(printf '%020d' 1).$older"
    set_env "HOME=$home" RSI_CARGO_SLOTS=2 RSI_CARGO_ADMISSION_MAX_SEC=2
    invoke /bin/sh -c "printf ran"
    assert_code 124 "blocked by an older ticket"
    assert_eq "" "$RUN_OUT" "stdout"
    assert_contains "$RUN_ERR" "older ticket" "the wait reason"
    [[ -e "$ticket" ]] || tap_fail "the older ticket was removed"
    # The timed-out waiter removed its own ticket; the older one is untouched.
    assert_eq "$(basename -- "$ticket")" "$(ls -A "$(dirname -- "$ticket")")" "tickets after the timeout"
    kill "$older" 2>/dev/null
    wait "$older" 2>/dev/null || true # a zombie still answers kill -0
    set_env RSI_CARGO_ADMISSION_MAX_SEC=10
    invoke /bin/sh -c "printf ran"
    assert_code 0 "run once the older ticket is gone"
    assert_eq "ran" "$RUN_OUT" "stdout"
    assert_no_file "$ticket" "the dead ticket"
}

case_a_higher_priority_ticket_goes_ahead_of_an_older_normal_ticket() {
    fixture
    local home="$BASE/priority-home" ticket
    sleeper
    local older=$SLEEPER_PID
    live_ticket "$home" "9$(printf '%020d' 1)" "$older" # priority 0
    ticket="$home/.rsi/cargo-slot-queue/build/tickets/9$(printf '%020d' 1).$older"
    set_env "HOME=$home" RSI_CARGO_SLOTS=2 RSI_CARGO_SLOT_PRIORITY=5 RSI_CARGO_ADMISSION_MAX_SEC=10
    invoke /bin/sh -c "printf ran"
    assert_code 0 "run ahead of an older normal ticket"
    assert_eq "ran" "$RUN_OUT" "stdout"
    [[ -e "$ticket" ]] || tap_fail "the older ticket was disturbed"
}

case_a_local_disk_guard_wait_keeps_a_ticket_and_cleans_it_on_timeout() {
    fixture
    local home="$BASE/disk-home"
    set_env "HOME=$home" RSI_CARGO_MIN_FREE_GB=1000000
    invoke /bin/sh -c "printf ran"
    assert_code 124 "disk guard wait"
    assert_eq "" "$RUN_OUT" "stdout"
    assert_contains "$RUN_ERR" "admission-timeout: local disk wait" "the timeout phase"
    assert_eq "" "$(ls -A "$home/.rsi/cargo-slot-queue/build/tickets")" "leftover tickets"
}

case_the_local_disk_floor_compares_bytes_and_reports_the_rounded_boundary() {
    fixture
    local floor=$((30 * 1024 * 1024 * 1024)) free detail timeout_line
    mkdir -p "$BASE/bin"
    set_env "PATH=$BASE/bin:$PATH" RSI_CARGO_MIN_FREE_GB=30 RSI_CARGO_NO_POOL_RECLAIM=1
    for free in $((floor - 1)) "$floor"; do
        write_exec "$BASE/bin/df" "#!/bin/sh
printf 'Avail\\n$free\\n'
"
        invoke_default
        if [[ "$free" -lt "$floor" ]]; then
            assert_code 124 "just below the floor"
            assert_eq "" "$RUN_OUT" "stdout"
            detail="30.0 GiB ($free bytes) free on /, floor 30 GiB ($floor bytes)"
            assert_contains "$RUN_ERR" "only $detail; waiting for space" "the wait notice"
            timeout_line="$(printf '%s\n' "$RUN_ERR" | grep 'admission-timeout: local disk wait')"
            assert_contains "$timeout_line" "$detail" "the timeout line"
        else
            assert_code 0 "exactly at the floor"
            assert_eq "command-ran" "$RUN_OUT" "stdout"
        fi
    done
}

case_cargo_check_uses_its_own_local_pool_and_ignores_the_load_gate() {
    fixture
    mkdir -p "$HOME_DIR/.rsi"
    # #2430: a closed build pool and a closed load gate hold a build but not a check.
    set_env RSI_CARGO_SLOTS=0 RSI_CARGO_MAX_LOAD=0
    invoke /bin/sh -c "printf command-ran" cargo check -p x
    assert_code 0 "cargo check past a closed build pool"
    assert_eq "command-ran" "$RUN_OUT" "stdout"
    # The check pool is a pool of its own: with zero check slots it times out as a slot wait.
    set_env RSI_CARGO_SLOTS=4 RSI_CARGO_MAX_LOAD=1000000 RSI_CHECK_SLOTS=0
    invoke /bin/sh -c "printf command-ran" cargo check -p x
    assert_code 124 "cargo check with no check slots"
    assert_contains "$RUN_ERR" "admission-timeout: local slot wait" "the timeout phase"
    assert_contains "$RUN_ERR" "check slots busy" "the wait reason"
    # A non-check cargo run is still gated by the same closed load gate.
    set_env RSI_CHECK_SLOTS=2 RSI_CARGO_MAX_LOAD=0
    invoke_default
    assert_code 124 "a build with a closed load gate"
}

case_the_local_disk_wait_reclaims_once_and_a_pool_build_never_does() {
    fixture
    fake_pool_tool
    local calls="$POOL_CALLS"
    set_env RSI_CARGO_MIN_FREE_GB=1000000 RSI_CARGO_ADMISSION_MAX_SEC=3
    invoke_default
    assert_code 124 "local disk wait"
    assert_contains "$RUN_ERR" "admission-timeout: local disk wait" "the timeout phase"
    assert_eq "reclaim-idle" "$(cat "$calls")" "reclaim calls"
    rm -f "$calls"
    set_env RSI_CARGO_NO_POOL_RECLAIM=1
    invoke_default
    assert_code 124 "local disk wait inside a pool build"
    assert_no_file "$calls" "a pool build must not evict its own ancestors"
}

case_an_unavailable_governor_records_the_error_before_the_local_admission_timeout() {
    fixture
    # Unsetting Bash's special SECONDS makes it a plain variable. Advance only on sleep, so
    # governor probing and local setup cannot spend the one-second admission budget before the
    # first load sample.
    printf 'unset SECONDS\nSECONDS=0\nsleep() { SECONDS=$((SECONDS + $1)); }\n' >"$BASE/clock.bash"
    # The missing socket lives under a short /tmp directory (as start_governor does): a long sandbox
    # TMPDIR would make the client answer "path is too long" instead of "No such file or directory".
    SOCK_DIR="$(mktemp -d /tmp/cgs.XXXXXX)" || tap_fail "no socket directory"
    set_env "BASH_ENV=$BASE/clock.bash" -RSI_CARGO_SLOT_LOCAL "RSI_DAEMON_SOCKET_PATH=$SOCK_DIR/missing.sock" RSI_CARGO_MAX_LOAD=0
    invoke_default
    assert_code 124 "an unreachable governor then a closed local gate"
    assert_eq "" "$RUN_OUT" "stdout"
    assert_contains "$RUN_ERR" "governor-error: unreachable" "the governor error"
    assert_contains "$RUN_ERR" "No such file or directory" "the cause"
    assert_contains "$RUN_ERR" "admission-timeout: local load wait after 1s (limit 1s)" "the timeout line"
    assert_before "$RUN_ERR" "governor-error:" "admission-timeout:" "the order of the lines"
}

# ---- status (every host) ----------------------------------------------------------

case_local_status_lists_holders_and_the_ordered_queue_without_stale_tickets() {
    fixture
    local home="$BASE/status-home" first second dead
    sleeper; first=$SLEEPER_PID
    sleeper; second=$SLEEPER_PID
    dead="$(dead_pid)"
    live_ticket "$home" "0$(printf '%020d' 2)" "$second"
    live_ticket "$home" "0$(printf '%020d' 1)" "$first"
    live_ticket "$home" "0$(printf '%020d' 3)" "$dead"
    hold_slot_lock "$home" "a build"
    set_env "HOME=$home" "RSI_DAEMON_SOCKET_PATH=$BASE/none.sock" -RSI_CARGO_SLOT_LOCAL
    status
    assert_code 0 "status"
    local lines
    lines="$(stdout_lines)"
    assert_eq "$(printf '%s\n' "$(printf '%s\n' "$lines" | sed -n 1p)" "holder build-slot-0.lock: pid 1: a build" "queue build position 0 pid $first" "queue build position 1 pid $second")" "$lines" "status lines"
    assert_contains "$(printf '%s\n' "$lines" | sed -n 1p)" "local fallback (governor status unavailable" "the first line"
}

SNAPSHOT_DAEMON='{"leases":[{"class":"build","pid":123,"label":"daemon holder","granted_at":"2026-01-01T00:00:00Z"}],"queue":[{"class":"build","position":0,"pid":456,"label":"daemon waiter"}]}'

case_status_lists_current_governor_holders_and_queue_order() {
    fixture
    start_governor snapshot 'STUB_SNAPSHOT={"leases":[{"class":"build","pid":123,"label":"test holder","granted_at":"2026-01-01T00:00:00Z"}],"queue":[{"class":"build","position":0,"pid":456,"label":"older"},{"class":"build","position":1,"pid":789,"label":"newer"}]}'
    status
    assert_code 0 "status"
    assert_stdout_is $'holder build pid 123: test holder\nqueue build position 0 pid 456: older\nqueue build position 1 pid 789: newer\n' "status lines"
}

local_holder_and_waiter() { # <home>: sets WAITER_PID
    sleeper; WAITER_PID=$SLEEPER_PID
    live_ticket "$1" "0$(printf '%020d' 1)" "$WAITER_PID"
    hold_slot_lock "$1" "a local build"
}

case_status_shows_live_local_holders_and_waiters_beside_a_reachable_daemon() {
    fixture
    start_governor snapshot 'STUB_SNAPSHOT={"leases":[],"queue":[]}'
    local home="$BASE/both-home"
    local_holder_and_waiter "$home"
    set_env "HOME=$home"
    status
    assert_code 0 "status"
    assert_stdout_is "local fallback (live local holders and waiters beside the daemon)"$'\n'"holder build-slot-0.lock: pid 1: a local build"$'\n'"queue build position 0 pid $WAITER_PID"$'\n' "status lines"
}

case_status_lists_daemon_state_first_then_the_labelled_local_state() {
    fixture
    start_governor snapshot "STUB_SNAPSHOT=$SNAPSHOT_DAEMON"
    local home="$BASE/both-order-home"
    local_holder_and_waiter "$home"
    set_env "HOME=$home"
    status
    assert_code 0 "status"
    assert_stdout_is $'holder build pid 123: daemon holder\nqueue build position 0 pid 456: daemon waiter\n'"local fallback (live local holders and waiters beside the daemon)"$'\n'"holder build-slot-0.lock: pid 1: a local build"$'\n'"queue build position 0 pid $WAITER_PID"$'\n' "status lines"
}

case_forced_local_status_reports_the_local_backend_and_skips_the_daemon() {
    fixture
    start_governor snapshot "STUB_SNAPSHOT=$SNAPSHOT_DAEMON"
    local home="$BASE/forced-home"
    local_holder_and_waiter "$home"
    set_env "HOME=$home" RSI_CARGO_SLOT_LOCAL=1
    status
    assert_code 0 "status"
    assert_stdout_is "local fallback (RSI_CARGO_SLOT_LOCAL=1: daemon skipped)"$'\n'"holder build-slot-0.lock: pid 1: a local build"$'\n'"queue build position 0 pid $WAITER_PID"$'\n' "status lines"
    assert_eq "" "$(cat "$GOV_LOG")" "the daemon was asked"
}

# ---- the governor path (every host) -----------------------------------------------

case_cargo_check_asks_the_governor_for_the_check_class() {
    fixture
    start_governor grant STUB_TICKET=00000000-0000-0000-0000-000000000009
    local words expected
    for words in "check:cargo check -p x" "build:cargo test -p x" "build:cargo build --check"; do
        expected="${words%%:*}"
        : >"$GOV_LOG"
        # shellcheck disable=SC2086
        invoke /bin/sh -c "printf ran" ${words#*:}
        assert_code 0 "run for [$words]"
        assert_eq "$expected" "$(acquire_classes | head -n 1)" "the class asked for [$words]"
    done
}

# fake_pool_tool: a stand-in target-pool that counts how often cargo-slot asks it to reclaim;
# sets POOL_CALLS (its call log) and points RSI_TARGET_POOL_TOOL at it.
fake_pool_tool() {
    local tool="$BASE/target-pool"
    POOL_CALLS="$BASE/reclaim-calls"
    write_exec "$tool" "#!/bin/sh
echo \"\$@\" >> $POOL_CALLS
echo '{\"removed\": [\"pool-a\"], \"freed_bytes\": 100}'
"
    set_env "RSI_TARGET_POOL_TOOL=$tool"
}

case_a_worker_waiting_on_the_disk_floor_reclaims_the_idle_pool_once() {
    fixture
    # #2773: the governor reports the disk floor; cargo-slot evicts idle target pools once
    # (bounded), records it as a phase line, and keeps waiting until the governor grants.
    fake_pool_tool
    local calls="$POOL_CALLS"
    start_governor disk STUB_TICKET=00000000-0000-0000-0000-000000000011 \
        'STUB_MESSAGE=only 30.0 GiB (32212254719 bytes) free on /, floor 30 GiB (32212254720 bytes); waiting for space'
    set_env RSI_CARGO_ADMISSION_MAX_SEC=30
    invoke_default
    assert_code 0 "the run after the disk wait"
    assert_eq "command-ran" "$RUN_OUT" "stdout"
    [[ "$(acquire_classes | wc -l | tr -d ' ')" -ge 4 ]] || tap_fail "expected at least four acquire calls"
    assert_eq "reclaim-idle" "$(cat "$calls")" "reclaim calls"
    assert_contains "$RUN_ERR" "slot-wait: disk floor: reclaimed idle target pools:" "the phase line"
    assert_contains "$RUN_ERR" "pool-a" "the reclaim report"
}

case_other_waits_do_not_reclaim_the_pool() {
    fixture
    fake_pool_tool
    local calls="$POOL_CALLS"
    start_governor queue STUB_TICKET=00000000-0000-0000-0000-000000000012
    invoke_default
    assert_code 124 "a governor slot wait"
    assert_no_file "$calls" "a slot wait must leave the warm pool alone"
}

case_a_memory_wait_survives_a_transient_governor_timeout_with_the_same_ticket() {
    fixture
    local ticket=00000000-0000-0000-0000-000000000001
    start_governor memory-stall "STUB_TICKET=$ticket" 'STUB_MESSAGE=memory low (available 15 GB < 16); waiting'
    # Local fallback would stall; successful execution must use the governor.
    set_env RSI_CARGO_ADMISSION_MAX_SEC=20 RSI_CARGO_MAX_LOAD=0
    invoke_default
    assert_code 0 "the run after a stalled governor"
    assert_eq "command-ran" "$RUN_OUT" "stdout"
    assert_contains "$RUN_ERR" "memory wait (governor)" "the wait phase"
    assert_contains "$RUN_ERR" "transient RPC failure:" "the retry note"
    [[ "$(acquire_classes | wc -l | tr -d ' ')" -ge 3 ]] || tap_fail "expected at least three acquire calls"
    # Every retry reuses the queued ticket.
    local retries
    retries="$(grep '"AcquireAdmission"' "$GOV_LOG" | sed -n '2,$p' | grep -c "\"ticket_id\":\"$ticket\"" || true)"
    assert_eq "$(( $(acquire_classes | wc -l | tr -d ' ') - 1 ))" "$retries" "acquires after the first that carry the ticket"
    assert_contains "$(last_request)" '"ReleaseAdmission"' "the last request"
    assert_contains "$(last_request)" "\"lease_id\":\"$ticket\"" "the released lease"
}

case_the_governor_queue_is_bounded_and_cancels_its_ticket() {
    fixture
    local ticket=00000000-0000-0000-0000-000000000002
    start_governor queue "STUB_TICKET=$ticket"
    invoke_default
    assert_code 124 "a queue that never grants"
    assert_eq "" "$RUN_OUT" "stdout"
    assert_contains "$RUN_ERR" "admission-timeout: governor wait" "the timeout phase"
    assert_contains "$(last_request)" '"ReleaseAdmission"' "the last request"
    assert_contains "$(last_request)" "\"lease_id\":\"$ticket\"" "the released ticket"
}

case_a_budgeted_caller_can_extend_the_interactive_admission_limit() {
    fixture
    # Accelerate the 600s interactive default to 1s; the override must still grant after 2s
    # instead of clamping a daemon job back to that default.
    sed 's/admission_max=600/admission_max=1/' "$SCRIPT" >"$BASE/cargo-slot" || tap_fail "cannot copy the script"
    start_governor queue-then-grant STUB_TICKET=00000000-0000-0000-0000-000000000003
    set_env RSI_CARGO_ADMISSION_MAX_SEC=10
    SLOT_SCRIPT="$BASE/cargo-slot" invoke_default
    assert_code 0 "the run with an extended limit"
    assert_eq "command-ran" "$RUN_OUT" "stdout"
    [[ "$(acquire_classes | wc -l | tr -d ' ')" -ge 2 ]] || tap_fail "expected at least two acquire calls"
    assert_contains "$(last_request)" '"ReleaseAdmission"' "the last request"
}

run_lander() { # [NAME=value...]: a stand-in rsi-rolling-land through the governor; sets LANDER_TMPDIR/TARGET
    write_exec "$BASE/rsi-rolling-land" $'#!/bin/sh\nprintf "%s\\n%s\\n" "$TMPDIR" "${CARGO_TARGET_DIR-unset}"\n'
    mkdir -p "$HOME_DIR/.rsi"
    set_env RSI_CARGO_ADMISSION_MAX_SEC=20 "TMPDIR=$BASE" "$@"
    invoke "$BASE/rsi-rolling-land"
    assert_code 0 "the lander stand-in"
    LANDER_TMPDIR="$(printf '%s\n' "$RUN_OUT" | sed -n 1p)"
    LANDER_TARGET="$(printf '%s\n' "$RUN_OUT" | sed -n 2p)"
}

case_cargo_target_dir_reaches_the_command_unchanged() {
    fixture
    start_governor grant
    # #938: cargo-slot never overrides an explicit target (a lander side's).
    local side="$BASE/candidate-cargo-target"
    set_env "CARGO_TARGET_DIR=$side"
    invoke /bin/sh -c 'printf %s "$CARGO_TARGET_DIR"'
    assert_code 0 "a command that reads CARGO_TARGET_DIR"
    assert_eq "$side" "$RUN_OUT" "the target the command saw"
    run_lander
    assert_eq "$side" "$LANDER_TARGET" "the target the lander saw"
    set_env -CARGO_TARGET_DIR
    run_lander
    assert_eq "unset" "$LANDER_TARGET" "an unset target stays unset"
}

case_the_command_runs_where_ionice_is_absent() {
    fixture
    start_governor grant
    # A PATH holding only the tools the script and the stand-in need, no ionice.
    mkdir -p "$BASE/path"
    local tool
    for tool in sh bash env nice cat printf date sleep rm mkdir dirname basename cut awk sed tr head tail grep expr id uname readlink kill; do
        [[ -x "$BASE/path/$tool" ]] || ln -s "$(command -v "$tool" 2>/dev/null || echo /bin/false)" "$BASE/path/$tool" 2>/dev/null || true
    done
    rm -f "$BASE/path/ionice"
    set_env "PATH=$BASE/path"
    invoke /bin/sh -c "printf command-ran"
    assert_code 0 "a run on a host without ionice"
    assert_eq "command-ran" "$RUN_OUT" "stdout"
}

case_cargo_slot_without_rsi_repo_guard_exits_127_and_says_so() {
    fixture
    # No binary anywhere cargo-slot looks: an empty HOME, no target, no PATH entry.
    mkdir -p "$BASE/lib"
    cp "$SCRIPT" "$BASE/lib/cargo-slot"
    set_env -RSI_REPO_GUARD_BIN -CARGO_TARGET_DIR "HOME=$BASE/empty-home" PATH=/usr/bin:/bin
    SLOT_SCRIPT="$BASE/lib/cargo-slot" invoke /bin/sh -c "printf ran"
    assert_code 127 "cargo-slot without the helper binary"
    assert_eq "" "$RUN_OUT" "the command ran"
    assert_contains "$RUN_ERR" "rsi-repo-guard is not installed" "the message"
}

case_cargo_slot_status_without_rsi_repo_guard_never_reads_as_healthy() {
    fixture
    mkdir -p "$BASE/lib"
    cp "$SCRIPT" "$BASE/lib/cargo-slot"
    set_env -RSI_REPO_GUARD_BIN -CARGO_TARGET_DIR "HOME=$BASE/empty-home" PATH=/usr/bin:/bin
    run_capture env $(env_args) bash "$BASE/lib/cargo-slot" --status
    assert_code 127 "status without the helper binary"
    assert_contains "$RUN_ERR" "rsi-repo-guard is not installed" "the message"
}


# ---- which rsi-repo-guard is trusted, and a failing governor client (#3893) --------------------

# stale_guard <path>: a build from before `cargo-slot` existed. When STALE_MARK is set it
# records every run, so a case can prove a candidate was never executed.
stale_guard() {
    write_exec "$1" "#!/bin/sh
echo \"ran \$0 \$*\" >>\"\${STALE_MARK:-/dev/null}\"
echo \"rsi-repo-guard: unknown guard 'cargo-slot'; known guards: agents-md-size\" >&2
exit 2
"
}

# guard_fixture: a copy of the script under $BASE/lib (so nothing beside it or in the checkout is
# found), an empty install tree, and no override or target directory in the environment.
guard_fixture() {
    fixture
    mkdir -p "$BASE/lib" "$HOME_DIR/.rsi/install" "$HOME_DIR/.local/bin"
    cp "$SCRIPT" "$BASE/lib/cargo-slot"
    SLOT_SCRIPT="$BASE/lib/cargo-slot"
    set_env -RSI_REPO_GUARD_BIN -CARGO_TARGET_DIR PATH=/usr/bin:/bin
}

case_a_stale_guard_is_skipped_and_named_and_the_next_candidate_serves() {
    guard_fixture
    stale_guard "$HOME_DIR/.rsi/install/rsi-repo-guard"
    cp "$RSI_REPO_GUARD_BIN" "$HOME_DIR/.local/bin/rsi-repo-guard"
    start_governor grant
    invoke_default
    assert_code 0 "the run behind a stale first candidate"
    assert_eq "command-ran" "$RUN_OUT" "stdout"
    assert_contains "$RUN_ERR" "guard-skipped: $HOME_DIR/.rsi/install/rsi-repo-guard" "the skipped candidate is named"
    assert_contains "$RUN_ERR" "unknown guard 'cargo-slot'" "the reason is shown"
    assert_contains "$RUN_ERR" "slot-granted" "the governed grant"
    assert_eq "AcquireAdmission" "$(methods | sed -n 1p)" "the usable guard asked the governor"
}

case_the_installed_guard_wins_over_a_sandbox_build() {
    guard_fixture
    cp "$RSI_REPO_GUARD_BIN" "$HOME_DIR/.rsi/install/rsi-repo-guard"
    # A sandbox build, reachable only through CARGO_TARGET_DIR, must never be executed.
    stale_guard "$BASE/target/release/rsi-repo-guard"
    set_env "CARGO_TARGET_DIR=$BASE/target" "STALE_MARK=$BASE/stale.marks"
    start_governor grant
    invoke_default
    assert_code 0 "the run with an installed guard and a sandbox build"
    assert_eq "command-ran" "$RUN_OUT" "stdout"
    assert_no_file "$BASE/stale.marks" "the sandbox build was never run"
    assert_eq "AcquireAdmission" "$(methods | sed -n 1p)" "the installed guard asked the governor"
}

case_when_every_guard_is_stale_cargo_slot_exits_127_naming_each() {
    guard_fixture
    stale_guard "$HOME_DIR/.rsi/install/rsi-repo-guard"
    stale_guard "$HOME_DIR/.local/bin/rsi-repo-guard"
    invoke_default
    assert_code 127 "every candidate failed its probe"
    assert_eq "" "$RUN_OUT" "the command ran"
    assert_contains "$RUN_ERR" "guard-skipped: $HOME_DIR/.rsi/install/rsi-repo-guard" "first skipped path"
    assert_contains "$RUN_ERR" "guard-skipped: $HOME_DIR/.local/bin/rsi-repo-guard" "second skipped path"
    assert_contains "$RUN_ERR" "rsi-repo-guard is not installed, or every copy failed its probe" "the message"
}

# flaky_guard <path> <failures>: the real client, except its first <failures> `rpc acquire` calls
# exit 1 with a message on stderr (the probe and every other subcommand pass through).
flaky_guard() {
    write_exec "$1" "#!/bin/sh
if [ \"\$1 \$2 \$3\" = \"cargo-slot rpc acquire\" ]; then
  n=\$(cat '$BASE/acquires' 2>/dev/null || echo 0); n=\$((n + 1)); echo \$n >'$BASE/acquires'
  if [ \$n -le $2 ]; then echo \"simulated client crash \$n\" >&2; exit 1; fi
fi
exec '$RSI_REPO_GUARD_BIN' \"\$@\"
"
}

case_a_failing_governor_client_is_visible_and_retried_not_treated_as_unreachable() {
    guard_fixture
    flaky_guard "$HOME_DIR/.rsi/install/rsi-repo-guard" 2
    start_governor grant
    set_env RSI_CARGO_ADMISSION_MAX_SEC=30
    invoke_default
    assert_code 0 "the run after two client failures"
    assert_eq "command-ran" "$RUN_OUT" "stdout"
    assert_contains "$RUN_ERR" "governor-error: RPC client exited 1 (attempt 1): simulated client crash 1" "the first failure with its stderr"
    assert_contains "$RUN_ERR" "governor-error: RPC client exited 1 (attempt 2): simulated client crash 2" "the second failure"
    assert_contains "$RUN_ERR" "RPC client failed (exit 1), retrying: simulated client crash" "the retry note"
    assert_contains "$RUN_ERR" "slot-granted" "the governed grant after the retries"
    assert_eq "3" "$(cat "$BASE/acquires")" "client calls"
    assert_eq "AcquireAdmission" "$(methods | sed -n 1p)" "the governor saw the request"
}

case_a_persistent_client_failure_falls_back_then_the_reask_leaves_the_local_gates() {
    guard_fixture
    # Three failures in a row put the run on the local gates (load gate closed, so they never
    # grant); the next ask, one second later, is answered and the run leaves the fallback.
    flaky_guard "$HOME_DIR/.rsi/install/rsi-repo-guard" 3
    start_governor grant
    set_env RSI_CARGO_ADMISSION_MAX_SEC=30 RSI_CARGO_MAX_LOAD=0 RSI_CARGO_GOVERNOR_REASK_SEC=1
    invoke_default
    assert_code 0 "the run after the governor answered the re-ask"
    assert_eq "command-ran" "$RUN_OUT" "stdout"
    assert_contains "$RUN_ERR" "governor-error: RPC client exited 1 (attempt 3): simulated client crash 3" "the last failure"
    assert_contains "$RUN_ERR" "governor unreachable; using the local slot gates (asking the governor again every 1s)" "the fallback line"
    assert_before "$RUN_ERR" "using the local slot gates" "asking the governor again from the local fallback" "the re-ask"
    assert_before "$RUN_ERR" "asking the governor again from the local fallback" "the governor answered; leaving the local slot gates" "the exit from the fallback"
    assert_contains "$RUN_ERR" "slot-granted" "the grant"
    tickets_are "$HOME_DIR/.rsi/cargo-slot-queue/build/tickets" 0 || tap_fail "the local ticket was left behind"
}

# ---- the lander TMPDIR [linux] ----------------------------------------------------

case_the_lander_tmpdir_goes_to_tmpfs_only_with_room_for_this_user() {
    fixture
    start_governor grant
    [[ -d /dev/shm && -w /dev/shm ]] || tap_fail "no writable /dev/shm"
    run_lander RSI_LANDER_TMPFS_MIN_GB=0
    case "$LANDER_TMPDIR" in /dev/shm/rsi-lander-*) ;; *) tap_fail "TMPDIR is $LANDER_TMPDIR" ;; esac
    assert_no_file "$LANDER_TMPDIR" "removed after the run"
    run_lander RSI_LANDER_TMPFS_MIN_GB=1000000
    assert_eq "$BASE" "$LANDER_TMPDIR" "TMPDIR with no room"
    assert_contains "$RUN_ERR" "lander TMPDIR stays" "the notice"
}

case_the_lander_tmpdir_counts_the_user_quota_not_just_free_space() {
    fixture
    start_governor grant
    [[ -d /dev/shm && -w /dev/shm ]] || tap_fail "no writable /dev/shm"
    # #938: systemd's per-user tmpfs quota (80% of the mount) runs out before df's free space;
    # a lander TMPDIR sized by df failed with EDQUOT. The quota-aware room is `cargo-slot
    # free-gb`: asking for one more GiB than it reports must leave TMPDIR alone and say so.
    local room
    room="$("$RSI_REPO_GUARD_BIN" cargo-slot free-gb /dev/shm)" || tap_fail "free-gb failed"
    run_lander "RSI_LANDER_TMPFS_MIN_GB=$((room + 1))"
    assert_eq "$BASE" "$LANDER_TMPDIR" "TMPDIR when the room is below the minimum"
    assert_contains "$RUN_ERR" "quota included" "the notice"
}

linux_case "[linux] the local load gate stops admission and names the phase" case_the_local_load_gate_stops_admission "$LOCAL_NEEDS"
linux_case "[linux] the local memory gate stops admission and names the phase" case_the_local_memory_gate_stops_admission "$LOCAL_NEEDS"
linux_case "[linux] the local disk gate stops admission and names the phase" case_the_local_disk_gate_stops_admission "$LOCAL_NEEDS"
linux_case "[linux] the local slot gate stops admission and names the phase" case_the_local_slot_gate_stops_admission "$LOCAL_NEEDS"
linux_case "[linux] local slot tickets are served in FIFO order" case_local_slot_tickets_are_served_in_fifo_order "$LOCAL_NEEDS"
linux_case "[linux] the local queue removes a dead stale-pid ticket" case_the_local_queue_removes_a_dead_stale_pid_ticket "$LOCAL_NEEDS"
linux_case "[linux] an older live ticket blocks a free slot until it goes away" case_an_older_live_ticket_blocks_a_free_slot_until_it_goes_away "$LOCAL_NEEDS"
linux_case "[linux] a higher-priority ticket goes ahead of an older normal ticket" case_a_higher_priority_ticket_goes_ahead_of_an_older_normal_ticket "$LOCAL_NEEDS"
linux_case "[linux] a local disk-guard wait keeps a ticket and cleans it on timeout" case_a_local_disk_guard_wait_keeps_a_ticket_and_cleans_it_on_timeout "$LOCAL_NEEDS"
linux_case "[linux] the local disk floor compares bytes and reports the rounded boundary" case_the_local_disk_floor_compares_bytes_and_reports_the_rounded_boundary "$LOCAL_NEEDS"
run_case "local status lists holders and the ordered queue without stale tickets" case_local_status_lists_holders_and_the_ordered_queue_without_stale_tickets
linux_case "[linux] cargo check uses its own local pool and ignores the load gate" case_cargo_check_uses_its_own_local_pool_and_ignores_the_load_gate "$LOCAL_NEEDS"
run_case "cargo check asks the governor for the check class" case_cargo_check_asks_the_governor_for_the_check_class
run_case "a worker waiting on the disk floor reclaims the idle pool once" case_a_worker_waiting_on_the_disk_floor_reclaims_the_idle_pool_once
run_case "other waits do not reclaim the pool" case_other_waits_do_not_reclaim_the_pool
linux_case "[linux] the local disk wait reclaims once and a pool build never does" case_the_local_disk_wait_reclaims_once_and_a_pool_build_never_does "$LOCAL_NEEDS"
run_case "status lists current governor holders and queue order" case_status_lists_current_governor_holders_and_queue_order
run_case "status shows live local holders and waiters beside a reachable daemon" case_status_shows_live_local_holders_and_waiters_beside_a_reachable_daemon
run_case "status lists daemon state first, then the labelled local state" case_status_lists_daemon_state_first_then_the_labelled_local_state
run_case "forced local status reports the local backend and skips the daemon" case_forced_local_status_reports_the_local_backend_and_skips_the_daemon
run_case "a memory wait survives a transient governor timeout with the same ticket" case_a_memory_wait_survives_a_transient_governor_timeout_with_the_same_ticket
run_case "the governor queue is bounded and cancels its ticket" case_the_governor_queue_is_bounded_and_cancels_its_ticket
run_case "a budgeted caller can extend the interactive admission limit" case_a_budgeted_caller_can_extend_the_interactive_admission_limit
linux_case "[linux] an unavailable governor records the error before the local admission timeout" case_an_unavailable_governor_records_the_error_before_the_local_admission_timeout "$LOCAL_NEEDS"
run_case "CARGO_TARGET_DIR reaches the command unchanged" case_cargo_target_dir_reaches_the_command_unchanged
linux_case "[linux] the lander TMPDIR goes to tmpfs only with room for this user" case_the_lander_tmpdir_goes_to_tmpfs_only_with_room_for_this_user "/dev/shm"
linux_case "[linux] the lander TMPDIR counts the user quota, not just free space" case_the_lander_tmpdir_counts_the_user_quota_not_just_free_space "/dev/shm and a user block quota"
run_case "the command runs where ionice is absent" case_the_command_runs_where_ionice_is_absent
run_case "cargo-slot without rsi-repo-guard exits 127 and says so" case_cargo_slot_without_rsi_repo_guard_exits_127_and_says_so
run_case "cargo-slot --status without rsi-repo-guard never reads as healthy" case_cargo_slot_status_without_rsi_repo_guard_never_reads_as_healthy
run_case "a stale guard is skipped and named and the next candidate serves" case_a_stale_guard_is_skipped_and_named_and_the_next_candidate_serves
run_case "the installed guard wins over a sandbox build" case_the_installed_guard_wins_over_a_sandbox_build
run_case "when every guard is stale cargo-slot exits 127 naming each" case_when_every_guard_is_stale_cargo_slot_exits_127_naming_each
run_case "a failing governor client is visible and retried, not treated as unreachable" case_a_failing_governor_client_is_visible_and_retried_not_treated_as_unreachable
linux_case "[linux] a persistent client failure falls back, then the re-ask leaves the local gates" case_a_persistent_client_failure_falls_back_then_the_reask_leaves_the_local_gates "$LOCAL_NEEDS"
tap_done
