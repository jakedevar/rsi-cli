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
    state.reconcile_filter_selection();
    // While the filter is being typed, printable keys are query text: they
    // never reach vim navigation, numeric selection, `q` or a parent's leader.
    // A query edit re-highlights the first match, so typing then Enter picks
    // the best hit.
    if state.filter_editing {
        match key.code {
            // Esc leaves the query and its filtered list in place for j/k and
            // 1-9; the next Esc closes the picker.
            KeyCode::Esc => {
                state.filter_editing = false;
                return ModelDropdownAction::Consumed;
            }
            KeyCode::Backspace => {
                if state.filter_query.pop().is_none() {
                    // Backspace on an empty query leaves search mode.
                    state.filter_editing = false;
                }
                state.select_first_match();
                return ModelDropdownAction::Consumed;
            }
            KeyCode::Char('u' | 'w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if key.code == KeyCode::Char('w') {
                    let kept = state.filter_query.trim_end().len();
                    state.filter_query.truncate(kept);
                    let word_start = state
                        .filter_query
                        .rfind(char::is_whitespace)
                        .map_or(0, |i| i + 1);
                    state.filter_query.truncate(word_start);
                } else {
                    state.filter_query.clear();
                }
                state.select_first_match();
                return ModelDropdownAction::Consumed;
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                state.filter_query.push(c);
                state.select_first_match();
                return ModelDropdownAction::Consumed;
            }
            _ => {}
        }
    } else if key.code == KeyCode::Char('/') {
        state.filter_editing = true;
        return ModelDropdownAction::Consumed;
    }

    let indices = state.filtered_indices();
    let mut visible_index = indices
        .iter()
        .position(|i| *i == state.selected_index)
        .unwrap_or(0);
    let nav_key = match key.code {
        KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => Some(KeyCode::Down),
        KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => Some(KeyCode::Up),
        KeyCode::Home => Some(KeyCode::Char('g')),
        KeyCode::End => Some(KeyCode::Char('G')),
        KeyCode::Up | KeyCode::Down => Some(key.code),
        KeyCode::Char('j' | 'k' | 'g' | 'G') if !state.filter_editing => Some(key.code),
        _ => None,
    };
    if let Some(code) = nav_key {
        list::handle_list_nav_key(
            &mut visible_index,
            indices.len(),
            &KeyEvent::new(code, KeyModifiers::NONE),
        );
        state.selected_index = indices.get(visible_index).copied().unwrap_or(0);
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
            if indices.contains(&state.selected_index) {
                let (model_id, _) = &state.models[state.selected_index];
                ModelDropdownAction::Selected(model_id.clone())
            } else {
                ModelDropdownAction::Consumed
            }
        }
        KeyCode::Char(c @ '1'..='9') if !state.filter_editing => {
            let idx = (c as usize) - ('1' as usize);
            if let Some(source_index) = indices.get(idx) {
                let (model_id, _) = &state.models[*source_index];
                ModelDropdownAction::Selected(model_id.clone())
            } else {
                ModelDropdownAction::Consumed
            }
        }
        KeyCode::Esc | KeyCode::Char('q') => ModelDropdownAction::Dismissed,
        _ if state.filter_editing => ModelDropdownAction::Consumed,
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
    // #1407: cycle only the providers the provider profile offers.
    let builtins: Vec<SessionProvider> = builtins
        .iter()
        .copied()
        .filter(|provider| crate::provider_profile_view::provider_offered(*provider))
        .collect();
    let builtins = builtins.as_slice();
    let custom_providers = if crate::provider_profile_view::custom_providers_offered() {
        custom_providers
    } else {
        &[]
    };
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
    state.reconcile_filter_selection();
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

    fn type_query(state: &mut ModelDropdownState, query: &str) {
        assert_eq!(
            handle_model_dropdown_key(state, &make_key(KeyCode::Char('/')), &[]),
            ModelDropdownAction::Consumed
        );
        for c in query.chars() {
            assert_eq!(
                handle_model_dropdown_key(state, &make_key(KeyCode::Char(c)), &[]),
                ModelDropdownAction::Consumed
            );
        }
    }

    #[test]
    fn model_dropdown_search_matches_name_and_id_with_all_terms() {
        let mut state = ModelDropdownState::new(SessionProvider::Claude, sample_models(), None);
        type_query(&mut state, "OPUS 20250514");
        assert_eq!(state.filter_query, "OPUS 20250514");
        assert_eq!(state.filtered_indices(), vec![1]);
        assert_eq!(
            handle_model_dropdown_key(&mut state, &make_key(KeyCode::Enter), &[]),
            ModelDropdownAction::Selected("claude-opus-4-20250514".into())
        );
    }

    #[test]
    fn model_dropdown_search_captures_shortcut_letters_and_numbers() {
        let mut state = ModelDropdownState::new(SessionProvider::Claude, sample_models(), None);
        type_query(&mut state, "jkqgG19/ ");
        assert_eq!(state.filter_query, "jkqgG19/ ");
        assert!(state.filter_editing);
        assert!(state.filtered_indices().is_empty());
        assert_eq!(
            handle_model_dropdown_key(&mut state, &make_key(KeyCode::Enter), &[]),
            ModelDropdownAction::Consumed
        );
    }

    #[test]
    fn model_dropdown_filtered_navigation_and_numbers_use_visible_rows() {
        let mut state = ModelDropdownState::new(SessionProvider::Claude, sample_models(), None);
        type_query(&mut state, "claude 3");
        assert_eq!(state.filtered_indices(), vec![2]);
        handle_model_dropdown_key(&mut state, &make_key(KeyCode::Esc), &[]);
        assert_eq!(
            handle_model_dropdown_key(&mut state, &make_key(KeyCode::Char('1')), &[]),
            ModelDropdownAction::Selected("claude-haiku-3.5".into())
        );

        state.filter_query = "claude".into();
        state.filter_editing = true;
        for code in [KeyCode::Home, KeyCode::Down, KeyCode::Down, KeyCode::Down] {
            handle_model_dropdown_key(&mut state, &make_key(code), &[]);
        }
        assert_eq!(state.selected_index, 2);
        handle_model_dropdown_key(
            &mut state,
            &KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            &[],
        );
        assert_eq!(state.selected_index, 1);
        handle_model_dropdown_key(
            &mut state,
            &KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL),
            &[],
        );
        assert_eq!(state.selected_index, 2);
        handle_model_dropdown_key(&mut state, &make_key(KeyCode::Esc), &[]);
        handle_model_dropdown_key(&mut state, &make_key(KeyCode::Char('g')), &[]);
        assert_eq!(state.selected_index, 0);
        handle_model_dropdown_key(&mut state, &make_key(KeyCode::Char('G')), &[]);
        assert_eq!(state.selected_index, 2);
    }

    #[test]
    fn model_dropdown_empty_results_block_selection_and_recover() {
        let mut state = ModelDropdownState::new(SessionProvider::Claude, sample_models(), None);
        type_query(&mut state, "missing");
        for code in [KeyCode::Down, KeyCode::End, KeyCode::Enter] {
            assert_eq!(
                handle_model_dropdown_key(&mut state, &make_key(code), &[]),
                ModelDropdownAction::Consumed
            );
        }
        handle_model_dropdown_key(&mut state, &make_key(KeyCode::Esc), &[]);
        assert_eq!(
            handle_model_dropdown_key(&mut state, &make_key(KeyCode::Char('1')), &[]),
            ModelDropdownAction::Consumed
        );
        assert_eq!(
            handle_model_dropdown_key(&mut state, &make_key(KeyCode::Enter), &[]),
            ModelDropdownAction::Consumed
        );
        // Resume editing and clear the query: the full catalog returns.
        handle_model_dropdown_key(&mut state, &make_key(KeyCode::Char('/')), &[]);
        handle_model_dropdown_key(
            &mut state,
            &KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
            &[],
        );
        assert_eq!(state.filtered_indices(), vec![0, 1, 2]);
        assert_eq!(
            handle_model_dropdown_key(&mut state, &make_key(KeyCode::Enter), &[]),
            ModelDropdownAction::Selected("claude-sonnet-5".into())
        );
    }

    /// Esc steps out of the query first (keeping the filtered list), then a
    /// second Esc closes; `q` types while editing and closes outside it.
    #[test]
    fn model_dropdown_esc_leaves_search_then_closes() {
        let mut state = ModelDropdownState::new(SessionProvider::Claude, sample_models(), None);
        type_query(&mut state, "haiku");
        assert_eq!(
            handle_model_dropdown_key(&mut state, &make_key(KeyCode::Esc), &[]),
            ModelDropdownAction::Consumed
        );
        assert!(!state.filter_editing);
        assert_eq!(state.filter_query, "haiku");
        assert_eq!(state.filtered_indices(), vec![2]);
        assert_eq!(
            handle_model_dropdown_key(&mut state, &make_key(KeyCode::Esc), &[]),
            ModelDropdownAction::Dismissed
        );

        let mut state = ModelDropdownState::new(SessionProvider::Claude, sample_models(), None);
        type_query(&mut state, "q");
        assert_eq!(state.filter_query, "q");
        handle_model_dropdown_key(&mut state, &make_key(KeyCode::Esc), &[]);
        assert_eq!(
            handle_model_dropdown_key(&mut state, &make_key(KeyCode::Char('q')), &[]),
            ModelDropdownAction::Dismissed
        );
    }

    #[test]
    fn model_dropdown_typing_highlights_the_first_match() {
        // The current model (Haiku, index 2) starts highlighted.
        let mut state = ModelDropdownState::new(
            SessionProvider::Claude,
            sample_models(),
            Some("claude-haiku-3.5"),
        );
        assert_eq!(state.selected_index, 2);
        type_query(&mut state, "claude");
        assert_eq!(state.selected_index, 0, "a query edit restarts at the top");
        assert_eq!(
            handle_model_dropdown_key(&mut state, &make_key(KeyCode::Enter), &[]),
            ModelDropdownAction::Selected("claude-sonnet-5".into())
        );
    }

    #[test]
    fn model_dropdown_search_ignores_punctuation_and_spacing() {
        let mut models = sample_models();
        models.push(("claude-opus-4-5".into(), "Opus 4.5".into()));
        models.push(("gpt-5.5-codex".into(), "GPT 5.5 Codex".into()));
        let mut state = ModelDropdownState::new(SessionProvider::Claude, models, None);
        state.filter_query = "opus45".into();
        assert_eq!(state.filtered_indices(), vec![3]);
        state.filter_query = "GPT55".into();
        assert_eq!(state.filtered_indices(), vec![4]);
        state.filter_query = "haiku3.5".into();
        assert_eq!(state.filtered_indices(), vec![2]);
        state.filter_query = "-".into();
        assert_eq!(state.filtered_indices(), vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn model_dropdown_backspace_on_empty_query_and_ctrl_w() {
        let mut state = ModelDropdownState::new(SessionProvider::Claude, sample_models(), None);
        type_query(&mut state, "claude opus ");
        handle_model_dropdown_key(
            &mut state,
            &KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL),
            &[],
        );
        assert_eq!(state.filter_query, "claude ");
        handle_model_dropdown_key(
            &mut state,
            &KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL),
            &[],
        );
        assert_eq!(state.filter_query, "");
        assert!(state.filter_editing);
        handle_model_dropdown_key(&mut state, &make_key(KeyCode::Backspace), &[]);
        assert!(
            !state.filter_editing,
            "Backspace on an empty query ends search"
        );
        assert!(state.open);
        handle_model_dropdown_key(&mut state, &make_key(KeyCode::Char('j')), &[]);
        assert_eq!(state.selected_index, 1);
    }

    #[test]
    fn model_dropdown_navigation_skips_hidden_models() {
        let mut models = sample_models();
        models.push(("vendor/future".into(), "Friendly Sonnet".into()));
        let mut state = ModelDropdownState::new(SessionProvider::Claude, models, None);
        type_query(&mut state, "sonnet");
        assert_eq!(state.filtered_indices(), vec![0, 3]);
        handle_model_dropdown_key(&mut state, &make_key(KeyCode::Down), &[]);
        assert_eq!(state.selected_index, 3);
        handle_model_dropdown_key(&mut state, &make_key(KeyCode::Up), &[]);
        assert_eq!(state.selected_index, 0);
        handle_model_dropdown_key(&mut state, &make_key(KeyCode::Esc), &[]);
        handle_model_dropdown_key(&mut state, &make_key(KeyCode::Char('j')), &[]);
        assert_eq!(state.selected_index, 3);
        assert_eq!(
            handle_model_dropdown_key(&mut state, &make_key(KeyCode::Char('2')), &[]),
            ModelDropdownAction::Selected("vendor/future".into())
        );
        state.filter_query = "FRIENDLY vendor".into();
        assert_eq!(state.filtered_indices(), vec![3]);
    }

    #[test]
    fn model_dropdown_backspace_handles_unicode_and_control_u_clears() {
        let mut state = ModelDropdownState::new(SessionProvider::Claude, sample_models(), None);
        type_query(&mut state, "opusé");
        handle_model_dropdown_key(&mut state, &make_key(KeyCode::Backspace), &[]);
        assert_eq!(state.filter_query, "opus");
        assert_eq!(state.filtered_indices(), vec![1]);
        handle_model_dropdown_key(
            &mut state,
            &KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
            &[],
        );
        assert_eq!(state.filter_query, "");
        assert_eq!(state.filtered_indices(), vec![0, 1, 2]);
        assert!(state.filter_editing);
        state.close();
        state.toggle();
        assert!(state.open);
        assert!(!state.filter_editing);
        assert_eq!(state.filter_query, "");
    }

    #[test]
    fn model_dropdown_query_survives_provider_cycle_and_discovery() {
        let mut state = ModelDropdownState::new(SessionProvider::Claude, sample_models(), None);
        type_query(&mut state, "gpt");
        assert_eq!(
            handle_model_dropdown_key(&mut state, &make_key(KeyCode::Tab), &[]),
            ModelDropdownAction::ProviderCycled
        );
        assert_eq!(state.provider, SessionProvider::Codex);
        assert_eq!(state.filter_query, "gpt");
        assert!(state.filter_editing);
        state.replace_models(vec![
            ("other".into(), "Other".into()),
            ("gpt-z".into(), "GPT Z".into()),
        ]);
        assert_eq!(state.selected_index, 1);
        state.replace_models(vec![
            ("gpt-z".into(), "GPT Z".into()),
            ("gpt-new".into(), "GPT New".into()),
        ]);
        assert_eq!(state.selected_index, 0);
        assert_eq!(
            handle_model_dropdown_key(&mut state, &make_key(KeyCode::Enter), &[]),
            ModelDropdownAction::Selected("gpt-z".into())
        );
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
