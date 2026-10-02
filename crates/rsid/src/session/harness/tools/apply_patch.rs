// Adapted from openai/codex codex-rs/apply-patch/src/parser.rs @ c248f6d48b97eb4a2aa56147a0b11b7d763278b9 (Apache-2.0)
// and openai/codex codex-rs/core/assets/tools/apply_patch.lark @ c248f6d48b97eb4a2aa56147a0b11b7d763278b9 (Apache-2.0).

use crate::path_safety::resolve_sandboxed_path;
use crate::session::harness::tools::{HarnessTool, is_system_blocked};
use crate::session::harness::types::{HarnessFreeformToolFormat, ToolContentBlock, ToolResult};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub const APPLY_PATCH_GRAMMAR: &str = r#"start: begin_patch hunk+ end_patch
begin_patch: "*** Begin Patch" LF
end_patch: "*** End Patch" LF?

hunk: add_hunk | delete_hunk | update_hunk
add_hunk: "*** Add File: " filename LF add_line+
delete_hunk: "*** Delete File: " filename LF
update_hunk: "*** Update File: " filename LF change_move? change?

filename: /(.+)/
add_line: "+" /(.*)/ LF -> line

change_move: "*** Move to: " filename LF
change: (change_context | change_line)+ eof_line?
change_context: ("@@" | "@@ " /(.+)/) LF
change_line: ("+" | "-" | " ") /(.*)/ LF
eof_line: "*** End of File" LF

%import common.LF
"#;

const MAX_ERROR_CHARS: usize = 4_096;

#[derive(Debug, Clone, PartialEq, Eq)]
enum PatchOperation {
    AddFile {
        path: String,
        contents: String,
    },
    DeleteFile {
        path: String,
    },
    UpdateFile {
        path: String,
        destination: Option<String>,
        chunks: Vec<UpdateChunk>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct UpdateChunk {
    context: Option<String>,
    old_lines: Vec<String>,
    new_lines: Vec<String>,
    end_of_file: bool,
}

pub struct ApplyPatchTool;

#[async_trait::async_trait]
impl HarnessTool for ApplyPatchTool {
    fn name(&self) -> &str {
        "apply_patch"
    }

    fn description(&self) -> &str {
        "Apply a multi-file, multi-hunk patch using the Codex freeform patch grammar"
    }

    fn parameters_json(&self) -> &str {
        r#"{"type":"object","required":["patch"],"properties":{"patch":{"type":"string","description":"Raw text beginning with *** Begin Patch and ending with *** End Patch"}}}"#
    }

    fn to_spec(&self) -> crate::session::harness::types::HarnessToolSpec {
        crate::session::harness::types::HarnessToolSpec {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters_json: self.parameters_json().to_string(),
            freeform: Some(HarnessFreeformToolFormat {
                r#type: "grammar".into(),
                syntax: "lark".into(),
                definition: APPLY_PATCH_GRAMMAR.into(),
            }),
            kind: crate::session::harness::types::HarnessToolSpecKind::Function,
        }
    }

    async fn execute(&self, args: serde_json::Value, working_dir: &Path) -> ToolResult {
        let patch = patch_argument(&args).unwrap_or_else(|| {
            Err(ToolResult::from_blocks(
                vec![text_block("apply_patch requires a non-empty string")],
                true,
            ))
        });
        let patch = match patch {
            Ok(patch) => patch,
            Err(result) => return result,
        };

        let operations = match parse_patch(&patch) {
            Ok(operations) => operations,
            Err(error) => return error_result(error),
        };

        match apply_operations(working_dir, operations).await {
            Ok(count) => ToolResult::from_blocks(
                vec![text_block(&format!("Applied patch to {count} file(s)"))],
                false,
            ),
            Err(error) => error_result(error),
        }
    }
}

fn text_block(text: &str) -> ToolContentBlock {
    ToolContentBlock::Text { text: text.into() }
}

fn bounded_error(message: String) -> String {
    if message.chars().count() <= MAX_ERROR_CHARS {
        return message;
    }
    let suffix = "…";
    message
        .chars()
        .take(MAX_ERROR_CHARS.saturating_sub(suffix.chars().count()))
        .chain(suffix.chars())
        .collect()
}

fn error_result(message: String) -> ToolResult {
    ToolResult::from_blocks(vec![text_block(&bounded_error(message))], true)
}

fn patch_argument(args: &serde_json::Value) -> Option<Result<String, ToolResult>> {
    let patch = args.as_str().or_else(|| args.get("patch")?.as_str())?;
    if patch.trim().is_empty() {
        return Some(Err(error_result(
            "apply_patch requires a non-empty patch".to_string(),
        )));
    }
    Some(Ok(patch.to_string()))
}

fn parse_patch(patch: &str) -> Result<Vec<PatchOperation>, String> {
    let lines: Vec<&str> = patch
        .lines()
        .map(|line| line.trim_end_matches('\r'))
        .collect();
    if lines.first() != Some(&"*** Begin Patch") {
        return Err("patch must begin with *** Begin Patch".to_string());
    }
    if lines.last() != Some(&"*** End Patch") {
        return Err("patch must end with *** End Patch".to_string());
    }

    let mut operations = Vec::new();
    let mut index = 1;
    while index < lines.len().saturating_sub(1) {
        let line = lines[index];
        index += 1;
        if let Some(path) = marker_value(line, "*** Add File: ") {
            let mut contents = Vec::new();
            while index < lines.len() && lines[index].starts_with('+') {
                contents.push(lines[index][1..].to_string());
                index += 1;
            }
            if contents.is_empty() {
                return Err(format!("file '{path}', hunk 1: add operation has no lines"));
            }
            if index < lines.len().saturating_sub(1) && !is_operation_marker(lines[index]) {
                return Err(format!(
                    "file '{path}', hunk 1: add operation contains an invalid line"
                ));
            }
            operations.push(PatchOperation::AddFile {
                path,
                contents: format!("{}\n", contents.join("\n")),
            });
        } else if let Some(path) = marker_value(line, "*** Delete File: ") {
            operations.push(PatchOperation::DeleteFile { path });
        } else if let Some(path) = marker_value(line, "*** Update File: ") {
            let mut destination = None;
            if index < lines.len().saturating_sub(1) {
                if let Some(move_path) = marker_value(lines[index], "*** Move to: ") {
                    destination = Some(move_path);
                    index += 1;
                }
            }
            let mut chunks = Vec::new();
            while index < lines.len().saturating_sub(1) && !is_operation_marker(lines[index]) {
                if let Some(context) = change_context(lines[index]) {
                    chunks.push(UpdateChunk {
                        context,
                        ..Default::default()
                    });
                } else if lines[index] == "*** End of File" {
                    let Some(chunk) = chunks.last_mut() else {
                        return Err(format!(
                            "file '{path}', hunk 1: *** End of File has no hunk"
                        ));
                    };
                    chunk.end_of_file = true;
                } else if let Some((prefix, value)) = change_line(lines[index]) {
                    let Some(chunk) = chunks.last_mut() else {
                        return Err(format!(
                            "file '{path}', hunk 1: change line appears before a hunk"
                        ));
                    };
                    match prefix {
                        '+' => chunk.new_lines.push(value),
                        '-' => chunk.old_lines.push(value),
                        _ => {
                            chunk.old_lines.push(value.clone());
                            chunk.new_lines.push(value);
                        }
                    }
                } else {
                    return Err(format!(
                        "file '{path}', hunk {}: invalid change line",
                        chunks.len().max(1)
                    ));
                }
                index += 1;
            }
            if destination.is_none() && chunks.is_empty() {
                return Err(format!(
                    "file '{path}', hunk 1: update operation has no changes or move"
                ));
            }
            operations.push(PatchOperation::UpdateFile {
                path,
                destination,
                chunks,
            });
        } else {
            return Err(format!("patch line {index}: invalid operation marker"));
        }
    }
    if operations.is_empty() {
        return Err("patch must contain at least one operation".to_string());
    }
    Ok(operations)
}

fn marker_value<'a>(line: &'a str, marker: &str) -> Option<String> {
    line.starts_with(marker)
        .then(|| line[marker.len()..].trim().to_string())
        .filter(|value| !value.is_empty())
}

fn is_operation_marker(line: &str) -> bool {
    line.starts_with("*** Add File: ")
        || line.starts_with("*** Delete File: ")
        || line.starts_with("*** Update File: ")
}

fn change_context(line: &str) -> Option<Option<String>> {
    if line == "@@" {
        Some(None)
    } else if let Some(context) = line.strip_prefix("@@ ") {
        Some(Some(context.to_string()))
    } else {
        None
    }
}

fn change_line(line: &str) -> Option<(char, String)> {
    let mut chars = line.chars();
    let prefix = chars.next()?;
    if !matches!(prefix, '+' | '-' | ' ') {
        return None;
    }
    let value: String = chars.collect();
    Some((prefix, value))
}

async fn apply_operations(
    working_dir: &Path,
    operations: Vec<PatchOperation>,
) -> Result<usize, String> {
    let mut resolved_operations = Vec::new();
    for operation in operations {
        resolved_operations.push(resolve_operation(working_dir, operation)?);
    }

    let mut effects: HashMap<PathBuf, Option<String>> = HashMap::new();
    let mut touched = 0;
    for operation in resolved_operations {
        match operation {
            ResolvedOperation::AddFile { path, contents } => {
                if path.exists() || effects.contains_key(&path) {
                    return Err(format!("file '{}': already exists", path.display()));
                }
                effects.insert(path, Some(contents));
                touched += 1;
            }
            ResolvedOperation::DeleteFile { path } => {
                read_effect(&effects, &path).await?;
                effects.insert(path, None);
                touched += 1;
            }
            ResolvedOperation::UpdateFile {
                path,
                destination,
                chunks,
            } => {
                let contents = read_effect(&effects, &path).await?;
                let updated = apply_update_chunks(&contents, &path, &chunks)?;
                effects.insert(path.clone(), None);
                effects.insert(destination.clone(), Some(updated));
                touched += 1;
            }
        }
    }

    let mut backups = Vec::new();
    for path in effects.keys() {
        backups.push((path.clone(), tokio::fs::read(&path).await.ok()));
    }

    for (path, contents) in &effects {
        if let Some(contents) = contents {
            if let Some(parent) = path.parent() {
                if let Err(error) = tokio::fs::create_dir_all(parent).await {
                    let cause = format!("cannot create directory for {}: {error}", path.display());
                    return Err(rollback(&backups, cause).await);
                }
            }
            if let Err(error) = tokio::fs::write(path, contents).await {
                let cause = format!("cannot write {}: {error}", path.display());
                return Err(rollback(&backups, cause).await);
            }
        } else if tokio::fs::remove_file(path).await.is_err() && path.exists() {
            let error = format!("cannot delete {}", path.display());
            return Err(rollback(&backups, error).await);
        }
    }
    Ok(touched)
}

enum ResolvedOperation {
    AddFile {
        path: PathBuf,
        contents: String,
    },
    DeleteFile {
        path: PathBuf,
    },
    UpdateFile {
        path: PathBuf,
        destination: PathBuf,
        chunks: Vec<UpdateChunk>,
    },
}

fn resolve_operation(
    working_dir: &Path,
    operation: PatchOperation,
) -> Result<ResolvedOperation, String> {
    match operation {
        PatchOperation::AddFile { path, contents } => {
            let path = resolve_path(working_dir, &path)?;
            Ok(ResolvedOperation::AddFile { path, contents })
        }
        PatchOperation::DeleteFile { path } => {
            let path = resolve_path(working_dir, &path)?;
            Ok(ResolvedOperation::DeleteFile { path })
        }
        PatchOperation::UpdateFile {
            path,
            destination,
            chunks,
        } => {
            let path = resolve_path(working_dir, &path)?;
            let destination = match destination {
                Some(destination) => resolve_path(working_dir, &destination)?,
                None => path.clone(),
            };
            Ok(ResolvedOperation::UpdateFile {
                path,
                destination,
                chunks,
            })
        }
    }
}

fn resolve_path(working_dir: &Path, path: &str) -> Result<PathBuf, String> {
    let resolved = resolve_sandboxed_path(working_dir, path)
        .map_err(|error| format!("file '{path}': {error}"))?;
    if is_system_blocked(&resolved) {
        return Err(format!("file '{path}': system path is blocked"));
    }
    Ok(resolved)
}

async fn read_effect(
    effects: &HashMap<PathBuf, Option<String>>,
    path: &Path,
) -> Result<String, String> {
    if let Some(contents) = effects.get(path) {
        return contents
            .clone()
            .ok_or_else(|| format!("file '{}': already deleted", path.display()));
    }
    tokio::fs::read_to_string(path)
        .await
        .map_err(|error| format!("cannot read file '{}': {error}", path.display()))
}

fn apply_update_chunks(
    contents: &str,
    path: &Path,
    chunks: &[UpdateChunk],
) -> Result<String, String> {
    let mut lines = split_lines(contents);
    let mut replacements = Vec::new();
    let mut search_start = 0;
    for (hunk_index, chunk) in chunks.iter().enumerate() {
        let hunk_number = hunk_index + 1;
        if let Some(context) = &chunk.context {
            let context_index = lines[search_start..]
                .iter()
                .position(|line| line == context)
                .map(|index| index + search_start)
                .ok_or_else(|| {
                    format!(
                        "file '{}', hunk {hunk_number}: context mismatch; expected context not found: {}",
                        path.display(),
                        bounded_error(context.clone())
                    )
                })?;
            search_start = context_index;
        }

        if chunk.old_lines.is_empty() {
            let insertion_index = if search_start > lines.len() {
                lines.len()
            } else {
                search_start
            };
            replacements.push((insertion_index, 0, chunk.new_lines.clone()));
            search_start = insertion_index;
            continue;
        }

        let match_index = lines[search_start..]
            .windows(chunk.old_lines.len())
            .position(|window| window == chunk.old_lines)
            .map(|index| index + search_start);
        let Some(match_index) = match_index else {
            return Err(format!(
                "file '{}', hunk {hunk_number}: context mismatch; expected lines not found:\n{}",
                path.display(),
                bounded_error(chunk.old_lines.join("\n"))
            ));
        };
        if chunk.end_of_file && match_index + chunk.old_lines.len() != lines.len() {
            return Err(format!(
                "file '{}', hunk {hunk_number}: *** End of File does not match the final lines",
                path.display()
            ));
        }
        replacements.push((match_index, chunk.old_lines.len(), chunk.new_lines.clone()));
        search_start = match_index + chunk.old_lines.len();
    }

    for (start, length, replacement) in replacements.iter().rev() {
        lines.splice(*start..*start + length, replacement.iter().cloned());
    }
    if lines.is_empty() {
        return Ok(String::new());
    }
    Ok(format!("{}\n", lines.join("\n")))
}

fn split_lines(contents: &str) -> Vec<String> {
    let mut lines: Vec<String> = contents.split('\n').map(str::to_string).collect();
    if lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines
}

async fn rollback(backups: &[(PathBuf, Option<Vec<u8>>)], cause: String) -> String {
    for (path, backup) in backups.iter().rev() {
        let restore = match backup {
            Some(contents) => tokio::fs::write(path, contents).await,
            None => match tokio::fs::remove_file(path).await {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error),
            },
        };
        if restore.is_err() {
            return format!("{cause}; rollback also failed for {}", path.display());
        }
    }
    cause
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_dir() -> tempfile::TempDir {
        tempfile::TempDir::new().unwrap()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[tokio::test]
    async fn apply_patch_supports_add_update_delete_and_move() {
        let working_dir = temp_dir();
        tokio::fs::write(
            working_dir.path().join("update.txt"),
            "alpha\nold\nomega\nold two\n",
        )
        .await
        .unwrap();
        tokio::fs::write(working_dir.path().join("delete.txt"), "delete me\n")
            .await
            .unwrap();
        tokio::fs::write(working_dir.path().join("move.txt"), "move me\n")
            .await
            .unwrap();

        let patch = "\
*** Begin Patch
*** Add File: added.txt
+first
+second
*** Update File: update.txt
@@ alpha
 alpha
-old
+new
@@ omega
 omega
-old two
+new two
*** Delete File: delete.txt
*** Update File: move.txt
*** Move to: moved/renamed.txt
*** End Patch
";
        let result = ApplyPatchTool
            .execute(json!({"patch": patch}), working_dir.path())
            .await;
        assert!(result.success, "{:?}", result.error_msg);
        assert_eq!(
            tokio::fs::read_to_string(working_dir.path().join("added.txt"))
                .await
                .unwrap(),
            "first\nsecond\n"
        );
        assert_eq!(
            tokio::fs::read_to_string(working_dir.path().join("update.txt"))
                .await
                .unwrap(),
            "alpha\nnew\nomega\nnew two\n"
        );
        assert!(!working_dir.path().join("delete.txt").exists());
        assert!(!working_dir.path().join("move.txt").exists());
        assert_eq!(
            tokio::fs::read_to_string(working_dir.path().join("moved/renamed.txt"))
                .await
                .unwrap(),
            "move me\n"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[tokio::test]
    async fn malformed_patch_returns_bounded_error_and_writes_nothing() {
        let working_dir = temp_dir();
        let patch = "*** Begin Patch\n*** Add File: broken.txt\nnot a patch line\n*** End Patch\n";
        let result = ApplyPatchTool
            .execute(json!({"patch": patch}), working_dir.path())
            .await;
        assert!(!result.success);
        assert!(result.has_typed_blocks());
        assert!(result.output.contains("file 'broken.txt'"));
        assert!(!working_dir.path().join("broken.txt").exists());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[tokio::test]
    async fn context_mismatch_is_all_or_nothing() {
        let working_dir = temp_dir();
        tokio::fs::write(working_dir.path().join("target.txt"), "one\n")
            .await
            .unwrap();
        let patch = "\
*** Begin Patch
*** Add File: would-exist.txt
+value
*** Update File: target.txt
@@ missing-context
 missing-context
-one
+two
*** End Patch
";
        let result = ApplyPatchTool
            .execute(json!({"patch": patch}), working_dir.path())
            .await;
        assert!(!result.success);
        assert!(result.output.contains("file "));
        assert!(result.output.contains("hunk 1"));
        assert!(result.output.contains("context mismatch"));
        assert!(!working_dir.path().join("would-exist.txt").exists());
        assert_eq!(
            tokio::fs::read_to_string(working_dir.path().join("target.txt"))
                .await
                .unwrap(),
            "one\n"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[tokio::test]
    async fn path_escape_is_rejected_before_any_write() {
        let working_dir = temp_dir();
        let patch = "\
*** Begin Patch
*** Add File: ../escaped.txt
+outside
*** End Patch
";
        let result = ApplyPatchTool
            .execute(json!({"patch": patch}), working_dir.path())
            .await;
        assert!(!result.success);
        assert!(result.output.contains("Path escapes working directory"));
        assert!(!working_dir.path().join("../escaped.txt").exists());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn raw_custom_patch_argument_is_accepted() {
        let args: serde_json::Value = "*** Begin Patch\n*** End Patch\n".to_string().into();
        assert_eq!(
            patch_argument(&args).unwrap().unwrap(),
            "*** Begin Patch\n*** End Patch\n"
        );
    }
}
