//! flywheel-signal: Signal bridge for RSI (Linux).
//!
//! Polls `signal-cli --output=json receive` for incoming DMs, dispatches them
//! to the RSI daemon over JSON-RPC, subscribes to push events for
//! responses, and sends replies back through `signal-cli send`.

mod access;
mod chunker;
mod config;
mod daemon_client;
mod debounce;
mod echo_cache;
mod push_stream;
mod router;
mod signal_cli;
mod state;

use config::SignalConfig;
use daemon_client::DaemonClient;
use rsi_common::types::SessionProvider;
use serde_json::Value;
use signal_cli::{InboundMessage, SignalCli};
use state::BridgeState;
use std::path::{Path, PathBuf};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().with_target(false).init();

    let config_path = parse_args();

    let config = match SignalConfig::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Failed to load config: {}", e);
            std::process::exit(1);
        }
    };

    if !config.enabled {
        tracing::info!("Bridge is disabled in config. Exiting.");
        return;
    }

    tracing::info!("flywheel-signal bridge starting");
    tracing::info!("  config: {}", config_path.display());
    tracing::info!("  account: {}", config.account);
    tracing::info!("  poll interval: {}ms", config.poll_interval_ms);
    tracing::info!("  dm policy: {:?}", config.dm_policy);
    tracing::info!(
        "  default working dir: {}",
        config.default_working_dir.display()
    );
    if !config.allow_from.is_empty() {
        tracing::info!("  allowlist: {} entries", config.allow_from.len());
    }

    let socket_path = DaemonClient::default_socket_path();
    if let Err(msg) = startup_health_checks(&config, &socket_path).await {
        tracing::error!("Startup check failed: {}", msg);
        std::process::exit(1);
    }

    let mut bridge_state = BridgeState::load();

    let mut client = DaemonClient::new(socket_path.clone());
    client.connect_with_backoff().await;

    let mut echo_cache = echo_cache::SentMessageCache::new(config.echo_cache_ttl_ms);
    let mut debouncer = debounce::Debouncer::new(config.debounce_ms);

    let mut push = push_stream::PushStream::spawn(&socket_path);

    let signal = match SignalCli::new(config.account.clone(), config.signal_cli_path.clone()) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("signal-cli init failed: {}", e);
            std::process::exit(1);
        }
    };

    tracing::info!("Bridge ready. Entering main loop.");

    let mut poll_interval =
        tokio::time::interval(std::time::Duration::from_millis(config.poll_interval_ms));

    loop {
        tokio::select! {
            _ = poll_interval.tick() => {
                handle_poll_tick(
                    &signal,
                    &config,
                    &mut bridge_state,
                    &echo_cache,
                    &mut debouncer,
                ).await;

                let flushed = debouncer.flush();
                for msg in flushed {
                    dispatch_message(
                        msg,
                        &config,
                        &mut client,
                        &signal,
                        &mut bridge_state,
                        &mut echo_cache,
                    ).await;
                }
            }

            event = push.recv() => {
                if let Some(event) = event {
                    handle_push_event(
                        &event,
                        &config,
                        &mut bridge_state,
                        &mut echo_cache,
                        &signal,
                    ).await;
                }
            }

            _ = tokio::signal::ctrl_c() => {
                tracing::info!("Shutting down...");
                bridge_state.save();
                break;
            }
        }
    }

    tracing::info!("flywheel-signal bridge stopped.");
}

/// Poll signal-cli, filter, and stage messages into the debouncer.
async fn handle_poll_tick(
    signal: &SignalCli,
    config: &SignalConfig,
    state: &mut BridgeState,
    echo_cache: &echo_cache::SentMessageCache,
    debouncer: &mut debounce::Debouncer,
) {
    match signal.poll().await {
        Ok(messages) => {
            if messages.is_empty() {
                return;
            }
            let max_ts = messages
                .iter()
                .map(|m| m.timestamp_ms)
                .max()
                .unwrap_or(state.last_seen_timestamp);
            if max_ts > state.last_seen_timestamp {
                state.last_seen_timestamp = max_ts;
            }
            for msg in messages {
                if !access::is_allowed(&msg.sender, config) {
                    tracing::debug!("Dropping message from denied sender: {}", msg.sender);
                    continue;
                }
                if echo_cache.is_echo(&msg.sender, &msg.text) {
                    tracing::debug!(
                        "Dropping echo from {} (is_sync={}): {}",
                        msg.sender,
                        msg.is_sync,
                        msg.text.chars().take(40).collect::<String>()
                    );
                    continue;
                }
                if msg.is_sync {
                    tracing::debug!(
                        "Sync-replay from {} routed as user input (not an echo)",
                        msg.sender
                    );
                }
                debouncer.push(msg);
            }
            state.save();
        }
        Err(e) => tracing::warn!("signal-cli poll error: {}", e),
    }
}

/// Dispatch a coalesced inbound message through the router and daemon RPC.
async fn dispatch_message(
    msg: InboundMessage,
    config: &SignalConfig,
    client: &mut DaemonClient,
    signal: &SignalCli,
    state: &mut BridgeState,
    echo_cache: &mut echo_cache::SentMessageCache,
) {
    let sender = msg.sender.clone();
    match router::route(&msg, config, state) {
        router::RouterAction::LaunchSession { query, project_id } => {
            let provider = config
                .default_provider
                .as_deref()
                .and_then(parse_provider)
                .unwrap_or_default();
            match client
                .launch_session(
                    &query,
                    &config.default_working_dir,
                    provider,
                    config.default_model.as_deref(),
                    project_id.or(config.default_project_id),
                )
                .await
            {
                Ok(result) => {
                    if let Some(sid_str) = result.get("session_id").and_then(|v| v.as_str())
                        && let Ok(sid) = uuid::Uuid::parse_str(sid_str)
                    {
                        state.map_sender_to_session(&sender, sid);
                        state.save();
                        tracing::info!("Launched session {} for {}", sid, sender);
                    }
                    send_reply(
                        signal,
                        echo_cache,
                        &sender,
                        "Session launched. I'll reply when it's ready.",
                    )
                    .await;
                }
                Err(e) => {
                    tracing::error!("Failed to launch session: {}", e);
                    send_reply(
                        signal,
                        echo_cache,
                        &sender,
                        &format!("Launch failed: {}", e),
                    )
                    .await;
                }
            }
        }
        router::RouterAction::ContinueSession { session_id, query } => {
            if let Err(e) = client.continue_session(session_id, &query).await {
                tracing::error!("Failed to continue session {}: {}", session_id, e);
                send_reply(
                    signal,
                    echo_cache,
                    &sender,
                    &format!("Continue failed: {}", e),
                )
                .await;
            }
        }
        router::RouterAction::InterruptSession { session_id } => {
            match client.interrupt_session(session_id).await {
                Ok(()) => {
                    send_reply(signal, echo_cache, &sender, "Session interrupted.").await;
                }
                Err(e) => {
                    send_reply(
                        signal,
                        echo_cache,
                        &sender,
                        &format!("Interrupt failed: {}", e),
                    )
                    .await;
                }
            }
        }
        router::RouterAction::GetStatus { session_id } => {
            let name = session_id.map(|s| s.to_string());
            match router::handle_status(client, name.as_deref()).await {
                Ok(reply) => {
                    send_reply(signal, echo_cache, &sender, &reply).await;
                }
                Err(e) => {
                    send_reply(signal, echo_cache, &sender, &format!("Status error: {}", e)).await;
                }
            }
        }
        router::RouterAction::SendHelp => {
            send_reply(signal, echo_cache, &sender, &router::help_text()).await;
        }
        router::RouterAction::RouteToMapped { text } => {
            if let Some(session_id) = state.get_session_for_sender(&sender) {
                if let Err(e) = client.continue_session(session_id, &text).await {
                    tracing::warn!("Failed to route to session {}: {}", session_id, e);
                    send_reply(
                        signal,
                        echo_cache,
                        &sender,
                        &format!("Session error: {}. !new to start fresh.", e),
                    )
                    .await;
                }
            } else {
                // No active session — launch one with this text.
                let provider = config
                    .default_provider
                    .as_deref()
                    .and_then(parse_provider)
                    .unwrap_or_default();
                match client
                    .launch_session(
                        &text,
                        &config.default_working_dir,
                        provider,
                        config.default_model.as_deref(),
                        config.default_project_id,
                    )
                    .await
                {
                    Ok(result) => {
                        if let Some(sid_str) = result.get("session_id").and_then(|v| v.as_str())
                            && let Ok(sid) = uuid::Uuid::parse_str(sid_str)
                        {
                            state.map_sender_to_session(&sender, sid);
                            state.save();
                        }
                        send_reply(
                            signal,
                            echo_cache,
                            &sender,
                            "New session launched. I'll reply when ready.",
                        )
                        .await;
                    }
                    Err(e) => {
                        send_reply(
                            signal,
                            echo_cache,
                            &sender,
                            &format!("Launch failed: {}", e),
                        )
                        .await;
                    }
                }
            }
        }
    }
}

/// Convenience wrapper: remember an outbound text in the echo cache, then send.
/// Sync-replay envelopes for our own sends will be filtered on the next poll.
async fn send_reply(
    signal: &SignalCli,
    echo_cache: &mut echo_cache::SentMessageCache,
    recipient: &str,
    text: &str,
) {
    echo_cache.remember(recipient, text);
    if let Err(e) = signal.send(recipient, text).await {
        tracing::warn!("Failed to send Signal reply to {}: {}", recipient, e);
    }
}

/// Handle a push event from the daemon (outbound reply pipeline).
async fn handle_push_event(
    event: &rsi_common::rpc::BusEvent,
    config: &SignalConfig,
    bridge_state: &mut BridgeState,
    echo_cache: &mut echo_cache::SentMessageCache,
    signal: &SignalCli,
) {
    match event.event_type.as_str() {
        "conversation_event" => {
            if let Some((session_id, text)) = assistant_message(event)
                && let Some(sender) = bridge_state.get_sender_for_session(session_id)
            {
                let chunks = chunker::chunk_message(text, config.max_message_length);
                for (i, chunk) in chunks.iter().enumerate() {
                    echo_cache.remember(&sender, chunk);
                    if let Err(e) = signal.send(&sender, chunk).await {
                        tracing::warn!("Failed to send Signal chunk: {}", e);
                    }
                    if chunks.len() > 1 && i + 1 < chunks.len() {
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    }
                }
                bridge_state.save();
            }
        }
        "session_status_changed" => {
            if let Some((session_id, status)) = status_change(event)
                && let Some(sender) = bridge_state.get_sender_for_session(session_id)
            {
                let notification = match status {
                    "WaitingApproval" => Some("Session needs approval.".to_string()),
                    "Completed" => Some("Session completed.".to_string()),
                    "Failed" => Some("Session failed.".to_string()),
                    "Interrupted" => Some("Session interrupted.".to_string()),
                    _ => None,
                };
                if let Some(msg) = notification {
                    echo_cache.remember(&sender, &msg);
                    if let Err(e) = signal.send(&sender, &msg).await {
                        tracing::warn!("Failed to send status notification: {}", e);
                    }
                }
            }
        }
        _ => {}
    }
}

fn bus_payload(data: &Value) -> &Value {
    data.get("data").unwrap_or(data)
}

fn assistant_message(event: &rsi_common::rpc::BusEvent) -> Option<(uuid::Uuid, &str)> {
    let data = bus_payload(&event.data);
    let event_data = data.get("event")?;
    let role = event_data.get("role").and_then(Value::as_str);
    let event_type = event_data.get("event_type").and_then(Value::as_str);
    if role != Some("Assistant") || event_type != Some("Message") {
        return None;
    }

    let session_id = data
        .get("session_id")
        .and_then(Value::as_str)
        .and_then(|s| uuid::Uuid::parse_str(s).ok())?;
    let content = event_data.get("content").and_then(Value::as_str)?;
    Some((session_id, content))
}

fn status_change(event: &rsi_common::rpc::BusEvent) -> Option<(uuid::Uuid, &str)> {
    let data = bus_payload(&event.data);
    let session_id = data
        .get("session_id")
        .and_then(Value::as_str)
        .and_then(|s| uuid::Uuid::parse_str(s).ok())?;
    let status = data.get("new_status").and_then(Value::as_str)?;
    Some((session_id, status))
}

fn parse_args() -> PathBuf {
    let args: Vec<String> = std::env::args().collect();
    let mut config_path = SignalConfig::default_path();

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--config" | "-c" => {
                if i + 1 < args.len() {
                    config_path = PathBuf::from(&args[i + 1]);
                    i += 2;
                } else {
                    tracing::error!("--config requires a path argument");
                    std::process::exit(1);
                }
            }
            "--help" | "-h" => {
                println!("flywheel-signal — Signal bridge for RSI (Linux, signal-cli)");
                println!();
                println!("USAGE:");
                println!("    flywheel-signal [OPTIONS]");
                println!();
                println!("OPTIONS:");
                println!(
                    "    -c, --config <PATH>    Config file path (default: ~/.rsi/signal.toml)"
                );
                println!("    -h, --help             Print help");
                std::process::exit(0);
            }
            _ => {
                tracing::error!("Unknown argument: {}", args[i]);
                std::process::exit(1);
            }
        }
    }

    config_path
}

async fn startup_health_checks(
    config: &SignalConfig,
    socket_path: &Path,
) -> std::result::Result<(), String> {
    // 1. signal-cli binary resolvable (config override first, then PATH)
    let binary = match &config.signal_cli_path {
        Some(p) => p.clone(),
        None => which::which("signal-cli").map_err(|_| {
            "signal-cli not found on PATH (install from https://github.com/AsamK/signal-cli or set signal_cli_path in config)"
                .to_string()
        })?,
    };

    // 2. signal-cli binary is executable
    let metadata = std::fs::metadata(&binary)
        .map_err(|e| format!("cannot stat signal-cli at {}: {}", binary.display(), e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(format!(
                "signal-cli at {} is not executable",
                binary.display()
            ));
        }
    }
    // Reference metadata unconditionally so non-unix builds (none expected, but
    // keeping the crate portable) do not generate an unused-variable warning.
    let _ = metadata;

    // 3. Account configured (non-empty E.164)
    if config.account.trim().is_empty() {
        return Err("config.account is empty (required: E.164, e.g. +15551234567)".to_string());
    }

    // 4. Working directory exists
    if !config.default_working_dir.exists() {
        return Err(format!(
            "default_working_dir does not exist: {}",
            config.default_working_dir.display()
        ));
    }

    // 5. Daemon socket — warn-only; DaemonClient::connect_with_backoff handles retry
    if !socket_path.exists() {
        tracing::warn!(
            "Daemon socket not found at {}. Bridge will retry with backoff.",
            socket_path.display()
        );
    }

    Ok(())
}

fn parse_provider(s: &str) -> Option<SessionProvider> {
    match s.to_lowercase().as_str() {
        "claude" => Some(SessionProvider::Claude),
        "codex" => Some(SessionProvider::Codex),
        "pioneer" => Some(SessionProvider::Pioneer),
        "openrouter" => Some(SessionProvider::OpenRouter),
        "bedrock" => Some(SessionProvider::Bedrock),
        "local" => Some(SessionProvider::Local),
        "gemini" | "antigravity" | "agy" => Some(SessionProvider::Antigravity),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use rsi_common::rpc::BusEvent;
    use serde_json::json;
    use uuid::Uuid;

    fn bus_event(event_type: &str, data: Value) -> BusEvent {
        BusEvent {
            event_type: event_type.to_string(),
            timestamp: Utc::now(),
            data,
        }
    }

    #[test]
    fn assistant_message_reads_current_bus_payload_shape() {
        let session_id = Uuid::new_v4();
        let event = bus_event(
            "conversation_event",
            json!({
                "session_id": session_id,
                "event": {
                    "event_type": "Message",
                    "role": "Assistant",
                    "content": "done"
                }
            }),
        );

        assert_eq!(assistant_message(&event), Some((session_id, "done")));
    }

    #[test]
    fn assistant_message_reads_legacy_nested_payload_shape() {
        let session_id = Uuid::new_v4();
        let event = bus_event(
            "conversation_event",
            json!({
                "data": {
                    "session_id": session_id,
                    "event": {
                        "event_type": "Message",
                        "role": "Assistant",
                        "content": "done"
                    }
                }
            }),
        );

        assert_eq!(assistant_message(&event), Some((session_id, "done")));
    }

    #[test]
    fn assistant_message_ignores_non_assistant_events() {
        let session_id = Uuid::new_v4();
        let event = bus_event(
            "conversation_event",
            json!({
                "session_id": session_id,
                "event": {
                    "event_type": "Message",
                    "role": "User",
                    "content": "hello"
                }
            }),
        );

        assert!(assistant_message(&event).is_none());
    }

    #[test]
    fn status_change_reads_current_bus_payload_shape() {
        let session_id = Uuid::new_v4();
        let event = bus_event(
            "session_status_changed",
            json!({
                "session_id": session_id,
                "new_status": "Completed"
            }),
        );

        assert_eq!(status_change(&event), Some((session_id, "Completed")));
    }
}
