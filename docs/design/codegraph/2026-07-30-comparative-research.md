---
date: 2026-07-30
status: complete
research_schema_version: 2
json_companion: docs/design/codegraph/2026-07-30-comparative-research.json
upstream_repository: https://github.com/Graphify-Labs/graphify
upstream_pin: 2fa6cd3d5548577f8c5f591b713f0bf80c1af183
upstream_pin_version: 0.9.25
---

# Codegraph comparative research

## Executive finding

RSI should build codegraph as a third knowledge domain, not as an extension of
the workflow graph and not as another memory index. The strongest Graphify
ideas are deterministic tree-sitter extraction, compact graph-grounded query
results, content-hashed incremental work, rationale/document ingestion, and
explicit provenance. RSI should redesign Graphify's file persistence,
identity, evidence, revision, isolation, and bounded-work behavior around
transactional SQLite and server-bound workspace scope.

The recommended canonical structural store is a separate, rebuildable
`codegraph.sqlite` for each effective workspace/worktree. A small durable
federation ledger belongs in the main `rsi.db`; `memory.sqlite` remains the
derived memory index and is not a codegraph store. Tree-sitter plus Cargo
metadata is the MVP extraction stack. rust-analyzer/SCIP, community detection,
LLM node summaries, broad export, and multi-repository federation are later
measured additions.

## Stage contract

### Inputs

- Static RSI inputs:
  `thoughts/shared/orchestration/2026-07-02-pending-slices-board.md`,
  `thoughts/ideas-intake.md`,
  `thoughts/shared/plans/2026-05-11-project-scoped-agent-memory-retrieval.md`,
  `docs/memory-architecture.md`, `crates/rsi-graph/`,
  `crates/rsid/src/memory/`, session launch/rotation code, RPC types, native
  tool registries, ContextPipeline, and TUI overlays.
- Historical inputs: git log/blame for the D5 idea, memory implementation, and
  relevant planning/verification artifacts.
- Comparative input: Graphify repository pinned to
  `2fa6cd3d5548577f8c5f591b713f0bf80c1af183` (`v0.9.25`, branch `v8`).
- A bounded code-discovery budget covering the named modules and upstream
  build/query/extractor/watcher/cache/export/community/learning modules.
- Primary Rust/SQLite documentation linked in “Implementation choices.”

### Process

- Search current code and artifacts before defining a new initiative.
- Inspect the pinned Graphify source rather than relying on product copy.
- Classify each requested upstream capability as adopt, redesign, defer, or
  reject.
- Record later upstream revisions only as post-pin evidence.
- Convert observations into stable findings usable by downstream plans.

### Outputs

- Current-truth audit.
- Pinned comparative capability matrix.
- Storage and implementation-choice evaluation.
- Stable `F-###` findings and a schema-v2 JSON companion.

### Verify

- Pin identity and version can be reproduced with
  `git show 2fa6cd3d5548577f8c5f591b713f0bf80c1af183:pyproject.toml`.
- Every comparative classification cites source evidence.
- The sidecar passes `rsi-research-validate --strict --schema-version 2`.
- Downstream slice items declare `satisfies: [F-...]`.

## Current truth in RSI

### Existing overlap: iterate D5

The only directly overlapping proposal is D5 on the pending-slices board:
“Code-graph (Gable-like),” sketched as rust-analyzer SCIP to SQLite to an RPC
query verb, with worker-contract integration and a D2 before/after measurement
gate. The board calls it a prototype spike rather than a sliced active plan
(`thoughts/shared/orchestration/2026-07-02-pending-slices-board.md:324-327`).
The ideas-intake plan excludes it from small slices because it needs its own
program. No codegraph implementation, schema migration, dependency, handoff, or
verification manifest exists.

This design therefore **iterates D5**. It retains SQLite, measurement, and
agent-facing retrieval, while revising SCIP-from-day-one into a measured
tree-sitter/Cargo foundation with optional SCIP enrichment. The older
`thoughts/ideas-intake.md` “Gable AI integration” resolution should be
reconciled as superseded terminology if implementation is approved.

### The May 11 memory plan is implemented history

The project-scoped memory plan is not active. Its frontmatter says
`status: implemented`, and its implementation note says all nine phases are
complete
(`thoughts/shared/plans/2026-05-11-project-scoped-agent-memory-retrieval.md:2-7`).
The current architecture confirms daemon ownership, a separate
`memory.sqlite`, incremental synchronization, hybrid FTS/vector retrieval,
observations, Dreamer derivations, and native tools
(`docs/memory-architecture.md:3-16`, `docs/memory-architecture.md:143-203`).

Codegraph may reuse its operational patterns—single-writer ownership, bounded
commands, watcher debounce, content hashes, atomic replacement, status
events—but must not turn structural graph traversal into memory retrieval.

### The three-domain boundary

| Domain | Canonical truth | Typical entities | Query semantics |
|---|---|---|---|
| rsi-graph | Workflow definitions and executions | workflow nodes, task edges, recursive DAG state | orchestration, scheduling, handoff |
| RSI memory | Episodic/semantic records and derived indexes | transcripts, chunks, observations, cards, embeddings | search, recall, dialectic context |
| codegraph | Source and manifest structure at an observed workspace revision | files, modules, symbols, calls, imports, dependencies, evidence | exact structure, paths, impact, revision diff |

`rsi-graph` already defines conceptual `ContextSourceType::FileIndex` and a
`RetrievalBackend` interface (`crates/rsi-graph/src/context.rs:8-30`,
`crates/rsi-graph/src/retrieval.rs:37-70`), but production code does not
register a file-index backend. These are future integration seams, not evidence
that workflow graph nodes are code nodes.

### Workspace truth is effective-working-directory scoped

RSI resolves a canonical project root, may allocate a sandbox worktree, and
launches the provider in the effective worktree root while retaining the
canonical `Session.working_dir` and optional `sandbox_root` separately
(`crates/rsid/src/session/launch.rs:490-584`,
`crates/rsid/src/session/launch.rs:941-1024`). Rotation reuses a valid sandbox
instead of silently falling back to project-wide state
(`crates/rsid/src/session/rotation.rs:1283-1344`).

Codegraph scope must therefore be:

```text
project authorization
  -> repository identity
    -> effective workspace/worktree identity
      -> observed revision (HEAD plus dirty-content digest)
```

A project-wide graph without worktree identity would leak stale or parallel
agent edits into another session.

## Pinned Graphify baseline

The comparison below is pinned to
[`2fa6cd3d...`](https://github.com/Graphify-Labs/graphify/tree/2fa6cd3d5548577f8c5f591b713f0bf80c1af183),
whose `pyproject.toml` reports `0.9.25`. The upstream is Apache-2.0/MIT dual
licensed at that revision. All source links in this section include the exact
commit.

Graphify builds NetworkX graphs from deterministic language extractors and
LLM-assisted document/media extraction. Its own architecture document says
code parsing is deterministic tree-sitter work, document extraction can use an
LLM, communities use Leiden/Louvain, and no embeddings are required for graph
traversal ([how-it-works.md](https://github.com/Graphify-Labs/graphify/blob/2fa6cd3d5548577f8c5f591b713f0bf80c1af183/docs/how-it-works.md)).

The public representation is largely dictionary-shaped nodes/edges stored in
`graph.json`; hyperedges live as graph metadata. Its Rust extractor captures a
useful symbol/relation set, but locations are line strings rather than exact
byte spans
([rust.py](https://github.com/Graphify-Labs/graphify/blob/2fa6cd3d5548577f8c5f591b713f0bf80c1af183/graphify/extractors/rust.py)).

### Adopt/redesign/defer/reject matrix

| Capability | Decision | Pinned evidence | RSI-native treatment |
|---|---|---|---|
| Deterministic tree-sitter extraction | **Adopt** | Code extractors parse supported languages deterministically; Rust covers types, impls, imports, calls, and tests. | Use pinned workspace Rust grammars; record exact half-open byte ranges, 1-based display spans, file hashes, grammar/extractor/rule versions. |
| Typed nodes and directed relation edges | **Redesign** | Nodes and edges are flexible dictionaries in NetworkX/JSON. | Define versioned Rust enums and normalized tables; reject unknown kinds at persistence boundaries; direction is part of the contract. |
| Parallel edges and multiple evidence records | **Redesign** | `build.py` can use a multigraph, but its default graph and edge dedupe can collapse same `(source,target,relation)` facts. | Preserve distinct edge occurrences and attach one-to-many evidence records; provide aggregate views rather than destructive dedupe. |
| Hyperedges | **Adopt concept; redesign storage** | Hyperedges are stored under graph metadata rather than first-class typed records. | First-class hyperedge plus ordered/role-labelled members, validity, evidence, and producer version. Initially use for Rust impl/trait/type and multi-party dependency facts only where a binary edge loses meaning. |
| Communities | **Defer** | Deterministic Leiden with Louvain fallback is used for clustering. | Exclude from MVP answer correctness. Benchmark pure-Rust Leiden/Louvain later; persist as derived, invalidatable annotations, never source truth. |
| `EXTRACTED`/`INFERRED`/`AMBIGUOUS` and confidence | **Adopt; tighten semantics** | Graphify documents the three tags and confidence rubric. | Retain enum plus `0.0..=1.0`; structural `INFERRED` means deterministic resolution only. LLM output cannot enter structural tables. Record producer, rule, version, and evidence. |
| Content-hashed incremental rebuilds | **Adopt; redesign replacement** | Cache hashes inputs; merge mode replaces contributions and prunes absent files. | Hash content plus normalized path and extractor inputs. Parse into staging, atomically replace all contributions of changed/deleted files in one transaction, and commit snapshot/health last. |
| Exact query/search | **Adopt** | CLI searches node labels and attributes. | Typed exact/prefix search with optional FTS seed selection, stable sorting, scope binding, exact locations, and hard count/time/token caps. |
| Explain | **Adopt; redesign** | Explain selects a match and summarizes connections/learning. | Require exact node resolution or return candidates; include provenance/evidence and why each edge exists; never silently choose ambiguous labels. |
| Incoming/outgoing neighbors | **Adopt** | Query traversal supports graph walks but direction handling varies by command. | Direction is explicit: incoming, outgoing, or both. Cap depth, nodes, edges, wall time, and serialized tokens. |
| Shortest path | **Adopt; redesign** | Path loads the graph, converts as needed, and returns relation-labelled paths. | Deterministic tie-breaks, directed by default, explicit relation filter, bounded induced snapshot, truncation metadata, and exact edge evidence. |
| Bounded subgraph | **Adopt; redesign** | Query has a token budget and depth defaults. | Enforce depth/result/edge/time/token limits at the manager and worker; report applied caps and truncation cause in every response. |
| Affected/impact | **Adopt; redesign** | Reverse traversal uses a relation allowlist and depth default. | Versioned impact profiles, direction-aware traversal, confidence floor, deterministic ordering, test/dependency categories, and evidence on every hop. |
| Graph diff | **Adopt; redesign** | Diff compares node IDs and `(source,target,relation)` triples. | Compare typed entity/edge versions, evidence, locations, attributes, and snapshot validity; distinguish moved, modified, added, removed, resolved, and newly ambiguous. |
| Token-bounded agent output | **Adopt universally** | Query rendering approximates tokens and truncates to a requested budget. Some commands lack equivalent bounds. | Put `max_output_tokens` in every native query contract; enforce exact serializer budgets centrally and return continuation/cut information. |
| Document/rationale extraction | **Adopt selectively** | Markdown headings/references are deterministic; rationale workflows may use LLM extraction. | Parse Markdown, rustdoc, plan/research IDs, ADR/rationale markers deterministically. Put LLM-derived rationale links in the federation layer with evidence and confidence. |
| Node summaries | **Defer** | Node summaries are an RFC, not a complete pinned capability. | Derived table only, with source snapshot, model/prompt/version, evidence set, content hash, and invalidation. No summaries in MVP correctness path. |
| Visualization and export | **Defer breadth; adopt inspector** | Pinned exporters cover interactive HTML, GraphML, Neo4j, Obsidian, SVG, and others. | MVP gets dense ratatui inspection and bounded JSON/DOT diagnostic export on temporary/output paths. Broader exporters wait for demand and security review. |
| Work-memory/learning overlays | **Redesign as federation** | `.graphify_learning.json` is explicitly separate from the structural graph. | Preserve that separation, but use durable typed cross-domain links with project/workspace authorization, temporal validity, provenance, confidence, and deletion semantics. |
| Multi-repository graphs | **Defer; redesign** | Global graph prefixes IDs and merges per-repository graphs into a user-level JSON file. | No MVP global graph. Later use explicit repository namespaces, server-authorized sets, per-repo snapshots, cross-repo edge evidence, and deny-by-default query scope. |

### Pinned implementation observations

1. **Identity is string-convention dependent.** The ID helper normalizes
   label-like strings and requires extractors, build, and LLM paths to agree
   ([ids.py](https://github.com/Graphify-Labs/graphify/blob/2fa6cd3d5548577f8c5f591b713f0bf80c1af183/graphify/ids.py)).
   RSI needs explicit repository namespaces and stable identity/version
   separation.
2. **Canonical JSON makes replacement recoverable but not transactional.**
   Graphify includes safe/atomic export mechanics, yet a build still reasons
   over a whole JSON graph and cache sidecars. SQLite staging plus one commit
   narrows the crash window and makes deleted-file contribution removal
   testable.
3. **Incremental/watch work is not sufficiently bounded for a daemon.** The
   watcher debounces events but collects paths in an unbounded set and may
   trigger broad rebuilds
   ([watch.py](https://github.com/Graphify-Labs/graphify/blob/2fa6cd3d5548577f8c5f591b713f0bf80c1af183/graphify/watch.py)).
   RSI requires bounded queues, coalescing, cancellation, backpressure,
   observable health, and full-reconcile fallback.
4. **SCIP ingestion is exploratory at the pin.** The module describes a
   simplified JSON path rather than a fully wired protobuf ingestion surface
   ([scip_ingest.py](https://github.com/Graphify-Labs/graphify/blob/2fa6cd3d5548577f8c5f591b713f0bf80c1af183/graphify/scip_ingest.py)).
   It is evidence to evaluate SCIP, not a ready architecture to copy.
5. **Learning is correctly non-structural.** The reflection subsystem says its
   learning sidecar does not modify the structural graph
   ([reflect.py](https://github.com/Graphify-Labs/graphify/blob/2fa6cd3d5548577f8c5f591b713f0bf80c1af183/graphify/reflect.py)).
   RSI should keep this invariant while replacing an untyped sidecar with a
   governed link ledger.

## Later upstream revision, recorded separately

The `v8` branch was observed at
[`4fe11092...`](https://github.com/Graphify-Labs/graphify/commit/4fe11092ccbe9f543608f140c790f68d5d83cae4)
(`v0.9.31`, 2026-07-30). It is **not** part of the pinned comparison.

Post-pin release history repeatedly fixes incremental merge/pruning,
community staleness, absolute-path identity leakage, cached IDs from old roots,
multi-project context bounds, relationship direction, and ambiguous explain
selection. That later history strengthens the case for first-class invariants:
root-independent identity, atomic per-file replacement, explicit direction,
scope-bound queries, and ambiguity-preserving results. It should not be cited
as functionality present in v0.9.25.

Representative post-pin evidence:

| Later commit | Change | Design signal |
|---|---|---|
| [`137dcf23`](https://github.com/Graphify-Labs/graphify/commit/137dcf23fe0aa68954f299aa61c3559994d67603) | incremental `--no-cluster` merges instead of overwriting | replacement semantics need regression tests |
| [`2f78439f`](https://github.com/Graphify-Labs/graphify/commit/2f78439ffd526bb05dd3d3c58eb237e8f916a527) | stop pruning live files as deleted | deletion/current-view invariants must be transactional |
| [`f6711336`](https://github.com/Graphify-Labs/graphify/commit/f67113361b99500f2cbd36e36ea6e7a60ba74da3) | close absolute-path node-ID leaks | stable IDs must exclude workspace roots |
| [`c5d43271`](https://github.com/Graphify-Labs/graphify/commit/c5d432710f169f786f756c78719dcfc159eedef8) | re-anchor cached IDs from another root | every cache key must include authorized workspace identity |
| [`b4865ffc`](https://github.com/Graphify-Labs/graphify/commit/b4865ffcbfc91161d861eef7dc412030400d2032) | bound multi-project graph contexts | query scope and fan-out are security/performance inputs |
| [`2ca565ac`](https://github.com/Graphify-Labs/graphify/commit/2ca565ac1ad21d6647939943bfd7b1d186f6e0ee) / [`50be9dcd`](https://github.com/Graphify-Labs/graphify/commit/50be9dcd364c90851c6f49868ae473b498e10b65) | preserve ambiguous explain results and edge direction | ambiguity and direction belong in typed result contracts |

## Storage option audit

### Option 1: main `rsi.db`

**Strengths:** durable daemon-owned identity, projects/sessions/observations/
cards already live there, one backup boundary, and true foreign keys for
durable federation links.

**Weaknesses:** structural indexing is high-churn and rebuildable; worktree
graphs can be large; parser evolution would burden the primary session store;
reindex and historical-retention writes would contend with lifecycle traffic.

**Decision:** reject as the canonical structural graph. Use it only for a small
workspace catalog and durable cross-domain link ledger whose loss cannot be
reconstructed from source.

### Option 2: existing `memory.sqlite`

**Strengths:** already derived, project-scoped, daemon-owned, WAL-backed, and
operated through a sequential worker. It demonstrates content hashes, bounded
commands, and atomic reindex replacement.

**Weaknesses:** its schema and lifecycle serve chunks/FTS/vectors; full reindex
can swap the database; structural history and worktree isolation have different
cardinality and retention; durable semantic links would be lost or orphaned by
a derived-memory rebuild.

**Decision:** reject for structural state and durable federation links. Permit
read-only use as a natural-language seed source later.

### Option 3: separate rebuildable `codegraph.sqlite`

**Strengths:** independent migration/version lifecycle, transactional normalized
structure, atomic contribution replacement, per-worktree authorization and
cleanup, bounded graph snapshots, and safe delete/rebuild recovery.

**Weaknesses:** another database to own, backup/cleanup/health complexity,
cross-database references cannot use SQLite foreign keys, and per-worktree
instances need quotas.

**Decision:** recommend one derived DB per effective workspace/worktree,
managed by rsid. Keep durable cross-domain links in `rsi.db`, and validate
external target existence through the manager rather than pretending
cross-database foreign keys exist.

## Rust implementation choices

### Tree-sitter

RSI already directly depends on tree-sitter and Rust/TOML/Markdown grammars for
TUI highlighting. Tree-sitter exposes incremental edits/reparsing and exact
byte/point ranges in its Rust API
([official Rust docs](https://docs.rs/tree-sitter/latest/tree_sitter/)).
It is the recommended MVP syntax and exact-span substrate. Structural indexing
must use its own extractor/version contracts rather than reuse presentation
highlight queries blindly.

### Cargo metadata

`cargo metadata` supplies authoritative workspace/crate/package/target and
resolved dependency structure. The Rust `cargo_metadata::MetadataCommand`
supports manifest/current-directory/features controls
([crate documentation](https://docs.rs/cargo_metadata/latest/cargo_metadata/struct.MetadataCommand.html)).
Evaluate adding the crate versus invoking pinned Cargo and parsing JSON during
the implementation slice; no dependency is authorized by this design.

### rust-analyzer

rust-analyzer can resolve semantics that a syntax-only extractor cannot:
cross-module references, method dispatch, macro-expanded identities, and type
relationships. Its public command-line surface describes batch/debugging tasks,
while SCIP generation is not presented as a stable library API
([official CLI documentation](https://rust-lang.github.io/rust-analyzer/rust_analyzer/cli/index.html)).
Treat it as an optional precision augmentation after measuring setup cost,
availability, output stability, cancellation, and incremental latency.

### SCIP

SCIP is a protobuf protocol for symbols, occurrences, ranges, roles, and
relationships, with Rust bindings and a rust-analyzer indexer
([SCIP repository](https://github.com/scip-code/scip),
[indexer guidance](https://sourcegraph.com/docs/code-navigation/writing-an-indexer)).
It is attractive as an ingestion boundary, especially for future languages,
but does not replace manifests, Markdown/rationale extraction, revision
ownership, or RSI authorization. Defer it from the no-external-runtime MVP;
design an importer behind the same contribution transaction later.

### petgraph and SQLite traversal

`petgraph` supplies directed graph structures and traversal/shortest-path
algorithms without defining persistence
([crate documentation](https://docs.rs/petgraph/latest/petgraph/)). SQLite
recursive CTEs can execute bounded graph walks
([SQLite WITH documentation](https://sqlite.org/lang_with.html)).

Use SQL for exact lookup and shallow bounded neighbors/subgraphs, then load only
the authorized induced subgraph into petgraph for deterministic shortest path
and impact algorithms. Do not load a whole repository graph for every query.
FTS/vector retrieval may select candidate start nodes, but traversal over typed
edges remains the answer substrate
([SQLite FTS5 documentation](https://www.sqlite.org/fts5.html)).

### Community detection

`leiden-rs` offers a pure-Rust Leiden implementation with petgraph adapters;
`graphrs` offers Leiden/Louvain and multiedges in its own graph model
([leiden-rs](https://docs.rs/leiden-rs/latest/leiden_rs/),
[graphrs](https://docs.rs/graphrs/latest/graphrs/)). Both add algorithmic and
data-model risk before communities have proven MVP value. Defer adoption,
benchmark both for determinism, memory, quality, maintenance, and license, and
store results only as derived snapshot annotations.

## Federation research

The main store already has durable observations and entity cards.
`Observation.source_ids` means parent observation IDs in the Dreamer lineage,
not an arbitrary cross-domain reference
(`crates/rsi-common/src/types.rs:1495-1537`). Generalizing that field would
weaken its invariant and require every observation consumer to understand
heterogeneous targets.

Recommendation:

- keep `source_ids` observation-specific;
- create a separate typed `codegraph_links` ledger in `rsi.db`;
- bind every link to project and workspace authorization;
- target stable code node identity plus, when needed, a node version/snapshot;
- support Observation, EntityCard, MemoryChunk, Session, ResearchFinding,
  Plan, and Commit targets;
- use relation types including `MENTIONS`, `DISCUSSES`, `DECIDED_ABOUT`,
  `IMPLEMENTS_FINDING`, `INVALIDATES`, and `SUPPORTED_BY`;
- record evidence records, producer kind/version, confidence, created/valid
  intervals, supersession, and logical deletion;
- never promote an LLM-generated link to a structural edge.

Memory chunks are derived and may disappear during reindex. A link to a chunk
therefore carries a stable content/source fingerprint and becomes
`target_missing` rather than being silently retargeted. Links to observations,
cards, sessions, research findings, plans, and commits are durable and should
use their native identities.

## Operational patterns worth reusing

RSI memory already demonstrates a bounded worker queue, sequential DB ownership,
debounced watching, hash-based file synchronization, deletion detection, and an
atomic replacement path (`crates/rsid/src/memory/worker.rs:16-85`,
`crates/rsid/src/memory/sync.rs:153-253`). Codegraph should reuse the operational
shape, not the database or retrieval semantics:

```text
watch/coalesce -> bounded command queue -> cancelable parser pool
  -> staging contributions -> single SQLite writer transaction
  -> snapshot/current-view commit -> compact status event
```

A full reconcile is the safe fallback after overflow, crash, extractor upgrade,
or uncertain watcher state. Index health must expose queue depth, last
successful snapshot, dirty/pending counts, extractor versions, cancellation,
error, and stale/reconcile-needed state.

## Stable findings

- **F-001 — Existing work:** D5 is an unsliced, benchmark-gated codegraph idea;
  this work must refine it rather than create a competing program.
- **F-002 — Memory status:** The May 11 project-scoped memory plan is implemented
  and its separate episodic architecture is current, not active planning.
- **F-003 — Domain boundary:** Workflow DAGs, episodic memory, and deterministic
  source structure have different truth, lifecycle, and query semantics.
- **F-004 — Scope:** The effective sandbox/worktree, not only the canonical
  project root, determines structural truth visible to a session.
- **F-005 — Extraction:** Pinned Graphify validates deterministic tree-sitter
  extraction as a useful foundation, but exact spans and versioned rules need
  stronger representation.
- **F-006 — Graph fidelity:** Dictionary/JSON graph storage risks type drift,
  collapsed parallel relations, and weak multi-evidence representation.
- **F-007 — Provenance:** `EXTRACTED`, `INFERRED`, `AMBIGUOUS`, numeric
  confidence, producer identity, and evidence must be explicit on derived
  structural facts.
- **F-008 — Incrementality:** Hashing is valuable, but changed and deleted file
  contributions require atomic replacement and stale-edge elimination.
- **F-009 — Query bounds:** Search, explain, neighbors, path, subgraph, affected,
  and diff need uniform count, depth, time, memory, and token budgets.
- **F-010 — Rationale:** Deterministic document/reference markers belong in
  structure; LLM rationale interpretation belongs in evidence-labelled
  federation.
- **F-011 — Summaries:** Node summaries are derived, model-dependent material
  requiring provenance and invalidation; they are not MVP structural truth.
- **F-012 — Presentation:** A dense native inspector is valuable; broad
  visualization/export formats should follow measured demand.
- **F-013 — Learning overlays:** Work-memory overlays should remain separate
  from structure and become governed federation links.
- **F-014 — Multi-repository:** Global graphs require explicit repository
  namespaces, authorized query sets, and cross-repository evidence; defer them.
- **F-015 — Upstream drift:** Post-pin fixes repeatedly expose identity,
  staleness, ambiguity, direction, and scope as first-class invariants.
- **F-016 — Storage:** A per-workspace rebuildable SQLite graph plus durable
  `rsi.db` links best separates churn, recovery, isolation, and irreplaceable
  knowledge.
- **F-017 — MVP stack:** Existing tree-sitter grammars plus Cargo metadata are
  the lowest-risk Rust/Cargo/Markdown MVP extraction base.
- **F-018 — Semantic augmentation:** rust-analyzer and SCIP promise precision
  but require measured, optional ingestion contracts rather than MVP coupling.
- **F-019 — Traversal:** Bounded SQL plus bounded petgraph snapshots avoids both
  recursive-query overreach and whole-graph per-request loading.
- **F-020 — Communities:** Leiden/Louvain should be a later derived benchmark,
  not an MVP dependency or correctness input.
- **F-021 — Federation:** Dreamer `source_ids` should remain observation lineage;
  cross-domain relationships need a separate typed link ledger.
- **F-022 — Native authorization:** Harness and CodexAppServer already bind
  project/caller scope at tool construction, a suitable read-only codegraph
  pattern.
- **F-023 — Retrieval seam:** rsi-graph's dormant `FileIndex` and
  `RetrievalBackend` concepts can expose codegraph without reusing workflow
  node/edge types.
- **F-024 — Context policy:** Codegraph retrieval should be on demand, not
  unconditional ContextPipeline prompt injection.
- **F-025 — Operations:** Bounded queues, debounce, cancellation, backpressure,
  reconciliation, and observable health are required daemon behaviors.
- **F-026 — TUI:** Existing dense navigator/inspector overlays provide a native
  interaction grammar for a codegraph surface.
- **F-027 — Proof:** D2-style before/after benchmarks and exact-evidence golden
  questions must gate expansion beyond a small dogfood slice.

## Open questions carried to human approval

1. Should codegraph indexing be opt-in for the first slice or automatically
   enabled only for RSI's own repository?
2. How many clean and dirty snapshots should each worktree retain by default?
3. Is adding a small durable federation table to `rsi.db` acceptable while the
   structural graph remains separately rebuildable?
4. Should the first slice include a minimal TUI inspector or prove daemon/native
   query value first?
5. Is `gC` an acceptable future keybinding, subject to the normal keybinding
   design and documentation change?
