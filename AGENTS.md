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
  or handoff. Ask the operator only about `main` or releases, real spending,
  credentials, deleting user data, or a genuine product choice.
- If a process step (seal, review, lock, ledger bookkeeping) blocks a working
  fix and is not a hard rule below, take the reasonable path, say so in your
  handoff, and keep moving. Do not stall waiting for ceremony.
- Tests: cover what you change. The landing gate is **no new failures relative
  to `rolling`**. Name baseline reds you hit; fix unrelated reds separately.
- Preserve existing behaviour unless the task is to change it.

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
   `crates/rsid/src/store/migrations/vNNN.rs` holding the `if version < N`
   block and the `user_version` bump (`build.rs` collects the files; the head is
   the highest number, so no shared file is edited); released
   migrations are immutable. Timestamps are RFC3339 with nanoseconds, UUIDs are
   lowercase, enum strings match serde exactly. Sandbox tombstones flip cleanup
   state and null the path/branch atomically.
4. **Secrets.** Never write credentials or `$RSI_SESSION_TOKEN` into files,
   logs, prompts or `--params`. The token is transport-only.
5. **Wakes.** A self-wake always uses `mode:"resume"`. Never `agent_fresh` on
   your own session: it puts a second writer in your tree.
6. **Formatting.** Format only files you changed:
   `git diff --name-only --diff-filter=d -- '*.rs' | xargs -r rustfmt --edition 2024`.
   Never `cargo fmt -- <paths>` (it reformats whole crates). Stage explicit
   paths, never `git add -A`.
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
11. **Long cargo runs.** Scope them (`cargo test -p rsid --lib <filter>`), run in
    the foreground, and `tee` long runs to a log. Never report a run you did not
    see finish as green.
12. **Detach with systemd.** Any process that must outlive this turn (a lander,
    a long test run, anything you check on a later wake) MUST be launched via
    `systemd-run --user --collect`; `setsid`, `nohup`, `disown` and bare `&`
    backgrounding do not survive the daemon reaping this session's process tree
    on resume.

## Project map

`rsi` is a vim-like TUI for managing many AI coding sessions across providers.

- `crates/rsi`: the TUI (ratatui, crossterm, modalkit). TUI notes are in `crates/rsi/CLAUDE.md`.
- `crates/rsid`: the daemon; spawns and manages provider subprocesses. RPC family contracts are in `crates/rsid/AGENTS.md`.
- `crates/rsi-common`: shared types, JSON-RPC protocol, validators.

The TUI talks to the daemon over `~/.rsi/daemon.sock` (JSON-RPC 2.0). SQLite
lives at `~/.rsi/rsi.db`. Prefer daemon RPC for normal flows; raw SQL is fine for
diagnostics and repairs. Providers (`Session.provider`): `Claude`, `Codex`,
`Pioneer`, `OpenRouter`, `Bedrock`, `Local`, `Antigravity`, `CodexAppServer`,
`Harness` (direct API).

## Build and test

```bash
make release-install      # build release binaries, relink ~/.local/bin, restart rsid
./scripts/dev-daemon.sh   # dev daemon (must run before the TUI)
./scripts/dev-tui.sh
make test-fast            # quick lanes; make test-full for everything
./tools/install-hooks.sh
```

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

Publish with `rsi-rolling-land --repo <sandbox> --remote origin --accepted <SHA>`.
Until it is installed on PATH, use `~/.cargo/shared-target/debug/rsi-rolling-land`.
Add `--test-filter PACKAGE=FILTER` for the modules you touched: a filtered
package's gate is a compile check plus the named tests (no new failures against
the base); the QA sweep runs the rest. The lander's policy is mechanical and
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
