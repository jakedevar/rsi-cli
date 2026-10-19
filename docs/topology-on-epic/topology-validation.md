# Topology Validation

Source of truth: `crates/rsid-store/src/store_support/topology_validation.rs`
(pure validators), `crates/rsi-common/src/types.rs` (`TopologyStep`,
`CatalogOp`, `GateCondition`, `EdgeWhen`), and `crates/rsid/src/topology/steps.rs`
(execute-time decode and validation).

`crates/rsid/src/session/topology_ops.rs` and `topology::steps` re-export these
at their old paths (issue #1021 S3a). Read the store file, not the re-exports.

Validation is **pure** (no DB access) and runs before any mutation, at three
points:

| When | Caller | Input |
|---|---|---|
| Definition write | `CreateTopology` / `UpdateTopology` (when `definition` is `Some`), `AgentTopologyUpsert` | the `TopologyDefinition` |
| Execute | `ExecuteTopology` / `AgentTopologyExecute` / `ExecuteWorkflow` | the workflow snapshot (`steps::validate_workflow`) |
| Launch | each node launch of an agent execution | provider policy (see [rpc-reference.md](rpc-reference.md) "Agent topology verbs") |

The first failing stage short-circuits and returns `DaemonError::InvalidParam`
with a specific message. For agent upserts the message arrives as a
`diagnostics[]` entry with `invalid_definition`.

---

## Validation Pipeline

```
validate_topology_definition(def)
  ├── 1. validate_node_kinds(def)
  ├── 2. validate_edges_reference_existing_nodes(def)
  ├── 3. detect_cycles(def)
  ├── 4. validate_iteration_caps(def)
  └── 5. validate_topology(def)        typed steps + edge routing (#635, #1641)
          └── validate_graph(nodes, edges)
```

Stages 1-4 are the original graph-shape checks; stage 5 is the typed-step
layer. A topology with no `params.step` anywhere is a legacy all-session
topology: stage 5 only checks `params.when` and the forbidden author keys.

---

## Stage 1: Node Kind Legality

Every `node.kind` must be in `legal_children(Some(SessionKind::Epic))`: the
leaf kinds (`Story`, `Task`, `Bug`, `Feature`, `Refactor`, `Research`,
`Standard`, `TaskRabbit`). Container kinds (`Group`, `Epic`) are illegal.

Error: `illegal node kind: <kind> (must be in legal_children(Some(Epic)))`.

Every node, whatever its step, still carries a leaf `kind` (the kind of the
session it would launch; command, gate, review and land nodes launch no agent
turn of their own).

## Stage 2: Edge Reference Validity

Every `edge.from` and `edge.to` must name a `node.id`, over the full edge set
including `loop_edge: true`.

Error: `edge references unknown node id: <id>`.

## Stage 3: Cycle Detection (Kahn)

Kahn's sort over edges with `loop_edge == false`. A leftover node means a
cycle. Intentional cycles are written as `loop_edge: true` edges.

Error: `topology DAG contains a cycle (excluding loop_edge: true)`.

## Stage 4: Iteration Caps and Loop Guards

- **Per-node cap.** `node.max_iterations`, when set, is at most
  `MAX_ITERATIONS = 32` (`topology_validation.rs:17`). Error:
  `max_iterations <n> exceeds hard cap 32`.
- **Loop guard.** Tarjan's SCCs over the full edge set. A loop region is an SCC
  of size > 1, or a single node with a self-loop. Without `def.until`, at least
  one node in a loop SCC must set `max_iterations`. Error:
  `topology contains a loop region with no termination guard (define until or per-node max_iterations)`.

`compute_scc_regions` returns the loop regions (members sorted by id, regions
sorted by minimum id); the bridge stamps them into the workflow metadata as
`scc_regions`.

---

## Stage 5: Typed Steps and Edge Routing

### Where the data lives

A node's typed kind is `TopologyNode.params["step"]`; the routing of its
incoming edges is `params["when"]`, a map `{source_node_id: EdgeWhen}` on the
**target** node. A node without `step` is a legacy session node; an edge
without a `when` entry is `success`. Both decode strictly (`deny_unknown_fields`):
an unknown field or kind is an error, never ignored. The bridge copies them
into workflow metadata `topology_steps` and `edge_when`; the executor decodes
only from there.

`params.when` may only name a source that has an edge into the node
(`node <id>: when names <from>, which has no edge into it`).

### The five step kinds

| `step.kind` | Fields | What runs |
|---|---|---|
| `session` | `expects_commit` (bool), `pass_content` (bool), `require_verified` (bool), `structured_handoff` (bool) | An ordinary agent session. Succeeds only on a validated `PIPELINE HANDOFF` with `status: complete` and a clean tree. `expects_commit` requires HEAD to differ from the fork commit; `pass_content` renders upstream nodes' full last message into the prompt; `structured_handoff` (#2759, omitted when false) has the handoff-format continuation turn validated against the handoff JSON Schema by the harness (Claude `--json-schema`, Codex `--output-schema` + `-o`). |
| `command` | `op` (a `CatalogOp`) | One closed, daemon-owned op; no agent turn. |
| `gate` | `condition` (a `GateCondition`) | A pure boolean over upstream typed outputs; no agent turn. |
| `review` | `of`, `reviewer {provider, model, effort}`, `max_rounds` (default 2) | An independent review of the commit node `of` produced; no agent turn of the topology's own. |
| `land` | `accepted`, `test_filters` (default empty) | Publishes the accepted commit through the daemon merge queue; no agent turn. |

### Command catalog (`CatalogOp`, rule 7)

The author names an op and typed params. The daemon builds argv and env and
derives the effect class (`check` for every v1 op); the author supplies none of
those.

| `op.name` | Params |
|---|---|
| `cargo_test_focused` | `crate`, `filter?`, `lib_only` |
| `cargo_clippy_crate` | `crate` |
| `cargo_check_crate` | `crate` |
| `rolling_baseline` | `crates?` (1..=8 unique; default `rsi-common`, `rsid`, `rsi`) |

`CatalogOp::validate` checks syntax only: crate names are 1..=64 bytes of
`[A-Za-z0-9_-]` not starting with `-`; a test filter is 1..=128 bytes of
`[A-Za-z0-9_:]`. Workspace membership is checked against the execution's
repository at execute time.

**Forbidden author keys (rule 8).** `argv`, `env`, `script`, `script_path` and
`effect_class` (`TOPOLOGY_FORBIDDEN_AUTHOR_PARAMS`) are refused on **every**
node: `node <id>: author-supplied <key> is refused; command nodes run only
daemon catalog ops (#645)`. At execute time the same check reads the
`key=value` tags the bridge made from unknown params.

### Gate conditions (rule 2)

`GateCondition` is a closed JSON AST tagged by `op`: `eq`, `ne`, `in`,
`exists`, `lt`, `le`, `gt`, `ge`, `and`, `or`, `not`. Limits: depth at most
`GATE_MAX_DEPTH = 16`; `and`/`or` take 1..=32 operands; `in` takes 1..=32
values; ordering comparisons need a finite number. Every `path` is
`nodes.<ancestor_id>.<field>[.<field>…]` with segments of 1..=64 bytes of
`[A-Za-z0-9_-]`.

Static rule: every node a gate path names must be a **forward ancestor** of the
gate (`node <id>: gate path references <x>, which is not an ancestor`). At run
time the operators differ:

- `eq`, `ne` and `in` need a value: a missing (or `null`) path fails the gate
  (`gate_error`). A present value is compared as JSON (numbers compare
  numerically), so a type difference is simply "not equal", not an error.
- `lt`, `le`, `gt`, `ge` need a number: a missing path or a non-numeric value
  fails the gate (`gate_error`).
- `exists` is a presence predicate: a missing or `null` path yields `false`, not
  an error, so `not(exists(...))` is `true` for an absent path. Use it to route
  `gate_false` on absence.
- `and`, `or` and `not` propagate an error from any operand they evaluate
  (`and`/`or` short-circuit, so an unevaluated operand cannot error).

A `gate_error` is never coerced to `false`.

### Edge routing (`EdgeWhen`)

| `when` | Taken when the source ... |
|---|---|
| `success` (default) | succeeded |
| `failure` | failed terminally (not cancelled) |
| `completed` | succeeded or failed terminally |
| `gate_true` / `gate_false` | is a gate whose boolean (`output.fields.value`) matches |
| `verdict_accepted` / `verdict_changes_requested` | is a review whose `output.fields.verdict` matches |
| `review_required` (#2688) | is a `land` node the daemon refused `land_requires_review` (a migration, or a change that needs a pre-land review); no other failure takes it, and that refusal takes no `failure` edge |

An untaken edge never feeds its target. A node whose incoming edges are all
untaken is `skipped` (`dead_path`). A source under legacy
`FailurePolicy::Skip` takes only its `success` edges.

Static edge rules (`validate_graph`):

1. Every edge names two known nodes.
2. `gate_true` / `gate_false` must leave a `gate` node.
3. `verdict_accepted` / `verdict_changes_requested` must leave a `review` node.
4. An edge leaving a review node must not be a plain `success` edge. A review
   that asked for changes still succeeds, so `success` would carry a rejected
   commit onward. The accepted routings are `verdict_accepted`,
   `verdict_changes_requested`, and the outcome edges `failure` and `completed`
   (`completed` also fires for a successfully settled `changes_requested`
   review). The verdict-only restriction applies to a `land` node, whose only input is
   the `verdict_accepted` edge from its review.
5. A `loop_edge` must be a `success` edge.
6. `review_required` must leave a `land` node. A land with such an out-edge
   settles that refusal as a routed `failed(land_requires_review)` instead of
   `blocked`, so the execution continues on the edge (the starters
   `issue-pipeline` and `issue-pipeline-post-review` route it to `pre_review`).
   Without the edge the land still blocks `land_requires_review`.

### Review rules (#1641)

`validate_review` (`topology_validation.rs:467-514`):

- `max_rounds` is in `1..=TOPOLOGY_MAX_REVIEW_ROUNDS` (3, the store's
  per-revision cap).
- `reviewer.model` and `reviewer.effort` are non-empty: the launch is explicit
  (`reviewer.provider` is a typed enum). At upsert and launch the daemon also
  requires the reviewer's vendor family to differ from the author's, and
  that the explicit launch matches an operator `allowed_launches` entry.
- `of` names a node that is a `session` with `expects_commit: true`.
- `of` is a forward ancestor of the review, **or** the fix node whose loop edge
  re-enters the review (`loop_edge` from `of` to the review).

Executions with a review node need an Epic: its Work lives on the Epic's
manager ledger, and an owning manager resolves from the Epic's project. An
operator start without one is refused with a typed reason.

### Land rules (#1641 S2)

`validate_land` (`topology_validation.rs:516-564`):

- `test_filters` has at most `ROLLING_QUEUE_MAX_TEST_FILTERS` entries, each a
  valid `PACKAGE=FILTER` pair (`rsi_common::rolling_queue::valid_test_filter`).
- `accepted` names a **review** node.
- The land node's **only** input is the `verdict_accepted` edge from that
  review, and it is not a loop edge. Another input, or any loop edge into it,
  could publish a commit nobody accepted.
- The land node cannot be the source of a loop edge (it would publish twice).

The daemon enqueues on behalf of the execution's owner (its manager or Epic
lead; for an operator start, the Epic's project manager).

### Custody (execute time)

Not part of `validate_topology_definition`: `plan_topology_custody`
(`crates/rsid/src/topology/custody.rs`) runs when a live workflow is accepted
and refuses a plan it cannot resolve.

- A node forks from the commit of its single non-loop input; with none, from
  the execution's base commit.
- A node with **more than one** input must declare `params.custody.from`
  (`base` or `node:<upstream_id>`); otherwise: `node <id> has multiple inputs;
  custody.from is required`. A `node:` source must be upstream.
- At most one loop custody source per node.
- Inside one SCC a node forks from the source's **current iteration**; outside,
  from the source's latest result. A loop's first node after iteration 0 forks
  from the previous iteration of its loop source.
- Every fork is a daemon-observed exact commit, never a moving ref. A session
  whose tree is dirty at the end is `uncommitted_work`; a catalog op that
  changes its sandbox outside `target/` and `.rsi-tmp/` settles
  `sandbox_mutated` and is never re-run in place; diverged work is preserved
  under a ref and the attempt blocks as `preserved_work` for
  `ResolveTopologyAttempt`.

### Execute-time re-validation

`steps::validate_workflow` runs the same `validate_graph` over the snapshot:
steps may only name known nodes, `edge_when` may only name existing edges, and
each (from, to) pair has at most one routing condition. An author cannot reach
the executor with an unvalidated step even through `ExecuteWorkflow`. Typed
effects (command, gate, review, land) additionally require the durable
executor: with `topology_executor_enabled=false` a live run of one is refused
(`command and gate nodes need the durable topology executor`).

---

## Runtime bounds

| Bound | Value | Source |
|---|---|---|
| Hard cap on iterations | 32 | `MAX_ITERATIONS` |
| Charged attempts per node instance | 3 | `topology/graph.rs` `MAX_ATTEMPTS_PER_NODE` |
| All attempts per execution | 64 | `MAX_NODE_ATTEMPTS_PER_EXECUTION` |
| Session node wall time | 4 hours | `topology/executor.rs` `SESSION_WALL_TIME` |
| Catalog op wall time | 1 hour | `topology/catalog.rs` `CATALOG_OP_WALL_TIME` |
| Review rounds | 1..=3 per node | `TOPOLOGY_MAX_REVIEW_ROUNDS` |
| Command build nodes in parallel | daemon config `topology_max_concurrent_build_nodes` | |
| Session nodes of an agent execution in parallel | 3 | |

A queued landing is not bounded by the node wall time: the queue has its own
gate budget and always settles an entry.

Loop termination at run time is the executor's: `until`, per-node
`max_iterations`, or a lead `/halt`
([spawn-child-grammar.md](spawn-child-grammar.md)), under the hard cap above.

## Failure classes

Recorded on the attempt (`topology/store.rs` `failure`): `session_failed`,
`timeout`, `interrupted`, `lost_before_session`, `lost_after_session`,
`preserved_work`, `custody_refused`, `cancelled`, `preserved_work_discarded`,
`handoff_invalid`, `handoff_blocked`, `sandbox_mutated`, `exit_nonzero`,
`gate_error`, `review_unsettled`, `review_rounds_exhausted`,
`review_reopened`, `review_no_manager_ledger`, `land_admission_lost`,
`land_refused`, `queue_disabled`, `dead_path`, `launch_refused`, `policy_refused`. Losses that are not the node's fault (`lost_*`, `interrupted`) are
never charged against the execution cap.

---

## Name uniqueness

`topologies.name` is unique: a daemon pre-check (`topology_name_exists`) returns
`topology name already exists`, and the `UNIQUE` constraint is the final guard.
`UpdateTopology` excludes the topology's own id so a no-op rename is fine.

## Delete guard

`DeleteTopology` refuses while an Epic session's `workflow_id` points at the
topology (`topology in use by Epic <uuid>: clear the reference before
deleting`). `workflow_id_override` is not checked; a dangling override resolves
to no topology and falls through to the Epic's `workflow_id`.

## Error reference

| `TopologyError` | `InvalidParam` message |
|---|---|
| `DuplicateName` | `topology name already exists` |
| `UnknownNode(id)` | `edge references unknown node id: <id>` |
| `CycleDetected` | `topology DAG contains a cycle (excluding loop_edge: true)` |
| `IllegalNodeKind(kind)` | `illegal node kind: <kind> (must be in legal_children(Some(Epic)))` |
| `IterationCapExceeded(n)` | `max_iterations <n> exceeds hard cap 32` |
| `UnboundedLoop` | `topology contains a loop region with no termination guard (define until or per-node max_iterations)` |
| `NotFound` | `topology not found` |
| `InUse(epic_id)` | `topology in use by Epic <uuid>: clear the reference before deleting` |
| `InvalidStep(message)` | the message itself (every typed-step, routing, review, land and forbidden-key rule above) |

---

## See Also

- [README.md](README.md): overview and "Operational Starters (#1641)", the current typed-step examples
- [rpc-reference.md](rpc-reference.md): RPC errors, plus the typed-step reference under "Agent topology verbs"
- [topology-workflow-bridge.md](topology-workflow-bridge.md): the P1 bridge and the legacy runner (`topology_executor_enabled=false`)
- [spawn-child-grammar.md](spawn-child-grammar.md): runtime spawn-time validation
