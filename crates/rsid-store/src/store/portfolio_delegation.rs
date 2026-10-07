//! Fractal manager hierarchy S5 (#1239, plan §2.4, §3, §5 M3): delegation at
//! every portfolio level.
//!
//! A portfolio node's active seat may appoint, replace or revoke a child
//! inside its own grant:
//! - **Project target.** Appoint or replace the project manager (PM) of a
//!   covered project; the project is saved with the node's `child_policy`
//!   (its own policy when unset).
//! - **New child node.** A portfolio node under the caller's node (grantor
//!   `node:<caller node>`) over a strict subset of its coverage, disjoint from
//!   its other children, narrowing it in every dimension (`grant_narrows`).
//! - **Seat replacement.** A new seat for a child the node granted; the
//!   child keeps its grant and epoch, so its ledger and workers stay.
//! - **Revoke.** A child the node granted, with every node whose authority
//!   came from it; operator-granted descendants re-parent. An operator-granted
//!   child is the operator's (`manager_child_operator_granted`).
//!
//! The seat's launch is a session the grantor creates (#1314): admission and
//! the Model Control admission immediately before the provider runs apply
//! the gates of the grantor's own `create_session` (its policy and every
//! ancestor's pause, concurrency, provider, spend and creation allowance),
//! the launch reserves its slot in the grantor's ledger, and the appointment
//! is charged to the grantor and its ancestors (#1301).
//!
//! Every refusal happens before a session is created: the appointment row
//! is recorded with a daemon-reserved session id only after all checks pass,
//! and the launch persists exactly that id, so a replay never launches twice.
//! The checks run again when the launched seat is appointed. The grantor's
//! authority epoch is recorded: an operator re-grant of the grantor between
//! launch and appointment refuses the appointment (`global_manager_not_seat`),
//! while a context-cap seat transfer (same epoch) lets the successor finish
//! it. The direct-report cap counts a node's active child nodes and the live
//! PMs of the projects it covers deepest (its child slots, plan §2.4).

use rsi_common::global_manager::{GLOBAL_MANAGER_NOT_SEAT, MANAGER_PROJECT_NOT_IN_SCOPE};
use rsi_common::grant_narrowing::{handed_policy_narrows, portfolio_effective_launches};
use rsi_common::harness_manager::ConfigureHarnessManagerRequestV1;
use rsi_common::harness_manager_v2::{
    ConfigureHarnessManagerPolicyRequestV2, ManagerCapabilityV2, ManagerOperatingModeV2,
    ManagerPolicyV2,
};
use rsi_common::portfolio_delegation::{
    AgentManagerAppointChildRequestV1, AgentManagerRevokeChildRequestV1, AppointChildTargetV1,
    MANAGER_CHILD_OPERATOR_GRANTED, MANAGER_DIRECT_REPORT_CAP, MANAGER_NODE_NOT_IN_SCOPE,
    MANAGER_SCOPE_NOT_NARROWED,
};
use rsi_common::portfolio_nodes::{
    ConfigurePortfolioNodeRequestV1, MANAGER_NODE_STALE, MANAGER_SCOPE_OVERLAP,
    PORTFOLIO_DEFAULT_MAX_DIRECT_REPORTS, PORTFOLIO_IDEMPOTENCY_CONFLICT, PortfolioNodeV1,
    RevokePortfolioNodeRequestV1,
};
use rsi_common::types::{SessionKind, SessionStatus};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{
    GrantRecord, PORTFOLIO_DEPTH_LIMIT, PortfolioGrantor, PortfolioRevokeOutcome,
    active_children_on, covering_node_on, grant_by_key_on, latest_node_grant_on, node_depth_on,
    node_grant_on, node_row_on, seat_grant_on,
};
use crate::error::{DaemonError, Result};
use crate::store::Store;

/// #1290, #1299: how long a launched appointment without a session row
/// still reserves its report after it was reserved or last renewed by a
/// replay (a launch in flight).
const RESERVATION_LAUNCH_GRACE_SECONDS: i64 = 900;

/// #1314: an appointment is a charged creation of its grantor once its seat
/// exists, or while its launch is in flight (reserved or renewed within
/// [`RESERVATION_LAUNCH_GRACE_SECONDS`]). `?1` is the grace cutoff.
const APPOINTMENT_CHARGED: &str =
    "(EXISTS(SELECT 1 FROM sessions s WHERE s.id=a.session_id) OR a.reserved_at>=?1)";

/// The reservation cutoff: launches reserved before it have lapsed.
fn grace_cutoff() -> String {
    (chrono::Utc::now() - chrono::Duration::seconds(RESERVATION_LAUNCH_GRACE_SECONDS))
        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

/// Schema version of `manager_portfolio_appointments` (M3,
/// `store/migrations/v156.rs`).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const PORTFOLIO_APPOINTMENT_SCHEMA_VERSION: i32 = 156;

/// M3 catalog objects, for presence assertions.
#[cfg(test)]
pub(crate) const CATALOG_OBJECTS: [(&str, &str); 5] = [
    ("table", "manager_portfolio_appointments"),
    ("index", "manager_portfolio_appointments_by_target"),
    ("trigger", "manager_portfolio_appointments_no_delete"),
    (
        "trigger",
        "manager_portfolio_appointments_identity_immutable",
    ),
    ("trigger", "manager_portfolio_appointments_appointed_final"),
];

/// Teardown for the fixture rewind (back below V156), newest object first.
#[cfg(test)]
pub(crate) const REWIND_SQL: &str = "DROP TRIGGER manager_portfolio_appointments_appointed_final;
DROP TRIGGER manager_portfolio_appointments_identity_immutable;
DROP TRIGGER manager_portfolio_appointments_no_delete;
DROP INDEX manager_portfolio_appointments_by_target;
DROP TABLE manager_portfolio_appointments;";

fn refused(code: &str) -> DaemonError {
    DaemonError::InvalidParam(code.into())
}

fn stamp() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

fn parse_uuid(text: &str) -> Result<Uuid> {
    Uuid::parse_str(text)
        .map_err(|_| DaemonError::Store("invalid portfolio appointment identity".into()))
}

fn digest(value: &impl serde::Serialize) -> Result<String> {
    let bytes = serde_json::to_vec(value)
        .map_err(|error| DaemonError::Store(format!("portfolio appointment digest: {error}")))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

/// The child an appointment seats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildTargetRef {
    /// The PM seat of a project.
    Project(Uuid),
    /// A child portfolio node (reserved before it exists, for a new child).
    Portfolio(Uuid),
}

impl ChildTargetRef {
    /// `project:<id>` or `portfolio:<id>`.
    #[must_use]
    pub fn as_column(self) -> String {
        match self {
            Self::Project(id) => format!("project:{id}"),
            Self::Portfolio(id) => format!("portfolio:{id}"),
        }
    }

    fn parse(text: &str) -> Result<Self> {
        if let Some(id) = text.strip_prefix("project:") {
            return Ok(Self::Project(parse_uuid(id)?));
        }
        if let Some(id) = text.strip_prefix("portfolio:") {
            return Ok(Self::Portfolio(parse_uuid(id)?));
        }
        Err(DaemonError::Store(
            "invalid portfolio appointment target".into(),
        ))
    }
}

/// One recorded delegated appointment.
#[derive(Debug, Clone)]
pub struct ChildAppointment {
    pub id: Uuid,
    pub grantor_node_id: Uuid,
    pub grantor_authority_epoch: i64,
    pub target: ChildTargetRef,
    /// The project the seat launches in.
    pub launch_project_id: Uuid,
    /// The daemon-reserved session id the launch must persist.
    pub session_id: Uuid,
    /// `(scope_version, policy_version)` once appointed: a project's manager
    /// scope and policy versions, or a child node's epoch and grant version.
    pub appointed: Option<(i64, i64)>,
    /// A project target's exact policy request, recorded before it is sent.
    pub policy_request: Option<ConfigureHarnessManagerPolicyRequestV2>,
    /// The row existed under this `(grantor, idempotency_key)`.
    pub replayed: bool,
}

const APPOINTMENT_SELECT: &str = "SELECT id,grantor_node_id,grantor_authority_epoch,target_ref,launch_project_id,session_id,request_digest,scope_version,policy_version,policy_request_json FROM manager_portfolio_appointments";

type AppointmentRow = (
    String,
    String,
    i64,
    String,
    String,
    String,
    String,
    Option<i64>,
    Option<i64>,
    Option<String>,
);

fn read_appointment_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AppointmentRow> {
    Ok((
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
    ))
}

fn appointment_from(raw: AppointmentRow) -> Result<(ChildAppointment, String)> {
    let (id, grantor, epoch, target, launch, session, stored_digest, scope, policy, request) = raw;
    Ok((
        ChildAppointment {
            id: parse_uuid(&id)?,
            grantor_node_id: parse_uuid(&grantor)?,
            grantor_authority_epoch: epoch,
            target: ChildTargetRef::parse(&target)?,
            launch_project_id: parse_uuid(&launch)?,
            session_id: parse_uuid(&session)?,
            appointed: scope.zip(policy),
            policy_request: request
                .map(|json| serde_json::from_str(&json))
                .transpose()?,
            replayed: true,
        },
        stored_digest,
    ))
}

/// #1314: a launched, unappointed appointment as its seat's launch sees it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LaunchedAppointment {
    pub grantor_node_id: Uuid,
    pub grantor_authority_epoch: i64,
    pub launch_project_id: Uuid,
    /// Already counted as a creation of its grantor (see `APPOINTMENT_CHARGED`).
    pub charged: bool,
}

/// The caller's own node, as resolved for a delegation.
struct DelegatingNode {
    record: GrantRecord,
    node: Uuid,
    epoch: i64,
}

impl Store {
    /// The caller's node: the active node whose current seat is exactly the
    /// caller, with every ancestor live. Anything else is
    /// `global_manager_not_seat`.
    fn delegating_node(&self, caller: Uuid) -> Result<DelegatingNode> {
        let not_seat = || refused(GLOBAL_MANAGER_NOT_SEAT);
        let record = seat_grant_on(&self.conn, caller)?.ok_or_else(not_seat)?;
        let node = record.node_id.ok_or_else(not_seat)?;
        let row = node_row_on(&self.conn, node)?
            .filter(|row| row.active)
            .ok_or_else(not_seat)?;
        node_depth_on(&self.conn, node)?;
        Ok(DelegatingNode {
            record,
            node,
            epoch: row.authority_epoch,
        })
    }

    /// `node`'s direct reports: its active child nodes, the live PMs of the
    /// projects it covers deepest, and the reports its in-flight
    /// appointments reserve (#1290), except the appointment `exclude`.
    pub(crate) fn portfolio_direct_reports(
        &self,
        node: Uuid,
        record: &GrantRecord,
        exclude: Option<Uuid>,
    ) -> Result<usize> {
        let mut reports = active_children_on(&self.conn, node)?.len();
        for project in &record.grant.project_ids {
            if covering_node_on(&self.conn, *project)? == Some(node)
                && self.global_live_manager(*project)?.is_some()
            {
                reports += 1;
            }
        }
        Ok(reports + self.reserved_reports(node, exclude)?)
    }

    /// #1290: launched-but-unappointed appointments that will add a report
    /// to `node`: a child node it is creating, or a PM for a vacant project
    /// it covers deepest. A reservation lapses when its launch failed (the
    /// session ended `Failed`, was archived, or never appeared within
    /// [`RESERVATION_LAUNCH_GRACE_SECONDS`]).
    fn reserved_reports(&self, node: Uuid, exclude: Option<Uuid>) -> Result<usize> {
        let rows: Vec<(String, String, String, String)> = {
            let mut statement = self.conn.prepare(
                "SELECT grantor_node_id,target_ref,session_id,reserved_at FROM manager_portfolio_appointments
                 WHERE state='launched' AND (?1 IS NULL OR id<>?1)",
            )?;
            statement
                .query_map([exclude.map(|id| id.to_string())], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })?
                .collect::<rusqlite::Result<_>>()?
        };
        let mut reserved = 0;
        for (grantor, target, session, reserved_at) in rows {
            let adds_report = match ChildTargetRef::parse(&target)? {
                ChildTargetRef::Portfolio(child) => {
                    grantor == node.to_string() && node_row_on(&self.conn, child)?.is_none()
                }
                ChildTargetRef::Project(project) => {
                    covering_node_on(&self.conn, project)? == Some(node)
                        && self.global_live_manager(project)?.is_none()
                }
            };
            if !adds_report {
                continue;
            }
            if self.reservation_live(&session, &reserved_at)? {
                reserved += 1;
            }
        }
        Ok(reserved)
    }

    /// Whether a launched appointment still holds its reservation: its
    /// session is not `Failed`, archived or deleted, or (no session yet) it
    /// was reserved or renewed within [`RESERVATION_LAUNCH_GRACE_SECONDS`].
    fn reservation_live(&self, session: &str, reserved_at: &str) -> Result<bool> {
        Ok(match self.get_session(parse_uuid(session)?)? {
            Some(row) => !matches!(
                row.status,
                SessionStatus::Failed | SessionStatus::Archived | SessionStatus::Deleted
            ),
            None => super::super::parse_timestamp(reserved_at).is_ok_and(|reserved| {
                (chrono::Utc::now() - reserved).num_seconds() < RESERVATION_LAUNCH_GRACE_SECONDS
            }),
        })
    }

    /// #1298: in-flight PM appointments (other than `exclude`) of projects
    /// that have no manager scope yet and are not `project`: each will take
    /// one of the `MAX_ACTIVE_MANAGERS` scope slots.
    fn reserved_new_scopes(&self, project: Uuid, exclude: Option<Uuid>) -> Result<usize> {
        let rows: Vec<(String, String, String)> = {
            let mut statement = self.conn.prepare(
                "SELECT target_ref,session_id,reserved_at FROM manager_portfolio_appointments a
                 WHERE state='launched' AND (?1 IS NULL OR id<>?1) AND target_ref LIKE 'project:%'
                   AND target_ref<>?2
                   AND NOT EXISTS(SELECT 1 FROM harness_manager_scopes s WHERE 'project:'||s.project_id=a.target_ref)",
            )?;
            statement
                .query_map(
                    params![
                        exclude.map(|id| id.to_string()),
                        ChildTargetRef::Project(project).as_column()
                    ],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )?
                .collect::<rusqlite::Result<_>>()?
        };
        let mut projects = std::collections::BTreeSet::new();
        for (target, session, reserved_at) in rows {
            if self.reservation_live(&session, &reserved_at)? {
                projects.insert(target);
            }
        }
        Ok(projects.len())
    }

    /// #1314: the launched, unappointed appointment whose reserved seat is
    /// `session_id`, or `manager_appointment_launch_changed`.
    pub(crate) fn launched_appointment_for_session(
        &self,
        session_id: Uuid,
    ) -> Result<LaunchedAppointment> {
        let sql = format!(
            "SELECT a.grantor_node_id,a.grantor_authority_epoch,a.launch_project_id,{APPOINTMENT_CHARGED}
             FROM manager_portfolio_appointments a WHERE a.session_id=?2 AND a.state='launched'"
        ); // sql-dynamic-ok: static APPOINTMENT_CHARGED clause
        let row: Option<(String, i64, String, bool)> = self
            .conn
            .query_row(
                &sql,
                params![grace_cutoff(), session_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let (node, epoch, project, charged) =
            row.ok_or_else(|| refused("manager_appointment_launch_changed"))?;
        Ok(LaunchedAppointment {
            grantor_node_id: parse_uuid(&node)?,
            grantor_authority_epoch: epoch,
            launch_project_id: parse_uuid(&project)?,
            charged,
        })
    }

    /// #1314, #1301: every charged appointment launching in `project` since
    /// `since`, as its `created_at` and its grantor node: a delegated seat
    /// is a session its grantor created, charged to it and its ancestors.
    pub(crate) fn appointment_creation_charges(
        &self,
        project: Uuid,
        since: &str,
    ) -> Result<Vec<(String, Uuid)>> {
        let sql = format!(
            "SELECT a.created_at,a.grantor_node_id FROM manager_portfolio_appointments a
             WHERE a.launch_project_id=?2 AND a.created_at>=?3 AND {APPOINTMENT_CHARGED}"
        ); // sql-dynamic-ok: static APPOINTMENT_CHARGED clause
        let mut statement = self.conn.prepare(&sql)?;
        let rows = statement
            .query_map(params![grace_cutoff(), project.to_string(), since], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|(created, node)| Ok((created, parse_uuid(&node)?)))
            .collect()
    }

    /// #1314: the resource gate of an appointment's launch, as the grantor's
    /// own `create_session` (see `Store::manager_v2_appointment_resource_gate`).
    /// A replay whose seat already exists launches nothing and is not gated.
    fn appointment_launch_admission(
        &self,
        caller: &DelegatingNode,
        launch: &rsi_common::harness_manager_v2::ManagerLaunchChoiceV2,
        project: Uuid,
        replayed: Option<&ChildAppointment>,
    ) -> Result<()> {
        let (existing, charged) = match replayed {
            None => (None, false),
            Some(appointment) => {
                if self.get_session(appointment.session_id)?.is_some() {
                    return Ok(());
                }
                let current = self.launched_appointment_for_session(appointment.session_id)?;
                (Some(appointment.session_id), current.charged)
            }
        };
        self.manager_v2_appointment_resource_gate(
            caller.node,
            caller.epoch,
            project,
            launch.provider,
            existing,
            charged,
        )
        .map(|_| ())
    }

    /// #1297, #1298: every deterministic check an appointment's completion
    /// makes, run before any launch, for a fresh request and for every
    /// replay alike: the target checks, and for a PM target the
    /// area-delegate refusal and the manager-scope limit.
    fn admission_preflight(
        &self,
        caller: &DelegatingNode,
        request: &AgentManagerAppointChildRequestV1,
        appointment_id: Uuid,
        session_id: Uuid,
        reserved: Option<ChildTargetRef>,
    ) -> Result<(ChildTargetRef, Uuid)> {
        let (target, launch) =
            self.authorize_child_target(caller, request, appointment_id, session_id, reserved)?;
        if let ChildTargetRef::Project(project) = target {
            self.global_appointment_delegates_free(project)?;
            if self.get_harness_manager_notice_config(project)?.is_none() {
                let exclude = reserved.map(|_| appointment_id);
                self.manager_scope_slots_free(self.reserved_new_scopes(project, exclude)?)?;
            }
        }
        Ok((target, launch))
    }

    fn refuse_over_cap(
        &self,
        node: Uuid,
        record: &GrantRecord,
        exclude: Option<Uuid>,
    ) -> Result<()> {
        if self.portfolio_direct_reports(node, record, exclude)?
            >= usize::from(record.max_direct_reports)
        {
            return Err(refused(MANAGER_DIRECT_REPORT_CAP));
        }
        Ok(())
    }

    /// #1287: the policy a node hands to a PM it appoints: its `child_policy`
    /// (which must narrow the node's own policy in every dimension) or, when
    /// unset, its own policy.
    ///
    /// #1412: the launch list is not copied. An empty V2 list means "inherit":
    /// the launch gate resolves it live against the ancestor chain's
    /// effective launches (`manager_ancestor_launch_gate`), so widening or
    /// narrowing a grant later reaches a PM already appointed. A
    /// `child_policy` with an explicit list keeps it (the operator's own
    /// narrowing), still within the node's effective launches.
    fn handed_pm_policy(record: &GrantRecord) -> Result<ManagerPolicyV2> {
        let grant = &record.grant;
        Ok(match &record.child_policy {
            Some(child) => {
                handed_policy_narrows(child, &grant.allowed_launches, &grant.project_policy)
                    .map_err(refused)?;
                child.clone()
            }
            None => {
                let mut policy = grant.project_policy.clone();
                policy.allowed_launches = Vec::new();
                policy
            }
        })
    }

    /// #1289: the checks the policy save makes against the target project,
    /// run before any launch: every group is a live root Group of the
    /// project, every paused Epic is a live Epic of the project (or already
    /// paused in its saved policy).
    fn pm_policy_fits_project(&self, project: Uuid, policy: &ManagerPolicyV2) -> Result<()> {
        let live = |status: SessionStatus| {
            !matches!(status, SessionStatus::Archived | SessionStatus::Deleted)
        };
        for group in &policy.group_ids {
            let fits = self.get_session(*group)?.is_some_and(|row| {
                row.project_id == Some(project)
                    && row.parent_id.is_none()
                    && row.session_kind == SessionKind::Group
                    && live(row.status)
            });
            if !fits {
                return Err(refused("manager_v2_group_out_of_scope"));
            }
        }
        let saved = self
            .get_harness_manager_policy(project)?
            .map(|saved| saved.policy.paused_epic_ids)
            .unwrap_or_default();
        let epics = self.global_project_epics(project)?;
        for epic in &policy.paused_epic_ids {
            if !epics.contains(epic) && !saved.contains(epic) {
                return Err(refused("manager_v2_epic_out_of_scope"));
            }
        }
        Ok(())
    }

    /// Whether `node` sits strictly below `ancestor` (through active
    /// grants above `start_parent`).
    fn portfolio_below(&self, start_parent: Option<Uuid>, ancestor: Uuid) -> Result<bool> {
        let mut cursor = start_parent;
        for _ in 0..PORTFOLIO_DEPTH_LIMIT {
            let Some(parent) = cursor else {
                return Ok(false);
            };
            if parent == ancestor {
                return Ok(true);
            }
            cursor = node_grant_on(&self.conn, parent)?.and_then(|record| record.parent_node_id);
        }
        Ok(false)
    }

    /// The newest grant of `child`, which must be a descendant `node`
    /// granted. An operator-granted descendant is
    /// `manager_child_operator_granted`; every other node (a sibling, an
    /// ancestor, another subtree, one granted by another node) is
    /// `manager_node_not_in_scope`.
    fn node_granted_descendant(&self, node: Uuid, child: Uuid) -> Result<GrantRecord> {
        let out_of_scope = || refused(MANAGER_NODE_NOT_IN_SCOPE);
        if child == node {
            return Err(out_of_scope());
        }
        let latest = latest_node_grant_on(&self.conn, child)?.ok_or_else(out_of_scope)?;
        if !self.portfolio_below(latest.parent_node_id, node)? {
            return Err(out_of_scope());
        }
        if latest.grantor == PortfolioGrantor::Operator.as_column() {
            return Err(refused(MANAGER_CHILD_OPERATOR_GRANTED));
        }
        if latest.grantor != PortfolioGrantor::Node(node).as_column() {
            return Err(out_of_scope());
        }
        Ok(latest)
    }

    /// The operator-shaped configure request of a new child node.
    fn child_configure_request(
        caller: &DelegatingNode,
        request: &AgentManagerAppointChildRequestV1,
        seat: Uuid,
        key: String,
    ) -> Result<ConfigurePortfolioNodeRequestV1> {
        let AppointChildTargetV1::Portfolio {
            node_id: None,
            tier_label: Some(tier_label),
            project_ids,
            allowed_launches,
            policy: Some(policy),
            child_policy,
            max_direct_reports,
            ..
        } = &request.target
        else {
            return Err(DaemonError::Store(
                "a child configure needs a new-child target".into(),
            ));
        };
        let grant = &caller.record.grant;
        Ok(ConfigurePortfolioNodeRequestV1 {
            node_id: None,
            parent_node_id: Some(caller.node),
            adopt_node_ids: Vec::new(),
            expected_parent_grant_version: None,
            tier_label: tier_label.clone(),
            seat_session_id: seat,
            project_ids: project_ids.clone(),
            allowed_launches: if allowed_launches.is_empty() {
                portfolio_effective_launches(&grant.allowed_launches, &grant.project_policy)
            } else {
                allowed_launches.clone()
            },
            policy: policy.clone(),
            child_policy: child_policy.clone(),
            max_direct_reports: max_direct_reports.unwrap_or_else(|| {
                PORTFOLIO_DEFAULT_MAX_DIRECT_REPORTS.min(caller.record.max_direct_reports)
            }),
            expected_node_grant_version: 0,
            expected_authority_epoch: 0,
            idempotency_key: key,
        })
    }

    /// Every check of an appointment, at admission and again at the
    /// appointment itself. Writes nothing. `reserved` is the target recorded
    /// at admission (the new child's reserved id).
    fn authorize_child_target(
        &self,
        caller: &DelegatingNode,
        request: &AgentManagerAppointChildRequestV1,
        appointment_id: Uuid,
        session_id: Uuid,
        reserved: Option<ChildTargetRef>,
    ) -> Result<(ChildTargetRef, Uuid)> {
        // At the appointment (and on a replay) the appointment's own
        // reservation is not counted against its cap.
        let exclude = reserved.map(|_| appointment_id);
        let grant = &caller.record.grant;
        Self::global_launch_allowed(grant, &request.launch)?;
        match &request.target {
            AppointChildTargetV1::Project { project_id } => {
                if !grant.project_ids.contains(project_id) {
                    return Err(refused(MANAGER_PROJECT_NOT_IN_SCOPE));
                }
                // A vacant PM slot adds a report to the node it sits under.
                if self.global_live_manager(*project_id)?.is_none() {
                    let parent = covering_node_on(&self.conn, *project_id)?.unwrap_or(caller.node);
                    let parent_record = node_grant_on(&self.conn, parent)?
                        .ok_or_else(|| refused(MANAGER_NODE_STALE))?;
                    self.refuse_over_cap(parent, &parent_record, exclude)?;
                }
                let policy = Self::handed_pm_policy(&caller.record)?;
                self.pm_policy_fits_project(*project_id, &policy)?;
                Ok((ChildTargetRef::Project(*project_id), *project_id))
            }
            AppointChildTargetV1::Portfolio {
                node_id: None,
                project_ids,
                launch_project_id,
                ..
            } => {
                if project_ids
                    .iter()
                    .any(|project| !grant.project_ids.contains(project))
                {
                    return Err(refused(MANAGER_PROJECT_NOT_IN_SCOPE));
                }
                if project_ids.len() >= grant.project_ids.len() {
                    return Err(refused(MANAGER_SCOPE_NOT_NARROWED));
                }
                let child = match reserved {
                    Some(ChildTargetRef::Portfolio(child)) => child,
                    _ => Uuid::new_v4(),
                };
                let depth = i64::from(node_depth_on(&self.conn, caller.node)?) + 1;
                for project in project_ids {
                    let holder: Option<String> = self
                        .conn
                        .query_row(
                            "SELECT node_id FROM manager_portfolio_coverage WHERE project_id=?1 AND depth=?2",
                            params![project.to_string(), depth],
                            |row| row.get(0),
                        )
                        .optional()?;
                    if holder.is_some_and(|holder| holder != child.to_string()) {
                        return Err(refused(MANAGER_SCOPE_OVERLAP));
                    }
                }
                self.refuse_over_cap(caller.node, &caller.record, exclude)?;
                let configure = Self::child_configure_request(
                    caller,
                    request,
                    session_id,
                    format!("appoint:{appointment_id}"),
                )?;
                self.portfolio_configure_preflight(
                    &configure,
                    PortfolioGrantor::Node(caller.node),
                )?;
                Ok((
                    ChildTargetRef::Portfolio(child),
                    launch_project_id.unwrap_or(project_ids[0]),
                ))
            }
            AppointChildTargetV1::Portfolio {
                node_id: Some(child),
                launch_project_id,
                expected_grant_version,
                ..
            } => {
                let record = self.node_granted_descendant(caller.node, *child)?;
                if record.parent_node_id != Some(caller.node) {
                    return Err(refused(MANAGER_NODE_NOT_IN_SCOPE));
                }
                let active = node_grant_on(&self.conn, *child)?
                    .filter(|active| active.grant.grant_version == record.grant.grant_version)
                    .ok_or_else(|| refused(MANAGER_NODE_STALE))?;
                if expected_grant_version
                    .is_some_and(|version| version != active.grant.grant_version)
                {
                    return Err(refused(MANAGER_NODE_STALE));
                }
                let launch = launch_project_id.unwrap_or(active.grant.project_ids[0]);
                if !active.grant.project_ids.contains(&launch) {
                    return Err(refused(MANAGER_PROJECT_NOT_IN_SCOPE));
                }
                Ok((ChildTargetRef::Portfolio(*child), launch))
            }
        }
    }

    fn child_appointment_by_key(
        &self,
        node: Uuid,
        key: &str,
    ) -> Result<Option<(ChildAppointment, String)>> {
        let sql = format!("{APPOINTMENT_SELECT} WHERE grantor_node_id=?1 AND idempotency_key=?2"); // sql-dynamic-ok: static literals
        self.conn
            .query_row(&sql, params![node.to_string(), key], read_appointment_row)
            .optional()?
            .map(appointment_from)
            .transpose()
    }

    fn child_appointment_by_id(&self, id: Uuid) -> Result<ChildAppointment> {
        let sql = format!("{APPOINTMENT_SELECT} WHERE id=?1"); // sql-dynamic-ok: static literals
        let raw = self
            .conn
            .query_row(&sql, [id.to_string()], read_appointment_row)?;
        Ok(appointment_from(raw)?.0)
    }

    /// Admission of `AgentManagerAppointChild`: in one IMMEDIATE transaction
    /// resolve the caller's node, return the recorded appointment on a replay
    /// (`portfolio_idempotency_conflict` for a different request under the
    /// key), else run every check and record the appointment with a reserved
    /// session id. A refusal records nothing.
    ///
    /// # Errors
    /// `global_manager_not_seat`, `manager_project_not_in_scope`,
    /// `global_launch_not_allowed`, `manager_scope_not_narrowed`,
    /// `manager_scope_overlap`, `manager_direct_report_cap`, the narrowing
    /// refusals, `manager_child_operator_granted`, `manager_node_not_in_scope`,
    /// `manager_node_stale`, the area-delegates refusal, a manager resource
    /// refusal (`manager_v2_policy_paused`, `manager_v2_concurrency_capacity`,
    /// `manager_v2_provider_capacity`, `manager_v2_spend_*`,
    /// `manager_v2_creation_limit`, the ancestor allowance), an idempotency
    /// conflict or a persistence error.
    pub fn begin_child_appointment(
        &self,
        caller: Uuid,
        request: &AgentManagerAppointChildRequestV1,
    ) -> Result<ChildAppointment> {
        request.validate().map_err(refused)?;
        let request_digest = digest(request)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let delegating = self.delegating_node(caller)?;
        if let Some((existing, stored)) =
            self.child_appointment_by_key(delegating.node, &request.idempotency_key)?
        {
            if stored != request_digest {
                return Err(refused(PORTFOLIO_IDEMPOTENCY_CONFLICT));
            }
            // #1288: an unfinished replay may launch, so it is authorized
            // again first, under the epoch it was admitted in.
            if existing.appointed.is_none() {
                if existing.grantor_authority_epoch != delegating.epoch {
                    return Err(refused(GLOBAL_MANAGER_NOT_SEAT));
                }
                self.admission_preflight(
                    &delegating,
                    request,
                    existing.id,
                    existing.session_id,
                    Some(existing.target),
                )?;
                self.appointment_launch_admission(
                    &delegating,
                    &request.launch,
                    existing.launch_project_id,
                    Some(&existing),
                )?;
                // #1299: the replay holds its report again from now, under
                // the same lock that rechecked the cap.
                let now = stamp();
                self.conn.execute(
                    "UPDATE manager_portfolio_appointments SET reserved_at=?2,updated_at=?2
                     WHERE id=?1 AND state='launched'",
                    params![existing.id.to_string(), now],
                )?;
            }
            tx.commit()?;
            return Ok(existing);
        }
        let id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        let (target, launch_project_id) =
            self.admission_preflight(&delegating, request, id, session_id, None)?;
        self.appointment_launch_admission(&delegating, &request.launch, launch_project_id, None)?;
        let now = stamp();
        self.conn.execute(
            "INSERT INTO manager_portfolio_appointments(id,grantor_node_id,grantor_authority_epoch,target_ref,launch_project_id,caller_session_id,idempotency_key,request_digest,session_id,state,created_at,updated_at,reserved_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,'launched',?10,?10,?10)",
            params![
                id.to_string(),
                delegating.node.to_string(),
                delegating.epoch,
                target.as_column(),
                launch_project_id.to_string(),
                caller.to_string(),
                request.idempotency_key,
                request_digest,
                session_id.to_string(),
                now,
            ],
        )?;
        tx.commit()?;
        Ok(ChildAppointment {
            id,
            grantor_node_id: delegating.node,
            grantor_authority_epoch: delegating.epoch,
            target,
            launch_project_id,
            session_id,
            appointed: None,
            policy_request: None,
            replayed: false,
        })
    }

    /// The caller must still be the seat of the appointment's grantor under
    /// the same authority epoch (a context-cap successor qualifies; an
    /// operator re-grant does not).
    fn appointment_grantor(
        &self,
        caller: Uuid,
        appointment: &ChildAppointment,
    ) -> Result<DelegatingNode> {
        let delegating = self.delegating_node(caller)?;
        if delegating.node != appointment.grantor_node_id
            || delegating.epoch != appointment.grantor_authority_epoch
        {
            return Err(refused(GLOBAL_MANAGER_NOT_SEAT));
        }
        Ok(delegating)
    }

    fn complete_child_appointment(&self, id: Uuid, versions: (i64, i64)) -> Result<()> {
        self.conn.execute(
            "UPDATE manager_portfolio_appointments SET state='appointed',scope_version=?2,policy_version=?3,updated_at=?4
             WHERE id=?1 AND state='launched'",
            params![id.to_string(), versions.0, versions.1, stamp()],
        )?;
        Ok(())
    }

    /// Appoint a launched child node seat (a new child, or a replacement
    /// seat) in one IMMEDIATE transaction: the checks run again, the grant is
    /// written and the appointment completes together. Returns the child's
    /// `(authority epoch, grant version)`.
    ///
    /// # Errors
    /// `global_manager_not_seat` (the grantor's seat or epoch changed), the
    /// admission refusals, `global_manager_seat_unavailable` or a persistence
    /// error.
    pub fn finish_portfolio_appointment(
        &self,
        caller: Uuid,
        request: &AgentManagerAppointChildRequestV1,
        appointment: &ChildAppointment,
    ) -> Result<(i64, i64)> {
        let ChildTargetRef::Portfolio(child) = appointment.target else {
            return Err(DaemonError::Store(
                "a portfolio appointment names a project".into(),
            ));
        };
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let delegating = self.appointment_grantor(caller, appointment)?;
        if let Some(done) = self.child_appointment_by_id(appointment.id)?.appointed {
            tx.commit()?;
            return Ok(done);
        }
        let key = format!("appoint:{}", appointment.id);
        if grant_by_key_on(&self.conn, &key)?.is_none() {
            self.authorize_child_target(
                &delegating,
                request,
                appointment.id,
                appointment.session_id,
                Some(appointment.target),
            )?;
            let create = matches!(
                request.target,
                AppointChildTargetV1::Portfolio { node_id: None, .. }
            );
            if create {
                let configure = Self::child_configure_request(
                    &delegating,
                    request,
                    appointment.session_id,
                    key,
                )?;
                self.configure_portfolio_node_as_in_tx(
                    &configure,
                    PortfolioGrantor::Node(delegating.node),
                    &format!("agent:{caller}"),
                    Some(child),
                )?;
            } else {
                self.replace_portfolio_seat_in_tx(child, appointment.session_id, &key)?;
            }
        }
        let epoch = node_row_on(&self.conn, child)?
            .map(|row| row.authority_epoch)
            .ok_or_else(|| DaemonError::Store("an appointed child node vanished".into()))?;
        let version = node_grant_on(&self.conn, child)?
            .map(|record| record.grant.grant_version)
            .ok_or_else(|| refused(MANAGER_NODE_STALE))?;
        self.complete_child_appointment(appointment.id, (epoch, version))?;
        tx.commit()?;
        Ok((epoch, version))
    }

    /// Appoint a launched PM seat (the v0 `AgentGlobalAppointManager` steps,
    /// generalized to any node) in one IMMEDIATE transaction (#1289): the
    /// checks run again (cap, coverage, launch, the handed policy and its fit
    /// to the project), then whole-project scope displacing the current PM,
    /// then the handed policy saved under the new scope version, then the
    /// completion. A refusal anywhere leaves the previous PM and policy in
    /// place. Returns the project's `(scope_version, policy_version)`.
    ///
    /// # Errors
    /// `global_manager_not_seat` (the grantor's seat or epoch changed), the
    /// admission refusals, a manager-store refusal or a persistence error.
    pub fn finish_project_appointment(
        &self,
        caller: Uuid,
        request: &AgentManagerAppointChildRequestV1,
        appointment: &ChildAppointment,
    ) -> Result<(i64, i64)> {
        let ChildTargetRef::Project(project) = appointment.target else {
            return Err(DaemonError::Store(
                "a project appointment names a portfolio node".into(),
            ));
        };
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let delegating = self.appointment_grantor(caller, appointment)?;
        if let Some(done) = self.child_appointment_by_id(appointment.id)?.appointed {
            tx.commit()?;
            return Ok(done);
        }
        self.authorize_child_target(
            &delegating,
            request,
            appointment.id,
            appointment.session_id,
            Some(appointment.target),
        )?;
        let expected_row_version = self
            .get_harness_manager(project)?
            .map_or(0, |config| config.row_version);
        let (config, _) = self.configure_harness_manager_in_tx(
            &tx,
            &ConfigureHarnessManagerRequestV1 {
                project_id: project,
                session_id: appointment.session_id,
                epic_ids: None,
                group_ids: vec![],
                expected_row_version,
            },
        )?;
        let expected_policy_version = self
            .get_harness_manager_policy(project)?
            .map_or(0, |policy| policy.row_version);
        let policy_request = ConfigureHarnessManagerPolicyRequestV2 {
            project_id: project,
            expected_scope_version: config.row_version,
            expected_policy_version,
            idempotency_key: format!("node-appoint-{}", appointment.id), // sql-dynamic-ok: a key, not SQL
            policy: Self::handed_pm_policy(&delegating.record)?,
        };
        self.conn.execute(
            "UPDATE manager_portfolio_appointments SET policy_request_json=?2,updated_at=?3
             WHERE id=?1 AND state='launched' AND policy_request_json IS NULL",
            params![
                appointment.id.to_string(),
                serde_json::to_string(&policy_request)?,
                stamp()
            ],
        )?;
        let policy = self.configure_harness_manager_policy_in_tx(&tx, &policy_request)?;
        let versions = (policy.scope_version, policy.row_version);
        self.complete_child_appointment(appointment.id, versions)?;
        tx.commit()?;
        Ok(versions)
    }

    /// `AgentManagerRevokeChild`: revoke a child node the caller's node
    /// granted (grantor-scoped, plan §3): nodes whose authority came from it
    /// die with it, operator-granted descendants re-parent. A replay against
    /// the revoked child at the same grant version returns it with
    /// `deduplicated`.
    ///
    /// # Errors
    /// `global_manager_not_seat`, `manager_child_operator_granted`,
    /// `manager_node_not_in_scope`, `manager_node_stale`, or a persistence
    /// error.
    pub fn revoke_child_portfolio_node(
        &self,
        caller: Uuid,
        request: &AgentManagerRevokeChildRequestV1,
    ) -> Result<(PortfolioNodeV1, PortfolioRevokeOutcome, bool)> {
        request.validate().map_err(refused)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let delegating = self.delegating_node(caller)?;
        self.node_granted_descendant(delegating.node, request.node_id)?;
        let row = node_row_on(&self.conn, request.node_id)?
            .ok_or_else(|| refused(MANAGER_NODE_NOT_IN_SCOPE))?;
        let (view, outcome) = self.revoke_portfolio_node_in_tx(&RevokePortfolioNodeRequestV1 {
            node_id: request.node_id,
            expected_grant_version: request.expected_grant_version,
            expected_authority_epoch: row.authority_epoch,
            idempotency_key: request.idempotency_key.clone(),
        })?;
        tx.commit()?;
        Ok((view, outcome, !row.active))
    }

    /// #1239 session control over child seats (plan §2.4): a portfolio seat
    /// reads, watches and mails the seat of any descendant node and the PM
    /// seat of any covered project; it halts or continues only a direct
    /// child seat (a child node's seat, including its retired seats, or the
    /// PM of a project it covers deepest). `Ok(false)` outside that reach. A
    /// mutation also needs the node's own `SessionControl` in Execute mode,
    /// unpaused, and no pending human gate on the target.
    ///
    /// # Errors
    /// `manager_v2_capability_denied`, `manager_v2_paused`,
    /// `manager_v2_human_or_recovery_owner` or a persistence error.
    pub fn portfolio_seat_reach(&self, caller: Uuid, target: Uuid, mutation: bool) -> Result<bool> {
        if caller == target {
            return Ok(false);
        }
        let Ok(delegating) = self.delegating_node(caller) else {
            return Ok(false);
        };
        let node = delegating.node;
        let in_reach = match self.portfolio_seat_node(target)? {
            Some(target_node) if target_node == node => false,
            Some(target_node) => {
                let parent = node_grant_on(&self.conn, target_node)?.and_then(|r| r.parent_node_id);
                if mutation {
                    parent == Some(node)
                } else {
                    self.portfolio_below(parent, node)?
                }
            }
            None => {
                let project = self.get_session(target)?.and_then(|row| row.project_id);
                match project {
                    Some(project)
                        if delegating.record.grant.project_ids.contains(&project)
                            && self.global_live_manager(project)? == Some(target) =>
                    {
                        !mutation || covering_node_on(&self.conn, project)? == Some(node)
                    }
                    _ => false,
                }
            }
        };
        if !in_reach || !mutation {
            return Ok(in_reach);
        }
        let policy = &delegating.record.grant.project_policy;
        if !policy
            .capabilities
            .contains(&ManagerCapabilityV2::SessionControl)
        {
            return Err(refused("manager_v2_capability_denied"));
        }
        if policy.mode != ManagerOperatingModeV2::Execute || policy.paused {
            return Err(refused("manager_v2_paused"));
        }
        self.manager_session_control_human_gate(target, false)?;
        Ok(true)
    }

    /// Who granted the PM seat whose appointed session is `manager`:
    /// `node:<id>` when a portfolio node appointed it (S5, or a v0 global
    /// appointment), else `operator`.
    pub(crate) fn project_manager_grantor(&self, project: Uuid, manager: Uuid) -> Result<String> {
        let node: Option<String> = self
            .conn
            .query_row(
                "SELECT grantor_node_id FROM manager_portfolio_appointments
                   WHERE target_ref=?1 AND session_id=?2 AND state='appointed'
                 UNION ALL
                 SELECT g.node_id FROM global_manager_appointments a
                   JOIN global_manager_grants g ON g.id=a.grant_id
                   WHERE a.project_id=?3 AND a.session_id=?2 AND a.state='appointed' AND g.node_id IS NOT NULL
                 LIMIT 1",
                params![
                    ChildTargetRef::Project(project).as_column(),
                    manager.to_string(),
                    project.to_string()
                ],
                |row| row.get(0),
            )
            .optional()?;
        Ok(node.map_or_else(
            || PortfolioGrantor::Operator.as_column(),
            |node| format!("node:{node}"),
        ))
    }
}
