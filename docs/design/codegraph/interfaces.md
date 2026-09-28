# Codegraph RPC, event, native-tool, and TUI contracts

Status: proposed API contract; no implementation

Versioning principle: typed request/response structures in `rsi-common`

## Stage contract

### Inputs

- Findings `F-004`, `F-009`, `F-012`, `F-016`, and `F-022` through `F-026`.
- Existing `RpcRequest`/typed RPC patterns, `DaemonEvent` to `BusEvent`
  conversion, memory and recursive-DAG overlays, Harness native tools,
  CodexAppServer dynamic tool registry, session effective-worktree resolution,
  and ContextPipeline.

### Process

- Design operator RPCs separately from read-only native agent tools.
- Bind authorization and workspace scope server-side.
- Put bounds, snapshot identity, evidence, truncation, and errors in shared
  response envelopes.
- Map each surface to current crate ownership and interaction patterns.

### Outputs

- Proposed `rsi-common` types and RPC verbs.
- Daemon manager/worker ownership and event contracts.
- Native Harness/CodexAppServer tool schemas.
- Dense TUI inspector and index-status behavior.
- rsi-graph, ContextPipeline, and later CLI-provider integration.

### Verify

- Native schemas contain no caller-supplied project, root, workspace, database,
  or write capability.
- Every query has server-enforced bounds and structured truncation.
- Every location is relative, exact, and resolvable against the caller's
  effective workspace.
- Events contain no unbounded graph or source payload.

## Shared `rsi-common` types

### Scope and snapshots

```rust
pub struct CodegraphScope {
    pub project_id: Uuid,
    pub repository_id: CodeRepositoryId,
    pub workspace_id: CodeWorkspaceId,
}

pub struct CodegraphSnapshotRef {
    pub snapshot_id: CodeSnapshotId,
    pub git_commit: Option<String>,
    pub dirty: bool,
    pub revision_digest: String,
    pub observed_at: DateTime<Utc>,
    pub current: bool,
}

pub struct CodegraphLocation {
    pub relative_path: PathBuf,
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
    pub start_byte: u64,
    pub end_byte: u64,
    pub source_hash: String,
}
```

`CodegraphScope` appears in operator-facing responses and internal calls. Native
tool requests do not contain it.

### Bounds

```rust
pub struct CodegraphQueryBounds {
    pub max_depth: u16,
    pub max_nodes: u32,
    pub max_edges: u32,
    pub max_evidence_per_fact: u16,
    pub max_candidates: u16,
    pub max_output_tokens: u32,
    pub timeout_ms: u32,
}

pub struct CodegraphAppliedBounds {
    pub requested: CodegraphQueryBounds,
    pub effective: CodegraphQueryBounds,
    pub server_profile: String,
}

pub enum CodegraphTruncationReason {
    Depth,
    NodeCount,
    EdgeCount,
    EvidenceCount,
    CandidateCount,
    OutputTokens,
    Timeout,
    MemoryBudget,
}
```

All fields are required in internal typed requests. Public/native schemas may
default missing optional values, but the manager materializes a full bounds
object and clamps it to a server profile. There is no “unlimited” value.

### Result envelope

```rust
pub struct CodegraphQueryMeta {
    pub scope: CodegraphScope,
    pub snapshot: CodegraphSnapshotRef,
    pub query_id: Uuid,
    pub elapsed_ms: u64,
    pub applied_bounds: CodegraphAppliedBounds,
    pub truncated: bool,
    pub truncation_reasons: Vec<CodegraphTruncationReason>,
    pub continuation: Option<String>,
    pub index_health: CodegraphHealthSummary,
}

pub struct CodegraphNodeSummary {
    pub id: CodeNodeId,
    pub version_id: CodeNodeVersionId,
    pub kind: CodeNodeKind,
    pub language: CodeLanguage,
    pub display_name: String,
    pub qualified_name: String,
    pub resolution: ResolutionState,
    pub provenance: StructuralProvenance,
    pub confidence: f32,
    pub primary_location: Option<CodegraphLocation>,
}

pub struct CodegraphEdgeSummary {
    pub id: CodeEdgeId,
    pub version_id: CodeEdgeVersionId,
    pub source_id: CodeNodeId,
    pub target_id: CodeNodeId,
    pub relation: CodeRelationKind,
    pub direction: EdgeDirection,
    pub provenance: StructuralProvenance,
    pub confidence: f32,
    pub evidence: Vec<CodegraphEvidenceSummary>,
}
```

Result types never flatten parallel edges into one undocumented record. An
optional aggregate view includes `occurrence_count` and the underlying IDs.

## Operator RPC proposal

These verbs are regular daemon RPCs for TUI/operator use. They are **not**
automatically added to the agent-facing allowlist.

### Index lifecycle

| Verb | Request highlights | Response |
|---|---|---|
| `GetCodegraphStatus` | optional operator-selected project/workspace, validated server-side | health, scope, current snapshot, pending jobs, sizes, versions |
| `RequestCodegraphIndex` | validated workspace, mode `incremental|full`, reason, idempotency key | accepted job ID and prior/current state |
| `CancelCodegraphIndex` | job ID, reason | terminal or cancellation-requested state |
| `ListCodegraphSnapshots` | workspace, bounded page, clean/dirty filters | retained complete snapshots |

Index requests are idempotent. Equivalent pending reconciles coalesce. Full
rebuild requires an explicit mode but remains recoverable because the existing
complete DB/snapshot is served until replacement commits.

### Queries

| Verb | Distinct request fields | Response |
|---|---|---|
| `CodegraphSearch` | query, match mode, kind/language filters, optional snapshot, bounds | candidate nodes and exact locations |
| `GetCodegraphNode` | exact node ID, optional version/snapshot, federation inclusion | node, attributes, containment, evidence, authorized links |
| `CodegraphExplain` | exact node ID or resolution input, relation filters, bounds | identity explanation, candidates if ambiguous, selected relations/evidence |
| `CodegraphNeighbors` | exact seed IDs, incoming/outgoing/both, filters, bounds | nodes/parallel edges/boundary |
| `CodegraphPath` | exact source/target IDs, directed flag, relation/confidence filters, bounds | zero or bounded equal-shortest evidence-bearing paths |
| `CodegraphSubgraph` | seed IDs, filters, bounds | bounded induced subgraph |
| `CodegraphAffected` | changed node/file IDs, named profile/version, bounds | hop-labelled impact groups and paths |
| `CodegraphDiff` | from/to snapshots, filters, bounds | typed node/edge/evidence changes |

`GetCodegraphNode` keys on `node_id`; it does not accept a display label.
Resolution by name belongs to `CodegraphSearch`/`CodegraphExplain`.

### Proposed errors

Use stable machine codes with concise user messages:

| Code | Meaning |
|---|---|
| `codegraph_not_enabled` | workspace has no configured index |
| `codegraph_not_ready` | no complete snapshot is available |
| `codegraph_scope_denied` | project/workspace authorization failed |
| `codegraph_workspace_stale` | effective root is gone or tombstoned |
| `codegraph_snapshot_unknown` | snapshot absent, purged, or wrong workspace |
| `codegraph_node_unknown` | stable node absent in authorized snapshot |
| `codegraph_node_ambiguous` | name input resolves to multiple candidates |
| `codegraph_query_too_broad` | minimum safe bounds still exceed policy |
| `codegraph_query_timeout` | bounded deadline reached; partial result policy explicit |
| `codegraph_index_busy` | incompatible lifecycle operation already owns writer |
| `codegraph_index_cancelled` | index operation cancelled safely |
| `codegraph_integrity_error` | DB/snapshot invariant failed; last complete snapshot retained |

Partial query results are returned only when the operation contract can label
them safely; otherwise the error includes no misleading answer.

## Events

Add typed internal `DaemonEvent` variants and map them to the existing
`BusEvent { event_type: String, ... }` contract:

```rust
CodegraphIndexQueued {
    workspace_id,
    job_id,
    cause,
}
CodegraphIndexStarted {
    workspace_id,
    job_id,
    mode,
}
CodegraphIndexProgress {
    workspace_id,
    job_id,
    phase,
    files_done,
    files_total,
    queue_depth,
}
CodegraphIndexUpdated {
    workspace_id,
    job_id,
    snapshot_id,
    changed_files,
    deleted_files,
    nodes,
    edges,
    duration_ms,
}
CodegraphIndexFailed {
    workspace_id,
    job_id,
    phase,
    error_code,
    retryable,
}
CodegraphIndexCancelled {
    workspace_id,
    job_id,
}
CodegraphHealthChanged {
    workspace_id,
    prior,
    current,
    reason,
}
```

External event type strings:

```text
codegraph_index_queued
codegraph_index_started
codegraph_index_progress
codegraph_index_updated
codegraph_index_failed
codegraph_index_cancelled
codegraph_health_changed
```

Progress is rate-limited/coalesced. Payloads contain counts, identifiers, phase,
and bounded error summaries—not file contents, graph records, or unbounded path
lists.

## rsid manager and worker ownership

### `CodegraphManager`

Daemon-wide owner responsible for:

- workspace catalog resolution and project authorization;
- one worker handle per active code workspace;
- lifecycle/idempotency/coalescing;
- server bounds and query admission;
- snapshot/DB locator resolution;
- cross-domain link joins through typed domain services;
- event publication and health aggregation;
- quotas, tombstones, and restart recovery.

The manager is the only interface RPC/native-tool code calls. Callers never open
the derived database directly.

### `CodegraphWorker`

Single-writer workspace actor responsible for:

- watcher debounce/coalescing and bounded command queue;
- content inventory/hashing and dirty revision fingerprint;
- bounded parser tasks and cancellation;
- Cargo metadata execution/parsing;
- staging validation and transactional contribution replacement;
- snapshot/current-view integrity;
- SQL query execution and bounded induced-subgraph loading;
- local health/metrics.

Read queries may use a managed read pool against a complete WAL snapshot, but
the worker/manager controls connections and prevents reads of staging or
incomplete snapshots.

### Suggested command channel

```rust
enum CodegraphCommand {
    Reconcile { cause, mode, idempotency_key, reply },
    Cancel { job_id, reason, reply },
    Status { reply },
    Search { request, reply },
    Explain { request, reply },
    Neighbors { request, reply },
    Path { request, reply },
    Subgraph { request, reply },
    Affected { request, reply },
    Diff { request, reply },
    Shutdown,
}
```

The channel is bounded. Filesystem events never each allocate a command; they
update a bounded/coalesced pending set or flip `full_reconcile_required`.

## Effective-workspace resolution

The integration point must reuse session launch/rotation truth:

1. Begin with the caller session captured by the native registry or resolved by
   authenticated operator RPC.
2. Resolve mandatory `Session.working_dir` and its project via `ProjectIndex`.
3. If the session has a valid active `sandbox_root`, that is the effective
   codegraph root; otherwise use the canonical working directory fallback.
4. Resolve/register the corresponding `CodeWorkspaceId`.
5. Confirm the workspace catalog's `project_id` equals the caller's authorized
   project.
6. Capture scope in the manager handle used by the tool/query.

Rotation using the same sandbox gets the same workspace ID and newest complete
snapshot. A new sandbox gets a new workspace ID even when its initial commit
matches.

## Native Harness and CodexAppServer tools

### Common rules

- Read-only.
- Constructed with a scope-bound handle.
- No `project_id`, `workspace_id`, root, snapshot database, SQL, or path escape
  fields.
- Optional snapshot selector permits only `current`, an authorized retained
  snapshot ID, or `git_commit` resolved within the same workspace.
- Results serialize through one central token-budgeted renderer.
- Defaults are conservative; native callers may request smaller limits only.
- Federation links are excluded unless `include_federation: true`, and then
  only accepted, authorized links are returned by default.

### `rsi_codegraph_search`

```json
{
  "query": "dispatch_lc_action",
  "match": "exact|prefix|fts",
  "kinds": ["function", "method"],
  "languages": ["rust"],
  "snapshot": "current",
  "max_results": 20,
  "max_output_tokens": 1200
}
```

Returns node IDs, qualified names, kinds, resolution, confidence, exact
locations, snapshot, and truncation. `fts` only selects candidates; it does not
manufacture relationships.

### `rsi_codegraph_explain`

```json
{
  "node_id": "uuid",
  "incoming": true,
  "outgoing": true,
  "relations": ["calls", "declares", "implements"],
  "include_federation": false,
  "max_edges": 40,
  "max_evidence_per_fact": 2,
  "max_output_tokens": 2000
}
```

For convenience, a mutually exclusive `query` may be accepted instead of
`node_id`; if it is ambiguous, the tool returns candidates and performs no
explanation.

### `rsi_codegraph_traverse`

```json
{
  "operation": "neighbors|path|subgraph|affected|diff",
  "seed_node_ids": ["uuid"],
  "target_node_id": "uuid",
  "from_snapshot": "uuid",
  "to_snapshot": "current",
  "direction": "incoming|outgoing|both",
  "relations": ["calls", "depends_on"],
  "impact_profile": "rust-default-v1",
  "max_depth": 3,
  "max_nodes": 80,
  "max_edges": 160,
  "max_output_tokens": 3000
}
```

Fields are conditionally valid by operation and validated with
`deny_unknown_fields`-equivalent behavior. One compact tool avoids five nearly
identical registrations while retaining typed operation-specific Rust request
enums internally.

### Registry mapping

- Harness: register beside the active memory/control tools in
  `crates/rsid/src/session/harness/tools/`, capturing project, caller, effective
  workspace, and a `CodegraphManager` handle at tool construction.
- CodexAppServer: register through `ToolRegistry`, whose definitions already
  carry project/caller-bound services. Dynamic tool calls route to the same
  manager contract.
- The tool result schema is identical across both providers.

No native `index`, `cancel`, `link`, or `export` tool ships in MVP.

## TUI contract

### Entry and state

Proposed command: `:codegraph`. Proposed normal binding: `gC`, pending human
approval and a future `docs/keybindings.md` update.

```rust
struct CodegraphInspectorState {
    scope: CodegraphScopeSummary,
    snapshot: SnapshotSelection,
    health: CodegraphHealthSummary,
    query: String,
    match_mode: MatchMode,
    filters: CodegraphFilters,
    results: ListState,
    selected_node: Option<CodeNodeDetails>,
    relation_tab: RelationTab,
    evidence: ListState,
    diff: Option<CodegraphDiffState>,
    pending_request: Option<RequestGeneration>,
    error: Option<String>,
}
```

Requests use generation IDs so stale async responses cannot overwrite a newer
query/selection.

### Layout

- **Header:** project/workspace alias, abbreviated commit plus dirty digest,
  health badge, queue/pending counts.
- **Left pane:** search result or containment tree; dense kind/language/status
  glyphs and qualified names.
- **Upper-right:** selected node identity, attributes, provenance, confidence,
  snapshot validity, exact primary location.
- **Lower-right tabs:** incoming, outgoing, both, evidence, federation, diff.
- **Footer:** filters, effective caps, result/truncation counts, mode-specific
  keys, last index update/error.

### Interaction

Vim-normal navigation follows existing overlays. Proposed actions:

- `/` or insert action: query;
- `Tab`/`Shift-Tab`: pane or relation tab;
- `i`/`o`/`b`: incoming/outgoing/both;
- `e`: evidence detail;
- `f`: filters;
- `s`: snapshot chooser;
- `d`: diff endpoint/mode;
- `Enter`: expand/select;
- `g f`-style existing open-location action, if compatible: open source at
  exact span;
- `r`: operator reconcile after confirmation/clear mode indication;
- `Ctrl-C`: cancel active index job, not merely close overlay;
- `y`: copy bounded textual result.

Actual bindings must be reconciled with the complete overlay input map during
implementation. This document does not reserve keys.

### Health states

```text
disabled
empty
indexing
ready
ready_dirty_pending
stale_reconcile_required
degraded_last_snapshot_available
failed_no_snapshot
cancelled
workspace_missing
```

The UI always distinguishes “querying old complete snapshot while update is
pending” from “current.”

## rsi-graph integration

Implement a codegraph adapter behind the existing conceptual interfaces:

```rust
CodegraphRetrievalBackend: RetrievalBackend {
    source_type() -> ContextSourceType::FileIndex
}
```

The adapter translates a workflow's declared retrieval request into the same
bounded manager query and returns provenance-bearing
`RetrievalResult` metadata. Required adjustments likely include stronger typed
scope, snapshot, location, evidence, and bounds fields; those should be added
without redefining workflow `Node`/`Edge` as code entities.

Registration is explicit per authorized project/workspace. No global
`FileIndex` source is visible across projects.

## ContextPipeline policy

Default integration:

1. ContextPipeline may include a tiny “codegraph ready/stale/unavailable” tool
   capability descriptor.
2. It does not execute a graph query during ordinary prompt assembly.
3. User/agent tool calls perform on-demand retrieval.
4. A later workflow stage can declare a static bounded codegraph retrieval
   input, making token cost and snapshot explicit in the stage contract.
5. Any future auto-context experiment is feature-gated and benchmarked against
   the on-demand default.

This differs intentionally from current project memory auto-retrieval; the two
domains solve different context problems.

## Later CLI-backed provider contract

The safe route is a new guarded read-only control capability, not generic RPC:

```text
authenticated provider token
  -> caller session resolved server-side
  -> effective workspace bound server-side
  -> fixed read-only codegraph operation
  -> server caps and federation redaction
```

Before exposure:

- amend the explicitly advertised agent allowlist and discovery text;
- add token supersession/rotation/restart tests;
- test self/direct-child/Epic-lead observation scope as applicable;
- decide whether codegraph is self-only even for Epic leads;
- prevent target session/project/workspace fields from entering schemas;
- test malicious IDs, continuation tokens, relation filters, and federation
  pivots.

Until that slice is approved, CLI providers continue using source tools and
ordinary RSI mechanisms.

## Contract-level test obligations

- serde round trips and unknown-field rejection for all request/response types;
- request-bound clamping and truncation reason accuracy;
- directionality and parallel-edge preservation;
- exact `CodegraphLocation` byte/line correctness;
- ambiguous name returns candidates;
- snapshot/workspace mismatch denial;
- native schema snapshots prove absence of scope/write fields;
- Harness and CodexAppServer output equivalence;
- event mapping and payload bounds;
- stale async TUI response suppression;
- clickable location resolves under active sandbox, never canonical project by
  accident;
- federation authorization intersects both endpoint scopes.
