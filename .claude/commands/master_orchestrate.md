---
description: Evidence-driven orchestration for bounded implementation slices
capability_class: architect
---

# Master Orchestrate

Carry one bounded implementation slice from current evidence to verified
closure. Select work by unsatisfied evidence obligations, not by a mandatory
phase sequence.

Use this command for a specific implementation slice, a ticket with existing
research or a plan, hazardous work needing independent acceptance, or one slice
inside a larger program. Do not use it for tiny one-shot edits, brainstorming,
planning-only requests, review-only requests, or an unsliced whole feature.

The coordinator owns routing, custody, gates, and acceptance. One primary owner
implements. Additional workers exist only for independent questions, named
risks, or required independent review.

## Layering And Conditional References

Apply three policy layers in precedence order:

1. this portable core;
2. backend policy injected by the active harness;
3. a project overlay or caller-provided gate pack, which may tighten but not
   weaken the first two layers.

During input audit, read a project overlay when present. In RSI repositories,
the conventional overlay is
`.claude/commands/_shared/master_orchestrate_rsi_overlay.md`. The overlay may
link mode-specific references. Read only references activated by current work;
do not load program, provider, or domain detail into unrelated slices.

If no worker mechanism exists, compile the exact next-worker prompt and stop.
Do not imply that a worker ran. The coordinator may implement directly only
when the route permits current-agent work or the user explicitly authorizes it.

## Invocation

```text
/master_orchestrate <plan path | ticket path | bounded goal>
mode: slice | program
start_stage: auto | research | plan | implement | review | verify | smoke
domain_gate_pack: <path>
ledger: <path>
stop_after_slice: true | false
allow_parallel: auto | false | true
limits: none | <caller-defined limits>
```

Defaults: `mode:slice`, `start_stage:auto`, `stop_after_slice:true`,
`allow_parallel:auto`, and no caller-defined resource limits.

`stop_after_slice:true` means finish applicable obligations for the current
slice, then stop before selecting another. It never means stop immediately
after implementation.

## Safety Invariants

These survive every route and optimization:

- Bind work, checks, and review to the exact source/specification revision.
- Preserve backend authority, sandbox custody, protected-branch, approval, and
  destructive/production gates.
- Keep one mutating owner per conflict domain. Parallel work must have disjoint
  files, artifacts, and external effects.
- Distinguish reported, observed, reviewed, accepted, integrated, and delivered
  states. A worker exit or valid envelope is not acceptance.
- Reconcile an uncertain dispatch or external effect through its original
  identity and owner before retrying. Never hide uncertainty with a new key.
- Preserve current evidence. Do not repeat research, planning, verification, or
  review already valid for the exact current revision and policy.
- Never accept unresolved blocking findings. A review limit caps repetition,
  not correctness.
- Reconcile contradictory requirements before implementation. Do not silently
  choose one side.

## Adaptive Routing

Choose the lowest tier supported by current facts:

| Tier | Route |
| --- | --- |
| Tier-0 | Current agent handles atomic, deterministic work; no child review. |
| Tier-1 | One primary owner; investigate or review only for a named evidence gap or risk. |
| Tier-2 | One primary owner plus required independent exact-revision review and hazardous-work gates. |

Tier-2 includes schema, auth, custody, concurrency, integration, destructive
lifecycle, dangerous external effects, and disputed-risk work.

Child count, elapsed time, token use, and successor generations have no portable
default ceiling. Explicit operator, caller, plan, overlay, backend, or provider
limits remain binding. Review rounds are bounded separately because another
review of the same scope is not an independent progress strategy.

### Default Review Budget

A review round is one independent reviewer invocation against one logical
slice. A delta re-review counts as a round. A source revision or renamed attempt
does not reset the slice budget.

| Route | Default budget |
| --- | --- |
| Tier-0 | 0 rounds |
| Tier-1 | At most 2 rounds: initial review when a named risk requires it, plus one delta re-review only if the fix changes that risk |
| Tier-2 | 2 rounds: initial review, then one finding-focused delta re-review after fixes |
| Explicit hazardous specialist gate | At most 3 rounds: Tier-2 budget plus one different specialist |

An explicit stricter budget wins. Expanding a budget requires explicit caller,
plan, overlay, or backend policy; the coordinator cannot expand it because a
finding remains open.

At budget exhaustion:

1. keep acceptance blocked while blocking findings remain;
2. do not dispatch the same broad review again;
3. record every finding disposition and choose `split_required`,
   `replan_required`, `spec_conflict`, `technical_impasse`, or a real authority/
   production/destructive gate;
4. continue only through a materially changed, bounded action. A new slice must
   have narrower or changed acceptance scope, not a renamed retry.

Nonblocking findings may be deferred only through project policy with a durable
owner. Review count alone is never a human gate.

## Evidence-Obligation Routing

Audit current source, artifacts, and receipts before dispatch. Determine which
obligations remain:

1. **Understand/design** — facts, constraints, interfaces, risks, and a decision
   are missing.
2. **Implement** — current source does not meet explicit criteria.
3. **Verify** — required deterministic or end-to-end evidence is absent or stale.
4. **Review** — route policy requires independent semantic judgment for current
   source.
5. **Document** — final source changes a user-facing or public contract.
6. **Close** — accepted evidence or durable state has not been reconciled.

Start at the earliest unsatisfied obligation. Stage labels remain compatibility
metadata for existing handoffs and validators; they are not mandatory worker
boundaries.

### Combined Investigation And Design

Use one investigation/design owner instead of separate research and planning
workers when all are true:

- one bounded uncertainty or tightly coupled set of questions;
- exact current baseline;
- no independent competing investigation needed;
- no unresolved authority or production decision;
- one context can produce source-backed facts, explicit design choice,
  alternatives, affected interfaces/files, checks, rollback, and open questions.

Keep research and planning separate only when questions are independently
parallelizable, a required artifact has separate custody, or a planning decision
must be reviewed after research settles. If an existing consumer requires both
artifact types, one owner may produce both without creating another session.

## Harness And Input Audit

Before mutating dispatch:

1. identify the available worker/control surface and apply injected backend
   semantics exactly;
2. read the bounded input, active project overlay, applicable conditional
   reference, and gate pack;
3. record slice identity, repo, exact HEAD, branch, constraints, allowed files,
   forbidden changes, conflict domain, required checks, review budget, and next
   unmet obligation;
4. run `git status --short`, `git rev-parse HEAD`, and
   `git branch --show-current` in the actual custody worktree;
5. reconcile any ledger or durable state against source rather than trusting its
   status text.

Do not dispatch a mutating owner into unexplained dirty state. User-owned dirty
files may coexist only after the user declares them compatible and file
ownership is disjoint.

Parallel work is allowed only when it shortens the critical path and conflict
domains, touched files, artifact paths, and effects are disjoint. Schema,
security, shared protocol, and live external effects keep one mutating owner;
read-only evidence work may still be independent.

## Worker Assignment And Return

Use the backend's capability classes:

- `architect`: ambiguous design, cross-subsystem or correctness-critical work,
  planning that cannot be combined cheaply, and independent semantic review;
- `implementer`: scoped implementation, investigation/design, and bounded fixes;
- `lookup_fast`: deterministic lookup, format, lint, build, test, or smoke work.

Escalate one class only when output proves capability insufficiency. Authority,
credentials, malformed input, formatting, resource, and external-state failures
are not model insufficiency. Explicit caller model/effort choices remain pinned.

Every assignment states only decision-relevant inputs:

```text
role and evidence obligation
goal and acceptance criteria
repo and exact source/spec revision
allowed files and forbidden changes
applicable overlay/gate reference
required checks or review focus
artifact requirements, only when a consumer requires them
review round and remaining budget, when applicable
```

Do not ask workers to re-summarize prior stages or author bookkeeping documents.
Detailed logs stay in source tools or durable storage. Workers return the compact
handoff required by the active backend/project validator. Portable fields are:

```text
PIPELINE HANDOFF — <STAGE>:
Status: complete | partial | blocked | human_gate
doc_path: <absolute existing or created main artifact; required except VERIFY>
manifest_path: <absolute path; required for IMPLEMENTATION and VERIFY>
Commit: <sha, when source changed>
Findings count: <review only>
Daemon checks: <passed>/<total, VERIFY only>
Failed checks: <titles, required for non-complete VERIFY>
Blocker: <required when non-complete>
Blocker class: <required only for blocked/human_gate>
Blocker evidence: <required when non-complete>
Next action hint: <optional>
```

Reference an existing source artifact when no new document is due; never author
a placeholder solely to satisfy `doc_path`. Append any project-required Stage
contract block in canonical `Inputs`, `Process`, `Outputs`, `Verify` order.

Use a project validator when present. Formatting defects that can be normalized
without changing meaning are repaired by the current coordinator, not another
worker. Re-read source-of-truth files or typed state when detail is needed.

## Implementation, Verification, And Documentation

One primary owner implements the bounded slice, adds tests proportional to risk,
and updates required user-facing documentation in the same source revision.
There is no separate documentation worker by default.

Documentation is due only for changed user-visible behavior, keybindings, RPCs,
schemas, configuration, or public contracts, plus explicit project obligations.
Internal closure prose and an authored `none required` artifact are not due.
The coordinator records the docs decision in durable state or the final report.

Before acceptance, run checks selected by changed behavior and policy. Portable
minimum: `git diff --check`, focused automated tests, and relevant build/lint.
Run smoke or end-to-end checks when the behavior cannot be established below
that boundary. A reported command is a claim until its required observation is
captured at the current revision.

Tier-2 receives one independent review after deterministic checks are available.
A fix owner receives only open findings and affected criteria. Delta re-review
checks prior finding dispositions, changed hunks, and newly affected risk—not
the entire original investigation. Follow the review budget above.

## Closure

Close only after source, findings, checks, docs obligation, and applicable
integration state agree. Update an existing ledger or typed store; do not create
a Markdown ledger solely to satisfy this command.

Record or report:

- slice and final status;
- exact repo/head and repo state;
- touched files and conflict domain;
- implementation/review identity when applicable;
- check commands and observed outcomes;
- docs paths or concise not-due reason;
- unresolved risk and next allowed work.

## Program Mode

Program mode coordinates a queue but still completes one bounded slice at a
time. Read the active backend/project program reference before the first control
action. Its registration, continuation, succession, and no-idle rules are
binding.

Reconcile queue state against source, select the earliest authorized unmet
obligation, finish the current slice, then update durable state before selecting
another. `stop_after_slice:false` permits direct continuation only while repo,
ownership, review budget, and gates permit it.

Budget exhaustion, reclassification, or review findings do not authorize false
completion or duplicate work. Preserve the exact continuation owner required by
the backend. A context checkpoint retains accepted evidence and current finding
state; it never restarts completed phases.

## Final Report

Return concise, source-backed status:

```text
ORCHESTRATION COMPLETE
Slice:
Status:
Repo/head:
Repo state:
Route and review rounds used:
Evidence completed:
Checks:
Documentation:
Remaining risk:
Next allowed work:
```

When the backend requires a machine-readable program outcome, emit exactly one
`orchestration_outcome_v1` carrier and validate it with the supplied validator.
An open queue may end a turn only in a backend-proved continuation state. Queue
exhaustion and an evidenced typed human gate are the only idle terminal states.
