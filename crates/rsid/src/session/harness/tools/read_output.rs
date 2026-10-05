//! `read_output`: exact detail from a spilled tool output (#1097).
//!
//! Large tool results are stored under a handle and the transcript carries a
//! compact stub. This tool reads the stored text back on demand (a line range
//! and/or a grep), so no information is lost and no earlier message is ever
//! rewritten.

use super::{HarnessTool, ToolContext, ToolExecutionMode, truncation::truncate_text};
use crate::session::harness::types::ToolResult;
use rsi_common::spill::{self, SpillConfig};
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;

/// Most bytes one `read_output` call returns; ask for a narrower range or a
/// grep for more.
const MAX_READ_OUTPUT_BYTES: usize = 24 * 1024;

pub struct ReadOutputTool {
    config: Arc<SpillConfig>,
}

impl ReadOutputTool {
    pub fn new(config: Arc<SpillConfig>) -> Self {
        Self { config }
    }

    fn read(&self, args: &Value, limit: usize) -> ToolResult {
        let fail = |message: String| ToolResult {
            success: false,
            output: String::new(),
            error_msg: Some(message),
        };
        let Some(handle) = args.get("handle").and_then(Value::as_str) else {
            return fail(
                "read_output requires a string 'handle' (from a [rsi-spill ...] stub)".into(),
            );
        };
        let grep = args.get("grep").and_then(Value::as_str);
        let range = match args
            .get("range")
            .and_then(Value::as_str)
            .map(spill::parse_range)
        {
            Some(Err(message)) => return fail(message),
            Some(Ok(range)) => Some(range),
            None => None,
        };
        match spill::show(&self.config.root, handle, grep, range) {
            Err(message) => fail(message),
            Ok(text) if text.is_empty() => ToolResult {
                success: true,
                output: "(no lines matched)".into(),
                error_msg: None,
            },
            Ok(text) => ToolResult {
                success: true,
                output: bound(&text, limit),
                error_msg: None,
            },
        }
    }
}

/// Keep whole lines within `limit`; say how to continue when cut.
fn bound(text: &str, limit: usize) -> String {
    let bounded = truncate_text(text, limit, false);
    if !bounded.truncated {
        return bounded.content;
    }
    let shown = bounded.content[..bounded.retained_bytes].lines().count();
    let total = text.lines().count();
    format!(
        "{}[read_output: showing {shown} of {total} lines; narrow with range or grep]",
        &bounded.content[..bounded.retained_bytes]
    )
}

#[async_trait::async_trait]
impl HarnessTool for ReadOutputTool {
    fn name(&self) -> &str {
        "read_output"
    }

    fn description(&self) -> &str {
        "Read the full text of a spilled tool output. A large tool result appears in the \
transcript as a stub starting `[rsi-spill <handle>]`; pass that handle here to fetch exact \
detail. Optional `grep` (regex) returns matching lines prefixed `N:`; optional `range` \
(`a:b`, `a:`, `:b` or `a`, 1-based inclusive) returns those lines."
    }

    fn parameters_json(&self) -> &str {
        r#"{"type":"object","properties":{"handle":{"type":"string","description":"The handle from the [rsi-spill <handle>] stub, e.g. 1a2b3c4d/7"},"grep":{"type":"string","description":"Regex; return only matching lines, prefixed with their line number"},"range":{"type":"string","description":"Line range a:b (1-based, inclusive)"}},"required":["handle"]}"#
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::ParallelSafe
    }

    async fn execute(&self, args: Value, _working_dir: &Path) -> ToolResult {
        self.read(&args, MAX_READ_OUTPUT_BYTES)
    }

    async fn execute_with_context(&self, args: Value, context: &ToolContext) -> ToolResult {
        self.read(
            &args,
            MAX_READ_OUTPUT_BYTES.min(context.policy.max_output_bytes),
        )
    }
}
