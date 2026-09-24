# Grade: team_implement.md

**Final grade: C**  |  **Score: 43/70**

## Rubric
| Dimension | Score | Comment |
|---|---|---|
| Role & Task Clarity | 8/10 | Master vs. worker responsibilities separated cleanly; tier model is a strong RTCCOF (#1) fit. |
| Structural Scaffolding | 6/10 | Steps 0-8 are well-numbered, but no XML tags (#2); the BLOCKED handoff block and WORKER REPORT block share no visual grammar. |
| Output Contract | 3/10 | WORKER REPORT has fields but no caps (#3, #4). `Failure detail` and `Mismatch` are open-ended prose. `EXTRA FIELDS` is the biggest bloat vector in the file. |
| Context Discipline | 2/10 | L54 "Never use limit/offset — read it completely." L153 "Read all files to be modified fully." L192 "Read every file in YOUR SCOPE completely before touching anything." Three separate "read everything" mandates — #5 and #6 comprehensively violated. |
| Stop Conditions & Escape Hatches | 6/10 | BLOCKED flow exists and hands back to user (good partial #9). But "max 3 attempts" (L196) is only on worker checks; master's "1 attempt only" fix (L255) is ad-hoc; no token/time budget. |
| Negative Space | 6/10 | L212-215 "No reasoning, no code snippets, no test output" is a proper forbidden-content list (#7 — nice). Undercut by L227 EXTRA FIELDS inviting exactly that. |
| Composition & DRY | 4/10 | Seven-expert framework restated yet again (#14). Mismatch Handling (L370) and BLOCKED flow (L261) are near-duplicates. Pipeline Mode Override duplicates the team_create_plan version. |

## Margin comments (teacher annotations)
- **L24** — "IQ 150, ADHD, vim devotee" — *Persona flavor; fine, but it's doing no work in the prompt. Either anchor a constraint to it ("ADHD → no confirmation dialogs in worker reports") or cut it. #13 (anti-sycophancy) suggests trimming the flattery.*
- **L54** — "Never use limit/offset — read it completely." — *Plans can be 500+ lines. Master doesn't need all of it; it needs the phase manifest. Violates #5, #6. Fix: "extract PLAN MANIFEST via targeted reads; never hold the full plan in context."*
- **L153** — "Read all files to be modified fully" — *Same anti-pattern, now in Tier 0. Master-as-implementer should use Grep/Read windows, not `limit=None`. #10 (think-before-acting): form a hypothesis, then read narrowly.*
- **L192** — "Read every file in YOUR SCOPE completely before touching anything." — *Three "read everything" directives in one file is a pattern, not an oversight. Each worker now pays full-file cost × N workers × M phases. At 10 phases this is the whole codebase in aggregate.*
- **L211-215** — "MUST CONTAIN ONLY THE WORKER REPORT BLOCK ... No reasoning, no code snippets, no test output" — *This is the best paragraph in the file. #7 done right. Now honor it by deleting L227.*
- **L219-226** — WORKER REPORT schema — *No word caps. `Failure detail: [paste the last error message]` — error messages can be 2KB of Rust trait resolution output. Cap: "≤500 chars; truncate with `…`". #3.*
- **L211** — "accesses your actual changes via the shared git worktree" — *Good: acknowledges the git log is the source of truth. But the report still duplicates `Files modified` which `git show --stat` already reveals. The report should be `{phase, status, check_results, blocker?}` — nothing else. #5.*
- **L227** — "[EXTRA FIELDS — master appends 1-2 lines here for complex phases only, leave blank otherwise]" + **L232-236** — *Biggest bloat vector in the file. "Design decisions / Risk flags / Type changes" are free-text invitations to narrate. If these are genuinely needed, promote them to first-class fields with explicit caps; otherwise delete. "1-2 lines" is not enforced by anything. #4, #7.*
- **L246-252** — "For each completed worker, parse their WORKER REPORT ... Mark their phase's completed items [x]" — *Master never drops the parsed reports. After 6 workers, master is holding 6 full report payloads in context for the rest of the run. Add: "After marking [x], discard the report text; retain only `{phase, status}`." #6.*
- **L261-277** — BLOCKED flow pastes `Expected/Found/Why this matters` verbatim to the user — *No caps. A chatty worker writes three paragraphs and they all land in the master's user-facing message. Cap each field: "≤25 words". #3.*
- **L299-304** — `cargo test --workspace && cargo clippy --workspace` at final verification — *Good, but output from a failing workspace test is hundreds of lines. Add: "on failure, extract only the first failing test's name and first 10 lines of stderr; do not paste full output into context."*
- **L339-354** — Step 8 commit and push — *`git add [files]` spec is good; Step 8 saying `git add` all changed files contradicts L160 "Do NOT use git add . or git add -A". Which is it?*
- **L370-382** — Mismatch Handling — *Duplicate of the BLOCKED flow at L261 with slightly different wording. #14 violation. One escape-hatch format, reused.*
- **L411-420** — Pipeline Mode Override — *Near-verbatim copy of the same block in team_create_plan. Extract to a shared include (`.claude/commands/_pipeline_mode.md`) or at minimum keep the wording identical so downstream parsers can match one regex. #14.*

## Principles missing (with one-sentence fix)
- **#3 Output contracts (numeric caps)** — Every WORKER REPORT field needs a char/word cap; today all are open-ended.
- **#4 Structured return schemas** — Convert the report to a typed schema `{status: enum, phase: int, checks: [{name, result}], blocker?: {expected≤25w, found≤25w}}` with validator-friendly shape.
- **#5 Disk-is-the-channel** — The git log + worktree diff IS the channel. The report should reference commit SHAs and stop re-listing what `git show` already provides.
- **#6 Discard-after-extract** — After a worker's phase is marked `[x]`, master must shed the report body; only `{phase, status}` survives into later tiers.
- **#8 Stop conditions** — No global budget for master's self-fix attempts or re-dispatches; "1 attempt only" at L255 is the only hard cap. Add a per-tier retry budget.
- **#11 Format anchoring** — No fully-worked example of a completed WORKER REPORT. Workers will hallucinate the shape.
- **#15 Tool-use preflight** — `EnterWorktree` is used at L43 but this environment reveals it as a deferred tool requiring ToolSearch. The prompt should say "resolve `EnterWorktree` via ToolSearch before invoking."

## Overall verdict

The orchestration logic is genuinely good. Tiering (L79-105), the independence predicate at L84-90, model-tier assignment at L106-117, and the Tier 0 "master does foundational work sequentially" rule at L148 all reflect real thinking about when parallelism pays. The BLOCKED→user→three-options handoff at L261 is the right shape for #9. This is competent systems design.

It falls apart on the channel. The prompt repeatedly tells workers and master alike to "read everything completely," then asks workers to return reports with no caps on any text field, then has master accumulate those reports across tiers without discarding. By Tier 2, master is holding six uncapped WORKER REPORTs plus the full plan text plus three full-file reads plus the workspace test output — in a harness whose entire point is context discipline. The EXTRA FIELDS hatch at L227 is the load-bearing mistake: it single-handedly undoes the forbidden-content list at L212.

The fix is mechanical: cap every field in WORKER REPORT (≤500 chars total), delete EXTRA FIELDS, rewrite the three "read fully" directives to "read narrowly with a hypothesis," and add an explicit discard step after each worker is marked `[x]`. Also merge Mismatch Handling into the BLOCKED flow and extract Pipeline Mode Override to a shared file. Do that and you're at A-. As written: the plan runs, the context bleeds.
