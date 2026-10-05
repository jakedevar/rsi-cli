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

/// Typed code for a Claude `AskUserQuestion` that no human has answered.
///
/// The daemon runs Claude headless (`-p`, no TTY), so the CLI rejects the tool
/// inside the turn with an error result that reads like a decline. The question
/// is nevertheless recorded as pending and visible; the code marks both the
/// transcript record and the agent-facing continuation text.
pub const UNDELIVERABLE_CODE: &str = "ask_user_question_undeliverable";

/// Agent-facing text for a continuation that arrives while the question is
/// still unanswered: nobody can answer it here, so the agent decides.
pub fn undeliverable_notice() -> String {
    format!(
        "[rsi:{UNDELIVERABLE_CODE}] Your AskUserQuestion was NOT declined by a human: the \
         headless CLI auto-rejected it and no human has answered it here. Decide yourself \
         and record the decision (and the assumption it rests on) in your handoff or commit \
         message so the operator can review it."
    )
}

/// Prefix `query` with the typed no-human-answer notice when the session still
/// holds an unanswered question. `answer_question` clears the pending question
/// before it continues, so an answered continuation is never prefixed.
pub fn with_unanswered_notice(pending: Option<&PendingQuestion>, query: String) -> String {
    if pending.is_none() {
        return query;
    }
    format!("{}\n\n{query}", undeliverable_notice())
}

/// True when `event` is the CLI's rejection result for the `AskUserQuestion`
/// tool call that is currently pending.
pub fn is_rejection_of_pending(
    event: &ConversationEvent,
    prior_events: &[ConversationEvent],
    pending: Option<&PendingQuestion>,
) -> bool {
    if pending.is_none() || event.event_type != EventType::ToolResult {
        return false;
    }
    let is_error = event
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get("is_error"))
        .and_then(serde_json::Value::as_bool)
        == Some(true);
    let Some(result_id) = event.tool_use_id.as_deref() else {
        return false;
    };
    is_error
        && prior_events.iter().rev().any(|prior| {
            prior.event_type == EventType::ToolUse
                && prior.tool_name.as_deref() == Some("AskUserQuestion")
                && prior.tool_use_id.as_deref() == Some(result_id)
        })
}

/// Copy of the rejection result annotated with the typed code so the transcript
/// and queries show a pending question, not a decline.
pub fn tag_rejection(event: &ConversationEvent) -> ConversationEvent {
    let mut tagged = event.clone();
    let mut metadata = event
        .metadata
        .as_ref()
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    metadata.insert("rsi_code".into(), UNDELIVERABLE_CODE.into());
    metadata.insert("question_pending".into(), true.into());
    tagged.metadata = Some(Box::new(serde_json::Value::Object(metadata)));
    tagged
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::types::{ConversationEvent, EventType, SessionProvider};

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn test_encode_answer() {
        let ans = encode_answer(SessionProvider::Claude, "Blue");
        assert_eq!(ans, "My answer to your question: Blue");
    }

    fn pending() -> PendingQuestion {
        serde_json::from_value(serde_json::json!({"questions":[{
            "question":"Ship it?","header":"Ship","options":[],"multiSelect":false}]}))
        .unwrap()
    }

    fn ask_use(tool_use_id: &str) -> ConversationEvent {
        let mut event = rejection("ignored", Some(tool_use_id), false);
        event.event_type = EventType::ToolUse;
        event.tool_name = Some("AskUserQuestion".into());
        event
    }

    fn rejection(text: &str, tool_use_id: Option<&str>, is_error: bool) -> ConversationEvent {
        ConversationEvent {
            id: 0,
            session_id: uuid::Uuid::new_v4(),
            sequence: 2,
            event_type: EventType::ToolResult,
            role: None,
            created_at: chrono::Utc::now(),
            content: text.into(),
            tool_name: None,
            tool_input: None,
            offload_id: None,
            tool_use_id: tool_use_id.map(String::from),
            metadata: Some(Box::new(serde_json::json!({ "is_error": is_error }))),
        }
    }

    // Undeliverable path: the continuation carries the documented typed text.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn unanswered_continuation_carries_the_typed_no_human_notice() {
        let query = with_unanswered_notice(Some(&pending()), "keep going".into());
        assert!(query.starts_with("[rsi:ask_user_question_undeliverable] "));
        assert!(query.contains("no human has answered it here. Decide yourself"));
        assert!(query.ends_with("\n\nkeep going"));
    }

    // Deliverable path: a real answer is never prefixed with the notice.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn answered_continuation_and_no_question_are_untouched() {
        assert_eq!(
            with_unanswered_notice(None, "keep going".into()),
            "keep going"
        );
        let answered = encode_answer(SessionProvider::Claude, "Blue");
        assert_eq!(with_unanswered_notice(None, answered.clone()), answered);
        assert!(!answered.contains(UNDELIVERABLE_CODE));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn cli_rejection_of_the_pending_question_is_tagged_with_the_typed_code() {
        let prior = vec![ask_use("toolu_1")];
        let result = rejection("Answer questions?", Some("toolu_1"), true);
        assert!(is_rejection_of_pending(&result, &prior, Some(&pending())));
        let tagged = tag_rejection(&result);
        let metadata = tagged.metadata.expect("typed metadata");
        assert_eq!(metadata["rsi_code"], "ask_user_question_undeliverable");
        assert_eq!(metadata["question_pending"], true);
        assert_eq!(metadata["is_error"], true);
        assert_eq!(tagged.content, "Answer questions?");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn only_the_error_result_of_the_pending_ask_is_a_rejection() {
        let prior = vec![ask_use("toolu_1")];
        let ok = rejection("fine", Some("toolu_1"), false);
        let other = rejection("boom", Some("toolu_2"), true);
        let rejected = rejection("no", Some("toolu_1"), true);
        assert!(!is_rejection_of_pending(&ok, &prior, Some(&pending())));
        assert!(!is_rejection_of_pending(&other, &prior, Some(&pending())));
        assert!(!is_rejection_of_pending(&rejected, &prior, None));
    }
}
