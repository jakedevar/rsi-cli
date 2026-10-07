//! SQLite persistence layer for the flywheel daemon.
//!
//! Submodules split by domain: sessions, events, projects, metrics, approvals.
//! Row mappers and enum converters live in `row_mappers`.

pub mod agent_authority;
pub mod agent_child_relaunch_intents;
pub mod agent_coordination;
pub mod agent_message_dispatcher;
pub mod agent_message_reconciler;
mod approvals;
pub mod archive_cleanup;
pub mod capacity_recovery;
mod cards;
pub mod chain_iterations;
pub mod closure_kernel;
pub mod cohort_settlement;
mod compiled_prompts;
// The file stays at its released path: tools/released-migrations.json pins the
// v128 sections of `daemon_restart_persistence.rs` by path.
#[path = "../daemon_restart_persistence.rs"]
pub mod daemon_restart_persistence;
pub mod daemon_settings; // V48 — RSI-026 key-value table for daemon-owned settings.
pub(crate) mod efficiency_metrics;
pub mod efficiency_publications;
mod esp_games;
mod events;
pub mod graph_cache;
pub mod harness_manager;
pub mod harness_manager_v2;
pub mod ideas;
pub mod manager_coordinator;
mod manager_decision_history;
mod manager_decision_rulings;
pub mod manager_decisions;
pub mod pending_approvals;
pub mod session_retention;
pub mod source_worktree_v120;
pub use ideas::IdeaControllerReconciliationCursor;
#[cfg(any(test, feature = "test-seam"))]
pub use ideas::{IdeaControllerWriteFault, inject_d03_idea_controller_write_fault};
pub mod agent_deploys;
pub mod agent_deploys_operator;
pub mod agent_jobs;
mod catalog_convergence;
pub mod child_autonomy;
pub mod custody_lock_order;
pub mod daemon_info;
mod failure_signatures;
mod fleet;
pub mod friction;
pub use fleet::FleetScope;
pub mod global_manager;
mod issues;
mod labels;
mod lineage_convergence;
pub mod manager_actions;
pub mod manager_intent;
pub mod manager_ledger;
pub mod manager_node_workspace;
pub mod manager_nodes;
mod manager_notices;
mod manager_prepared_actions;
pub mod manager_resources;
pub mod manager_review_v121;
pub mod manager_reviews;
pub mod manager_successions;
pub mod manager_tier_routing;
pub mod manager_tree;
pub mod manager_watch_settlement;
mod metrics;
pub(crate) mod migration_allocation;
pub mod migration_backup;
mod model_control;
mod observations;
mod offload;
pub(crate) mod operator_messages;
pub mod origin_authority;
pub mod pending_questions;
mod permissions;
pub mod portfolio_nodes;
pub mod program_runs;
mod projects;
pub mod provider_exhaustion;
pub mod provider_status;
pub mod queue;
mod rate_limits;
pub mod recursive_dag;
pub mod restart_intents;
pub mod rolling_queue;
pub mod rotation_abandon;
mod rotation_events;
#[cfg(any(test, feature = "test-seam"))]
pub mod stripe_liveness_support;
pub use rotation_events::{BlockedRotation, COMPLETED_TRIGGER_PHASE, RotationRequestSuccessor};
pub mod portable_bundle; // #1406 clean export/import of durable state.
pub mod row_mappers;
pub mod runaway_process; // #1337 CPU-time andon notice to the owning manager.
pub mod sandbox_custody;
#[allow(clippy::redundant_pub_crate)]
pub mod sandbox_reclaim;
pub mod satellite_dispatch;
#[allow(clippy::redundant_pub_crate)]
pub mod satellite_identity;
pub mod satellite_inbound_attempts;
pub mod satellite_registry;
pub mod satellite_reports;
pub mod scheduled_jobs;
#[cfg(test)]
mod scheduled_jobs_list_tests; // #954 B paging tests
pub(crate) mod session_completion_gates;
mod session_diagnostics;
mod session_model_updates;
pub mod session_tool_policy;
pub mod session_transient_heal;
mod sessions;
pub mod successor_reservations;
mod summaries;
pub mod target_reclaim_sweep;
#[cfg(test)]
pub mod tests;
mod topologies; // P1.4 — DB-stored named topology templates.
pub mod topology_agent_audit; // #633 agent topology request ledger.
pub mod topology_v129; // #634 durable topology executor tables.
pub mod transient_heal;
mod usage; // T8 — read-only lifetime usage aggregate for Settings -> Stats.
pub mod worker_baton; // #1254 worker context cap baton.
pub mod worker_no_result;
mod workflows;

#[cfg(any(test, feature = "test-seam"))]
pub use issues::master_no_idle_test_fail_after_wake;
pub use issues::{C5SettlementOutcome, MasterNoIdleStoreRecovery};
pub use model_control::{CapacityStoreAdmissionOutcome, OrchestrationEscalationDenial};
pub use model_control::{
    ModelControlPolicyTransition, StoreAdmissionOutcome, StoreCancellationOutcome,
    StoreCompletionOutcome,
};
pub use row_mappers::parse_timestamp;

use crate::error::DaemonError;
use crate::error::Result;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
#[cfg(any(test, feature = "test-seam"))]
use std::fs::{File, OpenOptions, TryLockError as FileTryLockError};
use std::path::Path;
#[cfg(any(test, feature = "test-seam"))]
use std::path::PathBuf;
#[cfg(any(test, feature = "test-seam"))]
use std::sync::{Mutex, MutexGuard, OnceLock, TryLockError as MutexTryLockError};
#[cfg(any(test, feature = "test-seam"))]
use std::time::{Duration, Instant};
use uuid::Uuid;

include!("migrations/support.rs");
include!(concat!(env!("OUT_DIR"), "/store_migrations.rs"));

/// Stable project seeded only into isolated in-memory test stores. Production
/// stores never synthesize project ownership; D04 writers always receive it
/// from their bound caller or operator request.
#[cfg(any(test, feature = "test-seam"))]
pub fn d04_test_project_id() -> Uuid {
    Uuid::parse_str("00000000-0000-4000-8000-000000000077").expect("fixed test UUID")
}

/// Compile-time gate proving rusqlite's `functions` feature is enabled for
/// this crate (C-P2-17).
///
/// V81 CHECK constraints call `rsi_jsonrpc_id_is_canonical`, which only exists
/// when [`Store::register_sql_functions`] can register it. Without the feature
/// this reference fails to resolve and the BUILD breaks, which is the intended
/// outcome: silently degrading to a Rust-only check would let raw SQL admit a
/// malformed ID.
const _: fn() = || {
    let _ = rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC;
};

/// Exact canonical persistence form for a JSON-RPC request/response ID
/// (C-P2-12, C-P2-15, C-P2-17).
///
/// A JSON-RPC ID is either an `i64` number or a string of at most 256 raw
/// UTF-8 bytes. It persists reversibly as `n:<decimal>` or
/// `s:<canonical JSON string>` so a numeric and a string ID can never alias.
///
/// Fails closed on: a missing/unknown prefix; a non-canonical decimal (leading
/// `+`, leading zeros, `-0`, or anything outside `i64`); the `n:0` alias,
/// which the daemon must never synthesize as a stand-in for an absent ID; a
/// malformed or non-string JSON payload after `s:`; a `s:` payload whose JSON
/// encoding is not the canonical `serde_json` form; and an oversized string.
#[must_use]
pub fn is_canonical_jsonrpc_id(value: &str) -> bool {
    if let Some(decimal) = value.strip_prefix("n:") {
        return match decimal.parse::<i64>() {
            // `to_string()` equality rejects `+7`, `007`, and `-0` in one
            // check, and `!= 0` rejects the forbidden `n:0` alias.
            Ok(parsed) => parsed != 0 && parsed.to_string() == decimal,
            Err(_) => false,
        };
    }
    if let Some(encoded) = value.strip_prefix("s:") {
        let Ok(serde_json::Value::String(decoded)) =
            serde_json::from_str::<serde_json::Value>(encoded)
        else {
            return false;
        };
        if decoded.len() > rsi_common::agent_coordination::APP_SERVER_MAX_JSONRPC_STRING_ID_BYTES {
            return false;
        }
        // Reject a non-canonical escape spelling so one logical ID has exactly
        // one persisted representation.
        return serde_json::to_string(&decoded).is_ok_and(|canonical| canonical == encoded);
    }
    false
}

#[must_use]
fn is_canonical_uuid(value: &str) -> bool {
    Uuid::parse_str(value).is_ok_and(|parsed| parsed.to_string() == value)
}

#[must_use]
fn is_canonical_sha256_digest(value: &str) -> bool {
    value.len() == 71
        && value.strip_prefix("sha256:").is_some_and(|hex| {
            !hex.is_empty()
                && hex
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        })
}

#[must_use]
fn is_canonical_rfc3339_nanos(value: &str) -> bool {
    chrono::DateTime::parse_from_rfc3339(value).is_ok_and(|parsed| {
        parsed
            .with_timezone(&chrono::Utc)
            .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
            == value
    })
}

#[must_use]
fn is_valid_rfc3339(value: &str) -> bool {
    (20..=40).contains(&value.len()) && chrono::DateTime::parse_from_rfc3339(value).is_ok()
}

pub struct Store {
    pub conn: Connection,
    /// Process-local semantic controller grants. Durable ownership remains in
    /// `SQLite`; this registry is deliberately rebuilt only from a live provider,
    /// a durable active Session row, and the current A6 token binding.
    pub(crate) controller_grants:
        RefCell<HashMap<Uuid, crate::idea_control::BoundControllerWriteAuthority>>,
    /// Process-local incarnation of each semantic grant. Reinstalling even an
    /// otherwise identical D03 grant changes this witness, fencing capabilities
    /// bound before a revoke/reconstruct ABA cycle.
    pub(crate) controller_grant_incarnations: RefCell<HashMap<Uuid, Uuid>>,
    /// Daemon-process identity used for ProgramRun claim and lease ownership.
    /// Session IDs never substitute for this witness.
    program_run_boot_id: Cell<Uuid>,
    /// Daemon-incarnation identity of the **agent-message delivery path**,
    /// stamped into every delivery attempt's
    /// [`MessageAttemptFenceV1::delivery_boot_id`] (Issue 21 P2-05).
    ///
    /// **Why this is a separate witness from [`Store::program_run_boot_id`]**,
    /// despite being the same shape and seeded the same way: the two fence
    /// different things and are read by different kernels. Sharing one cell
    /// would make an agent-message attempt's crash-recovery identity move
    /// whenever the ProgramRun kernel re-seeded its own, silently invalidating
    /// live delivery attempts that have nothing to do with a program run. They
    /// are deliberately one *idiom* and two *values*.
    ///
    /// It exists so a delivery attempt recorded by a previous daemon
    /// incarnation is distinguishable from one this incarnation owns: after a
    /// crash between the claim commit and the provider dispatch, the attempt
    /// row survives with the dead incarnation's boot id, which is what lets
    /// recovery tell "this attempt is mine and still in flight" from "this
    /// attempt belongs to a process that no longer exists". That mismatch is
    /// the *sound* half of the proof P2-06 needs, because a dead incarnation
    /// cannot have an in-flight send and the send is strictly after the commit.
    ///
    /// # Scope of the identity, stated exactly (H21-P2-R4-002)
    ///
    /// The two [`Store`] constructors each seed this cell with an independent
    /// [`Uuid::new_v4`], so a constructor seed alone is a **per-`Store`-instance**
    /// value, not a per-process one — and the daemon genuinely opens more than
    /// one `Store` over the same database file in a single incarnation
    /// (`main.rs:153` for the session path, `main.rs:1221` inside
    /// `init_memory_system` for the memory system). A doc claiming bare
    /// "process identity" over that shape would be false, and a recovery
    /// decision resting on it would be unsound.
    ///
    /// What makes the value trustworthy is the explicit production seeder in
    /// [`SessionManager::new`](crate::session::SessionManager::new), which
    /// re-seeds this cell once per daemon incarnation on the `Store` that owns
    /// the delivery path — exactly mirroring `set_program_run_boot_id` beside
    /// it. The memory system's `Store` keeps its own unrelated constructor
    /// seed, and that is harmless rather than a loophole: nothing under
    /// `crates/rsid/src/memory/` reads or writes `agent_messages` or any
    /// delivery attempt, so that handle is never a delivery witness. The
    /// invariant is therefore precisely: **every agent-message delivery attempt
    /// in one daemon incarnation is stamped with one identity, and a different
    /// incarnation cannot produce that same identity.**
    ///
    /// Pinned by
    /// `session::issue21_phase2_tests::the_delivery_witness_a_consumer_reads_is_the_seeded_daemon_identity`.
    delivery_boot_id: Cell<Uuid>,
    /// #1103: process-local queue of reports the satellite's appointed manager
    /// wrote for the hub. Deliberately not durable (informational, at most
    /// once): the hub pulls and acknowledges them, a restart loses unsent ones.
    pub(crate) hub_reports:
        RefCell<std::collections::VecDeque<rsi_common::satellite_dispatch::SatelliteReportV1>>,
}

#[cfg(any(test, feature = "test-seam"))]
const CURRENT_SCHEMA_TEMPLATE_PAGE_LIMIT: i64 = 16_384;
#[cfg(any(test, feature = "test-seam"))]
const CURRENT_SCHEMA_TEMPLATE_PAGES_PER_STEP: i32 = 64;
#[cfg(any(test, feature = "test-seam"))]
const CURRENT_SCHEMA_TEMPLATE_STEP_LIMIT: usize = 272;
#[cfg(any(test, feature = "test-seam"))]
const CURRENT_SCHEMA_TEMPLATE_NO_PROGRESS_LIMIT: usize = 8;
#[cfg(any(test, feature = "test-seam"))]
const CURRENT_SCHEMA_TEMPLATE_BUSY_LOCKED_LIMIT: usize = 8;
/// Hang net for building/copying the cached test fixture and for waiting on its
/// source mutex. The copy's real progress bounds are the step, no-progress and
/// busy/locked budgets above; this wall clock only stops a wedged copy or a
/// deadlocked mutex from hanging a shard forever. It is deliberately generous:
/// under parallel cargo-slot load a thread can be descheduled for far longer
/// than the copy itself takes, and a tight clock turns that scheduling jitter
/// into spurious `LockDeadline`/`BackupDeadline` fixture failures. It matches
/// `CURRENT_SCHEMA_TEMPLATE_CACHE_LOCK_DEADLINE` so every fixture wait shares
/// one ceiling.
#[cfg(any(test, feature = "test-seam"))]
const CURRENT_SCHEMA_TEMPLATE_DEADLINE: Duration = Duration::from_secs(30);
#[cfg(any(test, feature = "test-seam"))]
const CURRENT_SCHEMA_TEMPLATE_RETRY_PAUSE: Duration = Duration::from_millis(1);
#[cfg(any(test, feature = "test-seam"))]
const CURRENT_SCHEMA_TEMPLATE_CACHE_PROTOCOL: u32 = 1;
#[cfg(any(test, feature = "test-seam"))]
const CURRENT_SCHEMA_TEMPLATE_CACHE_LOCK_DEADLINE: Duration = Duration::from_secs(30);

#[cfg(any(test, feature = "test-seam"))]
type CurrentSchemaCatalogRow = (String, String, String, String);

#[cfg(any(test, feature = "test-seam"))]
struct CurrentSchemaTemplateSource {
    conn: Connection,
    catalog: Vec<CurrentSchemaCatalogRow>,
    page_count: i64,
}

#[cfg(any(test, feature = "test-seam"))]
struct SharedCurrentSchemaTemplateSource {
    path: PathBuf,
    catalog: Vec<CurrentSchemaCatalogRow>,
    page_count: i64,
}

#[cfg(any(test, feature = "test-seam"))]
enum CurrentSchemaTemplateBacking {
    Shared(SharedCurrentSchemaTemplateSource),
    Local(Mutex<CurrentSchemaTemplateSource>),
}

#[cfg(any(test, feature = "test-seam"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CurrentSchemaTemplateErrorCode {
    InitSpawn,
    InitJoin,
    InitStore,
    CachePath,
    CacheDirectory,
    CacheLock,
    CacheLockDeadline,
    CacheOpen,
    CacheRemove,
    CacheBuild,
    CachePublish,
    MutexPoisoned,
    LockDeadline,
    SourceInvariant,
    SourceTooLarge,
    BackupInit,
    BackupStep,
    BackupStepBudget,
    BackupNoProgress,
    BackupBusyLocked,
    BackupDeadline,
    BackupUnknownResult,
    DestinationInvariant,
    RuntimeIdentity,
    Seed,
}

#[cfg(any(test, feature = "test-seam"))]
impl CurrentSchemaTemplateErrorCode {
    fn as_str(self) -> &'static str {
        match self {
            Self::InitSpawn => "init_spawn",
            Self::InitJoin => "init_join",
            Self::InitStore => "init_store",
            Self::CachePath => "cache_path",
            Self::CacheDirectory => "cache_directory",
            Self::CacheLock => "cache_lock",
            Self::CacheLockDeadline => "cache_lock_deadline",
            Self::CacheOpen => "cache_open",
            Self::CacheRemove => "cache_remove",
            Self::CacheBuild => "cache_build",
            Self::CachePublish => "cache_publish",
            Self::MutexPoisoned => "mutex_poisoned",
            Self::LockDeadline => "lock_deadline",
            Self::SourceInvariant => "source_invariant",
            Self::SourceTooLarge => "source_too_large",
            Self::BackupInit => "backup_init",
            Self::BackupStep => "backup_step",
            Self::BackupStepBudget => "backup_step_budget",
            Self::BackupNoProgress => "backup_no_progress",
            Self::BackupBusyLocked => "backup_busy_locked",
            Self::BackupDeadline => "backup_deadline",
            Self::BackupUnknownResult => "backup_unknown_result",
            Self::DestinationInvariant => "destination_invariant",
            Self::RuntimeIdentity => "runtime_identity",
            Self::Seed => "seed",
        }
    }

    fn error(self, detail: impl std::fmt::Display) -> DaemonError {
        DaemonError::Store(format!(
            "test_current_schema_template:{}: {detail}",
            self.as_str()
        ))
    }
}

#[cfg(any(test, feature = "test-seam"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CachedCurrentSchemaTemplateError {
    code: CurrentSchemaTemplateErrorCode,
}

#[cfg(any(test, feature = "test-seam"))]
impl CachedCurrentSchemaTemplateError {
    const fn new(code: CurrentSchemaTemplateErrorCode) -> Self {
        Self { code }
    }

    fn to_daemon(self) -> DaemonError {
        self.code.error("cached initialization failed")
    }
}

#[cfg(any(test, feature = "test-seam"))]
static CURRENT_SCHEMA_TEMPLATE: OnceLock<
    std::result::Result<CurrentSchemaTemplateBacking, CachedCurrentSchemaTemplateError>,
> = OnceLock::new();

#[cfg(any(test, feature = "test-seam"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CurrentSchemaTemplateInitMode {
    Real,
    InjectSpawnFailure,
    InjectJoinPanic,
}

#[cfg(any(test, feature = "test-seam"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CurrentSchemaTemplateBackupObservation {
    More(i32),
    Done(i32),
    Busy,
    Locked,
    Error,
    Unknown,
}

#[cfg(any(test, feature = "test-seam"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CurrentSchemaTemplateBackupAction {
    Continue,
    Pause,
    Done,
}

#[cfg(any(test, feature = "test-seam"))]
struct CurrentSchemaTemplateBackupBudget {
    calls: usize,
    consecutive_no_progress: usize,
    busy_locked: usize,
    previous_remaining: i32,
}

#[cfg(any(test, feature = "test-seam"))]
impl CurrentSchemaTemplateBackupBudget {
    fn new(page_count: i64) -> Result<Self> {
        if !(1..=CURRENT_SCHEMA_TEMPLATE_PAGE_LIMIT).contains(&page_count) {
            return Err(CurrentSchemaTemplateErrorCode::SourceTooLarge
                .error(format_args!("page_count={page_count}")));
        }
        Ok(Self {
            calls: 0,
            consecutive_no_progress: 0,
            busy_locked: 0,
            previous_remaining: page_count as i32,
        })
    }

    fn observe(
        &mut self,
        observation: CurrentSchemaTemplateBackupObservation,
    ) -> Result<CurrentSchemaTemplateBackupAction> {
        self.calls += 1;
        if self.calls > CURRENT_SCHEMA_TEMPLATE_STEP_LIMIT {
            return Err(CurrentSchemaTemplateErrorCode::BackupStepBudget
                .error(format_args!("step_calls={}", self.calls)));
        }
        match observation {
            CurrentSchemaTemplateBackupObservation::More(remaining) => {
                if remaining < 0 || remaining > self.previous_remaining {
                    return Err(CurrentSchemaTemplateErrorCode::BackupUnknownResult
                        .error(format_args!("remaining={remaining}")));
                }
                if remaining < self.previous_remaining {
                    self.consecutive_no_progress = 0;
                } else {
                    self.consecutive_no_progress += 1;
                    if self.consecutive_no_progress > CURRENT_SCHEMA_TEMPLATE_NO_PROGRESS_LIMIT {
                        return Err(CurrentSchemaTemplateErrorCode::BackupNoProgress
                            .error(format_args!("consecutive={}", self.consecutive_no_progress)));
                    }
                }
                self.previous_remaining = remaining;
                Ok(CurrentSchemaTemplateBackupAction::Continue)
            }
            CurrentSchemaTemplateBackupObservation::Busy
            | CurrentSchemaTemplateBackupObservation::Locked => {
                self.busy_locked += 1;
                if self.busy_locked > CURRENT_SCHEMA_TEMPLATE_BUSY_LOCKED_LIMIT {
                    return Err(CurrentSchemaTemplateErrorCode::BackupBusyLocked
                        .error(format_args!("results={}", self.busy_locked)));
                }
                Ok(CurrentSchemaTemplateBackupAction::Pause)
            }
            CurrentSchemaTemplateBackupObservation::Done(0) => {
                Ok(CurrentSchemaTemplateBackupAction::Done)
            }
            CurrentSchemaTemplateBackupObservation::Done(remaining) => {
                Err(CurrentSchemaTemplateErrorCode::BackupUnknownResult
                    .error(format_args!("done_remaining={remaining}")))
            }
            CurrentSchemaTemplateBackupObservation::Error => {
                Err(CurrentSchemaTemplateErrorCode::BackupStep.error("injected step error"))
            }
            CurrentSchemaTemplateBackupObservation::Unknown => {
                Err(CurrentSchemaTemplateErrorCode::BackupUnknownResult
                    .error("unknown step result"))
            }
        }
    }
}

#[cfg(any(test, feature = "test-seam"))]
fn current_schema_template_catalog(
    conn: &Connection,
    code: CurrentSchemaTemplateErrorCode,
) -> Result<Vec<CurrentSchemaCatalogRow>> {
    let mut statement = conn
        .prepare(
            "SELECT type,name,tbl_name,COALESCE(sql,'')
             FROM sqlite_master
             ORDER BY type,name,tbl_name,COALESCE(sql,'')",
        )
        .map_err(|error| code.error(error))?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .map_err(|error| code.error(error))?;
    let mut catalog = Vec::new();
    for row in rows {
        catalog.push(row.map_err(|error| code.error(error))?);
    }
    if catalog.is_empty() {
        return Err(code.error("empty sqlite_master catalog"));
    }
    Ok(catalog)
}

#[cfg(any(test, feature = "test-seam"))]
fn current_schema_template_zero_seed_rows(
    conn: &Connection,
    code: CurrentSchemaTemplateErrorCode,
) -> Result<i64> {
    let count = conn
        .query_row(
            "SELECT count(*) FROM projects WHERE id=?1",
            [d04_test_project_id().to_string()],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|error| code.error(error))?;
    if count != 0 {
        return Err(code.error(format_args!("D04 source rows={count}")));
    }
    Ok(count)
}

#[cfg(any(test, feature = "test-seam"))]
fn verify_current_schema_connection(
    conn: &Connection,
    expected_catalog: Option<&[CurrentSchemaCatalogRow]>,
    code: CurrentSchemaTemplateErrorCode,
) -> Result<(Vec<CurrentSchemaCatalogRow>, i64)> {
    let user_version = conn
        .query_row("PRAGMA user_version", [], |row| row.get::<_, i32>(0))
        .map_err(|error| code.error(error))?;
    if user_version != LATEST_SCHEMA_VERSION {
        return Err(code.error(format_args!("user_version={user_version}")));
    }
    let catalog = current_schema_template_catalog(conn, code)?;
    if expected_catalog.is_some_and(|expected| expected != catalog.as_slice()) {
        return Err(code.error("normalized catalog mismatch"));
    }
    let integrity = conn
        .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
        .map_err(|error| code.error(error))?;
    if integrity != "ok" {
        return Err(code.error(format_args!("integrity_check={integrity}")));
    }
    let foreign_key_violations = conn
        .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(|error| code.error(error))?;
    if foreign_key_violations != 0 {
        return Err(code.error(format_args!(
            "foreign_key_violations={foreign_key_violations}"
        )));
    }
    let foreign_keys = conn
        .query_row("PRAGMA foreign_keys", [], |row| row.get::<_, i64>(0))
        .map_err(|error| code.error(error))?;
    if foreign_keys != 1 {
        return Err(code.error(format_args!("foreign_keys={foreign_keys}")));
    }
    if !conn.is_autocommit() {
        return Err(code.error("connection is not autocommit"));
    }
    let page_count = conn
        .query_row("PRAGMA page_count", [], |row| row.get::<_, i64>(0))
        .map_err(|error| code.error(error))?;
    if !(1..=CURRENT_SCHEMA_TEMPLATE_PAGE_LIMIT).contains(&page_count) {
        return Err(code.error(format_args!("page_count={page_count}")));
    }
    current_schema_template_zero_seed_rows(conn, code)?;
    Ok((catalog, page_count))
}

#[cfg(any(test, feature = "test-seam"))]
fn verify_current_schema_template_source_for_copy(
    source: &CurrentSchemaTemplateSource,
) -> Result<()> {
    let code = CurrentSchemaTemplateErrorCode::SourceInvariant;
    let user_version = source
        .conn
        .query_row("PRAGMA user_version", [], |row| row.get::<_, i32>(0))
        .map_err(|error| code.error(error))?;
    if user_version != LATEST_SCHEMA_VERSION {
        return Err(code.error(format_args!("user_version={user_version}")));
    }
    let page_count = source
        .conn
        .query_row("PRAGMA page_count", [], |row| row.get::<_, i64>(0))
        .map_err(|error| code.error(error))?;
    if page_count != source.page_count {
        return Err(code.error(format_args!(
            "page_count={page_count}, recorded={}",
            source.page_count
        )));
    }
    if !(1..=CURRENT_SCHEMA_TEMPLATE_PAGE_LIMIT).contains(&page_count) {
        return Err(CurrentSchemaTemplateErrorCode::SourceTooLarge
            .error(format_args!("page_count={page_count}")));
    }
    if !source.conn.is_autocommit() {
        return Err(code.error("source is not autocommit"));
    }
    current_schema_template_zero_seed_rows(&source.conn, code)?;
    Ok(())
}

#[cfg(any(test, feature = "test-seam"))]
fn current_schema_template_runtime_identity() -> Result<Uuid> {
    let identity = Uuid::new_v4();
    if identity.is_nil() {
        return Err(CurrentSchemaTemplateErrorCode::RuntimeIdentity.error("nil UUID"));
    }
    Ok(identity)
}

#[cfg(any(test, feature = "test-seam"))]
fn raw_in_memory_store_for_test() -> Result<Store> {
    let conn = Connection::open_in_memory()?;
    Store::register_sql_functions(&conn)?;
    conn.execute_batch("PRAGMA foreign_keys=ON;")?;
    Ok(Store {
        conn,
        controller_grants: RefCell::new(HashMap::new()),
        controller_grant_incarnations: RefCell::new(HashMap::new()),
        program_run_boot_id: Cell::new(current_schema_template_runtime_identity()?),
        delivery_boot_id: Cell::new(current_schema_template_runtime_identity()?),
        hub_reports: RefCell::new(std::collections::VecDeque::new()),
    })
}

#[cfg(any(test, feature = "test-seam"))]
fn seed_d04_test_project(store: &Store, timestamp: &str) -> Result<()> {
    let code = CurrentSchemaTemplateErrorCode::Seed;
    if !is_canonical_rfc3339_nanos(timestamp) {
        return Err(code.error("timestamp is not canonical RFC3339 nanoseconds"));
    }
    let project_id = d04_test_project_id().to_string();
    let inserted = store
        .conn
        .execute(
            "INSERT INTO projects (id, name, path, description, color, context_files, created_at, updated_at)
             VALUES (?1, 'D04 test project', NULL, NULL, '#89b4fa', NULL, ?2, ?2)",
            params![&project_id, timestamp],
        )
        .map_err(|error| code.error(error))?;
    if inserted != 1 {
        return Err(code.error(format_args!("inserted={inserted}")));
    }
    let shape = store
        .conn
        .query_row(
            "SELECT id,name,path,description,color,context_files,created_at,updated_at
             FROM projects WHERE id=?1",
            [&project_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                ))
            },
        )
        .map_err(|error| code.error(error))?;
    if shape
        != (
            project_id,
            "D04 test project".to_owned(),
            None,
            None,
            Some("#89b4fa".to_owned()),
            None,
            timestamp.to_owned(),
            timestamp.to_owned(),
        )
    {
        return Err(code.error("seed row shape mismatch"));
    }
    Ok(())
}

#[cfg(any(test, feature = "test-seam"))]
fn build_current_schema_template_source_in_memory() -> Result<CurrentSchemaTemplateSource> {
    let store = raw_in_memory_store_for_test()?;
    store.init_schema()?;
    let (catalog, page_count) = verify_current_schema_connection(
        &store.conn,
        None,
        CurrentSchemaTemplateErrorCode::SourceInvariant,
    )?;
    let Store { conn, .. } = store;
    Ok(CurrentSchemaTemplateSource {
        conn,
        catalog,
        page_count,
    })
}

#[cfg(any(test, feature = "test-seam"))]
fn current_schema_template_cache_directory() -> Result<PathBuf> {
    if let Some(directory) = std::env::var_os("RSID_TEST_SCHEMA_CACHE_DIR") {
        return Ok(PathBuf::from(directory));
    }
    let executable = std::env::current_exe()
        .map_err(|error| CurrentSchemaTemplateErrorCode::CachePath.error(error))?;
    let parent = executable.parent().ok_or_else(|| {
        CurrentSchemaTemplateErrorCode::CachePath.error(format_args!(
            "test executable has no parent: {}",
            executable.display()
        ))
    })?;
    Ok(parent.join(".rsid-current-schema-cache"))
}

#[cfg(any(test, feature = "test-seam"))]
fn current_schema_template_cache_key() -> String {
    format!(
        "v{}-schema{}-{}.sqlite3",
        CURRENT_SCHEMA_TEMPLATE_CACHE_PROTOCOL,
        LATEST_SCHEMA_VERSION,
        env!("RSID_TEST_SCHEMA_SOURCE_DIGEST")
    )
}

#[cfg(any(test, feature = "test-seam"))]
fn prepare_current_schema_template_cache_directory(directory: &Path) -> Result<()> {
    match std::fs::symlink_metadata(directory) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(
                CurrentSchemaTemplateErrorCode::CacheDirectory.error(format_args!(
                    "unsafe cache directory: {}",
                    directory.display()
                )),
            );
        }
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(CurrentSchemaTemplateErrorCode::CacheDirectory.error(error));
        }
    }
    std::fs::create_dir_all(directory)
        .map_err(|error| CurrentSchemaTemplateErrorCode::CacheDirectory.error(error))?;
    let metadata = std::fs::symlink_metadata(directory)
        .map_err(|error| CurrentSchemaTemplateErrorCode::CacheDirectory.error(error))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(
            CurrentSchemaTemplateErrorCode::CacheDirectory.error(format_args!(
                "unsafe cache directory: {}",
                directory.display()
            )),
        );
    }
    Ok(())
}

#[cfg(any(test, feature = "test-seam"))]
fn open_shared_current_schema_template(path: &Path) -> Result<SharedCurrentSchemaTemplateSource> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| CurrentSchemaTemplateErrorCode::CacheOpen.error(error))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(CurrentSchemaTemplateErrorCode::CacheOpen
            .error(format_args!("unsafe cache file: {}", path.display())));
    }
    let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
        | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
        | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW;
    let conn = Connection::open_with_flags(path, flags)
        .map_err(|error| CurrentSchemaTemplateErrorCode::CacheOpen.error(error))?;
    Store::register_sql_functions(&conn)
        .map_err(|error| CurrentSchemaTemplateErrorCode::CacheOpen.error(error))?;
    conn.execute_batch("PRAGMA foreign_keys=ON;")
        .map_err(|error| CurrentSchemaTemplateErrorCode::CacheOpen.error(error))?;
    let (catalog, page_count) =
        verify_current_schema_connection(&conn, None, CurrentSchemaTemplateErrorCode::CacheOpen)?;
    Ok(SharedCurrentSchemaTemplateSource {
        path: path.to_path_buf(),
        catalog,
        page_count,
    })
}

#[cfg(any(test, feature = "test-seam"))]
fn acquire_current_schema_template_cache_lock(path: &Path) -> Result<File> {
    if std::fs::symlink_metadata(path)
        .is_ok_and(|metadata| metadata.file_type().is_symlink() || !metadata.is_file())
    {
        return Err(CurrentSchemaTemplateErrorCode::CacheLock
            .error(format_args!("unsafe cache lock: {}", path.display())));
    }
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(path)
        .map_err(|error| CurrentSchemaTemplateErrorCode::CacheLock.error(error))?;
    let deadline = Instant::now() + CURRENT_SCHEMA_TEMPLATE_CACHE_LOCK_DEADLINE;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(FileTryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(CURRENT_SCHEMA_TEMPLATE_RETRY_PAUSE);
            }
            Err(FileTryLockError::WouldBlock) => {
                return Err(CurrentSchemaTemplateErrorCode::CacheLockDeadline
                    .error("cache publication lock exceeded 30 s"));
            }
            Err(FileTryLockError::Error(error)) => {
                return Err(CurrentSchemaTemplateErrorCode::CacheLock.error(error));
            }
        }
    }
}

#[cfg(any(test, feature = "test-seam"))]
struct CurrentSchemaTemplateTemporaryFile(PathBuf);

#[cfg(any(test, feature = "test-seam"))]
impl Drop for CurrentSchemaTemplateTemporaryFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(any(test, feature = "test-seam"))]
fn raw_file_store_for_test(path: &Path) -> Result<Store> {
    let conn = Connection::open(path)
        .map_err(|error| CurrentSchemaTemplateErrorCode::CacheBuild.error(error))?;
    Store::register_sql_functions(&conn)
        .map_err(|error| CurrentSchemaTemplateErrorCode::CacheBuild.error(error))?;
    conn.execute_batch("PRAGMA foreign_keys=ON;")
        .map_err(|error| CurrentSchemaTemplateErrorCode::CacheBuild.error(error))?;
    Ok(Store {
        conn,
        controller_grants: RefCell::new(HashMap::new()),
        controller_grant_incarnations: RefCell::new(HashMap::new()),
        program_run_boot_id: Cell::new(current_schema_template_runtime_identity()?),
        delivery_boot_id: Cell::new(current_schema_template_runtime_identity()?),
        hub_reports: RefCell::new(std::collections::VecDeque::new()),
    })
}

#[cfg(any(test, feature = "test-seam"))]
fn publish_current_schema_template(cache_path: &Path) -> Result<()> {
    let directory = cache_path.parent().ok_or_else(|| {
        CurrentSchemaTemplateErrorCode::CachePath.error(format_args!(
            "cache path has no parent: {}",
            cache_path.display()
        ))
    })?;
    let temporary_path = directory.join(format!(
        ".current-schema-{}-{}.tmp",
        std::process::id(),
        Uuid::new_v4()
    ));
    let reserved = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&temporary_path)
        .map_err(|error| CurrentSchemaTemplateErrorCode::CacheBuild.error(error))?;
    drop(reserved);
    let temporary = CurrentSchemaTemplateTemporaryFile(temporary_path.clone());

    let store = raw_file_store_for_test(&temporary_path)?;
    store
        .init_schema()
        .map_err(|error| CurrentSchemaTemplateErrorCode::CacheBuild.error(error))?;
    verify_current_schema_connection(
        &store.conn,
        None,
        CurrentSchemaTemplateErrorCode::CacheBuild,
    )?;
    drop(store);

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&temporary_path)
        .map_err(|error| CurrentSchemaTemplateErrorCode::CachePublish.error(error))?;
    let mut permissions = file
        .metadata()
        .map_err(|error| CurrentSchemaTemplateErrorCode::CachePublish.error(error))?
        .permissions();
    permissions.set_readonly(true);
    file.set_permissions(permissions)
        .map_err(|error| CurrentSchemaTemplateErrorCode::CachePublish.error(error))?;
    file.sync_all()
        .map_err(|error| CurrentSchemaTemplateErrorCode::CachePublish.error(error))?;
    drop(file);
    std::fs::rename(&temporary_path, cache_path)
        .map_err(|error| CurrentSchemaTemplateErrorCode::CachePublish.error(error))?;
    drop(temporary);
    File::open(directory)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| CurrentSchemaTemplateErrorCode::CachePublish.error(error))?;
    Ok(())
}

#[cfg(any(test, feature = "test-seam"))]
fn shared_current_schema_template_at(
    directory: &Path,
) -> Result<SharedCurrentSchemaTemplateSource> {
    prepare_current_schema_template_cache_directory(directory)?;
    let cache_path = directory.join(current_schema_template_cache_key());
    if let Ok(source) = open_shared_current_schema_template(&cache_path) {
        return Ok(source);
    }

    let lock_path = directory.join(format!("{}.lock", current_schema_template_cache_key()));
    let lock = acquire_current_schema_template_cache_lock(&lock_path)?;
    if let Ok(source) = open_shared_current_schema_template(&cache_path) {
        return Ok(source);
    }
    match std::fs::symlink_metadata(&cache_path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(
                CurrentSchemaTemplateErrorCode::CacheRemove.error(format_args!(
                    "refusing cache symlink: {}",
                    cache_path.display()
                )),
            );
        }
        Ok(_) => std::fs::remove_file(&cache_path)
            .map_err(|error| CurrentSchemaTemplateErrorCode::CacheRemove.error(error))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(CurrentSchemaTemplateErrorCode::CacheRemove.error(error)),
    }
    publish_current_schema_template(&cache_path)?;
    let source = open_shared_current_schema_template(&cache_path)?;
    drop(lock);
    Ok(source)
}

#[cfg(any(test, feature = "test-seam"))]
fn build_current_schema_template_backing() -> Result<CurrentSchemaTemplateBacking> {
    let cache_directory = current_schema_template_cache_directory()?;
    match shared_current_schema_template_at(&cache_directory) {
        Ok(source) => Ok(CurrentSchemaTemplateBacking::Shared(source)),
        Err(cache_error) => {
            tracing::warn!(error = %cache_error, "falling back to the process-local test schema template");
            build_current_schema_template_source_in_memory()
                .map(|source| CurrentSchemaTemplateBacking::Local(Mutex::new(source)))
        }
    }
}

#[cfg(any(test, feature = "test-seam"))]
fn initialize_current_schema_template(
    mode: CurrentSchemaTemplateInitMode,
) -> std::result::Result<CurrentSchemaTemplateBacking, CachedCurrentSchemaTemplateError> {
    if mode == CurrentSchemaTemplateInitMode::InjectSpawnFailure {
        return Err(CachedCurrentSchemaTemplateError::new(
            CurrentSchemaTemplateErrorCode::InitSpawn,
        ));
    }
    let thread = std::thread::Builder::new()
        .name("rsid-test-current-schema-template".to_owned())
        .spawn(move || {
            if mode == CurrentSchemaTemplateInitMode::InjectJoinPanic {
                panic!("injected current-schema template join failure");
            }
            build_current_schema_template_backing()
        })
        .map_err(|_| {
            CachedCurrentSchemaTemplateError::new(CurrentSchemaTemplateErrorCode::InitSpawn)
        })?;
    match thread.join() {
        Ok(Ok(source)) => Ok(source),
        Ok(Err(_)) => Err(CachedCurrentSchemaTemplateError::new(
            CurrentSchemaTemplateErrorCode::InitStore,
        )),
        Err(_) => Err(CachedCurrentSchemaTemplateError::new(
            CurrentSchemaTemplateErrorCode::InitJoin,
        )),
    }
}

#[cfg(any(test, feature = "test-seam"))]
fn current_schema_template_from_cache<'a, T>(
    cache: &'a OnceLock<std::result::Result<T, CachedCurrentSchemaTemplateError>>,
    initialize: impl FnOnce() -> std::result::Result<T, CachedCurrentSchemaTemplateError>,
) -> Result<&'a T> {
    match cache.get_or_init(initialize) {
        Ok(source) => Ok(source),
        Err(error) => Err(error.to_daemon()),
    }
}

#[cfg(any(test, feature = "test-seam"))]
fn current_schema_template() -> Result<&'static CurrentSchemaTemplateBacking> {
    current_schema_template_from_cache(&CURRENT_SCHEMA_TEMPLATE, || {
        initialize_current_schema_template(CurrentSchemaTemplateInitMode::Real)
    })
}

#[cfg(any(test, feature = "test-seam"))]
fn try_current_schema_template_lock<'a, T>(
    mutex: &'a Mutex<T>,
    deadline: Instant,
) -> Result<MutexGuard<'a, T>> {
    loop {
        match mutex.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(MutexTryLockError::Poisoned(_)) => {
                return Err(CurrentSchemaTemplateErrorCode::MutexPoisoned.error("poisoned mutex"));
            }
            Err(MutexTryLockError::WouldBlock) => {
                if Instant::now() >= deadline {
                    return Err(
                        CurrentSchemaTemplateErrorCode::LockDeadline.error(format_args!(
                            "source mutex acquisition exceeded {CURRENT_SCHEMA_TEMPLATE_DEADLINE:?}"
                        )),
                    );
                }
                std::thread::sleep(CURRENT_SCHEMA_TEMPLATE_RETRY_PAUSE);
            }
        }
    }
}

#[cfg(any(test, feature = "test-seam"))]
fn backup_current_schema_template_connection(
    source: &Connection,
    source_catalog: &[CurrentSchemaCatalogRow],
    source_page_count: i64,
    destination: &mut Connection,
    deadline: Instant,
) -> Result<(Vec<CurrentSchemaCatalogRow>, i64)> {
    use rusqlite::backup::{Backup, StepResult};

    let mut budget = CurrentSchemaTemplateBackupBudget::new(source_page_count)?;
    let backup = Backup::new(source, destination)
        .map_err(|error| CurrentSchemaTemplateErrorCode::BackupInit.error(error))?;
    loop {
        if Instant::now() >= deadline {
            return Err(
                CurrentSchemaTemplateErrorCode::BackupDeadline.error(format_args!(
                    "copy exceeded {CURRENT_SCHEMA_TEMPLATE_DEADLINE:?}"
                )),
            );
        }
        let step = backup
            .step(CURRENT_SCHEMA_TEMPLATE_PAGES_PER_STEP)
            .map_err(|error| CurrentSchemaTemplateErrorCode::BackupStep.error(error))?;
        let progress = backup.progress();
        let observation = match step {
            StepResult::More => CurrentSchemaTemplateBackupObservation::More(progress.remaining),
            StepResult::Done => CurrentSchemaTemplateBackupObservation::Done(progress.remaining),
            StepResult::Busy => CurrentSchemaTemplateBackupObservation::Busy,
            StepResult::Locked => CurrentSchemaTemplateBackupObservation::Locked,
            _ => CurrentSchemaTemplateBackupObservation::Unknown,
        };
        match budget.observe(observation)? {
            CurrentSchemaTemplateBackupAction::Continue => {}
            CurrentSchemaTemplateBackupAction::Pause => {
                std::thread::sleep(CURRENT_SCHEMA_TEMPLATE_RETRY_PAUSE);
            }
            CurrentSchemaTemplateBackupAction::Done => break,
        }
    }
    drop(backup);
    Ok((source_catalog.to_vec(), source_page_count))
}

#[cfg(any(test, feature = "test-seam"))]
fn backup_current_schema_template(
    source: &CurrentSchemaTemplateBacking,
    destination: &mut Connection,
) -> Result<(Vec<CurrentSchemaCatalogRow>, i64)> {
    let deadline = Instant::now() + CURRENT_SCHEMA_TEMPLATE_DEADLINE;
    match source {
        CurrentSchemaTemplateBacking::Shared(source) => {
            let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
                | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW;
            let conn = Connection::open_with_flags(&source.path, flags)
                .map_err(|error| CurrentSchemaTemplateErrorCode::CacheOpen.error(error))?;
            Store::register_sql_functions(&conn)
                .map_err(|error| CurrentSchemaTemplateErrorCode::CacheOpen.error(error))?;
            conn.execute_batch("PRAGMA foreign_keys=ON;")
                .map_err(|error| CurrentSchemaTemplateErrorCode::CacheOpen.error(error))?;
            let (_, page_count) = verify_current_schema_connection(
                &conn,
                Some(&source.catalog),
                CurrentSchemaTemplateErrorCode::SourceInvariant,
            )?;
            if page_count != source.page_count {
                return Err(
                    CurrentSchemaTemplateErrorCode::SourceInvariant.error(format_args!(
                        "page_count={page_count}, recorded={}",
                        source.page_count
                    )),
                );
            }
            backup_current_schema_template_connection(
                &conn,
                &source.catalog,
                source.page_count,
                destination,
                deadline,
            )
        }
        CurrentSchemaTemplateBacking::Local(source) => {
            let source = try_current_schema_template_lock(source, deadline)?;
            verify_current_schema_template_source_for_copy(&source)?;
            backup_current_schema_template_connection(
                &source.conn,
                &source.catalog,
                source.page_count,
                destination,
                deadline,
            )
        }
    }
}

#[cfg(any(test, feature = "test-seam"))]
fn open_in_memory_from_current_schema_template_with_timestamp(
    timestamp: impl FnOnce() -> String,
) -> Result<Store> {
    let source = current_schema_template()?;
    let mut store = raw_in_memory_store_for_test()
        .map_err(|error| CurrentSchemaTemplateErrorCode::DestinationInvariant.error(error))?;
    let (catalog, _) = backup_current_schema_template(source, &mut store.conn)?;
    verify_current_schema_connection(
        &store.conn,
        Some(&catalog),
        CurrentSchemaTemplateErrorCode::DestinationInvariant,
    )?;
    let timestamp = timestamp();
    seed_d04_test_project(&store, &timestamp)?;
    Ok(store)
}

#[cfg(any(test, feature = "test-seam"))]
pub(crate) fn current_schema_template_source_proof_for_test() -> Result<(i64, i64)> {
    let source = current_schema_template()?;
    match source {
        CurrentSchemaTemplateBacking::Shared(source) => {
            let reopened = open_shared_current_schema_template(&source.path)?;
            Ok((reopened.page_count, 0))
        }
        CurrentSchemaTemplateBacking::Local(source) => {
            let deadline = Instant::now() + CURRENT_SCHEMA_TEMPLATE_DEADLINE;
            let source = try_current_schema_template_lock(source, deadline)?;
            verify_current_schema_template_source_for_copy(&source)?;
            let zero_seed_rows = current_schema_template_zero_seed_rows(
                &source.conn,
                CurrentSchemaTemplateErrorCode::SourceInvariant,
            )?;
            Ok((source.page_count, zero_seed_rows))
        }
    }
}

#[cfg(any(test, feature = "test-seam"))]
pub(crate) fn current_schema_template_is_unlocked_and_zero_seed_for_test() -> bool {
    let Ok(source) = current_schema_template() else {
        return false;
    };
    match source {
        CurrentSchemaTemplateBacking::Shared(source) => {
            open_shared_current_schema_template(&source.path).is_ok()
        }
        CurrentSchemaTemplateBacking::Local(source) => {
            let Ok(source) = source.try_lock() else {
                return false;
            };
            current_schema_template_zero_seed_rows(
                &source.conn,
                CurrentSchemaTemplateErrorCode::SourceInvariant,
            )
            .is_ok()
        }
    }
}

#[cfg(any(test, feature = "test-seam"))]
pub(crate) fn current_schema_template_disk_cache_for_test(
    directory: &Path,
) -> Result<(PathBuf, i64)> {
    let source = shared_current_schema_template_at(directory)?;
    Ok((source.path, source.page_count))
}

#[cfg(any(test, feature = "test-seam"))]
pub(crate) fn current_schema_template_cache_key_for_test() -> String {
    current_schema_template_cache_key()
}

#[cfg(any(test, feature = "test-seam"))]
pub(crate) fn current_schema_template_cached_failure_replay_for_test() -> (usize, String, String) {
    let cache: OnceLock<std::result::Result<(), CachedCurrentSchemaTemplateError>> =
        OnceLock::new();
    let attempts = Cell::new(0_usize);
    let mut errors = Vec::new();
    for _ in 0..2 {
        let result = current_schema_template_from_cache(&cache, || {
            attempts.set(attempts.get() + 1);
            Err(CachedCurrentSchemaTemplateError::new(
                CurrentSchemaTemplateErrorCode::InitStore,
            ))
        });
        errors.push(match result {
            Ok(_) => String::new(),
            Err(error) => error.to_string(),
        });
    }
    (attempts.get(), errors.remove(0), errors.remove(0))
}

#[cfg(any(test, feature = "test-seam"))]
pub(crate) fn current_schema_template_init_failure_for_test(join: bool) -> String {
    let mode = if join {
        CurrentSchemaTemplateInitMode::InjectJoinPanic
    } else {
        CurrentSchemaTemplateInitMode::InjectSpawnFailure
    };
    match initialize_current_schema_template(mode) {
        Ok(_) => String::new(),
        Err(error) => error.to_daemon().to_string(),
    }
}

#[cfg(any(test, feature = "test-seam"))]
pub(crate) fn current_schema_template_poison_failure_for_test() -> String {
    let mutex = std::sync::Arc::new(Mutex::new(()));
    let poisoner = std::sync::Arc::clone(&mutex);
    let _ = std::thread::spawn(move || {
        let _guard = poisoner.lock().ok();
        panic!("injected current-schema template mutex poison");
    })
    .join();
    match try_current_schema_template_lock(
        mutex.as_ref(),
        Instant::now() + CURRENT_SCHEMA_TEMPLATE_DEADLINE,
    ) {
        Ok(_) => String::new(),
        Err(error) => error.to_string(),
    }
}

#[cfg(any(test, feature = "test-seam"))]
pub(crate) fn current_schema_template_backup_bounds_for_test()
-> (i32, i64, usize, usize, usize, Duration) {
    (
        CURRENT_SCHEMA_TEMPLATE_PAGES_PER_STEP,
        CURRENT_SCHEMA_TEMPLATE_PAGE_LIMIT,
        CURRENT_SCHEMA_TEMPLATE_STEP_LIMIT,
        CURRENT_SCHEMA_TEMPLATE_NO_PROGRESS_LIMIT,
        CURRENT_SCHEMA_TEMPLATE_BUSY_LOCKED_LIMIT,
        CURRENT_SCHEMA_TEMPLATE_DEADLINE,
    )
}

#[cfg(any(test, feature = "test-seam"))]
pub(crate) fn current_schema_template_backup_seam_for_test(
    mutex: &Mutex<()>,
    page_count: i64,
    observations: &[CurrentSchemaTemplateBackupObservation],
    inject_deadline: bool,
) -> Result<()> {
    let deadline = Instant::now() + CURRENT_SCHEMA_TEMPLATE_DEADLINE;
    let guard = try_current_schema_template_lock(mutex, deadline)?;
    let mut budget = CurrentSchemaTemplateBackupBudget::new(page_count)?;
    if inject_deadline {
        return Err(CurrentSchemaTemplateErrorCode::BackupDeadline.error("injected copy deadline"));
    }
    for observation in observations {
        match budget.observe(*observation)? {
            CurrentSchemaTemplateBackupAction::Continue
            | CurrentSchemaTemplateBackupAction::Pause => {}
            CurrentSchemaTemplateBackupAction::Done => {
                drop(guard);
                return Ok(());
            }
        }
    }
    Err(CurrentSchemaTemplateErrorCode::BackupStepBudget.error("incomplete injected backup"))
}

#[cfg(any(test, feature = "test-seam"))]
pub(crate) fn cached_current_schema_early_failure_for_test() -> Result<Store> {
    current_schema_template()?;
    Err(CurrentSchemaTemplateErrorCode::DestinationInvariant.error("injected early clone failure"))
}

impl Store {
    pub fn set_program_run_boot_id(&self, boot_id: Uuid) -> Result<()> {
        if boot_id.is_nil() {
            return Err(DaemonError::Store(
                "ProgramRun daemon boot identity must be non-nil".into(),
            ));
        }
        self.program_run_boot_id.set(boot_id);
        Ok(())
    }

    pub fn program_run_boot_id(&self) -> Uuid {
        self.program_run_boot_id.get()
    }

    /// Re-seed the agent-message delivery boot identity.
    ///
    /// Mirrors [`Store::set_program_run_boot_id`] exactly, including the
    /// non-nil rejection: the nil UUID is the one value that would compare
    /// equal across two distinct daemon incarnations, so admitting it would
    /// defeat the fence this identity exists to provide.
    ///
    /// # Errors
    ///
    /// Returns [`DaemonError::Store`] when `boot_id` is nil.
    pub fn set_delivery_boot_id(&self, boot_id: Uuid) -> Result<()> {
        if boot_id.is_nil() {
            return Err(DaemonError::Store(
                "agent-message delivery boot identity must be non-nil".into(),
            ));
        }
        self.delivery_boot_id.set(boot_id);
        Ok(())
    }

    /// The identity to stamp into the next delivery attempt's fence.
    //
    // Both halves of the production pair now exist, so the `dead_code` allow
    // that stood here is gone. The *writer* is `SessionManager::new`, which
    // seeds this cell once per daemon incarnation (H21-P2-R4-002). The *reader*
    // is `session::agent_message_delivery::deliver_at_idle_boundary`, which
    // stamps this value into every `ClaimAgentMessageRequest` it builds at the
    // monitor's idle boundary (P2-05b) — so a delivery attempt's
    // `MessageAttemptFenceV1::delivery_boot_id` is now a genuine daemon
    // identity in production rather than only a test literal.
    //
    // CORRECTED BY REVIEW R5 (H21-P2-R5-001, fix option (ii)). The prose here
    // previously read: "That is what makes the boot-id MISMATCH usable as the
    // durable no-effect proof for an attempt crashed between its claim commit
    // and its dispatch." **That claim is WITHDRAWN — it was unsound.**
    //
    // A boot-id mismatch proves only that the incarnation which wrote the row is
    // dead. It does NOT prove the dead incarnation never completed a send,
    // because the first durable trace of the send is the TX-C admission record
    // written strictly AFTER it. A crash in the interval between the send and
    // that record leaves the identical durable triple — `claimed`, foreign boot
    // id, no recorded admission — as a genuine pre-dispatch crash.
    //
    // So: `claimed` + foreign `delivery_boot_id` + no recorded admission is
    // **`uncertain`, NOT `proved_no_effect`**, and P2-06 MUST NOT requeue on it.
    // That requeue is BLOCKED pending a durable PRE-DISPATCH marker. The full
    // derivation, the three ways into the window, and the structural fix are in
    // the crash-window rule stated in `agent_message_delivery`.
    //
    // This identity remains genuinely useful and is NOT dead: it is what lets a
    // recovery pass tell a FOREIGN incarnation's attempt from one belonging to
    // the live incarnation. It simply does not, on its own, license a requeue.
    pub fn delivery_boot_id(&self) -> Uuid {
        self.delivery_boot_id.get()
    }

    /// Register every deterministic SQL scalar function the schema's raw CHECK
    /// constraints depend on.
    ///
    /// This is the ONLY registration site (C-P2-17). It must run immediately
    /// after a connection is created and strictly BEFORE any PRAGMA,
    /// `init_schema()`, migration, schema evaluation, or reopen, because V81
    /// CHECK constraints call `rsi_jsonrpc_id_is_canonical` and a connection
    /// lacking it errors rather than silently admitting a malformed row.
    ///
    /// # Errors
    ///
    /// Returns a store error when SQLite refuses the function registration.
    pub fn register_sql_functions(conn: &Connection) -> Result<()> {
        use rusqlite::functions::FunctionFlags;

        // RSI-RELEASED-MIGRATION-BEGIN: v120-source-worktree-session-relevance-function
        conn.create_scalar_function(
            "rsi_swc_v120_session_dependency_relevance",
            31,
            FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
            source_worktree_v120::session_dependency_relevance_sql,
        )?;
        // RSI-RELEASED-MIGRATION-END: v120-source-worktree-session-relevance-function

        conn.create_scalar_function(
            "rsi_jsonrpc_id_is_canonical",
            1,
            FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
            |ctx| {
                let raw = ctx.get_raw(0);
                Ok(match raw {
                    // NULL propagates: nullable columns guard with IS NULL OR ...
                    rusqlite::types::ValueRef::Null => None,
                    rusqlite::types::ValueRef::Text(bytes) => Some(i32::from(
                        std::str::from_utf8(bytes).is_ok_and(is_canonical_jsonrpc_id),
                    )),
                    // A non-text value can never be a canonical ID.
                    _ => Some(0),
                })
            },
        )?;
        for (name, predicate) in [
            (
                "rsi_uuid_is_canonical",
                is_canonical_uuid as fn(&str) -> bool,
            ),
            (
                "rsi_sha256_digest_is_canonical",
                is_canonical_sha256_digest as fn(&str) -> bool,
            ),
            (
                "rsi_rfc3339_nanos_is_canonical",
                is_canonical_rfc3339_nanos as fn(&str) -> bool,
            ),
            ("rsi_rfc3339_is_valid", is_valid_rfc3339 as fn(&str) -> bool),
        ] {
            conn.create_scalar_function(
                name,
                1,
                FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
                move |ctx| {
                    let raw = ctx.get_raw(0);
                    Ok(match raw {
                        rusqlite::types::ValueRef::Null => None,
                        rusqlite::types::ValueRef::Text(bytes) => {
                            Some(i32::from(std::str::from_utf8(bytes).is_ok_and(predicate)))
                        }
                        _ => Some(0),
                    })
                },
            )?;
        }
        conn.create_scalar_function(
            "rsi_execution_origin_request_key_is_canonical",
            1,
            FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
            |ctx| {
                let raw = ctx.get_raw(0);
                Ok(match raw {
                    rusqlite::types::ValueRef::Null => None,
                    rusqlite::types::ValueRef::Text(bytes) => Some(i32::from(
                        std::str::from_utf8(bytes)
                            .is_ok_and(origin_authority::is_canonical_execution_origin_request_key),
                    )),
                    _ => Some(0),
                })
            },
        )?;
        conn.create_scalar_function(
            "rsi_execution_origin_request_key_family",
            1,
            FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
            |ctx| {
                let raw = ctx.get_raw(0);
                Ok(match raw {
                    rusqlite::types::ValueRef::Null => None,
                    rusqlite::types::ValueRef::Text(bytes) => std::str::from_utf8(bytes)
                        .ok()
                        .and_then(origin_authority::execution_origin_request_key_family),
                    _ => None,
                })
            },
        )?;
        conn.create_scalar_function(
            "rsi_execution_origin_request_key_matches_claim",
            10,
            FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
            |ctx| {
                let request_key: String = ctx.get(0)?;
                let claimant_kind: String = ctx.get(1)?;
                let source_session_id: String = ctx.get(2)?;
                let scheduled_job_id: Option<String> = ctx.get(3)?;
                let scheduled_fire_at: Option<String> = ctx.get(4)?;
                let rotation_id: Option<String> = ctx.get(5)?;
                let source_model_invocation_id: Option<String> = ctx.get(6)?;
                let rotation_action_digest: Option<String> = ctx.get(7)?;
                let retry_attempt: Option<i64> = ctx.get(8)?;
                let c5_marker_digest: Option<String> = ctx.get(9)?;
                Ok(i32::from(
                    origin_authority::execution_origin_request_key_matches_claim(
                        &request_key,
                        &claimant_kind,
                        &source_session_id,
                        scheduled_job_id.as_deref(),
                        scheduled_fire_at.as_deref(),
                        rotation_id.as_deref(),
                        source_model_invocation_id.as_deref(),
                        rotation_action_digest.as_deref(),
                        retry_attempt,
                        c5_marker_digest.as_deref(),
                    ),
                ))
            },
        )?;
        conn.create_scalar_function(
            "rsi_execution_origin_request_key_matches_receipt",
            4,
            FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
            |ctx| {
                let request_key: String = ctx.get(0)?;
                let source_session_id: Option<String> = ctx.get(1)?;
                let scheduled_job_id: Option<String> = ctx.get(2)?;
                let scheduled_fire_at: Option<String> = ctx.get(3)?;
                Ok(i32::from(
                    origin_authority::execution_origin_request_key_matches_receipt(
                        &request_key,
                        source_session_id.as_deref(),
                        scheduled_job_id.as_deref(),
                        scheduled_fire_at.as_deref(),
                    ),
                ))
            },
        )?;
        conn.create_scalar_function(
            "rsi_execution_origin_controller_is_canonical",
            15,
            FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
            |ctx| {
                let values = (
                    ctx.get::<String>(0),
                    ctx.get::<String>(1),
                    ctx.get::<String>(2),
                    ctx.get::<Option<String>>(3),
                    ctx.get::<Option<String>>(4),
                    ctx.get::<Option<i64>>(5),
                    ctx.get::<Option<String>>(6),
                    ctx.get::<Option<String>>(7),
                    ctx.get::<Option<String>>(8),
                    ctx.get::<Option<String>>(9),
                    ctx.get::<Option<String>>(10),
                    ctx.get::<Option<String>>(11),
                    ctx.get::<Option<String>>(12),
                    ctx.get::<Option<String>>(13),
                    ctx.get::<Option<String>>(14),
                );
                let (
                    Ok(claimant_kind),
                    Ok(source_session_id),
                    Ok(claimant_session_id),
                    Ok(source_model_invocation_id),
                    Ok(rotation_action_digest),
                    Ok(retry_attempt),
                    Ok(c5_marker_digest),
                    Ok(controller_project_id),
                    Ok(controller_idea_id),
                    Ok(controller_transfer_key),
                    Ok(controller_reservation_id),
                    Ok(controller_candidate_session_id),
                    Ok(controller_base_row_id),
                    Ok(controller_base_event_id),
                    Ok(controller_expires_at),
                ) = values
                else {
                    return Ok(0);
                };
                Ok(i32::from(
                    origin_authority::execution_origin_controller_is_canonical(
                        &claimant_kind,
                        &source_session_id,
                        &claimant_session_id,
                        source_model_invocation_id.as_deref(),
                        rotation_action_digest.as_deref(),
                        retry_attempt,
                        c5_marker_digest.as_deref(),
                        controller_project_id.as_deref(),
                        controller_idea_id.as_deref(),
                        controller_transfer_key.as_deref(),
                        controller_reservation_id.as_deref(),
                        controller_candidate_session_id.as_deref(),
                        controller_base_row_id.as_deref(),
                        controller_base_event_id.as_deref(),
                        controller_expires_at.as_deref(),
                    ),
                ))
            },
        )?;
        conn.create_scalar_function(
            "rsi_execution_origin_c5_is_canonical",
            9,
            FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
            |ctx| {
                let values = (
                    ctx.get::<String>(0),
                    ctx.get::<Option<String>>(1),
                    ctx.get::<Option<String>>(2),
                    ctx.get::<Option<String>>(3),
                    ctx.get::<Option<String>>(4),
                    ctx.get::<Option<String>>(5),
                    ctx.get::<Option<String>>(6),
                    ctx.get::<Option<i64>>(7),
                    ctx.get::<Option<i64>>(8),
                );
                let (
                    Ok(source_session_id),
                    Ok(prepared_terminal_status),
                    Ok(prepared_c5_cause),
                    Ok(prepared_c5_key),
                    Ok(prepared_c5_value),
                    Ok(prepared_c5_at),
                    Ok(prepared_c5_digest),
                    Ok(prepared_c5_expected_retry_count),
                    Ok(prepared_c5_max_retries),
                ) = values
                else {
                    return Ok(0);
                };
                Ok(i32::from(
                    origin_authority::execution_origin_c5_is_canonical(
                        &source_session_id,
                        prepared_terminal_status.as_deref(),
                        prepared_c5_cause.as_deref(),
                        prepared_c5_key.as_deref(),
                        prepared_c5_value.as_deref(),
                        prepared_c5_at.as_deref(),
                        prepared_c5_digest.as_deref(),
                        prepared_c5_expected_retry_count,
                        prepared_c5_max_retries,
                    ),
                ))
            },
        )?;
        Ok(())
    }

    /// Filesystem database path; in-memory fixtures have no recovery target.
    pub fn database_path(&self) -> Option<&str> {
        self.conn.path().filter(|path| !path.is_empty())
    }

    pub fn schema_version(&self) -> Result<i32> {
        Ok(self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))?)
    }

    /// Open or create database at the given path.
    /// Runs schema initialization on open.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        // Refuse a newer schema before the first PRAGMA write (journal_mode
        // can rewrite the file header) or any DDL/DML.
        Self::refuse_newer_schema(&conn)?;
        Self::register_sql_functions(&conn)?;
        migration_backup::before_migration(&conn, path)?;

        // Enable WAL mode for better concurrent read performance
        conn.execute_batch("PRAGMA journal_mode=WAL;")?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;
        // Give external clients (sqlite3 CLI, validator binaries, the db skill) a
        // grace window instead of an instant SQLITE_BUSY when they race the
        // daemon's writer. In-process access is already serialized by
        // Arc<Mutex<Store>>, so this only helps external raw-SQLite access.
        // Note: rusqlite's Connection::open already calls sqlite3_busy_timeout(db, 5000)
        // internally as an implementation detail (not a documented API guarantee), so we
        // deliberately set a distinct, larger value here to make our own timeout explicit,
        // future-proof against upstream default changes, and independently verifiable.
        conn.execute_batch("PRAGMA busy_timeout=10000;")?;

        let store = Self {
            conn,
            controller_grants: RefCell::new(HashMap::new()),
            controller_grant_incarnations: RefCell::new(HashMap::new()),
            program_run_boot_id: Cell::new(Uuid::new_v4()),
            delivery_boot_id: Cell::new(Uuid::new_v4()),
            hub_reports: RefCell::new(std::collections::VecDeque::new()),
        };
        store.init_schema()?;
        Ok(store)
    }

    /// Open an in-memory SQLite database for testing.
    /// Clones the process-wide migration-built current-schema template.
    #[cfg(any(test, feature = "test-seam"))]
    pub fn open_in_memory() -> Result<Self> {
        open_in_memory_from_current_schema_template_with_timestamp(|| {
            chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
        })
    }

    /// Build a current-schema in-memory fixture by replaying every migration.
    #[cfg(any(test, feature = "test-seam"))]
    pub(crate) fn open_in_memory_via_migrations_for_test() -> Result<Self> {
        let store = raw_in_memory_store_for_test()?;
        store.init_schema()?;
        verify_current_schema_connection(
            &store.conn,
            None,
            CurrentSchemaTemplateErrorCode::DestinationInvariant,
        )?;
        let timestamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        seed_d04_test_project(&store, &timestamp)?;
        Ok(store)
    }

    #[cfg(any(test, feature = "test-seam"))]
    pub(crate) fn open_in_memory_with_timestamp_for_test(
        timestamp: impl FnOnce() -> String,
    ) -> Result<Self> {
        open_in_memory_from_current_schema_template_with_timestamp(timestamp)
    }

    #[cfg(any(test, feature = "test-seam"))]
    pub(crate) fn open_in_memory_v92_for_test() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::register_sql_functions(&conn)?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;
        let store = Self {
            conn,
            controller_grants: RefCell::new(HashMap::new()),
            controller_grant_incarnations: RefCell::new(HashMap::new()),
            program_run_boot_id: Cell::new(Uuid::new_v4()),
            delivery_boot_id: Cell::new(Uuid::new_v4()),
            hub_reports: RefCell::new(std::collections::VecDeque::new()),
        };
        store.init_schema_internal(false, true)?;
        Ok(store)
    }

    /// Build an authenticated predecessor shell without executing V85-V88.
    /// V88 migration tests use this to construct deployed V87 inputs solely
    /// from the sealed predecessor literals instead of rewinding V88.
    #[cfg(any(test, feature = "test-seam"))]
    pub(crate) fn open_test_predecessor_v84(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        Self::register_sql_functions(&conn)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=10000;",
        )?;
        let store = Self {
            conn,
            controller_grants: RefCell::new(HashMap::new()),
            controller_grant_incarnations: RefCell::new(HashMap::new()),
            program_run_boot_id: Cell::new(Uuid::new_v4()),
            delivery_boot_id: Cell::new(Uuid::new_v4()),
            hub_reports: RefCell::new(std::collections::VecDeque::new()),
        };
        store.init_schema_internal(true, false)?;
        Ok(store)
    }

    /// Ensure the tag autocomplete index exists (idempotent — V43 already
    /// creates it). Called once at daemon startup as a safety net.
    pub fn ensure_tag_indexes(&self) -> rusqlite::Result<()> {
        self.conn
            .execute_batch("CREATE INDEX IF NOT EXISTS idx_session_tags_tag ON session_tags(tag);")
    }

    /// Initialize schema with version-based migrations.
    /// Uses PRAGMA user_version to track which migrations have been applied.
    fn init_schema(&self) -> Result<()> {
        self.init_schema_internal(false, false)
            .map_err(lineage_convergence::annotate_v112_branched_lineage_fault)
    }

    /// Fail closed when the database's `PRAGMA user_version` is greater than
    /// `LATEST_SCHEMA_VERSION`. Read-only: it runs no PRAGMA write and no
    /// statement that could change the file. A failed version read is an
    /// error, never coerced to zero.
    fn refuse_newer_schema(conn: &Connection) -> Result<()> {
        let version: i32 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version > LATEST_SCHEMA_VERSION {
            return Err(DaemonError::SchemaTooNew {
                database_version: version,
                supported_version: LATEST_SCHEMA_VERSION,
            });
        }
        Ok(())
    }

    fn init_schema_internal(&self, stop_after_v84: bool, stop_after_v92: bool) -> Result<()> {
        Self::refuse_newer_schema(&self.conn)?;
        let version: i32 = self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))?;

        // One file per schema version under `migrations/`, collected in
        // ascending order by `build.rs` (see `MIGRATION_STEPS`). Each step is
        // gated on `version` internally, so every open runs the full list.
        for &(step_version, migrate) in MIGRATION_STEPS {
            migrate(self, version)?;
            if stop_after_v84 && step_version == 84 {
                debug_assert_eq!(
                    self.conn
                        .query_row("PRAGMA user_version", [], |row| row.get::<_, i32>(0))
                        .unwrap_or_default(),
                    84
                );
                return Ok(());
            }
            if stop_after_v92 && step_version == 92 {
                return Ok(());
            }
        }

        // V120 definitions and the internal cursor key are authenticated on
        // every reopen, before any projection reconciliation can write.
        source_worktree_v120::validate_v120_catalog(&self.conn)?;
        let live_version: i32 = self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if live_version < 134 {
            manager_review_v121::validate_v121_catalog(&self.conn)?;
        }
        agent_coordination::watch_repair_v123::validate_v123_catalog(&self.conn)?;
        crate::store::daemon_restart_persistence::validate_v128_catalog(&self.conn)?;
        topology_v129::validate_v129_catalog(&self.conn)?;
        topology_agent_audit::validate_v133_catalog(&self.conn)?;
        restart_intents::validate_v134_catalog(&self.conn)?;
        satellite_registry::validate_catalog(&self.conn)?;

        // Path evidence is never assumed from the raw scheduled-job field.
        // Reconciliation is bounded and leaves malformed/missing rows
        // unverified for later dependency proof to retain rather than ignore.
        source_worktree_v120::reconcile_scheduled_job_path_projections(&self.conn)?;

        Ok(())
    }

    /// Add a column to a table if it doesn't already exist.
    /// Uses pragma_table_info for introspection to ensure idempotency.
    fn add_column_if_not_exists(&self, table: &str, column: &str, col_type: &str) -> Result<()> {
        let exists: bool = self
            .conn
            .prepare(&format!(
                "SELECT COUNT(*) FROM pragma_table_info('{}') WHERE name = '{}'",
                table, column
            ))?
            .query_row([], |row| row.get::<_, i64>(0))?
            > 0;

        if !exists {
            self.conn.execute(
                &format!("ALTER TABLE {} ADD COLUMN {} {}", table, column, col_type),
                [],
            )?;
        }
        Ok(())
    }
}

fn sqlite_table_exists_tx(tx: &rusqlite::Transaction<'_>, table: &str) -> Result<bool> {
    Ok(tx.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name = ?1",
        params![table],
        |row| row.get::<_, i64>(0),
    )? > 0)
}

fn column_exists_tx(tx: &rusqlite::Transaction<'_>, table: &str, column: &str) -> Result<bool> {
    Ok(tx.query_row(
        &format!(
            "SELECT COUNT(*) FROM pragma_table_info('{}') WHERE name = ?1",
            table
        ),
        params![column],
        |row| row.get::<_, i64>(0),
    )? > 0)
}

fn add_column_if_not_exists_tx(
    tx: &rusqlite::Transaction<'_>,
    table: &str,
    column: &str,
    col_type: &str,
) -> Result<()> {
    if !column_exists_tx(tx, table, column)? {
        tx.execute(
            &format!("ALTER TABLE {} ADD COLUMN {} {}", table, column, col_type),
            [],
        )?;
    }
    Ok(())
}
