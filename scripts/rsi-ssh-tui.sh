#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat >&2 <<'EOF'
usage: rsi-ssh-tui.sh [host]

Run the RSI TUI on a remote machine over SSH, inside a persistent tmux session.

Configuration:
  RSI_SSH_HOST              Remote host/IP. Used when [host] is omitted.
  RSI_SSH_USER              SSH user. Default: jakedevar
  RSI_SSH_PORT              SSH port. Default: 22
  RSI_SSH_IDENTITY_FILE     Optional SSH private key path.
  RSI_TMUX_SESSION          Remote tmux session name. Default: rsi
  RSI_REMOTE_RSI_COMMAND    Command tmux should run. Default: rsi if installed,
                            otherwise cargo run from /home/jakedevar/rsi.

Examples:
  rsi-ssh-tui.sh 71.227.210.118
  RSI_SSH_PORT=2222 rsi-ssh-tui.sh my-home.ddns.net
EOF
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi

host="${1:-${RSI_SSH_HOST:-}}"
if [[ -z "$host" ]]; then
  usage
  exit 2
fi

user="${RSI_SSH_USER:-jakedevar}"
port="${RSI_SSH_PORT:-22}"
identity_file="${RSI_SSH_IDENTITY_FILE:-}"
session="${RSI_TMUX_SESSION:-rsi}"

if [[ ! "$session" =~ ^[A-Za-z0-9_.-]+$ ]]; then
  echo "invalid RSI_TMUX_SESSION: use only letters, numbers, _, ., or -" >&2
  exit 2
fi

target="$host"
if [[ "$host" != *@* ]]; then
  target="${user}@${host}"
fi

remote_rsi_command="${RSI_REMOTE_RSI_COMMAND:-bash -lc 'if command -v rsi >/dev/null 2>&1; then exec rsi; else cd /home/jakedevar/rsi && exec cargo run --bin rsi; fi'}"
remote_command="tmux new-session -A -s '$session' \"$remote_rsi_command\""

ssh_args=(
  -t \
  -p "$port" \
  -o ServerAliveInterval=30 \
  -o ServerAliveCountMax=3
)

if [[ -n "$identity_file" ]]; then
  ssh_args+=(-i "$identity_file")
fi

exec ssh "${ssh_args[@]}" "$target" "$remote_command"
