---
ticket: S6
plan_doc: thoughts/shared/plans/2026-06-29-mwp-icm-slice-plan.md
generated: 2026-06-29T15:00:00Z
phases_sealed: [1]
status: pending_verification
---

# Verification Manifest - S6 linkage (fully covered)

## Phase 1 - cross-stage linkage

### Automated
- verification_manifest round-trips satisfies/covers
  satisfies: F-001, PLAN-3
- agent_contract cross-stage pass rejects drift
  covers: F-002

### Daemon-level
- [PENDING] rsi-rpc ListSessions returns JSON
  - check: RSI_DAEMON_SOCKET_PATH=/tmp/rsi.sock rsi-rpc ListSessions
  - expected: stdout parses as JSON object with result key
  - satisfies: F-003

### TUI manual
- (none)
