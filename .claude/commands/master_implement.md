---
description: End-to-end autonomous pipeline — research, plan, and implement a ticket using agent teams
model: opus
capability_class: architect
---

# Master Implement

Full autonomous pipeline orchestrator. Given a ticket or goal, the master sequentially drives three agent teams through the complete research → plan → implement cycle, collecting a compact executive summary from each stage. The master owns the human verification gate and delivers a detailed final report to Jake.

**Status:** legacy-compatible full-pipeline command. Prefer `/master_orchestrate`
for slice-scoped, resumable, high-risk, review-gated, or multi-slice program
work. Use this command when you specifically want the older full
research → plan → implement pipeline shape.

**Use this for:** tickets where you want zero manual handoffs between research, planning, and implementation.

**Use the individual team commands when:** you want to review findings between stages, adjust the plan before implementing, or re-run a single stage in isolation.

---

<handoff_contract>
  <required>
    <field name="doc_path">absolute path to the artifact this stage produced</field>
    <field name="status">one of: complete | blocked | partial</field>
    <field name="manifest_path" stages="IMPLEMENTATION,VERIFY">absolute path to verification manifest</field>
  </required>
  <optional>
    <field name="blocker" max_words="30">one-sentence blocker description</field>
    <field name="next_action_hint" max_words="20">optional pointer for next stage</field>
    <field name="daemon_checks" stages="VERIFY">PASS_count/total_count</field>
    <field name="failed_checks" stages="VERIFY">comma-separated failed daemon checks</field>
  </optional>
  <forbidden>findings, plan_summary, code_snippets, file_contents, stage_narrative</forbidden>
</handoff_contract>

**Worker preamble (binding for all spawned workers):** `/home/jakedevar/rsi/.claude/commands/_shared/worker_preamble.md`. Each spawned worker MUST load and obey that file's rules.

**Preflight (RSI-021):** `SendMessage` is a deferred tool. Before Step 1, invoke `ToolSearch` with `select:SendMessage,PushNotification,TaskUpdate` to load their schemas. The corrective-retry path in each parse step depends on `SendMessage`; Step 4 uses `PushNotification` + `TaskUpdate`. Also build the contract and manifest validators plus RPC helper once before dispatching: `cargo build -q -p rsi-common --bin rsi-contract-validate --bin rsi-manifest-validate --bin rsi-rpc`.

---

## Shared safety procedures

These procedures are referenced by every stage below. They exist once here so per-stage logic stays tight.

### Validator failure handling

When a stage's `rsi-contract-validate` exits non-zero, parse the validator's `kind` field:

- **Format failures** (`kind ∈ {malformed_first_line, unknown_stage, missing_field, malformed_field}`) are template/parsing mismatches, not bad work. SendMessage once with the corrective contract block. Re-validate. If the second reply also fails as a format error, retry ONCE more. If the third attempt still fails, HALT — the template is broken; manual intervention required.
- **Content failures** (any other kind, or absent) reflect real problems with the work. SendMessage once with a corrective directive. Re-validate. If it still fails, HALT and report to Jake.

### Post-stage scope audit

After each stage's validator exits 0, run:

```bash
git -C <repo_or_worktree> log <prev_head>..HEAD --oneline
```

(`<prev_head>` is the SHA before the stage started; record it before dispatching the agent.) Every commit subject MUST match the stage's expected prefix:

- Research → `research:` or `research(<ticket>):`
- Plan → `<ticket>:` (`<ticket> plan…` is common)
- Implementation → `<ticket> Phase N:` or `<ticket>:`

Any commit that doesn't match (unrelated work, sweeps of a dirty worktree, accidental rebases) is surfaced to Jake before dispatching the next stage. If you cannot determine whether a commit belongs to the stage, ASK — never silently proceed.

If independently re-running the worker's tests from outside the worker's worktree, force a separate target dir to avoid stale-cache false-negatives: `CARGO_TARGET_DIR=~/.rsi/tmp/cargo-targets/<ticket-id> cargo test ...`. The repo's `.cargo/config.toml` points all worktrees at `~/.cargo/shared-target`, and that cache will otherwise serve binaries built from the parent worktree's source and silently report `0 passed; 0 failed`. NEVER point a scratch `CARGO_TARGET_DIR` at `/tmp` — it is a tmpfs mounted with a per-user quota, and multi-GB cargo target trees there exhaust the quota and fail ALL writes system-wide with `EDQUOT`. `~/.rsi/tmp/cargo-targets/` is disk-backed and reclaimed by `make clean-shared`.

### Validator behavior reference

The master pipes worker replies through `cargo run -q -p rsi-common --bin rsi-contract-validate -- <ticket> < reply.txt`. Internal grammar (source: `crates/rsi-common/src/agent_contract.rs`):

- The validator scans the **first 200 bytes** of the reply for the marker regex `(?m)^PIPELINE HANDOFF — ([A-Z]+):\s*$`. Small preambles (< 200 bytes before the marker, anchored on its own line) ARE tolerated; longer preambles push the marker past the window and produce `MalformedFirstLine` or `MissingMarker`. The worker_preamble's "first non-blank line MUST be the header" rule is STRICTER than the validator — preferable, but the validator alone won't always catch a small preamble.
- Marker must use **em-dash U+2014** (`—`), not hyphen-minus (`--`). RSI-021 picked this delimiter to avoid collisions with `--` in code blocks.
- Stage label is case-sensitive and must be exactly one of `RESEARCH`, `PLAN`, `IMPLEMENTATION`, `VERIFY`. Lowercase fails. `PLANNING` fails (legacy template label — fix on sight).
- `doc_path` accepts these aliases (case-insensitive): `doc_path`, `research document`, `plan document`, `implementation document`, `impl_doc`.
- `manifest_path` accepts: `manifest_path`, `manifest path`, `manifest`. Required for IMPLEMENTATION and VERIFY stages; absence yields `MissingField{manifest_path}`.
- `status` is OPTIONAL — defaults to `complete` if missing. Workers may omit it from research/plan handoffs.
- `ticket:` line in the handoff is OPTIONAL; only validated if the worker emits one. If emitted and mismatched with the CLI `--ticket <id>` arg, yields `TicketMismatch`.

### Project status writeback (BEFORE and AFTER every stage)

Every pipeline stage MUST refresh the **ticket file frontmatter** TWICE per stage — immediately before the worker spawns AND immediately after the contract validator exits 0. The ticket frontmatter is the **single source of truth** for pipeline state. The project index status table is a derived view, regenerated from ticket frontmatter by `scripts/render-project-index.sh` (called by each writeback). Never write to the index Status column by hand; it will be overwritten.

Without this writeback discipline, the SVR-004 cycle's bookkeeping (status flip from `ready` → `complete`) ran only once at the very end, manually — meaning a mid-pipeline crash would have left the ticket showing `ready` while the work was already half-done.

**Locating files:**
- The ticket file is the master's input path (or recovered via Step 0.5 reconciliation).
- The project directory is `<ticket_file_directory>/..`. If it contains an `index.md`, run the render script after each ticket-frontmatter change. If it does not, the ticket is standalone — just write the ticket file (do NOT halt).

**Status vocabulary** (value of the `status:` key in the ticket frontmatter — the index Status column mirrors this automatically via the render script):

| Value | Meaning |
|---|---|
| `ready` | Never started |
| `researching` | `pipeline-research` currently running |
| `research-done` | Research artifact recorded, ready for plan |
| `planning` | `pipeline-plan` currently running |
| `plan-done` | Plan artifact recorded, ready for implementation |
| `implementing` | `pipeline-implement` currently running |
| `impl-done` | Implementation artifacts recorded, ready for verify |
| `verifying` | `pipeline-verify` currently running |
| `verify-done` | Daemon checks passed (or `n/a` if zero daemon items) |
| `awaiting-jake` | At the human verification gate (Step 4) |
| `complete` | Branch pushed, final report delivered |
| `blocked` | Any HALT — surface the blocking condition in the ticket body |

**Before each stage** (immediately before the foreground agent spawn):

1. Update the ticket file: `status: <verb>` (e.g. `researching`), `last_run: <today's ISO datetime>`.
2. Regenerate the project index: `scripts/render-project-index.sh <project-dir>` (idempotent; safe to run unconditionally).
3. Commit alone with subject `<TICKET>: enter <stage>`. Use explicit `git add` paths:
   ```bash
   git add <ticket-file> <project-dir>/index.md
   git commit -m "<TICKET>: enter <stage>"
   ```
   If the ticket has no parent project, omit the index.md path.

**After each stage** (immediately after the contract validator exits 0, after the post-stage scope audit, before the next stage's before-writeback):

1. Update the ticket file: `status: <done-state>` (e.g. `research-done`) plus the stage's artifact keys (`research_doc`, `plan_doc`, `manifest_path`, `branch`, `worktree`, `phase_commits`, `last_stage`).
2. Regenerate the project index: `scripts/render-project-index.sh <project-dir>`.
3. Commit alone with subject `<TICKET>: finish <stage>` — explicit `git add` paths for the ticket file and index.md.

**Crash-recovery semantics:** if the master process dies between the before-writeback and the after-writeback, Step 0.5 reconciliation on the next invocation sees the ticket frozen at the verb-form status (e.g. `researching`) and surfaces it as drift to Jake with the recovery options. If the master dies between the after-writeback and the next before-writeback, the ticket reads the done-form (e.g. `research-done`) and the resume path is unambiguous. The index is informational only — it can be regenerated at any time from the canonical ticket frontmatter and never needs to be trusted in a recovery decision.

**Write-failure handling:** if the ticket-file write fails (file unwritable, frontmatter malformed), log the intended write verbatim and continue. The pipeline doesn't halt on bookkeeping failure — Step 6 surfaces unapplied writes for manual application. If `render-project-index.sh` exits non-zero, commit the ticket-file change anyway (it's the source of truth) and surface the script error to Jake — the index will be stale but the canonical record stays correct.

**Commit hygiene:** each writeback is its own commit. Six writebacks per cycle (three before + three after, plus Step 6.5) creates a clear timeline a future maintainer can read with `git log --oneline -- thoughts/shared/projects/<project>/`. Don't bundle writebacks with other work. The index regeneration is bundled into the same commit as the ticket-frontmatter change that triggered it.

**Design rationale (recorded for future maintainers):** an earlier version of this spec (committed `507aa5d7`, superseded) wrote pipeline state to BOTH the ticket frontmatter AND the index status table as separate manual updates. The Tech Wizard expert lens vetoed that design — same information written to two places is the textbook drift recipe. The current design writes to one canonical place; the render script derives the other. See the Agentic Harness Architect lens: derived caches, not duplicated state.

---

## Step 0: Input Ingestion

**MANDATORY FIRST:** Detect the execution environment before any other work. Run:

```bash
uname -a && echo "---" && cat /etc/os-release 2>/dev/null || sw_vers 2>/dev/null || echo "Unknown OS"
```

Record the exact OS name, kernel version, and architecture. This information MUST be injected into every downstream agent prompt to prevent platform assumption errors.

Read any provided files (ticket, research docs, related plans) per the shared preamble's read budget — Grep-then-Read targeted ranges, full-file reads only for files <400 lines.

Extract and log:
- Core goal
- Explicit constraints or requirements
- Any files/systems already known to be involved
- **Environment:** OS, kernel, architecture (from detection above)

If no input is provided, respond:

```
Please provide a ticket, goal, or file path to start the full pipeline.

Examples:
  /master_implement thoughts/allison/tickets/eng_1234.md
  /master_implement "add per-session model switching to TUI and daemon"
```

Then wait for input.

---

## Step 0.5: State Reconciliation

If the input was a free-text goal (no ticket file), skip this step.

If the input was a ticket file path, the ticket file is the master plan record — it is the source of truth for "what stage is this ticket in". Before dispatching any agent, reconcile that record against reality.

Read the ticket's YAML frontmatter and check these fields (any may be absent on a never-run ticket):

- `status`: one of the values defined in the shared **Project status writeback** vocabulary (`ready`, `researching`, `research-done`, `planning`, `plan-done`, `implementing`, `impl-done`, `verifying`, `verify-done`, `awaiting-jake`, `complete`, `blocked`)
- `research_doc`: absolute path to research artifact, if any
- `plan_doc`: absolute path to plan artifact, if any
- `manifest_path`: absolute path to verification manifest, if any
- `branch`: feature branch name, if any
- `worktree`: absolute path to worktree, if any
- `phase_commits`: list of SHAs landed by the implementation stage, if any
- `last_stage`: `research` | `plan` | `implementation` | `verify`
- `last_run`: ISO datetime of last pipeline activity
- `completed`: ISO date the ticket was marked `complete`, if any
- `pr_url`: URL of the PR opened/merged in Step 5, if any

Verb-state values (`researching` / `planning` / `implementing` / `verifying` / `awaiting-jake`) ALWAYS indicate the previous run died mid-stage. Done-state values (`research-done` / `plan-done` / `impl-done` / `verify-done`) indicate clean cross-stage boundaries — pick those up by resuming at the next stage's before-writeback.

In parallel, scan filesystem and git for evidence the frontmatter may not yet reflect (drift detection):

```bash
git branch -a | grep -i <ticket-id>
ls thoughts/shared/research/ 2>/dev/null | grep -i <ticket-id>
ls thoughts/shared/plans/ 2>/dev/null | grep -i <ticket-id>
```

**If `status == ready` AND zero artifacts found anywhere** → ticket is virgin. Proceed to Step 1.

**Otherwise** the ticket is non-virgin. HALT and report to Jake:

```
Ticket [ID] is not in a fresh state.

Frontmatter:    status=[X], last_stage=[Y], last_run=[Z]
Research doc:   [path from frontmatter, or "none recorded"]
Plan doc:       [path from frontmatter, or "none recorded"]
Branch:         [name from frontmatter, or "none recorded"]
Worktree:       [path from frontmatter, or "none recorded"]

Filesystem / git evidence (drift):
  - [any artifacts found on disk or in git not recorded in frontmatter]

Options:
  1. Resume from <next stage based on last_stage> — thread existing artifacts forward
  2. Force re-run from research stage — existing artifacts archived to .bak before overwrite
  3. Skip pipeline — read existing implementation from disk, jump to Step 4 verification gate
  4. Abort — let me investigate manually
```

Wait for Jake's choice. Do NOT proceed without explicit direction.

**Reconciliation rules:**
- Option 1 (resume): skip completed stages, dispatch only remaining stages, pass existing artifact paths forward as inputs.
- Option 3 (skip to verification): read implementation artifacts from disk, synthesize the verification checklist directly without re-dispatching the implementation agent.
- Any drift discovered during reconciliation (artifact on disk but not in frontmatter, or vice versa) is itself a finding to surface — never silently overwrite.

---

## Step 1: Research Stage

**Project status writeback (BEFORE):** run the shared procedure with verb-state `researching` — writes `status: researching` and `last_run: <now>` to the ticket frontmatter, regenerates the index via `scripts/render-project-index.sh`, commits with subject `<TICKET>: enter research`. Do this before the spawn.

Spawn a **foreground agent** (do NOT use `run_in_background`) named `"pipeline-research"` with this prompt:

```
PIPELINE MODE: true
PIPELINE STAGE: research

ENVIRONMENT:
OS: [exact OS from Step 0 detection]
Kernel: [kernel version]
Architecture: [architecture]

FIRST ACTION (mandatory before any other work):
  Run: export CLAUDE_AGENT_ROLE=pipeline-research
  This tells the pre-commit hook (tools/git-hooks/pre-commit, RSI-021) which
  branch/path invariants apply to your stage. The hook rejects commits that
  violate those invariants — without this export, you may silently land
  research on the wrong branch.

Your job is to run the /team_research command for the following goal.

GOAL:
[FULL TICKET TEXT OR GOAL DESCRIPTION]

CRITICAL: Before any implementation research, verify the goal's platform feasibility
on the detected environment above. If the goal requires platform-specific APIs,
libraries, or system services that are NOT available on the detected OS, you MUST
mark the research as BLOCKED in your handoff.

Use the Skill tool to invoke /team_research with the goal above as input.
Follow the team_research command instructions in full.

After research is complete and the document is committed, your FINAL MESSAGE TO THE
MASTER MUST CONTAIN ONLY the block below — nothing before it, nothing after it:

PIPELINE HANDOFF — RESEARCH:
=============================
Research document: [path to thoughts/shared/research/... file]
Research question: [1 sentence]
Platform feasibility: [confirmed | BLOCKED — reason]
Key findings:
  - [finding 1 with file:line reference]
  - [finding 2 with file:line reference]
  - [finding 3 with file:line reference]
Codebase areas: [comma-separated list of areas researched]
Open questions: [unresolved gaps, or "none"]
```

Wait for this agent to complete. Validate the worker's reply via:

  cargo run -q -p rsi-common --bin rsi-contract-validate -- RSI-XXX < reply.txt

(Replace RSI-XXX with the ticket ID for this run.) On non-zero exit, follow the shared **Validator failure handling** procedure above. On exit 0, also run the shared **Post-stage scope audit** before dispatching the next stage.

Once the validator exits 0, parse the structured `PipelineHandoff` JSON it
emits to stdout for {stage, doc_path, status, blocker?}.

**Platform feasibility gate (MANDATORY):** Check the handoff JSON for
`platform_feasibility` field. If the value contains "BLOCKED", immediately
stop and report:

```
Pipeline stopped at Research stage.

PLATFORM INCOMPATIBILITY DETECTED
Environment: [OS from Step 0]
Reason: [platform feasibility blocker from research]

This goal requires platform-specific capabilities that are not available
on the detected environment. Consider:
  1. Adjust the goal to use cross-platform alternatives
  2. Run the pipeline on the required platform
  3. Split the goal into platform-agnostic parts
```

If the research agent fails or returns a blocker, stop and report to Jake:

```
Pipeline stopped at Research stage.

Reason: [what the research agent returned]
Recommended action: [retry with narrower scope / investigate manually]
```

**Project status writeback (AFTER):** run the shared procedure with done-state `research-done` and stage artifacts `research_doc: <path>`, `last_stage: research`. Commits with subject `<TICKET>: finish research`. The Step 2 before-writeback follows immediately as a separate commit.

---

## Step 2: Planning Stage

<discard_after_extract>
After parsing the RESEARCH handoff, extract ONLY {doc_path, status, blocker?}.
Explicitly drop all other stage content from working memory before spawning the next stage.
Do not quote, do not summarize, do not retain.
</discard_after_extract>

**Project status writeback (BEFORE):** run the shared procedure with verb-state `planning` — commits with subject `<TICKET>: enter plan`.

Spawn a **foreground agent** named `"pipeline-plan"` with this prompt:

```
PIPELINE MODE: true
PIPELINE STAGE: plan
RESEARCH DOC: <path from Step 1>

ENVIRONMENT:
OS: [exact OS from Step 0 detection]
Kernel: [kernel version]
Architecture: [architecture]

FIRST ACTION (mandatory before any other work):
  Run: export CLAUDE_AGENT_ROLE=pipeline-plan
  This tells the pre-commit hook (tools/git-hooks/pre-commit, RSI-021) which
  branch/path invariants apply to your stage. The hook rejects commits that
  violate those invariants — without this export, you may silently land
  the plan on the wrong branch.

Read the research doc yourself. BEFORE creating any plan, verify that the research
shows "Platform feasibility: confirmed". If it shows "BLOCKED", do NOT create a
plan — immediately return Status: blocked with the platform incompatibility as
the blocker.

Deliverable: a plan document committed to disk.

After the plan is committed, your FINAL MESSAGE TO THE MASTER MUST CONTAIN ONLY
the block below — nothing before it, nothing after it. The first character of
your reply must be the literal "P" of "PIPELINE HANDOFF — PLAN:". The validator
(`crates/rsi-common/src/agent_contract.rs`) accepts the literal stage token
"PLAN" — not "PLANNING" — so use exactly the form below:

PIPELINE HANDOFF — PLAN:
=============================
Plan document: [absolute path to thoughts/shared/plans/... file]
Status: complete | partial | blocked
Blocker: [≤30 words, omit if status=complete]
Next action hint: [≤20 words, optional]
```

Wait for this agent to complete. Validate the worker's reply via the same
binary used in Step 1 (`cargo run -q -p rsi-common --bin rsi-contract-validate
-- RSI-XXX < reply.txt`). On non-zero exit, follow the shared **Validator
failure handling** procedure. On exit 0, parse the structured `PipelineHandoff`
JSON for {stage, doc_path, status, blocker?} and run the shared **Post-stage
scope audit**.

If the plan agent fails or produces unresolved blockers, stop and report to Jake:

```
Pipeline stopped at Planning stage.

Reason: [what the plan agent returned]
Plan document (partial): [path if written]
Recommended action: [revise the plan to resolve the blocker / adjust scope]
```

**Project status writeback (AFTER):** run the shared procedure with done-state `plan-done` and stage artifact `plan_doc: <path>`, `last_stage: plan`. Commits with subject `<TICKET>: finish plan`.

---

## Step 3: Implementation Stage

<discard_after_extract>
After parsing the PLAN handoff, extract ONLY {doc_path, status, blocker?}.
Explicitly drop all other stage content from working memory before spawning the next stage.
Do not quote, do not summarize, do not retain.
</discard_after_extract>

**Project status writeback (BEFORE):** run the shared procedure with verb-state `implementing` — commits with subject `<TICKET>: enter implementation`.

Spawn a **foreground agent** named `"pipeline-implement"` with this prompt:

```
PIPELINE MODE: true
PIPELINE STAGE: implementation

ENVIRONMENT:
OS: [exact OS from Step 0 detection]
Kernel: [kernel version]
Architecture: [architecture]

FIRST ACTION (mandatory before any other work):
  Run: export CLAUDE_AGENT_ROLE=pipeline-implement
  This tells the pre-commit hook (tools/git-hooks/pre-commit, RSI-021) which
  branch/path invariants apply to your stage. The hook refuses commits on
  main when this role is set — without the export, you may silently land
  implementation on main and bypass the worktree invariant.

PLAN DOC: <absolute path written by Step 2>
MANIFEST PATH CONVENTION: compute as `<worktree>/thoughts/shared/verification/<TICKET>-<YYYY-MM-DD>.md`
  where <YYYY-MM-DD> is today's date and <TICKET> is the ticket ID (e.g. SVR-005).
  Echo the absolute path back in your handoff. The MASTER writes the manifest
  file from your `VERIFICATION_ITEMS:` body — per worker_preamble v6 you MUST
  NOT write the manifest file yourself. The convention path matters: the
  master uses it to verify your echoed path and the file location stays
  predictable across cycles.

Read the plan doc yourself as needed. Do NOT expect plan content in this prompt.

Your deliverable: implementation per the plan, committed to a feature branch in
a git worktree (do NOT push — the master pushes after Jake's manual-verification
sign-off).

**Verification items emit (REQUIRED):** worker_preamble.md mandates you emit a
`VERIFICATION_ITEMS:` block in your handoff body using the three-bucket rubric.
The master parses this block and writes the manifest file atomically. Each
bucket is required:

- `### Automated` — `cargo test`/`cargo build`/`cargo clippy`/`grep`/`sqlite3`/etc.
  Each item: `- [x] <description>` followed by `check: <command>` and
  `expected: <outcome>` lines (sub-lines are NOT mandatory for Automated but
  STRONGLY encouraged so the manifest is independently re-runnable).
- `### Daemon-level` — daemon-side observable behavior verified via `rsi-rpc`
  against an isolated daemon (RSI_DAEMON_SOCKET_PATH=/tmp/<ticket>.sock).
  EVERY daemon-level item MUST include `check:` AND `expected:` sub-lines —
  the manifest validator REJECTS items missing either. If your work has no
  daemon surface, emit exactly one line: `- (none - <one-sentence reason>)`.
- `### TUI manual` — Jake-eyeballed visual/keyboard items. Each item: `- [ ]
  <description>` followed by an indented Markdown bullet block of sub-details.
  If your work has no TUI surface change, emit: `- (none - <one-sentence reason>)`.

Phase headings inside VERIFICATION_ITEMS use the format `## Phase N - <title>`
followed by `Status: sealed` and one `Commit: \`<sha>\` - <commit subject>`
line per phase commit.

After implementation is complete and committed, your FINAL MESSAGE TO THE MASTER
MUST CONTAIN ONLY the block below — first non-blank line MUST be the header
(validator scans the first 200 bytes; small preambles MIGHT slip past but
worker_preamble v6 forbids them, full stop). The validator accepts
`Implementation document:`, `impl_doc:`, or `doc_path:` for the artifact path.

PIPELINE HANDOFF — IMPLEMENTATION:
===================================
Implementation document: <absolute path to plan doc>
Manifest path: <absolute path you computed via the convention above>
Branch: <exact branch name>
Worktree: <absolute path to worktree>
Status: complete | partial | blocked
Phases:
  - Phase 1 <title>: COMPLETED | FAILED | SKIPPED
  - Phase 2 <title>: COMPLETED | FAILED | SKIPPED
Automated checks:
  - cargo build --workspace: PASS | FAIL
  - cargo test --workspace: PASS | FAIL
  - cargo clippy --workspace: PASS | FAIL
Files modified: <count>
Commits: <count>
Outstanding issues: <≤30 words, or "none">
Blocker: <≤30 words, omit if status=complete>

VERIFICATION_ITEMS:

## Phase 1 - <title>

Status: sealed
Commit: `<sha>` - <commit subject from git log>

### Automated
- [x] <item 1>
  check: <command>
  expected: <outcome>
- [x] <item 2>
  check: <command>
  expected: <outcome>

### Daemon-level
- (none - <reason>)  OR concrete items with `check:` + `expected:` sub-lines

### TUI manual
- [ ] <item 1>
  - <sub-detail>
  - <sub-detail>
```

Wait for this agent to complete. Validate the worker's reply via the same
binary used in Steps 1 and 2 (`cargo run -q -p rsi-common --bin
rsi-contract-validate -- RSI-XXX < reply.txt`). On non-zero exit, follow the
shared **Validator failure handling** procedure. On exit 0, parse the
structured `PipelineHandoff` JSON for {stage, doc_path, manifest_path, status,
blocker?} and run the shared **Post-stage scope audit**.

**Closure-tagged implementation override:** when the implementation handoff is
bound to a Closure `program_id` and `source_id`, do not write or commit the
ordinary V1 manifest in the source worktree. Seal the clean implementation HEAD
and record the source ref and source worktree HEAD at that exact SHA. Dispatch
an independent review session in a distinct evidence sandbox/branch allocated
from that SHA, passing the program/source IDs, sealed SHA, reviewer session ID,
current model invocation ID, and persisted review-policy digest. The reviewer
must follow `_shared/worker_preamble.md`'s Closure exception: write only the
canonical strict review JSON and head-bound Manifest V2, then run
`scripts/seal-closure-review-evidence.sh`, which checks reviewer correlation,
executes `rsi-closure-evidence-validate`, commits only those files, and proves
the source ref/worktree unchanged. The reviewer returns the strict
`PIPELINE HANDOFF — REVIEW:` fields (`reviewer_session_id`,
`reviewer_model_invocation_id`, `review_json_path`, `manifest_v2_path`,
`sealed_source_sha`, `evidence_commit_sha`) plus the canonical Stage contract.
Validate that handoff, the one-parent/two-path evidence commit, and the artifacts
with the same parser. Re-resolve the source ref and source worktree HEAD and
halt unless they remain the sealed SHA while only the evidence ref advances.
Use the committed V2 path for later verification/import. Non-Closure execution
continues through the V1 manifest authoring procedure below unchanged.

**Manifest authoring (non-Closure only; run immediately after the contract validator exits 0 — before Step 3.5):**

The worker emitted a `VERIFICATION_ITEMS:` block in their handoff body. The master writes the manifest file from that block atomically — workers may NOT write the manifest themselves (worker_preamble v6 rule).

1. Confirm the worker's echoed `Manifest path` matches the convention `<worktree>/thoughts/shared/verification/<TICKET>-<YYYY-MM-DD>.md`. If not, log the discrepancy and use the convention path — never trust an off-convention path.
2. Parse the `VERIFICATION_ITEMS:` block. It contains one or more `## Phase N - <title>` sections, each with `### Automated`, `### Daemon-level`, `### TUI manual` subsections.
3. Compose the manifest file. Required frontmatter (per `rsi-manifest-validate` schema v1):
   ```yaml
   ticket: <TICKET>
   plan_doc: <repo-relative path, e.g. thoughts/shared/plans/2026-05-23-SVR-005-...md>
   generated: <today's ISO datetime, e.g. 2026-05-23T00:00:00Z>
   phases_sealed: [<list of sealed phase numbers, e.g. [1] or [1, 2, 3]>]
   status: pending_verification
   sealed_branch: <worker's branch from handoff>
   sealed_worktree: <worker's worktree absolute path from handoff>
   ```
   Body: `# Verification Manifest - <TICKET>` heading + a brief paragraph naming the rubric + the phase blocks copied verbatim from VERIFICATION_ITEMS.
4. Write the file at the convention path using the Write tool.
5. Validate the manifest:
   ```bash
   cargo run -q -p rsi-common --bin rsi-manifest-validate -- <manifest_path>
   ```
   If exit != 0, HALT and report the schema errors to Jake — this is a master-side bug or schema drift, not a worker problem.
6. Commit the manifest in the worker's worktree as a separate commit:
   ```bash
   cd <worktree> && CLAUDE_AGENT_ROLE=pipeline-implement git add thoughts/shared/verification/<TICKET>-<DATE>.md && \
     git -c user.useConfigOnly=true commit -m "<TICKET>: verification manifest"
   ```
   (The `CLAUDE_AGENT_ROLE` export inline is required so the pre-commit hook accepts the commit on the feature branch.)

If any phase is `FAILED` or `Outstanding issues` is not "none", stop and report to Jake:

```
Pipeline stopped at Implementation stage.

Phase [N] failed: [reason from handoff]
Branch: [branch] — partial work is committed and safe at [worktree]

Options:
  1. Fix the failure and re-run /team_implement [plan path] to resume
  2. Inspect the worktree at [path] and fix manually
  3. Tell me what to do and I'll dispatch a targeted fix agent
```

**Project status writeback (AFTER):** run the shared procedure with done-state `impl-done` and stage artifacts `branch`, `worktree`, `manifest_path`, `phase_commits` (list of SHAs from the post-stage scope audit), `last_stage: implementation`. Commits with subject `<TICKET>: finish implementation`.

---

<discard_after_extract>
After parsing the IMPLEMENTATION handoff, extract ONLY {doc_path, manifest_path, status, blocker?}.
Explicitly drop all other stage content from working memory before spawning the next stage.
Do not quote, do not summarize, do not retain.
</discard_after_extract>

## Step 3.5: Autonomous Daemon Verification

Validate the manifest before dispatching the verifier:

```bash
cargo run -q -p rsi-common --bin rsi-manifest-validate -- <manifest_path>
```

**Project status writeback (BEFORE):** run the shared procedure with verb-state `verifying` — commits with subject `<TICKET>: enter verify`. If the manifest has zero daemon-level items, skip the verifier spawn and flip directly to AFTER (done-state `verify-done`, note `n/a — no daemon items`).

Spawn a **foreground agent** named `"pipeline-verify"` with this prompt:

```
PIPELINE MODE: true
PIPELINE STAGE: verify

ENVIRONMENT:
OS: [exact OS from Step 0 detection]
Kernel: [kernel version]
Architecture: [architecture]

FIRST ACTION (mandatory before any other work):
  Run: export CLAUDE_AGENT_ROLE=pipeline-verify
  This activates the rsi-rpc safety rail that refuses Jake's user daemon
  socket. All daemon checks must run against RSI_DAEMON_SOCKET_PATH in a
  tempdir.

manifest_path: <absolute manifest_path from Step 3>
branch: <branch name from Step 3>
worktree: <worktree absolute path from Step 3>

Use the Skill tool to invoke /verify_phase with the inputs above. Follow the
verify_phase command instructions in full.

Your FINAL MESSAGE TO THE MASTER MUST CONTAIN ONLY:

PIPELINE HANDOFF — VERIFY:
=============================
Manifest: <absolute path>
Daemon checks: <PASS_count>/<total_count>
Status: complete | blocked
Failed checks: [comma-separated item titles, omit if status=complete]
Blocker: [≤30 words, omit if status=complete]
```

Wait for the verifier. Validate its reply:

```bash
cargo run -q -p rsi-common --bin rsi-contract-validate -- RSI-XXX < reply.txt
```

On non-zero exit, follow the shared **Validator failure handling** procedure.
On exit 0, parse {stage, manifest_path, daemon_checks_passed, total, failed,
status, blocker?}.

If `status: blocked`, HALT and surface the failed daemon checks to Jake. Do not
enter the human verification gate until daemon-level items are resolved.

If `status: complete`, proceed to Step 4.

**Project status writeback (AFTER):** run the shared procedure with done-state `verify-done` and stage artifacts `daemon_checks_passed`, `daemon_checks_total`, `last_stage: verify`. Commits with subject `<TICKET>: finish verify`.

## Step 4: Human Verification Gate

**Project status writeback (BEFORE):** run the shared procedure with verb-state `awaiting-jake` — commits with subject `<TICKET>: enter human verification gate`. This is the ONLY stage whose verb-state can persist for hours/days, so this writeback is doubly important: it lets a fresh session pick up the ticket at the gate without having to scrape session transcripts.

This is a hard pause — the pipeline yields to Jake and stops actively doing anything. Make the paused state OBVIOUS rather than silent. Execute these sub-steps in order before rendering the verification checklist:

1. **PushNotification** — fire a notification with title `RSI: pipeline paused` and body `<ticket-id> awaiting manual TUI verification.` Jake may not be in the terminal when the gate hits.
2. **TaskUpdate** — flip the active stage task subject to `WAITING ON JAKE: <ticket-id> verification` so the task panel reflects the blocker state instead of an in-progress spinner.
3. Render the verification block below with the banner intact — do NOT remove the banner thinking it's decorative; it's the visual signal Jake greps for in scrollback.

Read the manifest from disk and present ONLY pending items under `### TUI manual`
sections to Jake. Do not show daemon-level items here; Step 3.5 already handled
them. Do not show automated items here; Step 3 already handled them.

```
████████████████████████████████████████████████████
PIPELINE PAUSED — YOUR DECISION REQUIRED
████████████████████████████████████████████████████

Ticket: <ticket-id>
Branch: <branch>  (local-only, not pushed)
Worktree: <path>

Automated checks: PASS
  cargo test --workspace: PASS
  cargo clippy --workspace: PASS

Daemon checks: PASS
  <PASS_count>/<total_count> verified by pipeline-verify

Please verify the following TUI manual items from <manifest_path>:

Phase [N]: [title]
  - [ ] [manual step]
  - [ ] [manual step]

Phase [M]: [title]
  - [ ] [manual step]

Reply "push" to publish the branch, "reject" to dispatch a fix agent,
or "defer" to leave the branch local for offline inspection.
```

Wait for Jake's confirmation. Do NOT proceed until confirmed. While waiting, do nothing — no polling, no opportunistic re-reads, no "let me also check…". The gate is the gate.

---

## Step 5: Final Push

After verification confirmation, push the feature branch:

```bash
cd [worktree path] && git push -u origin HEAD
```

Do NOT merge into main. The branch stays as-is for review.

---

## Step 6: Final Report to Jake

Do NOT trust in-context memory of stage outputs. Re-read the three doc paths from disk (research doc, plan doc, implementation artifacts). They are the source of truth. Your final report synthesizes from those files, not from prompt-history recall.

**Read-back evidence (required):** the report MUST include a `Sources re-read` block at the top listing each doc path with its current line count from disk (use `wc -l`). This is the mechanical observable that proves `<discard_after_extract>` was respected and the report is built from source-of-truth files, not stale prompt-history. If a path no longer exists or the line count is 0, that's a finding — surface it instead of fabricating content.

Deliver a complete pipeline summary:

```
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
Pipeline Complete
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

Goal: [original goal]

Sources re-read:
  • Research doc: <path> (<N> lines)
  • Plan doc:     <path> (<N> lines)
  • Impl commit:  <branch>@<sha> (<files> files, <insertions>+/<deletions>-)

━━━ Stage 1: Research ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
Document:  [research doc path]
Areas:     [codebase areas covered]
Findings:
  • [finding 1]
  • [finding 2]
  • [finding 3]
Open questions: [resolved / outstanding]

━━━ Stage 2: Plan ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
Document:  [plan doc path]
Phases:    [N]
  [Phase 1: title — description]
  [Phase 2: title — description]
  [...]
Design decisions:
  • [decision 1 — expert lens]
  • [decision 2 — expert lens]

━━━ Stage 3: Implementation ━━━━━━━━━━━━━━━━━━━━━━━
Branch:    [branch name]
Phases:    [N/N completed]
Workers:   [count used]
Files:     [count modified]
Checks:    all pass
Verified:  confirmed by Jake

━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
Ready to merge: branch [branch-name]
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
```

---

## Step 6.5: Completion Writeback

If the input was a free-text goal (no ticket file), skip this step.

Otherwise, after delivering the final report, run the shared **Project status writeback** procedure one last time with done-state `complete` and stage artifacts `completed: <today's ISO date>`, `last_stage: verified`. The render script propagates this into the index Status column automatically. Commits with subject `<TICKET>: complete`.

If a PR was opened or merged during Step 5, also write `pr_url: <url>` to the ticket frontmatter in the same commit.

Also update the verification manifest before the ticket writeback:

1. Mark every confirmed pending TUI manual item under `### TUI manual` as
   checked.
2. Flip manifest frontmatter `status: tui_only_pending` to `verified`.
3. Run `cargo run -q -p rsi-common --bin rsi-manifest-validate -- <manifest_path>`.
4. If validation fails, halt the completion writeback and surface the manifest
   error to Jake.

This closes the reconciliation loop: the next /master_implement invocation on the same ticket will see `status: done` in Step 0.5 and immediately surface the completed state rather than re-running anything. Without this writeback, the loop is open and the same ticket can silently round-trip the pipeline.

If the writeback fails, log the intended write and surface it to Jake with a one-line manual-apply instruction.

---

## Failure Handling Reference

| Stage | Failure type | Master action |
|---|---|---|
| 0.5 State Reconciliation | Ticket non-virgin (status ≠ ready, or artifacts found on disk/git) | Halt, present discovered state + drift, offer resume / re-run / skip-to-verification / abort |
| 0.5 State Reconciliation | Frontmatter ↔ filesystem drift | Surface drift in the options report; never silently overwrite |
| Research | Agent errors or returns blocked | Report to Jake, offer narrowed retry |
| Research | Open questions block planning | Spawn targeted follow-up researcher, continue |
| Planning | Worker blocked | Surface to Jake with options from team_plan |
| Planning | Plan has unresolved questions | Master resolves via codebase research before continuing |
| Implementation | Phase FAILED | Surface to Jake with recovery options |
| Implementation | Worker BLOCKED | Surface to Jake, wait for guidance |
| 1 / 2 / 3 / 3.5 / 4 / 6.5 | Project status writeback fails (ticket file unwritable, frontmatter malformed) | Log the intended write verbatim, continue pipeline, surface in Step 6 final report for manual apply |
| 1 / 2 / 3 / 3.5 / 4 / 6.5 | `render-project-index.sh` exits non-zero (script bug, missing project dir) | Commit ticket-file change anyway (canonical source); surface script error to Jake; index will be stale until re-rendered |
| Any | Unexpected error | Stop, report full context, preserve partial work |

Never silently swallow a failure — every block surfaces to Jake immediately with the branch and worktree path so no work is ever lost.
