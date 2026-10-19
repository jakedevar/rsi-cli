//! #2469: project-owned pending manager correspondence survives a seat or
//! scope replacement.
//!
//! Manager mail is bound to the `(manager_session_id, scope_version)` tuple it
//! was written under, and a replacement bumps that tuple. The old rows are
//! immutable and stay as the audit record; what the successor needs is a live
//! copy under the new tuple. Each copy is a fresh message row whose
//! idempotency key names its source (`adopt:<source id>` for replies and lead
//! notices, the established `readdress:<source id>:<lead>` for open requests,
//! so the existing `readdressed_from` surfaces light up), written only while
//! the Epic is still inside the live scope with a valid lead. Nothing here
//! grants authority: the policy grant is revoked by the same scope bump, and
//! operator answers and approvals are not carried.

use super::*;

/// Prefix of the idempotency key of an adopted reply or lead notice.
const ADOPTED_MESSAGE_KEY_PREFIX: &str = "adopt:";

impl Store {
    /// A live copy of one pending (never-retrieved) lead-to-manager message
    /// for the current seat. Returns whether a copy was written.
    ///
    /// Refused silently (`Ok(false)`) when the source is not a reply or lead
    /// notice of the named old tuple, its Epic left the live scope or has no
    /// valid lead, the lead is no longer its sender, the Epic is node-managed
    /// (node mail owns its own grants), or it was already adopted.
    pub(crate) fn adopt_pending_manager_message(
        &self,
        config: &HarnessManagerConfigV1,
        from_manager: Uuid,
        from_scope: i64,
        message_id: Uuid,
    ) -> Result<bool> {
        let Some(source) = self.get_manager_message(message_id)? else {
            return Ok(false);
        };
        let lead_origin = source.is_lead_notice();
        if source.project_id != config.project_id
            || source.manager_session_id != from_manager
            || source.scope_version != from_scope
            || (source.public.request_id.is_none() && !lead_origin)
            || (lead_origin && source.public.request_id.is_some())
        {
            return Ok(false);
        }
        let epic = source.public.epic_id;
        if !config.epic_ids.contains(&epic)
            || !self.manager_message_has_no_node_address(&source)?
            || !matches!(self.manager_mail_node_for_epic(config, epic), Ok(None))
        {
            return Ok(false);
        }
        let Ok(lead) = self.manager_lead(config.project_id, epic) else {
            return Ok(false);
        };
        let manager = self.manager_lineage_tip(config.manager_session_id)?;
        if manager == lead.id
            || self
                .manager_lineage_tip(source.public.sender_session_id)
                .ok()
                != Some(lead.id)
        {
            return Ok(false);
        }
        let key = format!("{ADOPTED_MESSAGE_KEY_PREFIX}{message_id}");
        let adopted: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM harness_manager_messages
             WHERE sender_session_id=?1 AND idempotency_key=?2)",
            params![source.public.sender_session_id.to_string(), key],
            |row| row.get(0),
        )?;
        if adopted {
            return Ok(false);
        }
        let id = Uuid::new_v4();
        // The text, sender, correlation and original timestamp are copied from
        // the immutable source row, never re-derived.
        self.conn.execute(
            "INSERT INTO harness_manager_messages(id,project_id,manager_session_id,epic_id,
                scope_version,sender_session_id,recipient_session_id,request_id,idempotency_key,
                request_fingerprint,message,created_at)
             SELECT ?2,project_id,?3,epic_id,?4,sender_session_id,?5,request_id,?6,
                request_fingerprint,message,created_at
             FROM harness_manager_messages WHERE id=?1",
            params![
                message_id.to_string(),
                id.to_string(),
                config.manager_session_id.to_string(),
                config.row_version,
                manager.to_string(),
                key
            ],
        )?;
        let receipt = HarnessManagerMessageReceiptV1 {
            message_id: id,
            sequence: self.conn.last_insert_rowid(),
            request_id: source.public.request_id,
            deduplicated: false,
            rolled_over_from: None,
            manager_seat: None,
        };
        self.ensure_manager_watch(config, &lead, true, &receipt.sequence.to_string(), true)?;
        self.record_manager_message_notice(config, &lead, &receipt, true)?;
        Ok(true)
    }

    /// Re-issue the open requests of the replaced `previous` tuple to their
    /// current leads under the live `config` tuple, through the same
    /// `request_readdress` link a lead replacement uses. Only the newest
    /// [`MAX_READDRESSED_REQUESTS`] of an Epic move; node-addressed mail and
    /// requests whose lead changed are left to their own settlement rules. The
    /// older overflow is settled `superseded_on_replace` on the retired tuple.
    /// Returns the Epic-lead transports that need a post-commit wake.
    pub(crate) fn adopt_open_manager_requests(
        &self,
        previous: &HarnessManagerConfigV1,
        config: &HarnessManagerConfigV1,
    ) -> Result<Vec<Uuid>> {
        if config.current_session_id.is_none()
            || config.is_revoked()
            || (previous.manager_session_id == config.manager_session_id
                && previous.row_version == config.row_version)
        {
            return Ok(Vec::new());
        }
        let manager = self.manager_lineage_tip(config.manager_session_id)?;
        let query = format!(
            "SELECT {MESSAGE_COLUMNS} FROM harness_manager_messages m
             WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3 AND {}
             ORDER BY sequence DESC LIMIT ?4",
            manager_request_open_sql()
        );
        let mut statement = self.conn.prepare(&query)?;
        let mut open = statement
            .query_map(
                params![
                    previous.project_id.to_string(),
                    previous.manager_session_id.to_string(),
                    previous.row_version,
                    ORPHAN_SWEEP_LIMIT
                ],
                read_message,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        // Newest first above; re-issue oldest first so the lead reads them in
        // the order they were asked.
        open.reverse();
        let mut per_epic: std::collections::HashMap<Uuid, usize> = std::collections::HashMap::new();
        for request in &open {
            *per_epic.entry(request.public.epic_id).or_default() += 1;
        }
        let mut jobs = Vec::new();
        for original in open {
            let epic = original.public.epic_id;
            let remaining = per_epic.entry(epic).or_default();
            // The oldest overflow beyond the readdress limit does not move.
            let skip = *remaining > MAX_READDRESSED_REQUESTS;
            *remaining -= 1;
            let id = original.public.message_id;
            if !config.epic_ids.contains(&epic)
                || !self.manager_message_has_no_node_address(&original)?
                || !matches!(self.manager_mail_node_for_epic(config, epic), Ok(None))
                || self.manager_request_readdressed(config, id)?.is_some()
            {
                continue;
            }
            let Ok(lead) = self.manager_lead(config.project_id, epic) else {
                continue;
            };
            if manager == lead.id
                || self
                    .manager_lineage_tip(original.public.recipient_session_id)
                    .ok()
                    != Some(lead.id)
            {
                continue;
            }
            if skip {
                // #2486: settle it on the retired tuple, as a lead replacement
                // does (#973), so it never stays open and hidden from the
                // successor.
                self.guarded_adoption(|| {
                    self.manager_v2_put_record(
                        previous,
                        REQUEST_SETTLE_KIND,
                        &id.to_string(),
                        Some(epic),
                        0,
                        &serde_json::json!({
                            "disposition": "superseded_on_replace",
                            "actor": "daemon",
                            "previous_manager": previous.manager_session_id,
                            "previous_scope_version": previous.row_version,
                            "current_manager": config.manager_session_id,
                            "current_scope_version": config.row_version,
                            "readdress_limit": MAX_READDRESSED_REQUESTS,
                        }),
                    )
                })?;
                continue;
            }
            let adopted = self.guarded_adoption(|| {
                let key = format!("readdress:{id}:{}", lead.id);
                let exists: bool = self.conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM harness_manager_messages
                     WHERE sender_session_id=?1 AND idempotency_key=?2)",
                    params![manager.to_string(), key],
                    |row| row.get(0),
                )?;
                if exists {
                    return Ok(None);
                }
                let digest = fingerprint(&AgentManagerSendRequestV1 {
                    epic_id: epic,
                    message: original.public.message.clone(),
                    idempotency_key: key.clone(),
                    informational: false,
                })?;
                let receipt = self.insert_manager_message(
                    config,
                    manager,
                    lead.id,
                    epic,
                    None,
                    &original.public.message,
                    &key,
                    &digest,
                )?;
                self.manager_v2_put_record(
                    config,
                    "request_readdress",
                    &id.to_string(),
                    Some(epic),
                    0,
                    &serde_json::json!({"new_request_id":receipt.message_id}),
                )?;
                self.ensure_manager_watch(
                    config,
                    &lead,
                    false,
                    &receipt.sequence.to_string(),
                    true,
                )?;
                self.record_manager_message_notice(config, &lead, &receipt, false)
            })?;
            if let Some(Some(job)) = adopted {
                jobs.push(job);
            }
        }
        Ok(jobs)
    }

    /// Run one adoption step in its own savepoint. A typed refusal (a full
    /// transport budget, a node grant that moved) rolls only that step back
    /// and skips it, so one unadoptable item never blocks the replacement or
    /// the retirement of its page; a storage error still propagates.
    pub(crate) fn guarded_adoption<T>(
        &self,
        step: impl FnOnce() -> Result<T>,
    ) -> Result<Option<T>> {
        self.conn.execute_batch("SAVEPOINT manager_seat_adoption")?;
        match step() {
            Ok(value) => {
                self.conn.execute_batch("RELEASE manager_seat_adoption")?;
                Ok(Some(value))
            }
            Err(DaemonError::InvalidParam(code)) => {
                self.conn.execute_batch(
                    "ROLLBACK TO manager_seat_adoption; RELEASE manager_seat_adoption",
                )?;
                tracing::warn!(code, "manager mail adoption step skipped");
                Ok(None)
            }
            Err(error) => {
                self.conn.execute_batch(
                    "ROLLBACK TO manager_seat_adoption; RELEASE manager_seat_adoption",
                )?;
                Err(error)
            }
        }
    }

    /// True when the message was not addressed to a manager node (#888), whose
    /// grant-scoped mail this seat-only adoption must not re-home.
    fn manager_message_has_no_node_address(&self, record: &StoredMessage) -> Result<bool> {
        Ok(!self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM harness_manager_v2_events a
             WHERE a.project_id=?1 AND a.manager_session_id=?2 AND a.scope_version=?3
               AND a.kind=?4 AND a.record_key=?5)",
            params![
                record.project_id.to_string(),
                record.manager_session_id.to_string(),
                record.scope_version,
                NODE_MAIL_ADDRESS_KIND,
                record.public.message_id.to_string()
            ],
            |row| row.get::<_, bool>(0),
        )?)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::too_many_lines)]

    use super::*;
    use crate::test_support::test_session;
    use rsi_common::harness_manager_v2::{ConfigureHarnessManagerPolicyRequestV2, ManagerPolicyV2};
    use rsi_common::types::Project;
    use std::path::PathBuf;

    struct Fixture {
        store: Store,
        project: Uuid,
        manager: Uuid,
        successor: Uuid,
        epics: [Uuid; 2],
        leads: [Uuid; 2],
        config: HarnessManagerConfigV1,
    }

    /// Project-wide manager `manager` with a granted policy, two Epics with
    /// current leads, and an idle second session `successor` that can take
    /// the seat.
    fn fixture() -> Fixture {
        let store = Store::open_in_memory().unwrap();
        let project = Uuid::new_v4();
        let now = Utc::now();
        store
            .insert_project(&Project {
                id: project,
                name: "Seat mail".into(),
                path: None,
                description: None,
                color: Project::DEFAULT_COLOR.into(),
                context_files: None,
                created_at: now,
                updated_at: now,
            })
            .unwrap();
        let mut manager = test_session(Uuid::new_v4(), PathBuf::from("/tmp/seat-mail"));
        manager.session_kind = SessionKind::Standard;
        manager.project_id = Some(project);
        manager.status = SessionStatus::Running;
        store.insert_session(&manager).unwrap();
        let mut successor = manager.clone();
        successor.id = Uuid::new_v4();
        store.insert_session(&successor).unwrap();
        let mut group = manager.clone();
        group.id = Uuid::new_v4();
        group.session_kind = SessionKind::Group;
        group.status = SessionStatus::Completed;
        store.insert_session(&group).unwrap();
        let epics = [Uuid::new_v4(), Uuid::new_v4()];
        let leads = [Uuid::new_v4(), Uuid::new_v4()];
        for index in 0..2 {
            let mut epic = group.clone();
            epic.id = epics[index];
            epic.session_kind = SessionKind::Epic;
            epic.parent_id = Some(group.id);
            store.insert_session(&epic).unwrap();
            let mut lead = manager.clone();
            lead.id = leads[index];
            lead.session_kind = SessionKind::Feature;
            lead.parent_id = Some(epic.id);
            store.insert_session(&lead).unwrap();
            store.set_lead_session(epic.id, Some(lead.id)).unwrap();
        }
        let config = store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                group_ids: Vec::new(),
                project_id: project,
                session_id: manager.id,
                epic_ids: None,
                expected_row_version: 0,
            })
            .unwrap();
        store
            .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
                project_id: project,
                expected_scope_version: config.row_version,
                expected_policy_version: 0,
                idempotency_key: "grant".into(),
                policy: ManagerPolicyV2::default(),
            })
            .unwrap();
        Fixture {
            store,
            project,
            manager: manager.id,
            successor: successor.id,
            epics,
            leads,
            config,
        }
    }

    fn send(f: &Fixture, epic: Uuid, key: &str) -> HarnessManagerMessageReceiptV1 {
        f.store
            .manager_send(
                f.manager,
                &AgentManagerSendRequestV1 {
                    epic_id: epic,
                    message: format!("Request {key}"),
                    idempotency_key: key.into(),
                    informational: false,
                },
            )
            .unwrap()
    }

    fn inbox(store: &Store, caller: Uuid) -> AgentManagerInboxResultV1 {
        store
            .manager_inbox(caller, &AgentManagerInboxRequestV1::default())
            .unwrap()
    }

    /// Pending correspondence for the seat holder `f.manager`: an open
    /// request to Epic 0, a reply and a lead notice from Epic 1 (a request of
    /// Epic 1 answered), and a project-wide provider-auth notice.
    struct Pending {
        open_request: Uuid,
        reply_text: &'static str,
        notice_text: &'static str,
    }

    fn record_pending(f: &Fixture) -> Pending {
        let open = send(f, f.epics[0], "open-0");
        let answered = send(f, f.epics[1], "answered-1");
        f.store
            .manager_reply(
                f.leads[1],
                &AgentManagerReplyRequestV1 {
                    request_id: answered.message_id,
                    message: "Epic one reply".into(),
                    idempotency_key: "reply-1".into(),
                    still_running: false,
                },
            )
            .unwrap();
        f.store
            .manager_lead_notice(f.leads[1], "Epic one lead notice", "notice-1")
            .unwrap();
        let at = Utc::now();
        f.store
            .record_provider_auth_invalid_notice(f.project, "claude", at, at)
            .unwrap()
            .expect("provider auth notice recorded");
        f.store
            .record_qa_issue_notice(
                f.project,
                &crate::store::manager_notices::QaIssueNotice {
                    issue_number: 2736,
                    title: "Full library QA test red after landing a0a9a7e6c".into(),
                    outcome: "test_red",
                    rolling_sha: "a".repeat(40),
                    failing_tests: vec!["session::launch::tests::red".into()],
                },
            )
            .unwrap()
            .expect("qa Issue notice recorded");
        Pending {
            open_request: open.message_id,
            reply_text: "Epic one reply",
            notice_text: "Epic one lead notice",
        }
    }

    fn replace(f: &Fixture, epic_ids: Option<Vec<Uuid>>) -> HarnessManagerConfigV1 {
        f.store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                group_ids: Vec::new(),
                project_id: f.project,
                session_id: f.successor,
                epic_ids,
                expected_row_version: f.config.row_version,
            })
            .unwrap()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn seat_replacement_carries_pending_mail_and_notices_to_the_successor_once() {
        let f = fixture();
        let pending = record_pending(&f);
        // The old seat never read any of it.
        let replaced = replace(&f, None);
        assert_eq!(replaced.manager_session_id, f.successor);
        assert!(replaced.row_version > f.config.row_version);

        let read = inbox(&f.store, f.successor);
        let texts: Vec<_> = read.messages.iter().map(|m| m.message.as_str()).collect();
        assert_eq!(
            texts
                .iter()
                .filter(|text| **text == pending.reply_text)
                .count(),
            1
        );
        assert_eq!(
            texts
                .iter()
                .filter(|text| **text == pending.notice_text)
                .count(),
            1
        );
        let reply = read
            .messages
            .iter()
            .find(|m| m.message == pending.reply_text)
            .unwrap();
        assert_eq!(reply.sender_session_id, f.leads[1]);
        assert_eq!(reply.recipient_session_id, f.successor);
        assert!(reply.request_id.is_some());
        let notice = read
            .messages
            .iter()
            .find(|m| m.message == pending.notice_text)
            .unwrap();
        assert_eq!(notice.request_id, None);
        assert_eq!(notice.epic_id, f.epics[1]);

        // Mail notices for both and the project-wide notice, each once.
        let mail_notices = read
            .notices
            .iter()
            .filter(|n| n.kind == "message" && n.direction == "to_manager")
            .count();
        assert_eq!(mail_notices, 2);
        let auth: Vec<_> = read
            .notices
            .iter()
            .filter(|n| {
                n.kind == "ledger_change" && n.subject_id.starts_with("provider_auth_invalid:")
            })
            .collect();
        assert_eq!(auth.len(), 1);
        assert_eq!(
            auth[0].state["carried_over"]["from_manager_session_id"],
            f.manager.to_string()
        );
        // #2743: an open qa red outlives the seat that was told of it.
        let qa: Vec<_> = read
            .notices
            .iter()
            .filter(|n| n.kind == "ledger_change" && n.subject_id == "qa_issue:2736")
            .collect();
        assert_eq!(qa.len(), 1, "{:?}", read.notices);
        assert_eq!(qa[0].state["rolling_sha"], "a".repeat(40));
        assert_eq!(
            qa[0].state["carried_over"]["from_manager_session_id"],
            f.manager.to_string()
        );
        assert!(read.notices.iter().all(|n| n.kind != "operator_answer"));

        // Settled once: the next read holds nothing new.
        let again = f
            .store
            .manager_inbox(
                f.successor,
                &AgentManagerInboxRequestV1 {
                    after_sequence: read.messages.last().unwrap().sequence,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(again.messages.is_empty());
        assert!(again.notices.is_empty());

        // The open request reaches its lead, linked to the immutable source.
        let lead = inbox(&f.store, f.leads[0]);
        let moved = lead
            .messages
            .iter()
            .find(|m| m.readdressed_from == Some(pending.open_request))
            .unwrap();
        assert_eq!(moved.sender_session_id, f.successor);
        assert_eq!(moved.message, "Request open-0");
        // Replacing the seat grants nothing: the policy stays revoked until
        // re-granted, and the old rows are untouched.
        assert!(
            f.store
                .get_harness_manager_policy(f.project)
                .unwrap()
                .unwrap()
                .revoked
        );
        let old_rows: i64 = f
            .store
            .conn
            .query_row(
                "SELECT count(*) FROM harness_manager_messages WHERE manager_session_id=?1
                 AND scope_version=?2",
                params![f.manager.to_string(), f.config.row_version],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(old_rows, 4);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn narrowed_scope_replacement_exposes_no_foreign_mail() {
        let f = fixture();
        record_pending(&f);
        let replaced = replace(&f, Some(vec![f.epics[0]]));
        assert_eq!(replaced.epic_ids, vec![f.epics[0]]);

        let read = inbox(&f.store, f.successor);
        // Everything pending lived on Epic 1 (outside the narrowed scope) or is
        // project-wide: no Epic 1 mail or notice is shown.
        assert!(read.messages.iter().all(|m| m.epic_id == f.epics[0]));
        assert!(read.notices.iter().all(|n| n.epic_id != Some(f.epics[1])));
        assert!(
            read.notices
                .iter()
                .all(|n| !(n.kind == "message" && n.direction == "to_manager"))
        );
        // Project-wide operator-relevant notices still follow the project.
        assert_eq!(
            read.notices
                .iter()
                .filter(|n| n.subject_id.starts_with("provider_auth_invalid:"))
                .count(),
            1
        );
        // The Epic 0 request is carried; the Epic 1 lead sees nothing new.
        assert!(
            inbox(&f.store, f.leads[0])
                .messages
                .iter()
                .any(|m| m.readdressed_from.is_some())
        );
        // Epic 1 left the scope: its lead is denied the manager mail surface.
        assert!(
            f.store
                .manager_inbox(f.leads[1], &AgentManagerInboxRequestV1::default())
                .is_err()
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn revoking_the_seat_carries_nothing() {
        let f = fixture();
        record_pending(&f);
        let revoked = f
            .store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                group_ids: Vec::new(),
                project_id: f.project,
                session_id: f.manager,
                epic_ids: Some(Vec::new()),
                expected_row_version: f.config.row_version,
            })
            .unwrap();
        assert!(revoked.is_revoked());
        let carried: i64 = f
            .store
            .conn
            .query_row(
                "SELECT count(*) FROM harness_manager_messages WHERE idempotency_key GLOB 'adopt:*'
                 OR idempotency_key GLOB 'readdress:*'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(carried, 0);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn seat_replacement_settles_open_requests_beyond_the_readdress_limit() {
        let f = fixture();
        let stale = MAX_READDRESSED_REQUESTS + 4;
        let sent: Vec<Uuid> = (0..stale)
            .map(|n| send(&f, f.epics[0], &format!("overflow-{n}")).message_id)
            .collect();
        replace(&f, None);

        let (old, newest) = sent.split_at(4);
        let lead = inbox(&f.store, f.leads[0]);
        let moved: Vec<Uuid> = lead
            .messages
            .iter()
            .filter_map(|m| m.readdressed_from)
            .collect();
        assert_eq!(moved.len(), MAX_READDRESSED_REQUESTS);
        for id in newest {
            assert!(moved.contains(id));
        }
        for id in old {
            let (disposition, scope): (String, i64) = f
                .store
                .conn
                .query_row(
                    "SELECT json_extract(payload_json,'$.disposition'), scope_version
                     FROM harness_manager_v2_records
                     WHERE kind='request_settle' AND record_key=?1",
                    [id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!(disposition, "superseded_on_replace");
            assert_eq!(scope, f.config.row_version);
        }
        // None stays open-and-unsettled on the retired tuple except the moved
        // ones, which the successor now owns through the live copies.
        let open: i64 = f
            .store
            .conn
            .query_row(
                &format!(
                    "SELECT count(*) FROM harness_manager_messages m
                     WHERE m.manager_session_id=?1 AND m.scope_version=?2 AND {}
                       AND m.id IN (SELECT value FROM json_each(?3))",
                    manager_request_open_sql()
                ),
                params![
                    f.manager.to_string(),
                    f.config.row_version,
                    serde_json::to_string(&old.iter().map(Uuid::to_string).collect::<Vec<_>>())
                        .unwrap()
                ],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(open, 0);
    }
}
