# Topology Validation

Source: `crates/rsid/src/session/topology_ops.rs`

The daemon runs a four-stage validation pipeline on every `CreateTopology` and
`UpdateTopology` call (when `definition` is `Some`). Validation is **pure**
(no DB access) and runs before any mutation. The first failing stage short-
circuits and returns `DaemonError::InvalidParam` with a specific message.

---

## Validation Pipeline

```
validate_topology_definition(def)
  ├── 1. validate_node_kinds(def)
  ├── 2. validate_edges_reference_existing_nodes(def)
  ├── 3. detect_cycles(def)
  └── 4. validate_iteration_caps(def)
```

Source: `crates/rsid/src/session/topology_ops.rs:250-258`

---

## Stage 1: Node Kind Legality

**Function:** `validate_node_kinds(def)`
Source: `crates/rsid/src/session/topology_ops.rs:262-272`

Every `node.kind` must be in `legal_children(Some(SessionKind::Epic))`.

The canonical legality set (from `rsi-common/src/types.rs:740-751`): all leaf
kinds — `Story`, `Task`, `Bug`, `Feature`, `Refactor`, `Research`, `Standard`,
`TaskRabbit`. Container kinds (`Group`, `Epic`) are illegal node kinds.

**Why:** A topology node represents a work unit that runs as a session. Only
leaf kinds spawn provider subprocesses. Container kinds are organizational
nodes that cannot execute.

**Error message:** `"illegal node kind: <kind> (must be in legal_children(Some(Epic)))"`

```json
// Bad: Epic cannot be a topology node
{"nodes": [{"id": "wrapper", "kind": "Epic", "label": "..."}], "edges": []}
// Error: "illegal node kind: Epic (must be in legal_children(Some(Epic)))"
```

---

## Stage 2: Edge Reference Validity

**Function:** `validate_edges_reference_existing_nodes(def)`
Source: `crates/rsid/src/session/topology_ops.rs:275-288`

Every `edge.from` and `edge.to` must resolve to a `node.id` in `def.nodes`.
Checked over the full edge set (including `loop_edge: true`).

**Error message:** `"edge references unknown node id: <id>"`

```json
// Bad: edge targets a node that doesn't exist
{"nodes": [{"id": "a", "kind": "Task", "label": "A"}],
 "edges": [{"from": "a", "to": "nonexistent", "loop_edge": false}]}
// Error: "edge references unknown node id: nonexistent"
```

---

## Stage 3: Cycle Detection (Kahn's Algorithm)

**Function:** `detect_cycles(def)`
Source: `crates/rsid/src/session/topology_ops.rs:292-332`

Runs Kahn's topological sort over the **acyclic subset** of edges — only edges
where `loop_edge == false`. Loop edges are explicitly excluded because the
loop mechanism is the correct way to express intentional cycles.

**Algorithm:**
1. Build in-degree map and adjacency list from non-loop edges only.
2. Initialize queue with zero-in-degree nodes.
3. Process queue: decrement neighbors' in-degrees; push newly-zero nodes.
4. If `visited != len(nodes)`, a cycle exists → `CycleDetected`.

**Error message:** `"topology DAG contains a cycle (excluding loop_edge: true)"`

```json
// Bad: a→b and b→a with loop_edge=false is a true cycle
{"nodes": [{"id":"a","kind":"Task","label":"A"},{"id":"b","kind":"Task","label":"B"}],
 "edges": [{"from":"a","to":"b","loop_edge":false},{"from":"b","to":"a","loop_edge":false}]}
// Error: "topology DAG contains a cycle (excluding loop_edge: true)"

// OK: b→a with loop_edge=true is an intentional loop (handled by stage 4)
{"edges": [{"from":"a","to":"b","loop_edge":false},{"from":"b","to":"a","loop_edge":true}]}
```

---

## Stage 4: Iteration Cap Validation

**Function:** `validate_iteration_caps(def)`
Source: `crates/rsid/src/session/topology_ops.rs:338-384`

Two sub-checks:

### 4a: Per-node cap

Every `node.max_iterations`, when `Some`, must be `<= MAX_ITERATIONS (32)`.

**Error message:** `"max_iterations <n> exceeds hard cap 32"`

```json
// Bad: cap of 33 exceeds the hard limit
{"nodes": [{"id":"a","kind":"Task","label":"A","max_iterations":33}], "edges": []}
// Error: "max_iterations 33 exceeds hard cap 32"
```

### 4b: Loop termination guard

Any **loop region** must declare at least one termination guard. A loop region
is identified via Tarjan's SCC algorithm over the **full edge set** (including
`loop_edge: true`).

A loop region is:
- An SCC of size > 1, OR
- A single-node SCC with a self-loop edge.

**Termination guard options (any one suffices):**
1. `def.until` is `Some` (topology-level predicate applies to the whole graph)
2. At least one node in the loop SCC has `max_iterations: Some(n)`

**Error message:** `"topology contains a loop region with no termination guard (define until or per-node max_iterations)"`

```json
// Bad: loop region {a,b} with no guard
{"nodes": [{"id":"a","kind":"Task","label":"A"},{"id":"b","kind":"Task","label":"B"}],
 "edges": [{"from":"a","to":"b","loop_edge":false},{"from":"b","to":"a","loop_edge":true}]}
// Error: unbounded loop

// OK: topology-level until guard
{"nodes": [...same...],
 "edges": [...same...],
 "until": {"type":"lead_halt"}}

// OK: per-node cap on a node in the SCC
{"nodes": [{"id":"a","kind":"Task","label":"A","max_iterations":5},{"id":"b","kind":"Task","label":"B"}],
 "edges": [{"from":"a","to":"b","loop_edge":false},{"from":"b","to":"a","loop_edge":true}]}
```

### SCC discovery (Tarjan's algorithm)

Source: `crates/rsid/src/session/topology_ops.rs:390-475`

The private `strongly_connected_components(def)` function implements Tarjan's
iterative SCC discovery over the full edge set. Used by `validate_iteration_caps`
to identify loop regions.

---

## `MAX_ITERATIONS = 32`

The hard cap is defined in two places:

1. **Topology validator** (definition-level): `crates/rsid/src/session/topology_ops.rs:27`
   — rejects definitions with per-node caps > 32.
2. **Spawn coordinator** (runtime): `crates/rsid/src/session/spawn_coordinator.rs:44`
   — rejects spawn directives where iteration > min(node.max_iterations, 32).

Both places use the same numeric constant but are independent. The spawn-time
cap is the runtime guard; the definition-time cap prevents invalid definitions
from being stored.

---

## Name Uniqueness (SELECT-then-INSERT)

Name uniqueness is enforced in two layers:

1. **Daemon pre-check** (`topology_name_exists`): SELECT before INSERT. Returns
   `InvalidParam("topology name already exists")` on conflict.
2. **DB constraint**: `UNIQUE` on `topologies.name` is the final guard against
   concurrent writes.

For `UpdateTopology`, the uniqueness check uses `exclude_id` to allow a
topology to be renamed to its current name (no-op) without a false conflict.
Source: `crates/rsid/src/store/topologies.rs:191-217`

---

## Delete Guard (In-Use Check)

`DeleteTopology` checks whether any Epic session's `workflow_id` column points
at the target topology before deleting.

**Query:** `SELECT id FROM sessions WHERE workflow_id = ?1 AND session_kind = 'Epic' LIMIT 1`
Source: `crates/rsid/src/store/topologies.rs:168-186`

`workflow_id_override` pointers are NOT checked. If a session has
`workflow_id_override` pointing at a deleted topology, `effective_topology()`
returns `None` for that session (the walk yields no match), which is safe —
it falls through to the Epic's `workflow_id` instead.

---

## Error Reference

| `TopologyError` variant | `InvalidParam` message |
|------------------------|------------------------|
| `DuplicateName` | `"topology name already exists"` |
| `UnknownNode(id)` | `"edge references unknown node id: <id>"` |
| `CycleDetected` | `"topology DAG contains a cycle (excluding loop_edge: true)"` |
| `IllegalNodeKind(kind)` | `"illegal node kind: <kind> (must be in legal_children(Some(Epic)))"` |
| `IterationCapExceeded(n)` | `"max_iterations <n> exceeds hard cap 32"` |
| `UnboundedLoop` | `"topology contains a loop region with no termination guard (define until or per-node max_iterations)"` |
| `NotFound` | `"topology not found"` |
| `InUse(epic_id)` | `"topology in use by Epic <uuid>: clear the reference before deleting"` |

Source: `crates/rsid/src/session/topology_ops.rs:53-87`

---

## Loop Termination: Three Mechanisms

The loop guard requirement exists at definition time. At runtime, the executor
enforces termination via three independent mechanisms (all implemented as of P1.11):

| Mechanism | Phase available | How it fires |
|-----------|-----------------|--------------|
| `until: MaxIterations(n)` | P1.4 (definition) | Topology-level; `UntilEvaluator` stops when iteration hits n |
| Per-node `max_iterations` | P1.4 (definition) | Node-level; `FailurePolicy::Retry` counts against cap |
| Lead emits `/halt` | P1.11 | `until: LeadHalt` — `DaemonEvent::HaltDirective` bus event triggers halt |

Any one of these terminating is sufficient. The `UnboundedLoop` error ensures
at least one is declared. The `MAX_ITERATIONS = 32` hard cap in `run_loop_region`
provides a hard backstop independent of the definition.

See [spawn-child-grammar.md](spawn-child-grammar.md) for `/halt` directive details.

---

## Loop Regions at Runtime (P1.11)

P1.4 validates loop region structure at definition time. P1.11 enforces these at
execution time:

**What the bridge stamps (P1.11):** When `ExecuteTopology` bridges a topology,
the workflow metadata receives:
- `loop_edges`: JSON list of `{from, to}` pairs for all `loop_edge: true` edges
- `scc_regions`: JSON list of SCC member node ID groups (Tarjan's algorithm, pre-computed)
- `until_condition`: structured JSON of the `UntilCondition` variant

**What the executor does:** `run_graph_workflow` reads these metadata keys and,
when `scc_regions` is non-empty, switches to the loop-aware execution path:
1. Pre-loop layers execute once (acyclic nodes before the first SCC layer).
2. The SCC region executes via `run_loop_region`, which:
   - Runs acyclic-skeleton layers concurrently per iteration.
   - After each iteration, calls `UntilEvaluator::check_async` to decide halt/continue.
   - Enforces `MAX_ITERATIONS = 32` as a hard ceiling.
3. Post-loop layers execute once after the loop halts.

**FailurePolicy at runtime:** Per-node `on_failure` policies are read from
`metadata["failure_policies"]` and applied in `execute_layer`:
- `Halt` (default): abort the workflow on node failure.
- `Skip`: insert empty `NodeData` for the failed node; downstream nodes still run.
- `Retry`: re-run the node up to `repeat_policy.max_iterations` times before halting.

Source: `crates/rsid/src/session/graph_runner.rs` — `run_graph_workflow`,
`run_loop_region`, `execute_layer`.

## Per-node `params` bag

`TopologyNode.params` is an opaque `HashMap<String, serde_json::Value>`.
The daemon does **not** validate its contents — `validate_topology_definition`
deliberately skips it. Consumers (Phase 5 scheduler, spawn coordinator) own
the interpretation contract. A topology with malformed params will fail at
execution time, not at `CreateTopology` time.

The field serializes as `"params": {}` when empty (not elided). Pre-P1.8
topologies stored without the key deserialize cleanly to an empty map.

---

## See Also

- [README.md](README.md) — Phase 1 overview and Quickstart
- [rpc-reference.md](rpc-reference.md) — `CreateTopology` / `UpdateTopology` error codes that validation produces
- [topology-workflow-bridge.md](topology-workflow-bridge.md) — how `params` keys are consumed at `ExecuteTopology` time
- [spawn-child-grammar.md](spawn-child-grammar.md) — runtime spawn-time validation (prereqs, iteration cap, uniqueness)
