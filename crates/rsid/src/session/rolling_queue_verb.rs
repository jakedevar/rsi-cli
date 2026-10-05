//! `AgentEnqueueLandingSource` (#1007 S1): admit one accepted source onto the
//! daemon-owned rolling merge queue.
//!
//! Authority is the current appointed manager or the current lead of the
//! caller's Epic, resolved from the token-bound caller by the same projection
//! the other Agent verbs advertise from. Workers are refused
//! `queue_not_authorized`.

use super::agent_verbs::AgentControlHandle;
use crate::error::{DaemonError, Result};
use crate::store::rolling_queue::NewQueueEntry;
use rsi_common::rolling_queue::{
    AgentEnqueueLandingSourceReceiptV1, AgentEnqueueLandingSourceRequestV1, QUEUE_DISABLED,
    QUEUE_FILTER_MATCHES_NO_TESTS, QUEUE_NOT_AUTHORIZED, QUEUE_SOURCE_INVALID, RollingQueueBinding,
};
use std::path::PathBuf;
use uuid::Uuid;

impl AgentControlHandle {
    /// # Errors
    /// A stable `queue_*` refusal, or a persistence error.
    pub async fn agent_enqueue_landing_source(
        &self,
        caller: Uuid,
        request: AgentEnqueueLandingSourceRequestV1,
        queue_enabled: bool,
    ) -> Result<AgentEnqueueLandingSourceReceiptV1> {
        request
            .validate()
            .map_err(|code| DaemonError::InvalidParam(code.into()))?;
        let (session, is_lead) = {
            let store = self.store.lock().await;
            let projection = store
                .agent_authority_projection(caller)
                .map_err(|_| DaemonError::PolicyDenied(QUEUE_NOT_AUTHORIZED.into()))?;
            if !(projection.is_lead || projection.is_manager) {
                return Err(DaemonError::PolicyDenied(QUEUE_NOT_AUTHORIZED.into()));
            }
            let session = store
                .get_session(caller)?
                .ok_or_else(|| DaemonError::PolicyDenied(QUEUE_NOT_AUTHORIZED.into()))?;
            (session, projection.is_lead)
        };
        if !queue_enabled {
            return Err(DaemonError::InvalidParam(QUEUE_DISABLED.into()));
        }
        let repo: PathBuf = session
            .sandbox_root
            .clone()
            .unwrap_or_else(|| session.working_dir.clone());
        let source = request.source_commit.clone();
        let probe_repo = repo.clone();
        let filters = request.test_filters.clone();
        let (facts, empty_filter) = tokio::task::spawn_blocking(move || {
            crate::rolling_queue::source_commit_exists(&probe_repo, &source).then(|| {
                (
                    crate::rolling_queue::derive_source_facts(&probe_repo, &source),
                    crate::rolling_queue::filter_selecting_no_tests(&probe_repo, &source, &filters),
                )
            })
        })
        .await
        .map_err(|error| DaemonError::Process(format!("enqueue source probe: {error}")))?
        .ok_or_else(|| DaemonError::InvalidParam(QUEUE_SOURCE_INVALID.into()))?;
        if let Some(filter) = empty_filter {
            return Err(DaemonError::InvalidParam(format!(
                "{QUEUE_FILTER_MATCHES_NO_TESTS}: {filter}"
            )));
        }
        let new = NewQueueEntry {
            project_id: session.project_id,
            repo_path: repo.display().to_string(),
            source_commit: request.source_commit,
            source_session_id: caller,
            owner_epic_id: is_lead.then_some(session.parent_id).flatten(),
            binding: RollingQueueBinding::Unbound,
            work_key: None,
            migration_version: facts.migration_version,
            hot_files: facts.hot_files,
            test_filters: request.test_filters,
            idempotency_key: request.idempotency_key,
        };
        let (entry, replayed) = self
            .store
            .lock()
            .await
            .enqueue_rolling_queue_source(&new, chrono::Utc::now())?;
        Ok(AgentEnqueueLandingSourceReceiptV1 { entry, replayed })
    }
}
