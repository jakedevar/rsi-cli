# Human decisions required before codegraph implementation

Status: blocking approval checklist

No implementation has been authorized

## Stage contract

### Inputs

- Comparative research and stable findings.
- North-star specification, schema, interfaces, ADRs, verification contract,
  risk register, and slice map.
- RSI maintainer preferences for disk, indexing, TUI, database migrations, and
  staged delivery.

### Process

- Present only decisions that materially change architecture, scope, risk, or
  the first implementation slice.
- Give a recommended default and its consequence.
- Stop before implementation until the human explicitly resolves the blocking
  decisions.

### Outputs

- A concise approval checklist and recommended defaults.
- A clear distinction between first-slice blockers and later decisions.

### Verify

- Every blocking research open question appears here.
- Choosing the recommendations yields one unambiguous S1 implementation scope.
- No answer is inferred from silence.

## Blocking before S1

### D1. Canonical structural storage

**Recommendation:** approve a separate rebuildable `codegraph.sqlite` per
effective workspace/worktree. Permit only a small workspace catalog and later
durable federation ledger in main `rsi.db`; keep `memory.sqlite` unchanged.

Alternatives:

- put structure in main `rsi.db` (simpler references, larger critical failure/
  migration/write domain);
- put structure in `memory.sqlite` (fewer DBs, incompatible lifecycle/scope);
- one global codegraph DB (easier future cross-repo, higher leakage/contention
  risk).

**Decision needed now:** approve/reject the per-workspace derived DB foundation.

### D2. First implementation slice

**Recommendation:** approve S1 only after S0 fixtures/benchmark guards:
worktree-scoped DB, typed identity/snapshot/file/module/function/evidence subset,
explicit operator indexing/status, and determinism/scope/span tests.

This intentionally excludes watcher/incremental updates, native tools, TUI,
federation, broad extraction, ContextPipeline, rust-analyzer/SCIP, communities,
summaries, export, and multi-repo.

**Decision needed now:** approve this narrow foundation or request a different
vertical boundary.

### D3. Initial indexing policy

**Recommendation:** explicit opt-in and RSI-dogfood-only during S1 through S7.
Do not automatically index every project until quotas, health, value, and
cleanup are proven.

Alternative: auto-enable for any recognized Rust workspace, which provides
faster adoption but creates unmeasured daemon/disk load.

**Decision needed now:** opt-in dogfood or broader automatic indexing.

### D4. Snapshot retention for the first slice

**Recommendation:** S1 retains only the current complete snapshot plus the
immediately previous complete snapshot; dirty snapshots are replaced unless
explicitly pinned by an active comparison. Make limits configurable only after
measurement, not in the first slice.

Alternative: retain full structural history immediately, increasing schema/
quota/pruning complexity.

**Decision needed now:** bounded two-snapshot foundation or a different
retention contract.

### D5. Cargo integration boundary

This affects S3, but the S1 schema should avoid assuming a crate API.

**Recommendation:** benchmark both approaches during S3 planning:

1. invoke pinned `cargo metadata --format-version 1` and parse JSON;
2. add `cargo_metadata` if it materially improves typing/control without
   version drift.

Do not add the dependency in S1.

**Decision needed now:** accept this measured deferral, or require a specific
Cargo boundary up front.

## Blocking before S5/S6

### D6. Native tool surface

**Recommendation:** three read-only scope-bound tools:
`rsi_codegraph_search`, `rsi_codegraph_explain`, and
`rsi_codegraph_traverse`. No native index, link, export, or arbitrary scope
tools.

**Decision timing:** before S5.

### D7. TUI timing and keybinding

**Recommendation:** prove daemon/query/native value before S6; then add
`:codegraph`. Consider `gC` only after a full collision/ergonomics review and
update `docs/keybindings.md`.

Alternative: include minimal status/inspector in the first program to improve
operator observability, at higher delivery scope.

**Decision timing:** decide whether S6 is required before the S7 value gate and
whether `gC` is acceptable.

### D8. Query default maxima

**Recommendation:** begin with conservative defaults:

- depth 2 for neighbors/subgraph, profile-specific depth 3 for impact;
- 80 nodes, 160 edges, two evidence records per fact;
- 2 seconds manager timeout;
- 1,200 search, 2,000 explain, and 3,000 traversal output tokens.

Server hard maxima should be modest multiples, not unlimited.

**Decision timing:** before S4/S5; confirm or tune from benchmark evidence.

## Blocking before S8 federation

### D9. Main `rsi.db` federation ledger

**Recommendation:** approve a later versioned migration for a small workspace
catalog and durable typed `codegraph_links`/evidence ledger. Keep all structural
tables out of main DB.

**Decision timing:** after S7 proves value; not needed for S1-S7.

### D10. LLM-generated federation links

**Recommendation:** deterministic/manual links first. If LLM suggestions are
later admitted, store them as `candidate`, excluded by default until accepted.
Never map them into structural tables.

**Decision timing:** before any semantic link producer in S8/S9.

### D11. Historical link behavior

**Recommendation:** stable node links survive structural rebuilds; links pinned
to purged versions remain historical-but-unavailable unless retention protects
them. Never retarget by name or embedding similarity.

**Decision timing:** before S8 and snapshot-pruning behavior beyond S7.

### D12. Dreamer lineage

**Recommendation:** explicitly affirm that `Observation.source_ids` remains
observation-to-observation lineage. Use the separate federation ledger for
codegraph relationships.

**Decision timing:** before S8.

## Deferred decisions, not S1 blockers

### D13. rust-analyzer versus SCIP

**Recommendation:** neither in MVP. Run S10 only if S7 identifies
resolution-specific failures and the precision gain fits latency/RSS/
cancellation budgets.

### D14. petgraph dependency

**Recommendation:** use SQL first for exact/shallow queries and approve a direct
petgraph dependency only when S4 path/impact benchmarks justify bounded induced
graphs. Petgraph is never persistence.

### D15. Community detection

**Recommendation:** defer. If a named task justifies it, benchmark
`leiden-rs` against `graphrs`; store results as derived annotations.

### D16. Node summaries

**Recommendation:** defer. Require correctness-per-token benefit plus full
model/prompt/evidence provenance and invalidation.

### D17. FTS/vector seed selection

**Recommendation:** allow optional candidate selection only after exact search.
Traversal over typed graph edges remains the answer substrate.

### D18. Automatic ContextPipeline retrieval

**Recommendation:** keep on-demand default. Any automatic injection is a
feature-gated S9 experiment with token/correctness measurement.

### D19. CLI-backed provider access

**Recommendation:** later purpose-built, read-only, token/caller-bound
capability. Do not expose generic RPC or database access.

### D20. Multi-repository graphs

**Recommendation:** defer until single-repository value and isolation are
proven. Later federate authorized per-workspace DBs rather than creating an
unscoped global mutable graph.

### D21. Visualization/export breadth

**Recommendation:** native TUI first; bounded JSON/DOT diagnostics only if
needed. Defer broad exporters.

## Recommended approval response

An unambiguous approval could state:

> Approve D1-D5 as recommended and authorize planning/implementation of S0 and
> S1 only. Keep D6-D21 deferred to their stated gates.

Any approval narrower or different should name the changed decision. Until such
approval, the correct next state is research/design complete and implementation
stopped.
