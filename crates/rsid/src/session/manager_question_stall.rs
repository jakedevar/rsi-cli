//! Issue #1861: a manager seat never silently ends a turn on a question.
//!
//! A project PM or portfolio/area manager whose final message asks for a
//! ruling, with no `AgentReportUp` in the turn and no enabled resume/`when`
//! wake, would sit idle until another manager's safety net found it. The
//! daemon records a friction event and arms one one-shot resume wake that
//! tells the seat to decide or report up. Chosen over refusing the turn end
//! (a provider turn cannot be un-ended) and over a plain inbox nudge (an idle
//! seat is not woken by mail without a wake): the wake is what actually
//! resumes it. Capped per session so a seat that keeps asking is only
//! recorded after `MANAGER_QUESTION_NUDGE_CAP` nudges.

use rsi_common::friction::{FrictionKind, NewFrictionEventV1};
use rsi_common::types::{ConversationEvent, EventType, Role, Session};
use tokio::sync::Mutex;

use crate::store::Store;
use crate::store::manager_question_stall::ManagerQuestionArm;

/// Stable friction code recorded for the stall.
pub(crate) const STALL_CODE: &str = "manager_question_stall";
/// Friction code recorded once the nudge budget is spent.
pub(crate) const STALL_CAPPED_CODE: &str = "manager_question_stall_capped";

const ASK_PHRASES: &[&str] = &[
    "tell me how",
    "let me know how",
    "how do you want",
    "how would you like",
    "how should i proceed",
    "do you want me to",
    "would you like me to",
    "your call",
    "please advise",
    "please decide",
    "need a decision",
    "need your decision",
    "awaiting your",
    "waiting for your",
    "which do you prefer",
    "which would you prefer",
];

/// True when `text` ends asking for a decision: one of its last three
/// non-empty lines ends in `?` or holds a request phrase.
pub(crate) fn asks_for_decision(text: &str) -> bool {
    text.lines()
        .map(|line| line.trim().trim_end_matches(['*', '_', '`', ')']).trim())
        .filter(|line| !line.is_empty())
        .rev()
        .take(3)
        .any(|line| {
            let lower = line.to_ascii_lowercase();
            // `Friction:` and handoff metadata lines are not questions.
            !lower.starts_with("friction:")
                && (line.ends_with('?') || ASK_PHRASES.iter().any(|p| lower.contains(p)))
        })
}

fn is_report_up_call(event: &ConversationEvent) -> bool {
    fn normalized(raw: &str) -> String {
        raw.chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase()
    }
    let hit = |raw: &str| {
        let n = normalized(raw);
        n.contains("reportup") || n.contains("reporttoglobal")
    };
    event.event_type == EventType::ToolUse
        && (event.tool_name.as_deref().is_some_and(hit)
            || event
                .tool_input
                .as_ref()
                .is_some_and(|v| hit(&v.to_string())))
}

/// The current turn (events after the last user message) ended on a decision
/// request and sent nothing up.
pub(crate) fn turn_ends_asking(events: &[ConversationEvent]) -> bool {
    let turn: Vec<&ConversationEvent> = events
        .iter()
        .rev()
        .take_while(|event| {
            !(event.event_type == EventType::Message && event.role == Some(Role::User))
        })
        .collect();
    if turn.iter().any(|event| is_report_up_call(event)) {
        return false;
    }
    // `turn` is newest first: the last assistant message is the first hit.
    turn.iter()
        .find(|event| {
            event.event_type == EventType::Message
                && event.role == Some(Role::Assistant)
                && !rsi_common::agent_session_events::is_provider_diagnostic_metadata(
                    event.metadata.as_deref(),
                )
        })
        .is_some_and(|event| asks_for_decision(&event.content))
}

/// Lifecycle hook for a session that just finalized `Completed`: when it is a
/// manager seat that ended its turn asking, record friction and arm the
/// safety wake. Never fails the finalizer.
pub(crate) async fn guard_completed_manager(
    store: &Mutex<Store>,
    session: &Session,
    events: &[ConversationEvent],
) {
    if session.pending_question.is_some() || !turn_ends_asking(events) {
        return;
    }
    let outcome = {
        let store = store.lock().await;
        match store.session_holds_manager_seat(session.id) {
            Ok(true) => store.arm_manager_question_wake(session),
            Ok(false) => return,
            Err(error) => Err(error),
        }
    };
    let code = match outcome {
        Ok(ManagerQuestionArm::Armed) => {
            tracing::info!(session_id = %session.id, "Manager seat ended its turn asking a question; armed a safety wake");
            STALL_CODE
        }
        Ok(ManagerQuestionArm::HoldsResumeWake) => return,
        Ok(ManagerQuestionArm::CapReached) => STALL_CAPPED_CODE,
        Err(error) => {
            tracing::warn!(session_id = %session.id, %error, "manager question guard failed");
            return;
        }
    };
    let event = NewFrictionEventV1::new(FrictionKind::Terminal, &[code])
        .session(Some(session.id))
        .evidence("session", session.id);
    crate::friction::note(store, event).await;
}
