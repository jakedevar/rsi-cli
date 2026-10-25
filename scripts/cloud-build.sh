#!/usr/bin/env bash
# The Python-free cloud BUILD lane (#3844, parent #3460): build one exact rolling
# commit on an ephemeral AWS SPOT host and run a small test set there, then
# destroy the host. Bash and Rust only; no .py file is invoked on the hub or on
# the host.
#
#   cloud-build.sh cloud SHA [--instance-type TYPE] [--repo DIR] [--mirror DIR]
#       The one command the daemon's cloud_sweep job runs for `lane: "build"`.
#       Preflights the prerequisites (a missing one exits 2 with a typed
#       `cloud-build: missing <what>` line, before any spend), bundles SHA out
#       of the daemon-owned mirror (scripts/cloud-sweep.sh bundle; SHA must be
#       the current rolling tip), and only then runs scripts/cloud-gate.sh --run
#       with a 60-minute host lifetime. cloud-gate.sh owns the spend reservation
#       (rsi-cloud spend reserve, ~$0.78 at the $0.7813/h spot rate), apply, the
#       ledger lines and the EXIT-trap destroy. A host AWS takes back ends the
#       run with `cloud-gate: spot_interrupted` (exit 75): INCOMPLETE, never red.
#   cloud-build.sh hosted SHA
#       Runs under cloud-gate.sh --run (RSI_GATE_HOST, RSI_GATE_IDENTITY,
#       RSI_GATE_RUN_AS, RSI_GATE_INSTANCE_ID in the environment, the bundle at
#       RSI_BUILD_BUNDLE): ship the bundle, run scripts/cloud-build-remote.sh on
#       the host over ssh, keep its output in ~/.rsi/cloud/results/build-SHA.
#
# The verdict is not decided here: the daemon classifies it in Rust from the
# `BUILD_STEP <step> <code>` lines this prints and the spend ledger.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"

usage() {
  echo 'usage: cloud-build.sh cloud SHA [--instance-type TYPE] [--repo DIR] [--mirror DIR] | hosted SHA' >&2
  exit 2
}

[[ $# -ge 2 ]] || usage
mode="$1"
sha="$2"
[[ "$sha" =~ ^[0-9a-f]{40}$ ]] || usage
results="$HOME/.rsi/cloud/results/build-$sha"

if [[ "$mode" == cloud ]]; then
  shift 2
  gate_args=()
  repo="$here/.."
  mirror="${RSI_SWEEP_MIRROR:-$HOME/.rsi/cloud/sweep-mirror.git}"
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --instance-type) gate_args+=(--instance-type "${2:?}"); shift 2 ;;
      --repo) repo="${2:?}"; shift 2 ;;
      --mirror) mirror="${2:?}"; shift 2 ;;
      *) usage ;;
    esac
  done

  # Prerequisites, named before any spend. cloud-gate.sh checks the same tools
  # itself; this is the typed refusal the daemon reports. No aws call is made:
  # the profile is checked in the files the aws CLI reads.
  missing() { echo "cloud-build: missing $1" >&2; exit 2; }
  for tool in terraform aws ssh scp ssh-keyscan ssh-keygen flock git; do
    command -v "$tool" >/dev/null || missing "$tool"
  done
  if [[ -n "${RSI_CLOUD_BIN:-}" ]]; then
    rsi_cloud="$RSI_CLOUD_BIN"
  elif [[ -x "$here/../bin/rsi-cloud" ]]; then
    rsi_cloud="$here/../bin/rsi-cloud"
  else
    rsi_cloud="$(command -v rsi-cloud || true)"
  fi
  [[ -n "$rsi_cloud" && -x "$rsi_cloud" ]] || missing rsi-cloud
  cloud_dir="${RSI_CLOUD_DIR:-$HOME/.rsi/cloud}"
  [[ -f "$cloud_dir/gate.tfvars" ]] || missing gate.tfvars
  [[ -f "${RSI_GATE_IDENTITY:-$HOME/.ssh/id_ed25519_rsi}" ]] || missing ssh-identity
  profile="${AWS_PROFILE:-rsi-cloud-terraform}"
  if [[ -z "${AWS_ACCESS_KEY_ID:-}" ]] &&
    ! grep -qxF "[profile $profile]" "${AWS_CONFIG_FILE:-$HOME/.aws/config}" 2>/dev/null &&
    ! grep -qxF "[$profile]" "${AWS_SHARED_CREDENTIALS_FILE:-$HOME/.aws/credentials}" 2>/dev/null; then
    missing "aws-profile $profile"
  fi

  rm -rf "$results"
  mkdir -p "$results"
  chmod 700 "$results"
  # Bundle first, so a SHA that is not the rolling tip fails before the host is
  # reserved or applied.
  bundle="$results/source.bundle"
  "$here/cloud-sweep.sh" bundle "$sha" "$bundle" --repo "$repo" --mirror "$mirror"
  chmod 600 "$bundle"
  gate_status=0
  RSI_BUILD_BUNDLE="$bundle" RSI_GATE_STATS_COPY="$results/sccache.json" \
    "$here/cloud-gate.sh" ${gate_args[@]+"${gate_args[@]}"} --max-lifetime-minutes 60 --run -- \
    "$here/cloud-build.sh" hosted "$sha" || gate_status=$?
  rm -f "$bundle"
  exit "$gate_status"
fi

[[ "$mode" == hosted && $# -eq 2 ]] || usage
host="${RSI_GATE_HOST:-}"
key="${RSI_GATE_IDENTITY:-}"
run_as="${RSI_GATE_RUN_AS:-rsi}"
bundle="${RSI_BUILD_BUNDLE:-}"
[[ "$host" =~ ^ec2-user@[0-9.]+$ && -f "$key" && -f "$bundle" && "$run_as" =~ ^[a-z_][a-z0-9_-]*$ ]] || usage
mkdir -p "$results"
chmod 700 "$results"

# The host key was pinned by cloud-gate.sh after the console confirmed it.
ssh_opts=(-F /dev/null -o BatchMode=yes -o IdentitiesOnly=yes -o StrictHostKeyChecking=yes
  -o ConnectTimeout=10 -i "$key")
remote_bundle="/tmp/rsi-cloud-build-$sha.bundle"
echo "BUILD_HOST instance=${RSI_GATE_INSTANCE_ID:-unknown} sha=$sha"
scp "${ssh_opts[@]}" -q "$bundle" "$host:$remote_bundle"
# The remote script comes from this trusted tree on stdin, never from the
# commit under test. 50 minutes of a 60-minute host: the guest's own shutdown
# is the backstop.
set +e
timeout 3000 ssh "${ssh_opts[@]}" -T -o ServerAliveInterval=30 -o ServerAliveCountMax=10 "$host" \
  "sudo -n chown $run_as $remote_bundle && exec sudo -n -H -u $run_as bash -s -- $sha $remote_bundle" \
  <"$here/cloud-build-remote.sh" 2>&1 | tee "$results/remote.out"
remote_status="${PIPESTATUS[0]}"
set -e
chmod 600 "$results/remote.out" 2>/dev/null || true
echo "BUILD_REMOTE_EXIT $remote_status"
exit "$remote_status"
