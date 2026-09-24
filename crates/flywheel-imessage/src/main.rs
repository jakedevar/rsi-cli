//! flywheel-imessage: iMessage bridge for RSI.
//!
//! Polls macOS chat.db for incoming iMessages, dispatches them to the
//! RSI daemon via JSON-RPC, subscribes to push events for responses,
//! and sends replies back through iMessage via AppleScript.

mod access;
mod applescript;
mod chatdb;
mod chunker;
mod config;
mod daemon_client;
mod debounce;
mod echo_cache;
mod push_stream;
mod router;
mod state;

use config::ImessageConfig;
use daemon_client::DaemonClient;
use rsi_common::types::SessionProvider;
use state::BridgeState;
use std::path::PathBuf;

#[tokio::main]
async fn main() {
    // Initialize logging
    tracing_subscriber::fmt().with_target(false).init();

    // Parse CLI args
    let config_path = parse_args();

    // Load config
    let config = match ImessageConfig::load(&config_path) {
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

    // Print startup banner
    tracing::info!("flywheel-imessage bridge starting");
    tracing::info!("  config: {}", config_path.display());
    tracing::info!("  chat.db: {}", config.chat_db_path().display());
    tracing::info!("  poll interval: {}ms", config.poll_interval_ms);
    tracing::info!("  dm policy: {:?}", config.dm_policy);
    tracing::info!(
        "  default working dir: {}",
        config.default_working_dir.display()
    );
    if !config.allow_from.is_empty() {
        tracing::info!("  allowlist: {} entries", config.allow_from.len());
    }

    // Startup health checks
    let socket_path = DaemonClient::default_socket_path();
    if let Err(msg) = startup_health_checks(&config, &socket_path).await {
        tracing::error!("Startup check failed: {}", msg);
        std::process::exit(1);
    }

    // Load persistent state
    let mut bridge_state = BridgeState::load();

    // Connect to daemon
    let mut client = DaemonClient::new(socket_path.clone());
    client.connect_with_backoff().await;

    // Initialize subsystems
    let echo_cache = echo_cache::SentMessageCache::new(config.echo_cache_ttl_ms);
    let mut debouncer = debounce::Debouncer::new(config.debounce_ms);

    // Start push stream for outbound replies
    let mut push = push_stream::PushStream::spawn(&socket_path);

    // Open chat.db
    let chat_db_path = config.chat_db_path();
    let chatdb = match chatdb::ChatDb::open(&chat_db_path) {
        Ok(db) => db,
        Err(e) => {
            tracing::error!(
                "Failed to open chat.db at {}: {}",
                chat_db_path.display(),
                e
            );
            tracing::error!("Hint: On macOS, grant Full Disk Access to your terminal app");
            std::process::exit(1);
        }
    };

    tracing::info!("Bridge ready. Entering main loop.");

    // Main event loop
    let mut poll_interval =
        tokio::time::interval(std::time::Duration::from_millis(config.poll_interval_ms));

    loop {
        tokio::select! {
            // Periodic chat.db poll
            _ = poll_interval.tick() => {
                match chatdb.poll(bridge_state.last_seen_rowid) {
                    Ok(messages) => {
                        if !messages.is_empty() {
                            let max_rowid = messages.iter().map(|m| m.rowid).max().unwrap_or(bridge_state.last_seen_rowid);
                            bridge_state.last_seen_rowid = max_rowid;

                            for msg in messages {
                                // Access control
                                if !access::is_allowed(&msg.sender, &config) {
                                    tracing::debug!("Dropping message from denied sender: {}", msg.sender);
                                    continue;
                                }

                                // Echo detection
                                let chat_id = msg.chat_identifier.as_deref().unwrap_or("");
                                if echo_cache.is_echo(chat_id, &msg.text) {
                                    tracing::debug!("Dropping echo: {}", msg.text.chars().take(50).collect::<String>());
                                    continue;
                                }

                                // Feed into debouncer
                                debouncer.push(msg);
                            }

                            bridge_state.save();
                        }
                    }
                    Err(e) => {
                        tracing::warn!("chat.db poll error: {}", e);
                    }
                }

                // Flush debounced messages
                let flushed = debouncer.flush();
                for msg in flushed {
                    let chat_identifier = msg.chat_identifier.clone().unwrap_or_default();
                    match router::route(&msg, &config, &bridge_state) {
                        router::RouterAction::LaunchSession { query, project_id } => {
                            let provider = config.default_provider.as_deref()
                                .and_then(parse_provider)
                                .unwrap_or_default();
                            match client.launch_session(
                                &query,
                                &config.default_working_dir,
                                provider,
                                config.default_model.as_deref(),
                                project_id.or(config.default_project_id),
                            ).await {
                                Ok(result) => {
                                    // Try to extract session_id from the response
                                    if let Some(sid_str) = result.get("session_id").and_then(|v| v.as_str())
                                        && let Ok(sid) = uuid::Uuid::parse_str(sid_str) {
                                            bridge_state.map_chat_to_session(&chat_identifier, sid);
                                            bridge_state.save();
                                            tracing::info!("Launched session {} for chat {}", sid, chat_identifier);
                                        }
                                    let _ = applescript::send_imessage(&msg.sender, "Session launched. I'll send the response when it's ready.").await;
                                }
                                Err(e) => {
                                    tracing::error!("Failed to launch session: {}", e);
                                    let _ = applescript::send_imessage(&msg.sender, &format!("Failed to launch session: {}", e)).await;
                                }
                            }
                        }
                        router::RouterAction::ContinueSession { session_id, query } => {
                            match client.continue_session(session_id, &query).await {
                                Ok(()) => {
                                    tracing::info!("Continued session {} with query", session_id);
                                }
                                Err(e) => {
                                    tracing::error!("Failed to continue session: {}", e);
                                    let _ = applescript::send_imessage(&msg.sender, &format!("Failed to continue session: {}", e)).await;
                                }
                            }
                        }
                        router::RouterAction::AnswerQuestion { session_id, response } => {
                            match client.answer_question(session_id, &response).await {
                                Ok(()) => {
                                    tracing::info!("Answered question for session {}", session_id);
                                }
                                Err(e) => {
                                    tracing::error!("Failed to answer question: {}", e);
                                    let _ = applescript::send_imessage(&msg.sender, &format!("Failed to approve: {}", e)).await;
                                }
                            }
                        }
                        router::RouterAction::InterruptSession { session_id } => {
                            match client.interrupt_session(session_id).await {
                                Ok(()) => {
                                    let _ = applescript::send_imessage(&msg.sender, "Session interrupted.").await;
                                }
                                Err(e) => {
                                    let _ = applescript::send_imessage(&msg.sender, &format!("Failed to interrupt: {}", e)).await;
                                }
                            }
                        }
                        router::RouterAction::ListSessions => {
                            match client.list_sessions().await {
                                Ok(sessions) => {
                                    let reply = router::format_session_list(&sessions);
                                    let _ = applescript::send_imessage(&msg.sender, &reply).await;
                                }
                                Err(e) => {
                                    let _ = applescript::send_imessage(&msg.sender, &format!("Failed to list sessions: {}", e)).await;
                                }
                            }
                        }
                        router::RouterAction::GetStatus { session_name } => {
                            match router::handle_status(&mut client, session_name.as_deref()).await {
                                Ok(reply) => {
                                    let _ = applescript::send_imessage(&msg.sender, &reply).await;
                                }
                                Err(e) => {
                                    let _ = applescript::send_imessage(&msg.sender, &format!("Status error: {}", e)).await;
                                }
                            }
                        }
                        router::RouterAction::SendHelp => {
                            let help = router::help_text();
                            let _ = applescript::send_imessage(&msg.sender, &help).await;
                        }
                        router::RouterAction::RouteToMapped { text } => {
                            if let Some(session_id) = bridge_state.get_session_for_chat(&chat_identifier) {
                                match client.continue_session(session_id, &text).await {
                                    Ok(()) => {
                                        tracing::info!("Routed message to mapped session {}", session_id);
                                    }
                                    Err(e) => {
                                        tracing::warn!("Failed to route to session {}: {}", session_id, e);
                                        let _ = applescript::send_imessage(&msg.sender, &format!("Session error: {}. Use /new to start a new session.", e)).await;
                                    }
                                }
                            } else {
                                // No mapped session — start a new one with this text
                                let provider = config.default_provider.as_deref()
                                    .and_then(parse_provider)
                                    .unwrap_or_default();
                                match client.launch_session(
                                    &text,
                                    &config.default_working_dir,
                                    provider,
                                    config.default_model.as_deref(),
                                    config.default_project_id,
                                ).await {
                                    Ok(result) => {
                                        if let Some(sid_str) = result.get("session_id").and_then(|v| v.as_str())
                                            && let Ok(sid) = uuid::Uuid::parse_str(sid_str) {
                                                bridge_state.map_chat_to_session(&chat_identifier, sid);
                                                bridge_state.save();
                                            }
                                        let _ = applescript::send_imessage(&msg.sender, "New session launched. I'll send the response when it's ready.").await;
                                    }
                                    Err(e) => {
                                        let _ = applescript::send_imessage(&msg.sender, &format!("Failed to launch session: {}", e)).await;
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Push events from daemon
            event = push.recv() => {
                if let Some(event) = event {
                    handle_push_event(&event, &config, &mut bridge_state, &mut echo_cache.clone()).await;
                }
            }

            // Graceful shutdown
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("Shutting down...");
                bridge_state.save();
                break;
            }
        }
    }

    tracing::info!("flywheel-imessage bridge stopped.");
}

/// Handle a push event from the daemon (outbound reply pipeline).
async fn handle_push_event(
    event: &rsi_common::rpc::BusEvent,
    config: &ImessageConfig,
    bridge_state: &mut BridgeState,
    echo_cache: &mut echo_cache::SentMessageCache,
) {
    match event.event_type.as_str() {
        "conversation_event" => {
            // Extract session_id and check if it's an assistant message
            let data = &event.data;
            let role = data
                .get("data")
                .and_then(|d| d.get("event"))
                .and_then(|e| e.get("role"))
                .and_then(|r| r.as_str());
            let event_type = data
                .get("data")
                .and_then(|d| d.get("event"))
                .and_then(|e| e.get("event_type"))
                .and_then(|t| t.as_str());
            let session_id_str = data
                .get("data")
                .and_then(|d| d.get("session_id"))
                .and_then(|s| s.as_str());
            let content = data
                .get("data")
                .and_then(|d| d.get("event"))
                .and_then(|e| e.get("content"))
                .and_then(|c| c.as_str());

            if role == Some("Assistant")
                && event_type == Some("Message")
                && let (Some(sid_str), Some(text)) = (session_id_str, content)
                && let Ok(session_id) = uuid::Uuid::parse_str(sid_str)
            {
                // Reverse lookup: session_id -> chat_identifier
                if let Some(chat_id) = bridge_state.get_chat_for_session(session_id) {
                    // Look up the sender handle for this chat
                    if let Some(sender) = bridge_state.get_sender_for_chat(&chat_id) {
                        let chunks = chunker::chunk_message(text, config.max_message_length);
                        for chunk in &chunks {
                            echo_cache.remember(&chat_id, chunk);
                            if let Err(e) = applescript::send_imessage(&sender, chunk).await {
                                tracing::warn!("Failed to send iMessage reply: {}", e);
                            }
                            // Brief delay between chunks
                            if chunks.len() > 1 {
                                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                            }
                        }
                    }
                }
            }
        }
        "session_status_changed" => {
            let data = &event.data;
            let new_status = data
                .get("data")
                .and_then(|d| d.get("new_status"))
                .and_then(|s| s.as_str());
            let session_id_str = data
                .get("data")
                .and_then(|d| d.get("session_id"))
                .and_then(|s| s.as_str());

            if let (Some(sid_str), Some(status)) = (session_id_str, new_status)
                && let Ok(session_id) = uuid::Uuid::parse_str(sid_str)
                && let Some(chat_id) = bridge_state.get_chat_for_session(session_id)
                && let Some(sender) = bridge_state.get_sender_for_chat(&chat_id)
            {
                let notification = match status {
                    "WaitingApproval" => Some(format!(
                        "Session needs approval. Reply /approve {} [message] to continue.",
                        &sid_str[..8]
                    )),
                    "Completed" => Some("Session completed.".to_string()),
                    "Failed" => Some("Session failed.".to_string()),
                    "Interrupted" => Some("Session interrupted.".to_string()),
                    _ => None,
                };
                if let Some(msg) = notification {
                    let _ = applescript::send_imessage(&sender, &msg).await;
                }
            }
        }
        _ => {}
    }
}

fn parse_args() -> PathBuf {
    let args: Vec<String> = std::env::args().collect();
    let mut config_path = ImessageConfig::default_path();

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
                println!("flywheel-imessage — iMessage bridge for RSI");
                println!();
                println!("USAGE:");
                println!("    flywheel-imessage [OPTIONS]");
                println!();
                println!("OPTIONS:");
                println!(
                    "    -c, --config <PATH>    Config file path (default: ~/.rsi/imessage.toml)"
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
    config: &ImessageConfig,
    socket_path: &std::path::Path,
) -> std::result::Result<(), String> {
    // Check chat.db exists and is readable
    let chat_db_path = config.chat_db_path();
    if !chat_db_path.exists() {
        return Err(format!(
            "chat.db not found at {}. Is this running on macOS with Messages.app configured?",
            chat_db_path.display()
        ));
    }

    // Check daemon socket exists
    if !socket_path.exists() {
        tracing::warn!(
            "Daemon socket not found at {}. Bridge will retry connection with backoff.",
            socket_path.display()
        );
    }

    // Check working directory exists
    if !config.default_working_dir.exists() {
        return Err(format!(
            "default_working_dir does not exist: {}",
            config.default_working_dir.display()
        ));
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
