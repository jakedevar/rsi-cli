#!/usr/bin/env bash
# suite-gate: tap
# Guard: every AgentScheduleWake `mode:"when"` example in the guidance names `message`.
#
# `AgentScheduleWake` refuses a call without `message` (invalid_input `required "message"`),
# including `mode:"when"` (#119). A doc or shipped guidance text that shows a `mode:"when"`
# request with a `when` predicate but no `message` teaches workers a call the daemon refuses,
# so each such example block must mention `message`. Port of the Python suite
# tools/test_wake_when_message_docs.py (#366).
source "$(dirname "$0")/../scripts/tests/lib/tap.sh"

SCAN="AGENTS.md docs .claude .agents .codex .gemini crates/rsid/src/session/guidance"

# The block scanner: paragraphs and list items. Prints the first line number of every block
# that shows a wake `mode:"when"` example with a predicate: `awk -v want=violations` only
# those without `message`, `awk -v want=examples` all of them.
BLOCKS_AWK='
function flush(   mode_when, payload, verb, has_message) {
    if (cur == "") return
    verb = (cur ~ /AgentScheduleWake|schedule_wake/)
    mode_when = (cur ~ /mode[^[:alnum:]_]?[^[:alnum:]_]?[^[:alnum:]_]?[^[:alnum:]_]?when([^[:alnum:]_]|$)/)
    payload = (cur ~ /when["`'"'"']?[[:space:]]*:[[:space:]]*\{/ || cur ~ /(^|[^[:alnum:]_])when\.(jobs_terminal|sha_on_rolling|issue_closed|todo_closed|topology_execution_settled|settlement_run_settled|health_anomaly)/)
    has_message = (cur ~ /(^|[^[:alnum:]_])message([^[:alnum:]_]|$)/)
    if (verb && mode_when && payload && (want == "examples" || !has_message)) print start
    cur = ""
}
{
    blank = ($0 ~ /^[[:space:]]*$/)
    if (blank || $0 ~ /^[[:space:]]*([-*]|[0-9]+\.)[[:space:]]/) { flush(); start = NR }
    if (!blank) {
        if (cur == "") start = NR
        cur = (cur == "") ? $0 : cur "\n" $0
    }
}
END { flush() }'

scan_text() { # <want> <text>
    printf '%s\n' "$2" | awk -v want="$1" "$BLOCKS_AWK"
}

scanned_files() {
    local entry
    for entry in $SCAN; do
        if [[ -f "$REPO_ROOT/$entry" ]]; then
            echo "$REPO_ROOT/$entry"
        elif [[ -d "$REPO_ROOT/$entry" ]]; then
            find "$REPO_ROOT/$entry" -type f \( -name '*.md' -o -name '*.toml' \) | sort
        fi
    done
}

case_the_detector_flags_a_missing_message() {
    assert_eq 1 "$(scan_text violations $'Then ONE `AgentScheduleWake`\n`mode:"when"` (`when.jobs_terminal`), then end.')" "two-line prose"
    assert_eq 1 "$(scan_text violations 'arm `AgentScheduleWake {mode:"when", when:{jobs_terminal:[ids]}}`.')" "inline call"
}

case_the_detector_accepts_a_message() {
    assert_eq "" "$(scan_text violations 'ONE `AgentScheduleWake {"mode":"when","message":"done","when":{"jobs_terminal":["<id>"]}}`.')" "report"
}

case_the_scan_is_not_vacuous() {
    local total=0 file n
    while IFS= read -r file; do
        n="$(scan_text examples "$(cat "$file")" | grep -c . || true)"
        total=$((total + n))
    done < <(scanned_files)
    [[ "$total" -ge 8 ]] || tap_fail "expected the known mode:when examples to be scanned; found $total"
}

case_every_mode_when_example_names_message() {
    local problems="" file lines line
    while IFS= read -r file; do
        lines="$(scan_text violations "$(cat "$file")")"
        for line in $lines; do
            problems="$problems ${file#"$REPO_ROOT"/}:$line"
        done
    done < <(scanned_files)
    assert_eq "" "$problems" "AgentScheduleWake mode:when examples must include \`message\` (#119)"
}

case_the_shipped_preamble_text_requires_message() {
    assert_contains "$(cat "$REPO_ROOT/crates/rsid/src/session/preamble.rs")" '`mode` and `message`'
}

run_case "the detector flags a missing message" case_the_detector_flags_a_missing_message
run_case "the detector accepts a message" case_the_detector_accepts_a_message
run_case "the scan is not vacuous" case_the_scan_is_not_vacuous
run_case "every mode:when example names message" case_every_mode_when_example_names_message
run_case "the shipped preamble text requires message" case_the_shipped_preamble_text_requires_message
tap_done
