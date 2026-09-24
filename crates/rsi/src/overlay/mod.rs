//! Overlay input handling — vim modal editing for popup overlays.

pub(crate) mod ai_chat;
pub(crate) mod ai_command;
pub mod budget_policy_form;
pub(crate) mod card_editor;
pub mod create_entity_form;
mod diagnostics;
pub(crate) mod dialectic;
mod esp_square;
pub mod file_explorer;
pub mod graph;
pub mod harness_manager;
pub mod hook_form;
pub mod keybindings_help;
pub mod label_form;
pub mod label_picker;
pub mod list;
pub mod manager_v2;
pub mod memory_search;
pub mod message_bridge_form;
pub mod parent_picker;
// Model selection handled by widget::model_dropdown (inline dropdown, not an overlay)
pub mod cohort_settlement;
pub mod color_customizer;
pub mod command_palette;
mod notification_browser;
pub mod project_form;
pub mod project_picker;
pub mod prompt;
pub mod prompt_preview;
pub mod provider_form;
pub mod question_modal;
pub mod rating;
mod recent_completions;
pub mod recursive_dag;
mod rename_session;
pub mod schedule_browser;
pub mod schedule_form;
pub mod session_info;
pub mod skill_preview;
pub mod sort_picker;
pub mod telescope;
pub mod terminal;
pub mod text_area_bg_editor;
pub mod theme_picker;
pub mod theme_role_editor;
pub mod trash_browser;

#[cfg(test)]
mod help_tests;
#[cfg(test)]
mod tests;

use crate::app::App;
use crate::types::{OverlayState, PopupMode};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Resolve Ctrl+Y/Ctrl+Shift+G in a session prompt through its selected launch
/// model. A modal override wins; otherwise use the current global launch
/// selection. Other surfaces continue using the persistent processor target.
fn selected_prompt_processor_config(
    config: &crate::settings::PromptProcessorConfig,
    selected_model: Option<String>,
    selected_provider: rsi_common::types::SessionProvider,
    selected_custom_provider: Option<&crate::settings::CustomProviderEntry>,
    model_override: Option<String>,
    provider_override: Option<rsi_common::types::SessionProvider>,
    override_custom_provider: Option<&crate::settings::CustomProviderEntry>,
) -> crate::settings::PromptProcessorConfig {
    let has_override = model_override.is_some();
    let model = model_override.or(selected_model);
    let provider = provider_override.unwrap_or(selected_provider);
    match model {
        Some(model) => crate::prompt_processor::config_for_selected_model(
            config,
            model,
            provider,
            if has_override {
                override_custom_provider
            } else {
                selected_custom_provider
            },
        ),
        None => config.clone(),
    }
}

// Re-export public items that external code depends on.
pub use budget_policy_form::open_budget_policy_form;
pub use cohort_settlement::open_source_worktree_settlement;
pub use color_customizer::open_color_customizer;
pub use create_entity_form::open_create_entity_form;
pub use hook_form::open_hook_form;
pub use keybindings_help::{close_keybindings_help, open_keybindings_help};
pub use label_picker::open_label_picker;
pub use memory_search::open_memory_search;
pub use message_bridge_form::open_message_bridge_form;
pub use parent_picker::open_parent_picker;
pub use project_form::PROJECT_COLORS;
pub use project_picker::open_project_picker;
pub use prompt::{open_blank_popup, open_continue_popup, open_taskrabbit_popup, open_typed_prompt};
pub use prompt_preview::open_prompt_preview;
pub use provider_form::open_provider_form;
pub use question_modal::open_question_modal;
pub use rating::open_rating_overlay;
pub use recursive_dag::open_recursive_dag_browser;
pub use session_info::open_session_info_panel;
pub use skill_preview::open_skill_preview;
pub use sort_picker::{close_sort_picker, open_sort_picker};
pub use text_area_bg_editor::open_text_area_bg_editor;
pub use theme_picker::open_theme_picker;
pub use theme_role_editor::open_theme_role_editor;
pub use trash_browser::open_trash_browser;

fn text_overlay_is_in_normal_mode(overlay: &OverlayState) -> bool {
    match overlay {
        OverlayState::Prompt { surface, .. } | OverlayState::InputModal { surface, .. } => {
            surface.mode == PopupMode::Normal
        }
        OverlayState::QuestionModal { mode, .. } => *mode == PopupMode::Normal,
        OverlayState::ProviderForm { .. } => {
            provider_form::focused_mode(overlay).is_some_and(|mode| mode == PopupMode::Normal)
        }
        _ => false,
    }
}

const fn text_entry_overlay_owns_space(overlay: &OverlayState) -> bool {
    matches!(
        overlay,
        OverlayState::ProjectForm { .. }
            | OverlayState::MessageBridgeForm { .. }
            | OverlayState::HookForm { .. }
            | OverlayState::BudgetPolicyForm { .. }
            | OverlayState::MemorySearch { .. }
            | OverlayState::RenameSession { .. }
            | OverlayState::LabelForm { .. }
            | OverlayState::AiCommand { .. }
            | OverlayState::AiChat { .. }
            | OverlayState::ScheduleForm { .. }
            | OverlayState::CreateEntityForm { .. }
            | OverlayState::Dialectic { .. }
            | OverlayState::CardEditor {
                editing: Some(_),
                ..
            }
            | OverlayState::SourceWorktreeSettlement(
                crate::types::SourceWorktreeSettlementOverlayState {
                    authorization_active: true,
                    ..
                }
            )
    )
}

fn handle_text_overlay_prompt_leader(app: &mut App, key: KeyEvent) -> bool {
    // These surfaces own Space directly. The explorer's finder also owns
    // printable keys, and Scheduled Jobs uses Space to toggle its selection.
    if matches!(
        &app.overlay,
        OverlayState::Terminal
            | OverlayState::HarnessManagerScope(..)
            | OverlayState::HarnessManagerV2(..)
            | OverlayState::ScheduleBrowser { .. }
            | OverlayState::FileExplorer { .. }
    ) {
        app.overlay_leader_pending = false;
        return false;
    }

    if text_entry_overlay_owns_space(&app.overlay) {
        app.overlay_leader_pending = false;
        return false;
    }

    if !text_overlay_is_in_normal_mode(&app.overlay) {
        // Text overlays in Insert mode: clear leader and pass through.
        if matches!(
            app.overlay,
            OverlayState::Prompt { .. }
                | OverlayState::InputModal { .. }
                | OverlayState::QuestionModal { .. }
                | OverlayState::ProviderForm { .. }
        ) {
            app.overlay_leader_pending = false;
            return false;
        }

        // Non-text overlays (GraphReview, ThemePicker, SortPicker, etc.):
        // honour Space+o / Space+m to dismiss the overlay and open a session modal.
        if app.overlay_leader_pending {
            app.overlay_leader_pending = false;
            if key.modifiers.is_empty() {
                match key.code {
                    KeyCode::Char('o') => {
                        prompt::open_taskrabbit_popup(app);
                        return true;
                    }
                    KeyCode::Char('m') => {
                        prompt::open_blank_popup(app);
                        return true;
                    }
                    KeyCode::Char(';') => {
                        command_palette::open_command_palette(app);
                        return true;
                    }
                    _ => {}
                }
            }
            return false;
        }
        if key.modifiers.is_empty() && key.code == KeyCode::Char(' ') {
            app.overlay_leader_pending = true;
            return true; // consume Space so the overlay doesn't act on it as a leader
        }
        return false;
    }

    if app.overlay_leader_pending {
        app.overlay_leader_pending = false;
        if key.modifiers.is_empty() && key.code == KeyCode::Char('o') {
            prompt::open_taskrabbit_popup(app);
            return true;
        }
        if key.modifiers.is_empty() && key.code == KeyCode::Char('m') {
            prompt::open_blank_popup(app);
            return true;
        }
        if key.modifiers.is_empty() && key.code == KeyCode::Char(';') {
            command_palette::open_command_palette(app);
            return true;
        }
        if key.modifiers.is_empty() && key.code == KeyCode::Esc {
            return true;
        }
        return false;
    }

    if key.modifiers.is_empty() && key.code == KeyCode::Char(' ') {
        app.overlay_leader_pending = true;
        return true;
    }

    false
}

/// Handle Space+o / Space+m leader sequence for input overlay stack.
fn handle_input_overlay_leader(app: &mut App, key: KeyEvent) -> bool {
    let focused = match app.focused_input_overlay() {
        Some(o) => o,
        None => return false,
    };
    if !text_overlay_is_in_normal_mode(focused) {
        app.overlay_leader_pending = false;
        return false;
    }

    if app.overlay_leader_pending {
        app.overlay_leader_pending = false;
        if key.modifiers.is_empty() && key.code == KeyCode::Char('o') {
            prompt::open_taskrabbit_popup(app);
            return true;
        }
        if key.modifiers.is_empty() && key.code == KeyCode::Char('m') {
            prompt::open_blank_popup(app);
            return true;
        }
        if key.modifiers.is_empty() && key.code == KeyCode::Esc {
            return true;
        }
        return false;
    }

    if key.modifiers.is_empty() && key.code == KeyCode::Char(' ') {
        app.overlay_leader_pending = true;
        return true;
    }

    false
}

/// Handle a key event when an overlay is active.
/// Returns `true` if the overlay consumed the event (caller should skip normal dispatch).
/// Returns `false` if no overlay is active.
pub async fn handle_overlay_key(app: &mut App, key: KeyEvent) -> bool {
    if matches!(app.overlay, OverlayState::CommandPalette { .. }) {
        command_palette::handle_key(app, key).await;
        return true;
    }
    if matches!(app.overlay, OverlayState::KeybindingsHelp { .. }) {
        keybindings_help::handle_keybindings_help_key(app, key);
        return true;
    }

    // --- Global model dropdown intercept (widget, not overlay) ---
    if app.model_dropdown.open {
        use crate::widget::model_dropdown::ModelDropdownAction;
        let action = crate::widget::model_dropdown::handle_model_dropdown_key(
            &mut app.model_dropdown,
            &key,
            &app.settings.custom_providers,
        );
        let consumed = !matches!(action, ModelDropdownAction::Ignored);
        match action {
            ModelDropdownAction::Selected(model_id) => {
                app.selected_model = Some(model_id);
                if let Some(model) = app.selected_model.as_deref() {
                    rsi_common::model_utils::reconcile_effort(model, &mut app.selected_effort);
                }
                app.model_dropdown.close();
            }
            ModelDropdownAction::ProviderCycled => {
                app.selected_provider = app.model_dropdown.provider;
                app.custom_provider_index = app.model_dropdown.custom_provider_index;
                app.available_models = app.model_dropdown.models.clone();
                app.selected_model = app.available_models.first().map(|(id, _)| id.clone());
                if let Some(model) = app.selected_model.as_deref() {
                    rsi_common::model_utils::reconcile_effort(model, &mut app.selected_effort);
                } else {
                    // Harness/custom catalogs may be empty until discovery;
                    // without a selected model, no effort remains compatible.
                    app.selected_effort = None;
                }
                app.model_discovery_rx = None;
                app.model_refresh_provider = Some(app.selected_provider);
                app.needs_model_refresh = true;
            }
            ModelDropdownAction::Dismissed => {
                app.model_dropdown.close();
            }
            ModelDropdownAction::Consumed | ModelDropdownAction::Ignored => {}
        }
        if consumed {
            return true;
        }
    }

    // --- Input overlay stack (Blank/TaskRabbit prompts shown simultaneously) ---
    // Input overlays take key priority over regular overlays — they sit on top visually.
    if !app.input_overlays.is_empty() {
        app.detail_list_focused = false;
        return handle_input_overlay_key(app, key).await;
    }

    // --- Regular overlay dispatch ---
    if matches!(app.overlay, OverlayState::None) {
        return false;
    }
    // Clear detail list focus when overlay intercepts input, so overlay
    // handlers that call focused_pane() see the real pane (SessionDetail)
    // rather than the proxied SessionList.
    app.detail_list_focused = false;

    if handle_text_overlay_prompt_leader(app, key) {
        return true;
    }

    let role_editor_discovery_key = matches!(
        app.overlay,
        OverlayState::ThemeRoleEditor {
            committed: true,
            ..
        }
    ) && matches!(key.code, KeyCode::Char('?'));
    let registry_surface = matches!(app.overlay, OverlayState::ScheduleBrowser { .. })
        || (matches!(app.overlay, OverlayState::ThemeRoleEditor { .. })
            && matches!(key.code, KeyCode::Enter | KeyCode::Delete | KeyCode::Esc))
        || role_editor_discovery_key;
    if registry_surface {
        let context = crate::action_registry::ActionContext::from_app(app);
        if crate::action_registry::has_binding_for_key(&context, key) {
            match crate::action_registry::request_for_key(&context, key) {
                crate::action_registry::ActionAvailability::Available(request) => {
                    crate::action_handler::dispatch_registered_action(app, request).await;
                }
                crate::action_registry::ActionAvailability::Unavailable { reason } => {
                    // A rejected second `d` still consumes the existing delete
                    // chord.  This preserves the established cancellation
                    // behavior while preventing a disconnected RPC mutation.
                    if matches!(
                        context.mode,
                        crate::action_registry::ActionMode::PendingDelete
                    ) && matches!(key.code, KeyCode::Char('d'))
                    {
                        if let OverlayState::ScheduleBrowser { pending_delete, .. } =
                            &mut app.overlay
                        {
                            *pending_delete = false;
                        }
                    }
                    app.notify(reason);
                }
            }
            return true;
        }
    }

    match &app.overlay {
        OverlayState::None => unreachable!(),
        OverlayState::CommandPalette { .. } | OverlayState::KeybindingsHelp { .. } => {
            unreachable!("handled before regular overlay dispatch")
        }
        OverlayState::ThemePicker { .. } => {
            theme_picker::handle_theme_picker_key(app, key);
            return true;
        }
        OverlayState::ThemeRoleEditor { .. } => {
            theme_role_editor::handle_theme_role_editor_key(app, key);
            return true;
        }
        OverlayState::ProjectPicker { .. } => {
            project_picker::handle_project_picker_key(app, key).await;
            return true;
        }
        OverlayState::ProjectForm { .. } => {
            project_form::handle_project_form_key(app, key).await;
            return true;
        }
        OverlayState::ProviderForm { .. } => {
            provider_form::handle_provider_form_key(app, key);
            return true;
        }
        OverlayState::MessageBridgeForm { .. } => {
            message_bridge_form::handle_message_bridge_form_key(app, key);
            return true;
        }
        OverlayState::HookForm { .. } => {
            hook_form::handle_hook_form_key(app, key);
            return true;
        }
        OverlayState::BudgetPolicyForm { .. } => {
            budget_policy_form::handle_budget_policy_form_key(app, key);
            return true;
        }
        OverlayState::HookConflictPrompt { .. } => {
            hook_form::handle_hook_conflict_key(app, key);
            return true;
        }
        OverlayState::SkillPreview { .. } => {
            skill_preview::handle_skill_preview_key(app, key);
            return true;
        }
        OverlayState::SortPicker { .. } => {
            sort_picker::handle_sort_picker_key(app, key);
            return true;
        }
        OverlayState::PromptPreview { .. } => {
            prompt_preview::handle_prompt_preview_key(app, key);
            return true;
        }
        OverlayState::SourceWorktreeSettlement(..) => {
            cohort_settlement::handle_source_worktree_settlement_key(app, key).await;
            return true;
        }
        OverlayState::HarnessManagerV2(..) => {
            manager_v2::handle_key(app, key).await;
        }
        OverlayState::HarnessManagerScope(..) => {
            harness_manager::handle_key(app, key).await;
            return true;
        }
        OverlayState::TrashBrowser { .. } => {
            trash_browser::handle_trash_browser_key(app, key).await;
            return true;
        }
        OverlayState::RecentCompletions { .. } => {
            recent_completions::handle_recent_completions_key(app, key);
            return true;
        }
        OverlayState::NotificationBrowser { .. } => {
            notification_browser::handle_notification_browser_key(app, key);
            return true;
        }
        OverlayState::FileExplorer {
            explorer_focused,
            finder_active,
            ..
        } => {
            // Finder input takes priority over the tree's close bindings.
            if *finder_active {
                file_explorer::handle_file_explorer_key(app, key);
                return true;
            }

            // The tree and viewer-focus shell retain their close bindings.
            if matches!(key.code, KeyCode::Esc | KeyCode::Char('q' | ' ')) {
                app.overlay = OverlayState::None;
                return true;
            }

            if *explorer_focused {
                file_explorer::handle_file_explorer_key(app, key);
                return true;
            }
            // Explorer is open but viewer has focus — only intercept Ctrl+H to refocus
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('h') {
                if let OverlayState::FileExplorer {
                    explorer_focused, ..
                } = &mut app.overlay
                {
                    *explorer_focused = true;
                }
                return true;
            }
            // All other keys pass through to the file viewer / normal dispatch
            return false;
        }
        OverlayState::Diagnostics => {
            diagnostics::handle_diagnostics_key(app, key);
            return true;
        }
        OverlayState::RecursiveDagBrowser(..) => {
            recursive_dag::handle_recursive_dag_key(app, key);
            return true;
        }
        OverlayState::MemorySearch { .. } => {
            memory_search::handle_memory_search_key(app, key).await;
            return true;
        }
        OverlayState::RenameSession { .. } => {
            rename_session::handle_rename_session_key(app, key).await;
            return true;
        }
        OverlayState::QuestionModal { .. } => {
            question_modal::handle_question_modal_key(app, key).await;
            return true;
        }
        OverlayState::LabelPicker { .. } => {
            label_picker::handle_label_picker_key(app, key).await;
            return true;
        }
        OverlayState::LabelForm { .. } => {
            label_form::handle_label_form_key(app, key).await;
            return true;
        }
        OverlayState::CreateEntityForm { .. } => {
            create_entity_form::handle_create_entity_form_key(app, key).await;
            return true;
        }
        OverlayState::ParentPicker { .. } => {
            parent_picker::handle_parent_picker_key(app, key).await;
            return true;
        }
        OverlayState::EspSquare { .. } => {
            esp_square::handle_esp_square_key(app, key).await;
            app.mark_dirty();
            return true;
        }
        OverlayState::InputModal { .. } => {
            handle_input_modal_key(app, key).await;
            return true;
        }
        OverlayState::Telescope { .. } => {
            telescope::handle_telescope_key(app, key);
            return true;
        }
        OverlayState::AiCommand { .. } => {
            ai_command::handle_ai_command_key(app, key).await;
            return true;
        }
        OverlayState::AiChat { .. } => {
            ai_chat::handle_ai_chat_key(app, key).await;
            return true;
        }
        OverlayState::GraphReview { .. } => {
            graph::handle_graph_review_key(app, key).await;
            return true;
        }
        OverlayState::CardEditor { .. } => {
            card_editor::handle_card_editor_key(app, key).await;
            return true;
        }
        OverlayState::Dialectic { .. } => {
            dialectic::handle_dialectic_key(app, key).await;
            return true;
        }
        OverlayState::ScheduleBrowser { .. } => {
            schedule_browser::handle_schedule_browser_key(app, key).await;
            return true;
        }
        OverlayState::ScheduleForm { .. } => {
            schedule_form::handle_schedule_form_key(app, key).await;
            return true;
        }
        OverlayState::Terminal => {
            terminal::handle_terminal_key(app, key);
            return true;
        }
        OverlayState::RatingOverlay { .. } => {
            rating::handle_rating_overlay_key(app, key).await;
            return true;
        }
        OverlayState::SessionInfoPanel { .. } => {
            session_info::handle_session_info_panel_key(app, key).await;
            return true;
        }
        OverlayState::Prompt { .. } => {}
        OverlayState::ColorCustomizer { .. } => {
            color_customizer::handle_color_customizer_key(app, key);
            return true;
        }
        OverlayState::TextAreaBgEditor { .. } => {
            text_area_bg_editor::handle_text_area_bg_editor_key(app, key);
            return true;
        }
    };

    // === Prompt-specific keys for ContinueSession prompts in app.overlay ===
    handle_overlay_prompt_keys(app, key).await
}

/// Handle Prompt-specific keys when the overlay is a ContinueSession prompt in `app.overlay`.
async fn handle_overlay_prompt_keys(app: &mut App, key: KeyEvent) -> bool {
    let mut selected_override_model = None;
    // Model dropdown intercept: when the prompt's dropdown is open, route keys to it
    if let OverlayState::Prompt {
        model_dropdown,
        model_override,
        provider_override,
        ..
    } = &mut app.overlay
    {
        if model_dropdown.open {
            use crate::widget::model_dropdown::ModelDropdownAction;
            let action = crate::widget::model_dropdown::handle_model_dropdown_key(
                model_dropdown,
                &key,
                &app.settings.custom_providers,
            );
            let consumed = !matches!(action, ModelDropdownAction::Ignored);
            match action {
                ModelDropdownAction::Selected(model_id) => {
                    selected_override_model = Some(model_id.clone());
                    *model_override = Some(model_id);
                    *provider_override = Some(model_dropdown.provider);
                    model_dropdown.close();
                }
                ModelDropdownAction::ProviderCycled => {
                    app.model_refresh_provider = Some(model_dropdown.provider);
                    app.needs_model_refresh = true;
                }
                ModelDropdownAction::Dismissed => model_dropdown.close(),
                ModelDropdownAction::Consumed | ModelDropdownAction::Ignored => {}
            }
            if consumed {
                if let Some(model) = selected_override_model.as_deref() {
                    rsi_common::model_utils::reconcile_effort(model, &mut app.selected_effort);
                }
                return true;
            }
        }
    }

    // Ctrl+T submits and opens in new tab
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('t') {
        prompt::submit_prompt_in_new_tab(app).await;
        return true;
    }

    // Ctrl+S submits and opens in new split
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
        prompt::submit_prompt_in_new_split(app).await;
        return true;
    }

    // Ctrl+Shift+A — open AI chat
    if matches!(key.code, KeyCode::Char('a') | KeyCode::Char('A'))
        && key.modifiers == (KeyModifiers::CONTROL | KeyModifiers::SHIFT)
    {
        if app.prompt_processor.is_none() {
            app.notify("AI assistant requires prompt processor");
            return true;
        }
        let (source_text, overlay_id) = match &app.overlay {
            OverlayState::Prompt {
                overlay_id,
                surface,
                ..
            } => (surface.content_trimmed(), *overlay_id),
            _ => return true,
        };
        if source_text.is_empty() {
            return true;
        }
        app.push_current_overlay();
        app.overlay = OverlayState::AiChat {
            messages: Vec::new(),
            input: String::new(),
            source_text,
            source: crate::types::AiAssistantSource::OverlaySurface(overlay_id),
            in_flight: false,
            scroll_offset: 0,
        };
        app.mark_dirty();
        return true;
    }

    // Ctrl+A — open AI command
    if key.code == KeyCode::Char('a') && key.modifiers.contains(KeyModifiers::CONTROL) {
        if app.prompt_processor.is_none() {
            app.notify("AI assistant requires prompt processor");
            return true;
        }
        let (source_text, overlay_id) = match &app.overlay {
            OverlayState::Prompt {
                overlay_id,
                surface,
                ..
            } => (surface.content_trimmed(), *overlay_id),
            _ => return true,
        };
        if source_text.is_empty() {
            return true;
        }
        app.push_current_overlay();
        app.overlay = OverlayState::AiCommand {
            command: String::new(),
            source_text,
            source: crate::types::AiAssistantSource::OverlaySurface(overlay_id),
            in_flight: false,
        };
        app.mark_dirty();
        return true;
    }

    // Ctrl+Y — prompt compilation
    if key.code == KeyCode::Char('y') && key.modifiers.contains(KeyModifiers::CONTROL) {
        let processor_settings = app.settings.prompt_processor.clone();
        let selected_model = app.selected_model.clone();
        let selected_provider = app.selected_provider;
        let custom_providers = app.settings.custom_providers.clone();
        let selected_custom_provider = app
            .custom_provider_index
            .and_then(|index| custom_providers.get(index).cloned());
        if let OverlayState::Prompt {
            overlay_id,
            surface,
            model_override,
            provider_override,
            model_dropdown,
            ..
        } = &mut app.overlay
        {
            if surface.correction_in_flight || app.prompt_compile_rx.is_some() {
                return true;
            }
            if app.prompt_processor.is_none() {
                app.notify(
                    "No prompt processor configured (set settings.prompt_processor.enabled)",
                );
                return true;
            }

            let input = surface.content_trimmed();
            if input.is_empty() {
                return true;
            }

            surface.correction_in_flight = true;
            let overlay_id = *overlay_id;
            let original_input = input.clone();

            let config = selected_prompt_processor_config(
                &processor_settings,
                selected_model,
                selected_provider,
                selected_custom_provider.as_ref(),
                model_override.clone(),
                *provider_override,
                model_dropdown
                    .custom_provider_index
                    .and_then(|index| custom_providers.get(index)),
            );
            let (tx, rx) = tokio::sync::oneshot::channel();
            app.prompt_compile_rx = Some((overlay_id, original_input, rx));

            tokio::spawn(async move {
                let processor = crate::prompt_processor::build_processor(&config);
                let result = match processor {
                    Some(p) => p.compile(&input).await.map_err(|e| e.to_string()),
                    None => Err("Processor disabled".to_string()),
                };
                let _ = tx.send(result);
            });

            app.mark_dirty();
        }
        return true;
    }

    // Ctrl+Shift+G — grammar/spelling correction
    if key
        .modifiers
        .contains(KeyModifiers::CONTROL | KeyModifiers::SHIFT)
        && matches!(key.code, KeyCode::Char('g') | KeyCode::Char('G'))
    {
        let processor_settings = app.settings.prompt_processor.clone();
        let selected_model = app.selected_model.clone();
        let selected_provider = app.selected_provider;
        let custom_providers = app.settings.custom_providers.clone();
        let selected_custom_provider = app
            .custom_provider_index
            .and_then(|index| custom_providers.get(index).cloned());
        if let OverlayState::Prompt {
            overlay_id,
            surface,
            model_override,
            provider_override,
            model_dropdown,
            ..
        } = &mut app.overlay
        {
            if surface.correction_in_flight || app.prompt_compile_rx.is_some() {
                return true;
            }
            if app.prompt_processor.is_none() {
                app.notify(
                    "No prompt processor configured (set settings.prompt_processor.enabled)",
                );
                return true;
            }

            let input = surface.content_trimmed();
            if input.is_empty() {
                return true;
            }

            surface.correction_in_flight = true;
            let overlay_id = *overlay_id;

            let config = selected_prompt_processor_config(
                &processor_settings,
                selected_model,
                selected_provider,
                selected_custom_provider.as_ref(),
                model_override.clone(),
                *provider_override,
                model_dropdown
                    .custom_provider_index
                    .and_then(|index| custom_providers.get(index)),
            );
            let (tx, rx) = tokio::sync::oneshot::channel();
            app.prompt_compile_rx = Some((overlay_id, String::new(), rx));

            tokio::spawn(async move {
                let processor = crate::prompt_processor::build_processor(&config);
                let result = match processor {
                    Some(p) => p
                        .send(crate::prompt_processor::GRAMMAR_SYSTEM_PROMPT, &input)
                        .await
                        .map(|text| crate::prompt_processor::CompileResult {
                            compiled: text,
                            contract: crate::prompt_processor::OutputContract::Complete,
                            layer_validation: crate::prompt_processor::LayerValidation {
                                semantic: true,
                                syntactic: true,
                                deictic: true,
                                discourse: true,
                                pragmatic: true,
                            },
                        })
                        .map_err(|e| e.to_string()),
                    None => Err("Processor disabled".to_string()),
                };
                let _ = tx.send(result);
            });

            app.mark_dirty();
        }
        return true;
    }

    // Ctrl+V — paste from clipboard
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('v') | KeyCode::Char('V'))
    {
        paste_into_overlay(app);
        return true;
    }

    // Ctrl+M toggles inline model dropdown in Blank and TaskRabbit prompts.
    if matches!(key.code, KeyCode::Char('m') | KeyCode::Char('M'))
        && key.modifiers == KeyModifiers::CONTROL
    {
        if let OverlayState::Prompt {
            surface,
            purpose,
            model_dropdown,
            ..
        } = &mut app.overlay
        {
            if surface.mode == PopupMode::Normal
                && matches!(
                    purpose,
                    crate::types::PromptPurpose::Blank | crate::types::PromptPurpose::TaskRabbit
                )
            {
                model_dropdown.toggle();
                if model_dropdown.open {
                    app.needs_model_refresh = true;
                }
                return true;
            }
        }
    }

    // Ctrl+E cycles effort level.
    if matches!(key.code, KeyCode::Char('e') | KeyCode::Char('E'))
        && key.modifiers == KeyModifiers::CONTROL
    {
        // Snapshot the focused prompt's override so `cycle_effort` can borrow
        // app mutably below without aliasing the overlay.
        let (effort_model_override, effort_provider_override) = match &app.overlay {
            OverlayState::Prompt {
                model_override,
                provider_override,
                ..
            } => (model_override.clone(), *provider_override),
            _ => (None, None),
        };
        if let OverlayState::Prompt {
            surface, purpose, ..
        } = &app.overlay
        {
            if surface.mode == PopupMode::Normal
                && matches!(
                    purpose,
                    crate::types::PromptPurpose::Blank | crate::types::PromptPurpose::TaskRabbit
                )
            {
                cycle_effort(
                    app,
                    effort_model_override.as_deref(),
                    effort_provider_override,
                );
                app.mark_dirty();
                return true;
            }
        }
    }

    // Ctrl+B toggles sandbox mode (capability-gated: no-op when daemon lacks sandbox support).
    if key.code == KeyCode::Char('b') && key.modifiers == KeyModifiers::CONTROL {
        let sandbox_caps = app.poll.sandbox_supported;
        if let OverlayState::Prompt {
            surface,
            purpose,
            sandbox_enabled,
            ..
        } = &mut app.overlay
        {
            if surface.mode == PopupMode::Normal
                && matches!(
                    purpose,
                    crate::types::PromptPurpose::Blank | crate::types::PromptPurpose::TaskRabbit
                )
                && sandbox_caps
            {
                *sandbox_enabled = !*sandbox_enabled;
                app.mark_dirty();
                return true;
            }
        }
    }

    // === Delegate to shared InputSurface handler ===
    let (available_commands, working_dir) = if let OverlayState::Prompt {
        available_commands,
        working_dir,
        ..
    } = &app.overlay
    {
        (available_commands.clone(), Some(working_dir.clone()))
    } else {
        return true;
    };

    let config = crate::input_surface::InputSurfaceConfig {
        pass_through_unhandled: false,
        available_commands: &available_commands,
        working_dir: working_dir.as_deref(),
        submit_on_enter: false,
    };

    let action = if let OverlayState::Prompt { surface, .. } = &mut app.overlay {
        crate::input_surface::handle_key(surface, key, &config)
    } else {
        return true;
    };

    match action {
        crate::input_surface::InputAction::Consumed => {
            app.mark_dirty();
        }
        crate::input_surface::InputAction::Submit(_) => {
            prompt::submit_prompt(app).await;
        }
        crate::input_surface::InputAction::Close => {
            prompt::close_overlay(app);
        }
        crate::input_surface::InputAction::Passthrough(_) => {}
        crate::input_surface::InputAction::CompileDecision {
            accepted,
            context,
            compiled_output,
        } => {
            let session_id = if let OverlayState::Prompt { purpose, .. } = &app.overlay {
                match purpose {
                    crate::types::PromptPurpose::ContinueSession(id) => Some(*id),
                    _ => None,
                }
            } else {
                None
            };
            let params = rsi_common::rpc::SaveCompiledPromptParams {
                session_id,
                original_input: context.original_input,
                compiled_output,
                contract_status: context.contract_status,
                layer_semantic: context.layer_semantic,
                layer_syntactic: context.layer_syntactic,
                layer_deictic: context.layer_deictic,
                layer_discourse: context.layer_discourse,
                layer_pragmatic: context.layer_pragmatic,
                accepted,
            };
            let _ = app.client.save_compiled_prompt(params).await;
            app.mark_dirty();
        }
    }

    true
}

/// Handle key events for the input overlay stack (multiple Blank/TaskRabbit prompts).
async fn handle_input_overlay_key(app: &mut App, key: KeyEvent) -> bool {
    // Ctrl+J: focus next (downward) input overlay
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('j') {
        if app.input_overlays.len() > 1 && app.focused_input_idx + 1 < app.input_overlays.len() {
            app.focused_input_idx += 1;
            app.mark_dirty();
        }
        return true;
    }

    // Ctrl+K: focus previous (upward) input overlay
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('k') {
        if app.input_overlays.len() > 1 && app.focused_input_idx > 0 {
            app.focused_input_idx -= 1;
            app.mark_dirty();
        }
        return true;
    }

    // Leader sequence (Space+o for TaskRabbit toggle)
    if handle_input_overlay_leader(app, key) {
        return true;
    }

    // Route prompt-specific keys to the focused input overlay
    handle_focused_input_prompt_keys(app, key).await
}

/// Handle Prompt-specific keys for the focused input overlay.
async fn handle_focused_input_prompt_keys(app: &mut App, key: KeyEvent) -> bool {
    let idx = app.focused_input_idx;
    let mut selected_override_model = None;

    // Model dropdown intercept: when the prompt's dropdown is open, route keys to it
    if let Some(OverlayState::Prompt {
        model_dropdown,
        model_override,
        provider_override,
        ..
    }) = app.input_overlays.get_mut(idx)
    {
        if model_dropdown.open {
            use crate::widget::model_dropdown::ModelDropdownAction;
            let action = crate::widget::model_dropdown::handle_model_dropdown_key(
                model_dropdown,
                &key,
                &app.settings.custom_providers,
            );
            let consumed = !matches!(action, ModelDropdownAction::Ignored);
            match action {
                ModelDropdownAction::Selected(model_id) => {
                    selected_override_model = Some(model_id.clone());
                    *model_override = Some(model_id);
                    *provider_override = Some(model_dropdown.provider);
                    model_dropdown.close();
                }
                ModelDropdownAction::ProviderCycled => {
                    app.model_refresh_provider = Some(model_dropdown.provider);
                    app.needs_model_refresh = true;
                }
                ModelDropdownAction::Dismissed => model_dropdown.close(),
                ModelDropdownAction::Consumed | ModelDropdownAction::Ignored => {}
            }
            if consumed {
                if let Some(model) = selected_override_model.as_deref() {
                    rsi_common::model_utils::reconcile_effort(model, &mut app.selected_effort);
                }
                return true;
            }
        }
    }

    // Ctrl+T: submit in new tab
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('t') {
        prompt::submit_input_overlay_in_new_tab(app).await;
        return true;
    }

    // Ctrl+S: submit in new split
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
        prompt::submit_input_overlay_in_new_split(app).await;
        return true;
    }

    // Ctrl+Shift+A — open AI chat (opens in app.overlay, input overlays stay visible)
    if matches!(key.code, KeyCode::Char('a') | KeyCode::Char('A'))
        && key.modifiers == (KeyModifiers::CONTROL | KeyModifiers::SHIFT)
    {
        if app.prompt_processor.is_none() {
            app.notify("AI assistant requires prompt processor");
            return true;
        }
        let (source_text, overlay_id) = match app.input_overlays.get(idx) {
            Some(OverlayState::Prompt {
                overlay_id,
                surface,
                ..
            }) => (surface.content_trimmed(), *overlay_id),
            _ => return true,
        };
        if source_text.is_empty() {
            return true;
        }
        app.overlay = OverlayState::AiChat {
            messages: Vec::new(),
            input: String::new(),
            source_text,
            source: crate::types::AiAssistantSource::OverlaySurface(overlay_id),
            in_flight: false,
            scroll_offset: 0,
        };
        app.mark_dirty();
        return true;
    }

    // Ctrl+A — open AI command (opens in app.overlay, input overlays stay visible)
    if key.code == KeyCode::Char('a') && key.modifiers.contains(KeyModifiers::CONTROL) {
        if app.prompt_processor.is_none() {
            app.notify("AI assistant requires prompt processor");
            return true;
        }
        let (source_text, overlay_id) = match app.input_overlays.get(idx) {
            Some(OverlayState::Prompt {
                overlay_id,
                surface,
                ..
            }) => (surface.content_trimmed(), *overlay_id),
            _ => return true,
        };
        if source_text.is_empty() {
            return true;
        }
        app.overlay = OverlayState::AiCommand {
            command: String::new(),
            source_text,
            source: crate::types::AiAssistantSource::OverlaySurface(overlay_id),
            in_flight: false,
        };
        app.mark_dirty();
        return true;
    }

    // Ctrl+Y — prompt compilation
    if key.code == KeyCode::Char('y') && key.modifiers.contains(KeyModifiers::CONTROL) {
        let processor_settings = app.settings.prompt_processor.clone();
        let selected_model = app.selected_model.clone();
        let selected_provider = app.selected_provider;
        let custom_providers = app.settings.custom_providers.clone();
        let selected_custom_provider = app
            .custom_provider_index
            .and_then(|index| custom_providers.get(index).cloned());
        if let Some(OverlayState::Prompt {
            overlay_id,
            surface,
            model_override,
            provider_override,
            model_dropdown,
            ..
        }) = app.input_overlays.get_mut(idx)
        {
            if surface.correction_in_flight || app.prompt_compile_rx.is_some() {
                return true;
            }
            if app.prompt_processor.is_none() {
                app.notify(
                    "No prompt processor configured (set settings.prompt_processor.enabled)",
                );
                return true;
            }
            let input = surface.content_trimmed();
            if input.is_empty() {
                return true;
            }
            surface.correction_in_flight = true;
            let overlay_id = *overlay_id;
            let original_input = input.clone();

            let config = selected_prompt_processor_config(
                &processor_settings,
                selected_model,
                selected_provider,
                selected_custom_provider.as_ref(),
                model_override.clone(),
                *provider_override,
                model_dropdown
                    .custom_provider_index
                    .and_then(|index| custom_providers.get(index)),
            );
            let (tx, rx) = tokio::sync::oneshot::channel();
            app.prompt_compile_rx = Some((overlay_id, original_input, rx));

            tokio::spawn(async move {
                let processor = crate::prompt_processor::build_processor(&config);
                let result = match processor {
                    Some(p) => p.compile(&input).await.map_err(|e| e.to_string()),
                    None => Err("Processor disabled".to_string()),
                };
                let _ = tx.send(result);
            });
            app.mark_dirty();
        }
        return true;
    }

    // Ctrl+Shift+G — grammar correction
    if key
        .modifiers
        .contains(KeyModifiers::CONTROL | KeyModifiers::SHIFT)
        && matches!(key.code, KeyCode::Char('g') | KeyCode::Char('G'))
    {
        let processor_settings = app.settings.prompt_processor.clone();
        let selected_model = app.selected_model.clone();
        let selected_provider = app.selected_provider;
        let custom_providers = app.settings.custom_providers.clone();
        let selected_custom_provider = app
            .custom_provider_index
            .and_then(|index| custom_providers.get(index).cloned());
        if let Some(OverlayState::Prompt {
            overlay_id,
            surface,
            model_override,
            provider_override,
            model_dropdown,
            ..
        }) = app.input_overlays.get_mut(idx)
        {
            if surface.correction_in_flight || app.prompt_compile_rx.is_some() {
                return true;
            }
            if app.prompt_processor.is_none() {
                app.notify(
                    "No prompt processor configured (set settings.prompt_processor.enabled)",
                );
                return true;
            }
            let input = surface.content_trimmed();
            if input.is_empty() {
                return true;
            }
            surface.correction_in_flight = true;
            let overlay_id = *overlay_id;

            let config = selected_prompt_processor_config(
                &processor_settings,
                selected_model,
                selected_provider,
                selected_custom_provider.as_ref(),
                model_override.clone(),
                *provider_override,
                model_dropdown
                    .custom_provider_index
                    .and_then(|index| custom_providers.get(index)),
            );
            let (tx, rx) = tokio::sync::oneshot::channel();
            app.prompt_compile_rx = Some((overlay_id, String::new(), rx));

            tokio::spawn(async move {
                let processor = crate::prompt_processor::build_processor(&config);
                let result = match processor {
                    Some(p) => p
                        .send(crate::prompt_processor::GRAMMAR_SYSTEM_PROMPT, &input)
                        .await
                        .map(|text| crate::prompt_processor::CompileResult {
                            compiled: text,
                            contract: crate::prompt_processor::OutputContract::Complete,
                            layer_validation: crate::prompt_processor::LayerValidation {
                                semantic: true,
                                syntactic: true,
                                deictic: true,
                                discourse: true,
                                pragmatic: true,
                            },
                        })
                        .map_err(|e| e.to_string()),
                    None => Err("Processor disabled".to_string()),
                };
                let _ = tx.send(result);
            });
            app.mark_dirty();
        }
        return true;
    }

    // Ctrl+V — paste from clipboard
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('v') | KeyCode::Char('V'))
    {
        paste_into_overlay(app);
        return true;
    }

    // Ctrl+M toggles inline model dropdown in Blank and TaskRabbit prompts.
    if matches!(key.code, KeyCode::Char('m') | KeyCode::Char('M'))
        && key.modifiers == KeyModifiers::CONTROL
    {
        if let Some(OverlayState::Prompt {
            surface,
            purpose,
            model_dropdown,
            ..
        }) = app.input_overlays.get_mut(idx)
        {
            if surface.mode == PopupMode::Normal
                && matches!(
                    purpose,
                    crate::types::PromptPurpose::Blank | crate::types::PromptPurpose::TaskRabbit
                )
            {
                model_dropdown.toggle();
                if model_dropdown.open {
                    app.needs_model_refresh = true;
                }
                return true;
            }
        }
    }

    // Ctrl+E cycles effort level.
    if matches!(key.code, KeyCode::Char('e') | KeyCode::Char('E'))
        && key.modifiers == KeyModifiers::CONTROL
    {
        // Snapshot the focused prompt's override so `cycle_effort` can borrow
        // app mutably below without aliasing the input-overlay stack.
        let (effort_model_override, effort_provider_override) = match app.input_overlays.get(idx) {
            Some(OverlayState::Prompt {
                model_override,
                provider_override,
                ..
            }) => (model_override.clone(), *provider_override),
            _ => (None, None),
        };
        if let Some(OverlayState::Prompt {
            surface, purpose, ..
        }) = app.input_overlays.get(idx)
        {
            if surface.mode == PopupMode::Normal
                && matches!(
                    purpose,
                    crate::types::PromptPurpose::Blank | crate::types::PromptPurpose::TaskRabbit
                )
            {
                cycle_effort(
                    app,
                    effort_model_override.as_deref(),
                    effort_provider_override,
                );
                app.mark_dirty();
                return true;
            }
        }
    }

    // Ctrl+B toggles sandbox mode (capability-gated: no-op when daemon lacks sandbox support).
    if key.code == KeyCode::Char('b') && key.modifiers == KeyModifiers::CONTROL {
        let sandbox_caps = app.poll.sandbox_supported;
        if let Some(OverlayState::Prompt {
            surface,
            purpose,
            sandbox_enabled,
            ..
        }) = app.input_overlays.get_mut(idx)
        {
            if surface.mode == PopupMode::Normal
                && matches!(
                    purpose,
                    crate::types::PromptPurpose::Blank | crate::types::PromptPurpose::TaskRabbit
                )
                && sandbox_caps
            {
                *sandbox_enabled = !*sandbox_enabled;
                app.mark_dirty();
                return true;
            }
        }
    }

    // === Delegate to shared InputSurface handler ===
    let (available_commands, working_dir) = match app.input_overlays.get(idx) {
        Some(OverlayState::Prompt {
            available_commands,
            working_dir,
            ..
        }) => (available_commands.clone(), Some(working_dir.clone())),
        _ => return true,
    };

    let config = crate::input_surface::InputSurfaceConfig {
        pass_through_unhandled: false,
        available_commands: &available_commands,
        working_dir: working_dir.as_deref(),
        submit_on_enter: false,
    };

    let action = match app.input_overlays.get_mut(idx) {
        Some(OverlayState::Prompt { surface, .. }) => {
            crate::input_surface::handle_key(surface, key, &config)
        }
        _ => return true,
    };

    match action {
        crate::input_surface::InputAction::Consumed => {
            app.mark_dirty();
        }
        crate::input_surface::InputAction::Submit(_) => {
            prompt::submit_input_overlay(app).await;
        }
        crate::input_surface::InputAction::Close => {
            prompt::close_input_overlay(app);
        }
        crate::input_surface::InputAction::Passthrough(_) => {}
        crate::input_surface::InputAction::CompileDecision {
            accepted,
            context,
            compiled_output,
        } => {
            let session_id = match app.input_overlays.get(idx) {
                Some(OverlayState::Prompt { purpose, .. }) => match purpose {
                    crate::types::PromptPurpose::ContinueSession(id) => Some(*id),
                    _ => None,
                },
                _ => None,
            };
            let params = rsi_common::rpc::SaveCompiledPromptParams {
                session_id,
                original_input: context.original_input,
                compiled_output,
                contract_status: context.contract_status,
                layer_semantic: context.layer_semantic,
                layer_syntactic: context.layer_syntactic,
                layer_deictic: context.layer_deictic,
                layer_discourse: context.layer_discourse,
                layer_pragmatic: context.layer_pragmatic,
                accepted,
            };
            let _ = app.client.save_compiled_prompt(params).await;
            app.mark_dirty();
        }
    }

    true
}

/// Handle keys for the InputModal overlay (quarter-size centered popup).
async fn handle_input_modal_key(app: &mut App, key: KeyEvent) {
    // Ctrl+Shift+A — open AI chat for Q&A about the text
    if matches!(key.code, KeyCode::Char('a') | KeyCode::Char('A'))
        && key.modifiers == (KeyModifiers::CONTROL | KeyModifiers::SHIFT)
    {
        if app.prompt_processor.is_none() {
            app.notify("AI assistant requires prompt processor");
            return;
        }
        let (source_text, overlay_id) = match &app.overlay {
            OverlayState::InputModal {
                overlay_id,
                surface,
                ..
            } => (surface.content_trimmed(), *overlay_id),
            _ => return,
        };
        if source_text.is_empty() {
            return;
        }
        app.push_current_overlay();
        app.overlay = OverlayState::AiChat {
            messages: Vec::new(),
            input: String::new(),
            source_text,
            source: crate::types::AiAssistantSource::OverlaySurface(overlay_id),
            in_flight: false,
            scroll_offset: 0,
        };
        app.mark_dirty();
        return;
    }

    // Ctrl+A — open AI command input for text transformation
    if key.code == KeyCode::Char('a') && key.modifiers.contains(KeyModifiers::CONTROL) {
        if app.prompt_processor.is_none() {
            app.notify("AI assistant requires prompt processor");
            return;
        }
        let (source_text, overlay_id) = match &app.overlay {
            OverlayState::InputModal {
                overlay_id,
                surface,
                ..
            } => (surface.content_trimmed(), *overlay_id),
            _ => return,
        };
        if source_text.is_empty() {
            return;
        }
        app.push_current_overlay();
        app.overlay = OverlayState::AiCommand {
            command: String::new(),
            source_text,
            source: crate::types::AiAssistantSource::OverlaySurface(overlay_id),
            in_flight: false,
        };
        app.mark_dirty();
        return;
    }

    // Ctrl+Y — trigger prompt compilation via local model
    if key.code == KeyCode::Char('y') && key.modifiers.contains(KeyModifiers::CONTROL) {
        if let OverlayState::InputModal {
            overlay_id,
            surface,
            ..
        } = &mut app.overlay
        {
            if surface.correction_in_flight || app.prompt_compile_rx.is_some() {
                return;
            }
            if app.prompt_processor.is_none() {
                app.notify(
                    "No prompt processor configured (set settings.prompt_processor.enabled)",
                );
                return;
            }
            let input = surface.content_trimmed();
            if input.is_empty() {
                return;
            }
            surface.correction_in_flight = true;
            let overlay_id = *overlay_id;
            let original_input = input.clone();
            let config = app.settings.prompt_processor.clone();
            let (tx, rx) = tokio::sync::oneshot::channel();
            app.prompt_compile_rx = Some((overlay_id, original_input, rx));
            tokio::spawn(async move {
                let processor = crate::prompt_processor::build_processor(&config);
                let result = match processor {
                    Some(p) => p.compile(&input).await.map_err(|e| e.to_string()),
                    None => Err("Processor disabled".to_string()),
                };
                let _ = tx.send(result);
            });
            app.mark_dirty();
        }
        return;
    }

    // Ctrl+Shift+G — grammar/spelling correction only (no prompt restructuring)
    if key
        .modifiers
        .contains(KeyModifiers::CONTROL | KeyModifiers::SHIFT)
        && matches!(key.code, KeyCode::Char('g') | KeyCode::Char('G'))
    {
        if let OverlayState::InputModal {
            overlay_id,
            surface,
            ..
        } = &mut app.overlay
        {
            if surface.correction_in_flight || app.prompt_compile_rx.is_some() {
                return;
            }
            if app.prompt_processor.is_none() {
                app.notify(
                    "No prompt processor configured (set settings.prompt_processor.enabled)",
                );
                return;
            }
            let input = surface.content_trimmed();
            if input.is_empty() {
                return;
            }
            surface.correction_in_flight = true;
            let overlay_id = *overlay_id;
            let config = app.settings.prompt_processor.clone();
            let (tx, rx) = tokio::sync::oneshot::channel();
            app.prompt_compile_rx = Some((overlay_id, String::new(), rx));
            tokio::spawn(async move {
                let processor = crate::prompt_processor::build_processor(&config);
                let result = match processor {
                    Some(p) => p
                        .send(crate::prompt_processor::GRAMMAR_SYSTEM_PROMPT, &input)
                        .await
                        .map(|text| crate::prompt_processor::CompileResult {
                            compiled: text,
                            contract: crate::prompt_processor::OutputContract::Complete,
                            layer_validation: crate::prompt_processor::LayerValidation {
                                semantic: true,
                                syntactic: true,
                                deictic: true,
                                discourse: true,
                                pragmatic: true,
                            },
                        })
                        .map_err(|e| e.to_string()),
                    None => Err("Processor disabled".to_string()),
                };
                let _ = tx.send(result);
            });
            app.mark_dirty();
        }
        return;
    }

    // Ctrl+V paste from clipboard (delegates to shared paste_into_overlay)
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('v') | KeyCode::Char('V'))
    {
        paste_into_overlay(app);
        return;
    }

    // Delegate to shared InputSurface handler
    let config = crate::input_surface::InputSurfaceConfig {
        pass_through_unhandled: false,
        available_commands: &[], // No command suggestions in the input modal
        working_dir: None,       // No file suggestions in the input modal
        submit_on_enter: app.settings.submit_on_enter,
    };

    let action = if let OverlayState::InputModal { surface, .. } = &mut app.overlay {
        crate::input_surface::handle_key(surface, key, &config)
    } else {
        return;
    };

    match action {
        crate::input_surface::InputAction::Consumed => {
            app.mark_dirty();
        }
        crate::input_surface::InputAction::Submit(content) => {
            // Extract session_id and close
            // Snapshot the modal's raw lines alongside the id: both the modal
            // surface and the input bar are destroyed below, so a rejected
            // continue needs them to put the message back verbatim.
            let (session_id, typed) = if let OverlayState::InputModal {
                session_id,
                surface,
                ..
            } = &app.overlay
            {
                (*session_id, surface.textarea.lines().to_vec())
            } else {
                return;
            };
            app.restore_previous_overlay();
            if !content.is_empty() {
                // Clear the input bar (message is being sent, not transferred back)
                if let Some(state) = app.sessions.get_mut(&session_id) {
                    state.input_bar.surface.clear();
                    state.follow_tail = true;
                }
                // Both the modal surface and the input bar were destroyed above,
                // so a rejected continue would lose the message entirely. The
                // Close arm below already transfers modal text back to the input
                // bar on cancel; do the same on failure.
                if !app.continue_session(session_id, &content).await {
                    app.restore_continue_lines(session_id, typed);
                }
            }
        }
        crate::input_surface::InputAction::Close => {
            // Transfer modal text back to the input bar before closing
            if let OverlayState::InputModal {
                surface,
                session_id,
                ..
            } = &app.overlay
            {
                let content = surface.content_trimmed();
                let sid = *session_id;
                // We need to drop the borrow before mutating
                let lines: Vec<String> = content.lines().map(str::to_string).collect();
                app.restore_previous_overlay();
                if !content.is_empty() {
                    if let Some(state) = app.sessions.get_mut(&sid) {
                        state.input_bar.surface =
                            crate::input_surface::InputSurface::new_insert_with_content(lines);
                        // Return to normal mode in the input bar (matches input bar default)
                        state.input_bar.surface.mode = crate::types::PopupMode::Normal;
                    }
                }
            } else {
                app.restore_previous_overlay();
            }
            app.mark_dirty();
        }
        crate::input_surface::InputAction::Passthrough(_) => {
            // Modal never passes through
        }
        crate::input_surface::InputAction::CompileDecision {
            accepted,
            context,
            compiled_output,
        } => {
            let session_id = if let OverlayState::InputModal { session_id, .. } = &app.overlay {
                Some(*session_id)
            } else {
                None
            };
            let params = rsi_common::rpc::SaveCompiledPromptParams {
                session_id,
                original_input: context.original_input,
                compiled_output,
                contract_status: context.contract_status,
                layer_semantic: context.layer_semantic,
                layer_syntactic: context.layer_syntactic,
                layer_deictic: context.layer_deictic,
                layer_discourse: context.layer_discourse,
                layer_pragmatic: context.layer_pragmatic,
                accepted,
            };
            let _ = app.client.save_compiled_prompt(params).await;
            app.mark_dirty();
        }
    }
}

/// Cycle effort level for new sessions.
/// None -> model default -> next levels -> low.
fn cycle_effort(
    app: &mut App,
    model_override: Option<&str>,
    provider_override: Option<rsi_common::types::SessionProvider>,
) {
    let model = model_override
        .filter(|m| !m.is_empty())
        .or(app.selected_model.as_deref())
        .unwrap_or("");
    let provider = provider_override.unwrap_or(app.selected_provider);
    let ladder = match provider {
        rsi_common::types::SessionProvider::Claude
        | rsi_common::types::SessionProvider::Codex
        | rsi_common::types::SessionProvider::Pioneer
        | rsi_common::types::SessionProvider::OpenRouter
        | rsi_common::types::SessionProvider::Bedrock
        | rsi_common::types::SessionProvider::CodexAppServer => {
            rsi_common::model_utils::effort_ladder(model)
        }
        _ => &[],
    };
    if ladder.is_empty() {
        app.selected_effort = None;
        return;
    }

    let next = if let Some(index) = app
        .selected_effort
        .as_deref()
        .and_then(|effort| ladder.iter().position(|level| *level == effort))
    {
        ladder[(index + 1) % ladder.len()]
    } else {
        rsi_common::model_utils::default_effort_level(model).unwrap_or("high")
    };
    app.selected_effort = Some(next.to_string());
}

/// Try to paste clipboard content into the active overlay.
/// Returns `true` when the overlay owns the paste.
/// Called from the event loop before the Press-only filter to handle terminals
/// (like Ghostty) that send Ctrl+V as a Release event.
pub fn try_paste_overlay(app: &mut App) -> bool {
    if matches!(app.overlay, OverlayState::CommandPalette { .. }) {
        command_palette::paste_clipboard(app);
        return true;
    }
    // Telescope has its own query field — handle before the general insert-mode check
    if matches!(app.overlay, OverlayState::Telescope { .. }) {
        telescope::paste_clipboard_telescope(app);
        return true;
    }
    if matches!(app.overlay, OverlayState::FileExplorer { .. }) {
        return file_explorer::paste_clipboard(app);
    }
    paste_into_overlay(app);
    true
}

/// Try to paste pre-read text into the active overlay (for bracketed paste).
/// Automatically enters insert mode if the overlay has a text surface but is in normal mode.
pub fn try_paste_text_overlay(app: &mut App, text: &str) -> bool {
    if matches!(app.overlay, OverlayState::CommandPalette { .. }) {
        command_palette::paste_text(app, text);
        return true;
    }
    // Telescope has its own query field — handle before the general insert-mode check
    if matches!(app.overlay, OverlayState::Telescope { .. }) {
        telescope::paste_text_telescope(app, text);
        return true;
    }
    if matches!(app.overlay, OverlayState::FileExplorer { .. }) {
        return file_explorer::paste_text(app, text);
    }
    paste_text_into_overlay(app, text);
    true
}

/// Paste clipboard content (image or text) into the overlay textarea.
fn paste_into_overlay(app: &mut App) {
    let paste_dir = app.paste_dir.clone();
    match crate::clipboard::read_clipboard(&paste_dir) {
        crate::clipboard::ClipboardContent::Image { reference, .. } => {
            paste_image_into_overlay(app, &reference);
        }
        crate::clipboard::ClipboardContent::Text(text) => {
            paste_text_into_overlay(app, &text);
        }
        crate::clipboard::ClipboardContent::Empty => {
            app.notify("Clipboard empty");
        }
    }
}

/// Paste pre-read text into the active overlay textarea with normalization and wrapping.
fn paste_text_into_overlay(app: &mut App, text: &str) {
    let normalized = crate::input_surface::normalize_pasted_text(text);
    let mut handled = false;

    match &mut app.overlay {
        OverlayState::Prompt { surface, .. } | OverlayState::InputModal { surface, .. } => {
            paste_text_into_input_surface(surface, &normalized);
            handled = true;
        }
        OverlayState::QuestionModal { mode, textarea, .. } => {
            if *mode != PopupMode::Insert {
                *mode = PopupMode::Insert;
            }
            textarea.insert_str(&normalized);
            handled = true;
        }
        OverlayState::ProviderForm {
            focused_field,
            name,
            base_url,
            api_key,
            default_model,
            ..
        } => {
            let surface = match *focused_field {
                0 => Some(name),
                1 => Some(base_url),
                2 => Some(api_key),
                3 => Some(default_model),
                _ => None,
            };
            if let Some(surface) = surface {
                paste_text_into_input_surface(surface, &normalized);
            }
            handled = true;
        }
        OverlayState::ProjectForm {
            focused_field,
            name,
            path,
            ..
        } => {
            match *focused_field {
                0 => name.push_str(&normalized),
                1 => path.push_str(&normalized),
                _ => {}
            }
            handled = true;
        }
        OverlayState::MessageBridgeForm {
            focused_field,
            account,
            allow_from,
            working_dir,
            ..
        } => {
            match *focused_field {
                1 => account.push_str(&normalized),
                2 => allow_from.push_str(&normalized),
                3 => working_dir.push_str(&normalized),
                _ => {}
            }
            handled = true;
        }
        OverlayState::HookForm {
            focused_field,
            matcher,
            command,
            timeout,
            ..
        } => {
            match *focused_field {
                1 => matcher.push_str(&normalized),
                2 => command.push_str(&normalized),
                3 => timeout.push_str(&normalized),
                _ => {}
            }
            handled = true;
        }
        OverlayState::BudgetPolicyForm {
            focused_field,
            scope_id,
            purpose,
            max_total_tokens,
            max_concurrency,
            max_calls_per_window,
            rate_window_seconds,
            alert_threshold_ratio,
            ..
        } => {
            match *focused_field {
                1 => scope_id.push_str(&normalized),
                2 => purpose.push_str(&normalized),
                4 => max_total_tokens.push_str(&normalized),
                5 => max_concurrency.push_str(&normalized),
                6 => max_calls_per_window.push_str(&normalized),
                7 => rate_window_seconds.push_str(&normalized),
                8 => alert_threshold_ratio.push_str(&normalized),
                _ => {}
            }
            handled = true;
        }
        OverlayState::MemorySearch { query, .. } => {
            query.push_str(&normalized);
            handled = true;
        }
        OverlayState::RenameSession { title, .. } => {
            title.push_str(&normalized);
            handled = true;
        }
        OverlayState::LabelForm {
            focused_field,
            name,
            description,
            ..
        } => {
            match *focused_field {
                0 => name.push_str(&normalized),
                1 => description.push_str(&normalized),
                _ => {}
            }
            handled = true;
        }
        OverlayState::AiCommand { command, .. } => {
            command.push_str(&normalized);
            handled = true;
        }
        OverlayState::AiChat { input, .. } => {
            input.push_str(&normalized);
            handled = true;
        }
        OverlayState::ScheduleForm {
            focused_field,
            name,
            message,
            interval,
            anchor_date,
            anchor_time,
            ..
        } => {
            match *focused_field {
                0 => name.push_str(&normalized),
                1 => message.push_str(&normalized),
                3 => interval.push_str(&normalized),
                4 => anchor_date.push_str(&normalized),
                5 => anchor_time.push_str(&normalized),
                _ => {}
            }
            handled = true;
        }
        OverlayState::CreateEntityForm {
            name,
            body,
            focused_field,
            insert_mode,
            ..
        } => {
            if *focused_field == crate::types::CreateEntityField::Body {
                // S1: the multi-line body consumes the RAW paste text —
                // newlines are content here, not dictation wrapping
                // (F-020/F-021/F-022). Single-line fields keep `normalized`.
                paste_text_into_input_surface(body, text);
                handled = true;
            } else if *focused_field == crate::types::CreateEntityField::Name && *insert_mode {
                // Only paste into the Name field when insert_mode + Name focused.
                name.push_str(&normalized);
                handled = true;
            }
        }
        OverlayState::GraphReview {
            mode, edit_buffer, ..
        } => {
            // Only paste when a node field (name/instructions/strategy) is being
            // edited inline; the buffer is committed to the node on Enter.
            if mode.editing_field().is_some() {
                edit_buffer.push_str(&normalized);
                handled = true;
            }
        }
        _ => {}
    }

    if !handled {
        // Fallback to any stacked input overlay.
        if let Some(OverlayState::Prompt { surface, .. }) = app.focused_input_overlay_mut() {
            paste_text_into_input_surface(surface, &normalized);
            handled = true;
        }
    }

    if handled {
        app.mark_dirty();
    }
}

fn paste_image_into_overlay(app: &mut App, reference: &str) {
    let mut handled = false;

    match &mut app.overlay {
        OverlayState::Prompt { surface, .. } | OverlayState::InputModal { surface, .. } => {
            paste_text_into_input_surface(surface, reference);
            app.notify("Image pasted");
            handled = true;
        }
        OverlayState::ProviderForm { .. }
        | OverlayState::QuestionModal { .. }
        | OverlayState::ProjectForm { .. }
        | OverlayState::MessageBridgeForm { .. }
        | OverlayState::HookForm { .. }
        | OverlayState::BudgetPolicyForm { .. }
        | OverlayState::MemorySearch { .. }
        | OverlayState::RenameSession { .. }
        | OverlayState::LabelForm { .. }
        | OverlayState::AiCommand { .. }
        | OverlayState::AiChat { .. }
        | OverlayState::ScheduleForm { .. }
        | OverlayState::CreateEntityForm { .. } => {
            app.notify("Image paste isn't supported here");
            handled = true;
        }
        _ => {}
    }

    if !handled {
        if let Some(OverlayState::Prompt { surface, .. }) = app.focused_input_overlay_mut() {
            paste_text_into_input_surface(surface, reference);
            app.notify("Image pasted");
            handled = true;
        }
    }

    if handled {
        app.mark_dirty();
    }
}

fn paste_text_into_input_surface(surface: &mut crate::input_surface::InputSurface, text: &str) {
    surface.insert_pasted_text(text);
}

#[cfg(test)]
mod file_explorer_paste_tests {
    use super::*;
    use crate::client::DaemonClient;
    use std::path::PathBuf;

    fn explorer_app(finder_active: bool, explorer_focused: bool) -> App {
        let mut app = App::new(DaemonClient::new(PathBuf::from("/tmp/test.sock")));
        app.overlay = OverlayState::FileExplorer {
            root: PathBuf::from("/tmp"),
            entries: Vec::new(),
            selected_index: 0,
            scroll_offset: 0,
            show_hidden: false,
            trash: Vec::new(),
            pending_yank: false,
            pending_delete: false,
            finder_active,
            finder_query: String::new(),
            finder_cache: vec![PathBuf::from("src/file_explorer.rs")],
            finder_results: Vec::new(),
            finder_selected: 0,
            explorer_focused,
        };
        app
    }

    #[test]
    fn bracketed_paste_updates_file_explorer_finder_only() {
        let mut app = explorer_app(true, true);

        assert!(try_paste_text_overlay(&mut app, "file\nexplorer"));

        let OverlayState::FileExplorer {
            finder_query,
            finder_results,
            ..
        } = &app.overlay
        else {
            panic!("expected file explorer overlay");
        };
        assert_eq!(finder_query, "fileexplorer");
        assert_eq!(finder_results, &vec![0]);
    }

    #[test]
    fn bracketed_paste_in_file_tree_does_not_fall_through() {
        let mut app = explorer_app(false, true);

        assert!(try_paste_text_overlay(&mut app, "hidden prompt text"));

        let OverlayState::FileExplorer { finder_query, .. } = &app.overlay else {
            panic!("expected file explorer overlay");
        };
        assert!(finder_query.is_empty());
        assert_eq!(
            app.notifications
                .back()
                .map(|notification| notification.message.as_str()),
            Some("Open finder with / before pasting")
        );
    }

    #[test]
    fn unfocused_file_explorer_leaves_paste_for_file_viewer() {
        let mut app = explorer_app(true, false);

        assert!(!try_paste_text_overlay(&mut app, "viewer text"));

        let OverlayState::FileExplorer { finder_query, .. } = &app.overlay else {
            panic!("expected file explorer overlay");
        };
        assert!(finder_query.is_empty());
        assert!(app.notifications.is_empty());
    }
}

#[cfg(test)]
mod text_entry_leader_tests {
    use super::*;
    use crate::client::DaemonClient;
    use crate::types::SourceWorktreeSettlementOverlayState;
    use crossterm::event::KeyModifiers;
    use std::path::PathBuf;

    fn app() -> App {
        App::new(DaemonClient::new(PathBuf::from("/tmp/test.sock")))
    }

    #[test]
    fn keybindings_help_has_one_dispatch_path() {
        let source = include_str!("mod.rs");
        assert_eq!(
            source
                .matches(concat!(
                    "keybindings_help::",
                    "handle_keybindings_help_key(app, key);"
                ))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn space_enters_dialectic_question() {
        let mut app = app();
        dialectic::open_dialectic(&mut app, Some("how".into()));
        handle_overlay_key(
            &mut app,
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
        )
        .await;
        let OverlayState::Dialectic { input, .. } = &app.overlay else {
            panic!("dialectic")
        };
        assert_eq!(input, "how ");
    }

    #[tokio::test]
    async fn space_enters_card_fact() {
        let mut app = app();
        app.overlay = OverlayState::CardEditor {
            entity_type: "user".into(),
            entity_id: "self".into(),
            display_name: "User".into(),
            facts: vec!["fact".into()],
            selected_index: 0,
            scroll_offset: 0,
            editing: Some("new".into()),
            loading: false,
            pending_delete: false,
        };
        handle_overlay_key(
            &mut app,
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
        )
        .await;
        let OverlayState::CardEditor { editing, .. } = &app.overlay else {
            panic!("card editor")
        };
        assert_eq!(editing.as_deref(), Some("new "));
    }

    #[tokio::test]
    async fn space_enters_settlement_authorization() {
        let mut app = app();
        app.overlay =
            OverlayState::SourceWorktreeSettlement(SourceWorktreeSettlementOverlayState {
                cohorts: vec![],
                selected_index: 0,
                scroll_offset: 0,
                audit: None,
                receipt: None,
                authorization_input: "allow".into(),
                authorization_active: true,
                idempotency_key: None,
                last_error: None,
            });
        handle_overlay_key(
            &mut app,
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
        )
        .await;
        let OverlayState::SourceWorktreeSettlement(state) = &app.overlay else {
            panic!("settlement")
        };
        assert_eq!(state.authorization_input, "allow ");
    }
}
