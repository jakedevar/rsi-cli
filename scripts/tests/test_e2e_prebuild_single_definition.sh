#!/usr/bin/env bash
# suite-gate: tap
# The e2e prebuild binary list has exactly one definition (#1651, #1637).
#
# scripts/e2e-prebuild-bins.txt names every binary the e2e_tui harness needs; the Rust
# scoped runner, the Python scoped-test still in transit, rsi-rolling-land and
# scripts/e2e.sh must read it instead of spelling their own list, and it must carry
# rsi-agent-mcp. Port of the Python suite of the same name (#488, umbrella #366).
#
# Every guard is a `check_*` function over a repository root. Each case runs it on the real
# checkout (must pass) and on a scratch tree built to break exactly that guard (must fail
# with the reason), so the suite is red when the subject is broken.
source "$(dirname "$0")/lib/tap.sh"

LIST_REL="scripts/e2e-prebuild-bins.txt"
LANDER_REL="crates/rsid/src/bin/rsi-rolling-land.rs"
PLANNER_REL="crates/rsi-repo-guards/src/scoped_runner/planner.rs"
RUNNER_REL="crates/rsi-repo-guards/src/scoped_runner/runner.rs"
REQUIRED_BINARIES="rsid rsi-agent-mcp rsi-rpc rsi-turn-shim"

# pairs <root>: the "PACKAGE BINARY" pairs of the shared list, one per line. The grammar of
# the Rust planner (`e2e_prebuild_pairs`): `#` starts a comment, blank lines are skipped, and
# anything but exactly two words is malformed (exit 2, as the planner refuses it). An empty
# list is an error too: a definition that builds nothing proves nothing.
pairs() {
    local list="$1/$LIST_REL"
    [[ -f "$list" ]] || { echo "missing $LIST_REL" >&2; return 2; }
    awk '
        { sub(/#.*/, ""); gsub(/^[ \t]+|[ \t]+$/, "") }
        $0 == "" { next }
        NF != 2 { printf "malformed prebuild line %d: [%s]\n", NR, $0 > "/dev/stderr"; bad = 1; next }
        { print $1, $2; n++ }
        END { if (bad) exit 2; if (n == 0) { print "no prebuild pairs" > "/dev/stderr"; exit 2 } }
    ' "$list"
}

# file_has <path> <fixed text>: the file exists and contains the text.
file_has() {
    [[ -f "$1" ]] || { echo "missing file $1" >&2; return 2; }
    grep -qF -- "$2" "$1"
}

# --- the guards -------------------------------------------------------------------------

check_required_siblings() { # <root>
    local list required
    list="$(pairs "$1")" || return 2
    for required in $REQUIRED_BINARIES; do
        printf '%s\n' "$list" | awk -v want="$required" '$2 == want { found = 1 } END { exit !found }' \
            || { echo "the list does not name the binary $required" >&2; return 1; }
    done
}

check_binaries_exist() { # <root>
    local list package binary crate
    list="$(pairs "$1")" || return 2
    while read -r package binary; do
        crate="$1/crates/$package"
        if [[ -f "$crate/src/bin/$binary.rs" ]] || { [[ -f "$crate/src/main.rs" ]] && [[ "$binary" == "$package" ]]; }; then
            continue
        fi
        echo "$package:$binary is not a binary of that package" >&2
        return 1
    done <<EOF
$list
EOF
}

# e2e.sh is the executable consumer of the list: run it against a recording `cargo` and demand
# the prebuild call it makes is exactly one `build`: each package once (`-p`, at its first pair) and
# each binary (`--bin`) in list order.
check_e2e_script_builds_every_binary() { # <root>
    local root="$1" list expected packages="" line package binary got bin="$CASE_DIR/fakebin" log="$CASE_DIR/cargo.log"
    list="$(pairs "$root")" || return 2
    [[ -f "$root/scripts/e2e.sh" ]] || { echo "missing scripts/e2e.sh" >&2; return 2; }
    mkdir -p "$bin" || return 2
    : >"$log" || return 2
    printf '#!/bin/sh\nprintf "%%s\\n" "$*" >>"%s"\n' "$log" >"$bin/cargo" || return 2
    chmod +x "$bin/cargo" || return 2
    PATH="$bin:$PATH" bash "$root/scripts/e2e.sh" --cargo-test test -p rsi --test e2e_tui >/dev/null 2>"$CASE_DIR/e2e.err" \
        || { echo "e2e.sh failed: $(cat "$CASE_DIR/e2e.err")" >&2; return 2; }
    expected="build"
    while read -r package binary; do
        case " $packages " in *" $package "*) ;; *) packages="$packages $package"; expected="$expected -p $package" ;; esac
        expected="$expected --bin $binary"
    done <<EOF
$list
EOF
    got="$(sed -n 2p "$log")" || return 2
    if [[ "$got" != "$expected" ]]; then
        echo "e2e.sh prebuild call differs: expected [$expected], got [$got]" >&2
        return 1
    fi
    line="$(sed -n 1p "$log")" || return 2
    [[ "$line" == "build -p rsi --bin rsi" ]] || { echo "e2e.sh first build is [$line]" >&2; return 1; }
    line="$(sed -n 3p "$log")" || return 2
    [[ "$line" == "test -p rsi --test e2e_tui" ]] || { echo "e2e.sh test call is [$line]" >&2; return 1; }
}

# The Rust scoped runner reads the list and builds the command from it with the planner; neither
# file spells a sibling binary of its own (the planner's test module is excluded: it uses fixtures).
check_rust_runner_reads_the_shared_list() { # <root>
    local root="$1" name production
    file_has "$root/$RUNNER_REL" 'work.join("scripts/e2e-prebuild-bins.txt")' || { echo "runner.rs does not read the shared list" >&2; return 1; }
    file_has "$root/$RUNNER_REL" "planner::e2e_prebuild_command(" || { echo "runner.rs does not build the prebuild with the planner" >&2; return 1; }
    file_has "$root/$PLANNER_REL" "pub fn e2e_prebuild_command(" || { echo "planner.rs lost e2e_prebuild_command" >&2; return 1; }
    production="$CASE_DIR/planner-production.rs"
    awk '/^#\[cfg\(test\)\]/ { exit } { print }' "$root/$PLANNER_REL" >"$production" || return 2
    [[ -s "$production" ]] || { echo "planner.rs has no production part" >&2; return 2; }
    for name in rsi-agent-mcp rsi-rpc rsi-turn-shim; do
        if grep -qF -- "$name" "$production"; then echo "planner.rs spells the binary $name" >&2; return 1; fi
        if grep -qF -- "$name" "$root/$RUNNER_REL"; then echo "runner.rs spells the binary $name" >&2; return 1; fi
    done
}

check_lander_reads_the_shared_list() { # <root>
    local lander="$1/$LANDER_REL" body="$CASE_DIR/lander-body.rs"
    file_has "$lander" 'include_str!("../../../../scripts/e2e-prebuild-bins.txt")' \
        || { echo "the lander does not include the shared list" >&2; return 1; }
    # From `fn e2e_prebuild_command` to the next `fn cargo_guard_command`; both must exist.
    awk '
        !started && index($0, "fn e2e_prebuild_command") { started = 1 }
        started && index($0, "fn cargo_guard_command") { ended = 1; exit }
        started { print }
        END { exit !(started && ended) }
    ' "$lander" >"$body" || { echo "the lander has no e2e_prebuild_command ... cargo_guard_command region" >&2; return 2; }
    file_has "$body" "e2e_prebuild_bins(E2E_PREBUILD_LIST)" || { echo "the lander prebuild does not use the shared list" >&2; return 1; }
    if grep -qF -- "rsi-agent-mcp" "$body"; then echo "the lander prebuild spells rsi-agent-mcp" >&2; return 1; fi
    if grep -qF -- '"rsid"' "$body"; then echo "the lander prebuild spells \"rsid\"" >&2; return 1; fi
}

check_scripts_have_no_literal_list() { # <root>
    local file
    for file in scripts/scoped-test scripts/e2e.sh; do
        [[ -f "$1/$file" ]] || { echo "missing $file" >&2; return 2; }
        if grep -qF -- "rsi-agent-mcp" "$1/$file"; then echo "$file spells rsi-agent-mcp" >&2; return 1; fi
        grep -qF -- "e2e-prebuild-bins.txt" "$1/$file" || { echo "$file does not read e2e-prebuild-bins.txt" >&2; return 1; }
    done
}

# --- helpers ----------------------------------------------------------------------------

# passes <check> [args]: the check holds on the real checkout, silently.
passes() {
    run_capture "$@"
    assert_code 0 "$1 on the real checkout (stderr: $RUN_ERR)"
    assert_stderr_is "" "$1 stderr"
}

# fails_with <code> <reason text> <check> [args]: the check refuses the scratch tree.
fails_with() {
    local code="$1" reason="$2"
    shift 2
    run_capture "$@"
    assert_code "$code" "$1 on the broken tree"
    assert_contains "$RUN_ERR" "$reason" "$1 reason"
}

# A scratch tree holding the real list plus empty stubs of every listed binary's source file.
scratch_tree() { # <dir>
    local dir="$1" package binary list
    mkdir -p "$dir/scripts" || tap_fail "mkdir $dir"
    must cp "$REPO_ROOT/$LIST_REL" "$dir/$LIST_REL"
    list="$(pairs "$REPO_ROOT")" || tap_fail "cannot read the real list"
    while read -r package binary; do
        write_file "$dir/crates/$package/src/bin/$binary.rs" "fn main() {}
"
    done <<EOF
$list
EOF
}

# --- cases ------------------------------------------------------------------------------

case_the_list_contains_the_daemon_siblings_the_harness_needs() {
    passes check_required_siblings "$REPO_ROOT"
    # Negative: the list without rsi-agent-mcp (a sibling every session launch needs).
    local tree="$CASE_DIR/t"
    scratch_tree "$tree"
    must grep -vF "rsi-agent-mcp" "$REPO_ROOT/$LIST_REL" >"$tree/$LIST_REL.new"
    mv "$tree/$LIST_REL.new" "$tree/$LIST_REL" || tap_fail "mv"
    fails_with 1 "the list does not name the binary rsi-agent-mcp" check_required_siblings "$tree"
}

case_the_list_grammar_is_package_space_binary() {
    # The real list parses; a three-word or one-word line is malformed, as the planner refuses it.
    run_capture pairs "$REPO_ROOT"
    assert_code 0 "the real list parses (stderr: $RUN_ERR)"
    assert_contains "$RUN_OUT" "rsid rsid" "the rsid pair"
    local tree="$CASE_DIR/t"
    mkdir -p "$tree/scripts"
    write_file "$tree/$LIST_REL" "rsid rsid
one two three
"
    fails_with 2 "malformed prebuild line 2: [one two three]" pairs "$tree"
    write_file "$tree/$LIST_REL" "rsid
"
    fails_with 2 "malformed prebuild line 1: [rsid]" pairs "$tree"
    write_file "$tree/$LIST_REL" "# only a comment
"
    fails_with 2 "no prebuild pairs" pairs "$tree"
    # A comment after a pair and a commented-out pair: only the live pair counts.
    write_file "$tree/$LIST_REL" "# rsid ghost
rsid rsid # note
"
    run_capture pairs "$tree"
    assert_code 0 "comments parse (stderr: $RUN_ERR)"
    assert_stdout_is "rsid rsid
" "exactly the live pair"
}

case_every_listed_binary_exists_in_its_package() {
    passes check_binaries_exist "$REPO_ROOT"
    # Negative: a pair naming a binary the package does not have.
    local tree="$CASE_DIR/t"
    scratch_tree "$tree"
    write_file "$tree/$LIST_REL" "rsid rsid
rsi-common rsi-ghost
"
    write_file "$tree/crates/rsid/src/main.rs" "fn main() {}
"
    fails_with 1 "rsi-common:rsi-ghost is not a binary of that package" check_binaries_exist "$tree"
    # A main.rs only counts for the package's own binary.
    write_file "$tree/$LIST_REL" "rsid rsi-other
"
    fails_with 1 "rsid:rsi-other is not a binary of that package" check_binaries_exist "$tree"
}

case_the_e2e_script_builds_every_listed_binary() {
    passes check_e2e_script_builds_every_binary "$REPO_ROOT"
    # Negative 1: an e2e.sh that ignores the list and builds a hard-coded subset.
    local tree="$CASE_DIR/t"
    scratch_tree "$tree"
    write_exec "$tree/scripts/e2e.sh" '#!/usr/bin/env bash
cargo build -p rsi --bin rsi
cargo build -p rsid --bin rsid
exec env RSI_E2E=1 cargo "$@"
'
    fails_with 1 "e2e.sh prebuild call differs" check_e2e_script_builds_every_binary "$tree"
    # Negative 2: the real e2e.sh over a list with a different pair builds that pair, so the
    # script follows the list rather than a copy (the check passes on the edited list).
    must cp "$REPO_ROOT/scripts/e2e.sh" "$tree/scripts/e2e.sh"
    write_file "$tree/$LIST_REL" "rsid rsid
rsi-common rsi-rpc
"
    passes check_e2e_script_builds_every_binary "$tree"
    assert_eq "build -p rsid --bin rsid -p rsi-common --bin rsi-rpc" "$(sed -n 2p "$CASE_DIR/cargo.log")" "the prebuild follows the edited list"
}

case_the_rust_runner_reads_the_shared_list() {
    passes check_rust_runner_reads_the_shared_list "$REPO_ROOT"
    local tree="$CASE_DIR/t"
    mkdir -p "$tree/$(dirname "$RUNNER_REL")"
    # Negative 1: a runner that does not read the list.
    write_file "$tree/$RUNNER_REL" 'fn plan() { planner::e2e_prebuild_command(&cargo, text); }
'
    must cp "$REPO_ROOT/$PLANNER_REL" "$tree/$PLANNER_REL"
    fails_with 1 "runner.rs does not read the shared list" check_rust_runner_reads_the_shared_list "$tree"
    # Negative 2: a planner that spells a sibling of its own.
    write_file "$tree/$RUNNER_REL" 'fn plan() { read(work.join("scripts/e2e-prebuild-bins.txt")); planner::e2e_prebuild_command(&cargo, text); }
'
    write_file "$tree/$PLANNER_REL" 'pub fn e2e_prebuild_command(c: &[String]) { push("--bin", "rsi-agent-mcp"); }
#[cfg(test)]
mod tests {}
'
    fails_with 1 "planner.rs spells the binary rsi-agent-mcp" check_rust_runner_reads_the_shared_list "$tree"
    # The same name in the planner's test module is a fixture, not a copy of the definition.
    write_file "$tree/$PLANNER_REL" 'pub fn e2e_prebuild_command(c: &[String]) {}
#[cfg(test)]
mod tests { const FIXTURE: &str = "rsi-agent-mcp"; }
'
    passes check_rust_runner_reads_the_shared_list "$tree"
}

case_the_lander_reads_the_shared_list_and_has_no_literal_copy() {
    passes check_lander_reads_the_shared_list "$REPO_ROOT"
    local tree="$CASE_DIR/t" lander
    lander="$tree/$LANDER_REL"
    mkdir -p "$tree/$(dirname "$LANDER_REL")"
    # Negative 1: the include is gone.
    write_file "$lander" 'const E2E_PREBUILD_LIST: &str = "rsid rsid";
fn e2e_prebuild_command() { e2e_prebuild_bins(E2E_PREBUILD_LIST); }
fn cargo_guard_command() {}
'
    fails_with 1 "the lander does not include the shared list" check_lander_reads_the_shared_list "$tree"
    # Negative 2: a literal copy of a sibling inside the prebuild function.
    write_file "$lander" 'const L: &str = include_str!("../../../../scripts/e2e-prebuild-bins.txt");
fn e2e_prebuild_command() { e2e_prebuild_bins(E2E_PREBUILD_LIST); args.push("rsi-agent-mcp"); }
fn cargo_guard_command() {}
'
    fails_with 1 "the lander prebuild spells rsi-agent-mcp" check_lander_reads_the_shared_list "$tree"
    # Negative 3: a literal "rsid".
    write_file "$lander" 'const L: &str = include_str!("../../../../scripts/e2e-prebuild-bins.txt");
fn e2e_prebuild_command() { e2e_prebuild_bins(E2E_PREBUILD_LIST); args.push("rsid"); }
fn cargo_guard_command() {}
'
    fails_with 1 'the lander prebuild spells "rsid"' check_lander_reads_the_shared_list "$tree"
    # Negative 4: the region is not found (the function was renamed): refused, never read as empty.
    write_file "$lander" 'const L: &str = include_str!("../../../../scripts/e2e-prebuild-bins.txt");
fn renamed() {}
'
    fails_with 2 "the lander has no e2e_prebuild_command" check_lander_reads_the_shared_list "$tree"
    # A prebuild that is a literal copy outside the region is not this guard's concern; one that
    # reads the list passes.
    write_file "$lander" 'const L: &str = include_str!("../../../../scripts/e2e-prebuild-bins.txt");
fn e2e_prebuild_command() { e2e_prebuild_bins(E2E_PREBUILD_LIST); }
fn cargo_guard_command() { "rsi-agent-mcp"; "rsid"; }
'
    passes check_lander_reads_the_shared_list "$tree"
}

case_scoped_test_and_the_e2e_script_have_no_literal_binary_list() {
    passes check_scripts_have_no_literal_list "$REPO_ROOT"
    local tree="$CASE_DIR/t"
    mkdir -p "$tree/scripts"
    # Negative 1: a literal sibling in scoped-test.
    write_file "$tree/scripts/scoped-test" 'x = ["rsi-agent-mcp"]  # e2e-prebuild-bins.txt
'
    write_file "$tree/scripts/e2e.sh" 'read e2e-prebuild-bins.txt
'
    fails_with 1 "scripts/scoped-test spells rsi-agent-mcp" check_scripts_have_no_literal_list "$tree"
    # Negative 2: e2e.sh no longer reads the list.
    write_file "$tree/scripts/scoped-test" 'read e2e-prebuild-bins.txt
'
    write_file "$tree/scripts/e2e.sh" 'cargo build --bin rsid
'
    fails_with 1 "scripts/e2e.sh does not read e2e-prebuild-bins.txt" check_scripts_have_no_literal_list "$tree"
    # Negative 3: a missing file is refused, not read as clean.
    must rm "$tree/scripts/e2e.sh"
    fails_with 2 "missing scripts/e2e.sh" check_scripts_have_no_literal_list "$tree"
}

run_case "the list contains the daemon siblings the harness needs" case_the_list_contains_the_daemon_siblings_the_harness_needs
run_case "the list grammar is PACKAGE BINARY per line" case_the_list_grammar_is_package_space_binary
run_case "every listed binary exists in its package" case_every_listed_binary_exists_in_its_package
run_case "the e2e script builds every listed binary" case_the_e2e_script_builds_every_listed_binary
run_case "the Rust scoped runner reads the shared list" case_the_rust_runner_reads_the_shared_list
run_case "the lander reads the shared list and has no literal copy" case_the_lander_reads_the_shared_list_and_has_no_literal_copy
run_case "scoped-test and the e2e script have no literal binary list" case_scoped_test_and_the_e2e_script_have_no_literal_binary_list
tap_done
