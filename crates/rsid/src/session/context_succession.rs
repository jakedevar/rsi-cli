//! #959: context pressure never interrupts an agent mid-turn or loses its task.
//!
//! Workers (children, reviewers, operator sessions) are never rotated at an
//! automatic context threshold; they keep working through provider-native or
//! Harness in-loop compaction. Coordinators, the appointed manager's live seat
//! and Epic leads, are not interrupted either: the crossing records one durable
//! request, and at the coordinator's next idle boundary the daemon delivers a
//! `<rsid-daemon-message source="context-succession">` asking it to pass its
//! seat through the existing authority-preserving succession (`succeed_manager`
//! or `AgentReserveSuccessor`). Manual rotation is unaffected.

use crate::error::Result;
use crate::store::Store;
use chrono::{DateTime, Duration, Utc};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A session whose seat passes through authority-preserving succession.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub(super) enum CoordinatorRole {
    /// The live lineage tip of its project's appointed manager.
    Manager,
    /// The current lead of an Epic.
    Lead { epic_id: Uuid },
}

/// The coordinator role `session_id` holds now, or `None` for a worker.
pub(super) fn coordinator_role(store: &Store, session_id: Uuid) -> Result<Option<CoordinatorRole>> {
    let epic: Option<String> = store
        .conn
        .query_row(
            "SELECT id FROM sessions WHERE session_kind='Epic' AND lead_session_id=?1
               AND status NOT IN ('Archived','Deleted') ORDER BY id LIMIT 1",
            [session_id.to_string()],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(epic) = epic {
        let epic_id = Uuid::parse_str(&epic)
            .map_err(|error| crate::error::DaemonError::Store(error.to_string()))?;
        return Ok(Some(CoordinatorRole::Lead { epic_id }));
    }
    let Some(project_id) = store.get_session(session_id)?.and_then(|s| s.project_id) else {
        return Ok(None);
    };
    let manager = store
        .get_harness_manager_notice_config(project_id)?
        .and_then(|config| config.current_session_id);
    Ok((manager == Some(session_id)).then_some(CoordinatorRole::Manager))
}

/// `daemon_settings` key prefix of a seat's succession request.
const MARKER_PREFIX: &str = "context_succession_due:";
/// An ignored request is delivered again after this long.
pub(super) const RESEND_AFTER_SECS: i64 = 15 * 60;
/// Deliveries per request before the daemon escalates instead.
pub(super) const MAX_DELIVERIES: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum RequestState {
    Due,
    Cleared,
}

/// One durable succession request per seat and threshold crossing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct SuccessionRequest {
    pub state: RequestState,
    pub coordinator: CoordinatorRole,
    pub context_pct: f64,
    pub threshold_pct: f64,
    pub crossed_at: DateTime<Utc>,
    pub deliveries: u32,
    pub last_delivered_at: Option<DateTime<Utc>>,
    pub escalated: bool,
    pub cleared_reason: Option<String>,
}

fn marker_key(session_id: Uuid) -> String {
    format!("{MARKER_PREFIX}{session_id}")
}

pub(super) fn succession_request(
    store: &Store,
    session_id: Uuid,
) -> Result<Option<SuccessionRequest>> {
    store
        .get_daemon_setting(&marker_key(session_id))?
        .map(|raw| {
            serde_json::from_str(&raw)
                .map_err(|error| crate::error::DaemonError::Store(error.to_string()))
        })
        .transpose()
}

fn put_request(store: &Store, session_id: Uuid, request: &SuccessionRequest) -> Result<()> {
    store.set_daemon_setting(&marker_key(session_id), &serde_json::to_string(request)?)
}

/// Record a crossing. Returns `false` when a request is already due for this
/// seat, so one crossing yields one request.
pub(super) fn record_succession_due(
    store: &Store,
    session_id: Uuid,
    coordinator: CoordinatorRole,
    context_pct: f64,
    threshold_pct: f64,
    now: DateTime<Utc>,
) -> Result<bool> {
    if succession_request(store, session_id)?.is_some_and(|r| r.state == RequestState::Due) {
        return Ok(false);
    }
    put_request(
        store,
        session_id,
        &SuccessionRequest {
            state: RequestState::Due,
            coordinator,
            context_pct,
            threshold_pct,
            crossed_at: now,
            deliveries: 0,
            last_delivered_at: None,
            escalated: false,
            cleared_reason: None,
        },
    )?;
    Ok(true)
}

/// Settle a due request (the seat passed, or occupancy fell after
/// compaction). The row is kept as a cleared record, never deleted.
pub(super) fn clear_succession(store: &Store, session_id: Uuid, reason: &str) -> Result<bool> {
    let Some(mut request) = succession_request(store, session_id)? else {
        return Ok(false);
    };
    if request.state != RequestState::Due {
        return Ok(false);
    }
    request.state = RequestState::Cleared;
    request.cleared_reason = Some(reason.into());
    put_request(store, session_id, &request)?;
    Ok(true)
}

#[derive(Debug, Clone, PartialEq)]
pub(super) enum SuccessionAction {
    /// Ask the idle seat to pass itself on.
    Deliver { session_id: Uuid, message: String },
    /// The seat ignored every delivery: tell the operator once.
    Escalate { session_id: Uuid, message: String },
}

/// What the idle-boundary pass should do now. `is_active` reports a session
/// that is mid-turn; it is never sent anything. Requests whose seat has
/// passed are cleared here.
pub(super) fn due_succession_actions(
    store: &Store,
    now: DateTime<Utc>,
    is_active: impl Fn(Uuid) -> bool,
) -> Result<Vec<SuccessionAction>> {
    let keys: Vec<String> = {
        let mut statement = store
            .conn
            .prepare("SELECT key FROM daemon_settings WHERE key LIKE ?1 ORDER BY key")?;
        statement
            .query_map([format!("{MARKER_PREFIX}%")], |row| row.get(0))?
            .collect::<std::result::Result<_, _>>()?
    };
    let resend = Duration::seconds(RESEND_AFTER_SECS);
    let mut actions = Vec::new();
    for key in keys {
        let Some(session_id) = key
            .strip_prefix(MARKER_PREFIX)
            .and_then(|id| Uuid::parse_str(id).ok())
        else {
            continue;
        };
        let Some(request) = succession_request(store, session_id)? else {
            continue;
        };
        if request.state != RequestState::Due {
            continue;
        }
        if coordinator_role(store, session_id)? != Some(request.coordinator) {
            clear_succession(store, session_id, "seat_passed")?;
            continue;
        }
        if is_active(session_id) {
            continue;
        }
        let waited = request
            .last_delivered_at
            .is_none_or(|at| now.signed_duration_since(at) >= resend);
        if !waited {
            continue;
        }
        if request.deliveries < MAX_DELIVERIES {
            actions.push(SuccessionAction::Deliver {
                session_id,
                message: succession_message(&request),
            });
        } else if !request.escalated {
            actions.push(SuccessionAction::Escalate {
                session_id,
                message: format!(
                    "Context succession for {session_id} was requested {} times since {} and the seat has not passed. Check the session.",
                    request.deliveries,
                    request.crossed_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
                ),
            });
        }
    }
    Ok(actions)
}

pub(super) fn record_delivered(store: &Store, session_id: Uuid, now: DateTime<Utc>) -> Result<()> {
    if let Some(mut request) = succession_request(store, session_id)? {
        request.deliveries += 1;
        request.last_delivered_at = Some(now);
        put_request(store, session_id, &request)?;
    }
    Ok(())
}

pub(super) fn record_escalated(store: &Store, session_id: Uuid) -> Result<()> {
    if let Some(mut request) = succession_request(store, session_id)? {
        request.escalated = true;
        put_request(store, session_id, &request)?;
    }
    Ok(())
}

/// The daemon's request text; `wrap` adds the context-succession envelope.
fn succession_message(request: &SuccessionRequest) -> String {
    let pass = match request.coordinator {
        CoordinatorRole::Manager => {
            "pass the manager seat with AgentManagerControl `succeed_manager` (see the rsi-project-manager skill)"
        }
        CoordinatorRole::Lead { .. } => {
            "pass the Epic lead with `AgentReserveSuccessor` (native `rsi_control_reserve_successor`)"
        }
    };
    format!(
        "Context succession requested: your context reached {:.0}% of its window (threshold {:.0}%). \
         Now, before any new work: commit your changes, write one handoff under thoughts/shared/handoffs/ \
         (list unfinished manager requests first), then {pass}, and end your turn. \
         If you cannot pass the seat now, tell your manager why.",
        request.context_pct, request.threshold_pct
    )
}

impl super::SessionManager {
    /// The idle-boundary pass: deliver each due succession request to its
    /// seat when the seat is idle (a busy seat is refused by the continuation
    /// fence and retried on a later pass), and escalate ignored ones once.
    ///
    /// # Errors
    /// Store failures reading or recording requests. A refused continuation
    /// is not an error: the request stays due for a later pass.
    pub async fn deliver_due_context_successions(&self) -> Result<usize> {
        if !self
            .runtime_config
            .context_rotation_enabled
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return Ok(0);
        }
        let active: std::collections::HashSet<Uuid> =
            self.active.read().await.keys().copied().collect();
        let actions = {
            let store = self.store.lock().await;
            due_succession_actions(&store, Utc::now(), |id| active.contains(&id))?
        };
        let mut delivered = 0;
        for action in actions {
            match action {
                SuccessionAction::Deliver {
                    session_id,
                    message,
                } => {
                    let fenced = async {
                        let fence = self
                            .capture_exact_continuation_fence(
                                session_id,
                                crate::store::manager_actions::fence::ContinuationAuthorityV1::Automated,
                            )
                            .await?;
                        self.continue_fenced(
                            session_id,
                            rsi_common::daemon_message::wrap("context-succession", &message),
                            fence,
                        )
                        .await
                    };
                    match Box::pin(fenced).await {
                        Ok(()) => {
                            record_delivered(&*self.store.lock().await, session_id, Utc::now())?;
                            delivered += 1;
                            tracing::info!(%session_id, "Context succession requested at idle boundary");
                        }
                        Err(error) => tracing::info!(
                            %session_id,
                            %error,
                            "Context succession request deferred"
                        ),
                    }
                }
                SuccessionAction::Escalate {
                    session_id,
                    message,
                } => {
                    record_escalated(&*self.store.lock().await, session_id)?;
                    tracing::warn!(%session_id, "{message}");
                    self.event_bus
                        .publish(crate::bus::DaemonEvent::SystemMessage {
                            level: "warn".into(),
                            message,
                        });
                }
            }
        }
        Ok(delivered)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    use super::*;
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    use rsi_common::types::{SessionKind, SessionStatus};

    /// A lead and its Epic.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    fn lead_of_epic(store: &Store) -> (Uuid, Uuid) {
        let dir = std::path::PathBuf::from("/tmp/issue-959");
        let lead = crate::session::agent_verbs::tests::test_session(Uuid::new_v4(), dir.clone());
        store.insert_session(&lead).unwrap();
        let mut epic = crate::session::agent_verbs::tests::test_session(Uuid::new_v4(), dir);
        epic.session_kind = SessionKind::Epic;
        epic.status = SessionStatus::Completed;
        epic.lead_session_id = Some(lead.id);
        store.insert_session(&epic).unwrap();
        (lead.id, epic.id)
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    fn at(minutes: i64) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-28T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
            + Duration::minutes(minutes)
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn one_request_per_crossing_delivered_idle_resent_after_delay_then_escalated_once() {
        let store = Store::open_in_memory().unwrap();
        let (lead, epic_id) = lead_of_epic(&store);
        let role = CoordinatorRole::Lead { epic_id };
        assert_eq!(coordinator_role(&store, lead).unwrap(), Some(role));
        assert!(record_succession_due(&store, lead, role, 70.0, 65.0, at(0)).unwrap());
        assert!(!record_succession_due(&store, lead, role, 80.0, 65.0, at(1)).unwrap());

        // Mid-turn: nothing is sent.
        assert!(
            due_succession_actions(&store, at(0), |_| true)
                .unwrap()
                .is_empty()
        );
        let actions = due_succession_actions(&store, at(0), |_| false).unwrap();
        let [
            SuccessionAction::Deliver {
                session_id,
                message,
            },
        ] = actions.as_slice()
        else {
            panic!("one delivery expected: {actions:?}");
        };
        assert_eq!(*session_id, lead);
        assert!(message.contains("AgentReserveSuccessor"), "{message}");
        assert!(message.contains("70%"), "{message}");

        record_delivered(&store, lead, at(0)).unwrap();
        assert!(
            due_succession_actions(&store, at(14), |_| false)
                .unwrap()
                .is_empty()
        );
        assert!(matches!(
            due_succession_actions(&store, at(15), |_| false)
                .unwrap()
                .as_slice(),
            [SuccessionAction::Deliver { .. }]
        ));
        record_delivered(&store, lead, at(15)).unwrap();
        record_delivered(&store, lead, at(30)).unwrap();
        assert_eq!(
            succession_request(&store, lead)
                .unwrap()
                .unwrap()
                .deliveries,
            MAX_DELIVERIES
        );
        let actions = due_succession_actions(&store, at(45), |_| false).unwrap();
        assert!(
            matches!(actions.as_slice(), [SuccessionAction::Escalate { session_id, .. }] if *session_id == lead),
            "{actions:?}"
        );
        record_escalated(&store, lead).unwrap();
        assert!(
            due_succession_actions(&store, at(90), |_| false)
                .unwrap()
                .is_empty()
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn passed_seat_clears_its_request_and_a_worker_has_no_role() {
        let store = Store::open_in_memory().unwrap();
        let (lead, epic_id) = lead_of_epic(&store);
        let role = CoordinatorRole::Lead { epic_id };
        record_succession_due(&store, lead, role, 70.0, 65.0, at(0)).unwrap();
        let successor = crate::session::agent_verbs::tests::test_session(
            Uuid::new_v4(),
            std::path::PathBuf::from("/tmp/issue-959"),
        );
        store.insert_session(&successor).unwrap();
        store.set_lead_session(epic_id, Some(successor.id)).unwrap();
        assert_eq!(coordinator_role(&store, lead).unwrap(), None);
        assert!(
            due_succession_actions(&store, at(0), |_| false)
                .unwrap()
                .is_empty()
        );
        let request = succession_request(&store, lead).unwrap().unwrap();
        assert_eq!(request.state, RequestState::Cleared);
        assert_eq!(request.cleared_reason.as_deref(), Some("seat_passed"));
        // The seat's next crossing starts a new request.
        store.set_lead_session(epic_id, Some(lead)).unwrap();
        assert!(record_succession_due(&store, lead, role, 70.0, 65.0, at(5)).unwrap());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn manager_seat_is_asked_to_succeed_the_manager() {
        let request = SuccessionRequest {
            state: RequestState::Due,
            coordinator: CoordinatorRole::Manager,
            context_pct: 91.0,
            threshold_pct: 91.0,
            crossed_at: at(0),
            deliveries: 0,
            last_delivered_at: None,
            escalated: false,
            cleared_reason: None,
        };
        let message = succession_message(&request);
        assert!(message.contains("succeed_manager"), "{message}");
        assert!(message.contains("thoughts/shared/handoffs/"), "{message}");
    }
}
