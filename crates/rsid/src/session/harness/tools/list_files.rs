use crate::session::harness::tools::{HarnessTool, is_system_blocked};
use crate::session::harness::types::ToolResult;
use regex::Regex;
use std::path::Path;
use walkdir::WalkDir;

/// Tool to list directory contents and optionally filter by pattern.
pub struct ListFilesTool;

#[async_trait::async_trait]
impl HarnessTool for ListFilesTool {
    fn name(&self) -> &str {
        "list_files"
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

        // Truncate output if it's too large to prevent context window explosion
        let mut output = paths.join("\n");
        let max_len = 100_000;
        if output.len() > max_len {
            output.truncate(max_len);
            output.push_str("\n... [output truncated due to length]");
        }

        if output.is_empty() {
            output = "(no matching files found)".to_string();
        }

        ToolResult {
            success: true,
            output,
            error_msg: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_list_files_basic() {
        let temp = TempDir::new().unwrap();
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
        let temp = TempDir::new().unwrap();
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
