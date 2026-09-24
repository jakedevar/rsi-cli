//! Message chunking for long replies.
//!
//! Splits long messages into chunks that fit within iMessage's
//! practical character limit (default 4000). Splits at paragraph
//! boundaries first, then sentence boundaries, then hard splits.

/// Split a message into chunks of at most `max_len` characters.
/// Each chunk is prefixed with `[N/M]` when there are multiple chunks.
pub fn chunk_message(text: &str, max_len: usize) -> Vec<String> {
    if text.len() <= max_len {
        return vec![text.to_string()];
    }

    let raw_chunks = split_at_boundaries(text, max_len);

    if raw_chunks.len() == 1 {
        return raw_chunks;
    }

    // Add [N/M] prefix to each chunk
    let total = raw_chunks.len();
    raw_chunks
        .into_iter()
        .enumerate()
        .map(|(i, chunk)| format!("[{}/{}] {}", i + 1, total, chunk))
        .collect()
}

fn split_at_boundaries(text: &str, max_len: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut remaining = text;

    // Reserve space for prefix like "[99/99] "
    let effective_max = max_len.saturating_sub(10);
    if effective_max == 0 {
        // Degenerate case: just hard split
        return hard_split(text, max_len);
    }

    while !remaining.is_empty() {
        if remaining.len() <= effective_max {
            chunks.push(remaining.to_string());
            break;
        }

        // Try to find a paragraph break within the limit
        if let Some(split_pos) = find_paragraph_break(remaining, effective_max) {
            chunks.push(remaining[..split_pos].trim_end().to_string());
            remaining = remaining[split_pos..].trim_start();
            continue;
        }

        // Try sentence boundary
        if let Some(split_pos) = find_sentence_break(remaining, effective_max) {
            chunks.push(remaining[..split_pos].trim_end().to_string());
            remaining = remaining[split_pos..].trim_start();
            continue;
        }

        // Try word boundary
        if let Some(split_pos) = find_word_break(remaining, effective_max) {
            chunks.push(remaining[..split_pos].trim_end().to_string());
            remaining = remaining[split_pos..].trim_start();
            continue;
        }

        // Hard split at max_len
        let split_at = remaining
            .char_indices()
            .take_while(|(i, _)| *i < effective_max)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(effective_max.min(remaining.len()));
        chunks.push(remaining[..split_at].to_string());
        remaining = &remaining[split_at..];
    }

    chunks
}

fn find_paragraph_break(text: &str, max_pos: usize) -> Option<usize> {
    let search_range = &text[..max_pos.min(text.len())];
    search_range.rfind("\n\n").map(|pos| pos + 2)
}

fn find_sentence_break(text: &str, max_pos: usize) -> Option<usize> {
    let search_range = &text[..max_pos.min(text.len())];
    // Look for sentence-ending punctuation followed by space
    let mut last_sentence_end = None;
    for (i, c) in search_range.char_indices() {
        if (c == '.' || c == '!' || c == '?') && i + 1 < search_range.len() {
            let next = search_range.as_bytes().get(i + 1);
            if next == Some(&b' ') || next == Some(&b'\n') {
                last_sentence_end = Some(i + 2);
            }
        }
    }
    last_sentence_end
}

fn find_word_break(text: &str, max_pos: usize) -> Option<usize> {
    let search_range = &text[..max_pos.min(text.len())];
    search_range.rfind(' ').map(|pos| pos + 1)
}

fn hard_split(text: &str, max_len: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut remaining = text;
    while !remaining.is_empty() {
        let split_at = remaining
            .char_indices()
            .take_while(|(i, _)| *i < max_len)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(remaining.len().min(max_len));
        chunks.push(remaining[..split_at].to_string());
        remaining = &remaining[split_at..];
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_short_message_no_chunking() {
        let chunks = chunk_message("Hello world", 100);
        assert_eq!(chunks, vec!["Hello world"]);
    }

    #[test]
    fn test_exact_limit() {
        let text = "a".repeat(100);
        let chunks = chunk_message(&text, 100);
        assert_eq!(chunks.len(), 1);
    }

    #[test]
    fn test_paragraph_split() {
        let text = format!("{}\n\n{}", "a".repeat(50), "b".repeat(50));
        let chunks = chunk_message(&text, 70);
        assert!(chunks.len() >= 2);
    }

    #[test]
    fn test_sentence_split() {
        let text = "This is sentence one. This is sentence two. This is sentence three.";
        let chunks = chunk_message(text, 40);
        assert!(chunks.len() >= 2);
        // Each chunk should end at a sentence boundary
    }

    #[test]
    fn test_hard_split() {
        let text = "a".repeat(200);
        let chunks = chunk_message(&text, 100);
        assert!(chunks.len() >= 2);
    }

    #[test]
    fn test_chunk_prefix() {
        let text = "a".repeat(200);
        let chunks = chunk_message(&text, 100);
        assert!(chunks[0].starts_with("[1/"));
    }

    #[test]
    fn test_empty_message() {
        let chunks = chunk_message("", 100);
        assert_eq!(chunks, vec![""]);
    }

    #[test]
    fn test_unicode_safe_split() {
        // Ensure we don't split in the middle of a multi-byte character
        let text = "a".repeat(99) + "\u{1F600}"; // emoji at the end
        let chunks = chunk_message(&text, 100);
        // Should handle gracefully without panic
        assert!(!chunks.is_empty());
    }
}
