//! Key handlers for the Prompt Creator/Editor view.
//!
//! Intercepts all keys when the focused pane is `Pane::PromptCreator`.
//! In list mode: j/k navigate, Enter opens editor, Ctrl+n creates new,
//! d deletes, m toggles model dropdown, q closes.
//! In editor mode: delegates to the file viewer's InputSurface.

use crate::app::App;
use crate::types::{FileViewerState, Pane};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Handle a key event when the prompt creator view is focused.
/// Returns `true` if the key was consumed.
pub fn handle_prompt_creator_key(app: &mut App, key: KeyEvent) -> bool {
    // Model dropdown open — handle dropdown keys first
    if let Some(ref mut dropdown) = app.prompt_creator_state.model_dropdown {
        if dropdown.open {
            return handle_model_dropdown_key(app, key);
        }
    }

    if app.prompt_creator_state.editing {
        handle_editor_key(app, key)
    } else {
        handle_list_key(app, key)
    }
}

/// Handle keys in the prompt list view.
fn handle_list_key(app: &mut App, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Char('q') => {
            close_prompt_creator(app);
            true
        }

        KeyCode::Char('j') | KeyCode::Down => {
            let len = app.prompt_creator_state.prompts.len();
            if len > 0 && app.prompt_creator_state.selected_index + 1 < len {
                app.prompt_creator_state.selected_index += 1;
            }
            true
        }

        KeyCode::Char('k') | KeyCode::Up => {
            if app.prompt_creator_state.selected_index > 0 {
                app.prompt_creator_state.selected_index -= 1;
            }
            true
        }

        KeyCode::Enter => {
            if !app.prompt_creator_state.prompts.is_empty() {
                let path = app.prompt_creator_state.prompts
                    [app.prompt_creator_state.selected_index]
                    .path
                    .clone();
                open_prompt_editor(app, &path);
            }
            true
        }

        KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            match crate::prompt_creator::create_new_prompt() {
                Ok(path) => {
                    app.refresh_prompts();
                    // Select the newly created prompt (should be first since sorted by modified_at desc)
                    app.prompt_creator_state.selected_index = 0;
                    open_prompt_editor(app, &path);
                }
                Err(e) => {
                    app.notify_error(format!("Failed to create prompt: {e}"));
                }
            }
            true
        }

        KeyCode::Char('d') => {
            if !app.prompt_creator_state.prompts.is_empty() {
                let path = app.prompt_creator_state.prompts
                    [app.prompt_creator_state.selected_index]
                    .path
                    .clone();
                if let Err(e) = std::fs::remove_file(&path) {
                    app.notify_error(format!("Failed to delete prompt: {e}"));
                } else {
                    app.refresh_prompts();
                }
            }
            true
        }

        KeyCode::Char('m') => {
            if let Some(ref mut dropdown) = app.prompt_creator_state.model_dropdown {
                dropdown.toggle();
            }
            true
        }

        _ => true, // consume all keys in prompt creator
    }
}

/// Handle keys in the editor view.
fn handle_editor_key(app: &mut App, key: KeyEvent) -> bool {
    // Ctrl+S: save
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
        save_current_prompt(app);
        return true;
    }

    // Check if viewer is in normal mode for 'q' to close
    let is_normal = app
        .prompt_creator_viewer
        .as_ref()
        .map(|v| v.surface.mode == crate::types::PopupMode::Normal)
        .unwrap_or(false);

    if is_normal && key.code == KeyCode::Char('q') {
        // Close editor, return to list
        app.prompt_creator_state.editing = false;
        app.prompt_creator_viewer = None;
        app.refresh_prompts();
        return true;
    }

    // Delegate to the file viewer's InputSurface
    if let Some(ref mut viewer) = app.prompt_creator_viewer {
        let config = crate::input_surface::InputSurfaceConfig {
            pass_through_unhandled: false,
            available_commands: &[],
            working_dir: None,
            // Prompt files are documents; Enter always inserts a line break.
            submit_on_enter: false,
        };

        let action = crate::input_surface::handle_key(&mut viewer.surface, key, &config);
        match action {
            crate::input_surface::InputAction::Consumed => {
                // Mark dirty on any edit in insert mode
                if viewer.surface.mode == crate::types::PopupMode::Insert {
                    viewer.dirty = true;
                }
                // Invalidate highlight cache
                viewer.content_version += 1;
            }
            crate::input_surface::InputAction::Close => {
                app.prompt_creator_state.editing = false;
                app.prompt_creator_viewer = None;
                app.refresh_prompts();
            }
            _ => {}
        }
    }

    true
}

/// Handle keys when the model dropdown is open.
fn handle_model_dropdown_key(app: &mut App, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Char('j') | KeyCode::Down => {
            if let Some(ref mut dropdown) = app.prompt_creator_state.model_dropdown {
                if dropdown.selected_index + 1 < dropdown.models.len() {
                    dropdown.selected_index += 1;
                }
            }
            true
        }
        KeyCode::Char('k') | KeyCode::Up => {
            if let Some(ref mut dropdown) = app.prompt_creator_state.model_dropdown {
                if dropdown.selected_index > 0 {
                    dropdown.selected_index -= 1;
                }
            }
            true
        }
        KeyCode::Enter => {
            let selected = app
                .prompt_creator_state
                .model_dropdown
                .as_ref()
                .and_then(|d| d.models.get(d.selected_index).cloned());
            if let Some((model_id, _)) = selected {
                app.prompt_creator_state.selected_model = Some(model_id);
                if let Some(ref mut dropdown) = app.prompt_creator_state.model_dropdown {
                    dropdown.close();
                }
            }
            true
        }
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('m') => {
            if let Some(ref mut dropdown) = app.prompt_creator_state.model_dropdown {
                dropdown.close();
            }
            true
        }
        _ => true,
    }
}

/// Close the prompt creator and restore the previous pane.
pub fn close_prompt_creator(app: &mut App) {
    let tab = &mut app.tabs[app.active_tab];
    let focused = tab.focused_pane;
    if let Some(pane) = tab.layout.find_pane_mut(focused) {
        if !matches!(pane, Pane::PromptCreator) {
            return;
        }
        if let Some(prev) = app.pre_prompt_creator_pane.take() {
            *pane = prev;
        } else {
            *pane = Pane::SessionList {
                selected_index: 0,
                selected_session: None,
                scroll_offset: 0,
                active_zone: Default::default(),
                taskrabbit_selected_index: 0,
                archive_selected_index: 0,
                jobs_selected_index: 0,
            };
        }
    }
    app.prompt_creator_state.editing = false;
    app.prompt_creator_viewer = None;
}

/// Open a prompt file in the editor.
fn open_prompt_editor(app: &mut App, path: &std::path::Path) {
    match std::fs::read_to_string(path) {
        Ok(content) => {
            let viewer = FileViewerState::new(path.to_path_buf(), content);
            app.prompt_creator_viewer = Some(viewer);
            app.prompt_creator_state.editing = true;
        }
        Err(e) => {
            app.notify_error(format!("Failed to open prompt: {e}"));
        }
    }
}

/// Save the current prompt's content to disk.
fn save_current_prompt(app: &mut App) {
    let (content, path) = if let Some(ref viewer) = app.prompt_creator_viewer {
        (viewer.surface.content(), viewer.file_path.clone())
    } else {
        return;
    };

    match crate::prompt_creator::save_prompt(&path, &content) {
        Ok(()) => {
            if let Some(ref mut viewer) = app.prompt_creator_viewer {
                viewer.dirty = false;
            }
            let display = path.display();
            app.notify_success(format!("Saved: {display}"));
            app.refresh_prompts();
        }
        Err(e) => {
            app.notify_error(format!("Save failed: {e}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::DaemonClient;
    use crate::types::PopupMode;
    use std::path::PathBuf;

    #[test]
    fn plain_enter_inserts_newline_in_prompt_editor_with_submit_setting() {
        let mut app = App::new(DaemonClient::new(PathBuf::from("/tmp/test.sock")));
        app.settings.submit_on_enter = true;
        app.prompt_creator_state.editing = true;
        let mut viewer =
            FileViewerState::new(PathBuf::from("/tmp/prompt-enter.md"), "first".into());
        viewer.surface.mode = PopupMode::Insert;
        viewer
            .surface
            .textarea
            .move_cursor(tui_textarea::CursorMove::End);
        app.prompt_creator_viewer = Some(viewer);

        assert!(handle_editor_key(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
        ));
        let Some(viewer) = app.prompt_creator_viewer.as_ref() else {
            panic!("prompt editor")
        };
        assert_eq!(viewer.surface.content(), "first\n");
        assert!(viewer.dirty);
    }
}
