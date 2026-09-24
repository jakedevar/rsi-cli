# RSI Master-Orchestrate Program Mechanics

Read this reference only for explicit `mode:program`. It defines RSI-specific
registration, no-idle, continuation, and authority-preserving turnover.

## Program Registration And No-Idle

First RSI control action registers the daemon-authoritative program sentinel.
If `rsi_control_program_guard` is advertised, call it with `{}`. Otherwise
use `AgentScheduleWake` or Harness `schedule_wake` with exactly:

```json
{"message":"master-orchestrate program guard","mode":"program_guard"}
```

Do not supply timing, recurrence, name, watch target, session identity, or row
UUID. Record returned id as registration evidence, never as
`continuation_job_id`. Sentinel is program identity, not continuation.

Do not dispatch or perform another RSI control action until registration
succeeds. Exact replay returns the same deterministic row. Only explicit
program-guard registration rearms a closed sentinel; enabling a scheduled-job
row elsewhere does not.

Before an unfinished program turn ends, prove exactly one durable condition:

1. `AgentGetProgress` reports current child terminal watch enabled;
2. one same-session one-shot `AgentScheduleWake` with `mode:"resume"` is
   armed;
3. queue is exhausted; or
4. a human gate names `authority`, `forbidden_scope`, `destructive`,
   `production`, `resource`, or `technical_impasse` with concrete evidence.

Never use Fresh or AgentFresh for no-idle recovery. Queue exhaustion and typed
human gates disable sentinel. Open-queue outcomes leave it as program identity.

Final response carries exactly one valid `orchestration_outcome_v1`.
`child_watch` or `resume_wake` names exact scheduled-job UUID as
`continuation_job_id`. Sentinel id is never that value.

```text
orchestration_outcome_v1: {"schema_version":1,"mode":"program","next_slice_ready":true,"continuation_state":"child_watch","continuation_job_id":"<scheduled-job-uuid>","evidence":"enabled watch row verified through AgentGetProgress"}
```

Allowed continuation states are `child_watch`, `resume_wake`,
`queue_exhausted`, and `human_gate`. Terminal states omit
`continuation_job_id`. `human_gate` adds exactly one `blocker_class` from
`authority|forbidden_scope|destructive|production|resource|technical_impasse`;
all other states omit it. Validate carrier with
`rsi-contract-validate --orchestration-outcome`.

## Review Budget In Program Mode

Portable review budgets remain binding. Budget exhaustion blocks acceptance but
does not permit an idle open queue or another identical review.

At limit, persist findings and choose materially changed work:
`split_required`, `replan_required`, finding disposition under existing
policy, or a real typed gate. A split must narrow or change acceptance scope; a
renamed retry or new source hash alone does not reset the logical-slice budget.
Schedule continuation when authorized work remains.

## Context Continuity

Token counts, context percentages, elapsed time, and lineage generation are
diagnostics unless caller, plan, operator, backend, or provider makes a limit
authoritative. Checkpoint only when required, explicitly limited, operator
requested, or observed context quality no longer supports reliable work.

Prefer clean slice closure. For forced mid-slice checkpoint:

1. freeze dispatch and record exact reason;
2. reconcile queue/tree, current obligation, workers, commits, artifacts,
   findings, remaining gates, and next action; persist `logical_slice_id`,
   `review_rounds_used`, `review_rounds_remaining`, and finding dispositions in
   existing ledger/checkpoint so rotation cannot reset review budget;
3. append one continuity observation when a durable ledger exists;
4. write a resume handoff only when the active backend or program consumer
   requires it;
5. compose successor query naming durable state and exact next action;
6. while predecessor retains authority, reserve one successor through
   `rsi_control_reserve_successor` or `AgentReserveSuccessor` using one stable
   idempotency key.

Before a clean-boundary checkpoint, prove live predecessor control authority,
zero in-flight mutating children, and sole-active-master ownership using current
daemon/DB/process evidence. Forced mid-slice checkpoints instead record every
live or orphaned child and exact state, then avoid conflicting work.

Request includes only successor launch fields and idempotency key. Never supply
caller, predecessor, Epic, parent, lead, candidate, reservation, generation, or
token identity. Persist receipt fields `reservation_id`,
`candidate_session_id`, `state`, and `state_version` in checkpoint before
predecessor settlement. This is mandatory. Exact replay must return same
reservation and candidate; changed content under same key is conflict.

Predecessor remains authoritative until provider establishment and fenced lead
transfer commit. Reservation or launch is not transfer. After committed transfer,
predecessor stops acting. Successor's first RSI control action is program-guard
registration before program output or dispatch.

Generic Fresh/AgentFresh transfers no hierarchy or lead authority. Missing or
uncertain successor support requires reconciliation of exact effect and recovery
owner; never create another writer.

## Minimum Progress

When context permits, every continuation advances one bounded evidenced unit:
harvest a worker, close/disposition a finding, finish a check, complete an
obligation, or reconcile an uncertain effect. Reloading inputs is not progress.

Repeated checkpoints without evidence require strategy audit, narrower inputs or
scope, reuse of existing evidence, and reconciliation of current retry owner.
Generation count alone is never a human gate. Honor real operator caps, pauses,
decisions, provider limits, denied authority, and unsafe or unsettled effects.
