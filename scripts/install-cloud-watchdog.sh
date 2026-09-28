#!/usr/bin/env bash
set -euo pipefail
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
mkdir -p "$HOME/.rsi/bin" "$HOME/.rsi/cloud" "$HOME/.config/systemd/user"
install -m 700 "$repo_root/scripts/cloud-watchdog.py" "$HOME/.rsi/bin/cloud-watchdog.py"
install -m 644 "$repo_root/scripts/cloud-watchdog.service" "$HOME/.config/systemd/user/cloud-watchdog.service"
install -m 644 "$repo_root/scripts/cloud-watchdog.timer" "$HOME/.config/systemd/user/cloud-watchdog.timer"
printf 'RSI_CLOUD_WATCHDOG_ARGS="--dry-run --threshold-usd 0"\n' > "$HOME/.rsi/cloud/watchdog.env"
restore_live_config() {
  printf 'RSI_CLOUD_WATCHDOG_ARGS="--threshold-usd 150"\n' > "$HOME/.rsi/cloud/watchdog.env"
}
trap restore_live_config EXIT
systemctl --user daemon-reload
systemctl --user start cloud-watchdog.service
tail -n 1 "$HOME/.rsi/cloud-watchdog.log" | grep -q 'reason=budget dry_run=True'
restore_live_config
trap - EXIT
systemctl --user enable --now cloud-watchdog.timer
echo 'cloud-watchdog.timer active; dry-run $0 threshold fired before apply'
