---
name: plan
description: Create detailed implementation plans autonomously through thorough research
---

# Implementation Plan

Turn a ticket, specification, or research document into an executable plan for
the current rsi checkout. Produce a complete first draft with concrete files,
ordering, and checks. Keep questions for the operator only when missing
authority or an unresolved product choice changes the requested outcome.

**Worker preamble (binding):** This command and any sub-agents it spawns MUST load and obey `/home/jakedevar/rsi/.claude/commands/_shared/worker_preamble.md` with `role=planning` before acting. That file defines the read budget, return budget, forbidden-content rules, and failure-mode contract. The rules below COMPOSE ON TOP and may tighten (never loosen) any limit declared there.

## Five-Expert Decision Framework

This application is built for a single user (IQ 150, ADHD, vim devotee). Quality over speed of implementation. Every planning decision must pass through all five expert lenses:

1. **SWE** (Clean architecture): Will this plan create duplicate code? Are the abstractions at the right boundaries? Single responsibility maintained across modules?
2. **Tech Wizard** (Zero-waste correctness): Does this ordering avoid throwaway code? Are we building on the correct foundation first? **HAS VETO POWER over implementation ordering.** If the foundation is wrong, the Tech Wizard blocks until it's fixed — no bolting features onto a layer that will be rewritten.
3. **UI/UX Power User** (Information density): Is the planned UI dense enough for a power user who reads fast and thinks fast? Every action reachable by keyboard in 1-2 keystrokes? No hand-holding, no confirmation dialogs, no progressive simplification?
4. **Cognitive Flow Engineer** (ADHD-aware design): Does this plan protect flow state? Are there latency risks (polling intervals, spinners, forced modals)? Will any transition create a test coverage gap that forces debugging by feel? **Write new tests BEFORE deleting old ones.** Never leave the user without a regression safety net.
5. **Vim Language Designer** (Compositional grammar): Is the action grammar designed BEFORE implementation? Do keybindings compose with vim's verb-noun model (`d` + `is` = kill inner session)? Custom operators that wait for motions, not flat key overloads? **HAS VETO POWER over keybinding design.** The grammar must be coherent before code is written.

**If any expert objects, resolve the conflict before finalizing the plan.** Present the conflict and resolution in the plan document.

Canonical reference with full expert descriptions: `five-experts.md`

## Start

Read the supplied ticket and linked files before searching or dispatching
work. If no task is supplied, request one. Keep any RSI-owned sandbox as the
working tree; do not switch branches or move into another worktree during an
RSI-managed session. Check the active branch, HEAD, and dirty state before
planning.

## Scope Routing — MANDATORY

After reading the ticket/research inputs (Step 1 below) and before spawning the parallel research sub-agents (Step 2), assess scope. If ANY of the following are true, automatically route to `/team_plan`:

- Plan will likely have >3 phases
- Touches >2 crates (e.g., TUI + daemon + common)
- Mixes >2 of {schema migration, RPC surface, UI, persistence, daemon process management}
- Estimated >30 min of focused planning work

Announce the routing decision once:
```
Scope is team-sized (reason: <1 sentence>); routing to `/team_plan`.
```

Continue immediately in the current invocation using the Team Create Plan
workflow in `team_plan.md`. Do not ask for confirmation and do not require the
user to re-invoke another command. If the active harness cannot dispatch team
workers, apply the same domain decomposition and synthesis discipline in the
current session and note the degraded execution mode; do not turn missing team
tooling into a user gate. An explicit user instruction to keep the work solo
overrides this automatic route.

## Plan workflow

1. **Collect source evidence.** Read the stated requirements, the relevant
   code and tests, and current AGENTS.md rules. Discover unknown paths with
   bounded `rg`/`rg --files` searches. Use prior `thoughts/` documents to
   explain decisions, then verify their claims against the current checkout.
   For each cited research markdown file, derive
   its sibling `.json`. If it is absent or the markdown says
   `json_companion_status: invalid`, use the markdown. Otherwise build the
   repository's `rsi-research-validate` binary if needed and validate the
   JSON. Read valid `areas`, `findings`, `file_refs`, and blocking
   `open_questions`; on validation failure, record the error in the trace
   and use the markdown. Preserve each `findings[].id` as its join key.
2. **Investigate by domain.** Trace current behavior through source, tests,
   storage, RPC, and UI as applicable. Delegate independent domains only when
   the active harness supports it and the scope justifies it; built-in
   `Explore` and `codebase-analyzer` may locate and explain code paths.
   Verify returned paths and claims before using them in the plan.
3. **Resolve design choices.** Compare feasible approaches with the Five-Expert
   Decision Framework. Put foundation and migrations before dependent code.
   Reconcile every requirement with the proposed behavior; an unresolved
   contradiction is a blocker to a ready plan. Record the decision and its
   evidence.
4. **Write the plan.** Use
   `thoughts/shared/plans/YYYY-MM-DD-ENG-XXXX-topic.md` for numbered work,
   or omit the ticket segment when none exists. Include scope, current state,
   desired end state, evidence register, design decisions, ordered phases,
   dependencies, tests, risks, and explicit out-of-scope items. Each phase
   names exact files, the intended behavior, and a check that proves it.
5. **Validate and commit.** Check cited paths, phase ordering, contract
   sections, provenance coverage, and success criteria. Run
   `rsi-contract-validate` or `rsi-manifest-validate` when the plan's
   workflow provides those artifacts. Commit the plan and task-owned files
   using explicit paths, then report its location and material assumptions.

## Provenance and evidence

   - **Cross-stage linkage (MWP/ICM — binding):** every phase/item that traces
     to research declares `satisfies: [F-…]` naming the research `Finding.id`(s)
     it closes (see the phase template). This is the provenance anchor
     `validate_plan` and the cross-stage VERIFY pass (`cross_stage_verify_coverage`)
     check: an item that closes a finding but declares no `satisfies:` is
     research→plan→impl drift and fails validation. Pure-scaffolding items with
     no research antecedent MAY omit it — note the omission so it reads as
     intentional, not a gap.
   - **Evidence-tier audit (binding):** classify each load-bearing claim as
     `[observed]`, `[source]`, or `[inferred]`. Cite its primary artifact or
     result. Extract machine-readable facts from source artifacts and check
     that the current tool surface can express the proposed action exactly.

Use these tags in the Evidence Register and success criteria:

- `[observed]`: checked during this planning pass; cite the tool result.
- `[source]`: read from an authoritative requirement or source path; cite it.
- `[inferred]`: reasoned but untested; state the check needed to resolve it.

`[inferred]` is forbidden in Acceptance Criteria and Success Criteria. A plan
with any load-bearing `[inferred]` claim must execute and reclassify the claim
or downgrade readiness; it cannot be labeled `decision-complete` or
`implementation-ready`, described as `ready for implementation`, or given any
equivalent readiness promise. Cite the primary artifact/result for `[observed]`
criteria and the authoritative requirement for `[source]` criteria; a command
name alone is not evidence.

## Plan format

```markdown
# [Task] Implementation Plan

## Overview
## Current State Analysis
## Desired End State
## Evidence Register
- [observed] [claim] — [primary artifact/result]
- [source] [claim] — [exact source]
- [inferred] [claim] — [what must be checked]

## Design Decisions
## Scope and Non-Goals
## Phase 1: [Outcome]
satisfies: [F-001, F-002]

### Inputs
[Named ticket, source files, research IDs, or a bounded discovery budget]

### Process
[Ordered changes with exact file paths and dependencies]

### Outputs
[Files and behavior produced by this phase]

### Verify
[Commands and expected evidence]

### Success Criteria
#### Automated Verification
- [ ] [observed] [check] — [tool result]
#### Manual Verification
- [ ] [source] [specific action and result] — [requirement]

## Testing Strategy
## Risks and Recovery
## References
```

Repeat the phase block as needed. A phase with no research antecedent may omit
`satisfies:` only when the plan says why. Keep verification tied to the
same finding IDs so `cross_stage_verify_coverage` can establish that every
research finding assigned to the plan is checked during implementation.

## Stage contract

### Inputs

The supplied ticket, specification, or research document; named files or a
bounded code-discovery budget; current repository instructions and source.

### Process

Inspect inputs and live code, classify evidence, decide the implementation
order, write the plan, and check provenance and verification coverage.

### Outputs

A committed plan under `thoughts/shared/plans/` with phase-local
`satisfies:` links where research findings apply.

### Verify

Each acceptance item is measurable and backed by `[observed]` or `[source]`
evidence. Confirm every promised command exists on the current tool surface
and every load-bearing citation identifies a primary artifact.

## Final check

- State the positive user-visible end state. When changing a displayed name,
  title, label, or identifier, specify where the user will see it after the
  change. Do not encode data loss as a success criterion.
- Separate automated checks from manual checks. A human gate must have a
  concrete reason; routine implementation choices should be resolved in the
  plan.
- Keep source and plan in agreement. If new evidence changes a design choice,
  revise and commit the plan before implementation proceeds.
- **COMMIT AND PUSH — REQUIRED**: Before responding, commit the plan and other
  task-owned files using explicit paths. Never stage the entire `thoughts/`
  directory or unrelated edits. Leave the tree clean. Push only if the user
  explicitly asked; then push the current feature branch as a safe
  fast-forward, never `main`.
- The plan path under `thoughts/shared/plans/` is the daemon's pipeline input
  for `/implement`; include it in the final report.
