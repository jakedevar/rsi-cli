#!/usr/bin/env bash
# suite-gate: tap
# suite-gate: scans scripts/tests
# No scripts/tests fixture runs Git on an implicit working directory (#2574, #225).
#
# A fixture that runs `git commit` after `cd <scratch> &&` acts on whatever directory it is in
# when the `cd` or the setup went wrong. In an RSI sandbox that is the worker's own assigned
# worktree, and a stray `git commit` or `git reset` there moves the assigned branch
# (AGENTS.md hard rule 2). Shell fixtures use `git -C "$REPO"` (the harness `fixture_repo`
# also checks the repository is its own work tree and fences Git discovery).
#
# This suite scans every `scripts/tests/*.sh` with `lib/fixture_git_scan.awk` and rejects the
# four unsafe shapes named there. Only executable lines are inspected: comments and heredoc
# bodies are inert. A line that must stay an exception ends with
# `# fixture-git: allow <reason>`; the reason is mandatory and the list is asserted.
# Port of the Python suite of the same name (#366). The Python-source half of that scanner (the
# `ast` checks of `.py` fixtures) is native: `crates/rsi-repo-guards/src/fixture_git_py.rs`
# tokenizes the still-committed `.py` fixtures as TEXT (no interpreter) and its unit tests are
# the thirteen Python-specific legacy cases. Case map (legacy test_fixture_git_guard.py ->):
#   every_scripts_tests_fixture_runs_git_in_an_explicit_validated_repository -> case 1 here (*.sh) AND
#       fixture_git_py::tests::every_python_fixture_runs_git_in_an_explicit_validated_repository (*.py)
#   the_scan_covers_the_fixture_modules_that_run_git        -> case 2 (shell fixtures; the Python fixture
#       inventory is asserted by the Rust test above, which requires test_scratch_git.py while it exists)
#   every_allowance_names_its_reason                        -> case 3 here (.py AND .sh) + the Rust allowance test
#   the_only_allowance_is_the_scratch_git_helper_test_on_its_fake_assigned_repo -> case 4 here + Rust test
#       (an EXACT singleton in either language; an empty inventory fails)
#   bare_subprocess_git_with_cwd_is_rejected .. an_allow_pragma_with_a_reason_exempts_only_its_line (13 Python
#       cases) -> the same-named tests in fixture_git_py (rsi-repo-guards)
#   shell_fixture_bare_git_and_cd_chains_are_rejected       -> case 5; shell_fixture_mktemp_* -> case 6;
#       shell_fixture_explicit_repo_and_inert_text_are_accepted -> case 7; (pragma) -> case 8
# Run the Rust half as an exact job: package rsi-repo-guards, filters `fixture_git_py::tests::...`.
source "$(dirname "$0")/lib/tap.sh"

SCAN="$TAP_LIB_DIR/fixture_git_scan.awk"

# scan <body>: the scanner's report for a fixture with that body.
scan() {
    # A scanner that cannot run must not read as "no violations" (accepted inputs expect "").
    printf '%s\n' "$1" | awk -f "$SCAN" || tap_fail "the scanner failed"
}

count() { # <report>: number of violations
    if [[ -z "$1" ]]; then echo 0; else printf '%s\n' "$1" | wc -l | tr -d ' '; fi
}

case_every_shell_fixture_runs_git_in_an_explicit_validated_repository() {
    local file report="" one scanned=0
    for file in "$SCRIPTS_DIR"/tests/*.sh; do
        # This suite holds sample fixture bodies in strings, which no line scanner can tell
        # from code; it runs no Git of its own.
        [[ "$file" == "$SUITE_FILE" ]] && continue
        one="$(awk -f "$SCAN" "$file")" || tap_fail "the scanner failed on $file"
        scanned=$((scanned + 1))
        [[ -z "$one" ]] || report="$report$(basename "$file"): $one"$'\n'
    done
    [[ "$scanned" -ge 10 ]] || tap_fail "only $scanned shell fixtures were scanned"
    assert_eq "" "$report" "unsafe fixture Git (use git -C <dir>)"
}

case_the_scan_covers_the_fixture_modules_that_run_git() {
    local required
    for required in test_rsi_stash.sh check-corpus-pins.test.sh test_fmt_changed.sh test_check_merge_whitespace.sh test_cloud_sweep_origins.sh; do
        assert_file "$SCRIPTS_DIR/tests/$required" "the scanned fixture $required"
    done
}

# Every `# fixture-git: allow <reason>` pragma of a .py or .sh fixture (this suite's own sample
# strings excluded), one `file:line:reason` per line. The pattern is the scanner's: the reason must
# start right after `allow` and whitespace, so a bare `allow` is not an allowance.
allowance_lines() {
    local file
    for file in "$SCRIPTS_DIR"/tests/*.py "$SCRIPTS_DIR"/tests/*.sh; do
        [[ -f "$file" && "$file" != "$SUITE_FILE" ]] || continue
        grep -nE '#[[:space:]]*fixture-git:[[:space:]]*allow[[:space:]]+[^[:space:]]' "$file" 2>/dev/null \
            | sed "s|^|$(basename "$file"):|"
    done
}

case_every_allowance_names_its_reason() {
    local line reason words seen=0
    allowance_lines >"$CASE_DIR/allowances"
    while IFS= read -r line; do
        seen=$((seen + 1))
        reason="${line#*fixture-git: allow }"
        words="$(printf '%s\n' "$reason" | wc -w | tr -d ' ')"
        [[ "$words" -ge 3 ]] || tap_fail "an allowance needs a real reason (3+ words): [$line]"
    done <"$CASE_DIR/allowances"
    [[ "$seen" -ge 1 ]] || tap_fail "no allowance was inspected: the inventory scan is vacuous"
}

# scratch_git refuses a protected root by design, and test_scratch_git builds that root on
# purpose; no other fixture needs an exception. EXACTLY that one file, in either language: an
# empty or larger inventory fails (the legacy test compared the list for equality).
case_the_only_allowance_is_the_scratch_git_helper_test_on_its_fake_assigned_repo() {
    local files
    allowance_lines >"$CASE_DIR/allowances"
    files="$(cut -d: -f1 "$CASE_DIR/allowances" | sort -u | tr '\n' ' ')"
    case "$files" in
        "test_scratch_git.py " | "test_scratch_git.sh ") ;;
        *) tap_fail "the allowance inventory must be exactly the scratch_git helper test, got [$files]" ;;
    esac
}

case_bare_git_and_cd_chains_are_rejected() {
    local body
    for body in 'cd "$REPO" && git commit -qm x' 'git init -q' 'GIT() { git "$@"; }' 'x="$(git rev-parse HEAD)"' 'cd "$REPO"; git add .'; do
        assert_eq 1 "$(count "$(scan "$body")")" "violations in [$body]"
    done
}

case_mktemp_without_a_directory_is_rejected_when_the_fixture_runs_git() {
    assert_eq 1 "$(count "$(scan $'TMP="$(mktemp -d)"\ngit -C "$TMP" init')")" "mktemp then git"
    assert_eq 0 "$(count "$(scan $'TMP="$(mktemp -d)"\necho hi')")" "mktemp without git"
}

case_explicit_repo_and_inert_text_are_accepted() {
    local body
    body=$'TMP="$(mktemp -d "$parent/fixture.XXXXXX")"\nGIT() { git -C "$REPO" "$@"; }\nGIT init -q\ngit -C "$REPO" log --oneline\n# cd x && git commit is only a comment\ncat > file <<\'EOF\'\ncd x && git commit\ngit add .\nEOF\necho "git add is just text"'
    assert_eq "" "$(scan "$body")" "report"
}

case_an_allow_pragma_with_a_reason_exempts_only_its_line() {
    local body
    body=$'git status  # fixture-git: allow product wrapper reads the caller cwd\ngit status'
    assert_eq "2: shell Git without \`-C\` runs on an implicit working directory" "$(scan "$body")" "report"
    body=$'# fixture-git: allow product wrapper reads the caller cwd\ngit status\ngit status'
    assert_eq "3: shell Git without \`-C\` runs on an implicit working directory" "$(scan "$body")" "report"
}

run_case "every shell fixture runs Git in an explicit validated repository" case_every_shell_fixture_runs_git_in_an_explicit_validated_repository
run_case "the scan covers the fixture modules that run Git" case_the_scan_covers_the_fixture_modules_that_run_git
run_case "every allowance names its reason" case_every_allowance_names_its_reason
run_case "the only allowance is the scratch_git helper test on its fake assigned repo" case_the_only_allowance_is_the_scratch_git_helper_test_on_its_fake_assigned_repo
run_case "bare git and cd chains are rejected" case_bare_git_and_cd_chains_are_rejected
run_case "mktemp without a directory is rejected when the fixture runs git" case_mktemp_without_a_directory_is_rejected_when_the_fixture_runs_git
run_case "explicit repo and inert text are accepted" case_explicit_repo_and_inert_text_are_accepted
run_case "an allow pragma with a reason exempts only its line" case_an_allow_pragma_with_a_reason_exempts_only_its_line
tap_done
