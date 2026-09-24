//! File I/O tools: read_file, write_file, file_edit.
//!
//! All paths resolved through path_safety::resolve_sandboxed_path
//! and checked against the system blocklist.

use super::{HarnessTool, is_system_blocked};
use crate::harness::types::ToolResult;
use crate::path_safety::resolve_sandboxed_path;
use std::path::Path;

const MAX_READ_BYTES: usize = 50 * 1024; // 50 KB

pub struct ReadFileTool;

#[async_trait::async_trait]
impl HarnessTool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read the contents of a file at the given path (relative to working directory). \
         Returns the file content or an error message."
    }

    fn parameters_json(&self) -> &str {
        r#"{"type":"object","required":["path"],"properties":{"path":{"type":"string","description":"File path relative to working directory"}}}"#
    }

    async fn execute(&self, args: serde_json::Value, working_dir: &Path) -> ToolResult {
        let path_str = args
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or_default();

        let resolved = match resolve_sandboxed_path(working_dir, path_str) {
            Ok(p) => p,
            Err(e) => {
                return ToolResult {
                    success: false,
                    output: String::new(),
                    error_msg: Some(format!("Path error: {e}")),
                };
            }
        };

        if is_system_blocked(&resolved) {
            return ToolResult {
                success: false,
                output: String::new(),
                error_msg: Some("Access to system path blocked".into()),
            };
        }

        match tokio::fs::read_to_string(&resolved).await {
            Ok(content) => {
                let output = if content.len() > MAX_READ_BYTES {
                    format!(
                        "{}...\n\n[truncated at {} bytes, file is {} bytes total]",
                        &content[..MAX_READ_BYTES],
                        MAX_READ_BYTES,
                        content.len()
                    )
                } else {
                    content
                };
                ToolResult {
                    success: true,
                    output,
                    error_msg: None,
                }
            }
            Err(e) => ToolResult {
                success: false,
                output: String::new(),
                error_msg: Some(format!("Error reading file: {e}")),
            },
        }
    }
}

pub struct WriteFileTool;

#[async_trait::async_trait]
impl HarnessTool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        "Write content to a file at the given path. Creates parent directories if needed. \
         Overwrites existing content."
    }

    fn parameters_json(&self) -> &str {
        r#"{"type":"object","required":["path","content"],"properties":{"path":{"type":"string","description":"File path relative to working directory"},"content":{"type":"string","description":"Content to write"}}}"#
    }

    async fn execute(&self, args: serde_json::Value, working_dir: &Path) -> ToolResult {
        let path_str = args
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or_default();

        let resolved = match resolve_sandboxed_path(working_dir, path_str) {
            Ok(p) => p,
            Err(e) => {
                return ToolResult {
                    success: false,
                    output: String::new(),
                    error_msg: Some(format!("Path error: {e}")),
                };
            }
        };

        if is_system_blocked(&resolved) {
            return ToolResult {
                success: false,
                output: String::new(),
                error_msg: Some("Write to system path blocked".into()),
            };
        }

        // Create parent dirs
        if let Some(parent) = resolved.parent()
            && let Err(e) = tokio::fs::create_dir_all(parent).await
        {
            return ToolResult {
                success: false,
                output: String::new(),
                error_msg: Some(format!("Cannot create directory: {e}")),
            };
        }

        match tokio::fs::write(&resolved, content).await {
            Ok(()) => ToolResult {
                success: true,
                output: format!("Wrote {} bytes to {}", content.len(), path_str),
                error_msg: None,
            },
            Err(e) => ToolResult {
                success: false,
                output: String::new(),
                error_msg: Some(format!("Error writing file: {e}")),
            },
        }
    }
}

pub struct EditFileTool;

#[async_trait::async_trait]
impl HarnessTool for EditFileTool {
    fn name(&self) -> &str {
        "file_edit"
    }

    fn description(&self) -> &str {
        "Edit a file by replacing an exact string match with new content. \
         The old_string must appear exactly once in the file."
    }

    fn parameters_json(&self) -> &str {
        r#"{"type":"object","required":["path","old_string","new_string"],"properties":{"path":{"type":"string","description":"File path relative to working directory"},"old_string":{"type":"string","description":"Exact text to find (must be unique in the file)"},"new_string":{"type":"string","description":"Text to replace it with"}}}"#
    }

    async fn execute(&self, args: serde_json::Value, working_dir: &Path) -> ToolResult {
        let path_str = args
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let old = args
            .get("old_string")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let new = args
            .get("new_string")
            .and_then(|v| v.as_str())
            .unwrap_or_default();

        let resolved = match resolve_sandboxed_path(working_dir, path_str) {
            Ok(p) => p,
            Err(e) => {
                return ToolResult {
                    success: false,
                    output: String::new(),
                    error_msg: Some(format!("Path error: {e}")),
                };
            }
        };

        if is_system_blocked(&resolved) {
            return ToolResult {
                success: false,
                output: String::new(),
                error_msg: Some("Edit of system path blocked".into()),
            };
        }

        let content = match tokio::fs::read_to_string(&resolved).await {
            Ok(c) => c,
            Err(e) => {
                return ToolResult {
                    success: false,
                    output: String::new(),
                    error_msg: Some(format!("Error reading file: {e}")),
                };
            }
        };

        let count = content.matches(old).count();
        if count == 0 {
            return ToolResult {
                success: false,
                output: String::new(),
                error_msg: Some("old_string not found in file".into()),
            };
        }
        if count > 1 {
            return ToolResult {
                success: false,
                output: String::new(),
                error_msg: Some(format!("old_string found {count} times; must be unique")),
            };
        }

        let new_content = content.replacen(old, new, 1);
        match tokio::fs::write(&resolved, &new_content).await {
            Ok(()) => ToolResult {
                success: true,
                output: format!("Edited {path_str}"),
                error_msg: None,
            },
            Err(e) => ToolResult {
                success: false,
                output: String::new(),
                error_msg: Some(format!("Error writing file: {e}")),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp_dir() -> PathBuf {
        std::env::temp_dir()
    }

    #[tokio::test]
    async fn test_read_file_missing() {
        let tool = ReadFileTool;
        let result = tool
            .execute(
                serde_json::json!({"path": "nonexistent_rsi_test_file.txt"}),
                &tmp_dir(),
            )
            .await;
        assert!(!result.success);
        assert!(result.error_msg.is_some());
    }

    #[tokio::test]
    async fn test_read_file_traversal_rejected() {
        let tool = ReadFileTool;
        let result = tool
            .execute(serde_json::json!({"path": "../../etc/passwd"}), &tmp_dir())
            .await;
        assert!(!result.success);
    }

    #[tokio::test]
    async fn test_write_and_read_roundtrip() {
        let wd = std::env::temp_dir();
        let filename = "flywheel_harness_tool_test_roundtrip.txt";
        let content = "hello from write_file tool\n";

        let write_tool = WriteFileTool;
        let write_result = write_tool
            .execute(
                serde_json::json!({"path": filename, "content": content}),
                &wd,
            )
            .await;
        assert!(write_result.success, "{:?}", write_result.error_msg);

        let read_tool = ReadFileTool;
        let read_result = read_tool
            .execute(serde_json::json!({"path": filename}), &wd)
            .await;
        assert!(read_result.success, "{:?}", read_result.error_msg);
        assert_eq!(read_result.output, content);

        // Cleanup
        let _ = tokio::fs::remove_file(wd.join(filename)).await;
    }

    #[tokio::test]
    async fn test_edit_file_unique_match() {
        let wd = std::env::temp_dir();
        let filename = "flywheel_harness_tool_test_edit.txt";
        let original = "hello world\nfoo bar\n";

        tokio::fs::write(wd.join(filename), original).await.unwrap();

        let tool = EditFileTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "path": filename,
                    "old_string": "foo bar",
                    "new_string": "baz qux"
                }),
                &wd,
            )
            .await;
        assert!(result.success, "{:?}", result.error_msg);

        let content = tokio::fs::read_to_string(wd.join(filename)).await.unwrap();
        assert_eq!(content, "hello world\nbaz qux\n");

        let _ = tokio::fs::remove_file(wd.join(filename)).await;
    }

    #[tokio::test]
    async fn test_edit_file_not_found_in_file() {
        let wd = std::env::temp_dir();
        let filename = "flywheel_harness_tool_test_edit_notfound.txt";
        tokio::fs::write(wd.join(filename), "hello world\n")
            .await
            .unwrap();

        let tool = EditFileTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "path": filename,
                    "old_string": "DOES NOT EXIST",
                    "new_string": "replacement"
                }),
                &wd,
            )
            .await;
        assert!(!result.success);
        assert!(
            result
                .error_msg
                .unwrap()
                .contains("old_string not found in file")
        );

        let _ = tokio::fs::remove_file(wd.join(filename)).await;
    }

    #[tokio::test]
    async fn test_edit_file_non_unique_rejected() {
        let wd = std::env::temp_dir();
        let filename = "flywheel_harness_tool_test_edit_dupe.txt";
        tokio::fs::write(wd.join(filename), "foo\nfoo\n")
            .await
            .unwrap();

        let tool = EditFileTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "path": filename,
                    "old_string": "foo",
                    "new_string": "bar"
                }),
                &wd,
            )
            .await;
        assert!(!result.success);
        assert!(result.error_msg.unwrap().contains("must be unique"));

        let _ = tokio::fs::remove_file(wd.join(filename)).await;
    }

    #[tokio::test]
    async fn test_write_blocked_system_path() {
        let tool = WriteFileTool;
        // /etc is a system-blocked prefix; resolve_sandboxed_path will reject it
        // before we even hit is_system_blocked since it's an absolute path outside wd
        let result = tool
            .execute(
                serde_json::json!({"path": "/etc/flywheel_test", "content": "x"}),
                &std::env::temp_dir(),
            )
            .await;
        assert!(!result.success);
    }
}
