//! Issue #548: read-only work/ownership projection for a session the current
//! manager created (`create_session` / `retry_lead` / `replace_lead` entity).
//!
//! Every identity is daemon-derived: the caller is the live lineage tip of a
//! root session bound to a `harness_manager_v2_entities` row of the current
//! logical manager seat at the current scope version, inside a live scoped
//! Epic, under a live V2 grant for that seat and scope. Anything else fails
//! closed with a typed code and no data. The read runs in one deferred
//! transaction that is always rolled back, so it can never write a row.
use super::super::Store;
use super::super::harness_manager_v2::refused;
use super::{FactReach, Ownership, WorkRecord, decode};
use crate::error::Result;
use chrono::{DateTime, Utc};
use rsi_common::harness_manager::{
    AgentManagerWorkViewRequestV1, AgentManagerWorkViewResultV1, HarnessManagerConfigV1,
    MANAGER_WORK_VIEW_MAX_RELAY, ManagerWorkViewOwnershipV1, ManagerWorkViewRelayV1,
    ManagerWorkViewStageV1, ManagerWorkViewWorkV1,
};
use rsi_common::types::SessionStatus;
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use uuid::Uuid;

fn stamp(text: &str) -> Result<DateTime<Utc>> {
    super::super::parse_timestamp(text).map_err(|_| refused("manager_v2_invalid_stored_timestamp"))
}

fn maybe_stamp(text: Option<&str>) -> Result<Option<DateTime<Utc>>> {
    text.map(stamp).transpose()
}

type RelayTimes = (
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

/// Daemon-derived identity of an authorized work-view caller.
struct WorkViewCaller {
    config: HarnessManagerConfigV1,
    caller: Uuid,
    root: Uuid,
    epic: Uuid,
    policy_version: i64,
    paused: bool,
}

/// One keyset page of the caller Epic's works.
struct WorkPage {
    works: Vec<(String, i64, WorkRecord)>,
    next_after_work_key: Option<String>,
}

impl Store {
    pub(crate) fn manager_work_view(
        &self,
        caller: Uuid,
        request: &AgentManagerWorkViewRequestV1,
    ) -> Result<AgentManagerWorkViewResultV1> {
        request.validate().map_err(refused)?;
        // Deferred read snapshot; dropped (rolled back) on every path.
        let _snapshot = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        let who = self.work_view_caller(caller)?;
        let page = self.work_view_page(&who, request)?;
        let works = page
            .works
            .iter()
            .map(|(key, row_version, work)| self.work_view_work(&who, key, *row_version, work))
            .collect::<Result<Vec<_>>>()?;
        let keys: Vec<String> = page.works.into_iter().map(|(key, _, _)| key).collect();
        let ownership = self.work_view_ownership(&who, &keys)?;
        let (relay, more_relay) = self.work_view_relay(&who)?;
        Ok(AgentManagerWorkViewResultV1 {
            observed_at: Utc::now(),
            epic_id: who.epic,
            manager_session_id: who.config.manager_session_id,
            scope_version: who.config.row_version,
            policy_version: who.policy_version,
            paused: who.paused,
            works,
            ownership,
            relay,
            more_relay,
            next_after_work_key: page.next_after_work_key,
        })
    }

    fn work_view_caller(&self, caller: Uuid) -> Result<WorkViewCaller> {
        let session = self
            .get_session(caller)?
            .ok_or_else(|| refused("manager_work_view_unsupported_topology"))?;
        let Some(project) = session.project_id.filter(|_| {
            rsi_common::is_leaf_kind(session.session_kind)
                && session.status != SessionStatus::Deleted
        }) else {
            return Err(refused("manager_work_view_unsupported_topology"));
        };
        let config = self
            .get_harness_manager(project)?
            .ok_or_else(|| refused("manager_not_configured"))?;
        if config.current_session_id.is_none() {
            return Err(refused("manager_current_session_required"));
        }
        let root = self.manager_lineage_root(caller)?;
        if session.status == SessionStatus::Archived || self.manager_lineage_tip(root)? != caller {
            return Err(refused("manager_work_view_stale_session"));
        }
        let managed: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM harness_manager_v2_entities
              WHERE session_id=?1 AND project_id=?2 AND manager_session_id=?3
                AND scope_version=?4 AND kind='session')",
            params![
                root.to_string(),
                project.to_string(),
                config.manager_session_id.to_string(),
                config.row_version
            ],
            |row| row.get(0),
        )?;
        if !managed {
            return Err(refused("manager_work_view_not_managed"));
        }
        let epic = self.manager_v2_descendant_epic(&config, caller)?;
        let grant = self
            .get_harness_manager_policy(project)?
            .filter(|grant| {
                !grant.revoked
                    && grant.manager_session_id == config.manager_session_id
                    && grant.scope_version == config.row_version
            })
            .ok_or_else(|| refused("manager_v2_policy_changed"))?;
        Ok(WorkViewCaller {
            config,
            caller,
            root,
            epic,
            policy_version: grant.row_version,
            // Pause is reported, never refused: reading changes nothing.
            paused: grant.policy.paused || grant.policy.paused_epic_ids.contains(&epic),
        })
    }

    fn work_view_page(
        &self,
        who: &WorkViewCaller,
        request: &AgentManagerWorkViewRequestV1,
    ) -> Result<WorkPage> {
        let after = request.after_work_key.as_deref().unwrap_or("");
        let candidates = match request.work_key.as_deref() {
            // Exact key: another Epic's work stays invisible (empty page); a
            // terminal (archived or integrated) work of this Epic is refused
            // with a typed code, never projected with its stale ownership.
            Some(key) => match self.manager_v2_fact(who.config.project_id, "work", key)? {
                Some(row) if row.epic_id == Some(who.epic) => {
                    if !self.manager_v2_fact_is_live(who.config.project_id, "work", key)? {
                        return Err(refused("manager_work_view_work_not_live"));
                    }
                    vec![row]
                }
                _ => Vec::new(),
            },
            None => self.manager_v2_facts(&who.config, "work", FactReach::Scoped)?,
        };
        let limit = usize::from(request.limit);
        let mut page = WorkPage {
            works: Vec::new(),
            next_after_work_key: None,
        };
        for row in candidates {
            if row.epic_id != Some(who.epic) || row.key.as_str() <= after {
                continue;
            }
            if page.works.len() == limit {
                page.next_after_work_key = page.works.last().map(|(key, _, _)| key.clone());
                break;
            }
            let work: WorkRecord = decode(&row)?;
            page.works.push((row.key, row.row_version, work));
        }
        Ok(page)
    }

    fn work_view_work(
        &self,
        who: &WorkViewCaller,
        key: &str,
        row_version: i64,
        work: &WorkRecord,
    ) -> Result<ManagerWorkViewWorkV1> {
        let mine = work.source_session_id.is_some_and(|source| {
            source == who.caller
                || source == who.root
                || self.manager_lineage_root(source).ok() == Some(who.root)
        });
        Ok(ManagerWorkViewWorkV1 {
            work_key: key.to_owned(),
            title: work.title.clone(),
            kind: work.kind,
            spec_revision: work.spec_revision,
            row_version,
            mine,
            source_session_id: work.source_session_id,
            source_commit: work.source_commit.clone(),
            stages: work
                .stages
                .iter()
                .map(|s| {
                    Ok(ManagerWorkViewStageV1 {
                        stage: s.stage,
                        state: s.state,
                        updated_at: stamp(&s.updated_at)?,
                    })
                })
                .collect::<Result<_>>()?,
            accepted: work.acceptance.is_some(),
            integrated: work.integration.is_some(),
        })
    }

    fn work_view_ownership(
        &self,
        who: &WorkViewCaller,
        keys: &[String],
    ) -> Result<Vec<ManagerWorkViewOwnershipV1>> {
        let mut ownership = Vec::new();
        for row in self.manager_v2_facts_of_works(who.config.project_id, "ownership", keys)? {
            if row.archived || row.epic_id != Some(who.epic) {
                continue;
            }
            let claim: Ownership = decode(&row)?;
            if !claim.active {
                continue;
            }
            ownership.push(ManagerWorkViewOwnershipV1 {
                key: row.key,
                work_key: claim.work_key,
                domain: claim.domain,
                mode: claim.mode,
                files: claim.files,
                active: claim.active,
                row_version: row.row_version,
                updated_at: stamp(&row.updated_at)?,
            });
        }
        Ok(ownership)
    }

    /// Unanswered manager requests to the caller's Epic lead, delivery fields
    /// only: no message body leaves the manager/lead inbox.
    fn work_view_relay(&self, who: &WorkViewCaller) -> Result<(Vec<ManagerWorkViewRelayV1>, bool)> {
        let rows = self.manager_v2_request_rows(
            &who.config,
            Some(who.epic),
            "",
            MANAGER_WORK_VIEW_MAX_RELAY + 1,
            true,
        )?;
        let more = rows.len() > MANAGER_WORK_VIEW_MAX_RELAY;
        let mut relay = Vec::new();
        for row in rows.into_iter().take(MANAGER_WORK_VIEW_MAX_RELAY) {
            let request_id = row["request_id"]
                .as_str()
                .and_then(|id| Uuid::parse_str(id).ok())
                .ok_or_else(|| refused("manager_v2_invalid_stored_identity"))?;
            let (created, queued, delivered, retrieved_at, settled): RelayTimes = self
                .conn
                .query_row(
                    "SELECT m.created_at,n.queued_at,n.delivered_at,n.retrieved_at,n.settled_at
                       FROM harness_manager_messages m
                       LEFT JOIN harness_manager_notices n ON n.sequence=(
                            SELECT max(x.sequence) FROM harness_manager_notices x
                             WHERE x.project_id=m.project_id AND x.direction='to_lead'
                               AND x.kind='message' AND x.subject_id=m.id)
                      WHERE m.id=?1",
                    [request_id.to_string()],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                )
                .optional()?
                .ok_or_else(|| refused("manager_v2_invalid_stored_identity"))?;
            relay.push(ManagerWorkViewRelayV1 {
                request_id,
                state: row["state"].as_str().unwrap_or("queued").to_owned(),
                retrieved: row["retrieved"].as_bool().unwrap_or(false),
                replied: row["replied"].as_bool().unwrap_or(false),
                delivery_issue: row["delivery_issue"].as_str().map(str::to_owned),
                queued_at: stamp(queued.as_deref().unwrap_or(&created))?,
                delivered_at: maybe_stamp(delivered.as_deref())?,
                retrieved_at: maybe_stamp(retrieved_at.as_deref())?,
                settled_at: maybe_stamp(settled.as_deref())?,
            });
        }
        Ok((relay, more))
    }
}
