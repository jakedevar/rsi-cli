//! Operator-only K1 Closure control surface.

use crate::claude::LaunchConfig;
use crate::error::{DaemonError, Result};
use crate::session::SessionManager;
use crate::store::closure_kernel::{
    ClosureSourceLaunchClaimV1, PreparedClosureEvidenceV1, PreparedClosureSourceV1,
    ResolvedClosureProgramV1,
};
use rsi_common::agent_contract::parse_closure_review_handoff_v1;
use rsi_common::closure_kernel::*;
use rsi_common::model_control::ModelInvocationPurpose;
use rsi_common::program_runs::{canonical_program_run_json, program_run_fingerprint};
use rsi_common::types::{SandboxKind, SandboxSpec, SessionKind};
use rsi_common::verification_manifest::{ItemStatus, ManifestStatus};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::Mutex;
use uuid::Uuid;

pub(crate) async fn create_program(
    manager: &SessionManager,
    request: CreateClosureProgramRequestV1,
) -> Result<CreateClosureProgramResultV1> {
    if let Some(replay) = manager
        .store()
        .lock()
        .await
        .replay_create_closure_program(&request)?
    {
        return Ok(replay);
    }
    let resolved = resolve_program_config(&request.config).await?;
    manager
        .store()
        .lock()
        .await
        .create_closure_program(&request, &resolved)
}

pub(crate) async fn update_program(
    manager: &SessionManager,
    request: UpdateClosureProgramRequestV1,
) -> Result<ClosureProgramMutationResultV1> {
    if let Some(replay) = manager
        .store()
        .lock()
        .await
        .replay_update_closure_program(&request)?
    {
        return Ok(replay);
    }
    let resolved = resolve_program_config(&request.config).await?;
    manager
        .store()
        .lock()
        .await
        .update_closure_program(&request, &resolved)
}

pub(crate) async fn list_programs(
    manager: &SessionManager,
    request: ListClosureProgramsRequestV1,
) -> Result<ClosureProgramListV1> {
    manager.store().lock().await.list_closure_programs(&request)
}

pub(crate) async fn get_program(
    manager: &SessionManager,
    request: GetClosureProgramRequestV1,
) -> Result<ClosureProgramDetailV1> {
    manager
        .store()
        .lock()
        .await
        .get_closure_program_summary(&request)
}

pub(crate) async fn launch_source(
    manager: &SessionManager,
    request: LaunchClosureSourceRequestV1,
) -> Result<LaunchClosureSourceResultV1> {
    let resumed = manager
        .store()
        .lock()
        .await
        .resume_closure_source_launch(&request)?;
    let (reservation, config) = match resumed {
        Some(ClosureSourceLaunchClaimV1::Result(result)) => return Ok(result),
        Some(ClosureSourceLaunchClaimV1::Reserved(reservation)) => {
            let (_, config) = manager
                .store()
                .lock()
                .await
                .closure_program_launch_config(request.program_id)?;
            (reservation, config)
        }
        None => {
            if request.title.trim().is_empty() || request.query.trim().is_empty() {
                return Err(DaemonError::InvalidParam(
                    "Closure title and query must be non-empty".into(),
                ));
            }
            let (program, config) = manager
                .store()
                .lock()
                .await
                .closure_program_launch_config(request.program_id)?;
            if program.state != ClosureProgramStateV1::Configured || program.source_id.is_some() {
                return Err(DaemonError::InvalidParam(
                    "Closure program is not launchable".into(),
                ));
            }
            let observed_base =
                resolve_ref(&config.repository_root, config.base_ref.as_str()).await?;
            let observed_destination =
                resolve_ref(&config.repository_root, config.destination_ref.as_str()).await?;
            if observed_base != program.base_sha
                || observed_destination != program.destination_pre_head
            {
                return Err(DaemonError::InvalidParam(
                    "Closure configured base or destination moved before launch".into(),
                ));
            }
            let reservation = match manager
                .store()
                .lock()
                .await
                .reserve_closure_source_launch(&request)?
            {
                ClosureSourceLaunchClaimV1::Result(result) => return Ok(result),
                ClosureSourceLaunchClaimV1::Reserved(reservation) => reservation,
            };
            (reservation, config)
        }
    };
    create_ref_once(
        &config.repository_root,
        reservation.source.staging_ref.as_str(),
        reservation.source.destination_pre_head.as_str(),
    )
    .await?;

    let selector = super::ClosureLaunchSelectorV1 {
        program_id: request.program_id,
        source_id: reservation.source.source_id,
        base_sha: reservation.source.source_base_sha.clone(),
        destination_ref: reservation.source.destination_ref.clone(),
        destination_pre_head: reservation.source.destination_pre_head.clone(),
        staging_ref: reservation.source.staging_ref.clone(),
        lineage_root_session_id: None,
        custody_id: None,
        custody_generation: None,
        model_invocation_id: None,
    };
    let launch = LaunchConfig {
        query: request.query.clone(),
        title: Some(request.title.clone()),
        agent_role: None,
        epic_spawn_ordinal: None,
        working_dir: Some(config.repository_root.clone()),
        provider: Some(request.provider),
        model: request.model.clone(),
        configured_context_window: None,
        max_turns: None,
        system_prompt: None,
        resume_session_id: None,
        session_kind: Some(SessionKind::Feature),
        project_id: None,
        rsi_session_id: None,
        rsi_socket: None,
        rsi_session_token: None,
        continued_from: None,
        openai_base_url: None,
        openai_api_key: None,
        conversation_history: None,
        workflow_id: None,
        workflow_id_override: None,
        max_retries: Some(0),
        group_id: None,
        parent_id: None,
        effort: request.effort.clone(),
        issue_identifier: None,
        issue_url: None,
        issue_tracker_id: None,
        scheduled_job_id: None,
        skip_project_model_default: false,
        model_invocation_purpose: ModelInvocationPurpose::SessionLaunchFresh,
        model_invocation_owner: None,
        model_invocation_dedup_key: Some(format!(
            "closure.source:{}",
            reservation.source.source_id
        )),
        model_invocation_request_fingerprint: Some(
            closure_request_fingerprint("LaunchClosureSource", &request)
                .map_err(DaemonError::InvalidParam)?,
        ),
        sandbox: Some(SandboxSpec {
            kind: Some(SandboxKind::GitWorktree),
            branch: Some(reservation.branch_name.clone()),
        }),
        cargo_target_dir: None,
        execution_scratch: None,
        is_eval: false,
        skip_context_pipeline: false,
        capability_class: None,
        tags: vec!["closure".into(), "closure-source".into()],
        topology_node_id: None,
        topology_iteration: 0,
        closure_selector: Some(selector),
    };
    let root_session_id = manager
        .launch_reserved_closure_source(
            launch,
            reservation.root_session_id,
            reservation.source.custody_id,
        )
        .await?;
    let custody = manager
        .store()
        .lock()
        .await
        .live_custody_for_session(root_session_id)?;
    if custody.custody_id != reservation.source.custody_id
        || custody.generation != reservation.source.custody_generation
        || custody.source_commit != reservation.source.source_base_sha.as_str()
        || custody.sandbox_branch != reservation.branch_name
    {
        return Err(DaemonError::Store(
            "Closure launch custody identity does not match the explicit selector".into(),
        ));
    }
    let prepared = PreparedClosureSourceV1 {
        source: reservation.source,
        root_session_id,
        idempotency_key: reservation.idempotency_key,
        request_fingerprint: reservation.request_fingerprint,
    };
    manager
        .store()
        .lock()
        .await
        .record_closure_source_launch(&prepared)
}

pub(crate) async fn record_evidence(
    manager: &SessionManager,
    request: RecordClosureEvidenceRequestV1,
) -> Result<ClosureEvidenceResultV1> {
    record_evidence_with_store(manager.store(), request).await
}

async fn record_evidence_with_store(
    store: &Arc<Mutex<crate::store::Store>>,
    request: RecordClosureEvidenceRequestV1,
) -> Result<ClosureEvidenceResultV1> {
    #[cfg(test)]
    {
        return record_evidence_with_store_inner(store, request, None).await;
    }
    #[cfg(not(test))]
    {
        record_evidence_with_store_inner(store, request).await
    }
}

#[cfg(test)]
#[derive(Clone)]
struct ClosureEvidencePrecommitBarrier {
    preflight_complete: Arc<tokio::sync::Barrier>,
    commit_allowed: Arc<tokio::sync::Barrier>,
}

async fn record_evidence_with_store_inner(
    store: &Arc<Mutex<crate::store::Store>>,
    request: RecordClosureEvidenceRequestV1,
    #[cfg(test)] precommit_barrier: Option<ClosureEvidencePrecommitBarrier>,
) -> Result<ClosureEvidenceResultV1> {
    macro_rules! refuse {
        ($code:expr) => {
            return store
                .lock()
                .await
                .record_closure_evidence_refusal(&request, $code)
        };
    }
    if let Some(replay) = store.lock().await.replay_closure_evidence(&request)? {
        return Ok(replay);
    }
    let required = match &request.disposition {
        ClosureEvidenceDispositionV1::RequiredIndependent {
            reviewer_session_id,
            reviewer_model_invocation_id,
            expected_evidence_commit,
            review_json_path,
            manifest_v2_path,
        } => Some((
            *reviewer_session_id,
            *reviewer_model_invocation_id,
            expected_evidence_commit.clone(),
            review_json_path.clone(),
            manifest_v2_path.clone(),
        )),
        ClosureEvidenceDispositionV1::NotRequired { .. } => None,
    };

    let source = store
        .lock()
        .await
        .closure_source_identity(request.source_id)?;
    let (_, program_config) = store
        .lock()
        .await
        .closure_program_launch_config(source.program_id)?;
    let source_ref_before = match resolve_ref(
        Path::new(&program_config.repository_root),
        source.source_ref.as_str(),
    )
    .await
    {
        Ok(value) => value,
        Err(_) => refuse!(ClosureRefusalCodeV1::StaleReviewedSha),
    };
    let source_worktree = store
        .lock()
        .await
        .closure_source_worktree_root(request.source_id)?;
    let source_worktree_head_before = match resolve_ref(Path::new(&source_worktree), "HEAD").await {
        Ok(value) => value,
        Err(_) => refuse!(ClosureRefusalCodeV1::StaleReviewedSha),
    };
    let source_worktree_clean_before = match git_output(
        Path::new(&source_worktree),
        &["status", "--porcelain=v1", "-z"],
    )
    .await
    {
        Ok(value) => value.is_empty(),
        Err(_) => false,
    };
    if source_ref_before != request.expected_sealed_source_head
        || source_worktree_head_before != request.expected_sealed_source_head
        || !source_worktree_clean_before
    {
        refuse!(ClosureRefusalCodeV1::StaleReviewedSha);
    }

    let prepared = if let Some((
        reviewer_session_id,
        reviewer_invocation_id,
        evidence_commit,
        review_path,
        manifest_path,
    )) = required
    {
        // Drop the store mutex before routing a typed refusal back through the
        // atomic idempotency writer. A guard held as the `match` scrutinee
        // otherwise lives through the refusal arm and would self-deadlock.
        let context_result = {
            let store = store.lock().await;
            store.closure_evidence_admission_context(
                request.source_id,
                reviewer_session_id,
                reviewer_invocation_id,
            )
        };
        let context = match context_result {
            Ok(context) => context,
            Err(_) => {
                refuse!(ClosureRefusalCodeV1::InvalidEvidenceCustody);
            }
        };
        if context.sealed_source_head != request.expected_sealed_source_head
            || context.source_state != ClosureSourceStateV1::AwaitingEvidence
            || !matches!(
                context.review_policy,
                ClosureReviewPolicyV1::RequiredIndependent
            )
        {
            refuse!(ClosureRefusalCodeV1::ReviewPolicyMismatch);
        }
        let expected_review_path = format!(
            "thoughts/shared/reviews/closure/{}/{}-review-v1.json",
            context.program_id, request.source_id
        );
        let expected_manifest_path = format!(
            "thoughts/shared/verification/closure/{}/{}-manifest-v2.md",
            context.program_id, request.source_id
        );
        if review_path.as_str() != expected_review_path
            || manifest_path.as_str() != expected_manifest_path
        {
            refuse!(ClosureRefusalCodeV1::InvalidEvidenceCustody);
        }
        let handoff = match parse_closure_review_handoff_v1(&context.review_handoff_raw) {
            Ok(handoff) => handoff,
            Err(_) => {
                refuse!(ClosureRefusalCodeV1::InvalidEvidenceCustody);
            }
        };
        if handoff.reviewer_session_id != reviewer_session_id
            || handoff.reviewer_model_invocation_id != reviewer_invocation_id
            || handoff.review_json_path != review_path
            || handoff.manifest_v2_path != manifest_path
            || handoff.sealed_source_sha != request.expected_sealed_source_head
            || handoff.evidence_commit_sha != evidence_commit
        {
            refuse!(ClosureRefusalCodeV1::InvalidEvidenceCustody);
        }
        let reviewer_status = match git_output(
            Path::new(&context.reviewer_worktree_root),
            &["status", "--porcelain=v1", "-z"],
        )
        .await
        {
            Ok(value) => value,
            Err(_) => {
                refuse!(ClosureRefusalCodeV1::InvalidEvidenceCustody);
            }
        };
        let reviewer_head =
            match resolve_ref(Path::new(&context.reviewer_worktree_root), "HEAD").await {
                Ok(value) => value,
                Err(_) => {
                    refuse!(ClosureRefusalCodeV1::InvalidEvidenceCustody);
                }
            };
        let reviewer_ref = match resolve_ref(
            Path::new(&context.repository_root),
            &format!("refs/heads/{}", context.reviewer_branch),
        )
        .await
        {
            Ok(value) => value,
            Err(_) => {
                refuse!(ClosureRefusalCodeV1::InvalidEvidenceCustody);
            }
        };
        if !reviewer_status.is_empty()
            || reviewer_head != evidence_commit
            || reviewer_ref != evidence_commit
        {
            refuse!(ClosureRefusalCodeV1::InvalidEvidenceCustody);
        }
        let parents = match git_text(
            Path::new(&context.repository_root),
            &["rev-list", "--parents", "-n", "1", evidence_commit.as_str()],
        )
        .await
        {
            Ok(value) => value,
            Err(_) => {
                refuse!(ClosureRefusalCodeV1::InvalidEvidenceCustody);
            }
        };
        let parent_parts = parents.split_whitespace().collect::<Vec<_>>();
        if parent_parts.len() != 2
            || parent_parts[1] != request.expected_sealed_source_head.as_str()
        {
            refuse!(ClosureRefusalCodeV1::InvalidEvidenceCustody);
        }
        let changed = match git_text(
            Path::new(&context.repository_root),
            &[
                "diff-tree",
                "--no-commit-id",
                "--name-only",
                "-r",
                evidence_commit.as_str(),
            ],
        )
        .await
        {
            Ok(value) => value,
            Err(_) => {
                refuse!(ClosureRefusalCodeV1::InvalidEvidenceCustody);
            }
        };
        let actual_paths = changed.lines().collect::<BTreeSet<_>>();
        let expected_paths = [review_path.as_str(), manifest_path.as_str()]
            .into_iter()
            .collect::<BTreeSet<_>>();
        if actual_paths != expected_paths {
            refuse!(ClosureRefusalCodeV1::InvalidEvidenceCustody);
        }
        let review_bytes = match committed_blob(
            Path::new(&context.repository_root),
            &evidence_commit,
            review_path.as_str(),
        )
        .await
        {
            Ok(value) => value,
            Err(_) => {
                refuse!(ClosureRefusalCodeV1::InvalidEvidenceCustody);
            }
        };
        let manifest_bytes = match committed_blob(
            Path::new(&context.repository_root),
            &evidence_commit,
            manifest_path.as_str(),
        )
        .await
        {
            Ok(value) => value,
            Err(_) => {
                refuse!(ClosureRefusalCodeV1::InvalidEvidenceCustody);
            }
        };
        let review_worktree_bytes = match bounded_regular_worktree_file(
            Path::new(&context.reviewer_worktree_root),
            review_path.as_str(),
        )
        .await
        {
            Ok(value) => value,
            Err(_) => {
                refuse!(ClosureRefusalCodeV1::InvalidEvidenceCustody);
            }
        };
        let manifest_worktree_bytes = match bounded_regular_worktree_file(
            Path::new(&context.reviewer_worktree_root),
            manifest_path.as_str(),
        )
        .await
        {
            Ok(value) => value,
            Err(_) => {
                refuse!(ClosureRefusalCodeV1::InvalidEvidenceCustody);
            }
        };
        if review_worktree_bytes != review_bytes || manifest_worktree_bytes != manifest_bytes {
            refuse!(ClosureRefusalCodeV1::InvalidEvidenceCustody);
        }
        let review_text = match std::str::from_utf8(&review_bytes) {
            Ok(value) => value,
            Err(_) => {
                refuse!(ClosureRefusalCodeV1::InvalidEvidenceCustody);
            }
        };
        let manifest_text = match std::str::from_utf8(&manifest_bytes) {
            Ok(value) => value,
            Err(_) => {
                refuse!(ClosureRefusalCodeV1::InvalidEvidenceCustody);
            }
        };
        let expectation = ClosureEvidenceExpectationV1 {
            source_head: request.expected_sealed_source_head.clone(),
            reviewer_session_id,
            model_invocation_id: reviewer_invocation_id,
            review_policy_digest: context.review_policy_digest.clone(),
        };
        let bundle =
            match validate_closure_evidence_bundle_v1(review_text, manifest_text, &expectation) {
                Ok(bundle) => bundle,
                Err(error) => refuse!(evidence_validation_refusal(&error)),
            };
        if bundle.review.verdict != ClosureReviewVerdictV1::Accepted
            || !verification_policy_passes(&context.verification_policy, &bundle.manifest)
        {
            refuse!(ClosureRefusalCodeV1::EvidenceNotEligible);
        }
        let source_ref_after = match resolve_ref(
            Path::new(&context.repository_root),
            source.source_ref.as_str(),
        )
        .await
        {
            Ok(value) => value,
            Err(_) => refuse!(ClosureRefusalCodeV1::StaleReviewedSha),
        };
        let source_worktree_head_after =
            match resolve_ref(Path::new(&source_worktree), "HEAD").await {
                Ok(value) => value,
                Err(_) => refuse!(ClosureRefusalCodeV1::StaleReviewedSha),
            };
        let source_worktree_clean_after = match git_output(
            Path::new(&source_worktree),
            &["status", "--porcelain=v1", "-z"],
        )
        .await
        {
            Ok(value) => value.is_empty(),
            Err(_) => false,
        };
        if source_ref_after != source_ref_before
            || source_worktree_head_after != source_worktree_head_before
            || !source_worktree_clean_after
        {
            refuse!(ClosureRefusalCodeV1::StaleReviewedSha);
        }
        let normalized_review_json =
            canonical_program_run_json(&bundle.review).map_err(DaemonError::InvalidParam)?;
        PreparedClosureEvidenceV1 {
            request: request.clone(),
            evidence: AcceptedReviewEvidenceV1 {
                evidence_id: ClosureEvidenceIdV1::new(Uuid::new_v4()),
                source_id: request.source_id,
                reviewed_source_head: request.expected_sealed_source_head.clone(),
                review_schema_version: Some(bundle.review.schema_version),
                review_digest: Some(bundle.review_digest),
                finding_set_digest: Some(bundle.finding_set_digest),
                manifest_schema_version: Some(bundle.manifest.frontmatter.schema_version),
                manifest_digest: Some(bundle.manifest_digest),
                manifest_source_head: bundle.manifest.frontmatter.source_head,
                reviewer_session_id: Some(reviewer_session_id),
                reviewer_model_invocation_id: Some(reviewer_invocation_id),
                reviewer_provider: Some(context.reviewer_provider),
                reviewer_model: context.reviewer_model,
                reviewer_custody_id: Some(context.reviewer_custody_id),
                reviewer_custody_generation: Some(context.reviewer_custody_generation),
                evidence_commit: Some(evidence_commit),
                review_handoff_event_id: Some(context.review_handoff_event_id),
                review_handoff_digest: Some(program_run_fingerprint(
                    "closure-review-handoff:v1",
                    context.review_handoff_raw.as_bytes(),
                )),
                source_ref_before,
                source_ref_after,
                source_worktree_head_before,
                source_worktree_head_after,
                review_policy_digest: context.review_policy_digest,
                verification_policy_digest: context.verification_policy_digest,
                verifier_policy_digest: context.verifier_policy_digest,
                accepted_at: chrono::Utc::now(),
            },
            review_raw_bytes: Some(review_bytes),
            normalized_review_json: Some(normalized_review_json),
            manifest_raw_bytes: Some(manifest_bytes),
        }
    } else {
        let configured = store
            .lock()
            .await
            .closure_program_launch_config(source.program_id)?
            .0;
        let ClosureEvidenceDispositionV1::NotRequired { basis, rationale } = &request.disposition
        else {
            unreachable!();
        };
        let permitted = matches!(
            (&program_config.review_policy, basis),
            (
                ClosureReviewPolicyV1::NotRequired {
                    basis: ClosureReviewNotRequiredBasisV1::Tier0Deterministic,
                    ..
                },
                ClosureReviewNotRequiredBasisV1::Tier0Deterministic
            )
        ) && !rationale.trim().is_empty();
        if !permitted || configured.state != ClosureProgramStateV1::AwaitingEvidence {
            refuse!(ClosureRefusalCodeV1::ReviewPolicyMismatch);
        }
        let context = store
            .lock()
            .await
            .closure_program_policy_digests(source.program_id)?;
        PreparedClosureEvidenceV1 {
            request: request.clone(),
            evidence: AcceptedReviewEvidenceV1 {
                evidence_id: ClosureEvidenceIdV1::new(Uuid::new_v4()),
                source_id: request.source_id,
                reviewed_source_head: request.expected_sealed_source_head.clone(),
                review_schema_version: None,
                review_digest: None,
                finding_set_digest: None,
                manifest_schema_version: None,
                manifest_digest: None,
                manifest_source_head: None,
                reviewer_session_id: None,
                reviewer_model_invocation_id: None,
                reviewer_provider: None,
                reviewer_model: None,
                reviewer_custody_id: None,
                reviewer_custody_generation: None,
                evidence_commit: None,
                review_handoff_event_id: None,
                review_handoff_digest: None,
                source_ref_before: source_ref_before.clone(),
                source_ref_after: source_ref_before,
                source_worktree_head_before: source_worktree_head_before.clone(),
                source_worktree_head_after: source_worktree_head_before,
                review_policy_digest: context.0,
                verification_policy_digest: context.1,
                verifier_policy_digest: context.2,
                accepted_at: chrono::Utc::now(),
            },
            review_raw_bytes: None,
            normalized_review_json: None,
            manifest_raw_bytes: None,
        }
    };
    #[cfg(test)]
    if let Some(barrier) = precommit_barrier {
        barrier.preflight_complete.wait().await;
        barrier.commit_allowed.wait().await;
    }
    store.lock().await.record_closure_evidence(&prepared)
}

fn evidence_validation_refusal(error: &ClosureEvidenceValidationErrorV1) -> ClosureRefusalCodeV1 {
    match error {
        ClosureEvidenceValidationErrorV1::UnsupportedReviewSchema { .. } => {
            ClosureRefusalCodeV1::UnsupportedReviewSchema
        }
        ClosureEvidenceValidationErrorV1::StaleReviewedSourceHead => {
            ClosureRefusalCodeV1::StaleReviewedSha
        }
        ClosureEvidenceValidationErrorV1::ReviewerCorrelationMismatch => {
            ClosureRefusalCodeV1::InvalidEvidenceCustody
        }
        ClosureEvidenceValidationErrorV1::ReviewPolicyDigestMismatch => {
            ClosureRefusalCodeV1::ReviewPolicyMismatch
        }
        ClosureEvidenceValidationErrorV1::Manifest { message }
            if message.contains("ManifestV1Unbound") =>
        {
            ClosureRefusalCodeV1::ManifestV1Unbound
        }
        ClosureEvidenceValidationErrorV1::Manifest { message }
            if message.contains("ExactHeadMatch") =>
        {
            ClosureRefusalCodeV1::ManifestSourceHeadMismatch
        }
        _ => ClosureRefusalCodeV1::EvidenceNotEligible,
    }
}

fn verification_policy_passes(
    policy: &ClosureVerificationPolicyV1,
    manifest: &rsi_common::verification_manifest::VerificationManifest,
) -> bool {
    if manifest.frontmatter.status != ManifestStatus::Verified {
        return false;
    }
    let present = manifest
        .phases
        .iter()
        .flat_map(|phase| phase.buckets_present.iter())
        .map(|bucket| bucket.heading().to_ascii_lowercase())
        .collect::<BTreeSet<_>>();
    if policy
        .required_buckets
        .iter()
        .any(|bucket| !present.contains(&bucket.to_ascii_lowercase()))
    {
        return false;
    }
    if policy.required_buckets.iter().any(|required| {
        !manifest
            .phases
            .iter()
            .flat_map(|phase| &phase.items)
            .any(|item| item.bucket.heading().eq_ignore_ascii_case(required))
    }) {
        return false;
    }
    if policy.require_all_items_pass
        && manifest
            .phases
            .iter()
            .flat_map(|phase| &phase.items)
            .any(|item| !matches!(item.status, Some(ItemStatus::Pass | ItemStatus::Checked)))
    {
        return false;
    }
    true
}

async fn committed_blob(
    repository: &Path,
    commit: &ClosureGitShaV1,
    path: &str,
) -> Result<Vec<u8>> {
    let entry = format!("{}:{path}", commit.as_str());
    let object_type = git_text(repository, &["cat-file", "-t", &entry]).await?;
    if object_type.trim() != "blob" {
        return Err(DaemonError::InvalidParam(
            "Closure evidence path is not a committed regular blob".into(),
        ));
    }
    let listing = git_text(repository, &["ls-tree", commit.as_str(), "--", path]).await?;
    if listing.starts_with("120000 ") {
        return Err(DaemonError::InvalidParam(
            "Closure evidence path cannot be a symlink".into(),
        ));
    }
    let bytes = git_output(repository, &["show", &entry]).await?;
    if bytes.len() > CLOSURE_MAX_ARTIFACT_BYTES_V1 {
        return Err(DaemonError::InvalidParam(
            "Closure evidence artifact exceeds size bound".into(),
        ));
    }
    Ok(bytes)
}

async fn bounded_regular_worktree_file(root: &Path, relative_path: &str) -> Result<Vec<u8>> {
    let canonical_root = tokio::fs::canonicalize(root).await.map_err(|error| {
        DaemonError::InvalidParam(format!("Closure evidence root is unavailable: {error}"))
    })?;
    let candidate = canonical_root.join(relative_path);
    let metadata = tokio::fs::symlink_metadata(&candidate)
        .await
        .map_err(|error| {
            DaemonError::InvalidParam(format!("Closure evidence file is unavailable: {error}"))
        })?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(DaemonError::InvalidParam(
            "Closure evidence path is not a regular worktree file".into(),
        ));
    }
    let canonical_file = tokio::fs::canonicalize(&candidate).await.map_err(|error| {
        DaemonError::InvalidParam(format!("Closure evidence file cannot be resolved: {error}"))
    })?;
    if !canonical_file.starts_with(&canonical_root) {
        return Err(DaemonError::InvalidParam(
            "Closure evidence path escapes reviewer custody".into(),
        ));
    }
    let bytes = tokio::fs::read(canonical_file).await?;
    if bytes.len() > CLOSURE_MAX_ARTIFACT_BYTES_V1 {
        return Err(DaemonError::InvalidParam(
            "Closure evidence artifact exceeds size bound".into(),
        ));
    }
    Ok(bytes)
}

async fn resolve_program_config(
    config: &ClosureProgramConfigV1,
) -> Result<ResolvedClosureProgramV1> {
    let root = config.repository_root.canonicalize().map_err(|error| {
        DaemonError::InvalidParam(format!("Closure repository root is unavailable: {error}"))
    })?;
    let clean = git_output(&root, &["status", "--porcelain=v1", "-z"]).await?;
    if !clean.is_empty() {
        return Err(DaemonError::InvalidParam(
            "Closure repository must be clean while identity is captured".into(),
        ));
    }
    let base_sha = resolve_ref(&root, config.base_ref.as_str()).await?;
    let destination_pre_head = resolve_ref(&root, config.destination_ref.as_str()).await?;
    let common = git_text(
        &root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .await?;
    let identity_path = PathBuf::from(common.trim())
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(common.trim()));
    let repository_identity =
        ClosureRepositoryIdentityV1::parse(format!("git-common-dir:{}", identity_path.display()))
            .map_err(DaemonError::InvalidParam)?;
    Ok(ResolvedClosureProgramV1 {
        repository_root: root.to_string_lossy().into_owned(),
        repository_identity,
        base_sha,
        destination_pre_head,
    })
}

async fn resolve_ref(root: &Path, reference: &str) -> Result<ClosureGitShaV1> {
    let value = git_text(
        root,
        &["rev-parse", "--verify", &format!("{reference}^{{commit}}")],
    )
    .await?;
    ClosureGitShaV1::parse(value.trim()).map_err(DaemonError::InvalidParam)
}

async fn create_ref_once(root: &Path, reference: &str, target: &str) -> Result<()> {
    let zero = "0".repeat(target.len());
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["update-ref", reference, target, &zero])
        .output()
        .await?;
    if output.status.success() {
        return Ok(());
    }
    if resolve_ref(root, reference)
        .await
        .is_ok_and(|observed| observed.as_str() == target)
    {
        return Ok(());
    }
    Err(DaemonError::Process(format!(
        "failed to create or reconcile immutable Closure staging ref: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}

async fn git_text(root: &Path, args: &[&str]) -> Result<String> {
    let bytes = git_output(root, args).await?;
    String::from_utf8(bytes)
        .map_err(|error| DaemonError::Process(format!("git output was not UTF-8: {error}")))
}

async fn git_output(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .await?;
    if !output.status.success() {
        return Err(DaemonError::InvalidParam(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use crate::store::sandbox_custody::{CustodyCause, NewCustodyRoot, SessionCustodyBinding};
    use chrono::Utc;
    use rsi_common::types::{
        ConversationEvent, EventType, Role, SandboxCleanupState, SessionProvider, SessionStatus,
    };

    fn git(root: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("UTF-8 git output")
            .trim()
            .to_string()
    }

    #[tokio::test]
    async fn closure_program_identity_uses_explicit_refs_when_canonical_head_differs() {
        let directory = tempfile::tempdir().expect("temporary repository");
        let repository = directory.path();
        git(repository, &["init", "-q"]);
        git(
            repository,
            &["config", "user.email", "closure@example.test"],
        );
        git(repository, &["config", "user.name", "Closure Test"]);
        std::fs::write(repository.join("tracked"), "base\n").expect("base file");
        git(repository, &["add", "tracked"]);
        git(repository, &["commit", "-qm", "base"]);
        let base = git(repository, &["rev-parse", "HEAD"]);
        git(repository, &["branch", "closure-base", &base]);
        std::fs::write(repository.join("tracked"), "destination\n").expect("destination file");
        git(repository, &["commit", "-qam", "destination"]);
        let destination = git(repository, &["rev-parse", "HEAD"]);
        git(repository, &["branch", "closure-destination", &destination]);
        std::fs::write(repository.join("tracked"), "unrelated head\n").expect("head file");
        git(repository, &["commit", "-qam", "unrelated head"]);
        let canonical_head = git(repository, &["rev-parse", "HEAD"]);
        assert_ne!(canonical_head, base);
        assert_ne!(canonical_head, destination);

        let resolved = resolve_program_config(&ClosureProgramConfigV1 {
            repository_root: repository.to_path_buf(),
            base_ref: ClosureLocalBranchRefV1::parse("refs/heads/closure-base").expect("base ref"),
            destination_ref: ClosureLocalBranchRefV1::parse("refs/heads/closure-destination")
                .expect("destination ref"),
            review_policy: ClosureReviewPolicyV1::RequiredIndependent,
            verification_policy: ClosureVerificationPolicyV1 {
                required_buckets: vec!["Automated".into()],
                require_all_items_pass: true,
            },
            verifier_policy: ClosureVerifierPolicyV1 {
                commands: Vec::new(),
            },
            checkout_remediation_policy: ClosureCheckoutRemediationPolicyV1::Refuse,
        })
        .await
        .expect("resolve explicit identity");
        assert_eq!(resolved.base_sha.as_str(), base);
        assert_eq!(resolved.destination_pre_head.as_str(), destination);

        let staging_ref = "refs/heads/rsi/closure-stage/identity-test";
        create_ref_once(
            repository,
            staging_ref,
            resolved.destination_pre_head.as_str(),
        )
        .await
        .expect("create staging ref");
        create_ref_once(
            repository,
            staging_ref,
            resolved.destination_pre_head.as_str(),
        )
        .await
        .expect("restart adopts the exact immutable staging ref");
        assert_eq!(git(repository, &["rev-parse", staging_ref]), destination);
        let alien_ref = "refs/heads/rsi/closure-stage/alien";
        git(repository, &["update-ref", alien_ref, &base]);
        assert!(
            create_ref_once(repository, alien_ref, &destination)
                .await
                .is_err(),
            "an alien staging ref must never be overwritten during replay"
        );
        assert_eq!(git(repository, &["rev-parse", alien_ref]), base);
        assert_eq!(git(repository, &["rev-parse", "HEAD"]), canonical_head);
    }

    #[derive(Clone, Copy)]
    enum EvidenceMutation {
        Valid,
        UnknownReviewSchema,
        ManifestV1,
        ManifestWrongSource,
        StaleReviewSha,
        WrongReviewSession,
        WrongReviewInvocation,
        WrongPolicyDigest,
        MalformedReviewJson,
        MissingManifest,
        ExtraCommittedPath,
        DirtyEvidenceWorktree,
        MutatedSourceRef,
    }

    struct EvidenceFixture {
        _tempdir: tempfile::TempDir,
        store: Arc<Mutex<Store>>,
        repository: PathBuf,
        source_worktree: PathBuf,
        evidence_worktree: PathBuf,
        source_ref: String,
        evidence_ref: String,
        sealed_source_head: ClosureGitShaV1,
        evidence_commit: ClosureGitShaV1,
        reviewer_session_id: Uuid,
        reviewer_invocation_id: Uuid,
        source_session_id: Uuid,
        source_invocation_id: Uuid,
        review_path: ClosureEvidencePathV1,
        manifest_path: ClosureEvidencePathV1,
        request: RecordClosureEvidenceRequestV1,
    }

    impl EvidenceFixture {
        fn new(label: &str, mutation: EvidenceMutation) -> Self {
            let tempdir = tempfile::tempdir().expect("evidence fixture");
            let repository = tempdir.path().join("repository");
            let source_worktree = tempdir.path().join("source");
            let evidence_worktree = tempdir.path().join("evidence");
            std::fs::create_dir_all(&repository).expect("repository directory");
            git(&repository, &["init", "-q"]);
            git(
                &repository,
                &["config", "user.email", "closure@example.test"],
            );
            git(&repository, &["config", "user.name", "Closure Test"]);
            std::fs::write(repository.join("tracked"), "base\n").expect("base file");
            git(&repository, &["add", "tracked"]);
            git(&repository, &["commit", "-qm", "base"]);
            let base_sha =
                ClosureGitShaV1::parse(git(&repository, &["rev-parse", "HEAD"])).expect("base SHA");
            let source_branch = format!("rsi/closure-evidence-source-{label}-{}", Uuid::new_v4());
            let evidence_branch = format!("rsi/closure-evidence-review-{label}-{}", Uuid::new_v4());
            git(
                &repository,
                &[
                    "worktree",
                    "add",
                    "-qb",
                    &source_branch,
                    source_worktree.to_str().expect("source path"),
                    base_sha.as_str(),
                ],
            );
            std::fs::write(source_worktree.join("tracked"), "sealed source\n")
                .expect("source change");
            git(&source_worktree, &["commit", "-qam", "sealed source"]);
            let sealed_source_head =
                ClosureGitShaV1::parse(git(&source_worktree, &["rev-parse", "HEAD"]))
                    .expect("sealed source SHA");
            git(
                &repository,
                &[
                    "worktree",
                    "add",
                    "-qb",
                    &evidence_branch,
                    evidence_worktree.to_str().expect("evidence path"),
                    sealed_source_head.as_str(),
                ],
            );
            let destination_ref = "refs/heads/closure-evidence-destination";
            git(
                &repository,
                &["update-ref", destination_ref, base_sha.as_str()],
            );
            let git_common = repository
                .join(".git")
                .canonicalize()
                .expect("git common dir");
            let repository_identity = ClosureRepositoryIdentityV1::parse(format!(
                "git-common-dir:{}",
                git_common.display()
            ))
            .expect("repository identity");

            let mut store = Store::open_in_memory().expect("store");
            let source_session_id = Uuid::new_v4();
            let source_invocation_id = Uuid::new_v4();
            let source_custody_id = Uuid::new_v4();
            Self::insert_session(
                &mut store,
                source_session_id,
                source_invocation_id,
                source_custody_id,
                &repository,
                &source_worktree,
                &source_branch,
                &repository_identity,
                &base_sha,
                "closure-source",
            );
            let create_request = CreateClosureProgramRequestV1 {
                config: ClosureProgramConfigV1 {
                    repository_root: repository.clone(),
                    base_ref: ClosureLocalBranchRefV1::parse("refs/heads/master")
                        .expect("base ref"),
                    destination_ref: ClosureLocalBranchRefV1::parse(destination_ref)
                        .expect("destination ref"),
                    review_policy: ClosureReviewPolicyV1::RequiredIndependent,
                    verification_policy: ClosureVerificationPolicyV1 {
                        required_buckets: vec!["Automated".into()],
                        require_all_items_pass: true,
                    },
                    verifier_policy: ClosureVerifierPolicyV1 {
                        commands: Vec::new(),
                    },
                    checkout_remediation_policy: ClosureCheckoutRemediationPolicyV1::Refuse,
                },
                idempotency_key: Uuid::new_v4(),
            };
            let created = store
                .create_closure_program(
                    &create_request,
                    &ResolvedClosureProgramV1 {
                        repository_root: repository.display().to_string(),
                        repository_identity: repository_identity.clone(),
                        base_sha: base_sha.clone(),
                        destination_pre_head: base_sha.clone(),
                    },
                )
                .expect("Closure program");
            let source_id = ClosureSourceIdV1::new(Uuid::new_v4());
            let source_ref = format!("refs/heads/{source_branch}");
            let staging_ref = format!(
                "refs/heads/rsi/closure-stage/{}/{source_id}",
                created.program_id
            );
            git(
                &repository,
                &["update-ref", &staging_ref, base_sha.as_str()],
            );
            store
                .record_closure_source_launch(&PreparedClosureSourceV1 {
                    source: ClosureSourceIdentityV1 {
                        program_id: created.program_id,
                        source_id,
                        custody_id: source_custody_id,
                        custody_generation: 1,
                        lineage_root_session_id: source_session_id,
                        repository_identity: repository_identity.clone(),
                        source_ref: ClosureLocalBranchRefV1::parse(source_ref.clone())
                            .expect("source ref"),
                        source_base_sha: base_sha.clone(),
                        destination_ref: ClosureLocalBranchRefV1::parse(destination_ref)
                            .expect("destination ref"),
                        destination_pre_head: base_sha,
                        staging_ref: ClosureLocalBranchRefV1::parse(staging_ref)
                            .expect("staging ref"),
                    },
                    root_session_id: source_session_id,
                    idempotency_key: Uuid::new_v4(),
                    request_fingerprint: program_run_fingerprint(
                        "closure.test.evidence-source",
                        label.as_bytes(),
                    ),
                })
                .expect("Closure source");
            store
                .conn
                .execute(
                    "UPDATE closure_sources SET state='awaiting_evidence',source_head=?2 WHERE id=?1",
                    rusqlite::params![source_id.to_string(), sealed_source_head.as_str()],
                )
                .expect("seal source");
            store
                .conn
                .execute(
                    "UPDATE closure_programs SET state='awaiting_evidence' WHERE id=?1",
                    [created.program_id.to_string()],
                )
                .expect("await evidence");

            let reviewer_session_id = Uuid::new_v4();
            let reviewer_invocation_id = Uuid::new_v4();
            let reviewer_custody_id = Uuid::new_v4();
            Self::insert_session(
                &mut store,
                reviewer_session_id,
                reviewer_invocation_id,
                reviewer_custody_id,
                &repository,
                &evidence_worktree,
                &evidence_branch,
                &repository_identity,
                &sealed_source_head,
                "closure-reviewer",
            );
            let (review_policy_digest, _, _) = store
                .closure_program_policy_digests(created.program_id)
                .expect("policy digests");
            let review_path = ClosureEvidencePathV1::parse(format!(
                "thoughts/shared/reviews/closure/{}/{}-review-v1.json",
                created.program_id, source_id
            ))
            .expect("review path");
            let manifest_path = ClosureEvidencePathV1::parse(format!(
                "thoughts/shared/verification/closure/{}/{}-manifest-v2.md",
                created.program_id, source_id
            ))
            .expect("manifest path");
            let review = ClosureReviewArtifactV1 {
                schema_version: CLOSURE_REVIEW_ARTIFACT_SCHEMA_VERSION,
                reviewed_source_head: sealed_source_head.clone(),
                reviewer: ClosureReviewerIdentityV1 {
                    session_id: reviewer_session_id,
                    model_invocation_id: reviewer_invocation_id,
                },
                verdict: ClosureReviewVerdictV1::Accepted,
                findings: Vec::new(),
                unresolved_finding_ids: Vec::new(),
                scope_policy_audit: ClosureScopePolicyAuditV1 {
                    scope_result: ClosureAuditResultV1::Pass,
                    policy_result: ClosureAuditResultV1::Pass,
                    reviewed_scope: vec!["K1 only".into()],
                    review_policy_digest: review_policy_digest.clone(),
                },
            };
            let mut review_value = serde_json::to_value(review).expect("review JSON value");
            match mutation {
                EvidenceMutation::UnknownReviewSchema => {
                    review_value["schema_version"] = serde_json::json!(999);
                }
                EvidenceMutation::StaleReviewSha => {
                    review_value["reviewed_source_head"] =
                        serde_json::json!("0123456789012345678901234567890123456789");
                }
                EvidenceMutation::WrongReviewSession => {
                    review_value["reviewer"]["session_id"] = serde_json::json!(Uuid::new_v4());
                }
                EvidenceMutation::WrongReviewInvocation => {
                    review_value["reviewer"]["model_invocation_id"] =
                        serde_json::json!(Uuid::new_v4());
                }
                EvidenceMutation::WrongPolicyDigest => {
                    review_value["scope_policy_audit"]["review_policy_digest"] =
                        serde_json::json!(format!("sha256:{}", "f".repeat(64)));
                }
                _ => {}
            }
            let review_bytes = if matches!(mutation, EvidenceMutation::MalformedReviewJson) {
                b"{ definitely not JSON".to_vec()
            } else {
                serde_json::to_vec_pretty(&review_value).expect("review bytes")
            };
            let manifest_schema = if matches!(mutation, EvidenceMutation::ManifestV1) {
                1
            } else {
                2
            };
            let manifest_source = if matches!(mutation, EvidenceMutation::ManifestWrongSource) {
                "0123456789012345678901234567890123456789"
            } else {
                sealed_source_head.as_str()
            };
            let manifest_bytes = format!(
                "---\nschema_version: {manifest_schema}\nsource_head: {manifest_source}\nticket: K1\nplan_doc: thoughts/shared/plans/2026-08-14-p0-1-closure-kernel.md\ngenerated: 2026-08-14T12:00:00Z\nphases_sealed: [1]\nstatus: verified\n---\n\n# Verification Manifest - K1\n\n## Phase 1 - Closure\n\n### Automated\n- [PASS] strict evidence fixture\n  satisfies: F-003\n\n### Daemon-level\n- (none)\n\n### TUI manual\n- (none)\n"
            )
            .into_bytes();
            let review_absolute = evidence_worktree.join(review_path.as_str());
            std::fs::create_dir_all(review_absolute.parent().expect("review parent"))
                .expect("review directory");
            std::fs::write(&review_absolute, review_bytes).expect("review artifact");
            git(&evidence_worktree, &["add", review_path.as_str()]);
            if !matches!(mutation, EvidenceMutation::MissingManifest) {
                let manifest_absolute = evidence_worktree.join(manifest_path.as_str());
                std::fs::create_dir_all(manifest_absolute.parent().expect("manifest parent"))
                    .expect("manifest directory");
                std::fs::write(&manifest_absolute, manifest_bytes).expect("manifest artifact");
                git(&evidence_worktree, &["add", manifest_path.as_str()]);
            }
            if matches!(mutation, EvidenceMutation::ExtraCommittedPath) {
                std::fs::write(evidence_worktree.join("extra-evidence"), "extra\n")
                    .expect("extra artifact");
                git(&evidence_worktree, &["add", "extra-evidence"]);
            }
            git(&evidence_worktree, &["commit", "-qm", "Closure evidence"]);
            let evidence_commit =
                ClosureGitShaV1::parse(git(&evidence_worktree, &["rev-parse", "HEAD"]))
                    .expect("evidence commit");
            let handoff = format!(
                "PIPELINE HANDOFF — REVIEW:\nreviewer_session_id: {reviewer_session_id}\nreviewer_model_invocation_id: {reviewer_invocation_id}\nreview_json_path: {review_path}\nmanifest_v2_path: {manifest_path}\nsealed_source_sha: {sealed_source_head}\nevidence_commit_sha: {evidence_commit}\n\n## Stage contract\n\n### Inputs\n\n- Static: `{review_path}` and `{manifest_path}`.\n\n### Process\n\nIndependent review.\n\n### Outputs\n\nStrict artifacts.\n\n### Verify\n\nExact head.\n"
            );
            store
                .insert_event_with_provenance(
                    &ConversationEvent {
                        id: 0,
                        session_id: reviewer_session_id,
                        sequence: 1,
                        event_type: EventType::Message,
                        role: Some(Role::Assistant),
                        created_at: Utc::now(),
                        content: handoff,
                        tool_name: None,
                        tool_input: None,
                        offload_id: None,
                        tool_use_id: None,
                        metadata: None,
                    },
                    &ConversationEventProvenanceV1 {
                        producer_kind: ConversationEventProducerKindV1::ProviderAssistantOutput,
                        model_invocation_id: reviewer_invocation_id,
                        provider_event_type: "assistant".into(),
                    },
                )
                .expect("review handoff");
            if matches!(mutation, EvidenceMutation::DirtyEvidenceWorktree) {
                std::fs::write(evidence_worktree.join("dirty"), "dirty\n")
                    .expect("dirty reviewer worktree");
            }
            if matches!(mutation, EvidenceMutation::MutatedSourceRef) {
                std::fs::write(source_worktree.join("tracked"), "mutated after seal\n")
                    .expect("mutated source");
                git(&source_worktree, &["commit", "-qam", "mutated source"]);
            }
            let request = RecordClosureEvidenceRequestV1 {
                source_id,
                expected_sealed_source_head: sealed_source_head.clone(),
                disposition: ClosureEvidenceDispositionV1::RequiredIndependent {
                    reviewer_session_id,
                    reviewer_model_invocation_id: reviewer_invocation_id,
                    expected_evidence_commit: evidence_commit.clone(),
                    review_json_path: review_path.clone(),
                    manifest_v2_path: manifest_path.clone(),
                },
                idempotency_key: Uuid::new_v4(),
            };
            Self {
                _tempdir: tempdir,
                store: Arc::new(Mutex::new(store)),
                repository,
                source_worktree,
                evidence_worktree,
                source_ref,
                evidence_ref: format!("refs/heads/{evidence_branch}"),
                sealed_source_head,
                evidence_commit,
                reviewer_session_id,
                reviewer_invocation_id,
                source_session_id,
                source_invocation_id,
                review_path,
                manifest_path,
                request,
            }
        }

        #[allow(clippy::too_many_arguments)]
        fn insert_session(
            store: &mut Store,
            session_id: Uuid,
            invocation_id: Uuid,
            custody_id: Uuid,
            repository: &Path,
            worktree: &Path,
            branch: &str,
            repository_identity: &ClosureRepositoryIdentityV1,
            source_commit: &ClosureGitShaV1,
            model: &str,
        ) {
            let mut session = crate::store::tests::make_test_session();
            session.id = session_id;
            session.status = SessionStatus::Starting;
            session.provider = SessionProvider::Claude;
            session.model = Some(model.into());
            session.working_dir = repository.to_path_buf();
            session.sandbox_kind = Some(SandboxKind::GitWorktree);
            session.sandbox_root = Some(worktree.to_path_buf());
            session.sandbox_branch = Some(branch.into());
            session.sandbox_cleanup_state = Some(SandboxCleanupState::Live);
            store
                .conn
                .execute(
                    "INSERT INTO model_invocations
                     (id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,
                      provider,model,trigger_source,session_id,policy_snapshot_json,
                      usage_confidence,created_at)
                     VALUES (?1,'session_launch_fresh','session_lifecycle','foreground',
                             'paid_capable','admitted','running','Claude',?2,
                             'closure_evidence_test',?3,'{}','unavailable',?4)",
                    rusqlite::params![
                        invocation_id.to_string(),
                        model,
                        session_id.to_string(),
                        Utc::now().to_rfc3339()
                    ],
                )
                .expect("model invocation");
            store
                .insert_direct_session_with_custody_and_invocation(
                    &session,
                    SessionCustodyBinding::New(NewCustodyRoot {
                        custody_id,
                        canonical_repo_dir: repository.display().to_string(),
                        sandbox_root: worktree.display().to_string(),
                        sandbox_branch: branch.into(),
                        repository_identity: repository_identity.as_str().into(),
                        source_commit: source_commit.as_str().into(),
                        cause: CustodyCause::FreshLaunch,
                    }),
                    invocation_id,
                )
                .expect("session custody");
            store
                .update_session_status(session_id, SessionStatus::Completed)
                .expect("complete session");
            store
                .conn
                .execute(
                    "UPDATE model_invocations SET status='completed',completed_at=?2 WHERE id=?1",
                    rusqlite::params![invocation_id.to_string(), Utc::now().to_rfc3339()],
                )
                .expect("complete invocation");
        }
    }

    #[tokio::test]
    async fn closure_evidence_accepts_strict_bundle_and_replays_without_mutating_source() {
        let fixture = EvidenceFixture::new("valid", EvidenceMutation::Valid);
        let source_ref_before = git(&fixture.repository, &["rev-parse", &fixture.source_ref]);
        let context = fixture
            .store
            .lock()
            .await
            .closure_evidence_admission_context(
                fixture.request.source_id,
                fixture.reviewer_session_id,
                fixture.reviewer_invocation_id,
            )
            .expect("valid evidence admission context");
        parse_closure_review_handoff_v1(&context.review_handoff_raw)
            .expect("valid persisted review handoff");
        assert!(
            git(&fixture.evidence_worktree, &["status", "--porcelain"]).is_empty(),
            "evidence worktree must be clean"
        );
        assert_eq!(
            git(
                &fixture.repository,
                &[
                    "diff-tree",
                    "--no-commit-id",
                    "--name-only",
                    "-r",
                    fixture.evidence_commit.as_str(),
                ],
            )
            .lines()
            .collect::<Vec<_>>(),
            [fixture.review_path.as_str(), fixture.manifest_path.as_str()]
        );
        let accepted = record_evidence_with_store(&fixture.store, fixture.request.clone())
            .await
            .expect("record evidence");
        assert!(accepted.eligible);
        assert_eq!(accepted.refusal, None);
        assert!(!accepted.replayed);
        let evidence = accepted.evidence.expect("accepted evidence");
        assert_eq!(evidence.review_schema_version, Some(1));
        assert_eq!(evidence.manifest_schema_version, Some(2));
        assert_eq!(
            evidence.manifest_source_head,
            Some(fixture.sealed_source_head.clone())
        );
        assert_eq!(
            evidence.evidence_commit,
            Some(fixture.evidence_commit.clone())
        );
        assert_eq!(
            evidence.reviewer_session_id,
            Some(fixture.reviewer_session_id)
        );
        assert_eq!(
            evidence.reviewer_model_invocation_id,
            Some(fixture.reviewer_invocation_id)
        );
        assert_eq!(
            git(&fixture.repository, &["rev-parse", &fixture.source_ref]),
            source_ref_before
        );
        assert_eq!(
            git(&fixture.source_worktree, &["rev-parse", "HEAD"]),
            fixture.sealed_source_head.as_str()
        );
        assert_eq!(
            git(&fixture.repository, &["rev-parse", &fixture.evidence_ref]),
            fixture.evidence_commit.as_str()
        );
        let replay = record_evidence_with_store(&fixture.store, fixture.request.clone())
            .await
            .expect("exact replay");
        assert!(replay.replayed);
        assert!(replay.eligible);
        let mut conflict = fixture.request;
        conflict.expected_sealed_source_head =
            ClosureGitShaV1::parse("0123456789012345678901234567890123456789")
                .expect("different SHA");
        let conflict = record_evidence_with_store(&fixture.store, conflict)
            .await
            .expect("conflicting replay");
        assert!(conflict.replayed);
        assert_eq!(
            conflict.refusal,
            Some(ClosureRefusalCodeV1::IdempotencyMismatch)
        );
        let store = fixture.store.lock().await;
        let queue_count: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM closure_integration_queue",
                [],
                |row| row.get(0),
            )
            .expect("queue count");
        let forbidden_k2_k3: i64 = store
            .conn
            .query_row(
                "SELECT (SELECT count(*) FROM closure_integration_attempts)
                      + (SELECT count(*) FROM integration_receipts)
                      + (SELECT count(*) FROM closure_cleanup_actions)",
                [],
                |row| row.get(0),
            )
            .expect("K2/K3 rows");
        assert_eq!(queue_count, 1);
        assert_eq!(forbidden_k2_k3, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn closure_evidence_concurrent_admission_durably_consumes_loser_key() {
        let fixture = EvidenceFixture::new("concurrent-admission", EvidenceMutation::Valid);
        let left_request = fixture.request.clone();
        let mut right_request = fixture.request.clone();
        right_request.idempotency_key = Uuid::new_v4();
        let barrier = ClosureEvidencePrecommitBarrier {
            preflight_complete: Arc::new(tokio::sync::Barrier::new(3)),
            commit_allowed: Arc::new(tokio::sync::Barrier::new(3)),
        };

        let left_store = Arc::clone(&fixture.store);
        let left_task_request = left_request.clone();
        let left_barrier = barrier.clone();
        let left = tokio::spawn(async move {
            record_evidence_with_store_inner(&left_store, left_task_request, Some(left_barrier))
                .await
        });
        let right_store = Arc::clone(&fixture.store);
        let right_task_request = right_request.clone();
        let right_barrier = barrier.clone();
        let right = tokio::spawn(async move {
            record_evidence_with_store_inner(&right_store, right_task_request, Some(right_barrier))
                .await
        });

        barrier.preflight_complete.wait().await;
        barrier.commit_allowed.wait().await;
        let left_result = left.await.expect("left task").expect("left result");
        let right_result = right.await.expect("right task").expect("right result");
        assert_ne!(
            left_result.eligible, right_result.eligible,
            "exactly one request must win admission"
        );
        let (loser_request, loser_result) = if left_result.eligible {
            (right_request, right_result)
        } else {
            (left_request, left_result)
        };
        assert_eq!(
            loser_result.refusal,
            Some(ClosureRefusalCodeV1::EvidenceNotEligible)
        );
        assert!(!loser_result.replayed);

        let replay = record_evidence_with_store(&fixture.store, loser_request.clone())
            .await
            .expect("exact loser replay");
        assert!(replay.replayed);
        assert_eq!(
            replay.refusal,
            Some(ClosureRefusalCodeV1::EvidenceNotEligible)
        );
        let mut changed = loser_request;
        changed.expected_sealed_source_head =
            ClosureGitShaV1::parse("0".repeat(40)).expect("changed sealed SHA");
        let mismatch = record_evidence_with_store(&fixture.store, changed)
            .await
            .expect("changed loser replay");
        assert!(mismatch.replayed);
        assert_eq!(
            mismatch.refusal,
            Some(ClosureRefusalCodeV1::IdempotencyMismatch)
        );

        let store = fixture.store.lock().await;
        assert_eq!(
            store
                .conn
                .query_row("SELECT count(*) FROM closure_evidence", [], |row| row
                    .get::<_, i64>(0))
                .expect("evidence count"),
            1
        );
        assert_eq!(
            store
                .conn
                .query_row(
                    "SELECT count(*) FROM closure_operator_requests WHERE method='RecordClosureEvidence'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("operator request count"),
            2
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn closure_evidence_transaction_stale_sha_refusal_is_durable() {
        let fixture = EvidenceFixture::new("transaction-stale-sha", EvidenceMutation::Valid);
        let request = fixture.request.clone();
        let barrier = ClosureEvidencePrecommitBarrier {
            preflight_complete: Arc::new(tokio::sync::Barrier::new(2)),
            commit_allowed: Arc::new(tokio::sync::Barrier::new(2)),
        };
        let task_store = Arc::clone(&fixture.store);
        let task_request = request.clone();
        let task_barrier = barrier.clone();
        let task = tokio::spawn(async move {
            record_evidence_with_store_inner(&task_store, task_request, Some(task_barrier)).await
        });

        barrier.preflight_complete.wait().await;
        fixture
            .store
            .lock()
            .await
            .conn
            .execute(
                "UPDATE closure_sources SET source_head=?2 WHERE id=?1",
                rusqlite::params![request.source_id.to_string(), "0".repeat(40)],
            )
            .expect("move durable sealed source head after preflight");
        barrier.commit_allowed.wait().await;
        let result = task.await.expect("evidence task").expect("typed refusal");
        assert!(!result.replayed);
        assert_eq!(result.refusal, Some(ClosureRefusalCodeV1::StaleReviewedSha));

        let replay = record_evidence_with_store(&fixture.store, request.clone())
            .await
            .expect("exact stale-SHA replay");
        assert!(replay.replayed);
        assert_eq!(replay.refusal, Some(ClosureRefusalCodeV1::StaleReviewedSha));
        let mut changed = request;
        changed.expected_sealed_source_head =
            ClosureGitShaV1::parse("1".repeat(40)).expect("changed sealed SHA");
        let mismatch = record_evidence_with_store(&fixture.store, changed)
            .await
            .expect("changed stale-SHA replay");
        assert!(mismatch.replayed);
        assert_eq!(
            mismatch.refusal,
            Some(ClosureRefusalCodeV1::IdempotencyMismatch)
        );
        assert_eq!(
            fixture
                .store
                .lock()
                .await
                .conn
                .query_row(
                    "SELECT count(*) FROM closure_operator_requests WHERE method='RecordClosureEvidence'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("stale-SHA receipt count"),
            1
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn closure_evidence_transaction_invalid_custody_refusal_is_durable() {
        let fixture = EvidenceFixture::new("transaction-invalid-custody", EvidenceMutation::Valid);
        let request = fixture.request.clone();
        let barrier = ClosureEvidencePrecommitBarrier {
            preflight_complete: Arc::new(tokio::sync::Barrier::new(2)),
            commit_allowed: Arc::new(tokio::sync::Barrier::new(2)),
        };
        let task_store = Arc::clone(&fixture.store);
        let task_request = request.clone();
        let task_barrier = barrier.clone();
        let task = tokio::spawn(async move {
            record_evidence_with_store_inner(&task_store, task_request, Some(task_barrier)).await
        });

        barrier.preflight_complete.wait().await;
        fixture
            .store
            .lock()
            .await
            .conn
            .execute(
                "UPDATE sessions SET status='Running' WHERE id=?1",
                [fixture.reviewer_session_id.to_string()],
            )
            .expect("invalidate reviewer after preflight");
        barrier.commit_allowed.wait().await;
        let result = task.await.expect("evidence task").expect("typed refusal");
        assert!(!result.replayed);
        assert_eq!(
            result.refusal,
            Some(ClosureRefusalCodeV1::InvalidEvidenceCustody)
        );

        let replay = record_evidence_with_store(&fixture.store, request.clone())
            .await
            .expect("exact invalid-custody replay");
        assert!(replay.replayed);
        assert_eq!(
            replay.refusal,
            Some(ClosureRefusalCodeV1::InvalidEvidenceCustody)
        );
        let mut changed = request;
        changed.expected_sealed_source_head =
            ClosureGitShaV1::parse("0".repeat(40)).expect("changed sealed SHA");
        let mismatch = record_evidence_with_store(&fixture.store, changed)
            .await
            .expect("changed invalid-custody replay");
        assert!(mismatch.replayed);
        assert_eq!(
            mismatch.refusal,
            Some(ClosureRefusalCodeV1::IdempotencyMismatch)
        );
        assert_eq!(
            fixture
                .store
                .lock()
                .await
                .conn
                .query_row(
                    "SELECT count(*) FROM closure_operator_requests WHERE method='RecordClosureEvidence'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("invalid-custody receipt count"),
            1
        );
    }

    #[tokio::test]
    async fn closure_evidence_rejects_schema_binding_custody_and_git_failures() {
        for (label, mutation, refusal) in [
            (
                "unknown-schema",
                EvidenceMutation::UnknownReviewSchema,
                ClosureRefusalCodeV1::UnsupportedReviewSchema,
            ),
            (
                "manifest-v1",
                EvidenceMutation::ManifestV1,
                ClosureRefusalCodeV1::ManifestV1Unbound,
            ),
            (
                "manifest-wrong-source",
                EvidenceMutation::ManifestWrongSource,
                ClosureRefusalCodeV1::ManifestSourceHeadMismatch,
            ),
            (
                "stale-review-sha",
                EvidenceMutation::StaleReviewSha,
                ClosureRefusalCodeV1::StaleReviewedSha,
            ),
            (
                "wrong-review-session",
                EvidenceMutation::WrongReviewSession,
                ClosureRefusalCodeV1::InvalidEvidenceCustody,
            ),
            (
                "wrong-review-invocation",
                EvidenceMutation::WrongReviewInvocation,
                ClosureRefusalCodeV1::InvalidEvidenceCustody,
            ),
            (
                "wrong-policy-digest",
                EvidenceMutation::WrongPolicyDigest,
                ClosureRefusalCodeV1::ReviewPolicyMismatch,
            ),
            (
                "malformed-review-json",
                EvidenceMutation::MalformedReviewJson,
                ClosureRefusalCodeV1::EvidenceNotEligible,
            ),
            (
                "missing-manifest",
                EvidenceMutation::MissingManifest,
                ClosureRefusalCodeV1::InvalidEvidenceCustody,
            ),
            (
                "extra-committed-path",
                EvidenceMutation::ExtraCommittedPath,
                ClosureRefusalCodeV1::InvalidEvidenceCustody,
            ),
            (
                "dirty-reviewer-worktree",
                EvidenceMutation::DirtyEvidenceWorktree,
                ClosureRefusalCodeV1::InvalidEvidenceCustody,
            ),
            (
                "mutated-source-ref",
                EvidenceMutation::MutatedSourceRef,
                ClosureRefusalCodeV1::StaleReviewedSha,
            ),
        ] {
            let fixture = EvidenceFixture::new(label, mutation);
            let result = record_evidence_with_store(&fixture.store, fixture.request.clone())
                .await
                .expect("typed refusal");
            assert!(!result.eligible, "{label}");
            assert_eq!(result.refusal, Some(refusal), "{label}");
            let replay = record_evidence_with_store(&fixture.store, fixture.request.clone())
                .await
                .expect("exact refusal replay");
            assert!(replay.replayed, "{label}");
            assert_eq!(replay.refusal, Some(refusal), "{label}");
            let mut changed = fixture.request.clone();
            changed.expected_sealed_source_head =
                ClosureGitShaV1::parse("0".repeat(40)).expect("changed sealed SHA");
            let mismatch = record_evidence_with_store(&fixture.store, changed)
                .await
                .expect("changed-payload refusal replay");
            assert!(mismatch.replayed, "{label}");
            assert_eq!(
                mismatch.refusal,
                Some(ClosureRefusalCodeV1::IdempotencyMismatch),
                "{label}"
            );
            assert_eq!(
                fixture
                    .store
                    .lock()
                    .await
                    .conn
                    .query_row("SELECT count(*) FROM closure_evidence", [], |row| row
                        .get::<_, i64>(0))
                    .expect("evidence count"),
                0,
                "{label}"
            );
            assert_eq!(
                fixture
                    .store
                    .lock()
                    .await
                    .conn
                    .query_row(
                        "SELECT count(*) FROM closure_operator_requests WHERE method='RecordClosureEvidence'",
                        [],
                        |row| row.get::<_, i64>(0),
                    )
                    .expect("refusal replay count"),
                1,
                "{label}"
            );
        }
    }

    #[tokio::test]
    async fn closure_evidence_requires_reviewer_outside_source_lineage_and_custody() {
        let fixture = EvidenceFixture::new("source-as-reviewer", EvidenceMutation::Valid);
        let mut request = fixture.request;
        request.disposition = ClosureEvidenceDispositionV1::RequiredIndependent {
            reviewer_session_id: fixture.source_session_id,
            reviewer_model_invocation_id: fixture.source_invocation_id,
            expected_evidence_commit: fixture.evidence_commit,
            review_json_path: fixture.review_path,
            manifest_v2_path: fixture.manifest_path,
        };
        let result = record_evidence_with_store(&fixture.store, request)
            .await
            .expect("typed source-custody refusal");
        assert!(!result.eligible);
        assert_eq!(
            result.refusal,
            Some(ClosureRefusalCodeV1::InvalidEvidenceCustody)
        );
    }

    #[test]
    fn closure_verification_policy_rejects_vacuous_or_nonverified_manifest_v2() {
        let source_head = ClosureGitShaV1::parse("a".repeat(40)).expect("source head");
        let policy = ClosureVerificationPolicyV1 {
            required_buckets: vec!["Automated".into()],
            require_all_items_pass: true,
        };
        let manifest = |status: &str, automated: &str| {
            let raw = format!(
                "---\nschema_version: 2\nsource_head: {source_head}\nticket: K1\nplan_doc: thoughts/shared/plans/closure.md\ngenerated: 2026-08-14T12:00:00Z\nphases_sealed: [1]\nstatus: {status}\n---\n\n# Verification Manifest - K1\n\n## Phase 1 - Closure\n\n### Automated\n{automated}\n\n### Daemon-level\n- (none)\n\n### TUI manual\n- (none)\n"
            );
            rsi_common::verification_manifest::parse_closure_v2_for_source(&raw, &source_head)
                .expect("strict manifest V2")
        };

        assert!(!verification_policy_passes(
            &policy,
            &manifest("verified", "- (none)")
        ));
        assert!(!verification_policy_passes(
            &policy,
            &manifest("pending_verification", "- [PASS] automated proof")
        ));
        assert!(!verification_policy_passes(
            &policy,
            &manifest("tui_only_pending", "- [PASS] automated proof")
        ));
        assert!(verification_policy_passes(
            &policy,
            &manifest("verified", "- [PASS] automated proof")
        ));

        let tui_required = ClosureVerificationPolicyV1 {
            required_buckets: vec!["Automated".into(), "TUI manual".into()],
            require_all_items_pass: true,
        };
        assert!(!verification_policy_passes(
            &tui_required,
            &manifest("verified", "- [PASS] automated proof")
        ));
    }
}
