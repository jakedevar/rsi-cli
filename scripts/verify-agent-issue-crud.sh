#!/usr/bin/env bash

# V97 guarded Issue control smoke. This owns an isolated HOME, socket, and DB,
# invokes the real rsi-rpc executable against an in-process RpcServer, and
# retains all fixture evidence on failure. It never restarts a live daemon.
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
fixture_parent="${TMPDIR:-/tmp}"
fixture_root="$(mktemp -d "$fixture_parent/rsi-agent-issue-crud.XXXXXX")"
cleanup() {
  status=$?
  if ((status == 0)); then
    case "$fixture_root" in
      "$fixture_parent"/rsi-agent-issue-crud.*) rm -rf -- "$fixture_root" ;;
      *) echo "refusing to remove unexpected fixture root: $fixture_root" >&2 ;;
    esac
  else
    echo "verification failed; retained fixture root: $fixture_root" >&2
  fi
  exit "$status"
}
trap cleanup EXIT

fixture_home="$fixture_root/home"
fixture_tmp="$fixture_root/tmp"
fixture_socket="$fixture_root/daemon.sock"
mkdir -p "$fixture_home" "$fixture_tmp"

cd "$repo_root"
cargo build -q -p rsi-common --bin rsi-rpc

rsi_rpc="$repo_root/target/debug/rsi-rpc"
production_root="$HOME/.rsi"
production_before="$fixture_root/production-before.txt"
production_after="$fixture_root/production-after.txt"

snapshot_path() {
  path="$1"
  if [[ -e "$path" || -S "$path" ]]; then
    stat -Lc 'kind=%F device=%d inode=%i mode=%f size=%s mtime=%Y path=%n' "$path"
    if [[ -f "$path" ]]; then
      sha256sum -- "$path"
    fi
    pids="$(lsof -t -- "$path" 2>/dev/null | sort -nu | paste -sd, - || true)"
    echo "pids=${pids:-none} path=$path"
    if [[ -n "$pids" ]]; then
      while IFS= read -r pid; do
        executable="$(readlink -f "/proc/$pid/exe" 2>/dev/null || true)"
        echo "pid=$pid executable=${executable:-unavailable} path=$path"
        if [[ -n "$executable" && -f "$executable" ]]; then
          sha256sum -- "$executable"
        fi
      done < <(tr ',' '\n' <<<"$pids")
    fi
  else
    echo "absent path=$path"
  fi
}

snapshot_production() {
  output="$1"
  {
    echo "production_root=$production_root"
    snapshot_path "$production_root/daemon.sock"
    snapshot_path "$production_root/rsi.db"
  } >"$output"
}

echo "check: exact 15-verb agent catalog with one AgentUpdateIssueStatus entry"
echo "expected: rsi-rpc advertises only the closed agent control surface"
catalog="$($rsi_rpc agent)"
[[ "$(grep -c '^  Agent' <<<"$catalog")" -eq 15 ]]
[[ "$(grep -c '^  AgentUpdateIssueStatus ' <<<"$catalog")" -eq 1 ]]

echo "check: production socket/database path, inode, PID, and hashes before isolated smoke"
echo "expected: the before/after snapshots are byte-identical"
snapshot_production "$production_before"

echo "check: isolated real rsi-rpc subprocesses via HOME=$fixture_home socket=$fixture_socket"
echo "expected: ordinary create and all seven lead verbs pass replay, CAS, archive, history, denial, restart, and token-remint assertions"
RSI_AGENT_ISSUE_SMOKE_ROOT="$fixture_root" \
  RSI_AGENT_ISSUE_SMOKE_RSI_RPC="$rsi_rpc" \
  cargo test -q -p rsid --lib \
    'rpc::tests::agent_issue_in_process_rpc_server_unix_socket_rsi_rpc_restart_fixture' \
    -- --exact --test-threads=1 --nocapture

snapshot_production "$production_after"
if ! cmp -s "$production_before" "$production_after"; then
  diff -u "$production_before" "$production_after" >&2 || true
  echo "production socket/database custody changed during isolated smoke" >&2
  exit 1
fi

echo "check: isolated fixture never mutated production socket/database custody"
echo "expected: production path, inode, PID, and content hashes are unchanged"
