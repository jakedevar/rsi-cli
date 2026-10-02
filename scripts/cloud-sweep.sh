#!/usr/bin/env bash
# Transfer one exact rolling commit over SSH and run the complete test matrix.
#
#   cloud-sweep.sh cloud SHA [--instance-type TYPE] [--repo DIR]
#       One command: apply an ephemeral gate host (scripts/cloud-gate.sh --run,
#       which owns apply, spend guard, ledger lines, cache stats and the
#       EXIT-trap destroy), run the full sweep of SHA (the current rolling tip)
#       there, collect the report into ~/.rsi/cloud/results/SHA, destroy.
#       --repo DIR names the git repository whose `origin` URL says where
#       rolling lives (default: this script's repository); the daemon's
#       cloud_sweep job runs an embedded copy of this script and passes the
#       caller's tree here, so the script itself never comes from that tree.
#       Git never runs inside DIR or with its configuration: the URL is read
#       with `git config --file` and rolling is fetched into a daemon-owned
#       bare mirror (--mirror DIR, default ~/.rsi/cloud/sweep-mirror.git) under
#       sanitized git configuration, then bundled from the mirror.
#   cloud-sweep.sh bundle SHA OUT [--repo DIR] [--mirror DIR]
#       Only that fetch-and-bundle step: write a bundle of origin/rolling to OUT
#       after checking it is SHA.
#   cloud-sweep.sh hub|collect SHA ec2-user@IP [SSH_KEY]
#       The same steps against a host you already have; hub only starts.
#   cloud-sweep.sh wait SHA ec2-user@IP [SSH_KEY]
#       Block until the started sweep unit has exited.
#
# RSI_SWEEP_ROOT / RSI_SWEEP_HOME (default /srv/rsi for both) place the work
# tree and the toolchain owner's home on the host; cloud mode sets them for the
# gate host (/srv/rsi-sweep, /home/rsi).
set -euo pipefail
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

usage() {
  echo 'usage: cloud-sweep.sh cloud SHA [--instance-type TYPE] [--repo DIR] [--mirror DIR] | bundle SHA OUT [--repo DIR] [--mirror DIR] | hub|collect|wait SHA ec2-user@IP [SSH_KEY] | remote SHA' >&2
  exit 2
}

# Git for the sweep's bundle step: never the caller's repository configuration
# or environment. Its `core.sshCommand`, fsmonitor, hooks, filters and
# includes would otherwise run under the operator's cloud credentials.
safe_git() {
  env -u GIT_DIR -u GIT_WORK_TREE -u GIT_INDEX_FILE -u GIT_SSH -u GIT_SSH_COMMAND \
    -u GIT_PROXY_COMMAND -u GIT_CONFIG_COUNT -u GIT_CONFIG_PARAMETERS -u GIT_EXEC_PATH \
    -u GIT_EXTERNAL_DIFF -u GIT_ALTERNATE_OBJECT_DIRECTORIES -u GIT_OBJECT_DIRECTORY \
    GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null \
    GIT_TERMINAL_PROMPT=0 GIT_PAGER=cat \
    git -c core.sshCommand=ssh -c core.fsmonitor=false -c core.hooksPath=/dev/null \
    -c core.attributesFile=/dev/null -c core.pager=cat -c credential.helper= \
    -c gc.auto=0 -c maintenance.auto=false -c fetch.fsckObjects=true \
    -c protocol.allow=never -c "protocol.file.allow=$sweep_file_proto" -c protocol.ssh.allow=always \
    -c protocol.https.allow=always -c protocol.ext.allow=never "$@"
}

# The repository's `origin` URL, read without running git in it: the config
# file is parsed by `git config --file`, which starts no program.
origin_url_of() {
  local repo="$1" gitdir common url
  gitdir="$repo/.git"
  if [[ -f "$gitdir" ]]; then
    gitdir="$(sed -n 's/^gitdir: //p' "$gitdir" | head -n 1)"
    [[ "$gitdir" == /* ]] || gitdir="$repo/$gitdir"
  fi
  common="$gitdir"
  if [[ -f "$gitdir/commondir" ]]; then
    common="$(cd "$gitdir" && cd "$(head -n 1 commondir)" && pwd)"
  fi
  [[ -f "$common/config" ]] || { echo "cloud-sweep: no git config under $repo" >&2; return 1; }
  url="$(safe_git config --file "$common/config" --get remote.origin.url)" || {
    echo "cloud-sweep: $repo has no origin URL" >&2
    return 1
  }
  [[ -n "$url" && "$url" != -* ]] || { echo "cloud-sweep: refusing origin URL" >&2; return 1; }
  printf '%s\n' "$url"
}

# Whether file transport (a local origin) is allowed: only for an origin under
# a root named with --allow-local-root, which the daemon never passes. Set
# unconditionally so nothing in the environment can turn it on.
sweep_file_proto=never
sweep_local_roots=()
origin_checked=

# The origin URL is caller-written data. Only https://, ssh:// and scp-style
# git@host:path are fetched; ext::, a leading '-', whitespace and any other
# transport are refused. A local path or file:// URL is fetched only when it
# resolves under an --allow-local-root directory. The result is left in
# origin_checked (not a subshell: file transport is enabled as a side effect).
validated_origin() {
  local url="$1" path root real re
  [[ -n "$url" && "$url" != -* && "$url" != *[[:space:]]* && "$url" != *$'\n'* ]] || return 1
  case "$url" in
    ext::*|*::*) return 1 ;;
    https://*|ssh://*)
      # POSIX classes: a leading `]` is literal, `-` last (no backslash escapes).
      re='^(https|ssh)://[]A-Za-z0-9_@:%.[][]A-Za-z0-9._@:%[-]*(/[^[:space:]]*)?$'
      [[ "$url" =~ $re ]] || return 1
      ;;
    file://*) path="${url#file://}" ;;
    /*) path="$url" ;;
    *)
      [[ "$url" =~ ^[A-Za-z0-9._-]+@[A-Za-z0-9.-]+:[^-][^[:space:]]*$ ]] || return 1
      ;;
  esac
  if [[ -n "${path:-}" ]]; then
    [[ "$path" == /* && ${#sweep_local_roots[@]} -gt 0 ]] || return 1
    real="$(realpath -e -- "$path" 2>/dev/null)" || return 1
    for root in "${sweep_local_roots[@]}"; do
      root="$(realpath -e -- "$root" 2>/dev/null)" || continue
      [[ "$real" == "$root"/* ]] && { sweep_file_proto=always; origin_checked="$real"; return 0; }
    done
    return 1
  fi
  origin_checked="$url"
}

owner_and_mode() {
  stat -c '%u %a' -- "$1" 2>/dev/null || stat -f '%u %Lp' -- "$1"
}

# The mirror holds only what this script wrote: a real directory owned by this
# user with mode 0700, a config identical to a fresh `git init --bare`, no
# alternates, and only the entries git creates. A symlink, another owner's
# directory or a planted config (remote.<name>.uploadpack, core.sshCommand,
# includes) makes it untrusted: it is deleted and rebuilt from scratch.
mirror_is_trusted() {
  local mirror="$1" fresh="$2" entry
  [[ ! -L "$mirror" && -d "$mirror" ]] || return 1
  [[ "$(owner_and_mode "$mirror")" == "$(id -u) 700" ]] || return 1
  [[ -f "$mirror/config" && ! -L "$mirror/config" ]] || return 1
  cmp -s "$mirror/config" "$fresh/config" || return 1
  [[ ! -e "$mirror/objects/info/alternates" && ! -e "$mirror/commondir" && ! -e "$mirror/gitdir" ]] || return 1
  [[ -z "$(find "$mirror" -type l -print -quit)" ]] || return 1
  for entry in "$mirror"/* "$mirror"/.[!.]*; do
    [[ -e "$entry" ]] || continue
    case "${entry##*/}" in
      HEAD|config|objects|refs|packed-refs|FETCH_HEAD|logs|description|info|branches) ;;
      *) return 1 ;;
    esac
  done
  [[ ! -e "$mirror/hooks" ]]
}

# Fetch rolling into the daemon-owned bare mirror and bundle exactly SHA. The
# fetch names the validated literal URL, never a remote name from the caller.
make_bundle() {
  local sha="$1" out="$2" repo="$3" mirror="$4" url fresh
  url="$(origin_url_of "$repo")"
  validated_origin "$url" || { echo 'cloud-sweep: refusing origin URL' >&2; return 1; }
  url="$origin_checked"
  mkdir -p "$(dirname "$mirror")"
  cd /
  fresh="$(mktemp -d "$(dirname "$mirror")/.sweep-mirror-fresh.XXXXXX")"
  chmod 700 "$fresh"
  (umask 077; safe_git init -q --bare --template= "$fresh")
  if ! mirror_is_trusted "$mirror" "$fresh"; then
    if [[ -L "$mirror" ]]; then rm -f -- "$mirror"; else rm -rf -- "$mirror"; fi
    [[ ! -e "$mirror" ]] || { rm -rf -- "$fresh"; echo 'cloud-sweep: cannot recreate the sweep mirror' >&2; return 1; }
    mv -- "$fresh" "$mirror"
  else
    rm -rf -- "$fresh"
  fi
  safe_git --git-dir="$mirror" update-ref -d refs/remotes/origin/rolling
  safe_git --git-dir="$mirror" -c remote.origin.uploadpack=git-upload-pack fetch --no-tags -q \
    --upload-pack=git-upload-pack -- "$url" refs/heads/rolling:refs/remotes/origin/rolling
  [[ "$(safe_git --git-dir="$mirror" rev-parse refs/remotes/origin/rolling)" == "$sha" ]] || {
    echo 'requested SHA is not the current rolling tip' >&2
    return 1
  }
  safe_git --git-dir="$mirror" bundle create "$out" refs/remotes/origin/rolling
  safe_git bundle list-heads "$out" | grep -q "^$sha refs/remotes/origin/rolling$"
}

# `check-origin URL`: exit 0 when the sweep would fetch that origin URL.
if [[ "${1:-}" == check-origin && $# -eq 2 ]]; then
  validated_origin "$2"
  exit $?
fi

[[ $# -ge 2 ]] || usage
mode="$1"
sha="$2"
[[ "$sha" =~ ^[0-9a-f]{40}$ ]] || usage

if [[ "$mode" == bundle ]]; then
  [[ $# -ge 3 ]] || usage
  out="$3"
  shift 3
  repo="$repo_root"
  mirror="${RSI_SWEEP_MIRROR:-$HOME/.rsi/cloud/sweep-mirror.git}"
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --repo) repo="${2:?}"; shift 2 ;;
      --mirror) mirror="${2:?}"; shift 2 ;;
      --allow-local-root) sweep_local_roots+=("${2:?}"); shift 2 ;;
      *) usage ;;
    esac
  done
  make_bundle "$sha" "$out" "$repo" "$mirror"
  exit $?
fi

if [[ "$mode" == cloud ]]; then
  shift 2
  gate_args=()
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --instance-type) gate_args+=(--instance-type "${2:?}"); shift 2 ;;
      --repo) export RSI_SWEEP_REPO="${2:?}"; shift 2 ;;
      --mirror) export RSI_SWEEP_MIRROR="${2:?}"; shift 2 ;;
      *) usage ;;
    esac
  done
  # The sweep builds 16 separate rsid test target trees, hence the larger disk.
  mkdir -p "$HOME/.rsi/cloud/results/$sha"
  RSI_SWEEP_ROOT=/srv/rsi-sweep RSI_SWEEP_HOME=/home/rsi \
    RSI_GATE_STATS_COPY="$HOME/.rsi/cloud/results/$sha/sccache.json" \
    exec "$repo_root/scripts/cloud-gate.sh" ${gate_args[@]+"${gate_args[@]}"} \
    --root-volume-gib 400 --max-lifetime-minutes 420 --run -- \
    "$repo_root/scripts/cloud-sweep.sh" hosted "$sha"
fi

sweep_root="${RSI_SWEEP_ROOT:-/srv/rsi}"
sweep_home="${RSI_SWEEP_HOME:-$sweep_root}"

if [[ "$mode" == hosted ]]; then
  # Runs under cloud-gate.sh --run, which exports the host and identity.
  [[ $# -eq 2 && -n "${RSI_GATE_HOST:-}" && -n "${RSI_GATE_IDENTITY:-}" ]] || usage
  "$0" hub "$sha" "$RSI_GATE_HOST" "$RSI_GATE_IDENTITY"
  wait_status=0
  "$0" wait "$sha" "$RSI_GATE_HOST" "$RSI_GATE_IDENTITY" || wait_status=$?
  "$0" collect "$sha" "$RSI_GATE_HOST" "$RSI_GATE_IDENTITY" || wait_status=$?
  exit "$wait_status"
fi

if [[ "$mode" == hub || "$mode" == collect || "$mode" == wait ]]; then
  [[ $# -ge 3 && $# -le 4 ]] || usage
  host="$3"
  key="${4:-$HOME/.ssh/id_ed25519_rsi}"
  [[ "$host" =~ ^ec2-user@[0-9.]+$ && -f "$key" ]] || usage
  ssh_opts=(-i "$key" -o IdentitiesOnly=yes -o StrictHostKeyChecking=accept-new)
  if [[ "$mode" == hub ]]; then
    bundle="$(mktemp /tmp/rsi-cloud-sweep.XXXXXX.bundle)"
    trap 'rm -f "$bundle"' EXIT
    # `bundle` mode is this script's own fetch-and-bundle step; git never runs
    # in the caller's tree or with its config (see safe_git above).
    "$0" bundle "$sha" "$bundle" --repo "${RSI_SWEEP_REPO:-$repo_root}" \
      --mirror "${RSI_SWEEP_MIRROR:-$HOME/.rsi/cloud/sweep-mirror.git}"
    scp "${ssh_opts[@]}" "$bundle" "$host:/tmp/rsi-cloud-$sha.bundle"
    ssh "${ssh_opts[@]}" "$host" "bash -s -- $sha $sweep_root $sweep_home" <<'REMOTE'
set -euo pipefail
sha="$1"
root="$2"
home="$3"
sudo install -d -o rsi -g rsi "$root"
sudo install -d -o rsi -g rsi "$root/incoming" "$root/src" "$root/sweeps" "$root/sweeps/source"
# AL2023 mounts /tmp as tmpfs, and the custody code rejects sandbox roots on
# tmpfs/ramfs, so give the remote sweep a disk-backed TMPDIR under /srv/rsi.
sudo install -d -o rsi -g rsi -m 700 "$root/sweeps/tmp/$sha"
command -v python3.11 >/dev/null 2>&1 || sudo dnf install -y python3.11
sudo mv "/tmp/rsi-cloud-$sha.bundle" "$root/incoming/$sha.bundle"
sudo chown rsi:rsi "$root/incoming/$sha.bundle"
sudo -H -u rsi git -C "$root/src" init -q
sudo -H -u rsi git -C "$root/src" fetch -q "$root/incoming/$sha.bundle" refs/remotes/origin/rolling
test "$(sudo -H -u rsi git -C "$root/src" rev-parse FETCH_HEAD)" = "$sha"
if [ ! -e "$root/sweeps/source/$sha/.git" ]; then
  sudo -H -u rsi git -C "$root/src" worktree add --detach "$root/sweeps/source/$sha" "$sha"
fi
test ! -e "$root/sweeps/results/$sha"
sudo systemd-run --unit="rsi-cloud-sweep-${sha:0:12}" --remain-after-exit \
  --uid=rsi --working-directory="$root/sweeps/source/$sha" \
  --setenv=HOME="$home" \
  --setenv=RSI_SWEEP_ROOT="$root" \
  --setenv=PATH="$home/.cargo/bin:/usr/local/bin:/usr/bin:/bin" \
  --setenv=TMPDIR="$root/sweeps/tmp/$sha" \
  --property=RequiresMountsFor="$root" \
  /usr/bin/bash "$root/sweeps/source/$sha/scripts/cloud-sweep.sh" remote "$sha"
REMOTE
    echo "CLOUD SWEEP started sha=$sha host=$host; wait, then collect with: scripts/cloud-sweep.sh collect $sha $host"
    exit 0
  fi
  if [[ "$mode" == wait ]]; then
    # The unit outlives its process (--remain-after-exit); SubState leaves
    # "running" when the sweep script exits. Give up if the host stays
    # unreachable (the guest also terminates itself at its lifetime limit).
    deadline=$(( $(date +%s) + ${RSI_SWEEP_WAIT_SECS:-25200} ))
    unit="rsi-cloud-sweep-${sha:0:12}"
    fails=0
    while :; do
      if state="$(ssh "${ssh_opts[@]}" -o ConnectTimeout=15 "$host" \
        "systemctl show -p SubState -p ExecMainStatus $unit" 2>/dev/null)"; then
        fails=0
        if ! grep -qx 'SubState=running\|SubState=start' <<<"$state"; then
          echo "CLOUD SWEEP finished sha=$sha $(tr '\n' ' ' <<<"$state")"
          exit 0
        fi
      else
        fails=$((fails + 1))
        [[ $fails -lt 20 ]] || { echo "cloud-sweep: host unreachable while waiting" >&2; exit 1; }
      fi
      [[ $(date +%s) -lt $deadline ]] || { echo "cloud-sweep: wait deadline reached" >&2; exit 1; }
      sleep 30
    done
  fi
  local_result="$HOME/.rsi/cloud/results/$sha"
  mkdir -p "$local_result"
  scp "${ssh_opts[@]}" "$host:$sweep_root/sweeps/results/$sha/QA.md" "$local_result/QA.md"
  scp "${ssh_opts[@]}" "$host:$sweep_root/sweeps/results/$sha/report.json" "$local_result/report.json"
  chmod 600 "$local_result/QA.md" "$local_result/report.json"
  # Keep each failed lane's log excerpt: the host (and its full logs) is
  # destroyed after collect, so a red must stay diagnosable from results/SHA
  # alone (Issue #1060). Best effort: never fail the collect over excerpts.
  failure_logs="$local_result/failure-logs"
  mkdir -p "$failure_logs"
  if ssh "${ssh_opts[@]}" "$host" "bash -s -- $sweep_root/sweeps/results/$sha" \
    <"$repo_root/scripts/cloud-sweep-excerpts.sh" | tar -xf - -C "$failure_logs"; then
    chmod -R go-rwx "$failure_logs"
  else
    echo "cloud-sweep: could not collect failure log excerpts from $host" >&2
  fi
  # Mechanical KNOWN/NEW verdict from the collected logs (Issue #1016). Best
  # effort and after everything above: the host destroy never depends on it.
  python3 "$repo_root/scripts/cloud-sweep-verdict.py" "$local_result" >/dev/null 2>&1 ||
    echo "cloud-sweep: verdict step failed; classify $local_result by hand" >&2
  cat "$local_result/QA.md"
  exit 0
fi

[[ "$mode" == remote && $# -eq 2 ]] || usage
cd "$repo_root"
[[ "$(git rev-parse HEAD)" == "$sha" ]] || {
  echo 'remote checkout does not match requested SHA' >&2
  exit 1
}
# Disk-backed scratch for this sweep SHA: AL2023 mounts /tmp as tmpfs, which
# the custody code rejects as a sandbox root. One directory per SHA keeps test
# isolation; it is removed on exit by the trap below.
export TMPDIR="$sweep_root/sweeps/tmp/$sha"
mkdir -p "$TMPDIR"
chmod 700 "$TMPDIR"
mkdir -p "$sweep_root/sweeps/results"
exec 9>"$sweep_root/sweeps/cloud-sweep.lock"
flock -n 9 || { echo 'another cloud sweep is active' >&2; exit 1; }
result_dir="$sweep_root/sweeps/results/$sha"
[[ ! -e "$result_dir" ]] || { echo "sweep already recorded: $result_dir" >&2; exit 1; }
mkdir -p "$result_dir/logs" "$result_dir/status" "$result_dir/lists" "$result_dir/list-status" "$result_dir/bin"
# The shard runner invokes python3, while AL2023's system python3 is 3.9.
ln -s "$(command -v python3.11)" "$result_dir/bin/python3"
export PATH="$result_dir/bin:$PATH"
export CARGO_BUILD_JOBS=4 CARGO_INCREMENTAL=0 CARGO_PROFILE_TEST_DEBUG=0
if ! command -v cargo >/dev/null 2>&1; then
  curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal
fi
export PATH="$sweep_home/.cargo/bin:$PATH"
# The gate host's bootstrap installs sccache with an S3 backend next to the
# toolchain owner's config; use it so this sweep starts warm (Issue #1010).
if [[ -x /usr/local/bin/sccache && -f "$sweep_home/.config/sccache/config" ]]; then
  export RUSTC_WRAPPER=/usr/local/bin/sccache
fi
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
    "$sweep_root/seed-failures.txt" >"$result_dir/QA.md" || true
  python3.11 scripts/cloud-sweep-json.py "$sha" "$started" "$finished" "$result_dir" \
    nextest >"$result_dir/report.json" || true
  cat "$result_dir/QA.md" 2>/dev/null || true
  rm -rf "$TMPDIR" || true
  exit "$code"
}
trap finish EXIT

run_lane() {
  local label="$1" status
  shift
  if env CARGO_TARGET_DIR="$sweep_root/target/cloud-$sha/$label" "$@" >"$result_dir/logs/$label.log" 2>&1; then status=0; else status=$?; fi
  printf '%s\t%s\n' "$label" "$status" >"$result_dir/status/$label.tsv"
}

run_list() {
  local shard="$1" status
  if env CARGO_TARGET_DIR="$sweep_root/target/cloud-$sha/rsid-$shard" \
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
