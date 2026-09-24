# TUI Event Loop & Application State

> `rsi` — the vim-like TUI client

## Initialization (`main.rs`)

```
main()
├── Install color_eyre
├── Setup file-based tracing (~/.rsi/tui.log)
├── try_auto_start_daemon()
│   ├── Check for daemon.sock
│   ├── If absent, spawn rsid as detached process
│   └── Redirect daemon stdout/stderr to daemon.log
├── Enable crossterm raw mode + alternate screen + mouse + Kitty protocol
├── Create DaemonClient → App::new()
├── run_event_loop() ← blocks here
└── Cleanup: paste files, raw mode, alternate screen
```

## Event Loop (`event.rs`)

Single-threaded tokio (`current_thread` flavor). Interleaves six concerns via `tokio::select!` with biased priority:

```
tokio::select! (biased) {
│
├── [P1] render_interval (120fps, 8333µs)
│         └── terminal.draw() if app.needs_redraw
│
├── [P2] crossterm reader.next()
│         └── Key/mouse/resize events → dispatch pipeline
│
├── [P3] notification_stream.recv()
│         └── Push events from daemon → app.apply_push_event()
│
├── [P4] poll_sleep (500ms or 5s) if no active poll
│         └── start_poll_cycle() → PollPhase::ListSessions
│
├── [P5] poll_step() if active poll phase
│         └── Advance one RPC step per loop iteration
│
├── [P6-P9] Async channel receivers
│         ├── model_discovery_rx
│         ├── prompt_compile_rx
│         ├── input_bar_compile_rx
│         └── ai_chat_rx / ai_command_rx
│
├── [P10-11] Signal handlers (SIGTERM, SIGHUP)
│
├── [P12] persist_interval (30s) → save PersistedState
│
├── [P13] taskrabbit_recalc_interval (60s)
│
└── [P14] repaint_interval (1s) → defensive repaint
}
```

### Startup Before Loop

```
1. app.connect()
2. app.poll_sessions()                    # Blocking initial poll
3. app.apply_pending_dev_state()          # Restore hot-reload state
4. app.apply_pending_fold_states()        # Restore cold-start fold states
5. app.reconcile_restored_selections()    # Validate pane selections
6. Expand initially selected card
```

## Poll Phase State Machine

Non-blocking phased polling — one RPC call per event loop iteration:

```
Connect ──→ ListSessions ──→ ListProjects ──→ ListGroups ──→ FetchConversations
                                                                │
                                                    Batches of 4 sessions
                                                    per iteration until done
```

`FetchConversations` carries `(ids: Vec<Uuid>, index: usize)` and advances 4 sessions per tick. When push notifications are active, only unloaded sessions are fetched.

## Application State (`App` struct)

### Daemon Connection
- `client: DaemonClient` — Unix socket RPC
- `poll: PollController` — connected, batch_fetch_supported, push_supported, memory_search_supported
- `notification_stream: Option<NotificationStream>` — second socket for push events

### Session Data
- `sessions: HashMap<Uuid, SessionState>` — all known sessions
- `session_order: Vec<Uuid>` — insertion order
- `filtered_session_order / filtered_taskrabbit_order / filtered_archived_order` — view-filtered lists

### Layout
- `tabs: Vec<Tab>` — each tab has independent `SplitNode` binary tree
- `active_tab: usize`
- `next_pane_id: u64` — monotonic counter

### Input
- `input_mode: InputMode` — Normal / Input / Command / Search
- `key_manager: LcKeyManager` — modalkit vim machine
- `vim_machine_pending: bool` — mid-sequence flag

### Overlay
- `overlay: OverlayState` — active overlay (one at a time)
- `saved_overlay: Option<Box<OverlayState>>` — stacked overlay for nesting

## State Persistence

Two layers:

### PersistedState (`~/.rsi/state.json`)
Survives restarts. Always valid if parseable.

```
current_project_id, sort_order, session_jumplist, jumplist_cursor,
tabs, active_tab, next_pane_id, last_viewed_session,
selected_model, selected_provider, theme_flavor,
settings: UserSettings, merge_queue, merge_queue_cleared,
session_fold_states: HashMap<String, bool>
```

### DevState (`~/.rsi/dev-state.json`)
Survives hot-reloads only (10-second TTL). Superset of PersistedState plus:

```
session_views: HashMap<String, SessionViewState>
├── scroll_offset, collapsed_events, expanded_events
├── show_system_events, show_tool_results, follow_tail
├── input_bar_mode, input_bar_lines
├── center_content, list_card_expanded
└── file_viewer (path, lines, cursor, folds)
```

### Restoration Order
```
App::new()
├── DevState (if saved_at within 10s) → full restore with scroll positions
├── PersistedState (cold start) → tabs + project filter + settings
└── Hardcoded defaults (first launch)
```

## DaemonClient (`client.rs`)

Unix socket JSON-RPC client:

- `request()` — synchronous: write JSON + `\n`, read response line
- `fire_and_forget()` — async: write only, set `pending_response = true`
- `drain_pending()` — consume outstanding response before next request

Socket path: `MOTHERSHIP_SOCKET` env var → `~/.rsi/daemon.sock` → `/tmp/rsi-daemon.sock`

## SessionState — Per-Session TUI State

```
SessionState
├── session: Session                      # Daemon data
├── events: Vec<ConversationEvent>        # Full event log
├── model_segments: Vec<ModelSegment>     # For model-switch dividers
├── scroll_offset: usize                  # Virtual scroll position
├── collapsed_events: HashSet<i32>        # Collapsed tool pairs
├── expanded_events: HashSet<i32>         # Fully expanded events
├── event_heights/offsets: Vec<usize>     # Pre-computed layout
├── total_content_height: usize
├── follow_tail: bool                     # Auto-scroll to bottom
├── input_bar: InputBarState              # Persistent input
├── render_cache: HashMap<RenderCacheKey, CachedRenderedEvent>
├── last_sequence: Option<i32>            # Drives incremental fetch
├── file_viewer: Option<FileViewerState>  # Active file viewer
├── file_viewer_cache: HashMap<PathBuf, FileViewerState>
└── list_card_expanded: bool              # Accordion state
```
