use crate::session::types::MonitorBreakReason;
use rsi_common::types::{ConversationEvent, EventType};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetryClassification {
    Transient,
    RateLimited,
    Overloaded,
    Network,
    Auth,
    Quota,
    Config,
    Input,
    Validation,
    Cancel,
    PolicyDenied,
    UserCancelled,
    Terminal,
}

impl RetryClassification {
    pub const fn retryable(self) -> bool {
        matches!(
            self,
            Self::Transient | Self::RateLimited | Self::Overloaded | Self::Network
        )
    }

    pub const fn as_reason(self) -> &'static str {
        match self {
            Self::Transient => "transient failure",
            Self::RateLimited => "rate limited",
            Self::Overloaded => "provider overloaded",
            Self::Network => "network failure",
            Self::Auth => "authentication failure",
            Self::Quota => "quota exceeded",
            Self::Config => "invalid configuration",
            Self::Input => "invalid input",
            Self::Validation => "validation failure",
            Self::Cancel => "explicit cancellation",
            Self::PolicyDenied => "policy denied",
            Self::UserCancelled => "user cancelled",
            Self::Terminal => "terminal completion",
        }
    }
}

pub(crate) fn classify_session_retry(
    break_reason: &MonitorBreakReason,
    received_any_event: bool,
    received_meaningful_output: bool,
    exit_code: Option<i32>,
    events: &[ConversationEvent],
) -> RetryClassification {
    if matches!(break_reason, MonitorBreakReason::StallTimeout) {
        return RetryClassification::Transient;
    }
    if matches!(break_reason, MonitorBreakReason::Interrupted) {
        return RetryClassification::UserCancelled;
    }
    if matches!(break_reason, MonitorBreakReason::Rotation) {
        return RetryClassification::Cancel;
    }
    if matches!(break_reason, MonitorBreakReason::Result) && received_meaningful_output {
        return RetryClassification::Terminal;
    }
    if !received_any_event {
        return RetryClassification::Transient;
    }

    let recent = events
        .iter()
        .rev()
        .take(5)
        .filter(|event| matches!(event.event_type, EventType::System | EventType::Message))
        .map(|event| event.content.to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join(" ");

    if recent.contains("401")
        || recent.contains("unauthorized")
        || recent.contains("invalid api key")
        || recent.contains("invalid_api_key")
        || recent.contains("permission denied")
        || recent.contains("forbidden")
    {
        return RetryClassification::Auth;
    }
    if recent.contains("resource_exhausted")
        || recent.contains("resource exhausted")
        || recent.contains("usage limit")
        || recent.contains("weekly limit")
        || recent.contains("quota")
    {
        return RetryClassification::Quota;
    }
    if recent.contains("model not found")
        || recent.contains("unknown model")
        || recent.contains("unsupported model")
    {
        return RetryClassification::Config;
    }
    if recent.contains("invalid param")
        || recent.contains("invalid input")
        || recent.contains("bad request")
    {
        return RetryClassification::Input;
    }
    if recent.contains("validation") {
        return RetryClassification::Validation;
    }
    if recent.contains("policy denied") || recent.contains("stop-all") {
        return RetryClassification::PolicyDenied;
    }
    if recent.contains("rate limit") || recent.contains("429") {
        return RetryClassification::RateLimited;
    }
    if recent.contains("overloaded") || recent.contains("529") {
        return RetryClassification::Overloaded;
    }
    if recent.contains("connection reset")
        || recent.contains("timed out")
        || recent.contains("network")
    {
        return RetryClassification::Network;
    }
    if !received_meaningful_output {
        return RetryClassification::Transient;
    }
    if exit_code.is_some_and(|code| code != 0) {
        return RetryClassification::Transient;
    }
    RetryClassification::Terminal
}

pub(crate) fn classify_error_message(message: &str) -> RetryClassification {
    let recent = message.to_ascii_lowercase();
    if recent.contains("401")
        || recent.contains("unauthorized")
        || recent.contains("invalid api key")
        || recent.contains("invalid_api_key")
        || recent.contains("permission denied")
        || recent.contains("forbidden")
    {
        return RetryClassification::Auth;
    }
    if recent.contains("resource_exhausted")
        || recent.contains("resource exhausted")
        || recent.contains("usage limit")
        || recent.contains("weekly limit")
        || recent.contains("quota")
    {
        return RetryClassification::Quota;
    }
    if recent.contains("model not found")
        || recent.contains("unknown model")
        || recent.contains("unsupported model")
    {
        return RetryClassification::Config;
    }
    if recent.contains("invalid param")
        || recent.contains("invalid input")
        || recent.contains("bad request")
    {
        return RetryClassification::Input;
    }
    if recent.contains("validation") {
        return RetryClassification::Validation;
    }
    if recent.contains("policy denied") || recent.contains("stop-all") {
        return RetryClassification::PolicyDenied;
    }
    if recent.contains("cancelled") || recent.contains("canceled") {
        return RetryClassification::Cancel;
    }
    if recent.contains("rate limit") || recent.contains("429") {
        return RetryClassification::RateLimited;
    }
    if recent.contains("overloaded") || recent.contains("529") {
        return RetryClassification::Overloaded;
    }
    if recent.contains("connection reset")
        || recent.contains("timed out")
        || recent.contains("network")
    {
        return RetryClassification::Network;
    }
    RetryClassification::Transient
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use rsi_common::types::{ConversationEvent, EventType, Role};
    use uuid::Uuid;

    fn event(content: &str) -> ConversationEvent {
        ConversationEvent {
            id: 0,
            session_id: Uuid::new_v4(),
            sequence: 0,
            event_type: EventType::System,
            role: Some(Role::Assistant),
            content: content.to_string(),
            tool_name: None,
            tool_input: None,
            created_at: Utc::now(),
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        }
    }

    #[test]
    fn quota_is_not_retryable() {
        let classification = classify_session_retry(
            &MonitorBreakReason::StreamClosed,
            true,
            false,
            Some(1),
            &[event("resource exhausted quota exceeded")],
        );
        assert_eq!(classification, RetryClassification::Quota);
        assert!(!classification.retryable());
    }

    #[test]
    fn rate_limit_is_retryable() {
        let classification = classify_session_retry(
            &MonitorBreakReason::StreamClosed,
            true,
            false,
            Some(1),
            &[event("429 rate limit")],
        );
        assert_eq!(classification, RetryClassification::RateLimited);
        assert!(classification.retryable());
    }
}
