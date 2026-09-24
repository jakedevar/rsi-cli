# Shared Types & RPC Protocol

> `rsi-common` — the contract crate shared by TUI and daemon

---

## What is RPC? (Conceptual Primer)

**The one-line version:** RPC (Remote Procedure Call) is calling a function in another process as if it were local.

### Why the app needs it

`rsi` (TUI) and `rsid` (daemon) are two separate OS processes. They don't share memory. The TUI handles rendering and keyboard input; the daemon does the actual work — spawning Claude/Codex subprocesses, writing to SQLite, tracking session state. They're split intentionally: the daemon keeps running even if the TUI crashes or you close the window.

The problem: the TUI needs to *tell* the daemon things ("launch a session", "list my sessions") and *get data back*. RPC is the communication contract that makes that possible.

### The architecture

```
┌─────────────────┐        Unix Socket          ┌──────────────────┐
│   rsi (TUI)     │  ~/.rsi/daemon.sock          │   rsid (Daemon)  │
│                 │ ──────────────────────────►  │                  │
│  keypress → ... │   { method: "ListSessions" } │  queries SQLite  │
│                 │ ◄──────────────────────────  │  returns rows    │
│                 │   { result: [...sessions] }  │                  │
└─────────────────┘                              └──────────────────┘
```

The transport is a **Unix socket** (a local pipe between two processes on the same machine) — faster than HTTP, no TCP overhead, file-permission–based auth (only your user can touch the socket file).

### The wire format — JSON-RPC 2.0

A standardized envelope for the messages. Every call looks like:

```json
// Request (TUI → Daemon)
{ "jsonrpc": "2.0", "id": 42, "method": "LaunchSession", "params": { "working_dir": "/home/jake/project", "provider": "Claude" } }

// Response (Daemon → TUI)
{ "jsonrpc": "2.0", "id": 42, "result": { "session_id": "abc-123", "status": "Starting" } }
```

The `id` field pairs responses back to requests — the TUI can fire multiple calls without blocking and match responses as they arrive.

### What counts as an "RPC method"

Every named operation in the table below is an RPC method — essentially the internal API surface between TUI and daemon. When you hear "add an RPC method," it means: define a new named operation so the TUI can request a new behavior from the daemon.

---

## Crate Structure

```
rsi-common/src/
├── lib.rs          # Re-exports both modules at crate root
├── types.rs        # All domain types
└── rpc.rs          # JSON-RPC 2.0 request/response types + method params
```

Both `types` and `rpc` are `pub use *` re-exported — consumers import directly as `rsi_common::Session`.

---

## Domain Types (`types.rs`)

### Session — The Primary Entity

```
Session
├── Identity (cache line 1)
│   ├── id: Uuid
│   ├── status: SessionStatus
│   ├── session_kind: SessionKind          # Standard | TaskRabbit | Bug
│   ├── provider: SessionProvider          # Claude | Codex | OpenCode | Local | Gemini | Nullclaw
│   ├── context_usage_confidence: ContextUsageConfidence
│   ├── rotation_depth: u32               # 0 = original, blocked at 4
│   ├── retry_attempt: Option<u8>
│   ├── max_retries: Option<u8>
│   ├── created_at / updated_at: DateTime<Utc>
│   ├── pinned_at: Option<DateTime<Utc>>
│   └── testing_needed_at: Option<DateTime<Utc>>
│
├── Display (cache lines 2-3)
│   ├── query: String
│   ├── title: Option<String>             # Haiku-generated or user-set
│   ├── description: Option<String>       # LLM-generated paragraph
│   ├── working_dir: PathBuf              # REQUIRED
│   ├── git_branch: Option<String>
│   └── model: Option<String>
│
├── Secondary Identity (cache line 4)
│   ├── claude_session_id: Option<String>  # Provider-side ID for --resume
│   ├── project_id: Option<Uuid>
│   ├── continued_from: Option<Uuid>       # Parent in rotation chain
│   ├── handoff_filepath: Option<String>
│   ├── active_task: Option<String>        # Injected into system prompt
│   ├── group_id: Option<Uuid>
│   └── stop_reason: Option<String>
│
└── Analytics (cold fields)
    ├── cost_usd, duration_ms, num_turns
    ├── input_tokens, output_tokens, context_window
    ├── total_input/output/cache_creation/cache_read_tokens
    ├── daemon_input/output_tokens          # No API dependency
    ├── pipeline_artifact: Option<String>
    ├── workflow_id: Option<Uuid>
    ├── pending_question: Option<PendingQuestion>
    └── pending_archive: bool
```

### SessionStatus — Lifecycle State Machine

```
Starting ──→ Running ──→ Completed
                │              │
                ├──→ WaitingApproval ──→ (back to Running)
                │
                ├──→ Failed
                ├──→ Interrupted
                └──→ Archived
```

No `Interrupting` state — interrupt tracking is local to the daemon's `TrackedSession`.

### SessionProvider

```
Claude (default)   — Claude CLI with stream-json output
Codex              — Codex CLI subprocess
OpenCode           — OpenCode CLI gateway
Local              — Local models via Ollama/llama.cpp
Gemini             — Google Gemini CLI
Nullclaw           — Zig-based agent, plain-text output
```

### ConversationEvent

```
ConversationEvent
├── id: i64                    # DB primary key (0 before persistence)
├── session_id: Uuid
├── sequence: i32              # Per-session monotonic
├── event_type: EventType      # Message | ToolUse | ToolResult | System | Thinking
├── role: Option<Role>         # User | Assistant (None for system/tool)
├── created_at: DateTime<Utc>
├── content: String            # Never Option — empty string when no content
├── tool_name: Option<String>
└── tool_input: Option<Box<Value>>  # Boxed to reduce hot-path size
```

### Supporting Types

```
Project         { id, name, path?, description?, color, context_files?, timestamps }
SessionGroup    { id, name, description?, project_id?, color, timestamps }
TurnMetric      { id, session_id, turn_number, token counts, stop_reason, tools_used, model }
ModelSegment    { id, session_id, model_id, from_sequence, to_sequence?, created_at }
Workflow        { id, title, stage: WorkflowStage, artifact_path?, project_id?, timestamps }
Approval        { id, session_id, tool_name, tool_input, status, timestamps }
EspGame         { id, played_at, score, rounds_played, total_rounds, p_value, round_details }
BusEvent        { event_type: String, timestamp, data: Value }
```

### WorkflowStage Pipeline

```
Research → ResearchComplete → Planning → PlanComplete → Implementing → ImplementComplete → Complete
```

### ContextUsageConfidence

```
Counted  — Daemon-counted from streamed content (real-time, no API dependency)
Full     — API-reported with cache tokens (exact)
Partial  — API-reported without cache tokens (approximation)
Missing  — No usage data (default; accumulator stale)
```

---

## RPC Protocol (`rpc.rs`)

### Wire Format

Newline-delimited JSON-RPC 2.0 over Unix socket (`~/.rsi/daemon.sock`).

```
→ {"jsonrpc":"2.0","id":1,"method":"ListSessions","params":null}\n
← {"jsonrpc":"2.0","id":1,"result":[...]}\n
```

### RPC Methods

| Category | Methods |
|---|---|
| **Session Lifecycle** | `LaunchSession`, `GetSession`, `ListSessions`, `DeleteSession`, `InterruptSession`, `ContinueSession`, `RotateSession`, `TogglePin`, `ToggleTestingNeeded`, `UpdateSessionProject/Title/Description`, `UpdateActiveTask`, `AnswerQuestion` |
| **Conversations** | `GetConversation`, `GetConversationsSince` (batched), `GetTurnMetrics`, `GetModelSegments` |
| **Archive** | `ArchiveSession`, `MarkPendingArchive`, `ListArchivedSessions`, `UnarchiveSession` |
| **Projects** | `ListProjects`, `CreateProject`, `UpdateProject`, `DeleteProject`, `GetProject` |
| **Groups** | `ListGroups`, `CreateGroup`, `UpdateGroup`, `DeleteGroup`, `UpdateSessionGroup` |
| **Workflows** | `ListWorkflows`, `GetWorkflow` |
| **Discovery** | `GetHealthStatus`, `GetDaemonCapabilities`, `DiscoverModels` |
| **Memory** | `MemorySearch`, `MemoryStatus`, `MemoryIndex`, `MemoryRead` |
| **Push** | `Subscribe` (switches connection to streaming mode) |
| **Games** | `SaveEspGame`, `ListEspGames` |

### Key Parameter Structs

- `LaunchSessionParams` — query, working_dir, provider, model, system_prompt, session_kind, project_id, continued_from, openai_base_url/api_key, workflow_id, max_retries, group_id
- `ContinueSessionParams` — session_id, query
- `GetConversationsSinceParams` — `Vec<ConversationFetchCursor>` (session_id + since_sequence)
- `SubscribeParams` — event_types filter, session_id filter
- `DaemonCapabilities` — version-gated feature flags (incremental_polling, batch_fetch, push_notifications, memory_search, workflows, stall_detection)

---

## rsi-signal Binary

Minimal CLI tool injected into session subprocess environments. Uses `MOTHERSHIP_SOCKET` and `MOTHERSHIP_SESSION_ID` env vars to send JSON-RPC requests back to the daemon.

```
rsi-signal archive   # Sends MarkPendingArchive for the owning session
```

Used by session subprocesses to signal lifecycle events back to the daemon without direct socket management.
