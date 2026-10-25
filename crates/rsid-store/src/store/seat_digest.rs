//! #3386: the seat digest and the launch turn of a rotation successor.
//!
//! A manager seat that rotates starts its successor with a `session.rotate.child`
//! invocation; that first turn rehydrates the seat (overview, inbox, wakes,
//! queue, authority, notebook) and is the dominant cost of a rotation. This
//! read answers "what did the last rotations cost and how many tool calls did
//! they make" from the invocation ledger and the transcript, so the next
//! measurement is a read of `AgentManagerOverview`, not a database query.

use crate::error::Result;
use crate::store::Store;
use chrono::{SecondsFormat, Utc};
use rsi_common::daemon_handoff::HandoffProjectStateV1;
use rsi_common::global_manager::SeatLaunchTurnV1;
use rusqlite::{OptionalExtension, params};
use uuid::Uuid;

impl Store {
    /// #3386: one project's headline state, counted in the database, so a
    /// successor seat's launch prompt carries what an overview read would.
    ///
    /// # Errors
    /// A database error.
    pub fn handoff_project_state(&self, project_id: Uuid) -> Result<HandoffProjectStateV1> {
        let key = project_id.to_string();
        let (issues_open, issues_in_progress, open_operator_requests): (i64, i64, i64) =
            self.conn.query_row(
                "SELECT
                    COALESCE(SUM(status='Open'),0),
                    COALESCE(SUM(status='InProgress'),0),
                    COALESCE(SUM(status='Open' AND EXISTS(SELECT 1 FROM json_each(issues.labels) WHERE json_each.value='operator-request')),0)
                 FROM issues WHERE project_id=?1 AND archived_at IS NULL",
                [&key],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
        let (running_sessions, waiting_approval_sessions, pending_questions): (i64, i64, i64) =
            self.conn.query_row(
                "SELECT
                    COALESCE(SUM(status='Running'),0),
                    COALESCE(SUM(status='WaitingApproval'),0),
                    COALESCE(SUM(pending_question_json IS NOT NULL AND status NOT IN ('Archived','Deleted')),0)
                 FROM sessions WHERE project_id=?1",
                [&key],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
        let pending_approvals: i64 = self.conn.query_row(
            "SELECT count(*) FROM approvals a JOIN sessions s ON s.id=a.session_id
             WHERE s.project_id=?1 AND a.status='Pending'",
            [&key],
            |row| row.get(0),
        )?;
        Ok(HandoffProjectStateV1 {
            issues_open,
            issues_in_progress,
            open_operator_requests,
            running_sessions,
            waiting_approval_sessions,
            pending_questions,
            pending_approvals,
        })
    }

    /// The launch turn of `session_id` when it was started as a rotation
    /// successor, else `None`. While the launch invocation runs, the tool-call
    /// count is the count so far and `settled` is false.
    ///
    /// # Errors
    /// A database error.
    pub fn seat_launch_turn(&self, session_id: Uuid) -> Result<Option<SeatLaunchTurnV1>> {
        let session = session_id.to_string();
        let launch: Option<(Option<f64>, Option<String>)> = self
            .conn
            .query_row(
                concat!(
                    "SELECT ",
                    "CASE WHEN ",
                    "rsi_billing_mode(provider,model,(SELECT json_group_object(key,value) FROM daemon_settings WHERE key IN ('claude_billing_mode','codex_billing_mode','codexappserver_billing_mode','pioneer_billing_mode','antigravity_billing_mode','billing_login.claude','billing_login.codex')))",
                    "='subscription' THEN NULL ELSE estimated_cost_usd END",
                    ", completed_at FROM model_invocations
                 WHERE session_id=?1 AND purpose='session.rotate.child'
                   AND admission_status='admitted'
                 ORDER BY created_at LIMIT 1"
                ),
                params![session],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((cost_usd, completed_at)) = launch else {
            return Ok(None);
        };
        let settled = completed_at.is_some();
        let until =
            completed_at.unwrap_or_else(|| Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true));
        // An Archived or Deleted session may have moved its ToolUse rows to cold
        // storage (#2384), so it reports no launch turn rather than a short
        // count; the join keeps the read disjoint from cold sessions.
        let live: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions s WHERE s.id=?1
                 AND s.status NOT IN ('Archived','Deleted'))",
            params![session],
            |row| row.get(0),
        )?;
        if !live {
            return Ok(None);
        }
        let tool_calls: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM conversation_events
             WHERE session_id=?1 AND event_type='ToolUse' AND created_at<=?2",
            params![session, until],
            |row| row.get(0),
        )?;
        Ok(Some(SeatLaunchTurnV1 {
            cost_usd: if settled { cost_usd } else { None },
            tool_calls,
            settled,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Dollar-accounting fixtures use API logins. Subscription coverage lives
    // in provider_billing::tests and never treats CLI list prices as bills.
    fn metered_store() -> Store {
        let store = Store::open_in_memory().unwrap();
        store
            .set_daemon_setting("claude_billing_mode", "metered_api")
            .unwrap();
        store
            .set_daemon_setting("codex_billing_mode", "metered_api")
            .unwrap();
        store
    }

    use crate::test_support::test_session;
    use rsi_common::types::{ConversationEvent, EventType};
    use std::path::PathBuf;

    fn at(offset_secs: i64) -> String {
        (Utc::now() + chrono::Duration::seconds(offset_secs))
            .to_rfc3339_opts(SecondsFormat::Nanos, true)
    }

    fn session(store: &Store) -> Uuid {
        let id = Uuid::new_v4();
        store
            .insert_session(&test_session(id, PathBuf::from("/tmp/launch-turn")))
            .unwrap();
        id
    }

    fn launch_invocation(store: &Store, session: Uuid, cost: f64, completed: Option<String>) {
        store
            .conn
            .execute(
                "INSERT INTO model_invocations (id,purpose,invocation_kind,foreground,paid_risk,
                     admission_status,status,trigger_source,session_id,policy_snapshot_json,
                     created_at,started_at,completed_at,estimated_cost_usd)
                 VALUES (?1,'session.rotate.child','session_lifecycle','foreground','paid_capable',
                     'admitted',?2,'test',?3,'{}',?4,?4,?5,?6)",
                params![
                    Uuid::new_v4().to_string(),
                    if completed.is_some() {
                        "completed"
                    } else {
                        "running"
                    },
                    session.to_string(),
                    at(-100),
                    completed,
                    cost,
                ],
            )
            .unwrap();
    }

    fn tool_use(store: &Store, session: Uuid, sequence: i32, created: String) {
        store
            .insert_event(&ConversationEvent {
                id: 0,
                session_id: session,
                sequence,
                event_type: EventType::ToolUse,
                role: None,
                content: String::new(),
                tool_name: Some("Bash".into()),
                tool_input: None,
                created_at: chrono::DateTime::parse_from_rfc3339(&created)
                    .unwrap()
                    .with_timezone(&Utc),
                offload_id: None,
                tool_use_id: None,
                metadata: None,
            })
            .unwrap();
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn project_state_counts_issues_sessions_and_approvals() {
        let store = metered_store();
        let project = Uuid::new_v4();
        let now = Utc::now();
        store
            .insert_project(&rsi_common::types::Project {
                id: project,
                name: "digest".into(),
                path: None,
                description: None,
                color: rsi_common::types::Project::DEFAULT_COLOR.into(),
                context_files: None,
                created_at: now,
                updated_at: now,
            })
            .unwrap();
        let mut running = test_session(Uuid::new_v4(), PathBuf::from("/tmp/digest"));
        running.project_id = Some(project);
        running.status = rsi_common::types::SessionStatus::Running;
        store.insert_session(&running).unwrap();
        let mut other = test_session(Uuid::new_v4(), PathBuf::from("/tmp/digest"));
        other.status = rsi_common::types::SessionStatus::Running;
        store.insert_session(&other).unwrap();
        let state = store.handoff_project_state(project).unwrap();
        assert_eq!(state.running_sessions, 1);
        assert_eq!(state.issues_open, 0);
        assert_eq!(state.pending_approvals, 0);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn a_settled_launch_turn_reports_its_cost_and_only_its_own_tool_calls() {
        let store = metered_store();
        let successor = session(&store);
        for (sequence, offset) in [(1, -90), (2, -80), (3, -70), (4, 60)] {
            tool_use(&store, successor, sequence, at(offset));
        }
        launch_invocation(&store, successor, 3.43, Some(at(-10)));
        let turn = store.seat_launch_turn(successor).unwrap().unwrap();
        assert_eq!(
            turn,
            SeatLaunchTurnV1 {
                cost_usd: Some(3.43),
                tool_calls: 3,
                settled: true,
            }
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn a_running_launch_turn_is_unsettled_with_the_calls_so_far() {
        let store = metered_store();
        let successor = session(&store);
        tool_use(&store, successor, 1, at(-50));
        launch_invocation(&store, successor, 1.0, None);
        let turn = store.seat_launch_turn(successor).unwrap().unwrap();
        assert_eq!(turn.cost_usd, None);
        assert_eq!(turn.tool_calls, 1);
        assert!(!turn.settled);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn an_archived_seat_reports_no_launch_turn_because_its_tool_rows_may_be_cold() {
        let store = metered_store();
        let successor = session(&store);
        tool_use(&store, successor, 1, at(-50));
        launch_invocation(&store, successor, 2.0, Some(at(-10)));
        store
            .conn
            .execute(
                "UPDATE sessions SET status='Archived' WHERE id=?1",
                params![successor.to_string()],
            )
            .unwrap();
        assert_eq!(store.seat_launch_turn(successor).unwrap(), None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn a_session_that_was_not_a_rotation_child_has_no_launch_turn() {
        let store = metered_store();
        let plain = session(&store);
        tool_use(&store, plain, 1, at(-5));
        assert_eq!(store.seat_launch_turn(plain).unwrap(), None);
    }
}
