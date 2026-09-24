# Topology-on-Epic — Phase 1 Documentation

Phase 1 shipped a complete foundation: DAG-stored named topology templates, a
multi-tag system, extended launch/container params, and per-session topology
node binding. This directory is the reference for all of it.

## What Shipped in Phase 1

| Phase | Ticket | What it added |
|-------|--------|---------------|
| P1.1 | V43 migration | `sessions.tag` column, `session_tags` join table, `topologies` table |
| P1.2 | `effective_topology` helper | Walk `parent_id` chain to resolve topology from Epic ancestor |
| P1.3 | Kill workflow-id copy | `/spawn_child` no longer copies `workflow_id` onto child rows |
| P1.4 | Topology RPC surface | 5 RPC methods — CRUD + DAG validation |
| P1.5 | Tag RPC surface | 4 RPC methods — normalize, CRUD, `ListTags` |
| P1.6 | Extended params | `LaunchSession.tags`, `CreateContainer.tags + topology_id` |
| P1.7 | Node binding + V47 | `topology_node_id`, `topology_iteration` columns + `/spawn_child` kwargs |
| P1.8 | `TopologyNode.params` extension | Opaque `HashMap<String, serde_json::Value>` bag on each node; `#[serde(default)]` for backward compat; bridge reads `instructions`, `provider`, `model`, `temperature`, `working_dir`, `sandbox`, `tags` keys |
| P1.9 | Index status sidecar | `INDEX.status.json` per-ticket pipeline status; `UpdateIndexStatus`/`GetIndexStatus` RPCs |
| P1.10 | Topology → workflow bridge | `ExecuteTopology` RPC; `topology_bridge.rs`; bridges stored topology to `graph_runner` |
| P1.11 | Loop-aware executor | `run_loop_region` iteration driver; `UntilEvaluator`; `FailurePolicy`; `HaltDirective` bus event; loop-edge metadata stamping |

## Quickstart — Running a Topology Today

Prereqs: daemon running (`cargo run --bin rsid`), `rsi-rpc` binary built:

```bash
cargo build -p rsi-common --bin rsi-rpc
alias rpc='cargo run -q -p rsi-common --bin rsi-rpc --'
```

### a) Create an acyclic topology

```bash
rpc CreateTopology --params '{
  "name": "research-implement",
  "definition": {
    "nodes": [
      {
        "id": "research", "kind": "Research", "label": "Research",
        "params": { "instructions": "Research the problem and produce a summary.", "provider": "claude" }
      },
      {
        "id": "impl", "kind": "Task", "label": "Implement",
        "prereqs": ["research"],
        "params": { "instructions": "Implement based on the research output.", "provider": "codex" }
      }
    ],
    "edges": [{"from": "research", "to": "impl", "loop_edge": false}]
  }
}'
# → {"id": "<TOPOLOGY_UUID>"}
```

`params` keys recognized by the bridge: `instructions`, `provider`, `model`,
`temperature`, `working_dir`, `sandbox`, `tags` (array). Unknown keys are
preserved as `"key=value"` tags on the `NodeDef`. Source:
`crates/rsid/src/session/topology_bridge.rs`.

### b) Execute it

```bash
# Top-level run (spawned children are orphans — visible in the flat session
# list only):
rpc ExecuteTopology --params '{
  "topology_id": "<TOPOLOGY_UUID>",
  "project_id": null,
  "inputs": null
}'
# → {"execution_id": "<EXEC_UUID>"}

# P1.12: Epic-scoped run — spawned children write parent_id = Epic.id to the
# sessions table and appear in the Epic's session-list child view. The TUI
# chord `gR` on a focused Epic with a topology binding fires this exact form.
rpc ExecuteTopology --params '{
  "topology_id": "<TOPOLOGY_UUID>",
  "project_id": null,
  "inputs": null,
  "parent_id": "<EPIC_UUID>"
}'
```

The bridge runs in-memory for the executor payload. **As of P1.12 §9**, the
daemon ALSO mirrors the topology into the `workflows` table on every
`ExecuteTopology` / `UpdateTopology` so the topology surfaces in the `gv`
graph picker automatically. Loop-bearing topologies are fully supported
(P1.11). The mirrored workflow row's primary key is deterministic:
`Uuid::new_v5(BRIDGE_NAMESPACE_UUID, topology_id)`. `DeleteTopology` cascades
the workflow row delete before the topology row.

### c) Poll progress

```bash
rpc GetWorkflowExecution --params '{"execution_id": "<EXEC_UUID>"}'
# Interrupt if needed:
rpc InterruptWorkflowExecution --params '{"execution_id": "<EXEC_UUID>"}'
```

Status fields follow the same schema as `ExecuteWorkflow` executions.

### d) Loop-bearing topologies (P1.11)

Topologies with `loop_edge: true` edges execute end-to-end. The bridge stamps
`loop_edges`, `scc_regions`, and `until_condition` into the `WorkflowDefinition`
metadata; `run_loop_region` in `graph_runner.rs` drives the iteration loop.

The canonical meta-implementation topology (UUID
`8c9b9d08-7d17-437d-a5be-b1c749cb9206`, name `rpi-with-verify-and-docs`) is
executable. AI session failures within the loop are expected in test environments
(no real AI APIs). The executor wires the loop regions correctly regardless.

## Architecture Overview

```
 ┌─────────────────────────────────────────────────────────────────┐
 │                          Epic (container)                        │
 │   workflow_id ──────────────► Topology (DB row)                  │
 │                                │                                 │
 │                                ▼                                 │
 │               ┌───────────────────────────────┐                  │
 │               │  TopologyDefinition             │                  │
 │               │  nodes: [plan_v1, impl_v1, …]  │                  │
 │               │  edges: [(plan→impl, loop=F)]   │                  │
 │               │  until: None | LeadHalt | MaxN  │                  │
 │               └───────────────────────────────┘                  │
 │                        │                                         │
 │                        │ effective_topology() walk               │
 │                        ▼                                         │
 │   ┌─────────┐  topology_node_id = "plan_v1"                     │
 │   │ Session │  topology_iteration = 2                            │
 │   │ (leaf)  │  parent_id ──────────────────────────────────────► │
 │   └─────────┘                                                    │
 └─────────────────────────────────────────────────────────────────┘

 /spawn_child flow (P1.7):
 Lead emits block → SpawnCoordinator.handle()
   → effective_topology_with_override(epic)
   → validate node kind + prereqs + iteration cap + uniqueness
   → LaunchConfig { topology_node_id, topology_iteration }
   → session row written with binding

 ExecuteTopology flow (P1.10/P1.11):
 ExecuteTopology RPC
   → get_topology(id) from DB
   → topology_bridge.rs: bridge_topology_to_workflow(&topology)
   │    ── validate prereqs have edges
   │    ── translate nodes/edges/params → WorkflowDefinition
   │    ── stamp loop_edges + scc_regions + until_condition (P1.11)
   ▼
 [WorkflowDefinition] ──────────────────────────────► graph_runner.rs
    (in-memory; not persisted)                         (execute_workflow_live)
                                                        └── run_loop_region (P1.11)

 Bridge path (P1.10 — dotted = in-memory, no DB row):
 ┌──────────────────┐       bridge_topology_to_workflow()
 │  topologies table│ ·····················►┌────────────────────┐
 │  (stored DAG)    │                       │ topology_bridge.rs │
 └──────────────────┘                       └─────────┬──────────┘
                                                      │ (pure fn, no I/O)
                                                      ▼
                                              ┌──────────────────┐
                                              │  graph_runner.rs │
                                              │ execute_workflow_ │
                                              │      live()      │
                                              └──────────────────┘
```

**Key invariants:**
- `Session.workflow_id` is NEVER written to child rows at spawn time (P1.3).
  Children always derive topology by walking `parent_id` to the Epic.
- `Session.workflow_id_override` can pin a topology to a specific session,
  superseding Epic inheritance. Set via the `r` keybinding on the `gv` overlay.
- `topology_node_id` is a string matching a `TopologyNode.id` within the
  topology resolved by `effective_topology()`. `None` = unbound session.
- `topology_iteration` is 0-based; hard-capped at `MAX_ITERATIONS = 32`.

## Table of Contents

| Doc | What it covers |
|-----|---------------|
| [rpc-reference.md](rpc-reference.md) | All RPC methods — params, returns, errors, JSON examples |
| [schema.md](schema.md) | V43 + V47 DB migrations — columns, constraints, indexes, rollback |
| [spawn-child-grammar.md](spawn-child-grammar.md) | `/spawn_child` directive grammar with P1.7 kwargs |
| [tag-system.md](tag-system.md) | Tag normalization, dual-column semantics, hydration timing |
| [topology-validation.md](topology-validation.md) | DAG validators, cycle detection, loop termination rules |
| [index-status-sidecar.md](index-status-sidecar.md) | `INDEX.status.json` sidecar — schema, RPC, bootstrap, atomic-write design |
| [topology-workflow-bridge.md](topology-workflow-bridge.md) | `ExecuteTopology` bridge — when to use, what is preserved, error reference |
| [runbook-rolling-health-baseline.md](runbook-rolling-health-baseline.md) | Stored rolling health baseline: upsert, execute, report, and recovery |
| [runbook-readonly-fanout-audit.md](runbook-readonly-fanout-audit.md) | Stored read-only fan-out audit: inputs, model routing, execution, and limits |

## Status Sidecar (P1.9)

`thoughts/shared/projects/<project>/INDEX.status.json` is a machine-managed
companion to `INDEX.md`. Agents update it via the `UpdateIndexStatus` and
`GetIndexStatus` RPC methods; the file is the authoritative per-ticket pipeline
status source for automation. Human-readable context stays in `INDEX.md`.

See [index-status-sidecar.md](index-status-sidecar.md) for the full schema
reference, RPC docs, and bootstrap workflow.

## Cheat-Sheet (Spawn + Tag)

### Attach topology to an Epic

```bash
cargo run -q -p rsi-common --bin rsi-rpc -- CreateContainer --params '{
  "kind": "Epic",
  "name": "My Epic",
  "tags": ["q3", "backend"],
  "topology_id": "<uuid-from-CreateTopology>"
}'
```

### Spawn a bound child (inside a lead session)

```
<docregblock>
/spawn_child kind=Research topology_node=research iteration=0
QUERY:
Research the authentication subsystem.
</docregblock>
```

### Tag operations

```bash
# Replace full tag set
cargo run -q -p rsi-common --bin rsi-rpc -- UpdateSessionTags --params '{"session_id":"<uuid>","tags":["ci","infra"]}'

# Add single tag
cargo run -q -p rsi-common --bin rsi-rpc -- AddSessionTag --params '{"session_id":"<uuid>","tag":"urgent"}'

# List all tags by frequency
cargo run -q -p rsi-common --bin rsi-rpc -- ListTags --params '{}'
```

## Related Documents

| Doc | Path |
|-----|------|
| rsi-graph vs topology-on-epic audit | `thoughts/shared/research/2026-05-17-rsi-graph-vs-topology-on-epic-audit.md` |
| P1.11 loop-executor extension design | `thoughts/shared/research/2026-05-17-loop-executor-extension-design.md` |
| Project INDEX | `thoughts/shared/projects/topology-on-epic/INDEX.md` |
| Project INDEX status sidecar | `thoughts/shared/projects/topology-on-epic/INDEX.status.json` |

---

**Phase 1 complete: 11/11 tickets shipped (2026-05-04 → 2026-05-17).**

The meta-implementation topology (`rpi-with-verify-and-docs`, UUID
`8c9b9d08-7d17-437d-a5be-b1c749cb9206`) is fully executable end-to-end via
`ExecuteTopology`. Both loop regions wire correctly; session failures within
the loop are AI-call failures (expected in test environments without live APIs).
