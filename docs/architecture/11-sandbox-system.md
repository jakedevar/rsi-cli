# Sandbox System

> Opt-in per-session git-worktree isolation for provider subprocesses

## Overview

The sandbox system allocates a `git worktree` for each sandboxed session before
the provider subprocess is spawned. The subprocess runs with `current_dir` set
to the worktree root; the canonical `working_dir` is never touched. This gives
each session an isolated filesystem scope — writes in one session are invisible
to the canonical repo and to other sessions until explicitly merged.

Sandboxing is opt-in, capability-gated (daemon must advertise
`DaemonCapabilities.sandbox = true`), and defaults to off at every launch site.

---

## System Architecture

```
TUI (overlay/prompt.rs)
  │  key `s` toggles sandbox_enabled
  │  LaunchSessionParams { sandbox: Some(SandboxSpec) }
  ▼
SessionManager::launch_session (session/launch.rs)
  │
  ├─ SandboxAllocator::allocate(session_id, &working_dir, GitWorktree)
  │    ├─ ensure_base()        ← create $RSI_SANDBOX_BASE (0o700)
  │    ├─ git rev-parse        ← reject non-git directories (fail-closed)
  │    ├─ proposed_root = base / session_uuid
  │    ├─ path safety check    ← reject symlink cycles / invalid UTF-8
  │    ├─ abort if root exists ← orphan guard
  │    └─ git worktree add -b rsi/<8-hex> <root>
  │
  ├─ On Err → return Err, NO Session row inserted
  │
  ├─ effective_working_dir = allocation.root
  ├─ config.working_dir = effective_working_dir  ← single mutation point
  │
  ├─ Session {
  │    working_dir:           canonical repo root (unchanged)
  │    sandbox_kind:          Some(GitWorktree)
  │    sandbox_root:          Some(worktree path)
  │    sandbox_branch:        Some("rsi/<short>")
  │    sandbox_cleanup_state: Some(Live)
  │  }
  └─ persist.insert_session

Provider subprocess spawns in effective_working_dir (the worktree root).
All shell/git/file tools operate in the worktree — canonical repo is clean.
```

---

## Data Model

### `sessions` table — sandbox columns (V39)

| Column | SQLite type | Nullable | Rust type |
|---|---|---|---|
| `sandbox_kind` | `TEXT` | YES | `Option<SandboxKind>` |
| `sandbox_root` | `TEXT` | YES | `Option<PathBuf>` |
| `sandbox_branch` | `TEXT` | YES | `Option<String>` |
| `sandbox_cleanup_state` | `TEXT` | YES | `Option<SandboxCleanupState>` |

All default to `NULL`. Pre-V39 rows carry `NULL` → deserializes to `None`.

### Retained execution projections (V83, detached in V90)

`session_execution_projections` is custody/audit history, not ordinary
Session-owned payload. Every Session receives exactly one projection. A soft
delete changes the Session status to `Deleted` and leaves both rows intact, so
the Session can be restored.

An authorized hard purge first moves an ordinary unsandboxed projection to the
verified `historical_purged` state, then permanently removes the Session and
its ordinary child payloads in the same transaction. The projection remains
under the purged Session UUID. An already verified `historical_purged` or
`historical_transferred` projection is retained without rewriting its custody
identity or timestamps. Live, cleanup-failed, quarantined, invalid, missing, or
otherwise non-terminal projections block hard purge. A hard-purged Session has
no downgrade or restore path; only its retained projection survives.

### Shared types (`rsi-common::types`)

**`SandboxKind`:**

| Variant | String | Meaning |
|---|---|---|
| `None` | `"None"` | No sandbox; zero overhead fast path |
| `GitWorktree` | `"GitWorktree"` | `git worktree add` isolation (only shipped backend) |

**`SandboxCleanupState`:**

| Variant | String | Meaning |
|---|---|---|
| `Live` | `"Live"` | Worktree on disk; owned by active/completed session |
| `Purged` | `"Purged"` | Destroy confirmed; filesystem removed |
| `Failed` | `"Failed"` | Destroy attempted but may have left partial state; orphan sweep retries |

**`SandboxSpec`** (in `LaunchSessionParams`):

| Field | Type | Default |
|---|---|---|
| `kind` | `Option<SandboxKind>` | `None` → resolves to `GitWorktree` at launch |
| `branch` | `Option<String>` | `None` → `rsi/<8-hex-uuid-prefix>` |

---

## Configuration

| Env var | Default | Created at | Permission |
|---|---|---|---|
| `RSI_SANDBOX_BASE` | `~/.rsi/sandboxes/` | Daemon startup | `0o700` |

Legacy aliases: `MOTHERSHIP_SANDBOX_BASE`, `FLYWHEEL_SANDBOX_BASE`. Falls back
to `/tmp/rsi-sandboxes` if `$HOME` is unresolvable.

To reduce SSD wear, mount a tmpfs:

```bash
export RSI_SANDBOX_BASE=/dev/shm/rsi-sandboxes
```

---

## Lifecycle

### Allocation

1. `LaunchSessionParams.sandbox = Some(SandboxSpec)` arrives at `launch_session`.
2. `SandboxAllocator::allocate` runs all pre-flight checks; fails fast if the
   directory is not inside a git repo.
3. `git worktree add -b rsi/<short> <base>/<session-uuid>` creates the worktree.
4. On `Err` → `launch_session` propagates and returns without inserting a row.
5. On `Ok` → `config.working_dir` is mutated to point at the worktree root.
   All four `sandbox_*` columns are populated; `cleanup_state = Live`.

### Rotation and Continue

**Rotation** (`session/rotation.rs`):

- Child `Session` row copies all four `sandbox_*` fields verbatim from the
  parent row.
- `LaunchConfig.sandbox = None` prevents a second allocation — existing
  worktree is reused.
- `spawn_working_dir` resolves to `sandbox_root` when `Some`.
- Worktree is destroyed only when the last session in the lineage reaches
  terminal state (reference-count scan in `maybe_destroy_sandbox`).

**Continue** (`ContinueSession` RPC):

- Session loaded from DB; `sandbox_root` carries forward automatically from
  the persisted row.
- `spawn_working_dir` resolves to `sandbox_root`; subprocess re-enters the
  same worktree.
- `LaunchConfig.sandbox = None` — no re-allocation.

### Cleanup

`maybe_destroy_sandbox` is called by three callers:

| Caller | When |
|---|---|
| `archive_session` | Session archived by user or rotation saga |
| `delete_session` | Reversible delete; sandbox cleanup precedes the retained `Deleted` Session row |
| `purge_session` | Irreversible row purge from trash; destroy before the guarded Store transaction |

Algorithm:

```
maybe_destroy_sandbox(session_id, snapshot):
  1. Early return: kind == None or cleanup_state == Purged
  2. Reference-count scan: any other session in active/completed/DB with
     same sandbox_root? → skip destroy (rotation chain sharing worktree)
  3. Two-phase flip:
       persist cleanup_state = Failed   ← crash-safe retry signal
       if DB write fails → abort (do not destroy without retry signal)
  4. spawn_blocking: allocator.destroy(allocation)
       git worktree remove --force <root>
       git worktree prune
       git branch -D <branch>
       fs::remove_dir_all(root)         ← belt-and-suspenders
  5. On Ok  → persist cleanup_state = Purged
     On Err → log ERROR; state remains Failed (orphan sweep retries)
```

Destroy errors are NOT propagated — archive/delete always succeeds from the
user's perspective.

### Orphan Sweep

Runs once at daemon startup from `restore_sessions` → `sandbox_orphan_sweep`.

**Pass 1 — DB-driven (terminal-state rows still marked Live):**

1. `store.list_live_sandbox_owners()` — rows where `sandbox_cleanup_state =
   'Live' AND sandbox_root IS NOT NULL`.
2. For each terminal-status row: stamp `Failed`, call `allocator.destroy()`,
   stamp `Purged` on success.
3. Emits `DaemonEvent::SandboxOrphanCleaned` per successful removal.

**Pass 2 — Disk-driven (UUID dirs with no matching Live row):**

1. `allocator.list_on_disk()` — `read_dir($RSI_SANDBOX_BASE)`, parse UUID dirs.
2. Any UUID dir NOT in the live-owner set → `allocator.destroy_by_path(path)`.
   (Uses `fs::remove_dir_all`; cannot run `git worktree remove` without knowing
   the origin — leaves stale `.git/worktrees/` metadata cleared by a future
   `git worktree prune`.)

---

## RPC Surface

| Element | Location | Notes |
|---|---|---|
| Launch param | `LaunchSessionParams.sandbox: Option<SandboxSpec>` | `None` = non-sandboxed; zero wire overhead |
| Capability flag | `DaemonCapabilities.sandbox: bool` | TUI gates the toggle on this; `true` in `Default::default()` |
| Bus event | `DaemonEvent::SandboxOrphanCleaned` | `event_type = "sandbox_orphan_cleaned"` |

---

## TUI Integration

### Launch-prompt overlay (`overlay/prompt.rs`)

- Key `s` toggles `sandbox_enabled` on `OverlayState::Prompt`.
- No-op when `app.poll.sandbox_supported` is `false`.
- Applies to `PromptPurpose::Blank` and `PromptPurpose::TaskRabbit` only.
  Continue overlays always reuse the persisted row's sandbox state.
- Footer hint (capability-gated):
  ```
  [s]andbox: on     ← when enabled
  [s]andbox: off    ← when disabled
  ```
- On submit: `sandbox_spec_from_bool(sandbox_enabled)` produces
  `Some(SandboxSpec { kind: Some(GitWorktree), branch: None })` when on, `None`
  when off.

### Session-list indicator (`ui/session.rs`)

Character `⊡` (U+22A1) in `theme::teal()` at the left margin of the session
card when `session.sandbox_kind.is_some()`.

### Session-detail header (`ui/session.rs`)

When `session.sandbox_root.is_some()` and `scroll_offset == 0`:

```
⊡ sandbox: ~/.rsi/sandboxes/<uuid>  (rsi/<short>)
```

Root path abbreviates `$HOME` to `~`. Branch shown in `theme::overlay2()`.

---

## Failure Modes

| Failure | Error type | Propagation |
|---|---|---|
| `working_dir` not a git repo | `DaemonError::InvalidParam` | RPC error → TUI; no Session row inserted |
| `git worktree add` non-zero | `DaemonError::Process(stderr)` | Same — fail-closed |
| Path safety rejection | `DaemonError::InvalidParam` | Same |
| Root already exists | `DaemonError::InvalidParam("…possible orphan")` | Same |
| `destroy` filesystem error | `DaemonError::Process` | Logged; `cleanup_state = Failed`; caller sees `Ok` |
| `update_sandbox_cleanup_state(Failed)` DB write fails | — | Logged ERROR; destroy aborted |
| Orphan `destroy_by_path` error | `DaemonError::Process` | Logged; sweep continues |

---

## Performance

| Operation | Cost |
|---|---|
| Non-sandboxed fast path | Zero — `allocation.as_ref()` pattern; no allocator code runs when `config.sandbox = None` |
| `git worktree add` | ~100 ms (repo-size dependent) |
| `git worktree remove` + branch delete | ~30 ms |
| Reference-count scan | O(active + completed session count) — acceptable at low-hundreds scale |
| Orphan sweep | O(live DB rows + on-disk UUID dirs) — runs once at startup |

---

## File Map

**Daemon (`crates/rsid/src/`):**

| File | Purpose |
|---|---|
| `sandbox/mod.rs` | `SandboxAllocator` — `allocate`, `destroy`, `destroy_by_path`, `list_on_disk`, `ensure_base` |
| `sandbox/git_worktree.rs` | Git-worktree backend — shell commands, pre-flight, idempotency |
| `session/launch.rs` | Allocation call, `config.working_dir` mutation, `sandbox_orphan_sweep` |
| `session/lifecycle.rs` | `maybe_destroy_sandbox`, `SandboxSnapshot`, callers (archive/delete/purge) |
| `session/rotation.rs` | Child session sandbox inheritance; `spawn_working_dir` resolution |
| `store/mod.rs` | V39 migration block; `add_column_if_not_exists` |
| `store/sessions.rs` | `insert_session` columns; `list_live_sandbox_owners`; `update_sandbox_cleanup_state` |
| `store/row_mappers.rs` | `sandbox_kind_to_str`, `str_to_sandbox_kind`, cleanup state converters |
| `bus.rs` | `DaemonEvent::SandboxOrphanCleaned` |
| `config.rs` | `Config.sandbox_base`; `RSI_SANDBOX_BASE` env var parsing |

**Shared types (`crates/rsi-common/src/`):**

| File | Purpose |
|---|---|
| `types.rs` | `SandboxKind`, `SandboxCleanupState`, `SandboxSpec`; four `sandbox_*` fields on `Session` |
| `rpc.rs` | `LaunchSessionParams.sandbox`; `DaemonCapabilities.sandbox` |

**TUI (`crates/rsi/src/`):**

| File | Purpose |
|---|---|
| `overlay/prompt.rs` | `sandbox_spec_from_bool`; submit extraction; `sandbox_enabled` default `false` |
| `overlay/mod.rs` | `s` key handler; capability gate; `OverlayState::Prompt.sandbox_enabled` |
| `ui/overlay/prompt.rs` | `[s]andbox: on\|off` footer hint rendering |
| `ui/session.rs` | `is_sandboxed` flag; `⊡` indicator; sandbox detail-view header |
| `client.rs` | `sandbox: Option<SandboxSpec>` param in `launch_blank` / `launch_taskrabbit` |
| `poll_controller.rs` | `PollState.sandbox_supported: bool` |
| `app/polling.rs` | `poll.sandbox_supported = caps.sandbox` on capabilities response |

**Tests (`crates/rsid/tests/`):**

| File | Covers |
|---|---|
| `sandbox_isolation.rs` | Filesystem isolation — writes invisible to canonical repo; concurrent sessions independent |
| `sandbox_cleanup.rs` | Lifecycle hooks (archive/delete/purge), idempotency, rotation ref-counting, orphan sweep |
| `sandbox_e2e.rs` | Write → destroy → file gone; two concurrent sessions same path; cleanup-state contract |
| `sandbox_noop.rs` | Non-sandboxed fast path — zero sandbox fields; `RSI_SANDBOX_BASE` untouched |

---

## Design Invariants

1. **`Session.working_dir` is always the canonical repo root.** `sandbox_root` is
   a separate field. The single mutation `config.working_dir = effective_working_dir`
   in `session/launch.rs` is the only point that changes subprocess cwd.

2. **Rotation child inherits the parent's sandbox.** All four `sandbox_*` fields
   are copied verbatim; `LaunchConfig.sandbox = None` prevents re-allocation.

3. **Per-session subdir layout.** Every sandbox lives at
   `$RSI_SANDBOX_BASE/<session-uuid>/`. Two sessions against the same repo each
   get their own independent worktree.

4. **Fail-closed launch.** Allocation precedes Session row insertion. If
   `allocate()` returns `Err`, `launch_session` propagates it and no row is
   created. A sandboxed session that silently degraded to the canonical directory
   would be a security downgrade — the allocator never degrades.

---

## Known Limitations

- **`Bubblewrap` / `SystemdNspawn` variants** reserved in `rsi-common/src/types.rs`
  but commented out. Re-enabling requires no migration.

- **Reconciliation loop deferred.** A continuous sweep of `Failed`-state rows was
  planned but not shipped. `Failed` rows are retried only on daemon restart via the
  orphan sweep.

- **`destroy_by_path` stale worktree metadata.** Disk-orphan sweep cannot call
  `git worktree remove` without knowing the origin; stale `.git/worktrees/<name>`
  entries persist until a future `git worktree prune` against the origin.

---

## References

- Detailed implementation doc: `docs/agent-sandbox.md`
- Plan: `thoughts/shared/plans/2026-04-19-agent-sandbox-feature.md`
- Ticket: `thoughts/shared/tickets/rsi-harness/RSI-019_agent_sandbox_git_worktree.md`
- Keybindings: `docs/keybindings.md`
