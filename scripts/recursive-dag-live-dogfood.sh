#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ENV_FILE="${RSI_DAG_DOGFOOD_ENV:-$ROOT_DIR/target/recursive-dag-live-dogfood.env}"

usage() {
    cat <<'EOF'
usage: scripts/recursive-dag-live-dogfood.sh <command>

Commands:
  setup          Build tools, create a fresh live dogfood fixture, and write env file
  env            Print the current dogfood env file
  daemon         Start rsid against the current dogfood fixture
  gate           Enable the gated live scheduler control and print capabilities
  tui            Start rsi against the current dogfood fixture
  claude-login   Log Claude into the current dogfood fixture HOME
  status         Print capabilities, seeded graphs, and live attempts
  commit         Commit the latest live attempt output for the dogfood graph
  clean          Remove the fixture directory and env file; requires CONFIRM=1
  daemon-fresh   Create a fresh fixture and start rsid against it
  tui-ready      Enable gate, print status, and start rsi against the fixture
  commit-status  Commit latest live attempt output, then print status
EOF
}

require_cmd() {
    if ! command -v "$1" >/dev/null 2>&1; then
        printf 'missing required command: %s\n' "$1" >&2
        exit 1
    fi
}

target_dir() {
    cargo metadata --no-deps --format-version 1 | jq -r .target_directory
}

shell_quote() {
    printf '%q' "$1"
}

write_env_file() {
    local target_dir_value="$1"
    local fixture_home="$2"
    local socket_path="$3"
    local graph_id="$4"
    local summary_json="$5"

    mkdir -p "$(dirname "$ENV_FILE")"
    {
        printf 'export TARGET_DIR=%s\n' "$(shell_quote "$target_dir_value")"
        printf 'export RSI_FIXTURE_HOME=%s\n' "$(shell_quote "$fixture_home")"
        printf 'export RSI_SOCKET=%s\n' "$(shell_quote "$socket_path")"
        printf 'export RSI_LIVE_DOGFOOD_GRAPH_ID=%s\n' "$(shell_quote "$graph_id")"
        printf 'export RSI_DAG_DOGFOOD_SUMMARY_JSON=%s\n' "$(shell_quote "$summary_json")"
    } >"$ENV_FILE"
}

load_env_file() {
    if [[ ! -f "$ENV_FILE" ]]; then
        printf 'dogfood env file not found: %s\n' "$ENV_FILE" >&2
        printf 'run: make recursive-dag-live-dogfood-setup\n' >&2
        exit 1
    fi
    # shellcheck disable=SC1090
    source "$ENV_FILE"
}

rpc() {
    "$TARGET_DIR"/debug/rsi-rpc --socket "$RSI_SOCKET" "$@"
}

build_tools() {
    require_cmd cargo
    require_cmd jq
    cargo build -p rsid --features dev-fixtures --bin rsid --bin rsi-recursive-dag-smoke-fixture
    cargo build -p rsi-common --bin rsi-rpc
    cargo build -p rsi --bin rsi
}

setup_fixture() {
    local print_next="${1:-1}"
    require_cmd cargo
    require_cmd jq

    build_tools

    local target_dir_value
    target_dir_value="$(target_dir)"

    local fixture_home
    fixture_home="$(mktemp -d /tmp/rsi-recursive-dag-live.XXXXXX)"

    local socket_path="$fixture_home/.rsi/daemon.sock"
    "$target_dir_value"/debug/rsi-recursive-dag-smoke-fixture \
        --output-home "$fixture_home" \
        --include-live-dogfood-graph \
        >"$fixture_home/recursive-dag-smoke-fixture.stdout.json"

    local summary_json="$fixture_home/recursive-dag-smoke-fixture.json"
    local graph_id
    graph_id="$(jq -r '.ids.live_dogfood_graph_id' "$summary_json")"
    if [[ -z "$graph_id" || "$graph_id" == "null" ]]; then
        printf 'fixture did not expose ids.live_dogfood_graph_id in %s\n' "$summary_json" >&2
        exit 1
    fi

    write_env_file "$target_dir_value" "$fixture_home" "$socket_path" "$graph_id" "$summary_json"

    printf '\nrecursive DAG live dogfood fixture ready\n'
    printf '  env:     %s\n' "$ENV_FILE"
    printf '  home:    %s\n' "$fixture_home"
    printf '  socket:  %s\n' "$socket_path"
    printf '  graph:   %s\n' "$graph_id"
    if [[ "$print_next" == "1" ]]; then
        printf '\nOrdered workflow:\n'
        printf '  make recursive-dag-live-dogfood-daemon\n'
        printf '  make recursive-dag-live-dogfood-claude-login   # if Claude is not logged into this temp HOME\n'
        printf '  make recursive-dag-live-dogfood-gate\n'
        printf '  make recursive-dag-live-dogfood-tui\n'
        printf '  make recursive-dag-live-dogfood-commit\n'
        printf '  make recursive-dag-live-dogfood-status\n'
    fi
}

print_env() {
    load_env_file
    cat "$ENV_FILE"
    printf '\nsummary:\n'
    jq '{output_db, summary_json, ids: {live_dogfood_graph_id: .ids.live_dogfood_graph_id}}' \
        "$RSI_DAG_DOGFOOD_SUMMARY_JSON"
}

start_daemon() {
    load_env_file
    exec env HOME="$RSI_FIXTURE_HOME" \
        RSI_DAEMON_SOCKET_PATH="$RSI_SOCKET" \
        RSI_SMOKE_SUPPRESS_RETRY_RESTORE=true \
        "$TARGET_DIR"/debug/rsid
}

start_fresh_daemon() {
    setup_fixture 0
    printf '\nstarting rsid for fresh recursive DAG live dogfood fixture\n'
    start_daemon
}

enable_gate() {
    load_env_file
    rpc UpdateDaemonConfig \
        --params '{"field":"recursive_dag_live_scheduler_control_enabled","value":true}'
    rpc GetDaemonCapabilities | jq '.result | {
        recursive_dag_live_scheduler_control,
        recursive_dag_live_execution,
        recursive_dag_background_loop
    }'
}

start_tui() {
    load_env_file
    exec env HOME="$RSI_FIXTURE_HOME" \
        RSI_DAEMON_SOCKET_PATH="$RSI_SOCKET" \
        "$TARGET_DIR"/debug/rsi
}

start_ready_tui() {
    enable_gate
    status
    printf '\nstarting rsi for recursive DAG live dogfood fixture\n'
    start_tui
}

claude_login() {
    load_env_file
    require_cmd claude
    HOME="$RSI_FIXTURE_HOME" claude auth login --claudeai
    HOME="$RSI_FIXTURE_HOME" claude auth status
}

status() {
    load_env_file
    printf 'env file: %s\n' "$ENV_FILE"
    printf 'home:     %s\n' "$RSI_FIXTURE_HOME"
    printf 'socket:   %s\n' "$RSI_SOCKET"
    printf 'graph:    %s\n\n' "$RSI_LIVE_DOGFOOD_GRAPH_ID"

    printf 'capabilities:\n'
    rpc GetDaemonCapabilities | jq '.result | {
        recursive_dag_live_scheduler_control,
        recursive_dag_live_execution,
        recursive_dag_background_loop
    }'

    printf '\ngraphs:\n'
    rpc ListRecursiveTaskGraphs --params '{}' | jq '.result[] | {
        id,
        title,
        status,
        execution_mode
    }'

    printf '\nlive attempts for dogfood graph:\n'
    rpc ListRecursiveLiveAttempts \
        --params "{\"graph_id\":\"$RSI_LIVE_DOGFOOD_GRAPH_ID\",\"include_terminal\":true}" \
        | jq '.result[]? | {
            id: .summary.id,
            status: .summary.status,
            session_id: .summary.session_id,
            completed_at: .summary.completed_at
        }'
}

commit_latest_attempt() {
    load_env_file
    local attempts_json
    attempts_json="$(rpc ListRecursiveLiveAttempts \
        --params "{\"graph_id\":\"$RSI_LIVE_DOGFOOD_GRAPH_ID\",\"include_terminal\":true}")"

    local live_attempt_id
    live_attempt_id="$(jq -r '
        .result
        | sort_by(.summary.created_at)
        | reverse
        | map(select(.summary.status != "succeeded"
            and .summary.status != "failed"
            and .summary.status != "cancelled"
            and .summary.status != "blocked"
            and .summary.status != "decomposed"
            and .summary.status != "interrupted"
            and .summary.status != "lost"))
        | first
        | .summary.id // empty
    ' <<<"$attempts_json")"

    if [[ -z "$live_attempt_id" ]]; then
        live_attempt_id="$(jq -r '.result | sort_by(.summary.created_at) | last | .summary.id // empty' \
            <<<"$attempts_json")"
    fi

    if [[ -z "$live_attempt_id" ]]; then
        printf 'no live attempts found for graph %s\n' "$RSI_LIVE_DOGFOOD_GRAPH_ID" >&2
        exit 1
    fi

    printf 'committing live attempt %s\n' "$live_attempt_id"
    rpc CommitRecursiveLiveAttemptOutput \
        --params "{\"live_attempt_id\":\"$live_attempt_id\"}" \
        | tee "$RSI_FIXTURE_HOME/recursive-dag-live-output-commit.json" \
        | jq '{
            live_attempt_status: .result.readback.live_attempt.summary.status,
            task_status: .result.readback.task.status,
            validation_status: .result.validation_result.summary.status,
            output_kind: .result.validation_result.summary.output_kind,
            validation_id: .result.validation_result.summary.validation_id
        }'
}

commit_then_status() {
    commit_latest_attempt
    printf '\npost-commit status:\n'
    status
}

clean_fixture() {
    load_env_file
    if [[ "${CONFIRM:-}" != "1" ]]; then
        printf 'refusing to remove %s without CONFIRM=1\n' "$RSI_FIXTURE_HOME" >&2
        printf 'run: CONFIRM=1 make recursive-dag-live-dogfood-clean\n' >&2
        exit 1
    fi
    rm -rf "$RSI_FIXTURE_HOME"
    rm -f "$ENV_FILE"
    printf 'removed dogfood fixture and env file\n'
}

main() {
    cd "$ROOT_DIR"
    case "${1:-}" in
        setup) setup_fixture ;;
        env) print_env ;;
        daemon) start_daemon ;;
        gate) enable_gate ;;
        tui) start_tui ;;
        claude-login) claude_login ;;
        status) status ;;
        commit) commit_latest_attempt ;;
        clean) clean_fixture ;;
        daemon-fresh) start_fresh_daemon ;;
        tui-ready) start_ready_tui ;;
        commit-status) commit_then_status ;;
        -h|--help|help|"") usage ;;
        *)
            printf 'unknown command: %s\n\n' "$1" >&2
            usage >&2
            exit 1
            ;;
    esac
}

main "$@"
