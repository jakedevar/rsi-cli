# Flywheel Features

## Session Lifecycle Management
**Category:** Session Management
**Description:** Flywheel provides comprehensive session lifecycle management covering the full state machine from creation through termination. Sessions progress through statuses: Starting → Running → WaitingApproval → Completed/Failed/Interrupted/Archived. The TUI exposes every transition via keybindings: `x` sends SIGINT to interrupt, `DD` performs a hard delete, `Space+a` archives (soft delete), `U` unarchives from the archive browser, `R` triggers context rotation, and `X`/`Space+c` quick-continues an idle session. The daemon enforces state machine invariants and stores sessions in SQLite with nanosecond-precision RFC 3339 timestamps and UUID identifiers.

## Provider Integration — Claude
**Category:** Provider Integration
**Description:** Flywheel integrates with the official Claude CLI (`claude` binary) through a subprocess-based architecture in `crates/flywheeld/src/claude.rs`. The daemon discovers the claude binary via `which::which("claude")`, spawns it with `--output-format stream-json`, and reads NDJSON lines from stdout. The `ClaudeClient` supports model discovery by querying aliases (opus, sonnet, haiku) to resolve actual model IDs with version numbers. Session resume is supported via `resume_session_id`. The parser handles all Claude stream event types: `assistant`, `user`, `result`, and extracts tool use, tool results, thinking blocks, and usage metadata. SIGINT is sent for graceful interruption.

## Provider Integration — Codex
**Category:** Provider Integration
**Description:** Flywheel integrates with OpenAI's Codex CLI through `crates/flywheeld/src/codex.rs`, following the same subprocess-based pattern as the Claude integration. The `CodexClient` discovers the codex binary in PATH, spawns it with configured model and working directory parameters, and streams JSON output from stdout. Supported models include `gpt-5-codex` and `codex-mini-latest`. Sessions can be interrupted via SIGINT and force-killed. The `CodexProcess` struct wraps the `tokio::process::Child` and provides the same interrupt/kill/wait interface as other providers.

## Provider Integration — Local (Ollama)
**Category:** Provider Integration
**Description:** Flywheel supports local models via any OpenAI-compatible server (primarily Ollama) through `OpenAiClient`. The Local provider connects to a local HTTP endpoint (typically `http://localhost:11434/v1`), discovers available models via `GET /v1/models`, and streams completions using the OpenAI chat completions API format. Default models include `qwen3:14b`. This enables fully offline AI sessions without sending data to external APIs. The daemon performs model discovery at startup to populate the available local model list.

## Provider Integration — Gemini
**Category:** Provider Integration
**Description:** Flywheel integrates with Google's Gemini CLI through `crates/flywheeld/src/gemini.rs`, following the same subprocess pattern as Claude. The `GeminiClient` discovers the `gemini` binary in PATH and spawns it with stream-json output format. Supported models include `gemini-3.1-pro-preview` and `gemini-3-flash-preview`. The `GeminiProcess` provides the same interrupt/kill/wait interface as other CLI-based providers. Stream output is parsed using the same `StreamEvent` type as the Claude integration for format compatibility.

## Custom OpenAI-Compatible Provider Support
**Category:** Provider Integration
**Description:** Flywheel supports user-defined OpenAI-compatible providers through the `ProviderForm` overlay, accessible from the Settings pane under the "API Models" category. Users can add, edit, and delete custom providers by specifying a display name, base URL, API key (masked in display), and default model ID. These custom providers appear in the model selector alongside built-in providers. The provider configuration is stored in `UserSettings` and passed to session launch via `openai_base_url` and `openai_api_key` fields in `LaunchSessionParams`.

## TUI Layout System — Binary Split Trees
**Category:** Layout
**Description:** Flywheel implements a flexible pane layout using a binary tree of `SplitNode` values stored in `types.rs`. Each leaf node contains either a `SessionList`, `SessionDetail`, or `Settings` pane with its own `PaneId`. Internal split nodes carry a `SplitDirection` (Horizontal or Vertical) and two child subtrees. The tree supports finding, mutating, and removing panes by ID. Ratatui renders the tree recursively using `Constraint`s computed from the split direction. Vim split commands (`:split`, `:vsplit`) add new leaves; `:close` removes a leaf and promotes its sibling. Focus moves between panes with `Ctrl+{h,j,k,l}`.

## TUI Layout System — Tabs (Workspaces)
**Category:** Layout
**Description:** Flywheel organizes multiple layout trees into named tabs, each bound to a project. The `Tab` struct carries a name, a `SplitNode` layout tree, the focused `PaneId`, and an optional `project_id`. Navigation uses `gt`/`gT`, `L`/`H`, or `{count}gt` for direct tab access. The `:tabnew` command creates a new tab, `:tabclose` removes one. The `OpenSessionInNewTab` action opens any session in a fresh tab. Active tab index and the full tab list persist in `PersistedState` and survive process restarts. The status bar shows a `tab N/M` indicator when more than one tab is open.

## Overlay System — Prompt Overlay
**Category:** Overlay
**Description:** The Prompt overlay is the primary interface for creating and continuing sessions. It opens as a full-screen modal with a tui-textarea that supports full vim normal mode editing via `vim_textarea.rs` (motions, operators, text objects, visual mode, dot-repeat, f/t character search). `Ctrl+Enter` submits the current text, `Ctrl+T` submits and opens in a new tab, `Ctrl+S` submits and opens in a vertical split. The overlay supports three purposes: new session, continue existing session (pre-fills with session context), and TaskRabbit one-shot task. Slash-command autocompletion (suggestions) is available for custom commands.

## Overlay System — Model Selector
**Category:** Overlay
**Description:** The Model Selector overlay (`M` key) allows switching between AI providers and models. Providers are cycled with `Tab`/`Shift+Tab` (Claude → Codex → Local → Gemini → custom providers). Models within a provider are navigated with `j`/`k` and selected with `Enter` or digit keys `1`-`9`. When opened from the session list, it sets the global default model for new sessions. When opened from a session detail view, it sends a `SwitchSessionModel` RPC to change the running session's model without restarting it, creating a model segment boundary in the conversation history.

## Overlay System — Theme Picker
**Category:** Overlay
**Description:** The Theme Picker overlay (`T` key) provides live theme preview while navigating. It supports six built-in Catppuccin variants (Latte, Frappé, Macchiato, Mocha) and custom palettes (Goth, Junk Yard) accessible via `j`/`k` navigation or digit keys `1`-`6`. Themes are applied immediately on navigation for live preview. Pressing `Esc` reverts to the original theme. The selected theme key is persisted in `PersistedState.theme_flavor` (e.g., "latte", "goth") and restored on restart. The `:theme [name]` command allows direct theme selection by key from command mode.

## Overlay System — Project Picker
**Category:** Overlay
**Description:** The Project Picker overlay (`Space+p`) provides telescope-style project navigation with real-time fuzzy filtering by name. The picker lists all projects preceded by an "All projects" option and followed by "(unassigned)". Selecting a project opens its dedicated workspace tab (or focuses it if already open). From the picker, `Ctrl+n` opens the create project form, `Ctrl+e` edits the highlighted project, and `Ctrl+d` deletes it. When opened via `Space+C` from a session detail, it operates in `SessionReassign` context, updating the session's project association instead of switching workspace tabs.

## Overlay System — Session Picker (MRU Switcher)
**Category:** Overlay
**Description:** The Session Picker is triggered by holding the semicolon key (`;`) and provides an MRU (most-recently-used) ordered session switcher. The list is snapshotted from the current filtered session order when the overlay opens, ensuring stable navigation. Releasing `;` or pressing `Enter` selects the highlighted session and navigates to it. `g`/`G` jump to the first/last session. This enables rapid session switching without losing context, similar to vim's buffer switcher but semantically ordered by recency.

## Overlay System — Sort Picker
**Category:** Overlay
**Description:** The Sort Picker overlay (`Space+s`) lets users choose how sessions are ordered in the list. Four sort orders are available: StalestFirst (longest-neglected sessions at top, the default), NewestFirst (most recently active at top), OldestCreated (earliest created at top), and NewestCreated (most recently created at top). The selected sort order persists in `PersistedState.sort_order` across restarts. The sort picker uses the standard list navigation pattern with `j`/`k` and `Enter` to apply.

## Overlay System — Archive Browser
**Category:** Overlay
**Description:** The Archive Browser overlay (`ga`) provides a chronologically organized view of archived sessions grouped by date headers. Sessions can be browsed with `j`/`k` (which skip section headers), opened in detail view with `Enter`, or unarchived back to the active list with `U`. The overlay loads archived sessions via the `ListArchivedSessions` RPC and builds a flat list of `ArchiveListItem::Header` and `ArchiveListItem::Session` entries. Scroll offset is maintained for large archives. `gg`/`G` jump to the first/last archived session.

## Overlay System — Notification Browser
**Category:** Overlay
**Description:** The Notification Browser overlay (`Space+n`) displays the full notification history with timestamp, kind, and message for each entry. Notifications are typed by kind (TaskRabbitComplete/Failed, BugComplete/Failed, SessionLaunching/Resuming, Connected/ConnectionFailed/ConnectionLost, OperationSuccess/Failed, Info) and prioritized (Low/Medium/High) with TTLs of 5 or 10 seconds. The browser allows navigation with `j`/`k` and shows notifications that have already expired from the status bar. Sessions linked to a notification can be opened via `Enter` for direct navigation.

## Overlay System — Merge Queue
**Category:** Overlay
**Description:** The Merge Queue overlay (`gm`) shows sessions the user has manually marked as ready to land (merge to main). Sessions in the queue are displayed in FIFO order. `Enter` pops the top session from the queue and opens it in detail view, while `d` removes the highlighted entry without focusing it. The merge queue is persisted in `PersistedState.merge_queue` (a `Vec<Uuid>`) across restarts. The status bar shows a `MQ: N` segment when the queue is non-empty. This provides a lightweight personal workflow tool for tracking work ready to commit.

## Overlay System — Project Form
**Category:** Overlay
**Description:** The Project Form overlay handles creation and editing of projects with three fields: Name, Path (optional filesystem path for automatic session-to-project assignment), and Color (selected from a Catppuccin palette via `Left`/`Right` cycling). Field navigation uses `Tab`/`Shift+Tab`. `Enter` saves and returns to the project picker, `Esc` cancels. For new projects, a `CreateProject` RPC is sent; for edits, an `UpdateProject` RPC. The path field enables the daemon's `ProjectIndex` longest-prefix-match system to automatically assign new sessions to projects based on working directory.

## Overlay System — Session Drawer
**Category:** Overlay
**Description:** The Session Drawer overlay (`Space+e`) is a left-anchored sidebar showing the full session list. Unlike the main pane, it overlays the existing content without replacing it, providing at-a-glance session visibility while in a detail view. It supports standard `j`/`k` navigation and session selection with `Enter`. The drawer is useful when a session detail view is open and the user wants to quickly reference other sessions without navigating away. It follows the same filtering (project filter, search) as the main session list.

## Overlay System — Provider Form
**Category:** Overlay
**Description:** The Provider Form overlay manages creation and editing of custom OpenAI-compatible provider configurations. Four fields are provided: Name, Base URL, API Key (displayed masked), and Default Model. Tab/Shift+Tab navigates between fields, Enter saves from any field, Esc cancels. On save, the provider is added to `UserSettings.custom_providers` and immediately available in the model selector. Editing an existing provider is triggered from the Settings pane via the `d` key on the selected provider entry. UUID identifiers distinguish multiple custom providers.

## Overlay System — Prompt Preview
**Category:** Overlay
**Description:** The Prompt Preview overlay (`p` key) shows the full initial query/prompt of the currently selected session in a scrollable popup without navigating to the session detail. As the user navigates the session list with `j`/`k`, the popup updates in real-time to show the selected session's prompt. This enables rapid preview of session context for triage and selection. `Ctrl+d`/`Ctrl+u` scroll long prompts. `Enter` opens the session detail, while `p`/`Esc`/`q` close the overlay.

## Overlay System — Keybindings Help
**Category:** Overlay
**Description:** The Keybindings Help overlay (`?`) renders the full `docs/keybindings.md` reference in a scrollable popup. A fuzzy search mode activated by `/` filters keybindings by key sequence or description text as the user types. Navigation uses `j`/`k` for single-line scrolling, `Ctrl+d/u` for half-page, `Ctrl+f/b` for full-page, and `g`/`G` to jump to top/bottom. `Esc` clears the active filter (or closes if no filter); `q`/`?` close the overlay. This provides in-app documentation without requiring an external browser or terminal.

## Overlay System — Memory Search
**Category:** Overlay
**Description:** The Memory Search overlay (`Space+M`) provides live full-text and semantic search over indexed memory files. As the user types a query, a `MemorySearch` RPC is sent to the daemon, and results are displayed showing file path, line range, relevance score, and a text snippet. Results are sorted by hybrid score (0.7 × vector similarity + 0.3 × BM25 text score). Navigation with `j`/`k` highlights results; `Enter` would open the referenced file. The overlay shows a loading indicator while the search RPC is in-flight and supports the daemon's FTS-only fallback when no embedding provider is configured.

## Overlay System — Rename Session
**Category:** Overlay
**Description:** The Rename Session overlay (`F2` key) provides an inline text field for editing a session's display title. The current title (or query if no title is set) is pre-filled. Pressing `Enter` sends an `UpdateSessionTitle` RPC to the daemon and closes the overlay; `Esc` cancels without modification. Session titles are displayed instead of the raw query in the session list and detail header. The title is stored in `sessions.title` in SQLite and returned with the session in `ListSessions` responses. Haiku-generated titles (from Claude) can also populate this field.

## Overlay System — Diagnostics
**Category:** Overlay
**Description:** The Diagnostics overlay (`:diagnostics` or `:diag` command, also shown on `AppMetrics` access) displays runtime performance metrics collected by the TUI's profiling system when `FLYWHL_PROFILE=1` is set. Metrics include last render time in milliseconds, last poll cycle time in milliseconds, and render cache hit rate (percentage of events rendered from cache vs. recomputed). The overlay provides operational visibility into TUI performance without external tooling. When profiling is disabled, the overlay shows zeroes for all metrics.

## Vim Keybinding System (modalkit)
**Category:** Input
**Description:** Flywheel's navigation layer uses the `modalkit` library (a Rust vim keybinding engine) for all non-text-editing input. The `LcAction` enum in `modalkit_types.rs` defines 69 application-specific actions mapped through `build_vim_machine()` in `keybindings.rs`. The system provides Normal, Insert, Command, and Search modes. Multi-key sequences (e.g., `ZZ`, `]a`, `gs`) are handled with timing. Count modifiers work for navigation and scroll commands. Standard vim motions (`gg`, `G`, `Ctrl+d/u/f/b`) are inherited from modalkit's default bindings. All keybindings are documented in `docs/keybindings.md` which is also served by the in-app help overlay.

## Vim Text Editing (vim_textarea)
**Category:** Input
**Description:** Flywheel implements a full Tier 2 vim text editor in `vim_textarea.rs` for the input bar and prompt overlay. This is independent of modalkit and provides: count-prefixed motions (`3w`, `5j`), operators with text objects (`ciw`, `da"`, `yi(`), visual mode (character and line), dot-repeat recording and playback, character search (`f`/`t`/`F`/`T`) with `;`/`,` repeat, all insert-mode entry variants (`i`, `a`, `A`, `I`, `o`, `O`), and undo/redo. The `VimState` struct tracks pending operator, visual range, count accumulator, char-search state, and dot-repeat buffer. Both the session input bar and the prompt overlay share this same handler.

## Slash Command Suggestions
**Category:** Input
**Description:** Both the input bar and the prompt overlay provide slash-command autocompletion through `suggestions.rs`. As the user types, the current line is scanned for a leading `/` followed by a partial command name. Matching commands from `app.available_commands` are scored and filtered, then displayed in a dropdown list above the textarea. `Tab`/`Enter` accept the highlighted suggestion; `Down`/`Ctrl+n` and `Up`/`Ctrl+p` navigate; `Esc` dismisses the suggestions without losing input. Commands come from the `available_commands` Vec populated from `commands.rs`.

## Prompt Correction (Ctrl+G)
**Category:** Input
**Description:** Flywheel integrates a prompt compilation feature via `Ctrl+G` in both the input bar and the prompt overlay. When triggered, the current draft is sent to a configured `prompt_processor` LLM (from `settings.prompt_processor`) which rewrites it as a linguistically complete, structurally valid prompt per the five-layer prompt compiler spec in `docs/prompt-compiler.md`. The result appears in a preview panel above the input. The user can accept (`a` to replace textarea), discard (`d`/`Esc`), or send the original regardless (`Ctrl+Enter`). A `CORRECTING...` badge shows while the correction is in-flight.

## RPC Protocol — Session Lifecycle Methods
**Category:** Daemon Communication
**Description:** The JSON-RPC 2.0 protocol over a Unix socket (`~/.flywheel/daemon.sock`) carries all TUI-to-daemon communication. Session lifecycle methods include: `LaunchSession` (creates a new subprocess session with query, working_dir, provider, model, system_prompt, session_kind, project_id, and continued_from), `GetSession` (fetch single session by ID), `ListSessions` (list all active sessions, optionally filtered by project_id), `DeleteSession` (hard delete), `InterruptSession` (SIGINT), `ContinueSession` (send follow-up query), `ApproveRequest` (approve/deny a pending tool use), `RotateSession` (trigger context rotation), `SwitchSessionModel` (change model mid-session), `TogglePin` (pin/unpin), and `UpdateSessionProject` (reassign project).

## RPC Protocol — Conversation Methods
**Category:** Daemon Communication
**Description:** Conversation data is fetched via `GetConversation` (single session, optionally incremental via `since_sequence`) and `GetConversationsSince` (batched multi-session fetch using `ConversationFetchCursor` list — the primary poll method). The batched endpoint returns a `ConversationBatchResponse` containing a `ConversationBatchEntry` per session with all new events since the provided sequence numbers. `GetTurnMetrics` fetches per-turn token usage analytics for a session. `GetModelSegments` returns the list of model segments (sequence ranges) for rendering model-switch dividers in the conversation view.

## RPC Protocol — Archive Methods
**Category:** Daemon Communication
**Description:** Archive operations are managed via four RPCs: `ArchiveSession` (soft-deletes an active session, moves to Archived status), `MarkPendingArchive` (flags an active session to auto-archive on completion), `ListArchivedSessions` (returns all archived sessions, optionally filtered by project_id), and `UnarchiveSession` (restores an archived session to Completed status). Archived sessions are excluded from `ListSessions` but remain in the database with full conversation history. The archive browser overlay uses these methods to provide a complete archive management interface.

## RPC Protocol — Project Methods
**Category:** Daemon Communication
**Description:** Project management is handled by five RPCs: `ListProjects` (returns all projects), `CreateProject` (name, path, description, color), `UpdateProject` (partial update by id), `DeleteProject` (by id), and `GetProject` (by id). All project mutations go through the daemon to ensure proper UUID generation, timestamp management, and ProjectIndex cache invalidation. The `color` field defaults to Catppuccin blue (`#89b4fa`). The `path` field is used by the daemon's `ProjectIndex` for automatic session-to-project assignment via longest-prefix matching against `Session.working_dir`.

## RPC Protocol — Discovery and Health Methods
**Category:** Daemon Communication
**Description:** Service discovery and health monitoring use four RPCs: `GetHealthStatus` returns operational metrics (persistence queue depth/capacity, last command duration, project cache size/hits/misses, last poll payload bytes/events, per-provider availability flags). `GetDaemonCapabilities` returns the `DaemonCapabilities` struct with feature flags (incremental_polling, batch_fetch, health_status, push_notifications, memory_search) for TUI-side feature negotiation. `DiscoverModels` queries the provider binaries for available models (Claude aliases, local Ollama /v1/models). `GetModelSegments` returns conversation model segments for divider rendering.

## RPC Protocol — Memory Methods
**Category:** Daemon Communication
**Description:** Four memory-related RPCs expose the daemon's semantic memory subsystem: `MemorySearch` performs hybrid FTS5+vector search over indexed files (query, max_results, min_score parameters), returning `MemorySearchResult` items with path, line range, score, and snippet. `MemoryStatus` returns `MemoryProviderStatus` describing the embedding backend, model, search mode, file/chunk counts, and availability flags. `MemoryIndex` triggers a forced re-sync of all memory files. `MemoryRead` returns the raw text of a specific memory file between specified line bounds. These are negotiated via the `memory_search` capability flag.

## SQLite Persistence — Schema and Migrations
**Category:** Persistence
**Description:** The daemon uses SQLite with WAL journal mode for all persistent storage in `~/.flywheel/flywheel.db`. The schema is version-tracked via `PRAGMA user_version` and applied through additive migrations. V0 creates the base `sessions`, `conversation_events`, and `approvals` tables with indices. V1 adds session metadata columns (cost_usd, duration_ms, num_turns, model, token counts) and the `turn_metrics` table. V2 adds the `projects` table and `sessions.project_id` FK. V3-V10+ add columns for pinning, rotation_depth, context tracking, model segments, session title, session_kind, daemon token counts, handoff_filepath, and pipeline_artifact. All timestamps use nanosecond-precision RFC 3339 format.

## Async SQLite Write Worker (store_worker)
**Category:** Persistence
**Description:** The daemon decouples SQLite writes from the session hot path using a `StoreWorker` that owns the `Store` connection and processes `StoreCommand` values from an mpsc channel. Commands include: InsertSession, UpdateSessionStatus, InsertEvent, InsertTurnMetric, UpdateSessionMetadata, AttachProject, and Shutdown. The channel has a configurable capacity (default 256) with a warn threshold at 80%. Queue depth, last command duration, and capacity are exposed via `StoreMetrics` (atomic counters) and reported in `GetHealthStatus`. This ensures event streaming latency is not gated on SQLite I/O.

## Event Bus (pub/sub)
**Category:** Daemon Architecture
**Description:** The daemon's `EventBus` in `bus.rs` provides broadcast pub/sub using a tokio `broadcast::Sender<Arc<DaemonEvent>>`. The `DaemonEvent` enum covers: SessionStatusChanged, ConversationEvent, SystemMessage, SessionDeleted, SessionArchived, SessionUnarchived, SessionMetadataChanged, MemoryIndexUpdated, and ContextUsageUpdated. Events are only sent when at least one subscriber is registered (tracked via `AtomicUsize`). The bus converts `DaemonEvent` to `BusEvent` (a struct with event_type string, timestamp, and JSON data) for external transport. The `Subscribe` RPC enables TUI clients to receive push notifications over a dedicated connection.

## Push Notification System
**Category:** Daemon Communication
**Description:** The daemon supports a `Subscribe` RPC that transitions a connection from request/response mode to push streaming mode. The TUI's `PollController` tracks `push_supported` from `GetDaemonCapabilities` and uses it to enable event-driven updates instead of pure polling. When subscribed, the daemon streams `BusEvent` JSON lines for session status changes, new conversation events, and metadata updates. The TUI's notification system (`types.rs::Notification`) renders these as time-limited toasts with priority levels (Low/Medium/High) and TTLs (5s for low/medium, 10s for high).

## Content Rendering — Markdown-Like Parsing
**Category:** Rendering
**Description:** The `ui/content.rs` module parses conversation event content into styled ratatui `Span` sequences. It handles bold (`**text**`), inline code (`` `code` ``), code fences (``` ```lang ... ``` ```), headers (`# title`), bullet lists (`- item`), numbered lists (`1. item`), blockquotes (`> text`), horizontal rules (`---`), and inline tool annotations. Text wrapping respects terminal width and optionally centers content on wide terminals when `center_content` is true. Content truncation is applied for collapsed events, with expansion via fold commands.

## Content Rendering — Syntax Highlighting
**Category:** Rendering
**Description:** `ui/highlight.rs` integrates `syntect` for code block syntax highlighting within conversation events. Language detection uses the fence label (e.g., ` ```rust`, ` ```python`) to select a `syntect` theme and grammar. Highlighted tokens are converted to ratatui `Color` values and applied as inline spans within code blocks. The syntax highlighter is initialized once at startup to avoid repeated grammar loading. When no fence language is specified, the code block is rendered with a monochrome style. Highlighting works in both the detail view and content-preview surfaces.

## Content Rendering — Height Pre-computation
**Category:** Rendering
**Description:** `ui/height.rs` pre-computes the exact number of terminal lines each conversation event will occupy after word-wrapping, enabling correct virtual scroll position calculation without rendering all content. Heights are cached per `(sequence, width, fold_state)` key and invalidated by `events_generation` counter increments. The `event_offsets` Vec stores cumulative line counts for O(1) scroll position translation. This system prevents the naive O(N) "render everything to count lines" approach and is essential for large conversations with hundreds of events.

## Content Rendering — PipeWire Audio Visualization
**Category:** Rendering
**Description:** `ui/audio.rs` renders a real-time audio level visualization in the status bar or session header using PipeWire as the audio backend. The visualizer samples audio levels and renders them as Unicode block characters with color gradients. This feature provides ambient context about audio activity (e.g., when a session is speaking or recording) in the terminal UI without requiring a separate audio monitor. The audio module is conditionally compiled and fails gracefully when PipeWire is unavailable.

## State Persistence — PersistedState
**Category:** State Management
**Description:** `PersistedState` in `state.rs` captures TUI state that must survive process restarts and is saved to `~/.flywheel/state.json` on every graceful exit. Persisted fields include: current project filter, sort order, session jumplist (Vec<Uuid>) and cursor, full tab/split layout, active tab index, next pane ID counter, last viewed session, selected model and provider, selected theme key, full `UserSettings`, and the merge queue (Vec<Uuid>). The state is loaded at startup via `serde_json::from_str` with `unwrap_or_default()` for graceful forward-compatibility when new fields are added.

## State Persistence — DevState
**Category:** State Management
**Description:** `DevState` captures the full UI snapshot for hot-reload scenarios, saved to `~/.flywheel/dev-state.json`. It extends `PersistedState` with per-session view state: scroll offsets, collapsed/expanded event sets, system event visibility, follow-tail flag, input bar vim mode and draft text content, and center-content flag. A `saved_at` Unix timestamp enables a freshness check (auto-clear if older than 10 seconds). The hot-reload workflow (`./scripts/dev-tui.sh`) uses SIGTERM to trigger `DevState::capture()` before process exit, allowing cargo-watch to restart with fully restored scroll positions and input drafts.

## Project Management
**Category:** Project Management
**Description:** Flywheel organizes sessions into projects via a first-class `Project` struct with id (UUID), name, path (optional PathBuf), description, color (Catppuccin hex), and timestamps. The daemon's `ProjectIndex` maintains a sorted `Vec<(PathBuf, Uuid)>` for longest-prefix-match lookup: when a session is launched in `/home/user/myproject/feature/`, the index returns the most specific project whose path is a prefix of that working directory. The TUI uses per-project workspace tabs, with each tab binding to one `project_id`. Sessions can be reassigned via the project picker or `:project-edit` command.

## Context Rotation / Handoff
**Category:** Session Management
**Description:** Flywheel implements context rotation to extend long-running sessions beyond Claude's context window limit. The `RotateSession` RPC triggers `ROTATION_HANDOFF_PROMPT` (`/create_handoff`) in the current session. When the session writes a handoff document (detected via Write tool interception), `handoff_filepath_detected` is set in `TrackedSession`. On completion, `spawn_rotation_child()` creates a new session with `continued_from` pointing to the parent and `rotation_depth` incremented. Auto-rotation is blocked at depth 4 to prevent infinite chains, injecting a System warning event. The `Session.rotation_depth` field and `continued_from` field track the chain.

## Session Kinds
**Category:** Session Management
**Description:** Three `SessionKind` variants classify sessions by their creation intent. `Standard` is the default for user-initiated sessions. `TaskRabbit` marks one-shot task executor sessions created via `Space+o` or `:task`, which use a specialized system prompt instructing the model to complete tasks concisely without follow-up questions and emit `[TASKRABBIT_ESCALATE]` if blocked. `Bug` marks sessions created via the bug report overlay for structured defect tracking. The session kind affects TUI filtering, rendering treatment (distinct colors), and notification kinds (TaskRabbitComplete vs. BugComplete).

## TaskRabbit System
**Category:** Session Management
**Description:** The TaskRabbit system provides a one-shot task execution mode for autonomous work. The `TaskRabbitPrompt` overlay (`Space+o`) accepts a task description, which is submitted via `LaunchSession` with `session_kind = TaskRabbit` and `system_prompt = TASKRABBIT_SYSTEM_PROMPT`. The system prompt instructs the model to complete tasks concisely, avoid follow-up questions, and emit `[TASKRABBIT_ESCALATE]` if blocked. When the session completes, a `TaskRabbitComplete` or `TaskRabbitFailed` notification is generated. The `:task <query>` command launches directly without opening the overlay.

## Session Jumplist
**Category:** Navigation
**Description:** Flywheel maintains a session navigation jumplist (similar to vim's `Ctrl+O`/`Ctrl+I` jump history) in `PersistedState.session_jumplist` (Vec<Uuid>) with a cursor position. `JumpBack` (`Ctrl+O`) navigates to the previous session in history; `JumpForward` (`Ctrl+I`) moves forward. Each time a session is explicitly opened, its UUID is appended to the jumplist, deduplicating adjacent identical entries. The jumplist and cursor persist across restarts, so recent navigation history is preserved. The status bar does not display the jumplist but the navigation is available at any time.

## Attention Queue Navigation
**Category:** Navigation
**Description:** Flywheel maintains an "attention queue" view of sessions requiring immediate action. The `]a` and `[a` keybindings (`NextAttention`/`PrevAttention`) cycle through sessions in `WaitingApproval` status that need user input. This allows rapid triage when multiple sessions simultaneously require tool use approval. The attention queue is derived dynamically from the session list by filtering for sessions in `WaitingApproval` status. Combined with the status bar's "N waiting" counter, this provides a complete workflow for managing approval queues.

## Tool Use Approval Flow
**Category:** Session Management
**Description:** When a Claude session requests tool use with `permission_mode: auto` disabled, it transitions to `WaitingApproval` status and writes an approval record to the `approvals` table. The TUI shows context-sensitive `a`/`d` keybindings only when the focused session is in `WaitingApproval`. Pressing `a` sends `ApproveRequest` with `approved: true`; `d` sends with `approved: false`. The daemon resolves the pending approval via `ApproveRequestParams` (approval_id, approved, comment) and resumes the session. The `WaitingCount` status bar segment shows the count of sessions waiting for approval.

## Token Tracking and Turn Metrics
**Category:** Analytics
**Description:** The daemon tracks token usage at two levels. API-reported tokens come from `stream-json` events (input, cache_creation, cache_read, output tokens per turn) and are accumulated in the `TrackedSession` live accumulators. Daemon-counted tokens use the `TokenCounter` (tiktoken cl100k_base BPE) to count content from raw stream events as a real-time fallback when API data is not yet available. Per-turn data is persisted to the `turn_metrics` table via the persistence queue. The `TurnMetric` struct captures all token categories, stop_reason, tools_used list, and model attribution. Context window fill percentage is computed and published via `ContextUsageUpdated` bus events.

## Model Discovery
**Category:** Provider Integration
**Description:** The `DiscoverModels` RPC triggers background model discovery for each provider. For Claude, the `ClaudeClient.discover_models()` queries the binary with model aliases (opus, sonnet, haiku) to resolve actual versioned model IDs (e.g., `claude-opus-4-6`). For Local (Ollama), it sends `GET /v1/models` to the local server and returns the available model list. Discovery results are cached in-memory and returned to the TUI, which updates `CLAUDE_MODELS`, `LOCAL_MODELS`, etc. at runtime. The TUI triggers model discovery via `needs_model_refresh` flag and receives results via a `oneshot::Receiver<Vec<(String, String)>>`.

## Poll Controller
**Category:** Daemon Communication
**Description:** The `PollController` manages the phased, non-blocking daemon poll loop in the TUI. The `PollPhase` enum steps through: Connect (reconnect if disconnected), ListSessions (fetch session list), ListProjects (fetch project list), FetchConversations (batch-fetch events for visible sessions). Between phases, the event loop can process keyboard input and render frames. The controller tracks `connected`, `batch_fetch_supported`, `push_supported`, and `memory_search_supported` capability flags negotiated via `GetDaemonCapabilities` on connect. The phased design prevents one slow RPC from blocking all rendering.

## Search — Session List Filtering
**Category:** Navigation
**Description:** Pressing `/` in the session list enters search mode, which filters sessions by case-insensitive substring match against session query/title text as the user types. The filter updates in real-time. `Enter` confirms the filter (list stays filtered until `Esc` in normal mode clears it). When switching tabs or panes, the search is cleared. Switching to detail view search mode changes to event content search within the selected session. The search target (`SearchTarget::SessionList` vs. `SessionDetail`) determines the behavior.

## Search — Event Content Search
**Category:** Navigation
**Description:** When `/` is pressed while a session detail pane is focused, search mode targets the conversation events within that session. As the user types, matching events are highlighted and the view scrolls to the first match. `n`/`N` (NextSearchMatch/PrevSearchMatch) cycle through matches. `Enter` keeps the highlighted match position and returns to normal mode. `Esc` in normal mode clears the search and restores the full event view. The search is implemented as a case-insensitive substring scan over event content strings.

## Fold System
**Category:** Navigation
**Description:** Conversation events support a fold system for managing information density. `zo` (OpenFold), `zc` (CloseFold), `za` (ToggleFold) operate on the event under the cursor (tracked via `current_event_index`). `zM` collapses all events; `zR` expands all. Tool use/result event pairs are collapsed by default (stored in `SessionState.collapsed_events`). Full expansion (bypassing truncation for long text) is tracked in `SessionState.expanded_events`. Fold states are serialized in `DevState` per session (not persisted across full restarts). Heights are recomputed when fold state changes via the `events_generation` invalidation mechanism.

## Event Visibility Toggles
**Category:** Navigation
**Description:** Two visibility toggles control which event types appear in the detail view. `zs` (ToggleSystemEvents) shows/hides `EventType::System` events (daemon-generated status messages). `zt` (ToggleThinkingEvents) expands/collapses `EventType::Thinking` events (Claude's extended reasoning content), which are normally rendered as a collapsed "N thinking blocks" bubble. Both states are per-session, stored in `SessionState.show_system_events` and `show_thinking_events`, and persisted in `DevState` for hot-reload survival.

## Yank Event Content
**Category:** Navigation
**Description:** The `yy` keybinding (YankEventContent) copies the text content of the conversation event under the cursor to the system clipboard using OSC 52 ANSI escape sequences. This works in any terminal that supports OSC 52 (most modern terminals including tmux with clipboard forwarding). The content excludes the event header (role, timestamp) and copies only the message/tool content string. This enables copying Claude's output to external applications without leaving the TUI.

## DocRegBlock Execution
**Category:** Session Management
**Description:** Flywheel implements a special document region block (`<docregblock>...</docregblock>`) tag system where embedded commands within session output can be launched as new sessions. `Space+x` (ExecuteDocRegBlocks) scans the current session's events for `<docregblock>` tags, extracts their content, and launches a new session for each one. `Space+X` (CommitAndPush) continues the current session with `/ci_commit` to trigger a commit-and-push workflow. The `docregblock_contents` Vec in `SessionState` caches detected blocks during event updates for fast access.

## Pipeline Artifact Detection
**Category:** Session Management
**Description:** The daemon monitors assistant text for paths matching `thoughts/shared/(research|plans|handoffs)/[filename].md` using a `LazyLock<Regex>`. When detected, `TrackedSession.pipeline_artifact` is set and propagated to the `Session` struct via `UpdateSessionMetadata`. The TUI can access this via the `Session.pipeline_artifact` field to render action buttons or inform the user that a pipeline artifact has been written. This supports a structured multi-agent research/planning pipeline where sessions produce artifacts consumed by subsequent sessions.

## Status Bar System
**Category:** Rendering
**Description:** The status bar is a configurable sequence of 15 named segments defined by `StatusBarSegment` enum: Mode badge, Connection status, Project indicator, Session count, Cost total, Active count, Waiting count, Merge queue depth, Notification count, Tab indicator, Selected model, Session metadata, Context usage %, Memory status, and Performance metrics. Each segment is conditionally displayed (e.g., Cost only shown when >= $0.01, ActiveCount only when > 0). The order of segments is user-configurable in the Settings pane ("Status Bar" category) via `J`/`K` reordering. The layout persists in `UserSettings`.

## Settings Pane
**Category:** Configuration
**Description:** The Settings pane (`Space+,` or `:set`) replaces the focused pane's content with a two-panel configuration editor. The left panel lists four categories: Display, Status Bar, Session Defaults, and API Models. Right-panel items for the selected category include boolean toggles (toggled with `Enter`/`Space`), enum cyclers, and the provider list (for API Models). Session Defaults includes default provider/model, prompt correction configuration, and other per-session defaults. Display settings include center-content, wrap behavior, and theme preferences. `q`/`Esc` closes settings and restores the previous pane content.

## Memory Subsystem
**Category:** Memory
**Description:** The daemon implements a full semantic memory system in `crates/flywheeld/src/memory/` (not fully read but referenced in documentation and config). It indexes markdown files from `~/.flywheel/memory/` into SQLite (`memory.sqlite`) using FTS5 for keyword search and sqlite-vec for vector similarity search. Embedding providers supported: Ollama (`GET /api/embed`) and OpenAI-compatible APIs, with "auto" detection via probe. Chunks are 400 tokens with 80-token overlap. Search uses a hybrid 0.7×vector + 0.3×FTS score with optional temporal decay (exponential with configurable half-life) and MMR re-ranking for diversity. Session transcripts of completed sessions are also indexed.

## Daemon Auto-Start
**Category:** Daemon Architecture
**Description:** The TUI's `main.rs` includes an `try_auto_start_daemon()` function that checks for the daemon socket at startup. If the socket does not exist or is stale (connection fails), it spawns `flywheeld` as a detached process in a new process group (preventing SIGHUP propagation). The daemon inherits a log file at `~/.flywheel/daemon.log` (append mode). The TUI then enters its reconnect loop (PollPhase::Connect) and displays connection status while waiting. This means users never need to manually start the daemon — simply running `flywheel` is sufficient.

## Log Rotation
**Category:** Operational
**Description:** The TUI performs log rotation on startup: if `~/.flywheel/tui.log` exists from a previous run, it is renamed to `tui-YYYYMMDD-HHMMSS.log` (with a numeric suffix for collision avoidance). Tracing output is written to the current `tui.log` file in non-ANSI format using `tracing_subscriber`. The daemon (by doc reference) also performs log rotation with gzip compression. This prevents unbounded log growth in long-running development environments.

## Hot-Reload Development Workflow
**Category:** Development
**Description:** Two `cargo-watch` scripts support hot-reload development: `./scripts/dev-daemon.sh` restarts flywheeld on source changes; `./scripts/dev-tui.sh` sends SIGTERM to the running TUI before cargo-watch restarts it. The SIGTERM handler calls `DevState::capture()` and `PersistedState::save()` before exiting, preserving all scroll positions, input drafts, and fold states. When the new process starts, it loads `DevState` (with a 10-second freshness check) and restores the full UI state, making hot-reload nearly seamless for development.

## Profiling System
**Category:** Development
**Description:** When `FLYWHL_PROFILE=1` is set, the TUI enables detailed performance instrumentation via `profiling.rs`. The `CacheCounters` struct tracks render cache hits and misses across all events. `AppMetrics` stores the last render time (ms), last poll time (ms), and cache hit rate. These are recorded in the main event loop and displayed in the Diagnostics overlay and the `PerfMetrics` status bar segment. This provides actionable performance data for optimizing render loop efficiency without external profilers.

## Theme System
**Category:** Rendering
**Description:** `ui/theme.rs` defines the full Catppuccin color palette (four official flavors: Latte, Frappé, Macchiato, Mocha) plus custom palettes (Goth with dark/minimal aesthetics, Junk Yard with warm/saturated colors). Themes are applied globally via a thread-local active theme key. Each theme defines named semantic colors for: base, surface, overlay, text, subtext, comment, accent colors (blue, green, yellow, red, mauve, teal, sky, sapphire, lavender, pink, flamingo, rosewater, peach). The theme system is used by all rendering modules for consistent color application.

## Mouse Support
**Category:** Input
**Description:** Flywheel enables mouse capture via `crossterm::event::EnableMouseCapture` at startup. Mouse events are handled in the event loop: left click sets the event cursor in the detail view by translating click coordinates against `last_content_area` to compute the event index. Mouse wheel scroll up/down moves 3 lines in the focused direction. Mouse support is disabled cleanly at exit via `DisableMouseCapture`. The keyboard enhancement flags (`REPORT_EVENT_TYPES`) are also enabled for improved key event reporting in supporting terminals.
