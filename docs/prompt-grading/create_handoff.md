# Grade: create_handoff.md

**Final grade: D+**  |  **Score: 33/70**

## Rubric
| Dimension | Score | Comment |
|---|---|---|
| Role & Task Clarity | 6/10 | L7 is crisp: "expert documentarian tasked with writing a handoff document... thorough, but also **concise**." Then the file spends the next 130 lines dismantling that second adjective. "More information, not less" (L127) openly inverts "concise" (L7). A reader cannot tell whether terse-and-structured or exhaustive-prose is the target. Same autonomy/interactivity split that dragged create_plan.md to C-. |
| Structural Scaffolding | 3/10 | Pure markdown. Zero XML tags — no `<role>`, no `<handoff_contract>`, no `<return_schema>`, no `<forbidden_content>`, no `<destructive_actions>`. The handoff template itself (L46-89) is a flat markdown skeleton with ten free-text prose sections and no structural anchors. Post-refactor siblings (`create_plan.md`, `master_implement.md`) wrap their contracts in XML; this file hasn't caught up. Principle #2 completely absent. |
| Output Contract | 2/10 | The handoff doc is a channel to the NEXT agent (`/resume_handoff`), which means every section here is a future context tax. Zero caps anywhere: no word budget on `Task(s)`, no max_items on `Recent changes`, no max_items on `Learnings`, no max_items on `Artifacts`, no cap on `Action Items & Next Steps`, no cap on `Other Notes`. Ten uncapped free-text sections × one-to-many bullets each = a worst-case handoff that out-bloats the conversation it's trying to compact. L127 "**more information, not less**" is the anti-contract — it instructs the agent to pad. |
| Context Discipline | 4/10 | The one bright spot: no "read files FULLY / never use limit/offset" mandate in this file. That's only because the command doesn't read files in bulk — it writes them. But the document this command produces has no budget, and downstream (`/resume_handoff`) must ingest it. The bloat is deferred one hop, not avoided. The "Original Request" section (L64) says "quoted VERBATIM if possible, or summarized in 1-2 sentences if it's too long" — good instinct, but "if too long" has no threshold. |
| Stop Conditions & Escape Hatches | 2/10 | None. Zero attempt caps. Zero "if git status shows uncommitted unrelated work, stop." Zero "if no handoff directory exists, stop and report." Zero handling for "I don't have an original question to restate." Zero failure path for the auto-push at L118-124. The only "failure" is L23 "the handoff is considered FAILED" if the bookending-original-question ritual isn't performed — which is enforcement, not recovery. |
| Negative Space | 2/10 | L129 ("avoid excessive code snippets") is the only real forbidden-content item, and it's qualified into uselessness ("one unless it's necessary"). No list of forbidden content in the document itself. No ban on chain-of-thought, hedging, prompt restatement, or narrative prose — all the things the shared worker preamble forbids. Nothing says the handoff doc is machine-readable for the next agent and therefore must be terse. |
| Composition & DRY | 3/10 | The "Original Request" / "Original Research Question" ceremony is triplicated: stated at L10-26, re-stated at L98-116, with the "REMINDER: Your response must ALSO begin with" at L113 — three enforcement passes on one rule. The Commit-and-Push block at L118-124 is verbatim duplicate of the same block in `research_codebase.md`, `create_plan.md`, `implement_plan.md`, and `team_*.md`. No reference to `_shared/worker_preamble.md` at all — this command predates the Phase-4 extraction and was never updated. |

## Margin comments (teacher annotations)

- **L7** — *"a handoff document that is thorough, but also **concise**"* — two adjectives pulling opposite directions, and the file never picks a side. Compare to the post-refactor `create_plan.md` L10 which now references the shared preamble's read budget and return budget as the arbiter. Adopt the same pattern: delete "thorough, but also concise" and point at `_shared/worker_preamble.md §Return budget` with a `role=planning` (or new `role=handoff`) selector. Violates #1 and #3.

- **L10-26** — *"CRITICAL — Original Question Passthrough (NON-NEGOTIABLE)"* block — the ALL-CAPS nuclear-strength framing ("single most important rule of this command", "takes priority over everything else", "considered FAILED") is doing the work that an XML `<terminal_contract>` tag should do. Replace with:
  ```
  <terminal_contract>
    Response MUST begin with the "Original request:" line and end with the "---\n**Original Research Question:**" block. Extract from: (a) user input, (b) prior handoff, (c) plan/research doc. Missing either side = malformed response; retry before sending.
  </terminal_contract>
  ```
  Saves ~15 lines and removes the duplicate enforcement pass at L98-116. Violates #2 and #14.

- **L20-22** — *"Original Research Question"* block at the BOTTOM with different casing than `Original request:` at the TOP — *inconsistent field names for the same semantic value. The top uses sentence case "Original request:"; the bottom uses title case "**Original Research Question:**". Downstream regex-based parsing (resume_handoff) has to match two forms. Pick one. Principle #11 (format anchoring) failure — the anchor itself drifts.*

- **L31-40** — *filepath convention block* — concrete and copy-pasteable, which is good, but buried under "## Process" and "### 1. Filepath & Metadata" headings without any XML anchor a downstream tool can find. Principle #2. Also L37 "Run the `scripts/spec_metadata.sh` script" — no fallback if the script doesn't exist. Script-missing is a common failure in fresh worktrees. Add: "If the script is absent, fall back to `date -u +%Y-%m-%d_%H-%M-%S` and `git rev-parse HEAD` inline." Violates #9.

- **L42-43** — *"### 2. Handoff writing. / using the above conventions, write your document."* — the step header has a trailing period and lowercase first word ("using"). Cosmetic, but the preceding 130 lines have been ALL-CAPS SHOUTING at the agent about NON-NEGOTIABLE rules and now the core instruction whispers. Capitalize and promote.

- **L46-89** — **the handoff template itself** — this is the central artifact the command produces, and it is the single largest context-bloat vector in the pipeline. Ten free-text prose sections, zero caps:
  - `## Task(s)` — no word/item cap. "description of the task(s) that you were working on" with no ceiling.
  - `## Critical References` — "only 2-3 most important file paths" (soft, no hard cap).
  - `## Recent changes` — no cap, no format beyond "line:file syntax".
  - `## Learnings` — "describe important things that you learned" — unbounded prose invitation.
  - `## Artifacts` — "an exhaustive list" — explicitly anti-budget.
  - `## Action Items & Next Steps` — no cap.
  - `## Other Notes` — uncapped catchall ("other notes, references, or useful information").

  Principle #3 (output contract) and #4 (structured return schema) comprehensively failed. Every one of these is a handoff to the next agent (`/resume_handoff`) — the next agent will read this doc FULLY on load. Contract bloat here compounds through the pipeline. Fix: wrap the whole template in `<handoff_document>` with `max_words` / `max_items` on every field:
  ```
  <handoff_document>
    <immediate_next_action max_words="30" required="true" load_bearing="true"/>
    <original_request max_words="60" required="true"/>
    <task_status max_items="5" max_words_per="25"/>
    <critical_references max_items="3" file_line_refs_only="true"/>
    <recent_changes max_items="8" format="file:line — ≤15 words"/>
    <learnings max_items="5" max_words_per="30"/>
    <artifacts max_items="6" file_line_refs_only="true"/>
    <next_steps max_items="5" max_words_per="20"/>
    <other_notes max_items="3" max_words_per="25" optional="true"/>
  </handoff_document>
  ```
  That is what `/resume_handoff` needs. That is all it needs.

- **L66-67** — *"## Immediate Next Action / {ONE clear, imperative sentence..."* — this is the single most load-bearing field in the entire document, buried as the second section below Original Request. The next agent's first decision point is "what do I do right now?" and the field answering that is one line of prose in a 45-line template. Promote it to the top, wrap in its own XML, and mark it as the load-bearing contract field. The current ordering prioritizes ritual (L64 original request) over action (L66 next action); both must be present but the next-action is what moves the work forward. Violates #3 (the load-bearing field is not anchored).

- **L69-70** — *"description of the task(s) that you were working on, along with the status of each (completed, work in progress, planned/discussed)"* — invitation to narrative prose. Compare to the shared preamble's forbidden list: no "I explored…", no "I noticed…". This field will be filled with exactly that. Fix: typed enum per item — `{task, status: enum[done|wip|planned], plan_phase?}` and cap at 5 items. Violates #4 and #7.

- **L76-77** — *"describe recent changes made to the codebase that you made in line:file syntax"* — "line:file syntax" is backwards (the canonical form is `file:line`, as shown in `research_codebase.md` and `create_plan.md`). Fix the direction — but more importantly: "recent changes" is already visible via `git log` + `git diff`. The handoff should reference commit SHAs or a branch name, not re-narrate the diff in prose. Principle #5 (disk-is-the-channel): the git log IS the channel. Tell the next agent to run `git log <branch>..HEAD --oneline` for detail and keep this field to a ≤3-bullet summary with SHAs.

- **L78-79** — *"## Learnings / describe important things that you learned"* — this is the highest-ambiguity section in the template. "Important" is a vibe. "Patterns", "root causes", "other important pieces of information" — three overlapping categories with no cap, no priority order, no forbidden content. Workers with ADHD-style overshare bias will fill 30 bullets here. Hard cap: ≤5 bullets × ≤30 words, and forbid restatement of things already in the plan/research doc. Violates #3 and #7.

- **L81-82** — *"## Artifacts / an exhaustive list of artifacts you produced..."* — **"exhaustive"** is a direct anti-contract adjective. The canonical fix: cap at 6 file paths with line ranges, no prose descriptions, just paths. The next agent opens what it needs; it doesn't need the producer's curated summary of each file. Principle #5, #3.

- **L84-85** — *"## Action Items & Next Steps / a list of action items and next steps for the next agent to accomplish"* — duplicates `## Immediate Next Action` (L66) semantically. What's the difference between "next action" and "next steps"? None made clear. Two fields covering one concept = the next agent gets contradictory priorities. Merge into one field or strictly define: `Immediate Next Action` = the single first action (imperative, one sentence); `Action Items` = the ordered backlog after it (≤5 bullets). Principle #1 and #14.

- **L87-88** — *"## Other Notes / other notes, references, or useful information..."* — the catchall escape hatch with no cap. Compare to `team_research_codebase.md` L136's "EXTRA FIELDS" block that `docs/prompt-grading/team_research_codebase.md` flagged as *"clever in theory, but no enforcement on the master side."* Same anti-pattern, same fix: either enumerate allowed extra fields or delete the hatch. Violates #7.

- **L88** — *"things you leanrned"* — typo ("leanrned"). Every handoff produced from this template propagates the typo through `$EDITOR` / `git log` metadata of nobody-fixes-it churn. Fix while you're in there.

- **L92-95** — *"### 3. Approve and Sync / Save the document."* — three-word step. The daemon-auto-detects-the-write paragraph is the substantive content and it's in the next block. Merge.

- **L95** — *"the daemon automatically detects this Write and derives the next pipeline step (`/resume_handoff`). No special tags are needed — the file path IS the signal."* — this is the cleanest sentence in the file. Keep it. It correctly encodes disk-is-the-channel (#5). Model the rest of the file on this sentence's discipline.

- **L98-116** — *"## Original Question Restatement — MANDATORY (ENFORCED)"* — duplicate of L10-26 with slightly different wording. "(ENFORCED)" added here but not above. "REMINDER: Your response must ALSO begin with:" at L113 is the third restatement of the top-of-response rule. You are telling the agent the same thing three times with progressively stronger enforcement language, which teaches the agent that rules can be safely ignored until the third pass. Collapse to one `<terminal_contract>` block at the top of the file. Violates #14 hard.

- **L118-124** — **"## Commit and Push — MANDATORY"** — *destructive-action clause buried in prose near the end of the file. Same failure mode `docs/prompt-grading/research_codebase.md` L212-216 and `implement_plan.md` L133-142 called out. `git add` the entire `thoughts/` directory is reckless — it picks up any unrelated draft the user had in flight. `git push` with no pre-flight is worse — push can fail (non-fast-forward, detached HEAD, no remote) and there is no recovery path. "Do NOT ask for permission. Do this automatically before your final response." is an assertion of authority, not a contract. Lift into an explicit XML block and pair every action with its failure path:*
  ```
  <destructive_actions>
    <action name="stage">
      scope: `thoughts/shared/handoffs/ENG-XXXX/` ONLY (narrower than "entire thoughts/")
      forbidden_paths: `.env*`, `*.secrets`, `**/credentials*`, user drafts outside handoffs/
    </action>
    <action name="commit">message_format: "handoff: ENG-XXXX — {description}" max_subject_chars=72</action>
    <action name="push">
      on_failure: "Abort. Emit <blocker> with the push stderr. Do NOT modify the handoff file. Do NOT retry automatically."
    </action>
  </destructive_actions>
  ```
  Violates #2, #7, #9, and latent reliability engineer principles from CLAUDE.md's Seven-Expert lens.

- **L126** — *"##.  Additional Notes & Instructions"* — the heading has a stray period-space-space before the text: `"##.  Additional..."`. That is a parse-break for markdown tooling that expects `## Heading`. Cosmetic but it signals this file hasn't been linted.

- **L127** — *"**more information, not less**. This is a guideline that defines the minimum of what a handoff should be."* — **the single most damaging line in the file.** The shared worker preamble at `_shared/worker_preamble.md` L41 says *"Over-budget returns will be rejected... Under-budget returns are welcome."* This file contradicts the preamble it doesn't reference. Delete. Replace with: "When in doubt, trim. The next agent pays the read cost." Violates #3 (anti-contract) and contradicts the post-refactor canon.

- **L128** — *"**be thorough and precise**. include both top-level objectives, and lower-level details as necessary."* — "thorough and precise" cannot co-exist with "concise" (L7). Pick. The whole top-to-bottom rhetoric of this file argues against terseness while the opening paragraph claims terseness is a goal. Resolve by letting the schema's caps enforce precision and deleting the prose that negotiates against them.

- **L129** — *"**avoid excessive code snippets**. While a brief snippet to describe some key change is important..."* — qualified away. Every loophole like this becomes an LLM excuse. Compare to the shared preamble's blanket ban: *"Code snippets of any length. The master reads `git diff`, file:line refs, and on-disk artifacts for details."* Adopt verbatim.

- **Missing throughout** — No reference to `/home/jakedevar/rsi/.claude/commands/_shared/worker_preamble.md`. Every post-Phase-4 command in the refactor set opens with a "Worker preamble (binding)" reference paragraph (see `create_plan.md` L10, `master_implement.md` L28). This file was not touched in the Phase 4 pass and still contains no anchor to the shared contract. Adding it is one sentence of work and eliminates the duplicate forbidden-content / commit-push / restatement boilerplate by reference.

- **Missing throughout** — No `<handoff_contract>` block defining the minimal fields `resume_handoff` receives. The document IS the handoff, and yet the command that writes it has no contract specifying what fields `resume_handoff` treats as load-bearing. Compare `master_implement.md` L16-26. Same pattern belongs here.

- **Missing throughout** — No `<discard_after_extract>` instruction. When the current agent writes the handoff, it should shed its working context — that's the whole point of handoff. The prompt never says so. Violates #6.

- **Missing throughout** — No `<forbidden_content>` block on the handoff doc itself. Violates #7. Workers will pad every prose field.

- **Missing throughout** — No ToolSearch preflight for any deferred tool the command might reach for (e.g., `TeamCreate` if the handoff spawns a continuation team, or `CronCreate` if scheduling a resume). Violates #15.

- **Missing throughout** — No stop conditions. What if the user hasn't set up `thoughts/shared/handoffs/`? What if `scripts/spec_metadata.sh` doesn't exist? What if the current branch is `main` and pushing would be dangerous? What if there's nothing to hand off (the conversation has no substantive state)? Silent failure on all of these.

## Principles missing (with one-sentence fix)

- **#1 RTCCOF** — Role and Task stated, but Constraints, Output-format, and Forbidden-content are scattered or absent. Refactor into RTCCOF-ordered sections with explicit headings.
- **#2 XML scaffolding** — Zero XML tags. The handoff template, the destructive-action clause, the terminal-contract ritual all belong in parseable `<tag>` blocks so `/resume_handoff` can regex-extract instead of LLM-extract.
- **#3 Output contracts** — No caps on any of the ten template sections. Add `max_words` / `max_items` on every prose field; adopt the preamble's return budget by reference.
- **#4 Structured return schemas** — Convert prose sections into typed fields: `task_status: list[{task, status: enum, phase?}, max_items=5]`, etc.
- **#5 Disk-is-the-channel** — "Recent changes" should be `git log` SHAs + short message; "Artifacts" should be paths only; neither should narrate. Let the next agent read what it needs.
- **#6 Discard-after-extract** — After writing the handoff, the current agent should shed working memory. Nothing says so.
- **#7 Forbidden-content lists** — No explicit ban on code snippets, prose narrative, hedging, prompt restatement, or chain-of-thought inside the handoff doc. The doc becomes an LLM diary instead of a state-transfer artifact.
- **#8 Stop conditions** — No failure paths for missing directories, missing scripts, push failures, or empty-conversation cases. Add explicit halts.
- **#9 Escape hatches** — No `<blocking_question>` mechanism when the agent genuinely cannot reconstruct a load-bearing field (e.g., original request is unknown and no prior handoff exists). Currently the agent must fabricate.
- **#11 Format anchoring** — Field names drift between "Original request:" (L14) and "Original Research Question:" (L20) for the same value. Pick one canonical name and use it everywhere.
- **#14 DRY preambles** — Commit-and-push, original-question-restatement, and the Five-Expert framework (implicit via project CLAUDE.md) are duplicated across this file and every sibling. Reference `_shared/worker_preamble.md` and add a `handoff` role variant there.
- **#15 Tool-use preflight** — No ToolSearch instruction for deferred tools the handoff pipeline may invoke.

## Overall verdict

This file is where the pipeline's compacting contract is supposed to land, and instead it's where the pipeline's bloat is laundered into a durable document. Every other command in the refactor set has been told to respect the shared preamble's token budget; this one tells the agent "more information, not less" and ships a ten-section free-text template with no caps on any field. The next agent (`/resume_handoff`) reads this doc to bootstrap a new session — meaning every uncapped prose field here becomes that new session's starting context tax. The command is a bloat-amplifier sitting at a channel boundary, and it predates the Phase-4 refactor entirely. No reference to `_shared/worker_preamble.md`, no `<handoff_contract>`, no `<return_schema>`, no `<forbidden_content>`, no XML at all.

The second structural problem is doctrinal: the file shouts "CRITICAL — NON-NEGOTIABLE — MANDATORY — ENFORCED — FAILED" five different times at the agent about a single rule (original-question passthrough). ALL-CAPS enforcement at three separate points in the file (L10, L98, L113) teaches the agent that rules restated with increasing desperation can be safely ignored until the final pass. The post-refactor voice in `master_implement.md` uses a calm `<handoff_contract>` XML block that says the same thing once and stops. Adopt that voice. Your enforcement mechanism is the contract, not the capitalization.

Third, the Commit-and-Push clause at L118-124 is a destructive side effect buried in prose. `git add` the entire `thoughts/` directory, commit, push — with no forbidden-path list, no push-failure recovery, no branch check, no dry-run. This is the same issue `docs/prompt-grading/research_codebase.md` flagged before that file's refactor, and it was never propagated here. Lift into a `<destructive_actions>` block with scoped paths, a push-failure fallback, and a main-branch guard.

**Highest-ROI fixes, in priority order:** (1) Add the worker-preamble reference paragraph to the top of the file and delete L127's "more information, not less" anti-contract; that single edit imports the forbidden-content list, the return budget, and the failure-mode contract by reference. (2) Wrap the handoff template in `<handoff_document>` XML with `max_words` / `max_items` on every field; this is the load-bearing change because it fixes the compounding bloat into `/resume_handoff`. (3) Collapse the triplicated Original-Question enforcement into one `<terminal_contract>` XML block at the top and delete the other two passes. (4) Lift Commit-and-Push into `<destructive_actions>` with a forbidden-path list and a push-failure abort. After those four, the file goes from D+ to A-. The scaffolding exists in sibling post-refactor commands; this file simply wasn't brought along.
