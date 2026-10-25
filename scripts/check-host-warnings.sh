#!/bin/sh
# Host warning lane (#348): judge THIS host's native build against tools/warning-baseline.json.
#
# The hub's scoped_test gate builds Linux only, so a warning that exists only on another host
# (macOS: Linux-only helpers that are dead code, unused imports behind a cfg(target_os = "linux")
# sibling) lands on rolling unseen and turns every later Mac scoped_test that touches the crate
# red (#9, #12, #19, #145, #154, #347, #348). Run this on a macOS satellite against current
# rolling (or a candidate) so the warning is reported before a worker trips over it.
#
# It counts compiler warnings the way scripts/scoped-test does: attributed to the Cargo package
# that emitted them, keyed by `file|lint code|message` (never a line number), two reports at one
# position (the lib and lib-test units) count once. A baselined package with a key above its
# baseline count is a finding; a package with no baseline entry is not judged. It never raises
# or rewrites the baseline.
#
# Usage: scripts/check-host-warnings.sh [--baseline FILE] [--root DIR] [--from-json FILE|-] [-- CARGO_ARGS...]
#   default: cargo check --workspace --all-targets --message-format=json (native host target)
#   --from-json   judge a saved `--message-format=json` stream instead of building (used by the test)
# Exit 0 clean, 1 findings, 2 usage or tooling error (a missing jq or cargo is never green).

set -u
ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
BASELINE=""
FROM=""
while [ $# -gt 0 ]; do
    case "$1" in
    --baseline) BASELINE="$2"; shift 2 ;;
    --root) ROOT="$2"; shift 2 ;;
    --from-json) FROM="$2"; shift 2 ;;
    --) shift; break ;;
    -h | --help) sed -n 2,22p "$0"; exit 0 ;;
    *) echo "check-host-warnings: unknown argument: $1" >&2; exit 2 ;;
    esac
done
[ -n "$BASELINE" ] || BASELINE="$ROOT/tools/warning-baseline.json"
command -v jq >/dev/null 2>&1 || { echo "check-host-warnings: jq is required (tooling blocker, not a pass)" >&2; exit 2; }
[ -r "$BASELINE" ] || { echo "check-host-warnings: cannot read baseline $BASELINE" >&2; exit 2; }

TMP="$(mktemp -d "${TMPDIR:-/tmp}/host-warnings.XXXXXX")" || exit 2
trap 'rm -rf "$TMP"' EXIT INT TERM
STREAM="$TMP/stream.json"

if [ -n "$FROM" ]; then
    if [ "$FROM" = "-" ]; then cat >"$STREAM"; else cp "$FROM" "$STREAM" || exit 2; fi
else
    command -v cargo >/dev/null 2>&1 || { echo "check-host-warnings: cargo is required" >&2; exit 2; }
    cd "$ROOT" || exit 2
    if [ $# -eq 0 ]; then set -- --workspace --all-targets; fi
    # stderr (cargo progress) is kept off the JSON stream; a failed build is not a clean one.
    cargo check "$@" --message-format=json >"$STREAM" 2>"$TMP/cargo.err"
    code=$?
    if [ "$code" -ne 0 ]; then
        tail -n 20 "$TMP/cargo.err" >&2
        echo "check-host-warnings: cargo check failed (exit $code); no verdict" >&2
        exit 2
    fi
fi

# Observed distinct warnings per package and key, then compare with the baseline counts.
jq -nr --arg root "$ROOT/" --slurpfile base "$BASELINE" -R '
  def pkgname:
    (.package_id // "") as $id
    | if ($id | contains("#")) then
        ($id | split("#")) as $p
        | ($p[-1] | split("@")[0]) as $n
        | if ($n | length > 0) and ($n | test("^[0-9]") | not) then $n
          else ($p[0:-1] | join("#") | rtrimstr("/") | split("/") | .[-1]) end
      else ($id | split(" ")[0]) end;
  [inputs | fromjson? | select(type == "object")
   | select(.reason == "compiler-message" and .message.level == "warning")
   | select(.message.message | test("^([0-9]+ warnings? emitted|aborting due to .*)$") | not)
   | (.message.spans // []) as $spans
   | ($spans | map(select(.is_primary)) | .[0] // $spans[0] // {}) as $span
   | {pkg: pkgname,
      key: (((($span.file_name // "") | ltrimstr($root)) + "|" + (.message.code.code // "") + "|" + .message.message)),
      pos: "\($span.line_start // 0):\($span.column_start // 0)"}
  ]
  | group_by([.pkg, .key])
  | map({pkg: .[0].pkg, key: .[0].key, observed: (map(.pos) | unique | length)})
  | map(. + {baseline: ($base[0].packages[.pkg].warnings[.key] // 0), judged: (.pkg as $p | $base[0].packages | has($p))})
  | map(select(.judged and .observed > .baseline))
  | .[] | "\(.pkg): \(.key): \(.observed) observed, baseline \(.baseline)"
' <"$STREAM" >"$TMP/findings.txt" || { echo "check-host-warnings: could not read the diagnostics stream" >&2; exit 2; }

if [ -s "$TMP/findings.txt" ]; then
    echo "check-host-warnings: warnings above tools/warning-baseline.json on this host:" >&2
    sed 's/^/  /' "$TMP/findings.txt" >&2
    echo "Fix the cause (cfg-gate the dead code to its platform, #[cfg(test)] a test-only helper, narrow visibility); never raise the baseline." >&2
    exit 1
fi
echo "check-host-warnings: ok (no baselined package has a warning above its baseline on this host)"
