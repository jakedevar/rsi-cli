# Codegraph storage and Rust-library decision records

Status: proposed decisions awaiting approval

Decision style: each record states recommendation, alternatives, consequences,
and a re-evaluation trigger.

## Stage contract

### Inputs

- Comparative findings `F-005` through `F-025`.
- Current `rsi.db`, `memory.sqlite`, daemon worker, tree-sitter, Cargo,
  ContextPipeline, worktree, and native-tool architecture.
- Official tree-sitter, Cargo metadata, rust-analyzer, SCIP, petgraph, SQLite,
  `leiden-rs`, and `graphrs` documentation.

### Process

- Evaluate alternatives against correctness, isolation, recoverability,
  bounded cost, dependency risk, and future evolution.
- Treat a separate derived DB as a hypothesis, not an assumption.
- Prefer existing Rust/runtime capabilities when they meet the contract.
- Defer choices whose value cannot be established before dogfood measurement.

### Outputs

- Proposed architecture decision records for persistence, extraction, semantic
  augmentation, traversal, community detection, federation, and summaries.

### Verify

- Every named storage and Rust implementation option is explicitly evaluated.
- Each recommendation includes costs and a revisit condition.
- No record authorizes dependency, schema, or configuration changes.

## ADR-CG-001: Split structural and durable federation persistence

**Status:** proposed

**Decision:** use one daemon-managed, rebuildable `codegraph.sqlite` per
effective workspace/worktree for structural and derived graph data. Store only
the workspace catalog and durable cross-domain links in main `rsi.db`. Store no
structural graph or durable link ledger in `memory.sqlite`.

### Alternatives

#### A. All tables in main `rsi.db`

Pros:

- native foreign keys to projects, sessions, observations, and entity cards;
- one backup and migration boundary;
- straightforward transactions for link endpoints.

Cons:

- high-churn, potentially large, rebuildable index shares the critical session
  lifecycle DB;
- worktree histories amplify write volume and retention;
- extractor schema/version churn forces primary-store migrations;
- reindex, compaction, and graph traversal contend with user-visible state.

**Disposition:** reject for structural state; accept for small durable catalog
and link tables.

#### B. Add graph tables to existing `memory.sqlite`

Pros:

- derived index lifecycle and operational patterns already exist;
- no new database family;
- FTS/vector seed lookup is nearby.

Cons:

- memory reindex/atomic swap may replace the file;
- memory is project-scoped, while code truth is effective-worktree and
  revision-scoped;
- chunk/vector and graph history cardinalities differ;
- durable federation links must survive a derived-memory rebuild;
- coupled schema and failure domains make both systems harder to reason about.

**Disposition:** reject. Later, query memory read-only for natural-language
seeds if authorized.

#### C. One global `codegraph.sqlite`

Pros:

- easier cross-repository query later;
- fewer files and pooled indexes.

Cons:

- a missing scope predicate becomes cross-project leakage;
- one writer/large WAL creates contention;
- sandbox cleanup and quota ownership become harder;
- corruption/rebuild impacts every project.

**Disposition:** reject for MVP. A future authorized catalog can coordinate
multiple per-workspace DBs without merging their storage.

#### D. One `codegraph.sqlite` per effective workspace

Pros:

- natural authorization and cleanup boundary;
- independent recovery/rebuild/migration;
- no stale canonical-project view leaking into a sandbox;
- smaller bounded read snapshots and writer contention;
- historical retention can be quota-scoped.

Cons:

- additional lifecycle and disk quota management;
- cross-DB durable links cannot use direct foreign keys;
- duplicate unchanged data across many sandboxes unless copy-on-write or
  deduplicated blobs are introduced later.

**Disposition:** recommend.

### Consequences

- `rsid` owns DB location, connections, lifecycle, and health.
- Structural DB loss degrades to “unavailable/rebuilding,” not lost durable
  knowledge.
- Link reads resolve structural identities through the manager and label
  unavailable versions explicitly.
- Main-DB changes require ordinary versioned migration approval in a future
  federation slice.

### Revisit when

- measured per-worktree disk duplication exceeds quota targets;
- SQLite writer/read latency fails the dogfood thresholds;
- authorized multi-repo queries become a primary use case.

## ADR-CG-002: Tree-sitter plus Cargo metadata for the MVP

**Status:** proposed

**Decision:** use Rust-native tree-sitter extraction for Rust/TOML/Markdown and
Cargo metadata for workspace/package/target/dependency truth.

### Alternatives

#### Tree-sitter

Pros:

- already a direct RSI dependency with relevant grammars;
- deterministic, fast, exact byte/point ranges, incremental parse support;
- no separate runtime or language server process.

Cons:

- syntax alone cannot fully resolve imports, method dispatch, macros, or types;
- highlight queries are not an index schema;
- grammar upgrades can change parse trees and identities.

**Disposition:** adopt with dedicated versioned extractors and explicit
ambiguous/unresolved outputs.

#### Cargo metadata

Pros:

- authoritative Cargo workspace/package/target/dependency model;
- available through RSI's pinned Rust toolchain/Cargo;
- complements manifest spans with resolved package identity.

Cons:

- process startup and full resolution may be expensive;
- features/target flags affect output;
- dirty/invalid manifests can fail.

**Disposition:** adopt. During implementation, benchmark direct
`cargo metadata --format-version 1` JSON invocation versus adding
`cargo_metadata`; select the smaller reliable boundary. Cache by relevant
manifest/lock/toolchain/config hashes and retain last good structure when an
edit temporarily breaks resolution.

#### Hand-written regex parsing

Pros: small initial code.

Cons: weak nesting/span/ambiguity correctness and high language drift.

**Disposition:** reject except for narrowly versioned rationale marker
recognition after tree-sitter/line structure has established the region.

### Consequences

- Initial calls are syntactic and honestly ambiguous where resolution is not
  provable.
- Cargo errors become visible health state, not partial silent graph mutation.
- No Python/JavaScript runtime is required.

### Revisit when

- golden questions show resolution recall below target;
- tree-sitter/Cargo build time exceeds thresholds;
- a stable compiler/semantic-index interface is demonstrably cheaper.

## ADR-CG-003: rust-analyzer and SCIP are optional precision layers

**Status:** proposed defer

**Decision:** define a versioned contribution importer compatible with future
SCIP/rust-analyzer output, but do not couple the first implementation slice or
MVP availability to them.

### rust-analyzer direct integration

Pros:

- high-fidelity Rust name/type/reference resolution;
- understands macros and crate semantics better than syntax-only extraction.

Cons:

- stable library embedding is difficult;
- process lifecycle, project loading, cancellation, memory, and version matching
  become daemon concerns;
- its public CLI does not promise SCIP generation as a stable general API.

**Disposition:** defer and spike on a temporary RSI copy after tree-sitter
baseline metrics.

### SCIP ingestion

Pros:

- language-neutral typed symbol/occurrence protocol;
- exact ranges/roles and potential future indexer interoperability;
- rust-analyzer indexer exists.

Cons:

- protobuf/schema/dependency and indexer lifecycle;
- does not define RSI snapshot, authorization, document rationale, Cargo
  ownership, evidence multiplicity, or federation;
- upstream Graphify's pinned importer is exploratory rather than a production
  model to reuse.

**Disposition:** defer; keep importer boundary.

### Consequences

- MVP answer correctness is scoped to facts it can prove.
- `Ambiguous`/`Unresolved` are product-visible, not embarrassing hidden states.
- Later semantic contributions can coexist with syntax evidence and supersede
  resolution without replacing stable nodes blindly.

### Revisit when

- measured golden failures are specifically resolvable by semantic indexing;
- setup/incremental latency and RSS fit daemon budgets;
- version/cancellation/output contracts are pinned.

## ADR-CG-004: Normalized SQLite persistence with hybrid bounded traversal

**Status:** proposed

**Decision:** use normalized SQLite as canonical state. Use indexed SQL and
bounded recursive CTEs for exact lookup and shallow traversal; materialize only
an authorized bounded induced graph in petgraph for shortest path and complex
impact algorithms.

### SQLite-only traversal

Pros:

- no graph duplication;
- strong scope/snapshot predicates and query plans;
- recursive CTEs handle many reachability questions.

Cons:

- complex path ranking/impact profiles become hard to audit;
- recursive work needs careful limits;
- equal-shortest-path enumeration and algorithm evolution are awkward.

**Disposition:** use for neighbors, one-hop explain, bounded subgraph expansion,
and candidate collection; do not force every algorithm into SQL.

### Whole-graph petgraph snapshots

Pros:

- ergonomic algorithms;
- predictable application-side control.

Cons:

- startup/memory cost proportional to repository;
- risks serving or mixing the wrong snapshot/scope;
- wasteful for small queries.

**Disposition:** reject per-request whole-graph loading.

### Bounded induced petgraph snapshots

Pros:

- algorithm ergonomics over an explicitly admitted node/edge budget;
- works with directed parallel-edge representations;
- keeps persistence independent.

Cons:

- boundary selection can affect path completeness;
- conversion cost and mapping tables require measurement.

**Disposition:** recommend for path/impact after SQL admission. If a complete
shortest path cannot be guaranteed within bounds, return bounded/truncated
status rather than “no path.”

### Consequences

- `petgraph` is an in-memory algorithm layer, never persistence.
- A direct dependency is not authorized until the implementation slice confirms
  the existing workspace version/API and benchmark.
- Query plans and cap enforcement are part of verification.

### Revisit when

- SQLite CTEs meet all algorithms more simply under benchmark;
- induced snapshot conversion dominates latency;
- repository scale requires a long-lived immutable read graph with strict
  snapshot/cache keys.

## ADR-CG-005: Defer community detection

**Status:** proposed defer

**Decision:** communities are not part of MVP correctness, agent answers, or
first-slice dependencies. Evaluate later as derived snapshot annotations.

### `leiden-rs`

Pros:

- pure Rust, petgraph adapter, Leiden quality functions.

Cons:

- comparatively young ecosystem/API;
- determinism and weighted/directed behavior need validation;
- adds value mainly for broad architecture exploration.

### `graphrs`

Pros:

- Leiden and Louvain included;
- multiedge-capable graph implementation.

Cons:

- introduces another graph model alongside petgraph;
- conversion and duplicate algorithm/persistence abstractions;
- must audit deterministic ordering and maintenance.

### Implement Louvain/Leiden internally

Pros: complete control.

Cons: high correctness/performance burden and unnecessary algorithm ownership.

**Disposition:** reject.

### Consequences

- Inspector groups initially use deterministic containment/crate/module
  structure.
- A later benchmark compares `leiden-rs` and `graphrs` on quality, determinism,
  CPU/RSS, API fit, licenses, and maintenance.
- Derived community rows always record snapshot and algorithm/config/version.

### Revisit when

- bounded subgraph/navigation tests show containment is insufficient;
- a concrete workflow/golden question benefits measurably.

## ADR-CG-006: Separate typed federation ledger; preserve Dreamer lineage

**Status:** proposed

**Decision:** retain `Observation.source_ids` as observation-to-observation
Dreamer lineage. Add cross-domain links later through a distinct typed ledger in
`rsi.db`.

### Generalize `source_ids`

Pros: no new table.

Cons:

- bare UUIDs cannot express target domain, relation, evidence, confidence,
  temporal validity, authorization, or deletion;
- breaks current observation-lineage semantics;
- code nodes and memory chunks have different lifecycle.

**Disposition:** reject.

### Put links in codegraph DB

Pros: structurally close.

Cons: links to decisions/observations are not rebuildable from source and would
be lost with the derived DB.

**Disposition:** reject for durable links. Derived cached projections may exist.

### Put links in memory DB

Pros: semantic adjacency.

Cons: memory DB is itself derived/rebuildable and chunk identities may change.

**Disposition:** reject.

### Main-DB typed ledger

Pros:

- durable audit/supersession;
- native project/session/observation/card ownership;
- survives graph and memory rebuilds.

Cons:

- polymorphic targets require manager validation;
- no direct FK into separate databases;
- adds a main-store migration in a later approved slice.

**Disposition:** recommend.

### Consequences

- candidate LLM links are excluded by default.
- target-missing is explicit; label/similarity never silently retargets.
- structural graph queries can omit federation entirely for pure correctness.

### Revisit when

- the ledger's cardinality becomes structural-index scale;
- a general RSI evidence graph emerges that warrants a broader domain-neutral
  link service.

## ADR-CG-007: On-demand retrieval and derived summaries

**Status:** proposed

**Decision:** default codegraph use to explicit native/RPC/workflow retrieval.
Do not unconditionally query it from ContextPipeline. Defer node summaries and
store them only as derived, provenance-bearing records when approved.

### Automatic ContextPipeline injection

Pros: agents always receive structure.

Cons:

- consumes tokens without knowing the task;
- may anchor the model to an irrelevant neighborhood;
- hides retrieval cost and undermines D2 measurement;
- risks stale context while an index is pending.

**Disposition:** reject as default; retain as a future feature-gated experiment.

### On-demand retrieval

Pros:

- task-specific bounds and snapshot;
- observable token/value measurement;
- agents can fall back to source when graph is insufficient.

Cons: agent must know and choose the tool.

**Disposition:** recommend, with compact tool descriptions and index health.

### Node summaries

Pros: concise orientation for large nodes/communities.

Cons:

- model/prompt drift, cost, hallucination, staleness, and invalidation;
- can obscure exact evidence.

**Disposition:** defer. If added, summary rows record node version, all input
evidence hashes, model/prompt/version, token count, validity, and generated
timestamp. Queries label them `derived_summary` and keep structure accessible.

### Revisit when

- dogfood tool usage shows discoverability problems;
- controlled experiments show automatic retrieval improves correctness per
  token;
- summary invalidation and model governance are established.

## Decision summary

| Topic | Recommendation | MVP |
|---|---|---|
| Canonical structure | per-effective-workspace normalized SQLite | yes |
| Durable federation | typed link ledger in main `rsi.db` | later slice |
| `memory.sqlite` | seed lookup only; no graph ownership | no structural writes |
| Syntax extraction | tree-sitter Rust/TOML/Markdown | yes |
| Workspace/dependencies | Cargo metadata plus manifest spans | yes |
| rust-analyzer | optional measured precision source | no |
| SCIP | optional versioned importer | no |
| Traversal | bounded SQL + bounded petgraph induced snapshot | yes, as needed |
| Communities | benchmark `leiden-rs`/`graphrs` later | no |
| Node summaries | derived/provenance-bearing later | no |
| Context | on-demand retrieval | yes |
| Multi-repo | explicit authorized federation later | no |
