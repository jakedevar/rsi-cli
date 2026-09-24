# Tiered Observation Extraction

## Overview

Tiered observation extraction automatically distills completed AI coding sessions into atomic, queryable facts. When a session finishes (status `Completed` or `Interrupted`), the daemon sends the conversation transcript to a local LLM, which extracts 3–15 self-contained factual statements: what was worked on, decisions made, tools used, errors encountered, and project state changes. These observations are stored in `memory.sqlite` alongside optional vector embeddings, and are queryable via RPC from the TUI.

The "tiered" naming refers to the `ObservationLevel` taxonomy — currently only `Explicit` observations are produced during extraction. The `Deductive`, `Inductive`, and `Contradiction` tiers are reserved for the Dreamer consolidation subsystem (see `bus.rs` `DreamCompleted` event).

## Architecture

```
SessionLifecycle (session/lifecycle.rs)
    │  on Completed/Interrupted
    ▼
MemoryWorker::extract_observations()  ← tokio::spawn (non-blocking)
    │
    ├─ observation::extract_observations()   (observation/extractor.rs)
    │       ├─ extract_session_text()        (memory/session_text.rs)
    │       ├─ build_extraction_prompt()
    │       ├─ call_llm()  →  Ollama (qwen3:14b) → Haiku fallback
    │       └─ parse_observations_response()
    │
    ├─ EmbeddingProvider::embed_batch()      (optional, memory/embedding/)
    │
    ├─ MemoryStore::insert_observations()    (memory/store.rs)
    │
    └─ EventBus::publish(ObservationsExtracted)  (bus.rs)

RPC (rpc.rs)
    ├─ ListObservations  → MemoryWorker::list_observations()
    └─ SearchObservations → MemoryWorker::search_observations()
```

The extraction task is spawned via `tokio::spawn` after the session is written to the completed map, so it never blocks the session lifecycle path.

## Data Model

### Common types (`rsi-common/src/types.rs`, lines 641–688)

**`ObservationLevel`** — tier taxonomy:
- `Explicit` — facts directly stated in the transcript (the only level produced by extraction)
- `Deductive` — logical necessities derived from explicit facts (Dreamer)
- `Inductive` — patterns across multiple observations (Dreamer)
- `Contradiction` — conflicting statements (Dreamer)

**`ObservationConfidence`** — used by inductive observations: `Low`, `Medium`, `High`.

**`Observation`** — the primary type sent over RPC:
- `id: Uuid`, `session_id: Uuid`, `project_id: Option<Uuid>`
- `level: ObservationLevel`, `content: String`
- `source_ids: Vec<Uuid>` — parent observations for derived tiers
- `confidence: Option<ObservationConfidence>`
- `times_derived: u32` — reinforcement counter
- `created_at`, `updated_at: DateTime<Utc>`

**`ObservationSearchResult`** — wraps `Observation` with a `score: f64` relevance field.

### Store-layer row (`memory/store.rs`, lines 26–41)

`ObservationRow` maps to the `observations` table. IDs are `String` UUIDs; `source_ids` and `embedding` are JSON-serialized; `level` and `confidence` are lowercase string literals (`"explicit"`, `"low"`, etc.).

### SQLite schema (`memory/store.rs`, lines 288–322)

The `observations` table lives in `~/.rsi/memory.sqlite` (schema version 2):

```sql
CREATE TABLE observations (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    project_id TEXT,
    level TEXT NOT NULL DEFAULT 'explicit',
    content TEXT NOT NULL,
    source_ids TEXT DEFAULT '[]',
    confidence TEXT,
    times_derived INTEGER NOT NULL DEFAULT 1,
    embedding TEXT DEFAULT '',
    deleted_at TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
```

Indexes on `session_id`, `project_id`, `level`, and `created_at` are created unconditionally. FTS5 (`observations_fts`) and vector (`observation_vectors` via sqlite-vec) tables are created optimistically with graceful degradation if unavailable.

Deletions are soft (`deleted_at` timestamp). All list/get queries filter `WHERE deleted_at IS NULL`.

## Extraction Pipeline

**Entry point**: `observation::extract_observations()` (`observation/extractor.rs`, line 120)

1. **Threshold check** — if `events.len() < min_events`, returns empty immediately (default threshold: 4 events).

2. **Text extraction** — calls `extract_session_text(events)` from `memory/session_text.rs`. Only `Message` events with `Role::User` or `Role::Assistant` contribute text. Returns `None` if no qualifying events exist.

3. **Truncation** — session text is truncated at `max_input_chars` (default 16,000) on a UTF-8 char boundary.

4. **Prompt construction** — `build_extraction_prompt()` (line 12) produces a system prompt instructing the model to output a JSON array of strings, 3–15 per session. The session query is embedded and truncated at 400 chars.

5. **LLM call** — `call_llm()` (line 191) tries Ollama first, then falls back to Haiku:
   - **Ollama**: `POST http://localhost:11434/api/generate` with model `qwen3:14b`, `temperature: 0.3`, `num_predict: 1024`, `think: false`, 60-second timeout.
   - **Haiku fallback**: headless `claude -p <prompt> --model haiku --output-format text --no-session-persistence --permission-mode bypassPermissions`

6. **Response parsing** — `parse_observations_response()` (line 53) tries three strategies in order: direct JSON array parse, embedded JSON array extraction (handles LLM preamble), then line-by-line fallback with bullet/number stripping.

7. **Struct construction** — each parsed string becomes an `Observation` with `level: Explicit`, fresh UUID, `times_derived: 1`, and `source_ids: []`.

## Lifecycle Integration

Trigger site: `session/lifecycle.rs`, lines 242–283.

The check runs inside the session completion handler. Extraction is gated on three conditions simultaneously:
- A `MemoryHandle` is present (memory system is enabled)
- `completed_session.events.len() >= 4` (hard-coded pre-check, mirrors `observation_min_events` default)
- Final status is `Completed` or `Interrupted`

If all conditions pass, the session's `project_id`, `query`, and `events` are cloned, and a `tokio::spawn` task calls `memory_handle.extract_observations(session_id, project_id, query, events)`. The spawn happens after the session is inserted into the completed map so it is queryable during extraction.

Errors from the spawned task are logged as warnings and do not affect session persistence.

## Configuration

All fields live in `MemoryConfig` (`memory/types.rs`, lines 242–248). Populated from environment variables or config file.

| Field | Default | Description |
|---|---|---|
| `observation_extraction_enabled` | `true` | Master on/off switch |
| `observation_min_events` | `4` | Minimum conversation events to attempt extraction |
| `observation_max_input_chars` | `16_000` | Character budget for transcript sent to LLM |

There are no dedicated env var names defined in the source — these fields are populated via the daemon's config loading path. Set `observation_extraction_enabled = false` to disable extraction entirely without disabling the rest of the memory system.

## RPC Methods

Both methods are handled in `rpc.rs` lines 1499–1524. They require the memory system to be initialized; if the `MemoryManager` is absent, the handler returns an error.

### `ListObservations`

Returns `Vec<Observation>` as a JSON array.

Params (`ListObservationsParams`, `rpc.rs` line 288):
- `session_id?: Uuid` — filter by session
- `project_id?: Uuid` — filter by project
- `limit?: usize` — maximum results to return

All params are optional. Omitting all returns all non-deleted observations up to `limit`.

### `SearchObservations`

Returns `Vec<ObservationSearchResult>` (observation + relevance score).

Params (`SearchObservationsParams`, `rpc.rs` line 299):
- `query: String` — required search string
- `max_results?: usize` — maximum results (defaults to 20 in `handle_search_observations`)

Search uses FTS5 keyword search via `observations_fts` if available. If FTS5 is unavailable, it falls back to a `LIKE` query on `observations.content`. Vector search is not yet wired into `SearchObservations` (vector lookup at `search_observations_by_vector` exists in `store.rs` but is not called from the worker's `handle_search_observations`).

## Bus Events

### `ObservationsExtracted`

Defined in `bus.rs` line 113. Published after successful DB insert.

```
DaemonEvent::ObservationsExtracted {
    session_id: Uuid,
    count: usize,
}
```

Serialized `event_type`: `"observations_extracted"`. The `data` field contains `session_id` and `count`.

The TUI can subscribe to this event to update observation counts or trigger UI refreshes. The `MemoryProviderStatus.observation_count` field (`memory/types.rs` line 194) reflects the total count and is included in `GetHealthStatus` responses.

## Key Files

| File | Role |
|---|---|
| `crates/rsi-common/src/types.rs` (lines 641–688) | `Observation`, `ObservationLevel`, `ObservationConfidence`, `ObservationSearchResult` types |
| `crates/rsid/src/observation/extractor.rs` | Full extraction pipeline: prompt, LLM call, response parsing |
| `crates/rsid/src/observation/mod.rs` | Module re-export |
| `crates/rsid/src/memory/store.rs` (lines 929–1210) | Observation CRUD: `insert_observations`, `list_observations`, `search_observations_by_keyword`, `search_observations_by_vector`, `delete_observations_by_session` |
| `crates/rsid/src/memory/worker.rs` (lines 44–67, 393–516) | `MemoryCommand` variants, `run_observation_extraction` task |
| `crates/rsid/src/memory/types.rs` (lines 242–248, 280–284) | `MemoryConfig` observation fields and defaults |
| `crates/rsid/src/session/lifecycle.rs` (lines 242–283) | Extraction trigger on session completion |
| `crates/rsid/src/bus.rs` (lines 113–116) | `ObservationsExtracted` event definition |
| `crates/rsid/src/rpc.rs` (lines 286–303, 1499–1524) | RPC param structs and handlers |
| `crates/rsid/src/memory/session_text.rs` | Session transcript text extraction (used by extractor) |
