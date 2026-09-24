//! Derived operational grouping for the session navigator.
//!
//! The focus index is intentionally TUI-local: it projects already-cached
//! session state into the four sections used by the wide session browser.
//! Container entries inherit the highest-priority state of their descendants
//! so a collapsed Group/Epic cannot hide work that needs the operator.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use rsi_common::types::{SessionKind, SessionStatus};
use uuid::Uuid;

use crate::types::SessionState;

const RECENT_DAYS: i64 = 10;
const MAX_FOCUS_DEPTH: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SessionFocusGroup {
    NeedsYou,
    InFlight,
    Recent,
    Quiet,
}

impl SessionFocusGroup {
    pub const ALL: [Self; 4] = [Self::NeedsYou, Self::InFlight, Self::Recent, Self::Quiet];

    pub const fn label(self) -> &'static str {
        match self {
            Self::NeedsYou => "NEEDS YOU",
            Self::InFlight => "IN FLIGHT",
            Self::Recent => "RECENT",
            Self::Quiet => "QUIET",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionFocusEntry {
    pub group: SessionFocusGroup,
    pub attention_count: usize,
    pub active_count: usize,
    pub running_agent_count: usize,
    pub running_epic_count: usize,
    pub descendant_count: usize,
}

/// Compute one bounded, memoized hierarchy rollup for every cached session.
///
/// Runtime is O(sessions + hierarchy edges). Malformed cycles fail closed at
/// `MAX_FOCUS_DEPTH` and retain each affected row's own operational state.
pub fn compute_session_focus_index(
    sessions: &HashMap<Uuid, SessionState>,
    now: DateTime<Utc>,
) -> HashMap<Uuid, SessionFocusEntry> {
    let mut children: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    for state in sessions.values() {
        if matches!(
            state.session.status,
            SessionStatus::Archived | SessionStatus::Deleted
        ) {
            continue;
        }
        if let Some(parent_id) = state.session.parent_id
            && sessions.contains_key(&parent_id)
        {
            children
                .entry(parent_id)
                .or_default()
                .push(state.session.id);
        }
    }

    let mut memo = HashMap::with_capacity(sessions.len());
    let mut visiting = HashSet::new();
    for id in sessions.keys().copied() {
        compute_entry(id, sessions, &children, now, 0, &mut visiting, &mut memo);
    }
    memo
}

fn compute_entry(
    id: Uuid,
    sessions: &HashMap<Uuid, SessionState>,
    children: &HashMap<Uuid, Vec<Uuid>>,
    now: DateTime<Utc>,
    depth: usize,
    visiting: &mut HashSet<Uuid>,
    memo: &mut HashMap<Uuid, SessionFocusEntry>,
) -> SessionFocusEntry {
    if let Some(entry) = memo.get(&id) {
        return *entry;
    }

    let Some(state) = sessions.get(&id) else {
        return SessionFocusEntry {
            group: SessionFocusGroup::Quiet,
            attention_count: 0,
            active_count: 0,
            running_agent_count: 0,
            running_epic_count: 0,
            descendant_count: 0,
        };
    };

    let own_attention = state.session.pending_question.is_some()
        || matches!(
            state.session.status,
            SessionStatus::WaitingApproval | SessionStatus::Failed
        );
    let own_active = matches!(
        state.session.status,
        SessionStatus::Running | SessionStatus::Starting
    );
    let age_days = now
        .signed_duration_since(state.session.updated_at)
        .num_days();
    let own_group = if own_attention {
        SessionFocusGroup::NeedsYou
    } else if own_active {
        SessionFocusGroup::InFlight
    } else if age_days < RECENT_DAYS {
        SessionFocusGroup::Recent
    } else {
        SessionFocusGroup::Quiet
    };

    let mut entry = SessionFocusEntry {
        group: own_group,
        attention_count: usize::from(own_attention),
        active_count: usize::from(own_active),
        running_agent_count: usize::from(
            own_active && rsi_common::is_leaf_kind(state.session.session_kind),
        ),
        running_epic_count: 0,
        descendant_count: 0,
    };

    if depth >= MAX_FOCUS_DEPTH || !visiting.insert(id) {
        return entry;
    }

    if let Some(child_ids) = children.get(&id) {
        for child_id in child_ids {
            let child = compute_entry(
                *child_id,
                sessions,
                children,
                now,
                depth + 1,
                visiting,
                memo,
            );
            entry.group = entry.group.min(child.group);
            entry.attention_count += child.attention_count;
            entry.active_count += child.active_count;
            entry.running_agent_count += child.running_agent_count;
            entry.running_epic_count += child.running_epic_count;
            if sessions.get(child_id).is_some_and(|child_state| {
                child_state.session.session_kind == SessionKind::Epic
                    && child.running_agent_count > 0
            }) {
                entry.running_epic_count += 1;
            }
            entry.descendant_count += 1 + child.descendant_count;
        }
    }

    visiting.remove(&id);
    memo.insert(id, entry);
    entry
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::app_test_helpers::baseline_session;
    use rsi_common::types::{PendingQuestion, QuestionItem, SessionKind};

    #[test]
    fn nested_running_epics_count_once_and_ignore_archived_leaves() {
        let now = Utc::now();
        let group_id = Uuid::new_v4();
        let nested_group_id = Uuid::new_v4();
        let epic_id = Uuid::new_v4();
        let nested_epic_id = Uuid::new_v4();
        let mut group = baseline_session(group_id, SessionKind::Group);
        group.status = SessionStatus::Completed;
        let mut nested_group = baseline_session(nested_group_id, SessionKind::Group);
        nested_group.parent_id = Some(group_id);
        nested_group.status = SessionStatus::Completed;
        let mut epic = baseline_session(epic_id, SessionKind::Epic);
        epic.parent_id = Some(group_id);
        epic.status = SessionStatus::Completed;
        let mut nested_epic = baseline_session(nested_epic_id, SessionKind::Epic);
        nested_epic.parent_id = Some(nested_group_id);
        nested_epic.status = SessionStatus::Completed;
        let mut sessions = HashMap::from([
            (group_id, SessionState::new(group)),
            (nested_group_id, SessionState::new(nested_group)),
            (epic_id, SessionState::new(epic)),
            (nested_epic_id, SessionState::new(nested_epic)),
        ]);
        for (parent, status) in [
            (epic_id, SessionStatus::Starting),
            (nested_epic_id, SessionStatus::Running),
            (nested_epic_id, SessionStatus::Archived),
            (nested_epic_id, SessionStatus::Deleted),
        ] {
            let id = Uuid::new_v4();
            let mut leaf = baseline_session(id, SessionKind::Task);
            leaf.parent_id = Some(parent);
            leaf.status = status;
            sessions.insert(id, SessionState::new(leaf));
        }
        let index = compute_session_focus_index(&sessions, now);
        assert_eq!(index[&group_id].running_agent_count, 2);
        assert_eq!(index[&group_id].running_epic_count, 2);
        assert_eq!(index[&nested_group_id].running_epic_count, 1);
        assert_eq!(index[&epic_id].running_agent_count, 1);
        assert_eq!(index[&nested_epic_id].running_agent_count, 1);
    }
    #[test]
    fn container_inherits_descendant_attention_and_counts() {
        let now = Utc::now();
        let parent_id = Uuid::new_v4();
        let child_id = Uuid::new_v4();

        let mut parent = baseline_session(parent_id, SessionKind::Group);
        parent.status = SessionStatus::Completed;
        parent.updated_at = now - chrono::Duration::days(30);

        let mut child = baseline_session(child_id, SessionKind::Standard);
        child.parent_id = Some(parent_id);
        child.status = SessionStatus::Completed;
        child.pending_question = Some(PendingQuestion {
            questions: vec![QuestionItem {
                question: "Choose a durable policy".to_string(),
                header: "Policy".to_string(),
                options: Vec::new(),
                multi_select: false,
            }],
        });

        let sessions = HashMap::from([
            (parent_id, SessionState::new(parent)),
            (child_id, SessionState::new(child)),
        ]);
        let index = compute_session_focus_index(&sessions, now);
        let parent_entry = index[&parent_id];

        assert_eq!(parent_entry.group, SessionFocusGroup::NeedsYou);
        assert_eq!(parent_entry.attention_count, 1);
        assert_eq!(parent_entry.active_count, 0);
        assert_eq!(parent_entry.descendant_count, 1);
    }

    #[test]
    fn direct_states_map_to_stable_focus_groups() {
        let now = Utc::now();
        let waiting_id = Uuid::new_v4();
        let running_id = Uuid::new_v4();
        let recent_id = Uuid::new_v4();
        let quiet_id = Uuid::new_v4();

        let mut waiting = baseline_session(waiting_id, SessionKind::Standard);
        waiting.status = SessionStatus::WaitingApproval;

        let running = baseline_session(running_id, SessionKind::Standard);

        let mut recent = baseline_session(recent_id, SessionKind::Standard);
        recent.status = SessionStatus::Completed;
        recent.updated_at = now - chrono::Duration::days(9);

        let mut quiet = baseline_session(quiet_id, SessionKind::Standard);
        quiet.status = SessionStatus::Completed;
        quiet.updated_at = now - chrono::Duration::days(10);

        let sessions = HashMap::from([
            (waiting_id, SessionState::new(waiting)),
            (running_id, SessionState::new(running)),
            (recent_id, SessionState::new(recent)),
            (quiet_id, SessionState::new(quiet)),
        ]);
        let index = compute_session_focus_index(&sessions, now);

        assert_eq!(index[&waiting_id].group, SessionFocusGroup::NeedsYou);
        assert_eq!(index[&running_id].group, SessionFocusGroup::InFlight);
        assert_eq!(index[&recent_id].group, SessionFocusGroup::Recent);
        assert_eq!(index[&quiet_id].group, SessionFocusGroup::Quiet);
    }
}
