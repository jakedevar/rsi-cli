# Topology → Workflow Bridge (P1.10/P1.11)

`topology_bridge.rs` converts stored `Topology` rows into `WorkflowDefinition`
form for the `graph_runner.rs` async executor. The `ExecuteTopology` RPC is the
caller-facing entry point.

---

## When to Use `ExecuteTopology` vs `ExecuteWorkflow`

| Use case | RPC |
|----------|-----|
| Topology row stored in `topologies` table (has a `Uuid` id) | `ExecuteTopology` |
| Raw `WorkflowDefinition` JSON blob, no backing topology row | `ExecuteWorkflow` |
| Chain execution (master_improve loop) | `StartChainedWorkflow` |

`ExecuteTopology` loads the topology, validates it (prereq check, etc.), bridges
it to a `WorkflowDefinition` in memory, and delegates to the same
`execute_workflow_live` path that `ExecuteWorkflow` uses. The executor
payload itself is in-memory only — the bridge does NOT persist a
`WorkflowDefinition` to disk for the executor. Loop-bearing topologies are
fully supported as of P1.11.

---

## P1.12 §9 — Workflows-Table Mirror (Persisted)

**Reversing the P1.10 "in-memory only" stance.** As of P1.12 §9, every
topology mutation auto-mirrors into the `workflows` table so the `gv` graph
picker surfaces every topology as a saved template — without manual workflow
row inserts and without stale rows.

### Trigger points

| RPC | When fired | What it does |
|-----|-----------|--------------|
| `ExecuteTopology` | BEFORE executor launch (gated; launch aborts if upsert fails) | `upsert_bridged_workflow(&topology, params.project_id)` |
| `UpdateTopology` | AFTER successful topology row update | Re-bridges + upserts; failures logged but do NOT block the update |
| `DeleteTopology` | BEFORE topology row delete (OQ2 ordering) | `delete_workflow_by_source_topology(params.id)`; cascade is idempotent |

### Idempotency key (deterministic workflow row id)

The mirrored workflow row's primary key is **not** random. It is derived
from the topology id via UUID v5 algebra:

```
workflow_id = Uuid::new_v5(BRIDGE_NAMESPACE_UUID, topology_id.as_bytes())
```

Where `BRIDGE_NAMESPACE_UUID = c5c5f8d6-3d6a-4f5e-9b4e-1f8c3a7d6b9e` is a
locked compile-time constant in `topology_bridge.rs`. It MUST NEVER CHANGE
across releases: bumping it would break idempotency for every already-
mirrored topology (new id ≠ old id), creating orphan duplicates.

`Store::upsert_workflow_definition` uses `INSERT INTO workflows … ON
CONFLICT(id) DO UPDATE` — the same v5 id makes the upsert idempotent.
Calling `upsert_bridged_workflow` twice against the same topology results in
exactly one workflow row, with `updated_at` refreshed on the second call and
`created_at` preserved from the first.

### Cascade-delete semantics (OQ2 ordering)

`handle_delete_topology` deletes in this order:

1. **Workflow row first** (`delete_workflow_by_source_topology(topology_id)`).
   - If this fails → topology delete is skipped, error surfaces, topology
     row remains as source-of-truth.
2. **Topology row second** (`delete_topology(topology_id)`).
   - If this fails → workflow row is already gone (recoverable: next
     `ExecuteTopology` re-creates it). Topology row remains.

Rationale: the workflow row is a *derived artifact* of the topology row.
Deleting the source first leaves a window where `gv` shows a workflow whose
`source_topology_id` points to nothing (orphan). Deleting workflow first
means any mid-failure leaves the workflow absent (recoverable) and the
topology intact (source of truth survives). A cross-table transaction is
out of scope; per-call best-effort ordering is sufficient.

### What's NOT covered

- `CreateTopology` does NOT auto-upsert. Empty/draft topologies stay
  invisible to the `gv` picker until the first `ExecuteTopology` or
  `UpdateTopology` against them.
- The upsert helper does NOT propagate `project_id` on update
  (`handle_update_topology` passes `None`). To change a workflow row's
  project_id, run `ExecuteTopology` with the right `project_id`.

---

## What the Bridge Preserves

| Topology field | WorkflowDefinition mapping |
|---------------|---------------------------|
| `node.id` | `NodeDef.id` |
| `node.label` | `NodeDef.name` |
| `node.kind` | Encoded as tag `"kind:{:?}"` in `NodeDef.tags` |
| `node.params["instructions"]` | `NodeDef.instructions` |
| `node.params["provider"]` | `NodeDef.provider` |
| `node.params["model"]` | `NodeDef.model_settings.model` |
| `node.params["temperature"]` | `NodeDef.model_settings.temperature` |
| `node.params["working_dir"]` | `NodeDef.working_dir` |
| `node.params["sandbox"]` | `NodeDef.sandbox` |
| `node.params["tags"]` (array) | Extended into `NodeDef.tags` |
| Unknown params keys | Encoded as `"key=value"` tags in `NodeDef.tags` |
| `node.max_iterations` | `NodeDef.repeat_policy.max_iterations` |
| `edge.from → edge.to` | `EdgeDef.source → EdgeDef.target` |
| `topology.name` | `WorkflowDefinition.name` |
| `topology.id` | `metadata["source_topology_id"]` |

---

## Loop Edge Metadata (P1.11)

As of P1.11, loop-bearing topologies are fully supported. The bridge stamps
additional metadata keys for the executor:

| Metadata key | Value | Consumer |
|-------------|-------|----------|
| `loop_edges` | JSON `[{"from":"b","to":"a"}, ...]` | `graph_runner`: filters from Kahn pass |
| `scc_regions` | JSON `[["a","b"], ...]` | `graph_runner`: identifies loop region members |
| `until_condition` | JSON-serialized `UntilCondition` | `UntilEvaluator`: termination predicate |
| `until_predicate` | Debug-format string (legacy compat) | Backward compat only |
| `failure_policies` | JSON `{"node_id":"Halt"|"Retry"|"Skip"}` | `execute_layer`: per-node failure handling |
| `prereqs` | JSON map | Stored for inspection; DAG enforced by edges |

`BridgeError::LoopEdgeUnsupported` is retained in the enum as `#[deprecated]`
for one release cycle; it is never constructed by the bridge after P1.11.
P1.12 does NOT remove it (deferred to a follow-up cycle to avoid coupling
removal to the §9 upsert work).

Source: `crates/rsid/src/session/topology_bridge.rs`

---

## Example: 3-Node Acyclic Topology with Params

### CreateTopology payload

```json
{
  "name": "research-plan-implement",
  "definition": {
    "nodes": [
      {
        "id": "research",
        "kind": "Task",
        "label": "Research",
        "params": {
          "instructions": "Research the problem domain and produce a summary.",
          "provider": "claude"
        }
      },
      {
        "id": "plan",
        "kind": "Task",
        "label": "Plan",
        "prereqs": ["research"],
        "params": {
          "instructions": "Create an implementation plan based on the research.",
          "provider": "claude"
        }
      },
      {
        "id": "implement",
        "kind": "Task",
        "label": "Implement",
        "prereqs": ["plan"],
        "params": {
          "instructions": "Implement the plan.",
          "provider": "codex",
          "sandbox": true
        }
      }
    ],
    "edges": [
      {"from": "research", "to": "plan", "loop_edge": false},
      {"from": "plan", "to": "implement", "loop_edge": false}
    ]
  }
}
```

### Resulting WorkflowDefinition (bridge output)

```json
{
  "version": "1.0",
  "name": "research-plan-implement",
  "nodes": [
    {
      "id": "research",
      "name": "Research",
      "type": "action",
      "instructions": "Research the problem domain and produce a summary.",
      "provider": "claude",
      "tags": ["kind:Task"]
    },
    {
      "id": "plan",
      "name": "Plan",
      "type": "action",
      "instructions": "Create an implementation plan based on the research.",
      "provider": "claude",
      "tags": ["kind:Task"]
    },
    {
      "id": "implement",
      "name": "Implement",
      "type": "action",
      "instructions": "Implement the plan.",
      "provider": "codex",
      "sandbox": true,
      "tags": ["kind:Task"]
    }
  ],
  "edges": [
    {"source": "research", "target": "plan"},
    {"source": "plan", "target": "implement"}
  ],
  "metadata": {
    "bridged_at": "2026-05-17T00:00:00Z",
    "prereqs": "{\"plan\":[\"research\"],\"implement\":[\"plan\"]}",
    "source_topology_id": "<uuid>",
    "source_topology_name": "research-plan-implement"
  }
}
```

### ExecuteTopology call

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "ExecuteTopology",
  "params": {
    "topology_id": "<uuid-from-create>",
    "project_id": null,
    "inputs": null
  }
}
```

### Response

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "execution_id": "<new-uuid>"
  }
}
```

Poll via `GetWorkflowExecution` with the returned `execution_id`.

---

## Error Reference

| BridgeError variant | Trigger condition | `InvalidParam` message |
|--------------------|-------------------|----------------------|
| `LoopEdgeUnsupported { edges }` | **Deprecated (P1.11)** — no longer triggered | N/A (variant retained for one cycle; remove in P1.12) |
| `PrereqsWithoutEdges { node_id, missing_prereqs }` | A node declares prereqs with no corresponding incoming edge | `"node <id> declares prereqs with no corresponding incoming edge: <list>"` |

Additionally, errors from the load path:

| Source | `InvalidParam` message |
|--------|----------------------|
| Topology not found | `"topology not found"` |
| Workflow executor validation | Various (from `rsi_graph::validate_executable_workflow`) |

---

## P1.11 Runtime Behavior

Loop-bearing topologies execute end-to-end. The bridge-stamped metadata keys
(`loop_edges`, `scc_regions`, `until_condition`) drive the `run_loop_region`
iteration driver in `graph_runner.rs`. See:
- `thoughts/shared/research/2026-05-17-loop-executor-extension-design.md` —
  design doc for the iteration driver and `UntilCondition` variants.
- [topology-validation.md](topology-validation.md) — "Loop Regions at Runtime"
  section for runtime enforcement details.

---

## See Also

- [README.md](README.md) — Phase 1 overview, architecture diagram, Quickstart (§b ExecuteTopology)
- [rpc-reference.md](rpc-reference.md) — `ExecuteTopology` RPC params, returns, and error codes
- [topology-validation.md](topology-validation.md) — definition-time validation that runs before the bridge (including `params` bag semantics)
- [schema.md](schema.md) — `topologies` table schema that the bridge reads from
