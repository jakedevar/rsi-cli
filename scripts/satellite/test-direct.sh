#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
tmp="$(mktemp -d /tmp/rsi-satellite-direct-test.XXXXXX)"
trap 'rm -rf -- "$tmp"' EXIT
bin="$tmp/bin"
home="$tmp/hub-home"
mkdir -p "$bin" "$home"
export HOME="$home"
export PATH="$bin:$PATH"
export FAKE_SYSTEMCTL_LOG="$tmp/systemctl.log"
export FAKE_SSH_LOG="$tmp/ssh.log"
: >"$FAKE_SYSTEMCTL_LOG"; : >"$FAKE_SSH_LOG"

fail() { echo "FAIL: $*" >&2; exit 1; }
contains() { grep -Fq -- "$2" "$1" || fail "$1 did not contain: $2"; }

cat >"$bin/ssh" <<'SSH'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"$FAKE_SSH_LOG"
if [[ " $* " == *" -L "* ]]; then
  exit 89
fi
[[ "${1:-}" == -o && "${2:-}" == BatchMode=yes ]] || { echo 'BatchMode missing' >&2; exit 90; }
printf 'RSI_HOME=%s\n' "$FAKE_LAPTOP_HOME"
SSH
chmod +x "$bin/ssh"

cat >"$bin/systemctl" <<'SYSTEMCTL'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"$FAKE_SYSTEMCTL_LOG"
if [[ "$*" == *'restart rsi-satellite-direct@fixture.service'* ]]; then
  socket_path="$(sed -n 's/^RSI_SATELLITE_LOCAL_SOCKET=//p' "$HOME/.config/rsi/satellites/fixture-direct.env")"
  python3 - "$socket_path" <<'PY'
import os, socket, sys
os.makedirs(os.path.dirname(sys.argv[1]), mode=0o700, exist_ok=True)
s = socket.socket(socket.AF_UNIX); s.bind(sys.argv[1]); s.close()
PY
fi
SYSTEMCTL
chmod +x "$bin/systemctl"
cat >"$bin/rsi-rpc" <<'RPC'
#!/usr/bin/env bash
[[ "${FAKE_RPC_FAIL:-0}" != 1 && "${RSI_DAEMON_SOCKET_PATH:-}" == *.sock && "$*" == GetHealthStatus ]]
RPC
chmod +x "$bin/rsi-rpc"
cat >"$bin/journalctl" <<'JOURNAL'
#!/usr/bin/env bash
echo 'journal-direct-fixture'
JOURNAL
chmod +x "$bin/journalctl"

laptop_home="$tmp/laptop-home"
mkdir -p "$laptop_home"
export FAKE_LAPTOP_HOME="$laptop_home"
"$repo_root/scripts/satellite/install-direct.sh" fixture laptop-ssh --port 2222 --jump bastion >"$tmp/install.out"
contains "$tmp/install.out" 'Installed and started rsi-satellite-direct@fixture.service'
contains "$FAKE_SSH_LOG" '-p 2222'
contains "$FAKE_SSH_LOG" '-J bastion'
contains "$FAKE_SYSTEMCTL_LOG" 'enable rsi-satellite-direct@fixture.service'
[[ "$(stat -c '%a' "$HOME/.config/rsi/satellites/fixture-direct.env")" == 600 ]] || fail 'metadata mode is not 0600'
[[ "$(stat -Lc '%a' "$HOME/.rsi/satellites/fixture-direct.sock")" == 600 ]] || fail 'direct socket mode is not 0600'
[[ "$(stat -c '%a' "$HOME/.rsi/satellites")" == 700 ]] || fail 'direct socket directory mode is not 0700'
contains "$HOME/.config/rsi/satellites/fixture-direct.env" "RSI_SATELLITE_REMOTE_SOCKET=$laptop_home/.rsi/daemon.sock"
RSI_SATELLITE_DIR="$HOME/.rsi/satellites" "$repo_root/scripts/satellite/hub-list.sh" >"$tmp/hub-list.out"
contains "$tmp/hub-list.out" 'fixture-direct'
contains "$tmp/hub-list.out" 'healthy'
contains "$repo_root/scripts/satellite/rsi-satellite-direct@.service" 'StartLimitBurst=3'

# A listener alone does not prove that the remote rsid answers RPC.
rm -f "$HOME/.rsi/satellites/fixture-direct.sock"
if FAKE_RPC_FAIL=1 "$repo_root/scripts/satellite/install-direct.sh" fixture laptop-ssh >"$tmp/unhealthy.out" 2>"$tmp/unhealthy.err"; then
  fail 'installer accepted a socket without daemon health'
fi
contains "$tmp/unhealthy.err" 'did not answer GetHealthStatus'
contains "$tmp/unhealthy.err" 'journal-direct-fixture'

# Preparation removes a stale owned socket but refuses a live socket.
socket_path="$HOME/.rsi/satellites/stale-direct.sock"
python3 - "$socket_path" <<'PY'
import socket, sys
s = socket.socket(socket.AF_UNIX); s.bind(sys.argv[1]); s.close()
PY
env RSI_SATELLITE_TARGET=laptop-ssh RSI_SATELLITE_LOCAL_SOCKET="$socket_path" RSI_SATELLITE_REMOTE_SOCKET=/home/laptop/.rsi/daemon.sock \
  "$repo_root/scripts/satellite/direct.sh" --prepare stale
[[ ! -e "$socket_path" ]] || fail 'prepare retained stale socket'

python3 - "$socket_path" <<'PY' &
import socket, sys, time
s = socket.socket(socket.AF_UNIX); s.bind(sys.argv[1]); s.listen(1)
while True: time.sleep(1)
PY
listener_pid=$!
trap 'kill "$listener_pid" 2>/dev/null || true; rm -rf -- "$tmp"' EXIT
for _ in {1..20}; do [[ -S "$socket_path" ]] && break; sleep 0.05; done
if env RSI_SATELLITE_TARGET=laptop-ssh RSI_SATELLITE_LOCAL_SOCKET="$socket_path" RSI_SATELLITE_REMOTE_SOCKET=/home/laptop/.rsi/daemon.sock \
  "$repo_root/scripts/satellite/direct.sh" --prepare stale >"$tmp/live.out" 2>"$tmp/live.err"; then fail 'prepare removed live socket'; fi
contains "$tmp/live.err" 'still accepting connections'
kill "$listener_pid"; wait "$listener_pid" 2>/dev/null || true
rm -f "$socket_path"

python3 - "$socket_path" <<'PY'
import socket, sys
s = socket.socket(socket.AF_UNIX); s.bind(sys.argv[1]); s.close()
PY
chmod 644 "$socket_path"
env RSI_SATELLITE_TARGET=laptop-ssh RSI_SATELLITE_LOCAL_SOCKET="$socket_path" RSI_SATELLITE_REMOTE_SOCKET=/home/laptop/.rsi/daemon.sock \
  "$repo_root/scripts/satellite/direct.sh" --secure stale
[[ "$(stat -c '%a' "$socket_path")" == 600 ]] || fail 'secure did not set direct socket mode'
rm -f "$socket_path"

# The run action retains all transport arguments and hardening options.
cat >"$bin/ssh" <<'SSHLOG'
#!/usr/bin/env bash
printf '%s\n' "$*" >"$FAKE_FORWARD_ARGS"
SSHLOG
chmod +x "$bin/ssh"
export FAKE_FORWARD_ARGS="$tmp/forward.args"
env RSI_SATELLITE_TARGET=laptop-ssh RSI_SATELLITE_SSH_PORT=2222 RSI_SATELLITE_SSH_JUMP=bastion RSI_SATELLITE_SSH_CONFIG=/tmp/ssh.conf \
  RSI_SATELLITE_LOCAL_SOCKET="$HOME/.rsi/satellites/run-direct.sock" RSI_SATELLITE_REMOTE_SOCKET=/home/laptop/.rsi/daemon.sock \
  "$repo_root/scripts/satellite/direct.sh" fixture
contains "$FAKE_FORWARD_ARGS" 'ExitOnForwardFailure=yes'
contains "$FAKE_FORWARD_ARGS" 'BatchMode=yes'
contains "$FAKE_FORWARD_ARGS" 'StreamLocalBindMask=0177'
contains "$FAKE_FORWARD_ARGS" '-p 2222 -J bastion -F /tmp/ssh.conf'
contains "$FAKE_FORWARD_ARGS" '-L '
contains "$FAKE_FORWARD_ARGS" 'laptop-ssh'

echo 'PASS: direct satellite fixtures'
