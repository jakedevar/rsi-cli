//! Read-only query methods: get/list sessions, conversations, turn metrics, health status.

use crate::error::{DaemonError, Result};
use crate::profiling;
use crate::sandbox::{AllocationPermit, SandboxAllocation, SandboxAllocator};
use rsi_common::types::{
    ConversationEvent, SandboxCleanupState, SandboxKind, Session, SessionDiagnosticV1, TurnMetric,
};
use std::collections::{HashMap, HashSet};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::sync::RwLock;
use uuid::Uuid;

use super::{CompletedSession, SessionManager, TrackedSession};

/// Reattach catalog and official descriptive facts to a persisted capability
/// tuple before it crosses an RPC boundary. V99 stores the active scalar and
/// provenance, not the full provider envelope; the exact version+digest fence
/// in the canonical registry decides whether catalog enrichment is valid.
fn rehydrate_context_budget_projection(session: &mut Session) {
    let Some(resolved) = session.resolved_context_budget.clone() else {
        return;
    };
    session.resolved_context_budget = Some(
        crate::provider_capabilities::rehydrate_resolved_context_budget(
            session.provider,
            session.model.as_deref().unwrap_or("unknown"),
            resolved,
        ),
    );
}

fn fresh_unarchive_sandbox_binding(
    session: &Session,
    sandbox_base: std::path::PathBuf,
    permit: AllocationPermit,
) -> Result<(
    SandboxAllocation,
    crate::store::sandbox_custody::SessionCustodyBinding,
)> {
    fresh_replacement_sandbox_binding(session, None, sandbox_base, permit)
}

/// Resolve `branch` in the canonical repository to its full commit id, or
/// `None` when the ref no longer exists.
fn resolve_branch_commit(working_dir: &std::path::Path, branch: &str) -> Option<String> {
    let output = Command::new("git")
        .args([
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}^{{commit}}"),
        ])
        .current_dir(working_dir)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|commit| !commit.is_empty())
}

/// Allocate a distinct replacement sandbox root for `session`. When
/// `preferred_branch` still resolves in the canonical repository, the new
/// worktree starts at that branch's tip so the prior sandbox's committed work
/// carries forward; otherwise it starts from the fresh rolling base.
pub(super) fn fresh_replacement_sandbox_binding(
    session: &Session,
    preferred_branch: Option<&str>,
    sandbox_base: std::path::PathBuf,
    permit: AllocationPermit,
) -> Result<(
    SandboxAllocation,
    crate::store::sandbox_custody::SessionCustodyBinding,
)> {
    let working_dir = session.working_dir.canonicalize().map_err(|error| {
        DaemonError::InvalidParam(format!(
            "cannot recreate sandbox: working directory '{}' is unavailable: {error}",
            session.working_dir.display()
        ))
    })?;
    if let Some(source_commit) =
        preferred_branch.and_then(|branch| resolve_branch_commit(&working_dir, branch))
    {
        let allocation = SandboxAllocator::new(sandbox_base).allocate_replacement_with_permit(
            permit,
            session.id,
            &working_dir,
            SandboxKind::GitWorktree,
            &source_commit,
            None,
        )?;
        let binding = super::launch::new_root_binding_from_allocation(
            session.id,
            &allocation,
            &working_dir,
            Some(&source_commit),
            crate::store::sandbox_custody::CustodyCause::FreshLaunch,
            None,
        )?;
        return Ok((allocation, binding));
    }
    let source_commit = Command::new("git")
        .args(["rev-parse", "--verify", "HEAD^{commit}"])
        .current_dir(&working_dir)
        .output()
        .map_err(|error| {
            DaemonError::Process(format!(
                "failed to resolve fresh sandbox source commit: {error}"
            ))
        })?;
    if !source_commit.status.success() {
        return Err(DaemonError::InvalidParam(
            "cannot recreate sandbox: source commit is unavailable".to_string(),
        ));
    }
    let source_commit = String::from_utf8_lossy(&source_commit.stdout)
        .trim()
        .to_string();
    let selection = crate::sandbox::git_worktree::fresh_rolling_base(
        &working_dir,
        Uuid::new_v4(),
        source_commit,
        crate::sandbox::git_worktree::RollingBasePolicy::RemoteTip,
    )?;
    let source_commit = selection.commit.clone();
    let allocation = selection.allocate_with_cleanup(|commit| {
        SandboxAllocator::new(sandbox_base.clone()).allocate_replacement_with_permit(
            permit,
            session.id,
            &working_dir,
            SandboxKind::GitWorktree,
            commit,
            None,
        )
    })?;
    let binding = super::launch::new_root_binding_from_allocation(
        session.id,
        &allocation,
        &working_dir,
        Some(&source_commit),
        crate::store::sandbox_custody::CustodyCause::FreshLaunch,
        None,
    )?;
    Ok((allocation, binding))
}

/// Resolve a session snapshot from the in-memory `active`/`completed` maps
/// with a store fallback, stamping the derived-on-read `context_fill_pct`.
///
/// Extracted from [`SessionManager::get_session`] so collaborators that hold
/// only the underlying `Arc`s (rather than a `&SessionManager`) — notably the
/// P2 [`super::agent_verbs::AgentControlHandle`] used by the native
/// `rsi_control` tools — resolve sessions through the exact same path the
/// RPC verbs use. Single source of truth for read semantics.
pub(super) async fn get_session_snapshot(
    active: &Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
    completed: &Arc<RwLock<HashMap<Uuid, CompletedSession>>>,
    store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
    session_id: Uuid,
) -> Option<Session> {
    if let Some(tracked) = active.read().await.get(&session_id) {
        let mut s = tracked.session.clone();
        project_approval_started_at(&mut s, tracked);
        rehydrate_context_budget_projection(&mut s);
        s.context_fill_pct = super::monitor::context_fill_pct_for_tracked(tracked);
        return Some(s);
    }
    if let Some(completed) = completed.read().await.get(&session_id) {
        let mut s = completed.session.clone();
        rehydrate_context_budget_projection(&mut s);
        s.context_fill_pct = super::monitor::context_fill_pct_from_persisted(&s);
        return Some(s);
    }
    let store = store.clone();
    match tokio::task::spawn_blocking(move || {
        let store = store.blocking_lock();
        store.get_session(session_id)
    })
    .await
    {
        Ok(Ok(session)) => session.map(|mut s| {
            rehydrate_context_budget_projection(&mut s);
            s.context_fill_pct = super::monitor::context_fill_pct_from_persisted(&s);
            s
        }),
        Ok(Err(e)) => {
            tracing::warn!(
                session_id = %session_id,
                error = %e,
                "get_session store fallback failed"
            );
            None
        }
        Err(e) => {
            tracing::warn!(
                session_id = %session_id,
                error = %e,
                "get_session store fallback task failed"
            );
            None
        }
    }
}

/// Project the in-memory `TrackedSession.approval_wait_start: Option<Instant>`
/// into a wall-clock `DateTime<Utc>` and write it into the cloned `Session`.
///
/// `Instant` is monotonic with no calendar anchor, so we derive
/// `Utc::now() - elapsed()` per call. Sub-second drift is invisible at the
/// 1Hz repaint cadence the TUI uses for the live counter.
///
/// Lives in this module (`crates/rsid/src/session/queries.rs`) — sibling to
/// `types.rs` where `approval_wait_start` is declared `pub(super)`. This
/// visibility constraint is the load-bearing reason the projection is here
/// rather than in `crates/rsid/src/rpc.rs`.
fn project_approval_started_at(session: &mut Session, tracked: &TrackedSession) {
    session.approval_started_at = tracked.approval_wait_start.map(|started| {
        let elapsed = chrono::Duration::from_std(started.elapsed()).unwrap_or_default();
        chrono::Utc::now() - elapsed
    });
}

fn is_visible_in_session_list(session: &Session) -> bool {
    !matches!(
        session.status,
        rsi_common::types::SessionStatus::Archived | rsi_common::types::SessionStatus::Deleted
    )
}

/// Load a completed session's transcript from SQLite.
///
/// C7 Phase 1: `SessionManager::restore_sessions` inserts every restored
/// Completed/Failed/Interrupted session with `events: Vec::new()` and
/// `events_hydrated: false` instead of eagerly loading the full transcript,
/// so the daemon does not pull an operator's entire session history (order of
/// a GB, in the measured case) into RAM at startup. This is the shared
/// on-demand loader callers use to hydrate the real transcript the first time
/// it's actually needed (a TUI poll, a resume, a rotation). `load_events` is
/// index-backed (`idx_events_session_sequence`), so this is a cheap query.
pub(super) async fn load_completed_events_from_store(
    store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
    session_id: Uuid,
) -> Result<Vec<ConversationEvent>> {
    let store = store.clone();
    tokio::task::spawn_blocking(move || {
        let store = store.blocking_lock();
        store.load_events(session_id)
    })
    .await
    .map_err(|e| DaemonError::Store(e.to_string()))?
}

impl SessionManager {
    /// Copy bounded selected detail under one nonblocking runtime map guard.
    /// This precedes the Store miss and the later scalar reread.
    pub(crate) fn remote_selected_runtime_session(
        &self,
        project: Uuid,
        session: Uuid,
    ) -> crate::remote_read::Result<Option<crate::remote_read::SelectedRuntimeSession>> {
        use crate::remote_read::{ReadError, SelectedRuntimeSession, SessionCandidateOrigin};

        let active = self.active.try_read().map_err(|_| ReadError::Busy)?;
        if let Some(tracked) = active.get(&session) {
            if tracked.session.project_id != Some(project) {
                return Err(ReadError::SourceUnavailable);
            }
            return SelectedRuntimeSession::capture(
                &tracked.session,
                SessionCandidateOrigin::Active,
                Some(tracked.spawn_generation),
                Some(&tracked.events),
                chrono::Utc::now(),
            )
            .map(Some);
        }
        drop(active);
        let completed = self.completed.try_read().map_err(|_| ReadError::Busy)?;
        let Some(entry) = completed.get(&session) else {
            return Ok(None);
        };
        if entry.session.project_id != Some(project) {
            return Err(ReadError::SourceUnavailable);
        }
        let selected = SelectedRuntimeSession::capture(
            &entry.session,
            SessionCandidateOrigin::Completed,
            None,
            entry.events_hydrated.then_some(entry.events.as_slice()),
            chrono::Utc::now(),
        )
        .map(Some);
        drop(completed);
        selected
    }

    /// Use the real nonblocking session and native-writer sources before the
    /// Store miss. The matching finish call runs only after Store unlock.
    pub(crate) fn remote_runtime_only_pending_capture(
        &self,
        project: Uuid,
        session: Uuid,
    ) -> crate::remote_read::Result<crate::remote_read::RuntimeOnlyPendingCapture> {
        crate::remote_read::capture_runtime_only_pending_sources(
            project,
            session,
            |project, session| self.remote_question_slot_list(project, session),
            super::pending_approvals::remote_native_approval_list,
        )
    }

    pub(crate) fn remote_runtime_only_pending_finish(
        &self,
        selected: &crate::remote_read::SelectedRuntimeOnlySession,
        capture: crate::remote_read::RuntimeOnlyPendingCapture,
    ) -> crate::remote_read::Result<crate::remote_read::RuntimeOnlyPendingSources> {
        crate::remote_read::finish_runtime_only_pending_sources(
            selected,
            capture,
            |before| self.remote_question_slot_list_recheck(before),
            super::pending_approvals::remote_native_approval_list_recheck,
        )
    }

    /// Capture real nonblocking pending sources before the saved Store read.
    pub(crate) fn remote_saved_pending_capture(
        &self,
        project: Uuid,
        session: Uuid,
    ) -> crate::remote_read::Result<crate::remote_read::SavedPendingRuntimeCapture> {
        crate::remote_read::capture_saved_pending_runtime_sources(
            project,
            session,
            |project, session| self.remote_question_slot_list(project, session),
            super::pending_approvals::remote_native_approval_list,
        )
    }

    /// Run only after the saved Store read has released its lock.
    pub(crate) fn remote_saved_pending_finish(
        &self,
        selected: &crate::remote_read::SelectedSavedSession,
        capture: crate::remote_read::SavedPendingRuntimeCapture,
        store: crate::remote_read::PendingStoreSources,
    ) -> crate::remote_read::Result<crate::remote_read::PendingAcquiredSources> {
        crate::remote_read::finish_saved_pending_sources(
            selected,
            capture,
            store,
            |before| self.remote_question_slot_list_recheck(before),
            super::pending_approvals::remote_native_approval_list_recheck,
        )
    }

    /// Copy only candidate scalars from each runtime map. Neither lock is
    /// awaited, and the active guard is dropped before completed is tried.
    /// The Store read and any payload projection happen after this returns.
    pub(crate) fn remote_runtime_session_candidates(
        &self,
    ) -> crate::remote_read::Result<crate::remote_read::RuntimeSessionCandidateSnapshot> {
        use crate::remote_read::{
            ReadError, RuntimeSessionCandidate, RuntimeSessionCandidateSnapshot,
        };

        let active = self.active.try_read().map_err(|_| ReadError::Busy)?;
        if active.len() > 10_000 {
            return Err(ReadError::ResourceLimit);
        }
        let mut active_rows: Vec<RuntimeSessionCandidate> = active
            .values()
            .map(|tracked| RuntimeSessionCandidate {
                id: tracked.session.id,
                project_id: tracked.session.project_id,
                status: tracked.session.status,
                is_eval: tracked.session.is_eval,
                updated_at_seconds: tracked.session.updated_at.timestamp(),
                updated_at_nanosecond: tracked.session.updated_at.timestamp_subsec_nanos(),
                spawn_generation: Some(tracked.spawn_generation),
            })
            .collect();
        active_rows.sort_unstable_by_key(|row| row.id);
        let active_observed_at = chrono::Utc::now();
        drop(active);

        let completed = self.completed.try_read().map_err(|_| ReadError::Busy)?;
        if completed.len() > 10_000 - active_rows.len() {
            return Err(ReadError::ResourceLimit);
        }
        let mut completed_rows: Vec<RuntimeSessionCandidate> = completed
            .values()
            .map(|entry| RuntimeSessionCandidate {
                id: entry.session.id,
                project_id: entry.session.project_id,
                status: entry.session.status,
                is_eval: entry.session.is_eval,
                updated_at_seconds: entry.session.updated_at.timestamp(),
                updated_at_nanosecond: entry.session.updated_at.timestamp_subsec_nanos(),
                spawn_generation: None,
            })
            .collect();
        completed_rows.sort_unstable_by_key(|row| row.id);
        let completed_observed_at = chrono::Utc::now();
        Ok(RuntimeSessionCandidateSnapshot {
            active: active_rows,
            active_observed_at,
            completed: completed_rows,
            completed_observed_at,
        })
    }

    /// Recopy both maps after Store work to detect observed source changes.
    /// Matching scalar witnesses do not prove a frozen runtime snapshot; the
    /// caller still reports source coverage and may retry a stale page.
    pub(crate) fn remote_runtime_session_recheck(
        &self,
        before: &crate::remote_read::RuntimeSessionCandidateSnapshot,
    ) -> crate::remote_read::Result<crate::remote_read::RuntimeSessionRecheckObservation> {
        use crate::remote_read::{RuntimeSessionRecheck, RuntimeSessionRecheckObservation};
        let after = self.remote_runtime_session_candidates()?;
        let state = if before.active == after.active && before.completed == after.completed {
            RuntimeSessionRecheck::NoObservedChange
        } else {
            RuntimeSessionRecheck::Changed
        };
        Ok(RuntimeSessionRecheckObservation {
            state,
            active_observed_at: after.active_observed_at,
            completed_observed_at: after.completed_observed_at,
        })
    }

    /// Copy at most 100 selected runtime summary rows after Store release.
    /// Exact scalar witnesses from the pre-Store snapshot fence every copy.
    pub(crate) fn remote_runtime_session_page_rows(
        &self,
        project: Uuid,
        selected: &[crate::remote_read::SessionCandidateSelection],
        before: &crate::remote_read::RuntimeSessionCandidateSnapshot,
    ) -> crate::remote_read::Result<Vec<Option<crate::remote_read::SessionRow>>> {
        use crate::remote_read::{ReadError, SessionCandidateOrigin, runtime_summary_row};
        if selected.len() > 100 {
            return Err(ReadError::ResourceLimit);
        }
        if selected.windows(2).any(|pair| pair[0].id >= pair[1].id) {
            return Err(ReadError::InvalidSource);
        }
        let mut rows = vec![None; selected.len()];
        let active = self.active.try_read().map_err(|_| ReadError::Busy)?;
        for (index, choice) in selected.iter().enumerate() {
            if choice.origin != SessionCandidateOrigin::Active {
                continue;
            }
            let tracked = active.get(&choice.id).ok_or(ReadError::SourceUnavailable)?;
            let position = before
                .active
                .binary_search_by_key(&choice.id, |row| row.id)
                .map_err(|_| ReadError::SourceUnavailable)?;
            rows[index] = Some(runtime_summary_row(
                &tracked.session,
                project,
                before.active[position],
                Some(tracked.spawn_generation),
            )?);
        }
        drop(active);
        let completed = self.completed.try_read().map_err(|_| ReadError::Busy)?;
        for (index, choice) in selected.iter().enumerate() {
            if choice.origin != SessionCandidateOrigin::Completed {
                continue;
            }
            let entry = completed
                .get(&choice.id)
                .ok_or(ReadError::SourceUnavailable)?;
            let position = before
                .completed
                .binary_search_by_key(&choice.id, |row| row.id)
                .map_err(|_| ReadError::SourceUnavailable)?;
            rows[index] = Some(runtime_summary_row(
                &entry.session,
                project,
                before.completed[position],
                None,
            )?);
        }
        drop(completed);
        Ok(rows)
    }

    /// Observe one selected question slot under one nonblocking runtime map
    /// lock. The generation is an exact slot fence, never an occurrence ID.
    /// The caller releases this observation before Store reads and then uses
    /// the recheck below; absence/replacement cannot become a tombstone.
    pub(crate) fn remote_selected_question_slot(
        &self,
        project: Uuid,
        session: Uuid,
        generation: crate::remote_read::QuestionSlotGeneration,
        mirror: crate::remote_read::QuestionSlotMirror,
    ) -> crate::remote_read::Result<crate::remote_read::RuntimeQuestionSlotSnapshot> {
        use crate::remote_read::{
            QuestionSlotGeneration, QuestionSlotMirror, ReadError, RuntimeQuestionSlotSnapshot,
            RuntimeQuestionSlotState, project_questions,
        };
        let started = std::time::Instant::now();
        let state = match generation {
            QuestionSlotGeneration::Spawn(expected) => {
                let active = self.active.try_read().map_err(|_| ReadError::Busy)?;
                let state = match active.get(&session) {
                    None => RuntimeQuestionSlotState::SourceChanged,
                    Some(tracked) => {
                        if tracked.session.project_id != Some(project) {
                            return Err(ReadError::SourceUnavailable);
                        }
                        if tracked.session.is_eval
                            || matches!(
                                tracked.session.status,
                                rsi_common::types::SessionStatus::Archived
                                    | rsi_common::types::SessionStatus::Deleted
                            )
                        {
                            return Err(ReadError::NotFound);
                        }
                        if tracked.spawn_generation != expected {
                            RuntimeQuestionSlotState::SourceChanged
                        } else {
                            let question = match mirror {
                                QuestionSlotMirror::Tracked => tracked.pending_question.as_ref(),
                                QuestionSlotMirror::Session => {
                                    tracked.session.pending_question.as_ref()
                                }
                            };
                            match question {
                                Some(question) => RuntimeQuestionSlotState::Present(
                                    project_questions(Some(question))?,
                                ),
                                None => RuntimeQuestionSlotState::Missing,
                            }
                        }
                    }
                };
                drop(active);
                state
            }
            QuestionSlotGeneration::Completed => {
                let completed = self.completed.try_read().map_err(|_| ReadError::Busy)?;
                let state = match completed.get(&session) {
                    None => RuntimeQuestionSlotState::SourceChanged,
                    Some(entry) => {
                        if entry.session.project_id != Some(project) {
                            return Err(ReadError::SourceUnavailable);
                        }
                        if entry.session.is_eval
                            || matches!(
                                entry.session.status,
                                rsi_common::types::SessionStatus::Archived
                                    | rsi_common::types::SessionStatus::Deleted
                            )
                        {
                            return Err(ReadError::NotFound);
                        }
                        match (mirror, entry.session.pending_question.as_ref()) {
                            (QuestionSlotMirror::Session, Some(question)) => {
                                RuntimeQuestionSlotState::Present(project_questions(Some(
                                    question,
                                ))?)
                            }
                            (QuestionSlotMirror::Session, None) => {
                                RuntimeQuestionSlotState::Missing
                            }
                            (QuestionSlotMirror::Tracked, _) => {
                                RuntimeQuestionSlotState::SourceChanged
                            }
                        }
                    }
                };
                drop(completed);
                state
            }
        };
        let observed_at = chrono::Utc::now();
        if started.elapsed() > std::time::Duration::from_millis(5) {
            return Err(ReadError::ResourceLimit);
        }
        Ok(RuntimeQuestionSlotSnapshot {
            project,
            session,
            generation,
            mirror,
            observed_at,
            state,
        })
    }

    /// A matching post-Store slot proves only that no bounded change was
    /// observed. The caller still reports source coverage, not a frozen view.
    pub(crate) fn remote_selected_question_slot_recheck(
        &self,
        before: &crate::remote_read::RuntimeQuestionSlotSnapshot,
    ) -> crate::remote_read::Result<crate::remote_read::RuntimeQuestionSlotRecheckObservation> {
        use crate::remote_read::{
            RuntimeQuestionSlotRecheck, RuntimeQuestionSlotRecheckObservation,
        };
        let after = self.remote_selected_question_slot(
            before.project,
            before.session,
            before.generation,
            before.mirror,
        )?;
        Ok(RuntimeQuestionSlotRecheckObservation {
            state: if matches!(
                &before.state,
                crate::remote_read::RuntimeQuestionSlotState::Present(_)
            ) && before.state == after.state
            {
                RuntimeQuestionSlotRecheck::NoObservedChange
            } else {
                RuntimeQuestionSlotRecheck::Changed
            },
            observed_at: after.observed_at,
        })
    }

    /// Capture every question mirror for one project-scoped session. Each
    /// runtime map uses its own nonblocking guard; the result has at most
    /// three bounded projections and is not a frozen view across both maps.
    pub(crate) fn remote_question_slot_list(
        &self,
        project: Uuid,
        session: Uuid,
    ) -> crate::remote_read::Result<crate::remote_read::RuntimeQuestionSlotListSnapshot> {
        use crate::remote_read::{
            QuestionSlotGeneration, QuestionSlotMirror, ReadError, RuntimeQuestionSlotListEntry,
            RuntimeQuestionSlotListSnapshot, project_questions,
        };
        let started = std::time::Instant::now();
        let mut slots = Vec::with_capacity(3);
        let active = self.active.try_read().map_err(|_| ReadError::Busy)?;
        let active_generation = match active.get(&session) {
            None => None,
            Some(tracked) => {
                if tracked.session.project_id != Some(project) {
                    return Err(ReadError::SourceUnavailable);
                }
                if tracked.session.is_eval
                    || matches!(
                        tracked.session.status,
                        rsi_common::types::SessionStatus::Archived
                            | rsi_common::types::SessionStatus::Deleted
                    )
                {
                    return Err(ReadError::NotFound);
                }
                let generation = QuestionSlotGeneration::Spawn(tracked.spawn_generation);
                if let Some(question) = tracked.pending_question.as_ref() {
                    slots.push(RuntimeQuestionSlotListEntry {
                        generation,
                        mirror: QuestionSlotMirror::Tracked,
                        projection: project_questions(Some(question))?,
                    });
                }
                if let Some(question) = tracked.session.pending_question.as_ref() {
                    slots.push(RuntimeQuestionSlotListEntry {
                        generation,
                        mirror: QuestionSlotMirror::Session,
                        projection: project_questions(Some(question))?,
                    });
                }
                Some(tracked.spawn_generation)
            }
        };
        let active_observed_at = chrono::Utc::now();
        drop(active);

        let completed = self.completed.try_read().map_err(|_| ReadError::Busy)?;
        let completed_found = match completed.get(&session) {
            None => false,
            Some(entry) => {
                if entry.session.project_id != Some(project) {
                    return Err(ReadError::SourceUnavailable);
                }
                if entry.session.is_eval
                    || matches!(
                        entry.session.status,
                        rsi_common::types::SessionStatus::Archived
                            | rsi_common::types::SessionStatus::Deleted
                    )
                {
                    return Err(ReadError::NotFound);
                }
                if let Some(question) = entry.session.pending_question.as_ref() {
                    slots.push(RuntimeQuestionSlotListEntry {
                        generation: QuestionSlotGeneration::Completed,
                        mirror: QuestionSlotMirror::Session,
                        projection: project_questions(Some(question))?,
                    });
                }
                true
            }
        };
        let completed_observed_at = chrono::Utc::now();
        drop(completed);
        if started.elapsed() > std::time::Duration::from_millis(5) {
            return Err(ReadError::ResourceLimit);
        }
        Ok(RuntimeQuestionSlotListSnapshot {
            project,
            session,
            active_observed_at,
            completed_observed_at,
            active_generation,
            completed_found,
            slots,
        })
    }

    /// Equality means no bounded source change was observed at the reread.
    /// It cannot prove either cache stayed stable between observations.
    pub(crate) fn remote_question_slot_list_recheck(
        &self,
        before: &crate::remote_read::RuntimeQuestionSlotListSnapshot,
    ) -> crate::remote_read::Result<crate::remote_read::RuntimeQuestionSlotListRecheckObservation>
    {
        use crate::remote_read::{
            RuntimeQuestionSlotListRecheckObservation, RuntimeQuestionSlotRecheck,
        };
        let after = self.remote_question_slot_list(before.project, before.session)?;
        Ok(RuntimeQuestionSlotListRecheckObservation {
            state: if before.active_generation == after.active_generation
                && before.completed_found == after.completed_found
                && before.slots == after.slots
            {
                RuntimeQuestionSlotRecheck::NoObservedChange
            } else {
                RuntimeQuestionSlotRecheck::Changed
            },
            active_observed_at: after.active_observed_at,
            completed_observed_at: after.completed_observed_at,
        })
    }

    /// Publish the operator's durably committed completed-transcript limit.
    pub(crate) async fn publish_completed_transcript_cache_cap(&self, cap: u64) {
        self.completed_transcript_cache
            .publish_cap(
                &self.runtime_config.completed_transcript_cache_max_bytes,
                cap,
            )
            .await;
    }

    /// Stamp the derived-on-read `context_fill_pct` onto each session before it
    /// is returned over RPC. The daemon is the single producer of this value:
    /// active sessions use their live runtime state (matching the
    /// `ContextUsageUpdated` bus event), idle/persisted sessions are computed
    /// from stored token fields — both through the one shared formula in
    /// `monitor.rs`. Never persisted; recomputed on every read.
    pub(crate) async fn stamp_context_fill_pct(&self, sessions: &mut [Session]) {
        let active = self.active.read().await;
        for session in sessions.iter_mut() {
            rehydrate_context_budget_projection(session);
            session.context_fill_pct = active.get(&session.id).map_or_else(
                || super::monitor::context_fill_pct_from_persisted(session),
                super::monitor::context_fill_pct_for_tracked,
            );
        }
    }

    pub async fn get_session(&self, session_id: Uuid) -> Option<Session> {
        get_session_snapshot(&self.active, &self.completed, &self.store, session_id).await
    }

    /// List all sessions (active and completed).
    ///
    /// RSI-006: eval-replay rows (`is_eval=true`) are excluded by default —
    /// they belong to the rsi-eval harness, not the user's TUI. The
    /// `rsi-eval --inspect` mode is the only documented consumer that should
    /// see them, and it goes through `list_sessions_including_eval` below.
    pub async fn list_sessions(&self) -> Vec<Session> {
        self.list_sessions_filtered(false).await
    }

    /// List all sessions including eval-replay rows. Used by the `rsi-eval`
    /// inspector and tests that need the full row set.
    pub async fn list_sessions_including_eval(&self) -> Vec<Session> {
        self.list_sessions_filtered(true).await
    }

    async fn list_sessions_filtered(&self, include_eval: bool) -> Vec<Session> {
        let mut sessions = Vec::new();
        let mut seen_ids = HashSet::new();

        for tracked in self.active.read().await.values() {
            if !is_visible_in_session_list(&tracked.session) {
                continue;
            }
            if !include_eval && tracked.session.is_eval {
                continue;
            }
            let mut s = tracked.session.clone();
            project_approval_started_at(&mut s, tracked);
            seen_ids.insert(s.id);
            sessions.push(s);
        }

        for completed in self.completed.read().await.values() {
            if !is_visible_in_session_list(&completed.session) {
                continue;
            }
            if !include_eval && completed.session.is_eval {
                continue;
            }
            seen_ids.insert(completed.session.id);
            sessions.push(completed.session.clone());
        }

        let store = self.store.clone();
        match tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.load_sessions()
        })
        .await
        {
            Ok(Ok(store_sessions)) => {
                for session in store_sessions {
                    if seen_ids.contains(&session.id) {
                        continue;
                    }
                    if !include_eval && session.is_eval {
                        continue;
                    }
                    sessions.push(session);
                }
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "list_sessions store fallback failed");
            }
            Err(e) => {
                tracing::warn!(error = %e, "list_sessions store fallback task failed");
            }
        }

        self.stamp_context_fill_pct(&mut sessions).await;
        sessions
    }

    /// Get conversation events for a session.
    pub async fn get_conversation(&self, session_id: Uuid) -> Result<Vec<ConversationEvent>> {
        self.get_conversation_since(session_id, None).await
    }

    /// Ensure an owned [`CompletedSession`] carries its real transcript,
    /// loading it from SQLite exactly once if it is still the restore-time
    /// hydration placeholder (`events_hydrated == false`). No-op otherwise.
    ///
    /// Callers that pull a `CompletedSession` out of the `completed` map by
    /// value (`continue_session`, rotation resume) MUST call this before
    /// reading `.events` -- a restored-but-never-viewed session's in-memory
    /// copy is empty until hydrated, and the events are about to be carried
    /// forward into a new `TrackedSession` or replayed to a provider.
    pub(super) async fn hydrate_completed_events(&self, cs: &mut CompletedSession) -> Result<()> {
        // Resume owns its event vector. A historical read must not retain a
        // second copy after the source leaves the completed map.
        self.completed_transcript_cache.evict(cs.session.id).await;
        if cs.events_hydrated {
            return Ok(());
        }
        cs.events = load_completed_events_from_store(&self.store, cs.session.id).await?;
        cs.events_hydrated = true;
        Ok(())
    }

    /// Get conversation events for a session, optionally only those after `since_sequence`.
    /// When `since_sequence` is Some, returns only events with sequence > that value.
    pub async fn get_conversation_since(
        &self,
        session_id: Uuid,
        since_sequence: Option<i32>,
    ) -> Result<Vec<ConversationEvent>> {
        if let Some(tracked) = self.active.read().await.get(&session_id) {
            let events = Self::filter_events_since(&tracked.events, since_sequence);
            Self::log_conversation_fetch(session_id, since_sequence, events.len(), "active");
            return Ok(events);
        }

        let completed_revision = {
            let completed = self.completed.read().await;
            completed.get(&session_id).map(|entry| {
                (
                    entry.session.updated_at,
                    entry.events_hydrated,
                    Self::filter_events_since(&entry.events, since_sequence),
                )
            })
        };
        if let Some((revision, hydrated, in_memory)) = completed_revision {
            if hydrated {
                Self::log_conversation_fetch(
                    session_id,
                    since_sequence,
                    in_memory.len(),
                    "completed",
                );
                return Ok(in_memory);
            }
            let all = self
                .completed_transcript_cache
                .get_or_load(
                    &self.store,
                    session_id,
                    &self.runtime_config.completed_transcript_cache_max_bytes,
                )
                .await?;
            // A continuation may have taken ownership while SQLite loaded.
            // Its active events are authoritative, including new events.
            let active_events = self
                .active
                .read()
                .await
                .get(&session_id)
                .map(|tracked| Self::filter_events_since(&tracked.events, since_sequence));
            if let Some(events) = active_events {
                self.completed_transcript_cache.evict(session_id).await;
                return Ok(events);
            }
            let same_completed_owner =
                self.completed
                    .read()
                    .await
                    .get(&session_id)
                    .is_some_and(|entry| {
                        entry.session.updated_at == revision && !entry.events_hydrated
                    });
            if !same_completed_owner {
                self.completed_transcript_cache.evict(session_id).await;
                if let Some(tracked) = self.active.read().await.get(&session_id) {
                    return Ok(Self::filter_events_since(&tracked.events, since_sequence));
                }
            }
            let events = Self::filter_events_since(&all, since_sequence);
            Self::log_conversation_fetch(session_id, since_sequence, events.len(), "completed");
            return Ok(events);
        }
        let store = self.store.clone();
        let events = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.load_events_since(session_id, since_sequence)
        })
        .await
        .map_err(|e| DaemonError::Store(e.to_string()))??;

        if events.is_empty() {
            Self::log_conversation_fetch(session_id, since_sequence, 0, "store");
            Err(DaemonError::SessionNotFound(session_id))
        } else {
            Self::log_conversation_fetch(session_id, since_sequence, events.len(), "store");
            Ok(events)
        }
    }

    /// Load one bounded, stable page of persisted daemon diagnostics for a
    /// session. Diagnostics are deliberately separate from conversation
    /// sequence numbers.
    pub async fn get_session_diagnostics(
        &self,
        session_id: Uuid,
        after_id: Option<i64>,
        limit: u32,
    ) -> Result<Vec<SessionDiagnosticV1>> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.list_session_diagnostics(session_id, after_id, limit)
        })
        .await
        .map_err(|error| DaemonError::Store(error.to_string()))?
    }

    /// Filter in-memory events by sequence threshold.
    fn filter_events_since(
        events: &[ConversationEvent],
        since_sequence: Option<i32>,
    ) -> Vec<ConversationEvent> {
        match since_sequence {
            None => events.to_vec(),
            Some(seq) => events
                .iter()
                .filter(|e| e.sequence > seq)
                .cloned()
                .collect(),
        }
    }

    fn log_conversation_fetch(
        session_id: Uuid,
        since_sequence: Option<i32>,
        returned: usize,
        source: &'static str,
    ) {
        if profiling::enabled() {
            tracing::trace!(
                target = "rsid::profile",
                session_id = %session_id,
                since = since_sequence.unwrap_or(-1),
                returned,
                source,
                "get_conversation_since"
            );
        }
    }

    /// Get turn metrics for a session.
    /// Checks active sessions first, then completed, then falls back to database.
    pub async fn get_turn_metrics(&self, session_id: Uuid) -> Result<Vec<TurnMetric>> {
        // Check active sessions (freshest data)
        if let Some(tracked) = self.active.read().await.get(&session_id) {
            return Ok(tracked.turn_metrics.clone());
        }
        // Check completed sessions (in-memory cache)
        if let Some(completed) = self.completed.read().await.get(&session_id) {
            return Ok(completed.turn_metrics.clone());
        }
        // Fall back to database (for sessions not in memory)
        let store = self.store.clone();
        let metrics = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.load_turn_metrics(session_id)
        })
        .await
        .map_err(|e| DaemonError::Store(e.to_string()))??;

        Ok(metrics)
    }

    /// List archived sessions, optionally filtered by project_id.
    /// Archived sessions are not kept in memory, so this queries the database directly.
    pub async fn list_archived_sessions(&self, project_id: Option<Uuid>) -> Result<Vec<Session>> {
        let store = self.store.clone();
        let sessions = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.load_archived_sessions(project_id)
        })
        .await
        .map_err(|e| DaemonError::Store(e.to_string()))??;

        let mut sessions = sessions;
        self.stamp_context_fill_pct(&mut sessions).await;
        Ok(sessions)
    }

    /// Unarchive a session: restore it to Completed status, load its events and
    /// turn_metrics into memory, and publish a SessionUnarchived event.
    pub async fn unarchive_session(&self, session_id: Uuid) -> Result<Session> {
        // Guard: reject if session is active (shouldn't happen, but be safe)
        if self.active.read().await.contains_key(&session_id) {
            return Err(DaemonError::Rpc("Session is currently active".to_string()));
        }

        let store = self.store.clone();
        let archived = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            let session = store.get_session(session_id)?;
            if session.as_ref().is_some_and(|session| {
                matches!(
                    (session.sandbox_kind, session.sandbox_cleanup_state),
                    (
                        Some(SandboxKind::GitWorktree),
                        Some(SandboxCleanupState::Purged)
                    )
                )
            }) {
                store.verify_archive_cleanup_unarchive_gate(session_id)?;
            }
            Ok::<_, DaemonError>(session)
        })
        .await
        .map_err(|error| DaemonError::Store(error.to_string()))??
        .ok_or(DaemonError::SessionNotFound(session_id))?;

        // A historically purged sandbox has no usable worktree. Recreate a
        // clean worktree before exposing the session as completed so a later
        // Continue can authenticate live custody rather than fail with
        // `historical_purged`.
        let mut session = if matches!(
            (archived.sandbox_kind, archived.sandbox_cleanup_state),
            (
                Some(SandboxKind::GitWorktree),
                Some(SandboxCleanupState::Purged)
            )
        ) {
            let sandbox_base = self.sandbox_allocator.base_dir().to_path_buf();
            let archived_for_restore = archived.clone();
            let permit = self.admit_sandbox_allocation().await?;
            let (allocation, binding) = tokio::task::spawn_blocking(move || {
                fresh_unarchive_sandbox_binding(&archived_for_restore, sandbox_base, permit)
            })
            .await
            .map_err(|error| DaemonError::Store(error.to_string()))??;
            // Store first, then the fresh root's stripe without waiting for it
            // under the Store (#1172).
            let restored =
                crate::store::sandbox_custody::restore_archived_session_with_fresh_custody_store_first(
                    &self.store,
                    session_id,
                    binding,
                )
                .await;
            match restored {
                Ok(session) => session,
                Err(error) => {
                    // D00 does not permit cleanup of an unbound worktree. Its
                    // allocated UUID root is retained for startup custody
                    // reconciliation rather than attempting an unproven
                    // destructive rollback here.
                    tracing::warn!(
                        %session_id,
                        sandbox_root = %allocation.root.display(),
                        error = %error,
                        "retaining unbound replacement sandbox after unarchive rejection"
                    );
                    return Err(error);
                }
            }
        } else {
            self.persistence
                .unarchive_session(session_id)
                .await?
                .ok_or(DaemonError::SessionNotFound(session_id))?
        };
        session.context_fill_pct = super::monitor::context_fill_pct_from_persisted(&session);

        // Load events and turn_metrics from database
        let store = self.store.clone();
        let sid = session_id;
        let (events, turn_metrics) = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            let events = store.load_events(sid)?;
            let turn_metrics = store.load_turn_metrics(sid)?;
            Ok::<_, DaemonError>((events, turn_metrics))
        })
        .await
        .map_err(|e| DaemonError::Store(e.to_string()))??;

        // Insert into completed map
        self.completed.write().await.insert(
            session_id,
            CompletedSession {
                session: session.clone(),
                events,
                turn_metrics,
                retry_cancel: None,
                retry_fired_at: None,
                superseded_by_retry: None,
                events_hydrated: true,
            },
        );

        if session.session_kind == rsi_common::types::SessionKind::Epic
            && session.lead_session_id.is_some()
        {
            self.repair_invalid_epic_leads_on_restore().await?;
        }

        // Publish event
        self.event_bus
            .publish(crate::bus::DaemonEvent::SessionUnarchived { session_id });

        Ok(session)
    }

    /// List deleted (soft-deleted) sessions, optionally filtered by project_id.
    pub async fn list_deleted_sessions(&self, project_id: Option<Uuid>) -> Result<Vec<Session>> {
        let store = self.store.clone();
        let sessions = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.load_deleted_sessions(project_id)
        })
        .await
        .map_err(|e| DaemonError::Store(e.to_string()))??;

        let mut sessions = sessions;
        self.stamp_context_fill_pct(&mut sessions).await;
        Ok(sessions)
    }

    /// Undelete a session: restore it to Completed status, load its data into memory.
    pub async fn undelete_session(&self, session_id: Uuid) -> Result<Session> {
        if self.active.read().await.contains_key(&session_id) {
            return Err(DaemonError::Rpc("Session is currently active".to_string()));
        }

        let mut session = self
            .persistence
            .undelete_session(session_id)
            .await?
            .ok_or(DaemonError::SessionNotFound(session_id))?;
        session.context_fill_pct = super::monitor::context_fill_pct_from_persisted(&session);

        // Load events and turn_metrics from database
        let store = self.store.clone();
        let sid = session_id;
        let (events, turn_metrics) = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            let events = store.load_events(sid)?;
            let turn_metrics = store.load_turn_metrics(sid)?;
            Ok::<_, DaemonError>((events, turn_metrics))
        })
        .await
        .map_err(|e| DaemonError::Store(e.to_string()))??;

        self.completed.write().await.insert(
            session_id,
            CompletedSession {
                session: session.clone(),
                events,
                turn_metrics,
                retry_cancel: None,
                retry_fired_at: None,
                superseded_by_retry: None,
                events_hydrated: true,
            },
        );

        if session.session_kind == rsi_common::types::SessionKind::Epic
            && session.lead_session_id.is_some()
        {
            self.repair_invalid_epic_leads_on_restore().await?;
        }

        self.event_bus
            .publish(crate::bus::DaemonEvent::SessionUnarchived { session_id });

        Ok(session)
    }

    /// Return daemon health metrics for observability.
    pub async fn get_health_status(&self) -> rsi_common::rpc::HealthStatusResponse {
        let project_cache_size = self.project_index.read().await.len();
        let worker_slice_memory_pressure =
            crate::process_scope::worker_slice_memory_pressure().await;

        // Fetch queue metrics and rate-limit windows with try_lock to avoid
        // blocking if store is busy. Both are advisory telemetry: a busy store
        // yields an empty reading, never a stalled health call.
        let (queue_metrics, rate_limits) = if let Ok(store) = self.store.try_lock() {
            (
                store.queue_metrics().ok(),
                store
                    .load_provider_rate_limit_snapshots()
                    .unwrap_or_default(),
            )
        } else {
            (None, Vec::new())
        };

        rsi_common::rpc::HealthStatusResponse {
            persistence_queue_depth: self.persistence.pending.load(Ordering::Relaxed),
            persistence_queue_capacity: self.persistence.capacity,
            last_command_duration_ms: self
                .persistence
                .last_command_duration_ms
                .load(Ordering::Relaxed),
            project_cache_size,
            rate_limits,
            project_cache_hits: 0,
            project_cache_misses: 0,
            last_poll_payload_bytes: 0,
            last_poll_event_count: 0,
            provider_claude_available: self.claude_client.is_some(),
            provider_codex_available: self.codex_client.is_some(),
            provider_pioneer_available: crate::pioneer::pioneer_provider_available(
                self.codex_client.is_some(),
            ),
            provider_bedrock_available: crate::bedrock::available(self.codex_client.is_some()),
            provider_openrouter_available:
                crate::openrouter::openrouter_provider_available_for_route(
                    &crate::vault::global(),
                    self.codex_client.is_some(),
                    self.runtime_config.any_openrouter_harness_route(),
                ),
            provider_local_available: self.local_client.is_some(),
            provider_antigravity_available: self.agy_client.is_some(),
            provider_harness_available: true,
            provider_clis_missing: crate::provider_cli::missing_provider_clis(),
            provider_codex_app_server_available:
                crate::codex_app_server::CodexAppServerClient::is_available(),
            queue_pending: queue_metrics.as_ref().map(|m| m.pending).unwrap_or(0),
            queue_claimed: queue_metrics.as_ref().map(|m| m.claimed).unwrap_or(0),
            queue_completed: queue_metrics.as_ref().map(|m| m.completed).unwrap_or(0),
            queue_failed: queue_metrics.as_ref().map(|m| m.failed).unwrap_or(0),
            latest_daemon_restart: self.latest_daemon_restart.clone(),
            worker_slice_memory_pressure,
            process_memory: crate::process_memory::report(),
            provider_credentials: Some(crate::vault::global().health_summary()),
            supervisor_mode: crate::daemon_info::supervisor_mode(),
        }
    }

    /// List sessions filtered by project.
    /// Combines in-memory active/completed sessions with project filter.
    pub async fn list_sessions_by_project(&self, project_id: Option<Uuid>) -> Vec<Session> {
        let mut sessions = Vec::new();
        let mut seen_ids = HashSet::new();

        // Filter active sessions
        for tracked in self.active.read().await.values() {
            if !is_visible_in_session_list(&tracked.session) {
                continue;
            }
            if tracked.session.project_id == project_id {
                let mut s = tracked.session.clone();
                project_approval_started_at(&mut s, tracked);
                seen_ids.insert(s.id);
                sessions.push(s);
            }
        }

        // Filter completed sessions
        for completed in self.completed.read().await.values() {
            if !is_visible_in_session_list(&completed.session) {
                continue;
            }
            if completed.session.project_id == project_id {
                seen_ids.insert(completed.session.id);
                sessions.push(completed.session.clone());
            }
        }

        let store = self.store.clone();
        match tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.load_sessions_by_project(project_id)
        })
        .await
        {
            Ok(Ok(store_sessions)) => {
                for session in store_sessions {
                    if seen_ids.insert(session.id) {
                        sessions.push(session);
                    }
                }
            }
            Ok(Err(e)) => {
                tracing::warn!(
                    project_id = ?project_id,
                    error = %e,
                    "list_sessions_by_project store fallback failed"
                );
            }
            Err(e) => {
                tracing::warn!(
                    project_id = ?project_id,
                    error = %e,
                    "list_sessions_by_project store fallback task failed"
                );
            }
        }

        self.stamp_context_fill_pct(&mut sessions).await;
        sessions
    }
}
