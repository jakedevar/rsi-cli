#!/usr/bin/env bash
# Run rsid library tests as bounded feature-selected harnesses.
# The static gate check runs before any Cargo command, so an incomplete
# extraction cannot quietly rebuild the original unsharded lib harness.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

usage() {
    cat >&2 <<'EOF'
Usage: scripts/run-rsid-test-shards.sh fast|full|list|warmup [--jobs N] [--evidence-dir DIR] [--dry-run] [--keep-rsid-artifacts]
       scripts/run-rsid-test-shards.sh shard SHARD [--jobs N] [--filterset EXPR] [--evidence-dir DIR] [--dry-run] [--keep-rsid-artifacts]
       scripts/run-rsid-test-shards.sh list|warmup --profile rsid-fast|ci-full [--jobs N] [--evidence-dir DIR]

fast: all rsid library shards, integration targets, binary targets, and doctests.
full: fast coverage plus every other workspace package's tests and doctests.
list: enumerate the 16 library shards and check their runtime identity union.
warmup: build shard, integration, binary, and selected workspace test artifacts without running tests.
shard: list, check, and run one library shard for a bounded edit/verify loop.
--dry-run checks the static inventory and prints commands without running Cargo.
--keep-rsid-artifacts retains each shard's rsid test build for debugging.
Cleanup skips while another runner uses the same Cargo target directory.
EOF
    exit 2
}

mode="${1:-}"
case "$mode" in fast|full|list|warmup) shift ;; shard) shift; requested_shard="${1:-}"; shift || usage ;; *) usage ;; esac
jobs="${NEXTEST_JOBS:-1}"
filterset=""
evidence_dir=""
dry_run=0
keep_rsid_artifacts=0
list_profile=""
while (( $# )); do
    case "$1" in
        --jobs)
            (( $# >= 2 )) || usage
            jobs="$2"
            shift 2
            ;;
        --filterset)
            (( $# >= 2 )) || usage
            filterset="$2"
            shift 2
            ;;
        --evidence-dir)
            (( $# >= 2 )) || usage
            evidence_dir="$2"
            shift 2
            ;;
        --dry-run)
            dry_run=1
            shift
            ;;
        --keep-rsid-artifacts)
            keep_rsid_artifacts=1
            shift
            ;;
        --profile)
            (( $# >= 2 )) || usage
            list_profile="$2"
            shift 2
            ;;
        *) usage ;;
    esac
done
[[ -z "$filterset" || "$mode" == shard ]] || usage
if [[ -n "$filterset" ]]; then
    filterset_pattern='^test\([A-Za-z0-9_:.-]+\)$'
    [[ "$filterset" =~ $filterset_pattern ]] || usage
fi
[[ "$jobs" =~ ^[1-9][0-9]*$ ]] || usage
if [[ "$mode" == list || "$mode" == warmup || "$mode" == shard ]]; then
    profile="${list_profile:-rsid-fast}"
    [[ "$profile" == rsid-fast || "$profile" == ci-full ]] || usage
else
    [[ -z "$list_profile" ]] || usage
    if [[ "$mode" == full ]]; then profile=ci-full; else profile=rsid-fast; fi
fi

if (( dry_run )); then
    shard_output="$(python3 scripts/check-rsid-test-shards.py --list-shards)"
else
    shard_output="$(python3 scripts/check-rsid-test-shards.py --require-gates --list-shards)"
fi
mapfile -t shards <<<"$shard_output"
(( ${#shards[@]} == 16 )) || { echo 'expected exactly 16 library shards' >&2; exit 1; }
if [[ "$mode" == shard ]]; then
    [[ " ${shards[*]} " == *" $requested_shard "* ]] || usage
    shards=("$requested_shard")
fi

integration_args=()
while IFS= read -r -d '' file; do
    target="${file##*/}"
    integration_args+=(--test "${target%.rs}")
done < <(find crates/rsid/tests -maxdepth 1 -type f -name '*.rs' -print0 | sort -z)
(( ${#integration_args[@]} > 0 )) || { echo 'no rsid integration targets found' >&2; exit 1; }

print_command() {
    printf '%q ' "$@"
    printf '\n'
}

if (( dry_run )); then
    echo "static plan: mode=$mode profile=$profile jobs=$jobs; no Cargo command executed"
    for shard in "${shards[@]}"; do
        print_command cargo nextest list --profile "$profile" -p rsid --lib --no-default-features --features "test-shard-$shard" --message-format json
    done
    runtime_check=(python3 scripts/check-rsid-test-shards.py --require-gates --runtime-dir '<evidence>/lists')
    if [[ "$mode" == shard ]]; then runtime_check+=(--runtime-shard "$requested_shard"); fi
    print_command "${runtime_check[@]}"
    if [[ "$mode" == warmup ]]; then
        print_command cargo nextest run --profile "$profile" -p rsid "${integration_args[@]}" --no-run
        print_command cargo nextest run --profile "$profile" -p rsid --bins --no-run
        if [[ "$profile" == ci-full ]]; then
            print_command cargo nextest run --profile ci-full --workspace --exclude rsid --no-run
        fi
    elif [[ "$mode" != list ]]; then
        for shard in "${shards[@]}"; do
            run_args=(cargo nextest run --profile "$profile" -p rsid --lib --no-default-features --features "test-shard-$shard" --status-level all --final-status-level all -j "$jobs")
            if [[ -n "$filterset" ]]; then run_args+=(--filterset "$filterset"); fi
            print_command "${run_args[@]}"
            if (( ! keep_rsid_artifacts )); then
                echo '# clean only with an exclusive target-directory lock; otherwise skip'
                print_command cargo clean -p rsid --profile test
            fi
        done
        if [[ "$mode" != shard ]]; then
            print_command cargo nextest run --profile "$profile" -p rsid "${integration_args[@]}" --status-level all --final-status-level all -j "$jobs"
            print_command cargo nextest run --profile "$profile" -p rsid --bins --status-level all --final-status-level all -j "$jobs"
            if [[ "$mode" == full ]]; then
                print_command cargo nextest run --profile ci-full --workspace --exclude rsid --status-level all --final-status-level all -j "$jobs"
            fi
            print_command cargo test -p rsid --doc -- --test-threads="$jobs"
            if [[ "$mode" == full ]]; then
                print_command cargo test --workspace --exclude rsid --doc -- --test-threads="$jobs"
            fi
        fi
    fi
    exit 0
fi

# Keep one target and the caller's compiler job limit across every shard. Callers may select
# the shared target directory with CARGO_TARGET_DIR; this runner never copies it.
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-1}" CARGO_INCREMENTAL=0 CARGO_PROFILE_TEST_DEBUG=0
# Cargo resolves environment, workspace and global target-dir settings for us.
# Keep this lock outside the package artifacts removed by `cargo clean -p`.
target_dir="$(cargo metadata --no-deps --format-version 1 | python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])')"
mkdir -p "$target_dir"
exec {artifact_lock_fd}>"$target_dir/.rsid-test-shards.lock"
flock --shared "$artifact_lock_fd"
if [[ -z "$evidence_dir" ]]; then
    mkdir -p target/rsid-test-shards
    evidence_dir="$(mktemp -d "target/rsid-test-shards/${mode}.XXXXXX")"
else
    mkdir -p "$(dirname "$evidence_dir")"
    mkdir "$evidence_dir" || { echo "evidence directory already exists: $evidence_dir" >&2; exit 1; }
fi
mkdir "$evidence_dir/lists" "$evidence_dir/logs"
evidence_dir="$(cd "$evidence_dir" && pwd)"
printf 'label\texit\n' >"$evidence_dir/status.tsv"
echo "rsid shard evidence: $evidence_dir"

record_command() {
    local label="$1"
    shift
    printf '%s\t' "$label" >>"$evidence_dir/commands.tsv"
    printf '%q ' "$@" >>"$evidence_dir/commands.tsv"
    printf '\n' >>"$evidence_dir/commands.tsv"
}

run_logged() {
    local label="$1" status
    shift
    record_command "$label" "$@"
    echo "running $label"
    if "$@" >"$evidence_dir/logs/$label.stdout" 2>"$evidence_dir/logs/$label.stderr"; then
        status=0
    else
        status=$?
    fi
    printf '%s\t%s\n' "$label" "$status" >>"$evidence_dir/status.tsv"
    cat "$evidence_dir/logs/$label.stdout" "$evidence_dir/logs/$label.stderr"
    (( status == 0 )) || return "$status"
}

clean_rsid_artifacts() {
    local label="$1" final="${2:-0}" clean_status=0
    # Explicitly release our shared lock before trying exclusively. A new run
    # waits for any exclusive clean before it can build or execute artifacts.
    flock --unlock "$artifact_lock_fd"
    if flock --exclusive --nonblock "$artifact_lock_fd"; then
        run_logged "$label" cargo clean -p rsid --profile test || clean_status=$?
    else
        echo "skipping $label: another rsid shard runner is using $target_dir" | tee "$evidence_dir/logs/$label.stdout"
        printf '%s\t0\n' "$label" >>"$evidence_dir/status.tsv"
    fi
    if (( ! final )); then
        flock --shared "$artifact_lock_fd"
    fi
    return "$clean_status"
}

finish_artifacts() {
    local status=$? clean_status=0 label=clean-final
    if [[ "$mode" == shard ]]; then label="clean-$requested_shard"; fi
    # Do not reacquire shared here: the last exiting runner must be able to
    # clean even when every intermediate cleanup skipped because of overlap.
    clean_rsid_artifacts "$label" 1 || clean_status=$?
    if (( status == 0 )); then status=$clean_status; fi
    exit "$status"
}
if (( ! keep_rsid_artifacts )) && [[ "$mode" != list && "$mode" != warmup ]]; then
    trap finish_artifacts EXIT
fi

for shard in "${shards[@]}"; do
    label="list-$shard"
    record_command "$label" cargo nextest list --profile "$profile" -p rsid --lib --no-default-features --features "test-shard-$shard" --message-format json
    echo "listing $shard"
    if cargo nextest list --profile "$profile" -p rsid --lib --no-default-features --features "test-shard-$shard" --message-format json >"$evidence_dir/lists/$shard.json" 2>"$evidence_dir/logs/$label.stderr"; then
        status=0
    else
        status=$?
    fi
    printf '%s\t%s\n' "$label" "$status" >>"$evidence_dir/status.tsv"
    cat "$evidence_dir/logs/$label.stderr"
    (( status == 0 )) || exit "$status"
done
runtime_check=(python3 scripts/check-rsid-test-shards.py --require-gates --runtime-dir "$evidence_dir/lists")
if [[ "$mode" == shard ]]; then runtime_check+=(--runtime-shard "$requested_shard"); fi
run_logged runtime-union "${runtime_check[@]}"
if [[ "$mode" == list ]]; then exit 0; fi
if [[ "$mode" == warmup ]]; then
    run_logged warmup-integrations cargo nextest run --profile "$profile" -p rsid "${integration_args[@]}" --no-run
    run_logged warmup-bins cargo nextest run --profile "$profile" -p rsid --bins --no-run
    if [[ "$profile" == ci-full ]]; then
        run_logged warmup-other-workspace cargo nextest run --profile ci-full --workspace --exclude rsid --no-run
    fi
    exit 0
fi

for shard in "${shards[@]}"; do
    run_args=(cargo nextest run --profile "$profile" -p rsid --lib --no-default-features --features "test-shard-$shard" --status-level all --final-status-level all -j "$jobs")
    if [[ -n "$filterset" ]]; then run_args+=(--filterset "$filterset"); fi
    if run_logged "run-$shard" "${run_args[@]}"; then
        run_status=0
    else
        run_status=$?
    fi
    if (( ! keep_rsid_artifacts )) && [[ "$mode" != shard ]]; then
        clean_rsid_artifacts "clean-$shard"
    fi
    (( run_status == 0 )) || exit "$run_status"
done
if [[ "$mode" == shard ]]; then exit 0; fi
run_logged rsid-integrations cargo nextest run --profile "$profile" -p rsid "${integration_args[@]}" --status-level all --final-status-level all -j "$jobs"
run_logged rsid-bins cargo nextest run --profile "$profile" -p rsid --bins --status-level all --final-status-level all -j "$jobs"
if [[ "$mode" == full ]]; then
    run_logged other-workspace cargo nextest run --profile ci-full --workspace --exclude rsid --status-level all --final-status-level all -j "$jobs"
fi
run_logged rsid-doctests cargo test -p rsid --doc -- --test-threads="$jobs"
if [[ "$mode" == full ]]; then
    run_logged other-doctests cargo test --workspace --exclude rsid --doc -- --test-threads="$jobs"
fi
