//! Tool trait, invocation context and shared limits for the direct-API harness.
//!
//! The daemon keeps the registry, admission and every tool that needs process
//! custody, the store, agent controls or memory; the pure file and navigation
//! tools live below it (#2616). `StreamEvent` is the only daemon value the
//! context carries, so it is defined here and the daemon re-exports it.

pub mod apply_patch;
pub mod file;
pub mod glob;
pub mod grep;
pub mod list_files;
pub mod navigation;
pub mod policy;
pub mod read_output;
pub mod truncation;
pub mod view_image;

use crate::stream::StreamEvent;
use crate::types::{HarnessToolSpec, HarnessToolSpecKind, ToolResult};
use serde_json::json;
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Whether a tool may execute alongside other calls from the same model turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolExecutionMode {
    ParallelSafe,
    Sequential,
}

/// Shared limits passed to tools. Later session policy can narrow these limits.
#[derive(Debug, Clone)]
pub struct ToolPolicy {
    pub max_output_bytes: usize,
    /// Network egress policy (#774). A network-capable tool reads it from its
    /// `ToolContext` and never from an ambient global; the default is the
    /// fail-closed `deny_private` mode.
    pub egress: rsi_common::egress_policy::EgressPolicy,
    /// Where the file tools may reach (#3172). Contained to the working
    /// directory unless the session policy opts into the task environment.
    pub file_paths: rsi_common::harness_tool_policy::FilePathScope,
    /// Extra environment names a host `shell` / `exec_command` launch passes
    /// through (#3169); credential-looking names are dropped at launch too.
    pub env_passthrough: std::sync::Arc<[String]>,
}

impl Default for ToolPolicy {
    fn default() -> Self {
        Self {
            max_output_bytes: 1024 * 1024,
            egress: rsi_common::egress_policy::EgressPolicy::default(),
            file_paths: rsi_common::harness_tool_policy::FilePathScope::default(),
            env_passthrough: std::sync::Arc::from([]),
        }
    }
}

/// Construction-bound context for one tool invocation.
#[derive(Clone)]
pub struct ToolContext {
    pub session_id: Option<uuid::Uuid>,
    pub working_dir: PathBuf,
    pub cancel: CancellationToken,
    pub event_sink: Option<mpsc::Sender<StreamEvent>>,
    pub policy: ToolPolicy,
}

impl ToolContext {
    pub async fn report_progress(&self, tool_name: &str, message: &str) {
        if let Some(sink) = &self.event_sink {
            let _ = sink
                .send(StreamEvent {
                    event_type: "tool_progress".into(),
                    data: json!({
                        "session_id": self.session_id,
                        "name": tool_name,
                        "message": message,
                    }),
                })
                .await;
        }
    }
}

/// Trait for tools executable by the agent harness.
#[async_trait::async_trait]
pub trait HarnessTool: Send + Sync {
    /// Tool name (must be unique within a registry).
    fn name(&self) -> &str;

    /// Human-readable description for the LLM.
    fn description(&self) -> &str;

    /// JSON Schema string for input parameters.
    fn parameters_json(&self) -> &str;

    /// Calls are sequential unless the tool explicitly declares itself safe.
    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Sequential
    }

    /// Execute the tool with the given arguments.
    async fn execute(&self, args: serde_json::Value, working_dir: &Path) -> ToolResult;

    /// Execute while honoring harness cancellation. Process-backed tools
    /// override this so cancellation waits for child cleanup and reap; other
    /// tools retain their existing future-drop behavior.
    async fn execute_cancellable(
        &self,
        args: serde_json::Value,
        working_dir: &Path,
        cancel: &CancellationToken,
    ) -> ToolResult {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => ToolResult {
                success: false,
                output: String::new(),
                error_msg: Some("Tool execution cancelled".to_string()),
            },
            result = self.execute(args, working_dir) => result,
        }
    }

    /// New tools receive progress, cancellation, policy and session context.
    /// Existing tools keep their cancellable implementation during migration.
    async fn execute_with_context(
        &self,
        args: serde_json::Value,
        context: &ToolContext,
    ) -> ToolResult {
        self.execute_cancellable(args, &context.working_dir, &context.cancel)
            .await
    }

    /// Convert to a HarnessToolSpec for provider API calls.
    fn to_spec(&self) -> HarnessToolSpec {
        HarnessToolSpec {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters_json: self.parameters_json().to_string(),
            freeform: None,
            kind: HarnessToolSpecKind::Function,
        }
    }
}

/// Most tool names listed in an unknown-tool error.
pub const MAX_LISTED_TOOL_NAMES: usize = 20;

/// Most "Did you mean" names in an unknown-tool error.
const MAX_TOOL_SUGGESTIONS: usize = 3;

/// Largest edit distance at which a callable name is suggested.
const MAX_SUGGESTION_DISTANCE: usize = 3;

/// Callable names a model most plausibly meant by `requested`, best first:
/// names that contain it or that it contains (`read` is not 3 edits from
/// `read_file`, but it plainly meant it), then names within
/// [`MAX_SUGGESTION_DISTANCE`] edits, nearest first. A short name allows only
/// half its length in edits, so `read` does not suggest `grep`. Case, `-`
/// versus `_` and a leading namespace such as `functions.` are ignored.
pub fn suggest_tool_names(requested: &str, callable: &[String]) -> Vec<String> {
    let requested = comparable_tool_name(requested);
    let length = requested.chars().count();
    if length == 0 {
        return Vec::new();
    }
    let allowed_distance = (length / 2).clamp(1, MAX_SUGGESTION_DISTANCE);
    let mut ranked: Vec<(bool, usize, usize, &String)> = callable
        .iter()
        .enumerate()
        .filter_map(|(order, candidate)| {
            let comparable = comparable_tool_name(candidate);
            let distance = edit_distance(&requested, &comparable);
            let related = length >= 3
                && (comparable.contains(&requested)
                    || (comparable.chars().count() >= 4 && requested.contains(&comparable)));
            (related || distance <= allowed_distance)
                .then_some((!related, distance, order, candidate))
        })
        .collect();
    ranked.sort_by_key(|(unrelated, distance, order, _)| (*unrelated, *distance, *order));
    ranked
        .into_iter()
        .take(MAX_TOOL_SUGGESTIONS)
        .map(|(_, _, _, name)| name.clone())
        .collect()
}

fn comparable_tool_name(name: &str) -> String {
    let local = name.trim().rsplit(['.', '/']).next().unwrap_or_default();
    local.to_ascii_lowercase().replace('-', "_")
}

/// Levenshtein distance over characters.
fn edit_distance(left: &str, right: &str) -> usize {
    let right: Vec<char> = right.chars().collect();
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    for (row, left_char) in left.chars().enumerate() {
        let mut current = vec![row + 1];
        for (column, right_char) in right.iter().enumerate() {
            let substitution = previous[column] + usize::from(left_char != *right_char);
            current.push(
                substitution
                    .min(previous[column + 1] + 1)
                    .min(current[column] + 1),
            );
        }
        previous = current;
    }
    previous[right.len()]
}

/// System blocklist -- these paths are never writable even with wildcard allowed_paths.
const SYSTEM_BLOCKLIST: &[&str] = &[
    "/bin",
    "/sbin",
    "/usr/bin",
    "/usr/sbin",
    "/usr/lib",
    "/etc",
    "/dev",
    "/proc",
    "/sys",
    "/boot",
];

/// Check if a resolved path falls within the system blocklist.
pub fn is_system_blocked(path: &Path) -> bool {
    let path_str = path.to_string_lossy();
    SYSTEM_BLOCKLIST
        .iter()
        .any(|blocked| path_str.starts_with(blocked))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_system_blocked() {
        assert!(is_system_blocked(Path::new("/etc/passwd")));
        assert!(is_system_blocked(Path::new("/bin/bash")));
        assert!(is_system_blocked(Path::new("/usr/bin/ls")));
        assert!(is_system_blocked(Path::new("/proc/self/maps")));
        assert!(!is_system_blocked(Path::new("/home/user/project/file.rs")));
        assert!(!is_system_blocked(Path::new("/tmp/workdir/file.txt")));
    }

    #[test]
    fn edit_distance_counts_character_edits() {
        assert_eq!(edit_distance("read_file", "read_file"), 0);
        assert_eq!(edit_distance("read_fil", "read_file"), 1);
        assert_eq!(edit_distance("grep", "glob"), 3);
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("kitten", "sitting"), 3);
    }

    #[test]
    fn suggestions_rank_containment_before_edit_distance() {
        let callable: Vec<String> = ["grep", "read_file", "write_file"]
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(suggest_tool_names("read", &callable), ["read_file"]);
        assert_eq!(suggest_tool_names("functions.Grep", &callable), ["grep"]);
        assert_eq!(suggest_tool_names("gerp", &callable), ["grep"]);
        assert!(suggest_tool_names("zzzzzz", &callable).is_empty());
        assert!(suggest_tool_names("  ", &callable).is_empty());
    }

    struct Echo;

    #[async_trait::async_trait]
    impl HarnessTool for Echo {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "echo"
        }
        fn parameters_json(&self) -> &str {
            "{}"
        }
        async fn execute(&self, args: serde_json::Value, _: &Path) -> ToolResult {
            ToolResult {
                success: true,
                output: args.to_string(),
                error_msg: None,
            }
        }
    }

    #[tokio::test]
    async fn a_default_tool_is_sequential_and_a_cancelled_call_reports_it() {
        let tool = Echo;
        assert_eq!(tool.execution_mode(), ToolExecutionMode::Sequential);
        assert_eq!(tool.to_spec().name, "echo");
        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = tool
            .execute_cancellable(json!({}), Path::new("."), &cancel)
            .await;
        assert!(!result.success);
        assert_eq!(
            result.error_msg.as_deref(),
            Some("Tool execution cancelled")
        );
    }

    #[tokio::test]
    async fn progress_reaches_the_event_sink_as_a_tool_progress_event() {
        let (sender, mut receiver) = mpsc::channel(1);
        let context = ToolContext {
            session_id: None,
            working_dir: PathBuf::from("."),
            cancel: CancellationToken::new(),
            event_sink: Some(sender),
            policy: ToolPolicy::default(),
        };
        context.report_progress("echo", "halfway").await;
        let event = receiver.recv().await.expect("progress event");
        assert_eq!(event.event_type, "tool_progress");
        assert_eq!(event.data["name"], "echo");
        assert_eq!(event.data["message"], "halfway");
    }
}
