//! Application event types and event loop setup.

use crossterm::event::KeyEvent;
use std::time::{Duration, Instant};

/// Events that flow through the TUI's main event loop.
#[derive(Debug)]
pub enum AppEvent {
    /// Terminal key press
    Key(KeyEvent),
    /// Terminal resize
    Resize(u16, u16),
    /// Time to poll daemon for updates
    Tick,
    /// Render frame
    Render,
}

trait EventClock {
    fn now(&self) -> Instant;
}

#[derive(Debug, Clone, Copy, Default)]
struct SystemEventClock;

impl EventClock for SystemEventClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct KeyStepResult {
    suppress_poll: bool,
}

fn expire_esp_flash(app: &mut crate::app::App, now: Instant) {
    if let crate::types::OverlayState::EspSquare {
        flash,
        flash_deadline,
        ..
    } = &mut app.overlay
    {
        if let Some(deadline) = *flash_deadline {
            if now >= deadline {
                *flash = None;
                *flash_deadline = None;
                app.needs_redraw = true;
            } else {
                app.needs_redraw = true;
            }
        }
    }
}

async fn handle_issue_workspace_registered_key(app: &mut crate::app::App, key: KeyEvent) -> bool {
    if !matches!(app.focused_pane(), Some(crate::types::Pane::Issues(_))) {
        return false;
    }
    if app.handle_issue_workspace_editor_key(key).await {
        return true;
    }
    let context = crate::action_registry::ActionContext::from_app(app);
    if !crate::action_registry::has_binding_for_key(&context, key) {
        return false;
    }
    match crate::action_registry::request_for_key(&context, key) {
        crate::action_registry::ActionAvailability::Available(request) => {
            crate::action_handler::dispatch_registered_action(app, request).await;
        }
        crate::action_registry::ActionAvailability::Unavailable { reason } => app.notify(reason),
    }
    true
}

/// Process one key event through the production TUI key path.
pub async fn step_once(app: &mut crate::app::App, key: KeyEvent) {
    let clock = SystemEventClock;
    let _ = step_once_with_clock(app, key, &clock).await;
}

fn is_config_gated_submission(app: &crate::app::App, key: KeyEvent) -> bool {
    use crate::key_tables::{CONFIG_GATED_SUBMIT_KEYS, ConfigGateEffect, ConfigGateGuard};
    use crate::types::{CreateEntityField, InputMode, InputPurpose, OverlayState, PromptPurpose};

    if app.authoritative_config_ready() {
        return false;
    }

    let launch_prompt = |overlay: Option<&OverlayState>| {
        matches!(
            overlay,
            Some(OverlayState::Prompt { purpose, .. })
                if !matches!(purpose, PromptPurpose::ContinueSession(_))
        )
    };
    let create_form = || match &app.overlay {
        OverlayState::CreateEntityForm {
            focused_field,
            insert_mode,
            model_dropdown,
            ..
        } if !model_dropdown
            .as_ref()
            .is_some_and(|dropdown| dropdown.open) =>
        {
            Some((focused_field, *insert_mode))
        }
        _ => None,
    };
    let entry = crate::key_tables::lookup(CONFIG_GATED_SUBMIT_KEYS, key, |guard| match guard {
        ConfigGateGuard::QuickLaunchInput => {
            app.input_mode == InputMode::Input
                && matches!(app.input_purpose, InputPurpose::NewSession)
        }
        ConfigGateGuard::LaunchPrompt => {
            launch_prompt(Some(&app.overlay)) || launch_prompt(app.focused_input_overlay())
        }
        ConfigGateGuard::CreateForm => create_form().is_some(),
        ConfigGateGuard::CreateFormNavigating => {
            create_form().is_some_and(|(focused_field, insert_mode)| {
                !insert_mode
                    && !matches!(
                        focused_field,
                        CreateEntityField::Body | CreateEntityField::Topology
                    )
            })
        }
    });
    match entry.map(|entry| entry.effect) {
        Some(ConfigGateEffect::RefuseUntilConfigReady) => true,
        None => false,
    }
}

/// Whether a global-intercept guard holds for the current app state.
fn global_guard_holds(app: &crate::app::App, guard: crate::key_tables::GlobalGuard) -> bool {
    use crate::key_tables::GlobalGuard;

    let unobstructed = || {
        app.input_mode == crate::types::InputMode::Normal
            && !app.any_overlay_active()
            && !is_input_bar_insert(app)
    };
    match guard {
        GlobalGuard::Always => true,
        GlobalGuard::TerminalOverlay => {
            matches!(&app.overlay, crate::types::OverlayState::Terminal)
        }
        GlobalGuard::Unobstructed => unobstructed(),
        GlobalGuard::UnobstructedSessionList => {
            unobstructed() && app.focused_pane_is_session_list()
        }
        GlobalGuard::UnobstructedOtherPane => unobstructed() && !app.focused_pane_is_session_list(),
        GlobalGuard::UnobstructedOtherPaneNoStaleIssueEditor => {
            unobstructed()
                && !app.focused_pane_is_session_list()
                && !matches!(
                    app.focused_pane(),
                    Some(crate::types::Pane::Issues(state))
                        if state
                            .transient
                            .editor
                            .as_ref()
                            .is_some_and(|editor| editor.stale_conflict)
                )
        }
        GlobalGuard::GeometryOverlay => {
            app.any_overlay_active() && app.current_overlay_geometry_key().is_some()
        }
        GlobalGuard::NormalNoOverlay => {
            app.input_mode == crate::types::InputMode::Normal && !app.any_overlay_active()
        }
    }
}

/// Run one global-intercept effect.
async fn run_global_effect(app: &mut crate::app::App, effect: crate::key_tables::GlobalEffect) {
    use crate::key_tables::GlobalEffect;
    use crate::modalkit_types::LcAction;

    match effect {
        GlobalEffect::ToggleHelp => {
            if matches!(
                app.overlay,
                crate::types::OverlayState::KeybindingsHelp { .. }
            ) {
                crate::overlay::close_keybindings_help(app);
            } else {
                crate::overlay::open_keybindings_help(app);
            }
        }
        GlobalEffect::InterruptTerminal => {
            // ^C = 0x03 (ETX / SIGINT)
            if let Some(ref mut term) = app.terminal {
                let _ = term.write_input(&[0x03]);
            }
        }
        GlobalEffect::Quit => app.quit = true,
        GlobalEffect::ToggleTerminal => {
            crate::action_handler::dispatch_lc_action(app, LcAction::ToggleTerminal).await;
        }
        GlobalEffect::JumpBack => app.jump_back(),
        GlobalEffect::JumpForward => app.jump_forward(),
        GlobalEffect::SessionListZonePrev => {
            crate::action_handler::dispatch_lc_action(app, LcAction::SessionListZonePrev).await;
        }
        GlobalEffect::SessionListZoneNext => {
            crate::action_handler::dispatch_lc_action(app, LcAction::SessionListZoneNext).await;
        }
        GlobalEffect::FocusLeft => app.focus_neighbor(crate::app::NavDirection::Left),
        GlobalEffect::FocusRight => app.focus_neighbor(crate::app::NavDirection::Right),
        GlobalEffect::ResizeOverlay(dw, dh) => app.adjust_overlay_geometry(0, 0, dw, dh),
        GlobalEffect::MoveOverlay(dx, dy) => app.adjust_overlay_geometry(dx, dy, 0, 0),
        GlobalEffect::ResetOverlayGeometry => app.reset_overlay_geometry(),
        GlobalEffect::GrowSidebar => {
            crate::action_handler::dispatch_lc_action(app, LcAction::GrowSidebar).await;
        }
        GlobalEffect::ShrinkSidebar => {
            crate::action_handler::dispatch_lc_action(app, LcAction::ShrinkSidebar).await;
        }
        GlobalEffect::NextEvent => {
            crate::action_handler::dispatch_lc_action(app, LcAction::NextEvent).await;
        }
        GlobalEffect::PrevEvent => {
            crate::action_handler::dispatch_lc_action(app, LcAction::PrevEvent).await;
        }
    }
}

async fn step_once_with_clock<C: EventClock>(
    app: &mut crate::app::App,
    key: KeyEvent,
    clock: &C,
) -> KeyStepResult {
    use crossterm::event::KeyEventKind;
    use keybindings::BindingMachine;
    use modalkit::key::TerminalKey;

    // Ctrl+V clipboard paste - must come before the Press-only filter.
    // Ghostty with `performable:ctrl+v=paste_from_clipboard` sends
    // the key as a Release event (not Press) with SHIFT|CONTROL modifiers.
    // We accept both Press and Release, with or without SHIFT, to handle
    // all terminal behaviors. Deduplicate via timestamp to prevent
    // double-paste on terminals that send both Press and Release.
    let paste =
        crate::key_tables::lookup(crate::key_tables::PASTE_KEYS, key, |guard| match guard {
            crate::key_tables::PasteGuard::PressOrRelease => {
                matches!(key.kind, KeyEventKind::Press | KeyEventKind::Release)
            }
        });
    if paste.map(|entry| entry.effect) == Some(crate::key_tables::PasteEffect::PasteClipboard) {
        let now = clock.now();
        if now.duration_since(app.last_paste_instant) < Duration::from_millis(50) {
            // Duplicate Ctrl+V event (Press+Release) - skip
            return KeyStepResult::default();
        }

        // Only paste in insert mode contexts
        let handled = if app.any_overlay_active() {
            // Overlay is active - delegate to overlay paste
            crate::overlay::try_paste_overlay(app)
        } else {
            // Try input bar paste
            crate::input_bar::try_paste_input_bar(app)
        };
        // Update timestamp AFTER the paste completes (after clipboard I/O).
        // If set before, screenshot PNG encoding (100-300ms) causes the
        // timestamp gap to exceed 50ms by the time the Release event is
        // processed, allowing a spurious second paste to fire.
        app.last_paste_instant = clock.now();
        if handled {
            app.mark_dirty();
            return KeyStepResult::default();
        }
        // Not in a paste-eligible context - fall through to normal handling
    }

    // Only process key press events - ignore Release and Repeat.
    // Terminals with Kitty keyboard protocol (Ghostty, Alacritty)
    // send Release events for every keypress, which would cause
    // the KeyManager to see each key twice and break multi-key
    // sequences like ZZ, ZQ, gg, gt, ]a, etc.
    if key.kind != KeyEventKind::Press {
        return KeyStepResult::default();
    }

    // Session/config submissions fail closed until this process has applied an
    // authoritative daemon configuration. Intercept before any input surface
    // clears or closes so the user's draft and launch intent remain intact.
    if is_config_gated_submission(app, key) {
        app.notify_error(app.daemon_config_unavailable_reason());
        app.mark_dirty();
        return KeyStepResult {
            suppress_poll: true,
        };
    }

    // Global intercepts (`key_tables::GLOBAL_KEY_INTERCEPTS`): the help
    // chord for active text entry, quit/terminal, jumplist, pane focus and
    // zone cycling, overlay geometry, sidebar width and event selection. The
    // first row whose chord and context match wins.
    let global =
        crate::key_tables::lookup(crate::key_tables::GLOBAL_KEY_INTERCEPTS, key, |guard| {
            global_guard_holds(app, guard)
        })
        .map(|entry| entry.effect);
    if let Some(effect) = global {
        run_global_effect(app, effect).await;
    }
    // NOTE: Plain Up/Down arrow keys scroll session detail
    // (handled by handle_detail_scroll_key below). They bypass
    // the input bar when no autocomplete suggestions are visible.
    // Mouse wheel also scrolls (see mouse event handler below).
    else {
        // Overlay captures all input when active
        if crate::overlay::handle_overlay_key(app, key).await {
            // Overlay consumed the key - skip normal dispatch
        } else if crate::file_viewer::handle_file_viewer_key(app, key) {
            // File viewer consumed the key
        } else if crate::input_bar::handle_input_bar_key(app, key).await {
            // Input bar (insert mode) consumed the key
        } else if handle_session_list_nav_key(app, key) {
            // Left/Right navigated session list
        } else if handle_detail_scroll_key(app, key) {
            // Plain Up/Down scrolled session detail
        } else {
            match app.input_mode {
                crate::types::InputMode::Normal => {
                    // Prompt creator pane intercepts keys before vim machine
                    let in_prompt_creator =
                        matches!(app.focused_pane(), Some(crate::types::Pane::PromptCreator));
                    // Settings pane intercepts keys before vim machine
                    let in_settings =
                        matches!(app.focused_pane(), Some(crate::types::Pane::Settings));
                    if in_prompt_creator
                        && crate::prompt_creator_keys::handle_prompt_creator_key(app, key)
                    {
                        // Prompt creator consumed the key; drain any async actions queued by the handler
                        let deferred = std::mem::take(&mut app.pending_lc_actions);
                        for lc_action in deferred {
                            crate::action_handler::dispatch_lc_action(app, lc_action).await;
                        }
                    } else if in_settings
                        && crate::settings_keys::handle_settings_key_event(app, key).await
                    {
                        // Settings consumed the key; drain any async actions queued by the handler
                        let deferred = std::mem::take(&mut app.pending_lc_actions);
                        for lc_action in deferred {
                            crate::action_handler::dispatch_lc_action(app, lc_action).await;
                        }
                    } else if handle_issue_workspace_registered_key(app, key).await {
                        // The Issues pane consumes only its registry-owned commands and editor text.
                    } else {
                        // Feed key to modalkit's KeyManager
                        let terminal_key = TerminalKey::from(key);
                        app.key_manager.input_key(terminal_key);

                        // Pop and dispatch all resulting actions
                        let mut actions = Vec::new();
                        while let Some((action, _ctx)) = app.key_manager.pop() {
                            actions.push(action);
                        }

                        // Track whether the vim machine is mid-sequence
                        // (key was consumed but produced no action yet).
                        // When pending, the input bar bypasses its normal-mode
                        // handling so leader sequences (Space+g) pass through.
                        app.vim_machine_pending = actions.is_empty();

                        for action in actions {
                            crate::action_handler::dispatch_action(app, action).await;
                        }

                        // Follow-ups that run after the Vim keymap
                        // (Esc clears a confirmed search).
                        let follow_up = crate::key_tables::lookup(
                            crate::key_tables::NORMAL_FOLLOW_UP_KEYS,
                            key,
                            |guard| match guard {
                                crate::key_tables::NormalFollowUpGuard::ConfirmedSearch => {
                                    !app.search_query.is_empty()
                                }
                            },
                        )
                        .map(|entry| entry.effect);
                        match follow_up {
                            Some(crate::key_tables::NormalFollowUpEffect::ClearConfirmedSearch) => {
                                app.search_query.clear();
                                app.search_matches.clear();
                                app.search_match_cursor = 0;
                                app.clear_search_filter();
                            }
                            None => {}
                        }
                    }
                }
                crate::types::InputMode::Input => {
                    handle_input_mode(app, key).await;
                }
                crate::types::InputMode::Command => {
                    handle_command_mode(app, key).await;
                }
                crate::types::InputMode::Search => {
                    handle_search_mode(app, key);
                }
            }
        }
    }

    app.mark_dirty(); // Any keypress = redraw
    KeyStepResult {
        suppress_poll: true,
    }
}

/// Run the main event loop.
/// Returns when the application should exit.
pub async fn run_event_loop(
    terminal: &mut ratatui::Terminal<impl ratatui::backend::Backend + std::io::Write>,
    app: &mut crate::app::App,
) {
    use crossterm::event::{Event as CEvent, EventStream};
    use futures::StreamExt;

    let mut reader = EventStream::new();
    let mut render_interval = tokio::time::interval(Duration::from_micros(8333)); // 120fps
    let clock = SystemEventClock;

    // Skip missed ticks instead of bursting to catch up. Without this,
    // blocking RPC calls cause missed ticks to fire rapidly in sequence,
    // cascading into 100% CPU and frozen rendering.
    render_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Dynamic poll interval: sleep-based so we can change the period at runtime.
    // When push notifications are active, poll every 5s (safety net for metadata sync).
    // When push is unavailable, poll every 500ms (legacy behavior).
    let poll_sleep = tokio::time::sleep(Duration::from_millis(500));
    // Pin the sleep future so it can be used in select!
    tokio::pin!(poll_sleep);
    // Give render and navigation-scoped fetch results a short runway before
    // starting a potentially slow fallback poll RPC.
    let mut suppress_poll_until = tokio::time::Instant::now();

    // Periodic save of PersistedState for crash resilience (every 30 seconds)
    let mut persist_interval = tokio::time::interval(Duration::from_secs(30));
    persist_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Periodic TaskRabbit visibility recalculation (every 60 seconds) for 2-hour window freshness
    let mut taskrabbit_recalc_interval = tokio::time::interval(Duration::from_secs(60));
    taskrabbit_recalc_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Defensive repaint every second. Covers missed SIGWINCH/resize events (Ghostty + i3
    // can drop the CEvent::Resize delivery). Ratatui queries backend.size() at draw time,
    // so the next terminal.draw() after mark_dirty() always uses the correct terminal
    // dimensions — no terminal.clear() needed, no visible flicker.
    let mut repaint_interval = tokio::time::interval(Duration::from_secs(1));
    repaint_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Catch SIGTERM so cargo-watch rebuilds trigger clean exits with state saved.
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("SIGTERM handler");

    // Catch SIGHUP so terminal death (closed tab, killed cargo-watch) triggers clean exit
    // instead of leaving an orphaned process spinning CPU on a dead pty.
    let mut sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        .expect("SIGHUP handler");

    // Start the shared bootstrap coordinator, but enter select immediately.
    // Rendering, input, signals, persistence, and repaint remain live while
    // the daemon withholds any or all startup RPC responses.
    app.start_bootstrap();

    // Phase-based polling state. When Some, the poll cycle is in progress
    // and each iteration executes one RPC call. When None, waiting for
    // the poll interval to start the next cycle.
    let mut poll_phase: Option<crate::poll_controller::PollPhase> = None;

    loop {
        tokio::select! {
            biased;

            // RENDER: highest priority — never starved by polling or input
            _ = render_interval.tick() => {
                // Expire TTL-based notifications before rendering
                if app.expire_notifications() {
                    app.mark_dirty();
                }

                // Expire ESP Square flash after deadline
                expire_esp_flash(app, clock.now());

                if app.needs_redraw {
                    // Detect terminal size changes even when CEvent::Resize was
                    // dropped (known i3/Ghostty issue). If the backend reports a
                    // different size than last draw, force a full clear.
                    if let Ok(size) = terminal.size() {
                        let current = (size.width, size.height);
                        if current != app.last_terminal_size && app.last_terminal_size != (0, 0) {
                            app.pane_switch_clear = true;
                        }
                        app.last_terminal_size = current;
                    }

                    // Skip rendering entirely when terminal has zero dimensions
                    // (can happen momentarily during i3 workspace transitions).
                    if app.last_terminal_size.0 == 0 || app.last_terminal_size.1 == 0 {
                        continue;
                    }

                    // Backend-level clear: resets ratatui's previous buffer so
                    // the next draw forces a full repaint. Needed on transparent
                    // terminals where ratatui's buffer diff can't detect
                    // Reset→Reset cell transitions after layout shifts, and
                    // after i3 hide/show cycles that desync the compositor
                    // surface from ratatui's internal buffer.
                    if app.pane_switch_clear {
                        let _ = terminal.clear();
                        app.pane_switch_clear = false;
                    }
                    // Resize embedded terminal to match overlay dimensions before draw
                    if matches!(&app.overlay, crate::types::OverlayState::Terminal) {
                        if let Some(ref mut term) = app.terminal {
                            let (w, h) = app.last_terminal_size;
                            let inner_w = (w * 90 / 100).max(20).saturating_sub(2);
                            let inner_h = (h * 80 / 100).max(5).saturating_sub(2);
                            term.resize(inner_h, inner_w);
                        }
                    }

                    if terminal.draw(|frame| {
                        crate::ui::render(frame, &mut *app);
                    }).is_err() {
                        app.quit = true;
                    } else {
                        app.record_first_frame();
                    }
                    // Keep redraws flowing while a loading bar is visible
                    // so its time-based animation stays smooth (~30 fps).
                    // Also keep redraws flowing while the terminal overlay is active so
                    // PTY output (shell prompt, command results) appears immediately without
                    // waiting for the next keypress or repaint interval.
                    app.needs_redraw = app.has_loading_bar
                        || app.has_formulation_animation
                        || matches!(
                            &app.overlay,
                            crate::types::OverlayState::Terminal
                        );
                }
            }

            // BOOTSTRAP RESULT: all startup/reconnect I/O is performed on
            // bounded background tasks and applied only on this main task.
            Some(result) = app.bootstrap.recv() => {
                if app.apply_bootstrap_event(result) {
                    app.mark_dirty();
                }
            }

            maybe_event = reader.next() => {
                match maybe_event {
                    Some(Ok(CEvent::Key(key))) => {
                        let result = step_once_with_clock(app, key, &clock).await;
                        if result.suppress_poll {
                            suppress_poll_until =
                                tokio::time::Instant::now() + Duration::from_millis(20);
                        }
                    }
                    Some(Ok(CEvent::Mouse(mouse))) => {
                        use crossterm::event::{MouseEventKind, MouseButton};
                        const MOUSE_SCROLL_LINES: usize = 3;
                        match mouse.kind {
                            MouseEventKind::Down(MouseButton::Left) => {
                                if !handle_file_viewer_mouse_click(
                                    &mut *app,
                                    mouse.column,
                                    mouse.row,
                                ) {
                                    handle_mouse_click(&mut *app, mouse.column, mouse.row);
                                }
                            }
                            MouseEventKind::ScrollUp => {
                                if !handle_file_viewer_mouse_scroll(
                                    &mut *app,
                                    mouse.column,
                                    mouse.row,
                                    true,
                                    MOUSE_SCROLL_LINES,
                                ) && !scroll_mouse_region_aware(&mut *app, mouse.column, mouse.row, true) {
                                    for _ in 0..MOUSE_SCROLL_LINES {
                                        app.nav_up();
                                    }
                                }
                            }
                            MouseEventKind::ScrollDown => {
                                if !handle_file_viewer_mouse_scroll(
                                    &mut *app,
                                    mouse.column,
                                    mouse.row,
                                    false,
                                    MOUSE_SCROLL_LINES,
                                ) && !scroll_mouse_region_aware(&mut *app, mouse.column, mouse.row, false) {
                                    for _ in 0..MOUSE_SCROLL_LINES {
                                        app.nav_down();
                                    }
                                }
                            }
                            MouseEventKind::Down(MouseButton::Right) => {
                                handle_mouse_right_click(&mut *app, mouse.column, mouse.row);
                            }
                            _ => {}
                        }
                        app.mark_dirty();
                    }
                    Some(Ok(CEvent::Paste(text))) => {
                        // Bracketed paste from terminal (e.g. Ghostty performable:ctrl+v).
                        // The terminal wraps pasted content in \e[200~...\e[201~ delimiters,
                        // crossterm parses it into this single event with the full text.
                        // Deduplicate against Ctrl+V handler (which also fires on some terminals).
                        let now = clock.now();
                        if now.duration_since(app.last_paste_instant) >= Duration::from_millis(50) {
                            app.last_paste_instant = now;
                            let handled = if matches!(app.overlay, crate::types::OverlayState::Terminal) {
                                // Forward paste straight to the embedded PTY.
                                if let Some(term) = &mut app.terminal {
                                    let _ = term.paste(&text);
                                }
                                true
                            } else if app.any_overlay_active() {
                                crate::overlay::try_paste_text_overlay(app, &text)
                            } else {
                                crate::input_bar::try_paste_text_input_bar(app, &text)
                            };
                            if handled {
                                app.mark_dirty();
                            }
                        }
                    }
                    Some(Ok(CEvent::Resize(w, h))) => {
                        // Force a full buffer clear so ratatui repaints every cell.
                        // Without this, i3 hide/show cycles desync the compositor
                        // surface from ratatui's internal diff buffer, producing a
                        // blank screen (ratatui thinks nothing changed).
                        app.pane_switch_clear = true;
                        app.last_terminal_size = (w, h);
                        app.mark_dirty();
                    }
                    Some(Err(_)) => {
                        app.quit = true;
                    }
                    None => {
                        app.quit = true;
                    }
                    _ => {}
                }
            }

            // PUSH EVENTS: receive real-time events from daemon notification stream
            event = async {
                match app.notification_stream.as_mut() {
                    Some(stream) => stream.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                if let Some(stream_event) = event {
                    if app.apply_notification_stream_event(stream_event) {
                        app.mark_dirty();
                    }
                    let deferred = std::mem::take(&mut app.pending_lc_actions);
                    for lc_action in deferred {
                        crate::action_handler::dispatch_lc_action(app, lc_action).await;
                    }
                } else {
                    // The worker normally emits a typed loss first. Channel
                    // closure without one is still routed through the same
                    // shared reconnect owner.
                    app.mark_notification_stream_lost();
                }
            }

            // FOCUS FETCH RESULT: on-demand GetConversation dispatched by
            // trigger_focus_fetch_if_needed (RSI hierarchy nav latency
            // refactor: Option A) lands here. `None` is impossible because
            // `focus_fetch_tx` is held by `App` and outlives the loop.
            Some(focus_result) = app.focus_fetch_rx.recv() => {
                if app.apply_focus_fetch_result(focus_result) {
                    app.mark_dirty();
                }
                // Enhanced: Longer suppression when navigation effects are active
                let suppression_duration = if !app.focus_fetch_inflight.is_empty()
                    || !app.hierarchy_fetch_inflight.is_empty() {
                    Duration::from_millis(100) // Allow navigation effects to complete
                } else {
                    Duration::from_millis(20)  // Standard brief suppression
                };
                suppress_poll_until = tokio::time::Instant::now() + suppression_duration;
            }

            // FALLBACK CONVERSATION POLL RESULT: the periodic poll owns a
            // short-lived client and timeout. Applying its result (including
            // reconnect classification) stays on the event-loop task.
            Some(result) = app.conversation_poll_rx.recv() => {
                if app.apply_conversation_poll_result(result) {
                    app.mark_dirty();
                }
            }

            // AUTO RESUME HANDOFF RESULT: launch socket work is owned by a
            // bounded task so applying a conversation poll never awaits a
            // daemon write on the event loop.
            Some(result) = app.auto_resume_handoff_rx.recv() => {
                if app.apply_auto_resume_handoff_result(result) {
                    app.mark_dirty();
                }
            }

            // INTERACTIVE LAUNCH RESULT: the main task alone owns draft
            // destruction and returned-session placement.
            Some(result) = app.interactive_launch_rx.recv() => {
                if app.apply_interactive_launch_result(result) {
                    app.mark_dirty();
                }
            }

            // DOCUMENT REGISTRATION RESULT: launch/archive lifecycle commits
            // occur only after their exact semantic responses arrive.
            Some(result) = app.docreg_operation_rx.recv() => {
                if app.apply_docreg_operation_result(result) {
                    app.mark_dirty();
                }
            }

            // CLASSIFIER CONFIG RESULT: keep dropdown and daemon mirror
            // unchanged until the exact UpdateDaemonConfig response settles.
            Some(result) = app.classifier_config_rx.recv() => {
                if app.apply_classifier_config_result(result) {
                    app.mark_dirty();
                }
            }

            // HIERARCHY FETCH RESULT: targeted ListSessionChildren dispatched
            // by the active navigation-node effect.
            Some(hierarchy_result) = app.hierarchy_fetch_rx.recv() => {
                if app.apply_hierarchy_fetch_result(hierarchy_result) {
                    app.mark_dirty();
                }
                // Enhanced: Longer suppression when navigation effects are active
                let suppression_duration = if !app.focus_fetch_inflight.is_empty()
                    || !app.hierarchy_fetch_inflight.is_empty() {
                    Duration::from_millis(100) // Allow navigation effects to complete
                } else {
                    Duration::from_millis(20)  // Standard brief suppression
                };
                suppress_poll_until = tokio::time::Instant::now() + suppression_duration;
            }

            // MODEL SEGMENTS FETCH RESULT: model segments fetched on SessionDetail focus
            Some(segments_result) = app.model_segments_fetch_rx.recv() => {
                if app.apply_model_segments_fetch_result(segments_result) {
                    app.mark_dirty();
                }
            }

            // POLL START: begin a new poll cycle when the sleep completes
            // ENHANCED: Only start poll cycles for active conversations, not navigation data
            () = &mut poll_sleep, if poll_phase.is_none()
                && app.focus_fetch_inflight.is_empty()
                && app.hierarchy_fetch_inflight.is_empty()
                && tokio::time::Instant::now() >= suppress_poll_until => {
                poll_phase = app.start_poll_cycle();
                // ENHANCED: Longer intervals since we only poll active sessions now
                let interval = if app.bootstrap_polling_pending() {
                    // Bounded retry cadence while initial/reconnect bootstrap
                    // is incomplete. `start_bootstrap` itself is single-flight.
                    Duration::from_millis(500)
                } else if poll_phase.is_some() {
                    // Active sessions need regular polling
                    if app.notification_stream.is_some() {
                        Duration::from_secs(10) // Reduced frequency with push notifications
                    } else {
                        Duration::from_secs(2)  // Still frequent for active sessions without push
                    }
                } else {
                    // No active sessions - much longer sleep
                    Duration::from_secs(30)
                };
                poll_sleep.as_mut().reset(tokio::time::Instant::now() + interval);
            }

            // POLL STEP: execute one RPC call per iteration, with a small
            // delay to let render and input arms fire between steps.
            _ = tokio::time::sleep(Duration::from_millis(1)), if poll_phase.is_some()
                && app.focus_fetch_inflight.is_empty()
                && app.hierarchy_fetch_inflight.is_empty()
                && tokio::time::Instant::now() >= suppress_poll_until => {
                let (next_phase, changed) = app.poll_step(poll_phase.take().unwrap()).await;
                poll_phase = next_phase;
                if changed {
                    app.mark_dirty();
                }
            }

            // Manager catalogs own their request/generation and never update global model selection.
            // The same policy-owned branch also delivers background usage (#674).
            result = crate::overlay::manager_v2::policy::usage::next_result(&mut app.overlay) => {
                crate::overlay::manager_v2::policy::usage::dispatch_result(&mut app.overlay, result);
                app.mark_dirty();
            }

            // MODEL DISCOVERY: kick off background task when provider tab changes
            _ = tokio::time::sleep(Duration::from_millis(1)), if app.needs_model_refresh && app.model_discovery_rx.is_none() => {
                app.needs_model_refresh = false;
                let provider = app.model_refresh_provider.take().unwrap_or(app.selected_provider);
                let socket_path = app.client.socket_path().to_path_buf();
                let fallback = crate::app::models_for_provider(provider);
                let (tx, rx) = tokio::sync::oneshot::channel();
                app.model_discovery_rx = Some(rx);
                tokio::spawn(async move {
                    let mut client = crate::client::DaemonClient::new(socket_path);
                    let models = if client.connect().await.is_ok() {
                        client.discover_models(provider).await.unwrap_or(fallback)
                    } else {
                        fallback
                    };
                    let _ = tx.send(crate::app::ModelDiscoveryResult { provider, models });
                });
            }

            // MODEL DISCOVERY RESULT: receive background discovery results
            result = async {
                match app.model_discovery_rx.as_mut() {
                    Some(rx) => rx.await,
                    None => std::future::pending().await,
                }
            } => {
                app.model_discovery_rx = None;
                if let Ok(crate::app::ModelDiscoveryResult { provider, models }) = result {
                    if !models.is_empty() {
                        if provider == app.selected_provider {
                            app.available_models = models.clone();
                            // Preserve persisted model selection if it exists in the discovered list
                            let persisted_valid = app.selected_model.as_ref()
                                .is_some_and(|m| app.available_models.iter().any(|(id, _)| id == m));
                            if !persisted_valid {
                                app.selected_model = app.available_models.first().map(|(id, _)| id.clone());
                            }
                        }
                        // Sync to global model dropdown widget if open and provider matches
                        if app.model_dropdown.open && app.model_dropdown.provider == provider {
                            app.model_dropdown.models = models.clone();
                            if app.model_dropdown.selected_index >= app.model_dropdown.models.len() {
                                app.model_dropdown.selected_index = 0;
                            }
                        }
                        // Sync to per-prompt dropdowns (app.overlay and input_overlays)
                        if let crate::types::OverlayState::Prompt { model_dropdown, .. } = &mut app.overlay {
                            if model_dropdown.open && model_dropdown.provider == provider {
                                model_dropdown.models = models.clone();
                                if model_dropdown.selected_index >= model_dropdown.models.len() {
                                    model_dropdown.selected_index = 0;
                                }
                            }
                        }
                        for input_overlay in &mut app.input_overlays {
                            if let crate::types::OverlayState::Prompt { model_dropdown, .. } = input_overlay {
                                if model_dropdown.open && model_dropdown.provider == provider {
                                    model_dropdown.models = models.clone();
                                    if model_dropdown.selected_index >= model_dropdown.models.len() {
                                        model_dropdown.selected_index = 0;
                                    }
                                }
                            }
                        }
                        // Sync to the create-entity form's embedded dropdown (Group/Epic/
                        // Story/Task/Bug creation) if open and provider matches.
                        if let crate::types::OverlayState::CreateEntityForm { model_dropdown: Some(model_dropdown), .. } = &mut app.overlay {
                            if model_dropdown.provider == provider {
                                model_dropdown.models = models.clone();
                                if model_dropdown.selected_index >= model_dropdown.models.len() {
                                    model_dropdown.selected_index = 0;
                                }
                            }
                        }
                        // Sync to settings dropdown if open and provider matches
                        if app.settings_state.model_dropdown.open
                            && app.settings_state.model_dropdown.provider == provider
                        {
                            app.settings_state.model_dropdown.models = models;
                            if app.settings_state.model_dropdown.selected_index
                                >= app.settings_state.model_dropdown.models.len()
                            {
                                app.settings_state.model_dropdown.selected_index = 0;
                            }
                        }
                    }
                }
                app.mark_dirty();
            }

            Some(event) = app.issues_rx.recv() => {
                app.apply_issue_workspace_event(event);
            }

            // SETTINGS MODEL DISCOVERY RESULT: receive Agent Actors row-specific discovery results
            result = async {
                match app.settings_model_discovery_rx.as_mut() {
                    Some(rx) => rx.await,
                    None => std::future::pending().await,
                }
            } => {
                app.settings_model_discovery_rx = None;
                if let Ok(result) = result {
                    let still_current = {
                        let dropdown = &app.settings_state.model_dropdown;
                        dropdown.open
                            && app.settings_state.active_dropdown_item == Some(result.item_idx)
                            && dropdown.provider == result.provider
                            && dropdown.custom_provider_index == result.custom_provider_index
                    };
                    if still_current && !result.models.is_empty() {
                        let current_model = match result.item_idx {
                            0 => Some(app.settings.title_model_local.as_str()),
                            1 => Some(app.settings.prompt_processor.model.as_str()),
                            2 => Some(app.settings.memory_model_fallback.as_str()),
                            4 => Some(app.settings.dream_model.as_str()),
                            _ => None,
                        };
                        let dropdown = &mut app.settings_state.model_dropdown;
                        dropdown.models = result.models;
                        dropdown.selected_index = current_model
                            .and_then(|cm| dropdown.models.iter().position(|(id, _)| id == cm))
                            .unwrap_or(0);
                    }
                }
                app.mark_dirty();
            }

            // LOCAL MODEL DISCOVERY: kick off background task to fetch Ollama models
            _ = tokio::time::sleep(Duration::from_millis(1)), if app.needs_local_model_refresh && app.local_model_discovery_rx.is_none() => {
                app.needs_local_model_refresh = false;
                let socket_path = app.client.socket_path().to_path_buf();
                let fallback = crate::app::models_for_provider(rsi_common::types::SessionProvider::Local);
                let (tx, rx) = tokio::sync::oneshot::channel();
                app.local_model_discovery_rx = Some(rx);
                tokio::spawn(async move {
                    let mut client = crate::client::DaemonClient::new(socket_path);
                    let models = if client.connect().await.is_ok() {
                        client.discover_models(rsi_common::types::SessionProvider::Local).await.unwrap_or(fallback)
                    } else {
                        fallback
                    };
                    let _ = tx.send(models);
                });
            }

            // LOCAL MODEL DISCOVERY RESULT: receive Ollama model list
            result = async {
                match app.local_model_discovery_rx.as_mut() {
                    Some(rx) => rx.await,
                    None => std::future::pending().await,
                }
            } => {
                app.local_model_discovery_rx = None;
                if let Ok(models) = result {
                    if !models.is_empty() {
                        app.local_models = models;
                        // Live-update any Agent Actors dropdown currently pointed at the
                        // built-in Local provider so fresh Ollama models appear in place.
                        let local_item_open = matches!(
                            app.settings_state.active_dropdown_item,
                            Some(0) | Some(1) | Some(2) | Some(4)
                        ) && app.settings_state.model_dropdown.provider
                            == rsi_common::types::SessionProvider::Local
                            && app.settings_state.model_dropdown.custom_provider_index.is_none();
                        if app.settings_state.model_dropdown.open && local_item_open {
                            let current = match app.settings_state.active_dropdown_item {
                                Some(0) => Some(app.settings.title_model_local.clone()),
                                Some(1) => Some(app.settings.prompt_processor.model.clone()),
                                Some(2) => Some(app.settings.memory_model_fallback.clone()),
                                Some(4) => Some(app.settings.dream_model.clone()),
                                _ => None,
                            };
                            app.settings_state.model_dropdown.models = app.local_models.clone();
                            // Re-anchor selection to current model; fall back to 0.
                            app.settings_state.model_dropdown.selected_index = current
                                .and_then(|cm| {
                                    app.settings_state
                                        .model_dropdown
                                        .models
                                        .iter()
                                        .position(|(id, _)| id == &cm)
                                })
                                .unwrap_or(0);
                        }
                        app.mark_dirty();
                    }
                }
            }

            // PROMPT COMPILE RESULT: receive background compilation results
            result = async {
                match app.prompt_compile_rx.as_mut() {
                    Some((_, _, rx)) => rx.await,
                    None => std::future::pending().await,
                }
            } => {
                let taken = app.prompt_compile_rx.take();
                let overlay_id = taken.as_ref().map(|(id, _, _)| *id);
                let original_input = taken.map(|(_, input, _)| input);

                if let Some(overlay_id) = overlay_id {
                    match result {
                        Ok(outcome) => app.apply_prompt_compile_result(overlay_id, outcome, original_input),
                        Err(_) => {
                            if let Some(surface) = app.overlay_input_surface_mut(overlay_id) {
                                surface.correction_in_flight = false;
                            }
                        }
                    }
                }
                app.mark_dirty();
            }

            // INPUT BAR COMPILE RESULT: receive prompt compilation for session detail input bar
            result = async {
                match app.input_bar_compile_rx.as_mut() {
                    Some((_, _, rx)) => rx.await,
                    None => std::future::pending().await,
                }
            } => {
                let taken = app.input_bar_compile_rx.take();
                let session_id = taken.as_ref().map(|(id, _, _)| *id);
                let original_input = taken.map(|(_, input, _)| input);
                let mut notification: Option<String> = None;

                if let Some(sid) = session_id {
                    if let Some(state) = app.sessions.get_mut(&sid) {
                        state.input_bar.surface.correction_in_flight = false;
                        match result {
                            Ok(Ok(compile_result)) => {
                                if !compile_result.layer_validation.all_present() {
                                    let missing = compile_result.layer_validation.missing().join(", ");
                                    notification = Some(format!("Compiler: weak layers [{missing}]"));
                                }
                                match &compile_result.contract {
                                    crate::prompt_processor::OutputContract::Complete
                                    | crate::prompt_processor::OutputContract::Incomplete { .. } => {
                                        if let Some(orig) = &original_input {
                                            state.input_bar.surface.corrected_compile_context = Some(
                                                crate::input_surface::CompileContext {
                                                    original_input: orig.clone(),
                                                    contract_status: compile_result.contract.to_status_string(),
                                                    layer_semantic: compile_result.layer_validation.semantic,
                                                    layer_syntactic: compile_result.layer_validation.syntactic,
                                                    layer_deictic: compile_result.layer_validation.deictic,
                                                    layer_discourse: compile_result.layer_validation.discourse,
                                                    layer_pragmatic: compile_result.layer_validation.pragmatic,
                                                },
                                            );
                                        }
                                        state.input_bar.surface.corrected_preview = Some(compile_result.compiled);
                                    }
                                    crate::prompt_processor::OutputContract::Error { kind, message } => {
                                        notification = Some(format!("Compile ERROR [{kind}]: {message}"));
                                    }
                                    _ => {
                                        notification = Some("Compile result: unknown contract variant".to_string());
                                    }
                                }
                            }
                            Ok(Err(err)) => {
                                if let Some(desc) = err.strip_prefix("Ambiguous intent: ") {
                                    state.input_bar.surface.corrected_preview =
                                        Some(format!("CLARIFY:\n- {desc}"));
                                } else {
                                    notification = Some(format!("Prompt compilation failed: {err}"));
                                }
                            }
                            Err(_) => {} // channel dropped, discard
                        }
                    }
                }

                if let Some(msg) = notification {
                    app.notify(msg);
                }
                app.mark_dirty();
            }

            // AI CHAT RESULT: receive AI chat response
            result = async {
                match app.ai_chat_rx.as_mut() {
                    Some(rx) => rx.await,
                    None => std::future::pending().await,
                }
            } => {
                app.ai_chat_rx = None;

                if let crate::types::OverlayState::AiChat {
                    messages,
                    in_flight,
                    scroll_offset,
                    ..
                } = &mut app.overlay
                {
                    *in_flight = false;
                    match result {
                        Ok(Ok(response)) => {
                            messages.push(("assistant".to_string(), response));
                            // Auto-scroll to bottom
                            *scroll_offset = usize::MAX; // render fn will clamp
                        }
                        Ok(Err(err)) => {
                            messages
                                .push(("assistant".to_string(), format!("Error: {err}")));
                        }
                        Err(_) => {} // channel dropped
                    }
                }
                app.mark_dirty();
            }

            // DIALECTIC RESULT: receive dialectic query response
            result = async {
                match app.dialectic_rx.as_mut() {
                    Some(rx) => rx.await,
                    None => std::future::pending().await,
                }
            } => {
                app.dialectic_rx = None;

                if let crate::types::OverlayState::Dialectic {
                    messages,
                    sources,
                    in_flight,
                    scroll_offset,
                    ..
                } = &mut app.overlay
                {
                    *in_flight = false;
                    match result {
                        Ok(Ok(response)) => {
                            messages.push(("assistant".to_string(), response.answer));
                            *sources = response.sources;
                            // Auto-scroll to bottom
                            *scroll_offset = messages.len().saturating_sub(1);
                        }
                        Ok(Err(err)) => {
                            messages
                                .push(("assistant".to_string(), format!("Error: {err}")));
                        }
                        Err(_) => {} // channel dropped
                    }
                }
                app.mark_dirty();
            }

            // RECURSIVE DAG RESULT: receive read-only browser RPC response
            result = async {
                match app.recursive_dag_rx.as_mut() {
                    Some(rx) => rx.await,
                    None => std::future::pending().await,
                }
            } => {
                app.recursive_dag_rx = None;
                match result {
                    Ok(outcome) => crate::overlay::recursive_dag::apply_recursive_dag_load_result(app, outcome),
                    Err(_) => crate::overlay::recursive_dag::apply_recursive_dag_load_result(
                        app,
                        Err("recursive DAG load task was cancelled".to_string()),
                    ),
                }
                app.mark_dirty();
            }

            // AI COMMAND RESULT: receive AI text transformation results
            result = async {
                match app.ai_command_rx.as_mut() {
                    Some((_, rx)) => rx.await,
                    None => std::future::pending().await,
                }
            } => {
                let source_info = app.ai_command_rx.take().map(|(source, _)| source);

                // Close the AiCommand overlay, restoring any saved overlay
                if matches!(&app.overlay, crate::types::OverlayState::AiCommand { .. }) {
                    app.restore_previous_overlay();
                }

                if let Some(source) = source_info {
                    match result {
                        Ok(outcome) => app.apply_ai_command_result(source, outcome),
                        Err(_) => {} // channel dropped, discard
                    }
                }
                app.mark_dirty();
            }

            // TERMINAL PTY OUTPUT: receive bytes from embedded terminal reader thread
            data = async {
                match app.terminal_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending::<Option<Vec<u8>>>().await,
                }
            } => {
                match data {
                    Some(bytes) => {
                        if let Some(ref mut term) = app.terminal {
                            term.process_bytes(&bytes);
                            // Only redraw if terminal overlay is visible
                            if matches!(&app.overlay, crate::types::OverlayState::Terminal) {
                                app.mark_dirty();
                            }
                        }
                    }
                    None => {
                        // Reader thread exited — shell died
                        if let Some(ref mut term) = app.terminal {
                            term.alive = false;
                        }
                        app.terminal_rx = None;
                        if matches!(&app.overlay, crate::types::OverlayState::Terminal) {
                            app.mark_dirty();
                        }
                    }
                }
            }

            _ = sigterm.recv() => {
                app.quit = true;
            }

            _ = sighup.recv() => {
                app.quit = true;
            }

            // PERIODIC SAVE: persist navigation state for crash resilience
            _ = persist_interval.tick() => {
                crate::state::PersistedState::capture(app).save();
            }

            // TASKRABBIT RECALCULATION: refresh 2-hour visibility window every 60s
            _ = taskrabbit_recalc_interval.tick() => {
                app.recalculate_filtered_order();
                // Fix up selections after potential removal due to age-out
                app.reconcile_restored_selections();
                // State may have visibly changed (session order, selections) — redraw
                app.mark_dirty();
            }

            // DEFENSIVE REPAINT: covers missed SIGWINCH events from Ghostty/i3 resize.
            // Ratatui auto-queries terminal size at draw time, so this is sufficient to
            // correct any stale layout without a terminal.clear().
            _ = repaint_interval.tick() => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis()
                    .min(i64::MAX as u128) as i64;
                app.issue_workspace_tick(now);
                app.mark_dirty();
            }
        }

        app.poll_manager_roster_refresh().await;

        if let Some(mode) = app.pending_manual.take() {
            crate::manual::open::open_manual(app, mode);
        }
        // External commands borrow the real terminal only after input dispatch returns.
        if let Some(request) = app.pending_external.take() {
            let (program, args, cwd) = match request {
                crate::app::ExternalRequest::Lazygit(directory) => {
                    ("lazygit".to_string(), Vec::new(), directory)
                }
                crate::app::ExternalRequest::Pager(path) => {
                    let (program, args) = crate::manual::open::pager_command(&path);
                    let cwd = path
                        .parent()
                        .unwrap_or_else(|| std::path::Path::new("/"))
                        .to_path_buf();
                    (program, args, cwd)
                }
            };
            if let Err(error) = spawn_external(terminal, &program, &args, &cwd) {
                app.notify_error(format!("{program} failed: {error}"));
            }
            app.mark_dirty();
        }

        if app.quit {
            break;
        }
    }

    // Kill embedded terminal shell before exit
    if let Some(mut term) = app.terminal.take() {
        term.kill();
    }
    app.terminal_rx = None;

    // Emergency graph draft save — dirty drafts written to ~/.flywheel/graph-drafts/
    // so they survive daemon loss, crashes, or normal restarts.
    crate::state::emergency_drafts::save(app.active_graph_draft_id, &app.graph_drafts);

    // Save dev state for hot-reload (catches all exit paths: Ctrl+C, :q, ZQ, SIGTERM, EOF)
    crate::state::DevState::capture(app).save();
    // Save persisted state for restart persistence
    crate::state::PersistedState::capture(app).save();
}

/// Suspend the TUI, run a program with terminal access, and restore the TUI on every exit path.
fn spawn_external(
    terminal: &mut ratatui::Terminal<impl ratatui::backend::Backend + std::io::Write>,
    program: &str,
    args: &[std::ffi::OsString],
    working_dir: &std::path::Path,
) -> std::io::Result<std::process::ExitStatus> {
    use std::process::{Command, Stdio};

    // Suspend: leave alternate screen, disable raw mode
    let _ = crossterm::execute!(
        terminal.backend_mut(),
        crossterm::event::PopKeyboardEnhancementFlags,
        crossterm::terminal::LeaveAlternateScreen,
        crossterm::event::DisableMouseCapture
    );
    let _ = crossterm::terminal::disable_raw_mode();
    let _ = terminal.show_cursor();

    // Spawn with full terminal access. Restoration runs for success and errors.
    let result = Command::new(program)
        .args(args)
        .current_dir(working_dir)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status();

    // Resume: re-enter alternate screen, re-enable raw mode
    let _ = crossterm::terminal::enable_raw_mode();
    let _ = crossterm::execute!(
        terminal.backend_mut(),
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::EnableMouseCapture,
        crossterm::event::PushKeyboardEnhancementFlags(
            crossterm::event::KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                .union(crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )
    );
    let _ = terminal.hide_cursor();
    let _ = terminal.clear();
    result
}

/// Handle key events in Input mode (typing a query for new session / continue).
async fn handle_input_mode(app: &mut crate::app::App, key: KeyEvent) {
    use crate::key_tables::{LineEditEffect, LineEditor};

    let Some(effect) = line_edit_effect(LineEditor::QuickInput, key) else {
        return;
    };
    match effect {
        LineEditEffect::Submit => {
            let query = app.input_buffer.clone();
            if !query.is_empty() {
                match &app.input_purpose {
                    crate::types::InputPurpose::NewSession => {
                        app.request_quick_launch(
                            &query,
                            None,
                            None,
                            crate::app::InteractiveLaunchOrigin::QuickInput {
                                query: query.clone(),
                            },
                            crate::app::LaunchPlacement::CurrentPane,
                        );
                        return;
                    }
                    crate::types::InputPurpose::ContinueSession(session_id) => {
                        let sid = *session_id;
                        app.continue_session(sid, &query).await;
                    }
                }
            }
            app.input_buffer.clear();
            app.input_mode = crate::types::InputMode::Normal;
            app.input_purpose = crate::types::InputPurpose::NewSession; // reset
        }
        LineEditEffect::Cancel => {
            if app.interactive_quick_launch_pending() {
                app.notify("Session launch is awaiting daemon acceptance; draft preserved");
                return;
            }
            app.input_buffer.clear();
            app.input_mode = crate::types::InputMode::Normal;
            app.input_purpose = crate::types::InputPurpose::NewSession; // reset
        }
        LineEditEffect::DeleteBackward => {
            app.input_buffer.pop();
        }
        LineEditEffect::InsertChar => {
            if let Some(c) = crate::key_tables::Chord::typed_char(key) {
                app.input_buffer.push(c);
            }
        }
    }
}

/// The `LINE_EDIT_KEYS` effect for `key` in one of event.rs's line editors.
fn line_edit_effect(
    editor: crate::key_tables::LineEditor,
    key: KeyEvent,
) -> Option<crate::key_tables::LineEditEffect> {
    crate::key_tables::lookup(crate::key_tables::LINE_EDIT_KEYS, key, |guard| {
        guard == editor
    })
    .map(|entry| entry.effect)
}

/// Return the currently focused viewer when a mouse coordinate lies in its rendered body.
fn file_viewer_at_mouse(app: &crate::app::App, col: u16, row: u16) -> Option<uuid::Uuid> {
    let session_id = match app.focused_pane()? {
        crate::types::Pane::SessionDetail { session_id } => *session_id,
        crate::types::Pane::SessionList {
            selected_session: Some(session_id),
            ..
        } => *session_id,
        _ => return None,
    };
    let viewer = app.sessions.get(&session_id)?.file_viewer.as_ref()?;
    crate::file_viewer::mouse_targets_viewer(viewer, col, row).then_some(session_id)
}

/// Handle a left click in the file viewer before general session-list/detail hit testing.
fn handle_file_viewer_mouse_click(app: &mut crate::app::App, col: u16, row: u16) -> bool {
    let Some(session_id) = file_viewer_at_mouse(app, col, row) else {
        return false;
    };
    if let Some(viewer) = app
        .sessions
        .get_mut(&session_id)
        .and_then(|state| state.file_viewer.as_mut())
    {
        let _ = crate::file_viewer::set_cursor_from_mouse(viewer, col, row);
    }
    true
}

/// Handle a wheel event in the file viewer before falling back to session navigation.
fn handle_file_viewer_mouse_scroll(
    app: &mut crate::app::App,
    col: u16,
    row: u16,
    is_up: bool,
    amount: usize,
) -> bool {
    let Some(session_id) = file_viewer_at_mouse(app, col, row) else {
        return false;
    };
    if let Some(viewer) = app
        .sessions
        .get_mut(&session_id)
        .and_then(|state| state.file_viewer.as_mut())
    {
        crate::file_viewer::scroll_from_mouse(viewer, amount, !is_up);
    }
    true
}

/// Handle a mouse left-click: if it lands inside a session list pane, select the clicked
/// card. If it lands inside a session detail pane's content area, copy any file path,
/// web link, or code block on the clicked line to clipboard, then set the event cursor.
fn handle_mouse_click(app: &mut crate::app::App, col: u16, row: u16) {
    use crate::types::Pane;

    if app.any_overlay_active() {
        return;
    }

    let physical_pane = {
        let tab = app.active_tab();
        tab.layout.find_pane(tab.focused_pane).cloned()
    };

    match physical_pane.clone() {
        Some(Pane::SessionList { .. }) => {
            handle_session_list_click(app, col, row);
            return;
        }
        Some(Pane::SessionDetail { .. }) => {} // fall through to existing logic
        _ => return,
    }

    let session_id = match physical_pane {
        Some(Pane::SessionDetail { session_id }) => session_id,
        _ => return,
    };

    if app
        .sessions
        .get(&session_id)
        .is_some_and(|state| session_id_control_clicked(state, col, row))
    {
        let id = session_id.to_string();
        crate::clipboard::osc52_copy(&id);
        app.notify_success(format!("Copied session ID: {id}"));
        return;
    }

    // Extract all derived data within a limited borrow scope so we can call
    // app.notify_success() afterward (which requires &mut App).
    let (_, raw_path, code_block_content, web_link, working_dir) = {
        let Some(state) = app.sessions.get_mut(&session_id) else {
            return;
        };

        let (event_idx, path, code_block, web_link) = match detect_click_target_at(state, col, row)
        {
            Some(result) => result,
            None => return,
        };

        // Update cursor to clicked event (existing behavior — always runs)
        state.current_event_index = Some(event_idx);
        crate::ui::height::invalidate_heights(state);

        let wd = state.session.working_dir.clone();
        (event_idx, path, code_block, web_link, wd)
    };
    // `state` borrow ends here; `app` is fully accessible again

    if let Some(ref raw) = raw_path {
        // Resolve the path to a full absolute path before copying, so the user
        // always gets a usable, complete path in clipboard.
        let clean = raw.split(':').next().unwrap_or(raw);
        let resolved = resolve_file_path(clean, &working_dir);
        let copy_text = if resolved.is_file() {
            // Canonicalize to resolve any `.` / `..` components for a clean path
            resolved
                .canonicalize()
                .unwrap_or(resolved)
                .display()
                .to_string()
        } else if let Some(found) = crate::file_utils::find_file_by_suffix(&working_dir, clean) {
            found.canonicalize().unwrap_or(found).display().to_string()
        } else {
            // Last resort: join against working_dir so even unresolved paths
            // are copied as absolute rather than bare relative fragments.
            let fallback = if std::path::Path::new(raw).is_relative() {
                working_dir.join(raw).display().to_string()
            } else {
                raw.clone()
            };
            fallback
        };
        crate::clipboard::osc52_copy(&copy_text);
        app.notify_success(format!("Copied: {copy_text}"));
    } else if let Some(ref url) = web_link {
        crate::clipboard::osc52_copy(url);
        app.notify_success(format!("Copied: {url}"));
    } else if let Some(ref code) = code_block_content {
        crate::clipboard::osc52_copy(code);
        app.notify_success("Copied code block to clipboard");
    }
}

/// Detect click targets at the given screen coordinates within a session detail pane.
/// Returns the event index and optional file/code/web targets.
fn detect_click_target_at(
    state: &mut crate::types::SessionState,
    col: u16,
    row: u16,
) -> Option<(usize, Option<String>, Option<String>, Option<String>)> {
    // Ensure height cache is up-to-date before mapping coordinates.
    // Otherwise, if new events arrived but the render loop hasn't run yet,
    // the height cache will be stale/short, leading to out-of-bounds mapping
    // or clamping to the wrong event.
    if state.last_height_generation != state.events_generation
        || state.event_heights.len() != state.events.len()
    {
        crate::ui::height::update_event_heights(state, state.last_render_width);
    }

    let area = state.last_content_area;
    // Use the pre-computed Y offset that accounts for the compact detail header
    // plus any top-of-transcript context rows.
    // SESSION_DETAIL_HORIZ_INSET / 2 = the shared two-cell transcript inset.
    let inner_x = area.x + (crate::ui::theme::SESSION_DETAIL_HORIZ_INSET / 2);
    let inner_y = area.y + state.last_content_y_offset;
    let inner_w = area
        .width
        .saturating_sub(crate::ui::theme::SESSION_DETAIL_HORIZ_INSET);
    // Bottom: 1 for border
    let inner_h = area.height.saturating_sub(state.last_content_y_offset + 1);

    if col < inner_x || col >= inner_x + inner_w || row < inner_y || row >= inner_y + inner_h {
        return None;
    }

    let screen_line = (row - inner_y) as usize;
    let content_line = state.scroll_offset + screen_line;

    let i = state
        .event_offsets
        .iter()
        .rposition(|&off| off <= content_line)
        .filter(|&i| i < state.events.len())?;

    if state.event_heights.get(i).copied().unwrap_or(0) == 0 {
        return None;
    }

    let visual_line = content_line.saturating_sub(state.event_offsets[i]);

    let first_visible = state
        .event_offsets
        .iter()
        .rposition(|&off| off <= state.scroll_offset)
        .unwrap_or(0);

    let lines_to_skip = if i == first_visible {
        state.scroll_offset.saturating_sub(state.event_offsets[i])
    } else {
        0
    };

    let has_top_border =
        state.events[i].event_type != rsi_common::types::EventType::Message && lines_to_skip == 0;
    let adjusted_visual_line = if has_top_border {
        visual_line.saturating_sub(1)
    } else {
        visual_line
    };

    // Events are cached at width.saturating_sub(4) in update_event_heights()
    // to account for the shared rail/card horizontal inset.
    // We must use the same inner width here so the RenderCacheKey matches.
    let render_width = state.last_render_width.saturating_sub(4);
    let is_cursor_pre = state.current_event_index == Some(i);

    let mut path = None;
    let mut code_block = None;
    let mut web_link = None;

    if let Some(cached) =
        crate::ui::height::cached_render_event(state, i, render_width, is_cursor_pre)
    {
        let logical_idx = visual_to_logical_line(&cached.lines, adjusted_visual_line, render_width);

        // Check if the click lies inside a code block
        if let Some(cb) = cached
            .code_blocks
            .iter()
            .find(|cb| logical_idx >= cb.start_line && logical_idx < cb.end_line)
        {
            code_block = Some(cb.content.clone());
        }

        // Check if there is a file path span on the clicked logical line
        if let Some(line) = cached.lines.get(logical_idx) {
            path = cached
                .file_links
                .iter()
                .find(|link| link.line == logical_idx)
                .map(|link| link.target.clone())
                .or_else(|| {
                    let path_fg = crate::ui::theme::file_path_fg();
                    line.spans
                        .iter()
                        .find(|span| {
                            span.style.fg == Some(path_fg)
                                && span
                                    .style
                                    .add_modifier
                                    .contains(ratatui::style::Modifier::UNDERLINED)
                        })
                        .map(|span| span.content.as_ref().to_string())
                });

            // Web links are only checked when there's no file path on the same
            // line — a line never renders both. Unlike the file-path fallback
            // above, there's no span-scan fallback needed here: `scan_link_targets`
            // records every web link (markdown and bare URL) at build time, so
            // `web_links` metadata is always complete for lines that have one.
            if path.is_none() {
                web_link = cached
                    .web_links
                    .iter()
                    .find(|link| link.line == logical_idx)
                    .map(|link| link.target.clone());
            }
        }
    }

    Some((i, path, code_block, web_link))
}

/// True only for the two-cell ID control in the session-detail metadata row.
fn session_id_control_clicked(state: &crate::types::SessionState, col: u16, row: u16) -> bool {
    let area = state.last_content_area;
    row == area.y.saturating_add(1)
        && col >= area.x.saturating_add(1)
        && col < area.x.saturating_add(3)
}

/// Detect a file path or web link at the given screen coordinates, for the
/// right-click "navigate" handler — which needs to tell the two apart (open
/// in the file viewer vs. open in the browser) but never needs the code
/// block leg that `detect_click_target_at` also returns.
fn detect_link_target_at(
    state: &mut crate::types::SessionState,
    col: u16,
    row: u16,
) -> Option<(usize, Option<String>, Option<String>)> {
    detect_click_target_at(state, col, row).map(|(i, path, _, web_link)| (i, path, web_link))
}

/// Map a wrapped visual line index to the corresponding logical (pre-wrap) line index.
///
/// When ratatui wraps long lines, a single logical `Line` may occupy multiple visual
/// rows on screen. The height cache stores wrapped heights (via `Paragraph::line_count`),
/// but `cached.lines` contains the original unwrapped logical lines. This function
/// walks through logical lines, computing each one's wrapped height, to find which
/// logical line contains the target visual line.
fn visual_to_logical_line(
    lines: &[ratatui::text::Line<'static>],
    visual_line: usize,
    width: u16,
) -> usize {
    use ratatui::widgets::{Paragraph, Wrap};

    if width == 0 || lines.is_empty() {
        return 0;
    }

    let mut visual_offset = 0;
    for (logical_idx, line) in lines.iter().enumerate() {
        let wrapped_height = Paragraph::new(vec![line.clone()])
            .wrap(Wrap { trim: false })
            .line_count(width);
        let wrapped_height = wrapped_height.max(1);

        if visual_line < visual_offset + wrapped_height {
            return logical_idx;
        }
        visual_offset += wrapped_height;
    }

    lines.len().saturating_sub(1)
}

/// Resolve a cleaned file path string to an absolute `PathBuf`.
///
/// Handles `~/` (home dir expansion), absolute paths, and relative paths
/// (joined against `working_dir`).
fn resolve_file_path(clean_path: &str, working_dir: &std::path::Path) -> std::path::PathBuf {
    if clean_path.starts_with("~/") {
        dirs::home_dir()
            .map(|h| h.join(&clean_path[2..]))
            .unwrap_or_else(|| std::path::PathBuf::from(clean_path))
    } else if clean_path.starts_with('/') {
        std::path::PathBuf::from(clean_path)
    } else {
        working_dir.join(clean_path)
    }
}

/// Handle a mouse right-click: if it lands on a file path in session detail,
/// open the file in the per-session file viewer; if it lands on a web link,
/// open it in the default browser.
fn handle_mouse_right_click(app: &mut crate::app::App, col: u16, row: u16) {
    use crate::types::Pane;

    let session_id = match app.focused_pane().cloned() {
        Some(Pane::SessionDetail { session_id }) => session_id,
        _ => return,
    };

    // Detect file path or web link at click coordinates
    let (raw_path, web_link) = {
        let Some(state) = app.sessions.get_mut(&session_id) else {
            return;
        };

        match detect_link_target_at(state, col, row) {
            Some((event_idx, path, web_link)) if path.is_some() || web_link.is_some() => {
                // Select the clicked message before opening the file viewer / browser
                state.current_event_index = Some(event_idx);
                crate::ui::height::invalidate_heights(state);
                (path, web_link)
            }
            _ => return,
        }
    };

    if let Some(url) = web_link {
        match crate::browser::open_url(&url) {
            Ok(()) => app.notify_success(format!("Opened: {url}")),
            Err(e) => app.notify_error(format!("Could not open browser: {e}")),
        }
        return;
    }

    let raw_path = match raw_path {
        Some(path) => path,
        None => return,
    };

    // Strip line number suffix (:N or :N-M) that path detection may have included
    let clean_path = raw_path.split(':').next().unwrap_or(&raw_path);

    let working_dir = app
        .sessions
        .get(&session_id)
        .map(|s| s.session.working_dir.clone())
        .unwrap_or_else(|| std::path::PathBuf::from("."));

    // Resolve the path (handle ~/ and relative paths)
    let resolved = resolve_file_path(clean_path, &working_dir);

    // Validate file exists; if not, try fallback suffix search in working_dir
    let resolved = if resolved.is_file() {
        resolved
    } else if let Some(found) = crate::file_utils::find_file_by_suffix(&working_dir, clean_path) {
        found
    } else {
        app.notify_error(format!("File not found: {}", resolved.display()));
        return;
    };

    // Read file content with size guard (max 1MB)
    const MAX_FILE_SIZE: u64 = 1_048_576;
    match std::fs::metadata(&resolved) {
        Ok(meta) if meta.len() > MAX_FILE_SIZE => {
            app.notify_error(format!(
                "File too large ({:.1} MB): {}",
                meta.len() as f64 / 1_048_576.0,
                resolved.display()
            ));
            return;
        }
        Err(e) => {
            app.notify_error(format!("Cannot read {}: {e}", resolved.display()));
            return;
        }
        _ => {}
    }

    let content = match std::fs::read_to_string(&resolved) {
        Ok(c) => c,
        Err(e) => {
            app.notify_error(format!("Cannot read {}: {e}", resolved.display()));
            return;
        }
    };

    // Open file viewer — restore from cache if available, but detect external
    // changes by comparing fresh disk content to the cached disk_content
    // snapshot (same fencing as the file explorer and direct open paths).
    let Some(state) = app.sessions.get_mut(&session_id) else {
        return;
    };
    let conflict_detected =
        crate::file_viewer::activate_cached_viewer(state, resolved.clone(), content);

    if conflict_detected {
        app.notify_error(format!(
            "External change detected: {} (use :e! to reload or :w! to overwrite)",
            resolved.display()
        ));
    } else {
        app.notify_success(format!("Opened: {}", resolved.display()));
    }
}

/// Handle a mouse click on a session list pane: map click coordinates to a card
/// index using binary search through cumulative height arrays.
fn session_card_index_at(zone: &crate::types::ZoneRenderState, content_y: usize) -> Option<usize> {
    if content_y >= zone.total_content_height || zone.card_offsets.is_empty() {
        return None;
    }
    let candidate = zone
        .card_offsets
        .partition_point(|&offset| offset <= content_y);
    let card_index = candidate.saturating_sub(1);
    let card_top = *zone.card_offsets.get(card_index)?;
    let card_height = *zone.card_heights.get(card_index)?;
    (content_y >= card_top && content_y < card_top + card_height).then_some(card_index)
}

fn handle_session_list_click(app: &mut crate::app::App, col: u16, row: u16) {
    use crate::types::Pane;

    let rs = &app.session_list_render;
    let Some(list_area) = rs.last_list_area else {
        return;
    };

    // Check click is within the list area
    if row < list_area.y || row >= list_area.y + list_area.height {
        return;
    }

    let focused = app.tabs[app.active_tab].focused_pane;

    // Determine active zone to select the right render state
    let current_zone = app.tabs[app.active_tab]
        .layout
        .find_pane(focused)
        .and_then(|p| match p {
            Pane::SessionList { active_zone, .. } => Some(*active_zone),
            _ => None,
        })
        .unwrap_or(crate::types::SessionListZone::Main);

    // Check if click is in the cards area (covers whichever tab is active)
    let rs = &app.session_list_render;
    if let Some(cards) = rs.cards_area {
        if row >= cards.y
            && row < cards.y + cards.height
            && col >= cards.x
            && col < cards.x + cards.width
        {
            let zr = match current_zone {
                crate::types::SessionListZone::Main => &rs.main,
                crate::types::SessionListZone::TaskRabbit => &rs.taskrabbit,
                crate::types::SessionListZone::Archive => &rs.archive,
                crate::types::SessionListZone::Jobs => &rs.jobs,
            };
            let content_y = (row - cards.y) as usize + zr.scroll_offset;
            if let Some(card_idx) = session_card_index_at(zr, content_y) {
                let order = match current_zone {
                    crate::types::SessionListZone::Main => &app.filtered_session_order,
                    crate::types::SessionListZone::TaskRabbit => &app.filtered_taskrabbit_order,
                    crate::types::SessionListZone::Archive => &app.filtered_archived_order,
                    crate::types::SessionListZone::Jobs => &app.filtered_jobs_order,
                };
                if let Some(Pane::SessionList {
                    selected_index,
                    selected_session,
                    taskrabbit_selected_index,
                    archive_selected_index,
                    jobs_selected_index,
                    ..
                }) = app.tabs[app.active_tab].layout.find_pane_mut(focused)
                {
                    match current_zone {
                        crate::types::SessionListZone::Main => {
                            *selected_index = card_idx;
                        }
                        crate::types::SessionListZone::TaskRabbit => {
                            *taskrabbit_selected_index = card_idx;
                        }
                        crate::types::SessionListZone::Archive => {
                            *archive_selected_index = card_idx;
                        }
                        crate::types::SessionListZone::Jobs => {
                            *jobs_selected_index = card_idx;
                        }
                    }
                    *selected_session = order.get(card_idx).copied();
                }
            }
        }
    }
}

/// Test-only wrapper to call handle_input_mode from other modules.
#[cfg(test)]
pub async fn test_handle_input_mode(app: &mut crate::app::App, key: KeyEvent) {
    handle_input_mode(app, key).await;
}

/// Check if the mouse is hovering over the session list clone area.
/// If so, scroll the session list and return true. Otherwise return false
/// and let the caller handle default scroll behavior.
fn scroll_mouse_region_aware(app: &mut crate::app::App, col: u16, row: u16, is_up: bool) -> bool {
    // Only applies when viewing a session detail
    let tab = &app.tabs[app.active_tab];
    if !matches!(
        tab.layout.find_pane(tab.focused_pane),
        Some(crate::types::Pane::SessionDetail { .. })
    ) {
        return false;
    }

    // Check if mouse is over the session list clone area
    if let Some(list_area) = app.session_list_render.last_list_area {
        if row >= list_area.y
            && row < list_area.y + list_area.height
            && col >= list_area.x
            && col < list_area.x + list_area.width
        {
            const MOUSE_SCROLL_LINES: usize = 3;
            for _ in 0..MOUSE_SCROLL_LINES {
                if is_up {
                    app.nav_list_up();
                } else {
                    app.nav_list_down();
                }
            }
            return true;
        }
    }

    false
}

/// Handle Left/Right arrow keys for session list navigation.
/// Works in both SessionList and SessionDetail views.
/// Returns `true` if the key was consumed (focused pane is SessionList or
/// SessionDetail, input bar is NOT in insert mode, and key is Left or Right).
fn handle_session_list_nav_key(app: &mut crate::app::App, key: KeyEvent) -> bool {
    use crate::key_tables::{SESSION_LIST_NAV_KEYS, SessionListNavEffect, SessionListNavGuard};

    let Some(entry) = crate::key_tables::lookup(SESSION_LIST_NAV_KEYS, key, |guard| match guard {
        // Pane and insert-mode checks follow below.
        SessionListNavGuard::ListOrDetail => true,
    }) else {
        return false;
    };
    // Returns (is_left, should_enter_session).
    let (is_left, should_enter) = match entry.effect {
        SessionListNavEffect::Prev => (true, false),
        SessionListNavEffect::Next => (false, false),
        SessionListNavEffect::PrevAndOpen => (true, true),
        SessionListNavEffect::NextAndOpen => (false, true),
    };

    // Determine which pane is focused
    let tab = &app.tabs[app.active_tab];
    let pane = tab.layout.find_pane(tab.focused_pane);
    let is_session_list = matches!(pane, Some(crate::types::Pane::SessionList { .. }));
    let is_session_detail = matches!(pane, Some(crate::types::Pane::SessionDetail { .. }));

    if !is_session_list && !is_session_detail {
        return false;
    }

    // Only when input bar is NOT in insert mode
    if is_input_bar_insert(app) {
        return false;
    }

    // Navigate the session list
    if is_session_detail {
        // In detail view, use nav_list_up/down which finds the list pane
        if is_left {
            app.nav_list_up();
        } else {
            app.nav_list_down();
        }
    } else {
        // In list view, use regular nav_up/down
        if is_left {
            app.nav_up();
        } else {
            app.nav_down();
        }
    }

    // Shift+Left/Right: also open the newly selected session in detail view
    if should_enter {
        app.enter_session();
    }

    // Auto-expand sidebar when at minimum percentage width and navigating
    crate::action_handler::maybe_auto_expand_session_list(app);
    true
}

/// Handle plain Up/Down arrow keys for session detail scrolling.
/// Returns `true` if the key was consumed (focused pane is SessionDetail and key is Up/Down).
///
/// Scrolls 3 lines per keypress (matching mouse wheel). After scrolling,
/// performs viewport-aware selection tracking: if the current event cursor
/// would leave the viewport, it's clamped to the nearest visible edge event.
/// This differs from mouse wheel (which never updates selection) and
/// Shift+Up/Down (which jump to the event and snap scroll).
fn handle_detail_scroll_key(app: &mut crate::app::App, key: KeyEvent) -> bool {
    use crate::key_tables::{DETAIL_SCROLL_KEYS, DetailScrollEffect, DetailScrollGuard};

    // Only plain Up/Down (no modifiers); the focused-pane check follows.
    let Some(entry) = crate::key_tables::lookup(DETAIL_SCROLL_KEYS, key, |guard| match guard {
        DetailScrollGuard::DetailFocused => true,
    }) else {
        return false;
    };
    let direction: i32 = match entry.effect {
        DetailScrollEffect::ScrollUp => -1,
        DetailScrollEffect::ScrollDown => 1,
    };

    // Only when focused on a session detail pane
    let session_id = match app.focused_pane().cloned() {
        Some(crate::types::Pane::SessionDetail { session_id }) => session_id,
        _ => return false,
    };

    let Some(state) = app.sessions.get_mut(&session_id) else {
        return false;
    };

    const ARROW_SCROLL_LINES: usize = 3;

    // Capture whether we're disengaging follow_tail on this scroll.
    // When unlocking from the tail by scrolling up, we need to actively
    // move the selection rather than using lazy tracking — otherwise the
    // newest event (which can be very tall) stays selected for dozens of
    // keypresses while it remains partially visible.
    let was_follow_tail = state.follow_tail;

    // Clear follow_tail_hold — arrow scroll is a deliberate user action
    // that should allow follow_tail re-engage when scrolling back to bottom.
    state.follow_tail_hold = false;

    // Scroll
    for _ in 0..ARROW_SCROLL_LINES {
        if direction > 0 {
            state.scroll_offset = state.scroll_offset.saturating_add(1);
        } else {
            state.scroll_offset = state.scroll_offset.saturating_sub(1);
        }
    }
    state.follow_tail = false;

    // Clamp scroll_offset to valid range now so the viewport-aware selection
    // tracking below sees the real bounded offset. Without this, scrolling
    // down at the bottom temporarily exceeds max_scroll, which makes
    // find_visible_events report the last (short) event as outside the
    // viewport and erroneously moves the cursor off the last message.
    let max_scroll = state
        .total_content_height
        .saturating_sub(state.last_viewport_height);
    if direction > 0 {
        state.scroll_offset = state.scroll_offset.min(max_scroll);
    }

    // Viewport-aware selection tracking:
    // If the current event cursor would leave the viewport, clamp it
    // to the nearest visible edge event.
    if let Some(current_idx) = state.current_event_index {
        let viewport = state.last_viewport_height;
        if viewport > 0
            && let Some((first_vis, last_vis)) = crate::ui::session::find_visible_events(
                &state.event_offsets,
                &state.event_heights,
                state.scroll_offset,
                viewport,
            )
        {
            if was_follow_tail && direction < 0 {
                // Just disengaged follow_tail by scrolling up: actively
                // set selection to first visible event so it "unsticks"
                // from the newest (potentially very tall) event.
                state.current_event_index = Some(first_vis);
            } else if current_idx < first_vis {
                // Selection scrolled above viewport → clamp to first visible
                state.current_event_index = Some(first_vis);
            } else if current_idx > last_vis {
                // Selection scrolled below viewport → clamp to last visible
                state.current_event_index = Some(last_vis);
            }
        }
    }

    true
}

/// Check if the focused pane is a SessionDetail with its input bar in Insert mode.
fn is_input_bar_insert(app: &crate::app::App) -> bool {
    let session_id = match app.focused_pane() {
        Some(crate::types::Pane::SessionDetail { session_id }) => *session_id,
        _ => return false,
    };
    app.sessions
        .get(&session_id)
        .is_some_and(|s| s.input_bar.surface.mode == crate::types::PopupMode::Insert)
}

/// Handle key events in Command mode (typing a : command).
async fn handle_command_mode(app: &mut crate::app::App, key: KeyEvent) {
    use crate::key_tables::{LineEditEffect, LineEditor};

    let Some(effect) = line_edit_effect(LineEditor::Command, key) else {
        return;
    };
    match effect {
        LineEditEffect::Submit => {
            let command = app.command_buffer.clone();
            crate::action_handler::dispatch_command(app, &command).await;
            app.command_buffer.clear();
            app.input_mode = crate::types::InputMode::Normal;
            reset_vim_command_mode(app);
        }
        LineEditEffect::Cancel => {
            app.command_buffer.clear();
            app.input_mode = crate::types::InputMode::Normal;
            reset_vim_command_mode(app);
        }
        LineEditEffect::DeleteBackward => {
            app.command_buffer.pop();
        }
        LineEditEffect::InsertChar => {
            if let Some(c) = crate::key_tables::Chord::typed_char(key) {
                app.command_buffer.push(c);
            }
        }
    }
}

/// Handle key events in Search mode (typing a / search query).
fn handle_search_mode(app: &mut crate::app::App, key: KeyEvent) {
    use crate::key_tables::{LineEditEffect, LineEditor};

    let Some(effect) = line_edit_effect(LineEditor::Search, key) else {
        return;
    };
    match effect {
        LineEditEffect::Submit => {
            // Confirm search — keep filter/position active, return to Normal
            app.input_mode = crate::types::InputMode::Normal;
            reset_vim_command_mode(app);
        }
        LineEditEffect::Cancel => {
            // Cancel search — clear everything, return to Normal
            app.search_query.clear();
            app.search_matches.clear();
            app.search_match_cursor = 0;
            app.clear_search_filter();
            app.input_mode = crate::types::InputMode::Normal;
            reset_vim_command_mode(app);
        }
        LineEditEffect::DeleteBackward => {
            app.search_query.pop();
            app.apply_search();
        }
        LineEditEffect::InsertChar => {
            if let Some(c) = crate::key_tables::Chord::typed_char(key) {
                app.search_query.push(c);
                app.apply_search();
            }
        }
    }
}

/// Feed an Escape into the VimMachine to exit its internal command mode,
/// draining any resulting actions so they don't leak into the next keypress.
pub(crate) fn reset_vim_command_mode(app: &mut crate::app::App) {
    use keybindings::BindingMachine;
    use modalkit::key::TerminalKey;

    let esc_key = TerminalKey::from(crate::key_tables::MODALKIT_RESET_KEY);
    app.key_manager.input_key(esc_key);
    while let Some((_action, _ctx)) = app.key_manager.pop() {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action_registry::{ActionAvailability, ActionContext, ActionId};
    use crate::settings_registry::SettingsSection;
    use crate::types::{
        FileViewerMouseLayout, FileViewerMouseRow, FileViewerState, OverlayState, Pane,
        SettingsFocus,
    };
    use ratatui::layout::Rect;
    use std::path::PathBuf;

    #[tokio::test]
    async fn colon_and_space_semicolon_open_the_same_palette_and_reset_command_mode() {
        use crossterm::event::{KeyCode, KeyModifiers};

        let mut app = crate::app::app_test_helpers::with_session_list(0);
        step_once(
            &mut app,
            KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE),
        )
        .await;
        let (colon_results, colon_origin) = match &app.overlay {
            OverlayState::CommandPalette {
                query,
                results,
                selected,
                origin,
                ..
            } => {
                assert!(query.is_empty());
                assert_eq!(*selected, 0);
                (results.clone(), *origin)
            }
            _ => panic!("colon opens palette"),
        };
        step_once(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)).await;
        assert!(matches!(app.overlay, OverlayState::None));
        step_once(
            &mut app,
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
        )
        .await;
        step_once(
            &mut app,
            KeyEvent::new(KeyCode::Char(';'), KeyModifiers::NONE),
        )
        .await;
        let OverlayState::CommandPalette {
            query,
            results,
            selected,
            origin,
            ..
        } = &app.overlay
        else {
            panic!("Space semicolon opens palette")
        };
        assert!(query.is_empty());
        assert_eq!(*selected, 0);
        assert_eq!(*results, colon_results);
        assert_eq!(*origin, colon_origin);
    }

    #[tokio::test]
    async fn colon_in_insert_input_stays_text() {
        use crossterm::event::{KeyCode, KeyModifiers};

        let mut app = crate::app::app_test_helpers::with_session_list(0);
        app.input_mode = crate::types::InputMode::Input;
        app.input_buffer = "prefix".to_string();
        step_once(
            &mut app,
            KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE),
        )
        .await;
        assert_eq!(app.input_buffer, "prefix:");
        assert!(matches!(app.overlay, OverlayState::None));
    }

    fn esp_overlay(started_at: Instant, deadline: Instant) -> OverlayState {
        OverlayState::EspSquare {
            round: 0,
            correct: 0,
            interactive: true,
            rounds: vec![None; 12],
            message: String::new(),
            flash: Some((4, true)),
            flash_deadline: Some(deadline),
            target: 4,
            round_details: vec![None; 12],
            cursor: None,
            last_guess: None,
            started_at,
        }
    }

    fn settings_app() -> crate::app::App {
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        let focused = app.active_tab().focused_pane;
        *app.active_tab_mut()
            .layout
            .find_pane_mut(focused)
            .expect("focused pane") = Pane::Settings;
        app
    }

    #[test]
    fn session_id_click_target_is_exactly_two_cells_in_metadata_row() {
        let (mut app, session_id) = crate::app::app_test_helpers::with_session_detail();
        let state = app
            .sessions
            .get_mut(&session_id)
            .expect("session detail fixture");
        state.last_content_area = Rect::new(10, 5, 100, 30);
        assert!(session_id_control_clicked(state, 11, 6));
        assert!(session_id_control_clicked(state, 12, 6));
        assert!(!session_id_control_clicked(state, 10, 6));
        assert!(!session_id_control_clicked(state, 13, 6));
        assert!(!session_id_control_clicked(state, 11, 5));
    }

    #[test]
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    fn right_click_reopen_reports_conflict_and_preserves_draft() {
        use rsi_common::types::{ConversationEvent, EventType, Role};
        let path =
            std::env::temp_dir().join(format!("rsi-right-click-{}.rs", uuid::Uuid::new_v4()));
        std::fs::write(&path, "original").unwrap();
        let (mut app, session_id) = crate::app::app_test_helpers::with_session_detail();
        let focused = app.active_tab().focused_pane;
        *app.active_tab_mut().layout.find_pane_mut(focused).unwrap() =
            Pane::SessionDetail { session_id };
        crate::file_viewer::open_path_in_session(&mut app, session_id, path.clone());
        let state = app.sessions.get_mut(&session_id).unwrap();
        let viewer = state.file_viewer.as_mut().unwrap();
        viewer
            .surface
            .textarea
            .move_cursor(tui_textarea::CursorMove::End);
        viewer.surface.textarea.insert_str(" draft");
        viewer.dirty = true;
        state.events = vec![ConversationEvent {
            id: 0,
            session_id,
            sequence: 1,
            event_type: EventType::Message,
            role: Some(Role::Assistant),
            content: format!("See `{}`", path.display()),
            tool_name: None,
            tool_input: None,
            created_at: chrono::Utc::now(),
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        }];
        state.events_generation += 1;
        state.last_render_width = 120;
        state.last_content_area = Rect::new(0, 0, 120, 30);
        state.last_content_y_offset = 3;
        crate::ui::height::update_event_heights(state, 120);
        let click = (3..29)
            .flat_map(|y| (2..118).map(move |x| (x, y)))
            .find(|&(x, y)| {
                detect_link_target_at(state, x, y)
                    .and_then(|(_, path, _)| path)
                    .is_some_and(|target| target.contains("rsi-right-click-"))
            })
            .expect("clickable file path in rendered message");
        std::fs::write(&path, "external").unwrap();

        handle_mouse_right_click(&mut app, click.0, click.1);

        let viewer = app
            .sessions
            .get(&session_id)
            .unwrap()
            .file_viewer
            .as_ref()
            .unwrap();
        assert_eq!(viewer.surface.content(), "original draft");
        assert_eq!(
            viewer.external_conflict.as_deref(),
            Some("external"),
            "focus={:?} path={:?} notice={:?}",
            app.focused_pane(),
            viewer.file_path,
            app.notifications.back()
        );
        let notice = app.notifications.back().unwrap();
        assert_eq!(notice.kind, crate::types::NotificationKind::OperationFailed);
        assert!(notice.message.contains("External change detected"));
        std::fs::remove_file(path).unwrap();
    }

    fn registered_request(app: &crate::app::App, key: KeyEvent) -> ActionAvailability {
        crate::action_registry::request_for_key(&ActionContext::from_app(app), key)
    }

    #[tokio::test]
    async fn config_pending_launch_refusal_preserves_the_input_draft() {
        use crate::types::{InputMode, InputPurpose};
        use crossterm::event::{KeyCode, KeyModifiers};

        let mut app = crate::app::app_test_helpers::with_session_list(0);
        app.poll.connected = true;
        app.input_mode = InputMode::Input;
        app.input_purpose = InputPurpose::NewSession;
        app.input_buffer = "preserve this launch draft".to_string();

        step_once(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)).await;

        assert_eq!(app.input_buffer, "preserve this launch draft");
        assert_eq!(app.input_mode, InputMode::Input);
        assert_eq!(
            app.notifications
                .back()
                .map(|notification| notification.message.as_str()),
            Some("Daemon configuration is still loading; draft preserved")
        );
    }

    #[tokio::test]
    async fn registered_continue_immediately_sends_literal_to_the_selected_row() {
        use crossterm::event::{KeyCode, KeyModifiers};
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixListener;

        let mut app = crate::app::app_test_helpers::with_session_list(1);
        let session_id = app.selected_session_id().expect("selected session");
        let socket_path = crate::test_support::short_socket_path("quick-continue");
        let listener = UnixListener::bind(&socket_path).expect("bind test daemon socket");
        app.client = crate::client::DaemonClient::new(socket_path.clone());
        app.client
            .connect()
            .await
            .expect("connect test daemon client");
        app.poll.connected = true;
        let continue_key = KeyEvent::new(KeyCode::Char('X'), KeyModifiers::NONE);
        let context = ActionContext::from_app(&app);
        assert!(
            matches!(
                crate::action_registry::request_for_key(&context, continue_key),
                ActionAvailability::Available(request) if request.id == ActionId::ContinueSession
            ),
            "continue context: {context:?}"
        );

        let rpc = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept quick continue");
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut line = String::new();
            reader
                .read_line(&mut line)
                .await
                .expect("read quick continue");
            let request: serde_json::Value =
                serde_json::from_str(&line).expect("decode quick continue request");
            writer
                .write_all(
                    format!(
                        "{}\n",
                        serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": request["id"],
                            "result": null,
                        })
                    )
                    .as_bytes(),
                )
                .await
                .expect("ack quick continue");
            request
        });

        step_once(&mut app, continue_key).await;
        let request = rpc.await.expect("quick continue daemon task");
        std::fs::remove_file(&socket_path).expect("remove test daemon socket");

        assert!(matches!(app.overlay, OverlayState::None));
        assert_eq!(
            app.sessions
                .get(&session_id)
                .map(|state| state.session.status),
            Some(rsi_common::types::SessionStatus::Starting)
        );
        assert_eq!(request["method"], "ContinueSession");
        assert_eq!(request["params"]["session_id"], session_id.to_string());
        assert_eq!(request["params"]["query"], "continue");
    }

    #[tokio::test]
    async fn issue_workspace_result_applies_while_help_is_open() {
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        let project_id = uuid::Uuid::new_v4();
        app.tabs[app.active_tab].project_id = Some(project_id);
        let pane_id = app.open_or_focus_issues();
        if let Some(crate::types::Pane::Issues(state)) =
            app.active_tab_mut().layout.find_pane_mut(pane_id)
        {
            state.transient.in_flight.clear();
            state
                .transient
                .generations
                .insert(crate::types::IssueWorkspaceDataKind::SyncStatus, 1);
            state.active_tab = crate::types::IssueWorkspaceTab::Sync;
        }
        crate::overlay::keybindings_help::open_contextual_help(&mut app);
        assert!(matches!(app.overlay, OverlayState::KeybindingsHelp { .. }));
        app.apply_issue_workspace_event(crate::app::issues::IssueWorkspaceAsyncEvent {
            pane_id,
            project_id: Some(project_id),
            data_kind: crate::types::IssueWorkspaceDataKind::SyncStatus,
            generation: 1,
            request_identity: crate::types::IssueWorkspaceRequestIdentity::SyncStatus,
            outcome: Ok(crate::app::issues::IssueWorkspaceAsyncPayload::SyncStatus(
                rsi_common::issue_workspace::IssueTrackerStatusV1 {
                    enabled: true,
                    tracker: "fixture".to_string(),
                    last_poll_at: None,
                    next_poll_at: None,
                    dispatched_count: 1,
                    max_concurrent: 2,
                    poll_interval_ms: 30_000,
                    active_states: vec!["started".to_string()],
                },
            )),
        });
        crate::overlay::keybindings_help::close_keybindings_help(&mut app);
        assert!(matches!(
            app.active_tab().layout.find_pane(pane_id),
            Some(crate::types::Pane::Issues(state))
                if state.transient.sync_status.as_ref().map(|status| status.tracker.as_str()) == Some("fixture")
        ));
    }

    #[tokio::test]
    async fn jk_crosses_managers_to_needs_you() {
        use crate::app::app_test_helpers::{baseline_session, with_session_list};
        use crate::app::manager_roster::{ManagerRosterEntry, ManagerTier};
        use crossterm::event::{KeyCode, KeyModifiers};
        use rsi_common::types::{SessionKind, SessionStatus};

        let mut app = with_session_list(0);
        let manager_id = uuid::Uuid::new_v4();
        let worker_id = uuid::Uuid::new_v4();
        let manager = baseline_session(manager_id, SessionKind::Standard);
        let mut worker = baseline_session(worker_id, SessionKind::Standard);
        worker.status = SessionStatus::WaitingApproval;
        app.update_sessions(vec![worker, manager]);
        app.manager_roster.by_project.insert(
            uuid::Uuid::new_v4(),
            ManagerRosterEntry {
                session_id: manager_id,
                tier: ManagerTier::Project,
            },
        );
        app.sort_sessions(false);
        app.reset_selection_to_first();
        assert_eq!(app.selected_session_id(), Some(manager_id));

        step_once(
            &mut app,
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
        )
        .await;
        assert_eq!(app.selected_session_id(), Some(worker_id));
        step_once(
            &mut app,
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
        )
        .await;
        assert_eq!(app.selected_session_id(), Some(manager_id));
    }

    #[tokio::test]
    async fn raw_settings_keys_dispatch_the_available_category_and_editable_item_requests() {
        use crossterm::event::{KeyCode, KeyModifiers};

        let mut app = settings_app();
        let down = KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE);
        assert!(matches!(
            registered_request(&app, down),
            ActionAvailability::Available(request) if request.id == ActionId::MoveDown
        ));
        step_once(&mut app, down).await;
        assert_eq!(app.settings_state.section, SettingsSection::Screen);

        let open = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(
            registered_request(&app, open),
            ActionAvailability::Available(request) if request.id == ActionId::Open
        ));
        step_once(&mut app, open).await;
        assert_eq!(app.settings_state.focus, SettingsFocus::Items);

        app.settings_state.section = SettingsSection::Screen;
        app.settings_state.selected_index = 0;
        let toggle = KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE);
        assert!(matches!(
            registered_request(&app, toggle),
            ActionAvailability::Available(request) if request.id == ActionId::ToggleSetting
        ));
        let before = app.settings.text_area_backfill_enabled;
        step_once(&mut app, toggle).await;
        assert_eq!(app.settings.text_area_backfill_enabled, !before);
    }

    #[tokio::test]
    async fn raw_settings_keys_report_registry_reasons_for_empty_and_read_only_rows() {
        use crossterm::event::{KeyCode, KeyModifiers};

        let mut app = settings_app();
        app.settings_state.focus = SettingsFocus::Items;
        app.settings_state.section = SettingsSection::ClaudeSkills;
        app.settings_state.selected_index = 0;
        app.cached_user_skills = Some(Vec::new());
        let open = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let empty_reason = match registered_request(&app, open) {
            ActionAvailability::Unavailable { reason } => reason,
            available => panic!("empty row unexpectedly available: {available:?}"),
        };
        step_once(&mut app, open).await;
        assert_eq!(
            app.notifications
                .back()
                .map(|notification| notification.message.as_str()),
            Some(empty_reason)
        );
        assert!(app.pending_lc_actions.is_empty());

        app.notifications.clear();
        app.settings_state.section = SettingsSection::Usage;
        app.settings_state.selected_index = 0;
        let toggle = KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE);
        let read_only_reason = match registered_request(&app, toggle) {
            ActionAvailability::Unavailable { reason } => reason,
            available => panic!("read-only row unexpectedly available: {available:?}"),
        };
        step_once(&mut app, toggle).await;
        assert_eq!(
            app.notifications
                .back()
                .map(|notification| notification.message.as_str()),
            Some(read_only_reason)
        );
        assert!(app.pending_lc_actions.is_empty());
    }

    #[tokio::test]
    async fn settings_registry_adapter_leaves_search_and_command_text_owned() {
        use crossterm::event::{KeyCode, KeyModifiers};

        let mut app = settings_app();
        let question = KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE);

        app.input_mode = crate::types::InputMode::Search;
        step_once(&mut app, question).await;
        assert_eq!(app.search_query, "?");
        assert!(matches!(app.overlay, OverlayState::None));

        app.input_mode = crate::types::InputMode::Command;
        step_once(&mut app, question).await;
        assert_eq!(app.command_buffer, "?");
        assert!(matches!(app.overlay, OverlayState::None));
    }

    #[tokio::test]
    async fn explicit_help_chord_opens_from_text_mode_without_changing_the_draft() {
        use crossterm::event::{KeyCode, KeyModifiers};

        let mut app = settings_app();
        app.input_mode = crate::types::InputMode::Command;
        app.command_buffer = "draft command".to_string();
        step_once(
            &mut app,
            KeyEvent::new(
                KeyCode::Char('g'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            ),
        )
        .await;
        assert!(matches!(app.overlay, OverlayState::KeybindingsHelp { .. }));
        assert_eq!(app.command_buffer, "draft command");
        crate::overlay::close_keybindings_help(&mut app);
        assert_eq!(app.command_buffer, "draft command");
        assert_eq!(app.input_mode, crate::types::InputMode::Command);
    }

    #[tokio::test]
    async fn explicit_help_chord_restores_an_overlay_text_draft() {
        use crossterm::event::{KeyCode, KeyModifiers};

        let mut app = settings_app();
        app.overlay = OverlayState::ThemeRoleEditor {
            role: crate::ui::theme_roles::ThemeRole::Accent,
            input: "#123456".to_string(),
            opening_overrides: Vec::new(),
            assessment: None,
            committed: false,
            pending_acknowledgement: None,
        };
        step_once(
            &mut app,
            KeyEvent::new(
                KeyCode::Char('g'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            ),
        )
        .await;
        assert!(matches!(
            app.overlay,
            OverlayState::KeybindingsHelp {
                origin: crate::action_registry::HelpOrigin::ThemeRoleEditor,
                ..
            }
        ));
        crate::overlay::close_keybindings_help(&mut app);
        assert!(matches!(
            &app.overlay,
            OverlayState::ThemeRoleEditor { input, committed: false, .. }
                if input == "#123456"
        ));
    }

    #[test]
    fn one_line_navigator_rows_are_mouse_hittable_while_headers_are_not() {
        let zone = crate::types::ZoneRenderState {
            card_offsets: vec![1, 3],
            card_heights: vec![1, 1],
            total_content_height: 4,
            ..Default::default()
        };

        assert_eq!(session_card_index_at(&zone, 0), None);
        assert_eq!(session_card_index_at(&zone, 1), Some(0));
        assert_eq!(session_card_index_at(&zone, 2), None);
        assert_eq!(session_card_index_at(&zone, 3), Some(1));
    }

    /// Regression pin for the Ctrl+Left / Ctrl+Right zone-cycle removal.
    ///
    /// These chords used to cycle the session list zone (Main -> TaskRabbit ->
    /// Jobs -> Archive) from any pane. They now move pane focus by direction,
    /// matching the `Ctrl+h` / `Ctrl+l` focus fallback. Zone jumps remain the
    /// explicit `gs` / `gt` / `gj` / `ga` chords.
    #[tokio::test]
    async fn ctrl_arrows_move_focus_without_cycling_the_zone() {
        use crate::types::SplitDirection;
        use crossterm::event::{KeyCode, KeyModifiers};

        // (key, start leaf index, expected leaf index, label)
        for (code, start, expected, label) in [
            (KeyCode::Left, 1, 0, "Ctrl+Left"),
            (KeyCode::Right, 0, 1, "Ctrl+Right"),
        ] {
            let mut app = crate::app::app_test_helpers::with_session_list(1);
            app.split_focused(SplitDirection::Vertical);
            let leaf_ids = app.active_tab().layout.leaf_ids();
            assert_eq!(leaf_ids.len(), 2, "{label} needs a two-pane layout");

            app.active_tab_mut().focused_pane = leaf_ids[start];

            step_once(&mut app, KeyEvent::new(code, KeyModifiers::CONTROL)).await;

            assert_eq!(
                app.active_tab().focused_pane,
                leaf_ids[expected],
                "{label} must move focus to the neighboring pane"
            );

            let zone = match app.session_list_pane_mut() {
                crate::types::Pane::SessionList { active_zone, .. } => *active_zone,
                other => panic!("expected a session list pane, found {other:?}"),
            };
            assert_eq!(
                zone,
                crate::types::SessionListZone::Main,
                "{label} must not cycle the session list zone"
            );
        }
    }

    #[test]
    fn selected_operational_row_remains_one_cell_high() {
        let zone = crate::types::ZoneRenderState {
            card_offsets: vec![1, 3],
            card_heights: vec![1, 1],
            total_content_height: 4,
            ..Default::default()
        };

        assert_eq!(session_card_index_at(&zone, 1), Some(0));
        assert_eq!(session_card_index_at(&zone, 2), None);
        assert_eq!(session_card_index_at(&zone, 3), Some(1));
    }

    #[test]
    fn file_viewer_mouse_click_and_wheel_are_routed_before_session_navigation() {
        let mut app = crate::app::app_test_helpers::with_session_list(1);
        let session_id = app.filtered_session_order[0];
        let mut viewer = FileViewerState::new(
            PathBuf::from("/tmp/mouse.txt"),
            (0..8)
                .map(|line| format!("line {line}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        viewer.mouse_layout = FileViewerMouseLayout {
            editor_area: Some(Rect::new(5, 5, 50, 6)),
            content_area: Some(Rect::new(10, 5, 45, 6)),
            rows: (0..6)
                .map(|line| FileViewerMouseRow {
                    line,
                    char_start: 0,
                    char_end: 6,
                })
                .collect(),
        };
        app.sessions
            .get_mut(&session_id)
            .expect("fixture session should exist")
            .file_viewer = Some(viewer);

        assert!(handle_file_viewer_mouse_click(&mut app, 13, 6));
        let viewer = app.sessions[&session_id]
            .file_viewer
            .as_ref()
            .expect("viewer should remain open");
        assert_eq!(viewer.surface.textarea.cursor(), (1, 3));

        assert!(handle_file_viewer_mouse_scroll(&mut app, 13, 6, false, 3));
        let viewer = app.sessions[&session_id]
            .file_viewer
            .as_ref()
            .expect("viewer should remain open");
        assert_eq!(viewer.surface.textarea.cursor(), (4, 3));
    }

    #[test]
    fn centered_browser_gutters_signal_and_inspector_are_not_clickable() {
        use crate::types::SplitNode;
        use crate::ui::session::{SessionListSurface, render_session_list};
        use ratatui::{Terminal, backend::TestBackend, layout::Rect};

        let mut app = crate::app::app_test_helpers::with_session_list(8);
        let pane = match &app.tabs[0].layout {
            SplitNode::Leaf { pane, .. } => pane.clone(),
            _ => unreachable!("fixture is one pane"),
        };
        let backend = TestBackend::new(200, 32);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_session_list(
                    frame,
                    Rect::new(0, 0, 200, 32),
                    &pane,
                    true,
                    &mut app,
                    SessionListSurface::Full,
                    None,
                );
            })
            .unwrap();
        let cards = app.session_list_render.cards_area.unwrap();
        assert_eq!(cards.x, 13);

        handle_session_list_click(&mut app, 0, cards.y);
        handle_session_list_click(&mut app, 150, cards.y);
        let selected = app.tabs[0]
            .layout
            .find_pane(app.tabs[0].focused_pane)
            .and_then(|pane| match pane {
                crate::types::Pane::SessionList { selected_index, .. } => Some(*selected_index),
                _ => None,
            })
            .unwrap();
        assert_eq!(selected, 0);

        let second_offset = app.session_list_render.main.card_offsets[1];
        let second_y = cards.y
            + second_offset.saturating_sub(app.session_list_render.main.scroll_offset) as u16;
        handle_session_list_click(&mut app, cards.x + 2, second_y);
        let selected = app.tabs[0]
            .layout
            .find_pane(app.tabs[0].focused_pane)
            .and_then(|pane| match pane {
                crate::types::Pane::SessionList { selected_index, .. } => Some(*selected_index),
                _ => None,
            })
            .unwrap();
        assert_eq!(selected, 1);

        let mut narrow = crate::app::app_test_helpers::with_session_list(8);
        let pane = match &narrow.tabs[0].layout {
            SplitNode::Leaf { pane, .. } => pane.clone(),
            _ => unreachable!("fixture is one pane"),
        };
        let backend = TestBackend::new(120, 32);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_session_list(
                    frame,
                    Rect::new(0, 0, 120, 32),
                    &pane,
                    true,
                    &mut narrow,
                    SessionListSurface::Full,
                    None,
                );
            })
            .unwrap();
        handle_session_list_click(&mut narrow, 4, 1);
        let selected = narrow.tabs[0]
            .layout
            .find_pane(narrow.tabs[0].focused_pane)
            .and_then(|pane| match pane {
                crate::types::Pane::SessionList { selected_index, .. } => Some(*selected_index),
                _ => None,
            })
            .unwrap();
        assert_eq!(selected, 0, "selected signal is outside cards_area");
    }

    #[test]
    fn expire_esp_flash_clears_at_deadline() {
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        let started_at = Instant::now();
        let deadline = started_at + Duration::from_millis(300);
        app.overlay = esp_overlay(started_at, deadline);
        app.needs_redraw = false;

        expire_esp_flash(&mut app, deadline);

        match &app.overlay {
            OverlayState::EspSquare {
                flash,
                flash_deadline,
                ..
            } => {
                assert_eq!(*flash, None);
                assert_eq!(*flash_deadline, None);
            }
            _ => panic!("expected ESP Square overlay"),
        }
        assert!(app.needs_redraw);
    }

    #[test]
    fn expire_esp_flash_keeps_flash_before_deadline() {
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        let started_at = Instant::now();
        let deadline = started_at + Duration::from_millis(300);
        app.overlay = esp_overlay(started_at, deadline);
        app.needs_redraw = false;

        expire_esp_flash(&mut app, deadline - Duration::from_millis(1));

        match &app.overlay {
            OverlayState::EspSquare {
                flash,
                flash_deadline,
                ..
            } => {
                assert_eq!(*flash, Some((4, true)));
                assert_eq!(*flash_deadline, Some(deadline));
            }
            _ => panic!("expected ESP Square overlay"),
        }
        assert!(app.needs_redraw);
    }

    /// Epic M slice (a) T8c: behavior pin for the raw-key decoders, recorded
    /// on the pre-refactor `event.rs` and kept unmodified across the
    /// key-table refactor. Each row is (fixture, chord, observed effect
    /// delta); the delta lists every observable field that `step_once`
    /// changed (quit flag, mode, overlay, focused pane, per-pane list/detail
    /// position, jumplist cursor, overlay geometry, sidebar width, line
    /// buffers, notification count, embedded terminal presence).
    mod key_table_refactor_pin {
        use super::*;
        use crossterm::event::{KeyCode, KeyModifiers};
        use std::collections::BTreeMap;

        fn variant_name(debug: String) -> String {
            debug
                .split(|c: char| c == ' ' || c == '{' || c == '(')
                .next()
                .unwrap_or_default()
                .to_string()
        }

        fn observe(app: &crate::app::App) -> BTreeMap<&'static str, String> {
            let mut obs = BTreeMap::new();
            obs.insert("quit", app.quit.to_string());
            obs.insert("mode", format!("{:?}", app.input_mode));
            obs.insert(
                "overlay",
                match &app.overlay {
                    OverlayState::None => "None",
                    OverlayState::KeybindingsHelp { .. } => "KeybindingsHelp",
                    OverlayState::Terminal => "Terminal",
                    OverlayState::Prompt { .. } => "Prompt",
                    _ => "Other",
                }
                .to_string(),
            );
            obs.insert(
                "focus",
                app.focused_pane()
                    .map(|pane| variant_name(format!("{pane:?}")))
                    .unwrap_or_default(),
            );
            obs.insert("tabs", format!("{}/{}", app.tabs.len(), app.active_tab));
            let tab = app.active_tab();
            let mut panes = Vec::new();
            for id in tab.layout.leaf_ids() {
                match tab.layout.find_pane(id) {
                    Some(Pane::SessionList {
                        selected_index,
                        active_zone,
                        ..
                    }) => panes.push(format!("list[{active_zone:?}#{selected_index}]")),
                    Some(Pane::SessionDetail { session_id }) => {
                        let index = app
                            .filtered_session_order
                            .iter()
                            .position(|id| id == session_id);
                        let state = app.sessions.get(session_id);
                        panes.push(format!(
                            "detail[{index:?} scroll={:?} cursor={:?} tail={:?}]",
                            state.map(|s| s.scroll_offset),
                            state.and_then(|s| s.current_event_index),
                            state.map(|s| s.follow_tail),
                        ));
                    }
                    Some(other) => panes.push(variant_name(format!("{other:?}"))),
                    None => {}
                }
            }
            obs.insert("panes", panes.join(","));
            obs.insert("sidebar", tab.session_list_width_pct.to_string());
            obs.insert("jump", app.jumplist_cursor.to_string());
            obs.insert("geom", format!("{:?}", app.current_overlay_geometry()));
            obs.insert("input", app.input_buffer.clone());
            obs.insert("command", app.command_buffer.clone());
            obs.insert("search", app.search_query.clone());
            obs.insert("notes", app.notifications.len().to_string());
            obs.insert("terminal", app.terminal.is_some().to_string());
            obs
        }

        fn delta(
            before: &BTreeMap<&'static str, String>,
            after: &BTreeMap<&'static str, String>,
        ) -> String {
            let changed: Vec<String> = after
                .iter()
                .filter(|(key, value)| before.get(*key) != Some(*value))
                .map(|(key, value)| format!("{key}={value}"))
                .collect();
            if changed.is_empty() {
                "-".to_string()
            } else {
                changed.join("; ")
            }
        }

        fn config_ready(app: &mut crate::app::App) {
            app.poll.connected = true;
            app.poll.authoritative_config_ready = true;
        }

        async fn fixture(name: &str) -> crate::app::App {
            use crate::app::app_test_helpers::{with_session_detail, with_session_list};
            let press = |code: KeyCode| KeyEvent::new(code, KeyModifiers::NONE);
            match name {
                "list" => {
                    let mut app = with_session_list(3);
                    config_ready(&mut app);
                    app
                }
                "detail" => {
                    let (mut app, _) = with_session_detail();
                    config_ready(&mut app);
                    app.enter_session();
                    app
                }
                "jumplist" => {
                    let mut app = with_session_list(3);
                    config_ready(&mut app);
                    let order = app.filtered_session_order.clone();
                    for id in &order {
                        app.push_jumplist(*id);
                    }
                    app.jumplist_cursor = 1;
                    app
                }
                "detail_insert" => {
                    let (mut app, session_id) = with_session_detail();
                    config_ready(&mut app);
                    app.enter_session();
                    if let Some(state) = app.sessions.get_mut(&session_id) {
                        state.input_bar.surface.mode = crate::types::PopupMode::Insert;
                    }
                    app
                }
                "prompt_overlay" | "prompt_config_pending" => {
                    let mut app = with_session_list(3);
                    config_ready(&mut app);
                    step_once(&mut app, press(KeyCode::Char(' '))).await;
                    step_once(&mut app, press(KeyCode::Char('m'))).await;
                    if name == "prompt_config_pending" {
                        app.poll.authoritative_config_ready = false;
                    }
                    app
                }
                "terminal" => {
                    let mut app = with_session_list(3);
                    config_ready(&mut app);
                    app.overlay = OverlayState::Terminal;
                    app
                }
                "stale_issue_editor" => {
                    let mut app = with_session_list(0);
                    config_ready(&mut app);
                    let pane_id = app.active_tab().focused_pane;
                    let mut state =
                        crate::types::IssueWorkspaceState::new(Some(uuid::Uuid::new_v4()));
                    state.transient.editor = Some(crate::types::IssueEditorState {
                        mode: crate::types::IssueEditorMode::Create,
                        title: "Draft".to_string(),
                        body: String::new(),
                        priority: None,
                        assignee: None,
                        labels: Vec::new(),
                        status: None,
                        dependency_issue_id: None,
                        dependency_direction:
                            rsi_common::issue_workspace::IssueDependencyDirectionV1::BlockedBy,
                        dependency_candidates: Vec::new(),
                        dependency_selected_row: 0,
                        dependency_text: String::new(),
                        filter_draft: None,
                        filter_saved_view: None,
                        filter_mine_assignee: None,
                        base_issue: None,
                        latest_issue: None,
                        stale_conflict: true,
                        active_field: 0,
                        dirty: true,
                        discard_armed: false,
                        retry_key: "pin".to_string(),
                        error: None,
                        submitted: false,
                    });
                    *app.active_tab_mut()
                        .layout
                        .find_pane_mut(pane_id)
                        .expect("focused pane") = Pane::Issues(state);
                    app
                }
                "quick_input" | "quick_input_config_pending" => {
                    let mut app = with_session_list(3);
                    if name == "quick_input" {
                        config_ready(&mut app);
                    }
                    app.input_mode = crate::types::InputMode::Input;
                    app.input_buffer = "ab".to_string();
                    app
                }
                "command" => {
                    let mut app = with_session_list(3);
                    config_ready(&mut app);
                    app.input_mode = crate::types::InputMode::Command;
                    app.command_buffer = "zz".to_string();
                    app
                }
                "search" => {
                    let mut app = with_session_list(3);
                    config_ready(&mut app);
                    app.input_mode = crate::types::InputMode::Search;
                    app.search_query = "Fix".to_string();
                    app
                }
                other => panic!("unknown fixture {other}"),
            }
        }

        fn chord(name: &str) -> KeyEvent {
            let ctrl = KeyModifiers::CONTROL;
            let shift = KeyModifiers::SHIFT;
            let (code, modifiers) = match name {
                "Ctrl-Alt-G" => (KeyCode::Char('g'), ctrl | KeyModifiers::ALT),
                "Ctrl-C" => (KeyCode::Char('c'), ctrl),
                "Ctrl-\\" => (KeyCode::Char('\\'), ctrl),
                "Ctrl-O" => (KeyCode::Char('o'), ctrl),
                "Ctrl-I" => (KeyCode::Char('i'), ctrl),
                "Ctrl-H" => (KeyCode::Char('h'), ctrl),
                "Ctrl-L" => (KeyCode::Char('l'), ctrl),
                "Ctrl-Shift-Up" => (KeyCode::Up, ctrl | shift),
                "Ctrl-Shift-Down" => (KeyCode::Down, ctrl | shift),
                "Ctrl-Shift-Left" => (KeyCode::Left, ctrl | shift),
                "Ctrl-Shift-Right" => (KeyCode::Right, ctrl | shift),
                "Ctrl-Up" => (KeyCode::Up, ctrl),
                "Ctrl-Down" => (KeyCode::Down, ctrl),
                "Ctrl-Left" => (KeyCode::Left, ctrl),
                "Ctrl-Right" => (KeyCode::Right, ctrl),
                "Ctrl-0" => (KeyCode::Char('0'), ctrl),
                "Shift-Up" => (KeyCode::Up, shift),
                "Shift-Down" => (KeyCode::Down, shift),
                "Left" => (KeyCode::Left, KeyModifiers::NONE),
                "Right" => (KeyCode::Right, KeyModifiers::NONE),
                "Shift-Left" => (KeyCode::Left, shift),
                "Shift-Right" => (KeyCode::Right, shift),
                "Ctrl-Tab" => (KeyCode::Tab, ctrl),
                "Ctrl-Shift-Tab" => (KeyCode::Tab, ctrl | shift),
                "Ctrl-Shift-BackTab" => (KeyCode::BackTab, ctrl | shift),
                "Up" => (KeyCode::Up, KeyModifiers::NONE),
                "Down" => (KeyCode::Down, KeyModifiers::NONE),
                "Esc" => (KeyCode::Esc, KeyModifiers::NONE),
                "Enter" => (KeyCode::Enter, KeyModifiers::NONE),
                "Backspace" => (KeyCode::Backspace, KeyModifiers::NONE),
                "x" => (KeyCode::Char('x'), KeyModifiers::NONE),
                "Ctrl-Enter" => (KeyCode::Enter, ctrl),
                "Ctrl-T" => (KeyCode::Char('t'), ctrl),
                "Ctrl-S" => (KeyCode::Char('s'), ctrl),
                other => panic!("unknown chord {other}"),
            };
            KeyEvent::new(code, modifiers)
        }

        const FIXTURES: &[&str] = &[
            "list",
            "detail",
            "jumplist",
            "detail_insert",
            "prompt_overlay",
            "prompt_config_pending",
            "terminal",
            "stale_issue_editor",
            "quick_input",
            "quick_input_config_pending",
            "command",
            "search",
        ];

        /// `Ctrl-\` is exercised only where it closes an open terminal: from
        /// any other context it spawns a real shell. Ctrl-V (clipboard I/O)
        /// is not pinned here; its decoder entry is covered by T8a/T8b.
        const CHORDS: &[&str] = &[
            "Ctrl-Alt-G",
            "Ctrl-C",
            "Ctrl-O",
            "Ctrl-I",
            "Ctrl-H",
            "Ctrl-L",
            "Ctrl-Shift-Up",
            "Ctrl-Shift-Down",
            "Ctrl-Shift-Left",
            "Ctrl-Shift-Right",
            "Ctrl-Up",
            "Ctrl-Down",
            "Ctrl-Left",
            "Ctrl-Right",
            "Ctrl-0",
            "Shift-Up",
            "Shift-Down",
            "Left",
            "Right",
            "Shift-Left",
            "Shift-Right",
            "Ctrl-Tab",
            "Ctrl-Shift-Tab",
            "Ctrl-Shift-BackTab",
            "Up",
            "Down",
            "Esc",
            "Enter",
            "Backspace",
            "x",
            "Ctrl-Enter",
            "Ctrl-T",
            "Ctrl-S",
        ];

        async fn observed_rows() -> Vec<(String, String, String)> {
            let mut rows = Vec::new();
            for fixture_name in FIXTURES {
                let mut chords: Vec<&str> = CHORDS.to_vec();
                if *fixture_name == "terminal" {
                    chords.push("Ctrl-\\");
                }
                for chord_name in chords {
                    let mut app = fixture(fixture_name).await;
                    // Geometry persists through the shared test state file;
                    // start every case from the default geometry.
                    app.modal_geometries.clear();
                    let before = observe(&app);
                    step_once(&mut app, chord(chord_name)).await;
                    let after = observe(&app);
                    rows.push((
                        fixture_name.to_string(),
                        chord_name.to_string(),
                        delta(&before, &after),
                    ));
                }
            }
            rows
        }

        #[tokio::test]
        async fn key_table_refactor_preserves_behavior() {
            let observed = observed_rows().await;
            if std::env::var_os("RSI_PRINT_T8C").is_some() {
                for (fixture, chord, effect) in &observed {
                    println!("T8C\t{fixture}\t{chord}\t{effect}");
                }
            }
            let expected: Vec<(String, String, String)> = PINNED
                .iter()
                .map(|(fixture, chord, effect)| {
                    (fixture.to_string(), chord.to_string(), effect.to_string())
                })
                .collect();
            assert_eq!(observed.len(), expected.len());
            for (observed, expected) in observed.iter().zip(expected.iter()) {
                assert_eq!(observed, expected);
            }
        }

        /// Recorded on the pre-refactor decoders (`RSI_PRINT_T8C=1`). Never
        /// edit an existing row; the refactor must reproduce every one.
        #[rustfmt::skip]
        const PINNED: &[(&str, &str, &str)] = &[
            ("list", "Ctrl-Alt-G", "overlay=KeybindingsHelp"),
            ("list", "Ctrl-C", "quit=true"),
            ("list", "Ctrl-O", "-"),
            ("list", "Ctrl-I", "-"),
            ("list", "Ctrl-H", "notes=1; panes=list[Archive#0]"),
            ("list", "Ctrl-L", "panes=list[TaskRabbit#0]"),
            ("list", "Ctrl-Shift-Up", "-"),
            ("list", "Ctrl-Shift-Down", "-"),
            ("list", "Ctrl-Shift-Left", "sidebar=38"),
            ("list", "Ctrl-Shift-Right", "sidebar=44"),
            ("list", "Ctrl-Up", "-"),
            ("list", "Ctrl-Down", "-"),
            ("list", "Ctrl-Left", "-"),
            ("list", "Ctrl-Right", "-"),
            ("list", "Ctrl-0", "-"),
            ("list", "Shift-Up", "-"),
            ("list", "Shift-Down", "-"),
            ("list", "Left", "-"),
            ("list", "Right", "panes=list[Main#1]"),
            ("list", "Shift-Left", "focus=SessionDetail; panes=detail[Some(0) scroll=Some(0) cursor=None tail=Some(true)]"),
            ("list", "Shift-Right", "focus=SessionDetail; panes=detail[Some(1) scroll=Some(0) cursor=None tail=Some(true)]"),
            ("list", "Ctrl-Tab", "-"),
            ("list", "Ctrl-Shift-Tab", "panes=list[Main#1]"),
            ("list", "Ctrl-Shift-BackTab", "panes=list[Main#1]"),
            ("list", "Up", "-"),
            ("list", "Down", "panes=list[Main#1]"),
            ("list", "Esc", "-"),
            ("list", "Enter", "focus=SessionDetail; panes=detail[Some(0) scroll=Some(0) cursor=None tail=Some(true)]"),
            ("list", "Backspace", "-"),
            ("list", "x", "notes=1"),
            ("list", "Ctrl-Enter", "-"),
            ("list", "Ctrl-T", "-"),
            ("list", "Ctrl-S", "-"),
            ("detail", "Ctrl-Alt-G", "overlay=KeybindingsHelp"),
            ("detail", "Ctrl-C", "quit=true"),
            ("detail", "Ctrl-O", "-"),
            ("detail", "Ctrl-I", "-"),
            ("detail", "Ctrl-H", "-"),
            ("detail", "Ctrl-L", "-"),
            ("detail", "Ctrl-Shift-Up", "panes=detail[Some(0) scroll=Some(0) cursor=Some(0) tail=Some(false)]"),
            ("detail", "Ctrl-Shift-Down", "panes=detail[Some(0) scroll=Some(0) cursor=Some(0) tail=Some(false)]"),
            ("detail", "Ctrl-Shift-Left", "sidebar=38"),
            ("detail", "Ctrl-Shift-Right", "sidebar=44"),
            ("detail", "Ctrl-Up", "-"),
            ("detail", "Ctrl-Down", "-"),
            ("detail", "Ctrl-Left", "-"),
            ("detail", "Ctrl-Right", "-"),
            ("detail", "Ctrl-0", "-"),
            ("detail", "Shift-Up", "panes=detail[Some(0) scroll=Some(0) cursor=Some(0) tail=Some(false)]"),
            ("detail", "Shift-Down", "panes=detail[Some(0) scroll=Some(0) cursor=Some(0) tail=Some(false)]"),
            ("detail", "Left", "-"),
            ("detail", "Right", "-"),
            ("detail", "Shift-Left", "-"),
            ("detail", "Shift-Right", "-"),
            ("detail", "Ctrl-Tab", "-"),
            ("detail", "Ctrl-Shift-Tab", "-"),
            ("detail", "Ctrl-Shift-BackTab", "-"),
            ("detail", "Up", "panes=detail[Some(0) scroll=Some(0) cursor=None tail=Some(false)]"),
            ("detail", "Down", "panes=detail[Some(0) scroll=Some(3) cursor=None tail=Some(false)]"),
            ("detail", "Esc", "-"),
            ("detail", "Enter", "notes=1"),
            ("detail", "Backspace", "focus=SessionList; panes=list[Main#0]"),
            ("detail", "x", "notes=1"),
            ("detail", "Ctrl-Enter", "-"),
            ("detail", "Ctrl-T", "-"),
            ("detail", "Ctrl-S", "-"),
            ("jumplist", "Ctrl-Alt-G", "overlay=KeybindingsHelp"),
            ("jumplist", "Ctrl-C", "quit=true"),
            ("jumplist", "Ctrl-O", "focus=SessionDetail; panes=detail[Some(1) scroll=Some(0) cursor=None tail=Some(true)]"),
            ("jumplist", "Ctrl-I", "focus=SessionDetail; jump=2; panes=detail[Some(2) scroll=Some(0) cursor=None tail=Some(true)]"),
            ("jumplist", "Ctrl-H", "notes=1; panes=list[Archive#0]"),
            ("jumplist", "Ctrl-L", "panes=list[TaskRabbit#0]"),
            ("jumplist", "Ctrl-Shift-Up", "-"),
            ("jumplist", "Ctrl-Shift-Down", "-"),
            ("jumplist", "Ctrl-Shift-Left", "sidebar=38"),
            ("jumplist", "Ctrl-Shift-Right", "sidebar=44"),
            ("jumplist", "Ctrl-Up", "-"),
            ("jumplist", "Ctrl-Down", "-"),
            ("jumplist", "Ctrl-Left", "-"),
            ("jumplist", "Ctrl-Right", "-"),
            ("jumplist", "Ctrl-0", "-"),
            ("jumplist", "Shift-Up", "-"),
            ("jumplist", "Shift-Down", "-"),
            ("jumplist", "Left", "-"),
            ("jumplist", "Right", "panes=list[Main#1]"),
            ("jumplist", "Shift-Left", "focus=SessionDetail; jump=2; panes=detail[Some(0) scroll=Some(0) cursor=None tail=Some(true)]"),
            ("jumplist", "Shift-Right", "focus=SessionDetail; panes=detail[Some(1) scroll=Some(0) cursor=None tail=Some(true)]"),
            ("jumplist", "Ctrl-Tab", "-"),
            ("jumplist", "Ctrl-Shift-Tab", "panes=list[Main#1]"),
            ("jumplist", "Ctrl-Shift-BackTab", "panes=list[Main#1]"),
            ("jumplist", "Up", "-"),
            ("jumplist", "Down", "panes=list[Main#1]"),
            ("jumplist", "Esc", "-"),
            ("jumplist", "Enter", "focus=SessionDetail; jump=2; panes=detail[Some(0) scroll=Some(0) cursor=None tail=Some(true)]"),
            ("jumplist", "Backspace", "-"),
            ("jumplist", "x", "notes=1"),
            ("jumplist", "Ctrl-Enter", "-"),
            ("jumplist", "Ctrl-T", "-"),
            ("jumplist", "Ctrl-S", "-"),
            ("detail_insert", "Ctrl-Alt-G", "overlay=KeybindingsHelp"),
            ("detail_insert", "Ctrl-C", "quit=true"),
            ("detail_insert", "Ctrl-O", "-"),
            ("detail_insert", "Ctrl-I", "-"),
            ("detail_insert", "Ctrl-H", "-"),
            ("detail_insert", "Ctrl-L", "-"),
            ("detail_insert", "Ctrl-Shift-Up", "-"),
            ("detail_insert", "Ctrl-Shift-Down", "-"),
            ("detail_insert", "Ctrl-Shift-Left", "sidebar=38"),
            ("detail_insert", "Ctrl-Shift-Right", "sidebar=44"),
            ("detail_insert", "Ctrl-Up", "-"),
            ("detail_insert", "Ctrl-Down", "-"),
            ("detail_insert", "Ctrl-Left", "-"),
            ("detail_insert", "Ctrl-Right", "-"),
            ("detail_insert", "Ctrl-0", "-"),
            ("detail_insert", "Shift-Up", "-"),
            ("detail_insert", "Shift-Down", "-"),
            ("detail_insert", "Left", "-"),
            ("detail_insert", "Right", "-"),
            ("detail_insert", "Shift-Left", "-"),
            ("detail_insert", "Shift-Right", "-"),
            ("detail_insert", "Ctrl-Tab", "-"),
            ("detail_insert", "Ctrl-Shift-Tab", "-"),
            ("detail_insert", "Ctrl-Shift-BackTab", "-"),
            ("detail_insert", "Up", "panes=detail[Some(0) scroll=Some(0) cursor=None tail=Some(false)]"),
            ("detail_insert", "Down", "panes=detail[Some(0) scroll=Some(3) cursor=None tail=Some(false)]"),
            ("detail_insert", "Esc", "-"),
            ("detail_insert", "Enter", "-"),
            ("detail_insert", "Backspace", "-"),
            ("detail_insert", "x", "-"),
            ("detail_insert", "Ctrl-Enter", "-"),
            ("detail_insert", "Ctrl-T", "-"),
            ("detail_insert", "Ctrl-S", "-"),
            ("prompt_overlay", "Ctrl-Alt-G", "overlay=KeybindingsHelp"),
            ("prompt_overlay", "Ctrl-C", "quit=true"),
            ("prompt_overlay", "Ctrl-O", "-"),
            ("prompt_overlay", "Ctrl-I", "-"),
            ("prompt_overlay", "Ctrl-H", "-"),
            ("prompt_overlay", "Ctrl-L", "-"),
            ("prompt_overlay", "Ctrl-Shift-Up", "geom=ModalGeometry { dx: 0, dy: 0, dw: 0, dh: -2 }"),
            ("prompt_overlay", "Ctrl-Shift-Down", "geom=ModalGeometry { dx: 0, dy: 0, dw: 0, dh: 2 }"),
            ("prompt_overlay", "Ctrl-Shift-Left", "geom=ModalGeometry { dx: 0, dy: 0, dw: -2, dh: 0 }"),
            ("prompt_overlay", "Ctrl-Shift-Right", "geom=ModalGeometry { dx: 0, dy: 0, dw: 2, dh: 0 }"),
            ("prompt_overlay", "Ctrl-Up", "geom=ModalGeometry { dx: 0, dy: -2, dw: 0, dh: 0 }"),
            ("prompt_overlay", "Ctrl-Down", "geom=ModalGeometry { dx: 0, dy: 2, dw: 0, dh: 0 }"),
            ("prompt_overlay", "Ctrl-Left", "geom=ModalGeometry { dx: -2, dy: 0, dw: 0, dh: 0 }"),
            ("prompt_overlay", "Ctrl-Right", "geom=ModalGeometry { dx: 2, dy: 0, dw: 0, dh: 0 }"),
            ("prompt_overlay", "Ctrl-0", "-"),
            ("prompt_overlay", "Shift-Up", "-"),
            ("prompt_overlay", "Shift-Down", "-"),
            ("prompt_overlay", "Left", "-"),
            ("prompt_overlay", "Right", "-"),
            ("prompt_overlay", "Shift-Left", "-"),
            ("prompt_overlay", "Shift-Right", "-"),
            ("prompt_overlay", "Ctrl-Tab", "-"),
            ("prompt_overlay", "Ctrl-Shift-Tab", "-"),
            ("prompt_overlay", "Ctrl-Shift-BackTab", "-"),
            ("prompt_overlay", "Up", "-"),
            ("prompt_overlay", "Down", "-"),
            ("prompt_overlay", "Esc", "-"),
            ("prompt_overlay", "Enter", "-"),
            ("prompt_overlay", "Backspace", "-"),
            ("prompt_overlay", "x", "-"),
            ("prompt_overlay", "Ctrl-Enter", "-"),
            ("prompt_overlay", "Ctrl-T", "-"),
            ("prompt_overlay", "Ctrl-S", "-"),
            ("prompt_config_pending", "Ctrl-Alt-G", "overlay=KeybindingsHelp"),
            ("prompt_config_pending", "Ctrl-C", "quit=true"),
            ("prompt_config_pending", "Ctrl-O", "-"),
            ("prompt_config_pending", "Ctrl-I", "-"),
            ("prompt_config_pending", "Ctrl-H", "-"),
            ("prompt_config_pending", "Ctrl-L", "-"),
            ("prompt_config_pending", "Ctrl-Shift-Up", "geom=ModalGeometry { dx: 0, dy: 0, dw: 0, dh: -2 }"),
            ("prompt_config_pending", "Ctrl-Shift-Down", "geom=ModalGeometry { dx: 0, dy: 0, dw: 0, dh: 2 }"),
            ("prompt_config_pending", "Ctrl-Shift-Left", "geom=ModalGeometry { dx: 0, dy: 0, dw: -2, dh: 0 }"),
            ("prompt_config_pending", "Ctrl-Shift-Right", "geom=ModalGeometry { dx: 0, dy: 0, dw: 2, dh: 0 }"),
            ("prompt_config_pending", "Ctrl-Up", "geom=ModalGeometry { dx: 0, dy: -2, dw: 0, dh: 0 }"),
            ("prompt_config_pending", "Ctrl-Down", "geom=ModalGeometry { dx: 0, dy: 2, dw: 0, dh: 0 }"),
            ("prompt_config_pending", "Ctrl-Left", "geom=ModalGeometry { dx: -2, dy: 0, dw: 0, dh: 0 }"),
            ("prompt_config_pending", "Ctrl-Right", "geom=ModalGeometry { dx: 2, dy: 0, dw: 0, dh: 0 }"),
            ("prompt_config_pending", "Ctrl-0", "-"),
            ("prompt_config_pending", "Shift-Up", "-"),
            ("prompt_config_pending", "Shift-Down", "-"),
            ("prompt_config_pending", "Left", "-"),
            ("prompt_config_pending", "Right", "-"),
            ("prompt_config_pending", "Shift-Left", "-"),
            ("prompt_config_pending", "Shift-Right", "-"),
            ("prompt_config_pending", "Ctrl-Tab", "-"),
            ("prompt_config_pending", "Ctrl-Shift-Tab", "-"),
            ("prompt_config_pending", "Ctrl-Shift-BackTab", "-"),
            ("prompt_config_pending", "Up", "-"),
            ("prompt_config_pending", "Down", "-"),
            ("prompt_config_pending", "Esc", "-"),
            ("prompt_config_pending", "Enter", "-"),
            ("prompt_config_pending", "Backspace", "-"),
            ("prompt_config_pending", "x", "-"),
            ("prompt_config_pending", "Ctrl-Enter", "notes=1"),
            ("prompt_config_pending", "Ctrl-T", "notes=1"),
            ("prompt_config_pending", "Ctrl-S", "notes=1"),
            ("terminal", "Ctrl-Alt-G", "overlay=KeybindingsHelp"),
            ("terminal", "Ctrl-C", "-"),
            ("terminal", "Ctrl-O", "-"),
            ("terminal", "Ctrl-I", "-"),
            ("terminal", "Ctrl-H", "-"),
            ("terminal", "Ctrl-L", "-"),
            ("terminal", "Ctrl-Shift-Up", "-"),
            ("terminal", "Ctrl-Shift-Down", "-"),
            ("terminal", "Ctrl-Shift-Left", "-"),
            ("terminal", "Ctrl-Shift-Right", "-"),
            ("terminal", "Ctrl-Up", "-"),
            ("terminal", "Ctrl-Down", "-"),
            ("terminal", "Ctrl-Left", "-"),
            ("terminal", "Ctrl-Right", "-"),
            ("terminal", "Ctrl-0", "-"),
            ("terminal", "Shift-Up", "-"),
            ("terminal", "Shift-Down", "-"),
            ("terminal", "Left", "-"),
            ("terminal", "Right", "-"),
            ("terminal", "Shift-Left", "-"),
            ("terminal", "Shift-Right", "-"),
            ("terminal", "Ctrl-Tab", "-"),
            ("terminal", "Ctrl-Shift-Tab", "-"),
            ("terminal", "Ctrl-Shift-BackTab", "-"),
            ("terminal", "Up", "-"),
            ("terminal", "Down", "-"),
            ("terminal", "Esc", "-"),
            ("terminal", "Enter", "-"),
            ("terminal", "Backspace", "-"),
            ("terminal", "x", "-"),
            ("terminal", "Ctrl-Enter", "-"),
            ("terminal", "Ctrl-T", "-"),
            ("terminal", "Ctrl-S", "-"),
            ("terminal", "Ctrl-\\", "overlay=None"),
            ("stale_issue_editor", "Ctrl-Alt-G", "overlay=KeybindingsHelp"),
            ("stale_issue_editor", "Ctrl-C", "quit=true"),
            ("stale_issue_editor", "Ctrl-O", "-"),
            ("stale_issue_editor", "Ctrl-I", "-"),
            ("stale_issue_editor", "Ctrl-H", "-"),
            ("stale_issue_editor", "Ctrl-L", "-"),
            ("stale_issue_editor", "Ctrl-Shift-Up", "-"),
            ("stale_issue_editor", "Ctrl-Shift-Down", "-"),
            ("stale_issue_editor", "Ctrl-Shift-Left", "sidebar=38"),
            ("stale_issue_editor", "Ctrl-Shift-Right", "sidebar=44"),
            ("stale_issue_editor", "Ctrl-Up", "-"),
            ("stale_issue_editor", "Ctrl-Down", "-"),
            ("stale_issue_editor", "Ctrl-Left", "-"),
            ("stale_issue_editor", "Ctrl-Right", "-"),
            ("stale_issue_editor", "Ctrl-0", "-"),
            ("stale_issue_editor", "Shift-Up", "-"),
            ("stale_issue_editor", "Shift-Down", "-"),
            ("stale_issue_editor", "Left", "-"),
            ("stale_issue_editor", "Right", "-"),
            ("stale_issue_editor", "Shift-Left", "-"),
            ("stale_issue_editor", "Shift-Right", "-"),
            ("stale_issue_editor", "Ctrl-Tab", "-"),
            ("stale_issue_editor", "Ctrl-Shift-Tab", "-"),
            ("stale_issue_editor", "Ctrl-Shift-BackTab", "-"),
            ("stale_issue_editor", "Up", "notes=1"),
            ("stale_issue_editor", "Down", "notes=1"),
            ("stale_issue_editor", "Esc", "-"),
            ("stale_issue_editor", "Enter", "notes=1"),
            ("stale_issue_editor", "Backspace", "-"),
            ("stale_issue_editor", "x", "-"),
            ("stale_issue_editor", "Ctrl-Enter", "-"),
            ("stale_issue_editor", "Ctrl-T", "-"),
            ("stale_issue_editor", "Ctrl-S", "-"),
            ("quick_input", "Ctrl-Alt-G", "overlay=KeybindingsHelp"),
            ("quick_input", "Ctrl-C", "quit=true"),
            ("quick_input", "Ctrl-O", "input=abo"),
            ("quick_input", "Ctrl-I", "input=abi"),
            ("quick_input", "Ctrl-H", "input=abh"),
            ("quick_input", "Ctrl-L", "input=abl"),
            ("quick_input", "Ctrl-Shift-Up", "-"),
            ("quick_input", "Ctrl-Shift-Down", "-"),
            ("quick_input", "Ctrl-Shift-Left", "-"),
            ("quick_input", "Ctrl-Shift-Right", "-"),
            ("quick_input", "Ctrl-Up", "-"),
            ("quick_input", "Ctrl-Down", "-"),
            ("quick_input", "Ctrl-Left", "-"),
            ("quick_input", "Ctrl-Right", "-"),
            ("quick_input", "Ctrl-0", "input=ab0"),
            ("quick_input", "Shift-Up", "-"),
            ("quick_input", "Shift-Down", "-"),
            ("quick_input", "Left", "-"),
            ("quick_input", "Right", "panes=list[Main#1]"),
            ("quick_input", "Shift-Left", "focus=SessionDetail; panes=detail[Some(0) scroll=Some(0) cursor=None tail=Some(true)]"),
            ("quick_input", "Shift-Right", "focus=SessionDetail; panes=detail[Some(1) scroll=Some(0) cursor=None tail=Some(true)]"),
            ("quick_input", "Ctrl-Tab", "-"),
            ("quick_input", "Ctrl-Shift-Tab", "panes=list[Main#1]"),
            ("quick_input", "Ctrl-Shift-BackTab", "panes=list[Main#1]"),
            ("quick_input", "Up", "-"),
            ("quick_input", "Down", "-"),
            ("quick_input", "Esc", "input=; mode=Normal"),
            ("quick_input", "Enter", "-"),
            ("quick_input", "Backspace", "input=a"),
            ("quick_input", "x", "input=abx"),
            ("quick_input", "Ctrl-Enter", "-"),
            ("quick_input", "Ctrl-T", "input=abt"),
            ("quick_input", "Ctrl-S", "input=abs"),
            ("quick_input_config_pending", "Ctrl-Alt-G", "overlay=KeybindingsHelp"),
            ("quick_input_config_pending", "Ctrl-C", "quit=true"),
            ("quick_input_config_pending", "Ctrl-O", "input=abo"),
            ("quick_input_config_pending", "Ctrl-I", "input=abi"),
            ("quick_input_config_pending", "Ctrl-H", "input=abh"),
            ("quick_input_config_pending", "Ctrl-L", "input=abl"),
            ("quick_input_config_pending", "Ctrl-Shift-Up", "-"),
            ("quick_input_config_pending", "Ctrl-Shift-Down", "-"),
            ("quick_input_config_pending", "Ctrl-Shift-Left", "-"),
            ("quick_input_config_pending", "Ctrl-Shift-Right", "-"),
            ("quick_input_config_pending", "Ctrl-Up", "-"),
            ("quick_input_config_pending", "Ctrl-Down", "-"),
            ("quick_input_config_pending", "Ctrl-Left", "-"),
            ("quick_input_config_pending", "Ctrl-Right", "-"),
            ("quick_input_config_pending", "Ctrl-0", "input=ab0"),
            ("quick_input_config_pending", "Shift-Up", "-"),
            ("quick_input_config_pending", "Shift-Down", "-"),
            ("quick_input_config_pending", "Left", "-"),
            ("quick_input_config_pending", "Right", "panes=list[Main#1]"),
            ("quick_input_config_pending", "Shift-Left", "focus=SessionDetail; panes=detail[Some(0) scroll=Some(0) cursor=None tail=Some(true)]"),
            ("quick_input_config_pending", "Shift-Right", "focus=SessionDetail; panes=detail[Some(1) scroll=Some(0) cursor=None tail=Some(true)]"),
            ("quick_input_config_pending", "Ctrl-Tab", "-"),
            ("quick_input_config_pending", "Ctrl-Shift-Tab", "panes=list[Main#1]"),
            ("quick_input_config_pending", "Ctrl-Shift-BackTab", "panes=list[Main#1]"),
            ("quick_input_config_pending", "Up", "-"),
            ("quick_input_config_pending", "Down", "-"),
            ("quick_input_config_pending", "Esc", "input=; mode=Normal"),
            ("quick_input_config_pending", "Enter", "notes=1"),
            ("quick_input_config_pending", "Backspace", "input=a"),
            ("quick_input_config_pending", "x", "input=abx"),
            ("quick_input_config_pending", "Ctrl-Enter", "notes=1"),
            ("quick_input_config_pending", "Ctrl-T", "input=abt"),
            ("quick_input_config_pending", "Ctrl-S", "input=abs"),
            ("command", "Ctrl-Alt-G", "overlay=KeybindingsHelp"),
            ("command", "Ctrl-C", "quit=true"),
            ("command", "Ctrl-O", "command=zzo"),
            ("command", "Ctrl-I", "command=zzi"),
            ("command", "Ctrl-H", "command=zzh"),
            ("command", "Ctrl-L", "command=zzl"),
            ("command", "Ctrl-Shift-Up", "-"),
            ("command", "Ctrl-Shift-Down", "-"),
            ("command", "Ctrl-Shift-Left", "-"),
            ("command", "Ctrl-Shift-Right", "-"),
            ("command", "Ctrl-Up", "-"),
            ("command", "Ctrl-Down", "-"),
            ("command", "Ctrl-Left", "-"),
            ("command", "Ctrl-Right", "-"),
            ("command", "Ctrl-0", "command=zz0"),
            ("command", "Shift-Up", "-"),
            ("command", "Shift-Down", "-"),
            ("command", "Left", "-"),
            ("command", "Right", "panes=list[Main#1]"),
            ("command", "Shift-Left", "focus=SessionDetail; panes=detail[Some(0) scroll=Some(0) cursor=None tail=Some(true)]"),
            ("command", "Shift-Right", "focus=SessionDetail; panes=detail[Some(1) scroll=Some(0) cursor=None tail=Some(true)]"),
            ("command", "Ctrl-Tab", "-"),
            ("command", "Ctrl-Shift-Tab", "panes=list[Main#1]"),
            ("command", "Ctrl-Shift-BackTab", "panes=list[Main#1]"),
            ("command", "Up", "-"),
            ("command", "Down", "-"),
            ("command", "Esc", "command=; mode=Normal"),
            ("command", "Enter", "command=; mode=Normal; notes=1"),
            ("command", "Backspace", "command=z"),
            ("command", "x", "command=zzx"),
            ("command", "Ctrl-Enter", "command=; mode=Normal; notes=1"),
            ("command", "Ctrl-T", "command=zzt"),
            ("command", "Ctrl-S", "command=zzs"),
            ("search", "Ctrl-Alt-G", "overlay=KeybindingsHelp"),
            ("search", "Ctrl-C", "quit=true"),
            ("search", "Ctrl-O", "search=Fixo"),
            ("search", "Ctrl-I", "search=Fixi"),
            ("search", "Ctrl-H", "search=Fixh"),
            ("search", "Ctrl-L", "search=Fixl"),
            ("search", "Ctrl-Shift-Up", "-"),
            ("search", "Ctrl-Shift-Down", "-"),
            ("search", "Ctrl-Shift-Left", "-"),
            ("search", "Ctrl-Shift-Right", "-"),
            ("search", "Ctrl-Up", "-"),
            ("search", "Ctrl-Down", "-"),
            ("search", "Ctrl-Left", "-"),
            ("search", "Ctrl-Right", "-"),
            ("search", "Ctrl-0", "search=Fix0"),
            ("search", "Shift-Up", "-"),
            ("search", "Shift-Down", "-"),
            ("search", "Left", "-"),
            ("search", "Right", "panes=list[Main#1]"),
            ("search", "Shift-Left", "focus=SessionDetail; panes=detail[Some(0) scroll=Some(0) cursor=None tail=Some(true)]"),
            ("search", "Shift-Right", "focus=SessionDetail; panes=detail[Some(1) scroll=Some(0) cursor=None tail=Some(true)]"),
            ("search", "Ctrl-Tab", "-"),
            ("search", "Ctrl-Shift-Tab", "panes=list[Main#1]"),
            ("search", "Ctrl-Shift-BackTab", "panes=list[Main#1]"),
            ("search", "Up", "-"),
            ("search", "Down", "-"),
            ("search", "Esc", "mode=Normal; search="),
            ("search", "Enter", "mode=Normal"),
            ("search", "Backspace", "search=Fi"),
            ("search", "x", "search=Fixx"),
            ("search", "Ctrl-Enter", "mode=Normal"),
            ("search", "Ctrl-T", "search=Fixt"),
            ("search", "Ctrl-S", "search=Fixs"),
        ];
    }
}
