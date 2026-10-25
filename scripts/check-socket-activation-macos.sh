#!/usr/bin/env bash
# Compile the actual portable activation sources and tests without rsid's
# unrelated native C dependencies (which need an Apple SDK on a Linux host).
set -euo pipefail
root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
fixture="$(mktemp -d)"
trap 'rm -rf "$fixture"' EXIT

# toml_string <text>: a TOML basic string holding the text (backslash and quote escaped).
toml_string() {
    local text="${1//\\/\\\\}"
    printf '"%s"' "${text//\"/\\\"}"
}

# Plain heredocs (#465; this was an interpreter heredoc): the fixture package names the
# real sources by absolute path.
cat >"$fixture/Cargo.toml" <<TOML
[package]
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
path = $(toml_string "$root/crates/rsi-socket-hold/src/main.rs")
TOML
cat >"$fixture/listener.rs" <<RUST
#[path = $(toml_string "$root/crates/rsid/src/instance_guard/inherited_listener.rs")]
mod inherited_listener;
fn main() {}
RUST
cargo check --manifest-path "$fixture/Cargo.toml" --target x86_64-apple-darwin --all-targets
