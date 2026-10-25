#!/bin/bash
# The part of the cloud BUILD lane (#3844) that runs ON the gate host, as the
# toolchain owner. scripts/cloud-build.sh pipes this file over ssh (`bash -s`),
# so it never comes from the commit under test. Bash and the host's own tools
# only: no Python anywhere.
#
#   cloud-build-remote.sh SHA BUNDLE
#
# Steps, each logged to $root/logs/<step>.log with its exit code printed as
# `BUILD_STEP <step> <code>`; a step after a failed one prints `skipped`:
#   checkout   fetch the exact SHA from the git bundle the hub shipped
#   toolchain  rustup, at the channel pinned by rust-toolchain.toml
#   build      cargo build --workspace --locked
#   test       cargo nextest run (cargo test when nextest is absent) for
#              rsi-common and rsid-core, lib tests only
# Then a bounded tail of each log (every line prefixed `| ` so test output can
# never forge a marker line) and `BUILD_STEPS_DONE <sha>`. The script exits with
# the first failing step's code, 0 when all passed. The hub classifies the
# verdict from these lines (crates/rsid/src/agent_jobs/cloud_build.rs).
set -uo pipefail

sha="${1:-}"
bundle="${2:-}"
[[ "$sha" =~ ^[0-9a-f]{40}$ && -n "$bundle" ]] || { echo "usage: cloud-build-remote.sh SHA BUNDLE" >&2; exit 2; }

root="${RSI_BUILD_ROOT:-$HOME/rsi-build}"
src="$root/src"
logs="$root/logs"
rm -rf "$root"
mkdir -p "$logs" "$src"
export PATH="$HOME/.cargo/bin:$PATH"
# One-off build: no incremental state, and the line tables the gate uses.
export CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=line-tables-only
export CARGO_TARGET_DIR="$root/target"

status=0
failed=0

# step NAME COMMAND...: run, log, print the code. The first failure fixes the
# script's own exit code and skips every later step.
step() {
  local name="$1" code
  shift
  if [ "$failed" -ne 0 ]; then
    echo "BUILD_STEP $name skipped"
    return 0
  fi
  "$@" >"$logs/$name.log" 2>&1
  code=$?
  echo "BUILD_STEP $name $code"
  if [ "$code" -ne 0 ]; then
    failed=1
    status="$code"
  fi
  return 0
}

do_checkout() {
  git -C "$src" init -q &&
    git -C "$src" fetch -q "$bundle" refs/remotes/origin/rolling &&
    [ "$(git -C "$src" rev-parse FETCH_HEAD)" = "$sha" ] &&
    git -C "$src" checkout -q --detach "$sha"
}

channel=
do_toolchain() {
  channel="$(sed -n 's/^channel[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "$src/rust-toolchain.toml" | head -n 1)"
  [ -n "$channel" ] || { echo "rust-toolchain.toml names no channel"; return 1; }
  if ! command -v rustup >/dev/null 2>&1; then
    curl -fsSL --proto =https --tlsv1.2 https://sh.rustup.rs |
      sh -s -- -y --profile minimal --default-toolchain none || return 1
    export PATH="$HOME/.cargo/bin:$PATH"
  fi
  # The gate host's bootstrap installed this channel; install only when the
  # commit pins another one.
  rustup toolchain list | grep -q "^$channel" || rustup toolchain install "$channel" --profile minimal || return 1
  # The pinned channel wins over rust-toolchain.toml's component list, so cargo
  # never stops to download rustfmt or clippy it does not need.
  export RUSTUP_TOOLCHAIN="$channel"
  cargo --version && rustc --version
}

do_build() {
  (cd "$src" && cargo build --workspace --locked)
}

test_runner=cargo-test
do_test() {
  if cargo nextest --version >/dev/null 2>&1; then
    test_runner=nextest
    (cd "$src" && cargo nextest run --locked --no-fail-fast -p rsi-common -p rsid-core --lib)
  else
    (cd "$src" && cargo test --locked -p rsi-common -p rsid-core --lib)
  fi
}

step checkout do_checkout
step toolchain do_toolchain
step build do_build
step test do_test
echo "BUILD_TEST_RUNNER $test_runner"

# A bounded tail of each log: the host (and its logs) are destroyed after this.
for name in checkout toolchain build test; do
  [ -f "$logs/$name.log" ] || continue
  echo "BUILD_TAIL_BEGIN $name"
  tail -n 40 "$logs/$name.log" | cut -c1-400 | sed 's/^/| /'
  echo "BUILD_TAIL_END $name"
done
echo "BUILD_STEPS_DONE $sha"
exit "$status"
