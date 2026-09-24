# rsi Keybindings Reference

This document contains all keybindings used in the rsi TUI application. It follows vim-style modal editing principles with context-sensitive behavior.

For the operating model behind these controls—sessions, providers, projects,
sandboxes, hierarchy, diagnostics, and capability boundaries—start with the
[RSI Operator Manual: AI-Agent Harness](agent-harness-operator-manual.md).

**Last Updated**: 2026-09-22

---

## Global Keybindings

These work in all modes unless otherwise specified:

<!-- rsi:generated:begin raw -->
Keys the event loop decodes itself, before or beside the Vim keymap. Each table is consulted in order; the first row whose context holds wins.

**Clipboard paste** — Checked before anything else, on key press or release.

| Chord | Context | Effect |
| --- | --- | --- |
| `Ctrl-V` | any context (press or release) | paste the clipboard into the active overlay or input bar (text or image) |

**Submissions held until the daemon config is ready** — Until the TUI has applied an authoritative daemon configuration, these submissions are refused without closing the editor.

| Chord | Context | Effect |
| --- | --- | --- |
| `Enter` | quick-launch input, daemon config pending | refused with a notice until the daemon config is authoritative; the draft is kept |
| `Ctrl-Enter` | launch prompt, daemon config pending | refused with a notice until the daemon config is authoritative; the draft is kept |
| `Ctrl-T` | launch prompt, daemon config pending | refused with a notice until the daemon config is authoritative; the draft is kept |
| `Ctrl-S` | launch prompt, daemon config pending | refused with a notice until the daemon config is authoritative; the draft is kept |
| `Ctrl-Enter` | create form, daemon config pending | refused with a notice until the daemon config is authoritative; the draft is kept |
| `Enter` | create form (normal mode, one-line field), daemon config pending | refused with a notice until the daemon config is authoritative; the draft is kept |

**Global intercepts** — Consulted in order ahead of overlays, input surfaces and the Vim keymap; the first row whose context holds wins.

| Chord | Context | Effect |
| --- | --- | --- |
| `Ctrl-Alt-G` | any context | toggle contextual help without typing into the active editor |
| `Ctrl-C` | embedded terminal open | send SIGINT to the embedded terminal's shell |
| `Ctrl-C` | any context | quit rsi |
| `Ctrl-\` | any context | toggle the embedded terminal overlay (the shell keeps running) |
| `Ctrl-O` | normal mode, no overlay, not inserting | jump back in the session jumplist |
| `Ctrl-I` | normal mode, no overlay, not inserting | jump forward in the session jumplist |
| `Ctrl-H` | session list focused (normal mode, no overlay) | previous session-list zone (Main ← TaskRabbit ← Jobs ← Archive, wrapping) |
| `Ctrl-H` | other pane focused (normal mode, no overlay) | focus the pane to the left |
| `Ctrl-L` | session list focused (normal mode, no overlay) | next session-list zone (Main → TaskRabbit → Jobs → Archive, wrapping) |
| `Ctrl-L` | other pane focused (normal mode, no overlay; not a stale Issues editor) | focus the pane to the right |
| `Ctrl-Shift-Up` | launch prompt or input modal open | make the overlay shorter |
| `Ctrl-Shift-Down` | launch prompt or input modal open | make the overlay taller |
| `Ctrl-Shift-Left` | launch prompt or input modal open | make the overlay narrower |
| `Ctrl-Shift-Right` | launch prompt or input modal open | make the overlay wider |
| `Ctrl-Up` | launch prompt or input modal open | move the overlay up |
| `Ctrl-Down` | launch prompt or input modal open | move the overlay down |
| `Ctrl-Left` | launch prompt or input modal open | move the overlay left |
| `Ctrl-Right` | launch prompt or input modal open | move the overlay right |
| `Ctrl-0` | launch prompt or input modal open | reset the overlay's size and position |
| `Ctrl-Shift-Right` | normal mode, no overlay | widen the session-list sidebar |
| `Ctrl-Shift-Left` | normal mode, no overlay | narrow the session-list sidebar |
| `Ctrl-Left` | normal mode, no overlay, not inserting | focus the pane to the left (zones: gs / gt / gj / ga) |
| `Ctrl-Right` | normal mode, no overlay, not inserting | focus the pane to the right (zones: gs / gt / gj / ga) |
| `Shift-Down` | normal mode, no overlay, not inserting | select the next transcript event |
| `Shift-Up` | normal mode, no overlay, not inserting | select the previous transcript event |

**Session-list arrows** — Consulted after overlays, the file viewer and the input bar decline the key.

| Chord | Context | Effect |
| --- | --- | --- |
| `Left` | session list or detail focused, not inserting | previous session in the list |
| `Right` | session list or detail focused, not inserting | next session in the list |
| `Shift-Left` | session list or detail focused, not inserting | previous session and open its detail |
| `Shift-Right` | session list or detail focused, not inserting | next session and open its detail |
| `Ctrl-Tab` | session list or detail focused, not inserting | previous session in the list |
| `Ctrl-Shift-Tab` | session list or detail focused, not inserting | next session in the list |

**Session-detail scrolling** — Plain arrows scroll the focused transcript; Shift-Up/Down select events instead.

| Chord | Context | Effect |
| --- | --- | --- |
| `Up` | session detail focused | scroll the transcript up 3 lines (selection stays visible) |
| `Down` | session detail focused | scroll the transcript down 3 lines (selection stays visible) |

**Normal-mode follow-ups** — Run after the Vim keymap has handled the same key.

| Chord | Context | Effect |
| --- | --- | --- |
| `Esc` | normal mode with a confirmed search | clear the search query, matches and filter (after the Vim keymap runs) |

**One-line editors** — The quick-launch input, `:` command line and `/` search line.

| Chord | Context | Effect |
| --- | --- | --- |
| `Enter` | quick-launch / continue input line | launch a new session (or continue the target) with the typed query |
| `Esc` | quick-launch / continue input line | discard the query and return to normal mode (kept while a launch is pending) |
| `Backspace` | quick-launch / continue input line | delete the last character |
| `<char>` | quick-launch / continue input line | append the typed character |
| `Enter` | : command line | run the ex command and return to normal mode |
| `Esc` | : command line | discard the command and return to normal mode |
| `Backspace` | : command line | delete the last character |
| `<char>` | : command line | append the typed character |
| `Enter` | / search line | confirm the search; the filter and match position stay active |
| `Esc` | / search line | cancel the search and clear the query, matches and filter |
| `Backspace` | / search line | delete the last character and re-run the search |
| `<char>` | / search line | append the typed character and re-run the search (incremental) |
<!-- rsi:generated:end -->

Normal-mode chords such as `?` and `<Space>s` are listed in the Normal-mode table below.

---

## Normal Mode Keybindings

### All Normal-Mode Keys (generated)

<!-- rsi:generated:begin normal -->
Every Normal-mode chord, from the action registry. Vim motions such as `j`, `k`, `gg`, `G` and counts come from modalkit.

| Keys | Action | What it does |
| --- | --- | --- |
| `?` | Contextual help | Opens contextual help listing the keys and commands available in the current view; Ctrl-Alt-G works even while typing. |
| `Ctrl-Alt-G` | Contextual help | Opens contextual help listing the keys and commands available in the current view; Ctrl-Alt-G works even while typing. |
| `j` | Move selection or scroll down | Moves the selection down one row, or scrolls the focused view down. |
| `k` | Move selection or scroll up | Moves the selection up one row, or scrolls the focused view up. |
| `gg` | Jump to first item | Jumps the session-list selection to the first row. |
| `G` | Jump to last item | Jumps the session-list selection to the last row. |
| `Enter` | Open or activate selection | Opens the selected row: drills into a Group or Epic in place, opens a leaf session's detail, or activates the selected setting. |
| `L` | Open or activate selection | Opens the selected row: drills into a Group or Epic in place, opens a leaf session's detail, or activates the selected setting. |
| `/` | Filter sessions | Starts an incremental `/` filter over the session list; Enter keeps the filter, Esc clears it. |
| `r` | Refresh navigation | Re-fetches sessions, projects and labels from the daemon. |
| `T` | Choose built-in theme | Opens the built-in theme picker; `:theme <name>` applies a theme directly. |
| `<Space>b` | Edit legacy message/editor colors | Opens the legacy message and editor color customizer. |
| `<Space>i` | Open or focus Issues workspace | Opens the Issues workspace pane for the current project, or focuses it if already open. |
| `<Space>gp` | Edit manager policy | Opens the harness manager policy editor (operator-owned manager limits and choices). |
| `<Space>gb` | Open manager board | Opens the harness manager work board. |
| `<Space>gd` | Open manager decisions | Opens the harness manager decisions queue awaiting the operator. |
| `yy` | Copy Session UUID | Copies the selected session's UUID; in a transcript `yy` copies the selected event's content instead. |
| `x` | Interrupt running session | Interrupts the selected running session. |
| `X` | Continue selected session | Continues the selected session: opens a continue prompt, or `:continue <text>` sends the text directly. |
| `<Space>c` | Continue selected session | Continues the selected session: opens a continue prompt, or `:continue <text>` sends the text directly. |
| `P` | Pin or unpin selected session | Pins or unpins the selected session at the top of the list. |
| `<Space>a` | Archive selected session | Archives the selected session (reversible with U from the Archive zone). |
| `U` | Unarchive selected session | Restores the selected archived session to the Main zone. |
| `<Space>t` | Toggle testing-needed marker | Marks or clears the testing-needed flag on the selected leaf session. |
| `-` | Ascend hierarchy | Ascends one container level in the session hierarchy; does nothing at the root. |
| `H` | Ascend hierarchy | Ascends one container level in the session hierarchy; does nothing at the root. |
| `DD` | Delete selected session | Deletes the selected session after the double-tap `DD` (moves it to the trash). |
| `R` | Rotate session context | Rotates the selected session into a fresh context, carrying a handoff forward. |
| `ga` | Open archives | Switches the session list to the Archive zone. |
| `<Space>n` | Show attention alerts | Toggles the notification and attention history overlay. |
| `ZQ` | Quit | Quits rsi (the daemon and its sessions keep running). |
| `ZZ` | Quit | Quits rsi (the daemon and its sessions keep running). |
| `<Space>p` | Choose project | Opens the project picker. |
| `<Space>o` | Launch task session | Opens the TaskRabbit one-shot prompt; `:task <text>` launches it directly. |
| `<Space>m` | Launch blank session | Opens the blank general-purpose session prompt; `:blank <text>` launches it directly. |
| `<Space>,` | Open settings | Opens the settings pane. |
| `<Space>S` | Emergency stop all | Emergency stop: denies new paid work and cancels live model invocations. |
| `<Space>v` | Open graph review | Opens the visual workflow graph review editor. |
| `gL` | Set Epic lead | Sets the focused leaf session as the lead of its parent Epic. |
| `<Space>q` | Close pane | Closes the focused pane. |
| `>` | Next tab | Switches to the next tab. |
| `<` | Previous tab | Switches to the previous tab. |
| `n` | Next search match | Moves to the next match of the confirmed `/` search. |
| `N` | Previous search match | Moves to the previous match of the confirmed `/` search. |
| `]a` | Next session needing attention | Jumps to the next session waiting for you (approval, question or failure). |
| `[a` | Previous session needing attention | Jumps to the previous session waiting for you. |
| `<Space>1` | Jump to attention slot N | Jumps to the Nth session in the attention queue (`<Space>1` to `<Space>9`; bare digits stay Vim counts). |
| `<Space>2` | Jump to attention slot N | Jumps to the Nth session in the attention queue (`<Space>1` to `<Space>9`; bare digits stay Vim counts). |
| `<Space>3` | Jump to attention slot N | Jumps to the Nth session in the attention queue (`<Space>1` to `<Space>9`; bare digits stay Vim counts). |
| `<Space>4` | Jump to attention slot N | Jumps to the Nth session in the attention queue (`<Space>1` to `<Space>9`; bare digits stay Vim counts). |
| `<Space>5` | Jump to attention slot N | Jumps to the Nth session in the attention queue (`<Space>1` to `<Space>9`; bare digits stay Vim counts). |
| `<Space>6` | Jump to attention slot N | Jumps to the Nth session in the attention queue (`<Space>1` to `<Space>9`; bare digits stay Vim counts). |
| `<Space>7` | Jump to attention slot N | Jumps to the Nth session in the attention queue (`<Space>1` to `<Space>9`; bare digits stay Vim counts). |
| `<Space>8` | Jump to attention slot N | Jumps to the Nth session in the attention queue (`<Space>1` to `<Space>9`; bare digits stay Vim counts). |
| `<Space>9` | Jump to attention slot N | Jumps to the Nth session in the attention queue (`<Space>1` to `<Space>9`; bare digits stay Vim counts). |
| `]g` | Next label group | Moves the selection to the first session of the next label group. |
| `[g` | Previous label group | Moves the selection to the first session of the previous label group. |
| `gs` | Go to Main zone | Switches the session list to the Main zone. |
| `gt` | Go to TaskRabbit zone | Switches the session list to the TaskRabbit zone of one-shot sessions. |
| `gj` | Go to Jobs zone | Switches the session list to the Jobs zone of scheduled-job sessions. |
| `gr` | Recent completions | Focuses the recent-completions list in the right sidebar. |
| `l` | Enter from list | Enters the selected session from the list (moves focus right). |
| `Backspace` | Ascend or go back | Ascends one container level, or returns from session detail to the list at the root. |
| `<Space>s` | Choose sort order | Opens the session-list sort order picker. |
| `<Space>gX` | Open trash | Opens the trash browser of deleted sessions. |
| `i` | Type into the input bar | Enters the input bar in insert mode: `i` insert, `a` append, `o` / `O` open a new line below / above. |
| `a` | Type into the input bar | Enters the input bar in insert mode: `i` insert, `a` append, `o` / `O` open a new line below / above. |
| `o` | Type into the input bar | Enters the input bar in insert mode: `i` insert, `a` append, `o` / `O` open a new line below / above. |
| `O` | Type into the input bar | Enters the input bar in insert mode: `i` insert, `a` append, `o` / `O` open a new line below / above. |
| `e` | Session normal mode | Enters the selected session's detail with the input bar in normal mode. |
| `p` | Preview session prompt | Shows the full launch prompt of the selected session. |
| `]u` | Next user message | Selects the next user message in the transcript. |
| `[u` | Previous user message | Selects the previous user message in the transcript. |
| `zo` | Open fold | Expands the selected transcript event. |
| `zc` | Close fold | Collapses the selected transcript event. |
| `za` | Toggle fold | Toggles the selected transcript event between expanded and collapsed. |
| `zM` | Close all folds | Collapses every event in the transcript. |
| `zR` | Open all folds | Expands every event in the transcript. |
| `zs` | Toggle system events | Shows or hides system events in this transcript. |
| `zt` | Toggle thinking events | Shows or hides model thinking events in this transcript. |
| `F3` | Session info | Opens the session info panel: id, provider and model, working directory, project, hierarchy, rating (1-10), label, tags and context usage. |
| `F2` | Rename session | Renames the selected session inline. |
| `Ctrl-M` | Model dropdown | Opens or closes the model dropdown for the next launch (plain `M` stays free for text surfaces). |
| `<Space>C` | Change session project | Moves the selected session to another project. |
| `<Space>r` | Toggle auto-rotation | Disables or re-enables automatic context rotation for the selected session. |
| `<Space>k` | Cancel pending retry | Cancels the selected session's pending automatic retry. |
| `<Space>x` | Run docregblock tags | Launches a session for every docregblock tag in the selected session's output. |
| `<Space>X` | Commit and push | Continues the selected session with `/ci_commit` to commit and push its work. |
| `<Space>T` | Open in new tab | Opens the selected session in a new tab. |
| `<Space>gg` | Git panel (lazygit) | Suspends the TUI and runs lazygit in the session's working directory. |
| `<Space>e` | File explorer | Toggles the left-anchored file explorer drawer. |
| `<Space><Space>` | Find file | Opens the fuzzy file finder. |
| `gf1` | Open recent file N | Opens the Nth most recent file (`gf1` to `gf9`). |
| `gf2` | Open recent file N | Opens the Nth most recent file (`gf1` to `gf9`). |
| `gf3` | Open recent file N | Opens the Nth most recent file (`gf1` to `gf9`). |
| `gf4` | Open recent file N | Opens the Nth most recent file (`gf1` to `gf9`). |
| `gf5` | Open recent file N | Opens the Nth most recent file (`gf1` to `gf9`). |
| `gf6` | Open recent file N | Opens the Nth most recent file (`gf1` to `gf9`). |
| `gf7` | Open recent file N | Opens the Nth most recent file (`gf1` to `gf9`). |
| `gf8` | Open recent file N | Opens the Nth most recent file (`gf1` to `gf9`). |
| `gf9` | Open recent file N | Opens the Nth most recent file (`gf1` to `gf9`). |
| `<Space>gP` | Prompt creator | Opens the prompt creator and editor. |
| `<Space>;` | Command palette | Opens the command palette, the same palette as `:`. |
| `<Space>gq` | Answer waiting question | Opens the question modal for a session waiting on your answer. |
| `<Space>M` | Memory search | Opens memory search over the daemon's stored memories. |
| `<Space>K` | Scheduled jobs | Opens the scheduled jobs browser. |
| `<Space>gc` | ESP Square game | Opens the ESP Square guessing game. |
| `gR` | Run Epic topology | Runs the focused Epic's bound workflow topology. |
| `<Space>G` | Create Group | Creates a top-level Group container. |
| `<Space>E` | Create Epic | Creates an Epic container under the current Group. |
| `<Space>gS` | Create Story | Creates a Story leaf under the current Epic. |
| `<Space>gT` | Create Task | Creates a Task leaf under the current Epic. |
| `<Space>B` | Create Bug | Creates a Bug leaf under the current Epic. |
| `mp` | Move to parent | Opens the parent picker to move the focused session under another container. |
| `mo` | Move to top level | Moves the focused session to the top level. |

Reserved sequences: prefixes wait for the next key; no-ops keep retired chords from falling through to another key's action.

| Keys | Reserved as | Why |
| --- | --- | --- |
| `D` | prefix | prefix of DD (Vim's D would delete to end of line) |
| `y` | prefix | prefix of yy (Vim's y is the yank operator) |
| `<Space>` | prefix | leader key |
| `gf` | prefix | prefix of gf1..gf9 (Vim's gf would open a file path) |
| `<Space>gr` | no-op | retired; the label picker is `:group` |
| `<Space>R` | no-op | retired; the rating overlay is `:rate` |
| `gm` | no-op | retired merge-queue chord |
| `g?` | no-op | retired; the dialectic overlay is `:ask` |
| `gX` | no-op | retired modal launcher; the command moved under <Space>g |
| `gq` | no-op | retired modal launcher; the command moved under <Space>g |
| `gn` | no-op | retired modal launcher; the command moved under <Space>g |
| `gc` | no-op | retired modal launcher; the command moved under <Space>g |
| `gv` | no-op | retired modal launcher; the command moved under <Space>g |
| `gp` | no-op | retired modal launcher; the command moved under <Space>g |
| `gK` | no-op | retired modal launcher; the command moved under <Space>g |
| `gG` | no-op | retired modal launcher; the command moved under <Space>g |
| `gE` | no-op | retired modal launcher; the command moved under <Space>g |
| `gS` | no-op | retired modal launcher; the command moved under <Space>g |
| `gT` | no-op | retired modal launcher; the command moved under <Space>g |
| `gB` | no-op | retired modal launcher; the command moved under <Space>g |
<!-- rsi:generated:end -->

Normal mode is the default mode for navigation and actions.

### Session Management

| Key | Action | Description |
|-----|--------|-------------|
| `x` | InterruptSession | Send SIGINT to the focused session (exterminate) |
| `X` | QuickContinue | Send "continue" to idle session in detail view |
| `Enter` | EnterSession | Open the selected session in detail view |
| `p` | TogglePromptPreview | Preview full prompt of selected session (live updates with j/k) |
| `i` | EnterInputBarInsert(Insert) | Enter input bar insert mode at cursor |
| `a` | EnterInputBarInsert(Append) | Enter input bar insert mode after cursor |
| `o` | EnterInputBarInsert(OpenBelow) | Enter input bar, open line below |
| `O` | EnterInputBarInsert(OpenAbove) | Enter input bar, open line above |
| `e` | EnterSessionNormalMode | Enter session detail in normal mode (context-sensitive) |
| `F2` | RenameSession | Edit the title of the selected session |
| `F3` | OpenSessionInfoPanel | Open consolidated session info panel (id, provider/model, working dir, project, hierarchy, rating, label, tags, context usage; R/G jump to Rating/Label picker) |

### Session Lifecycle

| Key | Action | Description |
|-----|--------|-------------|
| `DD` | DeleteSession | Move the selected session to trash (logical delete) |
| `<Space>a` | ArchiveSession | Archive selected session (soft delete); on an active session, toggles auto-archive-on-completion |
| `U` | UnarchiveSession | Unarchive selected session in archive zone |
| `P` | TogglePinSession | Pin/unpin the selected session (pinned sessions stay at top) |
| `R` | RotateSession | Trigger context rotation on focused session |
| `<Space>t` | ToggleTestingNeeded | Toggle "manual testing needed" marker on selected session |
| `<Space>r` | ToggleRotationDisabled | Disable/enable auto context rotation for focused session |
| `<Space>k` | CancelRetry | Cancel a pending automatic retry on the focused session (↻N/M badge shows retry state) |

### Navigation - Attention Queue

| Key | Action | Description |
|-----|--------|-------------|
| `]a` | NextAttention | Jump to next session requiring attention |
| `[a` | PrevAttention | Jump to previous session requiring attention |

### Search

| Key | Action | Description |
|-----|--------|-------------|
| `/` | EnterSearch | Enter search mode (filter session list / search session detail) |
| `n` | NextSearchMatch | Jump to next search match (session detail) |
| `N` | PrevSearchMatch | Jump to previous search match (session detail) |

**Search mode keys** (while `/` search bar is active):

| Key | Action | Description |
|-----|--------|-------------|
| *any char* | Type | Append to search query (filters/searches live) |
| `Backspace` | Delete | Remove last character from query |
| `Enter` | Confirm | Keep filter/position active, return to normal mode |
| `Esc` | Cancel | Clear search, restore full list, return to normal mode |

In session list, search filters sessions by query text (case-insensitive). In session detail, search jumps to matching events. After confirming with `Enter`, pressing `Esc` in normal mode clears the search. Switching tabs/panes also clears the search.

### Navigation - Events (Detail View)

| Key | Action | Description |
|-----|--------|-------------|
| `Shift+Down` | NextEvent | Jump to the next event in the conversation |
| `Shift+Up` | PrevEvent | Jump to the previous event in the conversation |
| `]u` | NextUserMessage | Jump to next (newer) user message |
| `[u` | PrevUserMessage | Jump to previous (older) user message |
| `Down` | Scroll Down | Scroll session detail down 3 lines (viewport-aware: selection follows when leaving viewport) |
| `Up` | Scroll Up | Scroll session detail up 3 lines (viewport-aware: selection follows when leaving viewport) |
| Mouse wheel | Scroll | Scroll session detail content (selection stays fixed) |
| `Ctrl+Shift+→` | GrowSidebar | Grow session list sidebar by 3 percentage points (max 55%, matching the render clamp); activates percentage-based sidebar mode |
| `Ctrl+Shift+←` | ShrinkSidebar | Reduce session list sidebar width by 3 percentage points (min 20%); activates percentage-based sidebar mode |

### Navigation - Context-Sensitive

| Key | Action | Description |
|-----|--------|-------------|
| `gs` | GoToSessionsZone | Switch to sessions (main) zone from anywhere |
| `gt` | GoToTaskRabbitZone | Switch to TaskRabbit zone from anywhere |
| `gj` | GoToJobsZone | Switch to Jobs zone (scheduled job sessions) |
| `l` | NavigateRight | Enter session from list, OR next workspace from detail |
| `Left` / `Ctrl+Tab` | NavListUp | Navigate session list up (previous) from list or detail view (normal mode only) |
| `Right` / `Ctrl+Shift+Tab` | NavListDown | Navigate session list down (next) from list or detail view (normal mode only) |
| `Shift+Left` | NavListUp + Enter | Navigate to previous item and open it in session detail (Archive/TaskRabbit/Jobs/Main zones) |
| `Shift+Right` | NavListDown + Enter | Navigate to next item and open it in session detail (Archive/TaskRabbit/Jobs/Main zones) |
| `Enter` | EnterSession | Open selected session — works from both list and detail view |
| `Backspace` | AscendOrBack | Ascend one hierarchy container, or return to the session list/detail as applicable |

At the root of the main session list, daemon-appointed managers appear first in `MANAGERS` with a bold `M` in the ordinal column. `j` and `k` move across this section and the usual focus sections. Search and By Label retain their existing grouping.

Group and Epic rows with Starting/Running agents inside show a running-colored `◉`, which takes precedence over the container's own lifecycle icon and pending-archive/stalled color. Beside the title, running totals read `2 epics · 7 agents` or `7 agents` (singular: `1 epic`, `1 agent`); narrow rows use `2E·7A`, then `7A` when needed to keep the title visible.

### Session Detail Input Bar Priority

The session detail input bar gets first chance to handle keypresses before global normal-mode navigation. When the input bar is empty and in normal mode, keys pass through to the session/page bindings. When the input bar has draft text, normal mode becomes a vim text-editing surface for that draft, so vim textarea motions and edits such as `h`, `j`, `k`, `l`, `w`, `b`, `e`, `i`, `a`, `o`, `O`, and related operators are consumed by the input bar and do not reach global navigation.

Intentional pass-through exceptions:

| Key | Behavior |
|-----|----------|
| `Up` / `Down` | Always scroll the session detail transcript, even while editing the input bar |
| `Space` leader sequences | Pass through in input-bar normal mode so global leader chords still work |
| Unhandled normal-mode keys | Pass through when the input bar does not handle them |

Use `Esc` to leave input-bar insert mode; this does not blur the input bar if draft text remains. Submit with `Ctrl+Enter` or clear/send the draft to restore the fully transparent empty-input behavior.

### Model Selection

| Key | Action | Description |
|-----|--------|-------------|
| `Ctrl+M` | ToggleModelDropdown | Toggle model dropdown, anchored to the header row -- sets the default model for new sessions. Also works in Blank/TaskRabbit prompts for per-session model override. |

### Theme Selection

<!-- rsi:generated:begin themes -->
rsi ships 14 built-in themes, in picker order. `T` opens the picker with a live preview; `Esc` restores the theme that was active when it opened.

| # | Theme |
| --- | --- |
| 1 | Goth |
| 2 | Junk Yard |
| 3 | Transparent |
| 4 | Gruvbox Warm |
| 5 | High Contrast |
| 6 | Light |
| 7 | RAAAAINNNNNNBOOOZZZZZZZZ |
| 8 | Truly Transparent |
| 9 | Cup`a Joe |
| 10 | Emerald |
| 11 | Diamond |
| 12 | Ruby |
| 13 | Saphire |
| 14 | D. is for Devil |
<!-- rsi:generated:end -->

`Transparent` retains its dark readable
scrims; `Truly Transparent` yields passive surfaces to the terminal's default
background while keeping active rows, borders, and badges visible. `T` previews
immediately; `Esc` restores the theme that was active when the picker opened.

### Mouse Interactions

| Action | Description |
|--------|-------------|
| Left Click | Set event cursor in detail view (click on an event in the conversation) |
| Left Click on file path | Copy the resolved file path to clipboard (OSC 52) instead of just setting the cursor |
| Left Click on web link | Copy the URL to clipboard (OSC 52) instead of just setting the cursor — covers both `[text](url)` Markdown links and bare `http(s)://` URLs |
| Left Click on code block | Copy the code block content to clipboard (OSC 52) instead of just setting the cursor |
| Right Click on file path | Open the file in the built-in file viewer |
| Right Click on web link | Open the URL in the default browser (best-effort; a no-op if no opener command is available) |
| Scroll Up | Navigate up 3 lines |
| Scroll Down | Navigate down 3 lines |

### Project/Workspace Navigation

| Key | Action | Description |
|-----|--------|-------------|
| `<Space>p` | OpenProjectPicker | Toggle project picker (opens/focuses workspace) |

### Session Labeling

| Key | Action | Description |
|-----|--------|-------------|
| `]g` | NextLabelBoundary | Jump to next session with a different label |
| `[g` | PrevLabelBoundary | Jump to previous session with a different label |

### Session Hierarchy

Containers (`Group`, `Epic`) organize sessions into a project-tracker tree. Group ⊇ {Standard, Epic}; Epic ⊇ {Story, Task, Bug}; leaves hold no children. Containers never spawn provider subprocesses (status stays `Completed`).

#### Creation

| Key | Action | Description |
|-----|--------|-------------|
| `<Space>G` | CreateGroup | Open unified entity-creation modal with Kind pre-selected to `Group` |
| `<Space>E` | CreateEpic | Open unified entity-creation modal with Kind pre-selected to `Epic` |
| `<Space>gS` | CreateStory | Open unified entity-creation modal with Kind pre-selected to `Story` |
| `<Space>gT` | CreateTask | Open unified entity-creation modal with Kind pre-selected to `Task` |
| `<Space>B` | CreateBug | Open unified entity-creation modal with Kind pre-selected to `Bug` |

#### Unified entity-creation modal (Create entity form)

All five migrated chords (`<Space>G`, `<Space>E`, `<Space>gS`, `<Space>gT`,
`<Space>B`) open the same `CreateEntityForm` overlay
(shipped in P2.1, unified in P2.2, extended to V2 in P2.3) with the Kind
row pre-selected; the chord letter is a Kind shortcut, not a separate
overlay. Each chord runs a `legal_children` precheck against the current
descent head; illegal combinations surface as a toast and the overlay
does not open. The overlay re-runs the same check at open time and
surfaces rejection in its red banner as defense in depth.

**Field-visibility state machine (P2.3 §1.2):** the set of focusable
fields is Kind-driven, computed via
`overlay::create_entity_form::visibility::field_visibility(kind)`. Hidden
fields disappear entirely (no greyed rows). Tab/S-Tab cycle through the
current Kind's visible list. Toggling Kind reshuffles the list; if the
focused field is hidden by the new Kind, focus jumps to the first legal
field.

| Kind | Visible fields |
|------|----------------|
| `Group` | Kind, Name, Tag, Parent |
| `Epic` | Kind, Name, Tag, Parent, **Topology** |
| `Story` / `Task` / `Bug` / `Feature` / `Refactor` / `Research` / `Standard` | Kind, Name, Tag, Parent, **Provider**, **Model**, **Effort**, **Sandbox** |

**Field grammar (overlay owns all keyboard input when active):**

| Key | Field state | Effect |
|-----|-------------|--------|
| `Tab` | normal | Cycle focus forward through the Kind-specific visibility list |
| `S-Tab` | normal | Cycle focus backward |
| `n` | normal | Focus Name, enter insert mode |
| `k` | normal | Focus Kind (segmented selector — no insert needed) |
| `T` | normal | Focus Tag, enter insert mode |
| `t` | normal, Epic only | Focus Topology (Pattern 2 inline filter — no-op when hidden) |
| `Ctrl+p` / `Ctrl+Shift+p` | normal, leaf only | Cycle Provider forward / backward through 6 BUILTIN providers (Claude → Codex → Pioneer → Local → Gemini → Harness → wrap) |
| `m` | normal, leaf only | Open the model sub-overlay anchored on the form popup |
| `e` / `E` | normal, leaf only | Cycle Effort forward / backward through `None` and the selected model's ordered ladder (Opus 5 / Opus 4.7+ / Sonnet 5: `low`→`medium`→`high`→`xhigh`→`max`; Opus/Sonnet 4.6: `low`→`medium`→`high`→`max`; Codex GPT-5.6 Sol/Terra: `low`→`medium`→`high`→`xhigh`→`max`→`ultra`; GPT-5.6 Luna: through `max`; GPT-5.5/5.2: through `xhigh`) |
| `s` | normal, leaf only | Toggle Sandbox bool (off → on / on → off) |
| `gp` | normal | **Form-local intercept** — open the parent picker (overrides the global `<Space>gP` chord while the form is active) |
| `i` | normal, Name/Tag focused | Re-enter insert mode on the focused text field |
| `h` / `l` | Kind, normal | Move segment left/right (filtered to legal kinds for parent) |
| `<char>` | Topology, focused | Mutate the topology filter buffer; resets selected_index = 0 |
| `Enter` | Topology, focused | Commit highlighted topology to `topology_id`; clear filter; advance focus |
| `Esc` | Topology, focused | Clear `topology_id` (back to no-topology); clear filter; collapse preview; return focus to Kind |
| `Space` | Topology, focused | Toggle the ASCII DAG preview pane below the popup |
| `j` / `k` / `G` | Topology, focused | Navigate within the filtered dropdown |
| `<char>` | Name, insert | Append to name buffer |
| `Backspace` | Name, insert | Pop name char |
| `Esc` | Name/Tag, insert | Exit insert mode (overlay stays open) |
| `<char>` (not space/comma) | Tag, insert | Append to current pending chip |
| `Space` / `Tab` / `,` | Tag, insert | Commit current chip via `normalize_tag` |
| `Backspace` | Tag, insert, empty pending | Delete the rightmost committed chip |
| `Esc` | normal | Close overlay (draft auto-saved to DevState) |
| `<C-d>` | any | Explicit discard: clear draft, close overlay |
| `Enter` | normal | Submit |
| `<C-Enter>` | any | Submit unconditionally |

**Parent picker (`gp` chord):** opens a sub-overlay listing the legal
parent candidates for the form's current Kind. Esc closes only the
picker (form remains visible underneath); Enter writes the chosen
parent into the form's `parent_id` row and restores the form. The
form's `gp` intercepts the global prompt-creator action — outside
the form, `<Space>gP` opens the prompt creator.

**Model sub-overlay (`m` chord):** opens the reusable `ModelDropdown`
widget anchored on the form popup. Provider cycle inside the picker
re-discovers models on the fly. Enter writes the picked model into
`form.model` AND carries the dropdown's active provider into
`form.provider`. Esc closes the dropdown without mutating the form.

**Auto-draft (locked decision §B7):** Every keystroke that mutates the
overlay snapshots the current state to `DevState::create_entity_draft`.
P2.3 adds 5 new persistable fields (`topology_id`, `provider`, `model`,
`effort`, `sandbox`) — all serialized with `#[serde(default)]` so older
dev-state.json files load cleanly. The `topology_choices` cache is NOT
persisted (re-fetched on every overlay open via
`app.client.list_topologies(None)`). Esc closes the overlay but
preserves the draft for next open; only `<C-d>` or a successful submit
clears it.

**Mandatory tags (locked decision §B5):** Submit is rejected if the
committed tag set is empty or any chip is `Invalid` (failed
`normalize_tag`). Pending chips are auto-committed at submit time.

**Submit dispatch (P2.3 Phase 6):**

- **Containers** (`Group`, `Epic`) → `CreateContainer` RPC with
  `topology_id` threaded for `Epic`; `Group` defensively passes `None`
  even when `form.topology_id` is `Some(...)` (daemon also rejects
  topology on non-Epic).
- **Leaves** (`Story` / `Task` / `Bug` / `Feature` / `Refactor` /
  `Research` / `Standard`) → `LaunchSession` RPC with
  `provider` / `model` / `effort` / `sandbox` overrides threaded from
  the form; `workflow_id_override` stays `None` (per-leaf topology UI
  is reserved for Phase 4).

#### Navigation

| Key | Action | Description |
|-----|--------|-------------|
| `Enter` | EnterContainer (or open session) | When selected card is Group or Epic, descend into it; otherwise opens the session detail |
| `L` | EnterSession | Drill into container in-place / open leaf detail (mirrors `Enter`) |
| `H` | AscendContainer | Ascend one container level; no-op at root |
| `Backspace` | AscendOrBack | Ascend one level if descended; otherwise the existing BackToList behavior |
| `-` | AscendContainer | Unambiguous ascend chord (no fall-through) |
| `zo` | OpenFold | Open the selected card; a Group/Epic previews its children inline |
| `zc` | CloseFold | Close the selected card and hide its inline descendants |
| `za` | ToggleFold | Toggle the selected card/accordion |
| `zR` | OpenAllFolds | Open all cards and recursively preview `Group -> Epic -> Session` |
| `zM` | CloseAllFolds | Close every card and accordion |

Session-list folds are explicit: moving the cursor with `j`/`k` never opens a
card. Open a Group, move onto an inline Epic, and open that Epic to preview its
Sessions at the same list level. `Enter`/`L` remain navigation commands rather
than fold commands: they descend into a selected Group/Epic or open a leaf's
detail view.

#### Reassignment

| Key | Action | Description |
|-----|--------|-------------|
| `mp` | MoveToParent | Open parent picker — select a new legal parent for the focused session |
| `mo` | MoveToRoot | Move focused session to root (rejected by daemon if `legal_children(None)` doesn't accept the kind) |
| `gL` | SetEpicLead | Set focused leaf session as the lead of its parent Epic (no-op if parent is not an Epic) |
| `gR` | RunEpicTopology | Fire the focused Epic's bound topology with `parent_id = Epic.id`; spawned executor sessions appear in the Epic's session-list child view. No-op (with error toast) if focused session is not an Epic or has no `workflow_id` binding (P1.12) |

#### Lead assignment

| Key | Action | Description |
|-----|--------|-------------|
| `gL` | SetEpicLead | Promote the focused leaf session as the lead/orchestrator of its parent Epic. The lead's streamed `<docregblock>/spawn_child …</docregblock>` directives auto-spawn children under the same Epic. Mini-DAG marks the lead with `★`; the kind pill suffixes `★`. Also available as `:lead` / `:setlead` |

#### Epic topology execution

| Key | Action | Description |
|-----|--------|-------------|
| `gR` | RunEpicTopology | "go Run" — fire `ExecuteTopology` against the focused Epic's bound topology with `parent_id = Epic.id`. Spawned executor sessions write `parent_id = Epic.id` to the DB and show up under the Epic's session-list child view immediately. Three early-return toasts: "No session selected", "Not an Epic — gR only fires on Epic containers", "Epic has no topology binding — set one via `<Space>v` overlay first" (P1.12) |

#### Rebinds caused by hierarchy chords

- `<Space>gS` and `<Space>gT` are hierarchy creation chords.
- Trash browser is `<Space>gX`.

### Archive Zone

| Key | Action | Description |
|-----|--------|-------------|
| `ga` | GoToArchiveZone | Switch to the archive zone in the session list (loads archived sessions from daemon) |
| `U` | UnarchiveSession | Unarchive selected session in archive zone (restores to active list) |
| `<Space>gX` | GoToTrash | Open trash browser overlay — browse soft-deleted sessions |
| `gr` | OpenRecentCompletions | Toggle recent completions — jump to recently finished sessions |

### DocRegBlock Execution (Detail View)

| Key | Action | Description |
|-----|--------|-------------|
| `<Space>x` | ExecuteDocRegBlocks | Launch new sessions for all `<docregblock>` tags in current session |
| `<Space>X` | CommitAndPush | Continue session with `/ci_commit` to commit and push changes |

### Folding (Session List and Detail View)

| Key | Action | Description |
|-----|--------|-------------|
| `zo` | OpenFold | List: open selected card/accordion. Detail: expand the cursor event; a collapsed tool-call group expands as one unit |
| `zc` | CloseFold | List: close selected card/accordion. Detail: collapse the cursor event |
| `za` | ToggleFold | List: toggle selected card/accordion. Detail: toggle the cursor event/tool-call group |
| `zM` | CloseAllFolds | List: close every card/accordion. Detail: collapse all events |
| `zR` | OpenAllFolds | List: recursively open every card/accordion. Detail: expand all events |

### Yank

| Key | Action | Description |
|-----|--------|-------------|
| `yy` | CopySessionUuid | Session-list focus: copy the focused complete session UUID via OSC 52; toast: `session <uuid> copied` |
| `yy` | YankEventContent | Transcript focus: copy the content of the event at cursor to system clipboard (OSC 52, excludes header) |

Navigator symbols (one vocabulary shared by the list, inspector, and activity pane; the activity pane shows a dim key, defined in `crates/rsi/src/ui/glyphs.rs`):

- Pin and lifecycle: `∞` is the pin-column header and `◆` a pinned row; lifecycle is `◐` starting, `●` running, `?` waiting, `✓` completed, `×` failed, `■` interrupted, or `·` archived.
- Attention: `!` action required (waiting input/approval, failed), `↺` retry pending, `⧗` stalled, `•` unread output. `↻N` is rotation depth.
- Columns: `◔` context fill (`◌` = unknown, never `0%`), `⇄` turns, `▮` effort as a one-cell gauge scaled to the model's effort ladder (`▁`…`█`).
- Provider glyph before the model: `✻` Claude, `◎` Codex, `◉` Codex app-server, `⋈` OpenRouter, `⌂` Local, `△` Antigravity, `◇` Pioneer, `⌘` Harness. The list model is the canonical ID minus its vendor namespace (`claude-opus-5-5` → `opus-5-5`); the inspector shows the full ID.
- Roles: a leaf titled `Role: subject` shows a colored two-letter role code (`Mg` Manager, `Pl` Planner, `Rs` Researcher, `Iv` Investigator, `Im` Implementer, `Rf` Refactorer, `Db` Debugger, `Rv` Reviewer, `Vf` Verifier, …; unknown roles use their first two letters). Color is the role family: lead, plan, build, debug, check. Containers always show their full name.
- Inspector: `#` session ID, `⌂` working directory, `⊡` sandbox, `⎇` branch (`⊡ ⎇` when they share the session UUID), `+` created, `Δ` updated, `◷` work/run time, `→` current work or next action, `▎` quoted latest output.

### Visibility Toggles (Detail View)

| Key | Action | Description |
|-----|--------|-------------|
| `zs` | ToggleSystemEvents | Show/hide system events |
| `zt` | ToggleThinkingEvents | Expand/collapse thinking events |

### Leader Key and G-Chord Migration (`<Space>`)

This is the complete before/after inventory for Space chords and the Normal-mode
`g` sequences covered by #552. Unchanged Space slots are listed in both columns;
retained `g` actions keep their original sequence. Migrated `g` modal launchers
are retired at their former `g` sequence, with inert guards preventing fallback
to an unrelated single-key action. The form-local `gp` parent picker remains
unchanged.

| Before | After | Action / disposition |
|--------|-------|---------------------|
| `<Space>` | `<Space>` | Prefix only |
| `<Space><Space>` | `<Space><Space>` | OpenTelescope — fuzzy file picker |
| `<Space>;` | `<Space>;` | OpenCommandPalette — same palette as `:` |
| `<Space>,` | `<Space>,` | OpenSettings |
| `<Space>1` | `<Space>1` | JumpAttentionN(1) |
| `<Space>2` | `<Space>2` | JumpAttentionN(2) |
| `<Space>3` | `<Space>3` | JumpAttentionN(3) |
| `<Space>4` | `<Space>4` | JumpAttentionN(4) |
| `<Space>5` | `<Space>5` | JumpAttentionN(5) |
| `<Space>6` | `<Space>6` | JumpAttentionN(6) |
| `<Space>7` | `<Space>7` | JumpAttentionN(7) |
| `<Space>8` | `<Space>8` | JumpAttentionN(8) |
| `<Space>9` | `<Space>9` | JumpAttentionN(9) |
| `<Space>a` | `<Space>a` | ArchiveSession |
| `<Space>b` | `<Space>b` | OpenColorCustomizer |
| `<Space>c` | `<Space>c` | QuickContinue |
| `<Space>C` | `<Space>C` | ReassignSessionProject |
| `<Space>e` | `<Space>e` | ToggleFileExplorer |
| `<Space>g` | `<Space>g` | Prefix only; manager and Git subnamespace |
| `<Space>gg` | `<Space>gg` | ToggleGitPanel |
| `<Space>gb` | `<Space>gb` | OpenHarnessManagerBoard |
| `<Space>gd` | `<Space>gd` | OpenHarnessManagerDecisions |
| `<Space>gp` | `<Space>gp` | EditHarnessManagerPolicy |
| `<Space>gr` | `<Space>gr` | Reserved no-op |
| `<Space>i` | `<Space>i` | OpenIssuesWorkspace |
| `<Space>k` | `<Space>k` | CancelRetry |
| `<Space>K` | `<Space>K` | OpenScheduleBrowser |
| `<Space>m` | `<Space>m` | BlankPrompt |
| `<Space>M` | `<Space>M` | OpenMemorySearch |
| `<Space>o` | `<Space>o` | TaskRabbitPrompt |
| `<Space>p` | `<Space>p` | OpenProjectPicker |
| `<Space>q` | `<Space>q` | CloseFocusedPane |
| `<Space>r` | `<Space>r` | ToggleRotationDisabled |
| `<Space>R` | `<Space>R` | Reserved no-op |
| `<Space>s` | `<Space>s` | OpenSortPicker |
| `<Space>S` | `<Space>S` | EmergencyStopAll |
| `<Space>t` | `<Space>t` | ToggleTestingNeeded |
| `<Space>T` | `<Space>T` | OpenSessionInNewTab |
| `<Space>x` | `<Space>x` | ExecuteDocRegBlocks |
| `<Space>X` | `<Space>X` | CommitAndPush |
| `gX` | `<Space>gX` | GoToTrash; old `gX` is an inert guard |
| `gq` | `<Space>gq` | OpenQuestionModal; old `gq` is an inert guard |
| `gn` | `<Space>n` | ToggleNotifications; old `gn` is an inert guard |
| `gc` | `<Space>gc` | OpenEspSquare; old `gc` is an inert guard |
| `gv` | `<Space>v` | OpenGraphReview; old `gv` is an inert guard |
| `gp` | `<Space>gP` | OpenPromptCreator; old `gp` is an inert guard (form-local `gp` stays parent picker) |
| `gK` | `<Space>K` | OpenScheduleBrowser; old `gK` is an inert guard |
| `gG` | `<Space>G` | CreateGroup; old `gG` is an inert guard |
| `gE` | `<Space>E` | CreateEpic; old `gE` is an inert guard |
| `gS` | `<Space>gS` | CreateStory; old `gS` is an inert guard |
| `gT` | `<Space>gT` | CreateTask; old `gT` is an inert guard |
| `gB` | `<Space>B` | CreateBug; old `gB` is an inert guard |
| `gs` | `gs` | GoToSessionsZone — retained zone navigation |
| `ga` | `ga` | GoToArchiveZone — retained zone navigation |
| `gt` | `gt` | GoToTaskRabbitZone — retained zone navigation |
| `gj` | `gj` | GoToJobsZone — retained zone navigation |
| `gr` | `gr` | OpenRecentCompletions — retained navigation |
| `gL` | `gL` | SetEpicLead — retained mutation |
| `gR` | `gR` | RunEpicTopology — retained mutation |
| `gf1` | `gf1` | OpenRecentFileN(1) — retained navigation |
| `gf2` | `gf2` | OpenRecentFileN(2) — retained navigation |
| `gf3` | `gf3` | OpenRecentFileN(3) — retained navigation |
| `gf4` | `gf4` | OpenRecentFileN(4) — retained navigation |
| `gf5` | `gf5` | OpenRecentFileN(5) — retained navigation |
| `gf6` | `gf6` | OpenRecentFileN(6) — retained navigation |
| `gf7` | `gf7` | OpenRecentFileN(7) — retained navigation |
| `gf8` | `gf8` | OpenRecentFileN(8) — retained navigation |
| `gf9` | `gf9` | OpenRecentFileN(9) — retained navigation |
| `gg` | `gg` | Vim jump-top motion — retained |
| `gm` | `gm` | Retired no-op — retained |
| `g?` | `g?` | Retired no-op — retained |

The registered manager actions remain under `<Space>g` and contextual help (`?`)
continues to display their descriptors. `<Space>gr` remains reserved.

### Issues Workspace (`<Space>i` or `:issues`)

`<Space>i` focuses the first Issues leaf for the active project or replaces the
focused leaf with one. Issues is a normal workspace pane: split/focus/close,
tab-local state, serialization, and restart restoration use the ordinary pane
lifecycle. `[`/`]` switches Local, Dispatched, and Sync; `r` refreshes the
active tab; `?` opens context-filtered registry help without unmounting the
pane. A clean top-level `Esc`/`q` restores the replaced pane (or the default
session list after restart).

| Context | Key | Action |
|---------|-----|--------|
| Any Issues tab | `[` / `]` | Previous / next Issues tab |
| Any Issues tab | `r` | Refresh the active bounded view; on Sync this refreshes status only |
| Any Issues tab | `?` | Open contextual help; async responses continue applying underneath |
| Local table | `j` / `k` | Move by canonical Issue UUID; crossing an edge fetches one adjacent page and keeps selection in the bounded viewport |
| Local inspector | `j` / `k` | Scroll lower inspector fields and move the UUID-bound associated-session selection when present |
| Local inspector | `Tab` | Select Blocked by, Blocks, or Events as the bounded history section |
| Local inspector | `PageUp` / `PageDown` | Fetch one available previous/next dependency page or next event page |
| Local | `gg` / `G` | Fetch one first / last bounded page |
| Local | `/` | Open bounded filters focused on text search; applying resets to the first page |
| Local | `f` | Edit status/priority/readiness/assignee/unassigned/label/provenance/archive filters and Ready/Blocked/Mine/Recent saved views |
| Local | `s` | Cycle updated / display-number / priority sort |
| Local | `Enter` | Open the inspector, or jump to its UUID-selected associated session |
| Local | `n` | Create an Issue |
| Active unarchived Local | `e` / `S` / `p` / `a` / `l` | Edit content / status / priority / assignee / labels |
| Terminal unarchived Local | `S` | Open the registered reopen form with `Open` selected |
| Any selected Local Issue | `b` | Search a bounded dependency candidate page, choose Blocked by/Blocks, and submit add/remove to Store authority even for terminal/archived endpoints |
| Active unarchived Local | `dd` | Confirm status `Cancelled`; never delete or archive |
| Terminal unarchived Local | `A`, then `Enter` | Arm archive, then invoke the registered archive-confirmation action |
| Archived terminal Local | `U` | Restore without changing terminal status |
| Local | `yy` | Copy the complete canonical Issue UUID; toast `issue <uuid> copied` |
| Local | `y#` | Copy exactly `#N`; toast `issue #N copied` |
| Dispatched table | `j` / `k`, `Enter` | Select by session UUID plus tracker Issue ID, then open the durable dispatch inspector |
| Dispatched inspector | `j` / `k`, `Enter` | Scroll all dispatch fields, then open or recover the exact associated session UUID |
| Sync | `j` / `k` | Scroll the bounded status/manual-poll viewport |
| Sync | `P` | Run one manual tracker poll, show every ephemeral error, then independently refresh Sync and Dispatched |
| Form | `Tab` / `Shift-Tab` | Move between fields |
| Filter form | `Up` / `Down` | Choose saved view, readiness, or archive state; first Mine use captures a pane-local assignee |
| Dependency form | `Up` / `Down` | Choose edge direction or a bounded candidate |
| Status form | `Up` / `Down` | Choose the explicit lifecycle status, including reopen to Open |
| Filter/status/dependency form | `Enter` | Select the highlighted dropdown value or dependency candidate; does not save |
| Form | `Ctrl-Enter` | Save, or retry the identical frozen request envelope |
| Normal Issues pane with uncertain write | `Ctrl-Enter` | Retry the exact retained Cancel/Archive/Restore request and idempotency key |
| Frozen retry form | `Esc` / another form command | Keep the form and immutable request visible; neither close nor replace it |
| Stale form | `Ctrl-l` | Reload the latest projection; a dirty draft requires the same command twice consecutively |
| Stale form | `Ctrl-r` | Rebase the retained draft onto a validated newer matching projection and mint a new retry key |
| Dependency form | `Ctrl-d` | Submit exact edge removal rather than addition |
| Stale form | `Ctrl-r` / `Ctrl-l` | Rebase the draft / reload latest after a row-version conflict |
| Dirty form | `Esc`, `Esc` | First show `UNSAVED CHANGES`; second consecutive Esc discards |

Registry keys (generated):

<!-- rsi:generated:begin issue-tracker -->
Registry keys of the Issues workspace; some apply only in the tab or mode the action names.

| Keys | Action | What it does |
| --- | --- | --- |
| `Ctrl-Alt-G` | Contextual help | Opens contextual help listing the keys and commands available in the current view; Ctrl-Alt-G works even while typing. |
| `?` | Contextual help | Opens contextual help listing the keys and commands available in the current view; Ctrl-Alt-G works even while typing. |
| `j` | Move selection or scroll down | Moves the selection down one row, or scrolls the focused view down. |
| `k` | Move selection or scroll up | Moves the selection up one row, or scrolls the focused view up. |
| `Enter` | Inspect or open linked session | Inspects the selected issue, or opens its linked session when one exists. |
| `r` | Refresh active Issue view | Refreshes the active Issues tab from the daemon. |
| `P` | Run poll now | Runs the issue sync poll now instead of waiting for its schedule. |
| `Enter` | Select highlighted form value | Selects the highlighted value in the open issue form field. |
| `Ctrl-Enter` | Retry identical Issue write | Retries the last failed issue write with the identical request and idempotency key. |
| `Tab` | Select next inspector history section | Moves the issue inspector to its next history section. |
| `PageUp` | Load previous inspector page | Loads the previous page of the issue inspector history. |
| `PageDown` | Load next inspector page | Loads the next page of the issue inspector history. |
| `n` | New issue | Opens the editor to create a new issue. |
| `e` | Edit issue | Opens the editor on the selected issue. |
| `S` | Change lifecycle status | Changes the selected issue's lifecycle status. |
| `S` | Reopen terminal issue | Reopens a closed or cancelled issue. |
| `p` | Change priority | Edits the selected issue's priority. |
| `a` | Assign or unassign | Assigns or unassigns the selected issue. |
| `l` | Edit labels | Edits the selected issue's labels. |
| `b` | Edit dependencies | Edits the selected issue's blocked-by and blocks dependencies. |
| `Ctrl-L` | Reload latest Issue projection | Replaces a stale draft's base with the latest server version of the issue. |
| `Ctrl-R` | Rebase draft onto latest Issue | Rebases the stale draft's edits onto the latest server version of the issue. |
| `d` | Arm issue cancellation | First `d` of `dd`: arms cancellation of the selected issue. |
| `dd` | Cancel issue | Cancels the selected issue (`dd`). |
| `A` | Archive terminal issue | Archives the selected terminal (closed or cancelled) issue after confirmation. |
| `Enter` | Confirm archive | Confirms the pending issue archive. |
| `U` | Restore archived issue | Restores the selected archived issue. |
| `y` | Start issue copy command | First `y` of `yy` / `y#`: starts an issue copy command. |
| `yy` | Copy Issue UUID | Copies the selected issue's UUID (`yy`). |
| `y#` | Copy Issue number | Copies the selected issue's display number (`y#`). |
| `[` | Previous Issues tab | Switches to the previous Issues tab. |
| `]` | Next Issues tab | Switches to the next Issues tab. |
| `/` | Search local issues | Searches the locally loaded issues. |
| `f` | Edit filters and saved views | Edits the issue filters and saved views. |
| `s` | Cycle issue sort | Cycles the issue sort order. |
| `g` | Start first-page command | First `g` of `gg`: starts the first-page jump. |
| `gg` | Load first issue page | Loads the first issue page (`gg`). |
| `G` | Load last issue page | Loads the last issue page. |
| `q` | Close or cancel | Closes the current view or cancels the pending action. |
| `Esc` | Close or cancel | Closes the current view or cancels the pending action. |
<!-- rsi:generated:end -->

Selection, form targets, confirmations, and asynchronous reconciliation use the
canonical Issue UUID. Moving selection, changing tab/focus, another command,
Esc, or the two-second timeout clears an armed `dd`. Esc precedence is dirty
form warning/discard, confirmation, inspector, then pane restoration. The pane
renders no static action-hint strip; its help and executor routes come from the
shared action registry.

**Scheduled Jobs (`<Space>K`):** `j`/`k` moves, `n` creates, `Enter`/`e` edits, a single `Space` toggles enabled state, `t` triggers now, `r` refreshes, `dd` deletes, `?` opens contextual help, and `q`/`Esc` closes. Help preserves the selected row and an armed first `d`; closing it restores that exact pending state. Selected jobs retain the `> ` cursor and `[+]`/`[-]` enabled glyph. The overlay has no persistent action-hint row.

**Source Worktree Settlement:** open Settings with `<Space>,`, enter **Daemon
Features**, select **⚠ Source worktree settlement**, and press `Enter`. In the
overlay, `j`/`k` or `Down`/`Up` selects a daemon-discovered repository,
`Enter` runs a zero-write audit, `A` opens empty authorization input for a fresh
applyable audit, `r` refreshes the current durable receipt, `J`/`K` or
`PageDown`/`PageUp` scrolls by eight lines, and `q`/`Esc` closes. While
authorization input is active, character keys and `Backspace` edit the exact
phrase, `Enter` submits it, and `Esc` cancels without applying. See
[Source worktree cohort settlement](cohort-settlement.md) for the destructive
safety contract and recovery procedure.

### Quit

| Key | Action | Description |
|-----|--------|-------------|
| `ZQ` | Quit | Quit without saving (vim-style no-save quit) |
| `ZZ` | Quit | Save and quit (same as ZQ for rsi) |

### Standard Vim Motions (Inherited from modalkit)

| Key | Action | Description |
|-----|--------|-------------|
| `j` | Move Down | Navigate down in session list / settings (not session detail) |
| `k` | Move Up | Navigate up in session list / settings (not session detail) |
| `gg` | Jump to Top | Jump to first item/line |
| `G` | Jump to Bottom | Jump to last item/line |
| `Ctrl+d` | Scroll Down | Scroll down half a page |
| `Ctrl+u` | Scroll Up | Scroll up half a page |
| `Ctrl+f` | Page Down | Scroll down one full page |
| `Ctrl+b` | Page Up | Scroll up one full page |

### Workspace Management

| Key | Action | Description |
|-----|--------|-------------|
| `>` | Next Workspace | Switch to next project workspace |
| `<` | Previous Workspace | Switch to previous project workspace |

### Window/Split Management (Inherited from modalkit)

| Key | Action | Description |
|-----|--------|-------------|
| `Ctrl+h` | Prev Tab / Focus Left | Prev session list tab (when focused) / Move focus left (otherwise) |
| `Ctrl+l` | Next Tab / Focus Right | Next session list tab (when focused) / Move focus right (otherwise) |
| `Ctrl+Left` | Focus Left | Move focus to the pane to the left (zone cycling retired; use `gs`/`gt`/`gj`/`ga` to jump zones) |
| `Ctrl+Right` | Focus Right | Move focus to the pane to the right (zone cycling retired; use `gs`/`gt`/`gj`/`ga` to jump zones) |
| `Ctrl+w h` | Focus Left | Move focus to left pane |
| `Ctrl+w l` | Focus Right | Move focus to right pane |
| `Ctrl+w j` | Focus Down | Move focus to pane below |
| `Ctrl+w k` | Focus Up | Move focus to pane above |

---

## Command Mode

Press `:` or `<Space>;` from Normal mode to open the command palette. Type to
filter commands, use Up/Down or Ctrl-N/Ctrl-P to select, Enter to run, and Esc
to cancel. Exact command spellings with arguments (for example, `:model foo`)
run directly. Tab edits arguments for a selected command; commands that require
arguments open that editor on Enter.

### rsi-Specific Commands

<!-- rsi:generated:begin command-mode -->
`[arg]` marks an optional argument and `<arg>` a required one. Window and tab commands are handled by the pane's own window commands.

| Command | Aliases | Action | What it does |
| --- | --- | --- | --- |
| `:help` | `:keys` | Contextual help | Opens contextual help listing the keys and commands available in the current view; Ctrl-Alt-G works even while typing. |
| `:commands` |  | All commands | Opens the complete action, overlay, raw-key and file-viewer reference. |
| `:manual [arg]` | `:man [arg]` | Open manual | Opens the generated manual in a browser, pager or PDF viewer. |
| `:refresh` |  | Refresh navigation | Re-fetches sessions, projects and labels from the daemon. |
| `:theme [arg]` |  | Choose built-in theme | Opens the built-in theme picker; `:theme <name>` applies a theme directly. |
| `:issues` |  | Open or focus Issues workspace | Opens the Issues workspace pane for the current project, or focuses it if already open. |
| `:manager policy` |  | Edit manager policy | Opens the harness manager policy editor (operator-owned manager limits and choices). |
| `:manager board` |  | Open manager board | Opens the harness manager work board. |
| `:manager decisions` |  | Open manager decisions | Opens the harness manager decisions queue awaiting the operator. |
| `:manager inbox` |  | Open manager inbox | Opens the harness manager inbox of lead requests and replies. |
| `:manager inspect` |  | Open manager inspect | Opens the harness manager inspect view (workers, work, requests, topology and events). |
| `:kill` | `:ki` | Interrupt running session | Interrupts the selected running session. |
| `:continue [arg]` | `:cont [arg]` | Continue selected session | Continues the selected session: opens a continue prompt, or `:continue <text>` sends the text directly. |
| `:archive` | `:arc` | Archive selected session | Archives the selected session (reversible with U from the Archive zone). |
| `:delete` | `:del` | Delete selected session | Deletes the selected session after the double-tap `DD` (moves it to the trash). |
| `:rotate` | `:rot` | Rotate session context | Rotates the selected session into a fresh context, carrying a handoff forward. |
| `:archives` |  | Open archives | Switches the session list to the Archive zone. |
| `:model [arg]` | `:mod [arg]` | Choose model | Opens the model picker, or `:model <name>` selects a model for new launches. |
| `:sessions` | `:ls` | List sessions | Lists sessions in the session list. |
| `:alerts` | `:att` `:attention` | Show attention alerts | Toggles the notification and attention history overlay. |
| `:quit` | `:q` `:qall` `:qa` `:quit!` `:q!` | Quit | Quits rsi (the daemon and its sessions keep running). |
| `:projects` |  | Choose project | Opens the project picker. |
| `:project [arg]` |  | Switch project | Switches to the named project, or opens the picker without a name. |
| `:task [arg]` | `:ta [arg]` | Launch task session | Opens the TaskRabbit one-shot prompt; `:task <text>` launches it directly. |
| `:blank [arg]` | `:bl [arg]` | Launch blank session | Opens the blank general-purpose session prompt; `:blank <text>` launches it directly. |
| `:project-new [arg]` |  | Create project | Creates a project: `:project-new <name> [path]`, or opens the form. |
| `:project-edit [arg]` |  | Edit project | Edits the named or current project. |
| `:project-delete <arg>` |  | Delete project | Deletes the named project; the name argument is required. |
| `:set` | `:settings` | Open settings | Opens the settings pane. |
| `:stopall` | `:stop-all` | Emergency stop all | Emergency stop: denies new paid work and cancels live model invocations. |
| `:hooks` |  | Open hook settings | Opens settings at the Claude hooks list. |
| `:skills` |  | Open skill settings | Opens settings at the Claude skills list. |
| `:group` | `:groups` | Choose group label | Opens the group label picker for the selected session. |
| `:diagnostics` | `:diag` | Open diagnostics | Opens the diagnostics overlay. |
| `:graph` |  | Open graph review | Opens the visual workflow graph review editor. |
| `:topology-resolve [arg]` |  | Resolve preserved topology work | Inspects, accepts, retries or discards preserved work on a blocked durable topology execution: `:topology-resolve [<execution_id>] inspect\|accept\|retry\|discard [<commit>]` (the id may be omitted when exactly one is blocked; discard needs the full preserved commit). |
| `:dag` |  | Open recursive DAG | Opens the recursive DAG browser. |
| `:context [arg]` | `:ctx [arg]` | Set active task context | Sets or clears the active task context for new launches. |
| `:card [arg]` |  | Edit entity card | Opens the entity card editor for the current project or the named entity. |
| `:ask [arg]` |  | Ask a question | Opens the dialectic question overlay; `:ask <question>` asks directly. |
| `:term` | `:terminal` `:shell` | Toggle terminal | Toggles the embedded terminal overlay (the shell keeps running when hidden). |
| `:lead` | `:setlead` | Set Epic lead | Sets the focused leaf session as the lead of its parent Epic. |
| `:manager` |  | Open harness manager | Opens the harness manager overlay. |
| `:manager appoint` |  | Appoint harness manager | Appoints the selected session as the harness manager. |
| `:manager scope` |  | Edit manager scope | Edits the harness manager's scope. |
| `:manager clear` |  | Clear manager scope | Clears the harness manager's scope. |
| `:rate [arg]` | `:r [arg]` | Rate selected session | Rates the selected session 1-10: `:rate <n>`, or opens the rating overlay (digits 1-9, 0 = 10). |
| `:split` | `:sp` | Split pane horizontally | Splits the focused pane horizontally (handled by the pane's window commands). |
| `:vsplit` | `:vs` | Split pane vertically | Splits the focused pane vertically (handled by the pane's window commands). |
| `:close` | `:clo` | Close pane | Closes the focused pane. |
| `:only` | `:on` | Close other panes | Closes every pane except the focused one (handled by the pane's window commands). |
| `:tabnew` | `:tabe` | Create tab | Opens a new tab (handled by the pane's window commands). |
| `:tabclose` | `:tabc` | Close tab | Closes the current tab (handled by the pane's window commands). |
| `:tabnext` | `:tabn` | Next tab | Switches to the next tab. |
| `:tabprev` | `:tabp` `:tabprevious` | Previous tab | Switches to the previous tab. |
<!-- rsi:generated:end -->

Inside the unified manager surface, `1`–`5` select Board, Decisions, Inbox,
Inspect, and Policy. `Tab` / `Shift-Tab` move between those sections;
`[` / `]` move between Inspect subsections. Policy drafts stay loaded while
switching sections and are saved with `s`. The Normal-mode launchers
`<Space>gp`, `<Space>gb`, and `<Space>gd` open Policy, Board, and Decisions
respectively; `:manager inbox` and `:manager inspect` open their unified sections.
| `:context [text]` | `:ctx [text]` | Set active task context for focused session (max 500 chars); with no argument, clear it |
| `:card [user] [add <fact>]` | | Open project or user card editor, or add a fact without opening the editor |
| `:term` | `:terminal`, `:shell` | Toggle embedded terminal overlay |

### Vim Commands (Pass-Through)

These standard vim commands are handled by modalkit:

`:split`, `:vsplit`, `:close`, `:only`, `:tabnew`, `:tabclose`, `:tabnext` and `:tabprev` (and their aliases) are listed in the generated command table above.

---

## Overlay Keybindings

### Harness Manager Scope (`:manager appoint`, `:manager scope`)

New appointments default to the focused manager's whole project, including future
Epics. Space narrows scope to selected Groups/Epics; `a` restores whole-project
coverage. Select up to 32 Groups plus 32 individual Epics. Group membership follows
current topology and includes future Epics; empty Groups are selectable. Reopening
the same appointment preserves its selection. Each row retains its own name and
short ID. `[x]` is explicit selection; `[+]` is inherited coverage. Unavailable
selected rows remain visible for removal.

| Key | Action |
|-----|--------|
| `j` / `k`, `↓` / `↑` | Move between Groups/Epics |
| `g` / `G` | First / last matching row |
| `Space` | Toggle Group/Epic selection; deselect Group before choosing individual children |
| `a` | Select whole current project, including future Epics |
| `/` | Filter by name, parent Group name, or ID; Enter/Esc finishes filtering |
| `Enter` | Save scope; empty explicit selection revokes supervision |
| `Esc` / `q` | Cancel without saving |

Filtering preserves selections outside the current results. Save errors leave the
draft open. On a stale revision, cancel and reopen the command to review the latest
scope before saving again. `:manager` opens the existing conversation; it does not
start or continue the session. A daemon without manager support reports an update
and restart instruction.

### Harness Manager Policy (`:manager policy`)

Appoint a Standard root manager with `:projects`, `:blank`, then `:manager appoint`.
V2 policy is a separate opt-in. Observe, Execute, Full project control and Custom
prepare a draft for one save. Saved and draft summaries retain explicit limits,
including zero; Use suggested allowances edits only the named conflicting zeros.
Advanced retains every field and exact legacy restriction. Empty allowed models
means any valid choice, including future models. Catalog choices use exact IDs;
default effort means no explicit effort. Distinct TUI-only custom endpoints are
identified as unsupported for endpoint-specific manager restrictions; Local uses
the daemon-configured catalog. The form retains manager/project identity and edits
mode, operator/Epic pauses, capabilities, Group/root grants, creation quotas,
concurrency/provider ceilings, allowed provider/model/effort choices, retry count,
delay, request timeout and spend cap. Selected IDs outside the session cache remain
editable. See [manager setup and bounds](harness-manager.md).

| Key | Action |
| --- | --- |
| `j` / `k`, arrows, Tab / Shift-Tab | Move between fields |
| `g` / `G` | First / last field |
| Enter / Space | Apply a preset, expand Preview/Advanced, apply named suggestions, or edit a field; launch rows open the catalog |
| Ctrl-U / Backspace while editing | Clear / delete text; blank removes optional limits or effort |
| Enter / Esc while editing | Apply / cancel that scalar edit |
| `s` | Save the complete policy with observed scope/policy versions |
| `r` | Discard draft and reload current appointment/policy; errors otherwise retain draft and retry key |
| Esc / `q` | Close the form |

In the launch picker, Tab/Shift-Tab cycles all seven built-in providers, j/k or
arrows select a model, Enter chooses it and then confirms an effort. Backspace
returns from effort to models; Esc cancels the entire candidate, and r refreshes.
Catalog errors, empty results and unavailable retained models keep the exact draft.
Only s in the policy form saves. A revoked saved grant is labeled explicitly;
s then saves and regrants the displayed draft under the current appointment.

Save errors remain visible and preserve the draft. Unchanged retries retain the
idempotency key. Scope changes fence older policy. Reopening a revoked grant
retains its saved permissions and limits; only an appointment with no saved policy
starts from Status defaults. Opening the form grants no authority.

### Harness Manager Board / Decisions

| Key | Action |
| --- | --- |
| `1`–`5` | Jump to Board / Decisions / Inbox / Inspect / Policy |
| Tab / Shift-Tab | Next / previous section in that order |
| `[` / `]` in Inspect | Previous / next Inspect subsection: Workers, Work, Topology, Resources, Actions, Events |
| `o` | Open the selected row's session |
| `j` / `k`, arrows, `g` / `G` | Select row / first / last row; on the Board, `j` / `k` cross band boundaries |
| Enter on the Board | Open the selected row's full section (DECISIONS → Decisions, REQUESTS → Inbox, LEADS / SIGNALS → Inspect Overview, HEALTH → Inspect Health) with that row selected |
| `a` on a Board DECISIONS row | Answer in place with that row's exact digest/version and its Decisions page fences |
| `n` / `p` | Next / previous cursor page, including empty pages with a continuation cursor (full sections; the Board shows first pages) |
| Page Up / Page Down | Scroll selected record details |
| `r` | Refresh from page one (the Board reloads its four first pages); a stale error retains the current page |
| `a` / Enter in Decisions | Edit an answer for the selected pending record's exact digest/version |
| Ctrl-U / Backspace while answering | Clear / delete answer text |
| Enter / Esc while answering | Submit that exact answer / leave the target pending |
| Esc / `q` | Close the board |

The Board header shows manager identity, project, scope mode and Epic count,
policy mode, observation time and coverage across its four first pages
(Overview, Decisions, Requests, Health). The KPI line reports program and product
accepted/integrated/denominator, ready, partial and unknown counts. A band with
more rows than it shows, or whose first page is empty with a continuation cursor,
ends with a selectable `+more · Enter here opens full <section>` line (DECISIONS
also `2`, REQUESTS also `3`). If a daemon sorts program/product KPI rows past
Overview's first page, the Board reads at most 4 Overview pages to find them and
otherwise says the KPIs are beyond that bound.
Section headers show observation time and page coverage. Detail fields preserve
reported, source-accepted and integrated progress as separate facts. Null evidence
renders as unknown. Stale decision errors retain the draft; Esc then `r` refreshes
before selecting a new target. A submission receipt does not declare gate delivery
or completion; refresh to observe those results.

### Source-Worktree Settlement Authorization

Apply in the source-worktree settlement overlay (Settings ▸ Sandbox Storage ▸ Source-worktree settlement) first requires a fresh, applyable audit of the selected cohort. It then opens an authorization line that must receive the exact phrase shown by that audit; nothing is prefilled.

| Key | Action |
|-----|--------|
| Printable characters | Append to the authorization phrase (up to 8192 characters) |
| `Backspace` | Delete the last character |
| `Enter` | Apply the settlement, only if the typed phrase matches the audited phrase exactly |
| `Esc` | Leave authorization and clear the typed phrase |

### Terminal Overlay (Embedded Shell)

Toggled with `Ctrl+\` or `:term` / `:shell` commands. Shell persists when overlay is hidden.

#### Insert Mode (default when opened)

| Key | Action | Description |
|-----|--------|-------------|
| `Ctrl+\` | Close | Hide terminal overlay (shell keeps running) |
| `Esc` | Normal Mode | Enter terminal normal mode for scrollback navigation |
| All other keys | PTY passthrough | Sent directly to the shell |

#### Normal Mode

| Key | Action | Description |
|-----|--------|-------------|
| `i`, `a` | Insert Mode | Return to terminal insert mode |
| `j`, `Down` | Scroll Down | Scroll toward bottom of output |
| `k`, `Up` | Scroll Up | Scroll toward top of output |
| `G` | Bottom | Jump to bottom (current output) |
| `g` | Top | Jump to top of scrollback |
| `Ctrl+d` | Half Page Down | Scroll half page toward bottom |
| `Ctrl+u` | Half Page Up | Scroll half page toward top |

---

### Prompt Overlay (New Session / Continue Session)

Clipboard paste works from either mode and switches to insert mode automatically if needed.

#### Insert Mode (default when opened)

| Key | Action | Description |
|-----|--------|-------------|
| `Esc` | Exit Insert | Switch to normal mode (or dismiss suggestions) |
| `Enter` | Newline | Insert newline in multi-line input |
| `Ctrl+Enter` | Submit | Submit prompt and close overlay |
| `Shift+Enter` | Newline | Insert newline in multi-line input |
| `Ctrl+V` | Paste | Paste from system clipboard (image saves to `~/.rsi/paste/` and inserts `@path` reference; text inserts directly) |
| `Ctrl+T` | Submit + New Tab | Submit prompt and open session in new tab |
| `Ctrl+S` | Submit + Split | Submit prompt and open session in new vertical split |
| `Ctrl+Shift+A` | Open AI Chat | Ask the configured prompt processor about non-empty prompt text |
| Text keys | Type | Insert text into textarea |
| `Left` / `Right` | Move Cursor | Move cursor left/right (always available) |
| `Up` / `Down` | No-op / Suggestions | Navigate suggestions when visible; otherwise silently consumed (no cursor movement, no scroll) |

> **Submit-on-Enter setting** — the setting controls submit-capable input surfaces such as the session input bar, generic input modals, and the question panel. Session prompt overlays require `Ctrl+Enter` to submit. File and prompt editors always insert a newline with plain `Enter`.

**Suggestion Navigation** (when suggestions visible):

| Key | Action | Description |
|-----|--------|-------------|
| `Tab` | Accept | Accept selected suggestion |
| `Down` / `Ctrl+n` | Next | Move to next suggestion |
| `Up` / `Ctrl+p` | Previous | Move to previous suggestion |
| `Esc` | Dismiss | Hide suggestions (stay in insert mode) |

#### Normal Mode

Overlay normal mode supports the full vim text editing feature set documented in [Vim Text Editing (Input Bar & Overlay)](#vim-text-editing-input-bar--overlay): motions, counts, operators, text objects, visual mode, dot repeat, and f/t/F/T character search.

**Overlay-specific keys** (in addition to the shared vim features):

| Key | Action | Description |
|-----|--------|-------------|
| `Ctrl+Q` | Close | Close overlay without submitting (normal or insert mode) |
| `Ctrl+E` | Cycle Effort | Cycle effort level (Blank/TaskRabbit only; `None` starts at the selected model default, then follows the ordered ladder: Opus 5 / Opus 4.7+ / Sonnet 5 `low`→`medium`→`high`→`xhigh`→`max`; Opus/Sonnet 4.6 `low`→`medium`→`high`→`max`; Codex GPT-5.6 Sol/Terra `low`→`medium`→`high`→`xhigh`→`max`→`ultra`; GPT-5.6 Luna through `max`; GPT-5.5/5.2 through `xhigh`) |
| `Ctrl+M` | Model Selector | Open model selector (Blank/TaskRabbit only) — selection sets per-modal override |
| `Ctrl+B` | Toggle Sandbox | Toggle sandbox (git-worktree isolation) for this launch (Blank/TaskRabbit only; capability-gated — no-op against daemons without sandbox support) |

#### Modal Geometry (Any Mode)

These keys work when any Prompt overlay (Blank, TaskRabbit, ContinueSession) or the `Ctrl+G` input modal is active, in both insert and normal mode.

| Key | Action | Description |
|-----|--------|-------------|
| `Ctrl+Shift+Right` | Resize Right | Extend modal right edge (wider) |
| `Ctrl+Shift+Left` | Resize Left | Shrink modal from left edge |
| `Ctrl+Shift+Down` | Resize Down | Extend modal bottom edge (taller) |
| `Ctrl+Shift+Up` | Resize Up | Shrink modal from top edge |
| `Ctrl+Right` | Move Right | Move modal right |
| `Ctrl+Left` | Move Left | Move modal left |
| `Ctrl+Down` | Move Down | Move modal down |
| `Ctrl+Up` | Move Up | Move modal up |
| `Ctrl+0` | Reset Geometry | Reset modal position/size to defaults |

Geometry changes persist across sessions, keyed by modal purpose (Blank, TaskRabbit, ContinueSession, InputModal). Terminal resize recomputes the base rect; deltas apply on top.

### Question Panel

Claude sessions can raise structured questions through `AskUserQuestion`. The panel opens for sessions with a pending question; auto-open is opt-in under Settings → Display.

| Key | Mode | Description |
|-----|------|-------------|
| `<Space>gq` | normal | Open the pending question panel |
| `j` / `k` | panel normal | Move the highlighted option |
| `Space` | panel normal | Toggle the highlighted option for multi-select questions |
| `1`-`9` | panel normal | Select or toggle option N |
| `i` | panel normal | Enter free-text mode and clear option selections for the current question |
| `Enter` | panel normal | Advance to the next question |
| `Backspace` | panel normal | Return to the previous question |
| `Ctrl+Enter` | any panel mode | Submit the answer |
| `Enter` | panel insert | Submit when `submit_on_enter` is enabled; otherwise insert a newline |
| `d` | panel normal | Decline with no preference and let the agent use best judgment |
| `Esc` / `q` | panel normal | Dismiss without answering; the question stays pending |
| `Esc` | panel insert | Return to panel normal mode |

#### Input Overlay Stack Navigation

When multiple Blank/TaskRabbit input overlays are open simultaneously, use these keys to navigate focus between them. The focused overlay receives all key input and has a bright border; unfocused overlays have a dimmed border.

| Key | Action | Description |
|-----|--------|-------------|
| `Ctrl+J` | Focus Down | Move focus to the next (newer) input overlay |
| `Ctrl+K` | Focus Up | Move focus to the previous (older) input overlay |
| `Space+o` | Open TaskRabbit | Open a new TaskRabbit overlay (always opens, stacks below existing) |
| `Space+m` | Open Blank | Open a new Blank session overlay (always opens, stacks below existing) |

These keys work in both insert and normal mode within any input overlay. `Space+o` and `Space+m` require normal mode. Overlays stack vertically with the oldest at the top and the newest at the bottom. When more overlays exist than fit on screen, the view scrolls to keep the focused overlay visible.

`Space+o` and `Space+m` also work while non-text overlays without their own Space binding are active (e.g. GraphReview, ThemePicker, SortPicker, KeybindingsHelp). Pressing `Space` consumes the keypress as a leader; the subsequent `o` or `m` dismisses the current overlay and opens the session modal. Scheduled Jobs uses a single Space to toggle the selected job, and File Explorer uses it to close the tree or type in the finder. This bypass is not active in the Settings pane when items are focused (Space there toggles the selected item).

### Model Dropdown (Widget)

The model dropdown is a reusable inline widget anchored below the header row (or below the model badge in Blank/TaskRabbit prompts). It is NOT an overlay — it intercepts keys when open.

| Key | Action | Description |
|-----|--------|-------------|
| `j` / `Down` | Move Down | Navigate to next model |
| `k` / `Up` | Move Up | Navigate to previous model |
| `g` | Go to Top | Jump to first model |
| `G` | Go to Bottom | Jump to last model |
| `Tab` | Cycle Provider Forward | Cycle provider forward (Claude → Codex → Pioneer → Local → Gemini → Harness → custom providers) |
| `Shift+Tab` / `BackTab` | Cycle Provider Backward | Cycle provider in reverse order |
| `Enter` | Select | Select model and close dropdown |
| `1`-`9` | Direct Select | Select model by number (1-indexed) |
| `Esc` / `q` | Cancel | Close dropdown without changing model |

### Theme Picker Overlay

| Key | Action | Description |
|-----|--------|-------------|
| `j` / `Down` | Move Down | Navigate to next theme |
| `k` / `Up` | Move Up | Navigate to previous theme |
| `Enter` | Apply | Apply selected theme and close |
| `1`-`9` | Direct Select | Select the first nine themes directly (through ``Cup`a Joe``); use navigation for Emerald, Diamond, Ruby, and Saphire |
| `Esc` / `q` | Cancel | Close without changing theme |

### Project Picker Overlay (Workspace Opener)

| Key | Action | Description |
|-----|--------|-------------|
| `j` / `Down` | Move Down | Navigate to next project |
| `k` / `Up` | Move Up | Navigate to previous project |
| `Enter` | Open Workspace | Open/focus workspace for selected project |
| `Ctrl+n` | New | Open create project form |
| `Ctrl+e` | Edit | Open edit form for highlighted project |
| `Ctrl+d` | Delete | Delete highlighted project |
| `Esc` | Cancel | Close picker |
| Text keys | Filter | Type to filter projects by name |
| `Backspace` | Delete Char | Remove last character from filter |

### Project Form Overlay (Create/Edit)

| Key | Action | Description |
|-----|--------|-------------|
| `Tab` | Next Field | Cycle to next field (Name → Path → Color) |
| `Shift+Tab` | Prev Field | Cycle to previous field |
| `Enter` | Save | Save project and return to picker |
| `Esc` | Cancel | Return to picker without saving |
| Text keys | Type | Insert text in Name/Path fields |
| `Backspace` | Delete Char | Remove last character from Name/Path |
| `Left` / `Right` | Cycle Color | Cycle through color palette (Color field) |

### Card Editor Overlay (`:card` / `:card user`)

Entity card fact editor for project or user cards. Facts are injected into session context at launch.

#### Navigation Mode (default)

| Key | Action | Description |
|-----|--------|-------------|
| `j` / `Down` | Move Down | Navigate to next fact |
| `k` / `Up` | Move Up | Navigate to previous fact |
| `g` | Jump to Top | Jump to first fact |
| `G` | Jump to Bottom | Jump to last fact |
| `a` | Add Fact | Add new fact at end of list (enters edit mode) |
| `e` / `i` | Edit Fact | Edit the selected fact inline (enters edit mode) |
| `dd` | Delete Fact | Delete the selected fact (vim-style chord) |
| `J` (Shift+j) | Move Down | Move selected fact down (reorder) |
| `K` (Shift+k) | Move Up | Move selected fact up (reorder) |
| `Ctrl+s` | Save | Save card to daemon without closing |
| `Esc` / `q` | Save + Close | Save card and close overlay |

#### Edit Mode (editing a fact inline)

| Key | Action | Description |
|-----|--------|-------------|
| Text keys | Type | Insert text |
| `Backspace` | Delete Char | Remove last character |
| `Enter` | Confirm | Save edit (empty text deletes the fact) |
| `Esc` | Cancel | Cancel edit (newly added empty fact is removed) |

### Sort Picker Overlay (`<Space>s`)

| Key | Action | Description |
|-----|--------|-------------|
| `j` / `Down` | Move Down | Navigate to next sort option |
| `k` / `Up` | Move Up | Navigate to previous sort option |
| `Enter` | Apply | Apply selected sort order |
| `Esc` / `q` | Cancel | Close without changing sort |

### Prompt Preview Overlay

| Key | Action | Description |
|-----|--------|-------------|
| `j` / `Down` | Navigate Down | Move to next session (popup updates) |
| `k` / `Up` | Navigate Up | Move to previous session (popup updates) |
| `G` | Jump to Bottom | Jump to last session |
| `Ctrl+d` | Scroll Down | Scroll popup content down |
| `Ctrl+u` | Scroll Up | Scroll popup content up |
| `Enter` | Open Session | Close preview and enter session detail |
| `p` / `Esc` / `q` | Close | Close preview overlay |

### Archive Zone (Session List)

Activated with `ga`. Browse and restore archived sessions as a zone tab in the session list.

| Key | Action | Description |
|-----|--------|-------------|
| `j` / `k` | Navigate | Move through archived sessions (standard list navigation) |
| `gg` / `G` | Jump | Jump to first/last archived session |
| `Enter` / `l` | Open | Open selected archived session in detail view |
| `U` | Unarchive | Restore selected session (moves back to active list) |
| `gs` / `gt` / `gj` / `ga` | Zone Jump | Switch directly to Sessions / TaskRabbit / Jobs / Archive |

### File Explorer Overlay (`<Space>e`)

Left-anchored file tree drawer rooted at the focused session's working directory.

| Key | Action | Description |
|-----|--------|-------------|
| `j` / `Down` | Move Down | Navigate to next entry |
| `k` / `Up` | Move Up | Navigate to previous entry |
| `g` | Jump Top | Jump to first entry |
| `G` | Jump Bottom | Jump to last entry |
| `Enter` / `l` | Open/Expand | Open file in viewer, or expand/collapse directory |
| `h` | Parent | Jump to parent directory |
| `yy` | Copy Path | Copy full file path to clipboard |
| `yn` | Copy Name | Copy filename only to clipboard |
| `dd` | Delete | Delete file (to trash buffer) |
| `u` | Undo | Restore last deleted file |
| `/` | Find | Activate fuzzy file finder (type to search, including `q` and Space; j/k nav, Enter open, `Esc` returns to the tree) |
| `.` | Toggle Hidden | Show/hide dotfiles |
| `Esc` / `q` | Close | Close file explorer |
| `Space` / `Space Space` | Close | Close file explorer from the tree; the first Space closes it |

### Telescope File Picker (`<Space><Space>`)

Fuzzy file finder rooted at the focused session's working directory.

| Key | Action | Description |
|-----|--------|-------------|
| *any char* | Filter | Fuzzy search files by path |
| `Backspace` | Delete | Remove last character from query |
| `j` / `Down` | Move Down | Navigate to next result |
| `k` / `Up` | Move Up | Navigate to previous result |
| `Enter` | Open | Open selected file in file viewer |
| `Esc` | Close | Close telescope |

### File Viewer (opened from File Explorer)

The file viewer uses the shared vim text surface for source navigation and limited edits. The v1 contract is viewer-focused: open files, render highlighted/markdown content, and scroll reliably; full Neovim editing parity is deferred.

**Navigation & Editing:**

| Key | Action | Description |
|-----|--------|-------------|
| `Space+q` | Close | Close file viewer (cached for undo/redo on reopen) |
| `Space+m` | Markdown Preview | Toggle rendered markdown preview (`.md` files only) |
| `PageDown` / `Ctrl+f` | Page Down | Scroll down one viewer page |
| `PageUp` / `Ctrl+b` | Page Up | Scroll up one viewer page |
| `Ctrl+d` / `Ctrl+u` | Half Page | Scroll down/up half a viewer page |
| `Backspace` | Back to List | Toggle focus to session list clone (file viewer stays open) |
| `Ctrl+S` | Save | Save file to disk |
| `Enter` | Newline | Insert a line break in insert mode, regardless of `submit_on_enter` |
| `q` | Close | Close file viewer (normal mode) |

**Command Mode (`:`):**

Enter command mode by pressing `:` in normal mode. The command prompt appears at the bottom of the file viewer.

<!-- rsi:generated:begin file-viewer -->
The file viewer's own `:` command line (`:q` closes the viewer, not rsi).

| Command | What it does |
| --- | --- |
| `:w` | Save the file. |
| `:w!` | Save, overwriting changes made on disk since the file was opened. |
| `:wq` | Save and close the viewer. |
| `:x` | Save and close the viewer (same as `:wq`). |
| `:q` | Close the viewer; refused while there are unsaved changes. |
| `:q!` | Close the viewer, discarding unsaved changes. |
| `:quit!` | Close the viewer, discarding unsaved changes. |
| `:e!` | Revert the buffer to the version on disk. |
| `:42` | Jump to a line number (1-based): `:<n>`. |
| `:set autopair` | Turn bracket and quote auto-pairing on. |
| `:set noautopair` | Turn bracket and quote auto-pairing off. |
<!-- rsi:generated:end -->

**Search:**

| Key | Action | Description |
|-----|--------|-------------|
| `/` | Search Forward | Open forward search prompt (incremental, highlight-all) |
| `?` | Search Backward | Open backward search prompt |
| `n` | Next Match | Jump to next match in search direction |
| `N` | Previous Match | Jump to previous match (opposite direction) |
| `*` | Word Forward | Search forward for word under cursor |
| `#` | Word Backward | Search backward for word under cursor |
| `Enter` | Confirm Search | Finalize search pattern, close prompt |
| `Esc` | Cancel Search | Clear search highlights, close prompt |

**Code Folding (tree-sitter driven):**

| Key | Action | Description |
|-----|--------|-------------|
| `za` | Toggle Fold | Toggle fold at cursor line |
| `zo` | Open Fold | Expand fold at cursor line |
| `zc` | Close Fold | Collapse fold at cursor line |
| `zM` | Close All Folds | Collapse all foldable regions |
| `zR` | Open All Folds | Expand all folds |

Fold gutter markers: `▸` = collapsed, `▾` = expandable. Cursor automatically skips over folded interior lines during vertical movement. Search results inside folds auto-expand the fold.

**Bracket Matching:**

When the cursor is on a bracket character (`(`, `)`, `[`, `]`, `{`, `}`), the matching bracket is highlighted with a muted violet background. No keypress needed -- highlighting is automatic.

**Bracket Auto-pairing (insert mode):**

Enabled by default. Toggle with `:set autopair` / `:set noautopair`.

| Trigger | Behavior |
|---------|----------|
| Type `{`, `(`, `[` | Inserts matching pair, cursor between brackets |
| Type `}`, `)`, `]` when next char matches | Skips over existing close bracket |
| `Backspace` between empty pair (e.g. `{}`) | Deletes both brackets |

**Git Gutter:**

When a file is inside a git repository, the gutter shows per-line change indicators relative to `HEAD`:

| Sign | Color | Meaning |
|------|-------|---------|
| `+` | Green | Line added (not in HEAD) |
| `~` | Yellow | Line modified (content differs from HEAD) |
| `-` | Red | Deletion marker (lines removed before this line) |

Git gutter updates on file open and after each save (`:w` or `Ctrl+S`).

**Markdown Preview (`Space+m`):**

For `.md` files, `Space+m` toggles between raw source view (with tree-sitter syntax highlighting) and rendered markdown preview. In preview mode:
- Headers, bold, italic, inline code, bullet lists, numbered lists, and code blocks render with styling
- Editing keys are blocked (read-only)
- Navigation keys (`j`, `k`, `PageDown`, `PageUp`, `Ctrl+d/u/f/b`, `/`, `?`, `n`, `N`, `z`-commands) still work
- Press `Space+m` again to return to raw source view

### Settings Pane (`<Space>,` or `:set`)

The settings pane replaces the focused pane content. Categories on the left, items on the right.

<!-- rsi:generated:begin settings -->
Keys of the settings pane (category rail and items).

| Keys | Action | What it does |
| --- | --- | --- |
| `Ctrl-Alt-G` | Contextual help | Opens contextual help listing the keys and commands available in the current view; Ctrl-Alt-G works even while typing. |
| `?` | Contextual help | Opens contextual help listing the keys and commands available in the current view; Ctrl-Alt-G works even while typing. |
| `j` | Move selection or scroll down | Moves the selection down one row, or scrolls the focused view down. |
| `k` | Move selection or scroll up | Moves the selection up one row, or scrolls the focused view up. |
| `Enter` | Open or activate selection | Opens the selected row: drills into a Group or Epic in place, opens a leaf session's detail, or activates the selected setting. |
| `Delete` | Reset selected role | Resets the selected theme role to the active theme's default color. |
| `h` | Return to categories | Returns focus from the settings items to the category rail. |
| `Left` | Return to categories | Returns focus from the settings items to the category rail. |
| `l` | Enter selected category | Moves focus from the category rail into the selected category's items. |
| `Right` | Enter selected category | Moves focus from the category rail into the selected category's items. |
| `Space` | Toggle or activate setting | Toggles a boolean setting, cycles a choice, or activates the selected settings row. |
| `a` | Add item | Adds an item to the selected settings list (hooks, providers, budgets and similar lists). |
| `d` | Delete item | Deletes the selected item from a settings list. |
| `e` | Enable or disable skill | Enables or disables the selected Claude skill. |
| `R` | Refresh daemon-backed state | Re-reads daemon-backed settings state (config, hooks, skills, storage). |
| `/` | Search settings | Opens an incremental settings query; Enter jumps to the first match at or after the cursor. |
| `n` | Next settings match | Jumps to the next settings row matching the active query, wrapping with a notice. |
| `N` | Previous settings match | Jumps to the previous settings row matching the active query, wrapping with a notice. |
| `q` | Close or cancel | Closes the current view or cancels the pending action. |
| `Esc` | Close or cancel | Closes the current view or cancels the pending action. |
<!-- rsi:generated:end -->

**Theme & Colors category:**

This category has exactly 20 rows and keeps the legacy theme entry points intact.

| Row | Action | Behavior |
|-----|--------|----------|
| `Built-in theme` | `Enter` | Open the built-in theme picker (the list is under Theme Selection); preview is live and `Esc` rolls back |
| 17 semantic role rows | `Enter` / `Delete` | Edit that role's `#RRGGBB` override / reset only that role to the built-in value |
| `Legacy message/editor colors` | `Enter` | Open the existing seven-slot message-border/editor color customizer |
| `Reset active theme` | `Enter` | Clear all semantic role overrides without changing the selected built-in theme or legacy slots |

The role editor previews valid input on the next frame. `Enter` commits and `Esc` restores the complete opening override snapshot. A low-contrast or unverifiable value requires a second unchanged `Enter`; changing the input cancels that acknowledgement. Truecolor uses the entered RGB value, xterm-256 reports the quantized assessment, and ANSI-16/unknown capability or a terminal-default background is reported as unverifiable rather than inventing a contrast ratio. Only committed overrides are written to `state.json`.

**Message Bridge Form (Message Bridges category):**

| Key | Action | Description |
|-----|--------|-------------|
| `Tab` | Next Field | Move to next field |
| `Shift+Tab` | Prev Field | Move to previous field |
| `Space` | Toggle | Toggle enabled when the Enabled field is focused |
| `Enter` | Save | Write the bridge config and close form |
| `Esc` | Cancel | Discard changes and close form |
| Any char | Type | Append character to focused text field |
| `Backspace` | Delete | Remove last character from focused text field |

**Provider Form (API Models category):**

Clipboard paste works in every field. Each field is a shared vim text surface, so it supports insert/normal mode like the input bar.

| Key | Action | Description |
|-----|--------|-------------|
| `Tab` | Next Field | Move to next field (Name → URL → API Key → Model) |
| `Shift+Tab` | Prev Field | Move to previous field |
| `Ctrl+V` | Paste | Paste clipboard content into the focused field (image becomes `@path`; text inserts directly) |
| `Ctrl+Enter` | Save | Save provider and close form (works from any field) |
| `Esc` | Cancel | Discard changes and close form |
| `q` | Close | Close from normal mode |
| Any char | Type | Append character to focused field |
| `Backspace` | Delete | Remove last character from focused field |

**Hooks (Claude) category:**

Edits the `hooks` block of `~/.claude/settings.json`. Writes are atomic (tmp + rename) and preserve every other top-level key (`env`, `permissions`, `mcpServers`, `enabledPlugins`, …) verbatim. Saved hooks apply to **next-spawned** Claude sessions only — running sessions keep their already-loaded config.

| Key | Action | Description |
|-----|--------|-------------|
| `a` | Add Hook | Open empty `HookForm` overlay |
| `Enter` | Edit Hook | Open `HookForm` populated for the selected row |
| `d` | Delete Hook | Remove the selected hook (no confirm — matches API Models) |

**Hook Form Overlay:**

| Key | Action | Description |
|-----|--------|-------------|
| `Tab` / `Shift+Tab` | Cycle Field | Cycle Event → Matcher → Command → Timeout |
| `↑` / `↓` | Cycle Event | When Event field focused, cycle through `PreToolUse`, `PostToolUse`, `UserPromptSubmit`, `SessionStart`, `SessionEnd`, `Stop`, `SubagentStop`, `Notification`, `PreCompact` |
| `Enter` | Save | Validate + write `~/.claude/settings.json` atomically (works from any field) |
| `Esc` | Cancel | Discard changes and close |
| Any char | Type | Append to focused text field (Timeout accepts digits only) |
| `Backspace` | Delete | Remove last character from focused field |

**Hook Conflict Prompt** (triggered when the file changed externally between form-open and save):

| Key | Action | Description |
|-----|--------|-------------|
| `o` / `O` | Overwrite | Save anyway, clobbering external edits |
| `r` / `R` | Reload | Discard pending changes and re-read from disk |
| `c` / `C` / `Esc` | Cancel | Return to the form (no save, no reload) |

**Skills (Claude) category:**

Lists user-installed skills under `~/.claude/skills/`. Plugin-namespaced skills (sourced from `enabledPlugins`) are intentionally hidden — only directories directly inside `~/.claude/skills/` appear here. Skill changes take effect on the **next** Claude session.

| Key | Action | Description |
|-----|--------|-------------|
| `Enter` | Preview | Open read-only `SKILL.md` viewer |
| `e` | Toggle | Rename `<name>/` ↔ `<name>.disabled/` |
| `d` | Delete | Remove the skill directory (no confirm — matches API Models) |

**Skill Preview Overlay** (read-only `SKILL.md` viewer):

| Key | Action | Description |
|-----|--------|-------------|
| `j` / `Down` | Scroll Down | Scroll one line |
| `k` / `Up` | Scroll Up | Scroll one line |
| `Ctrl+d` / `Ctrl+u` | Half Page | Scroll ± 10 lines |
| `g` / `Home` | Top | Jump to first line |
| `G` / `End` | Bottom | Jump to last line |
| `q` / `Esc` | Close | Dismiss the preview |

**System Prompt category:**

| Key | Action | Description |
|-----|--------|-------------|
| `Enter` / `Space` | Cycle Preset | Cycle through Default → Concise → Code Only → Caveman |

**Budgets category:**

Views/adds/edits/deletes `rsi_common::model_control::ModelBudgetPolicy` rows via the daemon's `UpdateModelControlPolicy` RPC (`replace_policies: true`). Row count/label/value are derived live from the cached model-control status (`App::cached_model_control_status.policies`), same shape as Stats. An empty list shows a single placeholder row: it does **not** mean unlimited — hardcoded per-scope daemon defaults apply until an explicit policy is added.

| Key | Action | Description |
|-----|--------|-------------|
| `Enter` / `Space` | Edit / Add | Open `BudgetPolicyForm` populated for the selected row, or a blank form if the list is empty |
| `a` | Add | Open a blank `BudgetPolicyForm` |
| `d` | Delete | Delete the selected policy (RPC round-trip; no-op on the empty-state placeholder row) |
| `R` | Refresh | Re-fetch model-control status from the daemon |

**Budget Policy Form Overlay:**

| Key | Action | Description |
|-----|--------|-------------|
| `Tab` / `Shift+Tab` | Cycle Field | Cycle ScopeKind → ScopeId → Purpose → ModelTier → MaxTotalTokens → MaxConcurrency → MaxCallsPerWindow → RateWindowSeconds → AlertThresholdRatio |
| `↑` / `↓` | Cycle Value | When ScopeKind focused, cycle through Global/Provider/Project/Session/Tree/Workflow/Subsystem/Retry/ScheduledJob/IssueTracker/RecursiveGraph/Operator; when ModelTier focused, cycle any/local/standard/premium |
| `Enter` | Save | Validate, then enqueue the RPC write and close the form |
| `Esc` | Cancel | Discard changes and close |
| Any char | Type | Append to focused text field (numeric fields accept digits only; AlertThresholdRatio also accepts one `.`) |
| `Backspace` | Delete | Remove last character from focused text field (no-op on the two cycle fields) |

### Keybindings Help Overlay

Help is generated from the shared action registry and the captured focus, selection, mode, pending state, and daemon connection. It shows the bindings for that context and omits actions that cannot currently run. Search is case-insensitive, matches action names, descriptions, bindings, and aliases, and requires every space-separated term to match. Closing help restores the exact pane or suspended overlay, including its selection, scroll, editor draft, and transient state.

Contextual help also covers every overlay that owns its own keys (forms such as Create Entity in normal or insert mode, pickers, the model dropdown, File Explorer and its finder, the file viewer, Telescope, the command palette, the manager surfaces, notifications, the question panel, archive/trash, graph review, the recursive DAG browser, and settings-style editors). Those rows come from one discovery catalog in `crates/rsi/src/action_registry.rs` (`OVERLAY_HELP_ROUTES`), appear under the overlay's name, and match help search; they describe the overlay's handler and are never dispatched through the action registry or the command palette. From these overlays, open help with `Ctrl+Alt+G`; `?` is left to the overlay's own handler (Scheduled Jobs and the Theme Role Editor also accept `?`).

**Scroll Mode (default):**

| Key | Action | Description |
|-----|--------|-------------|
| `/` | Search | Enter descriptor search mode |
| `j` / `Down` | Scroll Down | Scroll content down one line |
| `k` / `Up` | Scroll Up | Scroll content up one line |
| `Ctrl+d` | Half Page Down | Scroll down 15 lines |
| `Ctrl+u` | Half Page Up | Scroll up 15 lines |
| `Ctrl+f` | Page Down | Scroll down 30 lines |
| `Ctrl+b` | Page Up | Scroll up 30 lines |
| `g` / `Home` | Jump to Top | Jump to beginning of content |
| `G` / `End` | Jump to Bottom | Jump to end of content |
| `Esc` | Clear / Close | Clear active filter, or close if no filter |
| `q` / `?` | Close | Close help overlay |

**Search Mode (after pressing `/`):**

| Key | Action | Description |
|-----|--------|-------------|
| *any char* | Filter | Search action names, descriptions, bindings, and aliases; all terms must match |
| `Backspace` | Delete | Remove last character from search |
| `Enter` | Accept | Accept filter and return to scroll mode |
| `Esc` | Clear | Clear filter and return to scroll mode |

While help search is active, `?` is ordinary filter text. In scroll mode, `Esc` first clears an accepted filter; only an unfiltered `Esc`, `q`, or `?` closes help and returns to the origin. During ordinary text entry, `?` remains input text; use `Ctrl+Alt+G` for help. This chord opens contextual help from any focus and is reserved by the global dispatcher.

### ESP Square Overlay (`<Space>gc`)

| Key | Action | Description |
|-----|--------|-------------|
| `u`/`i`/`o`/`j`/`k`/`l`/`m`/`,`/`.` | Guess | Guess a square by key position (u=top-left, .=bot-right) |
| `↑`/`↓`/`←`/`→` | Move Cursor | Arrow keys move cursor on 3×3 grid (appears on first press at top-left) |
| `Enter` | Guess / Reset | Confirm cursor guess (when cursor active, in-game); reset game when over or no cursor |
| `r` | Reset / New Game | Reset during a game or start a new game after round 12 |
| `;` | Pass | Flash the correct square in red and pick a new target (does not advance round) |
| `t` | Toggle Score | Toggle running score display during game |
| `Esc` / `q` | Close | Close ESP Square overlay |

**Flash behavior:** Correct guesses flash green, misses and peeks flash red over the target square. All flashes auto-clear after 300ms.

### Recursive DAG Browser Overlay (`:dag`)

Browser for recursive DAG graph inventory, selected graph tasks, scheduler runs, dedicated recovery, cancellation, heartbeat, interrupt, live attempt, validation, quarantine, and degraded artifact/test/diff/report inspectors. It exposes gated overlay-local `FAKE` scheduler, ordinary live scheduler, cancellation, and manual recovery continuation controls. Background loops, topology-scoped live controls, task-runtime cancellation, and direct session interrupts remain unavailable from `:dag`.

For setup, fixture generation, and end-to-end fake/manual usage, see
[Recursive DAG Operator Guide](recursive-dag.md).

| Key | Action | Description |
|-----|--------|-------------|
| `Tab` / `l` / `Right` | Next Panel | Cycle through graphs, tasks, runs, recovery, cancellations, heartbeats, interrupts, live, and artifacts |
| `Shift+Tab` / `h` / `Left` | Previous Panel | Cycle through graphs, tasks, runs, recovery, cancellations, heartbeats, interrupts, live, and artifacts |
| `j` / `Down` | Next Row | Move selection down in the active panel |
| `k` / `Up` | Previous Row | Move selection up in the active panel |
| `g` | First Row | Jump to the first row in the active panel |
| `G` | Last Row | Jump to the last row in the active panel |
| `Enter` | Load / Inspect | Hydrate the selected graph from the inventory; in Live, load selected live-attempt artifact buckets; in Artifacts, open the selected artifact, validation, test, diff, or report inspector, or load the next artifact summary page from the load-more row |
| `]a` / `[a` | Artifact Row | Move to the next/previous artifact inspector row inside `:dag` only |
| `p` | Preview View | Inside an open artifact inspector, call bounded `PreviewRecursiveExecutionArtifact` when `recursive_dag_artifact_preview_inspection` is enabled; otherwise show the bounded artifact preview unavailable state |
| `m` | Metadata View | Inside an open inspector, show bounded metadata |
| `l` | Links View | Inside an open inspector, switch to links; outside an inspector it remains Next Panel |
| `t` | Test View | Inside an open inspector, show typed test detail unavailable state without parsing raw artifact content |
| `d` | Diff View | Inside an open inspector, show typed diff detail unavailable state without parsing raw diff text |
| `r` | Refresh | Refresh the currently selected graph |
| `R` | FAKE Run Prompt | Open an overlay-local `max_steps` input for the selected graph when the daemon is connected and `recursive_dag_scheduler_control` is enabled |
| `L` | LIVE Run Prompt | Open an overlay-local `max_steps` input for the selected ordinary graph when the daemon is connected and `recursive_dag_live_scheduler_control` is enabled |
| `C` | Cancellation Prefix | Literal uppercase `C`, not Ctrl+C; opens the overlay-local cancellation prefix when connected and `recursive_dag_cancellation_control` is enabled |
| `C g` | Cancel Graph Prompt | Prompt for a nonempty typed reason, then call `RequestRecursiveGraphCancellation` for the selected active graph |
| `C r` | Cancel Run Prompt | Prompt for a nonempty typed reason, then call `RequestRecursiveSchedulerRunCancellation` for the selected active scheduler run |
| `C t` | Reserved Task Cancel | Disabled/reserved; task-scoped cancellation needs daemon execution semantics |
| `c` | Continue Recovery Prompt | Lowercase `c`; prompt for explicit positive `max_graphs` and optional digit-only `time_budget_ms`, then call `ContinueRecursiveRecovery` only when deferred work is visible |
| `!` | Control Gates | Show cancellation/recovery gate details without executing anything |
| `Ctrl+d` | Scroll Down | Scroll the browser content down |
| `Ctrl+u` | Scroll Up | Scroll the browser content up |
| `Esc` / `q` | Close | Close the open inspector first; close the recursive DAG browser when no inspector is open |

`R` and `L` never start unbounded runs: `max_steps` is required and must be a positive digit-only integer before the TUI calls `RunRecursiveFakeScheduler` or `RunRecursiveLiveScheduler`. When the daemon is disconnected or the relevant capability/config gate is disabled or missing, the browser renders the control as unavailable instead of calling the RPC.

Cancellation confirmation is the typed reason. The reason is trimmed, must be nonempty, and must be 240 characters or fewer; the TUI sends `requested_by = "rsi-tui"` and refreshes readbacks after a successful control RPC. Returned `Applied` or `Rejected` cancellation summaries are rendered as warnings because terminal targets can race with submit.

Recovery continuation confirmation is the explicit budget. `max_graphs` is required and positive; `time_budget_ms` is optional, may be `0`, and is omitted when left blank. Lowercase `c` continues deferred recovery manually; uppercase `C` is only the cancellation prefix. Recovery continuation is disabled when no deferred recovery work is visible.

The inspector is read-only and degraded until daemon preview/test/diff/report readbacks exist. It renders currently loaded artifact metadata, paginated artifact summary load-more/end/error states, validation details and issues, selected live-attempt artifact buckets, and explicit capability-disabled states. Bounded artifact preview unavailable, typed test detail unavailable, typed diff detail unavailable, and typed scheduler report inspector unavailable states do not call mutation, control, model, live execution, or background RPCs.

There are no global recursive DAG keybindings yet; `gd`/`gD` are intentionally unused in this phase.

### Graph Review Overlay (`<Space>v` or `:graph`)

Visual workflow graph editor for reviewing and editing WorkflowDefinitions.

| Key | Action | Description |
|-----|--------|-------------|
| `j` / `Down` | Node Below | Move selection to the nearest node below on the canvas |
| `k` / `Up` | Node Above | Move selection to the nearest node above on the canvas |
| `h` / `Left` | Node Left | Move selection to the nearest node to the left |
| `l` / `Right` | Node Right | Move selection to the nearest node to the right / Focus info dashboard (at rightmost boundary) |
| `[` / `]` | Predecessor / Successor | Follow an incoming / outgoing edge |
| `g` | First Node | Jump to the first node |
| `G` | Last Node | Jump to the last node |
| `Enter` | Detail Mode | Enter node detail/edit mode |
| `d` | Delete Node | Delete the selected node and its edges (undoable) |
| `u` | Undo | Undo the last edit (restores previous workflow state) |
| `z` | Toggle Fold | Collapse/expand topology or subgraph nodes |
| `r` | Run Workflow | Execute the workflow and show live execution status (authored workflows only; inert for read-only bridged views) |
| `R` | Recursive Graphs Picker | Open the "Recursive graphs" picker to render a recursive task graph read-only (gated on the `gv_render_recursive_origin` daemon capability; no-op when disabled) |
| `t` / `o` | Pickers | Open the Topology / SavedWorkflow picker (editable drafts; also on an empty graph) |
| `x` | Interrupt | Interrupt a running execution |
| `H` / `J` / `K` / `L` | Pan | Pan the graph view left / down / up / right |
| `c` | Follow | Return the camera to following the selection |
| `Tab` | Focus Dashboard | Switch focus from the canvas to the info dashboard column (when active) |
| `q` | Close | Close the graph review overlay |

**Info Dashboard keys** (when focused on the right-hand dashboard column):

| Key | Action | Description |
|-----|--------|-------------|
| `Tab` / `l` / `Right` | Next Panel | Cycle focus to the next dashboard panel (Runs, Recovery, Cancellations, Heartbeats, Interrupts) |
| `BackTab` / `h` / `Left` | Previous Panel | Cycle focus to the previous dashboard panel (focuses back to canvas from the leftmost Runs panel) |
| `j` / `Down` | Next Row | Select next row/event inside the active dashboard panel |
| `k` / `Up` | Previous Row | Select previous row/event inside the active dashboard panel |
| `g` | Jump Top | Jump to the first entry in the active dashboard panel |
| `G` | Jump Bottom | Jump to the last entry in the active dashboard panel |
| `Ctrl-D` | Scroll Down | Scroll the active dashboard panel down by 10 lines |
| `Ctrl-U` | Scroll Up | Scroll the active dashboard panel up by 10 lines |
| `r` | Refresh | Manually trigger a refresh/hydration of the dashboard data |
| `Esc` | Focus Canvas | Switch focus back to the graph canvas |
| `q` | Close | Close the graph review overlay |

**Detail mode keys** (after pressing `Enter`):

| Key | Action | Description |
|-----|--------|-------------|
| `i` | Edit Name | Start editing the node name inline |
| `I` | Edit Instructions | Start editing the node instructions inline |
| `s` / `S` | Edit Strategy | Start editing the integration / verification strategy (non-authored views) |
| `j` / `Down` | Next Edge | Select the next edge connected to this node |
| `k` / `Up` | Previous Edge | Select the previous edge connected to this node |
| `x` | Delete Edge | Delete the selected edge (undoable) |
| `[` / `]` | Predecessor / Successor | Follow an incoming / outgoing edge |
| `Enter` | Commit Edit | Save the current edit |
| `Esc` | Exit Detail | Return to graph navigation mode (or cancel edit) |
| `q` | Close | Close the graph review overlay |

**Picker mode keys** (after pressing `t` for Topology picker, `o` for SavedWorkflow picker, or `R` for the gated Recursive Graphs picker):

| Key | Action | Description |
|-----|--------|-------------|
| `j` / `Down` | Next Entry | Move selection down by one |
| `k` / `Up` | Previous Entry | Move selection up by one |
| `PageDown` / `Ctrl-D` | Half-Viewport Down | Scroll selection by ~10 entries |
| `PageUp` / `Ctrl-U` | Half-Viewport Up | Scroll selection by ~10 entries |
| `g` | First Entry | Jump to the first entry |
| `G` | Last Entry | Jump to the last entry |
| `Enter` | Select | Open the highlighted entry (template, saved workflow, or — for the Recursive Graphs picker — a read-only bridged view of the recursive graph) |
| `Esc` | Cancel | Close the picker and return to the previous mode |

The Recursive Graphs picker (`R`) renders a recursive task graph's **structure** read-only by bridging it to a workflow definition in-memory (no persistence). The title shows a `recursive (read-only)` badge and all mutating keys (`d`/`x`/`i`/`I`/`u`/`r`) are inert. The source is only reachable when the daemon capability `gv_render_recursive_origin` is enabled (see [recursive-dag.md](recursive-dag.md)).

### Prompt Creator (`<Space>gP`)

| Key | Action | Description |
|-----|--------|-------------|
| `j` / `Down` | Navigate Down | Move selection down in the prompt list |
| `k` / `Up` | Navigate Up | Move selection up in the prompt list |
| `Enter` | Open Editor | Open the selected prompt in the file editor |
| `Ctrl+n` | New Prompt | Create a new prompt file and open it in the editor |
| `d` | Delete Prompt | Delete the selected prompt file |
| `m` | Toggle Model | Toggle the model dropdown |
| `q` | Close | Close the prompt creator (or close editor, return to list) |
| `Ctrl+s` | Save | Save the current prompt (editor mode) |
| `Enter` | Newline | Insert a line break in editor insert mode, regardless of `submit_on_enter` |

---

## Overlay Key Reference (generated)

Generated from the overlay help catalog: the same rows `?` shows inside each overlay. The hand-written overlay sections above explain behavior; these tables list every key.

### Scheduled Jobs Browser

<!-- rsi:generated:begin schedule-browser -->
Keys of the scheduled jobs browser.

| Keys | Action | What it does |
| --- | --- | --- |
| `Ctrl-Alt-G` | Contextual help | Opens contextual help listing the keys and commands available in the current view; Ctrl-Alt-G works even while typing. |
| `?` | Contextual help | Opens contextual help listing the keys and commands available in the current view; Ctrl-Alt-G works even while typing. |
| `j` | Move selection or scroll down | Moves the selection down one row, or scrolls the focused view down. |
| `k` | Move selection or scroll up | Moves the selection up one row, or scrolls the focused view up. |
| `n` | Create scheduled job | Opens the form to create a scheduled job. |
| `Enter` | Edit selected job | Opens the selected scheduled job in the edit form. |
| `e` | Edit selected job | Opens the selected scheduled job in the edit form. |
| `Space` | Enable or disable job | Enables or disables the selected scheduled job. |
| `t` | Run selected job now | Runs the selected scheduled job now. |
| `d` | Begin delete chord | First `d` of `dd`: arms deletion of the selected scheduled job. |
| `dd` | Delete selected job | Deletes the selected scheduled job (`dd`). |
| `r` | Refresh scheduled jobs | Re-reads the scheduled jobs from the daemon. |
| `q` | Close or cancel | Closes the current view or cancels the pending action. |
| `Esc` | Close or cancel | Closes the current view or cancels the pending action. |
<!-- rsi:generated:end -->

### Theme Role Editor

<!-- rsi:generated:begin theme-role-editor -->
Keys of the theme role editor.

| Keys | Action | What it does |
| --- | --- | --- |
| `Ctrl-Alt-G` | Contextual help | Opens contextual help listing the keys and commands available in the current view; Ctrl-Alt-G works even while typing. |
| `?` | Contextual help | Opens contextual help listing the keys and commands available in the current view; Ctrl-Alt-G works even while typing. |
| `Enter` | Preview or commit color | Previews the typed color, or commits it to the role. |
| `Delete` | Reset this role | Resets this theme role to the active theme's default color. |
| `Esc` | Close or cancel | Closes the current view or cancels the pending action. |
<!-- rsi:generated:end -->

### Create Entity / Normal

<!-- rsi:generated:begin overlay-create-entity-normal -->
| Keys | Action |
| --- | --- |
| `Tab / Shift-Tab` | Next / previous field |
| `n / b / k / T / t` | Focus name / body / kind / tag / topology |
| `i` | Edit focused text field |
| `h / l` | Previous / next kind (kind field) |
| `Ctrl-P / Ctrl-Shift-P` | Cycle provider forward / backward |
| `e / E` | Cycle effort forward / backward |
| `s` | Toggle sandbox |
| `m` | Open model picker |
| `gp` | Open parent picker |
| `Enter` | Create entity |
| `Esc` | Save draft and close |
| `Ctrl-Enter` | Create entity |
| `Ctrl-D` | Discard draft and close |
<!-- rsi:generated:end -->

### Create Entity / Insert

<!-- rsi:generated:begin overlay-create-entity-insert -->
| Keys | Action |
| --- | --- |
| `Type, Backspace` | Edit name or tag |
| `Space / , / Tab` | Commit tag chip (tag field) |
| `Esc` | Return to normal mode |
| `Ctrl-Enter` | Create entity |
| `Ctrl-D` | Discard draft and close |
<!-- rsi:generated:end -->

### Create Entity / Body

<!-- rsi:generated:begin overlay-create-entity-body -->
| Keys | Action |
| --- | --- |
| `Tab / Shift-Tab` | Next / previous field |
| `Enter` | Insert newline (insert mode) |
| `Esc` | Save draft and close (normal mode) |
| `i / a / o` | Enter insert mode (normal mode) |
| `Esc` | Return to normal mode (insert mode) |
| `h j k l, w b` | Move cursor (normal mode) |
| `Ctrl-Enter` | Create entity |
| `Ctrl-D` | Discard draft and close |
<!-- rsi:generated:end -->

### Create Entity / Topology

<!-- rsi:generated:begin overlay-create-entity-topology -->
| Keys | Action |
| --- | --- |
| `Type, Backspace` | Filter topologies |
| `j / k, Down / Up, G` | Move selection |
| `Space` | Toggle topology preview |
| `Enter` | Choose topology and advance |
| `Esc` | Clear topology and focus kind |
| `Ctrl-Enter` | Create entity |
| `Ctrl-D` | Discard draft and close |
<!-- rsi:generated:end -->

### Model Picker

<!-- rsi:generated:begin overlay-model-picker -->
| Keys | Action |
| --- | --- |
| `j / k, Down / Up` | Move selection |
| `g / G` | Jump to first / last |
| `Tab / Shift-Tab` | Next / previous provider |
| `1-9` | Select numbered model |
| `Enter` | Select model |
| `Esc / q` | Close model picker |
<!-- rsi:generated:end -->

### Sort Picker

<!-- rsi:generated:begin overlay-sort-picker -->
| Keys | Action |
| --- | --- |
| `j / k, Down / Up` | Move selection |
| `g / G` | Jump to first / last |
| `Enter` | Apply sort order |
| `Esc / q` | Close without changing |
| `Space o` | Close and open a TaskRabbit session prompt |
| `Space m` | Close and open a blank session prompt |
| `Space ;` | Open the command palette |
<!-- rsi:generated:end -->

### Theme Picker

<!-- rsi:generated:begin overlay-theme-picker -->
| Keys | Action |
| --- | --- |
| `j / k, Down / Up` | Move and preview theme |
| `g / G` | Preview first / last theme |
| `1-9` | Apply numbered theme |
| `Enter` | Apply theme |
| `Esc / q` | Revert preview and close |
| `Space o` | Close and open a TaskRabbit session prompt |
| `Space m` | Close and open a blank session prompt |
| `Space ;` | Open the command palette |
<!-- rsi:generated:end -->

### Project Picker

<!-- rsi:generated:begin overlay-project-picker -->
| Keys | Action |
| --- | --- |
| `j / k, Down / Up` | Move selection |
| `g / G` | Jump to first / last |
| `Type, Backspace` | Filter projects |
| `Enter` | Select project |
| `Ctrl-N` | New project |
| `Ctrl-E` | Edit highlighted project |
| `Ctrl-D` | Delete highlighted project |
| `Esc` | Close picker |
| `Space o` | Close and open a TaskRabbit session prompt |
| `Space m` | Close and open a blank session prompt |
| `Space ;` | Open the command palette |
<!-- rsi:generated:end -->

### Label Picker

<!-- rsi:generated:begin overlay-label-picker -->
| Keys | Action |
| --- | --- |
| `j / k, Down / Up` | Move selection |
| `g / G` | Jump to first / last |
| `Type, Backspace` | Filter labels |
| `Enter` | Assign label |
| `Ctrl-N` | New label |
| `Ctrl-E` | Edit highlighted label |
| `Ctrl-D` | Delete highlighted label |
| `Esc` | Close picker |
| `Space o` | Close and open a TaskRabbit session prompt |
| `Space m` | Close and open a blank session prompt |
| `Space ;` | Open the command palette |
<!-- rsi:generated:end -->

### Parent Picker

<!-- rsi:generated:begin overlay-parent-picker -->
| Keys | Action |
| --- | --- |
| `j / k, Down / Up` | Move selection |
| `g / G` | Jump to first / last |
| `Type, Backspace` | Filter parents |
| `Enter` | Set parent |
| `Esc` | Close picker |
| `Space o` | Close and open a TaskRabbit session prompt |
| `Space m` | Close and open a blank session prompt |
| `Space ;` | Open the command palette |
<!-- rsi:generated:end -->

### File Explorer

<!-- rsi:generated:begin overlay-file-explorer -->
| Keys | Action |
| --- | --- |
| `j / k, Down / Up` | Move selection |
| `g / G` | Jump to first / last |
| `Enter / l` | Open file or directory |
| `h` | Go to parent directory |
| `yy / yn` | Copy path / file name |
| `dd` | Delete file to trash buffer |
| `u` | Restore last deleted file |
| `.` | Toggle hidden files |
| `/` | Open fuzzy finder |
| `Ctrl-L` | Focus file viewer |
| `Esc / q, Space / Space Space` | Close explorer |
<!-- rsi:generated:end -->

### File Explorer / Finder

<!-- rsi:generated:begin overlay-file-explorer-finder -->
| Keys | Action |
| --- | --- |
| `Type, Backspace` | Filter files |
| `j / k, Down / Up` | Move selection |
| `Enter` | Open selected file |
| `Esc` | Return to explorer tree |
| `Esc Esc` | Return to tree, then close explorer |
<!-- rsi:generated:end -->

### File Explorer / Viewer Focus

<!-- rsi:generated:begin overlay-file-explorer-viewer-focus -->
| Keys | Action |
| --- | --- |
| `Ctrl-H` | Focus explorer tree |
| `Other keys` | Go to the file viewer |
| `Esc / q` | Close explorer |
<!-- rsi:generated:end -->

### File Viewer

<!-- rsi:generated:begin overlay-file-viewer -->
| Keys | Action |
| --- | --- |
| `q, Space q` | Close viewer (normal mode) |
| `Space m` | Toggle markdown preview |
| `Space Space` | Open telescope file picker |
| `Ctrl-S` | Save file |
| `:` | Command mode (:w, :wq, :q, :q!, :e!, :N) |
| `/ / ?` | Search forward / backward |
| `n / N` | Next / previous match |
| `* / #` | Search word under cursor |
| `za / zo / zc` | Toggle / open / close fold |
| `zM / zR` | Close / open all folds |
| `PageDown / PageUp` | Scroll one page |
| `Backspace` | Back to session list |
| `i / a / o` | Enter insert mode (normal mode) |
| `Esc` | Return to normal mode (insert mode) |
| `h j k l, w b` | Move cursor (normal mode) |
<!-- rsi:generated:end -->

### Telescope

<!-- rsi:generated:begin overlay-telescope -->
| Keys | Action |
| --- | --- |
| `Type, Backspace` | Filter files |
| `j / k, Down / Up` | Move selection |
| `Enter` | Open file in viewer |
| `Esc` | Close telescope |
| `Space o` | Close and open a TaskRabbit session prompt |
| `Space m` | Close and open a blank session prompt |
| `Space ;` | Open the command palette |
<!-- rsi:generated:end -->

### Command Palette

<!-- rsi:generated:begin overlay-command-palette -->
| Keys | Action |
| --- | --- |
| `Type, Backspace` | Filter commands |
| `Down / Ctrl-N, Up / Ctrl-P` | Move selection |
| `Tab` | Edit command argument |
| `Enter` | Run selected command |
| `Esc` | Leave argument edit, then close |
<!-- rsi:generated:end -->

### Manager Scope

<!-- rsi:generated:begin overlay-manager-scope -->
| Keys | Action |
| --- | --- |
| `j / k, Down / Up` | Move selection |
| `g / G` | Jump to first / last |
| `/` | Search Epics |
| `Space` | Toggle selected Epic |
| `a` | Select whole project |
| `Enter` | Save manager scope |
| `Esc / Enter` | Finish search (search mode) |
| `Esc / q` | Close |
<!-- rsi:generated:end -->

### Manager Board

<!-- rsi:generated:begin overlay-manager-board -->
| Keys | Action |
| --- | --- |
| `1 / 2 / 3 / 4 / 5` | Board / Decisions / Inbox / Inspect / Policy |
| `Tab / Shift-Tab` | Next / previous section |
| `[ / ]` | Previous / next Inspect subsection (Inspect) |
| `j / k, Down / Up` | Move selection |
| `g / G` | Jump to first / last |
| `o` | Open the selected row's session |
| `n / p` | Next / previous page |
| `r` | Reload section |
| `PageDown / PageUp` | Scroll detail |
| `Esc / q` | Close manager |
<!-- rsi:generated:end -->

### Manager Decisions

<!-- rsi:generated:begin overlay-manager-decisions -->
| Keys | Action |
| --- | --- |
| `1 / 2 / 3 / 4 / 5` | Board / Decisions / Inbox / Inspect / Policy |
| `Tab / Shift-Tab` | Next / previous section |
| `[ / ]` | Previous / next Inspect subsection (Inspect) |
| `j / k, Down / Up` | Move selection |
| `g / G` | Jump to first / last |
| `Enter / a` | Answer selected decision |
| `o` | Open the selected row's session |
| `n / p` | Next / previous page |
| `r` | Reload section |
| `PageDown / PageUp` | Scroll detail |
| `Esc / q` | Close manager |
<!-- rsi:generated:end -->

### Manager Policy

<!-- rsi:generated:begin overlay-manager-policy -->
| Keys | Action |
| --- | --- |
| `1 / 2 / 3 / 4 / 5` | Board / Decisions / Inbox / Inspect / Policy |
| `Tab / Shift-Tab` | Next / previous section |
| `[ / ]` | Previous / next Inspect subsection (Inspect) |
| `j / k, Down / Up` | Move selection |
| `g / G` | Jump to first / last |
| `Enter / Space` | Edit selected policy field |
| `s` | Save policy |
| `r` | Reload policy |
| `Esc / q` | Close manager |
<!-- rsi:generated:end -->

### Manager / Text Entry

<!-- rsi:generated:begin overlay-manager-text-entry -->
| Keys | Action |
| --- | --- |
| `Type, Backspace` | Edit value |
| `Ctrl-U` | Clear value |
| `Enter` | Submit |
| `Esc` | Cancel entry |
<!-- rsi:generated:end -->

### Notifications

<!-- rsi:generated:begin overlay-notifications -->
| Keys | Action |
| --- | --- |
| `j / k, Down / Up` | Move selection |
| `g / G` | Jump to first / last |
| `x` | Dismiss selected notification |
| `N` | Dismiss all active notifications |
| `Enter` | Open source session |
| `Esc / q` | Close |
| `Space o` | Close and open a TaskRabbit session prompt |
| `Space m` | Close and open a blank session prompt |
| `Space ;` | Open the command palette |
<!-- rsi:generated:end -->

### Recent Completions

<!-- rsi:generated:begin overlay-recent-completions -->
| Keys | Action |
| --- | --- |
| `j / k, Down / Up` | Move selection |
| `g / G` | Jump to first / last |
| `Enter` | Open selected session |
| `i` | Return to input bar in insert mode |
| `Esc / q` | Close |
| `Space o` | Close and open a TaskRabbit session prompt |
| `Space m` | Close and open a blank session prompt |
| `Space ;` | Open the command palette |
<!-- rsi:generated:end -->

### Question / Normal

<!-- rsi:generated:begin overlay-question-normal -->
| Keys | Action |
| --- | --- |
| `j / k, Down / Up` | Move option cursor |
| `Space` | Select or toggle option |
| `1-9` | Select numbered option |
| `Enter / Backspace` | Next / previous question |
| `i` | Type a custom answer |
| `Ctrl-Enter` | Submit answers |
| `d` | Decline question |
| `Esc / q` | Hide question |
<!-- rsi:generated:end -->

### Question / Insert

<!-- rsi:generated:begin overlay-question-insert -->
| Keys | Action |
| --- | --- |
| `Type` | Write custom answer |
| `Enter / Ctrl-Enter` | Submit answers |
| `Shift-Enter` | Insert newline |
| `Esc` | Return to normal mode |
<!-- rsi:generated:end -->

### Trash

<!-- rsi:generated:begin overlay-trash -->
| Keys | Action |
| --- | --- |
| `j / k, Down / Up` | Move selection |
| `G` | Jump to last |
| `U` | Restore session |
| `D` | Purge session permanently |
| `Esc / q` | Close |
| `Space o` | Close and open a TaskRabbit session prompt |
| `Space m` | Close and open a blank session prompt |
| `Space ;` | Open the command palette |
<!-- rsi:generated:end -->

### Source Worktree Settlement

<!-- rsi:generated:begin overlay-source-worktree-settlement -->
| Keys | Action |
| --- | --- |
| `j / k, Down / Up` | Move selection |
| `J / K, PageDown / PageUp` | Page selection |
| `Enter` | Audit selected worktree |
| `A` | Begin authorization |
| `r` | Refresh receipt |
| `Esc / q` | Close |
| `Space o` | Close and open a TaskRabbit session prompt |
| `Space m` | Close and open a blank session prompt |
| `Space ;` | Open the command palette |
<!-- rsi:generated:end -->

### Graph Review

<!-- rsi:generated:begin overlay-graph-review -->
| Keys | Action |
| --- | --- |
| `h j k l, arrows` | Move to neighboring node |
| `[ / ]` | Predecessor / successor |
| `g / G` | First / last node |
| `Enter` | Open node detail |
| `d` | Delete node |
| `u` | Undo edit |
| `z` | Toggle collapse |
| `Tab` | Focus dashboard (bridged graphs) |
| `t` | Open topology picker |
| `o` | Open saved-workflow picker |
| `R` | Open recursive graph picker |
| `r` | Run authored draft |
| `x` | Interrupt running execution |
| `H / J / K / L` | Pan graph view |
| `c` | Camera follows selection |
| `q` | Close graph review |
| `Space o` | Close and open a TaskRabbit session prompt |
| `Space m` | Close and open a blank session prompt |
| `Space ;` | Open the command palette |
<!-- rsi:generated:end -->

### Graph Review / Empty

<!-- rsi:generated:begin overlay-graph-review-empty -->
| Keys | Action |
| --- | --- |
| `t` | Open topology picker |
| `o` | Open saved-workflow picker |
| `H / J / K / L` | Pan graph view |
| `c` | Camera follows selection |
| `q` | Close graph review |
| `Space o` | Close and open a TaskRabbit session prompt |
| `Space m` | Close and open a blank session prompt |
| `Space ;` | Open the command palette |
<!-- rsi:generated:end -->

### Graph Review / Node Detail

<!-- rsi:generated:begin overlay-graph-review-node-detail -->
| Keys | Action |
| --- | --- |
| `i / I` | Edit node name / instructions |
| `s / S` | Edit integration / verification strategy |
| `j / k, Down / Up` | Move edge selection |
| `[ / ]` | Predecessor / successor |
| `x` | Delete edge, or interrupt while running |
| `t` | Open topology picker |
| `o` | Open saved-workflow picker |
| `R` | Open recursive graph picker |
| `r` | Run authored draft |
| `x` | Interrupt running execution |
| `H / J / K / L` | Pan graph view |
| `c` | Camera follows selection |
| `Esc` | Back out of detail |
| `q` | Close graph review |
| `Space o` | Close and open a TaskRabbit session prompt |
| `Space m` | Close and open a blank session prompt |
| `Space ;` | Open the command palette |
<!-- rsi:generated:end -->

### Graph Review / Edit Field

<!-- rsi:generated:begin overlay-graph-review-edit-field -->
| Keys | Action |
| --- | --- |
| `Type, Backspace` | Edit field |
| `Enter` | Commit field |
| `Esc` | Cancel edit |
<!-- rsi:generated:end -->

### Graph Review / Picker

<!-- rsi:generated:begin overlay-graph-review-picker -->
| Keys | Action |
| --- | --- |
| `j / k, Down / Up` | Move selection |
| `g / G` | Jump to first / last |
| `PageDown / Ctrl-D, PageUp / Ctrl-U` | Page selection |
| `Enter` | Choose item |
| `Esc` | Close picker |
<!-- rsi:generated:end -->

### Recursive DAG Browser

<!-- rsi:generated:begin overlay-recursive-dag-browser -->
| Keys | Action |
| --- | --- |
| `Tab / l / Right` | Next panel |
| `Shift-Tab / h / Left` | Previous panel |
| `j / k, Down / Up` | Move selection |
| `g / G` | Jump to first / last |
| `Ctrl-D / Ctrl-U` | Scroll inspector |
| `]a / [a` | Next / previous artifact row |
| `Enter` | Open selection |
| `p / m / t / d` | Preview / metadata / test / diff view |
| `r` | Reload graph |
| `R / L` | Start fake / live run |
| `C g / C r` | Cancel graph / run (prompts for reason) |
| `c` | Continue recovery |
| `!` | Reveal control details |
| `Esc / q` | Close inspector, then browser |
| `Space o` | Close and open a TaskRabbit session prompt |
| `Space m` | Close and open a blank session prompt |
| `Space ;` | Open the command palette |
<!-- rsi:generated:end -->

### Project Form

<!-- rsi:generated:begin overlay-project-form -->
| Keys | Action |
| --- | --- |
| `Tab / Shift-Tab` | Next / previous field |
| `Type, Backspace` | Edit focused field |
| `Left / Right` | Cycle color (color field) |
| `Enter` | Save project |
| `Esc` | Cancel |
<!-- rsi:generated:end -->

### Label Form

<!-- rsi:generated:begin overlay-label-form -->
| Keys | Action |
| --- | --- |
| `Tab / Shift-Tab` | Next / previous field |
| `Type, Backspace` | Edit focused field |
| `Left / Right` | Cycle color (color field) |
| `Enter` | Save label |
| `Esc` | Cancel |
<!-- rsi:generated:end -->

### Provider Form

<!-- rsi:generated:begin overlay-provider-form -->
| Keys | Action |
| --- | --- |
| `Tab / Shift-Tab` | Next / previous field |
| `Enter / Ctrl-Enter` | Save provider |
| `Esc (normal mode), Ctrl-Q` | Close |
| `i / a / o` | Enter insert mode (normal mode) |
| `Esc` | Return to normal mode (insert mode) |
| `h j k l, w b` | Move cursor (normal mode) |
<!-- rsi:generated:end -->

### Message Bridge Form

<!-- rsi:generated:begin overlay-message-bridge-form -->
| Keys | Action |
| --- | --- |
| `Tab / Shift-Tab` | Next / previous field |
| `Type, Backspace` | Edit focused field |
| `Space` | Toggle enabled (first field) |
| `Enter` | Save bridge |
| `Esc` | Close |
<!-- rsi:generated:end -->

### Hook Form

<!-- rsi:generated:begin overlay-hook-form -->
| Keys | Action |
| --- | --- |
| `Tab / Shift-Tab` | Next / previous field |
| `Type, Backspace` | Edit focused field |
| `Up / Down` | Cycle event (event field) |
| `Enter` | Save hook |
| `Esc` | Close |
<!-- rsi:generated:end -->

### Hook Save Conflict

<!-- rsi:generated:begin overlay-hook-save-conflict -->
| Keys | Action |
| --- | --- |
| `o` | Overwrite settings on disk |
| `r` | Reload settings from disk |
| `c / Esc` | Cancel save |
| `Space o` | Close and open a TaskRabbit session prompt |
| `Space m` | Close and open a blank session prompt |
| `Space ;` | Open the command palette |
<!-- rsi:generated:end -->

### Budget Policy Form

<!-- rsi:generated:begin overlay-budget-policy-form -->
| Keys | Action |
| --- | --- |
| `Tab / Shift-Tab` | Next / previous field |
| `Type, Backspace` | Edit focused field |
| `Up / Down` | Cycle scope kind / model tier |
| `Enter` | Save policy |
| `Esc` | Close |
<!-- rsi:generated:end -->

### Schedule Form

<!-- rsi:generated:begin overlay-schedule-form -->
| Keys | Action |
| --- | --- |
| `Tab / Shift-Tab` | Next / previous field |
| `Type, Backspace` | Edit focused field |
| `Left / Right` | Cycle recurrence (recurrence field) |
| `Enter` | Save scheduled job |
| `Esc` | Return to Scheduled Jobs |
<!-- rsi:generated:end -->

### Rename Session

<!-- rsi:generated:begin overlay-rename-session -->
| Keys | Action |
| --- | --- |
| `Type, Backspace` | Edit text |
| `Enter` | Rename session |
| `Esc` | Cancel |
<!-- rsi:generated:end -->

### Color Customizer

<!-- rsi:generated:begin overlay-color-customizer -->
| Keys | Action |
| --- | --- |
| `Tab / j / Down` | Next field |
| `Shift-Tab / k / Up` | Previous field |
| `Type, Backspace` | Edit hex color |
| `Enter` | Apply color |
| `Delete` | Reset field to default |
| `Esc / q` | Close |
| `Space o` | Close and open a TaskRabbit session prompt |
| `Space m` | Close and open a blank session prompt |
| `Space ;` | Open the command palette |
<!-- rsi:generated:end -->

### Text Area Background

<!-- rsi:generated:begin overlay-text-area-background -->
| Keys | Action |
| --- | --- |
| `Type, Backspace` | Edit hex color |
| `Enter` | Apply color |
| `Delete` | Clear override |
| `Esc / q` | Close |
| `Space o` | Close and open a TaskRabbit session prompt |
| `Space m` | Close and open a blank session prompt |
| `Space ;` | Open the command palette |
<!-- rsi:generated:end -->

### Prompt Preview

<!-- rsi:generated:begin overlay-prompt-preview -->
| Keys | Action |
| --- | --- |
| `j / k, Down / Up` | Preview next / previous session |
| `G` | Preview last session |
| `Ctrl-D / Ctrl-U` | Scroll preview down / up |
| `Enter` | Open selected session |
| `p / Esc / q` | Close |
| `Space o` | Close and open a TaskRabbit session prompt |
| `Space m` | Close and open a blank session prompt |
| `Space ;` | Open the command palette |
<!-- rsi:generated:end -->

### Skill Preview

<!-- rsi:generated:begin overlay-skill-preview -->
| Keys | Action |
| --- | --- |
| `j / k, Down / Up` | Scroll |
| `Ctrl-D / Ctrl-U` | Half page down / up |
| `g / Home, G / End` | Jump to top / bottom |
| `Esc / q` | Close |
| `Space o` | Close and open a TaskRabbit session prompt |
| `Space m` | Close and open a blank session prompt |
| `Space ;` | Open the command palette |
<!-- rsi:generated:end -->

### Diagnostics

<!-- rsi:generated:begin overlay-diagnostics -->
| Keys | Action |
| --- | --- |
| `Esc / q` | Close |
| `Space o` | Close and open a TaskRabbit session prompt |
| `Space m` | Close and open a blank session prompt |
| `Space ;` | Open the command palette |
<!-- rsi:generated:end -->

### Session Info

<!-- rsi:generated:begin overlay-session-info -->
| Keys | Action |
| --- | --- |
| `R` | Rate session |
| `G` | Edit labels |
| `Esc / q` | Close |
| `Space o` | Close and open a TaskRabbit session prompt |
| `Space m` | Close and open a blank session prompt |
| `Space ;` | Open the command palette |
<!-- rsi:generated:end -->

### Rate Session

<!-- rsi:generated:begin overlay-rate-session -->
| Keys | Action |
| --- | --- |
| `1-9 / 0` | Set rating (0 = 10) |
| `h / l, Left / Right` | Lower / raise rating |
| `Enter` | Save rating |
| `Esc / q` | Close |
| `Space o` | Close and open a TaskRabbit session prompt |
| `Space m` | Close and open a blank session prompt |
| `Space ;` | Open the command palette |
<!-- rsi:generated:end -->

### Terminal

<!-- rsi:generated:begin overlay-terminal -->
| Keys | Action |
| --- | --- |
| `Other keys` | Send to the shell |
| `Ctrl-C` | Send interrupt to the shell |
| `Ctrl-\` | Close terminal |
<!-- rsi:generated:end -->

### Memory Search

<!-- rsi:generated:begin overlay-memory-search -->
| Keys | Action |
| --- | --- |
| `Type, Backspace` | Search memory |
| `j / k, Down / Up` | Move selection |
| `Esc / q` | Close |
<!-- rsi:generated:end -->

### AI Command

<!-- rsi:generated:begin overlay-ai-command -->
| Keys | Action |
| --- | --- |
| `Type, Backspace` | Edit text |
| `Enter` | Run instruction |
| `Esc` | Cancel |
<!-- rsi:generated:end -->

### AI Chat

<!-- rsi:generated:begin overlay-ai-chat -->
| Keys | Action |
| --- | --- |
| `Type, Backspace` | Edit text |
| `Enter` | Send question |
| `Up / Down` | Scroll conversation |
| `Esc` | Clear input, then close |
| `q` | Close (empty input) |
<!-- rsi:generated:end -->

### Card Editor

<!-- rsi:generated:begin overlay-card-editor -->
| Keys | Action |
| --- | --- |
| `j / k, Down / Up` | Move selection |
| `g / G` | Jump to first / last |
| `a` | Add fact |
| `e / i` | Edit selected fact |
| `dd` | Delete selected fact |
| `J / K` | Move fact down / up |
| `Ctrl-S` | Save card |
| `Enter` | Commit fact (editing) |
| `Esc` | Cancel fact edit (editing) |
| `Esc / q` | Save and close |
| `Space o` | Close and open a TaskRabbit session prompt |
| `Space m` | Close and open a blank session prompt |
| `Space ;` | Open the command palette |
<!-- rsi:generated:end -->

### Dialectic

<!-- rsi:generated:begin overlay-dialectic -->
| Keys | Action |
| --- | --- |
| `Type, Backspace` | Edit text |
| `Enter` | Send message |
| `Up / Down` | Scroll |
| `Esc` | Clear input, then close |
| `q` | Close (empty input) |
<!-- rsi:generated:end -->

### Input Modal

<!-- rsi:generated:begin overlay-input-modal -->
| Keys | Action |
| --- | --- |
| `Enter / Ctrl-Enter` | Send message |
| `Ctrl-Q` | Close and return text to the input bar |
| `Ctrl-A` | AI command on text |
| `Ctrl-Shift-A` | Ask AI about text |
| `Ctrl-Y` | Compile prompt |
| `Ctrl-Shift-G` | Grammar and spelling correction |
| `Ctrl-V` | Paste from clipboard |
| `i / a / o` | Enter insert mode (normal mode) |
| `Esc` | Return to normal mode (insert mode) |
| `h j k l, w b` | Move cursor (normal mode) |
<!-- rsi:generated:end -->

### Screens Without an Overlay Catalog

<!-- rsi:generated:begin overlay-exemptions -->
Screens without an overlay key catalog, and where their keys are documented.

| State | Reason | Keys documented at |
| --- | --- | --- |
| no overlay open | The focused pane's keys apply; see the Normal-mode table. | generated table `normal` |
| Scheduled Jobs browser | Its keys are registry routes, rendered as their own generated table. | generated table `schedule-browser` |
| Theme role editor | Its keys are registry routes, rendered as their own generated table. | generated table `theme-role-editor` |
| Launch / continue prompt | A text editor surface; its keys are documented by hand. | keybindings.md § Prompt Overlay (New Session / Continue Session) |
| Keybindings help | Help documents its own scroll, search and close keys. | keybindings.md § Keybindings Help Overlay |
| ESP Square game | A game with its own single-screen key legend. | keybindings.md § ESP Square Overlay (`<Space>gc`) |
| Manager policy: launch-choice catalog picker | A picker sub-mode of the manager policy editor with its own key handling. | keybindings.md § Harness Manager Policy (`:manager policy`) |
| Source-worktree settlement: authorization | A confirmation sub-mode of the settlement browser with its own key handling. | keybindings.md § Source-Worktree Settlement Authorization |
| Graph review: info dashboard focused | The dashboard panel owns keys while focused, separate from graph editing. | keybindings.md § Graph Review Overlay (`<Space>v` or `:graph`) |
<!-- rsi:generated:end -->

### Surfaces Documented by Hand

<!-- rsi:generated:begin untabulated -->
These surfaces decode keys in hand-written handlers. Their keys are not generated; see the named `docs/keybindings.md` section.

| Surface | Why it is not generated | Documented at |
| --- | --- | --- |
| Vim text editing (input bar and overlay editors) | The modalkit / textarea editing grammar (counts, motions, operators, visual mode) is not a key table. | keybindings.md § Vim Text Editing (Input Bar & Overlay) |
| Session input bar | Decoded directly by the input bar and input surface handlers. | keybindings.md § Input Bar Keybindings (Session Detail View) |
| Launch / continue prompt | An input-surface editor with its own mode handling. | keybindings.md § Prompt Overlay (New Session / Continue Session) |
| Prompt creator | Decoded directly by the prompt creator's key handler. | keybindings.md § Prompt Creator (`<Space>gP`) |
| File viewer (normal and insert keys) | Decoded directly by the file viewer; its `:` ex forms are generated. | keybindings.md § File Viewer (opened from File Explorer) |
<!-- rsi:generated:end -->

---

## Input Bar Keybindings (Session Detail View)

The input bar appears at the bottom of the session detail view for continuing sessions. There is no separate "focus" state — routing is automatic based on mode and content:

- **Insert mode**: input bar captures all keys (enter with `i`/`a`/`o`/`O`, exit with `Esc`)
- **Normal mode + content**: vim editing commands (`w`/`b`/`e`, `x`, `d`/`c`/`y`, `f`/`t`, etc.) go to the input bar; unrecognized keys pass through to session detail
- **Normal mode + empty**: all keys pass through to session detail
- Clipboard paste works from either mode and automatically enters insert mode if needed.

### Insert Mode

Enter with `i`/`a`/`o`/`O` from normal mode. `Esc` returns to normal mode (session detail).

| Key | Action | Description |
|-----|--------|-------------|
| `Esc` | Exit Insert | Return to normal mode / session detail (single press; dismisses suggestions if visible) |
| `Enter` | Submit | Send message and clear input bar (when `submit_on_enter: true`, default) |
| `Ctrl+Enter` | Submit | Send message and clear input bar (also submits from normal mode) |
| `Shift+Enter` | Newline | Insert newline in message (when `submit_on_enter: true`, default) |
| `Ctrl+V` | Paste | Paste from system clipboard (image saves to `~/.rsi/paste/` and inserts `@path` reference; text inserts directly) |
| `Ctrl+Shift+A` | Open AI Chat | Ask the configured prompt processor about non-empty draft text |
| `Ctrl+G` | Input Modal | Open quarter-size centered input modal for longer-form editing |
| `Ctrl+Y` | Compile Prompt | Send draft to LLM for 5-layer prompt compilation (requires `prompt_processor` configured) |
| `Ctrl+Shift+G` | Fix Grammar | Send draft to LLM for grammar/spelling correction only — no restructuring (requires `prompt_processor` configured) |
| Text keys | Type | Insert text |
| `Left` / `Right` | Move Cursor | Move cursor left/right (always available) |
| `Up` / `Down` | Scroll Detail | Scroll session detail (bypasses textarea; navigates suggestions when visible) |

> **Submit-on-Enter setting** — by default (`submit_on_enter: true`), plain `Enter` sends in the session input bar and `Shift+Enter` inserts a literal newline. Set it to `false` to restore the legacy bindings: `Ctrl+Enter` submits and plain `Enter` inserts a newline. `Ctrl+Enter` always submits regardless of the setting when the terminal can report it distinctly.
> The `Ctrl+G` input modal supports the same modal geometry keys as Prompt overlays: `Ctrl+Arrow` moves it, `Ctrl+Shift+Arrow` resizes it, and `Ctrl+0` resets its saved geometry.

**Suggestion Navigation** (when suggestions visible):

| Key | Action | Description |
|-----|--------|-------------|
| `Tab` | Accept | Accept selected suggestion |
| `Down` / `Ctrl+n` | Next | Move to next suggestion |
| `Up` / `Ctrl+p` | Previous | Move to previous suggestion |
| `Esc` | Exit | Dismiss suggestions and return to normal mode |

### Normal Mode (automatic when input bar has content)

When the input bar has draft text, vim editing commands are automatically routed to the input bar. Unrecognized keys pass through to session detail. This uses the full vim text editing feature set documented in [Vim Text Editing (Input Bar & Overlay)](#vim-text-editing-input-bar--overlay): motions, counts, operators, text objects, visual mode, dot repeat, and f/t/F/T character search.

**Input bar-specific keys** (in addition to the shared vim features):

| Key | Action | Description |
|-----|--------|-------------|
| `Ctrl+Enter` | Submit | Send message and clear input bar |
| `Ctrl+Shift+A` | Open AI Chat | Ask the configured prompt processor about non-empty draft text |
| `Ctrl+G` | Input Modal | Open quarter-size centered input modal for longer-form editing |
| `Ctrl+Y` | Compile Prompt | Send draft to LLM for 5-layer prompt compilation |
| `Ctrl+Shift+G` | Fix Grammar | Send draft to LLM for grammar/spelling correction only — no restructuring |

### Correction Preview

When `Ctrl+Y` (compile) or `Ctrl+Shift+G` (grammar) is pressed and a `prompt_processor` is configured, the draft is sent to an LLM. A preview panel appears above the input bar showing the result.

| Key | Action | Description |
|-----|--------|-------------|
| `a` | Accept | Replace textarea content with the corrected text (not shown for CLARIFY responses) |
| `d` / `Esc` | Discard | Dismiss the preview, keep original draft |
| `Ctrl+Enter` | Send Original | Submit the original draft, ignoring the preview |
| `Ctrl+Y` / `Ctrl+Shift+G` | Retry | Edit your draft and re-trigger the same correction |

While the correction is in-flight, a `CORRECTING...` badge is shown in the input bar area.

---

## Vim Text Editing (Input Bar & Overlay)

Both the session input bar and overlay prompt share a full vim normal-mode handler (`vim_textarea.rs`). All features below work identically in both surfaces.

### Count Modifiers

Prefix any motion, operator, or editing command with a numeric count:

| Example | Description |
|---------|-------------|
| `3w` | Move forward 3 words |
| `5j` | Move down 5 lines |
| `2dd` | Delete 2 lines |
| `3x` | Delete 3 characters |
| `2dw` | Delete 2 words |
| `10l` | Move right 10 characters |

Digits `1`-`9` start a count, `0` continues it (bare `0` is the line-start motion).

### Motions

| Key | Action | Description |
|-----|--------|-------------|
| `h` / `Left` | Move Left | Move cursor left |
| `j` / `Down` | Move Down | Move cursor down |
| `k` / `Up` | Move Up | Move cursor up |
| `l` / `Right` | Move Right | Move cursor right |
| `w` | Word Forward | Jump to start of next word |
| `b` | Word Back | Jump to start of previous word |
| `e` | Word End | Jump to end of current/next word |
| `0` | Line Start | Jump to beginning of line |
| `$` | Line End | Jump to end of line |
| `^` | First Non-Blank | Jump to first non-whitespace character on line |
| `gg` | Top | Jump to first line |
| `G` | Bottom | Jump to last line |
| `{` | Paragraph Back | Jump to previous blank line |
| `}` | Paragraph Forward | Jump to next blank line |
| `%` | Match Bracket | Jump to matching `()`, `[]`, or `{}` |

### Character Search (f/t/F/T)

| Key | Action | Description |
|-----|--------|-------------|
| `f{char}` | Find Forward | Jump to next occurrence of `{char}` on current line |
| `t{char}` | Till Forward | Jump to character before next `{char}` on current line |
| `F{char}` | Find Backward | Jump to previous occurrence of `{char}` on current line |
| `T{char}` | Till Backward | Jump to character after previous `{char}` on current line |
| `;` | Repeat Search | Repeat last `f`/`t`/`F`/`T` in same direction |
| `,` | Reverse Search | Repeat last `f`/`t`/`F`/`T` in opposite direction |

Character search works as a motion with operators: `df)` deletes through the next `)`, `ct"` changes up to (but not including) the next `"`.

### Mode Transitions

| Key | Action | Description |
|-----|--------|-------------|
| `i` | Insert | Enter insert mode at cursor |
| `a` | Append | Move forward and enter insert mode |
| `A` | Append End | Move to end of line and enter insert mode |
| `I` | Insert Start | Move to first non-blank and enter insert mode |
| `o` | Open Below | Insert newline below and enter insert mode |
| `O` | Open Above | Insert newline above and enter insert mode |

### Direct Editing

| Key | Action | Description |
|-----|--------|-------------|
| `x` | Delete Char | Delete character under cursor (accepts count: `3x`) |
| `s` | Substitute | Delete character and enter insert mode (accepts count: `3s`) |
| `r{char}` | Replace | Replace character under cursor with `{char}` (stays in normal mode) |
| `D` | Delete to End | Delete from cursor to end of line |
| `C` | Change to End | Delete to end of line and enter insert mode |
| `J` | Join Lines | Join current line with next line (accepts count: `3J`) |
| `~` | Toggle Case | Toggle uppercase/lowercase of character under cursor and advance (accepts count: `3~`) |
| `p` | Paste | Paste from clipboard |
| `u` | Undo | Undo last change |
| `Ctrl+r` | Redo | Redo last undone change |

### Operators + Text Objects

Operators (`d`, `c`, `y`) compose with text objects to act on structured regions of text:

| Key | Action | Description |
|-----|--------|-------------|
| `iw` / `aw` | Word | Inner word / a word (includes trailing space) |
| `i"` / `a"` | Double Quotes | Inside `"..."` / including quotes |
| `i'` / `a'` | Single Quotes | Inside `'...'` / including quotes |
| `i(` / `a(` | Parentheses | Inside `(...)` / including parens (also `)` or `b`) |
| `i{` / `a{` | Braces | Inside `{...}` / including braces (also `}` or `B`) |
| `i[` / `a[` | Brackets | Inside `[...]` / including brackets (also `]`) |
| `is` / `as` | Sentence | Inner sentence / a sentence (includes trailing space) |
| `ip` / `ap` | Paragraph | Inner paragraph / a paragraph (includes surrounding blank lines) |

**Examples:**

| Keys | Description |
|------|-------------|
| `ciw` | Change inner word (delete word, enter insert) |
| `da"` | Delete around double quotes (including the quotes) |
| `yi(` | Yank inside parentheses |
| `dap` | Delete a paragraph |
| `dgg` / `cgg` / `ygg` | Delete, change, or yank from the first line through the current line (linewise) |
| `ci{` | Change inside braces |

### Visual Mode

| Key | Action | Description |
|-----|--------|-------------|
| `v` | Visual Char | Enter character-wise visual selection |
| `V` | Visual Line | Enter line-wise visual selection |
| `v` (in visual) | Toggle | Exit char visual (or switch line→char) |
| `V` (in visual) | Toggle | Exit line visual (or switch char→line) |
| `Esc` | Exit Visual | Cancel visual selection |

In visual mode, motions extend the selection. Text objects work too (`viw` selects a word). Operators act on the selection immediately:

| Key | Action | Description |
|-----|--------|-------------|
| `d` / `x` | Delete | Delete visual selection |
| `c` / `s` | Change | Delete selection and enter insert mode |
| `y` | Yank | Copy selection (exits visual mode) |

### Dot Repeat

| Key | Action | Description |
|-----|--------|-------------|
| `.` | Repeat | Replay the last change (operator + motion + inserted text) |

Dot repeat records the full editing action. If the last change entered insert mode (e.g., `ciw` + typed text + `Esc`), the dot command replays both the deletion and the inserted text.

**Examples:**
- `dw` then `.` — deletes another word
- `ciw` → type "new" → `Esc` → move to another word → `.` — replaces that word with "new"
- `rX` then `l.` — replaces next character with X too
- `3x` then `.` — deletes 1 more character (count is not part of the recording)

---

## Session Detail and Recent Files

`Pane::SessionDetail` shows the status icon first in the provider/model metadata row. Vim mode (`NORMAL` or `INSERT`) appears immediately before `›` inside the input bar. The input extends to the bottom of the pane; there is no separate footer or T/C/M verification strip. The former `Ctrl+Shift+↑` / `Ctrl+Shift+↓` footer resize shortcuts are retired.

### Attention Jumps

| Key                       | Action               | Description                                                                                        |
|---------------------------|----------------------|----------------------------------------------------------------------------------------------------|
| `<Space>1` … `<Space>9`   | `JumpAttentionN`     | Jump to attention slot N from normal mode                                                        |
| `]a`                      | `NextAttention`      | Cycle to next attention session                                                                    |
| `[a`                      | `PrevAttention`      | Cycle to previous attention session                                                                |

`]a / [a` and `<Space>1..9` are driven by `crates/rsi/src/app/attention.rs::attention_session_ids`; bare digits in normal mode remain vim count prefixes such as `5j`.

### Recent Files

| Key            | Action               | Description                                                                                        |
|----------------|----------------------|----------------------------------------------------------------------------------------------------|
| `gf1` … `gf9`  | `OpenRecentFileN`    | Open recent-file slot N in the file viewer. Composable with the existing `g`-prefix grammar. Reuses the per-session `file_viewer_cache` so revisiting a path preserves cursor / fold state. |

The recent-files list is the top 5 distinct file paths from `Edit` / `MultiEdit` / `Write` / `Read` `tool_input.file_path` events for the focused session, newest-first.

---

## Notes

### Modal Toggle Behavior

Most popup modals support **toggle** behavior: the same keybinding that opens a modal will close it when pressed again. This applies to all modals opened from normal mode:

- **TaskRabbitPrompt** (`<Space>o`) — Opens a new TaskRabbit overlay (stacks with existing input overlays; close with `Ctrl+Q` or `Esc` from within)
- **BlankPrompt** (`<Space>m`) — Opens a new Blank overlay (stacks with existing input overlays; close with `Ctrl+Q` or `Esc` from within)
- **ModelDropdown** (`Ctrl+M`) — Press `Ctrl+M` to toggle model dropdown open/close (anchored below the header row or prompt model badge)
- **ThemePicker** (`T`) — Press `T` to toggle open/close
- **ProjectPicker** (`<Space>p`) — Press `<Space>p` to toggle open/close
- **SortPicker** (`<Space>s`) — Press `<Space>s` to toggle open/close
- **Archive Zone** (`ga`) — Press `ga` to switch to the archive zone in session list

All modals also support closing with:
- `Esc` key (dismiss overlay)
- `q` key (overlay normal mode only, for prompt-based modals)

### Context-Sensitive Behavior

Several keybindings change behavior based on context:

- **`l`**: Enter session from list view, OR next workspace from detail view
- **`i`/`a`/`o`/`O`**: Enter input bar insert mode with vim-native cursor semantics (`i` = insert at cursor, `a` = append after cursor, `o` = open line below, `O` = open line above). From list view, opens last session first. `Esc` returns to normal mode immediately (no intermediate focus state)
- **`j`/`k`**: Navigate sessions in list view only — do NOT scroll session detail (use mouse wheel or arrow keys for that)
- **Up/Down arrows**: Scroll session detail with viewport-aware selection tracking — navigate autocomplete suggestions when popup is visible
- **Mouse wheel**: Scroll session detail content — the original input method for line-level scrolling. Selection marker stays fixed during mouse scroll. When scrolled to the bottom, view auto-follows new content (scroll-lock). Scrolling up disengages scroll-lock; scrolling back to the bottom re-engages it.
- **`e`**: Enter selected session in normal mode when in list view, OR stay in normal mode when already in detail view
- **Fold operations (`zo`, `zc`, etc.)**: Only active in session detail view, operate on the event at cursor
- **Event cursor**: A `▎` marker in mauve shows which event is "at cursor" in the detail view. The cursor is derived from scroll position and updated by `Shift+Down`/`Shift+Up` navigation. Fold commands operate on the cursored event.

Normal Session List and Settings views do not reserve a persistent command-hint row. The row appears transiently only for `:`, `/`, or inline input. Medium/high-priority TTL notifications render in the center of the top status bar instead. A confirmed Session List filter remains visible in the scope header without consuming a row. Descriptive Settings context/impact panel footers are unchanged.

### Input Bar vs Overlay

- **Overlay**: Full-screen modal popup for new sessions or explicit continue prompts
- **Input Bar**: Inline bottom bar in session detail view for quick follow-ups

Both support vim-style modal editing and slash command suggestions.

### Vim Integration

rsi uses [modalkit](https://github.com/ulyssa/modalkit) for vim keybindings in the main navigation layer. Text input surfaces (input bar and overlay prompt) use a dedicated vim handler (`vim_textarea.rs`) with full Tier 2 support:

- **Navigation layer** (modalkit): `j`/`k` (list nav), mouse wheel (detail scroll), `h`, `l`, `gg`, `G`, `Ctrl+d/u/f/b`, `Ctrl+w {h,j,k,l}`
- **Text editing** (vim_textarea): counts (`3w`, `2dd`), operators + text objects (`ciw`, `da"`, `yi(`), visual mode (`v`, `V`), dot repeat (`.`), character search (`f`/`t`/`F`/`T`, `;`, `,`), and more — see [Vim Text Editing](#vim-text-editing-input-bar--overlay)

### Key Sequence Timing

Multi-key sequences like `ZZ`, `]a`, `gs`, `gi`, `gt` require the keys to be pressed in quick succession. If you type too slowly, the first key may be interpreted as a standalone command.

---

## Implementation Notes

**For developers**: When adding or modifying keybindings:

1. Add the descriptor and stable action identity in `crates/rsi/src/action_registry.rs`.
2. Add binding and command-alias metadata to that descriptor.
3. Implement the shared availability predicate and payload materialization.
4. Delegate registered dispatch to the existing domain executor; preserve unregistered `LcAction` behavior.
5. Update this document and add registry/binding/dispatch tests.

For command-mode commands:

1. Add payload-free aliases to the action descriptor; retain bounded parsing in `parse_command()` for arguments.
2. Update this document and test alias-to-action identity agreement.

For overlay-specific keybindings:

1. Add the target route binding and availability selector to the action descriptor.
2. Translate the raw target key into an `ActionRequest`, then delegate execution to the existing overlay handler.
3. Update this document and test both available and unavailable contexts.
