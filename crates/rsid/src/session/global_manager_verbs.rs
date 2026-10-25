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
    AgentReportToGlobalRequestV1, GlobalManagerMessageReceiptV1, GlobalManagerWorkspaceV1,
    GlobalPmSeatV1, GlobalProjectOverviewV1, GlobalSeatSessionV1, GlobalWorkspaceProjectV1,
    SEAT_PREDECESSOR_LIMIT, SeatPredecessorV1,
};
use rsi_common::manager_tier_routing::{
    AgentReportUpRequestV1, AgentSendDownRequestV1, ManagerNodeRefV1, ManagerTierMessageReceiptV1,
};
use rsi_common::portfolio_delegation::{
    AgentManagerAppointChildRequestV1, AgentManagerAppointChildResultV1,
    AgentManagerRevokeChildRequestV1, AgentManagerRevokeChildResultV1,
};
use uuid::Uuid;

use super::agent_verbs::AgentControlHandle;
use crate::claude::LaunchConfig;
use crate::error::{DaemonError, Result};
use crate::store::Store;
use crate::store::global_manager::GlobalProjectRow;
use crate::store::manager_node_workspace::ProjectSpan;
use crate::store::portfolio_nodes::delegation::{ChildAppointment, ChildTargetRef};

const HOME_ID: Uuid = rsi_common::types::Project::HOME_ID;

fn invalid(code: &'static str) -> DaemonError {
    DaemonError::InvalidParam(code.into())
}

impl AgentControlHandle {
    /// `AgentGlobalOverview`: one bounded read of every project in the
    /// caller's portfolio grant. #1240 keeps it as the v0-shaped alias of
    /// `AgentManagerOverview`: the same project rows and seats, over the
    /// whole grant, with the v0 refusal.
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
        let projects = self
            .workspace_projects(rows)
            .await
            .into_iter()
            .map(|project| project.overview)
            .collect();
        Ok(AgentGlobalOverviewResultV1 {
            grant_version: grant.grant_version,
            projects,
        })
    }

    /// Operator-only `GetGlobalManagerWorkspace` (#1213), since #1240 a shim
    /// over `GetManagerNodeWorkspace`: the operator workspace of the single
    /// active root labelled `global`, projected to the v0 shape. With no
    /// active root it shows the most recent (revoked) grant, as before. No
    /// caller check: the RPC is default-denied to tokened callers because it
    /// is not in the attributed verb registry.
    ///
    /// # Errors
    /// `global_manager_ambiguous` or a persistence error.
    pub async fn operator_global_workspace(&self) -> Result<GlobalManagerWorkspaceV1> {
        let node = self.store.lock().await.global_shim_node()?;
        if let Some(node_id) = node {
            return Ok(self
                .manager_node_workspace(
                    ManagerNodeRefV1::Portfolio { node_id },
                    ProjectSpan::Coverage,
                    false,
                )
                .await?
                .into_global());
        }
        let (grant, rows, missing_project_ids) = {
            let store = self.store.lock().await;
            let Some(grant) = store.latest_global_grant()? else {
                return Ok(GlobalManagerWorkspaceV1::default());
            };
            let rows = store.global_project_rows(&grant)?;
            let mut missing = Vec::new();
            for project_id in &grant.project_ids {
                if store.get_project(*project_id)?.is_none() {
                    missing.push(*project_id);
                }
            }
            (grant, rows, missing)
        };
        Ok(GlobalManagerWorkspaceV1 {
            seat: self.seat_view(Some(grant.seat_session_id)).await,
            grant: Some(grant),
            projects: self.workspace_projects(rows).await,
            missing_project_ids,
        })
    }

    /// A seat session as the workspace shows it; `None` when it is gone.
    /// #1627: with the seat's earlier sessions (`continued_from` chain).
    pub(super) async fn seat_view(&self, id: Option<Uuid>) -> Option<GlobalSeatSessionV1> {
        let session = self.get_session(id?).await?;
        let mut predecessors = Vec::new();
        let mut truncated = false;
        let mut seen = vec![session.id];
        let mut cursor = session.continued_from;
        while let Some(prior_id) = cursor {
            if seen.contains(&prior_id) {
                break;
            }
            if predecessors.len() >= SEAT_PREDECESSOR_LIMIT {
                truncated = true;
                break;
            }
            seen.push(prior_id);
            let Some(prior) = self.get_session(prior_id).await else {
                break;
            };
            cursor = prior.continued_from;
            let prior_cost = self.visible_session_cost(&prior).await;
            predecessors.push(SeatPredecessorV1 {
                session_id: prior.id,
                project_id: prior.project_id,
                status: prior.status,
                provider: prior.provider,
                model: prior.model,
                context_fill_pct: prior.context_fill_pct,
                cost_usd: prior_cost,
                launch_turn: Box::pin(self.seat_launch_turn(prior.id)).await,
                updated_at: prior.updated_at,
            });
        }
        let succession_refusal = self.succession_refusal_of(session.id).await;
        let session_cost = self.visible_session_cost(&session).await;
        Some(GlobalSeatSessionV1 {
            session_id: session.id,
            project_id: session.project_id,
            status: session.status,
            provider: session.provider,
            rotation_threshold_pct: self.seat_rotation_threshold_pct(&session),
            coordinator_budget: self.seat_coordinator_budget(&session).await,
            model: session.model,
            context_fill_pct: session.context_fill_pct,
            cost_usd: session_cost,
            launch_turn: Box::pin(self.seat_launch_turn(session.id)).await,
            updated_at: session.updated_at,
            pending_question: session.pending_question.is_some(),
            succession_refusal,
            predecessors,
            predecessors_truncated: truncated,
        })
    }

    async fn visible_session_cost(&self, session: &rsi_common::types::Session) -> Option<f64> {
        let store = self.store.lock().await;
        store
            .session_billing_mode(session)
            .ok()
            .filter(|mode| mode.is_metered())
            .and(session.cost_usd)
    }

    /// #3386: the launch turn of a rotation successor, or `None` for a session
    /// that was not one (or when the read fails: the seat view stays usable).
    async fn seat_launch_turn(
        &self,
        id: Uuid,
    ) -> Option<rsi_common::global_manager::SeatLaunchTurnV1> {
        match self.store.lock().await.seat_launch_turn(id) {
            Ok(turn) => turn,
            Err(error) => {
                tracing::warn!(session_id = %id, %error, "seat launch-turn read failed");
                None
            }
        }
    }

    /// #2558: the rotation threshold a manager seat is held to, beside its
    /// `context_fill_pct` (same unit, same window).
    fn seat_rotation_threshold_pct(&self, session: &rsi_common::types::Session) -> Option<f64> {
        let runtime = self.runtime_config.as_deref()?;
        super::context_cap::rotation_threshold_for(runtime, session, true).map(|t| t.pct)
    }

    pub(super) async fn seat_coordinator_budget(
        &self,
        session: &rsi_common::Session,
    ) -> Option<rsi_common::coordinator_budget::CoordinatorBudgetV1> {
        let mut budget = self.runtime_config.as_deref()?.resolved_coordinator_budget(
            session.provider,
            session.model.as_deref(),
            crate::provider_capabilities::resolved_context_budget_for_session(session),
        );
        // Issue #4021: the seat's wake-turn rate against the operator's cap.
        let store = self.store.lock().await;
        budget.turn_budget = Some(crate::scheduler::turn_budget::snapshot(
            &store,
            session.id,
            chrono::Utc::now(),
        ));
        Some(budget)
    }

    /// Operator settings projection. Enumerate only seat roots, then use the
    /// same live-tip/session projection as manager overviews (never all history).
    pub(crate) async fn coordinator_budgets(
        &self,
    ) -> Result<Vec<rsi_common::coordinator_budget::CoordinatorSeatBudgetV1>> {
        let ids = {
            let store = self.store.lock().await;
            let roots: Vec<String> = store.conn.prepare(
                "SELECT manager_session_id FROM harness_manager_scopes
                 UNION SELECT seat_root_session_id FROM manager_nodes WHERE state='active'
                 UNION SELECT seat_session_id FROM global_manager_grants WHERE state='active'
                 UNION SELECT lead_session_id FROM sessions WHERE lead_session_id IS NOT NULL AND session_kind='Epic'"
            )?.query_map([], |row| row.get(0))?.collect::<rusqlite::Result<_>>()?;
            let mut ids = std::collections::BTreeSet::new();
            for root in roots {
                let root = Uuid::parse_str(&root).map_err(|e| DaemonError::Store(e.to_string()))?;
                // Historical/revoked roots can have no valid live lineage.
                // They must not make GetDaemonConfig unavailable.
                let Ok(tip) = store.manager_lineage_tip(root) else {
                    continue;
                };
                if super::context_cap::capped_seat(&store, tip)?.is_some() {
                    ids.insert(tip);
                }
            }
            ids
        };
        let mut budgets = Vec::new();
        for session_id in ids {
            if let Some(session) = self.get_session(session_id).await {
                if let Some(budget) = self.seat_coordinator_budget(&session).await {
                    budgets.push(rsi_common::coordinator_budget::CoordinatorSeatBudgetV1 {
                        session_id,
                        budget,
                    });
                }
            }
        }
        Ok(budgets)
    }

    /// Each project row with its live PM seat.
    pub(super) async fn workspace_projects(
        &self,
        rows: Vec<GlobalProjectRow>,
    ) -> Vec<GlobalWorkspaceProjectV1> {
        let mut projects = Vec::with_capacity(rows.len());
        for row in rows {
            let manager = match row.manager_session_id {
                Some(id) => {
                    self.global_pm_seat(id, row.scope_version.unwrap_or(0))
                        .await
                }
                None => None,
            };
            let standing_directives = self
                .active_standing_directives(
                    rsi_common::manager_tier_routing::ManagerNodeRefV1::Project {
                        project_id: row.project_id,
                    },
                )
                .await;
            // #2743: is rolling known good? A read failure leaves the field
            // out rather than failing the whole snapshot.
            let rolling_health = self
                .store
                .lock()
                .await
                .rolling_health_for_project(row.project_id, chrono::Utc::now())
                .map_err(|error| {
                    tracing::warn!(project_id = %row.project_id, %error, "rolling health unavailable");
                })
                .ok();
            projects.push(GlobalWorkspaceProjectV1 {
                scope_revoked: row.scope_revoked,
                overview: GlobalProjectOverviewV1 {
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
                    standing_directives,
                    rolling_health,
                },
            });
        }
        projects
    }

    async fn global_pm_seat(&self, id: Uuid, scope_version: i64) -> Option<GlobalPmSeatV1> {
        let session = self.get_session(id).await?;
        let rotation_threshold_pct = self.seat_rotation_threshold_pct(&session);
        let coordinator_budget = self.seat_coordinator_budget(&session).await;
        let succession_refusal = self.succession_refusal_of(session.id).await;
        let session_cost = self.visible_session_cost(&session).await;
        Some(GlobalPmSeatV1 {
            session_id: session.id,
            status: session.status,
            provider: session.provider,
            model: session.model,
            rotation_threshold_pct,
            coordinator_budget,
            effort: session.effort,
            context_fill_pct: session.context_fill_pct,
            cost_usd: session_cost,
            updated_at: session.updated_at,
            scope_version,
            pending_question: session.pending_question.is_some(),
            succession_refusal,
        })
    }

    /// #2667: the typed code of the seat's finally refused succession.
    async fn succession_refusal_of(&self, session_id: Uuid) -> Option<String> {
        self.store
            .lock()
            .await
            .manager_succession_refusal(session_id)
            .unwrap_or_else(|error| {
                tracing::warn!(%session_id, %error, "manager succession refusal unavailable");
                None
            })
    }

    /// `AgentGlobalSend` (#1238: alias of `AgentSendDown` to a project for
    /// one release): durable mail to a granted project's current PM, with the
    /// v0 receipt, delivery text and refusal codes. It is written as a tier
    /// message; `global_manager_messages` keeps only pre-#1238 rows.
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
        self.store.lock().await.tier_global_send(caller, &request)
    }

    /// `AgentManagerRevokeChild` (#1239): revoke a child node the caller's
    /// node granted, grantor-scoped.
    ///
    /// # Errors
    /// `global_manager_not_seat`, `manager_child_operator_granted`,
    /// `manager_node_not_in_scope`, `manager_node_stale` or a persistence
    /// error.
    pub async fn agent_manager_revoke_child(
        &self,
        caller: Uuid,
        request: AgentManagerRevokeChildRequestV1,
    ) -> Result<AgentManagerRevokeChildResultV1> {
        request.validate().map_err(invalid)?;
        let (node, outcome, deduplicated) = self
            .store
            .lock()
            .await
            .revoke_child_portfolio_node(caller, &request)?;
        Ok(AgentManagerRevokeChildResultV1 {
            node_id: node.node_id,
            state: node.state,
            grant_version: node.grant.grant_version,
            revoked: outcome.revoked,
            reparented: outcome.reparented,
            deduplicated,
        })
    }

    /// `AgentReportToGlobal` (#1238: alias of `AgentReportUp` for one
    /// release): durable mail from a covered project's current PM to the
    /// deepest portfolio node covering it, with the v0 receipt and codes.
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
        self.store
            .lock()
            .await
            .tier_report_to_global(caller, &request)
    }

    /// `AgentReportUp` (#1238): durable mail to `parent_of(the caller's
    /// node)`; at the top of the chain an operator notice. No authority.
    ///
    /// # Errors
    /// `manager_tier_not_node_seat`, `manager_tier_target_vacant`, an
    /// idempotency conflict, a full mailbox or a persistence error.
    pub async fn agent_report_up(
        &self,
        caller: Uuid,
        request: AgentReportUpRequestV1,
    ) -> Result<ManagerTierMessageReceiptV1> {
        request.validate().map_err(invalid)?;
        self.store.lock().await.tier_report_up(caller, &request)
    }

    /// `AgentSendDown` (#1238): durable mail to a descendant node's seat
    /// inside the caller's coverage; never to a sibling or an ancestor.
    ///
    /// # Errors
    /// `manager_tier_not_node_seat`, `manager_target_not_descendant`,
    /// `manager_project_not_in_scope`, `manager_tier_target_unknown`,
    /// `manager_tier_target_vacant`, an idempotency conflict, a full mailbox
    /// or a persistence error.
    pub async fn agent_send_down(
        &self,
        caller: Uuid,
        request: AgentSendDownRequestV1,
    ) -> Result<ManagerTierMessageReceiptV1> {
        request.validate().map_err(invalid)?;
        self.store.lock().await.tier_send_down(caller, &request)
    }
}

/// `AgentManagerAppointChild` (#1239) with an injected launcher. `launch`
/// receives the reserved session id, the project to launch in and the
/// admitted appointment
/// (production passes [`super::SessionManager::launch_global_appointment`]
/// through [`child_launch_config`]).
///
/// Order:
/// 1. Admission (`Store::begin_child_appointment`): the caller's node, the
///    target inside its grant, the launch allowlist, sibling disjointness,
///    the direct-report cap, the narrowing rule and the grantor's manager
///    resource gates (#1314) are checked before any effect, and the
///    appointment is recorded with a reserved session id (a refusal leaves
///    no session or appointment row). A replay returns the recorded
///    appointment.
/// 2. The launch persists exactly that id, so a replay never launches twice.
///    Its Model Control admission rechecks the grantor's resource gates and
///    reserves the slot immediately before the provider runs (#1314). A
///    portfolio launch publishes its grant after durable session establishment
///    and before provider context or dispatch (#1835).
/// 3. After the launch, under the held store lock, completion is returned
///    idempotently for the published portfolio grant, or a project's PM is
///    appointed (whole-project scope, then the node's child policy). The
///    grantor's seat and authority epoch are re-checked at completion.
///
/// # Errors
/// The admission refusals, `global_manager_not_seat` after a re-grant, a
/// launch error or a manager-store refusal.
pub(crate) async fn appoint_child_with<F, Fut>(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    caller: Uuid,
    request: &AgentManagerAppointChildRequestV1,
    launch: F,
) -> Result<AgentManagerAppointChildResultV1>
where
    F: FnOnce(Uuid, Uuid, ChildAppointment) -> Fut,
    Fut: Future<Output = Result<Uuid>>,
{
    request.validate().map_err(invalid)?;
    let (appointment, session_exists) = {
        let guard = store.lock().await;
        let appointment = guard.begin_child_appointment(caller, request)?;
        let exists = guard.get_session(appointment.session_id)?.is_some();
        (appointment, exists)
    };
    let result = |versions: (i64, i64), deduplicated: bool| AgentManagerAppointChildResultV1 {
        appointment_id: appointment.id,
        session_id: appointment.session_id,
        target_ref: appointment.target.as_column(),
        project_id: match appointment.target {
            ChildTargetRef::Project(project) => Some(project),
            ChildTargetRef::Portfolio(_) => None,
        },
        node_id: match appointment.target {
            ChildTargetRef::Portfolio(node) => Some(node),
            ChildTargetRef::Project(_) => None,
        },
        scope_version: versions.0,
        policy_version: versions.1,
        deduplicated,
    };
    if let Some(versions) = appointment.appointed {
        return Ok(result(versions, true));
    }
    if !session_exists {
        let launched = launch(
            appointment.session_id,
            appointment.launch_project_id,
            appointment.clone(),
        )
        .await?;
        if launched != appointment.session_id {
            return Err(DaemonError::Store(
                "a delegated appointment launched an unexpected session".into(),
            ));
        }
    }
    let guard = store.lock().await;
    let versions = match appointment.target {
        ChildTargetRef::Project(_) => {
            guard.finish_project_appointment(caller, request, &appointment)?
        }
        ChildTargetRef::Portfolio(_) => {
            guard.finish_portfolio_appointment(caller, request, &appointment)?
        }
    };
    Ok(result(versions, false))
}

/// `AgentGlobalAppointManager`, the #1239 alias of
/// [`appoint_child_with`] for a project target. `launch` receives the
/// reserved session id.
///
/// # Errors
/// See [`appoint_child_with`].
pub(crate) async fn global_appoint_manager_with<F, Fut>(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    caller: Uuid,
    request: &AgentGlobalAppointManagerRequestV1,
    launch: F,
) -> Result<AgentGlobalAppointManagerResultV1>
where
    F: FnOnce(Uuid) -> Fut,
    Fut: Future<Output = Result<Uuid>>,
{
    request.validate().map_err(invalid)?;
    let child = AgentManagerAppointChildRequestV1::from(request);
    // The alias keeps v0's refusal codes for its callers.
    let result = appoint_child_with(store, caller, &child, |session_id, _, _| launch(session_id))
        .await
        .map_err(|error| match error {
            DaemonError::InvalidParam(code)
                if code == rsi_common::global_manager::MANAGER_PROJECT_NOT_IN_SCOPE =>
            {
                invalid(rsi_common::global_manager::GLOBAL_PROJECT_NOT_IN_GRANT)
            }
            DaemonError::InvalidParam(code)
                if code == rsi_common::portfolio_nodes::PORTFOLIO_IDEMPOTENCY_CONFLICT =>
            {
                invalid(rsi_common::global_manager::GLOBAL_MANAGER_IDEMPOTENCY_CONFLICT)
            }
            other => other,
        })?;
    Ok(AgentGlobalAppointManagerResultV1 {
        session_id: result.session_id,
        scope_version: result.scope_version,
        policy_version: result.policy_version,
        deduplicated: result.deduplicated,
    })
}

impl super::SessionManager {
    /// `AgentManagerAppointChild`: launch a Standard root session (the TUI
    /// `:blank` launch path) and seat it as a child of the caller's node.
    ///
    /// # Errors
    /// See [`appoint_child_with`].
    pub async fn agent_manager_appoint_child(
        &self,
        caller: Uuid,
        request: AgentManagerAppointChildRequestV1,
    ) -> Result<AgentManagerAppointChildResultV1> {
        let store = Arc::clone(self.store());
        appoint_child_with(
            &store,
            caller,
            &request,
            |session_id, project_id, appointment| {
                self.launch_child_seat(caller, &request, project_id, session_id, Some(appointment))
            },
        )
        .await
    }

    /// `AgentGlobalAppointManager`: the project-target alias of
    /// `AgentManagerAppointChild`.
    ///
    /// # Errors
    /// See [`appoint_child_with`].
    pub async fn agent_global_appoint_manager(
        &self,
        caller: Uuid,
        request: AgentGlobalAppointManagerRequestV1,
    ) -> Result<AgentGlobalAppointManagerResultV1> {
        let store = Arc::clone(self.store());
        let child = AgentManagerAppointChildRequestV1::from(&request);
        global_appoint_manager_with(&store, caller, &request, |session_id| {
            self.launch_child_seat(caller, &child, request.project_id, session_id, None)
        })
        .await
    }

    async fn launch_child_seat(
        &self,
        caller: Uuid,
        request: &AgentManagerAppointChildRequestV1,
        project_id: Uuid,
        session_id: Uuid,
        appointment: Option<ChildAppointment>,
    ) -> Result<Uuid> {
        let config = self
            .child_seat_launch_config(request, project_id, session_id)
            .await?;
        let portfolio = matches!(
            request.target,
            rsi_common::portfolio_delegation::AppointChildTargetV1::Portfolio { .. }
        );
        let publication = if portfolio {
            let appointment =
                appointment.expect("portfolio launch carries its admitted appointment");
            Some(Box::new(super::types::PortfolioAppointmentLaunchContext {
                caller,
                request: request.clone(),
                appointment,
            }))
        } else {
            None
        };
        self.launch_global_appointment(config, session_id, publication)
            .await
    }

    /// The launch of a child seat appointed with `project_id` as its
    /// admission project. Hub #3315: a portfolio seat (new child, replacement
    /// or self-handoff) is launched in the built-in home project at `$HOME`
    /// with no sandbox, never in a project of its coverage; `project_id` stays
    /// its admission and charge project. A store without the home project (it
    /// is registered at every daemon start) keeps the coverage project.
    /// Project PM seats are unchanged (#169's sandbox rule).
    pub(crate) async fn child_seat_launch_config(
        &self,
        request: &AgentManagerAppointChildRequestV1,
        project_id: Uuid,
        session_id: Uuid,
    ) -> Result<LaunchConfig> {
        let portfolio = matches!(
            request.target,
            rsi_common::portfolio_delegation::AppointChildTargetV1::Portfolio { .. }
        );
        let launch_project = if portfolio && self.get_project(HOME_ID).await?.is_some() {
            HOME_ID
        } else {
            project_id
        };
        let working_dir = self.resolve_project_working_dir(launch_project).await?;
        // The home project is an explicit no-sandbox case: `$HOME` is itself a
        // git repository, so it is never probed as a sandbox origin.
        let root_admits_sandbox =
            launch_project != HOME_ID && project_root_admits_sandbox(&working_dir).await;
        Ok(child_launch_config(
            request,
            launch_project,
            working_dir,
            session_id,
            root_admits_sandbox,
        ))
    }
}

/// The legacy PM launch conversion, retained for focused launch-shape tests.
#[cfg(test)]
pub(crate) fn global_manager_launch_config(
    request: &AgentGlobalAppointManagerRequestV1,
    working_dir: std::path::PathBuf,
    session_id: Uuid,
) -> LaunchConfig {
    child_launch_config(
        &AgentManagerAppointChildRequestV1::from(request),
        request.project_id,
        working_dir,
        session_id,
        // The launch-shape tests pin everything but the sandbox, which
        // `manager_seat_sandbox` and its launch tests pin.
        false,
    )
}

/// #169: whether a manager seat launched in `root` can get its own sandbox: the
/// allocation predicate itself (`origin_admits_sandbox`, a git work tree), run
/// off the async runtime because it starts a bounded `git` process.
pub(crate) async fn project_root_admits_sandbox(root: &std::path::Path) -> bool {
    let root = root.to_path_buf();
    tokio::task::spawn_blocking(move || crate::sandbox::git_worktree::origin_admits_sandbox(&root))
        .await
        .unwrap_or(false)
}

/// #169: the sandbox an appointed manager seat is launched with. A seat of a
/// git-backed project always gets its own git-worktree sandbox, like every
/// worker, so it submits jobs from its own tree and never writes the project's
/// shared root (hard rule 2); a caller's `sandbox: false` does not opt a
/// git-backed seat out. A seat of a non-git root gets none unless `true` is
/// asked, which the allocation then refuses (no worktree can be made), as before.
pub(crate) fn manager_seat_sandbox(
    requested: Option<bool>,
    root_admits_sandbox: bool,
) -> Option<rsi_common::types::SandboxSpec> {
    (root_admits_sandbox || requested == Some(true)).then_some(rsi_common::types::SandboxSpec {
        kind: Some(rsi_common::types::SandboxKind::GitWorktree),
        branch: None,
    })
}

/// A child seat's launch: a Standard root session in `project_id` (the
/// `:blank` launch shape) with the grant-checked provider, model and effort,
/// in its own sandbox when the project root admits one ([`manager_seat_sandbox`]);
/// never in the home project (hub #3315).
pub(crate) fn child_launch_config(
    request: &AgentManagerAppointChildRequestV1,
    project_id: Uuid,
    working_dir: std::path::PathBuf,
    session_id: Uuid,
    root_admits_sandbox: bool,
) -> LaunchConfig {
    {
        // Hub #3315: a home seat never gets a sandbox. `$HOME` is itself a git
        // repository, so the #169 rule would otherwise give it a worktree of
        // the operator's home repository; `sandbox: true` does not opt in.
        let sandbox = if HOME_ID == project_id {
            None
        } else {
            manager_seat_sandbox(request.sandbox, root_admits_sandbox)
        };
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
            project_id: Some(project_id),
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
            structured_handoff: false,
            exec_target: rsi_common::exec_target::ExecTarget::Host,
            bench_profile: None,
            loop_knobs: rsi_common::bench_profile::LoopKnobs::default(),
        }
    }
}

#[cfg(test)]
#[path = "global_manager_verbs_tests.rs"]
mod tests;
