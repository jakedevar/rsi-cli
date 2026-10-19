#!/bin/bash
# Program of the iOS re-sign launchd agent (hub #2963, retry policy hub #2979).
#
# The agent fires every few hours; this script decides. A free Personal Team
# profile expires 7 days after install, so it reinstalls from day 5 after the
# last success and keeps retrying on every run until an install succeeds:
#   - state file ~/.rsi/ios-resign.state holds the epoch seconds of the last
#     successful install; no (or an unreadable or future) state file = due now;
#   - younger than 5 days: no-op, logged as `skip`;
#   - due: runs `scripts/ios/dev-install.sh --network-first` from the checkout
#     this script lives in. Success rewrites the state file (clock reset);
#     failure leaves it, so the next run retries.
# Appends ONE line per run to ~/.rsi/logs/ios-resign.log:
#   <RFC3339 UTC> ok|skip|failed exit=<n> <short reason>
# Exit 3 (no reachable or unlocked iPhone) reads `failed exit=3 needs-phone ...`
# so the Mac PM can grep it; this script never calls rsid. On failure the full
# output is left in ~/.rsi/logs/ios-resign.last.log (removed after a successful
# run, so its presence means the last install attempt failed).
# Device id, team id and Apple ID come from env/local config read by dev-install.sh.
# Works under macOS /bin/bash 3.2 and Linux bash 5.
#
# Test overrides: RSI_IOS_DEV_INSTALL (dev-install script to run),
# RSI_IOS_RESIGN_STATE (state file), RSI_IOS_RESIGN_NOW (epoch seconds clock),
# RSI_IOS_RESIGN_DUE_DAYS (default 5).
set -u

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
dev_install="${RSI_IOS_DEV_INSTALL:-$here/dev-install.sh}"
: "${HOME:?HOME must be set}"
state_file="${RSI_IOS_RESIGN_STATE:-$HOME/.rsi/ios-resign.state}"
due_days="${RSI_IOS_RESIGN_DUE_DAYS:-5}"
log_dir="$HOME/.rsi/logs"
log_file="$log_dir/ios-resign.log"
last_log="$log_dir/ios-resign.last.log"
mkdir -p "$log_dir" "$(dirname "$state_file")" || exit 1

now="${RSI_IOS_RESIGN_NOW:-$(date -u +%s)}"
case "$now" in ''|*[!0-9]*) echo "weekly-resign: bad RSI_IOS_RESIGN_NOW '$now'" >&2; exit 2 ;; esac
case "$due_days" in ''|*[!0-9]*) echo "weekly-resign: bad RSI_IOS_RESIGN_DUE_DAYS '$due_days'" >&2; exit 2 ;; esac
due_secs=$((due_days * 86400))

log_line() { # status exit reason
  printf '%s %s exit=%s %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$1" "$2" "$3" >>"$log_file"
}

fmt_age() { # seconds -> "<d>d<h>h"
  printf '%sd%sh' "$(($1 / 86400))" "$((($1 % 86400) / 3600))"
}

# Gate: age of the last successful install.
age_note="no-state"
last_ok=""
if [ -f "$state_file" ]; then
  last_ok="$(head -n 1 "$state_file" | tr -d '[:space:]')"
  case "$last_ok" in
    ''|*[!0-9]*) age_note="bad-state"; last_ok="" ;;
    *) if [ "${#last_ok}" -gt 12 ]; then age_note="bad-state"; last_ok=""; else last_ok=$((10#$last_ok)); fi ;;
  esac
fi
if [ -n "$last_ok" ]; then
  if [ "$last_ok" -gt "$now" ]; then
    age_note="future-state"
  else
    age=$((now - last_ok))
    age_note="age=$(fmt_age "$age")"
    if [ "$age" -lt "$due_secs" ]; then
      log_line skip 0 "not-due $age_note due-after=${due_days}d"
      exit 0
    fi
  fi
fi

out="$(mktemp "$log_dir/ios-resign.out.XXXXXX")" || exit 1
trap 'rm -f "$out"' EXIT

rc=0
if [ -f "$dev_install" ]; then
  # Run from the repo root so relative paths in dev-install.sh resolve.
  (cd "$here/../.." && /bin/bash "$dev_install" --network-first) >"$out" 2>&1 || rc=$?
else
  echo "dev-install script not found: $dev_install" >"$out"
  rc=127
fi

# One-line, space-collapsed, length-capped reason from the last non-empty line.
last_line="$(grep -v '^[[:space:]]*$' "$out" | tail -n 1 | tr '\t\r' '  ' | tr -s ' ' | cut -c1-160)"
case "$rc" in
  0) status=ok; reason="installed (was $age_note)" ;;
  3) status=failed; reason="needs-phone no reachable unlocked iPhone, will retry ($age_note)" ;;
  4) status=failed; reason="needs-team no signing team, will retry ($age_note)" ;;
  127) status=failed; reason="${last_line:-dev-install not runnable}" ;;
  *) status=failed; reason="${last_line:-build or sign failure}" ;;
esac
[ -n "$reason" ] || reason="-"

if [ "$status" = ok ]; then
  rm -f "$last_log"
  state_tmp="$(mktemp "$state_file.XXXXXX")" && printf '%s\n' "$now" >"$state_tmp" && mv "$state_tmp" "$state_file" || {
    rm -f "${state_tmp:-}"
    reason="installed but state write failed (was $age_note)"
  }
else
  cp "$out" "$last_log"
fi
log_line "$status" "$rc" "$reason"
exit "$rc"
