# RPC Reference — Topology-on-Epic Phase 1

All methods use JSON-RPC 2.0 over the Unix socket at `~/.rsi/daemon.sock`.
Error codes follow the standard JSON-RPC spec: `-32602` = `INVALID_PARAMS`.

## Error codes used in this section

| Code | Meaning |
|------|---------|
| `-32602` | `InvalidParam` — validation failed; `message` carries the specific reason |
| `-32603` | `InternalError` — store failure |

---

## Topology RPCs (P1.4)

Source: `crates/rsid/src/rpc.rs:1612-1667`, `crates/rsid/src/session/topology_ops.rs`

Topologies are named, DB-stored DAG templates that live on the `topologies`
table. They are attached to Epics via `sessions.workflow_id`. The daemon runs
a four-stage validation pipeline on every Create/Update. See
[topology-validation.md](topology-validation.md) for rules.

---

### ListTopologies

List all topology rows, optionally filtered by name prefix. Returns rows in
ascending name order.

**Param struct:** `crates/rsi-common/src/rpc.rs:818-822`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `name_prefix` | `Option<String>` | `null` | Return only rows where `name LIKE 'prefix%'` |

**Returns:** `Array<Topology>`

**Errors:** None (empty array on no match).

```json
// Request
{"jsonrpc":"2.0","id":1,"method":"ListTopologies","params":{"name_prefix":"research"}}

// Response
{"jsonrpc":"2.0","id":1,"result":[
  {
    "id": "550e8400-e29b-41d4-a716-446655440001",
    "name": "research-implement",
    "definition": {
      "nodes": [
        {"id":"research","kind":"Research","label":"Research","prereqs":[],"max_iterations":null,"on_failure":null},
        {"id":"impl","kind":"Task","label":"Implement","prereqs":["research"],"max_iterations":null,"on_failure":null}
      ],
      "edges": [{"from":"research","to":"impl","loop_edge":false}],
      "until": null
    },
    "created_at": "2026-05-16T10:00:00Z",
    "updated_at": "2026-05-16T10:00:00Z"
  }
]}
```

**Note:** `ListTopologies` also accepts `{}` or omits `params` entirely — the
daemon falls back to `ListTopologiesParams { name_prefix: None }`.

---

### CreateTopology

Create a new named topology. The daemon validates the definition (all four
validators) before inserting. Returns `{ id: Uuid }` of the new row.

**P1.12 §9 note:** `CreateTopology` itself does NOT upsert into the
`workflows` table — the topology becomes visible in the `gv` graph picker
only after the first `ExecuteTopology` (or `UpdateTopology`) call against it.
This is intentional: empty/draft topologies stay invisible to the picker
until the operator actually wants to surface them via a run.

**Param struct:** `crates/rsi-common/src/rpc.rs:831-834`

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `name` | `String` | yes | Unique display name |
| `definition` | `TopologyDefinition` | yes | DAG definition (nodes + edges + optional `until`) |

**Returns:** `{ "id": "<uuid>" }`

**Errors:**

| Message | Cause |
|---------|-------|
| `"topology name already exists"` | Another row shares the name |
| `"edge references unknown node id: <id>"` | Edge endpoint not in `nodes` |
| `"topology DAG contains a cycle (excluding loop_edge: true)"` | Kahn's algorithm detected a cycle |
| `"illegal node kind: <kind> (must be in legal_children(Some(Epic)))"` | Node kind not legal under Epic |
| `"max_iterations <n> exceeds hard cap 32"` | Per-node cap > 32 |
| `"topology contains a loop region with no termination guard ..."` | Loop SCC with no `until` and no `max_iterations` |

```json
// Request
{"jsonrpc":"2.0","id":1,"method":"CreateTopology","params":{
  "name": "research-implement",
  "definition": {
    "nodes": [
      {"id":"research","kind":"Research","label":"Research phase"},
      {"id":"impl","kind":"Task","label":"Implementation","prereqs":["research"],
       "params":{"audience":"myself","model":"gpt-5"}}
    ],
    "edges": [{"from":"research","to":"impl","loop_edge":false}]
  }
}}

// Response (success)
{"jsonrpc":"2.0","id":1,"result":{"id":"550e8400-e29b-41d4-a716-446655440001"}}

// Response (duplicate name)
{"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"topology name already exists"}}
```

**Invariants:**
- Name uniqueness is checked daemon-side (SELECT) then enforced at DB layer
  (UNIQUE constraint on `topologies.name`). Race condition leaves the DB
  constraint as the final guard.
- `definition_json` is stored as serialized `TopologyDefinition`; the `until`
  field serializes as a tagged enum: `{"type":"lead_halt"}` or
  `{"type":"max_iterations","value":N}`.
- `params` on each node is an opaque key-value bag; the daemon stores it
  verbatim and performs no validation. Empty `params: {}` and absent `params`
  are both accepted and normalize to an empty map on load.

---

### UpdateTopology

Update an existing topology's name and/or definition. Either field may be
`null` (no-op for that field); both `null` is rejected.

**P1.12 §9:** after a successful update, the daemon re-bridges the topology
and upserts the corresponding workflow row in the `workflows` table so the
`gv` graph picker entry always reflects current topology state. The workflow
row's primary key is `Uuid::new_v5(BRIDGE_NAMESPACE_UUID, topology_id)`.

**Param struct:** `crates/rsi-common/src/rpc.rs:840-846`

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `id` | `Uuid` | yes | Topology to update |
| `name` | `Option<String>` | no | New name (null = keep existing) |
| `definition` | `Option<TopologyDefinition>` | no | New definition (null = keep existing) |

**Returns:** `{ "ok": true }`

**Errors:** Same validation errors as `CreateTopology`, plus:

| Message | Cause |
|---------|-------|
| `"topology not found"` | No row with the given id |
| `"update_topology: at least one of name or definition required"` | Both fields null |

```json
// Rename only
{"jsonrpc":"2.0","id":1,"method":"UpdateTopology","params":{
  "id": "550e8400-e29b-41d4-a716-446655440001",
  "name": "research-plan-implement"
}}

// Response
{"jsonrpc":"2.0","id":1,"result":{"ok":true}}
```

---

### DeleteTopology

Delete a topology row. Rejected if any Epic session's `workflow_id` still
references this topology — operator must clear those references first.

**P1.12 §9:** the daemon cascade-deletes the mirrored workflow row FIRST,
then the topology row. The cascade ordering (OQ2: workflow first, then
topology) guarantees that a mid-operation failure leaves the workflow row
absent (recoverable: next `ExecuteTopology` recreates it) and the topology
intact (source of truth survives). The mirrored row id is
`Uuid::new_v5(BRIDGE_NAMESPACE_UUID, topology_id)`. Cascade is idempotent:
absent workflow rows are silently ignored.

**Note:** Per-session `workflow_id_override` pointers are NOT checked. They
resolve to `None` at effective-topology read time when the target is gone
(Decision 1 in P1.4 plan).

**Param struct:** `crates/rsi-common/src/rpc.rs:852-854`

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `id` | `Uuid` | yes | Topology to delete |

**Returns:** `{ "ok": true }`

**Errors:**

| Message | Cause |
|---------|-------|
| `"topology not found"` | No row with the given id |
| `"topology in use by Epic <uuid>: clear the reference before deleting"` | Epic still points at this topology |

```json
// Request
{"jsonrpc":"2.0","id":1,"method":"DeleteTopology","params":{"id":"550e8400-e29b-41d4-a716-446655440001"}}

// Response (in use)
{"jsonrpc":"2.0","id":1,"error":{
  "code":-32602,
  "message":"topology in use by Epic 661e9511-f30c-52e5-b827-557766551112: clear the reference before deleting"
}}
```

---

### GetTopology

Fetch a single topology by id.

**Param struct:** `crates/rsi-common/src/rpc.rs:858-861`

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `id` | `Uuid` | yes | Topology id |

**Returns:** `Topology` (full row including `definition`)

**Errors:**

| Message | Cause |
|---------|-------|
| `"topology not found"` | No row with the given id |

```json
// Request
{"jsonrpc":"2.0","id":1,"method":"GetTopology","params":{"id":"550e8400-e29b-41d4-a716-446655440001"}}

// Response
{"jsonrpc":"2.0","id":1,"result":{
  "id": "550e8400-e29b-41d4-a716-446655440001",
  "name": "research-implement",
  "definition": { "nodes":[...], "edges":[...], "until":null },
  "created_at": "2026-05-16T10:00:00Z",
  "updated_at": "2026-05-16T10:00:00Z"
}}
```

---

### ExecuteTopology

Execute a stored topology through the rsi-graph loop-aware executor (P1.10/P1.11).

Loads the topology by id, bridges it to a `WorkflowDefinition` via
`topology_bridge.rs`, and dispatches it to `execute_workflow_live`. The bridge
is in-memory for the executor payload, but as of **P1.12 §9**, every
`ExecuteTopology` ALSO upserts a deterministic-id workflow row into the
`workflows` table so the topology surfaces in the `gv` graph picker
automatically. Loop-bearing topologies (with `loop_edge: true` edges) are
fully supported as of P1.11.

**Param struct:** `crates/rsi-common/src/rpc.rs` — `ExecuteTopologyParams`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `topology_id` | `Uuid` | required | ID of the stored topology to execute |
| `project_id` | `Option<Uuid>` | `null` | Project for resolving default working directory |
| `inputs` | `serde_json::Value` | `null` | Input value forwarded to the executor entry nodes |
| `parent_id` | `Option<Uuid>` | `null` | **P1.12:** parent container (Group or Epic) under which spawned sessions are placed. When `null`, sessions spawn as top-level orphans (legacy behavior; preserves backward compat with pre-P1.12 callers). The daemon validates the referenced session is a container kind (not a leaf) and not in a terminal state. |

**Returns:** `{ "execution_id": Uuid }`

Poll the execution status via `GetWorkflowExecution { "execution_id": … }`.
Interrupt via `InterruptWorkflowExecution { "execution_id": … }`.

**Errors:**

| Message | Cause |
|---------|-------|
| `"topology not found"` | No row with the given `topology_id` |
| `"node <id> declares prereqs with no corresponding incoming edge: …"` | A node's prereqs have no corresponding edge pointing into it |
| `"parent_id session <id> not found"` | **P1.12** — `parent_id` references a session that does not exist |
| `"parent_id session <id> is a leaf kind (…)"` | **P1.12** — `parent_id` references a leaf session (must be Group or Epic) |
| `"parent_id session <id> is in terminal state …"` | **P1.12** — `parent_id` references a Completed/Failed/Archived session |

**Notes:**
- Loop-bearing topologies use `UntilCondition` evaluation (`MaxIterations`,
  `LeadHalt`) and `FailurePolicy` enforcement (`Halt`, `Retry`, `Skip`).
  The bridge stamps `loop_edges`, `scc_regions`, `until_condition`, and
  `failure_policies` into `WorkflowDefinition.metadata`; the executor reads them.
- `prereqs` ordering hints are stamped into `metadata["prereqs"]` for inspection.
- **P1.12 §9:** the daemon automatically mirrors the topology into the
  `workflows` table via `upsert_bridged_workflow` before launching the
  executor. The workflow row's primary key is deterministic:
  `Uuid::new_v5(BRIDGE_NAMESPACE_UUID, topology_id)`. Calls are idempotent —
  the same topology always derives the same row id; `ON CONFLICT(id) DO
  UPDATE` refreshes title/stage/definition_json/updated_at. Edits flow
  through via `UpdateTopology`; deletes cascade via `DeleteTopology` (workflow
  row first, then topology row).

```json
// Request — acyclic topology, top-level (legacy / orphan spawned children)
{"jsonrpc":"2.0","id":1,"method":"ExecuteTopology","params":{
  "topology_id": "550e8400-e29b-41d4-a716-446655440001",
  "project_id": null,
  "inputs": null
}}

// Request — P1.12 — fire under a focused Epic so children appear in the
// Epic's session-list child view (parent_id = Epic.id).
{"jsonrpc":"2.0","id":1,"method":"ExecuteTopology","params":{
  "topology_id": "8c9b9d08-7d17-437d-a5be-b1c749cb9206",
  "project_id": null,
  "inputs": null,
  "parent_id": "00000000-0000-0000-0000-000000000abc"
}}

// Request — loop-bearing topology (2-node cycle, MaxIterations(3))
// (topology must have loop_edge: true on back-edge and until: {type: "max_iterations", value: 3})
{"jsonrpc":"2.0","id":1,"method":"ExecuteTopology","params":{
  "topology_id": "8c9b9d08-7d17-437d-a5be-b1c749cb9206",
  "project_id": null,
  "inputs": null
}}

// Response (same shape for all variants)
{"jsonrpc":"2.0","id":1,"result":{
  "execution_id": "9b1deb4d-3b7d-4bad-9bdd-2b0d7b3dcb6d"
}}
```

---

## Tag RPCs (P1.5)

Source: `crates/rsid/src/rpc.rs:1560-1610`, `crates/rsid/src/session/tag_ops.rs`

All tag mutations normalize inputs via `normalize_tag` before touching the DB.
See [tag-system.md](tag-system.md) for normalization rules.

---

### UpdateSessionTags

Replace the full tag set for a session atomically (DELETE all + INSERT new,
inside a single transaction). Rejected if `tags` is empty.

**Param struct:** `crates/rsi-common/src/rpc.rs:867-871`

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `session_id` | `Uuid` | yes | Session to update |
| `tags` | `Array<String>` | yes | New complete tag set (non-empty, each normalized) |

**Returns:** `{ "ok": true }`

**Errors:**

| Message | Cause |
|---------|-------|
| `"tags_required"` | `tags` array is empty |
| `"tag_malformed: <raw>"` | A tag fails `normalize_tag`; transaction rolled back |
| Session not found | `DaemonError::SessionNotFound` (code `-32603`) |

**Invariants:**
- Duplicates after normalization are removed (sort + dedup) before insert.
- `sessions.tag` is updated to the alphabetically-first tag in the new set
  (empty string if no tags remain).
- Atomicity: if any tag fails normalization, the existing set is unchanged.

```json
// Request
{"jsonrpc":"2.0","id":1,"method":"UpdateSessionTags","params":{
  "session_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
  "tags": ["CI", "infra", "backend"]
}}
// Stored as: ["backend", "ci", "infra"] (normalized + sorted)
// sessions.tag = "backend"

// Response
{"jsonrpc":"2.0","id":1,"result":{"ok":true}}
```

---

### AddSessionTag

Add a single tag to a session. Idempotent — adding a tag that already exists
is a no-op (no error).

**Param struct:** `crates/rsi-common/src/rpc.rs:874-878`

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `session_id` | `Uuid` | yes | Session to tag |
| `tag` | `String` | yes | Tag to add (normalized before insert) |

**Returns:** `{ "ok": true }`

**Errors:** `"tag_malformed: <raw>"` if normalization fails.

**Implementation:** `INSERT OR IGNORE INTO session_tags` — the
`PRIMARY KEY (session_id, tag)` constraint makes duplicates a no-op at the DB
layer. `sessions.tag` is refreshed after insert.

```json
{"jsonrpc":"2.0","id":1,"method":"AddSessionTag","params":{
  "session_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
  "tag": "urgent"
}}
```

---

### RemoveSessionTag

Remove a single tag from a session. Idempotent — removing an absent tag is a
no-op.

**Param struct:** `crates/rsi-common/src/rpc.rs:882-885`

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `session_id` | `Uuid` | yes | Session to untag |
| `tag` | `String` | yes | Tag to remove (normalized before delete) |

**Returns:** `{ "ok": true }`

**Errors:** `"tag_malformed: <raw>"` if normalization fails.

**Implementation:** `DELETE FROM session_tags WHERE session_id = ? AND tag = ?`.
`sessions.tag` is refreshed to the new alphabetically-first tag (or `""` if
none remain).

```json
{"jsonrpc":"2.0","id":1,"method":"RemoveSessionTag","params":{
  "session_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
  "tag": "urgent"
}}
```

---

### ListTags

List tags with session-counts, ordered by count descending then alphabetically.
Capped at 100 rows.

**Param struct:** `crates/rsi-common/src/rpc.rs:892-897`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `prefix` | `Option<String>` | `null` | Filter to tags starting with prefix |
| `project_id` | `Option<Uuid>` | `null` | Restrict to sessions in this project |

**Returns:** `Array<TagWithCount>` — `[{ "tag": String, "count": u32 }, ...]`

**Errors:** None (empty array on no match).

**Note:** `ListTags` falls back to `ListTagsParams { prefix: None, project_id: None }`
if params are omitted or fail to deserialize — it never returns a parse error
for missing optional fields.

```json
// Request: top tags in project
{"jsonrpc":"2.0","id":1,"method":"ListTags","params":{
  "prefix": "ci",
  "project_id": "b2c3d4e5-f6a7-8901-bcde-f12345678901"
}}

// Response
{"jsonrpc":"2.0","id":1,"result":[
  {"tag":"ci","count":14},
  {"tag":"ci-flaky","count":3}
]}
```

---

## Extended Existing RPCs (P1.6)

### LaunchSession — new fields

Source: `crates/rsi-common/src/rpc.rs:81-155`

Two fields added in P1.6:

| Field | Type | Serde | Description |
|-------|------|-------|-------------|
| `tags` | `Vec<String>` | **required** (no default) | Mandatory tag set. Caller must supply at least one. Daemon normalizes and rejects on malformed/empty. |
| `workflow_id_override` | `Option<Uuid>` | `serde(default)` = `null` | Per-leaf topology override. Persisted but not consumed until Phase 5. |

**Breaking change:** `LaunchSession` params without `tags` now fail to
deserialize (`missing field 'tags'`). All callers must supply the field.

Source: `crates/rsi-common/src/rpc.rs:1043-1081` (serde tests confirm this).

### CreateContainer — new fields

Source: `crates/rsi-common/src/rpc.rs:754-779`

Two fields added in P1.6:

| Field | Type | Serde | Description |
|-------|------|-------|-------------|
| `tags` | `Vec<String>` | **required** (no default) | Mandatory tag set, same normalization as `LaunchSession`. |
| `topology_id` | `Option<Uuid>` | `serde(default)` = `null` | Topology to attach to this Epic. Daemon rejects if `kind != Epic` and `topology_id` is `Some`. Persisted to `sessions.workflow_id`. |

```json
// Create Epic with topology
{"jsonrpc":"2.0","id":1,"method":"CreateContainer","params":{
  "kind": "Epic",
  "name": "Q3 Auth Overhaul",
  "tags": ["q3", "auth"],
  "topology_id": "550e8400-e29b-41d4-a716-446655440001"
}}
```

---

## Index Status RPCs (P1.9)

Source: `crates/rsid/src/rpc.rs:2594-2617`, `crates/rsid/src/session/index_status.rs`

The index status sidecar (`INDEX.status.json`) is a machine-managed companion
to `INDEX.md`. See [index-status-sidecar.md](index-status-sidecar.md) for the
full schema reference and atomic-write design.

---

### UpdateIndexStatus

Create or update a single ticket entry in the project's `INDEX.status.json`.

**Param struct:** `crates/rsi-common/src/rpc.rs` — `UpdateIndexStatusParams`

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `project` | `String` | yes | Project slug (`^[a-zA-Z0-9_-]+$`); used to construct path |
| `ticket_id` | `String` | yes | Ticket identifier, e.g. `"P1.9"` |
| `status` | `IndexStatusValue` | yes | One of: `not_started`, `ready`, `in_progress`, `shipped`, `blocked` |
| `last_shipped_commit` | `Option<String>` | no | Short commit SHA; only meaningful when `status = shipped` |
| `last_shipped_branch` | `Option<String>` | no | Branch name; only meaningful when `status = shipped` |

**Returns:** `{ "ok": true }`

**Errors:**
- `InvalidParam` — `project` fails slug validation.
- `InvalidParam` — `RSI_WORKSPACE_ROOTS` is empty (no workspace root configured).
- `Rpc` — filesystem I/O failure.

**Notes:**
- `last_shipped_at` is auto-set to `Utc::now()` when `status = shipped`.
- When `status != shipped`, all `last_shipped_*` fields are silently set to `null`.
- Write uses `tempfile::NamedTempFile::new_in(parent_dir)` + `.persist(target)` — POSIX-atomic.

```json
// Mark ticket shipped
{"jsonrpc":"2.0","id":1,"method":"UpdateIndexStatus","params":{
  "project": "topology-on-epic",
  "ticket_id": "P1.10",
  "status": "shipped",
  "last_shipped_commit": "5df9a2f3",
  "last_shipped_branch": "P1.10-topology-workflow-bridge"
}}

// Response
{"jsonrpc":"2.0","id":1,"result":{"ok":true}}
```

---

### GetIndexStatus

Read the full `INDEX.status.json` sidecar for a project.

**Param struct:** `crates/rsi-common/src/rpc.rs` — `GetIndexStatusParams`

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `project` | `String` | yes | Project slug (`^[a-zA-Z0-9_-]+$`) |

**Returns:** Full `IndexStatusSidecar` JSON object — `schema_version`, `project`,
`last_updated`, `tickets` (`BTreeMap<String, IndexTicketStatus>`).

**Errors:**
- `InvalidParam` — `project` fails slug validation.
- `InvalidParam` — sidecar file does not exist yet.
- `Rpc` — filesystem I/O or parse failure.

```json
// Request
{"jsonrpc":"2.0","id":1,"method":"GetIndexStatus","params":{"project":"topology-on-epic"}}

// Response
{"jsonrpc":"2.0","id":1,"result":{
  "schema_version": 1,
  "project": "topology-on-epic",
  "last_updated": "2026-05-17T06:56:48Z",
  "tickets": {
    "P1.1": {"status":"shipped","last_shipped_commit":"929bca1b","last_shipped_at":"2026-05-16T13:30:00Z","last_shipped_branch":"P1.4-topology-rpc-surface"},
    "P1.10": {"status":"shipped","last_shipped_commit":"5df9a2f3","last_shipped_at":"2026-05-17T06:56:00Z","last_shipped_branch":"P1.10-topology-workflow-bridge"}
  }
}}
```

---

## See Also

- [README.md](README.md) — Phase 1 overview, architecture diagram, Quickstart
- [topology-workflow-bridge.md](topology-workflow-bridge.md) — `ExecuteTopology` bridge internals, what is preserved vs deferred, error reference
- [tag-system.md](tag-system.md) — Tag normalization rules referenced by tag RPCs
- [index-status-sidecar.md](index-status-sidecar.md) — `INDEX.status.json` schema and atomic-write design
```
