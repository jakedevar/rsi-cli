//! Command catalog finder and its single-line argument editor.

use crate::action_registry::{
    ACTION_DESCRIPTORS, ActionAvailability, ActionContext, ActionDescriptor, ActionId,
    CommandArgument, availability, command_descriptor, resolve_command,
};
use crate::app::App;
use crate::commands::{CommandResult, parse_command};
use crate::types::OverlayState;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn candidates(origin: &ActionContext) -> Vec<&'static ActionDescriptor> {
    ACTION_DESCRIPTORS
        .iter()
        .filter(|descriptor| !descriptor.command_aliases.is_empty())
        .filter(|descriptor| {
            matches!(
                availability(descriptor, origin),
                ActionAvailability::Available(_)
            )
        })
        .collect()
}

fn score_text(descriptor: &ActionDescriptor) -> String {
    let bindings = descriptor
        .bindings
        .iter()
        .map(|binding| binding.sequence)
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "{} {} {} {}",
        descriptor.command_aliases.join(" "),
        descriptor.label,
        descriptor.category,
        bindings
    )
}

fn ranked(origin: &ActionContext, query: &str) -> Vec<ActionId> {
    let entries = candidates(origin);
    crate::overlay::telescope::rank_finder(
        query,
        entries.iter().map(|descriptor| score_text(descriptor)),
        100,
    )
    .into_iter()
    .map(|index| entries[index].id)
    .collect()
}

pub fn open_command_palette(app: &mut App) {
    let origin = ActionContext::from_app(app);
    if matches!(app.overlay, OverlayState::None) {
        app.overlay_stack.push(OverlayState::None);
    } else {
        app.push_current_overlay();
    }
    app.overlay = OverlayState::CommandPalette {
        query: String::new(),
        results: ranked(&origin, ""),
        selected: 0,
        argument_edit: false,
        argument_input: String::new(),
        origin,
    };
    app.mark_dirty();
}

fn close(app: &mut App) {
    if app.previous_overlay().is_some() {
        app.restore_previous_overlay();
    } else {
        app.overlay = OverlayState::None;
    }
    crate::event::reset_vim_command_mode(app);
    app.mark_dirty();
}

fn rescore(app: &mut App) {
    if let OverlayState::CommandPalette {
        query,
        results,
        selected,
        origin,
        ..
    } = &mut app.overlay
    {
        *results = ranked(origin, query);
        *selected = (*selected).min(results.len().saturating_sub(1));
    }
    app.mark_dirty();
}

fn selected_descriptor(app: &App) -> Option<&'static ActionDescriptor> {
    let OverlayState::CommandPalette {
        results, selected, ..
    } = &app.overlay
    else {
        return None;
    };
    results
        .get(*selected)
        .and_then(|id| command_descriptor(*id))
}

pub fn paste_text(app: &mut App, text: &str) {
    let cleaned = crate::overlay::telescope::normalize_finder_paste(text);
    if let OverlayState::CommandPalette {
        query,
        argument_edit,
        argument_input,
        ..
    } = &mut app.overlay
    {
        if *argument_edit {
            argument_input.push_str(&cleaned);
        } else {
            query.push_str(&cleaned);
            rescore(app);
        }
    }
}

pub fn paste_clipboard(app: &mut App) {
    if let crate::clipboard::ClipboardContent::Text(text) =
        crate::clipboard::read_clipboard(&app.paste_dir)
    {
        paste_text(app, &text);
    }
}

#[allow(clippy::future_not_send)]
async fn enter(app: &mut App) {
    let OverlayState::CommandPalette {
        query,
        argument_edit,
        argument_input,
        origin,
        ..
    } = &app.overlay
    else {
        return;
    };
    let origin = *origin;
    let selected = selected_descriptor(app);
    if !*argument_edit
        && let CommandResult::Unhandled(message) = parse_command(query)
        && message != query.trim()
        && !ACTION_DESCRIPTORS
            .iter()
            .flat_map(|descriptor| descriptor.command_aliases)
            .any(|alias| alias.starts_with(query.trim()))
    {
        close(app);
        app.notify_error(message);
        return;
    }
    let command = if *argument_edit {
        let Some(descriptor) = selected else {
            return;
        };
        let name = descriptor.command_aliases[0];
        format!("{name} {}", argument_input.trim())
    } else {
        // An exact command spelling with arguments wins over the fuzzy row.
        // A partial multiword spelling (such as `manager pol`) remains a search.
        let exact = resolve_command(query).filter(|(descriptor, args)| {
            args.is_empty() && descriptor.command_aliases.contains(&query.trim())
                || !args.is_empty() && matches!(parse_command(query), CommandResult::LcAction(_))
        });
        if exact.is_some() {
            query.trim().to_string()
        } else {
            let Some(descriptor) = selected else {
                return;
            };
            if descriptor.command_argument == CommandArgument::Required {
                if let OverlayState::CommandPalette { argument_edit, .. } = &mut app.overlay {
                    *argument_edit = true;
                }
                return;
            }
            descriptor.command_aliases[0].to_string()
        }
    };
    let Some((descriptor, args)) = resolve_command(&command) else {
        return;
    };
    if descriptor.command_argument == CommandArgument::Required && args.is_empty() {
        if let OverlayState::CommandPalette { argument_edit, .. } = &mut app.overlay {
            *argument_edit = true;
        }
        return;
    }
    close(app);
    let now = ActionContext::from_app(app);
    if now.surface != origin.surface {
        app.notify("Command is unavailable: focus changed");
        return;
    }
    if let ActionAvailability::Unavailable { reason } = availability(descriptor, &now) {
        app.notify(reason);
        return;
    }
    crate::action_handler::dispatch_command(app, &command).await;
}

#[allow(clippy::future_not_send)]
pub async fn handle_key(app: &mut App, key: KeyEvent) {
    let editing = matches!(
        app.overlay,
        OverlayState::CommandPalette {
            argument_edit: true,
            ..
        }
    );
    match key.code {
        KeyCode::Esc if editing => {
            if let OverlayState::CommandPalette { argument_edit, .. } = &mut app.overlay {
                *argument_edit = false;
            }
        }
        KeyCode::Esc => close(app),
        KeyCode::Enter => enter(app).await,
        KeyCode::Tab if !editing => {
            if let Some(descriptor) = selected_descriptor(app)
                && descriptor.command_argument != CommandArgument::None
                && let OverlayState::CommandPalette { argument_edit, .. } = &mut app.overlay
            {
                *argument_edit = true;
            }
        }
        KeyCode::Down | KeyCode::Char('n')
            if !editing
                && (key.code == KeyCode::Down || key.modifiers == KeyModifiers::CONTROL) =>
        {
            if let OverlayState::CommandPalette {
                selected, results, ..
            } = &mut app.overlay
            {
                crate::overlay::telescope::move_finder_selection(selected, results.len(), 1);
            }
        }
        KeyCode::Up | KeyCode::Char('p')
            if !editing && (key.code == KeyCode::Up || key.modifiers == KeyModifiers::CONTROL) =>
        {
            if let OverlayState::CommandPalette { selected, .. } = &mut app.overlay {
                crate::overlay::telescope::move_finder_selection(selected, usize::MAX, -1);
            }
        }
        KeyCode::Backspace => {
            if let OverlayState::CommandPalette {
                query,
                argument_input,
                ..
            } = &mut app.overlay
            {
                if editing {
                    argument_input.pop();
                } else {
                    query.pop();
                }
            }
            if !editing {
                rescore(app);
            }
        }
        KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let OverlayState::CommandPalette {
                query,
                argument_input,
                ..
            } = &mut app.overlay
            {
                if editing {
                    argument_input.push(ch);
                } else {
                    query.push(ch);
                }
            }
            if !editing {
                rescore(app);
            }
        }
        _ => {}
    }
    app.mark_dirty();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn command_palette_rank_clamp_empty_and_paste() {
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        open_command_palette(&mut app);
        assert!(
            matches!(&app.overlay, OverlayState::CommandPalette { query, selected: 0, .. } if query.is_empty())
        );
        paste_text(&mut app, "manager\npolicy");
        let OverlayState::CommandPalette {
            query,
            results,
            selected,
            ..
        } = &app.overlay
        else {
            panic!("palette")
        };
        assert_eq!(query, "managerpolicy");
        assert_eq!(*selected, 0);
        assert!(results.contains(&ActionId::ManagerPolicy));
        if let OverlayState::CommandPalette { selected, .. } = &mut app.overlay {
            *selected = 5;
        }
        paste_text(&mut app, "zzzzzzzzzzzzzz");
        assert!(
            matches!(&app.overlay, OverlayState::CommandPalette { results, selected: 0, .. } if results.is_empty())
        );
    }

    #[tokio::test]
    async fn command_palette_enter_and_escape_transitions() {
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        open_command_palette(&mut app);
        handle_key(&mut app, key(KeyCode::Esc)).await;
        assert!(matches!(app.overlay, OverlayState::None));

        let mut app = crate::app::app_test_helpers::with_session_list(0);
        open_command_palette(&mut app);
        paste_text(&mut app, "model foo");
        if let OverlayState::CommandPalette {
            results, selected, ..
        } = &mut app.overlay
        {
            *results = vec![ActionId::Quit];
            *selected = 0;
        }
        handle_key(&mut app, key(KeyCode::Enter)).await;
        assert_eq!(app.selected_model.as_deref(), Some("foo"));
        assert!(matches!(app.overlay, OverlayState::None));
        assert!(!app.quit);

        open_command_palette(&mut app);
        paste_text(&mut app, "quit");
        handle_key(&mut app, key(KeyCode::Enter)).await;
        assert!(app.quit);
        assert!(matches!(app.overlay, OverlayState::None));
    }

    #[tokio::test]
    async fn command_palette_rechecks_selection_when_context_changes() {
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        app.poll.connected = true;
        open_command_palette(&mut app);
        if let OverlayState::CommandPalette {
            results, selected, ..
        } = &mut app.overlay
        {
            *results = vec![ActionId::Refresh];
            *selected = 0;
        }
        app.poll.connected = false;
        handle_key(&mut app, key(KeyCode::Enter)).await;
        assert!(matches!(app.overlay, OverlayState::None));
        assert!(
            app.notifications
                .back()
                .is_some_and(|notification| notification.message.contains("unavailable"))
        );
    }

    #[tokio::test]
    async fn command_palette_required_argument_editor_returns_to_results() {
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        open_command_palette(&mut app);
        if let OverlayState::CommandPalette {
            results, selected, ..
        } = &mut app.overlay
        {
            *results = vec![ActionId::ProjectDelete];
            *selected = 0;
        }
        handle_key(&mut app, key(KeyCode::Enter)).await;
        assert!(matches!(
            app.overlay,
            OverlayState::CommandPalette {
                argument_edit: true,
                ..
            }
        ));
        handle_key(&mut app, key(KeyCode::Esc)).await;
        assert!(matches!(
            app.overlay,
            OverlayState::CommandPalette {
                argument_edit: false,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn command_palette_navigation_keeps_printable_j_as_query_and_empty_enter_inert() {
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        open_command_palette(&mut app);
        handle_key(&mut app, key(KeyCode::Down)).await;
        assert!(matches!(
            app.overlay,
            OverlayState::CommandPalette { selected: 1, .. }
        ));
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
        )
        .await;
        assert!(matches!(
            app.overlay,
            OverlayState::CommandPalette { selected: 0, .. }
        ));
        handle_key(&mut app, key(KeyCode::Char('j'))).await;
        assert!(matches!(&app.overlay, OverlayState::CommandPalette { query, .. } if query == "j"));
        paste_text(&mut app, "zzzzzzzzzzzzzz");
        handle_key(&mut app, key(KeyCode::Enter)).await;
        assert!(
            matches!(&app.overlay, OverlayState::CommandPalette { results, .. } if results.is_empty())
        );
    }

    #[tokio::test]
    async fn command_palette_restores_underlying_non_text_overlay() {
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        app.overlay = OverlayState::ThemePicker {
            selected_index: 2,
            original_index: 1,
        };
        crate::overlay::handle_overlay_key(&mut app, key(KeyCode::Char(' '))).await;
        crate::overlay::handle_overlay_key(&mut app, key(KeyCode::Char(';'))).await;
        assert!(matches!(app.overlay, OverlayState::CommandPalette { .. }));
        handle_key(&mut app, key(KeyCode::Esc)).await;
        assert!(matches!(
            app.overlay,
            OverlayState::ThemePicker {
                selected_index: 2,
                original_index: 1
            }
        ));
    }
}
