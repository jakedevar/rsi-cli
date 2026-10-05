//! Global manager v0 (#872 Slice B): the global seat's agent verbs.
//!
//! The daemon resolves the caller from its token; every verb refuses any
//! caller but the active global seat (`global_manager_not_seat`), except
//! `AgentReportToGlobal`, which only the current project manager (PM) of a
//! granted project may call. Mail is a durable one-shot resume wake on the
//! recipient: the agent-message queue refuses idle (`Completed`) targets, and
//! an idle PM or global seat is the normal case. The wake is delivered at the
//! recipient's next idle boundary and wakes an idle recipient.

use std::future::Future;
use std::sync::Arc;

use rsi_common::global_manager::{
    AgentGlobalAppointManagerRequestV1, AgentGlobalAppointManagerResultV1,
    AgentGlobalOverviewRequestV1, AgentGlobalOverviewResultV1, AgentGlobalSendRequestV1,
    AgentReportToGlobalRequestV1, GlobalManagerMessageReceiptV1, GlobalPmSeatV1,
    GlobalProjectOverviewV1,
};
use rsi_common::harness_manager::ConfigureHarnessManagerRequestV1;
use rsi_common::harness_manager_v2::ConfigureHarnessManagerPolicyRequestV2;
use uuid::Uuid;

use super::agent_verbs::AgentControlHandle;
use crate::claude::LaunchConfig;
use crate::error::{DaemonError, Result};
use crate::store::Store;
use crate::store::global_manager::{GlobalMessage, GlobalMessageDirection};

fn invalid(code: &'static str) -> DaemonError {
    DaemonError::InvalidParam(code.into())
}

fn project_name(store: &Store, project_id: Uuid) -> Result<String> {
    Ok(store
        .get_project(project_id)?
        .map_or_else(|| project_id.to_string(), |project| project.name))
}

impl AgentControlHandle {
    /// `AgentGlobalOverview`: one bounded read of every granted project.
    ///
    /// # Errors
    /// `global_manager_not_seat` or a persistence error.
    pub async fn agent_global_overview(
        &self,
        caller: Uuid,
        _request: AgentGlobalOverviewRequestV1,
    ) -> Result<AgentGlobalOverviewResultV1> {
        let (grant, rows) = {
            let store = self.store.lock().await;
            let grant = store.global_seat_grant(caller)?;
            let rows = store.global_project_rows(&grant)?;
            (grant, rows)
        };
        let mut projects = Vec::with_capacity(rows.len());
        for row in rows {
            let manager = match row.manager_session_id {
                Some(id) => {
                    self.global_pm_seat(id, row.scope_version.unwrap_or(0))
                        .await
                }
                None => None,
            };
            projects.push(GlobalProjectOverviewV1 {
                project_id: row.project_id,
                name: row.name,
                path: row.path,
                manager,
                policy: row.policy,
                issues: row.issues,
                running_sessions: row.running_sessions,
                waiting_approval_sessions: row.waiting_approval_sessions,
                pending_questions: row.pending_questions,
                pending_approvals: row.pending_approvals,
            });
        }
        Ok(AgentGlobalOverviewResultV1 {
            grant_version: grant.grant_version,
            projects,
        })
    }

    async fn global_pm_seat(&self, id: Uuid, scope_version: i64) -> Option<GlobalPmSeatV1> {
        let session = self.get_session(id).await?;
        Some(GlobalPmSeatV1 {
            session_id: session.id,
            status: session.status,
            provider: session.provider,
            model: session.model,
            effort: session.effort,
            context_fill_pct: session.context_fill_pct,
            cost_usd: session.cost_usd,
            updated_at: session.updated_at,
            scope_version,
            pending_question: session.pending_question.is_some(),
        })
    }

    /// `AgentGlobalSend`: durable mail to a granted project's current PM.
    ///
    /// # Errors
    /// `global_manager_not_seat`, `global_project_not_in_grant`,
    /// `global_project_has_no_manager`, an idempotency conflict or a
    /// persistence error.
    pub async fn agent_global_send(
        &self,
        caller: Uuid,
        request: AgentGlobalSendRequestV1,
    ) -> Result<GlobalManagerMessageReceiptV1> {
        request.validate().map_err(invalid)?;
        let store = self.store.lock().await;
        let grant = store.global_seat_grant(caller)?;
        Store::global_grant_covers(&grant, request.project_id)?;
        let target = store.global_current_manager(request.project_id)?;
        let name = project_name(&store, request.project_id)?;
        let delivery = format!(
            "Message from the global manager (session {caller}) to you, the project manager of {name} ({project}):\n\n{message}\n\nReport results, blockers and handoffs up with AgentReportToGlobal.",
            project = request.project_id,
            message = request.message,
        );
        store.queue_global_message(&GlobalMessage {
            grant: &grant,
            direction: GlobalMessageDirection::ToManager,
            project_id: request.project_id,
            sender: caller,
            target,
            idempotency_key: &request.idempotency_key,
            request: serde_json::to_value(&request)?,
            delivery,
        })
    }

    /// `AgentReportToGlobal`: durable mail from a granted project's current PM
    /// to the global seat.
    ///
    /// # Errors
    /// `global_report_not_authorized`, an idempotency conflict or a
    /// persistence error.
    pub async fn agent_report_to_global(
        &self,
        caller: Uuid,
        request: AgentReportToGlobalRequestV1,
    ) -> Result<GlobalManagerMessageReceiptV1> {
        request.validate().map_err(invalid)?;
        let store = self.store.lock().await;
        let (grant, project_id) = store.global_report_grant(caller)?;
        let target = grant.seat_session_id;
        let name = project_name(&store, project_id)?;
        let delivery = format!(
            "Report from the project manager of {name} ({project_id}, session {caller}):\n\n{message}",
            message = request.message,
        );
        store.queue_global_message(&GlobalMessage {
            grant: &grant,
            direction: GlobalMessageDirection::ToGlobal,
            project_id,
            sender: caller,
            target,
            idempotency_key: &request.idempotency_key,
            request: serde_json::to_value(&request)?,
            delivery,
        })
    }
}

/// `AgentGlobalAppointManager` with an injected launcher (production passes
/// [`super::SessionManager::launch_global_appointment`] through
/// [`global_manager_launch_config`]).
///
/// Order:
/// 1. The exact seat, grant scope, launch allowlist and the area-delegates
///    rule are checked before any effect (a refusal leaves no session or
///    appointment row).
/// 2. The appointment row is recorded with a daemon-reserved session id, and
///    the launch persists exactly that id, so a replay never launches twice.
/// 3. After the launch, under the held store lock, the same active grant
///    (id, version, seat) is re-checked before the appoint
///    (`configure_harness_manager`, whole-project scope, displacing the
///    current PM) and the policy save (`configure_harness_manager_policy`
///    under the new scope version).
/// 4. The policy request is persisted before it is sent and replayed verbatim
///    on retry, so a crash between the policy commit and the appointment's
///    completion never turns into a permanent idempotency conflict.
///
/// # Errors
/// `global_manager_not_seat`, `global_project_not_in_grant`,
/// `global_launch_not_allowed`, the area-delegates refusal, an idempotency
/// conflict, a launch error or a manager-store refusal.
pub(crate) async fn global_appoint_manager_with<F, Fut>(
    store: &Arc<tokio::sync::Mutex<Store>>,
    caller: Uuid,
    request: &AgentGlobalAppointManagerRequestV1,
    launch: F,
) -> Result<AgentGlobalAppointManagerResultV1>
where
    F: FnOnce(Uuid) -> Fut,
    Fut: Future<Output = Result<Uuid>>,
{
    request.validate().map_err(invalid)?;
    let (grant, appointment, session_exists) = {
        let guard = store.lock().await;
        let grant = guard.global_seat_grant(caller)?;
        Store::global_grant_covers(&grant, request.project_id)?;
        Store::global_launch_allowed(&grant, &request.launch)?;
        let existing = guard.global_appointment(&grant, &request.idempotency_key, request)?;
        if existing.is_none() {
            guard.global_appointment_delegates_free(request.project_id)?;
        }
        let appointment = match existing {
            Some(existing) => existing,
            None => guard.record_global_appointment_launch(
                &grant,
                request.project_id,
                caller,
                &request.idempotency_key,
                request,
                Uuid::new_v4(),
            )?,
        };
        let exists = guard.get_session(appointment.session_id)?.is_some();
        (grant, appointment, exists)
    };
    if let Some((scope_version, policy_version)) = appointment.appointed {
        return Ok(AgentGlobalAppointManagerResultV1 {
            session_id: appointment.session_id,
            scope_version,
            policy_version,
            deduplicated: true,
        });
    }
    if !session_exists {
        let launched = launch(appointment.session_id).await?;
        if launched != appointment.session_id {
            return Err(DaemonError::Store(
                "global appointment launched an unexpected session".into(),
            ));
        }
    }
    let guard = store.lock().await;
    let current = guard.global_seat_grant(caller)?;
    if current.grant_id != grant.grant_id || current.grant_version != grant.grant_version {
        return Err(invalid(rsi_common::global_manager::GLOBAL_MANAGER_NOT_SEAT));
    }
    let expected_row_version = guard
        .get_harness_manager(request.project_id)?
        .map_or(0, |config| config.row_version);
    let config = guard.configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
        project_id: request.project_id,
        session_id: appointment.session_id,
        epic_ids: None,
        group_ids: vec![],
        expected_row_version,
    })?;
    let policy_request = match appointment.policy_request {
        Some(recorded) => recorded,
        None => {
            let expected_policy_version = guard
                .get_harness_manager_policy(request.project_id)?
                .map_or(0, |policy| policy.row_version);
            let fresh = ConfigureHarnessManagerPolicyRequestV2 {
                project_id: request.project_id,
                expected_scope_version: config.row_version,
                expected_policy_version,
                idempotency_key: format!("global-appoint-{}", appointment.id),
                policy: grant.project_policy.clone(),
            };
            guard.record_global_appointment_policy_request(appointment.id, &fresh)?;
            fresh
        }
    };
    let policy = guard.configure_harness_manager_policy(&policy_request)?;
    guard.complete_global_appointment(appointment.id, policy.scope_version, policy.row_version)?;
    Ok(AgentGlobalAppointManagerResultV1 {
        session_id: appointment.session_id,
        scope_version: policy.scope_version,
        policy_version: policy.row_version,
        deduplicated: false,
    })
}

impl super::SessionManager {
    /// `AgentGlobalAppointManager`: launch a Standard root session in the
    /// project (the TUI `:blank` launch path), appoint it and save the grant's
    /// project policy.
    ///
    /// # Errors
    /// See [`global_appoint_manager_with`].
    pub async fn agent_global_appoint_manager(
        &self,
        caller: Uuid,
        request: AgentGlobalAppointManagerRequestV1,
    ) -> Result<AgentGlobalAppointManagerResultV1> {
        let store = Arc::clone(self.store());
        global_appoint_manager_with(&store, caller, &request, |session_id| {
            self.launch_global_manager_session(&request, session_id)
        })
        .await
    }

    async fn launch_global_manager_session(
        &self,
        request: &AgentGlobalAppointManagerRequestV1,
        session_id: Uuid,
    ) -> Result<Uuid> {
        let working_dir = self.resolve_project_working_dir(request.project_id).await?;
        let config = global_manager_launch_config(request, working_dir, session_id);
        self.launch_global_appointment(config, session_id).await
    }
}

/// The PM's launch: a Standard root session in the project (the `:blank`
/// launch shape) with the grant-checked provider, model and effort.
pub(crate) fn global_manager_launch_config(
    request: &AgentGlobalAppointManagerRequestV1,
    working_dir: std::path::PathBuf,
    session_id: Uuid,
) -> LaunchConfig {
    {
        let sandbox = request
            .sandbox
            .unwrap_or(true)
            .then_some(rsi_common::types::SandboxSpec {
                kind: Some(rsi_common::types::SandboxKind::GitWorktree),
                branch: None,
            });
        LaunchConfig {
            completion_gates: None,
            query: request.query.clone(),
            title: None,
            agent_role: None,
            epic_spawn_ordinal: None,
            working_dir: Some(working_dir),
            provider: Some(request.launch.provider),
            model: Some(request.launch.model.clone()),
            configured_context_window: None,
            max_turns: None,
            system_prompt: None,
            resume_session_id: None,
            session_kind: Some(rsi_common::types::SessionKind::Standard),
            project_id: Some(request.project_id),
            rsi_session_id: Some(session_id),
            rsi_socket: None,
            rsi_session_token: None,
            continued_from: None,
            openai_base_url: None,
            openai_api_key: None,
            conversation_history: None,
            workflow_id: None,
            workflow_id_override: None,
            max_retries: None,
            group_id: None,
            parent_id: None,
            effort: request.launch.effort.clone(),
            issue_identifier: None,
            issue_url: None,
            issue_tracker_id: None,
            scheduled_job_id: None,
            model_invocation_owner: None,
            model_invocation_dedup_key: Some(format!("global.appoint:{session_id}")),
            model_invocation_request_fingerprint: Some(
                crate::model_control::hash_request_fingerprint(&[&session_id.to_string()]),
            ),
            skip_project_model_default: true,
            tool_policy: None,
            model_invocation_purpose:
                rsi_common::model_control::ModelInvocationPurpose::SessionLaunchFresh,
            sandbox,
            cargo_target_dir: None,
            execution_scratch: None,
            is_eval: false,
            skip_context_pipeline: false,
            capability_class: None,
            tags: vec!["untagged".into()],
            topology_node_id: None,
            topology_iteration: 0,
            closure_selector: None,
        }
    }
}

#[cfg(test)]
#[path = "global_manager_verbs_tests.rs"]
mod tests;
