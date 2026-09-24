# Vim Emulation

> Complete vim-mode implementation over tui_textarea

## Architecture

The vim emulation is a pure function pipeline — no widget struct. Callers own the `TextArea` and pass it with a `VimState` to `handle_vim_normal()`.

```
TextArea (tui_textarea)  +  VimState  →  handle_vim_normal()  →  VimAction
                                              │
                                    ┌─────────┴──────────┐
                                    │                     │
                              motions.rs            operators.rs
                              (cursor movement)     (d/c/y + motions)
                                    │                     │
                                    └────────┬────────────┘
                                             │
                                       text_objects.rs
                                       (iw, aw, i", a(, etc.)
```

## VimState

```
VimState
├── pending_operator: Option<char>     # d, c, y waiting for motion
├── pending_count: Option<usize>       # Numeric prefix (3dw)
├── visual: Option<VisualMode>         # Char | Line
├── visual_anchor: (usize, usize)      # Where visual started
├── last_char_search: CharSearch       # For ; and , repeat
├── last_change: ChangeRecord          # For dot-repeat
├── recording_change: ChangeRecording  # In-progress recording
├── pending_g: bool                    # After g prefix
├── pending_replace: bool              # After r prefix
├── pending_textobj_prefix: char       # After i/a in operator-pending
├── pending_char_search_dir: CharSearchDir  # After f/t/F/T
├── replaying: bool                    # Suppresses new recordings
├── insert_start_snapshot: String      # For diff-based dot-repeat
└── desired_col: Option<usize>         # Vim's curswant
```

## Normal Mode Dispatch (`handle_vim_normal`)

Six ordered phases per keypress:

```
Phase 0: Prefix resolution
├── pending_g → handle_g_prefix()        # gg, ge, gE, g0, g$
├── pending_replace → handle_replace_char()  # r{char}
├── pending_textobj_prefix → handle_textobj_key()  # iw, a", etc.
└── pending_char_search_dir → handle_char_search_target()  # f{char}, t{char}

Phase 1: Count accumulation
├── Digits 1-9 extend pending_count
└── 0 extends if count exists, else → head-of-line motion

Phase 2: Operator-pending mode
└── pending_operator set → handle_operator_pending()

Phase 2b: Visual mode
└── visual set → handle_visual_mode()

Phase 3: Normal key dispatch
├── Motion keys: h/l/j/k/w/b/e/0/$
├── Operator starts: d/c/y
├── Mode changes: i/a/o/O/A/I/s/S
├── Visual: v/V
├── Edits: x/X/r/J/~/p/P/u/Ctrl+R
├── Navigation: {/}/G/gg/%/f/t/F/T/;/,
└── Special: . (dot-repeat), Esc

Post-dispatch: clear desired_col (unless vertical motion)
```

## Operator-Pending Mode

When `d`, `c`, or `y` is pending, the next key is interpreted as a motion:

```
Operator + Motion combinations:
├── dw, cw, yw — word forward
├── db, cb, yb — word backward
├── de, ce, ye — word end
├── dl, cl, yl — single char right
├── dh, ch, yh — single char left
├── d$, c$, y$ — to end of line
├── d0, c0, y0 — to start of line
├── dG, cG, yG — to buffer end
├── dgg, cgg, ygg — to buffer start
├── dj, cj, yj — line down
├── dk, ck, yk — line up
│
├── dd, cc, yy — double operator = line selection
│   └── Select from head, down (count-1) lines, to next line start
│
└── diw, daw, di", da(, ... — text objects
    └── Delegates to text_objects.rs
```

### apply_operator()

- `d` → `textarea.cut()`
- `c` → `textarea.cut()` + enter insert mode
- `y` → `textarea.copy()` + `osc52_copy()` (system clipboard via OSC 52)

## Visual Mode

All motions extend the active selection. Operators apply to the selection:

```
Visual mode keys:
├── Motions: h/j/k/l/w/b/e/0/$^/G/gg/{/}/%
├── Operators: d/x (cut), c/s (cut+insert), y (copy+OSC52)
├── Mode toggle: v (char↔line), V (reverse)
├── Text objects: i{obj}/a{obj}
└── Esc → exit visual
```

## Text Objects (`text_objects.rs`)

All operate on `&[String]` at `(row, col)`:

```
TextObjectKind: Inner | Around

word_object()       — char class boundaries (word/whitespace/punctuation)
                       Around includes trailing/leading whitespace

delimited_object()  — dispatches to quote or bracket
├── quote_object()  — sequential pairing ("...", '...')
│                      Inner excludes quotes, Around includes
└── bracket_object() — nesting-aware multiline search
                       ( ) [ ] { } < >

sentence_object()   — .!? sentence boundaries
paragraph_object()  — contiguous non-blank lines
                       Around includes trailing blank lines
```

## Motions (`motions.rs`)

```
move_cursor_to(row, col)           — absolute positioning via Up/Down + Forward
move_vertical_with_curswant()      — vim's curswant for j/k across short lines
execute_char_search(f/t/F/T, char) — scan current line, t/T stops one before
find_matching_bracket_pos()        — pure scan (no cursor mutation), exported for renderer
move_to_matching_bracket()         — same algorithm, mutates cursor
move_paragraph_backward/forward()  — scan for blank line boundaries
```

## Dot-Repeat

```
Recording lifecycle:
1. start_recording(operator_key)         — on d/c/y/x/r/...
2. record_key(key)                        — accumulates keys during motion
3. finalize_change(action_fn)             — stores ChangeRecord
4. On '.': replay last_change.action_fn() — with replaying=true flag

Insert recording:
1. snapshot_for_insert()                  — captures textarea content
2. (user types in insert mode)
3. finalize_insert_from_snapshot()        — diff-based computation of inserted text
```

## Integration Points

- **File viewer**: calls `handle_vim_normal()` with `FileEditorCtx` (enables auto-indent on o/O)
- **Input bar / overlays**: calls `handle_vim_normal()` with `ctx = None` (no auto-indent)
- **Yank operations**: always call `osc52_copy()` for system clipboard sync
- **Bracket matching**: `find_matching_bracket_pos()` exported for file viewer highlight rendering
