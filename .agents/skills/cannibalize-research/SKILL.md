---
name: cannibalize-research
description: Comparative codebase analysis — extract features worth adopting from an external project
---

# Cannibalize Research

Master-agent orchestrator for comparative codebase analysis. Given an external project path, the master dispatches parallel scouts to map both the external project and rsi across five comparison dimensions simultaneously, then synthesizes their compact reports into a prioritized extraction document.

This is explicitly evaluative — the goal is actionable intelligence, not neutral documentation. Workers document what exists; the master decides what's worth stealing.

**Use this when:**
- Evaluating a related or competing project for features worth adopting
- A user shares an external codebase and you need to understand its relationship to rsi
- You want a structured gap analysis with implementation-level detail
- You need to know adoption effort before committing to porting something

**Not for:**
- Purely documenting rsi itself — use `/team_research` for that
- Planning the actual adoption — use `/plan` after this produces the extraction doc


## Comparison Dimensions

All workers analyze their assigned project across these five dimensions. Every report must address all five:

1. **Core functionality** — what the project fundamentally does, its primary user-facing capabilities
2. **Architecture patterns** — structural choices: how components are divided, how they communicate, what the execution model is
3. **API interfaces** — the contracts between components: function signatures, RPC methods, event schemas, CLI surface
4. **Data models** — the key types, structs, schemas, and how state is represented and persisted
5. **Extensibility mechanisms** — plugin points, provider patterns, config systems, hooks, anything designed for external customization


## Step 1: Input Ingestion

Extract the external project path from the user's input. If a focus area is specified (e.g., "focus on the data model"), note it — workers will weight that dimension more heavily.

If no path is provided, respond:

```
Please provide the path to the external project to analyze.

Examples:
  /cannibalize_research ~/jiuwenclaw
  /cannibalize_research ~/some-project "focus on the provider pattern"
  /cannibalize_research /path/to/project

I'll map it against rsi across core functionality, architecture,
API interfaces, data models, and extensibility mechanisms.
```

Then wait for input.

Once a path is provided, verify it exists and is readable before proceeding.


## Step 2: Scout Dispatch Plan

Before spawning any workers, emit the dispatch plan:

```
Cannibalize Dispatch Plan
=========================

External project: [path]
Home project:     rsi (current repo)
Focus area:       [user-specified focus, or "all dimensions equally"]

Scout workers (parallel):
  scout-external [opus]    — map [project name] across all 5 dimensions
  scout-rsi [sonnet]  — extract rsi's feature surface across same 5 dimensions
  scout-thoughts [haiku]   — check thoughts/ for prior research on this project or domain

Synthesis: master compares both scout reports and writes the extraction doc
```

Proceed immediately — do not wait for confirmation.


## Step 3: Worker Dispatch

Spawn all three scouts in a **single message** using the `Agent` tool with `run_in_background: true`.

### scout-external [opus]

The external project is unknown. Opus handles it because it needs to make smart decisions about what to read in an unfamiliar codebase.

```
You are a scout in a comparative analysis. Your job is to map an external codebase
so a master agent can determine what features are worth adopting into rsi.

EXTERNAL PROJECT PATH: [EXTERNAL_PATH]
FOCUS AREA: [focus area or "all dimensions equally"]

EXPLORATION STRATEGY:
1. Start by reading these files if they exist (in order):
   README.md, README, CLAUDE.md, AGENTS.md, docs/ directory
   Package manifest: Cargo.toml, package.json, pyproject.toml, go.mod, etc.
   Entry points: main.rs, src/main.rs, index.ts, main.py, cmd/, bin/, etc.
2. Use LS to map the top-level directory structure
3. Use Glob to find source files by extension
4. Read the most architecturally significant files per the shared preamble's read budget — Grep-then-Read targeted ranges, full-file reads only for files <400 lines
5. Use Grep to find key patterns: trait/interface definitions, config schemas, plugin hooks
6. Prioritize breadth over depth — map the full surface before diving into any single file

WHAT TO EXTRACT across all five dimensions:
- Core functionality: what does this project DO for its user?
- Architecture: how are components divided? what's the execution model?
- API interfaces: what are the major contracts? (function signatures, RPC, CLI, events)
- Data models: what are the key types/structs/schemas? how is state persisted?
- Extensibility: what's designed to be extended? providers, plugins, config, hooks?

Also note: anything surprising or non-obvious that rsi doesn't have.

YOUR FINAL MESSAGE TO THE MASTER MUST CONTAIN ONLY THE SCOUT REPORT BELOW.
Nothing before it. Nothing after it. No reasoning narration. Short illustrative
code snippets (5 lines max) allowed only when essential to describe a pattern.

SCOUT REPORT — EXTERNAL:
=========================
Project: [detected project name]
Path: [EXTERNAL_PATH]
Stack: [language(s), key frameworks/libs, runtime]
Status: COMPLETE | PARTIAL | BLOCKED

Core functionality:
  Summary: [2-3 sentences]
  Key files: [file:line, file:line]

Architecture pattern:
  Summary: [2-3 sentences describing structural choices]
  Key files: [file:line, file:line]

API interfaces:
  [interface/function/RPC name]: [1-sentence description]
  [interface/function/RPC name]: [1-sentence description]
  Key files: [file:line, file:line]

Data models:
  [type/struct name]: [1-sentence description]
  [type/struct name]: [1-sentence description]
  Key files: [file:line, file:line]

Extensibility mechanisms:
  [mechanism name]: [1-sentence description]
  Key files: [file:line, file:line]

Standout features:
  [anything notable not typical for this category of project — 1-2 sentences each]

Gaps: [anything that couldn't be determined from available files]
[EXTRA FIELDS — master appends 1-2 lines for targeted focus areas, leave blank otherwise]
```


### scout-rsi [sonnet]

```
You are a scout in a comparative analysis. Your job is to extract rsi's
feature surface so a master agent can compare it against an external project.

HOME PROJECT: rsi (current working directory)
FOCUS AREA: [focus area or "all dimensions equally"]

WHAT TO EXTRACT across all five dimensions for rsi:
- Core functionality: what does rsi DO for its user?
- Architecture: three-crate structure, TUI/daemon/common split, Unix socket RPC
- API interfaces: RPC methods, LcAction enum, keybindings surface, overlay system
- Data models: Session, ConversationEvent, BusEvent, SessionStatus, SessionProvider, etc.
- Extensibility: provider pattern (Claude/Codex/OpenCode/Local), config system, overlay pattern

Key files to read:
  rsi-common/src/lib.rs — shared types and RPC protocol
  rsid/src/rpc.rs — RPC method surface
  rsi/src/modalkit_types.rs — LcAction enum (full action surface)
  rsi/src/keybindings.rs — keybinding grammar
  rsid/src/session.rs — session lifecycle
  rsid/src/store.rs — persistence layer

Read these per the shared preamble's read budget — Grep-then-Read targeted ranges, full-file reads only for files <400 lines.

YOUR FINAL MESSAGE TO THE MASTER MUST CONTAIN ONLY THE SCOUT REPORT BELOW.
Nothing before it. Nothing after it.

SCOUT REPORT — FLYWHEEL:
=========================
Status: COMPLETE | PARTIAL | BLOCKED

Core functionality:
  Summary: [2-3 sentences]
  Key files: [file:line, file:line]

Architecture pattern:
  Summary: [2-3 sentences]
  Key files: [file:line, file:line]

API interfaces:
  [RPC method / action variant]: [1-sentence description]
  [RPC method / action variant]: [1-sentence description]
  Key files: [file:line, file:line]

Data models:
  [type name]: [1-sentence description]
  Key files: [file:line, file:line]

Extensibility mechanisms:
  [mechanism]: [1-sentence description]
  Key files: [file:line, file:line]

Gaps: [anything not determinable from the key files above]
[EXTRA FIELDS — master appends 1-2 lines for focus areas, leave blank otherwise]
```


### scout-thoughts [haiku]

```
You are a scout checking for prior research on an external project.

EXTERNAL PROJECT: [project name or path]
HOME THOUGHTS DIR: thoughts/

Search for any existing research, plans, or notes that mention:
- The external project name
- Similar projects in the same category
- Prior comparative analysis or feature gap discussions

Use Grep and Glob across the thoughts/ directory.

YOUR FINAL MESSAGE TO THE MASTER MUST CONTAIN ONLY THE SCOUT REPORT BELOW.

SCOUT REPORT — THOUGHTS:
=========================
Status: COMPLETE | NOTHING FOUND
Prior research found:
  [thoughts/path/to/file.md] — [1 sentence on what it covers and relevance]
  [or "none"]
Relevant decisions documented:
  [any prior design decisions relevant to this comparison]
  [or "none"]
```


### Customizing Reports for Focus Areas

If the user specified a focus area, append an extra field to the external and rsi scout prompts:

- `Focus detail: provide deeper coverage of [focus area] — more file:line refs, more interface names`

Max 1 extra field for focus area refinement.


## Step 4: Synthesis + Comparison

After all three scouts report back via TaskOutput:

1. Parse all three reports
2. For each of the five dimensions, build a side-by-side comparison:
   - What does external have here?
   - What does rsi have here?
   - Is it a gap (external only), advantage (rsi only), or shared feature?
3. For each gap (external has it, rsi doesn't), assess adoption complexity:
   - **Low** — isolated feature, no deep architectural dependencies, could be added in 1-2 phases
   - **Medium** — requires new abstractions or touches multiple subsystems, 3-5 phases
   - **High** — requires architectural changes or is deeply entangled with external's unique design
4. Identify architectural differences that affect interoperability — what would break if you tried to integrate parts of the external project directly
5. If a gap is critical to the analysis and the scout report is incomplete, spawn one targeted follow-up worker (synchronous) to fill it


## Step 5: Document Generation

Write to `thoughts/shared/research/YYYY-MM-DD-cannibalize-[external-project-name].md`.

```markdown
date: [ISO timestamp with timezone]
researcher: claude-team
git_commit: [git rev-parse HEAD]
branch: [git branch --show-current]
repository: rsi
external_project: "[path]"
topic: "Cannibalize analysis: [external project name] vs rsi"
tags: [research, cannibalize, comparative-analysis, relevant-component-names]
status: complete
last_updated: [YYYY-MM-DD]
last_updated_by: claude-team

# Cannibalize Report: [External Project Name] vs Rsi

## Executive Summary
[3-4 sentences: what's most worth adopting, biggest architectural difference,
 overall assessment of the external project's relevance to rsi]

## Project Profiles

### [External Project Name]
**Stack**: [language, frameworks, key deps]
**Purpose**: [1-2 sentences]
**Architecture**: [1-2 sentences]

### Rsi
**Stack**: Rust — ratatui TUI + Unix socket daemon + SQLite
**Purpose**: Vim-like TUI for managing multiple AI coding sessions across providers
**Architecture**: Three-crate system (rsi TUI / rsid daemon / rsi-common), JSON-RPC 2.0 over Unix socket


## Features to Cannibalize

*Features the external project has that rsi does not — prioritized by value and ordered by adoption complexity.*

### [Feature Name] — Complexity: low | medium | high
**What it does**: [1-2 sentences]
**How it's implemented**: [2-3 sentences with file:line references from external]
**Adoption path**: [what would need to change in rsi to add this]
**External reference**: `[external:file:line]`

### [Next feature...]


## Rsi Advantages

*Features rsi has that the external project does not.*

| Feature | Rsi implementation | External gap |
|---|---|---|
| [feature] | `[file:line]` | [what they're missing] |


## Shared Features — Implementation Comparison

| Feature | External approach | Rsi approach | Complexity parity |
|---|---|---|---|
| [feature] | [how external does it] | [how rsi does it] | equivalent / theirs simpler / ours simpler |


## Architectural Differences + Interoperability

[Key structural differences. For each difference, note whether it's a barrier to adopting
specific features or just a context difference that doesn't affect feature portability.]

### [Difference 1]
**External**: [description]
**Rsi**: [description]
**Implication**: [what this means for feature adoption]


## Adoption Priority

*Ordered recommendation for what to actually steal, based on value vs. effort.*

| Priority | Feature | Complexity | Rationale |
|---|---|---|---|
| 1 | [feature] | low/med/high | [why this one first] |
| 2 | [feature] | low/med/high | [rationale] |


## Code References

**External project** (`[path]`):
- `[file:line]` — [what's there]

**Rsi**:
- `[file:line]` — [what's there]

## Historical Context
[Any relevant prior research from thoughts/ — from scout-thoughts report]

## Open Questions
[Gaps that need further investigation before adopting specific features]
```


## Step 6: Commit and Push — REQUIRED

Before responding, commit the analysis document and other task-owned files using explicit paths. Never stage the entire `thoughts/` directory or unrelated edits. Leave the tree clean. Push only if the user explicitly asked; then push the current feature branch as a safe fast-forward, never `main`.


## Step 7: Present Findings + Handle Follow-ups

Lead with the extraction hits — what's worth stealing, in priority order. Then cover the architectural differences. Keep it punchy.

If the user wants to drill into a specific feature for adoption:
- Spawn a targeted follow-up worker on just that feature in the external project
- Append findings under `## Follow-up: [feature] [timestamp]`
- Update frontmatter and push


## Final Response Format

```
Cannibalize complete: thoughts/shared/research/[filename].md

Scouts deployed: 3 (parallel)
  External project: [name] — [stack]
  Rsi: mapped across 5 dimensions

Top extraction targets:
  [Feature 1] — [complexity] — [1-sentence value prop]
  [Feature 2] — [complexity] — [1-sentence value prop]
  [Feature 3] — [complexity] — [1-sentence value prop]

Biggest architectural difference: [1 sentence]

Use /plan to start adopting any of these.
```


## Pipeline Mode Override

If your invocation prompt includes `PIPELINE MODE: true`, you are being called from a master pipeline agent. In this mode:

1. Override the Final Response Format entirely
2. Your **terminal message MUST contain ONLY the PIPELINE HANDOFF block** that the master defined in your prompt — nothing before it, nothing after it
3. All scout reports and synthesis narration belong in your working process — the master reads only the handoff block
4. The master parses your handoff to gate the planning stage

This keeps the master's context clean across pipeline stages.
