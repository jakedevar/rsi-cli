# Agent Sandbox (RSI-019)

## Overview

Agent sandbox is an opt-in, filesystem-only isolation layer that allocates a `git
worktree` checkout for each sandboxed session before the provider subprocess is
spawned. The subprocess runs with `current_dir` set to the worktree root; the
canonical `working_dir` is never touched. The feature is capability-gated in the
TUI (requires the daemon to advertise `DaemonCapabilities.sandbox = true`) and
defaults to off at every launch site.

Shipped across five commits on main:

| Commit | Phase | Content |
|---|---|---|
| `b06c959` | 1 | `rsi-common` data model + V39 migration + rotation inheritance |
| `4fb696f` | 2 | Daemon allocator, launch wiring, RPC pass-through |
| `122bf92` | 3 | Cleanup hooks, orphan sweep, `maybe_destroy_sandbox` |
| `5fdd8e0` | 4 | TUI prompt toggle, session-list indicator, detail-view header line |
| `4debd2d` | 5 | E2E integration tests + this doc |

---

## Design Invariants

1. **`Session.working_dir` is always the canonical repo root.** `sandbox_root` is
   a separate field. Provider subprocesses pick up the sandbox via
   `config.working_dir` mutation in `session/launch.rs` — nothing else changes.

2. **Rotation child inherits the parent's sandbox.** `rotate_session` and
   `rotate_completed_session` copy all four `sandbox_*` fields from the parent
   `Session` row into the child row. The child's `LaunchConfig.sandbox` is set to
   `None` to prevent re-allocation — the existing worktree is reused.

3. **Per-session subdir layout.** Every sandbox lives at
   `$RSI_SANDBOX_BASE/<session-uuid>/`. UUIDs are independent across sessions and
   repos; two sessions against the same repo each get their own worktree.

4. **Fail-closed launch.** Allocation happens before the `Session` row is inserted.
   If `allocate()` returns `Err`, `launch_session` propagates that error and no
   session is created. A sandbox-requested session that silently ran in the
   canonical directory would be a security downgrade; the allocator never degrades.

5. **D00 cleanup clamp.** Every production cleanup entry point reaches one
   daemon-local, read-only decision boundary before its first mutation. D00 has
   no positive cleanup proof, so every real sandbox candidate is retained.
   `NoCleanupRequired` means only that a durable row is non-sandboxed or is an
   already complete `Purged` tombstone; it is not cleanup authorization.

---

## Data Model

### `sessions` table — new columns (V39)

| Column | SQLite type | Nullable | Rust type |
|---|---|---|---|
| `sandbox_kind` | `TEXT` | YES | `Option<SandboxKind>` |
| `sandbox_root` | `TEXT` | YES | `Option<PathBuf>` |
| `sandbox_branch` | `TEXT` | YES | `Option<String>` |
| `sandbox_cleanup_state` | `TEXT` | YES | `Option<SandboxCleanupState>` |

All four default to `NULL`; pre-V39 rows carry `NULL` which deserializes to
`None`. Every field on `Session` uses `#[serde(default)]` for backward compat.

### Shared types (`rsi-common::types`)

**`SandboxKind`** — serialized as the variant name string.

| Variant | String | Meaning |
|---|---|---|
| `None` | `"None"` | No sandbox (default); fast path — zero allocations |
| `GitWorktree` | `"GitWorktree"` | `git worktree add` filesystem isolation |

`Bubblewrap` and `SystemdNspawn` are commented out as reserved variants in
`types.rs`; see §Known Limitations.

**`SandboxCleanupState`** — serialized as the variant name string.

| Variant | String | Meaning |
|---|---|---|
| `Live` | `"Live"` | On-disk sandbox exists; owned by an active/completed session |
| `Purged` | `"Purged"` | A historical destroy was confirmed and its root/ref metadata was tombstoned |
| `Failed` | `"Failed"` | A historical destroy attempt may have left partial state |

D00 adds no `Blocked` cleanup-state value. Blocking is deterministically
recomputed from the unchanged durable sandbox tuple on every attempt and after
restart. `Live`, `Failed`, and `Purged` keep their existing serialized meanings.

**`SandboxSpec`** — carried by `LaunchSessionParams.sandbox: Option<SandboxSpec>`.

| Field | Type | Default | Meaning |
|---|---|---|---|
| `kind` | `Option<SandboxKind>` | `None` → resolves to `GitWorktree` at launch | Which backend to use |
| `branch` | `Option<String>` | `None` → `rsi/<8-char-uuid-short>` | Branch name override |

---

## V39 Migration

The migration block (`store/mod.rs`, `if version < 39`) adds four nullable `TEXT`
columns to `sessions` via `add_column_if_not_exists`. Each call checks
`pragma_table_info('sessions')` before issuing `ALTER TABLE`, so the block is safe
to run against a database that already has some or all of the columns (idempotent).

On success, `PRAGMA user_version` is set to `39`.

**Rollback:** pre-V39 daemons ignore the nullable columns. A V39-migrated database
opened by a pre-V39 daemon works correctly because all existing queries omit the
new columns. To force the migration to re-run upward from V38:

```sql
PRAGMA user_version = 38;
```

---

## Configuration

| Env var | Default | Created | Permission |
|---|---|---|---|
| `RSI_SANDBOX_BASE` | `~/.rsi/sandboxes/` | At daemon startup (`ensure_base`) | `0o700` |

The macro `env_var_legacy!("SANDBOX_BASE")` also accepts `MOTHERSHIP_SANDBOX_BASE`
and `FLYWHEEL_SANDBOX_BASE` for legacy compat. If `$HOME` is unresolvable, falls
back to `/tmp/rsi-sandboxes`.

`ensure_base` is idempotent — safe to call concurrently. The `chmod 0o700` logs at
`WARN` and continues on failure (e.g. tmpfs mounts that don't support Unix perms).

To reduce SSD wear, point `RSI_SANDBOX_BASE` at a tmpfs:

```bash
export RSI_SANDBOX_BASE=/dev/shm/rsi-sandboxes
```

---

## Lifecycle — Allocation

```
LaunchSessionParams.sandbox = Some(SandboxSpec { kind: Some(GitWorktree), .. })
   │
   ▼ launch_session (session/launch.rs)
   1. Resolve working_dir (canonicalize, fallback to cwd)
   2. SandboxAllocator::allocate(session_id, &working_dir, kind)
      ├── ensure_base() — create $RSI_SANDBOX_BASE with 0o700
      ├── git -C origin rev-parse --is-inside-work-tree
      │     non-zero → DaemonError::InvalidParam("sandbox requires a git repository at '...'")
      ├── proposed_root = base_dir / session_id (UUID string)
      ├── path_safety::canonicalize_non_strict(proposed_root)
      │     rejects symlink cycles / invalid UTF-8
      ├── abort if root already exists (orphan guard)
      ├── branch = "rsi/<first-8-hex-chars-of-uuid>"
      └── git -C origin worktree add -b <branch> --quiet <root>
            non-zero → DaemonError::Process(stderr)
   3. On Err → return Err, NO Session row inserted (fail-closed)
   4. On Ok → effective_working_dir = allocation.root
   5. config.working_dir = Some(effective_working_dir)    ← providers pick this up
   6. Session { sandbox_kind: Some(kind), sandbox_root: Some(root),
                sandbox_branch: Some(branch),
                sandbox_cleanup_state: Some(Live), working_dir: canonical }
   7. persist.insert_session (all four sandbox_* columns populated)
```

`origin` on the `SandboxAllocation` is not stored in the `Session` row. At destroy
time it is reconstructed from `Session.working_dir`.

---

## Lifecycle — Rotation and Continue

**Rotation** (`session/rotation.rs`, `spawn_rotation_child` path):

- The child `Session` is built with `sandbox_kind/sandbox_root/sandbox_branch/
  sandbox_cleanup_state` copied verbatim from the parent row.
- `LaunchConfig.sandbox = None` prevents `launch_session` from allocating a new
  worktree.
- `spawn_working_dir` resolves to `child_session.sandbox_root` if `Some`, else
  `child_session.working_dir` — the child subprocess runs in the same worktree.
- Cleanup is independently classified for every row. Shared lineage ownership
  is one blocked reason, but becoming the last owner does not authorize cleanup.

**Continue** (`ContinueSession` RPC): the session is loaded from the DB; sandbox
fields carry forward automatically because they live on the persisted `Session` row.
No extra wiring required.

---

## Lifecycle — Cleanup

`sandbox::cleanup` owns the shared D00 candidate, decision, and blocked-reason
types. It has only two outcomes:

- `NoCleanupRequired` for a durably verified non-sandbox row or a complete
  tombstone with no remaining root or branch.
- `Blocked(reason)` for every real or uncertain candidate.

There is deliberately no `Eligible`, `Authorized`, proof token, boolean
override, destructive callback, or unchecked constructor. The observer reads
only the in-memory and durable ownership witnesses needed to reject shared or
drifting rows. A complete, exclusively owned worktree ends at
`missing_independently_verified_proof` before production classification invokes
Git, traverses the worktree, or runs configured Git helpers. More detailed Git
identity diagnostics remain isolated test machinery and cannot authorize a
lifecycle mutation.

Explicit archive, delete, purge, and `mark_pending_archive(true)` return a
stable policy-denied error for a real sandbox. The check happens before process
reaping, lead-pointer changes, retry cancellation, map eviction, lifecycle or
cleanup-state writes, success events, filesystem changes, Git worktree changes,
or ref changes. Clearing an existing pending-archive marker remains available
as a recovery action.

Normal terminalization may persist its truthful `Completed`, `Failed`, or
`Interrupted` outcome. If pending auto-archive was requested, the archive
sub-transition is classified separately and remains pending when blocked:
hierarchy, sandbox fields, cleanup state, filesystem, registration, and refs
remain unchanged.

---

## Lifecycle — Startup and Launch-Abort Retention

Startup still enumerates typed `Live` owners and UUID-named paths, but the pass
is classification-only:

1. Archived and deleted typed owners are re-read and passed through the shared
   clamp before any historical `Failed` stamp.
2. A failed row re-read becomes an attributed `RowReadFailure`; it never falls
   back to path-only removal.
3. A UUID path absent from the initial `Live` owner set becomes a `PathOnly`
   candidate. Absence, `Failed`, or `Purged` state does not imply disposability.
4. Every real candidate is logged as retained. No cleanup state, tombstone,
   success event, directory, worktree registration, or ref is changed.

A launch can abort after allocation but before a durable session row exists.
The launch guard classifies that allocation as `LaunchAbortWithoutOwner` and
retains the root, registration, branch, and content. A later startup sees the
same UUID path and retains it again.

Restore-time pending archive follows the same rule: a sandboxed row remains
pending and unarchived before any hierarchy or archive write.

### Operator recovery boundary

Preserve the retained worktree and back it up if the contents matter. V94/V95
source-worktree cohort settlement is the separate, reviewed cleanup path: from
the TUI daemon-features settings, audit the selected repository cohort, inspect
every retained and eligible item, then type the exact phrase generated by that
fresh audit. Dirty, unique/non-ancestor, active, held, or uncertain work remains
retained. See `docs/cohort-settlement.md`.

Do not manually delete the branch, recursively remove the root, prune away the
registration as a cleanup substitute, guess an integration target, accept
patch similarity as proof, create an agent-authored discard, or bypass the
guard.

Generic lifecycle cleanup remains retention-only. Cohort settlement is
operator-only, local-only, journaled, and exact-phrase-bound; it is absent from
agent verbs and does not grant archive/delete/purge a deletion capability.

Ordinary archive has one separate branch-preserving positive path for an exact
terminal, current, exclusive leaf. It accepts only `no_output` or exact
`integrated_ancestor`, journals before Git, and leaves every other case under
D00 retention. See [Safe cleanup during ordinary archive](archive-cleanup.md).

---

## RPC Surface

| Element | Location | Detail |
|---|---|---|
| Launch param | `LaunchSessionParams.sandbox: Option<SandboxSpec>` (`rsi-common/rpc.rs`) | `None` = non-sandboxed (zero wire overhead) |
| Capability flag | `DaemonCapabilities.sandbox: bool` (`rsi-common/rpc.rs`) | `true` in `Default::default()`; TUI gates the toggle on this |
| Bus event | `DaemonEvent::SandboxOrphanCleaned { session_id: Option<Uuid>, sandbox_root: PathBuf }` (`rsid/src/bus.rs`) | `event_type = "sandbox_orphan_cleaned"` |

`GetDaemonCapabilities` RPC is the negotiation point; the TUI reads
`caps.sandbox` into `poll.sandbox_supported` on every capabilities response and
only shows the `Ctrl+B: sandbox` toggle when it is `true`.

---

## TUI Exposure

### Launch-prompt overlay

Key `s` toggles `sandbox_enabled` on the focused `OverlayState::Prompt`. The
toggle is a no-op when `app.poll.sandbox_supported` is `false` (daemon older than
V39, or capability not advertised).

Applies to `PromptPurpose::Blank` and `PromptPurpose::TaskRabbit`. Does not apply
to `ContinueSession` overlays (continuing always reuses the existing row's sandbox
state).

Footer hint (normal mode only, capability-gated):

```
Ctrl+B: sandbox on     ← when enabled
Ctrl+B: sandbox off    ← when disabled
```

On submit, `sandbox_spec_from_bool(sandbox_enabled)` produces
`Some(SandboxSpec { kind: Some(GitWorktree), branch: None })` when on, `None` when
off. This is passed to `client.launch_blank()` or `client.launch_taskrabbit()`.

### Session-list indicator

Character `⊡` (U+22A1) rendered in `theme::teal()` at the left margin of the
session card, after the pinned/testing-needed/rotation-disabled icons. Derived from
`session.sandbox_kind.is_some()`.

### Session-detail header

When `session.sandbox_root.is_some()` and `scroll_offset == 0`, a one-line header
is inserted below the issue URL line:

```
⊡ sandbox: ~/.rsi/sandboxes/<uuid>  (rsi/<short>)
```

Root path abbreviates `$HOME` to `~`. Branch shown parenthetically in
`theme::overlay2()`.

See `docs/keybindings.md` for the full overlay key reference.

---

## Failure Modes and Error Surfacing

| Failure | Error type | Propagation |
|---|---|---|
| `working_dir` not inside a git repo | `DaemonError::InvalidParam("sandbox requires a git repository at '...'")` | Returned from `launch_session` → RPC error to TUI |
| `git worktree add` non-zero exit | `DaemonError::Process(stderr)` | Same — fail-closed, no Session row |
| `canonicalize_non_strict` rejects path | `DaemonError::InvalidParam` | Same |
| Sandbox root already exists | `DaemonError::InvalidParam("sandbox root already exists at '...' (possible orphan)")` | Same |
| Real sandbox archive/delete/purge | `DaemonError::PolicyDenied` | Returned before mutation with a stable `sandbox cleanup blocked: <reason>` detail |
| `mark_pending_archive(true)` for a real sandbox | `DaemonError::PolicyDenied` | In-memory and durable pending markers remain unchanged |
| Missing/unreadable/inconsistent row | blocked classification | No fail-open no-sandbox default and no path-only fallback |
| Launch abort after allocation | structured retained warning | Allocation remains available for recovery |
| Startup typed/path-only candidate | structured retained warning | Row, directory, registration, refs, and events remain unchanged |

---

## Performance

- **Non-sandboxed fast path:** `allocation.as_ref()` pattern in `launch_session` —
  no allocator code runs when `config.sandbox` is `None`. Zero overhead for the
  default case.
- **Allocation:** ~100 ms per `git worktree add` (plan estimate; actual depends on
  repo size and disk).
- **Cleanup classification:** two bounded ownership observations plus two
  read-only Git identity observations for a row-backed candidate. No
  destructive subprocess or filesystem call is scheduled.
- **Startup classification:** O(live DB rows + on-disk UUID dirs). Runs once at
  daemon startup and retains every real candidate.

---

## Testing

### Integration test files

| File | Covers |
|---|---|
| `crates/rsid/tests/sandbox_isolation.rs` | Sandboxed writes invisible to canonical repo; two concurrent sandboxed sessions writing same relative path produce independent contents |
| `crates/rsid/tests/sandbox_cleanup.rs` | Full filesystem/hash/ref/worktree/SQLite invariance snapshots for explicit lifecycle, dirty/missing/changed ref, sharing, inconsistent tuples, restore pending, typed startup, path-only/non-Live residuals, resumable terminal states, and no-target behavior |
| `crates/rsid/tests/sandbox_e2e.rs` | Dirty-content retention and two concurrent sandboxed sessions with independent contents |
| `crates/rsid/tests/sandbox_noop.rs` | Non-sandboxed sessions have no sandbox fields; `RSI_SANDBOX_BASE` dir untouched; serialized JSON has no non-null `sandbox_*` keys |

### Unit tests

| Test | Location | Asserts |
|---|---|---|
| `migration_v39_idempotent` | `rsid::store::tests` | V38→V39 migration adds four columns; second open is a no-op |
| `session_serde_sandbox_roundtrip` | `rsi-common::types::tests` | Full sandbox field set round-trips through JSON serde |
| `session_serde_backcompat` | `rsi-common::types::tests` | Old JSON (no sandbox keys) deserializes to all-`None` sandbox fields |
| `sandbox_allocate_happy_path` | `rsid::sandbox::tests` | Allocator produces `GitWorktree` allocation on valid git repo; root exists; branch prefixed `rsi/` |
| `sandbox_allocate_non_git_rejected` | `rsid::sandbox::tests` | Non-git directory returns `InvalidParam` containing `"git"` |
| `sandbox_destroy_removes_worktree_and_branch` | `rsid::sandbox::tests` | Root dir removed; branch deleted from origin after destroy |
| `sandbox_destroy_idempotent` | `rsid::sandbox::tests` | Second destroy call returns `Ok` |
| `launch_sandbox_columns_populated` | `rsid::sandbox::tests` | Allocation maps 1:1 to `Session.sandbox_*` columns; no allocation → all `None` |
| `sandbox_list_on_disk_missing_base_returns_empty` | `rsid::sandbox::tests` | Missing base dir returns empty Vec, no error |
| `launch_abort_guard_retains_root_registration_ref_and_content` | `session::launch::tests` | RAII drop classifies and retains an allocation before durable ownership |
| `startup_row_read_failure_candidate_is_inert` | `session::launch::tests` | Failed typed-row re-read cannot reach path-only removal |
| `pending_archive_admission_blocks_before_memory_or_store_mutation` | `session::lifecycle::d00_tests` | Pending admission leaves memory, SQLite, filesystem, registration, and ref unchanged |
| `terminal_pending_auto_archive_commits_truth_but_not_archive_cleanup` | `session::lifecycle::d00_tests` | Truthful terminal status persists while archive intent and sandbox remain |
| `real_ref_drift_is_barrier_controlled_and_classifier_is_inert` | `sandbox::cleanup::tests` | Barrier-controlled concurrent ref drift is detected without sleeps or daemon mutation |
| D00 integration matrix | `rsid::tests::sandbox_cleanup` | Explicit, startup, adverse-state, and no-target invariants described above |
| `session_row_sandbox_indicator` | `rsi::ui::session::tests` | `is_sandboxed = false` when `sandbox_kind = None`; `true` when `Some(GitWorktree)` |

---

## File Map

**Daemon (`crates/rsid/src/`):**

| File | Purpose |
|---|---|
| `sandbox/cleanup.rs` | Shared D00 candidate/decision/reason boundary; production rows stop before Git/path observation |
| `sandbox/mod.rs` | `SandboxAllocator` allocation/enumeration; raw destruction is reachable only inside the sandbox module and exercised only by isolated tests |
| `sandbox/git_worktree.rs` | Private Git-worktree backend; production session modules cannot reach destructive primitives |
| `session/launch.rs` | Allocation call, `config.working_dir` mutation, `sandbox_orphan_sweep` |
| `session/lifecycle.rs` | Durable row resolution, ownership observation, and guarded lifecycle callers |
| `session/rotation.rs` | Child session sandbox inheritance; `spawn_working_dir` resolution |
| `store/mod.rs` | V39 migration block; `add_column_if_not_exists` |
| `store/sessions.rs` | `insert_session` column list; `list_live_sandbox_owners`; `update_sandbox_cleanup_state` |
| `store/row_mappers.rs` | `sandbox_kind_to_str`, `str_to_sandbox_kind`, `sandbox_cleanup_state_to_str`, `str_to_sandbox_cleanup_state` |
| `bus.rs` | `DaemonEvent::SandboxOrphanCleaned`; `event_type = "sandbox_orphan_cleaned"` |
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
| `overlay/mod.rs` | `s` key handler; `sandbox_caps` gate; `OverlayState::Prompt.sandbox_enabled` |
| `ui/overlay/prompt.rs` | `Ctrl+B: sandbox on|off` footer hint rendering; capability gate |
| `ui/session.rs` | `is_sandboxed` flag; `⊡` indicator in session list; sandbox detail-view header |
| `client.rs` | `sandbox: Option<SandboxSpec>` param in `launch_blank` / `launch_taskrabbit` |
| `poll_controller.rs` | `PollState.sandbox_supported: bool` field |
| `app/polling.rs` | `poll.sandbox_supported = caps.sandbox` on capabilities response |

**Tests (`crates/rsid/tests/`):**

| File | Purpose |
|---|---|
| `sandbox_isolation.rs` | Filesystem isolation property |
| `sandbox_cleanup.rs` | Phase 3 lifecycle hooks + orphan sweep |
| `sandbox_e2e.rs` | Phase 5 end-to-end writes and concurrent sessions |
| `sandbox_noop.rs` | Zero-regression non-sandboxed fast path |

**Docs:**

| File | Purpose |
|---|---|
| `docs/agent-sandbox.md` | This document |

---

## Known Limitations / Follow-Ups

- **`Bubblewrap` / `SystemdNspawn` variants** are commented out as reserved in
  `rsi-common/src/types.rs`. The enum is serializable with them commented out;
  re-enabling them in a future version requires no migration — new string values
  simply won't match existing `GitWorktree` rows.

- **Proof-aware cleanup is intentionally absent from generic lifecycle.** D00
  does not represent an integration receipt, operator discard, source-to-target
  mapping, or post-head verification. V94/V95 cohort settlement owns those
  proofs at a separate operator-only boundary.

- **Harness dual-tree:** both `crates/rsid/src/harness/tools/` and
  `crates/rsid/src/session/harness/tools/` work correctly without sandbox-specific
  edits because both resolve their working directory through `config.working_dir`,
  which the Phase 2 mutation (`config.working_dir = Some(effective_working_dir)`)
  updates before any provider is spawned. The single mutation point covers both
  trees.

- **Retained state consumes disk.** This is the deliberate fail-closed cost for
  work that does not pass a fresh cohort audit. Missing ownership or metadata
  is a reason to retain, never a reason to delete. Target-cache reclaim is
  independent; source-root admission still needs a separate bounded-pressure
  follow-up.

---

## References

- Plan: `thoughts/shared/plans/2026-04-19-agent-sandbox-feature.md`
- Research: `thoughts/shared/research/2026-04-19-agent-sandbox-feature.md`
- Ticket: `thoughts/shared/tickets/rsi-harness/RSI-019_agent_sandbox_git_worktree.md`
- Keybindings: `docs/keybindings.md`
