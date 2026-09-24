//! Responsive TUI bootstrap coordination.
//!
//! Startup and reconnect have exactly three bounded jobs: one essential
//! handshake, one session snapshot, and one sandbox-storage preview. Operator
//! config/usage refreshes are separately fenced post-readiness work; they do
//! not join startup cardinality or alter launch readiness. Tasks only perform
//! I/O; all application state is applied by the main event-loop task.

use super::App;
use crate::client::DaemonClient;
use chrono::{DateTime, Utc};
use rsi_common::model_control::ModelControlStatusReport;
use rsi_common::rpc::{DaemonCapabilities, HealthStatusResponse, MemoryProviderStatus};
use rsi_common::sandbox_storage::{
    SandboxBuildCacheReclaimReport, SandboxBuildCacheReclaimSkipReason,
};
use rsi_common::types::{Project, Session, SessionLabel, UsageStats, WorkflowExecutionLookup};
use serde_json::Value;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use uuid::Uuid;

const BOOTSTRAP_RESULT_CAPACITY: usize = 8;
const BOOTSTRAP_RETRY_DELAY: Duration = Duration::from_secs(1);
const OPTIONAL_REFRESH_TIMEOUT: Duration = Duration::from_secs(3);

fn reconcile_memory_owner(
    settings: &mut crate::settings::UserSettings,
    json: &Value,
) -> Option<String> {
    if settings.memory_owner_migrated {
        return None;
    }
    let differing = [
        ("memory_enabled", serde_json::json!(settings.memory_enabled)),
        ("dream_enabled", serde_json::json!(settings.dream_enabled)),
        (
            "dream_observation_threshold",
            serde_json::json!(settings.observation_threshold),
        ),
        (
            "dream_cooldown_secs",
            serde_json::json!(settings.dream_cooldown_secs),
        ),
    ]
    .into_iter()
    .filter_map(|(field, local)| {
        json.get(field)
            .filter(|daemon| **daemon != local)
            .map(|daemon| format!("{field}={daemon}"))
    })
    .collect::<Vec<_>>();
    settings.memory_owner_migrated = true;
    (!differing.is_empty()).then(|| {
        format!(
            "Memory/dream settings now live in the daemon — kept daemon values: {}",
            differing.join(", ")
        )
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BootstrapComponentState {
    Pending,
    Loading,
    Ready,
    Error(String),
}

#[derive(Debug, Clone)]
pub(crate) enum SandboxStorageStatus {
    Unknown,
    RefreshingUnknown,
    Fresh {
        report: SandboxBuildCacheReclaimReport,
        observed_at: DateTime<Utc>,
    },
    RefreshingStale {
        report: SandboxBuildCacheReclaimReport,
        observed_at: DateTime<Utc>,
    },
    Error {
        message: String,
        last_success_at: Option<DateTime<Utc>>,
    },
}

pub(crate) struct HandshakeSuccess {
    client: DaemonClient,
    capabilities: DaemonCapabilities,
    compatibility_fallback: bool,
    health: Result<HealthStatusResponse, String>,
    config: Result<Value, String>,
}

pub(crate) struct SessionSnapshot {
    sessions: Vec<Session>,
}

pub(crate) struct SnapshotMetadata {
    projects: Result<Vec<Project>, String>,
    labels: Result<Vec<SessionLabel>, String>,
}

pub(crate) enum BootstrapEvent {
    Handshake {
        attempt: u64,
        result: Result<HandshakeSuccess, String>,
    },
    SessionSnapshot {
        attempt: u64,
        result: Result<SessionSnapshot, String>,
    },
    SnapshotMetadata {
        attempt: u64,
        metadata: SnapshotMetadata,
    },
    MemoryStatus {
        attempt: u64,
        result: Result<MemoryProviderStatus, String>,
    },
    SnapshotUsageStatus {
        attempt: u64,
        generation: u64,
        usage: Result<UsageStats, String>,
        model_control: Result<ModelControlStatusReport, String>,
    },
    UsageRefresh {
        attempt: u64,
        generation: u64,
        usage: Result<UsageStats, String>,
        model_control: Result<ModelControlStatusReport, String>,
    },
    GraphExecutions {
        attempt: u64,
        results: Vec<(Uuid, bool, Result<WorkflowExecutionLookup, String>)>,
    },
    SnapshotFinished {
        attempt: u64,
    },
    StorageStatus {
        generation: u64,
        result: Result<SandboxBuildCacheReclaimReport, String>,
    },
    DaemonConfigRefresh {
        attempt: u64,
        generation: u64,
        result: Result<Value, String>,
    },
}

/// Owns bootstrap task cardinality, retry state, and process-local freshness.
pub(crate) struct BootstrapCoordinator {
    tx: mpsc::Sender<BootstrapEvent>,
    rx: mpsc::Receiver<BootstrapEvent>,
    started_at: Instant,
    attempt: u64,
    handshake_handle: Option<JoinHandle<()>>,
    snapshot_handle: Option<JoinHandle<()>>,
    storage_handle: Option<JoinHandle<()>>,
    config_refresh_handle: Option<JoinHandle<()>>,
    usage_refresh_handle: Option<JoinHandle<()>>,
    retry_at: Option<Instant>,
    connection_error: Option<String>,
    pub(crate) config: BootstrapComponentState,
    pub(crate) sessions: BootstrapComponentState,
    initial_snapshot_applied: bool,
    usage_refresh_requested: bool,
    config_refresh_generation: u64,
    active_config_refresh_generation: Option<u64>,
    usage_refresh_generation: u64,
    active_usage_refresh_generation: Option<u64>,
    first_frame_recorded: bool,
    storage_generation: u64,
    active_storage_generation: Option<u64>,
    storage_follow_up: bool,
    storage_requested_for_attempt: bool,
    storage_started_for_attempt: bool,
    storage_admission_ready: bool,
    active_storage_automatic: bool,
    storage_busy_retry_available: bool,
    last_storage_success_at: Option<DateTime<Utc>>,
    pub(crate) storage_status: SandboxStorageStatus,
}

impl Default for BootstrapCoordinator {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel(BOOTSTRAP_RESULT_CAPACITY);
        Self {
            tx,
            rx,
            started_at: Instant::now(),
            attempt: 0,
            handshake_handle: None,
            snapshot_handle: None,
            storage_handle: None,
            config_refresh_handle: None,
            usage_refresh_handle: None,
            retry_at: None,
            connection_error: None,
            config: BootstrapComponentState::Pending,
            sessions: BootstrapComponentState::Pending,
            initial_snapshot_applied: false,
            usage_refresh_requested: false,
            config_refresh_generation: 0,
            active_config_refresh_generation: None,
            usage_refresh_generation: 0,
            active_usage_refresh_generation: None,
            first_frame_recorded: false,
            storage_generation: 0,
            active_storage_generation: None,
            storage_follow_up: false,
            storage_requested_for_attempt: false,
            storage_started_for_attempt: false,
            storage_admission_ready: false,
            active_storage_automatic: false,
            storage_busy_retry_available: true,
            last_storage_success_at: None,
            storage_status: SandboxStorageStatus::Unknown,
        }
    }
}

impl Drop for BootstrapCoordinator {
    fn drop(&mut self) {
        if let Some(handle) = self.handshake_handle.take() {
            handle.abort();
        }
        if let Some(handle) = self.snapshot_handle.take() {
            handle.abort();
        }
        if let Some(handle) = self.storage_handle.take() {
            handle.abort();
        }
        if let Some(handle) = self.config_refresh_handle.take() {
            handle.abort();
        }
        self.active_config_refresh_generation = None;
        if let Some(handle) = self.usage_refresh_handle.take() {
            handle.abort();
        }
        self.active_usage_refresh_generation = None;
    }
}

impl BootstrapCoordinator {
    pub(crate) async fn recv(&mut self) -> Option<BootstrapEvent> {
        self.rx.recv().await
    }

    pub(crate) fn attempt(&self) -> u64 {
        self.attempt
    }

    pub(crate) fn elapsed_ms(&self) -> u128 {
        self.started_at.elapsed().as_millis()
    }

    pub(crate) fn handshake_in_flight(&self) -> bool {
        self.handshake_handle.is_some()
    }

    pub(crate) fn storage_refresh_in_flight(&self) -> bool {
        self.active_storage_generation.is_some()
    }

    pub(crate) fn needs_retry(&self) -> bool {
        self.retry_at.is_some()
    }

    pub(crate) fn retry_due(&self) -> bool {
        self.retry_at
            .is_some_and(|deadline| Instant::now() >= deadline)
    }

    fn schedule_retry(&mut self) {
        self.retry_at = Some(Instant::now() + BOOTSTRAP_RETRY_DELAY);
    }

    fn prepare_handshake(&mut self) -> Option<(u64, mpsc::Sender<BootstrapEvent>)> {
        if self.handshake_handle.is_some() {
            return None;
        }
        if self.retry_at.is_some() && !self.retry_due() {
            return None;
        }
        self.retry_at = None;
        self.attempt = self.attempt.saturating_add(1);
        self.storage_requested_for_attempt = false;
        self.storage_started_for_attempt = false;
        self.storage_admission_ready = false;
        self.active_storage_automatic = false;
        self.storage_busy_retry_available = true;
        if let Some(handle) = self.config_refresh_handle.take() {
            handle.abort();
        }
        self.active_config_refresh_generation = None;
        if let Some(handle) = self.usage_refresh_handle.take() {
            handle.abort();
        }
        self.active_usage_refresh_generation = None;
        if let Some(handle) = self.snapshot_handle.take() {
            handle.abort();
        }
        self.config = BootstrapComponentState::Loading;
        if !self.initial_snapshot_applied {
            self.sessions = BootstrapComponentState::Loading;
        }
        Some((self.attempt, self.tx.clone()))
    }

    fn finish_handshake(&mut self, attempt: u64) -> bool {
        if attempt != self.attempt {
            return false;
        }
        self.handshake_handle = None;
        true
    }

    fn prepare_snapshot(&mut self) -> Option<mpsc::Sender<BootstrapEvent>> {
        if self.snapshot_handle.is_some() {
            return None;
        }
        self.sessions = BootstrapComponentState::Loading;
        Some(self.tx.clone())
    }

    fn finish_snapshot(&mut self, attempt: u64) {
        if attempt == self.attempt {
            self.snapshot_handle = None;
        }
    }

    fn take_initial_snapshot_application(&mut self) -> bool {
        if self.initial_snapshot_applied {
            false
        } else {
            self.initial_snapshot_applied = true;
            true
        }
    }

    fn prepare_automatic_storage_refresh(&mut self) -> Option<(u64, mpsc::Sender<BootstrapEvent>)> {
        self.storage_requested_for_attempt = true;
        if !self.storage_admission_ready {
            self.storage_status = refreshing_storage_state(&self.storage_status);
            return None;
        }
        self.storage_started_for_attempt = true;
        if self.active_storage_generation.is_some() {
            self.storage_follow_up = true;
            return None;
        }
        self.storage_generation = self.storage_generation.saturating_add(1);
        let generation = self.storage_generation;
        self.active_storage_generation = Some(generation);
        self.active_storage_automatic = true;
        self.storage_status = refreshing_storage_state(&self.storage_status);
        Some((generation, self.tx.clone()))
    }

    fn admit_automatic_storage_refresh(&mut self) -> bool {
        self.storage_admission_ready = true;
        self.storage_requested_for_attempt && !self.storage_started_for_attempt
    }

    fn begin_direct_storage_refresh(&mut self) -> u64 {
        self.storage_requested_for_attempt = true;
        if let Some(handle) = self.storage_handle.take() {
            handle.abort();
        }
        self.storage_generation = self.storage_generation.saturating_add(1);
        self.storage_follow_up = false;
        self.active_storage_generation = Some(self.storage_generation);
        self.active_storage_automatic = false;
        self.storage_status = refreshing_storage_state(&self.storage_status);
        self.storage_generation
    }

    fn finish_storage_refresh(
        &mut self,
        generation: u64,
        result: Result<SandboxBuildCacheReclaimReport, String>,
    ) -> (bool, bool, bool) {
        if self.active_storage_generation == Some(generation) {
            self.active_storage_generation = None;
            self.storage_handle = None;
        }
        if generation != self.storage_generation {
            return (false, false, false);
        }

        let retry_store_busy = self.active_storage_automatic
            && self.storage_busy_retry_available
            && result.as_ref().is_ok_and(|report| {
                report
                    .skip_counts
                    .get(&SandboxBuildCacheReclaimSkipReason::StoreBusy)
                    .copied()
                    .unwrap_or(0)
                    > 0
            });
        if retry_store_busy {
            self.storage_busy_retry_available = false;
        }
        self.active_storage_automatic = false;
        if !retry_store_busy {
            self.storage_status = match result {
                Ok(report) => {
                    let observed_at = Utc::now();
                    self.last_storage_success_at = Some(observed_at);
                    SandboxStorageStatus::Fresh {
                        report,
                        observed_at,
                    }
                }
                Err(message) => SandboxStorageStatus::Error {
                    message,
                    last_success_at: self.last_storage_success_at,
                },
            };
        }
        let follow_up = std::mem::take(&mut self.storage_follow_up) || retry_store_busy;
        (true, follow_up, retry_store_busy)
    }

    fn record_first_frame(&mut self) -> bool {
        if self.first_frame_recorded {
            false
        } else {
            self.first_frame_recorded = true;
            true
        }
    }

    fn prepare_config_refresh(&mut self) -> Option<(u64, u64, mpsc::Sender<BootstrapEvent>)> {
        if self.config_refresh_handle.is_some() {
            return None;
        }
        self.config_refresh_generation = self.config_refresh_generation.saturating_add(1);
        let generation = self.config_refresh_generation;
        self.active_config_refresh_generation = Some(generation);
        Some((self.attempt, generation, self.tx.clone()))
    }

    fn finish_config_refresh(&mut self, generation: u64) -> bool {
        if self.active_config_refresh_generation == Some(generation) {
            self.active_config_refresh_generation = None;
            self.config_refresh_handle = None;
            return true;
        }
        false
    }

    fn prepare_usage_refresh(&mut self) -> Option<(u64, u64, mpsc::Sender<BootstrapEvent>)> {
        if self.usage_refresh_handle.is_some() {
            return None;
        }
        self.usage_refresh_generation = self.usage_refresh_generation.saturating_add(1);
        let generation = self.usage_refresh_generation;
        self.active_usage_refresh_generation = Some(generation);
        Some((self.attempt, generation, self.tx.clone()))
    }

    fn prepare_snapshot_usage(&mut self) -> u64 {
        self.usage_refresh_generation = self.usage_refresh_generation.saturating_add(1);
        let generation = self.usage_refresh_generation;
        self.active_usage_refresh_generation = Some(generation);
        generation
    }

    fn finish_usage_refresh(&mut self, generation: u64) -> bool {
        if self.active_usage_refresh_generation == Some(generation) {
            self.active_usage_refresh_generation = None;
            self.usage_refresh_handle = None;
            return true;
        }
        false
    }
}

fn refreshing_storage_state(previous: &SandboxStorageStatus) -> SandboxStorageStatus {
    match previous {
        SandboxStorageStatus::Fresh {
            report,
            observed_at,
        }
        | SandboxStorageStatus::RefreshingStale {
            report,
            observed_at,
        } => SandboxStorageStatus::RefreshingStale {
            report: report.clone(),
            observed_at: observed_at.to_owned(),
        },
        SandboxStorageStatus::Unknown
        | SandboxStorageStatus::RefreshingUnknown
        | SandboxStorageStatus::Error { .. } => SandboxStorageStatus::RefreshingUnknown,
    }
}

fn compatibility_capabilities() -> DaemonCapabilities {
    DaemonCapabilities {
        batch_fetch: true,
        push_notifications: false,
        memory_search: false,
        sandbox: false,
        ..DaemonCapabilities::default()
    }
}

async fn run_handshake(socket_path: PathBuf, attempt: u64, tx: mpsc::Sender<BootstrapEvent>) {
    let mut client = DaemonClient::new(socket_path);
    let result = async {
        client.connect().await.map_err(|error| error.to_string())?;
        let (capabilities, compatibility_fallback) = match client.get_daemon_capabilities().await {
            Ok(capabilities) => (capabilities, false),
            Err(_) => (compatibility_capabilities(), true),
        };
        let health = client
            .get_health_status()
            .await
            .map_err(|error| error.to_string());
        let config = client
            .get_daemon_config()
            .await
            .map_err(|error| error.to_string());
        Ok(HandshakeSuccess {
            client,
            capabilities,
            compatibility_fallback,
            health,
            config,
        })
    }
    .await;
    let _ = tx.send(BootstrapEvent::Handshake { attempt, result }).await;
}

async fn run_snapshot(
    socket_path: PathBuf,
    attempt: u64,
    memory_supported: bool,
    config_syncs: Vec<(String, Value)>,
    usage_request: Option<(Option<Uuid>, u64)>,
    graph_requests: Vec<(Uuid, Uuid, bool)>,
    tx: mpsc::Sender<BootstrapEvent>,
) {
    let mut client = DaemonClient::new(socket_path);
    if let Err(error) = client.connect().await {
        let _ = tx
            .send(BootstrapEvent::SessionSnapshot {
                attempt,
                result: Err(error.to_string()),
            })
            .await;
        return;
    }

    let sessions = match client.list_sessions().await {
        Ok(sessions) => sessions,
        Err(error) => {
            let _ = tx
                .send(BootstrapEvent::SessionSnapshot {
                    attempt,
                    result: Err(error.to_string()),
                })
                .await;
            return;
        }
    };
    if tx
        .send(BootstrapEvent::SessionSnapshot {
            attempt,
            result: Ok(SessionSnapshot { sessions }),
        })
        .await
        .is_err()
    {
        return;
    }

    // Session identity is delivered before optional corpus metadata. The main
    // task uses that result as the deterministic admission barrier for the
    // independently owned storage preview.
    let projects = client
        .list_projects()
        .await
        .map_err(|error| error.to_string());
    let labels = client
        .list_labels()
        .await
        .map_err(|error| error.to_string());
    if tx
        .send(BootstrapEvent::SnapshotMetadata {
            attempt,
            metadata: SnapshotMetadata { projects, labels },
        })
        .await
        .is_err()
    {
        return;
    }

    for (field, value) in config_syncs {
        if let Err(error) = client.update_daemon_config(&field, value).await {
            tracing::warn!(attempt, %field, %error, "Optional daemon config sync failed");
        }
    }

    if let Some((project_id, generation)) = usage_request {
        let usage = client
            .get_usage_stats(project_id)
            .await
            .map_err(|error| error.to_string());
        let model_control = client
            .get_model_control_status(Some(12))
            .await
            .map_err(|error| error.to_string());
        if tx
            .send(BootstrapEvent::SnapshotUsageStatus {
                attempt,
                generation,
                usage,
                model_control,
            })
            .await
            .is_err()
        {
            return;
        }
    }

    if memory_supported {
        let result = client
            .memory_status()
            .await
            .map_err(|error| error.to_string());
        if tx
            .send(BootstrapEvent::MemoryStatus { attempt, result })
            .await
            .is_err()
        {
            return;
        }
    }

    let mut graph_results = Vec::with_capacity(graph_requests.len());
    for (draft_id, execution_id, notify_user) in graph_requests {
        graph_results.push((
            draft_id,
            notify_user,
            client
                .get_workflow_execution(execution_id)
                .await
                .map_err(|error| error.to_string()),
        ));
    }
    if !graph_results.is_empty()
        && tx
            .send(BootstrapEvent::GraphExecutions {
                attempt,
                results: graph_results,
            })
            .await
            .is_err()
    {
        return;
    }
    let _ = tx.send(BootstrapEvent::SnapshotFinished { attempt }).await;
}

async fn run_storage_status(
    socket_path: PathBuf,
    generation: u64,
    tx: mpsc::Sender<BootstrapEvent>,
    delay: Duration,
) {
    if !delay.is_zero() {
        tokio::time::sleep(delay).await;
    }
    let mut client = DaemonClient::new(socket_path);
    let result = async {
        client.connect().await.map_err(|error| error.to_string())?;
        client
            .get_sandbox_storage_status()
            .await
            .map(|report| report.report().clone())
            .map_err(|error| error.to_string())
    }
    .await;
    let _ = tx
        .send(BootstrapEvent::StorageStatus { generation, result })
        .await;
}

async fn run_daemon_config_refresh(
    socket_path: PathBuf,
    attempt: u64,
    generation: u64,
    tx: mpsc::Sender<BootstrapEvent>,
) {
    let result = match tokio::time::timeout(OPTIONAL_REFRESH_TIMEOUT, async {
        let mut client = DaemonClient::new(socket_path);
        client.connect().await.map_err(|error| error.to_string())?;
        client
            .get_daemon_config()
            .await
            .map_err(|error| error.to_string())
    })
    .await
    {
        Ok(result) => result,
        Err(_) => Err("daemon configuration refresh timed out".to_string()),
    };
    let _ = tx
        .send(BootstrapEvent::DaemonConfigRefresh {
            attempt,
            generation,
            result,
        })
        .await;
}

async fn run_usage_refresh(
    socket_path: PathBuf,
    attempt: u64,
    generation: u64,
    project_id: Option<Uuid>,
    tx: mpsc::Sender<BootstrapEvent>,
) {
    let result = tokio::time::timeout(OPTIONAL_REFRESH_TIMEOUT, async {
        let mut client = DaemonClient::new(socket_path);
        client.connect().await.map_err(|error| error.to_string())?;
        Ok::<_, String>((
            client
                .get_usage_stats(project_id)
                .await
                .map_err(|error| error.to_string()),
            client
                .get_model_control_status(Some(12))
                .await
                .map_err(|error| error.to_string()),
        ))
    })
    .await;
    let (usage, model_control) = match result {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => (Err(error.clone()), Err(error)),
        Err(_) => (
            Err("usage refresh timed out".to_string()),
            Err("usage refresh timed out".to_string()),
        ),
    };
    let _ = tx
        .send(BootstrapEvent::UsageRefresh {
            attempt,
            generation,
            usage,
            model_control,
        })
        .await;
}

impl App {
    pub(crate) fn start_bootstrap(&mut self) {
        let Some((attempt, tx)) = self.bootstrap.prepare_handshake() else {
            return;
        };
        self.poll.bootstrap_in_flight = true;
        self.poll.authoritative_config_ready = false;
        let socket_path = self.client.socket_path().to_path_buf();
        tracing::info!(
            attempt,
            elapsed_ms = self.bootstrap.elapsed_ms(),
            "tui_startup_started"
        );
        self.bootstrap.handshake_handle =
            Some(tokio::spawn(run_handshake(socket_path, attempt, tx)));
        self.mark_dirty();
    }

    pub(crate) fn bootstrap_should_start(&self) -> bool {
        !self.bootstrap.handshake_in_flight()
            && ((!self.poll.connected && !self.bootstrap.needs_retry())
                || self.bootstrap.retry_due())
    }

    pub(crate) fn bootstrap_polling_pending(&self) -> bool {
        self.bootstrap.handshake_in_flight()
            || self.bootstrap.needs_retry()
            || !self.poll.authoritative_config_ready
            || !self.poll.sessions_authoritative
    }

    pub(crate) fn authoritative_config_ready(&self) -> bool {
        self.poll.connected && self.poll.authoritative_config_ready
    }

    pub(crate) fn daemon_config_unavailable_reason(&self) -> String {
        match &self.bootstrap.config {
            BootstrapComponentState::Error(error) => {
                format!("Daemon configuration unavailable: {error}; draft preserved")
            }
            BootstrapComponentState::Pending | BootstrapComponentState::Loading => {
                "Daemon configuration is still loading; draft preserved".to_string()
            }
            BootstrapComponentState::Ready => {
                "Daemon configuration is unavailable; draft preserved".to_string()
            }
        }
    }

    pub(crate) fn apply_authoritative_daemon_config(&mut self, json: &Value) {
        let recovered = !self.authoritative_config_ready();
        crate::settings::DaemonFeatureEntry::update_from_json(&mut self.daemon_features, json);
        if !self.settings.memory_owner_migrated {
            if let Some(message) = reconcile_memory_owner(&mut self.settings, json) {
                self.notify(message);
            }
            crate::state::PersistedState::capture(self).save();
        }
        if let Some(slug) = json.get("system_prompt_preset").and_then(Value::as_str) {
            self.settings.system_prompt_preset =
                crate::settings::SystemPromptPreset::from_slug(slug);
        }
        self.poll.authoritative_config_ready = true;
        self.bootstrap.config = BootstrapComponentState::Ready;
        if recovered {
            self.advance_auto_resume_progress();
        }
        self.mark_dirty();
    }

    pub(crate) fn mark_daemon_config_error(&mut self, error: String) {
        self.poll.authoritative_config_ready = false;
        self.bootstrap.config = BootstrapComponentState::Error(error);
        self.bootstrap.schedule_retry();
        self.mark_dirty();
    }

    /// Classify a loss discovered by an owned background task on the main
    /// task, then reuse the same single-flight bootstrap/reconnect path.
    pub(crate) fn mark_transport_lost(&mut self, error: String) {
        if let Some(handle) = self.conversation_poll_handle.take() {
            handle.abort();
        }
        self.conversation_poll_active_generation = None;
        self.conversation_poll_active_cursors.clear();
        self.conversation_poll_pending_phase = None;
        self.conversation_poll_hydration_pending = None;
        self.poll.connected = false;
        self.poll.authoritative_config_ready = false;
        self.poll.sessions_authoritative = false;
        self.notification_stream = None;
        self.client.disconnect();
        self.bootstrap.connection_error = Some(error.clone());
        self.bootstrap.config = BootstrapComponentState::Pending;
        self.push_notification(
            crate::types::NotificationKind::ConnectionLost,
            crate::types::NotificationPriority::High,
            format!("Lost connection: {error}"),
            None,
        );
        self.start_bootstrap();
    }

    /// Push-stream EOF is transport loss even when no active session would
    /// cause fallback polling to observe it. Reuse the shared single-flight
    /// reconnect coordinator on the event-loop task.
    pub(crate) fn mark_notification_stream_lost(&mut self) {
        if !self.poll.connected && self.notification_stream.is_none() {
            return;
        }
        self.notification_stream = None;
        self.mark_transport_lost("daemon notification stream closed".to_string());
    }

    pub(crate) fn apply_notification_stream_event(
        &mut self,
        event: crate::notification_stream::NotificationStreamEvent,
    ) -> bool {
        match event {
            crate::notification_stream::NotificationStreamEvent::Bus(event) => {
                let relevant_progress = matches!(
                    event.event_type.as_str(),
                    "conversation_event" | "session_status_changed" | "session_metadata_changed"
                );
                let changed = self.apply_push_event(event);
                if changed && relevant_progress {
                    self.advance_auto_resume_progress();
                }
                changed
            }
            crate::notification_stream::NotificationStreamEvent::Lost(loss) => {
                self.notification_stream = None;
                self.mark_transport_lost(loss.message());
                true
            }
        }
    }

    fn start_snapshot_followups(&mut self, attempt: u64, memory_supported: bool) {
        let Some(tx) = self.bootstrap.prepare_snapshot() else {
            return;
        };
        let socket_path = self.client.socket_path().to_path_buf();
        let active_draft = self.graph_review_draft_id();
        let config_syncs = if self.authoritative_config_ready() {
            self.bootstrap_config_syncs()
        } else {
            Vec::new()
        };
        let usage_request = self.bootstrap.usage_refresh_requested.then(|| {
            (
                self.current_project_id,
                self.bootstrap.prepare_snapshot_usage(),
            )
        });
        self.bootstrap.usage_refresh_requested = false;
        let graph_requests = self
            .graph_drafts
            .values()
            .filter_map(|draft| {
                draft.last_execution_id.map(|execution_id| {
                    (
                        draft.draft_id,
                        execution_id,
                        active_draft == Some(draft.draft_id),
                    )
                })
            })
            .collect();
        self.bootstrap.snapshot_handle = Some(tokio::spawn(run_snapshot(
            socket_path,
            attempt,
            memory_supported,
            config_syncs,
            usage_request,
            graph_requests,
            tx,
        )));
    }

    pub(crate) fn request_usage_stats_refresh(&mut self) {
        self.bootstrap.usage_refresh_requested = true;
        if !self.poll.connected {
            return;
        }
        let Some((attempt, generation, tx)) = self.bootstrap.prepare_usage_refresh() else {
            return;
        };
        self.bootstrap.usage_refresh_requested = false;
        let socket_path = self.client.socket_path().to_path_buf();
        let project_id = self.current_project_id;
        self.bootstrap.usage_refresh_handle = Some(tokio::spawn(run_usage_refresh(
            socket_path,
            attempt,
            generation,
            project_id,
            tx,
        )));
    }

    /// Refresh daemon-owned settings without replacing the primary connection,
    /// restarting bootstrap, or withdrawing an already accepted launch gate.
    pub(crate) fn request_daemon_config_refresh(&mut self) {
        if !self.poll.connected {
            return;
        }
        let Some((attempt, generation, tx)) = self.bootstrap.prepare_config_refresh() else {
            return;
        };
        let socket_path = self.client.socket_path().to_path_buf();
        self.bootstrap.config_refresh_handle = Some(tokio::spawn(run_daemon_config_refresh(
            socket_path,
            attempt,
            generation,
            tx,
        )));
    }

    fn bootstrap_config_syncs(&self) -> Vec<(String, Value)> {
        let title_custom = self.settings.title_model_custom_provider_id.and_then(|id| {
            self.settings
                .custom_providers
                .iter()
                .find(|entry| entry.id == id)
        });
        let memory_custom = self
            .settings
            .memory_model_fallback_custom_provider_id
            .and_then(|id| {
                self.settings
                    .custom_providers
                    .iter()
                    .find(|entry| entry.id == id)
            });
        let dream_custom = self.settings.dream_model_custom_provider_id.and_then(|id| {
            self.settings
                .custom_providers
                .iter()
                .find(|entry| entry.id == id)
        });

        vec![
            (
                "title_model_local".into(),
                serde_json::json!(self.settings.title_model_local),
            ),
            (
                "title_model_fallback".into(),
                serde_json::json!(self.settings.title_model_fallback),
            ),
            (
                "title_model_provider".into(),
                serde_json::json!(self.settings.title_model_provider),
            ),
            (
                "title_model_base_url".into(),
                serde_json::json!(title_custom.map(|entry| entry.base_url.clone())),
            ),
            (
                "title_model_api_key".into(),
                serde_json::json!(title_custom.map(|entry| entry.api_key.clone())),
            ),
            (
                "memory_model_local".into(),
                serde_json::json!(self.settings.memory_model_local),
            ),
            (
                "memory_model_fallback".into(),
                serde_json::json!(self.settings.memory_model_fallback),
            ),
            (
                "memory_model_fallback_provider".into(),
                serde_json::json!(self.settings.memory_model_fallback_provider),
            ),
            (
                "memory_model_fallback_base_url".into(),
                serde_json::json!(memory_custom.map(|entry| entry.base_url.clone())),
            ),
            (
                "memory_model_fallback_api_key".into(),
                serde_json::json!(memory_custom.map(|entry| entry.api_key.clone())),
            ),
            (
                "dream_model".into(),
                serde_json::json!(self.settings.dream_model),
            ),
            (
                "dream_model_provider".into(),
                serde_json::json!(self.settings.dream_model_provider),
            ),
            (
                "dream_model_base_url".into(),
                serde_json::json!(dream_custom.map(|entry| entry.base_url.clone())),
            ),
            (
                "dream_model_api_key".into(),
                serde_json::json!(dream_custom.map(|entry| entry.api_key.clone())),
            ),
            (
                "prompt_compile_model_local".into(),
                serde_json::json!(self.settings.prompt_processor.model),
            ),
            (
                "prompt_compile_model_provider".into(),
                serde_json::json!(self.settings.prompt_processor.provider),
            ),
            (
                "prompt_compile_model_base_url".into(),
                serde_json::json!(self.settings.prompt_processor.custom_base_url),
            ),
            (
                "prompt_compile_model_api_key".into(),
                serde_json::json!(self.settings.prompt_processor.custom_api_key),
            ),
        ]
    }

    pub(crate) fn request_storage_status_refresh(&mut self) {
        self.request_storage_status_refresh_after(Duration::ZERO);
    }

    fn request_storage_status_refresh_after(&mut self, delay: Duration) {
        let Some((generation, tx)) = self.bootstrap.prepare_automatic_storage_refresh() else {
            self.sync_storage_status_display();
            return;
        };
        self.sync_storage_status_display();
        let socket_path = self.client.socket_path().to_path_buf();
        self.bootstrap.storage_handle = Some(tokio::spawn(run_storage_status(
            socket_path,
            generation,
            tx,
            delay,
        )));
        self.mark_dirty();
    }

    pub(crate) fn storage_refresh_in_flight(&self) -> bool {
        self.bootstrap.storage_refresh_in_flight()
    }

    pub(crate) fn begin_direct_storage_refresh(&mut self) -> u64 {
        let generation = self.bootstrap.begin_direct_storage_refresh();
        self.sync_storage_status_display();
        self.mark_dirty();
        generation
    }

    pub(crate) fn finish_direct_storage_refresh(
        &mut self,
        generation: u64,
        result: Result<SandboxBuildCacheReclaimReport, String>,
    ) {
        let (changed, follow_up, store_busy_retry) =
            self.bootstrap.finish_storage_refresh(generation, result);
        if changed {
            self.sync_storage_status_display();
            tracing::info!(
                attempt = self.bootstrap.attempt(),
                generation,
                elapsed_ms = self.bootstrap.elapsed_ms(),
                "storage_status_settled"
            );
            self.mark_dirty();
        }
        if follow_up {
            self.request_storage_status_refresh_after(if store_busy_retry {
                Duration::from_millis(25)
            } else {
                Duration::ZERO
            });
        }
    }

    fn sync_storage_status_display(&mut self) {
        crate::settings::DaemonFeatureEntry::update_sandbox_storage_status(
            &mut self.daemon_features,
            &self.bootstrap.storage_status,
        );
    }

    pub(crate) fn record_first_frame(&mut self) {
        if self.bootstrap.record_first_frame() {
            tracing::info!(
                attempt = self.bootstrap.attempt(),
                elapsed_ms = self.bootstrap.elapsed_ms(),
                "first_frame"
            );
        }
    }

    pub(crate) fn session_list_empty_message(&self) -> String {
        if !self.poll.connected {
            return match &self.bootstrap.connection_error {
                Some(error) => format!("Connection error: {error} · retrying"),
                None => "Connecting to daemon…".to_string(),
            };
        }
        match &self.bootstrap.config {
            BootstrapComponentState::Pending | BootstrapComponentState::Loading => {
                "Loading daemon configuration…".to_string()
            }
            BootstrapComponentState::Error(error) => {
                format!("Daemon configuration error: {error}")
            }
            BootstrapComponentState::Ready => match &self.bootstrap.sessions {
                BootstrapComponentState::Pending | BootstrapComponentState::Loading => {
                    "Loading sessions…".to_string()
                }
                BootstrapComponentState::Error(error) => {
                    format!("Session loading error: {error}")
                }
                BootstrapComponentState::Ready => {
                    "Ready · No sessions. Press 'n' to create one.".to_string()
                }
            },
        }
    }

    pub(crate) fn apply_bootstrap_event(&mut self, event: BootstrapEvent) -> bool {
        match event {
            BootstrapEvent::Handshake { attempt, result } => {
                if !self.bootstrap.finish_handshake(attempt) {
                    return false;
                }
                self.poll.bootstrap_in_flight = false;
                match result {
                    Err(error) => {
                        self.poll.connected = false;
                        self.poll.authoritative_config_ready = false;
                        self.notification_stream = None;
                        self.bootstrap.connection_error = Some(error.clone());
                        self.bootstrap.config = BootstrapComponentState::Pending;
                        self.bootstrap.schedule_retry();
                        tracing::warn!(attempt, %error, "TUI bootstrap connection failed");
                    }
                    Ok(success) => {
                        let HandshakeSuccess {
                            client,
                            capabilities,
                            compatibility_fallback,
                            health,
                            config,
                        } = success;
                        self.client = client;
                        self.poll.connected = true;
                        self.bootstrap.connection_error = None;
                        self.poll.batch_fetch_supported = capabilities.batch_fetch;
                        self.poll.push_supported = capabilities.push_notifications;
                        self.poll.memory_search_supported = capabilities.memory_search;
                        self.poll.sandbox_supported = capabilities.sandbox;
                        tracing::info!(
                            attempt,
                            elapsed_ms = self.bootstrap.elapsed_ms(),
                            compatibility_fallback,
                            "transport_connected"
                        );

                        if self.poll.push_supported {
                            self.notification_stream =
                                Some(crate::notification_stream::NotificationStream::spawn(
                                    self.client.socket_path(),
                                ));
                            tracing::info!("push notification stream connected");
                        } else {
                            self.notification_stream = None;
                        }
                        self.push_notification(
                            crate::types::NotificationKind::Connected,
                            crate::types::NotificationPriority::Medium,
                            "Connected to daemon".to_string(),
                            None,
                        );

                        match health {
                            Ok(status) => {
                                if let Some(restart) = &status.latest_daemon_restart {
                                    self.push_notification(
                                        crate::types::NotificationKind::Info,
                                        crate::types::NotificationPriority::High,
                                        format!(
                                            "Daemon watchdog restarted at {}: {} (last healthy {})",
                                            restart.observed_at.format("%Y-%m-%d %H:%M UTC"),
                                            restart.failed_probes.join(", "),
                                            restart.last_healthy_at.format("%Y-%m-%d %H:%M UTC"),
                                        ),
                                        None,
                                    );
                                }
                                use rsi_common::types::SessionProvider;
                                self.provider_availability.insert(
                                    SessionProvider::Claude,
                                    status.provider_claude_available,
                                );
                                self.provider_availability.insert(
                                    SessionProvider::Codex,
                                    status.provider_codex_available,
                                );
                                self.provider_availability.insert(
                                    SessionProvider::Pioneer,
                                    status.provider_pioneer_available,
                                );
                                self.provider_availability.insert(
                                    SessionProvider::OpenRouter,
                                    status.provider_openrouter_available,
                                );
                                self.provider_availability.insert(
                                    SessionProvider::Bedrock,
                                    status.provider_bedrock_available,
                                );
                                self.provider_availability.insert(
                                    SessionProvider::Local,
                                    status.provider_local_available,
                                );
                                self.provider_availability.insert(
                                    SessionProvider::Antigravity,
                                    status.provider_antigravity_available,
                                );
                                // V99/P1-B cold read belongs to the response-backed
                                // bootstrap handshake: seed the latest persisted
                                // provider snapshots without restoring the former
                                // event-loop-blocking health refresh.
                                for snapshot in status.rate_limits {
                                    self.provider_rate_limits
                                        .insert(snapshot.provider, snapshot);
                                }
                            }
                            Err(error) => {
                                tracing::warn!(attempt, %error, "Provider health unavailable during bootstrap");
                            }
                        }

                        match config {
                            Ok(json) => {
                                self.apply_authoritative_daemon_config(&json);
                            }
                            Err(error) => self.mark_daemon_config_error(error),
                        }
                        tracing::info!(
                            attempt,
                            ready = self.poll.authoritative_config_ready,
                            elapsed_ms = self.bootstrap.elapsed_ms(),
                            "daemon_config_settled"
                        );
                        // Claim the automatic preview immediately after the
                        // handshake. Its RPC admission remains fenced until the
                        // identity-bearing ListSessions result settles, so it
                        // cannot occupy the daemon ahead of session identity.
                        // It does not wait for projects, labels, conversations,
                        // or any optional follow-up.
                        self.request_storage_status_refresh();
                        self.start_snapshot_followups(attempt, capabilities.memory_search);
                    }
                }
                self.mark_dirty();
                true
            }
            BootstrapEvent::SessionSnapshot { attempt, result } => {
                if attempt != self.bootstrap.attempt() {
                    return false;
                }
                match result {
                    Ok(snapshot) => {
                        let _ = self.update_sessions(snapshot.sessions);
                        // Snapshot hydration has no conversation event to
                        // trigger this derivation, so completed selected leaves
                        // still receive their workflow commands before render.
                        let _ = self.refresh_workflow_buttons();
                        self.bootstrap.sessions = BootstrapComponentState::Ready;
                        self.poll.sessions_authoritative = true;
                        if self.bootstrap.take_initial_snapshot_application() {
                            self.apply_pending_dev_state();
                            self.apply_pending_fold_states();
                            self.reconcile_restored_selections();
                        }
                        self.run_navigation_effect_if_changed();
                        self.start_initial_conversation_hydration();
                        tracing::info!(
                            attempt,
                            elapsed_ms = self.bootstrap.elapsed_ms(),
                            "sessions_settled"
                        );
                        if self.bootstrap.admit_automatic_storage_refresh() {
                            self.request_storage_status_refresh();
                        }
                        self.mark_dirty();
                    }
                    Err(error) => {
                        self.bootstrap.sessions = BootstrapComponentState::Error(error.clone());
                        self.poll.sessions_authoritative = false;
                        self.bootstrap.finish_snapshot(attempt);
                        self.bootstrap.schedule_retry();
                        tracing::warn!(attempt, %error, "Initial session snapshot failed");
                        self.mark_dirty();
                        if self.bootstrap.admit_automatic_storage_refresh() {
                            self.request_storage_status_refresh();
                        }
                    }
                }
                true
            }
            BootstrapEvent::SnapshotMetadata { attempt, metadata } => {
                if attempt != self.bootstrap.attempt() {
                    return false;
                }
                match metadata.projects {
                    Ok(projects) => {
                        let _ = self.update_projects(projects);
                        // Reconnect may retain the same project metadata while
                        // the daemon's manager appointment changed offline.
                        self.manager_roster.request_refresh();
                    }
                    Err(error) => {
                        tracing::warn!(attempt, %error, "Initial project snapshot failed")
                    }
                }
                match metadata.labels {
                    Ok(labels) => self.update_labels(labels),
                    Err(error) => {
                        tracing::warn!(attempt, %error, "Initial label snapshot failed")
                    }
                }
                self.mark_dirty();
                true
            }
            BootstrapEvent::MemoryStatus { attempt, result } => {
                if attempt != self.bootstrap.attempt() {
                    return false;
                }
                match result {
                    Ok(status) => self.memory_status = Some(status),
                    Err(error) => tracing::warn!(attempt, %error, "Optional memory status failed"),
                }
                true
            }
            BootstrapEvent::SnapshotUsageStatus {
                attempt,
                generation,
                usage,
                model_control,
            } => {
                let current_generation = self.bootstrap.finish_usage_refresh(generation);
                if attempt != self.bootstrap.attempt() || !current_generation {
                    return false;
                }
                match usage {
                    Ok(stats) => self.cached_usage_stats = Some(stats),
                    Err(error) => tracing::warn!(attempt, %error, "Usage stats refresh failed"),
                }
                match model_control {
                    Ok(status) => {
                        crate::settings::DaemonFeatureEntry::update_model_control(
                            &mut self.daemon_features,
                            &status,
                        );
                        self.cached_model_control_status = Some(status);
                    }
                    Err(error) => {
                        tracing::warn!(attempt, %error, "Model control status refresh failed");
                    }
                }
                self.mark_dirty();
                true
            }
            BootstrapEvent::UsageRefresh {
                attempt,
                generation,
                usage,
                model_control,
            } => {
                // A stale completion must not clear a newer task's handle or
                // overwrite the newer view of usage/configuration state.
                let current_generation = self.bootstrap.finish_usage_refresh(generation);
                if attempt != self.bootstrap.attempt() || !current_generation {
                    return false;
                }
                match usage {
                    Ok(stats) => self.cached_usage_stats = Some(stats),
                    Err(error) => tracing::warn!(attempt, %error, "Usage stats refresh failed"),
                }
                match model_control {
                    Ok(status) => {
                        crate::settings::DaemonFeatureEntry::update_model_control(
                            &mut self.daemon_features,
                            &status,
                        );
                        self.cached_model_control_status = Some(status);
                    }
                    Err(error) => {
                        tracing::warn!(attempt, %error, "Model control status refresh failed");
                    }
                }
                self.mark_dirty();
                true
            }
            BootstrapEvent::GraphExecutions { attempt, results } => {
                if attempt != self.bootstrap.attempt() {
                    return false;
                }
                let mut changed = false;
                for (draft_id, notify_user, result) in results {
                    changed |= self.apply_graph_execution_lookup(draft_id, result, notify_user);
                }
                if changed {
                    self.mark_dirty();
                }
                true
            }
            BootstrapEvent::SnapshotFinished { attempt } => {
                self.bootstrap.finish_snapshot(attempt);
                false
            }
            BootstrapEvent::StorageStatus { generation, result } => {
                self.finish_direct_storage_refresh(generation, result);
                true
            }
            BootstrapEvent::DaemonConfigRefresh {
                attempt,
                generation,
                result,
            } => {
                let current_generation = self.bootstrap.finish_config_refresh(generation);
                if attempt != self.bootstrap.attempt() || !current_generation {
                    return false;
                }
                match result {
                    Ok(json) => self.apply_authoritative_daemon_config(&json),
                    Err(error) if self.authoritative_config_ready() => {
                        tracing::warn!(attempt, %error, "Optional daemon configuration refresh failed");
                    }
                    Err(error) => self.mark_daemon_config_error(error),
                }
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::UserSettings;
    use rsi_common::sandbox_storage::{
        SandboxBuildCacheReclaimConfig, SandboxBuildCacheReclaimStopReason, SandboxFilesystemStats,
    };
    use std::collections::BTreeMap;

    fn daemon_memory_json() -> Value {
        serde_json::json!({
            "memory_enabled": false,
            "dream_enabled": true,
            "dream_observation_threshold": 25,
            "dream_cooldown_secs": 60
        })
    }

    #[test]
    fn memory_reconcile_on_divergence_toasts_daemon_values_and_sets_marker() {
        let mut settings = UserSettings::default();
        let Some(notice) = reconcile_memory_owner(&mut settings, &daemon_memory_json()) else {
            panic!("divergence produces a notice");
        };
        assert!(settings.memory_owner_migrated);
        for value in [
            "memory_enabled=false",
            "dream_enabled=true",
            "dream_observation_threshold=25",
            "dream_cooldown_secs=60",
        ] {
            assert!(notice.contains(value), "{value} in {notice}");
        }
    }

    #[test]
    fn memory_reconcile_when_equal_sets_marker_with_notice_count_zero() {
        let mut settings = UserSettings {
            memory_enabled: false,
            dream_enabled: true,
            observation_threshold: 25,
            dream_cooldown_secs: 60,
            ..UserSettings::default()
        };
        let notices = reconcile_memory_owner(&mut settings, &daemon_memory_json())
            .into_iter()
            .count();
        assert!(settings.memory_owner_migrated);
        assert_eq!(notices, 0);
    }

    #[test]
    fn memory_reconcile_with_marker_present_emits_no_notice() {
        let mut settings = UserSettings::default();
        assert!(reconcile_memory_owner(&mut settings, &daemon_memory_json()).is_some());
        let notices = reconcile_memory_owner(&mut settings, &daemon_memory_json())
            .into_iter()
            .count();
        assert_eq!(notices, 0);
        assert!(settings.memory_owner_migrated);
    }

    #[test]
    fn bootstrap_sync_key_set_equals_tui_owned_fields() {
        let app = crate::app::app_test_helpers::with_session_list(0);
        let keys: std::collections::BTreeSet<_> = app
            .bootstrap_config_syncs()
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        let expected: std::collections::BTreeSet<_> = [
            "title_model_local",
            "title_model_fallback",
            "title_model_provider",
            "title_model_base_url",
            "title_model_api_key",
            "memory_model_local",
            "memory_model_fallback",
            "memory_model_fallback_provider",
            "memory_model_fallback_base_url",
            "memory_model_fallback_api_key",
            "dream_model",
            "dream_model_provider",
            "dream_model_base_url",
            "dream_model_api_key",
            "prompt_compile_model_local",
            "prompt_compile_model_provider",
            "prompt_compile_model_base_url",
            "prompt_compile_model_api_key",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        assert_eq!(keys, expected);
    }

    fn report(considered: u32) -> SandboxBuildCacheReclaimReport {
        SandboxBuildCacheReclaimReport {
            version: 1,
            dry_run: true,
            enabled: true,
            config: SandboxBuildCacheReclaimConfig {
                enabled: true,
                ttl_secs: 21_600,
                interval_secs: 3_600,
                high_watermark_pct: 85,
                low_watermark_pct: 75,
                max_candidates: 64,
            },
            pressure_active_before: false,
            pressure_active_after: false,
            filesystem_before: SandboxFilesystemStats {
                total_bytes: 100,
                available_bytes: 40,
                used_bytes: 60,
                used_percent: 60,
            },
            filesystem_after: SandboxFilesystemStats {
                total_bytes: 100,
                available_bytes: 40,
                used_bytes: 60,
                used_percent: 60,
            },
            candidates_considered: considered,
            eligible_candidates: 0,
            skip_counts: BTreeMap::new(),
            would_reclaim_count: 0,
            would_reclaim_bytes: 0,
            staged_count: 0,
            staged_bytes: 0,
            newly_staged_count: 0,
            newly_staged_bytes: 0,
            recovered_count: 0,
            recovered_bytes: 0,
            pending_count: 0,
            pending_bytes: 0,
            fully_removed_count: 0,
            reclaimed_bytes: 0,
            stopped_at_low_watermark: false,
            candidate_budget_exhausted: false,
            stop_reason: SandboxBuildCacheReclaimStopReason::Completed,
        }
    }

    #[test]
    fn sandbox_storage_status_transitions_coalesce_and_fence_generations() {
        let mut coordinator = BootstrapCoordinator::default();
        coordinator.storage_admission_ready = true;
        assert!(matches!(
            coordinator.storage_status,
            SandboxStorageStatus::Unknown
        ));

        let (first, _) = coordinator
            .prepare_automatic_storage_refresh()
            .expect("first automatic refresh starts");
        assert!(matches!(
            coordinator.storage_status,
            SandboxStorageStatus::RefreshingUnknown
        ));
        assert!(coordinator.prepare_automatic_storage_refresh().is_none());
        assert!(coordinator.storage_follow_up);

        let (changed, follow_up, _) = coordinator.finish_storage_refresh(first, Ok(report(1)));
        assert!(changed);
        assert!(follow_up);
        assert!(matches!(
            coordinator.storage_status,
            SandboxStorageStatus::Fresh { .. }
        ));

        let (second, _) = coordinator
            .prepare_automatic_storage_refresh()
            .expect("coalesced follow-up starts once");
        assert!(matches!(
            coordinator.storage_status,
            SandboxStorageStatus::RefreshingStale { .. }
        ));
        let direct = coordinator.begin_direct_storage_refresh();
        assert!(direct > second);
        assert!(coordinator.prepare_automatic_storage_refresh().is_none());
        assert!(coordinator.storage_follow_up);
        let (changed, _, _) = coordinator.finish_storage_refresh(second, Ok(report(2)));
        assert!(!changed, "older completion must be generation-fenced");
        let (changed, follow_up, _) =
            coordinator.finish_storage_refresh(direct, Err("preview failed".into()));
        assert!(changed);
        assert!(
            follow_up,
            "one coalesced refresh must follow the direct action"
        );
        assert!(matches!(
            coordinator.storage_status,
            SandboxStorageStatus::Error {
                last_success_at: Some(_),
                ..
            }
        ));

        let (retry, _) = coordinator
            .prepare_automatic_storage_refresh()
            .expect("retry after error starts");
        assert!(matches!(
            coordinator.storage_status,
            SandboxStorageStatus::RefreshingUnknown
        ));
        let (changed, follow_up, _) =
            coordinator.finish_storage_refresh(retry, Err("retry failed".into()));
        assert!(changed);
        assert!(!follow_up);
        assert!(matches!(
            coordinator.storage_status,
            SandboxStorageStatus::Error {
                last_success_at: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn automatic_storage_preview_retries_store_busy_once_per_bootstrap_attempt() {
        let mut coordinator = BootstrapCoordinator::default();
        coordinator.storage_admission_ready = true;
        let (first, _) = coordinator
            .prepare_automatic_storage_refresh()
            .expect("first automatic preview");
        let mut busy = report(1);
        busy.skip_counts
            .insert(SandboxBuildCacheReclaimSkipReason::StoreBusy, 1);
        let (changed, follow_up, store_busy_retry) =
            coordinator.finish_storage_refresh(first, Ok(busy.clone()));
        assert!(changed);
        assert!(store_busy_retry);
        assert!(
            follow_up,
            "transient snapshot-metadata contention gets one automatic retry"
        );
        assert!(matches!(
            coordinator.storage_status,
            SandboxStorageStatus::RefreshingUnknown
        ));

        let (retry, _) = coordinator
            .prepare_automatic_storage_refresh()
            .expect("bounded StoreBusy retry");
        let (changed, follow_up, store_busy_retry) =
            coordinator.finish_storage_refresh(retry, Ok(busy));
        assert!(changed);
        assert!(!follow_up, "StoreBusy retry is bounded for this attempt");
        assert!(!store_busy_retry);

        coordinator.prepare_handshake();
        assert!(
            coordinator.storage_busy_retry_available,
            "a new bootstrap attempt owns a fresh bounded retry"
        );
    }

    #[test]
    fn initial_snapshot_restoration_is_apply_once() {
        let mut coordinator = BootstrapCoordinator::default();
        assert!(coordinator.take_initial_snapshot_application());
        assert!(!coordinator.take_initial_snapshot_application());
    }

    #[test]
    fn optional_refresh_generations_do_not_clear_or_replace_newer_work() {
        let mut coordinator = BootstrapCoordinator::default();
        coordinator.attempt = 7;

        let snapshot_usage = coordinator.prepare_snapshot_usage();
        let (_, usage_first, _) = coordinator
            .prepare_usage_refresh()
            .expect("first usage refresh starts");
        assert!(
            !coordinator.finish_usage_refresh(snapshot_usage),
            "an older snapshot usage result cannot settle newer optional work"
        );
        assert_eq!(
            coordinator.active_usage_refresh_generation,
            Some(usage_first)
        );
        coordinator.usage_refresh_handle = None;
        assert!(coordinator.finish_usage_refresh(usage_first));
        let (_, usage_second, _) = coordinator
            .prepare_usage_refresh()
            .expect("second usage refresh starts");
        assert!(!coordinator.finish_usage_refresh(usage_first));
        assert_eq!(
            coordinator.active_usage_refresh_generation,
            Some(usage_second)
        );
        assert!(coordinator.finish_usage_refresh(usage_second));

        let (_, config_first, _) = coordinator
            .prepare_config_refresh()
            .expect("first config refresh starts");
        coordinator.config_refresh_handle = None;
        assert!(coordinator.finish_config_refresh(config_first));
        let (_, config_second, _) = coordinator
            .prepare_config_refresh()
            .expect("second config refresh starts");
        assert!(!coordinator.finish_config_refresh(config_first));
        assert_eq!(
            coordinator.active_config_refresh_generation,
            Some(config_second)
        );
        assert!(coordinator.finish_config_refresh(config_second));
    }

    #[test]
    fn post_handshake_storage_preview_survives_a_prehandshake_direct_request() {
        let mut coordinator = BootstrapCoordinator::default();
        let direct_generation = coordinator.begin_direct_storage_refresh();
        let _ = coordinator.prepare_handshake().expect("handshake starts");

        assert!(!coordinator.storage_started_for_attempt);
        assert!(coordinator.prepare_automatic_storage_refresh().is_none());
        assert!(coordinator.admit_automatic_storage_refresh());
        assert!(coordinator.prepare_automatic_storage_refresh().is_none());
        assert!(coordinator.storage_follow_up);
        let (_, follow_up, _) =
            coordinator.finish_storage_refresh(direct_generation, Ok(report(3)));
        assert!(
            follow_up,
            "post-handshake preview remains scheduled after direct work"
        );
    }

    #[test]
    fn automatic_storage_is_owned_post_handshake_but_admitted_after_session_identity() {
        let mut coordinator = BootstrapCoordinator::default();
        let _ = coordinator.prepare_handshake().expect("handshake starts");

        assert!(coordinator.prepare_automatic_storage_refresh().is_none());
        assert!(coordinator.storage_requested_for_attempt);
        assert!(!coordinator.storage_started_for_attempt);
        assert!(coordinator.active_storage_generation.is_none());
        assert!(matches!(
            coordinator.storage_status,
            SandboxStorageStatus::RefreshingUnknown
        ));

        assert!(coordinator.admit_automatic_storage_refresh());
        let (generation, _) = coordinator
            .prepare_automatic_storage_refresh()
            .expect("identity settlement admits the automatic preview");
        assert_eq!(coordinator.active_storage_generation, Some(generation));
        assert!(coordinator.storage_started_for_attempt);
    }

    #[tokio::test]
    async fn essential_handshake_is_single_flight_and_retry_bounded() {
        let mut coordinator = BootstrapCoordinator::default();
        let (attempt, _) = coordinator.prepare_handshake().expect("first handshake");
        coordinator.handshake_handle = Some(tokio::spawn(async {}));
        assert!(coordinator.prepare_handshake().is_none());
        assert_eq!(coordinator.attempt(), attempt);
        coordinator.handshake_handle.take().unwrap().abort();
        coordinator.schedule_retry();
        assert!(coordinator.prepare_handshake().is_none());
        coordinator.retry_at = Some(Instant::now());
        let (retry_attempt, _) = coordinator
            .prepare_handshake()
            .expect("one retry starts after its bounded delay");
        assert_eq!(retry_attempt, attempt + 1);
    }
}
