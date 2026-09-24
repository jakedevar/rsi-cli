# Recursive Master-Implement Capability Gate

This document records the intentionally bounded foundation for recursive
DAG-based master-implement workflows.

For operator-facing usage, fixture setup, TUI controls, and dogfooding
instructions, see [Recursive DAG Operator Guide](recursive-dag.md).

## What Exists

`crates/rsi-eval/src/recursive_dag.rs` provides a deterministic simulation
harness for dynamically injected task DAGs. It is deliberately isolated from
the live daemon and does not call an LLM.

`crates/rsid/src/store/recursive_dag.rs` provides daemon-owned SQLite
persistence and recovery for recursive DAG graphs, tasks, edges, attempts,
injection batches, lifecycle events, and artifacts. Read-only RPC inspection
surfaces expose those persisted read models.

`crates/rsid/src/recursive_dag.rs` provides the Phase 4 fake persisted
scheduler. It runs explicitly through `run_one_step` or bounded
`run_until_idle`, selects runnable tasks from SQLite-backed state, executes a
deterministic fake executor, writes attempts/lifecycle/artifacts through store
APIs, and persists a scheduler report artifact plus `last_stop_reason`.
Phase 5A.1 adds durable scheduler-run records for `run_until_idle` calls only:
each fake scheduler invocation records source, operator, started/completed
times, max steps, step count, terminal stop reason or failure reason, fake
executor mode, and a link to the `scheduler-report` artifact when one is
written. `run_one_step` remains the low-level primitive and does not create a
run record.
Phase 5A.2 adds durable cancellation requests for graph, run, and future
task-scoped cancellation. The fake scheduler checks graph/run cancellation
before the first step, between steps, before runnable task selection, and after
each committed step; cancellation stops the scheduler run with
`cancellation_requested` without adding live executor interruption.
Phase 5A.3 adds daemon-internal scheduler leases for `run_until_idle` records:
each run stores a lease owner, token, heartbeat time, and expiry time. The store
enforces at most one active scheduler run per graph, supports an explicit
global active-run cap policy, and can reclaim stale active runs as
`lease_expired`. The fake scheduler refreshes its lease only at normal
checkpoints; it does not fake mid-step interruption.
Phase 5A.4 adds budgeted restart recovery. Startup now records a recovery pass
with graph-count and wall-clock budgets, processes complete graphs only, and
defers any remaining candidates into visible deferred recovery rows. Deferred
recovery is explicitly continuable through daemon-internal store APIs; no
background recovery loop, timer, queue worker, or automatic continuation exists.
Phase 5A.5 exposes the durable scheduler-run, cancellation, and recovery state
through daemon RPC readbacks. It adds capability flags that distinguish
recursive DAG graph inspection, run inspection, recovery status, recovery
control, fake scheduler control, cancellation control, live execution, and
background scheduling. Control RPCs are disabled by default and become available
only through explicit config gates. The manual scheduler RPC is still fake-only,
requires an explicit `max_steps`, rejects values above 10,000, runs
synchronously, and uses the existing run-record, lease, and cancellation
behavior. No RPC or config field enables live recursive execution or a
background scheduler/recovery loop.
Phase 6.1 adds only the durable live-correlation foundation for future recursive
DAG live attempts: shared read-model types and a `recursive_live_attempts`
SQLite table linking graph, task, scheduler run, recursive attempt, optional
RSI session, provider/model snapshots, sandbox/worktree metadata, status,
timestamps, and recovery/reason fields. Narrow store helpers can create a
placeholder correlation row, attach an already-existing session id, update live
correlation status, and load rows by graph/task/run/session for tests and future
readback. No live executor exists yet, no provider/model call path is added, no
session launch is wired from recursive DAG code, and live execution remains
disabled.
Phase 6.2 adds a daemon-internal disabled live executor adapter. The adapter
loads graph/task/run context, creates a live correlation placeholder, builds an
inspectable launch envelope for a future `master implement` task, transitions
the live row through launching/running/failed states, and targets the existing
`SessionManager::launch_session` boundary through a narrow trait. Phase 6.2 now
also enforces the durable session-persistence contract required by that attach
boundary: a successful live launcher result means the session id is known, the
`sessions` row has been durably persisted, the row is loadable through the store
read path, and only then is it safe to attach the session id to a live attempt.
The durable wait is bounded, does not hold the store lock while polling, and a
timeout is recorded as a failed live attempt without retrying attach. The
disabled adapter defensively verifies this before attach and records a clear
failed live attempt if the row is missing. It is not constructed by RPC, the
fake scheduler, topology graph execution, the TUI, or a background loop. Live
execution remains disabled by default, daemon capability flags still report
live/background execution as unavailable, and config still rejects attempts to
enable live or background recursive execution.
Phase 6.3 adds internal live interrupt handle ownership for future recursive
DAG live attempts. A `recursive_live_interrupts` row durably links a live
attempt, graph, task, scheduler run, recursive attempt, graph/run cancellation
request, and attached session id to an interrupt status. The disabled live
executor now has an internal interrupt method that records requested/sent,
failed/rejected, and interrupted states, and duplicate requests for the same
live attempt are idempotent. Task-scoped modeled cancellation requests are not
runtime-executable live interrupts. The production interrupter boundary
delegates to the existing `SessionManager::interrupt_session` path; it does not
duplicate provider process-kill logic. This remains daemon-internal: no RPC, TUI,
scheduler live-mode, topology, background-loop, heartbeat, crash-recovery,
force-kill escalation, or output-validation path exposes it yet.
Phase 6.4A adds only the live attempt heartbeat foundation. Live attempt rows
can now hold token-owned heartbeat state, expose heartbeat read models, renew
heartbeats with token validation, clear heartbeat ownership when a live attempt
enters a terminal state, and list stale heartbeat-owned attempts for a later
recovery pass. The disabled live executor has internal heartbeat wrapper
methods, but no caller wires them into RPC, TUI, the fake scheduler, topology
execution, or a background loop. No crash recovery, reconciliation, background
heartbeat loop, background scheduler loop, live scheduler execution, model
calls, output validation, or force-kill escalation is implemented in Phase
6.4A. Live execution remains disabled and unreachable.
Phase 6.4B adds deterministic live attempt recovery/reconciliation for
already-persisted live attempts. A bounded startup pass runs after session
restore, reads existing live attempt/session/heartbeat/interrupt rows, and
classifies only durable state that already exists. Terminal linked sessions map
to live attempt terminal states, missing or stale live ownership is marked lost,
pending live interrupts are terminalized as interrupted without sending a new
interrupt, and fresh nonterminal heartbeat/session evidence is left unchanged.
Recovery clears heartbeat ownership only when it terminalizes a live attempt.
It does not launch sessions, interrupt sessions, call providers, validate model
output, schedule live work, expose live recovery through RPC/TUI, or add any
background heartbeat/scheduler/recovery loop.
Phase 6.5A adds only shared `rsi-common` serializable types for future live
recursive DAG output contracts, artifact/test/diff/dependency references,
decomposition payloads, failure/block/cancel payloads, validation issues, retry
decisions, mapping decisions, and validation result read models. These types
define the contract shape for later validators and status readback, but no
validator, persistence mapping, output parsing, RPC surface, TUI surface, model
call, scheduler integration, or background loop is implemented yet.
Phase 6.5B adds a pure deterministic `rsi-common` validator for live recursive
DAG output contracts. It validates parsed envelopes and raw JSON values/strings,
enforces strict raw unknown-field policy while preserving typed serde backward
compatibility, checks outcome-specific evidence, decomposition shape and
dependency cycles, artifact/dependency/diff/test references, cancellation
evidence, and emits deterministic validation status, mapping, retry decisions,
issues, and normalized digests. It is side-effect free and does not add
persistence mapping, validation tables, RPCs, TUI surfaces, live scheduler
execution, model calls, topology integration, output extraction, or background
loops.
Phase 6.5C adds transactional store persistence mapping for already-validated
live output. The daemon store records validation rows, validation artifacts,
raw/normalized output artifact links, issue metadata, mapping and retry
decisions, and linked produced/test/diff artifacts, then applies the validator's
accepted mapping to recursive attempts, tasks, live attempts, decomposition
children, and optional scheduler events in one transaction. The store consumes
the validator result directly and does not reinterpret raw model JSON. It still
does not add RPCs, TUI surfaces, live scheduler execution, output repair loops,
model calls, topology integration, output extraction, or background loops.
Phase 6.5D adds read-only daemon RPC/read-model surfaces over persisted live
attempts, heartbeat state, interrupt state, recovery state, and V60 live output
validation rows. These RPCs inspect already-persisted state only: live attempts
can be listed by graph/task/session/run, individual live attempts can be read
with compact linked-session and artifact/test/diff link context, validation
records and validation issues can be listed or fetched deterministically, and
live heartbeat/interrupt/recovery status can be read back. The capability flags
separate configured read-only live status and validation inspection from live
execution, background scheduling, recovery control, cancellation control, and
fake scheduler control. Phase 6.5D does not add control RPCs, scheduler
execution, repair loops, model calls, TUI surfaces, topology execution, live
execution enablement, or background loops.
Phase 6.5E slice 1 adds only the non-reachable foundation for a future explicit
ordinary recursive DAG live scheduler control gate. The shared wire contract now
names `RunRecursiveLiveScheduler` params and response shapes, and daemon
config/capability readbacks include false-only live scheduler control names.
There is still no registered `RunRecursiveLiveScheduler` route or handler, no
schema migration, no store helper, no `live_session` acceptance for graph, run,
or attempt scheduler rows, no session launch, no topology integration, no model
call, no TUI control, and no background loop.
Phase 6.5E slice 2 adds store-only primitives for future explicit live
scheduler runs while keeping the gate non-reachable. Schema V65 admits
`live_session` for recursive graph execution mode, scheduler run executor mode,
and recursive attempt executor kind; live-specific store helpers can create
live graphs, start live scheduler runs with request/policy metadata, start
live recursive attempts, finish/fail/cancel live runs through existing lease
semantics, and create-or-load live attempt correlations idempotently by
recursive attempt id. Fake scheduler APIs remain fake-only and reject live
graphs/modes. There is still no registered `RunRecursiveLiveScheduler` route or
handler, no disabled handler, no session launch, no model call, no live output
extraction or validation control path, no topology integration, no TUI control,
and no background loop.
Phase 6.5E slice 3 is an internal scheduler refactor only. The fake scheduler
now delegates shared runnable selection, lifecycle promotion, cancellation
observation, lease heartbeat, deterministic attempt/batch id derivation, and
scheduler-report finishing to daemon-internal scheduler core helpers so a later
live driver can reuse the same graph semantics. The fake scheduler still starts
and finishes runs through fake-only store APIs, still writes fake reports in the
same deterministic order, and the live gate remains unreachable: there is still
no registered `RunRecursiveLiveScheduler` route or handler, no disabled
handler, no session launch, no model call, no live output extraction or
validation control path, no topology integration, no TUI control, and no
background loop.
Phase 6.5E slice 4 adds an internal, request-scoped live scheduler driver for
test-only live scheduler runs. The driver reuses the shared scheduler core for
selection, lifecycle promotion, cancellation observation, deterministic attempt
ids, and scheduler run lease heartbeats; starts live-mode scheduler runs through
the slice-2 live store helper; records live-mode recursive attempts; and calls
the existing disabled live executor boundary with fake launchers in tests. The
live executor now uses the idempotent live attempt create-or-load helper, so
driver replay can load an existing live attempt/session correlation without a
second launch. Slice 4 deliberately stops at durable session correlation: after
a fake durable session is attached, the internal driver marks the scheduler run
failed and inspectable rather than extracting output, validating it, committing
recursive outcomes, or pretending the live task completed. Launch failures also
leave failed scheduler/live-attempt/recursive-attempt state visible for
inspection. The live gate remains unreachable: there is still no registered
`RunRecursiveLiveScheduler` route or handler, no disabled handler, no production
`SessionManager` invocation from the driver, no model/provider call, no live
output extraction or validation control path, no topology integration, no TUI
control, and no background loop.

The harness proves these primitives:

- Serializable task nodes, dependency edges, parent-child edges, lifecycle
  events, injection records, and execution attempts.
- A lifecycle state machine covering `Pending`, `Planning`, `Ready`, `Running`,
  `Decomposed`, `BlockedOnChildren`, `Integrating`, `Verifying`, `Succeeded`,
  `Failed`, `Blocked`, and `Cancelled`.
- Transactional decomposition injection: a decomposition is fully validated on
  a candidate graph before any child task or edge is committed.
- Acyclic graph enforcement across dependency and parent-child edges.
- Bounded decomposition by max depth, max fanout, and max descendants.
- Bounded retry behavior per task, with every failed attempt retained.
- Checked deserialization for persisted graph shape: edges must reference known
  nodes, the graph must remain acyclic, parent-child links must match
  `parent_id`, attempts/injection records must reference known tasks, and
  duplicate durable ids are rejected.
- Private graph storage inside the harness: callers must use scheduler and
  injection methods instead of mutating task, edge, attempt, or injection
  collections directly.
- A fake master-implement executor that deterministically succeeds, decomposes,
  fails retryably, fails once then retries, fails permanently, blocks, cancels,
  integrates successfully, or fails integration.
- A dry-run scheduler that selects only dependency-ready nodes, blocks parents
  on children, integrates parents after child success, and stops cleanly when
  the graph is terminal or no runnable work remains.

## Intentional Compromise

The eval gate still uses a serializable in-memory graph so deterministic
capability tests stay hermetic. The daemon path no longer stops at that
simulation: recursive DAG state is persisted in SQLite and the Phase 4 fake
scheduler executes against those persisted rows through store/service APIs.

The remaining compromise is execution scope. Phase 4 intentionally uses only a
fake executor and explicit caller-driven scheduler runs. It does not spawn
provider CLIs, call models, or run a daemon background scheduling loop.

## What It Does Not Do

- It does not spawn Claude, Codex, or any other model.
- It does not enable the disabled recursive DAG live executor adapter.
- It does not launch RSI sessions from recursive DAG scheduling.
- It does not add a daemon background loop.
- It does not add a background recursive DAG recovery loop; deferred recovery is
  visible and must be continued explicitly.
- It does not modify topology templates, graph-runner execution, or TUI
  keybindings.
- It does not add ungated mutation RPCs or live operator controls for recursive
  execution.
- It does not register or handle `RunRecursiveLiveScheduler`; the slice 1
  shared structs and slice 2 store primitives remain non-reachable scaffolding.
- It does not add TUI run surfaces.
- It does not expose live interrupt handles through RPC, TUI, scheduler
  live-mode, topology execution, or a background loop.
- It does not expose live heartbeat controls through RPC, TUI, scheduler
  live-mode, topology execution, or a background loop.
- It does not expose live recovery controls through RPC, TUI, scheduler
  live-mode, topology execution, or a background loop.
- It does not add force-kill escalation, background live heartbeat loops,
  output extraction, output repair loops, output-validation mutation/control
  RPCs, or scheduler/TUI entrypoints for the live adapter.
- It does not map validated live output into persisted task, attempt, artifact,
  live-attempt, or scheduler-run state.
- It does not wire recursive DAG scheduling into topology graph-runner
  execution.

## Verification

Eval focused test:

```bash
CARGO_TARGET_DIR=~/.rsi/tmp/cargo-targets/rsi-target cargo test -p rsi-eval recursive_dag -- --nocapture
```

Daemon persisted scheduler focused test:

```bash
CARGO_TARGET_DIR=~/.rsi/tmp/cargo-targets/rsi-target cargo test -p rsid recursive_dag -- --nocapture
```

The vertical-slice test covers:

1. Root decomposes into `A` and `B`.
2. `A` succeeds directly.
3. `B` decomposes into `B1` and `B2`.
4. `B1` succeeds.
5. `B2` fails once, retries within its retry budget, then succeeds.
6. `B` integrates and succeeds.
7. Root integrates and succeeds.

The daemon persisted vertical-slice test asserts final graph success, persisted
attempts, retry accounting, lifecycle events, artifacts, parent-after-child
completion ordering, clean scheduler stop, durable scheduler report readback,
and read-model inspection from SQLite.

Additional invariant tests cover malformed deserialization rejection,
integration failure/cancel propagation, scheduler step-limit termination,
quarantined graph run records without artifact mutation, partial failure
reports, persisted artifact writes, scheduler-report artifact linkage to run
records, failed post-start scheduler execution, run-record durability after
store reopen, durable cancellation request reopen, graph cancellation before the
first scheduler step, cancellation between fake scheduler steps, run
cancellation scoping, task-scoped cancellation modeling without execution,
scheduler lease durability after reopen, per-graph active run rejection, stale
lease reclaim, global active-run cap rejection, heartbeat token checks,
deterministic runnable ordering, budgeted recovery deferral and continuation,
deferred recovery listing, malformed graph quarantine within a budget,
malformed graph deferral beyond a budget, budgeted recovery idempotence,
deterministic zero-millisecond time-budget deferral before the first graph,
safe-active-graph skipped accounting, failed recovery pass persistence, active
lease and cancellation visibility after deferred recovery continuation, startup
budget construction and environment override, zero graph-budget rejection, empty
continuation idempotence, explicit reopen followed by a second scheduler-driven
decomposition, recursive DAG capability flags with controls disabled/enabled,
RPC readback for scheduler runs and run events, latest recovery and deferred
recovery RPC readback, malformed/unknown RPC parameter handling, disabled
control rejection before mutation, enabled recovery continuation, fake-only
manual scheduler execution with required `max_steps`, graph/run cancellation
request RPC durability and request-id readback, live execution mode rejection,
recursive DAG env gate rejection, alias parity, and no background loop
capability. Phase 6.1 additionally covers live-correlation schema migration,
row round-trips, graph/task/run/session lookups, duplicate session correlation
rejection, missing graph/task/run rejection, live status parse/serialization,
provider/model metadata round-trips, delayed session attachment, and the
invariant that adding the schema does not enable scheduler/session-launch side
effects. Phase 6.2 additionally covers the disabled live executor adapter's
fake-launch path, launch envelope contents, session-id attachment after durable
store verification, missing persisted-session failure without attachment,
duplicate session-correlation rejection, launch failure status/reason
recording, disabled rejection before mutation or launcher calls, fake scheduler
isolation, absence of a live RPC entrypoint, and continued config/capability
rejection for live/background execution.
Phase 6.3 additionally covers live interrupt row migration/readback, attached
attempt interruption through a fake interrupter, idempotent duplicate
interrupts, missing-session durable failure, terminal-attempt rejection, durable
interrupter failure reason recording, graph/run cancellation request linkage,
missing/invalid cancellation request rejection, disabled live interrupt
rejection before mutation, no RPC dispatch path, the production interrupter
delegating only to existing session interruption, and continued fake-scheduler
isolation.
Phase 6.4A additionally covers live heartbeat acquire/update/release, token
validation that rejects wrong-token heartbeats without mutating stale attempts,
terminal-attempt heartbeat rejection, durable terminal release, stale heartbeat
listing, fresh heartbeat exclusion, heartbeat state durability after store
reopen, explicit missing-session rejection for running attempts, disabled live
adapter heartbeat rejection before mutation, no RPC dispatch path, and continued
fake-scheduler isolation.
Phase 6.4B additionally covers stale active heartbeat recovery to lost,
missing-session recovery to lost, completed/failed/interrupted terminal session
mapping, fresh nonterminal heartbeat preservation, launch-incomplete recovery,
pending-interrupt recovery, terminal live attempt idempotence, deterministic
multi-attempt recovery ordering, budget deferral without beyond-budget mutation,
store-only recovery with no launch/interrupt/model call references, no RPC
dispatch path, and continued fake-scheduler isolation.
Phase 6.5B additionally covers pure live output validation for valid success
and decomposition contracts, malformed raw JSON, strict raw unknown-field
handling with typed serde compatibility, missing required fields, empty
decomposition children, child scope bounds, dependency cycles, unknown child
dependency edges, invalid artifact and dependency-output references, untrusted
diff sources, incoherent test counts, retryable/permanent failure mappings,
blocked and cancelled mappings, multiple precise issue locations, and
deterministic issue/result ordering.
Phase 6.5C additionally covers store commits for accepted success,
decomposition, retryable failure, permanent failure, blocked, and accepted
cancelled output; repairable output persistence without a repair loop;
validation digest/id consistency checks; duplicate commit rejection; scheduler
event linkage; and rollback of child/artifact/state writes when decomposition
or artifact persistence fails.
Phase 6.5D additionally covers RPC params serde, configured read-only
capability flags, live-attempt listing and detail readback, V60 validation row
readback, validation issue readback from issue summary JSON, artifact/test/diff
link readback, heartbeat readback, interrupt readback, malformed and unknown RPC
parameters, read-only handler non-mutation, and continued absence of
scheduler/model/control invocation.
Phase 6.5E slice 1 additionally covers `RunRecursiveLiveScheduler` shared
params/response serde and wire shape, live scheduler capability/config defaults
remaining false, env and runtime config rejection of live scheduler enablement,
method-not-found dispatch for the unregistered RPC with no store, session,
topology, or schema mutation, fake scheduler live-mode rejection, and unchanged
fake-only store acceptance.
Phase 6.5E slice 2 additionally covers V65 schema support for persisted
`live_session` graph/run/attempt modes, live scheduler run metadata/policy
round-trips, idempotent live attempt create-or-load by recursive attempt id,
conflicting live attempt correlation rejection, fake-versus-live store API
mode rejection, unchanged fake scheduler behavior, unregistered
`RunRecursiveLiveScheduler`, false live capability, rejected live config
enablement, and no session/model/topology/TUI/background side effects.
Phase 6.5E slice 3 additionally covers unchanged fake scheduler vertical-slice
execution, deterministic runnable ordering, stop reason and scheduler-report
readback, fake-versus-live store API rejection, unregistered
`RunRecursiveLiveScheduler`, false live capability, rejected live config
enablement, and continued absence of session/model/topology/TUI/background side
effects after the internal scheduler core extraction.
Phase 6.5E slice 4 additionally covers internal live driver creation of a
`live_session` scheduler run with fake launchers only, durable fake session
correlation through `recursive_live_attempts`, idempotent live attempt replay
without a second launch, durable failed/inspectable state at the slice-4
boundary and on launch failure, no production `SessionManager` wiring from the
driver, unchanged fake scheduler behavior, unregistered
`RunRecursiveLiveScheduler`, false live capability, rejected live config
enablement, and continued absence of topology/TUI/background/model side
effects.

## Remaining Production Work

- Decide how recursive task ids map to existing `Session`/`Workflow`/`Topology`
  ids.
- Wire a real executor behind the same decomposition contract.
- Add a real heartbeat loop, force-kill escalation if needed, and
  operator-facing control semantics before any scheduler/TUI/topology path can
  invoke the adapter. Phase 6.4B only classifies existing persisted live
  attempts during explicit bounded recovery, Phase 6.5C only commits
  validator-approved output through store APIs, and Phase 6.5D only exposes
  read-only live status/validation readbacks.
- Add TUI/operator surfaces for inspecting recursive DAG readbacks and, where
  appropriate, issuing the gated fake-only control RPCs.
- Wire recursive DAG execution into topology graph-runner only after the fake
  persisted scheduler remains stable under operator controls.
