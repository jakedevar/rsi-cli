//! Explicit operator grants and typed, durable manager coordination.
//!
//! V1 appointments alone grant none of these mutation capabilities. Agent
//! requests name scoped targets and observed versions, never their own identity.
#![allow(clippy::missing_errors_doc)]

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::manager_daemon_settings::{ManagerDaemonSettingBoundV2, validate_daemon_setting_bounds};
use crate::manager_operator_delegation::{
    OperatorCallFenceV1, OperatorCallResultV1, OperatorCallV1,
};
use crate::model_control::ModelTier;
use crate::types::{SessionKind, SessionProvider};

pub const MANAGER_V2_MAX_PAGE: u16 = 64;
pub const MANAGER_V2_MAX_RECORDS: usize = 1024;
pub const MANAGER_V2_MAX_PENDING_ACTIONS: usize = 128;
pub const MANAGER_V2_MAX_GROUPS: usize = 32;
pub const MANAGER_V2_MAX_WORK: usize = 256;
pub const MANAGER_REVIEW_MAX_FINDINGS: usize = 64;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagerWorkRiskTierV2 {
    #[default]
    Tier1,
    Tier2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagerCapabilityV2 {
    WorkPlan,
    LeadControl,
    Topology,
    SessionCreate,
    LeadAssign,
    Integration,
    SelfSuccession,
    /// Git mutation effect: advance an integration target ref. Distinct from
    /// `Integration` which records evidence only. Required by
    /// `ManagerActionV2::Integrate`.
    GitEffect,
    /// Halt, continue and mail sessions (Epic leads and their descendants)
    /// inside the manager's live scope through the generic agent control
    /// verbs. Read-only status/progress/watch reach needs scope only.
    SessionControl,
    /// Read and CAS-coordinate project Issues through the guarded agent Issue
    /// controls, alongside the unchanged owning-Epic-lead path.
    IssueCoordinate,
    /// K14 (#672): invoke the closed, versioned operator-method allowlist
    /// (`DELEGABLE_OPERATOR_METHODS_V1`) through the audited `operator_call`
    /// manager action, bound to the manager's project.
    OperatorDelegation,
    /// Author, execute, inspect, interrupt and resolve durable topologies on
    /// in-scope Epics through the `AgentTopology*` verbs (#633). Distinct
    /// from `Topology`, which creates containers. A land step additionally
    /// needs `GitEffect`.
    Automation,
    /// #1043: read daemon sandbox storage status and run the bounded build-cache
    /// reclaim (preview or real pass) through the audited `operator_call`
    /// manager action, under the configured watermarks and limits. Off by
    /// default; granted by the Full project control preset (not Execute);
    /// daemon storage settings stay operator-only.
    StorageControl,
    /// #1046: change the operator-allowlisted daemon settings (sandbox roots,
    /// minimum free space, reclaim watermarks) through the audited
    /// `operator_call` manager action, inside the per-key bounds the operator
    /// sets in `ManagerPolicyV2::daemon_setting_bounds`. Off by default;
    /// granted by the Full project control preset (not Execute), and inert
    /// until the operator bounds a key; spend, credentials, appointment and
    /// scope stay operator-only.
    DaemonSettings,
    /// #1045: ask the daemon to stage verified binaries and restart itself at a
    /// quiet point through `AgentRequestDeploy`. Off by default; granted by
    /// the Full project control preset (not Execute); needs Execute mode and
    /// no pause at call time.
    Deploy,
}

/// Upper bound on distinct grants in one policy; well above the variant count
/// so additive capabilities do not require a validation change.
pub const MANAGER_V2_MAX_CAPABILITIES: usize = 16;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagerOperatingModeV2 {
    #[default]
    Status,
    Monitor,
    Execute,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagerLaunchChoiceV2 {
    pub provider: SessionProvider,
    pub model: String,
    #[serde(default)]
    pub effort: Option<String>,
}

/// #1195: refusal for a malformed `sandbox_source` (a commit that is not 40
/// lowercase hex, a relative or oversized path).
pub const MANAGER_SANDBOX_SOURCE_INVALID: &str = "manager_sandbox_source_invalid";
/// #1195: a `path` source that is not a registered worktree of the project
/// repository (after canonicalization).
pub const MANAGER_SANDBOX_SOURCE_NOT_WORKTREE: &str = "manager_sandbox_source_not_worktree";
/// #1195: a `commit` source that is not a commit in the project repository.
pub const MANAGER_SANDBOX_SOURCE_COMMIT_UNKNOWN: &str = "manager_sandbox_source_commit_unknown";

/// #1195: where a manager-created worker's sandbox branches from.
///
/// Externally tagged: `{"rolling": {}}`, `{"commit": "<40-hex>"}` or
/// `{"path": "<absolute worktree path>"}`. Omitted means `rolling`: the
/// project's freshly fetched `origin/rolling` tip, observed at launch.
/// `commit` and `path` are resolved once, when the action is admitted or
/// prepared, and the pinned commit is what the sandbox is allocated from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ManagerSandboxSourceV1 {
    Rolling {},
    Commit(String),
    Path(String),
}

impl ManagerSandboxSourceV1 {
    /// Shape check only; the daemon resolves the commit or worktree.
    ///
    /// # Errors
    /// [`MANAGER_SANDBOX_SOURCE_INVALID`].
    pub fn validate(&self) -> Result<(), &'static str> {
        let valid = match self {
            Self::Rolling {} => true,
            Self::Commit(commit) => {
                commit.len() == 40
                    && commit
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            }
            Self::Path(path) => {
                path.len() <= 4096
                    && !path.contains('\0')
                    && std::path::Path::new(path).is_absolute()
            }
        };
        if valid {
            Ok(())
        } else {
            Err(MANAGER_SANDBOX_SOURCE_INVALID)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagerSandboxSourceKindV1 {
    Rolling,
    Commit,
    Path,
}

/// #1195: the sandbox source a `create_session` receipt was admitted with.
///
/// `commit` is the pinned commit for `commit` and `path`; a `rolling` source
/// is observed at launch and shows as the worker's `base_commit` in progress.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagerSandboxSourceReceiptV1 {
    pub kind: ManagerSandboxSourceKindV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// The named worktree had uncommitted changes; only its committed `HEAD`
    /// is used.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub source_dirty: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagerProviderLimitV2 {
    pub provider: SessionProvider,
    pub max_active: u16,
}

/// All defaults preserve status-only v1 behavior. Grants are operator-owned;
/// sessions and containers created under a grant consume persistent quotas.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ManagerPolicyV2 {
    pub mode: ManagerOperatingModeV2,
    pub paused: bool,
    pub paused_epic_ids: Vec<Uuid>,
    pub capabilities: Vec<ManagerCapabilityV2>,
    pub group_ids: Vec<Uuid>,
    pub allow_create_groups: bool,
    pub max_created_containers: u16,
    pub max_created_sessions: u16,
    pub max_active_sessions: u16,
    pub provider_limits: Vec<ManagerProviderLimitV2>,
    pub allowed_launches: Vec<ManagerLaunchChoiceV2>,
    pub max_recovery_attempts: u16,
    pub retry_delay_seconds: u32,
    pub request_timeout_seconds: u32,
    pub max_spend_usd: Option<f64>,
    /// Minimum reviewer model tier for explicitly classified Tier-2 work.
    pub minimum_tier2_reviewer_tier: ModelTier,
    /// #1046: operator-set `min`/`max` per allowlisted daemon setting. A key
    /// without an entry is not manager-adjustable (default: none).
    pub daemon_setting_bounds: Vec<ManagerDaemonSettingBoundV2>,
}

impl Default for ManagerPolicyV2 {
    fn default() -> Self {
        Self {
            mode: ManagerOperatingModeV2::Status,
            paused: false,
            paused_epic_ids: vec![],
            capabilities: vec![],
            group_ids: vec![],
            allow_create_groups: false,
            max_created_containers: 0,
            max_created_sessions: 0,
            max_active_sessions: 4,
            provider_limits: vec![],
            allowed_launches: vec![],
            max_recovery_attempts: 0,
            retry_delay_seconds: 60,
            request_timeout_seconds: 900,
            max_spend_usd: None,
            minimum_tier2_reviewer_tier: ModelTier::Standard,
            daemon_setting_bounds: vec![],
        }
    }
}

impl ManagerPolicyV2 {
    pub fn validate(&self) -> Result<(), &'static str> {
        unique_ids(&self.group_ids, MANAGER_V2_MAX_GROUPS)?;
        unique_ids(&self.paused_epic_ids, 32)?;
        if self.capabilities.len() > MANAGER_V2_MAX_CAPABILITIES
            || self
                .capabilities
                .iter()
                .enumerate()
                .any(|(i, c)| self.capabilities[..i].contains(c))
            || self.max_created_containers > 64
            || self.max_created_sessions > 1024
            || !(1..=100).contains(&self.max_active_sessions)
            || self.provider_limits.len() > 8
            || self.allowed_launches.len() > crate::global_manager::GLOBAL_MANAGER_MAX_LAUNCHES
            || self.max_recovery_attempts > 32
            || !(1..=86400).contains(&self.retry_delay_seconds)
            || !(30..=604800).contains(&self.request_timeout_seconds)
            || self
                .max_spend_usd
                .is_some_and(|n| !n.is_finite() || n <= 0.0)
            || (self.allow_create_groups
                && !self.capabilities.contains(&ManagerCapabilityV2::Topology))
        {
            return Err("manager_v2_invalid_policy");
        }
        for (i, limit) in self.provider_limits.iter().enumerate() {
            if !(1..=64).contains(&limit.max_active)
                || self.provider_limits[..i]
                    .iter()
                    .any(|old| old.provider == limit.provider)
            {
                return Err("manager_v2_invalid_provider_limit");
            }
        }
        for choice in &self.allowed_launches {
            choice.validate()?;
        }
        validate_daemon_setting_bounds(&self.daemon_setting_bounds)?;
        Ok(())
    }
}

impl ManagerLaunchChoiceV2 {
    /// An allowlist entry with omitted effort permits every effort the model
    /// supports. When RSI knows the model's effort ladder
    /// ([`crate::model_utils::known_effort_ladder`]) an effort off that ladder
    /// is not admitted, so an any-effort grant never admits a made-up effort
    /// string; an unknown model keeps admitting every effort (the provider CLI
    /// validates it). An exact entry admits exactly its own effort and cannot
    /// grant a child entry that permits every effort.
    #[must_use]
    pub fn allows(&self, launch: &Self) -> bool {
        if self.provider != launch.provider || self.model != launch.model {
            return false;
        }
        match (self.effort.as_deref(), launch.effort.as_deref()) {
            (Some(granted), Some(requested)) => granted == requested,
            (Some(_), None) => false,
            (None, None) => true,
            (None, Some(requested)) => crate::model_utils::known_effort_ladder(&self.model)
                .is_none_or(|ladder| ladder.contains(&requested)),
        }
    }

    #[must_use]
    pub fn effort_label(&self) -> &str {
        self.effort.as_deref().unwrap_or("any effort")
    }

    #[must_use]
    pub fn display(&self) -> String {
        format!("{:?}/{}/{}", self.provider, self.model, self.effort_label())
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        text(&self.model, 256)?;
        if let Some(effort) = &self.effort {
            text(effort, 32)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigureHarnessManagerPolicyRequestV2 {
    pub project_id: Uuid,
    pub expected_scope_version: i64,
    pub expected_policy_version: i64,
    pub idempotency_key: String,
    pub policy: ManagerPolicyV2,
}

/// #2237 (operator-only): appoint a manager and grant its policy in one
/// atomic step. The scope save and the policy save share one transaction, so a
/// refused grant never leaves an appointment without its policy (the two-step
/// `ConfigureHarnessManager` then `ConfigureHarnessManagerPolicy` flow can).
/// `policy` replaces the whole draft; the scope fence is `scope.expected_row_version`
/// and the policy fence is `expected_policy_version` (0 when none is saved).
/// A revocation (`scope` clearing the appointment) is not a valid request here.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigureHarnessManagerWithPolicyRequestV2 {
    pub scope: crate::harness_manager::ConfigureHarnessManagerRequestV1,
    pub expected_policy_version: i64,
    pub idempotency_key: String,
    pub policy: ManagerPolicyV2,
}

impl ConfigureHarnessManagerWithPolicyRequestV2 {
    /// # Errors
    /// A stable refusal code.
    pub fn validate(&self) -> Result<(), &'static str> {
        self.scope.validate()?;
        if self.scope.is_revocation()
            || self.expected_policy_version < 0
            || self.idempotency_key.trim().is_empty()
            || self.idempotency_key.len() > 128
        {
            return Err("manager_v2_invalid_policy_request");
        }
        self.policy.validate()
    }

    /// The policy half as the two-step flow would send it, fenced to the scope
    /// version the appointment produced.
    #[must_use]
    pub fn policy_request(&self, scope_version: i64) -> ConfigureHarnessManagerPolicyRequestV2 {
        ConfigureHarnessManagerPolicyRequestV2 {
            project_id: self.scope.project_id,
            expected_scope_version: scope_version,
            expected_policy_version: self.expected_policy_version,
            idempotency_key: self.idempotency_key.clone(),
            policy: self.policy.clone(),
        }
    }
}

/// `ConfigureHarnessManagerWithPolicy` result: the appointment and the grant it
/// carries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigureHarnessManagerWithPolicyResultV2 {
    pub config: crate::harness_manager::HarnessManagerConfigV1,
    pub scope_outcome: crate::harness_manager::HarnessManagerPolicyOutcomeV1,
    pub policy: HarnessManagerPolicyConfigV2,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessManagerPolicyConfigV2 {
    pub project_id: Uuid,
    pub manager_session_id: Uuid,
    pub scope_version: i64,
    pub row_version: i64,
    pub policy: ManagerPolicyV2,
    pub updated_at: DateTime<Utc>,
    /// A scope edit or new appointment invalidates an older grant.
    pub revoked: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagerFenceV2 {
    pub scope_version: i64,
    pub policy_version: i64,
}

impl ManagerFenceV2 {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.scope_version <= 0 || self.policy_version <= 0 {
            return Err("manager_v2_invalid_fence");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagerLeadFenceV2 {
    pub lead_session_id: Option<Uuid>,
    pub lead_generation: i64,
    pub event_sequence: i64,
    pub custody_generation: Option<i64>,
}

impl ManagerLeadFenceV2 {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.lead_session_id.is_some_and(|id| id.is_nil())
            || self.lead_generation < 0
            || self.event_sequence < 0
            || self
                .custody_generation
                .is_some_and(|generation| generation <= 0)
        {
            return Err("manager_v2_invalid_lead_fence");
        }
        Ok(())
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagerInspectSectionV2 {
    #[default]
    Overview,
    Workers,
    Work,
    Requests,
    Decisions,
    Topology,
    Resources,
    Actions,
    Events,
    Archive,
    /// One bounded fleet-health row per scoped Epic (#627).
    Health,
    /// Durable SQLite migration-version allocations and competing claims.
    MigrationAllocations,
    /// One read-only health row per registered satellite peer (#1017).
    Satellites,
    /// The project's friction rollup, one row per signature (#1333).
    Friction,
    /// Pending non-gate decision records the caller may settle with a
    /// `decision_ruling` update: its own ledger's and every ledger below it
    /// (a portfolio seat sees the project manager's), one page (#1415).
    Rulings,
}

const fn page_limit() -> u16 {
    32
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentManagerInspectRequestV2 {
    /// Workers only: retain all historical rows in creation order. The default
    /// actionable view is newest first; recent terminal results have a 7-day
    /// attention window, while explicit pending work/jobs/landings stay visible.
    #[serde(default)]
    pub include_history: bool,
    #[serde(default)]
    pub section: ManagerInspectSectionV2,
    #[serde(default)]
    pub epic_id: Option<Uuid>,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default = "page_limit")]
    pub limit: u16,
    /// #1235: the target project of a global manager seat acting inside its
    /// operator grant. Omitted means the caller's own project. A target the
    /// daemon checks against the grant, never caller identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
}

impl Default for AgentManagerInspectRequestV2 {
    fn default() -> Self {
        Self {
            project_id: None,
            include_history: false,
            section: ManagerInspectSectionV2::Overview,
            epic_id: None,
            cursor: None,
            limit: page_limit(),
        }
    }
}

impl AgentManagerInspectRequestV2 {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !(1..=MANAGER_V2_MAX_PAGE).contains(&self.limit)
            || self.epic_id.is_some_and(|id| id.is_nil())
            || self
                .cursor
                .as_ref()
                .is_some_and(|c| c.len() > 512 || c.contains('\0'))
        {
            return Err("manager_v2_invalid_page");
        }
        Ok(())
    }
}

/// Maximum unfinished Work versions returned on an Overview intent row.
pub const MANAGER_INTENT_WORK_LIMIT: usize = 16;

/// Disjoint states in the Overview intent's `work_summary_v1`. Counts cover
/// the same retained window as section `work`; retired history is settled.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagerIntentWorkCountsV1 {
    pub pending: usize,
    pub partial: usize,
    pub accepted: usize,
    pub integrated: usize,
    pub retired: usize,
}

/// Section rows are tagged typed records/projections; absence is not a passed
/// gate. `complete` describes traversal completeness, never feature completion.
#[derive(Debug, Clone, Deserialize)]
pub struct ManagerInspectionV2 {
    pub observed_at: DateTime<Utc>,
    pub scope_version: i64,
    pub policy: Option<HarnessManagerPolicyConfigV2>,
    pub section: ManagerInspectSectionV2,
    pub rows: Vec<serde_json::Value>,
    pub next_cursor: Option<String>,
    pub complete: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetHarnessManagerStateRequestV2 {
    pub project_id: Uuid,
    pub query: AgentManagerInspectRequestV2,
}

/// Observed root authority, never caller-supplied session identity. Ordinary
/// conversation events do not change this fence; custody and authority do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagerSuccessionFenceV2 {
    pub authority_epoch: i64,
    #[serde(default)]
    pub custody_generation: Option<i64>,
}

impl ManagerSuccessionFenceV2 {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.authority_epoch <= 0 || self.custody_generation.is_some_and(|n| n <= 0) {
            return Err("manager_v2_invalid_succession_fence");
        }
        Ok(())
    }
}

/// Committed content selection. The daemon must also authenticate source
/// custody and verify object types and exact tree membership before admission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagerCommittedHandoffV2 {
    pub source_commit: String,
    pub relative_path: String,
    pub blob_oid: String,
}

impl ManagerCommittedHandoffV2 {
    pub fn validate(&self) -> Result<(), &'static str> {
        let full_oid = |oid: &str| {
            matches!(oid.len(), 40 | 64)
                && oid
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        };
        if !full_oid(&self.source_commit)
            || !full_oid(&self.blob_oid)
            || self.relative_path.len() > 1024
            || self.relative_path.contains(['\0', '\\'])
            || self
                .relative_path
                .split('/')
                .any(|part| matches!(part, "" | "." | ".." | ".git"))
        {
            return Err("manager_v2_invalid_committed_handoff");
        }
        Ok(())
    }
}

/// Read-only observation in a manager state/inspection `manager_control` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagerSuccessionObservationV2 {
    pub logical_manager_session_id: Uuid,
    pub current_session_id: Uuid,
    pub expected: ManagerSuccessionFenceV2,
    pub unresolved_operation_id: Option<Uuid>,
}

/// Stable reason that an appointed current manager cannot obtain a root
/// self-succession fence. These are workflow outcomes, not raw Store errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagerSuccessionDenialReasonV2 {
    CurrentManagerUnavailable,
    CurrentManagerNotParentlessStandard,
    /// The operator setting `context_rotation_enabled` is off: guarded
    /// succession refuses (`manager_succession_rotation_disabled`) before any
    /// queue or reservation, so no fence is published.
    ContextRotationDisabled,
}

/// Bounded operator action associated with a succession denial. A denial never
/// carries a guessed authority or custody fence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagerSuccessionRequiredActionV2 {
    RepairManagerLineage,
    AppointParentlessStandardManager,
    /// Operator-only: turn `context_rotation_enabled` on. An agent keeps its
    /// seat, wakes and obligations meanwhile; nothing is cancelled for this.
    OperatorEnableContextRotation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagerSuccessionDenialV2 {
    pub reason: ManagerSuccessionDenialReasonV2,
    pub required_action: ManagerSuccessionRequiredActionV2,
}

/// Read-only `manager_control` preflight returned by an unfiltered Overview.
/// Eligibility only publishes exact server-observed fences; action admission
/// still rechecks SelfSuccession, policy, custody, handoff and publication.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "eligibility", rename_all = "snake_case", deny_unknown_fields)]
pub enum ManagerSuccessionPreflightV2 {
    Eligible {
        #[serde(flatten)]
        observation: ManagerSuccessionObservationV2,
    },
    Ineligible {
        logical_manager_session_id: Uuid,
        current_session_id: Option<Uuid>,
        denial: ManagerSuccessionDenialV2,
    },
}

impl ManagerSuccessionPreflightV2 {
    /// The one typed readiness for a disabled `context_rotation_enabled`. The
    /// Store (persisted setting) and the daemon's Overview overlay (live
    /// RuntimeConfig value) both build it here, so the denial cannot differ.
    pub fn rotation_disabled(logical_manager_session_id: Uuid, current_session_id: Uuid) -> Self {
        Self::Ineligible {
            logical_manager_session_id,
            current_session_id: Some(current_session_id),
            denial: ManagerSuccessionDenialV2 {
                reason: ManagerSuccessionDenialReasonV2::ContextRotationDisabled,
                required_action: ManagerSuccessionRequiredActionV2::OperatorEnableContextRotation,
            },
        }
    }
}

/// Stable, agent-authored portion of a prepared lifecycle action. Mutable
/// manager, policy, topology, lead, event, and custody fences are deliberately
/// absent: the daemon resolves and persists them when the action is prepared.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum PreparedManagerActionV2 {
    ResumeLead {
        epic_id: Uuid,
        message: String,
    },
    PauseLead {
        epic_id: Uuid,
        reason: String,
    },
    RetryLead {
        epic_id: Uuid,
        message: String,
        launch: Option<ManagerLaunchChoiceV2>,
    },
    ReplaceLead {
        epic_id: Uuid,
        query: String,
        launch: ManagerLaunchChoiceV2,
    },
    CreateSession {
        parent_id: Uuid,
        kind: SessionKind,
        query: String,
        launch: ManagerLaunchChoiceV2,
        /// #1195: omitted means a fresh `origin/rolling` base.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sandbox_source: Option<ManagerSandboxSourceV1>,
    },
    AssignLead {
        epic_id: Uuid,
        session_id: Option<Uuid>,
    },
}

impl PreparedManagerActionV2 {
    pub fn capability(&self) -> ManagerCapabilityV2 {
        match self {
            Self::ResumeLead { .. }
            | Self::PauseLead { .. }
            | Self::RetryLead { .. }
            | Self::ReplaceLead { .. } => ManagerCapabilityV2::LeadControl,
            Self::CreateSession { .. } => ManagerCapabilityV2::SessionCreate,
            Self::AssignLead { .. } => ManagerCapabilityV2::LeadAssign,
        }
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        match self {
            Self::ResumeLead { epic_id, message }
            | Self::RetryLead {
                epic_id, message, ..
            } => {
                if epic_id.is_nil() {
                    return Err("manager_v2_invalid_prepared_action");
                }
                text(message, 8192)?;
                if let Self::RetryLead {
                    launch: Some(launch),
                    ..
                } = self
                {
                    launch.validate()?;
                }
                Ok(())
            }
            Self::PauseLead { epic_id, reason } => {
                if epic_id.is_nil() {
                    return Err("manager_v2_invalid_prepared_action");
                }
                text(reason, 8192)
            }
            Self::ReplaceLead {
                epic_id,
                query,
                launch,
            } => {
                if epic_id.is_nil() {
                    return Err("manager_v2_invalid_prepared_action");
                }
                text(query, 32_768)?;
                launch.validate()
            }
            Self::CreateSession {
                parent_id,
                query,
                launch,
                sandbox_source,
                ..
            } => {
                if parent_id.is_nil() {
                    return Err("manager_v2_invalid_prepared_action");
                }
                text(query, 32_768)?;
                launch.validate()?;
                sandbox_source
                    .as_ref()
                    .map_or(Ok(()), ManagerSandboxSourceV1::validate)
            }
            Self::AssignLead {
                epic_id,
                session_id,
            } => {
                if epic_id.is_nil() || session_id.is_some_and(|id| id.is_nil()) {
                    Err("manager_v2_invalid_prepared_action")
                } else {
                    Ok(())
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentManagerPrepareControlRequestV2 {
    pub operation: PreparedManagerActionV2,
    /// #1235: the target project of a global manager seat acting inside its
    /// operator grant. Omitted means the caller's own project. A target the
    /// daemon checks against the grant, never caller identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
}

impl AgentManagerPrepareControlRequestV2 {
    pub fn validate(&self) -> Result<(), &'static str> {
        self.operation.validate()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagerPreparedActionReadinessV2 {
    Ready,
    Blocked,
}

/// Closed preflight classification. The strings carried by Store errors stay
/// internal; callers receive only these bounded workflow classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagerPreparedActionBlockerCodeV2 {
    PendingOperatorDecision,
    HumanOrRecoveryOwner,
    OperatorPause,
    ProgramEvidence,
    CapacityRecovery,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagerPreparedActionRequiredActionV2 {
    AnswerOperatorDecision,
    ResolveHumanOrRecoveryOwner,
    ResumeManagerOrPolicy,
    InspectProgramEvidence,
    WaitOrAdjustCapacity,
    /// The manager's own `PauseLead` pause on one Epic: `ResumeLead` clears it.
    ResumeEpicLead,
    /// The operator-saved policy pauses the manager or this Epic: edit the policy.
    ResumePolicyPause,
}

/// What a pause blocker is about (#1104).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagerPreparedActionPauseSubjectV2 {
    /// The whole policy is paused.
    Policy,
    /// One Epic is listed in the policy's paused Epics.
    PolicyEpic,
    /// One Epic's lead continuations were paused by a `PauseLead` action.
    EpicLead,
}

/// Names exactly what is paused, by whom and since when.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagerPreparedActionPauseDetailV2 {
    pub subject: ManagerPreparedActionPauseSubjectV2,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epic_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epic_title: Option<String>,
    /// `operator policy` or `manager session <id>`.
    pub paused_by: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagerPreparedActionBlockerV2 {
    pub code: ManagerPreparedActionBlockerCodeV2,
    pub required_action: ManagerPreparedActionRequiredActionV2,
    /// Present on `operator_pause` blockers (#1104).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<ManagerPreparedActionPauseDetailV2>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagerPreparedActionReceiptV2 {
    pub prepared_id: Uuid,
    pub target_digest: String,
    pub readiness: ManagerPreparedActionReadinessV2,
    pub blockers: Vec<ManagerPreparedActionBlockerV2>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentManagerCommitPreparedControlRequestV2 {
    pub prepared_id: Uuid,
    pub target_digest: String,
    pub idempotency_key: String,
    /// #1235: the target project of a global manager seat acting inside its
    /// operator grant. Omitted means the caller's own project. A target the
    /// daemon checks against the grant, never caller identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
}

impl AgentManagerCommitPreparedControlRequestV2 {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.prepared_id.is_nil()
            || self.target_digest.len() != 71
            || !self.target_digest.starts_with("sha256:")
            || !self.target_digest[7..]
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err("manager_v2_invalid_prepared_commit");
        }
        text(&self.idempotency_key, 128)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum ManagerPreparedActionCommitResultV2 {
    Queued {
        receipt: ManagerActionReceiptV2,
    },
    Blocked {
        prepared_id: Uuid,
        target_digest: String,
        blockers: Vec<ManagerPreparedActionBlockerV2>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentManagerGetActionRequestV2 {
    pub operation_id: Uuid,
    /// #1235: the target project of a global manager seat acting inside its
    /// operator grant. Omitted means the caller's own project. A target the
    /// daemon checks against the grant, never caller identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
}

impl AgentManagerGetActionRequestV2 {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.operation_id.is_nil() {
            Err("manager_v2_invalid_operation_id")
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum ManagerActionV2 {
    SucceedManager {
        expected: ManagerSuccessionFenceV2,
        launch: ManagerLaunchChoiceV2,
        handoff: ManagerCommittedHandoffV2,
    },
    ResumeLead {
        epic_id: Uuid,
        expected: ManagerLeadFenceV2,
        message: String,
    },
    PauseLead {
        epic_id: Uuid,
        expected: ManagerLeadFenceV2,
        reason: String,
    },
    RetryLead {
        epic_id: Uuid,
        expected: ManagerLeadFenceV2,
        message: String,
        launch: Option<ManagerLaunchChoiceV2>,
    },
    ReplaceLead {
        epic_id: Uuid,
        expected: ManagerLeadFenceV2,
        query: String,
        launch: ManagerLaunchChoiceV2,
    },
    CreateContainer {
        parent_id: Option<Uuid>,
        kind: SessionKind,
        name: String,
        tags: Vec<String>,
    },
    UpdateContainer {
        container_id: Uuid,
        expected_updated_at: DateTime<Utc>,
        name: String,
        description: Option<String>,
    },
    ArchiveContainer {
        container_id: Uuid,
        expected_updated_at: DateTime<Utc>,
    },
    DeleteContainer {
        container_id: Uuid,
        expected_updated_at: DateTime<Utc>,
    },
    RestoreContainer {
        container_id: Uuid,
        expected_updated_at: DateTime<Utc>,
    },
    CreateSession {
        parent_id: Uuid,
        kind: SessionKind,
        query: String,
        launch: ManagerLaunchChoiceV2,
        /// #1195: omitted means a fresh `origin/rolling` base.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sandbox_source: Option<ManagerSandboxSourceV1>,
    },
    AssignLead {
        epic_id: Uuid,
        expected: ManagerLeadFenceV2,
        session_id: Option<Uuid>,
    },
    Integrate {
        work_key: String,
        target_ref: String,
        expected_tip: String,
        source_commit: String,
    },
    /// Ask the daemon to re-observe one uncertain action's effect. It never
    /// re-executes that effect; unobservable effects stay uncertain.
    SettleUncertainAction {
        operation_id: Uuid,
        expected_row_version: i64,
    },
    /// Disable (never delete) a terminal or idle lead's resume wakes and
    /// program sentinel and supersede its exact agent-declared program outcome.
    RetireLeadContinuations {
        epic_id: Uuid,
        expected: ManagerLeadFenceV2,
    },
    /// Archive one terminal scoped leaf session (never a cascade).
    ArchiveSession {
        session_id: Uuid,
        expected_updated_at: DateTime<Utc>,
    },
    /// Restore one Archived scoped leaf session to Completed.
    RestoreSession {
        session_id: Uuid,
        expected_updated_at: DateTime<Utc>,
    },
    /// Edit one scoped leaf session's operator metadata.
    UpdateSession {
        session_id: Uuid,
        expected_updated_at: DateTime<Utc>,
        patch: ManagerSessionPatchV2,
    },
    /// K14 (#672): one delegated operator method under `OperatorDelegation`.
    /// The daemon resolves the closed allowlist; other methods are refused.
    OperatorCall {
        call: OperatorCallV1,
        #[serde(default)]
        expected: Option<OperatorCallFenceV1>,
    },
}

/// Set or explicitly clear one optional session field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagerFieldPatchV2<T> {
    Set(T),
    Clear,
}

impl<T> ManagerFieldPatchV2<T> {
    #[must_use]
    pub const fn value(&self) -> Option<&T> {
        match self {
            Self::Set(value) => Some(value),
            Self::Clear => None,
        }
    }
}

/// Typed per-session metadata patch. Every field is optional; at least one
/// must be present. `tags` is a full replacement set; `label` assigns an
/// existing label (label definitions stay operator-only).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagerSessionPatchV2 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<ManagerFieldPatchV2<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rating: Option<ManagerFieldPatchV2<i16>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_task: Option<ManagerFieldPatchV2<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<ManagerFieldPatchV2<Uuid>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
}

impl ManagerSessionPatchV2 {
    pub const MAX_TAGS: usize = 32;

    /// Shape validation only; tag normalization and label existence are
    /// checked by the daemon against its store.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.title.is_none()
            && self.description.is_none()
            && self.rating.is_none()
            && self.active_task.is_none()
            && self.label.is_none()
            && self.tags.is_none()
        {
            return Err("manager_v2_empty_session_patch");
        }
        if let Some(title) = &self.title {
            text(title, 512)?;
        }
        if let Some(ManagerFieldPatchV2::Set(description)) = &self.description
            && (description.len() > 8192 || description.contains('\0'))
        {
            return Err("manager_v2_invalid_description");
        }
        if let Some(ManagerFieldPatchV2::Set(rating)) = &self.rating
            && !(1..=10).contains(rating)
        {
            return Err("manager_v2_invalid_rating");
        }
        if let Some(ManagerFieldPatchV2::Set(task)) = &self.active_task {
            text(task, 8192)?;
        }
        if let Some(ManagerFieldPatchV2::Set(label)) = &self.label
            && label.is_nil()
        {
            return Err("manager_v2_invalid_action");
        }
        if let Some(tags) = &self.tags
            && (tags.is_empty() || tags.len() > Self::MAX_TAGS)
        {
            return Err("manager_v2_invalid_tags");
        }
        Ok(())
    }
}

impl ManagerActionV2 {
    /// Per-session housekeeping target (a scoped leaf), if this is one.
    #[must_use]
    pub const fn housekeeping_session(&self) -> Option<Uuid> {
        match self {
            Self::ArchiveSession { session_id, .. }
            | Self::RestoreSession { session_id, .. }
            | Self::UpdateSession { session_id, .. } => Some(*session_id),
            _ => None,
        }
    }

    pub const fn action_kind(&self) -> ManagerActionKindV2 {
        match self {
            Self::SucceedManager { .. } => ManagerActionKindV2::SucceedManager,
            Self::ResumeLead { .. } => ManagerActionKindV2::ResumeLead,
            Self::PauseLead { .. } => ManagerActionKindV2::PauseLead,
            Self::RetryLead { .. } => ManagerActionKindV2::RetryLead,
            Self::ReplaceLead { .. } => ManagerActionKindV2::ReplaceLead,
            Self::CreateContainer { .. } => ManagerActionKindV2::CreateContainer,
            Self::UpdateContainer { .. } => ManagerActionKindV2::UpdateContainer,
            Self::ArchiveContainer { .. } => ManagerActionKindV2::ArchiveContainer,
            Self::DeleteContainer { .. } => ManagerActionKindV2::DeleteContainer,
            Self::RestoreContainer { .. } => ManagerActionKindV2::RestoreContainer,
            Self::CreateSession { .. } => ManagerActionKindV2::CreateSession,
            Self::AssignLead { .. } => ManagerActionKindV2::AssignLead,
            Self::Integrate { .. } => ManagerActionKindV2::Integrate,
            Self::SettleUncertainAction { .. } => ManagerActionKindV2::SettleUncertainAction,
            Self::RetireLeadContinuations { .. } => ManagerActionKindV2::RetireLeadContinuations,
            Self::ArchiveSession { .. } => ManagerActionKindV2::ArchiveSession,
            Self::RestoreSession { .. } => ManagerActionKindV2::RestoreSession,
            Self::UpdateSession { .. } => ManagerActionKindV2::UpdateSession,
            Self::OperatorCall { .. } => ManagerActionKindV2::OperatorCall,
        }
    }

    #[must_use]
    pub fn target_type(&self) -> ManagerActionTargetTypeV2 {
        match self {
            Self::SucceedManager { .. } => ManagerActionTargetTypeV2::ManagerSession,
            Self::ResumeLead { .. }
            | Self::PauseLead { .. }
            | Self::AssignLead { .. }
            | Self::RetireLeadContinuations { .. } => ManagerActionTargetTypeV2::EpicLead,
            Self::SettleUncertainAction { .. } => ManagerActionTargetTypeV2::ManagerOperation,
            Self::RetryLead { .. }
            | Self::ReplaceLead { .. }
            | Self::CreateSession { .. }
            | Self::ArchiveSession { .. }
            | Self::RestoreSession { .. }
            | Self::UpdateSession { .. } => ManagerActionTargetTypeV2::ProviderSession,
            Self::CreateContainer {
                kind: SessionKind::Group,
                ..
            } => ManagerActionTargetTypeV2::GroupContainer,
            Self::CreateContainer {
                kind: SessionKind::Epic,
                ..
            } => ManagerActionTargetTypeV2::EpicContainer,
            Self::CreateContainer { .. }
            | Self::UpdateContainer { .. }
            | Self::ArchiveContainer { .. }
            | Self::DeleteContainer { .. }
            | Self::RestoreContainer { .. } => ManagerActionTargetTypeV2::Container,
            Self::Integrate { .. } => ManagerActionTargetTypeV2::IntegrationTarget,
            Self::OperatorCall { call, .. } => match call.typed().map(|c| c.session_id()) {
                Ok(Some(_)) => ManagerActionTargetTypeV2::ProviderSession,
                Ok(None) => ManagerActionTargetTypeV2::Project,
                Err(_) => ManagerActionTargetTypeV2::Unknown,
            },
        }
    }

    fn successful_result(&self) -> ManagerActionResultV2 {
        match self {
            Self::SucceedManager { .. } => ManagerActionResultV2::ManagerPublished,
            Self::ResumeLead { .. } => ManagerActionResultV2::LeadResumed,
            Self::PauseLead { .. } => ManagerActionResultV2::LeadPaused,
            Self::RetryLead { .. } | Self::ReplaceLead { .. } => {
                ManagerActionResultV2::ProviderEstablished {
                    lead_state: Some(ManagerLeadAssignmentStateV2::Assigned),
                }
            }
            Self::CreateContainer { kind, .. } => ManagerActionResultV2::ContainerCommitted {
                lead_state: (*kind == SessionKind::Epic)
                    .then_some(ManagerLeadAssignmentStateV2::Unassigned),
            },
            Self::UpdateContainer { .. } => ManagerActionResultV2::ContainerUpdated,
            Self::ArchiveContainer { .. } => ManagerActionResultV2::ContainerArchived,
            Self::DeleteContainer { .. } => ManagerActionResultV2::ContainerDeleted,
            Self::RestoreContainer { .. } => ManagerActionResultV2::ContainerRestored,
            Self::CreateSession { .. } => {
                ManagerActionResultV2::ProviderEstablished { lead_state: None }
            }
            Self::AssignLead {
                session_id: Some(_),
                ..
            } => ManagerActionResultV2::LeadAssigned,
            Self::AssignLead {
                session_id: None, ..
            } => ManagerActionResultV2::LeadUnassigned,
            Self::Integrate { .. } => ManagerActionResultV2::SourceIntegrated,
            Self::SettleUncertainAction { .. } => ManagerActionResultV2::UncertainActionSettled,
            Self::RetireLeadContinuations { .. } => ManagerActionResultV2::LeadContinuationsRetired,
            Self::ArchiveSession { .. } => ManagerActionResultV2::SessionArchived,
            Self::RestoreSession { .. } => ManagerActionResultV2::SessionRestored,
            Self::UpdateSession { .. } => ManagerActionResultV2::SessionUpdated,
            Self::OperatorCall { .. } => ManagerActionResultV2::OperatorCallSucceeded,
        }
    }

    pub fn capability(&self) -> ManagerCapabilityV2 {
        match self {
            Self::SucceedManager { .. } => ManagerCapabilityV2::SelfSuccession,
            Self::ResumeLead { .. }
            | Self::PauseLead { .. }
            | Self::RetryLead { .. }
            | Self::ReplaceLead { .. }
            | Self::SettleUncertainAction { .. }
            | Self::RetireLeadContinuations { .. } => ManagerCapabilityV2::LeadControl,
            Self::CreateContainer { .. }
            | Self::UpdateContainer { .. }
            | Self::ArchiveContainer { .. }
            | Self::DeleteContainer { .. }
            | Self::RestoreContainer { .. } => ManagerCapabilityV2::Topology,
            Self::CreateSession { .. } => ManagerCapabilityV2::SessionCreate,
            Self::AssignLead { .. } => ManagerCapabilityV2::LeadAssign,
            Self::Integrate { .. } => ManagerCapabilityV2::GitEffect,
            Self::ArchiveSession { .. }
            | Self::RestoreSession { .. }
            | Self::UpdateSession { .. } => ManagerCapabilityV2::SessionControl,
            Self::OperatorCall { call, .. } if call.requires_storage_control() => {
                ManagerCapabilityV2::StorageControl
            }
            Self::OperatorCall { call, .. } if call.requires_daemon_settings() => {
                ManagerCapabilityV2::DaemonSettings
            }
            Self::OperatorCall { .. } => ManagerCapabilityV2::OperatorDelegation,
        }
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        match self {
            Self::SucceedManager {
                expected,
                launch,
                handoff,
            } => {
                expected.validate()?;
                launch.validate()?;
                handoff.validate()
            }
            Self::ResumeLead {
                epic_id,
                expected,
                message,
            }
            | Self::RetryLead {
                epic_id,
                expected,
                message,
                ..
            } => {
                if epic_id.is_nil() {
                    return Err("manager_v2_invalid_action");
                }
                expected.validate()?;
                text(message, 8192)
            }
            Self::PauseLead {
                epic_id,
                expected,
                reason,
            } => {
                if epic_id.is_nil() {
                    return Err("manager_v2_invalid_action");
                }
                expected.validate()?;
                text(reason, 8192)
            }
            Self::ReplaceLead {
                epic_id,
                expected,
                query,
                launch,
            } => {
                if epic_id.is_nil() {
                    return Err("manager_v2_invalid_action");
                }
                expected.validate()?;
                text(query, 262_144)?;
                launch.validate()
            }
            Self::CreateContainer {
                parent_id,
                name,
                tags,
                ..
            } => {
                if parent_id.is_some_and(|id| id.is_nil()) || tags.len() > 64 {
                    return Err("manager_v2_invalid_action");
                }
                text(name, 512)
            }
            Self::UpdateContainer {
                container_id,
                name,
                description,
                ..
            } => {
                if container_id.is_nil() {
                    return Err("manager_v2_invalid_action");
                }
                text(name, 512)?;
                if let Some(description) = description {
                    text(description, 8192)?;
                }
                Ok(())
            }
            Self::ArchiveContainer { container_id, .. }
            | Self::DeleteContainer { container_id, .. }
            | Self::RestoreContainer { container_id, .. } => {
                if container_id.is_nil() {
                    Err("manager_v2_invalid_action")
                } else {
                    Ok(())
                }
            }
            Self::CreateSession {
                parent_id,
                query,
                launch,
                sandbox_source,
                ..
            } => {
                if parent_id.is_nil() {
                    return Err("manager_v2_invalid_action");
                }
                text(query, 262_144)?;
                launch.validate()?;
                sandbox_source
                    .as_ref()
                    .map_or(Ok(()), ManagerSandboxSourceV1::validate)
            }
            Self::AssignLead {
                epic_id,
                expected,
                session_id,
            } => {
                if epic_id.is_nil() || session_id.is_some_and(|id| id.is_nil()) {
                    return Err("manager_v2_invalid_action");
                }
                expected.validate()
            }
            Self::Integrate {
                work_key,
                target_ref,
                expected_tip,
                source_commit,
            } => {
                text(work_key, 256)?;
                text(target_ref, 256)?;
                if !canonical_git_oid(expected_tip) || !canonical_git_oid(source_commit) {
                    return Err("manager_v2_invalid_action");
                }
                Ok(())
            }
            Self::SettleUncertainAction {
                operation_id,
                expected_row_version,
            } => {
                if operation_id.is_nil() || *expected_row_version <= 0 {
                    return Err("manager_v2_invalid_action");
                }
                Ok(())
            }
            Self::RetireLeadContinuations { epic_id, expected } => {
                if epic_id.is_nil() {
                    return Err("manager_v2_invalid_action");
                }
                expected.validate()
            }
            Self::ArchiveSession { session_id, .. } | Self::RestoreSession { session_id, .. } => {
                if session_id.is_nil() {
                    Err("manager_v2_invalid_action")
                } else {
                    Ok(())
                }
            }
            Self::UpdateSession {
                session_id, patch, ..
            } => {
                if session_id.is_nil() {
                    return Err("manager_v2_invalid_action");
                }
                patch.validate()
            }
            // The method allowlist is resolved by the daemon so a refused
            // method is answered with its typed code, not a decode error.
            Self::OperatorCall { call, .. } => call.validate(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentManagerControlRequestV2 {
    pub fence: ManagerFenceV2,
    pub idempotency_key: String,
    pub operation: ManagerActionV2,
    /// #1235: the target project of a global manager seat acting inside its
    /// operator grant. Omitted means the caller's own project. A target the
    /// daemon checks against the grant, never caller identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
}

impl AgentManagerControlRequestV2 {
    /// Build a control request while keeping its authority fence, replay key,
    /// and operation explicit at every call site.
    pub fn new(fence: ManagerFenceV2, idempotency_key: String, operation: ManagerActionV2) -> Self {
        Self {
            fence,
            idempotency_key,
            operation,
            project_id: None,
        }
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        self.fence.validate()?;
        text(&self.idempotency_key, 128)?;
        self.operation.validate()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagerActionStateV2 {
    Queued,
    Running,
    Succeeded,
    Failed,
    Blocked,
    Uncertain,
    Revoked,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagerActionKindV2 {
    #[default]
    Unknown,
    SucceedManager,
    ResumeLead,
    PauseLead,
    RetryLead,
    ReplaceLead,
    CreateContainer,
    UpdateContainer,
    ArchiveContainer,
    DeleteContainer,
    RestoreContainer,
    CreateSession,
    AssignLead,
    Integrate,
    SettleUncertainAction,
    RetireLeadContinuations,
    ArchiveSession,
    RestoreSession,
    UpdateSession,
    OperatorCall,
}

/// Semantic target class for a manager action. A container reservation is not
/// a provider session, even though both receive a durable target UUID.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagerActionTargetTypeV2 {
    #[default]
    Unknown,
    ManagerSession,
    EpicLead,
    Container,
    GroupContainer,
    EpicContainer,
    ProviderSession,
    IntegrationTarget,
    /// A project-bound delegated read with no single target session.
    Project,
    /// Another manager lifecycle operation (uncertain-action settlement).
    ManagerOperation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagerLeadAssignmentStateV2 {
    Assigned,
    Unassigned,
}

/// A confirmed target result exists only after a succeeded receipt. Queue
/// admission and running execution deliberately carry no result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "target_state", rename_all = "snake_case")]
pub enum ManagerActionResultV2 {
    ManagerPublished,
    LeadResumed,
    LeadPaused,
    ProviderEstablished {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lead_state: Option<ManagerLeadAssignmentStateV2>,
    },
    ContainerCommitted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lead_state: Option<ManagerLeadAssignmentStateV2>,
    },
    ContainerUpdated,
    ContainerArchived,
    ContainerDeleted,
    ContainerRestored,
    LeadAssigned,
    LeadUnassigned,
    SourceIntegrated,
    UncertainActionSettled,
    LeadContinuationsRetired,
    SessionArchived,
    SessionRestored,
    SessionUpdated,
    OperatorCallSucceeded,
}

/// Acceptance provenance for a source commit cleared for integration.
/// Backed by the manager-ledger work-record acceptance
/// (`source_accepted`, `acceptance.source_commit`, `acceptance.method`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptedSource {
    pub work_key: String,
    pub epic_id: Uuid,
    pub source_commit: String,
    pub method: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagerActionReceiptV2 {
    pub operation_id: Uuid,
    pub state: ManagerActionStateV2,
    /// Additive semantic metadata. Defaults retain decoding compatibility with
    /// receipts written before lifecycle-clarity fields existed.
    #[serde(default)]
    pub action_kind: ManagerActionKindV2,
    #[serde(default)]
    pub target_type: ManagerActionTargetTypeV2,
    pub row_version: i64,
    pub target_session_id: Option<Uuid>,
    pub outcome: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<ManagerActionResultV2>,
    pub deduplicated: bool,
    /// K14 (#672): bounded result of a succeeded `operator_call`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Boxed so every receipt held by value in async lifecycle paths stays
    /// as small as before K14 (inline it added 96 bytes per receipt).
    pub operator_result: Option<Box<OperatorCallResultV1>>,
    /// #1195: the sandbox source of an admitted `create_session`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_source: Option<ManagerSandboxSourceReceiptV1>,
    /// #2652: the resume wakes a succeeded `succeed_manager` cancelled with the
    /// displaced seat. Present only on that action's succeeded receipt and only
    /// when it cancelled at least one wake; stored in the receipt itself, so
    /// every later read, refresh and idempotent replay returns the same ids.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancelled_wakes: Option<ManagerCancelledWakesV1>,
}

/// Resume wakes retired together with a displaced manager seat. The rows stay,
/// disabled; the audit event `succession_wakes_settled` records the same ids.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagerCancelledWakesV1 {
    /// Why they were cancelled (`manager_succession_displaced_seat`).
    pub reason: String,
    /// The disabled `scheduled_jobs` ids, sorted.
    pub job_ids: Vec<Uuid>,
}

/// #1311: why a queued action has not started yet. Present only while the
/// action is queued and something holds it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ManagerActionHeldV1 {
    /// `deploy_draining`: a deploy waits for its quiet point and holds new
    /// worker starts. The action runs when the hold releases.
    ///
    /// `host_load` (#1417): the host's 1-minute load is above the operator's
    /// `host_load_admission_threshold`, so the daemon holds this
    /// `create_session` (queued, never refused). It starts on its own once
    /// the load drops; held creates are released oldest-first. `load` and
    /// `threshold` are set, `deploy_id` and `release_by` are not (a load hold
    /// has no release time).
    ///
    /// `thermal` and `disk_pressure` (#2562): the host CPU is at or above
    /// `job_thermal_ceiling_c`, or free disk is below the governor floor
    /// `min_free_disk_gb`; the same predicate holds build/test jobs. The
    /// `create_session` is queued, never refused, and starts once the
    /// condition clears, oldest first, sharing one queue with `host_load`.
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deploy_id: Option<Uuid>,
    /// RFC3339 with nanoseconds: the hold releases no later than this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_by: Option<String>,
    /// `host_load` only: the host's 1-minute load average when last read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load: Option<f64>,
    /// `host_load` only: the operator threshold the load is compared with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold: Option<u32>,
    /// `host_load` only: launches the daemon admitted in the last minute that
    /// the 1-minute load has not absorbed yet; each counts one toward the
    /// comparison with `threshold` (`load + recent_admissions > threshold`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recent_admissions: Option<u32>,
    /// `thermal` only: the hottest CPU sensor, in degrees Celsius, when last read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature_c: Option<f64>,
    /// `thermal` only: the operator's `job_thermal_ceiling_c` the reading met.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ceiling_c: Option<u32>,
    /// `disk_pressure` only: free GiB on `/` when last read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub free_disk_gb: Option<f64>,
    /// `disk_pressure` only: the governor's `min_free_disk_gb` floor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_free_disk_gb: Option<u64>,
}

/// `AgentManagerGetAction` result: the stored receipt plus, while the action
/// is queued behind a hold, why (#1311, #1417).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManagerActionViewV2 {
    #[serde(flatten)]
    pub receipt: ManagerActionReceiptV2,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held: Option<ManagerActionHeldV1>,
}

impl ManagerActionReceiptV2 {
    /// Populate additive metadata for both new and legacy stored receipts.
    /// Terminal failure states keep their safe legacy `outcome` and never gain
    /// a successful target result.
    pub fn refresh_action_metadata(&mut self, action: &ManagerActionV2) {
        self.action_kind = action.action_kind();
        self.target_type = action.target_type();
        self.result =
            (self.state == ManagerActionStateV2::Succeeded).then(|| action.successful_result());
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagerWorkKindV2 {
    Program,
    Product,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagerWorkStageV2 {
    Planning,
    Implementation,
    Review,
    Verification,
    Integration,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagerStageStateV2 {
    #[default]
    Unknown,
    Pending,
    Running,
    Partial,
    Passed,
    Failed,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagerRequestStateV2 {
    Queued,
    Retrieved,
    Accepted,
    Declined,
    Running,
    Completed,
    Failed,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagerOwnershipModeV2 {
    Shared,
    Exclusive,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagerEvidenceV2 {
    pub source_session_id: Uuid,
    pub source_commit: String,
    pub artifact_path: String,
    pub artifact_commit: String,
    /// Optional already admitted Closure evidence receipt. A reported source
    /// artifact alone does not become independent verification.
    pub closure_evidence_id: Option<Uuid>,
}

/// A terminal verification job the authenticated caller owns, cited by a stage
/// update (`job_evidence`) instead of a worker session's evidence. Only the
/// job id and the committed report are supplied: the daemon reads the owner,
/// terminal state, receipt and tested commit from its own job table, so the
/// supplied `source_commit` must equal what that receipt records. It names no
/// session, so it widens no session scope and stands for no worker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagerJobEvidenceV2 {
    pub job_id: Uuid,
    pub source_commit: String,
    pub artifact_path: String,
    pub artifact_commit: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagerReviewVerdictV1 {
    Accepted,
    ChangesRequested,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagerReviewFindingSeverityV1 {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagerReviewFindingV1 {
    pub key: String,
    pub severity: ManagerReviewFindingSeverityV1,
    pub summary: String,
    #[serde(default)]
    pub location: Option<String>,
    pub blocking: bool,
}

impl ManagerReviewFindingV1 {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.key.len() > 64
            || self.key.is_empty()
            || !self
                .key
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err("manager_review_invalid_finding_key");
        }
        text(&self.summary, 2048)?;
        if let Some(location) = &self.location {
            text(location, 1024)?;
        }
        Ok(())
    }
}

/// Key prefix of a finding that reports an infrastructure outcome (#2570): a
/// check the reviewer needed could not run (no build slot, a missing runner),
/// which says nothing about the code under review.
pub const REVIEW_INFRASTRUCTURE_FINDING_PREFIX: &str = "infra_";

/// The unsettled reason a topology review reports for a receipt that
/// [`review_receipt_is_infrastructure_only`] (#2570).
pub const REVIEW_INFRASTRUCTURE_REASON: &str = "manager_review_infrastructure_unavailable";

/// The unsettled reason a topology review reports for a `blocked` receipt whose
/// reviewer session a hub deploy (`interrupt_workers`) cut off (#2809). Like
/// [`REVIEW_INFRASTRUCTURE_REASON`] it is no verdict on the code: the review
/// ledger does not count the receipt as a spent round and the executor retries
/// the review as an uncharged infrastructure loss.
pub const REVIEW_INTERRUPTED_REASON: &str = "manager_review_reviewer_interrupted_by_deploy";

/// Finding keys reviewers used for an unavailable test run before the typed
/// `infra_` prefix existed. They keep their infrastructure meaning.
const REVIEW_INFRASTRUCTURE_LEGACY_KEYS: [&str; 4] = [
    "focused_tests_unavailable",
    "required_tests_unrun",
    "tests_unavailable",
    "tests_unrun",
];

/// Whether `key` names an infrastructure outcome rather than a code finding.
#[must_use]
pub fn review_finding_is_infrastructure(key: &str) -> bool {
    key.starts_with(REVIEW_INFRASTRUCTURE_FINDING_PREFIX)
        || REVIEW_INFRASTRUCTURE_LEGACY_KEYS.contains(&key)
}

/// Whether a non-accepting receipt reports only infrastructure unavailability
/// (#2570): at least one finding is an infrastructure outcome and every
/// blocking finding is one. Such a receipt is never a code verdict; the
/// executor retries the review instead of blocking on it and the review ledger
/// does not count it as a spent round. `findings` is `(key, blocking)`.
#[must_use]
pub fn review_receipt_is_infrastructure_only<'a>(
    findings: impl IntoIterator<Item = (&'a str, bool)>,
) -> bool {
    let mut infrastructure = false;
    for (key, blocking) in findings {
        if review_finding_is_infrastructure(key) {
            infrastructure = true;
        } else if blocking {
            return false;
        }
    }
    infrastructure
}

/// Reviewer-authored content only. The daemon binds the caller, source,
/// invocation, and custody from the live assignment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSubmitReviewReceiptRequestV1 {
    pub assignment_id: Uuid,
    pub verdict: ManagerReviewVerdictV1,
    pub findings: Vec<ManagerReviewFindingV1>,
    pub idempotency_key: String,
}

impl AgentSubmitReviewReceiptRequestV1 {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.assignment_id.is_nil()
            || self.findings.len() > MANAGER_REVIEW_MAX_FINDINGS
            || self.findings.iter().enumerate().any(|(index, finding)| {
                finding.validate().is_err()
                    || self.findings[..index]
                        .iter()
                        .any(|prior| prior.key == finding.key)
            })
        {
            return Err("manager_review_invalid_receipt");
        }
        text(&self.idempotency_key, 128)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagerReviewReceiptV1 {
    pub receipt_id: Uuid,
    pub assignment_id: Uuid,
    pub source_commit: String,
    pub verdict: ManagerReviewVerdictV1,
    pub request_fingerprint: String,
    pub created_at: DateTime<Utc>,
    pub deduplicated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "update", rename_all = "snake_case", deny_unknown_fields)]
pub enum ManagerUpdateV2 {
    Work {
        key: String,
        expected_row_version: i64,
        epic_id: Uuid,
        title: String,
        kind: ManagerWorkKindV2,
        priority: u8,
        weight: u16,
        required_gates: Vec<ManagerWorkStageV2>,
        #[serde(default)]
        risk_tier: ManagerWorkRiskTierV2,
    },
    Stage {
        key: String,
        expected_row_version: i64,
        stage: ManagerWorkStageV2,
        state: ManagerStageStateV2,
        note: String,
        evidence: Option<ManagerEvidenceV2>,
        // Omitted when absent, so a stored request replays byte-identically.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        job_evidence: Option<ManagerJobEvidenceV2>,
    },
    Dependency {
        key: String,
        expected_row_version: i64,
        prerequisite: String,
        require_integrated: bool,
        enabled: bool,
    },
    Ownership {
        key: String,
        expected_row_version: i64,
        domain: String,
        mode: ManagerOwnershipModeV2,
        files: Vec<String>,
        active: bool,
    },
    Migration {
        key: String,
        expected_row_version: i64,
        version: u32,
        baseline_commit: String,
        inventory_digest: String,
    },
    /// Seal an exact Work source; the daemon chooses its landing-order version.
    MigrationSeal {
        key: String,
        expected_row_version: i64,
        source_commit: String,
        expires_at: DateTime<Utc>,
    },
    /// Withdraw an unlanded exact-source seal. The claim row version is its
    /// staleness fence; the allocator compacts later versions in seal order.
    MigrationSealRelease {
        key: String,
        claim_id: Uuid,
        expected_row_version: i64,
        source_commit: String,
    },
    /// Transfer one unlanded claim to a replacement Work source.
    MigrationSealTransfer {
        key: String,
        claim_id: Uuid,
        expected_row_version: i64,
        source_commit: String,
        new_source_commit: String,
        expires_at: DateTime<Utc>,
    },
    MigrationTransfer {
        key: String,
        expected_row_version: i64,
        version: u32,
    },
    MigrationRelease {
        key: String,
        expected_row_version: i64,
        version: u32,
    },
    RequestReview {
        key: String,
        expected_row_version: i64,
        source_commit: String,
        query: String,
        launch: ManagerLaunchChoiceV2,
        #[serde(default)]
        delta_of: Option<Uuid>,
        #[serde(default)]
        finding_keys: Vec<String>,
    },
    Accept {
        key: String,
        expected_row_version: i64,
    },
    Integration {
        key: String,
        expected_row_version: i64,
        source_commit: String,
        target_commit: String,
        verification: Option<ManagerEvidenceV2>,
    },
    Request {
        request_id: Uuid,
        expected_row_version: i64,
        state: ManagerRequestStateV2,
        message: String,
        work_key: Option<String>,
    },
    Decision {
        key: String,
        expected_row_version: i64,
        epic_id: Uuid,
        question: String,
        request_id: Option<Uuid>,
        work_key: Option<String>,
        /// #1415: the real gate this question belongs to, if any. A gated
        /// record is answered by the operator alone. Omitted: a non-gate
        /// question a delegated manager may settle (the daemon still refuses a
        /// manager ruling on a reserved key or a question that names a gate).
        #[serde(default)]
        gate: Option<ManagerDecisionGateV2>,
        /// #1415: the choices on offer, with the asker's recommendation.
        #[serde(default)]
        options: Vec<ManagerDecisionOptionV2>,
    },
    /// #1415: a delegated manager's audited ruling on a non-gate decision
    /// record: the owning manager, or any ancestor portfolio seat covering the
    /// project (`owner_manager_session_id` names the owning ledger as the
    /// `rulings` inspection rows show it; omitted is the caller's own ledger).
    /// A gate record refuses it (`manager_v2_decision_operator_gate`).
    DecisionRuling {
        key: String,
        expected_row_version: i64,
        target_digest: String,
        answer: String,
        #[serde(default)]
        owner_manager_session_id: Option<Uuid>,
    },
    /// #1415: the owning manager withdraws its own pending decision record.
    DecisionWithdraw {
        key: String,
        expected_row_version: i64,
        reason: String,
    },
    Handoff {
        summary: String,
        next_actions: Vec<String>,
    },
}

fn migration_source_sha(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentManagerUpdateRequestV2 {
    pub fence: ManagerFenceV2,
    pub idempotency_key: String,
    pub change: ManagerUpdateV2,
    /// #1235: the target project of a global manager seat acting inside its
    /// operator grant. Omitted means the caller's own project. A target the
    /// daemon checks against the grant, never caller identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
}

impl AgentManagerUpdateRequestV2 {
    pub fn validate(&self) -> Result<(), &'static str> {
        self.fence.validate()?;
        text(&self.idempotency_key, 128)?;
        match &self.change {
            ManagerUpdateV2::Handoff {
                summary,
                next_actions,
            } => {
                text(summary, 8192)?;
                if next_actions.len() > 64
                    || next_actions
                        .iter()
                        .any(|action| text(action, 8192).is_err())
                {
                    return Err("manager_v2_invalid_update");
                }
            }
            ManagerUpdateV2::Work {
                key,
                epic_id,
                title,
                required_gates,
                ..
            } => {
                if epic_id.is_nil() || required_gates.len() > 8 {
                    return Err("manager_v2_invalid_update");
                }
                text(key, 256)?;
                text(title, 512)?;
            }
            ManagerUpdateV2::Stage {
                key,
                note,
                evidence,
                job_evidence,
                ..
            } => {
                text(key, 256)?;
                text(note, 8192)?;
                if let Some(evidence) = evidence
                    && evidence.source_session_id.is_nil()
                {
                    return Err("manager_v2_invalid_update");
                }
                if let Some(job) = job_evidence {
                    if evidence.is_some() {
                        return Err("manager_v2_job_evidence_conflict");
                    }
                    if job.job_id.is_nil() {
                        return Err("manager_v2_invalid_update");
                    }
                }
            }
            ManagerUpdateV2::Dependency {
                key, prerequisite, ..
            } => {
                text(key, 256)?;
                text(prerequisite, 256)?;
            }
            ManagerUpdateV2::Ownership {
                key, domain, files, ..
            } => {
                if files.len() > 256 {
                    return Err("manager_v2_invalid_update");
                }
                text(key, 256)?;
                text(domain, 256)?;
            }
            ManagerUpdateV2::Migration {
                key,
                baseline_commit,
                inventory_digest,
                ..
            } => {
                text(key, 256)?;
                text(baseline_commit, 128)?;
                text(inventory_digest, 128)?;
            }
            ManagerUpdateV2::MigrationSeal {
                key, source_commit, ..
            } => {
                text(key, 256)?;
                if source_commit.len() != 40
                    || !source_commit
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                {
                    return Err("manager_v2_invalid_source_commit");
                }
            }
            ManagerUpdateV2::MigrationSealRelease {
                key,
                claim_id,
                source_commit,
                ..
            } => {
                text(key, 256)?;
                if claim_id.is_nil() || !migration_source_sha(source_commit) {
                    return Err("manager_v2_invalid_migration_claim");
                }
            }
            ManagerUpdateV2::MigrationSealTransfer {
                key,
                claim_id,
                source_commit,
                new_source_commit,
                ..
            } => {
                text(key, 256)?;
                if claim_id.is_nil()
                    || !migration_source_sha(source_commit)
                    || !migration_source_sha(new_source_commit)
                {
                    return Err("manager_v2_invalid_migration_claim");
                }
            }
            ManagerUpdateV2::MigrationTransfer { key, .. }
            | ManagerUpdateV2::MigrationRelease { key, .. } => text(key, 256)?,
            ManagerUpdateV2::RequestReview {
                key,
                source_commit,
                query,
                launch,
                delta_of,
                finding_keys,
                ..
            } => {
                text(key, 256)?;
                if source_commit.len() != 40
                    || !source_commit
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                {
                    return Err("manager_review_invalid_source");
                }
                text(query, 32_768)?;
                launch.validate()?;
                if delta_of.is_some() != !finding_keys.is_empty()
                    || finding_keys.len() > MANAGER_REVIEW_MAX_FINDINGS
                    || finding_keys.iter().any(|key| {
                        key.is_empty()
                            || key.len() > 64
                            || !key.bytes().all(|byte| {
                                byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
                            })
                    })
                    || finding_keys
                        .iter()
                        .enumerate()
                        .any(|(i, key)| finding_keys[..i].contains(key))
                {
                    return Err("manager_review_invalid_delta");
                }
            }
            ManagerUpdateV2::Accept { key, .. } => text(key, 256)?,
            ManagerUpdateV2::Integration {
                key,
                source_commit,
                target_commit,
                verification,
                ..
            } => {
                if verification
                    .as_ref()
                    .is_some_and(|verification| verification.source_session_id.is_nil())
                {
                    return Err("manager_v2_invalid_update");
                }
                text(key, 256)?;
                text(source_commit, 128)?;
                text(target_commit, 128)?;
            }
            ManagerUpdateV2::Request {
                request_id,
                message,
                work_key,
                ..
            } => {
                if request_id.is_nil() {
                    return Err("manager_v2_invalid_update");
                }
                text(message, 8192)?;
                if let Some(key) = work_key {
                    text(key, 256)?;
                }
            }
            ManagerUpdateV2::Decision {
                key,
                epic_id,
                question,
                work_key,
                options,
                ..
            } => {
                if epic_id.is_nil() {
                    return Err("manager_v2_invalid_update");
                }
                text(key, 256)?;
                text(question, 8192)?;
                if let Some(key) = work_key {
                    text(key, 256)?;
                }
                if options.len() > MANAGER_DECISION_MAX_OPTIONS
                    || options.iter().filter(|option| option.recommended).count() > 1
                {
                    return Err("manager_v2_invalid_update");
                }
                for option in options {
                    text(&option.label, 256)?;
                    if let Some(detail) = &option.detail {
                        text(detail, 2048)?;
                    }
                }
            }
            ManagerUpdateV2::DecisionRuling {
                key,
                expected_row_version,
                target_digest,
                answer,
                owner_manager_session_id,
            } => {
                if *expected_row_version <= 0
                    || owner_manager_session_id.is_some_and(|id| id.is_nil())
                {
                    return Err("manager_v2_invalid_update");
                }
                text(key, 256)?;
                text(target_digest, 128)?;
                text(answer, 16384)?;
            }
            ManagerUpdateV2::DecisionWithdraw {
                key,
                expected_row_version,
                reason,
            } => {
                if *expected_row_version <= 0 {
                    return Err("manager_v2_invalid_update");
                }
                text(key, 256)?;
                text(reason, 2048)?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagerMutationReceiptV2 {
    pub event_sequence: i64,
    pub key: String,
    pub row_version: i64,
    pub deduplicated: bool,
}

/// Only an operator can answer. The exact target identity/version and content
/// digest are observed in the decision inbox; stale answers never clear a gate.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnswerHarnessManagerDecisionRequestV2 {
    pub project_id: Uuid,
    pub fence: ManagerFenceV2,
    pub decision_key: String,
    pub expected_row_version: i64,
    pub target_digest: String,
    pub answer: String,
    pub idempotency_key: String,
}

/// #1415: the options one decision record may offer.
pub const MANAGER_DECISION_MAX_OPTIONS: usize = 8;
/// #1415: a pending decision record older than this many days is stale unless
/// the operator's request names another bound.
pub const MANAGER_DECISION_STALE_DAYS_DEFAULT: u32 = 7;
/// #1415: the longest stale bound a request may name.
pub const MANAGER_DECISION_STALE_DAYS_MAX: u32 = 3650;
/// #1415: stale decision rows one list or archive request may carry.
pub const MANAGER_DECISION_STALE_PAGE: usize = 128;

/// #1415: a real gate. A decision record in one of these classes is answered by
/// the operator alone: `main` or a release, real money or spend, credentials,
/// deleting user data, or a human approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagerDecisionGateV2 {
    MainOrRelease,
    Spend,
    Credentials,
    DataDeletion,
    HumanApproval,
}

impl ManagerDecisionGateV2 {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MainOrRelease => "main_or_release",
            Self::Spend => "spend",
            Self::Credentials => "credentials",
            Self::DataDeletion => "data_deletion",
            Self::HumanApproval => "human_approval",
        }
    }
}

/// #1415: one choice a decision offers, with the asker's recommendation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagerDecisionOptionV2 {
    pub label: String,
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default)]
    pub recommended: bool,
}

/// #1415 (operator-only): the project's pending decision records that are
/// stale: older than `older_than_days`, or owned by a manager that is gone.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListStaleManagerDecisionsRequestV2 {
    /// Omitted: every project.
    #[serde(default)]
    pub project_id: Option<Uuid>,
    /// Omitted: [`MANAGER_DECISION_STALE_DAYS_DEFAULT`].
    #[serde(default)]
    pub older_than_days: Option<u32>,
    /// Omitted: [`MANAGER_DECISION_STALE_PAGE`].
    #[serde(default)]
    pub limit: Option<u16>,
}

impl ListStaleManagerDecisionsRequestV2 {
    pub fn validate(&self) -> Result<(), &'static str> {
        validate_stale_days(self.older_than_days)?;
        if self.project_id.is_some_and(|id| id.is_nil())
            || self
                .limit
                .is_some_and(|n| n == 0 || usize::from(n) > MANAGER_DECISION_STALE_PAGE)
        {
            return Err("manager_v2_invalid_request");
        }
        Ok(())
    }
}

fn validate_stale_days(days: Option<u32>) -> Result<(), &'static str> {
    if days.is_some_and(|n| n == 0 || n > MANAGER_DECISION_STALE_DAYS_MAX) {
        return Err("manager_v2_invalid_request");
    }
    Ok(())
}

/// #1415: the exact identity and version of one decision record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagerDecisionRefV2 {
    pub project_id: Uuid,
    /// The owning manager's ledger principal.
    pub owner_manager_session_id: Uuid,
    pub scope_version: i64,
    pub key: String,
    pub expected_row_version: i64,
}

/// #1415: one stale record a list returns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StaleManagerDecisionV2 {
    /// The exact identity to hand back to the archive request.
    pub decision: ManagerDecisionRefV2,
    pub epic_id: Option<Uuid>,
    pub question: String,
    pub status: String,
    /// `operator` or `manager`.
    pub answerable_by: String,
    pub gate: Option<String>,
    pub created_at: String,
    pub age_seconds: i64,
    /// `older_than_days` or `manager_gone`.
    pub stale_reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListStaleManagerDecisionsResponseV2 {
    pub older_than_days: u32,
    pub rows: Vec<StaleManagerDecisionV2>,
    /// False when more stale records exist than `rows` carries.
    pub complete: bool,
}

/// #1415 (operator-only): archive stale decision records (never delete). Each
/// item is fenced by its row version and re-checked stale at archive time.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveStaleManagerDecisionsRequestV2 {
    pub items: Vec<ManagerDecisionRefV2>,
    /// Omitted: [`MANAGER_DECISION_STALE_DAYS_DEFAULT`]; the bound the items
    /// were listed with.
    #[serde(default)]
    pub older_than_days: Option<u32>,
}

impl ArchiveStaleManagerDecisionsRequestV2 {
    pub fn validate(&self) -> Result<(), &'static str> {
        validate_stale_days(self.older_than_days)?;
        if self.items.is_empty() || self.items.len() > MANAGER_DECISION_STALE_PAGE {
            return Err("manager_v2_invalid_request");
        }
        for item in &self.items {
            if item.project_id.is_nil()
                || item.owner_manager_session_id.is_nil()
                || item.scope_version <= 0
                || item.expected_row_version <= 0
            {
                return Err("manager_v2_invalid_request");
            }
            text(&item.key, 256)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkippedManagerDecisionV2 {
    pub decision: ManagerDecisionRefV2,
    /// `missing`, `changed`, `already_archived`, `not_stale` or `not_open`.
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchiveStaleManagerDecisionsResponseV2 {
    pub archived: Vec<ManagerDecisionRefV2>,
    pub skipped: Vec<SkippedManagerDecisionV2>,
}

pub fn text(value: &str, max: usize) -> Result<(), &'static str> {
    if value.trim().is_empty() || value.len() > max || value.contains('\0') {
        return Err("manager_v2_invalid_text");
    }
    Ok(())
}

/// Full lowercase SHA-1 (40 hex) or SHA-256 (64 hex) object name.
#[must_use]
pub fn canonical_git_oid(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub fn unique_ids(ids: &[Uuid], max: usize) -> Result<(), &'static str> {
    if ids.len() > max
        || ids.iter().any(Uuid::is_nil)
        || ids.iter().enumerate().any(|(i, id)| ids[..i].contains(id))
    {
        return Err("manager_v2_invalid_scope");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    /// #2570: only a receipt whose blocking findings are all infrastructure
    /// outcomes (and that names at least one) is an infrastructure outcome.
    #[test]
    fn infrastructure_only_receipts_are_typed_by_finding_key() {
        assert!(review_receipt_is_infrastructure_only([(
            "infra_tests_unavailable",
            true
        )]));
        assert!(review_receipt_is_infrastructure_only([
            ("required_tests_unrun", true),
            ("naming", false),
        ]));
        assert!(!review_receipt_is_infrastructure_only([
            ("infra_tests_unavailable", true),
            ("missing-test", true),
        ]));
        assert!(!review_receipt_is_infrastructure_only([(
            "missing-test",
            true
        )]));
        assert!(!review_receipt_is_infrastructure_only([]));
        assert!(!review_receipt_is_infrastructure_only([("naming", false)]));
    }

    use super::*;
    use serde_json::json;

    #[test]
    fn launch_choice_any_effort_matches_only_its_provider_and_model() {
        let any = ManagerLaunchChoiceV2 {
            provider: crate::SessionProvider::Codex,
            model: "gpt-6-astra".into(),
            effort: None,
        };
        assert!(any.allows(&any));
        for effort in crate::model_utils::effort_ladder(&any.model) {
            let mut exact = any.clone();
            exact.effort = Some((*effort).into());
            assert!(any.allows(&exact), "{effort}");
            assert!(exact.allows(&exact));
            assert!(!exact.allows(&any));
            let mut other = exact.clone();
            other.effort = Some("another-effort".into());
            assert!(!exact.allows(&other));
        }
        let mut other = any.clone();
        other.model = "gpt-6-luna".into();
        assert!(!any.allows(&other));
        other = any.clone();
        other.provider = crate::SessionProvider::Claude;
        assert!(!any.allows(&other));
        assert_eq!(any.display(), "Codex/gpt-6-astra/any effort");
    }

    #[test]
    fn launch_choice_any_effort_admits_only_efforts_on_a_known_model_ladder() {
        let choice = |provider, model: &str, effort: Option<&str>| ManagerLaunchChoiceV2 {
            provider,
            model: model.into(),
            effort: effort.map(Into::into),
        };
        let provider = crate::SessionProvider::Codex;
        let any = choice(provider, "gpt-6-astra", None);
        let ladder = crate::model_utils::known_effort_ladder("gpt-6-astra")
            .expect("gpt-6-astra has a known ladder");
        assert!(!ladder.is_empty());
        for effort in ladder {
            assert!(
                any.allows(&choice(provider, "gpt-6-astra", Some(effort))),
                "{effort}"
            );
        }
        // An off-ladder effort is not admitted by an any-effort entry, but an
        // exact entry still names exactly what the operator wrote.
        let off_ladder = choice(provider, "gpt-6-astra", Some("ungranted-effort"));
        assert!(!any.allows(&off_ladder));
        assert!(off_ladder.allows(&off_ladder));
        assert!(!off_ladder.allows(&any));

        // A model RSI has no ladder for keeps admitting any effort string.
        let claude = crate::SessionProvider::Claude;
        let unknown = choice(claude, "manager-scripted-provider", None);
        assert!(crate::model_utils::known_effort_ladder(&unknown.model).is_none());
        assert!(unknown.allows(&choice(
            claude,
            "manager-scripted-provider",
            Some("ungranted-effort")
        )));
        assert!(unknown.allows(&choice(claude, "manager-scripted-provider", None)));
    }

    #[test]
    fn manager_policy_and_area_grant_accept_64_launches_and_refuse_65() {
        let mut policy = ManagerPolicyV2::default();
        policy.allowed_launches = (0..64)
            .map(|i| ManagerLaunchChoiceV2 {
                provider: crate::SessionProvider::Claude,
                model: format!("model-{i}"),
                effort: None,
            })
            .collect();
        let mut area = crate::manager_nodes::ManagerNodeGrantV1 {
            capabilities: vec![],
            allowed_launches: policy.allowed_launches.clone(),
            allowance: crate::manager_nodes::ManagerNodeAllowanceV1 {
                max_created_containers: 0,
                max_created_sessions: 0,
                max_active_sessions: 1,
                max_build_slots: 0,
                max_disk_gib: 0,
                provider_limits: vec![],
                max_spend_usd: None,
            },
            max_direct_reports: crate::manager_nodes::MANAGER_NODE_DEFAULT_MAX_DIRECT_REPORTS,
        };
        assert_eq!(policy.validate(), Ok(()));
        assert_eq!(area.validate(), Ok(()));
        let extra = ManagerLaunchChoiceV2 {
            provider: crate::SessionProvider::Claude,
            model: "model-64".into(),
            effort: None,
        };
        policy.allowed_launches.push(extra.clone());
        area.allowed_launches.push(extra);
        assert_eq!(policy.validate(), Err("manager_v2_invalid_policy"));
        assert_eq!(area.validate(), Err("manager_node_invalid_grant"));
    }

    #[test]
    fn inspection_serializes_any_effort_display_and_round_trips() {
        let inspection = ManagerInspectionV2 {
            observed_at: Utc::now(),
            scope_version: 1,
            policy: Some(HarnessManagerPolicyConfigV2 {
                project_id: Uuid::new_v4(),
                manager_session_id: Uuid::new_v4(),
                scope_version: 1,
                row_version: 1,
                policy: ManagerPolicyV2 {
                    allowed_launches: vec![ManagerLaunchChoiceV2 {
                        provider: crate::SessionProvider::Claude,
                        model: "claude-opus-5-5".into(),
                        effort: None,
                    }],
                    ..Default::default()
                },
                updated_at: Utc::now(),
                revoked: false,
            }),
            section: ManagerInspectSectionV2::Overview,
            rows: vec![],
            next_cursor: None,
            complete: true,
        };
        let value = serde_json::to_value(&inspection).unwrap();
        assert_eq!(
            value["launches_display"][0],
            "Claude/claude-opus-5-5/any effort"
        );
        assert_eq!(
            value["policy"]["policy"]["allowed_launches"][0]["effort"],
            json!(null)
        );
        let restored: ManagerInspectionV2 = serde_json::from_value(value).unwrap();
        assert_eq!(
            restored.policy.unwrap().policy,
            inspection.policy.unwrap().policy
        );
    }

    #[test]
    fn tier2_review_policy_and_work_metadata_default_and_round_trip() {
        let default: ManagerPolicyV2 = serde_json::from_value(json!({})).unwrap();
        assert_eq!(default.minimum_tier2_reviewer_tier, ModelTier::Standard);
        let mut strict = default;
        strict.minimum_tier2_reviewer_tier = ModelTier::Premium;
        let restored: ManagerPolicyV2 =
            serde_json::from_value(serde_json::to_value(strict).unwrap()).unwrap();
        assert_eq!(restored.minimum_tier2_reviewer_tier, ModelTier::Premium);
        let work: ManagerUpdateV2 = serde_json::from_value(json!({
            "update":"work", "key":"authority", "expected_row_version":0,
            "epic_id":Uuid::new_v4(), "title":"Authority change", "kind":"program",
            "priority":1, "weight":1, "required_gates":["implementation"]
        }))
        .unwrap();
        assert!(matches!(
            work,
            ManagerUpdateV2::Work {
                risk_tier: ManagerWorkRiskTierV2::Tier1,
                ..
            }
        ));
    }

    #[test]
    fn manager_v2_default_grant_preserves_status_only_and_rejects_unknown_fields() {
        let policy: ManagerPolicyV2 = serde_json::from_value(json!({})).unwrap();
        assert_eq!(policy.mode, ManagerOperatingModeV2::Status);
        assert!(policy.capabilities.is_empty());
        assert_eq!(policy.max_created_sessions, 0);
        assert_eq!(policy.max_recovery_attempts, 0);
        assert!(policy.validate().is_ok());
        assert!(serde_json::from_value::<ManagerPolicyV2>(json!({"admin":true})).is_err());
    }

    #[test]
    fn manager_v2_policy_rejects_unbounded_or_ungranted_creation() {
        let mut policy = ManagerPolicyV2 {
            allow_create_groups: true,
            ..Default::default()
        };
        assert!(policy.validate().is_err());
        policy.capabilities.push(ManagerCapabilityV2::Topology);
        assert!(policy.validate().is_ok());
        policy.max_recovery_attempts = 33;
        assert!(policy.validate().is_err());
        policy.max_recovery_attempts = 1;
        policy.max_spend_usd = Some(f64::NAN);
        assert!(policy.validate().is_err());
    }

    #[test]
    fn manager_v2_policy_allows_created_session_quota_up_to_1024() {
        let mut policy = ManagerPolicyV2 {
            max_created_sessions: 1024,
            ..Default::default()
        };
        assert!(policy.validate().is_ok());
        policy.max_created_sessions = 1025;
        assert_eq!(policy.validate(), Err("manager_v2_invalid_policy"));
    }

    #[test]
    fn manager_v2_policy_allows_operator_requested_concurrency_bound() {
        let mut policy = ManagerPolicyV2 {
            capabilities: vec![ManagerCapabilityV2::Topology],
            max_active_sessions: 100,
            ..Default::default()
        };
        assert!(policy.validate().is_ok());
        policy.max_active_sessions = 101;
        assert_eq!(policy.validate(), Err("manager_v2_invalid_policy"));
        policy.max_active_sessions = 100;
        policy.max_spend_usd = Some(1.0);
        assert!(policy.validate().is_ok());
    }

    #[test]
    fn manager_v2_action_rejects_forged_actor_and_arbitrary_action() {
        assert!(serde_json::from_value::<AgentManagerControlRequestV2>(json!({
            "fence":{"scope_version":1,"policy_version":1},"idempotency_key":"a",
            "operation":{"action":"pause_lead","epic_id":Uuid::new_v4(),
                "expected":{"lead_session_id":null,"lead_generation":0,"event_sequence":0,"custody_generation":null},"reason":"pause"},
            "caller_session_id":Uuid::new_v4()
        })).is_err());
        assert!(
            serde_json::from_value::<ManagerActionV2>(
                json!({"action":"execute_shell","command":"anything"})
            )
            .is_err()
        );
    }

    #[test]
    fn manager_action_receipt_adds_typed_lifecycle_semantics_compatibly() {
        let operation_id = Uuid::new_v4();
        let target_session_id = Uuid::new_v4();
        let legacy = json!({
            "operation_id": operation_id,
            "state": "succeeded",
            "row_version": 3,
            "target_session_id": target_session_id,
            "outcome": "container_committed",
            "deduplicated": true
        });
        let mut receipt: ManagerActionReceiptV2 = serde_json::from_value(legacy.clone()).unwrap();
        assert_eq!(receipt.action_kind, ManagerActionKindV2::Unknown);
        assert_eq!(receipt.target_type, ManagerActionTargetTypeV2::Unknown);
        assert_eq!(receipt.result, None);

        receipt.refresh_action_metadata(&ManagerActionV2::CreateContainer {
            parent_id: Some(Uuid::new_v4()),
            kind: SessionKind::Epic,
            name: "Lifecycle clarity".into(),
            tags: vec!["manager".into()],
        });
        let current = serde_json::to_value(&receipt).unwrap();
        for field in [
            "operation_id",
            "state",
            "row_version",
            "target_session_id",
            "outcome",
            "deduplicated",
        ] {
            assert_eq!(current[field], legacy[field], "legacy field {field}");
        }
        assert_eq!(current["action_kind"], "create_container");
        assert_eq!(current["target_type"], "epic_container");
        assert_eq!(current["result"]["target_state"], "container_committed");
        assert_eq!(current["result"]["lead_state"], "unassigned");
    }

    /// #2652: the cancelled-wakes field is optional both ways. A receipt without
    /// it reads and writes exactly as before, one with it round-trips its ids and
    /// reason, and a metadata refresh never drops it.
    #[test]
    fn manager_action_receipt_names_cancelled_wakes_compatibly() {
        let operation_id = Uuid::new_v4();
        let legacy = json!({
            "operation_id": operation_id,
            "state": "succeeded",
            "row_version": 4,
            "target_session_id": null,
            "outcome": "manager_successor_established",
            "deduplicated": false
        });
        let plain: ManagerActionReceiptV2 = serde_json::from_value(legacy.clone()).unwrap();
        assert_eq!(plain.cancelled_wakes, None);
        assert!(
            serde_json::to_value(&plain)
                .unwrap()
                .get("cancelled_wakes")
                .is_none(),
            "an absent field is omitted, not written as null"
        );

        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let mut named = plain.clone();
        named.cancelled_wakes = Some(ManagerCancelledWakesV1 {
            reason: "manager_succession_displaced_seat".into(),
            job_ids: vec![a, b],
        });
        let wire = serde_json::to_value(&named).unwrap();
        assert_eq!(
            wire["cancelled_wakes"],
            json!({"reason":"manager_succession_displaced_seat","job_ids":[a,b]})
        );
        let mut back: ManagerActionReceiptV2 = serde_json::from_value(wire).unwrap();
        assert_eq!(back, named);

        let request: AgentManagerControlRequestV2 =
            serde_json::from_value(root_succession_request()).unwrap();
        back.refresh_action_metadata(&request.operation);
        assert_eq!(back.result, Some(ManagerActionResultV2::ManagerPublished));
        assert_eq!(
            back.cancelled_wakes, named.cancelled_wakes,
            "a refresh keeps the cancelled ids"
        );
    }

    fn root_succession_request() -> serde_json::Value {
        json!({
            "fence":{"scope_version":9,"policy_version":15},
            "idempotency_key":"manager-handoff-1",
            "operation":{
                "action":"succeed_manager",
                "expected":{"authority_epoch":3,"custody_generation":1},
                "launch":{"provider":"Codex","model":"gpt-6-astra","effort":"medium"},
                "handoff":{
                    "source_commit":"a".repeat(40),
                    "relative_path":"thoughts/shared/handoffs/manager.md",
                    "blob_oid":"b".repeat(40)
                }
            }
        })
    }

    #[test]
    fn manager_v2_session_and_issue_grants_roundtrip_with_stable_wire_names() {
        let policy: ManagerPolicyV2 = serde_json::from_value(json!({
            "mode":"execute",
            "capabilities":["work_plan","lead_control","topology","session_create","lead_assign",
                "integration","self_succession","git_effect","session_control","issue_coordinate"]
        }))
        .unwrap();
        assert_eq!(policy.validate(), Ok(()));
        assert!(
            policy
                .capabilities
                .contains(&ManagerCapabilityV2::SessionControl)
        );
        assert!(
            policy
                .capabilities
                .contains(&ManagerCapabilityV2::IssueCoordinate)
        );
        let wire = serde_json::to_value(&policy.capabilities).unwrap();
        assert_eq!(wire[8], json!("session_control"));
        assert_eq!(wire[9], json!("issue_coordinate"));
        let mut duplicate = policy;
        duplicate
            .capabilities
            .push(ManagerCapabilityV2::SessionControl);
        assert_eq!(duplicate.validate(), Err("manager_v2_invalid_policy"));
    }

    /// #633: `Automation` is additive; older stored policies (without it)
    /// still decode, and the new grant has a stable wire name.
    #[test]
    fn manager_v2_automation_grant_is_additive_with_stable_wire_name() {
        let legacy: ManagerPolicyV2 =
            serde_json::from_value(json!({"mode":"execute","capabilities":["work_plan"]})).unwrap();
        assert_eq!(legacy.capabilities, vec![ManagerCapabilityV2::WorkPlan]);
        let policy: ManagerPolicyV2 = serde_json::from_value(json!({
            "mode":"execute",
            "capabilities":["topology","git_effect","automation"]
        }))
        .unwrap();
        assert_eq!(policy.validate(), Ok(()));
        assert!(
            policy
                .capabilities
                .contains(&ManagerCapabilityV2::Automation)
        );
        assert_eq!(
            serde_json::to_value(ManagerCapabilityV2::Automation).unwrap(),
            json!("automation")
        );
        // #1045: `Deploy` is an ordinary, valid grant under its snake_case name.
        let deploy: ManagerPolicyV2 =
            serde_json::from_value(json!({"mode":"execute","capabilities":["deploy"]})).unwrap();
        assert_eq!(deploy.validate(), Ok(()));
        assert_eq!(
            serde_json::to_value(ManagerCapabilityV2::Deploy).unwrap(),
            json!("deploy")
        );
    }

    #[test]
    fn manager_v2_root_succession_roundtrips_and_requires_its_explicit_grant() {
        let value = root_succession_request();
        let request: AgentManagerControlRequestV2 = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(
            request.operation.capability(),
            ManagerCapabilityV2::SelfSuccession
        );
        assert_eq!(serde_json::to_value(&request).unwrap(), value);
        let mut legacy: ManagerPolicyV2 = serde_json::from_value(json!({
            "mode":"execute",
            "capabilities":["work_plan","lead_control","topology","session_create","lead_assign","integration"]
        })).unwrap();
        assert_eq!(legacy.capabilities.len(), 6);
        assert_eq!(legacy.max_recovery_attempts, 0);
        assert!(legacy.validate().is_ok());
        assert!(
            !legacy
                .capabilities
                .contains(&request.operation.capability())
        );
        legacy
            .capabilities
            .push(ManagerCapabilityV2::SelfSuccession);
        assert!(legacy.validate().is_ok());
        legacy
            .capabilities
            .push(ManagerCapabilityV2::SelfSuccession);
        assert_eq!(legacy.validate(), Err("manager_v2_invalid_policy"));
    }

    #[test]
    fn manager_v2_root_succession_rejects_identity_injection_at_every_nested_boundary() {
        for pointer in [
            "",
            "/operation",
            "/operation/expected",
            "/operation/launch",
            "/operation/handoff",
        ] {
            for key in [
                "caller_session_id",
                "project_id",
                "predecessor_id",
                "candidate_id",
                "sandbox_root",
                "token",
            ] {
                // #1235: a top-level project_id is the global seat's target.
                if pointer.is_empty() && key == "project_id" {
                    continue;
                }
                let mut value = root_succession_request();
                value
                    .pointer_mut(pointer)
                    .unwrap()
                    .as_object_mut()
                    .unwrap()
                    .insert(key.into(), json!(Uuid::new_v4()));
                assert!(
                    serde_json::from_value::<AgentManagerControlRequestV2>(value).is_err(),
                    "accepted injected {key} at {pointer}"
                );
            }
        }
    }

    #[test]
    fn manager_v2_succession_fence_requires_positive_observed_generations() {
        for (epoch, custody, valid) in [
            (1, None, true),
            (3, Some(1), true),
            (0, None, false),
            (-1, Some(1), false),
            (1, Some(0), false),
            (1, Some(-1), false),
        ] {
            let fence = ManagerSuccessionFenceV2 {
                authority_epoch: epoch,
                custody_generation: custody,
            };
            assert_eq!(fence.validate().is_ok(), valid);
        }
    }

    #[test]
    fn manager_v2_succession_preflight_has_closed_eligible_and_actionable_denial_shapes() {
        let logical = Uuid::new_v4();
        let current = Uuid::new_v4();
        let eligible = ManagerSuccessionPreflightV2::Eligible {
            observation: ManagerSuccessionObservationV2 {
                logical_manager_session_id: logical,
                current_session_id: current,
                expected: ManagerSuccessionFenceV2 {
                    authority_epoch: 7,
                    custody_generation: Some(3),
                },
                unresolved_operation_id: None,
            },
        };
        let eligible_value = json!({
            "eligibility":"eligible",
            "logical_manager_session_id":logical,
            "current_session_id":current,
            "expected":{"authority_epoch":7,"custody_generation":3},
            "unresolved_operation_id":null
        });
        assert_eq!(serde_json::to_value(&eligible).unwrap(), eligible_value);
        assert_eq!(
            serde_json::from_value::<ManagerSuccessionPreflightV2>(eligible_value).unwrap(),
            eligible
        );

        let ineligible = ManagerSuccessionPreflightV2::Ineligible {
            logical_manager_session_id: logical,
            current_session_id: Some(current),
            denial: ManagerSuccessionDenialV2 {
                reason: ManagerSuccessionDenialReasonV2::CurrentManagerNotParentlessStandard,
                required_action:
                    ManagerSuccessionRequiredActionV2::AppointParentlessStandardManager,
            },
        };
        let ineligible_value = json!({
            "eligibility":"ineligible",
            "logical_manager_session_id":logical,
            "current_session_id":current,
            "denial":{
                "reason":"current_manager_not_parentless_standard",
                "required_action":"appoint_parentless_standard_manager"
            }
        });
        assert_eq!(serde_json::to_value(&ineligible).unwrap(), ineligible_value);
        assert_eq!(
            serde_json::from_value::<ManagerSuccessionPreflightV2>(ineligible_value.clone())
                .unwrap(),
            ineligible
        );
        let disabled = ManagerSuccessionPreflightV2::Ineligible {
            logical_manager_session_id: logical,
            current_session_id: Some(current),
            denial: ManagerSuccessionDenialV2 {
                reason: ManagerSuccessionDenialReasonV2::ContextRotationDisabled,
                required_action: ManagerSuccessionRequiredActionV2::OperatorEnableContextRotation,
            },
        };
        let disabled_value = serde_json::to_value(&disabled).unwrap();
        assert_eq!(
            disabled_value["denial"]["reason"],
            "context_rotation_disabled"
        );
        assert_eq!(
            disabled_value["denial"]["required_action"],
            "operator_enable_context_rotation"
        );
        assert_eq!(
            serde_json::from_value::<ManagerSuccessionPreflightV2>(disabled_value).unwrap(),
            disabled
        );
        let mut injected = ineligible_value;
        injected["expected"] = json!({"authority_epoch":7,"custody_generation":3});
        assert!(serde_json::from_value::<ManagerSuccessionPreflightV2>(injected).is_err());
    }

    #[test]
    fn manager_v2_committed_handoff_requires_full_object_ids_and_relative_tree_path() {
        let handoff = ManagerCommittedHandoffV2 {
            source_commit: "a".repeat(40),
            relative_path: "thoughts/shared/handoffs/manager.md".into(),
            blob_oid: "b".repeat(40),
        };
        assert!(handoff.validate().is_ok());
        assert!(
            ManagerCommittedHandoffV2 {
                source_commit: "c".repeat(64),
                blob_oid: "d".repeat(64),
                ..handoff.clone()
            }
            .validate()
            .is_ok()
        );
        for path in [
            "",
            "/manager.md",
            "./manager.md",
            "../manager.md",
            "docs/../manager.md",
            "docs//manager.md",
            ".git/config",
            "docs/.git/config",
            "docs/manager.md/",
            "docs\\manager.md",
            "docs/\0manager.md",
        ] {
            assert!(
                ManagerCommittedHandoffV2 {
                    relative_path: path.into(),
                    ..handoff.clone()
                }
                .validate()
                .is_err(),
                "accepted {path:?}"
            );
        }
        assert!(
            ManagerCommittedHandoffV2 {
                relative_path: "x".repeat(1025),
                ..handoff.clone()
            }
            .validate()
            .is_err()
        );
        assert!(
            ManagerCommittedHandoffV2 {
                relative_path: "é".repeat(513),
                ..handoff.clone()
            }
            .validate()
            .is_err()
        );
        for oid in [
            "abc".into(),
            "A".repeat(40),
            "g".repeat(40),
            "1".repeat(41),
            "1".repeat(63),
        ] {
            assert!(
                ManagerCommittedHandoffV2 {
                    source_commit: oid.clone(),
                    ..handoff.clone()
                }
                .validate()
                .is_err()
            );
            assert!(
                ManagerCommittedHandoffV2 {
                    blob_oid: oid,
                    ..handoff.clone()
                }
                .validate()
                .is_err()
            );
        }
    }

    #[test]
    fn prepared_manager_actions_are_semantic_only_and_strict() {
        let epic = Uuid::new_v4();
        let launch = json!({"provider":"Codex","model":"gpt-5","effort":"high"});
        let fixtures = [
            json!({"action":"resume_lead","epic_id":epic,"message":"continue"}),
            json!({"action":"pause_lead","epic_id":epic,"reason":"operator review"}),
            json!({"action":"retry_lead","epic_id":epic,"message":"retry","launch":null}),
            json!({"action":"replace_lead","epic_id":epic,"query":"replace","launch":launch}),
            json!({"action":"create_session","parent_id":epic,"kind":"Task","query":"implement","launch":launch}),
            json!({"action":"assign_lead","epic_id":epic,"session_id":null}),
        ];
        for operation in fixtures {
            let request: AgentManagerPrepareControlRequestV2 =
                serde_json::from_value(json!({"operation":operation})).unwrap();
            assert!(request.validate().is_ok());
            let mut injected = serde_json::to_value(&request).unwrap();
            injected["operation"]["expected"] = json!({
                "lead_session_id":null,
                "lead_generation":1,
                "event_sequence":0,
                "custody_generation":null
            });
            assert!(
                serde_json::from_value::<AgentManagerPrepareControlRequestV2>(injected).is_err()
            );
        }
        for forbidden in ["create_container", "update_container", "succeed_manager"] {
            assert!(
                serde_json::from_value::<PreparedManagerActionV2>(json!({
                    "action": forbidden
                }))
                .is_err()
            );
        }
    }

    #[test]
    fn prepared_manager_commit_requires_exact_digest_and_key() {
        let valid = AgentManagerCommitPreparedControlRequestV2 {
            project_id: None,
            prepared_id: Uuid::new_v4(),
            target_digest: format!("sha256:{}", "a".repeat(64)),
            idempotency_key: "commit-prepared-v1".into(),
        };
        assert!(valid.validate().is_ok());
        for digest in [
            "sha256:short".into(),
            format!("sha256:{}", "A".repeat(64)),
            format!("sha257:{}", "a".repeat(64)),
        ] {
            assert!(
                AgentManagerCommitPreparedControlRequestV2 {
                    target_digest: digest,
                    ..valid.clone()
                }
                .validate()
                .is_err()
            );
        }
        assert!(
            AgentManagerGetActionRequestV2 {
                project_id: None,
                operation_id: Uuid::nil()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn review_receipts_are_bounded_and_reject_identity_injection() {
        let finding = ManagerReviewFindingV1 {
            key: "F-001".into(),
            severity: ManagerReviewFindingSeverityV1::Error,
            summary: "A bounded finding".into(),
            location: Some("crates/example.rs:1".into()),
            blocking: true,
        };
        let valid = AgentSubmitReviewReceiptRequestV1 {
            assignment_id: Uuid::new_v4(),
            verdict: ManagerReviewVerdictV1::ChangesRequested,
            findings: vec![finding.clone()],
            idempotency_key: "receipt-v1".into(),
        };
        assert!(valid.validate().is_ok());
        let mut injected = serde_json::to_value(&valid).unwrap();
        for field in [
            "reviewer_session_id",
            "reviewer_invocation_id",
            "reviewer_custody_id",
            "source_commit",
            "project_id",
        ] {
            injected[field] = json!(Uuid::new_v4());
            assert!(
                serde_json::from_value::<AgentSubmitReviewReceiptRequestV1>(injected.clone())
                    .is_err(),
                "accepted injected {field}"
            );
            injected.as_object_mut().unwrap().remove(field);
        }
        assert!(
            AgentSubmitReviewReceiptRequestV1 {
                findings: vec![finding.clone(); MANAGER_REVIEW_MAX_FINDINGS + 1],
                ..valid.clone()
            }
            .validate()
            .is_err()
        );
        assert!(
            AgentSubmitReviewReceiptRequestV1 {
                findings: vec![ManagerReviewFindingV1 {
                    summary: "x".repeat(2049),
                    ..finding.clone()
                }],
                ..valid.clone()
            }
            .validate()
            .is_err()
        );
        assert!(
            AgentSubmitReviewReceiptRequestV1 {
                findings: vec![finding.clone(), finding],
                ..valid
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn session_housekeeping_actions_are_session_control_and_validate_their_patch() {
        let id = Uuid::new_v4();
        let at = Utc::now();
        let patch = |patch: ManagerSessionPatchV2| ManagerActionV2::UpdateSession {
            session_id: id,
            expected_updated_at: at,
            patch,
        };
        for action in [
            ManagerActionV2::ArchiveSession {
                session_id: id,
                expected_updated_at: at,
            },
            ManagerActionV2::RestoreSession {
                session_id: id,
                expected_updated_at: at,
            },
            patch(ManagerSessionPatchV2 {
                rating: Some(ManagerFieldPatchV2::Set(10)),
                ..Default::default()
            }),
        ] {
            assert_eq!(action.capability(), ManagerCapabilityV2::SessionControl);
            assert_eq!(
                action.target_type(),
                ManagerActionTargetTypeV2::ProviderSession
            );
            assert_eq!(action.housekeeping_session(), Some(id));
            assert!(action.validate().is_ok());
        }
        for (bad, code) in [
            (
                ManagerSessionPatchV2::default(),
                "manager_v2_empty_session_patch",
            ),
            (
                ManagerSessionPatchV2 {
                    rating: Some(ManagerFieldPatchV2::Set(0)),
                    ..Default::default()
                },
                "manager_v2_invalid_rating",
            ),
            (
                ManagerSessionPatchV2 {
                    tags: Some(Vec::new()),
                    ..Default::default()
                },
                "manager_v2_invalid_tags",
            ),
            (
                ManagerSessionPatchV2 {
                    title: Some("  ".into()),
                    ..Default::default()
                },
                "manager_v2_invalid_text",
            ),
        ] {
            assert_eq!(patch(bad).validate(), Err(code));
        }
        let clear: ManagerSessionPatchV2 =
            serde_json::from_value(json!({"description":"clear","label":{"set":id}})).unwrap();
        assert_eq!(clear.description, Some(ManagerFieldPatchV2::Clear));
        assert_eq!(clear.label, Some(ManagerFieldPatchV2::Set(id)));
        assert!(
            serde_json::from_value::<ManagerSessionPatchV2>(json!({"rating":{"set":3},"x":1}))
                .is_err()
        );
    }
}

impl Serialize for ManagerInspectionV2 {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut output = serializer.serialize_struct("ManagerInspectionV2", 8)?;
        output.serialize_field("observed_at", &self.observed_at)?;
        output.serialize_field("scope_version", &self.scope_version)?;
        output.serialize_field("policy", &self.policy)?;
        output.serialize_field("section", &self.section)?;
        output.serialize_field("rows", &self.rows)?;
        output.serialize_field("next_cursor", &self.next_cursor)?;
        output.serialize_field("complete", &self.complete)?;
        let launches_display: Vec<_> = self
            .policy
            .as_ref()
            .map_or(&[][..], |grant| grant.policy.allowed_launches.as_slice())
            .iter()
            .map(crate::harness_manager_v2::ManagerLaunchChoiceV2::display)
            .collect();
        output.serialize_field("launches_display", &launches_display)?;
        output.end()
    }
}
