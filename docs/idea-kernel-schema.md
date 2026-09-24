# Idea kernel schema, event CAS, and controller transfer (D01–D03)

D01 adds the additive identity and persistence kernel. D02 activates its
internal operator-only event-plus-projection transaction path. D03 adds the
internal durable controller-transfer and controller-mutation boundary. These
slices do not migrate legacy records or expose an Idea writer through RPC,
agent verbs, native tools, StoreWorker, or the TUI.

```text
Project
  └─ Capture (immutable content digest/reference)
       └─ Idea (current bounded projection)
            ├─ IdeaEvent (append-only history)
            ├─ IdeaRelationship ──> Idea
            ├─ IdeaCollectionMembership <── IdeaCollection
            └─ IdeaCompatibilityMapping <── explicit legacy reference
```

## Shared types

All IDs are UUIDs and all timestamps are UTC `DateTime` values. Persistence
accepts only lowercase canonical UUID text and the exact UTC RFC3339
nanosecond form emitted by
`to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)`.

- `Sha256Digest` is exactly `sha256:` followed by 64 lowercase hexadecimal
  characters.
- `ContentAddressedRef` is exactly `cas://sha256/` followed by the same
  64-character digest. It must match the Capture's digest.
- `GenesisSpan` is `start`, `end`, and a `Sha256Digest`, with
  `0 <= start < end`.
- `Capture` contains `id`, `project_id`, `creator_kind`, `creator_id`,
  `captured_at`, `source_kind`, `raw_content_digest`, `storage_policy_id`, and
  `content_ref`.
- `Idea` contains `id`, `project_id`, `slug`, optional `sigil`,
  `genesis_capture_id`, the optional all-or-none `genesis_span_start`,
  `genesis_span_end`, and `genesis_span_digest`, `title`, `description`,
  `portfolio_summary`, `lifecycle`, `stage`, `priority`, `autonomy_policy`,
  `integration_target_ref`, optional `program_template_policy_id`, optional
  `current_controller_session_id`, `controller_epoch`, `row_version`,
  `next_event_sequence`, `created_at`, `updated_at`, optional `terminal_at`,
  and optional `superseded_at`.
- `IdeaEvent` contains `id`, `project_id`, `idea_id`, positive `sequence`,
  `event_type`, `actor_kind`, `actor_id`, optional controller identity/epoch,
  expected/resulting row versions, `idempotency_key`, `occurred_at`, structured
  `payload`, and typed artifact/evidence digest vectors.
- `IdeaRelationship` contains identity, same-project source/target Ideas,
  kind, creation event/time, and paired optional removal event/time.
- `IdeaCollection` contains identity, project, slug, name, optional
  description, creation/update times, and optional retirement time.
- `IdeaCollectionMembership` has its own identity, project, collection, Idea,
  addition time, and optional removal time.
- `IdeaCompatibilityMapping` contains explicit legacy source kind/UUID,
  at most one same-project Idea or collection target, status, structured
  provenance, optional disposition, timestamps, and optional mapping time.
- `IdeaWithGenesis` contains exactly one `Idea` and one `Capture`.

Text bounds use UTF-8 bytes in both layers: Rust uses `String::len()` and
SQLite uses BLOB length. Idea/collection slug is 1–128 bytes, sigil is 1–64
bytes when present, title and collection name are 1–512 bytes, Idea and
collection descriptions are at most 65,536 bytes, and portfolio summary is at
most 2,048 bytes. Controller and row versions are non-negative; next event
sequence is positive.

## Exact enum strings

| Type | Values |
|---|---|
| `IdeaActorKind` | `operator`, `session`, `system` |
| `CaptureSourceKind` | `operator_input`, `session_artifact`, `imported_artifact`, `legacy_reference` |
| `IdeaLifecycle` | `Open`, `Parked`, `Completed`, `Abandoned`, `Superseded` |
| `IdeaStage` | `Captured`, `Shaping`, `Researching`, `Planned`, `Implementing`, `Integrating`, `Verifying`, `Released` |
| `AutonomyPolicy` | `CaptureOnly`, `Research`, `PlanAndWait`, `Sandbox`, `IntegrateIdeaBranch`, `PromoteProjectTarget`, `ExternalEffects` |
| `IdeaRelationshipKind` | `depends_on`, `supersedes`, `derived_from` |
| `LegacyIdeaSourceKind` | `session_group`, `session_epic`, `session_label`, `session_child` |
| `IdeaCompatibilityStatus` | `pending`, `mapped`, `blocked`, `excluded` |
| `IdeaEventType` | `created`, `decision_recorded`, `projection_changed`, `lifecycle_transitioned`, `stage_transitioned`, `autonomy_changed`, `scope_changed`, `controller_reserved`, `controller_assigned`, `controller_released`, `question_asked`, `question_answered`, `issue_linked`, `artifact_sealed`, `finding_recorded`, `verdict_recorded`, `program_transitioned`, `gate_transitioned`, `integration_recorded`, `release_recorded`, `relationship_changed`, `split`, `superseded`, `failed`, `abandoned` |

Values are case-sensitive. There are no aliases. `Blocked` and
`AwaitingApproval` are not Idea stages.

## SQLite V75

V75 creates all seven tables, indexes, triggers, and `PRAGMA user_version = 75`
inside one unchecked transaction. The version bump is the last statement.
Every DDL statement is additive and guarded with `IF NOT EXISTS`. No V75
statement updates, deletes, backfills, or installs a trigger on a legacy table.

### `captures`

Columns: `id`, `project_id`, `creator_kind`, `creator_id`, `captured_at`,
`source_kind`, `raw_content_digest`, `storage_policy_id`, `content_ref`.

`project_id` references `projects(id) ON DELETE RESTRICT`; `(id, project_id)`
is unique. `idx_captures_project_captured_at` indexes
`(project_id, captured_at, id)`. Canonical digest/reference, exact enum, and
non-empty identity/policy checks are enforced. `captures_immutable_update`
rejects every update and `captures_no_delete` rejects every delete.

### `ideas`

Columns: `id`, `project_id`, `slug`, `sigil`, `genesis_capture_id`,
`genesis_span_start`, `genesis_span_end`, `genesis_span_digest`, `title`,
`description`, `portfolio_summary`, `lifecycle`, `stage`, `priority`,
`autonomy_policy`, `integration_target_ref`, `program_template_policy_id`,
`current_controller_session_id`, `controller_epoch`, `row_version`,
`next_event_sequence`, `created_at`, `updated_at`, `terminal_at`,
`superseded_at`.

The project FK is restricted. `(genesis_capture_id, project_id)` references
the same-project Capture. The optional controller references `sessions(id)
ON DELETE RESTRICT`. `(id, project_id)` and `(project_id, slug)` are unique.
Span fields are all absent or all present and valid. Indexes are
`idx_ideas_project_state(project_id, lifecycle, stage, priority, id)` and
`idx_ideas_controller_session(current_controller_session_id)`.
`ideas_genesis_immutable` protects identity/project/genesis while allowing
later event-backed projection updates; its span check is explicitly all-`NULL`
or all-non-`NULL`, so SQLite cannot admit a partial span. `ideas_no_delete`
rejects deletion.

### `idea_events`

Columns: `id`, `project_id`, `idea_id`, `sequence`, `event_type`, `actor_kind`,
`actor_id`, `controller_session_id`, `controller_epoch`,
`expected_row_version`, `resulting_row_version`, `idempotency_key`,
`occurred_at`, `payload_json`, `artifact_digests_json`,
`evidence_digests_json`.

The Idea FK is same-project and restricted; the optional controller session FK
is restricted. Unique keys are `(id, project_id)`,
`(id, idea_id, project_id)`, `(idea_id, sequence)`, and
`(idea_id, idempotency_key)`. JSON, exact enum, non-empty, sequence, and
version checks apply. `idx_idea_events_idea_sequence` indexes
`(idea_id, sequence)`. `idea_events_append_only_update` and
`idea_events_no_delete` make history append-only.

### `idea_relationships`

Columns: `id`, `project_id`, `source_idea_id`, `target_idea_id`, `kind`,
`created_event_id`, `created_at`, `removed_event_id`, `removed_at`.

Both Ideas are same-project and distinct. Creation/removal events are bound to
the source Idea and project. Removal event/time are paired. Directional indexes
are `idx_idea_relationships_source` and `idx_idea_relationships_target`;
`idx_idea_relationships_active` permits one active edge for a source, target,
and kind. `idea_relationships_identity_immutable` protects identity and
creation fields while allowing the first paired tombstone transition. Once
removed, removal event/time evidence cannot be cleared or rewritten;
re-adding the same edge creates a new row. `idea_relationships_no_delete`
rejects deletion.

### `idea_collections`

Columns: `id`, `project_id`, `slug`, `name`, `description`, `created_at`,
`updated_at`, `retired_at`.

The project FK is restricted. `(id, project_id)` and `(project_id, slug)` are
unique. `idx_idea_collections_project` indexes project/retirement/slug/id.
`idea_collections_no_delete` requires retirement rather than raw deletion.

### `idea_collection_memberships`

Columns: `id`, `project_id`, `collection_id`, `idea_id`, `added_at`,
`removed_at`.

Composite FKs enforce same-project Collection and Idea ownership.
`idx_idea_memberships_collection` and `idx_idea_memberships_idea` serve both
directions. Partial unique index `idx_idea_memberships_active` allows one
active `(collection_id, idea_id)` while preserving remove/re-add history.
`idea_memberships_identity_immutable` protects identity/addition fields and
allows only the first removal timestamp transition. A removal timestamp
cannot then be cleared or rewritten; re-add history uses a new row.
`idea_memberships_no_delete` requires tombstoning. Collections carry no
lifecycle, readiness, controller, lead, topology, or spawn authority.

### `idea_compatibility_mappings`

Columns: `id`, `project_id`, `legacy_source_kind`, `legacy_source_id`,
`idea_id`, `collection_id`, `status`, `provenance_json`, `disposition`,
`created_at`, `updated_at`, `mapped_at`.

Project and optional target FKs are restricted and same-project. At most one
target may exist. `mapped` requires exactly one target and `mapped_at`; every
other status requires no target and no `mapped_at`.
`(legacy_source_kind, legacy_source_id)` is unique. Indexes are
`idx_idea_compatibility_project_status`, `idx_idea_compatibility_idea`, and
`idx_idea_compatibility_collection`. `idea_compatibility_source_immutable`
protects source identity and creation; `idea_compatibility_no_delete`
preserves mapping evidence. The polymorphic legacy source is deliberately not
an FK and never changes its source row.

## D02 transaction kernel

The materialized `ideas` row is authoritative for bounded current reads.
`idea_events` is the append-only authoritative semantic history. Current reads
never rebuild the projection from event history, and no poll or RPC path lists
history.

Production creation is itself the first semantic mutation:

| Value | Committed value |
|---|---|
| Conceptual pre-create row version | `0` |
| `created` event sequence | `1` |
| `created` event version pair | `0 -> 1` |
| Idea `row_version` | `1` |
| Idea `next_event_sequence` | `2` |

Creation uses one UTC RFC3339-nanosecond timestamp for the Idea, event, and
optional relationship. Every later successful action consumes sequence `S`
and version `V`, emits event `S` with `V -> V + 1`, and commits projection
counters `row_version = V + 1` and `next_event_sequence = S + 1`. Sequences
and version pairs are therefore unique and contiguous. A stale version remains
stale after an ABA field change.

Each write begins an SQLite `IMMEDIATE` transaction. Replay is resolved before
current-version comparison. Authoritative scope and prerequisites are loaded
inside the transaction; the projection update predicates on Idea, project,
row version, and next sequence; one event is inserted; relationship changes
are written; and commit occurs last. Any constraint failure, injected
write-boundary failure, contention outcome, or commit failure rolls the whole
operation back. No counter, timestamp, slug, UUID, relationship tombstone, or
idempotency key is consumed by a rolled-back transaction.

### Internal authority

`IdeaControlHandle` is crate-internal and binds one `Store`, project UUID,
`IdeaActorKind::Operator`, and validated operator identity at construction.
Request types contain no project, Idea, actor, creator, session, controller,
epoch, event ID/type, timestamp, resulting version, fingerprint, or dedup
authority field. Session and System construction is unavailable in D02.

There is no D02 operator RPC, attributed RPC, `AGENT_VERBS`/`READ_VERBS`
entry, native `rsi_control` tool, StoreWorker command, generic
`AppendIdeaEvent`, or public controller grant. D03 must extend this same
handle after it implements server-resolved controller/epoch fencing rather
than adding a second transaction kernel.

### Closed semantic action catalog

D02 creation always enters `Open` / `Captured`. Later requests contain exactly
one typed action:

- change projection: optional sigil, title, description, portfolio summary,
  and priority patches, with at least one actual change;
- change scope: integration target and/or nullable program-template policy;
- change autonomy;
- park, reopen, or abandon with a non-empty explicit reason;
- transition stage;
- add or remove `depends_on` or `derived_from`;
- accept one or more sorted, unique replacement Ideas as supersession.

### D04 `link_issue` semantic operation

V77 adds the strict `link_issue` operation to the D02 envelope. It contains a
non-nil `issue_id`, optional `source_event_id`, and optional canonical
`source_finding_ref`. It uses event type `issue_linked`, an empty artifact
digest vector, and either no evidence digests or exactly the SHA-256 digest
embedded in the finding reference. The operator-bound transaction resolves
exact replay before stale-version or link-once checks, advances the Idea
version/sequence without changing projection fields, updates the Issue's
one-time linkage tuple, and appends the event in one transaction.

The event transfers no controller, Session, or Idea mutation authority to the
Issue creator. Agent/native Issue creation remains create-only and unlinked.

Requests cannot choose an event type or supply arbitrary event payload. The
kernel derives exactly one of `projection_changed`, `scope_changed`,
`autonomy_changed`, `lifecycle_transitioned`, `abandoned`,
`stage_transitioned`, `relationship_changed`, `superseded`, or D04's
`issue_linked`. Controller, question, artifact seal, finding, verdict,
ProgramRun, gate, integration, release, failure, and split events remain
reserved for later typed contracts; `issue_linked` is closed by the D04
operator-only contract above.

Lifecycle is fail-closed:

| From | To | D02 result |
|---|---|---|
| `Open` | `Parked` | allowed only through reasoned `Park` |
| `Parked` | `Open` | allowed only through reasoned `Reopen` |
| `Open` or `Parked` | `Abandoned` | reason required; sets `terminal_at` |
| `Open` or `Parked` | `Superseded` | only accepted supersession; sets both terminal timestamps |
| `Open` or `Parked` | `Completed` | prerequisite unavailable until D05/D10 |
| any | same state | no semantic change; no write |
| terminal | different state | terminal lifecycle rejection |
| any other pair | — | invalid transition |

Stage order is `Captured < Shaping < Researching < Planned < Implementing <
Integrating < Verifying < Released`. `Captured -> Shaping` is the only D02
forward move and requires lifecycle `Open`. Other forward moves, including
skips, fail because their server-verified seal/gate is not available yet.
Regressions require lifecycle `Open` and a non-empty reason. A parked Idea
must be reopened first; terminal Ideas reject stage changes; same-stage input
is a no-op. Request digests and caller booleans never prove completion or a
forward gate.

### Canonical replay envelope and deterministic IDs

Every D02 event payload is compact canonical JSON tagged
`rsi.idea.semantic-request/v1`. It includes the typed operation, bound project
and Idea, expected version, bound operator identity, and sorted/unique
artifact and evidence digest vectors. Object keys are recursively sorted.
Validated text is preserved byte-for-byte; trimming is only an emptiness
check. Idempotency keys contain 1–128 bytes and no NUL, index the envelope,
and are not themselves part of it.

Generated UUIDs, timestamp, sequence, resulting version, readback projection,
and dedup status are excluded from semantic comparison. UUIDv5 deterministically
derives the create Idea from project/key, the event from Idea/key, and
relationships from event/kind/target. Exact replay after process or Store
reopen returns the original event, historical committed projection, and
affected relationship rows with only `deduplicated=true`; reconstruction
reads semantic history in pages capped at 256. A changed actor, scope,
expected version, action member, target/kind, digest, or create field under
the same key returns `IdempotencyConflict` before writes.

Stored envelope, UUID, timestamp, enum, JSON, digest, event-type, actor,
controller, scope, or version mismatch is `CorruptStoredEvent`. It never
falls through to a replay miss or default value.

### Relationship semantics

Directions are exact:

- `source --depends_on--> target`: source readiness depends on target;
- `child --derived_from--> origin`: child/extraction points to its origin;
- `historical_source --supersedes--> current_replacement`: the historical
  source points to each accepted current Idea.

Generic add/remove supports only `depends_on` and `derived_from`. It requires
same-project, distinct endpoints, source CAS, no duplicate active edge, and no
active per-kind cycle. Removal writes the first event-bound tombstone; it
cannot be cleared or rewritten. Re-addition creates a new event and row.

Child creation may name one existing same-project origin. Child projection,
sequence-1 event, and derived-from edge commit atomically. Accepted
supersession validates one or more distinct same-project nonterminal
replacements, rejects active cycles, terminalizes the historical source,
emits one source event, and inserts all accepted source-to-replacement edges
atomically. Active supersedes edges are durable acceptance and cannot be
tombstoned through generic mutation.

Raw split, destructive merge, reverse supersedes direction, pending
acceptance, and multi-Idea split/merge batches are not representable in D02.
A later plan must own their cross-aggregate transaction contract.

### Bounded history and errors

Internal history reads require `after_sequence >= 0`; limit defaults to 100
and must be `1..=256`. SQL reads `limit + 1` in ascending sequence order,
returns at most the requested limit, and emits the last returned sequence as
the next cursor only when another row exists.

Stable internal categories distinguish invalid request/forbidden actor,
missing Idea/Capture/relationship, project mismatch, stale version,
idempotency conflict, no-op, lifecycle/stage/prerequisite fences,
relationship conflict, SQLite contention, corrupt stored event, and
unexpected storage failure. Raw SQLite text never makes an authority decision.

## Protected genesis boundary

Capture stores only a lowercase SHA-256 digest, a matching opaque
content-addressed reference, and a policy identifier. It has no plaintext,
prompt, excerpt, transcript, encrypted blob, key/nonce, provider, model, token,
cost, retry, sandbox, or context-window field. An optional genesis span stores
byte offsets and another digest, never excerpt text. D02's typed envelope
retains the reference/digest-only payload boundary.

`HUMAN-PROVIDER-DATA` remains blocked. The digest/reference contract grants no
permission to transfer provider-family data.

## Bounded operator read

`GetIdea` accepts `GetIdeaParams { idea_id }` and returns `IdeaWithGenesis`.
The store executes one Idea query and one same-project Capture query. It does
not replay events or list relationships or memberships. An unknown ID returns
the existing typed store-not-found error.

`GetIdea` is absent from both the agent `AGENT_VERBS` and `READ_VERBS`
allowlists. Any session-token-attributed request is denied before store access.
D01 adds no `CreateIdea`, update, event, controller, relationship, collection,
or compatibility mutation RPC or native agent-control capability.

## Compatibility matrix

| Existing concept | D01 disposition |
|---|---|
| Session and `SessionKind` | Unchanged; no `SessionKind::Idea` |
| Group and Epic | Unchanged containers; no reinterpretation or mapping rows |
| Workflow | Unchanged pipeline artifact/definition; no rename |
| Issue | Unchanged concrete obligation; D04 owns future linkage |
| Labels and `session_groups` | Unchanged; D15 owns explicit mapping population |
| `parent_id` hierarchy | Unchanged |
| `continued_from` rotation lineage | Unchanged |
| `lead_session_id` | Unchanged Epic-lead meaning |
| topology/workflow inheritance | Unchanged |
| rotation and provider sessions | Unchanged |

Project deletion is restricted when D01 rows exist. The existing transactional
`delete_project` session-pointer clearing therefore rolls back if the final
project deletion is restricted. Projects without D01 rows retain their
existing behavior.

## Slice ownership

- D01 owns identity types, additive schema, strict reads, and operator
  `GetIdea`.
- D02 owns the internal operator-bound transaction kernel, exact durable
  replay, lifecycle/stage tables, relationship semantics, and bounded history.
- D03 owns controller assignment/transfer and controller-session lifecycle.
- D04 owns Issue-to-Idea linkage.
- D05 owns ProgramRun and server-verified transition gates.
- D08 owns review findings and verdicts.
- D10 owns immutable integration/release receipts.
- D15 owns compatibility population, parity, cutover, retention, and any
  legacy disposition.
- D16 owns Idea UI reads and actions.

## Isolated migration and rollback

Never point candidate code at `~/.rsi/rsi.db`.

The generated test fixture creates a temporary full schema, seeds legacy
Project/Session/Group/Epic/Workflow/label/hierarchy/lineage/lead/topology/tag
state, removes only D01 objects, sets V74, checkpoints, and copies that closed
database to `upgrade.db` and byte-identical `rollback.db`. Candidate `Store`
opens only `upgrade.db`. Tests verify V75, FK/schema inventories, all-column
ordered legacy hashes, a failed-transaction rollback, valid-partial completion,
two reopens, and snapshot restoration.

For recovery, close every handle and preserve a failed copy as evidence. Copy
`rollback.db` to a new expendable path, then verify its V74 `user_version`,
foreign keys, schema fingerprint, and legacy hashes with a read-only/raw SQLite
connection. Do not decrement a migrated database's `user_version` and do not
invent a down-migration.

D02 is migration-free: V75 already holds the canonical envelope, actor/scope,
version pair, durable key, digest vectors, and event-bound relationship
acceptance. `LATEST_SCHEMA_VERSION` and canonical D01/legacy DDL remain
unchanged. D02 recovery tests copy a closed temporary V75 database, perform
writes only on the working copy, reopen and verify it, and restore the
untouched snapshot to a new expendable path. They require `user_version=75`,
identical schema/legacy fingerprints, empty `foreign_key_check`, and
`integrity_check=ok`.

If a future implementation discovers that an additive column, acceptance
state, fingerprint, backfill, rebuild, or version bump is required, D02's
architecture must be reopened under a replacement migration plan. Candidate
code must never open or migrate the live database.

To roll back D02 before downstream integration, keep consumers unwired or
disable/revert the consumer first, then revert the bounded implementation
commit. Existing projections, append-only events, accepted supersedes edges,
tombstones, Captures, and D01/legacy reads remain readable. Recovery never
rewrites/deletes events, decrements counters, clears tombstones, performs a
production down-migration, restarts the daemon, or authorizes cleanup.

## D03 durable controller transfer

D03 separates durable semantic ownership from the process-local A6 transport
token. A controller write is admitted only when both fences agree:

1. the current A6 token resolves server-side to the acting Session; and
2. the Idea names that Session at the bound controller epoch and row version.

Transfer is a durable `reserve -> launch-confirmed assign` protocol. Reserve
appends `controller_reserved`, advances only the Idea row version and event
sequence, and derives a deterministic reservation UUID, candidate Session UUID,
proposed monotonic epoch, timestamp, and five-minute expiry from the
server-bound Idea and transfer intent. Assign requires the exact unexpired
reservation, a durable matching Session row, installed provider/handshake
confirmation, and the still-current prospective A6 binding. A synchronous
provider is witnessed by its live installed handle; a fresh `CodexAppServer`
is witnessed by its completed initialize/thread-start handshake plus the exact
current live `ProviderProcess::CodexAppServer` installation. A CLI `Codex`
process never satisfies that app-server witness. It then changes
owner and epoch in the same `BEGIN IMMEDIATE` transaction that appends
`controller_assigned`.

Failures append a reservation-scoped `controller_released` event without
changing the former owner or epoch. Assigned release is a distinct operation:
it clears the exact owner, preserves the epoch, and is rejected while a live
reservation exists. Exact stage replay precedes stale-state checks, so retries
return the original event, timestamp, expiry, and outcome while the reservation
is live. After its lease expires, replay of that exact transfer intent records
or observes one `Expired` release and returns the stable resolved-reservation
result; it never creates a second reservation for the same intent. A different
intent may release the expired tail and reserve immediately in a second
`BEGIN IMMEDIATE` transaction. That reservation uses the post-release row as
its actual base while retaining the caller's original expected version in the
canonical semantic request. Every mutation uses row-version CAS and append-only
canonical payloads.

Controller mutations use the strict `rsi.idea.controller-mutation/v1` domain.
Its allowlist contains only projection changes and stage transitions. The only
controller control action available to an assigned Session is exact
self-release. Assignment and operator/system releases remain construction-only
daemon operations.

| Actor | Reserve | Confirmed assign | Release reservation | Release assignment | Mutate |
|---|---:|---:|---:|---:|---:|
| Bound operator | yes | no | yes | yes | full D02 catalog |
| Internal system | yes | yes | yes | yes | no |
| Bound assigned controller | no | no | no | self only | projection/stage only |
| Unbound Session | no | no | no | no | no |

SQLite failures are classified from structured extended result codes.
`SQLITE_CONSTRAINT_UNIQUE` and `SQLITE_CONSTRAINT_PRIMARYKEY` trigger a
deterministic in-transaction reread of the event ID/idempotency key or other
business key. `SQLITE_BUSY`, `SQLITE_BUSY_RECOVERY`, `SQLITE_LOCKED`, and
`SQLITE_LOCKED_SHAREDCACHE` are contention. Generic constraint plus foreign-key,
check, not-null, trigger, I/O, and unknown extended codes remain storage
failures. Message text is never parsed, and unrelated constraint classes are
never collapsed into an idempotency result.

Ordinary continuation and handoff-write resume share one same-ID reconstruction
path. While holding the active-session witness it verifies an installed live
provider whose tracked incarnation is not interrupt-requested, a durable
matching Session row in `Starting`, `Running`, or `WaitingApproval`, the current
reminted A6 binding, and the unchanged durable Idea assignment/epoch before
installing the grant in the Store-owned process-local registry. A failed
same-ID establishment removes the grant and revokes its prospective A6 binding.
A different Session ID—including
rotation and retry successors—must reserve and complete a new transfer.
`CodexAppServer`-to-CLI-`Codex` fallback is such a provider replacement: it uses
a new UUID and next epoch rather than rewriting the old Session's provider.
Lineage, `continued_from`, hierarchy, title, model, and transcript never confer
controller authority.

After a successor's assignment transaction commits, the process-local registry
installs its grant and removes the former owner's grant under the retained Store
and A6 guards. Assignment also retains the active-session write guard while it
revalidates the installed provider and `interrupt_requested`, giving real
`InterruptSession`/`AgentHalt` cancellation and assignment one linearization
point with lock order Active -> Store -> A6. A cancellation winner terminally
releases the exact reservation and preserves the former owner, epoch, grant,
token, and publication state. An assignment winner commits the durable transfer
before cancellation can return, then publication and former-authority
retirement may proceed. The durable owner/epoch and process-local grant
therefore move at the same confirmed saga boundary without trusting a stale
confirmation or inventing a rollback of append-only Idea history.

Startup reconciliation clears the process-local grant registry before restored
session handling, pages Ideas in
stable `(project_id, id)` order in batches of 64, and performs one indexed
controller-tail lookup per Idea. Every unresolved pre-boot reservation is
released with zero grace and `RestartRecovery`; assignments remain durable but
no A6 grant is recreated. Corrupt controller tails fail closed per Idea.
Committed recovery/assignment publication uses the Idea event UUID as the
consumer deduplication key.

V76 adds only:

```sql
CREATE INDEX idx_idea_events_controller_tail
ON idea_events(idea_id, sequence DESC)
WHERE event_type IN (
  'controller_reserved',
  'controller_assigned',
  'controller_released'
);
```

The exact recovery query uses keyed `SEARCH` through this partial index, with
no history scan or temporary sort. Migration rehearsals use copied, closed V75
fixtures only; candidate code must never open the live database.

D03 rollback is forward-safe: stop creating new reservations and deny new
controller handles, but retain the additive index, projections, assigned
epochs, and every event. Resolve an in-flight reservation through its exact
typed release. Never delete or rewrite an event, restore an earlier owner,
decrement/reuse an epoch, or down-migrate a live database.

## Release disposition and human gates

- `HUMAN-PROVIDER-DATA`: blocked; reference/digest only.
- `HUMAN-COLLECTION-UI`: blocked and not due.
- `HUMAN-RETENTION`: blocked.
- `HUMAN-OPPOSITION-LAUNCH`: blocked pending a separately launched
  opposite-family reviewer.
- `HUMAN-PROMOTE`: blocked.
- `HUMAN-DEPLOY`: blocked.
- `HUMAN-RESTART`: blocked.
- `HUMAN-DISCARD`: blocked.
- `HUMAN-CLEANUP`: blocked.
- `HUMAN-LEGACY-CUTOVER`: blocked.
- `HUMAN-LEGACY-DELETE`: blocked.
- `HUMAN-GRAPH-PRODUCTION`: `NOT-DUE` for D01 and remains blocked globally.

**NOT DEPLOYED. DAEMON NOT RESTARTED. NO DESTRUCTIVE CLEANUP. PROJECT TARGET
NOT PROMOTED.**

# ProgramRun linkage (V78/V79)

An Idea may own at most one nonterminal ProgramRun. Each productive ProgramRun
transaction CAS-updates `ideas.row_version`, appends one immutable
`idea_program_run_transitions` row, and appends one `idea_events` row with
`program_transitioned` or `gate_transitioned`. The transition's
`idea_event_id`, expected/resulting Idea versions, and the Idea event's versions
must describe the same commit.

D03 controller assignment is the exception to the normal one-transition/one-new
event construction path: it appends its existing single `controller_assigned`
Idea event and links a `controller_rebound` ProgramRun transition to that same
event. The transaction rebinds the active run and requested/held lock tickets,
using the real daemon boot identity for held leases. Reserved actions rebind in
place; claimed actions become the same reserved identity with a new claim
generation; published and acknowledged actions retain their state and external
reference while rebinding in place to the new controller/epoch/boot and claim
generation. The former authority is immediately fenced and no replacement
action is created.

Reservation events can advance the Idea version without a ProgramRun
transition. Consequently a controller rebind transition records the assignment
event's expected/resulting Idea versions, while the run projection advances to
the assignment's resulting version under its own run-version CAS.
