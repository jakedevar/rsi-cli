# Grade: create_plan.md

**Final grade: C-**  |  **Score: 38/70**

## Rubric
| Dimension | Score | Comment |
|---|---|---|
| Role & Task Clarity | 6/10 | L9 "You are tasked with creating detailed implementation plans through an interactive, iterative process" directly contradicts L120 "Do NOT pause to ask the user questions" and L312 "Write the full plan in one shot without pausing." Is this interactive or autonomous? The file never decides. A reader genuinely cannot tell what mode they're in. |
| Structural Scaffolding | 4/10 | Heading-driven markdown, numbered steps, fenced templates. No `<role>`, `<constraints>`, `<output_format>` XML tags. The plan template at L183-278 is the best-scaffolded thing in the file, but the prompt controlling the agent is flat prose. |
| Output Contract | 4/10 | The plan template (L183-278) is concrete format anchoring — a real strength. But no word/phase cap. "Determine the right number of phases based on complexity" (L165) is vibes. No per-phase word budget. No cap on "Changes Required" entries. Token budget absent. |
| Context Discipline | 1/10 | L87 "Use the Read tool WITHOUT limit/offset parameters to read entire files", L90 "NEVER read files partially", L105-108 "read ALL files [sub-agents] identified... Read them FULLY into the main context", L318 "Read all context files COMPLETELY before planning." Four restatements of the worst context-bloat antipattern in the repo. This single file reads every ticket, every research doc, every sub-agent-identified file, then writes a plan — the main context is a landfill by Step 4. |
| Stop Conditions & Escape Hatches | 4/10 | L335-340 "No Open Questions in Final Plan: If you encounter open questions during planning, STOP. Research or ask for clarification immediately" — decent stop condition, but then "Do NOT write the plan with unresolved questions" with no path forward. The agent is cornered: can't ask (L120), can't stop (must finish), can't leave questions open (L338). Result: agent fabricates resolutions. |
| Negative Space | 5/10 | "What We're NOT Doing" section is required in output (L202) — good. But prompt-level negative space is thin: no explicit list of forbidden content in the plan itself (e.g., "no pseudocode beyond 10 lines per phase", "no prose narration between phases"). |
| Composition & DRY | 2/10 | Five-expert block re-inlined L12-20 (same as research_codebase.md — DRY fail). Commit-and-push block at L466-472 duplicated from research_codebase.md L212-216. "Original Question Restatement — MANDATORY" at L452-464 duplicated. Two separate "Final Response Format" headings (L450 and L474). "Read files FULLY" rule restated four times. |

## Margin comments (teacher annotations)
- **L9** — "interactive, iterative process" — *contradicts L120, L158, L162, L312. Pick one. The prompt later commits to autonomous — delete "interactive, iterative" here.*
- **L12-20** — five-expert block re-inlined — *principle #14. Same violation as research_codebase.md. Canonical lives at `five-experts.md` (L22); cite, don't re-inline.*
- **L17** — "Write new tests BEFORE deleting old ones" — *good substantive rule, but buried inside an expert-lens bullet. Promote to top-level `<constraints>`.*
- **L35-47** — the canned "I'll help you create" block — *useful fallback, but the Tip line at L45-46 advertises `/create_plan think deeply about ...` — is "think deeply" a real trigger? If yes, it should appear in the logic of Step 1; if no, delete. Orphan instructions mislead agents.*
- **L54-78** — Step 0 Worktree Detection — *structurally the cleanest part of the file. Concrete match rules, explicit paths, clear pass/fail branches. This is how every other step should look.*
- **L87** — "Use the Read tool WITHOUT limit/offset parameters" — *context-bloat antipattern #1. Principle #4 violated.*
- **L90** — "NEVER read files partially" — *context-bloat antipattern #2. Same principle violated in stronger language.*
- **L105-108** — "read ALL files [sub-agents] identified... FULLY into the main context" — *context-bloat antipattern #3. This one is the worst because it scales with sub-agent output: more sub-agents → more files → main context explodes. Directly inverts principle #5 (disk-is-the-channel). Fix: sub-agents return `{path, line_range, summary}` only; main agent re-reads only what it cites.*
- **L120** — "Do NOT pause to ask the user questions — proceed directly" — *principle #9 violated. There is no escape hatch for true ambiguity. Combined with L338 "Do NOT write the plan with unresolved questions," the agent has no legal move when genuinely stuck. Add: "EXCEPT a single `<blocking_question>` when decomposition itself is impossible."*
- **L161-165** — Step 3 "Plan Structure Development" — *this entire step is three bullet points totaling 4 lines. It's a placeholder. Either delete the step and fold into Step 4, or give it real content (criteria for phase count, max phases, dependency ordering heuristic).*
- **L183-278** — plan template — *principle #11 (format anchoring) done well. Concrete, copy-pasteable. Keep.*
- **L241** — "pause here for manual confirmation from the human" — *contradicts the autonomous stance. Fine as a per-phase gate, but says nothing about what to do if the human doesn't respond, or if the agent is running in team_implement (which is non-interactive). Specify: "In autonomous contexts, proceed after automated criteria pass and log the manual-gate status."*
- **L305-340** — "Important Guidelines" — *"Be Skeptical / Be Autonomous / Be Thorough / Be Practical" is vibes. Collapse into `<constraints>` XML with concrete rules. "Question vague requirements" is meaningless without "how."*
- **L313** — "Write the full plan in one shot without pausing for confirmation" — *good, but please reconcile with L9.*
- **L335-340** — "No Open Questions in Final Plan: STOP... Do NOT write the plan with unresolved questions" — *stop condition without an exit. Either allow a `<blocking_question>` or allow a clearly-marked "Open Questions" section in the final plan. The current rule forces hallucination.*
- **L375-395** — "Common Patterns" (DB changes / new features / refactoring) — *these are helpful heuristics but pre-decide ordering the Tech Wizard lens is supposed to decide. Potential contradiction: L15 gives Tech Wizard veto over ordering, but L375-385 hard-codes "schema → store → business logic → API → clients." Either Tech Wizard decides or these patterns do.*
- **L411-414** — "If it mentions 'daemon', specify `hld/` directory... 'UI' when you mean 'WUI'" — *`hld/` and `WUI` are from a different project. This is dead text copy-pasted from another command. Per CLAUDE.md the crates are `rsi`, `rsid`, `rsi-common`. Delete or correct — it actively misleads.*
- **L422-430** — Python pseudocode block for "spawn these tasks concurrently" — *Claude Code doesn't run Python. This is misleading format anchoring. Replace with the real affordance: "Emit multiple Task tool calls in a single assistant message" (principle #12).*
- **L450 and L474** — two "## Final Response Format" headings — *literal duplicate heading. The file has been copy-pasted from two sources and never reconciled.*
- **L452-472** — "Original Question Restatement" + "Commit and Push" — *both verbatim duplicates of the research_codebase.md endings. Principle #14 — extract to shared include.*

## Principles missing (with one-sentence fix)
- **#1 RTCCOF** — Role/Task present but Constraints, Output-format, Context scattered across 400 lines. Refactor into RTCCOF-ordered sections.
- **#2 XML scaffolding** — zero XML tags. Wrap each section.
- **#3 Output contracts** — no plan-length cap, no phase-count cap, no per-phase code-block cap. Add `<output_budget>`.
- **#5 Disk-is-the-channel** — sub-agent results flow as prose into main context (L105-108 explicitly mandates this). Invert to path-based handoff.
- **#6 Discard-after-extract** — nothing licenses the main agent to drop sub-agent prose once the plan is written.
- **#8 Stop conditions** — no retry limits, no "if Step 2 sub-agents conflict, …" clause.
- **#9 Escape hatches** — explicitly forbidden (L120) with no exception. Add single `<blocking_question>` for genuine ambiguity.
- **#12 Parallel-spawn affordance** — L129 says "multiple Task agents to research different aspects concurrently" but never says "in a single assistant message with multiple tool_use blocks." Python-pseudocode at L422 is not a substitute.
- **#13 Anti-sycophancy** — nothing licenses "the codebase does not support this plan as specified." Combined with L338 "do not write the plan with unresolved questions," the agent will paper over real blockers.
- **#14 DRY preambles** — five-expert, commit-push, question-restatement, "read fully" rule all duplicated with research_codebase.md.
- **#15 Tool-use preflight** — no ToolSearch mention.

## Overall verdict

This file is longer than research_codebase.md and scores lower because it layers more contradictions. The core tension is autonomy: L9 says "interactive, iterative, collaborative," L120 and L312 say "autonomous, one-shot, do NOT ask." An agent reading this top-to-bottom will pick up both moods and the result is a plan writer that pretends to be collaborative (asks rhetorical questions in the plan's "Design Decisions" section) while refusing to actually block on the user. That's worst-of-both-worlds behavior.

The second structural problem is the same one research_codebase.md has, amplified: four restatements of "read files FULLY." In a planning command — where you're reading a ticket, 3-5 research docs, and every file a sub-agent mentions — this mandate guarantees the main context is saturated before writing the plan even begins. Compare Step 0 (worktree detection) against Steps 1-3: Step 0 is crisp, bounded, has clear branches. Steps 1-3 are loose, bloated, and self-contradicting. Rewrite Steps 1-3 in Step 0's voice.

Third, there is visible copy-paste debt. Line 413's reference to `hld/` and `WUI` is from another codebase — it does not exist in RSI. The Python `tasks = [...]` block at L422 is not executable by any Claude Code agent. The two "## Final Response Format" headings at L450 and L474 are a literal copy-paste merge artifact. These aren't style nits; they are wrong information the agent will act on.

**Highest-ROI single improvement: resolve the interactive-vs-autonomous contradiction, then rewrite Step 1.1 to forbid full-file reads.** Specifically: delete "interactive, iterative" from L9, delete L87/L90/L105-108/L318's "read FULLY" mandates, and add a single `<blocking_question>` escape hatch so the agent has a legal move when genuinely stuck. That single surgical pass fixes the role ambiguity, the context bloat, and the no-exit stop condition in one edit. After that, DRY the five-expert + commit-push + question-restatement blocks into shared includes, and delete the `hld/`/WUI/Python-pseudocode leftovers. Everything else is polish.
