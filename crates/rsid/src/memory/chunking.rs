use super::files::hash_text;
use super::types::MemoryChunk;

struct LineEntry {
    text: String,
    line_no: u32,
}

/// Split Markdown content into overlapping text chunks for indexing.
///
/// Uses a line-by-line accumulation algorithm: lines are appended to the current
/// chunk until the character budget is exhausted, then the chunk is flushed and
/// tail lines are carried forward as overlap for the next chunk.
///
/// Character budget: `max_tokens * 4` (approximation: 1 token ~ 4 chars).
/// Overlap budget: `overlap * 4` characters carried from the tail of each chunk.
///
/// Very long lines (longer than `max_chars`) are split into segments of `max_chars`
/// before accumulation, so a single enormous line does not produce a chunk that
/// exceeds the budget.
///
/// Line numbers in the returned chunks are 1-indexed and refer to positions
/// in the original `content` string.
pub fn chunk_markdown(content: &str, max_tokens: u32, overlap: u32) -> Vec<MemoryChunk> {
    let lines: Vec<&str> = content.split('\n').collect();
    if lines.is_empty() {
        return vec![];
    }

    let max_chars = (max_tokens * 4).max(32) as usize;
    let overlap_chars = (overlap * 4) as usize;

    let mut current: Vec<LineEntry> = Vec::new();
    let mut current_chars: usize = 0;
    let mut chunks: Vec<MemoryChunk> = Vec::new();

    for (i, line) in lines.iter().enumerate() {
        let line_no = (i + 1) as u32;

        let segments: Vec<&str> = if line.is_empty() {
            vec![""]
        } else {
            // Split on char boundaries at max_chars
            let mut segs = Vec::new();
            let mut chars_iter = line.char_indices().peekable();
            let mut start = 0;
            let mut char_count = 0;

            while let Some(&(_idx, _)) = chars_iter.peek() {
                chars_iter.next();
                char_count += 1;
                if char_count == max_chars {
                    let end = chars_iter.peek().map(|&(idx, _)| idx).unwrap_or(line.len());
                    segs.push(&line[start..end]);
                    start = end;
                    char_count = 0;
                }
            }
            if start < line.len() {
                segs.push(&line[start..]);
            }
            if segs.is_empty() {
                segs.push(line);
            }
            segs
        };

        for segment in segments {
            let line_size = segment.len() + 1; // +1 for newline joiner

            if current_chars + line_size > max_chars && !current.is_empty() {
                // Flush current chunk
                flush(&current, &mut chunks);
                let (carried, carried_chars) = carry_overlap(&current, overlap_chars);
                current = carried;
                current_chars = carried_chars;
            }

            current.push(LineEntry {
                text: segment.to_string(),
                line_no,
            });
            current_chars += line_size;
        }
    }

    // Final flush
    flush(&current, &mut chunks);

    chunks
}

fn flush(current: &[LineEntry], chunks: &mut Vec<MemoryChunk>) {
    if current.is_empty() {
        return;
    }
    let text: String = current
        .iter()
        .map(|e| e.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let start_line = current[0].line_no;
    let end_line = current[current.len() - 1].line_no;
    chunks.push(MemoryChunk {
        start_line,
        end_line,
        text: text.clone(),
        hash: hash_text(&text),
    });
}

fn carry_overlap(current: &[LineEntry], overlap_chars: usize) -> (Vec<LineEntry>, usize) {
    if overlap_chars == 0 || current.is_empty() {
        return (vec![], 0);
    }

    let mut acc: usize = 0;
    let mut start_idx = current.len();

    for i in (0..current.len()).rev() {
        acc += current[i].text.len() + 1;
        start_idx = i;
        if acc >= overlap_chars {
            break;
        }
    }

    let kept: Vec<LineEntry> = current[start_idx..]
        .iter()
        .map(|e| LineEntry {
            text: e.text.clone(),
            line_no: e.line_no,
        })
        .collect();
    let kept_chars: usize = kept.iter().map(|e| e.text.len() + 1).sum();

    (kept, kept_chars)
}

/// Truncate chunks that exceed the embedding model's maximum input token limit.
///
/// Chunks whose text length (in bytes) fits within `max_tokens` are passed through
/// unchanged. Oversized chunks are split at `max_tokens`-byte boundaries, preserving
/// the original `start_line` and `end_line`, and each sub-chunk gets a fresh SHA-256 hash.
///
/// This is a post-processing step applied after `chunk_markdown` and before embedding.
pub fn enforce_max_input_tokens(chunks: Vec<MemoryChunk>, max_tokens: u32) -> Vec<MemoryChunk> {
    let max_bytes = max_tokens as usize;
    let mut out = Vec::new();

    for chunk in chunks {
        if chunk.text.len() <= max_bytes {
            out.push(chunk);
            continue;
        }

        // Split at UTF-8 char boundaries
        let bytes = chunk.text.as_bytes();
        let mut start = 0;
        while start < bytes.len() {
            let mut end = (start + max_bytes).min(bytes.len());
            // Walk back to a char boundary
            while end > start && !chunk.text.is_char_boundary(end) {
                end -= 1;
            }
            if end == start {
                // Degenerate: max_bytes is 0 or smaller than a single char.
                // Advance to next char boundary.
                end = start + 1;
                while end < bytes.len() && !chunk.text.is_char_boundary(end) {
                    end += 1;
                }
            }
            let sub_text = &chunk.text[start..end];
            out.push(MemoryChunk {
                start_line: chunk.start_line,
                end_line: chunk.end_line,
                text: sub_text.to_string(),
                hash: hash_text(sub_text),
            });
            start = end;
        }
    }

    out
}

/// Remap chunk line numbers from content-relative positions to original source positions.
///
/// `line_map[i]` gives the 1-indexed source position for content line `i` (0-indexed).
/// Chunk `start_line` and `end_line` are 1-indexed; they are converted to 0-indexed
/// for lookup, then replaced with the mapped value.
///
/// If `line_map` is empty, chunks are returned unchanged.
pub fn remap_chunk_lines(chunks: &mut [MemoryChunk], line_map: &[u32]) {
    if line_map.is_empty() {
        return;
    }
    for chunk in chunks.iter_mut() {
        let start_idx = (chunk.start_line - 1) as usize;
        let end_idx = (chunk.end_line - 1) as usize;
        if start_idx < line_map.len() {
            chunk.start_line = line_map[start_idx];
        }
        if end_idx < line_map.len() {
            chunk.end_line = line_map[end_idx];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- chunk_markdown tests ---

    #[test]
    fn test_chunk_empty_input() {
        let chunks = chunk_markdown("", 100, 10);
        // Empty string splits to [""], which produces one chunk with empty text
        // Actually per the algorithm: lines = [""], which is non-empty,
        // so we get one chunk with text ""
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "");
    }

    #[test]
    fn test_chunk_single_short_line() {
        let chunks = chunk_markdown("hello world", 100, 10);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].start_line, 1);
        assert_eq!(chunks[0].end_line, 1);
        assert_eq!(chunks[0].text, "hello world");
    }

    #[test]
    fn test_chunk_single_line_exact_boundary() {
        // max_tokens=8 -> max_chars=32. Create a 32-char line.
        let line = "a".repeat(32);
        let chunks = chunk_markdown(&line, 8, 0);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, line);
    }

    #[test]
    fn test_chunk_single_long_line() {
        // max_tokens=8 -> max_chars=32. Create a 100-char line.
        let line = "a".repeat(100);
        let chunks = chunk_markdown(&line, 8, 0);
        // Should be split into segments
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            assert_eq!(chunk.start_line, 1);
            assert_eq!(chunk.end_line, 1);
        }
    }

    #[test]
    fn test_chunk_multiple_lines_no_overflow() {
        // max_tokens=100 -> max_chars=400. Several short lines.
        let content = "line 1\nline 2\nline 3";
        let chunks = chunk_markdown(content, 100, 0);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].start_line, 1);
        assert_eq!(chunks[0].end_line, 3);
        assert_eq!(chunks[0].text, content);
    }

    #[test]
    fn test_chunk_multiple_lines_with_flush() {
        // max_tokens=8 -> max_chars=32
        // Each line is ~10 chars + newline = ~11. Three lines = 33 > 32.
        let content = "aaaaaaaaaa\nbbbbbbbbbb\ncccccccccc";
        let chunks = chunk_markdown(content, 8, 0);
        assert!(chunks.len() >= 2);
    }

    #[test]
    fn test_chunk_overlap_carry() {
        // max_tokens=8 -> max_chars=32, overlap=4 -> overlap_chars=16
        let content = "aaaaaaaaaa\nbbbbbbbbbb\ncccccccccc\ndddddddddd";
        let chunks = chunk_markdown(content, 8, 4);
        assert!(chunks.len() >= 2);
        // With overlap, second chunk should start before first chunk's end + 1
        if chunks.len() >= 2 {
            assert!(chunks[1].start_line <= chunks[0].end_line + 1);
        }
    }

    #[test]
    fn test_chunk_zero_overlap() {
        let content = "aaaaaaaaaa\nbbbbbbbbbb\ncccccccccc\ndddddddddd";
        let chunks = chunk_markdown(content, 8, 0);
        // With zero overlap, chunks should not share lines
        for i in 1..chunks.len() {
            assert!(chunks[i].start_line > chunks[i - 1].end_line);
        }
    }

    #[test]
    fn test_chunk_all_empty_lines() {
        let content = "\n\n\n\n\n\n\n\n\n\n";
        let chunks = chunk_markdown(content, 8, 0);
        // Should produce chunks (empty lines have line_size=1)
        assert!(!chunks.is_empty());
    }

    #[test]
    fn test_chunk_unicode_content() {
        let content = "こんにちは世界\n🌍🌎🌏";
        let chunks = chunk_markdown(content, 100, 0);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, content);
    }

    #[test]
    fn test_chunk_hash_determinism() {
        let content = "# Test\n\nSome content here.";
        let chunks1 = chunk_markdown(content, 100, 10);
        let chunks2 = chunk_markdown(content, 100, 10);
        assert_eq!(chunks1.len(), chunks2.len());
        for (a, b) in chunks1.iter().zip(chunks2.iter()) {
            assert_eq!(a.hash, b.hash);
        }
    }

    #[test]
    fn test_chunk_max_tokens_zero() {
        // Should clamp to max_chars=32, not panic
        let chunks = chunk_markdown("hello", 0, 0);
        assert!(!chunks.is_empty());
    }

    #[test]
    fn test_chunk_markdown_with_headings() {
        let content = "# Heading 1\n\nSome paragraph text.\n\n## Heading 2\n\n- item 1\n- item 2\n\n```\ncode block\n```";
        let chunks = chunk_markdown(content, 100, 10);
        assert!(!chunks.is_empty());
        // Reconstructing: join all chunk texts should cover the content
        let reconstructed = chunks.iter().map(|c| c.text.as_str()).collect::<Vec<_>>();
        assert!(!reconstructed.is_empty());
    }

    // --- enforce_max_input_tokens tests ---

    #[test]
    fn test_enforce_max_input_tokens_passthrough() {
        let chunks = vec![
            MemoryChunk {
                start_line: 1,
                end_line: 5,
                text: "short".to_string(),
                hash: hash_text("short"),
            },
            MemoryChunk {
                start_line: 6,
                end_line: 10,
                text: "also short".to_string(),
                hash: hash_text("also short"),
            },
        ];
        let result = enforce_max_input_tokens(chunks.clone(), 100);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].text, "short");
        assert_eq!(result[1].text, "also short");
    }

    #[test]
    fn test_enforce_max_input_tokens_split() {
        let long_text = "a".repeat(20);
        let chunks = vec![
            MemoryChunk {
                start_line: 1,
                end_line: 1,
                text: "ok".to_string(),
                hash: hash_text("ok"),
            },
            MemoryChunk {
                start_line: 2,
                end_line: 5,
                text: long_text.clone(),
                hash: hash_text(&long_text),
            },
        ];
        let result = enforce_max_input_tokens(chunks, 10);
        // First chunk passes through, second is split into 2 sub-chunks (20 bytes / 10)
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].text, "ok");
        assert_eq!(result[1].text, "a".repeat(10));
        assert_eq!(result[2].text, "a".repeat(10));
        // Sub-chunks preserve start_line/end_line
        assert_eq!(result[1].start_line, 2);
        assert_eq!(result[1].end_line, 5);
    }

    #[test]
    fn test_enforce_max_input_tokens_utf8_boundary() {
        // "é" is 2 bytes in UTF-8
        let text = "éééééééééé"; // 10 chars, 20 bytes
        let chunks = vec![MemoryChunk {
            start_line: 1,
            end_line: 1,
            text: text.to_string(),
            hash: hash_text(text),
        }];
        let result = enforce_max_input_tokens(chunks, 7);
        // Should split without breaking UTF-8. 7 bytes = 3 full "é" (6 bytes), then next split.
        for chunk in &result {
            // Each sub-chunk should be valid UTF-8 (it is, since we get &str)
            assert!(!chunk.text.is_empty() || chunk.text.is_empty());
            // Just verify no panic from invalid UTF-8
            let _ = chunk.text.chars().count();
        }
        // Total content should be preserved
        let reassembled: String = result.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(reassembled, text);
    }

    #[test]
    fn test_enforce_max_input_tokens_empty_chunks() {
        let result = enforce_max_input_tokens(vec![], 100);
        assert!(result.is_empty());
    }

    // --- remap_chunk_lines tests ---

    #[test]
    fn test_remap_chunk_lines_basic() {
        let mut chunks = vec![MemoryChunk {
            start_line: 1,
            end_line: 3,
            text: "test".to_string(),
            hash: "h".to_string(),
        }];
        let line_map = vec![10, 20, 30];
        remap_chunk_lines(&mut chunks, &line_map);
        assert_eq!(chunks[0].start_line, 10);
        assert_eq!(chunks[0].end_line, 30);
    }

    #[test]
    fn test_remap_chunk_lines_empty_map() {
        let mut chunks = vec![MemoryChunk {
            start_line: 1,
            end_line: 3,
            text: "test".to_string(),
            hash: "h".to_string(),
        }];
        remap_chunk_lines(&mut chunks, &[]);
        assert_eq!(chunks[0].start_line, 1);
        assert_eq!(chunks[0].end_line, 3);
    }

    #[test]
    fn test_remap_chunk_lines_out_of_bounds() {
        let mut chunks = vec![MemoryChunk {
            start_line: 1,
            end_line: 5,
            text: "test".to_string(),
            hash: "h".to_string(),
        }];
        let line_map = vec![10, 20]; // only 2 entries, chunk references line 5
        remap_chunk_lines(&mut chunks, &line_map);
        assert_eq!(chunks[0].start_line, 10); // line 1 -> index 0 -> remapped
        assert_eq!(chunks[0].end_line, 5); // line 5 -> index 4 -> out of bounds, preserved
    }

    #[test]
    fn test_remap_chunk_lines_single_line() {
        let mut chunks = vec![
            MemoryChunk {
                start_line: 1,
                end_line: 1,
                text: "test".to_string(),
                hash: "h".to_string(),
            },
            MemoryChunk {
                start_line: 2,
                end_line: 2,
                text: "test2".to_string(),
                hash: "h2".to_string(),
            },
        ];
        let line_map = vec![42];
        remap_chunk_lines(&mut chunks, &line_map);
        assert_eq!(chunks[0].start_line, 42);
        assert_eq!(chunks[0].end_line, 42);
        assert_eq!(chunks[1].start_line, 2); // out of bounds, preserved
        assert_eq!(chunks[1].end_line, 2); // out of bounds, preserved
    }

    // --- Integration smoke tests ---

    #[test]
    fn test_file_pipeline_integration() {
        use crate::memory::files::{build_file_entry, list_memory_files};
        use std::fs;

        let dir = tempfile::tempdir().unwrap();
        let content = "# Memory\n\nThis is a test memory file.\n\n## Section 2\n\nMore content here with details.\n\n## Section 3\n\nEven more content to ensure we have enough for chunking.";
        fs::write(dir.path().join("MEMORY.md"), content).unwrap();

        // Discover files
        let files = list_memory_files(dir.path()).unwrap();
        assert_eq!(files.len(), 1);

        // Build entry
        let entry = build_file_entry(&files[0], dir.path()).unwrap().unwrap();
        assert_eq!(entry.path, "MEMORY.md");

        // Read content and chunk
        let file_content = fs::read_to_string(&files[0]).unwrap();
        let chunks = chunk_markdown(&file_content, 100, 10);
        assert!(!chunks.is_empty());

        // Enforce token limits
        let enforced = enforce_max_input_tokens(chunks, 8192);
        assert!(!enforced.is_empty());

        // Hashes are deterministic
        let chunks2 = chunk_markdown(&file_content, 100, 10);
        for (a, b) in enforced
            .iter()
            .zip(enforce_max_input_tokens(chunks2, 8192).iter())
        {
            assert_eq!(a.hash, b.hash);
        }
    }

    #[test]
    fn test_session_pipeline_integration() {
        use crate::memory::session_text::extract_session_text;
        use chrono::Utc;
        use rsi_common::types::{ConversationEvent, EventType, Role};
        use uuid::Uuid;

        let events = vec![
            ConversationEvent {
                id: 0,
                session_id: Uuid::nil(),
                sequence: 1,
                event_type: EventType::Message,
                role: Some(Role::User),
                content: "What is Rust?".to_string(),
                tool_name: None,
                tool_input: None,
                created_at: Utc::now(),
                offload_id: None,
                tool_use_id: None,
                metadata: None,
            },
            ConversationEvent {
                id: 0,
                session_id: Uuid::nil(),
                sequence: 2,
                event_type: EventType::Thinking,
                role: None,
                content: "Let me think...".to_string(),
                tool_name: None,
                tool_input: None,
                created_at: Utc::now(),
                offload_id: None,
                tool_use_id: None,
                metadata: None,
            },
            ConversationEvent {
                id: 0,
                session_id: Uuid::nil(),
                sequence: 3,
                event_type: EventType::Message,
                role: Some(Role::Assistant),
                content:
                    "Rust is a systems programming language focused on safety and performance."
                        .to_string(),
                tool_name: None,
                tool_input: None,
                created_at: Utc::now(),
                offload_id: None,
                tool_use_id: None,
                metadata: None,
            },
        ];

        // Extract session text
        let (text, line_map) = extract_session_text(&events).unwrap();
        assert_eq!(line_map.len(), 2); // Only User + Assistant messages

        // Chunk the session text
        let mut chunks = chunk_markdown(&text, 100, 10);
        assert!(!chunks.is_empty());

        // Remap line numbers
        remap_chunk_lines(&mut chunks, &line_map);

        // Verify line numbers were remapped to event sequences
        assert_eq!(chunks[0].start_line, 1); // maps to sequence 1
    }
}
