---
ticket: V1.1
plan_doc: thoughts/shared/plans/2026-05-06-verification-manifest-pipeline.md
generated: 2026-05-06T14:23:00Z
phases_sealed: [1]
status: pending_verification
---

# Verification Manifest - V1.1

## Phase 1 - daemon item missing command

### Automated
- cargo test --workspace

### Daemon-level
- [PENDING] ListSessions returns JSON
  - expected: stdout parses as JSON object with result key

### TUI manual
- (none)
