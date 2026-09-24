# `/spawn_child` Directive Grammar

Source: `crates/rsid/src/session/spawn_directive.rs`

Epic-lead sessions emit `<docregblock>/spawn_child …</docregblock>` blocks to
request child session spawns. The daemon's `monitor::monitor_session` scans
accumulated assistant text for blocks matching `SPAWN_DIRECTIVE_RE`, parses
each one via `SpawnDirective::parse()`, and hands the result to
`SpawnCoordinator::handle()`.

---

## Block Structure

```
<docregblock>
/spawn_child <header-fields>
QUERY:
<multiline body until close tag>
</docregblock>
```

Rules:
- `<docregblock>` must be at the start of a line (not indented). The regex
  anchors on this — indented blocks are silently ignored.
- Header line must start exactly with `/spawn_child ` (note trailing space) or
  equal `/spawn_child` (no fields). Any other string makes `parse()` return
  `Ok(None)` — a different directive type.
- `QUERY:` marker must appear on the next line after the header (trimmed
  comparison).
- Body runs from the line after `QUERY:` until `</docregblock>`. Blank lines
  in the body are preserved.
- Multiple blocks in the same text blob are each parsed independently; the
  regex finds them all. Source:
  `crates/rsid/src/session/spawn_directive.rs:293-310` (regex tests).

---

## Header Fields

All fields are `key=value` pairs, whitespace-separated, on the single header
line. Unknown keys are logged and ignored (forward compatibility).
Source: `crates/rsid/src/session/spawn_directive.rs:78-81`

| Key | Required | Type | Notes |
|-----|----------|------|-------|
| `kind` | YES | `SessionKind` string | See valid values below |
| `provider` | no | `SessionProvider` string | Explicit child backend; omit to inherit the emitter provider |
| `model` | no | string | Model slug, passed through as-is |
| `effort` | no | string | e.g. `high`, `medium`, `low` |
| `topology_node` | no | string | P1.7 — node id in the Epic's topology |
| `iteration` | no | u32 | P1.7 — explicit iteration override |
| `tags` | no | CSV string | P1.7 — comma-separated tag list |

### Valid `kind` values

`Story`, `Task`, `Bug`, `Feature`, `Refactor`, `Research`

Container kinds (`Group`, `Epic`) are rejected — non-leaf kinds cannot be
spawned as children.

### `provider=<SessionProvider>`

Selects the child provider independently of the Epic lead. Accepted canonical
values are `Claude`, `Codex`, `Local`, `Antigravity`, `CodexAppServer`, and
`Harness`; `Gemini` is accepted as the `Antigravity` alias. An invalid provider
rejects the whole directive rather than silently inheriting the lead provider.

- When absent, provider/model/effort retain the historical same-provider
  inheritance from the emitter.
- When present and different from the emitter provider, the child does not
  inherit the emitter model or effort. Supply a target-valid `model` and
  `effort` when required by the target backend.
- A cross-provider request without `model` deliberately uses the target
  provider's native model default, never a project model configured for the
  emitter provider.

---

## Pre-P1.7 Baseline Syntax

Before P1.7, the grammar had three optional fields: `kind`, `model`, `effort`.

```
<docregblock>
/spawn_child kind=Task model=claude-opus-4-7 effort=high
QUERY:
Implement the authentication handler.
</docregblock>
```

This form still works identically after P1.7. Old directives with no
`topology_node`, `iteration`, or `tags` keys parse with those fields as `None`.

---

## P1.7 Extended Syntax

### `topology_node=<node-id>`

Binds the spawned session to a named node in the Epic's resolved topology.

- Value must be non-empty and contain only `[a-zA-Z0-9\-_/]` characters.
  Source: `crates/rsid/src/session/spawn_directive.rs:96-101`
- The daemon looks up the node in `effective_topology(epic)` at spawn time.
- Rejection if: the Epic has no topology, the node id is not in the topology,
  the directive's `kind` mismatches the node's declared `kind`, prereqs are
  unsatisfied, or the `(epic_id, topology_node_id, topology_iteration)` tuple
  is already occupied.

### `iteration=<N>`

Explicit iteration override (u32). Overrides the daemon's auto-increment logic.

- `None` (field absent): daemon computes `max(topology_iteration for matching
  children) + 1`. First spawn of a node gets iteration 0 when no prior rows
  exist. Wait — actually iteration 1 (max=0 → 0+1=1). Use `iteration=0` to
  force the first slot explicitly.
- `Some(N)`: used directly. Still subject to cap enforcement.
- Hard cap: `MAX_ITERATIONS = 32` (`crates/rsid/src/session/spawn_coordinator.rs:44`).
  Per-node `max_iterations` sets a lower cap; the effective cap is
  `min(node.max_iterations, 32)`.

### `tags=<csv>`

Override tag set for the spawned child. Comma-separated, each entry normalized
via `normalize_tag`.

- `None` (field absent): child inherits the emitter's current tag set via
  `tags_for(emitter_id)`.
- `Some(vec![])` (empty value after `tags=`): rejected at parse time with
  `Malformed("invalid tags: must not be empty")`.
- Each tag is normalized before storing; malformed tags reject the entire
  directive.

---

## Examples

### Simple spawn (unbound, no topology)

```
<docregblock>
/spawn_child kind=Research
QUERY:
Investigate the token refresh flow in the auth module.
</docregblock>
```

### Topology-bound spawn

Requires the Epic to have a topology with a node `id="research"` of
`kind=Research`.

```
<docregblock>
/spawn_child kind=Research topology_node=research
QUERY:
Research phase: analyze the authentication subsystem.
</docregblock>
```

Daemon auto-assigns `iteration` (first spawn = 0 if no prior children at that
node, else max+1).

### Explicit iteration

Use when you need deterministic iteration numbering — e.g. retrying a specific
iteration slot after a failure.

```
<docregblock>
/spawn_child kind=Task topology_node=impl iteration=0
QUERY:
First implementation attempt. Focus on happy path only.
</docregblock>
```

### Full P1.7 kwargs

```
<docregblock>
/spawn_child kind=Task model=claude-opus-4-7 effort=high topology_node=impl iteration=2 tags=retry,urgent
QUERY:
Third implementation attempt (iteration=2). Previous two failed linting.
Implement the authentication handler with full test coverage.
</docregblock>
```

### Cross-provider child

Any Epic lead can use a different target backend. For example, a Codex lead
can request a Claude/Sonnet worker:

```
<docregblock>
/spawn_child kind=Task provider=Claude model=claude-sonnet-5 effort=high
QUERY:
Implement the authentication handler with focused coverage.
</docregblock>
```

### Multi-iteration loop fragment

A lead session driving a research→implement loop might emit:

```
<docregblock>
/spawn_child kind=Research topology_node=research iteration=0
QUERY:
Initial research pass.
</docregblock>

<docregblock>
/spawn_child kind=Task topology_node=impl iteration=0 tags=pass-1
QUERY:
First implementation attempt based on research output.
</docregblock>

<docregblock>
/spawn_child kind=Research topology_node=research iteration=1
QUERY:
Follow-up research after implementation feedback.
</docregblock>
```

The loop terminates when the topology's `until` condition fires (e.g.
`LeadHalt`) or when iteration hits the node cap.

---

## Backward Compatibility

The optional `provider`, `topology_node`, `iteration`, and `tags` keys all
default to `None` when absent. A directive identical to the pre-provider
baseline therefore continues to inherit its emitter provider and behaves as
before.

Source (serde test): `crates/rsid/src/session/spawn_directive.rs:391-403`
```
test new_keys_absent_parses_same_as_pre_p17 — passes
```

Unknown header keys are logged and ignored. If a future phase adds new keys,
old daemons drop them silently.

---

## Rejection Reasons

`SpawnCoordinator::handle()` can return `SpawnState::Rejected` with these
topology-related reasons:

| Variant | Cause |
|---------|-------|
| `NoTopologyOnEpic { epic_id }` | `topology_node` supplied but Epic has no topology |
| `TopologyNodeNotFound { node_id }` | Node id not in topology definition |
| `NodeKindMismatch { directive_kind, node_kind }` | Directive kind ≠ topology node kind |
| `PrereqsNotSatisfied { missing }` | Prerequisite nodes have no Completed child under Epic |
| `IterationCapExceeded { iteration, cap }` | Iteration > `min(node.max_iterations, 32)` |
| `DuplicateTopologyBinding { node_id, iteration }` | Another session already occupies this `(epic_id, node_id, iteration)` slot |

Source: `crates/rsid/src/session/spawn_coordinator.rs:79-89`

All rejections are non-fatal — the daemon logs and drops the directive. The
lead session is not notified (no feedback channel in Phase 1).

## See Also

- [README.md](README.md) — Phase 1 overview and Quickstart
- [topology-validation.md](topology-validation.md) — definition-level validation that runs at `CreateTopology`/`UpdateTopology` time
- [schema.md](schema.md) — V47 migration that added `topology_node_id` / `topology_iteration` columns
- [rpc-reference.md](rpc-reference.md) — `LaunchSession` / `CreateContainer` extended params (P1.6)
