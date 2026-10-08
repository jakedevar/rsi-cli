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

1. **Git.** Never `--force`, `--force-with-lease` or a `+refspec`. Fetch and
   integrate before pushing; `rolling` only moves by fast-forward. A rejected
   push means fetch, integrate, retry. Never merge or push `main` without
   explicit operator consent.
2. **Sandbox.** Work and commit in your `sandbox_root` on its assigned branch,
   never in the shared `~/rsi` worktree (`working_dir`). Do not checkout, switch
   or reset your sandbox branch: child launches pin to its commits.
3. **Database.** Never hard-delete rows without operator consent. Schema changes
   only through a new versioned migration: one new file
   `crates/rsid-store/src/store/migrations/vNNN.rs` holding the `if version < N`
   block and the `user_version` bump (`build.rs` collects the files; the head is
   the highest number, so no shared file is edited); released
   migrations are immutable. Timestamps are RFC3339 with nanoseconds, UUIDs are
   lowercase, enum strings match serde exactly. Sandbox tombstones flip cleanup
   state and null the path/branch atomically.
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
7. **Identity in tests.** Never assert that a user-visible name, title or label
   is absent; assert the positive end state (CI gate
   `scripts/check-identity-assertions.sh`).
8. **Commit before you finish.** Uncommitted sandbox work can be reclaimed.
   Commit scoped changes with the repo's message style and no attribution
   footers.
9. **No bare `git stash`** in sandboxes (the stash is shared across worktrees);
   use `scripts/rsi-stash`.
10. **Operator surfaces.** A setting meant for the operator ships with its
    RPC/TUI surface in the same change (no SQL-only knobs). Operator-only
    methods stay out of `AGENT_VERBS`, `READ_VERBS`, native tools and the agent
    CLI catalog.
11. **Long cargo runs.** Scope them with `scripts/scoped-test`, or name exact
    tests (`cargo test -p rsid --lib -- --exact <full::test::path>`; the shell
    hook refuses bare-word or module-prefix filters, #1656), run in
    the foreground, and `tee` long runs to a log. Never report a run you did not
    see finish as green. Exception: `scripts/scoped-test` is a job (below), not a
    foreground shell call (#1638).
12. **Detach with systemd.** Any process that must outlive this turn (a lander,
    a long test run, anything you check on a later wake) MUST be launched via
    `systemd-run --user --collect`; `setsid`, `nohup`, `disown` and bare `&`
    backgrounding do not survive the daemon reaping this session's process tree
    on resume. Stop only what you started: `systemctl --user stop <your-unit>`
    or `kill <your-PID>`. Never `pkill`, `killall`, `kill -1` or a
    `pgrep`-fed `kill`; every agent runs as one user, so a pattern kills other
    agents' test units and landers (#1227; the agent shell hooks refuse them).

## Project map

`rsi` is a vim-like TUI for managing many AI coding sessions across providers.

- `crates/rsi`: the TUI (ratatui, crossterm, modalkit). TUI notes are in `crates/rsi/CLAUDE.md`.
- `crates/rsid`: the daemon; spawns and manages provider subprocesses. RPC family contracts are in `crates/rsid/AGENTS.md`.
- `crates/rsid-store`: the SQLite store (schema migrations, every table accessor) plus config, bus, model_control, sandbox, vault and bedrock; `rsid` re-exports each module at its old path, so `crate::store::...` still resolves in the daemon. Nothing in it may depend on `rsid`.
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
{"scoped_test":{"base":"origin/rolling"}}}`, then ONE `AgentScheduleWake`
`mode:"when"` (`when.jobs_terminal`), then end the turn; you are resumed once with
`exit_code`, the log path and the typed receipt (#1638). A foreground call outlives
the provider's 10-minute tool cap on a busy host.
Use `--dry-run` to inspect the plan without building; add `--head <rev>` to
inspect another candidate. Execution requires the candidate to be this HEAD. It builds each selected package once and runs its matching tests under
a per-package budget (build included): `--runtime-max-sec 1800 --cpu-quota 200`.
Linux uses a foreground systemd user service; other platforms use a plain timeout
and explicitly report that no CPU cap is available. Output is tee'd under
`/tmp/rsi-scoped-test-*`; a timeout names the active test and exits nonzero.

Rust is pinned by `rust-toolchain.toml`. Keybinding changes also update
`docs/keybindings.md` via `make manual`.

## Type facts that bite

- `Session.working_dir` is required (use a fallback). `Session.provider` picks the subprocess.
- `SessionStatus`: `Starting`, `Running`, `WaitingApproval`, `Completed`, `Failed`, `Interrupted`, `Archived`, `Deleted`. There is no `Interrupting`.
- `ConversationEvent.id` is `i64` (0 before persistence); `content` is `String` (`""` when empty); `sequence` is per session.
- `BusEvent` is a struct with `event_type: String`. `RpcRequest.params` defaults to `Value::Null`.
- `Group` and `Epic` are containers and never spawn; leaf check `rsi_common::is_leaf_kind`; hierarchy `rsi_common::legal_children`.
- `parent_id` is hierarchy; `continued_from` is rotation lineage. `lead_session_id` only matters on containers.
- `title` is raw metadata; `agent_role` and `epic_spawn_ordinal` are display identity, inherited across lineage, never written into the title.

## Landing

**Land one accepted change per gate, filtered to what it touches.** Derive the
filters with `scripts/check-touched-shards --base origin/rolling` (worker
contract); never guess shards. Combine sources into one gate only when they
conflict and you have resolved the conflict, or when they are docs-only. A
gate's cost must scale with the change: if its filters span more than a few
rsid shards or any rsid-store module, split it. Batch waits (several jobs, one
`mode:"when"` wake), never landings. Evidence: `docs/agents/reference.md`.

Publish with `rsi-rolling-land --repo <sandbox> --remote origin --accepted <SHA>`.
`make release-install` installs it on PATH next to `rsid` (the daemon merge queue spawns it from there).
Add `--test-filter PACKAGE=FILTER` for the modules you touched: a filtered
package's gate is a compile check plus the named tests (no new failures against
the base); the QA sweep runs the rest. The `rsid` and `rsid-store` library tests
are partitioned into 16 shards (`test-shard-*` features, `store-01..04`,
`session-01..05`, `memory-01..02`, `other-01..05`); a shard is one feature name
that every package with tests in it declares, and `scripts/run-rsid-test-shards.sh`
runs it across those packages (`scripts/check-rsid-test-shards.py
--list-shard-packages`). Name one as `--test-filter rsid=shard:store-01:test(NAME)`
whichever package holds the test (the store tests live in `crates/rsid-store`).
A focused `rsid`/`rsid-store` filter (`shard:S:test(NAME)` or a plain name) runs
on one lib test build of both packages without shard features, so the name may
sit in any shard; a filter on either package scopes both. An unfiltered change
runs every library test that can observe it (the tests that name the changed
items, transitively, plus the source scanners; every library test when that is
uncertain or over 200 tests, #1280) and the tests of a changed bin or
integration target, plus `cargo check`; only a crate root, manifest or
build-script change runs all 16 shards. Another package's lib filters run in one
libtest invocation, and a filter that selects no test is refused after the
build (#1282). The gate runs
the candidate first and builds the base only for a candidate failure (#1244). The lander's policy is mechanical and
runs before any test: no Work binding, seal or hot-file claim is required, and a
new migration must be the rolling tip's schema head + 1 (the highest
`store/migrations/vNNN.rs`) as its own file with its `if version < N` block; one
landing may carry several migrations only as a contiguous run from that number,
each in its own file (the refusal names the number). It reports each
source's `source_binding=<SHA>:bound|unbound|unknown` for information only.
Fast-forward-only publication and the canary remain. The pre-push hook permits
the lander's marked `rolling` push; unmarked agent pushes to `rolling` and every
agent push to `main` remain refused.
The hook is a cooperative guard rail, not a security boundary: agents can spoof
the lander's `RSI_ROLLING_LANDER=1` marker or bypass hooks. Only the lander
enforces test gates and fast-forward publication.
Work is landed when `git merge-base --is-ancestor <SHA> origin/rolling` holds.
`rolling` is agent-dev intake: land after your tests pass. Pre-merge review
only for new schema migrations and credential/IAM/network-exposure changes (one
plain reviewer pass); other authority or custody changes land first and get one
post-land review.
The QA lane sweeps `rolling`, records each passing SHA in the pointer file
`thoughts/shared/qa/qa-green.sha` (landed on `rolling`; `qa-green` is not a
branch), and files regressions back as Issues.
`rolling` is agent-managed (merge and push fast-forwards freely); `main` is
operator-only. The manager integrates worker commits (`.claude/skills/rsi-project-manager/SKILL.md`);
workers follow `thoughts/shared/manager/worker-contract.md`.

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
