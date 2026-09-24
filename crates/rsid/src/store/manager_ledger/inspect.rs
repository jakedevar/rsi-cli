use super::*;
use crate::error::DaemonError;
use crate::store::harness_manager_v2::{
    BOOKKEEPING_LIMIT, COORDINATION_BUDGET_COUNT_SQL, LIVE_BOOKKEEPING_COUNTS_SQL,
    REQUEST_MARKER_KINDS, bookkeeping_class, bookkeeping_class_limit, live_class_count_sql,
};
use rsi_common::types::SessionStatus;
use std::collections::{BTreeMap, HashSet, VecDeque};

#[path = "health.rs"]
pub(super) mod health;
#[path = "paging.rs"]
mod paging;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    scope: i64,
    anchor: Uuid,
    section: ManagerInspectSectionV2,
    epic: Option<Uuid>,
    revision: i64,
    graph_stamp: Option<String>,
    after: String,
}

impl Store {
    pub(crate) fn manager_v2_inspect(
        &self,
        caller: Uuid,
        query: &AgentManagerInspectRequestV2,
    ) -> Result<ManagerInspectionV2> {
        let (config, is_manager) = self.manager_config_for_caller(caller)?;
        let mut q = query.clone();
        if !is_manager {
            if q.section == ManagerInspectSectionV2::Archive {
                return Err(refused("manager_v2_scope_denied"));
            }
            let epic = self
                .get_session(caller)?
                .and_then(|s| s.parent_id)
                .ok_or_else(|| refused("manager_v2_scope_denied"))?;
            if q.epic_id.is_some_and(|id| id != epic) {
                return Err(refused("manager_v2_epic_out_of_scope"));
            }
            q.epic_id = Some(epic);
        }
        let mut result = self.manager_v2_inspect_config(&config, &q)?;
        if is_manager
            && q.section == ManagerInspectSectionV2::Overview
            && let Some(project) = self.get_project(config.project_id)?
        {
            let project = json!({"id":project.id,"name":project.name,"path":project.path,
                "description":project.description});
            for row in &mut result.rows {
                if row["type"] == "overview" {
                    row["project"] = project.clone();
                }
            }
        }
        if !is_manager {
            if let Some(p) = &mut result.policy {
                p.policy.group_ids.clear();
                p.policy.paused_epic_ids.retain(|id| Some(*id) == q.epic_id);
            }
        }
        if !is_manager {
            for row in &mut result.rows {
                if row["type"] == "resource_policy" {
                    row["policy"] = json!(result.policy.as_ref().map(|p| &p.policy));
                }
            }
        }
        Ok(result)
    }
    pub(crate) fn manager_v2_inspect_operator(
        &self,
        project: Uuid,
        query: &AgentManagerInspectRequestV2,
    ) -> Result<ManagerInspectionV2> {
        let config = self
            .get_harness_manager(project)?
            .ok_or_else(|| refused("manager_not_configured"))?;
        self.manager_v2_inspect_config(&config, query)
    }
    fn manager_v2_inspect_config(
        &self,
        config: &HarnessManagerConfigV1,
        q: &AgentManagerInspectRequestV2,
    ) -> Result<ManagerInspectionV2> {
        q.validate().map_err(refused)?;
        if let Some(epic) = q.epic_id
            && !(config.epic_ids.contains(&epic)
                || q.section == ManagerInspectSectionV2::Archive
                    && self.manager_v2_archive_epic_in_scope(config, epic)?)
        {
            return Err(refused("manager_v2_epic_out_of_scope"));
        }
        // Question publication is authoritative; refreshing its bounded projection
        // makes the decision board useful before the periodic reconciler runs.
        if q.cursor.is_none()
            && matches!(
                q.section,
                ManagerInspectSectionV2::Decisions | ManagerInspectSectionV2::Overview
            )
        {
            self.manager_v2_refresh_question_decisions(config)?;
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        let policy = self.get_harness_manager_policy(config.project_id)?;
        let revision:i64=self.conn.query_row("SELECT COALESCE(MAX(sequence),0) FROM harness_manager_v2_events WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3 AND kind != 'decision_retrieval'",params![config.project_id.to_string(),config.manager_session_id.to_string(),config.row_version],|r|r.get(0))?;
        let leaf_scope = if matches!(
            q.section,
            ManagerInspectSectionV2::Workers | ManagerInspectSectionV2::Topology
        ) {
            Some(self.manager_v2_leaf_scope(config, policy.as_ref(), q.epic_id)?)
        } else {
            None
        };
        let graph_stamp = if let Some(scope) = &leaf_scope {
            Some(scope.stamp.clone())
        } else if matches!(
            q.section,
            ManagerInspectSectionV2::Workers
                | ManagerInspectSectionV2::Topology
                | ManagerInspectSectionV2::Resources
        ) {
            use sha2::{Digest, Sha256};
            let (ids, complete) = self.manager_v2_graph(config, q.epic_id)?;
            let mut hash = Sha256::new();
            hash.update([u8::from(complete)]);
            for (id, epic) in &ids {
                hash.update(id.as_bytes());
                hash.update(epic.as_bytes());
                let facts: (Option<String>, Option<String>, String) = self.conn.query_row(
                    "SELECT parent_id,continued_from,session_kind FROM sessions WHERE id=?1",
                    [id.to_string()],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )?;
                hash.update(serde_json::to_vec(&facts)?);
            }
            let owners = serde_json::to_string(&ids.iter().map(|(id, _)| id).collect::<Vec<_>>())?;
            let mut stmt = self.conn.prepare("SELECT child_session_id,state,updated_at FROM agent_spawn_requests WHERE owner_session_id IN (SELECT value FROM json_each(?1)) AND NOT EXISTS(SELECT 1 FROM sessions s WHERE s.id=child_session_id) ORDER BY child_session_id LIMIT ?2")?;
            for facts in stmt.query_map(params![owners, GRAPH_BUDGET as i64 + 1], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })? {
                hash.update(serde_json::to_vec(&facts?)?);
            }
            if q.section == ManagerInspectSectionV2::Topology {
                for row in self.manager_v2_topology_roots(config, policy.as_ref(), q.epic_id)? {
                    hash.update(serde_json::to_vec(&row)?);
                }
            }
            Some(format!("sha256:{:x}", hash.finalize()))
        } else if q.section == ManagerInspectSectionV2::Decisions {
            Some(fingerprint(
                &json!({"decision_keyset":1,"policy":policy.as_ref().map(|p|(p.row_version,p.revoked))}),
            )?)
        } else if q.section == ManagerInspectSectionV2::Requests {
            let sequence: i64 = self.conn.query_row("SELECT COALESCE(MAX(sequence),0) FROM harness_manager_messages WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3 AND (?4 IS NULL OR epic_id=?4)",params![config.project_id.to_string(),config.manager_session_id.to_string(),config.row_version,q.epic_id.map(|id|id.to_string())],|r|r.get(0))?;
            let mut leads = Vec::new();
            for epic in config
                .epic_ids
                .iter()
                .filter(|id| q.epic_id.is_none_or(|e| e == **id))
            {
                let lead = self.manager_lead(config.project_id, *epic).ok();
                leads.push(json!({"epic":epic,"lead":lead.as_ref().map(|s|s.id),"updated":lead.as_ref().map(|s|s.updated_at)}));
            }
            Some(fingerprint(
                &json!({"mail_sequence":sequence,"leads":leads}),
            )?)
        } else {
            None
        };
        let after = if let Some(raw) = &q.cursor {
            let c: Cursor =
                serde_json::from_str(raw).map_err(|_| refused("manager_v2_invalid_cursor"))?;
            if c.scope != config.row_version
                || c.anchor != config.manager_session_id
                || c.section != q.section
                || c.epic != q.epic_id
                || c.graph_stamp != graph_stamp
                || (!matches!(
                    q.section,
                    ManagerInspectSectionV2::Events
                        | ManagerInspectSectionV2::Decisions
                        | ManagerInspectSectionV2::Health
                ) && leaf_scope.is_none()
                    && c.revision != revision)
            {
                return Err(refused("manager_v2_cursor_changed"));
            }
            c.after
        } else {
            String::new()
        };
        let mut coverage = true;
        let mut page_after = None;
        let mut rows = match q.section {
            ManagerInspectSectionV2::Work => self.manager_v2_work_rows(config, q.epic_id)?,
            ManagerInspectSectionV2::Archive => {
                self.manager_v2_archive_rows(config, q.epic_id, &after, q.limit.saturating_add(1))?
            }
            ManagerInspectSectionV2::Health => {
                // Epic keyset: one row per scoped Epic. A row whose bounded
                // aggregate reached its bound is not complete traversal.
                let rows = self.manager_v2_health_rows(
                    config,
                    q.epic_id,
                    &after,
                    q.limit.saturating_add(1),
                )?;
                coverage = rows
                    .iter()
                    .take(usize::from(q.limit))
                    .all(|row| row["complete"] == true);
                rows
            }
            ManagerInspectSectionV2::Overview => {
                let work = self.manager_v2_work_rows(config, q.epic_id)?;
                let mut out = Vec::new();
                let unplanned_epics: Vec<_> = config
                    .epic_ids
                    .iter()
                    .filter(|id| q.epic_id.is_none_or(|epic| epic == **id))
                    .filter(|id| !work.iter().any(|w| w["epic_id"] == id.to_string()))
                    .copied()
                    .collect();
                // #669: durable seat observation beside manager_available.
                let manager_seat = self.manager_seat_state(config)?;
                for kind in ["program", "product"] {
                    let ws: Vec<_> = work.iter().filter(|r| r["kind"] == kind).collect();
                    let accepted = ws.iter().filter(|r| r["source_accepted"] == true).count();
                    let integrated = ws.iter().filter(|r| r["integrated"] == true).count();
                    let weight: u64 = ws.iter().map(|r| r["weight"].as_u64().unwrap_or(0)).sum();
                    let accepted_weight: u64 = ws
                        .iter()
                        .filter(|r| r["source_accepted"] == true)
                        .map(|r| r["weight"].as_u64().unwrap_or(0))
                        .sum();
                    let integrated_weight: u64 = ws
                        .iter()
                        .filter(|r| r["integrated"] == true)
                        .map(|r| r["weight"].as_u64().unwrap_or(0))
                        .sum();
                    out.push(json!({"type":"overview","key":kind,"kind":kind,"scope_revision":revision,"denominator":ws.len(),"accepted":accepted,"integrated":integrated,"weight_denominator":weight,"accepted_weight":accepted_weight,"integrated_weight":integrated_weight,"unknown":ws.iter().filter(|r|r["evidence_state"]=="unknown").count(),"partial":ws.iter().filter(|r|r["source_accepted"]!=true && !r["source_commit"].is_null()).count(),"ready":ws.iter().filter(|r|r["ready"]==true).count(),"missing_work_scope":ws.is_empty()||!unplanned_epics.is_empty(),"unplanned_epics":unplanned_epics,"manager_available":config.current_session_id.is_some(),"manager_seat":manager_seat}));
                }
                for kind in ["intent", "coordinator_error"] {
                    for record in self.manager_v2_records(config, kind)? {
                        if q.epic_id.is_none_or(|epic| record.epic_id == Some(epic)) {
                            let mut row = record_row(record);
                            // Keys remain unique across the mixed overview page.
                            row["key"] = json!(format!(
                                "{kind}:{}",
                                row["key"].as_str().unwrap_or_default()
                            ));
                            out.push(row);
                        }
                    }
                }
                for epic in config
                    .epic_ids
                    .iter()
                    .filter(|id| q.epic_id.is_none_or(|e| e == **id))
                {
                    let session = self.get_session(*epic)?;
                    out.push(json!({"type":"lead_control", "key":format!("lead:{epic}"), "epic_id":epic,
                        "title":session.as_ref().and_then(|s|s.title.as_ref()),
                        "expected":self.manager_action_lead_fence(*epic).ok(),
                        "fence_state":if self.manager_action_lead_fence(*epic).is_ok() {"current"} else {"unavailable"}}));
                }
                if q.epic_id.is_none() {
                    let scope = params![
                        config.project_id.to_string(),
                        config.manager_session_id.to_string(),
                        config.row_version
                    ];
                    let coordination: i64 =
                        self.conn
                            .query_row(COORDINATION_BUDGET_COUNT_SQL, scope, |r| r.get(0))?;
                    let (retrieval, resource, lifecycle): (i64, i64, i64) =
                        self.conn
                            .query_row(LIVE_BOOKKEEPING_COUNTS_SQL, scope, |r| {
                                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                            })?;
                    let mut budget = json!({"type":"record_budget","key":"record_budget",
                        "coordination":{"used":coordination,"limit":MANAGER_V2_MAX_RECORDS},
                        "retrieval":{"used":retrieval,"limit":BOOKKEEPING_LIMIT},
                        "resource":{"used":resource,"limit":BOOKKEEPING_LIMIT},
                        "lifecycle":{"used":lifecycle,"limit":BOOKKEEPING_LIMIT}});
                    // #664: each request-marker class, keyed by its kind; an
                    // unmetered class displays `limit: null`.
                    for kind in REQUEST_MARKER_KINDS {
                        let (predicate, _) = bookkeeping_class(kind)
                            .ok_or_else(|| refused("manager_v2_record_class"))?;
                        let used: i64 =
                            self.conn
                                .query_row(&live_class_count_sql(predicate), scope, |r| r.get(0))?;
                        budget[kind] = json!({"used":used,"limit":bookkeeping_class_limit(kind)});
                    }
                    out.push(budget);
                    // #664 (f): the same open counts that gate `manager_send`.
                    // Read-only: orphans are settled only by send/Progress.
                    let (capacity, per_epic) = self.manager_mail_capacity(config)?;
                    let mut row = serde_json::to_value(&capacity)?;
                    row["type"] = json!("mail_capacity");
                    row["key"] = json!("mail_capacity");
                    row["warning"] = json!(capacity.warning);
                    row["per_epic"] = json!(
                        per_epic
                            .iter()
                            .map(|(epic_id, pending)| json!({"epic_id":epic_id,"pending":pending}))
                            .collect::<Vec<_>>()
                    );
                    out.push(row);
                    let mut row = serde_json::to_value(self.manager_succession_preflight(config)?)?;
                    row["type"] = json!("manager_control");
                    row["key"] = json!(format!("manager_control:{}", config.manager_session_id));
                    if let Some(current) = config.current_session_id {
                        row["title"] = json!(self.get_session(current)?.and_then(|s| s.title));
                    }
                    out.push(row);
                    if let Some(r) = self.manager_v2_record(config, "handoff", "current")? {
                        out.push(record_row(r));
                    }
                }
                out
            }
            ManagerInspectSectionV2::Workers | ManagerInspectSectionV2::Topology => {
                let page = self.manager_v2_leaf_page(
                    config,
                    leaf_scope
                        .as_ref()
                        .ok_or_else(|| refused("manager_v2_scope_unavailable"))?,
                    q,
                    &after,
                )?;
                coverage = page.coverage;
                page_after = Some(page.next_after);
                page.rows
            }
            ManagerInspectSectionV2::Requests => self.manager_v2_request_rows(
                config,
                q.epic_id,
                &after,
                usize::from(q.limit) + 1,
                false,
            )?,
            ManagerInspectSectionV2::Decisions => {
                let mut out = Vec::new();
                for record in self.manager_v2_record_page(
                    config,
                    "decision",
                    q.epic_id,
                    &after,
                    usize::from(q.limit) + 1,
                )? {
                    if q.epic_id.is_none_or(|e| record.epic_id == Some(e)) {
                        let mut row = record_row(record);
                        if let Some(target) = self.manager_v2_record(
                            config,
                            "decision_target",
                            row["key"].as_str().unwrap_or_default(),
                        )? {
                            row["session_id"] = target.payload["session_id"].clone();
                        }
                        self.manager_v2_project_decision_retrieval(config, &mut row)?;
                        out.push(row);
                    }
                }
                let (legacy, scan_after, scan_complete) =
                    self.manager_v2_approval_gate_rows(config, q.epic_id, &after)?;
                coverage = scan_complete;
                out.extend(legacy);
                // A legacy or malformed question is still a visible human gate.
                for record in self.manager_v2_records(config, "intent")? {
                    if record.key.starts_with("question:")
                        && record.payload["state"] == "blocked"
                        && q.epic_id.is_none_or(|e| record.epic_id == Some(e))
                    {
                        let mut row = record_row(record);
                        row["key"] = json!(format!(
                            "unresolved:{}",
                            row["key"].as_str().unwrap_or_default()
                        ));
                        row["next_action"] = json!(
                            "Open the exact session question; its provenance is unavailable to this inbox."
                        );
                        out.push(row);
                    }
                }
                // A filtered native mirror still advances legacy scan work.
                // Stop all merged streams at that frontier so no decision or
                // eligible legacy identity can be skipped by the shared cursor.
                out.retain(|r| {
                    r["key"].as_str().is_some_and(|key| {
                        key > after.as_str() && scan_after.as_deref().is_none_or(|end| key <= end)
                    })
                });
                out.sort_by(|a, b| a["key"].as_str().cmp(&b["key"].as_str()));
                let next = if out.len() > usize::from(q.limit) {
                    out[usize::from(q.limit) - 1]["key"]
                        .as_str()
                        .map(str::to_owned)
                } else {
                    scan_after
                };
                page_after = Some(next);
                out
            }
            ManagerInspectSectionV2::Resources => {
                if q.epic_id.is_none() {
                    match self.manager_v2_resource_snapshot(config) {
                        Ok(row) => vec![row],
                        Err(error) => {
                            coverage = false;
                            vec![
                                json!({"type":"resources","key":"cohort","state":"unknown","reason":error.to_string(),"cohort_complete":false}),
                            ]
                        }
                    }
                } else {
                    // The shared manager budget does not grant a feature lead
                    // visibility into other Epics or manager-created Group work.
                    vec![
                        json!({"type":"resource_policy","key":"policy","epic_id":q.epic_id,
                        "policy":policy.as_ref().map(|p| &p.policy),"accounting_scope":"shared_manager_cohort",
                        "usage_state":"manager_aggregate","next_action":"The appointed manager inspects Resources without an Epic filter for shared admission and spend evidence."}),
                    ]
                }
            }
            ManagerInspectSectionV2::Actions => {
                let owners = if let Some(epic) = q.epic_id {
                    let (ids, complete) = self.manager_v2_graph(config, Some(epic))?;
                    coverage &= complete;
                    serde_json::to_string(&ids.into_iter().map(|(id, _)| id).collect::<Vec<_>>())?
                } else {
                    "[]".into()
                };
                let mut stmt=self.conn.prepare("SELECT o.id,o.state,o.row_version,o.target_session_id,o.kind,o.payload_json,o.attempts,o.not_before,o.outcome_json FROM harness_manager_v2_operations o WHERE o.project_id=?1 AND o.manager_session_id=?2 AND o.scope_version=?3 AND o.id>?4 AND (?6 IS NULL OR o.target_session_id IN (SELECT value FROM json_each(?7)) OR json_extract(o.payload_json,'$.request.operation.epic_id')=?6 OR EXISTS(SELECT 1 FROM harness_manager_v2_records context WHERE context.project_id=o.project_id AND context.manager_session_id=o.manager_session_id AND context.scope_version=o.scope_version AND context.kind='lifecycle_context' AND context.record_key=o.id AND context.epic_id=?6) OR json_extract(o.payload_json,'$.request.change.epic_id')=?6 OR EXISTS(SELECT 1 FROM harness_manager_v2_records r WHERE r.project_id=o.project_id AND r.manager_session_id=o.manager_session_id AND r.scope_version=o.scope_version AND r.epic_id=?6 AND r.kind='decision' AND r.record_key=json_extract(o.payload_json,'$.request.change.key')) OR EXISTS(SELECT 1 FROM harness_manager_v2_work_facts f WHERE f.project_id=o.project_id AND f.kind='work' AND f.record_key=json_extract(o.payload_json,'$.request.change.key') AND f.epic_id=?6) OR EXISTS(SELECT 1 FROM harness_manager_messages m WHERE m.project_id=o.project_id AND m.manager_session_id=o.manager_session_id AND m.scope_version=o.scope_version AND m.epic_id=?6 AND m.id=json_extract(o.payload_json,'$.request.change.request_id'))) ORDER BY o.id LIMIT ?5")?;
                let raw = stmt
                    .query_map(
                        params![
                            config.project_id.to_string(),
                            config.manager_session_id.to_string(),
                            config.row_version,
                            after,
                            i64::from(q.limit) + 1,
                            q.epic_id.map(|id| id.to_string()),
                            owners
                        ],
                        |r| {
                            Ok((
                                r.get::<_, String>(0)?,
                                r.get::<_, String>(1)?,
                                r.get::<_, i64>(2)?,
                                r.get::<_, Option<String>>(3)?,
                                r.get::<_, String>(4)?,
                                r.get::<_, String>(5)?,
                                r.get::<_, i64>(6)?,
                                r.get::<_, String>(7)?,
                                r.get::<_, Option<String>>(8)?,
                            ))
                        },
                    )?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let mut out = Vec::new();
                for (id, state, version, target, kind, payload, attempts, due, outcome) in
                    raw.into_iter().take(MANAGER_V2_MAX_RECORDS)
                {
                    let payload: Value = serde_json::from_str(&payload)?;
                    let operation = payload
                        .pointer("/request/operation")
                        .or_else(|| payload.pointer("/request/change"))
                        .cloned();
                    let outcome = outcome
                        .map(|raw| serde_json::from_str::<Value>(&raw))
                        .transpose()?;
                    let mut row = json!({"type":"action","key":id,"id":id,"state":state,"row_version":version,"target_session_id":target,"kind":kind.clone(),"operation":operation.clone(),"attempts":attempts,"not_before":due,"outcome":outcome.clone()});
                    if kind == "lifecycle_action" {
                        if let (Some(operation), Some(outcome)) =
                            (operation.as_ref(), outcome.as_ref())
                        {
                            if let (Ok(action), Ok(mut receipt)) = (
                                serde_json::from_value::<ManagerActionV2>(operation.clone()),
                                serde_json::from_value::<ManagerActionReceiptV2>(outcome.clone()),
                            ) {
                                receipt.refresh_action_metadata(&action);
                                row["action_kind"] = serde_json::to_value(receipt.action_kind)?;
                                row["target_type"] = serde_json::to_value(receipt.target_type)?;
                                row["receipt_state"] = serde_json::to_value(receipt.state)?;
                                row["result"] = serde_json::to_value(&receipt.result)?;
                                // Preserve the legacy `outcome` field and shape while
                                // making historical receipts carry current metadata.
                                row["outcome"] = serde_json::to_value(receipt)?;
                            }
                        }
                    }
                    out.push(row);
                }
                out
            }
            ManagerInspectSectionV2::Events => {
                let sequence = after.parse::<i64>().unwrap_or(0);
                let mut stmt=self.conn.prepare("SELECT e.sequence,e.kind,e.record_key,e.row_version,e.payload_json,e.created_at FROM harness_manager_v2_events e WHERE e.project_id=?1 AND e.manager_session_id=?2 AND e.scope_version=?3 AND e.sequence>?4 AND (?5 IS NULL OR EXISTS(SELECT 1 FROM harness_manager_v2_records r WHERE r.project_id=e.project_id AND r.manager_session_id=e.manager_session_id AND r.scope_version=e.scope_version AND r.kind=e.kind AND r.record_key=e.record_key AND r.epic_id=?5) OR EXISTS(SELECT 1 FROM harness_manager_v2_work_facts f WHERE f.project_id=e.project_id AND f.kind=e.kind AND f.record_key=e.record_key AND f.epic_id=?5)) ORDER BY e.sequence LIMIT ?6")?;
                stmt.query_map(params![config.project_id.to_string(),config.manager_session_id.to_string(),config.row_version,sequence,q.epic_id.map(|id|id.to_string()),i64::from(q.limit)+1],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,i64>(3)?,r.get::<_,String>(4)?,r.get::<_,String>(5)?)))?.map(|r|{let(seq,kind,key,version,payload,created)=r?;Ok(json!({"type":"event","key":format!("{seq:020}"),"sequence":seq,"kind":kind,"record_key":key,"row_version":version,"payload":serde_json::from_str::<Value>(&payload)?,"created_at":created}))}).collect::<Result<Vec<_>>>()?
            }
        };
        if page_after.is_none() {
            rows.retain(|r| r["key"].as_str().is_some_and(|k| k > after.as_str()));
            rows.sort_by(|a, b| a["key"].as_str().cmp(&b["key"].as_str()));
        }
        let more = page_after.as_ref().map_or(
            rows.len() > usize::from(q.limit),
            |next: &Option<String>| next.is_some(),
        );
        rows.truncate(usize::from(q.limit));
        if matches!(
            q.section,
            ManagerInspectSectionV2::Workers | ManagerInspectSectionV2::Topology
        ) {
            for row in &mut rows {
                row["graph_coverage"] = json!(if coverage {
                    "complete"
                } else {
                    "bounded_partial"
                });
            }
        }
        let next_cursor = if more {
            page_after
                .flatten()
                .or_else(|| {
                    rows.last()
                        .and_then(|r| r["key"].as_str().map(str::to_owned))
                })
                .map(|after| {
                    serde_json::to_string(&Cursor {
                        scope: config.row_version,
                        anchor: config.manager_session_id,
                        section: q.section,
                        epic: q.epic_id,
                        revision,
                        graph_stamp: graph_stamp.clone(),
                        after,
                    })
                })
                .transpose()?
        } else {
            None
        };
        tx.commit()?;
        Ok(ManagerInspectionV2 {
            observed_at: Utc::now(),
            scope_version: config.row_version,
            policy,
            section: q.section,
            rows,
            next_cursor,
            complete: coverage && !more,
        })
    }
    pub(crate) fn manager_v2_work_rows(
        &self,
        config: &HarnessManagerConfigV1,
        epic: Option<Uuid>,
    ) -> Result<Vec<Value>> {
        let mut rows = Vec::new();
        let mut records = self.manager_v2_records(config, "work")?;
        if records.len() > MANAGER_V2_MAX_WORK {
            return Err(refused("manager_v2_work_limit"));
        }
        // Live work plus the most recently delivered work, within the same
        // bound: history stays visible without growing the page without limit.
        let history =
            self.manager_v2_terminal_work_keys(config, MANAGER_V2_MAX_WORK - records.len())?;
        let project = config.project_id;
        records.extend(self.manager_v2_facts_of_works(project, "work", &history)?);
        records.sort_by(|a, b| a.key.cmp(&b.key));
        let mut dependencies = self.manager_v2_records(config, "dependency")?;
        dependencies.extend(self.manager_v2_facts_of_works(project, "dependency", &history)?);
        let mut ownership = self.manager_v2_records(config, "ownership")?;
        ownership.extend(self.manager_v2_facts_of_works(project, "ownership", &history)?);
        let mut migrations = self.manager_v2_records(config, "migration")?;
        migrations.extend(self.manager_v2_facts_of_works(project, "migration", &history)?);
        for record in records {
            let w: WorkRecord = decode(&record)?;
            if epic.is_some_and(|e| e != w.epic_id) || !config.epic_ids.contains(&w.epic_id) {
                continue;
            }
            let blockers = self.manager_v2_dependency_blockers(config, &w.key)?;
            let acceptance = w
                .source_commit
                .as_deref()
                .map(|source| self.manager_v2_accepted_source(config, &w, source))
                .transpose()?
                .flatten();
            let acceptance_recorded = acceptance.is_some();
            let accepted = acceptance_recorded && blockers.is_empty();
            let integrated = accepted
                && w.integration
                    .as_ref()
                    .is_some_and(|i| Some(&i.source_commit) == w.source_commit.as_ref());
            let mut value = record_row(record);
            let review = self.manager_review_projection(config, &w)?;
            let db_review = review["mode"] == "db_native";
            value["review"] = review;
            value["review_scope"] = json!(scope_label(&w));
            value["required_evidence_linkage"] = if db_review {
                Value::Null
            } else {
                json!(
                    w.required_gates
                        .iter()
                        .map(|stage| serde_json::to_value(stage).map(|s| format!(
                            "{}:{}",
                            scope_label(&w),
                            s.as_str().unwrap_or("unknown")
                        )))
                        .collect::<std::result::Result<Vec<_>, _>>()?
                )
            };
            value["evidence_policy_digest"] = if db_review {
                Value::Null
            } else {
                json!(
                    w.source_commit
                        .as_ref()
                        .map(|head| policy_digest(&w, head))
                        .transpose()?
                )
            };
            value["dependencies"] = json!(
                dependencies
                    .iter()
                    .filter(|r| r.payload["work_key"] == w.key)
                    .cloned()
                    .map(record_row)
                    .collect::<Vec<_>>()
            );
            value["ownership"] = json!(
                ownership
                    .iter()
                    .filter(|r| r.payload["work_key"] == w.key)
                    .cloned()
                    .map(record_row)
                    .collect::<Vec<_>>()
            );
            value["migration_reservations"] = json!(
                migrations
                    .iter()
                    .filter(|r| r.payload["work_key"] == w.key)
                    .cloned()
                    .map(|r| {
                        let mut row = record_row(r);
                        row["consumed"] = json!(
                            row["version"]
                                .as_u64()
                                .is_some_and(|v| v <= crate::store::LATEST_SCHEMA_VERSION as u64)
                        );
                        row
                    })
                    .collect::<Vec<_>>()
            );
            value["released_schema_head"] = json!(crate::store::LATEST_SCHEMA_VERSION);
            value["source_acceptance_recorded"] = json!(acceptance_recorded);
            value["evidence_state"] = json!(if db_review {
                if acceptance_recorded {
                    "review_receipt_accepted"
                } else {
                    "review_receipt_pending"
                }
            } else if w
                .required_gates
                .iter()
                .filter(|g| **g != ManagerWorkStageV2::Integration)
                .all(|g| w
                    .stages
                    .iter()
                    .any(|s| s.stage == *g && s.admission.is_some()))
            {
                "admitted"
            } else {
                "unknown"
            });
            value["source_accepted"] = json!(accepted);
            value["integrated"] = json!(integrated);
            value["dependency_blockers"] = json!(blockers);
            let mut blockers = blockers;
            if !accepted {
                for s in &w.stages {
                    if matches!(
                        s.state,
                        ManagerStageStateV2::Blocked | ManagerStageStateV2::Failed
                    ) {
                        blockers.push(format!(
                            "stage:{}",
                            serde_json::to_value(s.stage)?.as_str().unwrap_or("unknown")
                        ));
                    }
                }
            }
            if let Some(decision) = &w.pending_acceptance {
                blockers.push(format!("decision:{decision}"));
            }
            value["ready"] = json!(blockers.is_empty() && !integrated);
            value["integration_ready"] = json!(accepted && !integrated && blockers.is_empty());
            value["blockers"] = json!(blockers);
            value["integration_order"] = json!([w.priority, w.spec_revision]);
            value["target_freshness"] = json!(if integrated {
                "observation_required"
            } else {
                "unknown"
            });
            rows.push(value);
        }
        rows.sort_by_key(|v| {
            (
                v["ready"] != true,
                v["source_accepted"] != true,
                v["priority"].as_u64().unwrap_or(9),
                v["key"].as_str().unwrap_or_default().to_string(),
            )
        });
        for (rank, row) in rows.iter_mut().enumerate() {
            row["integration_rank"] = json!(rank);
        }
        Ok(rows)
    }
    fn manager_v2_approval_gate_rows(
        &self,
        config: &HarnessManagerConfigV1,
        epic: Option<Uuid>,
        after: &str,
    ) -> Result<(Vec<Value>, Option<String>, bool)> {
        let (graph, complete) = self.manager_v2_graph(config, epic)?;
        let lower = if after < "approval:" {
            Some("")
        } else {
            after.strip_prefix("approval:")
        };
        let (ids, next) = if let Some(lower) = lower {
            self.manager_v2_legacy_approval_candidates(
                &graph.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
                lower,
            )?
        } else {
            (Vec::new(), None)
        };
        let mut rows = Vec::new();
        for id in ids {
            let Some((session_id, tool, created)) = self.manager_v2_legacy_approval_row(&id)?
            else {
                continue;
            };
            let Ok(live_epic) = self.manager_v2_live_epic_for_session(config, session_id) else {
                continue;
            };
            rows.push(json!({"type":"operator_gate","key":format!("approval:{id}"),"approval_id":id,"session_id":session_id,"epic_id":live_epic,
                "question":format!("Pending tool approval: {}",bounded(&tool,256)),"state":"blocked","gate_state":"pending",
                "route_state":"unavailable","created_at":created,"next_action":"Open the exact session to inspect its provider approval. This legacy approval record has no verified reply channel; it remains pending."}));
        }
        if !complete && after < "~approval_coverage" {
            rows.push(json!({"type":"coverage","key":"~approval_coverage","state":"unknown","next_action":"Inspect one Epic to cover its pending approval gates."}));
        }
        Ok((rows, next.map(|id| format!("approval:{id}")), complete))
    }

    fn manager_v2_topology_roots(
        &self,
        config: &HarnessManagerConfigV1,
        policy: Option<&HarnessManagerPolicyConfigV2>,
        epic_filter: Option<Uuid>,
    ) -> Result<Vec<Value>> {
        use rsi_common::types::SessionKind;
        let mut ids = std::collections::BTreeSet::new();
        for epic in config
            .epic_ids
            .iter()
            .filter(|id| epic_filter.is_none_or(|e| e == **id))
        {
            ids.insert(*epic);
            if let Some(parent) = self.get_session(*epic)?.and_then(|s| s.parent_id) {
                ids.insert(parent);
            }
        }
        if epic_filter.is_none() {
            ids.extend(&config.group_ids);
            if config.scope_mode == rsi_common::harness_manager::HarnessManagerScopeModeV1::Project
            {
                let mut groups = self.conn.prepare("SELECT id FROM sessions WHERE project_id=?1 AND session_kind='Group' AND parent_id IS NULL AND status NOT IN ('Archived','Deleted') ORDER BY id")?;
                for id in groups.query_map([config.project_id.to_string()], |row| {
                    row.get::<_, String>(0)
                })? {
                    ids.insert(
                        Uuid::parse_str(&id?)
                            .map_err(|_| refused("manager_v2_invalid_stored_identity"))?,
                    );
                }
            }
            if let Some(policy) = policy.filter(|p| !p.revoked) {
                ids.extend(&policy.policy.group_ids);
            }
        }
        let mut rows = Vec::new();
        for id in ids {
            let Some(s) = self.get_session(id)?.filter(|s| {
                s.project_id == Some(config.project_id)
                    && rsi_common::is_container_kind(s.session_kind)
            }) else {
                continue;
            };
            rows.push(json!({"type":"topology","key":id,"id":id,"title":bounded(s.title.as_deref().unwrap_or(&s.query),512),
                "parent_id":s.parent_id,"kind":s.session_kind,"status":s.status,"lead_session_id":s.lead_session_id,
                "updated_at":s.updated_at,"expected_updated_at":s.updated_at,
                "expected":if s.session_kind == SessionKind::Epic { self.manager_action_lead_fence(id).ok() } else {None},
                "selected_epic":config.epic_ids.contains(&id),
                "selected_group":s.session_kind == SessionKind::Group && config.covers_group(id),
                "topology_granted":s.session_kind == SessionKind::Group && policy.is_some_and(|p|!p.revoked && (config.covers_group(id) || p.policy.group_ids.contains(&id)))}));
        }
        Ok(rows)
    }

    fn manager_v2_archive_rows(
        &self,
        config: &HarnessManagerConfigV1,
        epic_filter: Option<Uuid>,
        after: &str,
        limit: u16,
    ) -> Result<Vec<Value>> {
        let epics = serde_json::to_string(
            &config
                .epic_ids
                .iter()
                .map(Uuid::to_string)
                .collect::<Vec<_>>(),
        )?;
        let groups = serde_json::to_string(
            &config
                .group_ids
                .iter()
                .map(Uuid::to_string)
                .collect::<Vec<_>>(),
        )?;
        let project_scope =
            config.scope_mode == rsi_common::harness_manager::HarnessManagerScopeModeV1::Project;
        let mut stmt = self.conn.prepare(
            "SELECT s.id FROM sessions s
             WHERE s.project_id=?1 AND s.session_kind IN ('Group','Epic')
               AND s.status IN ('Archived','Deleted')
               AND (s.id>?2)
               AND (?3 OR s.id IN (SELECT value FROM json_each(?4))
                    OR s.id IN (SELECT value FROM json_each(?5))
                    OR (s.session_kind='Epic' AND s.parent_id IN (SELECT value FROM json_each(?4)))
                    OR EXISTS(SELECT 1 FROM harness_manager_v2_entities e
                              WHERE e.session_id=s.id AND e.project_id=?1
                                AND e.manager_session_id=?6
                                AND e.scope_version=?9
                                AND e.kind IN ('Group','Epic')))
               AND (?7 IS NULL OR s.id=?7
                    OR (s.session_kind='Group' AND s.id=(SELECT parent_id FROM sessions WHERE id=?7)))
             ORDER BY s.id LIMIT ?8",
        )?;
        let ids = stmt
            .query_map(
                params![
                    config.project_id.to_string(),
                    after,
                    project_scope,
                    groups,
                    epics,
                    config.manager_session_id.to_string(),
                    epic_filter.map(|id| id.to_string()),
                    i64::from(limit),
                    config.row_version,
                ],
                |row| row.get::<_, String>(0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut rows = Vec::with_capacity(ids.len());
        for raw_id in ids {
            let id = Uuid::parse_str(&raw_id)
                .map_err(|_| refused("manager_v2_invalid_stored_identity"))?;
            let Some(session) = self.get_session(id)? else {
                continue;
            };
            let descendants: i64 = self.conn.query_row(
                "WITH RECURSIVE descendants(id) AS (
                     SELECT id FROM sessions WHERE parent_id=?1
                     UNION
                     SELECT child.id FROM sessions child
                     JOIN descendants parent ON child.parent_id=parent.id
                 )
                 SELECT COUNT(*) FROM (SELECT id FROM descendants LIMIT 10001)",
                [id.to_string()],
                |row| row.get(0),
            )?;
            let restore_blocker = self.manager_v2_archive_restore_blocker(config, &session)?;
            rows.push(json!({
                "type":"archive", "key":id, "id":id,
                "kind":session.session_kind,
                "title":bounded(session.title.as_deref().unwrap_or(&session.query),512),
                "parent_id":session.parent_id, "status":session.status,
                "updated_at":session.updated_at,
                "expected_updated_at":session.updated_at,
                "descendant_count":descendants.min(10_000),
                "restorable":restore_blocker.is_none(),
                "restore_blocker":restore_blocker,
            }));
        }
        Ok(rows)
    }

    fn manager_v2_archive_restore_blocker(
        &self,
        config: &HarnessManagerConfigV1,
        session: &rsi_common::types::Session,
    ) -> Result<Option<String>> {
        let check = (|| {
            let grant = self
                .get_harness_manager_policy(config.project_id)?
                .ok_or_else(|| refused("manager_v2_grant_required"))?;
            let caller = config
                .current_session_id
                .ok_or_else(|| refused("manager_v2_scope_changed"))?;
            let authority = self.manager_v2_authorize(
                caller,
                &ManagerFenceV2 {
                    scope_version: config.row_version,
                    policy_version: grant.row_version,
                },
                Some(ManagerCapabilityV2::Topology),
            )?;
            self.manager_action_target(
                &authority,
                &ManagerActionV2::RestoreContainer {
                    container_id: session.id,
                    expected_updated_at: session.updated_at,
                },
                true,
            )?;
            self.manager_action_validate_restore_container(session, true)?;
            Ok(())
        })();
        match check {
            Ok(()) => Ok(None),
            Err(DaemonError::InvalidParam(code)) => Ok(Some(code)),
            Err(error) => Err(error),
        }
    }

    fn manager_v2_archive_epic_in_scope(
        &self,
        config: &HarnessManagerConfigV1,
        epic: Uuid,
    ) -> Result<bool> {
        let Some(session) = self.get_session(epic)?.filter(|session| {
            session.project_id == Some(config.project_id)
                && session.session_kind == rsi_common::types::SessionKind::Epic
        }) else {
            return Ok(false);
        };
        if config.epic_ids.contains(&epic)
            || config.explicit_epic_ids().contains(&epic)
            || config.scope_mode == rsi_common::harness_manager::HarnessManagerScopeModeV1::Project
        {
            return Ok(true);
        }
        if session
            .parent_id
            .is_some_and(|parent| config.group_ids.contains(&parent))
        {
            return Ok(true);
        }
        let created_by_manager: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM harness_manager_v2_entities
             WHERE session_id=?1 AND project_id=?2 AND manager_session_id=?3
               AND scope_version=?4 AND kind='Epic')",
            params![
                epic.to_string(),
                config.project_id.to_string(),
                config.manager_session_id.to_string(),
                config.row_version,
            ],
            |row| row.get(0),
        )?;
        Ok(created_by_manager)
    }

    pub(crate) fn manager_v2_graph(
        &self,
        config: &HarnessManagerConfigV1,
        epic: Option<Uuid>,
    ) -> Result<(Vec<(Uuid, Uuid)>, bool)> {
        let mut queue = VecDeque::new();
        let mut nodes = BTreeMap::new();
        let mut seen = HashSet::new();
        let mut complete = true;
        for id in config
            .epic_ids
            .iter()
            .filter(|id| epic.is_none_or(|e| e == **id))
        {
            if self.get_session(*id)?.is_some_and(|s| {
                s.project_id == Some(config.project_id)
                    && s.session_kind == rsi_common::types::SessionKind::Epic
            }) {
                queue.push_back((*id, *id));
            } else {
                complete = false;
            }
        }
        while let Some((id, root)) = queue.pop_front() {
            if !seen.insert(id) {
                return Err(refused("manager_v2_hierarchy_cycle"));
            }
            if nodes.len() >= GRAPH_BUDGET {
                complete = false;
                break;
            }
            nodes.insert(id, root);
            let mut stmt = self.conn.prepare(
                "SELECT id FROM sessions WHERE parent_id=?1 AND project_id=?2 ORDER BY id LIMIT ?3",
            )?;
            let children = stmt
                .query_map(
                    params![
                        id.to_string(),
                        config.project_id.to_string(),
                        (GRAPH_BUDGET - nodes.len() + 1) as i64
                    ],
                    |r| r.get::<_, String>(0),
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for child in children {
                queue.push_back((
                    Uuid::parse_str(&child)
                        .map_err(|_| refused("manager_v2_invalid_stored_identity"))?,
                    root,
                ));
            }
            if queue.len() + nodes.len() > GRAPH_BUDGET {
                complete = false;
                queue.truncate(GRAPH_BUDGET - nodes.len());
            }
        }
        Ok((nodes.into_iter().collect(), complete))
    }
    fn manager_v2_worker(
        &self,
        config: &HarnessManagerConfigV1,
        s: &rsi_common::types::Session,
        epic: Uuid,
        all_work: &[Value],
    ) -> Result<Value> {
        let mut tip = s.clone();
        let mut lineage = "proven";
        if let Some(previous) = s.continued_from {
            let transfer: bool = self.conn.query_row("SELECT EXISTS(SELECT 1 FROM sandbox_custody_events WHERE from_owner_session_id=?1 AND to_owner_session_id=?2 AND event_kind='transferred')", params![previous.to_string(), s.id.to_string()], |r|r.get(0))?;
            if !transfer || self.manager_v2_descendant_epic(config, previous).ok() != Some(epic) {
                lineage = "unproven";
            }
        }
        let mut seen = HashSet::new();
        for _ in 0..64 {
            if !seen.insert(tip.id) {
                lineage = "cycle";
                break;
            }
            let mut stmt = self
                .conn
                .prepare("SELECT id FROM sessions WHERE continued_from=?1 ORDER BY id LIMIT 2")?;
            let next = stmt
                .query_map([tip.id.to_string()], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            if next.is_empty() {
                break;
            }
            if next.len() > 1 {
                lineage = "ambiguous";
                break;
            }
            let id = Uuid::parse_str(&next[0])
                .map_err(|_| refused("manager_v2_invalid_stored_identity"))?;
            if self.manager_v2_descendant_epic(config, id).ok() != Some(epic) {
                lineage = "out_of_scope";
                break;
            }
            let transfer:bool=self.conn.query_row("SELECT EXISTS(SELECT 1 FROM sandbox_custody_events WHERE from_owner_session_id=?1 AND to_owner_session_id=?2 AND event_kind='transferred')",params![tip.id.to_string(),id.to_string()],|r|r.get(0))?;
            if !transfer {
                lineage = "unproven";
                break;
            }
            tip = self
                .get_session(id)?
                .ok_or_else(|| refused("manager_v2_session_unavailable"))?;
            if seen.len() == 64 {
                lineage = "budget_exceeded";
            }
        }
        let (sequence,last_event):(i64,Option<String>)=self.conn.query_row("SELECT COALESCE(MAX(sequence),0),MAX(created_at) FROM conversation_events WHERE session_id=?1",[tip.id.to_string()],|r|Ok((r.get(0)?,r.get(1)?)))?;
        let custody = self.live_custody_for_session(tip.id).ok();
        let work: Vec<_> = all_work
            .iter()
            .filter(|w| {
                w["source_session_id"] == tip.id.to_string()
                    || w["source_session_id"] == s.id.to_string()
            })
            .collect();
        let work_total = work.len();
        let pending_total = work.iter().filter(|w| w["integrated"] != true).count();
        let summaries:Vec<_>=work.iter().take(16).map(|w|json!({"key":w["key"],"title":w["title"],"source_commit":w["source_commit"],"source_accepted":w["source_accepted"],"integrated":w["integrated"],"blockers":w["blockers"],"stages":w["stages"].as_array().map(|stages|stages.iter().map(|s|json!({"stage":s["stage"],"state":s["state"]})).collect::<Vec<_>>())})).collect();
        let question = serde_json::to_value(&tip.pending_question)?;
        let question = if serde_json::to_vec(&question)?.len() > 8192 {
            json!({"state":"requires_session_view"})
        } else {
            question
        };
        let meaningful = work.iter().filter_map(|w| w["updated_at"].as_str()).max();
        Ok(
            json!({"type":"worker","key":s.id,"session_id":s.id,"lineage_tip_id":tip.id,"continued_from":s.continued_from,"lineage":lineage,"epic_id":epic,"parent_id":s.parent_id,"title":bounded(tip.title.as_deref().unwrap_or(&tip.query),512),"logical_title":bounded(s.title.as_deref().unwrap_or(&s.query),512),"active_task":tip.active_task.as_deref().map(|s|bounded(s,2048)),"status":tip.status,"stop_reason":tip.stop_reason,"status_updated_at":tip.updated_at,"last_event_at":last_event,"event_sequence":sequence,"last_meaningful_progress_at":meaningful,"pipeline_artifact":tip.pipeline_artifact,"pending_question":question,"branch":tip.sandbox_branch,"sandbox_root":tip.sandbox_root,"custody_id":custody.as_ref().map(|c|c.custody_id),"custody_generation":custody.as_ref().map(|c|c.generation),"base_commit":custody.as_ref().map(|c|&c.source_commit),"custody_state":if custody.is_some(){"verified"}else{"unknown"},"work":summaries,"work_count":work_total,"work_complete":work_total<=16,"pending_work":pending_total}),
        )
    }
    pub(crate) fn manager_v2_request_rows(
        &self,
        config: &HarnessManagerConfigV1,
        epic: Option<Uuid>,
        after: &str,
        limit: usize,
        unanswered_only: bool,
    ) -> Result<Vec<Value>> {
        // `unanswered_only` selects the shared #664 attention predicate: the
        // open set plus reopened (`failed -> accepted`) requests, which stay
        // visible here but never reclaim a capacity slot.
        let mut stmt=self.conn.prepare(&format!("SELECT id,epic_id,recipient_session_id,message,created_at FROM harness_manager_messages m WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3 AND {} AND (?4 IS NULL OR epic_id=?4) AND id>?5 AND (?7=0 OR ({})) ORDER BY id LIMIT ?6", super::super::harness_manager::MANAGER_REQUEST_ROW, super::super::harness_manager::manager_request_unanswered_sql()))?;
        let raw = stmt
            .query_map(
                params![
                    config.project_id.to_string(),
                    config.manager_session_id.to_string(),
                    config.row_version,
                    epic.map(|id| id.to_string()),
                    after,
                    limit as i64,
                    unanswered_only,
                ],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                    ))
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let timeout = self
            .get_harness_manager_policy(config.project_id)?
            .map_or(900, |p| p.policy.request_timeout_seconds);
        let mut rows = Vec::new();
        for (id, epic, recipient, message, created) in raw {
            let epic = Uuid::parse_str(&epic)
                .map_err(|_| refused("manager_v2_invalid_stored_identity"))?;
            let record = self.manager_v2_record(config, "request", &id)?;
            let current = self.manager_lead(config.project_id, epic).ok();
            let request_id =
                Uuid::parse_str(&id).map_err(|_| refused("manager_v2_invalid_stored_identity"))?;
            let readdressed_to = self.manager_request_readdressed(config, request_id)?;
            let readdressed_from = self.manager_readdress_origin_id(config, request_id)?;
            let settlement = self.manager_request_settlement(config, request_id)?;
            // Operator audit remains readable if the manager lineage itself is
            // unavailable. Mutation authorization still propagates that denial.
            let live = config.current_session_id.is_some()
                && self.manager_v2_operator_request_live(config, epic, request_id)?;
            let effective_recipient = current.as_ref().filter(|_| live).map(|s| s.id);
            let retrieved = if let Some(reader) = effective_recipient {
                self.manager_v2_record(config, "retrieval", &format!("{id}:{reader}"))?
                    .is_some()
            } else {
                false
            };
            let lead_changed = !live;
            let deadline = DateTime::parse_from_rfc3339(&created)
                .map_err(|_| refused("manager_v2_invalid_stored_timestamp"))?
                .with_timezone(&Utc)
                + chrono::Duration::seconds(i64::from(timeout));
            let state = record
                .as_ref()
                .map(|r| r.payload["state"].clone())
                .unwrap_or(json!(if retrieved { "retrieved" } else { "queued" }));
            let state = if readdressed_to.is_some() {
                json!("readdressed")
            } else {
                state
            };
            let terminal = matches!(state.as_str(), Some("completed" | "failed" | "declined"));
            let acknowledged = matches!(
                state.as_str(),
                Some("accepted" | "running" | "completed" | "blocked" | "failed" | "declined")
            );
            let reply: Option<String> = self.conn.query_row("SELECT id FROM harness_manager_messages WHERE request_id=?1 ORDER BY sequence DESC LIMIT 1",[&id],|r|r.get(0)).optional()?;
            let replied = reply.is_some();
            let reply_retrieved =
                if let (Some(reply), Some(manager)) = (&reply, config.current_session_id) {
                    self.manager_v2_record(config, "retrieval", &format!("{reply}:{manager}"))?
                        .is_some()
                } else {
                    false
                };
            rows.push(json!({"type":"request","key":id,"request_id":id,"epic_id":epic,"recipient_session_id":recipient,"effective_recipient_session_id":effective_recipient,"message":bounded(&message,2048),"state":state,"readdressed_to":readdressed_to,"readdressed_from":readdressed_from,"row_version":record.as_ref().map_or(0,|r|r.row_version),"execution":record.map(|r|r.payload),"retrieved":retrieved,"replied":replied,"reply_message_id":reply,"reply_retrieved":reply_retrieved,"accepted":matches!(state.as_str(),Some("accepted"|"running"|"completed")),"deadline":deadline,"settlement":settlement,"overdue":!terminal&&!acknowledged&&settlement.is_none()&&Utc::now()>deadline,"unanswered":!replied&&!acknowledged&&!terminal&&readdressed_to.is_none()&&settlement.is_none(),"delivery_issue":if readdressed_to.is_some(){None}else if lead_changed{Some("lead_changed")}else if current.as_ref().is_some_and(|s|matches!(s.status,SessionStatus::Failed|SessionStatus::Interrupted)){Some("lead_unavailable")}else if replied&&!reply_retrieved{Some("reply_unretrieved")}else{None}}));
        }
        Ok(rows)
    }
}
fn record_row(r: ManagerRecordV2) -> Value {
    let mut v = r.payload;
    v["type"] = json!(r.kind);
    v["key"] = json!(r.key);
    v["row_version"] = json!(r.row_version);
    v["created_at"] = json!(r.created_at);
    v["updated_at"] = json!(r.updated_at);
    v
}
fn bounded(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}
