//! Admin overlays, part 2 (slice 13a, hub #3015): the satellite registry,
//! remote settings, legacy scratch adoption and source-worktree settlement.
//! Each command validates its input and forwards exactly one operator method
//! the TUI client (`crates/rsi/src/client.rs`) calls. None of these carries a
//! credential (rsi-common `remote_control` and `satellite` hold ids, labels and
//! paths only); string values still pass the URL user-info mask on the way to
//! the webview.

use serde::Deserialize;
use serde_json::{Map, Value, json};
use tauri::State;

use crate::admin::bounded;
use crate::admin::call;
use crate::daemon::{DaemonClient, DaemonError};
use crate::settings::mask_url_credentials;
use crate::{AppState, CmdResult, forward, parse_uuid};

/// Mask URL user-info in every string of a response (ssh targets, socket paths
/// and remote URLs never need it on screen).
pub fn mask_strings(value: Value) -> Value {
    match value {
        Value::String(s) => Value::String(mask_url_credentials(&s)),
        Value::Array(a) => Value::Array(a.into_iter().map(mask_strings).collect()),
        Value::Object(m) => Value::Object(
            m.into_iter()
                .map(|(k, v)| (k, mask_strings(v)))
                .collect::<Map<_, _>>(),
        ),
        other => other,
    }
}

async fn read(state: &AppState, method: &str, params: Value) -> CmdResult {
    forward(state, method, params).await.map(mask_strings)
}

fn uuid_list(field: &str, ids: &[String], max: usize) -> Result<Vec<String>, String> {
    if ids.len() > max {
        return Err(format!("{field}: at most {max} ids"));
    }
    let mut out: Vec<String> = Vec::with_capacity(ids.len());
    for id in ids {
        let id = parse_uuid(field, id)?;
        if id == "00000000-0000-0000-0000-000000000000" || out.contains(&id) {
            return Err(format!("{field}: a nil or repeated id"));
        }
        out.push(id);
    }
    Ok(out)
}

// ---------- satellite registry ----------

/// `SATELLITE_INBOUND_MAX_ENTRIES`-sized lists (the daemon checks the exact bound).
/// The daemon's real bounds (rsi-common `satellite_dispatch`): a peer's scope
/// holds at most 64 sessions and 64 projects, an inbound policy 32 entries per
/// list. Checked here so a request is refused before any mutation (hub #3111).
const MAX_SCOPE_IDS: usize = rsi_common::satellite_dispatch::SATELLITE_SCOPE_MAX_SESSIONS;
const MAX_INBOUND_IDS: usize = rsi_common::satellite_dispatch::SATELLITE_INBOUND_MAX_ENTRIES;

/// One peer edit (`SatellitePutPeerRequestV1`).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct PeerEdit {
    pub expected_registry_revision: u64,
    pub peer_id: String,
    pub label: String,
    #[serde(default)]
    pub expected_installation_id: Option<String>,
    pub enabled: bool,
    pub read_enabled: bool,
    pub dispatch_enabled: bool,
    #[serde(default)]
    pub repair_quarantine: bool,
}

pub fn peer_params(e: &PeerEdit) -> Result<Value, String> {
    bounded("label", &e.label, 120)?;
    let expected = match e.expected_installation_id.as_deref().map(str::trim) {
        Some(id) if !id.is_empty() => Some(parse_uuid("expected installation id", id)?),
        _ => None,
    };
    Ok(json!({
        "expected_registry_revision": e.expected_registry_revision,
        "peer": {
            "peer_id": parse_uuid("peer_id", &e.peer_id)?,
            "label": e.label,
            "expected_installation_id": expected,
            "enabled": e.enabled,
            "read_enabled": e.read_enabled,
            "dispatch_enabled": e.dispatch_enabled,
        },
        "repair_quarantine": e.repair_quarantine,
    }))
}

/// One link edit (`SatellitePutLinkRequestV1`).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct LinkEdit {
    pub expected_registry_revision: u64,
    pub peer_id: String,
    pub link_id: String,
    pub direction: String,
    pub socket_path: String,
    #[serde(default)]
    pub ssh_target: Option<String>,
    pub trust_reference: String,
    pub enabled: bool,
    pub priority: u8,
}

pub fn link_params(e: &LinkEdit) -> Result<Value, String> {
    if !matches!(
        e.direction.as_str(),
        "dial_home_reverse" | "direct_local_forward"
    ) {
        return Err(format!("{:?} is not a link direction", e.direction));
    }
    bounded("socket path", &e.socket_path, 512)?;
    if e.trust_reference.len() > 256 || e.trust_reference.contains('\0') {
        return Err("trust reference is longer than 256 bytes".into());
    }
    let ssh = e
        .ssh_target
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(s) = ssh {
        bounded("ssh target", s, 256)?;
    }
    Ok(json!({
        "expected_registry_revision": e.expected_registry_revision,
        "peer_id": parse_uuid("peer_id", &e.peer_id)?,
        "link": {
            "link_id": parse_uuid("link_id", &e.link_id)?,
            "direction": e.direction,
            "socket_path": e.socket_path,
            "ssh_target": ssh,
            "trust_reference": e.trust_reference,
            "enabled": e.enabled,
            "priority": e.priority,
        },
    }))
}

pub fn scope_params(
    revision: u64,
    peer_id: &str,
    sessions: &[String],
    projects: &[String],
) -> Result<Value, String> {
    Ok(json!({
        "expected_registry_revision": revision,
        "peer_id": parse_uuid("peer_id", peer_id)?,
        "remote_session_ids": uuid_list("scope session", sessions, MAX_SCOPE_IDS)?,
        "remote_project_ids": uuid_list("scope project", projects, MAX_SCOPE_IDS)?,
    }))
}

pub fn inbound_params(
    hubs: &[String],
    roots: &[String],
    projects: &[String],
) -> Result<Value, String> {
    Ok(json!({ "policy": {
        "allowed_hub_installations": uuid_list("hub installation", hubs, MAX_INBOUND_IDS)?,
        "scope_roots": uuid_list("scope root", roots, MAX_INBOUND_IDS)?,
        "scope_projects": uuid_list("scope project", projects, MAX_INBOUND_IDS)?,
    }}))
}

/// The hub-owned registry (never connects to a peer).
#[tauri::command]
pub async fn list_satellite_peers(state: State<'_, AppState>) -> CmdResult {
    read(&state, "ListSatellitePeers", Value::Null).await
}

/// This daemon's declared inbound scope, resolved against live sessions.
#[tauri::command]
pub async fn get_satellite_scope_status(state: State<'_, AppState>) -> CmdResult {
    read(&state, "GetSatelliteScopeStatus", Value::Null).await
}

#[tauri::command]
pub async fn get_satellite_inbound_policy(state: State<'_, AppState>) -> CmdResult {
    read(&state, "GetSatelliteInboundPolicy", Value::Null).await
}

/// A revision-fenced peer edit; the daemon answers the new `{revision}`.
#[tauri::command]
pub async fn put_satellite_peer(state: State<'_, AppState>, edit: PeerEdit) -> CmdResult {
    forward(&state, "PutSatellitePeer", peer_params(&edit)?).await
}

/// One peer edit and its dispatch scope, saved together (hub #3111).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct PeerSave {
    pub edit: PeerEdit,
    pub sessions: Vec<String>,
    pub projects: Vec<String>,
    /// The scope the form was opened with; `None` for a new peer.
    #[serde(default)]
    pub previous_sessions: Option<Vec<String>>,
    #[serde(default)]
    pub previous_projects: Option<Vec<String>>,
}

fn same_ids(field: &str, now: &[String], before: &Option<Vec<String>>) -> bool {
    let Some(before) = before else { return false };
    let norm = |ids: &[String]| -> Option<Vec<String>> {
        let mut out = uuid_list(field, ids, usize::MAX).ok()?;
        out.sort();
        Some(out)
    };
    norm(now).is_some() && norm(now) == norm(before)
}

/// Save a peer edit and its scope so a refused scope can never leave the old
/// scope newly authorised. Everything is validated against the daemon's bounds
/// before the first call. An unchanged scope is one `PutSatellitePeer`. A
/// changed scope runs: the peer with every access flag off, then the scope,
/// then the requested access; a failure after the first call leaves access
/// off and says so, and the form reloads the registry.
pub async fn save_peer(client: &DaemonClient, save: &PeerSave) -> CmdResult {
    let requested = peer_params(&save.edit)?;
    scope_params(
        save.edit.expected_registry_revision,
        &save.edit.peer_id,
        &save.sessions,
        &save.projects,
    )?;
    if same_ids("scope session", &save.sessions, &save.previous_sessions)
        && same_ids("scope project", &save.projects, &save.previous_projects)
    {
        let revision = call(client, "PutSatellitePeer", requested).await?;
        return Ok(json!({ "revision": revision_of(&revision), "steps": ["peer"] }));
    }
    let staged = PeerEdit {
        enabled: false,
        read_enabled: false,
        dispatch_enabled: false,
        repair_quarantine: false,
        ..save.edit.clone()
    };
    let r1 = match client.call("PutSatellitePeer", peer_params(&staged)?).await {
        Ok(v) => revision_of(&v),
        Err(e) if refused(&e) => {
            return Err(format!(
                "The daemon refused the peer edit ({e}); nothing was saved."
            ));
        }
        Err(e) => {
            return Err(format!(
                "Whether the peer edit (step 1, access off) was saved is unknown ({e}). {}",
                read_back(client, &save.edit.peer_id).await
            ));
        }
    };
    let scope = scope_params(r1, &save.edit.peer_id, &save.sessions, &save.projects)?;
    let r2 = match client.call("PutSatellitePeerScope", scope).await {
        Ok(v) => revision_of(&v),
        Err(e) => {
            let what = if refused(&e) {
                format!("The daemon refused the new dispatch scope ({e}).")
            } else {
                format!("Whether the new dispatch scope was saved is unknown ({e}).")
            };
            return Err(format!(
                "Step 1 saved the peer with access off (registry revision {r1}). {what} {}",
                read_back(client, &save.edit.peer_id).await
            ));
        }
    };
    let wants_access = save.edit.enabled
        || save.edit.read_enabled
        || save.edit.dispatch_enabled
        || save.edit.repair_quarantine;
    if !wants_access {
        return Ok(json!({ "revision": r2, "steps": ["peer_access_off", "scope"] }));
    }
    let mut finish = save.edit.clone();
    finish.expected_registry_revision = r2;
    match client.call("PutSatellitePeer", peer_params(&finish)?).await {
        Ok(v) => Ok(
            json!({ "revision": revision_of(&v), "steps": ["peer_access_off", "scope", "peer"] }),
        ),
        Err(e) => {
            let what = if refused(&e) {
                format!("The daemon refused turning access back on ({e}).")
            } else {
                format!("Whether access was turned back on is unknown ({e}).")
            };
            Err(format!(
                "Steps 1-2 saved the peer with access off and the new scope (registry revision {r2}). {what} {}",
                read_back(client, &save.edit.peer_id).await
            ))
        }
    }
}

/// A JSON-RPC error reply, or a socket that never connected, proves the write
/// did not commit. Anything after the request was sent (timeout, closed
/// connection, a garbled reply) leaves its outcome unknown (hub #3121).
fn refused(e: &DaemonError) -> bool {
    matches!(e, DaemonError::Rpc { .. } | DaemonError::Connect(_))
}

/// What the registry holds for the peer now, so a failure message never claims
/// a state it did not observe (hub #3121).
async fn read_back(client: &DaemonClient, peer_id: &str) -> String {
    match client.call("ListSatellitePeers", Value::Null).await {
        Ok(view) => {
            let revision = view["revision"]
                .as_u64()
                .map_or_else(|| "unknown".to_string(), |r| r.to_string());
            let peer = view["peers"].as_array().and_then(|peers| {
                peers.iter().find(|p| {
                    p["config"]["peer_id"]
                        .as_str()
                        .is_some_and(|id| id.eq_ignore_ascii_case(peer_id))
                })
            });
            match peer {
                Some(p) => {
                    let c = &p["config"];
                    format!(
                        "Read back now (registry revision {revision}): enabled {}, reads {}, dispatch {}.",
                        c["enabled"], c["read_enabled"], c["dispatch_enabled"]
                    )
                }
                None => format!(
                    "Read back now (registry revision {revision}): the peer is not in the registry."
                ),
            }
        }
        Err(e) => format!(
            "Reading the registry back failed too ({e}), so its current state is unknown: reopen Satellites to check."
        ),
    }
}

/// `PutSatellitePeer`/`PutSatellitePeerScope` answer with the new registry
/// revision, bare or as `{revision}`.
fn revision_of(v: &Value) -> u64 {
    v.as_u64()
        .or_else(|| v.get("revision").and_then(Value::as_u64))
        .unwrap_or_default()
}

#[tauri::command]
pub async fn save_satellite_peer(state: State<'_, AppState>, save: PeerSave) -> CmdResult {
    save_peer(&state.client, &save).await
}

/// Replace one peer's dispatch scope (sessions and `project:` entries).
#[tauri::command]
pub async fn put_satellite_peer_scope(
    state: State<'_, AppState>,
    revision: u64,
    peer_id: String,
    sessions: Vec<String>,
    projects: Vec<String>,
) -> CmdResult {
    let params = scope_params(revision, &peer_id, &sessions, &projects)?;
    forward(&state, "PutSatellitePeerScope", params).await
}

#[tauri::command]
pub async fn put_satellite_inbound_policy(
    state: State<'_, AppState>,
    hubs: Vec<String>,
    roots: Vec<String>,
    projects: Vec<String>,
) -> CmdResult {
    let params = inbound_params(&hubs, &roots, &projects)?;
    forward(&state, "PutSatelliteInboundPolicy", params).await
}

#[tauri::command]
pub async fn put_satellite_link(state: State<'_, AppState>, edit: LinkEdit) -> CmdResult {
    forward(&state, "PutSatelliteLink", link_params(&edit)?).await
}

/// Probe a link without changing the registry.
#[tauri::command]
pub async fn probe_satellite_link(
    state: State<'_, AppState>,
    peer_id: String,
    link_id: String,
) -> CmdResult {
    let params = json!({
        "peer_id": parse_uuid("peer_id", &peer_id)?,
        "link_id": parse_uuid("link_id", &link_id)?,
    });
    read(&state, "ProbeSatelliteLink", params).await
}

/// One page (30, as the TUI) of a peer's cached remote sessions.
#[tauri::command]
pub async fn list_hub_satellite_sessions(
    state: State<'_, AppState>,
    peer_id: String,
    offset: u32,
) -> CmdResult {
    let params =
        json!({ "peer_id": parse_uuid("peer_id", &peer_id)?, "offset": offset, "limit": 30 });
    read(&state, "ListHubSatelliteSessions", params).await
}

// ---------- remote settings ----------

/// `MAX_REMOTE_PROJECTS`.
pub const MAX_REMOTE_PROJECTS: usize = 32;

/// A partial `RemoteSetConfigRequestV1`: absent fields keep their value.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RemoteEdit {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub allowed_node_ids: Option<Vec<String>>,
    #[serde(default)]
    pub project_ids: Option<Vec<String>>,
    #[serde(default)]
    pub allow_answers: Option<bool>,
    #[serde(default)]
    pub allow_messages: Option<bool>,
}

fn plain_ids(field: &str, ids: &[String], max: usize) -> Result<(), String> {
    if ids.len() > max {
        return Err(format!("{field}: at most {max}"));
    }
    for id in ids {
        bounded(field, id, 128)?;
        if id.chars().any(char::is_control) {
            return Err(format!("{field}: an id holds a control character"));
        }
    }
    Ok(())
}

pub fn remote_params(e: &RemoteEdit) -> Result<Value, String> {
    let mut p = Map::new();
    if let Some(v) = e.enabled {
        p.insert("enabled".into(), json!(v));
    }
    if let Some(ids) = &e.allowed_node_ids {
        plain_ids("device", ids, 256)?;
        p.insert("allowed_node_ids".into(), json!(ids));
    }
    if let Some(ids) = &e.project_ids {
        plain_ids("project", ids, MAX_REMOTE_PROJECTS)?;
        p.insert("project_ids".into(), json!(ids));
    }
    if let Some(v) = e.allow_answers {
        p.insert("allow_answers".into(), json!(v));
    }
    if let Some(v) = e.allow_messages {
        p.insert("allow_messages".into(), json!(v));
    }
    Ok(Value::Object(p))
}

#[tauri::command]
pub async fn remote_get_status(state: State<'_, AppState>) -> CmdResult {
    read(&state, "RemoteGetStatus", Value::Null).await
}

/// Edit Remote; an empty edit re-converges the gateway and serve route (`a`).
#[tauri::command]
pub async fn remote_set_config(state: State<'_, AppState>, edit: RemoteEdit) -> CmdResult {
    let params = remote_params(&edit)?;
    forward(&state, "RemoteSetConfig", params)
        .await
        .map(mask_strings)
}

// ---------- legacy scratch adoption ----------

/// `MAX_ADOPT_PATHS`.
pub const MAX_ADOPT_PATHS: usize = 32;

pub fn adopt_params(paths: &[String]) -> Result<Value, String> {
    if paths.is_empty() || paths.len() > MAX_ADOPT_PATHS {
        return Err(format!(
            "adopt 1 to {MAX_ADOPT_PATHS} directories at a time"
        ));
    }
    for p in paths {
        bounded("path", p, 4096)?;
        if !p.starts_with('/') {
            return Err(format!("{p:?} is not an absolute path"));
        }
    }
    Ok(json!({ "paths": paths }))
}

#[tauri::command]
pub async fn list_legacy_scratch(state: State<'_, AppState>) -> CmdResult {
    read(&state, "ListLegacyScratch", Value::Null).await
}

/// Record exactly the confirmed directories (deletes nothing itself).
#[tauri::command]
pub async fn adopt_legacy_scratch(state: State<'_, AppState>, paths: Vec<String>) -> CmdResult {
    read(&state, "AdoptLegacyScratch", adopt_params(&paths)?).await
}

// ---------- source-worktree settlement ----------

/// One apply, as the overlay sends it after the exact phrase was typed.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct SettlementApply {
    pub repository_identity: String,
    pub plan_digest: String,
    pub authorization: String,
    pub idempotency_key: String,
}

fn valid_identity(identity: &str) -> Result<(), String> {
    bounded("repository identity", identity, 4096)?;
    if identity.chars().any(char::is_control) {
        return Err("repository identity holds a control character".into());
    }
    Ok(())
}

/// `ApplySourceWorktreeCohortParams::validate`: the typed phrase must be
/// exactly `APPLY <identity> <digest>`; the key is `[A-Za-z0-9._:-]{1,128}`.
pub fn apply_params(a: &SettlementApply) -> Result<Value, String> {
    valid_identity(&a.repository_identity)?;
    // `Sha256Digest::parse`: `sha256:` and 64 lowercase hex characters.
    let digest_ok = a.plan_digest.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    });
    if !digest_ok {
        return Err("plan digest must be sha256: and 64 lowercase hex characters".into());
    }
    let key_ok = !a.idempotency_key.is_empty()
        && a.idempotency_key.len() <= 128
        && a.idempotency_key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b));
    if !key_ok {
        return Err("idempotency key must be 1-128 of A-Z a-z 0-9 . _ : -".into());
    }
    if a.authorization != format!("APPLY {} {}", a.repository_identity, a.plan_digest) {
        return Err("Authorization phrase does not match exactly".into());
    }
    Ok(json!({
        "repository_identity": a.repository_identity,
        "plan_digest": a.plan_digest,
        "authorization": a.authorization,
        "idempotency_key": a.idempotency_key,
    }))
}

#[tauri::command]
pub async fn list_source_worktree_cohorts(state: State<'_, AppState>) -> CmdResult {
    read(&state, "ListSourceWorktreeCohorts", json!({})).await
}

/// The 20 latest settlement runs (the TUI's receipt list).
#[tauri::command]
pub async fn list_source_worktree_settlement_runs(state: State<'_, AppState>) -> CmdResult {
    read(
        &state,
        "ListSourceWorktreeSettlementRuns",
        json!({ "limit": 20 }),
    )
    .await
}

/// A read-only audit of one cohort (writes nothing).
#[tauri::command]
pub async fn audit_source_worktree_cohort(
    state: State<'_, AppState>,
    repository_identity: String,
) -> CmdResult {
    valid_identity(&repository_identity)?;
    read(
        &state,
        "AuditSourceWorktreeCohort",
        json!({ "repository_identity": repository_identity }),
    )
    .await
}

#[tauri::command]
pub async fn apply_source_worktree_cohort(
    state: State<'_, AppState>,
    apply: SettlementApply,
) -> CmdResult {
    read(&state, "ApplySourceWorktreeCohort", apply_params(&apply)?).await
}

#[tauri::command]
pub async fn get_source_worktree_settlement_run(
    state: State<'_, AppState>,
    run_id: String,
) -> CmdResult {
    let params = json!({ "run_id": parse_uuid("run_id", &run_id)? });
    read(&state, "GetSourceWorktreeSettlementRun", params).await
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "E2CAE1D4-1833-48EB-A7E8-6F1DD480FDAE";
    const LA: &str = "e2cae1d4-1833-48eb-a7e8-6f1dd480fdae";
    const B: &str = "7b0c9e7e-8a8b-4c1e-9f57-3f5c2b6a9d10";
    const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn peer() -> PeerEdit {
        PeerEdit {
            expected_registry_revision: 7,
            peer_id: A.into(),
            label: "mac-mini".into(),
            expected_installation_id: Some(" ".into()),
            enabled: true,
            read_enabled: true,
            dispatch_enabled: false,
            repair_quarantine: false,
        }
    }

    #[test]
    fn peer_params_match_the_tui_request() {
        assert_eq!(
            peer_params(&peer()).unwrap(),
            json!({
                "expected_registry_revision": 7,
                "peer": {
                    "peer_id": LA, "label": "mac-mini", "expected_installation_id": null,
                    "enabled": true, "read_enabled": true, "dispatch_enabled": false,
                },
                "repair_quarantine": false,
            })
        );
        let p = peer_params(&PeerEdit {
            expected_installation_id: Some(B.into()),
            repair_quarantine: true,
            ..peer()
        })
        .unwrap();
        assert_eq!(p["peer"]["expected_installation_id"], B);
        assert_eq!(p["repair_quarantine"], true);
        assert!(
            peer_params(&PeerEdit {
                label: " ".into(),
                ..peer()
            })
            .is_err()
        );
        assert!(
            peer_params(&PeerEdit {
                expected_installation_id: Some("x".into()),
                ..peer()
            })
            .is_err()
        );
    }

    fn link() -> LinkEdit {
        LinkEdit {
            expected_registry_revision: 3,
            peer_id: A.into(),
            link_id: B.into(),
            direction: "dial_home_reverse".into(),
            socket_path: "/run/rsi/mini.sock".into(),
            ssh_target: Some("".into()),
            trust_reference: "known_hosts:mini".into(),
            enabled: false,
            priority: 2,
        }
    }

    #[test]
    fn link_params_match_the_tui_request() {
        assert_eq!(
            link_params(&link()).unwrap(),
            json!({
                "expected_registry_revision": 3,
                "peer_id": LA,
                "link": {
                    "link_id": B, "direction": "dial_home_reverse", "socket_path": "/run/rsi/mini.sock",
                    "ssh_target": null, "trust_reference": "known_hosts:mini", "enabled": false, "priority": 2,
                },
            })
        );
        assert_eq!(
            link_params(&LinkEdit {
                ssh_target: Some("rsi@mini".into()),
                ..link()
            })
            .unwrap()["link"]["ssh_target"],
            "rsi@mini"
        );
        assert!(
            link_params(&LinkEdit {
                direction: "sideways".into(),
                ..link()
            })
            .is_err()
        );
        assert!(
            link_params(&LinkEdit {
                socket_path: String::new(),
                ..link()
            })
            .is_err()
        );
    }

    #[test]
    fn scope_and_inbound_params_take_canonical_unique_ids() {
        assert_eq!(
            scope_params(8, A, &[B.into()], &[A.into()]).unwrap(),
            json!({
                "expected_registry_revision": 8, "peer_id": LA,
                "remote_session_ids": [B], "remote_project_ids": [LA],
            })
        );
        assert!(scope_params(8, A, &[B.into(), B.into()], &[]).is_err());
        assert!(scope_params(8, A, &["00000000-0000-0000-0000-000000000000".into()], &[]).is_err());
        assert_eq!(
            inbound_params(&[A.into()], &[], &[B.into()]).unwrap(),
            json!({ "policy": { "allowed_hub_installations": [LA], "scope_roots": [], "scope_projects": [B] } })
        );
        assert!(inbound_params(&["nope".into()], &[], &[]).is_err());
    }

    #[test]
    fn remote_params_send_only_the_changed_fields() {
        assert_eq!(remote_params(&RemoteEdit::default()).unwrap(), json!({}));
        assert_eq!(
            remote_params(&RemoteEdit {
                enabled: Some(true),
                ..Default::default()
            })
            .unwrap(),
            json!({ "enabled": true })
        );
        assert_eq!(
            remote_params(&RemoteEdit {
                allowed_node_ids: Some(vec!["nodeA".into()]),
                project_ids: Some(vec![]),
                allow_answers: Some(false),
                allow_messages: Some(true),
                ..Default::default()
            })
            .unwrap(),
            json!({ "allowed_node_ids": ["nodeA"], "project_ids": [], "allow_answers": false, "allow_messages": true })
        );
        let many = (0..=MAX_REMOTE_PROJECTS).map(|i| format!("p{i}")).collect();
        assert!(
            remote_params(&RemoteEdit {
                project_ids: Some(many),
                ..Default::default()
            })
            .is_err()
        );
        // The owner id and host are auto-detected: the request type refuses them.
        assert!(
            serde_json::from_value::<RemoteEdit>(json!({ "canonical_host": "x.ts.net" })).is_err()
        );
    }

    #[test]
    fn adopt_params_take_1_to_32_absolute_paths() {
        assert_eq!(
            adopt_params(&["/tmp/rsi-a".into()]).unwrap(),
            json!({ "paths": ["/tmp/rsi-a"] })
        );
        assert!(adopt_params(&[]).is_err());
        assert!(adopt_params(&["tmp/x".into()]).is_err());
        let many: Vec<String> = (0..=MAX_ADOPT_PATHS).map(|i| format!("/tmp/{i}")).collect();
        assert!(adopt_params(&many).is_err());
    }

    fn apply() -> SettlementApply {
        SettlementApply {
            repository_identity: "github.com/o/r".into(),
            plan_digest: DIGEST.into(),
            authorization: format!("APPLY github.com/o/r {DIGEST}"),
            idempotency_key: "desktop-7b0c9e7e-8a8b-4c1e-9f57-3f5c2b6a9d10".into(),
        }
    }

    #[test]
    fn apply_params_need_the_exact_phrase() {
        assert_eq!(
            apply_params(&apply()).unwrap(),
            json!({
                "repository_identity": "github.com/o/r",
                "plan_digest": DIGEST,
                "authorization": format!("APPLY github.com/o/r {DIGEST}"),
                "idempotency_key": "desktop-7b0c9e7e-8a8b-4c1e-9f57-3f5c2b6a9d10",
            })
        );
        for bad in [
            SettlementApply {
                authorization: format!("apply github.com/o/r {DIGEST}"),
                ..apply()
            },
            SettlementApply {
                authorization: format!("APPLY github.com/o/r {DIGEST} "),
                ..apply()
            },
            SettlementApply {
                plan_digest: "ABC".into(),
                ..apply()
            },
            SettlementApply {
                plan_digest: DIGEST.trim_start_matches("sha256:").into(),
                ..apply()
            },
            SettlementApply {
                idempotency_key: "has space".into(),
                ..apply()
            },
            SettlementApply {
                idempotency_key: String::new(),
                ..apply()
            },
        ] {
            assert!(apply_params(&bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn responses_have_url_userinfo_masked() {
        let out = mask_strings(json!({
            "url": "https://op:pw@host.ts.net/",
            "links": [{ "ssh_target": "rsi@mini", "socket_path": "/run/x.sock" }],
            "n": 3,
        }));
        assert!(!out.to_string().contains("pw@"));
        assert_eq!(out["links"][0]["ssh_target"], "rsi@mini");
        assert_eq!(out["n"], 3);
    }
}

#[cfg(all(test, unix))]
mod peer_save_tests {
    use super::*;
    use crate::admin::tests::round_trip::fake_daemon;

    const PEER: &str = "5f0d3c1e-7a44-4c2b-9d4e-2b8f7a9c1d00";
    const OLD: &str = "11111111-2222-4333-8444-555555555555";
    const NEW: &str = "66666666-7777-4888-9999-aaaaaaaaaaaa";

    fn edit(enabled: bool) -> PeerEdit {
        PeerEdit {
            expected_registry_revision: 7,
            peer_id: PEER.into(),
            label: "laptop".into(),
            expected_installation_id: None,
            enabled,
            read_enabled: enabled,
            dispatch_enabled: enabled,
            repair_quarantine: false,
        }
    }

    fn save(enabled: bool, sessions: &[&str], previous: Option<&[&str]>) -> PeerSave {
        let v = |ids: &[&str]| ids.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        PeerSave {
            edit: edit(enabled),
            sessions: v(sessions),
            projects: vec![],
            previous_sessions: previous.map(v),
            previous_projects: previous.map(|_| vec![]),
        }
    }

    fn registry(revision: u64, enabled: bool, read: bool, dispatch: bool) -> Value {
        json!({ "revision": revision, "peers": [{ "config": { "peer_id": PEER, "label": "laptop",
            "enabled": enabled, "read_enabled": read, "dispatch_enabled": dispatch } }] })
    }

    /// A registry that bumps its revision per write and refuses every scope write.
    fn scope_refused(method: &str, params: &Value) -> Value {
        match method {
            "PutSatellitePeerScope" => json!({ "__rpc_error": "invalid satellite dispatch scope" }),
            "ListSatellitePeers" => registry(8, false, false, false),
            _ => json!({ "revision": params["expected_registry_revision"].as_u64().unwrap() + 1 }),
        }
    }

    /// A stateful registry: a revision fence on every write, the peer's access
    /// flags, and a twist applied when the final access write arrives (hub #3121).
    #[derive(Clone, Copy)]
    enum Twist {
        /// Commit the final write, then close the connection without a reply.
        CommitThenDrop,
        /// Another operator writes first (enabled + reads, no dispatch), so the
        /// final write's fence is stale.
        ConcurrentWriter,
    }
    struct Registry {
        revision: u64,
        flags: (bool, bool, bool),
    }
    fn stateful(
        state: &std::sync::Mutex<Registry>,
        twist: Twist,
        method: &str,
        params: &Value,
    ) -> Value {
        let mut r = state.lock().unwrap();
        match method {
            "ListSatellitePeers" => registry(r.revision, r.flags.0, r.flags.1, r.flags.2),
            "PutSatellitePeer" | "PutSatellitePeerScope" => {
                let wants_access = params["peer"]["enabled"] == true;
                if method == "PutSatellitePeer"
                    && wants_access
                    && matches!(twist, Twist::ConcurrentWriter)
                {
                    r.revision += 1;
                    r.flags = (true, true, false);
                }
                if params["expected_registry_revision"].as_u64() != Some(r.revision) {
                    return json!({ "__rpc_error": "satellite registry revision conflict" });
                }
                r.revision += 1;
                if method == "PutSatellitePeer" {
                    let p = &params["peer"];
                    r.flags = (
                        p["enabled"] == true,
                        p["read_enabled"] == true,
                        p["dispatch_enabled"] == true,
                    );
                    if wants_access && matches!(twist, Twist::CommitThenDrop) {
                        return json!({ "__drop": true });
                    }
                }
                json!({ "revision": r.revision })
            }
            _ => Value::Null,
        }
    }
    static DROP_STATE: std::sync::Mutex<Registry> = std::sync::Mutex::new(Registry {
        revision: 7,
        flags: (false, false, false),
    });
    fn enable_reply_lost(method: &str, params: &Value) -> Value {
        stateful(&DROP_STATE, Twist::CommitThenDrop, method, params)
    }
    static RACE_STATE: std::sync::Mutex<Registry> = std::sync::Mutex::new(Registry {
        revision: 7,
        flags: (false, false, false),
    });
    fn concurrent_revision(method: &str, params: &Value) -> Value {
        stateful(&RACE_STATE, Twist::ConcurrentWriter, method, params)
    }

    fn accepting(_method: &str, params: &Value) -> Value {
        json!({ "revision": params["expected_registry_revision"].as_u64().unwrap() + 1 })
    }

    fn methods(log: &crate::admin::tests::round_trip::Log) -> Vec<(String, Value)> {
        log.lock()
            .unwrap()
            .iter()
            .map(|r| {
                (
                    r["method"].as_str().unwrap().to_string(),
                    r["params"].clone(),
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn a_refused_scope_leaves_the_peer_with_access_off_never_its_old_scope_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let (client, log) = fake_daemon(dir.path(), scope_refused);
        // A disabled peer with old scope OLD; enable everything while replacing the scope with NEW.
        let err = save_peer(&client, &save(true, &[NEW], Some(&[OLD])))
            .await
            .unwrap_err();
        assert!(
            err.contains("Step 1 saved the peer with access off (registry revision 8)"),
            "{err}"
        );
        assert!(
            err.contains("The daemon refused the new dispatch scope"),
            "{err}"
        );
        assert!(
            err.contains(
                "Read back now (registry revision 8): enabled false, reads false, dispatch false."
            ),
            "{err}"
        );
        let sent = methods(&log);
        assert_eq!(sent.len(), 3, "{sent:?}");
        assert_eq!(sent[2].0, "ListSatellitePeers");
        assert_eq!(sent[0].0, "PutSatellitePeer");
        let peer = &sent[0].1["peer"];
        assert_eq!(
            (
                peer["enabled"].clone(),
                peer["read_enabled"].clone(),
                peer["dispatch_enabled"].clone()
            ),
            (json!(false), json!(false), json!(false))
        );
        assert_eq!(sent[1].0, "PutSatellitePeerScope");
        assert_eq!(sent[1].1["expected_registry_revision"], 8);
        // No later call turns access on.
        assert!(
            sent.iter()
                .all(|(m, p)| m != "PutSatellitePeer" || p["peer"]["enabled"] == false)
        );
    }

    #[tokio::test]
    async fn an_invalid_scope_is_refused_before_any_call() {
        let dir = tempfile::tempdir().unwrap();
        let (client, log) = fake_daemon(dir.path(), accepting);
        let nil = "00000000-0000-0000-0000-000000000000";
        let many: Vec<String> = (0..65)
            .map(|i| format!("00000000-0000-4000-8000-{i:012}"))
            .collect();
        let many: Vec<&str> = many.iter().map(String::as_str).collect();
        for sessions in [vec![NEW, NEW], vec![nil], many] {
            assert!(
                save_peer(&client, &save(true, &sessions, Some(&[OLD])))
                    .await
                    .is_err()
            );
        }
        assert!(methods(&log).is_empty());
    }

    #[tokio::test]
    async fn a_changed_scope_is_written_before_access_is_turned_on() {
        let dir = tempfile::tempdir().unwrap();
        let (client, log) = fake_daemon(dir.path(), accepting);
        let out = save_peer(&client, &save(true, &[NEW], Some(&[OLD])))
            .await
            .unwrap();
        assert_eq!(out["revision"], 10);
        assert_eq!(out["steps"], json!(["peer_access_off", "scope", "peer"]));
        let sent = methods(&log);
        assert_eq!(
            sent.iter().map(|(m, _)| m.as_str()).collect::<Vec<_>>(),
            [
                "PutSatellitePeer",
                "PutSatellitePeerScope",
                "PutSatellitePeer"
            ]
        );
        assert_eq!(sent[0].1["peer"]["dispatch_enabled"], false);
        assert_eq!(sent[1].1["remote_session_ids"], json!([NEW]));
        assert_eq!(sent[2].1["expected_registry_revision"], 9);
        assert_eq!(sent[2].1["peer"]["dispatch_enabled"], true);
    }

    #[tokio::test]
    async fn an_unchanged_scope_is_one_peer_write() {
        let dir = tempfile::tempdir().unwrap();
        let (client, log) = fake_daemon(dir.path(), accepting);
        let out = save_peer(&client, &save(true, &[OLD], Some(&[OLD])))
            .await
            .unwrap();
        assert_eq!(out["steps"], json!(["peer"]));
        let sent = methods(&log);
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].1["peer"]["enabled"], true);
        // A new peer (no previous scope) always takes the staged path.
        let dir2 = tempfile::tempdir().unwrap();
        let (client, log) = fake_daemon(dir2.path(), accepting);
        save_peer(&client, &save(true, &[OLD], None)).await.unwrap();
        assert_eq!(methods(&log).len(), 3);
    }

    #[tokio::test]
    async fn a_lost_final_reply_is_reported_as_unknown_with_the_state_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let (client, log) = fake_daemon(dir.path(), enable_reply_lost);
        let err = save_peer(&client, &save(true, &[NEW], Some(&[OLD])))
            .await
            .unwrap_err();
        assert!(
            err.contains("Whether access was turned back on is unknown"),
            "{err}"
        );
        // The stateful registry committed the enable (7 -> 8 -> 9 -> 10) before dropping the reply.
        assert!(
            err.contains(
                "Steps 1-2 saved the peer with access off and the new scope (registry revision 9)"
            ),
            "{err}"
        );
        assert!(
            err.contains(
                "Read back now (registry revision 10): enabled true, reads true, dispatch true."
            ),
            "{err}"
        );
        assert_eq!(DROP_STATE.lock().unwrap().flags, (true, true, true));
        let sent = methods(&log);
        assert_eq!(
            sent.iter().map(|(m, _)| m.as_str()).collect::<Vec<_>>(),
            [
                "PutSatellitePeer",
                "PutSatellitePeerScope",
                "PutSatellitePeer",
                "ListSatellitePeers"
            ]
        );
    }

    #[tokio::test]
    async fn a_refused_final_enable_reports_the_concurrent_state_not_access_off() {
        let dir = tempfile::tempdir().unwrap();
        let (client, _log) = fake_daemon(dir.path(), concurrent_revision);
        let err = save_peer(&client, &save(true, &[NEW], Some(&[OLD])))
            .await
            .unwrap_err();
        assert!(
            err.contains("The daemon refused turning access back on"),
            "{err}"
        );
        // The other operator's write (revision 9 -> 10) won; the read-back reports it, not "access off".
        assert!(
            err.contains(
                "Read back now (registry revision 10): enabled true, reads true, dispatch false."
            ),
            "{err}"
        );
        assert!(!err.contains("stays"), "{err}");
        assert_eq!(RACE_STATE.lock().unwrap().revision, 10);
    }
}
