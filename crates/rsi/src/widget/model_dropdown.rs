//! Model dropdown widget key handling and provider cycling logic.
//!
//! This module contains the input-handling half of the reusable `ModelDropdown`
//! widget. Rendering lives in `ui::widget::model_dropdown`. Parents (status bar,
//! input modals, settings) own a `ModelDropdownState` and delegate key events
//! here when the dropdown is open.

use crate::overlay::list;
use crate::settings::CustomProviderEntry;
use crate::types::ModelDropdownState;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rsi_common::types::SessionProvider;

const BUILTIN: [SessionProvider; 9] = [
    SessionProvider::Claude,
    SessionProvider::Codex,
    SessionProvider::Pioneer,
    SessionProvider::OpenRouter,
    SessionProvider::Bedrock,
    SessionProvider::Local,
    SessionProvider::Antigravity,
    SessionProvider::CodexAppServer,
    SessionProvider::Harness,
];

/// Result of handling a key event in the model dropdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelDropdownAction {
    /// Key was consumed, no further action needed.
    Consumed,
    /// A model was selected -- caller should apply it.
    Selected(String),
    /// Dropdown was dismissed via Esc/q.
    Dismissed,
    /// Provider was cycled -- caller should trigger model discovery.
    ProviderCycled,
    /// Key was not consumed by the dropdown.
    Ignored,
}

/// Handle a key event for the model dropdown.
/// Returns `ModelDropdownAction` indicating what happened.
pub fn handle_model_dropdown_key(
    state: &mut ModelDropdownState,
    key: &KeyEvent,
    custom_providers: &[CustomProviderEntry],
) -> ModelDropdownAction {
    handle_model_dropdown_key_with_providers(state, key, &BUILTIN, custom_providers)
}

/// Reuse dropdown input with a caller-owned provider list; model IDs still come from the catalog.
pub fn handle_model_dropdown_key_with_providers(
    state: &mut ModelDropdownState,
    key: &KeyEvent,
    builtins: &[SessionProvider],
    custom_providers: &[CustomProviderEntry],
) -> ModelDropdownAction {
    // j/k/g/G/Down/Up list navigation
    if list::handle_list_nav_key(&mut state.selected_index, state.models.len(), key) {
        return ModelDropdownAction::Consumed;
    }

    match key.code {
        KeyCode::BackTab => {
            cycle_provider_with_providers(state, false, builtins, custom_providers);
            ModelDropdownAction::ProviderCycled
        }
        KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => {
            cycle_provider_with_providers(state, false, builtins, custom_providers);
            ModelDropdownAction::ProviderCycled
        }
        KeyCode::Tab => {
            cycle_provider_with_providers(state, true, builtins, custom_providers);
            ModelDropdownAction::ProviderCycled
        }
        KeyCode::Enter => {
            if state.selected_index < state.models.len() {
                let (model_id, _) = &state.models[state.selected_index];
                ModelDropdownAction::Selected(model_id.clone())
            } else {
                ModelDropdownAction::Consumed
            }
        }
        KeyCode::Char(c @ '1'..='9') => {
            let idx = (c as usize) - ('1' as usize);
            if idx < state.models.len() {
                let (model_id, _) = &state.models[idx];
                ModelDropdownAction::Selected(model_id.clone())
            } else {
                ModelDropdownAction::Consumed
            }
        }
        KeyCode::Esc | KeyCode::Char('q') => ModelDropdownAction::Dismissed,
        _ => ModelDropdownAction::Ignored,
    }
}

/// Cycle the dropdown to the next (forward=true) or previous provider.
/// Built-in cycle: Claude -> Codex -> Pioneer -> OpenRouter -> Bedrock -> Local -> Antigravity ->
/// CodexAppServer -> Harness, then custom providers, then wrap.
pub fn cycle_provider(
    state: &mut ModelDropdownState,
    forward: bool,
    custom_providers: &[CustomProviderEntry],
) {
    cycle_provider_with_providers(state, forward, &BUILTIN, custom_providers);
}

fn cycle_provider_with_providers(
    state: &mut ModelDropdownState,
    forward: bool,
    builtins: &[SessionProvider],
    custom_providers: &[CustomProviderEntry],
) {
    let custom_count = custom_providers.len();
    let base_slots = builtins.len();

    // Current slot: builtins first, then custom providers.
    let current_slot = if matches!(state.provider, SessionProvider::Local)
        && state.custom_provider_index.is_some()
        && custom_count > 0
    {
        base_slots
            + state
                .custom_provider_index
                .unwrap_or(0)
                .min(custom_count.saturating_sub(1))
    } else {
        builtins
            .iter()
            .position(|p| *p == state.provider)
            .unwrap_or(0)
    };

    let total_slots = base_slots + custom_count;
    if total_slots == 0 {
        return;
    }

    let next_slot = if forward {
        (current_slot + 1) % total_slots
    } else {
        (current_slot + total_slots - 1) % total_slots
    };

    if next_slot < base_slots {
        let provider = builtins[next_slot];
        state.provider = provider;
        state.custom_provider_index = None;
        state.models = crate::app::models_for_provider(provider);
    } else {
        let ci = next_slot - base_slots;
        let entry = &custom_providers[ci];
        let model_id = if entry.default_model.is_empty() {
            entry.name.clone()
        } else {
            entry.default_model.clone()
        };
        state.provider = SessionProvider::Local;
        state.custom_provider_index = Some(ci);
        state.models = vec![(model_id, entry.name.clone())];
    };

    state.selected_index = 0;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn make_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn make_shift_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::SHIFT)
    }

    fn sample_models() -> Vec<(String, String)> {
        vec![
            ("claude-sonnet-5".into(), "Claude Sonnet 5".into()),
            ("claude-opus-4-20250514".into(), "Claude Opus 4".into()),
            ("claude-haiku-3.5".into(), "Claude Haiku 3.5".into()),
        ]
    }

    #[test]
    fn test_state_new_preselects_model() {
        let models = sample_models();
        let state = ModelDropdownState::new(
            SessionProvider::Claude,
            models.clone(),
            Some("claude-opus-4-20250514"),
        );
        assert!(state.open);
        assert_eq!(state.selected_index, 1);
    }

    #[test]
    fn test_state_new_unknown_model_defaults_to_zero() {
        let models = sample_models();
        let state =
            ModelDropdownState::new(SessionProvider::Claude, models.clone(), Some("nonexistent"));
        assert_eq!(state.selected_index, 0);
    }

    #[test]
    fn test_state_new_no_current_model() {
        let models = sample_models();
        let state = ModelDropdownState::new(SessionProvider::Claude, models.clone(), None);
        assert_eq!(state.selected_index, 0);
    }

    #[test]
    fn test_state_toggle() {
        let mut state = ModelDropdownState::default();
        assert!(!state.open);
        state.toggle();
        assert!(state.open);
        state.toggle();
        assert!(!state.open);
    }

    #[test]
    fn test_state_close() {
        let mut state = ModelDropdownState::new(SessionProvider::Claude, sample_models(), None);
        assert!(state.open);
        state.close();
        assert!(!state.open);
    }

    #[test]
    fn test_state_closed_constructor() {
        let state = ModelDropdownState::closed(
            SessionProvider::Claude,
            sample_models(),
            Some("claude-opus-4-20250514"),
        );
        assert!(!state.open);
        assert_eq!(state.selected_index, 1);
    }

    #[test]
    fn test_key_navigation_j_k() {
        let mut state = ModelDropdownState::new(SessionProvider::Claude, sample_models(), None);

        let action = handle_model_dropdown_key(&mut state, &make_key(KeyCode::Char('j')), &[]);
        assert_eq!(action, ModelDropdownAction::Consumed);
        assert_eq!(state.selected_index, 1);

        let action = handle_model_dropdown_key(&mut state, &make_key(KeyCode::Char('k')), &[]);
        assert_eq!(action, ModelDropdownAction::Consumed);
        assert_eq!(state.selected_index, 0);
    }

    #[test]
    fn test_key_navigation_g_big_g() {
        let mut state = ModelDropdownState::new(SessionProvider::Claude, sample_models(), None);

        let action = handle_model_dropdown_key(&mut state, &make_key(KeyCode::Char('G')), &[]);
        assert_eq!(action, ModelDropdownAction::Consumed);
        assert_eq!(state.selected_index, 2);

        let action = handle_model_dropdown_key(&mut state, &make_key(KeyCode::Char('g')), &[]);
        assert_eq!(action, ModelDropdownAction::Consumed);
        assert_eq!(state.selected_index, 0);
    }

    #[test]
    fn test_key_enter_selects() {
        let mut state = ModelDropdownState::new(SessionProvider::Claude, sample_models(), None);
        state.selected_index = 1;

        let action = handle_model_dropdown_key(&mut state, &make_key(KeyCode::Enter), &[]);
        assert_eq!(
            action,
            ModelDropdownAction::Selected("claude-opus-4-20250514".into())
        );
    }

    #[test]
    fn test_key_number_direct_select() {
        let mut state = ModelDropdownState::new(SessionProvider::Claude, sample_models(), None);

        let action = handle_model_dropdown_key(&mut state, &make_key(KeyCode::Char('2')), &[]);
        assert_eq!(
            action,
            ModelDropdownAction::Selected("claude-opus-4-20250514".into())
        );
    }

    #[test]
    fn test_key_number_out_of_range() {
        let mut state = ModelDropdownState::new(SessionProvider::Claude, sample_models(), None);

        let action = handle_model_dropdown_key(&mut state, &make_key(KeyCode::Char('9')), &[]);
        assert_eq!(action, ModelDropdownAction::Consumed);
    }

    #[test]
    fn test_key_esc_dismisses() {
        let mut state = ModelDropdownState::new(SessionProvider::Claude, sample_models(), None);

        let action = handle_model_dropdown_key(&mut state, &make_key(KeyCode::Esc), &[]);
        assert_eq!(action, ModelDropdownAction::Dismissed);
    }

    #[test]
    fn test_key_q_dismisses() {
        let mut state = ModelDropdownState::new(SessionProvider::Claude, sample_models(), None);

        let action = handle_model_dropdown_key(&mut state, &make_key(KeyCode::Char('q')), &[]);
        assert_eq!(action, ModelDropdownAction::Dismissed);
    }

    #[test]
    fn test_key_unhandled_returns_ignored() {
        let mut state = ModelDropdownState::new(SessionProvider::Claude, sample_models(), None);

        let action = handle_model_dropdown_key(&mut state, &make_key(KeyCode::Char('x')), &[]);
        assert_eq!(action, ModelDropdownAction::Ignored);
    }

    #[test]
    fn test_tab_cycles_provider_forward() {
        let mut state = ModelDropdownState::new(SessionProvider::Claude, sample_models(), None);

        let action = handle_model_dropdown_key(&mut state, &make_key(KeyCode::Tab), &[]);
        assert_eq!(action, ModelDropdownAction::ProviderCycled);
        assert_eq!(state.provider, SessionProvider::Codex);
    }

    #[test]
    fn test_codex_cycles_to_pioneer_with_supported_offline_fallback() {
        let mut state = ModelDropdownState::new(SessionProvider::Codex, vec![], None);

        cycle_provider(&mut state, true, &[]);

        assert_eq!(state.provider, SessionProvider::Pioneer);
        assert_eq!(
            state.models,
            vec![("claude-sonnet-5".to_string(), "Claude Sonnet 5".to_string())]
        );
    }

    #[test]
    fn test_antigravity_cycles_to_codex_app_server() {
        let mut state = ModelDropdownState::new(SessionProvider::Antigravity, vec![], None);

        cycle_provider(&mut state, true, &[]);

        assert_eq!(state.provider, SessionProvider::CodexAppServer);
        assert_eq!(
            state.models,
            crate::app::models_for_provider(SessionProvider::CodexAppServer)
        );
    }

    #[test]
    fn test_pioneer_cycles_to_openrouter_with_offline_fallback() {
        let mut state = ModelDropdownState::new(SessionProvider::Pioneer, vec![], None);

        cycle_provider(&mut state, true, &[]);

        assert_eq!(state.provider, SessionProvider::OpenRouter);
        assert_eq!(
            state.models,
            crate::app::models_for_provider(SessionProvider::OpenRouter)
        );
    }

    #[test]
    fn test_backtab_cycles_provider_backward() {
        let mut state = ModelDropdownState::new(SessionProvider::Codex, vec![], None);

        let action = handle_model_dropdown_key(&mut state, &make_key(KeyCode::BackTab), &[]);
        assert_eq!(action, ModelDropdownAction::ProviderCycled);
        assert_eq!(state.provider, SessionProvider::Claude);
    }

    #[test]
    fn test_shift_tab_cycles_provider_backward() {
        let mut state = ModelDropdownState::new(SessionProvider::Codex, vec![], None);

        let action = handle_model_dropdown_key(&mut state, &make_shift_key(KeyCode::Tab), &[]);
        assert_eq!(action, ModelDropdownAction::ProviderCycled);
        assert_eq!(state.provider, SessionProvider::Claude);
    }

    #[test]
    fn test_pioneer_cycles_backward_to_codex() {
        let mut state = ModelDropdownState::new(SessionProvider::Pioneer, vec![], None);

        cycle_provider(&mut state, false, &[]);

        assert_eq!(state.provider, SessionProvider::Codex);
    }

    #[test]
    fn test_cycle_provider_wraps_forward() {
        // Harness is last built-in; cycling forward wraps to Claude
        let mut state = ModelDropdownState::new(SessionProvider::Harness, vec![], None);
        cycle_provider(&mut state, true, &[]);
        assert_eq!(state.provider, SessionProvider::Claude);
    }

    #[test]
    fn test_cycle_provider_wraps_backward() {
        // Claude is first built-in; cycling backward wraps to Harness
        let mut state = ModelDropdownState::new(SessionProvider::Claude, vec![], None);
        cycle_provider(&mut state, false, &[]);
        assert_eq!(state.provider, SessionProvider::Harness);
    }

    #[test]
    fn test_cycle_provider_with_custom() {
        let custom = vec![CustomProviderEntry {
            id: uuid::Uuid::new_v4(),
            name: "MyOllama".into(),
            base_url: "http://localhost:11434".into(),
            api_key: String::new(),
            default_model: "llama3".into(),
        }];

        // Harness is last built-in; cycling forward goes to custom provider
        let mut state = ModelDropdownState::new(SessionProvider::Harness, vec![], None);
        cycle_provider(&mut state, true, &custom);
        assert_eq!(state.provider, SessionProvider::Local);
        assert_eq!(state.custom_provider_index, Some(0));
        assert_eq!(state.models, vec![("llama3".into(), "MyOllama".into())]);

        // Cycling forward again wraps to Claude
        cycle_provider(&mut state, true, &custom);
        assert_eq!(state.provider, SessionProvider::Claude);
        assert_eq!(state.custom_provider_index, None);
    }

    #[test]
    fn test_cycle_provider_resets_selected_index() {
        let mut state = ModelDropdownState::new(SessionProvider::Claude, sample_models(), None);
        state.selected_index = 2;
        cycle_provider(&mut state, true, &[]);
        assert_eq!(state.selected_index, 0);
    }
}
