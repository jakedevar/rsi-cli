# Daemon Architecture

> `rsid` — the process orchestrator and persistence engine

## Startup Sequence

```
main()
├── 1. Init tracing
├── 2. Load Config from env vars
├── 3. Check provider availability (Claude, Codex, OpenCode, Local, Gemini, Nullclaw)
├── 4. Create data dir (~/.rsi/)
├── 5. Remove stale socket, bind Unix listener (0o600)
├── 6. Open SQLite store (~/.rsi/rsi.db)
├── 7. Create EventBus (broadcast channel)
├── 8. Init memory system (optional: MemoryStore + embeddings + worker)
├── 9. Create SessionManager
├── 10. Restore sessions from DB (Running/Starting → Failed, pending_archive → Archived)
├── 11. Spawn stall detector (60s interval)
├── 12. Create RpcServer
├── 13. Spawn retry handler loop
└── 14. Enter accept loop (tokio::select on ctrl_c + listener.accept)
```

## Core Components

```
┌─────────────────────────────────────────────────────┐
│                    rsid                         │
│                                                     │
│  ┌──────────────┐    ┌──────────────┐              │
│  │  RpcServer    │←──→│ SessionManager│              │
│  │  (per-conn    │    │              │              │
│  │   tokio task) │    │  active:     │              │
│  └──────┬───────┘    │  HashMap<Uuid,│              │
│         │            │  TrackedSess> │              │
│         │            │              │              │
│    Subscribe ──→     │  completed:  │              │
│    run_subscribe_    │  HashMap<Uuid,│              │
│    stream()          │  CompletedS> │              │
│         │            └──────┬───────┘              │
│         │                   │                       │
│         ▼                   ▼                       │
│  ┌──────────────┐   ┌──────────────┐              │
│  │   EventBus    │   │  Provider     │              │
│  │  (broadcast)  │   │  Clients      │              │
│  │              │   │  ┌─────────┐  │              │
│  │  DaemonEvent  │   │  │ Claude  │  │              │
│  │  → BusEvent   │   │  │ Codex   │  │              │
│  │  → JSON push  │   │  │ OpenCode│  │              │
│  └──────────────┘   │  │ Local   │  │              │
│                      │  │ Local   │  │              │
│                      │  │ Gemini  │  │              │
│  ┌──────────────┐   │  │ Nullclaw│  │              │
│  │ Persistence   │   │  └─────────┘  │              │
│  │ Handle        │   └──────────────┘              │
│  │ (mpsc→worker) │                                  │
│  └──────┬───────┘   ┌──────────────┐              │
│         │            │ StallDetector │              │
│         ▼            │ (60s interval)│              │
│  ┌──────────────┐   └──────────────┘              │
│  │ Store (SQLite)│                                  │
│  │ rsi.db   │   ┌──────────────┐              │
│  └──────────────┘   │ MemoryManager │              │
│                      │ (optional)    │              │
│                      └──────────────┘              │
└─────────────────────────────────────────────────────┘
```

## SessionManager

The central coordinator. Holds:

| Field | Type | Purpose |
|---|---|---|
| `active` | `Arc<RwLock<HashMap<Uuid, TrackedSession>>>` | Running sessions |
| `completed` | `Arc<RwLock<HashMap<Uuid, CompletedSession>>>` | Finished sessions |
| `event_bus` | `Arc<EventBus>` | Pub/sub for status events |
| `{provider}_client` | `Option<{Provider}Client>` | One per available provider |
| `store` | `Arc<Mutex<Store>>` | SQLite for queries |
| `persistence` | `PersistenceHandle` | Async write channel |
| `project_index` | `Arc<RwLock<ProjectIndex>>` | Longest-prefix path matching |
| `token_counter` | `Arc<TokenCounter>` | BPE tokenizer (cl100k_base) |
| `memory_handle` | `Option<MemoryHandle>` | For lifecycle hooks |
| `retry_tx/rx` | `mpsc::Sender/Receiver<Uuid>` | Failed session retry queue |

## Session Lifecycle

### Launch Flow

```
LaunchSession RPC
    │
    ▼
launch_session()  ← synchronous portion
    ├── Generate UUID
    ├── Resolve working_dir (fallback: cwd → /tmp)
    ├── Resolve project via ProjectIndex
    ├── Assemble ContextPipeline (system prompt)
    ├── provider.launch(&config) → (ProviderProcess, event_rx)
    ├── Resolve git branch
    ├── Build Session struct (Starting)
    ├── Insert into active map
    └── Return session_id immediately
         │
         ▼ (tokio::spawn background)
    ├── Persist session row
    ├── Create initial user ConversationEvent
    ├── Publish bus events
    ├── Count initial tokens
    ├── Spawn title generation
    └── monitor_session() ← blocks until session ends
```

### Monitor Loop

```
monitor_session()
    │
    tokio::select! {
    │   event_rx.recv() ──→ process StreamEvent
    │       ├── "system" init  → capture provider session ID, set Running
    │       ├── "assistant"    → extract tokens, check rotation threshold,
    │       │                    build ConversationEvent, persist, publish
    │       ├── "user"         → build ConversationEvent, persist
    │       ├── "tool_use"     → detect handoff/pipeline files, persist
    │       ├── "tool_result"  → persist
    │       ├── "result"       → extract metadata, build turn metric, break
    │       ├── "thinking"     → persist
    │       ├── "question"     → set pending_question
    │       └── "error"        → persist as System event
    │
    │   stop_rx.recv() ──→ break (Interrupted or Rotation)
    │
    │   deadline (WritingHandoff timeout) ──→ break
    }
    │
    ▼
finalize_session()
    ├── Determine final status:
    │   interrupt_requested? → Interrupted
    │   pending_question?    → WaitingApproval
    │   no output?           → Failed
    │   exit_code != 0?      → Failed
    │   otherwise            → Completed
    ├── Move active → completed
    ├── Persist final status
    ├── Advance workflow if applicable
    ├── Schedule retry if Failed + max_retries > 0
    └── Trigger memory sync
```

### Context Rotation

State machine per session via `RotationCoordinator`:

```
                ThresholdCheck(pct>=65%)
Idle ──────────────────────────────────→ PendingInterrupt  (depth < 4)
  │                                            │
  │   ThresholdCheck(pct>=65%, depth>=4)      │ MonitorCompleted
  └───────────────────→ DepthLimitHit         ▼
                                        SendCreateHandoff
                                              │
                                              ▼
                                        WritingHandoff (300s timeout)
                                              │ HandoffFileDetected
                                              │ MonitorCompleted
                                              ▼
                                        SpawnChild { handoff_filepath }
                                              │
                                              ▼
                                        New session (depth+1, archives parent)
```

## Context Pipeline

Assembles system prompt context from multiple sources:

```
ContextPipeline::assemble()
    │
    tokio::join! (3s master timeout)
    ├── gather_project_files()   [500ms] → priority 0-1
    ├── gather_memory()          [2s]    → priority 2
    └── gather_git_log()         [1s]    → priority 3
    │
    ▼
assemble_within_budget()
    ├── Budget = context_window / 50 (2%, min 200 tokens)
    ├── Include blocks in priority order until budget exhausted
    └── Truncate oversized blocks if > 50% of budget
```

## Provider Clients

All providers share `LaunchConfig` and return `(ProviderProcess, mpsc::Receiver<StreamEvent>)`.

| Provider | CLI Command | Output Format | Notes |
|---|---|---|---|
| Claude | `claude -p <q> --output-format stream-json` | NDJSON | `--resume`, `--model`, `--system-prompt` |
| Codex | `codex exec --json --sandbox workspace-write` | Custom JSON | Maps to StreamEvent format |
| OpenCode | `opencode run --format json` | JSON | Requires model param |
| Gemini | `gemini -p <q> --output-format stream-json` | NDJSON | Same as Claude format |
| Local | OpenAI API (`/v1/chat/completions`) | SSE stream | Uses conversation history |
| Nullclaw | `nullclaw agent -m <q>` | Plain text | Line-by-line to StreamEvent |

## EventBus

```
DaemonEvent enum (11 variants)
├── SessionStatusChanged { session_id, old_status, new_status }
├── ConversationEvent { session_id, event }
├── SessionDeleted/Archived/Unarchived { session_id }
├── SessionMetadataChanged { session_id, model, pinned_at, project_id, ... }
├── ContextUsageUpdated { session_id, pct, tokens... }
├── SessionStalled { session_id, status, idle_secs }
├── SessionRetrying { session_id, attempt, max_retries, backoff_ms, reason }
├── MemoryIndexUpdated { file_count, chunk_count }
└── SystemMessage { level, message }

publish() only sends if subscriber_count > 0 (zero-overhead when no TUI connected)
```

## RPC Server

Per-connection handler with two modes:

1. **Request-response**: read JSON line → dispatch to handler → write JSON response
2. **Subscribe streaming**: after `Subscribe` ack, enters `run_subscribe_stream()` — reads from EventBus broadcast channel, filters by params, writes newline-delimited BusEvent JSON until disconnect

## Stall Detector

60-second interval task. For each active session, compares `now - last_event_at` against thresholds (Running: 30min, WaitingApproval: 60min). Fires `SessionStalled` event once per crossing. Prunes reported set when sessions complete.

## Configuration (`config.rs`)

All fields from environment variables:

| Env Var | Default | Purpose |
|---|---|---|
| `MOTHERSHIP_SOCKET` | `~/.rsi/daemon.sock` | Socket path |
| `MOTHERSHIP_EVENT_BUFFER` | `100` | EventBus broadcast capacity |
| `MOTHERSHIP_CONTEXT_ROTATION_ENABLED` | `false` | Enable auto-rotation |
| `MOTHERSHIP_MEMORY_ENABLED` | `true` | Enable memory subsystem |
| `MOTHERSHIP_MEMORY_DIR` | `~/.rsi/memory` | Memory file directory |
| `MOTHERSHIP_MEMORY_EMBEDDING_MODEL` | `nomic-embed-text` | Embedding model |
| `MOTHERSHIP_STALL_TIMEOUT_RUNNING_SECS` | `1800` | 30min stall threshold |
| `MOTHERSHIP_STALL_TIMEOUT_WAITING_SECS` | `3600` | 60min stall threshold |
| `MOTHERSHIP_STALL_DETECTION_ENABLED` | `true` | Enable stall detector |

## Error Types

`DaemonError` enum covers: I/O, SessionNotFound/Exists, binary-not-found per provider, OpenAI API errors, JSON/RPC/Process errors, ChannelClosed, InvalidParam, Store/Database errors.
