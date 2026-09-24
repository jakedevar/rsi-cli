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
use rsi_common::manager_operator_delegation::DELEGABLE_OPERATOR_METHODS;
use rsi_common::types::{SessionKind, SessionStatus};
use rusqlite::{Transaction, TransactionBehavior, params};
use serde_json::json;
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// This is an advertisement, never an authorization grant. Callers must
/// refresh it after a revision change and keep server-side guards on calls.
#[derive(Debug, Clone)]
pub(crate) struct AgentAuthorityProjection {
    pub revision: String,
    pub pending: bool,
    pub is_lead: bool,
    pub is_manager: bool,
    pub is_reviewer: bool,
    pub verbs: Vec<Verb>,
    pub update_variants: Vec<&'static str>,
    pub control_actions: Vec<Action>,
    pub prepared_actions: Vec<&'static str>,
    pub delegated_operator_methods: Vec<&'static str>,
    pub guidance_ids: Vec<&'static str>,
}

const ACTIONS: &[(Action, Capability)] = &[
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
    ("handoff", Capability::WorkPlan, None, UpdateRole::Manager),
];

fn permitted_updates(
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
struct VerbRights {
    lead: bool,
    managed_lead: bool,
    managed_worker: bool,
    manager: bool,
    reviewer: bool,
    issue_lead: bool,
    issue_manager: bool,
    work_writer: bool,
    control: bool,
    prepared: bool,
}

fn permitted_verb(verb: Verb, rights: &VerbRights) -> bool {
    match verb {
        Verb::SpawnChild | Verb::ReserveSuccessor | Verb::ArchiveChild => rights.lead,
        Verb::GetProgress
        | Verb::SendMessage
        | Verb::GetStatus
        | Verb::Halt
        | Verb::ContinueChild
        | Verb::ScheduleWake
        | Verb::CreateIssue => true,
        Verb::ListIssues
        | Verb::GetIssue
        | Verb::UpdateIssue
        | Verb::UpdateIssueStatus
        | Verb::ArchiveIssue
        | Verb::RestoreIssue
        | Verb::ListIssueEvents => rights.issue_lead || rights.issue_manager,
        Verb::ManagerInbox => rights.managed_lead || rights.manager,
        Verb::ManagerReply | Verb::ManagerNotify => rights.managed_lead,
        Verb::ManagerWorkView => rights.managed_worker,
        Verb::ManagerProgress
        | Verb::ManagerSend
        | Verb::ManagerInspect
        | Verb::ManagerGetAction => rights.manager,
        Verb::ManagerUpdate => rights.work_writer,
        Verb::ManagerControl => rights.control,
        Verb::ManagerPrepareControl | Verb::ManagerCommitPreparedControl => rights.prepared,
        Verb::SubmitReviewReceipt => rights.reviewer,
    }
}

impl Store {
    /// Resolve one coherent SQLite read. No role, capability or caller identity
    /// is accepted from request JSON; `caller` is transport-bound upstream.
    pub(crate) fn agent_authority_projection(
        &self,
        caller: Uuid,
    ) -> Result<AgentAuthorityProjection> {
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
                (has(*capability)
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
        let delegated_operator_methods = if has(Capability::OperatorDelegation) {
            DELEGABLE_OPERATOR_METHODS.to_vec()
        } else {
            Vec::new()
        };
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
                current_tip
                    && managed
                    && self
                        .manager_v2_descendant_epic(config, caller)
                        .is_ok_and(|epic| config.epic_ids.contains(&epic))
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
        let custody = if reviewer_rows.iter().any(|row| row.1 == "active") {
            self.live_custody_for_session(caller).ok()
        } else {
            None
        };
        let is_reviewer = reviewer_rows.iter().any(|row| {
            if row.1 == "allocating" {
                return false;
            }
            let ready = Uuid::parse_str(&row.0).ok().is_some_and(|assignment_id| {
                self.prepare_manager_review_submission(
                    caller,
                    &AgentSubmitReviewReceiptRequestV1 {
                        assignment_id,
                        verdict: ManagerReviewVerdictV1::Accepted,
                        findings: Vec::new(),
                        idempotency_key: "authority-projection".into(),
                    },
                )
                .is_ok()
            });
            if !ready {
                pending = true;
            }
            ready
        });

        let rights = VerbRights {
            lead: is_lead,
            managed_lead: lead_in_manager_scope,
            managed_worker,
            manager: is_manager,
            reviewer: is_reviewer,
            issue_lead,
            issue_manager: manager_grant
                .is_some_and(|g| g.policy.capabilities.contains(&Capability::IssueCoordinate)),
            work_writer: !update_variants.is_empty(),
            control: !control_actions.is_empty(),
            prepared: !prepared_actions.is_empty(),
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
        });
        let revision = format!("sha256:{:x}", Sha256::digest(serde_json::to_vec(&facts)?));
        Ok(AgentAuthorityProjection {
            revision,
            pending,
            is_lead,
            is_manager,
            is_reviewer,
            verbs,
            update_variants,
            control_actions,
            prepared_actions,
            delegated_operator_methods,
            guidance_ids,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::agent_verbs::tests::test_session;
    use rsi_common::harness_manager::ConfigureHarnessManagerRequestV1;
    use rsi_common::harness_manager_v2::{ConfigureHarnessManagerPolicyRequestV2, ManagerPolicyV2};
    use rsi_common::types::Project;
    use std::path::PathBuf;

    #[test]
    fn manager_action_grants_cover_new_variants_and_closed_delegation() {
        assert_eq!(
            ACTIONS
                .iter()
                .find(|(action, _)| *action == Action::OperatorCall),
            Some(&(Action::OperatorCall, Capability::OperatorDelegation))
        );
        for action in [
            Action::SettleUncertainAction,
            Action::RetireLeadContinuations,
        ] {
            assert!(ACTIONS.contains(&(action, Capability::LeadControl)));
        }
        assert_eq!(DELEGABLE_OPERATOR_METHODS.len(), 4);
    }

    #[test]
    fn manager_update_variants_follow_role_and_grants() {
        let work_only = [Capability::WorkPlan];
        let manager = permitted_updates(&work_only, true, false);
        assert!(manager.contains(&"work"));
        assert!(manager.contains(&"stage"));
        assert!(!manager.contains(&"request"));
        assert!(!manager.contains(&"request_review"));
        assert!(!manager.contains(&"integration"));

        let lead = permitted_updates(&work_only, false, true);
        assert!(lead.contains(&"stage"));
        assert!(lead.contains(&"request"));
        assert!(!lead.contains(&"work"));
        assert!(!lead.contains(&"ownership"));

        let extended = [
            Capability::WorkPlan,
            Capability::SessionCreate,
            Capability::Integration,
        ];
        let lead_extended = permitted_updates(&extended, false, true);
        assert!(lead_extended.contains(&"request_review"));
        assert!(lead_extended.contains(&"integration"));
    }

    #[test]
    fn worker_baseline_and_role_specific_catalog_are_positive() {
        assert!(permitted_verb(Verb::GetStatus, &VerbRights::default()));
        assert!(permitted_verb(
            Verb::SpawnChild,
            &VerbRights {
                lead: true,
                ..Default::default()
            }
        ));
        assert!(permitted_verb(
            Verb::SubmitReviewReceipt,
            &VerbRights {
                reviewer: true,
                ..Default::default()
            }
        ));
        assert!(permitted_verb(
            Verb::ManagerWorkView,
            &VerbRights {
                managed_worker: true,
                ..Default::default()
            }
        ));
        assert!(permitted_verb(
            Verb::ManagerControl,
            &VerbRights {
                control: true,
                ..Default::default()
            }
        ));
    }

    #[test]
    fn committed_lead_publication_changes_revision_and_role() {
        let store = Store::open_in_memory().unwrap();
        let epic_id = Uuid::new_v4();
        let lead_id = Uuid::new_v4();
        let mut epic = test_session(epic_id, PathBuf::from("/tmp/authority-epic"));
        epic.session_kind = SessionKind::Epic;
        epic.status = SessionStatus::Running;
        epic.project_id = None;
        store.insert_session(&epic).unwrap();
        let mut lead = test_session(lead_id, PathBuf::from("/tmp/authority-lead"));
        lead.status = SessionStatus::Starting;
        lead.parent_id = Some(epic_id);
        lead.project_id = None;
        store.insert_session(&lead).unwrap();

        let pending = store.agent_authority_projection(lead_id).unwrap();
        assert!(pending.pending);
        assert!(!pending.is_lead);
        assert!(pending.verbs.contains(&Verb::GetStatus));

        store
            .conn
            .execute(
                "UPDATE sessions SET lead_session_id=?2,updated_at=?3 WHERE id=?1",
                params![
                    epic_id.to_string(),
                    lead_id.to_string(),
                    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
                ],
            )
            .unwrap();
        let published = store.agent_authority_projection(lead_id).unwrap();
        assert!(published.is_lead);
        assert!(published.verbs.contains(&Verb::SpawnChild));
        assert!(published.guidance_ids.contains(&"epic_lead"));
        assert_ne!(pending.revision, published.revision);
    }

    #[test]
    fn live_policy_publication_changes_manager_and_lead_catalogs() {
        let store = Store::open_in_memory().unwrap();
        let project = Uuid::new_v4();
        let manager_id = Uuid::new_v4();
        let group_id = Uuid::new_v4();
        let epic_id = Uuid::new_v4();
        let lead_id = Uuid::new_v4();
        let now = chrono::Utc::now();
        store
            .insert_project(&Project {
                id: project,
                name: "Authority projection".into(),
                path: None,
                description: None,
                color: Project::DEFAULT_COLOR.into(),
                context_files: None,
                created_at: now,
                updated_at: now,
            })
            .unwrap();
        let mut manager = test_session(manager_id, PathBuf::from("/tmp/authority-manager"));
        manager.project_id = Some(project);
        manager.status = SessionStatus::Running;
        store.insert_session(&manager).unwrap();
        let mut group = test_session(group_id, PathBuf::from("/tmp/authority-group"));
        group.project_id = Some(project);
        group.session_kind = SessionKind::Group;
        group.status = SessionStatus::Running;
        store.insert_session(&group).unwrap();
        let mut epic = test_session(epic_id, PathBuf::from("/tmp/authority-epic"));
        epic.project_id = Some(project);
        epic.session_kind = SessionKind::Epic;
        epic.status = SessionStatus::Running;
        epic.parent_id = Some(group_id);
        epic.lead_session_id = Some(lead_id);
        store.insert_session(&epic).unwrap();
        let mut lead = test_session(lead_id, PathBuf::from("/tmp/authority-lead"));
        lead.project_id = Some(project);
        lead.status = SessionStatus::Running;
        lead.parent_id = Some(epic_id);
        store.insert_session(&lead).unwrap();
        let lead_before_appointment = store.agent_authority_projection(lead_id).unwrap();
        assert!(lead_before_appointment.verbs.contains(&Verb::SpawnChild));
        assert!(!lead_before_appointment.verbs.contains(&Verb::ManagerInbox));
        store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                group_ids: Vec::new(),
                project_id: project,
                session_id: manager_id,
                epic_ids: Some(vec![epic_id]),
                expected_row_version: 0,
            })
            .unwrap();

        let before = store.agent_authority_projection(manager_id).unwrap();
        assert!(before.is_manager);
        assert!(before.verbs.contains(&Verb::ManagerInspect));
        assert!(!before.verbs.contains(&Verb::ManagerControl));

        let capabilities = vec![
            Capability::WorkPlan,
            Capability::IssueCoordinate,
            Capability::LeadControl,
            Capability::OperatorDelegation,
        ];
        store
            .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
                project_id: project,
                expected_scope_version: 1,
                expected_policy_version: 0,
                idempotency_key: "authority-policy".into(),
                policy: ManagerPolicyV2 {
                    mode: ManagerOperatingModeV2::Execute,
                    capabilities,
                    ..Default::default()
                },
            })
            .unwrap();
        let manager_after = store.agent_authority_projection(manager_id).unwrap();
        assert_ne!(before.revision, manager_after.revision);
        assert!(manager_after.verbs.contains(&Verb::ManagerUpdate));
        assert!(manager_after.verbs.contains(&Verb::ListIssues));
        assert!(manager_after.verbs.contains(&Verb::ManagerControl));
        assert!(
            manager_after
                .control_actions
                .contains(&Action::OperatorCall)
        );
        assert!(
            manager_after
                .control_actions
                .contains(&Action::RetireLeadContinuations)
        );
        assert_eq!(
            manager_after.delegated_operator_methods,
            DELEGABLE_OPERATOR_METHODS
        );

        let lead_after = store.agent_authority_projection(lead_id).unwrap();
        assert_ne!(lead_before_appointment.revision, lead_after.revision);
        assert!(lead_after.verbs.contains(&Verb::ManagerInbox));
        assert!(lead_after.verbs.contains(&Verb::ManagerUpdate));
        assert!(!lead_after.verbs.contains(&Verb::ManagerControl));

        store
            .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
                project_id: project,
                expected_scope_version: 1,
                expected_policy_version: 1,
                idempotency_key: "authority-policy-paused".into(),
                policy: ManagerPolicyV2 {
                    mode: ManagerOperatingModeV2::Execute,
                    paused: true,
                    capabilities: vec![
                        Capability::WorkPlan,
                        Capability::IssueCoordinate,
                        Capability::LeadControl,
                        Capability::OperatorDelegation,
                    ],
                    ..Default::default()
                },
            })
            .unwrap();
        let paused = store.agent_authority_projection(manager_id).unwrap();
        assert_ne!(manager_after.revision, paused.revision);
        assert!(paused.verbs.contains(&Verb::ListIssues));
        assert!(paused.verbs.contains(&Verb::ManagerUpdate));
        assert!(!paused.verbs.contains(&Verb::ManagerControl));
        assert!(paused.control_actions.is_empty());
        assert!(paused.delegated_operator_methods.is_empty());

        let reviewer_before_allocation = store.agent_authority_projection(lead_id).unwrap();
        let assignment_id = Uuid::new_v4();
        let at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        // Model action allocation precedes invocation/custody publication. The
        // synthetic action id represents that in-flight window only.
        store.conn.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
        store
            .conn
            .execute(
                "INSERT INTO manager_review_assignments (
                    assignment_id,project_id,epic_id,manager_session_id,scope_version,
                    work_key,spec_revision,author_session_id,source_sha,reviewer_session_id,
                    action_operation_id,state,row_version,request_json,request_fingerprint,
                    created_at,updated_at)
                 VALUES (?1,?2,?3,?4,1,'authority',1,?4,?5,?6,?7,
                         'allocating',1,'{}',?8,?9,?9)",
                params![
                    assignment_id.to_string(),
                    project.to_string(),
                    epic_id.to_string(),
                    manager_id.to_string(),
                    "0".repeat(40),
                    lead_id.to_string(),
                    Uuid::new_v4().to_string(),
                    format!("sha256:{}", "0".repeat(64)),
                    at,
                ],
            )
            .unwrap();
        store.conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();
        let allocating = store.agent_authority_projection(lead_id).unwrap();
        assert_ne!(reviewer_before_allocation.revision, allocating.revision);
        assert!(allocating.pending);
        assert!(!allocating.is_reviewer);
        assert!(!allocating.verbs.contains(&Verb::SubmitReviewReceipt));

        let next_lead_id = Uuid::new_v4();
        let mut next_lead = lead.clone();
        next_lead.id = next_lead_id;
        store.insert_session(&next_lead).unwrap();
        store
            .conn
            .execute(
                "UPDATE sessions SET lead_session_id=?2,updated_at=?3 WHERE id=?1",
                params![
                    epic_id.to_string(),
                    next_lead_id.to_string(),
                    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
                ],
            )
            .unwrap();
        let former = store.agent_authority_projection(lead_id).unwrap();
        let successor = store.agent_authority_projection(next_lead_id).unwrap();
        assert_ne!(lead_after.revision, former.revision);
        assert!(!former.verbs.contains(&Verb::SpawnChild));
        assert!(successor.verbs.contains(&Verb::SpawnChild));
        assert!(successor.guidance_ids.contains(&"epic_lead"));
    }
}
