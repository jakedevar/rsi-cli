#!/usr/bin/env bash
set -euo pipefail

usage() { echo "usage: direct.sh [--prepare|--secure] SATELLITE_NAME" >&2; }
action=run
if [[ "${1:-}" == --prepare || "${1:-}" == --secure ]]; then action="${1#--}"; shift; fi
if [[ $# -ne 1 || ! "$1" =~ ^[A-Za-z0-9][A-Za-z0-9_-]{0,31}$ ]]; then usage; exit 2; fi

: "${RSI_SATELLITE_TARGET:?missing RSI_SATELLITE_TARGET}"
: "${RSI_SATELLITE_LOCAL_SOCKET:?missing RSI_SATELLITE_LOCAL_SOCKET}"
: "${RSI_SATELLITE_REMOTE_SOCKET:?missing RSI_SATELLITE_REMOTE_SOCKET}"
ssh_port="${RSI_SATELLITE_SSH_PORT:-}"
ssh_jump="${RSI_SATELLITE_SSH_JUMP:-}"
ssh_config="${RSI_SATELLITE_SSH_CONFIG:-}"
if [[ ! "$RSI_SATELLITE_TARGET" =~ ^[A-Za-z0-9_][A-Za-z0-9_.@:-]*$ ||
      ! "$RSI_SATELLITE_LOCAL_SOCKET" =~ ^/[A-Za-z0-9_./-]+$ ||
      ! "$RSI_SATELLITE_REMOTE_SOCKET" =~ ^/[A-Za-z0-9_./-]+$ ||
      "$RSI_SATELLITE_LOCAL_SOCKET" == *"/../"* || "$RSI_SATELLITE_LOCAL_SOCKET" == */.. ||
      "$RSI_SATELLITE_REMOTE_SOCKET" == *"/../"* || "$RSI_SATELLITE_REMOTE_SOCKET" == */.. ]]; then
  echo "invalid direct satellite target or socket path" >&2; exit 2
fi
if ((${#RSI_SATELLITE_LOCAL_SOCKET} > 107 || ${#RSI_SATELLITE_REMOTE_SOCKET} > 107)); then
  echo "Unix socket paths must be at most 107 bytes" >&2; exit 2
fi
if [[ -n "$ssh_port" && ( ! "$ssh_port" =~ ^[0-9]{1,5}$ || 10#$ssh_port -lt 1 || 10#$ssh_port -gt 65535 ) ]]; then echo "invalid SSH port" >&2; exit 2; fi
if [[ -n "$ssh_jump" && ! "$ssh_jump" =~ ^[A-Za-z0-9_][A-Za-z0-9_.@,:-]*$ ]]; then echo "invalid SSH jump target" >&2; exit 2; fi
if [[ -n "$ssh_config" && ( ! "$ssh_config" =~ ^/[A-Za-z0-9_./-]+$ || "$ssh_config" == *"/../"* || "$ssh_config" == */.. ) ]]; then echo "invalid SSH config path" >&2; exit 2; fi

socket_dir="$(dirname -- "$RSI_SATELLITE_LOCAL_SOCKET")"
secure_socket_dir() {
  if [[ -L "$socket_dir" ]]; then echo "refusing symlink socket directory: $socket_dir" >&2; return 1; fi
  mkdir -p -- "$socket_dir"
  [[ -d "$socket_dir" && ! -L "$socket_dir" ]] || { echo "invalid socket directory: $socket_dir" >&2; return 1; }
  [[ "$(stat -c '%u' -- "$socket_dir")" == "$(id -u)" ]] || { echo "socket directory is not owned by this user" >&2; return 1; }
  chmod 700 -- "$socket_dir"
}
if [[ "$action" == prepare ]]; then
  secure_socket_dir
  python3 - "$RSI_SATELLITE_LOCAL_SOCKET" <<'PY'
import errno, os, socket, stat, sys
path = sys.argv[1]
try: previous = os.lstat(path)
except FileNotFoundError: raise SystemExit(0)
if not stat.S_ISSOCK(previous.st_mode) or previous.st_uid != os.getuid():
    raise SystemExit(f"refusing to replace non-owned socket path: {path}")
probe = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
probe.settimeout(2)
try: probe.connect(path)
except OSError as error:
    if error.errno not in (errno.ECONNREFUSED, errno.ENOENT): raise
else: raise SystemExit(f"socket is still accepting connections: {path}")
finally: probe.close()
try: current = os.lstat(path)
except FileNotFoundError: raise SystemExit(0)
if stat.S_ISSOCK(current.st_mode) and current.st_uid == os.getuid() and (current.st_dev, current.st_ino) == (previous.st_dev, previous.st_ino): os.unlink(path)
PY
  exit 0
fi
if [[ "$action" == secure ]]; then
  attempt=0
  while ((attempt < 300)); do
    if [[ -S "$RSI_SATELLITE_LOCAL_SOCKET" && ! -L "$RSI_SATELLITE_LOCAL_SOCKET" ]]; then
      [[ "$(stat -c '%u' -- "$RSI_SATELLITE_LOCAL_SOCKET")" == "$(id -u)" ]] || { echo "forwarded socket has unexpected owner" >&2; exit 1; }
      chmod 600 -- "$RSI_SATELLITE_LOCAL_SOCKET"
      exit 0
    fi
    sleep 0.1; attempt=$((attempt + 1))
  done
  echo "timed out waiting for direct socket: $RSI_SATELLITE_LOCAL_SOCKET" >&2; exit 1
fi

ssh_args=(-N -T -o ExitOnForwardFailure=yes -o BatchMode=yes -o ConnectTimeout=10 -o ServerAliveInterval=30 -o ServerAliveCountMax=3 -o StreamLocalBindMask=0177)
[[ -z "$ssh_port" ]] || ssh_args+=(-p "$ssh_port")
[[ -z "$ssh_jump" ]] || ssh_args+=(-J "$ssh_jump")
[[ -z "$ssh_config" ]] || ssh_args+=(-F "$ssh_config")
exec ssh "${ssh_args[@]}" -L "${RSI_SATELLITE_LOCAL_SOCKET}:${RSI_SATELLITE_REMOTE_SOCKET}" -- "$RSI_SATELLITE_TARGET"
