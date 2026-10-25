#!/usr/bin/env bash
# Minimal TAP harness for the bash residual suites (#366). Source it; do not run it.
#
#   #!/usr/bin/env bash
#   # suite-gate: tap
#   source "$(dirname "$0")/lib/tap.sh"
#   case_a_thing() { assert_eq "want" "$(thing)" "what it printed"; }
#   run_case "a thing works" case_a_thing
#   tap_done
#
# The residual runner (`rsi-scoped-test`, scoped_runner::residual_run) owns the
# verdict: a suite is red when it exits non-zero, prints any `not ok` (numbered or
# not), prints a malformed or incomplete TAP stream (no `1..N` plan, a plan that
# disagrees with the case lines, a case out of sequence, `Bail out!`), or executes no
# case (every case `# SKIP`, or none). Each case runs in its own subshell with its own
# scratch directory; an assertion that fails prints why, marks the case failed (a marker
# file, so a failure inside `$(...)` or a pipeline cannot be outlived by a case that
# returns 0) and leaves the subshell, so one red case never stops the rest.
# Bash 3.2 compatible (macOS /bin/bash); errexit is NOT enabled (it is switched off under
# the OR-lists these helpers run in, and `local x=$(false)` hides it): check statuses with
# `must <cmd>` or an explicit assertion, and compare file bytes with assert_file_is.
set -uo pipefail

# The checkout holding the suite (scripts/tests/lib/tap.sh -> three levels up).
TAP_LIB_DIR="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(CDPATH='' cd -- "$TAP_LIB_DIR/../../.." && pwd)"
SCRIPTS_DIR="$REPO_ROOT/scripts"
# The suite file itself (absolute), for suites that read their own declarations.
SUITE_FILE="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)/$(basename -- "$0")"

# Repository-location variables must never redirect fixture Git (hard rule 2).
unset GIT_DIR GIT_WORK_TREE GIT_COMMON_DIR GIT_INDEX_FILE GIT_OBJECT_DIRECTORY \
    GIT_ALTERNATE_OBJECT_DIRECTORIES GIT_NAMESPACE GIT_PREFIX GIT_CEILING_DIRECTORIES

TAP_COUNT=0
TAP_FAILED=0
TAP_SKIPPED=0
TAP_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/rsi-suite.XXXXXX")" || exit 2
trap 'rm -rf "$TAP_ROOT"' EXIT
# Fixture Git stays in its own repositories: discovery never climbs out of the scratch
# root (a sandbox TMPDIR is inside the checkout), and user/system config is ignored.
TAP_PARENT="$(CDPATH='' cd -- "$(dirname -- "$TAP_ROOT")" && pwd -P)"
export GIT_CEILING_DIRECTORIES="$TAP_PARENT"
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1

# Per-case scratch directory (set by run_case, removed after the case).
CASE_DIR=""
# The failure marker of the running case. `exit` inside `$(...)`, a pipeline or a `( )` group
# leaves only that subshell, and a case body that then goes on (or returns 0) would hide the
# failure; the marker file is how run_case still sees it (#468). Bash 3.2 safe: no errexit.
TAP_MARK=""

tap_fail() { # <message...>: say why, mark the case failed, leave the (sub)shell
    printf 'FAIL: %s\n' "$*" >&2
    if [[ -n "$TAP_MARK" ]]; then
        printf 'FAIL: %s\n' "$*" >>"$TAP_MARK" 2>/dev/null || true
    fi
    exit 1
}

must() { # <command...>: a setup or read step that must succeed (its failure fails the case)
    "$@" || tap_fail "command failed ($?): $*"
}

assert_eq() { # <expected> <actual> [label]
    if [[ "$1" != "$2" ]]; then
        tap_fail "${3:-values differ}: expected [$1], got [$2]"
    fi
}

assert_ne() { # <unwanted> <actual> [label]
    if [[ "$1" == "$2" ]]; then
        tap_fail "${3:-values equal}: did not expect [$1]"
    fi
}

assert_contains() { # <haystack> <needle> [label]
    case "$1" in
        *"$2"*) ;;
        *) tap_fail "${3:-missing text}: [$2] not found in [$1]" ;;
    esac
}

assert_not_contains() { # <haystack> <needle> [label]
    case "$1" in
        *"$2"*) tap_fail "${3:-unexpected text}: [$2] found in [$1]" ;;
        *) ;;
    esac
}

# offset <var> <text> <needle>: the byte offset of the first occurrence, stored in <var>. No command
# substitution: a missing needle fails THIS case (a $(...) subshell's exit would be lost and the
# empty value read as arithmetic zero, passing every ordering check; #468, #471).
offset() {
    local __prefix="${2%%"$3"*}"
    [[ "$__prefix" != "$2" ]] || tap_fail "[$3] not found in [$2]"
    printf -v "$1" '%s' "${#__prefix}"
}

# assert_before <text> <earlier> <later> [label]: both exist in the text and the first comes first.
assert_before() {
    local early late
    offset early "$1" "$2"
    offset late "$1" "$3"
    [[ "$early" -lt "$late" ]] || tap_fail "${4:-order}: [$2] (at $early) is not before [$3] (at $late)"
}

# toml_get <file> <table> <key>: the value of an ACTIVE `key = value` line directly in `[table]`
# (a commented line or a key in another table never counts; a repeated key is a TOML error).
# Single-line strings and scalars only: a quoted string is returned unquoted. The value must be
# WELL FORMED (#481): a basic or literal string needs its closing quote and nothing but
# whitespace or a comment after it, and a bare value must be a plain scalar word; anything else
# fails the case instead of returning half a value. A table header may carry a trailing comment.
# Fails the case when the table or key is absent, repeated or malformed.
toml_get() {
    local out
    [[ -f "$1" ]] || tap_fail "toml file missing: $1"
    out="$(awk -v table="$2" -v key="$3" -v sq="'" '
        function bad(why) { problem = why; exit 2 }
        /^[ \t]*\[/ {
            line = $0; gsub(/^[ \t]+|[ \t]+$/, "", line)
            on = 0
            if (line ~ /^\[\[/) next
            sub(/[ \t]*#.*$/, "", line)
            on = (line == "[" table "]")
            next
        }
        on && /^[ \t]*#/ { next }
        on {
            line = $0; sub(/^[ \t]+/, "", line)
            if (index(line, key) == 1) {
                rest = substr(line, length(key) + 1)
                if (rest ~ /^[ \t]*=/) {
                    sub(/^[ \t]*=[ \t]*/, "", rest)
                    if (rest ~ /^"/) {
                        if (!match(rest, /^"([^"\\]|\\.)*"/)) bad("unterminated string in [" table "] " key)
                        value = substr(rest, 2, RLENGTH - 2); tail = substr(rest, RLENGTH + 1)
                        if (tail !~ /^[ \t]*(#.*)?$/) bad("text after the closing quote in [" table "] " key)
                    } else if (substr(rest, 1, 1) == sq) {
                        close_at = index(substr(rest, 2), sq)
                        if (close_at == 0) bad("unterminated literal string in [" table "] " key)
                        value = substr(rest, 2, close_at - 1); tail = substr(rest, close_at + 2)
                        if (tail !~ /^[ \t]*(#.*)?$/) bad("text after the closing quote in [" table "] " key)
                    } else {
                        value = rest; sub(/[ \t]*#.*$/, "", value); sub(/[ \t]+$/, "", value)
                        if (value !~ /^[-+0-9A-Za-z_.:]+$/) bad("not a plain scalar in [" table "] " key ": " value)
                    }
                    print value; n++
                }
            }
        }
        END {
            if (problem != "") { print "toml: " problem > "/dev/stderr"; exit 2 }
            if (n != 1) exit 1
        }' "$1")" || tap_fail "[$2] $3 is absent, repeated or malformed in $1"
    printf '%s' "$out"
}

assert_file() { # <path> [label]
    [[ -f "$1" ]] || tap_fail "${2:-file missing}: $1"
}

assert_no_file() { # <path> [label]
    [[ ! -e "$1" ]] || tap_fail "${2:-file unexpectedly present}: $1"
}

# Byte-exact file comparison: `$(cat file)` strips trailing newlines, so a missing final newline
# or an extra blank tail would pass an assert_eq on it (#474). cmp compares every byte.
assert_file_is() { # <path> <expected bytes> [label]
    [[ -f "$1" ]] || tap_fail "${3:-file bytes}: $1 is missing"
    printf '%s' "$2" | cmp -s - "$1" || tap_fail "${3:-file bytes differ}: $1 holds [$(cat -- "$1" | od -c | head -c 400)], expected [$2]"
}

# The exact bytes of the last run_capture's stdout / stderr (RUN_OUT strips trailing newlines).
assert_stdout_is() { # <expected bytes> [label]
    assert_file_is "$CASE_DIR/.run.out" "$1" "${2:-stdout}"
}

assert_stderr_is() { # <expected bytes> [label]
    assert_file_is "$CASE_DIR/.run.err" "$1" "${2:-stderr}"
}

# Directory listings: `$(ls -A missing)` is empty, so an I/O failure would satisfy an "is empty" check.
assert_dir_empty() { # <dir> [label]: the directory exists and holds nothing (dotfiles included)
    [[ -d "$1" ]] || tap_fail "${2:-directory}: $1 does not exist"
    local listing
    listing="$(ls -A -- "$1")" || tap_fail "${2:-directory}: cannot list $1"
    [[ -z "$listing" ]] || tap_fail "${2:-directory} is not empty: [$listing]"
}

assert_dir_only() { # <dir> <the one expected entry> [label]
    [[ -d "$1" ]] || tap_fail "${3:-directory}: $1 does not exist"
    local listing
    listing="$(ls -A -- "$1")" || tap_fail "${3:-directory}: cannot list $1"
    assert_eq "$2" "$listing" "${3:-directory contents}"
}

# find_named <dir> <name>: the paths of that name under the directory; the directory MUST exist.
find_named() {
    [[ -d "$1" ]] || tap_fail "directory missing: $1"
    find "$1" -name "$2" || tap_fail "find failed in $1"
}

assert_same_bytes() { # <path a> <path b> [label]
    [[ -f "$1" && -f "$2" ]] || tap_fail "${3:-files}: $1 or $2 is missing"
    cmp -s "$1" "$2" || tap_fail "${3:-file bytes differ}: $1 and $2"
}

# Read a file's exact bytes (trailing newlines kept) into the named variable.
read_exact() { # <var> <path>
    local __rx
    [[ -f "$2" ]] || tap_fail "cannot read $2"
    # The sentinel is appended only when cat succeeded: `cat; printf x` would turn a failed read
    # into an empty value with status 0 (#480).
    __rx="$(cat -- "$2" && printf x)" || tap_fail "cannot read $2"
    printf -v "$1" '%s' "${__rx%x}"
}

assert_executable() { # <path>
    [[ -x "$1" ]] || tap_fail "not executable: $1"
}

# Run a command capturing stdout, stderr (as one stream each) and the exit code
# into RUN_OUT, RUN_ERR, RUN_CODE. Never trips the caller on a non-zero exit.
run_capture() { # <command...>
    local out_file="$CASE_DIR/.run.out" err_file="$CASE_DIR/.run.err"
    RUN_CODE=0
    "$@" >"$out_file" 2>"$err_file" || RUN_CODE=$?
    RUN_OUT="$(cat "$out_file")" || tap_fail "cannot read the captured stdout $out_file"
    RUN_ERR="$(cat "$err_file")" || tap_fail "cannot read the captured stderr $err_file"
}

assert_code() { # <expected exit code> [label]: of the last run_capture
    if [[ "$RUN_CODE" != "$1" ]]; then
        tap_fail "${2:-exit code}: expected $1, got $RUN_CODE (stdout [$RUN_OUT] stderr [$RUN_ERR])"
    fi
}

write_file() { # <path> <content>: parents created
    mkdir -p "$(dirname -- "$1")"
    printf '%s' "$2" >"$1"
}

write_exec() { # <path> <content>
    write_file "$1" "$2"
    chmod +x "$1"
}

# A throwaway Git repository with one commit, identity set, never the checkout.
fixture_repo() { # <dir>
    mkdir -p "$1"
    git -C "$1" init -q -b main
    [[ "$(git -C "$1" rev-parse --show-toplevel)" == "$(CDPATH='' cd -- "$1" && pwd -P)" ]] \
        || tap_fail "fixture repository $1 is not its own work tree"
    git -C "$1" config user.name tester
    git -C "$1" config user.email tester@example.invalid
    git -C "$1" config commit.gpgsign false
}

fixture_commit() { # <dir> <message>: stage everything, commit
    git -C "$1" add -A
    git -C "$1" commit -q --no-verify -m "$2"
}

run_case() { # <name> <function>
    local name="$1" fn="$2" output rc
    # Debugging aid for a person at a keyboard: only cases whose name contains $TAP_SELECT.
    case "$name" in *"${TAP_SELECT:-}"*) ;; *) return 0 ;; esac
    TAP_COUNT=$((TAP_COUNT + 1))
    CASE_DIR="$TAP_ROOT/case-$TAP_COUNT"
    TAP_MARK="$TAP_ROOT/failed-$TAP_COUNT"
    rm -f "$TAP_MARK"
    mkdir -p "$CASE_DIR"
    rc=0
    output="$( (cd "$CASE_DIR" && "$fn") 2>&1)" || rc=$?
    # Red on a non-zero exit AND on a marker: a failure inside a subshell the case outlived.
    if [[ "$rc" == 0 && ! -e "$TAP_MARK" ]]; then
        printf 'ok %s - %s\n' "$TAP_COUNT" "$name"
    else
        TAP_FAILED=$((TAP_FAILED + 1))
        printf 'not ok %s - %s\n' "$TAP_COUNT" "$name"
        printf '%s\n' "$output" | sed 's/^/# /'
        if [[ "$rc" == 0 ]]; then
            printf '# case returned 0 after a failed assertion in a subshell (see FAIL above)\n'
        fi
    fi
    TAP_MARK=""
    rm -rf "$CASE_DIR"
}

# A case this host cannot run (a platform feature): reported, never counted as executed.
# The runner records it in `skipped_tests`; a suite whose every case skipped is RED
# (`no_tests_executed`), so a platform-only suite keeps at least one case that runs everywhere.
skip_case() { # <name> <reason>
    TAP_COUNT=$((TAP_COUNT + 1))
    TAP_SKIPPED=$((TAP_SKIPPED + 1))
    printf 'ok %s - %s # SKIP %s\n' "$TAP_COUNT" "$1" "$(printf '%s' "$2" | tr '\n' ' ')"
}

# A case that needs a Linux feature (systemd, /proc, GNU userland); the reason names the
# hub Linux checklist so the skip is traceable (docs/agents/python-free-runner-bootstrap.md).
skip_linux_only() { # <name> <what it needs>
    skip_case "$1" "Linux-only, NOT run on $(uname -s): needs $2; hub #3621 Linux checklist item"
}

tap_done() {
    printf '1..%s\n' "$TAP_COUNT"
    if [[ $((TAP_COUNT - TAP_SKIPPED)) -le 0 ]]; then
        echo "# no executed case (cases $TAP_COUNT, skipped $TAP_SKIPPED): a suite that proved nothing is red" >&2
        exit 1
    fi
    [[ "$TAP_FAILED" == 0 ]]
}
