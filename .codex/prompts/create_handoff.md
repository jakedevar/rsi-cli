---
description: Write the handoff that lets the next session continue this work
---

# Create Handoff

You are the closing agent for a pipeline stage, tasked with producing a handoff document for the next session. The handoff is a channel, not a diary — every word in it is read-tax on the resuming agent. The shared worker preamble governs read and return budgets; the `<handoff_document>` schema below governs the document you write.

**Worker preamble (binding):** This command MUST load and obey `/home/jakedevar/rsi/.claude/commands/_shared/worker_preamble.md` with `role=handoff` before acting. That file defines the read budget (Grep-then-Read targeted ranges; full-file reads only for files <400 lines; ≤8k-token read budget before acting), the forbidden-content rules, and the failure-mode contract. The return-budget section of the preamble is INAPPLICABLE to this command — handoff commands write to disk or drive a live session rather than return a bounded blob to a master orchestrator. The rules below COMPOSE ON TOP of the preamble and may tighten (never loosen) any limit declared there.

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

## Process

### 1. Filepath & Metadata
Path and naming:
    - write to `thoughts/shared/handoffs/<TICKET>/<YYYY-MM-DD_HH-MM-SS>_<TICKET>_<slug>.md`:
        - date part: today, as `YYYY-MM-DD`
        - time part: the current time on a 24-hour clock, as `HH-MM-SS` (`13-55-22` for 1:55:22 pm)
        - `<TICKET>` directory: the ticket id, or `general` when there is none
        - `_<TICKET>` in the file name: the ticket id again; drop it and its underscore when there is none
        - `<slug>`: a few kebab-case words naming the work
    - Run the `scripts/spec_metadata.sh` script to generate all relevant metadata. If the script is absent, fall back to `date -u +%Y-%m-%d_%H-%M-%S` and `git rev-parse HEAD` inline.
    - Examples:
        - ticket: `ENG-2166/2026-09-23_21-40-05_ENG-2166_session-rotation-fix.md`
        - no ticket: `general/2026-09-23_21-40-05_session-rotation-fix.md`

### 2. Handoff Writing

Write the handoff document using the defined filepath and YAML frontmatter below. The `<handoff_document>` schema defines field caps — obey them. Fields render as markdown headings on disk so `/resume_handoff` extraction heuristics keep working; the XML is the author-side contract.

```markdown
---
date: [ISO 8601 timestamp with timezone offset]
researcher: [author name or agent id]
git_commit: [output of git rev-parse HEAD]
branch: [Current branch name]
repository: [Repository name]
topic: "[Work item] - implementation handoff"
tags: [implementation, <component-names>]
status: complete
last_updated: [YYYY-MM-DD]
last_updated_by: [same as researcher]
type: implementation_strategy
---
```

<handoff_document>
  <frontmatter>
    date, researcher, git_commit, branch, repository, topic, tags,
    status, last_updated, last_updated_by, type
    <!-- unchanged; keep the existing YAML contract above -->
  </frontmatter>

  <title max_words="10">ENG-XXXX {concise description}</title>

  <immediate_next_action max_words="20" required="true" load_bearing="true">
    ONE imperative sentence. Examples:
      "Implement the foo() function in src/bar.rs:42"
      "Run the failing test suite and fix the type error in crates/rsi/src/types.rs:88"
    This is the resuming agent's first decision point. Terse and specific.
    Rendered under heading: ## Immediate Next Action
  </immediate_next_action>

  <original_request max_words="60" required="true">
    The user's original question/request, quoted verbatim if ≤60 words;
    summarized in ≤60 words otherwise. Do NOT paraphrase stylistically —
    preserve wording where possible.
    Rendered under heading: ## Original Request
  </original_request>

  <tasks max_items="5" max_words_per="25" format="status_enum">
    Per item: {task, status: enum[done|wip|planned], plan_phase?}.
    If working from a plan, cite the phase (e.g. "Phase 2 wip").
    Rendered under heading: ## Task(s)
  </tasks>

  <critical_references max_items="3" file_line_refs_only="true">
    2-3 most important file:line or absolute paths. No prose.
    Rendered under heading: ## Critical References
  </critical_references>

  <recent_changes max_items="8" format="file:line — ≤15 words">
    Git log is the channel for detail. This field is a ≤8-bullet summary
    with file:line refs. Include commit SHA range or branch name if useful.
    Rendered under heading: ## Recent Changes
  </recent_changes>

  <learnings max_items="8" max_words_per="150_total">
    Patterns, root causes, gotchas the next agent needs to know.
    Hard cap: 150 words across ≤8 bullets. Do NOT restate content from
    the plan or research doc; reference by file path instead.
    Rendered under heading: ## Learnings
  </learnings>

  <artifacts max_items="6" file_line_refs_only="true">
    Paths to docs you produced or updated. Paths only. No descriptions.
    Rendered under heading: ## Artifacts
  </artifacts>

  <action_items max_items="10" max_words_per="20">
    Ordered backlog for the next agent, excluding `immediate_next_action`
    (which is the first step). ≤10 bullets × ≤20 words each.
    Rendered under heading: ## Action Items & Next Steps
  </action_items>

  <other_notes max_items="3" max_words_per="25" max_words="100" optional="true">
    Catchall. Hard cap 100 words total. Omit the field entirely if empty.
    Rendered under heading: ## Other Notes
  </other_notes>

  <stage_contract required="true">
    The MWP/ICM stage-contract block (see AGENTS.md §Agent Contract &
    Thought-System Discipline). Four `###` sub-sections in canonical order —
    Inputs, Process, Outputs, Verify. `### Inputs` MUST declare named static
    inputs (file/artifact refs) OR a code-discovery budget (`rg`/glob) or both.
    Self-check the shape against `scan_contract_block`
    (`crates/rsi-common/src/handoff_schema/body.rs`). This is ADDITIVE — it is
    NOT part of the on-disk handoff v1 schema and does not bump
    HANDOFF_SCHEMA_VERSION, so it does not affect `rsi-handoff-validate` in
    Step 3a; a handoff without it still validates, but pipeline handoffs must
    carry it so the next stage can verify coverage.
    Rendered under heading: ## Stage contract (with `### Inputs` / `### Process`
    / `### Outputs` / `### Verify` sub-sections).
  </stage_contract>
</handoff_document>

### 3a. Validate before save (RSI-013)

Before approving the draft, run the structured validator against it. This
is the write-time gate that prevents malformed handoffs from reaching
disk.

1. Confirm the binary exists at `target/debug/rsi-handoff-validate`. If it
   does not, run `cargo build -p rsi-common --bin rsi-handoff-validate
   --quiet`. The build is required before a handoff can be saved.
2. Run `target/debug/rsi-handoff-validate --strict <draft path>`.
3. **Exit 0** → proceed to Step 3 (approve and sync).
4. **Exit 2** → emit a `<blocker>` containing the validator's stdout
   `errors[]` array; do NOT save the file; do NOT stage; halt.
5. **Exit 1** → I/O failure (path missing, unreadable). Emit a
   `<blocker>` with the validator's stderr; halt.

The validator enforces the field caps declared in `<handoff_document>`
above. v1 schema version: `1` (defined in
`crates/rsi-common/src/handoff_schema/rules.rs::HANDOFF_SCHEMA_VERSION`).

### 3. Approve and Sync

Save the document. Once the handoff document is written to `thoughts/shared/handoffs/`, the daemon automatically detects this Write and derives the next pipeline step (`/resume_handoff`). No special tags are needed — the file path IS the signal.

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
      `master`; emit &lt;blocker&gt; with the branch name and abort.
    on_failure: "Abort. Emit &lt;blocker&gt; with the push stderr. Do NOT modify
      the handoff file. Do NOT retry automatically — surface to the user."
  </action>
</destructive_actions>
