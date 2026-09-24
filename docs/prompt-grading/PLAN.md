# RPI Prompt Refactor — Implementation Plan

## Goal
Refactor the 7 RPI (Research-Plan-Implement) slash commands to eliminate compounding context bloat, replace vague guidance with numeric contracts, and establish a single shared worker preamble as the source of truth. This is the foundation on which the RSI recursively-self-improving agent harness will mutate prompts — so getting the contracts crisp now determines whether RSI can measure and improve itself later.

## Context & Background
RPI is a three-stage pipeline — `research_codebase` → `create_plan` → `implement_plan` — with team variants (`team_research_codebase`, `team_create_plan`, `team_implement`) that fan out to worker sub-agents, and `master_implement` which orchestrates the full pipeline end-to-end. The grading pass in `/home/jakedevar/rsi/docs/prompt-grading/` (see `research_codebase.md`, `implement_plan.md`, `team_create_plan.md`) found three systemic problems:

1. **Full-file read mandate** — "Read files FULLY — never use limit/offset" appears ~7 times and single-handedly blows the context window on any real codebase.
2. **Master compounding bloat** — `master_implement` inlines each stage's findings into the next stage's prompt, so by Step 6 the master is carrying the entire research doc + entire plan doc + entire implementation handoff in-context instead of re-reading from disk.
3. **Unbounded worker returns** — team workers have no token budget, no forbidden-content list, and no typed schema, so they return verbose prose (often with inlined code) that the master must then hold in memory.

The fix is sequenced as four PRs: immediate relief, master rewrite, structured returns, shared preamble extraction. The last PR is the actual RSI foundation — one file the meta-harness can mutate to improve every command at once.

## Success Criteria
- Zero occurrences of the phrase `Read files FULLY` (or equivalent "never use limit/offset") across the 7 project-local commands. Verified by `grep -ri "read.*fully\|never use limit" .claude/commands/`.
- Every team command references the shared worker preamble by path (no duplicated preamble text).
- `master_implement.md` Step 2 prompt, when rendered, is ≥80% smaller than current (measured in characters of the prompt template, excluding the research-doc path).
- Every worker return schema has a numeric `max_words` / `max_items` on every field.
- Every worker prompt template includes an explicit `<forbidden_content>` list banning code snippets, full-file contents, and verbose prose.
- `implement_plan.md` and `team_implement.md` contain an explicit stop condition (3× failure → halt + report).
- `implement_plan.md` contains a `ToolSearch` preflight step for `EnterWorktree`.
- A new `<handoff_contract>` XML block exists in `master_implement.md` defining the minimal stage-return fields.
- Running `master_implement` on a small ticket end-to-end shows ≥3× reduction in master's peak context token count vs the pre-refactor baseline.

## Out of Scope
- Replacing the crude character-count context measurement with Anthropic API `usage` accounting — tracked separately; that effort wires usage telemetry into the harness itself.
- Sliding-window memory compression / automatic context eviction — future work once the RSI meta-harness is online.
- Updating the `~/.claude/commands/` global variants (`research_codebase_nt.md`, `create_plan_nt.md`, `create_plan_generic.md`, `research_codebase_generic.md`). Scope is the 7 project-local commands only; global variants can follow in a second pass.
- Behavioral changes to what each stage *does* (scope, outputs, thoughts-dir conventions). We are refactoring the prompts, not the pipeline semantics.
- Migrating handoff blocks from prose+XML to JSON/YAML — decided against (see Ambiguity Resolutions).

## Phases

### Phase 1 — Low-risk immediate relief
**Files touched:** `.claude/commands/research_codebase.md`, `.claude/commands/create_plan.md`, `.claude/commands/implement_plan.md`, `.claude/commands/team_research_codebase.md`, `.claude/commands/team_create_plan.md`, `.claude/commands/team_implement.md`

**Changes:**
- Grep each of the three solo commands for `Read files FULLY`, `never use limit`, `never use offset`, `full file`, and equivalents. Replace every hit (~7 total) with the canonical replacement:
  > *"Read targeted ranges via Grep-then-Read. Full-file reads only for files <400 lines. Budget: ≤8k tokens of file reads before acting."*
- Create `/home/jakedevar/rsi/.claude/commands/_shared/worker_preamble.md` as a stub containing:
  - Return budget: `≤250 tokens default` (research), `≤400 tokens` (planning), `≤300 tokens` (implementation) — annotated so team commands can reference the appropriate variant.
  - Forbidden-content list: no code snippets, no full-file contents, no verbose prose, no recap of the prompt, no hedging.
  - The rule: **"file:line refs are free; prose is taxed."**
- Add an "Include by reference" line near the top of each team command pointing at the shared preamble. (Full extraction / deduplication happens in Phase 4; this phase just plants the file and the reference.)
- Append an explicit stop condition to `implement_plan.md` and `team_implement.md`:
  > *"If automated checks (build/test/clippy) fail 3× on the same phase, STOP. Report the failing diff, the last error output, and the attempted fixes. Do not loop — hand control back to the user."*
- Add a `ToolSearch` preflight instruction to `implement_plan.md`, immediately before the first `EnterWorktree` reference:
  > *"Preflight: `EnterWorktree` is a deferred tool. Before calling it, invoke `ToolSearch` with `select:EnterWorktree,ExitWorktree` to load the schemas. Same applies to any other deferred tool listed in the environment reminder."*

**Success check:**
- `grep -ri "Read files FULLY\|never use limit\|never use offset" /home/jakedevar/rsi/.claude/commands/` returns zero hits.
- `/home/jakedevar/rsi/.claude/commands/_shared/worker_preamble.md` exists.
- Each team command contains a reference to the shared preamble path.
- `grep -n "fail 3" /home/jakedevar/rsi/.claude/commands/implement_plan.md /home/jakedevar/rsi/.claude/commands/team_implement.md` returns matches in both.
- `grep -n "ToolSearch" /home/jakedevar/rsi/.claude/commands/implement_plan.md` returns a match.

**Risk:** Low — additive constraints, no behavioral inversion. Worst case: a command reads slightly less of a file than before and asks for a targeted follow-up read.

---

### Phase 2 — `master_implement` compounding-bloat fix
**Files touched:** `.claude/commands/master_implement.md`

**Changes:**
- **Step 2 rewrite (research → planning handoff):** Replace the current prompt-construction that inlines research findings with a path-only handoff. The spawned planning agent receives:
  > ```
  > RESEARCH DOC: <absolute path written by Step 1>
  > Read it yourself as needed. Do NOT expect findings in this prompt.
  > Your deliverable: a plan doc at <absolute path>. Return only the handoff contract fields.
  > ```
- **Step 4 mirror (plan → implementation handoff):** Same pattern — pass `PLAN DOC: <path>` only, no inlined plan content.
- **Step 6 rewrite (final report generation):** Replace "reconstruct from handoff blocks held in context" with "re-read the three doc paths from disk, then summarize." Explicit instruction: `"Do NOT trust in-context memory of stage outputs. Re-read the three files; they are the source of truth."`
- **Inter-stage discard step:** Insert a `<discard_after_extract>` block between every stage transition:
  > *"After parsing the `<STAGE>` handoff, extract ONLY `{doc_path, status, blocker?}`. Explicitly drop all other stage content from working memory before spawning the next stage. Do not quote, do not summarize, do not retain."*
- **Add `<handoff_contract>` XML block** near the top of the command, defining the minimal fields master expects back from every stage:
  > ```
  > <handoff_contract>
  >   <required>
  >     <field name="doc_path">absolute path to the artifact this stage produced</field>
  >     <field name="status">one of: complete | blocked | partial</field>
  >   </required>
  >   <optional>
  >     <field name="blocker" max_words="30">one-sentence blocker description</field>
  >     <field name="next_action_hint" max_words="20">optional pointer for next stage</field>
  >   </optional>
  >   <forbidden>findings, plan_summary, code_snippets, file_contents, stage_narrative</forbidden>
  > </handoff_contract>
  > ```

**Success check:**
- Character count of the Step 2 spawn prompt (excluding the path itself) is ≥80% smaller than baseline (record baseline before editing).
- `grep -n "Re-read\|re-read" /home/jakedevar/rsi/.claude/commands/master_implement.md` hits in Step 6.
- `<handoff_contract>` appears exactly once in the file.
- `<discard_after_extract>` appears between each of the three stage transitions.

**Risk:** Medium — this is the behaviorally significant PR. A stage agent that silently depended on inlined findings will now have to read the doc itself. Mitigation: the handoff contract forces every stage to produce a doc path, so the downstream stage always has something concrete to read.

---

### Phase 3 — Structured return schemas for team commands
**Files touched:** `.claude/commands/team_research_codebase.md`, `.claude/commands/team_create_plan.md`, `.claude/commands/team_implement.md`

**Changes:**
- Add a typed return schema block to each team command's worker prompt template. Schemas:

  **team_research_codebase.md worker returns:**
  ```
  findings: list[str, max_words=20, max_items=5]
  file_refs: list[file_path_with_line, max_items=10]
  open_questions: list[str, max_words=15, max_items=3]
  ```

  **team_create_plan.md worker returns:**
  ```
  proposed_changes: list[str, max_words=20, max_items=8]
  file_refs: list[file_path_with_line, max_items=10]
  risks: list[str, max_words=15, max_items=3]
  ```

  **team_implement.md worker returns:**
  ```
  files_modified: list[file_path, max_items=10]   # the ONLY substantive field
  status: enum[complete, blocked, partial]
  blocker: str, max_words=15, optional
  ```

- Add an explicit `<forbidden_content>` block to each worker prompt, banning:
  - Code snippets of any length (master reads `git diff` for details).
  - Full-file contents or large excerpts.
  - Restating the prompt or worker role.
  - Prose narrative / "what I did" summaries beyond the schema fields.
- `team_implement.md`: add the rule verbatim — *"`files_modified` is the ONLY substantive field you return. The master will read `git diff` for details. Everything else is a status flag."*
- `team_create_plan.md`: cap `proposed_changes` at 8 bullets × ≤20 words. Ban code in worker contributions. Note clearly: *the plan document (written by master) still contains code — workers contribute only file:line refs and bullet summaries, the master composes the code sections from those refs.*

**Success check:**
- Each team command file contains a `<return_schema>` block with `max_words` / `max_items` annotations on every field.
- Each team command file contains a `<forbidden_content>` block.
- `grep -n "files_modified is the ONLY" /home/jakedevar/rsi/.claude/commands/team_implement.md` returns a match.

**Risk:** Low-medium. Workers may initially try to exceed the schema; the master prompt should explicitly reject over-budget returns and request a rewrite (covered by the shared preamble in Phase 4).

---

### Phase 4 — Shared preamble extraction (the RSI foundation)
**Files touched:** `.claude/commands/_shared/worker_preamble.md` (expand the stub from Phase 1), all 7 commands.

**Changes:**
- Flesh out `/home/jakedevar/rsi/.claude/commands/_shared/worker_preamble.md` as the single source of truth. Sections:
  1. **Role framing** — "You are a worker agent in an RPI team. Your master is waiting on a bounded return."
  2. **Read budget** — the ≤8k-tokens-of-file-reads rule from Phase 1.
  3. **Return budget** — per-role token caps (research 250, planning 400, implementation 300), selectable by the including command.
  4. **Forbidden content list** — code snippets, full-file contents, verbose prose, prompt restatement, hedging language.
  5. **Rubric output contract** — "file:line refs are free; prose is taxed." Every prose field has a `max_words` cap; every list has a `max_items` cap.
  6. **Failure modes** — if you cannot complete within budget, return `status: partial` with a `blocker` field; do not silently truncate.
- Refactor all 7 commands to replace their ad-hoc preamble/forbidden/budget text with an include directive:
  > `<!-- include: .claude/commands/_shared/worker_preamble.md (role=research|planning|implementation) -->`
  Since Claude Code slash commands don't support true includes, each command either (a) inlines the shared content via a clearly marked "BEGIN SHARED PREAMBLE / END SHARED PREAMBLE" fence kept in sync by hand, or (b) references the path with an explicit "read this file before proceeding" instruction. Decision: **(b) — reference by path**, because that is the surface the RSI meta-harness will eventually mutate. Mutating one file beats mutating seven copies.
- Remove the now-duplicated preamble fragments from the 7 commands, replacing them with the path reference.
- Add a comment at the top of the shared preamble: `<!-- This file is the RSI meta-harness mutation target. All RPI workers load their contract from here. -->`

**Success check:**
- `/home/jakedevar/rsi/.claude/commands/_shared/worker_preamble.md` contains all six sections above.
- Each of the 7 commands contains exactly one reference to the shared preamble path.
- `grep -c "forbidden" /home/jakedevar/rsi/.claude/commands/*.md` shows preamble content de-duplicated (forbidden-content lists no longer inlined in individual commands).
- Mutating a rule in the shared preamble (e.g., tightening the word cap) changes worker behavior for all 7 commands without further edits.

**Risk:** Low behaviorally (all content already exists post-Phase-3, just centralized); medium operationally — future edits must go through the shared file, and humans need to notice the reference. Mitigation: leave a one-line note at the top of each command pointing at the shared file.

---

## Ambiguity Resolutions
- **Shared preamble location** → `/home/jakedevar/rsi/.claude/commands/_shared/worker_preamble.md`. Underscore-prefixed directory signals "not a slash command", keeps the command list clean.
- **Global `~/.claude/commands/` variants** (`research_codebase_nt.md`, `create_plan_nt.md`, etc.) → Out of scope for this plan. User-global variants follow in a separate pass once the project-local pattern is proven.
- **Per-role token budgets** → Research workers 250, planning workers 400 (need more structural fields), implementation workers 300 (need `files_modified` list). Encoded as selectable roles in the shared preamble.
- **Handoff block format (JSON/YAML vs prose+XML)** → Keep prose with XML tags. Rationales: matches existing RPI style, stays grep-able from the shell, LLMs parse XML-tagged prose reliably, and JSON/YAML would require a harness-side parser that doesn't exist yet. Revisit when the RSI meta-harness needs structured extraction.
- **True includes vs path reference** → Path reference + "read this file before proceeding" instruction. Claude Code slash commands don't support server-side includes, and maintaining manually-synced inline fences across 7 files defeats the purpose. The meta-harness mutates one file.
- **Should the `<handoff_contract>` live in the shared preamble or `master_implement.md`?** → In `master_implement.md`. The contract is specifically the master↔stage interface, not the master↔worker interface. Workers use the shared preamble; stages use the handoff contract.
- **What happens if a worker over-runs its return budget?** → Master rejects the return and re-spawns the worker with a reminder. Codified in the shared preamble's "failure modes" section.
- **Do we version the shared preamble?** → Yes — add a `version: 1` frontmatter line. RSI meta-harness bumps the version when it mutates; downstream telemetry can correlate performance changes to preamble versions.
- **PR ordering vs dependency** → Phases 1–3 are independent in principle, but Phase 4 strictly requires Phases 1–3 (it de-duplicates content that doesn't exist until those land). Ship in order.
- **Should `create_plan.md` workers be allowed to emit code?** → No. Plan doc itself contains code (written by the master), but workers contribute only file:line refs + bullet summaries. Rationale: worker-emitted code is unverified, unreviewed, and duplicates what the master will write anyway with full context.

## Verification Plan
1. **Static checks (per phase):** Run the grep-based success checks listed under each phase.
2. **Baseline capture (before Phase 2):** Record the character count of `master_implement.md` Step 2 spawn prompt, and the peak context size observed when running the existing `master_implement` on a small ticket (pick a recent trivial fix from git log).
3. **Post-refactor end-to-end:** Run `master_implement` on the same small ticket after all 4 phases land. Measure:
   - Master's peak context token count at each stage boundary (Step 2 spawn, Step 4 spawn, Step 6 final). Target: ≥3× reduction vs baseline.
   - Worker return sizes (should be bounded by schema caps).
   - End-to-end success (the ticket is actually implemented with no regression vs the pre-refactor run).
4. **Schema compliance:** Spot-check a few worker returns for over-budget fields. If any field routinely exceeds its cap, tighten the prompt (not the cap) — the cap is the contract.
5. **Shared preamble mutation test:** Change one rule in `worker_preamble.md` (e.g., drop research return budget from 250 → 150). Verify the behavior change propagates to all 7 commands without further edits. This is the RSI foundation test.

## Rollback
Each phase ships as a separate PR. Rollback is per-PR via `git revert`.
- **Phase 1:** Safe to revert in isolation; additive only.
- **Phase 2:** The behaviorally significant one. Stage the commit so it can be isolated. If a downstream stage starts failing because it silently depended on inlined findings, revert Phase 2, fix the stage prompt to read its input doc explicitly, then re-land.
- **Phase 3:** Revertable in isolation. If workers start returning `status: blocked` at high rates due to over-tight caps, bump the caps in the schema (do not revert the schema mechanism itself).
- **Phase 4:** Revertable in isolation, but reverting re-introduces 7-way duplication. Prefer to forward-fix (edit the shared preamble) over reverting.
