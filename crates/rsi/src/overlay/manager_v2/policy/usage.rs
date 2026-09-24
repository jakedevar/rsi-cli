//! Creation and active-session usage for the policy editor header (#674).
//!
//! Lifetime created sessions against `max_created_sessions` (the count the
//! daemon's admission gate charges) and the active cohort, loaded in the
//! background from operator Inspect Resources so opening Policy never blocks.
use super::{PolicyState, catalog};
use crate::{client::DaemonClient, types::OverlayState};
use rsi_common::harness_manager_v2::{
    AgentManagerInspectRequestV2, GetHarnessManagerStateRequestV2, ManagerInspectSectionV2,
    ManagerInspectionV2,
};
use std::path::PathBuf;
use tokio::{
    sync::oneshot::{self, error::RecvError},
    task::JoinHandle,
};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreationUsage {
    pub created: i64,
    pub created_limit: Option<i64>,
    pub created_remaining: Option<i64>,
    pub active: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsageView {
    Loading,
    Loaded(CreationUsage),
    Failed(String),
    /// The snapshot belonged to a previous appointment scope and was
    /// discarded; it is never shown against the new scope.
    Stale(&'static str),
}

/// Reads the manager-wide `resources` row of an Inspect Resources page.
///
/// # Errors
/// Returns a display reason when the row is missing, unknown, or lacks usage.
pub fn usage_from_resources(page: &ManagerInspectionV2) -> Result<CreationUsage, String> {
    let row = page
        .rows
        .iter()
        .find(|row| row["type"] == "resources")
        .ok_or("Resources row unavailable")?;
    if row["state"] == "unknown" {
        return Err(row["reason"]
            .as_str()
            .unwrap_or("Resources unknown")
            .to_string());
    }
    let created = &row["created_sessions"];
    Ok(CreationUsage {
        created: created["used"]
            .as_i64()
            .ok_or("Resources row has no created_sessions")?,
        created_limit: created["limit"].as_i64(),
        created_remaining: created["remaining"].as_i64(),
        active: row["active_sessions"]
            .as_i64()
            .ok_or("Resources row has no active_sessions")?,
    })
}

impl UsageView {
    /// Header line. `created_limit` is a quota saved or reloaded after the
    /// snapshot (so a save shows the new cap at once); otherwise the daemon's
    /// reported limit. Remaining is recomputed from it. `active_limit` is the
    /// saved active session limit.
    #[must_use]
    pub fn render(
        &self,
        created_limit: Option<u16>,
        active_limit: u16,
        refreshing: bool,
    ) -> String {
        match self {
            Self::Loading => "Usage: loading · 5 refreshes".into(),
            Self::Loaded(u) => {
                let limit = created_limit.map(i64::from).or(u.created_limit);
                let created = limit.map_or_else(
                    || format!("{} created · no saved quota", u.created),
                    |limit| {
                        format!(
                            "{}/{limit} created · {} left",
                            u.created,
                            (limit - u.created).max(0)
                        )
                    },
                );
                format!(
                    "Usage: sessions {created} · active {}/{active_limit}{}",
                    u.active,
                    if refreshing { " · refreshing" } else { "" }
                )
            }
            Self::Failed(error) => format!("Usage unavailable: {error} · 5 refreshes"),
            Self::Stale(reason) => format!("Usage: {reason} · 5 refreshes"),
        }
    }
}

pub struct UsageRequest {
    pub receiver: oneshot::Receiver<Result<CreationUsage, String>>,
    /// `PolicyState::usage_generation` when the request started (K15B-1-RACE).
    pub generation: u64,
    task: JoinHandle<()>,
}
impl Drop for UsageRequest {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Starts one background Inspect Resources read on its own connection.
#[must_use]
pub fn request_usage(socket: PathBuf, project_id: Uuid) -> UsageRequest {
    let (send, receiver) = oneshot::channel();
    let task = tokio::spawn(async move {
        let mut client = DaemonClient::new(socket);
        let outcome = match client.connect().await {
            Ok(()) => client
                .get_harness_manager_state(GetHarnessManagerStateRequestV2 {
                    project_id,
                    query: AgentManagerInspectRequestV2 {
                        section: ManagerInspectSectionV2::Resources,
                        ..Default::default()
                    },
                })
                .await
                .map_err(|e| e.to_string())
                .and_then(|page| usage_from_resources(&page)),
            Err(e) => Err(e.to_string()),
        };
        let _ = send.send(outcome);
    });
    UsageRequest {
        receiver,
        generation: 0,
        task,
    }
}

impl PolicyState {
    /// (Re)loads usage in the background; a loaded value stays visible until
    /// the fresh one arrives. Dropping a prior request aborts it.
    pub fn start_usage(&mut self, socket: PathBuf) {
        if matches!(self.usage, UsageView::Failed(_) | UsageView::Stale(_)) {
            self.usage = UsageView::Loading;
        }
        self.usage_request = None;
        let mut request = request_usage(socket.clone(), self.config.project_id);
        request.generation = self.usage_generation;
        self.usage_request = Some(request);
        self.usage_socket = Some(socket);
    }
    /// A save or same-scope reload makes every in-flight snapshot older than
    /// the quota now shown; only a request started after this is applied.
    pub const fn bump_usage_generation(&mut self) {
        self.usage_generation = self.usage_generation.wrapping_add(1);
    }
    /// Reload keeps the usage view and any in-flight request of the state it
    /// replaces only when the appointment scope is unchanged. Counts from a
    /// previous scope are discarded (the old request is dropped, aborting it)
    /// and marked stale until `5` refetches them.
    pub fn adopt_usage(&mut self, previous: &mut Self) {
        let same_scope = previous.config.project_id == self.config.project_id
            && previous.config.manager_session_id == self.config.manager_session_id
            && previous.config.row_version == self.config.row_version;
        self.usage_socket.clone_from(&previous.usage_socket);
        self.usage_generation = previous.usage_generation;
        if same_scope {
            self.usage = std::mem::replace(&mut previous.usage, UsageView::Loading);
            self.usage_request = previous.usage_request.take();
            // The reloaded policy is newer than the kept snapshot and than any
            // retained in-flight request.
            self.usage_quota = self.saved.as_ref().map(|p| p.policy.max_created_sessions);
            self.bump_usage_generation();
        } else {
            self.usage = UsageView::Stale("scope changed");
            self.usage_request = None;
        }
    }
    pub fn apply_usage(&mut self, result: Result<Result<CreationUsage, String>, RecvError>) {
        let generation = self.usage_request.take().map(|r| r.generation);
        if generation.is_some_and(|g| g < self.usage_generation) {
            // Started before a save or reload: drop it, keep the saved quota,
            // and fetch a snapshot that postdates the change.
            if let Some(socket) = self.usage_socket.clone() {
                self.start_usage(socket);
            }
            return;
        }
        // A current snapshot carries the daemon's current limit.
        self.usage_quota = None;
        self.usage = match result {
            Ok(Ok(usage)) => UsageView::Loaded(usage),
            Ok(Err(error)) => UsageView::Failed(error),
            Err(_) => UsageView::Failed("request ended".into()),
        };
    }
    #[must_use]
    pub fn usage_summary(&self) -> String {
        self.usage.render(
            self.usage_quota,
            self.opened_draft.max_active_sessions,
            self.usage_request.is_some(),
        )
    }
}

/// One policy-editor background result: a launch catalog or usage.
pub enum PolicyAsyncResult {
    Catalog(Result<catalog::ManagerCatalogResult, RecvError>),
    Usage(Result<Result<CreationUsage, String>, RecvError>),
}

/// Awaits whichever policy-owned background request finishes first.
pub async fn next_result(overlay: &mut OverlayState) -> PolicyAsyncResult {
    if let OverlayState::HarnessManagerV2(view) = overlay
        && let Some(state) = view.policy.as_mut()
    {
        let catalog = state.catalog_request.as_mut().map(|r| &mut r.receiver);
        let usage = state.usage_request.as_mut().map(|r| &mut r.receiver);
        return match (catalog, usage) {
            (Some(catalog), Some(usage)) => tokio::select! {
                result = catalog => PolicyAsyncResult::Catalog(result),
                result = usage => PolicyAsyncResult::Usage(result),
            },
            (Some(catalog), None) => PolicyAsyncResult::Catalog(catalog.await),
            (None, Some(usage)) => PolicyAsyncResult::Usage(usage.await),
            (None, None) => std::future::pending().await,
        };
    }
    std::future::pending().await
}

pub fn dispatch_result(overlay: &mut OverlayState, result: PolicyAsyncResult) {
    match result {
        PolicyAsyncResult::Catalog(result) => catalog::dispatch_result(overlay, result),
        PolicyAsyncResult::Usage(result) => {
            if let OverlayState::HarnessManagerV2(view) = overlay
                && let Some(state) = view.policy.as_mut()
            {
                state.apply_usage(result);
            }
        }
    }
}
