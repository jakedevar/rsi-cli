//! Shared "needs attention" predicate.
//!
//! Single source of truth for "which sessions need attention, in what order".
//! Consumed by:
//!   - `action_handler::navigation::navigate_attention` (existing — `]a / [a` cycle)
//!   - `action_handler::navigation::jump_attention_n` (`1..9` jump)
//!
//! The navigation layers consume the bare ID list directly; all consumers see
//! the same set in the same order.
//!
//! Critical correctness invariant: `]a` / `[a` and digit-jumps `1..9` MUST
//! land on the SAME ordered set of sessions. The unit test
//! `tests::predicate_matches_navigate_attention_inputs` guards drift.

use uuid::Uuid;

use crate::app::App;
use rsi_common::types::SessionStatus;

/// Returns the ordered list of session UUIDs that currently need attention.
/// Order: `app.session_order` first (stable across polls), then any
/// High-priority notification sessions not already in the list.
pub fn attention_session_ids(app: &App) -> Vec<Uuid> {
    let mut out = Vec::new();
    for sid in &app.session_order {
        let Some(state) = app.sessions.get(sid) else {
            continue;
        };
        let s = &state.session;
        let needs_attn = s.pending_question.is_some()
            || matches!(
                s.status,
                SessionStatus::WaitingApproval | SessionStatus::Failed
            );
        if needs_attn {
            out.push(*sid);
        }
    }
    for n in &app.notifications {
        if n.priority == crate::types::NotificationPriority::High {
            if let Some(sid) = n.session_id {
                if !out.contains(&sid) {
                    out.push(sid);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::app_test_helpers;

    /// Guards the doc-comment-promised invariant (see module doc above):
    /// `]a`/`[a` (`navigate_attention`) and `<Space>1..9` (`jump_attention_n`)
    /// both consume this predicate's output with no additional
    /// transformation, so pinning the contract once transitively pins
    /// ordering agreement for both consumers (and the header's `⚠` count,
    /// which also calls this fn unmodified).
    #[test]
    fn predicate_matches_navigate_attention_inputs() {
        let mut app = app_test_helpers::with_session_list(5);
        let ids = app.filtered_session_order.clone();
        app.session_order = ids.clone();

        // id[0]: Running — must be absent from the result.
        if let Some(state) = app.sessions.get_mut(&ids[0]) {
            state.session.status = SessionStatus::Running;
        }
        // id[1]: Failed — qualifies.
        if let Some(state) = app.sessions.get_mut(&ids[1]) {
            state.session.status = SessionStatus::Failed;
        }
        // id[2]: WaitingApproval — qualifies.
        if let Some(state) = app.sessions.get_mut(&ids[2]) {
            state.session.status = SessionStatus::WaitingApproval;
        }
        // id[3]: pending_question set — qualifies regardless of status.
        if let Some(state) = app.sessions.get_mut(&ids[3]) {
            state.session.pending_question = Some(rsi_common::types::PendingQuestion {
                questions: Vec::new(),
            });
        }
        // id[4]: Completed — must be absent from the result.
        if let Some(state) = app.sessions.get_mut(&ids[4]) {
            state.session.status = SessionStatus::Completed;
        }

        // A 6th session, NOT in session_order, reachable only via a
        // High-priority notification — must be appended after the
        // session_order-derived members.
        let notif_only_id = Uuid::new_v4();
        app.notifications.push_back(crate::types::Notification {
            id: 1,
            kind: crate::types::NotificationKind::OperationSuccess,
            message: "test".to_string(),
            priority: crate::types::NotificationPriority::High,
            created_at: std::time::Instant::now(),
            ttl: std::time::Duration::from_secs(5),
            session_id: Some(notif_only_id),
            dismissed: false,
        });

        let result = attention_session_ids(&app);

        assert_eq!(
            result,
            vec![ids[1], ids[2], ids[3], notif_only_id],
            "expected session_order-traversal-order qualifiers followed by the notification-only id"
        );
        assert!(
            !result.contains(&ids[0]),
            "Running session must not need attention"
        );
        assert!(
            !result.contains(&ids[4]),
            "Completed session must not need attention"
        );
        let mut deduped = result.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(deduped.len(), result.len(), "no duplicate ids expected");
    }
}
