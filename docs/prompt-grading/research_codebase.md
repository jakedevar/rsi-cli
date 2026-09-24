# Grade: research_codebase.md

**Final grade: C**  |  **Score: 41/70**

## Rubric
| Dimension | Score | Comment |
|---|---|---|
| Role & Task Clarity | 7/10 | Opening sentence (L8) is clean: "conducting comprehensive research... spawning parallel sub-agents and synthesizing". The L24 "CRITICAL: YOUR ONLY JOB IS TO DOCUMENT" reinforces scope. But "five-expert lenses" (L10-22) muddies identity — the agent is told to "observe through five lenses" while simultaneously told "don't evaluate." These two moods coexist uneasily; a reader has to reconcile them. |
| Structural Scaffolding | 4/10 | Pure markdown headings with numbered lists. No `<role>`, `<constraints>`, `<output_format>`, `<forbidden>` tags. The "CRITICAL" ALL-CAPS bullets at L24-31 are doing work that XML tags should do. Principle #2 (XML scaffolding) is effectively absent. |
| Output Contract | 3/10 | There's a YAML frontmatter template (L113-168), which is good format anchoring. But there is no word budget, no token cap, no max-items constraint on "Detailed Findings", no cap on sub-agent count. "Comprehensive" (L8) is the opposite of a budget. |
| Context Discipline | 1/10 | Catastrophic. L46 "Use the Read tool WITHOUT limit/offset parameters to read entire files" is repeated at L217 "Always read mentioned files FULLY (no limit/offset)". Then L200 "Keep the main agent focused on synthesis, not deep file reading" directly contradicts both. The prompt fights itself, and the context-bloating side wins because it's stated twice with CRITICAL tags. |
| Stop Conditions & Escape Hatches | 3/10 | No retry limits, no fail-loud conditions, no handling of "sub-agent returned garbage." L89 says "Wait for ALL sub-agent tasks to complete" — but what if one hangs? No timeout, no escape. No `<blocking_question>` mechanism. |
| Negative Space | 7/10 | This is the prompt's strongest area. L24-31 gives seven explicit DO NOTs, and L203-205 reinforces. The negative space is clear even if verbosely stated. |
| Composition & DRY | 2/10 | L22 points to `five-experts.md` as canonical — good. But then L10-20 re-inlines the five lenses anyway, defeating the reference. L44-48 and L217 state the same "read files fully" rule twice. L206-216 and L232-237 both state "write to thoughts/shared/research/". Duplication throughout. |

## Margin comments (teacher annotations)
- **L8** — "comprehensive research across the codebase" — *"comprehensive" is the enemy of an output contract. Principle #3: replace with a numerical budget, e.g. "≤5 sub-agents, ≤3 file-paths per finding, final report ≤800 words."*
- **L10-22** — the entire five-lens block — *violates principle #14 (DRY preambles). You already cite `five-experts.md` on L22 as canonical. Delete L14-18 and keep only the one-line reference. Every command file that re-inlines this is a copy-paste DRY violation.*
- **L20** — "The experts shape your observation, not your judgment" — *elegant sentence, but contradicted by lens #4 ("Where are the latency risks? Where are test coverage gaps?") which is inherently evaluative. Either the lenses observe or they evaluate — pick one.*
- **L24-31** — seven DO-NOT bullets in ALL CAPS — *principle #7 (forbidden-content list) is achieved, but at the cost of readability. Collapse to one `<forbidden>` tag: "No recommendations, critiques, RCA, or refactoring proposals — describe only."*
- **L35-40** — the canned "I'm ready to research" gate — *this is wasted if the command is invoked with a parameter (compare with create_plan.md's L28-33 which handles the parameter case explicitly). Add a parameter check here too.*
- **L46** — "Use the Read tool WITHOUT limit/offset parameters to read entire files" — *single largest context-bloat vector in the RSI pipeline. Directly violates principle #4. For an RSI monorepo where some generated files exceed 10k LOC, this mandate guarantees the main agent blows its context window before spawning a single sub-agent. Rewrite: "Read mentioned files, but for files >500 lines, use Grep+ranged Read to target relevant sections."*
- **L52** — "Take time to ultrathink" — *vibes-based instruction. Principle #10 (think-before-acting) wants a concrete reasoning block: `<thinking>decompose query into N subquestions</thinking>` before tool calls.*
- **L57-86** — the sub-agent menu — *this is fine as a capability list, but there is no schema for the return value from each sub-agent. Principle #5 (disk-is-the-channel) says each sub-agent should return `{path, status, blocker?}` and not dump prose. Currently the main agent is implicitly receiving and re-reading prose from every sub-agent, multiplying the bloat from L46.*
- **L85** — "Don't write detailed prompts about HOW to search - the agents already know" — *good DRY instinct, but no format contract on the sub-agent call. Specify: "Sub-agent invocations: 1 sentence intent + target scope. Nothing else."*
- **L95** — "Verify all thoughts/ paths are correct (e.g., thoughts/allison/ not thoughts/shared/...)" — *this rule is re-stated more thoroughly at L223-230. DRY violation.*
- **L113-168** — the frontmatter+body template — *principle #11 (format anchoring) partial win: good that a concrete shape is given. But the body has no cap. "Detailed Findings" can be 50 entries or 5 — the agent is not told.*
- **L191** — "never rely solely on existing research documents" — *sensible, but un-budgeted. "Always run fresh codebase research" with no bound becomes "re-explore the whole repo every invocation." Add: "Re-verify at minimum the top 3 file:line references cited by prior research; trust the rest."*
- **L200** — "Keep the main agent focused on synthesis, not deep file reading" — *directly contradicts L46/L217. This is the single most impactful inconsistency in the file. Pick a side. The correct side is this one (L200) — delete the "read FULLY" mandates elsewhere.*
- **L206-211** — "ORIGINAL QUESTION RESTATEMENT — MANDATORY" — *fine mechanism for pipeline continuity, but this and the "COMMIT AND PUSH — MANDATORY" at L212-216 are structural boilerplate reproduced verbatim in create_plan.md. Principle #14: move to a shared include.*
- **L212-216** — auto commit-and-push — *no failure path. What happens if push fails (detached HEAD, no remote, conflict)? "Do NOT ask for permission" is bold when the thing can silently destroy unpushed worktree state. Add: "If push fails, abort with a single-line blocker and do not modify thoughts/."*
- **L217** — re-re-stating "read mentioned files FULLY (no limit/offset)" — *third restatement. Each one is net-negative. Delete.*

## Principles missing (with one-sentence fix)
- **#1 RTCCOF** — partially present, but Constraints and Output-format are buried in prose. Refactor into explicit `<role>`, `<task>`, `<constraints>`, `<output>` sections.
- **#2 XML scaffolding** — the prompt is heading-driven markdown; no parseable tags. Wrap each RTCCOF section in XML so sub-agents can cheaply extract.
- **#3 Output contracts** — no token/word caps. Add `<output_budget>final report ≤800 words, ≤25 file refs</output_budget>`.
- **#5 Disk-is-the-channel** — sub-agents appear to return prose into main agent's context. Require each sub-agent to write to a scratch path and return `{path, status}`.
- **#6 Discard-after-extract** — after synthesis the main agent should drop raw sub-agent prose; nothing says so.
- **#8 Stop conditions** — no retry/timeout limits. Add: "If a sub-agent fails twice, record the gap in 'Open Questions' and proceed."
- **#9 Escape hatches** — the command has "wait for the user" at L40 but no path for truly ambiguous queries mid-research. Add single `<blocking_question>` permitted only when decomposition itself is impossible.
- **#10 Think-before-acting** — "ultrathink" is vibes. Replace with explicit `<thinking>` placement before step 3's sub-agent spawn.
- **#12 Parallel-spawn affordance** — L57 says "multiple Task agents... concurrently" but never says "in a single tool-use message." This matters — without the phrase, models frequently serialize.
- **#13 Anti-sycophancy** — nothing licenses "I don't know" or "the codebase does not appear to contain this." Given the strict no-evaluation stance, the agent will hallucinate descriptions rather than admit absence.
- **#14 DRY preambles** — five-expert block, "read fully" rule, and commit-and-push block all duplicated. Extract.
- **#15 Tool-use preflight** — no mention of ToolSearch for deferred tools.

## Overall verdict

The strongest part of this file is its negative space: the L24-31 DO-NOT block is the clearest thing in it, and it genuinely shapes agent behavior. The weakest part — and it's weak enough to drag the whole grade down a full letter — is the context-discipline failure at L46/L217. Telling an Opus-class agent to read files "FULLY (no limit/offset)" in a Rust monorepo of this size is an antipattern. It turns the main agent into a context sink, which then defeats the whole "spawn parallel sub-agents for efficiency" premise stated on L191. You've built a spoke-and-hub design and then mandated that the hub eat every file itself.

The secondary problem is DRY. The five-expert framework is re-inlined at L10-22, the "read fully" rule is stated three times (L44, L46, L217), and the commit-and-push ceremony is duplicated across this file and create_plan.md. Each duplication is a maintenance surface and a subtle contradiction vector — you already have one self-contradiction (L46 vs L200) because of it.

**Highest-ROI single improvement: delete the "read FULLY (no limit/offset)" mandate everywhere and replace it with "read targeted sections; delegate deep reading to sub-agents who return {path, line_range, 1-line summary} only."** That single change fixes context discipline, reinforces the spoke-and-hub pattern you already claim, and removes the worst self-contradiction in the file. After that, extract the five-expert block and the commit-and-push ceremony into shared includes. Everything else is polish.
