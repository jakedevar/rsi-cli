# Grade: implement_plan.md

**Final grade: D+**  |  **Score: 32/70**

## Rubric
| Dimension | Score | Comment |
|---|---|---|
| Role & Task Clarity | 6/10 | Role stated ("implementing an approved plan"), but scope creep into verification, commit policy, and "original question restatement" muddies the single-responsibility focus. |
| Structural Scaffolding | 4/10 | Markdown headings only. No XML tags, no parseable sections, no typed fields. A downstream agent cannot regex anything meaningful out of the output. |
| Output Contract | 3/10 | Two ad-hoc code fences for "Issue in Phase" and "Phase Complete," no token/word caps anywhere, no forbidden content, no return schema. |
| Context Discipline | 1/10 | L40 explicitly mandates reading files fully and bans limit/offset. This is a textbook violation of #5 (Disk-is-the-channel) and #6 (Discard-after-extract). Single worst line in the file. |
| Stop Conditions & Escape Hatches | 2/10 | "Fix any issues before proceeding" with no attempt cap, no fail-loud threshold, no `<blocking_question>` schema. Agents will loop on flaky `make check` indefinitely. |
| Negative Space | 4/10 | A few "Do NOT" statements (skip worktree, merge main, ask permission) but no forbidden-content list for the response body, no anti-narration rules. |
| Composition & DRY | 3/10 | Five-Expert Framework re-pasted inline (#14 DRY preambles violated — canonical ref exists at `five-experts.md` but is still duplicated). No shared include for worktree-setup or commit-push boilerplate used across siblings. |

## Margin comments (teacher annotations)
- **L37** — "Create the worktree first (see above)" — *worktree setup depends on `EnterWorktree`, which is a deferred tool. No `ToolSearch "select:EnterWorktree"` preflight step. Violates principle #15 (Tool-use preflight). The agent will call EnterWorktree and get InputValidationError on first run.*
- **L40** — "Read files fully - never use limit/offset parameters, you need complete context" — *flat wrong. This is the single worst context-bloat directive in the harness. Violates #5 and #6. For a 3000-line file you force the agent to pull ~40k tokens when the plan already points at specific ranges. Fix: delete the line; replace with "Read only the ranges cited in the plan; use Grep to locate further call sites."*
- **L42** — "Create a todo list to track your progress" — *nonspecific. No schema, no cap, no reference to TodoWrite tool. Violates #4 (structured return).* 
- **L60-70** — the "Issue in Phase [N]" block — *this is the only escape hatch in the whole file, but it's a freeform template, not a typed `<blocking_question>` schema. Violates #9. Also competes with the later "If You Get Stuck" section — two escape hatches, neither canonical.*
- **L76** — "Run the success criteria checks (usually `make check test` covers everything)" — *no retry budget. Nothing says "after 3 failures, stop and emit blocker." Violates #8 (Stop conditions — fail-loud limits). Primary source of runaway-loop bugs.*
- **L80-92** — "Pause for human verification" block — *competes with L140 "Do NOT ask for permission" push policy. The prompt simultaneously mandates pausing for human verification AND auto-committing-and-pushing before the final response. Ambiguous — which comes first? Agent will pick one arbitrarily.*
- **L104** — "Use sub-tasks sparingly" — *vague. No definition of "sparingly," no affordance for #12 (Parallel-spawn) when sub-tasks actually are warranted. Fix: "Spawn at most N sub-agents; if >N, justify in a single `<reasoning>` block."*
- **L116-131** — "Original Question Restatement — MANDATORY" — *scope creep. This is pipeline plumbing that belongs in a shared include, not duplicated in every terminal slash command. Violates #14.*
- **L133-142** — "Commit and Push — MANDATORY" — *duplicated across implement_plan, team_research_codebase, and likely others. #14 violation. Also "git add" the entire `thoughts/` dir without a forbidden-path list is reckless (secrets, temp files).* 
- **L148** — "create a commit after you finish each phase" — *trailing single-line rule outside any section, contradicting L133's "before responding to the user, commit and push." Is it per-phase push or final push? Unclear.*
- **L9-19** — Five-Expert Framework inline — *this is already canonical in CLAUDE.md and in `five-experts.md`. Repeating it here inflates every worker's system prompt. Violates #14.*
- **L42-43** — "Start implementing if you understand what needs to be done" — *no #10 (Think-before-acting) block. No `<reasoning>` tag. Agent goes straight from read→implement with no checkpoint.*

## Principles missing (with one-sentence fix)
- **#2 XML scaffolding** — wrap sections in `<role>`, `<inputs>`, `<stop_conditions>`, `<output_contract>` so downstream parsing is deterministic.
- **#3 Output contracts** — add "Final response ≤ 300 tokens; commit message ≤ 72 chars subject + body ≤ 200 tokens."
- **#4 Structured return schemas** — typed `{branch, worktree_path, phases_completed:[int], blocker?:string}` return, not prose.
- **#5 Disk-is-the-channel** — delete L40; artifacts (plan, diffs, logs) live on disk, return path references only.
- **#7 Forbidden-content lists** — explicit "do not include: full diffs, full file contents, narration of every edit, chain-of-thought."
- **#8 Stop conditions** — after N=3 failed verification loops, halt and emit `<blocker>`.
- **#9 Escape hatches** — replace freeform "Issue in Phase" with single `<blocking_question>` schema.
- **#10 Think-before-acting** — require a `<reasoning>` block before first Edit.
- **#14 DRY preambles** — move Five-Expert, worktree, commit-push, original-question blocks into a shared include.
- **#15 Tool-use preflight** — `ToolSearch "select:EnterWorktree"` before the first worktree call.

## Overall verdict

This prompt reads like a human SOP, not an agent contract. It tells a diligent junior engineer what to do, but an LLM agent will happily obey the most expensive interpretations — "read files fully," "fix any issues before proceeding," "create a todo list" — and burn the context window before the second phase. The absence of any stop condition on the verification loop is the single most consequential omission: one flaky test turns this into an infinite compile-fix-compile treadmill, and nothing in the prompt prevents it.

The second-order problem is duplication. Five-Expert, worktree setup, commit-push, and original-question-restatement blocks are copy-pasted from sibling commands. Every invocation pays the token tax, and every edit to the canonical policy needs to be replicated N times — exactly the failure mode #14 exists to prevent. The Five-Expert section even links to `five-experts.md` on L21, then ignores its own advice and inlines the content anyway.

Fix path, in priority order: (1) delete L40 today, (2) add a 3-attempt cap with `<blocker>` escape on the verification loop, (3) add `ToolSearch "select:EnterWorktree"` preflight, (4) move Five-Expert / commit-push / original-question blocks to a shared `_preamble.md` include, (5) specify an XML output contract with token caps. Do 1-3 and this prompt goes from D+ to B- overnight.
