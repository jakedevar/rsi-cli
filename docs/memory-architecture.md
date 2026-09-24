# Memory System Architecture

## System Overview

Memory is daemon-owned. The TUI talks to the daemon over JSON-RPC and never
touches `memory.sqlite` directly.

- `SessionManager::launch_session` resolves the session project, builds the
  launch context, and injects `[Relevant Memory]` only when the session has a
  project.
- `MemoryWorker` owns the memory SQLite connection and handles sync/search/read
  commands over an mpsc queue.
- `MemorySyncEngine` indexes global memory files and session transcripts into
  `files`, `chunks`, `chunks_fts`, and `chunks_vec`.
- `MemoryManager` is the daemon-facing facade used by RPC handlers and the
  dialectic query engine.

## Indexing Pipeline (per file)

```
  ┌─────────────────────┐
  │  ~/.rsi/memory/  │    ┌──────────────────┐
  │  MEMORY.md           │    │  rsi.db       │
  │  memory/*.md         │    │  (session events) │
  └─────────┬───────────┘    └────────┬──────────┘
            │                          │
            ▼                          ▼
  ┌──────────────────────────────────────────────────┐
  │  sync_memory_files()    sync_session_transcripts()│
  │  • list .md files       • load completed sessions │
  │  • build_file_entry()   • extract event text      │
  │  • hash compare         • build_session_entry()   │
  └──────────────────────┬───────────────────────────┘
                         │ changed files only
                         ▼
  ┌──────────────────────────────────────────────────┐
  │                  index_file()                     │
  │                                                   │
  │  1. chunk_markdown(content, 400 tokens, 80 overlap)
  │     └─ splits on headings, respects line boundaries│
  │     └─ long lines → character-boundary segments   │
  │     └─ overlap carry for context continuity       │
  │                                                   │
  │  2. enforce_max_input_tokens(chunks, 8192)        │
  │     └─ splits oversized chunks at UTF-8 boundaries│
  │                                                   │
  │  3. remap_chunk_lines(&line_map)                  │
  │     └─ session transcripts only                   │
  │     └─ maps virtual lines → real event positions  │
  │                                                   │
  │  4. embed_chunks_in_batches()                     │
  │     ├─ check EmbeddingCache (SQLite)              │
  │     ├─ build_batches() (32K char budget)          │
  │     ├─ embed_batch_with_retry() (3 attempts)      │
  │     │   ├─ Ollama: POST /api/embed                │
  │     │   └─ OpenAI: POST /embeddings               │
  │     ├─ L2 normalize all vectors                   │
  │     └─ store in cache                             │
  │                                                   │
  │  5. memory_store.index_file_chunks()              │
  │     ├─ DELETE old chunks for this file            │
  │     ├─ INSERT into chunks (text + embedding JSON) │
  │     ├─ INSERT into chunks_fts (FTS5)              │
  │     ├─ INSERT into chunks_vec (sqlite-vec)        │
  │     └─ UPSERT file entry                         │
  └──────────────────────────────────────────────────┘
```

## Search Pipeline

```
  Query: "How does the rotation system work?"
    │
    ▼
  ┌──────────────────────────────────────────────────┐
  │  search::search(store, provider, query, config)   │
  │                                                   │
  │  ┌─────────────┐                                 │
  │  │ 1. QUERY    │  extract_keywords()              │
  │  │    EXTRACT   │  → strip stop words             │
  │  │             │  → tokenize, dedup               │
  │  └──────┬──────┘  → ["rotation", "system"]       │
  │         │                                         │
  │         ├─────────────────┬───────────────────┐   │
  │         ▼                 ▼                   │   │
  │  ┌─────────────┐  ┌─────────────┐            │   │
  │  │ 2. KEYWORD  │  │ 3. VECTOR   │            │   │
  │  │    SEARCH   │  │    SEARCH   │            │   │
  │  │             │  │             │            │   │
  │  │ FTS5 BM25   │  │ embed_query │            │   │
  │  │ chunks_fts  │  │ chunks_vec  │            │   │
  │  │ MATCH query │  │ cosine sim  │            │   │
  │  │             │  │             │            │   │
  │  │ text_score  │  │ vector_score│            │   │
  │  └──────┬──────┘  └──────┬──────┘            │   │
  │         │                │                   │   │
  │         └────────┬───────┘                   │   │
  │                  ▼                           │   │
  │  ┌─────────────────────────┐                 │   │
  │  │ 4. HYBRID MERGE         │                 │   │
  │  │                         │                 │   │
  │  │ score = 0.7 * vec_score │                 │   │
  │  │       + 0.3 * text_score│                 │   │
  │  │                         │                 │   │
  │  │ dedup by chunk_id       │                 │   │
  │  └────────────┬────────────┘                 │   │
  │               │                              │   │
  │               ▼                              │   │
  │  ┌─────────────────────────┐                 │   │
  │  │ 5. TEMPORAL DECAY       │  (optional)     │   │
  │  │                         │                 │   │
  │  │ multiplier = e^(-λ·age) │                 │   │
  │  │ λ = ln(2) / half_life   │                 │   │
  │  │                         │                 │   │
  │  │ Evergreen exempt:       │                 │   │
  │  │  MEMORY.md, memory.md   │                 │   │
  │  │  memory/projects.md     │                 │   │
  │  └────────────┬────────────┘                 │   │
  │               │                              │   │
  │               ▼                              │   │
  │  ┌─────────────────────────┐                 │   │
  │  │ 6. MMR RE-RANK          │  (optional)     │   │
  │  │                         │                 │   │
  │  │ Jaccard token diversity │                 │   │
  │  │ λ·relevance - (1-λ)·sim│                 │   │
  │  │ greedy selection        │                 │   │
  │  └────────────┬────────────┘                 │   │
  │               │                              │   │
  │               ▼                              │   │
  │  ┌─────────────────────────┐                 │   │
  │  │ 7. FILTER & TRUNCATE    │                 │   │
  │  │                         │                 │   │
  │  │ min_score >= 0.35       │                 │   │
  │  │ max_results = 6         │                 │   │
  │  │ snippet ≤ 700 chars     │                 │   │
  │  └────────────┬────────────┘                 │   │
  │               │                              │   │
  │               ▼                              │   │
  │  Vec<MemorySearchResult>                     │   │
  └──────────────────────────────────────────────┘   │
```

## SQLite Storage Layout

```
  memory.sqlite (WAL mode)
  ┌─────────────────────────────────────────────────────────┐
  │                                                         │
  │  ┌───────────────────┐  ┌───────────────────────────┐  │
  │  │ meta              │  │ files                     │  │
  │  │ (key-value)       │  │ (tracked file metadata)   │  │
  │  │                   │  │                           │  │
  │  │ model: nomic...   │  │ path TEXT PK              │  │
  │  │ provider: ollama  │  │ source TEXT               │  │
  │  │ chunk_tokens: 400 │  │ hash TEXT                 │  │
  │  │ chunk_overlap: 80 │  │ mtime INTEGER             │  │
  │  │ vector_dims: 384  │  │ size INTEGER              │  │
  │  └───────────────────┘  └───────────────────────────┘  │
  │                                                         │
  │  ┌─────────────────────────────────────────────────┐   │
  │  │ chunks (main storage)                            │   │
  │  │                                                  │   │
  │  │ id TEXT PK   (path:source:start_line:hash)       │   │
  │  │ path TEXT                                        │   │
  │  │ source TEXT  (memory | sessions)                 │   │
  │  │ start_line INTEGER                               │   │
  │  │ end_line INTEGER                                 │   │
  │  │ hash TEXT    (SHA-256 of text)                   │   │
  │  │ model TEXT   (embedding model name)              │   │
  │  │ text TEXT    (chunk content)                     │   │
  │  │ embedding TEXT (JSON f32 array or empty)         │   │
  │  │ updated_at INTEGER                               │   │
  │  └─────────────────────────────────────────────────┘   │
  │                                                         │
  │  ┌───────────────────────┐  ┌───────────────────────┐  │
  │  │ chunks_fts (FTS5)     │  │ chunks_vec (vec0)     │  │
  │  │                       │  │                       │  │
  │  │ text (indexed)        │  │ id TEXT PK            │  │
  │  │ id (stored)           │  │ embedding float[384]  │  │
  │  │ path (stored)         │  │                       │  │
  │  │ source (stored)       │  │ sqlite-vec extension  │  │
  │  │ model (stored)        │  │ vendored C source     │  │
  │  │ start_line (stored)   │  │ compiled via build.rs │  │
  │  │ end_line (stored)     │  │                       │  │
  │  └───────────────────────┘  └───────────────────────┘  │
  │                                                         │
  │  ┌─────────────────────────────────────────────────┐   │
  │  │ embedding_cache                                  │   │
  │  │                                                  │   │
  │  │ PK: (provider, model, provider_key, hash)        │   │
  │  │ embedding TEXT (JSON f32 array)                  │   │
  │  │ dims INTEGER                                     │   │
  │  │ updated_at INTEGER (LRU pruning key)             │   │
  │  │                                                  │   │
  │  │ Max: 10,000 entries (configurable)               │   │
  │  └─────────────────────────────────────────────────┘   │
  └─────────────────────────────────────────────────────────┘
```

## Implementation Status

The 2026-05-11 project-scoped memory retrieval work is implemented.
The live behavior is documented in the section below.

- Launch-time `[Relevant Memory]` injection is project-scoped and skips
  unassigned sessions rather than leaking global memory into agent prompts.
- The harness and Codex app-server memory tools capture project scope at
  launch time and do not expose `project_id` to the agent.
- `QueryMemory` threads `project_id` through prefetch and every tool call.
- Observation search accepts `project_id` and preserves the legacy global
  path when callers pass `None`.

## File Watcher Flow

```
  ~/.rsi/memory/
       │
       │  notify crate (RecursiveMode::Recursive)
       ▼
  ┌────────────────────────────────────┐
  │  MemoryFileWatcher                 │
  │                                    │
  │  should_process_event()            │
  │  • Create/Modify/Remove only      │
  │  • Skip .git, node_modules, etc.  │
  │  • Accept .md files or dirs       │
  │                                    │
  │  debounce_loop()                   │
  │  • Coalesce events within 1500ms  │
  │  • Single sync trigger per burst  │
  │                                    │
  │  → handle.memory_files_changed()   │
  │  → mark dirty + incremental sync   │
  └────────────────────────────────────┘
```

## Embedding Provider Resolution

```
  config.embedding_provider
       │
       ├─ "none"  ──────────────────────→  FTS-only mode
       │
       ├─ "auto"  ──┬─ probe_ollama()
       │             │   GET /api/tags (2s timeout)
       │             │
       │             ├─ Success → OllamaEmbeddingProvider
       │             │
       │             └─ Fail ──┬─ API key present?
       │                       │
       │                       ├─ Yes → OpenAiCompatEmbeddingProvider
       │                       │        (fallback_reason set)
       │                       │
       │                       └─ No  → FTS-only mode
       │                                (unavailable_reason set)
       │
       ├─ "ollama" ──┬─ probe_ollama()
       │              ├─ Success → OllamaEmbeddingProvider
       │              └─ Fail   → FTS-only mode
       │
       └─ "openai" ──┬─ API key present?
                      ├─ Yes → OpenAiCompatEmbeddingProvider
                      └─ No  → FTS-only mode
```

## Project-Scoped Agent Memory Retrieval

As of plan `2026-05-11-project-scoped-agent-memory-retrieval`, agent-facing
memory queries are strictly scoped to the launching session's project. This
fixes the cross-project memory bleed where snippets from unrelated projects
could leak into an agent's `[Relevant Memory]` block or come back through
the harness `memory_search` tool.

### Storage model (V3 schema)

`files` and `chunks` carry a denormalized `project_id TEXT` column. The
column is `NULL` for:

- Global memory files (`MEMORY.md`, `memory/*.md`) — by design, not a leak.
- Sessions that were never attached to a project.
- Pre-V3 rows that have not yet been reindexed.

`chunks_fts` mirrors `project_id` as an `UNINDEXED` column so the project
filter can apply directly in FTS queries without joining back to `chunks`.

The V3 migration sets a `reindex_required` meta flag for in-place upgrades
(skipped for brand-new stores). The next `MemorySyncEngine::run_sync` reads
the flag, treats it as a `ReindexTrigger::SchemaUpgrade`, and runs a full
reindex into a temp DB before atomic-swap. After swap the flag is gone
because the temp DB's fresh schema never set it.

### Search-time semantics

- `project_id: Some(pid)` — restrict to chunks with matching `project_id`.
  Global memory files (NULL project_id) are excluded. This is the agent
  delivery path.
- `project_id: None` — unscoped. All chunks (including global memory) are
  eligible. This is the manual TUI search / debug / admin path.

The keyword, sqlite-vec, and cosine-fallback search paths all honor the
filter directly in SQL.

### Enforcement boundaries

- **Launch-time `[Relevant Memory]` block** — `ContextPipeline::gather_memory`
  requires the session to have a `project_id`. When the session is project-
  less the block is skipped entirely (no global fallback for agents).
- **Harness `memory_search` tool** — `MemorySearchTool { handle, project_id }`
  captures the scope at session launch. The JSON schema does not expose
  `project_id`; agents cannot widen their own scope.
- **CodexAppServer `rsi_memory_search`** — the per-session `ToolRegistry`
  built in `launch.rs` binds `rsi_memory_search` to the resolved
  `project_id` via `register_builtin_tools`.
- **Dialectic `QueryMemory` RPC** — `DialecticEngine::query` threads
  `project_id` through prefetch and every tool call. `search_memory` is
  scoped; `list_sessions` defaults to the active project when args omit
  it; `get_session_detail` / `get_conversation_excerpt` refuse cross-project
  reads.
- **Observation search** — `MemoryStore::search_observations_by_keyword`
  accepts `project_id`; the FTS table already mirrored it.

### What is NOT scoped

- Manual TUI memory search (`crates/rsi/src/overlay/memory_search.rs`) is
  intentionally unscoped — user explicitly invoked it.
- Admin RPC callers can pass `project_id: None` to preserve the legacy
  global search behavior.

### Operational notes

- Existing memory DBs require a full reindex to populate `project_id` on
  legacy chunks. The V3 migration sets the trigger automatically; if a
  daemon was upgraded but interrupted before the first sync completed, the
  flag persists and the next sync will pick it up.
- If a session is reassigned to a different project or a project is deleted,
  the daemon schedules a memory sync and the next transcript sync rewrites
  the indexed rows with the new `project_id` (or `NULL` after deletion).
