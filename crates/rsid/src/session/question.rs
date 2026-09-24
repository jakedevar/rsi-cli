use rsi_common::types::{ConversationEvent, EventType, PendingQuestion, SessionProvider};

/// Recognize a provider's structured-question tool and normalize it to `PendingQuestion`.
/// Claude only for now; returns `None` for every other provider, event type, or tool name.
pub fn detect(provider: SessionProvider, event: &ConversationEvent) -> Option<PendingQuestion> {
    if provider != SessionProvider::Claude {
        return None;
    }
    if event.event_type != EventType::ToolUse {
        return None;
    }
    if event.tool_name.as_deref() != Some("AskUserQuestion") {
        return None;
    }
    let input = event.tool_input.as_ref()?;
    serde_json::from_value::<PendingQuestion>((**input).clone()).ok()
}

/// Frame the user's normalized answer as provider-facing continue text.
pub fn encode_answer(provider: SessionProvider, response_text: &str) -> String {
    match provider {
        // Claude consumes the answer as a fresh user turn via --resume (Fork 5: text resume).
        _ => format!("My answer to your question: {response_text}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::types::{ConversationEvent, EventType, SessionProvider};

    #[test]
    fn test_detect_claude_valid() {
        let question_json = serde_json::json!({
            "questions": [
                {
                    "question": "What is your favorite color?",
                    "header": "Color Query",
                    "options": [
                        {
                            "label": "Red",
                            "description": "The color of apples"
                        },
                        {
                            "label": "Blue",
                            "description": "The color of the sky"
                        }
                    ],
                    "multiSelect": false
                }
            ]
        });

        let event = ConversationEvent {
            id: 1,
            session_id: uuid::Uuid::new_v4(),
            sequence: 1,
            event_type: EventType::ToolUse,
            role: None,
            created_at: chrono::Utc::now(),
            content: String::new(),
            tool_name: Some("AskUserQuestion".to_string()),
            tool_input: Some(Box::new(question_json)),
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        };

        let result = detect(SessionProvider::Claude, &event);
        assert!(result.is_some());
        let pending = result.unwrap();
        assert_eq!(pending.questions.len(), 1);
        assert_eq!(
            pending.questions[0].question,
            "What is your favorite color?"
        );
        assert_eq!(pending.questions[0].multi_select, false);
    }

    #[test]
    fn test_detect_non_claude() {
        let question_json = serde_json::json!({
            "questions": []
        });

        let event = ConversationEvent {
            id: 1,
            session_id: uuid::Uuid::new_v4(),
            sequence: 1,
            event_type: EventType::ToolUse,
            role: None,
            created_at: chrono::Utc::now(),
            content: String::new(),
            tool_name: Some("AskUserQuestion".to_string()),
            tool_input: Some(Box::new(question_json)),
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        };

        let result = detect(SessionProvider::Codex, &event);
        assert!(result.is_none());
    }

    #[test]
    fn test_detect_wrong_tool() {
        let question_json = serde_json::json!({
            "questions": []
        });

        let event = ConversationEvent {
            id: 1,
            session_id: uuid::Uuid::new_v4(),
            sequence: 1,
            event_type: EventType::ToolUse,
            role: None,
            created_at: chrono::Utc::now(),
            content: String::new(),
            tool_name: Some("SomeOtherTool".to_string()),
            tool_input: Some(Box::new(question_json)),
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        };

        let result = detect(SessionProvider::Claude, &event);
        assert!(result.is_none());
    }

    #[test]
    fn test_detect_wrong_event_type() {
        let event = ConversationEvent {
            id: 1,
            session_id: uuid::Uuid::new_v4(),
            sequence: 1,
            event_type: EventType::Message,
            role: None,
            created_at: chrono::Utc::now(),
            content: String::new(),
            tool_name: Some("AskUserQuestion".to_string()),
            tool_input: None,
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        };

        let result = detect(SessionProvider::Claude, &event);
        assert!(result.is_none());
    }

    #[test]
    fn test_encode_answer() {
        let ans = encode_answer(SessionProvider::Claude, "Blue");
        assert_eq!(ans, "My answer to your question: Blue");
    }
}
