//! Git diff parsing for file viewer gutter indicators.
//!
//! Runs `git diff HEAD -- <path>` synchronously (fast, <10ms for typical files)
//! and parses unified diff `@@` hunk headers to produce per-line `GitLineState` maps.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use crate::types::GitLineState;

/// Parse the output of `git diff --unified=0 HEAD -- <file>` and produce
/// a map from 1-based line number → `GitLineState` for the working copy.
///
/// Lines not mentioned in the diff are `Unchanged`.
pub fn parse_git_diff(diff_output: &str) -> HashMap<usize, GitLineState> {
    let mut result: HashMap<usize, GitLineState> = HashMap::new();

    let mut new_count: usize;
    let mut old_count: usize;
    let mut in_hunk = false;
    let mut new_line: usize = 0;
    // Track how many new lines have been added in the current hunk
    let mut added_in_hunk: usize = 0;
    let mut hunk_old_count: usize = 0;

    for line in diff_output.lines() {
        if line.starts_with("@@") {
            if let Some((old_part, new_part)) = parse_hunk_header(line) {
                old_count = old_part.1;
                new_count = new_part.1;
                in_hunk = true;
                new_line = new_part.0;
                added_in_hunk = 0;
                hunk_old_count = old_count;

                // Pure deletion: new_count == 0
                if new_count == 0 {
                    let new_start = new_part.0;
                    let marker_line = if new_start == 0 { 1 } else { new_start };
                    result.insert(marker_line, GitLineState::Deleted);
                    in_hunk = false;
                }
            }
            continue;
        }

        if !in_hunk {
            continue;
        }

        match line.chars().next() {
            Some('+') => {
                // If we've removed at least as many old lines as we've added new ones,
                // this new line is replacing an old line → Modified
                let state = if added_in_hunk < hunk_old_count {
                    GitLineState::Modified
                } else {
                    GitLineState::Added
                };
                added_in_hunk += 1;
                result.insert(new_line, state);
                new_line += 1;
            }
            Some('-') => {
                // Old line removed — consumed without advancing new_line
            }
            Some(' ') => {
                new_line += 1;
            }
            _ => {}
        }
    }

    result
}

/// Parse a `@@ -a,b +c,d @@` hunk header.
fn parse_hunk_header(line: &str) -> Option<((usize, usize), (usize, usize))> {
    let inner = line.strip_prefix("@@")?.trim_start();
    let end = inner.find("@@").unwrap_or(inner.len());
    let range_str = inner[..end].trim();

    let mut parts = range_str.split_whitespace();
    let old_str = parts.next()?.strip_prefix('-')?;
    let new_str = parts.next()?.strip_prefix('+')?;

    let parse_range = |s: &str| -> Option<(usize, usize)> {
        if let Some((start, count)) = s.split_once(',') {
            Some((start.parse().ok()?, count.parse().ok()?))
        } else {
            Some((s.parse().ok()?, 1))
        }
    };

    Some((parse_range(old_str)?, parse_range(new_str)?))
}

/// Run `git diff --unified=0 HEAD -- <path>` and return the output.
/// Returns `None` if the command fails (not a git repo, file untracked, etc.).
fn run_git_diff(file_path: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["diff", "--unified=0", "HEAD", "--", file_path.to_str()?])
        .current_dir(file_path.parent()?)
        .output()
        .ok()?;

    if output.status.success() || !output.stdout.is_empty() {
        Some(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        None
    }
}

/// Compute git gutter line states for a file.
/// Returns a `Vec<GitLineState>` with one entry per line (0-indexed).
pub fn compute_git_gutter(file_path: &Path, line_count: usize) -> Vec<GitLineState> {
    let diff_output = match run_git_diff(file_path) {
        Some(o) => o,
        None => return vec![GitLineState::Unchanged; line_count],
    };

    let changes = parse_git_diff(&diff_output);
    (1..=line_count)
        .map(|n| changes.get(&n).copied().unwrap_or(GitLineState::Unchanged))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn states(diff: &str, line_count: usize) -> Vec<GitLineState> {
        let changes = parse_git_diff(diff);
        (1..=line_count)
            .map(|n| changes.get(&n).copied().unwrap_or(GitLineState::Unchanged))
            .collect()
    }

    #[test]
    fn test_pure_addition() {
        let diff = "@@ -0,0 +1,3 @@\n+line one\n+line two\n+line three\n";
        let result = states(diff, 3);
        assert_eq!(result, vec![GitLineState::Added; 3]);
    }

    #[test]
    fn test_single_line_modification() {
        let diff = "@@ -5,1 +5,1 @@\n-old content\n+new content\n";
        let result = states(diff, 8);
        assert_eq!(result[4], GitLineState::Modified);
        assert_eq!(result[0], GitLineState::Unchanged);
    }

    #[test]
    fn test_pure_deletion() {
        let diff = "@@ -3,2 +3,0 @@\n-deleted line 1\n-deleted line 2\n";
        let result = states(diff, 6);
        assert_eq!(result[2], GitLineState::Deleted);
    }

    #[test]
    fn test_added_lines_no_deletion() {
        let diff = "@@ -6,0 +7,2 @@\n+new line a\n+new line b\n";
        let result = states(diff, 10);
        assert_eq!(result[6], GitLineState::Added);
        assert_eq!(result[7], GitLineState::Added);
        assert_eq!(result[5], GitLineState::Unchanged);
    }

    #[test]
    fn test_parse_hunk_header_single_line() {
        let result = parse_hunk_header("@@ -5 +5 @@");
        assert_eq!(result, Some(((5, 1), (5, 1))));
    }

    #[test]
    fn test_parse_hunk_header_full() {
        let result = parse_hunk_header("@@ -10,3 +10,4 @@ some context");
        assert_eq!(result, Some(((10, 3), (10, 4))));
    }

    #[test]
    fn test_empty_diff_all_unchanged() {
        let result = states("", 5);
        assert_eq!(result, vec![GitLineState::Unchanged; 5]);
    }

    #[test]
    fn test_command_state_insert_and_backspace() {
        use crate::types::FileCommandState;

        let mut cmd = FileCommandState::default();
        cmd.active = true;
        cmd.insert_char('w');
        assert_eq!(cmd.buffer, "w");
        assert_eq!(cmd.cursor, 1);

        cmd.insert_char('q');
        assert_eq!(cmd.buffer, "wq");
        assert_eq!(cmd.cursor, 2);

        cmd.backspace();
        assert_eq!(cmd.buffer, "w");
        assert_eq!(cmd.cursor, 1);

        cmd.backspace();
        assert_eq!(cmd.buffer, "");
        assert_eq!(cmd.cursor, 0);

        // Backspace at 0 is a no-op
        cmd.backspace();
        assert_eq!(cmd.cursor, 0);
    }
}
