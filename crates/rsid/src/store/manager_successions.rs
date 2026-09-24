//! Parentless manager turnover. Logical appointment is resolved exclusively by
//! committed rotation edges; this aggregate is a durable occurrence, not a lead
//! pointer. Runtime witnesses are daemon-only, never deserialized from an RPC.

use super::Store;
use super::harness_manager_v2::{ManagerAuthorityV2, fingerprint, now, refused};
use super::manager_actions::{ManagerActionClaimV2, ManagerActionContextV2, ManagerActionOriginV2};
use super::sandbox_custody::PersistedCustody;
use crate::error::Result;
use rsi_common::harness_manager_v2::*;
use rsi_common::types::{Session, SessionKind, SessionProvider, SessionStatus};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ManagerRootState {
    Reserved,
    Executing,
    Established,
    CleanupRequired,
    Committed,
    Failed,
    Blocked,
    Revoked,
}

/// Exact source authenticated by runtime under predecessor/cwd custody guards.
/// Construction is restricted to daemon code. It is not a serialized capability;
/// Store checks its durable source projection again in the admission transaction.
#[derive(Debug, Clone)]
pub(crate) struct VerifiedManagerHandoff {
    predecessor: Session,
    handoff: ManagerCommittedHandoffV2,
    custody: Option<ManagerRootCustody>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ManagerRootCustody {
    pub custody_id: Uuid,
    pub generation: u64,
    pub root: String,
    pub branch: String,
    pub repository_identity: String,
    pub source_commit: String,
    pub owner_session_id: Uuid,
    pub allocation_session_id: Uuid,
    pub allocation_id: Uuid,
}
impl From<&PersistedCustody> for ManagerRootCustody {
    fn from(c: &PersistedCustody) -> Self {
        Self {
            custody_id: c.custody_id,
            generation: c.generation,
            root: c.sandbox_root.clone(),
            branch: c.sandbox_branch.clone(),
            repository_identity: c.repository_identity.clone(),
            source_commit: c.source_commit.clone(),
            owner_session_id: c.owner_session_id,
            allocation_session_id: c.allocation_session_id,
            allocation_id: c.allocation_id,
        }
    }
}
impl VerifiedManagerHandoff {
    /// Call only after exact HEAD/blob/type/size validation and filesystem
    /// authentication. Dirty bytes are deliberately neither read nor discarded.
    pub(crate) fn from_authenticated_source(
        predecessor: &Session,
        handoff: &ManagerCommittedHandoffV2,
        custody: Option<&PersistedCustody>,
    ) -> Result<Self> {
        handoff.validate().map_err(refused)?;
        if predecessor.parent_id.is_some() || predecessor.session_kind != SessionKind::Standard {
            return Err(refused("manager_succession_root_required"));
        }
        Ok(Self {
            predecessor: predecessor.clone(),
            handoff: handoff.clone(),
            custody: custody.map(ManagerRootCustody::from),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ManagerRootFrozen {
    pub request: AgentManagerControlRequestV2,
    pub predecessor: Session,
    pub predecessor_invocation_id: Option<Uuid>,
    pub predecessor_custody: Option<ManagerRootCustody>,
    pub handoff: ManagerCommittedHandoffV2,
    pub launch: ManagerLaunchChoiceV2,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ManagerRootSuccession {
    pub operation_id: Uuid,
    pub project_id: Uuid,
    pub manager_session_id: Uuid,
    pub predecessor_session_id: Uuid,
    pub candidate_session_id: Uuid,
    pub launch_attempt_id: Uuid,
    pub model_invocation_id: Uuid,
    pub scope_version: i64,
    pub policy_version: i64,
    pub authority_epoch: i64,
    pub frozen: ManagerRootFrozen,
    pub state: ManagerRootState,
    pub row_version: i64,
    pub claim_boot_id: Option<Uuid>,
    pub admission_recorded: bool,
    pub effect_claimed: bool,
    pub candidate_custody: Option<ManagerRootCustody>,
    pub published_epoch: Option<i64>,
}

impl ManagerRootSuccession {
    pub(crate) fn invocation_dedup_key(&self) -> String {
        format!(
            "manager.self_succession:{}:{}",
            self.operation_id, self.model_invocation_id
        )
    }

    pub(crate) fn invocation_fingerprint(&self) -> Result<String> {
        fingerprint(&serde_json::to_value(&self.frozen)?)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ManagerSuccessionClaim {
    pub action: ManagerActionClaimV2,
    pub reservation: ManagerRootSuccession,
}

/// Runtime creates this only after holding the exact predecessor spawn guard,
/// draining its monitor, excluding active/token writers, and physically settling
/// the exact process cohort. An absent active map entry alone is insufficient.
#[derive(Debug)]
pub(crate) struct ManagerPredecessorSettledWitness {
    session: Session,
    invocation_id: Option<Uuid>,
    boot_id: Uuid,
}
impl ManagerPredecessorSettledWitness {
    pub(crate) fn after_checked_drain(
        session: &Session,
        invocation_id: Option<Uuid>,
        boot_id: Uuid,
    ) -> Result<Self> {
        if !settled_status(session.status) || boot_id.is_nil() {
            return Err(refused("manager_succession_unsettled_predecessor"));
        }
        Ok(Self {
            session: session.clone(),
            invocation_id,
            boot_id,
        })
    }
}

/// Live provider establishment witness, constructed by the launch callback while
/// retaining candidate spawn/cwd custody guards. Queue or channel acceptance is
/// not establishment. No JSON constructor exists.
#[derive(Debug)]
pub(crate) struct ManagerSuccessionPublicationWitness {
    candidate_id: Uuid,
    invocation_id: Uuid,
    attempt_id: Uuid,
    boot_id: Uuid,
}
impl ManagerSuccessionPublicationWitness {
    pub(crate) fn after_provider_established(
        candidate_id: Uuid,
        invocation_id: Uuid,
        attempt_id: Uuid,
        boot_id: Uuid,
    ) -> Result<Self> {
        if [candidate_id, invocation_id, attempt_id, boot_id]
            .iter()
            .any(Uuid::is_nil)
        {
            return Err(refused("manager_succession_establishment_invalid"));
        }
        Ok(Self {
            candidate_id,
            invocation_id,
            attempt_id,
            boot_id,
        })
    }
}

#[derive(Debug)]
pub(crate) struct ManagerSuccessionCleanupWitness {
    operation_id: Uuid,
    invocation_id: Uuid,
    candidate_id: Uuid,
}
impl ManagerSuccessionCleanupWitness {
    /// Runtime must exclude live owned writers, check/reap the exact process
    /// cohort, then settle its invocation without inventing usage, in that order.
    pub(crate) fn after_checked_settlement(root: &ManagerRootSuccession) -> Self {
        Self {
            operation_id: root.operation_id,
            invocation_id: root.model_invocation_id,
            candidate_id: root.candidate_session_id,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ManagerRootResourceOrigin {
    pub session_id: Uuid,
    pub project_id: Uuid,
    pub manager_session_id: Uuid,
    pub scope_version: i64,
    pub operation_id: Option<Uuid>,
    pub predecessor_session_id: Option<Uuid>,
    pub model_invocation_id: Option<Uuid>,
    pub provider: SessionProvider,
    pub zero_origin: bool,
    pub known_floor_usd: f64,
}

fn settled_status(status: SessionStatus) -> bool {
    matches!(
        status,
        SessionStatus::Completed | SessionStatus::Interrupted | SessionStatus::Failed
    )
}

fn same_source(a: &Session, b: &Session) -> Result<bool> {
    Ok(super::sandbox_custody::rotation_session_authority(a)?
        == super::sandbox_custody::rotation_session_authority(b)?)
}

impl Store {
    pub(crate) fn manager_succession_preflight(
        &self,
        config: &rsi_common::harness_manager::HarnessManagerConfigV1,
    ) -> Result<ManagerSuccessionPreflightV2> {
        let unavailable = || ManagerSuccessionPreflightV2::Ineligible {
            logical_manager_session_id: config.manager_session_id,
            current_session_id: config.current_session_id,
            denial: ManagerSuccessionDenialV2 {
                reason: ManagerSuccessionDenialReasonV2::CurrentManagerUnavailable,
                required_action: ManagerSuccessionRequiredActionV2::RepairManagerLineage,
            },
        };
        let Some(current) = config.current_session_id else {
            return Ok(unavailable());
        };
        let Some(session) = self.get_session(current)? else {
            return Ok(unavailable());
        };
        if session.project_id != Some(config.project_id)
            || matches!(
                session.status,
                SessionStatus::Archived | SessionStatus::Deleted
            )
        {
            return Ok(unavailable());
        }
        if session.parent_id.is_some() || session.session_kind != SessionKind::Standard {
            return Ok(ManagerSuccessionPreflightV2::Ineligible {
                logical_manager_session_id: config.manager_session_id,
                current_session_id: Some(current),
                denial: ManagerSuccessionDenialV2 {
                    reason: ManagerSuccessionDenialReasonV2::CurrentManagerNotParentlessStandard,
                    required_action:
                        ManagerSuccessionRequiredActionV2::AppointParentlessStandardManager,
                },
            });
        }
        let epoch = self.manager_authority_epoch(config.project_id)?;
        let unresolved: Option<String> = self.conn.query_row(
            "SELECT operation_id FROM manager_root_successions WHERE project_id=?1 AND manager_session_id=?2 AND state IN ('reserved','executing','established','cleanup_required')",
            params![config.project_id.to_string(), config.manager_session_id.to_string()], |r| r.get(0)).optional()?;
        Ok(ManagerSuccessionPreflightV2::Eligible {
            observation: ManagerSuccessionObservationV2 {
                logical_manager_session_id: config.manager_session_id,
                current_session_id: current,
                expected: ManagerSuccessionFenceV2 {
                    authority_epoch: epoch,
                    custody_generation: self.manager_action_custody_generation(current)?,
                },
                unresolved_operation_id: unresolved.map(|s| parse_id(&s)).transpose()?,
            },
        })
    }

    pub(crate) fn manager_succession_observation(
        &self,
        caller: Uuid,
    ) -> Result<ManagerSuccessionObservationV2> {
        let (config, manager) = self.manager_config_for_caller(caller)?;
        if !manager {
            return Err(refused("manager_succession_root_required"));
        }
        match self.manager_succession_preflight(&config)? {
            ManagerSuccessionPreflightV2::Eligible { observation }
                if observation.current_session_id == caller =>
            {
                Ok(observation)
            }
            _ => Err(refused("manager_succession_root_required")),
        }
    }

    pub(crate) fn manager_authority_epoch(&self, project: Uuid) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT epoch FROM manager_authority_epochs WHERE project_id=?1",
            [project.to_string()],
            |r| r.get(0),
        )?)
    }

    fn manager_succession_authority(
        &self,
        root: &ManagerRootSuccession,
    ) -> Result<ManagerAuthorityV2> {
        let auth = self.manager_v2_authorize(
            root.predecessor_session_id,
            &root.frozen.request.fence,
            Some(ManagerCapabilityV2::SelfSuccession),
        )?;
        if auth.config.project_id != root.project_id
            || auth.config.manager_session_id != root.manager_session_id
            || auth.config.current_session_id != Some(root.predecessor_session_id)
            || self.manager_authority_epoch(root.project_id)? != root.authority_epoch
        {
            return Err(refused("manager_succession_authority_changed"));
        }
        self.manager_succession_policy_gate(
            &auth,
            root.predecessor_session_id,
            &root.frozen.launch,
        )?;
        let current = self
            .get_session(root.predecessor_session_id)?
            .ok_or_else(|| refused("manager_session_unavailable"))?;
        if !same_source(&current, &root.frozen.predecessor)? {
            return Err(refused("manager_succession_source_changed"));
        }
        self.manager_succession_check_source_custody(
            &current,
            root.frozen.predecessor_custody.as_ref(),
        )?;
        Ok(auth)
    }

    fn manager_succession_policy_gate(
        &self,
        authority: &ManagerAuthorityV2,
        predecessor: Uuid,
        launch: &ManagerLaunchChoiceV2,
    ) -> Result<()> {
        launch.validate().map_err(refused)?;
        let policy = &authority.grant.policy;
        if policy.mode != ManagerOperatingModeV2::Execute || policy.paused {
            return Err(refused("manager_v2_policy_paused"));
        }
        if !policy.allowed_launches.is_empty()
            && !policy.allowed_launches.iter().any(|c| {
                c.provider == launch.provider
                    && c.model == launch.model
                    && c.effort == launch.effort
            })
        {
            return Err(refused("manager_v2_launch_not_granted"));
        }
        let disabled: bool = self.conn.query_row("SELECT EXISTS(SELECT 1 FROM daemon_settings WHERE key='context_rotation_enabled' AND value<>'true')", [], |r| r.get(0))?;
        if disabled {
            return Err(refused("manager_succession_rotation_disabled"));
        }
        self.manager_action_human_gate(predecessor)?;
        for reason in [
            super::manager_actions::ManagerActionHoldReasonV2::Resources,
            super::manager_actions::ManagerActionHoldReasonV2::Decision,
        ] {
            if let Some(record) = self.manager_v2_record(
                &authority.config,
                super::manager_actions::MANAGER_ACTION_HOLD_KIND,
                &super::manager_actions::manager_action_hold_key(reason, None),
            )? {
                let hold: super::manager_actions::ManagerActionRuntimeHoldV2 =
                    serde_json::from_value(record.payload)?;
                if hold.blocked {
                    return Err(refused("manager_v2_root_hold"));
                }
            }
        }
        Ok(())
    }

    fn manager_succession_check_source_custody(
        &self,
        session: &Session,
        expected: Option<&ManagerRootCustody>,
    ) -> Result<()> {
        match expected {
            Some(custody) => {
                if ManagerRootCustody::from(&self.live_custody_for_session(session.id)?) != *custody
                {
                    return Err(refused("manager_succession_source_custody_changed"));
                }
            }
            None => {
                crate::sandbox::custody::CustodyService::authorize_ordinary(session)?;
                if self
                    .manager_action_custody_generation(session.id)?
                    .is_some()
                {
                    return Err(refused("manager_succession_source_custody_changed"));
                }
            }
        }
        Ok(())
    }

    /// Admission binds exact agent origin and public payload, freezes source and
    /// identities, and reserves one creation charge. It does not launch/interrupt.
    pub(crate) fn enqueue_manager_succession(
        &self,
        caller: Uuid,
        request: &AgentManagerControlRequestV2,
        source: &VerifiedManagerHandoff,
    ) -> Result<ManagerActionReceiptV2> {
        let ManagerActionV2::SucceedManager {
            expected,
            launch,
            handoff,
        } = &request.operation
        else {
            return Err(refused("manager_succession_action_required"));
        };
        request.fence.validate().map_err(refused)?;
        expected.validate().map_err(refused)?;
        handoff.validate().map_err(refused)?;
        text(&request.idempotency_key, 128).map_err(refused)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let auth = self.manager_v2_authorize(
            caller,
            &request.fence,
            Some(ManagerCapabilityV2::SelfSuccession),
        )?;
        let observed = self.manager_succession_observation(caller)?;
        let origin = ManagerActionOriginV2::Agent { caller };
        let payload = json!({"origin": origin, "request": request});
        // Current caller/scope/policy authorization precedes replay. The epoch is
        // only required for a new occurrence; replay never executes its old proof.
        if let Some(receipt) =
            self.manager_v2_replay(&auth.config, &request.idempotency_key, &payload)?
        {
            return Ok(serde_json::from_value(receipt)?);
        }
        if observed.expected.authority_epoch != expected.authority_epoch
            || observed.expected.custody_generation != expected.custody_generation
        {
            return Err(refused("manager_succession_authority_changed"));
        }
        if observed.unresolved_operation_id.is_some() {
            return Err(refused("manager_succession_unresolved"));
        }
        self.manager_succession_policy_gate(&auth, caller, launch)?;
        self.manager_action_creation_budget(&auth, false, false)?;
        let current = self
            .get_session(caller)?
            .ok_or_else(|| refused("manager_session_unavailable"))?;
        if !same_source(&current, &source.predecessor)?
            || caller != source.predecessor.id
            || serde_json::to_value(&source.handoff)? != serde_json::to_value(handoff)?
        {
            return Err(refused("manager_succession_source_changed"));
        }
        self.manager_succession_check_source_custody(&current, source.custody.as_ref())?;
        current
            .rotation_depth
            .checked_add(1)
            .ok_or_else(|| refused("manager_succession_depth_overflow"))?;
        let pending: i64 = self.conn.query_row("SELECT count(*) FROM harness_manager_v2_operations WHERE project_id=?1 AND kind='lifecycle_action' AND state IN ('queued','running','uncertain')", [auth.config.project_id.to_string()], |r| r.get(0))?;
        if pending >= 64 {
            return Err(refused("manager_v2_action_queue_full"));
        }
        let (_, _, invocation, _) =
            super::sandbox_custody::load_rotation_authority_session_on(&tx, caller)?
                .ok_or_else(|| refused("manager_session_unavailable"))?;
        if let Some(invocation) = invocation {
            let owned: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM model_invocations WHERE id=?1 AND session_id=?2)",
                params![invocation.to_string(), caller.to_string()],
                |r| r.get(0),
            )?;
            if !owned {
                return Err(refused(
                    "manager_succession_predecessor_invocation_unproven",
                ));
            }
        }
        let operation_id = Uuid::new_v4();
        let candidate = Uuid::new_v4();
        let receipt = ManagerActionReceiptV2 {
            operation_id,
            target_session_id: Some(candidate),
            state: ManagerActionStateV2::Queued,
            action_kind: request.operation.action_kind(),
            target_type: request.operation.target_type(),
            row_version: 1,
            outcome: Some("awaiting_predecessor_settlement".into()),
            result: None,
            deduplicated: false,
            operator_result: None,
        };
        let stamp = now();
        self.conn.execute("INSERT INTO harness_manager_v2_operations(id,project_id,manager_session_id,scope_version,policy_version,actor_session_id,idempotency_key,fingerprint,kind,payload_json,state,row_version,target_session_id,outcome_json,not_before,created_at,updated_at)
          VALUES(?1,?2,?3,?4,?5,?6,?7,?8,'lifecycle_action',?9,'queued',1,?10,?11,?12,?12,?12)", params![operation_id.to_string(),auth.config.project_id.to_string(),auth.config.manager_session_id.to_string(),auth.config.row_version,auth.grant.row_version,caller.to_string(),request.idempotency_key,fingerprint(&payload)?,serde_json::to_string(&payload)?,candidate.to_string(),serde_json::to_string(&receipt)?,stamp])?;
        let frozen = ManagerRootFrozen {
            request: request.clone(),
            predecessor: current.clone(),
            predecessor_invocation_id: invocation,
            predecessor_custody: source.custody.clone(),
            handoff: handoff.clone(),
            launch: launch.clone(),
        };
        self.conn.execute("INSERT INTO manager_root_successions(operation_id,project_id,manager_session_id,predecessor_session_id,candidate_session_id,launch_attempt_id,model_invocation_id,scope_version,policy_version,authority_epoch,frozen_json,state,row_version,created_at,updated_at)
          VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,'reserved',1,?12,?12)", params![operation_id.to_string(),auth.config.project_id.to_string(),auth.config.manager_session_id.to_string(),caller.to_string(),candidate.to_string(),Uuid::new_v4().to_string(),Uuid::new_v4().to_string(),auth.config.row_version,auth.grant.row_version,expected.authority_epoch,serde_json::to_string(&frozen)?,stamp])?;
        let context = ManagerActionContextV2 {
            origin,
            request: request.clone(),
            target_session_id: Some(candidate),
            source: Some(super::manager_actions::ManagerActionSourceV2 {
                session_id: caller,
                working_dir: current.working_dir.clone(),
                sandbox_root: current.sandbox_root.clone(),
                commit: handoff.source_commit.clone(),
                custody_generation: expected.custody_generation,
                historical_commit: false,
            }),
            launch: Some(launch.clone()),
            manager_pause_version: 0,
        };
        self.manager_v2_put_record(
            &auth.config,
            "lifecycle_context",
            &operation_id.to_string(),
            None,
            0,
            &serde_json::to_value(context)?,
        )?;
        self.manager_v2_event(
            &auth.config,
            Some(caller),
            "action_queued",
            &operation_id.to_string(),
            1,
            &serde_json::to_value(&receipt)?,
        )?;
        // The appointed root may have unknown historic cost. A lower bound is
        // useful evidence; it is never a fabricated zero-history checkpoint.
        let floor = current
            .cost_usd
            .filter(|n| n.is_finite() && *n >= 0.0)
            .unwrap_or(0.0);
        self.conn.execute("INSERT INTO manager_root_resource_origins(session_id,project_id,manager_session_id,scope_version,provider,zero_origin,known_floor_usd,created_at)
          VALUES(?1,?2,?3,?4,?5,0,?6,?7) ON CONFLICT(session_id) DO UPDATE SET known_floor_usd=MAX(manager_root_resource_origins.known_floor_usd,excluded.known_floor_usd)", params![caller.to_string(),auth.config.project_id.to_string(),auth.config.manager_session_id.to_string(),auth.config.row_version,serde_json::to_value(current.provider)?.as_str(),floor,stamp])?;
        tx.commit()?;
        Ok(receipt)
    }

    pub(crate) fn manager_succession(&self, id: Uuid) -> Result<Option<ManagerRootSuccession>> {
        let raw: Option<serde_json::Value> = self.conn.query_row("SELECT json_object('operation_id',operation_id,'project_id',project_id,'manager_session_id',manager_session_id,'predecessor_session_id',predecessor_session_id,'candidate_session_id',candidate_session_id,'launch_attempt_id',launch_attempt_id,'model_invocation_id',model_invocation_id,'scope_version',scope_version,'policy_version',policy_version,'authority_epoch',authority_epoch,'frozen',json(frozen_json),'state',state,'row_version',row_version,'claim_boot_id',claim_boot_id,'admission_recorded',json(CASE admission_recorded WHEN 1 THEN 'true' ELSE 'false' END),'effect_claimed',json(CASE effect_claimed WHEN 1 THEN 'true' ELSE 'false' END),'candidate_custody',json(candidate_json),'published_epoch',published_epoch) FROM manager_root_successions WHERE operation_id=?1", [id.to_string()], |r| r.get::<_,String>(0)).optional()?.map(|s| serde_json::from_str(&s)).transpose()?;
        raw.map(|value| serde_json::from_value(value).map_err(Into::into))
            .transpose()
    }

    /// Called only inside the generic action-claim IMMEDIATE transaction.
    pub(super) fn claim_manager_succession_on(&self, id: Uuid, boot_id: Uuid) -> Result<()> {
        if let Some(root) = self.manager_succession(id)? {
            if root.state != ManagerRootState::Reserved {
                return Err(refused("manager_succession_claim_changed"));
            }
            self.conn.execute("UPDATE manager_root_successions SET state='executing',row_version=row_version+1,claim_boot_id=?2,updated_at=?3 WHERE operation_id=?1 AND state='reserved'",params![id.to_string(),boot_id.to_string(),now()])?;
        }
        Ok(())
    }

    pub(crate) fn claim_manager_succession(
        &self,
        action: &ManagerActionClaimV2,
    ) -> Result<ManagerSuccessionClaim> {
        self.manager_action_assert_claim(action)?;
        let reservation = self
            .manager_succession(action.id())?
            .ok_or_else(|| refused("manager_succession_unavailable"))?;
        if reservation.state != ManagerRootState::Executing
            || reservation.claim_boot_id != Some(action.boot_id)
        {
            return Err(refused("manager_succession_claim_changed"));
        }
        Ok(ManagerSuccessionClaim {
            action: action.clone(),
            reservation,
        })
    }

    fn manager_succession_assert_claim(
        &self,
        claim: &ManagerSuccessionClaim,
    ) -> Result<ManagerRootSuccession> {
        self.manager_action_assert_claim(&claim.action)?;
        let root = self
            .manager_succession(claim.reservation.operation_id)?
            .ok_or_else(|| refused("manager_succession_unavailable"))?;
        if root.row_version != claim.reservation.row_version
            || root.claim_boot_id != Some(claim.action.boot_id)
            || !matches!(
                root.state,
                ManagerRootState::Executing | ManagerRootState::Established
            )
        {
            return Err(refused("manager_succession_claim_changed"));
        }
        Ok(root)
    }

    fn refreshed_manager_succession_claim(
        &self,
        claim: &ManagerSuccessionClaim,
    ) -> Result<ManagerSuccessionClaim> {
        Ok(ManagerSuccessionClaim {
            action: claim.action.clone(),
            reservation: self
                .manager_succession(claim.reservation.operation_id)?
                .ok_or_else(|| refused("manager_succession_unavailable"))?,
        })
    }

    /// Terminal metadata may precede the monitor's physical drain. Preserve the
    /// queued occurrence until runtime observes drain; this is not a new attempt.
    pub(crate) fn defer_manager_succession_drain(
        &self,
        claim: &ManagerSuccessionClaim,
    ) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let root = self.manager_succession_assert_claim(claim)?;
        self.manager_succession_authority(&root)?;
        // Runtime defers before recording physical drain, allocating custody or
        // entering Model Control. A captured settlement/candidate witness must
        // never be recycled into a fresh claim, even before the admission flag.
        let untouched: bool = self.conn.query_row(
            "SELECT settled_json IS NULL AND candidate_json IS NULL
                    AND establishment_json IS NULL AND published_epoch IS NULL
             FROM manager_root_successions WHERE operation_id=?1",
            [root.operation_id.to_string()],
            |row| row.get(0),
        )?;
        let operation = self.manager_action_assert_claim(&claim.action)?;
        if root.state != ManagerRootState::Executing
            || root.admission_recorded
            || root.effect_claimed
            || operation.effect_started
            || !untouched
        {
            return Err(refused("manager_succession_cleanup_required"));
        }
        let mut receipt = operation.receipt;
        receipt.state = ManagerActionStateV2::Queued;
        receipt.row_version += 1;
        receipt.outcome = Some("awaiting_predecessor_settlement".into());
        receipt.refresh_action_metadata(&operation.context.request.operation);
        let stamp = now();
        let changed = self.conn.execute(
            "UPDATE harness_manager_v2_operations
             SET state='queued',row_version=?2,outcome_json=?3,claim_boot_id=NULL,updated_at=?4
             WHERE id=?1 AND state='running' AND row_version=?5 AND claim_boot_id=?6",
            params![
                root.operation_id.to_string(),
                receipt.row_version,
                serde_json::to_string(&receipt)?,
                stamp,
                claim.action.operation.receipt.row_version,
                claim.action.boot_id.to_string()
            ],
        )?;
        if changed != 1 {
            return Err(refused("manager_v2_claim_changed"));
        }
        let changed = self.conn.execute(
            "UPDATE manager_root_successions
             SET state='reserved',claim_boot_id=NULL,row_version=row_version+1,updated_at=?2
             WHERE operation_id=?1 AND state='executing' AND row_version=?3 AND claim_boot_id=?4",
            params![
                root.operation_id.to_string(),
                stamp,
                root.row_version,
                claim.action.boot_id.to_string()
            ],
        )?;
        if changed != 1 {
            return Err(refused("manager_succession_claim_changed"));
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn record_manager_succession_predecessor_settled(
        &self,
        claim: &ManagerSuccessionClaim,
        proof: &ManagerPredecessorSettledWitness,
    ) -> Result<ManagerSuccessionClaim> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let root = self.manager_succession_assert_claim(claim)?;
        self.manager_succession_authority(&root)?;
        let (current, _, invocation, _) =
            super::sandbox_custody::load_rotation_authority_session_on(
                &tx,
                root.predecessor_session_id,
            )?
            .ok_or_else(|| refused("manager_session_unavailable"))?;
        if proof.boot_id != claim.action.boot_id
            || proof.invocation_id != invocation
            || invocation != root.frozen.predecessor_invocation_id
            || !settled_status(current.status)
            || proof.session.status != current.status
            || !same_source(&proof.session, &current)?
        {
            return Err(refused("manager_succession_predecessor_changed"));
        }
        let active: bool = self.conn.query_row("SELECT EXISTS(SELECT 1 FROM model_invocations WHERE session_id=?1 AND status IN ('running','cancellation_requested'))", [current.id.to_string()], |r| r.get(0))?;
        if active {
            return Err(refused("manager_succession_predecessor_ledger_live"));
        }
        let evidence = self.manager_succession_settlement_snapshot(current.id)?;
        self.conn.execute("UPDATE manager_root_successions SET settled_json=?2,row_version=row_version+1,updated_at=?3 WHERE operation_id=?1 AND settled_json IS NULL",params![root.operation_id.to_string(),evidence,now()])?;
        let result = self.refreshed_manager_succession_claim(claim)?;
        tx.commit()?;
        Ok(result)
    }

    fn manager_succession_settlement_snapshot(&self, id: Uuid) -> Result<String> {
        let (session, custody, invocation, legacy_tag) =
            super::sandbox_custody::load_rotation_authority_session_on(&self.conn, id)?
                .ok_or_else(|| refused("manager_session_unavailable"))?;
        let sequence: i64 = self.conn.query_row(
            "SELECT COALESCE(MAX(sequence),0) FROM conversation_events WHERE session_id=?1",
            [id.to_string()],
            |r| r.get(0),
        )?;
        Ok(serde_json::to_string(
            &json!({"session":session,"custody":custody,"invocation":invocation,"legacy_tag":legacy_tag,"sequence":sequence}),
        )?)
    }

    /// No transaction is opened: Model Control calls this while holding its own
    /// IMMEDIATE admission transaction, before any invocation/provider effect.
    pub(crate) fn manager_succession_effect_gate_on(
        &self,
        claim: &ManagerSuccessionClaim,
    ) -> Result<ManagerRootSuccession> {
        let root = self.manager_succession_assert_claim(claim)?;
        self.manager_succession_authority(&root)?;
        let settled: Option<String> = self.conn.query_row(
            "SELECT settled_json FROM manager_root_successions WHERE operation_id=?1",
            [root.operation_id.to_string()],
            |r| r.get(0),
        )?;
        if settled.as_deref()
            != Some(
                self.manager_succession_settlement_snapshot(root.predecessor_session_id)?
                    .as_str(),
            )
        {
            return Err(refused("manager_succession_settlement_changed"));
        }
        let live: bool = self.conn.query_row("SELECT EXISTS(SELECT 1 FROM model_invocations WHERE session_id=?1 AND status IN ('running','cancellation_requested'))",[root.predecessor_session_id.to_string()],|r|r.get(0))?;
        if live {
            return Err(refused("manager_succession_predecessor_ledger_live"));
        }
        Ok(root)
    }

    pub(crate) fn manager_succession_effect_gate(
        &self,
        claim: &ManagerSuccessionClaim,
    ) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let root = self.manager_succession_effect_gate_on(claim)?;
        self.manager_succession_resource_gate(
            claim,
            root.admission_recorded.then_some(root.candidate_session_id),
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Atomic Model Control hook. It must run AFTER exact invocation insertion
    /// (or exact replay validation) and the root/cohort resource gate in that SAME
    /// IMMEDIATE transaction. A row is never revived or recharged on replay.
    pub(crate) fn record_manager_succession_admission_on(
        &self,
        claim: &ManagerSuccessionClaim,
    ) -> Result<ManagerSuccessionClaim> {
        if self.conn.is_autocommit() {
            return Err(refused("manager_succession_transaction_required"));
        }
        let root = self.manager_succession_effect_gate_on(claim)?;
        self.manager_succession_check_invocation(&root)?;
        if !root.admission_recorded {
            let other: bool = self.conn.query_row("SELECT EXISTS(SELECT 1 FROM model_invocations WHERE session_id=?1 AND id<>?2) OR EXISTS(SELECT 1 FROM sessions WHERE id=?1)",params![root.candidate_session_id.to_string(),root.model_invocation_id.to_string()],|r|r.get(0))?;
            if other {
                return Err(refused("manager_succession_candidate_history"));
            }
            self.conn.execute("INSERT INTO manager_root_resource_origins(session_id,project_id,manager_session_id,scope_version,operation_id,predecessor_session_id,model_invocation_id,provider,zero_origin,known_floor_usd,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,1,0,?9)",params![root.candidate_session_id.to_string(),root.project_id.to_string(),root.manager_session_id.to_string(),root.scope_version,root.operation_id.to_string(),root.predecessor_session_id.to_string(),root.model_invocation_id.to_string(),serde_json::to_value(root.frozen.launch.provider)?.as_str(),now()])?;
            self.conn.execute("UPDATE manager_root_successions SET admission_recorded=1,row_version=row_version+1,updated_at=?2 WHERE operation_id=?1",params![root.operation_id.to_string(),now()])?;
        }
        self.refreshed_manager_succession_claim(claim)
    }

    fn manager_succession_check_invocation(&self, root: &ManagerRootSuccession) -> Result<()> {
        // Model Control owns catalog and request-fingerprint equality. Store pins
        // the immutable actual row identity/purpose/ancestry and frozen choice.
        let valid: bool = self.conn.query_row("SELECT EXISTS(SELECT 1 FROM model_invocations WHERE id=?1 AND session_id=?2 AND project_id=?3 AND status='running' AND purpose='session.rotate.child' AND provider=?4 AND model=?5 AND parent_invocation_id IS ?6 AND retry_of_invocation_id IS NULL AND effort IS ?7 AND dedup_key=?8 AND request_fingerprint=?9 AND trigger_source='manager_self_succession' AND admission_status='admitted') AND NOT EXISTS(SELECT 1 FROM model_invocations WHERE session_id=?2 AND id<>?1)",params![root.model_invocation_id.to_string(),root.candidate_session_id.to_string(),root.project_id.to_string(),serde_json::to_value(root.frozen.launch.provider)?.as_str(),root.frozen.launch.model,root.frozen.predecessor_invocation_id.map(|id|id.to_string()),root.frozen.launch.effort,root.invocation_dedup_key(),root.invocation_fingerprint()?],|r|r.get(0))?;
        if !valid {
            return Err(refused("manager_succession_invocation_changed"));
        }
        Ok(())
    }

    pub(crate) fn record_manager_succession_candidate_bound(
        &self,
        claim: &ManagerSuccessionClaim,
    ) -> Result<ManagerSuccessionClaim> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let root = self.manager_succession_effect_gate_on(claim)?;
        if !root.admission_recorded {
            return Err(refused("manager_succession_admission_required"));
        }
        self.manager_succession_check_invocation(&root)?;
        // Ordinary Session insertion does not copy the separate tag relation.
        // Freeze it before the provider boundary, in this exact candidate bind
        // transaction, preserving an operator edit instead of overwriting it.
        if root.candidate_custody.is_none() {
            let candidate = self
                .get_session(root.candidate_session_id)?
                .ok_or_else(|| refused("manager_succession_candidate_missing"))?;
            if (!candidate.tags.is_empty() && candidate.tags != root.frozen.predecessor.tags)
                || (!candidate.tag.is_empty() && candidate.tag != root.frozen.predecessor.tag)
            {
                return Err(refused("manager_succession_candidate_changed"));
            }
            for tag in &root.frozen.predecessor.tags {
                self.conn.execute(
                    "INSERT OR IGNORE INTO session_tags(session_id,tag) VALUES(?1,?2)",
                    params![root.candidate_session_id.to_string(), tag],
                )?;
            }
            self.conn.execute(
                "UPDATE sessions SET tag=?2 WHERE id=?1",
                params![
                    root.candidate_session_id.to_string(),
                    root.frozen.predecessor.tag
                ],
            )?;
        }
        let custody = self.manager_succession_candidate_custody(&root)?;
        self.conn.execute("UPDATE manager_root_successions SET candidate_json=?2,row_version=row_version+1,updated_at=?3 WHERE operation_id=?1 AND candidate_json IS NULL",params![root.operation_id.to_string(),serde_json::to_string(&custody)?,now()])?;
        let result = self.refreshed_manager_succession_claim(claim)?;
        tx.commit()?;
        Ok(result)
    }

    pub(super) fn manager_succession_candidate_custody(
        &self,
        root: &ManagerRootSuccession,
    ) -> Result<ManagerRootCustody> {
        let candidate = self
            .get_session(root.candidate_session_id)?
            .ok_or_else(|| refused("manager_succession_candidate_missing"))?;
        if candidate.status != SessionStatus::Starting
            || candidate.session_kind != SessionKind::Standard
            || candidate.parent_id.is_some()
            || candidate.project_id != Some(root.project_id)
            || candidate.continued_from != Some(root.predecessor_session_id)
            || root.frozen.predecessor.rotation_depth.checked_add(1)
                != Some(candidate.rotation_depth)
            || candidate.provider != root.frozen.launch.provider
            || candidate.model.as_deref() != Some(&root.frozen.launch.model)
            || candidate.effort != root.frozen.launch.effort
            || candidate.rotation_disabled_at != root.frozen.predecessor.rotation_disabled_at
            || candidate.tags != root.frozen.predecessor.tags
            || candidate.tag != root.frozen.predecessor.tag
        {
            return Err(refused("manager_succession_candidate_changed"));
        }
        let (_, _, invocation, _) =
            super::sandbox_custody::load_rotation_authority_session_on(&self.conn, candidate.id)?
                .ok_or_else(|| refused("manager_session_unavailable"))?;
        if invocation != Some(root.model_invocation_id) {
            return Err(refused("manager_succession_invocation_changed"));
        }
        let c = ManagerRootCustody::from(&self.live_custody_for_session(candidate.id)?);
        if c.owner_session_id != candidate.id
            || c.allocation_session_id != candidate.id
            || c.generation != 1
            || c.source_commit != root.frozen.handoff.source_commit
            || root
                .frozen
                .predecessor
                .sandbox_root
                .as_ref()
                .is_some_and(|p| *p == PathBuf::from(&c.root))
            || root.frozen.predecessor.working_dir == PathBuf::from(&c.root)
            || root.frozen.predecessor_custody.as_ref().is_some_and(|p| {
                p.custody_id == c.custody_id
                    || p.root == c.root
                    || p.branch == c.branch
                    || p.repository_identity != c.repository_identity
            })
        {
            return Err(refused("manager_succession_distinct_custody_required"));
        }
        Ok(c)
    }

    /// Durable one-use permission to cross the provider boundary. Lost ownership
    /// after this update can only enter checked cleanup, never start again.
    pub(crate) fn claim_manager_succession_provider_effect(
        &self,
        claim: &ManagerSuccessionClaim,
    ) -> Result<ManagerSuccessionClaim> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let root = self.manager_succession_effect_gate_on(claim)?;
        self.manager_succession_resource_gate(claim, Some(root.candidate_session_id))?;
        self.manager_succession_check_invocation(&root)?;
        if root.effect_claimed
            || root.candidate_custody.as_ref()
                != Some(&self.manager_succession_candidate_custody(&root)?)
        {
            return Err(refused("manager_succession_effect_unavailable"));
        }
        self.conn.execute("UPDATE manager_root_successions SET effect_claimed=1,row_version=row_version+1,updated_at=?2 WHERE operation_id=?1",params![root.operation_id.to_string(),now()])?;
        let result = self.refreshed_manager_succession_claim(claim)?;
        tx.commit()?;
        Ok(result)
    }

    pub(crate) fn commit_manager_succession(
        &self,
        claim: &ManagerSuccessionClaim,
        proof: &ManagerSuccessionPublicationWitness,
    ) -> Result<ManagerActionReceiptV2> {
        // Deterministic shard order; roots in the same shard acquire it once.
        let mut ids: Vec<Uuid> = claim
            .reservation
            .frozen
            .predecessor_custody
            .iter()
            .map(|c| c.custody_id)
            .chain(
                claim
                    .reservation
                    .candidate_custody
                    .iter()
                    .map(|c| c.custody_id),
            )
            .collect();
        ids.sort_by_key(|id| super::sandbox_custody::custody_root_lock_shard(*id));
        ids.dedup_by_key(|id| super::sandbox_custody::custody_root_lock_shard(*id));
        let _guards: Vec<_> = ids
            .into_iter()
            .map(super::sandbox_custody::lock_custody_root)
            .collect();
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let root = self.manager_succession_effect_gate_on(claim)?;
        self.manager_succession_resource_gate(claim, Some(root.candidate_session_id))?;
        if !root.effect_claimed
            || !root.admission_recorded
            || proof.candidate_id != root.candidate_session_id
            || proof.invocation_id != root.model_invocation_id
            || proof.attempt_id != root.launch_attempt_id
            || proof.boot_id != claim.action.boot_id
        {
            return Err(refused("manager_succession_establishment_changed"));
        }
        self.manager_succession_check_invocation(&root)?;
        if root.candidate_custody.as_ref()
            != Some(&self.manager_succession_candidate_custody(&root)?)
        {
            return Err(refused("manager_succession_candidate_custody_changed"));
        }
        let evidence = serde_json::to_string(
            &json!({"candidate":proof.candidate_id,"invocation":proof.invocation_id,"attempt":proof.attempt_id,"boot":proof.boot_id}),
        )?;
        self.conn.execute("UPDATE manager_root_successions SET state='established',establishment_json=?2,row_version=row_version+1,updated_at=?3 WHERE operation_id=?1",params![root.operation_id.to_string(),evidence,now()])?;
        self.finalize_distinct_manager_predecessor_on(&tx, &root)?;
        // Prove the existing bounded resolver actually reaches the candidate.
        // A fork/limit refusal rolls back archival, receipt and epoch together.
        let published = self
            .get_harness_manager(root.project_id)?
            .ok_or_else(|| refused("manager_succession_appointment_missing"))?;
        if published.manager_session_id != root.manager_session_id
            || published.current_session_id != Some(root.candidate_session_id)
            || published.row_version != root.scope_version
        {
            return Err(refused("manager_succession_publication_changed"));
        }
        let epoch = self.manager_authority_epoch(root.project_id)?;
        self.conn.execute("UPDATE manager_root_successions SET state='committed',published_epoch=?2,row_version=row_version+1,updated_at=?3 WHERE operation_id=?1",params![root.operation_id.to_string(),epoch,now()])?;
        let receipt = self.finish_manager_action_on(
            &claim.action,
            ManagerActionStateV2::Succeeded,
            "manager_successor_established",
        )?;
        tx.commit()?;
        Ok(receipt)
    }

    /// Central coupling used by all generic journal results and lost-owner paths.
    /// No admitted/effect-claimed occurrence may become terminal without cleanup.
    pub(super) fn finish_manager_succession_on(
        &self,
        id: Uuid,
        requested: ManagerActionStateV2,
    ) -> Result<ManagerActionStateV2> {
        let Some(root) = self.manager_succession(id)? else {
            return Ok(requested);
        };
        if requested == ManagerActionStateV2::Succeeded {
            if root.state != ManagerRootState::Committed {
                return Err(refused("manager_succession_publication_required"));
            }
            return Ok(requested);
        }
        if root.state == ManagerRootState::Committed {
            return Err(refused("manager_succession_already_committed"));
        }
        let durable_candidate: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1) OR EXISTS(SELECT 1 FROM model_invocations WHERE session_id=?1)",
            [root.candidate_session_id.to_string()], |r| r.get(0),
        )?;
        let uncertain = requested == ManagerActionStateV2::Uncertain
            || root.admission_recorded
            || root.effect_claimed
            || durable_candidate;
        let (state, result) = if uncertain {
            ("cleanup_required", ManagerActionStateV2::Uncertain)
        } else {
            match requested {
                ManagerActionStateV2::Revoked => ("revoked", requested),
                ManagerActionStateV2::Blocked => ("blocked", requested),
                _ => ("failed", ManagerActionStateV2::Failed),
            }
        };
        self.conn.execute("UPDATE manager_root_successions SET state=?2,row_version=row_version+1,reason=?3,updated_at=?4 WHERE operation_id=?1",params![id.to_string(),state,if uncertain {"execution_owner_lost_unconfirmed"} else {"pre_effect_refusal"},now()])?;
        Ok(result)
    }

    /// Checked settlement may run after authority revocation. It can only fail the
    /// already owned occurrence; it cannot publish or resurrect current authority.
    pub(crate) fn settle_manager_succession(
        &self,
        expected: &ManagerRootSuccession,
        proof: &ManagerSuccessionCleanupWitness,
    ) -> Result<ManagerActionReceiptV2> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let root = self
            .manager_succession(expected.operation_id)?
            .ok_or_else(|| refused("manager_succession_unavailable"))?;
        if root.state != ManagerRootState::CleanupRequired
            || root.row_version != expected.row_version
            || proof.operation_id != root.operation_id
            || proof.invocation_id != root.model_invocation_id
            || proof.candidate_id != root.candidate_session_id
        {
            return Err(refused("manager_succession_cleanup_changed"));
        }
        let active: bool = self.conn.query_row("SELECT EXISTS(SELECT 1 FROM model_invocations WHERE session_id=?1 AND status IN ('running','cancellation_requested')) OR EXISTS(SELECT 1 FROM sessions WHERE id=?1 AND status IN ('Starting','Running','WaitingApproval'))",[root.candidate_session_id.to_string()],|r|r.get(0))?;
        if active {
            return Err(refused("manager_succession_cleanup_unsettled"));
        }
        let op = self
            .manager_action_operation(root.operation_id)?
            .ok_or_else(|| refused("manager_v2_action_unavailable"))?;
        if op.receipt.state != ManagerActionStateV2::Uncertain {
            return Err(refused("manager_succession_cleanup_changed"));
        }
        let mut receipt = op.receipt;
        receipt.state = ManagerActionStateV2::Failed;
        receipt.row_version += 1;
        receipt.outcome = Some("manager_successor_checked_settlement".into());
        receipt.refresh_action_metadata(&op.context.request.operation);
        self.conn.execute("UPDATE manager_root_successions SET state='failed',row_version=row_version+1,reason='checked_settlement',updated_at=?2 WHERE operation_id=?1",params![root.operation_id.to_string(),now()])?;
        self.conn.execute("UPDATE harness_manager_v2_operations SET state='failed',row_version=?2,outcome_json=?3,updated_at=?4 WHERE id=?1 AND state='uncertain'",params![root.operation_id.to_string(),receipt.row_version,serde_json::to_string(&receipt)?,now()])?;
        self.conn.execute("INSERT INTO harness_manager_v2_events(project_id,manager_session_id,scope_version,actor_session_id,kind,record_key,row_version,payload_json,created_at) VALUES(?1,?2,?3,?4,'action_result',?5,?6,?7,?8)",params![root.project_id.to_string(),root.manager_session_id.to_string(),root.scope_version,root.predecessor_session_id.to_string(),root.operation_id.to_string(),receipt.row_version,serde_json::to_string(&receipt)?,now()])?;
        tx.commit()?;
        Ok(receipt)
    }

    pub(crate) fn list_recoverable_manager_successions(
        &self,
        after: Option<Uuid>,
        limit: usize,
    ) -> Result<Vec<ManagerRootSuccession>> {
        if !(1..=64).contains(&limit) {
            return Err(refused("manager_succession_invalid_limit"));
        }
        let mut s = self.conn.prepare("SELECT operation_id FROM manager_root_successions WHERE state='cleanup_required' AND (?1 IS NULL OR operation_id>?1) ORDER BY operation_id LIMIT ?2")?;
        let ids = s
            .query_map(params![after.map(|id| id.to_string()), limit as i64], |r| {
                r.get::<_, String>(0)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ids.into_iter()
            .map(|id| {
                self.manager_succession(parse_id(&id)?)?
                    .ok_or_else(|| refused("manager_succession_unavailable"))
            })
            .collect()
    }

    /// Monotonic measured lower-bound update; never upgrades unknown original
    /// history to zero-origin. Runtime/resource code owns measurement evidence.
    pub(crate) fn record_manager_root_spend_floor(&self, session: Uuid, floor: f64) -> Result<()> {
        if !floor.is_finite() || floor < 0.0 {
            return Err(refused("manager_succession_invalid_spend"));
        }
        let changed = self.conn.execute(
            "UPDATE manager_root_resource_origins SET known_floor_usd=MAX(known_floor_usd,?2) WHERE session_id=?1",
            params![session.to_string(), floor],
        )?;
        if changed != 1 {
            return Err(refused("manager_succession_resource_origin_missing"));
        }
        Ok(())
    }

    /// Project-retained accounting intentionally crosses scope/policy changes.
    /// Callers page all records and deduplicate these physical IDs with Epic rows.
    pub(crate) fn manager_root_resource_origins(
        &self,
        project: Uuid,
        after: Option<Uuid>,
        limit: usize,
    ) -> Result<Vec<ManagerRootResourceOrigin>> {
        if !(1..=256).contains(&limit) {
            return Err(refused("manager_succession_invalid_limit"));
        }
        let mut s = self.conn.prepare("SELECT session_id,project_id,manager_session_id,scope_version,operation_id,predecessor_session_id,model_invocation_id,provider,zero_origin,known_floor_usd FROM manager_root_resource_origins WHERE project_id=?1 AND (?2 IS NULL OR session_id>?2) ORDER BY session_id LIMIT ?3")?;
        let rows = s
            .query_map(
                params![
                    project.to_string(),
                    after.map(|id| id.to_string()),
                    limit as i64
                ],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, Option<String>>(4)?,
                        r.get::<_, Option<String>>(5)?,
                        r.get::<_, Option<String>>(6)?,
                        r.get::<_, String>(7)?,
                        r.get::<_, bool>(8)?,
                        r.get::<_, f64>(9)?,
                    ))
                },
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|(id, p, m, scope, op, pred, inv, provider, zero, floor)| {
                Ok(ManagerRootResourceOrigin {
                    session_id: parse_id(&id)?,
                    project_id: parse_id(&p)?,
                    manager_session_id: parse_id(&m)?,
                    scope_version: scope,
                    operation_id: op.map(|s| parse_id(&s)).transpose()?,
                    predecessor_session_id: pred.map(|s| parse_id(&s)).transpose()?,
                    model_invocation_id: inv.map(|s| parse_id(&s)).transpose()?,
                    provider: serde_json::from_value(json!(provider))?,
                    zero_origin: zero,
                    known_floor_usd: floor,
                })
            })
            .collect()
    }
}

fn parse_id(value: &str) -> Result<Uuid> {
    Uuid::parse_str(value).map_err(|_| refused("manager_succession_identity_corrupt"))
}

#[cfg(test)]
mod tests;
