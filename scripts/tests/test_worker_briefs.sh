#!/usr/bin/env bash
# suite-gate: tap
# Worker briefs keep script verification, the retired worker contract and baseline-red guidance
# consistent. Replaces the Python suite test_python_verification_briefs.py (#366): Python is
# forbidden (#358), so the briefs may no longer name a Python runner at all.
#
# Case map (legacy test_python_verification_briefs.py -> here):
#   test_canonical_briefs_name_stdlib_unittest   -> INTENTIONALLY REMOVED: Python is banned (#358), the briefs must
#                                                   not require any Python runner (the next case enforces the opposite)
#   test_canonical_briefs_name_the_typed_missing_runner -> canonical briefs name the typed missing runner
#   test_no_canonical_or_rendered_brief_requires_pytest -> no canonical or rendered brief requires a Python runner
#                                                   (pytest AND unittest now)
#   test_the_guard_pattern_catches_a_required_pytest_command -> the runner pattern catches a required Python runner
#   test_retired_contract_is_only_a_pointer      -> the retired contract is only a pointer (byte-exact)
#   test_no_live_file_references_the_retired_contract -> no live file references the retired contract
#   test_guard_covers_code_docs_and_mirrors_but_preserves_handoff_history -> the reference guard covers ... (uses the
#                                                   SAME retired_reference predicate as the scan, incl. the
#                                                   canonical-content negative)
#   test_worker_guidance_reuses_or_isolates_baseline_repairs -> same name
source "$(dirname "$0")/lib/tap.sh"

# Canonical script-verification guidance: shared preamble, the worker contract and the
# satellite delegation guide.
CANONICAL=".claude/commands/_shared/worker_preamble.md docs/agents/worker-contract.md docs/satellites.md"
# Rendered brief mirrors of the commands and skills workers are launched with.
RENDERED_DIRS=".claude/commands .codex/prompts .agents/skills .gemini/commands"
# Built from two parts so this file never matches its own search.
RETIRED="thoughts/shared/manager/worker-contract"".md"
CONTRACT_POINTER='See [the canonical worker contract](../../../docs/agents/worker-contract.md).'

# A required Python test runner: `python -m pytest`, `python3 -m unittest`, or `pytest <path>`.
RUNNER_PATTERN='(-m[[:space:]]+(pytest|unittest)([^[:alnum:]_]|$))|((^|[^[:alnum:]_./-])pytest[[:space:]]+(-[[:alnum:]_]+[[:space:]]+)*(scripts|tools)/)'

brief_files() { # every canonical and rendered brief, one path per line
    local path dir
    for path in $CANONICAL; do
        echo "$REPO_ROOT/$path"
    done
    for dir in $RENDERED_DIRS; do
        [[ -d "$REPO_ROOT/$dir" ]] && find "$REPO_ROOT/$dir" \( -name '*.md' -o -name '*.toml' \) -type f | sort
    done
}

case_canonical_briefs_name_the_typed_missing_runner() {
    local path
    for path in $CANONICAL; do
        assert_contains "$(cat "$REPO_ROOT/$path")" "runner_unavailable" "$path"
    done
}

case_no_canonical_or_rendered_brief_requires_a_python_runner() {
    local count=0 file hits=""
    while IFS= read -r file; do
        count=$((count + 1))
        if grep -Eq "$RUNNER_PATTERN" "$file"; then
            hits="$hits $file:$(grep -En "$RUNNER_PATTERN" "$file" | head -1)"
        fi
    done < <(brief_files)
    assert_eq "" "$hits" "a brief requires a Python runner"
    [[ "$count" -gt 3 ]] || tap_fail "only $count briefs were scanned"
}

case_the_runner_pattern_catches_a_required_python_runner() {
    local line
    for line in "python3 -m pytest scripts/tests/test_x.py" 'run `pytest -q tools/tests/test_x.py`' "python3 -m unittest -v scripts/tests/test_x.py"; do
        printf '%s\n' "$line" | grep -Eq "$RUNNER_PATTERN" || tap_fail "not caught: $line"
    done
    if printf '%s\n' "never require \`pytest\`; Python is banned" | grep -Eq "$RUNNER_PATTERN"; then
        tap_fail "a prohibition was read as a requirement"
    fi
}

case_the_retired_contract_is_only_a_pointer() {
    assert_file_is "$REPO_ROOT/$RETIRED" "$CONTRACT_POINTER"$'\n' "the retired contract"
}

# retired_reference <repo path> <file holding its bytes>: whether that file is a LIVE reference to the
# retired contract. The scan and the self-test below share this one predicate (#476): code, docs,
# mirrors and notes must not point at the retired file; the pointer file itself and handoff history may.
retired_reference() {
    [[ "$1" != "$RETIRED" && "$1" != thoughts/shared/handoffs/* ]] || return 1
    grep -aqF -- "$RETIRED" "$2"
}

case_no_live_file_references_the_retired_contract() {
    local name violations="" scanned=0
    # The file list is captured with a checked status: a failing git must not read as "no violations".
    git -C "$REPO_ROOT" ls-files --cached --others --exclude-standard -z >"$CASE_DIR/files" || tap_fail "git ls-files failed"
    while IFS= read -r -d '' name; do
        [[ -f "$REPO_ROOT/$name" ]] || continue
        scanned=$((scanned + 1))
        if retired_reference "$name" "$REPO_ROOT/$name"; then
            violations="$violations $name"
        fi
    done <"$CASE_DIR/files"
    [[ "$scanned" -gt 100 ]] || tap_fail "only $scanned files were scanned"
    assert_eq "" "$violations" "Repoint references to docs/agents/worker-contract.md"
}

case_the_reference_guard_covers_code_docs_and_mirrors_but_preserves_handoff_history() {
    local path
    printf '%s\n' "$RETIRED" >"$CASE_DIR/reference"
    printf '%s\n' "docs/agents/worker-contract.md" >"$CASE_DIR/canonical"
    for path in src/example.rs docs/example.md thoughts/shared/notes/example.md .claude/commands/example.md \
        .agents/skills/example/SKILL.md .codex/prompts/example.md .gemini/commands/example.toml; do
        retired_reference "$path" "$CASE_DIR/reference" || tap_fail "$path should be guarded"
    done
    ! retired_reference "$RETIRED" "$CASE_DIR/reference" || tap_fail "the pointer file itself is exempt"
    ! retired_reference thoughts/shared/handoffs/general/example.md "$CASE_DIR/reference" || tap_fail "handoff history is preserved"
    # A live path whose bytes hold only the canonical contract is not a reference.
    ! retired_reference docs/example.md "$CASE_DIR/canonical" || tap_fail "the canonical contract path was read as the retired one"
}

case_worker_guidance_reuses_or_isolates_baseline_repairs() {
    local relative text instruction
    for relative in .claude/commands/_shared/worker_preamble.md docs/agents/worker-contract.md; do
        text="$(cat "$REPO_ROOT/$relative")"
        for instruction in \
            "After a red run caused by your change" \
            'first query `AgentQueryFailureSignatures` with the failing test name' \
            'title-search open Issues with `AgentListIssues`' \
            '`title_contains` set to that name' \
            "If a fix exists or is queued, leave the test file untouched" \
            'name its Issue as `fixes_issue: <N>` in the handoff' \
            "If none exists, file one Issue naming the failing test" \
            "separate small, one-file source the manager can land first" \
            "never fold it into the feature branch"; do
            assert_contains "$text" "$instruction" "$relative"
        done
    done
}

run_case "canonical briefs name the typed missing runner" case_canonical_briefs_name_the_typed_missing_runner
run_case "no canonical or rendered brief requires a Python runner" case_no_canonical_or_rendered_brief_requires_a_python_runner
run_case "the runner pattern catches a required Python runner" case_the_runner_pattern_catches_a_required_python_runner
run_case "the retired contract is only a pointer" case_the_retired_contract_is_only_a_pointer
run_case "no live file references the retired contract" case_no_live_file_references_the_retired_contract
run_case "the reference guard covers code, docs and mirrors but preserves handoff history" case_the_reference_guard_covers_code_docs_and_mirrors_but_preserves_handoff_history
run_case "worker guidance reuses or isolates baseline repairs" case_worker_guidance_reuses_or_isolates_baseline_repairs
tap_done
