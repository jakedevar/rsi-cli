#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
unit_dir="$HOME/.config/systemd/user"
config_dir="$HOME/.config/rsi/satellites"
libexec_dir="$HOME/.local/libexec/rsi-satellite"
usage() {
  cat >&2 <<'EOF'
usage: install-direct.sh SATELLITE_NAME SSH_TARGET [SSH_OPTIONS...]

SSH options:
  --port PORT          SSH server port (default: SSH config / OpenSSH default)
  --jump TARGET        ProxyJump target; use SSH config for ProxyCommand
  --ssh-config PATH    Operator-managed SSH config file
EOF
}
if [[ "${1:-}" == -h || "${1:-}" == --help ]]; then usage; exit 0; fi
positionals=(); ssh_port=""; ssh_jump=""; ssh_config=""
while (($#)); do
  case "$1" in
    --port|--jump|--ssh-config)
      option="$1"; (($# >= 2)) || { echo "missing value for $option" >&2; exit 2; }
      case "$option" in --port) ssh_port="$2";; --jump) ssh_jump="$2";; --ssh-config) ssh_config="$2";; esac
      shift 2 ;;
    --*) echo "unknown option: $1" >&2; exit 2 ;;
    *) positionals+=("$1"); shift ;;
  esac
done
((${#positionals[@]} == 2)) || { usage; exit 2; }
name="${positionals[0]}"; target="${positionals[1]}"
[[ "$name" =~ ^[A-Za-z0-9][A-Za-z0-9_-]{0,31}$ ]] || { echo "invalid satellite name" >&2; exit 2; }
[[ "$target" =~ ^[A-Za-z0-9_][A-Za-z0-9_.@:-]*$ ]] || { echo "invalid SSH target" >&2; exit 2; }
if [[ -n "$ssh_port" && ( ! "$ssh_port" =~ ^[0-9]{1,5}$ || 10#$ssh_port -lt 1 || 10#$ssh_port -gt 65535 ) ]]; then echo "invalid SSH port: expected 1 through 65535" >&2; exit 2; fi
[[ -z "$ssh_jump" || "$ssh_jump" =~ ^[A-Za-z0-9_][A-Za-z0-9_.@,:-]*$ ]] || { echo "invalid SSH jump target" >&2; exit 2; }
if [[ -n "$ssh_config" && ( ! "$ssh_config" =~ ^/[A-Za-z0-9_./-]+$ || "$ssh_config" == *"/../"* || "$ssh_config" == */.. ) ]]; then echo "invalid SSH config path" >&2; exit 2; fi
[[ -z "$ssh_config" || -r "$ssh_config" ]] || { echo "SSH config is not readable: $ssh_config" >&2; exit 2; }
command -v ssh >/dev/null || { echo "ssh is required" >&2; exit 1; }
command -v systemctl >/dev/null || { echo "systemctl is required" >&2; exit 1; }
command -v rsi-rpc >/dev/null || { echo "rsi-rpc is required for the direct link health check" >&2; exit 1; }
command -v timeout >/dev/null || { echo "timeout is required for the direct link health check" >&2; exit 1; }

ssh_args=(-o BatchMode=yes -o ConnectTimeout=10 -o ExitOnForwardFailure=yes)
[[ -z "$ssh_port" ]] || ssh_args+=(-p "$ssh_port")
[[ -z "$ssh_jump" ]] || ssh_args+=(-J "$ssh_jump")
[[ -z "$ssh_config" ]] || ssh_args+=(-F "$ssh_config")
remote_output="$(ssh "${ssh_args[@]}" -- "$target" sh -s <<'SH'
printf 'RSI_HOME=%s\n' "$HOME"
SH
)"
remote_home="$(printf '%s\n' "$remote_output" | sed -n 's/^RSI_HOME=//p' | tail -n 1)"
if [[ ! "$remote_home" =~ ^/[A-Za-z0-9_./-]+$ || "$remote_home" == *"/../"* || "$remote_home" == */.. ]]; then
  echo "could not resolve a safe absolute home directory for $target" >&2; exit 1
fi
remote_socket="$remote_home/.rsi/daemon.sock"
local_socket="$HOME/.rsi/satellites/$name-direct.sock"
[[ "$local_socket" =~ ^/[A-Za-z0-9_./-]+$ && "$local_socket" != *"/../"* ]] || { echo "unsafe local socket path" >&2; exit 1; }
if ((${#local_socket} > 107 || ${#remote_socket} > 107)); then
  echo "Unix socket paths must be at most 107 bytes" >&2; exit 2
fi

mkdir -p -- "$unit_dir" "$config_dir" "$libexec_dir"
chmod 700 -- "$config_dir"
install -m 0644 "$script_dir/rsi-satellite-direct@.service" "$unit_dir/rsi-satellite-direct@.service"
install -m 0755 "$script_dir/direct.sh" "$libexec_dir/direct.sh"
config_file="$config_dir/$name-direct.env"
umask 077
cat >"$config_file" <<EOF
RSI_SATELLITE_TARGET=$target
RSI_SATELLITE_SSH_PORT=$ssh_port
RSI_SATELLITE_SSH_JUMP=$ssh_jump
RSI_SATELLITE_SSH_CONFIG=$ssh_config
RSI_SATELLITE_LOCAL_SOCKET=$local_socket
RSI_SATELLITE_REMOTE_SOCKET=$remote_socket
EOF
chmod 600 -- "$config_file"

unit="rsi-satellite-direct@$name.service"
systemctl --user daemon-reload
systemctl --user enable "$unit"
systemctl --user restart "$unit"
attempt=0
while ((attempt < 30)); do
  if [[ -S "$local_socket" && ! -L "$local_socket" ]]; then
    [[ "$(stat -c '%u' -- "$local_socket")" == "$(id -u)" ]] || { echo "direct socket has unexpected owner" >&2; exit 1; }
    chmod 600 -- "$local_socket"
    if ! timeout 5 env RSI_DAEMON_SOCKET_PATH="$local_socket" rsi-rpc GetHealthStatus >/dev/null 2>&1; then
      echo "Direct socket exists, but the satellite daemon did not answer GetHealthStatus: $local_socket" >&2
      journalctl --user -u "$unit" -n 20 --no-pager >&2 || true
      exit 1
    fi
    echo "Installed and started $unit"
    echo "Direct socket: $local_socket"
    echo "Status: systemctl --user status $unit"
    exit 0
  fi
  sleep 1; attempt=$((attempt + 1))
done
echo "Direct socket did not become available after restarting $unit." >&2
journalctl --user -u "$unit" -n 20 --no-pager >&2 || true
exit 1
