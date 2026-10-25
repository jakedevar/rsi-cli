//! The New session prompt's settings side (gap sweep G4, hub #3000): a launch
//! with git-worktree isolation (`Ctrl-B`) and the manager appointment planned
//! on the settings side (crates/rsi/src/overlay/launch_settings.rs).
//!
//! `launch_prompt` forwards one `LaunchSession` with the TUI client's params
//! plus `sandbox`. `appoint_launched_manager` mirrors the TUI's
//! `appoint_launched_manager`: it reads `GetHarnessManager` and
//! `GetHarnessManagerPolicy` for the fences, re-grants the project's saved
//! grant unless the operator stepped the Policy row (#3270, desktop #204), else
//! applies the chosen preset to a fresh policy with the TUI's own code
//! (`rsi_common::harness_manager_presets`, so the grant is exactly the TUI's),
//! and sends one atomic `ConfigureHarnessManagerWithPolicy` (#2237). No other
//! daemon method is used.

use std::collections::BTreeSet;

use rsi_common::harness_manager::{
    ConfigureHarnessManagerRequestV1, HarnessManagerConfigV1, HarnessManagerScopeModeV1,
};
use rsi_common::harness_manager_presets::{
    ManagerPolicyOrigin, ManagerPolicyPreset, ManagerPresetContext, apply_manager_policy_preset,
    classify_manager_policy,
};
use rsi_common::harness_manager_v2::{
    ConfigureHarnessManagerWithPolicyRequestV2, HarnessManagerPolicyConfigV2, ManagerPolicyV2,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tauri::State;
use uuid::Uuid;

use crate::{AppState, CmdResult, LaunchRequest, forward, launch_params};

pub const METHOD_LAUNCH: &str = "LaunchSession";
pub const METHOD_GET_MANAGER: &str = "GetHarnessManager";
pub const METHOD_GET_POLICY: &str = "GetHarnessManagerPolicy";
pub const METHOD_APPOINT: &str = "ConfigureHarnessManagerWithPolicy";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptLaunch {
    pub query: String,
    pub provider: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub working_dir: Option<String>,
    #[serde(default)]
    pub sandbox: bool,
}

/// `LaunchSession` params: the plain launch's, plus the sandbox the TUI's
/// `Ctrl+B` asks for (`SandboxSpec::GitWorktree` with no branch).
pub fn prompt_launch_params(req: &PromptLaunch) -> Result<Value, String> {
    let mut params = launch_params(&LaunchRequest {
        query: req.query.clone(),
        provider: req.provider.clone(),
        model: req.model.clone(),
        effort: req.effort.clone(),
        project_id: req.project_id.clone(),
        working_dir: req.working_dir.clone(),
    })?;
    if req.sandbox {
        params["sandbox"] = json!({ "kind": "GitWorktree", "branch": null });
    }
    Ok(params)
}

#[tauri::command]
pub async fn launch_prompt(state: State<'_, AppState>, request: PromptLaunch) -> CmdResult {
    let params = prompt_launch_params(&request)?;
    forward(&state, METHOD_LAUNCH, params).await
}

/// What the manager supervises (`ManagerLaunchScope`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Project,
    Group(Uuid),
    Epic(Uuid),
}

pub fn parse_scope(kind: &str, id: Option<&str>) -> Result<Scope, String> {
    let id = || -> Result<Uuid, String> {
        id.and_then(|s| Uuid::parse_str(s.trim()).ok())
            .ok_or_else(|| format!("a {kind} scope needs its id"))
    };
    match kind {
        "project" => Ok(Scope::Project),
        "group" => Ok(Scope::Group(id()?)),
        "epic" => Ok(Scope::Epic(id()?)),
        other => Err(format!("unknown manager scope {other}")),
    }
}

/// The launch presets, as the TUI offers them (`MANAGER_LAUNCH_PRESETS`).
pub fn parse_preset(raw: &str) -> Result<ManagerPolicyPreset, String> {
    match raw {
        "observe" => Ok(ManagerPolicyPreset::Observe),
        "execute" => Ok(ManagerPolicyPreset::Execute),
        "full" => Ok(ManagerPolicyPreset::FullProjectControl),
        other => Err(format!("unknown manager policy preset {other}")),
    }
}

pub fn scope_mode(scope: Scope) -> HarnessManagerScopeModeV1 {
    if scope == Scope::Project {
        HarnessManagerScopeModeV1::Project
    } else {
        HarnessManagerScopeModeV1::Selected
    }
}

/// `ManagerLaunchPlan::scope_request`: project scope omits Epics, a Group
/// selection sends an explicit empty Epic list.
pub fn scope_request(
    project_id: Uuid,
    session_id: Uuid,
    scope: Scope,
    expected_row_version: i64,
) -> ConfigureHarnessManagerRequestV1 {
    let (epic_ids, group_ids) = match scope {
        Scope::Project => (None, Vec::new()),
        Scope::Group(id) => (Some(Vec::new()), vec![id]),
        Scope::Epic(id) => (Some(vec![id]), Vec::new()),
    };
    ConfigureHarnessManagerRequestV1 {
        project_id,
        session_id,
        epic_ids,
        group_ids,
        expected_row_version,
    }
}

/// `ManagerLaunchPlan::policy`: the preset applied to a fresh default policy.
pub fn preset_policy(preset: ManagerPolicyPreset, scope: Scope) -> ManagerPolicyV2 {
    let touched = BTreeSet::new();
    apply_manager_policy_preset(
        &ManagerPolicyV2::default(),
        preset,
        &ManagerPresetContext {
            origin: ManagerPolicyOrigin::New,
            touched: &touched,
            scope_mode: scope_mode(scope),
        },
    )
    .policy
}

/// The policy the appointment grants (the TUI's `appoint_launched_manager`,
/// #3270): the project's saved grant when one exists and the operator did not
/// choose a preset, else the preset's. `true` when it re-grants the saved one.
pub fn granted_policy(
    preset: ManagerPolicyPreset,
    preset_chosen: bool,
    scope: Scope,
    stored: Option<&HarnessManagerPolicyConfigV2>,
) -> (ManagerPolicyV2, bool) {
    match stored.filter(|_| !preset_chosen) {
        Some(saved) => (saved.policy.clone(), true),
        None => (preset_policy(preset, scope), false),
    }
}

/// The granted policy's label: the planned preset's when it is that preset's
/// policy, else the preset the policy classifies as (the TUI's label rule).
pub fn granted_label(
    policy: &ManagerPolicyV2,
    preset: ManagerPolicyPreset,
    scope: Scope,
) -> &'static str {
    if *policy == preset_policy(preset, scope) {
        preset.label()
    } else {
        classify_manager_policy(policy, scope_mode(scope))
            .preset
            .label()
    }
}

/// The atomic appointment request, fenced on what the daemon holds now.
#[allow(clippy::too_many_arguments)]
pub fn appointment_request(
    project_id: Uuid,
    session_id: Uuid,
    scope: Scope,
    preset: ManagerPolicyPreset,
    preset_chosen: bool,
    previous: Option<&HarnessManagerConfigV1>,
    stored: Option<&HarnessManagerPolicyConfigV2>,
    idempotency_key: String,
) -> Result<ConfigureHarnessManagerWithPolicyRequestV2, String> {
    let request = ConfigureHarnessManagerWithPolicyRequestV2 {
        scope: scope_request(
            project_id,
            session_id,
            scope,
            previous.map_or(0, |c| c.row_version),
        ),
        expected_policy_version: stored.map_or(0, |p| p.row_version),
        idempotency_key,
        policy: granted_policy(preset, preset_chosen, scope, stored).0,
    };
    request.validate().map_err(str::to_string)?;
    Ok(request)
}

/// Whether the appointment replaces another live manager (the TUI's notice).
pub fn replaces(previous: Option<&HarnessManagerConfigV1>, session_id: Uuid) -> bool {
    previous.is_some_and(|c| {
        !c.is_revoked() && c.current_session_id.unwrap_or(c.manager_session_id) != session_id
    })
}

fn not_appointed(e: impl std::fmt::Display) -> String {
    format!("Session launched but not appointed manager: {e}. Retry with :manager appoint on it.")
}

#[tauri::command]
pub async fn appoint_launched_manager(
    state: State<'_, AppState>,
    project_id: String,
    session_id: String,
    scope_kind: String,
    scope_id: Option<String>,
    preset: String,
    preset_chosen: Option<bool>,
) -> CmdResult {
    let preset_chosen = preset_chosen.unwrap_or(false);
    let project =
        Uuid::parse_str(project_id.trim()).map_err(|_| "invalid project_id".to_string())?;
    let session =
        Uuid::parse_str(session_id.trim()).map_err(|_| "invalid session_id".to_string())?;
    let scope = parse_scope(&scope_kind, scope_id.as_deref())?;
    let preset = parse_preset(&preset)?;
    let ask = json!({ "project_id": project });
    let previous: Option<HarnessManagerConfigV1> = serde_json::from_value(
        forward(&state, METHOD_GET_MANAGER, ask.clone())
            .await
            .map_err(not_appointed)?,
    )
    .map_err(not_appointed)?;
    let stored: Option<HarnessManagerPolicyConfigV2> = serde_json::from_value(
        forward(&state, METHOD_GET_POLICY, ask)
            .await
            .map_err(not_appointed)?,
    )
    .map_err(not_appointed)?;
    let request = appointment_request(
        project,
        session,
        scope,
        preset,
        preset_chosen,
        previous.as_ref(),
        stored.as_ref(),
        Uuid::new_v4().to_string(),
    )
    .map_err(not_appointed)?;
    let regrants_saved = granted_policy(preset, preset_chosen, scope, stored.as_ref()).1;
    let label = granted_label(&request.policy, preset, scope);
    let replaced = replaces(previous.as_ref(), session);
    let result = forward(
        &state,
        METHOD_APPOINT,
        serde_json::to_value(&request).map_err(|e| e.to_string())?,
    )
    .await
    .map_err(not_appointed)?;
    let scope_text = match scope {
        Scope::Project => "the whole project".to_string(),
        Scope::Group(_) => "a Group".to_string(),
        Scope::Epic(_) => "an Epic".to_string(),
    };
    let granted = if regrants_saved {
        format!("re-granting the saved grant ({label})")
    } else {
        format!("with the {label} policy")
    };
    Ok(json!({
        "message": format!(
            "Manager appointed for {scope_text} {granted}{}",
            if replaced { "; it replaces the previous manager" } else { "" }
        ),
        "replaced": replaced,
        "result": result,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn launch() -> PromptLaunch {
        PromptLaunch {
            query: "fix it".into(),
            provider: "Claude".into(),
            model: Some("claude-opus-4-5".into()),
            effort: Some("high".into()),
            project_id: None,
            working_dir: Some("/tmp".into()),
            sandbox: true,
        }
    }

    #[test]
    fn a_sandboxed_launch_asks_for_a_git_worktree() {
        let p = prompt_launch_params(&launch()).unwrap();
        assert_eq!(
            p["sandbox"],
            json!({ "kind": "GitWorktree", "branch": null })
        );
        assert_eq!(p["provider"], "Claude");
        assert_eq!(p["effort"], "high");
        let mut plain = launch();
        plain.sandbox = false;
        assert!(
            prompt_launch_params(&plain)
                .unwrap()
                .get("sandbox")
                .is_none_or(Value::is_null)
        );
        let mut bad = launch();
        bad.query = " ".into();
        assert!(
            prompt_launch_params(&bad).is_err(),
            "the plain launch's checks still apply"
        );
    }

    #[test]
    fn scopes_and_presets_parse_like_the_settings_side() {
        let g = Uuid::new_v4();
        assert_eq!(parse_scope("project", None).unwrap(), Scope::Project);
        assert_eq!(
            parse_scope("group", Some(&g.to_string())).unwrap(),
            Scope::Group(g)
        );
        assert!(parse_scope("epic", None).is_err());
        assert!(parse_scope("tree", None).is_err());
        assert_eq!(
            parse_preset("full").unwrap(),
            ManagerPolicyPreset::FullProjectControl
        );
        assert!(
            parse_preset("custom").is_err(),
            "Custom stays in the full editor"
        );
    }

    #[test]
    fn the_scope_request_matches_the_tui_shape() {
        let (p, s, g, e) = (
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        );
        let whole = scope_request(p, s, Scope::Project, 3);
        assert_eq!(whole.epic_ids, None);
        assert!(whole.group_ids.is_empty());
        assert_eq!(whole.expected_row_version, 3);
        let group = scope_request(p, s, Scope::Group(g), 0);
        assert_eq!(group.epic_ids, Some(vec![]));
        assert_eq!(group.group_ids, vec![g]);
        let epic = scope_request(p, s, Scope::Epic(e), 0);
        assert_eq!(epic.epic_ids, Some(vec![e]));
    }

    #[test]
    fn the_appointment_carries_the_preset_policy_and_fresh_fences() {
        let (p, s) = (Uuid::new_v4(), Uuid::new_v4());
        let r = appointment_request(
            p,
            s,
            Scope::Project,
            ManagerPolicyPreset::Execute,
            false,
            None,
            None,
            "k".into(),
        )
        .unwrap();
        assert_eq!(r.scope.expected_row_version, 0);
        assert_eq!(r.expected_policy_version, 0);
        assert_eq!(
            r.policy,
            preset_policy(ManagerPolicyPreset::Execute, Scope::Project)
        );
        assert_ne!(
            preset_policy(ManagerPolicyPreset::Observe, Scope::Project),
            preset_policy(ManagerPolicyPreset::FullProjectControl, Scope::Project),
            "the presets grant different policies"
        );
        assert!(
            appointment_request(
                p,
                s,
                Scope::Project,
                ManagerPolicyPreset::Execute,
                false,
                None,
                None,
                String::new()
            )
            .is_err(),
            "an empty idempotency key is refused before any call"
        );
    }

    fn saved_grant(project_id: Uuid) -> HarnessManagerPolicyConfigV2 {
        serde_json::from_value(json!({
            "project_id": project_id,
            "manager_session_id": Uuid::new_v4(),
            "scope_version": 1,
            "row_version": 4,
            "policy": preset_policy(ManagerPolicyPreset::Observe, Scope::Project),
            "updated_at": "2026-10-20T00:00:00.000000000Z",
            "revoked": false,
        }))
        .unwrap()
    }

    #[test]
    fn an_untouched_policy_re_grants_the_saved_grant() {
        let (p, s) = (Uuid::new_v4(), Uuid::new_v4());
        let stored = saved_grant(p);
        let r = appointment_request(
            p,
            s,
            Scope::Project,
            ManagerPolicyPreset::Execute,
            false,
            None,
            Some(&stored),
            "k".into(),
        )
        .unwrap();
        assert_eq!(
            r.policy, stored.policy,
            "the saved grant is sent, not the preset"
        );
        assert_eq!(r.expected_policy_version, 4);
        assert!(
            granted_policy(
                ManagerPolicyPreset::Execute,
                false,
                Scope::Project,
                Some(&stored)
            )
            .1
        );
        assert_eq!(
            granted_label(&r.policy, ManagerPolicyPreset::Execute, Scope::Project),
            ManagerPolicyPreset::Observe.label(),
            "the label names what the saved grant classifies as"
        );
    }

    #[test]
    fn a_chosen_preset_overrides_the_saved_grant() {
        let (p, s) = (Uuid::new_v4(), Uuid::new_v4());
        let stored = saved_grant(p);
        let r = appointment_request(
            p,
            s,
            Scope::Project,
            ManagerPolicyPreset::FullProjectControl,
            true,
            None,
            Some(&stored),
            "k".into(),
        )
        .unwrap();
        assert_eq!(
            r.policy,
            preset_policy(ManagerPolicyPreset::FullProjectControl, Scope::Project)
        );
        assert_eq!(
            r.expected_policy_version, 4,
            "still fenced on the saved row"
        );
        assert!(
            !granted_policy(
                ManagerPolicyPreset::FullProjectControl,
                true,
                Scope::Project,
                Some(&stored)
            )
            .1
        );
    }
}
