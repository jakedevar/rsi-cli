# RSI Documentation

## Start here

- [RSI Operator Manual: AI-Agent Harness](agent-harness-operator-manual.md) —
  the end-to-end operating model, safe daily workflow, capability boundaries,
  diagnostics, and evaluation guidance.
- [Keybindings Reference](keybindings.md) — complete keyboard and `:` command
  reference for the TUI.

## Structured and advanced work

- [Using the DAG Feature With an Epic](dag-on-epic-guide.md) — create and run
  a dependency topology under an Epic.
- [Memory Architecture](memory-architecture.md) — project-scoped memory
  behavior, indexing, and retrieval.
- [ProgramRun kernel](program-run-kernel.md) — operator-only/internal durable
  program execution kernel; not an ordinary daily workflow.
- [Sandbox storage](sandbox-storage.md) — target-cache pressure controls,
  previews, bounded reclamation, safety refusals, and staged recovery.
- [Source worktree cohort settlement](cohort-settlement.md) — operator-only,
  audit-first retirement of terminal worktrees proved integrated by exact Git
  ancestry, with durable receipts and startup recovery.
- [Safe cleanup during ordinary archive](archive-cleanup.md) — the narrow
  branch-preserving cleanup proof, durable receipt/recovery states, and
  fresh-custody unarchive contract for one terminal Session leaf.

## Architecture and development reference

- [`architecture/`](architecture/) — system design material and subsystem
  references. Treat status and implementation claims there as context unless
  they are corroborated by the current UI, source, and focused guide.
- [Evaluation corpus guide](../eval/README.md) — isolated replay and baseline
  regression evaluation for harness development.

## Documentation status

The operator manual is the navigation layer. Specialist guides explain their
subsystems in depth. The live UI, session information (`F3`), diagnostics, and
daemon validation remain authoritative when a guide and a configured
installation differ.
