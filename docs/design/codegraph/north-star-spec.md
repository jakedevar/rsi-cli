# RSI codegraph north-star specification

Status: proposed; implementation approval required

Audience: RSI maintainers, daemon/TUI implementers, agent-harness designers

Dogfood target: the RSI repository

## Product statement

RSI codegraph gives a session exact, revision-correct answers about the source
workspace it is actually editing. It turns Rust syntax, Cargo structure,
Markdown rationale, and later semantic index inputs into a queryable,
evidence-bearing graph. It also creates a governed bridge from those code
entities to RSI's episodic knowledge without confusing memories with source
truth.

A useful codegraph answer is not “these chunks seem similar.” It is:

> `dispatch_lc_action` calls this handler through these typed edges, at these
> exact source spans, in this worktree snapshot; this research finding and
> decision discuss that node, with independently labelled evidence.

## Stage contract

### Inputs

- Findings `F-001` through `F-027` from the comparative research.
- Existing RSI project/session/worktree resolution, daemon persistence,
  memory, rsi-graph retrieval concepts, native tool registries, ContextPipeline,
  event bus, and ratatui interaction patterns.
- The dogfood requirements and capability list supplied by the human.

### Process

- Define user outcomes and invariants before implementation mechanisms.
- Keep structural, episodic, and orchestration data ownership explicit.
- Specify bounded behavior, authorization, failure recovery, and observability
  as product requirements.
- Separate MVP commitments from measured later opportunities.

### Outputs

- North-star use cases, goals, non-goals, architecture, lifecycle, and
  acceptance criteria.
- A dogfood MVP boundary and evolution path.

### Verify

- Every requested dogfood entity and query is represented.
- Every answer is workspace-bound, revision-labelled, evidence-bearing, and
  bounded.
- No Python or JavaScript runtime is required by the MVP.
- No design path unconditionally injects codegraph output into prompts.

## Problem

Agents currently discover source structure through repeated file search,
source reads, compiler feedback, and memory retrieval. Those mechanisms remain
valuable, but they repeatedly spend tokens reconstructing relationships such as
module ownership, impl membership, call direction, dependency reachability,
tests, documentation rationale, and change impact. Raw search also cannot
reliably distinguish the canonical project tree from a session's active
sandbox.

The prior D5 idea correctly identified the opportunity, SQLite, RPC access, and
the need for a before/after discovery-cost measurement. It left identity,
worktree scope, revision history, exact evidence, incremental deletion,
federation, authorization, and daemon operations unresolved. This specification
fills those gaps.

## Goals

1. Index the effective RSI worktree deterministically for Rust, Cargo
   manifests/TOML, and Markdown.
2. Represent exact source structure with stable identities, typed relations,
   exact spans, provenance, confidence, and evidence.
3. Replace changed/deleted file contributions atomically and eliminate stale
   edges.
4. Retain bounded structural history while making “current revision” cheap and
   unambiguous.
5. Answer exact search, explain, incoming/outgoing neighbors, shortest path,
   bounded subgraph, affected/impact, and graph-diff queries.
6. Return stable ordering, exact clickable locations, truncation metadata, and
   token-bounded agent output.
7. Expose scope-bound read-only native tools to Harness and CodexAppServer
   sessions without accepting a caller-supplied project or root.
8. Provide a dense ratatui inspector and index-health surface.
9. Federate code entities with observations, entity cards, memory chunks,
   sessions, research findings, plans, and commits through separately owned,
   evidence-labelled links.
10. Prove value and correctness on RSI itself against raw source inspection,
    `rg`, and pinned Graphify on temporary copies.

## Non-goals for the MVP

- Replacing rust-analyzer, the Rust compiler, `cargo metadata`, `rg`, or memory.
- Treating embedding similarity as graph evidence or traversal.
- LLM-generated structural nodes, edges, identities, or source spans.
- Automatic node summaries, community detection, or learning-derived impact.
- Multi-repository/global graph queries.
- Broad HTML/Neo4j/Obsidian/GraphML export.
- Write-capable agent tools.
- Safe access for CLI-backed providers; that requires a later guarded
  authorization slice.
- Python or JavaScript runtime dependencies.
- Unconditional prompt injection through ContextPipeline.

## Architectural invariants

### I1. Three knowledge domains

```text
rsi-graph                 RSI memory                    codegraph
workflow execution        episodic/semantic recall     structural source truth
Node/Edge task model      chunks/observations/cards    code nodes/edges/evidence
daemon workflow state     rsi.db + memory.sqlite       codegraph.sqlite
        \                       |                           /
         \________________ governed integration __________/
```

No crate type named `Node` or `Edge` is reused merely because the words match.
Integration happens through explicit identifiers and retrieval contracts.

### I2. Structural truth is deterministic

A structural record must be reproducible from source, manifests, a
deterministic semantic index, or a versioned deterministic rule. The allowed
provenance labels are:

- `EXTRACTED`: directly observed in an input and anchored to exact evidence;
- `INFERRED`: produced by a deterministic resolver from extracted facts;
- `AMBIGUOUS`: a deterministic resolver found multiple defensible targets.

All carry numeric confidence, producer kind/version, and evidence. LLM output
is never written to structural node/edge/hyperedge tables.

### I3. Semantic/episodic federation is separate

Links such as `DISCUSSES` and `DECIDED_ABOUT` may be manually authored or
LLM-suggested, but live in a distinct ledger with their own provenance,
confidence, evidence, validity, and deletion semantics. A link never changes
the target node's structural meaning.

### I4. Scope is server-bound

Every query is authorized against a daemon-resolved project and effective
workspace. Native tools omit project ID, repository ID, workspace path, and
arbitrary database path from their schemas. The server captures those values
when building the tool.

### I5. Revision includes dirty state

`HEAD` alone is insufficient. An observed snapshot identifies:

- repository/workspace identity;
- VCS commit when available;
- dirty-state digest over indexed inputs and index configuration;
- extractor/rule/grammar/manifest versions;
- observation timestamp and completion state.

Queries default to the latest complete snapshot for the caller's effective
workspace. In-progress snapshots are never served as current.

### I6. Replacement is atomic

Changed and deleted file contributions are staged, validated, and replaced in
one SQLite transaction. The transaction also commits snapshot/current-view and
health metadata. On failure, the last complete snapshot remains queryable.

### I7. Work is bounded

Indexing has bounded queues, parser concurrency, database batch sizes, memory,
and cancellation. Queries have maximum depth, nodes, edges, candidates, wall
time, memory estimate, and output tokens. Requested values can only reduce
server maxima.

### I8. Every answer explains itself

Structural answers include:

- workspace and snapshot identity;
- node/edge kinds and direction;
- exact source locations;
- provenance and confidence;
- evidence and producer version;
- applied limits, truncation reason, and deterministic continuation where
  supported.

## Dogfood corpus and extraction

### Rust

Extract:

- repository/workspace, crate, target, module, and file ownership;
- traits, structs, enums, unions, type aliases, constants, statics;
- functions, methods, impl blocks, associated items, fields, and enum variants;
- `use`/import/re-export relationships;
- impl-for-type and impl-trait-for-type relationships;
- syntactic calls and macro invocations, with unresolved/ambiguous candidates
  preserved;
- tests, test modules, fixtures where deterministically recognizable;
- rustdoc comments, intra-doc links, and explicit rationale/ADR markers;
- containment, declaration, reference, call, dependency, implementation,
  inheritance/bounds, test-of, and documentation-reference relations.

The MVP must not claim fully resolved dynamic/method call semantics from
tree-sitter alone. Syntactic call edges are `AMBIGUOUS` or target a stable
unresolved-symbol node until a deterministic resolver proves a unique target.

### Cargo/TOML

Use Cargo metadata plus manifest spans to extract:

- Cargo workspace and members;
- packages/crates, targets, features, and target kinds;
- normal/build/dev dependencies and package renames;
- local path relationships and resolved package identities;
- manifest definitions and references with exact TOML spans;
- crate-to-crate dependency edges with dependency kind, target condition, and
  feature evidence.

### Markdown

Extract:

- files, headings, stable anchors, links, and code/file/symbol references;
- research finding identifiers matching `F-\d+`;
- plan/slice identifiers and `satisfies: [F-...]` provenance;
- commit hashes and explicit source paths/locations;
- ADR/decision/rationale markers and cross-document references;
- stage-contract sections and named outputs where useful for provenance.

The extractor records deterministic references. Natural-language interpretation
such as “this paragraph decided the API” is a candidate federation link, never
a structural fact.

## Core entity and relationship vocabulary

The canonical enums are defined in the schema proposal. At the product level,
the graph must at least express:

```text
Workspace CONTAINS Crate CONTAINS Module CONTAINS Symbol
File DECLARES Symbol
Module IMPORTS Symbol|Module
Function|Method CALLS CallableCandidate
Impl IMPLEMENTS Trait FOR Type       (hyperedge-capable)
Crate DEPENDS_ON Crate
Test TESTS Symbol|Module|Crate
DocHeading REFERENCES CodeNode
PlanItem IMPLEMENTS_FINDING ResearchFinding  (federation, not structure)
Observation DISCUSSES CodeNode               (federation, not structure)
```

Unresolved and external entities are first-class typed nodes with explicit
resolution state. They are not silently dropped and not merged by display
label.

## Query behavior

### Exact search

Find nodes by canonical identity, qualified name, exact/prefix display label,
path, kind, relation, and snapshot. Optional FTS/vector lookup may rank seed
candidates, but the returned answer must be composed from stored typed nodes
and evidence.

### Explain

Resolve a unique node or return candidates. Explain identity, containment,
declaration, selected incoming/outgoing relationships, structural provenance,
federation links if authorized/requested, and exact evidence. It must never
pick the first ambiguous label silently.

### Neighbors

Return incoming, outgoing, or both; filter by relation/kind/confidence; preserve
parallel edges/evidence; apply deterministic ordering and caps.

### Path

Find a shortest directed path by default. An undirected request is explicit.
Filters define allowed relations and confidence. Multiple equal shortest paths
are deterministically ranked and capped.

### Subgraph

Return the bounded induced neighborhood around one or more exact seed nodes,
including nodes, edges, evidence summaries, boundary nodes, and truncation.

### Affected/impact

Apply a named, versioned impact profile over reverse and/or forward relations.
The default Rust profile distinguishes probable compile impact, runtime
callers, tests, dependents, and documentation/rationale. It reports each hop,
not only a flattened list.

### Graph diff

Compare two complete snapshots and classify:

- node added/removed/moved/renamed/modified;
- edge added/removed/changed-direction/changed-resolution;
- evidence and source-location changes;
- ambiguity introduced/resolved;
- federation links newly valid/invalidated when explicitly requested.

Dirty-to-dirty and commit-to-dirty comparisons are supported within one
workspace; cross-workspace comparison is denied unless an operator-level
contract explicitly authorizes it later.

## Indexing lifecycle

```text
filesystem/Cargo event
  -> debounce and coalesce
  -> enqueue bounded reconcile request
  -> establish workspace + revision candidate
  -> hash and classify changed/deleted inputs
  -> parse changed inputs in cancelable bounded pool
  -> validate contribution batches
  -> BEGIN IMMEDIATE
       insert incomplete snapshot metadata
       replace changed/deleted file contributions
       resolve deterministic cross-file edges
       validate no dangling current structural references
       finalize snapshot and current pointer
       update health
     COMMIT
  -> publish compact status/update event
```

Queue overflow marks `reconcile_required` and coalesces to one full reconcile;
it does not accumulate every path. A cancellation stops parse work and prevents
an incomplete snapshot from becoming current. Crash recovery removes or
resumes staging state, integrity-checks metadata, and keeps the last complete
snapshot.

## Federation lifecycle

Federation links are durable records in the main database:

1. Resolve and authorize the code node against its project/workspace.
2. Resolve the target in its owning domain.
3. Record relation, direction, evidence, producer, confidence, temporal
   validity, and optional snapshot specificity.
4. On graph rebuild, stable node identity retains links. Missing node versions
   become unresolved or invalidated; they are not retargeted by label.
5. On target deletion/reindex, use domain-specific semantics: durable targets
   soft-delete or invalidate; derived memory chunks become `target_missing`
   while retaining their content/source fingerprint.
6. LLM suggestions begin as `candidate`; promotion to `accepted` requires a
   defined policy or human action. Candidate links are excluded by default.

## Authorization and privacy

- Project and effective workspace are derived server-side.
- A native tool can observe only the workspace bound to its caller.
- An operator RPC may select a workspace only after validating it belongs to
  the requested project.
- Database paths and raw SQL are never API inputs.
- Federation queries intersect authorization of both endpoints.
- A link cannot be used to jump from an authorized code node into another
  project's session, observation, card, chunk, plan, or repository.
- Multi-repository queries are absent from MVP.
- Events carry IDs, counts, status, and errors—not source contents or graph
  payloads.

## Native agent experience

Harness and CodexAppServer sessions receive small read-only tools:

- `rsi_codegraph_search`
- `rsi_codegraph_explain`
- `rsi_codegraph_traverse`

The traversal tool uses an operation enum for neighbors, path, subgraph,
affected, and diff so the native surface stays compact. Results are structured,
token-bounded, and include clickable `path:line:column` locations. Indexing,
cancel, export, federation writes, and arbitrary scope selection are not native
agent tools.

## TUI experience

The proposed `:codegraph` inspector uses the existing dense RSI visual grammar:

```text
┌ query / scope / snapshot / health ────────────────────────────────┐
│ results / tree        │ node + exact evidence                    │
│ crate/module/symbol   │ kind · identity · provenance · validity  │
│                       ├───────────────────────────────────────────┤
│                       │ incoming / outgoing / federation tabs     │
└ status · filters · bounds · truncation · key hints ──────────────┘
```

Required interactions:

- incremental exact search with explicit FTS mode;
- navigate containment and relation lists;
- switch incoming/outgoing/both;
- choose snapshot/diff endpoints;
- open exact source location in the current effective worktree;
- inspect all evidence for parallel relations;
- see queue/index health and trigger/cancel operator indexing;
- copy a bounded JSON/text result;
- never imply that a stale snapshot is current.

A future `gC` binding appears unassigned in the current normal-mode map, but
keybinding selection remains a human decision and any implementation must
update `docs/keybindings.md`.

## ContextPipeline and rsi-graph integration

Codegraph is registered conceptually as a retrievable
`ContextSourceType::FileIndex` backend. Its implementation does not live inside
workflow graph node/edge persistence.

ContextPipeline behavior defaults to:

- expose native tool availability and compact index health;
- perform no graph query unless explicitly requested by the user/agent or a
  workflow input names a bounded retrieval;
- never append a generic codegraph neighborhood to every session prompt;
- allow a later workflow stage contract to declare a static codegraph query as
  an input, producing a bounded, version-labelled result.

This keeps prompt cost observable and preserves the D2 measurement question.

## Later CLI-provider access strategy

CLI-backed providers do not receive native in-process tools. A later security
slice may add a read-only, token-authorized agent verb or a purpose-built
`rsi-rpc codegraph` facade. It must:

- be explicitly included in the agent-facing allowlist;
- bind caller identity from the transport token;
- omit project/root/database selection;
- expose only read-only query operations with server caps;
- redact unauthorized federation targets;
- log query metadata without source payloads by default;
- ship only after adversarial authorization tests.

It is not acceptable to expose generic daemon RPC passthrough or an SQLite path
to achieve CLI access.

## Success criteria

The dogfood MVP is successful when:

1. RSI's Rust/Cargo/Markdown corpus indexes deterministically with no Python or
   JavaScript runtime.
2. Repeating an identical build produces byte-equivalent canonical query
   results and identical structural identities.
3. Editing or deleting one file atomically removes every stale contribution
   without disturbing unrelated files.
4. Parallel worktrees can contain conflicting edits without observable
   cross-contamination.
5. Golden architecture/impact questions meet defined answer and exact-evidence
   thresholds.
6. All queries honor server maxima and report truncation.
7. Native tools cannot select or escape caller scope.
8. Crash/restart/reindex leaves the last complete graph usable and health
   truthful.
9. Benchmarks report full build, incremental latency, query latency, CPU/RSS,
   output tokens, and answer correctness against `rg`, raw inspection, and
   pinned Graphify.
10. Expansion to semantic augmentation, summaries, communities, broad export,
    or multi-repo is separately approved from measured evidence.

## Delivery boundary

This specification authorizes no implementation. The recommended first slice
is in `slice-map.md`; unresolved choices and recommended defaults are in
`human-decisions.md`.
