# Codegraph typed schema proposal

Status: proposed logical schema; no migration or production schema change

Canonical structural store: per-workspace `codegraph.sqlite`

Canonical federation store: main `rsi.db`

## Stage contract

### Inputs

- Findings `F-003` through `F-021`, especially structural/episodic separation,
  workspace scope, exact evidence, atomic replacement, revision history, and
  federation ownership.
- Existing RSI UUID, timestamp, project, session, observation, entity-card,
  memory-index, and SQLite conventions.
- Pinned Graphify node/edge/hyperedge/provenance behavior.

### Process

- Model stable identity separately from snapshot-specific state.
- Preserve directed parallel relationships and one-to-many evidence.
- Represent validity, supersession, deletion, and ambiguity explicitly.
- Keep LLM-derived links and summaries outside deterministic structural tables.

### Outputs

- Rust-level type vocabulary.
- Normalized structural, derived, operational, and federation table proposals.
- Identity, evidence, temporal, transaction, and deletion semantics.

### Verify

- Every structural relationship can retain all evidence without deduplication.
- Every current row is traceable to a complete snapshot and producer version.
- Replacing or deleting a file cannot leave a current contribution from that
  file.
- Federation authorization can be checked without trusting display labels or
  paths.

## Type vocabulary

The names below are logical contracts. Exact Rust module placement and SQL DDL
belong to an approved implementation plan.

### Strong identifiers

```rust
struct CodeRepositoryId(Uuid);
struct CodeWorkspaceId(Uuid);
struct CodeSnapshotId(Uuid);
struct CodeFileId(Uuid);
struct CodeFileVersionId(Uuid);
struct CodeNodeId(Uuid);
struct CodeNodeVersionId(Uuid);
struct CodeEdgeId(Uuid);
struct CodeEdgeVersionId(Uuid);
struct CodeEvidenceId(Uuid);
struct CodeHyperedgeId(Uuid);
struct CodeCommunityId(Uuid);
struct CodeSummaryId(Uuid);
struct CodegraphLinkId(Uuid);
struct CodeIndexJobId(Uuid);
```

Separate newtypes prevent accidental use of a session, workflow node,
observation, or graph entity where a code entity is required.

### Core enums

```rust
enum CodeLanguage {
    Rust,
    Toml,
    Markdown,
}

enum CodeNodeKind {
    Repository,
    Workspace,
    CargoWorkspace,
    Package,
    Crate,
    Target,
    Module,
    File,
    Trait,
    Struct,
    Enum,
    Union,
    TypeAlias,
    Impl,
    Function,
    Method,
    Field,
    Variant,
    Constant,
    Static,
    Macro,
    Test,
    Manifest,
    ManifestFeature,
    DocHeading,
    ResearchFinding,
    PlanItem,
    ExternalSymbol,
    UnresolvedSymbol,
}

enum CodeRelationKind {
    Contains,
    Declares,
    Defines,
    Imports,
    Reexports,
    References,
    Calls,
    InvokesMacro,
    Implements,
    ImplementsFor,
    HasMember,
    HasField,
    HasVariant,
    DependsOn,
    EnablesFeature,
    Tests,
    Documents,
    Overrides,
    Bounds,
}

enum StructuralProvenance {
    Extracted,
    Inferred,
    Ambiguous,
}

enum ProducerKind {
    TreeSitter,
    CargoMetadata,
    ManifestParser,
    MarkdownParser,
    DeterministicResolver,
    ScipImporter,
    RustAnalyzerImporter,
}

enum ResolutionState {
    Resolved,
    Ambiguous,
    Unresolved,
    External,
}

enum SnapshotRevisionKind {
    CleanCommit,
    DirtyWorktree,
    NonGitContent,
}

enum LinkTargetKind {
    Observation,
    EntityCard,
    MemoryChunk,
    Session,
    ResearchFinding,
    Plan,
    Commit,
}

enum CodegraphLinkRelation {
    Mentions,
    Discusses,
    DecidedAbout,
    ImplementsFinding,
    Invalidates,
    SupportedBy,
}

enum SemanticLinkProvenance {
    Manual,
    DeterministicReference,
    Imported,
    LlmCandidate,
    LlmAccepted,
}

enum LinkState {
    Candidate,
    Accepted,
    Rejected,
    Invalidated,
    TargetMissing,
    Deleted,
}
```

Enums are serialized with stable explicit strings. Unknown values fail closed
at write boundaries; forward-compatible read responses may use a versioned
unknown wrapper if needed.

## Identity model

### Repository identity

`CodeRepositoryId` is a daemon-minted UUID recorded in the main workspace
catalog. It is not derived solely from a remote URL, absolute path, or commit:
remotes can change, local repositories can have no remote, and paths expose
machine-specific information. Recognition may use validated VCS metadata and
operator confirmation, but aliases do not become identity.

### Workspace identity

`CodeWorkspaceId` is a daemon-minted UUID for one effective source root:

- canonical project checkout;
- managed git sandbox/worktree;
- other explicitly registered workspace.

It records the authorized RSI `project_id`, repository ID, canonicalized root
fingerprint, sandbox owner/lifecycle metadata, and cleanup state. Raw roots stay
daemon-side and are not accepted from native query callers.

### Snapshot identity

A complete snapshot is immutable. Its revision fingerprint is:

```text
hash(
  repository_id
  + workspace_id
  + git_head_or_none
  + sorted(indexed_relative_path, content_hash)
  + extraction_config_hash
  + producer_versions_hash
)
```

The DB also stores the VCS commit, dirty flag, dirty-content digest,
observation timestamps, parent snapshot, and completion state. Only `Complete`
snapshots may be pointed to by `current_snapshot_id`.

### Stable node identity

`CodeNodeId` identifies a logical entity across snapshots within one
repository:

```text
UUIDv5(
  repository_namespace,
  language + node_kind + canonical_symbol_identity
)
```

Examples:

- Rust item: crate disambiguator + canonical module path + item kind/name;
- impl: implemented trait identity or `_` + target type identity + normalized
  generic discriminator;
- Cargo package: Cargo package ID when stable within resolved metadata;
- Markdown finding: normalized repository-relative document path + `F-###`;
- file: normalized repository-relative logical path.

Absolute workspace paths and byte offsets are forbidden in stable node IDs.
Anonymous/local items use a versioned syntactic-ancestry discriminator. If a
move/rename makes identity uncertain, create a new identity plus a
supersession/candidate-equivalence record; never merge by label.

`CodeNodeVersionId` captures snapshot-specific name, location, attributes,
resolution, and content digest. A stable node may have at most one current
version per complete snapshot.

### Edge identity

`CodeEdgeId` identifies a logical directed relationship occurrence. Its
identity includes source, target, relation, a versioned semantic qualifier, and
a stable occurrence discriminator. Two calls at different call sites remain
distinct edges. One call occurrence with multiple supporting mechanisms remains
one edge with multiple evidence records.

An aggregate query view may group equivalent `(source,target,relation)` edges,
but canonical rows are never destructively deduplicated.

## Exact source locations

```rust
struct SourceSpan {
    file_version_id: CodeFileVersionId,
    start_byte: u64,       // zero-based, inclusive
    end_byte: u64,         // zero-based, exclusive
    start_line: u32,       // one-based
    start_column: u32,     // one-based Unicode scalar display column contract
    end_line: u32,         // one-based
    end_column: u32,       // one-based, exclusive
    source_hash: Hash32,
}
```

Tree-sitter point columns are byte columns. The extractor must preserve those
raw points if useful, but the public clickable location contract is explicitly
defined and tested. A span is valid only against the named file version and
source hash. Responses include repository-relative display paths; the TUI
resolves them against the authorized effective root.

## Structural tables in `codegraph.sqlite`

The following columns emphasize ownership and invariants, not final SQL syntax.

### Metadata and scope

| Table | Key fields | Purpose |
|---|---|---|
| `meta` | `schema_version`, `created_at` | Derived DB version and format metadata |
| `repository` | `repository_id`, `identity_version` | Copy of authorized namespace metadata needed for offline integrity |
| `workspace` | `workspace_id`, `repository_id`, `scope_fingerprint` | Single owning workspace; DB must not mix unrelated roots |
| `snapshots` | `snapshot_id`, `parent_id`, revision fields, state, timestamps, hashes | Immutable observed revisions |
| `current_snapshot` | singleton `snapshot_id` | Points only to a complete snapshot |
| `producer_versions` | producer, version, rule/config hashes | Reproducibility and invalidation |

### File contribution ownership

| Table | Key fields | Purpose |
|---|---|---|
| `files` | `file_id`, repository-relative logical path, language | Stable file identity |
| `file_versions` | `file_version_id`, `file_id`, `snapshot_id`, content hash, size, line index | Snapshot-specific content metadata |
| `contribution_sets` | `contribution_id`, `file_version_id`, producer/version, state | Atomic ownership unit for all facts emitted from one input |
| `contribution_inputs` | contribution, dependent file/manifest/config hash | Cross-file invalidation inputs |

Every node version, edge version, hyperedge version, and evidence record has a
non-null owning `contribution_id` or an explicit deterministic resolver job
whose input contribution set is recorded.

### Nodes

| Table | Key fields | Purpose |
|---|---|---|
| `nodes` | `node_id`, repository ID, language, kind, canonical identity, created snapshot | Stable entity |
| `node_versions` | `node_version_id`, node ID, snapshot ID, name/qualified name, resolution, span, attributes hash, validity | Snapshot-specific state |
| `node_attributes` | node version, typed key/value | Sparse versioned attributes with a registry, not arbitrary ungoverned JSON |
| `node_supersessions` | old node, new node, reason, evidence, confidence, snapshots | Explicit rename/move/reidentification history |

Typed high-use attributes should be columns. A constrained attribute table
handles extractor-specific values; keys are versioned and registered.

### Directed edges and evidence

| Table | Key fields | Purpose |
|---|---|---|
| `edges` | `edge_id`, repository ID, source node, target node, relation, qualifier, occurrence key | Stable directed relationship occurrence |
| `edge_versions` | edge version, edge ID, snapshot, provenance, confidence, resolution, validity | Snapshot-specific relationship state |
| `evidence` | evidence ID, snapshot, contribution, producer, rule, source span, fact hash, excerpt hash | One exact justification |
| `edge_evidence` | edge version, evidence ID, role, ordinal | Many evidence records per edge version |
| `node_evidence` | node version, evidence ID, role, ordinal | Many evidence records per node version |

Confidence is constrained to `0.0 <= confidence <= 1.0`. `Extracted` normally
uses `1.0` only when parser recognition and identity are exact. An
`Ambiguous` edge targets an unresolved/candidate node or has explicit candidate
records; it may not masquerade as one resolved target.

### Hyperedges

| Table | Key fields | Purpose |
|---|---|---|
| `hyperedges` | hyperedge ID, kind, snapshot/version, provenance, confidence, qualifier | First-class n-ary fact |
| `hyperedge_members` | hyperedge ID, node ID, role, ordinal | Role-labelled ordered membership |
| `hyperedge_evidence` | hyperedge version, evidence ID, role | Supporting evidence |

Example:

```text
RustImplFact {
  impl_block: Impl#1,
  trait: Display,
  target_type: SessionStatus,
  generic_context: T,
}
```

Convenience binary edges may be derived into query views, but the hyperedge is
the lossless source.

### Derived annotations

| Table | Key fields | Purpose |
|---|---|---|
| `communities` / `community_members` | snapshot, algorithm/version/config, score | Invalidatable derived clustering |
| `node_summaries` | node version, model/prompt/version, input evidence hash, text, token count, state | Provenance-bearing derived summaries |
| `fts_nodes` | contentless/external-content FTS fields | Optional natural-language seed selection |
| `graph_metrics` | snapshot, algorithm/version, bounded metrics | Optional diagnostics |

These tables are excluded from structural identity and can be dropped/rebuilt
without changing graph correctness.

### Operations

| Table | Key fields | Purpose |
|---|---|---|
| `index_jobs` | job ID, cause, requested/started/finished, state, cancellation, counts | Durable job audit and restart recovery |
| `index_health` | workspace, current snapshot, queue/pending counts, stale/reconcile flags, last error | Status surface |
| `staging_*` | job/contribution-scoped candidates | Never served; cleared/recovered after crash |
| `query_audit` | optional metadata only | Duration, caps, counts, truncation; no source text by default |

## Current and historical views

Canonical writes are snapshot-versioned. Query helpers expose:

- `current_nodes`, `current_edges`, `current_hyperedges`: latest complete
  snapshot only;
- `snapshot_*`: exact immutable snapshot;
- `history_*`: versions and validity intervals;
- `relationship_aggregate`: query-time grouping without deleting occurrences;
- `current_evidence`: evidence valid for current facts.

Validity uses snapshot boundaries, not mutable wall-clock guesses:

```text
valid_from_snapshot_id  inclusive
valid_to_snapshot_id    exclusive, nullable while current
observed_at             RFC3339 nanosecond timestamp
superseded_by_id        optional explicit successor
deleted_at              logical deletion/audit marker where appropriate
```

Immutable snapshots can make intervals derivable; retaining explicit validity
indexes supports efficient “current” and history queries. Implementation must
choose one authoritative representation and assert equivalence rather than
allowing both to drift.

## Atomic contribution replacement

For a job with changed set `C` and deleted set `D`:

1. Hash and parse outside the write transaction into job-scoped staging.
2. Validate every staged identity, span, enum, confidence, evidence reference,
   and contribution ownership.
3. Begin one write transaction.
4. Insert the candidate snapshot as incomplete.
5. Copy forward unchanged contributions by immutable reference or snapshot
   mapping.
6. End validity for current contributions owned by `C ∪ D`.
7. Install staged contributions for `C`.
8. Re-run deterministic resolvers whose recorded inputs intersect `C ∪ D`.
9. Assert no current evidence references absent file versions; no current edge
   endpoint lacks a current node version unless explicitly external/unresolved.
10. Mark snapshot complete, update current pointer/health, and commit.

If any step fails, rollback preserves the previous complete current snapshot.
Files deleted during parse cause the job to restart/coalesce rather than commit
a mixed observation.

## Federation tables in main `rsi.db`

### Workspace catalog

`codegraph_workspaces` is a small durable catalog:

| Field | Meaning |
|---|---|
| `workspace_id` | daemon UUID |
| `repository_id` | durable repository namespace |
| `project_id` | mandatory authorization owner |
| `workspace_kind` | canonical checkout, managed sandbox, registered external |
| `root_fingerprint` | non-reversible identity/check value |
| `sandbox_session_id` | optional owner/lifecycle association |
| `db_locator` | daemon-internal relative locator, never API-visible |
| `cleanup_state` | active, stale, archived, purged |
| timestamps | RFC3339 nanosecond values |

Sandbox cleanup must update the catalog and derived DB lifecycle atomically with
existing sandbox tombstone conventions where the implementation touches both.

### Cross-domain link ledger

`codegraph_links`:

| Field | Meaning |
|---|---|
| `link_id` | UUID |
| `project_id`, `workspace_id`, `repository_id` | mandatory authorization scope |
| `code_node_id` | stable structural endpoint |
| `code_node_version_id`, `snapshot_id` | optional version-specific endpoint |
| `target_kind`, `target_id` | typed episodic/document/commit endpoint |
| `relation`, `direction` | semantic relationship |
| `provenance`, `producer`, `producer_version` | how the link was created |
| `confidence` | `0.0..=1.0` |
| `state` | candidate/accepted/rejected/invalidated/target-missing/deleted |
| `valid_from`, `valid_to`, `superseded_by`, `deleted_at` | temporal/deletion semantics |
| `target_fingerprint` | required for rebuildable memory chunks; optional otherwise |
| timestamps/actor | audit |

`codegraph_link_evidence` stores one-to-many evidence:

- evidence kind: exact document span, observation ID, manual note, model output
  digest, deterministic reference, commit diff;
- source identity and optional span;
- content hash/digest rather than unrestricted copied source;
- producer/model/prompt/rule version;
- confidence contribution and ordinal.

The target uses a typed `(target_kind, target_id)` rather than a polymorphic
foreign key. The manager validates target existence and same-project
authorization on writes and reads. Durable target tables retain native foreign
keys in domain-specific companion tables if an implementation later needs
stronger integrity; the logical contract remains typed.

### Relation semantics

| Relation | Direction | Default provenance | Validity/deletion |
|---|---|---|---|
| `MENTIONS` | target mentions code node | deterministic reference or accepted semantic | valid while evidence document/record and referenced identity remain |
| `DISCUSSES` | episodic target discusses code node | manual or LLM candidate/accepted | invalidated when evidence is withdrawn; target deletion soft-invalidates |
| `DECIDED_ABOUT` | decision-bearing target decides code behavior/API | manual or accepted semantic | remains historical; later decision supersedes, not erases |
| `IMPLEMENTS_FINDING` | plan/commit/code node implements `F-###` | deterministic plan syntax or manual | versioned by plan/commit; invalidated if finding/link is withdrawn |
| `INVALIDATES` | target invalidates code version or prior link | manual/deterministic change rule | historical fact remains; endpoint version may cease current |
| `SUPPORTED_BY` | codegraph link/claim supported by target | deterministic/manual/accepted semantic | invalidated or target-missing without silent replacement |

Candidate LLM links are excluded from default query responses. Accepted LLM
links remain semantically labelled and never become `Extracted` structure.

## Dreamer `source_ids`

Keep `Observation.source_ids: Vec<Uuid>` observation-specific. It expresses
Dreamer derivation lineage among observations and has existing callers that
assume that meaning. A heterogeneous generalization would:

- destroy referential clarity;
- make authorization target-dependent inside a bare UUID list;
- provide no relation/provenance/confidence/evidence fields;
- conflate observation derivation with code discussion.

The separate ledger can link an observation to any code node and can itself be
supported by the observation lineage when needed.

## Deletion and rebuild semantics

- **Structural DB purge:** derived graph can be deleted and rebuilt. Durable
  links remain but resolve as `code_target_unavailable` until the same stable
  node identity returns or a human supersedes them.
- **File deletion:** ends current node/edge/evidence validity atomically.
  Stable historical records remain per retention policy.
- **Node rename/move:** never retarget links by display name. An explicit
  deterministic/manual supersession can carry a link forward while preserving
  history.
- **Observation/card/session deletion:** follow owning domain logical-delete
  semantics; link becomes invalidated/deleted according to policy.
- **Memory chunk reindex:** compare target fingerprint. Missing exact chunk
  becomes `TargetMissing`; similarity cannot substitute.
- **Plan/research edit:** stable `F-###` remains target identity when the same
  document lineage is known; changed evidence versions the link.
- **Commit:** immutable; repository rewrite is represented as target missing or
  superseded, not mutated.

## Integrity constraints

At minimum:

- one repository and one workspace owner per derived DB;
- one current complete snapshot;
- lowercase canonical UUID strings and RFC3339 nanosecond timestamps;
- non-empty repository-relative normalized paths; no `..`, absolute, or NUL;
- exact span bounds within the recorded file size and line index;
- every current node/edge/hyperedge version belongs to the queried snapshot;
- edge source/target repository matches unless explicitly external or a future
  authorized cross-repo edge;
- confidence in range and provenance/producer combination valid;
- no LLM producer allowed in structural fact tables;
- all current facts have at least one evidence record unless their kind is a
  declared synthetic container;
- all response queries apply project/workspace scope predicates before
  traversal;
- link scope matches both endpoints' project authorization.

## Schema evolution

Structural DB migrations may rebuild from source when safer than in-place
conversion. The main `rsi.db` catalog/link tables still require the repository's
normal versioned migration and `user_version` bump. A design document does not
authorize either.

Producer upgrades are snapshot-visible. Changing a grammar, extractor,
identity rule, Cargo invocation, or resolver invalidates affected
contributions. Identity-rule changes require an explicit mapping/supersession
strategy or a clean namespace version rather than silently reusing old IDs.
