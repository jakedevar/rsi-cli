//! Prompt overlay — open, close, submit, draft persistence.
//!
//! Key handling is now delegated to the shared [`InputSurface`](crate::input_surface)
//! via `overlay/mod.rs`. This module provides open/close/submit functions and
//! maintains draft persistence for TaskRabbit and Blank prompts.

use crate::app::{App, InteractiveLaunchOrigin, LaunchPlacement};
use crate::input_surface::InputSurface;
use crate::types::{OverlayState, PromptPurpose};
use crate::ui::theme;
use ratatui::style::Style;
use ratatui::widgets::Block;
use rsi_common::types::{SandboxKind, SandboxSpec};

/// Build a [`SandboxSpec`] for git-worktree isolation when `enabled` is true.
/// Returns `None` when sandbox is disabled (preserves pre-sandbox RPC payload shape).
fn sandbox_spec_from_bool(enabled: bool) -> Option<SandboxSpec> {
    if enabled {
        Some(SandboxSpec {
            kind: Some(SandboxKind::GitWorktree),
            branch: None,
        })
    } else {
        None
    }
}

/// Submit the prompt content and close the overlay.
/// Used for ContinueSession prompts that live in `app.overlay`.
pub(super) async fn submit_prompt(app: &mut App) -> bool {
    submit_prompt_with_placement(app, LaunchPlacement::CurrentPane).await
}

async fn submit_prompt_with_placement(app: &mut App, placement: LaunchPlacement) -> bool {
    // Extract content, purpose, working_dir, and model overrides before closing
    // `typed` is the raw surface content, kept so a rejected ContinueSession can
    // restore the prompt verbatim — `content_for_send` collapses visual wraps.
    let (
        mut query,
        purpose,
        working_dir,
        model_override,
        provider_override,
        custom_provider_index,
        sandbox_enabled,
        typed,
        corrected_preview,
        overlay_id,
    ) = match &app.overlay {
        OverlayState::Prompt {
            overlay_id,
            surface,
            purpose,
            working_dir,
            model_override,
            provider_override,
            model_dropdown,
            sandbox_enabled,
            ..
        } => (
            surface.content_for_send(),
            purpose.clone(),
            working_dir.clone(),
            model_override.clone(),
            *provider_override,
            model_dropdown.custom_provider_index,
            *sandbox_enabled,
            surface.textarea.lines().to_vec(),
            surface.corrected_preview.clone(),
            *overlay_id,
        ),
        _ => return false,
    };

    if query.is_empty() {
        // Close the overlay for empty submissions
        app.restore_previous_overlay();
        return false;
    }

    // Append mandatory instructions for TaskRabbit submissions without exposing them in the textarea.
    if matches!(purpose, PromptPurpose::TaskRabbit) {
        query.push_str(
            "\n\nMANDATORY: After completing all requested work, you must commit and push the changes to the repository before finishing.",
        );
    }

    // ContinueSession keeps its existing response-bearing path. Launch
    // purposes remain open until the App-owned task returns a semantic
    // LaunchSession response for this exact overlay identity.
    match purpose.clone() {
        PromptPurpose::ContinueSession(session_id) => {
            // Anchor detail view to bottom so user sees the new prompt immediately
            if let Some(state) = app.sessions.get_mut(&session_id) {
                state.follow_tail = true;
            }
            // The prompt overlay (and its surface) was closed above and this
            // purpose has no draft field, so a rejected continue would
            // otherwise discard the text outright. Put it back in the session's
            // input bar.
            app.restore_previous_overlay();
            let accepted = app.continue_session(session_id, &query).await;
            if !accepted {
                app.restore_continue_lines(session_id, typed);
            }
            return accepted;
        }
        PromptPurpose::TaskRabbit => {
            let sandbox = sandbox_spec_from_bool(sandbox_enabled);
            app.request_taskrabbit_launch(
                &query,
                Some(&working_dir),
                model_override.as_deref(),
                provider_override,
                custom_provider_index,
                sandbox,
                InteractiveLaunchOrigin::LegacyPrompt {
                    overlay_id,
                    purpose,
                    draft_lines: typed,
                    corrected_preview,
                },
                placement,
            )
        }
        PromptPurpose::Blank => {
            let sandbox = sandbox_spec_from_bool(sandbox_enabled);
            app.request_blank_launch(
                &query,
                Some(&working_dir),
                model_override.as_deref(),
                provider_override,
                custom_provider_index,
                sandbox,
                InteractiveLaunchOrigin::LegacyPrompt {
                    overlay_id,
                    purpose,
                    draft_lines: typed,
                    corrected_preview,
                },
                placement,
            )
        }
        PromptPurpose::CreateTyped { kind, parent_id } => app.request_typed_launch(
            &query,
            &working_dir,
            model_override.as_deref(),
            provider_override,
            custom_provider_index,
            sandbox_spec_from_bool(sandbox_enabled),
            kind,
            parent_id,
            InteractiveLaunchOrigin::LegacyPrompt {
                overlay_id,
                purpose,
                draft_lines: typed,
                corrected_preview,
            },
            placement,
        ),
    }
}

/// Submit the prompt content, close the overlay, and flag the new session
/// to be opened in a new tab when it arrives from the daemon.
/// Only affects new-session prompts — ContinueSession is submitted normally.
pub(super) async fn submit_prompt_in_new_tab(app: &mut App) {
    let is_launch = matches!(
        &app.overlay,
        OverlayState::Prompt {
            purpose: PromptPurpose::TaskRabbit
                | PromptPurpose::Blank
                | PromptPurpose::CreateTyped { .. },
            ..
        }
    );
    if is_launch {
        let _ = submit_prompt_with_placement(app, LaunchPlacement::NewTab).await;
    } else {
        let _ = submit_prompt(app).await;
    }
}

/// Submit the prompt content, close the overlay, and flag the new session
/// to be opened in a new split pane when it arrives from the daemon.
pub(super) async fn submit_prompt_in_new_split(app: &mut App) {
    let is_launch = matches!(
        &app.overlay,
        OverlayState::Prompt {
            purpose: PromptPurpose::TaskRabbit
                | PromptPurpose::Blank
                | PromptPurpose::CreateTyped { .. },
            ..
        }
    );
    if is_launch {
        let _ = submit_prompt_with_placement(app, LaunchPlacement::NewSplit).await;
    } else {
        let _ = submit_prompt(app).await;
    }
}

/// Close the overlay without submitting.
/// Used for ContinueSession prompts that live in `app.overlay`.
pub(super) fn close_overlay(app: &mut App) {
    if let OverlayState::Prompt { overlay_id, .. } = &app.overlay
        && app.interactive_launch_pending_for_overlay(*overlay_id)
    {
        app.notify("Session launch is awaiting daemon acceptance; draft preserved");
        return;
    }
    // Save draft based on purpose
    match &app.overlay {
        OverlayState::Prompt {
            surface,
            purpose: PromptPurpose::TaskRabbit,
            ..
        } => {
            let lines: Vec<String> = surface
                .textarea
                .lines()
                .iter()
                .map(|s| s.to_string())
                .collect();
            let has_content = lines.iter().any(|l| !l.is_empty());
            app.taskrabbit_draft = if has_content { lines } else { Vec::new() };
        }
        OverlayState::Prompt {
            surface,
            purpose: PromptPurpose::Blank,
            ..
        } => {
            let lines: Vec<String> = surface
                .textarea
                .lines()
                .iter()
                .map(|s| s.to_string())
                .collect();
            let has_content = lines.iter().any(|l| !l.is_empty());
            app.blank_draft = if has_content { lines } else { Vec::new() };
        }
        _ => {}
    }
    app.restore_previous_overlay();
}

// === Input overlay stack functions ===

/// Submit the focused input overlay and remove it from the stack.
pub(super) async fn submit_input_overlay(app: &mut App) -> bool {
    submit_input_overlay_with_placement(app, LaunchPlacement::CurrentPane).await
}

async fn submit_input_overlay_with_placement(app: &mut App, placement: LaunchPlacement) -> bool {
    let idx = app.focused_input_idx;
    let (
        mut query,
        purpose,
        working_dir,
        model_override,
        provider_override,
        custom_provider_index,
        sandbox_enabled,
        draft_lines,
        corrected_preview,
        overlay_id,
    ) = match app.input_overlays.get(idx) {
        Some(OverlayState::Prompt {
            overlay_id,
            surface,
            purpose,
            working_dir,
            model_override,
            provider_override,
            model_dropdown,
            sandbox_enabled,
            ..
        }) => (
            surface.content_for_send(),
            purpose.clone(),
            working_dir.clone(),
            model_override.clone(),
            *provider_override,
            model_dropdown.custom_provider_index,
            *sandbox_enabled,
            surface.textarea.lines().to_vec(),
            surface.corrected_preview.clone(),
            *overlay_id,
        ),
        _ => return false,
    };

    if query.is_empty() {
        close_input_overlay(app);
        return false;
    }

    if matches!(purpose, PromptPurpose::TaskRabbit) {
        query.push_str(
            "\n\nMANDATORY: After completing all requested work, you must commit and push the changes to the repository before finishing.",
        );
    }

    match purpose.clone() {
        PromptPurpose::TaskRabbit => {
            let sandbox = sandbox_spec_from_bool(sandbox_enabled);
            app.request_taskrabbit_launch(
                &query,
                Some(&working_dir),
                model_override.as_deref(),
                provider_override,
                custom_provider_index,
                sandbox,
                InteractiveLaunchOrigin::StackedPrompt {
                    overlay_id,
                    purpose,
                    draft_lines,
                    corrected_preview,
                },
                placement,
            )
        }
        PromptPurpose::Blank => {
            let sandbox = sandbox_spec_from_bool(sandbox_enabled);
            app.request_blank_launch(
                &query,
                Some(&working_dir),
                model_override.as_deref(),
                provider_override,
                custom_provider_index,
                sandbox,
                InteractiveLaunchOrigin::StackedPrompt {
                    overlay_id,
                    purpose,
                    draft_lines,
                    corrected_preview,
                },
                placement,
            )
        }
        PromptPurpose::CreateTyped { kind, parent_id } => app.request_typed_launch(
            &query,
            &working_dir,
            model_override.as_deref(),
            provider_override,
            custom_provider_index,
            sandbox_spec_from_bool(sandbox_enabled),
            kind,
            parent_id,
            InteractiveLaunchOrigin::StackedPrompt {
                overlay_id,
                purpose,
                draft_lines,
                corrected_preview,
            },
            placement,
        ),
        _ => false,
    }
}

/// Submit the focused input overlay in a new tab.
pub(super) async fn submit_input_overlay_in_new_tab(app: &mut App) {
    let is_launch = matches!(
        app.focused_input_overlay(),
        Some(OverlayState::Prompt {
            purpose: PromptPurpose::TaskRabbit
                | PromptPurpose::Blank
                | PromptPurpose::CreateTyped { .. },
            ..
        })
    );
    if is_launch {
        let _ = submit_input_overlay_with_placement(app, LaunchPlacement::NewTab).await;
    } else {
        let _ = submit_input_overlay(app).await;
    }
}

/// Submit the focused input overlay in a new split.
pub(super) async fn submit_input_overlay_in_new_split(app: &mut App) {
    let is_launch = matches!(
        app.focused_input_overlay(),
        Some(OverlayState::Prompt {
            purpose: PromptPurpose::TaskRabbit
                | PromptPurpose::Blank
                | PromptPurpose::CreateTyped { .. },
            ..
        })
    );
    if is_launch {
        let _ = submit_input_overlay_with_placement(app, LaunchPlacement::NewSplit).await;
    } else {
        let _ = submit_input_overlay(app).await;
    }
}

/// Close the focused input overlay without submitting.
pub(super) fn close_input_overlay(app: &mut App) {
    let idx = app.focused_input_idx;
    if let Some(OverlayState::Prompt { overlay_id, .. }) = app.input_overlays.get(idx)
        && app.interactive_launch_pending_for_overlay(*overlay_id)
    {
        app.notify("Session launch is awaiting daemon acceptance; draft preserved");
        return;
    }
    // Save draft based on purpose
    match app.input_overlays.get(idx) {
        Some(OverlayState::Prompt {
            surface,
            purpose: PromptPurpose::TaskRabbit,
            ..
        }) => {
            let lines: Vec<String> = surface
                .textarea
                .lines()
                .iter()
                .map(|s| s.to_string())
                .collect();
            let has_content = lines.iter().any(|l| !l.is_empty());
            app.taskrabbit_draft = if has_content { lines } else { Vec::new() };
        }
        Some(OverlayState::Prompt {
            surface,
            purpose: PromptPurpose::Blank,
            ..
        }) => {
            let lines: Vec<String> = surface
                .textarea
                .lines()
                .iter()
                .map(|s| s.to_string())
                .collect();
            let has_content = lines.iter().any(|l| !l.is_empty());
            app.blank_draft = if has_content { lines } else { Vec::new() };
        }
        _ => {}
    }

    app.remove_focused_input_overlay();
}

/// Create an InputSurface for overlay use (insert mode, overlay-styled).
fn make_overlay_surface(lines: Option<Vec<String>>) -> InputSurface {
    let mut surface = match lines {
        Some(lines) if !lines.is_empty() => InputSurface::new_insert_with_content(lines),
        _ => InputSurface::new_insert(),
    };
    // Apply overlay styling
    surface
        .textarea
        .set_style(Style::default().fg(theme::text()).bg(theme::overlay_bg()));
    surface.textarea.move_cursor(tui_textarea::CursorMove::Top);
    surface.textarea.move_cursor(tui_textarea::CursorMove::Head);
    surface
}

/// Open the prompt popup overlay for continuing an existing session.
///
/// The popup uses the session's original working directory and is pre-configured
/// to call `continue_session()` on submit.  Lives in `app.overlay` (not the input stack).
pub fn open_continue_popup(app: &mut App, session_id: uuid::Uuid) {
    // Get working dir from the session state
    let working_dir = app
        .sessions
        .get(&session_id)
        .map(|s| s.session.working_dir.clone())
        .unwrap_or_else(|| {
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"))
        });

    let mut surface = make_overlay_surface(None);
    surface.textarea.set_block(Block::default());

    let model_dropdown = crate::types::ModelDropdownState::closed(
        app.selected_provider,
        app.available_models.clone(),
        app.selected_model.as_deref(),
    );
    app.overlay = OverlayState::Prompt {
        overlay_id: uuid::Uuid::new_v4(),
        surface,
        working_dir,
        purpose: PromptPurpose::ContinueSession(session_id),
        available_commands: app.available_commands.clone(),
        model_override: None,
        provider_override: None,
        model_dropdown,
        sandbox_enabled: false,
    };
}

/// Open the TaskRabbit prompt popup for one-shot tasks.
/// Pushes to the input overlay stack for simultaneous display.
pub fn open_taskrabbit_popup(app: &mut App) {
    let working_dir = app
        .current_project()
        .and_then(|p| p.path.clone())
        .or_else(|| {
            app.selected_session_state()
                .map(|s| s.session.working_dir.clone())
        })
        .unwrap_or_else(|| {
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"))
        });

    // Only restore draft when opening the first input overlay
    let draft = if app.input_overlays.is_empty() && !app.taskrabbit_draft.is_empty() {
        Some(app.taskrabbit_draft.clone())
    } else {
        None
    };
    let mut surface = make_overlay_surface(draft);
    surface.textarea.set_block(Block::default());

    let overlay = OverlayState::Prompt {
        overlay_id: uuid::Uuid::new_v4(),
        surface,
        working_dir,
        purpose: PromptPurpose::TaskRabbit,
        available_commands: app.available_commands.clone(),
        model_override: None,
        provider_override: None,
        model_dropdown: crate::types::ModelDropdownState::closed(
            app.selected_provider,
            app.available_models.clone(),
            app.selected_model.as_deref(),
        ),
        sandbox_enabled: false,
    };

    app.input_overlays.push(overlay);
    app.focused_input_idx = app.input_overlays.len() - 1;
}

/// Phase 4: open a typed-leaf prompt popup (Story / Task / Bug …) with an
/// optional hierarchical parent. Pushes to the input overlay stack like Blank.
///
/// Currently unreferenced (P2.2 retired the only call site) — retained for
/// P2.3's potential `p`-overlay reconciliation work. Removing this leaves
/// `PromptPurpose::CreateTyped` constructible only via submit-dispatch sites
/// which never construct it directly, so the variant would also need removal.
/// Keep as-is until P2.3 resolves the typed-prompt reuse question.
#[allow(dead_code)]
pub fn open_typed_prompt(
    app: &mut App,
    kind: rsi_common::types::SessionKind,
    parent_id: Option<uuid::Uuid>,
) {
    let working_dir = app
        .current_project()
        .and_then(|p| p.path.clone())
        .or_else(|| {
            app.selected_session_state()
                .map(|s| s.session.working_dir.clone())
        })
        .unwrap_or_else(|| {
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"))
        });

    let mut surface = make_overlay_surface(None);
    surface.textarea.set_block(Block::default());

    let overlay = OverlayState::Prompt {
        overlay_id: uuid::Uuid::new_v4(),
        surface,
        working_dir,
        purpose: PromptPurpose::CreateTyped { kind, parent_id },
        available_commands: app.available_commands.clone(),
        model_override: None,
        provider_override: None,
        model_dropdown: crate::types::ModelDropdownState::closed(
            app.selected_provider,
            app.available_models.clone(),
            app.selected_model.as_deref(),
        ),
        sandbox_enabled: false,
    };

    app.input_overlays.push(overlay);
    app.focused_input_idx = app.input_overlays.len() - 1;
    app.mark_dirty();
}

/// Open the Blank prompt popup — empty textarea, no pre-filled commands, general purpose.
/// Pushes to the input overlay stack for simultaneous display.
pub fn open_blank_popup(app: &mut App) {
    let working_dir = app
        .current_project()
        .and_then(|p| p.path.clone())
        .or_else(|| {
            app.selected_session_state()
                .map(|s| s.session.working_dir.clone())
        })
        .unwrap_or_else(|| {
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"))
        });

    // Only restore draft when opening the first input overlay
    let draft = if app.input_overlays.is_empty() && !app.blank_draft.is_empty() {
        Some(app.blank_draft.clone())
    } else {
        None
    };
    let mut surface = make_overlay_surface(draft);
    surface.textarea.set_block(Block::default());

    let overlay = OverlayState::Prompt {
        overlay_id: uuid::Uuid::new_v4(),
        surface,
        working_dir,
        purpose: PromptPurpose::Blank,
        available_commands: app.available_commands.clone(),
        model_override: None,
        provider_override: None,
        model_dropdown: crate::types::ModelDropdownState::closed(
            app.selected_provider,
            app.available_models.clone(),
            app.selected_model.as_deref(),
        ),
        // New blank sessions should be isolated unless the user explicitly
        // turns sandboxing off for this modal.
        sandbox_enabled: true,
    };

    app.input_overlays.push(overlay);
    app.focused_input_idx = app.input_overlays.len() - 1;
}
