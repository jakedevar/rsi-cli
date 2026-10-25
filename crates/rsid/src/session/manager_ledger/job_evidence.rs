//! Caller-owned verification job evidence for a stage update.
//!
//! A manager (or lead) that runs `scoped_test` itself owns no worker session
//! to cite. `ManagerUpdateV2::Stage { job_evidence }` names one of the caller's
//! own terminal jobs instead. Everything that makes it proof is read here from
//! the daemon's job table and the caller's own committed custody, never from
//! the request: the job's owner must be the authenticated caller, the job must
//! be recorded under the ledger's target project (a legacy job with no project
//! is refused, #2710), it must be a
//! terminal `scoped_test` whose recorded receipt names the tested commit, and
//! the committed report must be a regular file at an artifact commit the
//! caller's own branch contains. The result is reported evidence only: it is
//! never an admission and never accepts the Work.

use super::{AgentControlHandle, git};
use crate::store::harness_manager_v2::refused;
use crate::store::manager_ledger::{
    JobEvidenceObservation, JobEvidenceOutcome, JobKnownFailure, LedgerObservation, canonical_sha,
};
use crate::topology::verify::{self, Reason};
use crate::{error::Result, store::Store};
use rsi_common::agent_jobs::{AgentJobV1, JobParams};
use rsi_common::harness_manager_v2::ManagerJobEvidenceV2;
use sha2::{Digest, Sha256};
use std::path::Path;
use uuid::Uuid;

/// The job the caller cited, or the refusal naming why it is not usable.
/// Owner and project are compared first so a foreign job, or one recorded for
/// another project (or for none: fail closed for a legacy row), is
/// indistinguishable from a missing one.
fn cited_job(store: &Store, caller: Uuid, project: Uuid, job_id: Uuid) -> Result<AgentJobV1> {
    let unavailable = || refused("manager_v2_job_evidence_job_unavailable");
    let job = store
        .get_agent_job(job_id)?
        .map(|row| row.job)
        .filter(|job| job.owner_session_id == caller)
        .ok_or_else(unavailable)?;
    if store.agent_job_project(job_id)? != Some(project) {
        return Err(unavailable());
    }
    if !matches!(&job.params, JobParams::Test(test) if test.is_scoped_test()) {
        return Err(refused("manager_v2_job_evidence_kind_unsupported"));
    }
    if !job.state.is_terminal() {
        return Err(refused("manager_v2_job_evidence_job_running"));
    }
    Ok(job)
}

/// Judge the cited job against the supplied tested commit with the same
/// attribution a verified topology node uses, reading the project's open
/// known-failure records for the baseline distinction.
fn judged(
    store: &Store,
    project: Uuid,
    job: &AgentJobV1,
    source_commit: &str,
) -> Result<JobEvidenceObservation> {
    let signatures = store.open_failure_signature_issues(project)?;
    let known = verify::known_failures(&signatures);
    let (outcome, evidence) = match verify::judge(
        std::slice::from_ref(job),
        job.owner_session_id,
        source_commit,
        &known,
    ) {
        Ok(evidence) if evidence.exit_code == Some(0) => (JobEvidenceOutcome::Green, evidence),
        Ok(evidence) => (JobEvidenceOutcome::BaselineRed, evidence),
        Err(unverified) => {
            let outcome = match unverified.reason {
                Reason::NewFailures => JobEvidenceOutcome::Red,
                // A candidate/sandbox/unstamped tool copy is tool-under-test evidence,
                // never green or baseline-red (#3793).
                Reason::Incomplete | Reason::UntrustedTool => JobEvidenceOutcome::Incomplete,
                // The receipt names another commit: the job did not test this one.
                Reason::StaleHead => return Err(refused("manager_v2_job_evidence_head_mismatch")),
                Reason::NoReceipt => {
                    return Err(refused("manager_v2_job_evidence_kind_unsupported"));
                }
            };
            let evidence = unverified
                .evidence
                .filter(|evidence| evidence.head == source_commit)
                .ok_or_else(|| refused("manager_v2_job_evidence_receipt_missing"))?;
            (outcome, evidence)
        }
    };
    Ok(JobEvidenceObservation {
        job_id: job.id,
        owner_session_id: job.owner_session_id,
        source_commit: evidence.head,
        artifact_path: String::new(),
        artifact_commit: String::new(),
        artifact_digest: String::new(),
        outcome,
        exit_code: evidence.exit_code,
        plan: evidence.plan,
        failing_tests: evidence.failing_tests,
        known_failures: evidence
            .known_failures
            .into_iter()
            .map(|(test, issue)| JobKnownFailure { test, issue })
            .collect(),
        new_failures: evidence.new_failures,
        observed_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
    })
}

impl AgentControlHandle {
    /// Observe a stage update's `job_evidence`. The returned observation
    /// carries the caller's custody witness so the commit fences a moved root.
    pub(super) async fn observe_job_evidence(
        &self,
        caller: Uuid,
        project: Uuid,
        cited: &ManagerJobEvidenceV2,
    ) -> Result<LedgerObservation> {
        if !canonical_sha(&cited.source_commit) || !canonical_sha(&cited.artifact_commit) {
            return Err(refused("manager_v2_invalid_commit"));
        }
        git::artifact_path(&cited.artifact_path)?;
        let (mut observation, custody) = {
            let store = self.store.lock().await;
            let job = cited_job(&store, caller, project, cited.job_id)?;
            let observation = judged(&store, project, &job, &cited.source_commit)?;
            let custody = store
                .live_custody_for_session(caller)
                .map_err(|_| refused("manager_v2_job_evidence_custody_required"))?;
            (observation, custody)
        };
        let root = Path::new(&custody.sandbox_root);
        git::custody(&custody).await?;
        // The tested commit must be a real local object, and the report's commit
        // must sit in the caller's own committed history (never a dirty tree).
        git::head(root, &cited.source_commit).await?;
        let head = git::head(root, "HEAD").await?;
        if !git::ancestor(root, &cited.artifact_commit, &head).await? {
            return Err(refused("manager_v2_job_evidence_artifact_unreachable"));
        }
        let bytes = git::blob(root, &cited.artifact_commit, &cited.artifact_path).await?;
        git::custody(&custody).await?;
        observation.artifact_path.clone_from(&cited.artifact_path);
        observation
            .artifact_commit
            .clone_from(&cited.artifact_commit);
        observation.artifact_digest = format!("sha256:{:x}", Sha256::digest(&bytes));
        Ok(LedgerObservation {
            job_evidence: Some(observation),
            custody: vec![(caller, custody.custody_id, custody.generation)],
            ..Default::default()
        })
    }
}
