---
description: Investigate a problem through rsi logs, the database, and git; fix it in isolation when needed
---

# Debug

Use this when something breaks during manual testing or implementation. Investigate through the rsi logs, the database, and git history. If a fix is required, make it in your assigned sandbox and land it on `rolling`.

`/debug` has exactly two modes, nothing in between:

- **Mode A — Investigate.** Read-only: logs, DB, git state, root-cause synthesis. Ends in a Debug Report.
- **Mode B — Fix.** Only entered when a code change is required. Adds the Fix Workflow below.

`/debug` inherits dispatch, capability-class, handoff, and verification discipline from `master_orchestrate.md` and `worker_preamble.md` (v10) but stays one bounded command, never a multi-stage conveyor. If a fix outgrows Mode B, see **Scope growth** below.

## Fix Workflow

Applies to every `/debug` invocation that produces a code change. Work from `rolling` conventions in `AGENTS.md`.

1. **Stay in your sandbox.** Edit, build and test only in the session's assigned worktree on its assigned branch. Do not create, switch or reset branches, and never edit the shared `~/rsi` checkout. Outside a managed sandbox, make a worktree off `origin/rolling`.
2. **Unrelated dirt is not a blocker.** Check `git status --short` first; leave edits you did not make untouched and stage only your own explicit paths. Stop only if a dirty file overlaps the file you must change and you cannot tell whose it is.
3. **Self-check.** Re-read the diff against the root cause from Step 3: it must address the cause, not a symptom. Note this in the Debug Report's Closure section.
4. **Review only where `AGENTS.md` requires it:** new schema migrations and credential/IAM/network-exposure changes get one plain reviewer pass before landing. Everything else lands after tests pass and gets at most one post-land review.
5. **Commit.** Stage only the files relevant to the fix (no `git add -A`), including task-owned `thoughts/` paths. Message explains the why. Update matching docs (e.g. `docs/keybindings.md`) in the same commit, otherwise write `documentation: none required - <reason>` in the Closure section.
6. **Verification.** After committing, submit one `scoped_test` job against `origin/rolling` through `AgentSubmitJob {kind:"test", wake:"none", params:{scoped_test:{base:"origin/rolling"}}}`. Arm one `AgentScheduleWake {mode:"when", message:"scoped_test finished: read the receipt", when:{jobs_terminal:["<job id>"]}}` and end the turn. Read the completed receipt on resume; do not report a pending run as passed. Classify items per `worker_preamble.md` §Verification item categorization: **AUTOMATED** (a test whenever the behavior is expressible as one), **TUI MANUAL** (justify why it can't be automated), or **DAEMON AUTONOMOUS** (concrete `check:`/`expected:`). A manual item for behavior that could be a test is a failed fix.
7. **Hand off for landing.** Push the verified commit on the assigned branch without force and confirm the remote SHA. Give the manager the SHA, branch and completed `scoped_test` receipt for the daemon merge queue. If your current authority permits enqueueing, use `AgentEnqueueLandingSource {derive_filters:true}` for the accepted SHA. Follow `AGENTS.md` §Landing; never invoke the lander directly. Never merge or push `main` without explicit operator consent.

### Scope growth

If the fix grows past one bounded repair (several crates, schema migration, security boundary, high blast radius), decide yourself: split it into landable pieces, or file an Issue / spawn a worker through available RSI authority (`AgentCreateIssue`, `AgentSpawnChild`) for the remainder, and keep landing the part you have. Do not hand an engineering prompt back to the operator; only real operator gates (`main` or releases, real spending, credentials, deleting user data) go up, through the manager.

## Initial Response

Use the supplied Issue, failing test and reproduction evidence first. Ask only for missing information needed to investigate.

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

An Issue-bound worker with a reproduced, exact failing test and enough evidence to identify the cause fixes it directly in the assigned sandbox, following the Fix Workflow. Use the existing reproduction and inspect the implicated code; no investigation fan-out is required.

When the cause is unknown, gather only the evidence relevant to the symptom. If child spawning is permitted by the current authority catalog, dispatch relevant investigations from the three sub-tasks below in parallel through the first available RSI surface, in this order (AGENTS.md §Agent control via `rsi-rpc`; master_orchestrate.md §Harness And Input Audit):

1. **`rsi_control_*` native tools** — `rsi_control_spawn` / `rsi_control_status` / `rsi_control_halt`, when present in this session's tool set.
2. **`rsi-rpc` `Agent*` verbs** — `AgentSpawnChild` / `AgentGetStatus` / `AgentHalt` / `AgentContinueChild` / `AgentScheduleWake` via the `rsi-rpc` CLI. Authority rides `$RSI_SESSION_TOKEN` (transport-only — never in `--params`).
3. **`<docregblock>/spawn_child …</docregblock>` directive fallback** — only when neither (1) nor (2) is reachable.

If spawning is unavailable or not permitted, perform the relevant lookups directly and continue. Do not claim a worker ran. Record any evidence you cannot access in the Debug Report.

Outside a managed RSI backend, provider-native subagent/task tools are only ordinary harness-local workers — never describe them as equivalent to RSI's session/authority/wake/halt model (master_orchestrate.md §Layering And Conditional References).

For dispatched mechanical lookups, use class `lookup_fast` (master_orchestrate.md §Worker Assignment And Return; RSI overlay alias table):

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

Synthesize the evidence collected directly or returned by dispatched investigations into a root cause. If the root cause is cross-subsystem or architectural (spans `rsi`/`rsid`/`rsi-common` boundaries, touches shared protocol/storage, or needs a design call), request a synthesis pass at `[class: architect, model=opus, effort=xhigh]` through the Step 2 dispatch order when spawning is permitted. Otherwise reason from the available evidence and record remaining uncertainty. See **Scope growth** if the fix exceeds one bounded repair.

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
- **Investigation first, fixes second** - If investigation alone resolves the question, do not edit. If a code change is required, follow the **Fix Workflow** above (sandbox, self-check, tests, commit, land) — never edit the shared checkout directly.

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

When investigations are dispatched, keep their returns compact; otherwise inspect the relevant sources directly.
