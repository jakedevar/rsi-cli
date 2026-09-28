#!/usr/bin/env bash
# Transfer one exact rolling commit over SSH and run the complete test matrix.
set -euo pipefail
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

usage() {
  echo 'usage: cloud-sweep.sh hub|collect SHA ec2-user@IP [SSH_KEY] | remote SHA' >&2
  exit 2
}

[[ $# -ge 2 ]] || usage
mode="$1"
sha="$2"
[[ "$sha" =~ ^[0-9a-f]{40}$ ]] || usage

if [[ "$mode" == hub || "$mode" == collect ]]; then
  [[ $# -ge 3 && $# -le 4 ]] || usage
  host="$3"
  key="${4:-$HOME/.ssh/id_ed25519_rsi}"
  [[ "$host" =~ ^ec2-user@[0-9.]+$ && -f "$key" ]] || usage
  ssh_opts=(-i "$key" -o IdentitiesOnly=yes -o StrictHostKeyChecking=accept-new)
  if [[ "$mode" == hub ]]; then
    cd "$repo_root"
    git fetch origin rolling
    [[ "$(git rev-parse refs/remotes/origin/rolling)" == "$sha" ]] || {
      echo 'requested SHA is not the current rolling tip' >&2
      exit 1
    }
    bundle="$(mktemp /tmp/rsi-cloud-sweep.XXXXXX.bundle)"
    trap 'rm -f "$bundle"' EXIT
    git bundle create "$bundle" refs/remotes/origin/rolling
    git bundle list-heads "$bundle" | grep -q "^$sha refs/remotes/origin/rolling$"
    scp "${ssh_opts[@]}" "$bundle" "$host:/tmp/rsi-cloud-$sha.bundle"
    ssh "${ssh_opts[@]}" "$host" "bash -s -- $sha" <<'REMOTE'
set -euo pipefail
sha="$1"
sudo install -d -o rsi -g rsi /srv/rsi/incoming /srv/rsi/src /srv/rsi/sweeps /srv/rsi/sweeps/source
command -v python3.11 >/dev/null 2>&1 || sudo dnf install -y python3.11
sudo mv "/tmp/rsi-cloud-$sha.bundle" "/srv/rsi/incoming/$sha.bundle"
sudo chown rsi:rsi "/srv/rsi/incoming/$sha.bundle"
sudo -H -u rsi git -C /srv/rsi/src init -q
sudo -H -u rsi git -C /srv/rsi/src fetch -q "/srv/rsi/incoming/$sha.bundle" refs/remotes/origin/rolling
test "$(sudo -H -u rsi git -C /srv/rsi/src rev-parse FETCH_HEAD)" = "$sha"
if [ ! -e "/srv/rsi/sweeps/source/$sha/.git" ]; then
  sudo -H -u rsi git -C /srv/rsi/src worktree add --detach "/srv/rsi/sweeps/source/$sha" "$sha"
fi
test ! -e "/srv/rsi/sweeps/results/$sha"
sudo systemd-run --unit="rsi-cloud-sweep-${sha:0:12}" --remain-after-exit \
  --uid=rsi --working-directory="/srv/rsi/sweeps/source/$sha" \
  --setenv=HOME=/srv/rsi \
  --setenv=PATH=/srv/rsi/.cargo/bin:/usr/local/bin:/usr/bin:/bin \
  --property=RequiresMountsFor=/srv/rsi \
  /usr/bin/bash "/srv/rsi/sweeps/source/$sha/scripts/cloud-sweep.sh" remote "$sha"
REMOTE
    echo "CLOUD SWEEP started sha=$sha host=$host; collect with: scripts/cloud-sweep.sh collect $sha $host"
    exit 0
  fi
  local_result="$HOME/.rsi/cloud/results/$sha"
  mkdir -p "$local_result"
  scp "${ssh_opts[@]}" "$host:/srv/rsi/sweeps/results/$sha/QA.md" "$local_result/QA.md"
  scp "${ssh_opts[@]}" "$host:/srv/rsi/sweeps/results/$sha/report.json" "$local_result/report.json"
  chmod 600 "$local_result/QA.md" "$local_result/report.json"
  cat "$local_result/QA.md"
  exit 0
fi

[[ "$mode" == remote && $# -eq 2 ]] || usage
cd "$repo_root"
[[ "$(git rev-parse HEAD)" == "$sha" ]] || {
  echo 'remote checkout does not match requested SHA' >&2
  exit 1
}
mkdir -p /srv/rsi/sweeps/results
exec 9>/srv/rsi/sweeps/cloud-sweep.lock
flock -n 9 || { echo 'another cloud sweep is active' >&2; exit 1; }
result_dir="/srv/rsi/sweeps/results/$sha"
[[ ! -e "$result_dir" ]] || { echo "sweep already recorded: $result_dir" >&2; exit 1; }
mkdir -p "$result_dir/logs" "$result_dir/status" "$result_dir/lists" "$result_dir/list-status" "$result_dir/bin"
# The shard runner invokes python3, while AL2023's system python3 is 3.9.
ln -s "$(command -v python3.11)" "$result_dir/bin/python3"
export PATH="$result_dir/bin:$PATH"
export CARGO_BUILD_JOBS=4 CARGO_INCREMENTAL=0 CARGO_PROFILE_TEST_DEBUG=0
if ! command -v cargo >/dev/null 2>&1; then
  curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal
fi
export PATH="/srv/rsi/.cargo/bin:$PATH"
if ! command -v cargo-nextest >/dev/null 2>&1; then
  cargo install cargo-nextest --locked --version 0.9.137
fi
started="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo "CLOUD SWEEP running sha=$sha started=$started"

finish() {
  local code="$?" finished
  trap - EXIT
  finished="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  python3.11 scripts/cloud-sweep-report.py "$sha" "$started" "$finished" "$result_dir" \
    /srv/rsi/seed-failures.txt >"$result_dir/QA.md" || true
  python3.11 scripts/cloud-sweep-json.py "$sha" "$started" "$finished" "$result_dir" \
    nextest >"$result_dir/report.json" || true
  cat "$result_dir/QA.md" 2>/dev/null || true
  exit "$code"
}
trap finish EXIT

run_lane() {
  local label="$1" status
  shift
  if env CARGO_TARGET_DIR="/srv/rsi/target/cloud-$sha/$label" "$@" >"$result_dir/logs/$label.log" 2>&1; then status=0; else status=$?; fi
  printf '%s\t%s\n' "$label" "$status" >"$result_dir/status/$label.tsv"
}

run_list() {
  local shard="$1" status
  if env CARGO_TARGET_DIR="/srv/rsi/target/cloud-$sha/rsid-$shard" \
    cargo nextest list --profile rsid-fast -p rsid --lib --no-default-features \
    --features "test-shard-$shard" --message-format json \
    >"$result_dir/lists/$shard.json" 2>"$result_dir/logs/list-$shard.log"; then
    status=0
  else
    status=$?
  fi
  printf '%s\n' "$status" >"$result_dir/list-status/$shard"
}

bounded_wait() {
  if [[ "$(jobs -rp | wc -l)" -ge 4 ]]; then wait -n || true; fi
}

shard_output="$(python3.11 scripts/check-rsid-test-shards.py --require-gates --list-shards)"
mapfile -t shards <<<"$shard_output"
[[ "${#shards[@]}" -eq 16 ]] || { echo 'expected 16 rsid library shards' >&2; exit 1; }
for shard in "${shards[@]}"; do
  run_list "$shard" &
  bounded_wait
done
wait || true
[[ "$(find "$result_dir/list-status" -type f | wc -l)" -eq 16 ]] || exit 1
if grep -qv '^0$' "$result_dir"/list-status/*; then exit 1; fi
python3.11 scripts/check-rsid-test-shards.py --require-gates --runtime-dir "$result_dir/lists"
for shard in "${shards[@]}"; do
  run_lane "rsid-$shard" scripts/run-rsid-test-shards.sh shard "$shard" --jobs 4 &
  bounded_wait
done
wait || true

# Select integration targets explicitly: `--tests` also selects the unsharded
# rsid library harness, which would defeat the shard memory limit.
integration_args=()
for target in crates/rsid/tests/*.rs; do
  [[ -f "$target" ]] || continue
  label="${target##*/}"
  label="${label%.rs}"
  integration_args+=(--test "$label")
done
[[ "${#integration_args[@]}" -gt 0 ]] || { echo 'no rsid integration targets found' >&2; exit 1; }
run_lane rsid-integrations cargo nextest run --profile ci-full -p rsid \
  "${integration_args[@]}" --status-level all --final-status-level all -j 8 &
run_lane rsid-bins cargo nextest run --profile ci-full -p rsid --bins \
  --status-level all --final-status-level all -j 8 &
run_lane rsi cargo nextest run --profile ci-full -p rsi --lib \
  --status-level all --final-status-level all -j 8 &
run_lane rsi-common cargo nextest run --profile ci-full -p rsi-common --all-targets \
  --status-level all --final-status-level all -j 8 &
wait || true
run_lane other-workspace cargo nextest run --profile ci-full --workspace \
  --exclude rsid --exclude rsi --exclude rsi-common \
  --status-level all --final-status-level all -j 8 &
run_lane rsid-doctests cargo test -p rsid --doc -- --test-threads=8 &
run_lane other-doctests cargo test --workspace --exclude rsid --doc -- --test-threads=8 &
wait || true
