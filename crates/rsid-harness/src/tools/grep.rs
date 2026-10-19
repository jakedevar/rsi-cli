//! Native `grep`: regex content search over the working tree (#2254).
//!
//! A portable replacement for shelling out to `rg`: `ignore::WalkBuilder`
//! supplies the gitignore-aware walk and `regex` the matching, so the tool
//! needs no external binary and runs under the same sandbox, system-path and
//! cancellation rules as the other file tools. Matching is per line; a pattern
//! never spans lines.

use super::navigation::{
    Budget, FileGlob, SearchScope, StopReason, clip_at_char_boundary, failure, optional_uint,
    run_blocking, walker,
};
use super::{HarnessTool, ToolContext, ToolExecutionMode};
use crate::types::ToolResult;
use regex::bytes::{Regex, RegexBuilder};
use std::io::Read as _;
use std::path::Path;
use tokio_util::sync::CancellationToken;

const DEFAULT_HEAD_LIMIT: usize = 250;
const MAX_HEAD_LIMIT: usize = 2000;
const MAX_CONTEXT_LINES: usize = 10;
/// Files above this size are skipped, not searched.
const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;
/// A NUL byte in this prefix marks a file as binary.
const BINARY_SNIFF_BYTES: u64 = 8192;
/// Hard cap on the rendered result, whatever the session's tool policy allows.
const MAX_OUTPUT_BYTES: usize = 100_000;
/// A matching or context line longer than this is clipped (minified files).
const MAX_LINE_BYTES: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputMode {
    FilesWithMatches,
    Content,
    Count,
}

impl OutputMode {
    fn parse(value: Option<&str>) -> Result<Self, String> {
        match value {
            None | Some("files_with_matches") => Ok(Self::FilesWithMatches),
            Some("content") => Ok(Self::Content),
            Some("count") => Ok(Self::Count),
            Some(other) => Err(format!(
                "invalid_arguments: output_mode '{other}' is not one of \
                 files_with_matches, content, count"
            )),
        }
    }
}

/// Validated arguments; the regex compiles on the blocking thread.
struct GrepRequest {
    pattern: String,
    case_insensitive: bool,
    mode: OutputMode,
    context: usize,
    head_limit: usize,
    glob: Option<FileGlob>,
    scope: SearchScope,
    max_output_bytes: usize,
}

impl GrepRequest {
    fn parse(
        args: &serde_json::Value,
        working_dir: &Path,
        max_output_bytes: usize,
    ) -> Result<Self, ToolResult> {
        let pattern = args
            .get("pattern")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        if pattern.is_empty() {
            return Err(failure("invalid_arguments: pattern is required"));
        }
        let mode = OutputMode::parse(args.get("output_mode").and_then(serde_json::Value::as_str))
            .map_err(failure)?;
        let context = optional_uint(args, "context")
            .map_err(failure)?
            .unwrap_or(0)
            .min(MAX_CONTEXT_LINES);
        let head_limit = match optional_uint(args, "head_limit").map_err(failure)? {
            None => DEFAULT_HEAD_LIMIT,
            Some(0) => return Err(failure("invalid_arguments: head_limit must be at least 1")),
            Some(limit) => limit.min(MAX_HEAD_LIMIT),
        };
        let glob = match args.get("glob").and_then(serde_json::Value::as_str) {
            Some(glob) if !glob.trim().is_empty() => Some(
                FileGlob::new(glob).map_err(|error| failure(format!("invalid_glob: {error}")))?,
            ),
            _ => None,
        };
        let path_arg = args
            .get("path")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        Ok(Self {
            pattern: pattern.to_string(),
            case_insensitive: args
                .get("case_insensitive")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            mode,
            context,
            head_limit,
            glob,
            scope: SearchScope::resolve(working_dir, path_arg)?,
            max_output_bytes: max_output_bytes.min(MAX_OUTPUT_BYTES),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Truncation {
    HeadLimit,
    OutputBytes,
}

#[derive(Default)]
struct Report {
    out: String,
    /// Files (`files_with_matches`, `count`) or matching lines (`content`).
    results: usize,
    truncated: Option<Truncation>,
    stop: Option<StopReason>,
    files_searched: usize,
    skipped_binary: usize,
    skipped_large: usize,
    unreadable: usize,
}

impl Report {
    /// Append one rendered line unless it would pass the output cap.
    fn push_line(&mut self, line: &str, max_output_bytes: usize) -> bool {
        if self.out.len() + line.len() + 1 > max_output_bytes {
            self.truncated = Some(Truncation::OutputBytes);
            return false;
        }
        self.out.push_str(line);
        self.out.push('\n');
        true
    }

    fn note_head_limit(&mut self) -> bool {
        self.truncated = Some(Truncation::HeadLimit);
        false
    }
}

enum Skip {
    Binary,
    Large,
    Unreadable,
}

fn read_searchable(path: &Path) -> Result<Vec<u8>, Skip> {
    let mut file = std::fs::File::open(path).map_err(|_| Skip::Unreadable)?;
    let length = file.metadata().map_err(|_| Skip::Unreadable)?.len();
    if length > MAX_FILE_BYTES {
        return Err(Skip::Large);
    }
    let mut bytes = Vec::with_capacity(usize::try_from(length).unwrap_or(0));
    (&mut file)
        .take(BINARY_SNIFF_BYTES)
        .read_to_end(&mut bytes)
        .map_err(|_| Skip::Unreadable)?;
    if bytes.contains(&0) {
        return Err(Skip::Binary);
    }
    file.read_to_end(&mut bytes).map_err(|_| Skip::Unreadable)?;
    Ok(bytes)
}

/// The lines of `bytes` without their `\n` terminators; a final terminator
/// does not open an extra empty line.
fn lines(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    let body = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    let empty = bytes.is_empty();
    body.split(|byte| *byte == b'\n').filter(move |_| !empty)
}

fn render_line(line: &[u8]) -> String {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let text = String::from_utf8_lossy(line);
    let (kept, dropped) = clip_at_char_boundary(&text, MAX_LINE_BYTES);
    if dropped == 0 {
        kept.to_string()
    } else {
        format!("{kept} ... [+{dropped} bytes]")
    }
}

/// Search one file's lines. Returns `false` once the whole search must stop.
fn search_file(
    display: &str,
    bytes: &[u8],
    regex: &Regex,
    request: &GrepRequest,
    report: &mut Report,
) -> bool {
    let cap = request.max_output_bytes;
    match request.mode {
        OutputMode::FilesWithMatches => {
            if !lines(bytes).any(|line| regex.is_match(line)) {
                return true;
            }
            if report.results == request.head_limit {
                return report.note_head_limit();
            }
            report.results += 1;
            report.push_line(display, cap)
        }
        OutputMode::Count => {
            let count = lines(bytes).filter(|line| regex.is_match(line)).count();
            if count == 0 {
                return true;
            }
            if report.results == request.head_limit {
                return report.note_head_limit();
            }
            report.results += 1;
            report.push_line(&format!("{display}:{count}"), cap)
        }
        OutputMode::Content => search_file_content(display, bytes, regex, request, report),
    }
}

fn search_file_content(
    display: &str,
    bytes: &[u8],
    regex: &Regex,
    request: &GrepRequest,
    report: &mut Report,
) -> bool {
    let cap = request.max_output_bytes;
    let all: Vec<&[u8]> = lines(bytes).collect();
    let matched: Vec<bool> = all.iter().map(|line| regex.is_match(line)).collect();
    if !matched.contains(&true) {
        return true;
    }
    // Merge each match's context window into non-overlapping groups.
    let mut groups: Vec<(usize, usize)> = Vec::new();
    for (index, _) in matched.iter().enumerate().filter(|(_, hit)| **hit) {
        let low = index.saturating_sub(request.context);
        let high = (index + request.context).min(all.len() - 1);
        match groups.last_mut() {
            Some(last) if low <= last.1 + 1 => last.1 = high.max(last.1),
            _ => groups.push((low, high)),
        }
    }
    for (low, high) in groups {
        if request.context > 0 && !report.out.is_empty() && !report.push_line("--", cap) {
            return false;
        }
        for index in low..=high {
            let number = index + 1;
            if matched[index] {
                if report.results == request.head_limit {
                    return report.note_head_limit();
                }
                report.results += 1;
                let line = format!("{display}:{number}:{}", render_line(all[index]));
                if !report.push_line(&line, cap) {
                    return false;
                }
            } else {
                let line = format!("{display}-{number}-{}", render_line(all[index]));
                if !report.push_line(&line, cap) {
                    return false;
                }
            }
        }
    }
    true
}

fn search(request: &GrepRequest, budget: &Budget) -> Result<Report, String> {
    let regex = RegexBuilder::new(&request.pattern)
        .case_insensitive(request.case_insensitive)
        .multi_line(true)
        .build()
        .map_err(|error| format!("invalid_pattern: {error}"))?;
    let scope = &request.scope;
    let root_is_file = scope.root.is_file();
    let mut report = Report::default();
    for entry in walker(&scope.root).build() {
        if let Some(reason) = budget.stop_reason() {
            report.stop = Some(reason);
            break;
        }
        let Ok(entry) = entry else {
            report.unreadable += 1;
            continue;
        };
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let path = entry.path();
        if let Some(glob) = &request.glob
            && !root_is_file
            && !glob.is_match(path.strip_prefix(&scope.root).unwrap_or(path))
        {
            continue;
        }
        let bytes = match read_searchable(path) {
            Ok(bytes) => bytes,
            Err(Skip::Binary) => {
                report.skipped_binary += 1;
                continue;
            }
            Err(Skip::Large) => {
                report.skipped_large += 1;
                continue;
            }
            Err(Skip::Unreadable) => {
                report.unreadable += 1;
                continue;
            }
        };
        report.files_searched += 1;
        // A line match is also a match of the whole text under `multi_line`,
        // so a miss here clears the file without splitting it into lines.
        if !regex.is_match(&bytes) {
            continue;
        }
        if !search_file(&scope.display(path), &bytes, &regex, request, &mut report) {
            break;
        }
    }
    Ok(report)
}

fn render(report: Report, request: &GrepRequest) -> String {
    let mut output = report.out;
    if output.is_empty() && report.truncated.is_none() {
        output.push_str("(no matches found)\n");
    }
    match report.truncated {
        Some(Truncation::HeadLimit) => output.push_str(&format!(
            "[truncated at head_limit={}: more results exist; narrow the pattern, path or glob, \
             or raise head_limit (max {MAX_HEAD_LIMIT})]\n",
            request.head_limit
        )),
        Some(Truncation::OutputBytes) => output.push_str(
            "[truncated: output size limit reached; narrow the pattern, path or glob, \
             or lower context]\n",
        ),
        None => {}
    }
    if report.stop == Some(StopReason::TimedOut) {
        output.push_str(&format!(
            "[stopped: search time limit reached after {} files; narrow path or glob]\n",
            report.files_searched
        ));
    }
    let mut skipped = Vec::new();
    if report.skipped_binary > 0 {
        skipped.push(format!("{} binary", report.skipped_binary));
    }
    if report.skipped_large > 0 {
        skipped.push(format!(
            "{} over {} MiB",
            report.skipped_large,
            MAX_FILE_BYTES / (1024 * 1024)
        ));
    }
    if report.unreadable > 0 {
        skipped.push(format!("{} unreadable", report.unreadable));
    }
    if !skipped.is_empty() {
        output.push_str(&format!("[skipped: {}]\n", skipped.join(", ")));
    }
    output.truncate(output.trim_end_matches('\n').len());
    output
}

pub struct GrepTool;

impl GrepTool {
    async fn run(
        &self,
        args: serde_json::Value,
        working_dir: &Path,
        max_output_bytes: usize,
        cancel: &CancellationToken,
    ) -> ToolResult {
        let request = match GrepRequest::parse(&args, working_dir, max_output_bytes) {
            Ok(request) => request,
            Err(refusal) => return refusal,
        };
        let outcome = run_blocking(cancel, move |budget| {
            search(&request, budget).map(|report| render(report, &request))
        })
        .await;
        match outcome {
            Ok(Ok(output)) => ToolResult {
                success: true,
                output,
                error_msg: None,
            },
            Ok(Err(message)) => failure(message),
            Err(failed) => failed,
        }
    }
}

#[async_trait::async_trait]
impl HarnessTool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::ParallelSafe
    }

    fn description(&self) -> &str {
        "Search file contents with a regular expression (Rust regex syntax, matched per line). \
         Honors .gitignore, includes hidden files, skips .git, binary files and files over \
         10 MiB. output_mode files_with_matches (default) lists matching paths, content prints \
         path:line:text with optional context lines (path-line-text), count prints path:count. \
         Results stop at head_limit (default 250) and the output says when it was truncated."
    }

    fn parameters_json(&self) -> &str {
        r#"{
  "type": "object",
  "required": ["pattern"],
  "properties": {
    "pattern": {
      "type": "string",
      "description": "Regular expression to search for."
    },
    "path": {
      "type": "string",
      "description": "File or directory to search, relative to the working directory. Defaults to the working directory."
    },
    "glob": {
      "type": "string",
      "description": "Only search files matching this glob: '*.rs' matches names at any depth, 'src/**/*.ts' matches paths from the search root."
    },
    "case_insensitive": {
      "type": "boolean",
      "description": "Ignore case when matching. Default false."
    },
    "output_mode": {
      "type": "string",
      "enum": ["files_with_matches", "content", "count"],
      "description": "files_with_matches (default), content or count."
    },
    "context": {
      "type": "integer",
      "minimum": 0,
      "description": "Lines of context before and after each match (content mode, at most 10)."
    },
    "head_limit": {
      "type": "integer",
      "minimum": 1,
      "description": "Maximum results: files, or matching lines in content mode. Default 250, at most 2000."
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::navigation::fixture_repo;
    use serde_json::json;

    async fn grep(dir: &Path, args: serde_json::Value) -> ToolResult {
        GrepTool.execute(args, dir).await
    }

    #[tokio::test]
    async fn files_with_matches_skips_ignored_binary_and_git_paths() {
        let repo = fixture_repo();
        let result = grep(repo.path(), json!({"pattern": "needle"})).await;
        assert!(result.success, "{:?}", result.error_msg);
        let paths: Vec<&str> = result
            .output
            .lines()
            .filter(|l| !l.starts_with('['))
            .collect();
        // Hidden directories are searched; `target/`, `*.log`, `.git/` and the
        // binary file are not.
        assert_eq!(paths, [".github/workflows/ci.yml", "src/main.rs"]);
        assert!(
            result.output.contains("[skipped: 1 binary]"),
            "{}",
            result.output
        );
    }

    #[tokio::test]
    async fn content_mode_prints_numbered_matches_with_context() {
        let repo = fixture_repo();
        let result = grep(
            repo.path(),
            json!({
                "pattern": "println",
                "path": "src",
                "output_mode": "content",
                "context": 1,
            }),
        )
        .await;
        assert!(result.success, "{:?}", result.error_msg);
        assert_eq!(
            result.output,
            "src/main.rs-1-fn main() {\n\
             src/main.rs:2:    println!(\"needle in main\");\n\
             src/main.rs-3-}"
        );
    }

    #[tokio::test]
    async fn count_mode_glob_and_case_filters() {
        let repo = fixture_repo();
        let result = grep(
            repo.path(),
            json!({
                "pattern": "NEEDLE",
                "case_insensitive": true,
                "output_mode": "count",
                "glob": "*.rs",
            }),
        )
        .await;
        assert!(result.success, "{:?}", result.error_msg);
        assert_eq!(result.output, "src/lib.rs:1\nsrc/main.rs:1");

        // Case-sensitive: only the uppercase occurrence in lib.rs.
        let sensitive = grep(
            repo.path(),
            json!({"pattern": "NEEDLE", "output_mode": "count"}),
        )
        .await;
        assert_eq!(sensitive.output, "src/lib.rs:1\n[skipped: 1 binary]");
    }

    #[tokio::test]
    async fn head_limit_reports_truncation() {
        let repo = fixture_repo();
        let result = grep(repo.path(), json!({"pattern": "needle", "head_limit": 1})).await;
        assert!(result.success, "{:?}", result.error_msg);
        let mut lines = result.output.lines();
        assert_eq!(lines.next(), Some(".github/workflows/ci.yml"));
        let note = lines.next().unwrap();
        assert!(note.starts_with("[truncated at head_limit=1"), "{note}");

        // Exactly head_limit results is not a truncation.
        let exact = grep(repo.path(), json!({"pattern": "needle", "head_limit": 2})).await;
        assert!(!exact.output.contains("truncated"), "{}", exact.output);

        // Content mode counts matching lines against the limit.
        let content = grep(
            repo.path(),
            json!({
                "pattern": "e",
                "path": "src",
                "output_mode": "content",
                "head_limit": 2,
            }),
        )
        .await;
        assert_eq!(
            content
                .output
                .lines()
                .filter(|l| !l.starts_with('['))
                .count(),
            2
        );
        assert!(content.output.contains("truncated at head_limit=2"));
    }

    #[tokio::test]
    async fn no_match_invalid_pattern_and_bad_arguments_are_distinct() {
        let repo = fixture_repo();
        let none = grep(repo.path(), json!({"pattern": "zzz_absent"})).await;
        assert!(none.success);
        assert_eq!(none.output, "(no matches found)\n[skipped: 1 binary]");

        for (args, code) in [
            (json!({"pattern": "("}), "invalid_pattern"),
            (json!({}), "invalid_arguments"),
            (
                json!({"pattern": "a", "output_mode": "lines"}),
                "invalid_arguments",
            ),
            (
                json!({"pattern": "a", "head_limit": 0}),
                "invalid_arguments",
            ),
            (json!({"pattern": "a", "glob": "["}), "invalid_glob"),
        ] {
            let result = grep(repo.path(), args.clone()).await;
            assert!(!result.success, "{args}");
            let error = result.error_msg.unwrap();
            assert!(error.starts_with(code), "{args}: {error}");
        }
    }

    #[tokio::test]
    async fn search_paths_stay_inside_the_working_directory() {
        let repo = fixture_repo();
        for path in ["../..", "/etc"] {
            let result = grep(repo.path(), json!({"pattern": "root", "path": path})).await;
            assert!(!result.success, "{path}");
            assert!(result.output.is_empty());
        }
        let missing = grep(repo.path(), json!({"pattern": "a", "path": "nope"})).await;
        assert!(!missing.success);

        // A single file is searched even when it is gitignored.
        let log = grep(
            repo.path(),
            json!({"pattern": "needle", "path": "build.log", "output_mode": "content"}),
        )
        .await;
        assert_eq!(log.output, "build.log:1:needle in log");
    }

    #[tokio::test]
    async fn long_lines_and_crlf_are_rendered_safely() {
        let repo = fixture_repo();
        let long = format!("{}needle{}\r\n", "é".repeat(400), "x".repeat(100));
        std::fs::write(repo.path().join("minified.js"), &long).unwrap();
        let result = grep(
            repo.path(),
            json!({
                "pattern": "needle",
                "path": "minified.js",
                "output_mode": "content",
            }),
        )
        .await;
        assert!(result.success, "{:?}", result.error_msg);
        assert!(
            result.output.starts_with("minified.js:1:é"),
            "{}",
            result.output
        );
        assert!(result.output.contains("... [+"), "{}", result.output);
        assert!(!result.output.contains('\r'));
    }

    #[tokio::test]
    async fn output_cap_truncates_at_a_line_boundary() {
        let repo = fixture_repo();
        let body: String = (0..200).map(|n| format!("needle {n}\n")).collect();
        std::fs::write(repo.path().join("many.txt"), body).unwrap();
        let context = ToolContext {
            session_id: None,
            working_dir: repo.path().to_path_buf(),
            cancel: CancellationToken::new(),
            event_sink: None,
            policy: super::super::ToolPolicy {
                max_output_bytes: 300,
                ..super::super::ToolPolicy::default()
            },
        };
        let result = GrepTool
            .execute_with_context(
                json!({"pattern": "needle", "path": "many.txt", "output_mode": "content"}),
                &context,
            )
            .await;
        assert!(result.success, "{:?}", result.error_msg);
        assert!(result.output.contains("output size limit reached"));
        let kept: Vec<&str> = result
            .output
            .lines()
            .filter(|l| l.starts_with("many.txt:"))
            .collect();
        assert!(!kept.is_empty() && kept.len() < 200);
        assert!(
            kept.iter().all(|line| line.contains(":needle ")),
            "{kept:?}"
        );
    }

    #[tokio::test]
    async fn cancellation_stops_the_walk() {
        let repo = fixture_repo();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = GrepTool
            .run(
                json!({"pattern": "needle"}),
                repo.path(),
                MAX_OUTPUT_BYTES,
                &cancel,
            )
            .await;
        // A cancelled search stops at the first entry: nothing was searched.
        assert!(result.success);
        assert_eq!(result.output, "(no matches found)");
    }
}
