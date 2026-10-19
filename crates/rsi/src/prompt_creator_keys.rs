//! Key handlers for the Prompt Creator/Editor view.
//!
//! Intercepts all keys when the focused pane is `Pane::PromptCreator`.
//! In list mode: j/k navigate, Enter opens editor, Ctrl+n creates new,
//! d deletes, m toggles model dropdown, q closes.
//! In editor mode: delegates to the file viewer's InputSurface.
//!
//! Prompts are read and written through the daemon's operator prompt-file
//! RPCs (hub #3040), never the local disk; `d` moves a prompt to the daemon's
//! recoverable trash.

use crate::app::App;
use crate::types::{FileViewerState, Pane};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Handle a key event when the prompt creator view is focused.
/// Returns `true` if the key was consumed.
pub async fn handle_prompt_creator_key(app: &mut App, key: KeyEvent) -> bool {
    // Model dropdown open — handle dropdown keys first
    if let Some(ref mut dropdown) = app.prompt_creator_state.model_dropdown {
        if dropdown.open {
            return handle_model_dropdown_key(app, key);
        }
    }

    if app.prompt_creator_state.editing {
        handle_editor_key(app, key).await
    } else {
        handle_list_key(app, key).await
    }
}

/// Handle keys in the prompt list view.
async fn handle_list_key(app: &mut App, key: KeyEvent) -> bool {
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
                let prompt = app.prompt_creator_state.prompts
                    [app.prompt_creator_state.selected_index]
                    .clone();
                open_prompt_editor(app, &prompt.filename, &prompt.path).await;
            }
            true
        }

        KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            let name = crate::prompt_creator::new_prompt_name(std::time::SystemTime::now());
            match app
                .client
                .put_prompt_file(&name, crate::prompt_creator::NEW_PROMPT_CONTENT, true)
                .await
            {
                Ok(_) => {
                    app.refresh_prompts().await;
                    let index = app
                        .prompt_creator_state
                        .prompts
                        .iter()
                        .position(|prompt| prompt.filename == name);
                    app.prompt_creator_state.selected_index = index.unwrap_or(0);
                    let path = index
                        .map(|i| app.prompt_creator_state.prompts[i].path.clone())
                        .unwrap_or_else(|| std::path::PathBuf::from(&name));
                    open_prompt_editor(app, &name, &path).await;
                }
                Err(e) => {
                    app.notify_error(format!("Failed to create prompt: {e}"));
                }
            }
            true
        }

        KeyCode::Char('d') => {
            if !app.prompt_creator_state.prompts.is_empty() {
                let name = app.prompt_creator_state.prompts
                    [app.prompt_creator_state.selected_index]
                    .filename
                    .clone();
                match app.client.delete_prompt_file(&name).await {
                    Ok(deleted) => {
                        let trash = deleted.trash_path;
                        app.notify_success(format!("Moved {name} to trash: {trash}"));
                        app.refresh_prompts().await;
                    }
                    Err(e) => app.notify_error(format!("Failed to delete prompt: {e}")),
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
async fn handle_editor_key(app: &mut App, key: KeyEvent) -> bool {
    // Ctrl+S: save
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
        save_current_prompt(app).await;
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
        app.refresh_prompts().await;
        return true;
    }

    // Delegate to the file viewer's InputSurface
    let standard = app.standard_editing();
    let mut closed = false;
    if let Some(ref mut viewer) = app.prompt_creator_viewer {
        let config = crate::input_surface::InputSurfaceConfig {
            pass_through_unhandled: false,
            available_commands: &[],
            working_dir: None,
            // Prompt files are documents; Enter always inserts a line break.
            submit_on_enter: false,
            standard_editing: standard,
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
            crate::input_surface::InputAction::Close => closed = true,
            _ => {}
        }
    }
    if closed {
        app.prompt_creator_state.editing = false;
        app.prompt_creator_viewer = None;
        app.refresh_prompts().await;
    }

    true
}

/// Handle keys when the model dropdown is open.
fn handle_model_dropdown_key(app: &mut App, key: KeyEvent) -> bool {
    use crate::widget::model_dropdown::{
        ModelDropdownAction, handle_model_dropdown_key_with_providers,
    };
    let standard_editing = app.standard_editing();
    let Some(dropdown) = app.prompt_creator_state.model_dropdown.as_mut() else {
        return true;
    };
    // Prompt creator owns a fixed model catalog; retain its m-to-close shortcut.
    if !dropdown.filter_editing && key.code == KeyCode::Char('m') {
        dropdown.close();
        return true;
    }
    if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
        return true;
    }
    match handle_model_dropdown_key_with_providers(dropdown, &key, &[], &[], standard_editing) {
        ModelDropdownAction::Selected(model_id) => {
            app.prompt_creator_state.selected_model = Some(model_id);
            dropdown.close();
        }
        ModelDropdownAction::Dismissed => dropdown.close(),
        _ => {}
    }
    true
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
                archive_selected_index: 0,
                jobs_selected_index: 0,
            };
        }
    }
    app.prompt_creator_state.editing = false;
    app.prompt_creator_viewer = None;
}

/// Open a prompt in the editor (`GetPromptFile`). `path` is the display
/// path; the prompt is read and saved by `name`.
async fn open_prompt_editor(app: &mut App, name: &str, path: &std::path::Path) {
    match app.client.get_prompt_file(name).await {
        Ok(prompt) => {
            let viewer = FileViewerState::new(path.to_path_buf(), prompt.content);
            app.prompt_creator_viewer = Some(viewer);
            app.prompt_creator_state.editing = true;
        }
        Err(e) => {
            app.notify_error(format!("Failed to open prompt: {e}"));
        }
    }
}

/// Save the current prompt through the daemon (`PutPromptFile`, atomic).
async fn save_current_prompt(app: &mut App) {
    let (content, path) = if let Some(ref viewer) = app.prompt_creator_viewer {
        (viewer.surface.content(), viewer.file_path.clone())
    } else {
        return;
    };
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        app.notify_error(format!("Save failed: no prompt name in {}", path.display()));
        return;
    };
    let name = name.to_string();

    match app.client.put_prompt_file(&name, &content, false).await {
        Ok(_) => {
            if let Some(ref mut viewer) = app.prompt_creator_viewer {
                viewer.dirty = false;
            }
            let display = path.display();
            app.notify_success(format!("Saved: {display}"));
            app.refresh_prompts().await;
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

    #[tokio::test]
    async fn model_dropdown_prompt_creator_search_selects_and_captures_m() {
        let mut app = App::new(DaemonClient::new(PathBuf::from("/tmp/test.sock")));
        app.prompt_creator_state.model_dropdown = Some(crate::types::ModelDropdownState::new(
            rsi_common::types::SessionProvider::Claude,
            vec![
                ("first".into(), "First".into()),
                ("model-2".into(), "Model Two".into()),
            ],
            None,
        ));
        for c in "/model-2".chars() {
            assert!(
                handle_prompt_creator_key(
                    &mut app,
                    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
                )
                .await
            );
        }
        let dropdown = app.prompt_creator_state.model_dropdown.as_ref().unwrap();
        assert!(dropdown.open);
        assert_eq!(dropdown.filter_query, "model-2");
        assert_eq!(dropdown.selected_index, 1);
        handle_prompt_creator_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        assert_eq!(
            app.prompt_creator_state.selected_model.as_deref(),
            Some("model-2")
        );
        assert!(
            !app.prompt_creator_state
                .model_dropdown
                .as_ref()
                .unwrap()
                .open
        );
    }

    #[tokio::test]
    async fn plain_enter_inserts_newline_in_prompt_editor_with_submit_setting() {
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

        assert!(
            handle_editor_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)).await
        );
        let Some(viewer) = app.prompt_creator_viewer.as_ref() else {
            panic!("prompt editor")
        };
        assert_eq!(viewer.surface.content(), "first\n");
        assert!(viewer.dirty);
    }
}
