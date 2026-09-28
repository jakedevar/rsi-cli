//! One line-preserving output bound for Harness tools.

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
}
