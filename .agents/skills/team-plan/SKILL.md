---
name: team-plan
description: Create implementation plans using a master-worker planning team
---

# Team Create Plan

Master-agent orchestrator for creating implementation plans from tickets and research documents. The master decomposes the scope into codebase domains, dispatches background workers to research and draft plan sections simultaneously, then synthesizes their compact contributions into a coherent, ordered implementation plan.

**Worker preamble (binding):** This command's worker sub-agents MUST load and obey `/home/jakedevar/rsi/.claude/commands/_shared/worker_preamble.md` with `role=planning` before acting. That file defines the read budget, return budget, forbidden-content rules, and failure-mode contract. The rules below COMPOSE ON TOP and may tighten (never loosen) any limit declared there.

**Use this over `/plan` when:**
- The ticket spans 3+ codebase subsystems
- You need daemon + TUI + common types all researched before planning
- The plan is likely to have 5+ phases

**Use `/plan` instead when:**
- The ticket is focused on a single subsystem
- Fast turnaround matters more than parallel domain coverage


## Five-Expert Planning Framework

Every planning decision must pass through all seven lenses:

1. **SWE** (Clean architecture): No duplicate code, correct abstraction boundaries, single responsibility?
2. **Tech Wizard** (Zero-waste correctness): Correct foundation first? **HAS VETO POWER over phase ordering.**
3. **UI/UX Power User** (Information density): UI-dense, keyboard-efficient, no hand-holding?
4. **Systems Performance Engineer** (Runtime efficiency): No unnecessary allocations, event-driven over polling?
5. **Reliability Engineer** (Graceful degradation): Error paths visible, recoverable without restart?
6. **Scalability Expert** (Capacity planning): Stays responsive at 10x sessions/events?
7. **Vim Language Designer** (Compositional grammar): Keybindings compose with vim grammar? **HAS VETO POWER over keybinding design.**

**If any expert objects, resolve the conflict before finalizing the plan.**

Canonical reference: `five-experts.md`


## Step 0: Worktree Detection

If a file path is provided, check for a matching worktree:

1. Extract the filename stem (e.g., `2026-02-10-archived-session-browser` from its full path)
2. Run `git worktree list` — if a worktree path or branch name matches the stem, `cd` into it
3. If no match, proceed in current directory silently


## Step 1: Input Ingestion

1. Read mentioned files in the main context (tickets, research docs, related plans). Honor the shared preamble's read budget: Grep-then-Read targeted ranges, full-file reads only for files <400 lines.
1a. **JSON sidecar probe (RSI-014):** For each research document path among the inputs, the master alone runs the JSON-first read path. Workers continue to receive prose summaries; raw JSON does NOT cross the master/worker boundary.
   1. Compute `<doc>.json = <doc>.md` with `.md` replaced by `.json`.
   2. If `<doc>.json` is missing OR the markdown frontmatter contains `json_companion_status: invalid` → fall back to the markdown read above. No warning.
   3. Otherwise, ensure `target/debug/rsi-research-validate` exists (run `cargo build -p rsi-common --bin rsi-research-validate --quiet` if missing), then run the validator on `<doc>.json`.
   4. On exit `0`: parse the JSON. Use the typed shape directly:
      - `areas[]` → master's Step 2 Domain Decomposition (one worker per area, default).
      - `findings[]` → split by area in the master's working set; forward only the area-relevant findings as a prose summary to each domain worker (saves ~200 tokens per worker dispatch). **Preserve each `findings[].id`** (`F-001`, …) when forwarding: every plan phase/item that traces to a finding declares it under `satisfies:` (see the phase template), the provenance anchor the cross-stage VERIFY pass (`cross_stage_verify_coverage`) checks.
      - `file_refs[]` → preferred file:line dispatch targets for each worker.
      - `open_questions[]` with `blocks_planning: true` → resolve in the master synthesis step; do NOT punt them to workers.
   5. On exit `2` or `1`: log one line on stderr `JSON sidecar invalid: <reason>; falling back to markdown` and proceed via the markdown path.
2. **Do NOT spawn workers yet** — read first
3. Log what was read and extract:
   - The core goal
   - Explicit constraints or requirements
   - Any files/systems already known to be involved

If no input is provided, respond with:

```
I'll help you create a detailed implementation plan using a parallel planning team.

Please provide:
1. The task/ticket description (or path to a ticket file)
2. Constraints, context, or requirements that shape the work
3. Paths to related research or earlier implementations

Tip: /team_plan thoughts/allison/tickets/eng_1234.md
```

Then wait for input.


## Step 2: Domain Decomposition + Worker Assignment

Analyze the input and identify which codebase domains this work touches. For each domain, define:
- **Scope**: specific files/directories to investigate
- **Research questions**: what does the worker need to find out about current state?
- **Plan questions**: what changes are likely needed here to implement the ticket?
- **Model tier**: opus/sonnet/haiku based on complexity

Typical domain breakdown for this codebase:

| Domain | Scope | Typical model |
|---|---|---|
| Shared types / RPC protocol | `crates/rsi-common/` | `opus` — type changes are foundational |
| Database / store | `crates/rsid/src/store.rs`, migrations | `opus` — schema changes are foundational |
| Daemon session logic | `session.rs`, `monitor.rs`, provider files | `sonnet` |
| Daemon RPC handlers | `rpc.rs` | `sonnet` |
| TUI action dispatch | `action_handler/`, `modalkit_types.rs`, `keybindings.rs` | `sonnet` |
| TUI rendering / overlays | `ui/`, `overlay/` | `sonnet` |
| Historical context | `thoughts/` — prior decisions about this feature area | `haiku` |

Emit the Planning Dispatch Plan to the user before spawning:

```
Planning Dispatch Plan
======================

Ticket: [title or goal]

Domain workers (parallel):
  Shared types [opus]   — [what to research + what changes likely needed]
  Daemon RPC [sonnet]   — [what to research + what changes likely needed]
  TUI actions [sonnet]  — [what to research + what changes likely needed]
  Thoughts [haiku]      — [prior decisions to locate]

Plan synthesis: master orders phases after all contributions arrive
```

Proceed immediately without waiting for confirmation.


## Step 3: Worker Dispatch

Spawn all workers in a **single message** using the `Agent` tool with `run_in_background: true`.

Name each worker descriptively: `"planner-common"`, `"planner-daemon-rpc"`, `"planner-tui-actions"`, `"planner-thoughts"`, etc.

### Worker Prompt Template

Fill in for each worker's specific domain:

```
You are a focused planning worker in a master-worker planning team.

TICKET / GOAL: [FULL TICKET TEXT OR GOAL DESCRIPTION]
YOUR DOMAIN: [DOMAIN NAME]
YOUR SCOPE: [specific directories/files to research]

RESEARCH QUESTIONS — answer all of these about current state:
[Bulleted list of specific things master needs to know about this domain today]

PLAN QUESTIONS — propose concrete changes for each:
[Bulleted list of "what changes are needed in this domain to implement the ticket?"]

INSTRUCTIONS:
1. Read relevant files in YOUR SCOPE per the shared preamble's read budget — Grep-then-Read targeted ranges, full-file reads only for files <400 lines
2. Find specific file:line references for all existing code you reference
3. Identify dependencies: what from other domains must exist before your changes?
4. Flag any risks, unknowns, or constraints you discover

FIVE-EXPERT CHECK on your proposals:
- SWE: Clean abstraction, no duplication?
- Tech Wizard: Building on the right foundation? Anything thrown away later?
- Reliability: Error paths handled in proposed changes?

YOUR FINAL MESSAGE TO THE MASTER MUST CONTAIN ONLY THE PLAN CONTRIBUTION BELOW.
Nothing before it. Nothing after it. The master reads only your report.

<return_schema role="planning">
  proposed_changes: list[str, max_words=20, max_items=8]
  file_refs: list[file_path_with_line, max_items=10]
  risks: list[str, max_words=15, max_items=3]
</return_schema>

<forbidden_content>
(Extends the shared preamble's forbidden list — role-specific additions only.)
- Any field that exceeds its max_words / max_items cap — over-budget returns will be rejected and the worker re-dispatched.
</forbidden_content>

**Rule:** Workers contribute only file:line refs and bullet summaries. The plan document (written by the master) still contains code; worker returns must not. The master composes code sections from the file:line refs workers provide.

PLAN CONTRIBUTION:
==================
Domain: [your domain]
Status: COMPLETE | NEEDS_CLARIFICATION | BLOCKED
proposed_changes: [per schema above]
file_refs: [per schema above]
risks: [per schema above]
Dependencies: [what from other domains must exist before this domain's work starts]
Suggested phase: [early | middle | late]
```


## Step 4: Synthesis + Phase Ordering

After all workers report via TaskOutput:

1. Parse each `PLAN CONTRIBUTION`
2. Collect the bullet proposed_changes and file_refs across domains (master will write any code using the file:line refs)
3. If any worker reported `NEEDS_CLARIFICATION` or `BLOCKED`, research that gap yourself (master) before writing the plan — do not leave open questions in the plan
4. Build a dependency graph from the `Dependencies:` fields:
   - Which domains depend on other domains?
   - What must be foundational?
5. Apply **Tech Wizard veto**: reorder if any phase would build on a foundation that will be rewritten
6. Apply **Vim Language Designer veto**: if keybinding changes are involved, verify the grammar before committing to the phase order
7. Group all proposed changes into ordered phases:
   - Phase 1 (always): foundational — shared types, DB schema, migrations
   - Middle phases: independent domain work that can proceed after foundation
   - Late phases: wiring, integration, tests that span multiple domains
8. Ensure each phase has clear, measurable success criteria (automated AND manual)
9. Apply the shared evidence taxonomy during synthesis. Tag every load-bearing
   claim `[observed]`, `[source]`, or `[inferred]`, cite the primary artifact or
   reproducible extraction, and mechanically extract machine-readable facts.
   Prove the current tool surface can express an exact proposed shape before
   treating it as established. `[inferred]` is forbidden in Acceptance Criteria
   and Success Criteria. A plan with any load-bearing `[inferred]` claim must
   execute and reclassify the claim or downgrade readiness; it cannot be labeled
   `decision-complete` or `implementation-ready`, described as `ready for
   implementation`, or given any equivalent readiness promise.


## Step 5: Plan Writing

Write the plan to `thoughts/shared/plans/YYYY-MM-DD[-ENG-XXXX]-description.md`.

Use the standard plan template:

````markdown
# [Feature Name] Implementation Plan

## Overview
[What we're building and why — 2-4 sentences]

## Current State Analysis
[What exists now, key constraints from worker research — file:line references throughout]

## Desired End State
[Specification of end state and how to verify it]

## Evidence Register
- [observed] [claim executed/inspected in this pass — primary artifact/result citation]
- [source] [claim extracted from an authoritative source — exact citation or reproducible extraction]
- [inferred] [unexecuted reasoning — resolve before readiness or explicitly downgrade the plan]

### Key Discoveries:
- [file:line — important finding]
- [file:line — pattern to follow]
- [file:line — constraint to work within]

## What We're NOT Doing
[Explicit out-of-scope items to prevent scope creep]

## Implementation Approach
[High-level strategy and design decisions made, with Five-Expert rationale]

## Phase 1: [Descriptive Title]

### Overview
[The outcome this phase delivers]

satisfies: [F-001, F-014]  <!-- research Finding.id(s) this phase closes; RESEARCH_SCHEMA_VERSION >= 2. Omit only for pure scaffolding with no research antecedent, and say so explicitly — an uncovered item that closes a finding fails the cross-stage VERIFY pass. -->

### Changes Required:

#### 1. [Component or file group]
**File**: `path/to/file.ext`
**Changes**: [summary]

```rust
// the concrete change
```

### Success Criteria:

<!-- [inferred] is forbidden here. Every criterion must be supported by cited [observed] or [source] evidence. -->

#### Automated Verification:
- [ ] [source] [check]: `[command]` — [requirement citation]
- [ ] [observed] [tool/capability check]: `[command]` — [primary result]

#### Manual Verification:
- [ ] [source] [what to test manually] — [requirement citation]


## Phase 2: [Title]
[Same structure...]


## Testing Strategy
[Unit tests, integration tests, manual steps]

## Performance Considerations
[Any performance implications]

## Migration Notes
[If applicable]

## References
- Original ticket: `[path]`
- Related research: `[path]`
- Prior art in this repo: `[file:line]`
````


## Step 6: Commit and Push — REQUIRED

Before responding, commit the plan documents and other task-owned files using explicit paths. Never stage the entire `thoughts/` directory or unrelated edits. Leave the tree clean. Push only if the user explicitly asked; then push the current feature branch as a safe fast-forward, never `main`.


## Step 7: Present and Iterate

1. Present the plan path and a summary of key design decisions made
2. Note any expert-framework conflicts that were resolved and how
3. Let the user know: reply with changes to refine the plan, or run `/team_implement` to execute


## Final Response Format

```
Plan created: thoughts/shared/plans/[filename].md

Team summary:
  Domain workers: [N] (parallel)
  Phases planned: [N]
  Key design decisions:
    - [decision 1 and which expert lens drove it]
    - [decision 2]

Reply with changes to refine. Use /team_implement to execute.
```



## Pipeline Mode Override

If your invocation prompt includes `PIPELINE MODE: true`, you are being called from a master pipeline agent (`/master_implement`). In this mode:

1. Override the Final Response Format entirely
2. Your **terminal message MUST contain ONLY the PIPELINE HANDOFF block** that the master defined in your prompt — nothing before it, nothing after it
3. All reasoning, design decisions narration, and plan summaries belong in your working process — the master reads only the handoff block
4. The master parses your handoff to gate the implementation stage

This keeps the master's context clean across all three pipeline stages.
