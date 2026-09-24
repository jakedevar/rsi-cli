//! Memory search tool — queries Flywheel's memory index from within the harness.
//!
//! Wraps the existing `MemoryHandle` to expose memory search as a harness tool.
//! Search results include relevance-scored snippets from past sessions and documents.
//!
//! ## Project scope
//!
//! See `crate::session::harness::tools::memory` for the canonical version.
//! This top-level path is the legacy continuation/rotation harness; it is
//! currently invoked with `memory_handle = None` so the tool is never
//! registered. The struct still carries `project_id` for type parity with
//! the active path.

use super::HarnessTool;
use crate::harness::types::ToolResult;
use crate::memory::worker::MemoryHandle;
use std::path::Path;
use std::sync::Arc;
use uuid::Uuid;

pub struct MemorySearchTool {
    handle: Arc<MemoryHandle>,
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
