use std::path::PathBuf;

use rsi_common::types::{ConversationEvent, EventType, Role};
use uuid::Uuid;

use super::files::hash_text;
use super::types::{MemoryFileEntry, MemorySource};

/// Result of extracting text from a session's conversation events.
/// Carries the extracted text, line map, and file entry metadata
/// needed for indexing.
pub struct SessionTextEntry {
    /// Memory file entry metadata for the sync engine.
    pub entry: MemoryFileEntry,
    /// Extracted plaintext content (one line per qualifying message event).
    pub content: String,
    /// Maps each content line (0-indexed) to the event sequence number.
    pub line_map: Vec<u32>,
}

/// Normalize session event text: collapse whitespace and newlines to single spaces, trim.
fn normalize_session_text(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    let mut prev_was_space = true; // start true to trim leading
    for ch in value.chars() {
        if ch.is_whitespace() {
            if !prev_was_space {
                result.push(' ');
                prev_was_space = true;
            }
            continue;
        }
        result.push(ch);
        prev_was_space = false;
    }
    // trim trailing space
    if result.ends_with(' ') {
        result.pop();
    }
    result
}

/// Extract indexable plaintext from a session's conversation events.
///
/// Filters events to `Message` type with `User` or `Assistant` role,
/// concatenates their content as labeled lines (`"User: ..."` / `"Assistant: ..."`),
/// and produces a line map that maps each output line (0-indexed) back to the
/// source event's sequence number.
///
/// Returns `None` if no qualifying events are found.
pub fn extract_session_text(events: &[ConversationEvent]) -> Option<(String, Vec<u32>)> {
    let mut collected: Vec<String> = Vec::new();
    let mut line_map: Vec<u32> = Vec::new();

    for event in events {
        if event.event_type != EventType::Message {
            continue;
        }

        let label = match event.role {
            Some(Role::User) => "User",
            Some(Role::Assistant) => "Assistant",
            _ => continue,
        };

        let normalized = normalize_session_text(&event.content);
        if normalized.is_empty() {
            continue;
        }

        collected.push(format!("{label}: {normalized}"));
        line_map.push(event.sequence as u32);
    }

    if collected.is_empty() {
        return None;
    }

    Some((collected.join("\n"), line_map))
}

/// Build a MemoryFileEntry for a session's extracted text.
///
/// Extracts text from the session's events, computes a content hash
/// (incorporating the line map for change detection), and returns
/// the entry with a synthetic path of `sessions/{session_id}`.
///
/// `project_id` is the session's project scope (denormalized into the
/// memory index so search can filter by project without joining back to
/// the main session store). `None` means the session has no project.
///
/// Returns `None` if the session has no qualifying message events.
pub fn build_session_entry(
    session_id: Uuid,
    project_id: Option<Uuid>,
    events: &[ConversationEvent],
) -> Option<SessionTextEntry> {
    let (text, line_map) = extract_session_text(events)?;

    let line_map_str = line_map
        .iter()
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let hash_input = format!("{}\n{}", text, line_map_str);
    let hash = hash_text(&hash_input);

    let path = format!("sessions/{}", session_id);

    let mtime_ms = events
        .iter()
        .map(|e| e.created_at.timestamp_millis())
        .max()
        .unwrap_or(0);

    let entry = MemoryFileEntry {
        path: path.clone(),
        abs_path: PathBuf::from(&path),
        mtime_ms,
        size: text.len() as i64,
        hash,
        source: MemorySource::Sessions,
        project_id,
        // Set by the sync loop, which holds the probe result; this builder only
        // sees already-loaded events.
        watermark: None,
    };

    Some(SessionTextEntry {
        entry,
        content: text,
        line_map,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn make_event(
        sequence: i32,
        event_type: EventType,
        role: Option<Role>,
        content: &str,
    ) -> ConversationEvent {
        ConversationEvent {
            id: 0,
            session_id: Uuid::nil(),
            sequence,
            event_type,
            role,
            content: content.to_string(),
            tool_name: None,
            tool_input: None,
            created_at: Utc::now(),
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        }
    }

    // --- normalize_session_text tests ---

    #[test]
    fn test_normalize_session_text_basic() {
        assert_eq!(normalize_session_text("hello  world"), "hello world");
    }

    #[test]
    fn test_normalize_session_text_newlines() {
        assert_eq!(normalize_session_text("line1\n\nline2"), "line1 line2");
    }

    #[test]
    fn test_normalize_session_text_tabs() {
        assert_eq!(normalize_session_text("a\t\tb"), "a b");
    }

    #[test]
    fn test_normalize_session_text_empty() {
        assert_eq!(normalize_session_text(""), "");
    }

    #[test]
    fn test_normalize_session_text_only_whitespace() {
        assert_eq!(normalize_session_text("  \n\n  "), "");
    }

    // --- extract_session_text tests ---

    #[test]
    fn test_extract_empty_events() {
        assert!(extract_session_text(&[]).is_none());
    }

    #[test]
    fn test_extract_no_messages() {
        let events = vec![
            make_event(1, EventType::ToolUse, None, "tool call"),
            make_event(2, EventType::System, None, "system msg"),
        ];
        assert!(extract_session_text(&events).is_none());
    }

    #[test]
    fn test_extract_single_user_message() {
        let events = vec![make_event(1, EventType::Message, Some(Role::User), "hello")];
        let (text, map) = extract_session_text(&events).unwrap();
        assert_eq!(text, "User: hello");
        assert_eq!(map, vec![1]);
    }

    #[test]
    fn test_extract_single_assistant_message() {
        let events = vec![make_event(
            3,
            EventType::Message,
            Some(Role::Assistant),
            "hi",
        )];
        let (text, map) = extract_session_text(&events).unwrap();
        assert_eq!(text, "Assistant: hi");
        assert_eq!(map, vec![3]);
    }

    #[test]
    fn test_extract_mixed_events() {
        let events = vec![
            make_event(1, EventType::Message, Some(Role::User), "question"),
            make_event(2, EventType::Thinking, None, "thinking..."),
            make_event(3, EventType::ToolUse, None, "tool"),
            make_event(4, EventType::ToolResult, None, "result"),
            make_event(5, EventType::Message, Some(Role::Assistant), "answer"),
            make_event(6, EventType::System, None, "system"),
        ];
        let (text, map) = extract_session_text(&events).unwrap();
        assert_eq!(text, "User: question\nAssistant: answer");
        assert_eq!(map, vec![1, 5]);
    }

    #[test]
    fn test_extract_whitespace_normalization() {
        let events = vec![make_event(
            1,
            EventType::Message,
            Some(Role::User),
            "hello  \n\n  world",
        )];
        let (text, _) = extract_session_text(&events).unwrap();
        assert_eq!(text, "User: hello world");
    }

    #[test]
    fn test_extract_empty_content_skipped() {
        let events = vec![
            make_event(1, EventType::Message, Some(Role::User), ""),
            make_event(2, EventType::Message, Some(Role::Assistant), "reply"),
        ];
        let (text, map) = extract_session_text(&events).unwrap();
        assert_eq!(text, "Assistant: reply");
        assert_eq!(map, vec![2]);
    }

    #[test]
    fn test_extract_whitespace_only_content() {
        let events = vec![make_event(
            1,
            EventType::Message,
            Some(Role::User),
            "   \n\n  ",
        )];
        assert!(extract_session_text(&events).is_none());
    }

    #[test]
    fn test_extract_line_map_correctness() {
        let events = vec![
            make_event(1, EventType::Message, Some(Role::User), "a"),
            make_event(5, EventType::Message, Some(Role::Assistant), "b"),
            make_event(10, EventType::Message, Some(Role::User), "c"),
        ];
        let (_, map) = extract_session_text(&events).unwrap();
        assert_eq!(map, vec![1, 5, 10]);
    }

    #[test]
    fn test_extract_preserves_order() {
        let events = vec![
            make_event(1, EventType::Message, Some(Role::User), "first"),
            make_event(2, EventType::Message, Some(Role::Assistant), "second"),
            make_event(3, EventType::Message, Some(Role::User), "third"),
        ];
        let (text, _) = extract_session_text(&events).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "User: first");
        assert_eq!(lines[1], "Assistant: second");
        assert_eq!(lines[2], "User: third");
    }

    #[test]
    fn test_extract_unicode_content() {
        let events = vec![make_event(
            1,
            EventType::Message,
            Some(Role::User),
            "こんにちは 🌍",
        )];
        let (text, _) = extract_session_text(&events).unwrap();
        assert_eq!(text, "User: こんにちは 🌍");
    }

    // --- build_session_entry tests ---

    #[test]
    fn test_build_session_entry_basic() {
        let id = Uuid::new_v4();
        let events = vec![
            make_event(1, EventType::Message, Some(Role::User), "hello"),
            make_event(2, EventType::Message, Some(Role::Assistant), "world"),
        ];
        let entry = build_session_entry(id, None, &events).unwrap();
        assert_eq!(entry.entry.source, MemorySource::Sessions);
        assert!(!entry.content.is_empty());
        assert_eq!(entry.line_map.len(), 2);
        assert_eq!(entry.entry.size, entry.content.len() as i64);
    }

    #[test]
    fn test_build_session_entry_no_messages() {
        let id = Uuid::new_v4();
        let events = vec![make_event(1, EventType::ToolUse, None, "tool")];
        assert!(build_session_entry(id, None, &events).is_none());
    }

    #[test]
    fn test_build_session_entry_hash_includes_line_map() {
        let id = Uuid::new_v4();
        // Same text content but different sequence numbers -> different hashes
        let events1 = vec![
            make_event(1, EventType::Message, Some(Role::User), "hello"),
            make_event(2, EventType::Message, Some(Role::Assistant), "world"),
        ];
        let events2 = vec![
            make_event(10, EventType::Message, Some(Role::User), "hello"),
            make_event(20, EventType::Message, Some(Role::Assistant), "world"),
        ];
        let entry1 = build_session_entry(id, None, &events1).unwrap();
        let entry2 = build_session_entry(id, None, &events2).unwrap();
        // Same content text, but line maps differ -> hashes differ
        assert_eq!(entry1.content, entry2.content);
        assert_ne!(entry1.entry.hash, entry2.entry.hash);
    }

    #[test]
    fn test_build_session_entry_path_format() {
        let id = Uuid::new_v4();
        let events = vec![make_event(1, EventType::Message, Some(Role::User), "x")];
        let entry = build_session_entry(id, None, &events).unwrap();
        assert_eq!(entry.entry.path, format!("sessions/{}", id));
    }
}
