# Codegraph benchmark and golden-query specification

Status: proposed verification contract

Safety rule: benchmark copies only; never run Graphify or destructive mutation
benchmarks against the live RSI worktree

## Stage contract

### Inputs

- Findings `F-004`, `F-005`, `F-008`, `F-009`, `F-015` through `F-020`, and
  `F-022` through `F-027`.
- RSI dogfood repository fixtures and exact source/history expectations.
- Pinned Graphify commit
  `2fa6cd3d5548577f8c5f591b713f0bf80c1af183`.
- `rg` and raw source inspection baselines.
- Proposed schema, query, authorization, and operational contracts.

### Process

- Define correctness and performance questions before implementation.
- Separate structural extraction correctness from answer usefulness.
- Exercise clean/dirty revisions, mutation, deletion, isolation, crash recovery,
  determinism, and hard bounds.
- Compare all systems on disposable copies with equivalent corpus/scope.

### Outputs

- Golden architecture/change-impact query suite.
- Incremental, isolation, recovery, determinism, authorization, and bounds tests.
- Reproducible benchmark protocol and metric schema.
- MVP and expansion gates.

### Verify

- Every benchmark records corpus commit/digest, tool version/config, hardware,
  cache state, and applied bounds.
- No upstream benchmark process receives the live workspace path.
- Exact evidence is scored separately from plausible prose.
- Results include full build, incremental latency, query latency, CPU/RSS,
  output tokens, stale facts, and answer correctness.

## Verification principles

1. **Exact beats plausible.** A correct-sounding answer with a wrong direction,
   stale edge, or imprecise source location fails.
2. **Absence matters.** Deleted files and resolved ambiguities must disappear
   from the current view.
3. **Scope is part of correctness.** Returning a true fact from another
   worktree/project is a security failure.
4. **Bounds are semantic.** A truncated answer must say so; “no path” is not a
   valid substitute for “path may exist beyond the admitted boundary.”
5. **Determinism is observable.** Identical inputs/configuration must yield
   identical stable IDs and canonical query results.
6. **Baselines get the same task.** `rg`, raw inspection, RSI codegraph, and
   Graphify are measured against the same corpus copy and question rubric.
7. **Graphify remains pinned.** Later upstream results may be a labelled
   appendix, never silently substituted for v0.9.25.

## Fixture strategy

### Dogfood snapshot

- Start from a clean temporary clone/copy of the RSI repository at the exact
  benchmark commit.
- Record:
  - git commit;
  - sorted indexed-file content digest;
  - Rust/Cargo/tree-sitter/extractor versions;
  - enabled Cargo feature/target configuration;
  - excluded paths;
  - benchmark host CPU, RAM, filesystem, OS, and toolchain.
- Exclude `.git` object contents, build outputs, sandbox siblings, secrets, and
  the live `~/.rsi` state.

### Micro-fixtures

Small checked-in fixtures should isolate:

- same-named functions in multiple modules;
- two parallel call sites between the same symbols;
- an ambiguous method/reference later resolved;
- trait impl for a type with generics;
- re-export/import aliases;
- Cargo normal/dev/build dependencies and renamed path dependency;
- Rust tests and Markdown references;
- move/rename with stable and uncertain identity cases;
- a deleted file with incoming/outgoing cross-file edges;
- invalid/temporarily broken manifest;
- non-UTF-8 or unsupported input policy;
- symlink/path traversal and ignored-file cases;
- two repositories with identical display names;
- two worktrees with conflicting edits at the same relative path.

Every micro-fixture has canonical expected nodes, edges, hyperedges, evidence
spans, provenance, confidence, and snapshot changes in a human-reviewable
format.

## Golden RSI architecture questions

Each question defines expected answer components and source anchors. Line
numbers are verified at the benchmark commit and stored in fixture expectations,
not hard-coded forever in prose.

### GQ-01: Keypress dispatch

**Question:** How does a normal-mode keypress become an RSI action handler call?

Expected structural path:

```text
crossterm Event
  -> keybinding/VimMachine mapping
  -> LcAction
  -> dispatch_lc_action
  -> relevant action_handler submodule
```

Expected evidence domains:

- `crates/rsi/src/keybindings.rs`
- `crates/rsi/src/modalkit_types.rs`
- `crates/rsi/src/action_handler/mod.rs`

Pass: correct order/direction, exact declarations and call/reference evidence,
no confusion with workflow graph events.

### GQ-02: Effective worktree launch

**Question:** Which source root does a sandboxed provider process and its
ContextPipeline use?

Expected:

- canonical project resolution is distinct from effective root;
- valid sandbox root wins;
- provider `current_dir` and ContextPipeline receive effective root;
- `Session.working_dir` remains required/canonical and `sandbox_root` is
  separately recorded.

Anchor: `crates/rsid/src/session/launch.rs` and rotation logic.

### GQ-03: Native memory tool scope

**Question:** How is the project scope of a Harness memory search prevented from
being caller-spoofed?

Expected:

- registry captures project/handle at tool construction;
- tool schema omits project selection;
- manager performs project-scoped search.

Anchor:
`crates/rsid/src/session/harness/tools/mod.rs`,
`crates/rsid/src/session/harness/tools/memory.rs`.

### GQ-04: Memory ownership and persistence

**Question:** Where do raw observations, Dreamer lineage, memory chunks, FTS,
and vectors live, and who owns access?

Expected:

- observations/entity cards/main durable records in main store;
- chunks/cache/FTS/vector index in separate `memory.sqlite`;
- daemon ownership and sequential worker;
- `Observation.source_ids` means observation lineage.

Must not call the 2026-05-11 plan active.

### GQ-05: rsi-graph versus codegraph

**Question:** Why can codegraph use `FileIndex`/`RetrievalBackend` concepts
without using rsi-graph workflow `Node`/`Edge` types?

Expected:

- rsi-graph is workflow/multi-agent DAG execution;
- context/retrieval concepts are integration seams;
- structural schema and persistence are separate.

Anchor: `crates/rsi-graph/src/lib.rs`, `context.rs`, `retrieval.rs`.

### GQ-06: Session `working_dir` impact

**Question:** What would be affected by changing `Session.working_dir` from
required to optional?

Expected impact groups:

- common serde/type invariants and tests;
- project/effective-root resolution;
- provider launch/current directory;
- sandbox/rotation/reconciliation;
- ContextPipeline/memory/project scope;
- any UI/RPC consumers.

Pass requires evidence-bearing paths, not a full-repo “everything” answer.

### GQ-07: Adding an `AgentSpawnChild` field

**Question:** What code and tests are affected by changing the guarded spawn
request contract?

Expected:

- `rsi-common` request/serde contract;
- daemon RPC parsing/auth/control handle;
- native Harness/CodexAppServer schema generation;
- CLI discovery/examples where applicable;
- scoping/idempotency/authorization tests.

The answer must distinguish generic RPC from agent-facing allowlisted verbs.

### GQ-08: Memory reindex atomicity

**Question:** How does memory reindex avoid exposing a half-built index?

Expected:

- temp/replacement database or equivalent atomic swap;
- worker ownership;
- status/event/error behavior;
- old usable state remains until replacement.

Anchor: `crates/rsid/src/memory/sync.rs`, store/worker code, architecture docs.

### GQ-09: ContextPipeline assembly

**Question:** Which context sources are gathered, which run concurrently, and
where is project memory included?

Expected exact call/containment relationships in
`crates/rsid/src/session/context_pipeline.rs`, including current default memory
behavior and the distinction from proposed on-demand codegraph.

### GQ-10: Research provenance

**Question:** Which planned slices satisfy finding `F-008`, and what
verification proves stale-edge elimination?

Expected:

- deterministic parsing of the schema-v2 sidecar and `satisfies` declarations;
- links to slice identifiers and verification cases;
- no semantic guess based only on prose similarity.

### GQ-11: Daemon event conversion

**Question:** How does an internal typed daemon event become a public
`BusEvent`, and what would a codegraph index event need to preserve?

Expected:

- `DaemonEvent` internal enum;
- conversion to `BusEvent` struct with string `event_type`;
- bounded payload/event naming.

### GQ-12: Recursive DAG read surface

**Question:** Trace one recursive graph status/list request from common RPC
types through daemon handler to TUI inspector.

Purpose: test cross-crate path/query quality and avoid mistaking recursive task
DAGs for source codegraph.

## Golden change-impact scenarios

### GI-01: Delete a called function's file

Mutation:

- copy fixture/repository;
- delete one file containing a function called from two other files.

Expected after one incremental transaction:

- file version absent from current view;
- declaration/node version no longer current;
- all call/reference/import edges owned by or targeting removed current version
  resolved/removed according to policy;
- no evidence points to deleted current file;
- unrelated node/edge IDs unchanged;
- diff reports removals and affected callers/tests;
- historical snapshot remains correct if retained.

### GI-02: Edit one call target

Change a call from `foo()` to `bar()` in one file.

Expected:

- only that file contribution plus deterministic resolver dependents rebuild;
- call-site span/hash updates;
- `foo` edge ends validity and `bar` edge begins;
- parallel other `foo` call remains;
- no whole-repository identity churn.

### GI-03: Rename/move module

Expected:

- file/module path update;
- stable item identity preserved only where rule proves it;
- otherwise explicit supersession/candidate mapping;
- imports/calls/evidence retarget through deterministic resolver;
- diff classifies move/rename rather than arbitrary add/remove when justified.

### GI-04: Break and repair `Cargo.toml`

Expected:

- parser/metadata failure visible in health;
- last complete Cargo structure stays queryable and labelled stale/pending;
- no partial dependency graph becomes current;
- repair produces a complete new snapshot.

### GI-05: Resolve ambiguity

Start with two same-named candidate callees; make one uniquely resolvable.

Expected:

- prior snapshot retains `AMBIGUOUS` candidates;
- current snapshot has one deterministically resolved target;
- diff says ambiguity resolved;
- confidence/provenance/evidence changes are visible.

## Exact source and evidence tests

For every supported entity/relation kind:

1. slice the fixture bytes at returned `[start_byte, end_byte)`;
2. independently map bytes to asserted 1-based line/column;
3. verify source hash and file version;
4. confirm excerpt contains the intended syntactic construct;
5. ensure relation direction and endpoint kinds match;
6. ensure producer/rule/extractor version is present;
7. verify multiple call sites produce distinct edges;
8. verify multiple supporting facts attach multiple evidence records;
9. verify no absolute machine path appears in stable ID or serialized agent
   output.

Include Unicode before/inside spans, CRLF, final line without newline, tabs,
large files, and zero-width syntax/error nodes as applicable.

## Incremental and deletion test matrix

| Case | Required assertion |
|---|---|
| No-op reconcile | no new structural snapshot or identical digest policy; no ID churn |
| One Rust body edit | affected file only plus declared resolver dependents |
| One signature edit | callers/imports/impl/test impact refreshed |
| One Markdown link edit | document contribution only; federation candidate policy respected |
| Manifest dependency add/remove | Cargo/manifest contributions atomically replaced |
| File delete | zero current facts/evidence owned by deleted version |
| Directory rename | normalized paths and affected contribution ownership correct |
| Ignored file event | no graph mutation |
| Watcher overflow | one `reconcile_required`, bounded queue, full reconcile |
| Event during parse | coalesced follow-up; no mixed revision becomes current |
| Extractor version bump | affected contributions invalidated/rebuilt deterministically |

Define a stale-fact invariant query that must return zero:

```text
current fact/evidence
  whose owning contribution is not current
  OR whose file version is absent
  OR whose endpoint lacks a current version
     and is not explicitly external/unresolved
```

## Worktree and cross-project isolation

### WI-01: Conflicting worktrees

- Create two temporary git worktrees at the same commit.
- Change the same function differently in each.
- Index both concurrently.
- Query by caller-bound handles.

Pass:

- distinct workspace IDs and dirty digests;
- each query sees only its own content/evidence;
- identical untouched nodes may share logical repository identities but never
  snapshot/version rows;
- an explicit snapshot ID from the other workspace returns scope denial.

### WI-02: Two projects with identical names/paths

Use two repositories containing identical relative paths and symbols.

Pass:

- repository namespace prevents ID collision;
- native tool cannot supply the other project ID;
- FTS/candidate caches include scope key;
- events/status do not cross subscribers' authorization.

### WI-03: Sandbox cleanup

After sandbox tombstone/purge:

- native handle can no longer query the root;
- workspace health is missing/tombstoned;
- derived DB cleanup follows policy;
- durable links remain historical/unavailable, not retargeted to canonical
  checkout.

### WI-04: Federation pivot attempt

Create an authorized code node and a forged/invalid link target ID from another
project. Writes fail closed; reads never reveal existence. Test all target
kinds and candidate/accepted states.

## Crash, restart, and recovery

Inject termination/failure:

- before staging;
- after partial staging;
- after SQLite transaction begins;
- before current pointer update;
- after commit before event publication;
- during full reindex replacement;
- while cancellation is requested;
- with corrupt/incompatible derived DB metadata.

Pass:

- last complete snapshot remains queryable or the workspace is honestly
  `failed_no_snapshot`;
- incomplete staging never serves;
- restart reconstructs/coalesces safe work;
- duplicate idempotency keys do not double-commit;
- committed-but-unpublished updates are discoverable from persisted health;
- integrity failure triggers quarantine/rebuild path, not mutation of
  `rsi.db`/`memory.sqlite`;
- cancellation is terminal and does not become current later.

## Determinism

Run at least five clean builds with:

- same checkout/config/toolchain on same host;
- randomized parser task scheduling;
- randomized filesystem enumeration input order;
- two process restarts;
- optionally a second compatible host.

Compare:

- stable IDs;
- snapshot revision digest;
- sorted node/edge/hyperedge/evidence canonical export;
- query ordering and path tie-breaking;
- impact group/order;
- diff output;
- community/summaries excluded from MVP comparison.

Timestamps, job IDs, and performance counters are normalized out. Any
structural difference must be attributable to a declared environment input.

## Bounds and load tests

### Query bounds

For every operation, request zero, normal, maximum, and maliciously large:

- depth;
- nodes/edges/evidence;
- candidates;
- timeout;
- output tokens;
- continuation loops.

Pass:

- validation rejects invalid zero/overflow combinations;
- manager clamps before worker execution;
- worker also enforces admission;
- output never exceeds serializer token envelope beyond a small documented
  encoding tolerance;
- truncation cause is exact;
- no unbounded SQL recursion or whole-graph allocation;
- continuation cannot change scope/snapshot/filter.

### Index bounds

Measure and fault:

- filesystem event storm;
- many large Markdown files;
- a generated Rust file;
- slow Cargo metadata;
- parse cancellation;
- DB busy/disk-full conditions;
- too many active sandbox workers.

Pass:

- bounded queue and parser concurrency;
- coalescing/full-reconcile fallback;
- cancellation deadline;
- backpressure visible in health;
- per-workspace and daemon-wide CPU/RSS/disk quotas;
- interactive session/daemon latency remains within agreed budget.

## Benchmark systems

### A. `rg`

Use scripted exact searches plus human/operator source inspection. Record:

- command wall time;
- output bytes/tokens;
- number of files opened;
- end-to-end answer time and correctness.

`rg` is a discovery baseline, not expected to compute graph paths itself.

### B. Raw source inspection

Use the same golden question prompt with ordinary source/list/read tooling and
no codegraph. Record tool calls, bytes/tokens read, latency, and rubric score.
For agent comparisons, pin model/provider/prompt/settings and repeat enough to
report variance.

### C. RSI codegraph

Measure cold/warm exact query and end-to-end agent answer with:

- current snapshot already built;
- index build cost accounted separately;
- identical server bounds recorded;
- fallback source reads counted.

### D. pinned Graphify v0.9.25

- Checkout exactly
  `2fa6cd3d5548577f8c5f591b713f0bf80c1af183` in an isolated environment.
- Install its declared Python requirements outside RSI runtime.
- Point it only at a disposable RSI copy.
- Record Graphify config, optional extras, Python version, and graph artifact
  digest.
- Map its closest query command to the golden rubric; record unsupported
  operations rather than approximating them invisibly.
- Never infer later v0.9.31 behavior into this baseline.

An optional appendix may run later upstream at a separately named commit.

## Measurement protocol

### Build

- cold full index after clearing only the benchmark's derived state;
- warm/no-op reconcile;
- one-file Rust edit;
- one Markdown edit;
- manifest edit;
- one-file delete;
- watcher-to-queryable latency;
- restart with usable current snapshot;
- full reindex while old snapshot serves.

Report:

```text
wall_ms, cpu_ms, peak_rss_bytes, read_bytes, write_bytes,
db_bytes, wal_peak_bytes, parsed_files, reused_files,
nodes, edges, evidence, stale_fact_count, success/error
```

### Query

Run each golden query cold and warm, minimum 30 iterations for local latency
distributions after a small declared warmup. Report p50/p95/p99, CPU, peak
temporary memory, result nodes/edges/evidence, output bytes/tokens, and
truncation.

### Answer correctness

Use a blinded rubric:

- structural fact precision;
- structural fact recall against expected components;
- direction correctness;
- exact-evidence precision;
- stale/foreign fact count;
- ambiguity honesty;
- snapshot/scope labelling;
- unsupported-claim count;
- end-to-end task correctness;
- output tokens and source-tool fallback tokens.

Score deterministic graph response separately from model-authored prose.

## Initial threshold proposal

These are approval candidates, not silently binding constants:

| Metric | Proposed MVP gate |
|---|---|
| Exact evidence | 100% byte/hash validity in fixtures |
| Stale current facts after change/delete | 0 |
| Cross-worktree/project leakage | 0 |
| Identical-build structural determinism | 100% after normalized fields |
| Golden structural precision | at least 0.98 |
| Golden required-component recall | at least 0.90 for MVP-supported facts |
| Full RSI index | p95 under 30 seconds on reference host |
| One-file incremental queryable latency | p95 under 1 second excluding required Cargo full resolution; report separately |
| Exact search warm latency | p95 under 50 ms |
| One-hop neighbors/explain warm latency | p95 under 100 ms |
| Path/impact within default bounds | p95 under 500 ms |
| Default query output | at most 3,000 tokens; per-tool lower defaults |
| Daemon worker queue | fixed bounded capacity; overflow coalesces |
| Recovery | last complete snapshot usable after every injected pre-commit failure |

If reference hardware makes a latency number unrealistic, change it through an
explicit benchmark-review decision, not by omitting it.

## Expansion gates

- **rust-analyzer/SCIP:** add only if it materially raises golden recall/
  resolution and fits build/incremental/RSS budgets.
- **communities:** add only if a named exploration task improves over
  deterministic containment with acceptable determinism/cost.
- **node summaries:** add only if correctness-per-token improves and
  invalidation tests pass.
- **automatic ContextPipeline retrieval:** add only after a controlled
  on-demand-versus-auto experiment.
- **multi-repository:** add only after authorization/identity/isolation design
  and adversarial tests.
- **broad export:** add only after demand plus path/content/security review.

## Benchmark result artifact

An implementation run should emit a versioned machine-readable record and a
human report containing:

- benchmark schema version;
- corpus/tool/config/environment identities;
- raw per-run metrics;
- aggregate distributions;
- golden rubric details and evidence;
- failure/truncation/unsupported cases;
- comparison caveats;
- explicit pass/fail for each gate.

No benchmark result exists yet; this document authorizes none to be fabricated.
