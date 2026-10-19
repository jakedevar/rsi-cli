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
| [topology-validation.md](topology-validation.md) | Validators: DAG shape, loop guards, typed steps (`session`/`command`/`gate`/`review`/`land`), edge `when` routing, custody |
| [index-status-sidecar.md](index-status-sidecar.md) | `INDEX.status.json` sidecar — schema, RPC, bootstrap, atomic-write design |
| [topology-workflow-bridge.md](topology-workflow-bridge.md) | `ExecuteTopology` bridge (historical) and the legacy runner (`topology_executor_enabled=false`) |
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

## Operational Starters (#1641)

Five shared, operator-owned topologies are seeded by the daemon at boot
(`crates/rsid/src/topology/starters.rs`). Seeding is idempotent and never
overwrites an operator edit (a row whose text digest differs from its stored
`definition_digest` is left alone).

### `issue-implement-review-land`

`implement` (session, Claude Sonnet, expects a commit) -> `review` (DB-native
review of the `implement` commit by a Codex reviewer, `max_rounds` 2) ->
`land` (when the review verdict is accepted) or `fix` (when changes are
requested; a session that carries the findings, then a loop edge back to
`review`). `land` enqueues the exact accepted commit on the rolling merge queue.
The Issue is passed as `inputs.issue = <display_number>`; the daemon replaces it
at accept with a snapshot `{display_number,title,body,project_id}`.

### `issue-pipeline` (#2304)

The ordinary default for an Issue that needs no pre-merge review. `implement`
(a verified session: it must hand off a commit that its own final-HEAD
`scoped_test` job verified, see below) -> `land` (enqueues that commit with
derived filters and closes the Issue with the published SHA, `close_issue`).
A refused landing takes a verified `repair` -> `land_repair`; a repaired head
the queue refuses again (rolling advanced in the same file while it waited,
#2892) is repaired again by `repair_2` -> `land_repair_2` and `repair_3` ->
`land_repair_3` (three repairs in all, each told its attempt number, each
re-merging the current `origin/rolling` right before it verifies). A refusal
after the third repair, a repair worker's BLOCKED handoff naming a conflict
that needs a decision, a stale or missing receipt or an exhausted continuation
leaves a visible failed or parked execution and the Issue Open. The Issue is passed as
`inputs.issue = <display_number>`, as for the starter below. The worker never
pushes `rolling`: only the merge queue publishes.

A manager picks the route before the worker knows whether its change adds a
migration, so a land the daemon refuses `land_requires_review` (a numbered or
provisional migration, credential, IAM or network-exposure paths) never strands
the run (#2688). The `land -> pre_review` edge is `review_required`: the land
settles `failed(land_requires_review)`, the same execution runs `pre_review`
(the review node of `issue-pipeline-reviewed`: Codex, `max_rounds` 2) on the
exact verified head, `pre_fix` repairs changes requested inside the review loop
(`until` max 3 iterations) and `pre_land` lands only the accepted head. The
implementation is never redone and the ordinary `repair` route does not run for
this refusal. A land refused after that review takes no further repair.

The same route leaves `land_repair` (#2699): when an ordinary refusal's
`repair` produces a head the queue then refuses `land_requires_review` (the
repair adds a migration), `land_repair -> repair_review` (`review_required`)
reviews that exact repair head, `repair_fix` fixes changes requested inside the
same loop bound and `repair_land` lands only the accepted head.

### `issue-pipeline-reviewed` (#2565)

The pre-merge route for migrations and credential, IAM or network-exposure
changes. `implement` (verified) -> `review` (an independent Codex reviewer, the
other model family, of the exact verified head) -> `fix` loop on changes
requested, then `land` only on the accepted verdict. A refused landing takes one
verified `repair`, which gets its own `review_repair` before `land_repair`: a
repaired source is never enqueued on the first source's approval. Execute under
an Epic whose project manager owns the review ledger.

### `issue-pipeline-post-review` (#2565)

The post-land route for authority, custody, queue or deploy changes. The
`issue-pipeline` graph, repair chain included (#2899: up to three repairs, each
re-merging current `origin/rolling`), plus a `post_review` of whatever landed
(`post_review_repair`, `post_review_repair_2`, `post_review_repair_3` for the
first, second or third repaired landing; the untaken branches are skipped, so
only one review runs). Publication and Issue closure come first. When a reviewer asks for
changes, one `followup` session (explicit `custody.from=base`, it has two
inputs) files one deduped follow-up Issue per real finding; findings never
reopen the closed Issue or fail the execution.

### `rolling-qa-sweep`

`baseline` (command step `rolling_baseline` over `rsi-common`, `rsid`, `rsi`)
-> `healthy` (gate: `baseline` exit code 0) -> `triage` (a session that files an
Issue per new red, only when the baseline failed). It reports only: it never
writes `qa-green.sha`.

### Verified session nodes (#2249)

A session step may set `"require_verified": true` (it needs `expects_commit`).
After the handoff, commit and pin checks the daemon reads the node session's own
terminal `scoped_test` jobs (it never re-runs tests). The newest one whose
receipt `head` equals the node's result commit decides:

- Exit 0, or every failing test named by an open known-failure signature of the
  execution's project (#1016): the node succeeds and its output gains
  `verification {job_id, head, exit_code, plan, failing_tests, known_failures
  [{test, issue}], new_failures}`.
- Otherwise the node fails `verification_failed` (routable by a `failure` edge,
  never retried) with the same `verification` output plus `reason`:
  `no_receipt` (no scoped test job), `stale_head` (its newest receipt verifies
  another commit), `incomplete` (a job still running, or ended with no verdict:
  no receipt, timeout, deploy interruption, a build failure) or `new_failures`.

A `land` step's `accepted` may name such a node instead of a review. Its only
input must be that node's `success` edge. The land is admitted by the verified
node's job row, re-read under the enqueue lock, and enqueues exactly the
verified commit with queue-derived filters, with the node's session as the entry
source. It never lands a source that carries a schema migration (numbered or
provisional): the land blocks `land_requires_review`, because AGENTS.md reserves
the pre-land review for migrations, unless the land node has a `review_required`
out-edge (#2688), which routes the refusal to a review node in the same
execution (the Issue-pipeline starters do). Use a definition with a `review`
node for those, and for credential, IAM or network-exposure changes (not
detectable by the daemon).

### Starting one

- TUI: in the Issues workspace press `T` on an Issue (starts
  `issue-implement-review-land`) or run `:topology run <name> [<epic>]`; the run
  view opens on the execution.
- Agent: `AgentTopologyExecute` (the daemon RPC is `ExecuteTopology`, see [rpc-reference.md](rpc-reference.md)).
  Pass `on_call` (omitted means the project manager, or a named portfolio node that
  covers the project) to say who answers the run's questions.

**Operator-start ownership.** An operator-started run has no agent requester, so
the daemon derives an owner from rows, not from the request: `parent_id` must
be a live Epic of the request's project, and that project's appointed live
project manager must be able to act on the Epic (Automation capability, Epic in
scope, `execute` mode, not paused). That manager owns the `review` and `land`
nodes and lands under the same live authority as an agent requester; a revoked
grant, replaced lead, paused manager or moved scope stops the landing. A refusal
is a typed `operator_owner_*` code (`epic_required`, `project_required`,
`project_mismatch`, `no_manager`, `manager_unauthorized`) the TUI explains.

### On-call model

The on-call seat is the project manager, or the nearest live covering portfolio
seat when the PM cannot act (a named portfolio seat never falls back). Three
situations park a node on an ordinary decision record in that manager's ledger
(key `topology:<execution>:<node>:<iteration>:<attempt>`) instead of failing the
run:

- **Node question / BLOCKED handoff.** A session node ends BLOCKED (or with a
  `human_gate`) naming a `blocker_question`. The attempt goes `waiting` and the
  ruling's answer continues the SAME session. At most 3 questions per attempt.
- **Exhausted review.** The review ran out of rounds with changes still
  requested. The manager answers one word: `accept` (take the commit as is),
  `round` (one more round, offered only when the review policy allows it, once
  per instance) or `stop` (block the execution; also the default for any other
  answer).
- **Accept admits land for that commit only.** An `accept` ruling is recorded
  as a review-ledger fact for the exact commit under review, so `land` is
  admitted for that commit and no other.
- **Review with no usable verdict (#2280).** A review that ended without a
  verdict (including a reviewer that submitted `blocked` because its required
  checks never ran) earns one automatic new review: a fresh assignment under
  the review ledger's own round policy (the closing round uses a reviewer no
  earlier round used), after a 5-minute backoff when the failure looks like
  host capacity. If that also fails, or the policy offers no round, the
  attempt blocks `review_unsettled` and the execution blocks. Resolve it with
  `AgentTopologyResolveAttempt`: `retry` (another fresh review of the same
  commit), `accept` (the owning manager's ruling, recorded for the assignment
  that examined the commit; the review settles accepted and `land` proceeds),
  or interrupt the execution. `discard` is refused: a review keeps no
  preserved work. A docs-only diff needs no cargo tests in a review: the
  reviewer contract asks only for its static gates.

Records are re-read from the ledger every executor tick, so a restart loses
nothing. With no project-manager ledger covering the Epic there is nowhere to
file a record and the attempt falls back to blocked.

**Which questions reach the operator.** The record's gate follows the node's
blocker class: `production` -> `main_or_release`, `destructive` ->
`data_deletion`, `resource` -> `spend`, `authority` -> `credentials`. Questions
that declare a gate or whose text matches the gate scan, and any node question
the daemon cannot prove gate-free, are operator-only (they appear in
`<Space>gd`). Technical impasses and forbidden-scope questions, and every
ordinary exhausted review, are the on-call manager's to rule on. Related: the
global-manager grant and project creation contract (#1626) is in
[docs/harness-manager.md](../harness-manager.md).

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
