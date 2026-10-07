//! `:manager global` (#872 Slice B): the operator appoints, inspects and
//! revokes the global manager seat. All calls are operator-only daemon RPCs.
//!
//! - `:manager global` shows the active grant;
//! - `:manager global appoint [project names...]` appoints the focused session
//!   over the named projects (comma-separated, or space-separated single
//!   words; default: every project), with the allowlist defaulting to the
//!   operator's model directive and the PM policy to the Execute preset;
//! - `:manager global revoke` revokes the active grant;
//! - `:manager global set <field> <value> [confirm]` changes one cap of the
//!   active grant's per-project policy (fields: `active`, `sessions`,
//!   `containers`, `spend`, `groups`) and keeps everything else; lowering a cap
//!   a project already exceeds is previewed, and `confirm` accepts it;
//! - `:manager global configure <JSON>` sends a full typed request.

use std::collections::BTreeSet;

use rsi_common::global_manager::{
    ConfigureGlobalManagerRequestV1, GlobalManagerGrantV1, RevokeGlobalManagerRequestV1,
};
use rsi_common::harness_manager::HarnessManagerScopeModeV1;
use rsi_common::harness_manager_presets::{
    ManagerPolicyOrigin, ManagerPolicyPreset, ManagerPresetContext, apply_manager_policy_preset,
};
use rsi_common::harness_manager_v2::{ManagerLaunchChoiceV2, ManagerPolicyV2};
use rsi_common::types::{Project, SessionProvider};
use uuid::Uuid;

use crate::app::App;

/// Shown after a command's cap-reduction refusal opened the confirm step.
pub(crate) const CAP_CONFIRM_NOTICE: &str =
    "This lowers project caps: review the preview, y confirms, Esc cancels.";

const USAGE: &str = "Use :manager global [appoint [project names...]|revoke|set <field> <value> [confirm]|configure <JSON>].";
const SET_USAGE: &str = "Use :manager global set <active|sessions|containers|spend|groups> <value> [confirm] (spend: USD or none; groups: on|off).";

/// The caps of a grant's per-project policy the operator edits directly
/// (#1401): the same fields the `:manager tree` editor and `:manager global
/// set` change, with the bounds `ManagerPolicyV2::validate` enforces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CapField {
    Active,
    Sessions,
    Containers,
    Spend,
    Groups,
}

impl CapField {
    pub(crate) const ALL: [Self; 5] = [
        Self::Active,
        Self::Sessions,
        Self::Containers,
        Self::Spend,
        Self::Groups,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Active => "max active sessions",
            Self::Sessions => "max created sessions",
            Self::Containers => "max created containers",
            Self::Spend => "max spend (USD)",
            Self::Groups => "may create groups",
        }
    }

    fn parse(word: &str) -> Option<Self> {
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

/// The editable cap values of one policy.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct GrantCaps {
    pub max_active_sessions: u16,
    pub max_created_sessions: u16,
    pub max_created_containers: u16,
    pub max_spend_usd: Option<f64>,
    pub allow_create_groups: bool,
}

/// One `+`/`-` step of the spend cap, in USD.
const SPEND_STEP_USD: f64 = 5.0;

impl GrantCaps {
    pub(crate) fn from_policy(policy: &ManagerPolicyV2) -> Self {
        Self {
            max_active_sessions: policy.max_active_sessions,
            max_created_sessions: policy.max_created_sessions,
            max_created_containers: policy.max_created_containers,
            max_spend_usd: policy.max_spend_usd,
            allow_create_groups: policy.allow_create_groups,
        }
    }

    /// Write these caps into `policy`, leaving every other field alone.
    pub(crate) fn apply(&self, policy: &mut ManagerPolicyV2) {
        policy.max_active_sessions = self.max_active_sessions;
        policy.max_created_sessions = self.max_created_sessions;
        policy.max_created_containers = self.max_created_containers;
        policy.max_spend_usd = self.max_spend_usd;
        policy.allow_create_groups = self.allow_create_groups;
    }

    pub(crate) fn describe(&self, field: CapField) -> String {
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

    /// Step `field` by `delta` within the policy bounds (a boolean flips, and
    /// the spend cap steps by $5 with `none` below the lowest step).
    pub(crate) fn adjust(&mut self, field: CapField, delta: i32) {
        let step = |value: u16, low: u16, high: u16| {
            u16::try_from((i32::from(value) + delta).clamp(i32::from(low), i32::from(high)))
                .unwrap_or(low)
        };
        match field {
            CapField::Active => self.max_active_sessions = step(self.max_active_sessions, 1, 100),
            CapField::Sessions => {
                self.max_created_sessions = step(self.max_created_sessions, 0, 1024);
            }
            CapField::Containers => {
                self.max_created_containers = step(self.max_created_containers, 0, 64);
            }
            CapField::Spend => {
                let next = self.max_spend_usd.unwrap_or(0.0) + f64::from(delta) * SPEND_STEP_USD;
                self.max_spend_usd = (next > 0.0).then_some(next);
            }
            CapField::Groups => self.allow_create_groups = delta > 0,
        }
    }

    /// Set `field` from operator text, refusing values the policy would refuse.
    pub(crate) fn set_from_text(&mut self, field: CapField, value: &str) -> Result<(), String> {
        let value = value.trim();
        let number = |low: u16, high: u16| {
            value
                .parse::<u16>()
                .ok()
                .filter(|parsed| (low..=high).contains(parsed))
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
                    let usd = value
                        .trim_start_matches('$')
                        .parse::<f64>()
                        .ok()
                        .filter(|usd| usd.is_finite() && *usd > 0.0)
                        .ok_or("Spend must be a positive USD amount, or none.")?;
                    Some(usd)
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
}

/// The operator's 2026-10-02 model directive: managers on Claude Opus 5.5, and
/// (#1412) the launches an appointed PM hands to its workers: the routing
/// classes of `orchestration_router` (Sonnet for implementation, Codex for
/// cross-family review and lookup). The grant's list is the ceiling every PM
/// below it inherits, so a manager-only list left PMs unable to start a
/// worker on any other model or family.
pub(crate) fn default_allowed_launches() -> Vec<ManagerLaunchChoiceV2> {
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

/// The Execute preset applied to a new policy, for the PMs the seat appoints.
pub(crate) fn default_project_policy() -> ManagerPolicyV2 {
    let touched = BTreeSet::new();
    apply_manager_policy_preset(
        &ManagerPolicyV2::default(),
        ManagerPolicyPreset::Execute,
        &ManagerPresetContext {
            origin: ManagerPolicyOrigin::New,
            touched: &touched,
            scope_mode: HarnessManagerScopeModeV1::Project,
        },
    )
    .policy
}

fn find_project(projects: &[Project], name: &str) -> Result<Uuid, String> {
    projects
        .iter()
        .find(|project| project.name.eq_ignore_ascii_case(name))
        .map(|project| project.id)
        .ok_or_else(|| format!("Unknown project: {name}"))
}

/// Resolve `appoint` arguments to project ids: empty means every project; a
/// comma list names projects that may contain spaces; otherwise the whole text
/// may name one project, else each word names one.
pub(crate) fn resolve_projects(projects: &[Project], args: &str) -> Result<Vec<Uuid>, String> {
    let args = args.trim();
    if args.is_empty() {
        if projects.is_empty() {
            return Err("No projects to grant.".into());
        }
        return Ok(projects.iter().map(|project| project.id).collect());
    }
    if args.contains(',') {
        return args
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(|name| find_project(projects, name))
            .collect();
    }
    if let Ok(id) = find_project(projects, args) {
        return Ok(vec![id]);
    }
    args.split_whitespace()
        .map(|name| find_project(projects, name))
        .collect()
}

fn grant_summary(grant: &GlobalManagerGrantV1, projects: &[Project]) -> String {
    let names: Vec<String> = grant
        .project_ids
        .iter()
        .map(|id| {
            projects
                .iter()
                .find(|project| project.id == *id)
                .map_or_else(|| id.to_string(), |project| project.name.clone())
        })
        .collect();
    let launches: Vec<String> = grant
        .allowed_launches
        .iter()
        .map(|launch| {
            format!(
                "{:?}/{}/{}",
                launch.provider,
                launch.model,
                launch.effort.as_deref().unwrap_or("default")
            )
        })
        .collect();
    format!(
        "Global manager v{} seat {} ({}): projects [{}]; launches [{}]; PM policy {:?}",
        grant.grant_version,
        &grant.seat_session_id.to_string()[..8],
        grant.state,
        names.join(", "),
        launches.join(", "),
        grant.project_policy.mode,
    )
}

/// `set <field> <value> [confirm]`: one cap of the active grant, everything
/// else kept, fenced on the grant version read now.
async fn set_cap(app: &mut App, raw: &str) -> Result<String, String> {
    let (field, value, confirm) = parse_set(raw)?;
    let grant = active_version(app)
        .await?
        .filter(|grant| grant.state == "active")
        .ok_or_else(|| "No global manager is appointed.".to_string())?;
    let (request, change) = set_cap_request(&grant, field, &value)?;
    let grant = match app
        .client
        .configure_global_manager_confirmed(request.clone(), confirm)
        .await
    {
        Ok(grant) => grant,
        Err(error) => {
            let error = error.to_string();
            // #1544: without `confirm`, a cap-reduction refusal opens the same
            // confirm step as `appoint` instead of only erroring.
            if !confirm
                && crate::overlay::manager_tree::offer_cap_confirmation(
                    app,
                    "Change the global manager caps",
                    crate::overlay::manager_tree::PreparedRequest::ConfigureGlobal(request),
                    &error,
                )
                .await
            {
                return Ok(CAP_CONFIRM_NOTICE.into());
            }
            return Err(format!("Global manager: {error}"));
        }
    };
    Ok(format!(
        "Global manager {change}: {}",
        grant_summary(&grant, &app.projects)
    ))
}

fn parse_set(raw: &str) -> Result<(CapField, String, bool), String> {
    let mut words: Vec<&str> = raw.split_whitespace().collect();
    let confirm = words.last() == Some(&"confirm");
    if confirm {
        words.pop();
    }
    let [field, value] = words[..] else {
        return Err(SET_USAGE.into());
    };
    let field = CapField::parse(field).ok_or_else(|| SET_USAGE.to_string())?;
    Ok((field, value.to_string(), confirm))
}

/// The full configure request that changes one cap of `grant`, and a
/// `field before -> after` note.
fn set_cap_request(
    grant: &GlobalManagerGrantV1,
    field: CapField,
    value: &str,
) -> Result<(ConfigureGlobalManagerRequestV1, String), String> {
    let before = GrantCaps::from_policy(&grant.project_policy);
    let mut after = before;
    after.set_from_text(field, value)?;
    let mut project_policy = grant.project_policy.clone();
    after.apply(&mut project_policy);
    let request = ConfigureGlobalManagerRequestV1 {
        session_id: grant.seat_session_id,
        project_ids: grant.project_ids.clone(),
        allowed_launches: grant.allowed_launches.clone(),
        project_policy,
        expected_grant_version: grant.grant_version,
        idempotency_key: Uuid::new_v4().to_string(),
    };
    request
        .validate()
        .map_err(|error| format!("Global manager: {error}"))?;
    let change = format!(
        "{} {} -> {}",
        field.label(),
        before.describe(field),
        after.describe(field)
    );
    Ok((request, change))
}

pub(crate) async fn dispatch_global_command(app: &mut App, command: &str) {
    match run_global_command(app, command).await {
        Ok(message) => app.notify_success(message),
        Err(error) => app.notify_error(error),
    }
    app.mark_dirty();
}

async fn active_version(app: &mut App) -> Result<Option<GlobalManagerGrantV1>, String> {
    app.client
        .get_global_manager()
        .await
        .map_err(|error| format!("Global manager: {error}"))
}

async fn run_global_command(app: &mut App, command: &str) -> Result<String, String> {
    let command = command.trim();
    if command == "show" {
        let grant = active_version(app).await?;
        return Ok(grant.map_or_else(
            || {
                "No global manager is appointed. Focus a session and run :manager global appoint."
                    .to_string()
            },
            |grant| grant_summary(&grant, &app.projects),
        ));
    }
    if command == "revoke" {
        let grant = active_version(app)
            .await?
            .ok_or_else(|| "No global manager is appointed.".to_string())?;
        let revoked = app
            .client
            .revoke_global_manager(RevokeGlobalManagerRequestV1 {
                expected_grant_version: grant.grant_version,
                idempotency_key: Uuid::new_v4().to_string(),
            })
            .await
            .map_err(|error| format!("Global manager: {error}"))?;
        return Ok(format!(
            "Global manager revoked: {}",
            grant_summary(&revoked, &app.projects)
        ));
    }
    if let Some(raw) = command.strip_prefix("configure ") {
        let confirmed: rsi_common::portfolio_nodes::PortfolioCapConfirmation<
            ConfigureGlobalManagerRequestV1,
        > = serde_json::from_str(raw)
            .map_err(|error| format!("Global manager request JSON: {error}"))?;
        let request = confirmed.request;
        request
            .validate()
            .map_err(|error| format!("Global manager: {error}"))?;
        let grant = app
            .client
            .configure_global_manager_confirmed(request, confirmed.confirm_cap_reductions)
            .await
            .map_err(|error| format!("Global manager: {error}"))?;
        return Ok(format!(
            "Global manager saved: {}",
            grant_summary(&grant, &app.projects)
        ));
    }
    if let Some(raw) = command
        .strip_prefix("set")
        .filter(|rest| rest.is_empty() || rest.starts_with(' '))
    {
        return set_cap(app, raw).await;
    }
    let Some(args) = command
        .strip_prefix("appoint")
        .filter(|rest| rest.is_empty() || rest.starts_with(' '))
    else {
        return Err(USAGE.into());
    };
    let session_id = app
        .selected_session_id()
        .ok_or_else(|| "Focus the session to appoint as the global manager.".to_string())?;
    let project_ids = resolve_projects(&app.projects, args)?;
    let expected_grant_version = active_version(app)
        .await?
        .map_or(0, |grant| grant.grant_version);
    let request = ConfigureGlobalManagerRequestV1 {
        session_id,
        project_ids,
        allowed_launches: default_allowed_launches(),
        project_policy: default_project_policy(),
        expected_grant_version,
        idempotency_key: Uuid::new_v4().to_string(),
    };
    let grant = match app.client.configure_global_manager(request.clone()).await {
        Ok(grant) => grant,
        Err(error) => {
            let error = error.to_string();
            // #1544: a cap-reduction refusal opens the tree's confirm step
            // (preview, then `y` resends with confirm_cap_reductions:true).
            if crate::overlay::manager_tree::offer_cap_confirmation(
                app,
                "Appoint the global manager",
                crate::overlay::manager_tree::PreparedRequest::ConfigureGlobal(request),
                &error,
            )
            .await
            {
                return Ok(CAP_CONFIRM_NOTICE.into());
            }
            return Err(format!("Global manager: {error}"));
        }
    };
    Ok(format!(
        "Global manager appointed: {}",
        grant_summary(&grant, &app.projects)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::harness_manager_v2::{ManagerCapabilityV2, ManagerOperatingModeV2};

    fn project(name: &str) -> Project {
        let now = chrono::Utc::now();
        Project {
            id: Uuid::new_v4(),
            name: name.into(),
            path: None,
            description: None,
            color: Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn global_manager_appoint_resolves_project_names() {
        let projects = [project("Rsi"), project("Dictate Agent"), project("Notes")];
        let ids = |indices: &[usize]| indices.iter().map(|i| projects[*i].id).collect::<Vec<_>>();
        assert_eq!(resolve_projects(&projects, "").unwrap(), ids(&[0, 1, 2]));
        assert_eq!(
            resolve_projects(&projects, "rsi, Dictate Agent").unwrap(),
            ids(&[0, 1])
        );
        assert_eq!(
            resolve_projects(&projects, "Dictate Agent").unwrap(),
            ids(&[1])
        );
        assert_eq!(
            resolve_projects(&projects, "Rsi Notes").unwrap(),
            ids(&[0, 2])
        );
        assert_eq!(
            resolve_projects(&projects, "Rsi Missing").unwrap_err(),
            "Unknown project: Missing"
        );
    }

    fn grant() -> GlobalManagerGrantV1 {
        let now = chrono::Utc::now();
        GlobalManagerGrantV1 {
            grant_id: Uuid::new_v4(),
            grant_version: 7,
            seat_session_id: Uuid::new_v4(),
            state: "active".into(),
            project_ids: vec![Uuid::new_v4(), Uuid::new_v4()],
            allowed_launches: default_allowed_launches(),
            project_policy: default_project_policy(),
            operator_origin: "operator_rpc".into(),
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn set_raises_one_cap_and_keeps_the_rest_of_the_grant() {
        let grant = grant();
        let (request, change) = set_cap_request(&grant, CapField::Active, "12").unwrap();
        assert_eq!(request.project_policy.max_active_sessions, 12);
        assert_eq!(change, "max active sessions 4 -> 12");
        let mut expected = grant.project_policy.clone();
        expected.max_active_sessions = 12;
        assert_eq!(request.project_policy, expected);
        assert_eq!(request.session_id, grant.seat_session_id);
        assert_eq!(request.project_ids, grant.project_ids);
        assert_eq!(request.allowed_launches, grant.allowed_launches);
        assert_eq!(request.expected_grant_version, 7);
    }

    #[test]
    fn set_parses_fields_values_and_the_confirm_flag() {
        assert_eq!(
            parse_set(" active 12 confirm").unwrap(),
            (CapField::Active, "12".into(), true)
        );
        assert_eq!(
            parse_set("spend none").unwrap(),
            (CapField::Spend, "none".into(), false)
        );
        assert_eq!(parse_set("active").unwrap_err(), SET_USAGE);
        assert_eq!(parse_set("bogus 1").unwrap_err(), SET_USAGE);
        let grant = grant();
        assert!(set_cap_request(&grant, CapField::Active, "0").is_err());
        assert!(set_cap_request(&grant, CapField::Active, "101").is_err());
        assert!(set_cap_request(&grant, CapField::Containers, "65").is_err());
        assert!(set_cap_request(&grant, CapField::Spend, "-3").is_err());
        let (request, _) = set_cap_request(&grant, CapField::Spend, "$25.5").unwrap();
        assert_eq!(request.project_policy.max_spend_usd, Some(25.5));
        let (request, _) = set_cap_request(&grant, CapField::Spend, "none").unwrap();
        assert_eq!(request.project_policy.max_spend_usd, None);
    }

    #[test]
    fn adjust_steps_within_policy_bounds() {
        let mut caps = GrantCaps::from_policy(&default_project_policy());
        caps.adjust(CapField::Active, -100);
        assert_eq!(caps.max_active_sessions, 1);
        caps.adjust(CapField::Active, 500);
        assert_eq!(caps.max_active_sessions, 100);
        caps.adjust(CapField::Spend, 1);
        assert_eq!(caps.max_spend_usd, Some(5.0));
        caps.adjust(CapField::Spend, -1);
        assert_eq!(caps.max_spend_usd, None);
        caps.adjust(CapField::Groups, 1);
        assert!(caps.allow_create_groups);
        caps.adjust(CapField::Groups, -1);
        assert!(!caps.allow_create_groups);
    }

    #[test]
    fn global_manager_defaults_follow_the_operator_directive() {
        let launches = default_allowed_launches();
        assert_eq!(launches[0].provider, SessionProvider::Claude);
        assert_eq!(launches[0].model, "claude-opus-5-5");
        // #1412: the PMs below the seat inherit this list, so it carries the
        // worker classes (Sonnet and a second family), not only the manager.
        assert!(launches.iter().any(|launch| {
            launch.provider == SessionProvider::Claude
                && launch.model == "claude-sonnet-5-5"
                && launch.effort.as_deref() == Some("high")
        }));
        assert_eq!(
            launches
                .iter()
                .map(|launch| (
                    launch.provider,
                    launch.model.as_str(),
                    launch.effort.as_deref()
                ))
                .collect::<Vec<_>>(),
            vec![
                (SessionProvider::Claude, "claude-opus-5-5", Some("high")),
                (SessionProvider::Claude, "claude-opus-5-5", Some("xhigh")),
                (SessionProvider::Claude, "claude-sonnet-5-5", Some("high")),
                (SessionProvider::Codex, "gpt-6-astra", Some("xhigh")),
                (SessionProvider::Codex, "gpt-6-astra", Some("high")),
                (SessionProvider::Codex, "gpt-6-astra", Some("low")),
                (SessionProvider::Codex, "gpt-6.1-sol", Some("high")),
                (SessionProvider::Codex, "gpt-6.1-sol", Some("xhigh")),
                (SessionProvider::Codex, "gpt-6-luna", Some("high")),
                (SessionProvider::Codex, "gpt-6-luna", Some("xhigh")),
            ]
        );
        assert!(launches.iter().all(|launch| launch.validate().is_ok()));
        let policy = default_project_policy();
        assert_eq!(policy.mode, ManagerOperatingModeV2::Execute);
        assert!(
            policy
                .capabilities
                .contains(&ManagerCapabilityV2::SessionCreate)
        );
        assert_eq!(policy.validate(), Ok(()));
    }
}
