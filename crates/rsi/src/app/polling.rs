//! Connection and polling operations for App.

use super::App;
use super::ConversationPollResult;
use super::EventApplyMode;
use crate::poll_controller::PollPhase;
use crate::types::Pane;
use rsi_common::rpc::ConversationFetchCursor;
use rsi_common::types::ConversationEvent;
use std::collections::HashSet;
use uuid::Uuid;

const CONVERSATION_POLL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
const WORKER_PRESSURE_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);

/// Re-read sooner while the rsid CPU reading still needs its second sample.
const DAEMON_CPU_FIRST_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3);

/// One bounded background Health read: worker-slice pressure for the top
/// chrome plus rsid's own resource usage for the live-activity pane.
#[derive(Debug, Clone)]
pub(crate) struct DaemonHealthRead {
    pub(crate) pressure: Option<rsi_common::rpc::WorkerSliceMemoryPressure>,
    pub(crate) resources: crate::daemon_resources::DaemonResourceSample,
    /// #1122: the pending operator restart line, when one is waiting.
    pub(crate) restart_pending: Option<String>,
    /// Outer None preserves identity and guidance across a transient failed read.
    pub(crate) daemon_info: Option<Option<rsi_common::agent_daemon_info::DaemonInfoV1>>,
}

async fn fetch_worker_pressure(
    socket_path: std::path::PathBuf,
) -> Result<DaemonHealthRead, String> {
    tokio::time::timeout(CONVERSATION_POLL_TIMEOUT, async {
        let mut client = crate::client::DaemonClient::new(socket_path);
        client.connect().await.map_err(|error| error.to_string())?;
        let pid = client.peer_pid();
        let status = client
            .get_health_status()
            .await
            .map_err(|error| error.to_string())?;
        let restart_pending = client
            .get_operator_restart()
            .await
            .ok()
            .and_then(|restart| restart.summary());
        let daemon_info = match client.get_daemon_info().await {
            Ok(info) => Some(Some(info)),
            Err(crate::client::ClientError::Rpc { code, .. })
                if code == rsi_common::rpc::METHOD_NOT_FOUND =>
            {
                Some(None)
            }
            Err(_) => None,
        };
        Ok(DaemonHealthRead {
            resources: crate::daemon_resources::DaemonResourceSample::from_health(&status, pid),
            pressure: status.worker_slice_memory_pressure,
            restart_pending,
            daemon_info,
        })
    })
    .await
    .map_err(|_| "worker pressure Health read timed out".to_string())?
}

fn is_connection_error(error: &crate::client::ClientError) -> bool {
    matches!(
        error,
        crate::client::ClientError::Io(_)
            | crate::client::ClientError::NotConnected
            | crate::client::ClientError::ConnectionClosed
    )
}

async fn run_background_conversation_poll(
    socket_path: std::path::PathBuf,
    attempt: u64,
    generation: u64,
    cursors: Vec<ConversationFetchCursor>,
    batch_supported: bool,
) -> ConversationPollResult {
    run_background_conversation_poll_with_timeout(
        socket_path,
        attempt,
        generation,
        cursors,
        batch_supported,
        CONVERSATION_POLL_TIMEOUT,
    )
    .await
}

async fn run_background_conversation_poll_with_timeout(
    socket_path: std::path::PathBuf,
    attempt: u64,
    generation: u64,
    cursors: Vec<ConversationFetchCursor>,
    batch_supported: bool,
    method_timeout: std::time::Duration,
) -> ConversationPollResult {
    let mut client = crate::client::DaemonClient::new(socket_path);
    if let Err(error) = client.connect().await {
        return ConversationPollResult {
            attempt,
            generation,
            batches: Err(error.to_string()),
            batch_unsupported: false,
            connection_lost: is_connection_error(&error),
            timed_out: false,
        };
    }

    let poll = async {
        let mut batch_unsupported = false;

        if batch_supported {
            match client.get_conversations_batch(cursors.clone()).await {
                Ok(batches) => {
                    return Ok((
                        batches
                            .into_iter()
                            .map(|(session_id, events)| {
                                let since_sequence = cursors
                                    .iter()
                                    .find(|cursor| cursor.session_id == session_id)
                                    .and_then(|cursor| cursor.since_sequence);
                                (session_id, since_sequence, events)
                            })
                            .collect(),
                        false,
                    ));
                }
                Err(crate::client::ClientError::Rpc { code, .. })
                    if code == rsi_common::rpc::METHOD_NOT_FOUND =>
                {
                    batch_unsupported = true;
                }
                Err(error) => return Err((error.to_string(), is_connection_error(&error))),
            }
        }

        let mut batches = Vec::with_capacity(cursors.len());
        for cursor in cursors {
            match client
                .get_conversation(cursor.session_id, cursor.since_sequence)
                .await
            {
                Ok(events) => batches.push((cursor.session_id, cursor.since_sequence, events)),
                Err(error) => return Err((error.to_string(), is_connection_error(&error))),
            }
        }
        Ok((batches, batch_unsupported))
    };

    match tokio::time::timeout(method_timeout, poll).await {
        Ok(Ok((batches, batch_unsupported))) => ConversationPollResult {
            attempt,
            generation,
            batches: Ok(batches),
            batch_unsupported,
            connection_lost: false,
            timed_out: false,
        },
        Ok(Err((error, connection_lost))) => ConversationPollResult {
            attempt,
            generation,
            batches: Err(error),
            batch_unsupported: false,
            connection_lost,
            timed_out: false,
        },
        Err(_) => ConversationPollResult {
            attempt,
            generation,
            batches: Err(crate::client::ClientError::Timeout(method_timeout.as_secs()).to_string()),
            batch_unsupported: false,
            // A method deadline is not transport evidence. The daemon may be
            // healthy but slow; only an observed connect/read/write closure
            // withdraws readiness and starts bootstrap.
            connection_lost: false,
            timed_out: true,
        },
    }
}

impl App {
    /// Complete one bounded Health task and schedule the next without waiting
    /// for daemon I/O on the input/render task.
    pub(crate) async fn poll_worker_pressure_refresh(&mut self) {
        if self
            .worker_pressure_refresh_handle
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
            && let Some(handle) = self.worker_pressure_refresh_handle.take()
        {
            let (pressure, resources, restart_pending, daemon_info) = match handle.await {
                Ok(Ok(read)) => (
                    read.pressure,
                    Some(read.resources),
                    read.restart_pending,
                    read.daemon_info,
                ),
                Ok(Err(error)) => {
                    tracing::debug!(%error, "worker pressure Health refresh unavailable");
                    (None, None, None, None)
                }
                Err(error) => {
                    tracing::debug!(%error, "worker pressure Health task failed");
                    (None, None, None, None)
                }
            };
            if let Some(info) = daemon_info {
                self.apply_daemon_info(info);
            }
            if self.worker_slice_memory_pressure != pressure {
                self.worker_slice_memory_pressure = pressure;
                self.mark_dirty();
            }
            let resources = resources.map(|sample| {
                let mut view = crate::daemon_resources::DaemonResourceView::next(
                    self.daemon_resources.as_ref(),
                    sample,
                );
                view.restart_pending = restart_pending;
                view
            });
            if resources
                .as_ref()
                .is_some_and(crate::daemon_resources::DaemonResourceView::awaiting_cpu)
            {
                self.worker_pressure_next_refresh_at =
                    std::time::Instant::now() + DAEMON_CPU_FIRST_INTERVAL;
            }
            if self.daemon_resources != resources {
                self.daemon_resources = resources;
                self.mark_dirty();
            }
        }
        if !self.poll.connected
            || self.worker_pressure_refresh_handle.is_some()
            || std::time::Instant::now() < self.worker_pressure_next_refresh_at
        {
            return;
        }
        self.worker_pressure_next_refresh_at =
            std::time::Instant::now() + WORKER_PRESSURE_REFRESH_INTERVAL;
        let socket_path = self.client.socket_path().to_path_buf();
        self.worker_pressure_refresh_handle =
            Some(tokio::spawn(fetch_worker_pressure(socket_path)));
    }

    pub(crate) fn note_manager_turnover(&mut self, session: &rsi_common::types::Session) {
        if session
            .continued_from
            .is_some_and(|id| self.manager_roster.contains(id))
            || (self.manager_roster.contains(session.id)
                && session.status.is_terminal()
                && self
                    .sessions
                    .get(&session.id)
                    .is_some_and(|state| !state.session.status.is_terminal()))
        {
            self.manager_roster.request_refresh();
        }
    }

    /// Apply a completed batch, then start at most one new background batch.
    /// The event loop never waits for a manager RPC before handling input.
    #[allow(clippy::future_not_send)] // App is owned by the single-threaded event loop.
    pub(crate) async fn poll_manager_roster_refresh(&mut self) {
        if self
            .manager_roster_refresh
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
            && let Some(handle) = self.manager_roster_refresh.take()
        {
            let completed = handle.await;
            if let Ok(results) = completed {
                let projects: HashSet<Uuid> =
                    self.projects.iter().map(|project| project.id).collect();
                let mut changed = self.manager_roster.retain_projects(&projects);
                for (project_id, result) in results {
                    if !projects.contains(&project_id) {
                        continue;
                    }
                    match self.manager_roster.apply_result(project_id, result) {
                        Ok(updated) => changed |= updated,
                        Err(error) => self.notify_error(format!(
                            "Manager roster refresh failed for project {project_id}: {error}"
                        )),
                    }
                }
                if changed {
                    self.sort_sessions(false);
                    self.invalidate_card_cache();
                    self.mark_dirty();
                }
            } else {
                self.notify_error("Manager roster refresh task failed");
            }
        }
        if self.manager_roster_refresh.is_some() || !self.manager_roster.take_refresh() {
            return;
        }
        let project_ids: Vec<Uuid> = self.projects.iter().map(|project| project.id).collect();
        let socket_path = self.client.socket_path().to_path_buf();
        self.manager_roster_refresh = Some(tokio::spawn(async move {
            let mut results = Vec::with_capacity(project_ids.len());
            for project_id in project_ids {
                let result = tokio::time::timeout(std::time::Duration::from_secs(3), async {
                    let mut client = crate::client::DaemonClient::new(socket_path.clone());
                    client.connect().await.map_err(|error| error.to_string())?;
                    client
                        .get_harness_manager(project_id)
                        .await
                        .map_err(|error| error.to_string())
                })
                .await
                .unwrap_or_else(|_| Err("GetHarnessManager timed out".into()));
                results.push((project_id, result));
            }
            results
        }));
    }

    fn canonical_session_label(&self, session_id: Uuid, max_chars: usize) -> Option<String> {
        self.sessions.get(&session_id).map(|state| {
            crate::types::resolve_session_display_identity(&state.session, &self.sessions)
                .effective_title
                .chars()
                .take(max_chars)
                .collect()
        })
    }

    // --- Connection and polling ---

    /// Start the shared non-blocking startup/reconnect coordinator.
    ///
    /// Kept async for compatibility with existing call sites; it deliberately
    /// performs no RPC work on the caller task.
    pub async fn connect(&mut self) {
        self.start_bootstrap();
    }

    /// Read one session's pause marker with its age/held-succession detail.
    pub(crate) async fn probe_operator_pause(&mut self, session_id: Uuid) {
        if let Ok(info) = self.client.get_operator_pause_info(session_id).await {
            let level = info.pause_level;
            if self.operator_pauses.insert(session_id, level) != Some(level) {
                self.needs_redraw = true;
            }
            // The age only moves the label at minute granularity; the held
            // flag and level are what change the row.
            let changed = self
                .operator_pause_info
                .get(&session_id)
                .map(|old| (old.pause_level, old.held_succession, old.since.as_ref()))
                != Some((info.pause_level, info.held_succession, info.since.as_ref()));
            if changed {
                self.needs_redraw = true;
            }
            self.operator_pause_info.insert(session_id, info);
        }
    }

    /// Poll daemon for session updates and conversation events.
    pub async fn poll_sessions(&mut self) {
        if !self.poll.connected {
            return;
        }

        // Fetch sessions
        let result = tokio::time::timeout(CONVERSATION_POLL_TIMEOUT, self.client.list_sessions())
            .await
            .unwrap_or_else(|_| {
                Err(crate::client::ClientError::Timeout(
                    CONVERSATION_POLL_TIMEOUT.as_secs(),
                ))
            });
        match result {
            Ok(sessions) => {
                let _ = self.update_sessions(sessions);
            }
            Err(e) => {
                if is_connection_error(&e) {
                    self.mark_transport_lost(e.to_string());
                } else {
                    self.notify_error(e.to_string());
                }
                return;
            }
        }

        // A manager seat's pause is never invisible (#1541): probe every
        // rostered seat each poll, ahead of the round-robin below.
        let seats: Vec<Uuid> = self
            .session_order
            .iter()
            .copied()
            .filter(|id| self.manager_roster.contains(*id))
            .collect();
        for session_id in seats {
            self.probe_operator_pause(session_id).await;
        }

        // GetOperatorPause is a per-session operator read. Walk the visible
        // list in bounded batches so every row eventually reflects external
        // pause edits without holding an entire poll behind a large roster.
        for _ in 0..self.session_order.len().min(8) {
            let index = self.operator_pause_probe_cursor % self.session_order.len();
            self.operator_pause_probe_cursor = self.operator_pause_probe_cursor.wrapping_add(1);
            let session_id = self.session_order[index];
            self.probe_operator_pause(session_id).await;
            if let Ok(messages) = self.client.list_operator_messages(session_id).await
                && self.operator_messages.get(&session_id) != Some(&messages)
            {
                self.operator_messages.insert(session_id, messages);
                self.needs_redraw = true;
            }
        }

        // Fetch projects (less critical — errors ignored)
        if let Ok(projects) = self.client.list_projects().await {
            let _ = self.update_projects(projects);
        }

        // Fetch labels (less critical — errors ignored)
        if let Ok(labels) = self.client.list_labels().await {
            self.update_labels(labels);
        }

        // Collect session IDs that need event fetching:
        // - Sessions that are Running or Starting (actively producing output)
        // - Sessions currently visible in any focused pane
        let visible_session_ids = self.visible_session_ids();
        let ids_to_fetch: Vec<Uuid> = self
            .sessions
            .iter()
            .filter(|(id, state)| {
                let is_active = matches!(
                    state.session.status,
                    rsi_common::types::SessionStatus::Running
                        | rsi_common::types::SessionStatus::Starting
                );
                let is_visible = visible_session_ids.contains(id);
                is_active || is_visible
            })
            .map(|(id, _)| *id)
            .collect();

        for session_id in ids_to_fetch {
            match self.client.get_conversation(session_id, None).await {
                Ok(events) => {
                    // Resolve effective topology before the mutable borrow below.
                    let topology = self.effective_topology(session_id);
                    // D2 (stamp-while-watching): resolve focus before the
                    // mutable borrow too.
                    let is_focused_detail = matches!(
                        self.focused_pane(),
                        Some(Pane::SessionDetail { session_id: sid }) if *sid == session_id
                    );
                    if let Some(state) = self.sessions.get_mut(&session_id) {
                        let changed = Self::apply_session_events(
                            state,
                            events,
                            EventApplyMode::Replace,
                            &self.workflows,
                            topology,
                        );
                        if changed && is_focused_detail {
                            state.last_seen_events_generation = state.events_generation;
                        }
                    }
                }
                Err(_) => {
                    // Non-fatal: session may have been removed between list and get
                }
            }
        }
    }

    // --- Phase-based polling (non-blocking) ---

    /// Start a new poll cycle. Returns the first phase.
    /// ENHANCED: Only polls for actively running sessions, not navigation data.
    /// Navigation data is now handled by event-driven navigation effects and manual refresh.
    pub(crate) fn start_poll_cycle(&self) -> Option<PollPhase> {
        if self.bootstrap_should_start() {
            return Some(PollPhase::Connect);
        }

        if !self.poll.connected
            || self.conversation_poll_handle.is_some()
            || self.conversation_poll_pending_phase.is_some()
        {
            return None;
        }

        // Only poll for actively running sessions that produce output
        let active_running_ids = self.active_running_session_ids();
        if !active_running_ids.is_empty() {
            Some(PollPhase::FetchConversations {
                ids: active_running_ids,
                index: 0,
            })
        } else {
            None // No poll cycle needed
        }
    }

    /// Session IDs for the bounded catch-up poll: every running/starting leaf
    /// session, INCLUDING the focused detail session and the global manager
    /// workspace's conversation session (#1234). Push is the primary path, but a
    /// dropped push would otherwise leave an open transcript stale until the
    /// next refocus. The fetch is incremental (`since_sequence`) and batched, so
    /// the steady-state cost is one cheap empty read per cycle.
    ///
    /// Container kinds (Group/Epic) have no conversation events and are never
    /// polled; their metadata arrives via push. Sessions with an inflight focus
    /// fetch are skipped so the same transcript is not fetched twice.
    fn active_running_session_ids(&self) -> Vec<uuid::Uuid> {
        self.sessions
            .iter()
            .filter(|(id, state)| {
                let is_actively_running = matches!(
                    state.session.status,
                    rsi_common::types::SessionStatus::Running
                        | rsi_common::types::SessionStatus::Starting
                );
                let focus_fetch_owns_it = self.focus_fetch_inflight.contains(id);
                let is_container = rsi_common::is_container_kind(state.session.session_kind);
                is_actively_running && !focus_fetch_owns_it && !is_container
            })
            .map(|(id, _)| *id)
            .collect()
    }

    /// Restore the pre-bootstrap initial hydration contract without waiting on
    /// the event-loop task. Selected completed leaves are prioritized alongside
    /// active unloaded leaves; navigation-owned focus fetches remain deduped.
    pub(crate) fn start_initial_conversation_hydration(&mut self) {
        let visible = self.visible_session_ids();
        let mut cursors = Vec::new();
        let mut seen = HashSet::new();

        for session_id in visible.into_iter().chain(self.active_running_session_ids()) {
            if !seen.insert(session_id) || self.focus_fetch_inflight.contains(&session_id) {
                continue;
            }
            let Some(state) = self.sessions.get(&session_id) else {
                continue;
            };
            let unloaded = state.events.is_empty() && state.last_sequence.is_none();
            if unloaded && rsi_common::is_leaf_kind(state.session.session_kind) {
                cursors.push(ConversationFetchCursor {
                    session_id,
                    since_sequence: None,
                });
            }
        }

        self.enqueue_initial_conversation_hydration(cursors);
    }

    /// Execute one step of the poll cycle. Returns the next phase, or None if done.
    /// ENHANCED: Simplified to only handle connection and active conversation fetching.
    /// Navigation data (sessions, projects, labels) is now handled by event-driven effects.
    pub(crate) async fn poll_step(&mut self, phase: PollPhase) -> (Option<PollPhase>, bool) {
        let mut dirty = false;
        let next = match phase {
            PollPhase::Connect => {
                self.start_bootstrap();
                dirty = true;
                None
            }
            PollPhase::FetchConversations { ids, index } => {
                self.dispatch_conversation_poll_phase(ids, index);
                None
            }
        };
        (next, dirty)
    }

    /// Start one fallback conversation poll on an owned short-lived client.
    /// The task has a hard timeout and delivers a bounded result to the main
    /// event-loop task, so a withholding daemon cannot freeze input or render.
    fn start_background_conversation_poll(
        &mut self,
        cursors: Vec<ConversationFetchCursor>,
    ) -> bool {
        if cursors.is_empty() || self.conversation_poll_handle.is_some() {
            return false;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return false;
        };
        let socket_path = self.client.socket_path().to_path_buf();
        let attempt = self.bootstrap.attempt();
        self.conversation_poll_generation = self.conversation_poll_generation.saturating_add(1);
        let generation = self.conversation_poll_generation;
        self.conversation_poll_active_generation = Some(generation);
        self.conversation_poll_active_cursors = cursors.clone();
        let batch_supported = self.poll.batch_fetch_supported;
        let tx = self.conversation_poll_tx.clone();
        self.conversation_poll_handle = Some(runtime.spawn(async move {
            let result = run_background_conversation_poll(
                socket_path,
                attempt,
                generation,
                cursors,
                batch_supported,
            )
            .await;
            let _ = tx.send(result).await;
        }));
        true
    }

    fn enqueue_initial_conversation_hydration(&mut self, cursors: Vec<ConversationFetchCursor>) {
        let active: HashSet<_> = self
            .conversation_poll_active_cursors
            .iter()
            .map(|cursor| cursor.session_id)
            .collect();
        let mut queued = self
            .conversation_poll_hydration_pending
            .take()
            .unwrap_or_default();
        let mut queued_ids: HashSet<_> = queued.iter().map(|cursor| cursor.session_id).collect();
        queued.extend(cursors.into_iter().filter(|cursor| {
            !active.contains(&cursor.session_id) && queued_ids.insert(cursor.session_id)
        }));
        let mut cursors = queued;
        if cursors.is_empty() {
            return;
        }
        if self.conversation_poll_handle.is_some() {
            self.conversation_poll_hydration_pending = Some(cursors);
            return;
        }
        let remainder = if cursors.len() > super::CONVERSATION_BATCH_SIZE {
            Some(cursors.split_off(super::CONVERSATION_BATCH_SIZE))
        } else {
            None
        };
        let first_batch = cursors.clone();
        if self.start_background_conversation_poll(cursors) {
            self.conversation_poll_hydration_pending = remainder;
        } else if let Some(mut remainder) = remainder {
            // Restore the complete queue if task admission failed.
            let mut all = first_batch;
            all.append(&mut remainder);
            self.conversation_poll_hydration_pending = Some(all);
        } else {
            self.conversation_poll_hydration_pending = Some(first_batch);
        }
    }

    /// Retain later chunks in one owned continuation and start only the first
    /// accepted batch. Completion, not a one-millisecond timer, advances it.
    fn dispatch_conversation_poll_phase(&mut self, ids: Vec<Uuid>, index: usize) {
        if ids.is_empty() || self.conversation_poll_handle.is_some() {
            return;
        }
        let end = (index + super::CONVERSATION_BATCH_SIZE).min(ids.len());
        let cursors = ids[index..end]
            .iter()
            .filter_map(|session_id| {
                self.sessions
                    .get(session_id)
                    .map(|state| ConversationFetchCursor {
                        session_id: *session_id,
                        since_sequence: state.last_sequence,
                    })
            })
            .collect();
        if self.start_background_conversation_poll(cursors) && end < ids.len() {
            self.conversation_poll_pending_phase =
                Some(PollPhase::FetchConversations { ids, index: end });
        }
    }

    fn dispatch_pending_conversation_poll(&mut self) {
        if self.conversation_poll_handle.is_some() {
            return;
        }
        if let Some(PollPhase::FetchConversations { ids, index }) =
            self.conversation_poll_pending_phase.take()
        {
            self.dispatch_conversation_poll_phase(ids, index);
            return;
        }
        if let Some(mut cursors) = self.conversation_poll_hydration_pending.take() {
            let remainder = if cursors.len() > super::CONVERSATION_BATCH_SIZE {
                Some(cursors.split_off(super::CONVERSATION_BATCH_SIZE))
            } else {
                None
            };
            if self.start_background_conversation_poll(cursors.clone()) {
                self.conversation_poll_hydration_pending = remainder;
            } else {
                if let Some(mut remainder) = remainder {
                    cursors.append(&mut remainder);
                }
                self.conversation_poll_hydration_pending = Some(cursors);
            }
        }
    }

    /// Apply a completed fallback poll on the event-loop task. Transport loss
    /// is classified here, where reconnect shares the bootstrap coordinator.
    pub(crate) fn apply_conversation_poll_result(
        &mut self,
        result: ConversationPollResult,
    ) -> bool {
        if self.conversation_poll_active_generation != Some(result.generation) {
            return false;
        }
        self.conversation_poll_active_generation = None;
        self.conversation_poll_active_cursors.clear();
        self.conversation_poll_handle = None;
        if result.attempt != self.bootstrap.attempt() || !self.poll.connected {
            self.dispatch_pending_conversation_poll();
            return false;
        }
        if result.batch_unsupported {
            self.poll.batch_fetch_supported = false;
        }
        let batches = match result.batches {
            Ok(batches) => batches,
            Err(error) => {
                if result.connection_lost {
                    self.mark_transport_lost(error);
                } else if result.timed_out {
                    self.notify(error);
                    self.mark_dirty();
                } else {
                    tracing::debug!(%error, "fallback conversation poll failed");
                }
                self.dispatch_pending_conversation_poll();
                return result.timed_out;
            }
        };

        let mut dirty = false;
        for (session_id, since_sequence, events) in batches {
            let topology = self.effective_topology(session_id);
            let is_focused_detail = matches!(
                self.focused_pane(),
                Some(Pane::SessionDetail { session_id: sid }) if *sid == session_id
            );
            if let Some(state) = self.sessions.get_mut(&session_id) {
                let mode = if since_sequence.is_some() {
                    EventApplyMode::Append
                } else {
                    EventApplyMode::Replace
                };
                let changed =
                    Self::apply_session_events(state, events, mode, &self.workflows, topology);
                if changed && is_focused_detail {
                    state.last_seen_events_generation = state.events_generation;
                }
                dirty |= changed;
            }
        }
        if dirty {
            // Only demonstrable event progress reopens rejected auto-resume
            // sources. Repeated successful empty observations are not an
            // external-progress epoch and cannot create an endless retry loop.
            self.advance_auto_resume_progress();
        }
        if !dirty {
            self.request_auto_launch_resume_handoff();
        }
        self.dispatch_pending_conversation_poll();
        dirty
    }

    /// Collect session IDs visible in focused panes across all tabs.
    pub(crate) fn visible_session_ids(&self) -> Vec<Uuid> {
        let mut ids = Vec::new();
        let mut seen = HashSet::new();
        for tab in &self.tabs {
            if let Some(pane) = tab.layout.find_pane(tab.focused_pane) {
                match pane {
                    Pane::SessionDetail { session_id } => {
                        if seen.insert(*session_id) {
                            ids.push(*session_id);
                        }
                    }
                    Pane::SessionList {
                        selected_session, ..
                    } => {
                        if let Some(id) = selected_session {
                            if seen.insert(*id) {
                                ids.push(*id);
                            }
                        }
                    }
                    Pane::Settings | Pane::PromptCreator | Pane::Issues(_) => {}
                }
            }
        }
        ids
    }

    /// Handle a push event from the daemon notification stream.
    /// Returns true if the UI should redraw.
    pub fn apply_push_event(&mut self, event: rsi_common::rpc::BusEvent) -> bool {
        let result = match event.event_type.as_str() {
            "session_created" => {
                #[derive(serde::Deserialize)]
                struct CreatedInner {
                    session: rsi_common::types::Session,
                }
                if let Ok(parsed) = serde_json::from_value::<CreatedInner>(event.data) {
                    self.navigation_cache
                        .process_push_invalidation("session_created", Some(parsed.session.id));
                    if let Some(parent_id) = parsed.session.parent_id {
                        self.invalidate_hierarchy_node(Some(parent_id));
                    } else {
                        self.invalidate_hierarchy_node(None);
                    }
                    return self.upsert_session(parsed.session);
                }
                false
            }
            "conversation_event" => {
                #[derive(serde::Deserialize)]
                struct ConvInner {
                    session_id: Uuid,
                    event: ConversationEvent,
                }
                if let Ok(parsed) = serde_json::from_value::<ConvInner>(event.data) {
                    // Invalidate conversation cache for this session
                    self.navigation_cache
                        .process_push_invalidation("conversation_event", Some(parsed.session_id));

                    // Resolve effective topology before the mutable borrow below.
                    let topology = self.effective_topology(parsed.session_id);
                    // D2 (stamp-while-watching): resolve focus before the
                    // mutable borrow too.
                    let is_focused_detail = matches!(
                        self.focused_pane(),
                        Some(Pane::SessionDetail { session_id: sid }) if *sid == parsed.session_id
                    );
                    if let Some(state) = self.sessions.get_mut(&parsed.session_id) {
                        // Clear stall flag — session is producing output again
                        let cleared_stall = state.is_stalled;
                        state.is_stalled = false;
                        let changed = Self::apply_session_events(
                            state,
                            vec![parsed.event],
                            EventApplyMode::Append,
                            &self.workflows,
                            topology,
                        );
                        if changed && is_focused_detail {
                            state.last_seen_events_generation = state.events_generation;
                        }
                        if cleared_stall {
                            self.invalidate_session_browser_cache();
                        }
                        if changed {
                            self.capture_docreg_source_push(parsed.session_id);
                        }
                        return changed;
                    }
                }
                false
            }
            "session_question_raised" => {
                #[derive(serde::Deserialize)]
                struct QInner {
                    session_id: Uuid,
                    question: rsi_common::types::PendingQuestion,
                }
                if let Ok(parsed) = serde_json::from_value::<QInner>(event.data) {
                    let label = self
                        .canonical_session_label(parsed.session_id, 40)
                        .filter(|label| !label.is_empty())
                        .unwrap_or_else(|| "Session".to_string());
                    if let Some(state) = self.sessions.get_mut(&parsed.session_id) {
                        state.session.pending_question = Some(parsed.question);
                        state.session.status = rsi_common::types::SessionStatus::WaitingApproval;
                        self.push_notification(
                            crate::types::NotificationKind::Info,
                            crate::types::NotificationPriority::High,
                            format!("{}: needs your answer (gq)", label),
                            Some(parsed.session_id),
                        );
                        if self.settings.auto_open_question_modal
                            && matches!(self.overlay, crate::types::OverlayState::None)
                        {
                            crate::overlay::open_question_modal(self);
                        }
                        self.refresh_session_browser_order_after_focus_change();
                        return true;
                    }
                }
                false
            }
            "session_status_changed" => {
                #[derive(serde::Deserialize)]
                struct StatusInner {
                    session_id: Uuid,
                    new_status: rsi_common::types::SessionStatus,
                }
                if let Ok(parsed) = serde_json::from_value::<StatusInner>(event.data) {
                    if matches!(
                        parsed.new_status,
                        rsi_common::types::SessionStatus::Archived
                            | rsi_common::types::SessionStatus::Deleted
                    ) {
                        if self.manager_roster.contains(parsed.session_id) {
                            self.manager_roster.request_refresh();
                        }
                        let old_parent = self
                            .sessions
                            .get(&parsed.session_id)
                            .map(|s| s.session.parent_id);
                        self.sessions.remove(&parsed.session_id);
                        self.session_order.retain(|id| *id != parsed.session_id);
                        if !self.docreg_operation_is_archiving(parsed.session_id) {
                            self.recalculate_filtered_order();
                            self.reconcile_all_session_list_selections(true);
                            self.forget_removed_session(parsed.session_id);
                            if let Some(parent_id) = old_parent {
                                self.invalidate_hierarchy_node(parent_id);
                            }
                        }
                        return true;
                    }
                    let label = self
                        .canonical_session_label(parsed.session_id, 40)
                        .filter(|label| !label.is_empty())
                        .unwrap_or_else(|| "Session".to_string());
                    if let Some(state) = self.sessions.get_mut(&parsed.session_id) {
                        let old = state.session.status;
                        state.session.status = parsed.new_status;
                        if self.manager_roster.contains(parsed.session_id)
                            && parsed.new_status.is_terminal()
                            && old != parsed.new_status
                        {
                            self.manager_roster.request_refresh();
                        }
                        // Notify on completion
                        if parsed.new_status != rsi_common::types::SessionStatus::Running
                            && old == rsi_common::types::SessionStatus::Running
                        {
                            self.push_notification(
                                crate::types::NotificationKind::Info,
                                crate::types::NotificationPriority::Medium,
                                format!("{}: {:?}", label, parsed.new_status),
                                Some(parsed.session_id),
                            );
                        }
                        self.refresh_session_browser_order_after_focus_change();
                        return true;
                    }
                }
                false
            }
            "session_deleted" => {
                #[derive(serde::Deserialize)]
                struct DeletedInner {
                    session_id: Uuid,
                }
                if let Ok(parsed) = serde_json::from_value::<DeletedInner>(event.data) {
                    // Invalidate cache for deleted session
                    self.navigation_cache
                        .process_push_invalidation("session_deleted", Some(parsed.session_id));

                    let old_parent = self
                        .sessions
                        .get(&parsed.session_id)
                        .map(|s| s.session.parent_id);
                    self.sessions.remove(&parsed.session_id);
                    self.session_order.retain(|id| *id != parsed.session_id);
                    if !self.docreg_operation_is_archiving(parsed.session_id) {
                        self.recalculate_filtered_order();
                        self.reconcile_all_session_list_selections(true);
                        self.forget_removed_session(parsed.session_id);
                        if let Some(parent_id) = old_parent {
                            self.invalidate_hierarchy_node(parent_id);
                        }
                    } else if let Some(parent_id) = old_parent {
                        self.hierarchy_stale_nodes.insert(parent_id);
                        self.navigation_cache.invalidate_hierarchy(parent_id);
                    }
                    return true;
                }
                false
            }
            "session_archived" => {
                #[derive(serde::Deserialize)]
                struct ArchivedInner {
                    session_id: Uuid,
                }
                if let Ok(parsed) = serde_json::from_value::<ArchivedInner>(event.data) {
                    // Invalidate cache for archived session
                    self.navigation_cache
                        .process_push_invalidation("session_archived", Some(parsed.session_id));

                    self.record_docreg_archive_notification(parsed.session_id);

                    let old_parent = self
                        .sessions
                        .get(&parsed.session_id)
                        .map(|s| s.session.parent_id);
                    self.sessions.remove(&parsed.session_id);
                    self.session_order.retain(|id| *id != parsed.session_id);
                    if !self.docreg_operation_is_archiving(parsed.session_id) {
                        self.recalculate_filtered_order();
                        self.reconcile_all_session_list_selections(true);
                        self.forget_removed_session(parsed.session_id);
                        if let Some(parent_id) = old_parent {
                            self.invalidate_hierarchy_node(parent_id);
                        }
                    } else if let Some(parent_id) = old_parent {
                        self.hierarchy_stale_nodes.insert(parent_id);
                        self.navigation_cache.invalidate_hierarchy(parent_id);
                    }
                    return true;
                }
                false
            }
            "session_metadata_changed" => {
                #[derive(serde::Deserialize)]
                struct MetaInner {
                    session_id: Uuid,
                    model: Option<String>,
                    pinned_at: Option<Option<String>>,
                    project_id: Option<Option<Uuid>>,
                    parent_id: Option<Option<Uuid>>,
                    lead_session_id: Option<Option<Uuid>>,
                    rotation_disabled_at: Option<Option<String>>,
                    #[serde(default)]
                    resolved_context_budget:
                        Option<rsi_common::provider_capabilities::ResolvedContextBudget>,
                }
                if let Ok(parsed) = serde_json::from_value::<MetaInner>(event.data) {
                    if (parsed.model.is_some() || parsed.resolved_context_budget.is_some())
                        && (self
                            .coordinator_context_budgets
                            .iter()
                            .any(|s| s.session_id == parsed.session_id)
                            || self.settings_state.section
                                == crate::settings_registry::SettingsSection::ContextRotation)
                    {
                        self.request_daemon_config_refresh();
                    }
                    // Invalidate cache for metadata changes
                    self.navigation_cache.process_push_invalidation(
                        "session_metadata_changed",
                        Some(parsed.session_id),
                    );

                    let mut stale_parents: Vec<Option<Uuid>> = Vec::new();
                    if let Some(state) = self.sessions.get_mut(&parsed.session_id) {
                        let mut needs_refilter = false;
                        if let Some(model) = parsed.model {
                            state.session.model = Some(model);
                        }
                        if let Some(pinned_at) = parsed.pinned_at {
                            state.session.pinned_at = pinned_at.and_then(|s| {
                                chrono::DateTime::parse_from_rfc3339(&s)
                                    .ok()
                                    .map(|dt| dt.with_timezone(&chrono::Utc))
                            });
                            needs_refilter = true;
                        }
                        if let Some(project_id) = parsed.project_id {
                            state.session.project_id = project_id;
                            needs_refilter = true;
                        }
                        if let Some(parent_id) = parsed.parent_id {
                            if state.session.parent_id != parent_id {
                                stale_parents.push(state.session.parent_id);
                                stale_parents.push(parent_id);
                            }
                            state.session.parent_id = parent_id;
                            needs_refilter = true;
                        }
                        if let Some(lead_session_id) = parsed.lead_session_id {
                            state.session.lead_session_id = lead_session_id;
                            needs_refilter = true;
                        }
                        if let Some(rotation_disabled_at) = parsed.rotation_disabled_at {
                            state.session.rotation_disabled_at =
                                rotation_disabled_at.and_then(|s| {
                                    chrono::DateTime::parse_from_rfc3339(&s)
                                        .ok()
                                        .map(|dt| dt.with_timezone(&chrono::Utc))
                                });
                        }
                        if let Some(resolved_context_budget) = parsed.resolved_context_budget {
                            state.session.resolved_context_budget = Some(resolved_context_budget);
                        }
                        if needs_refilter {
                            self.sort_sessions(true);
                        }
                        for parent_id in stale_parents {
                            self.invalidate_hierarchy_node(parent_id);
                        }
                        return true;
                    }
                }
                false
            }
            rsi_common::agent_projects::PROJECT_CREATED_EVENT => {
                match serde_json::from_value::<rsi_common::agent_projects::ProjectCreatedEventV1>(
                    event.data,
                ) {
                    Ok(parsed) => self.note_agent_created_project(parsed),
                    Err(_) => false,
                }
            }
            "child_spawned" => {
                #[derive(serde::Deserialize)]
                struct ChildSpawnedInner {
                    parent_epic_id: Uuid,
                    child_id: Uuid,
                    kind: rsi_common::types::SessionKind,
                }
                if let Ok(parsed) = serde_json::from_value::<ChildSpawnedInner>(event.data) {
                    // Invalidate hierarchy cache for child spawn
                    self.navigation_cache
                        .process_push_invalidation("child_spawned", Some(parsed.parent_epic_id));
                    self.push_notification(
                        crate::types::NotificationKind::Info,
                        crate::types::NotificationPriority::Low,
                        format!("Spawned {:?} under Epic", parsed.kind),
                        Some(parsed.child_id),
                    );
                    self.invalidate_hierarchy_node(Some(parsed.parent_epic_id));
                    return true;
                }
                false
            }
            "context_usage_updated" => {
                use rsi_common::types::ContextUsageConfidence;

                #[derive(serde::Deserialize)]
                #[allow(dead_code)]
                struct CtxInner {
                    session_id: Uuid,
                    input_tokens: u64,
                    output_tokens: u64,
                    daemon_total: u64,
                    confidence: ContextUsageConfidence,
                    #[serde(default)]
                    context_window: u64,
                    #[serde(default)]
                    pct: f64,
                    #[serde(default)]
                    resolved_context_budget:
                        Option<rsi_common::provider_capabilities::ResolvedContextBudget>,
                }
                if let Ok(parsed) = serde_json::from_value::<CtxInner>(event.data) {
                    if parsed.context_window > 0
                        && self.coordinator_context_budgets.iter().any(|seat| {
                            seat.session_id == parsed.session_id
                                && seat.budget.window.active_tokens != parsed.context_window
                        })
                    {
                        self.request_daemon_config_refresh();
                    }
                    if let Some(state) = self.sessions.get_mut(&parsed.session_id) {
                        let api_total = parsed.input_tokens;
                        let codex = matches!(
                            state.session.provider,
                            rsi_common::types::SessionProvider::Codex
                                | rsi_common::types::SessionProvider::Pioneer
                                | rsi_common::types::SessionProvider::OpenRouter
                                | rsi_common::types::SessionProvider::Bedrock
                                | rsi_common::types::SessionProvider::CodexAppServer
                        );
                        if api_total > 0 && !codex {
                            // High-watermark persisted token totals. The visible percentage
                            // below still comes directly from the daemon event.
                            let existing = state.session.total_input_tokens.unwrap_or(0);
                            state.session.total_input_tokens = Some(existing.max(api_total));
                        }
                        if parsed.daemon_total > 0 {
                            state.session.daemon_input_tokens = Some(parsed.daemon_total);
                            state.session.daemon_output_tokens = Some(0);
                        }
                        state.session.context_usage_confidence = parsed.confidence;
                        state.session.input_tokens = (!codex
                            || parsed.input_tokens > 0
                            || parsed.confidence != ContextUsageConfidence::Missing)
                            .then_some(parsed.input_tokens);
                        state.session.output_tokens = Some(parsed.output_tokens);
                        if parsed.context_window > 0 {
                            state.session.context_window = Some(parsed.context_window);
                        }
                        if let Some(resolved_context_budget) = parsed.resolved_context_budget {
                            state.session.resolved_context_budget = Some(resolved_context_budget);
                        }

                        // Explicit presence distinguishes measured zero from unknown.
                        // Clear the persisted projection too, so it cannot revive an old pct.
                        if codex {
                            let measured = parsed.confidence != ContextUsageConfidence::Missing;
                            state.live_context_pct =
                                (measured && parsed.pct.is_finite()).then_some(parsed.pct);
                            state.session.context_fill_pct = state.live_context_pct;
                        } else if parsed.pct > 0.0
                            || parsed.input_tokens > 0
                            || parsed.daemon_total > 0
                        {
                            state.live_context_pct = Some(parsed.pct);
                        }

                        return true;
                    }
                }
                false
            }
            // V99/P1-B live path. Account-scoped, so there is no session to
            // look up — the snapshot replaces whatever the cache held for that
            // provider (latest-wins, exactly as the daemon stores it).
            "provider_rate_limit_updated" => {
                #[derive(serde::Deserialize)]
                struct RateLimitInner {
                    snapshot: rsi_common::rpc::ProviderRateLimitSnapshot,
                }
                if let Ok(parsed) = serde_json::from_value::<RateLimitInner>(event.data) {
                    self.provider_rate_limits
                        .insert(parsed.snapshot.provider, parsed.snapshot);
                    return true;
                }
                false
            }
            "session_retrying" => {
                #[derive(serde::Deserialize)]
                struct RetryInner {
                    session_id: Uuid,
                    attempt: u8,
                    max_retries: u8,
                    backoff_ms: u64,
                    reason: String,
                }
                if let Ok(parsed) = serde_json::from_value::<RetryInner>(event.data) {
                    let d = &parsed;
                    // Update session retry metadata if we have it
                    if let Some(state) = self.sessions.get_mut(&d.session_id) {
                        state.session.retry_attempt = Some(d.attempt);
                        state.session.max_retries = Some(d.max_retries);
                    }
                    // Push notification
                    let label = self
                        .canonical_session_label(d.session_id, 30)
                        .unwrap_or_else(|| "Session".to_string());
                    self.push_notification(
                        crate::types::NotificationKind::Info,
                        crate::types::NotificationPriority::Medium,
                        format!(
                            "{}: retrying {}/{} in {}s ({})",
                            label,
                            d.attempt,
                            d.max_retries,
                            d.backoff_ms / 1000,
                            d.reason
                        ),
                        Some(d.session_id),
                    );
                    return true;
                }
                false
            }
            "graph_execution" => {
                #[derive(serde::Deserialize)]
                struct GraphInner {
                    update: rsi_common::types::GraphExecutionUpdate,
                }
                if let Ok(parsed) = serde_json::from_value::<GraphInner>(event.data) {
                    let status = parsed.update.status;
                    let workflow_label = self
                        .workflows
                        .get(&parsed.update.workflow_id)
                        .map(|workflow| workflow.title.clone())
                        .unwrap_or_else(|| "workflow".to_string());
                    let changed = self.apply_graph_execution_update(parsed.update);
                    if changed
                        && matches!(
                            status,
                            rsi_common::types::WorkflowExecutionStatus::Failed
                                | rsi_common::types::WorkflowExecutionStatus::Interrupted
                                | rsi_common::types::WorkflowExecutionStatus::Blocked
                        )
                    {
                        self.notify(format!("{}: {:?}", workflow_label, status));
                    }
                    return changed;
                }
                false
            }
            "subscription_reset" => {
                // Invalidate all cache data for subscription reset
                self.navigation_cache
                    .process_push_invalidation("subscription_reset", None);

                // Trigger immediate full poll to re-sync
                true
            }
            "session_stalled" => {
                #[derive(serde::Deserialize)]
                struct StallInner {
                    session_id: Uuid,
                    #[allow(dead_code)]
                    idle_secs: u64,
                }
                if let Ok(parsed) = serde_json::from_value::<StallInner>(event.data) {
                    let label = self
                        .canonical_session_label(parsed.session_id, 40)
                        .filter(|label| !label.is_empty())
                        .unwrap_or_else(|| "Session".to_string());
                    if let Some(state) = self.sessions.get_mut(&parsed.session_id) {
                        state.is_stalled = true;
                        self.push_notification(
                            crate::types::NotificationKind::SessionStalled,
                            crate::types::NotificationPriority::Medium,
                            format!("{}: stalled ({}m idle)", label, parsed.idle_secs / 60),
                            Some(parsed.session_id),
                        );
                        self.invalidate_session_browser_cache();
                        return true;
                    }
                }
                false
            }
            // Stall classifier verdict — RSI-0XX. Ambient notification only;
            // the nudge (if any) is dispatched daemon-side via
            // continue_session, which produces its own conversation events.
            "session_classified" => {
                #[derive(serde::Deserialize)]
                struct ClassifiedInner {
                    session_id: Uuid,
                    verdict: String,
                    action_taken: String,
                    #[serde(default)]
                    confidence: f64,
                }
                if let Ok(parsed) = serde_json::from_value::<ClassifiedInner>(event.data) {
                    let label = self
                        .canonical_session_label(parsed.session_id, 40)
                        .unwrap_or_else(|| parsed.session_id.to_string().chars().take(8).collect());
                    let pct = (parsed.confidence * 100.0).clamp(0.0, 100.0) as u32;
                    self.push_notification(
                        crate::types::NotificationKind::SessionClassified,
                        crate::types::NotificationPriority::Low,
                        format!(
                            "⚖ {}: {} → {} ({}%)",
                            label, parsed.verdict, parsed.action_taken, pct
                        ),
                        Some(parsed.session_id),
                    );
                    return true;
                }
                false
            }
            "session_summary_updated" => {
                #[derive(serde::Deserialize)]
                struct SummaryInner {
                    session_id: Uuid,
                    kind: rsi_common::types::SummaryKind,
                    content: String,
                }
                if let Ok(parsed) = serde_json::from_value::<SummaryInner>(event.data) {
                    // Only update in-memory state for short summaries (displayed in session list)
                    if parsed.kind == rsi_common::types::SummaryKind::Short {
                        if let Some(state) = self.sessions.get_mut(&parsed.session_id) {
                            state.session.short_summary = Some(parsed.content);
                            return true;
                        }
                    }
                }
                false
            }
            "session_reconciled" => {
                #[derive(serde::Deserialize)]
                struct ReconciledInner {
                    session_id: Uuid,
                    old_status: rsi_common::types::SessionStatus,
                    new_status: rsi_common::types::SessionStatus,
                    reason: serde_json::Value,
                }
                if let Ok(parsed) = serde_json::from_value::<ReconciledInner>(event.data) {
                    let reason_str = parsed.reason.as_str().unwrap_or("unknown").to_string();
                    let label = self
                        .canonical_session_label(parsed.session_id, 40)
                        .unwrap_or_else(|| parsed.session_id.to_string());
                    let msg = format!(
                        "{}: reconciled ({:?} → {:?}, {})",
                        label, parsed.old_status, parsed.new_status, reason_str
                    );
                    self.push_notification(
                        crate::types::NotificationKind::Info,
                        crate::types::NotificationPriority::Medium,
                        msg,
                        Some(parsed.session_id),
                    );
                    // Update session status in the local cache if present
                    if let Some(state) = self.sessions.get_mut(&parsed.session_id) {
                        state.session.status = parsed.new_status;
                    }
                    self.refresh_session_browser_order_after_focus_change();
                    return true;
                }
                false
            }
            "scheduled_job_fired" => {
                #[derive(serde::Deserialize)]
                struct Inner {
                    job_name: String,
                    session_id: uuid::Uuid,
                }
                if let Ok(parsed) = serde_json::from_value::<Inner>(event.data) {
                    let short_id = &parsed.session_id.to_string()[..8];
                    self.push_notification(
                        crate::types::NotificationKind::Info,
                        crate::types::NotificationPriority::Medium,
                        format!("Scheduled: '{}' fired -> {}", parsed.job_name, short_id),
                        Some(parsed.session_id),
                    );
                    return true;
                }
                false
            }
            // Daemon system messages (coordinator warnings, #669 manager
            // seat alerts) were previously dropped. Priority follows level.
            "system_message" => {
                #[derive(serde::Deserialize)]
                struct Inner {
                    level: String,
                    message: String,
                }
                if let Ok(parsed) = serde_json::from_value::<Inner>(event.data) {
                    let (kind, priority) = system_message_notification(&parsed.level);
                    self.push_notification(kind, priority, parsed.message, None);
                    return true;
                }
                false
            }
            "model_invocation_admitted"
            | "model_invocation_denied"
            | "model_budget_near_limit"
            | "model_control_mode_changed"
            | "model_control_circuit_changed"
            | "model_invocation_cancellation_requested"
            | "model_invocation_cancellation_skipped"
            | "model_invocation_cancelled"
            | "model_invocation_completed" => {
                self.pending_lc_actions
                    .push(crate::modalkit_types::LcAction::RefreshUsageStats);
                if crate::settings_keys::DAEMON_FEATURE_SECTIONS
                    .contains(&self.settings_state.section)
                    || self.settings_state.section
                        == crate::settings_registry::SettingsSection::MemoryDreaming
                {
                    self.pending_lc_actions
                        .push(crate::modalkit_types::LcAction::RefreshDaemonFeatures);
                }
                true
            }
            _ => false,
        };

        // Process any pending cache invalidations from this push event
        self.navigation_cache.apply_pending_invalidations();

        result
    }

    /// Re-derive docregblock buttons from workflow stage for all workflow sessions.
    /// Called after workflow polling to catch stage changes that happen independently
    /// of conversation events (e.g. daemon advances stage after session completes).
    pub(crate) fn refresh_workflow_buttons(&mut self) -> bool {
        let mut changed = false;
        // P1.3: pre-collect (session_id, effective_topology) so the helper's
        // immutable `&self.sessions` borrow doesn't conflict with the
        // `values_mut()` mutable borrow below. Total cost: O(N) sessions for
        // the pre-collect (helper is O(depth) ≤ 16, realistic ≤ 5) plus O(N)
        // for the mutation loop — same big-O as the pre-P1.3 direct-read.
        let topo_by_session: Vec<(Uuid, Option<Uuid>)> = self
            .sessions
            .keys()
            .copied()
            .map(|sid| (sid, self.effective_topology(sid)))
            .collect();
        for (sid, wf_id_opt) in topo_by_session {
            let wf_id = match wf_id_opt {
                Some(id) => id,
                None => continue,
            };
            let workflow = match self.workflows.get(&wf_id) {
                Some(wf) => wf,
                None => continue,
            };
            let mut commands = Vec::new();
            match workflow.stage {
                rsi_common::types::WorkflowStage::ResearchComplete => {
                    if let Some(ref path) = workflow.artifact_path {
                        commands.push(format!("/plan @{}", path));
                    }
                }
                rsi_common::types::WorkflowStage::PlanComplete => {
                    if let Some(ref path) = workflow.artifact_path {
                        commands.push(format!("/implement @{}", path));
                    }
                }
                rsi_common::types::WorkflowStage::ImplementComplete => {
                    commands.push("/merge_ready".to_string());
                }
                _ => {}
            }
            if let Some(state) = self.sessions.get_mut(&sid)
                && state.docregblock_contents != commands
            {
                state.docregblock_contents = commands;
                changed = true;
            }
        }
        changed
    }
}

/// Map a daemon `system_message` level to a notification treatment:
/// `error` is High (10 s, emphasised), `warn` Medium, anything else Low.
fn system_message_notification(
    level: &str,
) -> (
    crate::types::NotificationKind,
    crate::types::NotificationPriority,
) {
    use crate::types::{NotificationKind, NotificationPriority};
    match level {
        "error" => (
            NotificationKind::OperationFailed,
            NotificationPriority::High,
        ),
        "warn" | "warning" => (NotificationKind::Info, NotificationPriority::Medium),
        _ => (NotificationKind::Info, NotificationPriority::Low),
    }
}

#[cfg(test)]
mod worker_pressure_refresh_tests {
    use super::super::App;
    use crate::client::DaemonClient;

    #[tokio::test]
    async fn local_build_warning_refreshes_and_clears_through_background_rpc() {
        use serde_json::{Value, json};
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let socket_path = crate::test_support::short_socket_path("local-build");
        let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();
        let server = tokio::spawn(async move {
            for step in 0..5 {
                let (stream, _) = listener.accept().await.unwrap();
                let (reader, mut writer) = stream.into_split();
                let mut reader = BufReader::new(reader);
                for method in ["GetHealthStatus", "GetOperatorRestart", "GetDaemonInfo"] {
                    let mut line = String::new();
                    reader.read_line(&mut line).await.unwrap();
                    let request: Value = serde_json::from_str(&line).unwrap();
                    assert_eq!(request["method"], method);
                    let result = match method {
                        "GetHealthStatus" => json!({
                            "persistence_queue_depth": 0, "persistence_queue_capacity": 1,
                            "last_command_duration_ms": 0, "project_cache_size": 0,
                        }),
                        "GetOperatorRestart" => json!({
                            "pending": false, "forced": false, "blockers": [],
                            "turns_in_flight": 0, "supervised": true,
                        }),
                        _ => json!({
                            "build_sha": "a".repeat(40), "started_at": "2026-10-19T00:00:00.000000000Z",
                            "uptime_secs": 1, "schema_version": 1, "disk_free": {},
                            "local_build": {
                                "sha": if step < 2 { "b".repeat(40) } else { "c".repeat(40) },
                                "path": "/fixture/rsid", "built_at": "2026-10-19T01:00:00.000000000Z",
                                "differs": step < 3,
                            },
                        }),
                    };
                    let response = if method == "GetDaemonInfo" && (step == 1 || step == 4) {
                        json!({"jsonrpc": "2.0", "id": request["id"], "error": {
                            "code": if step == 1 { -32603 } else { -32601 }, "message": "fixture unavailable",
                        }})
                    } else {
                        json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
                    };
                    writer
                        .write_all(format!("{response}\n").as_bytes())
                        .await
                        .unwrap();
                }
            }
        });
        let mut app = App::new(DaemonClient::new(socket_path.clone()));
        for expected in [
            Some("bbbbbbbbb"),
            Some("bbbbbbbbb"),
            Some("ccccccccc"),
            None,
            None,
        ] {
            let read = super::fetch_worker_pressure(socket_path.clone())
                .await
                .unwrap();
            let task = tokio::spawn(async move { Ok(read) });
            // Synchronize completion so the normal input-loop poll can consume it.
            while !task.is_finished() {
                tokio::task::yield_now().await;
            }
            app.worker_pressure_refresh_handle = Some(task);
            app.poll_worker_pressure_refresh().await;
            match expected {
                Some(sha) => assert!(app.local_build_warning.as_ref().unwrap().contains(sha)),
                None => assert_eq!(app.local_build_warning, None),
            }
        }
        server.await.unwrap();
        std::fs::remove_file(socket_path).unwrap();
    }

    #[tokio::test]
    async fn held_health_read_keeps_one_background_refresh_and_returns_promptly() {
        let temp_dir = crate::test_support::socket_tempdir().expect("temporary socket directory");
        let socket_path = temp_dir.path().join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&socket_path).expect("socket listener");
        let mut app = App::new(DaemonClient::new(socket_path));
        app.poll.connected = true;

        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            app.poll_worker_pressure_refresh(),
        )
        .await
        .expect("refresh schedules without waiting for Health");
        let first_id = app
            .worker_pressure_refresh_handle
            .as_ref()
            .expect("background Health task")
            .id();
        let (_held_stream, _) =
            tokio::time::timeout(std::time::Duration::from_secs(1), listener.accept())
                .await
                .expect("background connection starts")
                .expect("accept background connection");

        app.worker_pressure_next_refresh_at = std::time::Instant::now();
        app.poll_worker_pressure_refresh().await;
        assert_eq!(
            app.worker_pressure_refresh_handle
                .as_ref()
                .expect("single background Health task")
                .id(),
            first_id
        );
    }
}

#[cfg(test)]
mod system_message_tests {
    use super::super::App;
    use crate::client::DaemonClient;
    use crate::types::NotificationPriority;
    use std::path::PathBuf;

    #[test]
    #[allow(clippy::unwrap_used)]
    fn system_message_manager_seat_raises_high_notification() {
        crate::state::DevState::clear();
        crate::state::PersistedState::default().save();
        let mut app = App::new(DaemonClient::new(PathBuf::from(
            "/tmp/test-system-message.sock",
        )));
        let message = "[manager-seat] Harness manager p: seat down: manager m Failed (manager_seat_recovery_disabled).";
        let redraw = app.apply_push_event(rsi_common::rpc::BusEvent {
            event_type: "system_message".into(),
            timestamp: chrono::Utc::now(),
            data: serde_json::json!({"level": "error", "message": message}),
        });
        assert!(redraw);
        let last = app.notifications.back().unwrap();
        assert_eq!(last.priority, NotificationPriority::High);
        assert_eq!(last.message, message);
        app.apply_push_event(rsi_common::rpc::BusEvent {
            event_type: "system_message".into(),
            timestamp: chrono::Utc::now(),
            data: serde_json::json!({"level": "info", "message": "[manager-seat] seat recovered"}),
        });
        let last = app.notifications.back().unwrap();
        assert_eq!(last.priority, NotificationPriority::Low);
        assert_eq!(last.message, "[manager-seat] seat recovered");
    }
}

#[cfg(test)]
mod conversation_poll_filter_tests {
    //! Tests for the container-kind filter applied to
    //! `active_running_session_ids` (the fallback catch-up poll list).
    //!
    //! The contract: Group/Epic containers MUST NOT appear in the
    //! conversation-fetch poll lists, regardless of their status (Running,
    //! Completed, etc.) or visibility. Leaf kinds (Story, Standard, etc.)
    //! continue to be included via the usual active/visible/filtered rules.
    //! This makes the periodic poll cycle consistent with the focus-fetch
    //! effect, which already short-circuits on `!is_leaf_kind`.
    //!
    //! Together with the existing focus-fetch on selection change
    //! (`trigger_focus_fetch_if_needed` in `app/navigation.rs`), this gives a
    //! useEffect-style "fetch when navigation node changes" pattern for
    //! leaves AND a zero-cost no-op for containers — instantaneous view
    //! switch with no redundant network request.
    use super::super::App;
    use crate::client::DaemonClient;
    use crate::poll_controller::PollPhase;
    use crate::types::{OverlayState, Pane, SessionState};
    use rsi_common::types::{Session, SessionKind, SessionProvider, SessionStatus};
    use std::path::PathBuf;
    use uuid::Uuid;

    fn test_app() -> App {
        crate::state::DevState::clear();
        crate::state::PersistedState::default().save();
        App::new(DaemonClient::new(PathBuf::from(
            "/tmp/test-poll-filter.sock",
        )))
    }

    fn mk_session(id: Uuid, kind: SessionKind, status: SessionStatus) -> Session {
        Session {
            context_fill_pct: None,
            id,
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
            status,
            project_id: None,
            session_kind: kind,
            pinned_at: None,
            testing_needed_at: None,
            rotation_disabled_at: None,
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
            total_prompt_tokens: None,
            total_output_tokens: None,
            total_cache_creation_tokens: None,
            total_cache_read_tokens: None,
            stop_reason: None,
            context_usage_confidence: rsi_common::types::ContextUsageConfidence::default(),
            continued_from: None,
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

    fn select_main_session(app: &mut App, session_id: Uuid) {
        let selected_index = app
            .filtered_session_order
            .iter()
            .position(|id| *id == session_id)
            .expect("selected fixture session must be visible");
        let Some(Pane::SessionList {
            selected_index: pane_index,
            selected_session,
            ..
        }) = app.focused_pane_mut()
        else {
            panic!("fixture must focus a session-list pane");
        };
        *pane_index = selected_index;
        *selected_session = Some(session_id);
    }

    fn assert_main_selection(app: &App, expected_id: Uuid) {
        let expected_index = app
            .filtered_session_order
            .iter()
            .position(|id| *id == expected_id)
            .expect("selected fixture session must remain visible");
        let Some(Pane::SessionList {
            selected_index,
            selected_session,
            ..
        }) = app.focused_pane()
        else {
            panic!("fixture must focus a session-list pane");
        };
        assert_eq!(*selected_session, Some(expected_id));
        assert_eq!(*selected_index, expected_index);
    }

    #[test]
    fn poll_conversation_ids_excludes_group() {
        let mut app = test_app();
        let group_id = Uuid::new_v4();
        app.sessions.insert(
            group_id,
            SessionState::new(mk_session(
                group_id,
                SessionKind::Group,
                SessionStatus::Running,
            )),
        );
        app.filtered_session_order.push(group_id);

        let ids = app.active_running_session_ids();
        assert!(
            !ids.contains(&group_id),
            "Group containers must be filtered out of conversation polling"
        );
    }

    #[test]
    fn push_session_created_inserts_session_list_entry() {
        let mut app = test_app();
        let existing_id = Uuid::new_v4();
        app.update_sessions(vec![mk_session(
            existing_id,
            SessionKind::Standard,
            SessionStatus::Completed,
        )]);
        let session_id = Uuid::new_v4();
        let session = mk_session(session_id, SessionKind::Standard, SessionStatus::Starting);
        let event = rsi_common::rpc::BusEvent {
            event_type: "session_created".to_string(),
            timestamp: chrono::Utc::now(),
            data: serde_json::json!({ "session": session }),
        };

        assert!(app.apply_push_event(event));
        assert!(app.sessions.contains_key(&session_id));
        assert!(app.session_order.contains(&session_id));
        assert!(app.sessions.contains_key(&existing_id));
        assert!(app.session_order.contains(&existing_id));
        assert_eq!(app.sessions.len(), 2);
        assert_eq!(app.session_order.len(), 2);
    }

    #[test]
    fn push_context_usage_accepts_zero_pct_with_token_signal() {
        let mut app = test_app();
        let session_id = Uuid::new_v4();
        app.update_sessions(vec![mk_session(
            session_id,
            SessionKind::Standard,
            SessionStatus::Running,
        )]);

        let event = rsi_common::rpc::BusEvent {
            event_type: "context_usage_updated".to_string(),
            timestamp: chrono::Utc::now(),
            data: serde_json::json!({
                "session_id": session_id,
                "pct": 0.0,
                "input_tokens": 12_000,
                "output_tokens": 0,
                "daemon_total": 0,
                "confidence": rsi_common::types::ContextUsageConfidence::Full,
                "context_window": 258_400,
            }),
        };

        assert!(app.apply_push_event(event));
        let state = app.sessions.get(&session_id).unwrap();
        assert_eq!(state.live_context_pct, Some(0.0));
        assert_eq!(state.session.context_window, Some(258_400));
        assert_eq!(state.session.total_input_tokens, Some(12_000));
    }

    #[test]
    fn push_model_control_event_queues_refresh_action() {
        let mut app = test_app();
        let event = rsi_common::rpc::BusEvent {
            event_type: "model_control_mode_changed".to_string(),
            timestamp: chrono::Utc::now(),
            data: serde_json::json!({
                "previous_mode": "normal",
                "current_mode": "stop_all",
                "updated_at": chrono::Utc::now(),
            }),
        };

        assert!(app.apply_push_event(event));
        assert!(
            app.pending_lc_actions
                .contains(&crate::modalkit_types::LcAction::RefreshUsageStats)
        );
    }

    #[test]
    fn push_model_control_circuit_event_refreshes_stats_and_features() {
        let mut app = test_app();
        app.settings_state.section = crate::settings_registry::SettingsSection::ModelControl;
        let event = rsi_common::rpc::BusEvent {
            event_type: "model_control_circuit_changed".to_string(),
            timestamp: chrono::Utc::now(),
            data: serde_json::json!({
                "scope_kind": "provider",
                "scope_id": "claude",
                "state": "open",
                "reason": "quota_exhausted",
            }),
        };

        assert!(app.apply_push_event(event));
        assert!(
            app.pending_lc_actions
                .contains(&crate::modalkit_types::LcAction::RefreshUsageStats)
        );
        assert!(
            app.pending_lc_actions
                .contains(&crate::modalkit_types::LcAction::RefreshDaemonFeatures)
        );
    }

    #[test]
    fn push_session_question_raised_populates_pending_question() {
        let mut app = test_app();
        let session_id = Uuid::new_v4();
        let session = mk_session(session_id, SessionKind::Standard, SessionStatus::Running);
        app.update_sessions(vec![session]);

        let question_json = serde_json::json!({
            "questions": [
                {
                    "question": "Choose one?",
                    "header": "Question",
                    "options": [
                        {
                            "label": "Yes",
                            "description": "Proceed"
                        }
                    ],
                    "multiSelect": false
                }
            ]
        });

        let event = rsi_common::rpc::BusEvent {
            event_type: "session_question_raised".to_string(),
            timestamp: chrono::Utc::now(),
            data: serde_json::json!({
                "session_id": session_id,
                "question": question_json
            }),
        };

        assert!(app.apply_push_event(event));
        let state = app.sessions.get(&session_id).unwrap();
        assert!(state.session.pending_question.is_some());
        assert_eq!(
            state.session.status,
            rsi_common::types::SessionStatus::WaitingApproval
        );
    }

    #[test]
    fn question_push_reorders_groups_same_redraw_and_preserves_selected_uuid() {
        let mut app = test_app();
        let question_id = Uuid::new_v4();
        let running_id = Uuid::new_v4();
        let selected_recent_id = Uuid::new_v4();

        let mut question_target =
            mk_session(question_id, SessionKind::Standard, SessionStatus::Completed);
        question_target.updated_at = chrono::Utc::now() - chrono::Duration::days(20);
        let running = mk_session(running_id, SessionKind::Standard, SessionStatus::Running);
        let mut recent = mk_session(
            selected_recent_id,
            SessionKind::Standard,
            SessionStatus::Completed,
        );
        recent.updated_at = chrono::Utc::now() - chrono::Duration::days(2);
        app.update_sessions(vec![question_target, running, recent]);
        assert_eq!(
            app.filtered_session_order,
            vec![running_id, selected_recent_id, question_id]
        );
        select_main_session(&mut app, selected_recent_id);

        let event = rsi_common::rpc::BusEvent {
            event_type: "session_question_raised".to_string(),
            timestamp: chrono::Utc::now(),
            data: serde_json::json!({
                "session_id": question_id,
                "question": {
                    "questions": [{
                        "question": "Choose the relay policy?",
                        "header": "Policy",
                        "options": [],
                        "multiSelect": false
                    }]
                }
            }),
        };

        assert!(app.apply_push_event(event));
        assert_eq!(
            app.filtered_session_order,
            vec![question_id, running_id, selected_recent_id],
            "the raised question must enter Needs You before lower-priority groups immediately"
        );
        assert_main_selection(&app, selected_recent_id);
    }

    #[test]
    fn archived_manager_status_requests_roster_refresh() {
        let mut app = test_app();
        let manager_id = Uuid::new_v4();
        app.update_sessions(vec![mk_session(
            manager_id,
            SessionKind::Standard,
            SessionStatus::Running,
        )]);
        app.manager_roster.by_project.insert(
            Uuid::new_v4(),
            super::super::manager_roster::ManagerRosterEntry {
                session_id: manager_id,
                tier: super::super::manager_roster::ManagerTier::Project,
            },
        );
        let event = rsi_common::rpc::BusEvent {
            event_type: "session_status_changed".into(),
            timestamp: chrono::Utc::now(),
            data: serde_json::json!({
                "session_id": manager_id,
                "new_status": "Archived"
            }),
        };
        assert!(app.apply_push_event(event));
        assert!(app.manager_roster.take_refresh());
    }

    #[test]
    fn status_pushes_reorder_groups_same_redraw_and_preserve_selected_uuid() {
        for event_type in ["session_status_changed", "session_reconciled"] {
            let mut app = test_app();
            let waiting_id = Uuid::new_v4();
            let promoted_id = Uuid::new_v4();
            let selected_recent_id = Uuid::new_v4();

            let waiting = mk_session(
                waiting_id,
                SessionKind::Standard,
                SessionStatus::WaitingApproval,
            );
            let mut promoted =
                mk_session(promoted_id, SessionKind::Standard, SessionStatus::Completed);
            promoted.updated_at = chrono::Utc::now() - chrono::Duration::days(20);
            let mut recent = mk_session(
                selected_recent_id,
                SessionKind::Standard,
                SessionStatus::Completed,
            );
            recent.updated_at = chrono::Utc::now() - chrono::Duration::days(2);
            app.update_sessions(vec![promoted, recent, waiting]);
            assert_eq!(
                app.filtered_session_order,
                vec![waiting_id, selected_recent_id, promoted_id]
            );
            select_main_session(&mut app, selected_recent_id);

            let data = if event_type == "session_status_changed" {
                serde_json::json!({
                    "session_id": promoted_id,
                    "new_status": "Running"
                })
            } else {
                serde_json::json!({
                    "session_id": promoted_id,
                    "old_status": "Completed",
                    "new_status": "Running",
                    "reason": "provider recovered"
                })
            };
            let event = rsi_common::rpc::BusEvent {
                event_type: event_type.to_string(),
                timestamp: chrono::Utc::now(),
                data,
            };

            assert!(app.apply_push_event(event), "{event_type}");
            assert_eq!(
                app.filtered_session_order,
                vec![waiting_id, promoted_id, selected_recent_id],
                "{event_type} must move the running session into In Flight immediately"
            );
            assert_main_selection(&app, selected_recent_id);
        }
    }

    #[test]
    fn push_mutations_invalidate_container_focus_and_activity_same_redraw() {
        let mut app = test_app();
        let parent_id = Uuid::new_v4();
        let waiting_child_id = Uuid::new_v4();
        let running_child_id = Uuid::new_v4();
        let mut parent = mk_session(parent_id, SessionKind::Epic, SessionStatus::Completed);
        parent.title = Some("Cached parent".into());
        let mut waiting_child = mk_session(
            waiting_child_id,
            SessionKind::Task,
            SessionStatus::Completed,
        );
        waiting_child.parent_id = Some(parent_id);
        let mut running_child = mk_session(
            running_child_id,
            SessionKind::Task,
            SessionStatus::Completed,
        );
        running_child.parent_id = Some(parent_id);
        app.update_sessions(vec![parent, waiting_child, running_child]);
        app.refresh_session_focus_index();
        app.refresh_session_activity();
        assert_eq!(
            app.session_list_render.focus_index[&parent_id].attention_count,
            0
        );
        let primed_generation = app.session_list_render.focus_generation;

        let question = rsi_common::rpc::BusEvent {
            event_type: "session_question_raised".to_string(),
            timestamp: chrono::Utc::now(),
            data: serde_json::json!({
                "session_id": waiting_child_id,
                "question": {
                    "questions": [{
                        "question": "Choose a bounded policy?",
                        "header": "Policy",
                        "options": [],
                        "multiSelect": false
                    }]
                }
            }),
        };
        assert!(app.apply_push_event(question));
        assert_eq!(
            app.session_list_render.focus_generation, app.card_generation,
            "question push must rebuild the invalidated focus cache before returning"
        );
        assert_eq!(
            app.session_list_render.focus_generation, primed_generation,
            "an in-place push keeps card generation stable while refreshing its projection"
        );
        assert_eq!(
            app.session_list_render.focus_index[&parent_id].group,
            crate::types::SessionFocusGroup::NeedsYou
        );
        assert_eq!(
            app.session_list_render.focus_index[&parent_id].attention_count,
            1
        );

        let status = rsi_common::rpc::BusEvent {
            event_type: "session_status_changed".to_string(),
            timestamp: chrono::Utc::now(),
            data: serde_json::json!({
                "session_id": running_child_id,
                "new_status": "Running"
            }),
        };
        assert!(app.apply_push_event(status));
        app.refresh_session_activity();
        assert_eq!(
            app.session_list_render.focus_index[&parent_id].active_count,
            1
        );
        assert!(
            app.session_list_render
                .activity
                .as_ref()
                .is_some_and(|activity| activity.flow.needs_you >= 1)
        );
    }

    #[test]
    fn push_session_question_raised_auto_opens_when_enabled() {
        let mut app = test_app();
        app.settings.auto_open_question_modal = true;
        let session_id = Uuid::new_v4();
        let session = mk_session(session_id, SessionKind::Standard, SessionStatus::Running);
        app.update_sessions(vec![session]);

        let event = rsi_common::rpc::BusEvent {
            event_type: "session_question_raised".to_string(),
            timestamp: chrono::Utc::now(),
            data: serde_json::json!({
                "session_id": session_id,
                "question": {
                    "questions": [
                        {
                            "question": "Choose one?",
                            "header": "Question",
                            "options": [
                                { "label": "Yes", "description": "Proceed" }
                            ],
                            "multiSelect": false
                        }
                    ]
                }
            }),
        };

        assert!(app.apply_push_event(event));
        match &app.overlay {
            OverlayState::QuestionModal {
                session_id: modal_session_id,
                questions,
                ..
            } => {
                assert_eq!(*modal_session_id, session_id);
                assert_eq!(questions[0].question, "Choose one?");
            }
            _ => panic!("expected question modal"),
        }
    }

    #[test]
    fn push_session_question_raised_does_not_auto_open_over_active_overlay() {
        let mut app = test_app();
        app.settings.auto_open_question_modal = true;
        app.overlay = OverlayState::Diagnostics;
        let session_id = Uuid::new_v4();
        let session = mk_session(session_id, SessionKind::Standard, SessionStatus::Running);
        app.update_sessions(vec![session]);

        let event = rsi_common::rpc::BusEvent {
            event_type: "session_question_raised".to_string(),
            timestamp: chrono::Utc::now(),
            data: serde_json::json!({
                "session_id": session_id,
                "question": {
                    "questions": [
                        {
                            "question": "Choose one?",
                            "header": "Question",
                            "options": [],
                            "multiSelect": false
                        }
                    ]
                }
            }),
        };

        assert!(app.apply_push_event(event));
        assert!(matches!(app.overlay, OverlayState::Diagnostics));
        assert!(
            app.sessions
                .get(&session_id)
                .unwrap()
                .session
                .pending_question
                .is_some()
        );
    }

    #[test]
    fn poll_conversation_ids_excludes_epic() {
        let mut app = test_app();
        let epic_id = Uuid::new_v4();
        app.sessions.insert(
            epic_id,
            SessionState::new(mk_session(
                epic_id,
                SessionKind::Epic,
                SessionStatus::Running,
            )),
        );
        app.filtered_session_order.push(epic_id);

        let ids = app.active_running_session_ids();
        assert!(
            !ids.contains(&epic_id),
            "Epic containers must be filtered out of conversation polling"
        );
    }

    #[test]
    fn poll_conversation_ids_includes_story_leaf() {
        let mut app = test_app();
        let story_id = Uuid::new_v4();
        app.sessions.insert(
            story_id,
            SessionState::new(mk_session(
                story_id,
                SessionKind::Story,
                SessionStatus::Running,
            )),
        );
        app.filtered_session_order.push(story_id);

        let ids = app.active_running_session_ids();
        assert!(
            ids.contains(&story_id),
            "Story leaves MUST remain included (they have conversation events)"
        );
    }

    #[test]
    fn poll_conversation_ids_excludes_container_even_when_active() {
        // A container in Running status still must not be polled — containers
        // never produce conversation events regardless of status.
        let mut app = test_app();
        let group_id = Uuid::new_v4();
        app.sessions.insert(
            group_id,
            SessionState::new(mk_session(
                group_id,
                SessionKind::Group,
                SessionStatus::Running,
            )),
        );
        // Deliberately don't add to filtered_session_order — only the
        // "is_active" predicate would otherwise pick it up. Container filter
        // must still win.
        let ids = app.active_running_session_ids();
        assert!(
            !ids.contains(&group_id),
            "Container filter must beat the is_active inclusion rule"
        );
    }

    #[test]
    fn poll_conversation_ids_excludes_sessions_with_inflight_focus_fetch() {
        let mut app = test_app();
        let story_id = Uuid::new_v4();
        app.sessions.insert(
            story_id,
            SessionState::new(mk_session(
                story_id,
                SessionKind::Story,
                SessionStatus::Running,
            )),
        );
        app.filtered_session_order.push(story_id);

        // Pre-mark as having inflight focus fetch
        app.focus_fetch_inflight.insert(story_id);

        let ids = app.active_running_session_ids();
        assert!(
            !ids.contains(&story_id),
            "Sessions with inflight focus fetch must be excluded from polling to avoid redundant requests"
        );
    }

    #[tokio::test]
    async fn fallback_conversation_disconnect_restarts_shared_bootstrap() {
        let mut app = test_app();
        app.poll.connected = true;
        app.poll.authoritative_config_ready = true;
        app.conversation_poll_active_generation = Some(1);
        let changed = app.apply_conversation_poll_result(crate::app::ConversationPollResult {
            attempt: app.bootstrap.attempt(),
            generation: 1,
            batches: Err("connection closed by daemon".to_string()),
            batch_unsupported: false,
            connection_lost: true,
            timed_out: false,
        });

        assert!(!changed);
        assert!(!app.poll.connected);
        assert!(!app.poll.authoritative_config_ready);
        assert!(app.bootstrap.handshake_in_flight());
    }

    #[tokio::test]
    async fn initial_hydration_schedules_selected_completed_leaf() {
        let mut app = crate::app::app_test_helpers::with_session_list(1);
        let selected = app.selected_session_id().expect("selected fixture leaf");
        app.sessions.get_mut(&selected).unwrap().session.status = SessionStatus::Completed;

        app.start_initial_conversation_hydration();

        assert!(app.conversation_poll_handle.is_some());
    }

    #[tokio::test]
    async fn initial_hydration_includes_active_unloaded_leaf_with_selected_completed_leaf() {
        let mut app = crate::app::app_test_helpers::with_session_list(2);
        let selected = app.selected_session_id().expect("selected fixture leaf");
        app.sessions.get_mut(&selected).unwrap().session.status = SessionStatus::Completed;
        let active = app.filtered_session_order[1];
        app.sessions.get_mut(&active).unwrap().session.status = SessionStatus::Running;
        app.conversation_poll_handle = Some(tokio::spawn(std::future::pending()));

        app.start_initial_conversation_hydration();

        let retained = app
            .conversation_poll_hydration_pending
            .as_ref()
            .expect("initial hydration retains its bounded batch behind live work");
        assert!(retained.iter().any(|cursor| cursor.session_id == selected));
        assert!(retained.iter().any(|cursor| cursor.session_id == active));
        app.conversation_poll_handle
            .take()
            .expect("held task")
            .abort();
    }

    #[tokio::test]
    async fn initial_hydration_advances_every_mixed_leaf_exactly_once_in_bounded_chunks() {
        let mut app = crate::app::app_test_helpers::with_session_list(6);
        app.poll.connected = true;
        let selected = app.selected_session_id().expect("selected completed leaf");
        app.sessions.get_mut(&selected).unwrap().session.status = SessionStatus::Completed;
        let expected: std::collections::HashSet<_> = app
            .sessions
            .iter()
            .filter_map(|(id, state)| {
                (state.session.status == SessionStatus::Running || *id == selected).then_some(*id)
            })
            .collect();
        assert!(expected.len() > super::super::CONVERSATION_BATCH_SIZE);

        app.start_initial_conversation_hydration();
        let mut dispatched = Vec::new();
        while let Some(generation) = app.conversation_poll_active_generation {
            let batch = app.conversation_poll_active_cursors.clone();
            assert!(batch.len() <= super::super::CONVERSATION_BATCH_SIZE);
            dispatched.extend(batch.into_iter().map(|cursor| cursor.session_id));
            app.conversation_poll_handle
                .take()
                .expect("owned hydration task")
                .abort();
            app.apply_conversation_poll_result(crate::app::ConversationPollResult {
                attempt: app.bootstrap.attempt(),
                generation,
                batches: Ok(Vec::new()),
                batch_unsupported: false,
                connection_lost: false,
                timed_out: false,
            });
        }

        let unique: std::collections::HashSet<_> = dispatched.iter().copied().collect();
        assert_eq!(unique, expected);
        assert_eq!(
            dispatched.len(),
            unique.len(),
            "every target is dispatched once"
        );
        assert!(app.conversation_poll_hydration_pending.is_none());
    }

    #[tokio::test]
    async fn stale_conversation_result_cannot_apply_across_reconnect_attempts() {
        let mut app = test_app();
        app.poll.connected = true;
        let stale_attempt = app.bootstrap.attempt();
        app.conversation_poll_active_generation = Some(1);
        app.start_bootstrap();
        assert!(app.bootstrap.attempt() > stale_attempt);

        let applied = app.apply_conversation_poll_result(crate::app::ConversationPollResult {
            attempt: stale_attempt,
            generation: 1,
            batches: Ok(Vec::new()),
            batch_unsupported: false,
            connection_lost: false,
            timed_out: false,
        });
        assert!(!applied);
        assert!(app.bootstrap.handshake_in_flight());
    }

    #[tokio::test]
    async fn queued_old_generation_cannot_clear_new_task_and_app_drop_aborts_survivor() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };

        struct CancellationProbe(Arc<AtomicBool>);
        impl Drop for CancellationProbe {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let cancelled = Arc::new(AtomicBool::new(false));
        let mut app = test_app();
        app.poll.connected = true;
        app.conversation_poll_generation = 2;
        app.conversation_poll_active_generation = Some(2);
        let probe = Arc::clone(&cancelled);
        app.conversation_poll_handle = Some(tokio::spawn(async move {
            let _probe = CancellationProbe(probe);
            std::future::pending::<()>().await;
        }));
        tokio::task::yield_now().await;

        let applied = app.apply_conversation_poll_result(crate::app::ConversationPollResult {
            attempt: app.bootstrap.attempt(),
            generation: 1,
            batches: Ok(Vec::new()),
            batch_unsupported: false,
            connection_lost: false,
            timed_out: false,
        });
        assert!(!applied);
        assert_eq!(app.conversation_poll_active_generation, Some(2));
        assert!(app.conversation_poll_handle.is_some());

        drop(app);
        tokio::task::yield_now().await;
        assert!(cancelled.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn slow_method_timeout_preserves_healthy_transport_readiness() {
        let temp_dir = crate::test_support::socket_tempdir().expect("temporary socket directory");
        let socket_path = temp_dir.path().join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&socket_path).expect("socket listener");
        let held_server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("poll client connects");
            let _held_stream = stream;
            std::future::pending::<()>().await;
        });
        let session_id = Uuid::new_v4();
        let result = super::run_background_conversation_poll_with_timeout(
            socket_path.clone(),
            0,
            1,
            vec![rsi_common::rpc::ConversationFetchCursor {
                session_id,
                since_sequence: None,
            }],
            true,
            std::time::Duration::from_millis(25),
        )
        .await;
        assert!(!result.connection_lost);

        let mut app = App::new(DaemonClient::new(socket_path));
        app.poll.connected = true;
        app.poll.authoritative_config_ready = true;
        app.poll.sessions_authoritative = true;
        app.conversation_poll_active_generation = Some(1);
        assert!(app.apply_conversation_poll_result(result));
        assert!(app.poll.connected);
        assert!(app.poll.authoritative_config_ready);
        assert!(app.poll.sessions_authoritative);
        assert!(!app.bootstrap.handshake_in_flight());
        assert!(
            app.notifications
                .back()
                .unwrap()
                .message
                .starts_with("Daemon busy (no reply in")
        );
        held_server.abort();
    }

    #[tokio::test]
    async fn session_refresh_timeout_keeps_connection_and_next_action_succeeds() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let dir = crate::test_support::socket_tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut lines = BufReader::new(read).lines();
            let slow: serde_json::Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(slow["method"], "ListSessions");
            let next: serde_json::Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(next["method"], "ListProjects");
            // Coalesce the timed-out reply and the next action's reply. Use
            // different result shapes so accepting the wrong id cannot pass.
            let stale =
                serde_json::json!({"jsonrpc":"2.0", "id":slow["id"], "result":"stale sessions"});
            let fresh = serde_json::json!({"jsonrpc":"2.0", "id":next["id"], "result":[]});
            write
                .write_all(format!("{stale}\n{fresh}\n").as_bytes())
                .await
                .unwrap();
        });
        let mut app = App::new(DaemonClient::new(socket));
        app.client.connect().await.unwrap();
        app.poll.connected = true;
        app.poll.authoritative_config_ready = true;
        app.poll.sessions_authoritative = true;
        tokio::time::timeout(std::time::Duration::from_secs(5), app.poll_sessions())
            .await
            .unwrap();
        assert!(app.client.is_connected());
        assert!(app.poll.connected);
        assert!(app.poll.authoritative_config_ready);
        assert!(app.poll.sessions_authoritative);
        assert_eq!(
            app.notifications.back().unwrap().message,
            "Daemon busy (no reply in 3 s); try again"
        );
        let projects = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            app.client.list_projects(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(projects.is_empty());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn fallback_reply_eof_withdraws_readiness_and_starts_reconnect() {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let dir = crate::test_support::socket_tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut lines = BufReader::new(stream).lines();
            lines.next_line().await.unwrap().unwrap();
        });
        let result = super::run_background_conversation_poll_with_timeout(
            socket.clone(),
            0,
            1,
            vec![rsi_common::rpc::ConversationFetchCursor {
                session_id: Uuid::new_v4(),
                since_sequence: None,
            }],
            true,
            std::time::Duration::from_secs(1),
        )
        .await;
        assert!(result.connection_lost);
        let mut app = App::new(DaemonClient::new(socket));
        app.poll.connected = true;
        app.poll.authoritative_config_ready = true;
        app.conversation_poll_active_generation = Some(1);
        app.apply_conversation_poll_result(result);
        assert!(!app.client.is_connected());
        assert!(app.bootstrap.handshake_in_flight());
        assert_eq!(
            app.notifications.back().unwrap().kind,
            crate::types::NotificationKind::ConnectionLost
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn actual_connect_failure_withdraws_readiness_and_starts_bootstrap() {
        let temp_dir = crate::test_support::socket_tempdir().expect("temporary socket directory");
        let socket_path = temp_dir.path().join("missing.sock");
        let result = super::run_background_conversation_poll_with_timeout(
            socket_path.clone(),
            0,
            1,
            vec![rsi_common::rpc::ConversationFetchCursor {
                session_id: Uuid::new_v4(),
                since_sequence: None,
            }],
            true,
            std::time::Duration::from_millis(25),
        )
        .await;
        assert!(result.connection_lost);

        let mut app = App::new(DaemonClient::new(socket_path));
        app.poll.connected = true;
        app.poll.authoritative_config_ready = true;
        app.poll.sessions_authoritative = true;
        app.conversation_poll_active_generation = Some(1);
        assert!(!app.apply_conversation_poll_result(result));
        assert!(!app.poll.connected);
        assert!(!app.poll.authoritative_config_ready);
        assert!(!app.poll.sessions_authoritative);
        assert!(app.bootstrap.handshake_in_flight());
    }

    #[test]
    fn workflow_buttons_are_derived_for_snapshot_hydrated_leaves() {
        let mut app = test_app();
        let session_id = Uuid::new_v4();
        let workflow_id = Uuid::new_v4();
        let mut session = mk_session(session_id, SessionKind::Standard, SessionStatus::Completed);
        session.workflow_id = Some(workflow_id);
        app.sessions.insert(session_id, SessionState::new(session));
        app.workflows.insert(
            workflow_id,
            rsi_common::types::Workflow {
                id: workflow_id,
                title: "hydrated workflow".to_string(),
                stage: rsi_common::types::WorkflowStage::ResearchComplete,
                artifact_path: Some("thoughts/shared/research.md".to_string()),
                project_id: None,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            },
        );

        assert!(app.refresh_workflow_buttons());
        assert_eq!(
            app.sessions[&session_id].docregblock_contents,
            vec!["/plan @thoughts/shared/research.md".to_string()]
        );
    }

    #[tokio::test]
    async fn fallback_poll_keeps_later_batches_pending_until_dispatch_is_accepted() {
        let mut app = test_app();
        app.poll.connected = true;
        let ids: Vec<_> = (0..(super::super::CONVERSATION_BATCH_SIZE * 2 + 1))
            .map(|_| Uuid::new_v4())
            .collect();
        for session_id in &ids {
            app.sessions.insert(
                *session_id,
                SessionState::new(mk_session(
                    *session_id,
                    SessionKind::Standard,
                    SessionStatus::Running,
                )),
            );
        }

        let phase = PollPhase::FetchConversations {
            ids: ids.clone(),
            index: 0,
        };
        let (next, _) = app.poll_step(phase).await;
        assert!(
            next.is_none(),
            "a live batch never retains a timer poll phase"
        );
        assert!(matches!(
            app.conversation_poll_pending_phase,
            Some(PollPhase::FetchConversations { index, .. })
                if index == super::super::CONVERSATION_BATCH_SIZE
        ));

        app.conversation_poll_handle
            .take()
            .expect("first task")
            .abort();
        let first_generation = app
            .conversation_poll_active_generation
            .expect("first generation");
        app.apply_conversation_poll_result(crate::app::ConversationPollResult {
            attempt: app.bootstrap.attempt(),
            generation: first_generation,
            batches: Ok(Vec::new()),
            batch_unsupported: false,
            connection_lost: false,
            timed_out: false,
        });
        assert!(
            matches!(
                app.conversation_poll_pending_phase,
                Some(PollPhase::FetchConversations { index, .. })
                    if index == super::super::CONVERSATION_BATCH_SIZE * 2
            ),
            "the second batch dispatches only from first-result application"
        );
        assert!(app.conversation_poll_handle.is_some());

        app.conversation_poll_handle
            .take()
            .expect("second task")
            .abort();
        let second_generation = app
            .conversation_poll_active_generation
            .expect("second generation");
        app.apply_conversation_poll_result(crate::app::ConversationPollResult {
            attempt: app.bootstrap.attempt(),
            generation: second_generation,
            batches: Ok(Vec::new()),
            batch_unsupported: false,
            connection_lost: false,
            timed_out: false,
        });
        assert!(app.conversation_poll_pending_phase.is_none());
        assert!(app.conversation_poll_handle.is_some());
    }

    #[tokio::test]
    async fn held_post_snapshot_conversation_poll_keeps_main_task_state_live() {
        let temp_dir = crate::test_support::socket_tempdir().expect("temporary socket directory");
        let socket_path = temp_dir.path().join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&socket_path).expect("socket listener");
        let held_server = tokio::spawn(async move {
            let (stream, _) = listener
                .accept()
                .await
                .expect("conversation client connects");
            let _held_stream = stream;
            std::future::pending::<()>().await;
        });
        let mut app = App::new(DaemonClient::new(socket_path));
        app.poll.connected = true;
        let session_id = Uuid::new_v4();
        app.sessions.insert(
            session_id,
            SessionState::new(mk_session(
                session_id,
                SessionKind::Standard,
                SessionStatus::Running,
            )),
        );

        let (next, _) = app
            .poll_step(PollPhase::FetchConversations {
                ids: vec![session_id],
                index: 0,
            })
            .await;
        assert!(next.is_none());
        tokio::task::yield_now().await;
        assert!(app.conversation_poll_handle.is_some());

        app.input_buffer.push_str("input remains responsive");
        app.mark_dirty();
        assert_eq!(app.input_buffer, "input remains responsive");
        assert!(app.needs_redraw);

        app.conversation_poll_handle
            .take()
            .expect("held task")
            .abort();
        held_server.abort();
    }

    #[test]
    fn poll_conversation_ids_includes_currently_focused_running_session() {
        let mut app = test_app();
        let story_id = Uuid::new_v4();
        app.sessions.insert(
            story_id,
            SessionState::new(mk_session(
                story_id,
                SessionKind::Story,
                SessionStatus::Running,
            )),
        );
        app.filtered_session_order.push(story_id);

        // Set the session as focused in detail view
        if let Some(pane) = app.focused_pane_mut() {
            *pane = crate::types::Pane::SessionDetail {
                session_id: story_id,
            };
        }

        let ids = app.active_running_session_ids();
        assert!(
            ids.contains(&story_id),
            "a dropped push must not leave the open transcript stale: the focused running session is part of the bounded catch-up poll (#1234)"
        );
    }

    #[test]
    fn poll_conversation_ids_excludes_completed_session_even_when_focused() {
        let mut app = test_app();
        let story_id = Uuid::new_v4();
        app.sessions.insert(
            story_id,
            SessionState::new(mk_session(
                story_id,
                SessionKind::Story,
                SessionStatus::Completed,
            )),
        );
        if let Some(pane) = app.focused_pane_mut() {
            *pane = crate::types::Pane::SessionDetail {
                session_id: story_id,
            };
        }
        assert!(app.active_running_session_ids().is_empty());
    }
}
