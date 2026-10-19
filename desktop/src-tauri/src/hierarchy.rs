//! Session hierarchy creation and Epic lead (hub #3000 gap sweep G2): the
//! create-entity form's submit and `gL`. Each command validates its input and
//! forwards exactly one daemon method with the params the TUI client sends
//! (`crates/rsi/src/client.rs`): `CreateContainer` for a Group or Epic,
//! `LaunchSession` (launch_session_with_opts_params) for a leaf kind, and
//! `SetEpicLead`. The daemon re-checks containment and cycles.

use serde::Deserialize;
use serde_json::{Value, json};
use tauri::State;

use crate::{AppState, CmdResult, PROVIDERS, check_text, forward, parse_uuid};

pub const METHOD_CREATE_CONTAINER: &str = "CreateContainer";
pub const METHOD_LAUNCH: &str = "LaunchSession";
pub const METHOD_SET_LEAD: &str = "SetEpicLead";

/// `SessionKind` serde names a leaf may be created as.
pub const LEAF_KINDS: &[&str] = &[
    "Standard", "Story", "Task", "Bug", "Feature", "Refactor", "Research",
];
/// The effort levels a model ladder may offer (low .. ultra).
pub const EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max", "ultra"];
pub const MAX_NAME_CHARS: usize = 200;
pub const MAX_TAGS: usize = 32;
pub const MAX_TAG_BYTES: usize = 64;

fn name_param(raw: &str) -> Result<String, String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err("Name required".into());
    }
    if name.chars().count() > MAX_NAME_CHARS || name.chars().any(char::is_control) {
        return Err(format!(
            "the name must be one line of at most {MAX_NAME_CHARS} characters"
        ));
    }
    Ok(name.to_string())
}

/// `rsi_common::tag::normalize_tag`: trim, lowercase, whitespace runs to `-`,
/// then `^[a-z0-9][a-z0-9\-_/]*$`.
pub fn normalize_tag(raw: &str) -> Result<String, String> {
    let lower = raw.trim().to_lowercase();
    let collapsed = lower.split_whitespace().collect::<Vec<_>>().join("-");
    let mut chars = collapsed.chars();
    let first_ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    let rest_ok = chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "-_/".contains(c));
    if !first_ok || !rest_ok || collapsed.len() > MAX_TAG_BYTES {
        return Err(format!("“{}” is not a valid tag", raw.trim()));
    }
    Ok(collapsed)
}

/// At least one tag (the form's mandatory-tag rule), normalised and de-duplicated.
pub fn tags_param(raw: &[String]) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for t in raw {
        let n = normalize_tag(t)?;
        if !out.contains(&n) {
            out.push(n);
        }
    }
    if out.is_empty() {
        return Err("at least one tag required".into());
    }
    if out.len() > MAX_TAGS {
        return Err(format!("at most {MAX_TAGS} tags"));
    }
    Ok(out)
}

fn optional_uuid(field: &str, raw: Option<&str>) -> Result<Value, String> {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(Value::Null),
        Some(r) => Ok(Value::String(parse_uuid(field, r)?)),
    }
}

/// `CreateContainer` (client.rs create_container). Topology is Epic-only: a
/// Group always passes none, as the TUI's defensive gate does.
pub fn container_params(
    kind: &str,
    name: &str,
    parent_id: Option<&str>,
    project_id: Option<&str>,
    tags: &[String],
    topology_id: Option<&str>,
) -> Result<Value, String> {
    if kind != "Group" && kind != "Epic" {
        return Err(format!("{kind} is not a container kind"));
    }
    let topology = if kind == "Epic" {
        optional_uuid("topology_id", topology_id)?
    } else {
        Value::Null
    };
    Ok(json!({
        "kind": kind,
        "name": name_param(name)?,
        "parent_id": optional_uuid("parent_id", parent_id)?,
        "project_id": optional_uuid("project_id", project_id)?,
        "tags": tags_param(tags)?,
        "topology_id": topology,
    }))
}

#[derive(Debug, Deserialize)]
pub struct LeafLaunch {
    pub kind: String,
    pub title: String,
    pub query: String,
    pub provider: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub working_dir: Option<String>,
    #[serde(default)]
    pub sandbox: bool,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// `LaunchSession` for a leaf kind (create_entity_form.rs leaf_launch_options
/// through client.rs launch_session_with_opts_params): the name is the title,
/// the prompt the query, and a checked Sandbox asks for a git worktree.
pub fn leaf_params(req: &LeafLaunch) -> Result<Value, String> {
    if !LEAF_KINDS.contains(&req.kind.as_str()) {
        return Err(format!("{} is not a leaf kind", req.kind));
    }
    check_text("prompt", &req.query)?;
    if !PROVIDERS.contains(&req.provider.as_str()) {
        return Err(format!("unknown provider {}", req.provider));
    }
    let nonblank = |v: &Option<String>| {
        v.as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let effort = nonblank(&req.effort);
    if let Some(e) = &effort
        && !EFFORTS.contains(&e.as_str())
    {
        return Err(format!("unknown effort {e}"));
    }
    let working_dir = nonblank(&req.working_dir)
        .ok_or_else(|| "a leaf session needs a working directory".to_string())?;
    if !std::path::Path::new(&working_dir).is_absolute() {
        return Err("working directory must be an absolute path".into());
    }
    let sandbox = if req.sandbox {
        json!({ "kind": "GitWorktree", "branch": null })
    } else {
        Value::Null
    };
    Ok(json!({
        "query": req.query,
        "title": name_param(&req.title)?,
        "working_dir": working_dir,
        "provider": req.provider,
        "model": nonblank(&req.model),
        "system_prompt": null,
        "session_kind": req.kind,
        "project_id": optional_uuid("project_id", req.project_id.as_deref())?,
        "max_retries": null,
        "effort": effort,
        "parent_id": optional_uuid("parent_id", req.parent_id.as_deref())?,
        "sandbox": sandbox,
        "tags": tags_param(&req.tags)?,
        "workflow_id": null,
        "workflow_id_override": null,
    }))
}

/// `SetEpicLead` (client.rs set_epic_lead).
pub fn lead_params(epic_id: &str, new_lead_session_id: &str) -> Result<Value, String> {
    let epic = parse_uuid("epic_id", epic_id)?;
    let lead = parse_uuid("new_lead_session_id", new_lead_session_id)?;
    if epic == lead {
        return Err("an Epic cannot lead itself".into());
    }
    Ok(json!({ "epic_id": epic, "new_lead_session_id": lead }))
}

#[tauri::command]
pub async fn create_container(
    state: State<'_, AppState>,
    kind: String,
    name: String,
    parent_id: Option<String>,
    project_id: Option<String>,
    tags: Vec<String>,
    topology_id: Option<String>,
) -> CmdResult {
    let p = container_params(
        &kind,
        &name,
        parent_id.as_deref(),
        project_id.as_deref(),
        &tags,
        topology_id.as_deref(),
    )?;
    forward(&state, METHOD_CREATE_CONTAINER, p).await
}

#[tauri::command]
pub async fn launch_entity(state: State<'_, AppState>, request: LeafLaunch) -> CmdResult {
    forward(&state, METHOD_LAUNCH, leaf_params(&request)?).await
}

#[tauri::command]
pub async fn set_epic_lead(
    state: State<'_, AppState>,
    epic_id: String,
    new_lead_session_id: String,
) -> CmdResult {
    forward(
        &state,
        METHOD_SET_LEAD,
        lead_params(&epic_id, &new_lead_session_id)?,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "E2CAE1D4-1833-48EB-A7E8-6F1DD480FDAE";
    const A_LOWER: &str = "e2cae1d4-1833-48eb-a7e8-6f1dd480fdae";
    const B: &str = "7b1b1d9e-0c7a-4c44-9a36-2f9c8f1a4e10";

    fn tags(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn tags_follow_normalize_tag() {
        assert_eq!(normalize_tag("  Foo Bar ").unwrap(), "foo-bar");
        assert_eq!(normalize_tag("ui/desktop_2").unwrap(), "ui/desktop_2");
        assert!(normalize_tag("-x").is_err());
        assert!(normalize_tag("a.b").is_err());
        assert!(normalize_tag("  ").is_err());
        assert_eq!(tags_param(&tags(&["A", "a", "b"])).unwrap(), vec!["a", "b"]);
        assert_eq!(tags_param(&[]).unwrap_err(), "at least one tag required");
    }

    #[test]
    fn a_group_never_carries_a_topology() {
        let p =
            container_params("Group", " Web ", None, Some(B), &tags(&["web"]), Some(A)).unwrap();
        assert_eq!(
            p,
            json!({
                "kind": "Group", "name": "Web", "parent_id": null, "project_id": B,
                "tags": ["web"], "topology_id": null,
            })
        );
        let p = container_params("Epic", "E", Some(A), None, &tags(&["e"]), Some(A)).unwrap();
        assert_eq!(p["topology_id"], A_LOWER);
        assert_eq!(p["parent_id"], A_LOWER);
        assert!(container_params("Task", "x", None, None, &tags(&["t"]), None).is_err());
        assert!(container_params("Epic", "", None, None, &tags(&["t"]), None).is_err());
        assert!(container_params("Epic", "x", None, None, &[], None).is_err());
        assert!(container_params("Epic", "x", Some("nope"), None, &tags(&["t"]), None).is_err());
    }

    fn leaf() -> LeafLaunch {
        LeafLaunch {
            kind: "Task".into(),
            title: "Fix it".into(),
            query: "do the thing".into(),
            provider: "Claude".into(),
            model: Some(" ".into()),
            effort: Some("xhigh".into()),
            parent_id: Some(A.into()),
            project_id: None,
            working_dir: Some("/srv/web".into()),
            sandbox: true,
            tags: tags(&["bug"]),
        }
    }

    #[test]
    fn a_leaf_launches_with_its_kind_parent_tags_and_sandbox() {
        let p = leaf_params(&leaf()).unwrap();
        assert_eq!(p["session_kind"], "Task");
        assert_eq!(p["title"], "Fix it");
        assert_eq!(p["query"], "do the thing");
        assert_eq!(p["parent_id"], A_LOWER);
        assert!(p["model"].is_null());
        assert_eq!(p["effort"], "xhigh");
        assert_eq!(
            p["sandbox"],
            json!({ "kind": "GitWorktree", "branch": null })
        );
        assert_eq!(p["tags"], json!(["bug"]));
        assert!(p["workflow_id"].is_null() && p["workflow_id_override"].is_null());
        let mut no_box = leaf();
        no_box.sandbox = false;
        assert!(leaf_params(&no_box).unwrap()["sandbox"].is_null());
    }

    #[test]
    fn a_leaf_is_refused_without_its_essentials() {
        let mut l = leaf();
        l.kind = "Epic".into();
        assert!(leaf_params(&l).is_err());
        let mut l = leaf();
        l.query = "  ".into();
        assert!(leaf_params(&l).is_err());
        let mut l = leaf();
        l.working_dir = Some("relative".into());
        assert!(leaf_params(&l).is_err());
        let mut l = leaf();
        l.working_dir = None;
        assert!(leaf_params(&l).is_err());
        let mut l = leaf();
        l.provider = "Nope".into();
        assert!(leaf_params(&l).is_err());
        let mut l = leaf();
        l.effort = Some("huge".into());
        assert!(leaf_params(&l).is_err());
        let mut l = leaf();
        l.tags = Vec::new();
        assert!(leaf_params(&l).is_err());
    }

    #[test]
    fn lead_names_the_epic_and_the_new_lead() {
        assert_eq!(
            lead_params(A, B).unwrap(),
            json!({ "epic_id": A_LOWER, "new_lead_session_id": B })
        );
        assert!(lead_params(A, A).is_err());
        assert!(lead_params("x", B).is_err());
    }
}
