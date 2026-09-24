# Overlay System

> Modal popups that intercept all input when active

## Architecture

`OverlayState` is a single enum — one variant active at a time. When active, `handle_overlay_key()` intercepts all keyboard input before the normal dispatch pipeline. Overlays are rendered last in the frame (painter's algorithm — drawn over everything).

### Overlay Stacking

Some overlays support nesting via `saved_overlay`:

```
app.saved_overlay = Some(Box::new(current_overlay));
app.overlay = OverlayState::NewOverlay { .. };
// On close:
app.overlay = *app.saved_overlay.take();
```

Used when: ModelSelector opened from Blank prompt, AiChat/AiCommand opened from Prompt or InputModal.

## All Overlays (26 total)

### Text Input Overlays

| Overlay | Trigger | Purpose |
|---|---|---|
| **Prompt** | New session / Continue / TaskRabbit / Blank | Full InputSurface with vim modes. Ctrl+T→new tab, Ctrl+S→split, Ctrl+B→AiChat, Ctrl+A→AiCommand, Ctrl+P→compile |
| **InputModal** | Ctrl+G from input bar | Quarter-size popup with InputSurface. On submit→continue session. On close→transfers text back to input bar |
| **RenameSession** | F2 | Inline text field for session title |
| **AiChat** | Ctrl+B from prompt/input | Multi-turn Q&A on source text. Enter sends, Up/Down scroll history |
| **AiCommand** | Ctrl+A from prompt/input | Single-turn text transformation. Result delivered as `corrected_preview` |

### Selection Overlays

| Overlay | Trigger | Navigation | Purpose |
|---|---|---|---|
| **ModelSelector** | `M` | j/k, Tab cycles providers, 1-9 direct | Select AI model. 7 builtin providers + custom |
| **ThemePicker** | `T` | j/k, 1-9 direct | Live preview on navigation, reverts on cancel |
| **ProjectPicker** | `<Space>p` | j/k, type-to-filter | Global filter or session reassign. Ctrl+N/E/D for CRUD |
| **GroupPicker** | `<Space>gr` | j/k, type-to-filter | Assign session to group. Ctrl+N/E/D for CRUD |
| **SortPicker** | `<Space>s` | j/k | Select sort order from SortOrder::ALL |
| **Telescope** | `<Space><Space>` | j/k, type-to-filter | Fuzzy file finder (SkimMatcherV2). Enter opens file |

### Form Overlays

| Overlay | Trigger | Fields |
|---|---|---|
| **ProjectForm** | Ctrl+N from ProjectPicker | name, path, color (cycle through 8 Catppuccin colors) |
| **GroupForm** | Ctrl+N from GroupPicker | name, description, color |
| **ProviderForm** | From settings ApiModels | name, base_url, api_key, default_model |

### Browser Overlays

| Overlay | Trigger | Features |
|---|---|---|
| **ArchiveBrowser** | `ga` zone → overlay | Time-grouped sections. Enter opens, U unarchives |
| **NotificationBrowser** | `gn` | Active + dismissed history. x dismiss, N dismiss all, Enter→session |
| **PromptPreview** | `p` | Read-only query view with Ctrl+D/U scroll |
| **KeybindingsHelp** | `?` | Scroll + search modes. / activates filter |
| **MemorySearch** | `<Space>M` | Live search — each char triggers RPC |

### Focus-Mode Overlays (Gutter Rendering)

| Overlay | Trigger | Features |
|---|---|---|
| **MergeQueue** | `gm` | j/k nav, Enter marks cleared + opens, d removes |
| **RecentCompletions** | `gr` | j/k nav, Enter opens session, i→input mode |

### Interactive Overlays

| Overlay | Trigger | Features |
|---|---|---|
| **QuestionModal** | `gq` | Multi-question form for WaitingApproval. Normal/Insert modes, multi-choice with 1-9 keys |
| **FileExplorer** | `<Space>e` | Left-anchored tree drawer. yy yank path, dd delete, u undo, / fuzzy finder, Ctrl+L→file viewer focus |
| **Diagnostics** | `:diag` | Read-only profiling data (MOTHERSHIP_PROFILE=1) |
| **EspSquare** | `gc` | 3×3 grid game, 12 rounds. 1-9 numpad select, Enter confirm, p peek, saves with p-value |

## Overlay Key Handling Detail

### FileExplorer Split-Focus

The FileExplorer has a unique focus model: when `explorer_focused = false`, only `Ctrl+H` is intercepted (to re-focus the tree). All other keys pass through to the file viewer in the session detail. When focused, the explorer captures all input.

### Shared Patterns

- **List navigation**: Most selection overlays use `overlay/list.rs::handle_list_nav_key()` — handles j/k/g/G
- **Type-to-filter**: ProjectPicker, GroupPicker, Telescope accumulate characters into a query string, Backspace removes
- **Fuzzy matching**: Telescope and FileExplorer finder use `fuzzy_matcher::skim::SkimMatcherV2`
- **Esc behavior**: closes overlay or clears filter/input, context-dependent
- **q behavior**: closes in normal mode (most overlays), types character in insert mode
