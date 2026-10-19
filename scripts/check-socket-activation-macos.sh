#!/usr/bin/env bash
# Compile the actual portable activation sources and tests without rsid's
# unrelated native C dependencies (which need an Apple SDK on a Linux host).
set -euo pipefail
root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
fixture="$(mktemp -d)"
trap 'rm -rf "$fixture"' EXIT
python3 - "$root" "$fixture" <<'PY'
import json
import pathlib
import sys
root, fixture = map(pathlib.Path, sys.argv[1:])
(fixture / "Cargo.toml").write_text('''[package]
name = "rsi-socket-activation-portability"
version = "0.0.0"
edition = "2024"
[workspace]
[dependencies]
nix = { version = "=0.29.0", features = ["dir", "fs", "process", "signal", "user", "socket"] }
[dev-dependencies]
tempfile = "3"
[features]
test-shard-mode = []
test-shard-other-02 = []
[[bin]]
name = "inherited-listener"
path = "listener.rs"
[[bin]]
name = "rsi-socket-hold"
path = ''' + json.dumps(str(root / "crates/rsi-socket-hold/src/main.rs")) + '\n')
(fixture / "listener.rs").write_text('#[path = ' + json.dumps(str(root / "crates/rsid/src/instance_guard/inherited_listener.rs")) + ']\nmod inherited_listener;\nfn main() {}\n')
PY
cargo check --manifest-path "$fixture/Cargo.toml" --target x86_64-apple-darwin --all-targets
