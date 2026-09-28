#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
tmp="$(mktemp -d /tmp/rsi-satellite-test.XXXXXX)"
trap 'rm -rf -- "$tmp"' EXIT

bin="$tmp/bin"
home_dir="$tmp/laptop-home"
hub_home="$tmp/hub-home"
mkdir -p "$bin" "$home_dir" "$hub_home"
export HOME="$home_dir"
export FAKE_HUB_HOME="$hub_home"
export FAKE_SSH_LOG="$tmp/ssh.log"
export FAKE_SYSTEMCTL_LOG="$tmp/systemctl.log"
export PATH="$bin:$PATH"
: > "$FAKE_SSH_LOG"
: > "$FAKE_SYSTEMCTL_LOG"

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

contains() {
  grep -Fq -- "$2" "$1" || fail "$1 did not contain: $2"
}

not_contains() {
  if grep -Fq -- "$2" "$1"; then
    fail "$1 unexpectedly contained: $2"
  fi
}

cat > "$bin/ssh" <<'SSH'
#!/usr/bin/env bash
set -euo pipefail
args=("$@")
remote_command=""
batch_mode=0
while (($#)); do
  case "$1" in
    --)
      shift
      break
      ;;
    -o)
      [[ "${2:-}" == BatchMode=yes ]] && batch_mode=1
      shift 2
      ;;
    -p|-J|-F|-R)
      shift 2
      ;;
    -N|-T)
      shift
      ;;
    *)
      shift
      ;;
  esac
done
[[ $# -gt 0 ]] || { echo 'fake ssh: missing target' >&2; exit 90; }
target="$1"
shift
if [[ "${FAKE_SSH_FAIL_BATCHMODE:-0}" == 1 && "$batch_mode" == 1 ]]; then
  echo 'fake ssh: BatchMode authentication unavailable' >&2
  exit 255
fi
if [[ "${1:-}" != sh || "${2:-}" != -s ]]; then
  printf 'fake fish remote shell rejected command:' >&2
  printf ' %q' "$@" >&2
  printf '\n' >&2
  exit 91
fi
printf 'command=sh -s\n' >> "$FAKE_SSH_LOG"
((batch_mode)) && printf 'batchmode=yes\n' >> "$FAKE_SSH_LOG"
for arg in "${args[@]}"; do
  case "$arg" in
    -p|-J|-F) ;;
    *) printf 'arg=%s\n' "$arg" >> "$FAKE_SSH_LOG" ;;
  esac
done
shift 2
if [[ "${1:-}" == -- ]]; then
  shift
fi
HOME="$FAKE_HUB_HOME" FAKE_HUB_HOME="$FAKE_HUB_HOME" \
  SSH_CONNECTION='192.0.2.10 51234 192.0.2.20 22' sh -s "$@"
SSH
chmod +x "$bin/ssh"

cat > "$bin/sshd" <<'SSHD'
#!/usr/bin/env bash
set -euo pipefail
[[ "${1:-}" == -T && "${2:-}" == -C && "${3:-}" == *'addr=192.0.2.10'* ]] || exit 93
printf 'allowstreamlocalforwarding %s\n' "${FAKE_SSHD_FORWARDING:-remote}"
printf 'streamlocalbindunlink %s\n' "${FAKE_SSHD_BIND_UNLINK:-yes}"
printf 'disableforwarding %s\n' "${FAKE_SSHD_DISABLE:-no}"
SSHD
chmod +x "$bin/sshd"

cat > "$bin/systemctl" <<'SYSTEMCTL'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >> "$FAKE_SYSTEMCTL_LOG"
if [[ "$*" == *'restart rsi-satellite@fixture.service'* && "${FAKE_NO_SOCKET:-0}" != 1 ]]; then
  config_file="$HOME/.config/rsi/satellites/fixture.env"
  socket_path="$(sed -n 's/^RSI_SATELLITE_REMOTE_SOCKET=//p' "$config_file")"
  python3 - "$socket_path" <<'PY'
import os, socket, sys
path = sys.argv[1]
os.makedirs(os.path.dirname(path), exist_ok=True)
sock = socket.socket(socket.AF_UNIX)
sock.bind(path)
sock.close()
PY
fi
SYSTEMCTL
chmod +x "$bin/systemctl"

cat > "$bin/journalctl" <<'JOURNAL'
#!/usr/bin/env bash
set -euo pipefail
[[ "$*" == '--user -u rsi-satellite@fixture.service -n 20 --no-pager' ]] || {
  echo "unexpected journalctl invocation: $*" >&2
  exit 92
}
printf 'journal-tail-fixture\n'
JOURNAL
chmod +x "$bin/journalctl"

cat > "$bin/sleep" <<'SLEEP'
#!/usr/bin/env bash
exit 0
SLEEP
chmod +x "$bin/sleep"

# The fish-like remote endpoint rejects inline POSIX snippets.
if ssh -- hub-user@hub.example.net 'socket_path=/tmp/socket; while true; do :; done' >"$tmp/rejected.out" 2>"$tmp/rejected.err"; then
  fail 'fake fish remote shell accepted a bare POSIX assignment/while command'
fi
contains "$tmp/rejected.err" 'fake fish remote shell rejected command'

# BatchMode failures must give an operator action and must happen before enable.
if FAKE_SSH_FAIL_BATCHMODE=1 "$repo_root/scripts/satellite/install.sh" fixture hub-user@hub.example.net >"$tmp/auth.out" 2>"$tmp/auth.err"; then
  fail 'installer accepted unavailable BatchMode SSH'
fi
contains "$tmp/auth.err" 'Load the operator-managed key into an available ssh-agent'
not_contains "$FAKE_SYSTEMCTL_LOG" 'enable rsi-satellite@fixture.service'

# Effective hub sshd policy is checked before the unit is enabled.
for setting in FORWARDING BIND_UNLINK DISABLE; do
  case "$setting" in
    FORWARDING) override=(FAKE_SSHD_FORWARDING=no) ;;
    BIND_UNLINK) override=(FAKE_SSHD_BIND_UNLINK=no) ;;
    DISABLE) override=(FAKE_SSHD_DISABLE=yes) ;;
  esac
  if env "${override[@]}" "$repo_root/scripts/satellite/install.sh" fixture hub-user@hub.example.net >"$tmp/policy.out" 2>"$tmp/policy.err"; then
    fail "installer accepted blocked hub sshd $setting"
  fi
  contains "$tmp/policy.err" 'The narrow exception is a Match User/Address block'
  not_contains "$FAKE_SYSTEMCTL_LOG" 'enable rsi-satellite@fixture.service'
done

# Successful install proves both setup and readiness use explicit sh -s.
"$repo_root/scripts/satellite/install.sh" fixture hub-user@hub.example.net >"$tmp/install.out" 2>"$tmp/install.err" || { cat "$tmp/install.err" >&2; fail 'installer failed with a socket available'; }
contains "$tmp/install.out" 'Installed and started rsi-satellite@fixture.service'
contains "$tmp/install.out" 'Hub sshd permits remote Unix socket forwarding'
contains "$FAKE_SSH_LOG" 'command=sh -s'
contains "$FAKE_SSH_LOG" 'batchmode=yes'
contains "$FAKE_SYSTEMCTL_LOG" 'enable rsi-satellite@fixture.service'
contains "$FAKE_SYSTEMCTL_LOG" 'restart rsi-satellite@fixture.service'

# A missing socket must fail and surface the unit journal tail.
rm -f "$FAKE_SYSTEMCTL_LOG"
: > "$FAKE_SYSTEMCTL_LOG"
rm -f "$hub_home/.rsi/satellites/fixture.sock"
if FAKE_NO_SOCKET=1 "$repo_root/scripts/satellite/install.sh" fixture hub-user@hub.example.net >"$tmp/timeout.out" 2>"$tmp/timeout.err"; then
  fail 'installer succeeded without a hub socket'
fi
contains "$tmp/timeout.err" 'Hub socket did not become available'
contains "$tmp/timeout.err" 'journal-tail-fixture'
not_contains "$tmp/timeout.out" 'Installed and started'

# Prepare removes only a stale socket through a POSIX sh wrapper.
prepare_socket="$tmp/prepare.sock"
python3 - "$prepare_socket" <<'PY'
import socket, sys
sock = socket.socket(socket.AF_UNIX)
sock.bind(sys.argv[1])
sock.close()
PY
RSI_SATELLITE_HUB=hub-user@hub.example.net \
RSI_SATELLITE_LOCAL_SOCKET="$tmp/local.sock" \
RSI_SATELLITE_REMOTE_SOCKET="$prepare_socket" \
  "$repo_root/scripts/satellite/dial-home.sh" --prepare fixture
[[ ! -e "$prepare_socket" ]] || fail 'prepare did not remove the stale socket'

# Secure waits for a socket and applies mode 0600 through the same wrapper.
secure_socket="$tmp/secure.sock"
python3 - "$secure_socket" <<'PY'
import socket, sys
sock = socket.socket(socket.AF_UNIX)
sock.bind(sys.argv[1])
sock.close()
PY
chmod 644 "$secure_socket"
RSI_SATELLITE_HUB=hub-user@hub.example.net \
RSI_SATELLITE_LOCAL_SOCKET="$tmp/local.sock" \
RSI_SATELLITE_REMOTE_SOCKET="$secure_socket" \
RSI_SATELLITE_SSH_PORT=2222 \
RSI_SATELLITE_SSH_JUMP=bastion \
RSI_SATELLITE_SSH_CONFIG=/tmp/ssh-config \
  "$repo_root/scripts/satellite/dial-home.sh" --secure fixture
[[ "$(stat -c '%a' "$secure_socket")" == 600 ]] || fail 'secure did not set socket mode 0600'
contains "$FAKE_SSH_LOG" 'arg=2222'
contains "$FAKE_SSH_LOG" 'arg=bastion'
contains "$FAKE_SSH_LOG" 'arg=/tmp/ssh-config'

printf 'PASS: satellite remote shell fixtures\n'
