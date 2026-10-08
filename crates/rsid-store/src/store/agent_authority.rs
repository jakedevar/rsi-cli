//! One durable, read-only authority snapshot for startup guidance and catalogs.
//!
//! S1 exposes the projection; S2/S3 consume it at provider and discovery
//! boundaries. A pending publication retains the worker baseline. Every
//! mutating call still passes its existing target-specific guard.

use crate::error::{DaemonError, Result};
use crate::store::Store;
use rsi_common::agent_control_schema::{AgentControlVerbV1 as Verb, agent_control_catalog_v1};
use rsi_common::harness_manager_v2::{
    AgentSubmitReviewReceiptRequestV1, ManagerActionKindV2 as Action,
    ManagerCapabilityV2 as Capability, ManagerOperatingModeV2, ManagerReviewVerdictV1,
};
use rsi_common::manager_operator_delegation::{
    DAEMON_SETTINGS_OPERATOR_METHODS, OPERATOR_DELEGATION_METHODS, STORAGE_CONTROL_OPERATOR_METHODS,
};
use rsi_common::types::{SessionKind, SessionStatus};
use rusqlite::{Transaction, TransactionBehavior, params};
use serde_json::json;
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// This is an advertisement, never an authorization grant. Callers must
/// refresh it after a revision change and keep server-side guards on calls.
#[derive(Debug, Clone)]
pub struct AgentAuthorityProjection {
    pub revision: String,
    pub pending: bool,
    pub is_lead: bool,
    pub is_manager: bool,
    pub is_reviewer: bool,
    /// #872: the caller is the active global manager seat.
    pub is_global_manager: bool,
    pub verbs: Vec<Verb>,
    pub update_variants: Vec<&'static str>,
    pub control_actions: Vec<Action>,
    pub prepared_actions: Vec<&'static str>,
    pub delegated_operator_methods: Vec<&'static str>,
    pub guidance_ids: Vec<&'static str>,
}

pub const ACTIONS: &[(Action, Capability)] = &[
    (Action::SucceedManager, Capability::SelfSuccession),
    (Action::ResumeLead, Capability::LeadControl),
    (Action::PauseLead, Capability::LeadControl),
    (Action::RetryLead, Capability::LeadControl),
    (Action::ReplaceLead, Capability::LeadControl),
    (Action::SettleUncertainAction, Capability::LeadControl),
    (Action::RetireLeadContinuations, Capability::LeadControl),
    (Action::CreateContainer, Capability::Topology),
    (Action::UpdateContainer, Capability::Topology),
    (Action::ArchiveContainer, Capability::Topology),
    (Action::DeleteContainer, Capability::Topology),
    (Action::RestoreContainer, Capability::Topology),
    (Action::CreateSession, Capability::SessionCreate),
    (Action::AssignLead, Capability::LeadAssign),
    (Action::Integrate, Capability::GitEffect),
    (Action::ArchiveSession, Capability::SessionControl),
    (Action::RestoreSession, Capability::SessionControl),
    (Action::UpdateSession, Capability::SessionControl),
    (Action::OperatorCall, Capability::OperatorDelegation),
];

/// #1235: capabilities the global arm withholds in S1 (the store refuses them
/// with `manager_v2_capability_denied`), so the catalog never lists them.
const GLOBAL_WITHHELD: &[Capability] = &[
    Capability::SelfSuccession,
    Capability::GitEffect,
    Capability::OperatorDelegation,
    Capability::StorageControl,
    Capability::DaemonSettings,
];

/// #1235: update variants the global arm does not drive in S1.
const GLOBAL_WITHHELD_UPDATES: &[&str] = &[
    "request_review",
    "migration",
    "migration_transfer",
    "migration_release",
];

const PREPARED: &[(&str, Capability)] = &[
    ("resume_lead", Capability::LeadControl),
    ("pause_lead", Capability::LeadControl),
    ("retry_lead", Capability::LeadControl),
    ("replace_lead", Capability::LeadControl),
    ("create_session", Capability::SessionCreate),
    ("assign_lead", Capability::LeadAssign),
];

#[derive(Clone, Copy)]
enum UpdateRole {
    Manager,
    Lead,
    Either,
}

const UPDATES: &[(&str, Capability, Option<Capability>, UpdateRole)] = &[
    ("work", Capability::WorkPlan, None, UpdateRole::Manager),
    ("stage", Capability::WorkPlan, None, UpdateRole::Either),
    (
        "dependency",
        Capability::WorkPlan,
        None,
        UpdateRole::Manager,
    ),
    ("ownership", Capability::WorkPlan, None, UpdateRole::Manager),
    ("migration", Capability::WorkPlan, None, UpdateRole::Manager),
    (
        "migration_transfer",
        Capability::WorkPlan,
        None,
        UpdateRole::Manager,
    ),
    (
        "migration_release",
        Capability::WorkPlan,
        None,
        UpdateRole::Manager,
    ),
    (
        "request_review",
        Capability::WorkPlan,
        Some(Capability::SessionCreate),
        UpdateRole::Either,
    ),
    ("accept", Capability::WorkPlan, None, UpdateRole::Either),
    (
        "integration",
        Capability::Integration,
        None,
        UpdateRole::Either,
    ),
    ("request", Capability::WorkPlan, None, UpdateRole::Lead),
    ("decision", Capability::WorkPlan, None, UpdateRole::Either),
    // #1415: a delegated manager's ruling on a non-gate decision record, and
    // the owning manager's withdrawal of its own.
    (
        "decision_ruling",
        Capability::WorkPlan,
        None,
        UpdateRole::Manager,
    ),
    (
        "decision_withdraw",
        Capability::WorkPlan,
        None,
        UpdateRole::Manager,
    ),
    ("handoff", Capability::WorkPlan, None, UpdateRole::Manager),
];

pub fn permitted_updates(
    capabilities: &[Capability],
    manager: bool,
    managed_lead: bool,
) -> Vec<&'static str> {
    UPDATES
        .iter()
        .filter_map(|(name, capability, extra, role)| {
            let role_allowed = match role {
                UpdateRole::Manager => manager,
                UpdateRole::Lead => managed_lead,
                UpdateRole::Either => manager || managed_lead,
            };
            (role_allowed
                && capabilities.contains(capability)
                && extra.is_none_or(|required| capabilities.contains(&required)))
            .then_some(*name)
        })
        .collect()
}

#[derive(Default)]
pub struct VerbRights {
    pub lead: bool,
    pub managed_lead: bool,
    pub managed_worker: bool,
    pub manager: bool,
    pub reviewer: bool,
    pub issue_lead: bool,
    pub issue_manager: bool,
    /// #1235: Issue reads only (a Status-mode global project policy).
    pub issue_reader: bool,
    /// #1235: the global seat's project-manager reads (progress, inspect,
    /// action receipts) inside its grant.
    pub portfolio_reader: bool,
    /// #1235: the global seat lands from an in-reach session's sandbox.
    pub portfolio_lander: bool,
    /// A worker bound to one Issue by `AgentManagerLaunchIssueWorker`.
    pub issue_bound: bool,
    /// #1284: that binding is live, so the worker may append to its Issue.
    pub issue_bound_writer: bool,
    pub session_create: bool,
    pub work_writer: bool,
    pub control: bool,
    pub prepared: bool,
    pub topology_manager: bool,
    pub deploy: bool,
    pub global_manager: bool,
    /// #1626: an Execute-mode, unpaused project manager or portfolio seat may
    /// register and edit projects inside its coverage.
    pub project_admin: bool,
    pub global_reporter: bool,
    /// #1238: the caller holds a manager node seat (area, project or
    /// portfolio), so it may report up and send down.
    pub tier_seat: bool,
    /// #1238: an Epic lead in a project with no live PM that a portfolio node
    /// covers: its `AgentManagerNotify` goes to that node.
    pub portfolio_lead: bool,
}

/// Whether every live leaf session holds `verb` regardless of role. This is
/// the baseline a pending role publication keeps advertising.
pub fn is_baseline_verb(verb: Verb) -> bool {
    permitted_verb(verb, &VerbRights::default())
}

pub fn permitted_verb(verb: Verb, rights: &VerbRights) -> bool {
    match verb {
        Verb::SpawnChild | Verb::ReserveSuccessor | Verb::ArchiveChild => rights.lead,
        Verb::GetAuthorityCatalog
        | Verb::GetProgress
        | Verb::SendMessage
        | Verb::GetStatus
        | Verb::Halt
        | Verb::ContinueChild
        | Verb::ScheduleWake
        | Verb::CancelWake
        | Verb::ListWakes
        | Verb::ReadSessionEvents
        | Verb::CreateIssue
        | Verb::SubmitJob
        | Verb::GetJob
        | Verb::ListJobs
        | Verb::CancelJob
        | Verb::QueryFailureSignatures => true,
        Verb::GetIssue => {
            rights.issue_lead || rights.issue_manager || rights.issue_reader || rights.issue_bound
        }
        Verb::ManagerLaunchIssueWorker => rights.session_create && rights.issue_manager,
        Verb::ListIssues | Verb::ListIssueEvents => {
            rights.issue_lead || rights.issue_manager || rights.issue_reader
        }
        Verb::UpdateIssue => rights.issue_lead || rights.issue_manager || rights.issue_bound_writer,
        Verb::UpdateIssueStatus | Verb::ArchiveIssue | Verb::RestoreIssue => {
            rights.issue_lead || rights.issue_manager
        }
        Verb::ManagerInbox => rights.managed_lead || rights.manager,
        Verb::ManagerReply => rights.managed_lead,
        Verb::ManagerNotify => rights.managed_lead || rights.portfolio_lead,
        Verb::EnqueueLandingSource => rights.lead || rights.manager || rights.portfolio_lander,
        Verb::GetProviderStatus | Verb::GetDaemonInfo => rights.lead || rights.manager,
        Verb::SendSatelliteMessage | Verb::ReportToHub => rights.manager,
        Verb::RequestDeploy => rights.deploy,
        Verb::ManagerWorkView => rights.managed_worker,
        Verb::ManagerProgress | Verb::ManagerInspect | Verb::ManagerGetAction
            if rights.portfolio_reader =>
        {
            true
        }
        // #1238: a portfolio seat rules or forwards the escalation hops
        // addressed to it.
        Verb::ManagerListEscalations | Verb::ManagerResolveEscalation if rights.global_manager => {
            true
        }
        Verb::ManagerProgress
        | Verb::ManagerSend
        | Verb::ManagerInspect
        | Verb::ManagerDelegateNode
        | Verb::ManagerEscalate
        | Verb::ManagerListEscalations
        | Verb::ManagerResolveEscalation
        | Verb::ManagerGetAction => rights.manager,
        Verb::ManagerUpdate => rights.work_writer,
        Verb::ManagerControl => rights.control,
        Verb::ManagerPrepareControl | Verb::ManagerCommitPreparedControl => rights.prepared,
        Verb::SubmitReviewReceipt => rights.reviewer,
        Verb::GlobalOverview
        | Verb::GlobalSend
        | Verb::GlobalAppointManager
        // #1239: any portfolio node seat delegates to its children.
        | Verb::ManagerAppointChild
        | Verb::ManagerRevokeChild => rights.global_manager,
        Verb::CreateProject | Verb::UpdateProject => rights.project_admin,
        Verb::ReportToGlobal => rights.global_reporter,
        Verb::ReportUp | Verb::SendDown | Verb::ManagerOverview => rights.tier_seat,
        Verb::TopologyUpsert
        | Verb::TopologyList
        | Verb::TopologyExecute
        | Verb::TopologyGetExecution
        | Verb::TopologyInterrupt
        | Verb::TopologyResolveAttempt => rights.lead || rights.topology_manager,
    }
}

impl Store {
    /// #1277-#1279: the stable authority fence of `caller` (and, for a verb
    /// that acts on another session's sandbox, its `target`). It names the
    /// grant epoch and version, never a volatile field such as a timestamp,
    /// so an effect that re-resolves it under the store lock refuses exactly
    /// when the admitted authority changed (a revoked, replaced or rotated
    /// grant, a retired seat, a moved project).
    ///
    /// # Errors
    /// `agent_authority_caller_unavailable` for a caller that is not a live
    /// leaf, or a persistence error.
    pub fn agent_authority_fence(&self, caller: Uuid, target: Option<Uuid>) -> Result<String> {
        let _snapshot = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        let session = self
            .get_session(caller)?
            .filter(|s| {
                rsi_common::is_leaf_kind(s.session_kind)
                    && !matches!(s.status, SessionStatus::Archived | SessionStatus::Deleted)
            })
            .ok_or_else(|| {
                DaemonError::InvalidParam("agent_authority_caller_unavailable".into())
            })?;
        let lead = match session.parent_id {
            Some(parent) => self
                .get_session(parent)?
                .filter(|epic| epic.session_kind == SessionKind::Epic)
                .map(|epic| (epic.id, epic.lead_session_id, epic.project_id)),
            None => None,
        };
        let (manager, policy) = match session.project_id {
            Some(project) => (
                self.get_harness_manager(project)?
                    .map(|c| (c.manager_session_id, c.current_session_id, c.row_version)),
                self.get_harness_manager_policy(project)?.map(|g| {
                    (
                        g.manager_session_id,
                        g.scope_version,
                        g.row_version,
                        g.revoked,
                    )
                }),
            ),
            None => (None, None),
        };
        let seat_grant = self
            .portfolio_seat_grant(caller)?
            .map(|grant| (grant.grant_id, grant.grant_version, grant.seat_session_id));
        let area = self
            .manager_area_authority_current(caller)
            .ok()
            .flatten()
            .map(|a| {
                (
                    a.config.manager_session_id,
                    a.config.row_version,
                    a.grant.manager_session_id,
                    a.grant.scope_version,
                    a.grant.row_version,
                )
            });
        let target = match target {
            Some(target) => self
                .manager_session_control_scope(caller, target, false)?
                .map(|scope| {
                    let grant = self
                        .manager_policy_for_config(&scope.config)
                        .ok()
                        .flatten()
                        .map(|g| {
                            (
                                g.manager_session_id,
                                g.scope_version,
                                g.row_version,
                                g.revoked,
                            )
                        });
                    (
                        scope.target.id,
                        scope.target.project_id,
                        scope.config.manager_session_id,
                        scope.config.current_session_id,
                        scope.config.row_version,
                        grant,
                    )
                }),
            None => None,
        };
        let covering = match session.project_id {
            Some(project) => self
                .covering_portfolio_grant(project)?
                .map(|record| (record.grant.grant_version, record.grant.seat_session_id)),
            None => None,
        };
        Ok(format!(
            "v1:{}",
            json!({
                "caller": caller,
                "project": session.project_id,
                "lead": lead,
                "manager": manager,
                "policy": policy,
                "seat_grant": seat_grant,
                "covering": covering,
                "area": area,
                "target": target,
            })
        ))
    }

    /// Resolve one coherent SQLite read. No role, capability or caller identity
    /// is accepted from request JSON; `caller` is transport-bound upstream.
    pub fn agent_authority_projection(&self, caller: Uuid) -> Result<AgentAuthorityProjection> {
        let _snapshot = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        let session = self
            .get_session(caller)?
            .filter(|s| {
                rsi_common::is_leaf_kind(s.session_kind)
                    && !matches!(s.status, SessionStatus::Archived | SessionStatus::Deleted)
            })
            .ok_or_else(|| {
                DaemonError::InvalidParam("agent_authority_caller_unavailable".into())
            })?;

        let parent = session
            .parent_id
            .map(|id| self.get_session(id))
            .transpose()?
            .flatten();
        let is_lead = parent.as_ref().is_some_and(|epic| {
            epic.session_kind == SessionKind::Epic
                && epic.lead_session_id == Some(caller)
                && epic.project_id == session.project_id
                && !matches!(
                    epic.status,
                    SessionStatus::Archived | SessionStatus::Deleted
                )
        });
        let issue_lead = if is_lead {
            match Store::resolve_agent_issue_authority_tx(&_snapshot, caller) {
                Ok(_) => true,
                Err(DaemonError::PolicyDenied(_)) => false,
                Err(error) => return Err(error),
            }
        } else {
            false
        };
        let mut pending = session.status == SessionStatus::Starting
            && parent.as_ref().is_some_and(|epic| {
                epic.session_kind == SessionKind::Epic && epic.lead_session_id.is_none()
            });

        let config = session
            .project_id
            .map(|project| self.get_harness_manager(project))
            .transpose()?
            .flatten();
        let grant = session
            .project_id
            .map(|project| self.get_harness_manager_policy(project))
            .transpose()?
            .flatten();
        let is_manager = config
            .as_ref()
            .is_some_and(|c| c.current_session_id == Some(caller));
        let lead_in_manager_scope = is_lead
            && config.as_ref().is_some_and(|c| {
                c.current_session_id.is_some()
                    && parent
                        .as_ref()
                        .is_some_and(|epic| c.epic_ids.contains(&epic.id))
            });
        let active_grant = grant.as_ref().filter(|g| {
            !g.revoked
                && config.as_ref().is_some_and(|c| {
                    g.manager_session_id == c.manager_session_id && g.scope_version == c.row_version
                })
        });
        let manager_grant = active_grant.filter(|_| is_manager);
        let execute = manager_grant
            .filter(|g| g.policy.mode == ManagerOperatingModeV2::Execute && !g.policy.paused);
        let has = |capability| execute.is_some_and(|g| g.policy.capabilities.contains(&capability));
        let control_actions = ACTIONS
            .iter()
            .filter_map(|(action, capability)| {
                // #1043/#1046: `StorageControl` or `DaemonSettings` alone reach
                // `operator_call` for the methods each unlocks.
                ((has(*capability)
                    || (*action == Action::OperatorCall
                        && (has(Capability::StorageControl) || has(Capability::DaemonSettings))))
                    && (*action != Action::SucceedManager
                        || (session.session_kind == SessionKind::Standard
                            && session.parent_id.is_none())))
                .then_some(*action)
            })
            .collect::<Vec<_>>();
        let prepared_actions = PREPARED
            .iter()
            .filter_map(|(name, capability)| has(*capability).then_some(*name))
            .collect::<Vec<_>>();
        let mut delegated_operator_methods = Vec::new();
        if has(Capability::OperatorDelegation) {
            delegated_operator_methods.extend_from_slice(OPERATOR_DELEGATION_METHODS);
        }
        if has(Capability::StorageControl) {
            delegated_operator_methods.extend_from_slice(STORAGE_CONTROL_OPERATOR_METHODS);
        }
        if has(Capability::DaemonSettings) {
            delegated_operator_methods.extend_from_slice(DAEMON_SETTINGS_OPERATOR_METHODS);
        }
        delegated_operator_methods.sort_unstable();
        let update_variants = active_grant.map_or_else(Vec::new, |g| {
            permitted_updates(&g.policy.capabilities, is_manager, lead_in_manager_scope)
        });

        let managed_worker = if let (Some(project), Some(config), Some(grant)) =
            (session.project_id, &config, &grant)
        {
            if !is_manager
                && config.current_session_id.is_some()
                && !grant.revoked
                && grant.manager_session_id == config.manager_session_id
                && grant.scope_version == config.row_version
            {
                let root = self.manager_lineage_root(caller)?;
                let current_tip = self.manager_lineage_tip(root)? == caller;
                let managed: bool = self.conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM harness_manager_v2_entities \
                     WHERE session_id=?1 AND project_id=?2 AND manager_session_id=?3 \
                     AND scope_version=?4 AND kind='session')",
                    params![
                        root.to_string(),
                        project.to_string(),
                        config.manager_session_id.to_string(),
                        config.row_version
                    ],
                    |row| row.get::<_, bool>(0),
                )?;
                if current_tip && managed {
                    match self.manager_v2_descendant_epic(config, caller) {
                        Ok(epic) => config.epic_ids.contains(&epic),
                        Err(DaemonError::InvalidParam(reason))
                            if matches!(
                                reason.as_str(),
                                "manager_v2_epic_out_of_scope" | "manager_v2_session_out_of_scope"
                            ) =>
                        {
                            false
                        }
                        Err(error) => return Err(error),
                    }
                } else {
                    false
                }
            } else {
                false
            }
        } else {
            false
        };

        // An allocating assignment is a pending role change, never reviewer
        // authority. An active assignment must match this invocation and live
        // custody, as the receipt guard requires at call time.
        let mut reviewer_rows = Vec::new();
        {
            let mut statement = self.conn.prepare(
                "SELECT assignment_id,state,row_version \
                 FROM manager_review_assignments \
                 WHERE reviewer_session_id=?1 AND state IN ('allocating','active') \
                 ORDER BY updated_at DESC,assignment_id DESC LIMIT 65",
            )?;
            let rows = statement.query_map([caller.to_string()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?;
            for row in rows {
                reviewer_rows.push(row?);
            }
        }
        if reviewer_rows.len() > 64 {
            return Err(DaemonError::InvalidParam(
                "agent_authority_reviewer_limit".into(),
            ));
        }
        pending |= reviewer_rows.iter().any(|row| row.1 == "allocating");
        let invocation = self.session_model_invocation_id(caller)?;
        let custody = reviewer_rows
            .iter()
            .any(|row| row.1 == "active")
            .then(|| self.live_custody_for_session(caller))
            .transpose()?;
        let mut is_reviewer = false;
        for row in &reviewer_rows {
            if row.1 == "allocating" {
                continue;
            }
            let assignment_id = Uuid::parse_str(&row.0)
                .map_err(|_| DaemonError::Store("invalid reviewer assignment identity".into()))?;
            match self.prepare_manager_review_submission(
                caller,
                &AgentSubmitReviewReceiptRequestV1 {
                    assignment_id,
                    verdict: ManagerReviewVerdictV1::Accepted,
                    findings: Vec::new(),
                    idempotency_key: "authority-projection".into(),
                },
            ) {
                Ok(_) => is_reviewer = true,
                Err(DaemonError::InvalidParam(_)) | Err(DaemonError::PolicyDenied(_)) => {
                    pending = true;
                }
                Err(error) => return Err(error),
            }
        }

        // #1236: the caller's own portfolio node (one active grant per seat).
        let active_global = self.portfolio_seat_grant(caller)?;
        let is_global_manager = active_global.is_some();
        // The node covering the caller's project decides `AgentReportToGlobal`.
        let covering_global = match session.project_id {
            Some(project) => self
                .covering_portfolio_grant(project)?
                .map(|record| (record.grant.grant_version, record.grant.seat_session_id)),
            None => None,
        };
        let global_grant_version = (
            active_global
                .as_ref()
                .map(|grant| (grant.grant_version, grant.seat_session_id)),
            covering_global,
        );
        // #1235: the global seat holds the PM verb set its grant's project
        // policy grants (one policy for every covered project in S1). Execute
        // mode lists controls; Status mode lists reads only.
        let global_policy = active_global
            .as_ref()
            .filter(|_| is_global_manager)
            .map(|grant| grant.project_policy.clone());
        let global_executes = global_policy
            .as_ref()
            .is_some_and(|p| p.mode == ManagerOperatingModeV2::Execute && !p.paused);
        let global_has = |capability: Capability| {
            global_executes
                && !GLOBAL_WITHHELD.contains(&capability)
                && global_policy
                    .as_ref()
                    .is_some_and(|p| p.capabilities.contains(&capability))
        };
        let global_issue = global_policy
            .as_ref()
            .is_some_and(|p| p.capabilities.contains(&Capability::IssueCoordinate));
        let mut control_actions = control_actions;
        let mut prepared_actions = prepared_actions;
        let mut update_variants = update_variants;
        if global_executes {
            for (action, capability) in ACTIONS {
                if global_has(*capability) && !control_actions.contains(action) {
                    control_actions.push(*action);
                }
            }
            for (name, capability) in PREPARED {
                if global_has(*capability) && !prepared_actions.contains(name) {
                    prepared_actions.push(*name);
                }
            }
            let capabilities = global_policy
                .as_ref()
                .map(|p| p.capabilities.clone())
                .unwrap_or_default();
            for name in permitted_updates(&capabilities, true, false) {
                if !GLOBAL_WITHHELD_UPDATES.contains(&name) && !update_variants.contains(&name) {
                    update_variants.push(name);
                }
            }
        }
        let rights = VerbRights {
            lead: is_lead,
            managed_lead: lead_in_manager_scope,
            managed_worker,
            manager: is_manager,
            reviewer: is_reviewer,
            issue_lead,
            issue_manager: manager_grant
                .is_some_and(|g| g.policy.capabilities.contains(&Capability::IssueCoordinate))
                || (global_issue && global_executes),
            issue_reader: global_issue,
            portfolio_reader: global_policy.is_some(),
            portfolio_lander: global_executes,
            issue_bound: Self::bound_issue_for_worker_on(&self.conn, caller)?.is_some(),
            issue_bound_writer: Self::live_issue_binding_for_worker_on(&self.conn, caller)?
                .is_some(),
            session_create: has(Capability::SessionCreate) || global_has(Capability::SessionCreate),
            work_writer: !update_variants.is_empty(),
            control: !control_actions.is_empty(),
            prepared: !prepared_actions.is_empty(),
            deploy: has(Capability::Deploy) || global_has(Capability::Deploy),
            topology_manager: manager_grant
                .is_some_and(|g| g.policy.capabilities.contains(&Capability::Automation))
                || global_has(Capability::Automation),
            global_manager: is_global_manager,
            project_admin: self.agent_project_admin(caller)?,
            global_reporter: is_manager && self.global_reporter(caller)?,
            tier_seat: self.tier_caller_node(caller)?.is_some(),
            portfolio_lead: is_lead
                && match session.project_id {
                    Some(project) => {
                        self.global_live_manager(project)?.is_none()
                            && super::portfolio_nodes::covering_node_on(&self.conn, project)?
                                .is_some()
                    }
                    None => false,
                },
        };
        let verbs = agent_control_catalog_v1()
            .iter()
            .filter_map(|entry| permitted_verb(entry.verb, &rights).then_some(entry.verb))
            .collect();
        let mut guidance_ids = vec!["common", "worker"];
        if is_lead {
            guidance_ids.push("epic_lead");
        }
        if is_manager {
            guidance_ids.push("manager");
        }
        if is_reviewer {
            guidance_ids.push("assigned_reviewer");
        }
        if is_global_manager {
            guidance_ids.push("global_manager");
            guidance_ids.push("portfolio_manager");
        }
        let facts = json!({
            "caller": caller,
            "status": session.status,
            "session_updated_at": session.updated_at,
            "parent": parent.as_ref().map(|p| (p.id,p.lead_session_id,p.updated_at)),
            "manager": config.as_ref().map(|c| (c.manager_session_id,c.current_session_id,c.row_version)),
            "policy": grant.as_ref().map(|g| (g.row_version,g.revoked,&g.policy)),
            "reviewer": reviewer_rows,
            "invocation": invocation,
            "custody": custody.as_ref().map(|c| (c.custody_id,c.generation,&c.source_commit)),
            "pending": pending,
            "global": global_grant_version,
        });
        let revision = format!("sha256:{:x}", Sha256::digest(serde_json::to_vec(&facts)?));
        Ok(AgentAuthorityProjection {
            revision,
            pending,
            is_lead,
            is_manager,
            is_reviewer,
            is_global_manager,
            verbs,
            update_variants,
            control_actions,
            prepared_actions,
            delegated_operator_methods,
            guidance_ids,
        })
    }
}
