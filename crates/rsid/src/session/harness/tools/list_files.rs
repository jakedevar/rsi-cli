use crate::session::harness::tools::{
    HarnessTool, ToolContext, ToolExecutionMode, is_system_blocked, truncation::truncate_text,
};
use crate::session::harness::types::ToolResult;
use regex::Regex;
use std::path::Path;
use walkdir::WalkDir;

/// Tool to list directory contents and optionally filter by pattern.
pub struct ListFilesTool;

impl ListFilesTool {
    async fn execute_limited(
        &self,
        args: serde_json::Value,
        working_dir: &Path,
        max_output_bytes: usize,
    ) -> ToolResult {
        list_files(args, working_dir, max_output_bytes.min(100_000)).await
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
        "List files in a directory, optionally filtering by a pattern. The default is to list the contents of the current working directory recursively."
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
    }
  }
}"#
    }

    async fn execute(&self, args: serde_json::Value, working_dir: &Path) -> ToolResult {
        self.execute_limited(args, working_dir, 100_000).await
    }

    async fn execute_with_context(
        &self,
        args: serde_json::Value,
        context: &ToolContext,
    ) -> ToolResult {
        tokio::select! {
            biased;
            _ = context.cancel.cancelled() => ToolResult {
                success: false,
                output: String::new(),
                error_msg: Some("Tool execution cancelled".into()),
            },
            result = self.execute_limited(args, &context.working_dir, context.policy.max_output_bytes) => result,
        }
    }
}

async fn list_files(
    args: serde_json::Value,
    working_dir: &Path,
    max_output_bytes: usize,
) -> ToolResult {
    let dir_arg = args.get("dir_path").and_then(|v| v.as_str()).unwrap_or("");

    let pattern_arg = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("");

    // Resolve target directory
    let target_dir = if dir_arg.is_empty() || dir_arg == "." {
        working_dir.to_path_buf()
    } else {
        match crate::path_safety::resolve_sandboxed_path(working_dir, dir_arg) {
            Ok(p) => p,
            Err(e) => {
                return ToolResult {
                    success: false,
                    output: String::new(),
                    error_msg: Some(format!("Invalid path: {}", e)),
                };
            }
        }
    };

    // Safety check
    if is_system_blocked(&target_dir) {
        return ToolResult {
            success: false,
            output: String::new(),
            error_msg: Some(format!(
                "Access to {} is restricted by system blocklist.",
                target_dir.display()
            )),
        };
    }

    if !target_dir.exists() {
        return ToolResult {
            success: false,
            output: String::new(),
            error_msg: Some(format!("Directory not found: {}", target_dir.display())),
        };
    }

    if !target_dir.is_dir() {
        return ToolResult {
            success: false,
            output: String::new(),
            error_msg: Some(format!("Path is not a directory: {}", target_dir.display())),
        };
    }

    let mut paths = Vec::new();

    // Compile regex pattern if provided
    let regex = if !pattern_arg.is_empty() {
        match Regex::new(pattern_arg) {
            Ok(r) => Some(r),
            Err(e) => {
                return ToolResult {
                    success: false,
                    output: String::new(),
                    error_msg: Some(format!("Invalid regex pattern '{}': {}", pattern_arg, e)),
                };
            }
        }
    } else {
        None
    };

    for entry in WalkDir::new(&target_dir).into_iter().filter_entry(|e| {
        // simple ignore for .git to prevent noise
        e.file_name() != ".git"
    }) {
        if let Ok(entry) = entry {
            let path = entry.path();
            // Skip the base directory itself
            if path == target_dir {
                continue;
            }

            // Get path relative to target_dir for matching
            let rel_path = match path.strip_prefix(&target_dir) {
                Ok(p) => p.to_string_lossy().into_owned(),
                Err(_) => path.to_string_lossy().into_owned(),
            };

            let match_path = if path.is_dir() {
                format!("{}/", rel_path)
            } else {
                rel_path.clone()
            };

            let is_match = match &regex {
                Some(re) => re.is_match(&match_path) || re.is_match(&rel_path),
                None => true,
            };

            if is_match {
                // For output, prefer the path relative to working_dir to keep it clean
                let display_path = match path.strip_prefix(working_dir) {
                    Ok(p) => p.to_string_lossy().into_owned(),
                    Err(_) => path.to_string_lossy().into_owned(),
                };

                if path.is_dir() {
                    paths.push(format!("{}/", display_path));
                } else {
                    paths.push(display_path);
                }
            }
        }
    }

    paths.sort();

    let output = if paths.is_empty() {
        "(no matching files found)".to_string()
    } else {
        paths.join("\n")
    };

    ToolResult {
        success: true,
        output: truncate_text(&output, max_output_bytes, false).content,
        error_msg: None,
    }
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
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    fn fixture_dir() -> TempDir {
        let test_binary = std::env::current_exe().unwrap();
        tempfile::Builder::new()
            .tempdir_in(test_binary.parent().unwrap())
            .unwrap()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
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
}
