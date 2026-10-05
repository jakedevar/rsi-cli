//! `:manager global` (#872 Slice B): the operator appoints, inspects and
//! revokes the global manager seat. All calls are operator-only daemon RPCs.
//!
//! - `:manager global` shows the active grant;
//! - `:manager global appoint [project names...]` appoints the focused session
//!   over the named projects (comma-separated, or space-separated single
//!   words; default: every project), with the allowlist defaulting to the
//!   operator's model directive and the PM policy to the Execute preset;
//! - `:manager global revoke` revokes the active grant;
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

const USAGE: &str = "Use :manager global [appoint [project names...]|revoke|configure <JSON>].";

/// The operator's 2026-10-02 model directive: managers on Claude Opus 5.5.
pub(crate) fn default_allowed_launches() -> Vec<ManagerLaunchChoiceV2> {
    vec![ManagerLaunchChoiceV2 {
        provider: SessionProvider::Claude,
        model: "claude-opus-5-5".into(),
        effort: Some("high".into()),
    }]
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
        let request: ConfigureGlobalManagerRequestV1 = serde_json::from_str(raw)
            .map_err(|error| format!("Global manager request JSON: {error}"))?;
        request
            .validate()
            .map_err(|error| format!("Global manager: {error}"))?;
        let grant = app
            .client
            .configure_global_manager(request)
            .await
            .map_err(|error| format!("Global manager: {error}"))?;
        return Ok(format!(
            "Global manager saved: {}",
            grant_summary(&grant, &app.projects)
        ));
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
    let grant = app
        .client
        .configure_global_manager(ConfigureGlobalManagerRequestV1 {
            session_id,
            project_ids,
            allowed_launches: default_allowed_launches(),
            project_policy: default_project_policy(),
            expected_grant_version,
            idempotency_key: Uuid::new_v4().to_string(),
        })
        .await
        .map_err(|error| format!("Global manager: {error}"))?;
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

    #[test]
    fn global_manager_defaults_follow_the_operator_directive() {
        let launches = default_allowed_launches();
        assert_eq!(launches.len(), 1);
        assert_eq!(launches[0].provider, SessionProvider::Claude);
        assert_eq!(launches[0].model, "claude-opus-5-5");
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
