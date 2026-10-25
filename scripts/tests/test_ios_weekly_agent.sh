#!/usr/bin/env bash
# suite-gate: tap
# Tests for the weekly iOS re-sign launchd agent (hub #2963, mac-mini #7).
#
# Nothing here bootstraps a real launchd agent or runs xcodebuild: the installer runs
# with --dry-run or with a stub launchctl, and weekly-resign.sh runs against a stub
# dev-install.sh in a temp dir. Port of the Python suite of the same name (#366).
#
# Case map (legacy test_ios_weekly_agent.py, InstallWeeklyAgentTest + WeeklyResignTest -> here; the
# method name is the case name with spaces for underscores and `test_` dropped):
#   dry_run_plist_is_valid_retrying_agent .. install_is_idempotent_and_uninstall_removes (7)
#       -> the seven installer run_case lines, in the same order
#   success_logs_one_ok_line_and_passes_flag .. no_bsd_only_flags_in_scripts (12)
#       -> the twelve weekly-resign run_case lines, in the same order
# The legacy suite parsed the plist with plistlib: lib/plist.awk is that contract (well-formed
# XML, typed values, keyed dict members) and the case "the plist reader rejects malformed or
# mistyped documents" is its own negative coverage (a string StartInterval, mismatched tags,
# a value with no key, a repeated key).
source "$(dirname "$0")/lib/tap.sh"

IOS_DIR="$SCRIPTS_DIR/ios"
INSTALL="$IOS_DIR/install-weekly-agent.sh"
RESIGN="$IOS_DIR/weekly-resign.sh"
LABEL="dev.rsi.ios-resign"
NOW=1800000000 # fixed fake clock (epoch seconds)
DAY=86400

LOG_LINE='^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z (ok|skip|failed) exit=[0-9]+ [^[:space:]].*$'

setup_home() {
    HOME_DIR="$CASE_DIR/home"
    mkdir -p "$HOME_DIR"
    AGENT_DIR="$HOME_DIR/Library/LaunchAgents"
    PLIST_PATH="$AGENT_DIR/$LABEL.plist"
}

# run_script <script> [args...]: EXTRA_ENV holds extra NAME=value assignments.
run_script() {
    local script="$1"
    shift
    # shellcheck disable=SC2086
    run_capture env -i PATH="${PATH:-/usr/bin:/bin}" HOME="$HOME_DIR" RSI_IOS_AGENT_DIR="$AGENT_DIR" \
        RSI_IOS_UID=501 ${EXTRA_ENV:-} /bin/bash "$script" "$@"
}

# --- plist helpers: the rendered plist is parsed as a property list, not grepped ---
# lib/plist.awk is the plistlib contract of the legacy suite: well-formed XML, typed values,
# a key for every dict value (a `<string>21600</string>` StartInterval or a mismatched tag is
# a parse/type failure, not a text match).
PLIST_AWK="$TAP_LIB_DIR/plist.awk"
PLIST_FLAT=""

plist_text() { # the text between the plist markers of RUN_OUT
    printf '%s\n' "$RUN_OUT" | awk '/^--- plist begin ---$/{on=1;next} /^--- plist end ---$/{on=0} on{print}'
}

# plist_parse <xml text>: parse into PLIST_FLAT or fail the case with the parser's message.
plist_parse() {
    PLIST_FLAT="$(printf '%s\n' "$1" | awk -f "$PLIST_AWK" 2>"$CASE_DIR/.plist.err")" \
        || tap_fail "not a valid property list: $(cat "$CASE_DIR/.plist.err")"
    assert_ne "" "$PLIST_FLAT" "the parsed property list"
}

plist_load() { # parse the plist printed by the last dry run
    plist_parse "$(plist_text)"
}

# plist_get <dotted path> <type>: the value; the key must exist WITH that type.
plist_get() {
    printf '%s\n' "$PLIST_FLAT" | awk -F'\t' -v p="$1" -v t="$2" '
        $1 == p { if ($2 == t) { print $3; ok = 1 } else { bad = $2 } }
        END { if (!ok) { print "plist key [" p "] is " (bad == "" ? "missing" : "a " bad ", not a " t) > "/dev/stderr"; exit 1 } }' \
        || tap_fail "plist key [$1] is missing or not a $2"
}

plist_type() { # <dotted path>: the value's type, empty when the path is absent
    printf '%s\n' "$PLIST_FLAT" | awk -F'\t' -v p="$1" '$1 == p { print $2; exit }'
}

# swap <text> <from> <to>: replace the first occurrence; the mutation target MUST exist.
swap() {
    local pre="${1%%"$2"*}"
    [[ "$pre" != "$1" ]] || tap_fail "mutation target [$2] not found"
    printf '%s' "$pre$3${1#*"$2"}"
}

plist_has_key() { # <dotted path>
    printf '%s\n' "$PLIST_FLAT" | awk -F'\t' -v p="$1" '$1 == p { found = 1 } END { exit found ? 0 : 1 }'
}

plist_env_keys() { # the keys inside EnvironmentVariables
    printf '%s\n' "$PLIST_FLAT" | awk -F'\t' '$1 ~ /^EnvironmentVariables\./ { k = $1; sub(/^EnvironmentVariables\./, "", k); print k }'
}

dry_run() { # [args...]
    run_script "$INSTALL" --dry-run "$@"
    assert_code 0 "dry run (stderr: $RUN_ERR)"
    assert_ne "" "$(plist_text)" "the rendered plist"
    plist_load
}

# --- the installer ---

case_the_dry_run_plist_is_a_valid_retrying_agent() {
    setup_home
    dry_run
    assert_eq "$LABEL" "$(plist_get Label string)" "Label"
    # Fires every 6 hours; weekly-resign.sh decides whether an install is due. An integer.
    assert_eq $((6 * 3600)) "$(plist_get StartInterval integer)" "StartInterval"
    ! plist_has_key StartCalendarInterval || tap_fail "the plist has a StartCalendarInterval"
    assert_eq true "$(plist_get RunAtLoad true)" "RunAtLoad"
    assert_eq /bin/bash "$(plist_get ProgramArguments.0 string)" "ProgramArguments[0]"
    assert_eq "$RESIGN" "$(plist_get ProgramArguments.1 string)" "ProgramArguments[1]"
    plist_get ProgramArguments array >/dev/null
    assert_eq 2 "$(printf '%s\n' "$PLIST_FLAT" | grep -c '^ProgramArguments\.')" "ProgramArguments length"
    assert_ne "" "$(plist_get EnvironmentVariables.PATH string)" "the PATH variable"
    assert_eq "$HOME_DIR" "$(plist_get EnvironmentVariables.HOME string)" "the HOME variable"
    assert_contains "$RUN_OUT" "bootstrap gui/501 $PLIST_PATH"
    assert_contains "$RUN_OUT" "bootout gui/501/$LABEL"
}

# Negative coverage of the reader: each document is a valid plist with ONE defect.
case_the_plist_reader_rejects_malformed_or_mistyped_documents() {
    local good bad
    good=$'<?xml version="1.0" encoding="UTF-8"?>\n<plist version="1.0">\n<dict>\n  <key>Label</key>\n  <string>x</string>\n  <key>StartInterval</key>\n  <integer>21600</integer>\n  <key>RunAtLoad</key>\n  <true/>\n  <key>ProgramArguments</key>\n  <array>\n    <string>/bin/bash</string>\n  </array>\n</dict>\n</plist>'
    plist_parse "$good"
    assert_eq 21600 "$(plist_get StartInterval integer)" "the control document parses with typed values"
    assert_eq /bin/bash "$(plist_get ProgramArguments.0 string)" "the control array item"
    # A well-formed document whose StartInterval is a string: parses, but the type check fails.
    plist_parse "$(swap "$good" '<integer>21600</integer>' '<string>21600</string>')"
    assert_eq string "$(plist_type StartInterval)" "a string StartInterval is typed string, not integer"
    # Documents the reader must refuse outright.
    for bad in \
        "$(swap "$good" '</array>' '</dict>')" \
        "$(swap "$good" '</plist>' '')" \
        "$(swap "$good" '<integer>21600</integer>' '<integer>abc</integer>')" \
        "$(swap "$good" '<key>Label</key>' '')" \
        "$(swap "$good" '<key>StartInterval</key>' '<key>Label</key>')" \
        "$(swap "$good" '<true/>' '<true>')" \
        "$(swap "$good" '<dict>' '<dict><key>Orphan</key>')" \
        "stray text" \
        ""; do
        printf '%s\n' "$bad" | awk -f "$PLIST_AWK" >/dev/null 2>&1 && tap_fail "the reader accepted a defective plist: [$bad]"
    done
    return 0
}

case_the_dry_run_touches_nothing() {
    setup_home
    dry_run
    assert_no_file "$AGENT_DIR" "agent directory"
    assert_no_file "$HOME_DIR/.rsi" "~/.rsi"
}

# Shapes that must never appear in the plist: Apple team ids (10 uppercase
# alphanumerics), device UDIDs (8-16 hex groups) and e-mail addresses.
forbidden_shape() { # <text>: prints the pattern that matches, if any
    local text="$1" pattern
    for pattern in \
        '(^|[^[:alnum:]_])[0-9A-Z]{10}([^[:alnum:]_]|$)' \
        '(^|[^[:alnum:]_])[0-9A-Fa-f]{8}-[0-9A-Fa-f]{16}([^[:alnum:]_]|$)' \
        '(^|[^[:alnum:]_])[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}([^[:alnum:]_]|$)' \
        '@[A-Za-z0-9.-]+\.[A-Za-z]{2,}'; do
        if printf '%s\n' "$text" | grep -Eq "$pattern"; then
            echo "$pattern"
            return 0
        fi
    done
    return 0
}

case_the_plist_carries_no_device_team_or_apple_id() {
    setup_home
    local stdout scrubbed value
    # Even with such values in the environment, they never reach the plist.
    EXTRA_ENV="RSI_IOS_DEVICE_ID=00008110-001234567890ABCD RSI_IOS_TEAM_ID=ABCDE12345 RSI_IOS_APPLE_ID=someone@example.invalid" \
        run_script "$INSTALL" --dry-run
    assert_code 0 "dry run with identifiers in the environment (stderr: $RUN_ERR)"
    plist_load
    for value in 00008110-001234567890ABCD ABCDE12345 someone@example.invalid; do
        assert_not_contains "$RUN_OUT" "$value"
    done
    assert_eq $'HOME\nPATH' "$(plist_env_keys | sort)" "environment keys"
    # The temp home and the checkout path (a sandbox path may contain a session UUID)
    # are expected; scan the rest of the rendered text.
    stdout="$RUN_OUT"
    scrubbed="${stdout//$HOME_DIR/}"
    scrubbed="${scrubbed//$RESIGN/}"
    assert_eq "" "$(forbidden_shape "$scrubbed")" "a forbidden shape appears in the rendered text"
}

case_a_custom_interval() {
    setup_home
    dry_run --interval-hours 3
    assert_eq $((3 * 3600)) "$(plist_get StartInterval integer)" "StartInterval"
}

case_a_bad_interval_is_rejected() {
    setup_home
    local args
    for args in "--interval-hours 25" "--interval-hours 0" "--interval-hours x" "--interval-hours" "--weekday 1"; do
        # shellcheck disable=SC2086
        run_script "$INSTALL" --dry-run $args
        assert_code 2 "exit code for [$args] (stderr: $RUN_ERR)"
    done
    run_script "$INSTALL" --bogus
    assert_code 2 "exit code for --bogus"
}

case_the_uninstall_dry_run() {
    setup_home
    run_script "$INSTALL" --dry-run --uninstall
    assert_code 0 "exit code (stderr: $RUN_ERR)"
    assert_contains "$RUN_OUT" "bootout gui/501/$LABEL"
    assert_not_contains "$RUN_OUT" "bootstrap"
}

case_install_is_idempotent_and_uninstall_removes() {
    setup_home
    local calls="$HOME_DIR/launchctl.calls" stub="$HOME_DIR/launchctl-stub" round expected
    write_exec "$stub" "#!/bin/bash
echo \"\$@\" >> \"$calls\"
exit 0
"
    for round in 1 2; do
        EXTRA_ENV="RSI_IOS_LAUNCHCTL=$stub" run_script "$INSTALL"
        assert_code 0 "install round $round (stderr: $RUN_ERR)"
        plist_parse "$(cat "$PLIST_PATH")"
        assert_eq "$LABEL" "$(plist_get Label string)" "the installed plist Label key"
    done
    # Each install is bootout (replace) then bootstrap, twice, in that order.
    expected="bootout gui/501/$LABEL"$'\n'"bootstrap gui/501 $PLIST_PATH"
    assert_eq "$expected"$'\n'"$expected" "$(cat "$calls")" "launchctl calls"
    EXTRA_ENV="RSI_IOS_LAUNCHCTL=$stub" run_script "$INSTALL" --uninstall
    assert_code 0 "uninstall (stderr: $RUN_ERR)"
    assert_no_file "$PLIST_PATH" "the plist after uninstall"
    assert_eq "bootout gui/501/$LABEL" "$(tail -n 1 "$calls")" "the last launchctl call"
}

# --- weekly-resign.sh ---

LOG_FILE=""
LAST_LOG=""
STATE_FILE=""

setup_resign() {
    setup_home
    LOG_FILE="$HOME_DIR/.rsi/logs/ios-resign.log"
    LAST_LOG="$HOME_DIR/.rsi/logs/ios-resign.last.log"
    STATE_FILE="$HOME_DIR/.rsi/ios-resign.state"
}

# resign <dev-install stub body> [now]: run weekly-resign.sh against that stub.
resign() {
    local stub="$HOME_DIR/dev-install-stub.sh"
    write_exec "$stub" "#!/bin/bash
$1"
    EXTRA_ENV="RSI_IOS_DEV_INSTALL=$stub RSI_IOS_RESIGN_NOW=${2:-$NOW}" run_script "$RESIGN"
}

log_line() { # <n>: the nth (1-based) line of the log
    sed -n "${1}p" "$LOG_FILE"
}

log_count() { wc -l <"$LOG_FILE" | tr -d ' '; }

write_state() { # <epoch>
    mkdir -p "$(dirname "$STATE_FILE")"
    printf '%s\n' "$1" >"$STATE_FILE"
}

state_epoch() { tr -d '[:space:]' <"$STATE_FILE"; }

counter_lines() { wc -l <"$HOME_DIR/count" | tr -d ' '; }

assert_log_line_shape() { # <line>
    printf '%s\n' "$1" | grep -Eq "$LOG_LINE" || tap_fail "log line does not match the shape: [$1]"
}

case_success_logs_one_ok_line_and_passes_the_flag() {
    setup_resign
    resign "echo \"\$@\" > \"$HOME_DIR/argv\"
echo built
exit 0
"
    assert_code 0 "exit code (stderr: $RUN_ERR)"
    assert_eq 1 "$(log_count)" "log lines"
    assert_log_line_shape "$(log_line 1)"
    assert_contains "$(log_line 1)" " ok exit=0 "
    assert_eq "--network-first" "$(cat "$HOME_DIR/argv")" "dev-install arguments"
    assert_no_file "$LAST_LOG" "the last-failure log"
}

case_missing_state_is_due_now_and_success_seeds_the_clock() {
    setup_resign
    assert_no_file "$STATE_FILE"
    resign "echo x >> \"$HOME_DIR/count\"
exit 0
"
    assert_code 0 "exit code (stderr: $RUN_ERR)"
    assert_eq 1 "$(counter_lines)" "dev-install runs"
    assert_eq "$NOW" "$(state_epoch)" "state epoch"
    assert_contains "$(log_line 1)" "no-state"
}

case_before_day_5_is_a_skip() {
    setup_resign
    local last_ok=$((NOW - (4 * DAY + 23 * 3600))) # 4d23h old
    write_state "$last_ok"
    resign "echo x >> \"$HOME_DIR/count\"
exit 0
"
    assert_code 0 "exit code (stderr: $RUN_ERR)"
    assert_no_file "$HOME_DIR/count" "the dev-install counter"
    assert_eq "$last_ok" "$(state_epoch)" "state epoch"
    assert_eq 1 "$(log_count)" "log lines"
    assert_log_line_shape "$(log_line 1)"
    assert_contains "$(log_line 1)" " skip exit=0 "
    assert_contains "$(log_line 1)" "age=4d23h"
}

case_a_skip_does_not_touch_the_last_failure_log() {
    setup_resign
    resign "exit 3
"
    assert_file "$LAST_LOG" "the last-failure log"
    write_state $((NOW - DAY))
    resign "exit 0
"
    assert_file "$LAST_LOG" "the last-failure log after a skip"
}

case_from_day_5_it_runs_and_success_resets_the_clock() {
    setup_resign
    local body="echo x >> \"$HOME_DIR/count\"
exit 0
"
    write_state $((NOW - 5 * DAY))
    resign "$body"
    assert_code 0 "exit code (stderr: $RUN_ERR)"
    assert_eq 1 "$(counter_lines)" "dev-install runs"
    assert_eq "$NOW" "$(state_epoch)" "state epoch"
    assert_contains "$(log_line 1)" " ok exit=0 "
    # Clock reset: an hour later it is a skip again.
    resign "$body" $((NOW + 3600))
    assert_eq 1 "$(counter_lines)" "dev-install runs after the reset"
    assert_contains "$(log_line 2)" " skip exit=0 "
}

case_a_failure_keeps_the_state_and_the_next_run_retries() {
    setup_resign
    local last_ok=$((NOW - 6 * DAY))
    write_state "$last_ok"
    resign "echo x >> \"$HOME_DIR/count\"
exit 3
"
    assert_code 3 "failing run exit code"
    assert_eq "$last_ok" "$(state_epoch)" "state epoch after the failure"
    # Six hours later the phone is back: the retry runs and succeeds.
    resign "echo x >> \"$HOME_DIR/count\"
exit 0
" $((NOW + 6 * 3600))
    assert_code 0 "retry exit code (stderr: $RUN_ERR)"
    assert_eq 2 "$(counter_lines)" "dev-install runs"
    assert_eq $((NOW + 6 * 3600)) "$(state_epoch)" "state epoch after the retry"
    assert_eq 2 "$(log_count)" "log lines"
    assert_contains "$(log_line 1)" " failed exit=3 "
    assert_contains "$(log_line 2)" " ok exit=0 "
    assert_no_file "$LAST_LOG" "the last-failure log"
}

case_no_device_exit_3_reads_needs_phone() {
    setup_resign
    resign "echo \"no paired device\"
exit 3
"
    assert_code 3 "exit code"
    assert_eq 1 "$(log_count)" "log lines"
    assert_log_line_shape "$(log_line 1)"
    assert_contains "$(log_line 1)" " failed exit=3 needs-phone "
    assert_contains "$(cat "$LAST_LOG")" "no paired device"
    assert_no_file "$STATE_FILE" "the state file"
}

case_a_build_failure_keeps_the_full_output_and_one_line() {
    setup_resign
    resign "echo first line
printf 'error: signing\tfailed\n'
echo
exit 65
"
    assert_code 65 "exit code"
    assert_eq 1 "$(log_count)" "log lines"
    assert_log_line_shape "$(log_line 1)"
    assert_contains "$(log_line 1)" " failed exit=65 error: signing failed"
    # The full output, byte for byte: two lines and an empty third.
    assert_eq $'first line\nerror: signing\tfailed\n\nx' "$(cat "$LAST_LOG"; echo x)" "the kept output"
}

case_unreadable_or_future_state_is_due_now() {
    setup_resign
    local content
    for content in "garbage"$'\n' "$((NOW + 10 * DAY))"$'\n' ""; do
        rm -f "$HOME_DIR/count"
        mkdir -p "$(dirname "$STATE_FILE")"
        printf '%s' "$content" >"$STATE_FILE"
        resign "echo x >> \"$HOME_DIR/count\"
exit 3
"
        assert_eq 1 "$(counter_lines)" "dev-install runs for state [$content]"
        assert_eq "${content}x" "$(cat "$STATE_FILE"; printf x)" "state file for [$content]"
    done
}

case_success_after_a_failure_clears_the_last_log() {
    setup_resign
    resign "exit 3
"
    assert_file "$LAST_LOG" "the last-failure log"
    resign "exit 0
"
    assert_no_file "$LAST_LOG" "the last-failure log after a success"
    assert_eq 2 "$(log_count)" "log lines"
}

case_a_missing_dev_install_is_a_logged_failure() {
    setup_resign
    EXTRA_ENV="RSI_IOS_DEV_INSTALL=$HOME_DIR/does-not-exist.sh RSI_IOS_RESIGN_NOW=$NOW" run_script "$RESIGN"
    assert_code 127 "exit code"
    assert_eq 1 "$(log_count)" "log lines"
    assert_log_line_shape "$(log_line 1)"
    assert_contains "$(log_line 1)" " failed exit=127 "
    assert_no_file "$STATE_FILE" "the state file"
}

case_no_bsd_only_flags_in_the_scripts() {
    local script text pattern
    for script in "$INSTALL" "$RESIGN"; do
        text="$(cat "$script")"
        for pattern in "sed -i ''" "stat -f" "date -j" "mktemp -t" "mapfile" "declare -A"; do
            assert_not_contains "$text" "$pattern" "$(basename "$script")"
        done
    done
}

run_case "the dry-run plist is a valid retrying agent" case_the_dry_run_plist_is_a_valid_retrying_agent
run_case "the dry run touches nothing" case_the_dry_run_touches_nothing
run_case "the plist reader rejects malformed or mistyped documents" case_the_plist_reader_rejects_malformed_or_mistyped_documents
run_case "the plist carries no device, team or apple id" case_the_plist_carries_no_device_team_or_apple_id
run_case "a custom interval" case_a_custom_interval
run_case "a bad interval is rejected" case_a_bad_interval_is_rejected
run_case "the uninstall dry run" case_the_uninstall_dry_run
run_case "install is idempotent and uninstall removes" case_install_is_idempotent_and_uninstall_removes
run_case "success logs one ok line and passes the flag" case_success_logs_one_ok_line_and_passes_the_flag
run_case "missing state is due now and success seeds the clock" case_missing_state_is_due_now_and_success_seeds_the_clock
run_case "before day 5 is a skip" case_before_day_5_is_a_skip
run_case "a skip does not touch the last failure log" case_a_skip_does_not_touch_the_last_failure_log
run_case "from day 5 it runs and success resets the clock" case_from_day_5_it_runs_and_success_resets_the_clock
run_case "a failure keeps the state and the next run retries" case_a_failure_keeps_the_state_and_the_next_run_retries
run_case "no device (exit 3) reads needs-phone" case_no_device_exit_3_reads_needs_phone
run_case "a build failure keeps the full output and one line" case_a_build_failure_keeps_the_full_output_and_one_line
run_case "unreadable or future state is due now" case_unreadable_or_future_state_is_due_now
run_case "success after a failure clears the last log" case_success_after_a_failure_clears_the_last_log
run_case "a missing dev-install is a logged failure" case_a_missing_dev_install_is_a_logged_failure
run_case "no BSD-only flags in the scripts" case_no_bsd_only_flags_in_the_scripts
tap_done
