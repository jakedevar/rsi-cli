# Background Task Queue

## Overview

The background task queue provides async, SQLite-backed execution of memory operations that should not block the session hot path. Tasks are grouped into work units, batched by accumulated token count, and dispatched to `TaskProcessor` implementations. The queue supports optimistic claim locking, retry with backoff, stale claim recovery, and periodic purge of completed records.

Primary use cases: extracting observations from conversation events, generating session summaries, running dream consolidation cycles, updating entity cards, and reconciling embedding consistency.

## Architecture

```
QueueHandle.enqueue()
    └─ mpsc channel (capacity 128)
           └─ QueueWorker.run()
                  ├─ handle_enqueue()  → Store.enqueue_task()
                  ├─ process_eligible()
                  │      ├─ Store.list_eligible_work_units()   (token threshold gate)
                  │      ├─ Store.claim_work_unit()            (optimistic lock)
                  │      ├─ TaskProcessor.process()
                  │      └─ Store.complete_work_unit() / fail_work_unit()
                  ├─ clean_stale_claims()  → Store.release_stale_claims()
                  └─ purge_completed()    → Store.purge_completed_items()
```

**Key types:**

- `QueueHandle` (`worker.rs:35`) — cloneable sender side; used by callers to enqueue tasks and send control signals.
- `QueueWorker` (`worker.rs:87`) — owns the receiver and drives the polling loop as a tokio task.
- `TaskProcessor` (`processor.rs:10`) — async trait implemented per task type; receives a slice of `QueueItem`s for a single work unit.
- `Store` queue methods (`store/queue.rs`) — all SQLite mutations go through these; never write the `background_queue` table directly.

**Work unit key** groups related tasks: `"{task_type}:{project_id}:{session_id}"` (project falls back to `"_"` when absent). See `types.rs:94`.

## Task Types

Defined in `queue/types.rs:9` as `TaskType` enum:

| Variant | DB string | Token threshold | Trigger |
|---|---|---|---|
| `ExtractObservations` | `extract_observations` | 1024 | Token accumulation |
| `Summarize` | `summarize` | 2048 | Token accumulation |
| `Dream` | `dream` | 0 (immediate) | Scheduled / observation count |
| `UpdateCard` | `update_card` | 0 (immediate) | Observation count |
| `Reconcile` | `reconcile` | 0 (immediate) | Timer-driven |

A threshold of `0` means the task is eligible as soon as it exists — no batching wait.

### Adding a New Task Type

1. Add a variant to `TaskType` in `queue/types.rs` with `as_str()`, `from_str()`, and `default_token_threshold()` branches.
2. Implement `TaskProcessor` for the new type (either extend an existing processor or create a new struct).
3. Update `processor.handles()` to return `true` for the new variant.
4. Enqueue via `QueueHandle.enqueue()` with the new `TaskType`.

## Task Lifecycle

```
Pending ──(token threshold met)──► Claimed ──(processor Ok)──► Completed
                                      │
                                      └─(processor Err, attempts < max)──► Pending (retry)
                                      │
                                      └─(processor Err, attempts >= max)──► Failed
```

**Status constants** (`store/queue.rs:8-11`): `"pending"`, `"claimed"`, `"completed"`, `"failed"`.

**Claiming** uses an optimistic SQL `UPDATE … WHERE status = 'pending'`; if `updated == 0` the work unit was already claimed by a concurrent iteration and is skipped (`store/queue.rs:121-132`).

**Retry logic** (`store/queue.rs:148-171`):
- On failure, `attempts` is incremented and `error` is recorded.
- Items where `attempts < max_attempts` revert to `"pending"`.
- Items where `attempts >= max_attempts` are permanently set to `"failed"`.
- Default `max_attempts` is `5` (`types.rs:73`).

**Stale claim recovery**: on every poll tick, claims held longer than `stale_claim_timeout_secs` (default 300 s) are released back to `"pending"` (`worker.rs:297-308`). This protects against crashed processors.

**Shutdown drain**: on `QueueCommand::Shutdown`, the worker drains remaining `Enqueue` commands from the channel into SQLite before exiting (`worker.rs:324-353`), ensuring no in-flight enqueues are lost.

## SQLite Persistence

Table: `background_queue`. All mutations go through `Store` methods in `crates/rsid/src/store/queue.rs`.

**`QueueItem` columns** (`store/queue.rs:15-31`):

| Column | Type | Notes |
|---|---|---|
| `id` | `i64` | Auto-increment primary key |
| `work_unit_key` | `TEXT` | Groups related items |
| `task_type` | `TEXT` | Matches `TaskType::as_str()` |
| `session_id` | `TEXT?` | Optional session scope |
| `project_id` | `TEXT?` | Optional project scope |
| `payload` | `TEXT` | JSON task payload |
| `token_count` | `INTEGER` | Tokens contributed by this item |
| `status` | `TEXT` | `pending / claimed / completed / failed` |
| `priority` | `INTEGER` | Higher = processed first |
| `attempts` | `INTEGER` | Failure count |
| `max_attempts` | `INTEGER` | Retry cap (default 5) |
| `error` | `TEXT?` | Last failure message |
| `created_at` | `TEXT` | Nanosecond RFC 3339 |
| `claimed_at` | `TEXT?` | Set on claim, cleared on stale release |
| `completed_at` | `TEXT?` | Set on completion |

**Store methods summary:**

| Method | Location |
|---|---|
| `enqueue_task()` | `store/queue.rs:55` |
| `list_eligible_work_units()` | `store/queue.rs:87` |
| `claim_work_unit()` | `store/queue.rs:120` |
| `complete_work_unit()` | `store/queue.rs:135` |
| `fail_work_unit()` | `store/queue.rs:148` |
| `release_stale_claims()` | `store/queue.rs:175` |
| `purge_completed_items()` | `store/queue.rs:218` |
| `queue_metrics()` | `store/queue.rs:188` |

`list_eligible_work_units` queries `SUM(token_count) >= threshold` per `work_unit_key` where `status = 'pending'`, ordered by `MAX(priority) DESC, MIN(created_at) ASC` — higher priority and older work units run first.

## Bus Events

Defined in `crates/rsid/src/bus.rs` as `DaemonEvent` variants. Converted to `BusEvent` with `event_type` strings via `From<DaemonEvent>` (`bus.rs:157`).

**`QueueTaskCompleted`** (`bus.rs:93`):
- `event_type`: `"queue_task_completed"`
- Fields: `work_unit_key: String`, `task_type: String`, `item_count: usize`
- Emitted by `TaskProcessor` implementations after a successful work unit.

**`QueueTaskFailed`** (`bus.rs:99`):
- `event_type`: `"queue_task_failed"`
- Fields: `work_unit_key: String`, `task_type: String`, `error: String`, `attempts: i32`
- Emitted when a work unit exhausts all retry attempts.

Note: the worker itself does not currently emit bus events — that responsibility falls on `TaskProcessor` implementations, which have access to the bus via construction.

## Configuration

### Environment Variables

| Variable | Default | Description |
|---|---|---|
| `MOTHERSHIP_QUEUE_ENABLED` | `true` | Set `0/false/no/off` to disable |
| `MOTHERSHIP_QUEUE_POLL_INTERVAL_SECS` | `30` | Polling interval |
| `MOTHERSHIP_QUEUE_TOKEN_THRESHOLD` | `1024` | Global token threshold override |

### `QueueConfig` struct (`types.rs:64`)

| Field | Default | Notes |
|---|---|---|
| `poll_interval_secs` | 30 | Worker tick rate |
| `default_token_threshold` | 1024 | Applied to `list_eligible_work_units` |
| `stale_claim_timeout_secs` | 300 | Claims older than this are released |
| `max_attempts` | 5 | Per-item retry cap stored at insert time |
| `completed_retention_secs` | 86400 (24 h) | Completed rows purged after this |
| `enabled` | true | Short-circuit in `Config::queue_config()` |

`Config::queue_config()` (`config.rs:439`) builds a `QueueConfig` from env vars. It returns `None` when `queue_enabled = false`, which causes the daemon to skip spawning the worker entirely.

The `spawn_queue_worker()` helper (`worker.rs:357`) wires the `mpsc` channel, constructs the worker, spawns it as a tokio task, and returns the `QueueHandle`.

## Key Files

| File | Purpose |
|---|---|
| `crates/rsid/src/queue/mod.rs` | Module root, doc comment |
| `crates/rsid/src/queue/types.rs` | `TaskType`, `QueueConfig`, `make_work_unit_key` |
| `crates/rsid/src/queue/worker.rs` | `QueueWorker`, `QueueHandle`, `QueueCommand`, `spawn_queue_worker` |
| `crates/rsid/src/queue/processor.rs` | `TaskProcessor` trait, `NoOpProcessor` |
| `crates/rsid/src/store/queue.rs` | `QueueItem`, `WorkUnitSummary`, `QueueMetrics`, all Store methods |
| `crates/rsid/src/bus.rs` | `DaemonEvent::QueueTaskCompleted/Failed` (lines 93-104) |
| `crates/rsid/src/config.rs` | Queue env vars (lines 155-173), `Config::queue_config()` (line 439) |
