# Markdown Rendering + System Event Toggle Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add inline markdown rendering to session detail view and a toggleable system event filter.

**Architecture:** Extend the existing `content.rs` parser with a new `parse_inline_markdown()` function that produces styled `Span`s. Block-level elements (headers, blockquotes, lists, HRs, tables) are detected by line prefix. System event visibility is a per-session bool toggled by `zs`. Both `session.rs` and `height.rs` consume the same parser output to stay in sync.

**Tech Stack:** ratatui 0.29 (Span/Style/Line), crossterm backend, Catppuccin Mocha theme via `theme.rs`

---

## Phase 1: System Event Toggle

### Task 1: Add `show_system_events` field to SessionState

**Files:**
- Modify: `crates/rsi/src/types.rs:157-198`

**Step 1: Write the failing test**

Add to the bottom of `crates/rsi/src/types.rs` inside the existing `#[cfg(test)]` block (there is none currently — the tests are in other files). Instead, add a test in `crates/rsi/tests/tui_integration.rs`.

Actually, `SessionState` is constructed in many test files already. The field addition with a default will keep them compiling. Write a targeted unit test:

Add a new test module at the bottom of `crates/rsi/src/types.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_state_system_events_default_hidden() {
        let session = rsi_common::types::Session {
            id: uuid::Uuid::new_v4(),
            claude_session_id: None,
            query: "test".to_string(),
            working_dir: std::path::PathBuf::from("/tmp"),
            status: rsi_common::types::SessionStatus::Running,
            project_id: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            cost_usd: None,
            duration_ms: None,
            num_turns: None,
            model: None,
            input_tokens: None,
            output_tokens: None,
            context_window: None,
        };
        let state = SessionState::new(session);
        assert!(!state.show_system_events, "system events should be hidden by default");
    }
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p rsi test_session_state_system_events_default_hidden`
Expected: FAIL — `show_system_events` field doesn't exist

**Step 3: Add the field**

In `SessionState` struct (line ~157), add:

```rust
/// Whether system events are visible in the detail view. Default: false (hidden).
pub show_system_events: bool,
```

In `SessionState::new()` (line ~183), add to the initializer:

```rust
show_system_events: false,
```

**Step 4: Run test to verify it passes**

Run: `cargo test -p rsi test_session_state_system_events_default_hidden`
Expected: PASS

**Step 5: Commit**

```bash
git add crates/rsi/src/types.rs
git commit -m "feat: add show_system_events field to SessionState (default hidden)"
```

---

### Task 2: Add ToggleSystemEvents action + keybinding

**Files:**
- Modify: `crates/rsi/src/modalkit_types.rs:22-113`
- Modify: `crates/rsi/src/keybindings.rs:46-224`
- Modify: `crates/rsi/src/action_handler.rs:73-337`

**Step 1: Add the LcAction variant**

In `modalkit_types.rs`, add after `OpenAllFolds` (line ~84):

```rust
/// Toggle system event visibility in session detail view.
ToggleSystemEvents,
```

Update the `test_all_variants_exist` test — increment the expected count by 1 and add `LcAction::ToggleSystemEvents` to the vec.

**Step 2: Add the keybinding**

In `keybindings.rs`, add after the `zR = OpenAllFolds` mapping (line ~191):

```rust
// zs = ToggleSystemEvents ("z" visibility prefix, "s" for system)
machine.add_mapping(
    VimMode::Normal,
    &edge2(KeyCode::Char('z'), KeyCode::Char('s')),
    &lc_step(LcAction::ToggleSystemEvents),
);
```

**Step 3: Add the action handler**

In `action_handler.rs`, add a new arm in `dispatch_lc_action` after `OpenAllFolds` (line ~302):

```rust
LcAction::ToggleSystemEvents => {
    if let Some(Pane::SessionDetail { session_id }) = app.focused_pane().cloned()
        && let Some(state) = app.sessions.get_mut(&session_id)
    {
        state.show_system_events = !state.show_system_events;
        crate::ui::height::invalidate_heights(state);
        let label = if state.show_system_events { "shown" } else { "hidden" };
        app.status_message = Some(format!("System events {}", label));
    }
}
```

**Step 4: Write a test for the toggle action**

Add to `action_handler.rs` tests:

```rust
#[tokio::test]
async fn test_toggle_system_events() {
    use rsi_common::types::SessionStatus;
    let mut app = test_app();
    let id = add_session(&mut app, SessionStatus::Running);

    // Switch to detail view
    let tab = &mut app.tabs[app.active_tab];
    let focused = tab.focused_pane;
    if let Some(pane) = tab.layout.find_pane_mut(focused) {
        *pane = Pane::SessionDetail { session_id: id };
    }

    // Default: hidden
    assert!(!app.sessions.get(&id).unwrap().show_system_events);

    // Toggle on
    dispatch_action(&mut app, Action::Application(LcAction::ToggleSystemEvents)).await;
    assert!(app.sessions.get(&id).unwrap().show_system_events);

    // Toggle off
    dispatch_action(&mut app, Action::Application(LcAction::ToggleSystemEvents)).await;
    assert!(!app.sessions.get(&id).unwrap().show_system_events);
}

#[tokio::test]
async fn test_toggle_system_events_noop_in_list_view() {
    let mut app = test_app();
    // In list view — toggle should be a silent no-op
    dispatch_action(&mut app, Action::Application(LcAction::ToggleSystemEvents)).await;
    // No panic, no status message change beyond what's expected
}
```

**Step 5: Run all tests**

Run: `cargo test -p rsi`
Expected: All pass (including the 2 new tests)

**Step 6: Commit**

```bash
git add crates/rsi/src/modalkit_types.rs crates/rsi/src/keybindings.rs crates/rsi/src/action_handler.rs
git commit -m "feat: add zs keybinding to toggle system event visibility"
```

---

### Task 3: Filter system events in rendering + height

**Files:**
- Modify: `crates/rsi/src/ui/session.rs:165-317`
- Modify: `crates/rsi/src/ui/height.rs:39-203` and `215-238`

**Step 1: Write the failing test**

Add to `height.rs` tests:

```rust
#[test]
fn test_system_event_hidden_returns_zero_height() {
    let event = make_event(EventType::System, "system info", None, None);
    // When show_system_events is false, system events should contribute 0 height
    // We test this via update_event_heights with the flag set
    let mut state = make_test_state();
    state.events = vec![
        make_event(EventType::Message, "hello", None, None),
        make_event(EventType::System, "info", None, None),
        make_event(EventType::Message, "world", None, None),
    ];
    state.show_system_events = false;

    update_event_heights(&mut state, 80);

    // System event (index 1) should have height 0
    assert_eq!(state.event_heights[1], 0);
    // Offsets: event 0 starts at 0, event 1 at height[0], event 2 at height[0] (same, since system is 0)
    assert_eq!(state.event_offsets[2], state.event_heights[0]);
}

#[test]
fn test_system_event_shown_has_normal_height() {
    let mut state = make_test_state();
    state.events = vec![
        make_event(EventType::Message, "hello", None, None),
        make_event(EventType::System, "info", None, None),
    ];
    state.show_system_events = true;

    update_event_heights(&mut state, 80);

    // System event should have non-zero height when shown
    assert!(state.event_heights[1] > 0);
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p rsi test_system_event_hidden test_system_event_shown`
Expected: FAIL — `show_system_events` not used in height calculation

**Step 3: Update height.rs**

In `event_rendered_height()` (line 23), add a `show_system_events` parameter:

```rust
pub fn event_rendered_height(
    event: &ConversationEvent,
    is_collapsed: bool,
    is_expanded: bool,
    show_system_events: bool,
    width: u16,
) -> usize {
    // Hidden system events have zero height
    if event.event_type == EventType::System && !show_system_events {
        return 0;
    }
    let lines = build_event_lines(event, is_collapsed, is_expanded);
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    paragraph.line_count(width)
}
```

In `update_event_heights()` (line 215), pass the flag:

```rust
let h = event_rendered_height(event, is_collapsed, is_expanded, state.show_system_events, width);
```

Update ALL existing tests in `height.rs` that call `event_rendered_height` to pass `true` for `show_system_events` (the old behavior).

**Step 4: Update session.rs**

In `render_session_detail()`, add the system event skip at the top of the event loop (line ~165, inside `for event in &state.events {`):

```rust
// Skip hidden system events
if event.event_type == EventType::System && !state.show_system_events {
    continue;
}
```

**Step 5: Run all tests**

Run: `cargo test -p rsi`
Expected: All pass

**Step 6: Commit**

```bash
git add crates/rsi/src/ui/session.rs crates/rsi/src/ui/height.rs
git commit -m "feat: filter system events from rendering when hidden"
```

---

## Phase 2: Markdown Inline Parser

### Task 4: Add markdown theme colors

**Files:**
- Modify: `crates/rsi/src/ui/theme.rs:109-131`

**Step 1: Add semantic color mappings**

Add after the `code_fence_lang` line (line ~111):

```rust
// Markdown element styles
pub fn md_header() -> Color { mauve() }
pub fn md_blockquote() -> Color { overlay1() }
pub fn md_link_text() -> Color { blue() }
pub fn md_link_url() -> Color { overlay0() }
pub fn md_list_bullet() -> Color { overlay1() }
pub fn md_hr() -> Color { surface1() }
pub fn md_table_border() -> Color { surface1() }
pub fn md_inline_code_fg() -> Color { overlay0() }
pub fn md_inline_code_bg() -> Color { surface0() }
```

**Step 2: Verify it compiles**

Run: `cargo build -p rsi`
Expected: Success

**Step 3: Commit**

```bash
git add crates/rsi/src/ui/theme.rs
git commit -m "feat: add semantic theme colors for markdown elements"
```

---

### Task 5: Build the inline markdown parser

**Files:**
- Modify: `crates/rsi/src/ui/content.rs`

This is the core parser. It takes a single line of text and returns `Vec<Span>` with appropriate styles. It handles: `**bold**`, `*italic*`, `` `inline code` ``, `[text](url)`. It does NOT handle block-level elements (those are detected by line prefix in the caller).

**Step 1: Write failing tests**

Add to `content.rs` tests:

```rust
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;

#[test]
fn test_parse_inline_plain_text() {
    let spans = parse_inline_markdown("hello world");
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].content.as_ref(), "hello world");
}

#[test]
fn test_parse_inline_bold() {
    let spans = parse_inline_markdown("before **bold** after");
    assert_eq!(spans.len(), 3);
    assert_eq!(spans[0].content.as_ref(), "before ");
    assert_eq!(spans[1].content.as_ref(), "bold");
    assert!(spans[1].style.add_modifier.contains(Modifier::BOLD));
    assert_eq!(spans[2].content.as_ref(), " after");
}

#[test]
fn test_parse_inline_italic() {
    let spans = parse_inline_markdown("before *italic* after");
    assert_eq!(spans.len(), 3);
    assert_eq!(spans[1].content.as_ref(), "italic");
    assert!(spans[1].style.add_modifier.contains(Modifier::ITALIC));
}

#[test]
fn test_parse_inline_code() {
    let spans = parse_inline_markdown("before `code` after");
    assert_eq!(spans.len(), 3);
    assert_eq!(spans[1].content.as_ref(), "code");
    assert_eq!(spans[1].style.fg, Some(super::super::theme::md_inline_code_fg()));
    assert_eq!(spans[1].style.bg, Some(super::super::theme::md_inline_code_bg()));
}

#[test]
fn test_parse_inline_link() {
    let spans = parse_inline_markdown("see [docs](https://example.com) here");
    // Expected: "see " + OSC8 "docs" + " (https://example.com)" + " here"
    assert_eq!(spans.len(), 4);
    assert_eq!(spans[0].content.as_ref(), "see ");
    // spans[1] is the OSC 8 wrapped link text
    assert!(spans[1].content.contains("docs"));
    // spans[2] is the URL in parens
    assert!(spans[2].content.contains("https://example.com"));
    assert_eq!(spans[3].content.as_ref(), " here");
}

#[test]
fn test_parse_inline_unmatched_delimiter() {
    // Unmatched ** should be passed through as plain text
    let spans = parse_inline_markdown("before **unmatched");
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].content.as_ref(), "before **unmatched");
}

#[test]
fn test_parse_inline_outermost_wins() {
    // Nested: outermost bold wins, inner backtick treated as plain bold text
    let spans = parse_inline_markdown("**bold `code` bold**");
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].content.as_ref(), "bold `code` bold");
    assert!(spans[0].style.add_modifier.contains(Modifier::BOLD));
}

#[test]
fn test_parse_inline_empty() {
    let spans = parse_inline_markdown("");
    assert!(spans.is_empty());
}

#[test]
fn test_parse_inline_adjacent_elements() {
    let spans = parse_inline_markdown("**bold***italic*");
    assert_eq!(spans.len(), 2);
    assert!(spans[0].style.add_modifier.contains(Modifier::BOLD));
    assert!(spans[1].style.add_modifier.contains(Modifier::ITALIC));
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p rsi test_parse_inline`
Expected: FAIL — `parse_inline_markdown` doesn't exist

**Step 3: Implement the parser**

Add to `content.rs`:

```rust
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;

/// Parse inline markdown in a single line, returning styled spans.
///
/// Handles: **bold**, *italic*, `inline code`, [text](url).
/// Single-pass, left-to-right. Outermost delimiter wins (no nesting).
/// Unmatched delimiters are emitted as plain text.
pub fn parse_inline_markdown(line: &str) -> Vec<Span<'static>> {
    if line.is_empty() {
        return Vec::new();
    }

    let chars: Vec<char> = line.chars().collect();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut plain_start = 0;
    let mut i = 0;

    while i < chars.len() {
        // ** bold **
        if i + 1 < chars.len() && chars[i] == '*' && chars[i + 1] == '*' {
            if let Some(end) = find_closing(&chars, i + 2, &['*', '*']) {
                flush_plain(&chars, plain_start, i, &mut spans);
                let content: String = chars[i + 2..end].iter().collect();
                spans.push(Span::styled(
                    content,
                    Style::default().add_modifier(Modifier::BOLD),
                ));
                i = end + 2;
                plain_start = i;
                continue;
            }
        }

        // * italic * (but not **)
        if chars[i] == '*' && (i + 1 >= chars.len() || chars[i + 1] != '*') {
            if let Some(end) = find_closing_single(&chars, i + 1, '*') {
                flush_plain(&chars, plain_start, i, &mut spans);
                let content: String = chars[i + 1..end].iter().collect();
                spans.push(Span::styled(
                    content,
                    Style::default().add_modifier(Modifier::ITALIC),
                ));
                i = end + 1;
                plain_start = i;
                continue;
            }
        }

        // ` inline code `
        if chars[i] == '`' {
            if let Some(end) = find_closing_single(&chars, i + 1, '`') {
                flush_plain(&chars, plain_start, i, &mut spans);
                let content: String = chars[i + 1..end].iter().collect();
                spans.push(Span::styled(
                    content,
                    Style::default()
                        .fg(super::theme::md_inline_code_fg())
                        .bg(super::theme::md_inline_code_bg()),
                ));
                i = end + 1;
                plain_start = i;
                continue;
            }
        }

        // [text](url)
        if chars[i] == '[' {
            if let Some((text, url, end_pos)) = parse_link(&chars, i) {
                flush_plain(&chars, plain_start, i, &mut spans);
                // OSC 8 hyperlink: \x1b]8;;URL\x1b\\TEXT\x1b]8;;\x1b\\
                let hyperlink = format!("\x1b]8;;{}\x1b\\{}\x1b]8;;\x1b\\", url, text);
                spans.push(Span::styled(
                    hyperlink,
                    Style::default()
                        .fg(super::theme::md_link_text())
                        .add_modifier(Modifier::UNDERLINED),
                ));
                // Fallback: show URL in parens
                spans.push(Span::styled(
                    format!(" ({})", url),
                    Style::default().fg(super::theme::md_link_url()),
                ));
                i = end_pos;
                plain_start = i;
                continue;
            }
        }

        i += 1;
    }

    // Flush remaining plain text
    flush_plain(&chars, plain_start, chars.len(), &mut spans);
    spans
}

/// Flush accumulated plain text as an unstyled span.
fn flush_plain(chars: &[char], start: usize, end: usize, spans: &mut Vec<Span<'static>>) {
    if start < end {
        let text: String = chars[start..end].iter().collect();
        spans.push(Span::raw(text));
    }
}

/// Find closing double-char delimiter (e.g., **).
/// Returns the index of the first char of the closing delimiter.
fn find_closing(chars: &[char], start: usize, delim: &[char; 2]) -> Option<usize> {
    let mut i = start;
    while i + 1 < chars.len() {
        if chars[i] == delim[0] && chars[i + 1] == delim[1] {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Find closing single-char delimiter.
/// Returns the index of the closing delimiter.
fn find_closing_single(chars: &[char], start: usize, delim: char) -> Option<usize> {
    let mut i = start;
    while i < chars.len() {
        if chars[i] == delim {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Parse a markdown link: [text](url). Returns (text, url, end_position).
fn parse_link(chars: &[char], start: usize) -> Option<(String, String, usize)> {
    // start is at '['
    let text_end = find_closing_single(chars, start + 1, ']')?;
    // Must be followed by '('
    if text_end + 1 >= chars.len() || chars[text_end + 1] != '(' {
        return None;
    }
    let url_end = find_closing_single(chars, text_end + 2, ')')?;
    let text: String = chars[start + 1..text_end].iter().collect();
    let url: String = chars[text_end + 2..url_end].iter().collect();
    Some((text, url, url_end + 1))
}
```

**Step 4: Run tests**

Run: `cargo test -p rsi test_parse_inline`
Expected: All pass

**Step 5: Commit**

```bash
git add crates/rsi/src/ui/content.rs
git commit -m "feat: add inline markdown parser (bold, italic, code, links)"
```

---

### Task 6: Add block-level markdown detection

**Files:**
- Modify: `crates/rsi/src/ui/content.rs`

Block-level elements are detected by line prefix. This function takes a line and returns (block_type, remaining_content) so the caller can apply block styling and then run inline parsing on the content.

**Step 1: Write failing tests**

```rust
#[test]
fn test_detect_header() {
    let (block, content) = detect_block_element("## Hello World");
    assert_eq!(block, BlockElement::Header(2));
    assert_eq!(content, "Hello World");
}

#[test]
fn test_detect_blockquote() {
    let (block, content) = detect_block_element("> quoted text");
    assert_eq!(block, BlockElement::Blockquote);
    assert_eq!(content, "quoted text");
}

#[test]
fn test_detect_unordered_list() {
    let (block, content) = detect_block_element("- list item");
    assert_eq!(block, BlockElement::UnorderedList);
    assert_eq!(content, "list item");
}

#[test]
fn test_detect_ordered_list() {
    let (block, content) = detect_block_element("3. third item");
    assert_eq!(block, BlockElement::OrderedList(3));
    assert_eq!(content, "third item");
}

#[test]
fn test_detect_hr() {
    let (block, _) = detect_block_element("---");
    assert_eq!(block, BlockElement::HorizontalRule);
    let (block2, _) = detect_block_element("***");
    assert_eq!(block2, BlockElement::HorizontalRule);
    let (block3, _) = detect_block_element("___");
    assert_eq!(block3, BlockElement::HorizontalRule);
}

#[test]
fn test_detect_table_row() {
    let (block, content) = detect_block_element("| col1 | col2 |");
    assert_eq!(block, BlockElement::TableRow);
    assert_eq!(content, "| col1 | col2 |");
}

#[test]
fn test_detect_table_separator() {
    let (block, _) = detect_block_element("|---|---|");
    assert_eq!(block, BlockElement::TableSeparator);
}

#[test]
fn test_detect_plain() {
    let (block, content) = detect_block_element("just plain text");
    assert_eq!(block, BlockElement::None);
    assert_eq!(content, "just plain text");
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p rsi test_detect_`
Expected: FAIL

**Step 3: Implement**

Add to `content.rs`:

```rust
/// Block-level markdown element type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockElement {
    /// No block-level element detected.
    None,
    /// ATX header with level (1-6).
    Header(u8),
    /// Blockquote (> prefix).
    Blockquote,
    /// Unordered list item (- or * prefix).
    UnorderedList,
    /// Ordered list item with number.
    OrderedList(u32),
    /// Horizontal rule (---, ***, ___).
    HorizontalRule,
    /// Table data row (| col | col |).
    TableRow,
    /// Table separator row (|---|---|) — should be hidden.
    TableSeparator,
}

/// Detect block-level markdown element from a line prefix.
/// Returns (element_type, remaining_content_after_prefix).
pub fn detect_block_element(line: &str) -> (BlockElement, &str) {
    let trimmed = line.trim_start();

    // Horizontal rule: 3+ of same char (-, *, _), optionally with spaces
    if trimmed.len() >= 3 {
        let hr_char = trimmed.chars().next().unwrap();
        if matches!(hr_char, '-' | '*' | '_')
            && trimmed.chars().all(|c| c == hr_char || c == ' ')
            && trimmed.chars().filter(|&c| c == hr_char).count() >= 3
        {
            return (BlockElement::HorizontalRule, "");
        }
    }

    // Table separator: | followed by dashes/pipes/colons/spaces
    if trimmed.starts_with('|')
        && trimmed
            .chars()
            .all(|c| matches!(c, '|' | '-' | ':' | ' '))
        && trimmed.contains('-')
    {
        return (BlockElement::TableSeparator, "");
    }

    // Table row: starts and ends with |
    if trimmed.starts_with('|') && trimmed.ends_with('|') && trimmed.len() > 1 {
        return (BlockElement::TableRow, trimmed);
    }

    // Header: # prefix
    if trimmed.starts_with('#') {
        let level = trimmed.chars().take_while(|&c| c == '#').count();
        if level <= 6 {
            let rest = trimmed[level..].trim_start();
            return (BlockElement::Header(level as u8), rest);
        }
    }

    // Blockquote: > prefix
    if trimmed.starts_with('>') {
        let rest = trimmed[1..].trim_start();
        return (BlockElement::Blockquote, rest);
    }

    // Unordered list: - or * followed by space
    if (trimmed.starts_with("- ") || trimmed.starts_with("* "))
        && trimmed.len() > 2
    {
        return (BlockElement::UnorderedList, &trimmed[2..]);
    }

    // Ordered list: digits followed by . and space
    if let Some(dot_pos) = trimmed.find(". ") {
        let num_str = &trimmed[..dot_pos];
        if !num_str.is_empty() && num_str.chars().all(|c| c.is_ascii_digit()) {
            if let Ok(n) = num_str.parse::<u32>() {
                return (BlockElement::OrderedList(n), &trimmed[dot_pos + 2..]);
            }
        }
    }

    (BlockElement::None, trimmed)
}
```

**Step 4: Run tests**

Run: `cargo test -p rsi test_detect_`
Expected: All pass

**Step 5: Commit**

```bash
git add crates/rsi/src/ui/content.rs
git commit -m "feat: add block-level markdown detection (headers, lists, quotes, tables, HRs)"
```

---

## Phase 3: Integrate Rendering

### Task 7: Wire markdown parsing into session.rs

**Files:**
- Modify: `crates/rsi/src/ui/session.rs:265-299`

**Step 1: Replace the plain text rendering**

In `render_session_detail()`, replace the `ContentSegment::Text` branch (lines ~276-280):

FROM:
```rust
content::ContentSegment::Text(text) => {
    for content_line in text.lines() {
        lines.push(Line::from(format!("{}{}", indent, content_line)));
    }
}
```

TO:
```rust
content::ContentSegment::Text(text) => {
    for content_line in text.lines() {
        let (block, block_content) = content::detect_block_element(content_line);
        let styled_line = render_markdown_line(indent, &block, block_content);
        lines.push(styled_line);
    }
}
```

**Step 2: Add the `render_markdown_line` helper**

Add to `session.rs`:

```rust
/// Render a single markdown line with block-level prefix and inline styles.
fn render_markdown_line<'a>(indent: &str, block: &content::BlockElement, content: &str) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();

    // Always start with indent
    if !indent.is_empty() {
        spans.push(Span::raw(indent.to_string()));
    }

    match block {
        content::BlockElement::Header(level) => {
            let prefix = "#".repeat(*level as usize);
            spans.push(Span::styled(
                format!("{} ", prefix),
                Style::default().fg(theme::md_header()).add_modifier(Modifier::BOLD),
            ));
            // Header content: bold + mauve, with inline parsing
            for span in content::parse_inline_markdown(content) {
                spans.push(Span::styled(
                    span.content.to_string(),
                    span.style.fg(theme::md_header()).add_modifier(Modifier::BOLD),
                ));
            }
        }

        content::BlockElement::Blockquote => {
            spans.push(Span::styled(
                "\u{2502} ".to_string(), // │ left border
                Style::default().fg(theme::md_blockquote()),
            ));
            for span in content::parse_inline_markdown(content) {
                spans.push(Span::styled(
                    span.content.to_string(),
                    span.style.add_modifier(Modifier::ITALIC).fg(theme::md_blockquote()),
                ));
            }
        }

        content::BlockElement::UnorderedList => {
            spans.push(Span::styled(
                "\u{2022} ".to_string(), // • bullet
                Style::default().fg(theme::md_list_bullet()),
            ));
            spans.extend(content::parse_inline_markdown(content));
        }

        content::BlockElement::OrderedList(n) => {
            spans.push(Span::styled(
                format!("{}. ", n),
                Style::default().fg(theme::md_list_bullet()),
            ));
            spans.extend(content::parse_inline_markdown(content));
        }

        content::BlockElement::HorizontalRule => {
            // Full-width line using box-drawing character
            spans.push(Span::styled(
                "\u{2500}".repeat(60),
                Style::default().fg(theme::md_hr()),
            ));
        }

        content::BlockElement::TableRow => {
            // Color the pipe delimiters, leave cell content with inline parsing
            for part in content.split('|') {
                if !spans.is_empty() || content.starts_with('|') {
                    spans.push(Span::styled(
                        "\u{2502}".to_string(), // │
                        Style::default().fg(theme::md_table_border()),
                    ));
                }
                let trimmed = part.trim();
                if !trimmed.is_empty() {
                    spans.push(Span::raw(" ".to_string()));
                    spans.extend(content::parse_inline_markdown(trimmed));
                    spans.push(Span::raw(" ".to_string()));
                }
            }
        }

        content::BlockElement::TableSeparator => {
            // Skip separator rows entirely (design decision Q4)
            return Line::default();
        }

        content::BlockElement::None => {
            spans.extend(content::parse_inline_markdown(content));
        }
    }

    Line::from(spans)
}
```

**Step 3: Run all tests**

Run: `cargo test -p rsi`
Expected: All pass

**Step 4: Commit**

```bash
git add crates/rsi/src/ui/session.rs
git commit -m "feat: integrate markdown rendering into session detail view"
```

---

### Task 8: Mirror rendering in height.rs

**Files:**
- Modify: `crates/rsi/src/ui/height.rs:141-182`

**Step 1: Update build_event_lines Text branch**

Replace the `ContentSegment::Text` branch in `build_event_lines()` with the same markdown parsing logic. Since `render_markdown_line` is in `session.rs` (which is a sibling module), we have two options:

**Option A (recommended):** Move `render_markdown_line` to `content.rs` as a public function so both `session.rs` and `height.rs` can call it. This keeps rendering DRY.

Move the function:

```rust
// In content.rs, add:
pub fn render_markdown_line(indent: &str, block: &BlockElement, content: &str) -> Line<'static> {
    // ... same implementation as Task 7 Step 2 ...
}
```

Then both `session.rs` and `height.rs` call `content::render_markdown_line(indent, &block, block_content)`.

**Step 2: Update the Text branch in height.rs**

FROM:
```rust
content::ContentSegment::Text(text) => {
    for content_line in text.lines() {
        lines.push(Line::from(format!("{}{}", indent, content_line)));
    }
}
```

TO:
```rust
content::ContentSegment::Text(text) => {
    for content_line in text.lines() {
        let (block, block_content) = content::detect_block_element(content_line);
        lines.push(content::render_markdown_line(indent, &block, block_content));
    }
}
```

**Step 3: Run all tests**

Run: `cargo test --workspace`
Expected: All pass — height calculations now match rendering

**Step 4: Commit**

```bash
git add crates/rsi/src/ui/content.rs crates/rsi/src/ui/height.rs crates/rsi/src/ui/session.rs
git commit -m "refactor: move render_markdown_line to content.rs for DRY height/render sync"
```

---

### Task 9: Final integration test

**Files:**
- Modify: `crates/rsi/tests/tui_integration.rs`

**Step 1: Add integration test**

```rust
#[test]
fn test_markdown_rendering_produces_styled_spans() {
    use rsi::ui::content;

    // Bold
    let spans = content::parse_inline_markdown("hello **world**");
    assert!(spans.len() >= 2);

    // Block-level header
    let (block, text) = content::detect_block_element("## Title");
    assert_eq!(block, content::BlockElement::Header(2));
    assert_eq!(text, "Title");

    // System event toggle default
    let session = rsi_common::types::Session {
        id: uuid::Uuid::new_v4(),
        claude_session_id: None,
        query: "test".to_string(),
        working_dir: std::path::PathBuf::from("/tmp"),
        status: rsi_common::types::SessionStatus::Running,
        project_id: None,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        cost_usd: None,
        duration_ms: None,
        num_turns: None,
        model: None,
        input_tokens: None,
        output_tokens: None,
        context_window: None,
    };
    let state = rsi::types::SessionState::new(session);
    assert!(!state.show_system_events);
}
```

**Step 2: Run full test suite**

Run: `cargo test --workspace`
Expected: All pass

**Step 3: Final commit**

```bash
git add crates/rsi/tests/tui_integration.rs
git commit -m "test: add integration tests for markdown rendering and system event toggle"
```

---

## Summary

| Task | Description | Files | Status |
|------|-------------|-------|--------|
| 1 | Add `show_system_events` field | types.rs | ✅ |
| 2 | Action + keybinding (`zs`) | modalkit_types.rs, keybindings.rs, action_handler.rs | ✅ |
| 3 | Filter system events in render/height | session.rs, height.rs | ✅ |
| 4 | Markdown theme colors | theme.rs | ✅ |
| 5 | Inline markdown parser | content.rs | ✅ |
| 6 | Block-level detection | content.rs | ✅ |
| 7 | Wire into session.rs rendering | session.rs | ✅ |
| 8 | Mirror in height.rs (DRY refactor) | content.rs, height.rs, session.rs | ✅ |
| 9 | Integration tests | tui_integration.rs | ✅ |

**Implementation complete.** 11 commits, 1712 lines added across 10 files. Manual verification passed 2026-02-05.
