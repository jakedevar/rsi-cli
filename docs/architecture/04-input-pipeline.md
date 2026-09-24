# Input Pipeline & Keybindings

> From keypress to action — the full dispatch chain

## Pipeline Overview

```
crossterm KeyEvent
    │
    ▼
┌─ Priority Chain ──────────────────────────────────────────┐
│                                                            │
│  [1] Ctrl+V clipboard paste intercept                     │
│  [2] Non-Press filter (discard Release/Repeat for Kitty)  │
│  [3] Hard-wired keys (Ctrl+C quit, Ctrl+O/I jump,        │
│       Ctrl+H/L pane nav, Shift+Up/Down events)            │
│  [4] overlay::handle_overlay_key()     ← if overlay active│
│  [5] file_viewer::handle_file_viewer_key()                │
│  [6] input_bar::handle_input_bar_key()                    │
│  [7] handle_detail_scroll_key()        ← plain Up/Down    │
│  [8] Mode dispatch:                                        │
│       Normal → key_manager.input_key() → LcAction         │
│       Input  → handle_input_mode()                         │
│       Command → handle_command_mode()                      │
│       Search  → handle_search_mode()                       │
│                                                            │
└────────────────────────────────────────────────────────────┘
    │
    ▼
dispatch_action()
    ├── Action::Application(lc) → dispatch_lc_action()
    │       ├── session::dispatch     → lifecycle operations
    │       ├── navigation::dispatch  → focus, folds, scrolling
    │       ├── overlay::dispatch     → open/close overlays
    │       └── window::dispatch      → tabs, splits, panes
    │
    ├── Action::CommandBar(Focus/Unfocus) → mode switch
    ├── Action::Editor(action)  → j/k/G/gg motions
    ├── Action::Scroll(style)   → Ctrl+d/u/f/b
    ├── Action::Tab(action)     → gt/gT/{n}gt
    ├── Action::Window(action)  → Ctrl+W h/j/k/l, :split/:vsplit
    └── Action::Command(Run)    → parse_command() → dispatch_lc_action()
```

## Normal Mode Keybindings

### Session Navigation
| Key | Action |
|---|---|
| `j` / `k` | Navigate up/down in list/detail |
| `G` / `gg` | Jump to bottom/top |
| `Enter` | Enter session detail |
| `H` / `L` | Ascend / drill into container (yazi-style; `L` mirrors `Enter`) |
| `Backspace` | Back to session list |
| `l` | Context-sensitive navigate right |
| `]a` / `[a` | Next/prev attention item |
| `]g` / `[g` | Next/prev group boundary |
| `gs` | Go to sessions zone |
| `ga` | Go to archive zone |
| `gt` (zone) | Go to TaskRabbit zone |
| `Ctrl+O` / `Ctrl+I` | Jumplist back/forward |
| `Ctrl+H` / `Ctrl+L` | Navigate panes/zones |

### Session Lifecycle
| Key | Action |
|---|---|
| `x` | Interrupt session |
| `X` / `<Space>c` | Quick continue |
| `DD` | Delete session |
| `<Space>a` | Archive session |
| `U` | Unarchive session |
| `P` | Toggle pin |
| `R` | Rotate session (context rotation) |
| `<Space>t` | Toggle testing needed |
| `F2` | Rename session |

### Fold Operations
| Key | Action |
|---|---|
| `zo` | Open fold |
| `zc` | Close fold |
| `za` | Toggle fold |
| `zM` | Close all folds |
| `zR` | Open all folds |
| `zs` | Toggle system events |
| `zt` | Toggle thinking events |

### Overlays
| Key | Action |
|---|---|
| `M` | Model selector |
| `T` | Theme picker |
| `<Space>p` | Project picker |
| `<Space>s` | Sort picker |
| `<Space>o` | TaskRabbit prompt |
| `<Space>m` | Blank prompt |
| `<Space>M` | Memory search |
| `<Space>e` | File explorer |
| `<Space><Space>` | Telescope (fuzzy file finder) |
| `<Space>gr` | Group picker |
| `p` | Prompt preview |
| `gq` | Question modal |
| `gm` | Merge queue |
| `gr` | Recent completions |
| `gn` | Notification browser |
| `gc` | ESP Square game |
| `?` | Keybindings help |

### Tabs & Panes
| Key | Action |
|---|---|
| `<` / `>` | Prev/next tab |
| `<Space>T` | Open session in new tab |
| `<Space>q` | Close focused pane |
| `Ctrl+W h/j/k/l` | Focus neighbor pane |

### Other
| Key | Action |
|---|---|
| `i` / `a` / `o` / `O` | Enter input bar (insert/append/open below/above) |
| `e` | Enter session normal mode |
| `yy` | Yank event content |
| `/` | Enter search |
| `n` / `N` | Next/prev search match |
| `<Space>x` | Execute docregblocks |
| `<Space>X` | Commit and push |
| `<Space>gg` | Toggle git panel (lazygit) |
| `<Space>,` | Open settings |
| `ZQ` / `ZZ` | Quit |

## Command Mode (`:`)

| Command | Aliases | Action |
|---|---|---|
| `:continue [q]` | `:cont` | Continue session |
| `:kill` | `:ki` | Interrupt |
| `:delete` | `:del` | Delete session |
| `:archive` | `:arc` | Archive |
| `:rotate` | `:rot` | Context rotation |
| `:model [name]` | `:mod` | Select model |
| `:theme [name]` | | Select theme |
| `:sessions` | `:ls` | List sessions |
| `:task [q]` | `:ta` | Launch TaskRabbit |
| `:blank [q]` | `:bl` | Launch blank session |
| `:project [name]` | | Switch project |
| `:project-new` | | Create project |
| `:set` | `:settings` | Open settings |
| `:group` | `:groups` | Group picker |
| `:context [text]` | `:ctx` | Set active task |
| `:diagnostics` | `:diag` | Open diagnostics |
| `:q` | `:quit` | Quit |

## InputSurface — Shared Vim-Modal Editing

Used by: input bar, prompt overlay, input modal. Wraps `tui_textarea::TextArea` with `PopupMode` (Insert/Normal) and `VimState`.

```
InputSurface::handle_key()
├── Ctrl+Enter → Submit (always)
├── Correction preview active → a (accept) / d,Esc (discard)
├── Insert mode:
│   ├── Suggestions visible → Tab/Enter/Ctrl+N/P/arrows
│   ├── Esc → Normal mode
│   ├── Arrow keys → cursor movement
│   └── All others → textarea.input() + auto_wrap_if_needed()
└── Normal mode:
    ├── q → Close (unless pass-through with empty content)
    └── All others → vim_textarea::handle_vim_normal()
```

Auto-wrap runs on every insert-mode keystroke: merges overflowing paragraph lines, breaks at word boundaries, restores cursor position.

## LcAction Enum — 83 Variants

All user-facing actions. Grouped:

- **Session Lifecycle** (3): Interrupt, Continue, QuickContinue
- **View Navigation** (2): EnterSession, BackToList
- **Focus & Attention** (5): QuestionModal, Next/PrevAttention, Notifications, DismissAll
- **Detail Navigation** (2): NextEvent, PrevEvent
- **Fold Operations** (7): Open/Close/Toggle fold, CloseAll/OpenAll, ToggleSystem/Thinking
- **Session Management** (8): Delete, Archive, Pin, Testing, Reassign, Rotate, Sort, Diagnostics, MemorySearch
- **Model/Theme** (4): OpenModelSelector, SelectModel, OpenThemePicker, SelectTheme
- **Projects** (5): OpenPicker, Switch, Create, Edit, Delete
- **Tabs** (3): OpenInNewTab, NextTab, PrevTab
- **Input Bar** (2): EnterInputBarInsert, EnterSessionNormalMode
- **Overlays** (3): KeybindingsHelp, PromptPreview, FileExplorer
- **DocRegBlock** (3): Execute, Continue, CommitAndPush
- **Jumplist** (2): JumpBack, JumpForward
- **TaskRabbit** (2): Prompt, Launch
- **Zone Navigation** (4): GoToArchive/Sessions/TaskRabbit, Unarchive
- **Other** (assorted): MergeQueue, RecentCompletions, Settings, Yank, Search, Scroll, Quit, Groups, ESP, Telescope, GitPanel, Rename, Blank
