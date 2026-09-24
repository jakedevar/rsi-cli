#!/usr/bin/env bash

set -euo pipefail

provider=""
scenario=""
while (($#)); do
  case "$1" in
    --provider)
      provider="${2:?--provider requires a value}"
      shift 2
      ;;
    --scenario)
      scenario="${2:?--scenario requires a value}"
      shift 2
      ;;
    *)
      echo "unknown argument: $1" >&2
      exit 2
      ;;
  esac
done

case "$scenario" in
  committed)
    [[ "$provider" == "harness" ]] || {
      echo "committed requires --provider harness" >&2
      exit 2
    }
    tests=(
      rpc::tests::agent_successor_in_process_rpc_server_unix_socket_harness_restart_fixture
    )
    ;;
  restart-before-launch)
    [[ "$provider" == "codex-app-server-fake" ]] || {
      echo "restart-before-launch requires --provider codex-app-server-fake" >&2
      exit 2
    }
    tests=(
      session::launch::tests::agent_successor_codex_app_server_waits_for_handshake_before_commit
      session::launch::tests::agent_successor_uncertain_restart_reuses_stable_launch_identity
    )
    ;;
  stale-lead)
    [[ "$provider" == "harness" ]] || {
      echo "stale-lead requires --provider harness" >&2
      exit 2
    }
    tests=(
      session::launch::tests::agent_successor_stale_commit_revokes_and_settles_non_lead_candidate
      store::successor_reservations::tests::successor_authority_commit_rejects_aba_and_settles_stale
    )
    ;;
  closed-guard)
    [[ -z "$provider" ]] || {
      echo "closed-guard does not accept --provider" >&2
      exit 2
    }
    tests=(
      scheduler::tests::manual_trigger_of_closed_program_guard_never_resumes_session
      session::agent_verbs::tests::closed_program_guard_is_silent_rearms_explicitly_and_recovers_program_output_once
    )
    ;;
  *)
    echo "--scenario must be committed, restart-before-launch, stale-lead, or closed-guard" >&2
    exit 2
    ;;
esac

repo_root="$(git rev-parse --show-toplevel)"
fixture_parent="${TMPDIR:-/tmp}"
fixture_root="$(mktemp -d "$fixture_parent/rsi-agent-successor.XXXXXX")"
cleanup() {
  status=$?
  if ((status == 0)); then
    case "$fixture_root" in
      "$fixture_parent"/rsi-agent-successor.*) rm -rf -- "$fixture_root" ;;
      *) echo "refusing to remove unexpected fixture root: $fixture_root" >&2 ;;
    esac
  else
    echo "verification failed; retained fixture root: $fixture_root" >&2
  fi
  exit "$status"
}
trap cleanup EXIT

export TMPDIR="$fixture_root"
cd "$repo_root"

echo "check: provider=${provider:-none} scenario=$scenario exact_tests=${tests[*]}"
case "$scenario" in
  committed)
    echo "expected: in-process RpcServer Unix-socket transport fixture proves token-bound exact replay/conflict, manager/server reconstruction, native Harness services, and post-commit lead authority; no subprocess rsid or paid provider call"
    ;;
  restart-before-launch)
    echo "expected: production successor launch fixtures use a fake CodexAppServer handshake and stable restart identities; no paid provider call"
    ;;
  stale-lead)
    echo "expected: production/store fixtures reject stale lead commit and preserve the authoritative competing lead"
    ;;
  closed-guard)
    echo "expected: scheduler/agent-service fixtures keep a closed program guard inert until explicit re-registration"
    ;;
esac
for test_name in "${tests[@]}"; do
  cargo test -p rsid --lib "$test_name" -- --exact --test-threads=1
done
