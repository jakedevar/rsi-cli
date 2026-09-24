---
ticket: V1.1
plan_doc: thoughts/shared/plans/2026-05-06-verification-manifest-pipeline.md
branch: v1.1-verification-manifest-pipeline
generated: 2026-05-06T14:23:00Z
phases_sealed: [1, 2]
status: tui_only_pending
---

# Verification Manifest - V1.1

## Phase 1 - manifest schema

### Automated (PASSED in CI)
- cargo test -p rsi-common manifest_corpus

### Daemon-level
- (none - pure library phase)

### TUI manual
- (none)

## Phase 2 - rsi-rpc helper

### Automated
- cargo build -p rsi-common --bin rsi-rpc

### Daemon-level (delegated to autonomous verifier)
- [PENDING] rsi-rpc against tempdir daemon returns valid JSON for ListSessions
  - check: RSI_DAEMON_SOCKET_PATH=/tmp/rsi-test/daemon.sock cargo run -q -p rsi-common --bin rsi-rpc -- ListSessions
  - expected: stdout parses as JSON object with result key

### TUI manual (Jake)
- (none)
