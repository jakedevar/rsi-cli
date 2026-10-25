#!/usr/bin/env bash
# Install the AWS orphan sweeper as a systemd --user timer (5 min).
# Explicit opt-in: run `make install-cloud-watchdog`; release-install never does.
# The sweeper is `rsi-cloud watchdog` (crates/rsi-cloud). It is installed from
# RSI_CLOUD_BIN, else the rsi-cloud next to the checkout's release build, else
# the one on PATH (`make release-install` links it).
set -euo pipefail
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
source_bin="${RSI_CLOUD_BIN:-}"
if [[ -z "$source_bin" ]]; then
  for candidate in "${CARGO_TARGET_DIR:-$repo_root/target}/release/rsi-cloud" "$(command -v rsi-cloud || true)"; do
    if [[ -n "$candidate" && -x "$candidate" ]]; then
      source_bin="$candidate"
      break
    fi
  done
fi
[[ -n "$source_bin" && -x "$source_bin" ]] || {
  echo "install-cloud-watchdog: no rsi-cloud binary; run 'make release-install' (or set RSI_CLOUD_BIN)" >&2
  exit 2
}
mkdir -p "$HOME/.rsi/bin" "$HOME/.rsi/cloud" "$HOME/.config/systemd/user"
install -m 700 "$source_bin" "$HOME/.rsi/bin/rsi-cloud"
install -m 644 "$repo_root/scripts/cloud-watchdog.service" "$HOME/.config/systemd/user/cloud-watchdog.service"
install -m 644 "$repo_root/scripts/cloud-watchdog.timer" "$HOME/.config/systemd/user/cloud-watchdog.timer"
# A dry run proves the profile and the describe calls work before the timer can terminate anything.
"$HOME/.rsi/bin/rsi-cloud" watchdog --dry-run
systemctl --user daemon-reload
systemctl --user enable --now cloud-watchdog.timer
echo 'cloud-watchdog.timer active (every 5 min); log: ~/.rsi/cloud-watchdog.log'
