# Historical issue-pipeline measurement retirement (#725)

The PM decision in #477 handoff 13, based on the complete source-only research
in handoff 12, withdraws both shipped reproduction CLIs together:
`scripts/export-issue-pipeline-evidence.py` and `scripts/measure-issue-pipeline.py`.
Their own suites, `scripts/tests/test_export_issue_pipeline_evidence.py` and
`scripts/tests/test_measure_issue_pipeline.py`, retire in the same change.
All 58 feature-specific cases are deliberately withdrawn with the workflow:
7 exporter cases and 51 measurer cases. They are neither ported nor claimed
redundant or covered by native telemetry.

The complete case disposition, grouped by the original suite classes, is:

| Retired suite | Original class | Cases withdrawn |
| --- | --- | ---: |
| Exporter | AttributionTest | 5 |
| Exporter | CumulativeCostTest | 2 |
| Measurer | KnownTotalsTest | 8 |
| Measurer | DedupeTest | 7 |
| Measurer | AttributionTest | 7 |
| Measurer | UnknownEvidenceTest | 6 |
| Measurer | FleetTotalsTest | 2 |
| Measurer | WindowTest | 4 |
| Measurer | ComparisonTest | 6 |
| Measurer | ContractTest | 4 |
| Measurer | CliTest | 7 |

## Historical workflow and current consumers

The adoption measurement and reproduction commands in
[the 2026-10-15 adoption note](../../thoughts/shared/notes/2026-10-15-2304-adoption.md)
are historical; the shipped export/report reproduction workflow is withdrawn.
The [2026-10-20 cost note](../../thoughts/shared/notes/2026-10-20-2304-cost-measurement.md)
records a different method and exporter limitations. Its values and provenance
remain historical evidence, including any comparisons now known to be limited.
Neither note's historical Python invocation is a supported recovery procedure.

At source pin `e6fa373b352562f33ea070a4eaccef6883e5d847`, a fresh rolling/history
check and bounded tracked-source caller search found the active reference and
bootstrap classifications, with no additional required production invoker.
The known manual adoption reproduction is deliberately withdrawn by the PM.
This does not establish runtime inactivity or absence of external automation;
actual external use remains unknown. A newly discovered required consumer is a
counterexample to this disposition and must be recorded for a new decision.

The exporter differenced every non-title purpose, whereas current cost
accounting differences only the three cumulative session purposes with
admitted, priced predecessors. Other purposes keep their recorded costs.
Current consumers use internal session-scoped increment queries and separate
billing classification; the [cost reference](reference.md#model_invocationsestimated_cost_usd-is-session-cumulative-on-launchresume-rows-2593)
describes those rules. The test-only whole-store oracle is not a public API.
Native usage/cost telemetry is not declared compatible with the retired
export/report schemas, attribution, comparison or exit contracts.

## Saved evidence and compatibility

Saved JSON exports with `schema_version: 1` and reports with
`schema: "measure-issue-pipeline/1"` survive untouched, wherever they already
exist. Preserve their original values, identifiers, timestamps and provenance,
including unknown costs, partial coverage, assumptions and attribution limits.
All historical `thoughts/` files and old receipts remain unchanged. SQLite
databases, WAL/SHM files, backups, logs, refs, remotes, branches and timestamps
are preserved. This source retirement performs no saved-file search,
conversion, import, deletion, cleanup or runtime-state operation.

There is no promise to regenerate or replay an old export or report after
source, database schema, refs or history evolve. No wrapper or replacement
command ships here. A future compatible producer/consumer requires an explicit
versioned contract for schemas and unknown fields, decimal costs, nanosecond
timestamps, identity/deduplication, attribution, coverage/unknown semantics,
comparison and exit behavior. It must reconcile current cumulative-cost and
billing semantics and demonstrate compatibility with synthetic evidence.
Historical JSON, measurements and test/gate receipts confer no current
acceptance or production claim.

## Verification boundary and remaining obligations

This is a hand-driven SOURCE-ONLY implementation under #631/#637. No execution
starter or manual script, guard, Cargo, build, test, scoped/dry-run, job,
diagnostic, installer or lifecycle operation is authorized here. Normal
automatic Git hooks, including guard-only compilation, are permitted. All
compiler, static, native, scoped, candidate, baseline, warning and runtime
acceptance checks remain **UNRUN**; hook success is not an exact-head acceptance
receipt. An empty own-suite selection is not zero-test green.

Shared meaningful observers remain required: residual selection (including a
deleted subject with a surviving suite), surviving directory-wide
`scripts/tests/test_fixture_git_guard.sh` and `test_shell_tests.sh`, fixture Git
source readers, source scanners and retained-registry observers. No generic
gate, selector, registry, Make/CI/job/hook or warning baseline changes here.
The performance sampler and its separate 35-case port remain outside this
retirement.

The trusted Linux PM/hub lane must establish safe execution for both candidate
and baseline sources, including installer effects reached by directory-wide
wrappers, before executing the required existing observers and static/native/
scoped checks at the final source SHA. It must retain warning and no-new-failure
comparison evidence, an exact-head receipt and candidate acceptance, then use a
separate queue gate with `derive_filters:true` and verify fetched publication
on rolling. One post-land tooling/custody review remains required. No local
acceptance, enqueue, hold lift, deployment or umbrella closure is implied.
