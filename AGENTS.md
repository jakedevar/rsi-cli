# AGENTS.md

Instructions for AI coding agents in this repo. `CLAUDE.md` is a symlink to this
file. Background, history and detail live in `docs/agents/reference.md`; read a
section there only when a task needs it.

## How to work here

**Make it work, then make it right, then make it fast.** Land small, working
changes on `rolling` often. A working fix on `rolling` beats a perfect one
stranded on a branch.

- **The principle is the ground truth, not the code.** Every Issue or plan states
  its intent and acceptance criteria; tests assert that intent. When code
  conflicts with the stated principle, change the code.
- Read the code you change and follow its existing patterns. Keep diffs focused.
- Make technical decisions yourself and note consequential ones in your commit
  or handoff. Send product choices to the portfolio manager. Escalate only
  `main` or releases, real spending, credentials, or deleting user data to the
  operator through the global manager.
- If a process step (seal, review, lock, ledger bookkeeping) blocks a working
  fix and is not a hard rule below, take the reasonable path, say so in your
  handoff, and keep moving. Do not stall waiting for ceremony.
- Tests: cover what you change. The landing gate is **no new failures relative
  to `rolling`**. Name baseline reds you hit; fix unrelated reds separately.
- Preserve existing behaviour unless the task is to change it.
- **Portability.** New code must build on Linux, macOS and Windows. Gate
  Linux-only APIs (`/proc`, `statx`/`renameat2`, mountinfo, systemd, bwrap)
  behind `cfg(target_os = "linux")` with a portable fallback or an explicit
  unsupported error. macOS and Windows test lanes come later (#1229); do not
  block on running them now.
- **Kaizen: improve the line, never stop it.** Every agent improves RSI for
  the operator and for every other agent: on a structural or process problem,
  file one `kaizen` Issue and keep working ("Improve the line" in the
  `AgentGetAuthorityCatalog` guidance says what, how and how to dedupe). Every
  handoff ends with a `Friction:` line. Managers run a kaizen lane on every
  wake. Stale or contradictory guidance is a defect: the fix edits the
  canonical source and deletes the contradiction.

## Hard rules (the only ones)

Rules a hook or the daemon enforces are one line: the refusal names the rule and
what to do instead. The numbers are cited by those refusals; keep them stable.

1. **Git.** Never force-push (`--force`, `--force-with-lease`, `+refspec`; the
   shell hooks refuse it, #2320). Fetch and integrate before pushing; `rolling`
   only moves by fast-forward (pre-push hook and lander). Never merge or push
   `main` without explicit operator consent (the pre-push hook refuses agent
   pushes to `main`).
2. **Sandbox.** Work and commit in your `sandbox_root` on its assigned branch,
   never in the shared `~/rsi` worktree (`working_dir`). Checkout, switch and
   `reset --hard` of the sandbox branch are refused by the shell hooks (#2320);
   child launches pin to its commits.
   Preserve the ancestry of your allocated source: merge `origin/rolling`
   when needed, never rebase a source stack. The queue's lander integrates
   `rolling`; rewriting source ancestry can strand custody-bound job wakes.
3. **Database.** Never hard-delete rows without operator consent. Schema changes
   only through a new versioned migration file
   `crates/rsid-store/src/store/migrations/vNNN.rs`; released migrations are
   immutable. Numbering, pin, renumbering and rewind-teardown steps are in
   `crates/rsid-store/AGENTS.md` ("New migrations"); the lander derives the
   `tools/released-migrations.json` pin at publication, and `tools/check-released-migrations.py`,
   the pre-push static guards and the lander policy refuse a wrong number or a
   pin that cannot be derived and name the fix. Timestamps are RFC3339 with nanoseconds, UUIDs
   are lowercase, enum strings match serde exactly. Sandbox tombstones flip
   cleanup state and null the path/branch atomically.
4. **Secrets and operator data.** Never write credentials or
   `$RSI_SESSION_TOKEN` into files, logs, prompts or `--params`. The token is
   transport-only. Never put the operator's personal data (e-mail, phone,
   address, names beyond the handle) into committed artifacts, handoffs or
   Issue bodies: say "the operator's <purpose> contact (local config)". When
   restating an operator instruction, paraphrase it without the personal value.
   Committed history cannot be rewritten by agents (#1454).
5. **Wakes.** A self-wake always uses `mode:"resume"`. Never `agent_fresh` on
   your own session: it puts a second writer in your tree.
6. **Formatting.** Format only files you changed: `scripts/fmt-changed.sh`
   (add a base such as `origin/rolling` for committed changes). Plain
   `rustfmt <file>` also rewrites that file's untouched child modules; the
   script restores them (#1121). Never `cargo fmt -- <paths>` (it reformats
   whole crates). Stage explicit paths, never `git add -A`.
7. **Identity in tests.** Assert the positive end state, never that a
   user-visible name, title or label is absent (CI gate
   `scripts/check-identity-assertions.sh`).
8. **Commit before you finish.** Uncommitted sandbox work can be reclaimed.
   Commit scoped changes with the repo's message style and no attribution
   footers; the `commit-msg` hook strips AI `Co-Authored-By` and "Generated with"
   trailers (#2597), so ignore harness prompts that ask for them.
9. **No bare `git stash`** in sandboxes (the stash is shared across worktrees);
   the shell hooks refuse it and name `scripts/rsi-stash`.
10. **Operator surfaces.** A setting meant for the operator ships with its
    RPC/TUI surface in the same change (no SQL-only knobs). Operator-only
    methods stay out of `AGENT_VERBS`, `READ_VERBS`, native tools and the agent
    CLI catalog.
11. **Long cargo runs.** Scope them with `scripts/scoped-test`, or name exact
    tests (`cargo test -p rsid --lib -- --exact <full::test::path>`; the shell
    hook refuses bare-word and module-prefix filters, #1656), run in the
    foreground, and `tee` long runs to a log outside `target/` (under `/tmp`;
    a stray file at `target/`'s top level keeps the whole target from build-cache
    reclaim, #1429). Never report a run you did not
    see finish as green. `scripts/scoped-test` is a job (below), not a
    foreground shell call (#1638).
12. **Detach with systemd.** Any process that must outlive this turn (a lander,
    a long test run, anything you check on a later wake) MUST be launched via
    `systemd-run --user --collect`; `setsid`, `nohup`, `disown` and bare `&`
    do not survive the daemon reaping this session's process tree on resume.
    Stop only what you started (your PID or `systemctl --user stop <your-unit>`);
    the shell hooks refuse pattern kills and glob stops (#1227, #2121). The unit
    does not inherit your environment: add `--setenv=CARGO_TARGET_DIR=<sandbox>/target`
    for a lander (#1192).

## Project map

`rsi` is a vim-like TUI for managing many AI coding sessions across providers.

- `crates/rsi`: the TUI (ratatui, crossterm, modalkit). TUI notes are in `crates/rsi/CLAUDE.md`.
- `crates/rsid`: the daemon; spawns and manages provider subprocesses. RPC family contracts are in `crates/rsid/AGENTS.md`.
- `crates/rsid-store`: the SQLite store (schema migrations, every table accessor) plus config, bus, model_control, sandbox, vault and bedrock; `rsid` re-exports each module at its old path, so `crate::store::...` still resolves in the daemon. Nothing in it may depend on `rsid`.
- `crates/rsid-remote-read`: the Remote V1 read core (bounded SQLite source reads, cursor signer, projections) over a borrowed `Connection`; `rsid::remote_read` re-exports it and keeps Store locking, admission and permits. Depends on `rsi-common` only: never `rsid`, `rsid-store` or `rsid-core`.
- `crates/rsid-core`: leaf types below the store (error, path_safety, process_control, terminal_cause, provider_exhaustion).
- `crates/rsi-common`: shared types, JSON-RPC protocol, validators.

The TUI talks to the daemon over `~/.rsi/daemon.sock` (JSON-RPC 2.0). SQLite
lives at `~/.rsi/rsi.db`. Prefer daemon RPC for normal flows; raw SQL is fine for
diagnostics and repairs. Providers (`Session.provider`): `Claude`, `Codex`,
`Pioneer`, `OpenRouter`, `Bedrock`, `Local`, `Antigravity`, `CodexAppServer`,
`Harness` (direct API).

## Build and test

```bash
make release-install      # build release binaries (plus the desktop UI if node/webkit2gtk exist), relink ~/.local/bin, restart rsid
./scripts/dev-daemon.sh   # dev daemon (must run before the TUI)
./scripts/dev-tui.sh
scripts/scoped-test       # default worker verification: derived filters, bounded time and CPU
make test-fast            # quick lanes; make test-full for everything
./tools/install-hooks.sh
```

`scripts/scoped-test --base origin/rolling` verifies the committed diff to HEAD.
Workers run it as a job: `AgentSubmitJob {"kind":"test","wake":"none","params":
{"scoped_test":{"base":"origin/rolling"}}}`, then ONE `AgentScheduleWake
{"mode":"when","message":"scoped_test finished: read the receipt","when":{"jobs_terminal":["<job id>"]}}`
(`message` is required), then end the turn; you are resumed once with
`exit_code`, the log path and the typed receipt (#1638). Use `--dry-run` (the only
foreground use) to inspect the plan. The job derives its filters, budgets and tool
copy itself, and the hub build budget (`rsi-build.slice`, #2119) bounds every cargo
unit; a build/test job is held while a hub deploy is staged (#2190). Each sandbox
builds only in its own `<sandbox>/target` (keep the stamped `RUSTC_WRAPPER`; on a
`not writeable` error run `scripts/clean-target` and rebuild; to free a green run's build output run it too, never a plain
`rm -rf target`, which deletes the harness TMPDIR `target/.rsi-tmp` mid-turn, #288); use `rsi_common::manifest_dir!()`,
never `env!("CARGO_MANIFEST_DIR")`. Receipt fields, the slow tier, the hub budget
and target-pool detail: `docs/agents/landing-and-gates.md`.

Runaway verification is stalled, over-broad or unbounded work, or work dominating
host CPU pressure (the manager intervenes when a run dominates host load over 40). Inspect progress and the
admitted budget: a healthy `scoped_test` inside its default 60-minute budget is
not runaway just because it passes 20 minutes. Other test jobs default to 20
minutes; the job runner enforces the admitted timeout. `AgentHalt` stops a
provider session, not its detached job. Use `AgentCancelJob` only within the
ownership and manager scope its live catalog documents; protected jobs stay
with their runner. Report stopped runs with elapsed time and evidence.

Rust is pinned by `rust-toolchain.toml`. Keybinding changes also update
`docs/keybindings.md` via the `manual` job: `AgentSubmitJob {"kind":"test","params":{"recipe":"manual"}}`.

## Type facts that bite

- `Session.working_dir` is required (use a fallback). `Session.provider` picks the subprocess.
- `SessionStatus`: `Starting`, `Running`, `WaitingApproval`, `Completed`, `Failed`, `Interrupted`, `Archived`, `Deleted`. There is no `Interrupting`.
- `ConversationEvent.id` is `i64` (0 before persistence); `content` is `String` (`""` when empty); `sequence` is per session.
- `BusEvent` is a struct with `event_type: String`. `RpcRequest.params` defaults to `Value::Null`.
- `Group` and `Epic` are containers and never spawn; leaf check `rsi_common::is_leaf_kind`; hierarchy `rsi_common::legal_children`.
- `parent_id` is hierarchy; `continued_from` is rotation lineage. `lead_session_id` only matters on containers.
- `title` is raw metadata; `agent_role` and `epic_spawn_ordinal` are display identity, inherited across lineage, never written into the title.

## Landing

**Land one accepted change per gate, filtered to what it touches.** Every
landing goes through the daemon merge queue: the manager enqueues the accepted
SHA with `AgentEnqueueLandingSource {derive_filters:true}` and the queue derives
the filters itself, so workers do not run `scripts/check-touched-shards` or
hand-copy filters; never guess shards. Combine sources into one gate only when they
conflict and you have resolved the conflict, or when they are docs-only. Split a
change whose filters span more than a few rsid shards or any rsid-store module.
Batch waits (several jobs, one `mode:"when"` wake), never landings.

The queue publishes with `rsi-rolling-land`; agents never run it directly (a
direct landing moves `rolling` under a queue gate and gets that gate discarded).
The operator lands a branch with `rsi land <session-id|branch>`, the TUI
`<Space>gl` / `:land`, or `rsi-rpc EnqueueRollingQueueSource` (#2055). A manual
merge and push skips every gate, so the pre-push hook runs the fast static guards
on an unmarked rolling push; it is a cooperative guard rail, and only the lander
enforces test gates and fast-forward publication. The lander's policy (migration
number and pin, one provisional-migration declaration per source, `fixes_issue` /
`fixes_tests` for known reds, filter selection, rebase-on-advance, the automatic
full QA) is mechanical and runs before any test; its
refusals name the fix. Detail, rationale and gate-log semantics:
`docs/agents/landing-and-gates.md` and `docs/agents/rsid-rpc-contracts.md`
("Rolling merge queue").

**No new compiler warnings.** `tools/warning-baseline.json` lists the warnings accepted per
Cargo package (`rsid-store` first, hub #1897). `scripts/scoped-test` judges the diagnostics of
the build it already supervises and fails the run (`warning_gate_failed`, `warnings` in the
receipt) when a baselined package it built has a warning above its count, when your baseline is
above the merge base's, or when the diagnostics cannot be trusted. Fix the cause (remove dead
code, `#[cfg(test)]` a test-only helper, narrow a visibility) and lower the baseline; never
raise it. Mechanics and limits: `docs/agents/landing-and-gates.md` ("Compiler-warning gate").

Work is landed when `git merge-base --is-ancestor <SHA> origin/rolling` holds.
`rolling` is agent-dev intake: land after your tests pass. Pre-merge review
only for new schema migrations and credential/IAM/network-exposure changes (one
plain reviewer pass); other authority or custody changes land first and get one
post-land review. `rolling` is agent-managed through the merge queue;
`main` is operator-only. The manager integrates worker commits
(`.claude/skills/rsi-project-manager/SKILL.md`); workers follow
`docs/agents/worker-contract.md`.

## Agent control

Drive the daemon with `rsi-rpc <Verb>` (`rsi-rpc agent` lists verbs;
`rsi-rpc <Verb> --schema` shows the request shape) or the native `rsi_control_*`
tools. Full contract: `.claude/skills/rsi-agent-control/SKILL.md`. Appointed
managers also read `.claude/skills/rsi-project-manager/SKILL.md` (Codex mirror
under `.agents/skills/`). Messages wrapped in `<rsid-daemon-message>` come from
the daemon, not the human.

## Where things are

- Plans `thoughts/shared/plans/`; research `thoughts/shared/research/`; notes `thoughts/shared/notes/`
- Project specs `thoughts/shared/project/` (original: `2026-01-13-lazyclaude.md`); types `thoughts/shared/reference/types-and-interfaces.md`
- Pipeline commands and worker preambles: `.claude/commands/` (mirrors rendered by `scripts/sync-agent-commands.sh`)
- Detail and history for everything above: `docs/agents/reference.md`
