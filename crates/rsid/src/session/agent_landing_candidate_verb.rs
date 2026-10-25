//! `AgentCheckLandingCandidate` (#2310): tell a manager whether a worker's
//! `scoped_test` job verifies the commit it is about to enqueue.
//!
//! Read-only. The verb gate is the advertised projection (the appointed
//! manager, an Epic lead or a portfolio reader seat); which jobs a caller may
//! look at is the owning session's reach, exactly `AgentGetStatus`'
//! (`authorize_agent_target`: the caller itself, a direct child, a child of an
//! Epic it leads, or a session in its manager scope). An unknown job, an
//! unknown session and one outside that reach are refused the same way, so the
//! verb is no oracle for foreign jobs, and a job-id prefix counts only the jobs
//! inside the caller's reach.
//!
//! The verdict itself is pure (`rsi_common::agent_landing_candidate`); this
//! module resolves the job, applies the scope, and reads the one git fact the
//! receipt cannot carry: whether the commits between the receipt head and
//! `source_commit` touch documentation only.

use super::agent_verbs::AgentControlHandle;
use crate::error::{DaemonError, Result};
use crate::rolling_queue::{DeltaRead, changed_paths, rolling_merge_probe};
use rsi_common::agent_control_schema::AgentControlVerbV1;
use rsi_common::agent_jobs::AgentJobV1;
use rsi_common::agent_landing_candidate::{
    AgentCheckLandingCandidateRequestV1, AgentCheckLandingCandidateResultV1,
    CANDIDATE_JOB_ID_AMBIGUOUS, CANDIDATE_NOT_AUTHORIZED, CANDIDATE_NOT_FOUND_REFUSAL,
    CANDIDATE_TOOL_UNTRUSTED, RollingMergeProbe, assess_candidate,
    assess_candidate_with_alternatives, is_verification_job, job_id_hex, receipt_head,
};
use std::collections::HashMap;
use std::path::PathBuf;
use uuid::Uuid;

/// Jobs read for one id prefix before scoping (a prefix of 8 hex characters
/// collides with another job in this table only by accident).
const PREFIX_SCAN_LIMIT: usize = 64;
/// A session's newest jobs searched for its `scoped_test` runs.
const SESSION_JOB_SCAN_LIMIT: usize = 100;

/// One text for an unknown and an out-of-scope job or session: the code, then
/// the fix (kaizen #254).
fn not_found() -> DaemonError {
    DaemonError::InvalidParam(CANDIDATE_NOT_FOUND_REFUSAL.into())
}

impl AgentControlHandle {
    /// # Errors
    /// `candidate_invalid_request`, `candidate_not_authorized`,
    /// `candidate_not_found` (unknown or out of scope, identical) or
    /// `candidate_job_id_ambiguous`. A candidate that is simply not verified
    /// is a successful result with `ok: false` and a verdict `refusal`.
    pub async fn agent_check_landing_candidate(
        &self,
        caller: Uuid,
        request: AgentCheckLandingCandidateRequestV1,
    ) -> Result<AgentCheckLandingCandidateResultV1> {
        request
            .validate()
            .map_err(|code| DaemonError::InvalidParam(code.into()))?;
        {
            let store = self.store.lock().await;
            let projection = store
                .agent_authority_projection(caller)
                .map_err(|_| DaemonError::PolicyDenied(CANDIDATE_NOT_AUTHORIZED.into()))?;
            if !projection
                .verbs
                .contains(&AgentControlVerbV1::CheckLandingCandidate)
            {
                return Err(DaemonError::PolicyDenied(CANDIDATE_NOT_AUTHORIZED.into()));
            }
        }

        let commit = request.source_commit.as_str();
        let job = match request.job_id.as_deref() {
            Some(text) => self.candidate_job_by_id(caller, text, &request).await?,
            None => {
                let owner = request.source_session_id.ok_or_else(not_found)?;
                if !self.candidate_in_reach(caller, owner).await {
                    return Err(not_found());
                }
                match self.candidate_session_job(caller, owner, commit).await? {
                    Some(job) => job,
                    None => {
                        return Ok(AgentCheckLandingCandidateResultV1::no_job(
                            commit,
                            Some(owner),
                        ));
                    }
                }
            }
        };

        let mut result = assess_candidate(&job, commit);
        let checked_job = job.clone();
        let profile_stale = tokio::task::spawn_blocking(move || {
            crate::agent_jobs::profile_verification::check_profile_binding(&checked_job).is_err()
        })
        .await
        .unwrap_or(true);
        if profile_stale {
            result.refuse(rsi_common::agent_landing_candidate::CANDIDATE_PROFILE_STALE,
                "source, profile or runner differs from the daemon binding or cannot be read; rerun verification in the source's registered worktree");
        }
        if result.refusal.as_deref() == Some(CANDIDATE_TOOL_UNTRUSTED) {
            let alternatives = self
                .store
                .lock()
                .await
                .list_agent_jobs(job.owner_session_id, SESSION_JOB_SCAN_LIMIT)?;
            result = assess_candidate_with_alternatives(
                &job,
                alternatives.iter().map(|row| &row.job),
                commit,
            );
        }
        if let Some(head) = result.receipt_head.as_deref()
            && head != commit
        {
            let (from, to) = (head.to_owned(), commit.to_owned());
            match self
                .candidate_delta(caller, job.owner_session_id, from, to)
                .await
            {
                DeltaRead::Paths(paths) => result.set_delta(&paths),
                DeltaRead::TooLarge(why) => result.set_delta_too_large(why),
                DeltaRead::Unreadable => {}
            }
        }
        let probe = self
            .candidate_rolling_merge(caller, &job, commit.to_owned())
            .await;
        result.apply_rolling_merge(&probe);
        Ok(result)
    }

    /// A fresh, fetch-free trial merge of `source_commit` onto the local
    /// `origin/rolling` (#348), in the job owner's checkout and then the caller's;
    /// the first checkout that can answer wins, `Unknown` when neither can.
    async fn candidate_rolling_merge(
        &self,
        caller: Uuid,
        job: &AgentJobV1,
        commit: String,
    ) -> RollingMergeProbe {
        let mut repos = self.candidate_repos(caller, job.owner_session_id).await;
        if crate::agent_jobs::profile_verification::recorded_profile_binding(job).is_some() {
            repos.insert(0, PathBuf::from(&job.cwd));
        }
        tokio::task::spawn_blocking(move || {
            for repo in &repos {
                match rolling_merge_probe(repo, &commit) {
                    RollingMergeProbe::Unknown => {}
                    answer => return answer,
                }
            }
            RollingMergeProbe::Unknown
        })
        .await
        .unwrap_or(RollingMergeProbe::Unknown)
    }

    /// The job owner's checkout, then the caller's (one shared object store).
    async fn candidate_repos(&self, caller: Uuid, owner: Uuid) -> Vec<PathBuf> {
        let mut repos: Vec<PathBuf> = Vec::new();
        for id in [owner, caller] {
            if let Some(session) = self.get_session(id).await {
                let repo = session.sandbox_root.unwrap_or(session.working_dir);
                if !repos.contains(&repo) {
                    repos.push(repo);
                }
            }
        }
        repos
    }

    /// Whether `caller` reaches the session that owns a job.
    async fn candidate_in_reach(&self, caller: Uuid, owner: Uuid) -> bool {
        self.authorize_agent_target(caller, owner).await.is_ok()
    }

    /// The one job `text` (a full id or a prefix) names inside the caller's
    /// reach, optionally also owned by `request.source_session_id`.
    async fn candidate_job_by_id(
        &self,
        caller: Uuid,
        text: &str,
        request: &AgentCheckLandingCandidateRequestV1,
    ) -> Result<AgentJobV1> {
        let rows = self
            .store
            .lock()
            .await
            .agent_job_owners_by_id_prefix(&job_id_hex(text), PREFIX_SCAN_LIMIT)?;
        let mut reach: HashMap<Uuid, bool> = HashMap::new();
        let mut matches = Vec::new();
        for (job_id, owner) in rows {
            let reached = match reach.get(&owner) {
                Some(reached) => *reached,
                None => {
                    let reached = self.candidate_in_reach(caller, owner).await;
                    reach.insert(owner, reached);
                    reached
                }
            };
            if reached {
                {
                    let job = self
                        .store
                        .lock()
                        .await
                        .get_agent_job(job_id)?
                        .ok_or_else(not_found)?
                        .job;
                    let source = request.source_session_id.unwrap_or(owner);
                    if !self.candidate_source_matches(caller, source, &job).await {
                        continue;
                    }
                }
                matches.push(job_id);
            }
        }
        let job_id = match matches.as_slice() {
            [] => return Err(not_found()),
            [only] => *only,
            _ => return Err(DaemonError::InvalidParam(CANDIDATE_JOB_ID_AMBIGUOUS.into())),
        };
        self.store
            .lock()
            .await
            .get_agent_job(job_id)?
            .map(|row| row.job)
            .ok_or_else(not_found)
    }

    /// The session's `scoped_test` job to judge: the newest whose receipt head
    /// is `commit`, else its newest `scoped_test` job (so a rerun still in
    /// flight, or a run at an older head, is reported rather than hidden).
    async fn candidate_session_job(
        &self,
        caller: Uuid,
        owner: Uuid,
        commit: &str,
    ) -> Result<Option<AgentJobV1>> {
        let mut rows = self
            .store
            .lock()
            .await
            .list_agent_jobs(owner, SESSION_JOB_SCAN_LIMIT)?;
        // Manager-owned verification is discoverable by worktree too. Read
        // only the caller's jobs; every cross-owner match rechecks live scope.
        if caller != owner {
            let managed = self
                .store
                .lock()
                .await
                .list_agent_jobs(caller, SESSION_JOB_SCAN_LIMIT)?;
            for row in managed {
                if self.candidate_source_matches(caller, owner, &row.job).await {
                    rows.push(row);
                }
            }
        }
        rows.sort_by(|a, b| b.job.created_at.cmp(&a.job.created_at));
        let mut scoped = Vec::new();
        for row in rows {
            if is_verification_job(&row.job)
                && self.candidate_source_matches(caller, owner, &row.job).await
            {
                scoped.push(row.job);
            }
        }
        let at_head = scoped
            .iter()
            .position(|job| receipt_head(job) == Some(commit));
        Ok(match at_head {
            Some(index) => scoped.into_iter().nth(index),
            None => scoped.into_iter().next(),
        })
    }

    /// Owner mismatch is allowed only for a live scoped manager's profile job
    /// in this exact worker's registered custody root and project. The caller
    /// must reach both identities; matching a path alone grants nothing.
    async fn candidate_source_matches(&self, caller: Uuid, source: Uuid, job: &AgentJobV1) -> bool {
        if !self.candidate_in_reach(caller, source).await {
            return false;
        }
        let profile_job =
            crate::agent_jobs::profile_verification::recorded_profile_binding(job).is_some();
        if source == job.owner_session_id && !profile_job {
            return true;
        }
        if !profile_job {
            return false;
        }
        let manager = self
            .store
            .lock()
            .await
            .agent_authority_projection(job.owner_session_id)
            .is_ok_and(|projection| projection.is_manager);
        if source != job.owner_session_id
            && (!manager || !self.candidate_in_reach(job.owner_session_id, source).await)
        {
            return false;
        }
        let Some(session) = self.get_session(source).await else {
            return false;
        };
        let root = match session.sandbox_root {
            Some(root) => root,
            None if source == job.owner_session_id => session.working_dir,
            None => return false,
        };
        let project = self
            .store
            .lock()
            .await
            .agent_job_project(job.id)
            .ok()
            .flatten();
        if project.is_none() || project != session.project_id {
            return false;
        }
        let cwd = PathBuf::from(&job.cwd);
        tokio::task::spawn_blocking(move || {
            matches!((root.canonicalize(), cwd.canonicalize()), (Ok(root), Ok(cwd)) if root == cwd)
        }).await.unwrap_or(false)
    }

    /// `git diff --name-only from to` (bounded, #2344), run in the job owner's checkout and then
    /// the caller's (the commits live in one shared object store, so either
    /// worktree can answer once the other was reclaimed). `Unreadable` when neither
    /// can read both commits.
    async fn candidate_delta(
        &self,
        caller: Uuid,
        owner: Uuid,
        from: String,
        to: String,
    ) -> DeltaRead {
        let repos = self.candidate_repos(caller, owner).await;
        tokio::task::spawn_blocking(move || {
            for repo in &repos {
                match changed_paths(repo, &from, &to) {
                    DeltaRead::Unreadable => {}
                    // The same commits would be just as large in the other worktree.
                    answer => return answer,
                }
            }
            DeltaRead::Unreadable
        })
        .await
        .unwrap_or(DeltaRead::Unreadable)
    }
}

#[cfg(test)]
mod tests {
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn not_found_keeps_its_friction_code_and_names_the_fix() {
        let error = super::not_found();
        assert_eq!(
            crate::friction::agent_refusal_code(&error),
            "candidate_not_found"
        );
        assert!(error.to_string().contains("source_session_id"), "{error}");
    }
}
