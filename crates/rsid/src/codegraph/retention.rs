//! H3 detailed-history retention across project-bound codegraph databases.

use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, Utc};
use uuid::Uuid;

use super::{IndexError, Result};

pub const MAX_READY_GENERATIONS: usize = 20;
pub const MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
pub const GLOBAL_DETAIL_CEILING_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const WRITE_HEADROOM_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionPolicy {
    pub max_ready_generations: usize,
    pub max_age: Duration,
    pub global_disk_ceiling_bytes: u64,
}

impl RetentionPolicy {
    #[must_use]
    pub const fn new(global_disk_ceiling_bytes: u64) -> Self {
        Self {
            max_ready_generations: MAX_READY_GENERATIONS,
            max_age: MAX_AGE,
            global_disk_ceiling_bytes,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetailedGeneration {
    pub workspace_id: Uuid,
    pub generation: i64,
    pub published_at: DateTime<Utc>,
    pub detailed_bytes: u64,
    pub current: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionPlan {
    /// Detailed generations to remove in the codegraph store's transaction.
    /// Linked identity and tombstone records are never included here.
    pub prune: Vec<(Uuid, i64)>,
    /// Logical detailed bytes still above the ceiling because current heads
    /// are protected. Physical `SQLite` file size may remain higher until a
    /// separately approved compaction path exists.
    pub protected_over_ceiling_bytes: u64,
    /// Physical bytes still above the ceiling after attempted reclamation,
    /// including each project database and its WAL/SHM sidecars.
    pub physical_over_ceiling_bytes: u64,
}

/// Reserve room for the current write, WAL, and a later diagnostic update.
/// The main database is held below half the remaining project allocation so
/// one complete WAL image can fit alongside protected ready data.
fn planned_main_file_limit(ceiling: u64, physical: u64, current_main: u64) -> Option<u64> {
    let headroom = WRITE_HEADROOM_BYTES.min(ceiling / 4);
    let other_files = physical.checked_sub(current_main)?;
    let main_limit = ceiling.checked_sub(other_files)?.checked_sub(headroom)? / 2;
    (current_main < main_limit).then_some(main_limit)
}

/// Gate a new index write against all dedicated project databases. A writer
/// calls this again after staging, immediately before the ready-head flip.
pub(super) fn admit_write_with_ceiling(
    index_root: &Path,
    db_path: &Path,
    store: &rsi_codegraph::CodegraphStore,
    ceiling: u64,
) -> Result<()> {
    if !store.checkpoint_write_wal()? {
        return Err(IndexError::ActiveReader);
    }
    let physical = physical_bytes(index_root)?;
    let current_main = std::fs::metadata(db_path)?.len();
    let main_limit =
        planned_main_file_limit(ceiling, physical, current_main).ok_or(IndexError::DiskBudget {
            used_bytes: physical,
            ceiling_bytes: ceiling,
        })?;
    store.constrain_database_bytes(main_limit)?;
    Ok(())
}

fn physical_bytes(index_root: &Path) -> Result<u64> {
    let mut total = 0u64;
    let mut projects = 0usize;
    for entry in std::fs::read_dir(index_root)? {
        let entry = entry?;
        let Some(_project_id) = entry
            .file_name()
            .to_str()
            .and_then(|name| Uuid::parse_str(name).ok())
        else {
            continue;
        };
        projects += 1;
        if projects > 1024 {
            return Err(IndexError::DiscoveryLimit("codegraph project databases"));
        }
        for suffix in ["", "-wal", "-shm"] {
            let path = entry.path().join(format!("codegraph.sqlite{suffix}"));
            match std::fs::metadata(path) {
                Ok(metadata) => total = total.saturating_add(metadata.len()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(total)
}

/// Select only prior ready generations. This pure planner does not mutate S1
/// tables; a forward store API must apply its result atomically after publish.
pub fn plan_retention(
    policy: RetentionPolicy,
    now: DateTime<Utc>,
    generations: &[DetailedGeneration],
) -> RetentionPlan {
    let mut by_workspace = std::collections::BTreeMap::<Uuid, Vec<&DetailedGeneration>>::new();
    for generation in generations {
        by_workspace
            .entry(generation.workspace_id)
            .or_default()
            .push(generation);
    }
    let mut prune = Vec::new();
    let mut candidates = Vec::new();
    let mut retained_bytes = 0u64;
    for (workspace_id, mut rows) in by_workspace {
        rows.sort_by_key(|row| std::cmp::Reverse(row.generation));
        let mut prior_kept = 0usize;
        for row in rows {
            if row.current {
                retained_bytes = retained_bytes.saturating_add(row.detailed_bytes);
                continue;
            }
            let age = now.signed_duration_since(row.published_at);
            let too_old = age.to_std().is_ok_and(|age| age > policy.max_age);
            if too_old || prior_kept >= policy.max_ready_generations.saturating_sub(1) {
                prune.push((workspace_id, row.generation));
            } else {
                prior_kept += 1;
                retained_bytes = retained_bytes.saturating_add(row.detailed_bytes);
                candidates.push(row);
            }
        }
    }
    candidates.sort_by_key(|row| (row.published_at, row.workspace_id, row.generation));
    for row in candidates {
        if retained_bytes <= policy.global_disk_ceiling_bytes {
            break;
        }
        prune.push((row.workspace_id, row.generation));
        retained_bytes = retained_bytes.saturating_sub(row.detailed_bytes);
    }
    prune.sort_unstable();
    prune.dedup();
    RetentionPlan {
        prune,
        protected_over_ceiling_bytes: retained_bytes
            .saturating_sub(policy.global_disk_ceiling_bytes),
        physical_over_ceiling_bytes: 0,
    }
}

/// Apply one global plan over all project databases beneath the trusted index
/// root. A project database checks its bound project ID before any mutation.
pub fn enforce(index_root: &Path) -> Result<RetentionPlan> {
    enforce_with_policy(
        index_root,
        RetentionPolicy::new(GLOBAL_DETAIL_CEILING_BYTES),
    )
}

fn enforce_with_policy(index_root: &Path, policy: RetentionPolicy) -> Result<RetentionPlan> {
    let mut stores = Vec::new();
    for entry in std::fs::read_dir(index_root)? {
        let entry = entry?;
        let Some(id) = entry
            .file_name()
            .to_str()
            .and_then(|name| Uuid::parse_str(name).ok())
        else {
            continue;
        };
        let path = entry.path().join("codegraph.sqlite");
        if path.is_file() {
            stores.push((id, rsi_codegraph::CodegraphStore::open(path, id)?));
        }
    }
    if stores.len() > 1024 {
        return Err(IndexError::DiscoveryLimit("codegraph project databases"));
    }
    let mut generations = Vec::new();
    let mut owners = std::collections::HashMap::new();
    for (index, (_, store)) in stores.iter_mut().enumerate() {
        for row in store.generation_details()? {
            if owners
                .insert(row.workspace_id, index)
                .is_some_and(|old| old != index)
            {
                return Err(IndexError::UnsafeWorkspace(
                    "duplicate codegraph workspace identity".into(),
                ));
            }
            let published_at = DateTime::parse_from_rfc3339(&row.published_at)
                .map_err(|_| {
                    IndexError::UnsafeWorkspace("invalid codegraph publication time".into())
                })?
                .with_timezone(&Utc);
            generations.push(DetailedGeneration {
                workspace_id: row.workspace_id,
                generation: row.generation,
                published_at,
                detailed_bytes: row.detailed_bytes,
                current: row.current,
            });
        }
    }
    let mut plan = plan_retention(policy, Utc::now(), &generations);
    let mut pruned_stores = std::collections::HashSet::new();
    for (workspace_id, generation) in &plan.prune {
        let index = owners.get(workspace_id).ok_or_else(|| {
            IndexError::UnsafeWorkspace("retention generation lost ownership".into())
        })?;
        stores[*index]
            .1
            .prune_detailed_generation(*workspace_id, *generation)?;
        pruned_stores.insert(*index);
    }
    // Retry previously deferred compaction after readers release their
    // snapshots, without rewriting a large database for a few free pages.
    let reclaim_threshold = (policy.global_disk_ceiling_bytes / 100).min(64 * 1024 * 1024);
    for (index, (project_id, store)) in stores.iter_mut().enumerate() {
        let reclaimable = store.reclaimable_bytes()?;
        if reclaimable == 0 || (!pruned_stores.contains(&index) && reclaimable < reclaim_threshold)
        {
            continue;
        }
        let db = index_root
            .join(project_id.to_string())
            .join("codegraph.sqlite");
        let main_bytes = std::fs::metadata(&db)?.len();
        // SQLite VACUUM can need a replacement image and WAL before reclaiming
        // the old one. Defer it when that temporary space crosses the cap.
        let enough_vacuum_room = physical_bytes(index_root)?
            .saturating_add(main_bytes.saturating_mul(2))
            <= policy.global_disk_ceiling_bytes;
        let threshold = if enough_vacuum_room {
            reclaim_threshold
        } else {
            u64::MAX
        };
        if !store.compact_pruned_history(threshold)? {
            tracing::debug!(project_id = %project_id, "Codegraph compaction deferred by active reader");
        }
    }
    plan.physical_over_ceiling_bytes =
        physical_bytes(index_root)?.saturating_sub(policy.global_disk_ceiling_bytes);
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn physical_admission_reserves_wal_and_rejects_protected_overflow() {
        assert_eq!(planned_main_file_limit(1_000, 100, 100), Some(375));
        assert_eq!(planned_main_file_limit(1_000, 400, 400), None);
        assert_eq!(planned_main_file_limit(1_000, 1_001, 400), None);
    }

    #[test]
    fn second_project_consumes_global_write_budget_without_pruning_ready_heads() {
        use crate::codegraph::{
            RegisteredWorkspace,
            worker::{WorkerState, index_once, project_db_path},
        };
        use std::sync::atomic::AtomicU64;

        let indexes = tempfile::tempdir().unwrap();
        let mut projects = Vec::new();
        for name in ["alpha", "beta"] {
            let root = tempfile::tempdir().unwrap();
            std::fs::write(
                root.path().join("lib.rs"),
                format!("pub fn {name}() {{}}\n"),
            )
            .unwrap();
            let workspace = RegisteredWorkspace::primary(Uuid::new_v4(), root.path()).unwrap();
            let db = project_db_path(indexes.path(), workspace.project_id);
            let ready = index_once(
                &mut WorkerState::default(),
                &workspace,
                &db,
                &AtomicU64::new(1),
                1,
            )
            .unwrap()
            .ready;
            projects.push((root, workspace, db, ready));
        }
        let (_, first, first_db, first_ready) = &projects[0];
        let (_, second, second_db, second_ready) = &projects[1];
        let first_store = rsi_codegraph::CodegraphStore::open(first_db, first.project_id).unwrap();
        let main = std::fs::metadata(first_db).unwrap().len();
        let global = physical_bytes(indexes.path()).unwrap();
        let first_physical = ["", "-wal", "-shm"]
            .into_iter()
            .filter_map(|suffix| std::fs::metadata(format!("{}{suffix}", first_db.display())).ok())
            .map(|metadata| metadata.len())
            .sum::<u64>();
        let ceiling = (global + 1..global.saturating_mul(4))
            .step_by(4096)
            .find(|ceiling| {
                planned_main_file_limit(*ceiling, first_physical, main).is_some()
                    && planned_main_file_limit(*ceiling, global, main).is_none()
            })
            .expect("two project databases create a distinct global budget limit");
        assert!(global < ceiling);
        assert!(matches!(
            admit_write_with_ceiling(indexes.path(), first_db, &first_store, ceiling),
            Err(IndexError::DiskBudget { .. })
        ));
        assert!(physical_bytes(indexes.path()).unwrap() <= ceiling);
        assert_eq!(
            first_store.current_ready(first.workspace_id).unwrap(),
            *first_ready
        );
        drop(first_store);
        assert_eq!(
            rsi_codegraph::CodegraphStore::open(second_db, second.project_id)
                .unwrap()
                .current_ready(second.workspace_id)
                .unwrap(),
            *second_ready
        );

        let parking = tempfile::tempdir().unwrap();
        let parked = parking.path().join(second.project_id.to_string());
        std::fs::rename(second_db.parent().unwrap(), &parked).unwrap();
        let first_store = rsi_codegraph::CodegraphStore::open(first_db, first.project_id).unwrap();
        admit_write_with_ceiling(indexes.path(), first_db, &first_store, ceiling).unwrap();
        drop(first_store);
        std::fs::rename(&parked, second_db.parent().unwrap()).unwrap();
        assert_eq!(
            rsi_codegraph::CodegraphStore::open(second_db, second.project_id)
                .unwrap()
                .current_ready(second.workspace_id)
                .unwrap(),
            *second_ready
        );
    }

    #[test]
    fn global_ceiling_prunes_prior_generations_across_projects() {
        use crate::codegraph::{
            RegisteredWorkspace,
            worker::{WorkerState, index_once, project_db_path},
        };
        use std::sync::atomic::AtomicU64;
        let indexes = tempfile::tempdir().unwrap();
        let mut workspaces = Vec::new();
        for name in ["alpha", "beta"] {
            let root = tempfile::tempdir().unwrap();
            let workspace = RegisteredWorkspace::primary(Uuid::new_v4(), root.path()).unwrap();
            let db = project_db_path(indexes.path(), workspace.project_id);
            let revision = AtomicU64::new(1);
            let mut state = WorkerState::default();
            std::fs::write(
                root.path().join("lib.rs"),
                format!("pub fn {name}_old() {{}}\n"),
            )
            .unwrap();
            index_once(&mut state, &workspace, &db, &revision, 1).unwrap();
            std::fs::write(
                root.path().join("lib.rs"),
                format!("pub fn {name}_new() {{}}\n"),
            )
            .unwrap();
            index_once(&mut state, &workspace, &db, &revision, 1).unwrap();
            workspaces.push((root, workspace, db));
        }
        let plan = enforce_with_policy(indexes.path(), RetentionPolicy::new(1)).unwrap();
        assert_eq!(plan.prune.len(), 2);
        assert!(plan.protected_over_ceiling_bytes > 0);
        assert!(plan.physical_over_ceiling_bytes > 0);
        for (_, workspace, db) in workspaces {
            let mut store = rsi_codegraph::CodegraphStore::open(db, workspace.project_id).unwrap();
            let ready = store.current_ready(workspace.workspace_id).unwrap();
            assert!(matches!(
                admit_write_with_ceiling(
                    indexes.path(),
                    &project_db_path(indexes.path(), workspace.project_id),
                    &store,
                    1
                ),
                Err(IndexError::DiskBudget { .. })
            ));
            assert_eq!(store.current_ready(workspace.workspace_id).unwrap(), ready);
            let details = store.generation_details().unwrap();
            assert_eq!(details.len(), 1);
            assert!(details[0].current);
        }
    }

    #[test]
    fn age_boundary_prunes_only_prior_generation() {
        use crate::codegraph::{
            RegisteredWorkspace,
            worker::{WorkerState, index_once, project_db_path},
        };
        use std::sync::atomic::AtomicU64;
        let indexes = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let workspace = RegisteredWorkspace::primary(Uuid::new_v4(), root.path()).unwrap();
        let db = project_db_path(indexes.path(), workspace.project_id);
        let mut state = WorkerState::default();
        let revision = AtomicU64::new(1);
        std::fs::write(root.path().join("lib.rs"), "pub fn old() {}\n").unwrap();
        let old = index_once(&mut state, &workspace, &db, &revision, 1)
            .unwrap()
            .ready;
        std::fs::write(root.path().join("lib.rs"), "pub fn new() {}\n").unwrap();
        let current = index_once(&mut state, &workspace, &db, &revision, 1)
            .unwrap()
            .ready;
        let connection = rusqlite::Connection::open(&db).unwrap();
        connection.execute(
            "UPDATE cg_generation_detail SET published_at=?3 WHERE workspace_id=?1 AND generation=?2",
            rusqlite::params![workspace.workspace_id.to_string(), old.generation,
                (Utc::now() - chrono::Duration::days(31)).to_rfc3339()],
        ).unwrap();
        drop(connection);
        let plan = enforce(indexes.path()).unwrap();
        assert_eq!(plan.prune, vec![(workspace.workspace_id, old.generation)]);
        let mut store = rsi_codegraph::CodegraphStore::open(&db, workspace.project_id).unwrap();
        assert_eq!(
            store.current_ready(workspace.workspace_id).unwrap(),
            current
        );
        assert_eq!(store.generation_details().unwrap().len(), 1);
    }

    #[test]
    fn count_boundary_keeps_current_and_nineteen_prior() {
        use crate::codegraph::{
            RegisteredWorkspace,
            worker::{WorkerState, index_once, project_db_path},
        };
        use std::sync::atomic::AtomicU64;
        let indexes = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let workspace = RegisteredWorkspace::primary(Uuid::new_v4(), root.path()).unwrap();
        let db = project_db_path(indexes.path(), workspace.project_id);
        let mut state = WorkerState::default();
        let revision = AtomicU64::new(1);
        let mut current = None;
        for generation in 1..=21 {
            std::fs::write(
                root.path().join("lib.rs"),
                format!("pub fn version_{generation}() {{}}\n"),
            )
            .unwrap();
            current = Some(
                index_once(&mut state, &workspace, &db, &revision, 1)
                    .unwrap()
                    .ready,
            );
        }
        let mut store = rsi_codegraph::CodegraphStore::open(&db, workspace.project_id).unwrap();
        let details = store.generation_details().unwrap();
        assert_eq!(details.len(), MAX_READY_GENERATIONS);
        assert_eq!(details.first().unwrap().generation, 2);
        assert_eq!(
            store.current_ready(workspace.workspace_id).unwrap(),
            current.unwrap()
        );
    }

    #[test]
    fn current_survives_count_age_and_ceiling() {
        let now = Utc::now();
        let workspace = Uuid::new_v4();
        let rows = (1..=24)
            .map(|generation| DetailedGeneration {
                workspace_id: workspace,
                generation,
                published_at: now - chrono::Duration::days(24 - generation),
                detailed_bytes: 10,
                current: generation == 24,
            })
            .collect::<Vec<_>>();
        let plan = plan_retention(RetentionPolicy::new(50), now, &rows);
        assert!(plan.prune.contains(&(workspace, 1)));
        assert!(!plan.prune.contains(&(workspace, 24)));
        assert_eq!(plan.prune.len(), 19);
        let tiny = plan_retention(RetentionPolicy::new(1), now, &rows);
        assert_eq!(tiny.protected_over_ceiling_bytes, 9);
        assert!(!tiny.prune.contains(&(workspace, 24)));
    }
}
