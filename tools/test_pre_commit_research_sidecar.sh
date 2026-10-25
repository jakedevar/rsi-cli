#!/usr/bin/env bash
# suite-gate: tap
# Exercise the pre-commit research-artifact classifier (#2948).
#
# Raw measurement JSON under thoughts/shared/research/ must not be run through
# rsi-research-validate; real sidecars still must be, and a malformed one fails. A stub
# validator (RSI_VALIDATE_BIN_DIR) makes the cases deterministic: it records every path it
# is given and rejects a file containing MALFORMED. Port of the Python suite
# tools/test_pre_commit_research_sidecar.py (#366).
source "$(dirname "$0")/../scripts/tests/lib/tap.sh"

HOOKS="$REPO_ROOT/tools/git-hooks"
RAW='{"source_revision": "abc", "cpu": {"user": 1.5}, "io": {"read_bytes": 10}}'
SIDECAR='{"version": 2, "research_question": "q", "areas": ["a"]}'
BAD_SIDECAR='{"version": 2, "research_question": "MALFORMED"}'

setup_repo() {
    REPO="$CASE_DIR/repo"
    BIN="$CASE_DIR/bin"
    LOG="$CASE_DIR/validator.log"
    mkdir -p "$BIN"
    write_exec "$BIN/rsi-research-validate" '#!/bin/sh
echo "$1" >> "$VALIDATOR_LOG"
if grep -q MALFORMED "$1"; then
  echo "Parse: unknown field" >&2
  exit 2
fi
exit 0
'
    : >"$LOG"
    fixture_repo "$REPO"
    printf 'seed\n' >"$REPO/seed.txt"
    fixture_commit "$REPO" seed
    mkdir -p "$REPO/thoughts/shared/research"
}

stage() { # <name> <body>: prints the repository-relative path
    local path="thoughts/shared/research/$1"
    printf '%s\n' "$2" >"$REPO/$path"
    git -C "$REPO" add "$path"
    echo "$path"
}

pre_commit() {
    cd "$REPO" || exit 1
    run_capture env -u RSI_SKIP_STATIC_GUARDS CLAUDE_AGENT_ROLE=pipeline-research \
        RSI_VALIDATE_BIN_DIR="$BIN" VALIDATOR_LOG="$LOG" sh "$HOOKS/pre-commit"
    cd "$CASE_DIR" || exit 1
}

validated() { # the paths the stub validator was given, sorted, space separated
    sort "$LOG" | tr '\n' ' ' | sed 's/ $//'
}

case_raw_evidence_json_is_not_validated_as_a_sidecar() {
    setup_repo
    local name
    for name in cpu-io.json run.evidence.json run.measurements.json; do
        stage "$name" "$RAW" >/dev/null
    done
    pre_commit
    assert_code 0 "exit code (stderr: $RUN_ERR)"
    assert_eq "" "$(validated)" "validated paths"
}

case_an_exempt_suffix_wins_over_a_sibling_doc_and_marker() {
    setup_repo
    stage run.md "# run" >/dev/null
    stage run.evidence.json "$BAD_SIDECAR" >/dev/null
    pre_commit
    assert_code 0 "exit code (stderr: $RUN_ERR)"
    assert_eq "" "$(validated)" "validated paths"
}

case_valid_sidecars_are_validated_by_every_marker() {
    setup_repo
    local a b c
    a="$(stage a.research.json "$SIDECAR")"
    b="$(stage b.json "$SIDECAR")" # research_question marker
    c="$(stage c.json '{"version": 2}')" # sibling doc below
    stage c.md "# c" >/dev/null
    pre_commit
    assert_code 0 "exit code (stderr: $RUN_ERR)"
    assert_eq "$(printf '%s\n' "$a" "$b" "$c" | sort | tr '\n' ' ' | sed 's/ $//')" "$(validated)" "validated paths"
}

case_malformed_sidecars_are_rejected() {
    setup_repo
    stage x.research.json '{"MALFORMED": 1}' >/dev/null
    stage y.json "$BAD_SIDECAR" >/dev/null
    pre_commit
    assert_code 1 "exit code"
    assert_contains "$RUN_ERR" "research sidecar FAILED validation"
    assert_eq 4 "$(wc -l <"$LOG" | tr -d ' ')" "each sidecar runs twice on failure"
}

case_a_malformed_sidecar_is_rejected_beside_raw_evidence() {
    setup_repo
    stage cpu-io.json "$RAW" >/dev/null
    stage bad.research.json '{"MALFORMED": 1}' >/dev/null
    pre_commit
    assert_code 1 "exit code"
    assert_contains "$RUN_ERR" "thoughts/shared/research/bad.research.json"
    assert_not_contains "$RUN_ERR" "cpu-io.json"
}

run_case "raw evidence JSON is not validated as a sidecar" case_raw_evidence_json_is_not_validated_as_a_sidecar
run_case "an exempt suffix wins over a sibling doc and marker" case_an_exempt_suffix_wins_over_a_sibling_doc_and_marker
run_case "valid sidecars are validated by every marker" case_valid_sidecars_are_validated_by_every_marker
run_case "malformed sidecars are rejected" case_malformed_sidecars_are_rejected
run_case "a malformed sidecar is rejected beside raw evidence" case_a_malformed_sidecar_is_rejected_beside_raw_evidence
tap_done
