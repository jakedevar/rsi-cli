#!/usr/bin/env bash
set -euo pipefail

# Build the required binaries
cargo build -p rsi --bin rsi
cargo build -p rsid --bin rsid
cargo build -p rsi-common --bin rsi-agent-mcp

# Run the gated TUI E2E test target
RSI_E2E=1 cargo test -p rsi --test e2e_tui -- --nocapture --test-threads=1
