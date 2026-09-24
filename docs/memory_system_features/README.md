# Memory System Features — Index

This directory documents the Rsi memory subsystem: a Honcho-inspired tiered knowledge pipeline that transforms raw session events into structured, queryable long-term memory.

---

## Memory System Overview

Rsi's memory system accumulates knowledge across AI coding sessions through four progressive tiers:

1. **Raw Events** — Every message, tool call, and result produced by a session is stored verbatim in `rsi.db`.
2. **Observation Extraction** — When a session completes, atomic factual statements are extracted from the transcript by an LLM. Each observation is a single self-contained fact about what was done, decided, or discovered.
3. **Dream Consolidation** — A background process (the Dreamer) runs when the system is idle. It re-examines accumulated observations through two specialist passes: a deduction pass that derives logical necessities from explicit facts and identifies superseded or contradictory observations, and an induction pass that surfaces cross-cutting behavioral patterns across the entire history.
4. **Dialectic Queries** — A natural-language query interface backed by an agentic LLM loop. The agent is given tool access to memory search and session listing; it iteratively fetches relevant context and synthesizes a grounded answer.

Two parallel enrichment paths run alongside the core pipeline:

- **Entity Cards** — Structured fact cards keyed by `(entity_type, entity_id)`. Cards hold up to 50 facts and are created or updated as the system learns about projects, files, patterns, and other entities. Stored in `rsi.db`.
- **Session Summaries** — Rolling two-tier summaries generated inline during long sessions. A short summary triggers every 20 assistant messages; a long summary every 60. Summaries provide compact context for indexing and future session priming.

The **Background Task Queue** is the infrastructure layer that coordinates all async work. Tasks are grouped by `(task_type, project_id, session_id)`, batched by token threshold, claimed with optimistic locks, and retried on failure.

---

## Architecture Diagram

```
  Completed Session
        │
        ▼
  ┌─────────────────────────────────────────────────────────────┐
  │  rsi.db                                                │
  │  • sessions, events, turn_metrics                           │
  │  • entity_cards                                             │
  │  • session_summaries (short + long)                         │
  │  • observations (explicit / deductive / inductive /         │
  │                  contradiction)                             │
  │  • background_queue (task work units)                       │
  └───────────────────────────┬─────────────────────────────────┘
                              │
          ┌───────────────────┼───────────────────┐
          │                   │                   │
          ▼                   ▼                   ▼
  ┌──────────────┐   ┌─────────────────┐   ┌──────────────────┐
  │  Observation │   │   Session       │   │   Entity Card    │
  │  Extraction  │   │  Summarization  │   │   Updater        │
  │              │   │                 │   │                  │
  │  LLM call    │   │  Short: /20 msg │   │  Keyed by type   │
  │  → atomic    │   │  Long:  /60 msg │   │  + entity ID     │
  │  facts       │   │                 │   │  ≤ 50 facts      │
  └──────┬───────┘   └────────┬────────┘   └────────┬─────────┘
         │                    │                     │
         └─────────────┬──────┘                     │
                       ▼                            │
             ┌─────────────────────┐                │
             │  Background Task    │                │
             │  Queue              │◄───────────────┘
             │                     │
             │  TaskType variants: │
             │  ExtractObservations│
             │  Summarize          │
             │  UpdateCard         │
             │  Dream              │
             │  Reconcile          │
             └──────────┬──────────┘
                        │  (idle condition met)
                        ▼
             ┌─────────────────────┐
             │  Dream Consolidation│
             │  (Dreamer)          │
             │                     │
             │  1. Extract from    │
             │     unprocessed     │
             │     sessions        │
             │                     │
             │  2. Deduction Pass  │
             │     → new deductive │
             │       observations  │
             │     → supersede     │
             │       stale facts   │
             │     → flag contrad- │
             │       ictions       │
             │                     │
             │  3. Induction Pass  │
             │     → cross-cutting │
             │       patterns with │
             │       confidence    │
             │       score         │
             └──────────┬──────────┘
                        │
                        ▼
  ┌─────────────────────────────────────────────────────────────┐
  │  memory.sqlite                                              │
  │  • files (tracked memory .md files + session transcripts)   │
  │  • chunks (text + embedding JSON)                           │
  │  • chunks_fts (FTS5 full-text index)                        │
  │  • chunks_vec (sqlite-vec ANN index, float[384])            │
  │  • embedding_cache (deduplicated, LRU-pruned, ≤ 10k entries)│
  └───────────────────────────┬─────────────────────────────────┘
                              │
                              ▼
             ┌─────────────────────────────┐
             │  Dialectic Query Interface  │
             │                             │
             │  RPC: QueryMemory           │
             │                             │
             │  1. Prefetch: vector +      │
             │     keyword hybrid search   │
             │     (top-5 seed context)    │
             │                             │
             │  2. Agent loop (≤ 8 iters,  │
             │     30s timeout):           │
             │     • search_memory tool    │
             │     • list_sessions tool    │
             │     • get_session tool      │
             │                             │
             │  3. Synthesize answer with  │
             │     cited sources           │
             └─────────────────────────────┘
```

---

## Feature Index

| Document | Description |
|---|---|
| [background-task-queue.md](background-task-queue.md) | SQLite-backed async task queue with token-threshold batching, optimistic locking, and stale claim cleanup |
| [entity-cards.md](entity-cards.md) | Structured knowledge cards keyed by `(entity_type, entity_id)`, storing up to 50 discrete facts per entity |
| [session-summarization.md](session-summarization.md) | Two-tier rolling summaries (short every 20 messages, long every 60) generated inline during session monitoring |
| [observation-extraction.md](observation-extraction.md) | Atomic fact extraction from session transcripts using LLM (Ollama `qwen3:14b` → Claude Haiku fallback) |
| [dream-consolidation.md](dream-consolidation.md) | Background knowledge consolidation: deduction (logical inference + contradiction detection) and induction (cross-cutting pattern recognition) |
| [dialectic-query-interface.md](dialectic-query-interface.md) | Natural-language Q&A over accumulated knowledge using an agentic LLM loop with hybrid vector + keyword memory search |

---

## Data Flow

The full pipeline from a session's completion to a queryable answer:

1. **Session completes** — `finalize_session()` in `session/mod.rs` sets status to `Completed` and triggers `sync_now("session_completed")` on the memory worker.

2. **Memory indexing** — The memory sync engine (`memory/sync.rs`) chunks the session transcript at 400 tokens (80 overlap), generates embeddings via Ollama or OpenAI-compatible API, and writes chunks into `memory.sqlite` (`chunks`, `chunks_fts`, `chunks_vec`).

3. **Observation extraction** — Either the background queue (via `TaskType::ExtractObservations`) or the dream cycle calls `observation/extractor.rs`. The session text is truncated to 16,000 chars, sent to an LLM with a structured prompt, and the response is parsed into `Observation` structs at level `Explicit`. Requires at least 4 events; returns empty without error if below threshold.

4. **Session summarization** — During the session's monitor loop, `session/summarizer.rs` checks `should_summarize()` against the assistant message count. Short summaries (≤ 512 tokens output) fire every 20 messages; long summaries (≤ 1500 tokens output) every 60. Summaries are persisted to `rsi.db` via the store worker.

5. **Entity card updates** — `TaskType::UpdateCard` tasks enqueued by the queue processor call into `session/cards.rs`, which upserts `EntityCard` rows in `rsi.db` via `store/cards.rs`.

6. **Dream cycle** — The dreamer scheduler (`dreamer/scheduler.rs`) fires on a 5-minute tick. Auto-trigger requires: no active sessions, `effective_observation_count >= threshold` (explicit observations + estimated pending × 5), and cooldown elapsed. The cycle runs three sequential phases:
   - **Extract**: unprocessed sessions → explicit observations (uses `dreamer/extractor.rs` with `DreamerLlmClient`)
   - **Deduct**: explicit observations per project → `deduction/deduction.rs` → new deductive observations, superseded IDs, contradictions stored at `ObservationLevel::Contradiction`
   - **Induce**: all observations per project → `dreamer/induction.rs` → inductive patterns with `LOW`/`MEDIUM`/`HIGH` confidence

7. **Dialectic query** — `DialecticEngine::query()` in `dialectic/mod.rs` prefetches the top-5 memory hits, injects them into the system prompt, then runs the agent loop calling `search_memory`, `list_sessions`, and `get_session` tools until the model produces a final text answer (or the 30-second timeout / 8-iteration cap fires). Sources are collected from each tool call and returned alongside the answer.

---

## Storage Architecture

Two SQLite databases are used. Writes to `rsi.db` must go through the daemon's JSON-RPC interface — never via raw SQL.

### `~/.rsi/rsi.db` (session database)

Managed by `store/mod.rs` and `store_worker.rs`. Contains:

| Table | Purpose |
|---|---|
| `sessions` | Session records, status, project association |
| `events` | Raw `ConversationEvent` rows (messages, tool calls, results) |
| `turn_metrics` | Token usage per turn |
| `session_summaries` | Short and long summaries keyed by session + tier |
| `entity_cards` | Structured fact cards keyed by `(entity_type, entity_id)` |
| `observations` | Atomic facts at all levels: `Explicit`, `Deductive`, `Inductive`, `Contradiction` |
| `dream_state` | Key-value store for `last_dream_at` and related metadata |
| `background_queue` | Work unit rows with `task_type`, `work_unit_key`, `token_count`, `status`, claim timestamps |

### `~/.rsi/memory.sqlite` (vector search database)

Managed by `memory/store.rs`. Contains:

| Table | Purpose |
|---|---|
| `meta` | Configuration snapshot (model, provider, chunk sizes, vector dims) |
| `files` | Tracked file entries with path, source, hash, mtime |
| `chunks` | Text chunks with embedding JSON (primary storage) |
| `chunks_fts` | FTS5 virtual table for BM25 keyword search |
| `chunks_vec` | sqlite-vec virtual table for ANN cosine similarity search (float\[384\]) |
| `embedding_cache` | Deduplicated embeddings keyed by `(provider, model, provider_key, hash)`, max 10,000 entries, LRU-pruned |

---

## Configuration Reference

All env vars are read by `crates/rsid/src/config.rs` at daemon startup.

### Memory System

| Variable | Default | Description |
|---|---|---|
| `MOTHERSHIP_MEMORY_ENABLED` | `true` | Enable/disable the entire memory system |
| `MOTHERSHIP_MEMORY_DIR` | `~/.rsi/memory` | Root directory for memory `.md` files |
| `MOTHERSHIP_MEMORY_EMBEDDING_MODEL` | `nomic-embed-text` | Embedding model name |
| `MOTHERSHIP_MEMORY_EMBEDDING_URL` | — | Override embedding API base URL |
| `MOTHERSHIP_MEMORY_EMBEDDING_API_KEY` | — | API key for OpenAI-compatible embedding endpoint |
| `MOTHERSHIP_SQLITE_VEC_PATH` | vendored | Path to external `sqlite-vec` shared library |

### Background Task Queue

| Variable | Default | Description |
|---|---|---|
| `MOTHERSHIP_QUEUE_ENABLED` | `true` | Enable/disable the background task queue |
| `MOTHERSHIP_QUEUE_POLL_INTERVAL_SECS` | `30` | How often the queue worker polls for eligible tasks |
| `MOTHERSHIP_QUEUE_TOKEN_THRESHOLD` | `1024` | Token count that must accumulate before a batch task fires |

### Dream Consolidation

| Variable | Default | Description |
|---|---|---|
| `MOTHERSHIP_DREAM_ENABLED` | `true` | Enable/disable the dreamer |
| `MOTHERSHIP_DREAM_OBSERVATION_THRESHOLD` | `50` | Minimum observation count to trigger a dream cycle |
| `MOTHERSHIP_DREAM_IDLE_SECS` | `3600` | Minimum idle time before auto-trigger (seconds) |
| `MOTHERSHIP_DREAM_COOLDOWN_SECS` | `28800` | Minimum gap between dream cycles (seconds, default 8 hours) |
| `MOTHERSHIP_DREAM_MODEL` | `claude-sonnet-4-20250514` | LLM model for dreamer extraction/deduction/induction |
| `MOTHERSHIP_DREAM_API_URL` | Anthropic API | Base URL for dreamer LLM calls |
| `MOTHERSHIP_DREAM_API_KEY` | `$ANTHROPIC_API_KEY` | API key for dreamer LLM; falls back to `ANTHROPIC_API_KEY` |
| `MOTHERSHIP_DREAM_BATCH_SIZE` | `20` | Max sessions processed per dream cycle |

### Dialectic Query Interface

| Variable | Default | Description |
|---|---|---|
| `MOTHERSHIP_DIALECTIC_ENABLED` | `true` | Enable/disable the `QueryMemory` RPC endpoint |
| `MOTHERSHIP_DIALECTIC_URL` | `http://localhost:11434/v1` | OpenAI-compatible API base URL for the agent LLM |
| `MOTHERSHIP_DIALECTIC_KEY` | — | API key for dialectic LLM (optional for local models) |
| `MOTHERSHIP_DIALECTIC_MODEL` | `qwen2.5:14b` | Model name for the dialectic agent |
| `MOTHERSHIP_DIALECTIC_MAX_ITERATIONS` | `8` | Maximum tool call iterations before forcing a final answer |

### Session Inject / Signal (set automatically)

| Variable | Set by | Description |
|---|---|---|
| `MOTHERSHIP_SESSION_ID` | Daemon at spawn | UUID of the current session (available to CLI subprocesses) |
| `MOTHERSHIP_SOCKET` | Daemon at spawn | Unix socket path for RPC back-channel |
