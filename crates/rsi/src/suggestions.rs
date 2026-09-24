//! Command suggestion types and discovery for autocomplete.

use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;
use std::path::Path;

/// Which kind of suggestion is currently active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuggestionMode {
    /// No suggestions active.
    None,
    /// Slash-command suggestions (existing behavior).
    Command,
    /// File path suggestions triggered by `@`.
    File,
}

/// A known command that can be suggested in the prompt popup.
#[derive(Debug, Clone)]
pub struct CommandSuggestion {
    /// The slash command name without the leading `/` (e.g., "compact").
    pub name: String,
    /// Brief description (e.g., "Compress conversation context").
    pub description: String,
    /// Source of the command (for display differentiation).
    pub source: CommandSource,
}

/// Where a command was discovered from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandSource {
    /// From .claude/commands/*.md
    CustomCommand,
    /// From .claude/skills/*/SKILL.md
    Skill,
}

/// Discover commands from `.claude/commands/` and `.claude/skills/` directories.
///
/// Scans relative to `working_dir` (the cwd when the TUI starts).
pub fn discover_commands(working_dir: &Path) -> Vec<CommandSuggestion> {
    let mut commands = Vec::new();

    // Scan .claude/commands/*.md
    let commands_dir = working_dir.join(".claude/commands");
    if let Ok(entries) = std::fs::read_dir(&commands_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("md")
                && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
            {
                commands.push(CommandSuggestion {
                    name: stem.to_string(),
                    description: "Custom command from .claude/commands/".to_string(),
                    source: CommandSource::CustomCommand,
                });
            }
        }
    }

    // Scan .claude/skills/*/SKILL.md — use directory name as command name
    let skills_dir = working_dir.join(".claude/skills");
    if let Ok(entries) = std::fs::read_dir(&skills_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let skill_file = path.join("SKILL.md");
                if skill_file.exists()
                    && let Some(dir_name) = path.file_name().and_then(|s| s.to_str())
                {
                    commands.push(CommandSuggestion {
                        name: dir_name.to_string(),
                        description: "Skill from .claude/skills/".to_string(),
                        source: CommandSource::Skill,
                    });
                }
            }
        }
    }

    // Sort alphabetically for consistent ordering
    commands.sort_by(|a, b| a.name.cmp(&b.name));
    commands
}

/// A scored suggestion result from fuzzy matching.
#[derive(Debug, Clone)]
pub struct ScoredSuggestion {
    /// Index into the original `Vec<CommandSuggestion>`.
    pub index: usize,
    /// Fuzzy match score (higher = better match).
    pub score: i64,
}

/// Filter and score commands against a query using fuzzy matching.
///
/// Returns indices sorted by score (best match first). Empty query returns all commands.
pub fn filter_suggestions(commands: &[CommandSuggestion], query: &str) -> Vec<ScoredSuggestion> {
    let matcher = SkimMatcherV2::default();

    if query.is_empty() {
        // Show all commands when query is empty (just typed `/`)
        return commands
            .iter()
            .enumerate()
            .map(|(i, _)| ScoredSuggestion { index: i, score: 0 })
            .collect();
    }

    let mut scored: Vec<ScoredSuggestion> = commands
        .iter()
        .enumerate()
        .filter_map(|(i, cmd)| {
            matcher
                .fuzzy_match(&cmd.name, query)
                .map(|score| ScoredSuggestion { index: i, score })
        })
        .collect();

    // Sort by score descending (best matches first)
    scored.sort_by(|a, b| b.score.cmp(&a.score));
    scored
}

/// Check whether suggestions should be active based on textarea content.
///
/// Returns `Some(query)` with the text after `/` if the current line starts with `/`,
/// or `None` if suggestions should not be shown.
pub fn extract_slash_query(textarea: &tui_textarea::TextArea<'_>) -> Option<String> {
    let (row, col) = textarea.cursor();
    let lines = textarea.lines();
    let line = lines.get(row)?;

    // Line must start with `/`
    if !line.starts_with('/') {
        return None;
    }

    // Extract text from after `/` to cursor position. The cursor col is a
    // char index; convert to a byte offset before slicing (multibyte-safe).
    let end = crate::ui::session::byte_offset_of_col(line, col);
    if end < 1 {
        return Some(String::new());
    }
    let query = &line[1..end];
    Some(query.to_string())
}

/// Check whether `@` file suggestions should be active based on textarea content.
///
/// Scans backward from the cursor on the current line to find an `@` that is
/// preceded by a word boundary (space or start of line). Returns `Some(query)`
/// with the text between `@` and the cursor, or `None` if no valid trigger found.
pub fn extract_at_query(textarea: &tui_textarea::TextArea<'_>) -> Option<String> {
    let (row, col) = textarea.cursor();
    let lines = textarea.lines();
    let line = lines.get(row)?;
    // Cursor col is a char index; convert to a byte offset before slicing.
    let end = crate::ui::session::byte_offset_of_col(line, col);

    // Scan backward from cursor to find the `@` trigger
    let before_cursor = &line[..end];
    let at_pos = before_cursor.rfind('@')?;

    // `@` must be at position 0 or preceded by whitespace
    if at_pos > 0 && !line.as_bytes()[at_pos - 1].is_ascii_whitespace() {
        return None;
    }

    // Extract query text between `@` and cursor
    let query = &line[at_pos + 1..end];
    Some(query.to_string())
}

/// Filter and score file paths against a query using fuzzy matching.
///
/// Returns indices sorted by score (best match first). Empty query returns
/// the first `max_results` paths unchanged.
pub fn filter_file_suggestions(
    paths: &[String],
    query: &str,
    max_results: usize,
) -> Vec<ScoredSuggestion> {
    let matcher = SkimMatcherV2::default();

    if query.is_empty() {
        return paths
            .iter()
            .enumerate()
            .take(max_results)
            .map(|(i, _)| ScoredSuggestion { index: i, score: 0 })
            .collect();
    }

    let mut scored: Vec<ScoredSuggestion> = paths
        .iter()
        .enumerate()
        .filter_map(|(i, path)| {
            matcher
                .fuzzy_match(path, query)
                .map(|score| ScoredSuggestion { index: i, score })
        })
        .collect();

    scored.sort_by(|a, b| b.score.cmp(&a.score));
    scored.truncate(max_results);
    scored
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_discover_commands_empty_dir() {
        let temp_dir = std::env::temp_dir().join("rsi_test_empty");
        let _ = std::fs::create_dir(&temp_dir);
        let commands = discover_commands(&temp_dir);
        assert_eq!(commands.len(), 0);
        let _ = std::fs::remove_dir(&temp_dir);
    }

    #[test]
    fn test_filter_suggestions_empty_query() {
        let commands = vec![
            CommandSuggestion {
                name: "foo".to_string(),
                description: "Test".to_string(),
                source: CommandSource::CustomCommand,
            },
            CommandSuggestion {
                name: "bar".to_string(),
                description: "Test".to_string(),
                source: CommandSource::CustomCommand,
            },
        ];
        let results = filter_suggestions(&commands, "");
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_filter_suggestions_exact_match() {
        let commands = vec![
            CommandSuggestion {
                name: "foo".to_string(),
                description: "Test".to_string(),
                source: CommandSource::CustomCommand,
            },
            CommandSuggestion {
                name: "foobar".to_string(),
                description: "Test".to_string(),
                source: CommandSource::CustomCommand,
            },
        ];
        let results = filter_suggestions(&commands, "foo");
        assert!(!results.is_empty());
        // Exact match should score higher
        assert_eq!(commands[results[0].index].name, "foo");
    }

    #[test]
    fn test_filter_suggestions_fuzzy_match() {
        let commands = vec![
            CommandSuggestion {
                name: "review".to_string(),
                description: "Test".to_string(),
                source: CommandSource::CustomCommand,
            },
            CommandSuggestion {
                name: "random".to_string(),
                description: "Test".to_string(),
                source: CommandSource::CustomCommand,
            },
        ];
        let results = filter_suggestions(&commands, "rev");
        assert_eq!(results.len(), 1);
        assert_eq!(commands[results[0].index].name, "review");
    }

    #[test]
    fn test_filter_suggestions_no_match() {
        let commands = vec![CommandSuggestion {
            name: "foo".to_string(),
            description: "Test".to_string(),
            source: CommandSource::CustomCommand,
        }];
        let results = filter_suggestions(&commands, "xyz");
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn test_extract_slash_query_at_start() {
        let mut textarea = tui_textarea::TextArea::default();
        textarea.insert_str("/com");
        let query = extract_slash_query(&textarea);
        assert_eq!(query, Some("com".to_string()));
    }

    #[test]
    fn test_extract_slash_query_no_slash() {
        let mut textarea = tui_textarea::TextArea::default();
        textarea.insert_str("hello");
        let query = extract_slash_query(&textarea);
        assert_eq!(query, None);
    }

    #[test]
    fn test_extract_slash_query_empty_after_slash() {
        let mut textarea = tui_textarea::TextArea::default();
        textarea.insert_str("/");
        let query = extract_slash_query(&textarea);
        assert_eq!(query, Some("".to_string()));
    }

    #[test]
    fn test_extract_slash_query_cursor_at_col_zero() {
        let mut textarea = tui_textarea::TextArea::default();
        textarea.insert_str("/home/jakedevar/docs");
        // Move cursor to column 0 (simulating Shift-I)
        textarea.move_cursor(tui_textarea::CursorMove::Head);
        let query = extract_slash_query(&textarea);
        assert_eq!(query, Some("".to_string()));
    }

    #[test]
    fn test_extract_slash_query_mid_line_slash() {
        let mut textarea = tui_textarea::TextArea::default();
        textarea.insert_str("hello /cmd");
        let query = extract_slash_query(&textarea);
        assert_eq!(query, None);
    }

    // --- extract_at_query tests ---

    #[test]
    fn test_extract_at_query_at_start() {
        let mut textarea = tui_textarea::TextArea::default();
        textarea.insert_str("@src");
        let query = extract_at_query(&textarea);
        assert_eq!(query, Some("src".to_string()));
    }

    #[test]
    fn test_extract_at_query_empty_after_at() {
        let mut textarea = tui_textarea::TextArea::default();
        textarea.insert_str("@");
        let query = extract_at_query(&textarea);
        assert_eq!(query, Some("".to_string()));
    }

    #[test]
    fn test_extract_at_query_mid_line() {
        let mut textarea = tui_textarea::TextArea::default();
        textarea.insert_str("describe @src/main");
        let query = extract_at_query(&textarea);
        assert_eq!(query, Some("src/main".to_string()));
    }

    #[test]
    fn test_extract_at_query_no_word_boundary() {
        let mut textarea = tui_textarea::TextArea::default();
        textarea.insert_str("email@example");
        let query = extract_at_query(&textarea);
        assert_eq!(query, None);
    }

    #[test]
    fn test_extract_at_query_no_at() {
        let mut textarea = tui_textarea::TextArea::default();
        textarea.insert_str("hello world");
        let query = extract_at_query(&textarea);
        assert_eq!(query, None);
    }

    #[test]
    fn test_extract_at_query_multiple_at_uses_last() {
        let mut textarea = tui_textarea::TextArea::default();
        textarea.insert_str("@foo hello @bar");
        let query = extract_at_query(&textarea);
        assert_eq!(query, Some("bar".to_string()));
    }

    // --- filter_file_suggestions tests ---

    #[test]
    fn test_filter_file_suggestions_empty_query() {
        let paths = vec![
            "src/main.rs".to_string(),
            "src/lib.rs".to_string(),
            "Cargo.toml".to_string(),
        ];
        let results = filter_file_suggestions(&paths, "", 7);
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn test_filter_file_suggestions_caps_results() {
        let paths: Vec<String> = (0..20).map(|i| format!("file_{i}.rs")).collect();
        let results = filter_file_suggestions(&paths, "", 7);
        assert_eq!(results.len(), 7);
    }

    #[test]
    fn test_filter_file_suggestions_fuzzy_match() {
        let paths = vec![
            "src/main.rs".to_string(),
            "src/lib.rs".to_string(),
            "tests/integration.rs".to_string(),
        ];
        let results = filter_file_suggestions(&paths, "main", 7);
        assert_eq!(results.len(), 1);
        assert_eq!(paths[results[0].index], "src/main.rs");
    }

    #[test]
    fn test_filter_file_suggestions_no_match() {
        let paths = vec!["src/main.rs".to_string()];
        let results = filter_file_suggestions(&paths, "zzzzz", 7);
        assert_eq!(results.len(), 0);
    }

    // --- multibyte regression tests (char cursor col vs byte index) ---
    // Pre-fix, the extractors sliced lines with the char-based cursor column
    // used as a byte index, panicking mid-char on multibyte content.

    #[test]
    fn test_extract_slash_query_multibyte_accent() {
        let mut textarea = tui_textarea::TextArea::default();
        textarea.insert_str("/hé");
        let query = extract_slash_query(&textarea);
        assert_eq!(query, Some("hé".to_string()));
    }

    #[test]
    fn test_extract_slash_query_multibyte_emoji() {
        let mut textarea = tui_textarea::TextArea::default();
        textarea.insert_str("/dé✨ploy");
        let query = extract_slash_query(&textarea);
        assert_eq!(query, Some("dé✨ploy".to_string()));
    }

    #[test]
    fn test_extract_slash_query_smart_quotes() {
        let mut textarea = tui_textarea::TextArea::default();
        textarea.insert_str("/say “hì”");
        let query = extract_slash_query(&textarea);
        assert_eq!(query, Some("say “hì”".to_string()));
    }

    #[test]
    fn test_extract_slash_query_cursor_mid_line_after_multibyte() {
        let mut textarea = tui_textarea::TextArea::default();
        textarea.insert_str("/hé✨");
        // Move cursor back one char (before the emoji, after the accent).
        textarea.move_cursor(tui_textarea::CursorMove::Back);
        let query = extract_slash_query(&textarea);
        assert_eq!(query, Some("hé".to_string()));
    }

    #[test]
    fn test_extract_at_query_multibyte() {
        let mut textarea = tui_textarea::TextArea::default();
        textarea.insert_str("héllo @fïle");
        let query = extract_at_query(&textarea);
        assert_eq!(query, Some("fïle".to_string()));
    }

    #[test]
    fn test_extract_at_query_multibyte_word_boundary_guard() {
        // `@` preceded by a multibyte non-whitespace char must not trigger
        // (word-boundary guard preserved; the byte before `@` is mid-`é`).
        let mut textarea = tui_textarea::TextArea::default();
        textarea.insert_str("é@x");
        let query = extract_at_query(&textarea);
        assert_eq!(query, None);
    }
}
