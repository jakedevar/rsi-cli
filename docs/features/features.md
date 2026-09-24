# Rsi Application Features List

This document catalogs and details every feature implemented within the `rsi` application, spanning the client (`rsi`), the daemon (`rsid`), the shared common library (`rsi-common`), and supporting tools.

---

## 1. Core Client-Daemon & IPC Infrastructure

### Dual-Socket JSON-RPC 2.0 Communication
- **Purpose**: Decouples terminal rendering (TUI) from long-running AI session processes. This ensures the UI remains fully responsive at 120fps even if AI processes are spawning, running, or stalled.
- **Details**: Connects over two independent Unix Domain Sockets:
  1. **Request-Response Connection**: Synchronous JSON-RPC 2.0 communication for commands, session lists, configuration edits, etc.
  2. **Push Stream Connection**: A subscription socket receiving continuous live event updates.
- **Code Locations**:
  - Client RPC handler: [client.rs](file:///home/jakedevar/rsi/crates/rsi/src/client.rs)
  - Client Notification loop: [notification_stream.rs](file:///home/jakedevar/rsi/crates/rsi/src/notification_stream.rs)
  - Daemon RPC server: [rpc.rs](file:///home/jakedevar/rsi/crates/rsid/src/rpc.rs)

### Central Event Bus Broadcasting
- **Purpose**: Streams real-time session progress, token counts, system notices, and completion statuses to all subscribed TUI clients.
- **Details**: Uses a `tokio::sync::broadcast` channel to fan out 11 distinct event variants (such as `SessionStatusChanged`, `ConversationEvent`, `ContextUsageUpdated`, and `SessionStalled`).
- **Code Location**: [bus.rs](file:///home/jakedevar/rsi/crates/rsid/src/bus.rs)

### Persistent SQLite State Store
- **Purpose**: Persists sessions, hierarchy, settings, and conversation logs across restarts.
- **Details**: Operates in Write-Ahead Logging (WAL) mode for low-latency concurrent writes. Implements 10 tables and 39 schema migrations. Mutations are offloaded from the main event loop to an asynchronous worker pool using channels.
- **Code Locations**:
  - SQLite manager: [store/mod.rs](file:///home/jakedevar/rsi/crates/rsid/src/store/mod.rs)
  - Async store worker: [store_worker.rs](file:///home/jakedevar/rsi/crates/rsid/src/store_worker.rs)

---

## 2. Terminal User Interface (TUI) & Layout System

### Binary Layout Tree Split Engine
- **Purpose**: Multi-pane layout engine allowing developers to split screens vertically or horizontally and organize tasks side-by-side.
- **Details**: Tracks pane splits in a binary tree (`SplitNode`) per tab. Supports splitting, closing, resizing, and neighbor focus wrapping (`Ctrl+W h/j/k/l`).
- **Code Location**: [layout.rs](file:///home/jakedevar/rsi/crates/rsi/src/app/layout.rs)

### Multi-Tab Workspaces
- **Purpose**: Allows isolating tasks by tab. Each tab acts as a separate viewport owning its own split tree.
- **Details**: Tabs can inherit specific project settings, allowing multi-tasking across different repositories/folders. Navigated using `gt` and `gT` keys.
- **Code Locations**:
  - Tab state management: [app/mod.rs](file:///home/jakedevar/rsi/crates/rsi/src/app/mod.rs)
  - Tab layout: [layout.rs](file:///home/jakedevar/rsi/crates/rsi/src/app/layout.rs)

### Virtual Scrolling & Render Caching
- **Purpose**: Handles rendering thousands of lines of output at 120fps.
- **Details**:
  - **Virtual Scrolling**: Only renders visible lines by pre-computing height offsets and performing a binary search to find visible entries.
  - **Generation-Keyed Render Cache**: Caches formatted text blocks by event sequence and screen width to prevent costly re-wrapping.
- **Code Locations**:
  - Virtual scroll rendering: [ui/session.rs](file:///home/jakedevar/rsi/crates/rsi/src/ui/session.rs)
  - Cache implementation: [app/cache.rs](file:///home/jakedevar/rsi/crates/rsi/src/app/cache.rs)

---

## 3. Session Lifecycle & Process Orchestration

### AI Provider Subprocess Orchestrator
- **Purpose**: Launches and manages external AI command-line processes asynchronously.
- **Details**: Spawns processes in the background, attaches to their standard output streams, and monitors their output. Emits structured stream events to the TUI.
- **Code Locations**:
  - Process Manager: [session/mod.rs](file:///home/jakedevar/rsi/crates/rsid/src/session/mod.rs)
  - Session Launching: [session/launch.rs](file:///home/jakedevar/rsi/crates/rsid/src/session/launch.rs)
  - Session Monitoring: [session/monitor.rs](file:///home/jakedevar/rsi/crates/rsid/src/session/monitor.rs)

### Support for 7 AI Provider Engines
- **Purpose**: Permits developers to select different AI agents depending on cost, quality, or local usage constraints.
- **Details**:
  1. **Claude CLI**: Integrated Claude Code binary with stream parsing.
  2. **Gemini CLI**: Stream-json wrapper supporting Gemini models.
  3. **Codex CLI**: Autonomous agent engine.
  4. **OpenCode**: Code generation model CLI.
  5. **Local**: Direct HTTP connection to OpenAI-compatible endpoints (Ollama/LM Studio).
  6. **Nullclaw**: Plain-text terminal agent harness.
  7. **Mercury**: Specialized low-latency agent engine.
- **Code Locations**:
  - Interface definition: [provider.rs](file:///home/jakedevar/rsi/crates/rsid/src/provider.rs)
  - Claude adapter: [claude.rs](file:///home/jakedevar/rsi/crates/rsid/src/claude.rs)
  - Gemini adapter: [gemini.rs](file:///home/jakedevar/rsi/crates/rsid/src/gemini.rs)
  - Codex adapter: [codex.rs](file:///home/jakedevar/rsi/crates/rsid/src/codex.rs)
  - OpenAI/Local adapter: [openai.rs](file:///home/jakedevar/rsi/crates/rsid/src/openai.rs)
  - Nullclaw adapter: [crates/rsid/src/session/mod.rs](file:///home/jakedevar/rsi/crates/rsid/src/session/mod.rs)

### Inactivity Stall Detector & Nudger
- **Purpose**: Ensures spawned AI processes do not hang indefinitely.
- **Details**: A background thread scans active sessions every 60 seconds. If a session is in the `Running` state for more than 30 minutes, or `WaitingApproval` for more than 60 minutes, it generates a `SessionStalled` alert.
- **Code Location**: [stall_detector.rs](file:///home/jakedevar/rsi/crates/rsid/src/stall_detector.rs)

### Exponential Backoff Retry Handler
- **Purpose**: Automatically recovers sessions that failed due to temporary network glitches or API rate limits.
- **Details**: Detects `Failed` state sessions, calculates backoff delay, and automatically schedules a re-run using an async retry loop.
- **Code Location**: [session/lifecycle.rs](file:///home/jakedevar/rsi/crates/rsid/src/session/lifecycle.rs)

---

## 4. Vim Emulation & Modal Editor

### Modal Keyboard Handling (`modalkit`)
- **Purpose**: Brings complete modal keyboard navigation (Normal, Insert, Visual, Operator-Pending, Command) to AI sessions.
- **Details**: Interfaces the `modalkit` Vim state machine directly with crossterm input events. Translates keystrokes into typed actions (`LcAction`) mapped across app viewports.
- **Code Locations**:
  - Vim Machine binding: [keybindings.rs](file:///home/jakedevar/rsi/crates/rsi/src/keybindings.rs)
  - Actions definitions: [modalkit_types.rs](file:///home/jakedevar/rsi/crates/rsi/src/modalkit_types.rs)
  - Ex-commands parser: [commands.rs](file:///home/jakedevar/rsi/crates/rsi/src/commands.rs)

### Unified Vim Input Surface
- **Purpose**: Shared input component used in prompt dialogs, inline modals, and settings inputs.
- **Details**: Emulates text motions (`w`, `b`, `e`), visual selection, line deletions, dot-repeat, and yank/paste.
- **Code Location**: [input_surface.rs](file:///home/jakedevar/rsi/crates/rsi/src/input_surface.rs)

### Jumplist Navigation
- **Purpose**: Tracks navigation history to jump between sessions.
- **Details**: Keeps an in-memory stack of recently viewed sessions. Pressing `Ctrl+O` jumps back in time, and `Ctrl+I` jumps forward.
- **Code Location**: [app/jumplist.rs](file:///home/jakedevar/rsi/crates/rsi/src/app/jumplist.rs)

---

## 5. Code Editing & File Viewer Subsystems

### Multilingual File Viewer with Tree-sitter
- **Purpose**: View code files within the TUI workspace with native syntax highlighting.
- **Details**: Links 13 tree-sitter grammars (Rust, Python, JS, TS, Bash, JSON, TOML, YAML, Go, C/C++, Markdown) to perform high-fidelity color tokenization.
- **Code Locations**:
  - Highlights calculation: [ui/treesitter.rs](file:///home/jakedevar/rsi/crates/rsi/src/ui/treesitter.rs)
  - File editor UI: [file_viewer.rs](file:///home/jakedevar/rsi/crates/rsi/src/file_viewer.rs)

### Code Folding
- **Purpose**: Allows collapsing code blocks to inspect large source files within the TUI.
- **Details**: Uses the Tree-sitter AST to identify foldable scopes (functions, arrays, structs, blocks). Implements folding controls (`za` toggle, `zo` open, `zc` close, `zM` close all, `zR` open all).
- **Code Location**: [file_viewer.rs](file:///home/jakedevar/rsi/crates/rsi/src/file_viewer.rs)

### Ex-commands and Search inside Files
- **Purpose**: Modifies local files without exiting the TUI.
- **Details**: Supports Ex-commands (like `:w`, `:q`, `:wq`, `:e!`) and forward/backward searching (`/` and `?`) with cursor jumping.
- **Code Locations**:
  - File command router: [file_viewer_commands.rs](file:///home/jakedevar/rsi/crates/rsi/src/file_viewer_commands.rs)
  - Search logic: [file_viewer.rs](file:///home/jakedevar/rsi/crates/rsi/src/file_viewer.rs)

### Auto-Bracket Pairing (Auto-pair)
- **Purpose**: Automatically inserts closing delimiters during insert mode.
- **Details**: Automatically matches `(`, `{`, `[`, `"`, and `'`. Deleting an opening bracket also deletes the adjacent matched closing bracket automatically.
- **Code Location**: [file_viewer.rs](file:///home/jakedevar/rsi/crates/rsi/src/file_viewer.rs)

---

## 6. Git Gutter & Workspace Integration

### Live Git Diff Gutter
- **Purpose**: Displays file changes inside the viewer in real-time.
- **Details**: Executes `git diff --unified=0 HEAD` asynchronously on save or revert, parses @@ hunk headers, and displays line-by-line gutter indicators (`+` for added, `~` for modified, `-` for deleted).
- **Code Location**: [git_gutter.rs](file:///home/jakedevar/rsi/crates/rsi/src/git_gutter.rs)

### OSC 52 Remote Clipboard Integration
- **Purpose**: Supports yanking text from remote terminal sessions over SSH to the local client's clipboard.
- **Details**: Encodes text in Base64 and writes the ANSI escape sequence `\x1b]52;c;{base64}\x07` directly to standard output.
- **Code Location**: [clipboard.rs](file:///home/jakedevar/rsi/crates/rsi/src/clipboard.rs)

### Multi-Backend Clipboard Reading (Image & Text Pasting)
- **Purpose**: Permits pasting text or images directly into the chat interface.
- **Details**: Queries the system clipboard using native APIs (`arboard` / `xclip`). If an image is detected on the clipboard, it automatically saves a PNG file to a local project directory and inserts the file path.
- **Code Location**: [clipboard.rs](file:///home/jakedevar/rsi/crates/rsi/src/clipboard.rs)

---

## 7. Sandbox Isolation System (Git Worktree)

### Opt-in Filesystem Sandboxing
- **Purpose**: Protects the working directory by isolating shell executions and write tools.
- **Details**: Spawns sandboxed sessions inside dedicated `git worktree` clones. Subprocess actions occur within the temporary worktree and do not touch the main code repository until committed.
- **Code Locations**:
  - Sandbox Allocator: [sandbox/mod.rs](file:///home/jakedevar/rsi/crates/rsid/src/sandbox/mod.rs)
  - Git worktree implementation: [sandbox/git_worktree.rs](file:///home/jakedevar/rsi/crates/rsid/src/sandbox/git_worktree.rs)

### Orphan Sweep & Cleanup Engine
- **Purpose**: Prevents hard drives from filling up with old sandbox checkouts.
- **Details**: A cleanup routine runs during daemon startup to scan for orphaned worktree directories, removes files, and deletes temporary git branches.
- **Code Location**: [session/lifecycle.rs](file:///home/jakedevar/rsi/crates/rsid/src/session/lifecycle.rs)

---

## 8. Context Pipelines & Compaction (Rotation)

### Budgeted Context Pipeline
- **Purpose**: Assembles a system prompt by combining project files, git history, and memory segments without exceeding the LLM's context window.
- **Details**: Queries resources in parallel with a 3-second timeout. Sorts and includes blocks in priority order (project files > memory > git logs) until the context budget is reached.
- **Code Location**: [session/context_pipeline.rs](file:///home/jakedevar/rsi/crates/rsid/src/session/context_pipeline.rs)

### Automated Context Rotation
- **Purpose**: Prevents long-running sessions from exceeding the AI's maximum context window and dropping context.
- **Details**: Monitors token usage. When the context window reaches 65% capacity, it interrupts the provider process, runs `/create_handoff` to summarize findings, archives the parent session, and launches a child session that continues from the handoff file.
- **Code Locations**:
  - Rotation Controller: [session/rotation.rs](file:///home/jakedevar/rsi/crates/rsid/src/session/rotation.rs)
  - Coordinator State Machine: [session/rotation_coordinator.rs](file:///home/jakedevar/rsi/crates/rsid/src/session/rotation_coordinator.rs)

---

## 9. Semantic & BM25 Hybrid Memory System

### Hybrid Indexing and Search
- **Purpose**: Performs natural-language memory retrieval by combining text matching (BM25) and semantic vector search.
- **Details**:
  - **Keyword Match**: SQLite FTS5 extension.
  - **Semantic Match**: Generates embeddings (using local Ollama or OpenAI endpoints) and searches them via `sqlite-vec`.
  - **Hybrid Scoring**: Melds the text and vector ranks using a weighted formula (default: 70% vector, 30% text).
- **Code Locations**:
  - Search Pipeline: [memory/search/mod.rs](file:///home/jakedevar/rsi/crates/rsid/src/memory/search/mod.rs)
  - Memory Manager: [memory/manager.rs](file:///home/jakedevar/rsi/crates/rsid/src/memory/manager.rs)

### Incremental File Watcher Reindexing
- **Purpose**: Keeps the search index up to date when local files change.
- **Details**: Watches the project's memory directory (`/memory/` and `MEMORY.md`). Edits trigger a 1.5-second debounced re-index that calculates SHA-256 hashes to process edits.
- **Code Location**: [memory/watcher.rs](file:///home/jakedevar/rsi/crates/rsid/src/memory/watcher.rs)

### MMR Re-ranking & Temporal Decay
- **Purpose**: Promotes search result diversity and prioritizes recent files.
- **Details**:
  - **Maximal Marginal Relevance (MMR)**: Penilizes chunks that are highly similar to already-selected results.
  - **Temporal Decay**: Exponentially decays scores of older files based on a configurable half-life (default: 30 days).
- **Code Locations**:
  - MMR Diversity: [memory/math.rs](file:///home/jakedevar/rsi/crates/rsid/src/memory/math.rs)
  - Decay calculation: [memory/search/mod.rs](file:///home/jakedevar/rsi/crates/rsid/src/memory/search/mod.rs)

### Pre-Compaction Memory Flush
- **Purpose**: Prompt the active model to save long-term learnings before context rotation occurs.
- **Details**: Injects a system message instructing the model to write summaries to `memory/*.md` before the context window is compacted.
- **Code Location**: [memory/flush.rs](file:///home/jakedevar/rsi/crates/rsid/src/memory/flush.rs)

---

## 10. Agentic Q&A Dialectic Engine (`:ask`)

### Dialog-Based Dialectic Query Overlay
- **Purpose**: Ask natural language questions about the codebase, session history, and memory documents using an agentic Q&A overlay.
- **Details**: Activates an agent loop that runs local LLMs (like Qwen3) with tool capabilities. The model can invoke tools to inspect the workspace before providing an answer.
- **Code Locations**:
  - Dialectic Client UI: [overlay/mod.rs](file:///home/jakedevar/rsi/crates/rsi/src/overlay/mod.rs)
  - Dialectic Core Engine: [dialectic/mod.rs](file:///home/jakedevar/rsi/crates/rsid/src/dialectic/mod.rs)

### Dialectic Agent Tools
- **Purpose**: Allows the dialectic model to search the workspace and read session histories.
- **Details**: Implements the following tools for the agent:
  - `search_memory`: Semantic + keyword search.
  - `list_sessions`: Retrieves session lists.
  - `get_session_detail`: Gets session metadata.
  - `get_conversation_excerpt`: Extracts chat logs.
  - `get_project_info`: Reads project settings.
- **Code Location**: [dialectic/tools.rs](file:///home/jakedevar/rsi/crates/rsid/src/dialectic/tools.rs)

---

## 11. Recursive Directed Acyclic Graph (DAG) Task Runner (`:dag`)

### Multi-Task DAG Scheduler
- **Purpose**: Coordinates multi-stage agent pipelines by structuring dependencies as directed acyclic graphs.
- **Details**: Manages graphs, tasks, dependencies, validation runs, and outputs. Supports both a simulated (`fake`) scheduler and a `live_session` scheduler.
- **Code Locations**:
  - Core Task Runner: [session/graph_runner.rs](file:///home/jakedevar/rsi/crates/rsid/src/session/graph_runner.rs)
  - DAG client UI (`:dag`): [ui/vitals.rs](file:///home/jakedevar/rsi/crates/rsi/src/ui/vitals.rs)
  - Common Schema: [recursive_dag.rs](file:///home/jakedevar/rsi/crates/rsi-common/src/recursive_dag.rs)

### Graph Visualization & Interactive Operator Surface
- **Purpose**: Visualize and debug task DAGs within the TUI client.
- **Details**: Displays execution progress, logs, and artifacts in a multi-column view. Bridges recursive graphs into visual flow diagrams using a Sugiyama rendering canvas.
- **Code Locations**:
  - TUI Rendering: [ui/mini_dag.rs](file:///home/jakedevar/rsi/crates/rsi/src/ui/mini_dag.rs)
  - Visual Graph Bridge: [session/topology_bridge.rs](file:///home/jakedevar/rsi/crates/rsid/src/session/topology_bridge.rs)

---

## 12. Entity Cards Database (`:card`)

### Project and User Fact Cards
- **Purpose**: Persist facts about projects or user preferences (e.g. style guides, rules, credentials) and insert them into system prompts.
- **Details**: Supports editing project cards (`:card`) and user preference cards (`:card user`). Facts can be appended via CLI (`:card add "fact text"`).
- **Code Locations**:
  - Client interface: [commands.rs](file:///home/jakedevar/rsi/crates/rsi/src/commands.rs)
  - Daemon card manager: [session/cards.rs](file:///home/jakedevar/rsi/crates/rsid/src/session/cards.rs)

---

## 13. Subprocess Message Bridges (iMessage & Signal)

### iMessage Daemon Bridge (`flywheel-imessage`)
- **Purpose**: Interact with the `rsi` daemon remotely via Apple iMessage.
- **Details**: Running on macOS, it polls the local `chat.db` database for new messages. If the message passes access checks, it invokes the daemon's JSON-RPC launcher and responds using AppleScript.
- **Code Locations**:
  - Daemon loop: [main.rs](file:///home/jakedevar/rsi/crates/flywheel-imessage/src/main.rs)
  - Chat.db reader: [chatdb.rs](file:///home/jakedevar/rsi/crates/flywheel-imessage/src/chatdb.rs)
  - AppleScript sender: [applescript.rs](file:///home/jakedevar/rsi/crates/flywheel-imessage/src/applescript.rs)

### Signal Messenger Bridge (`flywheel-signal`)
- **Purpose**: Control AI coding sessions remotely via Signal.
- **Details**: Integrates with a local `signal-cli` client, parses incoming commands, and replies with session outputs.
- **Code Locations**:
  - Daemon loop: [main.rs](file:///home/jakedevar/rsi/crates/flywheel-signal/src/main.rs)
  - Signal CLI router: [signal_cli.rs](file:///home/jakedevar/rsi/crates/flywheel-signal/src/signal_cli.rs)

---

## 14. Settings, Theming, and UI Preferences

### Visual Settings Editor (`:settings`)
- **Purpose**: Configure keybindings, display modes, and provider credentials within the TUI.
- **Details**: Two-column settings interface (Categories on the left, Items on the right) supporting text updates and toggling options.
- **Code Locations**:
  - Settings state: [settings.rs](file:///home/jakedevar/rsi/crates/rsi/src/settings.rs)
  - UI controller: [settings_keys.rs](file:///home/jakedevar/rsi/crates/rsi/src/settings_keys.rs)
  - UI view: [ui/settings.rs](file:///home/jakedevar/rsi/crates/rsi/src/ui/settings.rs)

### Live Theme Swapper (`:theme`)
- **Purpose**: Changes the TUI color theme on the fly.
- **Details**: Selection menu featuring built-in Catppuccin flavors (Latte, Frappe, Macchiato, Mocha). Navigating the menu previews the theme in real-time, and pressing Escape reverts the change.
- **Code Locations**:
  - Themes definition: [ui/theme.rs](file:///home/jakedevar/rsi/crates/rsi/src/ui/theme.rs)
  - Theme picker: [overlay/mod.rs](file:///home/jakedevar/rsi/crates/rsi/src/overlay/mod.rs)

---

## 15. Harness Telemetry, Outcome Proxies & Evals (`rsi-eval`)

### Outcome Telemetry Tracking
- **Purpose**: Collects telemetry to evaluate AI agent efficiency and performance.
- **Details**: Measures session runtimes, token consumption, approval request counts, and execution exit codes. Permits rating sessions from 1 to 10 (`:rate`).
- **Code Location**: [session/outcome.rs](file:///home/jakedevar/rsi/crates/rsid/src/session/outcome.rs)

### Telemetry Replay & Regression Gate (`rsi-eval`)
- **Purpose**: Tests agent changes against a test suite to detect regressions.
- **Details**: Loads a frozen database corpus of coding tickets, runs them through the daemon serial execution runner, gathers outcome metrics, and compares performance against a target baseline.
- **Code Locations**:
  - Driver orchestrator: [driver.rs](file:///home/jakedevar/rsi/crates/rsi-eval/src/driver.rs)
  - Regression Gate: [gate.rs](file:///home/jakedevar/rsi/crates/rsi-eval/src/gate.rs)

---

## 16. Diagnostic, Profiling, and Builtin Games

### Live Diagnostics overlay (`:diag`)
- **Purpose**: Inspect cache hit-rates, memory usage, and operation durations.
- **Details**: Displays profiling counters when the `MOTHERSHIP_PROFILE` environment variable is active.
- **Code Locations**:
  - Profiling counters: [profiling.rs](file:///home/jakedevar/rsi/crates/rsi/src/profiling.rs)
  - View layout: [commands.rs](file:///home/jakedevar/rsi/crates/rsi/src/commands.rs)

### EspSquare Game (`gc` or `gq` from browser)
- **Purpose**: A builtin game designed for entertainment or testing.
- **Details**: Renders a 3x3 interactive card grid where players make selections over 12 rounds. Saves p-value scores to a local file.
- **Code Locations**:
  - UI controller: [overlay/mod.rs](file:///home/jakedevar/rsi/crates/rsi/src/overlay/mod.rs)
  - Game Engine: [session/esp_games.rs](file:///home/jakedevar/rsi/crates/rsid/src/session/esp_games.rs)
