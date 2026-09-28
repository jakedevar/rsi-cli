# Worker verification reference

Classify each verification item by the observable behavior:

| Check | Bucket |
| --- | --- |
| A code test can exercise the changed behavior | Automated |
| Rendering, color, focus, or keyboard outcome needs a TUI observation | TUI manual |
| DB row, log, RPC payload, sandbox, or socket effect needs daemon observation | Daemon autonomous |

Use an automated test when it can establish the behavior. Explain why each TUI manual item needs observation. Each daemon item carries concrete `check:` and `expected:` lines. Workers emit `VERIFICATION_ITEMS:` in their handoff when the including command asks; the orchestrator writes the manifest at phase seal. Do not append to the manifest mid-phase.

VERIFY-stage handoffs identify satisfied research finding IDs or plan items with `satisfies:` or `covers:`. Annotate the corresponding manifest item with the same keys. `cross_stage_verify_coverage` rejects uncovered declarations. See `crates/rsi-common/src/handoff_schema/body.rs` for parsed handoff shape and the current pipeline command for its return schema.

The separate Closure independent review lane is documented in `.claude/commands/_shared/master_orchestrate_rsi_closure.md`; ordinary workers follow their including command's verification path.
