#!/usr/bin/env bash
# Auto-rebuild TUI on code changes. Daemon must run separately.
set -euo pipefail
command -v cargo-watch &>/dev/null || cargo install cargo-watch
exec cargo watch -w crates/rsi/src -w crates/rsi-common/src -s 'cargo run --bin rsi'
