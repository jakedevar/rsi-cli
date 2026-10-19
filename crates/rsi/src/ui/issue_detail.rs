//! Issue detail (#2381): the inspector's body as wrapped markdown, handoff and
//! review sections that fold, and the related Issues (blocked by, blocks and
//! `#N` mentions) as navigable items. Pure functions over an [`Issue`] and the
//! dependency pages, so the renderer and the key handlers agree on the items.
//!
//! Every state carries a glyph and a word next to its colour, and every colour
//! comes from a theme role, so the high-contrast theme keeps the contrast it
//! promises.

use crate::types::{IssueWorkspaceState, OPERATOR_DECISION_LABEL, OPERATOR_REQUEST_LABEL};
use crate::ui::navigator_layout::display_width;
use crate::ui::{glyphs, session, theme};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use rsi_common::issue_workspace::IssueDependencyPageV1;
use rsi_common::types::{Issue, IssueHandoffV1, IssueStatus};
use unicode_segmentation::UnicodeSegmentation;
use uuid::Uuid;

/// Whether an Issue is an operator request or an operator decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OperatorKind {
    Request,
    Decision,
}

impl OperatorKind {
    /// A one-cell marker that stands beside the colour.
    pub(crate) fn glyph(self) -> &'static str {
        match self {
            Self::Request => glyphs::PIN,
            Self::Decision => glyphs::REQUIRED,
        }
    }

    pub(crate) fn word(self) -> &'static str {
        match self {
            Self::Request => "request",
            Self::Decision => "decision",
        }
    }
}

/// A decision outranks a request when an Issue carries both labels.
pub(crate) fn operator_kind(labels: &[String]) -> Option<OperatorKind> {
    if labels.iter().any(|label| label == OPERATOR_DECISION_LABEL) {
        Some(OperatorKind::Decision)
    } else if labels.iter().any(|label| label == OPERATOR_REQUEST_LABEL) {
        Some(OperatorKind::Request)
    } else {
        None
    }
}

/// The glyph, word and colour of a lifecycle state. The word is always shown.
pub(crate) fn status_mark(
    status: IssueStatus,
) -> (&'static str, &'static str, ratatui::style::Color) {
    match status {
        IssueStatus::Open => ("○", "Open", theme::status_waiting()),
        IssueStatus::InProgress => ("◐", "In progress", theme::status_running()),
        IssueStatus::Closed => (glyphs::DONE, "Closed", theme::status_completed()),
        IssueStatus::Cancelled => (glyphs::FAILED, "Cancelled", theme::status_failed()),
    }
}

/// `P1` style priority text and its colour; `P-` when unset.
pub(crate) fn priority_mark(priority: Option<u8>) -> (String, ratatui::style::Color) {
    match priority {
        Some(1) => ("P1".to_string(), theme::error_status()),
        Some(2) => ("P2".to_string(), theme::warning_status()),
        Some(value) => (format!("P{value}"), theme::text()),
        None => ("P-".to_string(), theme::dim_metadata()),
    }
}

// ---------------------------------------------------------------- inline ---

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ink {
    Plain,
    Bold,
    Code,
    Mention,
}

fn ink_style(ink: Ink) -> Style {
    match ink {
        Ink::Plain => Style::default().fg(theme::text()),
        Ink::Bold => Style::default()
            .fg(theme::text())
            .add_modifier(Modifier::BOLD),
        Ink::Code => Style::default().fg(theme::model_text()),
        Ink::Mention => Style::default()
            .fg(theme::accent())
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
    }
}

fn flush(plain: &mut String, out: &mut Vec<(String, Ink)>) {
    if !plain.is_empty() {
        out.push((std::mem::take(plain), Ink::Plain));
    }
}

/// Split one line into plain, `**bold**`, `` `code` `` and `#N` mention runs.
fn inline(text: &str) -> Vec<(String, Ink)> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut plain = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '*' && chars.get(i + 1) == Some(&'*') {
            let end = (i + 2..chars.len().saturating_sub(1))
                .find(|&k| chars[k] == '*' && chars[k + 1] == '*');
            if let Some(end) = end
                && end > i + 2
            {
                flush(&mut plain, &mut out);
                out.push((chars[i + 2..end].iter().collect(), Ink::Bold));
                i = end + 2;
                continue;
            }
        }
        if c == '`'
            && let Some(offset) = chars[i + 1..].iter().position(|&x| x == '`')
            && offset > 0
        {
            let end = i + 1 + offset;
            flush(&mut plain, &mut out);
            let code: String = chars[i..=end].iter().collect();
            out.push((code, Ink::Code));
            i = end + 1;
            continue;
        }
        if c == '#'
            && chars.get(i + 1).is_some_and(char::is_ascii_digit)
            && (i == 0 || !chars[i - 1].is_alphanumeric())
        {
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_ascii_digit() {
                j += 1;
            }
            if j - (i + 1) <= 9 {
                flush(&mut plain, &mut out);
                out.push((chars[i..j].iter().collect(), Ink::Mention));
                i = j;
                continue;
            }
        }
        plain.push(c);
        i += 1;
    }
    flush(&mut plain, &mut out);
    out
}

fn is_fence(line: &str) -> bool {
    line.trim_start().starts_with("```")
}

/// The Issue numbers a text mentions as `#N` (code fences excluded), in order
/// of first appearance.
pub(crate) fn mentions(text: &str) -> Vec<i64> {
    let mut found = Vec::new();
    let mut in_fence = false;
    for line in text.lines() {
        if is_fence(line) {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        for (run, ink) in inline(line) {
            if ink == Ink::Mention
                && let Ok(number) = run[1..].parse::<i64>()
                && number > 0
                && !found.contains(&number)
            {
                found.push(number);
            }
        }
    }
    found
}

// ------------------------------------------------------------------ wrap ---

type Word = Vec<(String, Style)>;

fn words(segments: &[(String, Style)]) -> Vec<Word> {
    let mut out = Vec::new();
    let mut current: Word = Vec::new();
    for (text, style) in segments {
        for ch in text.chars() {
            if ch.is_whitespace() {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            } else if let Some(last) = current.last_mut()
                && last.1 == *style
            {
                last.0.push(ch);
            } else {
                current.push((ch.to_string(), *style));
            }
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn word_width(word: &Word) -> usize {
    word.iter().map(|(text, _)| display_width(text)).sum()
}

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|span| display_width(&span.content)).sum()
}

fn push_run(spans: &mut Vec<Span<'static>>, text: &str, style: Style) {
    if let Some(last) = spans.last_mut()
        && last.style == style
    {
        last.content.to_mut().push_str(text);
    } else {
        spans.push(Span::styled(text.to_string(), style));
    }
}

/// Greedy word wrap into `width` cells; `first` leads the first line and `rest`
/// every following one. A word wider than a line is split at grapheme edges.
fn wrap(
    words: &[Word],
    first: Vec<Span<'static>>,
    rest: Vec<Span<'static>>,
    width: usize,
) -> Vec<Line<'static>> {
    let width = width.max(1);
    let rest_width = spans_width(&rest);
    let mut lines = Vec::new();
    let mut base = spans_width(&first);
    let mut col = base;
    let mut current = first;
    for word in words {
        let w = word_width(word);
        if col > base && col + 1 + w > width {
            lines.push(Line::from(std::mem::replace(&mut current, rest.clone())));
            base = rest_width;
            col = rest_width;
        } else if col > base {
            push_run(&mut current, " ", Style::default());
            col += 1;
        }
        for (text, style) in word {
            for grapheme in text.graphemes(true) {
                let gw = display_width(grapheme);
                if col + gw > width && col > base {
                    lines.push(Line::from(std::mem::replace(&mut current, rest.clone())));
                    base = rest_width;
                    col = rest_width;
                }
                push_run(&mut current, grapheme, *style);
                col += gw;
            }
        }
    }
    lines.push(Line::from(current));
    lines
}

fn wrap_text(
    text: &str,
    style: Style,
    first: Vec<Span<'static>>,
    rest: Vec<Span<'static>>,
    width: usize,
) -> Vec<Line<'static>> {
    wrap(&words(&[(text.to_string(), style)]), first, rest, width)
}

fn indent(n: usize) -> Vec<Span<'static>> {
    if n == 0 {
        Vec::new()
    } else {
        vec![Span::raw(" ".repeat(n))]
    }
}

fn inline_words(text: &str) -> Vec<Word> {
    let segments: Vec<(String, Style)> = inline(text)
        .into_iter()
        .map(|(run, ink)| (run, ink_style(ink)))
        .collect();
    words(&segments)
}

// -------------------------------------------------------------- markdown ---

fn dim() -> Style {
    Style::default().fg(theme::dim_metadata())
}

fn rule_line(width: usize) -> Line<'static> {
    Line::from(Span::styled("─".repeat(width), dim()))
}

fn heading_text(line: &str) -> Option<(usize, &str)> {
    let trimmed = line.trim_start();
    let level = trimmed.chars().take_while(|&c| c == '#').count();
    if (1..=6).contains(&level) {
        let rest = trimmed[level..].strip_prefix(' ')?;
        let rest = rest.trim();
        if !rest.is_empty() {
            return Some((level, rest));
        }
    }
    None
}

fn is_rule(line: &str) -> bool {
    let t = line.trim();
    t.len() >= 3
        && (t.chars().all(|c| c == '-')
            || t.chars().all(|c| c == '*')
            || t.chars().all(|c| c == '_'))
}

fn list_item(line: &str) -> Option<(usize, String, &str)> {
    let trimmed = line.trim_start();
    let pad = line.len() - trimmed.len();
    for bullet in ["- ", "* ", "+ "] {
        if let Some(rest) = trimmed.strip_prefix(bullet) {
            return Some((pad, "•".to_string(), rest));
        }
    }
    let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
    if (1..=3).contains(&digits) {
        let after = &trimmed[digits..];
        if let Some(rest) = after
            .strip_prefix(". ")
            .or_else(|| after.strip_prefix(") "))
        {
            return Some((pad, format!("{}.", &trimmed[..digits]), rest));
        }
    }
    None
}

fn chunk_cells(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut used = 0;
    for grapheme in text.graphemes(true) {
        let gw = display_width(grapheme);
        if used + gw > width && used > 0 {
            chunks.push(std::mem::take(&mut current));
            used = 0;
        }
        current.push_str(grapheme);
        used += gw;
    }
    chunks.push(current);
    chunks
}

fn flush_paragraph(paragraph: &mut Vec<String>, width: usize, out: &mut Vec<Line<'static>>) {
    if paragraph.is_empty() {
        return;
    }
    let joined = paragraph.join(" ");
    paragraph.clear();
    out.extend(wrap(&inline_words(&joined), Vec::new(), Vec::new(), width));
}

fn blank_before(out: &mut Vec<Line<'static>>) {
    if out.last().is_some_and(|line| line.width() > 0) {
        out.push(Line::default());
    }
}

/// Render markdown lines into wrapped, styled lines: headings, bullets and
/// numbered items with hanging indents, block quotes, rules, fenced code and
/// inline bold, code and `#N` mentions.
pub(crate) fn markdown_lines(source: &[String], width: usize) -> Vec<Line<'static>> {
    let width = width.max(8);
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut paragraph: Vec<String> = Vec::new();
    let mut in_fence = false;
    for raw in source {
        let line = raw.trim_end();
        if is_fence(line) {
            flush_paragraph(&mut paragraph, width, &mut out);
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            for chunk in chunk_cells(line, width.saturating_sub(2)) {
                out.push(Line::from(vec![
                    Span::styled("│ ", dim()),
                    Span::styled(chunk, ink_style(Ink::Code)),
                ]));
            }
            continue;
        }
        if line.trim().is_empty() {
            flush_paragraph(&mut paragraph, width, &mut out);
            blank_before(&mut out);
        } else if let Some((level, text)) = heading_text(line) {
            flush_paragraph(&mut paragraph, width, &mut out);
            blank_before(&mut out);
            let style = if level <= 2 {
                Style::default()
                    .fg(theme::accent())
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(theme::text())
                    .add_modifier(Modifier::BOLD)
            };
            out.extend(wrap_text(text, style, Vec::new(), Vec::new(), width));
        } else if is_rule(line) {
            flush_paragraph(&mut paragraph, width, &mut out);
            out.push(rule_line(width));
        } else if let Some((pad, marker, rest)) = list_item(line) {
            flush_paragraph(&mut paragraph, width, &mut out);
            let lead = pad.min(width / 2);
            let mut first = indent(lead);
            first.push(Span::styled(format!("{marker} "), dim()));
            let hang = lead + display_width(&marker) + 1;
            out.extend(wrap(&inline_words(rest), first, indent(hang), width));
        } else if let Some(quote) = line.trim_start().strip_prefix('>') {
            flush_paragraph(&mut paragraph, width, &mut out);
            let bar = vec![Span::styled(format!("{} ", glyphs::QUOTE_RAIL), dim())];
            out.extend(wrap(&inline_words(quote.trim()), bar.clone(), bar, width));
        } else {
            paragraph.push(line.trim().to_string());
        }
    }
    flush_paragraph(&mut paragraph, width, &mut out);
    while out.last().is_some_and(|line| line.width() == 0) {
        out.pop();
    }
    out
}

// ---------------------------------------------------------------- blocks ---

/// What a body block is: plain prose, or a handoff or review that folds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlockKind {
    Plain,
    Handoff,
    Review,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Block {
    pub heading: Option<String>,
    pub level: usize,
    pub lines: Vec<String>,
    /// A `---` rule opened this block.
    pub rule_before: bool,
    pub kind: BlockKind,
}

impl Block {
    fn is_blank(&self) -> bool {
        self.heading.is_none() && self.lines.iter().all(|line| line.trim().is_empty())
    }

    /// The lines a folded block shows when open: all of them, minus the first
    /// non-blank line when that line already names the block.
    fn content(&self) -> Vec<String> {
        if self.heading.is_some() {
            return self.lines.clone();
        }
        let mut skipped = false;
        self.lines
            .iter()
            .filter(|line| {
                if !skipped && !line.trim().is_empty() {
                    skipped = true;
                    return false;
                }
                true
            })
            .cloned()
            .collect()
    }

    /// The name a folded block shows: its heading, else its first line.
    fn title(&self) -> String {
        self.heading.clone().unwrap_or_else(|| {
            self.lines
                .iter()
                .find(|line| !line.trim().is_empty())
                .map(|line| line.trim().to_string())
                .unwrap_or_default()
        })
    }
}

fn classify(block: &Block) -> BlockKind {
    if let Some(heading) = &block.heading {
        // A heading names the section: `## Handoff`, `## Review verdict`.
        let heading = heading.to_lowercase();
        let has = |needle: &str| heading.contains(needle);
        return if has("review") || has("verdict") {
            BlockKind::Review
        } else if has("handoff") || has("hand-off") || has("result") {
            BlockKind::Handoff
        } else {
            BlockKind::Plain
        };
    }
    // No heading: the first line must open with the worker's own marker.
    let first = block
        .lines
        .iter()
        .find(|line| !line.trim().is_empty())
        .map(|line| line.trim().to_lowercase())
        .unwrap_or_default();
    if first.starts_with("review") || first.starts_with("verdict") {
        BlockKind::Review
    } else if first.starts_with("result ")
        || first.starts_with("pipeline handoff")
        || first.starts_with("handoff")
    {
        BlockKind::Handoff
    } else {
        BlockKind::Plain
    }
}

/// Split an Issue body at headings and `---` rules. A sub-heading under a
/// folding block stays inside it.
pub(crate) fn split_blocks(body: &str) -> Vec<Block> {
    let fresh = |heading: Option<(usize, String)>, rule_before: bool| Block {
        level: heading.as_ref().map_or(0, |(level, _)| *level),
        heading: heading.map(|(_, text)| text),
        lines: Vec::new(),
        rule_before,
        kind: BlockKind::Plain,
    };
    let mut blocks: Vec<Block> = Vec::new();
    let mut current = fresh(None, false);
    let mut in_fence = false;
    for line in body.lines() {
        if is_fence(line) {
            in_fence = !in_fence;
            current.lines.push(line.to_string());
        } else if in_fence {
            current.lines.push(line.to_string());
        } else if let Some((level, text)) = heading_text(line) {
            blocks.push(std::mem::replace(
                &mut current,
                fresh(Some((level, text.to_string())), false),
            ));
        } else if is_rule(line) && line.trim().starts_with('-') {
            blocks.push(std::mem::replace(&mut current, fresh(None, true)));
        } else {
            current.lines.push(line.to_string());
        }
    }
    blocks.push(current);
    blocks.retain(|block| !block.is_blank());
    for block in &mut blocks {
        block.kind = classify(block);
    }
    // A sub-heading under a folding block belongs to it.
    let mut merged: Vec<Block> = Vec::new();
    for block in blocks {
        if let Some(parent) = merged.last_mut()
            && parent.kind != BlockKind::Plain
            && parent.heading.is_some()
            && block.heading.is_some()
            && !block.rule_before
            && block.level > parent.level
        {
            let hashes = "#".repeat(block.level);
            parent.lines.push(format!(
                "{hashes} {}",
                block.heading.clone().unwrap_or_default()
            ));
            parent.lines.extend(block.lines);
        } else {
            merged.push(block);
        }
    }
    merged
}

// ----------------------------------------------------------------- model ---

/// One related Issue as the detail shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Dep {
    pub number: i64,
    pub title: String,
    pub status: IssueStatus,
}

/// A dependency list and how much of it the daemon has paged in.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct DepList {
    /// The daemon has an earlier page of this list.
    pub has_previous: bool,
    /// The daemon has a later page of this list.
    pub has_next: bool,
    pub items: Vec<Dep>,
    /// Why the list is empty and not yet known (loading or an error).
    pub note: Option<String>,
}

/// What the cursor can rest on, in display order: folding sections first, then
/// the related Issues.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DetailItem {
    /// An index into [`split_blocks`] of the body.
    Section(usize),
    /// #2420: a stored handoff record, by its per-Issue sequence.
    Handoff(i64),
    Issue(i64),
}

impl DetailItem {
    /// The key a folding item is remembered under in `detail_expanded`: the
    /// block index of a body section, and for a stored handoff a number counted
    /// down from `usize::MAX`, so the two never meet.
    pub(crate) fn fold_key(self) -> usize {
        match self {
            Self::Section(index) => index,
            Self::Handoff(sequence) => usize::MAX.saturating_sub(sequence.max(0) as usize),
            Self::Issue(number) => number.max(0) as usize,
        }
    }
}

/// Issue numbers the body or title mentions that are not already a dependency
/// and not the Issue itself.
fn mentioned(issue: &Issue, blocked_by: &DepList, blocks: &DepList) -> Vec<i64> {
    let known = |number: i64| {
        number == issue.display_number
            || blocked_by.items.iter().any(|dep| dep.number == number)
            || blocks.items.iter().any(|dep| dep.number == number)
    };
    let mut found = mentions(&issue.title);
    for number in mentions(&issue.body) {
        if !found.contains(&number) {
            found.push(number);
        }
    }
    found.retain(|&number| !known(number));
    found
}

pub(crate) fn items(
    issue: &Issue,
    blocked_by: &DepList,
    blocks: &DepList,
    handoffs: &[IssueHandoffV1],
) -> Vec<DetailItem> {
    let mut items: Vec<DetailItem> = split_blocks(&issue.body)
        .iter()
        .enumerate()
        .filter(|(_, block)| block.kind != BlockKind::Plain)
        .map(|(index, _)| DetailItem::Section(index))
        .collect();
    items.extend(
        handoffs
            .iter()
            .map(|handoff| DetailItem::Handoff(handoff.sequence)),
    );
    items.extend(
        blocked_by
            .items
            .iter()
            .chain(blocks.items.iter())
            .map(|dep| DetailItem::Issue(dep.number)),
    );
    items.extend(
        mentioned(issue, blocked_by, blocks)
            .into_iter()
            .map(DetailItem::Issue),
    );
    items
}

/// The dependency list of the selected Issue from a loaded page.
pub(crate) fn dep_list(
    page: &Option<IssueDependencyPageV1>,
    target: Option<Uuid>,
    issue_id: Uuid,
    error: Option<&str>,
) -> DepList {
    match page.as_ref().filter(|_| target == Some(issue_id)) {
        Some(page) => DepList {
            has_previous: page.previous_cursor.is_some(),
            has_next: page.next_cursor.is_some(),
            items: page
                .items
                .iter()
                .map(|item| Dep {
                    number: item.related_issue.display_number,
                    title: item.related_issue.title.clone(),
                    status: item.related_issue.status,
                })
                .collect(),
            note: None,
        },
        None => DepList {
            has_previous: false,
            has_next: false,
            items: Vec::new(),
            note: Some(error.unwrap_or("loading").to_string()),
        },
    }
}

/// The Issue the inspector is showing, with the same fallback the renderer uses.
pub(crate) fn inspected_issue(state: &IssueWorkspaceState) -> Option<&Issue> {
    let issue_id = state.local.selected_issue_id?;
    state
        .transient
        .selected_issue
        .as_ref()
        .filter(|issue| {
            state.transient.selected_issue_target == Some(issue_id) && issue.id == issue_id
        })
        .or_else(|| {
            state
                .transient
                .current_page
                .as_ref()
                .and_then(|page| page.rows.iter().find(|row| row.issue.id == issue_id))
                .map(|row| &row.issue)
        })
}

pub(crate) fn state_dep_lists(state: &IssueWorkspaceState, issue_id: Uuid) -> (DepList, DepList) {
    use crate::types::IssueWorkspaceDataKind;
    (
        dep_list(
            &state.transient.blocked_by,
            state.transient.blocked_by_target,
            issue_id,
            state.data_error(IssueWorkspaceDataKind::BlockedBy),
        ),
        dep_list(
            &state.transient.blocks,
            state.transient.blocks_target,
            issue_id,
            state.data_error(IssueWorkspaceDataKind::Blocks),
        ),
    )
}

/// The stored handoff records loaded for `issue_id` (none while another Issue's
/// are loaded).
pub(crate) fn state_handoffs(state: &IssueWorkspaceState, issue_id: Uuid) -> &[IssueHandoffV1] {
    if state.transient.handoffs_target == Some(issue_id) {
        &state.transient.handoffs
    } else {
        &[]
    }
}

/// The items of the Issue the inspector is showing (empty when none).
pub(crate) fn state_items(state: &IssueWorkspaceState) -> Vec<DetailItem> {
    let Some(issue) = inspected_issue(state) else {
        return Vec::new();
    };
    let (blocked_by, blocks) = state_dep_lists(state, issue.id);
    items(issue, &blocked_by, &blocks, state_handoffs(state, issue.id))
}

pub(crate) struct DetailInput<'a> {
    pub issue: &'a Issue,
    pub readiness: String,
    pub age: String,
    pub blocked_by: DepList,
    pub blocks: DepList,
    /// #2420: the stored handoff records, oldest first.
    pub handoffs: &'a [IssueHandoffV1],
    /// How many handoff records the Issue holds (the loaded page may be shorter).
    pub handoffs_total: i64,
    /// Why no handoff is shown when the read failed.
    pub handoffs_error: Option<String>,
    pub cursor: usize,
    pub expanded: &'a dyn Fn(usize) -> bool,
    pub width: usize,
    /// The technical facts (ids, versions, sessions, events), shown last.
    pub tail: String,
}

pub(crate) struct DetailModel {
    pub lines: Vec<Line<'static>>,
    /// The first line of each item.
    pub item_lines: Vec<usize>,
}

/// Lay pieces out left to right, breaking between pieces when a line is full.
fn flow(pieces: Vec<(String, Style)>, width: usize, lead: usize) -> Vec<Line<'static>> {
    let sep = " · ";
    let mut lines = Vec::new();
    let mut current: Vec<Span<'static>> = indent(lead);
    let mut col = lead;
    for (text, style) in pieces {
        let w = display_width(&text);
        if col > lead && col + display_width(sep) + w > width {
            lines.push(Line::from(std::mem::replace(&mut current, indent(lead))));
            col = lead;
        } else if col > lead {
            current.push(Span::styled(sep, dim()));
            col += display_width(sep);
        }
        current.push(Span::styled(text, style));
        col += w;
    }
    lines.push(Line::from(current));
    lines
}

fn rule(
    glyph: &str,
    name: &str,
    count: Option<usize>,
    color: ratatui::style::Color,
    width: usize,
) -> Line<'static> {
    let count_text = count.map_or_else(|| "  ".to_string(), |n| format!("  {n}  "));
    session::section_header_line(glyph, name, &count_text, color, theme::overlay_bg(), width)
}

fn relation_row(
    glyph: &str,
    word: &str,
    dep: &Dep,
    selected: bool,
    width: usize,
) -> Vec<Line<'static>> {
    let (status_glyph, status_word, status_color) = status_mark(dep.status);
    let marker = if selected { "›" } else { " " };
    let accent = Style::default()
        .fg(theme::accent())
        .add_modifier(Modifier::BOLD);
    let first = vec![
        Span::styled(marker, accent),
        Span::styled(format!(" {glyph} "), dim()),
        Span::styled(format!("{word:<10}"), dim()),
        Span::styled(format!("#{}", dep.number), ink_style(Ink::Mention)),
        Span::styled(
            format!(" {status_glyph} {status_word} "),
            Style::default().fg(status_color),
        ),
    ];
    let hang = spans_width(&first);
    let title_style = Style::default().fg(theme::text());
    // Too narrow to hang the title beside its metadata: give it its own lines.
    let mut lines = if width.saturating_sub(hang) < 24 {
        let mut lines = vec![Line::from(first)];
        lines.extend(wrap_text(
            &dep.title,
            title_style,
            indent(5),
            indent(5),
            width,
        ));
        lines
    } else {
        wrap_text(&dep.title, title_style, first, indent(hang), width)
    };
    if selected {
        for line in &mut lines {
            *line = std::mem::take(line).style(Style::default().bg(theme::selected_row_bg()));
        }
    }
    lines
}

/// One folding row (a body section or a stored handoff) and what it shows.
struct FoldRow {
    selected: bool,
    open: bool,
    kind_word: &'static str,
    title: String,
    content: Vec<String>,
}

/// Push a folding row: its title line (wrapped, with the collapsed-line count
/// when shut) and, when open, its markdown content under a gutter.
fn push_fold(lines: &mut Vec<Line<'static>>, row: FoldRow, width: usize) {
    let accent_bold = Style::default()
        .fg(theme::accent())
        .add_modifier(Modifier::BOLD);
    let FoldRow {
        selected,
        open,
        kind_word,
        title,
        content,
    } = row;
    let hidden = content
        .iter()
        .filter(|line| !line.trim().is_empty())
        .count();
    let title_row = vec![
        Span::styled(if selected { "›" } else { " " }, accent_bold),
        Span::styled(format!(" {} ", if open { "▾" } else { "▸" }), accent_bold),
        Span::styled(
            format!("{kind_word} "),
            Style::default()
                .fg(theme::text())
                .add_modifier(Modifier::BOLD),
        ),
    ];
    let hang = spans_width(&title_row);
    let suffix = if open {
        String::new()
    } else {
        format!(" (collapsed, {hidden} lines)")
    };
    let mut head_lines = wrap_text(
        &format!("{title}{suffix}").trim_start().to_string(),
        if open {
            Style::default().fg(theme::text())
        } else {
            dim()
        },
        title_row,
        indent(hang),
        width,
    );
    if selected {
        for line in &mut head_lines {
            *line = std::mem::take(line).style(Style::default().bg(theme::selected_row_bg()));
        }
    }
    lines.extend(head_lines);
    if open {
        for line in markdown_lines(&content, width.saturating_sub(4)) {
            let mut spans = vec![Span::styled("   │ ", dim())];
            spans.extend(line.spans);
            lines.push(Line::from(spans));
        }
    }
}

/// Build the whole inspector for one Issue.
pub(crate) fn build(input: &DetailInput<'_>) -> DetailModel {
    let issue = input.issue;
    let width = input.width.max(12);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let accent_bold = Style::default()
        .fg(theme::accent())
        .add_modifier(Modifier::BOLD);

    // Header: state glyph, number, title.
    let (glyph, word, color) = status_mark(issue.status);
    let head = vec![
        Span::styled(format!(" {glyph} "), Style::default().fg(color)),
        Span::styled(
            format!("#{}", issue.display_number),
            Style::default()
                .fg(theme::text())
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
    ];
    let hang = spans_width(&head);
    lines.extend(wrap_text(
        &issue.title,
        Style::default()
            .fg(theme::text())
            .add_modifier(Modifier::BOLD),
        head,
        indent(hang),
        width,
    ));

    // Facts: state, priority, readiness, owner, age. Words beside every colour.
    let (priority, priority_color) = priority_mark(issue.priority);
    let mut facts = vec![
        (word.to_string(), Style::default().fg(color)),
        (priority, Style::default().fg(priority_color)),
        (input.readiness.clone(), Style::default().fg(theme::text())),
        (
            issue
                .assignee
                .clone()
                .unwrap_or_else(|| "unassigned".to_string()),
            dim(),
        ),
        (format!("updated {} ago", input.age), dim()),
    ];
    if issue.archived_at.is_some() {
        facts.insert(
            1,
            (
                "ARCHIVED".to_string(),
                Style::default().fg(theme::warning_status()),
            ),
        );
    }
    lines.extend(flow(facts, width, 3));

    if let Some(kind) = operator_kind(&issue.labels) {
        lines.push(Line::from(vec![
            Span::raw("   "),
            Span::styled(
                format!("{} OPERATOR {}", kind.glyph(), kind.word().to_uppercase()),
                Style::default()
                    .fg(theme::warning_status())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                match kind {
                    OperatorKind::Request => " · asked for by the operator",
                    OperatorKind::Decision => " · waiting on the operator",
                },
                dim(),
            ),
        ]));
    }
    if !issue.labels.is_empty() {
        let pieces = issue
            .labels
            .iter()
            .map(|label| {
                let operator = label == OPERATOR_REQUEST_LABEL || label == OPERATOR_DECISION_LABEL;
                (
                    label.clone(),
                    if operator {
                        Style::default()
                            .fg(theme::warning_status())
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(theme::count_text())
                    },
                )
            })
            .collect::<Vec<_>>();
        let mut label_lines = flow(pieces, width, 3);
        if let Some(first) = label_lines.first_mut() {
            first.spans.insert(1, Span::styled("labels ", dim()));
        }
        lines.extend(label_lines);
    }

    // Body and its folding sections.
    let blocks = split_blocks(&issue.body);
    lines.push(Line::default());
    lines.push(rule(glyphs::ARTIFACT, "BODY", None, theme::accent(), width));
    let mut body_lines: Vec<String> = Vec::new();
    for block in blocks.iter().filter(|b| b.kind == BlockKind::Plain) {
        if block.rule_before && !body_lines.is_empty() {
            body_lines.push(String::new());
            body_lines.push("---".to_string());
            body_lines.push(String::new());
        }
        if let Some(text) = &block.heading {
            body_lines.push(format!("{} {text}", "#".repeat(block.level.max(1))));
        }
        body_lines.extend(block.lines.iter().cloned());
        body_lines.push(String::new());
    }
    let rendered = markdown_lines(&body_lines, width.saturating_sub(1));
    if rendered.is_empty() {
        lines.push(Line::from(Span::styled(" No description.", dim())));
    } else {
        lines.extend(rendered.into_iter().map(|line| {
            let mut spans = vec![Span::raw(" ")];
            spans.extend(line.spans);
            Line::from(spans)
        }));
    }

    let mut all_items: Vec<DetailItem> = Vec::new();
    let mut item_lines: Vec<usize> = Vec::new();
    let folding: Vec<(usize, &Block)> = blocks
        .iter()
        .enumerate()
        .filter(|(_, block)| block.kind != BlockKind::Plain)
        .collect();
    if !folding.is_empty() || !input.handoffs.is_empty() {
        lines.push(Line::default());
        lines.push(rule(
            glyphs::HANDOFF,
            "HANDOFFS AND REVIEWS",
            Some(folding.len() + input.handoffs.len()),
            theme::accent(),
            width,
        ));
        for (index, block) in folding {
            let item_index = all_items.len();
            let selected = input.cursor == item_index;
            let item = DetailItem::Section(index);
            let open = (input.expanded)(item.fold_key());
            all_items.push(item);
            item_lines.push(lines.len());
            let kind_word = if block.kind == BlockKind::Review {
                "REVIEW"
            } else {
                "HANDOFF"
            };
            // `## Handoff` under a HANDOFF label would only say it twice.
            let title = block.title();
            let title = if title.eq_ignore_ascii_case(kind_word) {
                String::new()
            } else {
                title
            };
            // A block with no heading is named by its first line; show the rest.
            push_fold(
                &mut lines,
                FoldRow {
                    selected,
                    open,
                    kind_word,
                    title,
                    content: block.content(),
                },
                width,
            );
        }
        // #2420: the stored handoff records, oldest first.
        for handoff in input.handoffs {
            let selected = input.cursor == all_items.len();
            let item = DetailItem::Handoff(handoff.sequence);
            let open = (input.expanded)(item.fold_key());
            all_items.push(item);
            item_lines.push(lines.len());
            push_fold(
                &mut lines,
                FoldRow {
                    selected,
                    open,
                    kind_word: "HANDOFF",
                    title: format!(
                        "#{} · {} ago · session {}",
                        handoff.sequence,
                        crate::ui::issues::issue_age(handoff.created_at),
                        &handoff.author_session_id.to_string()[..8],
                    ),
                    content: handoff.body.lines().map(str::to_string).collect(),
                },
                width,
            );
        }
        let held = usize::try_from(input.handoffs_total).unwrap_or(0);
        if held > input.handoffs.len() {
            lines.push(Line::from(Span::styled(
                format!(
                    "   showing the first {} of {held} stored handoffs",
                    input.handoffs.len()
                ),
                dim(),
            )));
        }
    }
    if input.handoffs.is_empty()
        && let Some(error) = &input.handoffs_error
    {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            format!(" Stored handoffs unavailable: {error}"),
            Style::default().fg(theme::warning_status()),
        )));
    }

    // Related Issues.
    let mentioned_numbers = mentioned(issue, &input.blocked_by, &input.blocks);
    let related_count =
        input.blocked_by.items.len() + input.blocks.items.len() + mentioned_numbers.len();
    lines.push(Line::default());
    lines.push(rule(
        glyphs::NEXT,
        "RELATED",
        Some(related_count),
        theme::accent(),
        width,
    ));
    let dep_block = |label: &str,
                     glyph: &str,
                     word: &str,
                     list: &DepList,
                     lines: &mut Vec<Line<'static>>,
                     all_items: &mut Vec<DetailItem>,
                     item_lines: &mut Vec<usize>| {
        lines.push(Line::from(vec![
            Span::raw(" "),
            Span::styled(label.to_string(), dim()),
        ]));
        if list.items.is_empty() {
            lines.push(Line::from(Span::styled(
                format!(
                    "   {}",
                    list.note.clone().unwrap_or_else(|| "none".to_string())
                ),
                dim(),
            )));
        }
        for dep in &list.items {
            let selected = input.cursor == all_items.len();
            all_items.push(DetailItem::Issue(dep.number));
            item_lines.push(lines.len());
            lines.extend(relation_row(glyph, word, dep, selected, width));
        }
        // Paging affordance: only when the daemon has another page.
        let hint = match (list.has_previous, list.has_next) {
            (true, true) => Some("‹ earlier · more ›"),
            (true, false) => Some("‹ earlier"),
            (false, true) => Some("more ›"),
            (false, false) => None,
        };
        if let Some(hint) = hint {
            lines.push(Line::from(Span::styled(format!("   {hint}"), dim())));
        }
    };
    dep_block(
        "Blocked by",
        "⊘",
        "blocked by",
        &input.blocked_by,
        &mut lines,
        &mut all_items,
        &mut item_lines,
    );
    dep_block(
        "Blocks",
        glyphs::NEXT,
        "blocks",
        &input.blocks,
        &mut lines,
        &mut all_items,
        &mut item_lines,
    );
    if !mentioned_numbers.is_empty() {
        lines.push(Line::from(vec![
            Span::raw(" "),
            Span::styled("Mentioned", dim()),
        ]));
        for number in mentioned_numbers {
            let selected = input.cursor == all_items.len();
            all_items.push(DetailItem::Issue(number));
            item_lines.push(lines.len());
            let mut line = Line::from(vec![
                Span::styled(if selected { "›" } else { " " }, accent_bold),
                Span::styled(" ↗ ", dim()),
                Span::styled(format!("{:<10}", "mentioned"), dim()),
                Span::styled(format!("#{number}"), ink_style(Ink::Mention)),
            ]);
            if selected {
                line = line.style(Style::default().bg(theme::selected_row_bg()));
            }
            lines.push(line);
        }
    }

    // The technical facts last, quieter.
    lines.push(Line::default());
    lines.push(rule(
        glyphs::SESSION_ID,
        "DETAILS",
        None,
        theme::overlay1(),
        width,
    ));
    for raw in input.tail.lines() {
        lines.extend(wrap_text(raw, dim(), indent(1), indent(3), width));
    }

    DetailModel { lines, item_lines }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue(body: &str, labels: &[&str]) -> Issue {
        let now = chrono::Utc::now();
        Issue {
            id: Uuid::new_v4(),
            project_id: Uuid::new_v4(),
            display_number: 2381,
            title: "Redesign the issue tracker so any ticket is found by number".to_string(),
            body: body.to_string(),
            priority: Some(1),
            labels: labels.iter().map(|label| label.to_string()).collect(),
            assignee: None,
            status: IssueStatus::Open,
            created_by_session_id: None,
            idea_id: None,
            source_event_id: None,
            source_finding_ref: None,
            created_at: now,
            updated_at: now,
            closed_at: None,
            archived_at: None,
            row_version: 1,
        }
    }

    const BODY: &str = "Intro with **bold**, `code` and a link to #3.\n\n\
## Acceptance\n- first criterion that is long enough to wrap around a narrow pane\n- second\n\n\
---\nWorker launched for this Issue.\n\n\
## Handoff\nRESULT abc123 slice A done\n\n### Tests\nall green\n\n\
## Review\nREVIEW verdict: approve\n";

    fn text(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn model_with(
        issue: &Issue,
        blocked_by: DepList,
        blocks: DepList,
        width: usize,
    ) -> DetailModel {
        build(&DetailInput {
            issue,
            readiness: "● Ready".to_string(),
            age: "2d".to_string(),
            blocked_by,
            blocks,
            handoffs: &[],
            handoffs_total: 0,
            handoffs_error: None,
            cursor: 0,
            expanded: &|_| false,
            width,
            tail: "UUID: x\nRow version: 1".to_string(),
        })
    }

    #[test]
    fn related_section_without_further_pages_shows_headings_and_none() {
        let issue = issue("plain body", &[]);
        let rendered = text(&model_with(&issue, DepList::default(), DepList::default(), 70).lines);
        assert!(rendered.contains("Blocked by\n   none"), "{rendered}");
        assert!(rendered.contains("Blocks\n   none"), "{rendered}");
    }

    #[test]
    fn related_section_with_a_next_page_shows_the_paging_hint() {
        let issue = issue("plain body", &[]);
        let paged = DepList {
            has_next: true,
            items: vec![Dep {
                number: 3,
                title: "t".to_string(),
                status: IssueStatus::Open,
            }],
            ..DepList::default()
        };
        let rendered = text(&model_with(&issue, paged, DepList::default(), 70).lines);
        assert!(rendered.contains("more ›"), "{rendered}");
        assert!(rendered.contains("Blocks\n   none"), "{rendered}");
    }

    fn model(issue: &Issue, cursor: usize, open: &[usize], width: usize) -> DetailModel {
        let expanded = |index: usize| open.contains(&index);
        build(&DetailInput {
            issue,
            readiness: "● Ready".to_string(),
            age: "2d".to_string(),
            blocked_by: DepList {
                items: vec![Dep {
                    number: 3,
                    title: "Search matches number, title, labels and body text".to_string(),
                    status: IssueStatus::Closed,
                }],
                note: None,
                ..DepList::default()
            },
            blocks: DepList::default(),
            handoffs: &[],
            handoffs_total: 0,
            handoffs_error: None,
            cursor,
            expanded: &expanded,
            width,
            tail: "UUID: x\nRow version: 1".to_string(),
        })
    }

    fn stored_handoff(sequence: i64, body: &str) -> IssueHandoffV1 {
        IssueHandoffV1 {
            sequence,
            author_session_id: Uuid::parse_str("abcdef12-0000-4000-8000-000000000000").unwrap(),
            body: body.to_string(),
            created_at: chrono::Utc::now(),
        }
    }

    fn model_with_handoffs(
        issue: &Issue,
        handoffs: &[IssueHandoffV1],
        total: i64,
        cursor: usize,
        open: &[usize],
    ) -> DetailModel {
        let expanded = |index: usize| open.contains(&index);
        build(&DetailInput {
            issue,
            readiness: "● Ready".to_string(),
            age: "2d".to_string(),
            blocked_by: DepList::default(),
            blocks: DepList::default(),
            handoffs,
            handoffs_total: total,
            handoffs_error: None,
            cursor,
            expanded: &expanded,
            width: 70,
            tail: "UUID: x\nRow version: 1".to_string(),
        })
    }

    #[test]
    fn mentions_find_issue_numbers_outside_code_and_words() {
        assert_eq!(
            mentions("see #3, issue:#5 and (#12). abc#4 no\n```\n#99\n```\nthen #3 again"),
            vec![3, 5, 12]
        );
        assert!(mentions("```\n#7\n```").is_empty());
    }

    #[test]
    fn markdown_wraps_every_line_inside_the_width() {
        let source: Vec<String> = BODY.lines().map(str::to_string).collect();
        for width in [20, 40, 80] {
            for line in markdown_lines(&source, width) {
                assert!(line.width() <= width, "width {width}: {line:?}");
            }
        }
        let rendered = text(&markdown_lines(&source, 30));
        assert!(rendered.contains("• first criterion"));
        assert!(rendered.contains("bold"));
        assert!(!rendered.contains("**"));
        assert!(rendered.contains("`code`"));
    }

    #[test]
    fn handoffs_and_reviews_fold_and_keep_their_subheadings() {
        let blocks = split_blocks(BODY);
        let kinds: Vec<(Option<&str>, BlockKind)> = blocks
            .iter()
            .map(|block| (block.heading.as_deref(), block.kind))
            .collect();
        assert_eq!(
            kinds,
            vec![
                (None, BlockKind::Plain),
                (Some("Acceptance"), BlockKind::Plain),
                (None, BlockKind::Plain),
                (Some("Handoff"), BlockKind::Handoff),
                (Some("Review"), BlockKind::Review),
            ]
        );
        assert!(blocks[3].lines.iter().any(|line| line.contains("Tests")));
        // A headingless block named by its first line is a handoff too.
        let bare = split_blocks("---\nRESULT deadbeef landed\nmore\n");
        assert_eq!(bare[0].kind, BlockKind::Handoff);
    }

    #[test]
    fn items_list_sections_then_dependencies_then_mentions_once() {
        let issue = issue(BODY, &[]);
        let blocked_by = DepList {
            items: vec![Dep {
                number: 3,
                title: "t".to_string(),
                status: IssueStatus::Open,
            }],
            ..DepList::default()
        };
        let mut body = BODY.to_string();
        body.push_str("\nAlso #8 and #2381 itself.\n");
        let issue = Issue { body, ..issue };
        assert_eq!(
            items(&issue, &blocked_by, &DepList::default(), &[]),
            vec![
                DetailItem::Section(3),
                DetailItem::Section(4),
                DetailItem::Issue(3),
                DetailItem::Issue(8),
            ]
        );
    }

    #[test]
    fn folded_sections_hide_their_text_until_opened() {
        let issue = issue(BODY, &["operator-request"]);
        let folded = text(&model(&issue, 0, &[], 70).lines);
        assert!(folded.contains("▸ HANDOFF"));
        assert!(folded.contains("(collapsed"));
        assert!(!folded.contains("all green"));
        let opened = text(&model(&issue, 0, &[3], 70).lines);
        assert!(opened.contains("▾ HANDOFF"));
        assert!(opened.contains("all green"));
        assert!(opened.contains("▸ REVIEW"));
    }

    #[test]
    fn stored_handoffs_are_collapsed_items_and_open_with_their_fold_key() {
        let issue = issue("plain body", &[]);
        let handoffs = [
            stored_handoff(1, "RESULT first landed\nsecond line"),
            stored_handoff(2, "RESULT second landed"),
        ];
        let blocked_by = DepList::default();
        // Each stored record is its own item, after the body sections.
        assert_eq!(
            items(&issue, &blocked_by, &blocked_by, &handoffs),
            vec![DetailItem::Handoff(1), DetailItem::Handoff(2)]
        );
        let folded = model_with_handoffs(&issue, &handoffs, 2, 0, &[]);
        let rendered = text(&folded.lines);
        assert!(rendered.contains("HANDOFFS AND REVIEWS"), "{rendered}");
        assert!(rendered.contains("▸ HANDOFF #1"), "{rendered}");
        assert!(rendered.contains("session abcdef12"), "{rendered}");
        assert!(rendered.contains("(collapsed, 2 lines)"), "{rendered}");
        assert!(!rendered.contains("first landed"), "{rendered}");
        assert!(!rendered.contains("second landed"), "{rendered}");
        assert_eq!(folded.item_lines.len(), 2);
        // `o` on the first record records its fold key; only that record opens.
        let key = DetailItem::Handoff(1).fold_key();
        assert_ne!(key, DetailItem::Handoff(2).fold_key());
        let opened = text(&model_with_handoffs(&issue, &handoffs, 2, 0, &[key]).lines);
        assert!(opened.contains("▾ HANDOFF #1"), "{opened}");
        assert!(opened.contains("first landed"), "{opened}");
        assert!(opened.contains("second line"), "{opened}");
        assert!(opened.contains("▸ HANDOFF #2"), "{opened}");
        assert!(!opened.contains("second landed"), "{opened}");
    }

    #[test]
    fn a_stored_handoff_never_shares_a_fold_key_with_a_body_section() {
        let issue = issue(BODY, &[]);
        let handoffs = [stored_handoff(1, "RESULT stored")];
        let blocked_by = DepList::default();
        assert_eq!(
            items(&issue, &blocked_by, &blocked_by, &handoffs),
            vec![
                DetailItem::Section(3),
                DetailItem::Section(4),
                DetailItem::Handoff(1),
                // BODY mentions #3.
                DetailItem::Issue(3)
            ]
        );
        // Opening body section 3 leaves the stored record shut.
        let opened = text(&model_with_handoffs(&issue, &handoffs, 1, 0, &[3]).lines);
        assert!(opened.contains("all green"), "{opened}");
        assert!(!opened.contains("RESULT stored"), "{opened}");
        assert!(opened.contains("▸ HANDOFF #1"), "{opened}");
    }

    #[test]
    fn a_truncated_handoff_page_says_how_many_are_stored() {
        let issue = issue("plain body", &[]);
        let handoffs = [stored_handoff(1, "RESULT one")];
        let rendered = text(&model_with_handoffs(&issue, &handoffs, 40, 0, &[]).lines);
        assert!(
            rendered.contains("showing the first 1 of 40 stored handoffs"),
            "{rendered}"
        );
    }

    #[test]
    fn the_detail_names_state_priority_operator_kind_and_related_issues_in_words() {
        let issue = issue(BODY, &["operator-decision", "tui"]);
        let detail = model(&issue, 2, &[], 80);
        let rendered = text(&detail.lines);
        assert!(rendered.contains("#2381"));
        assert!(rendered.contains("Open"));
        assert!(rendered.contains("P1"));
        assert!(rendered.contains("? OPERATOR DECISION"));
        assert!(rendered.contains("tui"));
        assert!(rendered.contains("RELATED"));
        assert!(rendered.contains("blocked by"));
        assert!(rendered.contains("#3"));
        assert!(rendered.contains("Closed"));
        assert!(rendered.contains("Row version: 1"));
        // The cursor rests on the related Issue (after the two sections).
        assert_eq!(detail.item_lines.len(), 3);
        let cursor_line = &detail.lines[detail.item_lines[2]];
        assert!(text(std::slice::from_ref(cursor_line)).contains('›'));
        for line in &detail.lines {
            assert!(line.width() <= 80, "{line:?}");
        }
    }

    #[test]
    fn every_state_has_its_own_glyph_and_word() {
        let marks: Vec<_> = [
            IssueStatus::Open,
            IssueStatus::InProgress,
            IssueStatus::Closed,
            IssueStatus::Cancelled,
        ]
        .into_iter()
        .map(|status| {
            let (glyph, word, _) = status_mark(status);
            (glyph, word)
        })
        .collect();
        for (index, mark) in marks.iter().enumerate() {
            assert!(!mark.1.is_empty());
            assert!(
                marks.iter().skip(index + 1).all(|other| other.0 != mark.0),
                "{mark:?} shares its glyph"
            );
        }
        assert_ne!(
            OperatorKind::Request.glyph(),
            OperatorKind::Decision.glyph()
        );
        assert_eq!(priority_mark(Some(1)).0, "P1");
        assert_eq!(priority_mark(None).0, "P-");
    }

    #[test]
    fn a_decision_outranks_a_request_when_both_labels_are_present() {
        let labels = vec![
            OPERATOR_REQUEST_LABEL.to_string(),
            OPERATOR_DECISION_LABEL.to_string(),
        ];
        assert_eq!(operator_kind(&labels), Some(OperatorKind::Decision));
        assert_eq!(operator_kind(&["bug".to_string()]), None);
    }

    #[test]
    fn colours_come_only_from_theme_roles() {
        // Contrast is the theme's job (its high-contrast palette included): the
        // tracker may only ask a theme role for a colour, never name one.
        for source in [include_str!("issue_detail.rs"), include_str!("issues.rs")] {
            let production = source.split("#[cfg(test)]").next().unwrap_or_default();
            assert!(!production.contains("Color::"));
            assert!(!production.contains("Color::Rgb"));
        }
    }

    #[test]
    fn every_new_tracker_action_has_a_key_shown_in_help() {
        use crate::action_registry::{ACTION_DESCRIPTORS, ActionId};
        for id in [
            ActionId::IssueGoToNumber,
            ActionId::IssueOperatorFilter,
            ActionId::IssueDetailNext,
            ActionId::IssueDetailPrevious,
            ActionId::IssueDetailActivate,
            ActionId::IssueDetailBack,
        ] {
            let descriptor = ACTION_DESCRIPTORS
                .iter()
                .find(|descriptor| descriptor.id == id)
                .expect("descriptor");
            assert!(descriptor.show_in_help, "{id:?}");
            assert!(!descriptor.bindings.is_empty(), "{id:?}");
            assert!(!descriptor.summary.is_empty(), "{id:?}");
        }
    }
}
