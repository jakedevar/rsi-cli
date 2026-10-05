//! File I/O tools: read_file, write_file, file_edit.
//!
//! All paths resolved through path_safety::resolve_sandboxed_path
//! and checked against the system blocklist.

use super::{
    HarnessTool, ToolContext, ToolExecutionMode, is_system_blocked, truncation::truncate_text,
};
use crate::path_safety::resolve_sandboxed_path;
use crate::session::harness::types::ToolResult;
use std::borrow::Cow;
use std::path::Path;

const MAX_READ_BYTES: usize = 50 * 1024; // 50 KB

pub struct ReadFileTool;

impl ReadFileTool {
    async fn execute_limited(
        &self,
        args: serde_json::Value,
        working_dir: &Path,
        max_output_bytes: usize,
    ) -> ToolResult {
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
            Ok(content) => ToolResult {
                success: true,
                output: truncate_text(&content, max_output_bytes.min(MAX_READ_BYTES), false)
                    .content,
                error_msg: None,
            },
            Err(e) => ToolResult {
                success: false,
                output: String::new(),
                error_msg: Some(format!("Error reading file: {e}")),
            },
        }
    }
}

#[async_trait::async_trait]
impl HarnessTool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::ParallelSafe
    }

    fn description(&self) -> &str {
        "Read the contents of a file at the given path (relative to working directory). \
         Returns the file content or an error message."
    }

    fn parameters_json(&self) -> &str {
        r#"{"type":"object","required":["path"],"properties":{"path":{"type":"string","description":"File path relative to working directory"}}}"#
    }

    async fn execute(&self, args: serde_json::Value, working_dir: &Path) -> ToolResult {
        self.execute_limited(args, working_dir, MAX_READ_BYTES)
            .await
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
         The old_string must appear exactly once in the file. When no exact \
         match exists, a fuzzy fallback folds Unicode quotes, dashes and \
         spaces to ASCII and trims trailing whitespace per line; the result \
         notes when the fuzzy fallback was used."
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

        if old.is_empty() {
            return ToolResult {
                success: false,
                output: String::new(),
                error_msg: Some("old_string must not be empty".into()),
            };
        }

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

        let matched = match_edit(&content, old);
        if matched.occurrences == 0 {
            return ToolResult {
                success: false,
                output: String::new(),
                error_msg: Some("old_string not found in file".into()),
            };
        }
        if matched.occurrences > 1 {
            let count = matched.occurrences;
            return ToolResult {
                success: false,
                output: String::new(),
                error_msg: Some(format!("old_string found {count} times; must be unique")),
            };
        }
        let Some(index) = matched.index else {
            return ToolResult {
                success: false,
                output: String::new(),
                error_msg: Some("old_string not found in file".into()),
            };
        };

        let source = matched.source;
        let new_content = format!(
            "{}{new}{}",
            &source[..index],
            &source[index + matched.length..]
        );
        if new_content == source.as_ref() {
            return ToolResult {
                success: false,
                output: String::new(),
                error_msg: Some(format!(
                    "no changes made to {path_str}: the replacement produced identical content"
                )),
            };
        }

        match tokio::fs::write(&resolved, &new_content).await {
            Ok(()) => ToolResult {
                success: true,
                output: if matched.fuzzy {
                    format!("Edited {path_str} (fuzzy match)")
                } else {
                    format!("Edited {path_str}")
                },
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

/// One resolved `file_edit` match over the original or fuzzy-normalized text.
struct EditMatch<'a> {
    /// Text the match offsets refer to; the fuzzy-normalized text when the
    /// exact match failed.
    source: Cow<'a, str>,
    /// Match offset into `source`, when a match exists.
    index: Option<usize>,
    /// Length of the matched text in `source`.
    length: usize,
    /// Number of occurrences found in `source`.
    occurrences: usize,
    /// Whether the fuzzy fallback produced the match.
    fuzzy: bool,
}

/// Locate `old` in `content`, preferring an exact match.
///
/// An exact match must be unique. When no exact match exists, both sides are
/// normalized and the match is retried. Adapted from PrimeIntellect-ai/prime-agent
/// `packages/coding-agent/src/core/tools/edit-diff.ts` @ cd1f215c (MIT); see
/// `THIRD_PARTY_NOTICES.md`.
fn match_edit<'a>(content: &'a str, old: &str) -> EditMatch<'a> {
    let exact = content.matches(old).count();
    if exact == 1 {
        return EditMatch {
            source: Cow::Borrowed(content),
            index: content.find(old),
            length: old.len(),
            occurrences: 1,
            fuzzy: false,
        };
    }
    if exact > 1 {
        return EditMatch {
            source: Cow::Borrowed(content),
            index: None,
            length: 0,
            occurrences: exact,
            fuzzy: false,
        };
    }

    let fuzzy_source = normalize_for_fuzzy_match(content);
    let fuzzy_old = normalize_for_fuzzy_match(old);
    let occurrences = if fuzzy_old.is_empty() {
        0
    } else {
        fuzzy_source.matches(fuzzy_old.as_str()).count()
    };
    if occurrences == 1 {
        let index = fuzzy_source.find(fuzzy_old.as_str());
        let length = fuzzy_old.len();
        EditMatch {
            source: Cow::Owned(fuzzy_source),
            index,
            length,
            occurrences,
            fuzzy: true,
        }
    } else {
        EditMatch {
            source: Cow::Borrowed(content),
            index: None,
            length: 0,
            occurrences,
            fuzzy: true,
        }
    }
}

/// Normalize text for the `file_edit` fuzzy fallback.
///
/// NFKC-normalizes, trims trailing whitespace from each line, and folds smart
/// quotes, Unicode dashes and special spaces to their ASCII equivalents.
/// Adapted from PrimeIntellect-ai/prime-agent
/// `packages/coding-agent/src/core/tools/edit-diff.ts` @ cd1f215c (MIT).
fn normalize_for_fuzzy_match(text: &str) -> String {
    use unicode_normalization::UnicodeNormalization as _;

    let normalized: String = text.nfkc().collect();
    let mut folded = String::with_capacity(normalized.len());
    for (line_index, line) in normalized.split('\n').enumerate() {
        if line_index > 0 {
            folded.push('\n');
        }
        for character in line.trim_end().chars() {
            folded.push(fold_fuzzy_character(character));
        }
    }
    folded
}

/// Fold one character of the fuzzy alphabet to its ASCII equivalent.
const fn fold_fuzzy_character(character: char) -> char {
    match character {
        '\u{2018}' | '\u{2019}' | '\u{201a}' | '\u{201b}' => '\'',
        '\u{201c}' | '\u{201d}' | '\u{201e}' | '\u{201f}' => '"',
        '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
        '\u{00a0}' | '\u{2002}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}' => ' ',
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp_dir() -> PathBuf {
        std::env::temp_dir()
    }

    use crate::test_support::disk_backed_tempdir as disk_backed_working_dir;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[tokio::test]
    async fn test_read_file_traversal_rejected() {
        let tool = ReadFileTool;
        let result = tool
            .execute(serde_json::json!({"path": "../../etc/passwd"}), &tmp_dir())
            .await;
        assert!(!result.success);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[tokio::test]
    async fn test_read_file_truncation_preserves_lines_and_utf8() {
        let temp = disk_backed_working_dir("session-harness-file-truncation-");
        let content = format!("first line\n{}é\nlast line", "x".repeat(MAX_READ_BYTES));
        tokio::fs::write(temp.path().join("large.txt"), &content)
            .await
            .unwrap();

        let result = ReadFileTool
            .execute(serde_json::json!({"path": "large.txt"}), temp.path())
            .await;
        assert!(result.success, "{:?}", result.error_msg);
        assert!(result.output.starts_with("first line\n[truncated:"));
        assert!(
            result
                .output
                .contains(&format!("{} bytes total", content.len()))
        );
        assert!(result.output.len() <= MAX_READ_BYTES);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[tokio::test]
    async fn read_file_context_policy_narrows_output_and_preserves_lines() {
        let temp = disk_backed_working_dir("session-harness-file-context-policy-");
        let content = format!("first line\n{}\nlast line\n", "x".repeat(200));
        tokio::fs::write(temp.path().join("large.txt"), &content)
            .await
            .unwrap();
        let context = ToolContext {
            session_id: None,
            working_dir: temp.path().to_path_buf(),
            cancel: tokio_util::sync::CancellationToken::new(),
            event_sink: None,
            policy: super::super::ToolPolicy {
                max_output_bytes: 80,
                ..super::super::ToolPolicy::default()
            },
        };

        let result = ReadFileTool
            .execute_with_context(serde_json::json!({"path": "large.txt"}), &context)
            .await;
        assert!(result.success, "{:?}", result.error_msg);
        assert!(
            result
                .output
                .starts_with("first line\n[truncated: limit 80 bytes;")
        );
        assert!(
            result
                .output
                .contains(&format!("{} bytes total", content.len()))
        );
        assert!(result.output.len() <= 80);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[tokio::test]
    async fn test_write_and_read_roundtrip() {
        let wd_dir = disk_backed_working_dir("session-harness-file-roundtrip-");
        let wd = wd_dir.path();
        let filename = "flywheel_tool_test_roundtrip.txt";
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[tokio::test]
    async fn test_edit_file_unique_match() {
        let wd_dir = disk_backed_working_dir("session-harness-file-edit-unique-");
        let wd = wd_dir.path();
        let filename = "flywheel_tool_test_edit.txt";
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[tokio::test]
    async fn test_edit_file_not_found_in_file() {
        let wd_dir = disk_backed_working_dir("session-harness-file-edit-notfound-");
        let wd = wd_dir.path();
        let filename = "flywheel_tool_test_edit_notfound.txt";
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[tokio::test]
    async fn test_edit_file_non_unique_rejected() {
        let wd_dir = disk_backed_working_dir("session-harness-file-edit-dupe-");
        let wd = wd_dir.path();
        let filename = "flywheel_tool_test_edit_dupe.txt";
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[tokio::test]
    async fn test_edit_file_fuzzy_smart_quote_match_is_flagged() {
        let wd_dir = disk_backed_working_dir("session-harness-file-edit-fuzzy-");
        let wd = wd_dir.path();
        let filename = "flywheel_tool_test_edit_fuzzy.txt";
        // The file holds a curly-quoted, non-breaking-space phrase; the edit
        // supplies the ASCII equivalent, which cannot match byte-for-byte.
        let original = "const greeting = \u{201c}hello\u{00a0}world\u{201d};\n";
        tokio::fs::write(wd.join(filename), original).await.unwrap();

        let tool = EditFileTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "path": filename,
                    "old_string": "const greeting = \"hello world\";",
                    "new_string": "const greeting = \"goodbye world\";"
                }),
                &wd,
            )
            .await;
        assert!(result.success, "{:?}", result.error_msg);
        assert!(result.output.contains("fuzzy match"), "{}", result.output);

        let content = tokio::fs::read_to_string(wd.join(filename)).await.unwrap();
        assert_eq!(content, "const greeting = \"goodbye world\";\n");

        let _ = tokio::fs::remove_file(wd.join(filename)).await;
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[tokio::test]
    async fn test_edit_file_fuzzy_ambiguous_reports_occurrences() {
        let wd_dir = disk_backed_working_dir("session-harness-file-edit-fuzzy-dupe-");
        let wd = wd_dir.path();
        let filename = "flywheel_tool_test_edit_fuzzy_dupe.txt";
        let original = "say \u{201c}hi\u{201d} now\nsay \u{201c}hi\u{201d} later\n";
        tokio::fs::write(wd.join(filename), original).await.unwrap();

        let tool = EditFileTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "path": filename,
                    "old_string": "\"hi\"",
                    "new_string": "\"bye\""
                }),
                &wd,
            )
            .await;
        assert!(!result.success);
        let error = result.error_msg.unwrap();
        assert!(error.contains("found 2 times"), "{error}");
        assert!(error.contains("must be unique"), "{error}");

        let _ = tokio::fs::remove_file(wd.join(filename)).await;
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[tokio::test]
    async fn test_edit_file_empty_old_string_rejected() {
        let wd_dir = disk_backed_working_dir("session-harness-file-edit-empty-");
        let wd = wd_dir.path();
        let filename = "flywheel_tool_test_edit_empty.txt";
        tokio::fs::write(wd.join(filename), "hello world\n")
            .await
            .unwrap();

        let result = EditFileTool
            .execute(
                serde_json::json!({
                    "path": filename,
                    "old_string": "",
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
                .contains("old_string must not be empty")
        );
        let content = tokio::fs::read_to_string(wd.join(filename)).await.unwrap();
        assert_eq!(content, "hello world\n");

        let _ = tokio::fs::remove_file(wd.join(filename)).await;
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
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
