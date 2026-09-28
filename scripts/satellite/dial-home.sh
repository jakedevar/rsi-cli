#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "usage: dial-home.sh [--prepare|--secure] SATELLITE_NAME" >&2
}

action="run"
if [[ "${1:-}" == "--prepare" || "${1:-}" == "--secure" ]]; then
  action="${1#--}"
  shift
fi

if [[ $# -ne 1 || ! "$1" =~ ^[A-Za-z0-9][A-Za-z0-9_-]{0,31}$ ]]; then
  usage
  exit 2
fi

: "${RSI_SATELLITE_HUB:?missing RSI_SATELLITE_HUB in the satellite unit environment}"
: "${RSI_SATELLITE_LOCAL_SOCKET:?missing RSI_SATELLITE_LOCAL_SOCKET in the satellite unit environment}"
: "${RSI_SATELLITE_REMOTE_SOCKET:?missing RSI_SATELLITE_REMOTE_SOCKET in the satellite unit environment}"
ssh_port="${RSI_SATELLITE_SSH_PORT:-}"
ssh_jump="${RSI_SATELLITE_SSH_JUMP:-}"
ssh_config="${RSI_SATELLITE_SSH_CONFIG:-}"

if [[ ! "$RSI_SATELLITE_HUB" =~ ^[A-Za-z0-9_][A-Za-z0-9_.@:-]*$ ||
      ! "$RSI_SATELLITE_LOCAL_SOCKET" =~ ^/[A-Za-z0-9_./-]+$ ||
      ! "$RSI_SATELLITE_REMOTE_SOCKET" =~ ^/[A-Za-z0-9_./-]+$ ||
      "$RSI_SATELLITE_REMOTE_SOCKET" == *"/../"* || "$RSI_SATELLITE_REMOTE_SOCKET" == */.. ]]; then
  echo "invalid satellite SSH target or socket path in the unit environment" >&2
  exit 2
fi
if [[ -n "$ssh_port" && ( ! "$ssh_port" =~ ^[0-9]{1,5}$ || 10#$ssh_port -lt 1 || 10#$ssh_port -gt 65535 ) ]]; then
  echo "invalid SSH port in the unit environment" >&2
  exit 2
fi
if [[ -n "$ssh_jump" && ! "$ssh_jump" =~ ^[A-Za-z0-9_][A-Za-z0-9_.@,:-]*$ ]]; then
  echo "invalid SSH jump target in the unit environment" >&2
  exit 2
fi
if [[ -n "$ssh_config" && ( ! "$ssh_config" =~ ^/[A-Za-z0-9_./-]+$ || "$ssh_config" == *"/../"* || "$ssh_config" == */.. ) ]]; then
  echo "invalid SSH config path in the unit environment" >&2
  exit 2
fi

transport_ssh_args=()
if [[ -n "$ssh_port" ]]; then transport_ssh_args+=(-p "$ssh_port"); fi
if [[ -n "$ssh_jump" ]]; then transport_ssh_args+=(-J "$ssh_jump"); fi
if [[ -n "$ssh_config" ]]; then transport_ssh_args+=(-F "$ssh_config"); fi

if [[ "$action" == "prepare" ]]; then
  prepare_ssh_args=(-o BatchMode=yes -o ConnectTimeout=10 -o ServerAliveInterval=30 -o ServerAliveCountMax=3)
  prepare_ssh_args+=("${transport_ssh_args[@]}" -- "$RSI_SATELLITE_HUB" sh -s -- "$RSI_SATELLITE_REMOTE_SOCKET")
  exec ssh "${prepare_ssh_args[@]}" <<'SH'
exec python3 - "$1" <<'PYTHON'
import errno
import os
import socket
import stat
import sys

path = sys.argv[1]
try:
    previous = os.lstat(path)
except FileNotFoundError:
    raise SystemExit(0)
if not stat.S_ISSOCK(previous.st_mode):
    raise SystemExit(f"refusing to replace non-socket hub path: {path}")

probe = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
probe.settimeout(2)
try:
    probe.connect(path)
except OSError as error:
    if error.errno not in (errno.ECONNREFUSED, errno.ENOENT):
        raise
else:
    raise SystemExit(f"hub socket is still accepting connections: {path}")
finally:
    probe.close()

try:
    current = os.lstat(path)
except FileNotFoundError:
    raise SystemExit(0)
if stat.S_ISSOCK(current.st_mode) and (current.st_dev, current.st_ino) == (previous.st_dev, previous.st_ino):
    os.unlink(path)
PYTHON
SH
fi

if [[ "$action" == "secure" ]]; then
  secure_ssh_args=(-o BatchMode=yes -o ConnectTimeout=10 -o ServerAliveInterval=30 -o ServerAliveCountMax=3)
  secure_ssh_args+=("${transport_ssh_args[@]}" -- "$RSI_SATELLITE_HUB" sh -s -- "$RSI_SATELLITE_REMOTE_SOCKET")
  exec ssh "${secure_ssh_args[@]}" <<'SH'
socket_path=$1
attempt=0
while [ "$attempt" -lt 50 ]; do
  if [ -S "$socket_path" ]; then
    chmod 600 "$socket_path"
    exit 0
  fi
  attempt=$((attempt + 1))
  sleep 0.1
done
echo 'timed out waiting for hub socket' >&2
exit 1
SH
fi

tunnel_ssh_args=(
  -N -T \
  -o ExitOnForwardFailure=yes \
  -o BatchMode=yes \
  -o ConnectTimeout=10 \
  -o ServerAliveInterval=30 \
  -o ServerAliveCountMax=3
)
tunnel_ssh_args+=("${transport_ssh_args[@]}" -R "${RSI_SATELLITE_REMOTE_SOCKET}:${RSI_SATELLITE_LOCAL_SOCKET}" -- "$RSI_SATELLITE_HUB")
exec ssh "${tunnel_ssh_args[@]}"
