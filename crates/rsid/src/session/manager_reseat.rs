//! #2750: `ReseatHarnessManager` — move a failed manager seat to another
//! provider in one supported step.
//!
//! A session's provider is fixed at creation and `succeed_manager` must be
//! called by the seat itself, which cannot take a turn once its provider is
//! gone. The daemon therefore does what the seat cannot: it launches a root
//! successor on the requested provider with `continued_from` set to the failed
//! seat (so every scope that pins the old session id, the hub's peer scope and
//! the satellite's inbound roots, resolves to the successor through
//! `satellite_delivery_tip`), then re-appoints it with the project's saved scope
//! and policy and records the reason (`Store::commit_manager_reseat`).
//!
//! Operator-only (`ReseatHarnessManager`); the hub manager reaches it for a
//! paired satellite through `AgentReseatSatelliteManager`, which the satellite
//! answers by calling this same function.

use super::SessionManager;
use super::spawn_single_flight::acquire_spawn_guard;
use crate::claude::LaunchConfig;
use crate::error::{DaemonError, Result};
use rsi_common::manager_reseat::{
    RESEAT_SEAT_LIVE, ReseatHarnessManagerParams, ReseatHarnessManagerResult, successor_brief,
};
use rsi_common::types::{ConversationEvent, EventType, SandboxKind, SandboxSpec};

/// `metadata` key of the System row a reseat writes on the replaced seat.
pub const MANAGER_RESEAT_METADATA_KEY: &str = "manager_reseat";

impl SessionManager {
    /// Replace the project's failed manager seat with a successor on
    /// `params.provider`.
    ///
    /// # Errors
    /// A stable `manager_reseat_*` / `manager_seat_*` refusal when the seat may
    /// not be replaced (see `Store::manager_reseat_plan`), the launch's own
    /// refusal, or the re-appointment's. A successor launched before a refused
    /// re-appointment is interrupted, so it never runs unappointed.
    pub async fn reseat_harness_manager(
        &self,
        params: ReseatHarnessManagerParams,
    ) -> Result<ReseatHarnessManagerResult> {
        let params = params
            .normalized()
            .map_err(|code| DaemonError::InvalidParam(code.into()))?;
        let seat_id =
            crate::store_lock::lock(&self.store, "manager_reseat::reseat_harness_manager")
                .await
                .manager_reseat_plan(params.project_id, params.provider_unavailable)?
                .seat
                .id;
        // One reseat per seat at a time; the plan is re-read under the guard
        // so the loser of a race sees the winner's healthy successor.
        let _seat_guard = acquire_spawn_guard(seat_id).await;
        let plan = crate::store_lock::lock(&self.store, "manager_reseat::reseat_harness_manager")
            .await
            .manager_reseat_plan(params.project_id, params.provider_unavailable)?;
        if plan.seat.id != seat_id || self.active.read().await.contains_key(&seat_id) {
            return Err(DaemonError::InvalidParam(RESEAT_SEAT_LIVE.into()));
        }
        let seat = &plan.seat;
        let tags = if plan.tags.is_empty() {
            vec!["manager".to_owned()]
        } else {
            plan.tags.clone()
        };
        // #169: a seat launched without a sandbox in a root that now admits one
        // (a project `git init`ed after appointment) is reseated into its own.
        let root_admits_sandbox =
            super::global_manager_verbs::project_root_admits_sandbox(&seat.working_dir).await;
        let config = successor_launch_config(&plan, &params, tags.clone(), root_admits_sandbox);
        let successor = Box::pin(self.launch_session(config)).await?;
        if let Err(error) = self.update_session_tags(successor, tags).await {
            tracing::warn!(%successor, %error, "reseat successor tags were not persisted");
        }
        let commit = {
            let store =
                crate::store_lock::lock(&self.store, "manager_reseat::reseat_harness_manager")
                    .await;
            store.commit_manager_reseat(&plan, successor, &params.reason)
        };
        let commit = match commit {
            Ok(commit) => commit,
            Err(error) => {
                if let Err(stop) = self.interrupt_session(successor).await {
                    tracing::warn!(%successor, %stop, "unappointed reseat successor was not interrupted");
                }
                return Err(error);
            }
        };
        tracing::warn!(
            project_id = %params.project_id,
            predecessor = %seat.id,
            %successor,
            provider = ?params.provider,
            reason = %params.reason,
            "Manager seat reseated onto another provider"
        );
        self.note_reseat_on_predecessor(&plan, successor, &params)
            .await;
        Ok(ReseatHarnessManagerResult {
            project_id: params.project_id,
            predecessor_session_id: seat.id,
            predecessor_provider: seat.provider,
            successor_session_id: successor,
            provider: params.provider,
            model: params.model,
            effort: params.effort,
            reason: params.reason,
            scope_version: commit.config.row_version,
            policy_version: commit.policy.map(|granted| granted.row_version),
        })
    }

    /// A visible System row on the replaced seat, so its transcript says where
    /// the seat went and why. The seat is stopped, so nothing else writes its
    /// sequence; a row that cannot be written is logged and dropped.
    async fn note_reseat_on_predecessor(
        &self,
        plan: &crate::store::manager_reseat::ManagerReseatPlan,
        successor: uuid::Uuid,
        params: &ReseatHarnessManagerParams,
    ) {
        let predecessor = plan.seat.id;
        let sequence =
            super::queries::next_transcript_sequence(&self.store, predecessor, &[]).await;
        let row = ConversationEvent {
            id: 0,
            session_id: predecessor,
            sequence,
            event_type: EventType::System,
            role: None,
            content: format!(
                "Manager seat reseated onto {:?} as {successor}: {}",
                params.provider, params.reason
            ),
            tool_name: None,
            tool_input: None,
            created_at: chrono::Utc::now(),
            offload_id: None,
            tool_use_id: None,
            metadata: Some(Box::new(serde_json::json!({
                MANAGER_RESEAT_METADATA_KEY: {
                    "predecessor_session_id": predecessor,
                    "predecessor_provider": plan.seat.provider,
                    "successor_session_id": successor,
                    "provider": params.provider,
                    "reason": params.reason,
                }
            }))),
        };
        match crate::store_lock::lock(&self.store, "manager_reseat::note_reseat_on_predecessor")
            .await
            .insert_event(&row)
        {
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(%predecessor, %error, "manager reseat row could not be recorded");
            }
        }
    }
}

/// The successor's launch: a root session in the seat's working directory on
/// the requested provider, `continued_from` the seat, in a sandbox of its own
/// when the seat had one or its root admits one (#169). A separate function so
/// the large config stays out of the async state of the reseat.
fn successor_launch_config(
    plan: &crate::store::manager_reseat::ManagerReseatPlan,
    params: &ReseatHarnessManagerParams,
    tags: Vec<String>,
    root_admits_sandbox: bool,
) -> LaunchConfig {
    let seat = &plan.seat;
    let sandbox = match seat.sandbox_kind.filter(|kind| *kind != SandboxKind::None) {
        Some(kind) => Some(SandboxSpec {
            kind: Some(kind),
            branch: None,
        }),
        None => super::global_manager_verbs::manager_seat_sandbox(None, root_admits_sandbox),
    };
    LaunchConfig {
        completion_gates: None,
        query: successor_brief(seat.id, seat.provider, &params.reason),
        title: seat.title.clone(),
        agent_role: seat.agent_role.clone(),
        epic_spawn_ordinal: seat.epic_spawn_ordinal,
        working_dir: Some(seat.working_dir.clone()),
        provider: Some(params.provider),
        model: params.model.clone(),
        configured_context_window: None,
        max_turns: None,
        system_prompt: None,
        session_kind: Some(seat.session_kind),
        resume_session_id: None,
        project_id: Some(params.project_id),
        rsi_session_id: None,
        rsi_socket: None,
        rsi_session_token: None,
        continued_from: Some(seat.id),
        openai_base_url: None,
        openai_api_key: None,
        conversation_history: None,
        workflow_id: seat.workflow_id,
        workflow_id_override: seat.workflow_id_override,
        max_retries: None,
        group_id: seat.group_id,
        parent_id: None,
        effort: params.effort.clone(),
        issue_identifier: None,
        issue_url: None,
        issue_tracker_id: None,
        scheduled_job_id: None,
        model_invocation_owner: None,
        model_invocation_dedup_key: None,
        model_invocation_request_fingerprint: None,
        skip_project_model_default: false,
        tool_policy: None,
        model_invocation_purpose:
            rsi_common::model_control::ModelInvocationPurpose::SessionLaunchFresh,
        sandbox,
        cargo_target_dir: None,
        execution_scratch: None,
        is_eval: false,
        skip_context_pipeline: false,
        capability_class: seat.capability_class,
        tags,
        topology_node_id: None,
        topology_iteration: 0,
        closure_selector: None,
        structured_handoff: false,
        exec_target: rsi_common::exec_target::ExecTarget::Host,
        bench_profile: None,
        loop_knobs: rsi_common::bench_profile::LoopKnobs::default(),
    }
}
