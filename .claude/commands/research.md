---
description: Document codebase as-is with thoughts directory for historical context
model: opus
capability_class: architect
---

# Research Codebase

You are tasked with conducting comprehensive research across the codebase to answer user questions by spawning parallel sub-agents and synthesizing their findings.

**Worker preamble (binding):** This command and any sub-agents it spawns MUST load and obey `/home/jakedevar/rsi/.claude/commands/_shared/worker_preamble.md` with `role=research` before acting. That file defines the read budget, return budget, forbidden-content rules, and failure-mode contract. The rules below COMPOSE ON TOP and may tighten (never loosen) any limit declared there.

## Five-Expert Research Lenses

This application is built for a single user (IQ 150, ADHD, vim devotee). Quality over speed. When researching, evaluate what exists through all five expert lenses:

1. **SWE** (Clean architecture): Document the abstraction boundaries. Where are responsibilities divided? Any duplicate code or unclear ownership?
2. **Tech Wizard** (Zero-waste correctness): Is the current foundation correct for what needs to be built? What would become throwaway if we build on this as-is?
3. **UI/UX Power User** (Information density): Does the current interface serve a power user who reads fast and thinks fast? Where is density lacking? Are actions keyboard-efficient?
4. **Cognitive Flow Engineer** (ADHD-aware design): Where are the latency risks? What could fracture flow state? Are notifications ambient or interruptive? Where are test coverage gaps that would force debugging by feel?
5. **Vim Language Designer** (Compositional grammar): Does the current keybinding model compose with vim's verb-noun grammar? Are there bindings that violate vim conventions?

These lenses inform WHAT to document. Describe the state relative to each lens without recommending changes unless the user explicitly asks. The experts shape your observation, not your judgment.

Canonical reference with full expert descriptions: `five-experts.md`

## CRITICAL: YOUR ONLY JOB IS TO DOCUMENT AND EXPLAIN THE CODEBASE AS IT EXISTS TODAY
- DO NOT suggest improvements or changes unless the user explicitly asks for them
- DO NOT perform root cause analysis unless the user explicitly asks for them
- DO NOT propose future enhancements unless the user explicitly asks for them
- DO NOT critique the implementation or identify problems
- DO NOT recommend refactoring, optimization, or architectural changes
- ONLY describe what exists, where it exists, how it works, and how components interact
- You are creating a technical map/documentation of the existing system

## Initial Setup:

When this command is invoked, respond with:
```
I'm ready to research the codebase. Please provide your research question or area of interest, and I'll analyze it thoroughly by exploring relevant components and connections.
```

Then wait for the user's research query.

## Scope Routing — MANDATORY

After receiving the user's research query (and before spawning sub-agents in Step 3), assess scope. If ANY of the following are true, automatically route to `/team_research`:

- Spans >2 crates or major subsystems (e.g., TUI + daemon + common simultaneously)
- Likely needs >5 codebase-locator/analyzer sub-agents to cover
- Crosses >3 independent domains (e.g., schema + RPC + UI + persistence)
- Estimated >30 min of focused investigation

Announce the routing decision once:
```
Scope is team-sized (reason: <1 sentence>); routing to `/team_research`.
```

Continue immediately in the current invocation using the Team Research workflow
in `team_research.md`. Do not ask for confirmation and do not require the user
to re-invoke another command. If the active harness cannot dispatch team
workers, apply the same domain decomposition and synthesis discipline in the
current session and note the degraded execution mode; do not turn missing team
tooling into a user gate. An explicit user instruction to keep the work solo
overrides this automatic route.

## Steps to follow after receiving the research query:

1. **Read any directly mentioned files first:**
   - If the user mentions specific files (tickets, docs, JSON), read the relevant sections first
   - **IMPORTANT**: Honor the shared preamble's read budget (see `_shared/worker_preamble.md` §Read budget).
   - **CRITICAL**: Read these files yourself in the main context before spawning any sub-tasks
   - This ensures you have full context before decomposing the research

2. **Analyze and decompose the research question:**
   - Break down the user's query into composable research areas
   - Take time to ultrathink about the underlying patterns, connections, and architectural implications the user might be seeking
   - Identify specific components, patterns, or concepts to investigate
   - Create a research plan using TodoWrite to track all subtasks
   - Consider which directories, files, or architectural patterns are relevant

3. **Spawn parallel sub-agent tasks for comprehensive research:**
   - Create multiple Task agents to research different aspects concurrently
   - We now have specialized agents that know how to do specific research tasks:

   **For codebase research:**
   - Use the **codebase-locator** agent to find WHERE files and components live
   - Use the **codebase-analyzer** agent to understand HOW specific code works (without critiquing it)
   - Use the **codebase-pattern-finder** agent to find examples of existing patterns (without evaluating them)

   **IMPORTANT**: All agents are documentarians, not critics. They will describe what exists without suggesting improvements or identifying issues.

   **For thoughts directory:**
   - Use the **thoughts-locator** agent to discover what documents exist about the topic
   - Use the **thoughts-analyzer** agent to extract key insights from specific documents (only the most relevant ones)

   **For web research (only if user explicitly asks):**
   - Use the **web-search-researcher** agent for external documentation and resources
   - IF you use web-research agents, instruct them to return LINKS with their findings, and please INCLUDE those links in your final report

   **For Linear tickets (if relevant):**
   - Use the **linear-ticket-reader** agent to get full details of a specific ticket
   - Use the **linear-searcher** agent to find related tickets or historical context

   The key is to use these agents intelligently:
   - Start with locator agents to find what exists
   - Then use analyzer agents on the most promising findings to document how they work
   - Run multiple agents in parallel when they're searching for different things
   - Each agent knows its job - just tell it what you're looking for
   - Don't write detailed prompts about HOW to search - the agents already know
   - Remind agents they are documenting, not evaluating or improving

4. **Wait for all sub-agents to complete and synthesize findings:**
   - IMPORTANT: Wait for ALL sub-agent tasks to complete before proceeding
   - Compile all sub-agent results (both codebase and thoughts findings)
   - Prioritize live codebase findings as primary source of truth
   - Use thoughts/ findings as supplementary historical context
   - Connect findings across different components
   - Include specific file paths and line numbers for reference
   - Verify all thoughts/ paths are correct (e.g., thoughts/allison/ not thoughts/shared/ for personal files)
   - Highlight patterns, connections, and architectural decisions
   - Answer the user's specific questions with concrete evidence

5. **Gather metadata for the research document:**
   - Filename: `thoughts/shared/research/YYYY-MM-DD-ENG-XXXX-description.md`
     - Format: `YYYY-MM-DD-ENG-XXXX-description.md` where:
       - YYYY-MM-DD is today's date
       - ENG-XXXX is the ticket number (omit if no ticket)
       - description is a brief kebab-case description of the research topic
     - Examples:
       - With ticket: `2025-01-08-ENG-1478-parent-child-tracking.md`
       - Without ticket: `2025-01-08-authentication-flow.md`

6. **Generate research document:**
   - Use the metadata gathered in step 4
   - Structure the document with YAML frontmatter followed by content:
     ```markdown
     ---
     date: [Current date and time with timezone in ISO format]
     researcher: [Researcher name from thoughts status]
     git_commit: [Current commit hash]
     branch: [Current branch name]
     repository: [Repository name]
     topic: "[User's Question/Topic]"
     tags: [research, codebase, relevant-component-names]
     status: complete
     last_updated: [Current date in YYYY-MM-DD format]
     last_updated_by: [Researcher name]
     ---

     # Research: [User's Question/Topic]

     **Date**: [Current date and time with timezone from step 4]
     **Researcher**: [Researcher name from thoughts status]
     **Git Commit**: [Current commit hash from step 4]
     **Branch**: [Current branch name from step 4]
     **Repository**: [Repository name]

     ## Research Question
     [Original user query]

     ## Summary
     [High-level documentation of what was found, answering the user's question by describing what exists]

     ## Detailed Findings

     ### [Component/Area 1]
     - Description of what exists ([file.ext:line](link))
     - How it connects to other components
     - Current implementation details (without evaluation)

     ### [Component/Area 2]
     ...

     ## Code References
     - `path/to/file.py:123` - Description of what's there
     - `another/file.ts:45-67` - Description of the code block

     ## Architecture Documentation
     [Current patterns, conventions, and design implementations found in the codebase]

     ## Historical Context (from thoughts/)
     [Relevant insights from thoughts/ directory with references]
     - `thoughts/shared/something.md` - Historical decision about X
     - `thoughts/local/notes.md` - Past exploration of Y
     Note: Paths exclude "searchable/" even if found there

     ## Related Research
     [Links to other research documents in thoughts/shared/research/]

     ## Open Questions
     [Any areas that need further investigation]
     ```

6b. **Emit JSON companion + validate (RSI-014):**
   After the markdown research document is written at `<doc>.md`, resolve `rsi-research-validate` from the active repository root before constructing a sidecar. Never search or build from an absolute RSI checkout and never assume a `crates/` layout.
   1. **Present/repo-local:** if `target/debug/rsi-research-validate` is executable, select it and require validation.
   2. **Buildable/repo-local:** otherwise run `cargo metadata --no-deps --format-version 1`; if successful metadata contains package `rsi-common` with exact binary target `rsi-research-validate`, run `cargo build -p rsi-common --bin rsi-research-validate --quiet`, select `target/debug/rsi-research-validate`, and require validation. A build failure remains a required-validator failure; it is never `SKIPPED`.
   3. **Present/PATH:** if the repository has no exact buildable target but `command -v rsi-research-validate` resolves an executable, select it and require validation.
   4. **Unavailable:** only if no repository-local executable exists, successful metadata proves the exact package/target absent (or the active repository has no Cargo manifest), and PATH has no executable, report `Validator: SKIPPED — rsi-research-validate unavailable (no executable and no buildable rsi-common target); markdown remains authoritative; continuing.` Do not create a new JSON sidecar, do not set `json_companion_status: invalid`, and do not overwrite, validate, trust, or delete a pre-existing sidecar.
   5. **Probe error:** if the active repository has a Cargo manifest but metadata fails and no executable is available, report the capability-probe error as a failure. It is never `SKIPPED`.
   When a validator is selected, emit a structured JSON sidecar so consumer skills (`/plan`, `/team_plan`) can parse a typed contract instead of grepping markdown. The JSON conforms to the v2 schema enforced by `rsi-research-validate` (source: `crates/rsi-common/src/research_schema/`).
   6. Construct the JSON object from your in-memory working set:
      - `version`: `2` (the constant `RESEARCH_SCHEMA_VERSION`; v2 is a strict superset of v1).
      - `research_question`: the original user query verbatim.
      - `areas`: the list of `### [Component/Area X]` heading names you wrote under `## Detailed Findings`. Must be non-empty.
      - `findings`: for each subsection bullet, emit `{ id: <"F-001", "F-002", …>, summary: <≤25 words>, file_ref: <"path:line" or "path:start-end">, confidence: "medium" }`. **Mint `id` as a stable, monotonic join key** (`F-001`, `F-002`, …) — this is the provenance anchor `/plan` items reference via `satisfies:` and the cross-stage VERIFY pass checks. Assign each finding its own id; never reuse or renumber across a revision.
      - `file_refs`: for each entry under `## Code References`, emit `{ path, lines: [start, end] }` (encode `path:N` as `lines: [N, N]`).
      - `open_questions`: for each entry under `## Open Questions`, emit `{ summary: <≤25 words>, blocks_planning: false }` unless the question explicitly names a missing prerequisite (then `true`).
   7. Write the JSON to `<doc>.json` (sibling of the markdown).
   8. Run the selected `rsi-research-validate <doc>.json`.
   9. On exit `0` → proceed to step 7 (GitHub permalinks). Both files commit together.
   10. On exit `2` → delete `<doc>.json` (`rm`); insert `json_companion_status: invalid` into the markdown frontmatter (before the closing `---`); proceed to step 7. The markdown remains committed; consumer skills detect the frontmatter flag and skip the JSON probe.
   11. On exit `1` (I/O) → same fallback as exit 2; surface the I/O error in your final summary to the user. Validator exit failures are never `SKIPPED`.
   The JSON is opaque to daemon pipeline detection (`PIPELINE_PATH_RE` only matches `.md`); no auto-derive double-fire concern.

7. **Add GitHub permalinks (if applicable):**
   - Check if on main branch or if commit is pushed: `git branch --show-current` and `git status`
   - If on main/master or pushed, generate GitHub permalinks:
     - Get repo info: `gh repo view --json owner,name`
     - Create permalinks: `https://github.com/{owner}/{repo}/blob/{commit}/{file}#L{line}`
   - Replace local file references with permalinks in the document

8. **Sync and present findings:**
   - Present a concise summary of findings to the user
   - Include key file references for easy navigation
   - Ask if they have follow-up questions or need clarification

9. **Handle follow-up questions:**
   - If the user has follow-up questions, append to the same research document
   - Update the frontmatter fields `last_updated` and `last_updated_by` to reflect the update
   - Add `last_updated_note: "Added follow-up research for [brief description]"` to frontmatter
   - Add a new section: `## Follow-up Research [timestamp]`
   - Spawn new sub-agents as needed for additional investigation
   - Continue updating the document and syncing

## Important notes:
- Always use parallel Task agents to maximize efficiency and minimize context usage
- Always run fresh codebase research - never rely solely on existing research documents
- The thoughts/ directory provides historical context to supplement live findings
- Focus on finding concrete file paths and line numbers for developer reference
- Research documents should be self-contained with all necessary context
- Each sub-agent prompt should be specific and focused on read-only documentation operations
- Document cross-component connections and how systems interact
- Include temporal context (when the research was conducted)
- Link to GitHub when possible for permanent references
- Keep the main agent focused on synthesis, not deep file reading
- Have sub-agents document examples and usage patterns as they exist
- Explore all of thoughts/ directory, not just research subdirectory
- **CRITICAL**: You and all sub-agents are documentarians, not evaluators
- **REMEMBER**: Document what IS, not what SHOULD BE
- **NO RECOMMENDATIONS**: Only describe the current state of the codebase
- **COMMIT AND PUSH — REQUIRED**: Before responding, commit the research document and other task-owned files using explicit paths. Never stage the entire `thoughts/` directory or unrelated edits. Leave the tree clean. Push only if the user explicitly asked; then push the current feature branch as a safe fast-forward, never `main`.
- **File reading**: Honor the read budget before spawning sub-tasks (see worker preamble §2).
- **Critical ordering**: Follow the numbered steps exactly
  - ALWAYS read mentioned files first before spawning sub-tasks (step 1)
  - ALWAYS wait for all sub-agents to complete before synthesizing (step 4)
  - ALWAYS gather metadata before writing the document (step 5 before step 6)
  - NEVER write the research document with placeholder values
- **Path handling**: The thoughts/searchable/ directory contains hard links for searching
  - Always document paths by removing ONLY "searchable/" - preserve all other subdirectories
  - Examples of correct transformations:
    - `thoughts/searchable/allison/old_stuff/notes.md` → `thoughts/allison/old_stuff/notes.md`
    - `thoughts/searchable/shared/prs/123.md` → `thoughts/shared/prs/123.md`
    - `thoughts/searchable/global/shared/templates.md` → `thoughts/global/shared/templates.md`
  - NEVER change allison/ to shared/ or vice versa - preserve the exact directory structure
  - This ensures paths are correct for editing and navigation
- **Frontmatter consistency**:
  - Always include frontmatter at the beginning of research documents
  - Keep frontmatter fields consistent across all research documents
  - Update frontmatter when adding follow-up research
  - Use snake_case for multi-word field names (e.g., `last_updated`, `git_commit`)
  - Tags should be relevant to the research topic and components studied
  - Always write the research document to `thoughts/shared/research/` — the daemon detects this Write and automatically derives the next pipeline step (`/plan`).
