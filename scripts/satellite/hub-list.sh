#!/usr/bin/env bash
set -euo pipefail

satellite_dir="${RSI_SATELLITE_DIR:-$HOME/.rsi/satellites}"
rpc_bin="${RSI_RPC_BIN:-rsi-rpc}"

if [[ ! -d "$satellite_dir" ]]; then
  echo "No satellite directory: $satellite_dir"
  exit 0
fi

shopt -s nullglob
sockets=("$satellite_dir"/*.sock)
if ((${#sockets[@]} == 0)); then
  echo "No satellite sockets in $satellite_dir"
  exit 0
fi

printf '%-24s %-10s %s\n' NAME HEALTH SOCKET
for socket in "${sockets[@]}"; do
  name="$(basename -- "$socket" .sock)"
  if [[ ! -S "$socket" ]]; then
    health="missing"
  elif [[ "$(stat -Lc '%u:%a' -- "$socket")" != "$(id -u):600" ]]; then
    health="insecure"
  elif ! command -v "$rpc_bin" >/dev/null 2>&1; then
    health="unknown (rsi-rpc missing)"
  elif timeout 5 env RSI_DAEMON_SOCKET_PATH="$socket" "$rpc_bin" GetHealthStatus >/dev/null 2>&1; then
    health="healthy"
  else
    health="unreachable"
  fi
  printf '%-24s %-10s %s\n' "$name" "$health" "$socket"
done
