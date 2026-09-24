# Persistence & Store Layer

> SQLite storage with async write workers

## Two SQLite Databases

```
~/.rsi/rsi.db     — Store: sessions, events, projects, metrics
~/.rsi/memory.sqlite   — MemoryStore: semantic memory index
```

Both use WAL mode. Accessed via dedicated worker tasks to keep SQLite off the hot path.

## Store Schema (`rsi.db`)

Version-tracked via `PRAGMA user_version`. 25 migrations (V0–V24).

### Tables

```
sessions (V0, extended through V24)
├── id, claude_session_id, provider, query, working_dir, status
├── created_at, updated_at, cost_usd, duration_ms, num_turns, model
├── input_tokens, output_tokens, context_window
├── total_input/output/cache_creation/cache_read_tokens
├── stop_reason, project_id, pinned_at, session_kind
├── continued_from, handoff_filepath, rotation_depth
├── daemon_input/output_tokens, title, pipeline_artifact
├── workflow_id, description, git_branch, active_task
├── group_id, pending_archive, testing_needed_at
└── Indexes: (none listed, indexed by id PK)

conversation_events (V0)
├── id AUTOINCREMENT, session_id FK, sequence, event_type
├── role, content, tool_name, tool_input, created_at
└── Indexes: idx_events_session_id, idx_events_session_sequence

turn_metrics (V1)
├── id AUTOINCREMENT, session_id FK, turn_number
├── input_tokens, cache_creation_tokens, cache_read_tokens
├── output_tokens, stop_reason, tools_used (JSON), tool_count
├── created_at, model (V9)
└── Indexes: idx_turn_metrics_session, idx_turn_metrics_session_turn

projects (V2)
├── id PK, name UNIQUE, path, description, color
├── created_at, updated_at, context_files (V23, JSON)
└── Index: idx_projects_path

model_segments (V9)
├── id AUTOINCREMENT, session_id FK ON DELETE CASCADE
├── model_id, from_sequence, to_sequence (nullable)
├── created_at
└── Index: idx_model_segments_session

workflows (V15)
├── id PK, title, stage, artifact_path, project_id FK
├── created_at, updated_at
└── Indexes: idx_workflows_project, idx_workflows_stage

session_groups (V21)
├── id PK, name, description, project_id, color
├── created_at, updated_at
└── Index: idx_session_groups_project

approvals (V0)
├── id PK, session_id FK, tool_name, tool_input
├── status, created_at, resolved_at
└── Index: idx_approvals_session_id

context_snapshots (V7)
├── id AUTOINCREMENT, session_id FK ON DELETE CASCADE
├── tokens_used, created_at
└── Index: idx_context_snapshots_session_latest

rotation_events (V18)
├── id AUTOINCREMENT, session_id, rotation_id, phase
├── event_type, metadata, created_at (nanosecond RFC 3339)
└── Indexes: idx_rotation_events_session, idx_rotation_events_rotation

esp_games (V16)
├── id PK, played_at, score, rounds_played
├── total_rounds, p_value, round_details (JSON)
└── (no additional indexes)
```

### Notable Migrations

- **V9**: Backfill — creates initial `model_segments` for all existing sessions
- **V10**: Data migration — converts `status = 'Rotated'` → `'Archived'`
- **V23**: Adds `context_files` column (JSON array of paths)

## Write Architecture

### PersistenceHandle (Session Operations)

```
SessionManager
    │
    ▼
PersistenceHandle
├── mpsc::Sender<StoreCommand>      (~30 command variants)
├── AtomicUsize queue_depth         (for monitoring)
└── AtomicU64 last_command_duration
    │
    ▼ (tokio task)
spawn_persistence_worker()
├── Receives StoreCommand from channel
├── run_store_op() → spawn_blocking → Store::method()
├── Logs warning if duration >= 100ms
└── Returns results via oneshot channels where needed
```

### StoreWorker (Legacy Path)

```
StoreHandle
├── std::sync::mpsc::SyncSender<StoreCommand>  (7 command variants)
├── StoreMetrics (queue_depth, last_command_duration, capacity)
    │
    ▼ (OS thread, not tokio)
StoreWorker::run()
├── Blocking rx.recv() loop
├── process_command_timed() with 100ms slow threshold
├── check_queue_depth() warns at 80% capacity
└── drain_remaining() on Shutdown
```

### Read Operations

Reads bypass workers entirely — they call `store.blocking_lock()` inside `tokio::task::spawn_blocking`:

```
list_sessions(), list_projects(), list_groups(),
list_workflows(), get_session(), load_events_since(), ...
```

## Row Mappers (`row_mappers.rs`)

Separate infallible extraction from fallible type conversion:

```
SQLite Row → SessionRow (37 raw fields) → Session (domain type)
                                              │
                                     parse_timestamp()
                                     str_to_session_status()
                                     str_to_session_provider()
                                     str_to_session_kind()
```

`parse_timestamp()`: tries RFC 3339 first, falls back to SQLite `datetime()` format for lenient parsing.

### Backward Compatibility

- `"Rotated"` → `SessionStatus::Archived`
- `"OpenAi"` → `SessionProvider::Local`

## Key Store Operations

| Operation | Method | Notes |
|---|---|---|
| Insert session | Full 36-parameter INSERT | All current columns |
| Update status | SET status + updated_at | |
| Update metadata | Batch update tokens, cost, model | |
| Load sessions | WHERE status != 'Archived' ORDER BY created_at | |
| Delete session | Transactional cascade: segments→metrics→approvals→events→sessions | |
| Toggle pin | Atomic SQL: `CASE WHEN pinned_at IS NULL THEN now ELSE NULL END` | |
| Insert event | Serializes tool_input to JSON, returns last_insert_rowid | |
| Load events | WHERE sequence > since_sequence ORDER BY sequence | Incremental fetch |
| Create model segment | Transactional: close current + insert new | Atomically swaps |
| Insert turn metric | Serializes tools_used Vec to JSON | |

## Project Index (`project_cache.rs`)

In-memory path resolver:

```
ProjectIndex
├── entries: Vec<(PathBuf, Uuid)>  — sorted by path length DESC
└── find_project_for_path(working_dir)
    └── Linear scan with path.starts_with() — returns first (longest) match
```

Rebuilt entirely on each project list change via `invalidate()`.
