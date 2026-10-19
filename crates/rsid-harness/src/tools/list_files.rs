//! `list_files`: recursive directory listing for the Harness loop.
//!
//! Walks with the shared gitignore-aware walker (`navigation.rs`, #2254): ignored
//! paths such as `target/` and the `.git` directory are not listed, the walk
//! can be limited with `max_depth`, and it stops as soon as the output cap is
//! reached instead of collecting the whole tree first.

use crate::tools::navigation::{
    Budget, SearchScope, StopReason, failure, optional_uint, run_blocking, walker,
};
use crate::tools::{HarnessTool, ToolContext, ToolExecutionMode};
use crate::types::ToolResult;
use regex::Regex;
use std::path::Path;
use tokio_util::sync::CancellationToken;

/// Hard cap on the rendered listing, whatever the session's tool policy allows.
const MAX_OUTPUT_BYTES: usize = 100_000;
/// Room kept for the truncation note so the whole result stays inside the cap.
const NOTE_RESERVE_BYTES: usize = 256;

/// Tool to list directory contents and optionally filter by pattern.
pub struct ListFilesTool;

impl ListFilesTool {
    async fn run(
        &self,
        args: serde_json::Value,
        working_dir: &Path,
        max_output_bytes: usize,
        cancel: &CancellationToken,
    ) -> ToolResult {
        let request = match ListRequest::parse(&args, working_dir, max_output_bytes) {
            Ok(request) => request,
            Err(refusal) => return refusal,
        };
        match run_blocking(cancel, move |budget| list(&request, budget)).await {
            Ok(output) => ToolResult {
                success: true,
                output,
                error_msg: None,
            },
            Err(failed) => failed,
        }
    }
}

#[async_trait::async_trait]
impl HarnessTool for ListFilesTool {
    fn name(&self) -> &str {
        "list_files"
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::ParallelSafe
    }

    fn description(&self) -> &str {
        "List files and directories under a directory, optionally filtering by a pattern. The \
         default is to list the working directory recursively. Honors .gitignore (ignored paths \
         such as target/ are not listed) and skips .git. Output stops at a size cap and says so; \
         use max_depth, dir_path or pattern to narrow it. To find files by name use glob; to \
         search file contents use grep."
    }

    fn parameters_json(&self) -> &str {
        r#"{
  "type": "object",
  "properties": {
    "dir_path": {
      "type": "string",
      "description": "Optional directory path to list. If omitted or empty, lists the current working directory."
    },
    "pattern": {
      "type": "string",
      "description": "Optional regex pattern to filter results (e.g. '\\.rs$', 'src/'). If omitted, defaults to listing all files."
    },
    "max_depth": {
      "type": "integer",
      "minimum": 1,
      "description": "Optional maximum depth below dir_path: 1 lists only its direct children. Default: unlimited."
    }
  }
}"#
    }

    async fn execute(&self, args: serde_json::Value, working_dir: &Path) -> ToolResult {
        self.run(
            args,
            working_dir,
            MAX_OUTPUT_BYTES,
            &CancellationToken::new(),
        )
        .await
    }

    async fn execute_with_context(
        &self,
        args: serde_json::Value,
        context: &ToolContext,
    ) -> ToolResult {
        self.run(
            args,
            &context.working_dir,
            context.policy.max_output_bytes,
            &context.cancel,
        )
        .await
    }
}

struct ListRequest {
    scope: SearchScope,
    regex: Option<Regex>,
    max_depth: Option<usize>,
    max_output_bytes: usize,
}

impl ListRequest {
    fn parse(
        args: &serde_json::Value,
        working_dir: &Path,
        max_output_bytes: usize,
    ) -> Result<Self, ToolResult> {
        let dir_arg = args.get("dir_path").and_then(|v| v.as_str()).unwrap_or("");
        let pattern_arg = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
        let max_depth = match optional_uint(args, "max_depth").map_err(failure)? {
            Some(0) => return Err(failure("invalid_arguments: max_depth must be at least 1")),
            depth => depth,
        };
        let scope = SearchScope::resolve(working_dir, dir_arg)?;
        if !scope.root.is_dir() {
            return Err(failure(format!(
                "Path is not a directory: {}",
                scope.root.display()
            )));
        }
        let regex = if pattern_arg.is_empty() {
            None
        } else {
            Some(Regex::new(pattern_arg).map_err(|error| {
                failure(format!("Invalid regex pattern '{pattern_arg}': {error}"))
            })?)
        };
        Ok(Self {
            scope,
            regex,
            max_depth,
            max_output_bytes: max_output_bytes.min(MAX_OUTPUT_BYTES),
        })
    }
}

fn list(request: &ListRequest, budget: &Budget) -> String {
    let scope = &request.scope;
    let line_budget = request.max_output_bytes.saturating_sub(NOTE_RESERVE_BYTES);
    let mut builder = walker(&scope.root);
    builder.max_depth(request.max_depth);
    let mut output = String::new();
    let mut listed = 0usize;
    let mut truncated = false;
    let mut stop = None;
    for entry in builder.build() {
        if let Some(reason) = budget.stop_reason() {
            stop = Some(reason);
            break;
        }
        let Ok(entry) = entry else { continue };
        if entry.depth() == 0 {
            continue;
        }
        let path = entry.path();
        let is_dir = entry.file_type().is_some_and(|kind| kind.is_dir());
        if let Some(regex) = &request.regex {
            let relative = path
                .strip_prefix(&scope.root)
                .unwrap_or(path)
                .to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, "/");
            let as_dir = format!("{relative}/");
            let matched = regex.is_match(&relative) || (is_dir && regex.is_match(&as_dir));
            if !matched {
                continue;
            }
        }
        let mut shown = scope.display(path);
        if is_dir {
            shown.push('/');
        }
        if output.len() + shown.len() + 1 > line_budget {
            truncated = true;
            break;
        }
        output.push_str(&shown);
        output.push('\n');
        listed += 1;
    }
    if listed == 0 && !truncated {
        output.push_str("(no matching files found)\n");
    }
    if truncated {
        output.push_str(&format!(
            "[truncated: output limit of {} bytes reached after {listed} entries; list a \
             subdirectory, set max_depth or add a pattern]\n",
            request.max_output_bytes
        ));
    }
    if stop == Some(StopReason::TimedOut) {
        output.push_str(&format!(
            "[stopped: listing time limit reached after {listed} entries; narrow dir_path or set \
             max_depth]\n"
        ));
    }
    output.truncate(output.trim_end_matches('\n').len());
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// A fixture root outside the harness system blocklist. Landing gates put
    /// TMPDIR on /dev/shm, which the blocklist correctly refuses, so a
    /// TMPDIR-relative fixture made both tests standing base reds on every
    /// lander. The test binary's directory is on disk in every runner.
    fn fixture_dir() -> TempDir {
        let test_binary = std::env::current_exe().unwrap();
        tempfile::Builder::new()
            .tempdir_in(test_binary.parent().unwrap())
            .unwrap()
    }

    #[tokio::test]
    async fn test_list_files_basic() {
        let temp = fixture_dir();
        let wd = temp.path();
        fs::write(wd.join("test.txt"), "hello").unwrap();
        fs::write(wd.join("lib.rs"), "fn main() {}").unwrap();
        fs::create_dir(wd.join("src")).unwrap();
        fs::write(wd.join("src/main.rs"), "fn main() {}").unwrap();

        let tool = ListFilesTool;
        let result = tool.execute(serde_json::json!({}), wd).await;

        assert!(result.success);
        assert!(result.output.contains("test.txt"));
        assert!(result.output.contains("lib.rs"));
        assert!(result.output.contains("src/"));
        assert!(result.output.contains("src/main.rs"));
    }

    #[tokio::test]
    async fn test_list_files_pattern() {
        let temp = fixture_dir();
        let wd = temp.path();
        fs::write(wd.join("test.txt"), "hello").unwrap();
        fs::write(wd.join("lib.rs"), "fn main() {}").unwrap();
        fs::create_dir(wd.join("src")).unwrap();
        fs::write(wd.join("src/main.rs"), "fn main() {}").unwrap();

        let tool = ListFilesTool;
        let result = tool
            .execute(serde_json::json!({ "pattern": "\\.rs$" }), wd)
            .await;

        assert!(result.success);
        assert!(!result.output.contains("test.txt"));
        assert!(result.output.contains("lib.rs"));
        assert!(result.output.contains("src/main.rs"));
    }

    #[tokio::test]
    async fn ignored_and_git_paths_are_not_listed() {
        let repo = crate::tools::navigation::fixture_repo();
        let result = ListFilesTool
            .execute(serde_json::json!({}), repo.path())
            .await;
        assert!(result.success, "{:?}", result.error_msg);
        let listed: Vec<&str> = result.output.lines().collect();
        assert_eq!(
            listed,
            [
                ".github/",
                ".github/workflows/",
                ".github/workflows/ci.yml",
                ".gitignore",
                "README.md",
                "bin.dat",
                "src/",
                "src/lib.rs",
                "src/main.rs",
            ]
        );
    }

    #[tokio::test]
    async fn max_depth_limits_the_listing_and_pattern_still_filters() {
        let repo = crate::tools::navigation::fixture_repo();
        let shallow = ListFilesTool
            .execute(serde_json::json!({"max_depth": 1}), repo.path())
            .await;
        assert!(shallow.success, "{:?}", shallow.error_msg);
        assert_eq!(
            shallow.output.lines().collect::<Vec<_>>(),
            [".github/", ".gitignore", "README.md", "bin.dat", "src/"]
        );

        let filtered = ListFilesTool
            .execute(
                serde_json::json!({"dir_path": "src", "pattern": "main"}),
                repo.path(),
            )
            .await;
        assert_eq!(filtered.output, "src/main.rs");

        let zero = ListFilesTool
            .execute(serde_json::json!({"max_depth": 0}), repo.path())
            .await;
        assert!(!zero.success);
        assert!(zero.error_msg.unwrap().starts_with("invalid_arguments"));
    }

    #[tokio::test]
    async fn listing_stops_at_the_byte_cap_with_a_note() {
        let repo = crate::tools::navigation::fixture_repo();
        for n in 0..300 {
            fs::write(repo.path().join(format!("file_{n:03}.txt")), "x").unwrap();
        }
        let context = ToolContext {
            session_id: None,
            working_dir: repo.path().to_path_buf(),
            cancel: CancellationToken::new(),
            event_sink: None,
            policy: crate::tools::ToolPolicy {
                max_output_bytes: 600,
                ..crate::tools::ToolPolicy::default()
            },
        };
        let result = ListFilesTool
            .execute_with_context(serde_json::json!({"pattern": "file_"}), &context)
            .await;
        assert!(result.success, "{:?}", result.error_msg);
        assert!(result.output.len() <= 600, "{}", result.output.len());
        let entries = result
            .output
            .lines()
            .filter(|line| line.starts_with("file_"))
            .count();
        assert!(entries > 0 && entries < 300, "{entries}");
        assert_eq!(result.output.lines().next(), Some("file_000.txt"));
        assert!(
            result
                .output
                .contains("[truncated: output limit of 600 bytes"),
            "{}",
            result.output
        );
    }

    #[tokio::test]
    async fn escaping_and_missing_directories_are_refused() {
        let repo = crate::tools::navigation::fixture_repo();
        for dir in ["../..", "/etc", "nope", "README.md"] {
            let result = ListFilesTool
                .execute(serde_json::json!({"dir_path": dir}), repo.path())
                .await;
            assert!(!result.success, "{dir}");
        }
    }
}
