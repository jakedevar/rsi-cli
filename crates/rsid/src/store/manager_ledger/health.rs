//! Per-Epic fleet health for `AgentManagerInspect` section `Health` (#627).
//!
//! One `health` row per scoped Epic, keyed and paged by Epic id. Every
//! aggregate is one indexed per-Epic read bounded by [`HEALTH_BOUND`]; a
//! bounded read that reaches its bound sets the row's `complete:false` and
//! names the field in `truncated`. Nothing here touches the filesystem, and
//! nothing here is a gate: `stuck` codes are daemon-detected observations with
//! their evidence ids, never acceptance or failure verdicts.
use super::{HarnessManagerConfigV1, Result, Store, Uuid, Value, bounded, json, params, refused};
use crate::store::harness_manager::{manager_request_open_sql, manager_request_unanswered_sql};
use crate::store::row_mappers::parse_timestamp;
use chrono::{DateTime, Utc};
use rsi_common::types::{Session, SessionStatus};
use rusqlite::OptionalExtension;
use std::collections::BTreeMap;

/// Items read per aggregate (counts, evidence ids, failure codes).
pub const HEALTH_BOUND: usize = 32;
/// A delivered manager notice still unretrieved this long is `manager_notice_unread`.
pub const HEALTH_NOTICE_UNREAD_AFTER_SECS: i64 = 30 * 60;
/// Newest lead transcript events searched for the K10d `delivery_abandoned` fact.
pub const HEALTH_FACT_EVENT_WINDOW: i64 = 2048;
/// Newest lead `rotation_events` rows searched for its latest rotation outcome.
pub const HEALTH_ROTATION_EVENT_WINDOW: i64 = 256;
/// SQL `LIMIT` for a bounded read: `HEALTH_BOUND + 1`, so one extra row
/// detects truncation.
const READ_LIMIT: i64 = 33;
const _: () = assert!(READ_LIMIT.unsigned_abs() == HEALTH_BOUND as u64 + 1);

/// Latest lead->manager reply or lead notice. Seeks by Epic: those messages are
/// sent by a direct Epic child, so the plan walks `idx_sessions_parent_id` and
/// then the `(sender_session_id, idempotency_key)` autoindex (`CROSS JOIN` pins
/// that order; unary `+` keeps the cross-Epic inbox index out of the probe).
/// Params: project, scope version, manager, Epic.
pub const LATEST_REPORT_SQL: &str = "SELECT m.id,m.created_at,m.request_id IS NOT NULL
     FROM sessions s CROSS JOIN harness_manager_messages m ON m.sender_session_id=s.id
     WHERE s.parent_id=?4 AND +m.project_id=?1 AND +m.scope_version=?2
       AND m.manager_session_id=?3 AND m.epic_id=?4
       AND (m.request_id IS NOT NULL OR m.request_fingerprint GLOB 'lead-notice:*')
     ORDER BY m.sequence DESC LIMIT 1";
/// Uncertain successor reservations of one Epic. Seeks by Epic: a
/// reservation's predecessor is a direct Epic child, so the plan walks
/// `idx_sessions_parent_id`, then `idx_agent_successor_predecessor`.
/// Params: Epic, limit.
pub const UNCERTAIN_SUCCESSOR_SQL: &str = "SELECT r.reservation_id
     FROM sessions s CROSS JOIN agent_successor_reservations r ON r.predecessor_session_id=s.id
     WHERE s.parent_id=?1 AND r.epic_id=?1 AND r.state='uncertain'
     ORDER BY r.updated_at,r.reservation_id LIMIT ?2";

const LIVE: &str = "('Starting','Running','WaitingApproval')";

fn age(now: DateTime<Utc>, raw: Option<&str>) -> Option<i64> {
    raw.and_then(|raw| parse_timestamp(raw).ok())
        .map(|at| (now - at).num_seconds().max(0))
}

/// Bounded page of one read: `true` when the bound was reached.
fn cap<T>(mut rows: Vec<T>) -> (Vec<T>, bool) {
    let over = rows.len() > HEALTH_BOUND;
    rows.truncate(HEALTH_BOUND);
    (rows, over)
}

/// One Epic's identity and the row being assembled.
struct Row<'a> {
    config: &'a HarnessManagerConfigV1,
    project: String,
    manager: String,
    epic: String,
    now: DateTime<Utc>,
    lead: Option<Session>,
    /// Current lead, or the Epic's stored lead pointer when it is not current.
    lead_id: Option<Uuid>,
    truncated: Vec<&'static str>,
    stuck: Vec<Value>,
}
impl Row<'_> {
    fn mark(&mut self, over: bool, field: &'static str) {
        if over {
            self.truncated.push(field);
        }
    }
    fn stuck(&mut self, code: &str, evidence: &[Value]) {
        if !evidence.is_empty() {
            self.stuck.push(json!({"code":code,"evidence":evidence}));
        }
    }
}

impl Store {
    /// Keyset page of health rows for scoped Epics after `after`. Returns up to
    /// `limit` rows (the caller asks for one extra to detect more pages).
    pub(super) fn manager_v2_health_rows(
        &self,
        config: &HarnessManagerConfigV1,
        epic_filter: Option<Uuid>,
        after: &str,
        limit: u16,
    ) -> Result<Vec<Value>> {
        let mut epics: Vec<String> = config
            .epic_ids
            .iter()
            .filter(|id| epic_filter.is_none_or(|e| e == **id))
            .map(Uuid::to_string)
            .filter(|id| id.as_str() > after)
            .collect();
        epics.sort();
        epics.dedup();
        epics.truncate(usize::from(limit));
        let now = Utc::now();
        epics
            .into_iter()
            .map(|id| {
                let epic = Uuid::parse_str(&id)
                    .map_err(|_| refused("manager_v2_invalid_stored_identity"))?;
                self.manager_v2_health_row(config, epic, now)
            })
            .collect()
    }

    fn manager_v2_health_row(
        &self,
        config: &HarnessManagerConfigV1,
        epic: Uuid,
        now: DateTime<Utc>,
    ) -> Result<Value> {
        let epic_session = self.get_session(epic)?;
        let lead = self.manager_lead(config.project_id, epic).ok();
        let lead_id = lead
            .as_ref()
            .map(|s| s.id)
            .or_else(|| epic_session.as_ref().and_then(|s| s.lead_session_id));
        let mut row = Row {
            config,
            project: config.project_id.to_string(),
            manager: config.manager_session_id.to_string(),
            epic: epic.to_string(),
            now,
            lead,
            lead_id,
            truncated: Vec::new(),
            stuck: Vec::new(),
        };
        let lead_json = row.lead.as_ref().map(|s| {
            let tip = self.manager_lineage_tip(s.id).ok();
            json!({"session_id":s.id,"status":s.status,"status_updated_at":s.updated_at,
                "age_seconds":(now - s.updated_at).num_seconds().max(0),
                "lineage_tip_id":tip,"lineage_state":if tip.is_some() {"current"} else {"unavailable"},
                "provider":s.provider,"model":s.model})
        });
        let (children, wakes) = self.health_children_and_wakes(&mut row)?;
        let notices = self.health_notices(&mut row)?;
        let (reports, lead_changed) = self.health_reports(&mut row)?;
        let reviews = self.health_reviews(&mut row)?;
        let uncertain = self.health_successors(&mut row)?;
        let fact = self.health_delivery_abandoned(&mut row)?;
        let rotation_refused = self.health_rotation_refused(&mut row)?;
        let retry_owner = self.health_lead_unavailable(&mut row)?;
        let latest_daemon_restart = self.latest_daemon_restart_record()?;
        let evidence: Vec<Value> = lead_changed
            .iter()
            .map(|r| r["request_id"].clone())
            .collect();
        row.stuck("manager_request_lead_changed", &evidence);
        Ok(json!({
            "type":"health", "key":epic, "epic_id":epic,
            "title":epic_session.as_ref().map(|s| bounded(s.title.as_deref().unwrap_or(&s.query), 512)),
            "epic_status":epic_session.as_ref().map(|s| s.status),
            "lead":lead_json, "lead_state":if row.lead.is_some() {"current"} else {"unavailable"},
            "children":children, "wakes":wakes, "notices":notices,
            "reports":reports, "reviews":reviews,
            "facts":{"delivery_abandoned":fact,"requests_lead_changed":lead_changed,
                "successor_uncertain":uncertain,"rotation_refused":rotation_refused,
                "lead_retry_owner":retry_owner},
            "stuck":row.stuck,
            "latest_daemon_restart":latest_daemon_restart,
            "complete":row.truncated.is_empty(),
            "truncated":row.truncated,
        }))
    }

    /// `children` and `wakes`, plus `manager_watch_missing`.
    fn health_children_and_wakes(&self, row: &mut Row<'_>) -> Result<(Value, Value)> {
        // idx_sessions_parent_id (parent_id, rowid): rowid order is creation
        // order, so the first live row is the oldest.
        let (children, over) = cap(self
            .conn
            .prepare(&format!(
                "SELECT id,created_at FROM sessions WHERE parent_id=?1 AND id IS NOT ?2
                 AND status IN {LIVE} ORDER BY rowid LIMIT ?3"
            ))?
            .query_map(
                params![row.epic, row.lead_id.map(|id| id.to_string()), READ_LIMIT],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?);
        row.mark(over, "children.live");
        // harness_manager_watches_scope (project_id, scope_version).
        let (watches, over) = cap(self
            .conn
            .prepare(
                "SELECT w.direction,COALESCE(j.enabled,0) FROM harness_manager_watches w
                 LEFT JOIN scheduled_jobs j ON j.id=w.job_id
                 WHERE w.project_id=?1 AND w.scope_version=?2 AND w.epic_id=?3
                 ORDER BY w.job_id LIMIT ?4",
            )?
            .query_map(
                params![row.project, row.config.row_version, row.epic, READ_LIMIT],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?)),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?);
        row.mark(over, "wakes.manager_watches");
        let mut manager_watches = json!({});
        for direction in ["to_manager", "to_lead"] {
            let of = |enabled: bool| {
                watches
                    .iter()
                    .filter(|(d, e)| d == direction && *e == enabled)
                    .count()
            };
            manager_watches[direction] = json!({"enabled":of(true),"disabled":of(false)});
        }
        // Lead terminal watches on its live Epic siblings:
        // idx_scheduled_jobs_agent_watch_natural (wake_session_id, wake_mode).
        let child_ids: Vec<&str> = children.iter().map(|(id, _)| id.as_str()).collect();
        let lead_child_watches: i64 = match row.lead_id {
            Some(lead) if !child_ids.is_empty() => self.conn.query_row(
                "SELECT count(*) FROM scheduled_jobs WHERE wake_session_id=?1 AND enabled=1
                 AND wake_mode LIKE 'on_terminal:%'
                 AND wake_mode IN (SELECT 'on_terminal:'||value FROM json_each(?2))",
                params![lead.to_string(), serde_json::to_string(&child_ids)?],
                |r| r.get(0),
            )?,
            _ => 0,
        };
        let idle_lead = row.lead.as_ref().filter(|lead| {
            matches!(
                lead.status,
                SessionStatus::Completed | SessionStatus::Interrupted
            )
        });
        if let Some(lead) = idle_lead
            && !children.is_empty()
            && lead_child_watches == 0
        {
            let mut evidence = vec![json!(lead.id)];
            evidence.extend(child_ids.iter().take(HEALTH_BOUND - 1).map(|id| json!(id)));
            row.stuck("manager_watch_missing", &evidence);
        }
        let oldest = children.first();
        Ok((
            json!({"live":children.len(),"oldest_live_session_id":oldest.map(|c| &c.0),
                "oldest_live_age_seconds":age(row.now, oldest.map(|c| c.1.as_str()))}),
            json!({"manager_watches":manager_watches,"lead_child_watches":lead_child_watches,
                "lead_child_watch":lead_child_watches > 0}),
        ))
    }

    /// Pending notices per direction, plus `manager_notice_transport_stalled`
    /// and `manager_notice_unread`.
    fn health_notices(&self, row: &mut Row<'_>) -> Result<Value> {
        let mut notices = json!({});
        let mut stalled = Vec::new();
        let mut unread = Vec::new();
        for (direction, field) in [
            ("to_manager", "notices.to_manager"),
            ("to_lead", "notices.to_lead"),
        ] {
            // harness_manager_notices_pending_epic_sequence
            // (project, manager, scope, direction, epic_id, sequence) WHERE pending.
            let (pending, over) = cap(self
                .conn
                .prepare(
                    "SELECT n.id,n.job_id,n.delivered_at,n.queued_at,COALESCE(j.enabled,0)
                     FROM harness_manager_notices n LEFT JOIN scheduled_jobs j ON j.id=n.job_id
                     WHERE n.project_id=?1 AND n.manager_session_id=?2 AND n.scope_version=?3
                       AND n.direction=?4 AND n.epic_id=?5
                       AND n.retired_at IS NULL AND n.settled_at IS NULL
                     ORDER BY n.sequence LIMIT ?6",
                )?
                .query_map(
                    params![
                        row.project,
                        row.manager,
                        row.config.row_version,
                        direction,
                        row.epic,
                        READ_LIMIT
                    ],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, Option<String>>(2)?,
                            r.get::<_, String>(3)?,
                            r.get::<_, bool>(4)?,
                        ))
                    },
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?);
            row.mark(over, field);
            for (id, job, delivered, _, enabled) in &pending {
                if !enabled && !stalled.contains(&json!(job)) {
                    stalled.push(json!(job));
                }
                if age(row.now, delivered.as_deref())
                    .is_some_and(|a| a >= HEALTH_NOTICE_UNREAD_AFTER_SECS)
                {
                    unread.push(json!(id));
                }
            }
            let undelivered = pending.iter().filter(|n| n.2.is_none()).count();
            let oldest_at = pending.first().map(|n| n.3.clone());
            notices[direction] = json!({"pending":pending.len(),"undelivered":undelivered,
                "delivered_unretrieved":pending.len() - undelivered,
                "oldest_pending_at":oldest_at,
                "oldest_pending_age_seconds":age(row.now, oldest_at.as_deref())});
        }
        row.stuck("manager_notice_transport_stalled", &stalled);
        row.stuck("manager_notice_unread", &unread);
        Ok(notices)
    }

    /// Latest lead->manager message and open manager requests; returns the
    /// `reports` object and the open requests addressed to a non-current lead.
    fn health_reports(&self, row: &mut Row<'_>) -> Result<(Value, Vec<Value>)> {
        // harness_manager_messages_inbox (project_id, scope_version, sequence),
        // newest first; replies and lead notices flow lead->manager.
        let latest = self
            .conn
            .query_row(
                LATEST_REPORT_SQL,
                params![row.project, row.config.row_version, row.manager, row.epic],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, bool>(2)?,
                    ))
                },
            )
            .optional()?
            .map(|(id, at, reply)| {
                json!({"message_id":id,"kind":if reply {"reply"} else {"lead_notice"},
                    "created_at":at,"age_seconds":age(row.now, Some(&at))})
            });
        // Active manager requests: the shared #664 attention predicate (the
        // open set plus reopened `failed -> accepted` requests), applied
        // BEFORE the LIMIT so closed rows can neither be reported as
        // `manager_request_lead_changed` nor crowd out a later open request.
        let (open, over) = cap(self
            .conn
            .prepare(&format!(
                "SELECT m.id,m.recipient_session_id FROM harness_manager_messages m
                 WHERE m.project_id=?1 AND m.scope_version=?2 AND m.manager_session_id=?3
                   AND m.epic_id=?4 AND {}
                 ORDER BY m.sequence LIMIT ?5",
                manager_request_unanswered_sql()
            ))?
            .query_map(
                params![
                    row.project,
                    row.config.row_version,
                    row.manager,
                    row.epic,
                    READ_LIMIT
                ],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?);
        row.mark(over, "reports.open_requests");
        let current = row.lead.as_ref().map(|s| s.id);
        let mut unanswered = 0usize;
        let mut lead_changed = Vec::new();
        for (id, recipient) in open {
            let tip = Uuid::parse_str(&recipient)
                .ok()
                .and_then(|r| self.manager_lineage_tip(r).ok());
            if tip.is_some() && tip == current {
                unanswered += 1;
            } else {
                lead_changed.push(json!({"request_id":id,"recipient_session_id":recipient}));
            }
        }
        // Unbounded aggregates: `active_requests` includes reopened requests,
        // `pending_requests` is the capacity slot count Progress and the send
        // cap read. A reopen never reclaims a slot, so active > pending makes
        // any overshoot above the per-Epic cap observable.
        let count = |predicate: String| -> Result<i64> {
            Ok(self.conn.query_row(
                &format!(
                    "SELECT count(*) FROM harness_manager_messages m
                     WHERE m.project_id=?1 AND m.scope_version=?2 AND m.manager_session_id=?3
                       AND m.epic_id=?4 AND {predicate}"
                ),
                params![row.project, row.config.row_version, row.manager, row.epic],
                |r| r.get(0),
            )?)
        };
        let active = count(manager_request_unanswered_sql())?;
        let pending = count(manager_request_open_sql())?;
        Ok((
            json!({"latest":latest,"unanswered_requests":unanswered,
                "active_requests":active,"pending_requests":pending}),
            lead_changed,
        ))
    }

    /// Current review assignments, plus one stuck entry per failure code.
    fn health_reviews(&self, row: &mut Row<'_>) -> Result<Value> {
        // manager_review_assignments_one_current (project_id, epic_id, ...)
        // WHERE superseded_by_assignment_id IS NULL.
        let (reviews, over) = cap(self
            .conn
            .prepare(
                "SELECT assignment_id,state,failure_code FROM manager_review_assignments
                 WHERE project_id=?1 AND epic_id=?2 AND superseded_by_assignment_id IS NULL
                   AND state IN ('reserved','allocating','active','failed')
                 ORDER BY assignment_id LIMIT ?3",
            )?
            .query_map(params![row.project, row.epic, READ_LIMIT], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?);
        row.mark(over, "reviews");
        let mut failure_codes = Vec::new();
        let mut by_code: BTreeMap<String, Vec<Value>> = BTreeMap::new();
        for (id, _, code) in reviews.iter().filter(|r| r.1 == "failed") {
            let code = code
                .clone()
                .unwrap_or_else(|| "manager_review_failed".into());
            failure_codes.push(json!({"assignment_id":id,"failure_code":code}));
            by_code.entry(code).or_default().push(json!(id));
        }
        for (code, evidence) in &by_code {
            row.stuck(code, evidence);
        }
        Ok(json!({"active":reviews.len() - failure_codes.len(),
            "failed":failure_codes.len(),"failure_codes":failure_codes}))
    }

    /// Uncertain successor reservation ids (`agent_successor_uncertain`).
    fn health_successors(&self, row: &mut Row<'_>) -> Result<Vec<String>> {
        let (uncertain, over) = cap(self
            .conn
            .prepare(UNCERTAIN_SUCCESSOR_SQL)?
            .query_map(params![row.epic, READ_LIMIT], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?);
        row.mark(over, "successors.uncertain");
        let evidence: Vec<Value> = uncertain.iter().map(|id| json!(id)).collect();
        row.stuck("agent_successor_uncertain", &evidence);
        Ok(uncertain)
    }

    /// `lead_unavailable`: the committed lead (the Epic's lead pointer) is
    /// Failed, Interrupted, Archived or Deleted. For a Failed lead, an open
    /// capacity-recovery incident (the only store-visible retry owner) is
    /// appended to the evidence and returned for `facts.lead_retry_owner`.
    fn health_lead_unavailable(&self, row: &mut Row<'_>) -> Result<Option<String>> {
        let lead = match (&row.lead, row.lead_id) {
            (Some(lead), _) => Some(lead.clone()),
            (None, Some(id)) => self.get_session(id)?,
            (None, None) => None,
        };
        let Some(lead) = lead else {
            return Ok(None);
        };
        let owner = if lead.status == SessionStatus::Failed {
            // idx_master_no_idle_capacity_incidents_controller
            // (controller_session_id, state, opened_at DESC, incident_id DESC).
            self.conn
                .query_row(
                    "SELECT incident_id FROM master_no_idle_capacity_incidents
                     WHERE controller_session_id=?1 AND state='open'
                     ORDER BY opened_at DESC,incident_id DESC LIMIT 1",
                    [lead.id.to_string()],
                    |r| r.get::<_, String>(0),
                )
                .optional()?
        } else {
            None
        };
        if matches!(
            lead.status,
            SessionStatus::Failed
                | SessionStatus::Interrupted
                | SessionStatus::Archived
                | SessionStatus::Deleted
        ) {
            // A retry owner is evidence, never a reason to drop the signal:
            // its wake may be disabled or the incident stale.
            let mut evidence = vec![json!(lead.id), json!(lead.status), json!(lead.updated_at)];
            evidence.extend(owner.iter().map(|id| json!(id)));
            row.stuck("lead_unavailable", &evidence);
        }
        Ok(owner)
    }

    /// `lead_rotation_refused`: the lead's newest rotation outcome is a
    /// `refused:<code>` event (so no later `completed` rotation follows it).
    fn health_rotation_refused(&self, row: &mut Row<'_>) -> Result<Option<Value>> {
        let Some(lead) = row.lead_id else {
            return Ok(None);
        };
        // idx_rotation_events_session (session_id, rowid = id), newest
        // HEALTH_ROTATION_EVENT_WINDOW rows of the lead only.
        let Some((rotation, event_type, at)) = self
            .conn
            .query_row(
                "SELECT rotation_id,event_type,created_at FROM (
                     SELECT id,rotation_id,event_type,created_at FROM rotation_events
                     WHERE session_id=?1 ORDER BY id DESC LIMIT ?2)
                 WHERE event_type IN ('completed','suppressed_final_handoff')
                    OR event_type LIKE 'refused:%'
                 ORDER BY id DESC LIMIT 1",
                params![lead.to_string(), HEALTH_ROTATION_EVENT_WINDOW],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?
        else {
            return Ok(None);
        };
        let Some(code) = event_type.strip_prefix("refused:") else {
            return Ok(None);
        };
        row.stuck(
            "lead_rotation_refused",
            &[json!(rotation), json!(code), json!(at)],
        );
        Ok(Some(json!({"rotation_id":rotation,"code":code,"at":at})))
    }

    /// Latest K10d `delivery_abandoned` fact on the lead; `lead_delivery_abandoned`
    /// when no provider output follows it.
    fn health_delivery_abandoned(&self, row: &mut Row<'_>) -> Result<Option<Value>> {
        let Some(lead) = row.lead_id else {
            return Ok(None);
        };
        // idx_events_session_sequence (session_id, sequence), newest
        // HEALTH_FACT_EVENT_WINDOW events of the lead only.
        let Some((sequence, created, metadata)) = self
            .conn
            .query_row(
                "SELECT sequence,created_at,metadata FROM (
                     SELECT sequence,created_at,metadata,event_type FROM conversation_events
                     WHERE session_id=?1 ORDER BY sequence DESC LIMIT ?2)
                 WHERE event_type='System'
                   AND (CASE WHEN json_valid(metadata)
                        THEN json_extract(metadata,'$.health_fact') END)='delivery_abandoned'
                 ORDER BY sequence DESC LIMIT 1",
                params![lead.to_string(), HEALTH_FACT_EVENT_WINDOW],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?
        else {
            return Ok(None);
        };
        let metadata: Value = serde_json::from_str(&metadata).unwrap_or(Value::Null);
        // Provider output after the fact (same index, forward from it).
        let answered: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM conversation_events WHERE session_id=?1 AND sequence>?2
               AND (role='Assistant' OR event_type='Thinking'))",
            params![lead.to_string(), sequence],
            |r| r.get(0),
        )?;
        if !answered {
            row.stuck("lead_delivery_abandoned", &[metadata["job_id"].clone()]);
        }
        Ok(Some(json!({"job_id":metadata["job_id"],
            "watched_session_id":metadata["watched_session_id"],
            "at":metadata.get("abandoned_at").filter(|v| v.is_string()).cloned()
                .unwrap_or_else(|| json!(created)),
            "event_sequence":sequence})))
    }
}
