//! Bounded, TUI-local cross-session activity view model.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use rsi_common::types::{SessionKind, SessionStatus};
use uuid::Uuid;

use crate::types::{
    SessionFocusEntry, SessionFocusGroup, SessionState, resolve_session_display_identity,
};

pub const OPERATOR_QUEUE_CACHE_LIMIT: usize = 5;
pub const RECENT_CHANGES_CACHE_LIMIT: usize = 7;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionActivityItem {
    pub session_id: Uuid,
    pub ordinal: usize,
    pub session_kind: SessionKind,
    pub title: String,
    pub status: SessionStatus,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionFlowSummary {
    pub needs_you: usize,
    pub in_flight: usize,
    pub recent: usize,
    pub quiet: usize,
    pub failed: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionActivityViewModel {
    pub operator_queue: Vec<SessionActivityItem>,
    pub recent_changes: Vec<SessionActivityItem>,
    pub flow: SessionFlowSummary,
}

pub fn compute_session_activity(
    sessions: &HashMap<Uuid, SessionState>,
    order: &[Uuid],
    focus_index: &HashMap<Uuid, SessionFocusEntry>,
) -> SessionActivityViewModel {
    let mut model = SessionActivityViewModel::default();

    for (rank, session_id) in order.iter().copied().enumerate() {
        let Some(state) = sessions.get(&session_id) else {
            continue;
        };
        let group = focus_index
            .get(&session_id)
            .map(|entry| entry.group)
            .unwrap_or(SessionFocusGroup::Recent);
        match group {
            SessionFocusGroup::NeedsYou => model.flow.needs_you += 1,
            SessionFocusGroup::InFlight => model.flow.in_flight += 1,
            SessionFocusGroup::Recent => model.flow.recent += 1,
            SessionFocusGroup::Quiet => model.flow.quiet += 1,
        }
        if state.session.status == SessionStatus::Failed {
            model.flow.failed += 1;
        }

        let item = SessionActivityItem {
            session_id,
            ordinal: rank + 1,
            session_kind: state.session.session_kind,
            title: clean_title(&state.session, sessions),
            status: state.session.status,
            updated_at: state.session.updated_at,
        };
        if group == SessionFocusGroup::NeedsYou
            && model.operator_queue.len() < OPERATOR_QUEUE_CACHE_LIMIT
        {
            model.operator_queue.push(item.clone());
        }
        insert_recent(&mut model.recent_changes, item, rank);
    }

    model
}

fn insert_recent(
    recent: &mut Vec<SessionActivityItem>,
    item: SessionActivityItem,
    order_rank: usize,
) {
    let insert_at = recent
        .iter()
        .enumerate()
        .find_map(|(index, current)| {
            if item.updated_at > current.updated_at {
                Some(index)
            } else if item.updated_at == current.updated_at && order_rank < current.ordinal - 1 {
                Some(index)
            } else {
                None
            }
        })
        .unwrap_or(recent.len());
    if insert_at < RECENT_CHANGES_CACHE_LIMIT {
        recent.insert(insert_at, item);
        recent.truncate(RECENT_CHANGES_CACHE_LIMIT);
    } else if recent.len() < RECENT_CHANGES_CACHE_LIMIT {
        recent.push(item);
    }
}

fn clean_title(
    session: &rsi_common::types::Session,
    sessions: &HashMap<Uuid, SessionState>,
) -> String {
    resolve_session_display_identity(session, sessions)
        .effective_title
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::app_test_helpers::baseline_session;
    use crate::types::compute_session_focus_index;
    use rsi_common::types::{PendingQuestion, QuestionItem, SessionKind};

    #[test]
    fn queues_are_bounded_and_preserve_operational_order() {
        let now = Utc::now();
        let mut sessions = HashMap::new();
        let mut order = Vec::new();
        for index in 0..10 {
            let id = Uuid::new_v4();
            let mut session = baseline_session(id, SessionKind::Task);
            session.title = Some(format!("Needs operator {index}"));
            session.status = SessionStatus::WaitingApproval;
            session.pending_question = Some(PendingQuestion {
                questions: vec![QuestionItem {
                    question: format!("Question {index}"),
                    header: String::new(),
                    options: Vec::new(),
                    multi_select: false,
                }],
            });
            session.updated_at = now - chrono::Duration::minutes((9 - index) as i64);
            sessions.insert(id, SessionState::new(session));
            order.push(id);
        }
        let focus = compute_session_focus_index(&sessions, now);
        let model = compute_session_activity(&sessions, &order, &focus);

        assert_eq!(model.operator_queue.len(), OPERATOR_QUEUE_CACHE_LIMIT);
        assert_eq!(model.operator_queue[0].session_id, order[0]);
        assert_eq!(model.recent_changes.len(), RECENT_CHANGES_CACHE_LIMIT);
        assert_eq!(model.recent_changes[0].session_id, order[9]);
        assert_eq!(model.flow.needs_you, 10);
    }

    #[test]
    fn ties_follow_current_order_and_failures_are_counted() {
        let now = Utc::now();
        let ids = [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
        let mut sessions = HashMap::new();
        for (index, id) in ids.iter().copied().enumerate() {
            let mut session = baseline_session(id, SessionKind::Standard);
            session.title = Some(format!("Session {index}"));
            session.updated_at = now;
            if index == 1 {
                session.status = SessionStatus::Failed;
            } else {
                session.status = SessionStatus::Completed;
            }
            sessions.insert(id, SessionState::new(session));
        }
        let focus = compute_session_focus_index(&sessions, now);
        let model = compute_session_activity(&sessions, &ids, &focus);

        assert_eq!(
            model
                .recent_changes
                .iter()
                .map(|item| item.session_id)
                .collect::<Vec<_>>(),
            ids
        );
        assert_eq!(model.flow.failed, 1);
        assert_eq!(model.operator_queue[0].status, SessionStatus::Failed);
    }

    #[test]
    fn no_events_are_required_for_explicit_activity_state() {
        let now = Utc::now();
        let id = Uuid::new_v4();
        let mut session = baseline_session(id, SessionKind::Standard);
        session.status = SessionStatus::Running;
        let sessions = HashMap::from([(id, SessionState::new(session))]);
        let focus = compute_session_focus_index(&sessions, now);
        let model = compute_session_activity(&sessions, &[id], &focus);

        assert_eq!(model.recent_changes[0].status, SessionStatus::Running);
        assert_eq!(model.flow.in_flight, 1);
    }
}
