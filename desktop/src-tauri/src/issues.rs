//! Issues workspace commands (hub #3010). Each command validates its input,
//! builds the exact request the TUI's Issues pane sends (crates/rsi/src/client.rs
//! `*_issue*`) and forwards one daemon method. The request builders are pure
//! and unit-tested; the daemon validates again and owns the authority.

use serde_json::{Map, Value, json};
use tauri::State;

use crate::{AppState, CmdResult, forward, parse_uuid};

const STATUSES: &[&str] = &["Open", "InProgress", "Closed", "Cancelled"];
const READINESS: &[&str] = &["any", "ready", "blocked"];
const PROVENANCE: &[&str] = &["operator", "session", "idea", "finding"];
const ARCHIVE: &[&str] = &["Active", "Archived", "All"];
const SORTS: &[&str] = &["updated_desc", "display_number_asc", "priority_asc"];
const PAGE_DIRECTIONS: &[&str] = &["forward", "backward"];
const DEP_DIRECTIONS: &[&str] = &["blocked_by", "blocks"];
const MAX_LIMIT: u64 = 256;
const MAX_HANDOFF_LIMIT: u64 = 32;
const MAX_TEXT: usize = 65_536;
const MAX_LABELS: usize = 64;

type Built = Result<Value, String>;

fn object<'a>(v: &'a Value, what: &str) -> Result<&'a Map<String, Value>, String> {
    v.as_object()
        .ok_or_else(|| format!("{what} must be an object"))
}

fn one_of(field: &str, v: &Value, allowed: &[&str]) -> Result<Value, String> {
    match v.as_str() {
        Some(s) if allowed.contains(&s) => Ok(json!(s)),
        _ => Err(format!("{field} must be one of {}", allowed.join(", "))),
    }
}

fn list_of(field: &str, v: &Value, allowed: &[&str]) -> Result<Value, String> {
    let items = v
        .as_array()
        .ok_or_else(|| format!("{field} must be a list"))?;
    let mut out = Vec::new();
    for item in items {
        out.push(one_of(field, item, allowed)?);
    }
    Ok(Value::Array(out))
}

fn priority(field: &str, v: &Value) -> Result<u8, String> {
    match v.as_u64() {
        Some(p) if (1..=4).contains(&p) => Ok(p as u8),
        _ => Err(format!("{field} must be 1..=4")),
    }
}

fn text(field: &str, v: &Value, max: usize) -> Result<String, String> {
    let s = v.as_str().ok_or_else(|| format!("{field} must be text"))?;
    if s.len() > max || s.contains('\0') {
        return Err(format!("{field} is too long or contains NUL"));
    }
    Ok(s.to_string())
}

fn nonblank(field: &str, v: &Value, max: usize) -> Result<String, String> {
    let s = text(field, v, max)?;
    if s.trim().is_empty() {
        return Err(format!("{field} is empty"));
    }
    Ok(s)
}

fn labels(field: &str, v: &Value) -> Result<Value, String> {
    let items = v
        .as_array()
        .ok_or_else(|| format!("{field} must be a list"))?;
    if items.len() > MAX_LABELS {
        return Err(format!("{field} has more than {MAX_LABELS} labels"));
    }
    let mut out = Vec::new();
    for item in items {
        out.push(json!(nonblank(field, item, 128)?));
    }
    Ok(Value::Array(out))
}

fn limit(v: &Value, max: u64) -> Result<u64, String> {
    match v.as_u64() {
        Some(n) if (1..=max).contains(&n) => Ok(n),
        _ => Err(format!("limit must be 1..={max}")),
    }
}

fn idem(v: Option<&Value>) -> Result<String, String> {
    let key = v.ok_or("idempotency_key is required")?;
    let s = key.as_str().ok_or("idempotency_key must be text")?;
    if s.is_empty() || s.len() > 128 || s.contains('\0') {
        return Err("idempotency_key must be 1..=128 bytes without NUL".into());
    }
    Ok(s.to_string())
}

fn row_version(v: Option<&Value>) -> Result<i64, String> {
    match v.and_then(Value::as_i64) {
        Some(n) if n >= 1 => Ok(n),
        _ => Err("expected_row_version must be a positive integer".into()),
    }
}

fn uuid_field(o: &Map<String, Value>, field: &str) -> Result<String, String> {
    let raw = o
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{field} is required"))?;
    parse_uuid(field, raw)
}

fn cursor(v: &Value) -> Result<Value, String> {
    // The daemon owns the cursor shape (deny_unknown_fields); keep it a small object.
    let o = object(v, "cursor")?;
    if o.len() > 5
        || !o
            .values()
            .all(|x| x.is_string() || x.is_number() || x.is_null())
    {
        return Err("cursor is malformed".into());
    }
    Ok(v.clone())
}

/// `ListIssuesPage` (the Local tab's bounded page).
pub fn list_params(request: &Value) -> Built {
    let o = object(request, "request")?;
    let mut out = Map::new();
    out.insert("project_id".into(), json!(uuid_field(o, "project_id")?));
    for (k, v) in o {
        match k.as_str() {
            "project_id" => {}
            "statuses" => {
                out.insert(k.clone(), list_of(k, v, STATUSES)?);
            }
            "priorities" => {
                let items = v.as_array().ok_or("priorities must be a list")?;
                let mut ps = Vec::new();
                for p in items {
                    ps.push(json!(priority("priorities", p)?));
                }
                out.insert(k.clone(), Value::Array(ps));
            }
            "readiness" => {
                out.insert(k.clone(), one_of(k, v, READINESS)?);
            }
            "assignee" if v.is_null() => {}
            "assignee" => {
                out.insert(k.clone(), json!(nonblank(k, v, 256)?));
            }
            "unassigned" => {
                out.insert(
                    k.clone(),
                    json!(v.as_bool().ok_or("unassigned must be true or false")?),
                );
            }
            "labels_all" | "labels_any" => {
                out.insert(k.clone(), labels(k, v)?);
            }
            "provenance" => {
                out.insert(k.clone(), list_of(k, v, PROVENANCE)?);
            }
            "query" if v.is_null() => {}
            "query" => {
                out.insert(k.clone(), json!(nonblank(k, v, 4096)?));
            }
            "archive" => {
                out.insert(k.clone(), one_of(k, v, ARCHIVE)?);
            }
            "sort" => {
                out.insert(k.clone(), one_of(k, v, SORTS)?);
            }
            "direction" => {
                out.insert(k.clone(), one_of(k, v, PAGE_DIRECTIONS)?);
            }
            "cursor" if v.is_null() => {}
            "cursor" => {
                out.insert(k.clone(), cursor(v)?);
            }
            "limit" => {
                out.insert(k.clone(), json!(limit(v, MAX_LIMIT)?));
            }
            other => return Err(format!("unknown filter {other}")),
        }
    }
    if out.contains_key("assignee") && out.get("unassigned") == Some(&json!(true)) {
        return Err("assignee and unassigned cannot both be set".into());
    }
    Ok(Value::Object(out))
}

fn scope(project_id: &str, issue_id: &str) -> Result<Map<String, Value>, String> {
    let mut m = Map::new();
    m.insert(
        "project_id".into(),
        json!(parse_uuid("project_id", project_id)?),
    );
    m.insert("issue_id".into(), json!(parse_uuid("issue_id", issue_id)?));
    Ok(m)
}

pub fn get_params(project_id: &str, issue_id: &str) -> Built {
    Ok(Value::Object(scope(project_id, issue_id)?))
}

/// `CreateIssueV2`: the draft plus a retry identity the UI keeps until the write settles.
pub fn create_params(project_id: &str, draft: &Value) -> Built {
    let o = object(draft, "draft")?;
    let mut out = Map::new();
    out.insert(
        "project_id".into(),
        json!(parse_uuid("project_id", project_id)?),
    );
    out.insert(
        "title".into(),
        json!(nonblank(
            "title",
            o.get("title").ok_or("title is required")?,
            1024
        )?),
    );
    out.insert(
        "body".into(),
        json!(text("body", o.get("body").unwrap_or(&json!("")), MAX_TEXT)?),
    );
    if let Some(p) = o.get("priority").filter(|p| !p.is_null()) {
        out.insert("priority".into(), json!(priority("priority", p)?));
    }
    out.insert(
        "labels".into(),
        labels("labels", o.get("labels").unwrap_or(&json!([])))?,
    );
    if let Some(a) = o.get("assignee").filter(|a| !a.is_null()) {
        out.insert("assignee".into(), json!(nonblank("assignee", a, 256)?));
    }
    out.insert(
        "idempotency_key".into(),
        json!(idem(o.get("idempotency_key"))?),
    );
    Ok(Value::Object(out))
}

/// `UpdateIssue`: `patch` keeps an absent field unchanged and sends `null` for
/// priority / assignee to clear them (the daemon's `NullablePatchV1`).
pub fn update_params(project_id: &str, issue_id: &str, write: &Value) -> Built {
    let o = object(write, "write")?;
    let mut out = scope(project_id, issue_id)?;
    out.insert(
        "expected_row_version".into(),
        json!(row_version(o.get("expected_row_version"))?),
    );
    out.insert(
        "idempotency_key".into(),
        json!(idem(o.get("idempotency_key"))?),
    );
    let patch = object(o.get("patch").ok_or("patch is required")?, "patch")?;
    let mut p = Map::new();
    for (k, v) in patch {
        match k.as_str() {
            "title" => {
                p.insert(k.clone(), json!(nonblank(k, v, 1024)?));
            }
            "body" => {
                p.insert(k.clone(), json!(text(k, v, MAX_TEXT)?));
            }
            "labels" => {
                p.insert(k.clone(), labels(k, v)?);
            }
            "priority" if v.is_null() => {
                p.insert(k.clone(), Value::Null);
            }
            "priority" => {
                p.insert(k.clone(), json!(priority(k, v)?));
            }
            "assignee" if v.is_null() => {
                p.insert(k.clone(), Value::Null);
            }
            "assignee" => {
                p.insert(k.clone(), json!(nonblank(k, v, 256)?));
            }
            other => return Err(format!("unknown patch field {other}")),
        }
    }
    if p.is_empty() {
        return Err("nothing to change".into());
    }
    out.insert("patch".into(), Value::Object(p));
    Ok(Value::Object(out))
}

fn versioned(
    project_id: &str,
    issue_id: &str,
    write: &Value,
) -> Result<Map<String, Value>, String> {
    let o = object(write, "write")?;
    let mut out = scope(project_id, issue_id)?;
    out.insert(
        "expected_row_version".into(),
        json!(row_version(o.get("expected_row_version"))?),
    );
    out.insert(
        "idempotency_key".into(),
        json!(idem(o.get("idempotency_key"))?),
    );
    Ok(out)
}

/// `UpdateIssueStatusV2`, `ArchiveIssue` and `RestoreIssue` share one shape.
pub fn status_params(project_id: &str, issue_id: &str, write: &Value) -> Built {
    let mut out = versioned(project_id, issue_id, write)?;
    let status = write.get("status").ok_or("status is required")?;
    out.insert("status".into(), one_of("status", status, STATUSES)?);
    Ok(Value::Object(out))
}

pub fn archive_params(project_id: &str, issue_id: &str, write: &Value) -> Built {
    Ok(Value::Object(versioned(project_id, issue_id, write)?))
}

pub fn dependencies_params(project_id: &str, issue_id: &str, page: &Value) -> Built {
    let o = object(page, "page")?;
    let mut out = scope(project_id, issue_id)?;
    let dir = o.get("direction").ok_or("direction is required")?;
    out.insert(
        "direction".into(),
        one_of("direction", dir, DEP_DIRECTIONS)?,
    );
    if let Some(d) = o.get("page_direction") {
        out.insert(
            "page_direction".into(),
            one_of("page_direction", d, PAGE_DIRECTIONS)?,
        );
    }
    if let Some(c) = o.get("cursor").filter(|c| !c.is_null()) {
        out.insert("cursor".into(), cursor(c)?);
    }
    if let Some(l) = o.get("limit") {
        out.insert("limit".into(), json!(limit(l, MAX_LIMIT)?));
    }
    Ok(Value::Object(out))
}

pub fn dependency_edit_params(project_id: &str, issue_id: &str, depends_on_id: &str) -> Built {
    let mut out = scope(project_id, issue_id)?;
    let other = parse_uuid("depends_on_id", depends_on_id)?;
    if out["issue_id"] == json!(other) {
        return Err("an Issue cannot depend on itself".into());
    }
    out.insert("depends_on_id".into(), json!(other));
    Ok(Value::Object(out))
}

/// `ListIssueEventsV2` and `ListIssueHandoffs`: ascending pages by sequence.
pub fn history_params(
    project_id: &str,
    issue_id: &str,
    after: Option<i64>,
    limit_in: Option<u64>,
    max: u64,
) -> Built {
    let mut out = scope(project_id, issue_id)?;
    if let Some(a) = after {
        if a < 0 {
            return Err("after_sequence must be nonnegative".into());
        }
        out.insert("after_sequence".into(), json!(a));
    }
    if let Some(l) = limit_in {
        out.insert("limit".into(), json!(limit(&json!(l), max)?));
    }
    Ok(Value::Object(out))
}

#[tauri::command]
pub async fn list_issues_page(state: State<'_, AppState>, request: Value) -> CmdResult {
    forward(&state, "ListIssuesPage", list_params(&request)?).await
}

#[tauri::command]
pub async fn get_issue(
    state: State<'_, AppState>,
    project_id: String,
    issue_id: String,
) -> CmdResult {
    forward(
        &state,
        "GetIssueInProject",
        get_params(&project_id, &issue_id)?,
    )
    .await
}

#[tauri::command]
pub async fn create_issue(
    state: State<'_, AppState>,
    project_id: String,
    draft: Value,
) -> CmdResult {
    forward(&state, "CreateIssueV2", create_params(&project_id, &draft)?).await
}

#[tauri::command]
pub async fn update_issue(
    state: State<'_, AppState>,
    project_id: String,
    issue_id: String,
    write: Value,
) -> CmdResult {
    forward(
        &state,
        "UpdateIssue",
        update_params(&project_id, &issue_id, &write)?,
    )
    .await
}

#[tauri::command]
pub async fn update_issue_status(
    state: State<'_, AppState>,
    project_id: String,
    issue_id: String,
    write: Value,
) -> CmdResult {
    forward(
        &state,
        "UpdateIssueStatusV2",
        status_params(&project_id, &issue_id, &write)?,
    )
    .await
}

#[tauri::command]
pub async fn archive_issue(
    state: State<'_, AppState>,
    project_id: String,
    issue_id: String,
    write: Value,
) -> CmdResult {
    forward(
        &state,
        "ArchiveIssue",
        archive_params(&project_id, &issue_id, &write)?,
    )
    .await
}

#[tauri::command]
pub async fn restore_issue(
    state: State<'_, AppState>,
    project_id: String,
    issue_id: String,
    write: Value,
) -> CmdResult {
    forward(
        &state,
        "RestoreIssue",
        archive_params(&project_id, &issue_id, &write)?,
    )
    .await
}

#[tauri::command]
pub async fn list_issue_dependencies(
    state: State<'_, AppState>,
    project_id: String,
    issue_id: String,
    page: Value,
) -> CmdResult {
    forward(
        &state,
        "ListIssueDependencies",
        dependencies_params(&project_id, &issue_id, &page)?,
    )
    .await
}

#[tauri::command]
pub async fn add_issue_dependency(
    state: State<'_, AppState>,
    project_id: String,
    issue_id: String,
    depends_on_id: String,
) -> CmdResult {
    forward(
        &state,
        "AddIssueDependency",
        dependency_edit_params(&project_id, &issue_id, &depends_on_id)?,
    )
    .await
}

#[tauri::command]
pub async fn remove_issue_dependency(
    state: State<'_, AppState>,
    project_id: String,
    issue_id: String,
    depends_on_id: String,
) -> CmdResult {
    forward(
        &state,
        "RemoveIssueDependency",
        dependency_edit_params(&project_id, &issue_id, &depends_on_id)?,
    )
    .await
}

#[tauri::command]
pub async fn list_issue_events(
    state: State<'_, AppState>,
    project_id: String,
    issue_id: String,
    after_sequence: Option<i64>,
    limit: Option<u64>,
) -> CmdResult {
    forward(
        &state,
        "ListIssueEventsV2",
        history_params(&project_id, &issue_id, after_sequence, limit, MAX_LIMIT)?,
    )
    .await
}

#[tauri::command]
pub async fn list_issue_handoffs(
    state: State<'_, AppState>,
    project_id: String,
    issue_id: String,
    after_sequence: Option<i64>,
    limit: Option<u64>,
) -> CmdResult {
    forward(
        &state,
        "ListIssueHandoffs",
        history_params(
            &project_id,
            &issue_id,
            after_sequence,
            limit,
            MAX_HANDOFF_LIMIT,
        )?,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: &str = "E2CAE1D4-1833-48EB-A7E8-6F1DD480FDAE";
    const I: &str = "11111111-1111-4111-8111-111111111111";

    #[test]
    fn list_request_is_normalised_and_closed() {
        let p = list_params(&json!({
            "project_id": P, "statuses": ["Open", "InProgress"], "priorities": [1, 2],
            "readiness": "ready", "assignee": null, "labels_any": ["operator-request"],
            "archive": "All", "sort": "priority_asc", "direction": "forward", "limit": 64,
            "query": "  ", "cursor": null
        }));
        assert!(p.is_err(), "a blank query is refused");
        let p = list_params(&json!({
            "project_id": P, "statuses": ["Open"], "priorities": [2], "readiness": "blocked",
            "assignee": null, "labels_any": ["a"], "archive": "Archived", "sort": "updated_desc",
            "cursor": null, "limit": 64
        }))
        .unwrap();
        assert_eq!(p["project_id"], "e2cae1d4-1833-48eb-a7e8-6f1dd480fdae");
        assert_eq!(p["readiness"], "blocked");
        assert!(p.get("assignee").is_none() && p.get("cursor").is_none());
        for bad in [
            json!({"project_id": P, "statuses": ["Blocked"]}),
            json!({"project_id": P, "priorities": [5]}),
            json!({"project_id": P, "limit": 257}),
            json!({"project_id": P, "limit": 0}),
            json!({"project_id": P, "sort": "oldest"}),
            json!({"project_id": P, "extra": 1}),
            json!({"project_id": P, "assignee": "me", "unassigned": true}),
            json!({"project_id": "nope"}),
            json!([]),
        ] {
            assert!(list_params(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn create_requires_title_and_key() {
        let ok = create_params(P, &json!({"title": "T", "body": "b", "priority": 2, "labels": ["x"], "assignee": "me", "idempotency_key": "k1"})).unwrap();
        assert_eq!(ok["priority"], 2);
        assert_eq!(ok["idempotency_key"], "k1");
        let minimal = create_params(P, &json!({"title": "T", "idempotency_key": "k"})).unwrap();
        assert_eq!(minimal["body"], "");
        assert_eq!(minimal["labels"], json!([]));
        assert!(create_params(P, &json!({"title": " ", "idempotency_key": "k"})).is_err());
        assert!(create_params(P, &json!({"title": "T"})).is_err());
        assert!(create_params(P, &json!({"title": "T", "idempotency_key": ""})).is_err());
        assert!(
            create_params(
                P,
                &json!({"title": "T", "priority": 0, "idempotency_key": "k"})
            )
            .is_err()
        );
    }

    #[test]
    fn update_patch_distinguishes_clear_from_unchanged() {
        let w = json!({"expected_row_version": 3, "idempotency_key": "k", "patch": {"priority": null, "assignee": "me", "labels": []}});
        let p = update_params(P, I, &w).unwrap();
        assert!(
            p["patch"]["priority"].is_null()
                && p["patch"].as_object().unwrap().contains_key("priority")
        );
        assert_eq!(p["patch"]["assignee"], "me");
        assert!(p["patch"].get("title").is_none());
        assert_eq!(p["expected_row_version"], 3);
        let empty = json!({"expected_row_version": 3, "idempotency_key": "k", "patch": {}});
        assert!(update_params(P, I, &empty).is_err());
        let stale =
            json!({"expected_row_version": 0, "idempotency_key": "k", "patch": {"title": "x"}});
        assert!(update_params(P, I, &stale).is_err());
        let unknown =
            json!({"expected_row_version": 1, "idempotency_key": "k", "patch": {"status": "Open"}});
        assert!(update_params(P, I, &unknown).is_err());
    }

    #[test]
    fn status_archive_and_restore_carry_a_version_and_key() {
        let w = json!({"expected_row_version": 2, "idempotency_key": "k", "status": "Cancelled"});
        assert_eq!(status_params(P, I, &w).unwrap()["status"], "Cancelled");
        assert!(
            status_params(
                P,
                I,
                &json!({"expected_row_version": 2, "idempotency_key": "k", "status": "Blocked"})
            )
            .is_err()
        );
        assert!(
            status_params(
                P,
                I,
                &json!({"expected_row_version": 2, "idempotency_key": "k"})
            )
            .is_err()
        );
        let a = archive_params(
            P,
            I,
            &json!({"expected_row_version": 2, "idempotency_key": "k", "status": "ignored"}),
        )
        .unwrap();
        assert!(a.get("status").is_none());
        assert!(archive_params(P, I, &json!({"idempotency_key": "k"})).is_err());
    }

    #[test]
    fn dependency_requests_are_bounded() {
        let page = json!({"direction": "blocked_by", "page_direction": "backward", "limit": 8, "cursor": {"display_number": 4, "issue_id": I}});
        let p = dependencies_params(P, I, &page).unwrap();
        assert_eq!(p["direction"], "blocked_by");
        assert!(dependencies_params(P, I, &json!({"direction": "sideways"})).is_err());
        assert!(dependencies_params(P, I, &json!({"direction": "blocks", "limit": 999})).is_err());
        let other = "22222222-2222-4222-8222-222222222222";
        assert_eq!(
            dependency_edit_params(P, I, other).unwrap()["depends_on_id"],
            other
        );
        assert!(dependency_edit_params(P, I, I).is_err());
        assert!(dependency_edit_params(P, I, "x").is_err());
    }

    #[test]
    fn history_pages_are_bounded() {
        let e = history_params(P, I, Some(5), Some(64), MAX_LIMIT).unwrap();
        assert_eq!(e["after_sequence"], 5);
        assert!(history_params(P, I, Some(-1), None, MAX_LIMIT).is_err());
        assert!(history_params(P, I, None, Some(33), MAX_HANDOFF_LIMIT).is_err());
        assert!(history_params(P, I, None, Some(32), MAX_HANDOFF_LIMIT).is_ok());
        assert_eq!(get_params(P, I).unwrap()["issue_id"], I);
    }
}
