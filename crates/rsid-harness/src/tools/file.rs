//! File I/O tools: read_file, write_file, file_edit.
//!
//! All paths resolved through path_safety::resolve_sandboxed_path
//! and checked against the system blocklist.

use super::navigation::{clip_at_char_boundary, failure, optional_uint};
use super::{HarnessTool, ToolContext, ToolExecutionMode, is_system_blocked};
use crate::paths::resolve_path_in_scope;
use crate::types::ToolResult;
use rsi_common::harness_tool_policy::FilePathScope;
use std::borrow::Cow;
use std::ops::Range;
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt as _, BufReader};

/// Most bytes one `read_file` call returns; a longer range stops at a line
/// boundary and the footer says where to continue.
const MAX_READ_BYTES: usize = 50 * 1024; // 50 KB
/// Default and maximum lines per call.
const MAX_READ_LINES: usize = 2000;
/// A single line longer than this is clipped (minified files).
const MAX_LINE_BYTES: usize = 2000;
/// A NUL byte in this prefix marks a file as binary.
const BINARY_SNIFF_BYTES: usize = 8192;
/// Larger files are refused: counting their lines would be an unbounded scan.
const MAX_READ_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// Room kept for the footer inside the output budget.
const FOOTER_RESERVE_BYTES: usize = 128;

/// The system blocklist protects the host; a task-environment session runs in
/// a disposable container whose `/etc` and `/usr/lib` the task legitimately edits.
fn system_blocked(path: &Path, scope: FilePathScope) -> bool {
    scope == FilePathScope::WorkingDir && is_system_blocked(path)
}

pub struct ReadFileTool;

impl ReadFileTool {
    async fn execute_limited(
        &self,
        args: serde_json::Value,
        working_dir: &Path,
        scope: FilePathScope,
        max_output_bytes: usize,
    ) -> ToolResult {
        let path_str = args
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if path_str.is_empty() {
            return failure("invalid_arguments: path is required");
        }
        let offset = match optional_uint(&args, "offset") {
            Ok(Some(0)) => {
                return failure("invalid_arguments: offset is 1-indexed and must be at least 1");
            }
            Ok(offset) => offset.unwrap_or(1),
            Err(message) => return failure(message),
        };
        let limit = match optional_uint(&args, "limit") {
            Ok(Some(0)) => return failure("invalid_arguments: limit must be at least 1"),
            Ok(limit) => limit.unwrap_or(MAX_READ_LINES).min(MAX_READ_LINES),
            Err(message) => return failure(message),
        };

        let resolved = match resolve_path_in_scope(working_dir, path_str, scope) {
            Ok(p) => p,
            Err(e) => {
                return ToolResult {
                    success: false,
                    output: String::new(),
                    error_msg: Some(format!("Path error: {e}")),
                };
            }
        };

        if system_blocked(&resolved, scope) {
            return ToolResult {
                success: false,
                output: String::new(),
                error_msg: Some("Access to system path blocked".into()),
            };
        }

        let budget = max_output_bytes
            .min(MAX_READ_BYTES)
            .saturating_sub(FOOTER_RESERVE_BYTES);
        match read_window(&resolved, path_str, offset, limit, budget).await {
            Ok(output) => ToolResult {
                success: true,
                output,
                error_msg: None,
            },
            Err(message) => failure(message),
        }
    }
}

/// One line as the model sees it: terminator removed, invalid UTF-8 replaced,
/// clipped at [`MAX_LINE_BYTES`].
fn display_line(raw: &[u8]) -> String {
    let raw = raw.strip_suffix(b"\n").unwrap_or(raw);
    let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
    let text = String::from_utf8_lossy(raw);
    let (kept, dropped) = clip_at_char_boundary(&text, MAX_LINE_BYTES);
    if dropped == 0 {
        kept.to_string()
    } else {
        format!("{kept} ... [line truncated: {dropped} more bytes]")
    }
}

/// Read lines `offset..offset+limit` of a text file as numbered `N<TAB>text`
/// lines, stopping early at `budget` bytes, followed by a footer that gives
/// the range, the total line count and, when lines remain, the offset to
/// continue from. Errors are typed `code: message` strings.
async fn read_window(
    path: &Path,
    shown_path: &str,
    offset: usize,
    limit: usize,
    budget: usize,
) -> Result<String, String> {
    let io_error = |error: std::io::Error| format!("Error reading file: {error}");
    let file = tokio::fs::File::open(path).await.map_err(io_error)?;
    let metadata = file.metadata().await.map_err(io_error)?;
    if !metadata.is_file() {
        return Err(format!(
            "not_a_file: {shown_path} is not a regular file; use list_files for directories"
        ));
    }
    if metadata.len() > MAX_READ_FILE_BYTES {
        return Err(format!(
            "file_too_large: {shown_path} is {} bytes (limit {MAX_READ_FILE_BYTES}); \
             search it with grep instead",
            metadata.len()
        ));
    }
    let mut reader = BufReader::with_capacity(BINARY_SNIFF_BYTES, file);
    let head = reader.fill_buf().await.map_err(io_error)?;
    if head.contains(&0) {
        return Err(format!(
            "binary_file: {shown_path} looks like a binary file (NUL byte in the first \
             {BINARY_SNIFF_BYTES} bytes); read_file returns text only"
        ));
    }

    let mut body = String::new();
    let mut raw = Vec::new();
    let mut total = 0usize;
    let mut shown = 0usize;
    let mut last_shown = 0usize;
    let mut window_closed = false;
    loop {
        raw.clear();
        if reader.read_until(b'\n', &mut raw).await.map_err(io_error)? == 0 {
            break;
        }
        total += 1;
        if total < offset || window_closed {
            continue;
        }
        if shown >= limit {
            window_closed = true;
            continue;
        }
        let numbered = format!("{total}\t{}\n", display_line(&raw));
        // The first line is always returned so a call makes progress.
        if shown > 0 && body.len() + numbered.len() > budget {
            window_closed = true;
            continue;
        }
        body.push_str(&numbered);
        shown += 1;
        last_shown = total;
    }

    if total == 0 {
        return Ok("[file is empty: 0 lines]".to_string());
    }
    if shown == 0 {
        return Err(format!(
            "offset_out_of_range: offset {offset} is past the end of {shown_path} \
             ({total} lines)"
        ));
    }
    body.push_str(&format!("[lines {offset}-{last_shown} of {total}]"));
    if last_shown < total {
        body.push_str(&format!(
            "\n[more lines: call with offset={}]",
            last_shown + 1
        ));
    }
    Ok(body)
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
        "Read a text file (path relative to the working directory). Each line is returned as \
         `N<TAB>text`, where the number is the 1-indexed line number and not part of the file, \
         followed by a `[lines A-B of TOTAL]` footer. Returns at most `limit` lines (default and \
         maximum 2000) starting at `offset`, and fewer when the output would pass 50 KB; when \
         lines remain the footer says `[more lines: call with offset=K]`. Binary files are \
         refused. To find where to read, use grep or glob first."
    }

    fn parameters_json(&self) -> &str {
        r#"{"type":"object","required":["path"],"properties":{"path":{"type":"string","description":"File path relative to working directory"},"offset":{"type":"integer","minimum":1,"description":"1-indexed line to start reading from. Default 1."},"limit":{"type":"integer","minimum":1,"description":"Maximum number of lines to return. Default and maximum 2000."}}}"#
    }

    async fn execute(&self, args: serde_json::Value, working_dir: &Path) -> ToolResult {
        self.execute_limited(args, working_dir, FilePathScope::WorkingDir, MAX_READ_BYTES)
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
            result = self.execute_limited(
                args,
                &context.working_dir,
                context.policy.file_paths,
                context.policy.max_output_bytes,
            ) => result,
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
        self.write_in_scope(args, working_dir, FilePathScope::WorkingDir)
            .await
    }

    async fn execute_with_context(
        &self,
        args: serde_json::Value,
        context: &ToolContext,
    ) -> ToolResult {
        tokio::select! {
            biased;
            _ = context.cancel.cancelled() => failure("Tool execution cancelled"),
            result = self.write_in_scope(args, &context.working_dir, context.policy.file_paths) => result,
        }
    }
}

impl WriteFileTool {
    async fn write_in_scope(
        &self,
        args: serde_json::Value,
        working_dir: &Path,
        scope: FilePathScope,
    ) -> ToolResult {
        let path_str = args
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or_default();

        let resolved = match resolve_path_in_scope(working_dir, path_str, scope) {
            Ok(p) => p,
            Err(e) => {
                return ToolResult {
                    success: false,
                    output: String::new(),
                    error_msg: Some(format!("Path error: {e}")),
                };
            }
        };

        if system_blocked(&resolved, scope) {
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

/// Unified-diff context lines around each changed region in the result.
const DIFF_CONTEXT_LINES: usize = 3;
/// Most bytes of diff excerpt `file_edit` returns; the rest is summarized.
const MAX_DIFF_BYTES: usize = 4 * 1024;
/// A diff line longer than this is clipped (minified files).
const MAX_DIFF_LINE_BYTES: usize = 400;
/// Wall-clock cap for computing the diff excerpt; the diff approximates after it.
const DIFF_TIMEOUT: Duration = Duration::from_millis(250);

/// One requested replacement, validated but not yet matched against the file.
#[derive(Debug)]
struct EditRequest {
    old: String,
    new: String,
    replace_all: bool,
}

/// Where one edit lands in the original file.
#[derive(Debug)]
struct EditSpan {
    /// Byte range of the file text (after any BOM) that the edit replaces.
    start: usize,
    end: usize,
    /// Index into the request list.
    edit: usize,
    /// Fuzzy-normalized text written before and after `new_string` when a fuzzy
    /// match starts or ends inside one unit of the map back to the original (a
    /// ligature, or a line that cannot be mapped character by character), so
    /// the span widens to the whole unit. Empty on the exact path.
    prefix: String,
    suffix: String,
}

/// Every edit matched against the original file, ready to apply.
struct EditPlan {
    /// Non-overlapping spans in file order, as byte offsets of the original.
    spans: Vec<EditSpan>,
    /// Whether the fuzzy fallback produced the match.
    fuzzy: bool,
}

/// `edits[i]: ` when the call carries several edits, so an error names the
/// entry the model must fix; empty for a single edit.
fn edit_label(index: usize, total: usize) -> String {
    if total > 1 {
        format!("edits[{index}]: ")
    } else {
        String::new()
    }
}

fn optional_bool(value: &serde_json::Value, key: &str) -> Result<Option<bool>, String> {
    match value.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Bool(flag)) => Ok(Some(*flag)),
        Some(_) => Err(format!("invalid_arguments: {key} must be a boolean")),
    }
}

/// Read the requested edits: the `edits` array, plus the top-level
/// `old_string`/`new_string` shorthand as one more edit when present. A
/// top-level `replace_all` is the default for every edit that sets none.
fn parse_edit_requests(args: &serde_json::Value) -> Result<Vec<EditRequest>, String> {
    let default_replace_all = optional_bool(args, "replace_all")?.unwrap_or(false);

    let mut entries: Vec<&serde_json::Value> = Vec::new();
    // Some models send an array argument as a JSON-encoded string.
    let decoded;
    if let Some(edits) = args.get("edits").filter(|value| !value.is_null()) {
        let edits = match edits.as_str() {
            Some(text) => {
                decoded = serde_json::from_str::<serde_json::Value>(text).map_err(|error| {
                    format!(
                        "invalid_arguments: edits must be an array of \
                         {{old_string, new_string}} objects ({error})"
                    )
                })?;
                &decoded
            }
            None => edits,
        };
        let Some(items) = edits.as_array() else {
            return Err("invalid_arguments: edits must be an array of \
                        {old_string, new_string} objects"
                .to_string());
        };
        entries.extend(items);
    }
    if ["old_string", "new_string"]
        .iter()
        .any(|key| args.get(key).is_some_and(|value| !value.is_null()))
    {
        entries.push(args);
    }
    if entries.is_empty() {
        return Err("invalid_arguments: provide edits (a non-empty array of \
                    {old_string, new_string}) or old_string and new_string"
            .to_string());
    }

    let total = entries.len();
    entries
        .into_iter()
        .enumerate()
        .map(|(index, entry)| {
            let label = edit_label(index, total);
            let old = entry
                .get("old_string")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| format!("invalid_arguments: {label}old_string is required"))?;
            if old.is_empty() {
                return Err(format!("{label}old_string must not be empty"));
            }
            let new = entry
                .get("new_string")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    format!(
                        "invalid_arguments: {label}new_string is required \
                         (use an empty string to delete the text)"
                    )
                })?;
            Ok(EditRequest {
                old: old.to_string(),
                new: new.to_string(),
                replace_all: optional_bool(entry, "replace_all")?.unwrap_or(default_replace_all),
            })
        })
        .collect()
}

/// The line ending `file_edit` writes into the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LineEnding {
    Lf,
    Crlf,
}

/// CRLF when the file has a line break and every one of them is CRLF. A mixed
/// file counts as LF, so text is then written as the model sent it.
fn detect_line_ending(text: &str) -> LineEnding {
    let breaks = text.matches('\n').count();
    if breaks == 0 || text.matches("\r\n").count() != breaks {
        return LineEnding::Lf;
    }
    LineEnding::Crlf
}

/// `text` with every bare LF written as `ending`, so an LF `old_string` finds
/// its CRLF lines and an LF `new_string` keeps a CRLF file CRLF.
fn with_line_ending(text: &str, ending: LineEnding) -> Cow<'_, str> {
    if ending == LineEnding::Lf || !text.contains('\n') {
        return Cow::Borrowed(text);
    }
    let mut converted = String::with_capacity(text.len() + text.len() / 16);
    let mut previous = '\0';
    for character in text.chars() {
        if character == '\n' && previous != '\r' {
            converted.push('\r');
        }
        converted.push(character);
        previous = character;
    }
    Cow::Owned(converted)
}

/// The file's UTF-8 byte order mark (or nothing) and the text after it. The
/// mark is kept out of matching and written back unchanged.
fn split_bom(content: &str) -> (&str, &str) {
    match content.strip_prefix('\u{feff}') {
        Some(body) => ("\u{feff}", body),
        None => ("", content),
    }
}

/// Every occurrence of every needle in `source` as `(start, end, edit)`,
/// refusing a needle that is missing, empty, or ambiguous without
/// `replace_all`.
fn find_matches<S: AsRef<str>>(
    source: &str,
    needles: &[S],
    edits: &[EditRequest],
) -> Result<Vec<(usize, usize, usize)>, String> {
    let mut matches = Vec::new();
    for (index, (edit, needle)) in edits.iter().zip(needles).enumerate() {
        let needle = needle.as_ref();
        let label = edit_label(index, edits.len());
        // A needle that normalizes to nothing matches everywhere; refuse it.
        let found: Vec<usize> = if needle.is_empty() {
            Vec::new()
        } else {
            source
                .match_indices(needle)
                .map(|(start, _)| start)
                .collect()
        };
        if found.is_empty() {
            return Err(format!("{label}old_string not found in file"));
        }
        if found.len() > 1 && !edit.replace_all {
            return Err(format!(
                "{label}old_string found {} times; must be unique (add surrounding context \
                 to make it unique, or set replace_all to replace every occurrence)",
                found.len()
            ));
        }
        matches.extend(
            found
                .into_iter()
                .map(|start| (start, start + needle.len(), index)),
        );
    }
    Ok(matches)
}

/// Match every edit against the original file and check that no two overlap.
///
/// All edits are matched against the same text, never against the result of
/// another edit, and every span is a byte range of the original, so several
/// edits apply in one pass with correct offsets. An exact match must be unique
/// unless the edit sets `replace_all`; in a CRLF file an `old_string` written
/// with LF matches its CRLF lines exactly. When any edit has no exact match,
/// every edit is matched in the fuzzy-normalized text instead (one common
/// offset space) and each match is mapped back to the original bytes, so only
/// the matched text changes. Adapted from PrimeIntellect-ai/prime-agent
/// `packages/coding-agent/src/core/tools/edit-diff.ts` @ cd1f215c (MIT); see
/// `THIRD_PARTY_NOTICES.md`.
fn plan_edits(
    content: &str,
    edits: &[EditRequest],
    ending: LineEnding,
) -> Result<EditPlan, String> {
    let needles: Vec<Cow<'_, str>> = edits
        .iter()
        .map(|edit| with_line_ending(&edit.old, ending))
        .collect();
    let fuzzy = needles
        .iter()
        .any(|needle| !content.contains(needle.as_ref()));
    let spans = if fuzzy {
        fuzzy_spans(content, edits)?
    } else {
        find_matches(content, &needles, edits)?
            .into_iter()
            .map(|(start, end, edit)| EditSpan {
                start,
                end,
                edit,
                prefix: String::new(),
                suffix: String::new(),
            })
            .collect()
    };
    let spans = sorted_without_overlap(spans)?;
    Ok(EditPlan { spans, fuzzy })
}

/// Match every edit in the fuzzy-normalized text and map each match back to
/// a byte range of the original.
fn fuzzy_spans(content: &str, edits: &[EditRequest]) -> Result<Vec<EditSpan>, String> {
    let normalized = FuzzyText::new(content);
    let needles: Vec<String> = edits
        .iter()
        .map(|edit| normalize_for_fuzzy_match(&edit.old))
        .collect();
    let matches = find_matches(&normalized.text, &needles, edits)?;
    Ok(matches
        .into_iter()
        .map(|(start, end, edit)| {
            let (start, prefix) = normalized.map_start(start);
            let (end, suffix) = normalized.map_end(end);
            EditSpan {
                start,
                end,
                edit,
                prefix: prefix.to_string(),
                suffix: suffix.to_string(),
            }
        })
        .collect())
}

/// The spans in file order, refused when two overlap.
fn sorted_without_overlap(mut spans: Vec<EditSpan>) -> Result<Vec<EditSpan>, String> {
    spans.sort_by_key(|span| (span.start, span.end));
    if let Some(pair) = spans.windows(2).find(|pair| pair[1].start < pair[0].end) {
        return Err(format!(
            "overlapping_edits: edits[{}] and edits[{}] match overlapping text; every edit \
             is matched against the original file, so merge them into one edit. No edit was \
             applied",
            pair[0].edit, pair[1].edit
        ));
    }
    Ok(spans)
}

/// The original text with every span replaced by its edit's `new_string`,
/// written with the file's line ending. Bytes outside the spans are copied
/// unchanged.
fn apply_plan(content: &str, plan: &EditPlan, edits: &[EditRequest], ending: LineEnding) -> String {
    let mut applied = String::with_capacity(content.len());
    let mut cursor = 0;
    for span in &plan.spans {
        applied.push_str(&content[cursor..span.start]);
        applied.push_str(&span.prefix);
        applied.push_str(&with_line_ending(&edits[span.edit].new, ending));
        applied.push_str(&span.suffix);
        cursor = span.end;
    }
    applied.push_str(&content[cursor..]);
    applied
}

/// A short unified diff of the change, so the model need not re-read the file.
/// Long diffs stop at a line boundary and say how much was left out.
fn diff_excerpt(shown_path: &str, before: &str, after: &str) -> String {
    let diff = similar::TextDiff::configure()
        .algorithm(similar::Algorithm::Myers)
        .timeout(DIFF_TIMEOUT)
        .diff_lines(before, after);
    let unified = diff
        .unified_diff()
        .context_radius(DIFF_CONTEXT_LINES)
        .header(&format!("a/{shown_path}"), &format!("b/{shown_path}"))
        .to_string();

    let total = unified.lines().count();
    let mut excerpt = String::new();
    let mut shown = 0usize;
    for line in unified.lines() {
        let (kept, dropped) = clip_at_char_boundary(line, MAX_DIFF_LINE_BYTES);
        let rendered = if dropped == 0 {
            kept.to_string()
        } else {
            format!("{kept} ... [line truncated: {dropped} more bytes]")
        };
        if shown > 0 && excerpt.len() + rendered.len() + 1 > MAX_DIFF_BYTES {
            break;
        }
        excerpt.push_str(&rendered);
        excerpt.push('\n');
        shown += 1;
    }
    if shown < total {
        excerpt.push_str(&format!(
            "[diff truncated: {} more lines; read_file shows the rest]\n",
            total - shown
        ));
    }
    excerpt.truncate(excerpt.trim_end().len());
    excerpt
}

#[async_trait::async_trait]
impl HarnessTool for EditFileTool {
    fn name(&self) -> &str {
        "file_edit"
    }

    fn description(&self) -> &str {
        "Edit a file by replacing exact text. Pass `edits`, an array of \
         `{old_string, new_string}` objects (each may set `replace_all`); prefer one call with \
         every change to one file over several calls. Each old_string is matched against the \
         file as it was before any edit, so edits must not overlap and cannot build on each \
         other; if any edit fails (not found, ambiguous, overlapping) none is applied. An \
         old_string must appear exactly once unless `replace_all` is true, which replaces every \
         occurrence. For one change you may pass `old_string`/`new_string` (and `replace_all`) \
         at the top level instead. The result carries a unified diff excerpt of the change, so \
         you need not read the file again. The file keeps its line endings (text you send with \
         LF is written as CRLF in a CRLF file) and any byte order mark. When no exact match \
         exists, a fuzzy fallback matches with Unicode quotes, dashes and spaces folded to \
         ASCII and trailing whitespace ignored; only the matched text is replaced and the rest \
         of the file stays byte-identical. The result notes when the fuzzy fallback was used."
    }

    fn parameters_json(&self) -> &str {
        r#"{"type":"object","required":["path"],"properties":{"path":{"type":"string","description":"File path relative to working directory"},"edits":{"type":"array","description":"Replacements to make, each matched against the original file; they must not overlap. All apply or none do.","items":{"type":"object","required":["old_string","new_string"],"properties":{"old_string":{"type":"string","description":"Exact text to find (must be unique in the file unless replace_all is true)"},"new_string":{"type":"string","description":"Text to replace it with"},"replace_all":{"type":"boolean","description":"Replace every occurrence of old_string. Default false."}}}},"old_string":{"type":"string","description":"Single-edit shorthand: exact text to find (must be unique in the file unless replace_all is true)"},"new_string":{"type":"string","description":"Single-edit shorthand: text to replace it with"},"replace_all":{"type":"boolean","description":"Replace every occurrence of old_string. Default false. Also the default for each entry of edits."}}}"#
    }

    async fn execute(&self, args: serde_json::Value, working_dir: &Path) -> ToolResult {
        self.edit_in_scope(args, working_dir, FilePathScope::WorkingDir)
            .await
    }

    async fn execute_with_context(
        &self,
        args: serde_json::Value,
        context: &ToolContext,
    ) -> ToolResult {
        tokio::select! {
            biased;
            _ = context.cancel.cancelled() => failure("Tool execution cancelled"),
            result = self.edit_in_scope(args, &context.working_dir, context.policy.file_paths) => result,
        }
    }
}

impl EditFileTool {
    async fn edit_in_scope(
        &self,
        args: serde_json::Value,
        working_dir: &Path,
        scope: FilePathScope,
    ) -> ToolResult {
        let path_str = args
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if path_str.is_empty() {
            return failure("invalid_arguments: path is required");
        }
        let edits = match parse_edit_requests(&args) {
            Ok(edits) => edits,
            Err(message) => return failure(message),
        };

        let resolved = match resolve_path_in_scope(working_dir, path_str, scope) {
            Ok(p) => p,
            Err(e) => return failure(format!("Path error: {e}")),
        };

        if system_blocked(&resolved, scope) {
            return failure("Edit of system path blocked");
        }

        let content = match tokio::fs::read_to_string(&resolved).await {
            Ok(c) => c,
            Err(e) => return failure(format!("Error reading file: {e}")),
        };

        let (bom, body) = split_bom(&content);
        let ending = detect_line_ending(body);
        let plan = match plan_edits(body, &edits, ending) {
            Ok(plan) => plan,
            Err(message) => return failure(message),
        };
        let new_body = apply_plan(body, &plan, &edits, ending);
        if new_body == body {
            return failure(format!(
                "no changes made to {path_str}: the replacement produced identical content"
            ));
        }
        let new_content = format!("{bom}{new_body}");

        if let Err(e) = tokio::fs::write(&resolved, &new_content).await {
            return failure(format!("Error writing file: {e}"));
        }

        let mut notes = Vec::new();
        if edits.len() > 1 {
            notes.push(format!("{} edits", edits.len()));
        }
        if plan.spans.len() != edits.len() {
            notes.push(format!("{} replacements", plan.spans.len()));
        }
        if plan.fuzzy {
            notes.push("fuzzy match".to_string());
        }
        let mut output = format!("Edited {path_str}");
        if !notes.is_empty() {
            output.push_str(&format!(" ({})", notes.join(", ")));
        }
        output.push('\n');
        output.push_str(&diff_excerpt(path_str, &content, &new_content));
        ToolResult {
            success: true,
            output,
            error_msg: None,
        }
    }
}

/// Normalize text for the `file_edit` fuzzy fallback.
///
/// Per line: NFKC-normalizes, trims trailing whitespace (including the `\r` of
/// a CRLF line), and folds smart quotes, Unicode dashes and special spaces to
/// their ASCII equivalents. Used only to find a match; the file itself is never
/// written back normalized. Adapted from PrimeIntellect-ai/prime-agent
/// `packages/coding-agent/src/core/tools/edit-diff.ts` @ cd1f215c (MIT).
fn normalize_for_fuzzy_match(text: &str) -> String {
    let lines: Vec<String> = text
        .split('\n')
        .map(|line| normalize_fuzzy_line(line).0)
        .collect();
    lines.join("\n")
}

/// One line normalized for the fuzzy fallback, with and without its trailing
/// whitespace trimmed: `(trimmed, untrimmed)`.
fn normalize_fuzzy_line(line: &str) -> (String, String) {
    use unicode_normalization::UnicodeNormalization as _;

    // Folding maps whitespace to whitespace and nothing else to whitespace, so
    // folding before the trim equals trimming first.
    let untrimmed: String = line.nfkc().map(fold_fuzzy_character).collect();
    (untrimmed.trim_end().to_string(), untrimmed)
}

/// A run of original bytes and the normalized bytes it became, both relative
/// to the start of their line.
#[derive(Debug)]
struct FuzzyUnit {
    original: Range<usize>,
    normalized: Range<usize>,
}

/// One line of a [`FuzzyText`].
#[derive(Debug)]
struct FuzzyLine {
    original_start: usize,
    normalized_start: usize,
    /// Contiguous units covering the normalized line from 0; empty when the
    /// line normalizes to nothing.
    units: Vec<FuzzyUnit>,
}

impl FuzzyLine {
    /// Where the line's trailing whitespace begins, relative to its start.
    fn kept_end(&self) -> usize {
        self.units.last().map_or(0, |unit| unit.original.end)
    }
}

/// The fuzzy-normalized form of a text plus a map from normalized offsets back
/// to original byte offsets.
///
/// Each line maps character by character when normalizing its characters one
/// at a time gives the same text as normalizing the line (true unless NFKC
/// composes across characters); otherwise the whole line is one unit. A match
/// boundary inside a unit widens the replaced span to the unit and keeps the
/// unit's normalized text outside the match (see [`EditSpan::prefix`]).
struct FuzzyText {
    text: String,
    lines: Vec<FuzzyLine>,
}

impl FuzzyText {
    fn new(original: &str) -> Self {
        let mut text = String::with_capacity(original.len());
        let mut lines = Vec::new();
        let mut original_start = 0;
        for (index, line) in original.split('\n').enumerate() {
            if index > 0 {
                text.push('\n');
            }
            let normalized_start = text.len();
            let (normalized, units) = fuzzy_line_units(line);
            text.push_str(&normalized);
            lines.push(FuzzyLine {
                original_start,
                normalized_start,
                units,
            });
            original_start += line.len() + 1;
        }
        Self { text, lines }
    }

    /// The line holding normalized `offset`; its `\n` belongs to it.
    fn line_at(&self, offset: usize) -> &FuzzyLine {
        let after = self
            .lines
            .partition_point(|line| line.normalized_start <= offset);
        &self.lines[after.saturating_sub(1)]
    }

    /// The original offset a match starting at normalized `offset` replaces
    /// from, and the normalized text of a widened unit that precedes the match.
    fn map_start(&self, offset: usize) -> (usize, &str) {
        let line = self.line_at(offset);
        let local = offset - line.normalized_start;
        let Some(unit) = line.units.iter().find(|unit| local < unit.normalized.end) else {
            // At the line break (or the end): replace from the trailing whitespace.
            return (line.original_start + line.kept_end(), "");
        };
        let prefix = &self.text[line.normalized_start + unit.normalized.start..offset];
        (line.original_start + unit.original.start, prefix)
    }

    /// The original offset a match ending at normalized `offset` replaces up
    /// to, and the normalized text of a widened unit that follows the match.
    fn map_end(&self, offset: usize) -> (usize, &str) {
        let line = self.line_at(offset);
        let local = offset - line.normalized_start;
        if local == 0 {
            return (line.original_start, "");
        }
        let Some(unit) = line.units.iter().find(|unit| local <= unit.normalized.end) else {
            return (line.original_start + line.kept_end(), "");
        };
        let suffix = &self.text[offset..line.normalized_start + unit.normalized.end];
        (line.original_start + unit.original.end, suffix)
    }
}

/// One line normalized for the fuzzy fallback with its map back to the
/// original: per character when that is exact, otherwise one unit for the line.
fn fuzzy_line_units(line: &str) -> (String, Vec<FuzzyUnit>) {
    let (normalized, untrimmed) = normalize_fuzzy_line(line);
    if let Some(units) = per_character_units(line, &untrimmed, normalized.len()) {
        return (normalized, units);
    }
    if normalized.is_empty() {
        return (normalized, Vec::new());
    }
    let whole = FuzzyUnit {
        original: 0..line.trim_end().len(),
        normalized: 0..normalized.len(),
    };
    (normalized, vec![whole])
}

/// Units of one character each, or `None` when normalizing the characters one
/// at a time does not reproduce the line's normalization or a character
/// straddles the trimmed end.
fn per_character_units(line: &str, untrimmed: &str, kept: usize) -> Option<Vec<FuzzyUnit>> {
    use unicode_normalization::UnicodeNormalization as _;

    let mut joined = String::with_capacity(untrimmed.len());
    let mut units = Vec::new();
    for (offset, character) in line.char_indices() {
        let start = joined.len();
        joined.extend(std::iter::once(character).nfkc().map(fold_fuzzy_character));
        if start >= kept {
            continue;
        }
        units.push(FuzzyUnit {
            original: offset..offset + character.len_utf8(),
            normalized: start..joined.len(),
        });
    }
    if joined != untrimmed {
        return None;
    }
    if units.last().is_some_and(|unit| unit.normalized.end != kept) {
        return None;
    }
    Some(units)
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

    async fn read(dir: &Path, args: serde_json::Value) -> ToolResult {
        ReadFileTool.execute(args, dir).await
    }

    fn numbered_file(lines: usize) -> String {
        (1..=lines).map(|n| format!("line {n}\n")).collect()
    }

    #[tokio::test]
    async fn read_file_numbers_lines_and_reports_the_total() {
        let temp = disk_backed_working_dir("session-harness-file-numbered-");
        tokio::fs::write(temp.path().join("a.txt"), "alpha\nbeta\ngamma\n")
            .await
            .unwrap();

        let result = read(temp.path(), serde_json::json!({"path": "a.txt"})).await;
        assert!(result.success, "{:?}", result.error_msg);
        assert_eq!(
            result.output,
            "1\talpha\n2\tbeta\n3\tgamma\n[lines 1-3 of 3]"
        );
    }

    #[tokio::test]
    async fn read_file_offset_and_limit_select_a_range_with_a_continuation_footer() {
        let temp = disk_backed_working_dir("session-harness-file-range-");
        tokio::fs::write(temp.path().join("ten.txt"), numbered_file(10))
            .await
            .unwrap();

        let middle = read(
            temp.path(),
            serde_json::json!({"path": "ten.txt", "offset": 4, "limit": 3}),
        )
        .await;
        assert!(middle.success, "{:?}", middle.error_msg);
        assert_eq!(
            middle.output,
            "4\tline 4\n5\tline 5\n6\tline 6\n[lines 4-6 of 10]\n[more lines: call with offset=7]"
        );

        // The last range has no continuation footer.
        let tail = read(
            temp.path(),
            serde_json::json!({"path": "ten.txt", "offset": 9, "limit": 50}),
        )
        .await;
        assert_eq!(tail.output, "9\tline 9\n10\tline 10\n[lines 9-10 of 10]");

        // An offset past the last line is a typed error that names the total.
        let past = read(
            temp.path(),
            serde_json::json!({"path": "ten.txt", "offset": 11}),
        )
        .await;
        assert!(!past.success);
        let error = past.error_msg.unwrap();
        assert!(error.starts_with("offset_out_of_range:"), "{error}");
        assert!(error.contains("10 lines"), "{error}");
    }

    #[tokio::test]
    async fn read_file_defaults_to_two_thousand_lines() {
        let temp = disk_backed_working_dir("session-harness-file-default-limit-");
        tokio::fs::write(temp.path().join("long.txt"), numbered_file(2500))
            .await
            .unwrap();

        let result = read(temp.path(), serde_json::json!({"path": "long.txt"})).await;
        assert!(result.success, "{:?}", result.error_msg);
        // 2000 short lines fit the byte budget, so the line cap is what stops it.
        assert!(result.output.contains("\n2000\tline 2000\n"));
        assert!(
            result
                .output
                .ends_with("[lines 1-2000 of 2500]\n[more lines: call with offset=2001]")
        );

        let capped = read(
            temp.path(),
            serde_json::json!({"path": "long.txt", "limit": 9000, "offset": 2400}),
        )
        .await;
        assert!(capped.output.ends_with("[lines 2400-2500 of 2500]"));
    }

    #[tokio::test]
    async fn read_file_stops_at_a_line_boundary_when_the_byte_budget_is_spent() {
        let temp = disk_backed_working_dir("session-harness-file-byte-budget-");
        let content: String = (1..=500)
            .map(|n| format!("{n:03} {}\n", "x".repeat(100)))
            .collect();
        tokio::fs::write(temp.path().join("wide.txt"), &content)
            .await
            .unwrap();

        let result = read(temp.path(), serde_json::json!({"path": "wide.txt"})).await;
        assert!(result.success, "{:?}", result.error_msg);
        assert!(
            result.output.len() <= MAX_READ_BYTES,
            "{}",
            result.output.len()
        );
        let footer = result.output.lines().last().unwrap();
        let next: usize = footer
            .strip_prefix("[more lines: call with offset=")
            .and_then(|rest| rest.strip_suffix(']'))
            .unwrap_or_else(|| panic!("no continuation footer: {footer}"))
            .parse()
            .unwrap();
        assert!(next > 100 && next < 500, "{next}");
        // Continuing from the footer returns the very next line.
        let more = read(
            temp.path(),
            serde_json::json!({"path": "wide.txt", "offset": next, "limit": 1}),
        )
        .await;
        assert!(more.output.starts_with(&format!("{next}\t{next:03} ")));
    }

    #[tokio::test]
    async fn read_file_context_policy_narrows_the_range_and_preserves_lines() {
        let temp = disk_backed_working_dir("session-harness-file-context-policy-");
        tokio::fs::write(temp.path().join("narrow.txt"), numbered_file(200))
            .await
            .unwrap();
        let context = ToolContext {
            session_id: None,
            working_dir: temp.path().to_path_buf(),
            cancel: tokio_util::sync::CancellationToken::new(),
            event_sink: None,
            policy: super::super::ToolPolicy {
                max_output_bytes: 400,
                ..super::super::ToolPolicy::default()
            },
        };

        let result = ReadFileTool
            .execute_with_context(serde_json::json!({"path": "narrow.txt"}), &context)
            .await;
        assert!(result.success, "{:?}", result.error_msg);
        assert!(result.output.len() <= 400, "{}", result.output.len());
        assert!(result.output.starts_with("1\tline 1\n2\tline 2\n"));
        assert!(
            result
                .output
                .contains("of 200]\n[more lines: call with offset=")
        );
    }

    fn scoped_context(working_dir: &Path, scope: FilePathScope) -> ToolContext {
        ToolContext {
            session_id: None,
            working_dir: working_dir.to_path_buf(),
            cancel: tokio_util::sync::CancellationToken::new(),
            event_sink: None,
            policy: super::super::ToolPolicy {
                file_paths: scope,
                ..super::super::ToolPolicy::default()
            },
        }
    }

    #[tokio::test]
    async fn write_file_creates_a_new_file_at_an_absolute_path_inside_working_dir() {
        let temp = disk_backed_working_dir("session-harness-file-abs-new-");
        let target = temp.path().canonicalize().unwrap().join("pkg/x.py");
        let result = WriteFileTool
            .execute(
                serde_json::json!({"path": target.to_str().unwrap(), "content": "print(1)\n"}),
                temp.path(),
            )
            .await;
        assert!(result.success, "{:?}", result.error_msg);
        assert_eq!(
            tokio::fs::read_to_string(&target).await.unwrap(),
            "print(1)\n"
        );
    }

    #[tokio::test]
    async fn write_file_refusal_outside_working_dir_names_it_and_suggests_relative() {
        let temp = disk_backed_working_dir("session-harness-file-abs-out-");
        let outside = disk_backed_working_dir("session-harness-file-abs-other-");
        let target = outside.path().join("x.py");
        let result = WriteFileTool
            .execute_with_context(
                serde_json::json!({"path": target.to_str().unwrap(), "content": "x"}),
                &scoped_context(temp.path(), FilePathScope::WorkingDir),
            )
            .await;
        assert!(!result.success);
        let message = result.error_msg.unwrap();
        let canon_wd = temp.path().canonicalize().unwrap();
        assert!(
            message.contains(&canon_wd.display().to_string()),
            "{message}"
        );
        assert!(
            message.contains("relative to the working directory"),
            "{message}"
        );
        assert!(!target.exists());
    }

    #[tokio::test]
    async fn task_environment_scope_lets_the_file_tools_reach_outside_working_dir() {
        let temp = disk_backed_working_dir("session-harness-file-env-wd-");
        let outside = disk_backed_working_dir("session-harness-file-env-out-");
        let target = outside
            .path()
            .canonicalize()
            .unwrap()
            .join("nested/app.conf");
        let path = target.to_str().unwrap();
        let context = scoped_context(temp.path(), FilePathScope::TaskEnvironment);

        let wrote = WriteFileTool
            .execute_with_context(
                serde_json::json!({"path": path, "content": "a=1\n"}),
                &context,
            )
            .await;
        assert!(wrote.success, "{:?}", wrote.error_msg);
        let edited = EditFileTool
            .execute_with_context(
                serde_json::json!({"path": path, "old_string": "a=1", "new_string": "a=2"}),
                &context,
            )
            .await;
        assert!(edited.success, "{:?}", edited.error_msg);
        let read = ReadFileTool
            .execute_with_context(serde_json::json!({"path": path}), &context)
            .await;
        assert!(read.success, "{:?}", read.error_msg);
        assert!(read.output.contains("a=2"), "{}", read.output);

        // The default scope stays contained for the same call.
        let contained = ReadFileTool
            .execute_with_context(
                serde_json::json!({"path": path}),
                &scoped_context(temp.path(), FilePathScope::WorkingDir),
            )
            .await;
        assert!(!contained.success);
    }

    #[tokio::test]
    async fn task_environment_scope_lifts_the_host_system_blocklist() {
        let temp = disk_backed_working_dir("session-harness-file-env-blocked-");
        let args = serde_json::json!({"path": "/proc/self/cmdline"});
        let contained = ReadFileTool
            .execute_with_context(
                args.clone(),
                &scoped_context(temp.path(), FilePathScope::WorkingDir),
            )
            .await;
        assert!(!contained.success);
        assert!(!system_blocked(
            Path::new("/etc/hosts"),
            FilePathScope::TaskEnvironment
        ));
        assert!(system_blocked(
            Path::new("/etc/hosts"),
            FilePathScope::WorkingDir
        ));
    }

    #[tokio::test]
    async fn read_file_clips_long_lines_at_a_character_boundary() {
        let temp = disk_backed_working_dir("session-harness-file-long-line-");
        // `é` is two bytes, so the 2000-byte cut falls between characters.
        let content = format!("first line\n{}\nlast line", "é".repeat(5000));
        tokio::fs::write(temp.path().join("minified.js"), &content)
            .await
            .unwrap();

        let result = read(temp.path(), serde_json::json!({"path": "minified.js"})).await;
        assert!(result.success, "{:?}", result.error_msg);
        let lines: Vec<&str> = result.output.lines().collect();
        assert_eq!(lines[0], "1\tfirst line");
        assert!(lines[1].starts_with("2\té"), "{}", &lines[1][..20]);
        assert!(lines[1].contains("... [line truncated: "), "{}", lines[1]);
        assert_eq!(lines[2], "3\tlast line");
        assert_eq!(lines[3], "[lines 1-3 of 3]");
    }

    #[tokio::test]
    async fn read_file_handles_crlf_missing_final_newline_empty_and_invalid_utf8() {
        let temp = disk_backed_working_dir("session-harness-file-shapes-");
        let write =
            |name: &str, bytes: &[u8]| std::fs::write(temp.path().join(name), bytes).unwrap();
        write("crlf.txt", b"one\r\ntwo");
        write("empty.txt", b"");
        write("latin.txt", b"caf\xe9\n");

        let crlf = read(temp.path(), serde_json::json!({"path": "crlf.txt"})).await;
        assert_eq!(crlf.output, "1\tone\n2\ttwo\n[lines 1-2 of 2]");
        let empty = read(temp.path(), serde_json::json!({"path": "empty.txt"})).await;
        assert!(empty.success);
        assert_eq!(empty.output, "[file is empty: 0 lines]");
        let latin = read(temp.path(), serde_json::json!({"path": "latin.txt"})).await;
        assert_eq!(latin.output, "1\tcaf\u{fffd}\n[lines 1-1 of 1]");
    }

    #[tokio::test]
    async fn read_file_refuses_binary_directories_escapes_and_bad_arguments() {
        let temp = disk_backed_working_dir("session-harness-file-refusals-");
        std::fs::write(temp.path().join("blob.bin"), b"\x7fELF\0\0binary").unwrap();
        std::fs::create_dir(temp.path().join("dir")).unwrap();
        std::fs::write(temp.path().join("ok.txt"), "x\n").unwrap();

        for (args, code) in [
            (serde_json::json!({"path": "blob.bin"}), "binary_file:"),
            (serde_json::json!({"path": "dir"}), "not_a_file:"),
            (
                serde_json::json!({"path": "../../etc/passwd"}),
                "Path error:",
            ),
            (serde_json::json!({"path": "/etc/passwd"}), "Path error:"),
            (serde_json::json!({}), "invalid_arguments:"),
            (
                serde_json::json!({"path": "ok.txt", "offset": 0}),
                "invalid_arguments:",
            ),
            (
                serde_json::json!({"path": "ok.txt", "limit": 0}),
                "invalid_arguments:",
            ),
            (
                serde_json::json!({"path": "ok.txt", "offset": -3}),
                "invalid_arguments:",
            ),
            (
                serde_json::json!({"path": "missing.txt"}),
                "Error reading file:",
            ),
        ] {
            let result = read(temp.path(), args.clone()).await;
            assert!(!result.success, "{args}");
            assert!(result.output.is_empty(), "{args}");
            let error = result.error_msg.unwrap();
            assert!(error.starts_with(code), "{args}: {error}");
        }
    }

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
        assert_eq!(
            read_result.output,
            "1\thello from write_file tool\n[lines 1-1 of 1]"
        );

        // Cleanup
        let _ = tokio::fs::remove_file(wd.join(filename)).await;
    }

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

    async fn edit(dir: &Path, args: serde_json::Value) -> ToolResult {
        EditFileTool.execute(args, dir).await
    }

    fn read_back(dir: &Path, name: &str) -> String {
        std::fs::read_to_string(dir.join(name)).unwrap()
    }

    #[tokio::test]
    async fn file_edit_applies_disjoint_edits_together() {
        let temp = disk_backed_working_dir("session-harness-file-edit-multi-");
        std::fs::write(
            temp.path().join("m.txt"),
            "alpha\nbeta\ngamma\ndelta\nepsilon\n",
        )
        .unwrap();

        let result = edit(
            temp.path(),
            serde_json::json!({
                "path": "m.txt",
                "edits": [
                    {"old_string": "epsilon", "new_string": "EPSILON"},
                    {"old_string": "beta", "new_string": "BETA"},
                ],
            }),
        )
        .await;
        assert!(result.success, "{:?}", result.error_msg);
        assert!(
            result.output.starts_with("Edited m.txt (2 edits)\n"),
            "{}",
            result.output
        );
        assert_eq!(
            read_back(temp.path(), "m.txt"),
            "alpha\nBETA\ngamma\ndelta\nEPSILON\n"
        );
    }

    #[tokio::test]
    async fn file_edit_matches_every_edit_against_the_original_and_allows_adjacent_edits() {
        let temp = disk_backed_working_dir("session-harness-file-edit-original-");
        std::fs::write(temp.path().join("o.txt"), "ab cd\n").unwrap();

        // The second edit's text only exists after the first edit, so it must
        // not match; the whole call is refused.
        let dependent = edit(
            temp.path(),
            serde_json::json!({
                "path": "o.txt",
                "edits": [
                    {"old_string": "ab", "new_string": "xy"},
                    {"old_string": "xy", "new_string": "zz"},
                ],
            }),
        )
        .await;
        assert!(!dependent.success);
        assert!(
            dependent
                .error_msg
                .unwrap()
                .starts_with("edits[1]: old_string not found in file")
        );
        assert_eq!(read_back(temp.path(), "o.txt"), "ab cd\n");

        // Touching but not overlapping spans are fine.
        let adjacent = edit(
            temp.path(),
            serde_json::json!({
                "path": "o.txt",
                "edits": [
                    {"old_string": "ab", "new_string": "AB"},
                    {"old_string": " cd", "new_string": "-CD"},
                ],
            }),
        )
        .await;
        assert!(adjacent.success, "{:?}", adjacent.error_msg);
        assert_eq!(read_back(temp.path(), "o.txt"), "AB-CD\n");
    }

    #[tokio::test]
    async fn file_edit_refuses_overlapping_edits_without_writing() {
        let temp = disk_backed_working_dir("session-harness-file-edit-overlap-");
        std::fs::write(temp.path().join("v.txt"), "foo bar baz\n").unwrap();

        for edits in [
            // Partial overlap.
            serde_json::json!([
                {"old_string": "foo bar", "new_string": "1"},
                {"old_string": "bar baz", "new_string": "2"},
            ]),
            // One edit contains the other.
            serde_json::json!([
                {"old_string": "foo bar baz", "new_string": "1"},
                {"old_string": "bar", "new_string": "2"},
            ]),
            // The same edit twice.
            serde_json::json!([
                {"old_string": "foo", "new_string": "1"},
                {"old_string": "foo", "new_string": "2"},
            ]),
        ] {
            let result = edit(
                temp.path(),
                serde_json::json!({"path": "v.txt", "edits": edits}),
            )
            .await;
            assert!(!result.success, "{edits}");
            let error = result.error_msg.unwrap();
            assert!(error.starts_with("overlapping_edits:"), "{error}");
            assert!(error.contains("edits[0] and edits[1]"), "{error}");
            assert!(error.contains("No edit was applied"), "{error}");
            assert_eq!(read_back(temp.path(), "v.txt"), "foo bar baz\n", "{edits}");
        }
    }

    #[tokio::test]
    async fn file_edit_one_failing_edit_refuses_all_edits() {
        let temp = disk_backed_working_dir("session-harness-file-edit-atomic-");
        let original = "one\ntwo\ntwo\nthree\n";
        std::fs::write(temp.path().join("a.txt"), original).unwrap();

        let missing = edit(
            temp.path(),
            serde_json::json!({
                "path": "a.txt",
                "edits": [
                    {"old_string": "one", "new_string": "1"},
                    {"old_string": "four", "new_string": "4"},
                    {"old_string": "three", "new_string": "3"},
                ],
            }),
        )
        .await;
        assert!(!missing.success);
        assert_eq!(
            missing.error_msg.unwrap(),
            "edits[1]: old_string not found in file"
        );
        assert_eq!(read_back(temp.path(), "a.txt"), original);

        let ambiguous = edit(
            temp.path(),
            serde_json::json!({
                "path": "a.txt",
                "edits": [
                    {"old_string": "one", "new_string": "1"},
                    {"old_string": "two", "new_string": "2"},
                ],
            }),
        )
        .await;
        assert!(!ambiguous.success);
        let error = ambiguous.error_msg.unwrap();
        assert!(
            error.starts_with("edits[1]: old_string found 2 times; must be unique"),
            "{error}"
        );
        assert!(error.contains("replace_all"), "{error}");
        assert_eq!(read_back(temp.path(), "a.txt"), original);
    }

    #[tokio::test]
    async fn file_edit_fuzzy_fallback_still_works_for_one_edit_among_exact_ones() {
        let temp = disk_backed_working_dir("session-harness-file-edit-multi-fuzzy-");
        // The file holds curly quotes and a non-breaking space; the second edit
        // supplies the ASCII equivalent, the first and third are exact.
        std::fs::write(
            temp.path().join("f.txt"),
            "let a = 1;\nlet b = \u{201c}hello\u{00a0}world\u{201d};\nlet c = 3;\n",
        )
        .unwrap();

        let result = edit(
            temp.path(),
            serde_json::json!({
                "path": "f.txt",
                "edits": [
                    {"old_string": "let a = 1;", "new_string": "let a = 10;"},
                    {"old_string": "let b = \"hello world\";", "new_string": "let b = \"bye\";"},
                    {"old_string": "let c = 3;", "new_string": "let c = 30;"},
                ],
            }),
        )
        .await;
        assert!(result.success, "{:?}", result.error_msg);
        assert!(
            result
                .output
                .starts_with("Edited f.txt (3 edits, fuzzy match)\n"),
            "{}",
            result.output
        );
        assert_eq!(
            read_back(temp.path(), "f.txt"),
            "let a = 10;\nlet b = \"bye\";\nlet c = 30;\n"
        );
    }

    #[tokio::test]
    async fn file_edit_returns_a_unified_diff_excerpt() {
        let temp = disk_backed_working_dir("session-harness-file-edit-diff-");
        std::fs::write(
            temp.path().join("d.txt"),
            "alpha\nbeta\ngamma\ndelta\nepsilon\n",
        )
        .unwrap();

        let result = edit(
            temp.path(),
            serde_json::json!({
                "path": "d.txt",
                "old_string": "beta",
                "new_string": "BETA",
            }),
        )
        .await;
        assert!(result.success, "{:?}", result.error_msg);
        assert!(
            result.output.starts_with("Edited d.txt\n"),
            "{}",
            result.output
        );
        for expected in [
            "--- a/d.txt\n",
            "+++ b/d.txt\n",
            "@@ -1,5 +1,5 @@\n",
            " alpha\n-beta\n+BETA\n gamma\n",
        ] {
            assert!(
                result.output.contains(expected),
                "{expected:?} in {}",
                result.output
            );
        }

        // Edits far apart give separate hunks and keep the excerpt short.
        let lines: String = (1..=100).map(|n| format!("row {n}\n")).collect();
        std::fs::write(temp.path().join("far.txt"), &lines).unwrap();
        let far = edit(
            temp.path(),
            serde_json::json!({
                "path": "far.txt",
                "edits": [
                    {"old_string": "row 10\n", "new_string": "ten\n"},
                    {"old_string": "row 90\n", "new_string": "ninety\n"},
                ],
            }),
        )
        .await;
        assert!(far.success, "{:?}", far.error_msg);
        assert_eq!(far.output.matches("\n@@ ").count(), 2, "{}", far.output);
        assert!(far.output.contains("\n-row 10\n+ten\n"), "{}", far.output);
        assert!(
            far.output.contains("\n-row 90\n+ninety\n"),
            "{}",
            far.output
        );
        assert!(!far.output.contains("row 50"), "{}", far.output);
    }

    #[tokio::test]
    async fn file_edit_diff_excerpt_is_capped_and_says_what_it_left_out() {
        let temp = disk_backed_working_dir("session-harness-file-edit-diff-cap-");
        let lines: String = (1..=400).map(|n| format!("item {n}\n")).collect();
        std::fs::write(temp.path().join("big.txt"), &lines).unwrap();

        let result = edit(
            temp.path(),
            serde_json::json!({
                "path": "big.txt",
                "old_string": "item",
                "new_string": "ITEM",
                "replace_all": true,
            }),
        )
        .await;
        assert!(result.success, "{:?}", result.error_msg);
        assert!(
            result
                .output
                .starts_with("Edited big.txt (400 replacements)\n"),
            "{}",
            result.output
        );
        assert!(
            result.output.contains("[diff truncated: "),
            "{}",
            result.output
        );
        assert!(
            result.output.len() < MAX_DIFF_BYTES + 512,
            "{}",
            result.output.len()
        );
        // The write itself is complete: only the excerpt is cut.
        let written = read_back(temp.path(), "big.txt");
        assert_eq!(written.matches("ITEM").count(), 400);
        assert!(!written.contains("item"));
    }

    #[tokio::test]
    async fn file_edit_replace_all_replaces_every_occurrence() {
        let temp = disk_backed_working_dir("session-harness-file-edit-replace-all-");
        std::fs::write(temp.path().join("r.txt"), "foo 1\nfoo 2\nbar\nfoo 3\n").unwrap();

        // Without replace_all a repeated old_string is refused (the original contract).
        let refused = edit(
            temp.path(),
            serde_json::json!({"path": "r.txt", "old_string": "foo", "new_string": "baz"}),
        )
        .await;
        assert!(!refused.success);
        assert!(refused.error_msg.unwrap().contains("found 3 times"));

        // Shorthand form with replace_all.
        let all = edit(
            temp.path(),
            serde_json::json!({
                "path": "r.txt", "old_string": "foo", "new_string": "baz", "replace_all": true,
            }),
        )
        .await;
        assert!(all.success, "{:?}", all.error_msg);
        assert!(
            all.output.starts_with("Edited r.txt (3 replacements)\n"),
            "{}",
            all.output
        );
        assert_eq!(
            read_back(temp.path(), "r.txt"),
            "baz 1\nbaz 2\nbar\nbaz 3\n"
        );

        // Per-edit replace_all next to an exact single edit, and a top-level
        // replace_all as the default for every entry.
        let mixed = edit(
            temp.path(),
            serde_json::json!({
                "path": "r.txt",
                "edits": [
                    {"old_string": "baz", "new_string": "qux", "replace_all": true},
                    {"old_string": "bar", "new_string": "BAR"},
                ],
            }),
        )
        .await;
        assert!(mixed.success, "{:?}", mixed.error_msg);
        assert!(
            mixed
                .output
                .starts_with("Edited r.txt (2 edits, 4 replacements)\n"),
            "{}",
            mixed.output
        );
        assert_eq!(
            read_back(temp.path(), "r.txt"),
            "qux 1\nqux 2\nBAR\nqux 3\n"
        );

        let defaulted = edit(
            temp.path(),
            serde_json::json!({
                "path": "r.txt", "replace_all": true,
                "edits": [{"old_string": "qux", "new_string": "q"}],
            }),
        )
        .await;
        assert!(defaulted.success, "{:?}", defaulted.error_msg);
        assert_eq!(read_back(temp.path(), "r.txt"), "q 1\nq 2\nBAR\nq 3\n");
    }

    #[tokio::test]
    async fn file_edit_accepts_a_json_string_edits_argument_and_the_shorthand_beside_it() {
        let temp = disk_backed_working_dir("session-harness-file-edit-shapes-");
        std::fs::write(temp.path().join("s.txt"), "one\ntwo\nthree\n").unwrap();

        let encoded = edit(
            temp.path(),
            serde_json::json!({
                "path": "s.txt",
                "edits": r#"[{"old_string":"one","new_string":"1"}]"#,
                "old_string": "three",
                "new_string": "3",
            }),
        )
        .await;
        assert!(encoded.success, "{:?}", encoded.error_msg);
        assert!(encoded.output.starts_with("Edited s.txt (2 edits)\n"));
        assert_eq!(read_back(temp.path(), "s.txt"), "1\ntwo\n3\n");
    }

    #[tokio::test]
    async fn file_edit_refuses_bad_arguments_and_identical_content_without_writing() {
        let temp = disk_backed_working_dir("session-harness-file-edit-bad-args-");
        std::fs::write(temp.path().join("b.txt"), "keep\n").unwrap();

        for (args, expected) in [
            (
                serde_json::json!({"edits": [{"old_string": "keep", "new_string": "x"}]}),
                "invalid_arguments: path is required",
            ),
            (
                serde_json::json!({"path": "b.txt"}),
                "invalid_arguments: provide edits",
            ),
            (
                serde_json::json!({"path": "b.txt", "edits": []}),
                "invalid_arguments: provide edits",
            ),
            (
                serde_json::json!({"path": "b.txt", "edits": "not json"}),
                "invalid_arguments: edits must be an array",
            ),
            (
                serde_json::json!({"path": "b.txt", "edits": {"old_string": "keep"}}),
                "invalid_arguments: edits must be an array",
            ),
            (
                serde_json::json!({"path": "b.txt", "old_string": "keep"}),
                "invalid_arguments: new_string is required",
            ),
            (
                serde_json::json!({"path": "b.txt", "new_string": "x"}),
                "invalid_arguments: old_string is required",
            ),
            (
                serde_json::json!({"path": "b.txt", "edits": [
                    {"old_string": "keep", "new_string": "x"},
                    {"old_string": "", "new_string": "y"},
                ]}),
                "edits[1]: old_string must not be empty",
            ),
            (
                serde_json::json!({"path": "b.txt", "edits": [
                    {"old_string": "keep", "new_string": "x"},
                    {"old_string": "keep"},
                ]}),
                "invalid_arguments: edits[1]: new_string is required",
            ),
            (
                serde_json::json!({
                    "path": "b.txt", "old_string": "keep", "new_string": "x", "replace_all": "yes",
                }),
                "invalid_arguments: replace_all must be a boolean",
            ),
            (
                serde_json::json!({"path": "b.txt", "old_string": "keep", "new_string": "keep"}),
                "no changes made to b.txt",
            ),
            (
                serde_json::json!({"path": "../../etc/passwd", "old_string": "a", "new_string": "b"}),
                "Path error:",
            ),
        ] {
            let result = edit(temp.path(), args.clone()).await;
            assert!(!result.success, "{args}");
            assert!(result.output.is_empty(), "{args}");
            let error = result.error_msg.unwrap();
            assert!(error.starts_with(expected), "{args}: {error}");
        }
        assert_eq!(read_back(temp.path(), "b.txt"), "keep\n");
    }

    #[tokio::test]
    async fn file_edit_schema_coerces_a_stringly_replace_all_inside_edits() {
        let temp = disk_backed_working_dir("session-harness-file-edit-schema-");
        std::fs::write(temp.path().join("c.txt"), "x x\n").unwrap();

        let schema: serde_json::Value = serde_json::from_str(EditFileTool.parameters_json())
            .expect("file_edit advertises valid JSON schema");
        // Only the path is top-level required: both the edits form and the
        // shorthand are valid calls.
        assert_eq!(schema["required"], serde_json::json!(["path"]));

        let mut args = serde_json::json!({
            "path": "c.txt",
            "edits": [{"old_string": "x", "new_string": "y", "replace_all": "true"}],
        });
        crate::arguments::coerce_with_schema(&mut args, &schema);
        assert_eq!(args["edits"][0]["replace_all"], serde_json::json!(true));

        let result = edit(temp.path(), args).await;
        assert!(result.success, "{:?}", result.error_msg);
        assert_eq!(read_back(temp.path(), "c.txt"), "y y\n");
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

    fn read_bytes(dir: &Path, name: &str) -> Vec<u8> {
        std::fs::read(dir.join(name)).unwrap()
    }

    #[tokio::test]
    async fn file_edit_fuzzy_path_keeps_crlf_and_every_untouched_line_byte_identical() {
        let temp = disk_backed_working_dir("session-harness-file-edit-fuzzy-crlf-");
        std::fs::write(
            temp.path().join("w.txt"),
            "first line  \r\nlet b = \u{201c}hi\u{201d};\r\nlet c = 3;\r\nlast\u{3000}\r\n",
        )
        .unwrap();

        // The second edit has no exact match (curly quotes), so both go through
        // the fuzzy path; the first grows the text ahead of the second.
        let result = edit(
            temp.path(),
            serde_json::json!({
                "path": "w.txt",
                "edits": [
                    {"old_string": "first line", "new_string": "first line changed\nextra"},
                    {
                        "old_string": "let b = \"hi\";\nlet c = 3;",
                        "new_string": "let b = \"bye\";\nlet c = 4;\nlet d = 5;"
                    },
                ],
            }),
        )
        .await;
        assert!(result.success, "{:?}", result.error_msg);
        assert!(
            result
                .output
                .starts_with("Edited w.txt (2 edits, fuzzy match)\n"),
            "{}",
            result.output
        );
        let written = read_back(temp.path(), "w.txt");
        assert_eq!(
            written,
            "first line changed\r\nextra  \r\nlet b = \"bye\";\r\nlet c = 4;\r\nlet d = 5;\r\n\
             last\u{3000}\r\n"
        );
        assert!(
            written
                .split_inclusive('\n')
                .all(|line| line.ends_with("\r\n")),
            "{written:?}"
        );
    }

    #[tokio::test]
    async fn file_edit_exact_path_keeps_crlf() {
        let temp = disk_backed_working_dir("session-harness-file-edit-exact-crlf-");
        std::fs::write(temp.path().join("c.txt"), "one\r\ntwo\r\nthree\r\n").unwrap();

        // An LF old_string matches the CRLF lines exactly; the LF new_string is
        // written with CRLF.
        let result = edit(
            temp.path(),
            serde_json::json!({
                "path": "c.txt",
                "edits": [
                    {"old_string": "one\ntwo\n", "new_string": "uno\ndos\ntres\n"},
                    {"old_string": "three", "new_string": "3"},
                ],
            }),
        )
        .await;
        assert!(result.success, "{:?}", result.error_msg);
        assert!(
            result.output.starts_with("Edited c.txt (2 edits)\n"),
            "{}",
            result.output
        );
        assert_eq!(
            read_bytes(temp.path(), "c.txt"),
            b"uno\r\ndos\r\ntres\r\n3\r\n".to_vec()
        );
    }

    #[tokio::test]
    async fn file_edit_keeps_a_byte_order_mark_on_the_exact_and_fuzzy_paths() {
        let temp = disk_backed_working_dir("session-harness-file-edit-bom-");
        std::fs::write(temp.path().join("e.txt"), "\u{feff}alpha\nbeta\n").unwrap();
        let exact = edit(
            temp.path(),
            serde_json::json!({"path": "e.txt", "old_string": "alpha", "new_string": "ALPHA"}),
        )
        .await;
        assert!(exact.success, "{:?}", exact.error_msg);
        assert_eq!(
            read_bytes(temp.path(), "e.txt"),
            "\u{feff}ALPHA\nbeta\n".as_bytes().to_vec()
        );

        std::fs::write(
            temp.path().join("f.txt"),
            "\u{feff}say \u{201c}hi\u{201d}\r\nnext\r\n",
        )
        .unwrap();
        let fuzzy = edit(
            temp.path(),
            serde_json::json!({
                "path": "f.txt",
                "old_string": "say \"hi\"",
                "new_string": "say \"bye\""
            }),
        )
        .await;
        assert!(fuzzy.success, "{:?}", fuzzy.error_msg);
        assert!(fuzzy.output.contains("fuzzy match"), "{}", fuzzy.output);
        assert_eq!(
            read_bytes(temp.path(), "f.txt"),
            "\u{feff}say \"bye\"\r\nnext\r\n".as_bytes().to_vec()
        );
    }

    #[tokio::test]
    async fn file_edit_fuzzy_path_keeps_compatibility_characters_outside_the_span() {
        let temp = disk_backed_working_dir("session-harness-file-edit-fuzzy-nfkc-");
        let original =
            "\u{fb01}le = 1;\nkey = \u{201c}v\u{201d}\n\u{ff21}\u{ff22}\u{ff23} x\u{b2}  \n";
        std::fs::write(temp.path().join("g.txt"), original).unwrap();

        let result = edit(
            temp.path(),
            serde_json::json!({
                "path": "g.txt",
                "old_string": "key = \"v\"",
                "new_string": "key = \"w\""
            }),
        )
        .await;
        assert!(result.success, "{:?}", result.error_msg);
        assert!(result.output.contains("fuzzy match"), "{}", result.output);
        assert_eq!(
            read_back(temp.path(), "g.txt"),
            "\u{fb01}le = 1;\nkey = \"w\"\n\u{ff21}\u{ff22}\u{ff23} x\u{b2}  \n"
        );
    }

    #[tokio::test]
    async fn file_edit_fuzzy_match_inside_a_ligature_or_composed_line_widens_to_that_unit() {
        let temp = disk_backed_working_dir("session-harness-file-edit-fuzzy-unit-");
        // The match starts inside the "ffi" ligature: the ligature is replaced
        // by its normalized text, the trailing ligature stays.
        std::fs::write(
            temp.path().join("h.txt"),
            "e\u{fb03}cient \u{2014} fast\n\u{fb01}n\n",
        )
        .unwrap();
        let ligature = edit(
            temp.path(),
            serde_json::json!({
                "path": "h.txt",
                "old_string": "ficient - fast",
                "new_string": "ficient, fast"
            }),
        )
        .await;
        assert!(ligature.success, "{:?}", ligature.error_msg);
        assert_eq!(
            read_back(temp.path(), "h.txt"),
            "efficient, fast\n\u{fb01}n\n"
        );

        // NFKC composes "e" + U+0301, so that line maps as one unit; only that
        // line takes its normalized form.
        std::fs::write(
            temp.path().join("i.txt"),
            "cafe\u{301} \u{201c}x\u{201d}\nnext\u{3000}\n",
        )
        .unwrap();
        let composed = edit(
            temp.path(),
            serde_json::json!({
                "path": "i.txt",
                "old_string": "\"x\"",
                "new_string": "\"y\""
            }),
        )
        .await;
        assert!(composed.success, "{:?}", composed.error_msg);
        assert_eq!(
            read_back(temp.path(), "i.txt"),
            "caf\u{e9} \"y\"\nnext\u{3000}\n"
        );
    }

    #[test]
    fn fuzzy_text_matches_the_needle_normalization() {
        for text in [
            "a \u{201c}b\u{201d}  \r\n\u{fb01} \u{2014}\u{a0}\n\ncafe\u{301}\u{3000}",
            "\r\n\r\n",
            "",
        ] {
            assert_eq!(FuzzyText::new(text).text, normalize_for_fuzzy_match(text));
        }
    }
}
