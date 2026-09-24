# Handoff Command Refactor — Implementation Plan

## Goal
Refactor the two handoff slash commands (`create_handoff.md` and `resume_handoff.md`) to close the compounding-context-bloat loophole the RPI refactor (commit 7c10f8d) left open at the pipeline's terminal boundaries. Bring both files onto the post-refactor shared-preamble contract, wrap the handoff template and the resume extraction flow in parseable XML, put numeric caps on every prose field, lift the destructive commit-and-push clause into an auditable `<destructive_actions>` block, and collapse the triplicated Original-Question-bookend ritual into one `<terminal_contract>`. Ship as a single commit on `main` per Jake's stated preference.

## Context & Background
Commit 7c10f8d ("refactor(rpi): kill context-bloat across RPI command suite") fixed three systemic problems in the seven RPI commands — full-file read mandates, master compounding bloat, and unbounded worker returns — and extracted a shared worker preamble at `.claude/commands/_shared/worker_preamble.md` that the RSI meta-harness will eventually mutate to improve every command at once. The two handoff commands were out of scope for that pass. They are the last files in the RPI surface that:

1. Still contain `FULLY` / "without limit/offset" mandates (three hits in `resume_handoff.md` — L30, L42, L65 — plus four supporting "read everything" restatements in the Guidelines section).
2. Do not reference the shared preamble at all.
3. Define a load-bearing document format (`create_handoff.md`'s ten-section template) with zero caps on any field, so every handoff produced compounds the next session's starting context tax.
4. Carry the "Commit and Push — MANDATORY" destructive block in bare prose, copy-pasted from four siblings that have since been refactored around it.
5. Repeat the Original-Question-bookend rule three times per file, each pass with increasingly desperate ALL-CAPS enforcement — teaching the agent that rules can be safely ignored until the third restatement.

The grading pass in `/home/jakedevar/rsi/docs/prompt-grading/` (`create_handoff.md`, D+ / 33-70; `resume_handoff.md`, D / 31-70) diagnosed each defect. This plan is the fix. After it lands, every RPI command in the project-local command set obeys the shared preamble; the pipeline has no terminal bloat leak; and the document format the next-session agent reads is a typed artifact instead of an LLM diary. This finishes the work 7c10f8d started.

## Success Criteria
- Zero occurrences of `Read files FULLY`, `read the handoff document FULLY`, `WITHOUT limit/offset`, or equivalent full-file mandates in either file. Verified by `grep -rEi "read.*(FULLY|completely)|without limit/offset|never use (limit|offset)" /home/jakedevar/rsi/.claude/commands/create_handoff.md /home/jakedevar/rsi/.claude/commands/resume_handoff.md` returning zero hits.
- Both files contain the canonical Worker-Preamble reference paragraph, pointing at `/home/jakedevar/rsi/.claude/commands/_shared/worker_preamble.md` with `role=handoff` (see "Preamble Reference Paragraph — Exact Text" below for the literal paragraph).
- `create_handoff.md` contains exactly one `<handoff_contract>` XML block declaring the load-bearing fields the resuming agent pulls (`doc_path`, `status`, `immediate_next_action`, `ticket_id?`, `critical_refs[]`, `blocker?`).
- `create_handoff.md` wraps its document template in a `<handoff_document>` XML block with `max_words` / `max_items` on every field (caps listed under Ambiguity Resolutions).
- `create_handoff.md` lifts Commit-and-Push into a `<destructive_actions>` XML block with scoped `stage` paths, a `main-branch guard`, a `push on_failure` clause, and an explicit forbidden-paths list.
- `create_handoff.md` and `resume_handoff.md` each replace their triplicated Original-Question-bookend passages with one `<terminal_contract>` block at the top, and the other two restatements are deleted.
- `resume_handoff.md` contains one `<handoff_intake>` XML block describing the extraction schema (`{handoff_path, status, immediate_next_action, ticket_id, critical_refs[]}`) and a `<discard_after_extract>` instruction telling the agent to drop the handoff body once fields are extracted.
- `resume_handoff.md` contains a `<forbidden_content>` block banning code-snippet restatement, handoff-body echo, narrative prose, and hedging.
- `resume_handoff.md` contains an explicit stop condition: 3 consecutive task failures halts the implementation loop with a blocker.
- `resume_handoff.md` parameter-handling (currently two near-duplicate branches at L28-45) is collapsed to one branch plus a 4-line ticket-resolution helper.
- Label drift between "Original request:" and "Original Research Question:" resolved to a single canonical field name used everywhere (decision below).
- `create_handoff.md` typo "leanrned" (L88) is fixed.
- `create_handoff.md` malformed heading `"##.  Additional Notes & Instructions"` (L126) is fixed.
- Anti-contract line "**more information, not less**" (L127) is deleted.
- Both files fit in one commit on `main` (see Ambiguity Resolutions).

## Out of Scope
- Behavioral changes to the handoff flow itself — the pipeline still writes handoffs to `thoughts/shared/handoffs/ENG-XXXX/`, the daemon still detects writes and derives the next pipeline step, `/resume_handoff` still picks up from the most recent file. We are refactoring the prompts, not the semantics.
- Migrating old handoff documents produced before this refactor. They stay as-is; `/resume_handoff` keeps ingesting them.
- Changes outside the two target files. No edits to `_shared/worker_preamble.md`, no edits to any other RPI command, no edits to the daemon, no edits to `CLAUDE.md`.
- Introduction of a brand-new `handoff` role variant to the shared preamble's `role_variants: [research, planning, implementation]` frontmatter list. We reference the preamble with `role=handoff`, but the preamble's existing rules already apply; extending the preamble's frontmatter is a future pass if the handoff commands ever need distinct read/return budgets.
- Updating global `~/.claude/commands/` variants. Scope is the project-local pair only.
- Any attempt to replace `<docregblock>?</docregblock>` (folklore at `resume_handoff.md` L129) with a harness-documented `<blocking_question>` schema. That schema does not yet exist; converging the handoff commands onto it is a separate cross-cutting change tracked elsewhere.

## Phases

Both files ship in one commit. The phases below are logical units inside that single commit, ordered so that a reviewer reading the diff can see the contract being defined before it is referenced. If Phase 2 grows beyond what a single-commit diff can safely carry (unlikely at the current scope), split per "Commit strategy" in Ambiguity Resolutions — otherwise keep atomic.

### Phase 1 — `create_handoff.md`: contract and template
**File touched:** `/home/jakedevar/rsi/.claude/commands/create_handoff.md`

**Changes (ordered top-down in the file after the edit):**

1. **Replace L7's conflicting "thorough, but also concise" opener** with a role statement that points at the shared preamble:
   > *"You are the closing agent for a pipeline stage, tasked with producing a handoff document for the next session. The handoff is a channel, not a diary — every word in it is read-tax on the resuming agent. The shared worker preamble governs read and return budgets; the `<handoff_document>` schema below governs the document you write."*

2. **Insert the Worker-Preamble reference paragraph** (exact text in "Preamble Reference Paragraph — Exact Text" below) immediately after the new role statement.

3. **Collapse the triplicated Original-Question-bookend ritual** (currently at L10-26, L98-116, and the embedded "REMINDER" at L113) into one `<terminal_contract>` block near the top of the file, directly after the preamble reference:
   ```xml
   <terminal_contract>
     Every response from this command MUST begin with the "Original request:" line
     and end with the "Original request:" bookend (same canonical field name — see
     format anchor below). Extract the field from, in order: (a) the user's current
     input; (b) the prior handoff's Original Request section; (c) the plan or
     research document being closed out.
     Missing either bookend = malformed response; retry before sending.
     Canonical field name: "Original request:" — do NOT rename it to "Original
     Research Question" at the bottom. One label, both positions.
   </terminal_contract>
   ```
   Delete the two other restatements in their entirety (L10-26 and L98-116).

4. **Insert the `<handoff_contract>` XML block** between the terminal contract and the Process section. This declares the fields the resuming agent extracts — it is the upstream half of the handoff-intake pipe:
   ```xml
   <handoff_contract>
     <required>
       <field name="doc_path">absolute path to the handoff document this command writes</field>
       <field name="status">one of: complete | paused | blocked</field>
       <field name="immediate_next_action" max_words="20">imperative, one sentence, load-bearing</field>
     </required>
     <optional>
       <field name="blocker" max_words="30">one-sentence blocker description, required if status=blocked</field>
       <field name="ticket_id">e.g. ENG-2166, or "general" if no ticket</field>
       <field name="critical_refs" max_items="5">list of file:line or absolute paths the resumer MUST open</field>
     </optional>
     <forbidden>body_prose_duplicated_outside_handoff_doc, code_snippets, stage_narrative</forbidden>
   </handoff_contract>
   ```

5. **Rewrite the template (L46-89) as a `<handoff_document>` XML block** with caps on every field. The schema below replaces the current markdown-heading-plus-free-text skeleton:
   ```xml
   <handoff_document>
     <frontmatter>
       date, researcher, git_commit, branch, repository, topic, tags,
       status, last_updated, last_updated_by, type
       <!-- unchanged; keep the existing YAML contract -->
     </frontmatter>

     <title max_words="10">ENG-XXXX {concise description}</title>

     <immediate_next_action max_words="20" required="true" load_bearing="true">
       ONE imperative sentence. Examples:
         "Implement the foo() function in src/bar.rs:42"
         "Run the failing test suite and fix the type error in crates/rsi/src/types.rs:88"
       This is the resuming agent's first decision point. Terse and specific.
     </immediate_next_action>

     <original_request max_words="60" required="true">
       The user's original question/request, quoted verbatim if ≤60 words;
       summarized in ≤60 words otherwise. Do NOT paraphrase stylistically —
       preserve wording where possible.
     </original_request>

     <tasks max_items="5" max_words_per="25" format="status_enum">
       Per item: {task, status: enum[done|wip|planned], plan_phase?}.
       If working from a plan, cite the phase (e.g. "Phase 2 wip").
     </tasks>

     <critical_references max_items="3" file_line_refs_only="true">
       2-3 most important file:line or absolute paths. No prose.
     </critical_references>

     <recent_changes max_items="8" format="file:line — ≤15 words">
       Git log is the channel for detail. This field is a ≤8-bullet summary
       with file:line refs. Include commit SHA range or branch name if useful.
     </recent_changes>

     <learnings max_items="8" max_words_per="150_total">
       Patterns, root causes, gotchas the next agent needs to know.
       Hard cap: 150 words across ≤8 bullets. Do NOT restate content from
       the plan or research doc; reference by file path instead.
     </learnings>

     <artifacts max_items="6" file_line_refs_only="true">
       Paths to docs you produced or updated. Paths only. No descriptions.
     </artifacts>

     <action_items max_items="10" max_words_per="20">
       Ordered backlog for the next agent, excluding `immediate_next_action`
       (which is the first step). ≤10 bullets × ≤20 words each.
     </action_items>

     <other_notes max_items="3" max_words_per="25" max_words="100" optional="true">
       Catchall. Hard cap 100 words total. Omit the field entirely if empty.
     </other_notes>
   </handoff_document>
   ```
   The instructional prose for each field is the XML attribute / inner text. Fields render into the written document under matching markdown headings (`## Immediate Next Action`, `## Original Request`, etc.) so `/resume_handoff`'s current extraction heuristics keep working — the XML is the contract the handoff-writer obeys; the markdown is what lands on disk.

6. **Lift Commit-and-Push (L118-124) into a `<destructive_actions>` block**, placed immediately after step 3 ("Approve and Sync") and replacing the bare prose:
   ```xml
   <destructive_actions>
     <action name="stage">
       scope: `thoughts/shared/handoffs/ENG-XXXX/` ONLY — do NOT `git add` the
         entire `thoughts/` directory; scope to the handoff subdirectory and
         any explicitly-modified code files staged separately.
       forbidden_paths: `.env*`, `*.secrets`, `**/credentials*`, user drafts
         outside `thoughts/shared/handoffs/`
     </action>
     <action name="commit">
       message_format: "handoff: ENG-XXXX — {description}"
       max_subject_chars: 72
     </action>
     <action name="push">
       main_branch_guard: refuse to push if current branch is `main` or
         `master`; emit <blocker> with the branch name and abort.
       on_failure: "Abort. Emit <blocker> with the push stderr. Do NOT modify
         the handoff file. Do NOT retry automatically — surface to the user."
     </action>
   </destructive_actions>
   ```
   Scope: commit-push only. File-writes stay governed by the regular daemon-detects-the-write signal. See Ambiguity Resolutions for why.

7. **Fix the cosmetics:**
   - Fix malformed heading at L126 (`"##.  Additional Notes & Instructions"` → `"## Additional Notes & Instructions"`).
   - Fix typo "leanrned" at L88 (inside the old Other Notes prose; since that prose is being replaced by the `<other_notes>` XML element, the typo vanishes with the rewrite — but verify after edit).
   - Fix backwards "line:file" → "file:line" wherever it appears (the `<recent_changes>` XML uses `file:line` already; confirm no stray "line:file" remains).

8. **Delete anti-contract prose:**
   - L127 ("more information, not less") — DELETE.
   - L128 ("be thorough and precise", "include both top-level objectives, and lower-level details as necessary") — DELETE. The XML caps enforce precision; prose negotiating against them dilutes the contract.
   - L129 ("avoid excessive code snippets") — REPLACE with a reference to the shared preamble's forbidden-content list rather than restating a weakened version.

9. **Remove duplicate "Original Question Restatement — MANDATORY (ENFORCED)" section** (L98-116) entirely — subsumed by the `<terminal_contract>` block at the top.

**Success check (Phase 1 scope):**
- `grep -c "Original Question" /home/jakedevar/rsi/.claude/commands/create_handoff.md` returns 1 (the `<terminal_contract>` block), not 3.
- `grep -c "<handoff_contract>" /home/jakedevar/rsi/.claude/commands/create_handoff.md` returns 1.
- `grep -c "<handoff_document>" /home/jakedevar/rsi/.claude/commands/create_handoff.md` returns 1.
- `grep -c "<destructive_actions>" /home/jakedevar/rsi/.claude/commands/create_handoff.md` returns 1.
- `grep -c "more information, not less" /home/jakedevar/rsi/.claude/commands/create_handoff.md` returns 0.
- `grep -c "leanrned" /home/jakedevar/rsi/.claude/commands/create_handoff.md` returns 0.
- `grep -c "_shared/worker_preamble.md" /home/jakedevar/rsi/.claude/commands/create_handoff.md` returns 1.

**Risk:** Low-medium. Behavioral risk is confined to the written handoff's shape: a terser doc may cause some downstream `/resume_handoff` heuristics to miss a field. Mitigation: the XML-driven markdown headings match the current heading names the resuming agent scans for, and Phase 2 rewrites the resuming side to be contract-driven rather than heuristic-driven anyway.

---

### Phase 2 — `resume_handoff.md`: intake and discipline
**File touched:** `/home/jakedevar/rsi/.claude/commands/resume_handoff.md`

**Changes (ordered top-down in the file after the edit):**

1. **Replace L7's "interactive process" opener** with an autonomous role statement that matches the actual behavior the rest of the file describes (L33, L45, L129, L149 all say "immediately begin work — do NOT ask for confirmation"):
   > *"You are the opening agent for a resumed pipeline stage, tasked with extracting load-bearing fields from a prior handoff, validating that the codebase still matches the handoff's assumptions, and resuming work autonomously from the immediate-next-action field. You do NOT ask for confirmation before proceeding — the shared worker preamble and the `<handoff_intake>` schema below define the contract; obey them and start."*

2. **Insert the Worker-Preamble reference paragraph** (exact text below) immediately after the new role statement.

3. **Collapse the triplicated Original-Question-bookend** (currently at L9-22, L129 interjection, L193-206) into one `<terminal_contract>` block near the top, mirroring Phase 1's Step 3. Use the same canonical label ("Original request:") both positions. Delete the other restatements.

4. **Insert the `<handoff_intake>` XML block** immediately after the `<terminal_contract>`. This is the structural inverse of `create_handoff.md`'s `<handoff_contract>`:
   ```xml
   <handoff_intake>
     Extract ONLY the following fields from the handoff document into your
     working set. After extraction, drop the full handoff body from active
     context; re-read from `handoff_path` only when a specific todo requires
     detail not in the working set.

     <working_set>
       <field name="handoff_path" required="true">absolute path to the handoff doc</field>
       <field name="status" required="true">one of: complete | paused | blocked</field>
       <field name="immediate_next_action" required="true" max_words="20">the first action to take</field>
       <field name="ticket_id">e.g. ENG-2166, or "general"</field>
       <field name="critical_refs" max_items="5">file:line or absolute paths to open on demand</field>
       <field name="blocker">present only if status=blocked</field>
     </working_set>

     <extraction_rules>
       Grep for each field's heading (e.g. `^## Immediate Next Action`) and
       Read only the surrounding ±20 lines. Full-file read of the handoff is
       permitted ONLY if the handoff is <400 lines (per worker preamble
       §Read budget).
     </extraction_rules>
   </handoff_intake>

   <discard_after_extract>
     After populating `<working_set>`, drop the full handoff body from
     active context. Re-read from `handoff_path` on demand for the current
     todo. Do not quote, do not summarize, do not retain the body in
     working memory. Linked plan and research docs under
     `thoughts/shared/plans/` and `thoughts/shared/research/` are read by
     ranged Grep-then-Read when the current todo requires them — NOT
     upfront, NOT in bulk.
   </discard_after_extract>
   ```

5. **Collapse the two parameter-handling branches (L28-45)** into one branch plus a 4-line ticket-resolution helper:
   - Branch: `(a) Resolve the handoff path from the parameter.` If the parameter is a path, use it directly. If it's a ticket ID (matches `ENG-\d+`), list `thoughts/shared/handoffs/<ticket_id>/`; zero files = halt and ask the user; one file = use it; multiple = pick the lexicographically-latest filename (they start with `YYYY-MM-DD_HH-MM-SS`).
   - Common flow: `(b) Execute `<handoff_intake>`; `(c) Present the analysis per §Step 2; (d) Start work on `immediate_next_action` without confirmation.`

6. **Delete every `FULLY` / `WITHOUT limit/offset` mandate:**
   - L30 "Immediately read the handoff document FULLY" → replaced by `<handoff_intake>`'s extraction rules.
   - L31 "Immediately read any research or plan documents that it links to" → replaced by the on-demand rule in `<discard_after_extract>`.
   - L42-43 (duplicate of L30-31, inside the second parameter branch) → deleted with the branch collapse.
   - L65 "Use the Read tool WITHOUT limit/offset parameters" → deleted; shared preamble governs this.
   - L91-93 "Read critical files identified... Read files from 'Learnings' section completely... Read files from 'Recent changes'" → replaced with: *"Read only the `file:line` ranges cited in the handoff's `<critical_references>`, `<recent_changes>`, or `<learnings>` fields. If a ref lacks a line range, Grep for the identifier first, then Read the surrounding ±40 lines."*
   - Guidelines section "Be Thorough in Analysis" subsection (L156-160, four full-read restatements) → replaced with a pointer to the shared preamble's read budget.

7. **Add `<forbidden_content>` block** in the Process Steps area:
   ```xml
   <forbidden_content>
     <banned>Restating the full handoff body in the response</banned>
     <banned>Echoing the `<original_request>` section as standalone prose (it belongs in the bookend, only)</banned>
     <banned>Code-snippet inlining from the handoff's `<recent_changes>` — read `git diff` or the file at the cited line instead</banned>
     <banned>Narrative prose ("I explored…", "I noticed…", "It seems…")</banned>
     <banned>Hedging language as load-bearing filler</banned>
     <banned>Chain-of-thought exposition outside a scoped `<reasoning>` block</banned>
   </forbidden_content>
   ```

8. **Upgrade the sub-task spawn template (L74-86) to a typed return schema.** The current "Return: Summary of artifact contents and key decisions" is unbounded prose. Replace with:
   ```xml
   <subtask_return_schema>
     artifact_path: file path (required)
     status: enum[present|missing|changed] (required)
     drift_summary: prose, max_words=20
     critical_refs_confirmed: list[file:line], max_items=5
   </subtask_return_schema>
   ```
   Note explicitly: Grep before Read for drift detection (the current template names only Read).

9. **Add a stop condition to Step 4 "Begin Implementation":**
   > *"STOP condition: after 3 consecutive failures on the same task (build error, test failure, type error, etc.), halt the loop. Emit a `<blocker>` with the last error output, the attempted fixes, and the failing file:line. Do NOT loop further. Hand control back to the user."*

10. **Add a ToolSearch preflight** near the top of Step 1 for any deferred tools the resuming agent may need (most commonly `EnterWorktree` and `ExitWorktree` if the handoff references a worktree):
    > *"Preflight: `EnterWorktree` and `ExitWorktree` are deferred tools. Before calling either, invoke `ToolSearch` with `select:EnterWorktree,ExitWorktree`. Same pattern for any other deferred tool named in the environment reminder."*

11. **Replace Step 3's unbounded `TodoWrite` emission** with a capped version:
    > *"Use TodoWrite to create ≤8 todos. If the handoff's `<action_items>` carries more than 8, group the tail into a single "create a sub-plan" todo — do NOT emit an unbounded list."*

12. **Lift Commit-and-Push (L186-191) into a `<destructive_actions>` block** identical in shape to Phase 1's Step 6. Use the same main-branch guard, same forbidden-paths list, same push-failure clause. The two files end up with structurally identical destructive-action blocks — this is deliberate; it is the contract `resume_handoff` inherits from `create_handoff` because they share a channel (`thoughts/shared/handoffs/`).

13. **Collapse the Guidelines section (L154-184)** into a `<constraints>` XML block with concrete rules. Delete "Be Thorough / Be Decisive / Leverage Handoff Wisdom / Track Continuity / Validate Before Acting" vibes prose. Keep the operational rules, express them concretely:
    ```xml
    <constraints>
      <rule name="validate_before_acting">
        Before executing `immediate_next_action`: (a) run `git log --oneline
        <handoff_branch>..HEAD` to detect commits since the handoff was
        written; if >20 commits, treat as STALE and re-evaluate strategy
        before the first todo. (b) Verify every path in `critical_refs[]`
        still exists via `ls`; missing path = emit `<blocker>` asking the
        user to confirm or provide a newer handoff.
      </rule>
      <rule name="no_confirmation_gate">
        Do NOT ask the user for permission to proceed. The only legal halt
        is a `<blocker>` emission per the failure modes below.
      </rule>
      <rule name="reference_learnings">
        When a todo touches a file referenced in the handoff's `<learnings>`
        block, re-read the learnings entry (ranged read) before editing.
        Do NOT re-narrate the learning in the response.
      </rule>
    </constraints>
    ```

14. **Delete L210-234 "Common Scenarios"** as decoration. If the operational content is salvageable, fold it into the `<constraints>` block as concrete decision rules. Otherwise remove. The grading doc flagged this section as unactionable.

15. **Update L236-254 "Example Interaction Flow"** to show Grep-then-Read + disk-first artifact handling instead of "[Reads handoff completely]" / "[Reads identified files]". The example currently reinforces the antipattern visually.

16. **Resolve the `<docregblock>?</docregblock>` folklore at L129**: document what it does (daemon detects the literal string and prompts Jake) in one inline sentence. Keep the mechanism — replacing it requires a harness-side change that is out of scope — but surface its semantics so the agent isn't acting on folklore.

**Success check (Phase 2 scope):**
- `grep -c "FULLY" /home/jakedevar/rsi/.claude/commands/resume_handoff.md` returns 0.
- `grep -c "WITHOUT limit" /home/jakedevar/rsi/.claude/commands/resume_handoff.md` returns 0.
- `grep -c "without limit" /home/jakedevar/rsi/.claude/commands/resume_handoff.md` returns 0.
- `grep -c "<handoff_intake>" /home/jakedevar/rsi/.claude/commands/resume_handoff.md` returns 1.
- `grep -c "<discard_after_extract>" /home/jakedevar/rsi/.claude/commands/resume_handoff.md` returns 1.
- `grep -c "<forbidden_content>" /home/jakedevar/rsi/.claude/commands/resume_handoff.md` returns 1.
- `grep -c "<destructive_actions>" /home/jakedevar/rsi/.claude/commands/resume_handoff.md` returns 1.
- `grep -c "Original Question" /home/jakedevar/rsi/.claude/commands/resume_handoff.md` returns 1 (inside `<terminal_contract>`).
- `grep -c "_shared/worker_preamble.md" /home/jakedevar/rsi/.claude/commands/resume_handoff.md` returns 1.
- `grep -c "ToolSearch" /home/jakedevar/rsi/.claude/commands/resume_handoff.md` returns ≥1.

**Risk:** Medium. This is the behaviorally significant half of the refactor. Pre-refactor, the resuming agent reads the entire handoff plus every referenced artifact before acting; post-refactor it extracts a working set and defers body reads. An existing handoff whose load-bearing content sits in `## Other Notes` instead of a proper field will lose that content. Mitigation: (a) the `<handoff_intake>` explicitly lists `critical_refs[]` as a fallback channel for "everything important that doesn't fit another slot"; (b) any handoff produced by the post-refactor `create_handoff.md` will have the fields in the right places; (c) if a pre-refactor handoff is encountered, the agent's first read (handoff is <400 lines in almost all cases) still loads the body — the discipline change is about what is retained, not what is initially read.

---

## Ambiguity Resolutions

- **Role label** → Use `role=handoff` in both files' preamble reference paragraph. Rationale: the shared preamble's `role_variants` list is currently `[research, planning, implementation]`, but the read and return budgets those roles gate are not the right levers for handoff commands — `create_handoff.md` writes a document to disk (its return channel is the file, not the master-facing prose return) and `resume_handoff.md` is a top-level user-invoked command with no master. Using `role=handoff` signals intent to the RSI meta-harness that these commands are a distinct shape and opens the door to a future preamble frontmatter extension without forcing one now. The preamble's READ budget and forbidden-content list apply cleanly; the RETURN budget doesn't (see next point).

- **Token budgets — READ vs RETURN** → READ budget applies to both files as-is (≤8k tokens of file reads before acting; Grep-then-Read for targeted ranges; full-file reads only for files <400 lines). RETURN budget is INAPPLICABLE to these two commands because they are top-level user-invoked commands rather than worker-agents-returning-to-a-master. `create_handoff.md`'s deliverable is a written file on disk; `resume_handoff.md`'s deliverable is ongoing session work. Neither one returns a bounded blob to an orchestrator. Document this inline in the preamble reference paragraph: "READ budget applies; RETURN budget is inapplicable (these commands write to disk / drive a live session rather than return to a master)."

- **Auto-commit-push in `create_handoff.md`** → KEEP the behavior. Jake depends on it for cross-worktree sync of the `thoughts/` directory. MOVE it into an explicit `<destructive_actions>` XML block with parseable fields (scoped paths, forbidden-paths list, main-branch guard, push-failure clause). Rationale: the behavior is correct; its current form is unaudited prose that hides a `git push` with no pre-flight, no failure path, and an over-broad `git add`. Moving to XML makes it reviewable, scopes the `git add` to the handoffs subdirectory only, and gives the push a concrete abort-on-failure contract.

- **Word/item caps on handoff template sections** → Each cap below is justified in one line tied to the receiver-context principle ("every word in the handoff is tax on the next session's starting context"):
  - `immediate_next_action` ≤20 words — it is the resuming agent's first decision; brevity is the contract.
  - `original_request` ≤60 words — preserves verbatim quoting for short requests while capping long ones; matches the existing "verbatim if possible, else 1-2 sentences" instinct with a hard threshold.
  - `tasks` ≤5 items × ≤25 words — more than 5 concurrent task threads is a plan, not a handoff.
  - `critical_references` ≤3 items — the grading audit flagged L73's "2-3 most important" as a soft cap; this hardens it and forbids prose.
  - `recent_changes` ≤8 items × ≤15 words/bullet — git log is the channel for detail; this field is a pointer set.
  - `learnings` ≤8 items, 150 words total — the single highest-bloat section pre-refactor; 150 words total is enough to encode 2-3 gotchas plus pattern references and nothing more.
  - `artifacts` ≤6 items, paths-only — "exhaustive list" (the current prose) is an anti-contract; 6 paths is a manifest.
  - `action_items` ≤10 items × ≤20 words — an ordered backlog cap that matches `TodoWrite`'s bounded shape on the resume side.
  - `other_notes` ≤3 items × ≤25 words, ≤100 words total, OPTIONAL — cap an escape hatch that is never load-bearing; allow omission.

- **Immediate Next Action formalization** → Elevate to a primary contract field. Move it to the TOP of the document template (above `Original Request`), mark it `load_bearing="true"` in the `<handoff_document>` schema, cap at ≤20 words, require imperative voice. Rationale: the grading audit documented that this field is the resuming agent's first decision point and was buried as the second section in the old template. Promoting it aligns the template's order with the resumer's decision order.

- **Disk-is-the-channel for `resume_handoff`** → Extract exactly these five fields into the working set and drop the body: `{handoff_path, status, immediate_next_action, ticket_id, critical_refs[]}`. `blocker` is added conditionally when `status=blocked`. The handoff body is re-read on demand from `handoff_path` using ranged Grep-then-Read when the current todo requires detail not in the working set. Linked plan / research docs are NEVER read upfront; they are read only when a specific todo requires them. This is the single most impactful change in the plan and fully matches the `master_implement` Phase 2 pattern that tore the same antipattern out of the orchestrator.

- **`<handoff_contract>` fields in `create_handoff.md`** → Mirror `master_implement.md`'s contract block shape. Fields: `doc_path` (required), `status` (required; `complete | paused | blocked`), `immediate_next_action` (required, ≤20 words — elevated to required because it is the resumer's anchor), `blocker?` (required when status=blocked), `ticket_id?`, `critical_refs[]` (≤5). Forbidden: `body_prose_duplicated_outside_handoff_doc, code_snippets, stage_narrative`.

- **`<destructive_actions>` scope** → commit-and-push ONLY. File writes (the actual handoff document) remain governed by the daemon's write-detection signal — that is the clean disk-is-the-channel mechanism described at L95 of the pre-refactor file and we are keeping it verbatim. Pulling file writes into `<destructive_actions>` would reintroduce the very dispatch machinery the daemon design removed. Rationale: only irreversible side effects that leave the local host (push) or rewrite git history (commit on the wrong branch) need the auditable block.

- **Triple-state-bookend collapse** → Collapse to a single `<terminal_contract>` block per file, placed at the top after the role statement and preamble reference. Delete the other two restatements entirely. The grading audit was explicit: three restatements with progressively stronger enforcement language teach the agent that rules can be safely ignored until the third pass. The XML tag is the enforcement mechanism, not the capitalization.

- **Canonical field label** → `"Original request:"` (sentence case, trailing colon). Use this identical label at BOTH the top and the bottom of the response, and as the section heading inside the written handoff document. The current split ("Original request:" at top, "Original Research Question:" at bottom) is format-anchor drift per principle #11 — downstream extraction has to match two forms for one semantic value. Picking sentence-case-colon is the least-disruptive change: it already appears at the top of the current file, matches the `## Original Request` section heading in the template, and aligns with the file's own `## Original Request` heading at L63.

- **Commit strategy** → Single commit on `main`, following Jake's stated preference. Both phases are edits to two files; the diff is reviewable in one commit and rolling back is one `git revert`. The phases above are logical units inside that commit, ordered so a reviewer sees the contract blocks (`<terminal_contract>`, `<handoff_contract>`, `<handoff_document>`, `<destructive_actions>`) defined before the process steps that reference them. Escape hatch: if the diff exceeds ~800 lines touched (unlikely given the current file sizes of 130 and 254 lines) or if a reviewer asks to isolate the destructive-action change, split at Phase boundary 1/2 — `create_handoff.md` first, `resume_handoff.md` second. Default: keep it atomic.

- **Handoff format (JSON/YAML vs prose+XML)** → Keep the written handoff as markdown-with-YAML-frontmatter. The `<handoff_document>` XML in the prompt is the author-side schema; the on-disk artifact stays markdown so `/resume_handoff`'s Grep extraction and existing third-party tools (editors, thoughts-locator) keep working. The XML defines what the writer emits; the markdown is what lands on disk. Same compromise the PLAN.md landed on for the existing RPI commands.

- **Spawning shape in `resume_handoff`** → Keep the single-task-spawn example AND explicitly license parallel spawn when multiple artifacts need verification. Add one sentence: "For ≥2 artifacts to verify, spawn the Task agents in a single assistant message (one tool-calls block, multiple invocations) rather than sequentially." This closes the principle #12 gap without requiring a full parallel-spawn rewrite.

- **`<docregblock>?</docregblock>` folklore** → DOCUMENT, don't replace. Inline one sentence: "The daemon listens for the literal string `<docregblock>?</docregblock>` in the agent's output and routes the next user turn as a clarifying reply. This is the current harness mechanism for a blocking question; a future `<blocking_question>` schema will unify it with the rest of the RPI contract." Keeps the behavior working, surfaces the semantics, and leaves the door open for a later cross-cutting replacement.

### Preamble Reference Paragraph — Exact Text

Copy the paragraph below verbatim into BOTH files, placed immediately after the new role statement. The same paragraph works for both files; the role token is `role=handoff` in both cases.

> **Worker preamble (binding):** This command MUST load and obey `/home/jakedevar/rsi/.claude/commands/_shared/worker_preamble.md` with `role=handoff` before acting. That file defines the read budget (Grep-then-Read targeted ranges; full-file reads only for files <400 lines; ≤8k-token read budget before acting), the forbidden-content rules, and the failure-mode contract. The return-budget section of the preamble is INAPPLICABLE to this command — handoff commands write to disk or drive a live session rather than return a bounded blob to a master orchestrator. The rules below COMPOSE ON TOP of the preamble and may tighten (never loosen) any limit declared there.

Both files get this exact paragraph. No per-file variation. If a future change requires distinct handoff-command budgets, extend the preamble's frontmatter `role_variants` list — do not fork the paragraph.

---

## Verification Plan

Run each check below after the commit lands. Expected-zero checks fail the refactor; expected-nonzero checks must produce the listed counts.

1. **`FULLY` / limit-offset mandates purged:**
   ```
   grep -rEi "read.*(FULLY|completely)|without limit|never use (limit|offset)" \
     /home/jakedevar/rsi/.claude/commands/create_handoff.md \
     /home/jakedevar/rsi/.claude/commands/resume_handoff.md
   ```
   Expected: **0 hits.**

2. **Preamble referenced in both files (exactly once each):**
   ```
   grep -c "_shared/worker_preamble.md" /home/jakedevar/rsi/.claude/commands/create_handoff.md
   grep -c "_shared/worker_preamble.md" /home/jakedevar/rsi/.claude/commands/resume_handoff.md
   ```
   Expected: **1 and 1.**

3. **Role token correct:**
   ```
   grep -c "role=handoff" /home/jakedevar/rsi/.claude/commands/create_handoff.md
   grep -c "role=handoff" /home/jakedevar/rsi/.claude/commands/resume_handoff.md
   ```
   Expected: **1 and 1.**

4. **Terminal contract collapsed (not triplicated):**
   ```
   grep -c "<terminal_contract>" /home/jakedevar/rsi/.claude/commands/create_handoff.md
   grep -c "<terminal_contract>" /home/jakedevar/rsi/.claude/commands/resume_handoff.md
   grep -c "Original Question" /home/jakedevar/rsi/.claude/commands/create_handoff.md
   grep -c "Original Question" /home/jakedevar/rsi/.claude/commands/resume_handoff.md
   ```
   Expected: first two return **1 each**; second two return **1 each** (the `<terminal_contract>` block mentions it once; all other restatements deleted).

5. **`create_handoff.md` structural blocks present (exactly once each):**
   ```
   grep -c "<handoff_contract>" /home/jakedevar/rsi/.claude/commands/create_handoff.md
   grep -c "<handoff_document>" /home/jakedevar/rsi/.claude/commands/create_handoff.md
   grep -c "<destructive_actions>" /home/jakedevar/rsi/.claude/commands/create_handoff.md
   ```
   Expected: **1, 1, 1.**

6. **`resume_handoff.md` structural blocks present (exactly once each):**
   ```
   grep -c "<handoff_intake>" /home/jakedevar/rsi/.claude/commands/resume_handoff.md
   grep -c "<discard_after_extract>" /home/jakedevar/rsi/.claude/commands/resume_handoff.md
   grep -c "<forbidden_content>" /home/jakedevar/rsi/.claude/commands/resume_handoff.md
   grep -c "<destructive_actions>" /home/jakedevar/rsi/.claude/commands/resume_handoff.md
   grep -c "<constraints>" /home/jakedevar/rsi/.claude/commands/resume_handoff.md
   ```
   Expected: **1, 1, 1, 1, 1.**

7. **Caps present on every handoff_document field (spot-check):**
   ```
   grep -E "max_words|max_items" /home/jakedevar/rsi/.claude/commands/create_handoff.md | wc -l
   ```
   Expected: **≥15** (the `<handoff_document>` schema has at least 10 capped fields plus `<handoff_contract>`'s optional-field caps and `<destructive_actions>`'s `max_subject_chars`).

8. **Anti-contract prose deleted:**
   ```
   grep -c "more information, not less" /home/jakedevar/rsi/.claude/commands/create_handoff.md
   grep -c "leanrned" /home/jakedevar/rsi/.claude/commands/create_handoff.md
   grep -c "##\. " /home/jakedevar/rsi/.claude/commands/create_handoff.md
   ```
   Expected: **0, 0, 0.**

9. **Stop condition in `resume_handoff.md`:**
   ```
   grep -c "3 consecutive" /home/jakedevar/rsi/.claude/commands/resume_handoff.md
   ```
   Expected: **≥1.**

10. **ToolSearch preflight in `resume_handoff.md`:**
    ```
    grep -c "ToolSearch" /home/jakedevar/rsi/.claude/commands/resume_handoff.md
    ```
    Expected: **≥1.**

11. **File-size delta sanity check:**
    ```
    wc -l /home/jakedevar/rsi/.claude/commands/create_handoff.md
    wc -l /home/jakedevar/rsi/.claude/commands/resume_handoff.md
    ```
    Expected: `create_handoff.md` lands between **110-160 lines** (currently 130; XML caps add structure but template prose shrinks); `resume_handoff.md` lands between **150-200 lines** (currently 254; collapsed branches and deleted Common Scenarios section shrink it substantially).

12. **Smoke test — produce a handoff, resume it:**
    - Run `/create_handoff` on a trivial in-flight ticket.
    - Verify the written file obeys the markdown-heading structure that matches the `<handoff_document>` schema (presence of `## Immediate Next Action`, `## Original Request`, `## Task(s)`, etc., in that order).
    - Verify git commit scope is `thoughts/shared/handoffs/<ticket>/` only (no unrelated `thoughts/` churn).
    - Run `/resume_handoff <path>` on the written file.
    - Verify the agent extracts the five working-set fields, does NOT echo the full body in its first response, and begins work on `immediate_next_action` without asking.

13. **Grading re-audit target:** after the refactor lands, the two files should grade **B+ or higher** on the same 7-dimension rubric used in the pre-refactor audits. The specific lifts: Structural Scaffolding goes from 3→8 (XML tags present), Output Contract from 2→8 (caps on every field), Context Discipline from 0/4→8 (disk-is-the-channel enforced), Composition & DRY from 2/3→8 (preamble referenced, bookend collapsed, branches merged).

## Rollback
Single commit on `main`. Rollback is `git revert <sha>`. The changes are prompt-only — no schema migrations, no daemon changes, no stored-state rewrites — so revert is complete and instantaneous. Existing handoff documents on disk are unaffected (they were produced under the pre-refactor template and remain ingestible by either the pre- or post-refactor `/resume_handoff`, since the markdown heading structure is preserved).

If a regression surfaces:
- **Resume agent fails to extract a field from a pre-refactor handoff:** forward-fix by widening the `<handoff_intake>` extraction rules to fall back to heading-based scan across a broader list of alternate heading names. Do NOT revert; the post-refactor intake is strictly more disciplined.
- **Destructive-action block rejects a legitimate push:** inspect the `main_branch_guard` or `on_failure` path; forward-fix the clause. Do NOT revert to prose — the pre-refactor prose had the same bugs, just silently.
- **Any other regression:** revert the commit, triage, re-land with the fix folded in.
