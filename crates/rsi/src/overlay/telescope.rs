//! Telescope fuzzy file picker overlay input handling.

use crate::app::App;
use crate::types::OverlayState;
use crossterm::event::{KeyCode, KeyEvent};

/// Open the telescope file picker overlay.
pub fn open_telescope(app: &mut App) {
    // Scope to current project's directory
    let root = match app.current_project().and_then(|p| p.path.clone()) {
        Some(path) => path,
        None => {
            app.notify("No project selected — telescope requires an active project");
            return;
        }
    };

    // Resolve session ID for file opening
    let session_id = match app.focused_pane() {
        Some(crate::types::Pane::SessionDetail { session_id }) => Some(*session_id),
        _ => app.selected_session_id(),
    };

    let Some(session_id) = session_id else {
        app.notify("No session selected");
        return;
    };

    // Scan files immediately (same as file explorer finder activation)
    let file_cache = crate::file_utils::walk_files_scoped(&root, false, Some(1));
    let results: Vec<usize> = (0..file_cache.len().min(100)).collect();

    app.overlay = OverlayState::Telescope {
        root,
        session_id,
        query: String::new(),
        file_cache,
        results,
        selected: 0,
    };
}

/// Handle keys when the telescope overlay is active.
pub fn handle_telescope_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            app.overlay = OverlayState::None;
        }
        KeyCode::Enter => {
            handle_telescope_enter(app);
        }
        KeyCode::Char('j') | KeyCode::Down => {
            if let OverlayState::Telescope {
                results, selected, ..
            } = &mut app.overlay
            {
                move_finder_selection(selected, results.len(), 1);
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            if let OverlayState::Telescope { selected, .. } = &mut app.overlay {
                move_finder_selection(selected, usize::MAX, -1);
            }
        }
        KeyCode::Backspace => {
            if let OverlayState::Telescope { query, .. } = &mut app.overlay {
                query.pop();
            }
            rescore_telescope(app);
        }
        KeyCode::Char(c) => {
            if let OverlayState::Telescope { query, .. } = &mut app.overlay {
                query.push(c);
            }
            rescore_telescope(app);
        }
        _ => {}
    }
}

/// Paste clipboard content into the telescope query (Ctrl+V path).
/// Reads from the system clipboard, extracts text, appends to query.
pub fn paste_clipboard_telescope(app: &mut App) {
    let paste_dir = app.paste_dir.clone();
    match crate::clipboard::read_clipboard(&paste_dir) {
        crate::clipboard::ClipboardContent::Text(text) => {
            paste_text_telescope(app, &text);
        }
        crate::clipboard::ClipboardContent::Image { .. } => {
            // Images don't make sense in a file search query — ignore
        }
        crate::clipboard::ClipboardContent::Empty => {}
    }
}

/// Paste pre-read text into the telescope query (bracketed paste path).
/// Strips newlines (file search is single-line) and appends to query.
pub fn paste_text_telescope(app: &mut App, text: &str) {
    // Collapse newlines — telescope query is single-line
    let cleaned = normalize_finder_paste(text);
    if cleaned.is_empty() {
        return;
    }
    if let OverlayState::Telescope { query, .. } = &mut app.overlay {
        query.push_str(&cleaned);
    }
    rescore_telescope(app);
}

pub(crate) fn normalize_finder_paste(text: &str) -> String {
    text.chars()
        .filter(|ch| *ch != '\n' && *ch != '\r')
        .collect()
}

pub(crate) fn move_finder_selection(selected: &mut usize, len: usize, delta: i8) {
    if delta.is_positive() {
        *selected = selected.saturating_add(1).min(len.saturating_sub(1));
    } else {
        *selected = selected.saturating_sub(1);
    }
}

/// Re-score file_cache against query, update results.
fn rescore_telescope(app: &mut App) {
    if let OverlayState::Telescope {
        query,
        file_cache,
        results,
        selected,
        ..
    } = &mut app.overlay
    {
        *selected = 0;
        *results = rank_finder(
            query,
            file_cache.iter().map(|path| path.to_string_lossy()),
            100,
        );
    }
}

/// Shared scoring, stable ordering, and cap for the file and command finders.
pub(crate) fn rank_finder(
    query: &str,
    candidates: impl Iterator<Item = impl AsRef<str>>,
    limit: usize,
) -> Vec<usize> {
    use fuzzy_matcher::FuzzyMatcher;
    use fuzzy_matcher::skim::SkimMatcherV2;

    if query.is_empty() {
        return candidates
            .enumerate()
            .take(limit)
            .map(|(index, _)| index)
            .collect();
    }
    let matcher = SkimMatcherV2::default();
    let mut scored: Vec<(usize, i64)> = candidates
        .enumerate()
        .filter_map(|(index, text)| {
            matcher
                .fuzzy_match(text.as_ref(), query)
                .map(|score| (index, score))
        })
        .collect();
    scored.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    scored.truncate(limit);
    scored.into_iter().map(|(index, _)| index).collect()
}

/// Open the selected telescope result in the file viewer.
fn handle_telescope_enter(app: &mut App) {
    let path = match &app.overlay {
        OverlayState::Telescope {
            root,
            file_cache,
            results,
            selected,
            ..
        } => {
            if results.is_empty() {
                return;
            }
            let cache_idx = results[*selected];
            let rel_path = &file_cache[cache_idx];
            root.join(rel_path)
        }
        _ => return,
    };

    // Close telescope first
    app.overlay = OverlayState::None;

    // Open file using existing infrastructure
    crate::overlay::file_explorer::open_file_by_path(app, &path);
}

#[cfg(test)]
mod finder_tests {
    use super::*;

    #[test]
    fn finder_scores_command_catalog_source_with_stable_ties_and_cap() {
        let descriptors: Vec<_> = crate::action_registry::ACTION_DESCRIPTORS
            .iter()
            .filter(|descriptor| !descriptor.command_aliases.is_empty())
            .collect();
        let rows = rank_finder(
            "manager policy",
            descriptors
                .iter()
                .map(|descriptor| descriptor.command_aliases.join(" ")),
            100,
        );
        assert_eq!(
            descriptors[rows[0]].id,
            crate::action_registry::ActionId::ManagerPolicy
        );
        assert!(
            rank_finder("", descriptors.iter().map(|descriptor| descriptor.label), 3).len() == 3
        );
        assert!(
            rank_finder(
                "zzzzzzzzzzzz",
                descriptors.iter().map(|descriptor| descriptor.label),
                100
            )
            .is_empty()
        );
    }
}
