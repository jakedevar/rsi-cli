#!/usr/bin/env bash
set -euo pipefail
repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"
test "$(node --version)" = v26.7.0
test "$(npm --version)" = 12.0.2
test "$(rustc +1.94.1 --version | cut -d' ' -f2)" = 1.94.1
lane=remote/contracts/v1
node "$lane/generate.mjs" --check
node "$lane/fixtures/generate.mjs" --check
npm --prefix "$lane" ci --ignore-scripts
npm --prefix "$lane" run typecheck
npm --prefix "$lane" run build
npm --prefix "$lane" test
cargo +1.94.1 test --locked --manifest-path "$lane/rust/Cargo.toml"
contract_tmp="$(mktemp -d)"
trap 'rm -rf -- "$contract_tmp"' EXIT
cargo +1.94.1 run --quiet --locked --manifest-path "$lane/rust/Cargo.toml" --bin remote-contract-fixtures > "$contract_tmp/rust.json"
node "$lane/tests/emit.mjs" > "$contract_tmp/typescript.json"
node "$lane/tests/parity.mjs" "$contract_tmp/rust.json" "$contract_tmp/typescript.json"
