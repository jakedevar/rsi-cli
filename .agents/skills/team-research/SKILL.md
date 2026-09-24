---
name: team-research
description: Parallel team research across codebase domains with compact executive reports
---

# Team Research Codebase

Master-agent orchestrator for comprehensive codebase research. The master decomposes the research question into domain-specific areas, dispatches background worker agents in parallel, and synthesizes their compact executive reports into a single authoritative research document.

**Worker preamble (binding):** Before acting, this command's worker sub-agents MUST load and obey the current repository's project-relative `.claude/commands/_shared/worker_preamble.md` with `role=research` when that file exists. Otherwise they MUST obey the worker contract injected by the active harness. Never resolve a worker contract from an absolute checkout or a different repository. The selected contract defines the read budget, return budget, forbidden-content rules, and failure-mode contract. The rules below COMPOSE ON TOP and may tighten (never loosen) any limit declared there.

**Use this over `/research` when:**
- The question spans 3+ distinct codebase systems
- You need TUI, daemon, AND common types researched simultaneously
- Historical context from `thoughts/` should run in parallel with live code research

**Use `/research` instead when:**
- The question is focused on a single component or subsystem
- Fast turnaround on a narrow area matters more than comprehensive coverage


## Research Principles — CRITICAL

You and all workers are **documentarians, not evaluators**:
- DO NOT suggest improvements or changes unless the user explicitly asks
- Do not grade or critique the code
- DO NOT recommend refactoring or architectural changes
- Report only what exists: where it lives, how it behaves, and how the parts connect

The Five-Expert lenses shape WHAT to document, not what to judge:
1. **SWE** — document abstraction boundaries and ownership divisions
2. **Tech Wizard** — document the foundation as-is, note what builds on what
3. **UI/UX Power User** — document keyboard efficiency and density as implemented
4. **Systems Performance Engineer** — document event-driven vs polling patterns as found
5. **Reliability Engineer** — document error paths and recovery mechanisms as they exist
6. **Scalability Expert** — document how sessions/events scale as designed
7. **Vim Language Designer** — document keybinding grammar as implemented


## Step 1: Question Ingestion + Direct File Reading

1. If the user mentions specific files (tickets, docs, existing research), read them in the main context per the shared preamble's read budget — Grep-then-Read targeted ranges, full-file reads only for files <400 lines
2. Do NOT spawn workers before reading these files yourself
3. Log the research question clearly
4. If no research question is provided, respond with:

```
I'm ready to research the codebase. Please provide your research question or area of interest, and I'll analyze it thoroughly by dispatching a team of parallel research workers across the relevant codebase domains.
```

Then wait for the user's query.


## Step 2: Domain Decomposition

Before defining domains, perform a bounded discovery pass from the active
repository root:

1. Reuse the named files and directories already read during question
   ingestion.
2. Inspect top-level directories plus any workspace or package manifests that
   describe component ownership.
3. Use at most two focused `rg`/glob searches to locate the requested behavior,
   its tests, and historical/project documentation. Do not inventory the whole
   tree or assume a language, build system, directory layout, or product name.
4. Derive independent research domains from the discovered ownership boundaries.
   Generic domain shapes may include a user interface, service/runtime, shared
   contracts, persistence, tests, or historical context, but only when the
   repository evidence actually exposes them.

If discovery yields fewer than three independent domains, route to `/research`
and use its single-agent flow. Do not manufacture workers merely to satisfy the
team shape.

For each domain, define:
- **Scope**: which directories/files to search
- **What to find**: the specific questions this worker must answer
- **Capability/model**: based on the discovered domain's complexity and the
  active provider surface

Preserve the existing complexity routing after domains are discovered:

- ordinary subsystem and persistence investigations use `sonnet`;
- straightforward historical-context or inventory work uses `haiku`;
- a targeted deep-dive into one or two especially complex files uses `opus`.

These aliases select capability after repository discovery; they do not imply
that any particular domain or path exists.

Emit the Research Dispatch Plan to the user before spawning:

```
Research Dispatch Plan
======================

Question: [research question]

Domain workers (parallel):
  [discovered domain] [capability/model]
    scope: [discovered paths/files]
    find: [domain-specific question]
  [discovered domain] [capability/model]
    scope: [discovered paths/files]
    find: [domain-specific question]
  [discovered domain] [capability/model]
    scope: [discovered paths/files]
    find: [domain-specific question]
```

Do not wait for confirmation — proceed immediately after emitting this.


## Step 3: Worker Dispatch

Spawn all domain workers in a **single message** using the `Agent` tool with `run_in_background: true`.

Name each worker descriptively: `"researcher-tui"`, `"researcher-daemon"`, `"researcher-thoughts"`, etc.

### Worker Prompt Template

Fill in for each worker's specific domain:

```
You are a focused research worker in a master-worker research team.

RESEARCH QUESTION (global context): [FULL RESEARCH QUESTION]
YOUR DOMAIN: [DOMAIN NAME]
YOUR SCOPE: [specific directories/files to search]

WHAT TO FIND:
[Bulleted list of specific questions master needs answered for this domain]

RESEARCH RULES:
1. You are a documentarian — describe what EXISTS, never suggest improvements
2. Use Read, Grep, Glob, LS tools to investigate your scope
3. Find specific file:line references for every finding
4. If a file is mentioned, read it per the shared preamble's read budget — Grep-then-Read targeted ranges, full-file reads only for files <400 lines
5. Do NOT critique the implementation or recommend changes

YOUR FINAL MESSAGE TO THE MASTER MUST CONTAIN ONLY THE RESEARCH REPORT BELOW.
Nothing before it. Nothing after it. The master reads only your report.

<return_schema role="research">
  findings: list[str, max_words=20, max_items=5]
  file_refs: list[file_path_with_line, max_items=10]
  open_questions: list[str, max_words=15, max_items=3]
</return_schema>

<forbidden_content>
(Extends the shared preamble's forbidden list — role-specific additions only.)
- Any field that exceeds its max_words / max_items cap — over-budget returns will be rejected and the worker re-dispatched.
</forbidden_content>

RESEARCH REPORT:
================
Domain: [your domain name]
Status: COMPLETE | PARTIAL | BLOCKED
findings: [per schema above]
file_refs: [per schema above]
open_questions: [per schema above]
```


## Step 4: Synthesis

After all workers report back via TaskOutput:

1. Parse each `RESEARCH REPORT`
2. Cross-reference findings across domains — note where domain A's findings connect to domain B's
3. Identify gaps or contradictions between domain reports
4. If a gap is critical and blocks the synthesis, spawn one targeted follow-up worker (synchronous, not background) to fill it
5. Synthesize a coherent, cross-domain picture before writing the document


## Step 5: Research Document Generation

Write the document to `thoughts/shared/research/YYYY-MM-DD[-ENG-XXXX]-description.md`.

Use this structure:

```markdown
date: [ISO timestamp with timezone]
researcher: claude-team
git_commit: [current commit hash from: git rev-parse HEAD]
branch: [current branch from: git branch --show-current]
repository: rsi
topic: "[research question]"
tags: [research, codebase, <component-names>]
status: complete
last_updated: [YYYY-MM-DD]
last_updated_by: claude-team

# Research: [Question]

**Date**: [timestamp]
**Git Commit**: [hash]
**Branch**: [branch]

## Research Question
[Original question verbatim]

## Summary
[3-5 sentences synthesizing all domain findings, directly answering the question]

## Findings by Domain

### [Domain 1]
[Synthesized from worker report — file:line references throughout]

### [Domain 2]
[...]

## Cross-Domain Connections
[How the domains interact — data flows, type dependencies, event paths, RPC contracts]

## Architecture Documentation
[Key patterns, conventions, design decisions as they exist in the codebase]

## Prior Work in thoughts/
[Earlier decisions and research found under thoughts/]
- `thoughts/shared/something.md` — [what it covers]

## Code References
[Consolidated list of all key file:line references from all domains]

## Open Questions
[Gaps that could not be determined — candidates for follow-up research]
```

**Path handling for thoughts/**: Always remove `searchable/` from paths but preserve all other subdirectories. `thoughts/searchable/allison/notes.md` → `thoughts/allison/notes.md`.


## Step 5b: Emit JSON companion + validate (RSI-014)

After the markdown research document is written at `<doc>.md`, resolve `rsi-research-validate` from the active repository root before constructing a sidecar. Never search or build from an absolute RSI checkout and never assume a `crates/` layout.

1. **Present/repo-local:** if `target/debug/rsi-research-validate` is executable, select it and require validation.
2. **Buildable/repo-local:** otherwise run `cargo metadata --no-deps --format-version 1`; if successful metadata contains package `rsi-common` with exact binary target `rsi-research-validate`, run `cargo build -p rsi-common --bin rsi-research-validate --quiet`, select `target/debug/rsi-research-validate`, and require validation. A build failure remains a required-validator failure; it is never `SKIPPED`.
3. **Present/PATH:** if the repository has no exact buildable target but `command -v rsi-research-validate` resolves an executable, select it and require validation.
4. **Unavailable:** only if no repository-local executable exists, successful metadata proves the exact package/target absent (or the active repository has no Cargo manifest), and PATH has no executable, report `Validator: SKIPPED — rsi-research-validate unavailable (no executable and no buildable rsi-common target); markdown remains authoritative; continuing.` Do not create a new JSON sidecar, do not set `json_companion_status: invalid`, and do not overwrite, validate, trust, or delete a pre-existing sidecar.
5. **Probe error:** if the active repository has a Cargo manifest but metadata fails and no executable is available, report the capability-probe error as a failure. It is never `SKIPPED`.
When a validator is selected, emit a structured JSON sidecar so consumer skills can parse a typed contract instead of grepping markdown. The JSON conforms to the v2 schema enforced by `rsi-research-validate` (source: `crates/rsi-common/src/research_schema/`).

6. **Construct the JSON object** from the synthesized working set produced in Step 4. Top-level fields:
   - `version`: `2` (the constant `RESEARCH_SCHEMA_VERSION`; v2 is a strict superset of v1).
   - `research_question`: the original user question verbatim.
   - `areas`: the list of dispatched-domain names from your Step 2 Domain Decomposition (e.g., `["TUI", "Daemon", "Common types", "Database", "Historical context", "Targeted deep-dive"]`). Must be non-empty.
   - `findings`: aggregate each domain worker's `findings[]` (already returned per `<return_schema role="research">`). For each finding string, emit `{ id: <"F-001", "F-002", …>, summary: <≤25 words>, file_ref: <"path:line" or "path:start-end">, confidence: "medium" }`. **Mint `id` as a stable, monotonic join key** (`F-001`, `F-002`, … assigned across the aggregated set) — this is the provenance anchor `/plan` items reference via `satisfies:` and the cross-stage VERIFY pass checks. If a worker reports more findings than `file_refs`, reuse the worker's first `file_ref` for the trailing entries.
   - `file_refs`: aggregate each worker's `file_refs[]`, deduplicate by `path`, expand each `path:N` into `{ path, lines: [N, N] }` and each `path:start-end` into `{ path, lines: [start, end] }` (start ≤ end).
   - `open_questions`: aggregate each worker's `open_questions[]`. Stamp `blocks_planning: false` for v1 unless a question explicitly names a missing prerequisite.
7. **Write** the JSON to `<doc>.json` (sibling of the markdown).
8. **Validate**: run the selected `rsi-research-validate <doc>.json`.
   - On exit `0` → proceed to Step 6 (commit/push). Both files are committed together.
   - On exit `2` (schema rejection) → delete `<doc>.json` (`rm`); insert `json_companion_status: invalid` into the markdown frontmatter (before the closing `---`); proceed to Step 6. The markdown is the authoritative human artifact and must remain committed; consumer skills detect the frontmatter flag and skip the JSON probe.
   - On exit `1` (I/O error) → same fallback as exit 2; surface the I/O error in your final summary to the user.
9. The JSON is opaque to daemon pipeline detection (`PIPELINE_PATH_RE` only matches `.md`); no auto-derive double-fire concern. Validator exit failures are never `SKIPPED`.


## Step 6: Commit and Push — REQUIRED

Before responding, commit the research documents and other task-owned files using explicit paths. Never stage the entire `thoughts/` directory or unrelated edits. Leave the tree clean. Push only if the user explicitly asked; then push the current feature branch as a safe fast-forward, never `main`.


## Step 7: Present Findings + Handle Follow-ups

Present a concise summary of findings to the user. Include:
- The research document path
- 2-3 key findings
- Key file references for navigation

If the user has follow-up questions:
- Spawn new targeted workers as needed
- Append findings under `## Follow-up Research [timestamp]` in the same document
- Update frontmatter: `last_updated`, `last_updated_by`, add `last_updated_note`
- Commit and push the update


## Final Response Format

```
Research complete: thoughts/shared/research/[filename].md

Team summary:
  Domains researched: [N] (parallel)
  Key files identified: [count]
  Cross-domain connections: [count noted]

[2-3 sentence summary of main findings]

Follow-up questions? I'll extend the same document.
```


## Pipeline Mode Override

If your invocation prompt includes `PIPELINE MODE: true`, you are being called from a master pipeline agent (`/master_implement`). In this mode:

1. Override the Final Response Format entirely
2. Your **terminal message MUST contain ONLY the PIPELINE HANDOFF block** that the master defined in your prompt — nothing before it, nothing after it
3. All reasoning, domain findings narration, and document paths belong in your working process — the master reads only the handoff block
4. The master parses your handoff to gate the planning stage

This keeps the master's context clean across all three pipeline stages.
