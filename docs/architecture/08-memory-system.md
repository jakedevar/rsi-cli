# Memory System

> Semantic search over Markdown files and session transcripts

## Architecture

```
┌─────────────────────────────────────────────────────┐
│                 Memory System                        │
│                                                      │
│  ┌──────────────┐   ┌────────────────┐              │
│  │ MemoryManager │──→│ MemoryHandle   │              │
│  │ (public API)  │   │ (mpsc sender)  │              │
│  └──────────────┘   └───────┬────────┘              │
│                              │                       │
│                              ▼                       │
│                    ┌──────────────────┐              │
│                    │  MemoryWorker    │              │
│                    │  (tokio task)    │              │
│                    │                  │              │
│                    │  Commands:       │              │
│                    │  • SyncNow       │              │
│                    │  • Search        │              │
│                    │  • Status        │              │
│                    │  • ReadFile      │              │
│                    │  • Shutdown      │              │
│                    └───────┬──────────┘              │
│                            │                         │
│              ┌─────────────┴──────────────┐         │
│              │                            │         │
│              ▼                            ▼         │
│    ┌──────────────────┐    ┌──────────────────┐    │
│    │ MemorySyncEngine  │    │ MemoryFileWatcher│    │
│    │                   │    │ (inotify/kqueue) │    │
│    │ • File discovery  │    │ • Debounce 1.5s  │    │
│    │ • Chunking        │    └──────────────────┘    │
│    │ • Embedding       │                             │
│    │ • Indexing         │                             │
│    └────────┬──────────┘                             │
│             │                                        │
│             ▼                                        │
│    ┌──────────────────┐    ┌──────────────────┐    │
│    │ MemoryStore       │    │ EmbeddingProvider│    │
│    │ (memory.sqlite)   │    │ (Ollama/OpenAI)  │    │
│    │                   │    │                  │    │
│    │ Tables:           │    │ embed_query()    │    │
│    │ • meta            │    │ embed_batch()    │    │
│    │ • files           │    └──────────────────┘    │
│    │ • chunks          │                             │
│    │ • chunks_fts      │                             │
│    │ • embedding_cache │                             │
│    └──────────────────┘                             │
└─────────────────────────────────────────────────────┘
```

## File Discovery

Scans three locations relative to workspace:

```
{workspace}/MEMORY.md           # Single file
{workspace}/memory.md           # Single file
{workspace}/memory/             # Recursive directory walk
    ├── *.md files only
    ├── No symlinks
    └── Canonical path deduplication
```

Files tracked in `MemoryStore.files` table with `path`, `hash` (SHA-256), `mtime_ms`, `size`, `source`.

## Indexing Pipeline

```
run_sync()
├── list_memory_files(workspace_dir)
├── For each file:
│   ├── build_file_entry() → hash + metadata
│   ├── Compare hash with stored version
│   ├── If changed:
│   │   ├── chunk_text() → Vec<MemoryChunk>
│   │   │   └── Token-bounded (400 tokens) with overlap (80 tokens)
│   │   ├── embed_batch() → Vec<Vec<f32>>  (if provider available)
│   │   ├── upsert_chunk() + upsert_fts_chunk()
│   │   └── Update files table
│   └── If unchanged: skip
├── Delete orphaned files/chunks
└── Publish MemoryIndexUpdated event
```

### Chunk IDs

Composite key: `"{path}:{source}:{start_line}:{hash}"` — deterministic, no UUID generation.

## Search Pipeline

```
search(store, provider, query, config)
│
├── Determine mode: FtsOnly (no embeddings) or Hybrid
│
├── [FTS Only path]
│   ├── extract_keywords(query) → remove stop words
│   └── For each keyword:
│       └── search_keyword() → FTS5 BM25 query
│           └── bm25_rank_to_score(): 1/(1+rank)
│
├── [Hybrid path]
│   ├── search_keyword(query) → FTS5 results
│   ├── provider.embed_query(query) → query vector
│   ├── search_vector(query_vec) → cosine similarity
│   └── merge_hybrid_results()
│       └── score = vector_weight(0.7) × vector_score
│                 + text_weight(0.3) × text_score
│
├── [Optional] apply_temporal_decay()
│   ├── Evergreen files exempt (MEMORY.md, undated memory/*.md)
│   ├── Date from filename (memory/YYYY-MM-DD.md) or mtime
│   └── score *= exp(-ln(2)/half_life × age_days)
│
├── [Optional] mmr_rerank()  (Maximal Marginal Relevance)
│   ├── Jaccard similarity between tokenized chunks
│   └── MMR = λ × relevance - (1-λ) × max_similarity
│
├── Filter by min_score (default 0.35)
├── Take max_results (default 6)
└── Truncate snippets at 700 chars
```

## Memory Flush

Pre-compaction memory flush for long-running sessions:

```
should_run_memory_flush()
├── Conditions: enabled, tokens>0, context>0, threshold>0
├── Not already flushed at current compaction count
└── total_tokens >= context_window - reserve_floor - soft_threshold
    │                                (20K tokens)    (4K tokens)
    ▼
Inject via ClaudeStdinInjector:
"Pre-compaction memory flush. Store durable memories now
 (use memory/YYYY-MM-DD.md; create memory/ if needed).
 IMPORTANT: If the file already exists, APPEND new content only..."
```

## Configuration Defaults

```
MemoryConfig
├── chunk_tokens: 400          # Max tokens per chunk
├── chunk_overlap: 80          # Overlap between chunks
├── max_results: 6             # Search result limit
├── min_score: 0.35            # Minimum relevance score
├── vector_weight: 0.7         # Hybrid search vector weight
├── text_weight: 0.3           # Hybrid search text weight
├── candidate_multiplier: 4    # Over-fetch factor
├── mmr_enabled: false         # Maximal Marginal Relevance
├── mmr_lambda: 0.7            # MMR diversity parameter
├── temporal_decay_enabled: false
├── temporal_decay_half_life_days: 30
├── watch_enabled: true        # File watcher
├── watch_debounce_ms: 1500    # Watcher debounce
├── cache_enabled: true        # Embedding cache
└── cache_max_entries: 10000   # Cache size limit
```

## MemoryStore Schema (`memory.sqlite`)

```
meta               (key TEXT PK, value TEXT)
files              (path TEXT PK, source, hash, mtime, size)
chunks             (id TEXT PK, path, source, start_line, end_line, hash,
                    model, text, embedding TEXT, updated_at INTEGER)
chunks_fts         (VIRTUAL FTS5: text, id, path, source, model, start/end_line)
embedding_cache    (provider, model, provider_key, hash — composite PK,
                    embedding TEXT, dims, updated_at)
```

FTS5 and sqlite-vec are optional — the system degrades gracefully if unavailable.
