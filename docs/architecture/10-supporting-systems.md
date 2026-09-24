# Supporting Systems

> File viewer, git integration, clipboard, notifications, and more

## File Viewer

### Key Handler (`file_viewer.rs`)

Dispatch priority when file viewer is active:

```
1. Ctrl+S → save file
2. Command mode active → handle_file_command_key()
3. Search input active → handle_search_input()
4. Pending leader (Space) → Space+q (close), Space+m (markdown preview), Space+Space (telescope)
5. Pending z → za/zo/zc/zM/zR fold commands
6. : → enter command mode
7. / ? → activate search, n/N navigate, * # word search
8. Backspace → close (list) or pass-through (detail)
9. Normal mode → handle_vim_normal() with FileEditorCtx
10. Insert mode → auto-pair intercept, then input_surface
```

### Code Folding

Tree-sitter based folding for 13 languages (Rust, Python, JS/TS/TSX, Bash, JSON, TOML, YAML, Go, C/C++, Markdown):

```
recompute_folds_from_content()
├── Parse file with tree-sitter grammar
├── Walk AST for FOLDABLE_TYPES (blocks, declarations, bodies, arguments)
├── Store (start_line, end_line) pairs in FoldState
└── skip_folded_cursor() — jumps cursor past folded ranges on vertical movement
```

### File Viewer Commands (`:` in file viewer)

| Command | Action |
|---|---|
| `:w` | Save file |
| `:wq` / `:x` | Save and close |
| `:q` | Close viewer (not app) |
| `:q!` | Force close without save |
| `:e!` | Revert to disk version |
| `:{n}` | Go to line N |
| `:set autopair` / `:set noautopair` | Toggle auto-pair brackets |

### Auto-pair

Insert mode bracket matching:
- On `{/(/ [` → insert pair, cursor between
- On closing bracket → skip-over if next char matches
- On Backspace between matching pair → delete both

### Search

- `/` forward, `?` backward, `n` next, `N` previous
- `*` search word under cursor forward, `#` backward
- Unfolds target line before jumping

## Git Gutter (`git_gutter.rs`)

```
compute_git_gutter(file_path, line_count)
├── git diff --unified=0 HEAD -- <path>
├── parse_git_diff()
│   ├── Parse @@ hunk headers for old/new ranges
│   ├── Pure deletions → Deleted marker at adjacent line
│   ├── + lines < removed count → Modified
│   └── + lines >= removed count → Added
└── Vec<GitLineState>: Unchanged | Added | Modified | Deleted
```

Re-computed on save and revert.

## Clipboard (`clipboard.rs`)

### Write (OSC 52)
```
osc52_copy(text)
├── Base64 encode text (hand-rolled encoder)
└── Write \x1b]52;c;{base64}\x07 to stdout
```
Works over SSH and in all OSC 52-capable terminals.

### Read (Multi-backend)
```
read_clipboard()
├── [1] arboard native image → save PNG to paste_dir
├── [2] xclip image/png (X11 fallback for Flameshot)
├── [3] arboard text
├── [4] xclip text (final fallback)
└── Returns: ClipboardContent::Image{path} | Text(string) | Empty
```

## Notification Stream (`notification_stream.rs`)

Separate Unix socket connection for daemon push events:

```
NotificationStream::spawn(socket_path)
├── Connect to daemon socket
├── Send Subscribe RPC
├── Read ack, enter streaming mode
└── Forward BusEvent JSON lines to mpsc channel

Reconnect loop with exponential backoff: 1s → 2s → ... → 10s
Drop sends shutdown signal to background task
```

## Prompt Processor

Local LLM prompt compilation:

```
OpenAiCompatibleProcessor
├── compile(input) → CompileResult
│   ├── POST /v1/chat/completions with SYSTEM_PROMPT (5-layer compiler)
│   ├── Strip Qwen3 <think>...</think> blocks
│   ├── Check for COMPILE_ERROR:AMBIGUOUS_INTENT
│   ├── Parse contract line: COMPLETE | INCOMPLETE:x | ERROR:t:m
│   └── validate_layers() heuristic scan
│
└── send(system_prompt, message) → String
    └── Raw model call for AI chat/command modes
```

Config: default base URL `http://localhost:11434/v1`, model `qwen3:14b`, temperature `0.15`.

## Suggestions (`suggestions.rs`)

Slash command discovery for input bar:

```
discover_commands(working_dir)
├── .claude/commands/*.md → CustomCommand
├── .claude/skills/*/SKILL.md → Skill
└── Sorted alphabetically

filter_suggestions(commands, query)
└── SkimMatcherV2 fuzzy matching, sorted by score desc
```

## Session Navigation

### Jumplist (`app/jumplist.rs`)

Mirrors vim's jumplist for session navigation:
- `push_jumplist()` — deduplicates consecutive entries, caps at 100
- `jump_back()` (Ctrl+O) — first jump from SessionList restores last viewed
- `jump_forward()` (Ctrl+I) — increment cursor, handle deleted sessions
- `clean_jumplist()` — remove deleted session, preserve cursor position

### Search (`app/search.rs`)

Two targets:
- `SessionList` — filters by title/query match, calls `recalculate_filtered_order()`
- `SessionDetail` — scans event content, navigates via `scroll_offset = event_offsets[idx]`

### Merge Queue (`app/merge_queue.rs`)

```
auto_enqueue_merge_ready()
└── Scan completed sessions for docregblock containing "merge_ready"

auto_launch_resume_handoff()
└── Scan completed sessions for "resume_handoff <path>"
    └── Auto-launch new session with resume handoff command
```

## Settings System

### UserSettings (`settings.rs`)

Persisted as part of `PersistedState`:

```
UserSettings
├── show_audio_waveform, audio_viz_mode
├── default_show_system_events
├── default_show_thinking_events
├── default_hide_tool_results
├── sort_order: SortOrder
├── status_bar_segments: Vec<StatusBarEntry>  (ordered, toggleable)
├── custom_providers: Vec<CustomProviderEntry>  (UUID, name, URL, key, model)
├── prompt_processor: PromptProcessorConfig
└── card_fields: Vec<CardFieldEntry>  (field visibility)
```

### Settings Pane (`settings_keys.rs`)

Two-column layout: Categories (left) / Items (right).

Categories: Display, StatusBar, SessionDefaults, ApiModels, CardFields.

Special behaviors:
- StatusBar: J/K reorder segments
- ApiModels: Enter edits, `a` adds, `d` deletes
- SessionDefaults: changes apply to all existing sessions immediately

## Profiling (`profiling.rs`)

Enabled via `MOTHERSHIP_PROFILE` env var. Thread-local zero-lock counters:
- `record_cache_hit()` / `record_cache_miss()` → drain via `drain_cache_counters()`
- `start_timer()` / `log_duration()` → trace-level logs with target `rsi::profile`

## App Layout Management (`app/layout.rs`)

Binary tree of `SplitNode` per tab:

```
Operations:
├── alloc_pane_id()           — monotonic u64 counter
├── create_tab()              — inherits active project
├── open_session_in_new_tab() — SessionDetail leaf in new tab
├── open_session_in_new_split() — replace_node() to split current
├── split_focused()           — same pattern, second pane = SessionList
├── close_focused_pane()      — if only pane: close tab; else remove + refocus
├── close_other_panes()       — replace entire layout with single focused clone
└── focus_neighbor(dir)       — cycle through leaf_ids() wrapping
```
