//! Navigation action handlers.
//!
//! Covers: session list navigation (enter/back, attention jumping),
//! event-level navigation (next/prev event), fold management,
//! event visibility toggles, jumplist, search.

use crate::app::App;
use crate::modalkit_types::LcAction;
use crate::types::{Pane, SearchTarget};
use rsi_common::types::{EventType, Role};

/// Rebuild list projections immediately after an explicit card/fold command.
/// Selection is preserved by UUID when it remains visible; collapsing an
/// ancestor falls back to the nearest row at the prior visual position.
fn refresh_session_list_after_fold(app: &mut App) {
    app.recalculate_filtered_order();
    app.reconcile_all_session_list_selections(false);
    app.invalidate_card_cache();
}

pub(super) fn dispatch(app: &mut App, action: LcAction) {
    match action {
        LcAction::EnterSession => {
            // Phase 4: route Enter on container cards into descent_path navigation.
            if let Some(sid) = app.selected_session_id() {
                let kind = app.sessions.get(&sid).map(|s| s.session.session_kind);
                if matches!(
                    kind,
                    Some(rsi_common::types::SessionKind::Group)
                        | Some(rsi_common::types::SessionKind::Epic)
                ) {
                    enter_container(app, sid);
                    return;
                }
            }
            app.enter_session();
        }

        LcAction::BackToList => {
            // In unified navigation mode, Left/Right always target the session list.
            // BackToList is a no-op from SessionDetail (Backspace available for repurposing).
        }

        LcAction::AscendContainer => {
            ascend_container(app);
        }

        LcAction::AscendOrBack => {
            if matches!(app.focused_pane(), Some(Pane::SessionDetail { .. })) {
                app.back_to_list();
            } else {
                let has_path = app
                    .tabs
                    .get(app.active_tab)
                    .map(|t| !t.descent_path.is_empty())
                    .unwrap_or(false);
                if has_path {
                    ascend_container(app);
                }
            }
        }

        LcAction::NextAttention => {
            navigate_attention(app, true);
        }

        LcAction::PrevAttention => {
            navigate_attention(app, false);
        }

        LcAction::ListSessions => {
            app.back_to_list();
        }

        LcAction::NextEvent => {
            if let Some(Pane::SessionDetail { session_id }) = app.focused_pane().cloned()
                && let Some(state) = app.sessions.get_mut(&session_id)
                && !state.events.is_empty()
            {
                let start = state.current_event_index.map(|i| i + 1).unwrap_or(0);
                if let Some(idx) = (start..state.events.len())
                    .find(|&i| state.event_heights.get(i).copied().unwrap_or(0) > 0)
                {
                    if let Some(&off) = state.event_offsets.get(idx) {
                        state.scroll_offset = off;
                    }
                    state.current_event_index = Some(idx);
                    // Disengage scroll-lock so the render loop doesn't
                    // snap back to the bottom and overwrite our cursor.
                    // Re-engage happens automatically when the user scrolls
                    // back to the bottom.
                    state.follow_tail = false;
                    state.follow_tail_hold = true;
                }
            }
        }

        LcAction::PrevEvent => {
            if let Some(Pane::SessionDetail { session_id }) = app.focused_pane().cloned()
                && let Some(state) = app.sessions.get_mut(&session_id)
                && !state.events.is_empty()
            {
                let end = state.current_event_index.unwrap_or(state.events.len());
                if let Some(idx) = (0..end)
                    .rev()
                    .find(|&i| state.event_heights.get(i).copied().unwrap_or(0) > 0)
                {
                    if let Some(&off) = state.event_offsets.get(idx) {
                        state.scroll_offset = off;
                    }
                    state.current_event_index = Some(idx);
                } else {
                    state.scroll_offset = 0;
                    state.current_event_index = Some(0);
                }
                state.follow_tail = false;
                state.follow_tail_hold = true;
            }
        }

        LcAction::NextUserMessage => {
            if let Some(Pane::SessionDetail { session_id }) = app.focused_pane().cloned()
                && let Some(state) = app.sessions.get_mut(&session_id)
                && !state.events.is_empty()
            {
                let start = state.current_event_index.map(|i| i + 1).unwrap_or(0);
                let is_visible_user_message = |i: usize| {
                    let event = &state.events[i];
                    event.event_type == EventType::Message
                        && event.role == Some(Role::User)
                        && state.event_heights.get(i).copied().unwrap_or(0) > 0
                };
                let idx = (start..state.events.len())
                    .find(|&i| is_visible_user_message(i))
                    .or_else(|| (0..start).find(|&i| is_visible_user_message(i)));

                if let Some(idx) = idx {
                    if let Some(&off) = state.event_offsets.get(idx) {
                        state.scroll_offset = off;
                    }
                    state.current_event_index = Some(idx);
                    state.follow_tail = false;
                    state.follow_tail_hold = true;
                }
            }
        }

        LcAction::PrevUserMessage => {
            if let Some(Pane::SessionDetail { session_id }) = app.focused_pane().cloned()
                && let Some(state) = app.sessions.get_mut(&session_id)
                && !state.events.is_empty()
            {
                // The live tail cursor denotes the position after the final
                // event for this first backward jump, so include a newest
                // user message. Once a jump disengages tail-following, keep
                // the ordinary strictly-older relative scan.
                let end = if state.follow_tail {
                    state.events.len()
                } else {
                    state.current_event_index.unwrap_or(state.events.len())
                };
                let is_visible_user_message = |i: usize| {
                    let event = &state.events[i];
                    event.event_type == EventType::Message
                        && event.role == Some(Role::User)
                        && state.event_heights.get(i).copied().unwrap_or(0) > 0
                };
                let idx = (0..end)
                    .rev()
                    .find(|&i| is_visible_user_message(i))
                    .or_else(|| {
                        (end..state.events.len())
                            .rev()
                            .find(|&i| is_visible_user_message(i))
                    });

                if let Some(idx) = idx {
                    if let Some(&off) = state.event_offsets.get(idx) {
                        state.scroll_offset = off;
                    }
                    state.current_event_index = Some(idx);
                    state.follow_tail = false;
                    state.follow_tail_hold = true;
                }
            }
        }

        LcAction::OpenFold => {
            if let Some(Pane::SessionList {
                selected_session: Some(sid),
                ..
            }) = app.focused_pane().cloned()
            {
                if let Some(state) = app.sessions.get_mut(&sid) {
                    state.list_card_expanded = true;
                }
                refresh_session_list_after_fold(app);
            } else if let Some(Pane::SessionDetail { session_id }) = app.focused_pane().cloned()
                && let Some(state) = app.sessions.get_mut(&session_id)
                && !state.events.is_empty()
                && let Some(idx) = state.current_event_index
            {
                let event = &state.events[idx];
                let seq = event.sequence;
                let is_tool = matches!(
                    event.event_type,
                    rsi_common::types::EventType::ToolUse
                        | rsi_common::types::EventType::ToolResult
                );
                let is_effectively_collapsed =
                    is_tool && crate::ui::height::is_event_effectively_collapsed(state, event);

                let group = crate::ui::height::tool_group_members(state, idx);
                let group_start_seq = crate::ui::height::tool_group_leader_seq(state, idx);

                if is_effectively_collapsed
                    && let (Some(members), Some(group_start_seq)) = (group, group_start_seq)
                {
                    if !state.expanded_tool_groups.contains(&group_start_seq) {
                        // First `zo` on a collapsed group: bulk-expand every event
                        // in it to full content in one keypress (D2 — redesigned
                        // single-press expand/collapse, fixes F-010's 2-step
                        // awkwardness). "Every event in it" is now the id-paired
                        // group's members (calls plus their results, which need not
                        // be contiguous), not a positional run. The
                        // `expanded_tool_groups` insert is kept alongside it, now
                        // defensive/vestigial bookkeeping.
                        for i in members {
                            let run_seq = state.events[i].sequence;
                            state.collapsed_events.remove(&run_seq);
                            state.expanded_events.insert(run_seq);
                        }
                        state.expanded_tool_groups.insert(group_start_seq);
                        crate::ui::height::invalidate_heights(state);
                    } else {
                        // Group already expanded (defensive — every run member is
                        // already in `expanded_events` once the branch above has
                        // run once for this group).
                        state.collapsed_events.remove(&seq);
                        state.expanded_events.insert(seq);
                        crate::ui::height::invalidate_heights(state);
                    }
                } else {
                    // Non-tool or already uncollapsed — standard expand behavior
                    state.collapsed_events.remove(&seq);
                    state.expanded_events.insert(seq);
                    crate::ui::height::invalidate_heights(state);
                }
            }
        }

        LcAction::CloseFold => {
            if let Some(Pane::SessionList {
                selected_session: Some(sid),
                ..
            }) = app.focused_pane().cloned()
            {
                if let Some(state) = app.sessions.get_mut(&sid) {
                    state.list_card_expanded = false;
                }
                refresh_session_list_after_fold(app);
            } else if let Some(Pane::SessionDetail { session_id }) = app.focused_pane().cloned()
                && let Some(state) = app.sessions.get_mut(&session_id)
                && !state.events.is_empty()
                && let Some(idx) = state.current_event_index
            {
                let event = &state.events[idx];
                let seq = event.sequence;
                let is_tool = matches!(
                    event.event_type,
                    rsi_common::types::EventType::ToolUse
                        | rsi_common::types::EventType::ToolResult
                );

                if is_tool {
                    // If the individual item is expanded, collapse it first
                    if state.expanded_events.contains(&seq) {
                        state.expanded_events.remove(&seq);
                        state.collapsed_events.insert(seq);
                    } else if let Some(group_start_seq) =
                        crate::ui::height::tool_group_leader_seq(state, idx)
                    {
                        // Already individually collapsed — re-group the whole group
                        state.expanded_tool_groups.remove(&group_start_seq);
                    }
                } else {
                    state.expanded_events.remove(&seq);
                }
                crate::ui::height::invalidate_heights(state);
            }
        }

        LcAction::ToggleFold => {
            if let Some(Pane::SessionList {
                selected_session: Some(sid),
                ..
            }) = app.focused_pane().cloned()
            {
                if let Some(state) = app.sessions.get_mut(&sid) {
                    state.list_card_expanded = !state.list_card_expanded;
                }
                refresh_session_list_after_fold(app);
            } else if let Some(Pane::SessionDetail { session_id }) = app.focused_pane().cloned()
                && let Some(state) = app.sessions.get_mut(&session_id)
                && !state.events.is_empty()
                && let Some(idx) = state.current_event_index
            {
                let seq = state.events[idx].sequence;
                let event = &state.events[idx];
                let is_tool = matches!(
                    event.event_type,
                    rsi_common::types::EventType::ToolUse
                        | rsi_common::types::EventType::ToolResult
                );
                let effectively_collapsed =
                    crate::ui::height::is_event_effectively_collapsed(state, event);

                let group = crate::ui::height::tool_group_members(state, idx);
                let group_start_seq = crate::ui::height::tool_group_leader_seq(state, idx);

                if effectively_collapsed
                    && is_tool
                    && let (Some(members), Some(group_start_seq)) = (group, group_start_seq)
                {
                    if !state.expanded_tool_groups.contains(&group_start_seq) {
                        // Group is collapsed — `za` bulk-expands every member of the
                        // id-paired group in one keypress, mirroring `zo`'s D2
                        // redesign (fixes F-010). The `expanded_tool_groups` insert
                        // is kept alongside it, now defensive/vestigial bookkeeping.
                        for i in members {
                            let run_seq = state.events[i].sequence;
                            state.collapsed_events.remove(&run_seq);
                            state.expanded_events.insert(run_seq);
                        }
                        state.expanded_tool_groups.insert(group_start_seq);
                    } else {
                        // Group already expanded (defensive) — toggle this
                        // individual item.
                        state.collapsed_events.remove(&seq);
                        state.expanded_events.insert(seq);
                    }
                } else if effectively_collapsed {
                    // Non-tool collapsed — standard expand
                    state.collapsed_events.remove(&seq);
                    state.expanded_events.insert(seq);
                } else {
                    // Currently expanded — collapse
                    state.expanded_events.remove(&seq);
                    if is_tool {
                        state.collapsed_events.insert(seq);
                    }
                }
                crate::ui::height::invalidate_heights(state);
            }
        }

        LcAction::CloseAllFolds => {
            if let Some(Pane::SessionList { .. }) = app.focused_pane().cloned() {
                for state in app.sessions.values_mut() {
                    state.list_card_expanded = false;
                }
                refresh_session_list_after_fold(app);
            } else if let Some(Pane::SessionDetail { session_id }) = app.focused_pane().cloned()
                && let Some(state) = app.sessions.get_mut(&session_id)
            {
                state.expanded_events.clear();
                state.expanded_tool_groups.clear();
                for event in &state.events {
                    if matches!(
                        event.event_type,
                        rsi_common::types::EventType::ToolUse
                            | rsi_common::types::EventType::ToolResult
                    ) {
                        state.collapsed_events.insert(event.sequence);
                    }
                }
                crate::ui::height::invalidate_heights(state);
            }
        }

        LcAction::OpenAllFolds => {
            if let Some(Pane::SessionList { .. }) = app.focused_pane().cloned() {
                for state in app.sessions.values_mut() {
                    state.list_card_expanded = true;
                }
                refresh_session_list_after_fold(app);
            } else if let Some(Pane::SessionDetail { session_id }) = app.focused_pane().cloned()
                && let Some(state) = app.sessions.get_mut(&session_id)
            {
                state.collapsed_events.clear();
                state.expanded_tool_groups.clear();
                for event in &state.events {
                    state.expanded_events.insert(event.sequence);
                }
                crate::ui::height::invalidate_heights(state);
            }
        }

        LcAction::ToggleSystemEvents => {
            if let Some(Pane::SessionDetail { session_id }) = app.focused_pane().cloned()
                && let Some(state) = app.sessions.get_mut(&session_id)
            {
                state.show_system_events = !state.show_system_events;
                crate::ui::height::invalidate_heights(state);
                let label = if state.show_system_events {
                    "shown"
                } else {
                    "hidden"
                };
                app.notify(format!("System events {}", label));
            }
        }

        LcAction::ToggleThinkingEvents => {
            if let Some(Pane::SessionDetail { session_id }) = app.focused_pane().cloned()
                && let Some(state) = app.sessions.get_mut(&session_id)
            {
                state.show_thinking_events = !state.show_thinking_events;
                crate::ui::height::invalidate_heights(state);
                let label = if state.show_thinking_events {
                    "expanded"
                } else {
                    "collapsed"
                };
                app.notify(format!("Thinking events {}", label));
            }
        }

        LcAction::JumpBack => {
            app.jump_back();
        }

        LcAction::JumpForward => {
            app.jump_forward();
        }

        LcAction::EnterSearch => {
            let target = match app.focused_pane().cloned() {
                Some(Pane::SessionList { .. }) => SearchTarget::SessionList,
                Some(Pane::SessionDetail { .. }) => SearchTarget::SessionDetail,
                _ => return,
            };
            app.search_query.clear();
            app.search_matches.clear();
            app.search_match_cursor = 0;
            app.search_target = target;
            app.input_mode = crate::types::InputMode::Search;
        }

        LcAction::NextSearchMatch => {
            app.next_search_match();
        }

        LcAction::PrevSearchMatch => {
            app.prev_search_match();
        }

        LcAction::DetailScrollDown => {
            if let Some(Pane::SessionDetail { session_id }) = app.focused_pane().cloned()
                && let Some(state) = app.sessions.get_mut(&session_id)
            {
                state.scroll_offset = state.scroll_offset.saturating_add(1);
                state.follow_tail = false;
            }
        }

        LcAction::DetailScrollUp => {
            if let Some(Pane::SessionDetail { session_id }) = app.focused_pane().cloned()
                && let Some(state) = app.sessions.get_mut(&session_id)
            {
                state.scroll_offset = state.scroll_offset.saturating_sub(1);
                state.follow_tail = false;
            }
        }

        LcAction::SessionListZoneNext => {
            // Read indices first to avoid overlapping borrows
            let zone_info = if let Pane::SessionList {
                active_zone,
                selected_index,
                taskrabbit_selected_index,
                archive_selected_index,
                jobs_selected_index,
                ..
            } = app.session_list_pane_mut()
            {
                let new_zone = match *active_zone {
                    crate::types::SessionListZone::Archive => crate::types::SessionListZone::Main,
                    crate::types::SessionListZone::Main => {
                        crate::types::SessionListZone::TaskRabbit
                    }
                    crate::types::SessionListZone::TaskRabbit => {
                        crate::types::SessionListZone::Jobs
                    }
                    crate::types::SessionListZone::Jobs => crate::types::SessionListZone::Archive,
                };
                Some((
                    new_zone,
                    *selected_index,
                    *taskrabbit_selected_index,
                    *archive_selected_index,
                    *jobs_selected_index,
                ))
            } else {
                None
            };
            if let Some((new_zone, sel, tr_sel, ar_sel, jobs_sel)) = zone_info {
                let new_session = match new_zone {
                    crate::types::SessionListZone::Main => {
                        app.filtered_session_order.get(sel).copied()
                    }
                    crate::types::SessionListZone::TaskRabbit => {
                        app.filtered_taskrabbit_order.get(tr_sel).copied()
                    }
                    crate::types::SessionListZone::Archive => {
                        app.filtered_archived_order.get(ar_sel).copied()
                    }
                    crate::types::SessionListZone::Jobs => {
                        app.filtered_jobs_order.get(jobs_sel).copied()
                    }
                };
                if let Pane::SessionList {
                    active_zone,
                    selected_session,
                    ..
                } = app.session_list_pane_mut()
                {
                    *active_zone = new_zone;
                    *selected_session = new_session;
                }
            }
        }

        LcAction::SessionListZonePrev => {
            // Read indices first to avoid overlapping borrows
            let zone_info = if let Pane::SessionList {
                active_zone,
                selected_index,
                taskrabbit_selected_index,
                archive_selected_index,
                jobs_selected_index,
                ..
            } = app.session_list_pane_mut()
            {
                let new_zone = match *active_zone {
                    crate::types::SessionListZone::Archive => crate::types::SessionListZone::Jobs,
                    crate::types::SessionListZone::Jobs => {
                        crate::types::SessionListZone::TaskRabbit
                    }
                    crate::types::SessionListZone::Main => crate::types::SessionListZone::Archive,
                    crate::types::SessionListZone::TaskRabbit => {
                        crate::types::SessionListZone::Main
                    }
                };
                Some((
                    new_zone,
                    *selected_index,
                    *taskrabbit_selected_index,
                    *archive_selected_index,
                    *jobs_selected_index,
                ))
            } else {
                None
            };
            if let Some((new_zone, sel, tr_sel, ar_sel, jobs_sel)) = zone_info {
                let new_session = match new_zone {
                    crate::types::SessionListZone::Main => {
                        app.filtered_session_order.get(sel).copied()
                    }
                    crate::types::SessionListZone::TaskRabbit => {
                        app.filtered_taskrabbit_order.get(tr_sel).copied()
                    }
                    crate::types::SessionListZone::Archive => {
                        app.filtered_archived_order.get(ar_sel).copied()
                    }
                    crate::types::SessionListZone::Jobs => {
                        app.filtered_jobs_order.get(jobs_sel).copied()
                    }
                };
                if let Pane::SessionList {
                    active_zone,
                    selected_session,
                    ..
                } = app.session_list_pane_mut()
                {
                    *active_zone = new_zone;
                    *selected_session = new_session;
                }
            }
        }

        LcAction::NextLabelBoundary => {
            navigate_label_boundary(app, true);
        }

        LcAction::PrevLabelBoundary => {
            navigate_label_boundary(app, false);
        }

        // --- Attention jumps ---
        LcAction::JumpAttentionN(n) => jump_attention_n(app, n),
        LcAction::OpenRecentFileN(n) => open_recent_file_n(app, n),

        _ => unreachable!("navigation::dispatch called with non-navigation action"),
    }
}

/// Jump to attention slot N (1..9). Always available via `<Space>1..9`.
/// Bare digits stay normal-mode count prefixes; numbered attention jumps live
/// on `<Space>1..9` only.
///
/// Consumes the SHARED predicate `crate::app::attention::attention_session_ids`
/// so this and `]a / [a` always land on the same session.
fn jump_attention_n(app: &mut App, n: u8) {
    let attention = crate::app::attention::attention_session_ids(app);
    let idx = (n as usize).saturating_sub(1);
    if let Some(target_id) = attention.get(idx).copied() {
        app.open_session_in_current_pane(target_id);
    }
}

/// Open recent-file slot N (1..9) in the file viewer. Bound to `gf1..gf9`
/// (composes with the `g`-prefix grammar). Reuses the per-session
/// `file_viewer_cache` so revisiting a path preserves cursor / fold state.
fn open_recent_file_n(app: &mut App, n: u8) {
    let Some(session_id) = app.selected_session_id() else {
        return;
    };
    let idx = (n as usize).saturating_sub(1);
    let path_opt = app
        .sessions
        .get_mut(&session_id)
        .map(|s| s.cached_recent_files())
        .and_then(|files| files.get(idx).cloned());
    let Some(path) = path_opt else {
        return;
    };
    crate::file_viewer::open_path_in_session(app, session_id, path);
}

/// Navigate to the next/previous session with a different group_id.
fn navigate_label_boundary(app: &mut App, forward: bool) {
    let current_id = match app.selected_session_id() {
        Some(id) => id,
        None => return,
    };
    let current_group = app.sessions.get(&current_id).map(|s| s.session.group_id);
    let current_idx = app.session_index_of(&current_id).unwrap_or(0);

    let target_idx = if forward {
        (current_idx + 1..app.filtered_session_order.len()).find(|&i| {
            app.filtered_session_order
                .get(i)
                .and_then(|id| app.sessions.get(id))
                .map(|s| s.session.group_id)
                != current_group
        })
    } else {
        (0..current_idx).rev().find(|&i| {
            app.filtered_session_order
                .get(i)
                .and_then(|id| app.sessions.get(id))
                .map(|s| s.session.group_id)
                != current_group
        })
    };

    if let Some(idx) = target_idx
        && let Some(&target_id) = app.filtered_session_order.get(idx)
    {
        let focused = app.interaction_pane_id();
        if let Some(Pane::SessionList {
            selected_index,
            selected_session,
            ..
        }) = app.tabs[app.active_tab].find_pane_mut(focused)
        {
            *selected_index = idx;
            *selected_session = Some(target_id);
        }
    }
}

/// Phase 4: push a container onto the active tab's `descent_path`. Mutual
/// exclusion with the in-place expand from Phase 3 — when entering a
/// container we collapse its list-card so the user sees children only.
///
/// Also runs the navigation-node effect after the descent path changes, so
/// child synchronization is tied to the container view switch.
fn enter_container(app: &mut App, session_id: uuid::Uuid) {
    // Capture lead_id before mutably borrowing app below.
    let lead_id = app
        .sessions
        .get(&session_id)
        .and_then(|s| s.session.lead_session_id);

    if let Some(state) = app.sessions.get_mut(&session_id) {
        state.list_card_expanded = false;
    }
    if let Some(tab) = app.tabs.get_mut(app.active_tab)
        && tab.descent_path.last().copied() != Some(session_id)
    {
        tab.descent_path.push(session_id);
    }
    // Recompute filtered order now that descent_path has changed.
    app.recalculate_filtered_order();
    // The visible list has switched from the parent level to the container's
    // children; keep the cursor row but update its UUID to the new occupant.
    app.reconcile_all_session_list_selections(true);
    // Default-focus the lead session if one is assigned.
    if let Some(lid) = lead_id {
        if let Some(idx) = app.filtered_session_order.iter().position(|id| *id == lid) {
            let focused = app.interaction_pane_id();
            if let Some(crate::types::Pane::SessionList {
                selected_index,
                selected_session,
                ..
            }) = app.tabs[app.active_tab].find_pane_mut(focused)
            {
                *selected_index = idx;
                *selected_session = Some(lid);
            }
        }
    }
    app.invalidate_card_cache();

    // View-switch effect (useEffect-style dependency on the active
    // navigation node). Replaces the prior reliance on the 500ms / 5s
    // periodic poll cycle to load the container's children -- the
    // fetch dispatches at the moment of descent, eliminating the
    // navigate->render delay. Dedup is enforced by
    // `hierarchy_fetch_inflight` / `focus_fetch_inflight`, so the
    // next periodic poll cycle won't duplicate the same RPC.
    app.run_navigation_effect_if_changed();

    // IMMEDIATE hierarchy fetch for instant navigation response.
    // This ensures hierarchy data is fetched immediately on descent
    // instead of waiting for the next effect poll cycle.
    app.trigger_hierarchy_fetch_immediate(Some(session_id));
}

/// Phase 4: pop one ancestor off the active tab's `descent_path`. No-op
/// when already at root.
///
fn ascend_container(app: &mut App) {
    if let Some(tab) = app.tabs.get_mut(app.active_tab) {
        tab.descent_path.pop();
    }
    app.recalculate_filtered_order();
    app.invalidate_card_cache();
    // View-switch effect -- see `enter_container` for the
    // useEffect-on-dependency-change rationale. Ascent changes the
    // active navigation node back to the parent (or Root), so the
    // dependency-key comparison inside the helper picks up the change
    // and fires the appropriate hierarchy refresh.
    app.run_navigation_effect_if_changed();

    // IMMEDIATE hierarchy fetch for instant navigation response on ascent.
    // Determines the correct parent_id for the new navigation context.
    let current_parent_id = app
        .tabs
        .get(app.active_tab)
        .and_then(|tab| tab.descent_path.last().copied());
    app.trigger_hierarchy_fetch_immediate(current_parent_id);
}

/// Navigate to the next/previous session needing attention.
///
/// Consumes the shared predicate `crate::app::attention::attention_session_ids`
/// so the order matches both `]a / [a` cycling AND the bottom-strip queue
/// zone's numbered `1..9` jumps. The shared predicate is the load-bearing
/// guarantee that the user lands on the same session regardless of which
/// keypath they take.
fn navigate_attention(app: &mut App, forward: bool) {
    let attention_ids = crate::app::attention::attention_session_ids(app);

    if attention_ids.is_empty() {
        return;
    }

    let current_idx = app
        .selected_session_id()
        .and_then(|id| attention_ids.iter().position(|aid| *aid == id));

    let target_idx = match current_idx {
        Some(idx) => {
            if forward {
                (idx + 1) % attention_ids.len()
            } else if idx == 0 {
                attention_ids.len() - 1
            } else {
                idx - 1
            }
        }
        None => 0,
    };

    let target_id = attention_ids[target_idx];

    if let Some(order_idx) = app.session_order.iter().position(|id| *id == target_id) {
        let focused = app.interaction_pane_id();
        let is_detail = matches!(
            app.tabs[app.active_tab].find_pane(focused),
            Some(Pane::SessionDetail { .. })
        );

        if is_detail {
            // In detail view: use open_session_in_current_pane so the jumplist
            // is updated. Without this, Ctrl+o skips the session you jumped from
            // and lands on an earlier one ("second item instead of first").
            app.open_session_in_current_pane(target_id);
        } else {
            let focused = app.interaction_pane_id();
            let tab = &mut app.tabs[app.active_tab];
            if let Some(Pane::SessionList {
                selected_index,
                selected_session,
                ..
            }) = tab.find_pane_mut(focused)
            {
                *selected_index = order_idx;
                *selected_session = Some(target_id);
            }
        }
    }
}

/// Dispatch async refresh actions for event-driven navigation updates.
/// These replace periodic polling with user-triggered manual refreshes.
pub(super) async fn dispatch_async_refresh(app: &mut App, action: LcAction) {
    match action {
        LcAction::RefreshNavigation => {
            // Force refresh all navigation data - sessions, projects, labels
            app.invalidate_all_cache();
            app.trigger_full_navigation_refresh().await;

            app.push_notification(
                crate::types::NotificationKind::Info,
                crate::types::NotificationPriority::Medium,
                "Navigation data refreshed".to_string(),
                None,
            );
        }

        LcAction::RefreshCurrentView => {
            // Refresh only the current view (session or hierarchy)
            app.run_navigation_effect_if_changed();

            // Force immediate refresh regardless of cache state
            let focused_node = app.active_navigation_effect_node();
            match focused_node {
                crate::app::NavigationEffectNode::Root => {
                    app.trigger_hierarchy_fetch_immediate(None);
                }
                crate::app::NavigationEffectNode::Container(id) => {
                    app.trigger_hierarchy_fetch_immediate(Some(id));
                }
                crate::app::NavigationEffectNode::Leaf(id) => {
                    app.trigger_focus_fetch_if_needed(id);
                }
            }

            app.push_notification(
                crate::types::NotificationKind::Info,
                crate::types::NotificationPriority::Low,
                "Current view refreshed".to_string(),
                None,
            );
        }

        LcAction::RefreshMetadata => {
            // Refresh only projects and labels metadata
            app.trigger_projects_fetch_immediate().await;
            app.trigger_labels_fetch_immediate().await;

            app.push_notification(
                crate::types::NotificationKind::Info,
                crate::types::NotificationPriority::Low,
                "Projects and labels refreshed".to_string(),
                None,
            );
        }

        _ => unreachable!("dispatch_async_refresh called with non-refresh action"),
    }
}

#[cfg(test)]
mod descent_tests {
    use super::*;
    use crate::client::DaemonClient;
    use crate::state::{DevState, PersistedState};
    use crate::types::SessionState;
    use rsi_common::types::{Session, SessionKind, SessionProvider, SessionStatus};
    use std::path::PathBuf;

    fn test_app() -> App {
        DevState::clear();
        PersistedState::default().save();
        App::new(DaemonClient::new(PathBuf::from("/tmp/test.sock")))
    }

    fn make_session(kind: SessionKind) -> Session {
        Session {
            context_fill_pct: None,
            id: uuid::Uuid::new_v4(),
            provider: SessionProvider::Claude,
            claude_session_id: None,
            query: "test".to_string(),
            title: None,
            agent_role: None,
            epic_spawn_ordinal: None,
            description: None,
            short_summary: None,
            pending_question: None,
            pending_archive: false,
            working_dir: PathBuf::from("/tmp"),
            git_branch: None,
            status: SessionStatus::Running,
            project_id: None,
            session_kind: kind,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            cost_usd: None,
            duration_ms: None,
            num_turns: None,
            model: None,
            input_tokens: None,
            output_tokens: None,
            context_window: None,
            resolved_context_budget: None,
            total_input_tokens: None,
            total_output_tokens: None,
            total_cache_creation_tokens: None,
            total_cache_read_tokens: None,
            stop_reason: None,
            context_usage_confidence: rsi_common::types::ContextUsageConfidence::default(),
            continued_from: None,
            pinned_at: None,
            testing_needed_at: None,
            rotation_disabled_at: None,
            handoff_filepath: None,
            active_task: None,
            group_id: None,
            scheduled_job_id: None,
            pipeline_artifact: None,
            workflow_id: None,
            workflow_id_override: None,
            rotation_depth: 0,
            retry_attempt: None,
            max_retries: None,
            daemon_input_tokens: None,
            daemon_output_tokens: None,
            effort: None,
            issue_identifier: None,
            issue_url: None,
            issue_tracker_id: None,
            rating: None,
            harness_version_hash: None,
            test_passed: None,
            clippy_passed: None,
            turn_count: None,
            retry_count: None,
            approval_wait_ms: None,
            work_time_ms: None,
            approval_started_at: None,
            sandbox_kind: None,
            sandbox_root: None,
            sandbox_branch: None,
            sandbox_cleanup_state: None,
            tag: String::new(),
            tags: Vec::new(),
            parent_id: None,
            lead_session_id: None,
            is_eval: false,
            capability_class: None,
            topology_node_id: None,
            topology_iteration: 0,
            provider_cli_version: None,
            provider_capabilities: Vec::new(),
            thinking_tokens: None,
            service_tier: None,
            cache_creation_1h_tokens: None,
            cache_creation_5m_tokens: None,
            permission_denial_count: None,
            subagent_stats_json: None,
            queued_turn_count: None,
            terminal_reason: None,
        }
    }

    #[test]
    fn enter_container_pushes_id_onto_descent_path() {
        let mut app = test_app();
        let group = make_session(SessionKind::Group);
        let group_id = group.id;
        app.sessions.insert(group_id, SessionState::new(group));

        super::enter_container(&mut app, group_id);
        assert_eq!(app.tabs[app.active_tab].descent_path, vec![group_id]);
    }

    #[test]
    fn ascend_container_pops_one_level() {
        let mut app = test_app();
        let id1 = uuid::Uuid::new_v4();
        let id2 = uuid::Uuid::new_v4();
        let mut s1 = make_session(SessionKind::Group);
        s1.id = id1;
        let mut s2 = make_session(SessionKind::Epic);
        s2.id = id2;
        s2.parent_id = Some(id1);
        app.sessions
            .insert(id1, crate::types::SessionState::new(s1));
        app.sessions
            .insert(id2, crate::types::SessionState::new(s2));
        app.tabs[app.active_tab].descent_path = vec![id1, id2];

        super::ascend_container(&mut app);
        assert_eq!(app.tabs[app.active_tab].descent_path, vec![id1]);

        super::ascend_container(&mut app);
        assert!(app.tabs[app.active_tab].descent_path.is_empty());

        // Empty descent_path: ascend is a no-op.
        super::ascend_container(&mut app);
        assert!(app.tabs[app.active_tab].descent_path.is_empty());
    }

    #[test]
    fn enter_container_collapses_list_card_for_target() {
        let mut app = test_app();
        let group = make_session(SessionKind::Group);
        let group_id = group.id;
        app.sessions.insert(group_id, SessionState::new(group));
        // Pre-set the card to expanded (Phase 3 behavior); enter should collapse it.
        if let Some(state) = app.sessions.get_mut(&group_id) {
            state.list_card_expanded = true;
        }
        super::enter_container(&mut app, group_id);
        assert!(!app.sessions.get(&group_id).unwrap().list_card_expanded);
    }

    #[test]
    fn enter_container_selects_first_visible_child_without_lead() {
        let mut app = test_app();
        let group = make_session(SessionKind::Group);
        let group_id = group.id;
        let mut epic = make_session(SessionKind::Epic);
        let epic_id = epic.id;
        epic.parent_id = Some(group_id);

        app.session_order.push(group_id);
        app.session_order.push(epic_id);
        app.sessions.insert(group_id, SessionState::new(group));
        app.sessions.insert(epic_id, SessionState::new(epic));
        app.sort_sessions(true);

        let focused = app.interaction_pane_id();
        if let Some(Pane::SessionList {
            selected_index,
            selected_session,
            ..
        }) = app.tabs[app.active_tab].find_pane_mut(focused)
        {
            *selected_index = 0;
            *selected_session = Some(group_id);
        }

        super::enter_container(&mut app, group_id);

        assert_eq!(app.filtered_session_order, vec![epic_id]);
        assert_eq!(app.selected_session_id(), Some(epic_id));
    }

    #[test]
    fn explicit_fold_commands_refresh_nested_list_and_close_all() {
        let mut app = test_app();
        let group = make_session(SessionKind::Group);
        let group_id = group.id;
        let mut epic = make_session(SessionKind::Epic);
        let epic_id = epic.id;
        epic.parent_id = Some(group_id);
        let mut leaf = make_session(SessionKind::Task);
        let leaf_id = leaf.id;
        leaf.parent_id = Some(epic_id);

        app.session_order.extend([group_id, epic_id, leaf_id]);
        app.sessions.insert(group_id, SessionState::new(group));
        app.sessions.insert(epic_id, SessionState::new(epic));
        app.sessions.insert(leaf_id, SessionState::new(leaf));
        app.sort_sessions(true);
        if let Some(Pane::SessionList {
            selected_index,
            selected_session,
            ..
        }) = app.focused_pane_mut()
        {
            *selected_index = 0;
            *selected_session = Some(group_id);
        }

        super::dispatch(&mut app, LcAction::ToggleFold);
        assert_eq!(app.filtered_session_order, vec![group_id, epic_id]);

        if let Some(Pane::SessionList {
            selected_index,
            selected_session,
            ..
        }) = app.focused_pane_mut()
        {
            *selected_index = 1;
            *selected_session = Some(epic_id);
        }
        super::dispatch(&mut app, LcAction::OpenFold);
        assert_eq!(app.filtered_session_order, vec![group_id, epic_id, leaf_id]);

        super::dispatch(&mut app, LcAction::CloseAllFolds);
        assert_eq!(app.filtered_session_order, vec![group_id]);
        assert!(app.sessions.values().all(|state| !state.list_card_expanded));
    }

    /// Entering a container updates the navigation dependency key. It must
    /// not fetch the lead leaf just because the list cursor moved; leaf
    /// conversations are fetched when the leaf detail view becomes active.
    #[tokio::test]
    async fn enter_container_updates_navigation_effect_node() {
        let mut app = test_app();

        // Container with lead set to the leaf child.
        let mut group = make_session(SessionKind::Group);
        let group_id = group.id;
        let leaf = make_session(SessionKind::Standard);
        let leaf_id = leaf.id;
        group.lead_session_id = Some(leaf_id);

        // Child must have parent_id = group_id so it appears in the
        // descended filtered list. Also: empty events, no last_sequence
        // (unloaded preconditions for the focus-fetch trigger).
        let mut child_session = leaf;
        child_session.parent_id = Some(group_id);

        app.session_order.push(group_id);
        app.session_order.push(leaf_id);
        app.sessions.insert(group_id, SessionState::new(group));
        app.sessions
            .insert(leaf_id, SessionState::new(child_session));
        // Mirror the production data path so children_by_parent is populated.
        app.sort_sessions(true);

        // Sanity: pre-call, nothing inflight.
        assert!(app.focus_fetch_inflight.is_empty());

        super::enter_container(&mut app, group_id);

        assert_eq!(
            app.navigation_effect_node,
            Some(crate::app::NavigationEffectNode::Container(group_id))
        );
        assert!(
            app.focus_fetch_inflight.is_empty(),
            "list cursor movement must not fetch leaf conversations"
        );
    }

    /// `ascend_container` without a lead-driven re-selection should not
    /// trigger a focus fetch (selected_session is unchanged).
    #[tokio::test]
    async fn ascend_container_without_selection_change_does_not_fetch() {
        let mut app = test_app();
        let id1 = uuid::Uuid::new_v4();
        let mut s1 = make_session(SessionKind::Group);
        s1.id = id1;
        app.sessions
            .insert(id1, crate::types::SessionState::new(s1));
        app.tabs[app.active_tab].descent_path = vec![id1];

        super::ascend_container(&mut app);

        assert!(app.tabs[app.active_tab].descent_path.is_empty());
        assert!(
            app.focus_fetch_inflight.is_empty(),
            "no selection change -> no focus fetch dispatched"
        );
    }

    #[tokio::test]
    async fn test_ascend_or_back_from_detail() {
        let mut app = test_app();
        let group_id = uuid::Uuid::new_v4();
        let epic_id = uuid::Uuid::new_v4();
        let leaf_id = uuid::Uuid::new_v4();

        let mut g = make_session(SessionKind::Group);
        g.id = group_id;
        let mut e = make_session(SessionKind::Epic);
        e.id = epic_id;
        e.parent_id = Some(group_id);
        let mut l = make_session(SessionKind::Standard);
        l.id = leaf_id;
        l.parent_id = Some(epic_id);

        app.sessions
            .insert(group_id, crate::types::SessionState::new(g));
        app.sessions
            .insert(epic_id, crate::types::SessionState::new(e));
        app.sessions
            .insert(leaf_id, crate::types::SessionState::new(l));

        app.tabs[app.active_tab].descent_path = vec![group_id, epic_id];
        let focused = app.tabs[app.active_tab].focused_pane;
        if let Some(pane) = app.tabs[app.active_tab].layout.find_pane_mut(focused) {
            *pane = Pane::SessionDetail {
                session_id: leaf_id,
            };
        }

        // Running AscendOrBack from SessionDetail should return to list (keeping descent_path)
        super::dispatch(&mut app, LcAction::AscendOrBack);
        assert!(matches!(app.focused_pane(), Some(Pane::SessionList { .. })));
        assert_eq!(
            app.tabs[app.active_tab].descent_path,
            vec![group_id, epic_id]
        );

        // Running AscendOrBack from SessionList should pop descent_path
        super::dispatch(&mut app, LcAction::AscendOrBack);
        assert_eq!(app.tabs[app.active_tab].descent_path, vec![group_id]);

        super::dispatch(&mut app, LcAction::AscendOrBack);
        assert!(app.tabs[app.active_tab].descent_path.is_empty());
    }
}

/// PI-7 (D2, F-010): `OpenFold`/`ToggleFold` redesigned single-press
/// expand/collapse, plus `CloseFold` regression pins proving its unchanged
/// 2-tier (collapse-one-item, then re-group) logic still works correctly
/// under the new bulk-expand behavior.
#[cfg(test)]
mod fold_tests {
    use super::*;
    use crate::app::app_test_helpers;
    use crate::types::{PaneId, SplitNode};
    use rsi_common::types::{ConversationEvent, EventType, Role};

    /// Fixture: `SessionDetail`-focused App with a 3-event contiguous
    /// `ToolUse` run (sequences 10, 11, 12, each carrying `tool_input` so all
    /// three default-collapsed under the D1 formula), cursor on the first
    /// (group-leader) event.
    fn fold_fixture() -> (App, uuid::Uuid) {
        let (mut app, session_id) = app_test_helpers::with_session_detail();
        {
            let state = app
                .sessions
                .get_mut(&session_id)
                .expect("fixture session should exist");
            state.events = (0..3)
                .map(|i| ConversationEvent {
                    id: 0,
                    session_id,
                    sequence: 10 + i,
                    event_type: EventType::ToolUse,
                    role: Some(Role::Assistant),
                    content: format!("tool call {i}"),
                    tool_name: Some("Bash".to_string()),
                    tool_input: Some(Box::new(serde_json::json!({"command": "echo hi"}))),
                    created_at: chrono::Utc::now(),
                    offload_id: None,
                    tool_use_id: None,
                    metadata: None,
                })
                .collect();
            state.events_generation += 1;
            state.current_event_index = Some(0);
            state.collapsed_events.clear();
            state.expanded_events.clear();
            state.expanded_tool_groups.clear();
        }
        let pane_id = PaneId(0);
        app.tabs[0].layout = SplitNode::Leaf {
            pane: Pane::SessionDetail { session_id },
            id: pane_id,
        };
        app.tabs[0].focused_pane = pane_id;
        (app, session_id)
    }

    #[test]
    fn open_fold_on_collapsed_run_expands_all_events_in_one_press() {
        let (mut app, session_id) = fold_fixture();

        dispatch(&mut app, LcAction::OpenFold);

        let state = app.sessions.get(&session_id).unwrap();
        for seq in [10, 11, 12] {
            assert!(
                state.expanded_events.contains(&seq),
                "seq {seq} should be expanded after a single zo on the collapsed run"
            );
            assert!(!state.collapsed_events.contains(&seq));
        }
    }

    #[test]
    fn toggle_fold_on_collapsed_run_expands_all_events_in_one_press() {
        let (mut app, session_id) = fold_fixture();

        dispatch(&mut app, LcAction::ToggleFold);

        let state = app.sessions.get(&session_id).unwrap();
        for seq in [10, 11, 12] {
            assert!(
                state.expanded_events.contains(&seq),
                "seq {seq} should be expanded after a single za on the collapsed run"
            );
        }
    }

    #[test]
    fn close_fold_collapses_only_the_cursor_item_within_expanded_run() {
        let (mut app, session_id) = fold_fixture();
        dispatch(&mut app, LcAction::OpenFold); // whole run expanded, cursor stays on seq 10

        dispatch(&mut app, LcAction::CloseFold);

        let state = app.sessions.get(&session_id).unwrap();
        assert!(
            state.collapsed_events.contains(&10),
            "cursor item (seq 10) should be individually collapsed"
        );
        assert!(!state.expanded_events.contains(&10));
        assert!(
            state.expanded_events.contains(&11),
            "seq 11 must remain expanded — zc must not lose the rest of the run"
        );
        assert!(
            state.expanded_events.contains(&12),
            "seq 12 must remain expanded — zc must not lose the rest of the run"
        );
    }

    #[test]
    fn close_fold_again_on_same_item_regroups_to_summary_line() {
        let (mut app, session_id) = fold_fixture();
        dispatch(&mut app, LcAction::OpenFold);
        dispatch(&mut app, LcAction::CloseFold); // first zc: individual collapse

        dispatch(&mut app, LcAction::CloseFold); // second zc: re-group

        let state = app.sessions.get(&session_id).unwrap();
        assert!(state.collapsed_events.contains(&10));
        assert!(
            !state.expanded_tool_groups.contains(&10),
            "a second zc on the same (still-collapsed) item should re-group it back \
             to the single summary line, per CloseFold's unchanged 2-tier logic"
        );
    }
}
