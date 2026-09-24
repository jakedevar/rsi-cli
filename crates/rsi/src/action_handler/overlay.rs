//! Overlay and project management action handlers.
//!
//! Covers: all overlay open/close/toggle, model selection, theme selection,
//! project CRUD (via command palette), notification management.

use crate::app::App;
use crate::modalkit_types::LcAction;
use crate::types::OverlayState;

pub(super) async fn dispatch(app: &mut App, action: LcAction) {
    match action {
        LcAction::ToggleNotifications => {
            if matches!(app.overlay, OverlayState::NotificationBrowser { .. }) {
                app.overlay = OverlayState::None;
            } else {
                app.overlay = OverlayState::NotificationBrowser {
                    selected_index: 0,
                    scroll_offset: 0,
                };
            }
        }

        LcAction::ToggleFileExplorer => {
            if matches!(app.overlay, OverlayState::FileExplorer { .. }) {
                app.overlay = OverlayState::None;
            } else {
                crate::overlay::file_explorer::open_file_explorer(app);
            }
        }

        LcAction::ToggleGitPanel => {
            // Resolve working directory and request lazygit spawn in the event loop.
            if let Some(wd) = app.focused_session_working_dir() {
                app.pending_external = Some(crate::app::ExternalRequest::Lazygit(wd));
            } else {
                app.notify("No session focused");
            }
        }

        LcAction::DismissAllNotifications => {
            let dismissed: Vec<_> = app.notifications.drain(..).collect();
            for n in dismissed {
                app.notification_history.push(n);
            }
        }

        LcAction::OpenSortPicker => {
            if matches!(&app.overlay, OverlayState::SortPicker { .. }) {
                app.overlay = OverlayState::None;
            } else {
                crate::overlay::open_sort_picker(app);
            }
        }

        LcAction::GoToArchiveZone => {
            // Switch to archive zone in the focused session list pane
            let focused = app.interaction_pane_id();
            // If we're in a detail pane, go back to list first
            if matches!(
                app.tabs[app.active_tab].find_pane(focused),
                Some(crate::types::Pane::SessionDetail { .. })
            ) {
                app.back_to_list();
            }
            // Now switch to archive zone
            let focused = app.interaction_pane_id();
            if let Some(crate::types::Pane::SessionList {
                active_zone,
                selected_session,
                archive_selected_index,
                ..
            }) = app.tabs[app.active_tab].find_pane_mut(focused)
            {
                *active_zone = crate::types::SessionListZone::Archive;
                *selected_session = app
                    .filtered_archived_order
                    .get(*archive_selected_index)
                    .copied();
            }
            // Refresh archived sessions from daemon
            app.refresh_archived_sessions().await;
        }

        LcAction::GoToSessionsZone => {
            // Switch to sessions (main) zone from anywhere
            let focused = app.interaction_pane_id();
            if matches!(
                app.tabs[app.active_tab].find_pane(focused),
                Some(crate::types::Pane::SessionDetail { .. })
            ) {
                app.back_to_list();
            }
            let focused = app.interaction_pane_id();
            if let Some(crate::types::Pane::SessionList {
                active_zone,
                selected_session,
                selected_index,
                ..
            }) = app.tabs[app.active_tab].find_pane_mut(focused)
            {
                *active_zone = crate::types::SessionListZone::Main;
                *selected_session = app.filtered_session_order.get(*selected_index).copied();
            }
        }

        LcAction::GoToTaskRabbitZone => {
            // Switch to TaskRabbit zone from anywhere
            let focused = app.interaction_pane_id();
            if matches!(
                app.tabs[app.active_tab].find_pane(focused),
                Some(crate::types::Pane::SessionDetail { .. })
            ) {
                app.back_to_list();
            }
            let focused = app.interaction_pane_id();
            if let Some(crate::types::Pane::SessionList {
                active_zone,
                selected_session,
                taskrabbit_selected_index,
                ..
            }) = app.tabs[app.active_tab].find_pane_mut(focused)
            {
                *active_zone = crate::types::SessionListZone::TaskRabbit;
                *selected_session = app
                    .filtered_taskrabbit_order
                    .get(*taskrabbit_selected_index)
                    .copied();
            }
        }

        LcAction::GoToJobsZone => {
            // Switch to Jobs zone from anywhere
            let focused = app.interaction_pane_id();
            if matches!(
                app.tabs[app.active_tab].find_pane(focused),
                Some(crate::types::Pane::SessionDetail { .. })
            ) {
                app.back_to_list();
            }
            let focused = app.interaction_pane_id();
            if let Some(crate::types::Pane::SessionList {
                active_zone,
                selected_session,
                jobs_selected_index,
                ..
            }) = app.tabs[app.active_tab].find_pane_mut(focused)
            {
                *active_zone = crate::types::SessionListZone::Jobs;
                *selected_session = app.filtered_jobs_order.get(*jobs_selected_index).copied();
            }
        }

        LcAction::OpenMemorySearch => {
            if matches!(&app.overlay, OverlayState::MemorySearch { .. }) {
                app.overlay = OverlayState::None;
            } else {
                crate::overlay::open_memory_search(app);
            }
        }

        LcAction::OpenDiagnostics => {
            if matches!(&app.overlay, OverlayState::Diagnostics) {
                app.overlay = OverlayState::None;
            } else {
                app.overlay = OverlayState::Diagnostics;
            }
        }

        LcAction::OpenRecursiveDagBrowser => {
            if matches!(&app.overlay, OverlayState::RecursiveDagBrowser(..)) {
                app.overlay = OverlayState::None;
                app.recursive_dag_rx = None;
            } else {
                crate::overlay::open_recursive_dag_browser(app);
            }
        }

        LcAction::OpenRecentCompletions => {
            if matches!(&app.overlay, OverlayState::RecentCompletions { .. }) {
                app.overlay = OverlayState::None;
            } else {
                app.overlay = OverlayState::RecentCompletions { selected_index: 0 };
            }
        }

        LcAction::ToggleModelDropdown => {
            if app.model_dropdown.open {
                app.model_dropdown.close();
            } else {
                app.model_dropdown = crate::types::ModelDropdownState::new(
                    app.selected_provider,
                    app.available_models.clone(),
                    app.selected_model.as_deref(),
                );
                app.model_dropdown.custom_provider_index = app.custom_provider_index;
                app.needs_model_refresh = true;
            }
        }

        LcAction::SelectModel(model) => {
            if let Some(ref model_id) = model {
                // If already on Harness provider (direct API) or Antigravity,
                // keep it — Harness and Antigravity models overlap with other CLI provider
                // prefixes (claude-*, gpt-*, etc.)
                if app.selected_provider == rsi_common::types::SessionProvider::Harness
                    || app.selected_provider == rsi_common::types::SessionProvider::Antigravity
                    || app.selected_provider == rsi_common::types::SessionProvider::Pioneer
                    || app.selected_provider == rsi_common::types::SessionProvider::OpenRouter
                {
                    // Provider stays the same; models are already loaded from discovery
                } else if model_id.starts_with("claude-") {
                    app.selected_provider = rsi_common::types::SessionProvider::Claude;
                    app.available_models = crate::app::CLAUDE_MODELS
                        .iter()
                        .map(|(id, name)| (id.to_string(), name.to_string()))
                        .collect();
                } else if model_id.starts_with("gemini-") || model_id.starts_with("gpt-oss-") {
                    app.selected_provider = rsi_common::types::SessionProvider::Antigravity;
                    app.available_models = crate::app::ANTIGRAVITY_MODELS
                        .iter()
                        .map(|(id, name)| (id.to_string(), name.to_string()))
                        .collect();
                } else if model_id.starts_with("qwen")
                    || model_id.starts_with("phi")
                    || model_id.contains("ollama")
                    || model_id.contains("llama")
                    || model_id.contains("gemma")
                    || model_id.contains("mistral")
                {
                    app.selected_provider = rsi_common::types::SessionProvider::Local;
                    app.available_models = crate::app::LOCAL_MODELS
                        .iter()
                        .map(|(id, name)| (id.to_string(), name.to_string()))
                        .collect();
                } else {
                    app.selected_provider = rsi_common::types::SessionProvider::Codex;
                    app.available_models = crate::app::CODEX_MODELS
                        .iter()
                        .map(|(id, name)| (id.to_string(), name.to_string()))
                        .collect();
                }
            }
            app.selected_model = model;
            if let Some(model) = app.selected_model.as_deref() {
                rsi_common::model_utils::reconcile_effort(model, &mut app.selected_effort);
            }
        }

        LcAction::OpenThemePicker => {
            if matches!(&app.overlay, OverlayState::ThemePicker { .. }) {
                app.overlay = OverlayState::None;
            } else {
                crate::overlay::open_theme_picker(app);
            }
        }

        LcAction::OpenColorCustomizer => {
            if matches!(&app.overlay, OverlayState::ColorCustomizer { .. }) {
                app.overlay = OverlayState::None;
            } else {
                crate::overlay::open_color_customizer(app);
            }
        }

        LcAction::SelectTheme(name) => {
            if !crate::ui::theme::set_theme_by_name(&name) {
                app.notify_error(format!("Unknown theme: {}", name));
            }
        }

        LcAction::OpenProjectPicker => {
            if matches!(&app.overlay, OverlayState::ProjectPicker { .. }) {
                app.overlay = OverlayState::None;
            } else {
                crate::overlay::open_project_picker(
                    app,
                    crate::types::ProjectPickerContext::GlobalFilter,
                );
            }
        }

        LcAction::OpenLabelPicker => {
            // Stub: will be fully implemented in Phase 6 (LabelPicker overlay).
            // For now, just log that the action was triggered.
            app.push_notification(
                crate::types::NotificationKind::Info,
                crate::types::NotificationPriority::Low,
                "Label picker not yet implemented (Phase 6)".to_string(),
                None,
            );
        }

        LcAction::SwitchProject(name) => {
            let name_lower = name.to_lowercase();
            let matched = app
                .projects
                .iter()
                .find(|p| p.name.to_lowercase().contains(&name_lower))
                .map(|p| (p.id, p.name.clone()));
            match matched {
                Some((id, pname)) => {
                    // Update all tabs to show this project
                    app.set_all_tabs_project(Some(id));
                    app.notify(format!("Workspace: {}", pname));
                }
                None => {
                    app.notify(format!("No project matching '{}'", name));
                }
            }
        }

        LcAction::CreateProject { name, path } => {
            let color = crate::overlay::PROJECT_COLORS[0].1;
            let path_buf = path.map(|p| {
                let trimmed = p.trim();
                if trimmed.starts_with("~/") || trimmed == "~" {
                    dirs::home_dir()
                        .map(|h| h.join(trimmed.strip_prefix("~/").unwrap_or("")))
                        .unwrap_or_else(|| std::path::PathBuf::from(trimmed))
                } else {
                    std::path::PathBuf::from(trimmed)
                }
            });
            match app
                .client
                .create_project(&name, path_buf.as_deref(), None, Some(color))
                .await
            {
                Ok(_) => {
                    app.notify_success(format!("Created project: {}", name));
                }
                Err(e) => {
                    app.notify_error(format!("Create project failed: {}", e));
                }
            }
        }

        LcAction::EditProjectByName(name) => {
            let name_lower = name.to_lowercase();
            let matched = app
                .projects
                .iter()
                .find(|p| p.name.to_lowercase().contains(&name_lower))
                .cloned();
            match matched {
                Some(project) => {
                    let color_index = crate::overlay::PROJECT_COLORS
                        .iter()
                        .position(|(_, hex)| *hex == project.color)
                        .unwrap_or(0);
                    let path_str = project
                        .path
                        .as_ref()
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_default();
                    app.overlay = crate::types::OverlayState::ProjectForm {
                        focused_field: 0,
                        name: project.name,
                        path: path_str,
                        color_index,
                        editing_id: Some(project.id),
                    };
                }
                None => {
                    app.notify(format!("No project matching '{}'", name));
                }
            }
        }

        LcAction::DeleteProjectByName(name) => {
            let name_lower = name.to_lowercase();
            let matched = app
                .projects
                .iter()
                .find(|p| p.name.to_lowercase().contains(&name_lower))
                .map(|p| (p.id, p.name.clone()));
            match matched {
                Some((id, pname)) => {
                    if let Err(e) = app.client.delete_project(id).await {
                        app.notify_error(format!("Delete project failed: {}", e));
                    } else {
                        app.projects.retain(|p| p.id != id);
                        if app.current_project_id == Some(id) {
                            app.set_project_filter(None);
                        }
                        app.notify_success(format!("Deleted project: {}", pname));
                    }
                }
                None => {
                    app.notify(format!("No project matching '{}'", name));
                }
            }
        }

        LcAction::ToggleKeybindingsHelp => match &app.overlay {
            crate::types::OverlayState::KeybindingsHelp { .. } => {
                crate::overlay::close_keybindings_help(app);
            }
            _ => {
                crate::overlay::open_keybindings_help(app);
            }
        },

        LcAction::OpenAllCommands => crate::overlay::keybindings_help::open_all_commands(app),
        LcAction::OpenManual(mode) => app.pending_manual = Some(mode),

        LcAction::TogglePromptPreview => match &app.overlay {
            crate::types::OverlayState::PromptPreview { .. } => {
                app.overlay = crate::types::OverlayState::None;
            }
            _ => {
                crate::overlay::open_prompt_preview(app);
            }
        },

        LcAction::OpenEspSquare => {
            if matches!(app.overlay, OverlayState::EspSquare { .. }) {
                app.overlay = OverlayState::None;
            } else {
                app.overlay = OverlayState::EspSquare {
                    round: 0,
                    correct: 0,
                    interactive: true,
                    rounds: Vec::with_capacity(12),
                    message: String::new(),
                    flash: None,
                    flash_deadline: None,
                    target: rand::Rng::gen_range(&mut rand::thread_rng(), 0..9usize),
                    round_details: Vec::with_capacity(12),
                    cursor: None,
                    last_guess: None,
                    started_at: std::time::Instant::now(),
                };
            }
        }

        LcAction::OpenTelescope => {
            if matches!(app.overlay, OverlayState::Telescope { .. }) {
                app.overlay = OverlayState::None;
            } else {
                crate::overlay::telescope::open_telescope(app);
            }
        }

        LcAction::OpenCommandPalette => {
            crate::overlay::command_palette::open_command_palette(app);
        }

        LcAction::ResolveTopologyAttempt(args) => {
            crate::overlay::graph::resolve_topology_attempt_command(app, &args).await;
        }

        LcAction::OpenGraphReview => {
            if matches!(app.overlay, OverlayState::GraphReview { .. }) {
                app.overlay = OverlayState::None;
            } else {
                crate::overlay::graph::open_graph_review(app).await;
            }
        }

        LcAction::GoToTrash => {
            crate::overlay::open_trash_browser(app).await;
        }

        LcAction::OpenProjectCard => {
            let project = app.current_project().cloned();
            match project {
                Some(p) => {
                    let entity_id = p.id.to_string();
                    let display_name = p.name.clone();
                    app.overlay = OverlayState::CardEditor {
                        entity_type: "project".to_string(),
                        entity_id: entity_id.clone(),
                        display_name,
                        facts: Vec::new(),
                        selected_index: 0,
                        scroll_offset: 0,
                        editing: None,
                        loading: true,
                        pending_delete: false,
                    };
                    match app.client.get_entity_card("project", &entity_id).await {
                        Ok(Some(card)) => {
                            if let OverlayState::CardEditor { facts, loading, .. } =
                                &mut app.overlay
                            {
                                *facts = card.facts;
                                *loading = false;
                            }
                        }
                        Ok(None) => {
                            if let OverlayState::CardEditor { loading, .. } = &mut app.overlay {
                                *loading = false;
                            }
                        }
                        Err(e) => {
                            app.overlay = OverlayState::None;
                            app.notify_error(format!("Failed to load card: {}", e));
                        }
                    }
                }
                None => {
                    app.notify("No active project — select a project first");
                }
            }
        }

        LcAction::OpenUserCard => {
            app.overlay = OverlayState::CardEditor {
                entity_type: "user".to_string(),
                entity_id: "self".to_string(),
                display_name: "User".to_string(),
                facts: Vec::new(),
                selected_index: 0,
                scroll_offset: 0,
                editing: None,
                loading: true,
                pending_delete: false,
            };
            match app.client.get_entity_card("user", "self").await {
                Ok(Some(card)) => {
                    if let OverlayState::CardEditor { facts, loading, .. } = &mut app.overlay {
                        *facts = card.facts;
                        *loading = false;
                    }
                }
                Ok(None) => {
                    if let OverlayState::CardEditor { loading, .. } = &mut app.overlay {
                        *loading = false;
                    }
                }
                Err(e) => {
                    app.overlay = OverlayState::None;
                    app.notify_error(format!("Failed to load user card: {}", e));
                }
            }
        }

        LcAction::AddProjectCardFact(fact) => {
            let project = app.current_project().cloned();
            if let Some(project) = project {
                let entity_id = project.id.to_string();
                match app.client.get_entity_card("project", &entity_id).await {
                    Ok(existing) => {
                        let mut facts = existing.map(|c| c.facts).unwrap_or_default();
                        if facts.len() >= 40 {
                            app.notify_error("Project card is full (40 facts max)");
                            return;
                        }
                        facts.push(fact);
                        match app
                            .client
                            .set_entity_card("project", &entity_id, facts)
                            .await
                        {
                            Ok(_) => app.notify_success("Added fact to project card"),
                            Err(e) => app.notify_error(format!("Failed: {}", e)),
                        }
                    }
                    Err(e) => app.notify_error(format!("Failed: {}", e)),
                }
            } else {
                app.notify("No active project");
            }
        }

        LcAction::AddUserCardFact(fact) => match app.client.get_entity_card("user", "self").await {
            Ok(existing) => {
                let mut facts = existing.map(|c| c.facts).unwrap_or_default();
                if facts.len() >= 40 {
                    app.notify_error("User card is full (40 facts max)");
                    return;
                }
                facts.push(fact);
                match app.client.set_entity_card("user", "self", facts).await {
                    Ok(_) => app.notify_success("Added fact to user card"),
                    Err(e) => app.notify_error(format!("Failed: {}", e)),
                }
            }
            Err(e) => app.notify_error(format!("Failed: {}", e)),
        },

        LcAction::OpenIssuesWorkspace => {
            app.open_or_focus_issues();
        }

        LcAction::OpenDialectic => {
            if matches!(&app.overlay, OverlayState::Dialectic { .. }) {
                app.overlay = OverlayState::None;
            } else {
                crate::overlay::dialectic::open_dialectic(app, None);
            }
        }

        LcAction::AskQuery(query) => {
            crate::overlay::dialectic::open_dialectic(app, Some(query));
        }

        LcAction::OpenScheduleBrowser => {
            if matches!(&app.overlay, OverlayState::ScheduleBrowser { .. }) {
                app.overlay = OverlayState::None;
            } else {
                crate::overlay::schedule_browser::open_schedule_browser(app).await;
            }
        }

        LcAction::ToggleTerminal => {
            match &app.overlay {
                OverlayState::Terminal { .. } => {
                    // Close terminal overlay (shell keeps running in background)
                    app.overlay = OverlayState::None;
                }
                OverlayState::None => {
                    // Lazy-spawn terminal on first open
                    if app.terminal.is_none() {
                        match crate::terminal::EmbeddedTerminal::spawn(24, 80) {
                            Ok((term, rx)) => {
                                app.terminal = Some(term);
                                app.terminal_rx = Some(rx);
                            }
                            Err(e) => {
                                app.notify_error(format!("Terminal spawn failed: {}", e));
                                return;
                            }
                        }
                    }
                    app.overlay = OverlayState::Terminal;
                }
                _ => {
                    // Another overlay is active — close it first, then open terminal
                    app.overlay = OverlayState::None;
                    if app.terminal.is_none() {
                        match crate::terminal::EmbeddedTerminal::spawn(24, 80) {
                            Ok((term, rx)) => {
                                app.terminal = Some(term);
                                app.terminal_rx = Some(rx);
                            }
                            Err(e) => {
                                app.notify_error(format!("Terminal spawn failed: {}", e));
                                return;
                            }
                        }
                    }
                    app.overlay = OverlayState::Terminal;
                }
            }
        }

        LcAction::OpenRatingOverlay => {
            // Pre-seeds the picker with the session's existing rating (if any).
            crate::overlay::open_rating_overlay(app);
        }

        LcAction::OpenSessionInfoPanel => {
            crate::overlay::open_session_info_panel(app);
        }

        // === Phase 4: hierarchy reassignment ===
        LcAction::MoveToParent => match app.selected_session_id() {
            Some(sid) => {
                crate::overlay::open_parent_picker(app, sid);
            }
            None => {
                app.notify("No session selected");
            }
        },

        LcAction::MoveToRoot => match app.selected_session_id() {
            Some(sid) => {
                if let Err(e) = app.client.set_session_parent(sid, None).await {
                    tracing::warn!(?sid, error = %e, "set_session_parent(root) failed");
                    app.notify_error(format!("Move to root failed: {}", e));
                } else {
                    let old_parent = app.sessions.get(&sid).map(|s| s.session.parent_id);
                    if let Some(state) = app.sessions.get_mut(&sid) {
                        state.session.parent_id = None;
                    }
                    app.sort_sessions(true);
                    if let Some(parent_id) = old_parent {
                        app.invalidate_hierarchy_node(parent_id);
                    }
                    app.invalidate_hierarchy_node(None);
                    app.notify_success("Moved to root");
                }
            }
            None => {
                app.notify("No session selected");
            }
        },

        LcAction::RateSession(rating) => {
            if !(1..=10).contains(&rating) {
                app.notify_error(format!("Invalid rating {} (must be 1–10)", rating));
                return;
            }
            if let Some(session_id) = app.selected_session_id() {
                if let Err(e) = app
                    .client
                    .update_session_rating(session_id, Some(rating as i16))
                    .await
                {
                    app.notify_error(format!("Rating failed: {}", e));
                } else {
                    if let Some(state) = app.sessions.get_mut(&session_id) {
                        state.session.rating = Some(rating as i16);
                    }
                    app.notify(format!("Rated {}/10", rating));
                }
            } else {
                app.notify("No session focused");
            }
        }

        _ => unreachable!("overlay::dispatch called with non-overlay action"),
    }
}
