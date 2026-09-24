//! Syntax highlighting for code blocks using syntect + Catppuccin flavors.

use std::sync::OnceLock;

use ratatui::style::Color;
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;
use syntect_tui::into_span;

use super::theme;

/// Global lazy-loaded SyntaxSet (expensive ~50ms init).
static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();

const TMTHEME_LATTE: &[u8] = include_bytes!("../../assets/catppuccin-latte.tmTheme");
const TMTHEME_FRAPPE: &[u8] = include_bytes!("../../assets/catppuccin-frappe.tmTheme");
const TMTHEME_MACCHIATO: &[u8] = include_bytes!("../../assets/catppuccin-macchiato.tmTheme");
const TMTHEME_MOCHA: &[u8] = include_bytes!("../../assets/catppuccin-mocha.tmTheme");
const TMTHEME_GOTH: &[u8] = include_bytes!("../../assets/goth.tmTheme");

const HIGHLIGHT_THEME_COUNT: usize = 5;
/// All registered syntax highlight themes (Latte→Goth order).
static THEMES: OnceLock<[Theme; HIGHLIGHT_THEME_COUNT]> = OnceLock::new();

fn syntax_set() -> &'static SyntaxSet {
    SYNTAX_SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn load_theme(bytes: &[u8]) -> Theme {
    ThemeSet::load_from_reader(&mut std::io::Cursor::new(bytes)).unwrap_or_else(|_| {
        let ts = ThemeSet::load_defaults();
        ts.themes["base16-ocean.dark"].clone()
    })
}

fn themes() -> &'static [Theme; HIGHLIGHT_THEME_COUNT] {
    THEMES.get_or_init(|| {
        [
            load_theme(TMTHEME_LATTE),
            load_theme(TMTHEME_FRAPPE),
            load_theme(TMTHEME_MACCHIATO),
            load_theme(TMTHEME_MOCHA),
            load_theme(TMTHEME_GOTH),
        ]
    })
}

fn active_theme() -> &'static Theme {
    let index = super::theme::highlight_theme_index().min(HIGHLIGHT_THEME_COUNT - 1);
    &themes()[index]
}

fn apply_syntax_background_policy(
    mut span: Span<'static>,
    terminal_default: bool,
) -> Span<'static> {
    if terminal_default {
        span.style = span.style.bg(Color::Reset);
    }
    span
}

/// Highlight code with the given language tag.
///
/// Returns styled `Line`s ready for ratatui rendering.
/// Falls back to plain text with code-block coloring if the language
/// is unrecognized or highlighting fails.
pub fn highlight_code(code: &str, language: Option<&str>) -> Vec<Line<'static>> {
    let ps = syntax_set();
    let theme = active_theme();
    let terminal_default = super::theme::uses_terminal_default_backgrounds();

    let syntax = language
        .and_then(|lang| {
            ps.find_syntax_by_token(lang)
                .or_else(|| ps.find_syntax_by_extension(lang))
        })
        .unwrap_or_else(|| ps.find_syntax_plain_text());

    let mut h = HighlightLines::new(syntax, theme);
    let mut lines = Vec::new();

    for line_text in LinesWithEndings::from(code) {
        match h.highlight_line(line_text, ps) {
            Ok(segments) => {
                let spans: Vec<Span<'static>> = segments
                    .into_iter()
                    .filter_map(|seg| into_span(seg).ok())
                    .map(|s| Span::styled(s.content.into_owned(), s.style))
                    .map(|span| apply_syntax_background_policy(span, terminal_default))
                    .collect();
                if spans.is_empty() {
                    lines.push(Line::from(Span::styled(
                        line_text.trim_end_matches('\n').to_owned(),
                        ratatui::style::Style::default().fg(theme::teal()),
                    )));
                } else {
                    lines.push(Line::from(spans));
                }
            }
            Err(_) => {
                lines.push(Line::from(Span::styled(
                    line_text.trim_end_matches('\n').to_owned(),
                    ratatui::style::Style::default().fg(theme::teal()),
                )));
            }
        }
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_highlight_known_language() {
        let lines = highlight_code("fn main() {}", Some("rust"));
        assert!(!lines.is_empty());
    }

    #[test]
    fn test_highlight_unknown_language_fallback() {
        let lines = highlight_code("some code", Some("nonexistent_lang_xyz"));
        assert!(!lines.is_empty());
    }

    #[test]
    fn test_highlight_no_language() {
        let lines = highlight_code("plain text", None);
        assert!(!lines.is_empty());
    }

    #[test]
    fn test_highlight_multiline() {
        let code = "fn main() {\n    println!(\"hello\");\n}";
        let lines = highlight_code(code, Some("rust"));
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn test_highlight_empty_code() {
        let lines = highlight_code("", Some("rust"));
        assert!(lines.is_empty());
    }

    #[test]
    fn terminal_default_policy_clears_imported_syntax_background() {
        let original = Span::styled(
            "let",
            ratatui::style::Style::default()
                .fg(Color::Green)
                .bg(Color::Red),
        );

        let unchanged = apply_syntax_background_policy(original.clone(), false);
        assert_eq!(unchanged.style.bg, Some(Color::Red));

        let cleared = apply_syntax_background_policy(original, true);
        assert_eq!(cleared.style.fg, Some(Color::Green));
        assert_eq!(cleared.style.bg, Some(Color::Reset));
    }
}
