#!/usr/bin/env bash
# suite-gate: tap
# #1647: rsid.socket and install-release.sh share a stable user-only front door.
# Port of the Python suite of the same name (#366).
source "$(dirname "$0")/lib/tap.sh"

case_managed_socket_is_user_only_and_is_retained_across_service_restarts() {
    local socket service
    socket="$(cat "$SCRIPTS_DIR/systemd/rsid.socket")"
    service="$(cat "$SCRIPTS_DIR/systemd/rsid.service")"
    assert_contains "$socket" "ListenStream=%h/.rsi/daemon.sock"
    assert_contains "$socket" "SocketMode=0600"
    assert_contains "$socket" "DirectoryMode=0700"
    assert_contains "$socket" "Accept=no"
    assert_contains "$socket" "Service=rsid.service"
    assert_contains "$socket" "WantedBy=sockets.target"
    assert_contains "$service" "Sockets=rsid.socket"
    assert_contains "$service" "Requires=rsi-workers.slice rsid.socket"
    assert_contains "$service" 'ExecStart=%h/.rsi/install/rsid-supervisor.sh %h/.local/bin/rsid'
    assert_contains "$service" "UMask=0077"
}

case_recovery_runbook_reuses_configured_socket_and_daemon_environment() {
    local runbook
    runbook="$(cat "$SCRIPTS_DIR/systemd/README.md")"
    assert_contains "$runbook" 'export RSI_DAEMON_SOCKET_PATH="$socket" RSI_SOCKET="$socket"'
    assert_contains "$runbook" "stat -c '%i %a %U' \"\$socket\""
    assert_contains "$runbook" 'rsi-rpc --socket "$socket" GetHealthStatus'
    assert_contains "$runbook" "source scripts/rsid-health.sh"
    assert_contains "$runbook" 'rsid_rpc_ready "$HOME/.rsi/install/rsi-rpc" "$socket" "$holder_pid"'
    assert_contains "$runbook" "rsid_daemon_environment"
    assert_contains "$runbook" '"${DAEMON_ENV_ARGS[@]}"'
    assert_contains "$runbook" '--setenv="RSI_DAEMON_SOCKET_PATH=$socket"'
}

case_recovery_unit_carries_the_installer_resource_limits() {
    local runbook installer recovery line
    runbook="$(cat "$SCRIPTS_DIR/systemd/README.md")"
    installer="$(cat "$SCRIPTS_DIR/install-release.sh")"
    # From `unit="rsid-recover-` up to the first `launcher_pid=` after it. Both delimiters MUST
    # exist (the legacy suite raised on absence): without them the window would silently widen
    # to the whole runbook or an unbounded suffix and any property anywhere would satisfy it.
    assert_contains "$runbook" 'unit="rsid-recover-' "the recovery unit start marker"
    recovery="${runbook#*unit=\"rsid-recover-}"
    assert_contains "$recovery" "launcher_pid=" "the recovery window end marker"
    recovery="${recovery%%launcher_pid=*}"
    assert_ne "" "$recovery" "the recovery window"
    assert_contains "$runbook" "source scripts/rsid-scope.sh"
    assert_contains "$runbook" "load_rsid_scope_settings"
    for line in \
        '--property="MemoryHigh=${RSID_SCOPE_MEMORY_HIGH_MIB}M"' \
        '--property="MemoryMax=${RSID_SCOPE_MEMORY_MAX_MIB}M"' \
        '--property="MemorySwapMax=${RSID_SCOPE_MEMORY_SWAP_MAX_MIB}M"' \
        '--property="CPUWeight=${RSID_SCOPE_CPU_WEIGHT}"'; do
        assert_contains "$installer" "$line" "install-release.sh"
        assert_contains "$recovery" "$line" "the recovery unit"
    done
}

# restart <platform> [no-holder]: run install-release.sh's restart_rsid with every effect
# mocked (no daemon, systemd unit or user file is touched). Sets RUN_* plus CALLS,
# SERVICE_ENV and MODES.
restart() {
    local platform="$1" holder_exists=true function_text harness
    [[ "${2:-}" == no-holder ]] && holder_exists=false
    FIXTURE="$CASE_DIR/fixture"
    mkdir -p "$FIXTURE"
    if [[ "$holder_exists" == true ]]; then
        write_exec "$FIXTURE/rsi-socket-hold" $'#!/bin/sh\nexit 0\n'
        chmod 700 "$FIXTURE/rsi-socket-hold"
    fi
    function_text="$(awk '/^installer_daemon_environment\(\) \{/{on=1} /^# #2188: the links now point at INSTALL_DIR/{on=0} on{print}' "$SCRIPTS_DIR/install-release.sh")"
    assert_ne "" "$function_text" "the extracted install-release.sh function block"
    harness="$CASE_DIR/harness.sh"
    {
        printf 'umask 022\n'
        cat "$SCRIPTS_DIR/rsid-health.sh"
        cat <<'MOCKS'
set -euo pipefail
calls="$FIXTURE/calls"
record() { printf '%s\n' "$*" >> "$calls"; }
uname() { echo "$PLATFORM"; }
systemctl() {
    if [[ "$*" == *--property=MainPID* ]]; then echo 999996; else return 1; fi
}
rsid_running_pids() {
    if [[ ! -f "$FIXTURE/probed" ]]; then touch "$FIXTURE/probed"; echo 999999; fi
}
rsid_launcher_pids() { record "snapshot $*"; echo "999998 999997"; }
rsid_announce_intended_stop() { record "announce $*"; }
rsid_pids_still_running() {
    record "launcher-probe $*"
    if [[ ! -f "$FIXTURE/launcher-probed" ]]; then
        touch "$FIXTURE/launcher-probed"; echo 999997
    else
        record launcher-released
    fi
}
sleep() { record "wait $*"; }
kill() { record "kill $*"; }
load_rsid_scope_settings() {
    RSID_SCOPE_MEMORY_HIGH_MIB=6144 RSID_SCOPE_MEMORY_MAX_MIB=8192
    RSID_SCOPE_MEMORY_SWAP_MAX_MIB=0 RSID_SCOPE_CPU_WEIGHT=20
}
provision_worker_slice() { record provision; }
run_restart_drain_hook() { record drain; }
systemd-run() {
    record "systemd-run $*"
    local arg key
    for arg in "$@"; do
        if [[ "$arg" == --setenv=* && "${arg#--setenv=}" != *=* ]]; then
            key="${arg#--setenv=}"
            printf '%s=%s\n' "$key" "${!key}" >> "$FIXTURE/service-env"
        fi
    done
}
nohup() { record "nohup $*"; }
disown() { :; }
wait_for_rsid_health() { record "health $*"; }
verify_rsid_scope() { record "verify $*"; }
daemon_socket_path() { echo "$FIXTURE/custom.sock"; }
RSI_HOME_DIR="$FIXTURE/home"
INSTALL_DIR="$FIXTURE/install"
SOCKET_HOLD_BIN="$FIXTURE/rsi-socket-hold"
RSID_BIN="$INSTALL_DIR/rsid"
RSI_RPC_BIN="$INSTALL_DIR/rsi-rpc"
DAEMON_LOG="$FIXTURE/daemon.log"
MOCKS
        printf '%s\n' "$function_text"
        # Give the mocked macOS nohup job time to write its arguments.
        cat <<'TAIL'
restart_rsid
wait
for path in "$RSI_HOME_DIR" "$DAEMON_LOG"; do { stat -c '%a' "$path" 2>/dev/null || stat -f '%Lp' "$path"; } | tr '\n' ' '; echo "$path"; done > "$FIXTURE/modes"
TAIL
    } >"$harness"
    FIXTURE="$FIXTURE" PLATFORM="$platform" RSI_MEMORY_ENABLED=false \
        XDG_CONFIG_HOME="/fixture/config with spaces" RSI_SANDBOX_BASE=/fixture/sandboxes \
        ANTHROPIC_API_KEY=fixture-provider-secret RSI_SESSION_TOKEN=fixture-agent-secret \
        run_capture bash "$harness"
    CALLS="" SERVICE_ENV="" MODES=""
    [[ -f "$FIXTURE/calls" ]] && CALLS="$(cat "$FIXTURE/calls")"
    [[ -f "$FIXTURE/service-env" ]] && SERVICE_ENV="$(cat "$FIXTURE/service-env")"
    [[ -f "$FIXTURE/modes" ]] && MODES="$(cat "$FIXTURE/modes")"
    rm -rf "$FIXTURE"
}

case_linux_launcher_is_durable_and_wraps_the_supervisor_in_the_holder() {
    restart Linux
    assert_code 0 "exit code (stderr: $RUN_ERR)"
    assert_contains "$CALLS" "systemd-run --user --collect --service-type=exec"
    assert_contains "$CALLS" ".service --slice=user.slice"
    assert_contains "$CALLS" "MemoryHigh=6144M"
    assert_contains "$CALLS" "MemoryMax=8192M"
    assert_contains "$CALLS" "MemorySwapMax=0M"
    assert_contains "$CALLS" "CPUWeight=20"
    assert_contains "$CALLS" "--property=StandardOutput=append:"
    assert_contains "$CALLS" "--setenv=RSI_SOCKET="
    printf '%s\n' "$CALLS" | grep -Eq 'rsi-socket-hold [^ ]+/custom.sock -- [^ ]+/install/rsid-supervisor.sh [^ ]+/install/rsid' \
        || tap_fail "the supervisor is not wrapped in the holder: $CALLS"
    assert_contains "$CALLS" "verify rsid-install-"
}

# umask 022 is the hostile default: without the launcher's own umask 077 the redirect
# would create daemon.log 0644 in a 0755 directory (#1852).
case_log_and_data_dir_are_created_owner_only_before_output_is_redirected() {
    local platform
    for platform in Linux Darwin; do
        restart "$platform"
        assert_code 0 "exit code on $platform (stderr: $RUN_ERR)"
        assert_eq 2 "$(printf '%s\n' "$MODES" | wc -l | tr -d ' ')" "mode lines on $platform: $MODES"
        case "$(printf '%s\n' "$MODES" | sed -n 1p)" in "700 "*) ;; *) tap_fail "data dir mode on $platform: $MODES" ;; esac
        case "$(printf '%s\n' "$MODES" | sed -n 2p)" in "600 "*) ;; *) tap_fail "log mode on $platform: $MODES" ;; esac
    done
}

case_transient_service_receives_configuration_without_logging_secrets() {
    restart Linux
    assert_code 0 "exit code (stderr: $RUN_ERR)"
    assert_contains "$SERVICE_ENV" "RSI_MEMORY_ENABLED=false"
    assert_contains "$SERVICE_ENV" "RSI_SANDBOX_BASE=/fixture/sandboxes"
    assert_contains "$SERVICE_ENV" "XDG_CONFIG_HOME=/fixture/config with spaces"
    assert_contains "$CALLS" "--setenv=RSI_MEMORY_ENABLED"
    local output
    for output in "$RUN_OUT" "$RUN_ERR" "$CALLS" "$SERVICE_ENV"; do
        assert_not_contains "$output" "fixture-provider-secret"
        assert_not_contains "$output" "fixture-agent-secret"
    done
}

case_replacement_waits_for_the_old_supervisor_and_holder() {
    restart Linux
    assert_code 0 "exit code (stderr: $RUN_ERR)"
    assert_before "$CALLS" "snapshot 999999" "kill -INT 999999" "snapshot after kill"
    # #3433: the supervisor must know this SIGINT is intended.
    assert_before "$CALLS" "announce install_release 999999" "kill -INT 999999" "announce after kill"
    assert_before "$CALLS" "wait 0.1" "launcher-released" "no wait before release"
    assert_before "$CALLS" "launcher-released" "systemd-run " "systemd-run before release"
}

case_launcher_snapshot_follows_only_known_parents_and_ignores_zombies() {
    local harness="$CASE_DIR/snapshot.sh"
    {
        cat "$SCRIPTS_DIR/rsid-processes.sh"
        cat <<'HARNESS'
ps() {
    case "$*" in
        '-o ppid= -p 10') echo 11 ;;
        '-o ppid= -p 11') echo 12 ;;
        '-o ppid= -p 12') echo 13 ;;
        '-o args= -p 11') echo '/bin/bash /fixture/rsid-supervisor.sh /fixture/rsid' ;;
        '-o args= -p 12') echo '/fixture/rsi-socket-hold /fixture/daemon.sock -- supervisor' ;;
        '-o args= -p 13') echo '/bin/bash' ;;
        '-o stat= -p 11') echo Z ;;
        '-o stat= -p 12') echo S ;;
    esac
}
rsid_launcher_pids 10
rsid_pids_still_running 11 12
HARNESS
    } >"$harness"
    run_capture bash "$harness"
    assert_code 0 "exit code (stderr: $RUN_ERR)"
    assert_eq $'11\n12\n12' "$RUN_OUT" "pids"
}

case_macos_uses_the_portable_holder_and_supervisor() {
    restart Darwin
    assert_code 0 "exit code (stderr: $RUN_ERR)"
    printf '%s\n' "$CALLS" | grep -Eq 'nohup [^ ]+/rsi-socket-hold [^ ]+/custom.sock -- [^ ]+/install/rsid-supervisor.sh [^ ]+/install/rsid' \
        || tap_fail "no portable holder invocation: $CALLS"
    assert_contains "$CALLS" "health "
}

case_missing_holder_refuses_before_stopping_the_running_daemon() {
    restart Linux no-holder
    assert_code 1 "exit code"
    assert_contains "$RUN_ERR" "refusing to start or restart without the front door"
    assert_eq "" "$CALLS" "recorded calls"
}

run_case "the managed socket is user-only and is retained across service restarts" case_managed_socket_is_user_only_and_is_retained_across_service_restarts
run_case "the recovery runbook reuses the configured socket and daemon environment" case_recovery_runbook_reuses_configured_socket_and_daemon_environment
run_case "the recovery unit carries the installer resource limits" case_recovery_unit_carries_the_installer_resource_limits
run_case "the Linux launcher is durable and wraps the supervisor in the holder" case_linux_launcher_is_durable_and_wraps_the_supervisor_in_the_holder
run_case "the log and data dir are created owner-only before output is redirected" case_log_and_data_dir_are_created_owner_only_before_output_is_redirected
run_case "the transient service receives configuration without logging secrets" case_transient_service_receives_configuration_without_logging_secrets
run_case "the replacement waits for the old supervisor and holder" case_replacement_waits_for_the_old_supervisor_and_holder
run_case "the launcher snapshot follows only known parents and ignores zombies" case_launcher_snapshot_follows_only_known_parents_and_ignores_zombies
run_case "macOS uses the portable holder and supervisor" case_macos_uses_the_portable_holder_and_supervisor
run_case "a missing holder refuses before stopping the running daemon" case_missing_holder_refuses_before_stopping_the_running_daemon
tap_done
