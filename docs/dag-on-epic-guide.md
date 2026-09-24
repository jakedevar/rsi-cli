# Using the DAG Feature With an Epic — User Manual

This manual explains how to drive the DAG (topology) feature **inside and on top
of an Epic** in the `rsi` TUI. It describes the system as it exists today.

An **Epic** is a container session that holds leaf children (Story / Task / Bug /
Feature / Refactor / Research). A **topology** is a named DAG of nodes and edges
stored in the daemon. When you attach a topology to an Epic and run it, the
daemon launches one child session per DAG node, in dependency order, parented
under the Epic. That is "a DAG on top of an Epic."

There are two ways to run a topology against an Epic:

1. **One-shot execution** — press `gR` on the Epic. The daemon walks the DAG,
   launches a child session per node in topological order, and parents every
   child under the Epic. Best for a fixed pipeline you want run end to end.
2. **Lead-driven spawning** — promote one child as the Epic's *lead* (`gL`). The
   lead's own output emits `/spawn_child` directives, which the daemon validates
   against the topology's nodes before materializing each child. Best when an
   agent should decide *when* each node runs.

Both paths share the same topology definition and the same parent-child wiring.

---

## Concepts (read once)

| Term | What it is |
|------|-----------|
| **Topology** | A named DAG template stored in the `topologies` table. Has `nodes` (each with an `id`, a `SessionKind`, `prereqs`, and `params`) and `edges` (`from` → `to`, optionally `loop_edge`). |
| **Node** | One unit of the DAG. Becomes one spawned session. Carries `params` like `instructions`, `provider`, `model`. |
| **Edge** | A dependency: `from` must complete before `to` runs. `loop_edge: true` marks a back-edge for iterative loops. |
| **`until` condition** | Optional loop guard on a topology: `LeadHalt`, `MaxIterations(n)`, or `Predicate(...)`. Hard-capped at 32 iterations. |
| **Epic binding** | The topology UUID stored on the Epic's `workflow_id` field. This is what links a DAG to an Epic. |
| **Lead** | One leaf child designated as the Epic's orchestrator (`lead_session_id`). Marked with `★` in the mini-DAG. |
| **Effective topology** | Children don't store the topology id. They resolve it at read time by walking `parent_id` up to the Epic and reading its `workflow_id`. |

Key invariant: **only the Epic owns the topology binding.** Children always
derive it by walking up to the Epic. A child can pin its own topology with a
per-session override (see [Pinning a topology to one session](#pinning-a-topology-to-one-session)).

---

## Quick start (TUI, keyboard-first)

### 1. Create or pick a topology

You need a topology before you can bind it. Either create one in the visual
editor or via the RPC helper.

**Visual editor:** press `gv` to open the graph editor. Inside:

| Key | Action |
|-----|--------|
| `t` | Open the topology picker — load an existing topology |
| `o` | Open the saved-workflow picker |
| `i` | (on a selected node, in detail mode) edit node name |
| `I` | (on a selected node, in detail mode) edit node instructions |
| `r` | Run the loaded workflow |

Authoring a topology from scratch is also done over RPC (see
[Authoring a topology over RPC](#authoring-a-topology-over-rpc)).

### 2. Create the Epic and bind the topology

Press `gE` to open the entity-creation form for an Epic. The form shows a
**Topology** field (Epics only):

| Key | Action |
|-----|--------|
| `n` | Focus Name, type the Epic name |
| `T` | Focus the Tag field |
| `t` | Focus the **Topology** field — type to filter, `Enter` to select |
| `Space` | (in Topology focus) toggle an ASCII DAG preview of the highlighted topology |
| `j` / `k` / `G` | Navigate the filtered topology dropdown |
| `Enter` | Submit the form and create the Epic |

Selecting a topology here writes its UUID onto the new Epic's `workflow_id`.

> If you already created the Epic without a topology, attach one later via the
> `gv` editor (`r` to set an override) or the `UpdateSessionWorkflow` RPC.

### 3. Run the DAG on the Epic

Focus the Epic in the session list and press **`gR`**.

This fires `ExecuteTopology` with `parent_id = Epic.id`. The daemon:

1. Loads the Epic's bound topology.
2. Bridges it into an executable workflow definition.
3. Computes dependency layers (topological sort).
4. Launches one child session per node — nodes in the same layer run
   concurrently — each parented under the Epic.
5. Feeds each completed node's final output downstream to its dependents.

The spawned children appear in the Epic's child session list.

**`gR` preconditions and toasts:**

| Situation | Toast |
|-----------|-------|
| No session focused | `No session selected` |
| Focused session is not an Epic | `Not an Epic — gR only fires on Epic containers` |
| Epic has no topology bound | `Epic has no topology binding — set one via gv overlay first` |

### 4. Watch progress

- The spawned children show up under the Epic — descend into the Epic with
  `Enter` to see them, ascend with `-`.
- Open `gv` and press `Tab` to focus the dashboard panel for run / recovery /
  cancellation state.

---

## The lead-driven path (agent decides when nodes run)

Instead of `gR` running the whole DAG at once, you can let one child *drive* the
DAG by emitting spawn directives.

### 1. Designate a lead

Focus a leaf child of the Epic and press **`gL`** (or `:lead` / `:setlead`).
This sets the Epic's `lead_session_id` to that child. The lead is marked `★` in
the mini-DAG.

Requirements: the candidate must be a leaf kind, must already be a direct child
of the Epic, and must not be archived or deleted. (The first leaf added to an
Epic with no lead is auto-promoted.)

### 2. The lead emits spawn directives

When the lead session's agent output contains a spawn block, the daemon scans
it, validates it against the Epic's topology, and launches the child:

```
<docregblock>
/spawn_child kind=Task topology_node=implement iteration=0
QUERY:
Implement the feature based on the research output.
</docregblock>
```

Header grammar (`/spawn_child` line):

| Field | Required | Meaning |
|-------|----------|---------|
| `kind` | yes | Session kind to spawn (must be a legal child of Epic and match the topology node's kind) |
| `topology_node` | no | The node `id` in the Epic's topology this child binds to |
| `iteration` | no | 0-based iteration index; defaults to `max seen + 1`; capped at 32 |
| `provider` | no | Child provider (`Claude`, `Codex`, `Pioneer`, `Local`, `Antigravity`, `CodexAppServer`, or `Harness`); omit to inherit the lead provider |
| `model` | no | Model override |
| `effort` | no | Effort override |
| `tags` | no | Comma-separated tags |

The body after `QUERY:` is the child's prompt.

To delegate to a different backend, set both the target provider and a model
valid for it. For example, a Codex lead can launch a Claude/Sonnet child:

```
<docregblock>
/spawn_child kind=Task provider=Claude model=claude-sonnet-5 effort=high
QUERY:
Implement the reviewed change and run focused tests.
</docregblock>
```

When `provider` is set but `model` is omitted, the target provider uses its
native default instead of the project's model selected for the lead provider.

### 3. What the daemon validates before spawning

Each directive passes through these gates (silently dropped if any fails):

1. **Dedup** — identical block from the same emitter is ignored.
2. **Lead identity** — the emitter must be the Epic's designated lead.
3. **Child kind** — must be a legal child of an Epic.
4. **Topology node match** (when `topology_node` is set) — the node must exist in
   the Epic's effective topology and its kind must match `kind`.
5. **Prereqs satisfied** — every prerequisite node must already have a completed
   child under this Epic.
6. **Iteration uniqueness** — no existing child with the same
   `(node, iteration)` pair.
7. **Rate limit** — a per-Epic token bucket (capacity 10, refills over 5 min).

If all gates pass, a child session is created with `parent_id = Epic.id`,
`topology_node_id = <node>`, and `topology_iteration = <n>`. The child's
`workflow_id` is **not** set — it derives the topology by walking up to the Epic.

See [`docs/topology-on-epic/spawn-child-grammar.md`](topology-on-epic/spawn-child-grammar.md)
for the full grammar reference.

---

## Pinning a topology to one session

By default a child resolves its topology from the Epic. To override that for a
single session, set its `workflow_id_override`:

- In the `gv` overlay, press **`r`** to pin the loaded topology to the focused
  session. The override supersedes the Epic inheritance walk.

This is useful when one child should run against a different DAG than its
siblings.

---

## Loop topologies

A topology can contain back-edges (`loop_edge: true`) and an `until` condition.
When run via `gR` / `ExecuteTopology`, the daemon partitions the graph into
pre-loop, loop-body, and post-loop regions, then iterates the loop body until the
`until` condition is met:

- `LeadHalt` — loop until the lead signals halt.
- `MaxIterations(n)` — loop `n` times.
- `Predicate(expr)` — loop until the predicate evaluates true.

All loops are hard-capped at 32 iterations regardless of the condition.

A canonical executable example ships in the daemon: the topology
`rpi-with-verify-and-docs` (research → plan → implement → verify → docs, with
loop regions).

---

## Authoring a topology over RPC

If you prefer the command line, build the RPC helper once:

```bash
cargo build -p rsi-common --bin rsi-rpc
alias rpc='cargo run -q -p rsi-common --bin rsi-rpc --'
```

### Create an acyclic topology

```bash
rpc CreateTopology --params '{
  "name": "research-implement",
  "definition": {
    "nodes": [
      { "id": "research", "kind": "Research", "label": "Research",
        "params": { "instructions": "Research the problem.", "provider": "claude" } },
      { "id": "impl", "kind": "Task", "label": "Implement", "prereqs": ["research"],
        "params": { "instructions": "Implement from the research.", "provider": "codex" } }
    ],
    "edges": [{ "from": "research", "to": "impl", "loop_edge": false }]
  }
}'
# → { "id": "<TOPOLOGY_UUID>" }
```

`params` keys recognized by the bridge: `instructions`, `provider`, `model`,
`temperature`, `working_dir`, `sandbox`, `tags`. Unknown keys are preserved as
`key=value` tags on the node.

### Create an Epic bound to it

```bash
rpc CreateContainer --params '{
  "kind": "Epic",
  "name": "My Epic",
  "tags": ["backend"],
  "topology_id": "<TOPOLOGY_UUID>"
}'
```

(Only Epics may carry `topology_id`; the topology must already exist.)

### Run it Epic-scoped (equivalent of `gR`)

```bash
rpc ExecuteTopology --params '{
  "topology_id": "<TOPOLOGY_UUID>",
  "project_id": null,
  "inputs": null,
  "parent_id": "<EPIC_UUID>"
}'
# → { "execution_id": "<EXEC_UUID>" }
```

Omitting `parent_id` runs the topology top-level; the spawned children are
orphans visible only in the flat session list. Supplying `parent_id = Epic.id`
parents them under the Epic (this is exactly what `gR` does).

### Poll / interrupt

```bash
rpc GetWorkflowExecution --params '{"execution_id": "<EXEC_UUID>"}'
rpc InterruptWorkflowExecution --params '{"execution_id": "<EXEC_UUID>"}'
```

### Manage tags

```bash
rpc UpdateSessionTags --params '{"session_id":"<uuid>","tags":["ci","infra"]}'
rpc AddSessionTag --params '{"session_id":"<uuid>","tag":"urgent"}'
rpc ListTags --params '{}'
```

---

## Keybinding reference

| Action | Key | Command |
|--------|-----|---------|
| Open graph editor | `gv` | `:graph` |
| Open recursive DAG browser (read-only/advanced) | — | `:dag` |
| Create Group | `gG` | — |
| Create Epic | `gE` | — |
| Create Story / Task / Bug | `gS` / `gT` / `gB` | — |
| Set Epic lead | `gL` | `:lead`, `:setlead` |
| Run Epic's bound topology | `gR` | — |
| Pin topology to focused session (in `gv`) | `r` | — |
| Move session to a parent | `mp` | — |
| Move session to root | `mo` | — |
| Ascend one container level | `-` | — |
| Ascend or back | `Backspace` | — |

In the `gv` editor: `t` = topology picker, `o` = workflow picker, `R` =
recursive-graph read-only picker, `r` = run / pin, `x` = interrupt.

In `:dag`: `Tab`/`h`/`l` move panels, `j`/`k` move rows, `Enter` loads,
`r` refresh, `R` fake scheduler, `L` live scheduler, `!` control-gate details.

---

## The advanced recursive DAG browser (`:dag`)

`rsi` has a second, separate DAG subsystem — the **recursive task graph** engine
— with its own durable scheduler, attempt tracking, and recovery. It is aimed at
operators and is mostly gated off by default. It is *not* the path `gR` uses.

For normal Epic work, you do not need it. If you want to inspect recursive graph
structure, fake-schedule, or run the gated live scheduler, see the dedicated
[`docs/recursive-dag.md`](recursive-dag.md) operator guide. Open the browser with
`:dag`.

Note: the `gv` editor can render a recursive graph's structure read-only by
pressing `R` (gated on the `gv_render_recursive_origin` daemon capability,
default off).

---

## Troubleshooting

| Symptom | Cause / fix |
|---------|-------------|
| `gR` says "Not an Epic" | You're focused on a Group or leaf. Focus the Epic itself. |
| `gR` says "Epic has no topology binding" | Bind a topology: re-create via `gE` Topology field, or set an override with `r` in `gv`, or call `UpdateSessionWorkflow`. |
| Spawn directive ignored | One of the validation gates failed — wrong lead, unmet prereq, duplicate `(node, iteration)`, or rate-limited. The lead must be the Epic's designated lead. |
| Children don't appear under the Epic | You ran `ExecuteTopology` without `parent_id`. Use `gR` (which always sets it) or pass `parent_id`. |
| Child runs against the wrong DAG | A `workflow_id_override` is pinned on that child. Clear it via `gv`. |

---

## Related documentation

| Doc | Covers |
|-----|--------|
| [topology-on-epic/README.md](topology-on-epic/README.md) | Architecture overview, quickstart, phase history |
| [topology-on-epic/spawn-child-grammar.md](topology-on-epic/spawn-child-grammar.md) | Full `/spawn_child` directive grammar |
| [topology-on-epic/rpc-reference.md](topology-on-epic/rpc-reference.md) | Topology / workflow RPC params and returns |
| [topology-on-epic/topology-validation.md](topology-on-epic/topology-validation.md) | DAG validators, cycle detection, loop rules |
| [topology-on-epic/topology-workflow-bridge.md](topology-on-epic/topology-workflow-bridge.md) | How `ExecuteTopology` bridges a topology to a workflow |
| [recursive-dag.md](recursive-dag.md) | The separate recursive task-graph operator engine (`:dag`) |
| [keybindings.md](keybindings.md) | Complete keybinding table |
