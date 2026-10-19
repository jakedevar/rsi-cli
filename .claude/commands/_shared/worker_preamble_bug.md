---
version: 10
kind: bug
inherits: worker_preamble.md
---

# Worker preamble — Bug

Reproduce the reported failure, localize the smallest cause, fix it, and verify neighboring behavior. Kind-specific detail: `docs/agents/worker-kinds.md`.

If the report cites `[observed]` database rows, replay the new predicate against them read-only (count before and after) and put it in the handoff; fixture rows you shaped yourself do not prove the fix (`docs/agents/worker-contract.md`, "Replay against cited rows").

## Stage contract

If your handoff includes this block, use all four headings; `### Inputs` names an artifact or discovery budget.

### Inputs
The report and reproducer; discover the owning code with `rg`.
### Process
Reproduce, isolate the cause, apply a focused fix, add a regression check.
### Outputs
A scoped fix, evidence, and verification items.
### Verify
The regression check passes and neighboring checks stay green.
