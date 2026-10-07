//! Friction telemetry, the "andon" (#1333): daemon-side recording helpers
//! and the periodic sweep that files deduplicated kaizen Issues. The schema,
//! rollup and filing rules live in `crate::store::friction`.

use crate::error::DaemonError;
use crate::session::SessionManager;
use crate::store::Store;
use rsi_common::friction::{FrictionKind, NewFrictionEventV1, is_friction_code};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

/// How often the andon sweep looks for due signatures.
const SWEEP_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// The stable code of an `Agent*` verb error, never its prose: a structured
/// `data.code` (or `data.error.code`), a code-shaped message or
/// `InvalidParam`, else a fixed class name.
#[must_use]
pub fn agent_refusal_code(error: &DaemonError) -> &str {
    fn code_prefix(raw: &str) -> Option<&str> {
        let head = raw.split([':', ' ']).next().unwrap_or(raw);
        is_friction_code(head).then_some(head)
    }
    match error {
        DaemonError::StructuredRpc { message, data, .. } => data
            .get("code")
            .and_then(serde_json::Value::as_str)
            .or_else(|| {
                data.pointer("/error/code")
                    .and_then(serde_json::Value::as_str)
            })
            .filter(|code| is_friction_code(code))
            .or_else(|| code_prefix(message))
            .unwrap_or("structured_refusal"),
        DaemonError::InvalidParam(message) => code_prefix(message).unwrap_or("invalid_param"),
        DaemonError::SessionNotFound(_) => "session_not_found",
        DaemonError::Json(_) => "invalid_json",
        _ => "internal_error",
    }
}

/// The event for a tokened `Agent*` verb that returned `error`. `verb` is a
/// registered verb name (the attribution gate admitted it).
#[must_use]
pub fn agent_refusal_event(
    verb: &str,
    caller: Option<Uuid>,
    error: &DaemonError,
) -> NewFrictionEventV1 {
    NewFrictionEventV1::new(
        FrictionKind::AgentRefusal,
        &[verb, agent_refusal_code(error)],
    )
    .session(caller)
}

/// Inspect only the final assistant message in the current turn. Friction
/// prose is classified in memory and never copied into the telemetry event.
#[must_use]
pub fn handoff_friction_event(
    session_id: Uuid,
    events: &[rsi_common::types::ConversationEvent],
) -> Option<NewFrictionEventV1> {
    use rsi_common::types::{EventType, Role};
    let message = events
        .iter()
        .rev()
        .take_while(|event| {
            !(event.event_type == EventType::Message && event.role == Some(Role::User))
        })
        .find(|event| {
            event.event_type == EventType::Message
                && event.role == Some(Role::Assistant)
                && !rsi_common::agent_session_events::is_provider_diagnostic_metadata(
                    event.metadata.as_deref(),
                )
        })?;
    let line = message
        .content
        .lines()
        .rev()
        .find_map(|line| line.trim().strip_prefix("Friction:"))?
        .trim();
    let names_issue = line
        .as_bytes()
        .windows(2)
        .any(|pair| pair[0] == b'#' && pair[1].is_ascii_digit());
    if line.eq_ignore_ascii_case("none") || names_issue {
        return None;
    }
    Some(
        NewFrictionEventV1::new(FrictionKind::Handoff, &["friction_unfiled"])
            .session(Some(session_id))
            .evidence("session", session_id),
    )
}

/// Record one event; friction telemetry never fails the observed operation.
pub async fn note(store: &tokio::sync::Mutex<Store>, event: NewFrictionEventV1) {
    if let Err(error) = store.lock().await.record_friction_event(&event) {
        tracing::warn!(signature = %event.signature, %error, "friction event not recorded");
    }
}

/// [`note`] on a store the caller already holds.
pub fn note_locked(store: &Store, event: &NewFrictionEventV1) {
    if let Err(error) = store.record_friction_event(event) {
        tracing::warn!(signature = %event.signature, %error, "friction event not recorded");
    }
}

fn andon_ticker() -> tokio::time::Interval {
    let mut interval = tokio::time::interval(SWEEP_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    interval
}

/// Sweep for due friction signatures every ten minutes (the first pass at
/// boot) and file their kaizen Issues.
pub async fn run_andon_loop(manager: Arc<SessionManager>) {
    let mut interval = andon_ticker();
    loop {
        interval.tick().await;
        let swept = manager.store().lock().await.andon_sweep(chrono::Utc::now());
        match swept {
            Ok(filed) => {
                for filing in filed {
                    tracing::info!(
                        project_id = %filing.project_id,
                        signature = %filing.signature,
                        issue = filing.display_number,
                        occurrences = filing.occurrences,
                        sessions = filing.sessions,
                        "andon filed a kaizen Issue for a repeating friction signature"
                    );
                }
            }
            Err(error) => tracing::warn!(%error, "andon sweep deferred"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn refusal_codes_come_from_codes_never_prose() {
        let structured = DaemonError::StructuredRpc {
            rpc_code: -32602,
            message: "Issue authority denied for this caller".into(),
            data: json!({"code": "agent_issue_authority_denied"}),
        };
        assert_eq!(
            agent_refusal_code(&structured),
            "agent_issue_authority_denied"
        );
        let nested = DaemonError::StructuredRpc {
            rpc_code: -32602,
            message: "x".into(),
            data: json!({"error": {"code": "sandbox_reclaim_prepared"}}),
        };
        assert_eq!(agent_refusal_code(&nested), "sandbox_reclaim_prepared");
        assert_eq!(
            agent_refusal_code(&DaemonError::InvalidParam(
                "manager_v2_lifecycle_unconfirmed: the lifecycle is unknown".into()
            )),
            "manager_v2_lifecycle_unconfirmed"
        );
        assert_eq!(
            agent_refusal_code(&DaemonError::InvalidParam(
                "/home/user/secret path is bad".into()
            )),
            "invalid_param"
        );
        assert_eq!(
            agent_refusal_code(&DaemonError::Store("disk full".into())),
            "internal_error"
        );
        let event = agent_refusal_event(
            "AgentGetIssue",
            None,
            &DaemonError::InvalidParam("stale_continuation".into()),
        );
        assert_eq!(
            event.signature,
            "agent_refusal:AgentGetIssue:stale_continuation"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn handoff_friction_uses_only_the_final_assistant_message_of_this_turn() {
        use rsi_common::types::{ConversationEvent, EventType, Role};
        let session_id = Uuid::new_v4();
        let message = ConversationEvent {
            id: 0,
            offload_id: None,
            session_id,
            sequence: 0,
            event_type: EventType::Message,
            role: Some(Role::Assistant),
            content: "Friction: private details".into(),
            tool_name: None,
            tool_input: None,
            tool_use_id: None,
            metadata: None,
            created_at: chrono::Utc::now(),
        };
        let mut events = vec![message.clone()];
        assert_eq!(
            handoff_friction_event(session_id, &events)
                .unwrap()
                .signature,
            "handoff:friction_unfiled"
        );
        let mut diagnostic = message.clone();
        diagnostic.content = "diagnostic".into();
        diagnostic.metadata = Some(Box::new(
            rsi_common::agent_session_events::provider_diagnostic_metadata("stderr", false),
        ));
        events.push(diagnostic);
        assert!(handoff_friction_event(session_id, &events).is_some());
        events.push(ConversationEvent {
            content: "Corrected final answer".into(),
            ..message.clone()
        });
        assert!(handoff_friction_event(session_id, &events).is_none());
        events.push(message.clone());
        events.push(ConversationEvent {
            role: Some(Role::User),
            content: "next turn".into(),
            ..message
        });
        assert!(handoff_friction_event(session_id, &events).is_none());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test(start_paused = true)]
    async fn andon_ticker_runs_on_boot_then_every_ten_minutes() {
        let mut ticker = andon_ticker();
        let first = ticker.tick().await;
        let second = ticker.tick().await;
        assert_eq!(second - first, SWEEP_INTERVAL);
    }
}
