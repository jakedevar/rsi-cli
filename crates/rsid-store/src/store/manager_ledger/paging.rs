//! Bounded live keysets for the legal Group -> Epic -> leaf hierarchy.
//!
//! A cursor fixes the sessions/reservations insertion horizons, not a database
//! snapshot. Every page rechecks current scope and parentage. Status, token and
//! ledger updates do not invalidate it. Reparented rows ahead of the keyset are
//! read at their current parent; rows behind it are not repeated. Refresh to see
//! later insertions or rows moved into scope behind the cursor. Container scope
//! changes reject the cursor. No full graph is materialized or hashed per page.
use super::*;
use rsi_common::types::{Session, SessionKind};

// Alias `s`; the page supplies pending/delivered scoped-work identities and a
// fixed seven-day cutoff. Explicit durable obligations do not expire.
const ACTIONABLE_WORKER: &str = "(
    s.status IN ('Starting','Running','WaitingApproval')
    OR s.id IN (SELECT value FROM json_each(:pending))
    OR EXISTS(SELECT 1 FROM agent_jobs j WHERE j.owner_session_id=s.id AND j.state IN ('queued','running'))
    OR s.id IN (SELECT source_session_id FROM rolling_queue_entries
                WHERE state IN ('queued','admitted','gating'))
    OR (s.status NOT IN ('Archived','Deleted') AND (
        s.pending_question_json IS NOT NULL
        OR (s.updated_at>=:recent AND s.status IN ('Failed','Interrupted'))
        OR (s.updated_at>=:recent AND s.status='Completed'
            AND (s.pipeline_artifact IS NOT NULL OR s.sandbox_branch IS NOT NULL)
            AND s.id NOT IN (SELECT value FROM json_each(:delivered))
            AND NOT EXISTS(SELECT 1 FROM rolling_queue_entries q
                WHERE q.source_session_id=s.id AND q.state='published'
                    AND q.finished_at>=s.updated_at))))
)";

pub(super) struct LeafScope {
    pub stamp: String,
    epics: Vec<Uuid>,
    roots: Vec<Value>,
    complete: bool,
}

pub(super) struct LeafPage {
    pub rows: Vec<Value>,
    pub next_after: Option<String>,
    pub coverage: bool,
}

// Compact field names keep the existing opaque cursor within its 512-byte cap.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Position {
    v: u8,
    #[serde(rename = "p")]
    phase: u8, // 0 roots, 1 sessions, 2 reservations, 3 coverage, 4 done, 5 owned containers
    #[serde(rename = "h")]
    hi: [i64; 3],
    #[serde(rename = "a")]
    at: [i64; 3],
    #[serde(rename = "r")]
    root: usize,
    #[serde(rename = "x")]
    partial: bool,
    #[serde(rename = "t")]
    recent_since: i64,
}

impl Store {
    pub(super) fn manager_v2_leaf_scope(
        &self,
        config: &HarnessManagerConfigV1,
        policy: Option<&HarnessManagerPolicyConfigV2>,
        epic: Option<Uuid>,
    ) -> Result<LeafScope> {
        let roots = self.manager_v2_topology_roots(config, policy, epic)?;
        let mut epics = Vec::new();
        let mut complete = true;
        for id in config
            .epic_ids
            .iter()
            .filter(|id| epic.is_none_or(|e| e == **id))
        {
            let Some(s) = self.get_session(*id)?.filter(|s| {
                s.project_id == Some(config.project_id) && s.session_kind == SessionKind::Epic
            }) else {
                complete = false;
                continue;
            };
            let group = s
                .parent_id
                .map(|id| self.get_session(id))
                .transpose()?
                .flatten();
            complete &= group.is_some_and(|g| {
                g.session_kind == SessionKind::Group
                    && g.project_id == Some(config.project_id)
                    && g.parent_id.is_none()
            });
            epics.push(*id);
        }
        let structure: Vec<_> = roots
            .iter()
            .map(|r| {
                json!({
                    "id":r["id"], "parent":r["parent_id"], "kind":r["kind"],
                    "selected":r["selected_epic"], "granted":r["topology_granted"],
                })
            })
            .collect();
        let stamp = fingerprint(&json!({"keyset":1,"epics":epics,"roots":structure,
            "complete":complete,"policy":policy.map(|p|(p.row_version,p.revoked))}))?;
        Ok(LeafScope {
            stamp,
            epics,
            roots,
            complete,
        })
    }

    pub(super) fn manager_v2_leaf_page(
        &self,
        config: &HarnessManagerConfigV1,
        scope: &LeafScope,
        query: &AgentManagerInspectRequestV2,
        after: &str,
    ) -> Result<LeafPage> {
        let topology = query.section == ManagerInspectSectionV2::Topology;
        let actionable = !topology && !query.include_history;
        let limit = usize::from(query.limit);
        let mut p: Position = if after.is_empty() {
            Position {
                v: 1,
                phase: if topology { 0 } else { 1 },
                hi: [
                    self.conn.query_row(
                        "SELECT COALESCE(MAX(rowid),0) FROM sessions",
                        [],
                        |r| r.get(0),
                    )?,
                    self.conn.query_row(
                        "SELECT COALESCE(MAX(rowid),0) FROM agent_spawn_requests",
                        [],
                        |r| r.get(0),
                    )?,
                    self.conn.query_row(
                        "SELECT COALESCE(MAX(rowid),0) FROM harness_manager_v2_entities",
                        [],
                        |r| r.get(0),
                    )?,
                ],
                at: [0, 0, 0],
                root: 0,
                partial: !scope.complete,
                recent_since: (Utc::now() - chrono::Duration::days(7)).timestamp(),
            }
        } else {
            serde_json::from_str(after).map_err(|_| refused("manager_v2_invalid_cursor"))?
        };
        if p.v != 1
            || p.phase > 5
            || (!topology && matches!(p.phase, 0 | 5))
            || (topology && p.phase == 2)
            || (query.epic_id.is_some() && p.phase == 5)
            || p.root > scope.roots.len()
            || p.at.iter().zip(p.hi).any(|(at, hi)| *at < 0 || hi < *at)
        {
            return Err(refused("manager_v2_invalid_cursor"));
        }
        p.partial |= !scope.complete;
        let mut rows = Vec::new();
        if p.phase == 0 {
            for row in scope.roots.iter().skip(p.root).take(limit) {
                rows.push(row.clone());
                p.root += 1;
            }
            if p.root == scope.roots.len() {
                p.phase = if query.epic_id.is_none() { 5 } else { 1 };
            }
        }
        if p.phase == 5 && rows.len() < limit {
            // The scoped kind index also carries rowid. Worker provenance
            // never makes container reads scan an entire creation history.
            let mut entities = Vec::new();
            for kind in ["Group", "Epic"] {
                let mut stmt = self.conn.prepare("SELECT rowid,session_id FROM harness_manager_v2_entities WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3 AND kind=?4 AND rowid>?5 AND rowid<=?6 ORDER BY rowid LIMIT 128")?;
                entities.extend(
                    stmt.query_map(
                        params![
                            config.project_id.to_string(),
                            config.manager_session_id.to_string(),
                            config.row_version,
                            kind,
                            p.at[2],
                            p.hi[2]
                        ],
                        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
                    )?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
                );
            }
            entities.sort_by_key(|r| r.0);
            entities.truncate(128);
            let end = entities.last().map_or(p.at[2], |r| r.0);
            let exhausted = entities.len() < 128 || end == p.hi[2];
            for (position, id) in entities {
                if rows.len() == limit {
                    break;
                }
                p.at[2] = position;
                if scope.roots.iter().any(|r| r["id"] == id) {
                    continue;
                }

                let id = Uuid::parse_str(&id)
                    .map_err(|_| refused("manager_v2_invalid_stored_identity"))?;
                let Some(s) = self.get_session(id)?.filter(|s| {
                    s.project_id == Some(config.project_id)
                        && rsi_common::is_container_kind(s.session_kind)
                }) else {
                    continue;
                };
                rows.push(json!({"type":"topology","key":id,"id":id,"title":bounded(s.title.as_deref().unwrap_or(&s.query),512),
                    "parent_id":s.parent_id,"kind":s.session_kind,"status":s.status,"lead_session_id":s.lead_session_id,
                    "updated_at":s.updated_at,"expected_updated_at":s.updated_at,
                    "expected":if s.session_kind == SessionKind::Epic {self.manager_action_lead_fence(id).ok()}else{None},
                    "selected_epic":config.epic_ids.contains(&id),"topology_granted":false}));
            }
            if exhausted && p.at[2] == end {
                p.phase = 1;
            }
        }
        let work = if topology {
            Vec::new()
        } else {
            self.manager_v2_work_rows(config, query.epic_id)?
        };
        let pending_sources = serde_json::to_string(
            &work
                .iter()
                .filter(|w| w["integrated"] != true && w["archived"] != true)
                .filter_map(|w| w["source_session_id"].as_str())
                .collect::<Vec<_>>(),
        )?;
        let delivered_sources = serde_json::to_string(
            &work
                .iter()
                .filter(|w| w["integrated"] == true)
                .filter_map(|w| w["source_session_id"].as_str())
                .collect::<Vec<_>>(),
        )?;
        let recent = DateTime::from_timestamp(p.recent_since, 0)
            .ok_or_else(|| refused("manager_v2_invalid_cursor"))?
            .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        if p.phase == 1 && rows.len() < limit {
            let take = limit - rows.len();
            let mut candidates = Vec::new();
            // idx_sessions_parent_id includes rowid as its implicit suffix.
            // One indexed seek per scoped Epic, returning at most one page.
            // The default filters before LIMIT so history cannot fill the page;
            // a history-only Epic can still require scanning its retained rows.
            // Seek from the last row rather than sorting the remaining Epic.
            for epic in &scope.epics {
                let position = if actionable {
                    "s.rowid<=:after"
                } else {
                    "s.rowid>:after"
                };
                let order = if actionable { "DESC" } else { "ASC" };
                let sql = format!(
                    "SELECT s.rowid,s.id FROM sessions s WHERE s.parent_id=:epic
                    AND {position} AND s.rowid<=:horizon AND (:actionable=0 OR {ACTIONABLE_WORKER})
                    ORDER BY s.rowid {order} LIMIT :limit"
                );
                let mut stmt = self.conn.prepare(&sql)?;
                for row in stmt.query_map(
                    rusqlite::named_params! {
                        ":epic": epic.to_string(),
                        ":after": if actionable {
                            if p.at[0] == 0 { p.hi[0] } else { p.at[0] - 1 }
                        } else { p.at[0] },
                        ":horizon": p.hi[0],
                        ":limit": take as i64,
                        ":pending": pending_sources,
                        ":recent": recent,
                        ":delivered": delivered_sources,
                        ":actionable": actionable,
                    },
                    |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
                )? {
                    let (position, id) = row?;
                    candidates.push((position, id, *epic));
                }
            }
            candidates.sort_by_key(|r| r.0);
            if actionable {
                candidates.reverse();
            }
            let exhausted = candidates.len() < take;
            for (position, id, epic) in candidates.into_iter().take(take) {
                p.at[0] = position;
                let id = Uuid::parse_str(&id)
                    .map_err(|_| refused("manager_v2_invalid_stored_identity"))?;
                let Some(s) = self
                    .get_session(id)?
                    .filter(|s| s.project_id == Some(config.project_id))
                else {
                    p.partial = true;
                    continue;
                };
                let nested: bool = self.conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sessions WHERE parent_id=?1)",
                    [id.to_string()],
                    |r| r.get(0),
                )?;
                let legal = rsi_common::legal_children(Some(SessionKind::Epic))
                    .contains(&s.session_kind)
                    && !nested;
                p.partial |= !legal;
                let mut row = if topology {
                    json!({"type":"topology","key":id,"id":id,"epic_id":epic,
                        "title":bounded(s.title.as_deref().unwrap_or(&s.query),512),"parent_id":s.parent_id,
                        "kind":s.session_kind,"status":s.status,"lead_session_id":s.lead_session_id,
                        "updated_at":s.updated_at,"expected_updated_at":s.updated_at,"expected":null})
                } else if rsi_common::is_leaf_kind(s.session_kind) {
                    self.manager_v2_worker(config, &s, epic, &work)?
                } else {
                    continue;
                };
                if !legal {
                    row["hierarchy_issue"] = json!("unsupported_hierarchy");
                }
                rows.push(row);
            }
            if exhausted {
                p.phase = if topology { 3 } else { 2 };
            }
        }
        if p.phase == 2 && rows.len() < limit {
            // The released reservation table has no Epic index. Scan a bounded
            // rowid window once, advancing even on an empty scoped page, instead
            // of filtering then LIMIT-ing an unbounded scan or adding a migration.
            const SCAN: usize = 128;
            let position = if actionable { "rowid<=?1" } else { "rowid>?1" };
            let order = if actionable { "DESC" } else { "ASC" };
            let mut stmt = self.conn.prepare(&format!(
                "SELECT rowid,child_session_id,owner_session_id,epic_id,state,updated_at,substr(json_extract(request_json,'$.query'),1,2048),safe_error_class,kind FROM agent_spawn_requests WHERE {position} AND rowid<=?2 ORDER BY rowid {order} LIMIT ?3"
            ))?;
            let pending = stmt
                .query_map(
                    params![
                        if actionable {
                            if p.at[1] == 0 { p.hi[1] } else { p.at[1] - 1 }
                        } else {
                            p.at[1]
                        },
                        p.hi[1],
                        SCAN as i64
                    ],
                    |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, String>(3)?,
                            r.get::<_, String>(4)?,
                            r.get::<_, String>(5)?,
                            r.get::<_, String>(6)?,
                            r.get::<_, Option<String>>(7)?,
                            r.get::<_, String>(8)?,
                        ))
                    },
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let end = pending.last().map_or(p.at[1], |r| r.0);
            let exhausted = pending.len() < SCAN || (!actionable && end == p.hi[1]);
            for (position, id, owner, epic, state, updated, task, error, kind) in pending {
                if rows.len() == limit {
                    break;
                }
                p.at[1] = position;
                let epic = Uuid::parse_str(&epic)
                    .map_err(|_| refused("manager_v2_invalid_stored_identity"))?;
                if !scope.epics.contains(&epic) {
                    continue;
                }
                let owner = Uuid::parse_str(&owner)
                    .map_err(|_| refused("manager_v2_invalid_stored_identity"))?;
                if !self
                    .get_session(owner)?
                    .is_some_and(|s| legal_member(&s, config.project_id, epic))
                {
                    p.partial = true;
                    continue;
                }
                let id = Uuid::parse_str(&id)
                    .map_err(|_| refused("manager_v2_invalid_stored_identity"))?;
                let published: Option<i64> = self
                    .conn
                    .query_row(
                        "SELECT rowid FROM sessions WHERE id=?1",
                        [id.to_string()],
                        |r| r.get(0),
                    )
                    .optional()?;
                if let Some(position) = published {
                    // Publication after the session horizon must not lose the
                    // reserved identity between the two lanes of this cursor.
                    if position > p.hi[0] {
                        if let Some(s) = self
                            .get_session(id)?
                            .filter(|s| legal_member(s, config.project_id, epic))
                        {
                            let visible = !actionable
                                || self.conn.query_row(
                                    &format!(
                                        "SELECT {ACTIONABLE_WORKER} FROM sessions s WHERE s.id=:id"
                                    ),
                                    rusqlite::named_params! {
                                        ":id": id.to_string(),
                                        ":pending": pending_sources,
                                        ":recent": recent,
                                        ":delivered": delivered_sources,
                                    },
                                    |r| r.get::<_, bool>(0),
                                )?;
                            if visible {
                                rows.push(self.manager_v2_worker(config, &s, epic, &work)?);
                            }
                        } else {
                            p.partial = true;
                        }
                    }
                    continue;
                }
                if state == "launched" {
                    p.partial = true;
                    continue;
                }
                if actionable && state == "failed" && updated < recent {
                    continue;
                }
                let kind: SessionKind = serde_json::from_value(json!(kind))?;
                let legal = rsi_common::legal_children(Some(SessionKind::Epic)).contains(&kind);
                p.partial |= !legal;
                rows.push(json!({"type":"worker","key":id,"session_id":id,"epic_id":epic,
                    "title":bounded(&task,512),"active_task":task,"owner_session_id":owner,
                    "safe_error_class":error,"spawn_state":state,"status":if state=="failed"{"Failed"}else{"Reserved"},
                    "updated_at":updated,"evidence_state":"unknown",
                    "hierarchy_issue":if legal {None}else{Some("unsupported_hierarchy")}}));
            }
            if exhausted && p.at[1] == end {
                p.phase = 3;
            }
        }
        if p.phase == 3 && (!p.partial || rows.len() < limit) {
            if p.partial {
                rows.push(json!({"type":"coverage","key":"~coverage","state":"unknown",
                    "reason":"unsupported_hierarchy_or_scope_unavailable",
                    "next_action":"Direct Epic children are paged; unsupported descendants require session inspection."}));
            }
            p.phase = 4;
        }
        for row in &mut rows {
            row["pagination"] = json!("live_keyset");
        }
        Ok(LeafPage {
            rows,
            coverage: !p.partial,
            next_after: if p.phase == 4 {
                None
            } else {
                Some(serde_json::to_string(&p)?)
            },
        })
    }
}

fn legal_member(s: &Session, project: Uuid, epic: Uuid) -> bool {
    s.project_id == Some(project)
        && s.parent_id == Some(epic)
        && rsi_common::legal_children(Some(SessionKind::Epic)).contains(&s.session_kind)
}
