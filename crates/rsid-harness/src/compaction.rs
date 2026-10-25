//! Context compaction: the three tiers and the history-rebuild rules.
//!
//! Tier 1 summarizes through a [`SummaryTransport`]: the daemon implements it with
//! model-call admission, settlement and its provider, so this module holds no
//! authority and sees no credential.

use crate::types::{ChatMessage, ChatRequest, MessageRole};
use async_trait::async_trait;
pub use rsi_common::compaction_policy::CompactionPolicy;
use rsi_common::compaction_policy::{
    DEFAULT_MESSAGE_TRIGGER, SUMMARY_INSTRUCTION_MAX_TOKENS_PLACEHOLDER,
};
use tokio_util::sync::CancellationToken;

/// Longest tool result a transcript line keeps (#3167). A command's exit code
/// and error lines sit at the end of its output, so an over-long result keeps
/// its head and tail plus the salient lines cut from the middle.
pub const TRANSCRIPT_TOOL_RESULT_CHARS: usize = 3_000;
/// Longest user or assistant text a transcript line keeps.
pub const TRANSCRIPT_TEXT_CHARS: usize = 1_500;
/// Longest tool-call argument string a transcript line keeps. The path a call
/// touches leads its arguments.
pub const TRANSCRIPT_TOOL_ARGS_CHARS: usize = 400;
/// Most salient lines recovered from a cut tool result's middle.
const SALIENT_LINES_MAX: usize = 12;
const SALIENT_LINE_CHARS: usize = 240;

fn is_salient_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    ["error", "exit", "fail", "panic", "denied", "not found"]
        .iter()
        .any(|needle| lower.contains(needle))
}

fn truncate_chars(text: &str, max: usize) -> &str {
    &text[..char_floor(text, max)]
}

/// Cut `text` to about `max` bytes: its head and tail, with the salient
/// (error/exit/failure) lines of the cut middle listed between them.
fn truncate_keep_ends(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let head_end = char_floor(text, max / 2);
    let tail_start = char_ceil(text, text.len().saturating_sub(max / 3)).max(head_end);
    let middle = &text[head_end..tail_start];
    let salient: Vec<&str> = middle
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && is_salient_line(line))
        .take(SALIENT_LINES_MAX)
        .map(|line| truncate_chars(line, SALIENT_LINE_CHARS))
        .collect();
    let mut out = String::with_capacity(max + 64);
    out.push_str(&text[..head_end]);
    out.push_str(&format!("\n... [{} bytes cut", middle.len()));
    if salient.is_empty() {
        out.push_str("] ...\n");
    } else {
        out.push_str("; salient lines:\n");
        out.push_str(&salient.join("\n"));
        out.push_str("\n] ...\n");
    }
    out.push_str(&text[tail_start..]);
    out
}

pub fn transcript_line(message: &ChatMessage) -> String {
    let role = match message.role {
        MessageRole::System => "system",
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::Tool => "tool",
    };
    // Image payloads belong in provider history, not the text-only summary.
    let (content, cap) = if message.role == MessageRole::Tool {
        (message.visible_tool_text(), TRANSCRIPT_TOOL_RESULT_CHARS)
    } else {
        (message.content.clone(), TRANSCRIPT_TEXT_CHARS)
    };
    let mut line = format!("[{role}]: {}", truncate_keep_ends(&content, cap));
    // The files an assistant touched and the commands it ran live in its tool
    // calls' arguments, not in its text (#3167).
    for call in &message.tool_calls {
        let args = truncate_chars(&call.arguments, TRANSCRIPT_TOOL_ARGS_CHARS);
        let cut = if args.len() < call.arguments.len() {
            "... [truncated]"
        } else {
            ""
        };
        line.push_str(&format!("\n[tool_call {}]: {args}{cut}", call.name));
    }
    line
}

/// Output budget of the summary call (#3167), derived from the compaction
/// threshold so a larger context budget buys a larger summary: 1/16 of the
/// threshold, between 3000 and 8000 tokens.
pub fn summary_max_tokens(token_limit: u64, budget_tokens: Option<u64>) -> u32 {
    summary_max_tokens_for(&CompactionPolicy::default(), token_limit, budget_tokens)
}

/// [`summary_max_tokens`] under `policy`: its `summary_tokens` when set, else the
/// value derived from the policy's threshold (#3199).
pub fn summary_max_tokens_for(
    policy: &CompactionPolicy,
    token_limit: u64,
    budget_tokens: Option<u64>,
) -> u32 {
    policy.summary_tokens.unwrap_or_else(|| {
        let threshold = policy.threshold_tokens(token_limit, budget_tokens);
        (threshold / 16).clamp(3_000, 8_000) as u32
    })
}

/// The summary call's system prompt: a fixed structure so the facts an agent
/// needs to continue survive (#3167).
pub fn summary_instruction(max_tokens: u32, focus: Option<&str>) -> String {
    summary_instruction_for(&CompactionPolicy::default(), max_tokens, focus)
}

/// [`summary_instruction`] under `policy`: its `summary_instruction` replaces the
/// built-in text (`{max_tokens}` in it names the summary length), and the focus
/// items are appended either way (#3199).
pub fn summary_instruction_for(
    policy: &CompactionPolicy,
    max_tokens: u32,
    focus: Option<&str>,
) -> String {
    let mut text = match &policy.summary_instruction {
        Some(custom) => custom.replace(
            SUMMARY_INSTRUCTION_MAX_TOKENS_PLACEHOLDER,
            &max_tokens.to_string(),
        ),
        None => default_summary_instruction(max_tokens),
    };
    if let Some(focus) = focus {
        text.push_str(&format!("\nPreserve the following items: {focus}"));
    }
    text
}

fn default_summary_instruction(max_tokens: u32) -> String {
    format!(
        "You are compacting the middle of a long agentic coding session so the \
         agent can continue it. Write a structured summary of at most about {max_tokens} \
         tokens, with exactly these sections, using bullet points and keeping exact \
         paths, commands, exit codes and error messages:\n\
         ## Files touched\nEvery file created, edited, or deleted, with what changed.\n\
         ## Commands run\nEach notable command with its outcome (exit code, key output).\n\
         ## Open errors and failed attempts\nErrors still unresolved, and approaches already tried that failed, so they are not repeated.\n\
         ## Decisions and findings\nKey decisions and facts learned about the code.\n\
         ## Plan and remaining steps\nThe current plan, what is done, and the concrete next steps.\n\
         Write \"none\" under a section with nothing to report."
    )
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
    CompactionPolicy::default().threshold_tokens(token_limit, budget_tokens)
}

/// Message-count backstop for Tier 1 compaction. It sits below `hard_trim`'s
/// 200-message cap so the summarizing path runs first, and far above the old
/// 50: at two messages per tool call, 50 compacted a Harness worker every ~25
/// calls (#1058). Raised from 150 (#3167): 150 messages is about 75 tool calls,
/// so it fired well before the token budget on a worker with the default
/// 150-iteration cap. Real pacing is token-driven (75% of the window or the
/// #966 budget, whichever is lower); this only guards a history of many tiny
/// messages from reaching `hard_trim`, which drops the middle unsummarized.
pub const AUTO_COMPACT_MESSAGE_TRIGGER: usize = DEFAULT_MESSAGE_TRIGGER;

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
    auto_compact_measured(
        history,
        transport,
        model,
        token_limit,
        budget_tokens,
        focus,
        &ContextMeasure::default(),
        cancel,
    )
    .await
}

/// What the loop knows about the live context besides the message text
/// (#3166). The trigger prefers the provider's own count of its last prompt
/// (it includes tool-call arguments and the tool specs) over our estimate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ContextMeasure {
    /// Full prompt tokens the provider reported for its last call; 0 when it
    /// reported none or the history changed under it (after a compaction).
    pub reported_prompt_tokens: u64,
    /// History length of the request that call answered. Messages at or after
    /// this index were appended since.
    pub reported_history_len: usize,
    /// Estimated tokens of the tool specs every request carries; used only by
    /// the fallback estimate, since a reported count already includes them.
    pub tool_spec_tokens: u64,
}

impl ContextMeasure {
    /// Live-context tokens of `history`: the provider-reported prompt tokens
    /// plus an estimate for the messages appended since that call, or, with no
    /// usable report, the estimate of every message plus the tool specs.
    pub fn live_tokens(&self, history: &[ChatMessage]) -> u64 {
        if self.reported_prompt_tokens > 0 && history.len() >= self.reported_history_len {
            let appended: u64 = history[self.reported_history_len..]
                .iter()
                .map(ChatMessage::estimated_tokens)
                .sum();
            return self.reported_prompt_tokens.saturating_add(appended);
        }
        let messages: u64 = history.iter().map(ChatMessage::estimated_tokens).sum();
        messages.saturating_add(self.tool_spec_tokens)
    }

    /// Forget the provider report: the history it described was rewritten.
    pub fn forget_report(&mut self) {
        self.reported_prompt_tokens = 0;
        self.reported_history_len = 0;
    }
}

/// [`auto_compact`] with the live-context [`ContextMeasure`] the agent loop
/// keeps from its provider calls.
#[allow(clippy::too_many_arguments)]
pub async fn auto_compact_measured(
    history: &mut Vec<ChatMessage>,
    transport: &dyn SummaryTransport,
    model: &str,
    token_limit: u64,
    budget_tokens: Option<u64>,
    focus: Option<&str>,
    measure: &ContextMeasure,
    cancel: &CancellationToken,
) -> Result<Option<CompactionRecord>, CompactionError> {
    auto_compact_with_policy(
        history,
        transport,
        model,
        token_limit,
        budget_tokens,
        focus,
        measure,
        &CompactionPolicy::default(),
        cancel,
    )
    .await
}

/// [`auto_compact_measured`] under a [`CompactionPolicy`] (#3199): the policy's
/// trigger, kept messages, window share and summary request replace the
/// built-in values. The default policy is the built-in behaviour.
#[allow(clippy::too_many_arguments)]
pub async fn auto_compact_with_policy(
    history: &mut Vec<ChatMessage>,
    transport: &dyn SummaryTransport,
    model: &str,
    token_limit: u64,
    budget_tokens: Option<u64>,
    focus: Option<&str>,
    measure: &ContextMeasure,
    policy: &CompactionPolicy,
    cancel: &CancellationToken,
) -> Result<Option<CompactionRecord>, CompactionError> {
    let message_count = history.len();
    let total_tokens = measure.live_tokens(history);
    let threshold_tokens = policy.threshold_tokens(token_limit, budget_tokens);

    if focus.is_none() && total_tokens < threshold_tokens && message_count < policy.message_trigger
    {
        return Ok(None);
    }

    tracing::info!(
        total_tokens,
        messages = message_count,
        "Auto-compacting conversation history"
    );

    let keep_recent = policy.keep_recent;
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

    let summary_tokens = summary_max_tokens_for(policy, token_limit, budget_tokens);
    let summary_instruction = summary_instruction_for(policy, summary_tokens, focus);

    let summary_request = ChatRequest {
        messages: vec![
            ChatMessage::system(&summary_instruction),
            ChatMessage::user(&transcript),
        ],
        model: model.to_string(),
        temperature: Some(0.2),
        max_tokens: Some(summary_tokens),
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

/// Focus handed to the summary call when the provider rejected a request as
/// over the context window (#3164).
const OVERFLOW_COMPACTION_FOCUS: &str =
    "the original task, the latest instruction, files changed, and the current state of the work";

/// Compacts after a provider `context_length_exceeded` (#3164). Always
/// compacts, whatever the size estimate says: the provider's count beats ours.
/// The summarizing tier runs first; when it cannot shrink the history (too
/// short, or its summary call failed or was denied and fell back to keeping 20
/// messages) the emergency tier ([`force_compact`]) drops down to the last 4.
/// Returns `None` when nothing could be removed, so a retry would only
/// overflow again.
///
/// The tail kept by either tier is verbatim, so a huge recent tool result
/// would survive and overflow the retry too. After compaction every tool
/// result over [`OVERFLOW_TOOL_OUTPUT_MAX_CHARS`] is cut to its head and tail
/// ([`shrink_oversized_tool_results`]); that alone is a retryable shrink for a
/// history too short to compact (reported with fallback
/// `tool_output_truncated`).
pub async fn compact_after_overflow(
    history: &mut Vec<ChatMessage>,
    transport: &dyn SummaryTransport,
    model: &str,
    token_limit: u64,
    budget_tokens: Option<u64>,
    cancel: &CancellationToken,
) -> Result<Option<CompactionRecord>, CompactionError> {
    let messages_before = history.len();
    let tokens_before: u64 = history.iter().map(|m| m.estimated_tokens()).sum();
    let record = compact_tiers_after_overflow(
        history,
        transport,
        model,
        token_limit,
        budget_tokens,
        cancel,
        messages_before,
        tokens_before,
    )
    .await?;
    if cancel.is_cancelled() {
        return Ok(record);
    }
    let shrunk = shrink_oversized_tool_results(history, OVERFLOW_TOOL_OUTPUT_MAX_CHARS);
    if record.is_none() && shrunk > 0 {
        return Ok(Some(CompactionRecord {
            summary: None,
            kept_messages: history.len() - leading_system_len(history),
            messages_before,
            tokens_before,
            fallback: Some("tool_output_truncated".to_string()),
        }));
    }
    Ok(record)
}

/// Longest tool result kept whole on the overflow-retry path.
pub const OVERFLOW_TOOL_OUTPUT_MAX_CHARS: usize = 8_000;

/// Cuts every tool result longer than `max_chars` to its head and tail around
/// a marker, as plain text (typed blocks such as images are dropped from the
/// retained copy). Returns how many messages shrank.
pub fn shrink_oversized_tool_results(history: &mut [ChatMessage], max_chars: usize) -> usize {
    let mut shrunk = 0;
    for message in history.iter_mut() {
        if message.role != MessageRole::Tool || message.content.len() <= max_chars {
            continue;
        }
        let text = message.visible_tool_text();
        let head_len = char_floor(&text, max_chars / 2);
        let tail_start = char_ceil(&text, text.len().saturating_sub(max_chars / 4));
        let cut = if head_len < tail_start {
            format!(
                "{}\n[... {} bytes of tool output truncated after a context overflow ...]\n{}",
                &text[..head_len],
                tail_start - head_len,
                &text[tail_start..]
            )
        } else {
            text
        };
        if cut.len() < message.content.len() {
            message.content = cut;
            shrunk += 1;
        }
    }
    shrunk
}

fn char_floor(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn char_ceil(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

#[allow(clippy::too_many_arguments)]
async fn compact_tiers_after_overflow(
    history: &mut Vec<ChatMessage>,
    transport: &dyn SummaryTransport,
    model: &str,
    token_limit: u64,
    budget_tokens: Option<u64>,
    cancel: &CancellationToken,
    messages_before: usize,
    tokens_before: u64,
) -> Result<Option<CompactionRecord>, CompactionError> {
    let record = auto_compact(
        history,
        transport,
        model,
        token_limit,
        budget_tokens,
        Some(OVERFLOW_COMPACTION_FOCUS),
        cancel,
    )
    .await?;
    if cancel.is_cancelled() || record.as_ref().is_some_and(|r| r.fallback.is_none()) {
        return Ok(record);
    }
    let len_after_summary = history.len();
    force_compact(history);
    if history.len() >= len_after_summary {
        return Ok(record);
    }
    Ok(Some(CompactionRecord {
        summary: None,
        kept_messages: 4,
        messages_before,
        tokens_before,
        fallback: Some("force_compact".to_string()),
    }))
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
        requests: Mutex<Vec<ChatRequest>>,
        outcomes: Mutex<VecDeque<SummaryOutcome>>,
    }

    impl ScriptedTransport {
        fn new(outcomes: impl IntoIterator<Item = SummaryOutcome>) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                transcripts: Mutex::new(Vec::new()),
                requests: Mutex::new(Vec::new()),
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
            let max_tokens = request.max_tokens.expect("a summary output budget");
            assert!((3_000..=8_000).contains(&max_tokens), "{max_tokens}");
            self.requests.lock().unwrap().push(request.clone());
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
        // A cut on a multi-byte character never splits it.
        let unicode = ChatMessage::user(format!(
            "{}é{}",
            "x".repeat(TRANSCRIPT_TEXT_CHARS / 2 - 1),
            "y".repeat(5_000)
        ));
        assert!(transcript_line(&unicode).contains("bytes cut"));
    }

    #[test]
    fn summary_budget_scales_with_the_context_budget_and_stays_bounded() {
        assert_eq!(summary_max_tokens(1_000_000, Some(128_000)), 8_000);
        assert_eq!(summary_max_tokens(1_000_000, Some(64_000)), 4_000);
        assert_eq!(summary_max_tokens(1_000_000, Some(8_000)), 3_000);
        assert_eq!(summary_max_tokens(1_000_000, None), 8_000);
        assert!(summary_max_tokens(200_000, Some(128_000)) > 2_000);
    }

    #[test]
    fn transcript_line_keeps_the_tail_and_salient_lines_of_a_long_tool_result() {
        let mut output = String::from("Compiling rsid v0.1.0\n");
        for idx in 0..400 {
            output.push_str(&format!("   Compiling dep{idx} v1.0.{idx}\n"));
            if idx == 200 {
                output.push_str("error[E0308]: mismatched types in src/lib.rs:42\n");
            }
        }
        output.push_str("test result: FAILED. 3 passed; 1 failed\nexit code: 101");
        let line = transcript_line(&ChatMessage::tool_result("c", output));
        assert!(
            line.len() < TRANSCRIPT_TOOL_RESULT_CHARS + 600,
            "{}",
            line.len()
        );
        assert!(line.ends_with("exit code: 101"));
        assert!(line.contains("error[E0308]: mismatched types in src/lib.rs:42"));
        assert!(line.starts_with("[tool]: Compiling rsid"));
    }

    #[test]
    fn transcript_line_carries_tool_call_arguments_so_touched_paths_survive() {
        let mut call = ChatMessage::assistant("patching");
        call.tool_calls = vec![ToolCall {
            id: "c1".into(),
            name: "apply_patch".into(),
            arguments: format!(
                "*** Update File: crates/foo/src/bar.rs\n{}",
                "+x\n".repeat(1_000)
            ),
            hosted: false,
            hosted_result: None,
        }];
        let line = transcript_line(&call);
        assert!(line.contains("[tool_call apply_patch]: *** Update File: crates/foo/src/bar.rs"));
        assert!(line.ends_with("... [truncated]"));
    }

    /// #3167 offline evaluation, no paid calls: a recorded-shape long session
    /// with a fixed checklist of facts. Every fact must reach the summarizer
    /// (the transcript and the structured instruction asking for it), and the
    /// summary must land in the rebuilt history next to the pinned task.
    #[tokio::test]
    async fn compaction_checklist_facts_reach_the_summarizer() {
        let mut history = vec![
            ChatMessage::system("system"),
            ChatMessage::user("Fix the flaky retry in crates/net/src/retry.rs"),
        ];
        let push_call = |history: &mut Vec<ChatMessage>,
                         idx: usize,
                         name: &str,
                         args: String,
                         out: String,
                         err: bool| {
            let mut assistant = ChatMessage::assistant(format!("step {idx}"));
            assistant.tool_calls = vec![ToolCall {
                id: format!("c{idx}"),
                name: name.into(),
                arguments: args,
                hosted: false,
                hosted_result: None,
            }];
            history.push(assistant);
            history.push(if err {
                ChatMessage::tool_error_result(format!("c{idx}"), out)
            } else {
                ChatMessage::tool_result(format!("c{idx}"), out)
            });
        };
        // Checklist: an edited path, a command with its exit code deep in long
        // output, an error line in the middle of long output, a failed attempt.
        push_call(
            &mut history,
            0,
            "apply_patch",
            "{\"path\":\"crates/net/src/retry.rs\",\"patch\":\"backoff\"}".into(),
            "ok".into(),
            false,
        );
        let mut long = "noise line\n".repeat(600);
        long.push_str("thread 'retry::tests::backs_off' panicked at retry.rs:88\n");
        long.push_str(&"more noise\n".repeat(600));
        long.push_str("test result: FAILED. exit code: 101");
        push_call(
            &mut history,
            1,
            "bash",
            "{\"command\":\"cargo test -p net --lib\"}".into(),
            long,
            true,
        );
        push_call(
            &mut history,
            2,
            "bash",
            "{\"command\":\"cargo clippy -p net\"}".into(),
            "tried approach A: lock-free retry failed".into(),
            true,
        );
        for idx in 3..60 {
            push_call(
                &mut history,
                idx,
                "read",
                format!("{{\"path\":\"f{idx}.rs\"}}"),
                format!("contents {idx}"),
                false,
            );
        }
        let facts = [
            "crates/net/src/retry.rs",
            "cargo test -p net --lib",
            "panicked at retry.rs:88",
            "exit code: 101",
            "lock-free retry failed",
        ];
        // The scripted summary is what a faithful summarizer writes from that
        // transcript: it carries every checklist fact (#3447), so the test pins
        // that a fact in the summary survives into the record and the rebuilt
        // history, not only that the summarizer was shown it.
        let scripted = format!(
            "## Files touched\n- {}: backoff patch\n\
             ## Commands run\n- `{}`: {}\n\
             ## Open errors and failed attempts\n- {}\n- {}\n",
            facts[0], facts[1], facts[3], facts[2], facts[4]
        );
        let transport = ScriptedTransport::new([SummaryOutcome::Summary(scripted.clone())]);
        let record = auto_compact(
            &mut history,
            &transport,
            "gpt-5.4",
            1_000_000,
            Some(128_000),
            Some("keep the retry task"),
            &CancellationToken::new(),
        )
        .await
        .unwrap()
        .expect("compacted");
        let summary = record.summary.as_deref().expect("a summary");
        assert_eq!(summary, scripted);
        for fact in facts {
            assert!(summary.contains(fact), "record.summary lost {fact:?}");
            assert!(
                history[2].content.contains(fact),
                "rebuilt history lost {fact:?}"
            );
        }
        let transcripts = transport.transcripts.lock().unwrap();
        for fact in facts {
            assert!(transcripts[0].contains(fact), "transcript lost {fact:?}");
        }
        let requests = transport.requests.lock().unwrap();
        let instruction = &requests[0].messages[0].content;
        for section in [
            "Files touched",
            "Commands run",
            "Open errors",
            "Plan and remaining steps",
        ] {
            assert!(instruction.contains(section), "instruction lacks {section}");
        }
        assert!(instruction.contains("keep the retry task"));
        assert_eq!(
            history[1].content,
            "Fix the flaky retry in crates/net/src/retry.rs"
        );
        assert!(
            history[2]
                .content
                .starts_with(&format!("[Conversation summary: {scripted}")),
            "{}",
            history[2].content
        );
    }

    fn long_history(messages: usize) -> Vec<ChatMessage> {
        let mut history = vec![ChatMessage::system("system"), ChatMessage::user("the task")];
        while history.len() < messages {
            history.push(ChatMessage::assistant(format!("step {}", history.len())));
        }
        history
    }

    /// #3199: the default policy is the pre-policy behaviour: the same trigger
    /// (180 messages), the same 20 kept messages, the same instruction and the
    /// same summary budget, whichever entry point compacts.
    #[tokio::test]
    async fn the_default_policy_is_the_built_in_behaviour() {
        assert_eq!(AUTO_COMPACT_MESSAGE_TRIGGER, 180);
        assert_eq!(
            summary_instruction_for(&CompactionPolicy::default(), 4_000, Some("x")),
            summary_instruction(4_000, Some("x"))
        );
        assert_eq!(
            summary_max_tokens_for(&CompactionPolicy::default(), 1_000_000, Some(128_000)),
            summary_max_tokens(1_000_000, Some(128_000))
        );
        let run = |len: usize, policy: Option<CompactionPolicy>| async move {
            let mut history = long_history(len);
            let transport = ScriptedTransport::new([SummaryOutcome::Summary("S".into())]);
            let record = match policy {
                None => {
                    auto_compact_measured(
                        &mut history,
                        &transport,
                        "m",
                        1_000_000,
                        None,
                        None,
                        &ContextMeasure::default(),
                        &CancellationToken::new(),
                    )
                    .await
                }
                Some(policy) => {
                    auto_compact_with_policy(
                        &mut history,
                        &transport,
                        "m",
                        1_000_000,
                        None,
                        None,
                        &ContextMeasure::default(),
                        &policy,
                        &CancellationToken::new(),
                    )
                    .await
                }
            }
            .unwrap();
            (record, history)
        };
        let (none, _) = run(179, None).await;
        assert_eq!(none, None);
        let (old, old_history) = run(180, None).await;
        let (new, new_history) = run(180, Some(CompactionPolicy::default())).await;
        assert_eq!(old, new);
        let texts = |history: &[ChatMessage]| -> Vec<String> {
            history.iter().map(|m| m.content.clone()).collect()
        };
        assert_eq!(texts(&old_history), texts(&new_history));
        assert_eq!(old.expect("compacted").kept_messages, 20);
    }

    /// #3199: each policy field changes the behaviour it names.
    #[tokio::test]
    async fn a_policy_changes_the_trigger_the_kept_tail_and_the_summary_request() {
        let policy = CompactionPolicy {
            message_trigger: 60,
            keep_recent: 8,
            summary_tokens: Some(4_321),
            summary_instruction: Some("Summarize in {max_tokens} tokens.".into()),
            ..CompactionPolicy::default()
        };
        let compact = |len: usize, focus: Option<&'static str>| {
            let policy = policy.clone();
            async move {
                let mut history = long_history(len);
                let transport = ScriptedTransport::new([SummaryOutcome::Summary("S".into())]);
                let record = auto_compact_with_policy(
                    &mut history,
                    &transport,
                    "m",
                    1_000_000,
                    None,
                    focus,
                    &ContextMeasure::default(),
                    &policy,
                    &CancellationToken::new(),
                )
                .await
                .unwrap();
                let request = transport.requests.lock().unwrap().first().cloned();
                (record, history, request)
            }
        };
        // Below the lowered trigger nothing compacts; at it, it does.
        assert_eq!(compact(59, None).await.0, None);
        let (record, history, request) = compact(60, Some("keep X")).await;
        assert_eq!(compact(60, None).await.0.map(|r| r.kept_messages), Some(8));
        let record = record.expect("compacted at the policy trigger");
        assert_eq!(record.kept_messages, 8);
        // system, the task, the summary, then the 8 kept messages.
        assert_eq!(history.len(), 1 + 1 + 1 + 8);
        let request = request.expect("a summary request");
        assert_eq!(request.max_tokens, Some(4_321));
        assert_eq!(
            request.messages[0].content,
            "Summarize in 4321 tokens.\nPreserve the following items: keep X"
        );
    }

    /// #3199: the window share moves the token threshold.
    #[tokio::test]
    async fn the_window_share_moves_the_token_threshold() {
        let big = "x".repeat(40_000);
        let history = || {
            let mut history = long_history(30);
            history.push(ChatMessage::assistant(&big));
            history
        };
        let run = |percent: u32| {
            let mut history = history();
            async move {
                let policy = CompactionPolicy {
                    window_percent: percent,
                    ..CompactionPolicy::default()
                };
                let transport = ScriptedTransport::new([SummaryOutcome::Summary("S".into())]);
                auto_compact_with_policy(
                    &mut history,
                    &transport,
                    "m",
                    20_000,
                    None,
                    None,
                    &ContextMeasure::default(),
                    &policy,
                    &CancellationToken::new(),
                )
                .await
                .unwrap()
            }
        };
        // ~10k estimated tokens: 75% of 20k = 15k does not fire, 25% = 5k does.
        assert_eq!(run(75).await, None);
        assert!(run(25).await.is_some());
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
    fn shrink_oversized_tool_results_keeps_head_and_tail_and_leaves_small_results() {
        let huge = format!("HEAD{}TAIL", "x".repeat(50_000));
        let mut history = vec![
            ChatMessage::user("task"),
            ChatMessage::tool_result("big", huge.clone()),
            ChatMessage::tool_error_result("small", "short failure"),
        ];

        let shrunk = shrink_oversized_tool_results(&mut history, 1_000);

        assert_eq!(shrunk, 1);
        assert!(history[1].content.len() < 2_000);
        assert!(history[1].content.starts_with("HEAD"));
        assert!(history[1].content.ends_with("TAIL"));
        assert!(
            history[1]
                .content
                .contains("truncated after a context overflow")
        );
        assert_eq!(history[1].tool_call_id.as_deref(), Some("big"));
        assert_eq!(history[2].content, "short failure");
        assert!(history[2].is_error);
        assert_eq!(shrink_oversized_tool_results(&mut history, 1_000), 0);
    }

    #[test]
    fn shrink_oversized_tool_results_cuts_on_char_boundaries() {
        let mut history = vec![ChatMessage::tool_result("big", "é".repeat(5_000))];
        assert_eq!(shrink_oversized_tool_results(&mut history, 1_001), 1);
        assert!(history[0].content.len() < 5_000);
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

    /// #3170: compaction drops the reasoning of the messages it summarizes; only
    /// the kept tail still carries its own.
    #[test]
    fn compaction_drops_the_reasoning_of_summarized_messages() {
        let mut history = task_history("original task", 30);
        for message in history
            .iter_mut()
            .filter(|m| m.role == MessageRole::Assistant)
        {
            message.reasoning = Some(format!("thinking about {}", message.content));
        }
        let tail_start = history.len() - 20;
        compact_in_place(
            &mut history,
            tail_start,
            Some(ChatMessage::system("[Conversation summary: s]")),
        );
        let kept: Vec<&str> = history
            .iter()
            .filter_map(|m| m.reasoning.as_deref())
            .collect();
        assert_eq!(kept.len(), 10, "only the 10 assistant steps in the tail");
        assert!(
            kept.iter()
                .all(|text| text.starts_with("thinking about step "))
        );
        assert!(!kept.contains(&"thinking about step 0"));
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

    /// An edit-heavy history: short content, large `apply_patch` arguments.
    fn edit_heavy_history(calls: usize, patch_bytes: usize) -> Vec<ChatMessage> {
        let mut history = vec![ChatMessage::system("system"), ChatMessage::user("task")];
        for idx in 0..calls {
            let mut assistant = ChatMessage::assistant("");
            assistant.tool_calls.push(ToolCall {
                id: format!("call-{idx}"),
                name: "apply_patch".into(),
                arguments: "p".repeat(patch_bytes),
                hosted: false,
                hosted_result: None,
            });
            history.push(assistant);
            history.push(ChatMessage::tool_result(format!("call-{idx}"), "ok"));
        }
        history
    }

    /// #3166: tool-call arguments count toward the trigger, so an edit-heavy
    /// history with tiny text compacts at the budget.
    #[tokio::test]
    async fn auto_compact_counts_tool_call_arguments_at_the_budget() {
        // 40 calls x 4000 bytes = ~40k tokens in arguments, ~0 in content.
        let mut history = edit_heavy_history(40, 4_000);
        let content_only: u64 = history
            .iter()
            .map(|m| (m.content.len() as u64).div_ceil(4))
            .sum();
        assert!(content_only < 100, "content estimate {content_only}");
        assert!(history.len() < AUTO_COMPACT_MESSAGE_TRIGGER);

        let transport = ScriptedTransport::new([]);
        let quiet = auto_compact(
            &mut history,
            &transport,
            "gpt-5.4",
            1_000_000,
            Some(64_000),
            None,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(quiet.is_none(), "below a 64k budget");
        assert_eq!(transport.calls.load(Ordering::SeqCst), 0);

        let transport = ScriptedTransport::new([SummaryOutcome::Summary("s".into())]);
        let record = auto_compact(
            &mut history,
            &transport,
            "gpt-5.4",
            1_000_000,
            Some(30_000),
            None,
            &CancellationToken::new(),
        )
        .await
        .unwrap()
        .expect("tool-call arguments over the budget compact");
        assert_eq!(record.summary.as_deref(), Some("s"));
        assert!(record.tokens_before >= 40_000, "{}", record.tokens_before);
    }

    /// #3166: the provider-reported prompt tokens trigger compaction even
    /// when the text estimate is far below the threshold, and messages
    /// appended since that call are added on top.
    #[tokio::test]
    async fn auto_compact_trusts_provider_reported_prompt_tokens() {
        let mut history = task_history("task", 30);
        let estimate: u64 = history.iter().map(ChatMessage::estimated_tokens).sum();
        assert!(estimate < 1_000, "text estimate {estimate}");
        let reported_len = history.len() - 2;

        let transport = ScriptedTransport::new([]);
        let under = ContextMeasure {
            reported_prompt_tokens: 40_000,
            reported_history_len: reported_len,
            tool_spec_tokens: 0,
        };
        let none = auto_compact_measured(
            &mut history,
            &transport,
            "gpt-5.4",
            1_000_000,
            Some(50_000),
            None,
            &under,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(none.is_none(), "reported 40k is under the 50k budget");
        assert_eq!(transport.calls.load(Ordering::SeqCst), 0);

        let transport = ScriptedTransport::new([SummaryOutcome::Summary("s".into())]);
        let over = ContextMeasure {
            reported_prompt_tokens: 60_000,
            ..under
        };
        let record = auto_compact_measured(
            &mut history,
            &transport,
            "gpt-5.4",
            1_000_000,
            Some(50_000),
            None,
            &over,
            &CancellationToken::new(),
        )
        .await
        .unwrap()
        .expect("reported tokens over the budget compact");
        assert!(record.tokens_before >= 60_000);
        assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn context_measure_adds_appended_messages_and_falls_back_without_a_report() {
        let mut history = vec![ChatMessage::system("system"), ChatMessage::user("task")];
        let measure = ContextMeasure {
            reported_prompt_tokens: 10_000,
            reported_history_len: history.len(),
            tool_spec_tokens: 500,
        };
        assert_eq!(measure.live_tokens(&history), 10_000);
        history.push(ChatMessage::user("x".repeat(400)));
        assert_eq!(measure.live_tokens(&history), 10_100);

        // A shorter history than the report described invalidates it.
        let shrunk = &history[..1];
        assert_eq!(measure.live_tokens(shrunk), 2 + 500);

        let mut forgotten = measure;
        forgotten.forget_report();
        assert_eq!(forgotten.live_tokens(&history), 2 + 1 + 100 + 500);
        assert_eq!(ContextMeasure::default().live_tokens(&history), 103);
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
