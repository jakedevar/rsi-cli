# Grade: team_create_plan.md

**Final grade: C+**  |  **Score: 46/70**

## Rubric
| Dimension | Score | Comment |
|---|---|---|
| Role & Task Clarity | 8/10 | Master/worker roles stated up front; use-when/don't-use fork is crisp. RTCCOF (#1) mostly satisfied. |
| Structural Scaffolding | 7/10 | Numbered steps + fenced templates, but no XML tags (#2). The "PLAN CONTRIBUTION" block is ASCII-fenced, which works — but boundaries between "instructions to worker" and "schema" are blurry. |
| Output Contract | 4/10 | Only soft caps ("1-2 sentences", "5 lines max"). No token cap, no item cap on the `Proposed changes` bullet list — violates #3 and #4. |
| Context Discipline | 3/10 | L51 "Read ALL mentioned files FULLY" and L140 "Read all relevant files in YOUR SCOPE fully — never use limit/offset" are direct anti-patterns. Workers return code snippets into master (#5, #6 violated). |
| Stop Conditions & Escape Hatches | 6/10 | `NEEDS_CLARIFICATION`/`BLOCKED` statuses exist (#9 partial), but no `<blocking_question>` tag and no budget/retry cap on master's "research that gap yourself." |
| Negative Space | 5/10 | "No reasoning narration," "Nothing before it. Nothing after it." is good. But no forbidden-content list for code, logs, test output, chain-of-thought (#7 weak). |
| Composition & DRY | 5/10 | Seven-expert framework is restated here and again in CLAUDE.md and again in team_implement (#14). Pipeline Mode Override is a nice reusable idea. |

## Margin comments (teacher annotations)
- **L23** — "Every planning decision must pass through all seven lenses" — *Section header says "Five-Expert" but body lists seven. Fix the header; sloppy framing erodes trust in the rest of the spec.*
- **L51** — "Read ALL mentioned files FULLY in the main context — Never use limit/offset." — *Direct violation of #5 (disk-is-channel) and #6 (discard-after-extract). Master should read only what it needs; delegate the bulk-read to workers and have workers return file:line handles.*
- **L118** — "Spawn all workers in a **single message**" — *Good. #12 (parallel-spawn) is honored. Keep this.*
- **L142** — "Propose concrete changes for each" + **L153** "code snippets (5 lines max) are allowed" — *This is the headline failure. Five lines × N bullets × M domains = hundreds of lines of Rust in the master's context before the plan is written. Ban code in the worker→master channel entirely (#5, #7). Master reopens the file itself if it needs to see code.*
- **L161-164** — "Proposed changes: - [file.rs:~LINE] — ... - [file.rs:~LINE] — ..." — *No `max_items`. A worker can return 30 bullets. Cap it: `Proposed changes (≤7 bullets, ≤20 words each)`. #4.*
- **L168** — "[EXTRA FIELDS — master appends 1-2 lines here for complex domains, leave blank otherwise]" — *Free-text escape hatch with no word cap. "1-2 lines" is aspirational, not enforced. Either enumerate the allowed extra fields as an enum or delete the hatch. #7.*
- **L187-192** — "Parse each PLAN CONTRIBUTION / Collect all proposed changes across domains" — *"Collect" implies concatenation. Principle #6 says distill, not collect. Rephrase: "extract {file:line, one-line intent} tuples, then DROP the raw reports from context before writing the plan."*
- **L189** — "research that gap yourself (master) before writing the plan" — *No budget. Master can spiral. Add: "≤2 tool calls to resolve; if unresolved, mark phase `deferred` and proceed."*
- **L139-144** — "Read all relevant files in YOUR SCOPE fully" — *The one place "fully" is defensible (worker's scoped domain), but even here workers should be told to return file:line references, not pull whole files into their own context. #10 (think-before-acting) — workers should skim, hypothesize, then read narrowly.*
- **L232-245** — plan template with full ```rust``` blocks — *This is the plan's output format, not the worker→master channel, so it's fine. But the template gives no word/line cap per phase — plans tend to balloon. Add: "Each phase ≤300 lines, ≤5 Changes Required blocks."*
- **L334-338** — "Original Research Question" restatement — *Good continuity mechanism, but it's the eighth mandatory tail section. DRY-violation #14: restatement, pipeline override, final response format, commit step — consider collapsing into one "Terminal Contract" section.*

## Principles missing (with one-sentence fix)
- **#3 Output contracts (hard numeric caps)** — Add "worker reports ≤250 tokens" and "≤7 Proposed changes bullets" to the PLAN CONTRIBUTION template.
- **#4 Structured return schemas (max_items)** — Convert PLAN CONTRIBUTION from prose-ish bullets into a typed JSON-ish schema with `max_words`/`max_items` per field.
- **#5 Disk-is-the-channel** — Forbid code snippets in worker reports; workers write findings to `thoughts/shared/research/planner-<domain>.md` and return only `{path, status, n_changes}`.
- **#6 Discard-after-extract** — Step 4 must say "after parsing, drop raw worker reports from context" — today it implies accumulation.
- **#7 Forbidden-content lists** — Add an explicit "DO NOT include: code blocks, test output, chain-of-thought, restated ticket text, quoted file contents" block to the worker prompt.
- **#11 Format anchoring** — Show one fully-filled example PLAN CONTRIBUTION so workers pattern-match instead of interpreting.

## Overall verdict

Solid bones, undisciplined channel. The decomposition-by-domain idea is right, the tier table (L85-93) is a genuinely good piece of domain modeling, and the parallel-spawn instruction at L118 shows you understand where wall-clock time actually comes from. Step 0 worktree detection is a nice touch. This is a B-level skeleton.

What drags it to C+ is the worker→master channel. You tell workers to be terse ("Nothing before it. Nothing after it.") and then in the same breath invite them to stuff 5-line code snippets into an uncapped bullet list with an EXTRA FIELDS escape hatch. A worker following the letter of your prompt can legitimately return a 2KB payload, and you're about to fan that out across 5-7 workers. That is exactly the context-bloat failure mode the RSI harness is trying to engineer around — and the prompt that *plans* the harness shouldn't be the one modeling the anti-pattern.

Fix: enforce numeric caps (≤250 tokens per report, ≤7 change bullets, ≤15 words each), ban code in reports outright, force workers to dump long findings to `thoughts/` files and return a path, and rewrite Step 4 to say "distill then discard." Do those four things and this jumps to A-. The scaffolding is already there; it just needs to bind.
