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
        if p.phase == 1 && rows.len() < limit {
            let take = limit - rows.len();
            let mut candidates = Vec::new();
            // idx_sessions_parent_id includes rowid as its implicit suffix.
            // One indexed seek per scoped Epic, visiting at most one page. Do not
            // sort UUIDs with this index: that sorts the entire remaining Epic.
            for epic in &scope.epics {
                let mut stmt = self.conn.prepare(
                    "SELECT rowid,id FROM sessions WHERE parent_id=?1 AND rowid>?2 AND rowid<=?3 ORDER BY rowid LIMIT ?4"
                )?;
                for row in stmt.query_map(
                    params![epic.to_string(), p.at[0], p.hi[0], take as i64],
                    |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
                )? {
                    let (position, id) = row?;
                    candidates.push((position, id, *epic));
                }
            }
            candidates.sort_by_key(|r| r.0);
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
            let mut stmt = self.conn.prepare(
                "SELECT rowid,child_session_id,owner_session_id,epic_id,state,updated_at,substr(json_extract(request_json,'$.query'),1,2048),safe_error_class,kind FROM agent_spawn_requests WHERE rowid>?1 AND rowid<=?2 ORDER BY rowid LIMIT ?3"
            )?;
            let pending = stmt
                .query_map(params![p.at[1], p.hi[1], SCAN as i64], |r| {
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
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let end = pending.last().map_or(p.at[1], |r| r.0);
            let exhausted = pending.len() < SCAN || end == p.hi[1];
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
                            rows.push(self.manager_v2_worker(config, &s, epic, &work)?);
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
