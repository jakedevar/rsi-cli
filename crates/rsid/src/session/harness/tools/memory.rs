//! Memory search tool — queries Flywheel's memory index from within the harness.
//!
//! Wraps the existing `MemoryHandle` to expose memory search as a harness tool.
//! Search results include relevance-scored snippets from past sessions and documents.
//!
//! ## Project scope is enforced, not negotiable
//!
//! Each tool instance captures an `Option<Uuid>` project scope at construction
//! time from the launching session. The tool's JSON schema deliberately does
//! NOT expose `project_id` — agents cannot widen their own scope (plan §D2).
//! When the host session has a project, queries are restricted to that
//! project's indexed memory; when the host session has no project, the tool
//! degrades to global search (matches manual TUI behavior).

use super::HarnessTool;
use crate::memory::worker::MemoryHandle;
use crate::session::harness::types::ToolResult;
use std::path::Path;
use std::sync::Arc;
use uuid::Uuid;

pub struct MemorySearchTool {
    handle: Arc<MemoryHandle>,
    /// Enforced project scope captured at session launch. Not exposed to
    /// the agent through the tool schema — see module doc.
    project_id: Option<Uuid>,
}

impl MemorySearchTool {
    pub fn new(handle: MemoryHandle, project_id: Option<Uuid>) -> Self {
        Self {
            handle: Arc::new(handle),
            project_id,
        }
    }
}

#[async_trait::async_trait]
impl HarnessTool for MemorySearchTool {
    fn name(&self) -> &str {
        "memory_search"
    }

    fn description(&self) -> &str {
        "Search Flywheel's memory index for relevant context from past sessions, \
         documents, and notes. Returns scored text snippets."
    }

    fn parameters_json(&self) -> &str {
        r#"{"type":"object","required":["query"],"properties":{"query":{"type":"string","description":"Search query (natural language or keywords)"},"max_results":{"type":"integer","description":"Maximum results (default 5, max 10)"}}}"#
    }

    async fn execute(&self, args: serde_json::Value, _working_dir: &Path) -> ToolResult {
        let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
        let max = args
            .get("max_results")
            .and_then(|v| v.as_u64())
            .unwrap_or(5)
            .min(10) as usize;

        match self
            .handle
            .search(query, Some(max), None, self.project_id)
            .await
        {
            Ok(results) => {
                let output: String = results
                    .iter()
                    .map(|r| {
                        format!(
                            "Source: {} (lines {}-{}, score: {:.2})\n{}\n",
                            r.path, r.start_line, r.end_line, r.score, r.snippet
                        )
                    })
                    .collect();

                ToolResult {
                    success: true,
                    output: if output.is_empty() {
                        "No results found.".into()
                    } else {
                        output
                    },
                    error_msg: None,
                }
            }
            Err(e) => ToolResult {
                success: false,
                output: String::new(),
                error_msg: Some(format!("Memory search error: {e}")),
            },
        }
    }
}
