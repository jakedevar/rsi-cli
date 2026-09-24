#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
config_file="${RSI_REMOTE_CONFIG:-$HOME/.rsi-remote.env}"

if [[ -f "$config_file" ]]; then
  # shellcheck disable=SC1090
  source "$config_file"
fi

host="${1:-${RSI_SSH_HOST:-}}"

if [[ -z "$host" ]]; then
  cat >&2 <<EOF
Missing remote host.

Create $config_file on this Mac, for example:

  RSI_SSH_HOST=71.227.210.118
  RSI_SSH_PORT=2222
  RSI_SSH_USER=jakedevar

Then run:

  $0

Or pass a host directly:

  $0 71.227.210.118
EOF
  exit 2
fi

export RSI_SSH_USER="${RSI_SSH_USER:-jakedevar}"
export RSI_SSH_PORT="${RSI_SSH_PORT:-2222}"

exec "$script_dir/rsi-ssh-tui.sh" "$host"
