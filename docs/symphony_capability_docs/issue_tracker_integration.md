# Issue Tracker Integration

## Overview

The issue tracker integration polls external issue trackers (currently Linear) on a configurable interval and automatically dispatches rsi sessions for eligible issues. Each eligible issue gets a dedicated session launched via `SessionManager::launch_session()`. The system tracks in-flight dispatch records, reconciles running issues against their upstream tracker state each tick, and optionally transitions issues to a new workflow state when the corresponding session completes.

The feature is entirely opt-in: the daemon only activates it when `MOTHERSHIP_LINEAR_API_KEY`, `MOTHERSHIP_LINEAR_TEAM_ID`, and `MOTHERSHIP_ISSUE_TRACKER_WORKING_DIR` are all set in the environment.

---

## Architecture

```
Config (env vars)
       |
       v
IssueTrackerManager
  ├── IssueTrackerConfig
  ├── Box<dyn Tracker>  ←── LinearClient (reqwest + GraphQL)
  ├── Mutex<PollerState>
  ├── Arc<dyn SessionLauncher>  ←── SessionManager
  ├── Arc<Mutex<Store>>
  └── Arc<EventBus>
       |
       v
  poller::tick()  (runs on tokio interval)
       ├── 1. Reconcile running issues (fetch_by_ids → remove terminal)
       ├── 2. fetch_candidates (paginated)
       ├── 3. sort_by_priority
       ├── 4. eligibility::check_eligibility per issue
       ├── 5. SessionLauncher::launch per eligible issue
       └── 6. Publish IssueTrackerPolled bus event

  spawn_completion_listener()  (separate tokio task)
       └── Watches SessionStatusChanged(Completed) → update_issue_state()
```

The manager holds the `PollerState` behind a `Mutex` for the entire duration of each tick, preventing double-dispatch races.

---

## Configuration

All configuration is read from environment variables by `Config::issue_tracker_config()` in `crates/rsid/src/config.rs`. The method returns `None` (disabling the feature) if any required variable is absent or empty.

| Environment Variable | Required | Default | Description |
|---|---|---|---|
| `MOTHERSHIP_LINEAR_API_KEY` | Yes | — | Linear API key (Bearer token) |
| `MOTHERSHIP_LINEAR_TEAM_ID` | Yes | — | Linear team UUID to scope issue queries |
| `MOTHERSHIP_ISSUE_TRACKER_WORKING_DIR` | Yes | — | Working directory for all dispatched sessions |
| `MOTHERSHIP_LINEAR_ASSIGNEE` | No | None | Filter issues to this Linear user ID; `"me"` resolves to the API viewer ID |
| `MOTHERSHIP_ISSUE_TRACKER_PROJECT_ID` | No | None | Rsi project UUID to associate dispatched sessions with |
| `MOTHERSHIP_ISSUE_TRACKER_PROVIDER` | No | `claude` | AI provider for dispatched sessions (`claude`, `codex`, `opencode`, `local`, `gemini`, `nullclaw`) |
| `MOTHERSHIP_ISSUE_TRACKER_MODEL` | No | None | Model override for dispatched sessions |
| `MOTHERSHIP_ISSUE_TRACKER_POLL_INTERVAL_MS` | No | `30000` | Polling interval in milliseconds |
| `MOTHERSHIP_ISSUE_TRACKER_ACTIVE_STATES` | No | `started,unstarted` | Comma-separated Linear state types to treat as candidates |
| `MOTHERSHIP_ISSUE_TRACKER_MAX_CONCURRENT` | No | `5` | Maximum simultaneously running issue sessions |
| `MOTHERSHIP_ISSUE_TRACKER_COMPLETION_STATE` | No | None | Linear workflow state name to transition an issue to when its session completes (e.g., `"In Review"`) |

Fixed internal defaults (not overridable via env):
- `max_retries`: `3` per dispatched session
- `stall_timeout_ms`: `300_000` (5 minutes)

---

## Key Components

### IssueTracker Trait

Defined in `crates/rsid/src/issue_tracker/tracker.rs`. Backend-agnostic async interface:

```rust
#[async_trait]
pub trait Tracker: Send + Sync {
    async fn fetch_candidates(&self, config: &IssueTrackerConfig) -> Result<Vec<TrackedIssue>>;
    async fn fetch_by_ids(&self, config: &IssueTrackerConfig, ids: &[String]) -> Result<Vec<TrackedIssue>>;
    async fn update_issue_state(&self, config: &IssueTrackerConfig, issue_id: &str, state_name: &str) -> Result<()>;
    async fn resolve_viewer_id(&self, config: &IssueTrackerConfig) -> Result<String>;
}
```

- `fetch_candidates` — fetches all candidate issues (handles pagination internally).
- `fetch_by_ids` — fetches specific issues by UUID list, used during reconciliation.
- `update_issue_state` — transitions an issue to a named workflow state (used post-completion).
- `resolve_viewer_id` — resolves `"me"` assignee filter to the authenticated user's Linear UUID.

### Linear Client

`LinearClient` in `crates/rsid/src/issue_tracker/linear.rs` implements `Tracker` against the Linear GraphQL API at `https://api.linear.app/graphql`.

**GraphQL operations:**

| Operation | Query/Mutation | Purpose |
|---|---|---|
| `FetchCandidates` | Query | Paginated fetch of issues by team + state types (50 per page) |
| `FetchByIds` | Query | Fetch specific issues by UUID list for reconciliation |
| `UpdateIssueState` | Mutation (`issueUpdate`) | Set a new workflow state on an issue |
| `Viewer` | Query | Resolve the authenticated user's Linear UUID |
| `FindState` | Query | Look up a workflow state ID by name and team before `issueUpdate` |

**Pagination:** `fetch_candidates` loops through cursor-based pagination using `pageInfo.hasNextPage` and `pageInfo.endCursor` until all pages are consumed.

**Rate limiting:** The `graphql()` helper retries on HTTP 429 up to 3 times, respecting the `retry-after` response header. If the header is absent it falls back to exponential backoff (`2^attempt` seconds). After 3 failed attempts it returns a `DaemonError::Process`.

**Assignee filtering:** Applied client-side after fetching each page. Issues whose `assignee.id` does not match `config.assignee` are dropped. If `config.assignee` is `None`, all issues pass.

**Blocker extraction:** Relations are filtered to only `type == "blocks"` entries. The `relatedIssue.id` and `relatedIssue.state.type` are captured in `BlockerRef`. Relations of type `"related"` and other types are discarded.

### Dispatch Engine

`poller::tick()` in `crates/rsid/src/issue_tracker/poller.rs` executes one full poll cycle:

1. **Reconcile** — if `state.running` is non-empty, calls `fetch_by_ids` for all running issue IDs. Issues returned with `state_type == "completed"` or `"cancelled"` are removed from `state.running`, persisted as terminal in the database, and a `IssueReconciled` bus event is published.

2. **Fetch candidates** — calls `fetch_candidates`. On error, appends to `result.errors` and returns early.

3. **Sort** — `eligibility::sort_by_priority` sorts the candidate slice in place.

4. **Filter and dispatch** — iterates sorted candidates, calling `eligibility::check_eligibility` for each. Eligible issues trigger a `SessionLauncher::launch()` call with a `LaunchConfig` whose `query` is formatted as:
   ```
   [ENG-42] Fix auth bug

   Issue: ENG-42
   URL: https://linear.app/...

   Description:
   <issue description>
   ```
   The `issue_identifier`, `issue_url`, and `issue_tracker_id` fields of `LaunchConfig` are set from the issue. On success, a `DispatchRecord` is persisted to SQLite and the issue ID is added to `state.claimed` and `state.running`.

5. **Publish** — emits `IssueTrackerPolled` bus event with counts.

### Eligibility Rules

`eligibility::check_eligibility()` in `crates/rsid/src/issue_tracker/eligibility.rs` evaluates rules in order. Returns `None` if the issue is eligible, or `Some(reason)` if not:

| Order | Rule | Rejection Reason |
|---|---|---|
| 1 | `issue.id`, `issue.identifier`, and `issue.title` must be non-empty | `"missing required fields"` |
| 2 | `issue.state.state_type` must be in `config.active_states` | `"state '<type>' not in active states"` |
| 3 | `issue.state.state_type` must NOT be `"completed"` or `"cancelled"` | `"terminal state"` |
| 4 | Issue ID must not be in `state.claimed` | `"already claimed"` |
| 5 | Issue ID must not be in `state.running` | `"already running"` |
| 6 | `running_count` must be less than `config.max_concurrent` | `"concurrency limit"` |
| 7 | If `state_type == "unstarted"` and `blocked_by` is non-empty, all blockers must have `state_type == "completed"` or `"cancelled"` | `"blocked"` |

`sort_by_priority` sorts issues by:
1. `priority` ascending (1=urgent → 4=low); `None` sorts last (treated as `u8::MAX`).
2. `created_at` ascending (older first) as tiebreaker.
3. `identifier` alphabetically as final tiebreaker.

### Polling Loop

`IssueTrackerManager::spawn()` in `crates/rsid/src/issue_tracker/manager.rs` launches a `tokio::spawn` task with a `tokio::time::interval` configured at `config.poll_interval_ms`. The interval uses `MissedTickBehavior::Skip` so a slow tick does not cause tick accumulation.

The `Mutex<PollerState>` is held for the entire duration of each tick, serializing concurrent ticks.

On tick error, the manager publishes a `DaemonEvent::SystemMessage { level: "error", ... }` to the event bus rather than panicking.

**Manual poll trigger:** `trigger_poll()` executes an immediate `poller::tick()` outside the interval, used by the `TriggerIssueTrackerPoll` RPC method.

**Completion listener:** `spawn_completion_listener()` launches a separate task that subscribes to the event bus and watches for `DaemonEvent::SessionStatusChanged { new_status: Completed }`. When a completed session matches a running dispatch record, the manager calls `update_issue_state()` with `config.completion_state` (if set) and removes the dispatch from `state.running`.

**State restoration:** `restore_from_db()` loads all non-terminal dispatch records from SQLite on startup and repopulates `state.claimed` and `state.running`, so a daemon restart does not re-dispatch already-running issues.

---

## Data Model

### Issue Types

**`TrackedIssue`** — normalized representation of an issue from any tracker backend:

| Field | Type | Description |
|---|---|---|
| `id` | `String` | Tracker-internal UUID (e.g., Linear issue UUID) |
| `identifier` | `String` | Human-readable identifier (e.g., `"ENG-42"`) |
| `title` | `String` | Issue title |
| `description` | `Option<String>` | Issue description body |
| `priority` | `Option<u8>` | 1=urgent, 2=high, 3=medium, 4=low; `None` = unset |
| `state` | `IssueState` | Current workflow state |
| `branch_name` | `Option<String>` | Associated git branch name from Linear |
| `url` | `String` | Direct URL to the issue |
| `labels` | `Vec<String>` | Label names |
| `blocked_by` | `Vec<BlockerRef>` | Issues blocking this one |
| `assignee_id` | `Option<String>` | Linear user UUID of the assignee |
| `created_at` | `DateTime<Utc>` | Issue creation timestamp |
| `updated_at` | `DateTime<Utc>` | Last update timestamp |

**`IssueState`**:

| Field | Type | Description |
|---|---|---|
| `id` | `String` | State UUID |
| `name` | `String` | Display name (e.g., `"In Progress"`) |
| `state_type` | `String` | Linear state type: `"started"`, `"unstarted"`, `"completed"`, `"cancelled"`, `"triage"` |

**`BlockerRef`**:

| Field | Type | Description |
|---|---|---|
| `id` | `String` | Blocking issue UUID |
| `state_type` | `String` | Current state type of the blocker |

**`DispatchRecord`** — persisted record of an issue dispatched to a session:

| Field | Type | Description |
|---|---|---|
| `issue_id` | `String` | Tracker-internal issue UUID |
| `issue_identifier` | `String` | Human-readable identifier (e.g., `"ENG-42"`) |
| `tracker` | `String` | Tracker name (e.g., `"linear"`) |
| `session_id` | `uuid::Uuid` | Rsi session UUID |
| `dispatched_at` | `DateTime<Utc>` | When the session was launched |
| `last_reconciled_at` | `Option<DateTime<Utc>>` | Last time state was reconciled |
| `terminal_state` | `Option<String>` | Set when issue reaches a terminal state |

**`IssueTrackerStatus`** — snapshot returned by the `GetIssueTrackerStatus` RPC:

| Field | Type | Description |
|---|---|---|
| `enabled` | `bool` | Whether the tracker is active |
| `tracker` | `String` | Tracker name (`"linear"`) |
| `last_poll_at` | `Option<DateTime<Utc>>` | Timestamp of the last completed poll |
| `next_poll_at` | `Option<DateTime<Utc>>` | Calculated next poll time (`last_poll_at + poll_interval_ms`) |
| `dispatched_count` | `usize` | Number of currently running issue sessions |
| `max_concurrent` | `usize` | Configured concurrency limit |
| `poll_interval_ms` | `u64` | Configured poll interval |
| `active_states` | `Vec<String>` | Configured active state types |

**`TickResult`** — result of a single poll tick (returned by `TriggerIssueTrackerPoll`):

| Field | Type | Description |
|---|---|---|
| `issues_found` | `usize` | Total candidate issues fetched |
| `dispatched` | `usize` | Number of sessions launched this tick |
| `skipped_claimed` | `usize` | Issues skipped because already claimed or running |
| `skipped_blocked` | `usize` | Issues skipped due to unresolved blockers |
| `errors` | `Vec<String>` | Error messages from failed launches or reconciliation |

### Session Fields

Three fields on `Session` (in `crates/rsi-common/src/types.rs`) are set for issue-driven sessions. All are `Option<String>` with `#[serde(default)]`:

| Field | Description |
|---|---|
| `issue_identifier` | Human-readable issue identifier (e.g., `"ENG-42"`) |
| `issue_url` | Direct URL to the issue in the tracker |
| `issue_tracker_id` | Tracker-internal UUID used for reconciliation queries |

These fields are `None` on manually launched sessions.

---

## Database Schema

Added in the V33 migration in `crates/rsid/src/store/mod.rs`.

**New columns on `sessions` table:**

```sql
ALTER TABLE sessions ADD COLUMN issue_identifier TEXT;
ALTER TABLE sessions ADD COLUMN issue_url TEXT;
ALTER TABLE sessions ADD COLUMN issue_tracker_id TEXT;
```

**New table `issue_tracker_dispatches`:**

```sql
CREATE TABLE IF NOT EXISTS issue_tracker_dispatches (
    issue_id          TEXT NOT NULL PRIMARY KEY,
    issue_identifier  TEXT NOT NULL,
    tracker           TEXT NOT NULL DEFAULT 'linear',
    session_id        TEXT NOT NULL,
    dispatched_at     TEXT NOT NULL,
    last_reconciled_at TEXT,
    terminal_state    TEXT,
    created_at        TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000000Z', 'now'))
);

CREATE INDEX IF NOT EXISTS idx_issue_dispatches_session
    ON issue_tracker_dispatches(session_id);
CREATE INDEX IF NOT EXISTS idx_issue_dispatches_tracker
    ON issue_tracker_dispatches(tracker);
```

`issue_id` is the primary key (tracker-internal UUID), enforcing the one-session-per-issue invariant at the database level. `terminal_state` is `NULL` while the session is running and set to `"completed"` or `"cancelled"` when the issue reaches a terminal state.

---

## RPC Methods

Handled in `crates/rsid/src/rpc.rs`. The `RpcServer` holds an `Option<Arc<IssueTrackerManager>>`; methods that require it return a disabled/error response when the manager is absent.

### `GetIssueTrackerStatus`

Returns the current `IssueTrackerStatus` snapshot. If the issue tracker is not configured, returns:
```json
{ "enabled": false, "tracker": "none", "dispatched_count": 0, "max_concurrent": 0 }
```

**Params:** none

**Returns:** `IssueTrackerStatus` (see Data Model above)

### `ListDispatchedIssues`

Returns all currently running dispatch records. Returns an empty array if the tracker is not configured.

**Params:** none

**Returns:** `Vec<DispatchRecord>`

### `TriggerIssueTrackerPoll`

Forces an immediate poll tick outside the regular interval. Returns an error if the tracker is not configured.

**Params:** none

**Returns:** `TickResult`

### `GetDaemonCapabilities` — `issue_tracker` field

The `DaemonCapabilities` struct in `crates/rsi-common/src/rpc.rs` includes:

```rust
pub issue_tracker: bool,  // set to true by daemon when configured
```

Defaults to `false`. Set to `self.issue_tracker_manager.is_some()` in the `GetDaemonCapabilities` handler.

---

## Bus Events

All events are variants of `DaemonEvent` in `crates/rsid/src/bus.rs` and are converted to `BusEvent` (with `event_type` string and serde `data` payload) for TUI subscribers.

### `IssueDispatched`

Published when a session is successfully launched for an issue.

| Field | Type | Description |
|---|---|---|
| `issue_id` | `String` | Tracker-internal issue UUID |
| `issue_identifier` | `String` | Human-readable identifier (e.g., `"ENG-42"`) |
| `session_id` | `Uuid` | Rsi session UUID that was launched |
| `tracker` | `String` | Tracker name (e.g., `"linear"`) |

`event_type`: `"issue_dispatched"`

### `IssueTrackerPolled`

Published at the end of every poll tick (success or partial).

| Field | Type | Description |
|---|---|---|
| `issues_found` | `usize` | Total candidate issues fetched |
| `dispatched` | `usize` | Sessions launched this tick |
| `skipped_claimed` | `usize` | Issues skipped as already claimed/running |
| `skipped_blocked` | `usize` | Issues skipped due to unresolved blockers |

`event_type`: `"issue_tracker_polled"`

### `IssueReconciled`

Published during reconciliation when a running issue's upstream state changes to terminal.

| Field | Type | Description |
|---|---|---|
| `issue_id` | `String` | Tracker-internal issue UUID |
| `issue_identifier` | `String` | Human-readable identifier |
| `old_state` | `String` | Previous state label (e.g., `"active"`) |
| `new_state` | `String` | New state type from tracker (e.g., `"completed"`, `"cancelled"`) |
| `action` | `String` | One of `"stopped"`, `"continued"`, `"completed"` |

`event_type`: `"issue_reconciled"`

---

## Source Files

| File | Role |
|---|---|
| `crates/rsid/src/issue_tracker/mod.rs` | Module declaration; re-exports submodules |
| `crates/rsid/src/issue_tracker/types.rs` | `TrackedIssue`, `IssueState`, `BlockerRef`, `DispatchRecord`, `IssueTrackerConfig`, `IssueTrackerStatus`, `TickResult` |
| `crates/rsid/src/issue_tracker/tracker.rs` | `Tracker` trait (backend-agnostic interface) |
| `crates/rsid/src/issue_tracker/linear.rs` | `LinearClient` — GraphQL implementation of `Tracker` |
| `crates/rsid/src/issue_tracker/manager.rs` | `IssueTrackerManager` — owns polling loop, completion listener, state restoration |
| `crates/rsid/src/issue_tracker/poller.rs` | `tick()` function, `PollerState`, `SessionLauncher` trait |
| `crates/rsid/src/issue_tracker/eligibility.rs` | `check_eligibility()`, `sort_by_priority()` |
| `crates/rsid/src/config.rs` | `Config::issue_tracker_config()` — env-var driven config builder |
| `crates/rsid/src/store/mod.rs` | V33 migration: `issue_tracker_dispatches` table + session columns |
| `crates/rsid/src/rpc.rs` | `GetIssueTrackerStatus`, `ListDispatchedIssues`, `TriggerIssueTrackerPoll` handlers |
| `crates/rsid/src/bus.rs` | `IssueDispatched`, `IssueTrackerPolled`, `IssueReconciled` `DaemonEvent` variants |
| `crates/rsi-common/src/types.rs` | `Session.issue_identifier`, `Session.issue_url`, `Session.issue_tracker_id` |
| `crates/rsi-common/src/rpc.rs` | `DaemonCapabilities.issue_tracker` capability flag |
# Local tracker note

RSI also ships a V72 local SQLite issue tracker; its complete operating and
agent-authority contract is [docs/local-issue-tracker.md](../local-issue-tracker.md).
Linear configuration remains applicable only when the Linear backend is selected.
