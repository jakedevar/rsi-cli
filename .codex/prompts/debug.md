---
description: Investigate a problem through rsi logs, the database, and git; fix it in isolation when needed
---

# Debug

Use this when something breaks during manual testing or implementation. Investigate through the rsi logs, the database, and git history. If a fix is required, all editing must happen in an **isolated worktree / sandbox**, never directly on `main`.

`/debug` has exactly two modes, nothing in between:

- **Mode A — Investigate.** Read-only: logs, DB, git state, root-cause synthesis. Ends in a Debug Report.
- **Mode B — Fix.** Only entered when a code change is required. Adds the MANDATORY Workflow Rules and Fix-Path Gates below.

`/debug` inherits dispatch, capability-class, handoff, and verification discipline from `master_orchestrate.md` and `worker_preamble.md` (v10) but stays one bounded command, never a multi-stage conveyor. If a fix outgrows Mode B, **Escalation** (below) hands off to `/master_orchestrate` instead of `/debug` quietly turning into one.

## MANDATORY Workflow Rules

These rules are **non-negotiable** for every `/debug` invocation that produces a code change:

### Preflight Gate

Before Rule 1 (creating the worktree), and before touching any file for the fix, run:

```bash
git -C <repo> status --short
git -C <repo> rev-parse HEAD
git -C <repo> branch --show-current
```

If status is non-empty, **HALT** and report the dirty files to the user. Do not create a worktree or edit anything until either the tree is clean, or the user explicitly declares the dirty files user-owned and compatible with this fix (master_orchestrate.md §Harness And Input Audit). This is separate from the `~/.rsi` runtime **Preflight checklist** under Environment Information below — that one confirms daemon/TUI artifacts exist; this one confirms the git tree is safe to mutate.

1. **Isolated worktree.** Before editing any file, create or enter a dedicated git worktree (or RSI sandbox) so the working copy never overlaps with the main checkout. For example:
   ```bash
   git worktree add -b debug/<short-slug> ../<short-slug> origin/main
   cd ../<short-slug>
   ```
   All subsequent edits, builds, and tests run inside that worktree. **Never edit files in the main checkout for a `/debug` fix.** Worktrees for this project live under `.claude/worktrees/<slug>/` per repo convention — adapt the path accordingly.
2. **Branch off `origin/main`.** The branch name should be `debug/<short-slug>` (e.g. `debug/modal-overflow`). Never reuse a stale branch.
3. **Commit and push are MANDATORY.** Complete the **Fix-Path Gates** below first. Then, before declaring the debug task complete, you MUST:
   - Verify the build is green (`cargo build --workspace`).
   - Run any new or affected tests (`cargo test -p <crate>` for the relevant crate; `cargo clippy --workspace` for lints).
   - Stage **only** the files relevant to the fix (no `git add -A` — accidental inclusion of secrets / generated files is the most common regression vector).
   - If this debug run created or modified files under `thoughts/`, stage those task-owned paths and include them in the commit. Never leave a newly created thoughts artifact untracked or dirty; do not absorb unrelated user edits.
   - Commit with a descriptive message that explains the **why**, not just the **what**.
   - If the fix touches keybindings, RPC surface, schema, config, or other user-facing behavior, update the matching docs (e.g. `docs/keybindings.md`) in the same commit — otherwise write `documentation: none required - <reason>` into the Debug Report's Closure section (Step 3).
   - Push the branch to `origin` (`git push -u origin <branch>`).
   - Report the commit SHA and pushed branch back to the user.

   **Push-policy divergence (explicit).** This push-on-completion mandate is `/debug`'s own top-level, user-facing behavior: it applies because the user invoked `/debug` directly, and only to the feature branch (never `main` — Rule 4). It is NOT a general worker policy. Any worker `/debug` dispatches (Step 2 investigation sub-tasks, the Fix-Path Gates independent review pass) MUST NOT push — per `worker_preamble.md` §Push policy (HARD), a dispatched worker commits locally only. Pushing stays owned by the top-level session the user is talking to.
4. **No silent pushes to `main`.** The user reviews via PR or fast-forward at their discretion. If the user explicitly says "land on main", only then merge and push to `main` — never silently.
5. **Cleanup.** When the worktree is no longer needed, prune it with `git worktree remove ../<short-slug>` and let the user know.

If the user objects to the worktree workflow for a particular session, ask once and proceed only after explicit confirmation.

### Fix-Path Gates

After the fix lands in the worktree, and before Rule 3's commit checklist:

(a) **Self-check.** Re-read the diff against the root cause identified in Step 3 — confirm it addresses the cause, not a symptom. Note this check in the Debug Report's Closure section.

(b) **Independent review.** Mandatory for any fix touching more than one file, or any logic change beyond a one-line guard/condition. Dispatch a review pass at `[class: architect, model=opus, effort=xhigh]` (master_orchestrate.md §Worker Assignment And Return; alias table in `.claude/commands/_shared/master_orchestrate_rsi_overlay.md`) through the Step 2 dispatch order, using its compact `PIPELINE HANDOFF — REVIEW:` contract. Single-line, single-file, mechanical fixes may skip straight to (d).

(c) **Delta re-review.** After review fixes, use the remaining review round once at the same `[class: architect, model=opus, effort=xhigh]`. Scope it to finding dispositions, changed hunks, and affected risk under master_orchestrate.md §Default Review Budget. Unresolved blockers still prevent acceptance when the budget is exhausted.

(d) **Verification bucketing.** Classify every verification item with `worker_preamble.md` §Verification item categorization: **AUTOMATED** (`#[test]`/`#[tokio::test]` — mandatory whenever the behavior is expressible as one), **TUI MANUAL** (fallback only; justify why it can't be automated), or **DAEMON AUTONOMOUS** (concrete `check:`/`expected:` command). A manual item for behavior that could be a test is a failed fix.

The independent-review handoff (b/c) emits a `## Stage contract` block — `### Inputs` / `### Process` / `### Outputs` / `### Verify`, with Inputs declaring named files or a discovery budget (`worker_preamble.md` §Stage contract block). Validate it with `rsi-contract-validate` when the binary is built (`cargo build -p rsi-common --bin rsi-contract-validate`); if it isn't available in this session, say so in the Debug Report instead of skipping the block.

## Escalation

`/debug` is a single bounded session, never a multi-stage conveyor. Check this before entering Mode B, and again if scope grows mid-fix.

If the fix is, or becomes, multi-file across more than one crate, high blast-radius, or touches a dangerous gate (schema migration, security boundary, live-execution reachability — RSI overlay conflict-domain vocabulary: `schema-migration`, `security-boundary`, `live-execution`), **STOP**:

1. Do not keep editing inline.
2. Compile the exact `/master_orchestrate <slice>` invocation prompt for the now-scoped slice — goal, repo, explicit constraints, expected artifacts, forbidden changes (master_orchestrate.md §Invocation).
3. Present the compiled prompt to the user and stop.

`/debug` must never silently expand into an orchestration run under its own steam — that decision belongs to the user.

## Initial Response

With a plan or ticket file:
```
Debugging against [file name]. Tell me:
- what you were testing or building
- what happened instead (exact error text if any)
- when it last worked

Then I'll check the logs, the database, and git state.
```

With no parameters:
```
What broke? Give me:
- what you were working on
- the symptom (exact error text if any)
- when it last worked

I'll dig through the logs, the database, and recent changes from there.
```

## Environment Information

Rsi keeps all runtime artifacts in `~/.rsi/` by default. Confirm each path exists before digging in.

| Resource | Default Location | Notes |
| --- | --- | --- |
| Rsid logs | `~/.rsi/logs/rsid-*.log` | Created by `cargo run --bin rsid` or `./scripts/dev-daemon.sh` |
| Rsi TUI logs | `~/.rsi/logs/rsi-*.log` | Created by `cargo run --bin rsi` or `./scripts/dev-tui.sh` |
| SQLite database | `~/.rsi/rsi.db` | Stores sessions, events, approvals, turn metrics |
| Unix socket | `~/.rsi/daemon.sock` | JSON-RPC endpoint between TUI and daemon |
| Persisted state | `~/.rsi/tui-state.json` | Long-lived TUI state |
| Dev-only state | `~/.rsi/dev-state.json` | Cleared on reload; scroll positions, drafts |

Preflight checklist:
1. `ls ~/.rsi` → ensure directory exists
2. `ls ~/.rsi/daemon.sock` → verify daemon is running
3. `ls ~/.rsi/rsi.db` → confirm DB present
4. `ls ~/.rsi/logs` → ensure logs are being written

If any file is missing, restart the daemon (`cargo run --bin rsid` or `./scripts/dev-daemon.sh`) and TUI (`cargo run --bin rsi` or `./scripts/dev-tui.sh`).

## Process Steps

### Step 1: Pin Down the Symptom

Once the user has described it:

1. **Read the context they gave** (plan or ticket):
   - the phase or step they were on
   - expected behaviour versus what they saw
2. **Snapshot the repo**:
   - branch, last few commits, uncommitted changes
   - when the symptom first appeared

### Step 2: Gather Evidence

Dispatch the three sub-tasks below in parallel, through the first available surface, in this order (AGENTS.md §Agent control via `rsi-rpc`; master_orchestrate.md §Harness And Input Audit):

1. **`rsi_control_*` native tools** — `rsi_control_spawn` / `rsi_control_status` / `rsi_control_halt`, when present in this session's tool set.
2. **`rsi-rpc` `Agent*` verbs** — `AgentSpawnChild` / `AgentGetStatus` / `AgentHalt` / `AgentContinueChild` / `AgentScheduleWake` via the `rsi-rpc` CLI. Authority rides `$RSI_SESSION_TOKEN` (transport-only — never in `--params`).
3. **`<docregblock>/spawn_child …</docregblock>` directive fallback** — only when neither (1) nor (2) is reachable.
4. **Prompt-compiler fallback** — if no RSI child-spawn surface exists at all, compile the three sub-task prompts below, present them to the user verbatim, and stop. Do not claim a worker ran.

Outside a managed RSI backend, provider-native subagent/task tools are only ordinary harness-local workers — never describe them as equivalent to RSI's session/authority/wake/halt model (master_orchestrate.md §Layering And Conditional References).

Each sub-task is mechanical lookup — dispatch class `lookup_fast` (master_orchestrate.md §Worker Assignment And Return; RSI overlay alias table):

**Sub-task 1 — Recent Logs** `[class: lookup_fast, model=haiku]`
```
Scan the newest rsi logs for the failure:
1. Newest daemon log: ls -t ~/.rsi/logs/rsid-*.log | head -1
2. Newest TUI log: ls -t ~/.rsi/logs/rsi-*.log | head -1
3. Grep ERROR, WARN, and panics in the window around the reported time
4. Record the working directory from the log's first line
5. Flag stack traces and errors that repeat
```

**Sub-task 2 — Database State** `[class: lookup_fast, model=haiku]`
```
Inspect ~/.rsi/rsi.db read-only (sqlite3 -readonly):
1. List tables (.tables) and read the schema of the ones involved (.schema <table>)
2. Newest sessions: SELECT id, status, provider, created_at FROM sessions ORDER BY created_at DESC LIMIT 5;
3. Last hour of events: SELECT session_id, event_type, created_at FROM conversation_events WHERE created_at > strftime('%Y-%m-%dT%H:%M:%S', 'now', '-1 hour') ORDER BY id DESC LIMIT 50;
4. Add queries specific to the symptom
5. Flag rows stuck in one status, orphaned references, or impossible combinations
```

**Sub-task 3 — Git and File State** `[class: lookup_fast, model=haiku]`
```
Establish what changed:
1. Branch and status: git branch --show-current; git status --short
2. Recent history: git log --oneline -10
3. Uncommitted edits: git diff --stat, then git diff on the suspicious files
4. Confirm the files named by the plan or the error exist
5. Note permission or ownership problems on those files
```

Each sub-task's FINAL message returns only this compact handoff — no file contents, no code snippets, no narrative prose (`worker_preamble.md` §Forbidden content in returns). This mirrors master_orchestrate.md's handoff shape, but "investigate" is not one of its pipeline `stage` values, so it is not piped through `rsi-contract-validate`:

```text
DEBUG HANDOFF — INVESTIGATE:
Stage: logs | db | git
Status: complete | partial | blocked
Findings: <=5 items, each a one-line fact or `file:line` / `table:row` ref
Blocker: <=30 words, omit if none
capability_class: lookup_fast
```

The master (this session) keeps only those fields and drops everything else; re-read the underlying log/DB/git source directly if more detail is needed.

### Step 3: Present Findings

Synthesize the three handoffs into a root cause yourself, at this session's own `implementer` class (frontmatter pins it to `model=sonnet, effort=high` — no extra dispatch needed for ordinary synthesis). If the root cause is cross-subsystem or architectural (spans `rsi`/`rsid`/`rsi-common` boundaries, touches shared protocol/storage, or needs a design call), that is a capability-class escalation, not a scope Escalation: dispatch a synthesis pass at `[class: architect, model=opus, effort=xhigh]` through the Step 2 dispatch order rather than reasoning it out inline. (Compare: the **Escalation** section under MANDATORY Workflow Rules is about the FIX outgrowing `/debug` entirely — a different decision.)

Then write the Debug Report, filling every section with concrete evidence:

````markdown
## Debug Report

### What's Wrong
[The failure, stated from the evidence]

### Evidence Found

**From Logs** (`~/.rsi/logs/`):
- [timestamped error or warning]
- [recurring pattern]

**From Database**:
```sql
-- query and result
[finding]
```

**From Git/Files**:
- [recent change that could be involved]
- [file-state problem]

### Root Cause
[Best-supported explanation, tagged [observed] or [inferred]]

### Next Steps

1. **First try**:
   ```bash
   [Specific command or action]
   ```

2. **If that fails**:
   - Restart services: `cargo run --bin rsid` and `cargo run --bin rsi`
   - Check for stale sockets in `~/.rsi/`
   - Run with verbose logging: `RUST_LOG=debug cargo run --bin rsid`

### Closure
_(Mode B / fix path only — omit this section entirely for investigate-only reports)_

Verification:
- [command]: PASS | FAIL | NOT RUN ([reason])
- (one line per command actually run: build, tests, clippy, review pass)

Repo state: clean | dirty-uncommitted | dirty-user-owned
Commit: [sha] on branch [branch]
Documentation: [paths touched] | none required - [reason]
Next allowed work: [what the user or a follow-up session may do next]

### Out of Reach
Not visible from this session:
- browser console output
- MCP server internals
- OS-level problems

[Name the one thing to check next, or the evidence you need from the user.]
````

## Important Notes

- **Built for breakages found while testing or implementing**: the symptom comes from a live run, not a code review.
- **No symptom, no debugging**: get the description before investigating.
- **Read budget** - Grep-then-Read: locate ranges with `rg`/Grep first; full-file reads only for files under 400 lines; ≤8k tokens of file reads before acting (`worker_preamble.md` §Read budget) — supersedes any older "always read whole files" habit, which doesn't scale to this repo's larger files
- **Know the git state first**: branch, diff, and recent history come before any theory.
- **Hand back what you cannot see**: browser consoles and MCP internals belong to the user.
- **Investigation first, fixes second** - If investigation alone resolves the question, do not edit. If a code change is required, check **Escalation** first, then follow the **MANDATORY Workflow Rules** above (Preflight Gate, isolated worktree, Fix-Path Gates, then commit + push) — never edit the main checkout directly.

## Quick Reference

**Newest Logs**:
```bash
ls -t ~/.rsi/logs/rsid-*.log | head -1
ls -t ~/.rsi/logs/rsi-*.log | head -1
```

**Database (read-only)**:
```bash
sqlite3 -readonly ~/.rsi/rsi.db ".tables"
sqlite3 -readonly ~/.rsi/rsi.db ".schema sessions"
sqlite3 -readonly ~/.rsi/rsi.db "SELECT id, status, provider, created_at FROM sessions ORDER BY created_at DESC LIMIT 5;"
```

**Service Check**:
```bash
pgrep -a rsid    # daemon running?
pgrep -a -x rsi  # TUI running?
```

**Git State**:
```bash
git status --short
git log --oneline -10
git diff --stat
```

Keep the digging out of the main window's context: the sub-tasks read the logs, the database, and git so this session only sees their findings.
