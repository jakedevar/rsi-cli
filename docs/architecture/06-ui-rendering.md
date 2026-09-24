# UI Rendering System

> ratatui-based rendering with virtual scrolling and render caching

## Render Pipeline

Called once per frame from the event loop (120fps target, 8333µs interval):

```
render() [ui/mod.rs]
│
├── [1] Clear detection
│       If overlay just closed or pane type switched → full-frame Clear
│
├── [2] Layout decision
│       Single pane + wide gutter → 2-row (main + cmd bar) + HUD rails
│       Otherwise → 3-row (status + main + cmd bar)
│
├── [3] render_split_node() — recursive binary tree traversal
│       SplitNode::Leaf → render_pane()
│       SplitNode::Split → 50/50 split, recurse both children
│
├── [4] HUD Rails (right gutter dashboard)
│       Sections: Workspace, Status, Attention, Context, Cost, Stats,
│       Merge Queue, Recent Completions
│
├── [5] Status line (top row, or minimal in rail mode)
│       Configurable segments: Connection, Project, SessionCount, Cost,
│       ActiveCount, WaitingCount, Model, SessionMeta, ContextPercent, Perf
│
├── [6] Command bar (bottom row)
│       :cmd, >input, /search, or last notification
│
├── [7] Connection dot (2×1 at 0,0 — always on top)
│
└── [8] Overlay (full frame, painter's algorithm — last drawn)
```

## Layout Constants

```
MIN_WIDTH_FOR_CENTERING = 80
MAX_CENTERED_WIDTH      = 129
LAYOUT_X_OFFSET         = 25     # Right-shift for asymmetric gutter
SESSION_DETAIL_BOTTOM_INSET = 22 # Rows removed from detail bottom
SESSION_DETAIL_HORIZ_INSET  = 4  # 1 border + 1 padding per side
MIN_RAIL_WIDTH          = 18
MAX_RAIL_WIDTH          = 24
MAX_DASHBOARD_RAIL_WIDTH = 32    # Scales with terminal width
```

## Session List Rendering

### Card Height Pre-computation

```
compute_card_heights()
├── Skip if (width, count, generation) unchanged
├── For each session:
│   ├── build_card_view_model()
│   ├── height = header(1) + spacer(1) + title_lines + desc_lines + pill_row + borders(2)
│   ├── title_lines = count_wrapped_lines(text_width)
│   └── Store in ZoneRenderState.card_heights/card_offsets
└── Insert group headers (1 line each) when SortOrder::ByGroup
```

### Virtual Scrolling

```
render_zone_cards()
├── Adjust scroll_offset to keep selected card visible
├── Binary search card_offsets.partition_point() → first_visible
├── Iterate first_visible.. while card_top < visible_end
├── Render group headers at card_top - 1
└── render_card() for each visible card
```

### Card Layout

```
┌─────────────────────────────────────────────┐
│ ● ❯❯❯❯❯❯❯❯❯❯·····  $0.42 T:5  12m ▾     │ header
│                                              │ spacer
│   Session title that may wrap across         │ title
│   multiple lines...                          │
│                                              │ spacer
│   Description text in dim color...           │ description (expanded only)
│                                              │ spacer
│   [TR] [/research]                           │ pills (expanded only)
└─────────────────────────────────────────────┘
```

Chevron bar: 16-slot rainbow bar, filled slots = context %, `❯` filled / `·` empty.

## Session Detail Rendering

The pane header (`detail_header_line()`) is a single left-aligned line —
`Session Detail  id: …  started: …  status: …  info: F3` — with the
session's project name (resolved from `session.project_id` against
`app.projects`) rendered as a second, centered `project: <name>` label on
the same row (omitted if it would collide with the left-aligned metadata).
`F3` opens the fuller info panel overlay; the header line itself stays a
single row.

### Event Height Pre-computation (`height.rs`)

```
update_event_heights()  ← called every frame before rendering
├── Guard: skip if (generation, width, count) unchanged
├── For each event:
│   ├── Hidden system/tool-result → height 0
│   ├── Consecutive thinking (folded) → first: 2 (indicator + blank), rest: 0
│   ├── Collapsed tool-group summary → first: 3 + EVENT_CARD_GAP, rest: 0
│   ├── Compact ToolUse (no input) → 3 + EVENT_CARD_GAP
│   └── Others → ensure_render_entry() → content height + 2 (border) + EVENT_CARD_GAP
├── Build event_offsets (cumulative sums)
└── Evict stale render cache entries
```

`EVENT_CARD_GAP` (1 row) is a trailing separator baked into every bordered
card's height so two same-colored cards (e.g. two `ToolResult` bubbles,
both `tool_bubble_bg`) never render edge-to-edge and read as one fused
block. The render loop (`card_rect_within()` in `session.rs`) renders each
card into `event_height - EVENT_CARD_GAP` rows and leaves the trailing row
showing the pane's base background, which is filled once per frame before
any card renders.

### Render Cache

Keyed on `RenderCacheKey { sequence, width, is_collapsed, is_expanded, is_cursor, is_last_event }`. Stores `CachedRenderedEvent { lines: Vec<Line<'static>>, height, generation }`.

Cache is checked via `ensure_render_entry()` during height computation and `cached_render_event()` during rendering — ensuring measurements always match rendered output.

### Content Rendering (`content.rs`)

```
build_event_lines()
├── Role-colored accent bar: ┃ (blue=assistant, green=user)
├── Header: role label, model name, sequence #N, time HH:MM
├── Event-type indicator: 🔧 tool_name, ← Result, ℹ System, 💭 Thinking
├── Content:
│   ├── parse_content() → Vec<ContentSegment>  (Text | Code)
│   ├── Text segments:
│   │   ├── detect_block_element() → ATX headers, blockquotes, lists, tables, HR
│   │   ├── render_markdown_line() → styled Line with inline markdown
│   │   └── word_wrap() at MAX_PROSE_WIDTH=90
│   └── Code segments:
│       └── highlight::highlight_code() via syntect
├── Truncation at MAX_CONTENT_LINES=30 unless expanded
└── Separator line: ┄×40 (yellow if cursor, dim otherwise)
```

### Inline Markdown Parser (`parse_inline_markdown`)

```
Recognizes:
├── **bold** / __bold__     → BOLD modifier
├── *italic* / _italic_     → ITALIC modifier
├── ~~strikethrough~~       → CROSSED_OUT + color
├── `inline code`           → code fg/bg (or file path style if filepath-like)
├── [text](url)             → underlined label; local files hide the target and retain click metadata
├── <docregblock>           → colored pill
└── Bare file paths         → UNDERLINED + sapphire color
```

### Virtual Scrolling in Detail

```
render_session_detail()
├── find_visible_events() → binary search on event_offsets
├── For each visible event:
│   ├── Thinking indicator (💭 N events) if collapsed
│   ├── Tool group indicator (▶ N tool calls) if collapsed
│   ├── cached_render_event() → Vec<Line>
│   ├── Paragraph::new(lines).scroll(lines_to_skip)
│   └── Model-switch dividers between segments
└── Loading bar animation (rainbow chevrons, time-driven)
```

## Two Highlighting Systems

| System | Used For | Engine | Color Mapping |
|---|---|---|---|
| **syntect** (`highlight.rs`) | Code blocks in conversation | `find_syntax_by_token()` → `HighlightLines` | Catppuccin `.tmTheme` files |
| **tree-sitter** (`treesitter.rs`) | File viewer content | Language-specific grammars (13 languages) | Capture names → theme colors |

## Theme System (`theme.rs`)

Three palettes: Goth, JunkYard, Transparent. Each has 26 color fields. Active palette selected via `AtomicU8` — live-switching without restart.

```
ACTIVE_THEME_INDEX (AtomicU8)
    │
    ▼
active_palette() → &CustomPalette
    │
    ▼
themed_color_fn! macro generates 24 accessor functions:
    base(), mantle(), text(), blue(), green(), red(), mauve(), ...
    │
    ▼
Semantic role functions:
    assistant_role()=blue, user_role()=green, status_running()=green,
    md_header()=primary, code_block_bg()=surface0, file_path_fg()=sapphire, ...
```

## Input Bar Rendering

```
render_input_bar()
├── Layout: [Length(10), Length(2), Fill(1), Length(2)]
├── Left: Powerline pill (NORMAL/INSERT mode label)
│   └── Nerd Font half-circle glyphs (\u{e0b6}, \u{e0b4})
└── Right: render_wrapped_textarea()
    ├── Manual word wrapping (bypasses tui-textarea widget)
    ├── Selection highlights applied per-span
    ├── Paragraph::new(visual_lines).scroll((offset, 0))
    └── Cursor: set_cursor_position (beam) or reversed-span (block)
```

## Loading Bar Animation

Rainbow chevron bar. Time-driven: `Utc::now().timestamp_millis() / 60`. Two counter-moving chevron sets with blended palette colors. Uses `▄` (lower half block) for overlap layering.

## HUD Rails (Right Gutter)

```
render_hud_rails()
├── Rail width: scales from 24 to 32 cols based on terminal width
├── Position: flush right edge, 1 col padding
└── Stacked sections (only if remaining height >= lines + 2):
    ├── Workspace (project, provider, model)
    ├── Status (session counts by state)
    ├── Attention (pending questions, failures, stalls)
    ├── Fleet Context (chevron bars for all running sessions)
    ├── Cost (total, average)
    ├── Cost Stats (max, hourly burn rate)
    ├── Merge Queue
    └── Recent Completions
```

## File Viewer Rendering

```
render_file_viewer_content()
├── Fold filtering: skip folded lines
├── Tree-sitter highlight cache (invalidate on content change)
├── Per visible line:
│   ├── Gutter: absolute line# (cursor), relative (others), git sign, fold marker
│   ├── Content: highlighted spans, fold summary, trailing whitespace dots
│   └── apply_highlights(): search match bg, bracket match bg, cursor line bg
└── Two Paragraph widgets: gutter + content
```
