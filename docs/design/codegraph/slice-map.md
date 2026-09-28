# Codegraph dependency-ordered slice map

Status: proposed plan; no slice is authorized for implementation

Traceability source:
`docs/design/codegraph/2026-07-30-comparative-research.json`

## Stage contract

### Inputs

- Stable research findings `F-001` through `F-027`.
- North-star specification, typed schema, interface contracts, ADRs, benchmark
  specification, and risk register in this directory.
- Existing D5 benchmark gate and RSI stage-contract/provenance rules.

### Process

- Order slices by prerequisites and measurable risk retirement.
- Keep each slice reviewable, independently verifiable, and merge-safe.
- Declare `satisfies: [F-...]` on every item.
- Put deferred enrichments behind measured expansion gates.

### Outputs

- Dependency graph and slice contracts.
- Full finding-to-slice coverage.
- One small recommended first implementation slice.
- Explicit stop/approval boundary.

### Verify

- Every `F-###` appears in at least one `satisfies` declaration.
- No slice depends on a later slice.
- Production schema/dependency choices remain approval-gated.
- The first slice excludes federation writes, automatic prompt injection,
  communities, summaries, multi-repository support, Python, and JavaScript.

## Dependency graph

```text
S0 benchmark + fixture contract
  |
  v
S1 scope/identity/schema spike
  |
  v
S2 manager/worker + transactional index foundation
  |
  v
S3 Rust/Cargo/Markdown extraction
  |
  v
S4 bounded query core
  |
  +-------------------+
  v                   v
S5 native tools      S6 TUI inspector
  \                   /
   +--------+---------+
            v
        S7 dogfood gate
            |
            +-----------> STOP / value review
            |
            +-----------> S13 rsi-graph retrieval adapter
            |
            v
        S8 federation
            |
      +-----+-------+
      v             v
 S9 derived       S10 semantic
 enrichments      augmentation
      \             /
       +-----+-----+
             v
        S11 CLI access
             |
             v
        S12 multi-repo
```

S0 is implementation-adjacent verification infrastructure, not a production
feature. S1 is the recommended first production implementation slice after
human decisions and approval.

## S0 — Safe benchmark fixtures and baselines

**satisfies:** [F-001, F-015, F-027]

### Inputs

- Pinned Graphify commit and temporary-copy rule.
- Golden questions, micro-fixtures, scoring rubric, and metric schema.
- Existing D2 measurement requirement.

### Process

- Add fixture corpus and expected graph facts without production index code.
- Build a harness that refuses the live workspace and records corpus/tool
  identities.
- Establish `rg` and raw-inspection baselines; define pinned Graphify runner as
  isolated/optional.

### Outputs

- Reviewable fixtures/expectations.
- Safe benchmark runner and result schema.
- Baseline correctness/token/time results.

### Verify

- Live-root guard test.
- Fixture expectations reviewed for direction, evidence, ambiguity, and
  parallel relations.
- Graphify pin/version recorded exactly.

**Dependencies:** none.

**Gate:** benchmark infrastructure itself must not require Python for RSI build
or runtime; Python is isolated to the optional upstream comparison.

## S1 — Scope, identity, and read-only structural skeleton

**satisfies:** [F-003, F-004, F-006, F-007, F-016, F-017]

### Inputs

- Approved per-workspace storage decision.
- Stable ID, snapshot, exact-span, node/edge/evidence schema subset.
- RSI project/effective-worktree resolution and one Rust micro-fixture.

### Process

- Introduce typed common structural IDs/enums and a derived DB locator owned by
  rsid.
- Create a per-effective-workspace SQLite DB with repository/workspace,
  complete snapshot, file/version, node/version, edge/version, and evidence
  tables.
- Index a deliberately tiny deterministic Rust subset from an explicit operator
  invocation: files/modules plus one or two declaration kinds.
- Expose operator status and an internal exact-node inspection path only.

### Outputs

- One worktree-scoped complete snapshot with exact source evidence.
- Stable IDs across two identical builds and copied roots.
- No watcher, native tool, federation, history UI, community, summary, or
  semantic resolver.

### Verify

- DB migration/rebuild and integrity tests.
- copied-root/worktree identity tests;
- exact byte/line/hash tests;
- parallel evidence representation test;
- scope mismatch denial;
- identical-build determinism.

**Dependencies:** S0.

**Why first:** it retires the hardest-to-reverse decisions—scope, identity,
evidence, and storage—through a small end-to-end artifact without promising the
full product.

## S2 — Manager/worker, incremental transaction, and health

**satisfies:** [F-004, F-008, F-015, F-016, F-025]

### Inputs

- S1 schema/identity.
- Memory worker/sync operational patterns.
- watcher, cancellation, queue, health, recovery, and retention contracts.

### Process

- Add daemon manager and per-workspace bounded worker.
- Inventory/hash inputs, debounce/coalesce events, stage contributions, and
  atomically replace changed/deleted files.
- Add cancel, full reconcile, last-complete recovery, and compact events.
- Expose operator status/index/cancel RPCs.

### Outputs

- Safe incremental index lifecycle.
- Observable queue/snapshot/health state.
- Crash/restart/full-reconcile behavior.

### Verify

- one-file update/delete and zero stale-fact tests;
- event storm/overflow/backpressure;
- cancellation at each phase;
- fault injection around transaction/commit/event;
- cross-worktree concurrent indexing;
- bounded CPU/RSS/queue.

**Dependencies:** S1.

## S3 — Dogfood Rust, Cargo/TOML, and Markdown extraction

**satisfies:** [F-005, F-007, F-008, F-010, F-017, F-018]

### Inputs

- S2 contribution transaction.
- Rust/TOML/Markdown vocabulary and fixtures.
- Cargo metadata invocation/dependency decision.
- Exact rationale/research/plan marker grammar.

### Process

- Implement requested Rust declarations/containment/imports/syntactic
  calls/impls/tests/docs.
- Add Cargo workspace/crate/target/dependency/feature extraction and TOML spans.
- Add Markdown headings/links/rationale markers and stable `F-###`/plan
  provenance.
- Preserve unresolved/ambiguous targets; record producer/rule versions.

### Outputs

- Full MVP dogfood corpus graph.
- Coverage/unsupported-input health report.
- Versioned extraction rules.

### Verify

- per-kind golden fixtures and exact spans;
- Cargo broken/repair behavior;
- ambiguous/resolved transitions;
- deleted cross-file target cleanup;
- no LLM producer in structural tables;
- no Python/JavaScript runtime.

**Dependencies:** S2.

## S4 — Bounded query and traversal core

**satisfies:** [F-006, F-007, F-009, F-015, F-019, F-020, F-024]

### Inputs

- S3 typed graph.
- Query envelopes and bounds.
- SQL/petgraph ADR and golden questions.

### Process

- Add exact/prefix/optional-FTS seed search and exact node get.
- Add explain, incoming/outgoing neighbors, path, bounded subgraph, affected
  profiles, and graph diff.
- Use SQL admission/shallow traversal and bounded induced petgraph graphs where
  measured appropriate.
- Centralize deterministic ordering, evidence rendering, continuation, and
  token budgets.

### Outputs

- Common request/response types and operator read RPCs.
- Complete/partial/unknown semantics and query metrics.

### Verify

- all golden architecture/impact questions;
- direction, ambiguity, parallel-edge/evidence, and diff cases;
- SQL plan/recursive bounds;
- malicious size/depth/time/token requests;
- no whole-graph per-query allocation;
- stable canonical results.

**Dependencies:** S3.

**Explicit exclusion:** communities are not used in answers.

## S5 — Scope-bound Harness and CodexAppServer tools

**satisfies:** [F-009, F-022, F-024, F-027]

### Inputs

- S4 manager queries.
- Existing native registry/caller/project-binding patterns.
- Proposed three-tool schemas and authorization tests.

### Process

- Register read-only search, explain, and traverse tools for both native
  providers.
- Capture effective workspace and manager handle server-side.
- Render identical bounded structured results.

### Outputs

- Native on-demand codegraph retrieval.
- Compact capability/index-health description.

### Verify

- schema snapshots have no scope/write fields;
- Harness/CodexAppServer parity;
- cross-project/worktree and forged snapshot/node denial;
- rotation/sandbox behavior;
- token/output bounds and source-location correctness.

**Dependencies:** S4.

## S6 — Dense TUI inspector and index status

**satisfies:** [F-012, F-025, F-026]

### Inputs

- S2 lifecycle/status RPCs and S4 read RPCs.
- Existing dense session/recursive-DAG/memory overlay patterns.
- Approved command/keybinding.

### Process

- Add `:codegraph`, navigator/inspector/relations/evidence/diff panes, health
  header, snapshot chooser, and operator reconcile/cancel.
- Resolve clickable paths against effective workspace.
- Suppress stale async results by request generation.

### Outputs

- Keyboard-dense codegraph inspection and status surface.
- Keybinding documentation if a binding is approved.

### Verify

- ratatui snapshot/state tests;
- dirty/pending/degraded health display;
- stale response suppression;
- correct sandbox source opening;
- bounded copy/export.

**Dependencies:** S2 and S4.

## S7 — RSI dogfood benchmark and go/no-go gate

**satisfies:** [F-001, F-009, F-012, F-017, F-018, F-019, F-020, F-024, F-027]

### Inputs

- S0 baselines and S3-S6 implementation.
- Temporary RSI corpus copies and golden rubric.

### Process

- Run full/incremental/query/recovery/isolation/bounds benchmarks.
- Compare `rg`, raw inspection, RSI codegraph, and pinned Graphify.
- Measure agent correctness per output/source-read token with on-demand
  retrieval.

### Outputs

- Versioned benchmark artifact and pass/fail report.
- Explicit recommendation: stop, revise foundation, or proceed.

### Verify

- all MVP thresholds;
- no live worktree benchmark;
- reproducible environment and raw metrics;
- unsupported upstream operations recorded honestly.

**Dependencies:** S0, S3, S4, S5, optionally S6 if approved for first program.

**Program stop:** do not proceed to S8+ without a human value review.

## S8 — Durable episodic federation

**satisfies:** [F-002, F-003, F-010, F-013, F-016, F-021, F-022]

### Inputs

- Successful S7 gate.
- Approved main-DB link-ledger decision and migration plan.
- Observation/card/chunk/session/research/plan/commit target contracts.

### Process

- Add workspace catalog/link/evidence tables to main `rsi.db` through a
  versioned migration.
- Keep Dreamer `source_ids` unchanged.
- Support deterministic/manual links first; optionally store LLM suggestions as
  excluded-by-default candidates.
- Enforce both-endpoint authorization, validity, supersession, and
  target-missing semantics.

### Outputs

- Governed federation queries and inspector/tool opt-in results.
- Research `F-###` to plan/commit/code traceability.

### Verify

- migration/rollback fixtures;
- all target kinds and relation semantics;
- cross-project pivot denial;
- graph rebuild/node rename/memory reindex/target delete behavior;
- structural integrity rejects LLM facts.

**Dependencies:** S7 and new human approval.

## S9 — Derived summaries, seed retrieval, communities, and export

**satisfies:** [F-010, F-011, F-012, F-013, F-019, F-020, F-024, F-027]

### Inputs

- Measured S7/S8 demand.
- Provenance/invalidation schema.
- Separate experiments for FTS/vector seeds, summaries, communities, and
  diagnostic export.

### Process

- Slice each feature independently; do not bundle.
- Benchmark `leiden-rs` versus `graphrs` if communities are justified.
- Keep FTS/vector as candidate selection only.
- Store summaries/communities as derived snapshot records.

### Outputs

- Only measured enrichments that pass individual gates.

### Verify

- deterministic/invalidation/correctness-per-token tests;
- no effect on structural answer substrate;
- export path/content security.

**Dependencies:** S7; federation-specific enrichment also depends on S8.

## S10 — Optional rust-analyzer/SCIP precision augmentation

**satisfies:** [F-005, F-007, F-015, F-018, F-027]

### Inputs

- S7 golden failures attributable to missing semantic resolution.
- Temporary-copy rust-analyzer/SCIP spike results.
- Versioned contribution importer.

### Process

- Compare direct rust-analyzer process integration and SCIP ingestion.
- Add the winner only as optional evidence/resolution contributions.
- Retain tree-sitter/Cargo graph when augmentation is absent/fails.

### Outputs

- Measured higher-precision resolution with explicit producer/version.

### Verify

- setup/full/incremental/RSS/cancellation;
- golden precision/recall delta;
- version mismatch and missing binary behavior;
- ambiguity supersession and evidence coexistence.

**Dependencies:** S7 and separate approval.

## S11 — Safe CLI-backed provider reads

**satisfies:** [F-004, F-009, F-022, F-024]

### Inputs

- Proven native query contract.
- Agent-token/allowlist/security design.

### Process

- Add a purpose-built read-only, caller-bound capability.
- Update discovery/allowlist deliberately.
- Exclude arbitrary project/workspace/root and federation pivots.

### Outputs

- Bounded self-workspace codegraph reads for approved CLI providers.

### Verify

- token lifecycle, rotation, restart, scope, malicious input, discovery surface,
  and audit/redaction tests.

**Dependencies:** S7, and S8 only if CLI federation reads are included.

**Gate:** separate security approval.

## S12 — Authorized multi-repository federation

**satisfies:** [F-014, F-015, F-016, F-019, F-027]

### Inputs

- Proven single-repository identity/isolation.
- Concrete cross-repository use cases and authorization set.
- Cross-repository evidence/import semantics.

### Process

- Query multiple per-workspace DBs through manager federation.
- Add repository-qualified identities and explicitly evidenced cross-repo edges.
- Keep deny-by-default scope and bounded fan-out.

### Outputs

- Authorized multi-repository search/path/impact without a global mutable graph.

### Verify

- repository collision, fan-out/backpressure, missing repo, mixed revision,
  authorization, and determinism tests.

**Dependencies:** S7; typically S8; separate approval.

## Finding coverage

| Finding | Covered by |
|---|---|
| F-001 | S0, S7 |
| F-002 | S8 |
| F-003 | S1, S8 |
| F-004 | S1, S2, S11 |
| F-005 | S3, S10 |
| F-006 | S1, S4 |
| F-007 | S1, S3, S4, S10 |
| F-008 | S2, S3 |
| F-009 | S4, S5, S7, S11 |
| F-010 | S3, S8, S9 |
| F-011 | S9 |
| F-012 | S6, S7, S9 |
| F-013 | S8, S9 |
| F-014 | S12 |
| F-015 | S0, S2, S4, S10, S12 |
| F-016 | S1, S2, S8, S12 |
| F-017 | S1, S3, S7 |
| F-018 | S3, S7, S10 |
| F-019 | S4, S7, S9, S12 |
| F-020 | S4, S7, S9 |
| F-021 | S8 |
| F-022 | S5, S8, S11 |
| F-023 | S13 below |
| F-024 | S4, S5, S7, S9, S11, S13 |
| F-025 | S2, S6 |
| F-026 | S6 |
| F-027 | S0, S5, S7, S9, S10, S12 |

## S13 — Explicit rsi-graph retrieval adapter

**satisfies:** [F-023, F-024]

### Inputs

- Proven S4 manager query and S7 value.
- `ContextSourceType::FileIndex`, `RetrievalBackend`, and stage-contract static
  input needs.

### Process

- Register an authorized codegraph file-index retrieval backend.
- Extend retrieval metadata only as needed for scope, snapshot, evidence, and
  bounds.
- Allow explicit workflow-stage retrieval; keep ordinary ContextPipeline
  on-demand.

### Outputs

- Codegraph retrieval consumable by workflows without reusing workflow graph
  node/edge types.

### Verify

- explicit registration and scope tests;
- bounded static-stage input;
- no default prompt injection;
- workflow and code entity type separation.

**Dependencies:** S7.

**Reason placed separately:** native value should be proven before modifying
the dormant workflow retrieval seam.

## Recommended first implementation slice

After S0 verification scaffolding and explicit human approval, implement **S1
only**.

Recommended S1 scope:

- per-effective-worktree derived SQLite database;
- strong IDs and typed repository/workspace/snapshot/file/module/function/
  evidence subset;
- exact Rust spans and content hashes;
- one explicit operator index command and status;
- deterministic rebuild and scope-denial tests.

Do not include:

- watcher/incremental mutation;
- Cargo or Markdown breadth;
- graph traversal beyond internal exact inspection;
- native agent tools or TUI;
- main `rsi.db` federation tables;
- memory integration;
- ContextPipeline/rsi-graph integration;
- rust-analyzer/SCIP/petgraph/community/summaries/export/multi-repo.

This slice is small enough to discard if identity/storage evidence fails, while
large enough to test the architectural foundation in real Rust.

## Approval boundary

No slice may begin from this document alone. Resolve the decisions in
`human-decisions.md`, obtain explicit implementation approval, and then create
or iterate an implementation plan under the repository's normal workflow.
