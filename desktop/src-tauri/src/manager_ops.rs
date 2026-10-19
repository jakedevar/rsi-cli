//! Manager commands and in-place edits (hub #3000 gap sweep G3), extending the
//! managers slice (src/managers.rs): `:manager global`, `:manager portfolio`,
//! `:manager escalations`, `:manager node`, `:manager restart`, the manager
//! tree's appoint / edit / appoint-above / move on global, portfolio and area
//! rows, the workspace's launch-and-appoint, node consoles and the policy
//! presets.
//!
//! Two kinds of command live here:
//! - **forwarders**: each validates its input against the TUI's typed request
//!   (`rsi_common::{global_manager, portfolio_nodes, manager_nodes, ...}`) and
//!   forwards exactly one operator daemon method, as `crates/rsi/src/client.rs`
//!   sends it;
//! - **planners** (no daemon): the pure request builders the TUI keeps in
//!   `crates/rsi/src/overlay/{global_manager_command,portfolio_command,
//!   manager_tree/actions}.rs`, over the snapshots the webview already read.
//!   The webview reads, plans, shows the review, then sends.
//!
//! These are operator RPCs (hub ruling #3015); nothing here touches the agent
//! verb lists. The daemon validates every request again and owns authority.

use std::collections::BTreeSet;

use rsi_common::global_manager::{ConfigureGlobalManagerRequestV1, GlobalManagerGrantV1};
use rsi_common::harness_manager::HarnessManagerScopeModeV1;
use rsi_common::harness_manager_presets::{
    ManagerPolicyOrigin, ManagerPolicyPreset, ManagerPresetContext, apply_manager_policy_preset,
    apply_suggested_manager_allowances, classify_manager_policy, suggested_manager_allowances,
};
use rsi_common::harness_manager_v2::{
    ConfigureHarnessManagerWithPolicyRequestV2, ManagerCapabilityV2, ManagerLaunchChoiceV2,
    ManagerPolicyV2,
};
use rsi_common::manager_nodes::{ConfigureManagerNodeRequestV1, ManagerNodeViewV1};
use rsi_common::portfolio_nodes::{
    ConfigurePortfolioNodeLaunchesRequestV1, ConfigurePortfolioNodeRequestV1,
    PORTFOLIO_DEFAULT_MAX_DIRECT_REPORTS, PORTFOLIO_MAX_DIRECT_REPORTS, PORTFOLIO_TIER_LABEL_MAX,
    PortfolioGrantPrefillV1, PortfolioNodeV1, prefill_from_revoked_root, valid_tier_label,
};
use rsi_common::types::SessionProvider;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tauri::State;
use uuid::Uuid;

use crate::{AppState, CmdResult, forward, parse_uuid};

type Built = Result<Value, String>;

pub const METHOD_CONFIGURE_GLOBAL: &str = "ConfigureGlobalManager";
pub const METHOD_CONFIGURE_PORTFOLIO: &str = "ConfigurePortfolioNode";
pub const METHOD_PORTFOLIO_LAUNCHES: &str = "ConfigurePortfolioNodeLaunches";
pub const METHOD_LIST_NODES: &str = "ListManagerNodes";
pub const METHOD_CONFIGURE_NODE: &str = "ConfigureManagerNode";
pub const METHOD_RESTART_STATUS: &str = "GetOperatorRestart";
pub const METHOD_RESTART_FORCE: &str = "ForceOperatorRestart";
pub const METHOD_RESTART_CANCEL: &str = "CancelOperatorRestart";
pub const METHOD_RULE: &str = "RuleOperatorEscalation";
pub const METHOD_ACK: &str = "AcknowledgeOperatorNotice";
pub const METHOD_ESCALATIONS: &str = "ListOperatorEscalations";
pub const METHOD_NODE_WORKSPACE: &str = "GetManagerNodeWorkspace";
pub const METHOD_WITH_POLICY: &str = "ConfigureHarnessManagerWithPolicy";

/// `ListManagerNodes` page (the TUI's `:manager node list`).
const NODE_PAGE: u16 = 64;
const MAX_RULING: usize = 16 * 1024;

fn typed<T: DeserializeOwned>(what: &str, v: &Value) -> Result<T, String> {
    serde_json::from_value(v.clone()).map_err(|e| format!("{what}: {e}"))
}

fn to_value<T: serde::Serialize>(v: &T) -> Built {
    serde_json::to_value(v).map_err(|e| e.to_string())
}

fn uuid(field: &str, raw: &str) -> Result<Uuid, String> {
    Uuid::parse_str(&parse_uuid(field, raw)?).map_err(|e| e.to_string())
}

fn uuids(field: &str, raw: &[String]) -> Result<Vec<Uuid>, String> {
    raw.iter().map(|r| uuid(field, r)).collect()
}

fn short(id: Uuid) -> String {
    id.to_string()[..8].to_string()
}

fn key() -> String {
    Uuid::new_v4().to_string()
}

/// `{...request, confirm_cap_reductions?}` (`PortfolioCapConfirmation`).
fn confirmed(mut request: Value, confirm: bool) -> Value {
    if confirm && let Some(o) = request.as_object_mut() {
        o.insert("confirm_cap_reductions".into(), json!(true));
    }
    request
}

// ---------- defaults (global_manager_command.rs) ----------

/// The launches a first appointment grants (the operator's model directive).
#[must_use]
pub fn default_allowed_launches() -> Vec<ManagerLaunchChoiceV2> {
    let launch = |provider, model: &str, effort: &str| ManagerLaunchChoiceV2 {
        provider,
        model: model.into(),
        effort: Some(effort.into()),
    };
    vec![
        launch(SessionProvider::Claude, "claude-opus-5-5", "high"),
        launch(SessionProvider::Claude, "claude-opus-5-5", "xhigh"),
        launch(SessionProvider::Claude, "claude-sonnet-5-5", "high"),
        launch(SessionProvider::Codex, "gpt-6-astra", "xhigh"),
        launch(SessionProvider::Codex, "gpt-6-astra", "high"),
        launch(SessionProvider::Codex, "gpt-6-astra", "low"),
        launch(SessionProvider::Codex, "gpt-6.1-sol", "high"),
        launch(SessionProvider::Codex, "gpt-6.1-sol", "xhigh"),
        launch(SessionProvider::Codex, "gpt-6-luna", "high"),
        launch(SessionProvider::Codex, "gpt-6-luna", "xhigh"),
    ]
}

/// The Execute preset applied to a new policy.
#[must_use]
pub fn default_project_policy() -> ManagerPolicyV2 {
    preset_policy(
        &ManagerPolicyV2::default(),
        ManagerPolicyPreset::Execute,
        ManagerPolicyOrigin::New,
        HarnessManagerScopeModeV1::Project,
    )
}

fn preset_policy(
    base: &ManagerPolicyV2,
    preset: ManagerPolicyPreset,
    origin: ManagerPolicyOrigin,
    scope_mode: HarnessManagerScopeModeV1,
) -> ManagerPolicyV2 {
    let touched = BTreeSet::new();
    apply_manager_policy_preset(
        base,
        preset,
        &ManagerPresetContext {
            origin,
            touched: &touched,
            scope_mode,
        },
    )
    .policy
}

// ---------- caps (`:manager global set`, the tree's grant editor) ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapField {
    Active,
    Sessions,
    Containers,
    Spend,
    Groups,
}

impl CapField {
    pub const ALL: [Self; 5] = [
        Self::Active,
        Self::Sessions,
        Self::Containers,
        Self::Spend,
        Self::Groups,
    ];

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Active => "max active sessions",
            Self::Sessions => "max created sessions",
            Self::Containers => "max created containers",
            Self::Spend => "max spend (USD)",
            Self::Groups => "may create groups",
        }
    }

    #[must_use]
    pub fn parse(word: &str) -> Option<Self> {
        match word.to_ascii_lowercase().as_str() {
            "active" | "max_active_sessions" => Some(Self::Active),
            "sessions" | "created" | "max_created_sessions" => Some(Self::Sessions),
            "containers" | "max_created_containers" => Some(Self::Containers),
            "spend" | "max_spend_usd" => Some(Self::Spend),
            "groups" | "allow_create_groups" => Some(Self::Groups),
            _ => None,
        }
    }
}

/// The editable caps of one per-project policy.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GrantCaps {
    pub max_active_sessions: u16,
    pub max_created_sessions: u16,
    pub max_created_containers: u16,
    pub max_spend_usd: Option<f64>,
    pub allow_create_groups: bool,
}

impl GrantCaps {
    #[must_use]
    pub fn from_policy(p: &ManagerPolicyV2) -> Self {
        Self {
            max_active_sessions: p.max_active_sessions,
            max_created_sessions: p.max_created_sessions,
            max_created_containers: p.max_created_containers,
            max_spend_usd: p.max_spend_usd,
            allow_create_groups: p.allow_create_groups,
        }
    }

    pub fn apply(&self, p: &mut ManagerPolicyV2) {
        p.max_active_sessions = self.max_active_sessions;
        p.max_created_sessions = self.max_created_sessions;
        p.max_created_containers = self.max_created_containers;
        p.max_spend_usd = self.max_spend_usd;
        p.allow_create_groups = self.allow_create_groups;
    }

    #[must_use]
    pub fn describe(&self, field: CapField) -> String {
        match field {
            CapField::Active => self.max_active_sessions.to_string(),
            CapField::Sessions => self.max_created_sessions.to_string(),
            CapField::Containers => self.max_created_containers.to_string(),
            CapField::Spend => self
                .max_spend_usd
                .map_or_else(|| "none".to_string(), |usd| format!("{usd}")),
            CapField::Groups => if self.allow_create_groups {
                "on"
            } else {
                "off"
            }
            .to_string(),
        }
    }

    /// Set `field` from operator text, refusing what the policy would refuse.
    ///
    /// # Errors
    /// The bound the value breaks.
    pub fn set_from_text(&mut self, field: CapField, value: &str) -> Result<(), String> {
        let value = value.trim();
        let number = |low: u16, high: u16| {
            value
                .parse::<u16>()
                .ok()
                .filter(|n| (low..=high).contains(n))
                .ok_or_else(|| format!("{} must be a whole number {low}-{high}.", field.label()))
        };
        match field {
            CapField::Active => self.max_active_sessions = number(1, 100)?,
            CapField::Sessions => self.max_created_sessions = number(0, 1024)?,
            CapField::Containers => self.max_created_containers = number(0, 64)?,
            CapField::Spend => {
                self.max_spend_usd = if matches!(value, "none" | "off" | "unlimited") {
                    None
                } else {
                    Some(
                        value
                            .trim_start_matches('$')
                            .parse::<f64>()
                            .ok()
                            .filter(|usd| usd.is_finite() && *usd > 0.0)
                            .ok_or("Spend must be a positive USD amount, or none.")?,
                    )
                };
            }
            CapField::Groups => {
                self.allow_create_groups = match value.to_ascii_lowercase().as_str() {
                    "on" | "true" | "yes" => true,
                    "off" | "false" | "no" => false,
                    _ => return Err("Groups must be on or off.".into()),
                };
            }
        }
        Ok(())
    }

    fn from_value(v: &Value) -> Result<Self, String> {
        let n = |k: &str| {
            v.get(k)
                .and_then(Value::as_u64)
                .and_then(|n| u16::try_from(n).ok())
                .ok_or_else(|| format!("{k} must be a whole number"))
        };
        Ok(Self {
            max_active_sessions: n("max_active_sessions")?,
            max_created_sessions: n("max_created_sessions")?,
            max_created_containers: n("max_created_containers")?,
            max_spend_usd: match v.get("max_spend_usd") {
                None | Some(Value::Null) => None,
                Some(x) => Some(x.as_f64().ok_or("max_spend_usd must be a number")?),
            },
            allow_create_groups: v
                .get("allow_create_groups")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }
}

// ---------- prefill notes (#1902) ----------

const DEFAULTS_NOTE: &str = ". Started from the first-appointment defaults (no revoked root grant over these projects was used); use `set` or `configure` to edit them.";

fn capability_name(c: ManagerCapabilityV2) -> String {
    serde_json::to_value(c)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| format!("{c:?}"))
}

/// The diff of a prefill from the first-appointment defaults.
#[must_use]
pub fn prefill_diff(prefill: &PortfolioGrantPrefillV1) -> Vec<String> {
    let default_policy = default_project_policy();
    let default_launches = default_allowed_launches();
    let mut lines = Vec::new();
    let names = |from: &[ManagerCapabilityV2], not_in: &[ManagerCapabilityV2], sign: char| {
        from.iter()
            .filter(|c| !not_in.contains(c))
            .map(|c| format!("{sign}{}", capability_name(*c)))
            .collect::<Vec<_>>()
    };
    let mut caps = names(
        &prefill.policy.capabilities,
        &default_policy.capabilities,
        '+',
    );
    caps.extend(names(
        &default_policy.capabilities,
        &prefill.policy.capabilities,
        '-',
    ));
    if !caps.is_empty() {
        lines.push(format!("capabilities {}", caps.join(" ")));
    }
    let added: Vec<String> = prefill
        .allowed_launches
        .iter()
        .filter(|l| !default_launches.contains(l))
        .map(ManagerLaunchChoiceV2::display)
        .collect();
    let dropped = default_launches
        .iter()
        .filter(|l| !prefill.allowed_launches.contains(l))
        .count();
    if !added.is_empty() || dropped > 0 {
        lines.push(format!(
            "launches +{} ({}) -{dropped}",
            added.len(),
            added.join(", ")
        ));
    }
    let (new_caps, old_caps) = (
        GrantCaps::from_policy(&prefill.policy),
        GrantCaps::from_policy(&default_policy),
    );
    for field in CapField::ALL {
        let (new, old) = (new_caps.describe(field), old_caps.describe(field));
        if new != old {
            lines.push(format!("{} {old} → {new}", field.label()));
        }
    }
    if prefill.child_policy.is_some() {
        lines.push("child policy kept".into());
    }
    if prefill.max_direct_reports != PORTFOLIO_DEFAULT_MAX_DIRECT_REPORTS {
        lines.push(format!(
            "direct reports {PORTFOLIO_DEFAULT_MAX_DIRECT_REPORTS} → {}",
            prefill.max_direct_reports
        ));
    }
    lines
}

#[must_use]
pub fn prefill_summary(prefill: &PortfolioGrantPrefillV1) -> String {
    let diff = prefill_diff(prefill);
    format!(
        "Prefilled from grant v{} of revoked node {} ({} coverage); {}",
        prefill.source_grant_version,
        short(prefill.source_node_id),
        if prefill.exact_coverage {
            "same"
        } else {
            "overlapping"
        },
        if diff.is_empty() {
            "no difference from defaults".to_string()
        } else {
            format!("differs from defaults: {}", diff.join("; "))
        }
    )
}

// ---------- global planners ----------

fn grant_of(v: &Value) -> Result<Option<GlobalManagerGrantV1>, String> {
    if v.is_null() {
        return Ok(None);
    }
    typed("global grant", v).map(Some)
}

fn nodes_of(v: &Value) -> Result<Vec<PortfolioNodeV1>, String> {
    let list = v.get("nodes").unwrap_or(v);
    if list.is_null() {
        return Ok(Vec::new());
    }
    typed("portfolio nodes", list)
}

/// `:manager global appoint|prefill`: the appointment request (or, without a
/// seat, only the preview note). An active grant keeps its launches and PM
/// policy; after a revoke the most recent revoked root grant over the same
/// coverage is the prefill unless `use_defaults`.
///
/// # Errors
/// A malformed snapshot or a request the daemon would refuse.
pub fn global_appoint_plan(
    current: &Value,
    nodes: &Value,
    session_id: Option<&str>,
    project_ids: &[String],
    use_defaults: bool,
) -> Built {
    let current = grant_of(current)?;
    let revoked = current.as_ref().is_some_and(|g| g.state != "active");
    let active = current.filter(|g| g.state == "active");
    let project_ids = uuids("project_ids", project_ids)?;
    let prefill = if active.is_none() && revoked && !use_defaults {
        prefill_from_revoked_root(&nodes_of(nodes)?, &project_ids)
    } else {
        None
    };
    let Some(session_id) = session_id else {
        let note = if active.is_some() {
            "A grant is active: appointing keeps its policy (nothing to prefill).".to_string()
        } else {
            prefill.as_ref().map_or_else(
                || "No revoked root grant over these projects: an appointment starts from the defaults.".to_string(),
                prefill_summary,
            )
        };
        return Ok(json!({ "request": null, "note": note }));
    };
    let request = ConfigureGlobalManagerRequestV1 {
        session_id: uuid("session_id", session_id)?,
        project_ids,
        allowed_launches: match (&active, &prefill) {
            (Some(g), _) => g.allowed_launches.clone(),
            (None, Some(p)) => p.allowed_launches.clone(),
            (None, None) => default_allowed_launches(),
        },
        project_policy: match (&active, &prefill) {
            (Some(g), _) => g.project_policy.clone(),
            (None, Some(p)) => p.policy.clone(),
            (None, None) => default_project_policy(),
        },
        expected_grant_version: active.as_ref().map_or(0, |g| g.grant_version),
        idempotency_key: key(),
    };
    request
        .validate()
        .map_err(|e| format!("Global manager: {e}"))?;
    let note = match &prefill {
        Some(p) => format!(". {}", prefill_summary(p)),
        None if revoked => DEFAULTS_NOTE.to_string(),
        None => String::new(),
    };
    Ok(json!({ "request": to_value(&request)?, "note": note }))
}

fn grant_request(
    grant: &GlobalManagerGrantV1,
    project_ids: Vec<Uuid>,
    policy: ManagerPolicyV2,
) -> Result<ConfigureGlobalManagerRequestV1, String> {
    let request = ConfigureGlobalManagerRequestV1 {
        session_id: grant.seat_session_id,
        project_ids,
        allowed_launches: grant.allowed_launches.clone(),
        project_policy: policy,
        expected_grant_version: grant.grant_version,
        idempotency_key: key(),
    };
    request
        .validate()
        .map_err(|e| format!("Global manager: {e}"))?;
    Ok(request)
}

fn active_grant(grant: &Value) -> Result<GlobalManagerGrantV1, String> {
    grant_of(grant)?
        .filter(|g| g.state == "active")
        .ok_or_else(|| "No global manager is appointed.".to_string())
}

/// `:manager global set <field> <value>`: one cap of the active grant.
///
/// # Errors
/// No active grant, an unknown field or a value out of bounds.
pub fn global_set_plan(grant: &Value, field: &str, value: &str) -> Built {
    let grant = active_grant(grant)?;
    let field = CapField::parse(field).ok_or("Use :manager global set <active|sessions|containers|spend|groups> <value> [confirm] (spend: USD or none; groups: on|off).")?;
    let before = GrantCaps::from_policy(&grant.project_policy);
    let mut after = before;
    after.set_from_text(field, value)?;
    let mut policy = grant.project_policy.clone();
    after.apply(&mut policy);
    let request = grant_request(&grant, grant.project_ids.clone(), policy)?;
    Ok(json!({
        "request": to_value(&request)?,
        "change": format!("{} {} -> {}", field.label(), before.describe(field), after.describe(field)),
    }))
}

/// `:manager global add-project`: the active grant plus `project_ids`.
///
/// # Errors
/// No active grant.
pub fn global_add_projects_plan(grant: &Value, project_ids: &[String]) -> Built {
    let grant = active_grant(grant)?;
    let mut all = grant.project_ids.clone();
    for id in uuids("project_ids", project_ids)? {
        if !all.contains(&id) {
            all.push(id);
        }
    }
    let request = grant_request(&grant, all, grant.project_policy.clone())?;
    to_value(&request)
}

/// The tree's `a` (new seat) or `e` (projects and caps) on the global row.
///
/// # Errors
/// No active grant, nothing kept, or an invalid request.
pub fn global_edit_plan(
    grant: &Value,
    seat: Option<&str>,
    project_ids: &[String],
    caps: Option<&Value>,
) -> Built {
    let grant = active_grant(grant)?;
    let project_ids = uuids("project_ids", project_ids)?;
    if project_ids.is_empty() {
        return Err("Keep at least one project; x on the global row revokes the grant.".into());
    }
    let mut policy = grant.project_policy.clone();
    if let Some(c) = caps {
        GrantCaps::from_value(c)?.apply(&mut policy);
    }
    let mut request = grant_request(&grant, project_ids, policy)?;
    if let Some(seat) = seat {
        request.session_id = uuid("session_id", seat)?;
    }
    to_value(&request)
}

// ---------- portfolio planners (portfolio_command.rs) ----------

/// Efforts a trailing `/segment` of a launch spelling may name.
const LAUNCH_EFFORTS: [&str; 6] = ["minimal", "low", "medium", "high", "xhigh", "max"];

/// `Provider/model[/effort]` (provider case-insensitive).
///
/// # Errors
/// The spelling to fix.
pub fn parse_launch(spelling: &str) -> Result<ManagerLaunchChoiceV2, String> {
    let bad = || format!("Invalid launch {spelling}; use Provider/model[/effort].");
    let (provider, rest) = spelling.split_once('/').ok_or_else(bad)?;
    let provider = SessionProvider::parse_name(provider).ok_or_else(bad)?;
    let (model, effort) = match rest.rsplit_once('/') {
        Some((model, effort)) if LAUNCH_EFFORTS.contains(&effort) => {
            (model, Some(effort.to_string()))
        }
        _ => (rest, None),
    };
    if model.is_empty() {
        return Err(bad());
    }
    Ok(ManagerLaunchChoiceV2 {
        provider,
        model: model.into(),
        effort,
    })
}

/// A grant every `children` node narrows: `base` widened to each child's
/// capabilities and one unit more than each in every finite allowance.
#[must_use]
pub fn covering_policy(base: &ManagerPolicyV2, children: &[&PortfolioNodeV1]) -> ManagerPolicyV2 {
    let mut policy = base.clone();
    for child in children {
        let child = &child.grant.project_policy;
        for c in &child.capabilities {
            if !policy.capabilities.contains(c) {
                policy.capabilities.push(*c);
            }
        }
        policy.max_created_containers = policy
            .max_created_containers
            .max(child.max_created_containers.saturating_add(1));
        policy.max_created_sessions = policy
            .max_created_sessions
            .max(child.max_created_sessions.saturating_add(1));
        policy.max_active_sessions = policy
            .max_active_sessions
            .max(child.max_active_sessions.saturating_add(1));
        policy.max_spend_usd = match (policy.max_spend_usd, child.max_spend_usd) {
            (Some(own), Some(child)) => Some(own.max(child)),
            _ => None,
        };
        for launch in &child.allowed_launches {
            if !policy.allowed_launches.is_empty()
                && !policy.allowed_launches.iter().any(|e| e.allows(launch))
            {
                policy.allowed_launches.push(launch.clone());
            }
        }
    }
    policy.provider_limits.clear();
    policy
}

#[must_use]
pub fn covering_launches(
    base: &[ManagerLaunchChoiceV2],
    children: &[&PortfolioNodeV1],
) -> Vec<ManagerLaunchChoiceV2> {
    let mut launches = base.to_vec();
    for child in children {
        for launch in &child.grant.allowed_launches {
            if !launches.iter().any(|e| e.allows(launch)) {
                launches.push(launch.clone());
            }
        }
    }
    launches
}

#[must_use]
pub fn covering_reports(base: u16, children: &[&PortfolioNodeV1]) -> u16 {
    children
        .iter()
        .map(|c| c.max_direct_reports)
        .chain([base, u16::try_from(children.len()).unwrap_or(u16::MAX)])
        .max()
        .unwrap_or(base)
        .min(PORTFOLIO_MAX_DIRECT_REPORTS)
}

fn appoint_above_request(
    label: &str,
    seat: Uuid,
    adopted: &[&PortfolioNodeV1],
    extra: &[Uuid],
) -> Result<ConfigurePortfolioNodeRequestV1, String> {
    let parent = adopted.first().and_then(|n| n.parent_node_id);
    if adopted.iter().any(|n| n.parent_node_id != parent) {
        return Err("The adopted nodes must share one parent (or all be roots).".into());
    }
    let mut project_ids: Vec<Uuid> = Vec::new();
    for id in adopted
        .iter()
        .flat_map(|n| n.grant.project_ids.iter())
        .chain(extra)
    {
        if !project_ids.contains(id) {
            project_ids.push(*id);
        }
    }
    Ok(ConfigurePortfolioNodeRequestV1 {
        node_id: None,
        parent_node_id: parent,
        adopt_node_ids: adopted.iter().map(|n| n.node_id).collect(),
        expected_parent_grant_version: None,
        tier_label: label.into(),
        seat_session_id: seat,
        project_ids,
        allowed_launches: covering_launches(&default_allowed_launches(), adopted),
        policy: covering_policy(&default_project_policy(), adopted),
        child_policy: None,
        max_direct_reports: covering_reports(PORTFOLIO_DEFAULT_MAX_DIRECT_REPORTS, adopted),
        expected_node_grant_version: 0,
        expected_authority_epoch: 0,
        idempotency_key: key(),
    })
}

/// The tier label a manager appointed above `label` gets (display only).
#[must_use]
pub fn label_above(label: &str) -> String {
    match label {
        "global" => "pinnacle".into(),
        "pinnacle" => "swarm".into(),
        other => format!("above {other}")
            .chars()
            .take(PORTFOLIO_TIER_LABEL_MAX)
            .collect::<String>()
            .trim()
            .to_string(),
    }
}

/// `:manager portfolio appoint <label> [--adopt <nodes>] [projects]`. Without
/// adopted nodes a new root over `project_ids` (prefilled from the last
/// revoked root over them); with them, a node above them covering their
/// projects plus `project_ids`.
///
/// # Errors
/// An invalid label, unknown adopted node or invalid request.
pub fn portfolio_appoint_plan(
    nodes: &Value,
    label: &str,
    seat: &str,
    project_ids: &[String],
    adopt: &[String],
) -> Built {
    if !valid_tier_label(label) {
        return Err(format!("Invalid tier label: {label}"));
    }
    let all = nodes_of(nodes)?;
    let seat = uuid("seat", seat)?;
    let project_ids = uuids("project_ids", project_ids)?;
    let (request, note) = if adopt.is_empty() {
        if project_ids.is_empty() {
            return Err(
                "Every project is already covered by a portfolio node; name the projects.".into(),
            );
        }
        let prefill = prefill_from_revoked_root(&all, &project_ids);
        let note = prefill.as_ref().map(prefill_summary);
        let request = ConfigurePortfolioNodeRequestV1 {
            node_id: None,
            parent_node_id: None,
            adopt_node_ids: Vec::new(),
            expected_parent_grant_version: None,
            tier_label: label.into(),
            seat_session_id: seat,
            project_ids,
            allowed_launches: prefill
                .as_ref()
                .map_or_else(default_allowed_launches, |p| p.allowed_launches.clone()),
            policy: prefill
                .as_ref()
                .map_or_else(default_project_policy, |p| p.policy.clone()),
            child_policy: prefill.as_ref().and_then(|p| p.child_policy.clone()),
            max_direct_reports: prefill
                .as_ref()
                .map_or(PORTFOLIO_DEFAULT_MAX_DIRECT_REPORTS, |p| {
                    p.max_direct_reports
                }),
            expected_node_grant_version: 0,
            expected_authority_epoch: 0,
            idempotency_key: key(),
        };
        (request, note)
    } else {
        let ids = uuids("adopt", adopt)?;
        let adopted: Vec<&PortfolioNodeV1> = ids
            .iter()
            .map(|id| {
                all.iter()
                    .find(|n| n.node_id == *id && n.state == "active")
                    .ok_or_else(|| format!("No active portfolio node {id}"))
            })
            .collect::<Result<_, _>>()?;
        (
            appoint_above_request(label, seat, &adopted, &project_ids)?,
            None,
        )
    };
    request.validate().map_err(|e| format!("Portfolio: {e}"))?;
    Ok(json!({ "request": to_value(&request)?, "note": note }))
}

/// `:manager portfolio launch add|remove <node> <launches...>`, CAS on the
/// node's grant version.
///
/// # Errors
/// A launch spelling or request the daemon would refuse.
pub fn portfolio_launch_plan(node: &Value, add: bool, launches: &[String]) -> Built {
    let node: PortfolioNodeV1 = typed("portfolio node", node)?;
    let launches = launches
        .iter()
        .map(|s| parse_launch(s))
        .collect::<Result<Vec<_>, _>>()?;
    if launches.is_empty() {
        return Err(
            "Use :manager portfolio launch add|remove <node> <Provider/model[/effort]>...".into(),
        );
    }
    let (add, remove) = if add {
        (launches, Vec::new())
    } else {
        (Vec::new(), launches)
    };
    let request = ConfigurePortfolioNodeLaunchesRequestV1 {
        node_id: node.node_id,
        add,
        remove,
        expected_node_grant_version: node.grant.grant_version,
        idempotency_key: key(),
    };
    request.validate().map_err(|e| format!("Portfolio: {e}"))?;
    to_value(&request)
}

fn node_request(
    node: &PortfolioNodeV1,
    seat: Uuid,
    project_ids: Vec<Uuid>,
    policy: ManagerPolicyV2,
) -> ConfigurePortfolioNodeRequestV1 {
    ConfigurePortfolioNodeRequestV1 {
        node_id: Some(node.node_id),
        parent_node_id: node.parent_node_id,
        adopt_node_ids: Vec::new(),
        expected_parent_grant_version: None,
        tier_label: node.tier_label.clone(),
        seat_session_id: seat,
        project_ids,
        allowed_launches: node.grant.allowed_launches.clone(),
        policy,
        child_policy: node.child_policy.clone(),
        max_direct_reports: node.max_direct_reports,
        expected_node_grant_version: node.grant.grant_version,
        expected_authority_epoch: node.authority_epoch,
        idempotency_key: key(),
    }
}

/// The tree's `a` (new seat) or `e` (projects and caps) on a portfolio row,
/// fenced on the node's current grant version and epoch.
///
/// # Errors
/// A revoked node, nothing kept or an invalid request.
pub fn portfolio_edit_plan(
    node: &Value,
    seat: Option<&str>,
    project_ids: Option<&[String]>,
    caps: Option<&Value>,
) -> Built {
    let node: PortfolioNodeV1 = typed("portfolio node", node)?;
    if node.state != "active" {
        return Err("The node is revoked: nothing to change.".into());
    }
    let seat = seat.map_or(Ok(node.grant.seat_session_id), |s| uuid("session_id", s))?;
    let project_ids = match project_ids {
        Some(ids) => uuids("project_ids", ids)?,
        None => node.grant.project_ids.clone(),
    };
    if project_ids.is_empty() {
        return Err("Keep at least one project; x revokes the node.".into());
    }
    let mut policy = node.grant.project_policy.clone();
    if let Some(c) = caps {
        GrantCaps::from_value(c)?.apply(&mut policy);
    }
    let request = node_request(&node, seat, project_ids, policy);
    request.validate().map_err(|e| format!("Portfolio: {e}"))?;
    to_value(&request)
}

/// The tree's `A`: the focused session appointed above `node`.
///
/// # Errors
/// An invalid request.
pub fn portfolio_above_plan(node: &Value, seat: &str) -> Built {
    let node: PortfolioNodeV1 = typed("portfolio node", node)?;
    let above = label_above(&node.tier_label);
    let request = appoint_above_request(&above, uuid("seat", seat)?, &[&node], &[])?;
    request.validate().map_err(|e| format!("Portfolio: {e}"))?;
    Ok(json!({ "request": to_value(&request)?, "label": above }))
}

/// The tree's `m` then `m`: `target` widens to cover and adopt `moved`.
///
/// # Errors
/// Nodes that are not active siblings, or an invalid request.
pub fn portfolio_move_plan(target: &Value, moved: &Value) -> Built {
    let target: PortfolioNodeV1 = typed("target node", target)?;
    let moved: PortfolioNodeV1 = typed("moved node", moved)?;
    if target.state != "active" || moved.state != "active" {
        return Err("A node of the move is revoked; press r.".into());
    }
    if target.node_id == moved.node_id {
        return Err("A node cannot adopt itself.".into());
    }
    if target.parent_node_id != moved.parent_node_id {
        return Err(format!(
            "{} {} is not a sibling of {} {}: only a node with the same parent (or another root) adopts it",
            target.tier_label,
            short(target.node_id),
            moved.tier_label,
            short(moved.node_id)
        ));
    }
    let mut project_ids = target.grant.project_ids.clone();
    let added: Vec<Uuid> = moved
        .grant
        .project_ids
        .iter()
        .filter(|id| !project_ids.contains(id))
        .copied()
        .collect();
    project_ids.extend(&added);
    let mut request = node_request(
        &target,
        target.grant.seat_session_id,
        project_ids,
        target.grant.project_policy.clone(),
    );
    request.adopt_node_ids = vec![moved.node_id];
    request.policy = covering_policy(&target.grant.project_policy, &[&moved]);
    request.allowed_launches = covering_launches(&target.grant.allowed_launches, &[&moved]);
    request.max_direct_reports = covering_reports(target.max_direct_reports, &[&moved]);
    request.validate().map_err(|e| format!("Portfolio: {e}"))?;
    Ok(json!({
        "request": to_value(&request)?,
        "added": added.iter().map(Uuid::to_string).collect::<Vec<_>>(),
    }))
}

// ---------- area nodes (manager_tree/actions.rs confirm_node_edit) ----------

/// The tree's `e` on an area node: active sessions, created sessions and
/// direct reports, fenced on the node and its parent.
///
/// # Errors
/// No change, a node with reports, or an invalid request.
pub fn node_edit_plan(node: &Value, parent: &Value, values: [u16; 3]) -> Built {
    let node: ManagerNodeViewV1 = typed("manager node", node)?;
    let parent: ManagerNodeViewV1 = typed("parent node", parent)?;
    if node.direct_reports > 0 {
        return Err(format!(
            "node has {} direct report(s); the daemon only edits a childless node's grant",
            node.direct_reports
        ));
    }
    let mut grant = node.grant.clone().ok_or("the node holds no grant")?;
    let mut policy = node.policy.clone().ok_or("the node holds no policy")?;
    let [active, created, reports] = values;
    if (active, created, reports)
        == (
            grant.allowance.max_active_sessions,
            grant.allowance.max_created_sessions,
            grant.max_direct_reports,
        )
    {
        return Err("No change to save.".into());
    }
    grant.allowance.max_active_sessions = active;
    grant.allowance.max_created_sessions = created;
    grant.max_direct_reports = reports;
    policy.max_active_sessions = active;
    policy.max_created_sessions = created;
    let request = ConfigureManagerNodeRequestV1 {
        node_id: Some(node.node_id),
        parent_node_id: parent.node_id,
        project_id: node.project_id,
        seat_root_session_id: node.seat_root_session_id,
        selector: node
            .selector
            .clone()
            .unwrap_or(rsi_common::manager_nodes::ManagerNodeSelectorV1::Project),
        grant,
        policy,
        expected_parent_grant_version: parent.grant_version,
        expected_parent_policy_version: parent.policy_version,
        expected_parent_authority_epoch: parent.authority_epoch,
        expected_node_grant_version: node.grant_version,
        idempotency_key: key(),
    };
    request
        .validate()
        .map_err(|e| format!("Manager node: {e}"))?;
    to_value(&request)
}

// ---------- policy presets (harness manager policy, grant editor) ----------

fn preset_of(name: &str) -> Result<ManagerPolicyPreset, String> {
    match name {
        "observe" => Ok(ManagerPolicyPreset::Observe),
        "execute" => Ok(ManagerPolicyPreset::Execute),
        "full" => Ok(ManagerPolicyPreset::FullProjectControl),
        "custom" => Ok(ManagerPolicyPreset::Custom),
        _ => Err("preset must be observe, execute, full or custom".into()),
    }
}

fn scope_mode_of(raw: &str) -> Result<HarnessManagerScopeModeV1, String> {
    serde_json::from_value(Value::String(raw.to_string()))
        .map_err(|_| "scope_mode must be project or selected".to_string())
}

/// What the policy form shows about a draft: the preset it matches, the
/// named zero allowances the preset would raise, and the validation refusal.
///
/// # Errors
/// A malformed policy or scope mode.
pub fn policy_classify(policy: &Value, scope_mode: &str) -> Built {
    let policy: ManagerPolicyV2 = typed("policy", policy)?;
    let c = classify_manager_policy(&policy, scope_mode_of(scope_mode)?);
    let suggestions = c
        .permission_profile
        .map(|profile| suggested_manager_allowances(&policy, profile))
        .unwrap_or_default();
    Ok(json!({
        "preset": c.preset.label(),
        "profile": c.permission_profile.map(ManagerPolicyPreset::label),
        "suggestions": suggestions
            .iter()
            .map(|s| json!({ "field": format!("{:?}", s.field), "from": s.from, "to": s.to }))
            .collect::<Vec<_>>(),
        "root_group_mismatch": c.root_group_permission_mismatch,
        "validation_error": c.validation_error,
    }))
}

/// Apply a preset to a draft (`saved`: the draft comes from a saved policy).
///
/// # Errors
/// A malformed policy, preset or scope mode.
pub fn policy_preset(policy: &Value, preset: &str, scope_mode: &str, saved: bool) -> Built {
    let base: ManagerPolicyV2 = typed("policy", policy)?;
    let origin = if saved {
        ManagerPolicyOrigin::Saved
    } else {
        ManagerPolicyOrigin::New
    };
    to_value(&preset_policy(
        &base,
        preset_of(preset)?,
        origin,
        scope_mode_of(scope_mode)?,
    ))
}

/// Raise exactly the named zero allowances the matched preset needs ("Use
/// suggested allowances").
///
/// # Errors
/// A malformed policy.
pub fn policy_suggestions(policy: &Value, scope_mode: &str) -> Built {
    let policy: ManagerPolicyV2 = typed("policy", policy)?;
    let profile = classify_manager_policy(&policy, scope_mode_of(scope_mode)?)
        .permission_profile
        .ok_or("This draft matches no preset; nothing to suggest.")?;
    let fields: Vec<_> = suggested_manager_allowances(&policy, profile)
        .into_iter()
        .map(|s| s.field)
        .collect();
    let edit = apply_suggested_manager_allowances(&policy, profile, &fields)
        .map_err(|e| format!("{e:?}"))?;
    to_value(&edit.policy)
}

/// Check a whole policy (the Advanced editor) the way the daemon will.
///
/// # Errors
/// The first field the daemon would refuse.
pub fn policy_check(policy: &Value) -> Built {
    let policy: ManagerPolicyV2 = typed("policy", policy)?;
    policy.validate().map_err(|e| format!("policy: {e}"))?;
    to_value(&policy)
}

/// The defaults a new grant starts from (`n` and `:manager global appoint`).
#[must_use]
pub fn default_grant() -> Value {
    json!({
        "policy": default_project_policy(),
        "launches": default_allowed_launches(),
    })
}

// ---------- workspace launch-and-appoint (launch_settings.rs) ----------

/// `ConfigureHarnessManagerWithPolicy` for a freshly launched PM: the whole
/// project, fenced on the stored appointment and policy versions; no grant
/// is the Execute preset.
///
/// # Errors
/// An invalid request.
pub fn pm_appoint_plan(
    project_id: &str,
    session_id: &str,
    expected_row_version: i64,
    expected_policy_version: i64,
    policy: Option<&Value>,
) -> Built {
    let policy = match policy {
        Some(p) if !p.is_null() => typed("policy", p)?,
        _ => default_project_policy(),
    };
    let request: ConfigureHarnessManagerWithPolicyRequestV2 = typed(
        "request",
        &json!({
            "scope": {
                "project_id": parse_uuid("project_id", project_id)?,
                "session_id": parse_uuid("session_id", session_id)?,
                "expected_row_version": expected_row_version,
            },
            "expected_policy_version": expected_policy_version,
            "idempotency_key": key(),
            "policy": policy,
        }),
    )?;
    request
        .validate()
        .map_err(|e| format!("Manager appointment: {e}"))?;
    to_value(&request)
}

// ---------- forwarder params ----------

/// `ConfigureGlobalManager` with an optional cap-reduction confirmation.
///
/// # Errors
/// A malformed or invalid request.
pub fn global_configure_params(request: &Value, confirm: bool) -> Built {
    let r: ConfigureGlobalManagerRequestV1 = typed("Global manager request", request)?;
    r.validate().map_err(|e| format!("Global manager: {e}"))?;
    Ok(confirmed(to_value(&r)?, confirm))
}

/// `ConfigurePortfolioNode` with an optional cap-reduction confirmation.
///
/// # Errors
/// A malformed or invalid request.
pub fn portfolio_configure_full_params(request: &Value, confirm: bool) -> Built {
    let r: ConfigurePortfolioNodeRequestV1 = typed("Portfolio request", request)?;
    r.validate().map_err(|e| format!("Portfolio: {e}"))?;
    Ok(confirmed(to_value(&r)?, confirm))
}

/// `ConfigurePortfolioNodeLaunches` with an optional confirmation.
///
/// # Errors
/// A malformed or invalid request.
pub fn portfolio_launches_params(request: &Value, confirm: bool) -> Built {
    let r: ConfigurePortfolioNodeLaunchesRequestV1 = typed("Portfolio launches request", request)?;
    r.validate().map_err(|e| format!("Portfolio: {e}"))?;
    Ok(confirmed(to_value(&r)?, confirm))
}

/// `ConfigureManagerNode`, for `:manager node configure` and the tree.
///
/// # Errors
/// A malformed or invalid request.
pub fn node_configure_params(request: &Value) -> Built {
    let r: ConfigureManagerNodeRequestV1 = typed("Manager node request", request)?;
    r.validate().map_err(|e| format!("Manager node: {e}"))?;
    to_value(&r)
}

/// `ListManagerNodes {project_id, after_node_id?, limit}`.
///
/// # Errors
/// A malformed id.
pub fn nodes_list_params(project_id: &str, after: Option<&str>) -> Built {
    Ok(json!({
        "project_id": parse_uuid("project_id", project_id)?,
        "after_node_id": after.map(|a| parse_uuid("after_node_id", a)).transpose()?,
        "limit": NODE_PAGE,
    }))
}

/// `RuleOperatorEscalation {hop_id, ruling, idempotency_key}`.
///
/// # Errors
/// A malformed id, an empty ruling or key.
pub fn rule_params(hop_id: &str, ruling: &str, idempotency_key: &str) -> Built {
    let ruling = ruling.trim();
    if ruling.is_empty() || ruling.len() > MAX_RULING || ruling.contains('\0') {
        return Err("the ruling must be non-empty text".into());
    }
    if idempotency_key.trim().is_empty() || idempotency_key.len() > 128 {
        return Err("idempotency_key must be 1..=128 bytes".into());
    }
    Ok(json!({
        "hop_id": parse_uuid("hop_id", hop_id)?,
        "ruling": ruling,
        "idempotency_key": idempotency_key,
    }))
}

/// `ListOperatorEscalations {include_closed: false, undelivered_after}`: one
/// page of failed and uncertain tier mail.
///
/// # Errors
/// A malformed cursor.
pub fn undelivered_params(after: Option<&str>) -> Built {
    if let Some(a) = after
        && (a.is_empty() || a.len() > 512 || a.contains(char::is_whitespace))
    {
        return Err("the undelivered cursor is malformed".into());
    }
    Ok(json!({ "include_closed": false, "undelivered_after": after }))
}

/// `GetManagerNodeWorkspace {node: {kind, node_id | project_id}}`.
///
/// # Errors
/// An unknown kind or malformed id.
pub fn node_workspace_params(kind: &str, id: &str) -> Built {
    let id = parse_uuid("node id", id)?;
    let node = match kind {
        "portfolio" => json!({ "kind": "portfolio", "node_id": id }),
        "area" => json!({ "kind": "area", "node_id": id }),
        "project" => json!({ "kind": "project", "project_id": id }),
        _ => return Err("kind must be portfolio, project or area".into()),
    };
    Ok(json!({ "node": node }))
}

// ---------- planner commands (no daemon) ----------

#[tauri::command]
pub fn mgr_global_appoint_plan(
    current: Value,
    nodes: Value,
    session_id: Option<String>,
    project_ids: Vec<String>,
    use_defaults: bool,
) -> CmdResult {
    global_appoint_plan(
        &current,
        &nodes,
        session_id.as_deref(),
        &project_ids,
        use_defaults,
    )
}

#[tauri::command]
pub fn mgr_global_set_plan(grant: Value, field: String, value: String) -> CmdResult {
    global_set_plan(&grant, &field, &value)
}

#[tauri::command]
pub fn mgr_global_add_projects_plan(grant: Value, project_ids: Vec<String>) -> CmdResult {
    global_add_projects_plan(&grant, &project_ids)
}

#[tauri::command]
pub fn mgr_global_edit_plan(
    grant: Value,
    seat: Option<String>,
    project_ids: Vec<String>,
    caps: Option<Value>,
) -> CmdResult {
    global_edit_plan(&grant, seat.as_deref(), &project_ids, caps.as_ref())
}

#[tauri::command]
pub fn mgr_portfolio_appoint_plan(
    nodes: Value,
    label: String,
    seat: String,
    project_ids: Vec<String>,
    adopt: Vec<String>,
) -> CmdResult {
    portfolio_appoint_plan(&nodes, &label, &seat, &project_ids, &adopt)
}

#[tauri::command]
pub fn mgr_portfolio_launch_plan(node: Value, add: bool, launches: Vec<String>) -> CmdResult {
    portfolio_launch_plan(&node, add, &launches)
}

#[tauri::command]
pub fn mgr_portfolio_edit_plan(
    node: Value,
    seat: Option<String>,
    project_ids: Option<Vec<String>>,
    caps: Option<Value>,
) -> CmdResult {
    portfolio_edit_plan(
        &node,
        seat.as_deref(),
        project_ids.as_deref(),
        caps.as_ref(),
    )
}

#[tauri::command]
pub fn mgr_portfolio_above_plan(node: Value, seat: String) -> CmdResult {
    portfolio_above_plan(&node, &seat)
}

#[tauri::command]
pub fn mgr_portfolio_move_plan(target: Value, moved: Value) -> CmdResult {
    portfolio_move_plan(&target, &moved)
}

#[tauri::command]
pub fn mgr_node_edit_plan(node: Value, parent: Value, values: [u16; 3]) -> CmdResult {
    node_edit_plan(&node, &parent, values)
}

#[tauri::command]
pub fn mgr_policy_classify(policy: Value, scope_mode: String) -> CmdResult {
    policy_classify(&policy, &scope_mode)
}

#[tauri::command]
pub fn mgr_policy_preset(
    policy: Value,
    preset: String,
    scope_mode: String,
    saved: bool,
) -> CmdResult {
    policy_preset(&policy, &preset, &scope_mode, saved)
}

#[tauri::command]
pub fn mgr_policy_suggestions(policy: Value, scope_mode: String) -> CmdResult {
    policy_suggestions(&policy, &scope_mode)
}

#[tauri::command]
pub fn mgr_policy_check(policy: Value) -> CmdResult {
    policy_check(&policy)
}

#[tauri::command]
pub fn mgr_default_grant() -> Value {
    default_grant()
}

#[tauri::command]
pub fn mgr_pm_appoint_plan(
    project_id: String,
    session_id: String,
    expected_row_version: i64,
    expected_policy_version: i64,
    policy: Option<Value>,
) -> CmdResult {
    pm_appoint_plan(
        &project_id,
        &session_id,
        expected_row_version,
        expected_policy_version,
        policy.as_ref(),
    )
}

// ---------- forwarders (one daemon method each) ----------

#[tauri::command]
pub async fn global_manager_configure(
    state: State<'_, AppState>,
    request: Value,
    confirm: bool,
) -> CmdResult {
    let params = global_configure_params(&request, confirm)?;
    forward(&state, METHOD_CONFIGURE_GLOBAL, params).await
}

#[tauri::command]
pub async fn portfolio_node_configure_request(
    state: State<'_, AppState>,
    request: Value,
    confirm: bool,
) -> CmdResult {
    let params = portfolio_configure_full_params(&request, confirm)?;
    forward(&state, METHOD_CONFIGURE_PORTFOLIO, params).await
}

#[tauri::command]
pub async fn portfolio_node_launches(
    state: State<'_, AppState>,
    request: Value,
    confirm: bool,
) -> CmdResult {
    let params = portfolio_launches_params(&request, confirm)?;
    forward(&state, METHOD_PORTFOLIO_LAUNCHES, params).await
}

#[tauri::command]
pub async fn manager_nodes_list(
    state: State<'_, AppState>,
    project_id: String,
    after: Option<String>,
) -> CmdResult {
    let params = nodes_list_params(&project_id, after.as_deref())?;
    forward(&state, METHOD_LIST_NODES, params).await
}

#[tauri::command]
pub async fn manager_node_configure(state: State<'_, AppState>, request: Value) -> CmdResult {
    let params = node_configure_params(&request)?;
    forward(&state, METHOD_CONFIGURE_NODE, params).await
}

#[tauri::command]
pub async fn operator_restart_status(state: State<'_, AppState>) -> CmdResult {
    forward(&state, METHOD_RESTART_STATUS, json!({})).await
}

#[tauri::command]
pub async fn operator_restart_force(state: State<'_, AppState>) -> CmdResult {
    forward(&state, METHOD_RESTART_FORCE, json!({})).await
}

#[tauri::command]
pub async fn operator_restart_cancel(state: State<'_, AppState>) -> CmdResult {
    forward(&state, METHOD_RESTART_CANCEL, json!({})).await
}

#[tauri::command]
pub async fn operator_escalation_rule(
    state: State<'_, AppState>,
    hop_id: String,
    ruling: String,
    idempotency_key: String,
) -> CmdResult {
    let params = rule_params(&hop_id, &ruling, &idempotency_key)?;
    forward(&state, METHOD_RULE, params).await
}

#[tauri::command]
pub async fn operator_notice_ack(state: State<'_, AppState>, message_id: String) -> CmdResult {
    let params = json!({ "message_id": parse_uuid("message_id", &message_id)? });
    forward(&state, METHOD_ACK, params).await
}

#[tauri::command]
pub async fn operator_undelivered(state: State<'_, AppState>, after: Option<String>) -> CmdResult {
    forward(
        &state,
        METHOD_ESCALATIONS,
        undelivered_params(after.as_deref())?,
    )
    .await
}

#[tauri::command]
pub async fn manager_node_workspace(
    state: State<'_, AppState>,
    kind: String,
    id: String,
) -> CmdResult {
    forward(
        &state,
        METHOD_NODE_WORKSPACE,
        node_workspace_params(&kind, &id)?,
    )
    .await
}

#[tauri::command]
pub async fn harness_manager_with_policy(state: State<'_, AppState>, request: Value) -> CmdResult {
    let r: ConfigureHarnessManagerWithPolicyRequestV2 = typed("request", &request)?;
    r.validate()
        .map_err(|e| format!("Manager appointment: {e}"))?;
    forward(&state, METHOD_WITH_POLICY, to_value(&r)?).await
}

#[cfg(test)]
#[path = "manager_ops_tests.rs"]
mod tests;
