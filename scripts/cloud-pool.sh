#!/bin/bash
# Warm spot pool: up to 6 leased AWS spot hosts inside the daemon spend caps
# (#3460 S4b). The daemon places jobs on the leases in S4c/S4d; this script only
# creates, renews and destroys the hosts and keeps the books.
#
#   scripts/cloud-pool.sh up N        make the pool N hosts (1-6; N is the total)
#   scripts/cloud-pool.sh down        destroy every host and settle its reservation
#   scripts/cloud-pool.sh status      leases, time left, idle time (no AWS call)
#   scripts/cloud-pool.sh reap        one maintenance pass (run it every few minutes)
#   scripts/cloud-pool.sh touch [SLOT]  record a placed job (every host without SLOT)
#
# Leases live in ~/.rsi/cloud/pool.json: per slot (pool-1 .. pool-6) the host
# (ec2-user@IP), instance id, lease start and expiry, reservation id and the
# time of the last placed job. `status --json` prints the file for S4c/S4d.
#
# Spend (the same guarantees as scripts/cloud-gate.sh, per host and per lease):
#   * up reserves every new host's 12 h lease (rsi-cloud spend reserve, lifetime
#     x the worst-case spot rate, never the on-demand price) BEFORE terraform
#     apply, all or nothing; a refusal at the daily cap or the stop line exits 4
#     with nothing reserved or applied.
#   * A lease is renewed only by a fresh reservation for the NEXT 12 h at the
#     same worst-case rate, taken in the last hour of the lease. A refusal
#     marks the lease renewal=refused: the host is not renewed and reap
#     destroys it at its expiry.
#   * reap destroys a host at its lease expiry, after 30 min with no placed job
#     (touch), or once AWS took it back (spot interruption); each destroy
#     writes the stop line that names the reservation and replaces it with the
#     actual cost. A renewal settles the ended lease the same way.
#   * Backstops, in the order they fire: the guest shuts itself down at the
#     lease end (shutdown terminates the instance), then the orphan sweep
#     (rsi-cloud watchdog) terminates a host whose rsi:expires-at tag is
#     10 minutes past. The tag moves with every renewal.
#
# Inputs (defaults in parentheses): RSI_CLOUD_DIR (~/.rsi/cloud) holds
# pool.tfvars (else gate.tfvars: ssh_key_name = "..."), pool.tfstate, pool.json,
# pool.lock and the spend ledger spend.md; RSI_GATE_IDENTITY (~/.ssh/id_ed25519_rsi)
# is the key pair's private key; RSI_POOL_HOURLY_USD (0.7813: spot max $0.75 +
# 200 GiB gp3 + public IPv4) is the reserve rate and the ledger rate;
# RSI_POOL_LEASE_MINUTES (720, 30-720), RSI_POOL_IDLE_MINUTES (30) and
# RSI_POOL_RENEW_WINDOW_MINUTES (60) tune the clock; RSI_POOL_NOW fixes the
# clock (tests); RSI_POOL_POLL_SECONDS (10) is the boot wait step.
# Needs terraform, rsi-cloud (AWS profile rsi-cloud-terraform), ssh, ssh-keyscan, flock and rsi-cloud (crates/rsi-cloud; RSI_CLOUD_BIN overrides).
# Each host keeps a warm local cargo target (infra/aws/pool/bootstrap.sh.tftpl)
# for its lease; there is no sccache bucket.
set -euo pipefail

usage() {
  sed -n '2,12p' "$0" >&2
  exit 2
}

repo_root=$(cd "$(dirname "$0")/.." && pwd -P)
cloud_dir=${RSI_CLOUD_DIR:-$HOME/.rsi/cloud}
identity=${RSI_GATE_IDENTITY:-$HOME/.ssh/id_ed25519_rsi}
hourly_usd=${RSI_POOL_HOURLY_USD:-0.7813}
lease_minutes=${RSI_POOL_LEASE_MINUTES:-720}
idle_minutes=${RSI_POOL_IDLE_MINUTES:-30}
renew_window=${RSI_POOL_RENEW_WINDOW_MINUTES:-60}
poll=${RSI_POOL_POLL_SECONDS:-10}
max_hosts=6
tf_dir="$repo_root/infra/aws/pool"
state="$cloud_dir/pool.tfstate"
pool_file="$cloud_dir/pool.json"
hosts_json="$cloud_dir/pool.hosts.tfvars.json"
ledger="$cloud_dir/spend.md"
tfvars="$cloud_dir/pool.tfvars"
[ -f "$tfvars" ] || tfvars="$cloud_dir/gate.tfvars"
region=us-west-1
export AWS_PROFILE=${AWS_PROFILE:-rsi-cloud-terraform}

# The spend guard and the lease state are crates/rsi-cloud (RSI_CLOUD_BIN, else rsi-cloud on PATH).
rsi_cloud=${RSI_CLOUD_BIN:-$(command -v rsi-cloud || true)}
spend() { "$rsi_cloud" spend "$@"; }
st() {
  if [ -n "${RSI_POOL_NOW:-}" ]; then
    "$rsi_cloud" pool-state --file "$pool_file" --now "$RSI_POOL_NOW" "$@"
  else
    "$rsi_cloud" pool-state --file "$pool_file" "$@"
  fi
}
now() { if [ -n "${RSI_POOL_NOW:-}" ]; then echo "$RSI_POOL_NOW"; else date -u +%Y-%m-%dT%H:%M:%SZ; fi; }
need_tools() {
  local tool
  for tool in terraform ssh ssh-keyscan ssh-keygen flock; do
    command -v "$tool" >/dev/null || { echo "cloud-pool: missing $tool" >&2; exit 2; }
  done
  [ -n "$rsi_cloud" ] && [ -x "$rsi_cloud" ] || { echo "cloud-pool: missing rsi-cloud (the spend guard; install it with make release-install)" >&2; exit 2; }
  [ -f "$tfvars" ] || { echo "cloud-pool: missing $tfvars (ssh_key_name = \"...\")" >&2; exit 2; }
  [ -f "$identity" ] || { echo "cloud-pool: missing identity $identity" >&2; exit 2; }
}
take_lock() {
  mkdir -p "$cloud_dir/logs"
  exec 9>>"$cloud_dir/pool.lock"
  flock -n 9 || { echo "cloud-pool: another pool command is running" >&2; exit 3; }
}

tf_apply() {
  terraform -chdir="$tf_dir" apply -input=false -no-color -auto-approve -state="$state" \
    -var-file="$tfvars" -var-file="$hosts_json" >"$cloud_dir/logs/pool-apply.log" 2>&1
}
# Make terraform match pool.json (minus --drop slots, plus --set SLOT=EXPIRY).
sync_pool() {
  st hosts-json --out "$hosts_json" "$@"
  tf_apply
}
tf_hosts() { terraform -chdir="$tf_dir" output -no-color -state="$state" -json hosts 2>/dev/null; }

remote() {
  local ip=$1
  shift
  ssh -F /dev/null -T -o BatchMode=yes -o IdentitiesOnly=yes -o StrictHostKeyChecking=yes \
    -o ConnectTimeout=10 -i "$identity" "ec2-user@$ip" "$@"
}

# ---- ledger lines: the formats rsi-cloud spend and rsi-cloud watchdog parse ----

ledger_release() { # TOKEN WHY
  printf 'Gate reservation %s: released, no host started (%s) (%s).\n' "$1" "$2" "$(now)" >>"$ledger"
}

# The host is gone (or being destroyed for good): the stop line names the
# lease's reservation and the actual cost replaces it.
ledger_stop() { # SLOT REASON DESTROY_NOTE
  local slot=$1 reason=$2 note=$3 iid tok end elapsed cost
  iid=$(st get "$slot" instance_id 2>/dev/null || true)
  tok=$(st get "$slot" reservation_id)
  end=$(now)
  set -- $(st cost "$slot" "$end")
  elapsed=$1
  cost=$2
  printf 'Gate window %s: stop %s, elapsed %s s = %s h, est compute $%s at $%s/h; %s; pool %s (%s); reservation %s.\n' \
    "${iid:-unknown}" "$end" "$elapsed" "$(awk -v s="$elapsed" 'BEGIN { printf "%.4f", s / 3600 }')" \
    "$cost" "$hourly_usd" "$note" "$slot" "$reason" "$tok" >>"$ledger"
}

# destroy_slots REASON SLOT...: drop the hosts from terraform, then settle each.
# A destroy that keeps failing leaves the lease (and its reservation) in place
# for the next reap; the guest shutdown and the orphan sweep still bound it.
destroy_slots() {
  local reason=$1 slot attempt ok=0
  shift
  for attempt in 1 2 3; do
    if sync_pool --drop "$@"; then
      ok=1
      break
    fi
    sleep "$poll"
  done
  if [ "$ok" -ne 1 ]; then
    echo "cloud-pool: terraform apply failed while destroying $*; leases kept, see $cloud_dir/logs/pool-apply.log" >&2
    return 1
  fi
  for slot in "$@"; do
    ledger_stop "$slot" "$reason" "terraform destroy ok"
    st drop "$slot"
    echo "cloud-pool: destroyed $slot ($reason)" >&2
  done
}

# ---- boot ----

# Pin the host key only after it matches the fingerprint cloud-init printed to
# the instance console (read out of band), then wait for the bootstrap marker.
boot_wait() { # IP INSTANCE_ID
  local ip=$1 iid=$2 scanned= fingerprint verified=0 ready=0 _
  ssh-keygen -R "$ip" >/dev/null 2>&1 || true
  for _ in $(seq 60); do
    scanned=$(ssh-keyscan -T 5 -t ed25519 "$ip" 2>/dev/null | grep -v '^#' || true)
    [ -n "$scanned" ] && break
    sleep "$poll"
  done
  [ -n "$scanned" ] || { echo "cloud-pool: SSH never answered on $ip" >&2; return 1; }
  fingerprint=$(printf '%s\n' "$scanned" | ssh-keygen -lf - | awk '{print $2}')
  for _ in $(seq 60); do
    if "$rsi_cloud" aws ec2 get-console-output --region "$region" --instance-id "$iid" --latest \
      --query Output --output text 2>/dev/null | grep -F "$fingerprint" | grep -q ED25519; then
      verified=1
      break
    fi
    sleep "$poll"
  done
  [ "$verified" -eq 1 ] || { echo "cloud-pool: host key $fingerprint of $ip not confirmed by the console" >&2; return 1; }
  mkdir -p "$HOME/.ssh"
  printf '%s\n' "$scanned" >>"$HOME/.ssh/known_hosts"
  for _ in $(seq 150); do
    if remote "$ip" test -f /var/lib/rsi-pool-ready 2>/dev/null; then
      ready=1
      break
    fi
    sleep "$poll"
  done
  [ "$ready" -eq 1 ] || { echo "cloud-pool: bootstrap did not finish on $ip" >&2; return 1; }
}

# ---- up ----

cmd_up() {
  local want=${1:-} new_slots slot token start expires out hosts_out iid ip
  [[ "$want" =~ ^[0-9]+$ ]] || usage
  if [ "$want" -lt 1 ] || [ "$want" -gt "$max_hosts" ]; then
    echo "cloud-pool: the pool holds 1 to $max_hosts hosts, not $want" >&2
    exit 2
  fi
  need_tools
  take_lock
  new_slots=$(st slots --want "$want" | sed -n 's/^new=//p')
  if [ -z "$new_slots" ]; then
    echo "cloud-pool: the pool already has $want host(s) or more" >&2
    return 0
  fi
  spend check --ledger "$ledger" || exit 4

  # Reserve every new host's lease before anything is applied; all or nothing.
  local reserved= held
  for slot in $new_slots; do
    token="res-$slot-$(date +%s)-$$"
    if [ -n "${RSI_POOL_NOW:-}" ]; then
      out=$(spend reserve --ledger "$ledger" --minutes "$lease_minutes" --rate "$hourly_usd" --token "$token" --now "$RSI_POOL_NOW") || out=
    else
      out=$(spend reserve --ledger "$ledger" --minutes "$lease_minutes" --rate "$hourly_usd" --token "$token") || out=
    fi
    if [ "$out" != "reservation_id=$token" ]; then
      for held in $reserved; do ledger_release "${held#*:}" "pool up refused"; done
      echo "cloud-pool: reservation refused for $slot; nothing applied" >&2
      exit 4
    fi
    reserved="$reserved $slot:$token"
  done

  start=$(now)
  expires=$(st shift "$start" "$lease_minutes")
  for slot in $new_slots; do
    token=$(printf '%s\n' $reserved | grep "^$slot:" | cut -d: -f2)
    st put "$slot" state=starting lease_no=1 "lease_start=$start" "lease_expires_at=$expires" \
      "lease_minutes=$lease_minutes" "reservation_id=$token" "hourly_usd=$hourly_usd" \
      "last_job_at=$start" renewal=scheduled instance_id= host= public_ip=
  done

  mkdir -p "$cloud_dir/logs"
  terraform -chdir="$tf_dir" init -input=false -no-color >/dev/null
  if ! sync_pool; then
    echo "cloud-pool: terraform apply failed; rolling the new hosts back (see $cloud_dir/logs/pool-apply.log)" >&2
    hosts_out=$(tf_hosts || echo '{}')
    for slot in $new_slots; do
      iid=$(printf '%s' "$hosts_out" | st output-field "$slot" instance_id 2>/dev/null || true)
      [ -n "$iid" ] && st put "$slot" "instance_id=$iid"
    done
    if sync_pool --drop $new_slots; then
      for slot in $new_slots; do
        if [ -n "$(st get "$slot" instance_id 2>/dev/null || true)" ]; then
          ledger_stop "$slot" apply_failed "rolled back after a failed apply"
        else
          ledger_release "$(st get "$slot" reservation_id)" "terraform apply failed"
        fi
        st drop "$slot"
      done
    else
      echo "cloud-pool: rollback failed too; the leases stay recorded (reservations stand) and the guest shutdown and orphan sweep bound them" >&2
    fi
    exit 1
  fi

  hosts_out=$(tf_hosts)
  local failed=
  for slot in $new_slots; do
    iid=$(printf '%s' "$hosts_out" | st output-field "$slot" instance_id)
    ip=$(printf '%s' "$hosts_out" | st output-field "$slot" public_ip)
    st put "$slot" "instance_id=$iid" "public_ip=$ip" "host=ec2-user@$ip"
    printf 'Gate window %s: start %s, %s spot pool %s, public IPv4 %s, est $%s/h (lease %s min until %s, reservation %s).\n' \
      "$iid" "$(now)" "$(terraform -chdir="$tf_dir" output -no-color -state="$state" -raw instance_type 2>/dev/null || echo c7i.8xlarge)" \
      "$slot" "$ip" "$hourly_usd" "$lease_minutes" "$expires" "$(st get "$slot" reservation_id)" >>"$ledger"
    echo "cloud-pool: $slot $iid at $ip (lease until $expires)" >&2
  done
  for slot in $new_slots; do
    ip=$(st get "$slot" public_ip)
    iid=$(st get "$slot" instance_id)
    if boot_wait "$ip" "$iid"; then
      st put "$slot" state=ready "last_job_at=$(now)"
    else
      failed="$failed $slot"
    fi
  done
  if [ -n "$failed" ]; then
    echo "cloud-pool: destroying hosts that never became ready:$failed" >&2
    # shellcheck disable=SC2086
    destroy_slots boot_failed $failed || true
    exit 1
  fi
}

# ---- renew ----

# Renew one lease: reserve the NEXT lease first, then move the guest timer and
# the tags, then settle the ended lease. Any failure releases the new
# reservation and leaves the host to be destroyed at its current expiry.
renew_lease() { # SLOT
  local slot=$1 old_exp old_tok lease_no ip tok new_exp mins why errfile
  old_exp=$(st get "$slot" lease_expires_at)
  old_tok=$(st get "$slot" reservation_id)
  lease_no=$(st get "$slot" lease_no)
  ip=$(st get "$slot" public_ip)
  tok="res-$slot-n$((lease_no + 1))-$(date +%s)-$$"
  errfile=$(mktemp "${TMPDIR:-/tmp}/cloud-pool-reserve.XXXXXX")
  # The next lease starts when this one ends, which fixes its UTC day.
  if ! spend reserve --ledger "$ledger" --minutes "$lease_minutes" --rate "$hourly_usd" \
    --token "$tok" --now "$old_exp" >/dev/null 2>"$errfile"; then
    why=$(tail -n 1 "$errfile" | cut -c1-240)
    rm -f "$errfile"
    st put "$slot" renewal=refused "renewal_note=$why"
    echo "cloud-pool: $slot not renewed ($why); destroyed at $old_exp" >&2
    return 0
  fi
  rm -f "$errfile"
  new_exp=$(st shift "$old_exp" "$lease_minutes")
  mins=$(st until "$new_exp")
  if ! remote "$ip" "sudo -n shutdown -c || true; sudo -n shutdown -h +$mins 'rsi pool lease reached'" </dev/null; then
    why="could not move the guest shutdown timer"
  elif ! sync_pool --set "$slot=$new_exp"; then
    why="terraform could not move the rsi:expires-at tags"
    # Put the guest timer back: the tags still say the old expiry.
    remote "$ip" "sudo -n shutdown -c || true; sudo -n shutdown -h +$(st until "$old_exp") 'rsi pool lease reached'" </dev/null || true
  else
    why=
  fi
  if [ -n "$why" ]; then
    ledger_release "$tok" "renewal of $slot failed: $why"
    st put "$slot" renewal=refused "renewal_note=$why"
    echo "cloud-pool: $slot not renewed ($why); destroyed at $old_exp" >&2
    return 0
  fi
  # The ended lease is settled in full: the host is alive and continues past it.
  set -- $(st cost "$slot" "$old_exp")
  printf 'Pool lease %s: %s ended, renewed for %s more min, elapsed %s s = %s h, est compute $%s at $%s/h; pool %s (%s); reservation %s.\n' \
    "$old_exp" "$(st get "$slot" instance_id)" "$lease_minutes" "$1" \
    "$(awk -v s="$1" 'BEGIN { printf "%.4f", s / 3600 }')" "$2" "$hourly_usd" "$slot" "lease $lease_no" "$old_tok" >>"$ledger"
  st put "$slot" "lease_no=$((lease_no + 1))" "lease_start=$old_exp" "lease_expires_at=$new_exp" \
    "reservation_id=$tok" renewal=scheduled renewal_note=
  echo "cloud-pool: $slot renewed until $new_exp (reservation $tok)" >&2
}

# ---- reap ----

cmd_reap() {
  need_tools
  take_lock
  local slot iid json inst_state action expire_slots= idle_slots= renew_slots= failed=0
  # 1. Hosts AWS already took back (spot interruption) or that vanished.
  for slot in $(st list); do
    iid=$(st get "$slot" instance_id 2>/dev/null || true)
    [ -n "$iid" ] || continue
    json=$("$rsi_cloud" aws ec2 describe-instances --region "$region" --instance-ids "$iid" --output json 2>/dev/null) || continue
    inst_state=$(printf '%s' "$json" | st instance-state)
    case "$inst_state" in
      terminated | shutting-down)
        if [ "$(printf '%s' "$json" | spend spot-reason 2>/dev/null || true)" = spot_interrupted ]; then
          echo "cloud-pool: spot_interrupted: AWS reclaimed $slot ($iid); its lease is settled" >&2
          destroy_slots spot_interrupted "$slot" || failed=1
        else
          destroy_slots vanished "$slot" || failed=1
        fi
        ;;
    esac
  done
  # 2. Expired and idle hosts are destroyed; leases near their end renew.
  while read -r slot action; do
    [ -n "$slot" ] || continue
    case "$action" in
      expire) expire_slots="$expire_slots $slot" ;;
      idle) idle_slots="$idle_slots $slot" ;;
      renew) renew_slots="$renew_slots $slot" ;;
    esac
  done <<EOF
$(st due --idle-minutes "$idle_minutes" --renew-window "$renew_window")
EOF
  # shellcheck disable=SC2086
  [ -z "$expire_slots" ] || destroy_slots lease_expired $expire_slots || failed=1
  # shellcheck disable=SC2086
  [ -z "$idle_slots" ] || destroy_slots idle_exit $idle_slots || failed=1
  for slot in $renew_slots; do
    renew_lease "$slot"
  done
  [ "$failed" -eq 0 ] || exit 1
  echo "cloud-pool: reap done at $(now): expired:${expire_slots:- -} idle:${idle_slots:- -} renewal considered:${renew_slots:- -}" >&2
}

# ---- down ----

cmd_down() {
  need_tools
  take_lock
  local attempt destroyed=0 leftover slot status=0
  mkdir -p "$cloud_dir/logs"
  terraform -chdir="$tf_dir" init -input=false -no-color >/dev/null
  st hosts-json --out "$hosts_json"
  for attempt in 1 2 3; do
    if terraform -chdir="$tf_dir" destroy -input=false -no-color -auto-approve -state="$state" \
      -var-file="$tfvars" -var-file="$hosts_json" >"$cloud_dir/logs/pool-destroy.log" 2>&1; then
      destroyed=1
      break
    fi
    sleep "$poll"
  done
  # Independent of Terraform state: nothing tagged as a pool host may remain.
  leftover=$("$rsi_cloud" aws ec2 describe-instances --region "$region" \
    --filters Name=tag:Service,Values=rsi-pool \
    Name=instance-state-name,Values=pending,running,stopping,stopped \
    --query 'Reservations[].Instances[].InstanceId' --output text 2>/dev/null) || leftover=unknown
  if [ -n "$leftover" ] && [ "$leftover" != None ]; then
    echo "cloud-pool: LEFTOVER pool instances after destroy: $leftover" >&2
    [ "$leftover" != unknown ] && "$rsi_cloud" aws ec2 terminate-instances --region "$region" \
      --instance-ids $leftover >/dev/null 2>&1 || true
    status=1
  fi
  if [ "$destroyed" -ne 1 ]; then
    echo "cloud-pool: terraform destroy failed; leases kept (reservations stand), see $cloud_dir/logs/pool-destroy.log" >&2
    exit 1
  fi
  # A successful destroy cannot settle leases while the independent check is unknown.
  if [ "$leftover" = unknown ]; then
    echo "cloud-pool: leftover-instance check unknown; leases kept (reservations stand); retry down when the check succeeds" >&2
    exit 1
  fi
  for slot in $(st list); do
    ledger_stop "$slot" down "terraform destroy ok"
    st drop "$slot"
  done
  echo "cloud-pool: pool destroyed and reservations settled" >&2
  exit "$status"
}

cmd=${1:-}
[ $# -gt 0 ] && shift
case "$cmd" in
  up) cmd_up "$@" ;;
  down) cmd_down ;;
  reap) cmd_reap ;;
  status) st status "$@" ;;
  touch) st touch "$@" ;;
  *) usage ;;
esac
