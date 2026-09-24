# Grade: master_implement.md

**Final grade: D+**  |  **Score: 38/70**

## Rubric
| Dimension | Score | Comment |
|---|---|---|
| Role & Task Clarity | 8/10 | Role is clear: master orchestrator, three stages, human gate. RTCCOF intact. Opening paragraph is crisp. |
| Structural Scaffolding | 7/10 | Step headings are clean and linear, but no XML tags for the handoff contracts; ``` fences are doing structural work they shouldn't. No `<handoff>`/`<forbidden>` anchors. |
| Output Contract | 6/10 | Handoff blocks have shape but no numeric caps (no "≤ 5 findings", no "≤ 120 chars per bullet"). "Final message must contain ONLY the block" is good format-anchoring; undermined by unbounded `[paste all findings]` in Step 2. |
| Context Discipline | 2/10 | Catastrophic. Step 2 re-inlines Stage 1 findings into the Stage 2 prompt. Step 6 re-inlines everything again. Triple-storage of research, double-storage of plan. This is the worst violator of #5 in the entire harness. |
| Stop Conditions & Escape Hatches | 8/10 | Excellent. Every stage has a failure report template; the reference table at L289–297 is exemplary. Human gate at Step 4 is correct. |
| Negative Space | 5/10 | Some good negatives ("DO NOT perform the final git push", "DO NOT wait for human verification", "Do NOT merge into main"). Missing the critical one: "DO NOT paste prior-stage findings into the next prompt — reference the doc path only." |
| Composition & DRY | 4/10 | Same handoff-schema prose is repeated three times inline instead of defined once and referenced. The final-report template re-describes every handoff field already captured on disk. No shared schema block. |

## Margin comments (teacher annotations)
- **L18** — *"Read any provided files FULLY ... never use limit/offset"* — Flat contradiction of #6 (discard-after-extract) and #5 (disk-is-the-channel). The ticket has already been read; the master should extract goal + constraints and drop the rest. "Never use limit/offset" is a superstition, not a principle.
- **L50** — *"[FULL TICKET TEXT OR GOAL DESCRIPTION]"* — Unbounded inlining. If the ticket is 4 KB, you just paid that cost in every downstream prompt. Prefer `Ticket path: <path>` + a one-sentence goal. Violates #5.
- **L58–68** — *"PIPELINE HANDOFF — RESEARCH: ..."* — Handoff schema is decent but uncapped. Add `Key findings: ≤ 5 bullets, ≤ 140 chars each` per #3. Also: no instruction to the research agent to *exclude* prose summaries — you'll get both block + preamble unless you forbid it explicitly (#7).
- **L96–100** — *"RESEARCH ALREADY COMPLETED: ... Key findings: [paste all findings from Stage 1 handoff]"* — **This is the single worst line in the RPI workflow.** Findings now live in: (a) research doc on disk, (b) master's context, (c) planner's context. Triple storage. Every downstream token pays this tax. Principle #5 violation; fix below.
- **L103–105** — *"treating the research above as pre-completed context"* — Wrong channel. The planner should open the research doc from disk, not rely on a prose relay. Replace the whole `RESEARCH ALREADY COMPLETED:` block with a single line: `Research document: <path> — read this first, treat as authoritative.`
- **L115–118** — *"Phases: Phase 1: [title] — [1 sentence description] ..."* — No cap on phase count. A 14-phase plan blows the handoff. Add `≤ 8 phases listed; if more, emit "... (N additional phases in plan doc)"`. Principle #3.
- **L149–151** — *"PLAN DOCUMENT: [plan path] ... GOAL: [goal sentence]"* — Good: only path + one sentence. This is what Step 2 should have looked like. The asymmetry between Stage 2 (bloated) and Stage 3 (clean) proves you already know the right pattern — you just didn't apply it upstream.
- **L162–183** — *"PIPELINE HANDOFF — IMPLEMENTATION"* — Best-shaped handoff in the file. Still uncapped on `Manual verification pending` — a 20-item checklist will balloon. Cap per phase at 5.
- **L205–224** — Step 4 verification prompt is fine, but it's re-rendering the handoff's manual-verification section. That's acceptable (Jake needs to see it), but phrase it as "render the `Manual verification pending` section of the implementation handoff verbatim" rather than re-templating the structure.
- **L246–283** — *Final report template* — Re-inlines findings, phases, design decisions. Master is now pulling from its own context window (where it should have discarded) rather than from the on-disk docs. The final report should be: three doc paths, branch name, verification status. If Jake wants detail, he opens the doc. Violates #5 and #6.
- **L256–259** — *"Findings: • [finding 1] • [finding 2] • [finding 3]"* — You're asking the master to recall findings at end-of-pipeline. By then Stage 1's handoff is 3 agent-turns stale and the master is reconstructing from memory of a summary of a summary. Link the doc. Delete the bullets.
- **L265–267** — *"[Phase 1: title — description] [Phase 2: title — description]"* — Same disease. Phase list already exists in the plan doc. Render `Phases: N — see <plan path>`.
- **L289–297** — Failure table is excellent. Keep as-is. Model for other commands.
- **L299** — *"Never silently swallow a failure"* — Good guardrail, correctly placed. Principle #9 honored.
- **Missing throughout** — No `<forbidden>` block. No "do not paraphrase prior-stage handoffs — pass paths only." No discard-after-extract instruction between stages. No parallel-spawn note for within-stage work (research subagents *can* parallelize, and the prompt doesn't say so — relies on the invoked skill). No tool-use preflight (#15).

## Principles missing (with one-sentence fix)
- **#5 disk-is-the-channel** — Replace every `[paste all findings from Stage N handoff]` with `<doc path>`; instruct downstream agents to read the doc, not receive its contents.
- **#6 discard-after-extract** — Add an explicit between-stages instruction: "After parsing the handoff block, retain only {doc_path, status, blocker}; drop the rest from working memory before spawning the next stage."
- **#3 numeric caps** — Cap findings (≤ 5), phases listed (≤ 8), verification items per phase (≤ 5), and per-bullet length (≤ 140 chars).
- **#7 forbidden-content** — Add a `<forbidden>` block per handoff: "no preamble, no prose summary, no re-pasted file excerpts, no thinking-out-loud."
- **#2 XML scaffolding** — Wrap handoff contracts in `<handoff stage="research">...</handoff>` so downstream parsing is unambiguous and ``` fences stop carrying semantic weight.
- **#12 parallel-spawn** — Note explicitly that within-stage parallelism is the invoked skill's responsibility, and that master spawns stages strictly sequentially — removes ambiguity.
- **#14 DRY preambles** — Define the handoff schema once at the top of the file; each stage references it. Right now the "FINAL MESSAGE MUST CONTAIN ONLY" clause is repeated three times with drift.

## Overall verdict

This file is where your entire workflow's context-bloat problem compounds, and it's worth being blunt about the mechanism. Each stage's subagent correctly writes a doc to disk and returns a tight handoff. Master then does the thing that kills you: it takes that handoff, expands it back into prose inside the *next* stage's prompt (L96–100 is the smoking gun), and at the end of the pipeline expands *all three* handoffs into a final report (L246–283). Research content now exists in four places simultaneously — the research doc, the research handoff, the planner's prompt, and the final report. For a long ticket this is the difference between a 20k-token pipeline and a 120k-token pipeline, and the cost is paid on *every* invocation of master_implement.

This is not a bug. It's a prompt-contract error. You told the subagents to write to disk (correct), you told them to return tight handoffs (correct), and then you told the *orchestrator* to treat those handoffs as the canonical artifact to paraphrase downstream — which inverts the contract. The handoff was meant to be a pointer; you're using it as the payload. Once you see it this way, the fix is mechanical.

The single-file fix with the largest blast radius on the whole harness: (1) in Step 2, replace L96–105 with one line — `Research document: <path> — read from disk; this is authoritative.` (2) In Step 6, replace the findings/phases/decisions bullets with three doc paths and a branch name. (3) Add a discard-after-extract instruction between stages: "Retain only `{doc_path, status, blocker}` from the prior handoff; do not carry findings text into the next prompt." (4) Cap everything numerically per #3. That's maybe 40 lines of diff and it fixes the dominant cost driver across the whole RPI workflow, because every other command that feeds into master_implement inherits this discipline by contract.

The bones of this file are good — the failure table, the human gate, the stage-level stop conditions, the Stage 3 prompt shape. You clearly know the right pattern because you applied it in Step 3 (path + goal sentence, nothing else). Apply Step 3's discipline to Step 2 and Step 6, and this file goes from D+ to A- in one focused edit. You are one contract correction away from the cleanest orchestrator in the harness.
