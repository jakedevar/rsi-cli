//! Manager ledger service: filesystem observation is bounded and outside Store locks.
use super::agent_verbs::AgentControlHandle;
use crate::store::harness_manager_v2::refused;
use crate::{error::Result, store::manager_ledger::*};
use rsi_common::{
    agent_contract::parse_closure_review_handoff_v1, closure_kernel::*, harness_manager_v2::*,
};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::Path;
use uuid::Uuid;
pub(crate) mod git;

#[cfg(test)]
type ManagerUpdateTestPause = (
    tokio::sync::oneshot::Sender<()>,
    tokio::sync::oneshot::Receiver<()>,
);

#[cfg(test)]
fn manager_update_test_pauses()
-> &'static std::sync::Mutex<std::collections::HashMap<String, ManagerUpdateTestPause>> {
    static PAUSES: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, ManagerUpdateTestPause>>,
    > = std::sync::OnceLock::new();
    PAUSES.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

#[cfg(test)]
pub(crate) fn install_manager_update_test_pause(
    idempotency_key: &str,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
) {
    let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    manager_update_test_pauses()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(idempotency_key.to_owned(), (reached_tx, release_rx));
    (reached_rx, release_tx)
}

fn review_manifest_path(review_path: &str) -> Result<String> {
    const CLOSURE_REVIEW_PREFIX: &str = "thoughts/shared/reviews/closure/";
    let Some(closure_tail) = review_path.strip_prefix(CLOSURE_REVIEW_PREFIX) else {
        return review_path
            .strip_suffix(".json")
            .map(|stem| format!("{stem}.manifest.md"))
            .ok_or_else(|| refused("manager_v2_review_path"));
    };
    let mut parts = closure_tail.split('/');
    let (Some(program), Some(filename), None) = (parts.next(), parts.next(), parts.next()) else {
        return Err(refused("manager_v2_review_path"));
    };
    let source = filename
        .strip_suffix("-review-v1.json")
        .filter(|source| !source.is_empty())
        .ok_or_else(|| refused("manager_v2_review_path"))?;
    let manifest =
        format!("thoughts/shared/verification/closure/{program}/{source}-manifest-v2.md");
    git::artifact_path(&manifest)?;
    Ok(manifest)
}

impl AgentControlHandle {
    pub async fn agent_manager_inspect(
        &self,
        caller: Uuid,
        request: AgentManagerInspectRequestV2,
    ) -> Result<ManagerInspectionV2> {
        let store = self.store.lock().await;
        let mut result = store.manager_v2_inspect(caller, &request)?;
        if result.section != ManagerInspectSectionV2::Work {
            // These pages perform no filesystem work. Keep authorization,
            // observation and answer retrieval in one Store-lock interval;
            // a queued update belongs to the next snapshot, not a failed read.
            store.manager_v2_retrieve_decision_answers(caller, &mut result)?;
            return Ok(result);
        }
        drop(store);
        let before = inspection_fence(&result)?;
        let witnesses = self.manager_inspection_freshness(&mut result).await;
        let store = self.store.lock().await;
        let current = store.manager_v2_inspect(caller, &request)?;
        if inspection_fence(&current)? != before {
            return Err(refused("manager_v2_inspection_changed"));
        }
        check_witnesses(&store, &result, &witnesses)?;
        store.manager_v2_retrieve_decision_answers(caller, &mut result)?;
        Ok(result)
    }
    pub async fn get_harness_manager_state(
        &self,
        request: GetHarnessManagerStateRequestV2,
    ) -> Result<ManagerInspectionV2> {
        let store = self.store.lock().await;
        let mut result = store.manager_v2_inspect_operator(request.project_id, &request.query)?;
        if result.section != ManagerInspectSectionV2::Work {
            return Ok(result);
        }
        drop(store);
        let before = inspection_fence(&result)?;
        let witnesses = self.manager_inspection_freshness(&mut result).await;
        let store = self.store.lock().await;
        let current = store.manager_v2_inspect_operator(request.project_id, &request.query)?;
        if inspection_fence(&current)? != before {
            return Err(refused("manager_v2_inspection_changed"));
        }
        check_witnesses(&store, &result, &witnesses)?;
        Ok(result)
    }
    async fn manager_inspection_freshness(
        &self,
        result: &mut ManagerInspectionV2,
    ) -> Vec<(Uuid, Uuid, u64, Uuid)> {
        let mut witnesses = Vec::new();
        if result.section != ManagerInspectSectionV2::Work {
            return witnesses;
        }
        // At most four source worktrees and four repositories per page, with
        // two seconds per source and remote read. Unobserved freshness stays
        // explicitly unknown.
        let mut sources = std::collections::BTreeMap::<Uuid, Option<(String, String, bool)>>::new();
        let mut targets = std::collections::BTreeMap::<String, String>::new();
        let mut remotes = std::collections::BTreeMap::<String, Option<String>>::new();
        let mut remote_ancestry =
            std::collections::BTreeMap::<(String, String, String), Option<bool>>::new();
        for row in &mut result.rows {
            row["target_freshness"] = json!("unknown");
            row["local_checkout_freshness"] = json!("unknown");
            row["remote_delivery_freshness"] = json!("unknown");
            row["source_freshness"] = json!("unknown");
            row["source_tree_state"] = json!("unknown");
            let Some(session) = row["source_session_id"]
                .as_str()
                .and_then(|s| Uuid::parse_str(s).ok())
            else {
                continue;
            };
            let Some(epic) = row["epic_id"]
                .as_str()
                .and_then(|s| Uuid::parse_str(s).ok())
            else {
                continue;
            };
            let Some(policy) = &result.policy else {
                continue;
            };
            let custody = {
                let store = self.store.lock().await;
                let config = store.get_harness_manager(policy.project_id).ok().flatten();
                config
                    .filter(|config| {
                        config.row_version == result.scope_version
                            && store.manager_v2_descendant_epic(config, session).ok() == Some(epic)
                    })
                    .and_then(|_| store.live_custody_for_session(session).ok())
            };
            let Some(c) = custody else {
                continue;
            };
            if !sources.contains_key(&c.custody_id) && sources.len() < 4 {
                let observed = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                    git::custody(&c).await?;
                    let root = Path::new(&c.sandbox_root);
                    let source = git::head(root, "HEAD").await?;
                    let target = git::head(root, "refs/heads/rolling").await?;
                    let clean = git::read(root, &["status", "--porcelain=v1", "-z"])
                        .await?
                        .is_empty();
                    Ok::<_, crate::error::DaemonError>((source, target, clean))
                })
                .await
                .ok()
                .and_then(std::result::Result::ok);
                if let Some((_, target, _)) = &observed {
                    targets.insert(c.repository_identity.clone(), target.clone());
                }
                sources.insert(c.custody_id, observed);
            }
            if !remotes.contains_key(&c.repository_identity) && remotes.len() < 4 {
                let remote = tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    git::remote_head(Path::new(&c.sandbox_root)),
                )
                .await
                .ok()
                .and_then(std::result::Result::ok);
                remotes.insert(c.repository_identity.clone(), remote);
            }
            if let Some(Some((source, _, clean))) = sources.get(&c.custody_id) {
                witnesses.push((session, c.custody_id, c.generation, epic));
                row["source_freshness"] = json!(if row["source_commit"] == *source {
                    "current"
                } else {
                    "head_advanced"
                });
                row["observed_source_commit"] = json!(source);
                row["source_tree_state"] = json!(if *clean { "clean" } else { "pending_changes" });
            } else {
                row["freshness_reason"] = json!("source_observation_unavailable_or_budget");
            }
            if let Some(head) = targets.get(&c.repository_identity) {
                witnesses.push((session, c.custody_id, c.generation, epic));
                let recorded = row
                    .pointer("/integration/target_commit")
                    .and_then(Value::as_str);
                row["target_freshness"] = json!(match recorded {
                    Some(recorded) if recorded == head => "current",
                    Some(_) => "target_advanced",
                    None => "not_integrated",
                });
                row["observed_target_commit"] = json!(head);
                row["local_checkout_freshness"] = row["target_freshness"].clone();
                row["observed_local_checkout_commit"] = json!(head);
            }
            if let Some(Some(head)) = remotes.get(&c.repository_identity) {
                let recorded = row
                    .pointer("/integration/target_commit")
                    .and_then(Value::as_str);
                if let Some(recorded) = recorded.filter(|recorded| *recorded != head.as_str()) {
                    let key = (
                        c.repository_identity.clone(),
                        recorded.to_owned(),
                        head.clone(),
                    );
                    if !remote_ancestry.contains_key(&key) && remote_ancestry.len() < 4 {
                        let root = Path::new(&c.sandbox_root);
                        let ancestry =
                            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                                git::head(root, head).await?;
                                git::ancestor(root, recorded, head).await
                            })
                            .await
                            .ok()
                            .and_then(std::result::Result::ok);
                        remote_ancestry.insert(key, ancestry);
                    }
                }
                row["remote_delivery_freshness"] = json!(match recorded {
                    Some(recorded) if recorded == head => "current",
                    Some(recorded) => {
                        let key = (
                            c.repository_identity.clone(),
                            recorded.to_owned(),
                            head.clone(),
                        );
                        match remote_ancestry.get(&key).copied().flatten() {
                            Some(true) => "target_advanced",
                            Some(false) => "target_rewound_or_diverged",
                            None => "target_changed_unknown_ancestry",
                        }
                    }
                    None => "not_integrated",
                });
                row["observed_remote_target_commit"] = json!(head);
            }
            let evidence_head = remotes
                .get(&c.repository_identity)
                .and_then(Option::as_deref)
                .or_else(|| targets.get(&c.repository_identity).map(String::as_str));
            if let Some(head) = evidence_head {
                row["integration_evidence_policy_digest"] =
                    if row.pointer("/review/mode").and_then(Value::as_str) == Some("db_native") {
                        Value::Null
                    } else {
                        json!(integration_policy_digest_projection(row, head).ok())
                    };
            }
        }
        witnesses
    }
    pub async fn agent_manager_update(
        &self,
        caller: Uuid,
        request: AgentManagerUpdateRequestV2,
    ) -> Result<ManagerMutationReceiptV2> {
        let context = {
            let store = self.store.lock().await;
            let context = store.manager_v2_prepare_update(caller, &request)?;
            let payload = json!({"actor":caller,"request":request});
            if let Some(receipt) = store.manager_v2_replay(
                &context.authority.config,
                &request.idempotency_key,
                &payload,
            )? {
                return Ok(serde_json::from_value(receipt)?);
            }
            context
        };
        let observed = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            self.observe_manager_update(&context, &request),
        )
        .await
        .map_err(|_| refused("manager_v2_observation_budget"))??;
        #[cfg(test)]
        if let Some((reached_tx, release_rx)) = {
            let pause = manager_update_test_pauses()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&request.idempotency_key);
            pause
        } {
            let _ = reached_tx.send(());
            let _ = release_rx.await;
        }
        let store = self.store.lock().await;
        let latest = store.manager_v2_prepare_update(caller, &request)?;
        if manager_update_work_snapshot_changed(
            &request.change,
            context.work_version,
            latest.work_version,
        ) || latest.authority.config.manager_session_id
            != context.authority.config.manager_session_id
        {
            return Err(refused("manager_v2_record_changed"));
        }
        let receipt = store.manager_v2_commit_update(caller, &request, &observed)?;
        if matches!(request.change, ManagerUpdateV2::RequestReview { .. })
            && let Ok(assignment_id) = Uuid::parse_str(&receipt.key)
            && let Err(error) = store.allocate_manager_review_assignment(assignment_id)
        {
            // The durable reserved row is the restart-safe retry owner. A
            // transient capacity/source race must not create a second action.
            tracing::warn!(%assignment_id,%error,"manager review allocation deferred");
        }
        Ok(receipt)
    }
    async fn observe_manager_update(
        &self,
        context: &LedgerObservationContext,
        request: &AgentManagerUpdateRequestV2,
    ) -> Result<LedgerObservation> {
        let (evidence, integration) = match &request.change {
            ManagerUpdateV2::Stage {
                evidence: Some(e), ..
            } => (Some(e), None),
            ManagerUpdateV2::Integration {
                source_commit,
                target_commit,
                verification,
                ..
            } => (verification.as_ref(), Some((source_commit, target_commit))),
            _ => (None, None),
        };
        if let ManagerUpdateV2::RequestReview { source_commit, .. } = &request.change {
            let source = context
                .source_session
                .as_ref()
                .ok_or_else(|| refused("manager_review_author_missing"))?;
            let custody = self
                .store
                .lock()
                .await
                .live_custody_for_session(source.id)?;
            let root = Path::new(&custody.sandbox_root);
            git::custody(&custody).await?;
            let head = git::head(root, "HEAD").await?;
            git::sealed_source_holds(root, source_commit, &head).await?;
            // The custody base belongs to the holder's inherited history.
            // The sealed commit must contain work authored after that base.
            if git::ancestor(root, source_commit, &custody.source_commit).await? {
                return Err(refused("manager_review_source_not_authored"));
            }
            return Ok(LedgerObservation {
                source_commit: Some(source_commit.clone()),
                custody: vec![(source.id, custody.custody_id, custody.generation)],
                ..Default::default()
            });
        }
        if evidence.is_none()
            && integration.is_none()
            && !matches!(request.change, ManagerUpdateV2::Migration { .. })
        {
            return Ok(LedgerObservation::default());
        }
        let source = context
            .source_session
            .as_ref()
            .ok_or_else(|| refused("manager_v2_source_custody_required"))?;
        if source.status == rsi_common::types::SessionStatus::Archived
            && let Some((original, target)) = integration
            && evidence.is_none()
        {
            let work = context
                .work
                .as_ref()
                .ok_or_else(|| refused("manager_v2_work_missing"))?;
            let (repository_identity, accepted_base) = {
                let store = self.store.lock().await;
                if !store.manager_review_enrolled(&context.authority.config, work, original)?
                    || store
                        .manager_v2_accepted_source(&context.authority.config, work, original)?
                        .is_none()
                {
                    return Err(refused("manager_v2_source_acceptance_required"));
                }
                let repository: Option<(String, String, String)> = store
                    .conn
                    .query_row(
                        "SELECT r.canonical_repo_dir,r.repository_identity,r.source_commit FROM sandbox_custody_roots r JOIN sessions s ON s.sandbox_custody_id=r.custody_id WHERE s.id=?1",
                        [source.id.to_string()],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .optional()?;
                drop(store);
                let (canonical_repo_dir, identity, base) =
                    repository.ok_or_else(|| refused("manager_v2_source_custody_required"))?;
                if Path::new(&canonical_repo_dir) != source.working_dir {
                    return Err(refused("manager_v2_custody_repository_changed"));
                }
                (identity, base)
            };
            let repository = repository_identity
                .strip_prefix("git-common-dir:")
                .unwrap_or(&repository_identity);
            let root = Path::new(repository);
            git::repository(root).await?;
            let remote_url = verify_integration_target(root, original, target).await?;
            git::accepted_content(root, &accepted_base, original, target).await?;
            git::repository(root).await?;
            integration_target_unchanged(root, original, target, remote_url.as_deref()).await?;
            return Ok(LedgerObservation {
                source_commit: Some(original.clone()),
                target_commit: Some(target.clone()),
                ..Default::default()
            });
        }
        let custody = self
            .store
            .lock()
            .await
            .live_custody_for_session(source.id)?;
        let root = Path::new(&custody.sandbox_root);
        git::custody(&custody).await?;
        let mut observation = LedgerObservation {
            custody: vec![(source.id, custody.custody_id, custody.generation)],
            ..Default::default()
        };
        let mut remote_url = None;
        if let ManagerUpdateV2::Migration {
            baseline_commit,
            inventory_digest,
            ..
        } = &request.change
        {
            git::migration(root, baseline_commit, inventory_digest).await?;
            observation.migration = Some((baseline_commit.clone(), inventory_digest.clone()));
            return Ok(observation);
        }
        if let Some((original, target)) = integration {
            let work = context
                .work
                .as_ref()
                .ok_or_else(|| refused("manager_v2_work_missing"))?;
            let db_review = self.store.lock().await.manager_review_enrolled(
                &context.authority.config,
                work,
                original,
            )?;
            if db_review {
                if evidence.is_some() {
                    return Err(refused("manager_review_legacy_evidence_forbidden"));
                }
                if !canonical_sha(target)
                    || self
                        .store
                        .lock()
                        .await
                        .manager_v2_accepted_source(&context.authority.config, work, original)?
                        .is_none()
                {
                    return Err(refused("manager_v2_source_acceptance_required"));
                }
                remote_url = verify_integration_target(root, original, target).await?;
                git::accepted_content(root, &custody.source_commit, original, target).await?;
                git::custody(&custody).await?;
                integration_target_unchanged(root, original, target, remote_url.as_deref()).await?;
                observation.source_commit = Some(original.clone());
                observation.target_commit = Some(target.clone());
                return Ok(observation);
            }
        }
        let evidence =
            evidence.ok_or_else(|| refused("manager_v2_combined_verification_unavailable"))?;
        if !canonical_sha(&evidence.source_commit) || !canonical_sha(&evidence.artifact_commit) {
            return Err(refused("manager_v2_invalid_commit"));
        }
        let current = git::head(root, "HEAD").await?;
        if integration.is_none() && current != evidence.source_commit {
            return Err(refused("manager_v2_source_changed"));
        }
        if integration.is_none() {
            git::clean(root).await?;
        }
        let source = integration.map_or(current.as_str(), |(source, _)| source.as_str());
        observation.source_commit = Some(source.into());
        let mut work = context
            .work
            .clone()
            .ok_or_else(|| refused("manager_v2_work_missing"))?;
        work.source_commit = Some(source.into());
        if integration.is_none() {
            work.source_session_id = Some(evidence.source_session_id);
        }
        if let Some((original, target)) = integration {
            let accepted = self.store.lock().await.manager_v2_accepted_source(
                &context.authority.config,
                &work,
                original,
            )?;
            if !canonical_sha(target) || accepted.is_none() || evidence.source_commit != *target {
                return Err(refused("manager_v2_source_acceptance_required"));
            }
            remote_url = verify_integration_target(root, original, target).await?;
            git::accepted_content(root, &custody.source_commit, original, target).await?;
            observation.target_commit = Some(target.clone());
        }
        let verified_head = &evidence.source_commit;
        let expected_policy = if integration.is_some() {
            integration_policy_digest(&work, verified_head)?
        } else {
            policy_digest(&work, verified_head)?
        };
        if let Some(id) = evidence.closure_evidence_id {
            let raw:Option<(String,String,String,Option<String>,Option<Vec<u8>>,Option<Vec<u8>>)>=self.store.lock().await.conn.query_row("SELECT reviewed_source_head,disposition,review_policy_digest,manifest_digest,review_raw_bytes,manifest_raw_bytes FROM closure_evidence WHERE id=?1 AND evidence_commit_sha=?2 AND review_json_path=?3",params![id.to_string(),evidence.artifact_commit,evidence.artifact_path],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).optional()?;
            if let Some((
                head,
                disposition,
                policy,
                Some(digest),
                Some(review_bytes),
                Some(manifest_bytes),
            )) = raw
            {
                if head == *verified_head && disposition == "required_independent" {
                    let review_text = std::str::from_utf8(&review_bytes)
                        .map_err(|_| refused("manager_v2_artifact_encoding"))?;
                    let manifest_text = std::str::from_utf8(&manifest_bytes)
                        .map_err(|_| refused("manager_v2_artifact_encoding"))?;
                    let review = parse_closure_review_artifact_v1(review_text)
                        .map_err(|_| refused("manager_v2_closure_evidence_invalid"))?;
                    if self.store.lock().await.manager_v2_descendant_epic(
                        &context.authority.config,
                        review.reviewer.session_id,
                    )? != work.epic_id
                    {
                        return Err(refused("manager_v2_evidence_out_of_scope"));
                    }
                    let bundle = validate_closure_evidence_bundle_v1(
                        review_text,
                        manifest_text,
                        &ClosureEvidenceExpectationV1 {
                            source_head: ClosureGitShaV1::parse(&head)
                                .map_err(|_| refused("manager_v2_invalid_commit"))?,
                            reviewer_session_id: review.reviewer.session_id,
                            model_invocation_id: review.reviewer.model_invocation_id,
                            review_policy_digest: rsi_common::types::Sha256Digest::parse(&policy)
                                .map_err(|_| {
                                refused("manager_v2_invalid_digest")
                            })?,
                        },
                    )
                    .map_err(|_| refused("manager_v2_closure_evidence_invalid"))?;
                    validate_work_bundle(&work, integration.is_some(), &bundle)?;
                    observation.evidence = Some(EvidenceAdmission {
                        source_commit: head,
                        artifact_commit: evidence.artifact_commit.clone(),
                        artifact_digest: digest,
                        policy_digest: expected_policy,
                        reviewer_session_id: Some(review.reviewer.session_id),
                        reviewer_invocation_id: None,
                        review_handoff_event_id: None,
                        closure_evidence_id: Some(id),
                        observed_at: chrono::Utc::now()
                            .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
                    });
                }
            }
        } else if !evidence.artifact_path.is_empty() {
            // A report without a canonical independent reviewer remains visible unknown.
            observation.evidence = self
                .observe_independent_evidence(
                    context,
                    &work,
                    evidence,
                    &custody,
                    &mut observation.custody,
                    integration.is_some(),
                )
                .await?;
        }
        if integration.is_some() && observation.evidence.is_none() {
            return Err(refused("manager_v2_combined_verification_unavailable"));
        }
        // Repeat mutable Git observations after bounded artifact reads.
        git::custody(&custody).await?;
        if integration.is_none() {
            git::clean(root).await?;
            if git::head(root, "HEAD").await? != current {
                return Err(refused("manager_v2_source_changed"));
            }
        }
        if let Some((source, target)) = integration {
            integration_target_unchanged(root, source, target, remote_url.as_deref()).await?;
        }
        Ok(observation)
    }
    async fn observe_independent_evidence(
        &self,
        context: &LedgerObservationContext,
        work: &WorkRecord,
        e: &ManagerEvidenceV2,
        source_custody: &crate::store::sandbox_custody::PersistedCustody,
        witnesses: &mut Vec<(Uuid, Uuid, u64)>,
        integration: bool,
    ) -> Result<Option<EvidenceAdmission>> {
        git::artifact_path(&e.artifact_path)?;
        let root = Path::new(&source_custody.sandbox_root);
        let bytes = git::blob(root, &e.artifact_commit, &e.artifact_path).await?;
        let raw =
            std::str::from_utf8(&bytes).map_err(|_| refused("manager_v2_artifact_encoding"))?;
        let review = match parse_closure_review_artifact_v1(raw) {
            Ok(r) => r,
            Err(_) => return Ok(None),
        };
        let expected_policy = if integration {
            integration_policy_digest(work, &e.source_commit)?
        } else {
            policy_digest(work, &e.source_commit)?
        };
        if review.reviewed_source_head.as_str() != e.source_commit
            || review.scope_policy_audit.review_policy_digest.as_str() != expected_policy
            || !review
                .scope_policy_audit
                .reviewed_scope
                .contains(&scope_label(work))
        {
            return Err(refused("manager_v2_review_work_correlation"));
        }
        let manifest_path = review_manifest_path(&e.artifact_path)?;
        let (reviewer, custody, handoff, handoff_event) = {
            let store = self.store.lock().await;
            let reviewer = store
                .get_session(review.reviewer.session_id)?
                .ok_or_else(|| refused("manager_v2_reviewer_unavailable"))?;
            if reviewer.id == e.source_session_id
                || Some(reviewer.id) == context.work.as_ref().and_then(|w| w.source_session_id)
                || reviewer.id == context.authority.caller
                || reviewer.id
                    == context
                        .authority
                        .config
                        .current_session_id
                        .unwrap_or(Uuid::nil())
                || store.manager_v2_descendant_epic(&context.authority.config, reviewer.id)?
                    != work.epic_id
                || reviewer.status != rsi_common::types::SessionStatus::Completed
            {
                return Err(refused("manager_v2_independent_reviewer_required"));
            }
            let custody = store.live_custody_for_session(reviewer.id)?;
            if custody.custody_id == source_custody.custody_id
                || custody.repository_identity != source_custody.repository_identity
                || custody.source_commit != e.source_commit
            {
                return Err(refused("manager_v2_independent_custody_required"));
            }
            let invocation_ok:bool=store.conn.query_row("SELECT EXISTS(SELECT 1 FROM model_invocations WHERE id=?1 AND session_id=?2 AND status='completed' AND EXISTS(SELECT 1 FROM sessions WHERE id=?2 AND model_invocation_id=?1))",params![review.reviewer.model_invocation_id.to_string(),reviewer.id.to_string()],|r|r.get(0))?;
            if !invocation_ok {
                return Err(refused("manager_v2_reviewer_invocation_unfinished"));
            }
            let handoff:Option<(String,i64)>=store.conn.query_row("SELECT e.content,e.id FROM conversation_events e JOIN conversation_event_provenance p ON p.conversation_event_id=e.id WHERE e.session_id=?1 AND p.model_invocation_id=?2 AND p.producer_kind='provider_assistant_output' AND e.role='Assistant' AND e.event_type='Message' AND trim(e.content)<>'' ORDER BY e.sequence DESC,e.id DESC LIMIT 1",params![reviewer.id.to_string(),review.reviewer.model_invocation_id.to_string()],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
            let (handoff, event) =
                handoff.ok_or_else(|| refused("manager_v2_review_handoff_required"))?;
            (reviewer, custody, handoff, event)
        };
        let handoff = parse_closure_review_handoff_v1(&handoff)
            .map_err(|_| refused("manager_v2_review_handoff_invalid"))?;
        if handoff.reviewer_session_id != reviewer.id
            || handoff.reviewer_model_invocation_id != review.reviewer.model_invocation_id
            || handoff.review_json_path.as_str() != e.artifact_path
            || handoff.manifest_v2_path.as_str() != manifest_path
            || handoff.evidence_commit_sha.as_str() != e.artifact_commit
            || handoff.sealed_source_sha.as_str() != e.source_commit
        {
            return Err(refused("manager_v2_review_handoff_correlation"));
        }
        let reviewer_root = Path::new(&custody.sandbox_root);
        git::custody(&custody).await?;
        git::clean(reviewer_root).await?;
        if git::head(reviewer_root, "HEAD").await? != e.artifact_commit {
            return Err(refused("manager_v2_reviewer_head_changed"));
        }
        let parents = git::read(
            root,
            &["rev-list", "--parents", "-n", "1", &e.artifact_commit],
        )
        .await?;
        let parents = String::from_utf8_lossy(&parents);
        let fields: Vec<_> = parents.split_whitespace().collect();
        if fields.len() != 2 || fields[1] != e.source_commit {
            return Err(refused("manager_v2_evidence_parent"));
        }
        let changed = git::read(
            root,
            &[
                "diff-tree",
                "--no-commit-id",
                "--name-only",
                "-r",
                &e.artifact_commit,
            ],
        )
        .await?;
        let mut changed = String::from_utf8_lossy(&changed)
            .lines()
            .map(str::to_string)
            .collect::<Vec<_>>();
        changed.sort();
        let mut expected = vec![e.artifact_path.clone(), manifest_path.clone()];
        expected.sort();
        if changed != expected {
            return Err(refused("manager_v2_evidence_files"));
        }
        let manifest = git::blob(root, &e.artifact_commit, &manifest_path).await?;
        let manifest =
            std::str::from_utf8(&manifest).map_err(|_| refused("manager_v2_artifact_encoding"))?;
        let bundle = validate_closure_evidence_bundle_v1(
            raw,
            manifest,
            &ClosureEvidenceExpectationV1 {
                source_head: ClosureGitShaV1::parse(&e.source_commit)
                    .map_err(|_| refused("manager_v2_invalid_commit"))?,
                reviewer_session_id: reviewer.id,
                model_invocation_id: review.reviewer.model_invocation_id,
                review_policy_digest: rsi_common::types::Sha256Digest::parse(&expected_policy)
                    .map_err(|_| refused("manager_v2_invalid_digest"))?,
            },
        )
        .map_err(|_| refused("manager_v2_review_bundle_invalid"))?;
        validate_work_bundle(work, integration, &bundle)?;
        witnesses.push((reviewer.id, custody.custody_id, custody.generation));
        Ok(Some(EvidenceAdmission {
            source_commit: e.source_commit.clone(),
            artifact_commit: e.artifact_commit.clone(),
            artifact_digest: format!("sha256:{:x}", Sha256::digest(&bytes)),
            policy_digest: expected_policy,
            reviewer_session_id: Some(reviewer.id),
            reviewer_invocation_id: Some(review.reviewer.model_invocation_id),
            review_handoff_event_id: Some(handoff_event),
            closure_evidence_id: None,
            observed_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
        }))
    }
}

async fn verify_integration_target(
    root: &Path,
    source: &str,
    target: &str,
) -> Result<Option<String>> {
    if git::configured_origin(root).await?.is_some() {
        return git::remote_target(root, source, target).await.map(Some);
    }
    if !git::local_only_policy(root).await? {
        return Err(refused("manager_v2_remote_missing"));
    }
    if git::head(root, "refs/heads/rolling").await? != target
        || !git::ancestor(root, source, target).await?
    {
        return Err(refused("manager_v2_target_or_ancestry_mismatch"));
    }
    Ok(None)
}

async fn integration_target_unchanged(
    root: &Path,
    source: &str,
    target: &str,
    url: Option<&str>,
) -> Result<()> {
    if let Some(url) = url {
        git::remote_target_unchanged(root, url, source, target).await
    } else {
        if git::configured_origin(root).await?.is_some() || !git::local_only_policy(root).await? {
            return Err(refused("manager_v2_local_only_policy_changed"));
        }
        if git::head(root, "refs/heads/rolling").await? != target {
            return Err(refused("manager_v2_target_changed"));
        }
        Ok(())
    }
}

/// Source-bound updates must commit against the work row observed before
/// filesystem custody checks. Other updates have their own record CAS and
/// authority checks, so an unrelated work-row advance must not reject them.
const fn requires_work_version_match(change: &ManagerUpdateV2) -> bool {
    matches!(
        change,
        ManagerUpdateV2::Stage {
            evidence: Some(_),
            ..
        } | ManagerUpdateV2::Integration { .. }
            | ManagerUpdateV2::Migration { .. }
            | ManagerUpdateV2::RequestReview { .. }
    )
}

const fn manager_update_work_snapshot_changed(
    change: &ManagerUpdateV2,
    observed_version: i64,
    latest_version: i64,
) -> bool {
    requires_work_version_match(change) && observed_version != latest_version
}

fn validate_work_bundle(
    work: &WorkRecord,
    integration: bool,
    bundle: &ValidatedClosureEvidenceBundleV1,
) -> Result<()> {
    use rsi_common::verification_manifest::{ItemStatus, ManifestStatus};
    if bundle.review.verdict != ClosureReviewVerdictV1::Accepted
        || !bundle.review.unresolved_finding_ids.is_empty()
        || !bundle
            .review
            .scope_policy_audit
            .reviewed_scope
            .contains(&scope_label(work))
        || bundle.manifest.frontmatter.status != ManifestStatus::Verified
        || bundle.manifest.phases.iter().flat_map(|p| &p.items).count() == 0
        || bundle
            .manifest
            .phases
            .iter()
            .flat_map(|p| &p.items)
            .any(|i| !matches!(i.status, Some(ItemStatus::Pass | ItemStatus::Checked)))
    {
        return Err(refused("manager_v2_evidence_not_passed"));
    }
    let covered = bundle.manifest.covered_linkage_keys();
    let mut gates = work.required_gates.clone();
    if integration {
        gates.push(ManagerWorkStageV2::Integration);
    }
    for stage in gates {
        if stage == ManagerWorkStageV2::Integration && !integration {
            continue;
        }
        let name = serde_json::to_value(stage)?
            .as_str()
            .unwrap_or("unknown")
            .to_string();
        if !covered.contains(&format!("{}:{name}", scope_label(work))) {
            return Err(refused("manager_v2_required_gate_uncovered"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod remote_tests;
#[cfg(test)]
mod tests;

fn inspection_fence(result: &ManagerInspectionV2) -> Result<Value> {
    Ok(
        json!({"scope":result.scope_version,"policy":result.policy,"rows":result.rows,
        "cursor":result.next_cursor,"complete":result.complete}),
    )
}
fn check_witnesses(
    store: &crate::store::Store,
    result: &ManagerInspectionV2,
    witnesses: &[(Uuid, Uuid, u64, Uuid)],
) -> Result<()> {
    if witnesses.is_empty() {
        return Ok(());
    }
    let policy = result
        .policy
        .as_ref()
        .ok_or_else(|| refused("manager_v2_scope_changed"))?;
    let config = store
        .get_harness_manager(policy.project_id)?
        .ok_or_else(|| refused("manager_v2_scope_changed"))?;
    for (session, custody, generation, epic) in witnesses {
        if store.manager_v2_descendant_epic(&config, *session)? != *epic {
            return Err(refused("manager_v2_evidence_out_of_scope"));
        }
        let current = store.live_custody_for_session(*session)?;
        if current.custody_id != *custody || current.generation != *generation {
            return Err(refused("manager_v2_evidence_custody_changed"));
        }
    }
    Ok(())
}
