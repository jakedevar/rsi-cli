//! Tree-sitter syntax highlighting for the file viewer.
//!
//! Grammar configurations are created once at first use and shared via Arc.
//! Highlighter instances are NOT Send, so we use thread_local! storage.
//! The public API is highlight_file() which returns styled ratatui Lines.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter};

use super::theme;

/// Canonical highlight capture names recognised by all grammar .scm files.
/// Index into this slice == the usize carried by HighlightEvent::HighlightStart.
pub const HIGHLIGHT_NAMES: &[&str] = &[
    "attribute",             // 0
    "comment",               // 1
    "constant",              // 2
    "constant.builtin",      // 3
    "constructor",           // 4
    "embedded",              // 5
    "function",              // 6
    "function.builtin",      // 7
    "keyword",               // 8
    "module",                // 9
    "number",                // 10
    "operator",              // 11
    "property",              // 12
    "property.builtin",      // 13
    "punctuation",           // 14
    "punctuation.bracket",   // 15
    "punctuation.delimiter", // 16
    "punctuation.special",   // 17
    "string",                // 18
    "string.special",        // 19
    "tag",                   // 20
    "type",                  // 21
    "type.builtin",          // 22
    "variable",              // 23
    "variable.builtin",      // 24
    "variable.parameter",    // 25
];

/// Map a tree-sitter highlight name to a ratatui Style using the active theme.
///
/// Called once per unique highlight name during rendering. Callers should not
/// cache the result across frames -- theme switches take effect immediately.
pub fn ts_highlight_to_style(name: &str) -> Style {
    let color = match name {
        "keyword" => theme::mauve(),
        "function" | "function.builtin" => theme::blue(),
        "string" | "string.special" => theme::green(),
        "number" => theme::peach(),
        "type" | "type.builtin" => theme::yellow(),
        "variable" => theme::text(),
        "variable.builtin" => theme::red(),
        "variable.parameter" => theme::maroon(),
        "comment" => theme::overlay0(),
        "operator" => theme::sky(),
        "constant" | "constant.builtin" => theme::peach(),
        "property" | "property.builtin" => theme::lavender(),
        "punctuation" | "punctuation.bracket" | "punctuation.delimiter" | "punctuation.special" => {
            theme::overlay1()
        }
        "attribute" => theme::yellow(),
        "module" => theme::yellow(),
        "constructor" => theme::sapphire(),
        "tag" => theme::red(),
        "embedded" => theme::text(),
        _ => theme::text(),
    };
    Style::default().fg(color)
}

/// Lazily-initialised registry mapping file extension to HighlightConfiguration.
///
/// Configurations are built once on first access per language. The OnceLock
/// ensures the registry itself is initialised exactly once.
static LANGUAGE_REGISTRY: OnceLock<LanguageRegistry> = OnceLock::new();

pub struct LanguageRegistry {
    /// Extension -> Arc<HighlightConfiguration>
    configs: HashMap<&'static str, Arc<HighlightConfiguration>>,
}

impl LanguageRegistry {
    fn build() -> Self {
        let mut configs = HashMap::new();

        // Each entry: file extension(s) -> grammar + highlight query
        type BuildFn = fn() -> HighlightConfiguration;
        let entries: &[(&[&str], BuildFn)] = &[
            (&["rs"], build_rust_config),
            (&["py", "pyi"], build_python_config),
            (&["js", "mjs", "cjs"], build_javascript_config),
            (&["ts", "mts", "cts"], build_typescript_config),
            (&["tsx"], build_tsx_config),
            (&["sh", "bash", "zsh", "fish"], build_bash_config),
            (&["json", "jsonc"], build_json_config),
            (&["toml"], build_toml_config),
            (&["yaml", "yml"], build_yaml_config),
            (&["go"], build_go_config),
            (&["c", "h"], build_c_config),
            (&["cpp", "cc", "cxx", "hpp", "hxx"], build_cpp_config),
            (&["md", "markdown"], build_markdown_config),
        ];

        for (exts, build_fn) in entries {
            let config = Arc::new(build_fn());
            for ext in *exts {
                configs.insert(*ext, Arc::clone(&config));
            }
        }

        Self { configs }
    }

    pub fn get(&self, extension: &str) -> Option<&Arc<HighlightConfiguration>> {
        self.configs.get(extension)
    }
}

fn registry() -> &'static LanguageRegistry {
    LANGUAGE_REGISTRY.get_or_init(LanguageRegistry::build)
}

// --- Grammar builder functions ---

fn build_rust_config() -> HighlightConfiguration {
    let mut config = HighlightConfiguration::new(
        tree_sitter_rust::LANGUAGE.into(),
        "rust",
        tree_sitter_rust::HIGHLIGHTS_QUERY,
        tree_sitter_rust::INJECTIONS_QUERY,
        "",
    )
    .expect("Rust highlight configuration is valid");
    config.configure(HIGHLIGHT_NAMES);
    config
}

fn build_python_config() -> HighlightConfiguration {
    let mut config = HighlightConfiguration::new(
        tree_sitter_python::LANGUAGE.into(),
        "python",
        tree_sitter_python::HIGHLIGHTS_QUERY,
        "",
        "",
    )
    .expect("Python highlight configuration is valid");
    config.configure(HIGHLIGHT_NAMES);
    config
}

fn build_javascript_config() -> HighlightConfiguration {
    let mut config = HighlightConfiguration::new(
        tree_sitter_javascript::LANGUAGE.into(),
        "javascript",
        tree_sitter_javascript::HIGHLIGHT_QUERY,
        tree_sitter_javascript::INJECTIONS_QUERY,
        tree_sitter_javascript::LOCALS_QUERY,
    )
    .expect("JavaScript highlight configuration is valid");
    config.configure(HIGHLIGHT_NAMES);
    config
}

fn build_typescript_config() -> HighlightConfiguration {
    // TypeScript highlights = JS highlights + TS-specific highlights
    let combined_highlights = format!(
        "{}\n{}",
        tree_sitter_javascript::HIGHLIGHT_QUERY,
        tree_sitter_typescript::HIGHLIGHTS_QUERY,
    );
    let mut config = HighlightConfiguration::new(
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        "typescript",
        &combined_highlights,
        "",
        tree_sitter_typescript::LOCALS_QUERY,
    )
    .expect("TypeScript highlight configuration is valid");
    config.configure(HIGHLIGHT_NAMES);
    config
}

fn build_tsx_config() -> HighlightConfiguration {
    let combined_highlights = format!(
        "{}\n{}\n{}",
        tree_sitter_javascript::HIGHLIGHT_QUERY,
        tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
        tree_sitter_typescript::HIGHLIGHTS_QUERY,
    );
    let mut config = HighlightConfiguration::new(
        tree_sitter_typescript::LANGUAGE_TSX.into(),
        "tsx",
        &combined_highlights,
        "",
        tree_sitter_typescript::LOCALS_QUERY,
    )
    .expect("TSX highlight configuration is valid");
    config.configure(HIGHLIGHT_NAMES);
    config
}

fn build_bash_config() -> HighlightConfiguration {
    let mut config = HighlightConfiguration::new(
        tree_sitter_bash::LANGUAGE.into(),
        "bash",
        tree_sitter_bash::HIGHLIGHT_QUERY,
        "",
        "",
    )
    .expect("Bash highlight configuration is valid");
    config.configure(HIGHLIGHT_NAMES);
    config
}

fn build_json_config() -> HighlightConfiguration {
    let mut config = HighlightConfiguration::new(
        tree_sitter_json::LANGUAGE.into(),
        "json",
        tree_sitter_json::HIGHLIGHTS_QUERY,
        "",
        "",
    )
    .expect("JSON highlight configuration is valid");
    config.configure(HIGHLIGHT_NAMES);
    config
}

fn build_toml_config() -> HighlightConfiguration {
    let mut config = HighlightConfiguration::new(
        tree_sitter_toml_ng::LANGUAGE.into(),
        "toml",
        tree_sitter_toml_ng::HIGHLIGHTS_QUERY,
        "",
        "",
    )
    .expect("TOML highlight configuration is valid");
    config.configure(HIGHLIGHT_NAMES);
    config
}

fn build_yaml_config() -> HighlightConfiguration {
    let mut config = HighlightConfiguration::new(
        tree_sitter_yaml::LANGUAGE.into(),
        "yaml",
        tree_sitter_yaml::HIGHLIGHTS_QUERY,
        "",
        "",
    )
    .expect("YAML highlight configuration is valid");
    config.configure(HIGHLIGHT_NAMES);
    config
}

fn build_go_config() -> HighlightConfiguration {
    let mut config = HighlightConfiguration::new(
        tree_sitter_go::LANGUAGE.into(),
        "go",
        tree_sitter_go::HIGHLIGHTS_QUERY,
        "",
        "",
    )
    .expect("Go highlight configuration is valid");
    config.configure(HIGHLIGHT_NAMES);
    config
}

fn build_c_config() -> HighlightConfiguration {
    let mut config = HighlightConfiguration::new(
        tree_sitter_c::LANGUAGE.into(),
        "c",
        tree_sitter_c::HIGHLIGHT_QUERY,
        "",
        "",
    )
    .expect("C highlight configuration is valid");
    config.configure(HIGHLIGHT_NAMES);
    config
}

fn build_cpp_config() -> HighlightConfiguration {
    let combined_highlights = format!(
        "{}\n{}",
        tree_sitter_c::HIGHLIGHT_QUERY,
        tree_sitter_cpp::HIGHLIGHT_QUERY,
    );
    let mut config = HighlightConfiguration::new(
        tree_sitter_cpp::LANGUAGE.into(),
        "cpp",
        &combined_highlights,
        "",
        "",
    )
    .expect("C++ highlight configuration is valid");
    config.configure(HIGHLIGHT_NAMES);
    config
}

fn build_markdown_config() -> HighlightConfiguration {
    // tree-sitter-md has separate block and inline grammars. The LANGUAGE
    // export is the block grammar, so we can only use HIGHLIGHT_QUERY_BLOCK.
    // The inline queries reference node types (e.g. code_span) that only
    // exist in INLINE_LANGUAGE and would cause a QueryError here.
    let mut config = HighlightConfiguration::new(
        tree_sitter_md::LANGUAGE.into(),
        "markdown",
        tree_sitter_md::HIGHLIGHT_QUERY_BLOCK,
        "",
        "",
    )
    .expect("Markdown highlight configuration is valid");
    config.configure(HIGHLIGHT_NAMES);
    config
}

// --- Thread-local Highlighter ---

thread_local! {
    static HIGHLIGHTER: std::cell::RefCell<Highlighter> =
        std::cell::RefCell::new(Highlighter::new());
}

/// Highlight `content` using the grammar for `extension` (e.g. "rs", "py").
///
/// Returns one `Line<'static>` per source line, with ratatui spans carrying
/// Catppuccin colors from the active theme. Returns plain (unstyled) lines if
/// the extension is unknown or tree-sitter fails.
///
/// For large files, pass `viewport_start..viewport_end` line range to limit
/// work to the visible area. Pass `0..usize::MAX` to highlight everything.
pub fn highlight_file(
    content: &str,
    extension: &str,
    viewport: std::ops::Range<usize>,
) -> Vec<Line<'static>> {
    let Some(config) = registry().get(extension) else {
        return plain_lines(content, viewport);
    };

    HIGHLIGHTER.with(|h| {
        let mut highlighter = h.borrow_mut();
        match highlighter.highlight(config, content.as_bytes(), None, |_| None) {
            Ok(events) => build_highlighted_lines(content, events, viewport),
            Err(_) => plain_lines(content, viewport),
        }
    })
}

/// Fallback: return plain Lines with default style for each line in viewport.
fn plain_lines(content: &str, viewport: std::ops::Range<usize>) -> Vec<Line<'static>> {
    content
        .lines()
        .enumerate()
        .filter(|(i, _)| viewport.contains(i))
        .map(|(_, text)| Line::from(Span::raw(text.to_owned())))
        .collect()
}

/// Convert a stream of HighlightEvents into ratatui Lines, restricted to viewport.
fn build_highlighted_lines(
    source: &str,
    events: impl Iterator<Item = Result<HighlightEvent, tree_sitter_highlight::Error>>,
    viewport: std::ops::Range<usize>,
) -> Vec<Line<'static>> {
    // Build a flat list of (byte_range, style) pairs from the event stream,
    // then reconstruct per-line spans.
    //
    // Style stack: HighlightStart pushes, HighlightEnd pops.
    // Source events carry byte ranges -- we accumulate (start, end, style) triples.
    let mut style_stack: Vec<Style> = Vec::new();
    let mut styled_ranges: Vec<(usize, usize, Style)> = Vec::new();

    for event in events.flatten() {
        match event {
            HighlightEvent::HighlightStart(hl) => {
                let name = HIGHLIGHT_NAMES.get(hl.0).copied().unwrap_or("");
                style_stack.push(ts_highlight_to_style(name));
            }
            HighlightEvent::HighlightEnd => {
                style_stack.pop();
            }
            HighlightEvent::Source { start, end } => {
                let style = style_stack.last().copied().unwrap_or_default();
                styled_ranges.push((start, end, style));
            }
        }
    }

    // Reconstruct per-line Line<'static> from byte-indexed styled ranges.
    // Walk source lines tracking byte offset, build spans per line.
    let mut result = Vec::new();
    let mut byte_offset: usize = 0;
    let mut range_idx: usize = 0;

    for (line_num, line_text) in source.lines().enumerate() {
        let line_start = byte_offset;
        let line_end = byte_offset + line_text.len();
        byte_offset = line_end + 1; // +1 for '\n'

        if line_num >= viewport.end {
            break;
        }
        if line_num < viewport.start {
            continue;
        }

        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut col = line_start;

        // Walk styled_ranges that overlap this line
        while range_idx < styled_ranges.len() {
            let (rs, re, style) = styled_ranges[range_idx];
            if rs >= line_end {
                break; // This range starts on a later line
            }
            if re <= line_start {
                range_idx += 1;
                continue; // This range ends before our line
            }

            let seg_start = rs.max(line_start);
            let seg_end = re.min(line_end);

            if col < seg_start {
                // Gap before this styled range -- plain text
                let gap = &line_text[col - line_start..seg_start - line_start];
                if !gap.is_empty() {
                    spans.push(Span::raw(gap.to_owned()));
                }
            }

            let text = &line_text[seg_start - line_start..seg_end - line_start];
            if !text.is_empty() {
                spans.push(Span::styled(text.to_owned(), style));
            }

            col = seg_end;

            if re >= line_end {
                break; // Range extends beyond this line -- don't advance range_idx
            }
            range_idx += 1;
        }

        // Remaining plain text on the line
        if col < line_end {
            let tail = &line_text[col - line_start..];
            if !tail.is_empty() {
                spans.push(Span::raw(tail.to_owned()));
            }
        }

        result.push(Line::from(spans));
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rust_highlights_keywords() {
        let lines = highlight_file("fn main() { let x = 1; }", "rs", 0..usize::MAX);
        assert!(!lines.is_empty(), "should produce at least one line");
        // The word "fn" should be a styled span, not plain text
        let first = &lines[0];
        let has_styled = first.spans.iter().any(|s| s.style != Style::default());
        assert!(has_styled, "rust code should have styled spans");
    }

    #[test]
    fn test_unknown_extension_returns_plain_lines() {
        let code = "hello world\nline two";
        let lines = highlight_file(code, "xyz_unknown", 0..usize::MAX);
        assert_eq!(lines.len(), 2);
        // All spans should be unstyled (plain fallback)
        for line in &lines {
            for span in &line.spans {
                assert_eq!(span.style, Style::default());
            }
        }
    }

    #[test]
    fn test_viewport_restricts_output() {
        let code = "line0\nline1\nline2\nline3\nline4";
        let lines = highlight_file(code, "xyz_unknown", 1..3);
        assert_eq!(lines.len(), 2, "viewport 1..3 should yield 2 lines");
    }

    #[test]
    fn test_empty_file_is_safe() {
        let lines = highlight_file("", "rs", 0..usize::MAX);
        assert!(lines.is_empty());
    }

    #[test]
    fn test_json_highlights_produce_spans() {
        let json = r#"{"key": "value", "num": 42}"#;
        let lines = highlight_file(json, "json", 0..usize::MAX);
        assert!(!lines.is_empty());
    }
}
