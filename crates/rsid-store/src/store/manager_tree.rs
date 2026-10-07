//! Recursive-manager operator tree (#890 Slice C v0): the read-only snapshot
//! behind `GetManagerTree`. Pure reads over the manager, grant, escalation and
//! decision tables; no authority is exercised here.

use std::collections::{BTreeMap, HashMap};

use rsi_common::grant_narrowing::portfolio_effective_launches;
use rsi_common::harness_manager::{HarnessManagerConfigV1, HarnessManagerScopeModeV1};
use rsi_common::harness_manager_v2::ManagerLaunchChoiceV2;
use rsi_common::manager_nodes::ManagerNodeSelectorV1;
use rsi_common::manager_tree::{
    MANAGER_TREE_MAX_EPICS_PER_PROJECT, ManagerTreeGrantV1, ManagerTreeKindV1, ManagerTreeLoadV1,
    ManagerTreeReservedV1, ManagerTreeRowV1, ManagerTreeSeatV1,
};
use rusqlite::params;
use uuid::Uuid;

use super::Store;
use super::manager_nodes::AreaNode;
use crate::error::Result;

/// The whole snapshot before paging.
pub struct ManagerTreeSnapshot {
    pub rows: Vec<ManagerTreeRowV1>,
    pub global_grant_version: Option<i64>,
}

struct EpicRow {
    id: Uuid,
    title: String,
    group_id: Option<Uuid>,
    lead: Option<Uuid>,
    running: i64,
}

fn scalar(store: &Store, sql: &str, args: impl rusqlite::Params) -> Option<i64> {
    store.conn.query_row(sql, args, |row| row.get(0)).ok()
}

fn selector_summary(selector: Option<&ManagerNodeSelectorV1>) -> String {
    match selector {
        None => "unknown scope".into(),
        Some(ManagerNodeSelectorV1::Project) => "project".into(),
        Some(ManagerNodeSelectorV1::Selected {
            group_ids,
            epic_ids,
        }) => format!(
            "{} group{}, {} epic{}",
            group_ids.len(),
            if group_ids.len() == 1 { "" } else { "s" },
            epic_ids.len(),
            if epic_ids.len() == 1 { "" } else { "s" },
        ),
    }
}

impl Store {
    fn tree_seat(&self, id: Uuid) -> Option<ManagerTreeSeatV1> {
        let session = self.get_session(id).ok().flatten()?;
        Some(ManagerTreeSeatV1 {
            session_id: session.id,
            status: session.status,
            model: session.model,
            context_fill_pct: session.context_fill_pct,
            updated_at: session.updated_at,
        })
    }

    fn tree_reserved(&self, node: Uuid) -> Option<Vec<ManagerTreeReservedV1>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT resource_kind,sum(amount) FROM manager_node_reservations
                 WHERE parent_node_id=?1 AND state='active' GROUP BY resource_kind ORDER BY resource_kind",
            )
            .ok()?;
        stmt.query_map([node.to_string()], |row| {
            Ok(ManagerTreeReservedV1 {
                resource_kind: row.get(0)?,
                amount: row.get(1)?,
            })
        })
        .ok()?
        .collect::<rusqlite::Result<Vec<_>>>()
        .ok()
    }

    fn tree_escalations(&self, node: Uuid) -> Option<i64> {
        scalar(
            self,
            // #1238: an escalation held above the project root counts at the
            // tier it waits on, not here.
            "SELECT count(*) FROM manager_node_escalations e WHERE e.target_node_id=?1 AND e.state='open'
               AND NOT EXISTS(SELECT 1 FROM manager_tier_escalations h WHERE h.escalation_id=e.id AND h.state='open')",
            [node.to_string()],
        )
    }

    /// Projects shown: the global grant's, then any project with a manager
    /// appointment. At most one more than the cap is read so overflow is known.
    fn tree_projects(&self, granted: &[Uuid]) -> Result<(Vec<Uuid>, bool)> {
        let mut ids: Vec<Uuid> = granted.to_vec();
        let mut stmt = self.conn.prepare(
            "SELECT project_id FROM harness_manager_scopes ORDER BY project_id LIMIT 257",
        )?;
        let mut overflow = false;
        for id in stmt.query_map([], |row| row.get::<_, String>(0))? {
            if let Ok(id) = Uuid::parse_str(&id?)
                && !ids.contains(&id)
            {
                if ids.len() >= 256 {
                    overflow = true;
                    break;
                }
                ids.push(id);
            }
        }
        Ok((ids, overflow))
    }

    fn tree_epics(&self, project: Uuid) -> Result<(Vec<EpicRow>, bool)> {
        let mut stmt = self.conn.prepare(
            "SELECT e.id,coalesce(e.title,''),e.parent_id,e.lead_session_id,
                    (SELECT count(*) FROM sessions w WHERE w.parent_id=e.id AND w.status='Running')
             FROM sessions e WHERE e.project_id=?1 AND e.session_kind='Epic'
               AND e.lead_session_id IS NOT NULL AND e.status NOT IN ('Archived','Deleted')
             ORDER BY e.id LIMIT ?2",
        )?;
        let cap = MANAGER_TREE_MAX_EPICS_PER_PROJECT;
        let mut epics = Vec::new();
        let rows = stmt.query_map(params![project.to_string(), cap as i64 + 1], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?;
        for row in rows {
            let (id, title, parent, lead, running) = row?;
            let Ok(id) = Uuid::parse_str(&id) else {
                continue;
            };
            epics.push(EpicRow {
                id,
                title,
                group_id: parent.and_then(|p| Uuid::parse_str(&p).ok()),
                lead: lead.and_then(|l| Uuid::parse_str(&l).ok()),
                running,
            });
        }
        let complete = epics.len() <= cap;
        epics.truncate(cap);
        Ok((epics, complete))
    }

    /// Pending operator decisions of the current appointment, by Epic.
    fn tree_decisions(
        &self,
        project: Uuid,
        manager: Uuid,
        scope_version: i64,
    ) -> Option<(i64, HashMap<Uuid, i64>)> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT epic_id,count(*) FROM harness_manager_v2_records
                 WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3
                   AND kind='decision' AND archived=0
                   AND json_extract(payload_json,'$.status') IN ('pending','answer_queued')
                 GROUP BY epic_id",
            )
            .ok()?;
        let mut total = 0;
        let mut by_epic = HashMap::new();
        let rows = stmt
            .query_map(
                params![project.to_string(), manager.to_string(), scope_version],
                |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, i64>(1)?)),
            )
            .ok()?;
        for row in rows {
            let (epic, count) = row.ok()?;
            total += count;
            if let Some(epic) = epic.and_then(|e| Uuid::parse_str(&e).ok()) {
                by_epic.insert(epic, count);
            }
        }
        Some((total, by_epic))
    }

    /// Build the whole depth-first snapshot. Counts the daemon cannot read are
    /// `None` and rows it cannot list completely are `complete == false`.
    ///
    /// #1236: every active portfolio node is a `Portfolio` row (its tier label
    /// is display only); a project sits under the deepest node covering it.
    pub fn manager_tree_snapshot(&self) -> Result<ManagerTreeSnapshot> {
        let nodes = self.list_portfolio_nodes(false)?;
        let index: HashMap<Uuid, usize> = nodes
            .iter()
            .enumerate()
            .map(|(i, node)| (node.node_id, i))
            .collect();
        // Depth of each node from its parent chain (roots are 0).
        let depth_of = |start: usize| -> u16 {
            let mut depth = 0u16;
            let mut cursor = nodes[start].parent_node_id;
            while let Some(parent) = cursor.and_then(|id| index.get(&id)) {
                depth += 1;
                if usize::from(depth) > nodes.len() {
                    break;
                }
                cursor = nodes[*parent].parent_node_id;
            }
            depth
        };
        let depths: Vec<u16> = (0..nodes.len()).map(depth_of).collect();
        // Each project's owner: the deepest node whose grant covers it.
        let mut owner: HashMap<Uuid, usize> = HashMap::new();
        let mut granted: Vec<Uuid> = Vec::new();
        for (i, node) in nodes.iter().enumerate() {
            for project in &node.grant.project_ids {
                if !granted.contains(project) {
                    granted.push(*project);
                }
                let deeper = owner
                    .get(project)
                    .is_none_or(|held| depths[*held] < depths[i]);
                if deeper {
                    owner.insert(*project, i);
                }
            }
        }
        let (projects, projects_overflow) = self.tree_projects(&granted)?;
        let mut blocks: HashMap<usize, Vec<Vec<ManagerTreeRowV1>>> = HashMap::new();
        let mut other_rows = Vec::new();
        for project in projects {
            let parent = owner.get(&project).copied();
            let block = self.tree_project_block(
                project,
                parent.map(|i| format!("portfolio:{}", nodes[i].node_id)),
                parent.map_or(0, |i| depths[i] + 1),
            )?;
            let mut block_rows = vec![block.0];
            block_rows.extend(block.1);
            match parent {
                Some(i) => blocks.entry(i).or_default().push(block_rows),
                None => other_rows.extend(block_rows),
            }
        }
        // Depth-first over nodes: children in creation order under a parent.
        let mut children: BTreeMap<Option<usize>, Vec<usize>> = BTreeMap::new();
        for (i, node) in nodes.iter().enumerate() {
            let parent = node.parent_node_id.and_then(|id| index.get(&id).copied());
            children.entry(parent).or_default().push(i);
        }
        let mut order = Vec::with_capacity(nodes.len());
        let mut pending: Vec<usize> = children.get(&None).cloned().unwrap_or_default();
        pending.reverse();
        while let Some(i) = pending.pop() {
            order.push(i);
            let mut kids = children.get(&Some(i)).cloned().unwrap_or_default();
            kids.reverse();
            pending.extend(kids);
        }
        let mut rows = Vec::new();
        for i in order {
            let node = &nodes[i];
            let owned = blocks.remove(&i).unwrap_or_default();
            let sum = |pick: fn(&ManagerTreeRowV1) -> Option<i64>| {
                owned
                    .iter()
                    .map(|block| pick(&block[0]))
                    .try_fold(0i64, |total, value| value.map(|v| total + v))
            };
            let pending_decisions = sum(|row| row.load.pending_decisions);
            // #1238: the subtree's pending escalations plus the hops addressed
            // to this node itself.
            let pending_escalations = sum(|row| row.load.pending_escalations).and_then(|below| {
                self.tier_open_hops_for_node(node.node_id)
                    .map(|own| below + own)
            });
            let direct_children = children.get(&Some(i)).map_or(0, Vec::len);
            let complete = owned.iter().flatten().all(|row| row.complete) && !projects_overflow;
            let policy = &node.grant.project_policy;
            rows.push(ManagerTreeRowV1 {
                key: format!("portfolio:{}", node.node_id),
                parent_key: node
                    .parent_node_id
                    .filter(|id| index.contains_key(id))
                    .map(|id| format!("portfolio:{id}")),
                depth: depths[i],
                kind: ManagerTreeKindV1::Portfolio,
                label: format!("{} manager", node.tier_label),
                tier_label: Some(node.tier_label.clone()),
                grantor: Some(node.grantor.clone()),
                project_id: None,
                node_id: Some(node.node_id),
                epic_id: None,
                scope: Some(format!(
                    "{} granted project(s)",
                    node.grant.project_ids.len()
                )),
                seat: self.tree_seat(node.grant.seat_session_id),
                focus_session_id: Some(node.grant.seat_session_id),
                grant: Some(ManagerTreeGrantV1 {
                    grant_version: node.grant.grant_version,
                    capabilities: policy.capabilities.clone(),
                    max_active_sessions: policy.max_active_sessions,
                    max_created_sessions: policy.max_created_sessions,
                    max_created_containers: policy.max_created_containers,
                    max_direct_reports: node.max_direct_reports,
                    max_spend_usd: policy.max_spend_usd,
                    reserved: Vec::new(),
                }),
                launches: portfolio_effective_launches(
                    &node.grant.allowed_launches,
                    &node.grant.project_policy,
                ),
                load: ManagerTreeLoadV1 {
                    running_workers: None,
                    direct_reports: Some((owned.len() + direct_children) as i64),
                    pending_escalations,
                    pending_decisions,
                },
                complete,
            });
            rows.extend(owned.into_iter().flatten());
        }
        rows.extend(other_rows);
        Ok(ManagerTreeSnapshot {
            rows,
            global_grant_version: self
                .active_global_grant()
                .ok()
                .flatten()
                .map(|g| g.grant_version),
        })
    }

    /// #1412: the launches a project manager may make now: the live
    /// intersection of every node above it (each one's grant list narrowed by
    /// its project policy) with the manager's own list when that is non-empty.
    /// Empty when nothing restricts it.
    fn tree_pm_launches(
        &self,
        config: &HarnessManagerConfigV1,
    ) -> Result<Vec<ManagerLaunchChoiceV2>> {
        let own = self
            .get_harness_manager_policy(config.project_id)?
            .filter(|grant| !grant.revoked)
            .map(|grant| grant.policy.allowed_launches)
            .unwrap_or_default();
        self.manager_effective_launches(config, &own)
    }

    /// One project row followed by its area nodes and led Epics. `depth` is the project row's depth.
    fn tree_project_block(
        &self,
        project: Uuid,
        parent_key: Option<String>,
        depth: u16,
    ) -> Result<(ManagerTreeRowV1, Vec<ManagerTreeRowV1>)> {
        let name = self
            .get_project(project)?
            .map_or_else(|| project.to_string(), |p| p.name);
        let config = self.get_harness_manager(project)?;
        let live = config.as_ref().filter(|c| !c.is_revoked());
        let seat_id = live.and_then(|c| c.current_session_id);
        let scope = match config.as_ref() {
            None => "no manager appointed".to_string(),
            Some(c) if c.is_revoked() => "manager revoked".to_string(),
            Some(c) if c.scope_mode == HarnessManagerScopeModeV1::Project => "project".into(),
            Some(c) => format!(
                "{} group(s), {} epic(s)",
                c.group_ids.len(),
                c.explicit_epic_ids().len()
            ),
        };
        let (decision_total, decisions_by_epic) = match live {
            Some(c) => match self.tree_decisions(project, c.manager_session_id, c.row_version) {
                Some((total, by_epic)) => (Some(total), Some(by_epic)),
                None => (None, None),
            },
            None => (Some(0), Some(HashMap::new())),
        };
        let running = scalar(
            self,
            "SELECT count(*) FROM sessions WHERE project_id=?1 AND status='Running'",
            [project.to_string()],
        );
        let nodes = self.list_area_nodes(project)?;
        let root = nodes.iter().find(|n| n.parent_node_id.is_none());
        let project_key = format!("project:{project}");
        let mut project_row = ManagerTreeRowV1 {
            key: project_key.clone(),
            parent_key,
            depth,
            kind: ManagerTreeKindV1::Project,
            label: name,
            tier_label: None,
            grantor: match live {
                Some(config) => {
                    Some(self.project_manager_grantor(project, config.manager_session_id)?)
                }
                None => None,
            },
            project_id: Some(project),
            node_id: root.map(|n| n.id),
            epic_id: None,
            scope: Some(scope),
            seat: seat_id.and_then(|id| self.tree_seat(id)),
            focus_session_id: seat_id,
            grant: None,
            launches: match live {
                Some(config) => self.tree_pm_launches(config)?,
                None => Vec::new(),
            },
            load: ManagerTreeLoadV1 {
                running_workers: running,
                direct_reports: root.map(|n| i64::from(n.direct_reports)),
                pending_escalations: match root {
                    Some(n) => self.tree_escalations(n.id),
                    None => Some(0),
                },
                pending_decisions: decision_total,
            },
            complete: true,
        };
        let (epics, epics_complete) = self.tree_epics(project)?;
        project_row.complete = epics_complete;
        let mut out = Vec::new();
        // Area nodes depth-first: children sorted by id under their parent.
        let mut children: BTreeMap<Option<Uuid>, Vec<&AreaNode>> = BTreeMap::new();
        for node in nodes.iter().filter(|n| n.parent_node_id.is_some()) {
            children.entry(node.parent_node_id).or_default().push(node);
        }
        let root_id = root.map(|n| n.id);
        let mut area_depth: HashMap<Uuid, u16> = HashMap::new();
        let mut area_epics: HashMap<Uuid, Vec<usize>> = HashMap::new();
        let mut project_epics: Vec<usize> = Vec::new();
        // Proper recursive DFS (explicit stack keeps order deterministic).
        let mut dfs: Vec<(&AreaNode, u16)> = Vec::new();
        let mut pending: Vec<(&AreaNode, u16)> = {
            let mut kids = children.get(&root_id).cloned().unwrap_or_default();
            kids.sort_by_key(|n| std::cmp::Reverse(n.id));
            kids.into_iter().map(|n| (n, depth + 1)).collect()
        };
        while let Some((node, d)) = pending.pop() {
            dfs.push((node, d));
            area_depth.insert(node.id, d);
            let mut kids = children.get(&Some(node.id)).cloned().unwrap_or_default();
            kids.sort_by_key(|n| std::cmp::Reverse(n.id));
            pending.extend(kids.into_iter().map(|n| (n, d + 1)));
        }
        for (index, epic) in epics.iter().enumerate() {
            let owner = dfs
                .iter()
                .filter(|(n, _)| {
                    n.active
                        && n.selector
                            .as_ref()
                            .is_some_and(|s| s.covers_epic(epic.id, epic.group_id))
                })
                .max_by_key(|(_, d)| *d)
                .map(|(n, _)| n.id);
            match owner {
                Some(id) => area_epics.entry(id).or_default().push(index),
                None => project_epics.push(index),
            }
        }
        let epic_row = |index: usize, parent_key: &str, d: u16| -> ManagerTreeRowV1 {
            let epic = &epics[index];
            ManagerTreeRowV1 {
                key: format!("epic:{}", epic.id),
                parent_key: Some(parent_key.to_string()),
                depth: d,
                kind: ManagerTreeKindV1::Epic,
                tier_label: None,
                grantor: None,
                label: if epic.title.is_empty() {
                    epic.id.to_string()
                } else {
                    epic.title.clone()
                },
                project_id: Some(project),
                node_id: None,
                epic_id: Some(epic.id),
                scope: None,
                seat: epic.lead.and_then(|id| self.tree_seat(id)),
                focus_session_id: epic.lead,
                grant: None,
                launches: Vec::new(),
                load: ManagerTreeLoadV1 {
                    running_workers: Some(epic.running),
                    direct_reports: None,
                    pending_escalations: None,
                    pending_decisions: decisions_by_epic
                        .as_ref()
                        .map(|m| m.get(&epic.id).copied().unwrap_or(0)),
                },
                complete: true,
            }
        };
        for (node, d) in &dfs {
            let key = format!("area:{}", node.id);
            let owned = area_epics.get(&node.id).cloned().unwrap_or_default();
            let running_workers =
                epics_complete.then(|| owned.iter().map(|i| epics[*i].running).sum::<i64>());
            let pending_decisions =
                decisions_by_epic
                    .as_ref()
                    .filter(|_| epics_complete)
                    .map(|m| {
                        owned
                            .iter()
                            .map(|i| m.get(&epics[*i].id).copied().unwrap_or(0))
                            .sum::<i64>()
                    });
            out.push(ManagerTreeRowV1 {
                key: key.clone(),
                parent_key: Some(match node.parent_node_id {
                    Some(p) if Some(p) != root_id => format!("area:{p}"),
                    _ => project_key.clone(),
                }),
                depth: *d,
                kind: ManagerTreeKindV1::Area,
                tier_label: None,
                grantor: None,
                label: format!(
                    "area {}{}",
                    &node.id.to_string()[..8],
                    if node.active { "" } else { " (revoked)" }
                ),
                project_id: Some(project),
                node_id: Some(node.id),
                epic_id: None,
                scope: Some(selector_summary(node.selector.as_ref())),
                seat: self.tree_seat(node.seat_root_session_id),
                focus_session_id: Some(node.seat_root_session_id),
                grant: node.grant.as_ref().map(|g| ManagerTreeGrantV1 {
                    grant_version: node.grant_version,
                    capabilities: g.capabilities.clone(),
                    max_active_sessions: g.allowance.max_active_sessions,
                    max_created_sessions: g.allowance.max_created_sessions,
                    max_created_containers: g.allowance.max_created_containers,
                    max_direct_reports: g.max_direct_reports,
                    max_spend_usd: g.allowance.max_spend_usd,
                    reserved: self.tree_reserved(node.id).unwrap_or_default(),
                }),
                launches: node
                    .grant
                    .as_ref()
                    .map(|g| g.allowed_launches.clone())
                    .unwrap_or_default(),
                load: ManagerTreeLoadV1 {
                    running_workers,
                    direct_reports: Some(i64::from(node.direct_reports)),
                    pending_escalations: self.tree_escalations(node.id),
                    pending_decisions,
                },
                complete: true,
            });
            for index in owned {
                out.push(epic_row(index, &key, d + 1));
            }
        }
        for index in project_epics {
            out.push(epic_row(index, &project_key, depth + 1));
        }
        Ok((project_row, out))
    }
}
