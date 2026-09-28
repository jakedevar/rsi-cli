#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
unit_dir="$HOME/.config/systemd/user"
config_dir="$HOME/.config/rsi/satellites"
libexec_dir="$HOME/.local/libexec/rsi-satellite"

usage() {
  cat >&2 <<'EOF'
usage: install.sh SATELLITE_NAME SSH_TARGET [LOCAL_RSID_SOCKET] [SSH_OPTIONS...]

Example:
  ./scripts/satellite/install.sh laptop hub-user@hub.example.net --port 2222 --jump bastion
  ./scripts/satellite/install.sh laptop hub-alias --ssh-config "$HOME/.ssh/rsi-hub.conf"

SSH options:
  --port PORT          SSH server port (default: SSH config / OpenSSH default)
  --jump TARGET        ProxyJump target; use SSH config for ProxyCommand
  --ssh-config PATH    Operator-managed SSH config file (may define ProxyCommand)
EOF
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi

positionals=()
ssh_port=""
ssh_jump=""
ssh_config=""
while (($#)); do
  case "$1" in
    --port|--jump|--ssh-config)
      option="$1"
      if (($# < 2)); then
        echo "missing value for $option" >&2
        exit 2
      fi
      case "$option" in
        --port) ssh_port="$2" ;;
        --jump) ssh_jump="$2" ;;
        --ssh-config) ssh_config="$2" ;;
      esac
      shift 2
      ;;
    *)
      positionals+=("$1")
      shift
      ;;
  esac
done

if ((${#positionals[@]} < 2 || ${#positionals[@]} > 3)); then
  usage
  exit 2
fi

name="${positionals[0]}"
target="${positionals[1]}"
local_socket="${positionals[2]:-$HOME/.rsi/daemon.sock}"

if [[ ! "$name" =~ ^[A-Za-z0-9][A-Za-z0-9_-]{0,31}$ ]]; then
  echo "invalid satellite name: use 1-32 letters, numbers, _ or - (starting with a letter or number)" >&2
  exit 2
fi
if [[ ! "$target" =~ ^[A-Za-z0-9_][A-Za-z0-9_.@:-]*$ ]]; then
  echo "invalid SSH target: use user@host or host with letters, numbers, _, ., @, : or -" >&2
  exit 2
fi
if [[ -n "$ssh_port" ]]; then
  if [[ ! "$ssh_port" =~ ^[0-9]{1,5}$ ]] || ((10#$ssh_port < 1 || 10#$ssh_port > 65535)); then
    echo "invalid SSH port: expected a number from 1 to 65535" >&2
    exit 2
  fi
fi
if [[ -n "$ssh_jump" && ! "$ssh_jump" =~ ^[A-Za-z0-9_][A-Za-z0-9_.@,:-]*$ ]]; then
  echo "invalid SSH jump target" >&2
  exit 2
fi
if [[ -n "$ssh_config" && ( ! "$ssh_config" =~ ^/[A-Za-z0-9_./-]+$ || "$ssh_config" == *"/../"* || "$ssh_config" == */.. ) ]]; then
  echo "invalid SSH config path: expected an absolute path without spaces or '..' components" >&2
  exit 2
fi
if [[ -n "$ssh_config" && ! -r "$ssh_config" ]]; then
  echo "SSH config is not readable: $ssh_config" >&2
  exit 2
fi
if [[ ! "$local_socket" =~ ^/[A-Za-z0-9_./-]+$ || "$local_socket" == *"/../"* || "$local_socket" == */.. ]]; then
  echo "invalid local socket path: expected an absolute path without spaces or '..' components" >&2
  exit 2
fi

command -v ssh >/dev/null || { echo "ssh is required" >&2; exit 1; }
command -v systemctl >/dev/null || { echo "systemctl is required" >&2; exit 1; }

echo "Preparing the hub socket directory on $target..."
setup_ssh_args=(-o ConnectTimeout=10 -o ServerAliveInterval=30 -o ServerAliveCountMax=3)
if [[ -n "$ssh_port" ]]; then setup_ssh_args+=(-p "$ssh_port"); fi
if [[ -n "$ssh_jump" ]]; then setup_ssh_args+=(-J "$ssh_jump"); fi
if [[ -n "$ssh_config" ]]; then setup_ssh_args+=(-F "$ssh_config"); fi
if ! ssh "${setup_ssh_args[@]}" -o BatchMode=yes -- "$target" sh -s <<'SH'
exit 0
SH
then
  cat >&2 <<EOF
Noninteractive SSH to $target failed; the satellite service cannot prompt for a key passphrase.
Load the operator-managed key into an available ssh-agent (or hardware-backed agent), confirm the hub is reachable, then rerun this installer.
This installer does not generate keys or edit SSH keys/configuration.
EOF
  exit 1
fi

if ! ssh "${setup_ssh_args[@]}" -o BatchMode=yes -- "$target" sh -s < "$script_dir/preflight-hub-sshd.sh"; then
  cat >&2 <<EOF
The hub's effective sshd policy prevents this dial-home Unix socket forward.
Ask the hub operator to check sshd -T for this SSH user and client address.
The narrow exception is a Match User/Address block with
AllowStreamLocalForwarding remote and StreamLocalBindUnlink yes; keep
DisableForwarding no for that match. The operator must review and apply the
hub configuration. This installer did not enable the service.
EOF
  exit 1
fi

remote_result="$(ssh "${setup_ssh_args[@]}" -o BatchMode=yes -- "$target" sh -s <<'SH'
umask 077
mkdir -p "$HOME/.rsi/satellites"
chmod 700 "$HOME/.rsi/satellites"
printf 'RSI_HOME=%s\n' "$HOME"
SH
)"
remote_home="$(printf '%s\n' "$remote_result" | sed -n 's/^RSI_HOME=//p' | tail -n 1)"
if [[ ! "$remote_home" =~ ^/[A-Za-z0-9_./-]+$ || "$remote_home" == *"/../"* || "$remote_home" == */.. ]]; then
  echo "could not resolve a safe absolute home directory for $target" >&2
  exit 1
fi

mkdir -p -- "$unit_dir" "$config_dir" "$libexec_dir"
chmod 700 "$config_dir"
install -m 0644 "$script_dir/rsi-satellite@.service" "$unit_dir/rsi-satellite@.service"
install -m 0755 "$script_dir/dial-home.sh" "$libexec_dir/dial-home.sh"

config_file="$config_dir/$name.env"
umask 077
cat >"$config_file" <<EOF
RSI_SATELLITE_HUB=$target
RSI_SATELLITE_SSH_PORT=$ssh_port
RSI_SATELLITE_SSH_JUMP=$ssh_jump
RSI_SATELLITE_SSH_CONFIG=$ssh_config
RSI_SATELLITE_LOCAL_SOCKET=$local_socket
RSI_SATELLITE_REMOTE_SOCKET=$remote_home/.rsi/satellites/$name.sock
EOF
chmod 600 "$config_file"

systemctl --user daemon-reload
systemctl --user enable "rsi-satellite@$name.service"
systemctl --user restart "rsi-satellite@$name.service"
if ! ssh "${setup_ssh_args[@]}" -o BatchMode=yes -- "$target" sh -s -- "$remote_home/.rsi/satellites/$name.sock" <<'SH'
socket_path=$1
attempt=0
while [ "$attempt" -lt 30 ]; do
  if [ -S "$socket_path" ]; then
    exit 0
  fi
  attempt=$((attempt + 1))
  sleep 1
done
echo "timed out waiting for hub socket: $socket_path" >&2
exit 1
SH
then
  echo "Hub socket did not become available after restarting rsi-satellite@$name.service." >&2
  journalctl --user -u "rsi-satellite@$name.service" -n 20 --no-pager >&2 || true
  exit 1
fi
echo "Installed and started rsi-satellite@$name.service"
echo "Hub socket: ~/.rsi/satellites/$name.sock"
echo "Status: systemctl --user status rsi-satellite@$name.service"
