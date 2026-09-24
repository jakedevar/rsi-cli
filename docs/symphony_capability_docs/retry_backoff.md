# Structured Retry with Exponential Backoff

## Overview

When a session fails due to a transient error — process crash, rate limit, API overload, or stall timeout — the daemon can automatically re-launch it after a computed delay. Each subsequent retry doubles the wait time starting from 10 seconds, capped by runtime config, so a short transient clears quickly while persistent problems back off gracefully.

Retry defaults are kind-scoped. `Story` and `Standard` sessions default to no automatic retry because retrying them can replay an interactive or orchestrator prompt. Worker kinds (`TaskRabbit`, `Task`, `Bug`, `Feature`, `Refactor`, `Research`) use the daemon worker default (`RSI_RETRY_MAX_DEFAULT`, default `3`). A session can still opt in or out explicitly with `LaunchSessionParams.max_retries`; explicit launch params are sovereign.

The retry counter and attempt number are persisted to SQLite so that a daemon restart does not silently drop a legitimate pending worker retry. A9 hardening makes restart recovery kind-gated, `retry_enabled`-gated, and convergence-safe by persisting the re-queue attempt bump.

## Architecture

The retry system spans three crates:

- **`rsi-common`** — `Session.retry_attempt` and `Session.max_retries` fields carry retry state in the shared wire format.
- **`rsid`** — All retry logic lives here: kind policy in `session/retry_policy.rs`, eligibility classification in `monitor.rs`, timer scheduling in `monitor.rs` and `launch.rs`, the `launch_retry()` and `cancel_retry()` methods in `lifecycle.rs`, the `CancelRetry` RPC handler in `rpc.rs`, and stall-triggered retry in `main.rs` and `lifecycle.rs`.
- **`rsi`** — The TUI reads `retry_attempt` / `max_retries` from the session and renders a badge in the session list card. A `session_retrying` bus event causes the TUI to push a notification and update the session state in memory.

The daemon uses a `tokio::sync::mpsc::Sender<Uuid>` channel (`retry_tx`) as the retry queue. Any subsystem that wants to schedule a retry (the monitor loop, stall detector, restart recovery) sends a session UUID into this channel. A dedicated `tokio::spawn` task in `main.rs` drains the channel and calls `session_manager.launch_retry(session_id)`.

```
Session fails
    └─ monitor.rs: classify_retry_eligibility()
           ├─ not eligible → no retry
           └─ eligible → backoff_ms(next_attempt)
                  └─ tokio::select! {
                         sleep(delay) → retry_tx.send(session_id)
                         cancel_rx    → log "Retry cancelled"
                     }
                         │
                   retry handler loop (main.rs)
                         │
                   session_manager.launch_retry(session_id)
                         │
                   launch_session() with new UUID, continued_from = original
```

## Configuration

All configuration is read from environment variables in `crates/rsid/src/config.rs`.

| Variable | Default | Description |
|---|---|---|
| `RSI_RETRY_MAX_DEFAULT` | `3` | Worker-kind default retry budget. `Story` and `Standard` still default to no retry unless launch params explicitly set `max_retries`. |
| `RSI_RETRY_MAX_BACKOFF_MS` | `120000` | Upper cap on backoff delay in milliseconds (2 minutes). |
| `RSI_RETRY_ON_STALL` | `true` | When `true`, the daemon starts the bus-driven stall retry handler at boot. |
| `RSI_RECONCILIATION_STALL_ACTION` | `notify` | Action for standard sessions on stall. Values: `notify`, `interrupt`, `interrupt_and_retry`. |
| `RSI_RECONCILIATION_STALL_ACTION_UNATTENDED` | `interrupt_and_retry` | Action for unattended sessions on stall. |

Legacy `MOTHERSHIP_*` and `FLYWHEEL_*` names are still accepted for retry variables.

`UpdateDaemonConfig` also exposes live runtime fields:

| Field | Effect |
|---|---|
| `retry_enabled` | Live kill-switch. `false` stops new retry arming and restart re-queueing, and zeroes `retry_max_default`. |
| `retry_max_default` | Live worker-kind default for future launches. Setting it to `0` also disables `retry_enabled`; setting it `>0` also re-enables `retry_enabled`. |
| `retry_max_backoff_ms` | Live cap used by newly armed retry timers. |

> **Re-enable note:** flipping `retry_enabled` back to `true` on its own does
> **not** restore `retry_max_default` — the `false`-flip zeroes the default, and
> the `true`-flip only sets the boolean, so worker retries stay at `0`. To
> re-enable worker retries after using the kill-switch, set `retry_max_default=N`
> (`N>0`); that restores the default **and** flips `retry_enabled=true` in one
> call.

Per-session overrides:

- `LaunchSessionParams.max_retries: Option<u8>` — overrides the kind default for one session.

## Key Components

### Backoff Formula

Defined in `crates/rsid/src/session/monitor.rs` (`backoff_ms()`):

```
delay = min(10_000 * 2^(attempt - 1), retry_max_backoff_ms)   [milliseconds]
```

| Attempt | Delay |
|---|---|
| 1 | 10 s |
| 2 | 20 s |
| 3 | 40 s |
| 4 | 80 s |
| 5+ | capped by `RSI_RETRY_MAX_BACKOFF_MS` |

`attempt` is 1-based. Edge case: `attempt=0` is treated identically to `attempt=1` (yields 10 s).

### Retry Eligibility Classification

`classify_retry_eligibility()` in `monitor.rs` returns `Some(reason_string)` if the failure is retryable, `None` if not. It is called only when `status == SessionStatus::Failed`, the runtime retry policy allows that session kind, `max_retries > 0`, and `current_attempt < max_retries`.

Decision table:

| Condition | Retryable | Reason string |
|---|---|---|
| `MonitorBreakReason::StallTimeout` | Yes | `"stall timeout"` |
| `MonitorBreakReason::Interrupted` (user-initiated) | No | — |
| `MonitorBreakReason::Rotation` | No | — |
| `MonitorBreakReason::Result` with meaningful output | No | — |
| Zero events received (startup crash) | Yes | `"process exited with zero events"` |
| No meaningful output, last events contain `401`/`unauthorized`/`auth` | No | — |
| No meaningful output, last events contain `permission denied` | No | — |
| No meaningful output, last events contain `rate limit`/`429` | Yes | `"rate limited (429)"` |
| No meaningful output, last events contain `overloaded`/`529` | Yes | `"API overloaded (529)"` |
| No meaningful output, no recognized pattern | Yes | `"no meaningful output produced"` |
| Non-zero exit code, last events contain `rate limit`/`429` | Yes | `"rate limited (429)"` |
| Non-zero exit code, last events contain `overloaded`/`529` | Yes | `"API overloaded (529)"` |
| Non-zero exit code, no recognized pattern | Yes | `"process exited with code N"` |

The classifier inspects up to the last 5 events of type `System` or `Message` for error content matching.

### Retry Pipeline

Full sequence from failure to re-launch:

1. Monitor loop exits, sets `break_reason`.
2. If `stall_interrupted` flag is set and `break_reason == Interrupted`, override to `StallTimeout`.
3. `finalize_session()` marks the session `Failed` (or another terminal status).
4. If the completed session has `status == Failed`, runtime retry is enabled for that kind, and `current_attempt < max_retries`: call `classify_retry_eligibility()`.
5. If eligible: compute `delay = backoff_ms(next_attempt)`, publish `DaemonEvent::SessionRetrying`, store `cancel_tx` in `CompletedSession.retry_cancel`, update `retry_attempt` in the completed map, call `persistence.update_retry_state()`.
6. Spawn a `tokio::select!` task: either sleep for `delay`, mark `retry_fired_at`, and send `session_id` to `retry_tx`, or cancel on `cancel_rx`.
7. The retry handler in `main.rs` drains `retry_rx` and calls `launch_retry(session_id)`.
8. `launch_retry()` serializes against `continue_session()` for the original id, re-reads the durable row, and only proceeds if the row is still `Failed` with remaining budget.
9. On success, `launch_retry()` builds a `LaunchConfig` from the original session's parameters, calls `launch_session()` with a new UUID, links the child with `continued_from`, persists retry state, exhausts the original row, and leaves a `superseded_by_retry` marker so a later user continue can interrupt the live clone before resuming the original.

The retry launches a **new session** (new UUID) linked to the original via `continued_from`. It is not a `--resume` continuation.

### Stall-Triggered Retry

Stall detection integrates with retry via two paths:

**Path 1 — `RSI_RETRY_ON_STALL=true` (main.rs stall-retry handler):**
- A dedicated task subscribes to the `EventBus`.
- On `DaemonEvent::SessionStalled`, it calls `session_manager.interrupt_if_stall_retryable(session_id)`.
- `interrupt_if_stall_retryable()` first checks runtime retry policy for the session kind and `max_retries > 0`. If either fails, it returns false without interrupting.
- If retries are configured, it sets `TrackedSession.stall_interrupted = true` then calls `interrupt_session()`.
- The monitor loop reads `stall_interrupted` during pre-finalization: if the flag is set and `break_reason == Interrupted`, it overrides to `MonitorBreakReason::StallTimeout`.
- `classify_retry_eligibility()` treats `StallTimeout` as unconditionally retryable.

**Path 2 — `StallAction::InterruptAndRetry` (stall_detector.rs):**
- The stall detector background task scans active sessions every 60 seconds.
- For sessions where the configured stall action is `InterruptAndRetry`, the detector can interrupt and enqueue retries via its `retry_tx` sender (passed in from `session_manager.retry_sender()`), subject to the same runtime kind gate.

### Daemon Restart Recovery

`restore_sessions()` in `launch.rs` runs at daemon startup:

1. Loads all sessions from SQLite into the `completed` map.
2. Sessions that were `Running` or `Starting` when the daemon died are marked `Failed`.
3. If `retry_enabled=false`, no restart recovery retries are armed.
4. For each failed worker-kind session where `max_retries > 0` and `retry_attempt < max_retries`: computes `backoff_ms(attempt + 1)`, publishes `DaemonEvent::SessionRetrying` with `reason: "daemon restart recovery"`, stores a new `cancel_tx` in the session's `CompletedSession`, persists the bumped `retry_attempt`, and spawns the delayed retry timer.

`Story` and `Standard` rows are skipped even if an older binary stamped them with a retry budget. Per retry chain, restart recovery arms only the best remaining candidate, not every row in the chain.

## RPC Methods

### `CancelRetry`

Cancels a pending retry timer for a failed session. The timer fires an async `oneshot` channel; cancellation sends to that channel, causing the `tokio::select!` to exit without re-queuing.

**Request params:**
```json
{ "session_id": "<uuid>" }
```

**Response:**
```json
{ "cancelled": true }
```

Returns `{ "cancelled": false }` if no retry was pending (session not found in completed map, or no retry timer active).

Handled in `crates/rsid/src/rpc.rs` by `handle_cancel_retry()`, which delegates to `SessionManager::cancel_retry()` in `lifecycle.rs`.

The `CancelRetry` capability is advertised in `DaemonCapabilities.retry_management`, which defaults to `true`.

### Implicit cancellation

A pending retry is cancelled (via the same `cancel_tx` mechanism) in these cases:
- `continue_session()` — user continues the session before the timer fires. This also exhausts the original row's retry budget durably so restart recovery cannot resurrect it.
- `delete_session()` — soft-delete before retry fires.
- `archive_session()` — archive before retry fires.
- `interrupt_session()` or `AgentHalt` — if `cancel_tx` is present in the completed map. This also persists retry exhaustion.
- `CancelRetry` — explicit manual cancellation also persists retry exhaustion.

Delete and archive do not need retry exhaustion writes because their durable status already excludes the row from retry recovery.

### Stale-finalize Guard

Every tracked provider process carries a monotonic `spawn_generation`. `finalize_session()` receives the generation it is finalizing and peeks before removing the active entry. If a newer incarnation was established by a continue or scheduled watch resume, the stale finalizer returns without marking the new process `Failed`, inserting completed state, or arming retry. This prevents watch-resumed sessions from being clobbered by an old monitor task.

## TUI Integration

### Session List Card Badge

The session list renders a `"retry N/M"` badge in the card header for sessions where `retry_attempt > 0` and `max_retries > 0`. Implemented in `crates/rsi/src/ui/session.rs`:

- The badge reads `session.retry_attempt.zip(session.max_retries).filter(|(attempt, max)| *attempt > 0 && *max > 0)`.
- Displayed as `" retry {attempt}/{max}"` in `theme::peach()` with `DIM` modifier.
- Controlled by the `CardField::RetryInfo` toggle in the card fields settings (`label: "Retry info"`).

### Bus Event Handling

`crates/rsi/src/app/polling.rs` handles `"session_retrying"` bus events:

- Updates `state.session.retry_attempt` and `state.session.max_retries` in the TUI's in-memory session map.
- Pushes a medium-priority Info notification: `"{query_prefix}: retrying {attempt}/{max} in {delay_s}s ({reason})"`.

### CancelRetry client method

`crates/rsi/src/client.rs` exposes `cancel_retry(session_id: Uuid) -> Result<bool>`, which sends a `CancelRetry` RPC request and returns the `cancelled` boolean from the response.

There is no dedicated keybinding for `CancelRetry` in the default keymap. The client method is available for programmatic use (e.g., from overlay handlers or future keybinding additions).

## Database Schema

### V31 Migration

Added in `crates/rsid/src/store/mod.rs`:

```sql
ALTER TABLE sessions ADD COLUMN retry_attempt INTEGER;
ALTER TABLE sessions ADD COLUMN max_retries    INTEGER;
```

Both columns are nullable. A session with no retry configuration has `NULL` in both columns.

The `StoreCommand::UpdateRetryState { session_id, retry_attempt: Option<u8>, max_retries: Option<u8> }` command, handled by the async store worker, performs the write. It is called from `monitor.rs` when a retry is scheduled and from `lifecycle.rs` when a retry is launched (to update the new session's fields).

## Source Files

| File | What it contains |
|---|---|
| `crates/rsid/src/store/mod.rs` | V31 migration adding `retry_attempt`/`max_retries` columns |
| `crates/rsid/src/session/retry_policy.rs` | Kind-scoped retry defaults and live runtime policy checks |
| `crates/rsid/src/session/monitor.rs` | `backoff_ms()`, `classify_retry_eligibility()`, retry scheduling after `finalize_session()`, `StallTimeout` break reason override, shared retry timer |
| `crates/rsid/src/session/lifecycle.rs` | `launch_retry()`, `cancel_retry()`, `interrupt_if_stall_retryable()`, durable cancellation, continue-vs-live-retry-child resolution, stale-finalize guard |
| `crates/rsid/src/session/types.rs` | `MonitorBreakReason::StallTimeout`, `CompletedSession.retry_cancel`, `CompletedSession.retry_fired_at`, `CompletedSession.superseded_by_retry`, `TrackedSession.spawn_generation`, `TrackedSession.stall_interrupted` |
| `crates/rsid/src/session/launch.rs` | `restore_sessions()` restart recovery, initial `retry_attempt`/`max_retries` assignment at launch |
| `crates/rsid/src/config.rs` | `retry_max_default`, `retry_max_backoff_ms`, `retry_on_stall` env vars |
| `crates/rsid/src/rpc.rs` | `CancelRetry` RPC handler |
| `crates/rsid/src/bus.rs` | `DaemonEvent::SessionRetrying { session_id, attempt, max_retries, backoff_ms, reason }` |
| `crates/rsid/src/main.rs` | Retry handler loop (`retry_rx`), stall-retry handler (`RSI_RETRY_ON_STALL`) |
| `crates/rsid/src/stall_detector.rs` | Stall detector with `InterruptAndRetry` action, `retry_tx` sender |
| `crates/rsi-common/src/types.rs` | `Session.retry_attempt: Option<u8>`, `Session.max_retries: Option<u8>` |
| `crates/rsi-common/src/rpc.rs` | `DaemonCapabilities.retry_management: bool`, `LaunchSessionParams.max_retries: Option<u8>` |
| `crates/rsi/src/client.rs` | `cancel_retry()` client method |
| `crates/rsi/src/ui/session.rs` | `retry_info` badge rendering, `CardField::RetryInfo` |
| `crates/rsi/src/types/mod.rs` | `CardField::RetryInfo` variant |
| `crates/rsi/src/app/polling.rs` | `"session_retrying"` bus event handler, notification push |
