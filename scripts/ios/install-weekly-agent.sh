#!/bin/bash
# Install (or remove) the iOS re-sign launchd user agent (hub #2963, #2979).
#
# A free Apple Personal Team provisioning profile expires after 7 days. The agent
# fires every few hours and scripts/ios/weekly-resign.sh decides: it reinstalls
# with scripts/ios/dev-install.sh from day 5 after the last success and retries
# on every run until an install succeeds (state ~/.rsi/ios-resign.state). The
# first run after install has no state and installs once.
# The plist names only the script path and PATH/HOME: device id, team id and
# Apple ID are read from env/local config when the agent runs, never written here.
#
# Usage: install-weekly-agent.sh [--dry-run] [--uninstall] [--interval-hours 1-24]
# Default: every 6 hours (StartInterval) and once at load. Re-running replaces
# the agent. Works under macOS /bin/bash 3.2.
#
# Test overrides: RSI_IOS_LAUNCHCTL (launchctl binary), RSI_IOS_AGENT_DIR
# (LaunchAgents directory), RSI_IOS_UID (domain uid).
set -eu

LABEL="dev.rsi.ios-resign"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
resign_script="$here/weekly-resign.sh"

dry_run=0
uninstall=0
interval_hours=6

usage() {
  sed -n '2,14p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

need_int() { # name value max
  case "$2" in
    ''|*[!0-9]*) echo "install-weekly-agent: $1 must be a number, got '$2'" >&2; exit 2 ;;
  esac
  if [ "$2" -gt "$3" ]; then
    echo "install-weekly-agent: $1 must be <= $3, got '$2'" >&2
    exit 2
  fi
}

while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run) dry_run=1 ;;
    --uninstall) uninstall=1 ;;
    --interval-hours)
      if [ $# -lt 2 ]; then echo "install-weekly-agent: $1 needs a value" >&2; exit 2; fi
      interval_hours="$2"
      shift
      ;;
    -h|--help) usage; exit 0 ;;
    *) echo "install-weekly-agent: unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
  shift
done
need_int interval-hours "$interval_hours" 24
if [ "$interval_hours" -lt 1 ]; then
  echo "install-weekly-agent: interval-hours must be >= 1" >&2
  exit 2
fi
interval_secs=$((interval_hours * 3600))

: "${HOME:?HOME must be set}"
launchctl_bin="${RSI_IOS_LAUNCHCTL:-/bin/launchctl}"
agent_dir="${RSI_IOS_AGENT_DIR:-$HOME/Library/LaunchAgents}"
plist_path="$agent_dir/$LABEL.plist"
uid="${RSI_IOS_UID:-$(id -u)}"
domain="gui/$uid"
log_dir="$HOME/.rsi/logs"

xml_escape() {
  printf '%s' "$1" | sed -e 's/&/\&amp;/g' -e 's/</\&lt;/g' -e 's/>/\&gt;/g'
}

render_plist() {
  e_script="$(xml_escape "$resign_script")"
  e_home="$(xml_escape "$HOME")"
  e_log="$(xml_escape "$log_dir/ios-resign.launchd.log")"
  cat <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>$LABEL</string>
  <key>ProgramArguments</key>
  <array>
    <string>/bin/bash</string>
    <string>$e_script</string>
  </array>
  <key>StartInterval</key>
  <integer>$interval_secs</integer>
  <key>RunAtLoad</key>
  <true/>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key>
    <string>/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin:/usr/local/bin</string>
    <key>HOME</key>
    <string>$e_home</string>
  </dict>
  <key>StandardOutPath</key>
  <string>$e_log</string>
  <key>StandardErrorPath</key>
  <string>$e_log</string>
</dict>
</plist>
PLIST
}

if [ "$uninstall" -eq 1 ]; then
  if [ "$dry_run" -eq 1 ]; then
    echo "# dry-run: would run (ignoring 'not loaded'):"
    echo "$launchctl_bin bootout $domain/$LABEL"
    echo "# dry-run: would remove: $plist_path"
    exit 0
  fi
  "$launchctl_bin" bootout "$domain/$LABEL" >/dev/null 2>&1 || true
  rm -f "$plist_path"
  echo "removed $LABEL ($plist_path)"
  exit 0
fi

if [ "$dry_run" -eq 1 ]; then
  echo "# dry-run: plist would be written to: $plist_path"
  echo "--- plist begin ---"
  render_plist
  echo "--- plist end ---"
  echo "# dry-run: would run (bootout is ignored when not loaded):"
  echo "$launchctl_bin bootout $domain/$LABEL"
  echo "$launchctl_bin bootstrap $domain $plist_path"
  exit 0
fi

if [ ! -f "$resign_script" ]; then
  echo "install-weekly-agent: missing $resign_script" >&2
  exit 1
fi
mkdir -p "$agent_dir" "$log_dir"
tmp="$plist_path.tmp.$$"
render_plist > "$tmp"
chmod 644 "$tmp"
mv "$tmp" "$plist_path"
# Idempotent replace: drop any loaded copy first, then load the new plist.
"$launchctl_bin" bootout "$domain/$LABEL" >/dev/null 2>&1 || true
"$launchctl_bin" bootstrap "$domain" "$plist_path"
echo "installed $LABEL: every ${interval_hours}h -> $resign_script"
