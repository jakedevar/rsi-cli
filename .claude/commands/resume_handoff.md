---
description: Pick up a prior session's work from its handoff - validate it, check drift, continue
model: sonnet
capability_class: implementer
---

# Resume from a handoff

You are the opening agent for a resumed pipeline stage, tasked with extracting load-bearing fields from a prior handoff, validating that the codebase still matches the handoff's assumptions, and resuming work autonomously from the immediate-next-action field. You do NOT ask for confirmation before proceeding — the shared worker preamble and the `<handoff_intake>` schema below define the contract; obey them and start.

**Worker preamble (binding):** This command MUST load and obey `/home/jakedevar/rsi/.claude/commands/_shared/worker_preamble.md` with `role=handoff` before acting. That file defines the read budget (Grep-then-Read targeted ranges; full-file reads only for files <400 lines; ≤8k-token read budget before acting), the forbidden-content rules, and the failure-mode contract. The return-budget section of the preamble is INAPPLICABLE to this command — handoff commands write to disk or drive a live session rather than return a bounded blob to a master orchestrator. The rules below COMPOSE ON TOP of the preamble and may tighten (never loosen) any limit declared there.

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

<forbidden_content>
  <banned>Restating the full handoff body in the response</banned>
  <banned>Echoing the original_request section in user-visible responses</banned>
  <banned>Code-snippet inlining from the handoff's recent_changes — read `git diff` or the file at the cited line instead</banned>
  <banned>Narrative prose ("I explored…", "I noticed…", "It seems…")</banned>
  <banned>Hedging language as load-bearing filler</banned>
  <banned>Chain-of-thought exposition outside a scoped `<reasoning>` block</banned>
</forbidden_content>

## Initial Response

When this command is invoked:

1. **Resolve the handoff path from the parameter:**
   - If the parameter is an absolute or relative file path, use it directly.
   - If the parameter matches `ENG-\d+` (a ticket ID): list `thoughts/shared/handoffs/<ticket_id>/`.
     - Zero files or directory absent → halt and tell the user: "I can't find a handoff for that ticket. Please provide the path directly."
     - One file → use it.
     - Multiple files → pick the lexicographically-latest filename (files start with `YYYY-MM-DD_HH-MM-SS`).
   - Execute `<handoff_intake>` to extract the working set.
   - Present the analysis per §Step 2.
   - Start work on `immediate_next_action` without confirmation.

2. **No parameter given:** reply with:
```
Which handoff should I resume?

Tip: You can invoke this command directly with a handoff path:
  /resume_handoff thoughts/shared/handoffs/ENG-XXXX/YYYY-MM-DD_HH-MM-SS_ENG-XXXX_description.md

Or using a ticket number:
  /resume_handoff ENG-XXXX
```
Then stop until the user answers.

**Preflight:** `EnterWorktree` and `ExitWorktree` are deferred tools. Before calling either, invoke `ToolSearch` with `select:EnterWorktree,ExitWorktree`. Same pattern for any other deferred tool named in the environment reminder.

## Process Steps

### Step 0: Validate handoff before extraction (RSI-013)

Before reaching for the grep extraction below, run the structured
validator against the handoff. This is the resume-time tolerance gate
(lenient mode) — it accepts legacy documents pre-April-2026 refactor but
rejects ones whose frontmatter is unparseable or whose four anchor
sections are missing.

1. Resolve `rsi-handoff-validate` from the active repository root. Never search
   or build from an absolute RSI checkout and never assume a `crates/` layout.
   1. **Present/repo-local:** if `target/debug/rsi-handoff-validate` is executable, select it and require validation.
   2. **Buildable/repo-local:** otherwise run `cargo metadata --no-deps --format-version 1`; if successful metadata contains package `rsi-common` with exact binary target `rsi-handoff-validate`, run `cargo build -p rsi-common --bin rsi-handoff-validate --quiet`, select `target/debug/rsi-handoff-validate`, and require validation. A build failure remains a required-validator failure; it is never `SKIPPED`.
   3. **Present/PATH:** if the repository has no exact buildable target but `command -v rsi-handoff-validate` resolves an executable, select it and require validation.
   4. **Unavailable:** only if no repository-local executable exists, successful metadata proves the exact package/target absent (or the active repository has no Cargo manifest), and PATH has no executable, report `Validator: SKIPPED — rsi-handoff-validate unavailable (no executable and no buildable rsi-common target); continuing to Step 1 extraction.` Do not write a `VALIDATOR_REJECTED` handoff; proceed immediately to Step 1 extraction.
   5. **Probe error:** if the active repository has a Cargo manifest but metadata fails and no executable is available, report the capability-probe error as a failure. It is never `SKIPPED`.
2. Run the selected `rsi-handoff-validate <handoff_path>` (default mode is lenient).
3. **Exit 0** → proceed to Step 1 grep extraction.
4. **Exit 2** → write a blocker handoff at
   `thoughts/shared/handoffs/general/<UTC ISO-ts>_VALIDATOR_REJECTED.md`
   with:
   - frontmatter: `status: blocked`, `schema_version: 1`,
     `topic: "Validator rejected: <handoff_path>"`, plus the canonical
     `date`, `researcher`, `last_updated`, `last_updated_by`, `repository`,
     `branch`, `git_commit`, `tags`, `type` keys per `<handoff_document>`
     in `create_handoff.md`.
   - body sections (all required for the blocker itself to validate
     strict): `## Immediate Next Action` = "Open <handoff_path> and
     repair the listed fields"; `## Original Request` = "Resume from
     <handoff_path>"; `## Task(s)` = `- Repair handoff: planned`;
     `## Critical References` = `- <handoff_path>`;
     `## Action Items & Next Steps` = "1. Repair handoff fields. 2.
     Re-invoke /resume_handoff <handoff_path>"; `## Artifacts` = empty
     (header only); `## Recent Changes` = empty; `## Learnings` = empty;
     `## Other Notes` = the JSON `errors[]` dump from the validator
     (kept under the 100-word cap by truncation if necessary).
   - The blocker handoff itself MUST validate strict — this proves the
     validator and skill are mutually consistent. Tested by
     `crates/rsi-common/tests/handoff_skill_blocker.rs`.
   - Halt; the daemon detects the new file and the user sees it.
5. **Exit 1** → standard blocker (I/O error or argument issue). Halt. Validator exit failures are never `SKIPPED`.

### Step 1: Extract Working Set

Execute `<handoff_intake>`:

1. Grep `^## Immediate Next Action` and Read ±20 lines → extract `immediate_next_action`.
2. Grep `^## (status|Status)` or YAML frontmatter → extract `status`.
3. Grep `^## Critical References` and Read ±20 lines → extract `critical_refs[]`.
4. Derive `ticket_id` from the file path (e.g. `ENG-2166` from `thoughts/shared/handoffs/ENG-2166/...`).
5. If `status=blocked`, Grep `^## Blocker` and Read ±20 lines → extract `blocker`.

Read only the `file:line` ranges cited in the handoff's `critical_references`, `recent_changes`, or `learnings` fields. If a ref lacks a line range, Grep for the identifier first, then Read the surrounding ±40 lines. Do NOT read linked plan or research docs upfront.

### Step 1.5: SHA-Divergence Check (RSI-021)

Catches the "stale handoff" failure mode: a handoff committed at SHA X with
subsequent commits at SHA X+N that the handoff's `## Immediate Next Action`
no longer reflects.

1. Grep `^git_commit:` in the handoff frontmatter (single-line read). Extract
   the value as `<handoff_commit>`.
2. If `git_commit:` is missing (legacy handoff pre-RSI-013), skip this step
   and note "handoff predates SHA tagging" in the Step 2 analysis. Proceed
   to Step 2.
3. Otherwise, run:

       git log <handoff_commit>..HEAD --oneline

   - **Empty output** → handoff is fresh against HEAD; proceed to Step 2.
   - **Non-empty output** → the handoff is stale relative to HEAD. Surface
     the divergence in your Step 2 analysis as:

         Handoff SHA: <short>     HEAD: <short>     Divergence: N commits
         Files changed since handoff: [list]

     Do NOT trust `## Immediate Next Action` blindly. Re-read the diff
     (`git diff <handoff_commit>..HEAD`), decide whether the next-action
     is still valid given the new commits, and explicitly call out any
     reconsideration in the Step 2 analysis output before acting.

### Step 2: Validate and Present Analysis

**Validate before acting:**
- Run `git log --oneline <handoff_branch>..HEAD` to detect commits since the handoff was written; if >20 commits, treat as STALE and note in the analysis before proceeding.
- Verify every path in `critical_refs[]` still exists; missing path → emit `<blocker>` asking the user to confirm or provide a newer handoff.
- **Stage-contract check (MWP/ICM):** if the handoff carries a `## Stage contract` block, confirm all four `### ` sub-sections are present in canonical order — Inputs, Process, Outputs, Verify — and that `### Inputs` declares named static inputs or a code-discovery budget (self-check against `scan_contract_block`, `crates/rsi-common/src/handoff_schema/body.rs`). If the handoff declares a `satisfies:`/`covers:` linkage line, note which research `Finding.id`(s) remain open and carry them into the action plan so the next VERIFY stage covers them. A malformed block or an unverified linkage key is drift — flag it under **Issues**, do not silently proceed.

**Present analysis** (format — all fields capped):

```
I've analyzed the handoff from [date] by [researcher]. Current situation:

**Tasks:** (≤5 bullets)
- [Task]: [handoff status] → [verified current state]

**Key Learnings Validated:** (≤5 bullets × ≤25 words each)
- [learning with file:line ref] — [still valid / changed]

**Recent Changes Status:** (≤5 bullets × ≤15 words each)
- [change file:line] — [present / missing / modified]

**Next Actions:** (≤10 items, ordered)
1. [immediate_next_action] ← STARTING NOW
2. [next priority]
...

**Issues:** (≤3 bullets, or omit if none)
- [conflict or regression]
```

Do NOT ask for confirmation before proceeding. The ONLY legal halt is a `<blocker>` emission. If genuinely confused about multiple conflicting paths, respond with `<docregblock>?</docregblock>` (the daemon detects this literal string and routes the next user turn as a clarifying reply; a future `<blocking_question>` schema will unify it with the rest of the RPI contract).

### Step 3: Build the Todo List

1. **Use TodoWrite to create ≤8 todos.** Convert action items from the handoff's action_items into todos. If the handoff carries more than 8 items, group the tail into a single "create a sub-plan" todo — do NOT emit an unbounded list.

2. **Show the plan and immediately begin:**
```
Created task list:
[Show todo list]

Starting with: [immediate_next_action]
```

### Step 4: Execute

1. **Start immediately with `immediate_next_action` — no permission needed.**
2. When a todo touches a file referenced in the handoff's `learnings` block, re-read that entry (ranged Grep-then-Read ±40 lines) before editing. Do NOT re-narrate the learning in the response.
3. Update todos as tasks complete.
4. For ≥2 artifacts to verify, spawn the Task agents in a single assistant message (one tool-calls block, multiple invocations) rather than sequentially.

**Sub-task spawn template:**
```
Task: Verify artifact [path]
1. Check file exists and is non-empty.
2. Grep for the change described in the handoff's recent_changes entry.
3. Report drift vs. handoff expectations.
Use tools: Grep, Read (targeted ranges only)
```

<subtask_return_schema>
  artifact_path: file path (required)
  status: enum[present|missing|changed] (required)
  drift_summary: prose, max_words=20
  critical_refs_confirmed: list[file:line], max_items=5
</subtask_return_schema>

**STOP condition:** After 3 consecutive failures on the same task (build error, test failure, type error, etc.), halt the loop. Emit a `<blocker>` with the last error output, the attempted fixes, and the failing file:line. Do NOT loop further. Hand control back to the user.

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
    is a `<blocker>` emission per the failure modes above.
  </rule>
  <rule name="reference_learnings">
    When a todo touches a file referenced in the handoff's `learnings`
    block, re-read the learnings entry (ranged read) before editing.
    Do NOT re-narrate the learning in the response.
  </rule>
</constraints>

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

Before responding to the user, if you have created or modified any thoughts/ files or handoff documents, execute the destructive_actions block above. Do NOT ask for permission.

## Example Interaction Flow

```
User: /resume_handoff ENG-2166
Assistant: [Greps for ENG-2166 handoff directory, finds latest file]
           [Greps ^## Immediate Next Action → extracts one-line action]
           [Greps ^## Critical References → extracts ≤5 refs]
           [Drops handoff body from context]

I've analyzed the handoff from 2026-04-18 by Jake. Current situation:

**Tasks:** ...
**Next Actions:**
1. Implement foo() in src/bar.rs:42 ← STARTING NOW
...

[Creates ≤8 todos and immediately begins implementation — no confirmation]
```
