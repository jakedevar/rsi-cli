//! Settings pane commands (slice 7, hub #3009): read and write the daemon's
//! runtime config through the same two methods the TUI uses
//! (`crates/rsi/src/client.rs`): `GetDaemonConfig` and `UpdateDaemonConfig`.
//!
//! - `get_daemon_config` returns the config with every credential-like field
//!   and URL user-info redacted, so a credential value never reaches the
//!   webview (the daemon's runtime config holds none today; this is the guard).
//! - `update_daemon_config` sends ONE `UpdateDaemonConfig {"updates": [...]}`
//!   carrying every staged field with its final value, as the TUI's apply
//!   (`w`) does: the daemon validates the set and applies all or none.
//!
//! The daemon validates every field and value again; this side only refuses
//! shapes the settings pane never sends (unknown field spelling, nested
//! values, duplicates, credential fields).

use serde::Deserialize;
use serde_json::{Map, Value, json};
use tauri::State;

use crate::daemon::DaemonClient;
use crate::{AppState, CmdResult};

/// At most this many fields in one apply (the TUI page has fewer rows).
pub const MAX_UPDATES: usize = 256;
/// Longest text value the pane sends (the daemon text editors).
pub const MAX_VALUE_BYTES: usize = 64 * 1024;
pub const REDACTED: &str = "[redacted]";

/// One staged field and its final value.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct FieldUpdate {
    pub field: String,
    pub value: Value,
}

/// True for a field whose value may be a credential (mirrors
/// `isSecretField` in `src/settings-model.ts`).
pub fn is_secret_field(field: &str) -> bool {
    let f = field.to_ascii_lowercase();
    let words: Vec<&str> = f.split(['.', '_']).collect();
    let joined = |a: &str, b: &str| {
        words
            .windows(2)
            .any(|w| w[0] == a && w[1].trim_end_matches('s') == b)
    };
    let secret_word = words.iter().any(|w| {
        matches!(
            w.trim_end_matches('s'),
            "secret" | "password" | "passphrase" | "credential" | "apikey" | "bearer"
        )
    });
    secret_word
        || joined("api", "key")
        || joined("private", "key")
        || joined("auth", "token")
        || joined("access", "token")
        || joined("refresh", "token")
        || joined("session", "token")
        || words.last() == Some(&"token")
}

/// `scheme://user:pass@host` with the user-info masked.
pub fn mask_url_credentials(text: &str) -> String {
    let Some(scheme_end) = text.find("://") else {
        return text.to_string();
    };
    let scheme = &text[..scheme_end];
    if scheme.is_empty()
        || !scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
    {
        return text.to_string();
    }
    let rest = &text[scheme_end + 3..];
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    match rest[..authority_end].rfind('@') {
        Some(at) => format!("{scheme}://{REDACTED}{}", &rest[at..]),
        None => text.to_string(),
    }
}

/// The config the webview may see.
pub fn redact_config(config: Value) -> Value {
    let Value::Object(map) = config else {
        return config;
    };
    let mut out = Map::new();
    for (k, v) in map {
        let v = if is_secret_field(&k) {
            match v {
                Value::Null => Value::Null,
                Value::String(s) if s.is_empty() => Value::String(s),
                _ => Value::String(REDACTED.into()),
            }
        } else {
            match v {
                Value::String(s) => Value::String(mask_url_credentials(&s)),
                other => other,
            }
        };
        out.insert(k, v);
    }
    Value::Object(out)
}

fn valid_field(field: &str) -> bool {
    !field.is_empty()
        && field.len() <= 96
        && field
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'.')
        && !field.starts_with('.')
        && !field.ends_with('.')
}

/// The `UpdateDaemonConfig` params for an apply, or why it is refused.
pub fn update_params(updates: &[FieldUpdate]) -> Result<Value, String> {
    if updates.is_empty() {
        return Err("no settings to apply".into());
    }
    if updates.len() > MAX_UPDATES {
        return Err(format!("at most {MAX_UPDATES} settings per apply"));
    }
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(updates.len());
    for u in updates {
        if !valid_field(&u.field) {
            return Err(format!("{:?} is not a daemon config field name", u.field));
        }
        if is_secret_field(&u.field) {
            return Err(format!(
                "{} is a credential field; set it in the TUI",
                u.field
            ));
        }
        if !seen.insert(u.field.as_str()) {
            return Err(format!("{} appears twice", u.field));
        }
        match &u.value {
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
            Value::String(s) if s.len() <= MAX_VALUE_BYTES => {}
            Value::String(_) => return Err(format!("{} is longer than 64 KiB", u.field)),
            _ => return Err(format!("{} must be a plain value", u.field)),
        }
        out.push(json!({ "field": u.field, "value": u.value }));
    }
    Ok(json!({ "updates": out }))
}

pub async fn read_config(client: &DaemonClient) -> CmdResult {
    client
        .call("GetDaemonConfig", json!({}))
        .await
        .map(redact_config)
        .map_err(|e| e.to_string())
}

pub async fn write_config(client: &DaemonClient, updates: &[FieldUpdate]) -> CmdResult {
    let params = update_params(updates)?;
    client
        .call("UpdateDaemonConfig", params)
        .await
        .map_err(|e| e.to_string())
}

/// The daemon's runtime config (credential-like values redacted).
#[tauri::command]
pub async fn get_daemon_config(state: State<'_, AppState>) -> CmdResult {
    read_config(&state.client).await
}

/// Apply every staged settings edit in one atomic daemon update.
#[tauri::command]
pub async fn update_daemon_config(
    state: State<'_, AppState>,
    updates: Vec<FieldUpdate>,
) -> CmdResult {
    write_config(&state.client, &updates).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn up(field: &str, value: Value) -> FieldUpdate {
        FieldUpdate {
            field: field.into(),
            value,
        }
    }

    #[test]
    fn update_params_carry_every_field_in_one_request() {
        let p = update_params(&[
            up("retry_enabled", json!(true)),
            up("session_retention_window_hours", json!(48)),
            up("context_rotation_global_pct", Value::Null),
            up("mcp.deferred_tool_threshold", json!(12)),
            up("cold_archive_dir", json!("")),
        ])
        .unwrap();
        assert_eq!(
            p,
            json!({ "updates": [
                { "field": "retry_enabled", "value": true },
                { "field": "session_retention_window_hours", "value": 48 },
                { "field": "context_rotation_global_pct", "value": null },
                { "field": "mcp.deferred_tool_threshold", "value": 12 },
                { "field": "cold_archive_dir", "value": "" },
            ]})
        );
    }

    #[test]
    fn update_params_refuse_shapes_the_pane_never_sends() {
        assert!(update_params(&[]).is_err());
        assert!(update_params(&[up("Retry", json!(true))]).is_err());
        assert!(update_params(&[up("a b", json!(true))]).is_err());
        assert!(update_params(&[up(".x", json!(true))]).is_err());
        assert!(update_params(&[up("x", json!({ "a": 1 }))]).is_err());
        assert!(update_params(&[up("x", json!([1]))]).is_err());
        assert!(update_params(&[up("x", json!(1)), up("x", json!(2))]).is_err());
        assert!(update_params(&[up("x", json!("a".repeat(MAX_VALUE_BYTES + 1)))]).is_err());
        let many: Vec<FieldUpdate> = (0..=MAX_UPDATES)
            .map(|i| up(&format!("f{i}"), json!(1)))
            .collect();
        assert!(update_params(&many).is_err());
        let err = update_params(&[up("openrouter_api_key", json!("k"))]).unwrap_err();
        assert!(err.contains("credential"), "{err}");
    }

    #[test]
    fn secret_fields_are_recognised_and_token_budgets_are_not() {
        for f in [
            "openrouter_api_key",
            "api_key",
            "vault.secret",
            "bridge_password",
            "github_token",
            "auth_tokens",
            "provider_credentials",
        ] {
            assert!(is_secret_field(f), "{f}");
        }
        for f in [
            "openrouter_context_budget_tokens",
            "coordinator_context_cap_tokens",
            "vault.env_compat",
            "harness_searxng_url",
            "retry_enabled",
        ] {
            assert!(!is_secret_field(f), "{f}");
        }
    }

    #[test]
    fn redaction_masks_secret_values_and_url_userinfo() {
        let out = redact_config(json!({
            "retry_enabled": true,
            "some_api_key": "sk-live-123",
            "empty_api_key": "",
            "unset_token": null,
            "harness_searxng_url": "https://user:pw@search.example/q",
            "title_model_base_url": "http://127.0.0.1:8080/v1",
        }));
        assert_eq!(out["retry_enabled"], json!(true));
        assert_eq!(out["some_api_key"], json!(REDACTED));
        assert_eq!(out["empty_api_key"], json!(""));
        assert_eq!(out["unset_token"], Value::Null);
        assert_eq!(
            out["harness_searxng_url"],
            json!(format!("https://{REDACTED}@search.example/q"))
        );
        assert_eq!(
            out["title_model_base_url"],
            json!("http://127.0.0.1:8080/v1")
        );
        assert!(!out.to_string().contains("sk-live-123"));
        assert!(!out.to_string().contains("pw@"));
    }

    #[cfg(unix)]
    mod round_trip {
        use super::*;
        use std::path::Path;
        use std::sync::{Arc, Mutex};
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixListener;

        type Log = Arc<Mutex<Vec<Value>>>;

        /// A daemon that keeps a config and answers `GetDaemonConfig` and
        /// `UpdateDaemonConfig {updates}` (all-or-nothing, refusing unknown
        /// fields), recording every request.
        fn fake_daemon(dir: &Path, initial: Value) -> (std::path::PathBuf, Log) {
            let sock = dir.join("d.sock");
            let listener = UnixListener::bind(&sock).unwrap();
            let log: Log = Arc::default();
            let seen = Arc::clone(&log);
            let config = Arc::new(Mutex::new(initial));
            tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        return;
                    };
                    let (r, mut w) = stream.into_split();
                    let mut line = String::new();
                    BufReader::new(r).read_line(&mut line).await.unwrap();
                    let req: Value = serde_json::from_str(&line).unwrap();
                    seen.lock().unwrap().push(req.clone());
                    let reply = {
                        let mut cfg = config.lock().unwrap();
                        match req["method"].as_str().unwrap_or_default() {
                            "GetDaemonConfig" => {
                                json!({ "jsonrpc": "2.0", "id": req["id"], "result": *cfg })
                            }
                            "UpdateDaemonConfig" => {
                                let updates = req["params"]["updates"]
                                    .as_array()
                                    .cloned()
                                    .unwrap_or_default();
                                let unknown = updates
                                    .iter()
                                    .find(|u| cfg.get(u["field"].as_str().unwrap_or("")).is_none());
                                if let Some(u) = unknown {
                                    json!({ "jsonrpc": "2.0", "id": req["id"], "error": { "code": -32602, "message": format!("unknown field {}", u["field"]) } })
                                } else {
                                    for u in &updates {
                                        cfg[u["field"].as_str().unwrap()] = u["value"].clone();
                                    }
                                    json!({ "jsonrpc": "2.0", "id": req["id"], "result": null })
                                }
                            }
                            other => {
                                json!({ "jsonrpc": "2.0", "id": req["id"], "error": { "code": -32601, "message": format!("no {other}") } })
                            }
                        }
                    };
                    w.write_all(format!("{reply}\n").as_bytes()).await.unwrap();
                }
            });
            (sock, log)
        }

        #[tokio::test]
        async fn read_stage_apply_reread() {
            let dir = tempfile::tempdir().unwrap();
            let (sock, log) = fake_daemon(
                dir.path(),
                json!({
                    "retry_enabled": false,
                    "session_retention_window_hours": 24,
                    "harness_searxng_url": null,
                    "vault_api_key": "never-shown",
                }),
            );
            let client = DaemonClient::new(&sock);

            // Read.
            let before = read_config(&client).await.unwrap();
            assert_eq!(before["retry_enabled"], json!(false));
            assert_eq!(before["vault_api_key"], json!(REDACTED));

            // Stage two edits (the pane holds them) and apply them as one update.
            let staged = [
                up("retry_enabled", json!(true)),
                up("session_retention_window_hours", json!(48)),
            ];
            write_config(&client, &staged).await.unwrap();

            // Re-read: both landed.
            let after = read_config(&client).await.unwrap();
            assert_eq!(after["retry_enabled"], json!(true));
            assert_eq!(after["session_retention_window_hours"], json!(48));

            let sent = log.lock().unwrap().clone();
            let methods: Vec<&str> = sent.iter().map(|r| r["method"].as_str().unwrap()).collect();
            assert_eq!(
                methods,
                ["GetDaemonConfig", "UpdateDaemonConfig", "GetDaemonConfig"]
            );
            assert_eq!(
                sent[1]["params"],
                json!({ "updates": [
                    { "field": "retry_enabled", "value": true },
                    { "field": "session_retention_window_hours", "value": 48 },
                ]})
            );
            assert!(sent.iter().all(|r| r.get("session_token").is_none()));
        }

        #[tokio::test]
        async fn a_refused_apply_changes_nothing() {
            let dir = tempfile::tempdir().unwrap();
            let (sock, _) = fake_daemon(dir.path(), json!({ "retry_enabled": false }));
            let client = DaemonClient::new(&sock);
            let err = write_config(
                &client,
                &[
                    up("retry_enabled", json!(true)),
                    up("no_such_field", json!(1)),
                ],
            )
            .await
            .unwrap_err();
            assert!(err.contains("unknown field"), "{err}");
            let after = read_config(&client).await.unwrap();
            assert_eq!(after["retry_enabled"], json!(false));
        }
    }
}
