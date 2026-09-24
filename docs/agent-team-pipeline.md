# Agent Team Pipeline

The agent team pipeline is a suite of five slash commands that replace single-agent sequential execution with master-worker parallelism across research, planning, and implementation. Each command can be used standalone or chained together via the full autonomous orchestrator.

---

## Pipeline Overview

```
Ticket / Goal
     │
     ▼
┌─────────────────────────────────────────────────────────┐
│  /master_implement  (Opus — pipeline orchestrator)      │
│                                                         │
│  Stage 1 ──► pipeline-research agent                   │
│               invokes /team_research_codebase           │
│               returns: PIPELINE HANDOFF — RESEARCH      │
│                        (compact, ~20 lines)             │
│                             │                           │
│  Stage 2 ──► pipeline-plan agent                       │
│               invokes /team_create_plan                 │
│               receives research handoff as context      │
│               returns: PIPELINE HANDOFF — PLAN          │
│                        (compact, ~20 lines)             │
│                             │                           │
│  Stage 3 ──► pipeline-implement agent                  │
│               invokes /team_implement                   │
│               receives plan path                        │
│               returns: PIPELINE HANDOFF — IMPLEMENTATION│
│                        (compact, ~30 lines)             │
│                             │                           │
│  Step 4  ──► human verification gate (Jake)            │
│  Step 5  ──► master pushes branch                      │
│  Step 6  ──► detailed 3-stage report to Jake           │
└─────────────────────────────────────────────────────────┘
```

Each stage spawns a **foreground** sub-agent (sequential — each must complete before the next). Within each team command, workers run as **background** agents (parallel). This gives two levels of concurrency: sequential pipeline stages, parallel domain workers within each stage.

---

## When to Use What

| Command | Use when |
|---|---|
| `/master_implement` | Large ticket, want zero manual handoffs between research → plan → implement |
| `/team_research_codebase` | Question spans 3+ codebase subsystems, want parallel domain coverage |
| `/team_create_plan` | Ticket touches daemon + TUI + common types, plan likely 5+ phases |
| `/team_implement` | Plan has 3+ phases touching different subsystems, want parallel execution |
| `/research` | Focused single-area question, fast turnaround needed |
| `/plan` | Single-subsystem ticket, fast turnaround needed |
| `/implement` | Small plan (1-2 phases), tightly coupled work, want per-phase human gates |

---

## Command Reference

### `/team_research_codebase`

Decomposes a research question into domain-specific areas, dispatches one background worker per domain in parallel, and synthesizes their compact reports into a research document at `thoughts/shared/research/`.

**Inputs:** Research question or ticket path.

**Outputs:** Research document at `thoughts/shared/research/YYYY-MM-DD-description.md`, committed from task-owned explicit paths. Push only when the invoking session explicitly authorizes it.

**Domain worker model assignment:**

| Domain | Scope | Model |
|---|---|---|
| TUI | `crates/rsi/src/` | `sonnet` |
| Daemon | `crates/rsid/src/` | `sonnet` |
| Common types | `crates/rsi-common/` | `sonnet` |
| Database / persistence | `store.rs`, migrations | `sonnet` |
| Historical context | `thoughts/` directory | `haiku` |
| Targeted deep-dive | 1-2 specific files needing full analysis | `opus` |

Each worker returns a compact **RESEARCH REPORT** (domain, status, key files, findings, architecture pattern, cross-domain connections, gaps). The master never receives raw file dumps — only the report.

---

### `/team_create_plan`

Workers research a domain AND draft their section of the plan simultaneously. The master synthesizes all contributions into a coherent ordered plan rather than reading all the raw files itself.

**Inputs:** Ticket path or goal description.

**Outputs:** Plan document at `thoughts/shared/plans/YYYY-MM-DD[-ENG-XXXX]-description.md`, committed and pushed.

**Domain worker model assignment:**

| Domain | Scope | Model |
|---|---|---|
| Shared types / RPC protocol | `crates/rsi-common/` | `opus` — type changes are foundational |
| Database / store | `store.rs`, migrations | `opus` — schema changes are foundational |
| Daemon session logic | `session.rs`, `monitor.rs`, provider files | `sonnet` |
| Daemon RPC handlers | `rpc.rs` | `sonnet` |
| TUI action dispatch | `action_handler/`, `modalkit_types.rs`, `keybindings.rs` | `sonnet` |
| TUI rendering / overlays | `ui/`, `overlay/` | `sonnet` |
| Historical context | `thoughts/` | `haiku` |

Each worker returns a compact **PLAN CONTRIBUTION** (proposed changes with file:line refs, dependencies, risks, suggested phase). The master applies the Seven-Expert framework to order phases, applying Tech Wizard and Vim Language Designer vetoes before writing the plan.

---

### `/team_implement`

Implements a plan from `thoughts/shared/plans/` using tiered parallel workers. Foundation phases run sequentially first (master); independent phases run in parallel tiers.

**Inputs:** Plan path.

**Outputs:** Feature branch with all phases committed and pushed.

**Phase tier structure:**

```
Tier 0 (master, sequential):
  Schema migrations, new shared types in rsi-common,
  phases with unspecified file lists

Tier 1 (parallel workers):
  All remaining phases whose file lists don't overlap

Tier 2 (parallel workers, after Tier 1):
  Phases that depend only on Tier 1 output

Sequential final (master):
  Integration tests, cross-subsystem wiring
```

Each worker is assigned a model (opus/sonnet/haiku) based on phase complexity, given a strict file scope, and returns a compact **WORKER REPORT** (status, files modified, check results). Workers commit only their scoped files — never `git add .`. The master does the final `git push`.

---

### `/master_implement`

Full pipeline orchestrator. Drives the three team commands in sequence, threading research findings into planning context and plan path into implementation. Owns the human verification gate and final push.

**Inputs:** Ticket path or goal description.

**Outputs:** Pushed feature branch + detailed 3-stage report to Jake.

**Stage agent prompts include:**

- `PIPELINE MODE: true` — activates compact handoff output in each team command
- Pre-populated context from the previous stage's handoff
- Exact PIPELINE HANDOFF block format for the stage agent to fill

The master accumulates three compact handoff blocks (~20-30 lines each) across the full run rather than absorbing three full command outputs. All actual work (research docs, plan docs, code) lives in `thoughts/` and git — the master holds only signals.

---

## Executive Report Pattern

Every team command and every worker uses the same output discipline: **the final message to the caller contains only the compact report block, nothing else**.

This is enforced by a mandatory instruction in every worker and stage-agent prompt:

> YOUR FINAL MESSAGE TO THE MASTER MUST CONTAIN ONLY THE [REPORT BLOCK] BELOW. Nothing before it. Nothing after it. No reasoning, no code snippets, no narration. All of that belongs in your working process — the caller reads only your report.

### Report block formats

**Worker → team command master (WORKER REPORT):**

```
Status: DONE | FAILED | BLOCKED
Phase: [N]
Files modified: [list]
Automated checks:
  [check name]: PASS | FAIL
Failure detail: [if FAILED]
Mismatch: [if BLOCKED — Expected / Found / Why]
[EXTRA FIELDS — master appends 1-2 lines for complex phases]
```

**Team command → pipeline master (PIPELINE HANDOFF):**

```
PIPELINE HANDOFF — RESEARCH | PLAN | IMPLEMENTATION:
=====================================================
[Stage-specific fields — document path, findings, phases, or
 branch/worktree/verification items depending on stage]
```

### Extra fields

Masters can append 1-2 extra field lines to any worker prompt's `[EXTRA FIELDS]` placeholder when a phase or domain needs targeted signal beyond the defaults. Hard cap of 2 — more defeats the purpose of the compact report.

Examples:
- `Design decisions: list any choices that deviated from the plan`
- `Type changes: list all new pub types/traits introduced`
- `Migration required: describe any DB schema migration needed`

---

## Pipeline Mode

Each team command checks for `PIPELINE MODE: true` in its invocation prompt and switches behavior:

| Behavior | Standalone mode | Pipeline mode |
|---|---|---|
| Final response | Full summary with document paths and findings | Only the PIPELINE HANDOFF block |
| Commit/push | Command handles it | Command handles it (except team_implement push) |
| `team_implement` git push | Command pushes | **Skipped** — master pushes after Jake verifies |
| Human verification gate | Pauses mid-command | Deferred to master (items collected in handoff) |
| Original Question Restatement | Appended to response | Omitted — master owns continuity |

The standalone behavior is fully preserved when `PIPELINE MODE: true` is absent — all five commands work independently.

---

## Failure Escalation

No failure is swallowed silently. Every block or failure surfaces to Jake immediately with the branch and worktree path so no work is lost.

| Stage | Failure | Master action |
|---|---|---|
| Research | Agent errors or blocks | Report to Jake, offer narrowed retry |
| Research | Open questions remain | Spawn targeted follow-up worker, continue |
| Planning | Worker blocked | Surface to Jake with options (skip / adjust plan / manual) |
| Planning | Unresolved open questions | Master resolves via codebase research before continuing |
| Implementation | Phase FAILED | Surface to Jake with branch path and recovery options |
| Implementation | Worker BLOCKED | Surface to Jake, wait for guidance |
| Any | Unexpected error | Stop, report full context, preserve partial work |

---

## Source Files

| File | Purpose |
|---|---|
| `.claude/commands/master_implement.md` | Full pipeline orchestrator — drives research → plan → implement |
| `.claude/commands/team_research_codebase.md` | Parallel domain research with compact executive reports |
| `.claude/commands/team_create_plan.md` | Parallel domain research + plan section drafting |
| `.claude/commands/team_implement.md` | Tiered parallel implementation with scoped workers |
| `.claude/commands/research.md` | Single-agent research (use for focused single-area questions) |
| `.claude/commands/plan.md` | Single-agent planning (use for single-subsystem tickets) |
| `.claude/commands/implement.md` | Single-agent implementation with interactive human gates |
# Auto-file note

Eligible settled worker failures may also create one bounded local issue per
lineage. This supplements, and never replaces, the required pipeline handoff
or escalation.

## Bounded codegraph queries

Codegraph queries are read-only over one ready generation; traversal state is
transient and the versioned database remains the only fact authority
(2026-09-23 CG-S2; `crates/rsi-codegraph/src/query.rs`). Searches support exact
name, substring, exact path, and FTS5 seeds. Traversals expose neighbors,
deterministic ordered shortest paths, incoming impact, and evidence-sensitive
graph diff under strict or exploratory provenance filters.

Hard limits are fail-closed and typed: at most 128 results, depth 8, 512 nodes,
1,024 relations, 256 frontier entries, 16 paths, 64 evidence items per fact, a
5-second query deadline, and 512 KiB output. Defaults are narrower, and ordered
partial results carry `complete:false` plus explicit truncation reasons such as
`Timeout`, `Results`, `Depth`, `Nodes`, `Relations`, `Frontier`, and `Paths`.
