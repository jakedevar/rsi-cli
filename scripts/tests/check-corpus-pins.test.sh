#!/bin/sh
# Self-test for scripts/check-corpus-pins.sh (Issues #622, #623).
#
# Builds a throwaway git repository with a crates/ tree, then exercises the gate
# against a fixture corpus markdown. Every assertion is POSITIVE: it requires a
# specific failure line to be printed, not merely a nonzero exit, so the test
# fails if the gate stops detecting a class of corruption.
#
# Fixture shapes:
#   * a real commit with a real declared parent                        -> passes
#   * a fabricated (unresolvable) 40-hex SHA                           -> unresolved-sha
#   * a valid commit that is NOT the declared parent                   -> wrong-parent
#   * a test name that already exists at the parent (#623's 73779a65b8) -> test-present-at-parent
#   * a short prefix of a real function name                            -> test-absent-at-fix
#   * a malformed declared parent                                       -> parent-malformed
#   * a bare backticked test name                                       -> exact test validation
#   * staged corpus content repaired only in the worktree               -> hook still fails
#   * a renamed test, declared via corpus-pins: renamed-from           -> passes
#
# Usage: scripts/tests/check-corpus-pins.test.sh   (exit 0 = pass)
set -eu

HERE="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
GATE="$(CDPATH='' cd -- "$HERE/.." && pwd)/check-corpus-pins.sh"

if [ ! -x "$GATE" ]; then
  echo "self-test: gate not executable: $GATE" >&2
  exit 2
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT HUP INT TERM

REPO="$TMP/repo"
mkdir -p "$REPO/crates/rsid/src"
cd "$REPO"

git init -q -b rolling
git config user.name "Corpus Pins Self Test"
git config user.email "corpus-pins@example.invalid"

GIT() { git "$@"; }

# --- c0: the parent tree -----------------------------------------------------
mkdir -p crates/rsid/src
cat > crates/rsid/src/lib.rs <<'RS'
pub fn parse(input: &str) -> Option<u32> {
    input.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_digits() {
        assert_eq!(parse("42"), Some(42));
    }

    #[test]
    fn legacy_name_renamed_later() {
        assert_eq!(parse("7"), Some(7));
    }
}
RS
GIT add .
GIT commit -q -m "c0: initial tree with an existing test"

# --- c1: a real fix that ADDS a new test (absence-at-parent holds) -----------
cat > crates/rsid/src/lib.rs <<'RS'
pub fn parse(input: &str) -> Option<u32> {
    input.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_digits() {
        assert_eq!(parse("42"), Some(42));
    }

    #[test]
    fn legacy_name_renamed_later() {
        assert_eq!(parse("7"), Some(7));
    }

    #[test]
    fn parse_tolerates_surrounding_whitespace() {
        assert_eq!(parse(" 42 "), Some(42));
    }

    #[test]
    fn parse_tolerates_surrounding_whitespace_and_suffix() {
        assert_eq!(parse(" 42 "), Some(42));
    }
}
RS
GIT add .
GIT commit -q -m "c1: fix parse to trim whitespace, add a new test"
C1="$(GIT rev-parse HEAD)"

# --- c2: a fix that RENAMES an existing test --------------------------------
cat > crates/rsid/src/lib.rs <<'RS'
pub fn parse(input: &str) -> Option<u32> {
    input.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_digits() {
        assert_eq!(parse("42"), Some(42));
    }

    #[test]
    fn renamed_single_char_digit_still_parses() {
        assert_eq!(parse("7"), Some(7));
    }

    #[test]
    fn parse_tolerates_surrounding_whitespace() {
        assert_eq!(parse(" 42 "), Some(42));
    }
}
RS
GIT add .
GIT commit -q -m "c2: rename legacy test"
C2="$(GIT rev-parse HEAD)"

# --- c3: an unrelated commit (a valid SHA that is NOT some fix's parent) -----
echo "pub const UNRELATED: u8 = 1;" > crates/rsid/src/unrelated.rs
GIT add .
GIT commit -q -m "c3: unrelated additive commit"
C3="$(GIT rev-parse HEAD)"

C1P="$(GIT rev-parse "$C1^")"
C3P="$(GIT rev-parse "$C3^")"
FULL_PARENT="$(GIT rev-parse "$C1^")"

# The fixture repo has no origin/remote; its ancestry base is its own branch.
export CORPUS_PINS_BASE_REF="rolling"

# --- Fixture 1: every pin correct -------------------------------------------
cat > "$TMP/good.md" <<EOF
# Fixture corpus (all pins correct)

| # | fix SHA | parent SHA | crate | regression test added by the fix |
|---|---------|-----------|-------|----------------------------------|
| G1 | \`$C1\` | \`$C1P\` | rsid | \`tests::parse_tolerates_surrounding_whitespace\` |

<!-- corpus-pins: renamed-from fix=$C2 old=tests::legacy_name_renamed_later -->
EOF

# --- Fixture 2: fabricated SHA + wrong parent --------------------------------
cat > "$TMP/bad.md" <<EOF
# Fixture corpus (corrupt pins)

| # | fix SHA | parent SHA | crate | regression test added by the fix |
|---|---------|-----------|-------|----------------------------------|
| B1 | \`1111111111111111111111111111111111111111\` | \`$C1P\` | rsid | - |
| B2 | \`$C1\` | \`$C3P\` | rsid | - |
EOF

# --- Fixture 3: the #623 repro shape — test already exists at the parent -----
cat > "$TMP/seeded.md" <<EOF
# Fixture corpus (extended, not added, test)

| # | fix SHA | parent SHA | crate | regression test added by the fix |
|---|---------|-----------|-------|----------------------------------|
| E1 | \`$C1\` | \`$C1P\` | rsid | \`tests::parse_accepts_digits\` |
EOF

pass=0; failed=0
check() { # check <name> <expected-substring> <expected-exit> <file> [gate-args...]
  name="$1"; want="$2"; want_rc="$3"; file="$4"; shift 4
  out="$TMP/out.$$"
  rc=0
  "$GATE" "$@" "$file" > "$out" 2>&1 || rc=$?
  if [ "$rc" -ne "$want_rc" ]; then
    echo "FAIL $name: exit=$rc want=$want_rc"; sed 's/^/    /' "$out"; failed=$((failed + 1)); rm -f "$out"; return 0
  fi
  if ! grep -qF -- "$want" "$out"; then
    echo "FAIL $name: missing expected line: $want"; sed 's/^/    /' "$out"; failed=$((failed + 1)); rm -f "$out"; return 0
  fi
  echo "ok $name"; pass=$((pass + 1)); rm -f "$out"
}

echo "== positive: correct pins pass =="
check "good corpus exits 0" "check-corpus-pins: ok" 0 "$TMP/good.md"
check "good corpus resolves fix sha" "resolves $C1" 0 "$TMP/good.md" --verbose
check "good corpus ancestor check runs" "ancestor $C1" 0 "$TMP/good.md" --verbose
check "good corpus parent identity holds" "parent $C1^ = $C1P" 0 "$TMP/good.md" --verbose
check "good corpus new test absent at parent" "test-absent-at-parent tests::parse_tolerates_surrounding_whitespace fix=1" 0 "$TMP/good.md" --verbose
check "good corpus rename proven" "renamed tests::legacy_name_renamed_later" 0 "$TMP/good.md" --verbose

echo "== #622: fabricated SHA is rejected =="
check "fabricated sha flagged" "unresolved-sha 1111111111111111111111111111111111111111" 1 "$TMP/bad.md"

echo "== #622: wrong declared parent is rejected =="
check "wrong parent flagged" "wrong-parent $C1 declared=$C3P actual=$C1P" 1 "$TMP/bad.md"

echo "== #623: test present at parent is rejected =="
check "extended test flagged" "test-present-at-parent tests::parse_accepts_digits parent=1 fix=1" 1 "$TMP/seeded.md"
check "seeded fixture exits nonzero" "check-corpus-pins:" 1 "$TMP/seeded.md"

cat > "$TMP/parent-bad.md" <<EOF
| # | fix SHA | parent SHA | regression test |
|---|---------|-----------|-----------------|
| P1 | \`$C1\` | \`bogus\` | - |
EOF
check "malformed parent rejected" "parent-malformed" 1 "$TMP/parent-bad.md"

cat > "$TMP/parent-missing.md" <<EOF
| # | fix SHA | parent SHA | regression test |
|---|---------|-----------|-----------------|
| P2 | \`$C1\` |  | - |
EOF
check "missing parent rejected" "parent-missing" 1 "$TMP/parent-missing.md"

cat > "$TMP/bare.md" <<EOF
| # | fix SHA | parent SHA | regression test |
|---|---------|-----------|-----------------|
| T1 | \`$C1\` | \`$FULL_PARENT\` | \`parse_tolerates_surrounding_whitespace\` |
EOF
check "bare test name exact definition accepted" "test-absent-at-parent parse_tolerates_surrounding_whitespace fix=1" 0 "$TMP/bare.md" --verbose
cat > "$TMP/prefix.md" <<EOF
| # | fix SHA | parent SHA | regression test |
|---|---------|-----------|-----------------|
| T2 | \`$C1\` | \`$FULL_PARENT\` | \`parse_tolerates_surrounding_whitespace_and\` |
EOF
check "short test prefix rejected" "test-absent-at-fix parse_tolerates_surrounding_whitespace_and" 1 "$TMP/prefix.md"
cat > "$TMP/unsupported.md" <<EOF
| # | fix SHA | parent SHA | regression test |
|---|---------|-----------|-----------------|
| T3 | \`$C1\` | \`$FULL_PARENT\` | \`not a valid test name\` |
EOF
check "unsupported test token fails closed" "unsupported-test-syntax not a valid test name" 1 "$TMP/unsupported.md"

echo "== pre-commit validates staged corpus bytes =="
mkdir -p tools/git-hooks thoughts/shared/research scripts
cp "$GATE" scripts/check-corpus-pins.sh
cp "$(CDPATH='' cd -- "$HERE/../../tools/git-hooks" && pwd)/pre-commit" tools/git-hooks/pre-commit
chmod +x tools/git-hooks/pre-commit
cat > thoughts/shared/research/staged-packet.md <<EOF
| # | fix SHA | parent SHA | regression test |
|---|---------|-----------|-----------------|
| S1 | \`1111111111111111111111111111111111111111\` | \`$FULL_PARENT\` | - |
EOF
GIT add thoughts/shared/research/staged-packet.md
cat > thoughts/shared/research/staged-packet.md <<EOF
| # | fix SHA | parent SHA | regression test |
|---|---------|-----------|-----------------|
| S1 | \`$C1\` | \`$FULL_PARENT\` | - |
EOF
rc=0
CORPUS_PINS_BASE_REF=rolling tools/git-hooks/pre-commit > "$TMP/hook-out" 2>&1 || rc=$?
if [ "$rc" -ne 0 ] && grep -qF "unresolved-sha 1111111111111111111111111111111111111111" "$TMP/hook-out"; then
  echo "ok hook rejects bad staged SHA after working-copy repair"; pass=$((pass + 1))
else
  echo "FAIL hook validates staged corpus content (exit=$rc)"; sed 's/^/    /' "$TMP/hook-out"; failed=$((failed + 1))
fi

echo "== probe mode mirrors the issue's exact gate =="
rc=0; "$GATE" --probe "$C1" "tests::parse_tolerates_surrounding_whitespace" > "$TMP/probe-ok" 2>&1 || rc=$?
if [ "$rc" -eq 0 ] && grep -qF "probe PASS" "$TMP/probe-ok"; then echo "ok probe accepts a genuinely new test"; pass=$((pass + 1)); else echo "FAIL probe accepts a genuinely new test (rc=$rc)"; sed 's/^/    /' "$TMP/probe-ok"; failed=$((failed + 1)); fi
rc=0; "$GATE" --probe "$C1" "tests::parse_accepts_digits" > "$TMP/probe-bad" 2>&1 || rc=$?
if [ "$rc" -eq 1 ] && grep -qF "test-present-at-parent tests::parse_accepts_digits parent=1" "$TMP/probe-bad"; then echo "ok probe rejects an extended test"; pass=$((pass + 1)); else echo "FAIL probe rejects an extended test (rc=$rc)"; sed 's/^/    /' "$TMP/probe-bad"; failed=$((failed + 1)); fi
rm -f "$TMP/probe-ok" "$TMP/probe-bad"

echo "== missing file is a failure, not a crash =="
check "missing file flagged" "file-missing" 1 "$TMP/does-not-exist.md"

echo
echo "check-corpus-pins self-test: $pass passed, $failed failed"
[ "$failed" -eq 0 ] || exit 1
exit 0
