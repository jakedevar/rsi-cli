//! Operator-only MCP server configuration actions.

use crate::app::App;
use rsi_common::mcp::McpCredentialMetadata;

use super::daemon_config::require_authoritative_config;

fn replace_credential(app: &mut App, metadata: McpCredentialMetadata) {
    if let Some(list) = app.cached_mcp_servers.as_mut() {
        if let Some(summary) = list
            .servers
            .iter_mut()
            .find(|summary| summary.definition.id == metadata.id)
        {
            summary.credential = metadata;
        }
    }
}

#[allow(clippy::future_not_send)]
pub async fn refresh_mcp_servers(app: &mut App) {
    if !require_authoritative_config(app) {
        return;
    }
    match app.client.list_mcp_servers().await {
        Ok(result) => app.cached_mcp_servers = Some(result),
        Err(error) => {
            tracing::warn!("Failed to list MCP servers: {}", error);
            app.notify_error(format!("Failed to refresh MCP servers: {error}"));
        }
    }
}

#[allow(clippy::future_not_send)]
pub async fn upsert_mcp_server(app: &mut App, server: rsi_common::mcp::McpServerDefinition) {
    if !require_authoritative_config(app) {
        return;
    }
    if let Err(error) = app.client.upsert_mcp_server(server.clone()).await {
        tracing::warn!("Failed to save MCP server: {}", error);
        app.notify_error(format!("Failed to save MCP server: {error}"));
        return;
    }
    refresh_mcp_servers(app).await;
}

#[allow(clippy::future_not_send)]
pub async fn set_mcp_server_enabled(app: &mut App, id: String, enabled: bool) {
    if !require_authoritative_config(app) {
        return;
    }
    if let Err(error) = app.client.set_mcp_server_enabled(id, enabled).await {
        tracing::warn!("Failed to update MCP server enablement: {}", error);
        app.notify_error(format!("Failed to update MCP server: {error}"));
        return;
    }
    refresh_mcp_servers(app).await;
}

#[allow(clippy::future_not_send)]
pub async fn set_mcp_server_secret(
    app: &mut App,
    id: String,
    secret: crate::types::McpSecretString,
) {
    if !require_authoritative_config(app) {
        return;
    }
    match app.client.set_mcp_server_secret(id, secret).await {
        Ok(metadata) => {
            replace_credential(app, metadata);
            app.notify_success("MCP credential set");
        }
        Err(error) => {
            tracing::warn!("Failed to set MCP credential: {}", error);
            app.notify_error(format!("Failed to set MCP credential: {error}"));
        }
    }
}

#[allow(clippy::future_not_send)]
pub async fn rotate_mcp_server_secret(
    app: &mut App,
    id: String,
    secret: crate::types::McpSecretString,
) {
    if !require_authoritative_config(app) {
        return;
    }
    match app.client.rotate_mcp_server_secret(id, secret).await {
        Ok(metadata) => {
            replace_credential(app, metadata);
            app.notify_success("MCP credential rotated");
        }
        Err(error) => {
            tracing::warn!("Failed to rotate MCP credential: {}", error);
            app.notify_error(format!("Failed to rotate MCP credential: {error}"));
        }
    }
}

#[allow(clippy::future_not_send)]
pub async fn clear_mcp_server_secret(app: &mut App, id: String) {
    if !require_authoritative_config(app) {
        return;
    }
    match app.client.clear_mcp_server_secret(id).await {
        Ok(metadata) => {
            replace_credential(app, metadata);
            app.notify_success("MCP credential cleared");
        }
        Err(error) => {
            tracing::warn!("Failed to clear MCP credential: {}", error);
            app.notify_error(format!("Failed to clear MCP credential: {error}"));
        }
    }
}
