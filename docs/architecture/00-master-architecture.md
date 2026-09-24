# Rsi Architecture — Master Diagram

> Complete system architecture synthesizing all subsystems

## System Overview

Rsi is a vim-like TUI for managing multiple AI coding sessions across providers. It consists of four crates operating as a client-daemon architecture over Unix sockets.

```
┌──────────────────────────────────────────────────────────────────────┐
│                          Terminal (crossterm)                          │
│                                                                       │
│  ┌─────────────────────────────────────────────────────────────────┐ │
│  │                     rsi (TUI Client)                        │ │
│  │                                                                   │ │
│  │  ┌──────────┐   ┌───────────┐   ┌──────────┐   ┌────────────┐ │ │
│  │  │ Input     │──→│ Vim Machine│──→│ Action   │──→│ App State  │ │ │
│  │  │ Pipeline  │   │ (modalkit) │   │ Handlers │   │ Mutation   │ │ │
│  │  └──────────┘   └───────────┘   └──────────┘   └─────┬──────┘ │ │
│  │                                                         │        │ │
│  │  ┌──────────────────────────────────────────────────────┘        │ │
│  │  │                                                               │ │
│  │  ▼                                                               │ │
│  │  ┌──────────────┐   ┌──────────────┐   ┌──────────────────────┐│ │
│  │  │ Overlay      │   │ UI Renderer   │   │ State Persistence    ││ │
│  │  │ System (26)  │   │ (ratatui)     │   │ PersistedState       ││ │
│  │  └──────────────┘   └──────┬───────┘   │ DevState (hot-reload)││ │
│  │                             │            └──────────────────────┘│ │
│  │                             │                                     │ │
│  │  ┌──────────────┐          │            ┌──────────────────────┐│ │
│  │  │ Vim Textarea  │          │            │ Supporting Systems   ││ │
│  │  │ (full vim     │          │            │ • File Viewer        ││ │
│  │  │  emulation)   │          │            │ • Git Gutter         ││ │
│  │  └──────────────┘          │            │ • Clipboard (OSC 52) ││ │
│  │                             │            │ • Prompt Processor   ││ │
│  │                             │            │ • Suggestions        ││ │
│  │                             │            └──────────────────────┘│ │
│  └─────────────────────────────┼─────────────────────────────────────┘ │
│                                │                                       │
│         ┌──────────────────────┘                                       │
│         │  terminal.draw()                                             │
│         ▼                                                              │
│  ┌─────────────────────────────────────────────────────────────────┐  │
│  │                     Render Layers (painter's)                     │  │
│  │  [1] Split tree  [2] HUD Rails  [3] Status  [4] Cmd Bar  [5] Overlay │
│  └─────────────────────────────────────────────────────────────────┘  │
└──────────────────────────────────────────────────────────────────────┘
         │                    │
         │ DaemonClient       │ NotificationStream
         │ (JSON-RPC 2.0)     │ (push events)
         │                    │
    Unix Socket          Unix Socket
    (request/response)   (streaming)
         │                    │
         ▼                    ▼
┌──────────────────────────────────────────────────────────────────────┐
│                        rsid (Daemon)                             │
│                                                                       │
│  ┌──────────────┐                                                    │
│  │  RPC Server   │←─ per-connection tokio task                       │
│  │  ├── Request/Response mode                                        │
│  │  └── Subscribe streaming mode                                     │
│  └──────┬───────┘                                                    │
│         │                                                             │
│         ▼                                                             │
│  ┌──────────────────────────────────────────────────────────────┐   │
│  │                    SessionManager                              │   │
│  │                                                                │   │
│  │  ┌─────────────┐     ┌──────────────┐     ┌───────────────┐ │   │
│  │  │ active       │     │ completed     │     │ Provider      │ │   │
│  │  │ HashMap<Uuid,│     │ HashMap<Uuid, │     │ Clients       │ │   │
│  │  │ TrackedSess> │     │ CompletedS>   │     │ ┌───────────┐│ │   │
│  │  └──────┬──────┘     └──────────────┘     │ │Claude     ││ │   │
│  │         │                                   │ │Codex      ││ │   │
│  │         │  monitor_session()                │ │OpenCode   ││ │   │
│  │         │  (per-session tokio task)         │ │Local      ││ │   │
│  │         │                                   │ │Local      ││ │   │
│  │         ├── Parse StreamEvents              │ │Gemini     ││ │   │
│  │         ├── Build ConversationEvents        │ │Nullclaw   ││ │   │
│  │         ├── Track token usage               │ └───────────┘│ │   │
│  │         ├── Drive RotationCoordinator       └───────────────┘ │   │
│  │         └── finalize_session()                                 │   │
│  │              ├── Determine final status                        │   │
│  │              ├── Move active → completed                       │   │
│  │              ├── Schedule retry if needed                      │   │
│  │              └── Trigger memory sync                           │   │
│  │                                                                │   │
│  │  ┌─────────────────┐   ┌──────────────┐                      │   │
│  │  │ ContextPipeline  │   │ Rotation     │                      │   │
│  │  │ (system prompt)  │   │ Coordinator  │                      │   │
│  │  │ • Project files  │   │ (state machine│                     │   │
│  │  │ • Memory search  │   │  per session) │                     │   │
│  │  │ • Git log        │   │              │                      │   │
│  │  │ • Active task    │   │ Idle→Pending │                      │   │
│  │  └─────────────────┘   │ →Writing     │                      │   │
│  │                         │ →SpawnChild  │                      │   │
│  └─────────────────────┐  └──────────────┘                      │   │
│                         │                                         │   │
│  ┌──────────────┐      │   ┌──────────────┐                     │   │
│  │  EventBus     │◄─────┘   │ ProjectIndex  │                    │   │
│  │  (broadcast)  │          │ (prefix match)│                    │   │
│  │  11 event     │          └──────────────┘                     │   │
│  │  variants     │                                                │   │
│  └──────────────┘   ┌──────────────┐   ┌──────────────────────┐ │   │
│                      │ StallDetector│   │ Memory System         │ │   │
│  ┌──────────────┐   │ (60s scan)   │   │ ┌─────────────────┐  │ │   │
│  │ Persistence   │   └──────────────┘   │ │ MemoryWorker    │  │ │   │
│  │ Handle        │                      │ │ ├── SyncEngine  │  │ │   │
│  │ (mpsc→worker) │                      │ │ ├── FileWatcher │  │ │   │
│  └──────┬───────┘                      │ │ └── Search      │  │ │   │
│         │                               │ │    ├── FTS5     │  │ │   │
│         ▼                               │ │    ├── Vector   │  │ │   │
│  ┌──────────────┐                      │ │    └── Hybrid   │  │ │   │
│  │ Store (SQLite)│                      │ └─────────────────┘  │ │   │
│  │ rsi.db   │                      │ ┌─────────────────┐  │ │   │
│  │ 10 tables     │                      │ │ MemoryStore     │  │ │   │
│  │ 25 migrations │                      │ │ memory.sqlite   │  │ │   │
│  └──────────────┘                      │ └─────────────────┘  │ │   │
│                                         └──────────────────────┘ │   │
└──────────────────────────────────────────────────────────────────────┘
         │
         │ spawns CLI subprocesses
         ▼
┌──────────────────────────────────────────────────────────────────────┐
│                    Provider CLI Subprocesses                           │
│                                                                       │
│  claude -p <q> --output-format stream-json --verbose                 │
│  codex exec --json --sandbox workspace-write                         │
│  opencode run --format json                                          │
│  gemini -p <q> --output-format stream-json --yolo                    │
│  nullclaw agent -m <q>                                               │
│  (Local uses HTTP API: /v1/chat/completions)                         │
│                                                                       │
│  Each subprocess has MOTHERSHIP_SESSION_ID + MOTHERSHIP_SOCKET env vars  │
│  Can call back via rsi-signal binary                            │
└──────────────────────────────────────────────────────────────────────┘
```

## Crate Dependency Graph

```
rsi (TUI binary)
├── rsi-common (shared types + RPC)
├── ratatui + crossterm (terminal rendering)
├── modalkit (vim keybinding machine)
├── tui-textarea (text editing widget)
├── syntect (code highlighting in conversation)
├── tree-sitter-* (13 languages for file viewer)
├── fuzzy-matcher (skim algorithm for telescope/explorer)
├── reqwest (prompt processor HTTP client)
└── tokio (async runtime, current_thread)

rsid (daemon binary)
├── rsi-common (shared types + RPC)
├── tokio (async runtime, multi-thread)
├── rusqlite (SQLite with WAL)
├── tiktoken-rs (BPE tokenizer for token counting)
├── sha2 + hex (file hashing for memory index)
├── sqlite-vec (optional vector search extension)
├── reqwest (embedding provider HTTP client)
└── nix (SIGINT for process interruption)

rsi-signal (helper binary)
├── tokio (async runtime, current_thread)
└── serde_json
```

## Data Flow Diagrams

### Request-Response Polling

```
TUI                              Daemon
 │                                  │
 │──── ListSessions ───────────────→│
 │←─── Vec<Session> ───────────────│
 │                                  │
 │──── ListProjects ───────────────→│
 │←─── Vec<Project> ───────────────│
 │                                  │
 │──── ListGroups ─────────────────→│
 │←─── Vec<SessionGroup> ──────────│
 │                                  │
 │──── GetConversationsSince ──────→│  (batch of 4 sessions)
 │←─── ConversationBatchResponse ──│
 │                                  │
 │──── GetConversationsSince ──────→│  (next batch of 4)
 │←─── ConversationBatchResponse ──│
 │     ...                          │
```

### Push Notification Flow

```
TUI (socket 2)                   Daemon
 │                                  │
 │──── Subscribe ──────────────────→│
 │←─── Ack ────────────────────────│
 │                                  │
 │←─── BusEvent (session_status) ──│  ← SessionManager publishes
 │←─── BusEvent (conversation) ────│  ← monitor_session() publishes
 │←─── BusEvent (context_usage) ───│  ← token tracking publishes
 │←─── BusEvent (session_stalled) ─│  ← StallDetector publishes
 │     ... (continuous stream)      │
```

### Session Lifecycle Flow

```
User types prompt
    ↓
TUI: LaunchSession RPC → Daemon
    ↓
Daemon: launch_session()
    ├── Resolve project (ProjectIndex)
    ├── Assemble context (ContextPipeline)
    │   ├── Project files (500ms)
    │   ├── Memory search (2s)
    │   └── Git log (1s)
    ├── Spawn provider CLI subprocess
    ├── Return UUID immediately
    └── Background:
        ├── Persist to SQLite
        ├── monitor_session() loop
        │   ├── Parse StreamEvents
        │   ├── Build + persist ConversationEvents
        │   ├── Track tokens → publish ContextUsageUpdated
        │   ├── Check rotation threshold (65%)
        │   └── On completion → finalize_session()
        └── Generate title (Haiku)
    ↓
TUI polls/receives push events
    ├── Updates SessionState.events
    ├── Invalidates render cache
    └── Renders updated conversation
```

### Context Rotation Flow

```
Session running at 65% context fill
    ↓
RotationCoordinator: Idle → PendingInterrupt
    ↓
Send SIGINT to provider process
    ↓
Monitor loop breaks → finalize_session(Interrupted)
    ↓
Coordinator: SendCreateHandoff
    ↓
Resume session with "/create_handoff" query
    ↓
Coordinator: WritingHandoff (300s timeout)
    ↓
Detect handoff file via tool_use event
    ↓
Monitor completes → Coordinator: SpawnChild
    ↓
Archive parent session
    ↓
Launch child session (depth+1):
    query = "/resume_handoff <path>"
    continued_from = parent_id
    rotation_depth = parent_depth + 1
```

## Key Design Decisions

### Communication

- **Unix socket + JSON-RPC 2.0** for all TUI↔daemon communication
- **Two connections**: one for request/response, one for push streaming
- **Non-blocking phased polling**: one RPC per event loop iteration to never block rendering

### Rendering

- **120fps render loop** with dirty flag — only draws when `needs_redraw` is set
- **Generation-keyed render cache** — per-event lines cached by `(sequence, width, fold state)`
- **Virtual scrolling** with pre-computed offset arrays + binary search
- **Painter's algorithm** — overlays rendered last over everything

### State

- **Two-tier persistence**: PersistedState (cold start) + DevState (10s TTL hot-reload)
- **In-memory session maps** on both TUI and daemon — SQLite for durability, not queries
- **Async write workers** — all SQLite mutations go through mpsc channels

### Input

- **modalkit vim machine** produces typed `LcAction` values from key sequences
- **Priority chain dispatch**: overlay → file viewer → input bar → detail scroll → vim machine
- **Shared `InputSurface`** for vim-modal editing across input bar, prompts, and modals

### Memory

- **Hybrid search**: FTS5 (BM25) + vector similarity (sqlite-vec) with configurable weights
- **File watcher** with 1.5s debounce triggers incremental re-indexing
- **Temporal decay** and **MMR re-ranking** as optional pipeline stages

## Document Index

| #                                 | Document            | Covers                                                                     |
| --------------------------------- | ------------------- | -------------------------------------------------------------------------- |
| [01](01-shared-types-and-rpc.md)  | Shared Types & RPC  | rsi-common types, RPC methods, rsi-signal                        |
| [02](02-daemon-core.md)           | Daemon Core         | SessionManager, lifecycle, providers, EventBus, rotation, context pipeline |
| [03](03-tui-event-loop.md)        | TUI Event Loop      | main.rs, event loop, App struct, state persistence, DaemonClient           |
| [04](04-input-pipeline.md)        | Input Pipeline      | Keybindings, LcAction enum, dispatch chain, command mode, InputSurface     |
| [05](05-overlay-system.md)        | Overlay System      | All 26 overlays, stacking, focus models                                    |
| [06](06-ui-rendering.md)          | UI Rendering        | Render pipeline, virtual scrolling, content parsing, themes, highlighting  |
| [07](07-persistence-and-store.md) | Persistence & Store | SQLite schema, migrations, write workers, row mappers                      |
| [08](08-memory-system.md)         | Memory System       | Indexing, search pipeline (FTS5+vector+hybrid), temporal decay, MMR        |
| [09](09-vim-emulation.md)         | Vim Emulation       | Normal/visual/operator-pending modes, motions, text objects, dot-repeat    |
| [10](10-supporting-systems.md)    | Supporting Systems  | File viewer, git gutter, clipboard, notifications, settings                |

## Codebase Statistics

- **~45,000 lines** of Rust across 4 crates
- **4 binaries**: rsi (TUI), rsid (daemon), rsi-signal (helper), plus tests
- **7 AI providers** supported
- **26 overlay types**
- **83 action variants** (LcAction enum)
- **10 SQLite tables** with 25 schema migrations
- **13 tree-sitter grammars** for file viewer highlighting
- **3 color themes** with live-switching
