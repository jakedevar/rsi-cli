//! One output bound for Harness tools: spill the full text and return a stub
//! (#1097), or fall back to a line-preserving truncation.

use rsi_common::spill::{self, SpillConfig, SpillMeta};

/// The byte count is exact only when the producer supplied its complete output.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct TruncatedText {
    pub content: String,
    pub limit_bytes: usize,
    pub captured_bytes: usize,
    pub original_bytes: Option<usize>,
    pub retained_bytes: usize,
    pub truncated: bool,
}

/// Keep complete lines within `limit_bytes`, including the truncation marker.
/// `upstream_truncated` means a bounded producer already dropped some bytes.
pub(crate) fn truncate_text(
    input: &str,
    limit_bytes: usize,
    upstream_truncated: bool,
) -> TruncatedText {
    let truncated = upstream_truncated || input.len() > limit_bytes;
    let original_bytes = (!upstream_truncated).then_some(input.len());
    if !truncated {
        return TruncatedText {
            content: input.to_owned(),
            limit_bytes,
            captured_bytes: input.len(),
            original_bytes,
            retained_bytes: input.len(),
            truncated: false,
        };
    }

    let total = match original_bytes {
        Some(bytes) => format!("{bytes} bytes total"),
        None => format!("at least {} bytes captured", input.len()),
    };
    let marker = format!("[truncated: limit {limit_bytes} bytes; {total}]");
    let source_budget = limit_bytes.saturating_sub(marker.len());
    let mut boundary = source_budget.min(input.len());
    while !input.is_char_boundary(boundary) {
        boundary -= 1;
    }
    let retained_bytes = input[..boundary].rfind('\n').map_or(0, |at| at + 1);
    let mut content = String::with_capacity(limit_bytes);
    if marker.len() <= limit_bytes {
        content.push_str(&input[..retained_bytes]);
        content.push_str(&marker);
    }
    TruncatedText {
        content,
        limit_bytes,
        captured_bytes: input.len(),
        original_bytes,
        retained_bytes,
        truncated: true,
    }
}

/// The stub's last line names the CLI; a Harness model has the
/// `read_output` tool instead, so say that.
fn harness_footer(stub: String, handle: &str) -> String {
    let body = stub.trim_end_matches('\n');
    let Some((head, last)) = body.rsplit_once('\n') else {
        return stub;
    };
    if !last.starts_with("full: rsi-spill show") {
        return stub;
    }
    format!(
        "{head}\nfull: read_output {{\"handle\":\"{handle}\",\"grep\":\"P\",\"range\":\"a:b\"}} (grep, range optional)"
    )
}

/// Bound `input` to `limit_bytes` by spilling instead of discarding.
///
/// Over the spill thresholds the full text is stored under a handle in the
/// shared rsi-common spill store and the result is the one deterministic stub
/// (exit status, size, error / FAILED / summary lines, head, tail, handle).
/// `deliberate` is for tools the model calls to see exact content (`read_file`):
/// they spill only when the text would otherwise be cut at `limit_bytes`.
/// Without a usable store (disabled, unwritable, no saving) this is
/// [`truncate_text`]. Nothing here edits an earlier message: it only decides
/// what a new tool result looks like.
pub(crate) fn spill_or_truncate(
    cfg: &SpillConfig,
    key: &str,
    tool: &str,
    exit: Option<i32>,
    input: &str,
    limit_bytes: usize,
    deliberate: bool,
) -> TruncatedText {
    let over_limit = input.len() > limit_bytes;
    if cfg.disabled || (deliberate && !over_limit) {
        return truncate_text(input, limit_bytes, false);
    }
    let thresholds = SpillConfig {
        max_bytes: if deliberate {
            limit_bytes
        } else {
            cfg.max_bytes.min(limit_bytes)
        },
        max_lines: if deliberate {
            usize::MAX
        } else {
            cfg.max_lines
        },
        ..cfg.clone()
    };
    let meta = SpillMeta {
        tool,
        command: None,
        exit,
    };
    let Some((handle, stub)) = spill::spill_text(&thresholds, key, &meta, input) else {
        return truncate_text(input, limit_bytes, false);
    };
    let stub = harness_footer(stub, &handle);
    TruncatedText {
        retained_bytes: stub.len(),
        content: stub,
        limit_bytes,
        captured_bytes: input.len(),
        original_bytes: Some(input.len()),
        truncated: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn complete_text_keeps_exact_bytes() {
        let result = truncate_text("one\ntwo", 7, false);
        assert_eq!(result.content, "one\ntwo");
        assert_eq!(result.original_bytes, Some(7));
        assert!(!result.truncated);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn truncation_keeps_only_complete_lines_and_reports_total() {
        let input = format!("first\n{}\nlast", "é".repeat(50));
        let result = truncate_text(&input, 70, false);
        assert!(result.truncated);
        assert_eq!(result.original_bytes, Some(input.len()));
        assert_eq!(result.retained_bytes, "first\n".len());
        assert!(result.content.starts_with("first\n[truncated:"));
        assert!(result.content.len() <= result.limit_bytes);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn upstream_cut_never_exposes_a_partial_line() {
        let result = truncate_text("complete\npartial", 80, true);
        assert_eq!(result.original_bytes, None);
        assert_eq!(result.retained_bytes, "complete\n".len());
        assert!(result.content.starts_with("complete\n[truncated:"));
        assert!(result.content.contains("at least 16 bytes captured"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn long_first_line_and_tiny_limit_stay_bounded() {
        let result = truncate_text(&"x".repeat(200), 60, false);
        assert_eq!(result.retained_bytes, 0);
        assert!(result.content.starts_with("[truncated:"));
        assert!(result.content.len() <= 60);
        assert_eq!(truncate_text("abcdef", 5, false).content, "");
    }

    fn spill_cfg(root: &std::path::Path) -> SpillConfig {
        SpillConfig {
            root: root.to_path_buf(),
            max_bytes: 8 * 1024,
            max_lines: 200,
            disabled: false,
        }
    }

    fn cargo_like_output(lines: usize) -> String {
        let mut out = String::from("running 4000 tests\n");
        for n in 0..lines {
            out.push_str(&format!("test suite::case_{n:05} ... ok\n"));
        }
        out.push_str("test suite::needle_case ... FAILED\n");
        out.push_str("test result: FAILED. 4000 passed; 1 failed; 0 ignored\n");
        out
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn a_two_megabyte_output_becomes_a_small_stub_and_the_exact_lines_stay_retrievable() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = spill_cfg(dir.path());
        let input = cargo_like_output(80_000);
        assert!(input.len() > 2_000_000);
        let result =
            spill_or_truncate(&cfg, "abcd1234", "shell", Some(101), &input, 4 << 20, false);

        assert!(result.truncated);
        assert!(result.content.len() < 2048, "{}", result.content.len());
        assert!(result.content.starts_with("[rsi-spill abcd1234/1]"));
        assert!(result.content.contains("needle_case ... FAILED"));
        assert!(result.content.contains("exit=101"));
        assert!(result.content.contains("read_output"));

        let found = spill::show(dir.path(), "abcd1234/1", Some("needle_case"), None).unwrap();
        let line_no = input
            .lines()
            .position(|l| l.contains("needle_case"))
            .unwrap()
            + 1;
        assert_eq!(
            found,
            format!("{line_no}:test suite::needle_case ... FAILED\n")
        );
        let range = spill::show(dir.path(), "abcd1234/1", None, Some((2, 3))).unwrap();
        assert_eq!(
            range,
            "test suite::case_00000 ... ok\ntest suite::case_00001 ... ok\n"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn stub_generation_is_deterministic_apart_from_the_handle() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = spill_cfg(dir.path());
        let input = cargo_like_output(2_000);
        let first = spill_or_truncate(&cfg, "k", "shell", Some(1), &input, 1 << 20, false).content;
        let second = spill_or_truncate(&cfg, "k", "shell", Some(1), &input, 1 << 20, false).content;
        assert_ne!(first, second, "handles differ");
        assert_eq!(first.replace("k/1", "k/N"), second.replace("k/2", "k/N"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn small_output_passes_through_unspilled() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = spill_cfg(dir.path());
        let result = spill_or_truncate(&cfg, "k", "shell", Some(0), "ok\nfine\n", 1 << 20, false);
        assert!(!result.truncated);
        assert_eq!(result.content, "ok\nfine\n");
        assert!(!dir.path().join("k").exists());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn a_deliberate_read_spills_only_where_it_would_have_been_cut() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = spill_cfg(dir.path());
        let file = "line of source code\n".repeat(2_000);
        let kept = spill_or_truncate(&cfg, "k", "read_file", None, &file, 1 << 20, true);
        assert!(!kept.truncated);
        assert_eq!(kept.content, file);

        let cut = spill_or_truncate(&cfg, "k", "read_file", None, &file, 10_000, true);
        assert!(cut.truncated);
        assert!(cut.content.starts_with("[rsi-spill k/1]"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("k/1.out")).unwrap(),
            file
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn a_disabled_store_falls_back_to_line_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = SpillConfig {
            disabled: true,
            ..spill_cfg(dir.path())
        };
        let input = "x\n".repeat(50_000);
        let result = spill_or_truncate(&cfg, "k", "shell", None, &input, 1_000, false);
        assert!(result.truncated);
        assert!(result.content.contains("[truncated: limit 1000 bytes;"));
        assert!(!dir.path().join("k").exists());
    }
}
