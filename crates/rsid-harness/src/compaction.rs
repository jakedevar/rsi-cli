//! Context compaction: the three tiers and the history-rebuild rules.
//!
//! Tier 1 summarizes through a [`SummaryTransport`]: the daemon implements it with
//! model-call admission, settlement and its provider, so this module holds no
//! authority and sees no credential.

use crate::types::{ChatMessage, ChatRequest, MessageRole};
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

pub fn transcript_line(message: &ChatMessage) -> String {
    let role = match message.role {
        MessageRole::System => "system",
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::Tool => "tool",
    };
    // Image payloads belong in provider history, not the text-only summary.
    let content = if message.role == MessageRole::Tool {
        message.visible_tool_text()
    } else {
        message.content.clone()
    };
    let content = if content.len() > 500 {
        let mut end = 500;
        while !content.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}... [truncated]", &content[..end])
    } else {
        content
    };
    format!("[{role}]: {content}")
}

/// What one auto-compaction replaced (#796). The agent loop persists it as a
/// `Compressed` conversation event so a continued session rebuilds the same
/// summary-plus-tail history instead of the full raw transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionRecord {
    /// The summary that replaced the dropped middle; `None` when the summary
    /// call failed and the middle was only truncated.
    pub summary: Option<String>,
    /// Conversation messages kept verbatim after the summary (the system
    /// prompt is not counted).
    pub kept_messages: usize,
    /// History length, including the system prompt, before compaction.
    pub messages_before: usize,
    /// Estimated history tokens before compaction.
    pub tokens_before: u64,
    /// Why the summary was not produced: `None` for a real summary,
    /// `Some("policy_denied")` / `Some("summary_failed")` when the middle was
    /// only truncated locally (#974).
    pub fallback: Option<String>,
}

/// Number of leading `System` messages (the real system prompt). A resumed
/// history rebuilt from persisted events carries none.
pub fn leading_system_len(history: &[ChatMessage]) -> usize {
    history
        .iter()
        .take_while(|m| m.role == MessageRole::System)
        .count()
}

/// Indices in `history[head..tail_start]` of the messages compaction must
/// never summarize (#1058): the first user message (the task) and the latest
/// user message (the newest instruction, distinct on a continuation).
pub fn pinned_task_indices(history: &[ChatMessage], head: usize, tail_start: usize) -> Vec<usize> {
    let region = head.min(tail_start)..tail_start;
    let users = || {
        region
            .clone()
            .filter(|&idx| history[idx].role == MessageRole::User)
    };
    let mut pinned: Vec<usize> = users().next().into_iter().collect();
    if let Some(last) = users().last()
        && !pinned.contains(&last)
    {
        pinned.push(last);
    }
    pinned
}

/// The messages compaction would summarize: everything between the system
/// prompt and `tail_start` except the pinned task/instruction messages.
pub fn summarizable(history: &[ChatMessage], tail_start: usize) -> Vec<&ChatMessage> {
    let head = leading_system_len(history);
    let pinned = pinned_task_indices(history, head, tail_start);
    (head.min(tail_start)..tail_start)
        .filter(|idx| !pinned.contains(idx))
        .map(|idx| &history[idx])
        .collect()
}

/// Rebuild `history` as system prompt, pinned task messages (verbatim), an
/// optional `marker` (the summary), then the tail from `tail_start`. Every
/// compaction path and the persisted-history rebuild go through this, so the
/// original task survives however often history is compacted (#1058).
/// Orphaned tool results at the tail's front are repaired by
/// `normalize_history` before the next provider request.
pub fn compact_in_place(
    history: &mut Vec<ChatMessage>,
    tail_start: usize,
    marker: Option<ChatMessage>,
) {
    let tail_start = tail_start.min(history.len());
    let head = leading_system_len(history);
    let pinned = pinned_task_indices(history, head, tail_start);
    let mut rebuilt: Vec<ChatMessage> = history[..head.min(tail_start)].to_vec();
    rebuilt.extend(pinned.iter().map(|&idx| history[idx].clone()));
    rebuilt.extend(marker);
    rebuilt.extend(history[tail_start..].iter().cloned());
    *history = rebuilt;
}

/// Drop the summarizable middle, keeping the system prompt, the pinned task
/// messages and the last `keep_recent` messages. Shared by the compaction
/// fallback paths.
pub fn truncate_middle(
    history: &mut Vec<ChatMessage>,
    keep_recent: usize,
    messages_before: usize,
    tokens_before: u64,
    fallback: &str,
) -> CompactionRecord {
    compact_in_place(history, messages_before - keep_recent, None);
    CompactionRecord {
        summary: None,
        kept_messages: keep_recent,
        messages_before,
        tokens_before,
        fallback: Some(fallback.to_string()),
    }
}

/// Live-context size at which Tier 1 compaction fires: 75% of the model's
/// window, or the absolute budget when that is lower.
pub fn compaction_threshold_tokens(token_limit: u64, budget_tokens: Option<u64>) -> u64 {
    let window = (token_limit as f32 * 0.75) as u64;
    budget_tokens.map_or(window, |budget| window.min(budget))
}

/// Message-count backstop for Tier 1 compaction. It sits below `hard_trim`'s
/// 200-message cap so the summarizing path runs first, and far above the old
/// 50: at two messages per tool call, 50 compacted a Harness worker every ~25
/// calls (#1058). Real pacing is token-driven (75% of the window or the #966
/// budget, whichever is lower).
pub const AUTO_COMPACT_MESSAGE_TRIGGER: usize = 150;

/// How one summary call ended, as reported by the daemon's transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SummaryOutcome {
    /// The provider returned this summary text.
    Summary(String),
    /// Model-call policy denied the call (#974): it was never attempted.
    PolicyDenied,
    /// The call was attempted and failed; the transport has settled it.
    Failed(String),
    /// The turn was cancelled while the call was in flight; the transport settled it.
    Interrupted,
}

/// A failure of the transport itself (admission or settlement), not of the summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionError(pub String);

impl std::fmt::Display for CompactionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CompactionError {}

/// Admits, runs and settles one summary call.
#[async_trait]
pub trait SummaryTransport: Send + Sync {
    /// `admission_key` and `transcript` identify the call for admission; `request` is the
    /// chat request to send when admitted.
    async fn summarize(
        &self,
        request: &ChatRequest,
        admission_key: &str,
        transcript: &str,
        cancel: &CancellationToken,
    ) -> Result<SummaryOutcome, CompactionError>;
}

/// Tier 1 — Auto-compaction
/// Triggers at 75% token usage, at the absolute `budget_tokens` when that is
/// lower (#966: an OpenRouter step costs roughly its live context), or at
/// [`AUTO_COMPACT_MESSAGE_TRIGGER`] messages. Keeps the system prompt, the
/// first user task message and the latest user instruction verbatim (#1058),
/// and the last 20 messages, and uses the transport to summarize the rest.
/// Falls back to truncation on error.
/// Returns what it replaced, or `None` when nothing was compacted.
pub async fn auto_compact(
    history: &mut Vec<ChatMessage>,
    transport: &dyn SummaryTransport,
    model: &str,
    token_limit: u64,
    budget_tokens: Option<u64>,
    focus: Option<&str>,
    cancel: &CancellationToken,
) -> Result<Option<CompactionRecord>, CompactionError> {
    let message_count = history.len();
    let total_tokens: u64 = history.iter().map(|m| m.estimated_tokens()).sum();
    let threshold_tokens = compaction_threshold_tokens(token_limit, budget_tokens);

    if focus.is_none()
        && total_tokens < threshold_tokens
        && message_count < AUTO_COMPACT_MESSAGE_TRIGGER
    {
        return Ok(None);
    }

    tracing::info!(
        total_tokens,
        messages = message_count,
        "Auto-compacting conversation history"
    );

    let keep_recent = 20;
    if message_count <= keep_recent + 1 {
        return Ok(None);
    }

    let to_summarize = summarizable(history, message_count - keep_recent);
    if to_summarize.is_empty() {
        return Ok(None);
    }

    let transcript: String = to_summarize
        .iter()
        .map(|m| transcript_line(m))
        .collect::<Vec<_>>()
        .join("\n");

    let summary_instruction = match focus {
        Some(focus) => format!(
            "Summarize this conversation concisely (max 2000 chars). \
             Focus on key decisions, actions taken, and current state. \
             Preserve the following items: {focus}"
        ),
        None => "Summarize this conversation concisely (max 2000 chars). \
                 Focus on key decisions, actions taken, and current state."
            .to_string(),
    };

    let summary_request = ChatRequest {
        messages: vec![
            ChatMessage::system(&summary_instruction),
            ChatMessage::user(&transcript),
        ],
        model: model.to_string(),
        temperature: Some(0.2),
        max_tokens: Some(2000),
        tools: Vec::new(),
        stream: false,
        reasoning_effort: None,
        context_editing: false,
        tool_choice_none: false,
    };

    if cancel.is_cancelled() {
        return Ok(None);
    }
    // #974: a policy denial (e.g. paid background work disabled by default in
    // Normal mode) means the summary call is never attempted; fall back to
    // local truncation so the turn keeps going. Other admission errors
    // propagate.
    let outcome = transport
        .summarize(
            &summary_request,
            &format!("messages:{message_count}:tokens:{total_tokens}"),
            &transcript,
            cancel,
        )
        .await?;

    let summary = match outcome {
        SummaryOutcome::Summary(summary) => summary,
        SummaryOutcome::PolicyDenied => {
            tracing::warn!(
                "Auto-compact summary denied by model-call policy, falling back to truncation"
            );
            return Ok(Some(truncate_middle(
                history,
                keep_recent,
                message_count,
                total_tokens,
                "policy_denied",
            )));
        }
        SummaryOutcome::Interrupted => return Ok(None),
        SummaryOutcome::Failed(error) => {
            tracing::warn!(%error, "Auto-compact LLM summary failed, falling back to truncation");
            // On LLM failure, fall back to local truncation (just drop the middle)
            return Ok(Some(truncate_middle(
                history,
                keep_recent,
                message_count,
                total_tokens,
                "summary_failed",
            )));
        }
    };

    // Replace the summarized middle with a single system summary message; the
    // task and latest instruction stay verbatim ahead of it.
    compact_in_place(
        history,
        message_count - keep_recent,
        Some(ChatMessage::system(&format!(
            "[Conversation summary: {}]",
            summary
        ))),
    );

    tracing::info!(new_len = history.len(), "Auto-compaction complete");
    Ok(Some(CompactionRecord {
        summary: Some(summary),
        kept_messages: keep_recent,
        messages_before: message_count,
        tokens_before: total_tokens,
        fallback: None,
    }))
}

/// Tier 2 — Force compaction
/// Emergency compaction when context is exhausted. Keeps the system prompt,
/// the pinned task messages (#1058) and the last 4 messages, dropping
/// everything in between. No LLM call.
pub fn force_compact(history: &mut Vec<ChatMessage>) {
    if history.len() <= 5 {
        return;
    }
    let tail_start = history.len() - 4;
    compact_in_place(
        history,
        tail_start,
        Some(ChatMessage::user(
            "[Previous conversation truncated due to context exhaustion]",
        )),
    );
}

/// Tier 3 — Hard trim
/// Cap at max_history_messages. Drops oldest messages after the system prompt
/// and the pinned task messages (#1058).
pub fn hard_trim(history: &mut Vec<ChatMessage>, max_messages: usize) {
    if history.len() <= max_messages {
        return;
    }
    // Reserve room for the system prompt and up to two pinned messages.
    let head = leading_system_len(history);
    let tail_len = max_messages.saturating_sub(head + 2).max(1);
    compact_in_place(history, history.len() - tail_len, None);
    history.shrink_to_fit();
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use crate::types::{ToolCall, ToolContentBlock};
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A scripted transport: the next outcome per call, recording each transcript.
    struct ScriptedTransport {
        calls: AtomicUsize,
        transcripts: Mutex<Vec<String>>,
        outcomes: Mutex<VecDeque<SummaryOutcome>>,
    }

    impl ScriptedTransport {
        fn new(outcomes: impl IntoIterator<Item = SummaryOutcome>) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                transcripts: Mutex::new(Vec::new()),
                outcomes: Mutex::new(outcomes.into_iter().collect()),
            }
        }
    }

    #[async_trait]
    impl SummaryTransport for ScriptedTransport {
        async fn summarize(
            &self,
            request: &ChatRequest,
            _admission_key: &str,
            transcript: &str,
            _cancel: &CancellationToken,
        ) -> Result<SummaryOutcome, CompactionError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.transcripts.lock().unwrap().push(transcript.to_owned());
            assert_eq!(request.max_tokens, Some(2000));
            Ok(self
                .outcomes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(SummaryOutcome::PolicyDenied))
        }
    }

    #[test]
    fn compaction_transcript_summarizes_image_without_its_payload() {
        let message = ChatMessage::tool_result_blocks(
            "image-call",
            vec![
                ToolContentBlock::Text {
                    text: "preview".into(),
                },
                ToolContentBlock::Image {
                    media_type: "image/png".into(),
                    data: "aGVsbG8=".into(),
                    detail: None,
                    reference: None,
                    width: None,
                    height: None,
                },
            ],
            false,
        );
        assert_eq!(
            transcript_line(&message),
            "[tool]: preview\n[image: image/png]"
        );
        let unicode = ChatMessage::user(format!("{}é", "x".repeat(499)));
        assert!(transcript_line(&unicode).ends_with("... [truncated]"));
    }

    #[test]
    fn test_hard_trim() {
        let mut history = vec![
            ChatMessage::system("system"),
            ChatMessage::user("msg1"),
            ChatMessage::user("msg2"),
            ChatMessage::user("msg3"),
            ChatMessage::user("msg4"),
            ChatMessage::user("msg5"),
        ];
        hard_trim(&mut history, 5);
        // System prompt, the pinned task (msg1) and latest instruction, tail.
        let contents: Vec<&str> = history.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(history[0].role, MessageRole::System);
        assert_eq!(contents, ["system", "msg1", "msg3", "msg4", "msg5"]);
        assert_eq!(history.capacity(), history.len());
    }

    #[test]
    fn test_force_compact() {
        let mut history = vec![
            ChatMessage::system("system"),
            ChatMessage::user("msg1"),
            ChatMessage::user("msg2"),
            ChatMessage::user("msg3"),
            ChatMessage::user("msg4"),
            ChatMessage::user("msg5"),
            ChatMessage::user("msg6"),
        ];
        force_compact(&mut history);
        // System prompt, task (msg1), latest summarized instruction (msg2),
        // truncation marker, last four.
        assert_eq!(history.len(), 8);
        assert_eq!(history[0].role, MessageRole::System);
        assert_eq!(history[0].content, "system");
        assert_eq!(history[1].content, "msg1");
        assert_eq!(history[2].content, "msg2");
        assert!(history[3].content.contains("truncated"));
        assert_eq!(history[7].content, "msg6");
    }

    #[test]
    fn force_compact_preserves_error_flag_in_retained_history() {
        let mut history = vec![
            ChatMessage::system("system"),
            ChatMessage::user("msg1"),
            ChatMessage::user("msg2"),
            ChatMessage::tool_error_result("call-1", "failed"),
            ChatMessage::user("msg4"),
            ChatMessage::user("msg5"),
            ChatMessage::user("msg6"),
        ];

        force_compact(&mut history);

        let retained_error = history
            .iter()
            .find(|message| message.tool_call_id.as_deref() == Some("call-1"))
            .expect("recent tool error remains in compacted history");
        assert!(retained_error.is_error);
    }

    #[test]
    fn compaction_threshold_is_the_lower_of_the_window_share_and_the_budget() {
        assert_eq!(compaction_threshold_tokens(1_000_000, None), 750_000);
        assert_eq!(
            compaction_threshold_tokens(1_000_000, Some(128_000)),
            128_000
        );
        // A budget above 75% of a small window never raises the trigger.
        assert_eq!(compaction_threshold_tokens(100_000, Some(128_000)), 75_000);
    }

    fn task_history(task: &str, pairs: usize) -> Vec<ChatMessage> {
        task_history_with_ids(task, pairs, task)
    }

    fn task_history_with_ids(task: &str, pairs: usize, id_prefix: &str) -> Vec<ChatMessage> {
        let mut history = vec![ChatMessage::system("system"), ChatMessage::user(task)];
        for idx in 0..pairs {
            let mut call = ChatMessage::assistant(format!("step {idx}"));
            call.tool_calls = vec![ToolCall {
                id: format!("{id_prefix}-{idx}"),
                name: "read".to_string(),
                arguments: "{}".to_string(),
                hosted: false,
                hosted_result: None,
            }];
            history.push(call);
            history.push(ChatMessage::tool_result(
                format!("{id_prefix}-{idx}"),
                format!("result {idx}"),
            ));
        }
        history
    }

    /// #1058: a continuation's latest user instruction survives compaction
    /// verbatim next to the original task.
    #[test]
    fn compaction_keeps_the_latest_instruction_and_the_task() {
        let mut history = task_history("original task", 30);
        history.push(ChatMessage::user("continuation instruction"));
        history.extend(task_history_with_ids("unused", 30, "later").split_off(2));
        let tail_start = history.len() - 20;
        compact_in_place(
            &mut history,
            tail_start,
            Some(ChatMessage::system("[Conversation summary: s]")),
        );
        let contents: Vec<&str> = history.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(
            &contents[..4],
            [
                "system",
                "original task",
                "continuation instruction",
                "[Conversation summary: s]"
            ]
        );
        assert_eq!(history.len(), 4 + 20);

        let mut trimmed = task_history("original task", 60);
        hard_trim(&mut trimmed, 30);
        assert!(trimmed.len() <= 30);
        assert_eq!(trimmed[1].content, "original task");
    }

    /// #1058: the message-count trigger is a high backstop below `hard_trim`'s
    /// cap, not a compaction every ~25 tool calls.
    #[tokio::test]
    async fn message_count_trigger_does_not_fire_at_fifty_messages() {
        let mut history = task_history("task", 30);
        assert!(history.len() > 50 && history.len() < AUTO_COMPACT_MESSAGE_TRIGGER);
        let transport = ScriptedTransport::new([]);
        let compacted = auto_compact(
            &mut history,
            &transport,
            "gpt-5.4",
            1_000_000,
            None,
            None,
            &CancellationToken::new(),
        )
        .await
        .expect("no compaction");
        assert!(compacted.is_none());
        assert_eq!(transport.calls.load(Ordering::SeqCst), 0);
        assert!(
            AUTO_COMPACT_MESSAGE_TRIGGER < 200,
            "must stay below hard_trim"
        );
    }

    /// #966: an OpenRouter-sized window (1M) no longer lets live context grow
    /// to 750k first; compaction is attempted once the budget is crossed.
    #[tokio::test]
    async fn auto_compact_fires_at_the_absolute_budget_below_the_window_share() {
        let mut history = vec![ChatMessage::system("system")];
        for idx in 0..30 {
            history.push(ChatMessage::user(format!(
                "message {idx} {}",
                "x".repeat(4_000)
            )));
        }
        let before = history.len();
        for quiet_budget in [None, Some(64_000)] {
            let transport = ScriptedTransport::new([]);
            let compacted = auto_compact(
                &mut history,
                &transport,
                "deepseek/deepseek-v4.1-flash",
                1_000_000,
                quiet_budget,
                None,
                &CancellationToken::new(),
            )
            .await
            .expect("below the trigger nothing is compacted");
            assert!(compacted.is_none());
            assert_eq!(transport.calls.load(Ordering::SeqCst), 0);
        }
        assert_eq!(history.len(), before);

        let transport = ScriptedTransport::new([SummaryOutcome::PolicyDenied]);
        let record = auto_compact(
            &mut history,
            &transport,
            "deepseek/deepseek-v4.1-flash",
            1_000_000,
            Some(20_000),
            None,
            &CancellationToken::new(),
        )
        .await
        .expect("the budget trigger reaches the summary transport")
        .expect("a denied summary still yields a truncation record");
        assert_eq!(record.summary, None);
        assert_eq!(record.fallback.as_deref(), Some("policy_denied"));
        assert_eq!(record.kept_messages, 20);
        assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn failed_and_interrupted_summaries_fall_back_or_leave_history_alone() {
        let mut failed = task_history("task", 40);
        let transport = ScriptedTransport::new([SummaryOutcome::Failed("boom".into())]);
        let record = auto_compact(
            &mut failed,
            &transport,
            "gpt-5.4",
            1_000_000,
            None,
            Some("force"),
            &CancellationToken::new(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(record.fallback.as_deref(), Some("summary_failed"));
        assert_eq!(record.summary, None);

        let mut interrupted = task_history("task", 40);
        let before = interrupted.len();
        let transport = ScriptedTransport::new([SummaryOutcome::Interrupted]);
        let record = auto_compact(
            &mut interrupted,
            &transport,
            "gpt-5.4",
            1_000_000,
            None,
            Some("force"),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(record.is_none());
        assert_eq!(interrupted.len(), before);
    }

    /// #1058: however many times history is compacted, the first user task
    /// message stays verbatim right after the system prompt, and the summary
    /// never carries it back through the summarizer as transcript.
    #[tokio::test]
    async fn forced_compactions_keep_the_original_task_verbatim() {
        let task = "Review commits abc123 and def456; report two findings.";
        for summarize in [false, true] {
            let mut history = task_history(task, 40);
            let transport = ScriptedTransport::new([]);
            for round in 0..6 {
                history
                    .extend(task_history_with_ids("unused", 30, &format!("r{round}")).split_off(2));
                transport.outcomes.lock().unwrap().push_back(if summarize {
                    SummaryOutcome::Summary(format!("summary {round}"))
                } else {
                    SummaryOutcome::PolicyDenied
                });
                let record = auto_compact(
                    &mut history,
                    &transport,
                    "gpt-5.4",
                    1_000_000,
                    None,
                    Some("force a compaction"),
                    &CancellationToken::new(),
                )
                .await
                .unwrap()
                .unwrap();
                assert_eq!(record.summary.is_some(), summarize);
                assert_eq!(history[0].content, "system");
                assert_eq!(history[1].role, MessageRole::User);
                assert_eq!(history[1].content, task, "round {round}");
                let transcripts = transport.transcripts.lock().unwrap();
                assert!(!transcripts.last().unwrap().contains(task));
            }
        }
    }
}
