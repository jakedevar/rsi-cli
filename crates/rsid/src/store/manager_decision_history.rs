//! Native decision history is retained independently of the bounded agent work
//! ledger. Current queues and page reads never materialize that history.
use super::{
    Store,
    harness_manager_v2::{ManagerRecordV2, refused},
};
use crate::error::Result;
use rsi_common::harness_manager::HarnessManagerConfigV1;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use uuid::Uuid;

pub(super) fn native_history(kind: &str, key: &str, payload: &Value) -> bool {
    kind == "decision_generation"
        || (matches!(kind, "decision" | "decision_target") && key.starts_with("approval:"))
        || (kind == "decision_delivery"
            && payload
                .pointer("/target/kind")
                .is_some_and(|k| k == "appserver_approval"))
        || (kind == "decision_retrieval"
            && (key.starts_with("manager:approval:") || key.starts_with("lead:approval:")))
}

pub(super) const LEGACY_APPROVAL_CANDIDATES_SQL: &str = "SELECT id FROM approvals INDEXED BY manager_pending_approval_scan WHERE session_id=?1 AND status='Pending' AND id>?2 ORDER BY id LIMIT 65";
pub(super) const LEGACY_APPROVAL_ROW_SQL: &str = "SELECT session_id,tool_name,created_at FROM approvals WHERE id=?1 AND status='Pending' AND NOT EXISTS(SELECT 1 FROM appserver_approval_publications p INDEXED BY manager_native_approval_mirror WHERE p.approval_id=approvals.id)";

impl Store {
    /// Read at most 65 indexed candidates per bounded graph member, retaining
    /// only the first 65 globally. Filtering native mirrors happens afterward;
    /// even an empty visible page must advance the returned scan frontier.
    pub(crate) fn manager_v2_legacy_approval_candidates(
        &self,
        sessions: &[Uuid],
        after: &str,
    ) -> Result<(Vec<String>, Option<String>)> {
        let mut ids = BTreeSet::new();
        let mut stmt = self.conn.prepare(LEGACY_APPROVAL_CANDIDATES_SQL)?;
        for session in sessions {
            for id in stmt.query_map(params![session.to_string(), after], |r| {
                r.get::<_, String>(0)
            })? {
                ids.insert(id?);
                if ids.len() > 65 {
                    ids.pop_last();
                }
            }
        }
        let more = ids.len() > 64;
        let ids: Vec<_> = ids.into_iter().take(64).collect();
        let next = more.then(|| ids.last().unwrap().clone());
        Ok((ids, next))
    }

    pub(crate) fn manager_v2_legacy_approval_row(
        &self,
        id: &str,
    ) -> Result<Option<(Uuid, String, String)>> {
        let row: Option<(String, String, String)> = self
            .conn
            .query_row(LEGACY_APPROVAL_ROW_SQL, [id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .optional()?;
        row.map(|(session, tool, created)| {
            Ok((
                Uuid::parse_str(&session)
                    .map_err(|_| refused("manager_v2_invalid_stored_identity"))?,
                tool,
                created,
            ))
        })
        .transpose()
    }
    pub(crate) fn manager_v2_decision_generation(
        &self,
        config: &HarnessManagerConfigV1,
        epic: Option<Uuid>,
    ) -> Result<i64> {
        Ok(self
            .manager_v2_record(
                config,
                "decision_generation",
                &epic.map_or_else(|| "project".into(), |e| e.to_string()),
            )?
            .map_or(0, |r| r.row_version))
    }

    pub(super) fn manager_v2_touch_decision_generation(
        &self,
        config: &HarnessManagerConfigV1,
        epic: Option<Uuid>,
    ) -> Result<()> {
        let key = epic.map_or_else(|| "project".into(), |e| e.to_string());
        let version = self.manager_v2_decision_generation(config, epic)?;
        self.manager_v2_put_record(
            config,
            "decision_generation",
            &key,
            epic,
            version,
            &json!({"epic_id":epic}),
        )?;
        Ok(())
    }

    /// The primary key supplies an indexed, live keyset. Later insertions or
    /// changed rows behind the cursor are visible on refresh, never repeated.
    pub(crate) fn manager_v2_record_page(
        &self,
        config: &HarnessManagerConfigV1,
        kind: &str,
        epic: Option<Uuid>,
        after: &str,
        limit: usize,
    ) -> Result<Vec<ManagerRecordV2>> {
        let sql = if epic.is_some() {
            "SELECT record_key FROM harness_manager_v2_records WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3 AND kind=?4 AND record_key>?5 AND epic_id=?6 ORDER BY record_key LIMIT ?7"
        } else {
            "SELECT record_key FROM harness_manager_v2_records WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3 AND kind=?4 AND record_key>?5 AND ?6 IS NULL ORDER BY record_key LIMIT ?7"
        };
        let mut stmt = self.conn.prepare(sql)?;
        let keys = stmt
            .query_map(
                params![
                    config.project_id.to_string(),
                    config.manager_session_id.to_string(),
                    config.row_version,
                    kind,
                    after,
                    epic.map(|e| e.to_string()),
                    limit.min(65) as i64
                ],
                |r| r.get::<_, String>(0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        keys.into_iter()
            .map(|key| {
                self.manager_v2_record(config, kind, &key)?
                    .ok_or_else(|| refused("manager_v2_record_missing"))
            })
            .collect()
    }

    pub(crate) fn manager_v2_queued_decision_deliveries(
        &self,
        config: &HarnessManagerConfigV1,
    ) -> Result<Vec<ManagerRecordV2>> {
        let mut stmt = self.conn.prepare("SELECT record_key FROM harness_manager_v2_records INDEXED BY harness_manager_v2_queued_answers WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3 AND kind='decision_delivery' AND json_extract(payload_json,'$.state')='queued' ORDER BY record_key LIMIT 32")?;
        let keys = stmt
            .query_map(
                params![
                    config.project_id.to_string(),
                    config.manager_session_id.to_string(),
                    config.row_version
                ],
                |r| r.get::<_, String>(0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        keys.into_iter()
            .map(|key| {
                self.manager_v2_record(config, "decision_delivery", &key)?
                    .ok_or_else(|| refused("manager_v2_record_missing"))
            })
            .collect()
    }

    pub(crate) fn manager_v2_unread_operator_answer(
        &self,
        config: &HarnessManagerConfigV1,
        epic: Uuid,
        lead: Uuid,
    ) -> Result<bool> {
        Ok(self.conn.query_row("SELECT EXISTS(SELECT 1 FROM harness_manager_v2_records d INDEXED BY harness_manager_v2_operator_inbox WHERE d.project_id=?1 AND d.manager_session_id=?2 AND d.scope_version=?3 AND d.kind='decision' AND d.epic_id=?4 AND json_extract(d.payload_json,'$.delivery.state')='available_in_scoped_inbox' AND NOT EXISTS(SELECT 1 FROM harness_manager_v2_records r WHERE r.project_id=d.project_id AND r.manager_session_id=d.manager_session_id AND r.scope_version=d.scope_version AND r.kind='decision_retrieval' AND r.record_key='lead:'||d.record_key AND json_extract(r.payload_json,'$.target_digest')=json_extract(d.payload_json,'$.target_digest') AND json_extract(r.payload_json,'$.actor_session_id')=?5))",params![config.project_id.to_string(),config.manager_session_id.to_string(),config.row_version,epic.to_string(),lead.to_string()],|r|r.get(0))?)
    }
}

#[cfg(test)]
mod tests;
