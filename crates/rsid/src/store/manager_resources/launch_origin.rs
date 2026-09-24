//! Reservation provenance for new agent UUIDs before Session publication.
//! These witnesses remain internal and confer no spawn or baton authority.

use super::*;
use crate::model_control::ModelAdmissionRequest;
use crate::store::agent_coordination::AgentSpawnRequestRecord;
use crate::store::successor_reservations::AgentSuccessorReservation;
use rsi_common::agent_coordination::{AgentSpawnStateV1, AgentSuccessorStateV1};
use rsi_common::model_control::ModelInvocationPurpose;

#[derive(Clone)]
pub(super) enum LaunchReservation {
    Child(AgentSpawnRequestRecord),
    Successor(AgentSuccessorReservation),
    ManagerSuccessor(super::super::manager_successions::ManagerSuccessionClaim),
}

impl Store {
    pub(crate) fn manager_v2_root_successor_origin(
        &self,
        claim: &super::super::manager_successions::ManagerSuccessionClaim,
    ) -> Result<ManagerResourceLaunchOrigin> {
        let root = self.manager_succession_effect_gate_on(claim)?;
        Ok(ManagerResourceLaunchOrigin {
            session_id: root.candidate_session_id,
            parent_id: None,
            project_id: Some(root.project_id),
            kind: SessionKind::Standard,
            scope: Some((root.manager_session_id, root.scope_version)),
            policy_required: true,
            reservation: Some(LaunchReservation::ManagerSuccessor(claim.clone())),
        })
    }

    pub(crate) fn manager_v2_bind_child_origin(
        &self,
        mut origin: ManagerResourceLaunchOrigin,
        request_id: Uuid,
        owner: Uuid,
    ) -> Result<ManagerResourceLaunchOrigin> {
        let record = self
            .get_agent_spawn_request(request_id)?
            .ok_or_else(|| refused("manager_v2_spawn_reservation_missing"))?;
        if record.owner_session_id != owner {
            return Err(refused("manager_v2_spawn_owner_changed"));
        }
        origin.reservation = Some(LaunchReservation::Child(record));
        origin.validate_reservation(self, None)?;
        Ok(origin)
    }

    pub(crate) fn manager_v2_bind_successor_origin(
        &self,
        mut origin: ManagerResourceLaunchOrigin,
        reservation: &AgentSuccessorReservation,
    ) -> Result<ManagerResourceLaunchOrigin> {
        origin.reservation = Some(LaunchReservation::Successor(reservation.clone()));
        origin.validate_reservation(self, None)?;
        Ok(origin)
    }
}

impl ManagerResourceLaunchOrigin {
    pub(crate) fn manager_succession_claim(
        &self,
    ) -> Option<&super::super::manager_successions::ManagerSuccessionClaim> {
        match &self.reservation {
            Some(LaunchReservation::ManagerSuccessor(claim)) => Some(claim),
            _ => None,
        }
    }

    pub(super) fn validate_reservation(
        &self,
        store: &Store,
        admission: Option<(&ModelAdmissionRequest, SessionProvider)>,
    ) -> Result<()> {
        match &self.reservation {
            Some(LaunchReservation::ManagerSuccessor(claim)) => {
                let root = store.manager_succession_effect_gate_on(claim)?;
                if root.candidate_session_id != self.session_id
                    || Some(root.project_id) != self.project_id
                    || self.parent_id.is_some()
                {
                    return Err(refused("manager_succession_invocation_changed"));
                }
                if let Some((request, provider)) = admission {
                    if request.owner.session_id != Some(root.candidate_session_id)
                        || request.owner.project_id != Some(root.project_id)
                        || request.purpose != ModelInvocationPurpose::SessionRotateChild
                        || request.trigger != "manager_self_succession"
                        || request.model.as_deref() != Some(root.frozen.launch.model.as_str())
                        || request.effort != root.frozen.launch.effort
                        || provider != root.frozen.launch.provider
                        || request.parent_invocation_id != root.frozen.predecessor_invocation_id
                        || request.retry_of_invocation_id.is_some()
                        || request.dedup_key.as_deref()
                            != Some(root.invocation_dedup_key().as_str())
                        || request.request_fingerprint.as_deref()
                            != Some(root.invocation_fingerprint()?.as_str())
                    {
                        return Err(refused("manager_succession_invocation_changed"));
                    }
                }
            }
            None => {
                if admission.is_some_and(|(request, _)| {
                    matches!(
                        request.purpose,
                        ModelInvocationPurpose::AgentSpawnChild
                            | ModelInvocationPurpose::AgentReserveSuccessor
                    )
                }) {
                    return Err(refused("manager_v2_launch_reservation_required"));
                }
            }
            Some(LaunchReservation::Child(expected)) => {
                let current = store
                    .get_agent_spawn_request(expected.spawn_request_id)?
                    .ok_or_else(|| refused("manager_v2_spawn_reservation_missing"))?;
                let owner = store
                    .get_session(expected.owner_session_id)?
                    .ok_or_else(|| refused("manager_v2_spawn_owner_missing"))?;
                let epic = store
                    .get_session(expected.epic_id)?
                    .ok_or_else(|| refused("manager_v2_launch_parent_missing"))?;
                if &current != expected
                    || current.state != AgentSpawnStateV1::Launching
                    || current.child_session_id != self.session_id
                    || Some(current.epic_id) != self.parent_id
                    || current.kind != self.kind
                    || current.request.kind != self.kind
                    || epic.session_kind != SessionKind::Epic
                    || epic.lead_session_id != Some(owner.id)
                    || owner.parent_id != Some(epic.id)
                    || owner.project_id != self.project_id
                    || epic.project_id != self.project_id
                    || !rsi_common::is_leaf_kind(owner.session_kind)
                    || matches!(
                        owner.status,
                        SessionStatus::Archived | SessionStatus::Deleted
                    )
                {
                    return Err(refused("manager_v2_spawn_reservation_changed"));
                }
                if let Some((request, provider)) = admission {
                    if request.purpose != ModelInvocationPurpose::AgentSpawnChild
                        || request.dedup_key.as_deref()
                            != Some(
                                format!("agent.spawn_child:{}", current.spawn_request_id).as_str(),
                            )
                        || request.request_fingerprint.as_deref()
                            != Some(current.request_fingerprint.as_str())
                        || provider != current.request.provider.unwrap_or(owner.provider)
                    {
                        return Err(refused("manager_v2_spawn_launch_changed"));
                    }
                }
            }
            Some(LaunchReservation::Successor(expected)) => {
                let current = store
                    .get_agent_successor(expected.reservation_id)?
                    .ok_or_else(|| refused("manager_v2_successor_reservation_missing"))?;
                let frozen = current.inherited_launch()?;
                let predecessor = store
                    .get_session(current.predecessor_session_id)?
                    .ok_or_else(|| refused("manager_v2_successor_predecessor_missing"))?;
                let authority: bool = store.conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sessions e JOIN epic_lead_generations g ON g.epic_id=e.id
                     WHERE e.id=?1 AND e.session_kind='Epic' AND e.lead_session_id=?2 AND g.generation=?3)",
                    params![current.epic_id.to_string(), current.predecessor_session_id.to_string(),
                        i64::try_from(current.expected_lead_generation).unwrap_or(-1)], |row| row.get(0),
                )?;
                if &current != expected
                    || !matches!(
                        current.state,
                        AgentSuccessorStateV1::Launching | AgentSuccessorStateV1::Uncertain
                    )
                    || current.candidate_session_id != self.session_id
                    || Some(current.epic_id) != self.parent_id
                    || current.candidate_kind != self.kind
                    || frozen.project_id != self.project_id
                    || current.model_invocation_id.is_none()
                    || current.launch_attempt_id.is_none()
                    || predecessor.project_id != self.project_id
                    || predecessor.parent_id != self.parent_id
                    || current.expected_lead_session_id != predecessor.id
                    || !authority
                {
                    return Err(refused("manager_v2_successor_reservation_changed"));
                }
                if let Some((request, provider)) = admission {
                    let key = format!(
                        "agent:reserve:successor:{}:{}",
                        current.reservation_id,
                        current.model_invocation_id.expect("checked above")
                    );
                    if request.purpose != ModelInvocationPurpose::AgentReserveSuccessor
                        || request.dedup_key.as_deref() != Some(key.as_str())
                        || request.request_fingerprint.as_deref()
                            != Some(current.request_fingerprint.as_str())
                        || provider != frozen.provider()?
                    {
                        return Err(refused("manager_v2_successor_launch_changed"));
                    }
                }
            }
        }
        Ok(())
    }
}

/// Preserve provider-default None. Known model ladders and each provider's
/// supported effort vocabulary are checked without rewriting the requested tuple.
pub(super) fn validate_root_choice(
    choice: &rsi_common::harness_manager_v2::ManagerLaunchChoiceV2,
) -> Result<()> {
    choice.validate().map_err(refused)?;
    if choice.model.chars().any(char::is_whitespace) || choice.model.starts_with('-') {
        return Err(refused("manager_succession_launch_invalid"));
    }
    if let Some(effort) = choice.effort.as_deref() {
        let valid = match choice.provider {
            SessionProvider::Codex
            | SessionProvider::CodexAppServer
            | SessionProvider::Pioneer
            | SessionProvider::OpenRouter
            | SessionProvider::Bedrock => {
                crate::codex::codex_reasoning_effort(Some(effort)).is_some()
                    && rsi_common::model_utils::known_codex_effort_ladder(&choice.model)
                        .is_none_or(|ladder| ladder.contains(&effort))
            }
            SessionProvider::Claude => crate::claude::claude_effort_level(Some(effort)).is_some(),
            // These engines do not send LaunchConfig.effort to their backend.
            // Refuse an explicit value instead of recording a silently ignored choice.
            SessionProvider::Local | SessionProvider::Antigravity => false,
            SessionProvider::Harness => {
                // The Anthropic adapter preserves adaptive effort, or maps the
                // three legacy levels to distinct thinking budgets. The current
                // OpenAI-compatible adapter does not serialize reasoning_effort.
                // Do not claim an exact setting that this engine would ignore
                // or map to the fallback medium budget.
                choice.model.starts_with("claude-")
                    && if crate::session::harness::providers::anthropic::uses_adaptive_thinking(
                        &choice.model,
                    ) {
                        crate::claude::claude_effort_level(Some(effort)).is_some()
                    } else {
                        matches!(effort, "low" | "medium" | "high")
                    }
            }
            _ => false,
        };
        if !valid {
            return Err(refused("manager_succession_launch_invalid"));
        }
    }
    Ok(())
}
