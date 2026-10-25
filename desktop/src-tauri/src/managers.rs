//! Manager surfaces (hub #3011, slice 9): the manager tree, the harness
//! manager board and decisions, the scope and policy forms, the global manager
//! workspace, its Budget panel and the fleet. Every command builds exactly the
//! request the TUI sends through `crates/rsi/src/client.rs` (the method named
//! next to each builder) and forwards one daemon method. These are operator
//! RPCs (hub ruling #3015): the desktop is an operator surface, so nothing here
//! touches the agent verb lists. The builders are pure and unit-tested; the
//! daemon validates every request again and owns the authority.

use serde_json::{Map, Value, json};
use tauri::State;

use crate::{AppState, CmdResult, forward, parse_uuid};

type Built = Result<Value, String>;

/// `ManagerInspectSectionV2` in its serde spelling.
pub const SECTIONS: &[&str] = &[
    "overview",
    "workers",
    "work",
    "requests",
    "decisions",
    "topology",
    "resources",
    "actions",
    "events",
    "archive",
    "health",
    "migration_allocations",
    "satellites",
    "friction",
    "rulings",
];
/// `MANAGER_TREE_MAX_LIMIT`.
const TREE_MAX_LIMIT: u64 = 200;
/// `ListHarnessManagerScope` page bound.
const SCOPE_MAX_LIMIT: u64 = 64;
/// `HARNESS_MANAGER_MAX_EPICS` / `HARNESS_MANAGER_MAX_GROUPS`.
const MAX_SCOPE_IDS: usize = 32;
/// `MANAGER_DECISION_STALE_PAGE`.
const STALE_PAGE: usize = 128;
const MAX_ANSWER: usize = 16 * 1024;
const MAX_POLICY_BYTES: usize = 64 * 1024;

fn object<'a>(v: &'a Value, what: &str) -> Result<&'a Map<String, Value>, String> {
    v.as_object()
        .ok_or_else(|| format!("{what} must be an object"))
}

fn uuid_at(o: &Map<String, Value>, field: &str) -> Result<String, String> {
    let raw = o
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{field} is required"))?;
    parse_uuid(field, raw)
}

fn int_at(o: &Map<String, Value>, field: &str, min: i64) -> Result<i64, String> {
    match o.get(field).and_then(Value::as_i64) {
        Some(n) if n >= min => Ok(n),
        _ => Err(format!("{field} must be an integer >= {min}")),
    }
}

fn key(raw: &str) -> Result<String, String> {
    if raw.trim().is_empty() || raw.len() > 128 || raw.contains('\0') {
        return Err("idempotency_key must be 1..=128 bytes without NUL".into());
    }
    Ok(raw.to_string())
}

fn key_at(o: &Map<String, Value>) -> Result<String, String> {
    key(o
        .get("idempotency_key")
        .and_then(Value::as_str)
        .ok_or("idempotency_key is required")?)
}

fn uuid_list(field: &str, v: &Value, max: usize) -> Result<Vec<String>, String> {
    let items = v
        .as_array()
        .ok_or_else(|| format!("{field} must be a list"))?;
    if items.len() > max {
        return Err(format!("{field} has more than {max} entries"));
    }
    let mut out: Vec<String> = Vec::new();
    for item in items {
        let id = parse_uuid(field, item.as_str().unwrap_or_default())?;
        if out.contains(&id) {
            return Err(format!("{field} names {id} twice"));
        }
        out.push(id);
    }
    Ok(out)
}

fn cursor(field: &str, v: Option<&str>) -> Result<Option<String>, String> {
    match v {
        None => Ok(None),
        Some(s) if s.is_empty() || s.len() > 512 || s.contains('\0') => {
            Err(format!("{field} is malformed"))
        }
        Some(s) => Ok(Some(s.to_string())),
    }
}

fn policy(v: Option<&Value>) -> Built {
    let p = v.ok_or("policy is required")?;
    object(p, "policy")?;
    if serde_json::to_vec(p).map_or(true, |b| b.len() > MAX_POLICY_BYTES) {
        return Err("policy is too large".into());
    }
    Ok(p.clone())
}

// ---------- reads ----------

/// `GetManagerTree {after?, limit}` (`Client::get_manager_tree`).
pub fn tree_params(after: Option<&str>, limit: u64) -> Built {
    if !(1..=TREE_MAX_LIMIT).contains(&limit) {
        return Err(format!("limit must be 1..={TREE_MAX_LIMIT}"));
    }
    let mut out = json!({ "limit": limit });
    if let Some(a) = cursor("after", after)? {
        if a.len() > 128 {
            return Err("after is malformed".into());
        }
        out["after"] = json!(a);
    }
    Ok(out)
}

/// `{project_id}`: `GetHarnessManager`, `GetHarnessManagerPolicy`.
pub fn project_params(project_id: &str) -> Built {
    Ok(json!({ "project_id": parse_uuid("project_id", project_id)? }))
}

/// `ListHarnessManagerScope {project_id, after_id?, limit}`.
pub fn scope_page_params(project_id: &str, after_id: Option<&str>, limit: u64) -> Built {
    if !(1..=SCOPE_MAX_LIMIT).contains(&limit) {
        return Err(format!("limit must be 1..={SCOPE_MAX_LIMIT}"));
    }
    let mut out = json!({ "project_id": parse_uuid("project_id", project_id)?, "limit": limit });
    if let Some(a) = after_id {
        out["after_id"] = json!(parse_uuid("after_id", a)?);
    }
    Ok(out)
}

/// `GetHarnessManagerState {project_id, query: {section, cursor?, limit}}`.
pub fn state_params(project_id: &str, section: &str, cursor_at: Option<&str>, limit: u64) -> Built {
    if !SECTIONS.contains(&section) {
        return Err(format!("section must be one of {}", SECTIONS.join(", ")));
    }
    if !(1..=128).contains(&limit) {
        return Err("limit must be 1..=128".into());
    }
    let mut query = json!({ "section": section, "limit": limit });
    if let Some(c) = cursor("cursor", cursor_at)? {
        query["cursor"] = json!(c);
    }
    Ok(json!({ "project_id": parse_uuid("project_id", project_id)?, "query": query }))
}

/// `GetGlobalManagerBudget {node_id?}` (`None`: the global node).
pub fn budget_params(node_id: Option<&str>) -> Built {
    Ok(match node_id {
        Some(id) => json!({ "node_id": parse_uuid("node_id", id)? }),
        None => json!({}),
    })
}

/// `GetGlobalDashboardParams::window` in its serde spelling (`DashboardWindow`).
pub const DASHBOARD_WINDOWS: &[&str] = &["1h", "24h", "7d"];

/// `GetGlobalDashboard {window}` (#3366): the window must be one the daemon's
/// `DashboardWindow` accepts (its params deny unknown fields).
pub fn dashboard_params(window: &str) -> Built {
    if DASHBOARD_WINDOWS.contains(&window) {
        Ok(json!({ "window": window }))
    } else {
        Err(format!(
            "window must be one of {}",
            DASHBOARD_WINDOWS.join(", ")
        ))
    }
}

/// `GetPortfolioNode {node_id}`.
pub fn portfolio_node_params(node_id: &str) -> Built {
    Ok(json!({ "node_id": parse_uuid("node_id", node_id)? }))
}

/// `GetManagerNode {project_id, node_id}`.
pub fn area_node_params(project_id: &str, node_id: &str) -> Built {
    Ok(json!({
        "project_id": parse_uuid("project_id", project_id)?,
        "node_id": parse_uuid("node_id", node_id)?,
    }))
}

/// `ListFrictionRollup {window_hours?}` (1..=720).
pub fn friction_params(window_hours: Option<u64>) -> Built {
    match window_hours {
        None => Ok(json!({})),
        Some(h) if (1..=720).contains(&h) => Ok(json!({ "window_hours": h })),
        Some(_) => Err("window_hours must be 1..=720".into()),
    }
}

/// `ListStaleManagerDecisions {project_id?}` with the daemon's default bound.
pub fn stale_params(project_id: Option<&str>) -> Built {
    Ok(match project_id {
        Some(id) => json!({ "project_id": parse_uuid("project_id", id)? }),
        None => json!({}),
    })
}

// ---------- writes ----------

/// `ConfigureHarnessManager`: replace the whole scope. Omitted `epic_ids` with
/// no Groups is the whole project; `epic_ids: []` with no Groups revokes.
pub fn scope_save_params(request: &Value) -> Built {
    let o = object(request, "request")?;
    let mut out = json!({
        "project_id": uuid_at(o, "project_id")?,
        "session_id": uuid_at(o, "session_id")?,
        "expected_row_version": int_at(o, "expected_row_version", 0)?,
    });
    match o.get("epic_ids") {
        None | Some(Value::Null) => {}
        Some(v) => out["epic_ids"] = json!(uuid_list("epic_ids", v, MAX_SCOPE_IDS)?),
    }
    match o.get("group_ids") {
        None | Some(Value::Null) => {}
        Some(v) => {
            let groups = uuid_list("group_ids", v, MAX_SCOPE_IDS)?;
            if !groups.is_empty() {
                out["group_ids"] = json!(groups);
            }
        }
    }
    Ok(out)
}

/// `ConfigureHarnessManagerPolicy`: the complete policy with both fences.
pub fn policy_save_params(request: &Value) -> Built {
    let o = object(request, "request")?;
    Ok(json!({
        "project_id": uuid_at(o, "project_id")?,
        "expected_scope_version": int_at(o, "expected_scope_version", 1)?,
        "expected_policy_version": int_at(o, "expected_policy_version", 0)?,
        "idempotency_key": key_at(o)?,
        "policy": policy(o.get("policy"))?,
    }))
}

/// `AnswerHarnessManagerDecision`: one exact pending record.
pub fn answer_params(request: &Value) -> Built {
    let o = object(request, "request")?;
    let fence = object(o.get("fence").unwrap_or(&Value::Null), "fence")?;
    let answer = o
        .get("answer")
        .and_then(Value::as_str)
        .ok_or("answer is required")?;
    if answer.trim().is_empty() || answer.len() > MAX_ANSWER || answer.contains('\0') {
        return Err("answer must be non-empty text".into());
    }
    let text_at = |field: &str| -> Result<String, String> {
        match o.get(field).and_then(Value::as_str) {
            Some(s) if !s.is_empty() && s.len() <= 512 => Ok(s.to_string()),
            _ => Err(format!("{field} is required")),
        }
    };
    Ok(json!({
        "project_id": uuid_at(o, "project_id")?,
        "fence": {
            "scope_version": int_at(fence, "scope_version", 1)?,
            "policy_version": int_at(fence, "policy_version", 1)?,
        },
        "decision_key": text_at("decision_key")?,
        "expected_row_version": int_at(o, "expected_row_version", 1)?,
        "target_digest": text_at("target_digest")?,
        "answer": answer,
        "idempotency_key": key_at(o)?,
    }))
}

/// `ArchiveStaleManagerDecisions {items, older_than_days}`: the rows a list returned.
pub fn archive_stale_params(request: &Value) -> Built {
    let o = object(request, "request")?;
    let items = o
        .get("items")
        .and_then(Value::as_array)
        .ok_or("items must be a list")?;
    if items.is_empty() || items.len() > STALE_PAGE {
        return Err(format!("items must hold 1..={STALE_PAGE} records"));
    }
    let mut out = Vec::new();
    for item in items {
        let i = object(item, "item")?;
        let k = i.get("key").and_then(Value::as_str).unwrap_or_default();
        if k.is_empty() || k.len() > 512 {
            return Err("item key is required".into());
        }
        out.push(json!({
            "project_id": uuid_at(i, "project_id")?,
            "owner_manager_session_id": uuid_at(i, "owner_manager_session_id")?,
            "scope_version": int_at(i, "scope_version", 1)?,
            "key": k,
            "expected_row_version": int_at(i, "expected_row_version", 1)?,
        }));
    }
    let mut req = json!({ "items": out });
    if let Some(d) = o.get("older_than_days").filter(|v| !v.is_null()) {
        match d.as_u64() {
            Some(n) if (1..=3650).contains(&n) => req["older_than_days"] = json!(n),
            _ => return Err("older_than_days must be 1..=3650".into()),
        }
    }
    Ok(req)
}

/// `RevokeGlobalManager {expected_grant_version, idempotency_key}`.
pub fn revoke_global_params(expected_grant_version: i64, idempotency_key: &str) -> Built {
    if expected_grant_version <= 0 {
        return Err("expected_grant_version must be positive".into());
    }
    Ok(json!({
        "expected_grant_version": expected_grant_version,
        "idempotency_key": key(idempotency_key)?,
    }))
}

/// `RevokePortfolioNode` / `RevokeManagerNode` (area nodes add `project_id`).
pub fn revoke_node_params(request: &Value, area: bool) -> Built {
    let o = object(request, "request")?;
    let mut out = json!({
        "node_id": uuid_at(o, "node_id")?,
        "expected_grant_version": int_at(o, "expected_grant_version", 1)?,
        "expected_authority_epoch": int_at(o, "expected_authority_epoch", 1)?,
        "idempotency_key": key_at(o)?,
    });
    if area {
        out["project_id"] = json!(uuid_at(o, "project_id")?);
    }
    Ok(out)
}

/// `ConfigurePortfolioNode` as the Budget panel sends it
/// (`raise_portfolio_request`): the node's current grant with one cap changed,
/// fenced on its grant version and authority epoch. The panel's review step is
/// the operator's confirmation of a lowered cap (`confirm_cap_reductions`).
pub fn portfolio_configure_params(request: &Value, confirm_cap_reductions: bool) -> Built {
    let o = object(request, "request")?;
    let mut out = Map::new();
    out.insert("node_id".into(), json!(uuid_at(o, "node_id")?));
    match o.get("parent_node_id") {
        None | Some(Value::Null) => {
            out.insert("parent_node_id".into(), Value::Null);
        }
        Some(v) => {
            out.insert(
                "parent_node_id".into(),
                json!(parse_uuid(
                    "parent_node_id",
                    v.as_str().unwrap_or_default()
                )?),
            );
        }
    }
    let tier = o
        .get("tier_label")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty() && s.len() <= 64)
        .ok_or("tier_label is required")?;
    out.insert("tier_label".into(), json!(tier));
    out.insert(
        "seat_session_id".into(),
        json!(uuid_at(o, "seat_session_id")?),
    );
    out.insert(
        "project_ids".into(),
        json!(uuid_list(
            "project_ids",
            o.get("project_ids").unwrap_or(&Value::Null),
            4096
        )?),
    );
    let launches = o
        .get("allowed_launches")
        .filter(|v| v.is_array())
        .ok_or("allowed_launches must be a list")?;
    out.insert("allowed_launches".into(), launches.clone());
    out.insert("policy".into(), policy(o.get("policy"))?);
    match o.get("child_policy") {
        None | Some(Value::Null) => {
            out.insert("child_policy".into(), Value::Null);
        }
        Some(p) => {
            out.insert("child_policy".into(), policy(Some(p))?);
        }
    }
    match o.get("max_direct_reports").and_then(Value::as_u64) {
        Some(n) if n <= u64::from(u16::MAX) => {
            out.insert("max_direct_reports".into(), json!(n));
        }
        _ => return Err("max_direct_reports must be 0..=65535".into()),
    }
    out.insert(
        "expected_node_grant_version".into(),
        json!(int_at(o, "expected_node_grant_version", 1)?),
    );
    out.insert(
        "expected_authority_epoch".into(),
        json!(int_at(o, "expected_authority_epoch", 1)?),
    );
    out.insert("idempotency_key".into(), json!(key_at(o)?));
    if confirm_cap_reductions {
        out.insert("confirm_cap_reductions".into(), json!(true));
    }
    Ok(Value::Object(out))
}

// ---------- commands ----------

#[tauri::command]
pub async fn manager_tree(
    state: State<'_, AppState>,
    after: Option<String>,
    limit: u64,
) -> CmdResult {
    forward(
        &state,
        "GetManagerTree",
        tree_params(after.as_deref(), limit)?,
    )
    .await
}

#[tauri::command]
pub async fn harness_manager_get(state: State<'_, AppState>, project_id: String) -> CmdResult {
    forward(&state, "GetHarnessManager", project_params(&project_id)?).await
}

#[tauri::command]
pub async fn harness_manager_policy_get(
    state: State<'_, AppState>,
    project_id: String,
) -> CmdResult {
    forward(
        &state,
        "GetHarnessManagerPolicy",
        project_params(&project_id)?,
    )
    .await
}

#[tauri::command]
pub async fn harness_manager_scope_page(
    state: State<'_, AppState>,
    project_id: String,
    after_id: Option<String>,
) -> CmdResult {
    forward(
        &state,
        "ListHarnessManagerScope",
        scope_page_params(&project_id, after_id.as_deref(), SCOPE_MAX_LIMIT)?,
    )
    .await
}

#[tauri::command]
pub async fn harness_manager_state(
    state: State<'_, AppState>,
    project_id: String,
    section: String,
    cursor: Option<String>,
    limit: u64,
) -> CmdResult {
    forward(
        &state,
        "GetHarnessManagerState",
        state_params(&project_id, &section, cursor.as_deref(), limit)?,
    )
    .await
}

#[tauri::command]
pub async fn harness_manager_configure_scope(
    state: State<'_, AppState>,
    request: Value,
) -> CmdResult {
    forward(
        &state,
        "ConfigureHarnessManager",
        scope_save_params(&request)?,
    )
    .await
}

#[tauri::command]
pub async fn harness_manager_configure_policy(
    state: State<'_, AppState>,
    request: Value,
) -> CmdResult {
    forward(
        &state,
        "ConfigureHarnessManagerPolicy",
        policy_save_params(&request)?,
    )
    .await
}

#[tauri::command]
pub async fn harness_manager_answer_decision(
    state: State<'_, AppState>,
    request: Value,
) -> CmdResult {
    forward(
        &state,
        "AnswerHarnessManagerDecision",
        answer_params(&request)?,
    )
    .await
}

#[tauri::command]
pub async fn manager_stale_decisions(
    state: State<'_, AppState>,
    project_id: Option<String>,
) -> CmdResult {
    forward(
        &state,
        "ListStaleManagerDecisions",
        stale_params(project_id.as_deref())?,
    )
    .await
}

#[tauri::command]
pub async fn manager_archive_stale_decisions(
    state: State<'_, AppState>,
    request: Value,
) -> CmdResult {
    forward(
        &state,
        "ArchiveStaleManagerDecisions",
        archive_stale_params(&request)?,
    )
    .await
}

#[tauri::command]
pub async fn global_manager_get(state: State<'_, AppState>) -> CmdResult {
    forward(&state, "GetGlobalManager", json!({})).await
}

#[tauri::command]
pub async fn global_manager_workspace(state: State<'_, AppState>) -> CmdResult {
    forward(&state, "GetGlobalManagerWorkspace", json!({})).await
}

#[tauri::command]
pub async fn global_manager_budget(
    state: State<'_, AppState>,
    node_id: Option<String>,
) -> CmdResult {
    forward(
        &state,
        "GetGlobalManagerBudget",
        budget_params(node_id.as_deref())?,
    )
    .await
}

#[tauri::command]
pub async fn global_manager_revoke(
    state: State<'_, AppState>,
    expected_grant_version: i64,
    idempotency_key: String,
) -> CmdResult {
    forward(
        &state,
        "RevokeGlobalManager",
        revoke_global_params(expected_grant_version, &idempotency_key)?,
    )
    .await
}

/// The operator-only global dashboard snapshot behind `gd` / `:dashboard` (#3366).
#[tauri::command]
pub async fn global_dashboard(state: State<'_, AppState>, window: String) -> CmdResult {
    forward(&state, "GetGlobalDashboard", dashboard_params(&window)?).await
}

#[tauri::command]
pub async fn fleet_overview(state: State<'_, AppState>) -> CmdResult {
    forward(&state, "GetFleetOverview", json!({})).await
}

#[tauri::command]
pub async fn portfolio_nodes_list(state: State<'_, AppState>, include_revoked: bool) -> CmdResult {
    forward(
        &state,
        "ListPortfolioNodes",
        json!({ "include_revoked": include_revoked }),
    )
    .await
}

#[tauri::command]
pub async fn portfolio_node_get(state: State<'_, AppState>, node_id: String) -> CmdResult {
    forward(&state, "GetPortfolioNode", portfolio_node_params(&node_id)?).await
}

#[tauri::command]
pub async fn portfolio_node_configure(
    state: State<'_, AppState>,
    request: Value,
    confirm_cap_reductions: bool,
) -> CmdResult {
    forward(
        &state,
        "ConfigurePortfolioNode",
        portfolio_configure_params(&request, confirm_cap_reductions)?,
    )
    .await
}

#[tauri::command]
pub async fn portfolio_node_revoke(state: State<'_, AppState>, request: Value) -> CmdResult {
    forward(
        &state,
        "RevokePortfolioNode",
        revoke_node_params(&request, false)?,
    )
    .await
}

#[tauri::command]
pub async fn area_node_get(
    state: State<'_, AppState>,
    project_id: String,
    node_id: String,
) -> CmdResult {
    forward(
        &state,
        "GetManagerNode",
        area_node_params(&project_id, &node_id)?,
    )
    .await
}

#[tauri::command]
pub async fn area_node_revoke(state: State<'_, AppState>, request: Value) -> CmdResult {
    forward(
        &state,
        "RevokeManagerNode",
        revoke_node_params(&request, true)?,
    )
    .await
}

#[tauri::command]
pub async fn operator_escalations(state: State<'_, AppState>, include_closed: bool) -> CmdResult {
    forward(
        &state,
        "ListOperatorEscalations",
        json!({ "include_closed": include_closed, "undelivered_after": null }),
    )
    .await
}

#[tauri::command]
pub async fn friction_rollup(state: State<'_, AppState>, window_hours: Option<u64>) -> CmdResult {
    forward(&state, "ListFrictionRollup", friction_params(window_hours)?).await
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: &str = "e2cae1d4-1833-48eb-a7e8-6f1dd480fdae";
    const S: &str = "0b7c8a52-5f63-4a9e-9d27-3c1a9a5b6e10";
    const E: &str = "7d0f4c2e-9a1b-4c3d-8e5f-6a7b8c9d0e1f";

    #[test]
    fn tree_pages_are_bounded() {
        assert_eq!(tree_params(None, 200).unwrap(), json!({"limit": 200}));
        assert_eq!(
            tree_params(Some("project:x"), 50).unwrap(),
            json!({"limit": 50, "after": "project:x"})
        );
        assert!(tree_params(None, 0).is_err());
        assert!(tree_params(None, 201).is_err());
        assert!(tree_params(Some(""), 5).is_err());
        assert!(tree_params(Some(&"k".repeat(129)), 5).is_err());
    }

    #[test]
    fn state_query_nests_the_section_and_cursor() {
        assert_eq!(
            state_params(P, "decisions", Some("c1"), 32).unwrap(),
            json!({"project_id": P, "query": {"section": "decisions", "limit": 32, "cursor": "c1"}})
        );
        assert_eq!(
            state_params(P, "overview", None, 8).unwrap()["query"],
            json!({"section": "overview", "limit": 8})
        );
        assert!(state_params(P, "Decisions", None, 8).is_err());
        assert!(state_params("nope", "overview", None, 8).is_err());
        assert!(state_params(P, "overview", None, 0).is_err());
    }

    #[test]
    fn project_scope_and_budget_reads() {
        assert_eq!(project_params(P).unwrap(), json!({"project_id": P}));
        assert_eq!(
            scope_page_params(P, Some(E), 64).unwrap(),
            json!({"project_id": P, "limit": 64, "after_id": E})
        );
        assert!(scope_page_params(P, None, 65).is_err());
        assert_eq!(budget_params(None).unwrap(), json!({}));
        assert_eq!(budget_params(Some(S)).unwrap(), json!({"node_id": S}));
        assert_eq!(
            area_node_params(P, S).unwrap(),
            json!({"project_id": P, "node_id": S})
        );
        assert_eq!(portfolio_node_params(S).unwrap(), json!({"node_id": S}));
        assert_eq!(friction_params(None).unwrap(), json!({}));
        assert_eq!(dashboard_params("24h").unwrap(), json!({"window": "24h"}));
        assert_eq!(dashboard_params("7d").unwrap(), json!({"window": "7d"}));
        assert!(dashboard_params("2h").is_err());
        assert!(dashboard_params("").is_err());
        assert_eq!(
            friction_params(Some(48)).unwrap(),
            json!({"window_hours": 48})
        );
        assert!(friction_params(Some(721)).is_err());
        assert_eq!(stale_params(Some(P)).unwrap(), json!({"project_id": P}));
    }

    #[test]
    fn scope_save_distinguishes_whole_project_selection_and_revocation() {
        let whole = scope_save_params(
            &json!({"project_id": P, "session_id": S, "expected_row_version": 0}),
        )
        .unwrap();
        assert_eq!(
            whole,
            json!({"project_id": P, "session_id": S, "expected_row_version": 0})
        );
        let revoke = scope_save_params(&json!({
            "project_id": P, "session_id": S, "expected_row_version": 3,
            "epic_ids": [], "group_ids": []
        }))
        .unwrap();
        assert_eq!(revoke["epic_ids"], json!([]));
        assert!(revoke.get("group_ids").is_none());
        let picked = scope_save_params(&json!({
            "project_id": P, "session_id": S, "expected_row_version": 3,
            "epic_ids": [E], "group_ids": [S]
        }))
        .unwrap();
        assert_eq!(picked["group_ids"], json!([S]));
        assert!(
            scope_save_params(&json!({
                "project_id": P, "session_id": S, "expected_row_version": 3, "epic_ids": [E, E]
            }))
            .is_err()
        );
        let many: Vec<String> = (0..33)
            .map(|i| format!("00000000-0000-4000-8000-{i:012}"))
            .collect();
        assert!(
            scope_save_params(&json!({
                "project_id": P, "session_id": S, "expected_row_version": 1, "epic_ids": many
            }))
            .is_err()
        );
        assert!(
            scope_save_params(
                &json!({"project_id": P, "session_id": S, "expected_row_version": -1})
            )
            .is_err()
        );
    }

    #[test]
    fn policy_save_carries_both_fences_and_the_whole_policy() {
        let policy = json!({"mode": "execute", "max_active_sessions": 6});
        let built = policy_save_params(&json!({
            "project_id": P, "expected_scope_version": 4, "expected_policy_version": 0,
            "idempotency_key": "k1", "policy": policy
        }))
        .unwrap();
        assert_eq!(
            built,
            json!({
                "project_id": P, "expected_scope_version": 4, "expected_policy_version": 0,
                "idempotency_key": "k1", "policy": {"mode": "execute", "max_active_sessions": 6}
            })
        );
        assert!(
            policy_save_params(&json!({
                "project_id": P, "expected_scope_version": 0, "expected_policy_version": 0,
                "idempotency_key": "k1", "policy": {}
            }))
            .is_err()
        );
        assert!(
            policy_save_params(&json!({
                "project_id": P, "expected_scope_version": 1, "expected_policy_version": 0,
                "idempotency_key": "", "policy": {}
            }))
            .is_err()
        );
        assert!(
            policy_save_params(&json!({
                "project_id": P, "expected_scope_version": 1, "expected_policy_version": 0,
                "idempotency_key": "k", "policy": []
            }))
            .is_err()
        );
    }

    #[test]
    fn an_answer_names_one_exact_record() {
        let req = json!({
            "project_id": P, "fence": {"scope_version": 2, "policy_version": 5},
            "decision_key": "d:1", "expected_row_version": 3, "target_digest": "abc",
            "answer": "B: Call the API", "idempotency_key": "k"
        });
        assert_eq!(answer_params(&req).unwrap(), req);
        let mut blank = req.clone();
        blank["answer"] = json!("  ");
        assert!(answer_params(&blank).is_err());
        let mut no_digest = req.clone();
        no_digest["target_digest"] = json!("");
        assert!(answer_params(&no_digest).is_err());
        let mut no_policy = req;
        no_policy["fence"]["policy_version"] = json!(0);
        assert!(answer_params(&no_policy).is_err());
    }

    #[test]
    fn stale_archive_hands_back_the_listed_identities() {
        let item = json!({
            "project_id": P, "owner_manager_session_id": S, "scope_version": 2,
            "key": "d:1", "expected_row_version": 1
        });
        assert_eq!(
            archive_stale_params(&json!({"items": [item], "older_than_days": 7})).unwrap(),
            json!({"items": [item], "older_than_days": 7})
        );
        assert!(archive_stale_params(&json!({"items": []})).is_err());
        assert!(archive_stale_params(&json!({"items": [item], "older_than_days": 0})).is_err());
    }

    #[test]
    fn revokes_are_fenced() {
        assert_eq!(
            revoke_global_params(3, "k").unwrap(),
            json!({"expected_grant_version": 3, "idempotency_key": "k"})
        );
        assert!(revoke_global_params(0, "k").is_err());
        let node = json!({
            "node_id": S, "expected_grant_version": 2, "expected_authority_epoch": 1,
            "idempotency_key": "k", "project_id": P
        });
        let portfolio = revoke_node_params(&node, false).unwrap();
        assert!(portfolio.get("project_id").is_none());
        assert_eq!(revoke_node_params(&node, true).unwrap(), node);
        let mut stale = node;
        stale["expected_authority_epoch"] = json!(0);
        assert!(revoke_node_params(&stale, false).is_err());
    }

    #[test]
    fn a_budget_raise_resends_the_whole_grant() {
        let req = json!({
            "node_id": S, "parent_node_id": null, "tier_label": "global",
            "seat_session_id": E, "project_ids": [P], "allowed_launches": [],
            "policy": {"max_created_sessions": 64}, "child_policy": null,
            "max_direct_reports": 8, "expected_node_grant_version": 4,
            "expected_authority_epoch": 2, "idempotency_key": "k"
        });
        let mut want = req.clone();
        want["confirm_cap_reductions"] = json!(true);
        assert_eq!(portfolio_configure_params(&req, true).unwrap(), want);
        assert!(
            portfolio_configure_params(&req, false)
                .unwrap()
                .get("confirm_cap_reductions")
                .is_none()
        );
        let mut no_seat = req;
        no_seat["seat_session_id"] = json!("x");
        assert!(portfolio_configure_params(&no_seat, true).is_err());
    }
}
