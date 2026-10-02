//! Deferred MCP tool discovery for large per-session catalogs.

use crate::session::harness::tools::{DeferredMcpCatalog, HarnessTool, ToolExecutionMode};
use crate::session::harness::types::ToolResult;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, RwLock};

const MAX_RESULTS: usize = 32;
const MAX_RESULTS_PER_SERVER: usize = 8;
const MAX_QUERY_BYTES: usize = 256;
const MAX_DESCRIPTION_BYTES: usize = 2048;
const MAX_SCHEMA_BYTES: usize = 4096;

struct SearchEntry {
    server_id: String,
    name: String,
    description: String,
    schema_text: String,
    schema: Value,
}

pub(crate) struct McpToolSearchTool {
    entries: Vec<SearchEntry>,
    deferred: Arc<RwLock<DeferredMcpCatalog>>,
}

impl McpToolSearchTool {
    pub(crate) fn new(
        entries: Vec<(String, String, String, Value)>,
        deferred: Arc<RwLock<DeferredMcpCatalog>>,
    ) -> Self {
        Self {
            entries: entries
                .into_iter()
                .map(|(server_id, name, description, schema)| {
                    let serialized = serde_json::to_string(&schema).unwrap_or_else(|_| "{}".into());
                    let (schema_text, schema) = if serialized.len() <= MAX_SCHEMA_BYTES {
                        (serialized, schema)
                    } else {
                        (
                            "schema omitted because it exceeds the search display bound".into(),
                            json!({"schema": "omitted", "reason": "too_large"}),
                        )
                    };
                    SearchEntry {
                        server_id,
                        name,
                        description: bounded_text(description, MAX_DESCRIPTION_BYTES),
                        schema_text,
                        schema,
                    }
                })
                .collect(),
            deferred,
        }
    }

    fn search(&self, query: &str) -> ToolResult {
        let terms = tokenize(query);
        if terms.is_empty() {
            return invalid_query();
        }
        let normalized_query = query.to_ascii_lowercase();
        let mut matches: Vec<_> = self
            .entries
            .iter()
            .map(|entry| {
                let mut score = 0;
                for term in &terms {
                    score += field_score(&entry.server_id, term) * 2;
                    score += field_score(&entry.name, term) * 5;
                    score += field_score(&entry.description, term) * 2;
                    score += field_score(&entry.schema_text, term);
                }
                if entry.name.to_ascii_lowercase().contains(&normalized_query) {
                    score += 20;
                }
                (score, &entry.name, entry)
            })
            .filter(|(score, _, _)| *score > 0)
            .collect();
        let total_matches = matches.len();
        matches.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(right.1)));
        let mut per_server_counts = HashMap::new();
        let mut selected = Vec::new();
        for (_, _, entry) in matches {
            let count = per_server_counts
                .entry(entry.server_id.as_str())
                .or_insert(0);
            if *count >= MAX_RESULTS_PER_SERVER {
                continue;
            }
            *count += 1;
            selected.push(json!({
                "name": entry.name,
                "server_id": entry.server_id,
                "description": entry.description,
                "input_schema": entry.schema,
            }));
            if selected.len() == MAX_RESULTS {
                break;
            }
        }
        let omitted = total_matches - selected.len();
        self.deferred
            .write()
            .expect("deferred MCP state lock")
            .reveal(selected.iter().filter_map(|result| {
                result
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            }));
        ToolResult {
            success: true,
            output: json!({"results": selected, "omitted": omitted}).to_string(),
            error_msg: None,
        }
    }
}

#[async_trait::async_trait]
impl HarnessTool for McpToolSearchTool {
    fn name(&self) -> &str {
        "tool_search"
    }

    fn description(&self) -> &str {
        "Search deferred MCP tools. Matching exact tool names are returned and become callable for this session; up to 32 tools, with at most 8 from one server, are revealed."
    }

    fn parameters_json(&self) -> &str {
        r#"{"type":"object","properties":{"query":{"type":"string","minLength":1,"maxLength":256}},"required":["query"],"additionalProperties":false}"#
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Sequential
    }

    async fn execute(&self, args: Value, _working_dir: &Path) -> ToolResult {
        let Some(query) = args.get("query").and_then(Value::as_str) else {
            return invalid_query();
        };
        if query.is_empty() || query.len() > MAX_QUERY_BYTES {
            return invalid_query();
        }
        self.search(query)
    }
}

fn invalid_query() -> ToolResult {
    ToolResult {
        success: false,
        output: String::new(),
        error_msg: Some("mcp_search_invalid_query: query must be 1-256 bytes".to_string()),
    }
}

fn field_score(text: &str, term: &str) -> usize {
    usize::from(text.to_ascii_lowercase().contains(term))
}

fn tokenize(text: &str) -> Vec<String> {
    text.chars()
        .map(|char| {
            if char.is_ascii_alphanumeric() {
                char.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

fn bounded_text(text: String, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut bounded = text[..end].to_string();
    bounded.push_str("...");
    bounded
}
