# Two-Tier Session Summarization

## Overview

The daemon maintains two rolling summaries per session, generated automatically as assistant messages accumulate:

- **Short** — 2-5 sentences, generated every ~20 assistant messages. Used for session list display via `Session.short_summary`.
- **Long** — structured multi-section summary, generated every ~60 assistant messages. Used as context for child sessions in context rotation handoffs (injected as `[Session History]` block).

Both tiers are purely additive — each generation creates a new row in `session_summaries`, not an overwrite. Queries always fetch the row with the latest `created_at`.

Summarization is skipped for `TaskRabbit` and `Bug` session kinds (ephemeral sessions where history is not useful).

## Data Model

### `SummaryKind` enum (`rsi-common/src/types.rs:620`)

```
Short   — rolling every 20 assistant messages
Long    — rolling every 60 assistant messages
```

### `SessionSummary` struct (`rsi-common/src/types.rs:627`)

| Field | Type | Description |
|---|---|---|
| `id` | `i64` | Auto-increment DB row ID (0 before persistence) |
| `session_id` | `Uuid` | Parent session |
| `kind` | `SummaryKind` | Short or Long |
| `content` | `String` | Generated summary text |
| `covers_through_sequence` | `i32` | Highest event sequence number included in this summary |
| `token_count` | `u32` | Approximate token count (~4 chars/token heuristic) |
| `created_at` | `DateTime<Utc>` | Generation timestamp |

### `Session.short_summary` (`rsi-common/src/types.rs:121`)

A denormalized `Option<String>` field on the `Session` struct. Not user-editable. Populated at query time by the store from the `session_summaries` table. Used by the TUI session list to display a one-line context string below the session title.

### Storage Schema (migration V30, `rsid/src/store/mod.rs:526`)

```sql
CREATE TABLE session_summaries (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    kind TEXT NOT NULL,           -- 'Short' or 'Long'
    content TEXT NOT NULL,
    covers_through_sequence INTEGER NOT NULL,
    token_count INTEGER NOT NULL,
    created_at TEXT NOT NULL      -- nanosecond RFC 3339
);

CREATE INDEX idx_session_summaries_session_kind ON session_summaries(session_id, kind);
CREATE INDEX idx_session_summaries_session_latest ON session_summaries(session_id, kind, created_at DESC);
```

## Generation

### Trigger

Checked in the session event loop in `rsid/src/session/monitor.rs` on every non-empty assistant `Message` event. The counter `assistant_message_count` is incremented and `summarizer::should_summarize()` is called (`rsid/src/session/summarizer.rs:34`).

For continued sessions, `assistant_message_count` is initialized from the store via `Store::count_assistant_messages()` (`rsid/src/store/summaries.rs:189`). Last-summary watermarks are initialized from the latest stored summaries.

Thresholds (`rsid/src/session/summarizer.rs:10-11`):

| Constant | Value |
|---|---|
| `SHORT_SUMMARY_INTERVAL` | 20 assistant messages |
| `LONG_SUMMARY_INTERVAL` | 60 assistant messages |

`SummarizeAction::Both` fires when both thresholds are simultaneously due (count 60, 120, 180, ...). The check is always: `current_count - last_generated_count >= interval`.

### LLM Call Pattern

Both functions in `rsid/src/session/summarizer.rs` use the same Ollama-then-Haiku fallback pattern (`generate_with_fallback()`):

1. POST to `http://localhost:11434/api/generate` with model `qwen3:14b`, 60s timeout, temperature 0.3.
2. On failure or empty response, fall back to `claude -p <prompt> --model haiku --output-format text --no-session-persistence`.

Max output tokens:

| Kind | Constant | Limit |
|---|---|---|
| Short | `SHORT_MAX_TOKENS` | 512 tokens |
| Long | `LONG_MAX_TOKENS` | 1500 tokens |

### Prompt Construction

**Short** (`summarizer.rs:95`): receives the previous short summary (if any) + last ~20 assistant messages formatted as `Role: content` lines (capped at 4000 chars of events, each message truncated to 400 chars). Instructs the model to write 2-5 sentences covering what was accomplished, current state, and open decisions.

**Long** (`summarizer.rs:135`): receives the previous long summary (if any) + all events since the last long summary (capped at 8000 chars). Instructs the model to produce a structured bullet-point summary: key themes, major technical decisions, files modified, current state, and handoff notes.

Both prompts include the session's original `query` string (truncated to 300 bytes) for context.

### Persistence Path

After generation, the daemon:

1. Builds a `SessionSummary` (id=0, covers_through_sequence=current event sequence).
2. Calls `persistence.insert_session_summary()` (async, offloaded to `StoreCommand::InsertSessionSummary` via the store worker — `rsid/src/session/persistence.rs:343`).
3. On success, publishes `DaemonEvent::SessionSummaryUpdated` to the event bus.
4. Updates the in-memory `last_short_summary_at` / `last_long_summary_at` watermarks.

## Context Pipeline

`rsid/src/session/context_pipeline.rs` assembles the SYSTEM prompt for new sessions. When a `parent_session_id` is present (context rotation handoff), `gather_session_summary()` (line 231) fetches the parent's latest **Long** summary from the store within a 100ms timeout.

If found and non-empty, it is injected as a `ContextBlock` with:

- `tag`: `[Session History]`
- `priority`: 0 (same as active task block — highest priority, not dropped under token pressure)

This is gathered in parallel with all other context pipeline futures via `tokio::join!` (line 130). A timeout or store error produces `None` (silently skipped — the session still launches).

## RPC Methods

### `GetSessionSummary`

Defined in `rsid/src/rpc.rs:570`.

**Params** (`GetSessionSummaryParams`, `rpc.rs:141`):

| Field | Type | Required | Description |
|---|---|---|---|
| `session_id` | `Uuid` | yes | Session to query |
| `kind` | `Option<String>` | no | `"Short"`, `"Long"`, or omit for both |

**Response**:

- `kind = "Short"` or `"Long"`: returns `SessionSummary` or `null` as JSON.
- `kind = null` (omitted): returns `{ "short": SessionSummary|null, "long": SessionSummary|null }`.

Always returns the latest row for the requested kind (ordered by `created_at DESC`).

## Bus Events

### `SessionSummaryUpdated` (`rsid/src/bus.rs:106`)

Published after a summary is successfully persisted. Sent for both Short and Long kinds.

| Field | Type | Description |
|---|---|---|
| `session_id` | `Uuid` | Session the summary belongs to |
| `kind` | `SummaryKind` | `Short` or `Long` |
| `content` | `String` | The generated summary text |

Event type string: `"session_summary_updated"` (used in `BusEvent.event_type`).

This event is session-scoped: the session-filter predicate in `rpc.rs` matches on `session_id` (line 1779).

## Key Files

| File | Role |
|---|---|
| `crates/rsi-common/src/types.rs` | `SummaryKind`, `SessionSummary`, `Session.short_summary` |
| `crates/rsid/src/session/summarizer.rs` | Threshold logic, LLM call, prompt construction |
| `crates/rsid/src/session/monitor.rs` | Trigger point in event loop, bus publish |
| `crates/rsid/src/session/persistence.rs` | `insert_session_summary()` async wrapper, `StoreCommand::InsertSessionSummary` |
| `crates/rsid/src/session/types.rs` | `StoreCommand::InsertSessionSummary` enum variant |
| `crates/rsid/src/store/summaries.rs` | All SQL operations: insert, latest query, batch query, message count |
| `crates/rsid/src/store/mod.rs` | V30 migration (schema creation) |
| `crates/rsid/src/store/sessions.rs` | `Session.short_summary` denormalization via batch query |
| `crates/rsid/src/session/context_pipeline.rs` | `gather_session_summary()` — Long summary injection for rotation handoffs |
| `crates/rsid/src/bus.rs` | `DaemonEvent::SessionSummaryUpdated` definition |
| `crates/rsid/src/rpc.rs` | `GetSessionSummary` handler, `GetSessionSummaryParams` |
