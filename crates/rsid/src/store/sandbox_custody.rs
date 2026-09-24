//! V83 custody aggregate persistence. This module owns the SQL-only session
//! link; the Rust/wire `Session` deliberately remains unchanged.

use super::{
    Store,
    row_mappers::{SESSION_COLUMN_COUNT, SESSION_COLUMNS, map_session_row},
    sessions::insert_session_on,
};
// Fixture-only: `insert_session_with_pre_v98_custody` is itself `#[cfg(test)]`,
// so its frozen-shape session insert must not be imported into a normal build.
#[cfg(test)]
use super::sessions::insert_legacy_session_on;
use crate::error::{DaemonError, Result, sandbox_custody_error};
use chrono::{SecondsFormat, Utc};
use rsi_common::types::{
    SandboxCleanupState, SandboxCustodyErrorCodeV1, SandboxCustodyErrorV1,
    SandboxCustodyRecoveryV1, SandboxCustodyTransitionV1, Session, SessionStatus,
};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use std::path::{Component, Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard, TryLockError};
use uuid::Uuid;

const STARTUP_AGGREGATE_PAGE_SQL: &str = "SELECT custody_id,sandbox_root,custody_id FROM sandbox_custody_roots WHERE custody_id>?1 ORDER BY custody_id LIMIT ?2";
const STARTUP_ROOTED_PAGE_SQL: &str = "SELECT sandbox_root,sandbox_root,NULL FROM sessions INDEXED BY idx_sessions_startup_unlinked_root WHERE sandbox_root IS NOT NULL AND sandbox_custody_id IS NULL AND sandbox_root>?1 GROUP BY sandbox_root ORDER BY sandbox_root LIMIT ?2";
const STARTUP_ROOTLESS_PAGE_SQL: &str = "SELECT id,NULL,NULL FROM sessions INDEXED BY idx_sessions_startup_unlinked_rootless WHERE sandbox_root IS NULL AND sandbox_custody_id IS NULL AND id>?1 ORDER BY id LIMIT ?2";

fn timestamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn prepared_reclaim_for_owner_tuple_on(
    conn: &Connection,
    owner: Uuid,
    sandbox_root: Option<&Path>,
    sandbox_branch: Option<&str>,
) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM sandbox_custody_roots root
            JOIN sandbox_target_reclaim_intents intent
              ON intent.custody_id=root.custody_id
             AND intent.generation=root.generation
            WHERE root.owner_session_id=?1 AND root.state='live'
              AND root.sandbox_root=?2 AND root.sandbox_branch=?3
              AND intent.state='Prepared'
        )",
        params![
            owner.to_string(),
            sandbox_root.map(|path| path.to_string_lossy().to_string()),
            sandbox_branch,
        ],
        |row| row.get(0),
    )
}

/// Root-local admission locks make filesystem validation and the associated
/// generation-CAS one critical section without serializing unrelated roots.
/// The registry lock is held only while locating a root lock; effect work is
/// never performed while either it or SQLite is locked.
/// A fixed number of stripes bounds synchronization storage for the daemon's
/// entire lifetime.  Different custody IDs can deliberately collide, but all
/// operations for one ID always select the same stripe.
pub(crate) const CUSTODY_ROOT_LOCK_SHARDS: usize = 64;
static ROOT_LOCKS: LazyLock<[Mutex<()>; CUSTODY_ROOT_LOCK_SHARDS]> =
    LazyLock::new(|| std::array::from_fn(|_| Mutex::new(())));

const ADMISSION_PATH_MAX_BYTES: usize = 4096;
const ADMISSION_PATH_MAX_COMPONENTS: usize = 256;

fn normalized_absolute_path(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() || path.as_os_str().as_encoded_bytes().len() > ADMISSION_PATH_MAX_BYTES {
        return None;
    }
    let mut normalized = PathBuf::from("/");
    let mut components = 0_usize;
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(segment) => {
                components = components.checked_add(1)?;
                if components > ADMISSION_PATH_MAX_COMPONENTS {
                    return None;
                }
                normalized.push(segment);
            }
            Component::ParentDir => {
                if normalized == Path::new("/") {
                    return None;
                }
                normalized.pop();
            }
            Component::Prefix(_) => return None,
        }
    }
    Some(normalized)
}

fn bounded_admission_path_variants(path: &Path) -> Option<Vec<PathBuf>> {
    let raw = normalized_absolute_path(path)?;
    let canonical = crate::path_safety::canonicalize_non_strict(path).ok()?;
    let canonical = normalized_absolute_path(&canonical)?;
    if canonical == raw {
        Some(vec![raw])
    } else {
        Some(vec![raw, canonical])
    }
}
#[cfg(test)]
static FAIL_NEXT_EFFECT_RELEASES: AtomicUsize = AtomicUsize::new(0);

#[cfg(test)]
pub(crate) fn fail_next_effect_releases(count: usize) {
    FAIL_NEXT_EFFECT_RELEASES.store(count, Ordering::Release);
}

pub(crate) struct CustodyRootGuard(MutexGuard<'static, ()>);

pub(crate) fn lock_custody_root(custody_id: Uuid) -> CustodyRootGuard {
    CustodyRootGuard(
        ROOT_LOCKS[custody_root_lock_shard(custody_id)]
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
}

/// Acquire one custody stripe without waiting. Maintenance callers use this
/// to preserve a hard pass-duration bound while ordinary custody effects keep
/// the blocking guard and their existing serialization contract.
pub(crate) fn try_lock_custody_root(custody_id: Uuid) -> Option<CustodyRootGuard> {
    match ROOT_LOCKS[custody_root_lock_shard(custody_id)].try_lock() {
        Ok(guard) => Some(CustodyRootGuard(guard)),
        Err(TryLockError::Poisoned(error)) => Some(CustodyRootGuard(error.into_inner())),
        Err(TryLockError::WouldBlock) => None,
    }
}

pub(crate) fn custody_root_lock_shard(custody_id: Uuid) -> usize {
    let bytes = custody_id.as_bytes();
    let mut hash = 0usize;
    for byte in bytes {
        hash = hash.wrapping_mul(31).wrapping_add(*byte as usize);
    }
    hash % CUSTODY_ROOT_LOCK_SHARDS
}

/// Check the current reclaim gate inside the writer transaction that would
/// otherwise grant or transfer custody authority. Staged and Deleting rows
/// have detached the old inode and intentionally permit fresh effects.
pub(crate) fn refuse_prepared_reclaim_on(
    tx: &Transaction<'_>,
    custody_id: Uuid,
    generation: u64,
    transition: SandboxCustodyTransitionV1,
) -> Result<()> {
    let prepared: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sandbox_target_reclaim_intents
         WHERE custody_id=?1 AND generation=?2 AND state='Prepared')",
        params![custody_id.to_string(), generation as i64],
        |row| row.get(0),
    )?;
    if prepared {
        return Err(sandbox_custody_error(SandboxCustodyErrorV1 {
            version: 1,
            code: SandboxCustodyErrorCodeV1::ReclaimPrepared,
            session_id: None,
            transition,
            retryable: true,
            recovery: SandboxCustodyRecoveryV1::RetryAfterReconcile,
        }));
    }
    Ok(())
}

/// Pair with preparation's unbound-successor check in the same SQLite writer
/// order. The caller must hold an IMMEDIATE transaction until reservation is
/// committed, so a gate cannot be inserted between this read and the child.
pub(crate) fn prepared_reclaim_for_successor_on(
    tx: &Transaction<'_>,
    predecessor: Uuid,
    sandbox_root: &Path,
    sandbox_branch: &str,
) -> Result<bool> {
    tx.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM sandbox_custody_roots r
            JOIN sandbox_target_reclaim_intents i
              ON i.custody_id=r.custody_id AND i.generation=r.generation
            WHERE r.owner_session_id=?1 AND r.state='live'
              AND r.sandbox_root=?2 AND r.sandbox_branch=?3
              AND i.state='Prepared'
        )",
        params![
            predecessor.to_string(),
            sandbox_root.to_string_lossy().as_ref(),
            sandbox_branch
        ],
        |row| row.get(0),
    )
    .map_err(Into::into)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CustodyCause {
    FreshLaunch,
    AgentSpawnChild,
    Rotation,
    Retry,
    AgentFresh,
    CodexAppServerReplacement,
    RecursiveLive,
    StartupReconciliation,
    Purge,
    CleanupFailure,
    EffectRevalidation,
}

/// Store-owned rotation authority proof. Callers can retain and return this
/// value, but only this module can inspect or construct its representation.
/// Unknown future `Session` fields remain fenced because the representation
/// begins with the complete serialized struct and removes only the explicitly
/// documented non-authority fields below.
pub(crate) struct RotationAuthorityFence {
    predecessor_id: Uuid,
    /// Status the predecessor held at capture: `Completed` for a live
    /// rotation, `Failed` only for a restart-recovered open rotation intent.
    /// Every later fence check and the archive/restore transitions key on it.
    predecessor_status: SessionStatus,
    session_authority: Vec<u8>,
    sandbox_custody_id: Option<Uuid>,
    model_invocation_id: Option<Uuid>,
    legacy_primary_tag: String,
}

pub(crate) enum RotationAuthorityCapture {
    Captured(RotationAuthorityFence),
    Missing,
    Changed,
}

/// Store-owned proof of the complete Failed source row authenticated before an
/// automatic retry can choose a successor identity.  The C5 transaction is
/// allowed to change only its documented retry counters and timestamp before
/// this fence is consumed by the custody bind.
pub(crate) struct RetryAuthorityFence {
    source_session_id: Uuid,
    session_authority_after_c5: Vec<u8>,
    sandbox_custody_id: Option<Uuid>,
    model_invocation_id: Option<Uuid>,
    legacy_primary_tag: String,
    retry_attempt: u8,
    max_retries: u8,
}

pub(crate) enum RetryAuthorityCapture {
    Captured(RetryAuthorityFence),
    Missing,
    Changed,
}

pub(crate) enum ArchivedRotationRestoration {
    Restored(Session),
    Refused,
}

// This classification is shared by rotation custody and manager succession.
// Keep all previously fenced fields fenced. Status is checked separately at
// each transition. The non-authority set contains projections and result
// telemetry; it cannot select execution identity, lineage, routing, provider,
// cwd, inherited prompt, retry/rotation policy, or custody. In particular,
// queued_turn_count is provider result telemetry, not an authority claim: it
// also gates a later manager-notice idle wake, which checks its live value.
// The exhaustive destructure in the test fails compilation when Session grows.
macro_rules! rotation_session_field_classes {
    (authority: [$($authority:ident),* $(,)?], non_authority: [$($non_authority:ident),* $(,)?],) => {
        #[cfg(test)]
        const ROTATION_AUTHORITY_SESSION_FIELDS: &[&str] = &[$(stringify!($authority)),*];
        const ROTATION_NON_AUTHORITY_SESSION_FIELDS: &[&str] = &[$(stringify!($non_authority)),*];

        #[cfg(test)]
        fn assert_rotation_session_fields_exhaustive(session: &Session) {
            let Session { $($authority: _,)* $($non_authority: _,)* } = session;
        }
    };
}

rotation_session_field_classes! {
    authority: [
        id, session_kind, provider, rotation_depth, retry_attempt, max_retries,
        created_at, pinned_at, testing_needed_at, rotation_disabled_at, query,
        agent_role, epic_spawn_ordinal, working_dir, git_branch, model,
        claude_session_id, project_id, continued_from, parent_id, lead_session_id,
        handoff_filepath, active_task, group_id, tag, tags, scheduled_job_id,
        pipeline_artifact, workflow_id, workflow_id_override, pending_question,
        pending_archive, effort, issue_identifier, issue_url, issue_tracker_id,
        rating, harness_version_hash, sandbox_kind, sandbox_root, sandbox_branch,
        sandbox_cleanup_state, is_eval, capability_class, topology_node_id,
        topology_iteration, provider_cli_version, provider_capabilities,
    ],
    non_authority: [
        status, updated_at, context_usage_confidence, title, description,
        short_summary, stop_reason, cost_usd, duration_ms, num_turns,
        input_tokens, output_tokens, context_window, resolved_context_budget,
        total_input_tokens, total_output_tokens, total_cache_creation_tokens,
        total_cache_read_tokens, daemon_input_tokens, daemon_output_tokens,
        context_fill_pct, test_passed, clippy_passed, turn_count, retry_count,
        approval_wait_ms, approval_started_at, work_time_ms, thinking_tokens,
        service_tier, cache_creation_1h_tokens, cache_creation_5m_tokens,
        permission_denial_count, subagent_stats_json, queued_turn_count,
        terminal_reason,
    ],
}

#[cfg(test)]
mod session_authority_classification_tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn every_session_field_has_exactly_one_rotation_authority_class() {
        let session = crate::store::tests::make_test_session();
        assert_rotation_session_fields_exhaustive(&session);
        let authority: BTreeSet<_> = ROTATION_AUTHORITY_SESSION_FIELDS.iter().copied().collect();
        let non_authority: BTreeSet<_> = ROTATION_NON_AUTHORITY_SESSION_FIELDS
            .iter()
            .copied()
            .collect();
        assert_eq!(authority.len(), ROTATION_AUTHORITY_SESSION_FIELDS.len());
        assert_eq!(
            non_authority.len(),
            ROTATION_NON_AUTHORITY_SESSION_FIELDS.len()
        );
        assert!(authority.is_disjoint(&non_authority));
        let classified: BTreeSet<_> = authority.union(&non_authority).copied().collect();
        let serialized = serde_json::to_value(&session).unwrap();
        let serialized: BTreeSet<_> = serialized
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert!(serialized.is_subset(&classified));
        assert!(non_authority.contains("queued_turn_count"));
        assert!(authority.contains("model"));
        assert!(authority.contains("sandbox_root"));
    }

    #[test]
    fn completed_rotation_fence_accepts_late_result_telemetry_but_rejects_model_change() {
        let store = Store::open_in_memory().unwrap();
        let mut session = crate::store::tests::make_test_session();
        session.status = SessionStatus::Completed;
        store.insert_session(&session).unwrap();
        let snapshot = store.get_session(session.id).unwrap().unwrap();
        let RotationAuthorityCapture::Captured(fence) = store
            .capture_completed_rotation_authority(&snapshot)
            .unwrap()
        else {
            panic!("completed predecessor should capture its authority");
        };
        let mut telemetry = snapshot.clone();
        telemetry.thinking_tokens = Some(8);
        telemetry.service_tier = Some("standard".into());
        telemetry.cache_creation_1h_tokens = Some(17);
        telemetry.cache_creation_5m_tokens = Some(2);
        telemetry.permission_denial_count = Some(0);
        telemetry.subagent_stats_json = Some("{\"spawned\":0}".into());
        telemetry.queued_turn_count = Some(0);
        telemetry.terminal_reason = Some("completed".into());
        store.update_session_metadata(&telemetry).unwrap();
        assert!(matches!(
            store
                .capture_completed_rotation_authority(&snapshot)
                .unwrap(),
            RotationAuthorityCapture::Captured(_)
        ));
        assert!(
            store
                .completed_rotation_authority_matches(&fence, session.id)
                .unwrap()
        );
        store
            .conn
            .execute(
                "UPDATE sessions SET model='changed' WHERE id=?1",
                [session.id.to_string()],
            )
            .unwrap();
        assert!(
            !store
                .completed_rotation_authority_matches(&fence, session.id)
                .unwrap()
        );
    }
}

impl CustodyCause {
    fn as_str(self) -> &'static str {
        match self {
            Self::FreshLaunch => "fresh_launch",
            Self::AgentSpawnChild => "agent_spawn_child",
            Self::Rotation => "rotation",
            Self::Retry => "retry",
            Self::AgentFresh => "agent_fresh",
            Self::CodexAppServerReplacement => "codex_app_server_replacement",
            Self::RecursiveLive => "recursive_live",
            Self::StartupReconciliation => "startup_reconciliation",
            Self::Purge => "purge",
            Self::CleanupFailure => "cleanup_failure",
            Self::EffectRevalidation => "effect_revalidation",
        }
    }

    pub(crate) fn transition(self) -> SandboxCustodyTransitionV1 {
        match self {
            Self::FreshLaunch => SandboxCustodyTransitionV1::FreshLaunch,
            Self::AgentSpawnChild => SandboxCustodyTransitionV1::AgentSpawnChild,
            Self::Rotation => SandboxCustodyTransitionV1::Rotation,
            Self::Retry => SandboxCustodyTransitionV1::Retry,
            Self::AgentFresh => SandboxCustodyTransitionV1::AgentFresh,
            Self::CodexAppServerReplacement => {
                SandboxCustodyTransitionV1::CodexAppServerReplacement
            }
            Self::RecursiveLive => SandboxCustodyTransitionV1::RecursiveLive,
            Self::StartupReconciliation => SandboxCustodyTransitionV1::StartupReconciliation,
            Self::Purge => SandboxCustodyTransitionV1::Purge,
            Self::CleanupFailure => SandboxCustodyTransitionV1::CleanupFailure,
            Self::EffectRevalidation => SandboxCustodyTransitionV1::EffectRevalidation,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct NewCustodyRoot {
    pub custody_id: Uuid,
    pub canonical_repo_dir: String,
    pub sandbox_root: String,
    pub sandbox_branch: String,
    pub repository_identity: String,
    pub source_commit: String,
    pub cause: CustodyCause,
}

#[derive(Debug, Clone)]
pub(crate) enum SessionCustodyBinding {
    Ordinary,
    New(NewCustodyRoot),
    Reuse {
        custody_id: Uuid,
        generation: u64,
        cause: CustodyCause,
    },
    Transfer {
        custody_id: Uuid,
        from_session_id: Uuid,
        generation: u64,
        cause: CustodyCause,
        origin_session_id: Option<Uuid>,
        scheduled_job_id: Option<Uuid>,
    },
}

/// Opaque-to-establishment fence for a rotation successor that was already
/// bound.  Unlike direct-launch failure, a transferred successor remains the
/// live root owner while Failed; no backward transfer is possible.
#[derive(Debug, Clone, Copy)]
pub(crate) enum BoundRotationCustody {
    Ordinary {
        predecessor_id: Uuid,
    },
    Transfer {
        custody_id: Uuid,
        predecessor_id: Uuid,
        generation: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EffectReservation {
    pub custody_id: Uuid,
    pub generation: u64,
    pub boot_id: Uuid,
}

/// Immutable persisted facts that the custody service must authenticate again
/// against the filesystem and git.  A caller never supplies these values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PersistedCustody {
    pub custody_id: Uuid,
    /// The Session that allocated this immutable worktree at generation one.
    /// Ownership can transfer; this identity cannot.
    pub allocation_session_id: Uuid,
    /// The immutable UUID that names this worktree allocation on disk.
    /// A restored session receives a new allocation identity while retaining
    /// its historical session identity.
    pub allocation_id: Uuid,
    pub owner_session_id: Uuid,
    pub generation: u64,
    pub canonical_repo_dir: String,
    pub sandbox_root: String,
    pub sandbox_branch: String,
    pub repository_identity: String,
    pub source_commit: String,
}

/// One deterministic startup work item.  The key is deliberately stable
/// across restarts: a non-null root is reconciled as one group, while each
/// rootless Session remains an independent item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StartupCustodyGroup {
    pub key: String,
    pub sandbox_root: Option<String>,
    /// Present for every durable aggregate, including terminal aggregates
    /// whose linked Session rows have already had their root nulled.
    pub custody_id: Option<Uuid>,
}

#[derive(Debug, Clone)]
pub(crate) struct StartupCustodyRoot {
    pub custody_id: Uuid,
    pub allocation_id: Uuid,
    pub owner_session_id: Option<Uuid>,
    pub state: String,
    pub generation: u64,
    pub canonical_repo_dir: String,
    pub sandbox_root: String,
    pub sandbox_branch: String,
    pub repository_identity: String,
    pub source_commit: String,
}

#[derive(Debug, Clone)]
pub(crate) struct LegacyStartupRoot {
    pub custody_id: Uuid,
    pub allocation_session_id: Uuid,
    pub canonical_repo_dir: String,
    pub sandbox_root: String,
    pub sandbox_branch: String,
    pub repository_identity: String,
    pub source_commit: String,
    /// Allocation owner followed by every authenticated transfer sink.
    pub lineage: Vec<(Uuid, CustodyCause)>,
}

#[derive(Debug, Clone)]
pub(crate) struct LegacyTerminalRoot {
    pub custody_id: Uuid,
    pub allocation_session_id: Uuid,
    pub canonical_repo_dir: String,
    pub sandbox_root: String,
    pub sandbox_branch: String,
    pub repository_identity: String,
    pub source_commit: String,
    pub cleanup_state: SandboxCleanupState,
}

impl Store {
    pub(crate) fn ordinary_session_path_inside_live_custody_root(
        &self,
        session_id: Uuid,
    ) -> Result<bool> {
        let working_dir = self
            .conn
            .query_row(
                "SELECT working_dir
                 FROM sessions
                 WHERE id=?1
                   AND sandbox_custody_id IS NULL
                   AND sandbox_kind IS NULL
                   AND sandbox_root IS NULL
                   AND sandbox_branch IS NULL
                   AND sandbox_cleanup_state IS NULL",
                [session_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let Some(working_dir) = working_dir else {
            return Ok(false);
        };
        let path = Path::new(&working_dir);
        Ok(self.path_is_inside_live_custody_root(path)?
            || self.path_is_inside_settlement_root(path)?)
    }

    pub(crate) fn path_is_inside_live_custody_root(&self, path: &Path) -> Result<bool> {
        let Some(variants) = bounded_admission_path_variants(path) else {
            return Ok(true);
        };
        for variant in variants {
            for ancestor in variant.ancestors() {
                let ancestor = ancestor.to_string_lossy().into_owned();
                let present: bool = self.conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sandbox_custody_roots
                                   WHERE state='live' AND sandbox_root=?1)",
                    [ancestor],
                    |row| row.get(0),
                )?;
                if present {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// Fail closed when a launch path is equal to or beneath an effect-capable
    /// settlement journal root.  Both the raw normalized spelling and the
    /// canonical identity are checked one ancestor at a time against the V95
    /// root-leading index, so admission stays bounded after roots are purged.
    pub(crate) fn path_is_inside_settlement_root(&self, path: &Path) -> Result<bool> {
        let Some(variants) = bounded_admission_path_variants(path) else {
            return Ok(true);
        };
        for variant in variants {
            for ancestor in variant.ancestors() {
                let ancestor = ancestor.to_string_lossy().into_owned();
                let present: bool = self.conn.query_row(
                    "SELECT EXISTS(
                         SELECT 1 FROM source_worktree_settlement_items
                         WHERE sandbox_root=?1
                           AND phase NOT IN ('refused','unattempted')
                     )",
                    [ancestor],
                    |row| row.get(0),
                )?;
                if present {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// Keyset enumeration for startup custody.  This deliberately includes
    /// Archived and Deleted rows and orphaned live aggregates; `load_sessions`
    /// cannot be used here because it omits both and loads an unbounded corpus.
    pub(crate) fn startup_custody_group_page(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<StartupCustodyGroup>> {
        // Do not collapse the three startup sources into a global UNION: it
        // defeats every source index with a temp sort.  The cursor carries a
        // phase marker and each phase is a direct, deterministic keyset scan.
        let limit = limit.max(1) as i64;
        let after = after.unwrap_or("");
        let (sql, cursor, phase) = if let Some(cursor) = after.strip_prefix("aggregate:") {
            (STARTUP_AGGREGATE_PAGE_SQL, cursor, "aggregate")
        } else if let Some(cursor) = after.strip_prefix("root:") {
            (STARTUP_ROOTED_PAGE_SQL, cursor, "root")
        } else if let Some(cursor) = after.strip_prefix("session:") {
            (STARTUP_ROOTLESS_PAGE_SQL, cursor, "session")
        } else {
            (STARTUP_AGGREGATE_PAGE_SQL, "", "aggregate")
        };
        let mut statement = self.conn.prepare(sql)?;
        let mut groups = statement
            .query_map(params![cursor, limit], |row| {
                let identity: String = row.get(0)?;
                Ok(StartupCustodyGroup {
                    key: format!("{phase}:{identity}"),
                    sandbox_root: row.get(1)?,
                    custody_id: row
                        .get::<_, Option<String>>(2)?
                        .map(|value| {
                            Uuid::parse_str(&value).map_err(|error| {
                                rusqlite::Error::FromSqlConversionFailure(
                                    2,
                                    rusqlite::types::Type::Text,
                                    Box::new(error),
                                )
                            })
                        })
                        .transpose()?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DaemonError::Database)?;
        // A short page completes its phase.  Probe the next indexed source in
        // the same call so a cursor never terminates startup prematurely at a
        // phase boundary.
        if groups.len() < limit as usize && phase == "aggregate" {
            groups.extend(
                self.startup_custody_group_page(Some("root:"), (limit as usize) - groups.len())?,
            );
        } else if groups.len() < limit as usize && phase == "root" {
            groups.extend(
                self.startup_custody_group_page(Some("session:"), (limit as usize) - groups.len())?,
            );
        }
        Ok(groups)
    }

    #[cfg(test)]
    pub(crate) fn startup_custody_group_page_query_plan(&self, phase: &str) -> Result<Vec<String>> {
        let sql = match phase {
            "aggregate" => STARTUP_AGGREGATE_PAGE_SQL,
            "root" => STARTUP_ROOTED_PAGE_SQL,
            "session" => STARTUP_ROOTLESS_PAGE_SQL,
            _ => return Err(DaemonError::Store("unknown startup group phase".into())),
        };
        self.conn
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?
            .query_map(params!["", 64_i64], |row| row.get(3))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DaemonError::Database)
    }

    /// Deterministic participant pages. Existing aggregates are inspected a
    /// page at a time; only legacy reconstruction is allowed to collect a
    /// bounded lineage from these pages.
    pub(crate) fn startup_custody_group_sessions_page(
        &self,
        group: &StartupCustodyGroup,
        after: Option<Uuid>,
        limit: usize,
    ) -> Result<Vec<Session>> {
        let after = after.map(|id| id.to_string()).unwrap_or_default();
        let ids: Vec<String> = if let Some(custody_id) = group.custody_id {
            let root = group.sandbox_root.as_deref().ok_or_else(|| {
                DaemonError::Store("aggregate startup custody group lacks persisted root".into())
            })?;
            // Three independently indexed sources make participant discovery
            // bidirectional.  In particular, a Session named by immutable
            // ownership history remains a participant even when a corrupt
            // SQL link and cached root have both been nulled.
            let mut statement = self.conn.prepare(
                "SELECT id FROM (
                     SELECT id FROM sessions
                      WHERE sandbox_custody_id=?1 AND id>?3
                     UNION
                     SELECT id FROM sessions
                      WHERE sandbox_root=?2 AND id>?3
                     UNION
                     SELECT to_owner_session_id AS id
                       FROM sandbox_custody_events
                      WHERE custody_id=?1
                        AND to_owner_session_id IS NOT NULL
                        AND to_owner_session_id>?3
                 ) ORDER BY id LIMIT ?4",
            )?;
            statement
                .query_map(
                    params![custody_id.to_string(), root, after, limit.max(1) as i64],
                    |row| row.get(0),
                )?
                .collect::<std::result::Result<Vec<_>, _>>()?
        } else if let Some(root) = &group.sandbox_root {
            let mut statement = self.conn.prepare(
                "SELECT id FROM sessions WHERE sandbox_root=?1 AND sandbox_custody_id IS NULL
                 AND id > ?2 ORDER BY id LIMIT ?3",
            )?;
            statement
                .query_map(params![root, after, limit.max(1) as i64], |row| row.get(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?
        } else {
            let id = group
                .key
                .strip_prefix("session:")
                .ok_or_else(|| DaemonError::Store("invalid rootless startup custody key".into()))?;
            if after.is_empty() {
                vec![id.to_owned()]
            } else {
                Vec::new()
            }
        };
        ids.into_iter()
            .map(|id| {
                let id =
                    Uuid::parse_str(&id).map_err(|error| DaemonError::Store(error.to_string()))?;
                self.get_session(id)?.ok_or_else(|| {
                    DaemonError::Store(
                        "startup custody session disappeared during enumeration".into(),
                    )
                })
            })
            .collect()
    }

    pub(crate) fn startup_session_has_custody_link(
        &self,
        session_id: Uuid,
        custody_id: Uuid,
    ) -> Result<bool> {
        self.conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1 AND sandbox_custody_id=?2)",
                params![session_id.to_string(), custody_id.to_string()],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    /// Reserved successors can survive a crash before their custody bind.
    /// Settle only the exact unverified Starting shape while the predecessor
    /// still owns the same live root. The startup caller pages by immutable ID.
    pub(crate) fn startup_unbound_successor_page(
        &self,
        after: Option<Uuid>,
        limit: usize,
    ) -> Result<Vec<Uuid>> {
        let mut statement = self.conn.prepare(
            "SELECT s.id FROM sessions s
             JOIN sandbox_custody_roots r ON r.owner_session_id=s.continued_from
               AND r.state='live' AND r.sandbox_root=s.sandbox_root
               AND r.sandbox_branch=s.sandbox_branch
             JOIN session_execution_projections p ON p.session_id=s.id
             JOIN model_invocations i ON i.id=s.model_invocation_id AND i.session_id=s.id
             WHERE s.id>?1 AND s.status='Starting' AND s.sandbox_custody_id IS NULL
               AND s.sandbox_kind='GitWorktree' AND s.sandbox_cleanup_state='Live'
               AND s.working_dir=r.canonical_repo_dir
               AND p.execution_state='live_sandboxed' AND p.freshness='unverified'
               AND p.effective_cwd IS NULL AND p.custody_id IS NULL
               AND p.custody_generation IS NULL AND p.validated_at IS NULL
               AND p.error_code IS NULL AND p.canonical_repo_dir=s.working_dir
               AND i.admission_status='admitted' AND i.status='running'
             ORDER BY s.id LIMIT ?2",
        )?;
        statement
            .query_map(
                params![
                    after.map(|id| id.to_string()).unwrap_or_default(),
                    limit.max(1) as i64
                ],
                |row| row.get::<_, String>(0),
            )?
            .map(|row| {
                Uuid::parse_str(&row?).map_err(|error| DaemonError::Store(error.to_string()))
            })
            .collect()
    }

    /// A failed unbound reservation has no custody or effect authority. Its
    /// retained root tuple is history, so it is not a competing claimant when
    /// startup authenticates the predecessor's aggregate.
    pub(crate) fn startup_settled_unbound_successor(
        &self,
        session_id: Uuid,
        owner_id: Uuid,
        custody_id: Uuid,
    ) -> Result<bool> {
        self.conn
            .query_row(
                "SELECT EXISTS(
               SELECT 1 FROM sessions s
               JOIN sandbox_custody_roots r ON r.custody_id=?3
               JOIN session_execution_projections p ON p.session_id=s.id
               WHERE s.id=?1 AND s.continued_from=?2 AND r.owner_session_id=?2
                 AND r.state='live' AND s.status='Failed'
                 AND s.stop_reason='sandbox_custody:ownership_missing'
                 AND s.sandbox_custody_id IS NULL
                 AND s.sandbox_kind='GitWorktree' AND s.sandbox_cleanup_state='Live'
                 AND s.sandbox_root=r.sandbox_root AND s.sandbox_branch=r.sandbox_branch
                 AND s.working_dir=r.canonical_repo_dir
                 AND p.execution_state='invalid' AND p.freshness='invalid'
                 AND p.effective_cwd IS NULL AND p.custody_id IS NULL
                 AND p.custody_generation IS NULL AND p.error_code='ownership_missing'
             )",
                params![
                    session_id.to_string(),
                    owner_id.to_string(),
                    custody_id.to_string()
                ],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    /// Every durable participant must be named by the immutable ownership
    /// chain. A SQL link alone is only a cache/projection relation and is not
    /// authority to restore an executable session.
    /// Return the one immutable generation at which a Session became owner.
    /// Duplicate owner events are malformed history rather than an ambiguity
    /// that startup may choose between.
    pub(crate) fn startup_session_ownership_generation(
        &self,
        session_id: Uuid,
        custody_id: Uuid,
    ) -> Result<Option<u64>> {
        let mut statement = self.conn.prepare(
            "SELECT to_generation FROM sandbox_custody_events
                  WHERE custody_id=?1 AND to_owner_session_id=?2
                    AND event_kind IN ('allocated','transferred')
                  ORDER BY sequence LIMIT 2",
        )?;
        let generations = statement
            .query_map(
                params![custody_id.to_string(), session_id.to_string()],
                |row| row.get::<_, i64>(0),
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        match generations.as_slice() {
            [] => Ok(None),
            [generation] if *generation > 0 => Ok(Some(*generation as u64)),
            _ => Err(DaemonError::Store(
                "sandbox custody ownership event generation is ambiguous".into(),
            )),
        }
    }

    pub(crate) fn startup_session_projection_matches(
        &self,
        session_id: Uuid,
        custody_id: Uuid,
        generation: u64,
        execution_state: &str,
    ) -> Result<bool> {
        self.conn
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM session_execution_projections
                     WHERE session_id=?1 AND custody_id=?2
                       AND custody_generation=?3 AND execution_state=?4
                )",
                params![
                    session_id.to_string(),
                    custody_id.to_string(),
                    generation as i64,
                    execution_state,
                ],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    /// Custody-invalid Failed rows remain visible history, but must never be
    /// selected as restart retry candidates.  This reads durable Session state
    /// rather than relying on a process-local restore filter.
    pub(crate) fn startup_retry_eligible_sessions(
        &self,
    ) -> Result<std::collections::HashSet<Uuid>> {
        let mut statement = self.conn.prepare(
            "SELECT s.id FROM sessions s
             JOIN session_execution_projections p ON p.session_id=s.id
             LEFT JOIN sandbox_custody_roots r ON r.custody_id=p.custody_id
             WHERE s.status='Failed'
               AND (s.stop_reason IS NULL OR s.stop_reason NOT GLOB 'sandbox_custody:*')
               AND NOT EXISTS (
                    SELECT 1 FROM source_worktree_settlement_items i
                    WHERE (i.session_id=s.id OR i.custody_id=p.custody_id)
                      AND i.phase NOT IN ('refused','unattempted')
               )
               AND (
                    (p.execution_state='ordinary_unsandboxed' AND p.freshness='verified')
                 OR (p.execution_state='live_sandboxed' AND p.freshness='verified'
                     AND r.state='live' AND r.validation_state='verified'
                     AND r.owner_session_id=s.id AND r.generation=p.custody_generation)
               )",
        )?;
        statement
            .query_map([], |row| row.get::<_, String>(0))?
            .map(|id| Uuid::parse_str(&id?).map_err(|error| DaemonError::Store(error.to_string())))
            .collect()
    }

    pub(crate) fn startup_custody_has_settlement_fence(&self, custody_id: Uuid) -> Result<bool> {
        self.conn
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM source_worktree_settlement_items
                    WHERE custody_id=?1
                      AND phase NOT IN ('refused','unattempted')
                 )",
                [custody_id.to_string()],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    pub(crate) fn startup_custody_root_for_sandbox_root(
        &self,
        sandbox_root: &str,
    ) -> Result<Option<StartupCustodyRoot>> {
        self.conn
            .query_row(
                "SELECT custody_id,allocation_id,owner_session_id,state,generation,canonical_repo_dir,sandbox_root,sandbox_branch,repository_identity,source_commit FROM sandbox_custody_roots WHERE sandbox_root=?1",
                [sandbox_root],
                |row| {
                    let custody_id: String = row.get(0)?;
                    let owner: Option<String> = row.get(2)?;
                    Ok(StartupCustodyRoot {
                        custody_id: Uuid::parse_str(&custody_id).map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                0,
                                rusqlite::types::Type::Text,
                                Box::new(error),
                            )
                        })?,
                        allocation_id: Uuid::parse_str(&row.get::<_, String>(1)?).map_err(
                            |error| {
                                rusqlite::Error::FromSqlConversionFailure(
                                    1,
                                    rusqlite::types::Type::Text,
                                    Box::new(error),
                                )
                            },
                        )?,
                        owner_session_id: owner
                            .map(|value| {
                                Uuid::parse_str(&value).map_err(|error| {
                                    rusqlite::Error::FromSqlConversionFailure(
                                        2,
                                        rusqlite::types::Type::Text,
                                        Box::new(error),
                                    )
                                })
                        })
                            .transpose()?,
                        state: row.get(3)?,
                        generation: row.get::<_, i64>(4)? as u64,
                        canonical_repo_dir: row.get(5)?,
                        sandbox_root: row.get(6)?,
                        sandbox_branch: row.get(7)?,
                        repository_identity: row.get(8)?,
                        source_commit: row.get(9)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    /// Read only a currently executable custody aggregate linked to `session`.
    /// Historical, unverified, and foreign-owner rows are deliberately absent
    /// from this query so callers cannot convert them into a cwd authority.
    pub(crate) fn live_custody_for_session(&self, session_id: Uuid) -> Result<PersistedCustody> {
        self.conn
            .query_row(
                "SELECT r.custody_id,e.to_owner_session_id,r.allocation_id,r.owner_session_id,r.generation,r.canonical_repo_dir,r.sandbox_root,r.sandbox_branch,r.repository_identity,r.source_commit
                 FROM sandbox_custody_roots r
                 JOIN sessions s ON s.sandbox_custody_id=r.custody_id
                 JOIN sandbox_custody_events e ON e.custody_id=r.custody_id
                   AND e.sequence=1 AND e.event_kind='allocated' AND e.to_generation=1
                   AND e.to_owner_session_id IS NOT NULL
                 WHERE s.id=?1 AND r.owner_session_id=?1 AND r.state='live' AND r.validation_state='verified' AND r.validated_generation=r.generation",
                [session_id.to_string()],
                |row| {
                    let custody_id: String = row.get(0)?;
                    let owner_session_id: String = row.get(3)?;
                    Ok(PersistedCustody {
                        custody_id: Uuid::parse_str(&custody_id).map_err(|err| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(err)))?,
                        allocation_session_id: Uuid::parse_str(&row.get::<_, String>(1)?).map_err(|err| rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, Box::new(err)))?,
                        allocation_id: Uuid::parse_str(&row.get::<_, String>(2)?).map_err(|err| rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(err)))?,
                        owner_session_id: Uuid::parse_str(&owner_session_id).map_err(|err| rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(err)))?,
                        generation: row.get::<_, i64>(4)? as u64,
                        canonical_repo_dir: row.get(5)?,
                        sandbox_root: row.get(6)?,
                        sandbox_branch: row.get(7)?,
                        repository_identity: row.get(8)?,
                        source_commit: row.get(9)?,
                    })
                },
            )
            .optional()?
            .ok_or_else(|| DaemonError::Store("live sandbox custody ownership is missing".into()))
    }

    /// Return the startup classifier's durable refusal for a legacy worktree
    /// row that has no custody aggregate. This is diagnostic only: callers
    /// must still refuse the launch rather than treating the cached tuple as
    /// authority.
    pub(crate) fn unlinked_invalid_custody_error(
        &self,
        session_id: Uuid,
    ) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT p.error_code
                 FROM sessions s
                 JOIN session_execution_projections p ON p.session_id=s.id
                 WHERE s.id=?1
                   AND s.sandbox_custody_id IS NULL
                   AND p.execution_state='invalid'
                   AND p.freshness='invalid'
                   AND p.custody_id IS NULL
                   AND p.custody_generation IS NULL
                   AND p.error_code IS NOT NULL",
                [session_id.to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    /// Direct fresh/ordinary path. The session insert, SQL-only link, root,
    /// event, and cached projection commit or roll back together.
    pub(crate) fn insert_session_with_custody(
        &mut self,
        session: &Session,
        binding: SessionCustodyBinding,
    ) -> Result<()> {
        let _root_guard = binding_custody_id(&binding).map(lock_custody_root);
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        insert_session_on(&tx, session)?;
        bind_on(&tx, session.id, binding)?;
        tx.commit()?;
        Ok(())
    }

    /// Test-only pre-V99 twin of [`Store::insert_session_with_custody`], for
    /// migration-chain fixtures that deliberately run below the current catalog.
    ///
    /// `sandbox_custody_roots.allocation_id` arrives with V98, while the
    /// operator-identity Session suffix arrives with V99
    /// (`apply_sandbox_allocation_identity_v98_migration`), and
    /// `rewind_store_to_schema_version` drops it again. The production insert
    /// names the current catalog unconditionally and must stay that way: a
    /// deployed store that is missing a current column has to fail closed, not
    /// silently write a stale row shape. Fixtures therefore select the frozen
    /// historical shape here instead.
    ///
    /// A root seeded here carries no allocation identity, exactly like a
    /// genuinely pre-V98 deployed row, so V98 backfills it from the sequence-one
    /// `allocated` event's owner when the fixture migrates forward.
    #[cfg(test)]
    pub(crate) fn insert_session_with_pre_v99_custody(
        &mut self,
        session: &Session,
        root: NewCustodyRoot,
    ) -> Result<()> {
        let schema_version: i32 = self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))?;
        // V83 introduced the custody aggregate; V99 is the first schema where
        // the current Session writer can target the catalog directly.
        if !(83..=98).contains(&schema_version) {
            return Err(DaemonError::Store(format!(
                "pre-V99 custody fixture insert requires a V83..=V98 store, found V{schema_version}"
            )));
        }
        let _root_guard = lock_custody_root(root.custody_id);
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        insert_legacy_session_on(&tx, session)?;
        ensure_session_matches_root(
            &tx,
            session.id,
            &root.canonical_repo_dir,
            &root.sandbox_root,
            &root.sandbox_branch,
        )?;
        if schema_version < 98 {
            insert_pre_v98_new_root(&tx, session.id, root)?;
        } else {
            insert_new_root(&tx, session.id, root)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Restore an archived, historically-purged sandbox session into a newly
    /// allocated custody root. The old root remains immutable history; this
    /// creates a distinct generation-one root for the same session identity.
    ///
    /// The complete transition, including the archived -> completed status
    /// change, is one transaction. This prevents an unarchived session from
    /// ever being visible with a purged sandbox tuple that `continue_session`
    /// must refuse.
    pub(crate) fn restore_archived_session_with_fresh_custody(
        &mut self,
        session_id: Uuid,
        binding: SessionCustodyBinding,
    ) -> Result<Session> {
        if self.source_worktree_settlement_blocks_unarchive(session_id)? {
            return Err(DaemonError::InvalidParam(
                "settled source worktree is historical and cannot be unarchived".into(),
            ));
        }
        self.verify_archive_cleanup_unarchive_gate(session_id)?;
        let SessionCustodyBinding::New(root) = binding else {
            return Err(DaemonError::Store(
                "archived sandbox restoration requires a fresh custody root".into(),
            ));
        };
        let _root_guard = lock_custody_root(root.custody_id);
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = timestamp();

        // A linked row may be restored only after its old aggregate reached a
        // verified, ownerless Purged terminal state. Pre-V83 rows have no
        // aggregate link, but must still carry the matching historical
        // projection left by `mark_sandbox_purged`.
        let restored = tx.execute(
            "UPDATE sessions
             SET status='Completed',
                 pending_archive=0,
                 working_dir=?1,
                 git_branch=?2,
                 sandbox_kind='GitWorktree',
                 sandbox_root=?3,
                 sandbox_branch=?2,
                 sandbox_cleanup_state='Live',
                 sandbox_custody_id=NULL,
                 updated_at=?4
             WHERE id=?5
               AND status='Archived'
               AND sandbox_kind='GitWorktree'
               AND sandbox_root IS NULL
               AND sandbox_branch IS NULL
               AND sandbox_cleanup_state='Purged'
               AND EXISTS (
                   SELECT 1
                   FROM session_execution_projections p
                   WHERE p.session_id=sessions.id
                     AND p.execution_state='historical_purged'
                     AND p.freshness='verified'
                     AND p.effective_cwd IS NULL
                     AND (
                         sessions.sandbox_custody_id IS NULL
                         AND p.custody_id IS NULL
                         AND p.custody_generation IS NULL
                         OR EXISTS (
                             SELECT 1
                             FROM sandbox_custody_roots old_root
                             WHERE old_root.custody_id=sessions.sandbox_custody_id
                               AND old_root.state='purged'
                               AND old_root.owner_session_id IS NULL
                               AND old_root.validation_state='verified'
                               AND old_root.validated_generation=old_root.generation
                               AND p.custody_id=old_root.custody_id
                               AND p.custody_generation=old_root.generation
                         )
                     )
               )",
            params![
                &root.canonical_repo_dir,
                &root.sandbox_branch,
                &root.sandbox_root,
                now,
                session_id.to_string(),
            ],
        )?;
        if restored != 1 {
            return Err(DaemonError::Store(
                "archived sandbox restoration lost its historical-state fence".into(),
            ));
        }

        insert_new_root(&tx, session_id, root)?;
        tx.commit()?;
        self.get_session(session_id)?.ok_or_else(|| {
            DaemonError::Store("restored sandbox session disappeared after commit".into())
        })
    }

    /// The direct interactive establishment boundary.  The admitted running
    /// invocation, complete Starting Session, and custody aggregate/projection
    /// become visible together or not at all.  Reservation and successor paths
    /// deliberately keep their existing transaction owners.
    pub(crate) fn insert_direct_session_with_custody_and_invocation(
        &mut self,
        session: &Session,
        binding: SessionCustodyBinding,
        invocation_id: Uuid,
    ) -> Result<()> {
        if session.status != SessionStatus::Starting {
            return Err(DaemonError::Store(
                "direct launch custody requires a Starting Session".into(),
            ));
        }
        if !matches!(
            &binding,
            SessionCustodyBinding::Ordinary | SessionCustodyBinding::New(_)
        ) {
            return Err(DaemonError::Store(
                "direct launch custody accepts only Ordinary or New bindings".into(),
            ));
        }
        let _root_guard = binding_custody_id(&binding).map(lock_custody_root);
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        insert_session_on(&tx, session)?;
        let bound = tx.execute(
            "UPDATE sessions
             SET model_invocation_id=?1, updated_at=?2
             WHERE id=?3 AND model_invocation_id IS NULL
               AND EXISTS (
                   SELECT 1 FROM model_invocations
                   WHERE id=?1 AND session_id=?3
                     AND admission_status='admitted' AND status='running'
               )",
            params![
                invocation_id.to_string(),
                timestamp(),
                session.id.to_string(),
            ],
        )?;
        if bound != 1 {
            return Err(DaemonError::Store(
                "direct launch admitted model invocation ownership/status fence lost".into(),
            ));
        }
        bind_on(&tx, session.id, binding)?;
        tx.commit()?;
        Ok(())
    }

    /// Rotation reserves its successor before it can bind or publish custody.
    /// Keep the complete Starting row and its already-admitted invocation in
    /// one immediate transaction so a crash cannot expose either half.
    ///
    /// This is the single durable insert of every rotation successor, and it is
    /// therefore where the one-continuation-per-predecessor invariant becomes
    /// unconditional rather than merely likely: the predecessor fence is
    /// re-proved inside this IMMEDIATE transaction, so a reservation that
    /// commits after `preflight_rotation_lead_transfer` and before this insert
    /// still cannot produce a branched `continued_from`.
    /// Also writes the durable `successor_reserved{successor_id}` rotation
    /// event for `rotation_id` in the same transaction: the provenance that
    /// keeps a reserved-but-unpublished successor out of the RPC-1 C2 legacy
    /// fallback (K2 finding b).
    pub(crate) fn insert_reserved_rotation_session_with_invocation(
        &mut self,
        session: &Session,
        invocation_id: Uuid,
        rotation_id: &str,
    ) -> Result<()> {
        let ordinary = session.sandbox_kind.is_none()
            && session.sandbox_root.is_none()
            && session.sandbox_branch.is_none()
            && session.sandbox_cleanup_state.is_none();
        let live = matches!(
            session.sandbox_kind,
            Some(rsi_common::types::SandboxKind::GitWorktree)
        ) && session.sandbox_root.is_some()
            && session.sandbox_branch.is_some()
            && matches!(
                session.sandbox_cleanup_state,
                Some(SandboxCleanupState::Live)
            );
        if session.status != SessionStatus::Starting
            || session.continued_from.is_none()
            || (!ordinary && !live)
        {
            return Err(DaemonError::Store(
                "rotation reservation requires a complete Starting continued_from successor".into(),
            ));
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(predecessor) = session.continued_from {
            super::successor_reservations::reject_agent_successor_predecessor_continuation_on(
                &tx,
                predecessor,
            )?;
            if let (Some(root), Some(branch)) = (&session.sandbox_root, &session.sandbox_branch)
                && prepared_reclaim_for_successor_on(&tx, predecessor, root, branch)?
            {
                return Err(sandbox_custody_error(SandboxCustodyErrorV1 {
                    version: 1,
                    code: SandboxCustodyErrorCodeV1::ReclaimPrepared,
                    session_id: Some(predecessor),
                    transition: SandboxCustodyTransitionV1::Rotation,
                    retryable: true,
                    recovery: SandboxCustodyRecoveryV1::RetryAfterReconcile,
                }));
            }
        }
        insert_session_on(&tx, session)?;
        let bound = tx.execute(
            "UPDATE sessions
             SET model_invocation_id=?1, updated_at=?2
             WHERE id=?3 AND status='Starting' AND continued_from IS NOT NULL
               AND model_invocation_id IS NULL
               AND EXISTS (
                   SELECT 1 FROM model_invocations
                   WHERE id=?1 AND session_id=?3
                     AND admission_status='admitted' AND status='running'
               )",
            params![
                invocation_id.to_string(),
                timestamp(),
                session.id.to_string(),
            ],
        )?;
        if bound != 1 {
            return Err(DaemonError::Store(
                "rotation reservation invocation ownership/status fence lost".into(),
            ));
        }
        if let Some(predecessor) = session.continued_from {
            tx.execute(
                "INSERT INTO rotation_events (session_id, rotation_id, phase, event_type, metadata, created_at)
                 VALUES (?1, ?2, 'reserved', 'successor_reserved', ?3, ?4)",
                params![
                    predecessor.to_string(),
                    rotation_id,
                    serde_json::json!({ "successor_id": session.id }).to_string(),
                    timestamp(),
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Read-only hint for a deferred rotation retry. Reservation still proves
    /// this predicate inside its own immediate transaction.
    pub(crate) fn prepared_reclaim_for_owner_tuple(
        &self,
        owner: Uuid,
        sandbox_root: Option<&Path>,
        sandbox_branch: Option<&str>,
    ) -> Result<bool> {
        Ok(prepared_reclaim_for_owner_tuple_on(
            &self.conn,
            owner,
            sandbox_root,
            sandbox_branch,
        )?)
    }

    /// A committed direct launch that cannot pass its first authorization or
    /// pre-provider preparation remains auditable but is never executable.
    pub(crate) fn fail_bound_direct_launch(
        &mut self,
        session_id: Uuid,
        error_code: &str,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = timestamp();
        let status = tx.execute(
            "UPDATE sessions
             SET status='Failed', stop_reason=?1, updated_at=?2
             WHERE id=?3 AND status='Starting'",
            params![
                format!("sandbox_custody:{error_code}"),
                now,
                session_id.to_string()
            ],
        )?;
        if status != 1 {
            return Err(DaemonError::Store(
                "direct launch failure settlement lost Starting session fence".into(),
            ));
        }
        let projection = tx.execute(
            "UPDATE session_execution_projections
             SET execution_state='invalid', freshness='invalid', effective_cwd=NULL,
                 validated_at=?2, error_code=?3, updated_at=?2
             WHERE session_id=?1",
            params![session_id.to_string(), now, error_code],
        )?;
        if projection != 1 {
            return Err(DaemonError::Store(
                "direct launch failure settlement projection fence lost".into(),
            ));
        }
        tx.commit()?;
        Ok(())
    }

    /// Settle a post-bind rotation failure without invalidating the verified
    /// successor projection.  The expected shape fences both ordinary rows and
    /// the live transfer owner at its post-transfer generation.
    pub(crate) fn fail_bound_rotation_successor(
        &mut self,
        session_id: Uuid,
        bound: BoundRotationCustody,
        code: SandboxCustodyErrorCodeV1,
    ) -> Result<()> {
        let _root_guard = match bound {
            BoundRotationCustody::Ordinary { .. } => None,
            BoundRotationCustody::Transfer { custody_id, .. } => {
                Some(lock_custody_root(custody_id))
            }
        };
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = timestamp();
        let status = match bound {
            BoundRotationCustody::Ordinary { predecessor_id } => tx.execute(
                "UPDATE sessions SET status='Failed', stop_reason=?1, updated_at=?2
                 WHERE id=?3 AND status='Starting' AND continued_from=?4
                   AND sandbox_kind IS NULL AND sandbox_root IS NULL
                   AND sandbox_branch IS NULL AND sandbox_cleanup_state IS NULL
                   AND sandbox_custody_id IS NULL
                   AND EXISTS (
                       SELECT 1 FROM session_execution_projections p
                       WHERE p.session_id=sessions.id
                         AND p.schema_version=1 AND p.projection_version=1
                         AND p.execution_state='ordinary_unsandboxed'
                         AND p.freshness='verified'
                         AND p.canonical_repo_dir=sessions.working_dir
                         AND p.effective_cwd=sessions.working_dir
                         AND p.custody_id IS NULL AND p.custody_generation IS NULL
                         AND p.validated_at IS NOT NULL AND p.error_code IS NULL
                   )",
                params![
                    format!("sandbox_custody:{}", code.as_str()),
                    now,
                    session_id.to_string(),
                    predecessor_id.to_string(),
                ],
            )?,
            BoundRotationCustody::Transfer {
                custody_id,
                predecessor_id,
                generation,
            } => tx.execute(
                "UPDATE sessions SET status='Failed', stop_reason=?1, updated_at=?2
                 WHERE id=?3 AND status='Starting' AND continued_from=?4
                   AND sandbox_kind='GitWorktree' AND sandbox_cleanup_state='Live'
                   AND sandbox_custody_id=?5
                   AND EXISTS (
                       SELECT 1 FROM sandbox_custody_roots r
                       JOIN session_execution_projections p ON p.session_id=sessions.id
                         WHERE r.custody_id=?5
                         AND r.owner_session_id=sessions.id AND r.generation=?6
                         AND r.state='live'
                         AND r.event_sequence=(
                           CASE WHEN r.validation_state='verified' THEN r.generation
                                ELSE r.generation+1 END
                         )
                         AND r.canonical_repo_dir=sessions.working_dir
                         AND r.sandbox_root=sessions.sandbox_root
                         AND r.sandbox_branch=sessions.sandbox_branch
                         AND p.schema_version=1 AND p.projection_version=1
                         AND p.execution_state='live_sandboxed'
                         AND p.canonical_repo_dir=r.canonical_repo_dir
                         AND p.custody_id=r.custody_id AND p.custody_generation=r.generation
                         AND p.validated_at IS NOT NULL
                         AND (
                           (r.validation_state='verified'
                            AND r.validated_generation=r.generation
                            AND r.validation_error_code IS NULL
                            AND p.freshness='verified'
                            AND p.effective_cwd=r.sandbox_root AND p.error_code IS NULL)
                           OR
                           (r.validation_state='invalid'
                            AND r.validated_generation=r.generation
                            AND r.validation_error_code=?7
                            AND p.freshness='invalid'
                            AND p.effective_cwd IS NULL AND p.error_code=?7)
                         )
                   )",
                params![
                    format!("sandbox_custody:{}", code.as_str()),
                    now,
                    session_id.to_string(),
                    predecessor_id.to_string(),
                    custody_id.to_string(),
                    generation as i64,
                    code.as_str(),
                ],
            )?,
        };
        if status == 0
            && let BoundRotationCustody::Transfer {
                custody_id,
                predecessor_id,
                generation,
            } = bound
        {
            // A failed ContextRead may already have quarantined the exact
            // transferred owner and atomically failed its Starting Session.
            // Accept only that exact non-authoritative result; never rewrite
            // it back to verified or replace its bounded refusal code.
            let quarantined: bool = tx.query_row(
                "SELECT EXISTS (
                     SELECT 1 FROM sessions s
                     JOIN session_execution_projections p ON p.session_id=s.id
                     JOIN sandbox_custody_roots r ON r.custody_id=s.sandbox_custody_id
                     WHERE s.id=?1 AND s.status='Failed' AND s.continued_from=?2
                       AND s.stop_reason=?3
                       AND s.sandbox_kind='GitWorktree'
                       AND s.sandbox_cleanup_state='Failed'
                       AND s.sandbox_custody_id=?4
                       AND r.owner_session_id IS NULL AND r.generation=?5
                       AND r.event_sequence=?8
                       AND r.state='quarantined' AND r.validation_state='invalid'
                       AND r.validated_generation=r.generation
                       AND r.validation_error_code=?6
                       AND r.canonical_repo_dir=s.working_dir
                       AND r.sandbox_root=s.sandbox_root
                       AND r.sandbox_branch=s.sandbox_branch
                       AND p.schema_version=1 AND p.projection_version=1
                       AND p.execution_state='live_sandboxed'
                       AND p.freshness='invalid' AND p.effective_cwd IS NULL
                       AND p.canonical_repo_dir=r.canonical_repo_dir
                       AND p.custody_id=r.custody_id AND p.custody_generation=?7
                       AND p.validated_at IS NOT NULL AND p.error_code=?6
                 )",
                params![
                    session_id.to_string(),
                    predecessor_id.to_string(),
                    format!("sandbox_custody:{}", code.as_str()),
                    custody_id.to_string(),
                    (generation + 1) as i64,
                    code.as_str(),
                    generation as i64,
                    (generation + 2) as i64,
                ],
                |row| row.get(0),
            )?;
            if quarantined {
                tx.commit()?;
                return Ok(());
            }
        }
        if status != 1 {
            return Err(DaemonError::Store(
                "rotation successor failure lost Starting session fence".into(),
            ));
        }
        // The projection is authority state, not failure metadata.  The
        // predicates above fence its exact legal shape; stop_reason is the
        // sole bounded rotation failure marker.
        tx.commit()?;
        Ok(())
    }

    /// Recover the exact bound shape only for an admitted automatic-retry
    /// candidate.  Controller cleanup uses this after launch establishment so
    /// cancellation/confirmation failure can mark the child Failed without
    /// transferring live custody backward.
    pub(crate) fn bound_retry_successor_custody(
        &self,
        session_id: Uuid,
    ) -> Result<Option<BoundRotationCustody>> {
        let ordinary: Option<String> = self
            .conn
            .query_row(
                "SELECT s.continued_from FROM sessions s
                 JOIN model_invocations i ON i.id=s.model_invocation_id
                 JOIN session_execution_projections p ON p.session_id=s.id
                 WHERE s.id=?1 AND i.purpose='session.retry.auto'
                   AND s.status IN ('Starting','Running','WaitingApproval')
                   AND s.continued_from IS NOT NULL
                   AND s.sandbox_kind IS NULL AND s.sandbox_root IS NULL
                   AND s.sandbox_branch IS NULL AND s.sandbox_cleanup_state IS NULL
                   AND s.sandbox_custody_id IS NULL
                   AND p.execution_state='ordinary_unsandboxed' AND p.freshness='verified'
                   AND p.effective_cwd=s.working_dir AND p.custody_id IS NULL
                   AND p.custody_generation IS NULL AND p.error_code IS NULL",
                [session_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(source_id) = ordinary {
            return Ok(Some(BoundRotationCustody::Ordinary {
                predecessor_id: Uuid::parse_str(&source_id).map_err(|error| {
                    DaemonError::Store(format!("invalid retry source UUID: {error}"))
                })?,
            }));
        }
        let live: Option<(String, String, i64)> = self
            .conn
            .query_row(
                "SELECT s.continued_from,r.custody_id,r.generation FROM sessions s
                 JOIN model_invocations i ON i.id=s.model_invocation_id
                 JOIN sandbox_custody_roots r ON r.custody_id=s.sandbox_custody_id
                 JOIN sandbox_custody_events e ON e.custody_id=r.custody_id
                   AND e.sequence=r.event_sequence AND e.event_kind='transferred'
                   AND e.cause='retry' AND e.to_generation=r.generation
                   AND e.from_owner_session_id=s.continued_from
                   AND e.to_owner_session_id=s.id
                   AND e.origin_session_id=s.continued_from
                   AND e.scheduled_job_id IS NULL
                 JOIN session_execution_projections p ON p.session_id=s.id
                 WHERE s.id=?1 AND i.purpose='session.retry.auto'
                   AND s.status IN ('Starting','Running','WaitingApproval')
                   AND s.continued_from IS NOT NULL
                   AND s.sandbox_kind='GitWorktree' AND s.sandbox_cleanup_state='Live'
                   AND s.working_dir=r.canonical_repo_dir
                   AND s.sandbox_root=r.sandbox_root AND s.sandbox_branch=r.sandbox_branch
                   AND r.owner_session_id=s.id AND r.state='live'
                   AND r.validation_state='verified' AND r.validated_generation=r.generation
                   AND p.execution_state='live_sandboxed' AND p.freshness='verified'
                   AND p.effective_cwd=r.sandbox_root AND p.custody_id=r.custody_id
                   AND p.custody_generation=r.generation AND p.error_code IS NULL",
                [session_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        live.map(|(source_id, custody_id, generation)| {
            Ok(BoundRotationCustody::Transfer {
                custody_id: Uuid::parse_str(&custody_id).map_err(|error| {
                    DaemonError::Store(format!("invalid retry custody UUID: {error}"))
                })?,
                predecessor_id: Uuid::parse_str(&source_id).map_err(|error| {
                    DaemonError::Store(format!("invalid retry source UUID: {error}"))
                })?,
                generation: generation as u64,
            })
        })
        .transpose()
    }

    /// Settle a retry candidate that reached controller establishment after
    /// its exact custody bind. Rotation deliberately remains Starting-only;
    /// controller confirmation/cancellation occurs after retry publication,
    /// so this retry-purpose fence also admits the active pre-terminal states.
    pub(crate) fn fail_established_retry_successor(
        &mut self,
        session_id: Uuid,
        bound: BoundRotationCustody,
        code: SandboxCustodyErrorCodeV1,
    ) -> Result<()> {
        let _root_guard = match bound {
            BoundRotationCustody::Ordinary { .. } => None,
            BoundRotationCustody::Transfer { custody_id, .. } => {
                Some(lock_custody_root(custody_id))
            }
        };
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = timestamp();
        let changed = match bound {
            BoundRotationCustody::Ordinary { predecessor_id } => tx.execute(
                "UPDATE sessions SET status='Failed',stop_reason=?1,updated_at=?2
                 WHERE id=?3 AND status IN ('Starting','Running','WaitingApproval','Completed','Failed','Interrupted')
                   AND continued_from=?4
                   AND sandbox_kind IS NULL AND sandbox_root IS NULL
                   AND sandbox_branch IS NULL AND sandbox_cleanup_state IS NULL
                   AND sandbox_custody_id IS NULL
                   AND EXISTS (
                     SELECT 1 FROM model_invocations i
                     JOIN session_execution_projections p ON p.session_id=sessions.id
                     WHERE i.id=sessions.model_invocation_id
                       AND i.purpose='session.retry.auto'
                       AND p.execution_state='ordinary_unsandboxed'
                       AND p.freshness='verified'
                       AND p.canonical_repo_dir=sessions.working_dir
                       AND p.effective_cwd=sessions.working_dir
                       AND p.custody_id IS NULL AND p.custody_generation IS NULL
                       AND p.error_code IS NULL
                   )",
                params![
                    format!("sandbox_custody:{}", code.as_str()),
                    now,
                    session_id.to_string(),
                    predecessor_id.to_string(),
                ],
            )?,
            BoundRotationCustody::Transfer {
                custody_id,
                predecessor_id,
                generation,
            } => tx.execute(
                "UPDATE sessions SET status='Failed',stop_reason=?1,updated_at=?2
                 WHERE id=?3 AND status IN ('Starting','Running','WaitingApproval','Completed','Failed','Interrupted')
                   AND continued_from=?4
                   AND sandbox_kind='GitWorktree' AND sandbox_cleanup_state='Live'
                   AND sandbox_custody_id=?5
                   AND EXISTS (
                     SELECT 1 FROM model_invocations i
                     JOIN sandbox_custody_roots r ON r.custody_id=sessions.sandbox_custody_id
                     JOIN sandbox_custody_events e ON e.custody_id=r.custody_id
                       AND e.sequence=r.event_sequence AND e.event_kind='transferred'
                       AND e.cause='retry' AND e.to_generation=r.generation
                       AND e.from_owner_session_id=sessions.continued_from
                       AND e.to_owner_session_id=sessions.id
                       AND e.origin_session_id=sessions.continued_from
                       AND e.scheduled_job_id IS NULL
                     JOIN session_execution_projections p ON p.session_id=sessions.id
                     WHERE i.id=sessions.model_invocation_id
                       AND i.purpose='session.retry.auto'
                       AND r.owner_session_id=sessions.id AND r.generation=?6
                       AND r.state='live' AND r.validation_state='verified'
                       AND r.validated_generation=r.generation
                       AND r.canonical_repo_dir=sessions.working_dir
                       AND r.sandbox_root=sessions.sandbox_root
                       AND r.sandbox_branch=sessions.sandbox_branch
                       AND p.execution_state='live_sandboxed' AND p.freshness='verified'
                       AND p.canonical_repo_dir=r.canonical_repo_dir
                       AND p.effective_cwd=r.sandbox_root
                       AND p.custody_id=r.custody_id
                       AND p.custody_generation=r.generation
                       AND p.error_code IS NULL
                   )",
                params![
                    format!("sandbox_custody:{}", code.as_str()),
                    now,
                    session_id.to_string(),
                    predecessor_id.to_string(),
                    custody_id.to_string(),
                    generation as i64,
                ],
            )?,
        };
        if changed != 1 {
            return Err(DaemonError::Store(
                "established retry failure lost exact active-session fence".into(),
            ));
        }
        tx.commit()?;
        Ok(())
    }

    /// Persist the post-context values that become part of the durable launch
    /// record before the unchanged provider funnel can execute.
    pub(crate) fn finalize_direct_launch_metadata(
        &mut self,
        session_id: Uuid,
        invocation_id: Uuid,
        git_branch: Option<&str>,
        harness_version_hash: Option<&str>,
    ) -> Result<()> {
        let updated = self.conn.execute(
            "UPDATE sessions
             SET git_branch=?1, harness_version_hash=?2, updated_at=?3
             WHERE id=?4 AND status='Starting' AND model_invocation_id=?5",
            params![
                git_branch,
                harness_version_hash,
                timestamp(),
                session_id.to_string(),
                invocation_id.to_string(),
            ],
        )?;
        if updated != 1 {
            return Err(DaemonError::Store(
                "direct launch metadata ownership/status fence lost".into(),
            ));
        }
        Ok(())
    }

    /// Coordination/reservation path. The V83 AFTER INSERT trigger has already
    /// made the row non-executable; this atomically replaces that projection.
    pub(crate) fn bind_reserved_session_custody(
        &mut self,
        session_id: Uuid,
        binding: SessionCustodyBinding,
    ) -> Result<()> {
        let _root_guard = binding_custody_id(&binding).map(lock_custody_root);
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        bind_reserved_session_custody_on(&tx, session_id, binding)?;
        tx.commit()?;
        Ok(())
    }

    /// Compare the completed-map snapshot with the current durable authority
    /// row. All SQL-only `sessions` authority columns are included in the
    /// proof even though they are absent from the public `Session` mapping.
    pub(crate) fn capture_completed_rotation_authority(
        &self,
        snapshot: &Session,
    ) -> Result<RotationAuthorityCapture> {
        if snapshot.status != SessionStatus::Completed {
            return Ok(RotationAuthorityCapture::Changed);
        }
        let Some((durable, sandbox_custody_id, model_invocation_id, legacy_primary_tag)) =
            load_rotation_authority_session_on(&self.conn, snapshot.id)?
        else {
            return Ok(RotationAuthorityCapture::Missing);
        };
        if durable.status != SessionStatus::Completed
            || rotation_session_authority(snapshot)? != rotation_session_authority(&durable)?
            || (ordinary_session_shape(snapshot) && sandbox_custody_id.is_some())
        {
            return Ok(RotationAuthorityCapture::Changed);
        }
        Ok(RotationAuthorityCapture::Captured(RotationAuthorityFence {
            predecessor_id: snapshot.id,
            predecessor_status: SessionStatus::Completed,
            session_authority: rotation_session_authority(&durable)?,
            sandbox_custody_id,
            model_invocation_id,
            legacy_primary_tag,
        }))
    }

    /// Restart recovery (F3 Slice 3a, RPC-1 C7): capture a predecessor that
    /// restore reconciled `Failed` because its provider died mid-rotation.
    /// Accepted only when the snapshot and durable row are both `Failed`, the
    /// C5 journal records the `process-died` cause for it, and its latest
    /// rotation has an open `writing_handoff`/`pending_interrupt` intent.
    /// The resulting fence pins `Failed`, so archive and rollback keep the
    /// same exact-authority checks as the Completed path.
    pub(crate) fn capture_recovered_rotation_authority(
        &self,
        snapshot: &Session,
    ) -> Result<RotationAuthorityCapture> {
        if snapshot.status != SessionStatus::Failed {
            return Ok(RotationAuthorityCapture::Changed);
        }
        let Some((durable, sandbox_custody_id, model_invocation_id, legacy_primary_tag)) =
            load_rotation_authority_session_on(&self.conn, snapshot.id)?
        else {
            return Ok(RotationAuthorityCapture::Missing);
        };
        let process_died = self
            .conn
            .query_row(
                "SELECT value FROM daemon_settings WHERE key=?1",
                [super::daemon_settings::c5_autofile_pending_key(snapshot.id)],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
            .is_some_and(|value| {
                value.get("cause").and_then(|c| c.as_str()) == Some("process-died")
            });
        if durable.status != SessionStatus::Failed
            || !process_died
            || self.latest_open_rotation_intent(snapshot.id)?.is_none()
            || rotation_session_authority(snapshot)? != rotation_session_authority(&durable)?
            || (ordinary_session_shape(snapshot) && sandbox_custody_id.is_some())
        {
            return Ok(RotationAuthorityCapture::Changed);
        }
        Ok(RotationAuthorityCapture::Captured(RotationAuthorityFence {
            predecessor_id: snapshot.id,
            predecessor_status: SessionStatus::Failed,
            session_authority: rotation_session_authority(&durable)?,
            sandbox_custody_id,
            model_invocation_id,
            legacy_primary_tag,
        }))
    }

    pub(crate) fn completed_rotation_authority_matches(
        &self,
        fence: &RotationAuthorityFence,
        predecessor_id: Uuid,
    ) -> Result<bool> {
        rotation_authority_matches_on(&self.conn, fence, predecessor_id, fence.predecessor_status)
    }

    /// Capture the complete durable Failed source before retry identity,
    /// controller, token, model, context, or provider work can begin.
    pub(crate) fn capture_failed_retry_authority(
        &self,
        snapshot: &Session,
    ) -> Result<RetryAuthorityCapture> {
        let retry_attempt = snapshot.retry_attempt.unwrap_or(0);
        let max_retries = snapshot.max_retries.unwrap_or(0);
        if snapshot.status != SessionStatus::Failed
            || max_retries == 0
            || retry_attempt >= max_retries
        {
            return Ok(RetryAuthorityCapture::Changed);
        }
        let Some((durable, sandbox_custody_id, model_invocation_id, legacy_primary_tag)) =
            load_rotation_authority_session_on(&self.conn, snapshot.id)?
        else {
            return Ok(RetryAuthorityCapture::Missing);
        };
        if durable.status != SessionStatus::Failed
            || serde_json::to_vec(snapshot).map_err(|error| {
                DaemonError::Store(format!("serialize retry source snapshot: {error}"))
            })? != serde_json::to_vec(&durable).map_err(|error| {
                DaemonError::Store(format!("serialize durable retry source: {error}"))
            })?
            || (ordinary_session_shape(snapshot) && sandbox_custody_id.is_some())
        {
            return Ok(RetryAuthorityCapture::Changed);
        }
        Ok(RetryAuthorityCapture::Captured(RetryAuthorityFence {
            source_session_id: snapshot.id,
            session_authority_after_c5: retry_session_authority_after_c5(&durable)?,
            sandbox_custody_id,
            model_invocation_id,
            legacy_primary_tag,
            retry_attempt,
            max_retries,
        }))
    }

    /// Retry-only reservation bind.  The unchanged C5 transaction has already
    /// exhausted the source and inserted the Starting child.  Recheck that
    /// exact post-C5 source shape in the same immediate transaction as the
    /// ordinary bind or one-generation Transfer.
    /// The custody runtime must hold the matching root stripe when `binding`
    /// is a Transfer, spanning its live filesystem/Git authentication through
    /// this transaction. Ordinary binding carries no root lock.
    pub(crate) fn bind_reserved_retry_session_custody_locked(
        &mut self,
        successor: &Session,
        binding: SessionCustodyBinding,
        source: &RetryAuthorityFence,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !retry_authority_matches_after_c5_on(&tx, source)? {
            return Err(DaemonError::Store(
                "retry source authority fence changed before bind".into(),
            ));
        }
        let preexisting_tags: i64 = tx.query_row(
            "SELECT COUNT(*) FROM session_tags WHERE session_id=?1",
            [successor.id.to_string()],
            |row| row.get(0),
        )?;
        if preexisting_tags != 0 {
            return Err(DaemonError::Store(
                "retry successor unexpectedly had tags before custody bind".into(),
            ));
        }
        for tag in &successor.tags {
            tx.execute(
                "INSERT INTO session_tags (session_id,tag) VALUES (?1,?2)",
                params![successor.id.to_string(), tag],
            )?;
        }
        tx.execute(
            "UPDATE sessions SET tag=?2 WHERE id=?1 AND tag=''",
            params![successor.id.to_string(), successor.tag],
        )?;
        let durable_successor = load_rotation_authority_session_on(&tx, successor.id)?
            .map(|(session, _, _, _)| session)
            .ok_or_else(|| DaemonError::Store("retry successor disappeared before bind".into()))?;
        if serde_json::to_vec(&durable_successor).map_err(|error| {
            DaemonError::Store(format!("serialize durable retry successor: {error}"))
        })? != serde_json::to_vec(successor).map_err(|error| {
            DaemonError::Store(format!("serialize expected retry successor: {error}"))
        })? {
            return Err(DaemonError::Store(
                "retry successor complete durable row fence changed before bind".into(),
            ));
        }
        let child_matches: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1 AND status='Starting' AND continued_from=?2 AND retry_attempt=?3 AND max_retries=?4)",
            params![
                successor.id.to_string(),
                source.source_session_id.to_string(),
                source.retry_attempt as i64,
                source.max_retries as i64,
            ],
            |row| row.get(0),
        )?;
        if !child_matches {
            return Err(DaemonError::Store(
                "retry successor lineage/counter fence changed before bind".into(),
            ));
        }
        bind_reserved_session_custody_on(&tx, successor.id, binding)?;
        tx.commit()?;
        Ok(())
    }

    /// Rotation-only reservation bind. The completed predecessor fence and
    /// successor custody bind are checked under the same `BEGIN IMMEDIATE`, so
    /// a durable authority mutation cannot land between validation and bind.
    pub(crate) fn bind_reserved_rotation_session_custody(
        &mut self,
        session_id: Uuid,
        binding: SessionCustodyBinding,
        predecessor: &RotationAuthorityFence,
    ) -> Result<()> {
        let _root_guard = binding_custody_id(&binding).map(lock_custody_root);
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !rotation_authority_matches_on(
            &tx,
            predecessor,
            predecessor.predecessor_id,
            predecessor.predecessor_status,
        )? {
            return Err(DaemonError::Store(
                "rotation predecessor authority fence changed before bind".into(),
            ));
        }
        bind_reserved_session_custody_on(&tx, session_id, binding)?;
        tx.commit()?;
        Ok(())
    }

    /// Restore only the exact ordinary predecessor archived by the rotation
    /// saga. Refusal is distinct from SQLite failure and never mutates the row.
    pub(crate) fn restore_archived_rotation_predecessor(
        &mut self,
        predecessor: &RotationAuthorityFence,
    ) -> Result<ArchivedRotationRestoration> {
        if predecessor.sandbox_custody_id.is_some() {
            return Ok(ArchivedRotationRestoration::Refused);
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !rotation_authority_matches_on(
            &tx,
            predecessor,
            predecessor.predecessor_id,
            SessionStatus::Archived,
        )? {
            return Ok(ArchivedRotationRestoration::Refused);
        }
        let exact_archived_ordinary: bool = tx.query_row(
            "SELECT EXISTS (
                 SELECT 1 FROM sessions s
                 JOIN session_execution_projections p ON p.session_id=s.id
                 WHERE s.id=?1 AND s.status='Archived' AND s.pending_archive=0
                   AND s.sandbox_kind IS NULL AND s.sandbox_root IS NULL
                   AND s.sandbox_branch IS NULL AND s.sandbox_cleanup_state IS NULL
                   AND s.sandbox_custody_id IS NULL
                   AND p.schema_version=1 AND p.projection_version=1
                   AND p.execution_state='ordinary_unsandboxed'
                   AND p.freshness='verified'
                   AND p.canonical_repo_dir=s.working_dir
                   AND p.effective_cwd=s.working_dir
                   AND p.custody_id IS NULL AND p.custody_generation IS NULL
                   AND p.validated_at IS NOT NULL AND p.error_code IS NULL
             )",
            [predecessor.predecessor_id.to_string()],
            |row| row.get(0),
        )?;
        if !exact_archived_ordinary {
            return Ok(ArchivedRotationRestoration::Refused);
        }
        let changed = tx.execute(
            "UPDATE sessions SET status=?3, pending_archive=0, updated_at=?2
             WHERE id=?1 AND status='Archived' AND pending_archive=0
               AND sandbox_kind IS NULL AND sandbox_root IS NULL
               AND sandbox_branch IS NULL AND sandbox_cleanup_state IS NULL
               AND sandbox_custody_id IS NULL",
            params![
                predecessor.predecessor_id.to_string(),
                timestamp(),
                super::row_mappers::session_status_to_str(predecessor.predecessor_status),
            ],
        )?;
        if changed != 1
            || !rotation_authority_matches_on(
                &tx,
                predecessor,
                predecessor.predecessor_id,
                predecessor.predecessor_status,
            )?
        {
            return Err(DaemonError::Store(
                "archived rotation predecessor restore lost exact authority fence".into(),
            ));
        }
        let restored = load_rotation_authority_session_on(&tx, predecessor.predecessor_id)?
            .map(|(session, _, _, _)| session)
            .ok_or_else(|| {
                DaemonError::Store("restored rotation predecessor disappeared before commit".into())
            })?;
        tx.commit()?;
        Ok(ArchivedRotationRestoration::Restored(restored))
    }

    /// After provider establishment, commit archival and the manager rotation
    /// receipt together. Both custody dispositions retain the complete
    /// predecessor authority fence; transferred custody additionally requires
    /// the exact committed transfer and current successor owner/generation.
    /// Root-manager publication preserves both independent execution projections.
    /// Caller owns deterministic custody locks and the authority/journal IMMEDIATE
    /// transaction; this helper never starts a transaction or moves a checkout.
    pub(super) fn finalize_distinct_manager_predecessor_on(
        &self,
        tx: &Transaction<'_>,
        root: &super::manager_successions::ManagerRootSuccession,
    ) -> Result<()> {
        let candidate = self.manager_succession_candidate_custody(root)?;
        if root.candidate_custody.as_ref() != Some(&candidate) {
            return Err(DaemonError::Store(
                "manager_succession_custody_changed".into(),
            ));
        }
        let changed = tx.execute(
            "UPDATE sessions SET status='Archived',updated_at=?2 WHERE id=?1
             AND status IN ('Completed','Interrupted','Failed') AND pending_archive=0",
            params![
                root.predecessor_session_id.to_string(),
                super::harness_manager_v2::now()
            ],
        )?;
        if changed != 1
            || !Self::record_harness_manager_rotation_on(
                tx,
                root.predecessor_session_id,
                root.candidate_session_id,
            )?
        {
            return Err(DaemonError::Store(
                "manager_succession_publication_changed".into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn finalize_rotation_predecessor(
        &mut self,
        predecessor: &RotationAuthorityFence,
        successor: Uuid,
        bound: BoundRotationCustody,
    ) -> Result<bool> {
        let (predecessor_id, custody_id) = match bound {
            BoundRotationCustody::Ordinary { predecessor_id } => (predecessor_id, None),
            BoundRotationCustody::Transfer {
                predecessor_id,
                custody_id,
                ..
            } => (predecessor_id, Some(custody_id)),
        };
        if predecessor_id != predecessor.predecessor_id
            || custody_id != predecessor.sandbox_custody_id
        {
            return Ok(false);
        }
        let _root_guard = custody_id.map(lock_custody_root);
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some((current, _, _, _)) = load_rotation_authority_session_on(&tx, predecessor_id)?
        else {
            return Ok(false);
        };
        if current.status != predecessor.predecessor_status
            && current.status != SessionStatus::Archived
        {
            return Ok(false);
        }
        if !rotation_authority_matches_on(&tx, predecessor, predecessor_id, current.status)? {
            return Ok(false);
        }
        let custody_matches: bool = match bound {
            BoundRotationCustody::Ordinary { .. } => tx.query_row(
                "SELECT EXISTS (
                   SELECT 1 FROM sessions previous JOIN sessions successor
                     ON successor.continued_from=previous.id
                   JOIN session_execution_projections p ON p.session_id=successor.id
                   WHERE previous.id=?1 AND successor.id=?2 AND successor.status<>'Deleted'
                     AND previous.pending_archive=0
                     AND previous.sandbox_kind IS NULL AND previous.sandbox_root IS NULL
                     AND previous.sandbox_branch IS NULL AND previous.sandbox_cleanup_state IS NULL
                     AND previous.sandbox_custody_id IS NULL
                     AND successor.sandbox_kind IS NULL AND successor.sandbox_root IS NULL
                     AND successor.sandbox_branch IS NULL AND successor.sandbox_cleanup_state IS NULL
                     AND successor.sandbox_custody_id IS NULL
                     AND p.schema_version=1 AND p.projection_version=1
                     AND p.execution_state='ordinary_unsandboxed' AND p.freshness='verified'
                     AND p.canonical_repo_dir=successor.working_dir
                     AND p.effective_cwd=successor.working_dir
                     AND p.custody_id IS NULL AND p.custody_generation IS NULL
                     AND p.validated_at IS NOT NULL AND p.error_code IS NULL
                 )",
                params![predecessor_id.to_string(), successor.to_string()],
                |row| row.get(0),
            )?,
            BoundRotationCustody::Transfer { custody_id, generation, .. } => tx.query_row(
                "SELECT EXISTS (
                   SELECT 1 FROM sessions previous JOIN sessions successor
                     ON successor.continued_from=previous.id
                   JOIN sandbox_custody_roots r ON r.custody_id=successor.sandbox_custody_id
                   JOIN sandbox_custody_events e ON e.custody_id=r.custody_id
                     AND e.sequence=r.event_sequence AND e.event_kind='transferred'
                     AND e.cause='rotation' AND e.from_generation=?4-1 AND e.to_generation=?4
                     AND e.from_owner_session_id=previous.id AND e.to_owner_session_id=successor.id
                     AND e.origin_session_id=previous.id AND e.scheduled_job_id IS NULL
                   JOIN session_execution_projections p ON p.session_id=successor.id
                   JOIN session_execution_projections history ON history.session_id=previous.id
                   WHERE previous.id=?1 AND successor.id=?2 AND successor.status<>'Deleted'
                     AND previous.pending_archive=0 AND previous.sandbox_custody_id=?3
                     AND previous.sandbox_kind='GitWorktree' AND previous.sandbox_cleanup_state='Live'
                     AND previous.working_dir=r.canonical_repo_dir
                     AND previous.sandbox_root=r.sandbox_root AND previous.sandbox_branch=r.sandbox_branch
                     AND successor.sandbox_kind='GitWorktree' AND successor.sandbox_cleanup_state='Live'
                     AND successor.working_dir=r.canonical_repo_dir
                     AND successor.sandbox_root=r.sandbox_root AND successor.sandbox_branch=r.sandbox_branch
                     AND r.custody_id=?3 AND r.owner_session_id=successor.id AND r.generation=?4
                     AND r.state='live' AND r.validation_state='verified'
                     AND r.validated_generation=r.generation AND r.validation_error_code IS NULL
                     AND p.schema_version=1 AND p.projection_version=1
                     AND p.execution_state='live_sandboxed' AND p.freshness='verified'
                     AND p.canonical_repo_dir=r.canonical_repo_dir AND p.effective_cwd=r.sandbox_root
                     AND p.custody_id=r.custody_id AND p.custody_generation=r.generation
                     AND p.validated_at IS NOT NULL AND p.error_code IS NULL
                     AND history.schema_version=1 AND history.projection_version=1
                     AND history.execution_state='historical_transferred' AND history.freshness='verified'
                     AND history.canonical_repo_dir=r.canonical_repo_dir AND history.effective_cwd IS NULL
                     AND history.custody_id=r.custody_id AND history.custody_generation=?4-1
                     AND history.validated_at IS NOT NULL AND history.error_code IS NULL
                 )",
                params![predecessor_id.to_string(), successor.to_string(), custody_id.to_string(), generation as i64],
                |row| row.get(0),
            )?,
        };
        if !custody_matches {
            return Ok(false);
        }
        if current.status == SessionStatus::Archived {
            // A replay only acknowledges the already committed, still-active
            // receipt. An ordinary rearchive has no such receipt after restore.
            return Ok(tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM harness_manager_rotation_edges
                 WHERE predecessor_session_id=?1 AND successor_session_id=?2 AND retired_at IS NULL)",
                params![predecessor_id.to_string(), successor.to_string()],
                |row| row.get(0),
            )?);
        }
        let changed = tx.execute(
            "UPDATE sessions SET status='Archived', updated_at=?2
             WHERE id=?1 AND status=?4 AND pending_archive=0
               AND EXISTS(SELECT 1 FROM sessions successor WHERE successor.id=?3 AND successor.status='Starting')",
            params![
                predecessor_id.to_string(),
                timestamp(),
                successor.to_string(),
                super::row_mappers::session_status_to_str(predecessor.predecessor_status),
            ],
        )?;
        if changed != 1
            || !rotation_authority_matches_on(
                &tx,
                predecessor,
                predecessor_id,
                SessionStatus::Archived,
            )?
        {
            return Err(DaemonError::Store(
                "rotation archive lost exact authority fence".into(),
            ));
        }
        Self::record_harness_manager_rotation_on(&tx, predecessor_id, successor)?;
        tx.commit()?;
        Ok(true)
    }

    pub(crate) fn record_failed_revalidation(
        &mut self,
        custody_id: Uuid,
        expected_generation: u64,
        code: SandboxCustodyErrorCodeV1,
        transition: SandboxCustodyTransitionV1,
    ) -> Result<()> {
        let _root_guard = lock_custody_root(custody_id);
        self.record_failed_revalidation_locked(custody_id, expected_generation, code, transition)
    }

    pub(crate) fn record_failed_revalidation_locked(
        &mut self,
        custody_id: Uuid,
        expected_generation: u64,
        code: SandboxCustodyErrorCodeV1,
        transition: SandboxCustodyTransitionV1,
    ) -> Result<()> {
        let cause = if transition == SandboxCustodyTransitionV1::StartupReconciliation {
            CustodyCause::StartupReconciliation
        } else {
            CustodyCause::EffectRevalidation
        };
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let root = load_root(&tx, custody_id)?;
        if root.generation != expected_generation || root.owner_session_id.is_none() {
            return Err(sandbox_custody_error(
                rsi_common::types::SandboxCustodyErrorV1 {
                    version: 1,
                    code: SandboxCustodyErrorCodeV1::CustodyChanged,
                    session_id: None,
                    transition,
                    retryable: true,
                    recovery: rsi_common::types::SandboxCustodyRecoveryV1::RetryAfterReconcile,
                },
            ));
        }
        let now = timestamp();
        let next_sequence = root.event_sequence + 1;
        insert_event(
            &tx,
            EventInput {
                custody_id,
                sequence: next_sequence,
                event_kind: "validation_failed",
                cause,
                from_generation: Some(root.generation),
                to_generation: root.generation,
                from_owner: root.owner_session_id,
                to_owner: root.owner_session_id,
                origin: None,
                scheduled_job: None,
                prior_state: Some(root.state.as_str()),
                next_state: "live",
                error_code: Some(code.as_str()),
                occurred_at: &now,
            },
        )?;
        tx.execute(
            "UPDATE sandbox_custody_roots SET validation_state='invalid', validated_generation=generation, validated_at=?2, validation_error_code=?3, event_sequence=?4, updated_at=?2 WHERE custody_id=?1",
            params![custody_id.to_string(), now, code.as_str(), next_sequence],
        )?;
        tx.execute(
            "UPDATE session_execution_projections SET freshness='invalid', effective_cwd=NULL, validated_at=?2, error_code=?3, updated_at=?2 WHERE custody_id=?1",
            params![custody_id.to_string(), timestamp(), code.as_str()],
        )?;
        if root.reserved_effects == 0 && root.active_effects == 0 {
            quarantine_invalid_root_on(&tx, custody_id, &root, next_sequence, &now, cause, code)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Publish the ordinary projection only after startup has classified the
    /// exact all-null tuple. This intentionally refuses to heal a partial
    /// tuple or attach a legacy custody aggregate by inference.
    pub(crate) fn publish_startup_ordinary(&mut self, session_id: Uuid) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_ordinary_session_tuple(&tx, session_id)?;
        tx.execute(
            "UPDATE session_execution_projections SET execution_state='ordinary_unsandboxed', freshness='verified', effective_cwd=canonical_repo_dir, custody_id=NULL, custody_generation=NULL, validated_at=?2, error_code=NULL, updated_at=?2 WHERE session_id=?1",
            params![session_id.to_string(), timestamp()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// A successful live authentication has no event or generation effect:
    /// it merely makes the cached execution projection current for this boot.
    pub(crate) fn publish_startup_live_verification(
        &mut self,
        session_id: Uuid,
        custody_id: Uuid,
        generation: u64,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let root = load_root(&tx, custody_id)?;
        if root.owner_session_id != Some(session_id)
            || root.generation != generation
            || !matches!(root.state, RootState::Live)
        {
            return Err(DaemonError::Store(
                "startup custody verification fence lost".into(),
            ));
        }
        let now = timestamp();
        tx.execute(
            "UPDATE sandbox_custody_roots SET validation_state='verified', validated_generation=?2, validated_at=?3, validation_error_code=NULL, updated_at=?3 WHERE custody_id=?1 AND owner_session_id=?4 AND generation=?2 AND state='live'",
            params![custody_id.to_string(), generation as i64, now, session_id.to_string()],
        )?;
        tx.execute(
            "UPDATE session_execution_projections SET execution_state='live_sandboxed', freshness='verified', effective_cwd=(SELECT sandbox_root FROM sandbox_custody_roots WHERE custody_id=?2), custody_id=?2, custody_generation=?3, validated_at=?4, error_code=NULL, updated_at=?4 WHERE session_id=?1",
            params![session_id.to_string(), custody_id.to_string(), generation as i64, timestamp()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Fail closed before restore maps are rebuilt. Terminal history remains
    /// terminal; only potentially executable rows change status.
    pub(crate) fn invalidate_startup_session(
        &mut self,
        session_id: Uuid,
        code: SandboxCustodyErrorCodeV1,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = timestamp();
        tx.execute(
            "UPDATE sessions SET status='Failed', stop_reason=?2, updated_at=?3 WHERE id=?1 AND status IN ('Starting','Running','WaitingApproval','Failed')",
            params![session_id.to_string(), format!("sandbox_custody:{}", code.as_str()), now],
        )?;
        tx.execute(
            "UPDATE session_execution_projections SET execution_state='invalid', freshness='invalid', effective_cwd=NULL, validated_at=?2, error_code=?3, updated_at=?2 WHERE session_id=?1",
            params![session_id.to_string(), timestamp(), code.as_str()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Publish an already-durable terminal aggregate without changing any
    /// terminal Session status. Quarantine remains invalid (and deliberately
    /// preserves its original error); purged/failed roots receive the
    /// compatible verified historical projection.
    pub(crate) fn publish_startup_terminal_root(&mut self, custody_id: Uuid) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let root = load_root(&tx, custody_id)?;
        let now = timestamp();
        match root.state {
            RootState::Purged => {
                tx.execute(
                    "UPDATE session_execution_projections SET execution_state='historical_purged', freshness='verified', effective_cwd=NULL, custody_id=?2, custody_generation=?3, validated_at=?4, error_code=NULL, updated_at=?4 WHERE session_id IN (SELECT id FROM sessions WHERE sandbox_custody_id=?1)",
                    params![custody_id.to_string(), custody_id.to_string(), root.generation as i64, now],
                )?;
            }
            RootState::Failed => {
                tx.execute(
                    "UPDATE session_execution_projections SET execution_state='historical_cleanup_failed', freshness='verified', effective_cwd=NULL, custody_id=?2, custody_generation=?3, validated_at=?4, error_code=NULL, updated_at=?4 WHERE session_id IN (SELECT id FROM sessions WHERE sandbox_custody_id=?1)",
                    params![custody_id.to_string(), custody_id.to_string(), root.generation as i64, now],
                )?;
            }
            RootState::Quarantined => {
                tx.execute(
                    "UPDATE session_execution_projections SET execution_state='quarantined', freshness='invalid', effective_cwd=NULL, custody_id=?2, custody_generation=?3, validated_at=?4, error_code=COALESCE((SELECT validation_error_code FROM sandbox_custody_roots WHERE custody_id=?1), error_code), updated_at=?4 WHERE session_id IN (SELECT id FROM sessions WHERE sandbox_custody_id=?1)",
                    params![custody_id.to_string(), custody_id.to_string(), root.generation as i64, now],
                )?;
            }
            RootState::Live => {
                return Err(DaemonError::Store(
                    "live custody root cannot publish terminal startup state".into(),
                ));
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// A terminal aggregate that has a conflicting retained claimant cannot
    /// remain an apparently trustworthy historical authority. There is no
    /// executable owner to revalidate, so quarantine it directly while
    /// retaining the original immutable identity and an explicit startup
    /// reconciliation event. Repeated startup passes preserve the first code.
    pub(crate) fn quarantine_startup_terminal_root(
        &mut self,
        custody_id: Uuid,
        code: SandboxCustodyErrorCodeV1,
    ) -> Result<()> {
        let _guard = lock_custody_root(custody_id);
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let root = load_root(&tx, custody_id)?;
        if matches!(root.state, RootState::Quarantined) {
            return Ok(());
        }
        if root.owner_session_id.is_some() || matches!(root.state, RootState::Live) {
            return Err(DaemonError::Store(
                "terminal startup quarantine requires ownerless aggregate".into(),
            ));
        }
        let now = timestamp();
        let next_generation = root.generation + 1;
        let next_sequence = root.event_sequence + 1;
        insert_event(
            &tx,
            EventInput {
                custody_id,
                sequence: next_sequence,
                event_kind: "quarantined",
                cause: CustodyCause::StartupReconciliation,
                from_generation: Some(root.generation),
                to_generation: next_generation,
                from_owner: None,
                to_owner: None,
                origin: None,
                scheduled_job: None,
                prior_state: Some(root.state.as_str()),
                next_state: "quarantined",
                error_code: Some(code.as_str()),
                occurred_at: &now,
            },
        )?;
        tx.execute(
            "UPDATE sandbox_custody_roots SET state='quarantined', generation=?2, event_sequence=?3, validation_state='invalid', validated_generation=?2, validated_at=?4, validation_error_code=?5, updated_at=?4 WHERE custody_id=?1 AND owner_session_id IS NULL AND generation=?6",
            params![custody_id.to_string(), next_generation as i64, next_sequence as i64, now, code.as_str(), root.generation as i64],
        )?;
        tx.execute(
            "UPDATE session_execution_projections SET execution_state='invalid', freshness='invalid', effective_cwd=NULL, custody_id=?2, custody_generation=?3, validated_at=?4, error_code=?5, updated_at=?4 WHERE custody_id=?1",
            params![custody_id.to_string(), custody_id.to_string(), next_generation as i64, now, code.as_str()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Root-null historical Purged rows predate authoritative custody. They
    /// have no authenticated root to attach, so publish only their compatible
    /// projection and never fabricate a custody aggregate.
    pub(crate) fn publish_startup_rootless_purged(&mut self, session_id: Uuid) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = timestamp();
        let changed = tx.execute(
            "UPDATE session_execution_projections SET execution_state='historical_purged', freshness='verified', effective_cwd=NULL, custody_id=NULL, custody_generation=NULL, validated_at=?2, error_code=NULL, updated_at=?2 WHERE session_id=?1 AND EXISTS (SELECT 1 FROM sessions WHERE id=?1 AND sandbox_kind='GitWorktree' AND sandbox_root IS NULL AND sandbox_branch IS NULL AND sandbox_cleanup_state='Purged')",
            params![session_id.to_string(), now],
        )?;
        if changed != 1 {
            return Err(DaemonError::Store(
                "rootless startup purged compatibility tuple mismatch".into(),
            ));
        }
        tx.commit()?;
        Ok(())
    }

    /// Root-null cleanup failures also predate authoritative custody. Keep the
    /// historical failure visible without manufacturing an aggregate identity.
    pub(crate) fn publish_startup_rootless_failed(&mut self, session_id: Uuid) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = timestamp();
        let changed = tx.execute(
            "UPDATE session_execution_projections SET execution_state='historical_cleanup_failed', freshness='verified', effective_cwd=NULL, custody_id=NULL, custody_generation=NULL, validated_at=?2, error_code=NULL, updated_at=?2 WHERE session_id=?1 AND EXISTS (SELECT 1 FROM sessions WHERE id=?1 AND sandbox_kind='GitWorktree' AND sandbox_root IS NULL AND sandbox_branch IS NULL AND sandbox_cleanup_state='Failed' AND sandbox_custody_id IS NULL)",
            params![session_id.to_string(), now],
        )?;
        if changed != 1 {
            return Err(DaemonError::Store(
                "rootless startup failed compatibility tuple mismatch".into(),
            ));
        }
        tx.commit()?;
        Ok(())
    }

    /// Create the initial authoritative aggregate from a fully authenticated
    /// legacy tuple. All writes for this one root (normalization, links,
    /// immutable allocation, transfers, and projections) commit together.
    pub(crate) fn reconstruct_legacy_startup_root(
        &mut self,
        root: LegacyStartupRoot,
    ) -> Result<()> {
        let _root_guard = lock_custody_root(root.custody_id);
        if root.lineage.first().map(|entry| entry.0) != Some(root.allocation_session_id) {
            return Err(DaemonError::Store(
                "legacy lineage does not begin at allocation identity".into(),
            ));
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let already_exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sandbox_custody_roots WHERE sandbox_root=?1)",
            [&root.sandbox_root],
            |row| row.get(0),
        )?;
        if already_exists {
            return Err(DaemonError::Store(
                "legacy root already has custody aggregate".into(),
            ));
        }
        for (session_id, _) in &root.lineage {
            ensure_legacy_session_matches_root(
                &tx,
                *session_id,
                &root.canonical_repo_dir,
                &root.sandbox_root,
                &root.sandbox_branch,
            )?;
        }
        let now = timestamp();
        let allocation = root.allocation_session_id;
        tx.execute("INSERT INTO sandbox_custody_roots (custody_id,allocation_id,canonical_repo_dir,sandbox_root,sandbox_branch,repository_identity,source_commit,state,owner_session_id,generation,event_sequence,validation_state,validated_generation,validated_at,validation_error_code,effect_boot_id,reserved_effects,active_effects,created_at,updated_at,tombstoned_at) VALUES (?1,?2,?3,?4,?5,?6,?7,'live',?8,1,1,'verified',1,?9,NULL,NULL,0,0,?9,?9,NULL)", params![root.custody_id.to_string(),allocation.to_string(),root.canonical_repo_dir,root.sandbox_root,root.sandbox_branch,root.repository_identity,root.source_commit,allocation.to_string(),now])?;
        insert_event(
            &tx,
            EventInput {
                custody_id: root.custody_id,
                sequence: 1,
                event_kind: "allocated",
                cause: CustodyCause::StartupReconciliation,
                from_generation: None,
                to_generation: 1,
                from_owner: None,
                to_owner: Some(allocation),
                origin: None,
                scheduled_job: None,
                prior_state: None,
                next_state: "live",
                error_code: None,
                occurred_at: &now,
            },
        )?;
        tx.execute(
            "UPDATE sessions SET sandbox_cleanup_state='Live', sandbox_custody_id=?2, updated_at=?3 WHERE id=?1",
            params![allocation.to_string(), root.custody_id.to_string(), now],
        )?;
        publish_verified(&tx, allocation, root.custody_id, 1, None)?;
        let mut owner = allocation;
        let mut generation = 1_u64;
        for (successor, cause) in root.lineage.into_iter().skip(1) {
            generation += 1;
            let sequence = generation;
            insert_event(
                &tx,
                EventInput {
                    custody_id: root.custody_id,
                    sequence,
                    event_kind: "transferred",
                    cause,
                    from_generation: Some(generation - 1),
                    to_generation: generation,
                    from_owner: Some(owner),
                    to_owner: Some(successor),
                    origin: Some(owner),
                    scheduled_job: None,
                    prior_state: Some("live"),
                    next_state: "live",
                    error_code: None,
                    occurred_at: &now,
                },
            )?;
            tx.execute("UPDATE sandbox_custody_roots SET owner_session_id=?2,generation=?3,event_sequence=?4,validated_generation=?3,validated_at=?5,updated_at=?5 WHERE custody_id=?1 AND owner_session_id=?6 AND generation=?7", params![root.custody_id.to_string(), successor.to_string(), generation as i64, sequence as i64, now, owner.to_string(), (generation - 1) as i64])?;
            tx.execute("UPDATE sessions SET sandbox_cleanup_state='Live',sandbox_custody_id=?2,updated_at=?3 WHERE id=?1", params![successor.to_string(), root.custody_id.to_string(), now])?;
            tx.execute("UPDATE session_execution_projections SET execution_state='historical_transferred',freshness='verified',effective_cwd=NULL,custody_id=?2,custody_generation=?3,validated_at=?4,error_code=NULL,updated_at=?4 WHERE session_id=?1", params![owner.to_string(), root.custody_id.to_string(), (generation - 1) as i64, now])?;
            publish_verified(&tx, successor, root.custody_id, generation, None)?;
            owner = successor;
        }
        tx.commit()?;
        Ok(())
    }

    /// Retained terminal history is represented by a normal immutable
    /// allocation followed by its terminal event. This keeps the root
    /// ownerless without pretending it is executable.
    pub(crate) fn reconstruct_legacy_terminal_root(
        &mut self,
        root: LegacyTerminalRoot,
    ) -> Result<()> {
        let _root_guard = lock_custody_root(root.custody_id);
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_terminal_session_matches_root(
            &tx,
            root.allocation_session_id,
            &root.canonical_repo_dir,
            &root.sandbox_root,
            &root.sandbox_branch,
            root.cleanup_state,
        )?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sandbox_custody_roots WHERE sandbox_root=?1)",
            [&root.sandbox_root],
            |row| row.get(0),
        )?;
        if exists {
            return Err(DaemonError::Store(
                "legacy terminal root already has custody aggregate".into(),
            ));
        }
        let now = timestamp();
        let (state, event_kind, projection, error) = match root.cleanup_state {
            SandboxCleanupState::Purged => ("purged", "tombstoned", "historical_purged", None),
            SandboxCleanupState::Failed => (
                "failed",
                "failed",
                "historical_cleanup_failed",
                Some(SandboxCustodyErrorCodeV1::CleanupFailed.as_str()),
            ),
            SandboxCleanupState::Live => {
                return Err(DaemonError::Store(
                    "live tuple is not terminal legacy history".into(),
                ));
            }
            _ => {
                return Err(DaemonError::Store(
                    "unknown cleanup state is not terminal legacy history".into(),
                ));
            }
        };
        tx.execute("INSERT INTO sandbox_custody_roots (custody_id,allocation_id,canonical_repo_dir,sandbox_root,sandbox_branch,repository_identity,source_commit,state,owner_session_id,generation,event_sequence,validation_state,validated_generation,validated_at,validation_error_code,effect_boot_id,reserved_effects,active_effects,created_at,updated_at,tombstoned_at) VALUES (?1,?2,?3,?4,?5,?6,?7,'live',?8,1,1,'verified',1,?9,NULL,NULL,0,0,?9,?9,NULL)", params![root.custody_id.to_string(),root.allocation_session_id.to_string(),root.canonical_repo_dir,root.sandbox_root,root.sandbox_branch,root.repository_identity,root.source_commit,root.allocation_session_id.to_string(),now])?;
        insert_event(
            &tx,
            EventInput {
                custody_id: root.custody_id,
                sequence: 1,
                event_kind: "allocated",
                cause: CustodyCause::StartupReconciliation,
                from_generation: None,
                to_generation: 1,
                from_owner: None,
                to_owner: Some(root.allocation_session_id),
                origin: None,
                scheduled_job: None,
                prior_state: None,
                next_state: "live",
                error_code: None,
                occurred_at: &now,
            },
        )?;
        insert_event(
            &tx,
            EventInput {
                custody_id: root.custody_id,
                sequence: 2,
                event_kind,
                cause: CustodyCause::StartupReconciliation,
                from_generation: Some(1),
                to_generation: 2,
                from_owner: Some(root.allocation_session_id),
                to_owner: None,
                origin: None,
                scheduled_job: None,
                prior_state: Some("live"),
                next_state: state,
                error_code: error,
                occurred_at: &now,
            },
        )?;
        tx.execute("UPDATE sandbox_custody_roots SET state=?2,owner_session_id=NULL,generation=2,event_sequence=2,validated_generation=2,validated_at=?3,updated_at=?3,tombstoned_at=?3 WHERE custody_id=?1", params![root.custody_id.to_string(), state, now])?;
        if root.cleanup_state == SandboxCleanupState::Purged {
            tx.execute("UPDATE sessions SET sandbox_root=NULL,sandbox_branch=NULL,sandbox_custody_id=?2,updated_at=?3 WHERE id=?1", params![root.allocation_session_id.to_string(), root.custody_id.to_string(), now])?;
        } else {
            tx.execute(
                "UPDATE sessions SET sandbox_custody_id=?2,updated_at=?3 WHERE id=?1",
                params![
                    root.allocation_session_id.to_string(),
                    root.custody_id.to_string(),
                    now
                ],
            )?;
        }
        tx.execute("UPDATE session_execution_projections SET execution_state=?2,freshness='verified',effective_cwd=NULL,custody_id=?3,custody_generation=2,validated_at=?4,error_code=NULL,updated_at=?4 WHERE session_id=?1", params![root.allocation_session_id.to_string(), projection, root.custody_id.to_string(), now])?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn reserve_effect(
        &mut self,
        custody_id: Uuid,
        expected_generation: u64,
        boot_id: Uuid,
    ) -> Result<EffectReservation> {
        let _root_guard = lock_custody_root(custody_id);
        self.reserve_effect_locked(custody_id, expected_generation, boot_id)
    }

    pub(crate) fn reserve_effect_locked(
        &mut self,
        custody_id: Uuid,
        expected_generation: u64,
        boot_id: Uuid,
    ) -> Result<EffectReservation> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        refuse_prepared_reclaim_on(
            &tx,
            custody_id,
            expected_generation,
            SandboxCustodyTransitionV1::EffectRevalidation,
        )?;
        let changed = tx.execute(
            "UPDATE sandbox_custody_roots SET reserved_effects=reserved_effects+1, effect_boot_id=?3, updated_at=?4 WHERE custody_id=?1 AND generation=?2 AND state='live' AND validation_state='verified' AND (effect_boot_id IS NULL OR effect_boot_id=?3)",
            params![custody_id.to_string(), expected_generation as i64, boot_id.to_string(), timestamp()],
        )?;
        if changed != 1 {
            return Err(DaemonError::Store(
                "custody effect reservation fence lost".into(),
            ));
        }
        tx.commit()?;
        Ok(EffectReservation {
            custody_id,
            generation: expected_generation,
            boot_id,
        })
    }

    pub(crate) fn settle_effect(
        &mut self,
        reservation: EffectReservation,
        activate: bool,
    ) -> Result<()> {
        let _root_guard = lock_custody_root(reservation.custody_id);
        self.settle_effect_locked(reservation, activate)
    }

    pub(crate) fn settle_effect_locked(
        &mut self,
        reservation: EffectReservation,
        activate: bool,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let sql = if activate {
            "UPDATE sandbox_custody_roots SET reserved_effects=reserved_effects-1, active_effects=active_effects+1, updated_at=?4 WHERE custody_id=?1 AND generation=?2 AND effect_boot_id=?3 AND reserved_effects>0"
        } else {
            "UPDATE sandbox_custody_roots SET reserved_effects=reserved_effects-1, effect_boot_id=CASE WHEN reserved_effects=1 AND active_effects=0 THEN NULL ELSE effect_boot_id END, updated_at=?4 WHERE custody_id=?1 AND generation=?2 AND effect_boot_id=?3 AND reserved_effects>0"
        };
        if tx.execute(
            sql,
            params![
                reservation.custody_id.to_string(),
                reservation.generation as i64,
                reservation.boot_id.to_string(),
                timestamp()
            ],
        )? != 1
        {
            return Err(DaemonError::Store(
                "custody effect settlement fence lost".into(),
            ));
        }
        tx.commit()?;
        Ok(())
    }

    /// End an active daemon-controlled effect.  Clearing `effect_boot_id`
    /// exactly when the final permit settles makes restart quarantine precise.
    pub(crate) fn release_effect(&mut self, reservation: EffectReservation) -> Result<()> {
        let _root_guard = lock_custody_root(reservation.custody_id);
        self.release_effect_locked(reservation)
    }

    fn release_effect_locked(&mut self, reservation: EffectReservation) -> Result<()> {
        #[cfg(test)]
        if FAIL_NEXT_EFFECT_RELEASES
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
        {
            return Err(DaemonError::Store(
                "injected custody effect release failure".into(),
            ));
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let root = load_root(&tx, reservation.custody_id)?;
        if root.generation != reservation.generation
            || root.effect_boot_id != Some(reservation.boot_id)
            || root.active_effects == 0
        {
            return Err(DaemonError::Store(
                "custody active-effect release fence lost".into(),
            ));
        }
        if root.validation_state == "invalid"
            && root.active_effects == 1
            && root.reserved_effects == 0
        {
            let now = timestamp();
            quarantine_invalid_root_on(
                &tx,
                reservation.custody_id,
                &root,
                root.event_sequence,
                &now,
                CustodyCause::EffectRevalidation,
                SandboxCustodyErrorCodeV1::CustodyChanged,
            )?;
            tx.commit()?;
            return Ok(());
        }
        if tx.execute(
            "UPDATE sandbox_custody_roots SET active_effects=active_effects-1, effect_boot_id=CASE WHEN active_effects=1 AND reserved_effects=0 THEN NULL ELSE effect_boot_id END, updated_at=?4 WHERE custody_id=?1 AND generation=?2 AND effect_boot_id=?3 AND active_effects>0",
            params![reservation.custody_id.to_string(), reservation.generation as i64, reservation.boot_id.to_string(), timestamp()],
        )? != 1 {
            return Err(DaemonError::Store("custody active-effect release fence lost".into()));
        }
        tx.commit()?;
        Ok(())
    }

    /// Settle a domain-reserved row that failed custody binding.  It never
    /// attaches a root or publishes an executable projection.
    pub(crate) fn settle_reserved_custody_failure(
        &mut self,
        session_id: Uuid,
        code: SandboxCustodyErrorCodeV1,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = timestamp();
        let changed = tx.execute(
            "UPDATE sessions SET status='Failed', stop_reason=?2, updated_at=?3 WHERE id=?1 AND status='Starting' AND sandbox_custody_id IS NULL",
            params![session_id.to_string(), format!("sandbox_custody:{}", code.as_str()), now],
        )?;
        if changed == 0 {
            tx.commit()?;
            return Ok(());
        }
        tx.execute(
            "UPDATE session_execution_projections SET execution_state='invalid', freshness='invalid', effective_cwd=NULL, custody_id=NULL, custody_generation=NULL, validated_at=?2, error_code=?3, updated_at=?2 WHERE session_id=?1",
            params![session_id.to_string(), timestamp(), code.as_str()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Settle an attempted rotation successor that never acquired custody.
    /// Unlike the generic domain helper, this is not idempotent: the caller
    /// must prove that the exact Starting lineage row and its trigger-created
    /// non-authoritative projection were both changed in this transaction.
    pub(crate) fn settle_reserved_rotation_custody_failure(
        &mut self,
        session_id: Uuid,
        code: SandboxCustodyErrorCodeV1,
    ) -> Result<()> {
        self.settle_reserved_successor_custody_failure(session_id, None, code, "rotation")
    }

    pub(crate) fn settle_reserved_retry_custody_failure(
        &mut self,
        session_id: Uuid,
        source_session_id: Uuid,
        code: SandboxCustodyErrorCodeV1,
    ) -> Result<()> {
        self.settle_reserved_successor_custody_failure(
            session_id,
            Some(source_session_id),
            code,
            "retry",
        )
    }

    fn settle_reserved_successor_custody_failure(
        &mut self,
        session_id: Uuid,
        expected_source_id: Option<Uuid>,
        code: SandboxCustodyErrorCodeV1,
        transition: &str,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = timestamp();
        let changed = tx.execute(
            "UPDATE sessions SET status='Failed', stop_reason=?2, updated_at=?3
             WHERE id=?1 AND continued_from IS NOT NULL
               AND (?4 IS NULL OR continued_from=?4) AND status='Starting'
               AND sandbox_custody_id IS NULL
               AND (
                 (sandbox_kind IS NULL AND sandbox_root IS NULL
                  AND sandbox_branch IS NULL AND sandbox_cleanup_state IS NULL)
                 OR
                 (sandbox_kind='GitWorktree' AND sandbox_root IS NOT NULL
                  AND sandbox_branch IS NOT NULL AND sandbox_cleanup_state='Live')
               )
               AND EXISTS (
                 SELECT 1 FROM session_execution_projections p
                 WHERE p.session_id=sessions.id
                   AND p.schema_version=1 AND p.projection_version=1
                   AND p.freshness='unverified' AND p.effective_cwd IS NULL
                   AND p.custody_id IS NULL AND p.custody_generation IS NULL
                   AND p.validated_at IS NULL AND p.error_code IS NULL
                   AND p.canonical_repo_dir=sessions.working_dir
                   AND (
                     (sessions.sandbox_kind IS NULL
                      AND p.execution_state='ordinary_unsandboxed')
                     OR
                     (sessions.sandbox_kind='GitWorktree'
                      AND p.execution_state='live_sandboxed')
                   )
               )",
            params![
                session_id.to_string(),
                format!("sandbox_custody:{}", code.as_str()),
                now,
                expected_source_id.map(|id| id.to_string()),
            ],
        )?;
        if changed != 1 {
            return Err(DaemonError::Store(format!(
                "{transition} reservation failure lost exact Starting lineage fence"
            )));
        }
        let projection = tx.execute(
            "UPDATE session_execution_projections
             SET execution_state='invalid', freshness='invalid', effective_cwd=NULL,
                 custody_id=NULL, custody_generation=NULL, validated_at=?3,
                 error_code=?4, updated_at=?3
             WHERE session_id=?1 AND canonical_repo_dir=?2
               AND execution_state IN ('ordinary_unsandboxed','live_sandboxed')
               AND freshness='unverified'
               AND effective_cwd IS NULL AND custody_id IS NULL
               AND custody_generation IS NULL AND validated_at IS NULL
               AND error_code IS NULL
               AND EXISTS (
                 SELECT 1 FROM sessions s
                 WHERE s.id=session_execution_projections.session_id
                   AND s.status='Failed' AND s.continued_from IS NOT NULL
                   AND s.sandbox_custody_id IS NULL
                   AND (
                     (s.sandbox_kind IS NULL AND s.sandbox_root IS NULL
                      AND s.sandbox_branch IS NULL AND s.sandbox_cleanup_state IS NULL
                      AND session_execution_projections.execution_state='ordinary_unsandboxed')
                     OR
                     (s.sandbox_kind='GitWorktree' AND s.sandbox_root IS NOT NULL
                      AND s.sandbox_branch IS NOT NULL AND s.sandbox_cleanup_state='Live'
                      AND session_execution_projections.execution_state='live_sandboxed')
                   )
               )",
            params![
                session_id.to_string(),
                tx.query_row(
                    "SELECT working_dir FROM sessions WHERE id=?1",
                    [session_id.to_string()],
                    |row| row.get::<_, String>(0),
                )?,
                now,
                code.as_str(),
            ],
        )?;
        if projection != 1 {
            return Err(DaemonError::Store(format!(
                "{transition} reservation failure lost exact unverified projection fence"
            )));
        }
        tx.commit()?;
        Ok(())
    }

    /// Root-scoped logical tombstone.  Session history and the aggregate stay
    /// durable; only executable paths are atomically removed.
    pub(crate) fn tombstone_custody_root(
        &mut self,
        custody_id: Uuid,
        expected_generation: u64,
        cause: CustodyCause,
    ) -> Result<()> {
        let _root_guard = lock_custody_root(custody_id);
        transition_terminal_root(
            self,
            custody_id,
            expected_generation,
            cause,
            "purged",
            "tombstoned",
            "historical_purged",
            None,
        )
    }

    pub(crate) fn record_custody_cleanup_failure(
        &mut self,
        custody_id: Uuid,
        expected_generation: u64,
        code: SandboxCustodyErrorCodeV1,
    ) -> Result<()> {
        let _root_guard = lock_custody_root(custody_id);
        transition_terminal_root(
            self,
            custody_id,
            expected_generation,
            CustodyCause::CleanupFailure,
            "failed",
            "failed",
            "historical_cleanup_failed",
            Some(code),
        )
    }

    /// Return the authenticated sandbox root for target reclamation.  The
    /// caller must retain the same root guard through filesystem revalidation
    /// and removal; this helper is intentionally locked-only.
    pub(crate) fn reclaim_terminal_target_locked(
        &self,
        custody_id: Uuid,
        expected_generation: u64,
    ) -> Result<Option<ReclaimTarget>> {
        self.conn
            .query_row(
                "SELECT r.canonical_repo_dir,r.sandbox_root,r.sandbox_branch,r.repository_identity,r.source_commit,r.allocation_id,r.owner_session_id,s.id,r.validated_generation
                 FROM sandbox_custody_roots r
                 JOIN sessions s ON s.id=r.owner_session_id AND s.sandbox_custody_id=r.custody_id
                 WHERE r.custody_id=?1 AND r.generation=?2 AND r.state='live'
                   AND r.validation_state='verified' AND r.validated_generation=r.generation
                   AND r.reserved_effects=0 AND r.active_effects=0
                   AND s.status IN ('Completed','Failed','Interrupted','Archived','Deleted')
                   AND s.working_dir=r.canonical_repo_dir
                   AND s.sandbox_kind='GitWorktree'
                   AND s.sandbox_root=r.sandbox_root
                   AND s.sandbox_branch=r.sandbox_branch
                   AND s.sandbox_cleanup_state='Live'",
                params![custody_id.to_string(), expected_generation as i64],
                |row| {
                    let allocation_id: String = row.get(5)?;
                    let owner_session_id: String = row.get(6)?;
                    let session_id: String = row.get(7)?;
                    Ok(ReclaimTarget {
                        canonical_repo_dir: PathBuf::from(row.get::<_, String>(0)?),
                        sandbox_root: PathBuf::from(row.get::<_, String>(1)?),
                        sandbox_branch: row.get(2)?,
                        repository_identity: row.get(3)?,
                        source_commit: row.get(4)?,
                        allocation_id: Uuid::parse_str(&allocation_id).map_err(|error| rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Text, Box::new(error)))?,
                        owner_session_id: Uuid::parse_str(&owner_session_id).map_err(|error| rusqlite::Error::FromSqlConversionFailure(6, rusqlite::types::Type::Text, Box::new(error)))?,
                        session_id: Uuid::parse_str(&session_id).map_err(|error| rusqlite::Error::FromSqlConversionFailure(7, rusqlite::types::Type::Text, Box::new(error)))?,
                        validated_generation: row.get::<_, i64>(8)? as u64,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    /// Reconcile effects left by another process boot. Process death cannot
    /// run permit Drop, so stale counters are durably quarantined instead of
    /// being reset as though they settled normally. Ordering is stable and
    /// `limit` bounds each transaction batch.
    pub(crate) fn reconcile_stale_effect_boots(
        &mut self,
        boot_id: Uuid,
        limit: usize,
    ) -> Result<usize> {
        let ids: Vec<Uuid> = {
            let mut stmt = self.conn.prepare(
                "SELECT custody_id FROM sandbox_custody_roots WHERE effect_boot_id IS NOT NULL AND effect_boot_id != ?1 ORDER BY custody_id LIMIT ?2",
            )?;
            stmt.query_map(params![boot_id.to_string(), limit as i64], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
            .into_iter()
            .map(|value| {
                Uuid::parse_str(&value).map_err(|error| DaemonError::Store(error.to_string()))
            })
            .collect::<Result<Vec<_>>>()?
        };
        let mut reconciled = 0;
        for custody_id in ids {
            let _root_guard = lock_custody_root(custody_id);
            let tx = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let root = load_root(&tx, custody_id)?;
            if root.effect_boot_id.is_some_and(|id| id != boot_id) {
                let now = timestamp();
                tx.execute(
                    "UPDATE session_execution_projections SET execution_state='invalid', freshness='invalid', effective_cwd=NULL, validated_at=?2, error_code=?3, updated_at=?2 WHERE custody_id=?1",
                    params![custody_id.to_string(), now, SandboxCustodyErrorCodeV1::CustodyChanged.as_str()],
                )?;
                quarantine_invalid_root_on(
                    &tx,
                    custody_id,
                    &root,
                    root.event_sequence,
                    &now,
                    CustodyCause::StartupReconciliation,
                    SandboxCustodyErrorCodeV1::CustodyChanged,
                )?;
                reconciled += 1;
            }
            tx.commit()?;
        }
        Ok(reconciled)
    }

    /// Finish startup reconciliation before restore is allowed to advance.
    /// Each query/transaction batch remains bounded, but this gate owns the
    /// complete deterministic keyset walk so callers never need a second
    /// restore invocation to become ready.
    pub(crate) fn reconcile_stale_effect_boots_complete(
        &mut self,
        boot_id: Uuid,
        limit: usize,
    ) -> Result<bool> {
        let limit = limit.max(1);
        loop {
            if self.reconcile_stale_effect_boots(boot_id, limit)? == 0 {
                return Ok(true);
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReclaimTarget {
    pub canonical_repo_dir: PathBuf,
    pub sandbox_root: PathBuf,
    pub sandbox_branch: String,
    pub repository_identity: String,
    pub source_commit: String,
    pub allocation_id: Uuid,
    pub owner_session_id: Uuid,
    pub session_id: Uuid,
    pub validated_generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TerminalCustodyReclaimCandidate {
    pub custody_id: Uuid,
    pub generation: u64,
    pub session_id: Uuid,
    pub updated_at: String,
}

#[derive(Debug)]
struct RootRow {
    owner_session_id: Option<Uuid>,
    generation: u64,
    event_sequence: u64,
    state: RootState,
    validation_state: String,
    effect_boot_id: Option<Uuid>,
    reserved_effects: u64,
    active_effects: u64,
}
#[derive(Debug, Clone, Copy)]
enum RootState {
    Live,
    Purged,
    Failed,
    Quarantined,
}
impl RootState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Purged => "purged",
            Self::Failed => "failed",
            Self::Quarantined => "quarantined",
        }
    }
}

fn load_root(tx: &Transaction<'_>, custody_id: Uuid) -> Result<RootRow> {
    tx.query_row("SELECT owner_session_id,generation,event_sequence,state,validation_state,effect_boot_id,reserved_effects,active_effects FROM sandbox_custody_roots WHERE custody_id=?1", [custody_id.to_string()], |r| {
        let owner: Option<String> = r.get(0)?;
        let state: String = r.get(3)?;
        let validation_state: String = r.get(4)?;
        let boot: Option<String> = r.get(5)?;
        Ok((owner, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, state, validation_state, boot, r.get::<_, i64>(6)?, r.get::<_, i64>(7)?))
    }).optional()?.ok_or_else(|| DaemonError::Store("sandbox custody root missing".into())).and_then(|(owner,generation,sequence,state,validation_state,boot,reserved_effects,active_effects)| {
        Ok(RootRow { owner_session_id: owner.map(|value| Uuid::parse_str(&value).map_err(|e| DaemonError::Store(e.to_string()))).transpose()?, generation: generation as u64, event_sequence: sequence as u64, state: match state.as_str() { "live" => RootState::Live, "purged" => RootState::Purged, "failed" => RootState::Failed, "quarantined" => RootState::Quarantined, _ => return Err(DaemonError::Store("invalid custody state".into())) }, validation_state, effect_boot_id: boot.map(|value| Uuid::parse_str(&value).map_err(|e| DaemonError::Store(e.to_string()))).transpose()?, reserved_effects: reserved_effects as u64, active_effects: active_effects as u64 })
    })
}

/// The invalidation transaction has already removed every effective cwd. Once
/// its final effect drains (or immediately when there was none), this turns
/// the root into durable ownerless quarantine in the same transaction.
fn quarantine_invalid_root_on(
    tx: &Transaction<'_>,
    custody_id: Uuid,
    root: &RootRow,
    prior_sequence: u64,
    now: &str,
    cause: CustodyCause,
    error_code: SandboxCustodyErrorCodeV1,
) -> Result<()> {
    let owner = root
        .owner_session_id
        .ok_or_else(|| DaemonError::Store("invalid custody root has no owner".into()))?;
    let sequence = prior_sequence + 1;
    let next_generation = root.generation + 1;
    insert_event(
        tx,
        EventInput {
            custody_id,
            sequence,
            event_kind: "quarantined",
            cause,
            from_generation: Some(root.generation),
            to_generation: next_generation,
            from_owner: Some(owner),
            to_owner: None,
            origin: None,
            scheduled_job: None,
            prior_state: Some(root.state.as_str()),
            next_state: "quarantined",
            error_code: Some(error_code.as_str()),
            occurred_at: now,
        },
    )?;
    if tx.execute(
        "UPDATE sandbox_custody_roots SET state='quarantined', owner_session_id=NULL, generation=?2, event_sequence=?3, validation_state='invalid', validated_generation=?2, validated_at=?4, validation_error_code=?5, effect_boot_id=NULL, reserved_effects=0, active_effects=0, updated_at=?4 WHERE custody_id=?1 AND owner_session_id=?6 AND generation=?7 AND reserved_effects=?8 AND active_effects=?9",
        params![custody_id.to_string(), next_generation as i64, sequence as i64, now, error_code.as_str(), owner.to_string(), root.generation as i64, root.reserved_effects as i64, root.active_effects as i64],
    )? != 1 {
        return Err(DaemonError::Store("custody quarantine compare-and-swap lost".into()));
    }
    tx.execute(
        "UPDATE sessions SET status='Failed', stop_reason=?2, updated_at=?3 WHERE sandbox_custody_id=?1 AND status IN ('Starting','Running','WaitingApproval','Failed')",
        params![custody_id.to_string(), format!("sandbox_custody:{}", error_code.as_str()), now],
    )?;
    // A quarantined aggregate is retained history, never an executable Live
    // tuple.  Stamp the compatible cleanup field in the same transaction so
    // a later startup group pass cannot reinterpret it as a live candidate
    // and overwrite the original custody_changed projection/stop reason.
    tx.execute(
        "UPDATE sessions SET sandbox_cleanup_state='Failed', updated_at=?2 WHERE sandbox_custody_id=?1",
        params![custody_id.to_string(), now],
    )?;
    Ok(())
}

fn transition_terminal_root(
    store: &mut Store,
    custody_id: Uuid,
    expected_generation: u64,
    cause: CustodyCause,
    next_state: &str,
    event_kind: &str,
    projection_state: &str,
    error_code: Option<SandboxCustodyErrorCodeV1>,
) -> Result<()> {
    let tx = store
        .conn
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    transition_terminal_root_tx(
        &tx,
        custody_id,
        expected_generation,
        cause,
        next_state,
        event_kind,
        projection_state,
        error_code,
    )?;
    tx.commit()?;
    Ok(())
}

/// Transaction-composable terminal transition used when archival, custody,
/// execution projection, and an external-effect receipt must commit together.
pub(crate) fn transition_terminal_root_tx(
    tx: &Transaction<'_>,
    custody_id: Uuid,
    expected_generation: u64,
    cause: CustodyCause,
    next_state: &str,
    event_kind: &str,
    projection_state: &str,
    error_code: Option<SandboxCustodyErrorCodeV1>,
) -> Result<usize> {
    refuse_prepared_reclaim_on(
        tx,
        custody_id,
        expected_generation,
        SandboxCustodyTransitionV1::Purge,
    )?;
    let root = load_root(tx, custody_id)?;
    let owner = root
        .owner_session_id
        .ok_or_else(|| DaemonError::Store("custody terminal transition has no owner".into()))?;
    if root.generation != expected_generation
        || !matches!(root.state, RootState::Live)
        || root.reserved_effects != 0
        || root.active_effects != 0
    {
        return Err(DaemonError::Store(
            "custody terminal transition fence lost".into(),
        ));
    }
    let linked_sessions = terminal_owner_and_linked_sessions(tx, custody_id, owner)?;
    let now = timestamp();
    let next_generation = root.generation + 1;
    let next_sequence = root.event_sequence + 1;
    insert_event(
        tx,
        EventInput {
            custody_id,
            sequence: next_sequence,
            event_kind,
            cause,
            from_generation: Some(root.generation),
            to_generation: next_generation,
            from_owner: Some(owner),
            to_owner: None,
            origin: None,
            scheduled_job: None,
            prior_state: Some("live"),
            next_state,
            error_code: error_code.map(SandboxCustodyErrorCodeV1::as_str),
            occurred_at: &now,
        },
    )?;
    if tx.execute(
        "UPDATE sandbox_custody_roots SET state=?2, owner_session_id=NULL, generation=?3, event_sequence=?4, validation_state='verified', validated_generation=?3, validated_at=?5, validation_error_code=NULL, effect_boot_id=NULL, tombstoned_at=?5, updated_at=?5 WHERE custody_id=?1 AND owner_session_id=?6 AND generation=?7 AND reserved_effects=0 AND active_effects=0",
        params![custody_id.to_string(), next_state, next_generation as i64, next_sequence as i64, now, owner.to_string(), expected_generation as i64],
    )? != 1 {
        return Err(DaemonError::Store("custody terminal compare-and-swap lost".into()));
    }
    if next_state == "purged" {
        let changed = tx.execute(
            "UPDATE sessions SET sandbox_cleanup_state='Purged', sandbox_root=NULL, sandbox_branch=NULL, updated_at=?2 WHERE sandbox_custody_id=?1",
            params![custody_id.to_string(), now],
        )?;
        if changed != linked_sessions {
            return Err(DaemonError::Store(
                "custody terminal session settlement incomplete".into(),
            ));
        }
    } else {
        let changed = tx.execute(
            "UPDATE sessions SET sandbox_cleanup_state='Failed', updated_at=?2 WHERE sandbox_custody_id=?1",
            params![custody_id.to_string(), now],
        )?;
        if changed != linked_sessions {
            return Err(DaemonError::Store(
                "custody terminal session settlement incomplete".into(),
            ));
        }
    }
    let projections = tx.execute(
        "UPDATE session_execution_projections SET execution_state=?2, freshness='verified', effective_cwd=NULL, custody_id=?1, custody_generation=?3, validated_at=?4, error_code=NULL, updated_at=?4 WHERE session_id IN (SELECT id FROM sessions WHERE sandbox_custody_id=?1)",
        params![custody_id.to_string(), projection_state, next_generation as i64, now],
    )?;
    if projections != linked_sessions {
        return Err(DaemonError::Store(
            "custody terminal projection settlement incomplete".into(),
        ));
    }
    Ok(linked_sessions)
}

/// Authenticate the live owner and every SQL-linked custody participant before
/// a terminal event can normalize the aggregate into historical state. Terminal
/// transitions deliberately perform no filesystem or Git work: this is a
/// logical custody boundary.
fn terminal_owner_and_linked_sessions(
    tx: &Transaction<'_>,
    custody_id: Uuid,
    owner: Uuid,
) -> Result<usize> {
    let owner_matches: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sessions s JOIN sandbox_custody_roots r ON r.custody_id=?2 WHERE s.id=?1 AND s.sandbox_custody_id=r.custody_id AND s.working_dir=r.canonical_repo_dir AND s.sandbox_kind='GitWorktree' AND s.sandbox_root=r.sandbox_root AND s.sandbox_branch=r.sandbox_branch AND s.sandbox_cleanup_state='Live')",
        params![owner.to_string(), custody_id.to_string()],
        |row| row.get(0),
    )?;
    if !owner_matches {
        return Err(DaemonError::Store(
            "custody terminal owner link or tuple mismatch".into(),
        ));
    }
    let linked_sessions: i64 = tx.query_row(
        "SELECT count(*) FROM sessions WHERE sandbox_custody_id=?1",
        [custody_id.to_string()],
        |row| row.get(0),
    )?;
    let linked_sessions = usize::try_from(linked_sessions)
        .map_err(|_| DaemonError::Store("custody terminal linked session count overflow".into()))?;
    let tuple_matches: i64 = tx.query_row(
        "SELECT count(*) FROM sessions s JOIN sandbox_custody_roots r ON r.custody_id=?1 WHERE s.sandbox_custody_id=?1 AND s.working_dir=r.canonical_repo_dir AND s.sandbox_kind='GitWorktree' AND s.sandbox_root=r.sandbox_root AND s.sandbox_branch=r.sandbox_branch AND s.sandbox_cleanup_state='Live'",
        [custody_id.to_string()],
        |row| row.get(0),
    )?;
    if usize::try_from(tuple_matches)
        .map_err(|_| DaemonError::Store("custody terminal linked session count overflow".into()))?
        != linked_sessions
    {
        return Err(DaemonError::Store(
            "custody terminal linked participant tuple mismatch".into(),
        ));
    }
    let authenticated_history: i64 = tx.query_row(
        "SELECT count(*) FROM sessions s WHERE s.sandbox_custody_id=?1 AND (SELECT count(*) FROM sandbox_custody_events e WHERE e.custody_id=?1 AND e.event_kind IN ('allocated','transferred') AND e.to_owner_session_id=s.id)=1",
        [custody_id.to_string()],
        |row| row.get(0),
    )?;
    if usize::try_from(authenticated_history)
        .map_err(|_| DaemonError::Store("custody terminal linked session count overflow".into()))?
        != linked_sessions
    {
        return Err(DaemonError::Store(
            "custody terminal linked participant ownership history mismatch".into(),
        ));
    }
    Ok(linked_sessions)
}

fn bind_reserved_session_custody_on(
    tx: &Transaction<'_>,
    session_id: Uuid,
    binding: SessionCustodyBinding,
) -> Result<()> {
    let exists = tx
        .query_row(
            "SELECT 1 FROM session_execution_projections WHERE session_id=?1 AND freshness='unverified'",
            [session_id.to_string()],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !exists {
        return Err(DaemonError::Store(
            "reserved session lacks V83 unverified projection".into(),
        ));
    }
    bind_on(tx, session_id, binding)
}

pub(super) fn rotation_session_authority(session: &Session) -> Result<Vec<u8>> {
    let mut value = serde_json::to_value(session)
        .map_err(|error| DaemonError::Store(format!("serialize rotation authority: {error}")))?;
    let object = value.as_object_mut().ok_or_else(|| {
        DaemonError::Store("serialized rotation authority was not an object".into())
    })?;
    for field in ROTATION_NON_AUTHORITY_SESSION_FIELDS {
        object.remove(*field);
    }
    serde_json::to_vec(&value)
        .map_err(|error| DaemonError::Store(format!("encode rotation authority: {error}")))
}

fn retry_session_authority_after_c5(session: &Session) -> Result<Vec<u8>> {
    let mut value = serde_json::to_value(session)
        .map_err(|error| DaemonError::Store(format!("serialize retry authority: {error}")))?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| DaemonError::Store("serialized retry authority was not an object".into()))?;
    // These are the only Session fields changed by admit_c5_retry_successor.
    object.remove("updated_at");
    object.remove("retry_attempt");
    object.remove("max_retries");
    serde_json::to_vec(&value)
        .map_err(|error| DaemonError::Store(format!("encode retry authority: {error}")))
}

fn ordinary_session_shape(session: &Session) -> bool {
    session.sandbox_kind.is_none()
        && session.sandbox_root.is_none()
        && session.sandbox_branch.is_none()
        && session.sandbox_cleanup_state.is_none()
}

pub(super) fn load_rotation_authority_session_on(
    conn: &rusqlite::Connection,
    session_id: Uuid,
) -> Result<Option<(Session, Option<Uuid>, Option<Uuid>, String)>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {SESSION_COLUMNS}, sandbox_custody_id, model_invocation_id, tag \
         FROM sessions WHERE id=?1"
    ))?;
    let row = stmt
        .query_row([session_id.to_string()], |row| {
            // Indices are derived from the session block's width, not
            // hardcoded: `SESSION_COLUMNS` grows, and a stale literal here
            // reads the wrong column with the wrong type.
            Ok((
                map_session_row(row)?,
                row.get::<_, Option<String>>(SESSION_COLUMN_COUNT)?,
                row.get::<_, Option<String>>(SESSION_COLUMN_COUNT + 1)?,
                row.get::<_, String>(SESSION_COLUMN_COUNT + 2)?,
            ))
        })
        .optional()?;
    let Some((row, custody_id, model_invocation_id, legacy_primary_tag)) = row else {
        return Ok(None);
    };
    let mut session = row.into_session()?;
    let mut tags =
        conn.prepare("SELECT tag FROM session_tags WHERE session_id=?1 ORDER BY tag ASC")?;
    session.tags = tags
        .query_map([session_id.to_string()], |row| row.get(0))?
        .collect::<std::result::Result<Vec<String>, _>>()?;
    session.tag = session.tags.first().cloned().unwrap_or_default();
    let custody_id = custody_id
        .as_deref()
        .map(Uuid::parse_str)
        .transpose()
        .map_err(|error| DaemonError::Store(format!("invalid sandbox custody UUID: {error}")))?;
    let model_invocation_id = model_invocation_id
        .as_deref()
        .map(Uuid::parse_str)
        .transpose()
        .map_err(|error| DaemonError::Store(format!("invalid model invocation UUID: {error}")))?;
    Ok(Some((
        session,
        custody_id,
        model_invocation_id,
        legacy_primary_tag,
    )))
}

fn rotation_authority_matches_on(
    conn: &rusqlite::Connection,
    expected: &RotationAuthorityFence,
    predecessor_id: Uuid,
    expected_status: SessionStatus,
) -> Result<bool> {
    if predecessor_id != expected.predecessor_id {
        return Ok(false);
    }
    let Some((current, sandbox_custody_id, model_invocation_id, legacy_primary_tag)) =
        load_rotation_authority_session_on(conn, predecessor_id)?
    else {
        return Ok(false);
    };
    Ok(current.status == expected_status
        && rotation_session_authority(&current)? == expected.session_authority
        && sandbox_custody_id == expected.sandbox_custody_id
        && model_invocation_id == expected.model_invocation_id
        && legacy_primary_tag == expected.legacy_primary_tag)
}

fn retry_authority_matches_after_c5_on(
    conn: &rusqlite::Connection,
    expected: &RetryAuthorityFence,
) -> Result<bool> {
    let Some((current, sandbox_custody_id, model_invocation_id, legacy_primary_tag)) =
        load_rotation_authority_session_on(conn, expected.source_session_id)?
    else {
        return Ok(false);
    };
    Ok(current.status == SessionStatus::Failed
        && current.retry_attempt == Some(expected.max_retries)
        && current.max_retries == Some(expected.max_retries)
        && retry_session_authority_after_c5(&current)? == expected.session_authority_after_c5
        && sandbox_custody_id == expected.sandbox_custody_id
        && model_invocation_id == expected.model_invocation_id
        && legacy_primary_tag == expected.legacy_primary_tag)
}

/// Stage or authenticate the retained execution projection before a Session
/// hard purge. Returns `false` only when the Session itself does not exist, so
/// repeated purge remains idempotent without weakening the one-projection
/// invariant for live rows.
pub(super) fn prepare_session_execution_projection_for_purge(
    tx: &Transaction<'_>,
    session_id: Uuid,
) -> Result<bool> {
    let session_id = session_id.to_string();
    let session_exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1)",
        [&session_id],
        |row| row.get(0),
    )?;
    if !session_exists {
        return Ok(false);
    }

    let projection = tx
        .query_row(
            "SELECT execution_state,freshness,effective_cwd,validated_at,error_code
             FROM session_execution_projections WHERE session_id=?1",
            [&session_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            },
        )
        .optional()?;
    let Some((state, freshness, effective_cwd, validated_at, error_code)) = projection else {
        return Err(DaemonError::Store(format!(
            "Session {session_id} has no retained execution projection"
        )));
    };

    match state.as_str() {
        "ordinary_unsandboxed" => {
            let now = timestamp();
            let changed = tx.execute(
                "UPDATE session_execution_projections
                 SET execution_state='historical_purged',freshness='verified',
                     effective_cwd=NULL,validated_at=?2,error_code=NULL,updated_at=?2
                 WHERE session_id=?1 AND execution_state='ordinary_unsandboxed'",
                params![&session_id, now],
            )?;
            if changed != 1 {
                return Err(DaemonError::Store(format!(
                    "Session {session_id} execution projection changed during purge preparation"
                )));
            }
        }
        "historical_purged" | "historical_transferred"
            if freshness == "verified"
                && effective_cwd.is_none()
                && validated_at.is_some()
                && error_code.is_none() => {}
        "live_sandboxed" | "historical_cleanup_failed" | "quarantined" | "invalid" => {
            return Err(DaemonError::Store(format!(
                "Session {session_id} execution projection state {state} blocks hard purge"
            )));
        }
        _ => {
            return Err(DaemonError::Store(format!(
                "Session {session_id} execution projection is not purge-ready: state={state}, freshness={freshness}"
            )));
        }
    }
    Ok(true)
}

pub(super) fn bind_on(
    tx: &Transaction<'_>,
    session_id: Uuid,
    binding: SessionCustodyBinding,
) -> Result<()> {
    match binding {
        SessionCustodyBinding::Ordinary => {
            ensure_ordinary_session_tuple(tx, session_id)?;
            tx.execute("UPDATE session_execution_projections SET execution_state='ordinary_unsandboxed', freshness='verified', effective_cwd=canonical_repo_dir, validated_at=?2, error_code=NULL, updated_at=?2 WHERE session_id=?1", params![session_id.to_string(), timestamp()])?;
        }
        SessionCustodyBinding::New(root) => {
            ensure_session_matches_root(
                tx,
                session_id,
                &root.canonical_repo_dir,
                &root.sandbox_root,
                &root.sandbox_branch,
            )?;
            insert_new_root(tx, session_id, root)?;
        }
        SessionCustodyBinding::Reuse {
            custody_id,
            generation,
            cause: _,
        } => {
            let root = load_root(tx, custody_id)?;
            if root.owner_session_id != Some(session_id) || root.generation != generation {
                return Err(DaemonError::Store("custody reuse fence lost".into()));
            }
            ensure_session_matches_persisted_root(tx, session_id, custody_id)?;
            publish_verified(tx, session_id, custody_id, generation, None)?;
        }
        SessionCustodyBinding::Transfer {
            custody_id,
            from_session_id,
            generation,
            cause,
            origin_session_id,
            scheduled_job_id,
        } => transfer_root(
            tx,
            session_id,
            custody_id,
            from_session_id,
            generation,
            cause,
            origin_session_id,
            scheduled_job_id,
        )?,
    }
    Ok(())
}

pub(super) fn binding_custody_id(binding: &SessionCustodyBinding) -> Option<Uuid> {
    match binding {
        SessionCustodyBinding::Ordinary => None,
        SessionCustodyBinding::New(root) => Some(root.custody_id),
        SessionCustodyBinding::Reuse { custody_id, .. }
        | SessionCustodyBinding::Transfer { custody_id, .. } => Some(*custody_id),
    }
}

fn insert_new_root(tx: &Transaction<'_>, session_id: Uuid, root: NewCustodyRoot) -> Result<()> {
    let now = timestamp();
    let allocation_id = std::path::Path::new(&root.sandbox_root)
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| Uuid::parse_str(name).ok())
        .unwrap_or(session_id);
    tx.execute("INSERT INTO sandbox_custody_roots (custody_id,allocation_id,canonical_repo_dir,sandbox_root,sandbox_branch,repository_identity,source_commit,state,owner_session_id,generation,event_sequence,validation_state,validated_generation,validated_at,validation_error_code,effect_boot_id,reserved_effects,active_effects,created_at,updated_at,tombstoned_at) VALUES (?1,?2,?3,?4,?5,?6,?7,'live',?8,1,1,'verified',1,?9,NULL,NULL,0,0,?9,?9,NULL)", params![root.custody_id.to_string(),allocation_id.to_string(),root.canonical_repo_dir,root.sandbox_root,root.sandbox_branch,root.repository_identity,root.source_commit,session_id.to_string(),now])?;
    finish_new_root(tx, session_id, &root, &now)
}

/// Frozen pre-V98 twin of the [`insert_new_root`] row write, for
/// migration-chain fixtures only. See
/// [`Store::insert_session_with_pre_v99_custody`] for why this exists instead
/// of a schema-tolerant production insert.
#[cfg(test)]
fn insert_pre_v98_new_root(
    tx: &Transaction<'_>,
    session_id: Uuid,
    root: NewCustodyRoot,
) -> Result<()> {
    let now = timestamp();
    tx.execute("INSERT INTO sandbox_custody_roots (custody_id,canonical_repo_dir,sandbox_root,sandbox_branch,repository_identity,source_commit,state,owner_session_id,generation,event_sequence,validation_state,validated_generation,validated_at,validation_error_code,effect_boot_id,reserved_effects,active_effects,created_at,updated_at,tombstoned_at) VALUES (?1,?2,?3,?4,?5,?6,'live',?7,1,1,'verified',1,?8,NULL,NULL,0,0,?8,?8,NULL)", params![root.custody_id.to_string(),root.canonical_repo_dir,root.sandbox_root,root.sandbox_branch,root.repository_identity,root.source_commit,session_id.to_string(),now])?;
    finish_new_root(tx, session_id, &root, &now)
}

/// The generation-one tail shared by every fresh-root write: the `allocated`
/// event, the SQL-only session link, and the verified projection. None of it
/// touches a column the V98 allocation-identity migration introduced.
fn finish_new_root(
    tx: &Transaction<'_>,
    session_id: Uuid,
    root: &NewCustodyRoot,
    now: &str,
) -> Result<()> {
    insert_event(
        tx,
        EventInput {
            custody_id: root.custody_id,
            sequence: 1,
            event_kind: "allocated",
            cause: root.cause,
            from_generation: None,
            to_generation: 1,
            from_owner: None,
            to_owner: Some(session_id),
            origin: None,
            scheduled_job: None,
            prior_state: None,
            next_state: "live",
            error_code: None,
            occurred_at: now,
        },
    )?;
    tx.execute(
        "UPDATE sessions SET sandbox_custody_id=?2 WHERE id=?1",
        params![session_id.to_string(), root.custody_id.to_string()],
    )?;
    publish_verified(tx, session_id, root.custody_id, 1, None)
}

fn transfer_root(
    tx: &Transaction<'_>,
    session_id: Uuid,
    custody_id: Uuid,
    from_session_id: Uuid,
    generation: u64,
    cause: CustodyCause,
    origin: Option<Uuid>,
    scheduled_job: Option<Uuid>,
) -> Result<()> {
    refuse_prepared_reclaim_on(tx, custody_id, generation, cause.transition())?;
    let root = load_root(tx, custody_id)?;
    if root.owner_session_id != Some(from_session_id)
        || root.generation != generation
        || !matches!(root.state, RootState::Live)
        || root.reserved_effects != 0
        || root.active_effects != 0
    {
        return Err(DaemonError::Store("custody transfer fence lost".into()));
    }
    ensure_session_matches_persisted_root(tx, session_id, custody_id)?;
    let now = timestamp();
    let next_generation = generation + 1;
    let next_sequence = root.event_sequence + 1;
    insert_event(
        tx,
        EventInput {
            custody_id,
            sequence: next_sequence,
            event_kind: "transferred",
            cause,
            from_generation: Some(generation),
            to_generation: next_generation,
            from_owner: Some(from_session_id),
            to_owner: Some(session_id),
            origin,
            scheduled_job,
            prior_state: Some("live"),
            next_state: "live",
            error_code: None,
            occurred_at: &now,
        },
    )?;
    if tx.execute("UPDATE sandbox_custody_roots SET owner_session_id=?2,generation=?3,event_sequence=?4,validation_state='verified',validated_generation=?3,validated_at=?5,validation_error_code=NULL,updated_at=?5 WHERE custody_id=?1 AND owner_session_id=?6 AND generation=?7 AND reserved_effects=0 AND active_effects=0", params![custody_id.to_string(),session_id.to_string(),next_generation as i64,next_sequence as i64,now,from_session_id.to_string(),generation as i64])? != 1 { return Err(DaemonError::Store("custody transfer compare-and-swap lost".into())); }
    tx.execute(
        "UPDATE sessions SET sandbox_custody_id=?2 WHERE id=?1",
        params![session_id.to_string(), custody_id.to_string()],
    )?;
    tx.execute("UPDATE session_execution_projections SET execution_state='historical_transferred',freshness='verified',effective_cwd=NULL,custody_id=?2,custody_generation=?3,validated_at=?4,error_code=NULL,updated_at=?4 WHERE session_id=?1", params![from_session_id.to_string(),custody_id.to_string(),generation as i64,timestamp()])?;
    publish_verified(tx, session_id, custody_id, next_generation, None)
}

fn ensure_ordinary_session_tuple(tx: &Transaction<'_>, session_id: Uuid) -> Result<()> {
    let tuple_is_all_null: bool = tx.query_row(
        "SELECT sandbox_kind IS NULL AND sandbox_root IS NULL AND sandbox_branch IS NULL AND sandbox_cleanup_state IS NULL FROM sessions WHERE id=?1",
        [session_id.to_string()],
        |row| row.get(0),
    )?;
    if tuple_is_all_null {
        Ok(())
    } else {
        Err(DaemonError::Store(
            "ordinary custody requires an all-null sandbox tuple".into(),
        ))
    }
}

fn ensure_session_matches_persisted_root(
    tx: &Transaction<'_>,
    session_id: Uuid,
    custody_id: Uuid,
) -> Result<()> {
    let (canonical_repo_dir, sandbox_root, sandbox_branch): (String, String, String) = tx
        .query_row(
            "SELECT canonical_repo_dir,sandbox_root,sandbox_branch FROM sandbox_custody_roots WHERE custody_id=?1",
            [custody_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?
        .ok_or_else(|| DaemonError::Store("custody root missing during session binding".into()))?;
    ensure_session_matches_root(
        tx,
        session_id,
        &canonical_repo_dir,
        &sandbox_root,
        &sandbox_branch,
    )
}

fn ensure_session_matches_root(
    tx: &Transaction<'_>,
    session_id: Uuid,
    canonical_repo_dir: &str,
    sandbox_root: &str,
    sandbox_branch: &str,
) -> Result<()> {
    let matches: bool = tx.query_row(
        "SELECT working_dir=?2 AND sandbox_kind='GitWorktree' AND sandbox_root=?3 AND sandbox_branch=?4 AND sandbox_cleanup_state='Live' FROM sessions WHERE id=?1",
        params![session_id.to_string(), canonical_repo_dir, sandbox_root, sandbox_branch],
        |row| row.get(0),
    )?;
    if matches {
        Ok(())
    } else {
        Err(DaemonError::Store(
            "sandbox custody binding session tuple does not match root".into(),
        ))
    }
}

fn ensure_legacy_session_matches_root(
    tx: &Transaction<'_>,
    session_id: Uuid,
    canonical_repo_dir: &str,
    sandbox_root: &str,
    sandbox_branch: &str,
) -> Result<()> {
    let matches: bool = tx.query_row(
        "SELECT working_dir=?2 AND sandbox_kind='GitWorktree' AND sandbox_root=?3 AND sandbox_branch=?4 AND (sandbox_cleanup_state IS NULL OR sandbox_cleanup_state='Live') FROM sessions WHERE id=?1",
        params![session_id.to_string(), canonical_repo_dir, sandbox_root, sandbox_branch],
        |row| row.get(0),
    )?;
    if matches {
        Ok(())
    } else {
        Err(DaemonError::Store(
            "legacy sandbox custody session tuple does not match root".into(),
        ))
    }
}

fn ensure_terminal_session_matches_root(
    tx: &Transaction<'_>,
    session_id: Uuid,
    canonical_repo_dir: &str,
    sandbox_root: &str,
    sandbox_branch: &str,
    cleanup_state: SandboxCleanupState,
) -> Result<()> {
    let state = match cleanup_state {
        SandboxCleanupState::Purged => "Purged",
        SandboxCleanupState::Failed => "Failed",
        SandboxCleanupState::Live => unreachable!("terminal helper cannot accept live"),
        _ => return Err(DaemonError::Store("unknown terminal cleanup state".into())),
    };
    let matches: bool = tx.query_row(
        "SELECT working_dir=?2 AND sandbox_kind='GitWorktree' AND sandbox_root=?3 AND sandbox_branch=?4 AND sandbox_cleanup_state=?5 FROM sessions WHERE id=?1",
        params![session_id.to_string(), canonical_repo_dir, sandbox_root, sandbox_branch, state],
        |row| row.get(0),
    )?;
    if matches {
        Ok(())
    } else {
        Err(DaemonError::Store(
            "legacy terminal custody session tuple does not match root".into(),
        ))
    }
}

fn publish_verified(
    tx: &Transaction<'_>,
    session_id: Uuid,
    custody_id: Uuid,
    generation: u64,
    error_code: Option<&str>,
) -> Result<()> {
    let now = timestamp();
    tx.execute("UPDATE session_execution_projections SET execution_state='live_sandboxed',freshness='verified',effective_cwd=(SELECT sandbox_root FROM sandbox_custody_roots WHERE custody_id=?2),custody_id=?2,custody_generation=?3,validated_at=?4,error_code=?5,updated_at=?4 WHERE session_id=?1", params![session_id.to_string(),custody_id.to_string(),generation as i64,now,error_code])?;
    Ok(())
}

struct EventInput<'a> {
    custody_id: Uuid,
    sequence: u64,
    event_kind: &'a str,
    cause: CustodyCause,
    from_generation: Option<u64>,
    to_generation: u64,
    from_owner: Option<Uuid>,
    to_owner: Option<Uuid>,
    origin: Option<Uuid>,
    scheduled_job: Option<Uuid>,
    prior_state: Option<&'a str>,
    next_state: &'a str,
    error_code: Option<&'a str>,
    occurred_at: &'a str,
}
fn insert_event(tx: &Transaction<'_>, event: EventInput<'_>) -> Result<()> {
    tx.execute("INSERT INTO sandbox_custody_events (event_id,custody_id,sequence,event_kind,cause,from_generation,to_generation,from_owner_session_id,to_owner_session_id,origin_session_id,scheduled_job_id,prior_state,next_state,error_code,occurred_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)", params![Uuid::new_v4().to_string(),event.custody_id.to_string(),event.sequence as i64,event.event_kind,event.cause.as_str(),event.from_generation.map(|v| v as i64),event.to_generation as i64,event.from_owner.map(|v| v.to_string()),event.to_owner.map(|v| v.to_string()),event.origin.map(|v| v.to_string()),event.scheduled_job.map(|v| v.to_string()),event.prior_state,event.next_state,event.error_code,event.occurred_at])?;
    Ok(())
}
