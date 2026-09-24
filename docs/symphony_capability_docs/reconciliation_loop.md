# Reconciliation Loop

## Overview

The reconciliation loop is a background task in `rsid` that periodically validates that in-memory session state matches subprocess reality and SQLite persistence. It solves two distinct problems:

1. **Zombie sessions** — sessions whose provider subprocess has exited (or never had a process handle) but remain marked as `Running`, `Starting`, or `WaitingApproval` in the in-memory `active` map.
2. **State drift** — sessions marked as active in SQLite but absent from the in-memory active map (or vice versa), which can happen after a daemon crash or a failed write.

Additionally, the stall detector (a related background task in `stall_detector.rs`) handles a third class of problems: sessions that are alive but have produced no output for a configurable duration.

## Architecture

The system has two independent timer-driven sub-tasks that run inside a single `tokio::spawn` loop:

- **Sub-step A — Liveness check** fires every `liveness_interval_secs` seconds (default: 120). It snapshots all active sessions, checks each provider process, and signals finalization for any dead or missing processes.
- **Sub-step B — Store consistency check** fires every `consistency_interval_secs` seconds (default: 600). It is implemented as a counter: after every `consistency_interval_secs / liveness_interval_secs` liveness ticks (default: 5), it compares the active map against SQLite and marks orphaned store records as `Failed`.

Both sub-steps skip sessions whose `RotationCoordinator` is in an active rotation phase (`PendingInterrupt` or `WritingHandoff`) to avoid interrupting mid-handoff state.

The stall detector runs as a completely separate `tokio::spawn` loop on a 60-second interval and is configured from the same `StallAction` values.

## Configuration

All configuration is read from environment variables by `Config::from_env()` in `crates/rsid/src/config.rs`.

| Environment Variable | Type | Default | Description |
|---|---|---|---|
| `MOTHERSHIP_RECONCILIATION_ENABLED` | bool (`0`/`false`/`no`/`off` to disable) | `true` | Enable or disable the reconciliation loop entirely |
| `MOTHERSHIP_RECONCILIATION_LIVENESS_SECS` | `u64` | `120` | Interval between process liveness checks (seconds) |
| `MOTHERSHIP_RECONCILIATION_CONSISTENCY_SECS` | `u64` | `600` | Interval between SQLite consistency checks (seconds) |
| `MOTHERSHIP_RECONCILIATION_STALL_ACTION` | `"notify"` \| `"interrupt"` \| `"interrupt_and_retry"` | `"notify"` | Action for standard (interactive) sessions when stalled |
| `MOTHERSHIP_RECONCILIATION_STALL_ACTION_UNATTENDED` | `"notify"` \| `"interrupt"` \| `"interrupt_and_retry"` | `"interrupt_and_retry"` | Action for unattended (`TaskRabbit`/`Bug`) sessions when stalled |

The stall detector also reads these additional variables:

| Environment Variable | Type | Default | Description |
|---|---|---|---|
| `MOTHERSHIP_STALL_DETECTION_ENABLED` | bool | `true` | Enable or disable stall detection |
| `MOTHERSHIP_STALL_TIMEOUT_RUNNING_SECS` | `u64` | `1800` | Idle threshold for `Running`/`Starting` sessions (30 min) |
| `MOTHERSHIP_STALL_TIMEOUT_WAITING_SECS` | `u64` | `3600` | Idle threshold for `WaitingApproval` sessions (60 min) |

## Key Components

### Liveness Checking

`collect_liveness_snapshots()` acquires a **write lock** on the active sessions map (required because `try_wait()` mutates process state) and builds a `Vec<LivenessSnapshot>` containing:

- `session_id: Uuid`
- `status: SessionStatus`
- `has_process: bool` — whether `TrackedSession.process` is `Some`
- `is_alive: bool` — result of `ProviderProcess::is_alive()`
- `is_rotating: bool` — whether `RotationCoordinator` is in `PendingInterrupt` or `WritingHandoff` state

`ProviderProcess::is_alive()` in `crates/rsid/src/session/types.rs` is a non-blocking check:
- For subprocess providers (`Claude`, `Codex`, `OpenCode`, `Gemini`, `Nullclaw`, `CodexAppServer`): calls `try_wait()` and returns `true` if no exit status is available yet.
- For task-based providers (`Local`, `Harness`): calls `is_finished()` on the `JoinHandle` and returns `true` if the task has not finished.

The liveness loop then classifies each snapshot:
- Sessions with status other than `Running`, `Starting`, or `WaitingApproval` are skipped.
- Sessions with `is_rotating = true` are skipped with a debug log.
- Sessions where `has_process = true` and `is_alive = false` are dead processes — `signal_finalization()` is called.
- Sessions where `has_process = false` (no process handle at all) are zombies — `signal_finalization()` is called.

### Finalization Signaling

`signal_finalization()` handles dead and zombie sessions without setting `interrupt_requested = true` (which would classify the termination as user-initiated). It:

1. Acquires a write lock on the active map.
2. Calls `tracked.stop_tx.try_send(())` to signal the session's monitor loop to exit, which triggers `finalize_session()`.
3. Publishes a `DaemonEvent::SessionReconciled` bus event with `reason: ReconciliationReason::ProcessDied` and `new_status: SessionStatus::Failed`.

Because `interrupt_requested` is not set, the monitor loop classifies the exit as a non-user-initiated failure, which keeps stall-interrupted sessions eligible for retry.

### Store Consistency

`reconcile_store_consistency()` detects drift between the in-memory active map and SQLite:

1. Collects all `Uuid` keys from the active map under a read lock.
2. Queries SQLite via `store.load_active_session_ids()` for sessions with active-like statuses.
3. For sessions in SQLite but not in memory: calls `store.update_session_status(session_id, SessionStatus::Failed)` directly and publishes `DaemonEvent::SessionReconciled` with `reason: ReconciliationReason::StoreDesync`. The `old_status` field is approximated as `Running` since an extra query is not performed.
4. For sessions in memory but not in SQLite (unusual — likely a pending write): logs a warning and publishes `DaemonEvent::SystemMessage` with level `"warn"`.

### Stall Auto-Remediation

The stall detector in `crates/rsid/src/stall_detector.rs` runs on a 60-second interval. It scans active sessions under a read lock and computes idle duration as `now - tracked.last_event_at`. Sessions exceeding the threshold trigger one of three `StallAction` behaviors:

- **`Notify`** — publishes `DaemonEvent::SessionStalled { session_id, status, idle_secs }`. No process interruption.
- **`Interrupt`** — calls `process.interrupt()` and `stop_tx.try_send(())`, then publishes `DaemonEvent::SessionReconciled` with `reason: ReconciliationReason::StallRemediation`. No retry is queued.
- **`InterruptAndRetry`** — same as `Interrupt` plus `retry_tx.try_send(session_id)` to queue a retry. Also publishes `DaemonEvent::SessionReconciled` with `reason: ReconciliationReason::StallRemediation`.

The stall action chosen depends on session kind: `TaskRabbit` and `Bug` sessions use `unattended_action`; all other kinds use `standard_action`.

Sessions in active rotation phases (`PendingInterrupt` or `WritingHandoff`) are skipped by the stall detector, matching the liveness loop's skip logic.

The stall detector tracks a `reported: HashSet<Uuid>` to suppress duplicate events — only the first threshold crossing fires a bus event per session. The set is pruned every tick to remove sessions that are no longer active.

The `retry_tx: mpsc::Sender<Uuid>` parameter was added to `spawn_stall_detector()` to enable `InterruptAndRetry` to queue retries via the same retry channel used by the main session monitor loop.

### Startup Reconciliation

`restore_sessions()` in `crates/rsid/src/session/launch.rs` runs once at daemon startup. It loads all sessions from SQLite and for any session whose stored status is `Running`, `Starting`, or `WaitingApproval` (meaning the daemon crashed while they were active), it:

1. Sets `session.status = SessionStatus::Failed`.
2. Calls `persistence.update_status(session.id, SessionStatus::Failed)` to update SQLite.
3. Publishes `DaemonEvent::SessionReconciled` with `reason: ReconciliationReason::ProcessDied` and the session's prior status as `old_status`.

Sessions with `pending_archive = true` are additionally auto-archived (`SessionStatus::Archived`) instead of `Failed`, and a `DaemonEvent::SessionArchived` event is published.

After loading, `restore_sessions()` re-queues retries for any `Failed` sessions that have remaining retry budget (`retry_attempt < max_retries`), with exponential backoff.

## Reconciliation Reasons

`ReconciliationReason` is defined in `crates/rsid/src/reconciliation.rs`. It is `Serialize`/`Deserialize` (JSON serde) and sent as part of `SessionReconciled` bus events.

| Variant | When it fires |
|---|---|
| `ProcessDied` | Liveness check found a process handle that is no longer alive, a zombie session with no process handle, or `restore_sessions()` found sessions that were active when the daemon crashed |
| `StoreDesync` | Store consistency check found a session active in SQLite but absent from the in-memory active map |
| `StallRemediation` | Stall detector took `Interrupt` or `InterruptAndRetry` action on an idle session |

## Bus Events

`DaemonEvent::SessionReconciled` is defined in `crates/rsid/src/bus.rs`:

```rust
SessionReconciled {
    session_id: Uuid,
    old_status: SessionStatus,
    new_status: SessionStatus,
    reason: crate::reconciliation::ReconciliationReason,
}
```

The event is converted to a `BusEvent` with `event_type = "session_reconciled"` when transmitted to TUI subscribers. The `data` field is the JSON-serialized `DaemonEvent` using the `#[serde(tag = "type", content = "data")]` envelope.

## TUI Integration

The TUI handles `"session_reconciled"` push events in `apply_push_event()` in `crates/rsi/src/app/polling.rs`.

When a `session_reconciled` event arrives:

1. The `reason` field is extracted as a string from the raw `serde_json::Value`.
2. A notification message is composed in the format: `"<label>: reconciled (<old_status:?> → <new_status:?>, <reason_str>)"`, where `<label>` is the session's title or first 40 characters of the query.
3. The notification is pushed with `NotificationKind::Info` and `NotificationPriority::Medium`, linked to the session ID.
4. The session's cached status in `self.sessions` is updated to `new_status`.
5. Returns `true` to trigger a UI redraw.

## Source Files

| File | Role |
|---|---|
| `crates/rsid/src/reconciliation.rs` | `ReconciliationConfig`, `ReconciliationReason`, `StallAction`, `spawn_reconciliation_loop`, `collect_liveness_snapshots`, `signal_finalization`, `reconcile_store_consistency` |
| `crates/rsid/src/stall_detector.rs` | `StallConfig`, `spawn_stall_detector` — stall detection and remediation loop |
| `crates/rsid/src/session/types.rs` | `ProviderProcess::is_alive()`, `TrackedSession` fields used by reconciliation (`process`, `stop_tx`, `rotation`, `last_event_at`, `stall_interrupted`) |
| `crates/rsid/src/session/launch.rs` | `restore_sessions()` — startup reconciliation, crash recovery, pending-archive auto-archive |
| `crates/rsid/src/bus.rs` | `DaemonEvent::SessionReconciled` variant definition and `BusEvent` conversion |
| `crates/rsid/src/config.rs` | Environment variable parsing for all reconciliation and stall configuration fields |
| `crates/rsid/src/main.rs` | `spawn_reconciliation_loop` and `spawn_stall_detector` integration — conditional on `reconciliation_enabled` and `stall_detection_enabled` |
| `crates/rsi/src/app/polling.rs` | TUI `apply_push_event()` handler for `"session_reconciled"` events |
