---
version: 9
kind: refactor
inherits: worker_preamble.md
---

# Worker preamble — Refactor

Improve a measured internal cost while preserving observable behavior. Kind-specific detail: `docs/agents/worker-kinds.md`.

## Stage contract

If your handoff includes this block, use all four headings; `### Inputs` names an artifact or discovery budget.

### Inputs
The structural target; discover affected call sites with `rg`.
### Process
Reshape the internals and measure the improvement.
### Outputs
A scoped diff and the measured improvement.
### Verify
Existing behavior checks pass and the improvement is recorded.
