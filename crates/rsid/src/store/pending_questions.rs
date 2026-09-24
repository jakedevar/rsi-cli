//! Exact producer publication for structured Claude questions.
//!
//! The first commit replaces the display snapshot and invalidates all prior
//! answerable identity. The second atomically inserts the event/provenance and
//! binds it to that publication. A failure or restart between them leaves an
//! explicit unresolved gate; matching question text never repairs identity.

use super::Store;
use crate::error::{DaemonError, Result};
use chrono::{SecondsFormat, Utc};
use rsi_common::closure_kernel::ConversationEventProvenanceV1;
use rsi_common::types::{ConversationEvent, EventType, PendingQuestion};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::{Value, json};
use uuid::Uuid;

fn refused(reason: &str) -> DaemonError {
    DaemonError::InvalidParam(reason.into())
}

/// A daemon-only fence, returned only after the invalidation commit succeeds.
/// It is not accepted from a provider or an agent request.
pub(super) struct PendingQuestionPublication {
    session_id: Uuid,
    id: Uuid,
    epoch: i64,
}

impl Store {
    /// Compatibility and producer invalidation boundary. Keep a retained row
    /// even after clearing, so an older writer cannot resurrect its identity.
    pub(super) fn reserve_pending_question_publication(
        &self,
        session_id: Uuid,
        raw: Option<&str>,
    ) -> Result<PendingQuestionPublication> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
        if tx.execute(
            "UPDATE sessions SET pending_question_json=?1,updated_at=?2 WHERE id=?3",
            params![raw, now, session_id.to_string()],
        )? == 0
        {
            // Retain the legacy setter's no-op for a missing session. A producer
            // bind will still reject this nonexistent publication.
            tx.commit()?;
            return Ok(PendingQuestionPublication {
                session_id,
                id: Uuid::new_v4(),
                epoch: 0,
            });
        }
        let id = Uuid::new_v4();
        let epoch: i64 = tx.query_row(
            "INSERT INTO pending_question_publications
             (session_id,publication_id,epoch,state,question_json,updated_at)
             VALUES(?1,?2,1,?3,?4,?5)
             ON CONFLICT(session_id) DO UPDATE SET
               publication_id=excluded.publication_id,epoch=epoch+1,state=excluded.state,
               question_json=excluded.question_json,conversation_event_id=NULL,
               event_sequence=NULL,tool_use_id=NULL,model_invocation_id=NULL,
               updated_at=excluded.updated_at RETURNING epoch",
            params![
                session_id.to_string(),
                id.to_string(),
                if raw.is_some() {
                    "unresolved"
                } else {
                    "cleared"
                },
                raw,
                now
            ],
            |r| r.get(0),
        )?;
        tx.commit()?;
        Ok(PendingQuestionPublication {
            session_id,
            id,
            epoch,
        })
    }

    /// FIFO persistence worker entry point for a detected provider question.
    /// A failed event insert must never roll back the earlier invalidation.
    pub(crate) fn publish_pending_question_event(
        &self,
        event: &ConversationEvent,
        provenance: Option<&ConversationEventProvenanceV1>,
        question: &PendingQuestion,
    ) -> Result<i64> {
        let raw = serde_json::to_string(question)?;
        let publication =
            self.reserve_pending_question_publication(event.session_id, Some(&raw))?;
        self.bind_pending_question_event(&publication, event, provenance, question)
    }

    fn bind_pending_question_event(
        &self,
        publication: &PendingQuestionPublication,
        event: &ConversationEvent,
        provenance: Option<&ConversationEventProvenanceV1>,
        question: &PendingQuestion,
    ) -> Result<i64> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let raw = serde_json::to_string(question)?;
        let current: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM pending_question_publications p
             JOIN sessions s ON s.id=p.session_id
             WHERE p.session_id=?1 AND p.publication_id=?2 AND p.epoch=?3
               AND p.state='unresolved' AND p.question_json=?4 AND s.pending_question_json=?4
               AND s.provider='Claude')",
            params![
                publication.session_id.to_string(),
                publication.id.to_string(),
                publication.epoch,
                raw
            ],
            |r| r.get(0),
        )?;
        let normalized = event
            .tool_input
            .as_ref()
            .and_then(|input| serde_json::from_value::<PendingQuestion>((**input).clone()).ok());
        if !current
            || event.session_id != publication.session_id
            || event.event_type != EventType::ToolUse
            || event.tool_name.as_deref() != Some("AskUserQuestion")
            || normalized.as_ref() != Some(question)
        {
            return Err(refused("manager_v2_question_publication_changed"));
        }
        let event_id = Self::insert_event_in_transaction(&tx, event, provenance)?;
        let tool_id = event
            .tool_use_id
            .as_deref()
            .filter(|id| !id.trim().is_empty());
        tx.execute(
            "UPDATE pending_question_publications SET state=?1,conversation_event_id=?2,
             event_sequence=?3,tool_use_id=?4,model_invocation_id=?5,updated_at=?6
             WHERE session_id=?7 AND publication_id=?8 AND epoch=?9",
            params![
                if tool_id.is_some() {
                    "published"
                } else {
                    "unresolved"
                },
                event_id,
                event.sequence,
                tool_id,
                provenance.map(|p| p.model_invocation_id.to_string()),
                Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true),
                publication.session_id.to_string(),
                publication.id.to_string(),
                publication.epoch
            ],
        )?;
        // The pending snapshot is part of the event binding transaction as
        // well; the earlier commit is solely its fail-closed publication gate.
        tx.execute(
            "UPDATE sessions SET pending_question_json=?1 WHERE id=?2",
            params![raw, event.session_id.to_string()],
        )?;
        tx.commit()?;
        Ok(event_id)
    }

    /// Read only explicitly published producer identity. This never searches
    /// for a look-alike event. No backfill is possible for a legacy snapshot.
    pub(crate) fn pending_question_target(&self, session_id: Uuid) -> Result<Option<Value>> {
        let raw: Option<String> = self.conn.query_row(
            "SELECT pending_question_json FROM sessions WHERE id=?1",
            [session_id.to_string()],
            |r| r.get(0),
        )?;
        let Some(raw) = raw else {
            return Ok(None);
        };
        let question: PendingQuestion =
            serde_json::from_str(&raw).map_err(|_| refused("manager_v2_question_malformed"))?;
        // All joins use exact producer IDs. A missing event/provenance, a
        // partial publication or an externally changed snapshot is unresolved.
        let target = self.conn.query_row(
            "SELECT p.publication_id,p.epoch,e.id,e.sequence,e.tool_use_id,p.model_invocation_id
             FROM pending_question_publications p
             JOIN sessions s ON s.id=p.session_id
             JOIN conversation_events e ON e.id=p.conversation_event_id AND e.session_id=p.session_id
             LEFT JOIN conversation_event_provenance v ON v.conversation_event_id=e.id
             WHERE p.session_id=?1 AND p.state='published' AND p.question_json=?2
               AND s.provider='Claude' AND e.event_type='ToolUse' AND e.tool_name='AskUserQuestion'
               AND e.sequence=p.event_sequence AND e.tool_use_id=p.tool_use_id
               AND p.model_invocation_id IS v.model_invocation_id",
            params![session_id.to_string(), raw], |r| Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?,
                r.get::<_,i64>(2)?,r.get::<_,i64>(3)?,r.get::<_,String>(4)?,r.get::<_,Option<String>>(5)?)),
        ).optional()?;
        let Some((publication_id, epoch, event_id, sequence, tool_id, invocation_id)) = target
        else {
            return Err(refused("manager_v2_question_identity_unavailable"));
        };
        let stale: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM conversation_events WHERE session_id=?1
             AND role='User' AND sequence>=?2)",
            params![session_id.to_string(), sequence],
            |r| r.get(0),
        )?;
        if stale {
            return Err(refused("manager_v2_question_identity_stale"));
        }
        Ok(Some(json!({"kind":"question","session_id":session_id,
            "publication_id":publication_id,"publication_epoch":epoch,
            "event_id":event_id,"event_sequence":sequence,"tool_use_id":tool_id,
            "model_invocation_id":invocation_id,"question":question})))
    }

    /// Called only after answer provider establishment under the spawn guard.
    /// One transaction compares the complete target and clears exactly that
    /// publication. A newer same-text question remains intact.
    pub(crate) fn clear_pending_question_exact(
        &self,
        session_id: Uuid,
        expected: &Value,
    ) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if self.pending_question_target(session_id)?.as_ref() != Some(expected) {
            return Err(refused("manager_v2_decision_target_changed"));
        }
        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
        tx.execute(
            "UPDATE sessions SET pending_question_json=NULL,updated_at=?1 WHERE id=?2",
            params![now, session_id.to_string()],
        )?;
        tx.execute("UPDATE pending_question_publications SET state='cleared',updated_at=?1 WHERE session_id=?2",
            params![now,session_id.to_string()])?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::closure_kernel::ConversationEventProducerKindV1;
    use rsi_common::types::{QuestionItem, Role, Session, SessionProvider};

    fn fixture(store: &Store) -> Session {
        let mut session = crate::session::agent_verbs::tests::test_session(
            Uuid::new_v4(),
            std::path::PathBuf::from("/var/tmp/question-publication"),
        );
        session.provider = SessionProvider::Claude;
        store.insert_session(&session).unwrap();
        session
    }

    fn question() -> PendingQuestion {
        PendingQuestion {
            questions: vec![QuestionItem {
                question: "Use the migration allocation?".into(),
                header: "Migration".into(),
                options: vec![],
                multi_select: false,
            }],
        }
    }

    fn event(session_id: Uuid, sequence: i32, tool_id: &str) -> ConversationEvent {
        ConversationEvent {
            id: 0,
            session_id,
            sequence,
            event_type: EventType::ToolUse,
            role: Some(Role::Assistant),
            created_at: Utc::now(),
            content: String::new(),
            tool_name: Some("AskUserQuestion".into()),
            tool_input: Some(Box::new(json!(question()))),
            offload_id: None,
            tool_use_id: Some(tool_id.into()),
            metadata: None,
        }
    }

    fn unresolved(store: &Store, session: Uuid) {
        assert!(
            store
                .pending_question_target(session)
                .unwrap_err()
                .to_string()
                .contains("question_identity_unavailable")
        );
        assert_eq!(
            store
                .get_session(session)
                .unwrap()
                .unwrap()
                .pending_question,
            Some(question())
        );
    }

    fn provenance(store: &Store, session: Uuid) -> ConversationEventProvenanceV1 {
        let invocation = Uuid::new_v4();
        // Isolated fixture row; the tested production writer verifies ownership
        // and inserts the real immutable event-provenance relation.
        store.conn.execute("INSERT INTO model_invocations
            (id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,
             provider,model,trigger_source,session_id,policy_snapshot_json,usage_confidence,created_at)
             VALUES(?1,'session_launch_fresh','session_lifecycle','foreground','paid_capable',
             'admitted','running','Claude','question-fixture','question_fixture',?2,'{}','unavailable',?3)",
            params![invocation.to_string(),session.to_string(),Utc::now().to_rfc3339()]).unwrap();
        ConversationEventProvenanceV1 {
            producer_kind: ConversationEventProducerKindV1::ProviderOther,
            model_invocation_id: invocation,
            provider_event_type: "assistant".into(),
        }
    }

    #[test]
    fn pending_question_publication_same_text_reservation_blocks_old_tool_before_insert() {
        let store = Store::open_in_memory().unwrap();
        let session = fixture(&store).id;
        store
            .publish_pending_question_event(&event(session, 1, "ask-old"), None, &question())
            .unwrap();
        let old = store.pending_question_target(session).unwrap().unwrap();
        let reserved = store
            .reserve_pending_question_publication(
                session,
                Some(&serde_json::to_string(&question()).unwrap()),
            )
            .unwrap();
        unresolved(&store, session);
        assert_eq!(store.load_events_since(session, None).unwrap().len(), 1);
        assert!(store.clear_pending_question_exact(session, &old).is_err());
        let id = store
            .bind_pending_question_event(
                &reserved,
                &event(session, 2, "ask-new"),
                None,
                &question(),
            )
            .unwrap();
        let new = store.pending_question_target(session).unwrap().unwrap();
        assert_eq!(new["event_id"], id);
        assert_eq!(new["tool_use_id"], "ask-new");
        assert_eq!(new["question"], old["question"]);
        assert_ne!(new["publication_id"], old["publication_id"]);
        assert!(
            new["publication_epoch"].as_i64().unwrap() > old["publication_epoch"].as_i64().unwrap()
        );
        assert!(store.clear_pending_question_exact(session, &old).is_err());
        assert_eq!(
            store.pending_question_target(session).unwrap(),
            Some(new.clone())
        );
        store.clear_pending_question_exact(session, &new).unwrap();
        assert_eq!(store.pending_question_target(session).unwrap(), None);
        assert_eq!(
            store
                .get_session(session)
                .unwrap()
                .unwrap()
                .pending_question,
            None
        );
    }

    #[test]
    fn pending_question_publication_failed_event_insert_stays_unresolved_after_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("questions.db");
        let store = Store::open(&path).unwrap();
        let session = fixture(&store).id;
        store
            .publish_pending_question_event(&event(session, 1, "ask-old"), None, &question())
            .unwrap();
        let old = store.pending_question_target(session).unwrap().unwrap();
        store.conn.execute_batch("CREATE TRIGGER fail_question_event BEFORE INSERT ON conversation_events
            WHEN NEW.tool_use_id='ask-new' BEGIN SELECT RAISE(ABORT,'question insert failpoint'); END;").unwrap();
        let err = store
            .publish_pending_question_event(&event(session, 2, "ask-new"), None, &question())
            .unwrap_err();
        assert!(err.to_string().contains("question insert failpoint"));
        unresolved(&store, session);
        assert_eq!(store.load_events_since(session, None).unwrap().len(), 1);
        drop(store);
        let reopened = Store::open(&path).unwrap();
        unresolved(&reopened, session);
        assert!(
            reopened
                .clear_pending_question_exact(session, &old)
                .is_err()
        );
        reopened
            .conn
            .execute_batch("DROP TRIGGER fail_question_event;")
            .unwrap();
        reopened
            .publish_pending_question_event(&event(session, 3, "ask-current"), None, &question())
            .unwrap();
        assert_eq!(
            reopened.pending_question_target(session).unwrap().unwrap()["tool_use_id"],
            "ask-current"
        );
    }

    #[test]
    fn pending_question_publication_event_and_invocation_bind_or_roll_back_together() {
        let store = Store::open_in_memory().unwrap();
        let session = fixture(&store).id;
        let source = provenance(&store, session);
        let id = store
            .publish_pending_question_event(
                &event(session, 1, "ask-old"),
                Some(&source),
                &question(),
            )
            .unwrap();
        let old = store.pending_question_target(session).unwrap().unwrap();
        assert_eq!(old["event_id"], id);
        assert_eq!(
            old["model_invocation_id"],
            source.model_invocation_id.to_string()
        );
        store.conn.execute_batch("CREATE TRIGGER fail_question_binding BEFORE UPDATE ON pending_question_publications
            WHEN NEW.state='published' BEGIN SELECT RAISE(ABORT,'question bind failpoint'); END;").unwrap();
        assert!(
            store
                .publish_pending_question_event(
                    &event(session, 2, "ask-new"),
                    Some(&source),
                    &question()
                )
                .is_err()
        );
        unresolved(&store, session);
        assert_eq!(store.load_events_since(session, None).unwrap().len(), 1);
        assert_eq!(
            store
                .conn
                .query_row(
                    "SELECT count(*) FROM conversation_event_provenance",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            1
        );
        store
            .conn
            .execute_batch("DROP TRIGGER fail_question_binding;")
            .unwrap();
        let other = fixture(&store).id;
        let wrong_source = provenance(&store, other);
        assert!(
            store
                .publish_pending_question_event(
                    &event(session, 3, "ask-wrong-owner"),
                    Some(&wrong_source),
                    &question()
                )
                .unwrap_err()
                .to_string()
                .contains("belongs to another session")
        );
        unresolved(&store, session);
        assert_eq!(store.load_events_since(session, None).unwrap().len(), 1);
    }

    #[test]
    fn pending_question_publication_interrupted_reservation_survives_restart_without_inference() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("questions.db");
        let store = Store::open(&path).unwrap();
        let session = fixture(&store).id;
        store
            .publish_pending_question_event(&event(session, 1, "ask-old"), None, &question())
            .unwrap();
        store
            .reserve_pending_question_publication(
                session,
                Some(&serde_json::to_string(&question()).unwrap()),
            )
            .unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        unresolved(&store, session);
    }

    #[test]
    fn pending_question_publication_stale_producer_cannot_overwrite_newer_or_cleared_epoch() {
        let store = Store::open_in_memory().unwrap();
        let session = fixture(&store).id;
        let reserved = store
            .reserve_pending_question_publication(
                session,
                Some(&serde_json::to_string(&question()).unwrap()),
            )
            .unwrap();
        store
            .publish_pending_question_event(&event(session, 2, "ask-current"), None, &question())
            .unwrap();
        let current = store.pending_question_target(session).unwrap().unwrap();
        assert!(
            store
                .bind_pending_question_event(
                    &reserved,
                    &event(session, 1, "ask-old"),
                    None,
                    &question()
                )
                .is_err()
        );
        assert_eq!(
            store.pending_question_target(session).unwrap(),
            Some(current.clone())
        );
        store
            .clear_pending_question_exact(session, &current)
            .unwrap();
        assert!(
            store
                .bind_pending_question_event(
                    &reserved,
                    &event(session, 1, "ask-old"),
                    None,
                    &question()
                )
                .is_err()
        );
        assert_eq!(store.pending_question_target(session).unwrap(), None);
        assert_eq!(store.load_events_since(session, None).unwrap().len(), 1);
    }

    #[test]
    fn pending_question_publication_legacy_and_partial_remain_readable_unresolved_gates() {
        let store = Store::open_in_memory().unwrap();
        let session = fixture(&store).id;
        store
            .publish_pending_question_event(&event(session, 1, "ask-old"), None, &question())
            .unwrap();
        store
            .update_session_pending_question_json(
                session,
                Some(&serde_json::to_string(&question()).unwrap()),
            )
            .unwrap();
        unresolved(&store, session);
        store
            .insert_event(&event(session, 2, "ask-legacy"))
            .unwrap();
        unresolved(&store, session);
        let mut partial = event(session, 3, "ignored");
        partial.tool_use_id = None;
        store
            .publish_pending_question_event(&partial, None, &question())
            .unwrap();
        unresolved(&store, session);
        store
            .publish_pending_question_event(&event(session, 4, "ask-current"), None, &question())
            .unwrap();
        assert_eq!(
            store.pending_question_target(session).unwrap().unwrap()["tool_use_id"],
            "ask-current"
        );
        store
            .insert_event(&event(session, 5, "ask-without-publication"))
            .unwrap();
        unresolved(&store, session);
        store
            .update_session_pending_question_json(session, None)
            .unwrap();
        assert_eq!(store.pending_question_target(session).unwrap(), None);
    }

    #[test]
    fn pending_question_publication_has_no_synthetic_non_claude_support() {
        let store = Store::open_in_memory().unwrap();
        let mut session = crate::session::agent_verbs::tests::test_session(
            Uuid::new_v4(),
            "/var/tmp/question-publication".into(),
        );
        session.provider = SessionProvider::Codex;
        store.insert_session(&session).unwrap();
        assert!(
            store
                .publish_pending_question_event(
                    &event(session.id, 1, "ask-codex"),
                    None,
                    &question()
                )
                .is_err()
        );
        unresolved(&store, session.id);
        assert_eq!(store.load_events_since(session.id, None).unwrap().len(), 0);
    }

    #[test]
    fn pending_question_publication_current_survives_reopen_but_later_user_input_is_stale() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("questions.db");
        let store = Store::open(&path).unwrap();
        let session = fixture(&store).id;
        store
            .publish_pending_question_event(&event(session, 1, "ask-current"), None, &question())
            .unwrap();
        let current = store.pending_question_target(session).unwrap().unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store.pending_question_target(session).unwrap(),
            Some(current)
        );
        let mut reply = event(session, 2, "unused");
        reply.role = Some(Role::User);
        reply.event_type = EventType::Message;
        reply.tool_name = None;
        reply.tool_input = None;
        reply.tool_use_id = None;
        store.insert_event(&reply).unwrap();
        assert!(
            store
                .pending_question_target(session)
                .unwrap_err()
                .to_string()
                .contains("question_identity_stale")
        );
    }
    #[test]
    fn pending_question_publication_preserves_explicit_purge_of_legacy_session_data() {
        let store = Store::open_in_memory().unwrap();
        for published in [false, true] {
            let session = fixture(&store).id;
            if published {
                store
                    .publish_pending_question_event(
                        &event(session, 1, "ask-purge"),
                        None,
                        &question(),
                    )
                    .unwrap();
            } else {
                store
                    .update_session_pending_question_json(session, None)
                    .unwrap();
            }
            store.soft_delete_session(session).unwrap();
            store.purge_session(session).unwrap();
            assert!(store.get_session(session).unwrap().is_none());
            assert_eq!(
                store
                    .conn
                    .query_row(
                        "SELECT count(*) FROM pending_question_publications WHERE session_id=?1",
                        [session.to_string()],
                        |r| r.get::<_, i64>(0)
                    )
                    .unwrap(),
                0
            );
        }
    }
}
