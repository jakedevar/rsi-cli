//! Provider form overlay input handling (add/edit custom providers).
//!
//! The provider form now uses the shared `InputSurface` editing engine so each
//! field behaves like a vim text area: insert/normal mode, motions, and
//! `Ctrl+Enter` submit. `Tab`/`Shift+Tab` still cycle fields.

use crate::app::App;
use crate::input_surface::{InputAction, InputSurface, InputSurfaceConfig};
use crate::types::{OverlayState, PopupMode};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tui_textarea::CursorMove;

/// Open the provider form overlay to add a new or edit an existing custom provider.
/// Pass `None` to create a new entry, or `Some(entry)` to pre-populate for editing.
pub fn open_provider_form(app: &mut App, entry: Option<&crate::settings::CustomProviderEntry>) {
    let (name, base_url, api_key, default_model, editing_id) = match entry {
        Some(e) => (
            make_surface(&e.name),
            make_surface(&e.base_url),
            make_surface(&e.api_key),
            make_surface(&e.default_model),
            Some(e.id),
        ),
        None => (
            make_surface(""),
            make_surface(""),
            make_surface(""),
            make_surface(""),
            None,
        ),
    };

    app.overlay = OverlayState::ProviderForm {
        focused_field: 0,
        name,
        base_url,
        api_key,
        default_model,
        editing_id,
    };
    app.mark_dirty();
}

/// Handle key events inside the ProviderForm overlay.
pub(super) fn handle_provider_form_key(app: &mut App, key: KeyEvent) {
    let submit_on_enter = app.settings.submit_on_enter;
    let (editing_id, focused_mode) = match &app.overlay {
        OverlayState::ProviderForm {
            focused_field,
            name,
            base_url,
            api_key,
            default_model,
            editing_id,
        } => {
            let mode = match *focused_field {
                0 => name.mode,
                1 => base_url.mode,
                2 => api_key.mode,
                3 => default_model.mode,
                _ => PopupMode::Normal,
            };
            (*editing_id, mode)
        }
        _ => return,
    };

    const FIELD_COUNT: usize = 4;

    match key.code {
        KeyCode::BackTab => {
            cycle_field(app, FIELD_COUNT, false);
        }
        KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => {
            cycle_field(app, FIELD_COUNT, false);
        }
        KeyCode::Tab => {
            cycle_field(app, FIELD_COUNT, true);
        }
        KeyCode::Esc if focused_mode == PopupMode::Normal => {
            app.overlay = OverlayState::None;
            app.mark_dirty();
        }
        _ => {
            let Some(surface) = focused_surface_mut(&mut app.overlay) else {
                return;
            };

            let action = crate::input_surface::handle_key(
                surface,
                key,
                &InputSurfaceConfig {
                    pass_through_unhandled: false,
                    available_commands: &[],
                    working_dir: None,
                    submit_on_enter,
                },
            );

            match action {
                InputAction::Consumed => {
                    app.mark_dirty();
                }
                InputAction::Submit(_) => {
                    submit_provider_form(app, editing_id);
                }
                InputAction::Close => {
                    app.overlay = OverlayState::None;
                    app.mark_dirty();
                }
                InputAction::Passthrough(_) | InputAction::CompileDecision { .. } => {}
            }
        }
    }
}

/// Return the currently focused field surface, if the overlay is a provider form.
pub(crate) fn focused_surface(overlay: &OverlayState) -> Option<&InputSurface> {
    match overlay {
        OverlayState::ProviderForm {
            focused_field,
            name,
            base_url,
            api_key,
            default_model,
            ..
        } => match *focused_field {
            0 => Some(name),
            1 => Some(base_url),
            2 => Some(api_key),
            3 => Some(default_model),
            _ => None,
        },
        _ => None,
    }
}

/// Return the currently focused field surface mutably, if the overlay is a provider form.
pub(crate) fn focused_surface_mut(overlay: &mut OverlayState) -> Option<&mut InputSurface> {
    match overlay {
        OverlayState::ProviderForm {
            focused_field,
            name,
            base_url,
            api_key,
            default_model,
            ..
        } => match *focused_field {
            0 => Some(name),
            1 => Some(base_url),
            2 => Some(api_key),
            3 => Some(default_model),
            _ => None,
        },
        _ => None,
    }
}

/// Return the current mode of the focused provider field.
pub(crate) fn focused_mode(overlay: &OverlayState) -> Option<PopupMode> {
    focused_surface(overlay).map(|surface| surface.mode)
}

fn cycle_field(app: &mut App, field_count: usize, forward: bool) {
    if let OverlayState::ProviderForm { focused_field, .. } = &mut app.overlay {
        if forward {
            *focused_field = (*focused_field + 1) % field_count;
        } else {
            *focused_field = (*focused_field + field_count - 1) % field_count;
        }
        app.mark_dirty();
    }
}

fn make_surface(content: &str) -> InputSurface {
    let mut surface = if content.is_empty() {
        InputSurface::new_insert()
    } else {
        InputSurface::new_insert_with_content(content.lines().map(str::to_string).collect())
    };
    surface.textarea.move_cursor(CursorMove::Top);
    surface.textarea.move_cursor(CursorMove::Head);
    surface
}

/// Submit the provider form (create or update), then close overlay.
fn submit_provider_form(app: &mut App, editing_id: Option<uuid::Uuid>) {
    // ProviderForm fields are semantically single-line. Strip any newlines that
    // slipped in (e.g. Shift+Enter when `submit_on_enter` is true, or plain
    // Enter when it is false) at the submit boundary so we never persist a
    // multi-line provider name / base_url / api_key / default_model.
    fn one_line(s: String) -> String {
        s.replace('\n', "").trim().to_string()
    }

    let (name, base_url, api_key, default_model) = match &app.overlay {
        OverlayState::ProviderForm {
            name,
            base_url,
            api_key,
            default_model,
            ..
        } => (
            one_line(name.content_for_send()),
            one_line(base_url.content_for_send()),
            one_line(api_key.content_for_send()),
            one_line(default_model.content_for_send()),
        ),
        _ => return,
    };

    if name.is_empty() {
        app.notify("Provider name required");
        return;
    }
    if base_url.is_empty() {
        app.notify("API URL required");
        return;
    }

    let mut updated = false;
    match editing_id {
        Some(id) => {
            if let Some(entry) = app
                .settings
                .custom_providers
                .iter_mut()
                .find(|e| e.id == id)
            {
                entry.name = name.clone();
                entry.base_url = base_url;
                entry.api_key = api_key;
                entry.default_model = default_model;
                updated = true;
                app.notify_success(format!("Updated provider: {}", name));
            } else {
                app.notify_error("Provider entry no longer exists");
            }
        }
        None => {
            let entry = crate::settings::CustomProviderEntry {
                id: uuid::Uuid::new_v4(),
                name: name.clone(),
                base_url,
                api_key,
                default_model,
            };
            app.settings.custom_providers.push(entry);
            updated = true;
            app.notify_success(format!("Added provider: {}", name));
        }
    }

    if updated {
        app.needs_model_refresh = true;
        app.overlay = OverlayState::None;
        app.mark_dirty();
    }
}
