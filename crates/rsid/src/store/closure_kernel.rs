//! V84 Closure Kernel store transactions.
//!
//! Git and artifact observations are made outside the SQLite lock. These
//! methods revalidate every durable identity under `BEGIN IMMEDIATE` and
//! commit state/event/queue effects together.

use super::Store;
use crate::error::{DaemonError, Result};
use crate::terminal_output::select_last_terminal_output;
use chrono::{DateTime, SecondsFormat, Utc};
use rsi_common::closure_kernel::*;
use rsi_common::program_runs::{canonical_program_run_json, program_run_fingerprint};
use rsi_common::types::{SessionProvider, Sha256Digest};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub(crate) struct ResolvedClosureProgramV1 {
    pub repository_root: String,
    pub repository_identity: ClosureRepositoryIdentityV1,
    pub base_sha: ClosureGitShaV1,
    pub destination_pre_head: ClosureGitShaV1,
}

#[derive(Debug, Clone)]
pub(crate) struct PreparedClosureSourceV1 {
    pub source: ClosureSourceIdentityV1,
    pub root_session_id: Uuid,
    pub idempotency_key: Uuid,
    pub request_fingerprint: String,
}

#[derive(Debug, Clone)]
pub(crate) struct ClosureSourceLaunchReservationV1 {
    pub source: ClosureSourceIdentityV1,
    pub root_session_id: Uuid,
    pub branch_name: String,
    pub idempotency_key: Uuid,
    pub request_fingerprint: String,
}

#[derive(Debug, Clone)]
pub(crate) enum ClosureSourceLaunchClaimV1 {
    Reserved(ClosureSourceLaunchReservationV1),
    Result(LaunchClosureSourceResultV1),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ClosureTerminalCaptureCandidateV1 {
    pub source: ClosureSourceIdentityV1,
    pub repository_root: String,
    pub source_worktree_root: String,
    pub source_created_at: DateTime<Utc>,
    pub tip_session_id: Uuid,
    pub rotation_depth: u32,
    pub model_invocation_id: Uuid,
    pub terminal_status: String,
    pub lineage_issue: Option<String>,
    pub event_id: Option<i64>,
    pub event_sequence: Option<i32>,
    pub provider_event_type: Option<String>,
    pub raw_handoff: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct ClosureOutputCaptureProposalV1 {
    pub key: ClosureOutputIngestionKeyV1,
    pub expected_custody_id: Uuid,
    pub expected_custody_generation: u64,
    pub expected_rotation_depth: u32,
    pub terminal_status: String,
    pub event_id: Option<i64>,
    pub event_sequence: Option<i32>,
    pub provider_event_type: Option<String>,
    pub raw_handoff: Option<String>,
    pub raw_handoff_digest: Option<String>,
    pub normalized_envelope: Option<ClosureChildOutputEnvelopeV1>,
    pub normalized_envelope_digest: Option<String>,
    pub disposition: ClosureOutputValidationDispositionV1,
    pub issues: Vec<String>,
    pub source_state: ClosureSourceStateV1,
    pub sealed_source_head: Option<ClosureGitShaV1>,
    pub observed_source_ref_head: Option<ClosureGitShaV1>,
    pub observed_worktree_head: Option<ClosureGitShaV1>,
    pub observed_worktree_clean: Option<bool>,
}

#[derive(Debug, Clone)]
pub(crate) struct PreparedClosureEvidenceV1 {
    pub request: RecordClosureEvidenceRequestV1,
    pub evidence: AcceptedReviewEvidenceV1,
    pub review_raw_bytes: Option<Vec<u8>>,
    pub normalized_review_json: Option<String>,
    pub manifest_raw_bytes: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub(crate) struct ClosureEvidenceAdmissionContextV1 {
    pub program_id: ClosureProgramIdV1,
    pub source_state: ClosureSourceStateV1,
    pub sealed_source_head: ClosureGitShaV1,
    pub source_ref: ClosureLocalBranchRefV1,
    pub source_worktree_root: String,
    pub repository_root: String,
    pub review_policy: ClosureReviewPolicyV1,
    pub verification_policy: ClosureVerificationPolicyV1,
    pub review_policy_digest: Sha256Digest,
    pub verification_policy_digest: Sha256Digest,
    pub verifier_policy_digest: Sha256Digest,
    pub reviewer_status: String,
    pub reviewer_provider: SessionProvider,
    pub reviewer_model: Option<String>,
    pub reviewer_model_invocation_id: Uuid,
    pub reviewer_custody_id: Uuid,
    pub reviewer_custody_generation: u64,
    pub reviewer_worktree_root: String,
    pub reviewer_branch: String,
    pub review_handoff_event_id: i64,
    pub review_handoff_raw: String,
}

fn timestamp(now: DateTime<Utc>) -> String {
    now.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn digest_json<T: Serialize>(domain: &str, value: &T) -> Result<Sha256Digest> {
    let canonical = canonical_program_run_json(value).map_err(DaemonError::InvalidParam)?;
    Sha256Digest::parse(program_run_fingerprint(domain, canonical.as_bytes()))
        .map_err(DaemonError::InvalidParam)
}

fn parse_json<T: serde::de::DeserializeOwned>(raw: &str, label: &str) -> Result<T> {
    serde_json::from_str(raw)
        .map_err(|error| DaemonError::Store(format!("invalid stored {label}: {error}")))
}

impl Store {
    pub(crate) fn replay_create_closure_program(
        &self,
        request: &CreateClosureProgramRequestV1,
    ) -> Result<Option<CreateClosureProgramResultV1>> {
        let fingerprint = closure_request_fingerprint("CreateClosureProgram", request)
            .map_err(DaemonError::InvalidParam)?;
        let row = self
            .conn
            .query_row(
                "SELECT request_fingerprint,result_json FROM closure_operator_requests
                 WHERE method='CreateClosureProgram' AND idempotency_key=?1",
                [request.idempotency_key.to_string()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        row.map(|(stored, raw)| {
            let mut result: CreateClosureProgramResultV1 =
                parse_json(&raw, "CreateClosureProgram result")?;
            result.replayed = true;
            if stored != fingerprint {
                result.refusal = Some(ClosureRefusalCodeV1::IdempotencyMismatch);
            }
            Ok(result)
        })
        .transpose()
    }

    pub(crate) fn replay_update_closure_program(
        &self,
        request: &UpdateClosureProgramRequestV1,
    ) -> Result<Option<ClosureProgramMutationResultV1>> {
        let fingerprint = closure_request_fingerprint("UpdateClosureProgram", request)
            .map_err(DaemonError::InvalidParam)?;
        let row = self
            .conn
            .query_row(
                "SELECT request_fingerprint,result_json FROM closure_operator_requests
                 WHERE method='UpdateClosureProgram' AND idempotency_key=?1",
                [request.idempotency_key.to_string()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        row.map(|(stored, raw)| {
            let mut result: ClosureProgramMutationResultV1 =
                parse_json(&raw, "UpdateClosureProgram result")?;
            result.replayed = true;
            if stored != fingerprint {
                result.refusal = Some(ClosureRefusalCodeV1::IdempotencyMismatch);
            }
            Ok(result)
        })
        .transpose()
    }

    pub(crate) fn replay_launch_closure_source(
        &self,
        request: &LaunchClosureSourceRequestV1,
    ) -> Result<Option<LaunchClosureSourceResultV1>> {
        let fingerprint = closure_request_fingerprint("LaunchClosureSource", request)
            .map_err(DaemonError::InvalidParam)?;
        let row = self
            .conn
            .query_row(
                "SELECT request_fingerprint,result_json FROM closure_operator_requests
                 WHERE method='LaunchClosureSource' AND idempotency_key=?1",
                [request.idempotency_key.to_string()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        row.map(|(stored, raw)| {
            let mut result: LaunchClosureSourceResultV1 =
                parse_json(&raw, "LaunchClosureSource result")?;
            result.replayed = true;
            if stored != fingerprint {
                result.refusal = Some(ClosureRefusalCodeV1::IdempotencyMismatch);
            }
            Ok(result)
        })
        .transpose()
    }

    /// Resolve a durable launch reservation before the operator performs any
    /// mutable Git observation. This is separate from creating a reservation:
    /// a genuinely new key must still pass operator-side Git preflight first,
    /// while an exact crash replay must converge on its already-owned identity
    /// even if those external refs subsequently move.
    pub(crate) fn resume_closure_source_launch(
        &self,
        request: &LaunchClosureSourceRequestV1,
    ) -> Result<Option<ClosureSourceLaunchClaimV1>> {
        let fingerprint = closure_request_fingerprint("LaunchClosureSource", request)
            .map_err(DaemonError::InvalidParam)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if let Some((stored_fingerprint, result_json)) =
            operator_replay(&tx, "LaunchClosureSource", request.idempotency_key)?
        {
            let mut result: LaunchClosureSourceResultV1 =
                parse_json(&result_json, "LaunchClosureSource result")?;
            result.replayed = true;
            if stored_fingerprint != fingerprint {
                result.refusal = Some(ClosureRefusalCodeV1::IdempotencyMismatch);
            }
            tx.commit()?;
            return Ok(Some(ClosureSourceLaunchClaimV1::Result(result)));
        }
        let Some(reservation) = load_source_launch_reservation(&tx, request.idempotency_key)?
        else {
            tx.commit()?;
            return Ok(None);
        };
        let claim = if reservation.request_fingerprint == fingerprint {
            ClosureSourceLaunchClaimV1::Reserved(reservation)
        } else {
            ClosureSourceLaunchClaimV1::Result(LaunchClosureSourceResultV1 {
                source: reservation.source,
                root_session_id: reservation.root_session_id,
                state: ClosureSourceStateV1::Working,
                replayed: true,
                refusal: Some(ClosureRefusalCodeV1::IdempotencyMismatch),
            })
        };
        tx.commit()?;
        Ok(Some(claim))
    }

    pub(crate) fn reserve_closure_source_launch(
        &self,
        request: &LaunchClosureSourceRequestV1,
    ) -> Result<ClosureSourceLaunchClaimV1> {
        let fingerprint = closure_request_fingerprint("LaunchClosureSource", request)
            .map_err(DaemonError::InvalidParam)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if let Some((stored_fingerprint, result_json)) =
            operator_replay(&tx, "LaunchClosureSource", request.idempotency_key)?
        {
            let mut result: LaunchClosureSourceResultV1 =
                parse_json(&result_json, "LaunchClosureSource result")?;
            result.replayed = true;
            if stored_fingerprint != fingerprint {
                result.refusal = Some(ClosureRefusalCodeV1::IdempotencyMismatch);
            }
            return Ok(ClosureSourceLaunchClaimV1::Result(result));
        }
        if let Some(reservation) = load_source_launch_reservation(&tx, request.idempotency_key)? {
            if reservation.request_fingerprint != fingerprint {
                return Ok(ClosureSourceLaunchClaimV1::Result(
                    LaunchClosureSourceResultV1 {
                        source: reservation.source,
                        root_session_id: reservation.root_session_id,
                        state: ClosureSourceStateV1::Working,
                        replayed: true,
                        refusal: Some(ClosureRefusalCodeV1::IdempotencyMismatch),
                    },
                ));
            }
            return Ok(ClosureSourceLaunchClaimV1::Reserved(reservation));
        }

        let program: (
            String,
            String,
            String,
            String,
            String,
            Option<String>,
            Option<String>,
        ) = tx
            .query_row(
                "SELECT state,repository_identity,base_sha,destination_ref,destination_pre_head,
                        (SELECT id FROM closure_sources WHERE program_id=closure_programs.id),
                        (SELECT id FROM closure_source_launch_reservations WHERE program_id=closure_programs.id)
                 FROM closure_programs WHERE id=?1",
                [request.program_id.to_string()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| DaemonError::InvalidParam("Closure program not found".into()))?;
        if program.0 != "configured" || program.5.is_some() || program.6.is_some() {
            return Err(DaemonError::InvalidParam(
                "Closure program is not launchable".into(),
            ));
        }

        let source_id = ClosureSourceIdV1::new(Uuid::new_v4());
        let root_session_id = Uuid::new_v4();
        let custody_id = Uuid::new_v4();
        let branch_name = format!("rsi/closure-source/{source_id}");
        let source_ref = ClosureLocalBranchRefV1::parse(format!("refs/heads/{branch_name}"))
            .map_err(DaemonError::InvalidParam)?;
        let staging_ref = ClosureLocalBranchRefV1::parse(format!(
            "refs/heads/rsi/closure-stage/{}/{source_id}",
            request.program_id
        ))
        .map_err(DaemonError::InvalidParam)?;
        let source = ClosureSourceIdentityV1 {
            program_id: request.program_id,
            source_id,
            custody_id,
            custody_generation: 1,
            lineage_root_session_id: root_session_id,
            repository_identity: ClosureRepositoryIdentityV1::parse(program.1)
                .map_err(DaemonError::Store)?,
            source_ref,
            source_base_sha: ClosureGitShaV1::parse(program.2).map_err(DaemonError::Store)?,
            destination_ref: ClosureLocalBranchRefV1::parse(program.3)
                .map_err(DaemonError::Store)?,
            destination_pre_head: ClosureGitShaV1::parse(program.4).map_err(DaemonError::Store)?,
            staging_ref,
        };
        let now = Utc::now();
        tx.execute(
            "INSERT INTO closure_source_launch_reservations
             (id,program_id,idempotency_key,request_fingerprint,request_json,source_id,
              root_session_id,custody_id,custody_generation,branch_name,repository_identity,
              source_ref,source_base_sha,destination_ref,destination_pre_head,staging_ref,
              created_at,completed_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,1,?9,?10,?11,?12,?13,?14,?15,?16,NULL)",
            params![
                Uuid::new_v4().to_string(),
                request.program_id.to_string(),
                request.idempotency_key.to_string(),
                fingerprint,
                canonical_program_run_json(request).map_err(DaemonError::InvalidParam)?,
                source.source_id.to_string(),
                root_session_id.to_string(),
                source.custody_id.to_string(),
                branch_name,
                source.repository_identity.as_str(),
                source.source_ref.as_str(),
                source.source_base_sha.as_str(),
                source.destination_ref.as_str(),
                source.destination_pre_head.as_str(),
                source.staging_ref.as_str(),
                timestamp(now),
            ],
        )?;
        tx.commit()?;
        Ok(ClosureSourceLaunchClaimV1::Reserved(
            ClosureSourceLaunchReservationV1 {
                source,
                root_session_id,
                branch_name,
                idempotency_key: request.idempotency_key,
                request_fingerprint: fingerprint,
            },
        ))
    }

    pub(crate) fn replay_closure_evidence(
        &self,
        request: &RecordClosureEvidenceRequestV1,
    ) -> Result<Option<ClosureEvidenceResultV1>> {
        let fingerprint = closure_request_fingerprint("RecordClosureEvidence", request)
            .map_err(DaemonError::InvalidParam)?;
        let row = self
            .conn
            .query_row(
                "SELECT request_fingerprint,result_json FROM closure_operator_requests
                 WHERE method='RecordClosureEvidence' AND idempotency_key=?1",
                [request.idempotency_key.to_string()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        row.map(|(stored, raw)| {
            let mut result: ClosureEvidenceResultV1 =
                parse_json(&raw, "RecordClosureEvidence result")?;
            result.replayed = true;
            if stored != fingerprint {
                result.refusal = Some(ClosureRefusalCodeV1::IdempotencyMismatch);
                result.evidence = None;
                result.eligible = false;
                result.integration_queue_item_id = None;
            }
            Ok(result)
        })
        .transpose()
    }

    pub(crate) fn record_closure_evidence_refusal(
        &self,
        request: &RecordClosureEvidenceRequestV1,
        refusal: ClosureRefusalCodeV1,
    ) -> Result<ClosureEvidenceResultV1> {
        let fingerprint = closure_request_fingerprint("RecordClosureEvidence", request)
            .map_err(DaemonError::InvalidParam)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if let Some((stored_fingerprint, result_json)) =
            operator_replay(&tx, "RecordClosureEvidence", request.idempotency_key)?
        {
            let mut result: ClosureEvidenceResultV1 =
                parse_json(&result_json, "RecordClosureEvidence result")?;
            result.replayed = true;
            if stored_fingerprint != fingerprint {
                result.refusal = Some(ClosureRefusalCodeV1::IdempotencyMismatch);
                result.evidence = None;
                result.eligible = false;
                result.integration_queue_item_id = None;
            }
            return Ok(result);
        }
        commit_closure_evidence_refusal(tx, request, &fingerprint, refusal)
    }

    pub(crate) fn create_closure_program(
        &self,
        request: &CreateClosureProgramRequestV1,
        resolved: &ResolvedClosureProgramV1,
    ) -> Result<CreateClosureProgramResultV1> {
        let fingerprint = closure_request_fingerprint("CreateClosureProgram", request)
            .map_err(DaemonError::InvalidParam)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if let Some((stored_fingerprint, result_json)) = tx
            .query_row(
                "SELECT request_fingerprint,result_json FROM closure_operator_requests
                 WHERE method='CreateClosureProgram' AND idempotency_key=?1",
                [request.idempotency_key.to_string()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?
        {
            let mut result: CreateClosureProgramResultV1 =
                parse_json(&result_json, "CreateClosureProgram result")?;
            if stored_fingerprint != fingerprint {
                result.replayed = true;
                result.refusal = Some(ClosureRefusalCodeV1::IdempotencyMismatch);
                return Ok(result);
            }
            result.replayed = true;
            return Ok(result);
        }

        let program_id = ClosureProgramIdV1::new(Uuid::new_v4());
        let now = Utc::now();
        let review_policy_json = canonical_program_run_json(&request.config.review_policy)
            .map_err(DaemonError::InvalidParam)?;
        let verification_policy_json =
            canonical_program_run_json(&request.config.verification_policy)
                .map_err(DaemonError::InvalidParam)?;
        let verifier_policy_json = canonical_program_run_json(&request.config.verifier_policy)
            .map_err(DaemonError::InvalidParam)?;
        let review_policy_digest =
            digest_json("closure-review-policy:v1", &request.config.review_policy)?;
        let verification_policy_digest = digest_json(
            "closure-verification-policy:v1",
            &request.config.verification_policy,
        )?;
        let verifier_policy_digest = digest_json(
            "closure-verifier-policy:v1",
            &request.config.verifier_policy,
        )?;
        tx.execute(
            "INSERT INTO closure_programs
             (id,version,repository_root,repository_identity,base_ref,base_sha,destination_ref,
              destination_pre_head,state,destination_claim_state,review_policy_json,
              review_policy_digest,verification_policy_json,verification_policy_digest,
              verifier_policy_json,verifier_policy_digest,checkout_remediation_policy,
              creation_idempotency_key,creation_request_fingerprint,created_at,updated_at)
             VALUES (?1,1,?2,?3,?4,?5,?6,?7,'configured','held',?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?17)",
            params![
                program_id.to_string(),
                resolved.repository_root,
                resolved.repository_identity.as_str(),
                request.config.base_ref.as_str(),
                resolved.base_sha.as_str(),
                request.config.destination_ref.as_str(),
                resolved.destination_pre_head.as_str(),
                review_policy_json,
                review_policy_digest.to_string(),
                verification_policy_json,
                verification_policy_digest.to_string(),
                verifier_policy_json,
                verifier_policy_digest.to_string(),
                serde_json::to_value(request.config.checkout_remediation_policy)?
                    .as_str()
                    .unwrap_or("refuse"),
                request.idempotency_key.to_string(),
                fingerprint,
                timestamp(now),
            ],
        )?;
        let result = CreateClosureProgramResultV1 {
            program_id,
            version: 1,
            repository_identity: resolved.repository_identity.clone(),
            base_sha: resolved.base_sha.clone(),
            destination_pre_head: resolved.destination_pre_head.clone(),
            state: ClosureProgramStateV1::Configured,
            replayed: false,
            refusal: None,
        };
        insert_operator_request(
            &tx,
            "CreateClosureProgram",
            request.idempotency_key,
            &fingerprint,
            request,
            &result,
            now,
        )?;
        insert_closure_event(
            &tx,
            program_id,
            None,
            "program_created",
            None,
            "configured",
            None,
            None,
            Some(request.idempotency_key.to_string()),
            serde_json::json!({"version": 1}),
            now,
        )?;
        tx.commit()?;
        Ok(result)
    }

    pub(crate) fn update_closure_program(
        &self,
        request: &UpdateClosureProgramRequestV1,
        resolved: &ResolvedClosureProgramV1,
    ) -> Result<ClosureProgramMutationResultV1> {
        let fingerprint = closure_request_fingerprint("UpdateClosureProgram", request)
            .map_err(DaemonError::InvalidParam)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if let Some((stored_fingerprint, result_json)) =
            operator_replay(&tx, "UpdateClosureProgram", request.idempotency_key)?
        {
            let mut result: ClosureProgramMutationResultV1 =
                parse_json(&result_json, "UpdateClosureProgram result")?;
            result.replayed = true;
            if stored_fingerprint != fingerprint {
                result.refusal = Some(ClosureRefusalCodeV1::IdempotencyMismatch);
            }
            return Ok(result);
        }
        let (version, state): (u64, String) = tx
            .query_row(
                "SELECT version,state FROM closure_programs WHERE id=?1",
                [request.program_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| DaemonError::InvalidParam("Closure program not found".into()))?;
        let review_policy_digest =
            digest_json("closure-review-policy:v1", &request.config.review_policy)?;
        let verification_policy_digest = digest_json(
            "closure-verification-policy:v1",
            &request.config.verification_policy,
        )?;
        let verifier_policy_digest = digest_json(
            "closure-verifier-policy:v1",
            &request.config.verifier_policy,
        )?;
        let mut result = ClosureProgramMutationResultV1 {
            program_id: request.program_id,
            version,
            review_policy_digest: review_policy_digest.clone(),
            verification_policy_digest: verification_policy_digest.clone(),
            verifier_policy_digest: verifier_policy_digest.clone(),
            replayed: false,
            refusal: None,
        };
        if version != request.expected_program_version
            || !matches!(state.as_str(), "draft" | "configured")
        {
            result.refusal = Some(ClosureRefusalCodeV1::ProgramVersionConflict);
            insert_operator_request(
                &tx,
                "UpdateClosureProgram",
                request.idempotency_key,
                &fingerprint,
                request,
                &result,
                Utc::now(),
            )?;
            tx.commit()?;
            return Ok(result);
        }
        let now = Utc::now();
        tx.execute(
            "UPDATE closure_programs SET version=version+1,repository_root=?2,
             repository_identity=?3,base_ref=?4,base_sha=?5,destination_ref=?6,
             destination_pre_head=?7,review_policy_json=?8,review_policy_digest=?9,
             verification_policy_json=?10,verification_policy_digest=?11,
             verifier_policy_json=?12,verifier_policy_digest=?13,
             checkout_remediation_policy=?14,updated_at=?15 WHERE id=?1",
            params![
                request.program_id.to_string(),
                resolved.repository_root,
                resolved.repository_identity.as_str(),
                request.config.base_ref.as_str(),
                resolved.base_sha.as_str(),
                request.config.destination_ref.as_str(),
                resolved.destination_pre_head.as_str(),
                canonical_program_run_json(&request.config.review_policy)
                    .map_err(DaemonError::InvalidParam)?,
                review_policy_digest.to_string(),
                canonical_program_run_json(&request.config.verification_policy)
                    .map_err(DaemonError::InvalidParam)?,
                verification_policy_digest.to_string(),
                canonical_program_run_json(&request.config.verifier_policy)
                    .map_err(DaemonError::InvalidParam)?,
                verifier_policy_digest.to_string(),
                serde_json::to_value(request.config.checkout_remediation_policy)?
                    .as_str()
                    .unwrap_or("refuse"),
                timestamp(now)
            ],
        )?;
        result.version = version + 1;
        insert_operator_request(
            &tx,
            "UpdateClosureProgram",
            request.idempotency_key,
            &fingerprint,
            request,
            &result,
            now,
        )?;
        tx.commit()?;
        Ok(result)
    }

    pub(crate) fn closure_program_launch_config(
        &self,
        program_id: ClosureProgramIdV1,
    ) -> Result<(ClosureProgramSummaryV1, ClosureProgramConfigV1)> {
        self.conn
            .query_row(
                "SELECT version,repository_root,repository_identity,base_ref,base_sha,destination_ref,
                        destination_pre_head,state,destination_claim_state,review_policy_json,
                        verification_policy_json,verifier_policy_json,checkout_remediation_policy,
                        created_at,updated_at,
                        (SELECT id FROM closure_sources WHERE program_id=closure_programs.id)
                 FROM closure_programs WHERE id=?1",
                [program_id.to_string()],
                |row| {
                    let state: String = row.get(7)?;
                    let claim: String = row.get(8)?;
                    let checkout: String = row.get(12)?;
                    Ok((
                        row.get::<_, u64>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?, row.get::<_, String>(4)?, row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?, state, claim, row.get::<_, String>(9)?,
                        row.get::<_, String>(10)?, row.get::<_, String>(11)?, checkout,
                        row.get::<_, String>(13)?, row.get::<_, String>(14)?,
                        row.get::<_, Option<String>>(15)?,
                    ))
                },
            )
            .optional()?
            .map(|row| map_program_config(program_id, row))
            .transpose()?
            .ok_or_else(|| DaemonError::InvalidParam("Closure program not found".into()))
    }

    pub(crate) fn list_closure_programs(
        &self,
        request: &ListClosureProgramsRequestV1,
    ) -> Result<ClosureProgramListV1> {
        let limit = request.limit.unwrap_or(50).clamp(1, 200);
        let cursor = request.cursor.as_deref().unwrap_or("");
        let state = request.state.map(|value| wire(&value)).transpose()?;
        let mut statement = self.conn.prepare(
            "SELECT version,repository_root,repository_identity,base_ref,base_sha,destination_ref,
                    destination_pre_head,state,destination_claim_state,review_policy_json,
                    verification_policy_json,verifier_policy_json,checkout_remediation_policy,
                    created_at,updated_at,
                    (SELECT id FROM closure_sources WHERE program_id=closure_programs.id),id
             FROM closure_programs
             WHERE id>?1 AND (?2 IS NULL OR state=?2)
             ORDER BY id LIMIT ?3",
        )?;
        let mut rows = statement.query(params![cursor, state, limit + 1])?;
        let mut programs = Vec::new();
        while let Some(row) = rows.next()? {
            let id: String = row.get(16)?;
            let program_id = ClosureProgramIdV1::new(
                Uuid::parse_str(&id).map_err(|error| DaemonError::Store(error.to_string()))?,
            );
            let values = (
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
                row.get(8)?,
                row.get(9)?,
                row.get(10)?,
                row.get(11)?,
                row.get(12)?,
                row.get(13)?,
                row.get(14)?,
                row.get(15)?,
            );
            programs.push(map_program_config(program_id, values)?.0);
        }
        let next_cursor = if programs.len() > limit as usize {
            programs.pop();
            programs
                .last()
                .map(|program| program.program_id.to_string())
        } else {
            None
        };
        Ok(ClosureProgramListV1 {
            programs,
            next_cursor,
        })
    }

    pub(crate) fn get_closure_program_summary(
        &self,
        request: &GetClosureProgramRequestV1,
    ) -> Result<ClosureProgramDetailV1> {
        let program_id = match (request.program_id, request.source_id) {
            (Some(program_id), None) => program_id,
            (None, Some(source_id)) => {
                let raw: String = self.conn.query_row(
                    "SELECT program_id FROM closure_sources WHERE id=?1",
                    [source_id.to_string()],
                    |row| row.get(0),
                )?;
                ClosureProgramIdV1::new(
                    Uuid::parse_str(&raw).map_err(|error| DaemonError::Store(error.to_string()))?,
                )
            }
            _ => {
                return Err(DaemonError::InvalidParam(
                    "exactly one of program_id or source_id is required".into(),
                ));
            }
        };
        let (program, _) = self.closure_program_launch_config(program_id)?;
        let source = program
            .source_id
            .map(|source_id| self.closure_source_identity(source_id))
            .transpose()?;
        let source_state = program
            .source_id
            .map(|source_id| {
                self.conn
                    .query_row(
                        "SELECT state FROM closure_sources WHERE id=?1",
                        [source_id.to_string()],
                        |row| row.get::<_, String>(0),
                    )
                    .map_err(DaemonError::from)
                    .and_then(|value| parse_json(&format!("\"{value}\""), "Closure source state"))
            })
            .transpose()?;
        let output_validation = program
            .source_id
            .map(|source_id| load_latest_output_validation(&self.conn, source_id))
            .transpose()?
            .flatten();
        let evidence = program
            .source_id
            .map(|source_id| load_accepted_evidence(&self.conn, source_id))
            .transpose()?
            .flatten();
        let integration_queue_item_id = program
            .source_id
            .map(|source_id| {
                self.conn
                    .query_row(
                        "SELECT id FROM closure_integration_queue WHERE source_id=?1",
                        [source_id.to_string()],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()
                    .map_err(DaemonError::from)
                    .and_then(|value| {
                        value
                            .map(|raw| {
                                Uuid::parse_str(&raw)
                                    .map_err(|error| DaemonError::Store(error.to_string()))
                            })
                            .transpose()
                    })
            })
            .transpose()?
            .flatten();
        let next_actions = match program.state {
            ClosureProgramStateV1::Configured => vec!["launch_source".into()],
            ClosureProgramStateV1::AwaitingEvidence => vec!["record_evidence".into()],
            ClosureProgramStateV1::AwaitingIntegration => vec!["k2_unavailable".into()],
            _ => Vec::new(),
        };
        Ok(ClosureProgramDetailV1 {
            program,
            source,
            source_state,
            output_validation,
            evidence,
            integration_queue_item_id,
            next_actions,
        })
    }

    pub(crate) fn closure_source_identity(
        &self,
        source_id: ClosureSourceIdV1,
    ) -> Result<ClosureSourceIdentityV1> {
        self.closure_terminal_capture_candidate(source_id)?
            .map(|candidate| candidate.source)
            .or_else(|| {
                self.conn
                    .query_row(
                        "SELECT s.program_id,s.custody_id,s.custody_generation,s.root_session_id,
                                p.repository_identity,s.source_ref,s.source_base_sha,s.destination_ref,
                                s.destination_pre_head,s.staging_ref
                         FROM closure_sources s JOIN closure_programs p ON p.id=s.program_id
                         WHERE s.id=?1",
                        [source_id.to_string()],
                        |row| {
                            Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,u64>(2)?,row.get::<_,String>(3)?,row.get::<_,String>(4)?,row.get::<_,String>(5)?,row.get::<_,String>(6)?,row.get::<_,String>(7)?,row.get::<_,String>(8)?,row.get::<_,String>(9)?))
                        },
                    )
                    .optional()
                    .ok()
                    .flatten()
                    .and_then(|row| {
                        Some(ClosureSourceIdentityV1 {
                            program_id: ClosureProgramIdV1::new(Uuid::parse_str(&row.0).ok()?),
                            source_id,
                            custody_id: Uuid::parse_str(&row.1).ok()?,
                            custody_generation: row.2,
                            lineage_root_session_id: Uuid::parse_str(&row.3).ok()?,
                            repository_identity: ClosureRepositoryIdentityV1::parse(row.4).ok()?,
                            source_ref: ClosureLocalBranchRefV1::parse(row.5).ok()?,
                            source_base_sha: ClosureGitShaV1::parse(row.6).ok()?,
                            destination_ref: ClosureLocalBranchRefV1::parse(row.7).ok()?,
                            destination_pre_head: ClosureGitShaV1::parse(row.8).ok()?,
                            staging_ref: ClosureLocalBranchRefV1::parse(row.9).ok()?,
                        })
                    })
            })
            .ok_or_else(|| DaemonError::InvalidParam("Closure source not found".into()))
    }

    pub(crate) fn record_closure_source_launch(
        &self,
        prepared: &PreparedClosureSourceV1,
    ) -> Result<LaunchClosureSourceResultV1> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if let Some((stored_fingerprint, result_json)) =
            operator_replay(&tx, "LaunchClosureSource", prepared.idempotency_key)?
        {
            let mut result: LaunchClosureSourceResultV1 =
                parse_json(&result_json, "LaunchClosureSource result")?;
            result.replayed = true;
            if stored_fingerprint != prepared.request_fingerprint {
                result.refusal = Some(ClosureRefusalCodeV1::IdempotencyMismatch);
            }
            return Ok(result);
        }
        let stored_reservation = load_source_launch_reservation(&tx, prepared.idempotency_key)?;
        #[cfg(not(test))]
        let reservation = stored_reservation.clone().ok_or_else(|| {
            DaemonError::Store("Closure source launch commit requires a durable reservation".into())
        })?;
        // Legacy fixture setup predates the public launch boundary. Production
        // builds have no bypass; focused reservation tests exercise the same
        // path production uses.
        #[cfg(test)]
        let reservation =
            stored_reservation
                .clone()
                .unwrap_or_else(|| ClosureSourceLaunchReservationV1 {
                    source: prepared.source.clone(),
                    root_session_id: prepared.root_session_id,
                    branch_name: prepared
                        .source
                        .source_ref
                        .as_str()
                        .trim_start_matches("refs/heads/")
                        .to_string(),
                    idempotency_key: prepared.idempotency_key,
                    request_fingerprint: prepared.request_fingerprint.clone(),
                });
        if reservation.request_fingerprint != prepared.request_fingerprint
            || reservation.root_session_id != prepared.root_session_id
            || reservation.source != prepared.source
        {
            return Err(DaemonError::Store(
                "Closure source launch commit does not match its durable reservation".into(),
            ));
        }
        let now = Utc::now();
        let source = &prepared.source;
        let program: (String, String, String, String, String) = tx.query_row(
            "SELECT state,repository_identity,base_sha,destination_ref,destination_pre_head
             FROM closure_programs WHERE id=?1",
            [source.program_id.to_string()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )?;
        if program.0 != "configured"
            || program.1 != source.repository_identity.as_str()
            || program.2 != source.source_base_sha.as_str()
            || program.3 != source.destination_ref.as_str()
            || program.4 != source.destination_pre_head.as_str()
        {
            return Err(DaemonError::InvalidParam(
                "Closure program configuration changed before source launch commit".into(),
            ));
        }
        let session_custody: (Option<String>, Option<String>, String, u64, String, String) = tx
            .query_row(
                "SELECT s.sandbox_custody_id,s.continued_from,c.state,c.generation,
                        c.repository_identity,c.source_commit
                 FROM sessions s JOIN sandbox_custody_roots c
                   ON c.custody_id=s.sandbox_custody_id
                 WHERE s.id=?1 AND c.owner_session_id=s.id",
                [prepared.root_session_id.to_string()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )?;
        if session_custody.0.as_deref() != Some(source.custody_id.to_string().as_str())
            || session_custody.1.is_some()
            || session_custody.2 != "live"
            || session_custody.3 != source.custody_generation
            || session_custody.4 != source.repository_identity.as_str()
            || session_custody.5 != source.source_base_sha.as_str()
        {
            return Err(DaemonError::InvalidParam(
                "Closure source session custody changed before launch commit".into(),
            ));
        }
        let existing_source: Option<String> = tx
            .query_row(
                "SELECT id FROM closure_sources WHERE program_id=?1",
                [source.program_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        if existing_source.is_some() {
            return Err(DaemonError::InvalidParam(
                "Closure program already has a source".into(),
            ));
        }
        tx.execute(
            "INSERT INTO closure_sources
             (id,program_id,custody_id,custody_generation,root_session_id,source_ref,source_base_sha,
              destination_ref,destination_pre_head,staging_ref,state,launch_idempotency_key,
              launch_request_fingerprint,created_at,updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,'working',?11,?12,?13,?13)",
            params![
                source.source_id.to_string(), source.program_id.to_string(), source.custody_id.to_string(),
                source.custody_generation, prepared.root_session_id.to_string(), source.source_ref.as_str(),
                source.source_base_sha.as_str(), source.destination_ref.as_str(), source.destination_pre_head.as_str(),
                source.staging_ref.as_str(), prepared.idempotency_key.to_string(), prepared.request_fingerprint,
                timestamp(now),
            ],
        )?;
        tx.execute(
            "INSERT INTO closure_source_sessions
             (source_id,session_id,rotation_depth,custody_id,custody_generation,continued_from_session_id,bound_at)
             VALUES (?1,?2,0,?3,?4,NULL,?5)",
            params![source.source_id.to_string(), prepared.root_session_id.to_string(), source.custody_id.to_string(), source.custody_generation, timestamp(now)],
        )?;
        let updated = tx.execute(
            "UPDATE closure_programs SET state='working',version=version+1,updated_at=?2 WHERE id=?1 AND state='configured'",
            params![source.program_id.to_string(), timestamp(now)],
        )?;
        if updated != 1 {
            return Err(DaemonError::Store(
                "Closure program state changed before source launch commit".into(),
            ));
        }
        let result = LaunchClosureSourceResultV1 {
            source: source.clone(),
            root_session_id: prepared.root_session_id,
            state: ClosureSourceStateV1::Working,
            replayed: false,
            refusal: None,
        };
        let request_json: serde_json::Value = match stored_reservation.as_ref() {
            Some(_) => tx
                .query_row(
                    "SELECT request_json FROM closure_source_launch_reservations WHERE idempotency_key=?1",
                    [prepared.idempotency_key.to_string()],
                    |row| row.get::<_, String>(0),
                )
                .and_then(|raw| {
                    serde_json::from_str(&raw).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })
                })?,
            None => serde_json::json!({
                "program_id": source.program_id,
                "source_id": source.source_id,
                "root_session_id": prepared.root_session_id,
            }),
        };
        insert_operator_request_raw(
            &tx,
            "LaunchClosureSource",
            prepared.idempotency_key,
            &prepared.request_fingerprint,
            &request_json,
            &result,
            now,
        )?;
        if stored_reservation.is_some() {
            let completed = tx.execute(
                "UPDATE closure_source_launch_reservations SET completed_at=?2
                 WHERE idempotency_key=?1 AND completed_at IS NULL",
                params![prepared.idempotency_key.to_string(), timestamp(now)],
            )?;
            if completed != 1 {
                return Err(DaemonError::Store(
                    "Closure source launch reservation completion fence lost".into(),
                ));
            }
        }
        insert_closure_event(
            &tx,
            source.program_id,
            Some(source.source_id),
            "source_launched",
            Some("configured"),
            "working",
            None,
            Some("working"),
            Some(prepared.idempotency_key.to_string()),
            serde_json::json!({"root_session_id": prepared.root_session_id}),
            now,
        )?;
        tx.commit()?;
        Ok(result)
    }

    pub(crate) fn append_closure_source_session(
        &self,
        source_id: ClosureSourceIdV1,
        session_id: Uuid,
        continued_from: Uuid,
        rotation_depth: u32,
        custody_id: Uuid,
        custody_generation: u64,
    ) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let expected_source: (String, u64) = tx.query_row(
            "SELECT custody_id,custody_generation FROM closure_sources WHERE id=?1",
            [source_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let session: (Option<String>, Option<String>, String, u64) = tx.query_row(
            "SELECT s.sandbox_custody_id,s.continued_from,c.owner_session_id,c.generation
             FROM sessions s JOIN sandbox_custody_roots c ON c.custody_id=s.sandbox_custody_id
             WHERE s.id=?1",
            [session_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        let existing: Option<(String, String, u64, Option<String>)> = tx
            .query_row(
                "SELECT session_id,custody_id,custody_generation,continued_from_session_id
                 FROM closure_source_sessions
                 WHERE (source_id=?1 AND rotation_depth=?2) OR session_id=?3",
                params![
                    source_id.to_string(),
                    rotation_depth,
                    session_id.to_string()
                ],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        if let Some(existing) = existing {
            if existing.0 == session_id.to_string()
                && existing.1 == custody_id.to_string()
                && existing.2 == custody_generation
                && existing.3.as_deref() == Some(continued_from.to_string().as_str())
            {
                return Ok(());
            }
            return Err(DaemonError::Store(
                "Closure rotation binding conflicts with durable lineage".into(),
            ));
        }
        if expected_source.0 != custody_id.to_string()
            || custody_generation != expected_source.1.saturating_add(1)
            || session.0.as_deref() != Some(custody_id.to_string().as_str())
            || session.1.as_deref() != Some(continued_from.to_string().as_str())
            || session.2 != session_id.to_string()
            || session.3 != custody_generation
        {
            return Err(DaemonError::InvalidParam(
                "Closure rotation session does not match current source custody generation or predecessor"
                    .into(),
            ));
        }
        tx.execute(
            "INSERT INTO closure_source_sessions
             (source_id,session_id,rotation_depth,custody_id,custody_generation,continued_from_session_id,bound_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![source_id.to_string(), session_id.to_string(), rotation_depth, custody_id.to_string(), custody_generation, continued_from.to_string(), timestamp(Utc::now())],
        )?;
        let updated = tx.execute(
            "UPDATE closure_sources SET custody_generation=?2,updated_at=?3
             WHERE id=?1 AND custody_generation=?4",
            params![
                source_id.to_string(),
                custody_generation,
                timestamp(Utc::now()),
                expected_source.1
            ],
        )?;
        if updated != 1 {
            return Err(DaemonError::Store(
                "Closure source custody generation changed before rotation binding commit".into(),
            ));
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn closure_terminal_capture_candidate(
        &self,
        source_id: ClosureSourceIdV1,
    ) -> Result<Option<ClosureTerminalCaptureCandidateV1>> {
        let source_row = self.conn.query_row(
            "SELECT s.program_id,s.custody_id,s.custody_generation,s.root_session_id,p.repository_identity,
                    s.source_ref,s.source_base_sha,s.destination_ref,s.destination_pre_head,s.staging_ref,s.created_at,
                    p.repository_root,c.sandbox_root
             FROM closure_sources s JOIN closure_programs p ON p.id=s.program_id
             JOIN sandbox_custody_roots c ON c.custody_id=s.custody_id WHERE s.id=?1",
            [source_id.to_string()],
            |row| Ok((row.get::<_, String>(0)?,row.get::<_, String>(1)?,row.get::<_, u64>(2)?,row.get::<_, String>(3)?,row.get::<_, String>(4)?,row.get::<_, String>(5)?,row.get::<_, String>(6)?,row.get::<_, String>(7)?,row.get::<_, String>(8)?,row.get::<_, String>(9)?,row.get::<_, String>(10)?,row.get::<_, String>(11)?,row.get::<_, String>(12)?)),
        ).optional()?;
        let Some(source_row) = source_row else {
            return Ok(None);
        };
        let tip = self.conn.query_row(
            "SELECT css.session_id,css.rotation_depth,se.status,se.model_invocation_id,mi.status
             FROM closure_source_sessions css JOIN sessions se ON se.id=css.session_id
             LEFT JOIN model_invocations mi ON mi.id=se.model_invocation_id AND mi.session_id=se.id
             WHERE css.source_id=?1 ORDER BY css.rotation_depth DESC LIMIT 1",
            [source_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, u32>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            },
        )?;
        if !matches!(tip.2.as_str(), "Completed" | "Failed" | "Interrupted") {
            return Ok(None);
        }
        let Some(invocation_raw) = tip.3 else {
            return Ok(None);
        };
        if !matches!(tip.4.as_deref(), Some("completed" | "failed" | "cancelled")) {
            return Ok(None);
        }
        let tip_session_id =
            Uuid::parse_str(&tip.0).map_err(|error| DaemonError::Store(error.to_string()))?;
        let model_invocation_id = Uuid::parse_str(&invocation_raw)
            .map_err(|error| DaemonError::Store(error.to_string()))?;
        let mut lineage_issue = validate_lineage_rows(
            &self.conn,
            source_id,
            &source_row.1,
            source_row.2,
            &source_row.3,
        )?;
        if lineage_issue.is_none() {
            let successor_count: i64 = self.conn.query_row(
                "SELECT count(*) FROM sessions WHERE continued_from=?1",
                [tip_session_id.to_string()],
                |row| row.get(0),
            )?;
            if successor_count != 0 {
                lineage_issue = Some(format!(
                    "Closure lineage tip has {successor_count} durable unbound successor(s)"
                ));
            }
        }
        let mut statement = self.conn.prepare(
            "SELECT e.id,e.sequence,p.provider_event_type,e.content,p.producer_kind
             FROM conversation_events e JOIN conversation_event_provenance p ON p.conversation_event_id=e.id
             WHERE e.session_id=?1 AND e.event_type='Message' AND e.role='Assistant' AND trim(e.content)!=''
               AND p.model_invocation_id=?2
             ORDER BY e.sequence,e.id",
        )?;
        let event_rows = statement
            .query_map(
                params![tip_session_id.to_string(), model_invocation_id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i32>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                },
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let event = select_last_terminal_output(&event_rows, |event| {
            event.4 == "provider_assistant_output"
        });
        Ok(Some(ClosureTerminalCaptureCandidateV1 {
            source: ClosureSourceIdentityV1 {
                program_id: ClosureProgramIdV1::new(
                    Uuid::parse_str(&source_row.0)
                        .map_err(|error| DaemonError::Store(error.to_string()))?,
                ),
                source_id,
                custody_id: Uuid::parse_str(&source_row.1)
                    .map_err(|error| DaemonError::Store(error.to_string()))?,
                custody_generation: source_row.2,
                lineage_root_session_id: Uuid::parse_str(&source_row.3)
                    .map_err(|error| DaemonError::Store(error.to_string()))?,
                repository_identity: ClosureRepositoryIdentityV1::parse(source_row.4)
                    .map_err(DaemonError::Store)?,
                source_ref: ClosureLocalBranchRefV1::parse(source_row.5)
                    .map_err(DaemonError::Store)?,
                source_base_sha: ClosureGitShaV1::parse(source_row.6)
                    .map_err(DaemonError::Store)?,
                destination_ref: ClosureLocalBranchRefV1::parse(source_row.7)
                    .map_err(DaemonError::Store)?,
                destination_pre_head: ClosureGitShaV1::parse(source_row.8)
                    .map_err(DaemonError::Store)?,
                staging_ref: ClosureLocalBranchRefV1::parse(source_row.9)
                    .map_err(DaemonError::Store)?,
            },
            repository_root: source_row.11,
            source_worktree_root: source_row.12,
            source_created_at: DateTime::parse_from_rfc3339(&source_row.10)
                .map_err(|error| DaemonError::Store(error.to_string()))?
                .with_timezone(&Utc),
            tip_session_id,
            rotation_depth: tip.1,
            model_invocation_id,
            terminal_status: tip.2,
            lineage_issue,
            event_id: event.map(|value| value.0),
            event_sequence: event.map(|value| value.1),
            provider_event_type: event.map(|value| value.2.clone()),
            raw_handoff: event.map(|value| value.3.clone()),
        }))
    }

    pub(crate) fn capture_or_replay_closure_output(
        &self,
        proposal: &ClosureOutputCaptureProposalV1,
    ) -> Result<ClosureOutputValidationResultV1> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if let Some(result) = load_output_validation(&tx, &proposal.key)? {
            return Ok(ClosureOutputValidationResultV1 {
                replayed: true,
                ..result
            });
        }
        revalidate_capture_key(&tx, proposal)?;
        let validation_id = Uuid::new_v4();
        let now = Utc::now();
        let normalized_json = proposal
            .normalized_envelope
            .as_ref()
            .map(canonical_program_run_json)
            .transpose()
            .map_err(DaemonError::InvalidParam)?;
        tx.execute(
            "INSERT INTO closure_output_validations
             (id,source_id,tip_session_id,model_invocation_id,conversation_event_id,conversation_sequence,
              producer_kind,provider_event_type,parser_source,raw_handoff,raw_handoff_digest,
              normalized_envelope_json,normalized_envelope_digest,disposition,validation_issues_json,
              source_state,observed_source_ref_head,observed_worktree_head,observed_worktree_clean,created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20)",
            params![
                validation_id.to_string(), proposal.key.source_id.to_string(), proposal.key.tip_session_id.to_string(),
                proposal.key.model_invocation_id.to_string(), proposal.event_id, proposal.event_sequence,
                proposal.event_id.map(|_| "provider_assistant_output"), proposal.provider_event_type,
                if proposal.event_id.is_some() { "closure_handoff_field_v1" } else { "missing_provider_output" },
                proposal.raw_handoff, proposal.raw_handoff_digest, normalized_json, proposal.normalized_envelope_digest,
                wire(&proposal.disposition)?, serde_json::to_string(&proposal.issues)?, wire(&proposal.source_state)?,
                proposal.observed_source_ref_head.as_ref().map(ClosureGitShaV1::as_str),
                proposal.observed_worktree_head.as_ref().map(ClosureGitShaV1::as_str),
                proposal.observed_worktree_clean.map(i64::from), timestamp(now),
            ],
        )?;
        tx.execute(
            "UPDATE closure_sources SET state=?2,source_head=COALESCE(?3,source_head),updated_at=?4 WHERE id=?1",
            params![proposal.key.source_id.to_string(), wire(&proposal.source_state)?, proposal.sealed_source_head.as_ref().map(ClosureGitShaV1::as_str), timestamp(now)],
        )?;
        let program_id_raw: String = tx.query_row(
            "SELECT program_id FROM closure_sources WHERE id=?1",
            [proposal.key.source_id.to_string()],
            |row| row.get(0),
        )?;
        let program_id = ClosureProgramIdV1::new(
            Uuid::parse_str(&program_id_raw)
                .map_err(|error| DaemonError::Store(error.to_string()))?,
        );
        let mut queue_id = None;
        let (program_state, final_source_state) = match proposal.source_state {
            ClosureSourceStateV1::OutcomeCommitted => (
                "awaiting_evidence".to_string(),
                "awaiting_evidence".to_string(),
            ),
            ClosureSourceStateV1::OutcomeNoChange => {
                let sealed = proposal.sealed_source_head.as_ref().ok_or_else(|| {
                    DaemonError::Store(
                        "accepted no-change output omitted sealed source head".into(),
                    )
                })?;
                let (review_policy_digest, verification_policy_digest, verifier_policy_digest): (
                    String,
                    String,
                    String,
                ) = tx.query_row(
                    "SELECT review_policy_digest,verification_policy_digest,verifier_policy_digest
                     FROM closure_programs WHERE id=?1",
                    [program_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )?;
                let evidence_id = Uuid::new_v4();
                tx.execute(
                    "INSERT INTO closure_evidence
                     (id,source_id,disposition,reviewed_source_head,review_schema_version,
                      review_raw_bytes,review_digest,normalized_review_json,finding_set_digest,
                      reviewer_session_id,reviewer_model_invocation_id,reviewer_provider,
                      reviewer_model,reviewer_custody_id,reviewer_custody_generation,
                      evidence_commit_sha,evidence_parent_sha,review_json_path,manifest_v2_path,
                      review_handoff_event_id,review_handoff_digest,manifest_schema_version,
                      manifest_raw_bytes,manifest_digest,manifest_source_head,source_ref_before,
                      source_ref_after,source_worktree_head_before,source_worktree_head_after,
                      review_policy_digest,verification_policy_digest,verifier_policy_digest,
                      accepted_by,accepted_at)
                     VALUES (?1,?2,'not_required_proven_no_change',?3,NULL,NULL,NULL,NULL,NULL,
                             NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,
                             NULL,NULL,?3,?3,?3,?3,?4,?5,?6,'daemon_git_observation',?7)",
                    params![
                        evidence_id.to_string(),
                        proposal.key.source_id.to_string(),
                        sealed.as_str(),
                        review_policy_digest,
                        verification_policy_digest,
                        verifier_policy_digest,
                        timestamp(now),
                    ],
                )?;
                let generated_queue_id = Uuid::new_v4();
                tx.execute(
                    "INSERT INTO closure_integration_queue
                     (id,source_id,evidence_id,source_head,state,created_at,updated_at)
                     VALUES (?1,?2,?3,?4,'eligible_k2',?5,?5)",
                    params![
                        generated_queue_id.to_string(),
                        proposal.key.source_id.to_string(),
                        evidence_id.to_string(),
                        sealed.as_str(),
                        timestamp(now),
                    ],
                )?;
                queue_id = Some(generated_queue_id);
                (
                    "awaiting_integration".to_string(),
                    "integration_queued".to_string(),
                )
            }
            _ => ("outcome_blocked".to_string(), wire(&proposal.source_state)?),
        };
        tx.execute(
            "UPDATE closure_sources SET state=?2,updated_at=?3 WHERE id=?1",
            params![
                proposal.key.source_id.to_string(),
                &final_source_state,
                timestamp(now)
            ],
        )?;
        tx.execute(
            "UPDATE closure_programs SET state=?2,version=version+1,updated_at=?3 WHERE id=?1",
            params![program_id.to_string(), &program_state, timestamp(now)],
        )?;
        insert_closure_event(
            &tx,
            program_id,
            Some(proposal.key.source_id),
            "terminal_output_captured",
            None,
            &program_state,
            None,
            Some(&final_source_state),
            Some(format!(
                "{}:{}:{}",
                proposal.key.source_id,
                proposal.key.tip_session_id,
                proposal.key.model_invocation_id
            )),
            serde_json::json!({"validation_id": validation_id, "disposition": proposal.disposition}),
            now,
        )?;
        let result = ClosureOutputValidationResultV1 {
            validation_id,
            key: proposal.key.clone(),
            conversation_event_id: proposal.event_id,
            conversation_sequence: proposal.event_sequence,
            disposition: proposal.disposition,
            source_state: proposal.source_state,
            normalized_envelope: proposal.normalized_envelope.clone(),
            normalized_digest: proposal.normalized_envelope_digest.clone(),
            issues: proposal.issues.clone(),
            replayed: false,
            integration_queue_item_id: queue_id,
        };
        tx.commit()?;
        Ok(result)
    }

    pub(crate) fn closure_output_recovery_page(
        &self,
        cursor: Option<&ClosureOutputRecoveryCursorV1>,
        high_water: Option<&ClosureOutputRecoveryCursorV1>,
        limit: u32,
    ) -> Result<Vec<ClosureSourceIdV1>> {
        let cursor_at = cursor.map(|value| timestamp(value.source_created_at));
        let cursor_id = cursor.map(|value| value.source_id.to_string());
        let high_at = high_water.map(|value| timestamp(value.source_created_at));
        let high_id = high_water.map(|value| value.source_id.to_string());
        let mut stmt = self.conn.prepare(
            "SELECT cs.id FROM closure_sources cs
             WHERE cs.state IN ('working','outcome_blocked')
               AND (?1 IS NULL OR cs.created_at>?1 OR (cs.created_at=?1 AND cs.id>?2))
               AND (?3 IS NULL OR cs.created_at<?3 OR (cs.created_at=?3 AND cs.id<=?4))
               AND EXISTS (
                 SELECT 1 FROM closure_source_sessions css JOIN sessions s ON s.id=css.session_id
                 JOIN model_invocations mi ON mi.id=s.model_invocation_id AND mi.session_id=s.id
                 WHERE css.source_id=cs.id AND css.rotation_depth=(SELECT max(rotation_depth) FROM closure_source_sessions WHERE source_id=cs.id)
                   AND s.status IN ('Completed','Failed','Interrupted') AND s.model_invocation_id IS NOT NULL
                   AND mi.status IN ('completed','failed','cancelled')
                   AND NOT EXISTS (SELECT 1 FROM closure_output_validations v WHERE v.source_id=cs.id AND v.tip_session_id=s.id AND v.model_invocation_id=s.model_invocation_id)
               )
             ORDER BY cs.created_at,cs.id LIMIT ?5",
        )?;
        stmt.query_map(
            params![cursor_at, cursor_id, high_at, high_id, limit],
            |row| row.get::<_, String>(0),
        )?
        .map(|row| {
            let raw = row?;
            Ok(ClosureSourceIdV1::new(Uuid::parse_str(&raw).map_err(
                |error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                },
            )?))
        })
        .collect::<std::result::Result<Vec<_>, rusqlite::Error>>()
        .map_err(Into::into)
    }

    pub(crate) fn closure_output_recovery_high_water(
        &self,
    ) -> Result<Option<ClosureOutputRecoveryCursorV1>> {
        self.conn
            .query_row(
                "SELECT created_at,id FROM closure_sources ORDER BY created_at DESC,id DESC LIMIT 1",
                [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?
            .map(|(created_at, source_id)| {
                Ok(ClosureOutputRecoveryCursorV1 {
                    source_created_at: DateTime::parse_from_rfc3339(&created_at)
                        .map_err(|error| DaemonError::Store(error.to_string()))?
                        .with_timezone(&Utc),
                    source_id: ClosureSourceIdV1::new(
                        Uuid::parse_str(&source_id)
                            .map_err(|error| DaemonError::Store(error.to_string()))?,
                    ),
                })
            })
            .transpose()
    }

    pub(crate) fn closure_source_for_session(
        &self,
        session_id: Uuid,
    ) -> Result<Option<ClosureSourceIdV1>> {
        self.conn
            .query_row(
                "SELECT source_id FROM closure_source_sessions WHERE session_id=?1",
                [session_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|raw| {
                Uuid::parse_str(&raw)
                    .map(ClosureSourceIdV1::new)
                    .map_err(|error| DaemonError::Store(error.to_string()))
            })
            .transpose()
    }

    pub(crate) fn closure_launch_selector_for_session(
        &self,
        session_id: Uuid,
    ) -> Result<Option<crate::closure_kernel::ClosureLaunchSelectorV1>> {
        self.conn
            .query_row(
                "SELECT cs.program_id,cs.id,cs.source_base_sha,cs.destination_ref,
                        cs.destination_pre_head,cs.staging_ref,cs.root_session_id,c.custody_id,
                        c.custody_generation,s.model_invocation_id
                 FROM closure_source_sessions css
                 JOIN closure_sources cs ON cs.id=css.source_id
                 JOIN closure_source_sessions c ON c.source_id=cs.id AND c.session_id=?1
                 JOIN sessions s ON s.id=c.session_id
                 WHERE css.session_id=?1",
                [session_id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, u64>(8)?,
                        row.get::<_, Option<String>>(9)?,
                    ))
                },
            )
            .optional()?
            .map(|row| {
                Ok(crate::closure_kernel::ClosureLaunchSelectorV1 {
                    program_id: ClosureProgramIdV1::new(
                        Uuid::parse_str(&row.0)
                            .map_err(|error| DaemonError::Store(error.to_string()))?,
                    ),
                    source_id: ClosureSourceIdV1::new(
                        Uuid::parse_str(&row.1)
                            .map_err(|error| DaemonError::Store(error.to_string()))?,
                    ),
                    base_sha: ClosureGitShaV1::parse(row.2).map_err(DaemonError::Store)?,
                    destination_ref: ClosureLocalBranchRefV1::parse(row.3)
                        .map_err(DaemonError::Store)?,
                    destination_pre_head: ClosureGitShaV1::parse(row.4)
                        .map_err(DaemonError::Store)?,
                    staging_ref: ClosureLocalBranchRefV1::parse(row.5)
                        .map_err(DaemonError::Store)?,
                    lineage_root_session_id: Some(
                        Uuid::parse_str(&row.6)
                            .map_err(|error| DaemonError::Store(error.to_string()))?,
                    ),
                    custody_id: Some(
                        Uuid::parse_str(&row.7)
                            .map_err(|error| DaemonError::Store(error.to_string()))?,
                    ),
                    custody_generation: Some(row.8),
                    model_invocation_id: row
                        .9
                        .map(|raw| {
                            Uuid::parse_str(&raw)
                                .map_err(|error| DaemonError::Store(error.to_string()))
                        })
                        .transpose()?,
                })
            })
            .transpose()
    }

    pub(crate) fn closure_source_created_at(
        &self,
        source_id: ClosureSourceIdV1,
    ) -> Result<DateTime<Utc>> {
        let raw: String = self.conn.query_row(
            "SELECT created_at FROM closure_sources WHERE id=?1",
            [source_id.to_string()],
            |row| row.get(0),
        )?;
        Ok(DateTime::parse_from_rfc3339(&raw)
            .map_err(|error| DaemonError::Store(error.to_string()))?
            .with_timezone(&Utc))
    }

    pub(crate) fn closure_source_worktree_root(
        &self,
        source_id: ClosureSourceIdV1,
    ) -> Result<String> {
        self.conn
            .query_row(
                "SELECT c.sandbox_root FROM closure_sources s
                 JOIN sandbox_custody_roots c ON c.custody_id=s.custody_id
                 WHERE s.id=?1 AND c.state='live' AND c.validation_state='verified'
                   AND c.validated_generation=c.generation AND c.generation=s.custody_generation",
                [source_id.to_string()],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| {
                DaemonError::InvalidParam(
                    "Closure source no longer has verified live sandbox custody".into(),
                )
            })
    }

    pub(crate) fn closure_program_policy_digests(
        &self,
        program_id: ClosureProgramIdV1,
    ) -> Result<(Sha256Digest, Sha256Digest, Sha256Digest)> {
        let row = self.conn.query_row(
            "SELECT review_policy_digest,verification_policy_digest,verifier_policy_digest
             FROM closure_programs WHERE id=?1",
            [program_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )?;
        Ok((
            Sha256Digest::parse(row.0).map_err(DaemonError::Store)?,
            Sha256Digest::parse(row.1).map_err(DaemonError::Store)?,
            Sha256Digest::parse(row.2).map_err(DaemonError::Store)?,
        ))
    }

    pub(crate) fn closure_evidence_admission_context(
        &self,
        source_id: ClosureSourceIdV1,
        reviewer_session_id: Uuid,
        expected_reviewer_invocation_id: Uuid,
    ) -> Result<ClosureEvidenceAdmissionContextV1> {
        let source = self.conn.query_row(
            "SELECT s.program_id,s.state,s.source_head,s.source_ref,sc.sandbox_root,
                    p.repository_root,p.review_policy_json,p.verification_policy_json,
                    p.review_policy_digest,p.verification_policy_digest,p.verifier_policy_digest
             FROM closure_sources s JOIN closure_programs p ON p.id=s.program_id
             JOIN sandbox_custody_roots sc ON sc.custody_id=s.custody_id
             WHERE s.id=?1",
            [source_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, String>(10)?,
                ))
            },
        )?;
        let sealed_source_head = source
            .2
            .ok_or_else(|| DaemonError::InvalidParam("Closure source head is not sealed".into()))?;
        let reviewer = self
            .conn
            .query_row(
                "SELECT s.status,s.provider,COALESCE(s.model,mi.model),s.model_invocation_id,s.sandbox_custody_id,
                    c.generation,c.sandbox_root,c.sandbox_branch,c.state,c.source_commit,mi.status
             FROM sessions s JOIN sandbox_custody_roots c ON c.custody_id=s.sandbox_custody_id
             JOIN model_invocations mi ON mi.id=s.model_invocation_id AND mi.session_id=s.id
             WHERE s.id=?1 AND c.validation_state='verified'
               AND c.validated_generation=c.generation",
                [reviewer_session_id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, u64>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, String>(8)?,
                        row.get::<_, String>(9)?,
                        row.get::<_, String>(10)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| {
                DaemonError::InvalidParam("reviewer session has no durable sandbox custody".into())
            })?;
        if reviewer.0 != "Completed" || reviewer.8 != "live" {
            return Err(DaemonError::InvalidParam(
                "reviewer session/custody is not completed and live".into(),
            ));
        }
        if reviewer.9 != sealed_source_head {
            return Err(DaemonError::InvalidParam(
                "reviewer custody was not allocated from the sealed Closure source head".into(),
            ));
        }
        if reviewer.10 != "completed" {
            return Err(DaemonError::InvalidParam(
                "reviewer model invocation is not completed".into(),
            ));
        }
        if reviewer.3.as_deref() != Some(expected_reviewer_invocation_id.to_string().as_str()) {
            return Err(DaemonError::InvalidParam(
                "reviewer model invocation does not match persistence".into(),
            ));
        }
        let same_lineage: i64 = self.conn.query_row(
            "SELECT count(*) FROM closure_source_sessions WHERE source_id=?1 AND session_id=?2",
            params![source_id.to_string(), reviewer_session_id.to_string()],
            |row| row.get(0),
        )?;
        if same_lineage != 0 {
            return Err(DaemonError::InvalidParam(
                "reviewer belongs to the Closure source lineage".into(),
            ));
        }
        let same_custody: i64 = self.conn.query_row(
            "SELECT count(*) FROM closure_sources WHERE id=?1 AND custody_id=?2",
            params![source_id.to_string(), reviewer.4],
            |row| row.get(0),
        )?;
        if same_custody != 0 {
            return Err(DaemonError::InvalidParam(
                "reviewer must use custody distinct from the Closure source".into(),
            ));
        }
        let handoff = self
            .conn
            .query_row(
                "SELECT e.id,e.content FROM conversation_events e
             JOIN conversation_event_provenance p ON p.conversation_event_id=e.id
             WHERE e.session_id=?1 AND e.event_type='Message' AND e.role='Assistant'
               AND trim(e.content)!='' AND p.producer_kind='provider_assistant_output'
               AND p.model_invocation_id=?2 ORDER BY e.sequence DESC,e.id DESC LIMIT 1",
                params![
                    reviewer_session_id.to_string(),
                    expected_reviewer_invocation_id.to_string()
                ],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?
            .ok_or_else(|| {
                DaemonError::InvalidParam("reviewer has no provider-authored review handoff".into())
            })?;
        Ok(ClosureEvidenceAdmissionContextV1 {
            program_id: ClosureProgramIdV1::new(
                Uuid::parse_str(&source.0)
                    .map_err(|error| DaemonError::Store(error.to_string()))?,
            ),
            source_state: parse_json(&format!("\"{}\"", source.1), "Closure source state")?,
            sealed_source_head: ClosureGitShaV1::parse(sealed_source_head)
                .map_err(DaemonError::Store)?,
            source_ref: ClosureLocalBranchRefV1::parse(source.3).map_err(DaemonError::Store)?,
            source_worktree_root: source.4,
            repository_root: source.5,
            review_policy: parse_json(&source.6, "Closure review policy")?,
            verification_policy: parse_json(&source.7, "Closure verification policy")?,
            review_policy_digest: Sha256Digest::parse(source.8).map_err(DaemonError::Store)?,
            verification_policy_digest: Sha256Digest::parse(source.9)
                .map_err(DaemonError::Store)?,
            verifier_policy_digest: Sha256Digest::parse(source.10).map_err(DaemonError::Store)?,
            reviewer_status: reviewer.0,
            reviewer_provider: parse_json(&format!("\"{}\"", reviewer.1), "reviewer provider")?,
            reviewer_model: reviewer.2,
            reviewer_model_invocation_id: expected_reviewer_invocation_id,
            reviewer_custody_id: Uuid::parse_str(&reviewer.4)
                .map_err(|error| DaemonError::Store(error.to_string()))?,
            reviewer_custody_generation: reviewer.5,
            reviewer_worktree_root: reviewer.6,
            reviewer_branch: reviewer.7,
            review_handoff_event_id: handoff.0,
            review_handoff_raw: handoff.1,
        })
    }

    pub(crate) fn record_closure_evidence(
        &self,
        prepared: &PreparedClosureEvidenceV1,
    ) -> Result<ClosureEvidenceResultV1> {
        let request = &prepared.request;
        let fingerprint = closure_request_fingerprint("RecordClosureEvidence", request)
            .map_err(DaemonError::InvalidParam)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if let Some((stored_fingerprint, result_json)) =
            operator_replay(&tx, "RecordClosureEvidence", request.idempotency_key)?
        {
            let mut result: ClosureEvidenceResultV1 =
                parse_json(&result_json, "RecordClosureEvidence result")?;
            result.replayed = true;
            if stored_fingerprint != fingerprint {
                result.refusal = Some(ClosureRefusalCodeV1::IdempotencyMismatch);
                result.evidence = None;
                result.eligible = false;
                result.integration_queue_item_id = None;
            }
            return Ok(result);
        }
        let source: (String, Option<String>, String, String, String, String) = tx.query_row(
            "SELECT s.program_id,s.source_head,s.state,p.review_policy_digest,
                    p.verification_policy_digest,p.verifier_policy_digest
             FROM closure_sources s JOIN closure_programs p ON p.id=s.program_id
             WHERE s.id=?1",
            [request.source_id.to_string()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )?;
        if source.1.as_deref() != Some(request.expected_sealed_source_head.as_str()) {
            return commit_closure_evidence_refusal(
                tx,
                request,
                &fingerprint,
                ClosureRefusalCodeV1::StaleReviewedSha,
            );
        }
        let evidence = &prepared.evidence;
        if source.2 != "awaiting_evidence"
            || evidence.source_id != request.source_id
            || evidence.reviewed_source_head != request.expected_sealed_source_head
            || evidence.review_policy_digest.as_str() != source.3
            || evidence.verification_policy_digest.as_str() != source.4
            || evidence.verifier_policy_digest.as_str() != source.5
        {
            return commit_closure_evidence_refusal(
                tx,
                request,
                &fingerprint,
                ClosureRefusalCodeV1::EvidenceNotEligible,
            );
        }
        if let (
            Some(reviewer_session_id),
            Some(reviewer_model_invocation_id),
            Some(reviewer_custody_id),
            Some(reviewer_custody_generation),
            Some(review_handoff_event_id),
        ) = (
            evidence.reviewer_session_id,
            evidence.reviewer_model_invocation_id,
            evidence.reviewer_custody_id,
            evidence.reviewer_custody_generation,
            evidence.review_handoff_event_id,
        ) {
            let reviewer: Option<(String, Option<String>, String, Option<String>, u64, String)> =
                tx.query_row(
                    "SELECT s.status,s.model_invocation_id,mi.status,s.sandbox_custody_id,
                            c.generation,c.state
                     FROM sessions s
                     JOIN model_invocations mi ON mi.id=s.model_invocation_id AND mi.session_id=s.id
                     JOIN sandbox_custody_roots c ON c.custody_id=s.sandbox_custody_id
                     WHERE s.id=?1 AND c.validation_state='verified'
                       AND c.validated_generation=c.generation",
                    [reviewer_session_id.to_string()],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                        ))
                    },
                )
                .optional()?;
            let handoff_still_final = tx
                .query_row(
                    "SELECT e.id FROM conversation_events e
                     JOIN conversation_event_provenance p ON p.conversation_event_id=e.id
                     WHERE e.session_id=?1 AND e.event_type='Message' AND e.role='Assistant'
                       AND trim(e.content)!='' AND p.producer_kind='provider_assistant_output'
                       AND p.model_invocation_id=?2
                     ORDER BY e.sequence DESC,e.id DESC LIMIT 1",
                    params![
                        reviewer_session_id.to_string(),
                        reviewer_model_invocation_id.to_string()
                    ],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?;
            let reviewer_valid = reviewer.is_some_and(|row| {
                row.0 == "Completed"
                    && row.1.as_deref() == Some(reviewer_model_invocation_id.to_string().as_str())
                    && row.2 == "completed"
                    && row.3.as_deref() == Some(reviewer_custody_id.to_string().as_str())
                    && row.4 == reviewer_custody_generation
                    && row.5 == "live"
            });
            if !reviewer_valid || handoff_still_final != Some(review_handoff_event_id) {
                return commit_closure_evidence_refusal(
                    tx,
                    request,
                    &fingerprint,
                    ClosureRefusalCodeV1::InvalidEvidenceCustody,
                );
            }
        } else if matches!(
            request.disposition,
            ClosureEvidenceDispositionV1::RequiredIndependent { .. }
        ) {
            return commit_closure_evidence_refusal(
                tx,
                request,
                &fingerprint,
                ClosureRefusalCodeV1::InvalidEvidenceCustody,
            );
        }
        let disposition = match request.disposition {
            ClosureEvidenceDispositionV1::RequiredIndependent { .. } => "required_independent",
            ClosureEvidenceDispositionV1::NotRequired {
                basis: ClosureReviewNotRequiredBasisV1::Tier0Deterministic,
                ..
            } => "not_required_tier0_deterministic",
            ClosureEvidenceDispositionV1::NotRequired {
                basis: ClosureReviewNotRequiredBasisV1::ProvenNoChange,
                ..
            } => "not_required_proven_no_change",
        };
        let manifest_source = evidence
            .manifest_source_head
            .as_ref()
            .map(ClosureGitShaV1::as_str);
        let now = Utc::now();
        tx.execute(
            "INSERT INTO closure_evidence
             (id,source_id,disposition,reviewed_source_head,review_schema_version,review_raw_bytes,review_digest,
              normalized_review_json,finding_set_digest,reviewer_session_id,reviewer_model_invocation_id,
              reviewer_provider,reviewer_model,reviewer_custody_id,reviewer_custody_generation,
              evidence_commit_sha,evidence_parent_sha,review_json_path,manifest_v2_path,
              review_handoff_event_id,review_handoff_digest,manifest_schema_version,manifest_raw_bytes,
              manifest_digest,manifest_source_head,source_ref_before,source_ref_after,
              source_worktree_head_before,source_worktree_head_after,review_policy_digest,
              verification_policy_digest,verifier_policy_digest,accepted_by,accepted_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26,?27,?28,?29,?30,?31,?32,'operator',?33)",
            params![
                evidence.evidence_id.to_string(), request.source_id.to_string(), disposition,
                evidence.reviewed_source_head.as_str(), evidence.review_schema_version,
                prepared.review_raw_bytes, evidence.review_digest, prepared.normalized_review_json,
                evidence.finding_set_digest, evidence.reviewer_session_id.map(|v| v.to_string()),
                evidence.reviewer_model_invocation_id.map(|v| v.to_string()),
                evidence.reviewer_provider.map(|v| wire(&v)).transpose()?, evidence.reviewer_model,
                evidence.reviewer_custody_id.map(|v| v.to_string()), evidence.reviewer_custody_generation,
                evidence.evidence_commit.as_ref().map(ClosureGitShaV1::as_str),
                matches!(request.disposition, ClosureEvidenceDispositionV1::RequiredIndependent { .. })
                    .then_some(evidence.reviewed_source_head.as_str()),
                match &request.disposition { ClosureEvidenceDispositionV1::RequiredIndependent { review_json_path, .. } => Some(review_json_path.as_str()), _ => None },
                match &request.disposition { ClosureEvidenceDispositionV1::RequiredIndependent { manifest_v2_path, .. } => Some(manifest_v2_path.as_str()), _ => None },
                evidence.review_handoff_event_id, evidence.review_handoff_digest,
                evidence.manifest_schema_version, prepared.manifest_raw_bytes,
                evidence.manifest_digest, manifest_source, evidence.source_ref_before.as_str(),
                evidence.source_ref_after.as_str(), evidence.source_worktree_head_before.as_str(),
                evidence.source_worktree_head_after.as_str(),
                evidence.review_policy_digest.to_string(), evidence.verification_policy_digest.to_string(), evidence.verifier_policy_digest.to_string(), timestamp(now),
            ],
        )?;
        let queue_id = Uuid::new_v4();
        tx.execute(
            "INSERT INTO closure_integration_queue(id,source_id,evidence_id,source_head,state,created_at,updated_at)
             VALUES (?1,?2,?3,?4,'eligible_k2',?5,?5)",
            params![queue_id.to_string(), request.source_id.to_string(), evidence.evidence_id.to_string(), request.expected_sealed_source_head.as_str(), timestamp(now)],
        )?;
        let source_updated = tx.execute(
            "UPDATE closure_sources SET state='integration_queued',updated_at=?2
             WHERE id=?1 AND state='awaiting_evidence'",
            params![request.source_id.to_string(), timestamp(now)],
        )?;
        if source_updated != 1 {
            return Err(DaemonError::Store(
                "Closure source changed before evidence admission commit".into(),
            ));
        }
        tx.execute("UPDATE closure_programs SET state='awaiting_integration',version=version+1,updated_at=?2 WHERE id=?1", params![source.0, timestamp(now)])?;
        let result = ClosureEvidenceResultV1 {
            evidence: Some(evidence.clone()),
            eligible: true,
            integration_queue_item_id: Some(queue_id),
            replayed: false,
            refusal: None,
        };
        insert_operator_request(
            &tx,
            "RecordClosureEvidence",
            request.idempotency_key,
            &fingerprint,
            request,
            &result,
            now,
        )?;
        let program_id = ClosureProgramIdV1::new(
            Uuid::parse_str(&source.0).map_err(|error| DaemonError::Store(error.to_string()))?,
        );
        insert_closure_event(
            &tx,
            program_id,
            Some(request.source_id),
            "evidence_admitted",
            Some("awaiting_evidence"),
            "awaiting_integration",
            Some("awaiting_evidence"),
            Some("integration_queued"),
            Some(request.idempotency_key.to_string()),
            serde_json::json!({"evidence_id": evidence.evidence_id, "queue_id": queue_id}),
            now,
        )?;
        tx.commit()?;
        Ok(result)
    }
}

type ProgramConfigRow = (
    u64,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    Option<String>,
);

fn map_program_config(
    program_id: ClosureProgramIdV1,
    row: ProgramConfigRow,
) -> Result<(ClosureProgramSummaryV1, ClosureProgramConfigV1)> {
    let source_id = row
        .15
        .as_deref()
        .map(Uuid::parse_str)
        .transpose()
        .map_err(|error| DaemonError::Store(error.to_string()))?
        .map(ClosureSourceIdV1::new);
    let config = ClosureProgramConfigV1 {
        repository_root: row.1.clone().into(),
        base_ref: ClosureLocalBranchRefV1::parse(row.3.clone()).map_err(DaemonError::Store)?,
        destination_ref: ClosureLocalBranchRefV1::parse(row.5.clone())
            .map_err(DaemonError::Store)?,
        review_policy: parse_json(&row.9, "Closure review policy")?,
        verification_policy: parse_json(&row.10, "Closure verification policy")?,
        verifier_policy: parse_json(&row.11, "Closure verifier policy")?,
        checkout_remediation_policy: parse_json(
            &format!("\"{}\"", row.12),
            "Closure checkout policy",
        )?,
    };
    Ok((
        ClosureProgramSummaryV1 {
            program_id,
            version: row.0,
            repository_identity: ClosureRepositoryIdentityV1::parse(row.2)
                .map_err(DaemonError::Store)?,
            base_ref: config.base_ref.clone(),
            base_sha: ClosureGitShaV1::parse(row.4).map_err(DaemonError::Store)?,
            destination_ref: config.destination_ref.clone(),
            destination_pre_head: ClosureGitShaV1::parse(row.6).map_err(DaemonError::Store)?,
            destination_claim_state: parse_json(
                &format!("\"{}\"", row.8),
                "Closure destination claim",
            )?,
            state: parse_json(&format!("\"{}\"", row.7), "Closure program state")?,
            source_id,
            created_at: DateTime::parse_from_rfc3339(&row.13)
                .map_err(|error| DaemonError::Store(error.to_string()))?
                .with_timezone(&Utc),
            updated_at: DateTime::parse_from_rfc3339(&row.14)
                .map_err(|error| DaemonError::Store(error.to_string()))?
                .with_timezone(&Utc),
        },
        config,
    ))
}

fn wire<T: Serialize>(value: &T) -> Result<String> {
    let value = serde_json::to_value(value)?;
    value
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| DaemonError::Store("expected string enum serialization".into()))
}

fn operator_replay(
    tx: &Transaction<'_>,
    method: &str,
    key: Uuid,
) -> Result<Option<(String, String)>> {
    tx.query_row("SELECT request_fingerprint,result_json FROM closure_operator_requests WHERE method=?1 AND idempotency_key=?2", params![method,key.to_string()], |row| Ok((row.get(0)?,row.get(1)?))).optional().map_err(Into::into)
}

fn load_source_launch_reservation(
    tx: &Transaction<'_>,
    idempotency_key: Uuid,
) -> Result<Option<ClosureSourceLaunchReservationV1>> {
    tx.query_row(
        "SELECT program_id,source_id,root_session_id,custody_id,custody_generation,
                branch_name,repository_identity,source_ref,source_base_sha,destination_ref,
                destination_pre_head,staging_ref,request_fingerprint
         FROM closure_source_launch_reservations WHERE idempotency_key=?1",
        [idempotency_key.to_string()],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, u64>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, String>(8)?,
                row.get::<_, String>(9)?,
                row.get::<_, String>(10)?,
                row.get::<_, String>(11)?,
                row.get::<_, String>(12)?,
            ))
        },
    )
    .optional()?
    .map(|row| {
        let program_id =
            ClosureProgramIdV1::new(parse_canonical_uuid(&row.0).map_err(DaemonError::Store)?);
        let source_id =
            ClosureSourceIdV1::new(parse_canonical_uuid(&row.1).map_err(DaemonError::Store)?);
        let root_session_id = parse_canonical_uuid(&row.2).map_err(DaemonError::Store)?;
        Ok(ClosureSourceLaunchReservationV1 {
            source: ClosureSourceIdentityV1 {
                program_id,
                source_id,
                custody_id: parse_canonical_uuid(&row.3).map_err(DaemonError::Store)?,
                custody_generation: row.4,
                lineage_root_session_id: root_session_id,
                repository_identity: ClosureRepositoryIdentityV1::parse(row.6)
                    .map_err(DaemonError::Store)?,
                source_ref: ClosureLocalBranchRefV1::parse(row.7).map_err(DaemonError::Store)?,
                source_base_sha: ClosureGitShaV1::parse(row.8).map_err(DaemonError::Store)?,
                destination_ref: ClosureLocalBranchRefV1::parse(row.9)
                    .map_err(DaemonError::Store)?,
                destination_pre_head: ClosureGitShaV1::parse(row.10).map_err(DaemonError::Store)?,
                staging_ref: ClosureLocalBranchRefV1::parse(row.11).map_err(DaemonError::Store)?,
            },
            root_session_id,
            branch_name: row.5,
            idempotency_key,
            request_fingerprint: row.12,
        })
    })
    .transpose()
}

fn insert_operator_request<T: Serialize, R: Serialize>(
    tx: &Transaction<'_>,
    method: &str,
    key: Uuid,
    fingerprint: &str,
    request: &T,
    result: &R,
    now: DateTime<Utc>,
) -> Result<()> {
    insert_operator_request_raw(tx, method, key, fingerprint, request, result, now)
}

fn commit_closure_evidence_refusal(
    tx: Transaction<'_>,
    request: &RecordClosureEvidenceRequestV1,
    fingerprint: &str,
    refusal: ClosureRefusalCodeV1,
) -> Result<ClosureEvidenceResultV1> {
    let result = ClosureEvidenceResultV1 {
        evidence: None,
        eligible: false,
        integration_queue_item_id: None,
        replayed: false,
        refusal: Some(refusal),
    };
    insert_operator_request(
        &tx,
        "RecordClosureEvidence",
        request.idempotency_key,
        fingerprint,
        request,
        &result,
        Utc::now(),
    )?;
    tx.commit()?;
    Ok(result)
}

fn insert_operator_request_raw<T: Serialize, R: Serialize>(
    tx: &Transaction<'_>,
    method: &str,
    key: Uuid,
    fingerprint: &str,
    request: &T,
    result: &R,
    now: DateTime<Utc>,
) -> Result<()> {
    tx.execute("INSERT INTO closure_operator_requests(id,method,idempotency_key,request_fingerprint,request_json,result_json,created_at) VALUES (?1,?2,?3,?4,?5,?6,?7)", params![Uuid::new_v4().to_string(),method,key.to_string(),fingerprint,canonical_program_run_json(request).map_err(DaemonError::InvalidParam)?,canonical_program_run_json(result).map_err(DaemonError::InvalidParam)?,timestamp(now)])?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn insert_closure_event(
    tx: &Transaction<'_>,
    program_id: ClosureProgramIdV1,
    source_id: Option<ClosureSourceIdV1>,
    event_kind: &str,
    prior_program_state: Option<&str>,
    next_program_state: &str,
    prior_source_state: Option<&str>,
    next_source_state: Option<&str>,
    correlation_key: Option<String>,
    payload: serde_json::Value,
    now: DateTime<Utc>,
) -> Result<()> {
    let sequence: i64 = tx.query_row(
        "SELECT COALESCE(MAX(sequence),0)+1 FROM closure_events WHERE program_id=?1",
        [program_id.to_string()],
        |row| row.get(0),
    )?;
    tx.execute("INSERT INTO closure_events(id,program_id,source_id,sequence,event_kind,prior_program_state,next_program_state,prior_source_state,next_source_state,correlation_key,payload_json,occurred_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)", params![Uuid::new_v4().to_string(),program_id.to_string(),source_id.map(|v| v.to_string()),sequence,event_kind,prior_program_state,next_program_state,prior_source_state,next_source_state,correlation_key,canonical_program_run_json(&payload).map_err(DaemonError::InvalidParam)?,timestamp(now)])?;
    Ok(())
}

fn revalidate_capture_key(
    tx: &Transaction<'_>,
    proposal: &ClosureOutputCaptureProposalV1,
) -> Result<()> {
    let row: (String, u64, u32, String, Option<String>, Option<String>) = tx.query_row(
        "SELECT css.custody_id,css.custody_generation,css.rotation_depth,s.status,
                s.model_invocation_id,mi.status
         FROM closure_source_sessions css JOIN sessions s ON s.id=css.session_id
         LEFT JOIN model_invocations mi ON mi.id=s.model_invocation_id AND mi.session_id=s.id
         WHERE css.source_id=?1 AND css.session_id=?2",
        params![
            proposal.key.source_id.to_string(),
            proposal.key.tip_session_id.to_string()
        ],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        },
    )?;
    if row.0 != proposal.expected_custody_id.to_string()
        || row.1 != proposal.expected_custody_generation
        || row.2 != proposal.expected_rotation_depth
        || row.3 != proposal.terminal_status
        || row.4.as_deref() != Some(proposal.key.model_invocation_id.to_string().as_str())
        || !matches!(row.5.as_deref(), Some("completed" | "failed" | "cancelled"))
    {
        return Err(DaemonError::Store(
            "Closure capture identity changed before commit".into(),
        ));
    }
    let max_depth: u32 = tx.query_row(
        "SELECT max(rotation_depth) FROM closure_source_sessions WHERE source_id=?1",
        [proposal.key.source_id.to_string()],
        |row| row.get(0),
    )?;
    if max_depth != proposal.expected_rotation_depth {
        return Err(DaemonError::Store(
            "Closure lineage tip changed before commit".into(),
        ));
    }
    let source_identity: (String, u64, String) = tx.query_row(
        "SELECT custody_id,custody_generation,root_session_id
         FROM closure_sources WHERE id=?1 AND state IN ('working','outcome_blocked')",
        [proposal.key.source_id.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    if let Some(issue) = validate_lineage_rows(
        tx,
        proposal.key.source_id,
        &source_identity.0,
        source_identity.1,
        &source_identity.2,
    )? {
        if proposal.disposition != ClosureOutputValidationDispositionV1::BlockedAmbiguousLineage {
            return Err(DaemonError::Store(format!(
                "Closure lineage became ambiguous before commit: {issue}"
            )));
        }
    } else {
        let successor_count: i64 = tx.query_row(
            "SELECT count(*) FROM sessions WHERE continued_from=?1",
            [proposal.key.tip_session_id.to_string()],
            |row| row.get(0),
        )?;
        if successor_count != 0
            && proposal.disposition != ClosureOutputValidationDispositionV1::BlockedAmbiguousLineage
        {
            return Err(DaemonError::Store(
                "Closure lineage tip changed before commit".into(),
            ));
        }
    }
    match proposal.event_id {
        Some(event_id) => {
            let event: (String, i32, String, String, String, String, String, String) = tx
                .query_row(
                    "SELECT e.session_id,e.sequence,e.event_type,e.role,p.producer_kind,
                        p.model_invocation_id,p.provider_event_type,e.content
                 FROM conversation_events e
                 JOIN conversation_event_provenance p ON p.conversation_event_id=e.id
                 WHERE e.id=?1",
                    [event_id],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                            row.get(6)?,
                            row.get(7)?,
                        ))
                    },
                )?;
            if event.0 != proposal.key.tip_session_id.to_string()
                || Some(event.1) != proposal.event_sequence
                || event.2 != "Message"
                || event.3 != "Assistant"
                || event.4 != "provider_assistant_output"
                || event.5 != proposal.key.model_invocation_id.to_string()
                || Some(event.6.as_str()) != proposal.provider_event_type.as_deref()
                || Some(event.7.as_str()) != proposal.raw_handoff.as_deref()
            {
                return Err(DaemonError::Store(
                    "Closure provider event evidence changed before commit".into(),
                ));
            }
        }
        None if proposal.event_sequence.is_some() || proposal.provider_event_type.is_some() => {
            return Err(DaemonError::Store(
                "Closure missing-output claim carried event evidence".into(),
            ));
        }
        None => {}
    }
    Ok(())
}

fn validate_lineage_rows(
    connection: &rusqlite::Connection,
    source_id: ClosureSourceIdV1,
    expected_custody_id: &str,
    expected_custody_generation: u64,
    expected_root_session_id: &str,
) -> Result<Option<String>> {
    let mut statement = connection.prepare(
        "SELECT css.session_id,css.rotation_depth,css.custody_id,css.custody_generation,
                css.continued_from_session_id,s.continued_from,s.sandbox_custody_id
         FROM closure_source_sessions css
         JOIN sessions s ON s.id=css.session_id
         WHERE css.source_id=?1 ORDER BY css.rotation_depth,css.session_id",
    )?;
    let rows = statement
        .query_map([source_id.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, u32>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, u64>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if rows.is_empty() || rows[0].0 != expected_root_session_id || rows[0].1 != 0 {
        return Ok(Some("root binding is missing or changed".into()));
    }
    let mut prior: Option<&str> = None;
    let mut prior_generation: Option<u64> = None;
    for (index, row) in rows.iter().enumerate() {
        if row.1 as usize != index
            || row.2 != expected_custody_id
            || row.6.as_deref() != Some(expected_custody_id)
        {
            return Ok(Some(format!("invalid binding at rotation depth {}", row.1)));
        }
        if index == 0 {
            if row.4.is_some() || row.5.is_some() {
                return Ok(Some("root session unexpectedly has a predecessor".into()));
            }
        } else if row.4.as_deref() != prior || row.5.as_deref() != prior {
            return Ok(Some(format!(
                "rotation predecessor mismatch at depth {}",
                row.1
            )));
        }
        if let Some(prior_generation) = prior_generation
            && row.3 != prior_generation.saturating_add(1)
        {
            return Ok(Some(format!(
                "custody generation gap at rotation depth {}",
                row.1
            )));
        }
        if index + 1 < rows.len() {
            let mut children = connection.prepare(
                "SELECT id,sandbox_custody_id FROM sessions WHERE continued_from=?1 ORDER BY id",
            )?;
            let children = children
                .query_map([row.0.as_str()], |child| {
                    Ok((
                        child.get::<_, String>(0)?,
                        child.get::<_, Option<String>>(1)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            if children.len() != 1
                || children[0].0 != rows[index + 1].0
                || children[0].1.as_deref() != Some(expected_custody_id)
            {
                return Ok(Some(format!(
                    "forked or missing successor at rotation depth {}",
                    row.1
                )));
            }
        } else {
            let successor_count: i64 = connection.query_row(
                "SELECT count(*) FROM sessions WHERE continued_from=?1",
                [row.0.as_str()],
                |child| child.get(0),
            )?;
            if successor_count > 1 {
                return Ok(Some(format!(
                    "forked terminal lineage at rotation depth {}",
                    row.1
                )));
            }
        }
        prior = Some(row.0.as_str());
        prior_generation = Some(row.3);
    }
    if prior_generation != Some(expected_custody_generation) {
        return Ok(Some(
            "tip custody generation does not match the Closure source".into(),
        ));
    }
    Ok(None)
}

fn load_output_validation(
    connection: &rusqlite::Connection,
    key: &ClosureOutputIngestionKeyV1,
) -> Result<Option<ClosureOutputValidationResultV1>> {
    let row = connection.query_row(
        "SELECT id,conversation_event_id,conversation_sequence,disposition,source_state,
                normalized_envelope_json,normalized_envelope_digest,validation_issues_json,
                (SELECT id FROM closure_integration_queue q WHERE q.source_id=closure_output_validations.source_id)
         FROM closure_output_validations WHERE source_id=?1 AND tip_session_id=?2 AND model_invocation_id=?3",
        params![key.source_id.to_string(),key.tip_session_id.to_string(),key.model_invocation_id.to_string()],
        |row| Ok((row.get::<_,String>(0)?,row.get::<_,Option<i64>>(1)?,row.get::<_,Option<i32>>(2)?,row.get::<_,String>(3)?,row.get::<_,String>(4)?,row.get::<_,Option<String>>(5)?,row.get::<_,Option<String>>(6)?,row.get::<_,String>(7)?,row.get::<_,Option<String>>(8)?)),
    ).optional()?;
    row.map(|row| {
        Ok(ClosureOutputValidationResultV1 {
            validation_id: Uuid::parse_str(&row.0)
                .map_err(|error| DaemonError::Store(error.to_string()))?,
            key: key.clone(),
            conversation_event_id: row.1,
            conversation_sequence: row.2,
            disposition: parse_json(&format!("\"{}\"", row.3), "Closure output disposition")?,
            source_state: parse_json(&format!("\"{}\"", row.4), "Closure source state")?,
            normalized_envelope: row
                .5
                .as_deref()
                .map(|raw| parse_json(raw, "Closure output envelope"))
                .transpose()?,
            normalized_digest: row.6,
            issues: parse_json(&row.7, "Closure validation issues")?,
            replayed: false,
            integration_queue_item_id: row
                .8
                .map(|raw| {
                    Uuid::parse_str(&raw).map_err(|error| DaemonError::Store(error.to_string()))
                })
                .transpose()?,
        })
    })
    .transpose()
}

fn load_latest_output_validation(
    connection: &rusqlite::Connection,
    source_id: ClosureSourceIdV1,
) -> Result<Option<ClosureOutputValidationResultV1>> {
    let key = connection
        .query_row(
            "SELECT tip_session_id,model_invocation_id
             FROM closure_output_validations WHERE source_id=?1
             ORDER BY created_at DESC,id DESC LIMIT 1",
            [source_id.to_string()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?
        .map(|row| -> Result<ClosureOutputIngestionKeyV1> {
            Ok(ClosureOutputIngestionKeyV1 {
                source_id,
                tip_session_id: Uuid::parse_str(&row.0)
                    .map_err(|error| DaemonError::Store(error.to_string()))?,
                model_invocation_id: Uuid::parse_str(&row.1)
                    .map_err(|error| DaemonError::Store(error.to_string()))?,
            })
        })
        .transpose()?;
    key.as_ref()
        .map(|key| load_output_validation(connection, key))
        .transpose()
        .map(Option::flatten)
}

#[derive(Debug)]
struct StoredClosureEvidenceRow {
    id: String,
    reviewed_source_head: String,
    review_schema_version: Option<u32>,
    review_digest: Option<String>,
    finding_set_digest: Option<String>,
    manifest_schema_version: Option<u32>,
    manifest_digest: Option<String>,
    manifest_source_head: Option<String>,
    reviewer_session_id: Option<String>,
    reviewer_model_invocation_id: Option<String>,
    reviewer_provider: Option<String>,
    reviewer_model: Option<String>,
    reviewer_custody_id: Option<String>,
    reviewer_custody_generation: Option<u64>,
    evidence_commit: Option<String>,
    review_handoff_event_id: Option<i64>,
    review_handoff_digest: Option<String>,
    source_ref_before: String,
    source_ref_after: String,
    source_worktree_head_before: String,
    source_worktree_head_after: String,
    review_policy_digest: String,
    verification_policy_digest: String,
    verifier_policy_digest: String,
    accepted_at: String,
}

fn load_accepted_evidence(
    connection: &rusqlite::Connection,
    source_id: ClosureSourceIdV1,
) -> Result<Option<AcceptedReviewEvidenceV1>> {
    let row = connection
        .query_row(
            "SELECT id,reviewed_source_head,review_schema_version,review_digest,finding_set_digest,
                    manifest_schema_version,manifest_digest,manifest_source_head,reviewer_session_id,
                    reviewer_model_invocation_id,reviewer_provider,reviewer_model,reviewer_custody_id,
                    reviewer_custody_generation,evidence_commit_sha,review_handoff_event_id,
                    review_handoff_digest,source_ref_before,source_ref_after,source_worktree_head_before,
                    source_worktree_head_after,review_policy_digest,verification_policy_digest,
                    verifier_policy_digest,accepted_at
             FROM closure_evidence WHERE source_id=?1",
            [source_id.to_string()],
            |row| {
                Ok(StoredClosureEvidenceRow {
                    id: row.get(0)?,
                    reviewed_source_head: row.get(1)?,
                    review_schema_version: row.get(2)?,
                    review_digest: row.get(3)?,
                    finding_set_digest: row.get(4)?,
                    manifest_schema_version: row.get(5)?,
                    manifest_digest: row.get(6)?,
                    manifest_source_head: row.get(7)?,
                    reviewer_session_id: row.get(8)?,
                    reviewer_model_invocation_id: row.get(9)?,
                    reviewer_provider: row.get(10)?,
                    reviewer_model: row.get(11)?,
                    reviewer_custody_id: row.get(12)?,
                    reviewer_custody_generation: row.get(13)?,
                    evidence_commit: row.get(14)?,
                    review_handoff_event_id: row.get(15)?,
                    review_handoff_digest: row.get(16)?,
                    source_ref_before: row.get(17)?,
                    source_ref_after: row.get(18)?,
                    source_worktree_head_before: row.get(19)?,
                    source_worktree_head_after: row.get(20)?,
                    review_policy_digest: row.get(21)?,
                    verification_policy_digest: row.get(22)?,
                    verifier_policy_digest: row.get(23)?,
                    accepted_at: row.get(24)?,
                })
            },
        )
        .optional()?;
    row.map(|row| {
        let parse_optional_uuid = |value: Option<String>| {
            value
                .map(|value| {
                    Uuid::parse_str(&value).map_err(|error| DaemonError::Store(error.to_string()))
                })
                .transpose()
        };
        let parse_optional_sha = |value: Option<String>| {
            value
                .map(|value| ClosureGitShaV1::parse(value).map_err(DaemonError::Store))
                .transpose()
        };
        Ok(AcceptedReviewEvidenceV1 {
            evidence_id: ClosureEvidenceIdV1::new(
                Uuid::parse_str(&row.id).map_err(|error| DaemonError::Store(error.to_string()))?,
            ),
            source_id,
            reviewed_source_head: ClosureGitShaV1::parse(row.reviewed_source_head)
                .map_err(DaemonError::Store)?,
            review_schema_version: row.review_schema_version,
            review_digest: row.review_digest,
            finding_set_digest: row.finding_set_digest,
            manifest_schema_version: row.manifest_schema_version,
            manifest_digest: row.manifest_digest,
            manifest_source_head: parse_optional_sha(row.manifest_source_head)?,
            reviewer_session_id: parse_optional_uuid(row.reviewer_session_id)?,
            reviewer_model_invocation_id: parse_optional_uuid(row.reviewer_model_invocation_id)?,
            reviewer_provider: row
                .reviewer_provider
                .map(|value| parse_json(&format!("\"{value}\""), "reviewer provider"))
                .transpose()?,
            reviewer_model: row.reviewer_model,
            reviewer_custody_id: parse_optional_uuid(row.reviewer_custody_id)?,
            reviewer_custody_generation: row.reviewer_custody_generation,
            evidence_commit: parse_optional_sha(row.evidence_commit)?,
            review_handoff_event_id: row.review_handoff_event_id,
            review_handoff_digest: row.review_handoff_digest,
            source_ref_before: ClosureGitShaV1::parse(row.source_ref_before)
                .map_err(DaemonError::Store)?,
            source_ref_after: ClosureGitShaV1::parse(row.source_ref_after)
                .map_err(DaemonError::Store)?,
            source_worktree_head_before: ClosureGitShaV1::parse(row.source_worktree_head_before)
                .map_err(DaemonError::Store)?,
            source_worktree_head_after: ClosureGitShaV1::parse(row.source_worktree_head_after)
                .map_err(DaemonError::Store)?,
            review_policy_digest: Sha256Digest::parse(row.review_policy_digest)
                .map_err(DaemonError::Store)?,
            verification_policy_digest: Sha256Digest::parse(row.verification_policy_digest)
                .map_err(DaemonError::Store)?,
            verifier_policy_digest: Sha256Digest::parse(row.verifier_policy_digest)
                .map_err(DaemonError::Store)?,
            accepted_at: DateTime::parse_from_rfc3339(&row.accepted_at)
                .map_err(|error| DaemonError::Store(error.to_string()))?
                .with_timezone(&Utc),
        })
    })
    .transpose()
}
