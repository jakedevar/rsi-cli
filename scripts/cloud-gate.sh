#!/bin/bash
# Run one rsi-rolling-land gate on an ephemeral AWS host, then destroy it.
#
#   scripts/cloud-gate.sh [--instance-type TYPE] [--keep-for-debug] -- LANDER_ARGS...
#   scripts/cloud-gate.sh [options] --run -- COMMAND...
#
# LANDER_ARGS are passed to rsi-rolling-land unchanged (for example
# --repo "$PWD" --remote origin --accepted BASE:SOURCE); this script adds the
# --remote-gate-* flags. Operator rule 2026-09-28: apply, one remote gate,
# destroy, no build volumes or snapshots kept. The EXIT trap always runs
# terraform destroy and then checks AWS for leftover gate instances. The guest
# also terminates itself after max_lifetime_minutes as a backstop.
#
# --run replaces the lander with COMMAND, run locally once the host is ready
# with RSI_GATE_HOST (ec2-user@IP), RSI_GATE_IDENTITY and RSI_GATE_RUN_AS in its
# environment (scripts/cloud-sweep.sh cloud uses it, so it shares the apply,
# ledger and destroy code). --root-volume-gib and --max-lifetime-minutes size
# the host for heavier runs.
#
# Every run first checks the spend ledger (scripts/cloud-spend.py check) and
# refuses to start at or above the grant's stop line. When the persistent cache
# stack (infra/aws/gate-cache, state gate-cache.tfstate) exists the host builds
# through sccache with an S3 backend; the run's sccache hits, misses and hit
# rate are printed and appended to its spend-ledger line.
#
# Inputs (defaults in parentheses): RSI_CLOUD_DIR (~/.rsi/cloud) holds
# gate.tfvars, gate.tfstate, gate.lock and the spend ledger spend.md;
# RSI_GATE_IDENTITY (~/.ssh/id_ed25519_rsi) is the private key of the
# tfvars ssh_key_name; RSI_GATE_HOURLY_USD (1.7808, the configured
# c7i.16xlarge $3.5616/h scaled to the default c7i.8xlarge) prices the
# ledger entry; set it when passing another --instance-type.
# Needs terraform, aws (profile rsi-cloud-terraform), ssh and ssh-keyscan.
# Slots (#969): the run holds no desktop lander slot. gate.lock is the
# remote-gate pool (one host per state). The lander's local builds and tests
# each take a desktop build slot through RSI_GATE_CARGO_SLOT
# (~/.rsi/bin/cargo-slot, passed as RSI_LANDER_LOCAL_SLOT_WRAPPER);
# /usr/bin/env disables that (smoke runs). RSI_REMOTE_GATE_PARALLEL sets how
# many shards run at once on the host (lander default 4).
set -euo pipefail

usage() {
  sed -n '2,37p' "$0" >&2
  exit 2
}

repo_root=$(cd "$(dirname "$0")/.." && pwd -P)
cloud_dir=${RSI_CLOUD_DIR:-$HOME/.rsi/cloud}
identity=${RSI_GATE_IDENTITY:-$HOME/.ssh/id_ed25519_rsi}
hourly_usd=${RSI_GATE_HOURLY_USD:-1.7808}
lander=${RSI_GATE_LANDER:-$HOME/.cargo/shared-target/debug/rsi-rolling-land}
cargo_slot=${RSI_GATE_CARGO_SLOT:-$HOME/.rsi/bin/cargo-slot}
tf_dir="$repo_root/infra/aws/gate"
state="$cloud_dir/gate.tfstate"
tfvars="$cloud_dir/gate.tfvars"
ledger="$cloud_dir/spend.md"
region=us-west-1
remote_dir=/srv/rsi-gate
export AWS_PROFILE=${AWS_PROFILE:-rsi-cloud-terraform}

instance_type=
root_volume_gib=
max_lifetime_minutes=
run_mode=0
keep=0
while [ $# -gt 0 ]; do
  case "$1" in
    --instance-type) instance_type=${2:?}; shift 2 ;;
    --root-volume-gib) root_volume_gib=${2:?}; shift 2 ;;
    --max-lifetime-minutes) max_lifetime_minutes=${2:?}; shift 2 ;;
    --run) run_mode=1; shift ;;
    --keep-for-debug) keep=1; shift ;;
    --) shift; break ;;
    *) usage ;;
  esac
done
[ $# -gt 0 ] || usage
for tool in terraform aws ssh ssh-keyscan ssh-keygen flock python3; do
  command -v "$tool" >/dev/null || { echo "cloud-gate: missing $tool" >&2; exit 2; }
done
[ -f "$tfvars" ] || { echo "cloud-gate: missing $tfvars (ssh_key_name = \"...\")" >&2; exit 2; }
[ -f "$identity" ] || { echo "cloud-gate: missing identity $identity" >&2; exit 2; }
[ "$run_mode" -eq 1 ] || [ -x "$lander" ] || { echo "cloud-gate: missing lander $lander" >&2; exit 2; }
# Refuse to spend past the grant's stop line (limits read from the ledger header).
python3 "$repo_root/scripts/cloud-spend.py" check --ledger "$ledger" || exit 4

# The shards run on the gate host, so this run takes no desktop lander slot
# and starts the host at once (#969). Local work still pays its way: every
# local Cargo or shard-runner command the lander runs takes a desktop build
# slot through the wrapper, and the lander itself runs at low priority.
# One-off gate builds never reuse incremental state (#1063).
export CARGO_INCREMENTAL=0
if [ "$cargo_slot" != /usr/bin/env ]; then
  export RSI_LANDER_LOCAL_SLOT_WRAPPER="$cargo_slot"
fi

# One gate host at a time; a second caller fails fast instead of sharing it.
exec 8>>"$cloud_dir/gate.lock"
flock -n 8 || { echo "cloud-gate: another gate host is running" >&2; exit 3; }

tf() { terraform -chdir="$tf_dir" "$@" -input=false -no-color; }
tf_output() { terraform -chdir="$tf_dir" output -no-color -state="$state" -raw "$1"; }
tf_vars=(-state="$state" -var-file="$tfvars")
[ -n "$instance_type" ] && tf_vars+=(-var "instance_type=$instance_type")
[ -n "$root_volume_gib" ] && tf_vars+=(-var "root_volume_gib=$root_volume_gib")
[ -n "$max_lifetime_minutes" ] && tf_vars+=(-var "max_lifetime_minutes=$max_lifetime_minutes")
utc() { date -u +%Y-%m-%dT%H:%M:%SZ; }

started_epoch=
instance_id=
public_ip=
run_as=rsi
sccache_bucket=
sccache_note='sccache: off (cold run)'
sccache_collected=0

remote() {
  ssh -F /dev/null -T -o BatchMode=yes -o IdentitiesOnly=yes -o StrictHostKeyChecking=yes \
    -o ConnectTimeout=10 -i "$identity" "ec2-user@$public_ip" "$@"
}

# Read the host's sccache counters once, while it is still alive: print them,
# keep the raw summary next to the other logs and remember the ledger note.
collect_sccache() {
  [ -n "$sccache_bucket" ] && [ -n "$public_ip" ] && [ "$sccache_collected" -eq 0 ] || return 0
  sccache_collected=1
  local raw summary
  raw=$(timeout 90 ssh -F /dev/null -T -o BatchMode=yes -o IdentitiesOnly=yes -o StrictHostKeyChecking=yes \
    -o ConnectTimeout=10 -i "$identity" "ec2-user@$public_ip" \
    sudo -n -H -u "$run_as" /usr/local/bin/sccache --show-stats --stats-format json 2>/dev/null) || raw=
  if summary=$(printf '%s' "$raw" | python3 "$repo_root/scripts/cloud-spend.py" cache \
    --json-out "$cloud_dir/logs/sccache-${instance_id:-unknown}.json" 2>/dev/null); then
    echo "cloud-gate: $summary" >&2
    echo "$summary"
    if [ -n "${RSI_GATE_STATS_COPY:-}" ]; then
      cp "$cloud_dir/logs/sccache-${instance_id:-unknown}.json" "$RSI_GATE_STATS_COPY" 2>/dev/null || true
    fi
    sccache_note="$(printf '%s' "$summary" | sed 's/sccache_//g; s/=/ /g; s/ hit_rate/ hit rate/; s/^/sccache: /')"
  else
    sccache_note='sccache: stats unavailable'
    echo "cloud-gate: $sccache_note" >&2
  fi
}

cleanup() {
  local status=$?
  trap - EXIT INT TERM HUP
  if [ "$keep" -eq 1 ] && [ -n "$instance_id" ]; then
    echo "cloud-gate: --keep-for-debug left $instance_id running; the guest terminates itself at its lifetime limit" >&2
    exit "$status"
  fi
  collect_sccache || true
  echo "cloud-gate: destroying gate host" >&2
  local attempt destroyed=0
  for attempt in 1 2 3; do
    if tf destroy -auto-approve "${tf_vars[@]}" >"$cloud_dir/logs/gate-destroy.log" 2>&1; then
      destroyed=1
      break
    fi
    sleep 20
  done
  # Independent of Terraform state: nothing tagged as a gate host may remain.
  local leftover
  leftover=$(aws ec2 describe-instances --region "$region" \
    --filters Name=tag:Service,Values=rsi-remote-gate \
    Name=instance-state-name,Values=pending,running,stopping,stopped \
    --query 'Reservations[].Instances[].InstanceId' --output text 2>/dev/null || echo unknown)
  if [ -n "$leftover" ] && [ "$leftover" != None ]; then
    echo "cloud-gate: LEFTOVER gate instances after destroy: $leftover" >&2
    [ "$leftover" != unknown ] && aws ec2 terminate-instances --region "$region" \
      --instance-ids $leftover >/dev/null 2>&1 || true
    status=1
  fi
  [ -n "$public_ip" ] && ssh-keygen -R "$public_ip" >/dev/null 2>&1 || true
  if [ -n "$started_epoch" ]; then
    local stopped elapsed
    stopped=$(date -u +%s)
    elapsed=$((stopped - started_epoch))
    printf 'Gate window %s: stop %s, elapsed %s s = %s h, est compute $%s at $%s/h; terraform destroy %s; leftover gate instances: %s; %s.\n' \
      "$instance_id" "$(utc)" "$elapsed" \
      "$(awk -v s="$elapsed" 'BEGIN { printf "%.4f", s / 3600 }')" \
      "$(awk -v s="$elapsed" -v r="$hourly_usd" 'BEGIN { printf "%.4f", s / 3600 * r }')" \
      "$hourly_usd" "$([ "$destroyed" -eq 1 ] && echo ok || echo FAILED)" "${leftover:-none}" \
      "$sccache_note" >>"$ledger"
  fi
  [ "$destroyed" -eq 1 ] || { echo "cloud-gate: terraform destroy failed; see $cloud_dir/logs/gate-destroy.log" >&2; status=1; }
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM HUP

mkdir -p "$cloud_dir/logs"
tf init >/dev/null
# The warm build cache is a persistent sibling stack (its bucket must outlive
# every destroy of this host). Without its state the host builds cold.
cache_state="$cloud_dir/gate-cache.tfstate"
# A failed apply of the cache stack still leaves its planned outputs in the
# state, so the bucket must really exist before it is used.
if [ -s "$cache_state" ] && cache_bucket=$(terraform -chdir="$tf_dir" output -no-color -state="$cache_state" -raw bucket 2>/dev/null) \
  && [ -n "$cache_bucket" ] && aws s3api head-bucket --bucket "$cache_bucket" --region "$region" >/dev/null 2>&1; then
  tf_vars+=(-var "sccache_bucket=$cache_bucket"
    -var "sccache_prefix=$(terraform -chdir="$tf_dir" output -no-color -state="$cache_state" -raw key_prefix)"
    -var "instance_profile_name=$(terraform -chdir="$tf_dir" output -no-color -state="$cache_state" -raw instance_profile)")
  sccache_bucket=$cache_bucket
  sccache_note='sccache: no stats'
else
  echo "cloud-gate: no cache stack state at $cache_state; building cold (see infra/aws/README.md)" >&2
fi
started_epoch=$(date -u +%s)
tf apply -auto-approve "${tf_vars[@]}" >"$cloud_dir/logs/gate-apply.log" 2>&1
instance_id=$(tf_output instance_id)
public_ip=$(tf_output public_ip)
run_as=$(tf_output run_as)
applied_type=$(tf_output instance_type)
printf 'Gate window %s: start %s, %s, public IPv4 %s, est $%s/h (apply began %s).\n' \
  "$instance_id" "$(utc)" "$applied_type" "$public_ip" "$hourly_usd" \
  "$(date -u -d "@$started_epoch" +%H:%M:%SZ)" >>"$ledger"
echo "cloud-gate: $instance_id ($applied_type) at $public_ip" >&2

# Pin the host key only after it matches the fingerprint cloud-init printed
# to the instance console, which AWS reads out of band.
ssh-keygen -R "$public_ip" >/dev/null 2>&1 || true
scanned=
for _ in $(seq 60); do
  scanned=$(ssh-keyscan -T 5 -t ed25519 "$public_ip" 2>/dev/null | grep -v '^#' || true)
  [ -n "$scanned" ] && break
  sleep 10
done
[ -n "$scanned" ] || { echo "cloud-gate: SSH never answered" >&2; exit 1; }
fingerprint=$(printf '%s\n' "$scanned" | ssh-keygen -lf - | awk '{print $2}')
verified=0
for _ in $(seq 60); do
  if aws ec2 get-console-output --region "$region" --instance-id "$instance_id" --latest \
    --query Output --output text 2>/dev/null | grep -F "$fingerprint" | grep -q ED25519; then
    verified=1
    break
  fi
  sleep 10
done
[ "$verified" -eq 1 ] || { echo "cloud-gate: host key $fingerprint not confirmed by the console" >&2; exit 1; }
printf '%s\n' "$scanned" >>"$HOME/.ssh/known_hosts"

ready=0
for _ in $(seq 150); do
  if remote test -f /var/lib/rsi-gate-ready 2>/dev/null; then
    ready=1
    break
  fi
  sleep 10
done
if [ "$ready" -ne 1 ]; then
  echo "cloud-gate: bootstrap did not finish; tail of its log:" >&2
  remote sudo tail -n 40 /var/log/rsi-gate-bootstrap.log >&2 || true
  exit 1
fi
echo "cloud-gate: host ready after $(( $(date -u +%s) - started_epoch )) s" >&2

lander_started=$(date -u +%s)
set +e
if [ "$run_mode" -eq 1 ]; then
  RSI_GATE_HOST="ec2-user@$public_ip" RSI_GATE_IDENTITY="$identity" RSI_GATE_RUN_AS="$run_as" \
    RSI_GATE_INSTANCE_ID="$instance_id" "$@" 8>&-
else
  nice -n 19 ionice -c3 env CARGO_BUILD_JOBS=4 CARGO_PROFILE_DEV_DEBUG=line-tables-only \
    "$lander" "$@" \
    --remote-gate-host "ec2-user@$public_ip" \
    --remote-gate-dir "$remote_dir" \
    --remote-gate-identity "$identity" \
    --remote-gate-run-as "$run_as" 8>&-
fi
lander_status=$?
set -e
collect_sccache
echo "remote_gate_wall_seconds=$(( $(date -u +%s) - lander_started ))"
echo "remote_gate_host_seconds=$(( $(date -u +%s) - started_epoch ))"
exit "$lander_status"
