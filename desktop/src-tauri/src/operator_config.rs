//! Operator configuration: Claude hooks, custom providers, message bridges
//! (hub #3039, Desktop 13B).
//!
//! The desktop edits these through the daemon's operator-only RPCs, the same
//! ones the TUI uses (`crates/rsid/src/operator_config/`, wire types in
//! `rsi_common::operator_config`), so both frontends read one source of truth.
//! Each command validates its input and forwards exactly one daemon method.
//! A custom provider's API key travels only in `PutCustomProvider` (the daemon
//! stores it in the vault); no read returns it, and no error text here repeats it.

use serde_json::{Map, Value, json};
use tauri::State;

use crate::{AppState, CmdResult, check_text, forward, parse_uuid};

const MAX_FIELD: usize = 4096;

pub const LIST_HOOKS: &str = "ListClaudeHooks";
pub const PUT_HOOKS: &str = "PutClaudeHooks";
pub const DELETE_HOOKS: &str = "DeleteClaudeHooks";
pub const LIST_PROVIDERS: &str = "ListCustomProviders";
pub const PUT_PROVIDER: &str = "PutCustomProvider";
pub const DELETE_PROVIDER: &str = "DeleteCustomProvider";
pub const SET_ROUTE: &str = "SetCustomProviderRoute";
pub const LIST_ROUTES: &str = "ListCustomProviderRoutes";
pub const LIST_BRIDGES: &str = "ListMessageBridges";
pub const PUT_BRIDGE: &str = "PutMessageBridge";
pub const DELETE_BRIDGE: &str = "DeleteMessageBridge";

/// Model roles a custom provider can serve (`CustomProviderRole`, snake_case).
const ROLES: &[&str] = &[
    "title_model",
    "memory_model_fallback",
    "dream_model",
    "prompt_compile_model",
];
/// Bridge kinds (`MessageBridgeKindV1`, snake_case).
const BRIDGE_KINDS: &[&str] = &["signal", "imessage"];

fn short_text(field: &str, text: &str) -> Result<String, String> {
    let text = text.trim();
    if text.len() > MAX_FIELD {
        return Err(format!("{field} exceeds {MAX_FIELD} bytes"));
    }
    Ok(text.to_string())
}

fn revision(raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    if raw.is_empty() || raw.len() > 256 {
        return Err("expected_revision must be 1 to 256 characters".into());
    }
    Ok(raw.to_string())
}

fn one_of(field: &str, allowed: &[&str], raw: &str) -> Result<String, String> {
    let raw = raw.trim().to_ascii_lowercase();
    if allowed.contains(&raw.as_str()) {
        Ok(raw)
    } else {
        Err(format!("{field} must be one of: {}", allowed.join(", ")))
    }
}

/// `PutClaudeHooks {expected_revision, hooks}`; `hooks` is the event-name keyed
/// object the daemon validates again.
pub fn put_hooks_params(expected_revision: &str, hooks: &Value) -> Result<Value, String> {
    if !hooks.is_object() {
        return Err("hooks must be an object keyed by event name".into());
    }
    Ok(json!({ "expected_revision": revision(expected_revision)?, "hooks": hooks }))
}

/// `DeleteClaudeHooks {expected_revision}`.
pub fn delete_hooks_params(expected_revision: &str) -> Result<Value, String> {
    Ok(json!({ "expected_revision": revision(expected_revision)? }))
}

/// The key change of a `PutCustomProvider`: `keep` (default), `set` with a
/// secret, or `clear`. The secret is never echoed in an error.
fn key_change(action: &str, secret: Option<&str>) -> Result<Value, String> {
    match action.trim().to_ascii_lowercase().as_str() {
        "keep" | "" => Ok(json!({ "action": "keep" })),
        "clear" => Ok(json!({ "action": "clear" })),
        "set" => match secret {
            Some(s) if !s.trim().is_empty() && s.len() <= MAX_FIELD => {
                Ok(json!({ "action": "set", "secret": s.trim() }))
            }
            _ => Err("an API key to set must be 1 to 4096 characters".into()),
        },
        _ => Err("api_key action must be one of: keep, set, clear".into()),
    }
}

/// `PutCustomProvider {id?, name, base_url, default_model, api_key}`.
pub fn put_provider_params(
    id: Option<&str>,
    name: &str,
    base_url: &str,
    default_model: &str,
    key_action: &str,
    secret: Option<&str>,
) -> Result<Value, String> {
    let name = short_text("name", name)?;
    check_text("name", &name)?;
    let base_url = short_text("base_url", base_url)?;
    if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
        return Err("base_url must start with http:// or https://".into());
    }
    let mut params = Map::new();
    if let Some(id) = id.map(str::trim).filter(|i| !i.is_empty()) {
        params.insert("id".into(), json!(parse_uuid("id", id)?));
    }
    params.insert("name".into(), json!(name));
    params.insert("base_url".into(), json!(base_url));
    params.insert(
        "default_model".into(),
        json!(short_text("default_model", default_model)?),
    );
    params.insert("api_key".into(), key_change(key_action, secret)?);
    Ok(Value::Object(params))
}

/// `{ "id": <lowercase uuid> }` for Get/DeleteCustomProvider.
pub fn provider_id_params(id: &str) -> Result<Value, String> {
    Ok(json!({ "id": parse_uuid("id", id)? }))
}

/// `SetCustomProviderRoute {role, id}`; no id detaches the role.
pub fn route_params(role: &str, id: Option<&str>) -> Result<Value, String> {
    let role = one_of("role", ROLES, role)?;
    let id = match id.map(str::trim).filter(|i| !i.is_empty()) {
        Some(i) => json!(parse_uuid("id", i)?),
        None => Value::Null,
    };
    Ok(json!({ "role": role, "id": id }))
}

/// `PutMessageBridge {kind, config: {enabled, account, allow_from, working_dir}}`.
pub fn put_bridge_params(
    kind: &str,
    enabled: bool,
    account: &str,
    allow_from: &str,
    working_dir: &str,
) -> Result<Value, String> {
    let kind = one_of("kind", BRIDGE_KINDS, kind)?;
    let working_dir = short_text("working_dir", working_dir)?;
    check_text("working_dir", &working_dir)?;
    Ok(json!({
        "kind": kind,
        "config": {
            "enabled": enabled,
            "account": short_text("account", account)?,
            "allow_from": short_text("allow_from", allow_from)?,
            "working_dir": working_dir,
        },
    }))
}

/// `{ "kind": ... }` for Get/DeleteMessageBridge.
pub fn bridge_kind_params(kind: &str) -> Result<Value, String> {
    Ok(json!({ "kind": one_of("kind", BRIDGE_KINDS, kind)? }))
}

/// Read the `hooks` block of `~/.claude/settings.json` and its revision.
#[tauri::command]
pub async fn list_claude_hooks(state: State<'_, AppState>) -> CmdResult {
    forward(&state, LIST_HOOKS, json!({})).await
}

/// Save the hooks block; a stale revision is refused `claude_hooks_conflict`.
#[tauri::command]
pub async fn put_claude_hooks(
    state: State<'_, AppState>,
    expected_revision: String,
    hooks: Value,
) -> CmdResult {
    let params = put_hooks_params(&expected_revision, &hooks)?;
    forward(&state, PUT_HOOKS, params).await
}

/// Remove the hooks block (other settings keys are kept); the frontend confirms first.
#[tauri::command]
pub async fn delete_claude_hooks(
    state: State<'_, AppState>,
    expected_revision: String,
) -> CmdResult {
    let params = delete_hooks_params(&expected_revision)?;
    forward(&state, DELETE_HOOKS, params).await
}

/// The custom providers (key state and fingerprint only, never the key).
#[tauri::command]
pub async fn list_custom_providers(state: State<'_, AppState>) -> CmdResult {
    forward(&state, LIST_PROVIDERS, json!({})).await
}

/// Create (no id) or replace a custom provider. `key_action` is `keep`, `set`
/// (with `secret`) or `clear`.
#[tauri::command]
pub async fn put_custom_provider(
    state: State<'_, AppState>,
    id: Option<String>,
    name: String,
    base_url: String,
    default_model: String,
    key_action: String,
    secret: Option<String>,
) -> CmdResult {
    let params = put_provider_params(
        id.as_deref(),
        &name,
        &base_url,
        &default_model,
        &key_action,
        secret.as_deref(),
    )?;
    forward(&state, PUT_PROVIDER, params).await
}

/// Delete a custom provider and clear the roles routed to it; the frontend confirms first.
#[tauri::command]
pub async fn delete_custom_provider(state: State<'_, AppState>, id: String) -> CmdResult {
    let params = provider_id_params(&id)?;
    forward(&state, DELETE_PROVIDER, params).await
}

/// Point a model role at a custom provider, or detach it (no `id`).
#[tauri::command]
pub async fn set_custom_provider_route(
    state: State<'_, AppState>,
    role: String,
    id: Option<String>,
) -> CmdResult {
    let params = route_params(&role, id.as_deref())?;
    forward(&state, SET_ROUTE, params).await
}

/// Keep only what a route view may carry: the role, the routed provider's id
/// and name, and its key state, fingerprint and set time. Anything else a
/// daemon sends (a key field included) is dropped, so a misbehaving reply
/// cannot reach the webview.
pub fn sanitize_routes(raw: &Value) -> Result<Value, String> {
    let routes = raw
        .get("routes")
        .and_then(Value::as_array)
        .ok_or("the daemon sent no route list")?;
    let mut out = Vec::new();
    for route in routes {
        let Some(role) = route
            .get("role")
            .and_then(Value::as_str)
            .filter(|r| ROLES.contains(r))
        else {
            continue;
        };
        let id = route
            .get("id")
            .and_then(Value::as_str)
            .and_then(|i| parse_uuid("id", i).ok());
        let name = route.get("name").and_then(Value::as_str);
        let key = route.get("api_key").and_then(Value::as_object);
        let key = key.map(|k| {
            let mut m = Map::new();
            let state = if k.get("state").and_then(Value::as_str) == Some("set") {
                "set"
            } else {
                "absent"
            };
            m.insert("state".into(), json!(state));
            for field in ["fingerprint", "set_at"] {
                if let Some(v) = k.get(field).and_then(Value::as_str) {
                    m.insert(field.into(), json!(v));
                }
            }
            Value::Object(m)
        });
        out.push(json!({
            "role": role,
            "id": id,
            "name": name.filter(|_| id.is_some()),
            "api_key": key.filter(|_| id.is_some()),
        }));
    }
    Ok(json!({ "routes": out }))
}

/// `ListCustomProviderRoutes {}` through `client`, sanitized.
pub(crate) async fn read_routes(client: &crate::daemon::DaemonClient) -> CmdResult {
    let raw = client
        .call(LIST_ROUTES, json!({}))
        .await
        .map_err(|e| e.to_string())?;
    sanitize_routes(&raw)
}

/// The stored route of every model role (the provider id and name, key state
/// and fingerprint only, never the key).
#[tauri::command]
pub async fn list_custom_provider_routes(state: State<'_, AppState>) -> CmdResult {
    read_routes(&state.client).await
}

/// Both message bridges (Signal, iMessage) with their config files.
#[tauri::command]
pub async fn list_message_bridges(state: State<'_, AppState>) -> CmdResult {
    forward(&state, LIST_BRIDGES, json!({})).await
}

/// Save one bridge's config.
#[tauri::command]
pub async fn put_message_bridge(
    state: State<'_, AppState>,
    kind: String,
    enabled: bool,
    account: String,
    allow_from: String,
    working_dir: String,
) -> CmdResult {
    let params = put_bridge_params(&kind, enabled, &account, &allow_from, &working_dir)?;
    forward(&state, PUT_BRIDGE, params).await
}

/// Remove one bridge's config file; the frontend confirms first.
#[tauri::command]
pub async fn delete_message_bridge(state: State<'_, AppState>, kind: String) -> CmdResult {
    let params = bridge_kind_params(&kind)?;
    forward(&state, DELETE_BRIDGE, params).await
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "E2CAE1D4-1833-48EB-A7E8-6F1DD480FDAE";

    #[test]
    fn method_names_are_the_daemons() {
        // The names of `rsi_common::operator_config::OPERATOR_METHODS` this app calls.
        assert_eq!(
            [
                LIST_HOOKS,
                PUT_HOOKS,
                DELETE_HOOKS,
                LIST_PROVIDERS,
                PUT_PROVIDER,
                DELETE_PROVIDER,
                SET_ROUTE,
                LIST_ROUTES,
                LIST_BRIDGES,
                PUT_BRIDGE,
                DELETE_BRIDGE,
            ],
            [
                "ListClaudeHooks",
                "PutClaudeHooks",
                "DeleteClaudeHooks",
                "ListCustomProviders",
                "PutCustomProvider",
                "DeleteCustomProvider",
                "SetCustomProviderRoute",
                "ListCustomProviderRoutes",
                "ListMessageBridges",
                "PutMessageBridge",
                "DeleteMessageBridge",
            ]
        );
    }

    #[test]
    fn hooks_writes_carry_the_revision_and_an_object() {
        let p = put_hooks_params(" rev-1 ", &json!({"Stop": []})).unwrap();
        assert_eq!(p["expected_revision"], "rev-1");
        assert_eq!(p["hooks"], json!({"Stop": []}));
        assert!(put_hooks_params("rev-1", &json!([])).is_err());
        assert!(put_hooks_params("  ", &json!({})).is_err());
        assert_eq!(
            delete_hooks_params("absent").unwrap(),
            json!({"expected_revision": "absent"})
        );
        assert!(delete_hooks_params("").is_err());
    }

    #[test]
    fn a_provider_put_normalizes_and_validates() {
        let p = put_provider_params(
            Some(ID),
            " Local ",
            "http://localhost:8080/v1",
            "m",
            "keep",
            None,
        )
        .unwrap();
        assert_eq!(p["id"], "e2cae1d4-1833-48eb-a7e8-6f1dd480fdae");
        assert_eq!(p["name"], "Local");
        assert_eq!(p["api_key"], json!({"action": "keep"}));
        let new = put_provider_params(None, "n", "https://x.example/v1", "", "", None).unwrap();
        assert!(new.get("id").is_none());
        assert_eq!(new["default_model"], "");
        assert!(put_provider_params(None, " ", "https://x", "", "keep", None).is_err());
        assert!(put_provider_params(None, "n", "ftp://x", "", "keep", None).is_err());
        assert!(put_provider_params(Some("nope"), "n", "https://x", "", "keep", None).is_err());
    }

    #[test]
    fn a_key_travels_only_with_the_set_action_and_is_never_in_an_error() {
        let p =
            put_provider_params(None, "n", "https://x", "", "set", Some(" sk-secret ")).unwrap();
        assert_eq!(
            p["api_key"],
            json!({"action": "set", "secret": "sk-secret"})
        );
        let clear =
            put_provider_params(None, "n", "https://x", "", "clear", Some("ignored")).unwrap();
        assert_eq!(clear["api_key"], json!({"action": "clear"}));
        let keep =
            put_provider_params(None, "n", "https://x", "", "keep", Some("ignored")).unwrap();
        assert!(!keep.to_string().contains("ignored"));
        let err = put_provider_params(None, "n", "https://x", "", "set", Some("  ")).unwrap_err();
        assert!(!err.contains("sk-"));
        let err = put_provider_params(None, "n", "https://x", "", "rotate", Some("sk-secret"))
            .unwrap_err();
        assert!(!err.contains("sk-secret"));
    }

    #[test]
    fn routes_name_a_known_role_and_an_optional_provider() {
        let p = route_params("Dream_Model", Some(ID)).unwrap();
        assert_eq!(p["role"], "dream_model");
        assert_eq!(p["id"], "e2cae1d4-1833-48eb-a7e8-6f1dd480fdae");
        assert_eq!(
            route_params("title_model", None).unwrap()["id"],
            Value::Null
        );
        assert_eq!(
            route_params("title_model", Some(" ")).unwrap()["id"],
            Value::Null
        );
        assert!(route_params("coding_model", None).is_err());
        for role in ROLES {
            assert!(route_params(role, None).is_ok());
        }
    }

    #[test]
    fn a_bridge_put_has_the_config_shape() {
        let p = put_bridge_params("Signal", true, "+1", "a,b", "/home/x").unwrap();
        assert_eq!(
            p,
            json!({"kind": "signal", "config": {
                "enabled": true, "account": "+1", "allow_from": "a,b", "working_dir": "/home/x"}})
        );
        assert!(put_bridge_params("sms", true, "", "", "/x").is_err());
        assert!(put_bridge_params("imessage", true, "", "", " ").is_err());
        assert_eq!(
            bridge_kind_params("IMessage").unwrap(),
            json!({"kind": "imessage"})
        );
        assert!(bridge_kind_params("matrix").is_err());
    }

    #[test]
    fn a_route_view_keeps_only_the_allowlisted_fields() {
        let raw = json!({"routes": [
            {"role": "dream_model", "id": ID, "name": "Local", "api_key": {
                "state": "set", "fingerprint": "ab12", "set_at": "2026-01-01T00:00:00Z",
                "secret": "sk-LEAK"}, "api_key_value": "sk-LEAK"},
            {"role": "title_model", "id": null, "name": null, "api_key": null},
            {"role": "coding_model", "id": ID, "name": "x", "api_key": null},
            {"role": "prompt_compile_model", "id": "not-a-uuid", "name": "y", "api_key": {"state": "bogus"}},
        ]});
        let view = sanitize_routes(&raw).unwrap();
        assert_eq!(
            view,
            json!({"routes": [
                {"role": "dream_model", "id": "e2cae1d4-1833-48eb-a7e8-6f1dd480fdae", "name": "Local",
                 "api_key": {"state": "set", "fingerprint": "ab12", "set_at": "2026-01-01T00:00:00Z"}},
                {"role": "title_model", "id": null, "name": null, "api_key": null},
                {"role": "prompt_compile_model", "id": null, "name": null, "api_key": null},
            ]})
        );
        assert!(!view.to_string().contains("LEAK"));
        assert!(sanitize_routes(&json!({})).is_err());
    }

    #[cfg(unix)]
    mod round_trip {
        use super::*;
        use crate::admin::tests::round_trip::fake_daemon;

        fn leaky(method: &str, params: &Value) -> Value {
            assert_eq!(method, LIST_ROUTES);
            assert_eq!(params, &json!({}));
            json!({"routes": [
                {"role": "dream_model", "id": ID, "name": "Local",
                 "api_key": {"state": "set", "fingerprint": "ab12", "secret": "sk-LEAK"},
                 "api_key_value": "sk-LEAK"},
                {"role": "title_model", "id": null, "name": null, "api_key": null},
            ]})
        }

        #[tokio::test]
        async fn the_route_read_forwards_one_method_and_never_returns_a_key() {
            let dir = tempfile::tempdir().unwrap();
            let (client, log) = fake_daemon(dir.path(), leaky);
            let view = read_routes(&client).await.unwrap();
            assert_eq!(view["routes"][0]["role"], "dream_model");
            assert_eq!(view["routes"][0]["api_key"]["fingerprint"], "ab12");
            assert_eq!(view["routes"][1]["id"], Value::Null);
            assert!(!view.to_string().contains("LEAK"));
            let log = log.lock().unwrap();
            assert_eq!(log.len(), 1);
            assert_eq!(log[0]["method"], "ListCustomProviderRoutes");
        }
    }
}
