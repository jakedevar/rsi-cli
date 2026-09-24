//! K14 (#672): store half of the delegated `operator_call` manager action.
//!
//! Reach is the grant's project only. The logical `ArchiveSession` shares one
//! IMMEDIATE transaction between the effect-time retention re-check and the
//! guarded status UPDATE; it never runs archive cleanup and never deletes rows.
use super::{ManagerActionClaimV2, Store};
use crate::error::{DaemonError, Result};
use crate::store::harness_manager_v2::{ManagerAuthorityV2, refused};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use rsi_common::archive_cleanup::ArchiveSessionResultV1;
use rsi_common::harness_manager_v2::{
    ManagerActionStateV2, ManagerActionV2, ManagerOperatingModeV2,
};
use rsi_common::manager_operator_delegation::{
    DELEGATED_PAGE_MAX_BYTES, DELEGATED_PAGE_MAX_ROWS, DelegatedListSessionsParamsV1,
    DelegatedOperatorCallV1, DelegatedSessionCursorV1, DelegatedSessionRowV1, OperatorCallFenceV1,
    OperatorCallResultV1, OperatorCallV1,
};
use rsi_common::types::{SandboxCleanupState, SandboxKind, Session, SessionStatus};
use rusqlite::{Transaction, TransactionBehavior, params};
use uuid::Uuid;

/// Sessions active this recently are retained by the delegated archive.
const RETENTION_ACTIVITY_WINDOW_HOURS: i64 = 24;

/// The session a delegated EFFECT targets (for Epic pause/decision gates).
pub(super) fn delegated_effect_session(action: &ManagerActionV2) -> Option<Uuid> {
    match action {
        ManagerActionV2::OperatorCall { call, .. } => call
            .typed()
            .ok()
            .filter(DelegatedOperatorCallV1::is_effect)
            .and_then(|call| call.session_id()),
        _ => None,
    }
}

fn typed_call(claim: &ManagerActionClaimV2) -> Result<DelegatedOperatorCallV1> {
    match claim.action() {
        ManagerActionV2::OperatorCall { call, .. } => call.typed().map_err(refused),
        _ => Err(refused("manager_v2_not_operator_call")),
    }
}

fn stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

impl Store {
    /// One leaf of the grant's project. A missing or foreign row is the same
    /// refusal, so reach never becomes a cross-project existence oracle.
    fn manager_action_project_target(
        &self,
        authority: &ManagerAuthorityV2,
        id: Uuid,
    ) -> Result<Session> {
        let session = self
            .get_session(id)?
            .filter(|s| s.project_id == Some(authority.config.project_id))
            .ok_or_else(|| refused("manager_v2_target_out_of_project"))?;
        if !rsi_common::is_leaf_kind(session.session_kind) {
            return Err(refused("manager_v2_leaf_required"));
        }
        Ok(session)
    }

    /// Admission (`check_version`) and effect-time target for `operator_call`.
    pub(super) fn manager_action_operator_target(
        &self,
        authority: &ManagerAuthorityV2,
        call: &OperatorCallV1,
        expected: Option<&OperatorCallFenceV1>,
        check_version: bool,
    ) -> Result<Option<Session>> {
        let typed = call.typed().map_err(refused)?;
        if check_version {
            let policy = &authority.grant.policy;
            if policy.mode != ManagerOperatingModeV2::Execute {
                return Err(refused("manager_v2_execute_required"));
            }
            if policy.paused {
                return Err(refused("manager_v2_policy_paused"));
            }
        }
        match typed {
            DelegatedOperatorCallV1::ArchiveSession(params) => {
                let session = self.manager_action_project_target(authority, params.session_id)?;
                if check_version {
                    self.delegated_effect_fence(authority, &session, expected)?;
                    if let Some(code) = self.delegated_archive_blocker(&session, Utc::now())? {
                        return Err(refused(&code));
                    }
                }
                Ok(Some(session))
            }
            DelegatedOperatorCallV1::UnarchiveSession(params) => {
                let session = self.manager_action_project_target(authority, params.session_id)?;
                if check_version {
                    self.delegated_effect_fence(authority, &session, expected)?;
                    // Same gates as the housekeeping `RestoreSession`: only an
                    // Archived row, never one whose sandbox history forbids a
                    // restore (purged worktree or source settlement).
                    if session.status != SessionStatus::Archived {
                        return Err(refused("manager_v2_session_state_changed"));
                    }
                    if super::historical_session_restore_blocked_on(&self.conn, session.id)? {
                        return Err(refused("manager_v2_historical_restore_refused"));
                    }
                }
                Ok(Some(session))
            }
            DelegatedOperatorCallV1::GetArchiveCleanupStatus(params) => {
                self.manager_action_project_target(authority, params.session_id)?;
                Ok(None)
            }
            DelegatedOperatorCallV1::ListSessions(_) => Ok(None),
        }
    }

    /// Epic pause and the required `session_updated_at` fence shared by every
    /// delegated effect, checked at admission and again at effect time.
    fn delegated_effect_fence(
        &self,
        authority: &ManagerAuthorityV2,
        session: &Session,
        expected: Option<&OperatorCallFenceV1>,
    ) -> Result<()> {
        if let Some(epic) = self.manager_action_session_epic(session.id)?
            && authority.grant.policy.paused_epic_ids.contains(&epic)
        {
            return Err(refused("manager_v2_policy_paused"));
        }
        let fence = expected.ok_or_else(|| refused("manager_v2_operator_fence_required"))?;
        if session.updated_at != fence.session_updated_at {
            return Err(refused("manager_v2_session_changed"));
        }
        Ok(())
    }

    /// Why a delegated logical archive of `session` is refused now (§5.2):
    /// the housekeeping gates first, then the decided retention exclusions.
    pub(super) fn delegated_archive_blocker(
        &self,
        session: &Session,
        now: DateTime<Utc>,
    ) -> Result<Option<String>> {
        if !rsi_common::is_leaf_kind(session.session_kind) {
            return Ok(Some("manager_v2_leaf_required".into()));
        }
        match self.manager_action_validate_archive_session(session) {
            Ok(()) => {}
            Err(DaemonError::InvalidParam(code)) => return Ok(Some(code)),
            Err(error) => return Err(error),
        }
        if session.pinned_at.is_some() {
            return Ok(Some("manager_v2_retention_pinned".into()));
        }
        let id = session.id.to_string();
        let wake: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM scheduled_jobs
              WHERE enabled=1 AND (wake_session_id=?1 OR wake_mode=?2))",
            params![id, format!("on_terminal:{id}")],
            |row| row.get(0),
        )?;
        if wake {
            return Ok(Some("manager_v2_retention_enabled_wake".into()));
        }
        let review: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM manager_review_assignments
              WHERE state IN ('reserved','allocating','active')
                AND (author_session_id=?1 OR reviewer_session_id=?1))",
            [&id],
            |row| row.get(0),
        )?;
        if review {
            return Ok(Some("manager_v2_retention_live_review".into()));
        }
        if let Some(project) = session.project_id
            && self.manager_v2_session_is_sealed_source(project, session.id)?
        {
            return Ok(Some("manager_v2_retention_sealed_source".into()));
        }
        if self
            .delegated_last_activity(session)?
            .is_none_or(|at| at > now - Duration::hours(RETENTION_ACTIVITY_WINDOW_HOURS))
        {
            return Ok(Some("manager_v2_retention_recent_activity".into()));
        }
        // Step 1 skips live worktrees so they stay reclaimable by the
        // unchanged cleanup proof service (K14c).
        if session.sandbox_kind == Some(SandboxKind::GitWorktree)
            && session.sandbox_cleanup_state == Some(SandboxCleanupState::Live)
        {
            return Ok(Some("manager_v2_retention_live_worktree".into()));
        }
        Ok(None)
    }

    /// Exact newest instant of the session's activity: its `updated_at` and
    /// EVERY conversation-event `created_at`, each parsed with chrono's strict
    /// RFC3339 parser and compared as instants. No SQL time function, ordering
    /// or text comparison takes part: `sequence` does not order `created_at`,
    /// the `julianday()` SQL function accepts shapes chrono rejects (no offset) and
    /// rounds sub-millisecond instants, and stored offsets differ.
    ///
    /// Streams one indexed column (`idx_events_session_id`), bounded by the
    /// session's event count (observed maximum 11,377, mean 163). `None` =
    /// some event time is not a text RFC3339 instant; callers treat that as
    /// recent activity (fail closed). Admission, the effect-time re-check and
    /// `ListSessions` all use this one function.
    fn delegated_last_activity(&self, session: &Session) -> Result<Option<DateTime<Utc>>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT created_at FROM conversation_events WHERE session_id=?1")?;
        let mut rows = stmt.query([session.id.to_string()])?;
        let mut latest = session.updated_at;
        while let Some(row) = rows.next()? {
            let rusqlite::types::ValueRef::Text(raw) = row.get_ref(0)? else {
                return Ok(None);
            };
            let Some(at) = std::str::from_utf8(raw)
                .ok()
                .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
            else {
                return Ok(None);
            };
            latest = latest.max(at.with_timezone(&Utc));
        }
        Ok(Some(latest))
    }

    /// Effect: runtime gate (grant, fence and retention re-check) and the
    /// guarded logical UPDATE in ONE transaction, then the bounded receipt.
    pub(crate) fn apply_delegated_archive(&self, claim: &ManagerActionClaimV2) -> Result<Uuid> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        self.manager_action_runtime_gate_on(claim)?;
        let DelegatedOperatorCallV1::ArchiveSession(params) = typed_call(claim)? else {
            return Err(refused("manager_v2_not_operator_call"));
        };
        let changed = self.conn.execute(
            "UPDATE sessions SET status='Archived',pending_archive=0,updated_at=?2
             WHERE id=?1 AND status IN ('Completed','Failed','Interrupted')",
            params![params.session_id.to_string(), stamp(Utc::now())],
        )?;
        if changed != 1 {
            return Err(refused("manager_v2_session_state_changed"));
        }
        let result = OperatorCallResultV1::Scalar {
            method: "ArchiveSession".into(),
            result: serde_json::to_value(ArchiveSessionResultV1::no_cleanup_required())?,
        };
        self.finish_manager_action_result_on(
            claim,
            ManagerActionStateV2::Succeeded,
            "session_archived",
            Some(result),
        )?;
        tx.commit()?;
        Ok(params.session_id)
    }

    /// K14b effect: the housekeeping `RestoreSession` UPDATE (Archived to
    /// Completed, exactly like the operator and housekeeping restores; no prior
    /// status is stored), after the runtime gate re-runs the fence and the
    /// restore gates in the SAME transaction. No worktree is recreated.
    pub(crate) fn apply_delegated_unarchive(&self, claim: &ManagerActionClaimV2) -> Result<Uuid> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        self.manager_action_runtime_gate_on(claim)?;
        let DelegatedOperatorCallV1::UnarchiveSession(params) = typed_call(claim)? else {
            return Err(refused("manager_v2_not_operator_call"));
        };
        let changed = self.conn.execute(
            "UPDATE sessions SET status='Completed',pending_archive=0,updated_at=?2
             WHERE id=?1 AND status='Archived'",
            params![params.session_id.to_string(), stamp(Utc::now())],
        )?;
        if changed != 1 {
            return Err(refused("manager_v2_session_state_changed"));
        }
        let result = OperatorCallResultV1::Scalar {
            method: "UnarchiveSession".into(),
            result: serde_json::json!({
                "session_id": params.session_id,
                "status": SessionStatus::Completed,
            }),
        };
        self.finish_manager_action_result_on(
            claim,
            ManagerActionStateV2::Succeeded,
            "session_restored",
            Some(result),
        )?;
        tx.commit()?;
        Ok(params.session_id)
    }

    /// Publish a delegated read result after re-running the runtime gate, so a
    /// grant revoked while the read ran never receives the result.
    pub(crate) fn finish_delegated_read(
        &self,
        claim: &ManagerActionClaimV2,
        result: OperatorCallResultV1,
    ) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        self.manager_action_runtime_gate_on(claim)?;
        self.finish_manager_action_result_on(
            claim,
            ManagerActionStateV2::Succeeded,
            "operator_call_succeeded",
            Some(result),
        )?;
        tx.commit()?;
        Ok(())
    }

    /// `ListSessions`: gate, project-bound page and receipt in one transaction.
    pub(crate) fn execute_delegated_list_sessions(
        &self,
        claim: &ManagerActionClaimV2,
    ) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let authority = self.manager_action_runtime_gate_on(claim)?;
        let DelegatedOperatorCallV1::ListSessions(params) = typed_call(claim)? else {
            return Err(refused("manager_v2_not_operator_call"));
        };
        let page =
            self.delegated_list_sessions(authority.config.project_id, &params, Utc::now())?;
        self.finish_manager_action_result_on(
            claim,
            ManagerActionStateV2::Succeeded,
            "operator_call_succeeded",
            Some(page),
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Keyset page over the raw stored `(updated_at, id)` key, bounded to
    /// `DELEGATED_PAGE_MAX_ROWS` rows AND `DELEGATED_PAGE_MAX_BYTES` bytes of
    /// serialized envelope. The cursor always names the last emitted row.
    pub(crate) fn delegated_list_sessions(
        &self,
        project_id: Uuid,
        params: &DelegatedListSessionsParamsV1,
        now: DateTime<Utc>,
    ) -> Result<OperatorCallResultV1> {
        params.validate().map_err(refused)?;
        let limit = params.limit.unwrap_or(DELEGATED_PAGE_MAX_ROWS);
        let statuses = if params.status_in.is_empty() {
            serde_json::to_string(&[
                SessionStatus::Starting,
                SessionStatus::Running,
                SessionStatus::WaitingApproval,
                SessionStatus::Completed,
                SessionStatus::Failed,
                SessionStatus::Interrupted,
                SessionStatus::Archived,
            ])?
        } else {
            serde_json::to_string(&params.status_in)?
        };
        let mut stmt = self.conn.prepare(
            "SELECT id,updated_at FROM sessions
              WHERE project_id=?1 AND status IN (SELECT value FROM json_each(?2))
                AND (?3 IS NULL OR julianday(updated_at) < julianday(?3))
                AND (?4 IS NULL OR updated_at > ?4 OR (updated_at = ?4 AND id > ?5))
              ORDER BY updated_at, id LIMIT ?6",
        )?;
        let candidates = stmt
            .query_map(
                params![
                    project_id.to_string(),
                    statuses,
                    params.terminal_before.map(stamp),
                    params.after.as_ref().map(|c| c.updated_at.clone()),
                    params.after.as_ref().map(|c| c.id.to_string()),
                    i64::from(limit) + 1,
                ],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let more = candidates.len() > usize::from(limit);
        let mut rows: Vec<DelegatedSessionRowV1> = Vec::new();
        let mut next_after = None;
        for (raw_id, raw_updated_at) in candidates.into_iter().take(usize::from(limit)) {
            let id = Uuid::parse_str(&raw_id).map_err(|_| refused("manager_v2_session_invalid"))?;
            let Some(session) = self.get_session(id)? else {
                continue;
            };
            rows.push(DelegatedSessionRowV1 {
                id,
                parent_id: session.parent_id,
                kind: session.session_kind,
                status: session.status,
                updated_at: raw_updated_at.clone(),
                // An unparseable event time is reported as the row's own
                // `updated_at`; its `archive_blocker` still refuses (fail closed).
                last_activity_at: stamp(
                    self.delegated_last_activity(&session)?
                        .unwrap_or(session.updated_at),
                ),
                pinned: session.pinned_at.is_some(),
                sandbox_cleanup_state: session.sandbox_cleanup_state,
                archive_blocker: self.delegated_archive_blocker(&session, now)?,
            });
            if page_bytes(&rows)? > DELEGATED_PAGE_MAX_BYTES {
                rows.pop();
                next_after = rows.last().map(cursor);
                break;
            }
        }
        if next_after.is_none() && more {
            next_after = rows.last().map(cursor);
        }
        if rows.is_empty() && next_after.is_none() && more {
            return Err(refused("manager_v2_operator_result_too_large"));
        }
        let row_count =
            u16::try_from(rows.len()).map_err(|_| refused("manager_v2_operator_result_invalid"))?;
        let page = OperatorCallResultV1::Page {
            method: "ListSessions".into(),
            rows,
            next_after,
            row_count,
        };
        page.validate().map_err(refused)?;
        Ok(page)
    }
}

fn cursor(row: &DelegatedSessionRowV1) -> DelegatedSessionCursorV1 {
    DelegatedSessionCursorV1 {
        updated_at: row.updated_at.clone(),
        id: row.id,
    }
}

/// Serialized size of the page envelope the rows would produce, with a
/// worst-case cursor so adding the cursor can never cross the bound.
fn page_bytes(rows: &[DelegatedSessionRowV1]) -> Result<usize> {
    let probe = OperatorCallResultV1::Page {
        method: "ListSessions".into(),
        rows: rows.to_vec(),
        next_after: Some(DelegatedSessionCursorV1 {
            updated_at: "x".repeat(40),
            id: Uuid::nil(),
        }),
        row_count: u16::MAX,
    };
    Ok(serde_json::to_vec(&probe)?.len())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// A terminal leaf last updated two days before `now`, with `events`
    /// stored verbatim as `(sequence, created_at)`.
    fn blocker_with_events(now: DateTime<Utc>, events: &[(i64, &str)]) -> Option<String> {
        let store = Store::open_in_memory().unwrap();
        let mut session = crate::session::agent_verbs::tests::test_session(
            Uuid::new_v4(),
            std::path::PathBuf::from("/tmp/k14-retention"),
        );
        session.status = SessionStatus::Completed;
        session.updated_at = now - Duration::hours(48);
        store.insert_session(&session).unwrap();
        for (sequence, created_at) in events {
            store
                .conn
                .execute(
                    "INSERT INTO conversation_events(session_id,sequence,event_type,role,content,created_at)
                     VALUES(?1,?2,'Message','assistant','done',?3)",
                    params![session.id.to_string(), sequence, created_at],
                )
                .unwrap();
        }
        let session = store.get_session(session.id).unwrap().unwrap();
        store.delegated_archive_blocker(&session, now).unwrap()
    }

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .unwrap()
            .with_timezone(&Utc)
    }

    const RECENT: Option<&str> = Some("manager_v2_retention_recent_activity");

    #[test]
    fn retention_activity_refuses_a_sub_millisecond_newer_event_with_mixed_offsets() {
        // Cutoff (now - 24 h) is 00:00:00.000250Z. The older event (.000100Z,
        // stored as 01:00:00.000100+01:00) shares a julianday millisecond with
        // the newer one (.000400Z) and sorts after it as text.
        let now = at("2026-09-23T00:00:00.000250Z");
        assert_eq!(
            blocker_with_events(
                now,
                &[
                    (1, "2026-09-22T00:00:00.000400Z"),
                    (2, "2026-09-22T01:00:00.000100+01:00"),
                ],
            )
            .as_deref(),
            RECENT
        );
        // Control: the same two events, both before the cutoff, pass retention.
        let later = at("2026-09-23T00:00:00.000500Z");
        assert_eq!(
            blocker_with_events(
                later,
                &[
                    (1, "2026-09-22T00:00:00.000400Z"),
                    (2, "2026-09-22T01:00:00.000100+01:00"),
                ],
            ),
            None
        );
    }

    #[test]
    fn retention_activity_fails_closed_on_an_offsetless_event_that_is_not_the_latest() {
        let now = at("2026-09-23T12:00:00Z");
        // SQLite reads the offsetless value as 2026-09-21T00:00:00 (older than
        // the valid event); chrono rejects it, so retention must refuse.
        assert_eq!(
            blocker_with_events(
                now,
                &[(1, "2026-09-21T00:00:00"), (2, "2026-09-22T00:00:00+00:00")],
            )
            .as_deref(),
            RECENT
        );
        // Control: the valid old event alone passes retention.
        assert_eq!(
            blocker_with_events(now, &[(1, "2026-09-22T00:00:00+00:00")]),
            None
        );
    }
}
