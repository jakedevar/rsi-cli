//! `AgentEnqueueLandingSource` (#1007 S1): admit one accepted source onto the
//! daemon-owned rolling merge queue.
//!
//! Authority is the current appointed manager or the current lead of the
//! caller's Epic, resolved from the token-bound caller by the same projection
//! the other Agent verbs advertise from. Workers are refused
//! `queue_not_authorized`.

use super::agent_verbs::AgentControlHandle;
use crate::error::{DaemonError, Result};
use crate::store::Store;
use crate::store::rolling_queue::NewQueueEntry;
use crate::topology::land::{LandEnqueue, LandRequest, LandStatus};
use rsi_common::rolling_queue::{
    AgentEnqueueLandingSourceReceiptV1, AgentEnqueueLandingSourceRequestV1, QUEUE_DISABLED,
    QUEUE_FILTER_MATCHES_NO_TESTS, QUEUE_NOT_AUTHORIZED, QUEUE_SOURCE_INVALID, RollingQueueBinding,
    RollingQueueEntryState,
};
use rsi_common::types::Session;
use std::path::PathBuf;
use uuid::Uuid;

/// The admission shared by the first resolution and the effect-time recheck:
/// the session whose sandbox is queued, whether the caller is its Epic lead,
/// and the authority fence.
fn admit(
    store: &Store,
    caller: Uuid,
    request: &AgentEnqueueLandingSourceRequestV1,
) -> Result<(Session, bool, String)> {
    let (session, is_lead) = match request.source_session_id {
        // #1235: land from an in-reach session's sandbox.
        Some(source) => (
            super::agent_jobs_verb::manager_reached_sandbox(
                store,
                caller,
                source,
                request.project_id,
                QUEUE_NOT_AUTHORIZED,
            )?,
            false,
        ),
        None => {
            let projection = store
                .agent_authority_projection(caller)
                .map_err(|_| DaemonError::PolicyDenied(QUEUE_NOT_AUTHORIZED.into()))?;
            if !(projection.is_lead || projection.is_manager) {
                return Err(DaemonError::PolicyDenied(QUEUE_NOT_AUTHORIZED.into()));
            }
            let session = store
                .get_session(caller)?
                .ok_or_else(|| DaemonError::PolicyDenied(QUEUE_NOT_AUTHORIZED.into()))?;
            if request
                .project_id
                .is_some_and(|project| session.project_id != Some(project))
            {
                return Err(DaemonError::InvalidParam(
                    rsi_common::global_manager::MANAGER_PROJECT_NOT_IN_SCOPE.into(),
                ));
            }
            (session, projection.is_lead)
        }
    };
    let fence = store
        .agent_authority_fence(caller, request.source_session_id)
        .map_err(|_| DaemonError::PolicyDenied(QUEUE_NOT_AUTHORIZED.into()))?;
    Ok((session, is_lead, fence))
}

/// One source to admit onto the queue, resolved by the caller's admission.
pub(crate) struct QueueSource {
    pub(crate) project_id: Option<Uuid>,
    /// The checkout the lander fetches the source from.
    pub(crate) repo: PathBuf,
    /// The entry's identity with `idempotency_key` (the replay key).
    pub(crate) source_session_id: Uuid,
    pub(crate) owner_epic_id: Option<Uuid>,
    pub(crate) commit: String,
    pub(crate) test_filters: Vec<String>,
    pub(crate) idempotency_key: String,
    /// Where the settlement wake goes when it is not the source session.
    pub(crate) wake_session_id: Option<Uuid>,
}

/// The git probe shared by every enqueue path: the source must exist in
/// `repo`, and no filter may provably select no test. Runs without the store
/// lock.
///
/// # Errors
/// `queue_source_invalid`, or `filter_matches_no_tests: <filter>`.
pub(crate) async fn probe_source(
    repo: &std::path::Path,
    commit: &str,
    filters: &[String],
) -> Result<crate::rolling_queue::SourceFacts> {
    let (probe_repo, source, filters) = (repo.to_path_buf(), commit.to_owned(), filters.to_vec());
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
    Ok(facts)
}

/// The insert shared by every enqueue path, under the caller's re-resolved
/// admission (the store lock stays held from the re-check through here).
pub(crate) fn insert_source(
    store: &Store,
    source: &QueueSource,
    facts: &crate::rolling_queue::SourceFacts,
) -> Result<(rsi_common::rolling_queue::RollingQueueEntryV1, bool)> {
    let new = NewQueueEntry {
        project_id: source.project_id,
        repo_path: source.repo.display().to_string(),
        source_commit: source.commit.clone(),
        source_session_id: source.source_session_id,
        owner_epic_id: source.owner_epic_id,
        binding: RollingQueueBinding::Unbound,
        work_key: None,
        migration_version: facts.migration_version,
        hot_files: facts.hot_files.clone(),
        test_filters: source.test_filters.clone(),
        idempotency_key: source.idempotency_key.clone(),
    };
    store.enqueue_rolling_queue_source_for(&new, source.wake_session_id, chrono::Utc::now())
}

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
        let (session, is_lead, fence) = {
            let store = self.store.lock().await;
            admit(&store, caller, &request)?
        };
        if !queue_enabled {
            return Err(DaemonError::InvalidParam(QUEUE_DISABLED.into()));
        }
        let repo: PathBuf = session
            .sandbox_root
            .clone()
            .unwrap_or_else(|| session.working_dir.clone());
        let facts = probe_source(&repo, &request.source_commit, &request.test_filters).await?;
        #[cfg(test)]
        super::effect_fence::seam::run(&self.store, caller, "enqueue").await;
        // #1277: the lock was released for the Git probe. Re-resolve the same
        // admission under the lock that stays held through the insert and
        // refuse if caller, grant, project, reach or custody changed.
        let store = self.store.lock().await;
        let (current, _, current_fence) = admit(&store, caller, &request)?;
        super::effect_fence::require_unchanged(&fence, &current_fence, QUEUE_NOT_AUTHORIZED)?;
        let current_repo = current
            .sandbox_root
            .clone()
            .unwrap_or_else(|| current.working_dir.clone());
        if current_repo != repo {
            return Err(DaemonError::PolicyDenied(QUEUE_NOT_AUTHORIZED.into()));
        }
        let (entry, replayed) = insert_source(
            &store,
            &QueueSource {
                project_id: session.project_id,
                repo,
                source_session_id: caller,
                owner_epic_id: is_lead.then_some(session.parent_id).flatten(),
                commit: request.source_commit,
                test_filters: request.test_filters,
                idempotency_key: request.idempotency_key,
                wake_session_id: None,
            },
            &facts,
        )?;
        Ok(AgentEnqueueLandingSourceReceiptV1 { entry, replayed })
    }

    /// #1641 S2: enqueue a topology land node's accepted commit on behalf of
    /// the execution's owning manager or Epic lead. The admission (the
    /// requester's live authority over the Epic and the review's admitted
    /// acceptance of exactly this commit) is re-derived from rows before the
    /// probe and again under the lock that stays held through the insert. The
    /// entry's replay identity is the reviewed author session plus the
    /// attempt's dedup key, so a duplicate enqueue returns the same entry; the
    /// settlement wake goes to the owner, never to a finished node session.
    ///
    /// # Errors
    /// A transient persistence or process error (asked again next tick);
    /// refusals are [`LandEnqueue`] values.
    pub(crate) async fn enqueue_for_topology(
        &self,
        request: &LandRequest,
        queue_enabled: bool,
    ) -> Result<LandEnqueue> {
        self.enqueue_topology_admitted(request, queue_enabled, review_admits)
            .await
    }

    /// [`Self::enqueue_for_topology`] with the review acceptance taken as given.
    #[cfg(test)]
    pub(crate) async fn enqueue_topology_admitted_for_test(
        &self,
        request: &LandRequest,
        queue_enabled: bool,
    ) -> Result<LandEnqueue> {
        self.enqueue_topology_admitted(request, queue_enabled, |_, _| Ok(true))
            .await
    }

    /// [`Self::enqueue_topology_admitted_for_test`], running `hook` at the
    /// probe boundary: after the first admission and the Git probe, before
    /// the insert's re-check.
    #[cfg(test)]
    pub(crate) async fn enqueue_topology_racing_for_test(
        &self,
        request: &LandRequest,
        hook: impl FnOnce(&Store) + Send + 'static,
    ) -> Result<LandEnqueue> {
        super::effect_fence::seam::install(request.execution_id, "topology_enqueue", hook);
        self.enqueue_topology_admitted_for_test(request, true).await
    }

    /// [`Self::enqueue_for_topology`] with the review-acceptance check
    /// injected, so a test can reach the probe boundary without a full review.
    async fn enqueue_topology_admitted(
        &self,
        request: &LandRequest,
        queue_enabled: bool,
        review_admitted: ReviewAdmitted,
    ) -> Result<LandEnqueue> {
        let owner = {
            let store = self.store.lock().await;
            match topology_admission(&store, request, review_admitted)? {
                Ok(owner) => owner,
                Err(reason) => return Ok(LandEnqueue::AdmissionLost(reason)),
            }
        };
        if !queue_enabled {
            return Ok(LandEnqueue::QueueDisabled);
        }
        let facts = match probe_source(
            &request.repo_root,
            &request.source_commit,
            &request.test_filters,
        )
        .await
        {
            Ok(facts) => facts,
            Err(DaemonError::InvalidParam(code)) => return Ok(LandEnqueue::Refused(code)),
            Err(error) => return Err(error),
        };
        #[cfg(test)]
        super::effect_fence::seam::run(&self.store, request.execution_id, "topology_enqueue").await;
        let store = self.store.lock().await;
        // The lock was released for the Git probe: refuse if the owner, the
        // acceptance or the scope changed meanwhile.
        match topology_admission(&store, request, review_admitted)? {
            Ok(current) if current == owner => {}
            Ok(_) => {
                return Ok(LandEnqueue::AdmissionLost(
                    "the execution's owner changed while the landing was prepared".into(),
                ));
            }
            Err(reason) => return Ok(LandEnqueue::AdmissionLost(reason)),
        }
        // A cancellation committed during the probe (owner and acceptance are
        // not revoked by it) must not create a NEW entry; an entry created
        // before it is still adopted so the attempt mirrors what the queue
        // may publish. The check holds the same lock through the insert.
        if topology_land_entry(&store, request)?.is_none() && !topology_land_live(&store, request)?
        {
            return Ok(LandEnqueue::Cancelled);
        }
        let source = QueueSource {
            project_id: request.project_id,
            repo: request.repo_root.clone(),
            source_session_id: request.author_session_id,
            owner_epic_id: request.epic_id,
            commit: request.source_commit.clone(),
            test_filters: request.test_filters.clone(),
            idempotency_key: request.dedup_key.clone(),
            wake_session_id: owner,
        };
        match insert_source(&store, &source, &facts) {
            Ok((entry, _)) => Ok(LandEnqueue::Queued(entry.id)),
            Err(DaemonError::InvalidParam(code)) if code.starts_with("queue_") => {
                Ok(LandEnqueue::Refused(code))
            }
            Err(error) => Err(error),
        }
    }
}

/// The current admission of a topology landing: `Ok(owner)` carries the
/// session that owns the execution (the settlement wake target); `Err` is the
/// short reason the landing is no longer admitted. Pure over rows.
fn topology_admission(
    store: &Store,
    request: &LandRequest,
    review_admitted: ReviewAdmitted,
) -> Result<std::result::Result<Option<Uuid>, String>> {
    let Some(execution) = crate::topology::store::load_execution(store, request.execution_id)?
    else {
        return Ok(Err("the topology execution no longer exists".into()));
    };
    if let Some(code) = crate::topology::agent::landing_gate(store, &execution)? {
        return Ok(Err(format!("land_not_authorized: {code}")));
    }
    if !review_admitted(store, request)? {
        return Ok(Err(
            "the review no longer admits this commit as accepted".into()
        ));
    }
    Ok(Ok(execution.requested_by_session_id))
}

/// Whether the accepted review still admits exactly the landing's commit.
type ReviewAdmitted = fn(&Store, &LandRequest) -> Result<bool>;

fn review_admits(store: &Store, request: &LandRequest) -> Result<bool> {
    store.topology_land_admitted(request.review_assignment_id, &request.source_commit)
}

/// Whether the landing is still the live work of a running execution: the
/// execution is `running` (not cancelling or settled) and the request's land
/// attempt is still its reserved or launching attempt. Pure over rows.
fn topology_land_live(store: &Store, request: &LandRequest) -> Result<bool> {
    use crate::topology::store::{self as rows, AttemptStatus, ExecutionStatus};
    let running = rows::load_execution(store, request.execution_id)?
        .is_some_and(|execution| execution.status == ExecutionStatus::Running);
    let reserved =
        rows::load_attempt(store, request.attempt_id)?.is_some_and(|(execution, attempt)| {
            execution == request.execution_id
                && matches!(
                    attempt.status,
                    AttemptStatus::Reserved | AttemptStatus::Launching
                )
        });
    Ok(running && reserved)
}

/// The queue entry a topology landing already created, if any.
pub(crate) fn topology_land_entry(store: &Store, request: &LandRequest) -> Result<Option<Uuid>> {
    Ok(store
        .get_rolling_queue_entry_by_key(request.author_session_id, &request.dedup_key)?
        .map(|entry| entry.id))
}

/// The queue entry's state as a land node mirrors it.
pub(crate) fn topology_land_status(store: &Store, entry_id: Uuid) -> Result<LandStatus> {
    let Some(entry) = store.get_rolling_queue_entry(entry_id)? else {
        return Ok(LandStatus::Refused {
            state: "missing".into(),
            reason: "queue_entry_missing".into(),
        });
    };
    Ok(match entry.state {
        RollingQueueEntryState::Queued
        | RollingQueueEntryState::Admitted
        | RollingQueueEntryState::Gating => LandStatus::Pending,
        RollingQueueEntryState::Published => LandStatus::Published {
            landed_sha: entry.outcome.and_then(|outcome| outcome.landed_sha),
        },
        state @ (RollingQueueEntryState::Refused
        | RollingQueueEntryState::Failed
        | RollingQueueEntryState::Superseded) => LandStatus::Refused {
            state: state.as_str().to_owned(),
            reason: entry
                .outcome
                .and_then(|outcome| outcome.refusal.or(outcome.detail))
                .unwrap_or_else(|| state.as_str().to_owned()),
        },
    })
}
