#!/bin/sh
# Self-test for scripts/check-host-warnings.sh (#348).
#
# Feeds saved `cargo --message-format=json` streams through --from-json, so no build runs.
# Every assertion is positive: a finding must name its package and key, a clean case must
# print the ok line.
#   * a macOS-only dead_code warning in a baselined package          -> exit 1, named
#   * the same warning counted by the lib and lib-test units once    -> still one position
#   * a warning that the baseline accepts                            -> exit 0
#   * a package with no baseline entry                               -> not judged
#   * a non-JSON line and the "N warnings emitted" noise             -> ignored
#   * an unreadable baseline                                         -> exit 2, never green
#
# Usage: scripts/check-host-warnings.selftest.sh   (exit 0 = pass)
set -eu

HERE="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
GATE="$HERE/check-host-warnings.sh"
[ -x "$GATE" ] || { echo "self-test: gate not executable: $GATE" >&2; exit 2; }
command -v jq >/dev/null 2>&1 || { echo "self-test: jq missing" >&2; exit 2; }

TMP="$(mktemp -d "${TMPDIR:-/tmp}/host-warnings-test.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT INT TERM
ROOT="/work/repo"

BASE="$TMP/baseline.json"
cat >"$BASE" <<'JSON'
{"schema":1,"packages":{
 "rsid":{"configs":["lib","lib test"],"warnings":{}},
 "rsid-store":{"configs":["lib"],"warnings":{
  "crates/rsid-store/src/a.rs|dead_code|constant `ACCEPTED` is never used":1}}}}
JSON

# warn PKGID FILE LINE CODE MESSAGE -> one compiler-message line
warn() {
    jq -nc --arg id "$1" --arg f "$2" --argjson l "$3" --arg c "$4" --arg m "$5" \
        '{reason:"compiler-message",package_id:$id,message:{level:"warning",message:$m,code:{code:$c},
          spans:[{is_primary:true,file_name:$f,line_start:$l,column_start:5}]}}'
}
RSID="path+file://$ROOT/crates/rsid#0.1.0"
STORE="path+file://$ROOT/crates/rsid-store#0.1.0"
OTHER="path+file://$ROOT/crates/rsi-common#0.1.0"

run() { # name stream expected_exit expected_text
    name="$1"; stream="$2"; want="$3"; text="$4"
    set +e
    out="$("$GATE" --baseline "$BASE" --root "$ROOT" --from-json "$stream" 2>&1)"
    got=$?
    set -e
    if [ "$got" -ne "$want" ]; then
        echo "FAIL $name: exit $got, wanted $want" >&2; echo "$out" >&2; exit 1
    fi
    case "$out" in *"$text"*) ;; *) echo "FAIL $name: output lacks '$text'" >&2; echo "$out" >&2; exit 1 ;; esac
    echo "ok   $name"
}

# 1. A macOS-only dead helper in rsid (baseline 0), reported by the lib-test unit.
{
    echo "not json, cargo progress"
    warn "$RSID" crates/rsid/src/rolling_queue.rs 10 dead_code 'function `detached_lander` is never used'
    echo '{"reason":"compiler-message","package_id":"x","message":{"level":"warning","message":"2 warnings emitted","spans":[]}}'
} >"$TMP/s1.json"
run mac-only-dead-code "$TMP/s1.json" 1 'rsid: crates/rsid/src/rolling_queue.rs|dead_code|function `detached_lander` is never used: 1 observed, baseline 0'

# 2. The lib and lib-test units report one warning at one position: one warning.
{
    warn "$STORE" crates/rsid-store/src/a.rs 7 dead_code 'constant `ACCEPTED` is never used'
    warn "$STORE" crates/rsid-store/src/a.rs 7 dead_code 'constant `ACCEPTED` is never used'
} >"$TMP/s2.json"
run unit-dedupe-within-baseline "$TMP/s2.json" 0 'check-host-warnings: ok'

# 3. A second report at another position exceeds the baseline count of 1.
{
    warn "$STORE" crates/rsid-store/src/a.rs 7 dead_code 'constant `ACCEPTED` is never used'
    warn "$STORE" crates/rsid-store/src/a.rs 90 dead_code 'constant `ACCEPTED` is never used'
} >"$TMP/s3.json"
run second-position-over-baseline "$TMP/s3.json" 1 'rsid-store: crates/rsid-store/src/a.rs|dead_code|constant `ACCEPTED` is never used: 2 observed, baseline 1'

# 4. A package with no baseline entry is not judged.
warn "$OTHER" crates/rsi-common/src/x.rs 3 unused_imports 'unused import: `Foo`' >"$TMP/s4.json"
run unbaselined-package-not-judged "$TMP/s4.json" 0 'check-host-warnings: ok'

# 5. A dependency-style package id with a NAME@VERSION fragment is attributed by NAME.
warn "path+file://$ROOT/crates/rsid-dir#rsid@0.1.0" crates/rsid/src/signal_sender.rs 140 unused_imports 'unused imports: `CGROUP_CAP` and `COMM_CAP`' >"$TMP/s5.json"
run name-at-version-package-id "$TMP/s5.json" 1 'rsid: crates/rsid/src/signal_sender.rs|unused_imports|unused imports: `CGROUP_CAP` and `COMM_CAP`: 1 observed, baseline 0'

# 6. An unreadable baseline is a tooling error, never a pass.
set +e
"$GATE" --baseline "$TMP/missing.json" --root "$ROOT" --from-json "$TMP/s2.json" >/dev/null 2>&1
code=$?
set -e
[ "$code" -eq 2 ] || { echo "FAIL missing-baseline: exit $code, wanted 2" >&2; exit 1; }
echo "ok   missing-baseline-is-error"
echo "all passed"
