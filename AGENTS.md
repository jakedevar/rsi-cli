# AGENTS.md

Canonical repo instructions for AI coding agents. Tool-specific files such as `CLAUDE.md` should point here or mirror this file exactly.

## What This Is

`rsi` is a vim-like TUI for managing multiple AI coding sessions across providers.

Crates:
- `rsi` — TUI client using ratatui, crossterm, and modalkit.
- `rsid` — daemon that spawns and manages provider CLI subprocesses.
- `rsi-common` — shared types, JSON-RPC protocol, serde contracts, validators.

The TUI talks to the daemon over a Unix socket at `~/.rsi/daemon.sock` using JSON-RPC 2.0. SQLite persistence lives at `~/.rsi/rsi.db`.

Providers are selected through `Session.provider`: `Claude`, `Codex`, `Pioneer` (Pioneer inference through Codex CLI), `Local` (Ollama/OpenAI compatible), `Antigravity` (Gemini alias, fully supported), `CodexAppServer`, or `Harness` (direct API).

## Build And Dev

```bash
make release-install
./scripts/dev-daemon.sh
./scripts/dev-tui.sh
cargo bench-all
make test-fast
make test-full
```

The daemon must be running before the TUI connects.

Useful validators and helpers:

```bash
./tools/install-hooks.sh
./scripts/check-identity-assertions.sh
```

`check-identity-assertions.sh` is a hard CI gate and a pre-commit check. It
rejects a test that requires an entity's OWN name to be invisible (asserting a
string that the same file assigns to a `title`/`name`/`query` field is absent
from rendered output). Asserting that chrome, key hints, or leaked values are
absent is legitimate and is not flagged.

Rust is pinned by `rust-toolchain.toml`. Use the pinned toolchain when building, testing, formatting, or linting.

## Critical Type Rules

- `Session.working_dir` is required. Use a fallback; do not make it optional.
- `Session.provider` determines which provider subprocess is spawned.
- `SessionStatus` variants are `Starting`, `Running`, `WaitingApproval`, `Completed`, `Failed`, `Interrupted`, `Archived`, `Deleted`.
- There is no `SessionStatus::Interrupting`; track interrupt-in-progress locally.
- `ConversationEvent.id` is `i64`; use `0` before persistence.
- `ConversationEvent.content` is `String`; use `""` when empty.
- `ConversationEvent.sequence` must be tracked per session.
- `BusEvent` is a struct with `event_type: String`, not an enum.
- `RpcRequest.params` defaults to `Value::Null`; do not use `.unwrap_or()` as if it were optional.
- `SessionKind::Group` and `SessionKind::Epic` are containers; they do not spawn provider subprocesses.
- Spawnable leaf check is `rsi_common::is_leaf_kind(kind)`.
- Legal hierarchy is defined by `rsi_common::legal_children(parent_kind)`.
- `Session.parent_id` is hierarchy; `continued_from` is rotation lineage. Do not conflate them.
- `Session.lead_session_id` is meaningful only for container kinds.
- `Session.title` remains raw/manual metadata. `Session.agent_role` and
  `Session.epic_spawn_ordinal` are nullable durable display identity; they are
  inherited across lineage and never derived from or written into the title.
- Epic ordinals are unique on logical spawn reservations, not session rows;
  lineage rows intentionally share one stored positive ordinal. The current
  lead's displayed ordinal `0` is virtual and never rewrites persisted data.
- `AgentSpawnChild.agent_role` is optional presentation metadata. Caller/Epic/
  parent/lead/ordinal authority stays transport-bound and absent from request
  JSON. `$CLAUDE_AGENT_ROLE` remains SessionKind-derived Git-hook policy and is
  not the display role.

## RPC Surface

The full method list is the match arms in `crates/rsid/src/rpc.rs`; the
non-obvious per-family contracts live in `crates/rsid/CLAUDE.md`. Operator-only
families (ProgramRun, source-worktree cohort settlement, harness-manager
appointment/policy/state/decision methods, generic Issue verbs,
`LinkIssueToIdea`, `ListIssueEvents`) must stay absent from `AGENT_VERBS`,
`READ_VERBS`, native provider tools, and the agent CLI catalog. The one
delegated path is `AgentManagerControl` `operator_call`: only the current
appointed manager whose operator-saved policy grants `OperatorDelegation`
(Execute, not paused) may invoke a method on the closed, versioned
`DELEGABLE_OPERATOR_METHODS` allowlist (v2: `ArchiveSession` logical-only,
`GetArchiveCleanupStatus`, `ListSessions`, `UnarchiveSession` logical restore),
bound to the manager's project. Each call is journaled with the manager as
actor and read back through `AgentManagerGetAction`. Appointment, scope,
policy and grant changes, human-gate answers, daemon config, launches or
continuations outside manager gates, deletion/purge, daemon-global custody,
and `main`/release stay undelegable (`NEVER_DELEGABLE`); every other caller
gains nothing. Write verbs (`AgentSendMessage`, `AgentManagerUpdate`,
`AgentManagerControl`) are never read-allowlisted.

### Agent control via `rsi-rpc`

An rsi-managed provider session drives the daemon through the closed `Agent*`
verbs (listed in `crates/rsid/CLAUDE.md`) and nothing else — every other
method is default-denied. Authority is enforced server-side against the caller
session resolved from
`$RSI_SESSION_TOKEN`, never from anything the agent supplies. The token is a
transport-only credential and must **NEVER** be typed into `--params`.

Scoping in one line: an Epic-lead may act on its Epic's children; any leaf may
act on itself and its own direct children; the current appointed manager with
the SessionControl grant may act on Epic leads and descendants in its live scope.

Full contract — verb-by-verb authority, token lifecycle and re-minting, native
in-process `rsi_control` tools, retry policy, and the spawn-directive fallback —
is the `rsi-agent-control` skill (`.claude/skills/rsi-agent-control/SKILL.md`).

## Database Rules

Prefer daemon RPC for normal session flows because it preserves status-machine, timestamp, UUID, and validation invariants.

Raw SQLite access is allowed for diagnostics, repairs, and bulk fixes when it is the right tool.

**The operator does not use raw SQLite for routine operation.** Raw SQL remains
legitimate for diagnostics, repairs, and bulk fixes as above, but it is NOT an
operator interface. Any knob, toggle, or setting intended for routine operator
use must ship an RPC/TUI/CLI surface **in the same change that introduces it** —
a value reachable only by hand-writing SQL against `~/.rsi/rsi.db` is not
shipped, it is inert. This applies in particular to new `daemon_settings` keys:
adding a key without wiring it into `GetDaemonConfig`/`UpdateDaemonConfig` (and
the TUI settings surface) is incomplete work. Operator-only settings must still
stay absent from `AGENT_VERBS`, `READ_VERBS`, native provider tools, and the
agent CLI catalog. See issue #35 for the outstanding audit.

Hard rules:
1. Never change schema without a versioned migration: an inline `if version < N` block in `crates/rsid/src/store/mod.rs` (see the V70 block for the pattern) plus a matching `user_version` bump in the same change. There is no separate `store/migrations/` directory. **Released migration DDL, catalog projections, and fingerprints are immutable. Never repair them in place; add a forward migration.** Mark migration-owned helper/catalog regions for the released-migration inventory and refresh it only to append the new version; CI rejects any changed existing pin relative to the base.
2. Never hard-delete rows without explicit user consent. Prefer logical delete, archive, or status transitions.
3. Timestamp values must be RFC3339 with nanosecond precision.
4. UUIDs must be lowercase canonical strings.
5. Enum strings must match serde variants exactly.
6. Sandbox tombstone updates must flip cleanup state and null sandbox path/branch atomically.

## Keybinding Changes

Keybinding sources and the end-to-end recipe for adding an action live in
`crates/rsi/CLAUDE.md`, which loads when you work in the TUI crate. Any
keybinding change must also update `docs/keybindings.md`: run `make manual`,
which regenerates its `rsi:generated` regions and `docs/manual/`.

## Design Philosophy

This is a single-user power tool for a vim-native user. Optimize for dense information, keyboard flow, fast feedback, and low interruption.

Prefer correct foundations over quick hacks. Do not create throwaway code when the correct abstraction is visible. Keep changes scoped, but do the complete job.

All completed feature and bug work must preserve existing behavior unless the user explicitly asks to change it.

### Do not let a defect defend itself

Two habits turn an ordinary mistake into one that survives review and blocks the
next agent sent to fix it. Both are binding on every agent and every pipeline
stage.

**Reconcile conflicting intent before implementing.** When a plan, ticket, or
spec states a constraint and a design decision that cannot both hold, that
contradiction is a blocker to resolve, not a judgment call to silently settle.
Name both statements, resolve them explicitly, and record the resolution. If it
cannot be resolved within your authority, stop and report the conflict. Picking
one side quietly ships the other side's violation as intended behavior.

**Never assert that user-visible information is absent.** A test asserting that
a name, title, label, or identifier does NOT appear on screen
(`!text.contains(...)`, `assert_not_visible`, a snapshot with it removed) pins
data loss as a requirement: the next agent to investigate finds a red test
telling them the bug is intended. Assert the positive end state instead. If
absence genuinely is the requirement, assert that the specific replacement is
present and name the test so the omission's rationale is explicit.

Precedent: a container row was changed to display its top child's title by
overwriting its own, against the same plan's stated constraint to preserve
container identity. A renderer test asserted the container's own name was
absent. Groups became unfindable in the session list -- one named
"Orchestration Agent Process Fix" rendered as "3".

## Seven-Expert Decision Framework

Use these lenses for non-trivial implementation, architecture, UI, agent orchestration, and spec decisions:

1. Software Engineer — clean Rust architecture, single responsibility, no duplicate logic.
2. Tech Wizard — zero-waste correctness; avoid work that will be thrown away.
3. UI/UX Power User — dense, keyboard-efficient, no hand-holding.
4. Systems Performance Engineer — bounded CPU/memory, event-driven where possible, efficient rendering.
5. Reliability Engineer — recoverable failures, visible errors, safe persistence.
6. Scalability Expert — bounded fan-out, backpressure, batching, stable latency as session counts grow.
7. Agentic Harness Architect — clear handoffs, context-budget awareness, validators, current model/tool capabilities.

If an approach violates one of these lenses, resolve the conflict before implementation. If presenting options to the user, include your recommendation and a brief reason.

## Recommendation-As-Default

The operator normally wants the agent to take the best supported path, not ask
for confirmation of the agent's own recommendation. When evidence, repository
policy, or a workflow selects an in-scope, non-destructive route, take it
automatically and state the decision briefly. In particular:

- Auto-route to the appropriate skill, team workflow, risk tier, or verification
  path when its selection criteria are met. Do not ask the operator to re-invoke
  it or approve the recommendation.
- Resolve ordinary technical ambiguity with the Seven-Expert Decision Framework,
  record consequential assumptions, and continue.
- Derive and apply ordinary safety gates, review loops, and bounded remediation
  without asking whether to proceed.
- Ask only when the choice cannot be resolved without materially new authority,
  changes the requested outcome or scope, or crosses an applicable destructive,
  production, external-impact, credential, privacy, or substantial-cost gate.
- An explicit operator choice always overrides this default when it remains
  within higher-priority safety and repository constraints.

## Agent Workflow

Read the code before changing it. Prefer existing patterns over new abstractions.

Use `rg` / `rg --files` for search. Keep edits narrow. Do not refactor unrelated code.

Do not overwrite user or parallel-agent changes. If the worktree is dirty, inspect relevant files and preserve unrelated changes.

**Never use `cargo fmt` to format a subset of files — `cargo fmt -- <paths>` does not scope to those paths.** Cargo hands rustfmt each crate's entry point and rustfmt walks the whole module tree, so the path arguments restrict nothing: passing a single `crates/rsi/` file reformats `crates/rsid/` files too. This idiom was recommended here previously and is the direct cause of at least one corrupted diff.

Format only the files you changed, by invoking `rustfmt` directly (no cargo):

```bash
git diff --name-only --diff-filter=d -- '*.rs' | xargs -r rustfmt --edition 2024
```

After any format, run `git status` and `git checkout -- <file>` for anything you did not intend to touch. Stage explicit paths, never `git add -A`, so stray churn cannot ride along in a commit.

The workspace baseline is rustfmt-clean as of `9ef48b18`. Keep it that way: if `cargo fmt --all -- --check` reports diffs in files you did not touch, that is drift to fix in its own commit, not something to route around. A `cargo fmt --all --check` CI gate is the outstanding follow-up.

Background-wait discipline (RSI-managed sessions): completion notifications
for backgrounded shell tasks frequently do NOT re-invoke the agent in this
harness — waiting idle on a background build/lint/test task can strand the
session until the operator nudges it. This occurs often (operator-confirmed
2026-07-27).

**Prefer the foreground.** The simplest correct answer is to run long local work
in the FOREGROUND with an explicit generous timeout rather than backgrounding it
and waiting. A foreground run needs no wake, no polling, and no notification.

**Do not size that timeout from this file — measure it.** A full
`cargo test -p rsid --lib -- --test-threads=1` was once ~4 minutes, and this
section used to say 600000 ms "covers it comfortably". That is no longer true:
the suite has since exceeded the 600000 ms ceiling (operator-confirmed
2026-08-27), which is the maximum the Bash tool accepts. When a run exceeds the
ceiling the harness MOVES IT TO THE BACKGROUND, and if the session then dies you
are left with a zero-byte output file, no completion record, and no way to tell
"suite passed" from "suite never ran". That exact sequence stranded a session
mid-verification.

Practical consequences, in order of preference:

1. **Scope the run.** Prefer `cargo test -p rsid --lib <filter>` for the modules
   your change touches. A targeted run finishes in seconds and is what you
   actually need for an edit-verify loop.
2. **Drop `--test-threads=1` unless you need it.** It exists for tests that
   contend on shared state; it is not required for the whole suite.
3. **If you must run everything, tee it to a file you own** so a
   background-promotion or a session death still leaves evidence:
   `cargo test -p rsid --lib 2>&1 | tee /tmp/rsid-suite.log`. Then read the log.
4. **Never report a suite as green that you did not see finish.** A promoted
   background run whose output file is empty is an UNKNOWN result, not a pass.
   Record it as an open verification gate in your handoff.

**Do NOT poll with foreground `sleep`.** Some harnesses block it outright, and a
blocked tool call can terminate the session. If you must background something,
re-check its output file with ordinary shell commands instead.

**Every agent-scheduled wake must supply an explicit `mode`; omission is an
error. If you arm a self-wake, it must use `mode:"resume"` — never
`wake_mode: agent_fresh` on your own session id.** `agent_fresh` launches a
SECOND agent process into your still-running session's sandbox worktree, giving
two uncoordinated writers on one tree; it has corrupted both source and test
evidence and has destroyed multiple sessions (issues #27 and #30, priority 1).
Disarm any wake you arm once the work it was waiting on completes.

**Commit early and often at clean boundaries.** An interrupted or restarted
session leaves uncommitted work stranded in its sandbox with no handoff, and
sandbox worktrees are reclaimable (issues #31 and #33). Work that is committed
survives; work that is only in the worktree may not.

**Completion requires a commit.** Before ending a turn that changes any
task-owned tracked file, every agent must commit those scoped changes. Do not
report the work complete while it exists only as an uncommitted diff. Stage
explicit paths only and, if a commit is genuinely blocked, preserve the work
and report the exact blocker. This requirement applies to all agent workflows
and skills; it does not authorize committing unrelated user changes.

**Unattended program no-idle invariant.** In `master_orchestrate` program mode,
budget/time/revision exhaustion, reclassification, and nonzero or pending review
findings are continuation conditions, never human gates. The first RSI control
action for explicit program mode is the argument-free
`rsi_control_program_guard` tool when advertised; otherwise use
`AgentScheduleWake`/Harness `schedule_wake` with `mode:"program_guard"`, no
timing fields, and no caller/row identity. The daemon binds a deterministic
far-future one-shot same-session Resume sentinel;
its returned UUID records program identity and must never be reported as a
continuation job. Before a program turn
with unfinished authorized work ends, exactly one durable state must exist: an
enabled child terminal watch, a same-session one-shot `Resume` wake, exhausted
queue, or a typed/evidenced human gate. Validate new worker handoffs with
`rsi-contract-validate --strict-v2` and final reports with
`rsi-contract-validate --orchestration-outcome`; active continuation states
must name the exact scheduled-job UUID. At terminal settlement rsid matches
the program sentinel and declared continuation by exact id, disables the
sentinel only for queue exhaustion or a typed human gate, and may insert only a
deterministic same-session one-shot `Resume` recovery. Recovery uses
`sandbox_root` when present and never creates a second `Fresh` or `AgentFresh`
writer.

A valid disabled sentinel is closed program identity. Ordinary later
non-program output is silent and does not rearm it; program output while closed
fails closed with one deterministic recovery. Explicit program-guard
registration is the sole rearm and restores the same deterministic row;
Scheduled Jobs toggles are not registration. A reserved program successor must
make program-guard registration its first RSI control action before any program
output. Successor launch itself never rearms the sentinel.

Use subagents or parallel agents only when the active harness supports them and the task is large enough to justify coordination overhead.

If committing:
- Scope each commit to one coherent unit.
- Stage explicit paths only.
- Follow existing commit-message style.
- Do not add attribution footers.
- Do not push unless explicitly asked.

### Branch policy — `rolling` is agent-managed, `main` is protected

Operator-confirmed 2026-08-07:

- **`rolling` is the agent-managed rolling development branch. Agents may merge
  into it as needed and do NOT need express consent to do so.** Keep it free of
  junk, and never merge work that carries broken code — merge only what passes
  the gates the change is subject to.
- **`main` is the protected branch. Merging to `main` requires explicit operator
  consent, every time.**

**Pushing:**

- **Pushing to `rolling` is permitted** and does not need express consent.
- Pushing to `main` still requires explicit operator consent, like merging to it.
- **NEVER push in a way that would overwrite commits that should have been pulled
  in first.** This is the actual rule, and it is about the OUTCOME, not the flag —
  `--force` is only the most common way to violate it. The operator works from more
  than one machine, so commits you have never seen can already exist on the remote.
  In practice:
  - Never `--force`, never `--force-with-lease`, never a `+refspec`.
  - `git fetch` and integrate BEFORE you push, every time.
  - Only ever push a clean fast-forward of the current remote tip.
  - **A push rejected as non-fast-forward is CORRECT and is protecting real work.**
    The answer is always fetch → integrate → push forward. It is never to re-run
    the push with more force.

**Work in `sandbox_root`, never in `working_dir`, whenever the two differ.** For
rsi-managed agent sessions `working_dir` points at the SHARED main worktree
(normally checked out on `rolling`), while `sandbox_root` is the session's own
isolated worktree. An agent that follows `working_dir` operates outside sandbox
isolation, on a tree shared with the operator and with any other process, and
its commits land on whatever branch happens to be checked out there. That tree
has more than one legitimate user, so the checked-out branch CAN change between
your commands and your own `git checkout -b` is not a durable guard: re-verify
`git branch --show-current` in the directory you are about to commit from. See
issue #38.

## Agent Contract & Thought-System Discipline (MWP/ICM)

The MWP/ICM stage-contract standard is **binding on every pipeline agent**, not
optional guidance. It is app-wide by construction: the daemon prepends the
orchestration router and the per-kind worker preamble to every leaf-kind worker
launch (`crates/rsid/src/session/{preamble,launch}.rs`), so every worker inherits
the contract regardless of which command spawned it.

The standard, in brief:
- **Stage-contract block** — each stage handoff carries a `## Stage contract`
  section with four `###` sub-sections in canonical order: **Inputs**,
  **Process**, **Outputs**, **Verify**. `### Inputs` must declare named static
  inputs OR a code-discovery budget (grep/glob) or both.
- **Provenance** — research sidecars are `RESEARCH_SCHEMA_VERSION = 2`; each
  `findings[].id` (`F-001`, `F-002`, …) is a stable join key. Plan items that
  trace to research declare `satisfies: [F-…]`; the implementation's verification
  manifest covers each declared key. Uncovered keys are research→plan→impl drift.
- **Validators** — `rsi-contract-validate` (handoff marker + `cross_stage_verify_coverage`
  against the manifest), `rsi-research-validate` (v2 research sidecars),
  `rsi-handoff-validate` (handoff docs), `rsi-manifest-validate` (verification
  manifests). Build them from `rsi-common` (see Build And Dev).

Canonical docs live in `.claude/commands/_shared/worker_preamble*.md` (the
per-worker contract) and the pipeline commands (`master_orchestrate.md`,
`research.md`, `plan.md`, `create_handoff.md`, `validate_plan.md`, …). This file
names the standard; the mechanics live there.

## Context Rotation

If the active harness supports context rotation and reports that rotation or auto-compaction is required, run the repo’s context-rotation flow immediately. Preserve structured handoff state before continuing.

## Reference Docs

Plans: `thoughts/shared/plans/`

Project spec: `thoughts/shared/project/2026-01-13-rsi.md`

Types reference: `thoughts/shared/reference/types-and-interfaces.md`

Keybindings: `docs/keybindings.md`

Feature-specific details belong in docs or reference files, not in this top-level instruction file.
