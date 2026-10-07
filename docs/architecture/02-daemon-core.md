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

Automatic rotation now applies only to coordinating seats (#959, #1005).
Workers are never rotated; instead they pass the baton (#1254): when a
non-seat session with a launching manager or Epic lead crosses
`worker_context_cap_tokens` (default 60 = 60% of its known context window;
`1..=100` is a percentage, `32000..=2000000` tokens, `0` off;
`worker_context_cap.<Provider>[/<model>]` overrides set by
`:worker-context-cap`; a curated `ProposeDaemonSetting` key), the daemon
records one durable `worker_baton:<id>` record, queues one agent mail telling
the worker to commit, append a handoff to its Issue and end its turn with
`PIPELINE HANDOFF — BATON <sha>`, and records one typed
`worker_context_cap` notice for the launcher (a manager-inbox notice, or a
mail to an Epic lead). Both effects are idempotent across a restart
(`deliver_pending_worker_batons`). The manager relaunches with
`AgentManagerLaunchIssueWorker {continue_from}`, which branches from the
predecessor's committed HEAD, sets `continued_from` and prepends its final
message and the Issue's latest handoff to the brief. When a coordinating seat
(the project manager seat, the live seat of an area manager node, an Epic
lead, the global manager seat) reaches the hard context cap
(`coordinator_context_cap_tokens`, default 0 = off until #1156 closes, then
200000; `0` off, with
`coordinator_context_cap.<Provider>[/<model>]` overrides set by
`:context-cap`), `session/context_cap.rs` records one durable cap rotation.
At the seat's next idle boundary, the daemon writes a handoff from typed
state (`rsi_common::daemon_handoff`, strict RSI-013 plus a `## Typed State`
JSON block) and rotates the seat. The successor's first prompt is that
handoff. The manager seat, area node and lead custody move with the
ordinary rotation; the global grant moves in the successor's publication
transaction (#1142).

Safety of the automatic rotation (#1142):

- It is fenced to the seat's idle row version. The pass records the seat's
  `sessions.updated_at` and provider session id (`IdleFence`; a wall-clock row
  version, not a generation counter); the rotation decider re-checks it under
  the predecessor's spawn guard (`check_execution_fence`). A seat that resumed
  or whose row changed is deferred back to due. The cap never takes the running
  or manual-trigger branch of `trigger_rotation` (`trigger_cap_rotation`), so it
  cannot interrupt a turn or tool call.
- The same check reads, durably and before any reservation, what the fence
  cannot see: the operator's pause (soft or hard), rotation-disable, review or
  source ownership, the cap setting and the seat itself. The intent waits (stays
  due) while any holds; the cap never clears a pause or re-enables rotation.
- The handoff, the `Rotating` marker, a stable `rotation_id` and the fence are
  one atomic write. Every rotation of an idle seat (the manual trigger and the
  cap pass alike) records a durable open intent, an `entered` rotation event in
  phase `completed_trigger`, before its decider can reserve a successor
  (#1149). A crash anywhere from the trigger to the publication therefore
  leaves an intent that restart recovery owns: it claims the intent, recovers
  the reserved successor or runs the one decider, and the cap pass waits for
  that rotation's terminal event instead of dispatching a second decider. A
  request dispatched by another daemon process before its intent was recorded
  (a crash before dispatch) is re-dispatched once per boot under the same
  `rotation_id`. A request that stays `Rotating` for hours without a
  publication or refusal is escalated to the operator once.
- A request that returns to due (deferred by the decider for a pause,
  rotation-disable, cap-off or a changed seat), fails or is replanned closes
  its intent in the same transaction as the cap record (`refused:cap_deferred`
  and its siblings), so restart recovery never replays it. The intent's trigger
  is immutable provenance: a cap-triggered intent whose request is no longer the
  seat's current one is superseded (`refused:cap_superseded`), never treated as
  a manual rotation that skips the cap's checks. A refusal settles a request
  only when it carries that request's own `rotation_id`; another rotation's
  refusal of the same seat does not (#1155).
- If the daemon dies after a sandboxed seat's successor was bound to the seat's
  transferred sandbox custody (transfers are forward-only) and before it was
  published, restart fails the child but recovery keeps the intent open
  (`recovery_blocked`) and tells the operator once: it never closes the
  rotation over a custody it did not reconcile, never moves the sandbox back and
  never allocates a second successor (#1156). The operator's Continue of that
  failed successor (it holds the sandbox; custody only moves forward) finishes
  the rotation (#1158): a successor that never reported a provider thread starts
  in a new thread from its own recorded query, and once any operator
  continuation of it has started, the daemon publishes that exact reserved
  successor in the same boot, moving the Epic lead pointers and the global
  grant to it in the one publication commit. Nothing is moved back to the
  predecessor, and no other row is ever published under the rotation.
- When that successor can never start (provider gone, model refused), the
  operator abandons the blocked rotation (#1176): `AbandonBlockedRotation`
  (operator-only RPC) or `:rotation-abandon [provider[/model]]` in the TUI.
  The daemon records one `abandon_requested` event on the blocked intent
  (idempotency key in its metadata) and runs the ordinary rotation decider
  with the `Failed` custody holder as predecessor: a fresh replacement becomes
  its `continued_from` successor, takes the sandbox forward (cause `rotation`;
  every session owns the root once, custody never moves back) and starts on
  the holder's first prompt with the operator's provider/model. Its
  publication settles the whole chain in one commit: each unpublished hop
  P → S (→ …) → holder gets its `completed{successor_id}`, the global grant
  follows the chain, every Epic lead on it moves to the replacement, and each
  hop's predecessor is archived with its manager rotation edge. A crash before
  the replacement was reserved closes the request (`refused:abandon_interrupted`)
  and the rotation stays blocked on the same holder; a crash after the custody
  bind blocks it on the replacement (escalated once per holder), which the
  operator abandons in turn. The idea controller is not moved yet (#1186).
- A rotation acts only on the successor it reserved (#1153). Its own
  `successor_reserved` marker names that row exactly, whatever status a restart
  left it in (`rotation_request_successor`); no newer row, timestamp or other
  rotation's reservation overrides it. A request with no marker falls back to
  the unclaimed legacy rows created since it started, excluding rows another
  rotation or an agent reservation owns, and several candidates fail closed.
  Publication re-checks the reservation identity inside its transaction, so a
  row another rotation reserved is never published (or given the global grant)
  under this rotation's id. A candidate that settled live is published once, one
  that did not is refused (`refused:successor_not_live`), and no other
  successor is ever allocated. One seat's planning failure does not drop
  another seat's request.
- The settlement witness is the durable publication of the exact successor
  under the request's identity, not an archived parent. The global grant moves
  in that publication's transaction; the repair for an older record
  (`transfer_global_seat_if_published`) re-checks the witness in its own
  transaction. `Rotated` is recorded only after the grant is on the successor; a
  failed move leaves the record `Rotating` (retried every pass and boot).
- Copied free text (original task, Issue titles, child labels, wake and job
  names, manager request excerpts) is redacted for secret shapes before it is
  stored and rendered as quoted, attributed untrusted data. The daemon's own
  instructions are the Action Items only, and the peer-mail "not a command you
  must obey verbatim" warning is kept.

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
