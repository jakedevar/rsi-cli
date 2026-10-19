//! Native `glob`: find files by path pattern, newest first (#2254).
//!
//! Gitignore-aware like `grep` and `list_files` (see `navigation.rs`), so a pattern
//! such as `**/*.rs` never returns build output under an ignored `target/`.

use super::navigation::{
    Budget, SearchScope, StopReason, failure, optional_uint, run_blocking, walker,
};
use super::{HarnessTool, ToolContext, ToolExecutionMode};
use crate::types::ToolResult;
use std::path::Path;
use std::time::SystemTime;
use tokio_util::sync::CancellationToken;

const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 1000;
/// Hard cap on the rendered result, whatever the session's tool policy allows.
const MAX_OUTPUT_BYTES: usize = 100_000;
/// Matches kept for sorting; a broader pattern is reported as truncated.
const MAX_COLLECTED: usize = 50_000;

struct GlobRequest {
    pattern: String,
    matcher: globset::GlobMatcher,
    limit: usize,
    scope: SearchScope,
    max_output_bytes: usize,
}

impl GlobRequest {
    fn parse(
        args: &serde_json::Value,
        working_dir: &Path,
        max_output_bytes: usize,
    ) -> Result<Self, ToolResult> {
        let pattern = args
            .get("pattern")
            .and_then(serde_json::Value::as_str)
            .map(|pattern| pattern.trim().trim_start_matches("./"))
            .unwrap_or_default();
        if pattern.is_empty() {
            return Err(failure("invalid_arguments: pattern is required"));
        }
        let matcher = globset::GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
            .map_err(|error| failure(format!("invalid_glob: '{pattern}': {error}")))?
            .compile_matcher();
        let limit = match optional_uint(args, "limit").map_err(failure)? {
            None => DEFAULT_LIMIT,
            Some(0) => return Err(failure("invalid_arguments: limit must be at least 1")),
            Some(limit) => limit.min(MAX_LIMIT),
        };
        let path_arg = args
            .get("path")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let scope = SearchScope::resolve(working_dir, path_arg)?;
        if !scope.root.is_dir() {
            return Err(failure(format!(
                "invalid_arguments: path is not a directory: {path_arg}"
            )));
        }
        Ok(Self {
            pattern: pattern.to_string(),
            matcher,
            limit,
            scope,
            max_output_bytes: max_output_bytes.min(MAX_OUTPUT_BYTES),
        })
    }
}

#[derive(Default)]
struct Found {
    /// `(modified time, path shown to the model)`.
    matches: Vec<(SystemTime, String)>,
    capped: bool,
    stop: Option<StopReason>,
}

fn find(request: &GlobRequest, budget: &Budget) -> Found {
    let scope = &request.scope;
    let mut found = Found::default();
    for entry in walker(&scope.root).build() {
        if let Some(reason) = budget.stop_reason() {
            found.stop = Some(reason);
            break;
        }
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let path = entry.path();
        if !request
            .matcher
            .is_match(path.strip_prefix(&scope.root).unwrap_or(path))
        {
            continue;
        }
        if found.matches.len() >= MAX_COLLECTED {
            found.capped = true;
            break;
        }
        let modified = entry
            .metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        found.matches.push((modified, scope.display(path)));
    }
    // Newest first; equal times fall back to path order so output is stable.
    found
        .matches
        .sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    found
}

fn render(found: Found, request: &GlobRequest) -> String {
    let total = found.matches.len();
    let mut output = String::new();
    let mut shown = 0;
    let mut by_bytes = false;
    for (_, path) in found.matches.iter().take(request.limit) {
        if output.len() + path.len() + 1 > request.max_output_bytes {
            by_bytes = true;
            break;
        }
        output.push_str(path);
        output.push('\n');
        shown += 1;
    }
    let at_least = if found.capped { "at least " } else { "" };
    if total == 0 {
        output.push_str("(no files matched)");
        if !request.pattern.contains('/') {
            output.push_str(&format!(
                "\n[note: the pattern matches paths relative to the search directory, so \
                 '{}' only matches there; use '**/{}' to match at any depth]",
                request.pattern, request.pattern
            ));
        }
    } else if shown < total {
        let reason = if by_bytes {
            "output size limit reached"
        } else {
            "limit reached"
        };
        output.push_str(&format!(
            "[showing the newest {shown} of {at_least}{total} matches ({reason}); \
             narrow the pattern or path, or raise limit (max {MAX_LIMIT})]"
        ));
    } else if found.capped {
        output.push_str(&format!(
            "[stopped after {total} matches; narrow the pattern or path]"
        ));
    }
    if found.stop == Some(StopReason::TimedOut) {
        output.push_str("\n[stopped: search time limit reached; narrow the pattern or path]");
    }
    output.truncate(output.trim_end_matches('\n').len());
    output
}

pub struct GlobTool;

impl GlobTool {
    async fn run(
        &self,
        args: serde_json::Value,
        working_dir: &Path,
        max_output_bytes: usize,
        cancel: &CancellationToken,
    ) -> ToolResult {
        let request = match GlobRequest::parse(&args, working_dir, max_output_bytes) {
            Ok(request) => request,
            Err(refusal) => return refusal,
        };
        match run_blocking(cancel, move |budget| {
            render(find(&request, budget), &request)
        })
        .await
        {
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
impl HarnessTool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::ParallelSafe
    }

    fn description(&self) -> &str {
        "Find files whose path matches a glob pattern, newest modification first. The pattern \
         is matched against paths relative to the search directory: '*.rs' matches only there, \
         '**/*.rs' matches at any depth, 'src/**/*.ts' under src. Returns files only; honors \
         .gitignore, includes hidden files and skips .git."
    }

    fn parameters_json(&self) -> &str {
        r#"{
  "type": "object",
  "required": ["pattern"],
  "properties": {
    "pattern": {
      "type": "string",
      "description": "Glob pattern, e.g. '**/*.rs' or 'src/**/mod.rs'."
    },
    "path": {
      "type": "string",
      "description": "Directory to search, relative to the working directory. Defaults to the working directory."
    },
    "limit": {
      "type": "integer",
      "minimum": 1,
      "description": "Maximum files to return. Default 100, at most 1000."
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
    use std::time::Duration;

    async fn glob(dir: &Path, args: serde_json::Value) -> ToolResult {
        GlobTool.execute(args, dir).await
    }

    fn touch(path: &Path, modified: SystemTime) {
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(modified)
            .unwrap();
    }

    #[tokio::test]
    async fn matches_files_only_newest_first_and_never_ignored_paths() {
        let repo = fixture_repo();
        let now = SystemTime::now();
        touch(
            &repo.path().join("src/lib.rs"),
            now - Duration::from_secs(600),
        );
        touch(&repo.path().join("src/main.rs"), now);
        // The ignored file is the newest of all, yet must not appear.
        touch(
            &repo.path().join("target/debug/out.rs"),
            now + Duration::from_secs(60),
        );

        let result = glob(repo.path(), json!({"pattern": "**/*.rs"})).await;
        assert!(result.success, "{:?}", result.error_msg);
        assert_eq!(result.output, "src/main.rs\nsrc/lib.rs");

        // Reversing the modification times reverses the order.
        touch(
            &repo.path().join("src/lib.rs"),
            now + Duration::from_secs(5),
        );
        let flipped = glob(repo.path(), json!({"pattern": "**/*.rs"})).await;
        assert_eq!(flipped.output, "src/lib.rs\nsrc/main.rs");
    }

    #[tokio::test]
    async fn pattern_is_relative_to_path_and_includes_hidden_directories() {
        let repo = fixture_repo();
        let hidden = glob(repo.path(), json!({"pattern": "**/*.yml"})).await;
        assert_eq!(hidden.output, ".github/workflows/ci.yml");

        let scoped = glob(repo.path(), json!({"pattern": "*.rs", "path": "src"})).await;
        assert!(scoped.success, "{:?}", scoped.error_msg);
        let mut paths: Vec<&str> = scoped.output.lines().collect();
        paths.sort_unstable();
        assert_eq!(paths, ["src/lib.rs", "src/main.rs"]);

        let top_level = glob(repo.path(), json!({"pattern": "*.md"})).await;
        assert_eq!(top_level.output, "README.md");
    }

    #[tokio::test]
    async fn no_match_explains_depth_and_limit_reports_truncation() {
        let repo = fixture_repo();
        let none = glob(repo.path(), json!({"pattern": "*.rs"})).await;
        assert!(none.success);
        assert!(
            none.output.starts_with("(no files matched)"),
            "{}",
            none.output
        );
        assert!(none.output.contains("'**/*.rs'"), "{}", none.output);

        let limited = glob(repo.path(), json!({"pattern": "**/*.rs", "limit": 1})).await;
        let mut lines = limited.output.lines();
        assert!(lines.next().unwrap().ends_with(".rs"));
        let note = lines.next().unwrap();
        assert!(
            note.starts_with("[showing the newest 1 of 2 matches"),
            "{note}"
        );
    }

    #[tokio::test]
    async fn bad_arguments_and_escaping_paths_are_refused() {
        let repo = fixture_repo();
        for (args, code) in [
            (json!({}), "invalid_arguments"),
            (json!({"pattern": "**/*", "limit": 0}), "invalid_arguments"),
            (json!({"pattern": "["}), "invalid_glob"),
            (
                json!({"pattern": "*", "path": "README.md"}),
                "invalid_arguments",
            ),
            (json!({"pattern": "*", "path": "../.."}), "Invalid path"),
            (json!({"pattern": "*", "path": "/etc"}), "Invalid path"),
        ] {
            let result = glob(repo.path(), args.clone()).await;
            assert!(!result.success, "{args}");
            let error = result.error_msg.unwrap();
            assert!(error.starts_with(code), "{args}: {error}");
        }
    }
}
