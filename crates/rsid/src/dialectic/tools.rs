//! Tool definitions and execution for the dialectic query engine.

use crate::error::Result;
use crate::memory::manager::MemoryManager;
use crate::session::SessionManager;
use rsi_common::rpc::DialecticSource;
use serde_json::Value;
use std::sync::Arc;
use uuid::Uuid;

/// Return OpenAI function-calling tool definitions for the dialectic agent.
pub fn tool_definitions() -> Value {
    serde_json::json!([
        {
            "type": "function",
            "function": {
                "name": "search_memory",
                "description": "Search indexed memory files and session transcripts for relevant content. Returns scored text snippets.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "Search query (natural language or keywords)" },
                        "max_results": { "type": "integer", "description": "Maximum results to return (default 5, max 10)" }
                    },
                    "required": ["query"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "list_sessions",
                "description": "List recent sessions with metadata (title, status, provider, timestamps). Optionally filter by project.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "limit": { "type": "integer", "description": "Maximum sessions to return (default 10, max 50)" },
                        "project_id": { "type": "string", "description": "Filter by project UUID (optional)" }
                    }
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "get_session_detail",
                "description": "Get detailed metadata for a specific session by ID.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "session_id": { "type": "string", "description": "Session UUID" }
                    },
                    "required": ["session_id"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "get_conversation_excerpt",
                "description": "Get conversation text (user and assistant messages) from a specific session. Returns the most recent events.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "session_id": { "type": "string", "description": "Session UUID" },
                        "max_events": { "type": "integer", "description": "Maximum events to return (default 20, max 50)" }
                    },
                    "required": ["session_id"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "get_project_info",
                "description": "Get project metadata (name, path, color) by project ID.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "project_id": { "type": "string", "description": "Project UUID" }
                    },
                    "required": ["project_id"]
                }
            }
        }
    ])
}

/// Execute a tool call and return the result as a JSON string + source citation.
///
/// `project_id` is the dialectic query's enforced project scope (from
/// `QueryMemoryParams.project_id`). It restricts:
/// - `search_memory` to the project's indexed memory (agent cannot widen)
/// - `list_sessions` to the project when the tool args omit `project_id`
/// - `get_session_detail` / `get_conversation_excerpt` reject sessions
///   that belong to a different project than the active scope
pub async fn execute_tool(
    name: &str,
    arguments: &Value,
    memory: &Option<MemoryManager>,
    sessions: &Arc<SessionManager>,
    project_id: Option<Uuid>,
) -> Result<(String, Option<DialecticSource>)> {
    match name {
        "search_memory" => execute_search_memory(arguments, memory, project_id).await,
        "list_sessions" => execute_list_sessions(arguments, sessions, project_id).await,
        "get_session_detail" => execute_get_session_detail(arguments, sessions, project_id).await,
        "get_conversation_excerpt" => {
            execute_get_conversation_excerpt(arguments, sessions, project_id).await
        }
        "get_project_info" => execute_get_project_info(arguments, sessions).await,
        _ => Ok((format!("Unknown tool: {name}"), None)),
    }
}

async fn execute_search_memory(
    args: &Value,
    memory: &Option<MemoryManager>,
    project_id: Option<Uuid>,
) -> Result<(String, Option<DialecticSource>)> {
    let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
    let max_results = args
        .get("max_results")
        .and_then(|v| v.as_u64())
        .unwrap_or(5)
        .min(10) as usize;

    let mm = match memory {
        Some(m) => m,
        None => {
            return Ok(("Memory system is not available.".to_string(), None));
        }
    };

    // Project scope is enforced from the caller — agent tool args do NOT
    // include a project_id field (plan §D2). Pass the captured scope down.
    let results = mm
        .search(query, Some(max_results), None, project_id)
        .await?;

    if results.is_empty() {
        return Ok((format!("No memory results found for: {query}"), None));
    }

    let mut output = Vec::new();
    for r in &results {
        output.push(format!(
            "[{} lines {}-{} score={:.2}] {}",
            r.path, r.start_line, r.end_line, r.score, r.snippet
        ));
    }
    let result_text = output.join("\n---\n");
    let source = DialecticSource {
        kind: "memory".to_string(),
        label: format!("{} results", results.len()),
        detail: Some(format!("query: {query}")),
    };
    Ok((result_text, Some(source)))
}

async fn execute_list_sessions(
    args: &Value,
    sessions: &Arc<SessionManager>,
    active_project_id: Option<Uuid>,
) -> Result<(String, Option<DialecticSource>)> {
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .unwrap_or(10)
        .min(50) as usize;
    // Honor caller-supplied project_id if provided, otherwise default to
    // the dialectic query's active project scope (plan §Phase 6). This
    // prevents an agent from listing every project's sessions just by
    // omitting the filter.
    let project_filter = args
        .get("project_id")
        .and_then(|v| v.as_str())
        .and_then(|s| Uuid::parse_str(s).ok())
        .or(active_project_id);

    let all = if let Some(pid) = project_filter {
        sessions.list_sessions_by_project(Some(pid)).await
    } else {
        sessions.list_sessions().await
    };

    let items: Vec<_> = all.iter().take(limit).collect();
    if items.is_empty() {
        return Ok(("No sessions found.".to_string(), None));
    }

    let mut output = Vec::new();
    for s in &items {
        let title = s.title.as_deref().unwrap_or("(untitled)");
        output.push(format!(
            "- {} [{}] {:?} | {:?} | {}",
            s.id, title, s.status, s.provider, s.created_at
        ));
    }
    let result_text = output.join("\n");
    let source = DialecticSource {
        kind: "session".to_string(),
        label: format!("{} sessions listed", items.len()),
        detail: None,
    };
    Ok((result_text, Some(source)))
}

async fn execute_get_session_detail(
    args: &Value,
    sessions: &Arc<SessionManager>,
    active_project_id: Option<Uuid>,
) -> Result<(String, Option<DialecticSource>)> {
    let sid = args
        .get("session_id")
        .and_then(|v| v.as_str())
        .and_then(|s| Uuid::parse_str(s).ok());

    let session_id = match sid {
        Some(id) => id,
        None => {
            return Ok(("Invalid or missing session_id.".to_string(), None));
        }
    };

    let session = sessions.get_session(session_id).await;
    // When the dialectic query is project-scoped, reject sessions that
    // belong to a different project (plan §Phase 6). The agent cannot use
    // `get_session_detail` to pivot into another project's history.
    if let (Some(active), Some(s)) = (active_project_id, &session)
        && s.project_id != Some(active)
    {
        return Ok((
            format!("Session {session_id} is outside the active project scope and cannot be read."),
            None,
        ));
    }
    match session {
        Some(s) => {
            let title = s.title.as_deref().unwrap_or("(untitled)");
            let summary = s.short_summary.as_deref().unwrap_or("(no summary)");
            let detail = format!(
                "Session: {}\nTitle: {}\nStatus: {:?}\nProvider: {:?}\nModel: {}\nCreated: {}\nWorking Dir: {}\nSummary: {}",
                s.id,
                title,
                s.status,
                s.provider,
                s.model.as_deref().unwrap_or("default"),
                s.created_at,
                s.working_dir.display(),
                summary,
            );
            let source = DialecticSource {
                kind: "session".to_string(),
                label: title.to_string(),
                detail: Some(s.id.to_string()),
            };
            Ok((detail, Some(source)))
        }
        None => Ok((format!("Session {session_id} not found."), None)),
    }
}

async fn execute_get_conversation_excerpt(
    args: &Value,
    sessions: &Arc<SessionManager>,
    active_project_id: Option<Uuid>,
) -> Result<(String, Option<DialecticSource>)> {
    let sid = args
        .get("session_id")
        .and_then(|v| v.as_str())
        .and_then(|s| Uuid::parse_str(s).ok());
    let max_events = args
        .get("max_events")
        .and_then(|v| v.as_u64())
        .unwrap_or(20)
        .min(50) as usize;

    let session_id = match sid {
        Some(id) => id,
        None => {
            return Ok(("Invalid or missing session_id.".to_string(), None));
        }
    };

    // Reject cross-project reads when scope is active (plan §Phase 6).
    if let Some(active) = active_project_id {
        let owner = sessions.get_session(session_id).await;
        if owner
            .as_ref()
            .map(|s| s.project_id != Some(active))
            .unwrap_or(false)
        {
            return Ok((
                format!(
                    "Session {session_id} is outside the active project scope and cannot be read."
                ),
                None,
            ));
        }
    }

    let events = sessions.get_conversation(session_id).await;
    match events {
        Ok(evts) => {
            // Take the last max_events events
            let start = evts.len().saturating_sub(max_events);
            let excerpt: Vec<_> = evts[start..]
                .iter()
                .filter(|e| matches!(e.event_type, rsi_common::types::EventType::Message))
                .map(|e| {
                    let content = if e.content.len() > 500 {
                        format!("{}...", &e.content[..500])
                    } else {
                        e.content.clone()
                    };
                    let role_label = e
                        .role
                        .map(|r| format!("{:?}", r))
                        .unwrap_or_else(|| "unknown".to_string());
                    format!("[{}] {}", role_label, content)
                })
                .collect();

            if excerpt.is_empty() {
                return Ok((
                    format!("No conversation events for session {session_id}."),
                    None,
                ));
            }

            let result_text = excerpt.join("\n---\n");
            let source = DialecticSource {
                kind: "session".to_string(),
                label: format!("conversation ({} events)", excerpt.len()),
                detail: Some(session_id.to_string()),
            };
            Ok((result_text, Some(source)))
        }
        Err(e) => Ok((format!("Failed to get conversation: {e}"), None)),
    }
}

async fn execute_get_project_info(
    args: &Value,
    sessions: &Arc<SessionManager>,
) -> Result<(String, Option<DialecticSource>)> {
    let pid = args
        .get("project_id")
        .and_then(|v| v.as_str())
        .and_then(|s| Uuid::parse_str(s).ok());

    let project_id = match pid {
        Some(id) => id,
        None => {
            return Ok(("Invalid or missing project_id.".to_string(), None));
        }
    };

    let project = sessions.get_project(project_id).await;
    match project {
        Ok(Some(p)) => {
            let path_str = p
                .path
                .as_ref()
                .map(|pb| pb.display().to_string())
                .unwrap_or_else(|| "(none)".to_string());
            let detail = format!(
                "Project: {}\nName: {}\nPath: {}\nColor: {}",
                p.id, p.name, path_str, p.color,
            );
            let source = DialecticSource {
                kind: "project".to_string(),
                label: p.name.clone(),
                detail: Some(p.id.to_string()),
            };
            Ok((detail, Some(source)))
        }
        Ok(None) => Ok((format!("Project {project_id} not found."), None)),
        Err(e) => Ok((format!("Failed to get project: {e}"), None)),
    }
}
