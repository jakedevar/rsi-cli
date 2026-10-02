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

## Known failures

Before debugging a red, classify it against the shared known-failure table:

- `rsi-known-failure classify --log <log>` annotates every failing test as
  `KNOWN #N <class>`, `KNOWN? #N` (name-only match) or `NEW`.
- `rsi-known-failure query <test-id>` prints the records for one test.
- Any agent session can ask the daemon directly: native
  `rsi_control_query_failure_signatures` (or `rsi-rpc AgentQueryFailureSignatures
  --params '{"test_id":"<id>"}'`, or `{"digest":"<sha256>"}`). It reads open Issues
  live, returns each record with its owner Issue (`record.issue`) and never
  returns the record of a closed Issue; `records: []` means not known.

When filing a qa-regression Issue for a red, paste a record into the Issue body:

```
rsi-known-failure block --test <test-id> --issue <n> --class <class> --log <log>
```

Records live in fenced `rsi-failure-signature` blocks owned by the Issue, so
closing the Issue retires its signatures (the live query drops them at once; the
snapshot file drops them at the next `export`, and every classifier ignores a
non-open owner and warns when the snapshot is older than 24 hours). The lander and `run-rolling-qa.py`
annotate their reds with the same classifier without changing any gate.
