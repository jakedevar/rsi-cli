//! Assist overlays (hub #3015 slice 13B): memory search, the AI command and AI
//! chat, the dialectic and the entity-card editor.
//!
//! Each command validates its input and forwards exactly one daemon method with
//! the request the TUI client sends (`crates/rsi/src/client.rs`,
//! `crates/rsi/src/prompt_processor.rs`):
//!
//! - `memory_search` → `MemorySearch {query, max_results: 10, project_id: null}`
//!   (`overlay/memory_search.rs` asks for ten global results on every edit);
//! - `ai_command` / `ai_chat` → `GenerateText {system, prompt}` with the TUI's
//!   own system prompts and message layout. The model always runs inside rsid:
//!   the desktop never calls a model API. It sends no `model`, `provider`,
//!   `base_url` or `api_key`, so rsid uses its configured prompt-processor
//!   model and no credential crosses the webview;
//! - `query_memory` → `QueryMemory {query, project_id, conversation_history}`;
//! - `get_entity_card` / `set_entity_card` → `GetEntityCard` / `SetEntityCard`
//!   for the project card (`entity_id` a project UUID) or the user card
//!   (`entity_id` `"self"`).
//!
//! The system prompts are fixed here, so the webview cannot choose one.

use serde::Deserialize;
use serde_json::{Value, json};
use tauri::State;

use crate::{AppState, CmdResult, forward, parse_uuid};

/// `crates/rsi/src/prompt_processor.rs` `AI_COMMAND_SYSTEM_PROMPT`, byte for byte.
pub const AI_COMMAND_SYSTEM_PROMPT: &str = "\
You are a text transformation assistant. The user will provide source text \
and an instruction. Apply the instruction to the source text and return ONLY \
the modified text. No explanation. No wrapper text. No markdown fences. \
If the instruction is unclear, make your best interpretation and apply it.";

/// `crates/rsi/src/prompt_processor.rs` `AI_CHAT_SYSTEM_PROMPT`, byte for byte.
pub const AI_CHAT_SYSTEM_PROMPT: &str = "\
You are a concise text analysis assistant. The user will provide source text \
and ask questions about it. Answer questions directly and briefly. \
Keep responses under 3 sentences unless the question requires more detail.";

/// Results the TUI's memory search asks for.
pub const MEMORY_RESULTS: u64 = 10;
/// `EntityCard::MAX_FACTS` in rsi-common.
pub const MAX_FACTS: usize = 40;

const MAX_QUERY_BYTES: usize = 8 * 1024;
const MAX_SOURCE_BYTES: usize = 256 * 1024;
const MAX_TURN_BYTES: usize = 64 * 1024;
const MAX_TURNS: usize = 128;
const MAX_FACT_BYTES: usize = 4 * 1024;

/// One message of an AI chat or dialectic conversation.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Turn {
    pub role: String,
    pub content: String,
}

fn nonblank(field: &str, text: &str, max: usize) -> Result<(), String> {
    if text.trim().is_empty() {
        return Err(format!("{field} is empty"));
    }
    if text.len() > max {
        return Err(format!("{field} exceeds {max} bytes"));
    }
    if text.contains('\0') {
        return Err(format!("{field} contains a NUL byte"));
    }
    Ok(())
}

/// The turns of a conversation: `user` / `assistant`, bounded, ending with the
/// question being sent (a `user` turn), as the TUI builds it.
fn conversation(turns: &[Turn]) -> Result<(), String> {
    if turns.is_empty() {
        return Err("the conversation is empty".into());
    }
    if turns.len() > MAX_TURNS {
        return Err(format!("the conversation exceeds {MAX_TURNS} messages"));
    }
    for (i, t) in turns.iter().enumerate() {
        if t.role != "user" && t.role != "assistant" {
            return Err(format!("message {i}: role must be user or assistant"));
        }
        nonblank(&format!("message {i}"), &t.content, MAX_TURN_BYTES)?;
    }
    if turns.last().map(|t| t.role.as_str()) != Some("user") {
        return Err("the last message must be the user's question".into());
    }
    Ok(())
}

/// `MemorySearch` params: the TUI's ten global results for `query`.
pub fn memory_search_params(query: &str) -> Result<Value, String> {
    nonblank("query", query, MAX_QUERY_BYTES)?;
    Ok(json!({ "query": query, "max_results": MEMORY_RESULTS, "project_id": null }))
}

/// `GenerateText` params for the AI command: the instruction applied to the
/// source text (`overlay/ai_command.rs`).
pub fn ai_command_params(instruction: &str, source_text: &str) -> Result<Value, String> {
    nonblank("instruction", instruction, MAX_QUERY_BYTES)?;
    nonblank("source text", source_text, MAX_SOURCE_BYTES)?;
    let prompt = format!(
        "INSTRUCTION: {}\n\nSOURCE TEXT:\n{}",
        instruction.trim(),
        source_text
    );
    Ok(json!({ "system": AI_COMMAND_SYSTEM_PROMPT, "prompt": prompt }))
}

/// `GenerateText` params for the AI chat: the source text and the whole
/// conversation, one `ROLE: text` line per message (`overlay/ai_chat.rs`).
pub fn ai_chat_params(source_text: &str, turns: &[Turn]) -> Result<Value, String> {
    nonblank("source text", source_text, MAX_SOURCE_BYTES)?;
    conversation(turns)?;
    let mut prompt = format!("SOURCE TEXT:\n{source_text}\n\n---\n\nCONVERSATION:\n");
    for t in turns {
        prompt.push_str(&format!("{}: {}\n", t.role.to_uppercase(), t.content));
    }
    Ok(json!({ "system": AI_CHAT_SYSTEM_PROMPT, "prompt": prompt }))
}

/// `QueryMemory` params (`overlay/dialectic.rs`): the question, the scoping
/// project (or null) and the conversation so far as `[role, text]` pairs, the
/// question last.
pub fn query_memory_params(
    query: &str,
    project_id: Option<&str>,
    history: &[Turn],
) -> Result<Value, String> {
    nonblank("query", query, MAX_QUERY_BYTES)?;
    conversation(history)?;
    let project = match project_id.map(str::trim).filter(|p| !p.is_empty()) {
        Some(p) => Value::String(parse_uuid("project_id", p)?),
        None => Value::Null,
    };
    let pairs: Vec<Value> = history.iter().map(|t| json!([t.role, t.content])).collect();
    Ok(json!({
        "query": query.trim(),
        "project_id": project,
        "conversation_history": pairs,
    }))
}

/// The card key: `("project", <uuid>)` or `("user", "self")`.
pub fn card_key(entity_type: &str, entity_id: &str) -> Result<(String, String), String> {
    match entity_type {
        "project" => Ok(("project".into(), parse_uuid("entity_id", entity_id)?)),
        "user" if entity_id == "self" => Ok(("user".into(), "self".into())),
        "user" => Err("the user card's entity_id is \"self\"".into()),
        _ => Err("entity_type must be project or user".into()),
    }
}

/// `GetEntityCard` params.
pub fn get_card_params(entity_type: &str, entity_id: &str) -> Result<Value, String> {
    let (t, id) = card_key(entity_type, entity_id)?;
    Ok(json!({ "entity_type": t, "entity_id": id }))
}

/// `SetEntityCard` params: the whole ordered fact list (it replaces the card).
pub fn set_card_params(
    entity_type: &str,
    entity_id: &str,
    facts: &[String],
) -> Result<Value, String> {
    let (t, id) = card_key(entity_type, entity_id)?;
    if facts.len() > MAX_FACTS {
        return Err(format!("a card holds at most {MAX_FACTS} facts"));
    }
    for (i, f) in facts.iter().enumerate() {
        nonblank(&format!("fact {}", i + 1), f, MAX_FACT_BYTES)?;
    }
    Ok(json!({ "entity_type": t, "entity_id": id, "facts": facts }))
}

#[tauri::command]
pub async fn memory_search(state: State<'_, AppState>, query: String) -> CmdResult {
    forward(&state, "MemorySearch", memory_search_params(&query)?).await
}

#[tauri::command]
pub async fn ai_command(
    state: State<'_, AppState>,
    instruction: String,
    source_text: String,
) -> CmdResult {
    let params = ai_command_params(&instruction, &source_text)?;
    forward(&state, "GenerateText", params).await
}

#[tauri::command]
pub async fn ai_chat(
    state: State<'_, AppState>,
    source_text: String,
    turns: Vec<Turn>,
) -> CmdResult {
    let params = ai_chat_params(&source_text, &turns)?;
    forward(&state, "GenerateText", params).await
}

#[tauri::command]
pub async fn query_memory(
    state: State<'_, AppState>,
    query: String,
    project_id: Option<String>,
    history: Vec<Turn>,
) -> CmdResult {
    let params = query_memory_params(&query, project_id.as_deref(), &history)?;
    forward(&state, "QueryMemory", params).await
}

#[tauri::command]
pub async fn get_entity_card(
    state: State<'_, AppState>,
    entity_type: String,
    entity_id: String,
) -> CmdResult {
    let params = get_card_params(&entity_type, &entity_id)?;
    forward(&state, "GetEntityCard", params).await
}

#[tauri::command]
pub async fn set_entity_card(
    state: State<'_, AppState>,
    entity_type: String,
    entity_id: String,
    facts: Vec<String>,
) -> CmdResult {
    let params = set_card_params(&entity_type, &entity_id, &facts)?;
    forward(&state, "SetEntityCard", params).await
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: &str = "E2CAE1D4-1833-48EB-A7E8-6F1DD480FDAE";
    const P_LOWER: &str = "e2cae1d4-1833-48eb-a7e8-6f1dd480fdae";

    fn turn(role: &str, content: &str) -> Turn {
        Turn {
            role: role.into(),
            content: content.into(),
        }
    }

    #[test]
    fn memory_search_is_the_tuis_ten_global_results() {
        assert_eq!(
            memory_search_params("rust tui").unwrap(),
            json!({ "query": "rust tui", "max_results": 10, "project_id": null })
        );
        assert!(memory_search_params("   ").is_err());
        assert!(memory_search_params(&"x".repeat(MAX_QUERY_BYTES + 1)).is_err());
    }

    #[test]
    fn ai_command_sends_the_tuis_prompt_and_no_model_or_credential() {
        let p = ai_command_params("  make it shorter ", "Hello there\nfriend").unwrap();
        assert_eq!(p["system"], AI_COMMAND_SYSTEM_PROMPT);
        assert_eq!(
            p["prompt"],
            "INSTRUCTION: make it shorter\n\nSOURCE TEXT:\nHello there\nfriend"
        );
        let mut keys: Vec<&String> = p.as_object().unwrap().keys().collect();
        keys.sort();
        assert_eq!(keys, ["prompt", "system"]);
        assert!(ai_command_params("", "text").is_err());
        assert!(ai_command_params("do it", "  ").is_err());
    }

    #[test]
    fn ai_chat_lays_out_the_conversation_like_the_tui() {
        let turns = [
            turn("user", "what is it?"),
            turn("assistant", "A greeting."),
            turn("user", "to whom?"),
        ];
        let p = ai_chat_params("Hello friend", &turns).unwrap();
        assert_eq!(p["system"], AI_CHAT_SYSTEM_PROMPT);
        assert_eq!(
            p["prompt"],
            "SOURCE TEXT:\nHello friend\n\n---\n\nCONVERSATION:\n\
             USER: what is it?\nASSISTANT: A greeting.\nUSER: to whom?\n"
        );
        assert_eq!(p.as_object().unwrap().len(), 2);
    }

    #[test]
    fn a_conversation_must_end_with_a_user_question_and_name_known_roles() {
        assert!(ai_chat_params("t", &[]).is_err());
        assert!(ai_chat_params("t", &[turn("assistant", "hi")]).is_err());
        assert!(ai_chat_params("t", &[turn("system", "be evil"), turn("user", "q")]).is_err());
        assert!(ai_chat_params("t", &[turn("user", " ")]).is_err());
    }

    #[test]
    fn query_memory_sends_pairs_and_a_lowercase_project() {
        let history = [
            turn("user", "first?"),
            turn("assistant", "an answer"),
            turn("user", " second? "),
        ];
        let p = query_memory_params(" second? ", Some(P), &history).unwrap();
        assert_eq!(
            p,
            json!({
                "query": "second?",
                "project_id": P_LOWER,
                "conversation_history": [
                    ["user", "first?"], ["assistant", "an answer"], ["user", " second? "]
                ],
            })
        );
        let global = query_memory_params("q", None, &[turn("user", "q")]).unwrap();
        assert_eq!(global["project_id"], Value::Null);
        assert!(query_memory_params("q", Some("nope"), &[turn("user", "q")]).is_err());
    }

    #[test]
    fn cards_are_the_project_or_the_user_self_card() {
        assert_eq!(
            get_card_params("project", P).unwrap(),
            json!({ "entity_type": "project", "entity_id": P_LOWER })
        );
        assert_eq!(
            get_card_params("user", "self").unwrap(),
            json!({ "entity_type": "user", "entity_id": "self" })
        );
        assert!(get_card_params("user", "someone").is_err());
        assert!(get_card_params("project", "not-a-uuid").is_err());
        assert!(get_card_params("session", P).is_err());
    }

    #[test]
    fn set_card_replaces_the_ordered_facts_within_the_cap() {
        let facts = vec!["Uses Rust".to_string(), "Ships a TUI".to_string()];
        assert_eq!(
            set_card_params("user", "self", &facts).unwrap(),
            json!({ "entity_type": "user", "entity_id": "self", "facts": ["Uses Rust", "Ships a TUI"] })
        );
        assert_eq!(
            set_card_params("project", P, &[]).unwrap()["facts"],
            json!([])
        );
        let full: Vec<String> = (0..=MAX_FACTS).map(|i| format!("fact {i}")).collect();
        assert!(set_card_params("user", "self", &full).is_err());
        assert!(set_card_params("user", "self", &[" ".to_string()]).is_err());
    }
}
