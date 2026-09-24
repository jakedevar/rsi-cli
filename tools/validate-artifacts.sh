#!/bin/sh
# tools/validate-artifacts.sh — run the MWP/ICM validators over pipeline artifacts.
#
# Usage:
#   validate-artifacts.sh changed [BASE]   Strict, HARD gate. Validates only the
#                                          artifacts changed vs BASE (default:
#                                          main). Exit 1 on any invalid changed
#                                          artifact; exit 0 otherwise. Zero
#                                          legacy noise — touches only new work.
#   validate-artifacts.sh corpus           Lenient, ADVISORY sweep of the whole
#                                          corpus. Prints pass/invalid counts and
#                                          ALWAYS exits 0 (backlog visibility).
#
# Artifact → validator mapping:
#   thoughts/shared/research/*.json          → rsi-research-validate  (strict|lenient)
#   thoughts/shared/handoffs/**/*.md         → rsi-handoff-validate   (strict|lenient)
#   *.md with `phases_sealed:` in frontmatter→ rsi-manifest-validate  (single mode)
# (rsi-contract-validate reads a worker reply from stdin, not a committed file,
#  so it has no staged/committed artifact to gate here.)
#
# Binary resolution (generic — the workspace may use a shared cargo target-dir):
#   $RSI_VALIDATE_BIN_DIR → PATH → $CARGO_TARGET_DIR → cargo-config target-dir →
#   repo target/ ; release preferred over debug. A missing binary is a
#   skip-with-warning (never a silent pass, never forces a compile).
#
# Repo-tooling only. Deps: sh, git, awk, grep, sed, find + the pre-built validators.

set -u

ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
cd "$ROOT" || { echo "validate-artifacts: cannot cd to repo root" >&2; exit 2; }

RESEARCH_DIR="thoughts/shared/research"
HANDOFF_DIR="thoughts/shared/handoffs"

# Resolve a validator binary by name; echo an absolute/relative path or "".
find_validator() {
  _name="$1"
  if [ -n "${RSI_VALIDATE_BIN_DIR:-}" ] && [ -x "$RSI_VALIDATE_BIN_DIR/$_name" ]; then
    echo "$RSI_VALIDATE_BIN_DIR/$_name"; return 0
  fi
  _p="$(command -v "$_name" 2>/dev/null || true)"
  [ -n "$_p" ] && { echo "$_p"; return 0; }
  _cfg="$(grep -shE '^[[:space:]]*target-dir[[:space:]]*=' \
            .cargo/config.toml .cargo/config "${HOME}/.cargo/config.toml" "${HOME}/.cargo/config" 2>/dev/null \
          | head -n1 | sed -E 's/.*=[[:space:]]*"?([^"]*)"?[[:space:]]*$/\1/')"
  for _base in "${CARGO_TARGET_DIR:-}" "$_cfg" "target"; do
    [ -n "$_base" ] || continue
    for _prof in release debug; do
      [ -x "$_base/$_prof/$_name" ] && { echo "$_base/$_prof/$_name"; return 0; }
    done
  done
  echo ""; return 0
}

# True iff $1 is a verification manifest (phases_sealed: in its FRONTMATTER).
# NOTE: awk `exit N` in a main rule still runs END, so the decision is made in
# END from a flag (a naive `exit 0` in a rule would be overridden by END).
is_manifest() {
  awk '
    NR==1   { if ($0 != "---") exit 1; next }
    done==1 { next }                          # stop scanning after frontmatter
    /^---[[:space:]]*$/ { done=1; next }       # end of frontmatter fence
    /^phases_sealed:/   { found=1 }
    END     { exit (found ? 0 : 1) }
  ' "$1" 2>/dev/null
}

# Validate one file. Echoes nothing; return: 0 valid, 1 invalid, 2 skip(no bin),
# 3 not-an-artifact.  $2 = strict|lenient.
validate_one() {
  _f="$1"; _mode="$2"
  [ -f "$_f" ] || return 3
  case "$_f" in
    "$RESEARCH_DIR"/*.json)
      _bin="$(find_validator rsi-research-validate)"; [ -n "$_bin" ] || return 2
      if [ "$_mode" = strict ]; then "$_bin" "$_f" --strict >/dev/null 2>&1
      else "$_bin" "$_f" >/dev/null 2>&1; fi ;;
    "$HANDOFF_DIR"/*.md)
      _bin="$(find_validator rsi-handoff-validate)"; [ -n "$_bin" ] || return 2
      if [ "$_mode" = strict ]; then "$_bin" --strict "$_f" >/dev/null 2>&1
      else "$_bin" "$_f" >/dev/null 2>&1; fi ;;
    *.md)
      is_manifest "$_f" || return 3
      _bin="$(find_validator rsi-manifest-validate)"; [ -n "$_bin" ] || return 2
      "$_bin" "$_f" >/dev/null 2>&1 ;;
    *)
      return 3 ;;
  esac
  # validator exit: 0 valid, nonzero (1 I/O | 2 invalid) → treat as fail
  [ $? -eq 0 ] && return 0 || return 1
}

mode_changed() {
  base="${1:-main}"
  # Best-effort: make BASE resolvable in CI shallow clones.
  git fetch --no-tags origin "$base:$base" >/dev/null 2>&1 || true
  if ! git rev-parse --verify "$base" >/dev/null 2>&1; then
    echo "validate-artifacts(changed): base ref '$base' not found; nothing to gate." >&2
    return 0
  fi
  files="$(git diff --name-only --diff-filter=d "$base...HEAD" 2>/dev/null || true)"
  if [ -z "$files" ]; then
    echo "validate-artifacts(changed vs $base): no changed pipeline artifacts."
    return 0
  fi
  rc=0; ok=0; skip=0
  for f in $files; do
    validate_one "$f" strict; r=$?
    case "$r" in
      0) ok=$((ok + 1));          echo "  ok:      $f" ;;
      1) rc=1;                     echo "  INVALID: $f" >&2 ;;
      2) skip=$((skip + 1));       echo "  [skip: validator not built] $f" >&2 ;;
      3) : ;;   # not an artifact
    esac
  done
  echo "validate-artifacts(changed vs $base): valid=$ok skipped=$skip$( [ $rc -ne 0 ] && echo ' — INVALID artifacts present' )"
  return $rc
}

sweep() {  # $1=find-root $2=name-glob  → prints "valid/total"
  _root="$1"; _glob="$2"; _total=0; _bad=0; _skip=0
  [ -d "$_root" ] || { echo "  (dir absent: $_root)"; return 0; }
  for f in $(find "$_root" -type f -name "$_glob" 2>/dev/null); do
    _total=$((_total + 1))
    validate_one "$f" lenient; _r=$?
    [ "$_r" -eq 1 ] && { _bad=$((_bad + 1)); echo "  invalid: $f"; }
    [ "$_r" -eq 2 ] && _skip=$((_skip + 1))
  done
  echo "  → $((_total - _bad))/$_total valid (skipped=$_skip)"
}

mode_corpus() {
  echo "== research sidecars (lenient) =="
  sweep "$RESEARCH_DIR" '*.json'
  echo "== handoffs (lenient) =="
  sweep "$HANDOFF_DIR" '*.md'
  echo "== manifests — phases_sealed: signature (lenient) =="
  mtotal=0; mbad=0
  for f in $(grep -rlE '^phases_sealed:' thoughts 2>/dev/null); do
    is_manifest "$f" || continue
    mtotal=$((mtotal + 1))
    validate_one "$f" lenient; [ $? -eq 1 ] && { mbad=$((mbad + 1)); echo "  invalid: $f"; }
  done
  echo "  → $((mtotal - mbad))/$mtotal valid"
  echo ""
  echo "validate-artifacts(corpus): advisory sweep complete (exit 0 regardless)."
  return 0
}

cmd="${1:-}"
case "$cmd" in
  changed) shift; mode_changed "${1:-main}" ;;
  corpus)  mode_corpus ;;
  *)
    echo "usage: validate-artifacts.sh {changed [BASE] | corpus}" >&2
    exit 2 ;;
esac
