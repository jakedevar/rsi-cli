//! Provenance-bound, exactly-once Closure terminal-output ingestion.

use crate::error::{DaemonError, Result};
use crate::store::Store;
use crate::store::closure_kernel::{
    ClosureOutputCaptureProposalV1, ClosureTerminalCaptureCandidateV1,
};
use rsi_common::agent_contract::parse_closure_pipeline_handoff_v1;
use rsi_common::closure_kernel::*;
use rsi_common::program_runs::program_run_fingerprint;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::process::Command;
use tokio::sync::Mutex;

pub(crate) async fn capture_or_replay_terminal_output(
    store: Arc<Mutex<Store>>,
    source_id: ClosureSourceIdV1,
) -> Result<Option<ClosureOutputValidationResultV1>> {
    let candidate = {
        let store = store.lock().await;
        store.closure_terminal_capture_candidate(source_id)?
    };
    let Some(candidate) = candidate else {
        return Ok(None);
    };
    let proposal = build_capture_proposal(&candidate).await?;
    let result = store
        .lock()
        .await
        .capture_or_replay_closure_output(&proposal)?;
    Ok(Some(result))
}

pub(crate) async fn capture_for_terminal_session(
    store: Arc<Mutex<Store>>,
    session_id: uuid::Uuid,
) -> Result<Option<ClosureOutputValidationResultV1>> {
    let source_id = store.lock().await.closure_source_for_session(session_id)?;
    match source_id {
        Some(source_id) => capture_or_replay_terminal_output(store, source_id).await,
        None => Ok(None),
    }
}

pub(crate) async fn reconcile_after_restart(
    store: Arc<Mutex<Store>>,
    budget: ClosureOutputRecoveryBudgetV1,
    cursor: Option<ClosureOutputRecoveryCursorV1>,
    high_water: Option<ClosureOutputRecoveryCursorV1>,
) -> Result<ClosureOutputRecoveryResultV1> {
    if budget.page_size == 0 || budget.max_pages == 0 || budget.time_budget_ms == 0 {
        return Err(DaemonError::InvalidParam(
            "Closure output recovery budget values must be positive".into(),
        ));
    }
    let started = Instant::now();
    let deadline = Duration::from_millis(budget.time_budget_ms);
    let high_water = match high_water {
        Some(value) => Some(value),
        None => store.lock().await.closure_output_recovery_high_water()?,
    };
    let mut cursor = cursor;
    let mut examined = 0_u32;
    let mut committed = 0_u32;
    let mut replayed = 0_u32;
    let mut deferred = 0_u32;
    let mut deadline_reached = false;
    let mut exhausted = false;

    for _ in 0..budget.max_pages {
        if started.elapsed() >= deadline {
            deadline_reached = true;
            break;
        }
        let page = store.lock().await.closure_output_recovery_page(
            cursor.as_ref(),
            high_water.as_ref(),
            budget.page_size,
        )?;
        if page.is_empty() {
            exhausted = true;
            break;
        }
        let mut processed_in_page = 0_usize;
        for source_id in &page {
            if started.elapsed() >= deadline {
                deadline_reached = true;
                break;
            }
            examined = examined.saturating_add(1);
            match capture_or_replay_terminal_output(Arc::clone(&store), *source_id).await? {
                Some(result) if result.replayed => replayed = replayed.saturating_add(1),
                Some(_) => committed = committed.saturating_add(1),
                None => deferred = deferred.saturating_add(1),
            }
            let created_at = {
                let store = store.lock().await;
                store.closure_source_created_at(*source_id)?
            };
            cursor = Some(ClosureOutputRecoveryCursorV1 {
                source_created_at: created_at,
                source_id: *source_id,
            });
            processed_in_page += 1;
        }
        if processed_in_page < page.len() {
            deadline_reached = true;
            break;
        }
        if page.len() < budget.page_size as usize {
            exhausted = !deadline_reached;
            break;
        }
    }

    Ok(ClosureOutputRecoveryResultV1 {
        examined,
        committed,
        replayed,
        deferred,
        next_cursor: (!exhausted).then_some(cursor).flatten(),
        high_water: (!exhausted).then_some(high_water).flatten(),
        deadline_reached,
    })
}

async fn build_capture_proposal(
    candidate: &ClosureTerminalCaptureCandidateV1,
) -> Result<ClosureOutputCaptureProposalV1> {
    let key = ClosureOutputIngestionKeyV1 {
        source_id: candidate.source.source_id,
        tip_session_id: candidate.tip_session_id,
        model_invocation_id: candidate.model_invocation_id,
    };
    let raw_digest = candidate
        .raw_handoff
        .as_deref()
        .map(|raw| program_run_fingerprint("closure-terminal-handoff:v1", raw.as_bytes()));
    let mut proposal = ClosureOutputCaptureProposalV1 {
        key,
        expected_custody_id: candidate.source.custody_id,
        expected_custody_generation: candidate.source.custody_generation,
        expected_rotation_depth: candidate.rotation_depth,
        terminal_status: candidate.terminal_status.clone(),
        event_id: candidate.event_id,
        event_sequence: candidate.event_sequence,
        provider_event_type: candidate.provider_event_type.clone(),
        raw_handoff: candidate.raw_handoff.clone(),
        raw_handoff_digest: raw_digest,
        normalized_envelope: None,
        normalized_envelope_digest: None,
        disposition: ClosureOutputValidationDispositionV1::MalformedOutput,
        issues: Vec::new(),
        source_state: ClosureSourceStateV1::OutcomeBlocked,
        sealed_source_head: None,
        observed_source_ref_head: None,
        observed_worktree_head: None,
        observed_worktree_clean: None,
    };

    if let Some(issue) = &candidate.lineage_issue {
        proposal.disposition = ClosureOutputValidationDispositionV1::BlockedAmbiguousLineage;
        proposal.issues.push(issue.clone());
        return Ok(proposal);
    }
    let Some(raw) = candidate.raw_handoff.as_deref() else {
        proposal.disposition = ClosureOutputValidationDispositionV1::MissingProviderOutput;
        proposal
            .issues
            .push("terminal invocation has no non-empty provider-authored assistant output".into());
        return Ok(proposal);
    };
    let handoff = match parse_closure_pipeline_handoff_v1(raw) {
        Ok(handoff) => handoff,
        Err(error) => {
            proposal.issues.push(error.to_string());
            return Ok(proposal);
        }
    };
    proposal.normalized_envelope_digest = Some(program_run_fingerprint(
        "closure-child-output-envelope:v1",
        handoff.raw_outcome_json.as_bytes(),
    ));
    proposal.normalized_envelope = Some(handoff.outcome.clone());
    let correlation = &handoff.outcome.correlation;
    if correlation.program_id != candidate.source.program_id
        || correlation.source_id != candidate.source.source_id
        || correlation.custody_id != candidate.source.custody_id
        || correlation.custody_generation != candidate.source.custody_generation
        || correlation.lineage_root_session_id != candidate.source.lineage_root_session_id
        || correlation.tip_session_id != candidate.tip_session_id
        || correlation.rotation_depth != candidate.rotation_depth
        || correlation.model_invocation_id != candidate.model_invocation_id
        || correlation.source_base_sha != candidate.source.source_base_sha
    {
        proposal.disposition = ClosureOutputValidationDispositionV1::CorrelationMismatch;
        proposal
            .issues
            .push("Closure outcome correlation does not match durable source/tip identity".into());
        return Ok(proposal);
    }

    match &handoff.outcome.outcome {
        ClosureChildOutcomeV1::Blocker { .. } => {
            proposal.disposition = ClosureOutputValidationDispositionV1::AcceptedBlocker;
            proposal.source_state = ClosureSourceStateV1::OutcomeBlocker;
            return Ok(proposal);
        }
        ClosureChildOutcomeV1::Committed { .. } | ClosureChildOutcomeV1::NoChange { .. }
            if candidate.terminal_status != "Completed" =>
        {
            proposal.issues.push(
                "committed/no_change outcome requires durable SessionStatus::Completed".into(),
            );
            return Ok(proposal);
        }
        _ => {}
    }

    let observed = observe_source(candidate).await?;
    proposal.observed_source_ref_head = Some(observed.source_ref_head.clone());
    proposal.observed_worktree_head = Some(observed.worktree_head.clone());
    proposal.observed_worktree_clean = Some(observed.clean);
    match &handoff.outcome.outcome {
        ClosureChildOutcomeV1::Committed {
            reported_source_head,
        } if observed.clean
            && observed.source_ref_head == observed.worktree_head
            && observed.source_ref_head == *reported_source_head
            && observed.source_ref_head != candidate.source.source_base_sha
            && observed.descends_from_base =>
        {
            proposal.disposition = ClosureOutputValidationDispositionV1::AcceptedCommitted;
            proposal.source_state = ClosureSourceStateV1::OutcomeCommitted;
            proposal.sealed_source_head = Some(observed.source_ref_head);
        }
        ClosureChildOutcomeV1::NoChange {
            observed_source_head,
            ..
        } if observed.clean
            && observed.source_ref_head == candidate.source.source_base_sha
            && observed.worktree_head == candidate.source.source_base_sha
            && *observed_source_head == candidate.source.source_base_sha =>
        {
            proposal.disposition = ClosureOutputValidationDispositionV1::AcceptedNoChange;
            proposal.source_state = ClosureSourceStateV1::OutcomeNoChange;
            proposal.sealed_source_head = Some(candidate.source.source_base_sha.clone());
        }
        _ => {
            proposal.disposition = ClosureOutputValidationDispositionV1::BlockedOutcomeGitMismatch;
            proposal
                .issues
                .push("declared outcome contradicts observed source Git state".into());
        }
    }
    Ok(proposal)
}

struct SourceObservation {
    source_ref_head: ClosureGitShaV1,
    worktree_head: ClosureGitShaV1,
    clean: bool,
    descends_from_base: bool,
}

async fn observe_source(
    candidate: &ClosureTerminalCaptureCandidateV1,
) -> Result<SourceObservation> {
    let source_ref_head = git_stdout(
        Path::new(&candidate.repository_root),
        &[
            "rev-parse",
            &format!("{}^{{commit}}", candidate.source.source_ref),
        ],
    )
    .await?;
    let worktree = Path::new(&candidate.source_worktree_root);
    let worktree_head = git_stdout(worktree, &["rev-parse", "HEAD^{commit}"]).await?;
    let status = git_stdout_raw(worktree, &["status", "--porcelain=v1", "-z"]).await?;
    let descends_from_base = git_status(
        Path::new(&candidate.repository_root),
        &[
            "merge-base",
            "--is-ancestor",
            candidate.source.source_base_sha.as_str(),
            source_ref_head.trim(),
        ],
    )
    .await?;
    Ok(SourceObservation {
        source_ref_head: ClosureGitShaV1::parse(source_ref_head.trim())
            .map_err(DaemonError::Store)?,
        worktree_head: ClosureGitShaV1::parse(worktree_head.trim()).map_err(DaemonError::Store)?,
        clean: status.is_empty(),
        descends_from_base,
    })
}

async fn git_stdout(root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .await?;
    if !output.status.success() {
        return Err(DaemonError::Process(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    String::from_utf8(output.stdout)
        .map_err(|error| DaemonError::Process(format!("git output was not UTF-8: {error}")))
}

async fn git_stdout_raw(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .await?;
    if !output.status.success() {
        return Err(DaemonError::Process(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(output.stdout)
}

async fn git_status(root: &Path, args: &[&str]) -> Result<bool> {
    let status = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .status()
        .await?;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(DaemonError::Process(format!(
            "git {} failed with {status}",
            args.join(" ")
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::closure_kernel::{PreparedClosureSourceV1, ResolvedClosureProgramV1};
    use crate::store::sandbox_custody::{CustodyCause, NewCustodyRoot, SessionCustodyBinding};
    use chrono::Utc;
    use rsi_common::types::{
        ConversationEvent, EventType, Role, SandboxCleanupState, SandboxKind, SessionStatus,
    };
    use uuid::Uuid;

    struct IngressFixture {
        _tempdir: tempfile::TempDir,
        store: Arc<Mutex<Store>>,
        program_id: ClosureProgramIdV1,
        source_id: ClosureSourceIdV1,
        session_id: Uuid,
        invocation_id: Uuid,
        custody_id: Uuid,
        base_sha: ClosureGitShaV1,
        repository: std::path::PathBuf,
        worktree: std::path::PathBuf,
    }

    impl IngressFixture {
        fn new(label: &str) -> Self {
            let tempdir = tempfile::tempdir().expect("temporary repository");
            let repository = tempdir.path().join("repository");
            let sandboxes = tempdir.path().join("sandboxes");
            let worktree = sandboxes.join("source");
            std::fs::create_dir_all(&repository).expect("repository directory");
            std::fs::create_dir_all(&sandboxes).expect("sandbox directory");
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
            let source_branch = format!("rsi/closure-test-{label}-{}", Uuid::new_v4());
            git(
                &repository,
                &[
                    "worktree",
                    "add",
                    "-qb",
                    &source_branch,
                    worktree.to_str().expect("worktree path"),
                    "HEAD",
                ],
            );
            let destination_ref = "refs/heads/closure-test-destination";
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
            let mut session = crate::store::tests::make_test_session();
            session.status = SessionStatus::Starting;
            session.working_dir = repository.clone();
            session.sandbox_kind = Some(SandboxKind::GitWorktree);
            session.sandbox_root = Some(worktree.clone());
            session.sandbox_branch = Some(source_branch.clone());
            session.sandbox_cleanup_state = Some(SandboxCleanupState::Live);
            let invocation_id = Uuid::new_v4();
            store
                .conn
                .execute(
                    "INSERT INTO model_invocations
                     (id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,
                      provider,model,trigger_source,session_id,policy_snapshot_json,
                      usage_confidence,created_at)
                     VALUES (?1,'session_launch_fresh','session_lifecycle','foreground',
                             'paid_capable','admitted','running','Claude','closure-test',
                             'closure_test',?2,'{}','unavailable',?3)",
                    rusqlite::params![
                        invocation_id.to_string(),
                        session.id.to_string(),
                        Utc::now().to_rfc3339()
                    ],
                )
                .expect("model invocation");
            let custody_id = Uuid::new_v4();
            store
                .insert_direct_session_with_custody_and_invocation(
                    &session,
                    SessionCustodyBinding::New(NewCustodyRoot {
                        custody_id,
                        canonical_repo_dir: repository.display().to_string(),
                        sandbox_root: worktree.display().to_string(),
                        sandbox_branch: source_branch.clone(),
                        repository_identity: repository_identity.as_str().to_string(),
                        source_commit: base_sha.as_str().to_string(),
                        cause: CustodyCause::FreshLaunch,
                    }),
                    invocation_id,
                )
                .expect("session/custody/invocation");
            store
                .update_session_status(session.id, SessionStatus::Completed)
                .expect("terminal session");
            store
                .conn
                .execute(
                    "UPDATE model_invocations SET status='completed',completed_at=?2 WHERE id=?1",
                    rusqlite::params![invocation_id.to_string(), Utc::now().to_rfc3339()],
                )
                .expect("complete invocation");

            let base_ref_name = git(&repository, &["symbolic-ref", "--short", "HEAD"]);
            let create_request = CreateClosureProgramRequestV1 {
                config: ClosureProgramConfigV1 {
                    repository_root: repository.clone(),
                    base_ref: ClosureLocalBranchRefV1::parse(format!("refs/heads/{base_ref_name}"))
                        .expect("base ref"),
                    destination_ref: ClosureLocalBranchRefV1::parse(destination_ref)
                        .expect("destination ref"),
                    review_policy: ClosureReviewPolicyV1::NotRequired {
                        basis: ClosureReviewNotRequiredBasisV1::Tier0Deterministic,
                        rationale: "deterministic test".into(),
                    },
                    verification_policy: ClosureVerificationPolicyV1 {
                        required_buckets: Vec::new(),
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
            let source_ref = ClosureLocalBranchRefV1::parse(format!("refs/heads/{source_branch}"))
                .expect("source ref");
            let staging_ref = ClosureLocalBranchRefV1::parse(format!(
                "refs/heads/rsi/closure-stage/{}/{source_id}",
                created.program_id
            ))
            .expect("staging ref");
            git(
                &repository,
                &["update-ref", staging_ref.as_str(), base_sha.as_str()],
            );
            store
                .record_closure_source_launch(&PreparedClosureSourceV1 {
                    source: ClosureSourceIdentityV1 {
                        program_id: created.program_id,
                        source_id,
                        custody_id,
                        custody_generation: 1,
                        lineage_root_session_id: session.id,
                        repository_identity,
                        source_ref,
                        source_base_sha: base_sha.clone(),
                        destination_ref: ClosureLocalBranchRefV1::parse(destination_ref)
                            .expect("destination ref"),
                        destination_pre_head: base_sha.clone(),
                        staging_ref,
                    },
                    root_session_id: session.id,
                    idempotency_key: Uuid::new_v4(),
                    request_fingerprint: program_run_fingerprint(
                        "closure.test.launch",
                        label.as_bytes(),
                    ),
                })
                .expect("Closure source");
            Self {
                _tempdir: tempdir,
                store: Arc::new(Mutex::new(store)),
                program_id: created.program_id,
                source_id,
                session_id: session.id,
                invocation_id,
                custody_id,
                base_sha,
                repository,
                worktree,
            }
        }

        async fn insert_provider_handoff(&self, sequence: i32, content: String) -> i64 {
            self.insert_event(
                sequence,
                content,
                ConversationEventProducerKindV1::ProviderAssistantOutput,
                "assistant",
            )
            .await
        }

        async fn insert_diagnostic(&self, sequence: i32, content: &str) -> i64 {
            self.insert_event(
                sequence,
                content.to_string(),
                ConversationEventProducerKindV1::DaemonProviderDiagnostic,
                "process_error",
            )
            .await
        }

        async fn insert_event(
            &self,
            sequence: i32,
            content: String,
            producer_kind: ConversationEventProducerKindV1,
            provider_event_type: &str,
        ) -> i64 {
            self.store
                .lock()
                .await
                .insert_event_with_provenance(
                    &ConversationEvent {
                        id: 0,
                        session_id: self.session_id,
                        sequence,
                        event_type: EventType::Message,
                        role: Some(Role::Assistant),
                        created_at: Utc::now(),
                        content,
                        tool_name: None,
                        tool_input: None,
                        offload_id: None,
                        tool_use_id: None,
                        metadata: None,
                    },
                    &ConversationEventProvenanceV1 {
                        producer_kind,
                        model_invocation_id: self.invocation_id,
                        provider_event_type: provider_event_type.into(),
                    },
                )
                .expect("event with provenance")
        }

        async fn insert_provider_handoff_for(
            &self,
            session_id: Uuid,
            invocation_id: Uuid,
            sequence: i32,
            content: String,
        ) -> i64 {
            self.store
                .lock()
                .await
                .insert_event_with_provenance(
                    &ConversationEvent {
                        id: 0,
                        session_id,
                        sequence,
                        event_type: EventType::Message,
                        role: Some(Role::Assistant),
                        created_at: Utc::now(),
                        content,
                        tool_name: None,
                        tool_input: None,
                        offload_id: None,
                        tool_use_id: None,
                        metadata: None,
                    },
                    &ConversationEventProvenanceV1 {
                        producer_kind: ConversationEventProducerKindV1::ProviderAssistantOutput,
                        model_invocation_id: invocation_id,
                        provider_event_type: "assistant".into(),
                    },
                )
                .expect("rotated event with provenance")
        }

        fn no_change_handoff(&self) -> String {
            self.handoff(ClosureChildOutcomeV1::NoChange {
                observed_source_head: self.base_sha.clone(),
                reason: "requested work was already present".into(),
            })
        }

        fn handoff(&self, outcome: ClosureChildOutcomeV1) -> String {
            self.handoff_for(self.session_id, self.invocation_id, 0, 1, outcome)
        }

        fn handoff_for(
            &self,
            tip_session_id: Uuid,
            model_invocation_id: Uuid,
            rotation_depth: u32,
            custody_generation: u64,
            outcome: ClosureChildOutcomeV1,
        ) -> String {
            let envelope = ClosureChildOutputEnvelopeV1 {
                schema_version: CLOSURE_CHILD_OUTPUT_SCHEMA_VERSION,
                correlation: ClosureChildCorrelationV1 {
                    program_id: self.program_id,
                    source_id: self.source_id,
                    custody_id: self.custody_id,
                    custody_generation,
                    lineage_root_session_id: self.session_id,
                    tip_session_id,
                    rotation_depth,
                    model_invocation_id,
                    source_base_sha: self.base_sha.clone(),
                },
                summary: "terminal Closure result".into(),
                outcome,
            };
            format!(
                "PIPELINE HANDOFF — CLOSURE:\nclosure_outcome_v1: {}\n\n## Stage contract\n\n### Inputs\n\nStatic input: sealed correlation\n\n### Process\n\nChecked the source.\n\n### Outputs\n\nNo change.\n\n### Verify\n\nGit identity checked.\n",
                serde_json::to_string(&envelope).expect("envelope JSON")
            )
        }

        fn commit_source(&self) -> ClosureGitShaV1 {
            std::fs::write(self.worktree.join("tracked"), "committed\n")
                .expect("write source change");
            git(&self.worktree, &["add", "tracked"]);
            git(&self.worktree, &["commit", "-qm", "Closure source change"]);
            ClosureGitShaV1::parse(git(&self.worktree, &["rev-parse", "HEAD"]))
                .expect("committed source SHA")
        }

        async fn set_invocation_status(&self, status: &str) {
            self.store
                .lock()
                .await
                .conn
                .execute(
                    "UPDATE model_invocations SET status=?2 WHERE id=?1",
                    rusqlite::params![self.invocation_id.to_string(), status],
                )
                .expect("model invocation status");
        }

        async fn rotate(&self) -> (Uuid, Uuid) {
            let mut store = self.store.lock().await;
            let mut successor = crate::store::tests::make_test_session();
            successor.id = Uuid::new_v4();
            successor.status = SessionStatus::Starting;
            successor.working_dir = self.repository.clone();
            successor.continued_from = Some(self.session_id);
            successor.rotation_depth = 1;
            successor.sandbox_kind = Some(SandboxKind::GitWorktree);
            successor.sandbox_root = Some(self.worktree.clone());
            successor.sandbox_branch = Some(git(&self.worktree, &["branch", "--show-current"]));
            successor.sandbox_cleanup_state = Some(SandboxCleanupState::Live);
            let invocation_id = Uuid::new_v4();
            store
                .conn
                .execute(
                    "INSERT INTO model_invocations
                     (id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,
                      provider,model,trigger_source,session_id,policy_snapshot_json,
                      usage_confidence,created_at)
                     VALUES (?1,'session_launch_fresh','session_lifecycle','foreground',
                             'paid_capable','admitted','running','Claude','closure-test',
                             'closure_rotation_test',?2,'{}','unavailable',?3)",
                    rusqlite::params![
                        invocation_id.to_string(),
                        successor.id.to_string(),
                        Utc::now().to_rfc3339()
                    ],
                )
                .expect("rotation model invocation");
            store
                .insert_session_with_model_invocation(&successor, invocation_id)
                .expect("rotation successor");
            store
                .bind_reserved_session_custody(
                    successor.id,
                    SessionCustodyBinding::Transfer {
                        custody_id: self.custody_id,
                        from_session_id: self.session_id,
                        generation: 1,
                        cause: CustodyCause::Rotation,
                        origin_session_id: Some(self.session_id),
                        scheduled_job_id: None,
                    },
                )
                .expect("rotation custody transfer");
            store
                .append_closure_source_session(
                    self.source_id,
                    successor.id,
                    self.session_id,
                    1,
                    self.custody_id,
                    2,
                )
                .expect("Closure rotation binding");
            store
                .update_session_status(successor.id, SessionStatus::Completed)
                .expect("terminal successor");
            store
                .conn
                .execute(
                    "UPDATE model_invocations SET status='completed',completed_at=?2 WHERE id=?1",
                    rusqlite::params![invocation_id.to_string(), Utc::now().to_rfc3339()],
                )
                .expect("complete rotation invocation");
            (successor.id, invocation_id)
        }

        async fn insert_unbound_successor(&self) -> Uuid {
            let mut successor = crate::store::tests::make_test_session();
            successor.id = Uuid::new_v4();
            successor.status = SessionStatus::Starting;
            successor.working_dir = self.repository.clone();
            successor.continued_from = Some(self.session_id);
            successor.rotation_depth = 1;
            self.store
                .lock()
                .await
                .insert_session(&successor)
                .expect("unbound rotation successor");
            successor.id
        }

        async fn add_missing_source(&self, label: &str) -> ClosureSourceIdV1 {
            let worktree = self._tempdir.path().join("sandboxes").join(label);
            std::fs::create_dir_all(&worktree).expect("additional sandbox path");
            let source_branch = format!("rsi/closure-recovery-{label}-{}", Uuid::new_v4());
            let destination_ref = format!("refs/heads/closure-recovery-{label}-{}", Uuid::new_v4());
            git(
                &self.repository,
                &["update-ref", &destination_ref, self.base_sha.as_str()],
            );
            git(
                &self.repository,
                &[
                    "update-ref",
                    &format!("refs/heads/{source_branch}"),
                    self.base_sha.as_str(),
                ],
            );
            let repository_identity = ClosureRepositoryIdentityV1::parse(format!(
                "git-common-dir:{}",
                self.repository
                    .join(".git")
                    .canonicalize()
                    .expect("git common dir")
                    .display()
            ))
            .expect("repository identity");
            let mut store = self.store.lock().await;
            let mut session = crate::store::tests::make_test_session();
            session.id = Uuid::new_v4();
            session.status = SessionStatus::Starting;
            session.working_dir = self.repository.clone();
            session.sandbox_kind = Some(SandboxKind::GitWorktree);
            session.sandbox_root = Some(worktree.clone());
            session.sandbox_branch = Some(source_branch.clone());
            session.sandbox_cleanup_state = Some(SandboxCleanupState::Live);
            let invocation_id = Uuid::new_v4();
            store
                .conn
                .execute(
                    "INSERT INTO model_invocations
                     (id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,
                      provider,model,trigger_source,session_id,policy_snapshot_json,
                      usage_confidence,created_at)
                     VALUES (?1,'session_launch_fresh','session_lifecycle','foreground',
                             'paid_capable','admitted','running','Claude','closure-test',
                             'closure_recovery_test',?2,'{}','unavailable',?3)",
                    rusqlite::params![
                        invocation_id.to_string(),
                        session.id.to_string(),
                        Utc::now().to_rfc3339()
                    ],
                )
                .expect("additional invocation");
            let custody_id = Uuid::new_v4();
            store
                .insert_direct_session_with_custody_and_invocation(
                    &session,
                    SessionCustodyBinding::New(NewCustodyRoot {
                        custody_id,
                        canonical_repo_dir: self.repository.display().to_string(),
                        sandbox_root: worktree.display().to_string(),
                        sandbox_branch: source_branch.clone(),
                        repository_identity: repository_identity.as_str().into(),
                        source_commit: self.base_sha.as_str().into(),
                        cause: CustodyCause::FreshLaunch,
                    }),
                    invocation_id,
                )
                .expect("additional session");
            store
                .update_session_status(session.id, SessionStatus::Completed)
                .expect("additional terminal session");
            store
                .conn
                .execute(
                    "UPDATE model_invocations SET status='completed',completed_at=?2 WHERE id=?1",
                    rusqlite::params![invocation_id.to_string(), Utc::now().to_rfc3339()],
                )
                .expect("additional terminal invocation");
            let base_ref = ClosureLocalBranchRefV1::parse(format!(
                "refs/heads/{}",
                git(&self.repository, &["symbolic-ref", "--short", "HEAD"])
            ))
            .expect("base ref");
            let created = store
                .create_closure_program(
                    &CreateClosureProgramRequestV1 {
                        config: ClosureProgramConfigV1 {
                            repository_root: self.repository.clone(),
                            base_ref,
                            destination_ref: ClosureLocalBranchRefV1::parse(
                                destination_ref.clone(),
                            )
                            .expect("destination ref"),
                            review_policy: ClosureReviewPolicyV1::NotRequired {
                                basis: ClosureReviewNotRequiredBasisV1::Tier0Deterministic,
                                rationale: "recovery fixture".into(),
                            },
                            verification_policy: ClosureVerificationPolicyV1 {
                                required_buckets: Vec::new(),
                                require_all_items_pass: true,
                            },
                            verifier_policy: ClosureVerifierPolicyV1 {
                                commands: Vec::new(),
                            },
                            checkout_remediation_policy: ClosureCheckoutRemediationPolicyV1::Refuse,
                        },
                        idempotency_key: Uuid::new_v4(),
                    },
                    &ResolvedClosureProgramV1 {
                        repository_root: self.repository.display().to_string(),
                        repository_identity: repository_identity.clone(),
                        base_sha: self.base_sha.clone(),
                        destination_pre_head: self.base_sha.clone(),
                    },
                )
                .expect("additional program");
            let source_id = ClosureSourceIdV1::new(Uuid::new_v4());
            let staging_ref = ClosureLocalBranchRefV1::parse(format!(
                "refs/heads/rsi/closure-stage/{}/{source_id}",
                created.program_id
            ))
            .expect("staging ref");
            git(
                &self.repository,
                &["update-ref", staging_ref.as_str(), self.base_sha.as_str()],
            );
            store
                .record_closure_source_launch(&PreparedClosureSourceV1 {
                    source: ClosureSourceIdentityV1 {
                        program_id: created.program_id,
                        source_id,
                        custody_id,
                        custody_generation: 1,
                        lineage_root_session_id: session.id,
                        repository_identity,
                        source_ref: ClosureLocalBranchRefV1::parse(format!(
                            "refs/heads/{source_branch}"
                        ))
                        .expect("source ref"),
                        source_base_sha: self.base_sha.clone(),
                        destination_ref: ClosureLocalBranchRefV1::parse(destination_ref)
                            .expect("destination ref"),
                        destination_pre_head: self.base_sha.clone(),
                        staging_ref,
                    },
                    root_session_id: session.id,
                    idempotency_key: Uuid::new_v4(),
                    request_fingerprint: program_run_fingerprint(
                        "closure.test.recovery",
                        label.as_bytes(),
                    ),
                })
                .expect("additional source");
            source_id
        }

        async fn counts(&self) -> (i64, i64, i64) {
            let store = self.store.lock().await;
            (
                store
                    .conn
                    .query_row("SELECT count(*) FROM closure_output_validations", [], |row| {
                        row.get(0)
                    })
                    .expect("validation count"),
                store
                    .conn
                    .query_row("SELECT count(*) FROM closure_integration_queue", [], |row| {
                        row.get(0)
                    })
                    .expect("queue count"),
                store
                    .conn
                    .query_row(
                        "SELECT count(*) FROM closure_events WHERE event_kind='terminal_output_captured'",
                        [],
                        |row| row.get(0),
                    )
                    .expect("event count"),
            )
        }
    }

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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn closure_missing_output_replay_and_terminal_crash_boundaries_converge_once() {
        let fixture = IngressFixture::new("missing-replay");
        let first =
            capture_or_replay_terminal_output(Arc::clone(&fixture.store), fixture.source_id)
                .await
                .expect("capture after simulated pre-claim crash")
                .expect("terminal candidate");
        assert_eq!(
            first.disposition,
            ClosureOutputValidationDispositionV1::MissingProviderOutput
        );
        assert_eq!(first.conversation_event_id, None);
        assert!(!first.replayed);

        let replay =
            capture_or_replay_terminal_output(Arc::clone(&fixture.store), fixture.source_id)
                .await
                .expect("capture after simulated post-commit crash")
                .expect("stored candidate");
        assert!(replay.replayed);
        assert_eq!(replay.validation_id, first.validation_id);
        assert_eq!(fixture.counts().await, (1, 0, 1));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn closure_valid_output_followed_by_stderr_selects_provider_and_race_converges() {
        let fixture = IngressFixture::new("provider-before-stderr");
        let provider_id = fixture
            .insert_provider_handoff(1, fixture.no_change_handoff())
            .await;
        fixture.insert_diagnostic(2, "late stderr").await;

        let live = capture_or_replay_terminal_output(Arc::clone(&fixture.store), fixture.source_id);
        let startup = reconcile_after_restart(
            Arc::clone(&fixture.store),
            ClosureOutputRecoveryBudgetV1::default(),
            None,
            None,
        );
        let (live, startup) = tokio::join!(live, startup);
        let live = live.expect("live capture").expect("terminal result");
        startup.expect("startup capture");
        assert_eq!(live.conversation_event_id, Some(provider_id));
        assert_eq!(
            live.disposition,
            ClosureOutputValidationDispositionV1::AcceptedNoChange
        );
        assert_eq!(fixture.counts().await, (1, 1, 1));
        let detail = fixture
            .store
            .lock()
            .await
            .get_closure_program_summary(&GetClosureProgramRequestV1 {
                program_id: None,
                source_id: Some(fixture.source_id),
            })
            .expect("inspect captured Closure source");
        assert_eq!(
            detail
                .output_validation
                .expect("inspect returns output validation")
                .validation_id,
            live.validation_id
        );
        assert!(
            detail.evidence.is_some(),
            "no-change evidence is inspectable"
        );
        assert!(detail.integration_queue_item_id.is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn closure_present_malformed_and_missing_outputs_replay_exactly_once() {
        for (label, content, expected, queued) in [
            (
                "present-replay",
                Some("VALID"),
                ClosureOutputValidationDispositionV1::AcceptedNoChange,
                1,
            ),
            (
                "malformed-replay",
                Some("not a Closure handoff"),
                ClosureOutputValidationDispositionV1::MalformedOutput,
                0,
            ),
            (
                "missing-replay-matrix",
                None,
                ClosureOutputValidationDispositionV1::MissingProviderOutput,
                0,
            ),
        ] {
            let fixture = IngressFixture::new(label);
            if let Some(content) = content {
                let content = if content == "VALID" {
                    fixture.no_change_handoff()
                } else {
                    content.to_string()
                };
                fixture.insert_provider_handoff(1, content).await;
            }
            let before_claim =
                capture_or_replay_terminal_output(Arc::clone(&fixture.store), fixture.source_id)
                    .await
                    .expect("capture before claim")
                    .expect("terminal source");
            assert_eq!(before_claim.disposition, expected, "{label}");
            assert!(!before_claim.replayed, "{label}");
            let after_atomic_commit =
                capture_or_replay_terminal_output(Arc::clone(&fixture.store), fixture.source_id)
                    .await
                    .expect("capture after atomic commit")
                    .expect("stored source");
            assert_eq!(
                after_atomic_commit.validation_id,
                before_claim.validation_id
            );
            assert!(after_atomic_commit.replayed, "{label}");
            assert_eq!(fixture.counts().await, (1, queued, 1), "{label}");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn closure_requires_terminal_invocation_and_never_infers_output_from_status() {
        let deferred = IngressFixture::new("running-invocation");
        deferred.set_invocation_status("running").await;
        deferred
            .insert_provider_handoff(1, deferred.no_change_handoff())
            .await;
        assert!(
            capture_or_replay_terminal_output(Arc::clone(&deferred.store), deferred.source_id)
                .await
                .expect("deferred capture")
                .is_none(),
            "Completed session with a running invocation must remain deferred"
        );
        assert_eq!(deferred.counts().await, (0, 0, 0));
        deferred.set_invocation_status("completed").await;
        assert_eq!(
            capture_or_replay_terminal_output(Arc::clone(&deferred.store), deferred.source_id)
                .await
                .expect("terminal capture")
                .expect("result")
                .disposition,
            ClosureOutputValidationDispositionV1::AcceptedNoChange
        );

        let missing = IngressFixture::new("status-not-outcome");
        assert_eq!(
            capture_or_replay_terminal_output(Arc::clone(&missing.store), missing.source_id)
                .await
                .expect("missing capture")
                .expect("result")
                .disposition,
            ClosureOutputValidationDispositionV1::MissingProviderOutput,
            "SessionStatus::Completed must not imply a Closure outcome"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn closure_committed_blocker_and_dirty_git_edges_are_bound_to_observation() {
        let committed = IngressFixture::new("committed");
        let committed_sha = committed.commit_source();
        committed
            .insert_provider_handoff(
                1,
                committed.handoff(ClosureChildOutcomeV1::Committed {
                    reported_source_head: committed_sha.clone(),
                }),
            )
            .await;
        let result =
            capture_or_replay_terminal_output(Arc::clone(&committed.store), committed.source_id)
                .await
                .expect("committed capture")
                .expect("result");
        assert_eq!(
            result.disposition,
            ClosureOutputValidationDispositionV1::AcceptedCommitted
        );
        assert_eq!(
            result
                .normalized_envelope
                .expect("normalized committed envelope")
                .outcome,
            ClosureChildOutcomeV1::Committed {
                reported_source_head: committed_sha,
            }
        );

        let blocker = IngressFixture::new("blocker");
        blocker
            .insert_provider_handoff(
                1,
                blocker.handoff(ClosureChildOutcomeV1::Blocker {
                    code: "operator_input_required".into(),
                    reason: "cannot safely proceed".into(),
                    requested_operator_action: "provide missing input".into(),
                }),
            )
            .await;
        assert_eq!(
            capture_or_replay_terminal_output(Arc::clone(&blocker.store), blocker.source_id)
                .await
                .expect("blocker capture")
                .expect("result")
                .disposition,
            ClosureOutputValidationDispositionV1::AcceptedBlocker
        );

        let dirty = IngressFixture::new("dirty");
        std::fs::write(dirty.worktree.join("untracked"), "dirty\n").expect("dirty source");
        dirty
            .insert_provider_handoff(1, dirty.no_change_handoff())
            .await;
        assert_eq!(
            capture_or_replay_terminal_output(Arc::clone(&dirty.store), dirty.source_id)
                .await
                .expect("dirty capture")
                .expect("result")
                .disposition,
            ClosureOutputValidationDispositionV1::BlockedOutcomeGitMismatch
        );
        assert_eq!(
            git(&dirty.repository, &["rev-parse", "HEAD"]),
            dirty.base_sha.as_str()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn closure_later_terminal_key_can_succeed_after_blocked_validation() {
        let fixture = IngressFixture::new("blocked-then-valid-rotation");
        let first =
            capture_or_replay_terminal_output(Arc::clone(&fixture.store), fixture.source_id)
                .await
                .expect("capture missing root output")
                .expect("blocked root validation");
        assert_eq!(
            first.disposition,
            ClosureOutputValidationDispositionV1::MissingProviderOutput
        );

        let (tip_session_id, invocation_id) = fixture.rotate().await;
        fixture
            .insert_provider_handoff_for(
                tip_session_id,
                invocation_id,
                1,
                fixture.handoff_for(
                    tip_session_id,
                    invocation_id,
                    1,
                    2,
                    ClosureChildOutcomeV1::NoChange {
                        observed_source_head: fixture.base_sha.clone(),
                        reason: "later genuine terminal invocation".into(),
                    },
                ),
            )
            .await;
        let later =
            capture_or_replay_terminal_output(Arc::clone(&fixture.store), fixture.source_id)
                .await
                .expect("capture later terminal key")
                .expect("accepted later validation");
        assert_eq!(
            later.disposition,
            ClosureOutputValidationDispositionV1::AcceptedNoChange
        );
        assert_ne!(later.key, first.key);
        assert_eq!(fixture.counts().await, (2, 1, 2));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn closure_rotation_advances_generation_and_binds_terminal_tip() {
        let fixture = IngressFixture::new("rotation");
        let (tip_session_id, invocation_id) = fixture.rotate().await;
        fixture
            .insert_provider_handoff_for(
                tip_session_id,
                invocation_id,
                1,
                fixture.handoff_for(
                    tip_session_id,
                    invocation_id,
                    1,
                    2,
                    ClosureChildOutcomeV1::NoChange {
                        observed_source_head: fixture.base_sha.clone(),
                        reason: "rotation preserved the sealed source".into(),
                    },
                ),
            )
            .await;
        let result =
            capture_or_replay_terminal_output(Arc::clone(&fixture.store), fixture.source_id)
                .await
                .expect("rotated capture")
                .expect("rotated result");
        assert_eq!(
            result.disposition,
            ClosureOutputValidationDispositionV1::AcceptedNoChange
        );
        assert_eq!(result.key.tip_session_id, tip_session_id);
        assert_eq!(result.key.model_invocation_id, invocation_id);
        assert_eq!(
            result
                .normalized_envelope
                .expect("rotated envelope")
                .correlation
                .custody_generation,
            2
        );
        let generations = fixture
            .store
            .lock()
            .await
            .conn
            .prepare(
                "SELECT custody_generation FROM closure_source_sessions
                 WHERE source_id=?1 ORDER BY rotation_depth",
            )
            .expect("lineage query")
            .query_map([fixture.source_id.to_string()], |row| row.get::<_, u64>(0))
            .expect("lineage rows")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("lineage generations");
        assert_eq!(generations, [1, 2]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn closure_restart_settles_single_unbound_rotation_as_ambiguous_lineage() {
        let fixture = IngressFixture::new("unbound-rotation");
        fixture.insert_unbound_successor().await;
        let recovered = reconcile_after_restart(
            Arc::clone(&fixture.store),
            ClosureOutputRecoveryBudgetV1::default(),
            None,
            None,
        )
        .await
        .expect("startup reconciliation settles unbound rotation");
        assert_eq!(
            (recovered.examined, recovered.committed, recovered.deferred),
            (1, 1, 0)
        );
        let blocked = fixture
            .store
            .lock()
            .await
            .get_closure_program_summary(&GetClosureProgramRequestV1 {
                program_id: None,
                source_id: Some(fixture.source_id),
            })
            .expect("inspect blocked source")
            .output_validation
            .expect("durable blocked validation");
        assert_eq!(
            blocked.disposition,
            ClosureOutputValidationDispositionV1::BlockedAmbiguousLineage
        );
        assert!(
            blocked
                .issues
                .iter()
                .any(|issue| issue.contains("unbound successor"))
        );
        assert_eq!(fixture.counts().await, (1, 0, 1));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn closure_startup_recovery_scans_bounded_pages_without_skips() {
        let fixture = IngressFixture::new("paged-recovery");
        fixture.add_missing_source("second").await;
        fixture.add_missing_source("third").await;
        let first = reconcile_after_restart(
            Arc::clone(&fixture.store),
            ClosureOutputRecoveryBudgetV1 {
                page_size: 1,
                max_pages: 2,
                time_budget_ms: 30_000,
            },
            None,
            None,
        )
        .await
        .expect("first bounded recovery pass");
        assert_eq!((first.examined, first.committed), (2, 2));
        assert!(first.next_cursor.is_some());
        assert!(first.high_water.is_some());
        let second = reconcile_after_restart(
            Arc::clone(&fixture.store),
            ClosureOutputRecoveryBudgetV1 {
                page_size: 1,
                max_pages: 2,
                time_budget_ms: 30_000,
            },
            first.next_cursor,
            first.high_water,
        )
        .await
        .expect("resumed bounded recovery pass");
        assert_eq!((second.examined, second.committed), (1, 1));
        assert!(second.next_cursor.is_none());
        assert!(second.high_water.is_none());
        assert_eq!(fixture.counts().await, (3, 0, 3));
    }
}
