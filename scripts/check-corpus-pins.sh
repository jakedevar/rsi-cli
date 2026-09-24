#!/bin/sh
# check-corpus-pins.sh — corpus provenance gate (Issues #622, #623).
#
# A benchmark corpus is an artifact: a wrong 40-hex pin silently mis-pins a
# seeded case or a control, and the packet builder then fails late or builds the
# wrong diff. This gate checks every pin a corpus markdown file declares.
#
# Usage:
#   scripts/check-corpus-pins.sh [--verbose] <file.md> [<file.md> ...]
#   scripts/check-corpus-pins.sh --probe <fix-sha> <regression-test-name>
#
# Exits 0 when every pin checks out, 1 when any check fails. Prints one
# `FAIL <file>:<line> <code> <detail>` line per failure. Run it from inside the
# repository the pins refer to.
#
# Recognized convention (documented in
# thoughts/shared/bench/reviewer-calibration/README.md):
#
#   * any inline `backticked 40-hex sha`
#       -> git cat-file -e <sha>^{commit}
#   * a markdown table column headed `fix SHA` or `commit SHA`
#       -> git merge-base --is-ancestor <sha> $CORPUS_PINS_BASE_REF
#   * a table column headed `parent SHA`, on a row whose fix column is set
#       -> require one 40-hex parent equal to `git rev-parse <fix>^`
#   * a table column headed `regression test*`, whose cells list Rust test names
#     or paths (`name` or `mod::tests::name`) in backticks
#       -> git grep -c -F -e <test> <fix>^ -- crates/   must be 0
#          git grep -c -F -e <test> <fix>  -- crates/   must be >= 1
#     (this distinguishes a new test from an extended one; a renamed test is
#      covered because the corpus records the NEW name)
#   * an annotation line
#       <!-- corpus-pins: renamed-from fix=<sha> old=<test-name> -->
#       -> <old> present at <fix>^ and absent at <fix>
#
# Environment:
#   CORPUS_PINS_BASE_REF   ancestry base ref      (default: origin/rolling)
#   CORPUS_PINS_SCOPE      git grep path scope    (default: crates/)
set -eu

VERBOSE=0
PROBE_SHA=""
PROBE_TEST=""
FILES=""

usage() {
  cat >&2 <<'USAGE'
usage: scripts/check-corpus-pins.sh [--verbose] <file> [<file> ...]
       scripts/check-corpus-pins.sh --probe <fix-sha> <regression-test-name>
USAGE
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    -v|--verbose) VERBOSE=1; shift ;;
    --probe)
      if [ "$#" -lt 3 ]; then usage; exit 2; fi
      PROBE_SHA="$2"; PROBE_TEST="$3"; shift 3 ;;
    -h|--help) usage; exit 0 ;;
    --) shift; break ;;
    -*) echo "check-corpus-pins: unknown option: $1" >&2; usage; exit 2 ;;
    *) FILES="$FILES $1"; shift ;;
  esac
done
while [ "$#" -gt 0 ]; do FILES="$FILES $1"; shift; done

BASE_REF="${CORPUS_PINS_BASE_REF:-origin/rolling}"
SCOPE="${CORPUS_PINS_SCOPE:-crates/}"

FAILS="$(mktemp)"
trap 'rm -f "$FAILS"' EXIT HUP INT TERM

# fail <line>  — recorded once, printed in first-seen order at the end.
fail() { printf '%s\n' "$1" >> "$FAILS"; }

ok() {
  if [ "$VERBOSE" -eq 1 ]; then printf 'ok %s\n' "$1"; fi
  return 0
}

# Match the Rust test FUNCTION, not the dotted path in the corpus cell. A cell
# like `store::tests::parse_timestamp_...` names a function whose definition is
# `fn parse_timestamp_...`, so only the final `::` segment is greppable source.
# Matching the full path would report 0 at both revisions and pass vacuously.
fn_of() { printf '%s\n' "${1##*::}"; }

# count_at <rev> <test-fn-leaf>: count exact test function definitions at <rev>.
count_at() {
  rev="$1"; leaf="$2"
  paths="$(git grep -l -E "(async[[:space:]]+)?fn[[:space:]]+$leaf[[:space:]]*[(<]" "$rev" -- "$SCOPE" 2>/dev/null | sed 's/^[^:]*://' || true)"
  total=0
  for path in $paths; do
    count=$(git show "$rev:$path" 2>/dev/null | awk -v "leaf=$leaf" '
      /^[[:space:]]*#\[(test|tokio::test)(\]|\()/ { test_attr=1; next }
      /^[[:space:]]*#\[/ { next }
      /^[[:space:]]*\/\// || /^[[:space:]]*$/ { next }
      {
        if (test_attr && $0 ~ ("^[[:space:]]*(async[[:space:]]+)?fn[[:space:]]+" leaf "[[:space:]]*[(<]")) count++
        test_attr=0
      }
      END { print count + 0 }
    ')
    total=$((total + count))
  done
  printf '%s\n' "$total"
}

# Absolute per-file table extraction: emits `ROW|line|fix|parent|tests|has-parent`
# for markdown tables that declare a SHA column.
extract_rows() {
  awk '
    BEGIN { n = 0 }
    {
      if ($0 ~ /^[ \t]*[|]/) { n++; block[n] = $0; start[n] = NR; next }
      flushblock()
    }
    END { flushblock() }

    function trim(s) { gsub(/^[ \t]+/, "", s); gsub(/[ \t]+$/, "", s); return s }

    function cells(line, out,    s, k, parts, i) {
      s = line
      sub(/^[ \t]*[|]/, "", s)
      sub(/[|][ \t]*$/, "", s)
      k = split(s, parts, "|")
      for (i = 1; i <= k; i++) out[i] = parts[i]
      return k
    }

    function flushblock(    i, k, nrow, hdr, row, c, fixc, parc, testc) {
      nrow = n
      n = 0
      if (nrow < 2) return
      if (block[2] !~ /^[ \t]*[|][ \t]*:?-+/) return
      k = cells(block[1], hdr)
      fixc = 0; parc = 0; testc = 0
      for (i = 1; i <= k; i++) {
        c = tolower(trim(hdr[i]))
        gsub(/`/, "", c)
        if (c == "fix sha" || c == "commit sha") { fixc = i }
        else if (c == "parent sha") { parc = i }
        else if (c ~ /regression test/) { testc = i }
      }
      if (fixc == 0) return
      for (i = 3; i <= nrow; i++) {
        k = cells(block[i], row)
        printf "ROW|%d|%s|%s|%s|%d\n", start[i], \
          (fixc <= k ? trim(row[fixc]) : "-"), \
          (parc > 0 && parc <= k ? trim(row[parc]) : "-"), \
          (testc > 0 && testc <= k ? trim(row[testc]) : "-"), parc
      }
    }
  ' "$1"
}

check_row_tests() { # lineno fixsha testcell
  lineno="$1"; fixsha="$2"; testcell="$3"
  pred="$(git rev-parse --verify --quiet "$fixsha^" || true)"
  if [ -z "$pred" ]; then
    fail "$file:$lineno fix-has-no-parent $fixsha"
    return 0
  fi
  # Backticks are literal markdown delimiters here, not command substitution.
  # shellcheck disable=SC2016
  names="$(printf '%s\n' "$testcell" | grep -oE '`[^`]+`' 2>/dev/null | tr -d '`' || true)"
  if [ -z "$names" ]; then
    fail "$file:$lineno unsupported-test-syntax $testcell"
    return 0
  fi
  printf '%s\n' "$names" | while IFS= read -r name; do
    # A corpus may explain a rename as `(renamed from `..._old_name`)` beside
    # the new test. That ellipsis-marked prose fragment is not a declared name;
    # the explicit corpus-pins: renamed-from annotation validates real old names.
    case "$name" in
      ...*)
        if printf '%s\n' "$testcell" | grep -qF '(renamed from'; then continue; fi
        ;;
    esac
    if ! printf '%s\n' "$name" | grep -qE '^[A-Za-z_][A-Za-z0-9_]*(::[A-Za-z_][A-Za-z0-9_]*)*$'; then
      fail "$file:$lineno unsupported-test-syntax $name"
      continue
    fi
    leaf="$(fn_of "$name")"
    at_parent="$(count_at "$pred" "$leaf")"
    at_fix="$(count_at "$fixsha" "$leaf")"
    if [ "$at_parent" -ne 0 ]; then
      fail "$file:$lineno test-present-at-parent $name parent=$at_parent fix=$at_fix ($fixsha)"
    elif [ "$at_fix" -lt 1 ]; then
      fail "$file:$lineno test-absent-at-fix $name parent=$at_parent fix=$at_fix ($fixsha)"
    else
      ok "$file:$lineno test-absent-at-parent $name fix=$at_fix"
    fi
  done
}

process_file() {
  file="$1"
  if [ ! -f "$file" ]; then fail "$file:0 file-missing $file"; return 0; fi

  # 1. Every inline backticked 40-hex SHA must be a commit in this repository.
  # shellcheck disable=SC2016
  grep -onE '`[0-9a-f]{40}`' "$file" 2>/dev/null | while IFS=: read -r lineno token; do
    sha="$(printf '%s' "$token" | tr -d '`')"
    if git cat-file -e "$sha^{commit}" 2>/dev/null; then
      ok "$file:$lineno resolves $sha"
    else
      fail "$file:$lineno unresolved-sha $sha"
    fi
  done

  # 2. Table pins: ancestry, declared-parent identity, regression-test absence.
  rows="$(mktemp)"
  extract_rows "$file" > "$rows"
  while IFS='|' read -r kind lineno fixcell parcell testcell hasparent; do
    [ "$kind" = "ROW" ] || continue
    fixsha="$(printf '%s' "$fixcell" | grep -oE '[0-9a-f]{40}' 2>/dev/null | head -n1 || true)"
    if [ -z "$fixsha" ]; then
      fail "$file:$lineno missing-fix-sha $fixcell"
      continue
    fi
    if ! git cat-file -e "$fixsha^{commit}" 2>/dev/null; then
      fail "$file:$lineno unresolved-sha $fixsha"
      continue
    fi
    if git merge-base --is-ancestor "$fixsha" "$BASE_REF" 2>/dev/null; then
      ok "$file:$lineno ancestor $fixsha <= $BASE_REF"
    else
      fail "$file:$lineno not-ancestor $fixsha (base $BASE_REF)"
    fi
    if [ "$hasparent" -gt 0 ]; then
      parent_value="$(printf '%s' "$parcell" | sed 's/^[[:space:]]*//; s/[[:space:]]*$//')"
      case "$parent_value" in
        \`*\`) parent_value="${parent_value#\`}"; parent_value="${parent_value%\`}" ;;
      esac
      if [ -z "$parent_value" ] || [ "$parent_value" = "-" ]; then
        fail "$file:$lineno parent-missing fix=$fixsha"
      elif [ "${#parent_value}" -ne 40 ] || ! printf '%s\n' "$parent_value" | grep -qE '^[0-9a-f]{40}$'; then
        fail "$file:$lineno parent-malformed fix=$fixsha declared=$parcell"
      else
        parsha="$parent_value"
        actual="$(git rev-parse "$fixsha^")"
        if [ "$actual" = "$parsha" ]; then
          ok "$file:$lineno parent $fixsha^ = $parsha"
        else
          fail "$file:$lineno wrong-parent $fixsha declared=$parsha actual=$actual"
        fi
      fi
    fi
    if [ -n "$testcell" ] && [ "$testcell" != "-" ]; then
      check_row_tests "$lineno" "$fixsha" "$testcell"
    fi
  done < "$rows"
  rm -f "$rows"

  # 3. Declared renames must actually be renames.
  grep -n 'corpus-pins:[[:space:]]*renamed-from' "$file" 2>/dev/null | while IFS=: read -r lineno rest; do
    ann_fix="$(printf '%s' "$rest" | sed -n 's/.*fix=\([0-9a-f]\{40\}\).*/\1/p')"
    ann_old="$(printf '%s' "$rest" | sed -n 's/.*old=[[:space:]]*\([^ 	]*\).*/\1/p' | tr -d '`')"
    if [ -z "$ann_fix" ] || [ -z "$ann_old" ]; then
      fail "$file:$lineno renamed-from-malformed $rest"
      continue
    fi
    if ! git cat-file -e "$ann_fix^{commit}" 2>/dev/null; then
      fail "$file:$lineno unresolved-sha $ann_fix"
      continue
    fi
    pred="$(git rev-parse --verify --quiet "$ann_fix^" || true)"
    ann_leaf="$(fn_of "$ann_old")"
    at_parent="$(count_at "$pred" "$ann_leaf")"
    at_fix="$(count_at "$ann_fix" "$ann_leaf")"
    if [ "$at_parent" -lt 1 ]; then
      fail "$file:$lineno renamed-old-name-absent-at-parent $ann_old parent=$at_parent fix=$at_fix"
    elif [ "$at_fix" -ne 0 ]; then
      fail "$file:$lineno renamed-old-name-still-present-at-fix $ann_old parent=$at_parent fix=$at_fix"
    else
      ok "$file:$lineno renamed $ann_old"
    fi
  done
}

report() {
  n=0
  if [ -s "$FAILS" ]; then
    awk '!seen[$0]++' "$FAILS"
    n="$(awk '!seen[$0]++' "$FAILS" | wc -l | tr -d ' ')"
  fi
  if [ "$n" -ne 0 ]; then
    echo "check-corpus-pins: $n failure(s)" >&2
    exit 1
  fi
  echo "check-corpus-pins: ok ($n failures)" >&2
  exit 0
}

if [ -n "$PROBE_SHA" ]; then
  if ! git cat-file -e "$PROBE_SHA^{commit}" 2>/dev/null; then
    echo "FAIL probe:0 unresolved-sha $PROBE_SHA"
    echo "check-corpus-pins: probe FAIL" >&2
    exit 1
  fi
  probe_parent="$(git rev-parse --verify --quiet "$PROBE_SHA^" || true)"
  if [ -z "$probe_parent" ]; then
    echo "FAIL probe:0 fix-has-no-parent $PROBE_SHA"
    echo "check-corpus-pins: probe FAIL" >&2
    exit 1
  fi
  at_parent="$(count_at "$probe_parent" "$(fn_of "$PROBE_TEST")")"
  at_fix="$(count_at "$PROBE_SHA" "$(fn_of "$PROBE_TEST")")"
  if [ "$at_parent" -eq 0 ] && [ "$at_fix" -ge 1 ]; then
    echo "PASS probe ${PROBE_SHA} $PROBE_TEST parent=0 fix=$at_fix"
    echo "check-corpus-pins: probe PASS" >&2
    exit 0
  fi
  if [ "$at_parent" -ne 0 ]; then
    echo "FAIL probe:0 test-present-at-parent $PROBE_TEST parent=$at_parent fix=$at_fix ($PROBE_SHA) — regression test already exists at the parent, so the case cannot fail without the fix"
  else
    echo "FAIL probe:0 test-absent-at-fix $PROBE_TEST parent=$at_parent fix=$at_fix ($PROBE_SHA)"
  fi
  echo "check-corpus-pins: probe FAIL" >&2
  exit 1
fi

if [ -z "$FILES" ]; then
  usage
  exit 2
fi

if ! git rev-parse --verify --quiet "$BASE_REF^{commit}" >/dev/null; then
  echo "check-corpus-pins: base ref '$BASE_REF' does not resolve; set CORPUS_PINS_BASE_REF" >&2
  exit 1
fi

for f in $FILES; do
  process_file "$f"
done

report
