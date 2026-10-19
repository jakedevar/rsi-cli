//! Failed message receipts are durable notice obligations (#2456).
//!
//! Receipt settlement and successful notice materialization share a transaction.
//! If the notice writer fails, retain the terminal receipt anyway: replaying an
//! uncertain remote effect to repair a local notification would be unsafe. The
//! dispatcher reconciles these retained obligations after restart. No payload,
//! remote error text, or peer label is copied into a notice.
//!
//! A message still `queued` past `SATELLITE_MESSAGE_STALL_NOTICE_SECS` also
//! notifies its sender, once (#2566): the sender then knows the instruction is
//! waiting, and on what, before it expires. That notice is a separate subject
//! version (`<id>:stalled`), so the later terminal notice is independent.
//!
//! A message whose remote target is itself terminal (failed, archived or
//! deleted) notifies its sender at once (#2744), naming the target's status and
//! stop reason, once per distinct (status, stop reason). It is not repeated as
//! a stalled notice: the sender already knows what the message waits on.

use super::{SATELLITE_MESSAGE_FAILURE_NOTICE_SUBJECT as SUBJECT, Store, parse_uuid, stamp};
use crate::error::Result;
use chrono::{DateTime, Duration, Utc};
use rsi_common::satellite_dispatch::{
    SATELLITE_MESSAGE_STALL_NOTICE_SECS, find_remote_terminal, split_waiting_for,
};
use rusqlite::{OptionalExtension, params};
use serde_json::json;
use uuid::Uuid;

const PAGE: usize = 64;

/// Subject-version suffix of the stalled-message notice (#2566).
const STALLED_SUFFIX: &str = ":stalled";

#[derive(Clone, Copy, PartialEq, Eq)]
enum NoticeKind {
    /// The message settled `failed` or `expired` (#2456).
    Terminal,
    /// The message is still `queued` past the stall threshold (#2566).
    Stalled,
    /// The message is `queued` and its remote target is terminal (#2744).
    RemoteTerminal,
}

impl Store {
    // Savepoint isolates notification failure from remote-effect settlement.
    pub(super) fn try_satellite_message_notice(&self, id: Uuid) {
        if let Err(error) = self.materialize_satellite_message_notice(id, NoticeKind::Terminal) {
            tracing::warn!(message_id=%id, %error, "satellite message notice retained for reconciliation");
        }
    }

    /// #2744: notify the sender that the remote target is terminal.
    pub(super) fn try_remote_terminal_notice(&self, id: Uuid) {
        if let Err(error) =
            self.materialize_satellite_message_notice(id, NoticeKind::RemoteTerminal)
        {
            tracing::warn!(message_id=%id, %error, "satellite remote-terminal notice retried next attempt");
        }
    }

    /// Scan a bounded page; the caller carries the cursor so an unauthorized
    /// sender cannot starve later obligations. Restart safely starts at the head.
    pub fn reconcile_satellite_message_notices(&self, after: Option<Uuid>) -> Result<Option<Uuid>> {
        let mut statement = self.conn.prepare(
            "SELECT m.id FROM satellite_messages m
             WHERE m.state IN ('failed','expired') AND m.id>?1
               AND NOT EXISTS (SELECT 1 FROM harness_manager_notices n
                 LEFT JOIN harness_manager_scopes s ON s.project_id=n.project_id
                 WHERE n.kind='ledger_change' AND n.subject_id=?2 AND n.subject_version=m.id
                   AND (n.settled_at IS NOT NULL OR
                     (n.retired_at IS NULL AND n.manager_session_id=s.manager_session_id
                      AND n.scope_version=s.row_version)))
             ORDER BY m.id LIMIT ?3",
        )?;
        let rows = statement
            .query_map(
                params![
                    after.map(|id| id.to_string()).unwrap_or_default(),
                    SUBJECT,
                    PAGE
                ],
                |row| row.get::<_, String>(0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        for id in &rows {
            self.materialize_satellite_message_notice(parse_uuid(id)?, NoticeKind::Terminal)?;
        }
        Ok(if rows.len() == PAGE {
            rows.last().map(|id| parse_uuid(id)).transpose()?
        } else {
            None
        })
    }

    /// #2566: notify the sender of each message that has been `queued` for
    /// `SATELLITE_MESSAGE_STALL_NOTICE_SECS` or longer, once. Bounded page with a
    /// caller-held cursor, like [`Self::reconcile_satellite_message_notices`].
    /// Never changes the message: delivery continues, and the terminal notice
    /// (if it later fails or expires) is a separate obligation.
    pub fn notify_stalled_satellite_messages(
        &self,
        now: DateTime<Utc>,
        after: Option<Uuid>,
    ) -> Result<Option<Uuid>> {
        let cutoff = stamp(now - Duration::seconds(SATELLITE_MESSAGE_STALL_NOTICE_SECS));
        let mut statement = self.conn.prepare(
            "SELECT m.id FROM satellite_messages m
             WHERE m.state='queued' AND m.created_at<=?1 AND m.id>?2
               AND instr(COALESCE(m.safe_error_class,''),'remote_terminal|')<>1
               AND NOT EXISTS (SELECT 1 FROM harness_manager_notices n
                 LEFT JOIN harness_manager_scopes s ON s.project_id=n.project_id
                 WHERE n.kind='ledger_change' AND n.subject_id=?3
                   AND n.subject_version=m.id||?4
                   AND (n.settled_at IS NOT NULL OR
                     (n.retired_at IS NULL AND n.manager_session_id=s.manager_session_id
                      AND n.scope_version=s.row_version)))
             ORDER BY m.id LIMIT ?5",
        )?;
        let rows = statement
            .query_map(
                params![
                    cutoff,
                    after.map(|id| id.to_string()).unwrap_or_default(),
                    SUBJECT,
                    STALLED_SUFFIX,
                    PAGE
                ],
                |row| row.get::<_, String>(0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        for id in &rows {
            if let Err(error) =
                self.materialize_satellite_message_notice(parse_uuid(id)?, NoticeKind::Stalled)
            {
                tracing::warn!(message_id=%id, %error, "satellite stalled-message notice retried next round");
            }
        }
        Ok(if rows.len() == PAGE {
            rows.last().map(|id| parse_uuid(id)).transpose()?
        } else {
            None
        })
    }

    fn materialize_satellite_message_notice(&self, id: Uuid, kind: NoticeKind) -> Result<()> {
        self.conn
            .execute_batch("SAVEPOINT satellite_message_notice")?;
        let result = self.satellite_message_notice_in_transaction(id, kind);
        match result {
            Ok(()) => self
                .conn
                .execute_batch("RELEASE satellite_message_notice")?,
            Err(error) => {
                self.conn.execute_batch(
                    "ROLLBACK TO satellite_message_notice; RELEASE satellite_message_notice",
                )?;
                return Err(error);
            }
        }
        Ok(())
    }

    fn satellite_message_notice_in_transaction(&self, id: Uuid, kind: NoticeKind) -> Result<()> {
        let wanted = match kind {
            NoticeKind::Terminal => "m.state IN ('failed','expired')",
            NoticeKind::Stalled => "m.state='queued'",
            NoticeKind::RemoteTerminal => {
                "m.state='queued' AND instr(COALESCE(m.safe_error_class,''),'remote_terminal|')=1"
            }
        };
        let facts = self
            .conn
            .query_row(
                &format!(
                    "SELECT m.owner_session_id,m.peer_id,m.state,m.safe_error_class,s.project_id,
                            m.attempt_count,m.created_at
                     FROM satellite_messages m JOIN sessions s ON s.id=m.owner_session_id
                     WHERE m.id=?1 AND {wanted}"
                ),
                [id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, i64>(5)?,
                        row.get::<_, String>(6)?,
                    ))
                },
            )
            .optional()?;
        let Some((owner, peer, state, class, Some(project), attempts, created)) = facts else {
            return Ok(());
        };
        // An expired row's column is the reason it was waiting, not an error.
        let class = if state == "expired" { None } else { class };
        let project = parse_uuid(&project)?;
        let Some(config) = self.get_harness_manager_notice_config(project)? else {
            return Ok(());
        };
        // Never guess an unrelated replacement manager. Only the sender's
        // published, unambiguous lineage can consume the retained obligation.
        if config.is_revoked()
            || config.current_session_id.is_none()
            || self.manager_lineage_tip(parse_uuid(&owner)?).ok() != config.current_session_id
            || self
                .get_harness_manager_policy(project)?
                .is_none_or(|policy| policy.revoked)
        {
            return Ok(());
        }
        let version = match kind {
            NoticeKind::Terminal => id.to_string(),
            NoticeKind::Stalled => format!("{id}{STALLED_SUFFIX}"),
            // One notice per distinct (status, stop reason) the target shows.
            NoticeKind::RemoteTerminal => match class.as_deref().and_then(find_remote_terminal) {
                Some(token) => format!("{id}:{token}"),
                None => return Ok(()),
            },
        };
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM harness_manager_notices
             WHERE kind='ledger_change' AND subject_id=?1 AND subject_version=?2
               AND (settled_at IS NOT NULL OR
                 (retired_at IS NULL AND manager_session_id=?3 AND scope_version=?4)))",
            params![
                SUBJECT,
                version,
                config.manager_session_id.to_string(),
                config.row_version
            ],
            |row| row.get(0),
        )?;
        if exists {
            return Ok(());
        }
        let (job, _, _) = self.manager_action_watch_identity(&config);
        self.ensure_manager_action_watch(&config, &version)?;
        let at = stamp(Utc::now());
        self.conn.execute(
            "INSERT INTO harness_manager_notices
             (id,job_id,project_id,manager_session_id,scope_version,epic_id,direction,
              source_session_id,recipient_session_id,kind,subject_id,subject_version,
              state_json,recorded_at,queued_at)
             VALUES(?1,?2,?3,?4,?5,NULL,'to_manager',?6,?4,'ledger_change',?7,?8,?9,?10,?10)",
            params![
                Uuid::new_v4().to_string(),
                job.to_string(),
                project.to_string(),
                config.manager_session_id.to_string(),
                config.row_version,
                owner,
                SUBJECT,
                version,
                match kind {
                    NoticeKind::Terminal => serde_json::to_string(&json!({
                        "record_kind": SUBJECT, "message_id": id, "peer_id": peer,
                        "terminal_state": state, "safe_error_class": class,
                        "automatic_replay": false,
                    }))?,
                    NoticeKind::Stalled => serde_json::to_string(&json!({
                        "record_kind": SUBJECT, "message_id": id, "peer_id": peer,
                        "state": "queued", "stalled": true,
                        "waiting_for": class, "attempt_count": attempts,
                        "queued_since": created,
                        "automatic_replay": false,
                    }))?,
                    NoticeKind::RemoteTerminal => {
                        let (waiting_for, status, reason) =
                            split_waiting_for(class.as_deref().unwrap_or_default());
                        serde_json::to_string(&json!({
                            "record_kind": SUBJECT, "message_id": id, "peer_id": peer,
                            "state": "queued", "waiting_for": waiting_for,
                            "remote_status": status, "remote_stop_reason": reason,
                            "attempt_count": attempts, "queued_since": created,
                            "automatic_replay": false,
                        }))?
                    }
                },
                at
            ],
        )?;
        self.refresh_manager_notice_job(job)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::harness_manager::{
        AgentManagerInboxRequestV1, ConfigureHarnessManagerRequestV1,
    };
    use rsi_common::harness_manager_v2::{ConfigureHarnessManagerPolicyRequestV2, ManagerPolicyV2};
    use rsi_common::satellite::{
        SatellitePeerConfigV1, SatellitePutPeerRequestV1, SatelliteUuidV1,
    };
    use rsi_common::satellite_dispatch::AgentSendSatelliteMessageRequestV1;
    use rsi_common::{Session, SessionStatus};

    fn manager(store: &Store) -> Session {
        let mut session = crate::store::tests::make_test_session();
        session.project_id = Some(crate::store::d04_test_project_id());
        session.status = SessionStatus::Completed;
        if store
            .get_project(session.project_id.unwrap())
            .unwrap()
            .is_none()
        {
            let at = Utc::now();
            store
                .insert_project(&rsi_common::types::Project {
                    id: session.project_id.unwrap(),
                    name: "Notice test".into(),
                    path: None,
                    description: None,
                    color: rsi_common::types::Project::DEFAULT_COLOR.into(),
                    context_files: None,
                    created_at: at,
                    updated_at: at,
                })
                .unwrap();
        }
        store.insert_session(&session).unwrap();
        let config = store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                project_id: session.project_id.unwrap(),
                session_id: session.id,
                epic_ids: None,
                group_ids: vec![],
                expected_row_version: 0,
            })
            .unwrap();
        store
            .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
                project_id: config.project_id,
                expected_scope_version: config.row_version,
                expected_policy_version: 0,
                idempotency_key: "notice-test-policy".into(),
                policy: ManagerPolicyV2::default(),
            })
            .unwrap();
        session
    }

    fn queue(store: &Store, owner: Uuid) -> (Uuid, Uuid) {
        let peer = Uuid::new_v4();
        let remote = Uuid::new_v4();
        let revision = store.satellite_registry_revision().unwrap();
        store
            .put_satellite_peer(
                &SatellitePutPeerRequestV1 {
                    expected_registry_revision: revision,
                    repair_quarantine: false,
                    peer: SatellitePeerConfigV1 {
                        peer_id: SatelliteUuidV1(peer),
                        label: "peer".into(),
                        expected_installation_id: Some(SatelliteUuidV1(Uuid::new_v4())),
                        enabled: true,
                        read_enabled: true,
                        dispatch_enabled: true,
                    },
                },
                std::path::Path::new("/tmp/satellites"),
            )
            .unwrap();
        store
            .put_satellite_peer_scope(revision + 1, peer, &[SatelliteUuidV1(remote)])
            .unwrap();
        let id = store
            .queue_satellite_message(
                owner,
                &AgentSendSatelliteMessageRequestV1 {
                    peer_id: SatelliteUuidV1(peer),
                    remote_session_id: SatelliteUuidV1(remote),
                    scope_project_id: None,
                    message: "private payload".into(),
                    idempotency_key: peer.to_string(),
                    expires_at: None,
                    issue_refs: Vec::new(),
                },
                Utc::now(),
            )
            .unwrap()
            .message_id;
        (id, peer)
    }

    fn rotate(store: &Store, previous: &Session) -> Session {
        let mut next = crate::store::tests::make_test_session();
        next.project_id = previous.project_id;
        next.parent_id = previous.parent_id;
        next.continued_from = Some(previous.id);
        next.rotation_depth = previous.rotation_depth + 1;
        next.status = SessionStatus::Completed;
        store.insert_session(&next).unwrap();
        store
            .update_session_status(previous.id, SessionStatus::Archived)
            .unwrap();
        assert!(
            store
                .record_harness_manager_rotation(previous.id, next.id)
                .unwrap()
        );
        next
    }

    fn inbox(
        store: &Store,
        owner: Uuid,
    ) -> Vec<rsi_common::harness_manager::HarnessManagerNoticeV1> {
        store
            .manager_inbox(owner, &AgentManagerInboxRequestV1::default())
            .unwrap()
            .notices
            .into_iter()
            .filter(|n| n.subject_id == SUBJECT)
            .collect()
    }

    fn assert_wake(store: &Store, session: &Session, id: Uuid) {
        let job = store
            .manager_notice_job_for_subject("ledger_change", SUBJECT, &id.to_string())
            .unwrap()
            .unwrap();
        assert!(store.get_scheduled_job(&job).unwrap().unwrap().enabled);
        assert_eq!(
            store
                .harness_manager_watch_route(job)
                .unwrap()
                .map(|(_, recipient)| recipient),
            Some(session.id)
        );
        assert_eq!(store.manager_notice_undelivered_count(job).unwrap(), 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn terminal_mail_notifies_current_sender_once_across_restart_and_rotation() {
        for terminal in ["expired", "target_not_authorized", "delivery_uncertain"] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("mail.db");
            let (sender, id, peer) = {
                let store = Store::open(&path).unwrap();
                let sender = manager(&store);
                let (id, peer) = queue(&store, sender.id);
                let rotated = rotate(&store, &sender);
                if terminal == "expired" {
                    assert_eq!(
                        store
                            .expire_satellite_messages(Utc::now() + chrono::Duration::hours(1))
                            .unwrap(),
                        1
                    );
                } else {
                    assert!(
                        store
                            .settle_satellite_message(id, "failed", Some(terminal), Utc::now())
                            .unwrap()
                    );
                }
                // The idle sender's current rotation receives the event wake,
                // while the outbox is terminal before the process restarts.
                assert_wake(&store, &rotated, id);
                (rotated, id, peer)
            };
            let store = Store::open(&path).unwrap();
            let next = rotate(&store, &sender);
            store.reconcile_satellite_message_notices(None).unwrap();
            assert_wake(&store, &next, id);
            let notices = inbox(&store, next.id);
            assert_eq!(notices.len(), 1);
            assert_eq!(notices[0].recipient_session_id, next.id);
            assert_eq!(
                notices[0].state,
                json!({
                    "record_kind": SUBJECT, "message_id": id, "peer_id": peer,
                    "terminal_state": if terminal == "expired" { "expired" } else { "failed" },
                    "safe_error_class": if terminal == "expired" { None } else { Some(terminal) },
                    "automatic_replay": false,
                })
            );
            assert!(store.queued_satellite_messages(10).unwrap().is_empty());
            assert_eq!(
                store
                    .expire_satellite_messages(Utc::now() + chrono::Duration::hours(2))
                    .unwrap(),
                0
            );
            assert!(
                !store
                    .settle_satellite_message(id, "failed", Some("late"), Utc::now())
                    .unwrap()
            );
            let last = rotate(&store, &next);
            store.reconcile_satellite_message_notices(None).unwrap();
            assert!(inbox(&store, last.id).is_empty());
            assert_eq!(
                store
                    .conn
                    .query_row(
                        "SELECT count(*) FROM harness_manager_notices WHERE subject_id=?1",
                        [SUBJECT],
                        |r| r.get::<_, i64>(0)
                    )
                    .unwrap(),
                1
            );
        }
    }

    /// #2566: a message still queued past the stall threshold notifies its
    /// sender once, with why it waits and how often it was tried; the later
    /// terminal notice is independent of it.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn a_stalled_queued_message_notifies_its_sender_once_with_the_wait_reason() {
        let store = Store::open_in_memory().unwrap();
        let sender = manager(&store);
        let (id, peer) = queue(&store, sender.id);
        store
            .note_satellite_message_attempt(id, Utc::now())
            .unwrap();
        store
            .note_satellite_message_wait(id, "remote_busy", Utc::now())
            .unwrap();
        let soon = Utc::now() + chrono::Duration::minutes(1);
        store.notify_stalled_satellite_messages(soon, None).unwrap();
        assert!(inbox(&store, sender.id).is_empty());

        let later = Utc::now()
            + chrono::Duration::seconds(
                rsi_common::satellite_dispatch::SATELLITE_MESSAGE_STALL_NOTICE_SECS + 5,
            );
        store
            .notify_stalled_satellite_messages(later, None)
            .unwrap();
        store
            .notify_stalled_satellite_messages(later, None)
            .unwrap();
        let notices = inbox(&store, sender.id);
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].recipient_session_id, sender.id);
        let state = &notices[0].state;
        assert_eq!(state["message_id"], json!(id));
        assert_eq!(state["peer_id"], json!(peer));
        assert_eq!(state["stalled"], json!(true));
        assert_eq!(state["waiting_for"], json!("remote_busy"));
        assert_eq!(state["attempt_count"], json!(1));
        assert!(state.get("payload").is_none());
        // The message itself is untouched and still deliverable.
        assert_eq!(store.queued_satellite_messages(10).unwrap().len(), 1);

        // Failing later is a separate, terminal obligation.
        assert!(
            store
                .settle_satellite_message(id, "failed", Some("delivery_uncertain"), Utc::now())
                .unwrap()
        );
        assert_eq!(
            store
                .conn
                .query_row(
                    "SELECT count(*) FROM harness_manager_notices WHERE subject_id=?1",
                    [SUBJECT],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            2
        );
        // A settled message is never announced as stalled.
        store
            .notify_stalled_satellite_messages(later, None)
            .unwrap();
        assert_eq!(
            store
                .conn
                .query_row(
                    "SELECT count(*) FROM harness_manager_notices WHERE subject_id=?1",
                    [SUBJECT],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            2
        );
    }

    /// #2744: a terminal remote target notifies the sender at once with the
    /// target's status and stop reason, once per distinct (status, reason), and
    /// the later stalled sweep does not repeat it.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn a_terminal_remote_target_notifies_once_at_once_with_status_and_reason() {
        let notice_count = |store: &Store| {
            store
                .conn
                .query_row(
                    "SELECT count(*) FROM harness_manager_notices WHERE subject_id=?1",
                    [SUBJECT],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap()
        };
        let store = Store::open_in_memory().unwrap();
        let sender = manager(&store);
        let (id, peer) = queue(&store, sender.id);
        let failed = "remote_terminal|failed|provider_error:codex_unauthorized";
        for _ in 0..3 {
            store
                .note_satellite_message_attempt(id, Utc::now())
                .unwrap();
            store
                .note_satellite_message_wait(id, failed, Utc::now())
                .unwrap();
        }
        let notices = inbox(&store, sender.id);
        assert_eq!(notices.len(), 1);
        assert_eq!(
            notices[0].state,
            json!({
                "record_kind": SUBJECT, "message_id": id, "peer_id": peer,
                "state": "queued", "waiting_for": "remote_terminal",
                "remote_status": "failed",
                "remote_stop_reason": "provider_error:codex_unauthorized",
                "attempt_count": 1, "queued_since": notices[0].state["queued_since"],
                "automatic_replay": false,
            })
        );
        // The stalled sweep leaves a terminal-target row alone.
        let later = Utc::now()
            + chrono::Duration::seconds(
                rsi_common::satellite_dispatch::SATELLITE_MESSAGE_STALL_NOTICE_SECS + 5,
            );
        store
            .notify_stalled_satellite_messages(later, None)
            .unwrap();
        assert_eq!(notice_count(&store), 1);
        // A different status is a new episode; a busy wait notifies no one.
        store
            .note_satellite_message_wait(id, "remote_busy", Utc::now())
            .unwrap();
        assert_eq!(notice_count(&store), 1);
        store
            .note_satellite_message_wait(id, "remote_terminal|archived|none", Utc::now())
            .unwrap();
        assert_eq!(notice_count(&store), 2);
        // Still queued and deliverable.
        assert_eq!(store.queued_satellite_messages(10).unwrap().len(), 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn local_notice_failure_retains_uncertain_effect_without_replay_and_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        let (sender, id) = {
            let store = Store::open(&path).unwrap();
            let sender = manager(&store);
            let (id, _) = queue(&store, sender.id);
            store.conn.execute_batch("CREATE TEMP TRIGGER fail_notice BEFORE INSERT ON harness_manager_notices BEGIN SELECT RAISE(ABORT,'injected notice failure'); END;").unwrap();
            assert!(
                store
                    .settle_satellite_message(id, "failed", Some("delivery_uncertain"), Utc::now())
                    .unwrap()
            );
            assert_eq!(
                store.satellite_message_state(id).unwrap().unwrap().0,
                "failed"
            );
            assert!(store.queued_satellite_messages(10).unwrap().is_empty());
            assert!(inbox(&store, sender.id).is_empty());
            (sender, id)
        };
        let store = Store::open(&path).unwrap();
        let current = rotate(&store, &sender);
        store.reconcile_satellite_message_notices(None).unwrap();
        assert_wake(&store, &current, id);
        assert_eq!(inbox(&store, current.id).len(), 1);
        store.reconcile_satellite_message_notices(None).unwrap();
        assert!(inbox(&store, current.id).is_empty());
        assert!(store.queued_satellite_messages(10).unwrap().is_empty());
    }
}
