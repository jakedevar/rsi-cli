//! Action handlers for the key vault / Provider Keys settings category
//! (#694 K1b).
//!
//! Every handler here is a direct `app.client.<verb>().await` round-trip
//! (mirrors `toggle_daemon_feature`/`cancel_model_invocation`, not the
//! heavier bootstrap-snapshot refresh path used for `Stats`/`DaemonFeatures`
//! periodic polling) — the vault RPCs are one-shot operator actions, not
//! polled state.
//!
//! Secret handling: the ONLY place a typed secret sits in persistent TUI
//! state is `OverlayState::ProviderCredentialForm.secret`. That buffer is
//! scrubbed in place (see `overlay::provider_credential_form::scrub_secret`)
//! the instant its form enqueues `LcAction::SetProviderCredentialSecret` /
//! `RotateProviderCredentialSecret` — before this module ever runs. The
//! `secret` parameter these handlers receive is moved directly into the
//! `app.client` RPC call and never copied or retained; once the call
//! returns there is no longer any TUI-owned copy left to scrub.

use crate::app::App;
use rsi_common::provider_credentials::{ProviderCredentialMetadata, ProviderCredentialSlot};

use super::daemon_config::require_authoritative_config;

/// Refresh `App::cached_provider_credentials` from the daemon.
#[allow(clippy::future_not_send)] // App is owned by the single-threaded event loop.
pub async fn refresh_provider_credentials(app: &mut App) {
    if !require_authoritative_config(app) {
        return;
    }
    match app.client.list_provider_credentials().await {
        Ok(result) => {
            app.cached_provider_credentials = Some(result);
        }
        Err(e) => {
            tracing::warn!("Failed to list provider credentials: {}", e);
            app.notify(format!("Failed to refresh provider keys: {e}"));
        }
    }
}

/// Merge one slot's updated metadata into the cached list (insert if the
/// list was never populated, replace the matching slot otherwise). Keeps
/// the Provider Keys row list current without a full re-fetch after every
/// single-slot mutation.
fn upsert_credential(app: &mut App, meta: ProviderCredentialMetadata) {
    match app.cached_provider_credentials.as_mut() {
        Some(list) => {
            if let Some(existing) = list.credentials.iter_mut().find(|m| m.slot == meta.slot) {
                *existing = meta;
            } else {
                list.credentials.push(meta);
            }
        }
        None => {
            // No prior `ListProviderCredentials` response yet — the daemon
            // settings (env_compat/check_ttl_secs) are read separately
            // (Settings -> Daemon Features); a follow-up refresh fills them
            // in. Seed with sane defaults so the row renders immediately
            // instead of staying on the loading placeholder.
            app.cached_provider_credentials = Some(
                rsi_common::provider_credentials::ListProviderCredentialsResult {
                    env_compat: true,
                    check_ttl_secs: rsi_common::provider_credentials::DEFAULT_CHECK_TTL_SECS,
                    credentials: vec![meta],
                },
            );
        }
    }
}

/// Send `SetProviderCredential`. `secret` is moved in and passed straight
/// to `app.client` — see module doc for the full secret-handling contract.
#[allow(clippy::future_not_send)] // App is owned by the single-threaded event loop.
pub async fn set_provider_credential(app: &mut App, slot: ProviderCredentialSlot, secret: String) {
    if !require_authoritative_config(app) {
        return;
    }
    match app.client.set_provider_credential(slot, secret).await {
        Ok(meta) => {
            upsert_credential(app, meta);
            app.notify_success(format!("{slot}: credential set"));
            // #1407: the `:aws-setup` flow verifies the stored Bedrock key.
            if slot == ProviderCredentialSlot::Bedrock && app.aws_setup_verify_pending {
                super::aws_setup::verify_bedrock_setup(app, None).await;
            }
        }
        Err(e) => {
            tracing::warn!("Failed to set provider credential {}: {}", slot, e);
            app.notify_error(format!("Failed to set {slot} credential: {e}"));
        }
    }
}

#[allow(clippy::future_not_send)] // App is owned by the single-threaded event loop.
pub async fn rotate_provider_credential(
    app: &mut App,
    slot: ProviderCredentialSlot,
    secret: String,
) {
    if !require_authoritative_config(app) {
        return;
    }
    match app.client.rotate_provider_credential(slot, secret).await {
        Ok(meta) => {
            upsert_credential(app, meta);
            app.notify_success(format!("{slot}: credential rotated"));
        }
        Err(e) => {
            tracing::warn!("Failed to rotate provider credential {}: {}", slot, e);
            app.notify_error(format!("Failed to rotate {slot} credential: {e}"));
        }
    }
}

#[allow(clippy::future_not_send)] // App is owned by the single-threaded event loop.
pub async fn clear_provider_credential(app: &mut App, slot: ProviderCredentialSlot) {
    if !require_authoritative_config(app) {
        return;
    }
    match app.client.clear_provider_credential(slot).await {
        Ok(meta) => {
            upsert_credential(app, meta);
            app.notify_success(format!("{slot}: credential cleared"));
        }
        Err(e) => {
            tracing::warn!("Failed to clear provider credential {}: {}", slot, e);
            app.notify_error(format!("Failed to clear {slot} credential: {e}"));
        }
    }
}

#[allow(clippy::future_not_send)] // App is owned by the single-threaded event loop.
pub async fn check_provider_credential(app: &mut App, slot: ProviderCredentialSlot) {
    if !require_authoritative_config(app) {
        return;
    }
    match app.client.check_provider_credential(slot).await {
        Ok(meta) => {
            let class = meta.check.as_ref().map_or("unknown", |c| {
                crate::provider_credential_rows::check_class_label(c.class)
            });
            upsert_credential(app, meta);
            app.notify_success(format!("{slot}: check → {class}"));
        }
        Err(e) => {
            tracing::warn!("Failed to check provider credential {}: {}", slot, e);
            app.notify_error(format!("Failed to check {slot} credential: {e}"));
        }
    }
}

#[allow(clippy::future_not_send)] // App is owned by the single-threaded event loop.
pub async fn import_provider_credentials_from_env(app: &mut App) {
    if !require_authoritative_config(app) {
        return;
    }
    match app.client.import_provider_credentials_from_env().await {
        Ok(result) => {
            let imported = result.imported.len();
            let skipped = result.skipped.len();
            for entry in result.imported {
                app.notify(format!("Imported {} from {}", entry.slot, entry.env_var));
            }
            app.notify_success(format!(
                "Import from env: {imported} imported, {skipped} skipped"
            ));
            // Single-slot metadata isn't returned by Import — re-fetch the
            // full list so every imported/skip-affected row reflects state.
            refresh_provider_credentials(app).await;
        }
        Err(e) => {
            tracing::warn!("Failed to import provider credentials from env: {}", e);
            app.notify_error(format!("Failed to import from env: {e}"));
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::client::DaemonClient;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    #[allow(clippy::future_not_send)] // App is owned by the single-threaded event loop.
    async fn test_app_connected(socket: std::path::PathBuf) -> App {
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        app.client = DaemonClient::new(socket);
        app.client.connect().await.unwrap();
        app.poll.connected = true;
        app.poll.authoritative_config_ready = true;
        app
    }

    fn credential_metadata_json(slot: &str) -> serde_json::Value {
        serde_json::json!({
            "slot": slot,
            "state": "vault",
            "fingerprint": "ab12ef34",
            "set_at": "2026-09-24T00:00:00Z",
            "rotated_from_fingerprint": null,
            "cleared_at": null,
            "check": null,
            "generation": 1,
            "route": "codex_cli",
            "cli_exposure": "always",
            "last_cli_exposure_at": null,
        })
    }

    /// Verifies the action dispatch builds the exact `SetProviderCredential`
    /// RPC method + params (slot + secret verbatim, nothing extra).
    #[tokio::test]
    async fn set_provider_credential_sends_exact_rpc() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut lines = BufReader::new(read).lines();
            let request: serde_json::Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(request["method"], "SetProviderCredential");
            assert_eq!(request["params"]["slot"], "openrouter");
            assert_eq!(request["params"]["secret"], "sk-test-canary-0001");
            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": request["id"],
                "result": credential_metadata_json("openrouter"),
            });
            write
                .write_all(format!("{response}\n").as_bytes())
                .await
                .unwrap();
        });

        let mut app = Box::pin(test_app_connected(socket)).await;
        set_provider_credential(
            &mut app,
            ProviderCredentialSlot::Openrouter,
            "sk-test-canary-0001".to_string(),
        )
        .await;
        server.await.unwrap();

        let meta = crate::provider_credential_rows::credential_metadata(
            &app,
            ProviderCredentialSlot::Openrouter,
        )
        .expect("credential cached after set");
        assert_eq!(meta.fingerprint.as_deref(), Some("ab12ef34"));
    }

    #[tokio::test]
    async fn rotate_provider_credential_sends_exact_rpc() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut lines = BufReader::new(read).lines();
            let request: serde_json::Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(request["method"], "RotateProviderCredential");
            assert_eq!(request["params"]["slot"], "anthropic");
            assert_eq!(request["params"]["secret"], "sk-ant-rotate-0001");
            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": request["id"],
                "result": credential_metadata_json("anthropic"),
            });
            write
                .write_all(format!("{response}\n").as_bytes())
                .await
                .unwrap();
        });

        let mut app = Box::pin(test_app_connected(socket)).await;
        rotate_provider_credential(
            &mut app,
            ProviderCredentialSlot::Anthropic,
            "sk-ant-rotate-0001".to_string(),
        )
        .await;
        server.await.unwrap();
    }

    #[tokio::test]
    async fn clear_provider_credential_sends_exact_rpc() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut lines = BufReader::new(read).lines();
            let request: serde_json::Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(request["method"], "ClearProviderCredential");
            assert_eq!(request["params"]["slot"], "bedrock");
            let mut result = credential_metadata_json("bedrock");
            result["state"] = serde_json::json!("cleared");
            result["fingerprint"] = serde_json::Value::Null;
            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": request["id"],
                "result": result,
            });
            write
                .write_all(format!("{response}\n").as_bytes())
                .await
                .unwrap();
        });

        let mut app = Box::pin(test_app_connected(socket)).await;
        clear_provider_credential(&mut app, ProviderCredentialSlot::Bedrock).await;
        server.await.unwrap();

        let meta = crate::provider_credential_rows::credential_metadata(
            &app,
            ProviderCredentialSlot::Bedrock,
        )
        .expect("credential cached after clear");
        assert_eq!(
            meta.state,
            rsi_common::provider_credentials::CredentialState::Cleared
        );
    }

    #[tokio::test]
    async fn check_provider_credential_sends_exact_rpc() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut lines = BufReader::new(read).lines();
            let request: serde_json::Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(request["method"], "CheckProviderCredential");
            assert_eq!(request["params"]["slot"], "openrouter");
            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": request["id"],
                "result": credential_metadata_json("openrouter"),
            });
            write
                .write_all(format!("{response}\n").as_bytes())
                .await
                .unwrap();
        });

        let mut app = Box::pin(test_app_connected(socket)).await;
        check_provider_credential(&mut app, ProviderCredentialSlot::Openrouter).await;
        server.await.unwrap();
    }

    #[tokio::test]
    async fn import_provider_credentials_from_env_sends_exact_rpc_then_refreshes() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut lines = BufReader::new(read).lines();

            let import_request: serde_json::Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(import_request["method"], "ImportProviderCredentialsFromEnv");
            let import_response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": import_request["id"],
                "result": {"imported": [], "skipped": []},
            });
            write
                .write_all(format!("{import_response}\n").as_bytes())
                .await
                .unwrap();

            let list_request: serde_json::Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(list_request["method"], "ListProviderCredentials");
            let list_response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": list_request["id"],
                "result": {"env_compat": true, "check_ttl_secs": 600, "credentials": []},
            });
            write
                .write_all(format!("{list_response}\n").as_bytes())
                .await
                .unwrap();
        });

        let mut app = Box::pin(test_app_connected(socket)).await;
        import_provider_credentials_from_env(&mut app).await;
        server.await.unwrap();
        assert!(app.cached_provider_credentials.is_some());
    }
}
