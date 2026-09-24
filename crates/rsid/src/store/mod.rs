//! SQLite persistence layer for the flywheel daemon.
//!
//! Submodules split by domain: sessions, events, projects, metrics, approvals.
//! Row mappers and enum converters live in `row_mappers`.

pub(crate) mod agent_child_relaunch_intents;
pub(crate) mod agent_coordination;
mod approvals;
pub(crate) mod archive_cleanup;
pub(crate) mod capacity_recovery;
mod cards;
pub mod chain_iterations;
pub(crate) mod closure_kernel;
pub(crate) mod cohort_settlement;
mod compiled_prompts;
pub mod daemon_settings; // V48 — RSI-026 key-value table for daemon-owned settings.
mod esp_games;
mod events;
pub mod graph_cache;
pub(crate) mod harness_manager;
pub(crate) mod harness_manager_v2;
mod ideas;
pub(crate) mod manager_coordinator;
mod manager_decision_history;
pub(crate) mod manager_decisions;
pub(crate) mod pending_approvals;
pub(crate) mod source_worktree_v120;
pub(crate) use ideas::IdeaControllerReconciliationCursor;
#[cfg(test)]
pub(crate) use ideas::{IdeaControllerWriteFault, inject_d03_idea_controller_write_fault};
mod catalog_convergence;
mod issues;
mod labels;
mod lineage_convergence;
pub(crate) mod manager_actions;
pub(crate) mod manager_intent;
pub(crate) mod manager_ledger;
mod manager_notices;
mod manager_prepared_actions;
pub(crate) mod manager_resources;
pub(crate) mod manager_review_v121;
pub(crate) mod manager_reviews;
pub(crate) mod manager_successions;
pub(crate) mod manager_watch_settlement;
mod metrics;
mod model_control;
mod observations;
mod offload;
pub(crate) mod origin_authority;
pub(crate) mod pending_questions;
mod permissions;
pub(crate) mod program_runs;
mod projects;
pub mod queue;
mod rate_limits;
pub mod recursive_dag;
mod rotation_events;
mod row_mappers;
pub(crate) mod sandbox_custody;
#[allow(clippy::redundant_pub_crate)]
pub(crate) mod sandbox_reclaim;
pub mod scheduled_jobs;
mod session_diagnostics;
mod session_model_updates;
mod sessions;
pub(crate) mod successor_reservations;
mod summaries;
pub(crate) mod target_reclaim_sweep;
#[cfg(test)]
pub(crate) mod tests;
mod topologies; // P1.4 — DB-stored named topology templates.
pub(crate) mod topology_v129; // #634 durable topology executor tables.
mod usage; // T8 — read-only lifetime usage aggregate for Settings -> Stats.
mod workflows;

#[cfg(test)]
pub(crate) use issues::master_no_idle_test_fail_after_wake;
pub(crate) use issues::{C5SettlementOutcome, MasterNoIdleStoreRecovery};
pub(crate) use model_control::{CapacityStoreAdmissionOutcome, OrchestrationEscalationDenial};
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
#[cfg(test)]
use std::fs::{File, OpenOptions, TryLockError as FileTryLockError};
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
#[cfg(test)]
use std::sync::{Mutex, MutexGuard, OnceLock, TryLockError as MutexTryLockError};
#[cfg(test)]
use std::time::{Duration, Instant};
use uuid::Uuid;

/// Latest schema version applied by `init_schema()`.
///
/// Bump this constant alongside the highest `if version < N` block in
/// `init_schema()` whenever a new migration is added. Tests assert against
/// this constant rather than a hard-coded literal so future migrations don't
/// recur the schema-version-bump cluster fixed in RSI-020.
///
/// Released migration DDL, catalog projections, and fingerprints are
/// immutable. Never repair them in place; add a forward migration. The
/// repository guard pins each released block and the marked helper regions.
pub const LATEST_SCHEMA_VERSION: i32 = 132;

// RSI-RELEASED-MIGRATION-BEGIN: v112-archive-cleanup-catalog
pub(crate) const V112_ARCHIVE_CATALOG_OBJECTS: [(&str, &str); 10] = [
    ("index", "archive_cleanup_runs_one_open_generation"),
    ("index", "archive_cleanup_runs_recovery_scan"),
    ("index", "archive_cleanup_runs_session_readback"),
    ("table", "archive_cleanup_events"),
    ("table", "archive_cleanup_runs"),
    ("trigger", "archive_cleanup_events_v101_no_delete"),
    ("trigger", "archive_cleanup_events_v101_no_update"),
    ("trigger", "archive_cleanup_runs_v101_identity_immutable"),
    ("trigger", "archive_cleanup_runs_v101_no_delete"),
    ("trigger", "archive_cleanup_runs_v101_phase_forward"),
];
// RSI-RELEASED-MIGRATION-END: v112-archive-cleanup-catalog

/// Ordered atomic boundaries of the V112 ordinary archive-cleanup journal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ArchiveCleanupV112MigrationFault {
    AfterPreflight,
    AfterTables,
    AfterIndexes,
    AfterTriggers,
    AfterChecks,
    AfterUserVersion,
    BeforeCommit,
}

impl ArchiveCleanupV112MigrationFault {
    #[cfg(test)]
    pub(crate) const ALL: [Self; 7] = [
        Self::AfterPreflight,
        Self::AfterTables,
        Self::AfterIndexes,
        Self::AfterTriggers,
        Self::AfterChecks,
        Self::AfterUserVersion,
        Self::BeforeCommit,
    ];
}

#[cfg(test)]
thread_local! {
    static ARCHIVE_CLEANUP_V112_MIGRATION_FAULT: RefCell<Option<ArchiveCleanupV112MigrationFault>> = const { RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn archive_cleanup_v112_test_fail_next_migration(
    fault: ArchiveCleanupV112MigrationFault,
) {
    ARCHIVE_CLEANUP_V112_MIGRATION_FAULT.with(|slot| *slot.borrow_mut() = Some(fault));
}

#[cfg(test)]
fn archive_cleanup_v112_migration_fault(fault: ArchiveCleanupV112MigrationFault) -> Result<()> {
    let injected = ARCHIVE_CLEANUP_V112_MIGRATION_FAULT.with(|slot| {
        if *slot.borrow() == Some(fault) {
            *slot.borrow_mut() = None;
            true
        } else {
            false
        }
    });
    if injected {
        return Err(DaemonError::Store(format!(
            "injected V112 archive cleanup migration fault: {fault:?}"
        )));
    }
    Ok(())
}

#[cfg(not(test))]
fn archive_cleanup_v112_migration_fault(_: ArchiveCleanupV112MigrationFault) -> Result<()> {
    Ok(())
}

/// Ordered atomic boundaries of the V112 Operator Views identity convergence migration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IdentityV112MigrationFault {
    AfterPreflight,
    AfterColumns,
    AfterBackfill,
    AfterIndexes,
    AfterTriggers,
    AfterChecks,
    AfterUserVersion,
    BeforeCommit,
}

impl IdentityV112MigrationFault {
    #[cfg(test)]
    pub(crate) const ALL: [Self; 8] = [
        Self::AfterPreflight,
        Self::AfterColumns,
        Self::AfterBackfill,
        Self::AfterIndexes,
        Self::AfterTriggers,
        Self::AfterChecks,
        Self::AfterUserVersion,
        Self::BeforeCommit,
    ];
}

#[cfg(test)]
thread_local! {
    static IDENTITY_V112_MIGRATION_FAULT: RefCell<Option<IdentityV112MigrationFault>> = const { RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn identity_v112_test_fail_next_migration(fault: IdentityV112MigrationFault) {
    IDENTITY_V112_MIGRATION_FAULT.with(|slot| *slot.borrow_mut() = Some(fault));
}

#[cfg(test)]
pub(crate) fn identity_v112_test_clear_migration_fault() {
    IDENTITY_V112_MIGRATION_FAULT.with(|slot| *slot.borrow_mut() = None);
}

#[cfg(test)]
fn identity_v112_migration_fault(fault: IdentityV112MigrationFault) -> Result<()> {
    let injected = IDENTITY_V112_MIGRATION_FAULT.with(|slot| {
        if *slot.borrow() == Some(fault) {
            *slot.borrow_mut() = None;
            true
        } else {
            false
        }
    });
    if injected {
        return Err(DaemonError::Store(format!(
            "injected V112 identity convergence fault: {fault:?}"
        )));
    }
    Ok(())
}

#[cfg(not(test))]
fn identity_v112_migration_fault(_: IdentityV112MigrationFault) -> Result<()> {
    Ok(())
}

// RSI-RELEASED-MIGRATION-BEGIN: v113-archive-projection-catalog
pub(crate) const V113_ARCHIVE_PROJECTION_CATALOG_OBJECTS: [(&str, &str); 10] = [
    ("index", "archive_cleanup_projection_consumers_pending"),
    ("index", "archive_cleanup_success_projections_pending"),
    ("table", "archive_cleanup_projection_consumers"),
    ("table", "archive_cleanup_success_projections"),
    (
        "trigger",
        "archive_cleanup_projection_consumers_v103_forward",
    ),
    (
        "trigger",
        "archive_cleanup_projection_consumers_v103_no_delete",
    ),
    (
        "trigger",
        "archive_cleanup_success_projections_v103_association_insert",
    ),
    (
        "trigger",
        "archive_cleanup_success_projections_v103_identity_immutable",
    ),
    (
        "trigger",
        "archive_cleanup_success_projections_v103_no_delete",
    ),
    (
        "trigger",
        "archive_cleanup_success_projections_v103_state_forward",
    ),
];
const V113_CONVERGED_FULL_CATALOG_FINGERPRINT: &str =
    "sha256:15b4036784bf33d7b5a9b757fb2212ceabe1e224200de155b6aec7cd0cddf68a";
// RSI-RELEASED-MIGRATION-END: v113-archive-projection-catalog

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ArchiveProjectionV113MigrationFault {
    AfterPreflight,
    AfterTables,
    AfterIndexes,
    AfterTriggers,
    AfterBackfill,
    AfterChecks,
    AfterUserVersion,
    BeforeCommit,
}

impl ArchiveProjectionV113MigrationFault {
    #[cfg(test)]
    pub(crate) const ALL: [Self; 8] = [
        Self::AfterPreflight,
        Self::AfterTables,
        Self::AfterIndexes,
        Self::AfterTriggers,
        Self::AfterBackfill,
        Self::AfterChecks,
        Self::AfterUserVersion,
        Self::BeforeCommit,
    ];
}

#[cfg(test)]
thread_local! {
    static ARCHIVE_PROJECTION_V113_MIGRATION_FAULT: RefCell<Option<ArchiveProjectionV113MigrationFault>> = const { RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn archive_projection_v113_test_fail_next_migration(
    fault: ArchiveProjectionV113MigrationFault,
) {
    ARCHIVE_PROJECTION_V113_MIGRATION_FAULT.with(|slot| *slot.borrow_mut() = Some(fault));
}

#[cfg(test)]
fn archive_projection_v113_migration_fault(
    fault: ArchiveProjectionV113MigrationFault,
) -> Result<()> {
    let injected = ARCHIVE_PROJECTION_V113_MIGRATION_FAULT.with(|slot| {
        if *slot.borrow() == Some(fault) {
            *slot.borrow_mut() = None;
            true
        } else {
            false
        }
    });
    if injected {
        return Err(DaemonError::Store(format!(
            "injected V113 archive projection migration fault: {fault:?}"
        )));
    }
    Ok(())
}

#[cfg(not(test))]
fn archive_projection_v113_migration_fault(_: ArchiveProjectionV113MigrationFault) -> Result<()> {
    Ok(())
}

// RSI-RELEASED-MIGRATION-BEGIN: v114-lineage-detachment-catalog
/// Every catalog object the V114 lineage-detachment migration installs, in the
/// `(type, name)` shape the fixture and chain assertions probe with.
pub(crate) const V114_LINEAGE_DETACHMENT_CATALOG_OBJECTS: [(&str, &str); 5] = [
    ("index", "session_lineage_detachments_predecessor"),
    ("table", "session_lineage_detachments"),
    ("trigger", "session_lineage_detachments_v114_no_delete"),
    ("trigger", "session_lineage_detachments_v114_no_update"),
    (
        "trigger",
        "session_lineage_detachments_v114_validate_insert",
    ),
];
// RSI-RELEASED-MIGRATION-END: v114-lineage-detachment-catalog

/// Ordered atomic boundaries of the V114 lineage-detachment migration. Every
/// boundary is inside one `Immediate` transaction that writes
/// `PRAGMA user_version = 114` last, so an injected fault rolls back to V113
/// with the catalog byte-identical and a clean retry succeeds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LineageDetachmentV114MigrationFault {
    AfterPreflight,
    AfterCatalog,
    AfterRows,
}

impl LineageDetachmentV114MigrationFault {
    #[cfg(test)]
    pub(crate) const ALL: [Self; 3] = [Self::AfterPreflight, Self::AfterCatalog, Self::AfterRows];
}

#[cfg(test)]
thread_local! {
    static LINEAGE_DETACHMENT_V114_MIGRATION_FAULT: RefCell<Option<LineageDetachmentV114MigrationFault>> = const { RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn lineage_detachment_v114_test_fail_next_migration(
    fault: LineageDetachmentV114MigrationFault,
) {
    LINEAGE_DETACHMENT_V114_MIGRATION_FAULT.with(|slot| *slot.borrow_mut() = Some(fault));
}

#[cfg(test)]
fn lineage_detachment_v114_migration_fault(
    fault: LineageDetachmentV114MigrationFault,
) -> Result<()> {
    let injected = LINEAGE_DETACHMENT_V114_MIGRATION_FAULT.with(|slot| {
        if *slot.borrow() == Some(fault) {
            *slot.borrow_mut() = None;
            true
        } else {
            false
        }
    });
    if injected {
        return Err(DaemonError::Store(format!(
            "injected V114 lineage detachment migration fault: {fault:?}"
        )));
    }
    Ok(())
}

#[cfg(not(test))]
fn lineage_detachment_v114_migration_fault(_: LineageDetachmentV114MigrationFault) -> Result<()> {
    Ok(())
}

/// Maximum number of active-like Session rows admitted to the startup
/// provider-inventory fence. The query reads at most one row beyond this
/// limit so startup memory remains bounded while still proving overflow.
pub(crate) const STARTUP_PROVIDER_CANDIDATE_MAX: usize = 16_384;

/// Ordered atomic boundaries of the V97 guarded Issue audit migration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IssueV97MigrationFault {
    AfterPreflight,
    AfterIssueColumns,
    AfterEventTable,
    AfterBaselineCopy,
    AfterIndexes,
    AfterTriggers,
    AfterParity,
    AfterChecks,
    AfterUserVersion,
    BeforeCommit,
}

impl IssueV97MigrationFault {
    #[cfg(test)]
    pub(crate) const ALL: [Self; 10] = [
        Self::AfterPreflight,
        Self::AfterIssueColumns,
        Self::AfterEventTable,
        Self::AfterBaselineCopy,
        Self::AfterIndexes,
        Self::AfterTriggers,
        Self::AfterParity,
        Self::AfterChecks,
        Self::AfterUserVersion,
        Self::BeforeCommit,
    ];
}

#[cfg(test)]
thread_local! {
    static ISSUE_V97_MIGRATION_FAULT: RefCell<Option<IssueV97MigrationFault>> = const { RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn issue_v97_test_fail_next_migration(fault: IssueV97MigrationFault) {
    ISSUE_V97_MIGRATION_FAULT.with(|slot| *slot.borrow_mut() = Some(fault));
}

#[cfg(test)]
pub(crate) fn issue_v97_test_clear_migration_fault() {
    ISSUE_V97_MIGRATION_FAULT.with(|slot| *slot.borrow_mut() = None);
}

#[cfg(test)]
fn issue_v97_migration_fault(fault: IssueV97MigrationFault) -> Result<()> {
    let injected = ISSUE_V97_MIGRATION_FAULT.with(|slot| {
        if *slot.borrow() == Some(fault) {
            *slot.borrow_mut() = None;
            true
        } else {
            false
        }
    });
    if injected {
        return Err(DaemonError::Store(format!(
            "injected V97 Issue migration fault: {fault:?}"
        )));
    }
    Ok(())
}

#[cfg(not(test))]
fn issue_v97_migration_fault(_: IssueV97MigrationFault) -> Result<()> {
    Ok(())
}

/// Ordered atomic boundaries of the V92 master-successor ledger migration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum H1V92MigrationFault {
    AfterPreflight,
    AfterSchema,
    AfterLeadSeed,
    AfterChecks,
    AfterUserVersion,
    BeforeCommit,
}

impl H1V92MigrationFault {
    pub(crate) const ALL: [Self; 6] = [
        Self::AfterPreflight,
        Self::AfterSchema,
        Self::AfterLeadSeed,
        Self::AfterChecks,
        Self::AfterUserVersion,
        Self::BeforeCommit,
    ];
}

#[cfg(test)]
thread_local! {
    static H1_V92_MIGRATION_FAULT: RefCell<Option<H1V92MigrationFault>> = const { RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn h1_v92_test_fail_next_migration(fault: H1V92MigrationFault) {
    H1_V92_MIGRATION_FAULT.with(|slot| *slot.borrow_mut() = Some(fault));
}

#[cfg(test)]
fn h1_v92_migration_fault(fault: H1V92MigrationFault) -> Result<()> {
    let injected = H1_V92_MIGRATION_FAULT.with(|slot| {
        if *slot.borrow() == Some(fault) {
            *slot.borrow_mut() = None;
            true
        } else {
            false
        }
    });
    if injected {
        return Err(DaemonError::Store(format!(
            "injected V92 migration fault: {fault:?}"
        )));
    }
    Ok(())
}

#[cfg(not(test))]
fn h1_v92_migration_fault(_: H1V92MigrationFault) -> Result<()> {
    Ok(())
}

/// Ordered atomic boundaries of the V91 exact-predecessor semantic repair.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum H1V91MigrationFault {
    AfterPreflight,
    AfterChecks,
    AfterUserVersion,
    BeforeCommit,
}

impl H1V91MigrationFault {
    pub(crate) const ALL: [Self; 4] = [
        Self::AfterPreflight,
        Self::AfterChecks,
        Self::AfterUserVersion,
        Self::BeforeCommit,
    ];
}

#[cfg(test)]
thread_local! {
    static H1_V91_MIGRATION_FAULT: RefCell<Option<H1V91MigrationFault>> = const { RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn h1_v91_test_fail_next_migration(fault: H1V91MigrationFault) {
    H1_V91_MIGRATION_FAULT.with(|slot| *slot.borrow_mut() = Some(fault));
}

#[cfg(test)]
pub(crate) fn h1_v91_test_clear_migration_fault() {
    H1_V91_MIGRATION_FAULT.with(|slot| *slot.borrow_mut() = None);
}

#[cfg(test)]
fn h1_v91_migration_fault(fault: H1V91MigrationFault) -> Result<()> {
    let injected = H1_V91_MIGRATION_FAULT.with(|slot| {
        if *slot.borrow() == Some(fault) {
            *slot.borrow_mut() = None;
            true
        } else {
            false
        }
    });
    if injected {
        return Err(DaemonError::Store(format!(
            "injected V91 migration fault: {fault:?}"
        )));
    }
    Ok(())
}

#[cfg(not(test))]
fn h1_v91_migration_fault(_: H1V91MigrationFault) -> Result<()> {
    Ok(())
}

/// Exact V90 descendant of the deployed-additive V88 predecessor authenticated
/// by `capacity_recovery`. V89 and V90 preserve its historical table SQL rows,
/// so V91 must admit this one deterministic descendant for startup to reach the
/// current schema without weakening its full-catalog gate.
pub(crate) const H1_V91_ACCEPTED_DEPLOYED_ADDITIVE_V90_FULL_CATALOG_FINGERPRINT: &str =
    "sha256:fe409edcd1afaa24307edb48eb88fefc48ea895ecfcb031c6c17cb2843ad8552";

/// Exact V90 descendant of the pre-Antigravity V88 predecessor.
pub(crate) const H1_V91_ACCEPTED_PRE_ANTIGRAVITY_V90_FULL_CATALOG_FINGERPRINT: &str =
    "sha256:e2fac5894220de2948aab7c8dbe6fcd3531d32b9878e982f2134eb95a7969eb6";

/// Exact complete V90 catalogs for the fresh-history, legacy-V0,
/// deployed-additive, and pre-Antigravity current histories. The four V88 pins
/// remain owned and unchanged by the V89 migration; V91 authenticates their
/// exact V90 descendants.
const H1_V91_ACCEPTED_V90_FULL_CATALOG_FINGERPRINTS: [&str; 4] = [
    "sha256:851c03ab005430836ed86228b7b5aa315eaaae6f26f61659803b9b28137defdd",
    "sha256:557246c025ce01c5de886b6a1127e63a35af3660ad75cd0e3e9b8e32c07fba63",
    H1_V91_ACCEPTED_DEPLOYED_ADDITIVE_V90_FULL_CATALOG_FINGERPRINT,
    H1_V91_ACCEPTED_PRE_ANTIGRAVITY_V90_FULL_CATALOG_FINGERPRINT,
];

/// Exact complete V92 catalogs descended from the four authenticated V90/V91
/// histories. V92 is additive, so each accepted predecessor has one and only
/// one valid successor-ledger catalog.
pub(crate) const H1_V92_ACCEPTED_PRE_ANTIGRAVITY_FULL_CATALOG_FINGERPRINT: &str =
    "sha256:4d8b315e9d4c8df87bf78be4ace38f80a2144ec55b0016621259c9f268039b10";

const H1_V92_ACCEPTED_FULL_CATALOG_FINGERPRINTS: [&str; 4] = [
    "sha256:28deba51f4b96c06117c758c526e6ec88275b0f67fddf975d00e3185f9f55960",
    "sha256:5e2fafe31a0d1678aa8b50536faae8075c70a69c2f30cc42f0a3c0eeff5c5dc1",
    "sha256:c38b1591894bcc242eb5cda7413b6e0b52128595464a261a881a74019998b162",
    H1_V92_ACCEPTED_PRE_ANTIGRAVITY_FULL_CATALOG_FINGERPRINT,
];

// RSI-RELEASED-MIGRATION-BEGIN: v94-v95-full-catalog-fingerprints
/// Exact normalized V93 descendants of the four authenticated V88 histories.
/// The fresh and pre-Antigravity lineages converge after the V93 recursive-live
/// rebuild; duplicate pins keep the lineage audit explicit.
pub(crate) const H1_V94_ACCEPTED_FRESH_V93_FULL_CATALOG_FINGERPRINT: &str =
    "sha256:d05feed969be66415f54e973396658ed2b1a34f177eacac75cbfa7daf5138674";
pub(crate) const H1_V94_ACCEPTED_LEGACY_V0_V93_FULL_CATALOG_FINGERPRINT: &str =
    "sha256:6071b07279d4589f803fae5fb94340658c2c4cefec33e953b9e1e97281b379d6";
pub(crate) const H1_V94_ACCEPTED_DEPLOYED_ADDITIVE_V93_FULL_CATALOG_FINGERPRINT: &str =
    "sha256:139bb31e3f2974aaddbd5291de40b9e908ccd1e84ab46d1882e29d844bd3a1d9";
pub(crate) const H1_V94_ACCEPTED_PRE_ANTIGRAVITY_V93_FULL_CATALOG_FINGERPRINT: &str =
    H1_V94_ACCEPTED_FRESH_V93_FULL_CATALOG_FINGERPRINT;

const H1_V94_ACCEPTED_V93_FULL_CATALOG_FINGERPRINTS: [&str; 4] = [
    H1_V94_ACCEPTED_FRESH_V93_FULL_CATALOG_FINGERPRINT,
    H1_V94_ACCEPTED_LEGACY_V0_V93_FULL_CATALOG_FINGERPRINT,
    H1_V94_ACCEPTED_DEPLOYED_ADDITIVE_V93_FULL_CATALOG_FINGERPRINT,
    H1_V94_ACCEPTED_PRE_ANTIGRAVITY_V93_FULL_CATALOG_FINGERPRINT,
];

pub(crate) const H1_V95_ACCEPTED_FRESH_V94_FULL_CATALOG_FINGERPRINT: &str =
    "sha256:24f5d622e51c3f8900d41ccb8d30a958071c1097a5b8291ba99a986247c793a9";
pub(crate) const H1_V95_ACCEPTED_LEGACY_V0_V94_FULL_CATALOG_FINGERPRINT: &str =
    "sha256:2cd1f912eb1d9fe456ec58bba1c895889de463cfa7d6b052054de53150d6efc9";
pub(crate) const H1_V95_ACCEPTED_DEPLOYED_ADDITIVE_V94_FULL_CATALOG_FINGERPRINT: &str =
    "sha256:23d3b9dbaf876dc69a4377795ee3b8a4a6ae2fd5be993166f0bea4d2dbaf69f8";
pub(crate) const H1_V95_ACCEPTED_PRE_ANTIGRAVITY_V94_FULL_CATALOG_FINGERPRINT: &str =
    H1_V95_ACCEPTED_FRESH_V94_FULL_CATALOG_FINGERPRINT;

const H1_V95_ACCEPTED_V94_FULL_CATALOG_FINGERPRINTS: [&str; 4] = [
    H1_V95_ACCEPTED_FRESH_V94_FULL_CATALOG_FINGERPRINT,
    H1_V95_ACCEPTED_LEGACY_V0_V94_FULL_CATALOG_FINGERPRINT,
    H1_V95_ACCEPTED_DEPLOYED_ADDITIVE_V94_FULL_CATALOG_FINGERPRINT,
    H1_V95_ACCEPTED_PRE_ANTIGRAVITY_V94_FULL_CATALOG_FINGERPRINT,
];

/// Exact complete deployed-V94 catalogs for the four accepted historical
/// lineages. The original released V94 settlement projection differs from the
/// corrected static V94 projection, so it must be authenticated as its own
/// predecessor before the bridge replaces either settlement table.
const H1_V95_ACCEPTED_DEPLOYED_V94_FULL_CATALOG_FINGERPRINTS: [&str; 4] = [
    "sha256:0ecbcb1b530693a801d25b8d85b1e2e022d981771e031eefa780c1aec081b83d",
    "sha256:cd819bb709acd16c24bd9db7fd57a62860fffd57665906225d73d98aff93475c",
    "sha256:1861147c8ad706212240c7bef0a3af9e26acb95e55c6624e0646c184a1fcb0b4",
    "sha256:0ecbcb1b530693a801d25b8d85b1e2e022d981771e031eefa780c1aec081b83d",
];
// RSI-RELEASED-MIGRATION-END: v94-v95-full-catalog-fingerprints

/// Ordered atomic boundaries of the V90 retained-projection rebuild.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum V90MigrationFault {
    AfterPreflight,
    AfterRename,
    AfterCreate,
    AfterCopy,
    AfterOldTableDrop,
    AfterIndexes,
    AfterTriggers,
    AfterChecks,
    AfterUserVersion,
    BeforeCommit,
}

impl V90MigrationFault {
    pub(crate) const ALL: [Self; 10] = [
        Self::AfterPreflight,
        Self::AfterRename,
        Self::AfterCreate,
        Self::AfterCopy,
        Self::AfterOldTableDrop,
        Self::AfterIndexes,
        Self::AfterTriggers,
        Self::AfterChecks,
        Self::AfterUserVersion,
        Self::BeforeCommit,
    ];
}

#[cfg(test)]
thread_local! {
    static V90_MIGRATION_FAULT: RefCell<Option<V90MigrationFault>> = const { RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn v90_test_fail_next_migration(fault: V90MigrationFault) {
    V90_MIGRATION_FAULT.with(|slot| *slot.borrow_mut() = Some(fault));
}

#[cfg(test)]
fn v90_migration_fault(fault: V90MigrationFault) -> Result<()> {
    let injected = V90_MIGRATION_FAULT.with(|slot| {
        if *slot.borrow() == Some(fault) {
            *slot.borrow_mut() = None;
            true
        } else {
            false
        }
    });
    if injected {
        return Err(DaemonError::Store(format!(
            "injected V90 migration fault: {fault:?}"
        )));
    }
    Ok(())
}

#[cfg(not(test))]
fn v90_migration_fault(_: V90MigrationFault) -> Result<()> {
    Ok(())
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum V90CopyCorruption {
    Count,
    Bytes,
}

#[cfg(test)]
thread_local! {
    static V90_COPY_CORRUPTION: RefCell<Option<V90CopyCorruption>> = const { RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn v90_test_corrupt_next_copy(corruption: V90CopyCorruption) {
    V90_COPY_CORRUPTION.with(|slot| *slot.borrow_mut() = Some(corruption));
}

#[cfg(test)]
fn v90_corrupt_copy_for_test(tx: &Transaction<'_>) -> Result<()> {
    let corruption = V90_COPY_CORRUPTION.with(|slot| slot.borrow_mut().take());
    match corruption {
        Some(V90CopyCorruption::Count) => {
            tx.execute(
                "DELETE FROM session_execution_projections
                 WHERE session_id=(SELECT session_id FROM session_execution_projections ORDER BY session_id LIMIT 1)",
                [],
            )?;
        }
        Some(V90CopyCorruption::Bytes) => {
            tx.execute(
                "UPDATE session_execution_projections
                 SET canonical_repo_dir=canonical_repo_dir || '/v90-copy-corruption'
                 WHERE session_id=(SELECT session_id FROM session_execution_projections ORDER BY session_id LIMIT 1)",
                [],
            )?;
        }
        None => {}
    }
    Ok(())
}

#[cfg(not(test))]
fn v90_corrupt_copy_for_test(_: &Transaction<'_>) -> Result<()> {
    Ok(())
}

/// Ordered atomic boundaries of the V89 provider-capacity recovery ledger.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum V89MigrationFault {
    AfterPreflight,
    AfterTables,
    AfterIndexes,
    AfterTriggers,
    AfterChecks,
    AfterUserVersion,
    BeforeCommit,
}

impl V89MigrationFault {
    pub(crate) const ALL: [Self; 7] = [
        Self::AfterPreflight,
        Self::AfterTables,
        Self::AfterIndexes,
        Self::AfterTriggers,
        Self::AfterChecks,
        Self::AfterUserVersion,
        Self::BeforeCommit,
    ];
}

#[cfg(test)]
thread_local! {
    static V89_MIGRATION_FAULT: RefCell<Option<V89MigrationFault>> = const { RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn v89_test_fail_next_migration(fault: V89MigrationFault) {
    V89_MIGRATION_FAULT.with(|slot| *slot.borrow_mut() = Some(fault));
}

#[cfg(test)]
fn v89_migration_fault(fault: V89MigrationFault) -> Result<()> {
    let injected = V89_MIGRATION_FAULT.with(|slot| {
        if *slot.borrow() == Some(fault) {
            *slot.borrow_mut() = None;
            true
        } else {
            false
        }
    });
    if injected {
        return Err(DaemonError::Store(format!(
            "injected V89 migration fault: {fault:?}"
        )));
    }
    Ok(())
}

#[cfg(not(test))]
fn v89_migration_fault(_: V89MigrationFault) -> Result<()> {
    Ok(())
}

/// Ordered atomic boundaries of the private V85 catalog upgrade.  This is
/// test-only: production always uses one immediate SQLite transaction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum H1V85MigrationFault {
    AfterPreflight,
    AfterTables,
    AfterBackfill,
    AfterIndexes,
    AfterTriggers,
    AfterForeignKeyCheck,
    AfterUserVersion,
    BeforeCommit,
}

impl H1V85MigrationFault {
    pub(crate) const ALL: [Self; 8] = [
        Self::AfterPreflight,
        Self::AfterTables,
        Self::AfterBackfill,
        Self::AfterIndexes,
        Self::AfterTriggers,
        Self::AfterForeignKeyCheck,
        Self::AfterUserVersion,
        Self::BeforeCommit,
    ];
}

#[cfg(test)]
thread_local! {
    static H1_V85_MIGRATION_FAULT: RefCell<Option<H1V85MigrationFault>> = const { RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn h1_v85_test_fail_next_migration(fault: H1V85MigrationFault) {
    H1_V85_MIGRATION_FAULT.with(|slot| *slot.borrow_mut() = Some(fault));
}

#[cfg(test)]
fn h1_v85_migration_fault(fault: H1V85MigrationFault) -> Result<()> {
    let injected = H1_V85_MIGRATION_FAULT.with(|slot| {
        if *slot.borrow() == Some(fault) {
            *slot.borrow_mut() = None;
            true
        } else {
            false
        }
    });
    if injected {
        return Err(DaemonError::Store(format!(
            "injected V85 migration fault: {fault:?}"
        )));
    }
    Ok(())
}

#[cfg(not(test))]
fn h1_v85_migration_fault(_: H1V85MigrationFault) -> Result<()> {
    Ok(())
}

/// Ordered atomic boundaries of the V86 request-key provenance rebuild.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum H1V86MigrationFault {
    AfterPreflight,
    AfterRename,
    AfterCopy,
    AfterIndexes,
    AfterTriggers,
    AfterForeignKeyCheck,
    AfterUserVersion,
    BeforeCommit,
}

impl H1V86MigrationFault {
    pub(crate) const ALL: [Self; 8] = [
        Self::AfterPreflight,
        Self::AfterRename,
        Self::AfterCopy,
        Self::AfterIndexes,
        Self::AfterTriggers,
        Self::AfterForeignKeyCheck,
        Self::AfterUserVersion,
        Self::BeforeCommit,
    ];
}

#[cfg(test)]
thread_local! {
    static H1_V86_MIGRATION_FAULT: RefCell<Option<H1V86MigrationFault>> = const { RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn h1_v86_test_fail_next_migration(fault: H1V86MigrationFault) {
    H1_V86_MIGRATION_FAULT.with(|slot| *slot.borrow_mut() = Some(fault));
}

#[cfg(test)]
fn h1_v86_migration_fault(fault: H1V86MigrationFault) -> Result<()> {
    let injected = H1_V86_MIGRATION_FAULT.with(|slot| {
        if *slot.borrow() == Some(fault) {
            *slot.borrow_mut() = None;
            true
        } else {
            false
        }
    });
    if injected {
        return Err(DaemonError::Store(format!(
            "injected V86 migration fault: {fault:?}"
        )));
    }
    Ok(())
}

#[cfg(not(test))]
fn h1_v86_migration_fault(_: H1V86MigrationFault) -> Result<()> {
    Ok(())
}

/// Ordered atomic boundaries of the V87 Session-fence trigger replacement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum H1V87MigrationFault {
    AfterPreflight,
    AfterDrop,
    AfterCreate,
    AfterChecks,
    AfterUserVersion,
    BeforeCommit,
}

impl H1V87MigrationFault {
    pub(crate) const ALL: [Self; 6] = [
        Self::AfterPreflight,
        Self::AfterDrop,
        Self::AfterCreate,
        Self::AfterChecks,
        Self::AfterUserVersion,
        Self::BeforeCommit,
    ];
}

#[cfg(test)]
thread_local! {
    static H1_V87_MIGRATION_FAULT: RefCell<Option<H1V87MigrationFault>> = const { RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn h1_v87_test_fail_next_migration(fault: H1V87MigrationFault) {
    H1_V87_MIGRATION_FAULT.with(|slot| *slot.borrow_mut() = Some(fault));
}

#[cfg(test)]
fn h1_v87_migration_fault(fault: H1V87MigrationFault) -> Result<()> {
    let injected = H1_V87_MIGRATION_FAULT.with(|slot| {
        if *slot.borrow() == Some(fault) {
            *slot.borrow_mut() = None;
            true
        } else {
            false
        }
    });
    if injected {
        return Err(DaemonError::Store(format!(
            "injected V87 migration fault: {fault:?}"
        )));
    }
    Ok(())
}

#[cfg(not(test))]
fn h1_v87_migration_fault(_: H1V87MigrationFault) -> Result<()> {
    Ok(())
}

/// Ordered atomic boundaries of the V88 static state-machine rebuild.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum H1V88MigrationFault {
    AfterPreflight,
    AfterRename,
    AfterCopy,
    AfterIndexes,
    AfterTriggers,
    AfterChecks,
    AfterUserVersion,
    BeforeCommit,
}

impl H1V88MigrationFault {
    pub(crate) const ALL: [Self; 8] = [
        Self::AfterPreflight,
        Self::AfterRename,
        Self::AfterCopy,
        Self::AfterIndexes,
        Self::AfterTriggers,
        Self::AfterChecks,
        Self::AfterUserVersion,
        Self::BeforeCommit,
    ];
}

#[cfg(test)]
thread_local! {
    static H1_V88_MIGRATION_FAULT: RefCell<Option<H1V88MigrationFault>> = const { RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn h1_v88_test_fail_next_migration(fault: H1V88MigrationFault) {
    H1_V88_MIGRATION_FAULT.with(|slot| *slot.borrow_mut() = Some(fault));
}

#[cfg(test)]
pub(crate) fn h1_v88_test_clear_migration_fault() {
    H1_V88_MIGRATION_FAULT.with(|slot| *slot.borrow_mut() = None);
}

#[cfg(test)]
fn h1_v88_migration_fault(fault: H1V88MigrationFault) -> Result<()> {
    let injected = H1_V88_MIGRATION_FAULT.with(|slot| {
        if *slot.borrow() == Some(fault) {
            *slot.borrow_mut() = None;
            true
        } else {
            false
        }
    });
    if injected {
        return Err(DaemonError::Store(format!(
            "injected V88 migration fault: {fault:?}"
        )));
    }
    Ok(())
}

#[cfg(not(test))]
fn h1_v88_migration_fault(_: H1V88MigrationFault) -> Result<()> {
    Ok(())
}

/// Compare the V76 Issue catalog semantically, rather than trusting only the
/// column list. `SQLite` preserves harmless formatting and comments in
/// `sqlite_master`, so normalize those away while retaining every table
/// constraint and all three named legacy indexes.
fn d04_normalize_catalog_sql(sql: &str) -> String {
    sql.lines()
        .map(|line| line.split_once("--").map_or(line, |(before, _)| before))
        .collect::<Vec<_>>()
        .join(" ")
        .replace('"', "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

/// Authenticate the V96 conversation-correlation tail before V97 mutates the
/// retained Issue projection. The source version alone is not catalog proof.
fn validate_v96_tool_correlation_catalog(tx: &Transaction<'_>) -> Result<()> {
    let columns: i64 = tx.query_row(
        "SELECT count(*) FROM pragma_table_info('conversation_events')
         WHERE name IN ('tool_use_id','metadata')
           AND upper(type)='TEXT' AND \"notnull\"=0 AND dflt_value IS NULL AND pk=0",
        [],
        |row| row.get(0),
    )?;
    if columns != 2 {
        return Err(DaemonError::Store(
            "V97 requires the exact V96 conversation-event correlation columns".to_string(),
        ));
    }
    let actual_index: Option<String> = tx
        .query_row(
            "SELECT sql FROM sqlite_master
             WHERE type='index' AND name='idx_events_tool_use_id'
               AND tbl_name='conversation_events'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let expected_index = "CREATE INDEX idx_events_tool_use_id ON conversation_events(tool_use_id)";
    if actual_index.as_deref().map(d04_normalize_catalog_sql)
        != Some(d04_normalize_catalog_sql(expected_index))
    {
        return Err(DaemonError::Store(
            "V97 requires the exact V96 conversation-event correlation index".to_string(),
        ));
    }
    Ok(())
}

// RSI-RELEASED-MIGRATION-BEGIN: v112-dual-v111-catalog-authenticators
const V112_CURRENT_TARGET_V111_FULL_CATALOG_FINGERPRINT: &str =
    "sha256:4bf2963212bdae91d267304e3d6df56ca545e68d9e93f65e74b0cd2fbc0f50ea";
const V112_CURRENT_LEGACY_V0_V111_FULL_CATALOG_FINGERPRINT: &str =
    "sha256:c52211673e05cd35d9d91e02acdebbd93bc459280210502248e5259098682195";
const V112_CURRENT_DEPLOYED_ADDITIVE_V111_FULL_CATALOG_FINGERPRINT: &str =
    "sha256:623ee44d1a42aaa0d5c221ae579865f13de516f05e6b8ccca30bd940423ab794";
const V112_HISTORICAL_OPERATOR_VIEWS_V111_FULL_CATALOG_FINGERPRINT: &str =
    "sha256:a2a609d8ccc191cfc034ec49151900024db182fdb15f0101df3e66f0efaec4a4";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum V112SourceCatalog {
    CurrentTargetV111,
    HistoricalOperatorViewsV111,
}

fn classify_v112_source_catalog(tx: &Transaction<'_>) -> Result<V112SourceCatalog> {
    let actual = capacity_recovery::v88_full_catalog_fingerprint(tx)?;
    match actual.as_str() {
        V112_CURRENT_TARGET_V111_FULL_CATALOG_FINGERPRINT
        | V112_CURRENT_LEGACY_V0_V111_FULL_CATALOG_FINGERPRINT
        | V112_CURRENT_DEPLOYED_ADDITIVE_V111_FULL_CATALOG_FINGERPRINT => {
            Ok(V112SourceCatalog::CurrentTargetV111)
        }
        V112_HISTORICAL_OPERATOR_VIEWS_V111_FULL_CATALOG_FINGERPRINT => {
            Ok(V112SourceCatalog::HistoricalOperatorViewsV111)
        }
        _ => Err(DaemonError::Store(format!(
            "V112 requires one exact authenticated V111 source catalog, found {actual}"
        ))),
    }
}

fn install_v112_capability_columns(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "ALTER TABLE sessions ADD COLUMN context_window_source TEXT
                 CHECK (context_window_source IS NULL OR context_window_source IN (
                     'official_documentation', 'provider_catalog', 'configured',
                     'runtime_telemetry', 'repository_fallback', 'legacy_unverified'
                 ));
         ALTER TABLE sessions ADD COLUMN context_window_source_version TEXT;
         ALTER TABLE sessions ADD COLUMN context_window_source_digest TEXT;
         ALTER TABLE sessions ADD COLUMN context_window_observed_at TEXT;
         UPDATE sessions
            SET context_window_source='legacy_unverified'
          WHERE context_window IS NOT NULL;
         ALTER TABLE sessions ADD COLUMN context_window_configured_tokens INTEGER
                 CHECK (context_window_configured_tokens IS NULL OR
                        context_window_configured_tokens > 0);",
    )?;
    Ok(())
}
// RSI-RELEASED-MIGRATION-END: v112-dual-v111-catalog-authenticators

const SESSION_EXECUTION_PROJECTION_V89_TABLE_SQL: &str = "
                CREATE TABLE session_execution_projections (
                    session_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE RESTRICT,
                    schema_version INTEGER NOT NULL CHECK (schema_version=1),
                    projection_version INTEGER NOT NULL CHECK (projection_version=1),
                    execution_state TEXT NOT NULL CHECK (execution_state IN ('ordinary_unsandboxed','live_sandboxed','historical_purged','historical_transferred','historical_cleanup_failed','quarantined','invalid')),
                    freshness TEXT NOT NULL CHECK (freshness IN ('verified','unverified','invalid')),
                    canonical_repo_dir TEXT NOT NULL,
                    effective_cwd TEXT,
                    custody_id TEXT REFERENCES sandbox_custody_roots(custody_id) ON DELETE RESTRICT,
                    custody_generation INTEGER,
                    validated_at TEXT,
                    error_code TEXT,
                    updated_at TEXT NOT NULL,
                    CHECK ((effective_cwd IS NOT NULL AND freshness='verified' AND execution_state IN ('ordinary_unsandboxed','live_sandboxed'))
                        OR (effective_cwd IS NULL AND NOT (freshness='verified' AND execution_state IN ('ordinary_unsandboxed','live_sandboxed')))),
                    CHECK ((freshness='verified' AND validated_at IS NOT NULL AND error_code IS NULL)
                        OR (freshness='unverified' AND validated_at IS NULL AND error_code IS NULL AND effective_cwd IS NULL)
                        OR (freshness='invalid' AND validated_at IS NOT NULL AND error_code IS NOT NULL AND effective_cwd IS NULL))
                )";

const SESSION_EXECUTION_PROJECTION_V90_TABLE_SQL: &str = "
    CREATE TABLE session_execution_projections (
        session_id TEXT PRIMARY KEY,
        schema_version INTEGER NOT NULL CHECK (schema_version=1),
        projection_version INTEGER NOT NULL CHECK (projection_version=1),
        execution_state TEXT NOT NULL CHECK (execution_state IN ('ordinary_unsandboxed','live_sandboxed','historical_purged','historical_transferred','historical_cleanup_failed','quarantined','invalid')),
        freshness TEXT NOT NULL CHECK (freshness IN ('verified','unverified','invalid')),
        canonical_repo_dir TEXT NOT NULL,
        effective_cwd TEXT,
        custody_id TEXT REFERENCES sandbox_custody_roots(custody_id) ON DELETE RESTRICT,
        custody_generation INTEGER,
        validated_at TEXT,
        error_code TEXT,
        updated_at TEXT NOT NULL,
        CHECK ((effective_cwd IS NOT NULL AND freshness='verified' AND execution_state IN ('ordinary_unsandboxed','live_sandboxed'))
            OR (effective_cwd IS NULL AND NOT (freshness='verified' AND execution_state IN ('ordinary_unsandboxed','live_sandboxed')))),
        CHECK ((freshness='verified' AND validated_at IS NOT NULL AND error_code IS NULL)
            OR (freshness='unverified' AND validated_at IS NULL AND error_code IS NULL AND effective_cwd IS NULL)
            OR (freshness='invalid' AND validated_at IS NOT NULL AND error_code IS NOT NULL AND effective_cwd IS NULL))
    )";

const SESSION_EXECUTION_PROJECTION_INDEX_SQL: &str = "CREATE INDEX idx_session_execution_projections_custody ON session_execution_projections(custody_id, session_id)";
const SESSION_EXECUTION_PROJECTION_NO_DELETE_SQL: &str = "CREATE TRIGGER session_execution_projections_no_delete BEFORE DELETE ON session_execution_projections BEGIN SELECT RAISE(ABORT, 'sandbox custody projections are retained history'); END";
const SESSION_EXECUTION_PROJECTION_AFTER_INSERT_SQL: &str = "
                CREATE TRIGGER sessions_execution_projection_after_insert AFTER INSERT ON sessions BEGIN
                    INSERT INTO session_execution_projections (session_id,schema_version,projection_version,execution_state,freshness,canonical_repo_dir,effective_cwd,custody_id,custody_generation,validated_at,error_code,updated_at)
                    VALUES (NEW.id,1,1,CASE WHEN NEW.sandbox_kind IS NULL AND NEW.sandbox_root IS NULL AND NEW.sandbox_branch IS NULL AND NEW.sandbox_cleanup_state IS NULL THEN 'ordinary_unsandboxed' WHEN NEW.sandbox_cleanup_state='Purged' THEN 'historical_purged' WHEN NEW.sandbox_cleanup_state='Failed' THEN 'historical_cleanup_failed' WHEN NEW.sandbox_kind IS NOT NULL AND NEW.sandbox_root IS NOT NULL AND NEW.sandbox_branch IS NOT NULL AND NEW.sandbox_cleanup_state='Live' THEN 'live_sandboxed' ELSE 'invalid' END,'unverified',NEW.working_dir,NULL,NULL,NULL,NULL,NULL,NEW.updated_at);
                END";
const SESSION_EXECUTION_PROJECTION_PURGE_GUARD_SQL: &str = "
    CREATE TRIGGER sessions_execution_projection_purge_guard BEFORE DELETE ON sessions
    WHEN NOT EXISTS (
        SELECT 1 FROM session_execution_projections
        WHERE session_id=OLD.id
          AND execution_state IN ('historical_purged','historical_transferred')
          AND freshness='verified'
          AND effective_cwd IS NULL
          AND validated_at IS NOT NULL
          AND error_code IS NULL
    )
    BEGIN SELECT RAISE(ABORT, 'session purge requires retained terminal execution projection'); END";

type SessionExecutionProjectionForeignKey =
    (i64, i64, String, String, String, String, String, String);

fn session_execution_projection_foreign_keys(
    tx: &Transaction<'_>,
) -> rusqlite::Result<Vec<SessionExecutionProjectionForeignKey>> {
    tx.prepare("PRAGMA foreign_key_list(session_execution_projections)")?
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
            ))
        })?
        .collect()
}

fn session_execution_projection_catalog_matches(
    tx: &Transaction<'_>,
    detached: bool,
) -> rusqlite::Result<bool> {
    let actual = tx
        .prepare(
            "SELECT type,name,sql FROM sqlite_master
             WHERE sql IS NOT NULL AND (
                 (type='table' AND name='session_execution_projections')
                 OR (type='index' AND tbl_name='session_execution_projections')
                 OR (type='trigger' AND instr(lower(sql),'session_execution_projections')>0)
             )
             ORDER BY type,name",
        )?
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                d04_normalize_catalog_sql(&row.get::<_, String>(2)?),
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut expected = vec![
        (
            "index".to_string(),
            "idx_session_execution_projections_custody".to_string(),
            d04_normalize_catalog_sql(SESSION_EXECUTION_PROJECTION_INDEX_SQL),
        ),
        (
            "table".to_string(),
            "session_execution_projections".to_string(),
            d04_normalize_catalog_sql(if detached {
                SESSION_EXECUTION_PROJECTION_V90_TABLE_SQL
            } else {
                SESSION_EXECUTION_PROJECTION_V89_TABLE_SQL
            }),
        ),
        (
            "trigger".to_string(),
            "session_execution_projections_no_delete".to_string(),
            d04_normalize_catalog_sql(SESSION_EXECUTION_PROJECTION_NO_DELETE_SQL),
        ),
        (
            "trigger".to_string(),
            "sessions_execution_projection_after_insert".to_string(),
            d04_normalize_catalog_sql(SESSION_EXECUTION_PROJECTION_AFTER_INSERT_SQL),
        ),
    ];
    if detached {
        expected.push((
            "trigger".to_string(),
            "sessions_execution_projection_purge_guard".to_string(),
            d04_normalize_catalog_sql(SESSION_EXECUTION_PROJECTION_PURGE_GUARD_SQL),
        ));
    }
    expected.sort();
    Ok(actual == expected)
}

fn session_execution_projection_foreign_keys_match(
    tx: &Transaction<'_>,
    detached: bool,
) -> rusqlite::Result<bool> {
    let expected = if detached {
        vec![(
            0,
            0,
            "sandbox_custody_roots".to_string(),
            "custody_id".to_string(),
            "custody_id".to_string(),
            "NO ACTION".to_string(),
            "RESTRICT".to_string(),
            "NONE".to_string(),
        )]
    } else {
        vec![
            (
                0,
                0,
                "sandbox_custody_roots".to_string(),
                "custody_id".to_string(),
                "custody_id".to_string(),
                "NO ACTION".to_string(),
                "RESTRICT".to_string(),
                "NONE".to_string(),
            ),
            (
                1,
                0,
                "sessions".to_string(),
                "session_id".to_string(),
                "id".to_string(),
                "NO ACTION".to_string(),
                "RESTRICT".to_string(),
                "NONE".to_string(),
            ),
        ]
    };
    Ok(session_execution_projection_foreign_keys(tx)? == expected)
}

fn h1_v91_validate_v90_catalog(tx: &Transaction<'_>) -> Result<()> {
    let actual = capacity_recovery::v88_full_catalog_fingerprint(tx)?;
    if !H1_V91_ACCEPTED_V90_FULL_CATALOG_FINGERPRINTS.contains(&actual.as_str()) {
        return Err(DaemonError::Store(format!(
            "V91 requires exact accepted V90 source catalog, found {actual}"
        )));
    }
    if !session_execution_projection_catalog_matches(tx, true)?
        || !session_execution_projection_foreign_keys_match(tx, true)?
    {
        return Err(DaemonError::Store(
            "V91 requires exact V90 detached execution-projection catalog".into(),
        ));
    }
    Ok(())
}

fn h1_v92_validate_catalog(tx: &Transaction<'_>) -> Result<()> {
    let actual = capacity_recovery::v88_full_catalog_fingerprint(tx)?;
    if !H1_V92_ACCEPTED_FULL_CATALOG_FINGERPRINTS.contains(&actual.as_str()) {
        return Err(DaemonError::Store(format!(
            "V92 requires exact accepted full catalog, found {actual}"
        )));
    }
    Ok(())
}

// RSI-RELEASED-MIGRATION-BEGIN: v94-v95-catalog-authenticators
fn h1_v94_validate_v93_catalog(tx: &Transaction<'_>) -> Result<()> {
    let actual = capacity_recovery::v88_full_catalog_fingerprint(tx)?;
    if !H1_V94_ACCEPTED_V93_FULL_CATALOG_FINGERPRINTS.contains(&actual.as_str()) {
        return Err(DaemonError::Store(format!(
            "V94 requires exact accepted normalized V93 source catalog, found {actual}"
        )));
    }
    Ok(())
}

fn h1_v95_validate_v94_catalog(
    tx: &Transaction<'_>,
) -> Result<cohort_settlement::V94SettlementCatalog> {
    let catalog = cohort_settlement::classify_v94_settlement_catalog(tx)?;
    let actual = capacity_recovery::v88_full_catalog_fingerprint(tx)?;
    let accepted = match catalog {
        cohort_settlement::V94SettlementCatalog::Current => {
            H1_V95_ACCEPTED_V94_FULL_CATALOG_FINGERPRINTS.as_slice()
        }
        cohort_settlement::V94SettlementCatalog::Deployed => {
            H1_V95_ACCEPTED_DEPLOYED_V94_FULL_CATALOG_FINGERPRINTS.as_slice()
        }
    };
    if !accepted.contains(&actual.as_str()) {
        let source_label = match catalog {
            cohort_settlement::V94SettlementCatalog::Current => "V94",
            cohort_settlement::V94SettlementCatalog::Deployed => "deployed V94",
        };
        return Err(DaemonError::Store(format!(
            "V95 requires exact accepted {source_label} source catalog, found {actual}"
        )));
    }
    Ok(catalog)
}
// RSI-RELEASED-MIGRATION-END: v94-v95-catalog-authenticators

/// Reject any active V87/V90 execution-origin predecessor that cannot execute
/// its next unchanged Session-fence statement. The query intentionally uses no
/// V88-added columns so the same predicate runs before historical V88 DDL and
/// again over deployed V90 data in V91.
fn h1_validate_exact_active_predecessors(
    tx: &Transaction<'_>,
    target_version: i32,
    source_version: i32,
) -> Result<()> {
    let incoherent_active_predecessors: i64 = tx.query_row(
        "SELECT count(*)
         FROM execution_origin_authorities a
         WHERE a.active_claim_id IS NOT NULL
           AND NOT EXISTS (
             SELECT 1
             FROM execution_origin_claims c
             JOIN sessions s ON s.id=a.owner_session_id
             JOIN execution_origin_events e
               ON e.authority_kind=a.authority_kind
              AND e.authority_uuid=a.authority_uuid
              AND e.sequence=a.event_sequence
             WHERE c.claim_id=a.active_claim_id
               AND c.authority_kind=a.authority_kind
               AND c.authority_uuid=a.authority_uuid
               AND c.claimant_session_id=a.owner_session_id
               AND c.expected_owner_generation
                   + CASE WHEN c.claimant_kind IN ('agent_fresh','rotation','automatic_retry') THEN 1 ELSE 0 END
                   = a.owner_generation
               AND c.expected_claim_generation+1=a.claim_generation
               AND c.boot_id=a.boot_id
               AND a.phase=CASE
                   WHEN c.phase='recovery_quarantined' THEN 'quarantined'
                   WHEN c.phase IN ('settled','failed','abandoned','quarantined') THEN 'settling'
                   ELSE c.phase
               END
               AND c.phase IN ('claimed','launch_ready','launching','provider_live','recovery_quarantined','settling','settled','failed','abandoned','quarantined')
               AND (
                 (c.phase IN ('claimed','launch_ready','launching')
                  AND c.provider_create_state='not_attempted'
                  AND c.observed_cell_phase IS NULL
                  AND c.absence_evidence IS NULL)
                 OR
                 (c.phase='provider_live'
                  AND c.provider_create_state='created'
                  AND c.observed_cell_phase IN ('configured_unpublished','published')
                  AND c.absence_evidence IS NULL)
                 OR
                 (c.phase='recovery_quarantined'
                  AND c.provider_create_state IN ('created','create_unknown')
                  AND c.observed_cell_phase IN ('cleanup_requested','quarantined','terminal')
                  AND (c.absence_evidence IS NULL OR c.absence_evidence IN ('immediate_exit','cli_reaped','task_joined','foreign_boot_task_absent')))
                 OR
                 (c.phase IN ('settling','settled','failed','abandoned','quarantined') AND (
                   (c.provider_create_state='no_create'
                    AND c.observed_cell_phase IS NULL
                    AND c.absence_evidence='no_create')
                   OR
                   (c.provider_create_state IN ('created','create_unknown')
                    AND c.observed_cell_phase='terminal'
                    AND c.absence_evidence IN ('immediate_exit','cli_reaped','task_joined','foreign_boot_task_absent'))
                 ))
               )
               AND c.authorized_session_status IS NOT NULL
               AND c.authorized_session_write_seq IS NOT NULL
               AND c.authorized_session_status IN ('Starting','Running','WaitingApproval')
               AND (c.phase NOT IN ('claimed','launch_ready','launching') OR c.authorized_session_status='Starting')
               AND rsi_execution_origin_controller_is_canonical(
                   c.claimant_kind,c.source_session_id,c.claimant_session_id,
                   c.source_model_invocation_id,c.rotation_action_digest,
                   c.retry_attempt,c.c5_marker_digest,c.controller_project_id,
                   c.controller_idea_id,c.controller_transfer_key,
                   c.controller_reservation_id,c.controller_candidate_session_id,
                   c.controller_base_row_id,c.controller_base_event_id,
                   c.controller_expires_at)=1
               AND (
                 (c.phase IN ('claimed','launch_ready','launching','provider_live','recovery_quarantined')
                  AND c.prepared_terminal_status IS NULL
                  AND c.prepared_stop_reason IS NULL
                  AND c.prepared_session_write_seq IS NULL
                  AND c.prepared_provider_evidence IS NULL
                  AND c.terminal_model_invocation_id IS NULL)
                 OR
                 (c.phase IN ('settling','settled','failed','abandoned','quarantined')
                  AND c.prepared_terminal_status IS NOT NULL
                  AND c.prepared_stop_reason IS NOT NULL
                  AND c.prepared_session_write_seq=c.authorized_session_write_seq+1
                  AND c.prepared_provider_evidence=c.absence_evidence
                  AND c.terminal_model_invocation_id IS NOT NULL
                  AND (
                    c.phase='settling'
                    OR (c.phase='settled' AND c.prepared_terminal_status='Completed')
                    OR (c.phase='failed' AND c.prepared_terminal_status='Failed')
                    OR (c.phase IN ('abandoned','quarantined') AND c.prepared_terminal_status='Interrupted')
                  ))
               )
               AND rsi_execution_origin_c5_is_canonical(
                   c.source_session_id,c.prepared_terminal_status,
                   c.prepared_c5_cause,c.prepared_c5_key,c.prepared_c5_value,
                   c.prepared_c5_at,c.prepared_c5_digest,
                   c.prepared_c5_expected_retry_count,c.prepared_c5_max_retries)=1
               AND e.claim_id=c.claim_id
               AND e.to_phase=a.phase
               AND e.to_owner_generation=a.owner_generation
               AND e.provider_absence_evidence IS c.absence_evidence
               AND e.event_kind=CASE a.phase
                   WHEN 'claimed' THEN 'claimed'
                   WHEN 'launch_ready' THEN 'launch_ready'
                   WHEN 'launching' THEN 'launching'
                   WHEN 'provider_live' THEN 'provider_live'
                   WHEN 'quarantined' THEN 'quarantined'
                   WHEN 'settling' THEN 'settlement_prepared'
               END
               AND e.from_owner_generation=CASE
                   WHEN a.phase='claimed' THEN c.expected_owner_generation
                   ELSE a.owner_generation
               END
               AND (
                 (a.phase='claimed' AND e.from_phase='idle')
                 OR (a.phase='launch_ready' AND e.from_phase='claimed')
                 OR (a.phase='launching' AND e.from_phase='launch_ready')
                 OR (a.phase='provider_live' AND e.from_phase='launching')
                 OR (a.phase='quarantined' AND e.from_phase IN ('claimed','launch_ready','launching','provider_live'))
                 OR (a.phase='settling' AND e.from_phase IN ('claimed','launch_ready','launching','provider_live','quarantined'))
               )
               AND (
                 (s.execution_origin_claim_id IS NULL
                  AND s.status='Starting'
                  AND c.phase='claimed'
                  AND c.provider_create_state='not_attempted'
                  AND c.observed_cell_phase IS NULL
                  AND c.absence_evidence IS NULL
                  AND e.provider_absence_evidence IS NULL
                  AND c.authorized_session_status='Starting'
                  AND c.authorized_session_write_seq=s.execution_origin_write_seq+1)
                 OR
                 (s.execution_origin_claim_id=c.claim_id AND (
                   (s.status=c.authorized_session_status
                    AND s.execution_origin_write_seq=c.authorized_session_write_seq)
                   OR
                   (c.phase='provider_live'
                    AND c.authorized_session_status IN ('Running','WaitingApproval')
                    AND c.authorized_session_write_seq=s.execution_origin_write_seq+1
                    AND s.status IN ('Starting','Running','WaitingApproval'))
                 ))
               )
               AND (
                 c.phase NOT IN ('settling','settled','failed','abandoned','quarantined')
                 OR EXISTS (
                   SELECT 1 FROM model_invocations terminal
                   WHERE terminal.id=c.terminal_model_invocation_id
                     AND terminal.session_id=s.id
                     AND terminal.status IN ('completed','failed','cancelled','denied')
                 )
               )
               AND (c.prepared_c5_key IS NULL OR EXISTS (
                 SELECT 1 FROM daemon_settings ds
                 WHERE ds.key=c.prepared_c5_key
                   AND ds.value=c.prepared_c5_value
                   AND ds.updated_at=c.prepared_c5_at
               ))
               AND (
                 (a.authority_kind='ordinary'
                  AND s.sandbox_custody_id IS NULL
                  AND s.sandbox_kind IS NULL
                  AND s.sandbox_root IS NULL
                  AND s.sandbox_branch IS NULL
                  AND s.sandbox_cleanup_state IS NULL)
                 OR
                 (a.authority_kind='sandbox'
                  AND s.sandbox_custody_id=a.authority_uuid
                  AND s.sandbox_kind='GitWorktree'
                  AND s.sandbox_cleanup_state='Live'
                  AND EXISTS (
                    SELECT 1 FROM sandbox_custody_roots root
                    WHERE root.custody_id=a.authority_uuid
                      AND root.owner_session_id=s.id
                      AND root.sandbox_root=s.sandbox_root
                      AND root.sandbox_branch=s.sandbox_branch
                      AND root.generation=a.owner_generation
                      AND root.state='live'
                      AND root.validation_state='verified'
                      AND root.validated_generation=root.generation
                      AND root.effect_boot_id=c.boot_id
                      AND root.reserved_effects=0
                      AND root.active_effects=1
                  ))
               )
           )",
        [],
        |row| row.get(0),
    )?;
    if incoherent_active_predecessors != 0 {
        return Err(DaemonError::Store(format!(
            "V{target_version} cannot classify {incoherent_active_predecessors} active V{source_version} authority/claim binding(s)"
        )));
    }

    // A row admitted by the old V88 classifier may already have finalized by
    // the time a deployed V90 database opens. Refuse that terminal-pending
    // shape unless the unchanged V87 unbind's invocation/C5 facts now exist.
    let incoherent_terminal_pending: i64 = tx.query_row(
        "SELECT count(*)
         FROM sessions s
         JOIN execution_origin_claims c ON c.claim_id=s.execution_origin_claim_id
         JOIN execution_origin_authorities a
           ON a.authority_kind=c.authority_kind AND a.authority_uuid=c.authority_uuid
         WHERE c.claimant_session_id=s.id
           AND c.phase IN ('settled','failed','abandoned','quarantined')
           AND a.phase='idle'
           AND a.active_claim_id IS NULL
           AND (
             NOT EXISTS (
               SELECT 1 FROM model_invocations terminal
               WHERE terminal.id=c.terminal_model_invocation_id
                 AND terminal.session_id=s.id
                 AND terminal.status IN ('completed','failed','cancelled','denied')
             )
             OR
             (c.prepared_c5_key IS NOT NULL AND NOT EXISTS (
               SELECT 1 FROM daemon_settings ds
               WHERE ds.key=c.prepared_c5_key
                 AND ds.value=c.prepared_c5_value
                 AND ds.updated_at=c.prepared_c5_at
             ))
           )",
        [],
        |row| row.get(0),
    )?;
    if incoherent_terminal_pending != 0 {
        return Err(DaemonError::Store(format!(
            "V{target_version} cannot classify {incoherent_terminal_pending} terminal-pending V{source_version} Session binding(s)"
        )));
    }
    Ok(())
}

fn d04_v76_issue_catalog_matches(tx: &Transaction<'_>) -> rusqlite::Result<bool> {
    const TABLES: [(&str, &str); 2] = [
        (
            "issues",
            "CREATE TABLE issues (
                id TEXT PRIMARY KEY,
                display_number INTEGER NOT NULL UNIQUE,
                title TEXT NOT NULL,
                body TEXT NOT NULL DEFAULT '',
                status TEXT NOT NULL DEFAULT 'Open',
                priority INTEGER,
                labels TEXT NOT NULL DEFAULT '[]',
                created_by_session_id TEXT,
                assignee TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                closed_at TEXT
            )",
        ),
        (
            "issue_deps",
            "CREATE TABLE issue_deps (
                issue_id TEXT NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
                depends_on_id TEXT NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
                created_at TEXT NOT NULL,
                PRIMARY KEY (issue_id, depends_on_id),
                CHECK (issue_id <> depends_on_id)
            )",
        ),
    ];
    const INDEXES: [(&str, &str); 3] = [
        (
            "idx_issue_deps_depends_on",
            "CREATE INDEX idx_issue_deps_depends_on ON issue_deps(depends_on_id)",
        ),
        (
            "idx_issues_created_by",
            "CREATE INDEX idx_issues_created_by ON issues(created_by_session_id)",
        ),
        (
            "idx_issues_status",
            "CREATE INDEX idx_issues_status ON issues(status)",
        ),
    ];

    for (name, expected) in TABLES {
        let actual = tx
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [name],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if actual.as_deref().map(d04_normalize_catalog_sql)
            != Some(d04_normalize_catalog_sql(expected))
        {
            return Ok(false);
        }
    }

    let named_indexes = tx
        .prepare(
            "SELECT name FROM sqlite_master
             WHERE type = 'index' AND sql IS NOT NULL
               AND tbl_name IN ('issues', 'issue_deps')
             ORDER BY name",
        )?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if named_indexes
        != INDEXES
            .iter()
            .map(|(name, _)| (*name).to_string())
            .collect::<Vec<_>>()
    {
        return Ok(false);
    }
    for (name, expected) in INDEXES {
        let actual = tx.query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = ?1",
            [name],
            |row| row.get::<_, String>(0),
        )?;
        if d04_normalize_catalog_sql(&actual) != d04_normalize_catalog_sql(expected) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// V77 attributes every issue to an owning project. Issues created through the
/// operator surface carry no `created_by_session_id`, so no session-derived
/// project exists for them — and V77's own target schema keeps that column
/// nullable precisely because that absence is legitimate provenance, not
/// corruption. Their owning project cannot be inferred from any V76 column, so
/// it is supplied once by the operator through this migration-scoped variable.
const V77_UNOWNED_ISSUE_PROJECT_ENV: &str = "RSI_V77_UNOWNED_ISSUE_PROJECT";

/// Serializes tests that mutate [`V77_UNOWNED_ISSUE_PROJECT_ENV`]. Env mutation
/// is process-global, so a parallel test migrating its own fixture could
/// otherwise observe a declaration it never made.
#[cfg(test)]
pub(crate) static TEST_V77_UNOWNED_PROJECT_LOCK: parking_lot::Mutex<()> =
    parking_lot::Mutex::new(());

/// One-shot boundaries for the V77 rebuild. This is test-only by design: the
/// production migration has one atomic transaction and no injectable control
/// path. Keeping the inventory adjacent to the migration makes an omitted
/// rollback boundary mechanically visible to the copied-V76 regression.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum D04MigrationFault {
    AfterPreflight,
    AfterCreateIssues,
    AfterCreateIssueDeps,
    AfterCopyIssues,
    AfterCopyIssueDeps,
    AfterParityChecks,
    AfterDropIssueDeps,
    AfterDropIssues,
    AfterRenameIssues,
    AfterRenameIssueDeps,
    AfterIndexIssuesStatus,
    AfterIndexIssuesCreatedBy,
    AfterIndexIssuesProjectDisplay,
    AfterIndexIssuesProjectReady,
    AfterIndexIssuesProjectCreator,
    AfterIndexIssuesProjectIdea,
    AfterIndexIssuesSourceEvent,
    AfterIndexIssuesSourceFinding,
    AfterIndexIssueDepsBlocker,
    AfterIndexActiveDispatch,
    AfterForeignKeyCheck,
    AfterUserVersion,
    BeforeCommit,
}

#[cfg(test)]
impl D04MigrationFault {
    pub(crate) const ALL: [Self; 23] = [
        Self::AfterPreflight,
        Self::AfterCreateIssues,
        Self::AfterCreateIssueDeps,
        Self::AfterCopyIssues,
        Self::AfterCopyIssueDeps,
        Self::AfterParityChecks,
        Self::AfterDropIssueDeps,
        Self::AfterDropIssues,
        Self::AfterRenameIssues,
        Self::AfterRenameIssueDeps,
        Self::AfterIndexIssuesStatus,
        Self::AfterIndexIssuesCreatedBy,
        Self::AfterIndexIssuesProjectDisplay,
        Self::AfterIndexIssuesProjectReady,
        Self::AfterIndexIssuesProjectCreator,
        Self::AfterIndexIssuesProjectIdea,
        Self::AfterIndexIssuesSourceEvent,
        Self::AfterIndexIssuesSourceFinding,
        Self::AfterIndexIssueDepsBlocker,
        Self::AfterIndexActiveDispatch,
        Self::AfterForeignKeyCheck,
        Self::AfterUserVersion,
        Self::BeforeCommit,
    ];
}

#[cfg(test)]
thread_local! {
    static D04_MIGRATION_FAULT: RefCell<Option<D04MigrationFault>> = const { RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn d04_test_fail_next_migration(fault: D04MigrationFault) {
    D04_MIGRATION_FAULT.with(|slot| *slot.borrow_mut() = Some(fault));
}

#[cfg(test)]
fn d04_migration_fault(fault: D04MigrationFault) -> Result<()> {
    let injected = D04_MIGRATION_FAULT.with(|slot| {
        if *slot.borrow() == Some(fault) {
            *slot.borrow_mut() = None;
            true
        } else {
            false
        }
    });
    if injected {
        return Err(DaemonError::Store(format!(
            "injected V77 migration fault: {fault:?}"
        )));
    }
    Ok(())
}

/// Test-only failpoints for every durable boundary in the V78 `ProgramRun`
/// migration. Production builds contain no switch or environment hook.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum D05MigrationFault {
    AfterExactSource,
    AfterRuns,
    AfterTransitions,
    AfterGates,
    AfterBudgets,
    AfterLocks,
    AfterActions,
    AfterAttemptRefs,
    AfterLookupIndexes,
    AfterPartialUniqueIndexes,
    AfterNoDeleteTriggers,
    AfterImmutableTriggers,
    AfterForeignKeyCheck,
    AfterUserVersion,
    BeforeCommit,
}

#[cfg(test)]
impl D05MigrationFault {
    pub(crate) const ALL: [Self; 15] = [
        Self::AfterExactSource,
        Self::AfterRuns,
        Self::AfterTransitions,
        Self::AfterGates,
        Self::AfterBudgets,
        Self::AfterLocks,
        Self::AfterActions,
        Self::AfterAttemptRefs,
        Self::AfterLookupIndexes,
        Self::AfterPartialUniqueIndexes,
        Self::AfterNoDeleteTriggers,
        Self::AfterImmutableTriggers,
        Self::AfterForeignKeyCheck,
        Self::AfterUserVersion,
        Self::BeforeCommit,
    ];
}

#[cfg(test)]
thread_local! {
    static D05_MIGRATION_FAULT: RefCell<Option<D05MigrationFault>> = const { RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn d05_test_fail_next_migration(fault: D05MigrationFault) {
    D05_MIGRATION_FAULT.with(|slot| *slot.borrow_mut() = Some(fault));
}

#[cfg(test)]
fn d05_migration_fault(fault: D05MigrationFault) -> Result<()> {
    let injected = D05_MIGRATION_FAULT.with(|slot| {
        if *slot.borrow() == Some(fault) {
            *slot.borrow_mut() = None;
            true
        } else {
            false
        }
    });
    if injected {
        return Err(DaemonError::Store(format!(
            "injected V78 migration fault: {fault:?}"
        )));
    }
    Ok(())
}

/// Test-only failpoints for the forward-only V79 ProgramRun custody repair.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum D05V79MigrationFault {
    AfterExactSource,
    AfterRunRowsValidated,
    AfterTransitionRowsValidated,
    AfterGateRowsValidated,
    AfterBudgetRowsValidated,
    AfterLockRowsValidated,
    AfterActionRowsValidated,
    AfterAttemptRowsValidated,
    AfterClaimRunColumn,
    AfterClaimLeaseColumn,
    AfterValidationTriggers,
    AfterFingerprint,
    AfterForeignKeyCheck,
    AfterUserVersion,
    BeforeCommit,
}

#[cfg(test)]
impl D05V79MigrationFault {
    pub(crate) const ALL: [Self; 15] = [
        Self::AfterExactSource,
        Self::AfterRunRowsValidated,
        Self::AfterTransitionRowsValidated,
        Self::AfterGateRowsValidated,
        Self::AfterBudgetRowsValidated,
        Self::AfterLockRowsValidated,
        Self::AfterActionRowsValidated,
        Self::AfterAttemptRowsValidated,
        Self::AfterClaimRunColumn,
        Self::AfterClaimLeaseColumn,
        Self::AfterValidationTriggers,
        Self::AfterFingerprint,
        Self::AfterForeignKeyCheck,
        Self::AfterUserVersion,
        Self::BeforeCommit,
    ];
}

#[cfg(test)]
thread_local! {
    static D05_V79_MIGRATION_FAULT: RefCell<Option<D05V79MigrationFault>> = const { RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn d05_v79_test_fail_next_migration(fault: D05V79MigrationFault) {
    D05_V79_MIGRATION_FAULT.with(|slot| *slot.borrow_mut() = Some(fault));
}

#[cfg(test)]
fn d05_v79_migration_fault(fault: D05V79MigrationFault) -> Result<()> {
    let injected = D05_V79_MIGRATION_FAULT.with(|slot| {
        if *slot.borrow() == Some(fault) {
            *slot.borrow_mut() = None;
            true
        } else {
            false
        }
    });
    if injected {
        return Err(DaemonError::Store(format!(
            "injected V79 migration fault: {fault:?}"
        )));
    }
    Ok(())
}

/// Canonical V78 DDL for the two `ProgramRun` tables whose transition foreign
/// keys are `DEFERRABLE INITIALLY DEFERRED`. These are the only D05 tables the
/// V78 block amended after V78 had already shipped, so they are the only ones
/// a legacy V78 baseline can disagree about. Both the V78 create path and the
/// legacy-baseline repair below build them from these constants, so the shape
/// the repair converges on can never drift from the shape V78 creates.
const D05_V78_GATES_TABLE_DDL: &str = "CREATE TABLE idea_program_run_gates (
                    id TEXT PRIMARY KEY CHECK(length(id)=36 AND id=lower(id)),
                    program_run_id TEXT NOT NULL,
                    cursor_ordinal INTEGER NOT NULL CHECK(cursor_ordinal >= 0),
                    cursor_key TEXT NOT NULL CHECK(length(CAST(cursor_key AS BLOB)) BETWEEN 1 AND 64),
                    revision_no INTEGER NOT NULL CHECK(revision_no >= 0),
                    gate_key TEXT NOT NULL CHECK(length(CAST(gate_key AS BLOB)) BETWEEN 1 AND 64),
                    evaluation_no INTEGER NOT NULL CHECK(evaluation_no > 0),
                    result TEXT NOT NULL CHECK(result IN ('passed','failed','blocked')),
                    policy_key TEXT NOT NULL CHECK(length(CAST(policy_key AS BLOB)) BETWEEN 1 AND 64),
                    policy_version INTEGER NOT NULL CHECK(policy_version > 0),
                    evidence_ref TEXT NOT NULL CHECK(length(CAST(evidence_ref AS BLOB)) BETWEEN 1 AND 2048),
                    evidence_digest TEXT NOT NULL CHECK(
                        length(evidence_digest)=71 AND substr(evidence_digest,1,7)='sha256:'
                        AND substr(evidence_digest,8) NOT GLOB '*[^0-9a-f]*'),
                    transition_id TEXT NOT NULL,
                    idempotency_key TEXT NOT NULL CHECK(length(CAST(idempotency_key AS BLOB)) BETWEEN 1 AND 256),
                    request_fingerprint TEXT NOT NULL CHECK(
                        length(request_fingerprint)=71 AND substr(request_fingerprint,1,7)='sha256:'
                        AND substr(request_fingerprint,8) NOT GLOB '*[^0-9a-f]*'),
                    request_json TEXT NOT NULL CHECK(json_valid(request_json) AND length(CAST(request_json AS BLOB)) <= 1048576),
                    evaluator_kind TEXT NOT NULL CHECK(evaluator_kind IN ('operator','controller','scheduler','system')),
                    evaluator_session_id TEXT CHECK(evaluator_session_id IS NULL OR (length(evaluator_session_id)=36 AND evaluator_session_id=lower(evaluator_session_id))),
                    created_at TEXT NOT NULL CHECK(length(created_at)=30 AND substr(created_at,30,1)='Z'),
                    UNIQUE(program_run_id,cursor_ordinal,revision_no,gate_key,evaluation_no),
                    UNIQUE(program_run_id,idempotency_key),
                    FOREIGN KEY(program_run_id) REFERENCES idea_program_runs(id) ON DELETE RESTRICT,
                    FOREIGN KEY(transition_id) REFERENCES idea_program_run_transitions(id) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
                    FOREIGN KEY(evaluator_session_id) REFERENCES sessions(id) ON DELETE RESTRICT
                );";

/// Canonical V78 DDL for `idea_program_run_attempt_refs`. See
/// [`D05_V78_GATES_TABLE_DDL`].
const D05_V78_ATTEMPT_REFS_TABLE_DDL: &str = "CREATE TABLE idea_program_run_attempt_refs (
                    id TEXT PRIMARY KEY CHECK(length(id)=36 AND id=lower(id)),
                    program_run_id TEXT NOT NULL,
                    action_id TEXT NOT NULL UNIQUE,
                    cursor_ordinal INTEGER NOT NULL CHECK(cursor_ordinal >= 0),
                    cursor_key TEXT NOT NULL CHECK(length(CAST(cursor_key AS BLOB)) BETWEEN 1 AND 64),
                    revision_no INTEGER NOT NULL CHECK(revision_no >= 0),
                    attempt_no INTEGER NOT NULL CHECK(attempt_no > 0),
                    state TEXT NOT NULL CHECK(state IN ('reserved','launched','terminal_observed','output_committed','failed','interrupted','cancelled')),
                    session_id TEXT,
                    model_invocation_id TEXT,
                    observed_session_status TEXT,
                    observed_at TEXT,
                    output_ref TEXT CHECK(output_ref IS NULL OR length(CAST(output_ref AS BLOB)) <= 2048),
                    output_digest TEXT CHECK(output_digest IS NULL OR (
                        length(output_digest)=71 AND substr(output_digest,1,7)='sha256:'
                        AND substr(output_digest,8) NOT GLOB '*[^0-9a-f]*')),
                    creating_transition_id TEXT NOT NULL,
                    completing_transition_id TEXT,
                    created_at TEXT NOT NULL CHECK(length(created_at)=30 AND substr(created_at,30,1)='Z'),
                    updated_at TEXT NOT NULL CHECK(length(updated_at)=30 AND substr(updated_at,30,1)='Z'),
                    UNIQUE(program_run_id,cursor_ordinal,revision_no,attempt_no),
                    CHECK((state='reserved' AND session_id IS NULL AND model_invocation_id IS NULL AND observed_at IS NULL AND output_digest IS NULL)
                       OR (state='launched' AND (session_id IS NOT NULL OR model_invocation_id IS NOT NULL) AND observed_at IS NULL AND output_digest IS NULL)
                       OR (state='terminal_observed' AND observed_session_status IS NOT NULL AND observed_at IS NOT NULL AND output_digest IS NULL)
                       OR (state='output_committed' AND output_ref IS NOT NULL AND output_digest IS NOT NULL AND completing_transition_id IS NOT NULL)
                       OR state IN ('failed','interrupted','cancelled')),
                    FOREIGN KEY(program_run_id) REFERENCES idea_program_runs(id) ON DELETE RESTRICT,
                    FOREIGN KEY(action_id) REFERENCES idea_program_run_actions(id) ON DELETE RESTRICT,
                    FOREIGN KEY(session_id) REFERENCES sessions(id) ON DELETE RESTRICT,
                    FOREIGN KEY(creating_transition_id) REFERENCES idea_program_run_transitions(id) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
                    FOREIGN KEY(completing_transition_id) REFERENCES idea_program_run_transitions(id) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED
                );";

/// Scratch table used to park rows while a legacy V78 `ProgramRun` table is
/// rebuilt. The name deliberately does not match `idea_program_run%`, so it can
/// never be picked up by the D05 catalog validators or the schema fingerprint.
const D05_V78_REPAIR_SCRATCH_TABLE: &str = "d05_v78_baseline_repair_scratch";

/// Rebuild one D05 table onto `canonical_ddl`, preserving its rows along with
/// its own indexes and triggers.
///
/// `SQLite` cannot alter a foreign-key clause in place, so the table has to be
/// recreated. Only the `CREATE TABLE` text comes from code: the indexes and
/// triggers are replayed from the database's own catalog, in catalog order,
/// because those are identical across the legacy and canonical baselines and
/// replaying them keeps this repair from restating (and later drifting from)
/// the V78 DDL. Catalog order matters — `PRAGMA index_list` reports a per-table
/// sequence that the schema fingerprint hashes. Implicit `sqlite_autoindex_*`
/// entries carry no `sql` and are recreated by the `CREATE TABLE` itself.
///
/// Neither rebuilt table is the target of a foreign key, so the implicit delete
/// that `DROP TABLE` performs under `PRAGMA foreign_keys=ON` cannot violate a
/// constraint, and `DROP TABLE` does not fire the `BEFORE DELETE` guards that
/// otherwise make these tables append-only.
fn rebuild_d05_v78_table(tx: &Transaction<'_>, table: &str, canonical_ddl: &str) -> Result<()> {
    let aux_ddl = {
        let mut catalog = tx.prepare(
            "SELECT sql FROM sqlite_master
             WHERE tbl_name = ?1 AND type IN ('index','trigger') AND sql IS NOT NULL
             ORDER BY rowid",
        )?;
        catalog
            .query_map([table], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };

    let scratch = D05_V78_REPAIR_SCRATCH_TABLE;
    tx.execute_batch(&format!(
        "DROP TABLE IF EXISTS {scratch};
         CREATE TABLE {scratch} AS SELECT * FROM {table};
         DROP TABLE {table};"
    ))?;
    tx.execute_batch(canonical_ddl)?;
    for statement in aux_ddl {
        tx.execute_batch(&format!("{statement};"))?;
    }
    // The scratch copy preserves column order, so the positional insert is
    // exact; the amendment changed foreign-key clauses only, never columns.
    tx.execute_batch(&format!(
        "INSERT INTO {table} SELECT * FROM {scratch};
         DROP TABLE {scratch};"
    ))?;
    Ok(())
}

/// Test-only failpoints for V80 agent-coordination storage. Production has no
/// environment or runtime switch capable of interrupting a migration.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AgentCoordinationV80MigrationFault {
    AfterExactSource,
    AfterSchema,
    AfterFingerprint,
    AfterForeignKeyCheck,
    AfterUserVersion,
    BeforeCommit,
}

#[cfg(test)]
impl AgentCoordinationV80MigrationFault {
    pub(crate) const ALL: [Self; 6] = [
        Self::AfterExactSource,
        Self::AfterSchema,
        Self::AfterFingerprint,
        Self::AfterForeignKeyCheck,
        Self::AfterUserVersion,
        Self::BeforeCommit,
    ];
}

#[cfg(test)]
thread_local! {
    static AGENT_COORDINATION_V80_MIGRATION_FAULT: RefCell<Option<AgentCoordinationV80MigrationFault>> = const { RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn agent_coordination_v80_test_fail_next_migration(
    fault: AgentCoordinationV80MigrationFault,
) {
    AGENT_COORDINATION_V80_MIGRATION_FAULT.with(|slot| *slot.borrow_mut() = Some(fault));
}

#[cfg(test)]
fn agent_coordination_v80_migration_fault(fault: AgentCoordinationV80MigrationFault) -> Result<()> {
    let injected = AGENT_COORDINATION_V80_MIGRATION_FAULT.with(|slot| {
        if *slot.borrow() == Some(fault) {
            *slot.borrow_mut() = None;
            true
        } else {
            false
        }
    });
    if injected {
        return Err(DaemonError::Store(format!(
            "injected V80 migration fault: {fault:?}"
        )));
    }
    Ok(())
}

/// Every-statement failpoint for the corrected V81 mailbox migration (P2-02).
///
/// A failpoint exists after each relation family, the copy, the row-count
/// parity check, the trigger and index families, the fingerprint, the fully
/// drained `PRAGMA foreign_key_check`, the version write, and precommit. Every
/// one must roll back to an identical source catalog, data, and `user_version`.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AgentMessageV81MigrationFault {
    AfterExactSource,
    AfterV80Fingerprint,
    AfterSchema,
    AfterCopy,
    AfterFingerprint,
    AfterForeignKeyCheck,
    AfterUserVersion,
    BeforeCommit,
}

#[cfg(test)]
impl AgentMessageV81MigrationFault {
    pub(crate) const ALL: [Self; 8] = [
        Self::AfterExactSource,
        Self::AfterV80Fingerprint,
        Self::AfterSchema,
        Self::AfterCopy,
        Self::AfterFingerprint,
        Self::AfterForeignKeyCheck,
        Self::AfterUserVersion,
        Self::BeforeCommit,
    ];
}

#[cfg(test)]
thread_local! {
    static AGENT_MESSAGE_V81_MIGRATION_FAULT: RefCell<Option<AgentMessageV81MigrationFault>> = const { RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn agent_message_v81_test_fail_next_migration(fault: AgentMessageV81MigrationFault) {
    AGENT_MESSAGE_V81_MIGRATION_FAULT.with(|slot| *slot.borrow_mut() = Some(fault));
}

#[cfg(test)]
fn agent_message_v81_migration_fault(fault: AgentMessageV81MigrationFault) -> Result<()> {
    let injected = AGENT_MESSAGE_V81_MIGRATION_FAULT.with(|slot| {
        if *slot.borrow() == Some(fault) {
            *slot.borrow_mut() = None;
            true
        } else {
            false
        }
    });
    if injected {
        return Err(DaemonError::Store(format!(
            "injected V81 migration fault: {fault:?}"
        )));
    }
    Ok(())
}

/// Every-statement failpoint for the V82 mailbox amendment (P2-06d).
///
/// V82 is the first migration in this campaign that runs against a database
/// already carrying real operator data, and it REBUILDS a relation four other
/// relations hold `ON DELETE RESTRICT` foreign keys into. A half-applied
/// rebuild is therefore not a failing test, it is a corrupt database — so every
/// step gets a failpoint and every one must roll back to an identical source
/// catalog, data, and `user_version`.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AgentMessageV82MigrationFault {
    AfterExactSource,
    AfterV81Fingerprint,
    AfterSchema,
    AfterCopy,
    AfterForeignKeyCheck,
    AfterUserVersion,
    AfterFingerprint,
    BeforeCommit,
}

#[cfg(test)]
impl AgentMessageV82MigrationFault {
    pub(crate) const ALL: [Self; 8] = [
        Self::AfterExactSource,
        Self::AfterV81Fingerprint,
        Self::AfterSchema,
        Self::AfterCopy,
        Self::AfterForeignKeyCheck,
        Self::AfterUserVersion,
        Self::AfterFingerprint,
        Self::BeforeCommit,
    ];
}

#[cfg(test)]
thread_local! {
    static AGENT_MESSAGE_V82_MIGRATION_FAULT: RefCell<Option<AgentMessageV82MigrationFault>> = const { RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn agent_message_v82_test_fail_next_migration(fault: AgentMessageV82MigrationFault) {
    AGENT_MESSAGE_V82_MIGRATION_FAULT.with(|slot| *slot.borrow_mut() = Some(fault));
}

#[cfg(test)]
fn agent_message_v82_migration_fault(fault: AgentMessageV82MigrationFault) -> Result<()> {
    let injected = AGENT_MESSAGE_V82_MIGRATION_FAULT.with(|slot| {
        if *slot.borrow() == Some(fault) {
            *slot.borrow_mut() = None;
            true
        } else {
            false
        }
    });
    if injected {
        return Err(DaemonError::Store(format!(
            "injected V82 migration fault: {fault:?}"
        )));
    }
    Ok(())
}

/// Stable project seeded only into isolated in-memory test stores. Production
/// stores never synthesize project ownership; D04 writers always receive it
/// from their bound caller or operator request.
#[cfg(test)]
pub(crate) fn d04_test_project_id() -> Uuid {
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
pub(crate) fn is_canonical_jsonrpc_id(value: &str) -> bool {
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
    pub(crate) conn: Connection,
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
}

#[cfg(test)]
const CURRENT_SCHEMA_TEMPLATE_PAGE_LIMIT: i64 = 16_384;
#[cfg(test)]
const CURRENT_SCHEMA_TEMPLATE_PAGES_PER_STEP: i32 = 64;
#[cfg(test)]
const CURRENT_SCHEMA_TEMPLATE_STEP_LIMIT: usize = 272;
#[cfg(test)]
const CURRENT_SCHEMA_TEMPLATE_NO_PROGRESS_LIMIT: usize = 8;
#[cfg(test)]
const CURRENT_SCHEMA_TEMPLATE_BUSY_LOCKED_LIMIT: usize = 8;
#[cfg(test)]
const CURRENT_SCHEMA_TEMPLATE_DEADLINE: Duration = Duration::from_millis(500);
#[cfg(test)]
const CURRENT_SCHEMA_TEMPLATE_RETRY_PAUSE: Duration = Duration::from_millis(1);
#[cfg(test)]
const CURRENT_SCHEMA_TEMPLATE_CACHE_PROTOCOL: u32 = 1;
#[cfg(test)]
const CURRENT_SCHEMA_TEMPLATE_CACHE_LOCK_DEADLINE: Duration = Duration::from_secs(30);

#[cfg(test)]
type CurrentSchemaCatalogRow = (String, String, String, String);

#[cfg(test)]
struct CurrentSchemaTemplateSource {
    conn: Connection,
    catalog: Vec<CurrentSchemaCatalogRow>,
    page_count: i64,
}

#[cfg(test)]
struct SharedCurrentSchemaTemplateSource {
    path: PathBuf,
    catalog: Vec<CurrentSchemaCatalogRow>,
    page_count: i64,
}

#[cfg(test)]
enum CurrentSchemaTemplateBacking {
    Shared(SharedCurrentSchemaTemplateSource),
    Local(Mutex<CurrentSchemaTemplateSource>),
}

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CachedCurrentSchemaTemplateError {
    code: CurrentSchemaTemplateErrorCode,
}

#[cfg(test)]
impl CachedCurrentSchemaTemplateError {
    const fn new(code: CurrentSchemaTemplateErrorCode) -> Self {
        Self { code }
    }

    fn to_daemon(self) -> DaemonError {
        self.code.error("cached initialization failed")
    }
}

#[cfg(test)]
static CURRENT_SCHEMA_TEMPLATE: OnceLock<
    std::result::Result<CurrentSchemaTemplateBacking, CachedCurrentSchemaTemplateError>,
> = OnceLock::new();

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CurrentSchemaTemplateInitMode {
    Real,
    InjectSpawnFailure,
    InjectJoinPanic,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CurrentSchemaTemplateBackupObservation {
    More(i32),
    Done(i32),
    Busy,
    Locked,
    Error,
    Unknown,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CurrentSchemaTemplateBackupAction {
    Continue,
    Pause,
    Done,
}

#[cfg(test)]
struct CurrentSchemaTemplateBackupBudget {
    calls: usize,
    consecutive_no_progress: usize,
    busy_locked: usize,
    previous_remaining: i32,
}

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
fn current_schema_template_runtime_identity() -> Result<Uuid> {
    let identity = Uuid::new_v4();
    if identity.is_nil() {
        return Err(CurrentSchemaTemplateErrorCode::RuntimeIdentity.error("nil UUID"));
    }
    Ok(identity)
}

#[cfg(test)]
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
    })
}

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
fn current_schema_template_cache_key() -> String {
    format!(
        "v{}-schema{}-{}.sqlite3",
        CURRENT_SCHEMA_TEMPLATE_CACHE_PROTOCOL,
        LATEST_SCHEMA_VERSION,
        env!("RSID_TEST_SCHEMA_SOURCE_DIGEST")
    )
}

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
struct CurrentSchemaTemplateTemporaryFile(PathBuf);

#[cfg(test)]
impl Drop for CurrentSchemaTemplateTemporaryFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(test)]
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
    })
}

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
fn current_schema_template_from_cache<'a, T>(
    cache: &'a OnceLock<std::result::Result<T, CachedCurrentSchemaTemplateError>>,
    initialize: impl FnOnce() -> std::result::Result<T, CachedCurrentSchemaTemplateError>,
) -> Result<&'a T> {
    match cache.get_or_init(initialize) {
        Ok(source) => Ok(source),
        Err(error) => Err(error.to_daemon()),
    }
}

#[cfg(test)]
fn current_schema_template() -> Result<&'static CurrentSchemaTemplateBacking> {
    current_schema_template_from_cache(&CURRENT_SCHEMA_TEMPLATE, || {
        initialize_current_schema_template(CurrentSchemaTemplateInitMode::Real)
    })
}

#[cfg(test)]
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
                    return Err(CurrentSchemaTemplateErrorCode::LockDeadline
                        .error("source mutex acquisition exceeded 500 ms"));
                }
                std::thread::sleep(CURRENT_SCHEMA_TEMPLATE_RETRY_PAUSE);
            }
        }
    }
}

#[cfg(test)]
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
                CurrentSchemaTemplateErrorCode::BackupDeadline.error("copy exceeded 500 ms")
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

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
pub(crate) fn current_schema_template_disk_cache_for_test(
    directory: &Path,
) -> Result<(PathBuf, i64)> {
    let source = shared_current_schema_template_at(directory)?;
    Ok((source.path, source.page_count))
}

#[cfg(test)]
pub(crate) fn current_schema_template_cache_key_for_test() -> String {
    current_schema_template_cache_key()
}

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
pub(crate) fn cached_current_schema_early_failure_for_test() -> Result<Store> {
    current_schema_template()?;
    Err(CurrentSchemaTemplateErrorCode::DestinationInvariant.error("injected early clone failure"))
}

impl Store {
    pub(crate) fn set_program_run_boot_id(&self, boot_id: Uuid) -> Result<()> {
        if boot_id.is_nil() {
            return Err(DaemonError::Store(
                "ProgramRun daemon boot identity must be non-nil".into(),
            ));
        }
        self.program_run_boot_id.set(boot_id);
        Ok(())
    }

    pub(crate) fn program_run_boot_id(&self) -> Uuid {
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
    pub(crate) fn set_delivery_boot_id(&self, boot_id: Uuid) -> Result<()> {
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
    pub(crate) fn delivery_boot_id(&self) -> Uuid {
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

    /// Open or create database at the given path.
    /// Runs schema initialization on open.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        Self::register_sql_functions(&conn)?;

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
        };
        store.init_schema()?;
        Ok(store)
    }

    /// Open an in-memory SQLite database for testing.
    /// Clones the process-wide migration-built current-schema template.
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        open_in_memory_from_current_schema_template_with_timestamp(|| {
            chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
        })
    }

    /// Build a current-schema in-memory fixture by replaying every migration.
    #[cfg(test)]
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

    #[cfg(test)]
    pub(crate) fn open_in_memory_with_timestamp_for_test(
        timestamp: impl FnOnce() -> String,
    ) -> Result<Self> {
        open_in_memory_from_current_schema_template_with_timestamp(timestamp)
    }

    #[cfg(test)]
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
        };
        store.init_schema_internal(false, true)?;
        Ok(store)
    }

    /// Build an authenticated predecessor shell without executing V85-V88.
    /// V88 migration tests use this to construct deployed V87 inputs solely
    /// from the sealed predecessor literals instead of rewinding V88.
    #[cfg(test)]
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

    /// Normalize a pre-amendment V78 `ProgramRun` baseline onto the canonical V78
    /// schema.
    ///
    /// V78 shipped, and was then edited in place to mark three transition
    /// foreign keys `DEFERRABLE INITIALLY DEFERRED`. Databases that ran the
    /// original DDL therefore hold a baseline that the V79 fingerprint gate
    /// rejects forever. This repair converges them onto the amended shape and
    /// proves it did so by asserting the canonical V78 fingerprint — the same
    /// constant [`program_runs::validate_d05_catalog`] enforces on the create
    /// path — before committing.
    ///
    /// The repair is idempotent, and it is atomic: any failure rolls the
    /// transaction back and leaves the database exactly as it was.
    fn repair_legacy_d05_v78_baseline(&self) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if program_runs::d05_schema_fingerprint(&tx)? == program_runs::D05_V78_SCHEMA_FINGERPRINT {
            return Ok(());
        }

        for (table, canonical_ddl) in [
            ("idea_program_run_gates", D05_V78_GATES_TABLE_DDL),
            (
                "idea_program_run_attempt_refs",
                D05_V78_ATTEMPT_REFS_TABLE_DDL,
            ),
        ] {
            rebuild_d05_v78_table(&tx, table, canonical_ddl)?;
        }

        let repaired = program_runs::d05_schema_fingerprint(&tx)?;
        if repaired != program_runs::D05_V78_SCHEMA_FINGERPRINT {
            return Err(DaemonError::Store(format!(
                "legacy V78 ProgramRun baseline repair did not reach the canonical V78 schema: {repaired}"
            )));
        }
        tx.commit()?;
        tracing::info!("V78 ProgramRun baseline normalized to the canonical schema");
        Ok(())
    }

    /// Initialize schema with version-based migrations.
    /// Uses PRAGMA user_version to track which migrations have been applied.
    fn init_schema(&self) -> Result<()> {
        self.init_schema_internal(false, false)
            .map_err(lineage_convergence::annotate_v112_branched_lineage_fault)
    }

    fn init_schema_internal(&self, stop_after_v84: bool, stop_after_v92: bool) -> Result<()> {
        let version: i32 = self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))?;

        // V0: Original schema
        self.conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS sessions (
                id              TEXT PRIMARY KEY,
                claude_session_id TEXT,
                provider        TEXT NOT NULL DEFAULT 'Claude',
                query           TEXT NOT NULL,
                working_dir     TEXT NOT NULL,
                status          TEXT NOT NULL,
                created_at      TEXT NOT NULL,
                updated_at      TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS conversation_events (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id      TEXT NOT NULL REFERENCES sessions(id),
                sequence        INTEGER NOT NULL,
                event_type      TEXT NOT NULL,
                role            TEXT,
                content         TEXT NOT NULL DEFAULT '',
                tool_name       TEXT,
                tool_input      TEXT,
                created_at      TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_events_session_id
                ON conversation_events(session_id);
            CREATE INDEX IF NOT EXISTS idx_events_session_sequence
                ON conversation_events(session_id, sequence);

            CREATE TABLE IF NOT EXISTS approvals (
                id              TEXT PRIMARY KEY,
                session_id      TEXT NOT NULL REFERENCES sessions(id),
                tool_name       TEXT NOT NULL,
                tool_input      TEXT NOT NULL,
                status          TEXT NOT NULL,
                created_at      TEXT NOT NULL,
                resolved_at     TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_approvals_session_id
                ON approvals(session_id);
            ",
        )?;

        // V1: Session metadata columns + turn_metrics table
        if version < 1 {
            self.add_column_if_not_exists("sessions", "cost_usd", "REAL")?;
            self.add_column_if_not_exists("sessions", "duration_ms", "INTEGER")?;
            self.add_column_if_not_exists("sessions", "num_turns", "INTEGER")?;
            self.add_column_if_not_exists("sessions", "model", "TEXT")?;
            self.add_column_if_not_exists("sessions", "input_tokens", "INTEGER")?;
            self.add_column_if_not_exists("sessions", "output_tokens", "INTEGER")?;
            self.add_column_if_not_exists("sessions", "context_window", "INTEGER")?;
            self.add_column_if_not_exists("sessions", "total_input_tokens", "INTEGER")?;
            self.add_column_if_not_exists("sessions", "total_output_tokens", "INTEGER")?;
            self.add_column_if_not_exists("sessions", "total_cache_creation_tokens", "INTEGER")?;
            self.add_column_if_not_exists("sessions", "total_cache_read_tokens", "INTEGER")?;
            self.add_column_if_not_exists("sessions", "stop_reason", "TEXT")?;

            self.conn.execute_batch(
                "
                CREATE TABLE IF NOT EXISTS turn_metrics (
                    id                      INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_id              TEXT NOT NULL REFERENCES sessions(id),
                    turn_number             INTEGER NOT NULL,
                    input_tokens            INTEGER NOT NULL DEFAULT 0,
                    cache_creation_tokens   INTEGER NOT NULL DEFAULT 0,
                    cache_read_tokens       INTEGER NOT NULL DEFAULT 0,
                    output_tokens           INTEGER NOT NULL DEFAULT 0,
                    stop_reason             TEXT,
                    tools_used              TEXT,
                    tool_count              INTEGER NOT NULL DEFAULT 0,
                    created_at              TEXT NOT NULL
                );

                CREATE INDEX IF NOT EXISTS idx_turn_metrics_session
                    ON turn_metrics(session_id);
                CREATE INDEX IF NOT EXISTS idx_turn_metrics_session_turn
                    ON turn_metrics(session_id, turn_number);
                ",
            )?;

            self.conn.execute("PRAGMA user_version = 1", [])?;
        }

        // V2: Projects table and session project_id FK
        if version < 2 {
            self.conn.execute_batch(
                "
                CREATE TABLE IF NOT EXISTS projects (
                    id          TEXT PRIMARY KEY,
                    name        TEXT NOT NULL UNIQUE,
                    path        TEXT,
                    description TEXT,
                    color       TEXT DEFAULT '#89b4fa',
                    created_at  TEXT NOT NULL,
                    updated_at  TEXT NOT NULL
                );

                CREATE INDEX IF NOT EXISTS idx_projects_path ON projects(path);
                ",
            )?;

            // Add project_id to sessions (nullable FK)
            self.add_column_if_not_exists("sessions", "project_id", "TEXT")?;

            self.conn.execute_batch(
                "CREATE INDEX IF NOT EXISTS idx_sessions_project_id ON sessions(project_id);",
            )?;

            self.conn.execute("PRAGMA user_version = 2", [])?;
        }

        // V3: Pinned sessions
        if version < 3 {
            self.add_column_if_not_exists("sessions", "pinned_at", "TEXT")?;
            self.conn.execute("PRAGMA user_version = 3", [])?;
        }

        // V4: Session kind for TaskRabbit support
        if version < 4 {
            self.add_column_if_not_exists(
                "sessions",
                "session_kind",
                "TEXT NOT NULL DEFAULT 'Standard'",
            )?;
            self.conn.execute("PRAGMA user_version = 4", [])?;
        }

        // V5: Context rotation support (continued_from FK)
        if version < 5 {
            self.add_column_if_not_exists("sessions", "continued_from", "TEXT")?;
            self.conn.execute_batch(
                "CREATE INDEX IF NOT EXISTS idx_sessions_continued_from ON sessions(continued_from);",
            )?;
            self.conn.execute("PRAGMA user_version = 5", [])?;
        }

        // V6: Provider support (Claude/Codex)
        if version < 6 {
            self.add_column_if_not_exists(
                "sessions",
                "provider",
                "TEXT NOT NULL DEFAULT 'Claude'",
            )?;
            self.conn.execute("PRAGMA user_version = 6", [])?;
        }

        // V7: Context snapshots for crash recovery
        if version < 7 {
            self.conn.execute_batch(
                "
                CREATE TABLE IF NOT EXISTS context_snapshots (
                    id          INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                    tokens_used INTEGER NOT NULL,
                    created_at  TEXT NOT NULL
                );

                CREATE INDEX IF NOT EXISTS idx_context_snapshots_session_latest
                    ON context_snapshots(session_id, created_at DESC);
                ",
            )?;
            self.conn.execute("PRAGMA user_version = 7", [])?;
        }

        // V8: Add handoff_filepath for automatic rotation
        if version < 8 {
            self.add_column_if_not_exists("sessions", "handoff_filepath", "TEXT")?;
            self.conn.execute("PRAGMA user_version = 8", [])?;
        }

        // V9: Model segments for mid-session model switching
        if version < 9 {
            tracing::info!("Applying migration V9: model_segments table");

            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS model_segments (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_id TEXT NOT NULL,
                    model_id TEXT NOT NULL,
                    from_sequence INTEGER NOT NULL,
                    to_sequence INTEGER,
                    created_at TEXT NOT NULL,
                    FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE
                );

                CREATE INDEX IF NOT EXISTS idx_model_segments_session
                    ON model_segments(session_id, from_sequence);",
            )?;

            self.add_column_if_not_exists("turn_metrics", "model", "TEXT")?;

            // Backfill: create initial segment for each session with a model
            self.conn.execute(
                "INSERT INTO model_segments (session_id, model_id, from_sequence, to_sequence, created_at)
                 SELECT id, model, 0, NULL, created_at
                 FROM sessions
                 WHERE model IS NOT NULL",
                [],
            )?;

            let segment_count: i64 =
                self.conn
                    .query_row("SELECT COUNT(*) FROM model_segments", [], |r| r.get(0))?;
            tracing::info!(
                "V9 migration complete: {} segments backfilled",
                segment_count
            );

            self.conn.execute("PRAGMA user_version = 9", [])?;
        }

        if version < 10 {
            self.conn.execute(
                "UPDATE sessions SET status = 'Archived' WHERE status = 'Rotated'",
                [],
            )?;
            let converted: i64 = self.conn.query_row("SELECT changes()", [], |r| r.get(0))?;
            tracing::info!(
                "V10 migration complete: {} Rotated sessions converted to Archived",
                converted
            );
            self.conn.execute("PRAGMA user_version = 10", [])?;
        }

        if version < 11 {
            self.add_column_if_not_exists(
                "sessions",
                "rotation_depth",
                "INTEGER NOT NULL DEFAULT 0",
            )?;
            tracing::info!("V11 migration complete: added rotation_depth column to sessions");
            self.conn.execute("PRAGMA user_version = 11", [])?;
        }

        // V12: Daemon-counted token columns (monotonically increasing, no API dependency)
        if version < 12 {
            self.add_column_if_not_exists("sessions", "daemon_input_tokens", "INTEGER")?;
            self.add_column_if_not_exists("sessions", "daemon_output_tokens", "INTEGER")?;
            tracing::info!(
                "V12 migration complete: added daemon_input_tokens/daemon_output_tokens to sessions"
            );
            self.conn.execute("PRAGMA user_version = 12", [])?;
        }

        // V13: Haiku-generated or user-set session title
        if version < 13 {
            self.add_column_if_not_exists("sessions", "title", "TEXT")?;
            tracing::info!("V13 migration complete: added title column to sessions");
            self.conn.execute("PRAGMA user_version = 13", [])?;
        }

        // V14: Pipeline artifact path (Write-tool detected output file)
        if version < 14 {
            self.add_column_if_not_exists("sessions", "pipeline_artifact", "TEXT")?;
            tracing::info!("V14 migration complete: added pipeline_artifact column to sessions");
            self.conn.execute("PRAGMA user_version = 14", [])?;
        }

        // V15: Workflows table and session workflow_id FK
        if version < 15 {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS workflows (
                    id          TEXT PRIMARY KEY,
                    title       TEXT NOT NULL,
                    stage       TEXT NOT NULL DEFAULT 'Research',
                    artifact_path TEXT,
                    definition_json TEXT,
                    project_id  TEXT REFERENCES projects(id),
                    created_at  TEXT NOT NULL,
                    updated_at  TEXT NOT NULL
                );

                CREATE INDEX IF NOT EXISTS idx_workflows_project ON workflows(project_id);
                CREATE INDEX IF NOT EXISTS idx_workflows_stage ON workflows(stage);",
            )?;

            self.add_column_if_not_exists("sessions", "workflow_id", "TEXT")?;
            self.conn.execute_batch(
                "CREATE INDEX IF NOT EXISTS idx_sessions_workflow ON sessions(workflow_id);",
            )?;

            tracing::info!("V15 migration complete: workflows table + session workflow_id");
            self.conn.execute("PRAGMA user_version = 15", [])?;
        }

        // V16: ESP games table
        if version < 16 {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS esp_games (
                    id TEXT PRIMARY KEY,
                    played_at TEXT NOT NULL,
                    score INTEGER NOT NULL,
                    rounds_played INTEGER NOT NULL,
                    total_rounds INTEGER NOT NULL,
                    p_value REAL NOT NULL,
                    round_details TEXT NOT NULL
                );",
            )?;
            tracing::info!("V16 migration complete: esp_games table");
            self.conn.execute("PRAGMA user_version = 16", [])?;
        }

        // V17: LLM-generated paragraph description for sessions
        if version < 17 {
            self.add_column_if_not_exists("sessions", "description", "TEXT")?;
            tracing::info!("V17 migration complete: added description column to sessions");
            self.conn.execute("PRAGMA user_version = 17", [])?;
        }

        // V18: Rotation events table for debugging context rotation lifecycle
        if version < 18 {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS rotation_events (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_id TEXT NOT NULL,
                    rotation_id TEXT NOT NULL,
                    phase TEXT NOT NULL,
                    event_type TEXT NOT NULL,
                    metadata TEXT,
                    created_at TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_rotation_events_session ON rotation_events(session_id);
                CREATE INDEX IF NOT EXISTS idx_rotation_events_rotation ON rotation_events(rotation_id);
                PRAGMA user_version = 18;",
            )?;
            tracing::info!("V18 migration complete: rotation_events table");
        }

        // V19: Git branch tracking per session
        if version < 19 {
            self.add_column_if_not_exists("sessions", "git_branch", "TEXT")?;
            tracing::info!("V19 migration complete: added git_branch column to sessions");
            self.conn.execute("PRAGMA user_version = 19", [])?;
        }

        // V20: Durable work context — persistent active task description
        if version < 20 {
            self.add_column_if_not_exists("sessions", "active_task", "TEXT")?;
            tracing::info!("V20 migration complete: added active_task column to sessions");
            self.conn.execute("PRAGMA user_version = 20", [])?;
        }

        // V21: Session grouping (lightweight convoy)
        if version < 21 {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS session_groups (
                    id          TEXT PRIMARY KEY,
                    name        TEXT NOT NULL,
                    description TEXT,
                    project_id  TEXT,
                    color       TEXT NOT NULL DEFAULT '#cba6f7',
                    created_at  TEXT NOT NULL,
                    updated_at  TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_session_groups_project ON session_groups(project_id);",
            )?;

            self.add_column_if_not_exists("sessions", "group_id", "TEXT")?;
            self.conn.execute_batch(
                "CREATE INDEX IF NOT EXISTS idx_sessions_group_id ON sessions(group_id);",
            )?;

            tracing::info!("V21 migration complete: session_groups table + sessions.group_id");
            self.conn.execute("PRAGMA user_version = 21", [])?;
        }

        // V22: Durable pending-archive flag
        if version < 22 {
            self.add_column_if_not_exists(
                "sessions",
                "pending_archive",
                "INTEGER NOT NULL DEFAULT 0",
            )?;
            tracing::info!("V22 migration complete: added pending_archive column to sessions");
            self.conn.execute("PRAGMA user_version = 22", [])?;
        }

        // V23: Project context files for structured context injection
        if version < 23 {
            self.add_column_if_not_exists("projects", "context_files", "TEXT")?;
            tracing::info!("V23 migration complete: added context_files column to projects");
            self.conn.execute("PRAGMA user_version = 23", [])?;
        }

        // V24: Testing-needed session marker
        if version < 24 {
            self.add_column_if_not_exists("sessions", "testing_needed_at", "TEXT")?;
            tracing::info!("V24 migration complete: added testing_needed_at column to sessions");
            self.conn.execute("PRAGMA user_version = 24", [])?;
        }

        // V25: Per-session rotation toggle
        if version < 25 {
            self.add_column_if_not_exists("sessions", "rotation_disabled_at", "TEXT")?;
            tracing::info!("V25 migration complete: added rotation_disabled_at column to sessions");
            self.conn.execute("PRAGMA user_version = 25", [])?;
        }

        // V26: Effort parameter for Claude sessions
        if version < 26 {
            self.add_column_if_not_exists("sessions", "effort", "TEXT")?;
            tracing::info!("V26 migration complete: added effort column to sessions");
            self.conn.execute("PRAGMA user_version = 26", [])?;
        }

        // V27: Persist full workflow definitions alongside metadata.
        if version < 27 {
            self.add_column_if_not_exists("workflows", "definition_json", "TEXT")?;
            tracing::info!("V27 migration complete: added definition_json to workflows");
            self.conn.execute("PRAGMA user_version = 27", [])?;
        }

        // V28: Background task queue for async memory operations
        if version < 28 {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS background_queue (
                    id              INTEGER PRIMARY KEY AUTOINCREMENT,
                    work_unit_key   TEXT NOT NULL,
                    task_type       TEXT NOT NULL,
                    session_id      TEXT,
                    project_id      TEXT,
                    payload         TEXT NOT NULL DEFAULT '{}',
                    token_count     INTEGER NOT NULL DEFAULT 0,
                    status          TEXT NOT NULL DEFAULT 'pending',
                    priority        INTEGER NOT NULL DEFAULT 0,
                    attempts        INTEGER NOT NULL DEFAULT 0,
                    max_attempts    INTEGER NOT NULL DEFAULT 5,
                    error           TEXT,
                    created_at      TEXT NOT NULL,
                    claimed_at      TEXT,
                    completed_at    TEXT
                );

                CREATE INDEX IF NOT EXISTS idx_bg_queue_work_unit
                    ON background_queue(work_unit_key, status);
                CREATE INDEX IF NOT EXISTS idx_bg_queue_status_type
                    ON background_queue(status, task_type);
                CREATE INDEX IF NOT EXISTS idx_bg_queue_session
                    ON background_queue(session_id, status);
                CREATE INDEX IF NOT EXISTS idx_bg_queue_claimed_stale
                    ON background_queue(claimed_at) WHERE status = 'claimed';",
            )?;
            tracing::info!("V28 migration complete: background_queue table");
            self.conn.execute("PRAGMA user_version = 28", [])?;
        }

        // V29: Entity cards (project + user cards for context injection)
        if version < 29 {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS entity_cards (
                    id          TEXT PRIMARY KEY,
                    entity_type TEXT NOT NULL,
                    entity_id   TEXT NOT NULL,
                    facts       TEXT NOT NULL DEFAULT '[]',
                    created_at  TEXT NOT NULL,
                    updated_at  TEXT NOT NULL,
                    UNIQUE(entity_type, entity_id)
                );
                CREATE INDEX IF NOT EXISTS idx_entity_cards_lookup
                    ON entity_cards(entity_type, entity_id);",
            )?;
            tracing::info!("V29 migration complete: entity_cards table");
            self.conn.execute("PRAGMA user_version = 29", [])?;
        }

        // V30: Two-tier session summarization
        if version < 30 {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS session_summaries (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                    kind TEXT NOT NULL,
                    content TEXT NOT NULL,
                    covers_through_sequence INTEGER NOT NULL,
                    token_count INTEGER NOT NULL,
                    created_at TEXT NOT NULL
                );

                CREATE INDEX IF NOT EXISTS idx_session_summaries_session_kind
                    ON session_summaries(session_id, kind);
                CREATE INDEX IF NOT EXISTS idx_session_summaries_session_latest
                    ON session_summaries(session_id, kind, created_at DESC);",
            )?;
            tracing::info!("V30 migration complete: session_summaries table");
            self.conn.execute("PRAGMA user_version = 30", [])?;
        }

        // V31: Retry tracking columns
        if version < 31 {
            self.add_column_if_not_exists("sessions", "retry_attempt", "INTEGER")?;
            self.add_column_if_not_exists("sessions", "max_retries", "INTEGER")?;
            tracing::info!("V31 migration complete: added retry_attempt/max_retries to sessions");
            self.conn.execute("PRAGMA user_version = 31", [])?;
        }

        // V32: Observations table for dream consolidation (Dreamer system)
        if version < 32 {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS observations (
                    id              TEXT PRIMARY KEY,
                    session_id      TEXT NOT NULL,
                    project_id      TEXT,
                    level           TEXT NOT NULL DEFAULT 'explicit',
                    content         TEXT NOT NULL,
                    source_ids      TEXT DEFAULT '[]',
                    confidence      TEXT,
                    times_derived   INTEGER NOT NULL DEFAULT 1,
                    deleted_at      TEXT,
                    created_at      TEXT NOT NULL,
                    updated_at      TEXT NOT NULL
                );

                CREATE INDEX IF NOT EXISTS idx_observations_session ON observations(session_id);
                CREATE INDEX IF NOT EXISTS idx_observations_project ON observations(project_id);
                CREATE INDEX IF NOT EXISTS idx_observations_level ON observations(level);
                CREATE INDEX IF NOT EXISTS idx_observations_deleted ON observations(deleted_at);
                CREATE INDEX IF NOT EXISTS idx_observations_created ON observations(created_at);

                CREATE TABLE IF NOT EXISTS dream_state (
                    key             TEXT PRIMARY KEY,
                    value           TEXT NOT NULL,
                    updated_at      TEXT NOT NULL
                );",
            )?;
            tracing::info!("V32 migration complete: observations + dream_state tables");
            self.conn.execute("PRAGMA user_version = 32", [])?;
        }

        // V33: Issue tracker integration
        if version < 33 {
            self.add_column_if_not_exists("sessions", "issue_identifier", "TEXT")?;
            self.add_column_if_not_exists("sessions", "issue_url", "TEXT")?;
            self.add_column_if_not_exists("sessions", "issue_tracker_id", "TEXT")?;

            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS issue_tracker_dispatches (
                    issue_id TEXT NOT NULL PRIMARY KEY,
                    issue_identifier TEXT NOT NULL,
                    tracker TEXT NOT NULL DEFAULT 'linear',
                    session_id TEXT NOT NULL,
                    dispatched_at TEXT NOT NULL,
                    last_reconciled_at TEXT,
                    terminal_state TEXT,
                    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000000Z', 'now'))
                );

                CREATE INDEX IF NOT EXISTS idx_issue_dispatches_session
                    ON issue_tracker_dispatches(session_id);
                CREATE INDEX IF NOT EXISTS idx_issue_dispatches_tracker
                    ON issue_tracker_dispatches(tracker);",
            )?;

            tracing::info!("V33 migration complete: issue tracker columns + dispatch table");
            self.conn.execute("PRAGMA user_version = 33", [])?;
        }

        // V34: Lifecycle hooks, tool permissions, and context compression
        if version < 34 {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS permission_rules (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    tool_pattern TEXT NOT NULL,
                    action TEXT NOT NULL DEFAULT 'Ask',
                    scope TEXT NOT NULL DEFAULT 'global',
                    scope_id TEXT,
                    priority INTEGER NOT NULL DEFAULT 0,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_perm_rules_scope
                    ON permission_rules(scope, scope_id);

                CREATE TABLE IF NOT EXISTS offloaded_content (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_id TEXT NOT NULL,
                    event_sequence INTEGER NOT NULL,
                    content_hash TEXT NOT NULL,
                    original_content TEXT NOT NULL,
                    byte_size INTEGER NOT NULL,
                    created_at TEXT NOT NULL,
                    UNIQUE(session_id, event_sequence)
                );
                CREATE INDEX IF NOT EXISTS idx_offload_session_seq
                    ON offloaded_content(session_id, event_sequence);",
            )?;
            tracing::info!("V34 migration complete: permission_rules + offloaded_content tables");
            self.conn.execute("PRAGMA user_version = 34", [])?;
        }

        // V35: Scheduled jobs
        if version < 35 {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS scheduled_jobs (
                    id              TEXT PRIMARY KEY,
                    name            TEXT NOT NULL,
                    message         TEXT NOT NULL,
                    schedule_json   TEXT NOT NULL,
                    last_fired_at   TEXT,
                    next_fire_at    TEXT NOT NULL,
                    enabled         INTEGER NOT NULL DEFAULT 1,
                    working_dir     TEXT,
                    provider        TEXT,
                    model           TEXT,
                    project_id      TEXT,
                    created_at      TEXT NOT NULL,
                    updated_at      TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_scheduled_jobs_enabled_next
                    ON scheduled_jobs(enabled, next_fire_at)
                    WHERE enabled = 1;
                CREATE INDEX IF NOT EXISTS idx_scheduled_jobs_project
                    ON scheduled_jobs(project_id);",
            )?;
            tracing::info!("V35 migration complete: scheduled_jobs table");
            self.conn.pragma_update(None, "user_version", 35)?;
        }

        // V36: Add scheduled_job_id to sessions
        if version < 36 {
            self.add_column_if_not_exists("sessions", "scheduled_job_id", "TEXT")?;
            tracing::info!("V36 migration complete: sessions.scheduled_job_id");
            self.conn.pragma_update(None, "user_version", 36)?;
        }

        // V37: Compiled prompts
        if version < 37 {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS compiled_prompts (
                    id              TEXT PRIMARY KEY,
                    session_id      TEXT,
                    original_input  TEXT NOT NULL,
                    compiled_output TEXT NOT NULL,
                    contract_status TEXT NOT NULL,
                    layer_semantic  INTEGER NOT NULL DEFAULT 0,
                    layer_syntactic INTEGER NOT NULL DEFAULT 0,
                    layer_deictic   INTEGER NOT NULL DEFAULT 0,
                    layer_discourse INTEGER NOT NULL DEFAULT 0,
                    layer_pragmatic INTEGER NOT NULL DEFAULT 0,
                    accepted        INTEGER NOT NULL,
                    created_at      TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_compiled_prompts_session
                    ON compiled_prompts(session_id);
                CREATE INDEX IF NOT EXISTS idx_compiled_prompts_created
                    ON compiled_prompts(created_at DESC);",
            )?;
            tracing::info!("V37 migration complete: compiled_prompts table");
            self.conn.pragma_update(None, "user_version", 37)?;
        }

        // V38: Session rating + harness versioning + outcome proxies
        if version < 38 {
            self.add_column_if_not_exists("sessions", "rating", "INTEGER")?;
            self.add_column_if_not_exists("sessions", "harness_version_hash", "TEXT")?;
            self.add_column_if_not_exists("sessions", "test_passed", "INTEGER")?;
            self.add_column_if_not_exists("sessions", "clippy_passed", "INTEGER")?;
            self.add_column_if_not_exists("sessions", "turn_count", "INTEGER")?;
            self.add_column_if_not_exists("sessions", "retry_count", "INTEGER")?;
            tracing::info!(
                "V38 migration complete: session rating, harness versioning, outcome proxies"
            );
            self.conn.pragma_update(None, "user_version", 38)?;
        }

        // V39: Agent sandbox columns (filesystem-only; process isolation reserved).
        if version < 39 {
            self.add_column_if_not_exists("sessions", "sandbox_kind", "TEXT")?;
            self.add_column_if_not_exists("sessions", "sandbox_root", "TEXT")?;
            self.add_column_if_not_exists("sessions", "sandbox_branch", "TEXT")?;
            self.add_column_if_not_exists("sessions", "sandbox_cleanup_state", "TEXT")?;
            tracing::info!("V39 migration complete: agent sandbox columns");
            self.conn.pragma_update(None, "user_version", 39)?;
        }

        // V40: Hierarchical parent_id (organizational tree: Group/Epic containers).
        if version < 40 {
            self.add_column_if_not_exists("sessions", "parent_id", "TEXT")?;
            self.conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_sessions_parent_id ON sessions(parent_id)",
                [],
            )?;
            tracing::info!("V40 migration complete: hierarchical parent_id column + index");
            self.conn.pragma_update(None, "user_version", 40)?;
        }

        // V41: WaitingApproval duration accumulator (server-side telemetry correctness).
        // Nullable for symmetry with V38 outcome proxies: None = unmeasured (pre-V41 row
        // or session that never reached finalize), Some(0) = measured-but-never-waited.
        if version < 41 {
            self.add_column_if_not_exists("sessions", "approval_wait_ms", "INTEGER")?;
            tracing::info!("V41 migration complete: approval_wait_ms column");
            self.conn.pragma_update(None, "user_version", 41)?;
        }

        // V42: Lead/orchestrator pointer on Group/Epic container rows.
        // Points at a leaf child (parent_id == self.id) auto-promoted to lead.
        // Index supports rotation lead-inherit lookup (find_epics_by_lead).
        if version < 42 {
            self.add_column_if_not_exists("sessions", "lead_session_id", "TEXT")?;
            self.conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_sessions_lead_session_id ON sessions(lead_session_id)",
                [],
            )?;
            tracing::info!("V42 migration complete: lead_session_id column + index");
            self.conn.pragma_update(None, "user_version", 42)?;
        }

        // V43: Multi-tag system + DB-stored named topology templates.
        //   - sessions.tag: fallback single-tag column for legacy/migration compat
        //     (multi-tag lives in session_tags; this column carries the canonical
        //     "primary" tag for legacy single-tag callsites).
        //   - session_tags: join table for the multi-tag rollout (P1.5 owns inserts).
        //   - topologies: DB-stored named templates (P1.4 owns inserts/CRUD).
        // All DDL is idempotent (CREATE ... IF NOT EXISTS / pragma_table_info gate
        // in add_column_if_not_exists). Foreign-key clauses are documentation-grade
        // until a future ticket enables PRAGMA foreign_keys = ON.
        if version < 43 {
            self.add_column_if_not_exists("sessions", "tag", "TEXT NOT NULL DEFAULT ''")?;
            self.conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_sessions_tag ON sessions(tag)",
                [],
            )?;

            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS session_tags (
                    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                    tag        TEXT NOT NULL,
                    PRIMARY KEY (session_id, tag)
                );

                CREATE INDEX IF NOT EXISTS idx_session_tags_tag ON session_tags(tag);

                CREATE TABLE IF NOT EXISTS topologies (
                    id              TEXT PRIMARY KEY,
                    name            TEXT NOT NULL UNIQUE,
                    definition_json TEXT NOT NULL,
                    created_at      TEXT NOT NULL,
                    updated_at      TEXT NOT NULL
                );

                CREATE INDEX IF NOT EXISTS idx_topologies_name ON topologies(name);",
            )?;

            tracing::info!(
                "V43 migration complete: sessions.tag column + idx_sessions_tag, \
                 session_tags table + index, topologies table + index"
            );
            self.conn.pragma_update(None, "user_version", 43)?;
        }

        // V44: Eval-session tagging (RSI-006).
        // `is_eval=1` rows are excluded from production analytics and from default
        // `ListSessions` projections. Default 0 keeps every pre-V44 row in the
        // production set without backfill. Boolean encoded as INTEGER per SQLite
        // convention; matches the `pending_archive`, `test_passed`, `clippy_passed`
        // pattern.
        if version < 44 {
            self.add_column_if_not_exists("sessions", "is_eval", "INTEGER NOT NULL DEFAULT 0")?;
            self.conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_sessions_is_eval ON sessions(is_eval)",
                [],
            )?;
            tracing::info!("V44 migration complete: is_eval column + idx_sessions_is_eval");
            self.conn.pragma_update(None, "user_version", 44)?;
        }

        // V45: Capability class for routing enforcement (RSI-010).
        // Nullable TEXT column; pre-V45 rows deserialize to `None`.
        // (Originally written as V41 in PR #13; renumbered during 2026-05-13 cherry-pick replay
        // because V41–V44 were taken by main between PR #13 author time and replay.)
        if version < 45 {
            self.add_column_if_not_exists("sessions", "capability_class", "TEXT")?;
            tracing::info!("V45 migration complete: capability_class column (RSI-010)");
            self.conn.pragma_update(None, "user_version", 45)?;
        }

        // V46: Master-Improve chain iteration metadata (one row per iteration).
        // (Originally written as V45 in PR #10; renumbered during 2026-05-15 merge
        // because PR #18 took V45 for capability_class.)
        if version < 46 {
            self.conn.execute(
                "CREATE TABLE IF NOT EXISTS chain_iterations (
                    chain_id            TEXT NOT NULL,
                    iteration_index     INTEGER NOT NULL,
                    parent_execution_id TEXT,
                    child_execution_id  TEXT NOT NULL,
                    halt_reason         TEXT,
                    goal_text           TEXT NOT NULL,
                    refined_goal_text   TEXT,
                    token_count         INTEGER,
                    pre_failure_count   INTEGER,
                    post_failure_count  INTEGER,
                    cap                 INTEGER NOT NULL,
                    started_at          TEXT NOT NULL,
                    ended_at            TEXT,
                    PRIMARY KEY (chain_id, iteration_index)
                )",
                [],
            )?;
            self.conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_chain_iterations_child_exec
                    ON chain_iterations(child_execution_id)",
                [],
            )?;
            self.conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_chain_iterations_chain_active
                    ON chain_iterations(chain_id, halt_reason)
                    WHERE halt_reason IS NULL",
                [],
            )?;
            tracing::info!("V46 migration complete: chain_iterations table");
            self.conn.pragma_update(None, "user_version", 46)?;
        }

        if version < 47 {
            self.add_column_if_not_exists("sessions", "topology_node_id", "TEXT")?;
            self.add_column_if_not_exists(
                "sessions",
                "topology_iteration",
                "INTEGER NOT NULL DEFAULT 0",
            )?;
            self.conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_sessions_topology_node \
                 ON sessions(parent_id, topology_node_id)",
                [],
            )?;
            tracing::info!("V47 migration complete: topology_node_id + topology_iteration columns");
            self.conn.pragma_update(None, "user_version", 47)?;
        }

        // V48: Daemon settings key-value table (RSI-026).
        // Authoritative store for global daemon-owned settings that previously
        // lived in ~/.rsi/state.json. First inhabitant: system_prompt_preset.
        // Future single-value daemon settings reuse this table by string key.
        if version < 48 {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS daemon_settings (
                    key        TEXT PRIMARY KEY,
                    value      TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );",
            )?;
            tracing::info!("V48 migration complete: daemon_settings key-value table (RSI-026)");
            self.conn.pragma_update(None, "user_version", 48)?;
        }

        // V49: Recursive DAG persistence foundation.
        //
        // SQLite constraints cover row-local shape, cheap uniqueness, and safe
        // references. Service/store validation remains authoritative for
        // aggregate invariants SQLite cannot enforce: exactly one root,
        // root_task_id matching the root node, acyclicity across all edge
        // kinds, parent_task_id/parent_child edge mirroring, same-graph edge
        // endpoints, child depth/scope rules, injection batch completeness,
        // lifecycle transition legality, retry budgets, recovery semantics, and
        // nonterminal transient work reconciliation.
        if version < 49 {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS recursive_task_graphs (
                    id TEXT PRIMARY KEY,
                    root_task_id TEXT NOT NULL,
                    title TEXT NOT NULL CHECK (length(trim(title)) > 0),
                    objective TEXT NOT NULL CHECK (length(trim(objective)) > 0),
                    status TEXT NOT NULL CHECK (status IN (
                        'active', 'terminal', 'blocked', 'failed', 'cancelled', 'malformed'
                    )),
                    project_id TEXT REFERENCES projects(id),
                    workflow_id TEXT REFERENCES workflows(id),
                    topology_id TEXT REFERENCES topologies(id),
                    parent_session_id TEXT REFERENCES sessions(id),
                    source_execution_id TEXT,
                    source_eval_id TEXT,
                    execution_mode TEXT NOT NULL DEFAULT 'fake' CHECK (
                        execution_mode IN ('fake', 'live_session')
                    ),
                    max_depth INTEGER NOT NULL CHECK (max_depth >= 0),
                    max_fanout INTEGER NOT NULL CHECK (max_fanout > 0),
                    max_descendants INTEGER NOT NULL CHECK (max_descendants > 0),
                    step_limit INTEGER NOT NULL CHECK (step_limit > 0),
                    last_stop_reason TEXT,
                    malformed_reason TEXT,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    recovered_at TEXT
                );

                CREATE TABLE IF NOT EXISTS recursive_task_nodes (
                    id TEXT PRIMARY KEY,
                    graph_id TEXT NOT NULL REFERENCES recursive_task_graphs(id) ON DELETE CASCADE,
                    parent_task_id TEXT REFERENCES recursive_task_nodes(id),
                    title TEXT NOT NULL CHECK (length(trim(title)) > 0),
                    objective TEXT NOT NULL CHECK (length(trim(objective)) > 0),
                    scope TEXT NOT NULL CHECK (length(trim(scope)) > 0),
                    acceptance_criteria_json TEXT NOT NULL CHECK (json_valid(acceptance_criteria_json)),
                    depth INTEGER NOT NULL CHECK (depth >= 0),
                    scope_units INTEGER NOT NULL CHECK (scope_units > 0),
                    max_retries INTEGER NOT NULL CHECK (max_retries >= 0),
                    status TEXT NOT NULL CHECK (status IN (
                        'pending', 'planning', 'ready', 'running', 'decomposed',
                        'blocked_on_children', 'integrating', 'verifying',
                        'succeeded', 'failed', 'blocked', 'cancelled'
                    )),
                    decomposed_once INTEGER NOT NULL DEFAULT 0 CHECK (decomposed_once IN (0, 1)),
                    integration_strategy TEXT,
                    verification_strategy TEXT,
                    blocked_reason TEXT,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS recursive_task_edges (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    graph_id TEXT NOT NULL REFERENCES recursive_task_graphs(id) ON DELETE CASCADE,
                    from_task_id TEXT NOT NULL REFERENCES recursive_task_nodes(id) ON DELETE CASCADE,
                    to_task_id TEXT NOT NULL REFERENCES recursive_task_nodes(id) ON DELETE CASCADE,
                    kind TEXT NOT NULL CHECK (kind IN ('parent_child', 'dependency')),
                    injection_batch_id TEXT REFERENCES recursive_injection_batches(id),
                    created_at TEXT NOT NULL,
                    CHECK (from_task_id <> to_task_id),
                    UNIQUE (graph_id, from_task_id, to_task_id, kind)
                );

                CREATE TABLE IF NOT EXISTS recursive_task_attempts (
                    id TEXT PRIMARY KEY,
                    graph_id TEXT NOT NULL REFERENCES recursive_task_graphs(id) ON DELETE CASCADE,
                    task_id TEXT NOT NULL REFERENCES recursive_task_nodes(id) ON DELETE CASCADE,
                    phase TEXT NOT NULL CHECK (phase IN ('execute', 'integrate')),
                    attempt_no INTEGER NOT NULL CHECK (attempt_no > 0),
                    retry_count INTEGER NOT NULL CHECK (retry_count >= 0),
                    status TEXT NOT NULL CHECK (status IN (
                        'running', 'succeeded', 'decomposed', 'failed',
                        'blocked', 'cancelled', 'interrupted'
                    )),
                    started_at TEXT NOT NULL,
                    finished_at TEXT,
                    failure_reason TEXT,
                    block_reason TEXT,
                    dependency_snapshot_json TEXT NOT NULL CHECK (json_valid(dependency_snapshot_json)),
                    executor_kind TEXT NOT NULL DEFAULT 'fake' CHECK (
                        executor_kind IN ('fake', 'live_session')
                    ),
                    session_id TEXT REFERENCES sessions(id),
                    workflow_execution_id TEXT,
                    CHECK (
                        (status = 'running' AND finished_at IS NULL)
                        OR (status <> 'running' AND finished_at IS NOT NULL)
                    ),
                    UNIQUE (task_id, phase, attempt_no)
                );

                CREATE TABLE IF NOT EXISTS recursive_injection_batches (
                    id TEXT PRIMARY KEY,
                    graph_id TEXT NOT NULL REFERENCES recursive_task_graphs(id) ON DELETE CASCADE,
                    parent_task_id TEXT NOT NULL REFERENCES recursive_task_nodes(id) ON DELETE CASCADE,
                    attempt_id TEXT REFERENCES recursive_task_attempts(id),
                    reason_for_decomposition TEXT NOT NULL,
                    child_task_ids_json TEXT NOT NULL CHECK (json_valid(child_task_ids_json)),
                    edge_count INTEGER NOT NULL CHECK (edge_count > 0),
                    committed_at TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS recursive_lifecycle_events (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    graph_id TEXT NOT NULL REFERENCES recursive_task_graphs(id) ON DELETE CASCADE,
                    task_id TEXT NOT NULL REFERENCES recursive_task_nodes(id) ON DELETE CASCADE,
                    from_status TEXT NOT NULL CHECK (from_status IN (
                        'pending', 'planning', 'ready', 'running', 'decomposed',
                        'blocked_on_children', 'integrating', 'verifying',
                        'succeeded', 'failed', 'blocked', 'cancelled'
                    )),
                    to_status TEXT NOT NULL CHECK (to_status IN (
                        'pending', 'planning', 'ready', 'running', 'decomposed',
                        'blocked_on_children', 'integrating', 'verifying',
                        'succeeded', 'failed', 'blocked', 'cancelled'
                    )),
                    reason TEXT,
                    created_at TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS recursive_execution_artifacts (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    graph_id TEXT NOT NULL REFERENCES recursive_task_graphs(id) ON DELETE CASCADE,
                    task_id TEXT NOT NULL REFERENCES recursive_task_nodes(id) ON DELETE CASCADE,
                    attempt_id TEXT REFERENCES recursive_task_attempts(id) ON DELETE CASCADE,
                    kind TEXT NOT NULL CHECK (kind IN (
                        'inline', 'file', 'session_event', 'workflow_execution'
                    )),
                    label TEXT NOT NULL CHECK (length(trim(label)) > 0),
                    content TEXT,
                    uri TEXT,
                    metadata_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(metadata_json)),
                    created_at TEXT NOT NULL
                );

                CREATE UNIQUE INDEX IF NOT EXISTS idx_recursive_graph_root
                    ON recursive_task_graphs(id, root_task_id);
                CREATE INDEX IF NOT EXISTS idx_recursive_graph_project
                    ON recursive_task_graphs(project_id, updated_at DESC);
                CREATE INDEX IF NOT EXISTS idx_recursive_graph_workflow
                    ON recursive_task_graphs(workflow_id, updated_at DESC);
                CREATE INDEX IF NOT EXISTS idx_recursive_graph_topology
                    ON recursive_task_graphs(topology_id, updated_at DESC);
                CREATE INDEX IF NOT EXISTS idx_recursive_graph_status
                    ON recursive_task_graphs(status, updated_at DESC);

                CREATE INDEX IF NOT EXISTS idx_recursive_nodes_graph_status
                    ON recursive_task_nodes(graph_id, status, updated_at);
                CREATE INDEX IF NOT EXISTS idx_recursive_nodes_parent
                    ON recursive_task_nodes(graph_id, parent_task_id);
                CREATE INDEX IF NOT EXISTS idx_recursive_nodes_depth
                    ON recursive_task_nodes(graph_id, depth);

                CREATE INDEX IF NOT EXISTS idx_recursive_edges_from
                    ON recursive_task_edges(graph_id, from_task_id, kind);
                CREATE INDEX IF NOT EXISTS idx_recursive_edges_to
                    ON recursive_task_edges(graph_id, to_task_id, kind);
                CREATE UNIQUE INDEX IF NOT EXISTS idx_recursive_one_parent_edge
                    ON recursive_task_edges(graph_id, to_task_id)
                    WHERE kind = 'parent_child';

                CREATE INDEX IF NOT EXISTS idx_recursive_attempts_task_phase
                    ON recursive_task_attempts(task_id, phase, attempt_no);
                CREATE INDEX IF NOT EXISTS idx_recursive_attempts_running
                    ON recursive_task_attempts(graph_id, status, started_at)
                    WHERE status = 'running';
                CREATE INDEX IF NOT EXISTS idx_recursive_attempts_session
                    ON recursive_task_attempts(session_id)
                    WHERE session_id IS NOT NULL;

                CREATE INDEX IF NOT EXISTS idx_recursive_injections_parent
                    ON recursive_injection_batches(parent_task_id, committed_at);
                CREATE INDEX IF NOT EXISTS idx_recursive_events_task
                    ON recursive_lifecycle_events(task_id, id);
                CREATE INDEX IF NOT EXISTS idx_recursive_events_graph
                    ON recursive_lifecycle_events(graph_id, id);
                CREATE INDEX IF NOT EXISTS idx_recursive_artifacts_task
                    ON recursive_execution_artifacts(task_id, created_at);
                CREATE INDEX IF NOT EXISTS idx_recursive_artifacts_attempt
                    ON recursive_execution_artifacts(attempt_id);",
            )?;
            tracing::info!("V49 migration complete: recursive DAG persistence foundation");
            self.conn.pragma_update(None, "user_version", 49)?;
        }

        // V50: Recursive DAG recovery quarantine metadata.
        //
        // Quarantined graphs remain inspectable through read models but are
        // excluded from normal recursive DAG write APIs. `recovered_at` existed
        // in V49; V50 adds explicit quarantine and last effective recovery
        // check evidence without broadening the mutation surface.
        if version < 50 {
            self.add_column_if_not_exists("recursive_task_graphs", "quarantined_at", "TEXT")?;
            self.add_column_if_not_exists("recursive_task_graphs", "quarantine_reason", "TEXT")?;
            self.add_column_if_not_exists("recursive_task_graphs", "recovery_checked_at", "TEXT")?;
            self.conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_recursive_graph_quarantine
                    ON recursive_task_graphs(quarantined_at, updated_at DESC)
                    WHERE quarantined_at IS NOT NULL",
                [],
            )?;
            tracing::info!("V50 migration complete: recursive DAG quarantine metadata");
            self.conn.pragma_update(None, "user_version", 50)?;
        }

        // V51: Recursive DAG scheduler run records.
        //
        // This is Phase 5A.1's durable invocation audit. It records explicit
        // fake scheduler runs and links completed runs to their
        // `scheduler-report` artifact without adding cancellation,
        // leases/concurrency enforcement, recovery passes, RPC controls, or
        // background scheduling.
        if version < 51 {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS recursive_scheduler_runs (
                    id TEXT PRIMARY KEY,
                    graph_id TEXT NOT NULL REFERENCES recursive_task_graphs(id) ON DELETE CASCADE,
                    status TEXT NOT NULL CHECK (status IN (
                        'running', 'cancelling', 'completed', 'cancelled',
                        'failed', 'rejected', 'lease_expired'
                    )),
                    source TEXT NOT NULL CHECK (source IN (
                        'test_harness', 'manual_rpc', 'startup_recovery', 'future_daemon_loop'
                    )),
                    operator TEXT,
                    started_at TEXT NOT NULL,
                    completed_at TEXT,
                    stop_reason TEXT CHECK (
                        stop_reason IS NULL OR stop_reason IN (
                            'graph_terminal', 'idle_no_runnable', 'step_limit_exceeded',
                            'partial_failure', 'quarantined', 'cancellation_requested',
                            'lease_expired', 'executor_error', 'recovery_deferred'
                        )
                    ),
                    step_count INTEGER NOT NULL DEFAULT 0 CHECK (step_count >= 0),
                    max_steps INTEGER NOT NULL CHECK (max_steps > 0),
                    executor_mode TEXT NOT NULL DEFAULT 'fake' CHECK (
                        executor_mode IN ('fake', 'live_session')
                    ),
                    failure_reason TEXT,
                    report_artifact_id INTEGER REFERENCES recursive_execution_artifacts(id),
                    CHECK (
                        (status IN ('running', 'cancelling') AND completed_at IS NULL)
                        OR (status NOT IN ('running', 'cancelling') AND completed_at IS NOT NULL)
                    )
                );

                CREATE INDEX IF NOT EXISTS idx_recursive_scheduler_runs_graph_started
                    ON recursive_scheduler_runs(graph_id, started_at DESC);
                CREATE INDEX IF NOT EXISTS idx_recursive_scheduler_runs_status
                    ON recursive_scheduler_runs(status, started_at DESC);

                CREATE TABLE IF NOT EXISTS recursive_scheduler_run_events (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    run_id TEXT NOT NULL REFERENCES recursive_scheduler_runs(id) ON DELETE CASCADE,
                    graph_id TEXT NOT NULL REFERENCES recursive_task_graphs(id) ON DELETE CASCADE,
                    event_type TEXT NOT NULL,
                    message TEXT,
                    metadata_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(metadata_json)),
                    created_at TEXT NOT NULL
                );

                CREATE INDEX IF NOT EXISTS idx_recursive_scheduler_run_events_run
                    ON recursive_scheduler_run_events(run_id, id);",
            )?;
            tracing::info!("V51 migration complete: recursive DAG scheduler run records");
            self.conn.pragma_update(None, "user_version", 51)?;
        }

        // V52: Recursive DAG cancellation requests.
        //
        // This is Phase 5A.2's durable cancellation control plane for the
        // fake scheduler. It models graph/run/task scopes, records request
        // observation/application, and links cancelled scheduler runs back to
        // the request. It does not add leases, RPCs, TUI surfaces, live
        // interrupt handles, or a background executor loop.
        if version < 52 {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS recursive_cancellation_requests (
                    id TEXT PRIMARY KEY,
                    graph_id TEXT NOT NULL REFERENCES recursive_task_graphs(id) ON DELETE CASCADE,
                    run_id TEXT REFERENCES recursive_scheduler_runs(id) ON DELETE CASCADE,
                    task_id TEXT REFERENCES recursive_task_nodes(id) ON DELETE CASCADE,
                    scope TEXT NOT NULL CHECK (scope IN ('graph', 'run', 'task')),
                    status TEXT NOT NULL CHECK (status IN (
                        'requested', 'observed', 'applied', 'rejected'
                    )),
                    source TEXT NOT NULL CHECK (source IN (
                        'manual_rpc', 'test_harness', 'startup_recovery'
                    )),
                    reason TEXT NOT NULL CHECK (length(trim(reason)) > 0),
                    requested_by TEXT,
                    requested_at TEXT NOT NULL,
                    observed_at TEXT,
                    applied_at TEXT,
                    rejection_reason TEXT,
                    CHECK (
                        (scope = 'graph' AND run_id IS NULL AND task_id IS NULL)
                        OR (scope = 'run' AND run_id IS NOT NULL AND task_id IS NULL)
                        OR (scope = 'task' AND run_id IS NULL AND task_id IS NOT NULL)
                    ),
                    CHECK (status != 'observed' OR observed_at IS NOT NULL),
                    CHECK (status != 'applied' OR applied_at IS NOT NULL),
                    CHECK (status != 'rejected' OR rejection_reason IS NOT NULL)
                );

                CREATE INDEX IF NOT EXISTS idx_recursive_cancel_graph_open
                    ON recursive_cancellation_requests(graph_id, requested_at, id)
                    WHERE status IN ('requested', 'observed');
                CREATE INDEX IF NOT EXISTS idx_recursive_cancel_run_open
                    ON recursive_cancellation_requests(run_id, requested_at, id)
                    WHERE status IN ('requested', 'observed');
                CREATE INDEX IF NOT EXISTS idx_recursive_cancel_task_open
                    ON recursive_cancellation_requests(graph_id, task_id, requested_at, id)
                    WHERE status IN ('requested', 'observed');",
            )?;
            self.add_column_if_not_exists(
                "recursive_scheduler_runs",
                "cancellation_request_id",
                "TEXT REFERENCES recursive_cancellation_requests(id)",
            )?;
            self.add_column_if_not_exists(
                "recursive_scheduler_runs",
                "cancellation_reason",
                "TEXT",
            )?;
            tracing::info!("V52 migration complete: recursive DAG cancellation requests");
            self.conn.pragma_update(None, "user_version", 52)?;
        }

        // V53: Recursive DAG scheduler leases.
        //
        // This is Phase 5A.3's per-graph scheduler lease foundation. It adds
        // durable lease metadata, expires any pre-lease active runs as stale
        // crash leftovers, and enforces at most one active scheduler run per
        // graph. Global concurrency caps are enforced by the store API because
        // they are policy, not a fixed schema invariant.
        if version < 53 {
            self.add_column_if_not_exists("recursive_scheduler_runs", "lease_owner", "TEXT")?;
            self.add_column_if_not_exists("recursive_scheduler_runs", "lease_token", "TEXT")?;
            self.add_column_if_not_exists(
                "recursive_scheduler_runs",
                "lease_heartbeat_at",
                "TEXT",
            )?;
            self.add_column_if_not_exists("recursive_scheduler_runs", "lease_expires_at", "TEXT")?;

            let migrated_at =
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
            self.conn.execute(
                "UPDATE recursive_scheduler_runs
                 SET status = 'lease_expired',
                     completed_at = COALESCE(completed_at, ?1),
                     stop_reason = 'lease_expired',
                     failure_reason = COALESCE(
                         failure_reason,
                         'recursive scheduler active run expired during V53 lease migration'
                     )
                 WHERE status IN ('running', 'cancelling')",
                params![migrated_at],
            )?;

            self.conn.execute_batch(
                "CREATE UNIQUE INDEX IF NOT EXISTS idx_recursive_scheduler_runs_one_active_per_graph
                    ON recursive_scheduler_runs(graph_id)
                    WHERE status IN ('running', 'cancelling');
                CREATE INDEX IF NOT EXISTS idx_recursive_scheduler_runs_active_lease
                    ON recursive_scheduler_runs(status, lease_expires_at, started_at)
                    WHERE status IN ('running', 'cancelling');
                CREATE INDEX IF NOT EXISTS idx_recursive_scheduler_runs_lease_owner
                    ON recursive_scheduler_runs(lease_owner, status, lease_expires_at)
                    WHERE lease_owner IS NOT NULL;",
            )?;
            tracing::info!("V53 migration complete: recursive DAG scheduler leases");
            self.conn.pragma_update(None, "user_version", 53)?;
        }

        // V54: Recursive DAG budgeted recovery passes.
        //
        // Phase 5A.4 makes daemon-startup recovery bounded and inspectable.
        // Recovery remains explicit and synchronous; this schema records pass
        // budgets, counters, stop reasons, and the graph-level deferred state
        // needed for later operator readback without adding a background loop.
        if version < 54 {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS recursive_recovery_passes (
                    id TEXT PRIMARY KEY,
                    source TEXT NOT NULL CHECK (source IN (
                        'startup', 'manual_rpc', 'test_harness'
                    )),
                    status TEXT NOT NULL CHECK (status IN (
                        'running', 'completed', 'deferred', 'failed'
                    )),
                    started_at TEXT NOT NULL,
                    completed_at TEXT,
                    max_graphs INTEGER NOT NULL CHECK (max_graphs > 0),
                    time_budget_ms INTEGER CHECK (
                        time_budget_ms IS NULL OR time_budget_ms >= 0
                    ),
                    checked INTEGER NOT NULL DEFAULT 0 CHECK (checked >= 0),
                    recovered INTEGER NOT NULL DEFAULT 0 CHECK (recovered >= 0),
                    quarantined INTEGER NOT NULL DEFAULT 0 CHECK (quarantined >= 0),
                    deferred INTEGER NOT NULL DEFAULT 0 CHECK (deferred >= 0),
                    skipped INTEGER NOT NULL DEFAULT 0 CHECK (skipped >= 0),
                    errors INTEGER NOT NULL DEFAULT 0 CHECK (errors >= 0),
                    last_graph_id TEXT,
                    stop_reason TEXT CHECK (
                        stop_reason IS NULL OR stop_reason IN (
                            'completed', 'max_graphs', 'time_budget', 'store_error'
                        )
                    ),
                    error TEXT,
                    CHECK (
                        (status = 'running' AND completed_at IS NULL)
                        OR (status <> 'running' AND completed_at IS NOT NULL)
                    )
                );

                CREATE INDEX IF NOT EXISTS idx_recursive_recovery_passes_started
                    ON recursive_recovery_passes(started_at DESC, id DESC);
                CREATE INDEX IF NOT EXISTS idx_recursive_recovery_passes_status
                    ON recursive_recovery_passes(status, started_at DESC);

                CREATE TABLE IF NOT EXISTS recursive_recovery_deferred_graphs (
                    graph_id TEXT PRIMARY KEY REFERENCES recursive_task_graphs(id) ON DELETE CASCADE,
                    pass_id TEXT NOT NULL REFERENCES recursive_recovery_passes(id) ON DELETE CASCADE,
                    deferred_at TEXT NOT NULL,
                    reason TEXT NOT NULL CHECK (length(trim(reason)) > 0),
                    next_after TEXT,
                    last_attempted_at TEXT,
                    last_error TEXT
                );

                CREATE INDEX IF NOT EXISTS idx_recursive_recovery_deferred_due
                    ON recursive_recovery_deferred_graphs(next_after, deferred_at, graph_id);

                CREATE TABLE IF NOT EXISTS recursive_recovery_graph_states (
                    graph_id TEXT PRIMARY KEY REFERENCES recursive_task_graphs(id) ON DELETE CASCADE,
                    state TEXT NOT NULL CHECK (state IN (
                        'pending_recovery', 'recovered', 'quarantined', 'deferred'
                    )),
                    pass_id TEXT REFERENCES recursive_recovery_passes(id) ON DELETE SET NULL,
                    last_attempted_at TEXT,
                    completed_at TEXT,
                    deferred_at TEXT,
                    reason TEXT,
                    last_error TEXT,
                    updated_at TEXT NOT NULL
                );

                CREATE INDEX IF NOT EXISTS idx_recursive_recovery_graph_states_state
                    ON recursive_recovery_graph_states(state, updated_at DESC);",
            )?;
            tracing::info!("V54 migration complete: recursive DAG budgeted recovery passes");
            self.conn.pragma_update(None, "user_version", 54)?;
        }

        // V55: Recursive DAG live attempt/session correlation.
        //
        // Phase 6.1 is schema/read-model only. This records a durable
        // correlation between a future live recursive attempt and the
        // scheduler run, recursive attempt, optional RSI session, provider,
        // model, and sandbox snapshots. Graphs and scheduler runs remain
        // fake-only; this table does not launch sessions or enable live mode.
        if version < 55 {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS recursive_live_attempts (
                    id TEXT PRIMARY KEY,
                    graph_id TEXT NOT NULL REFERENCES recursive_task_graphs(id) ON DELETE CASCADE,
                    task_id TEXT NOT NULL REFERENCES recursive_task_nodes(id) ON DELETE CASCADE,
                    scheduler_run_id TEXT NOT NULL REFERENCES recursive_scheduler_runs(id) ON DELETE CASCADE,
                    attempt_id TEXT NOT NULL UNIQUE REFERENCES recursive_task_attempts(id) ON DELETE CASCADE,
                    phase TEXT NOT NULL CHECK (phase IN ('execute', 'integrate')),
                    attempt_no INTEGER NOT NULL CHECK (attempt_no > 0),
                    session_id TEXT REFERENCES sessions(id),
                    provider TEXT CHECK (
                        provider IS NULL OR provider IN (
                            'Claude', 'Codex', 'Local', 'Antigravity', 'Gemini', 'CodexAppServer', 'Harness'
                        )
                    ),
                    model TEXT CHECK (model IS NULL OR length(trim(model)) > 0),
                    sandbox_kind TEXT CHECK (
                        sandbox_kind IS NULL OR sandbox_kind IN ('None', 'GitWorktree')
                    ),
                    sandbox_root TEXT,
                    sandbox_branch TEXT,
                    sandbox_worktree_id TEXT,
                    workflow_id TEXT REFERENCES workflows(id),
                    topology_id TEXT REFERENCES topologies(id),
                    workflow_execution_id TEXT,
                    topology_workflow_id TEXT REFERENCES workflows(id),
                    execution_mode TEXT NOT NULL CHECK (execution_mode IN ('live_session')),
                    status TEXT NOT NULL CHECK (status IN (
                        'created', 'launching', 'running', 'waiting_approval',
                        'succeeded', 'decomposed', 'failed', 'blocked',
                        'cancelled', 'interrupted', 'lost', 'recovery_pending'
                    )),
                    recovery_status TEXT NOT NULL DEFAULT 'none' CHECK (
                        recovery_status IN ('none', 'pending', 'recovered', 'lost', 'quarantined')
                    ),
                    prompt_artifact_id INTEGER REFERENCES recursive_execution_artifacts(id),
                    output_artifact_id INTEGER REFERENCES recursive_execution_artifacts(id),
                    diff_artifact_id INTEGER REFERENCES recursive_execution_artifacts(id),
                    test_artifact_id INTEGER REFERENCES recursive_execution_artifacts(id),
                    cancellation_request_id TEXT REFERENCES recursive_cancellation_requests(id),
                    lease_owner TEXT,
                    lease_token TEXT,
                    heartbeat_at TEXT,
                    lease_expires_at TEXT,
                    max_wall_time_ms INTEGER CHECK (
                        max_wall_time_ms IS NULL OR max_wall_time_ms > 0
                    ),
                    created_at TEXT NOT NULL,
                    started_at TEXT,
                    launched_at TEXT,
                    completed_at TEXT,
                    recovery_checked_at TEXT,
                    recovered_at TEXT,
                    failure_reason TEXT,
                    interruption_reason TEXT,
                    cancellation_reason TEXT,
                    recovery_reason TEXT,
                    error TEXT,
                    updated_at TEXT NOT NULL,
                    CHECK (
                        (
                            status IN (
                                'succeeded', 'decomposed', 'failed', 'blocked',
                                'cancelled', 'interrupted', 'lost'
                            )
                            AND completed_at IS NOT NULL
                        )
                        OR (
                            status NOT IN (
                                'succeeded', 'decomposed', 'failed', 'blocked',
                                'cancelled', 'interrupted', 'lost'
                            )
                            AND completed_at IS NULL
                        )
                    ),
                    UNIQUE (graph_id, task_id, phase, attempt_no)
                );

                CREATE INDEX IF NOT EXISTS idx_recursive_live_attempts_graph
                    ON recursive_live_attempts(graph_id, created_at DESC, id);
                CREATE INDEX IF NOT EXISTS idx_recursive_live_attempts_task
                    ON recursive_live_attempts(task_id, created_at DESC, id);
                CREATE INDEX IF NOT EXISTS idx_recursive_live_attempts_run
                    ON recursive_live_attempts(scheduler_run_id, created_at DESC, id);
                CREATE INDEX IF NOT EXISTS idx_recursive_live_attempts_status
                    ON recursive_live_attempts(status, updated_at DESC, id);
                CREATE UNIQUE INDEX IF NOT EXISTS idx_recursive_live_attempts_session
                    ON recursive_live_attempts(session_id)
                    WHERE session_id IS NOT NULL;",
            )?;
            tracing::info!("V55 migration complete: recursive DAG live correlation schema");
            self.conn.pragma_update(None, "user_version", 55)?;
        }

        // V56: Recursive DAG recovery pass tables for time-budgeted recovery
        if version < 56 {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS recursive_recovery_passes (
                    id TEXT PRIMARY KEY,
                    source TEXT NOT NULL CHECK (source IN ('startup', 'manual_rpc', 'test_harness')),
                    status TEXT NOT NULL CHECK (status IN ('running', 'completed', 'deferred', 'failed')),
                    started_at TEXT NOT NULL,
                    completed_at TEXT,
                    max_graphs INTEGER NOT NULL CHECK (max_graphs > 0),
                    time_budget_ms INTEGER NOT NULL CHECK (time_budget_ms > 0),
                    checked INTEGER NOT NULL DEFAULT 0,
                    recovered INTEGER NOT NULL DEFAULT 0,
                    quarantined INTEGER NOT NULL DEFAULT 0,
                    deferred INTEGER NOT NULL DEFAULT 0,
                    last_graph_id TEXT,
                    stop_reason TEXT,
                    error TEXT
                );

                CREATE TABLE IF NOT EXISTS recursive_recovery_deferred_graphs (
                    graph_id TEXT PRIMARY KEY REFERENCES recursive_task_graphs(id) ON DELETE CASCADE,
                    pass_id TEXT NOT NULL REFERENCES recursive_recovery_passes(id) ON DELETE CASCADE,
                    deferred_at TEXT NOT NULL,
                    reason TEXT NOT NULL,
                    next_after TEXT,
                    last_error TEXT
                );

                CREATE INDEX IF NOT EXISTS idx_recursive_recovery_passes_source_started
                    ON recursive_recovery_passes(source, started_at DESC);
                CREATE INDEX IF NOT EXISTS idx_recursive_recovery_passes_status
                    ON recursive_recovery_passes(status, started_at DESC);
                CREATE INDEX IF NOT EXISTS idx_recursive_recovery_deferred_next_after
                    ON recursive_recovery_deferred_graphs(next_after, deferred_at)
                    WHERE next_after IS NOT NULL;",
            )?;
            tracing::info!("V56 migration complete: recursive DAG recovery pass tables");
            self.conn.pragma_update(None, "user_version", 56)?;
        }

        // V57: Recursive DAG live interrupt ownership.
        //
        // Phase 6.3 records one durable interrupt handle per live attempt. It
        // links graph/run cancellation requests to the live attempt/session
        // interrupt path, but does not expose RPC/TUI controls, add force-kill
        // escalation, heartbeat, crash recovery, or live scheduler reachability.
        if version < 57 {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS recursive_live_interrupts (
                    id TEXT PRIMARY KEY,
                    live_attempt_id TEXT NOT NULL UNIQUE REFERENCES recursive_live_attempts(id) ON DELETE CASCADE,
                    graph_id TEXT NOT NULL REFERENCES recursive_task_graphs(id) ON DELETE CASCADE,
                    task_id TEXT NOT NULL REFERENCES recursive_task_nodes(id) ON DELETE CASCADE,
                    scheduler_run_id TEXT NOT NULL REFERENCES recursive_scheduler_runs(id) ON DELETE CASCADE,
                    attempt_id TEXT NOT NULL REFERENCES recursive_task_attempts(id) ON DELETE CASCADE,
                    session_id TEXT REFERENCES sessions(id),
                    cancellation_request_id TEXT REFERENCES recursive_cancellation_requests(id),
                    status TEXT NOT NULL CHECK (status IN (
                        'requested', 'sent', 'interrupted', 'failed',
                        'rejected', 'ignored'
                    )),
                    reason TEXT NOT NULL CHECK (length(trim(reason)) > 0),
                    failure_reason TEXT,
                    requested_at TEXT NOT NULL,
                    sent_at TEXT,
                    completed_at TEXT,
                    CHECK (status != 'sent' OR sent_at IS NOT NULL),
                    CHECK (
                        (status IN ('interrupted', 'failed', 'rejected', 'ignored')
                            AND completed_at IS NOT NULL)
                        OR (status IN ('requested', 'sent') AND completed_at IS NULL)
                    ),
                    CHECK (
                        status NOT IN ('failed', 'rejected')
                        OR failure_reason IS NOT NULL
                    )
                );

                CREATE INDEX IF NOT EXISTS idx_recursive_live_interrupts_live_attempt
                    ON recursive_live_interrupts(live_attempt_id);
                CREATE INDEX IF NOT EXISTS idx_recursive_live_interrupts_status
                    ON recursive_live_interrupts(status, requested_at DESC, id);
                CREATE INDEX IF NOT EXISTS idx_recursive_live_interrupts_cancellation
                    ON recursive_live_interrupts(cancellation_request_id)
                    WHERE cancellation_request_id IS NOT NULL;
                CREATE INDEX IF NOT EXISTS idx_recursive_live_interrupts_session
                    ON recursive_live_interrupts(session_id)
                    WHERE session_id IS NOT NULL;",
            )?;
            tracing::info!("V57 migration complete: recursive DAG live interrupt handles");
            self.conn.pragma_update(None, "user_version", 57)?;
        }

        // V58: Recursive DAG live attempt heartbeat readback support.
        //
        // Phase 6.4A uses the owner/token/heartbeat columns introduced with
        // recursive_live_attempts in V55. This migration adds only an index for
        // token-owned active attempt expiry scans; it does not add recovery,
        // background heartbeat loops, or live scheduler reachability.
        if version < 58 {
            self.conn.execute_batch(
                "CREATE INDEX IF NOT EXISTS idx_recursive_live_attempts_heartbeat_expiry
                    ON recursive_live_attempts(status, lease_expires_at, heartbeat_at, id)
                    WHERE lease_token IS NOT NULL
                      AND heartbeat_at IS NOT NULL
                      AND lease_expires_at IS NOT NULL;",
            )?;
            tracing::info!("V58 migration complete: recursive DAG live attempt heartbeat index");
            self.conn.pragma_update(None, "user_version", 58)?;
        }

        // V59: Tighten recursive DAG live heartbeat stale-scan index.
        //
        // V58 introduced the heartbeat expiry index before the stale scan
        // required heartbeat_at to be present. Rebuild the partial index so
        // already-migrated V58 databases get the same narrower index shape as
        // fresh databases.
        if version < 59 {
            self.conn.execute_batch(
                "DROP INDEX IF EXISTS idx_recursive_live_attempts_heartbeat_expiry;
                 CREATE INDEX IF NOT EXISTS idx_recursive_live_attempts_heartbeat_expiry
                    ON recursive_live_attempts(status, lease_expires_at, heartbeat_at, id)
                    WHERE lease_token IS NOT NULL
                      AND heartbeat_at IS NOT NULL
                      AND lease_expires_at IS NOT NULL;",
            )?;
            tracing::info!("V59 migration complete: recursive DAG live heartbeat index tightened");
            self.conn.pragma_update(None, "user_version", 59)?;
        }

        // V60: Recursive DAG live output validation persistence.
        //
        // Phase 6.5C records validator decisions and artifact links so later
        // read-only status RPCs can inspect live output commits without
        // scanning execution artifacts or reinterpreting raw model JSON.
        if version < 60 {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS recursive_live_output_validations (
                    id TEXT PRIMARY KEY,
                    live_attempt_id TEXT NOT NULL REFERENCES recursive_live_attempts(id) ON DELETE CASCADE,
                    graph_id TEXT NOT NULL REFERENCES recursive_task_graphs(id) ON DELETE CASCADE,
                    task_id TEXT NOT NULL REFERENCES recursive_task_nodes(id) ON DELETE CASCADE,
                    scheduler_run_id TEXT NOT NULL REFERENCES recursive_scheduler_runs(id) ON DELETE CASCADE,
                    attempt_id TEXT NOT NULL REFERENCES recursive_task_attempts(id) ON DELETE CASCADE,
                    session_id TEXT REFERENCES sessions(id),
                    raw_output_artifact_id INTEGER REFERENCES recursive_execution_artifacts(id),
                    normalized_output_artifact_id INTEGER REFERENCES recursive_execution_artifacts(id),
                    validation_artifact_id INTEGER REFERENCES recursive_execution_artifacts(id),
                    status TEXT NOT NULL CHECK (status IN (
                        'valid', 'invalid', 'repairable', 'ambiguous',
                        'operator_review_required'
                    )),
                    output_kind TEXT CHECK (
                        output_kind IS NULL OR output_kind IN (
                            'success', 'decomposition', 'retryable_failure',
                            'permanent_failure', 'blocked', 'cancelled'
                        )
                    ),
                    mapping_decision TEXT CHECK (
                        mapping_decision IS NULL OR mapping_decision IN (
                            'success', 'decomposition', 'retry_failure',
                            'permanent_failure', 'blocked', 'cancelled',
                            'repair_same_live_attempt', 'fail_attempt',
                            'block_task', 'operator_review', 'no_op'
                        )
                    ),
                    issue_count INTEGER NOT NULL CHECK (issue_count >= 0),
                    error_count INTEGER NOT NULL CHECK (error_count >= 0),
                    warning_count INTEGER NOT NULL CHECK (warning_count >= 0),
                    info_count INTEGER NOT NULL CHECK (info_count >= 0),
                    issue_summary_json TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(issue_summary_json)),
                    parser_source_json TEXT CHECK (
                        parser_source_json IS NULL OR json_valid(parser_source_json)
                    ),
                    retry_decision_json TEXT CHECK (
                        retry_decision_json IS NULL OR json_valid(retry_decision_json)
                    ),
                    produced_artifact_ids_json TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(produced_artifact_ids_json)),
                    test_artifact_ids_json TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(test_artifact_ids_json)),
                    diff_artifact_ids_json TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(diff_artifact_ids_json)),
                    validation_report_json TEXT CHECK (
                        validation_report_json IS NULL OR json_valid(validation_report_json)
                    ),
                    metadata_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(metadata_json)),
                    normalized_digest TEXT,
                    created_at TEXT NOT NULL,
                    UNIQUE (live_attempt_id, normalized_digest)
                );

                CREATE INDEX IF NOT EXISTS idx_recursive_live_output_validations_live
                    ON recursive_live_output_validations(live_attempt_id, created_at DESC, id);
                CREATE INDEX IF NOT EXISTS idx_recursive_live_output_validations_attempt
                    ON recursive_live_output_validations(attempt_id, created_at DESC, id);
                CREATE INDEX IF NOT EXISTS idx_recursive_live_output_validations_session
                    ON recursive_live_output_validations(session_id, created_at DESC, id)
                    WHERE session_id IS NOT NULL;
                CREATE INDEX IF NOT EXISTS idx_recursive_live_output_validations_status
                    ON recursive_live_output_validations(status, created_at DESC, id);
                CREATE INDEX IF NOT EXISTS idx_recursive_live_output_validations_digest
                    ON recursive_live_output_validations(live_attempt_id, normalized_digest)
                    WHERE normalized_digest IS NOT NULL;",
            )?;
            tracing::info!("V60 migration complete: recursive DAG live output validations");
            self.conn.pragma_update(None, "user_version", 60)?;
        }

        // V61: Index read-only recursive DAG live validation status paths.
        //
        // Phase 6.5D exposes validation readback anchored by graph, task,
        // scheduler run, and output kind. These indexes are read-only support
        // only; no RPC handler creates indexes opportunistically at runtime.
        if version < 61 {
            self.conn.execute_batch(
                "CREATE INDEX IF NOT EXISTS idx_recursive_live_output_validations_graph
                    ON recursive_live_output_validations(graph_id, created_at DESC, id);
                 CREATE INDEX IF NOT EXISTS idx_recursive_live_output_validations_task
                    ON recursive_live_output_validations(graph_id, task_id, created_at DESC, id);
                 CREATE INDEX IF NOT EXISTS idx_recursive_live_output_validations_run
                    ON recursive_live_output_validations(scheduler_run_id, created_at DESC, id);
                 CREATE INDEX IF NOT EXISTS idx_recursive_live_output_validations_output_kind
                    ON recursive_live_output_validations(output_kind, created_at DESC, id)
                    WHERE output_kind IS NOT NULL;",
            )?;
            tracing::info!("V61 migration complete: recursive DAG live validation read indexes");
            self.conn.pragma_update(None, "user_version", 61)?;
        }

        // V62: Recursive topology-to-DAG linkage.
        //
        // T1 creates durable ownership/idempotency/snapshot records for
        // topology-derived recursive graphs and task links. This is schema
        // and graph-creation linkage only: no scheduler runs, live attempts,
        // sessions, workflow execution updates, or background behavior.
        if version < 62 {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS recursive_topology_graph_links (
                    graph_id TEXT PRIMARY KEY REFERENCES recursive_task_graphs(id) ON DELETE CASCADE,
                    execution_owner TEXT NOT NULL,
                    owner_key TEXT NOT NULL,
                    idempotency_key TEXT,
                    request_fingerprint TEXT NOT NULL,
                    topology_id TEXT NOT NULL REFERENCES topologies(id),
                    workflow_id TEXT REFERENCES workflows(id),
                    workflow_execution_id TEXT,
                    source_topology_node_id TEXT NOT NULL,
                    source_topology_iteration INTEGER NOT NULL CHECK (source_topology_iteration >= 0),
                    parent_session_id TEXT REFERENCES sessions(id),
                    project_id TEXT REFERENCES projects(id),
                    creation_mode TEXT NOT NULL,
                    include_prerequisite_closure INTEGER NOT NULL CHECK (include_prerequisite_closure IN (0, 1)),
                    topology_name_snapshot TEXT NOT NULL,
                    topology_updated_at_snapshot TEXT NOT NULL,
                    topology_snapshot_json TEXT NOT NULL CHECK (json_valid(topology_snapshot_json)),
                    selected_slice_json TEXT NOT NULL CHECK (json_valid(selected_slice_json)),
                    policy_snapshot_json TEXT NOT NULL CHECK (json_valid(policy_snapshot_json)),
                    workflow_execution_linkage_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(workflow_execution_linkage_json)),
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS recursive_topology_task_links (
                    graph_id TEXT NOT NULL REFERENCES recursive_task_graphs(id) ON DELETE CASCADE,
                    task_id TEXT NOT NULL REFERENCES recursive_task_nodes(id) ON DELETE CASCADE,
                    topology_id TEXT NOT NULL REFERENCES topologies(id),
                    topology_node_id TEXT NOT NULL,
                    topology_iteration INTEGER NOT NULL CHECK (topology_iteration >= 0),
                    topology_node_kind TEXT NOT NULL,
                    source_params_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(source_params_json)),
                    topology_node_snapshot_json TEXT NOT NULL CHECK (json_valid(topology_node_snapshot_json)),
                    policy_snapshot_json TEXT NOT NULL CHECK (json_valid(policy_snapshot_json)),
                    created_at TEXT NOT NULL,
                    UNIQUE (graph_id, topology_node_id, topology_iteration),
                    UNIQUE (task_id)
                );

                CREATE UNIQUE INDEX IF NOT EXISTS idx_recursive_topology_graph_links_owner_key
                    ON recursive_topology_graph_links(owner_key);
                CREATE INDEX IF NOT EXISTS idx_recursive_topology_graph_links_owner_lookup
                    ON recursive_topology_graph_links(execution_owner, owner_key);
                CREATE UNIQUE INDEX IF NOT EXISTS idx_recursive_topology_graph_links_idempotency
                    ON recursive_topology_graph_links(execution_owner, idempotency_key)
                    WHERE idempotency_key IS NOT NULL;
                CREATE INDEX IF NOT EXISTS idx_recursive_topology_graph_links_source
                    ON recursive_topology_graph_links(
                        topology_id, source_topology_node_id,
                        source_topology_iteration, created_at, graph_id
                    );
                CREATE INDEX IF NOT EXISTS idx_recursive_topology_graph_links_workflow_execution
                    ON recursive_topology_graph_links(
                        workflow_execution_id, source_topology_node_id, graph_id
                    );
                CREATE INDEX IF NOT EXISTS idx_recursive_topology_graph_links_workflow
                    ON recursive_topology_graph_links(workflow_id, topology_id, graph_id);

                CREATE INDEX IF NOT EXISTS idx_recursive_topology_task_links_node
                    ON recursive_topology_task_links(
                        topology_id, topology_node_id, topology_iteration, graph_id
                    );",
            )?;
            tracing::info!("V62 migration complete: recursive topology graph linkage");
            self.conn.pragma_update(None, "user_version", 62)?;
        }

        // V63: Recursive cancellation idempotency for topology wrappers.
        //
        // T4 keeps topology cancellation as a thin wrapper over recursive DAG
        // cancellation requests. These columns let the wrapper de-duplicate
        // concrete graph/run targets without changing generic cancellation RPC
        // behavior or adding a second cancellation table.
        if version < 63 {
            self.add_column_if_not_exists(
                "recursive_cancellation_requests",
                "idempotency_key",
                "TEXT",
            )?;
            self.add_column_if_not_exists(
                "recursive_cancellation_requests",
                "request_fingerprint",
                "TEXT",
            )?;
            self.add_column_if_not_exists(
                "recursive_cancellation_requests",
                "source_context_json",
                "TEXT CHECK (source_context_json IS NULL OR json_valid(source_context_json))",
            )?;
            self.conn.execute_batch(
                "CREATE UNIQUE INDEX IF NOT EXISTS idx_recursive_cancel_idempotency_key_unique
                    ON recursive_cancellation_requests(idempotency_key)
                    WHERE idempotency_key IS NOT NULL;
                CREATE INDEX IF NOT EXISTS idx_recursive_cancel_graph_scope_status_requested
                    ON recursive_cancellation_requests(graph_id, scope, status, requested_at, id);
                CREATE INDEX IF NOT EXISTS idx_recursive_cancel_run_scope_status_requested
                    ON recursive_cancellation_requests(run_id, scope, status, requested_at, id)
                    WHERE run_id IS NOT NULL;",
            )?;
            tracing::info!("V63 migration complete: recursive cancellation idempotency indexes");
            self.conn.pragma_update(None, "user_version", 63)?;
        }

        // V64: Recursive recovery pass idempotency and topology provenance.
        //
        // T5 keeps topology-scoped recovery as a selector-bound wrapper around
        // the existing recursive recovery pass tables. These columns let the
        // wrapper safely replay caller-keyed requests and expose audit context
        // without introducing a topology-specific recovery table.
        if version < 64 {
            self.add_column_if_not_exists("recursive_recovery_passes", "idempotency_key", "TEXT")?;
            self.add_column_if_not_exists(
                "recursive_recovery_passes",
                "request_fingerprint",
                "TEXT",
            )?;
            self.add_column_if_not_exists(
                "recursive_recovery_passes",
                "source_context_json",
                "TEXT CHECK (source_context_json IS NULL OR json_valid(source_context_json))",
            )?;
            self.conn.execute_batch(
                "CREATE UNIQUE INDEX IF NOT EXISTS idx_recursive_recovery_pass_idempotency_key_unique
                    ON recursive_recovery_passes(idempotency_key)
                    WHERE idempotency_key IS NOT NULL;
                CREATE INDEX IF NOT EXISTS idx_recursive_recovery_passes_source_context_started
                    ON recursive_recovery_passes(source, started_at DESC, id DESC)
                    WHERE source_context_json IS NOT NULL;
                CREATE INDEX IF NOT EXISTS idx_recursive_topology_graph_links_recovery_topology
                    ON recursive_topology_graph_links(
                        execution_owner, topology_id, source_topology_iteration,
                        source_topology_node_id, created_at, graph_id
                    );
                CREATE INDEX IF NOT EXISTS idx_recursive_topology_graph_links_recovery_workflow_execution
                    ON recursive_topology_graph_links(
                        execution_owner, workflow_execution_id, source_topology_iteration,
                        source_topology_node_id, created_at, graph_id
                    )
                    WHERE workflow_execution_id IS NOT NULL;
                CREATE INDEX IF NOT EXISTS idx_recursive_topology_graph_links_recovery_node
                    ON recursive_topology_graph_links(
                        execution_owner, topology_id, source_topology_node_id,
                        source_topology_iteration, created_at, graph_id
                    );",
            )?;
            tracing::info!(
                "V64 migration complete: recursive recovery idempotency and topology indexes"
            );
            self.conn.pragma_update(None, "user_version", 64)?;
        }

        // V65: Recursive DAG live scheduler store primitives.
        //
        // This migration admits the persisted `live_session` mode for graphs,
        // scheduler runs, and task attempts, and adds live-run request/policy
        // metadata columns. No RPC route is registered here and no session
        // launch path is made reachable; store APIs still decide which callers
        // may write live rows.
        if version < 65 {
            self.conn.execute_batch(
                "PRAGMA foreign_keys=OFF;
                BEGIN;

                CREATE TABLE recursive_task_graphs_v65_new (
                    id TEXT PRIMARY KEY,
                    root_task_id TEXT NOT NULL,
                    title TEXT NOT NULL CHECK (length(trim(title)) > 0),
                    objective TEXT NOT NULL CHECK (length(trim(objective)) > 0),
                    status TEXT NOT NULL CHECK (status IN (
                        'active', 'terminal', 'blocked', 'failed', 'cancelled', 'malformed'
                    )),
                    project_id TEXT REFERENCES projects(id),
                    workflow_id TEXT REFERENCES workflows(id),
                    topology_id TEXT REFERENCES topologies(id),
                    parent_session_id TEXT REFERENCES sessions(id),
                    source_execution_id TEXT,
                    source_eval_id TEXT,
                    execution_mode TEXT NOT NULL DEFAULT 'fake' CHECK (
                        execution_mode IN ('fake', 'live_session')
                    ),
                    max_depth INTEGER NOT NULL CHECK (max_depth >= 0),
                    max_fanout INTEGER NOT NULL CHECK (max_fanout > 0),
                    max_descendants INTEGER NOT NULL CHECK (max_descendants > 0),
                    step_limit INTEGER NOT NULL CHECK (step_limit > 0),
                    last_stop_reason TEXT,
                    malformed_reason TEXT,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    recovered_at TEXT,
                    quarantined_at TEXT,
                    quarantine_reason TEXT,
                    recovery_checked_at TEXT
                );
                INSERT INTO recursive_task_graphs_v65_new (
                    id, root_task_id, title, objective, status, project_id, workflow_id,
                    topology_id, parent_session_id, source_execution_id, source_eval_id,
                    execution_mode, max_depth, max_fanout, max_descendants, step_limit,
                    last_stop_reason, malformed_reason, created_at, updated_at, recovered_at,
                    quarantined_at, quarantine_reason, recovery_checked_at
                )
                SELECT
                    id, root_task_id, title, objective, status, project_id, workflow_id,
                    topology_id, parent_session_id, source_execution_id, source_eval_id,
                    execution_mode, max_depth, max_fanout, max_descendants, step_limit,
                    last_stop_reason, malformed_reason, created_at, updated_at, recovered_at,
                    quarantined_at, quarantine_reason, recovery_checked_at
                FROM recursive_task_graphs;
                DROP TABLE recursive_task_graphs;
                ALTER TABLE recursive_task_graphs_v65_new RENAME TO recursive_task_graphs;
                CREATE UNIQUE INDEX IF NOT EXISTS idx_recursive_graph_root
                    ON recursive_task_graphs(id, root_task_id);
                CREATE INDEX IF NOT EXISTS idx_recursive_graph_project
                    ON recursive_task_graphs(project_id, updated_at DESC);
                CREATE INDEX IF NOT EXISTS idx_recursive_graph_workflow
                    ON recursive_task_graphs(workflow_id, updated_at DESC);
                CREATE INDEX IF NOT EXISTS idx_recursive_graph_topology
                    ON recursive_task_graphs(topology_id, updated_at DESC);
                CREATE INDEX IF NOT EXISTS idx_recursive_graph_status
                    ON recursive_task_graphs(status, updated_at DESC);
                CREATE INDEX IF NOT EXISTS idx_recursive_graph_quarantine
                    ON recursive_task_graphs(quarantined_at, updated_at DESC)
                    WHERE quarantined_at IS NOT NULL;

                CREATE TABLE recursive_task_attempts_v65_new (
                    id TEXT PRIMARY KEY,
                    graph_id TEXT NOT NULL REFERENCES recursive_task_graphs(id) ON DELETE CASCADE,
                    task_id TEXT NOT NULL REFERENCES recursive_task_nodes(id) ON DELETE CASCADE,
                    phase TEXT NOT NULL CHECK (phase IN ('execute', 'integrate')),
                    attempt_no INTEGER NOT NULL CHECK (attempt_no > 0),
                    retry_count INTEGER NOT NULL CHECK (retry_count >= 0),
                    status TEXT NOT NULL CHECK (status IN (
                        'running', 'succeeded', 'decomposed', 'failed',
                        'blocked', 'cancelled', 'interrupted'
                    )),
                    started_at TEXT NOT NULL,
                    finished_at TEXT,
                    failure_reason TEXT,
                    block_reason TEXT,
                    dependency_snapshot_json TEXT NOT NULL CHECK (json_valid(dependency_snapshot_json)),
                    executor_kind TEXT NOT NULL DEFAULT 'fake' CHECK (
                        executor_kind IN ('fake', 'live_session')
                    ),
                    session_id TEXT REFERENCES sessions(id),
                    workflow_execution_id TEXT,
                    CHECK (
                        (status = 'running' AND finished_at IS NULL)
                        OR (status <> 'running' AND finished_at IS NOT NULL)
                    ),
                    UNIQUE (task_id, phase, attempt_no)
                );
                INSERT INTO recursive_task_attempts_v65_new (
                    id, graph_id, task_id, phase, attempt_no, retry_count, status,
                    started_at, finished_at, failure_reason, block_reason,
                    dependency_snapshot_json, executor_kind, session_id, workflow_execution_id
                )
                SELECT
                    id, graph_id, task_id, phase, attempt_no, retry_count, status,
                    started_at, finished_at, failure_reason, block_reason,
                    dependency_snapshot_json, executor_kind, session_id, workflow_execution_id
                FROM recursive_task_attempts;
                DROP TABLE recursive_task_attempts;
                ALTER TABLE recursive_task_attempts_v65_new RENAME TO recursive_task_attempts;
                CREATE INDEX IF NOT EXISTS idx_recursive_attempts_task_phase
                    ON recursive_task_attempts(task_id, phase, attempt_no);
                CREATE INDEX IF NOT EXISTS idx_recursive_attempts_running
                    ON recursive_task_attempts(graph_id, status, started_at)
                    WHERE status = 'running';
                CREATE INDEX IF NOT EXISTS idx_recursive_attempts_session
                    ON recursive_task_attempts(session_id)
                    WHERE session_id IS NOT NULL;

                CREATE TABLE recursive_scheduler_runs_v65_new (
                    id TEXT PRIMARY KEY,
                    graph_id TEXT NOT NULL REFERENCES recursive_task_graphs(id) ON DELETE CASCADE,
                    status TEXT NOT NULL CHECK (status IN (
                        'running', 'cancelling', 'completed', 'cancelled',
                        'failed', 'rejected', 'lease_expired'
                    )),
                    source TEXT NOT NULL CHECK (source IN (
                        'test_harness', 'manual_rpc', 'startup_recovery', 'future_daemon_loop'
                    )),
                    operator TEXT,
                    started_at TEXT NOT NULL,
                    completed_at TEXT,
                    stop_reason TEXT CHECK (
                        stop_reason IS NULL OR stop_reason IN (
                            'graph_terminal', 'idle_no_runnable', 'step_limit_exceeded',
                            'partial_failure', 'quarantined', 'cancellation_requested',
                            'lease_expired', 'executor_error', 'recovery_deferred'
                        )
                    ),
                    step_count INTEGER NOT NULL DEFAULT 0 CHECK (step_count >= 0),
                    max_steps INTEGER NOT NULL CHECK (max_steps > 0),
                    executor_mode TEXT NOT NULL DEFAULT 'fake' CHECK (
                        executor_mode IN ('fake', 'live_session')
                    ),
                    failure_reason TEXT,
                    report_artifact_id INTEGER REFERENCES recursive_execution_artifacts(id),
                    cancellation_request_id TEXT REFERENCES recursive_cancellation_requests(id),
                    cancellation_reason TEXT,
                    lease_owner TEXT,
                    lease_token TEXT,
                    lease_heartbeat_at TEXT,
                    lease_expires_at TEXT,
                    idempotency_key TEXT,
                    request_fingerprint TEXT,
                    policy_snapshot_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(policy_snapshot_json)),
                    CHECK (
                        (status IN ('running', 'cancelling') AND completed_at IS NULL)
                        OR (status NOT IN ('running', 'cancelling') AND completed_at IS NOT NULL)
                    )
                );
                INSERT INTO recursive_scheduler_runs_v65_new (
                    id, graph_id, status, source, operator, started_at, completed_at,
                    stop_reason, step_count, max_steps, executor_mode, failure_reason,
                    report_artifact_id, cancellation_request_id, cancellation_reason,
                    lease_owner, lease_token, lease_heartbeat_at, lease_expires_at,
                    idempotency_key, request_fingerprint, policy_snapshot_json
                )
                SELECT
                    id, graph_id, status, source, operator, started_at, completed_at,
                    stop_reason, step_count, max_steps, executor_mode, failure_reason,
                    report_artifact_id, cancellation_request_id, cancellation_reason,
                    lease_owner, lease_token, lease_heartbeat_at, lease_expires_at,
                    NULL, NULL, '{}'
                FROM recursive_scheduler_runs;
                DROP TABLE recursive_scheduler_runs;
                ALTER TABLE recursive_scheduler_runs_v65_new RENAME TO recursive_scheduler_runs;
                CREATE INDEX IF NOT EXISTS idx_recursive_scheduler_runs_graph_started
                    ON recursive_scheduler_runs(graph_id, started_at DESC);
                CREATE INDEX IF NOT EXISTS idx_recursive_scheduler_runs_status
                    ON recursive_scheduler_runs(status, started_at DESC);
                CREATE UNIQUE INDEX IF NOT EXISTS idx_recursive_scheduler_runs_one_active_per_graph
                    ON recursive_scheduler_runs(graph_id)
                    WHERE status IN ('running', 'cancelling');
                CREATE INDEX IF NOT EXISTS idx_recursive_scheduler_runs_active_lease
                    ON recursive_scheduler_runs(status, lease_expires_at, started_at)
                    WHERE status IN ('running', 'cancelling');
                CREATE INDEX IF NOT EXISTS idx_recursive_scheduler_runs_lease_owner
                    ON recursive_scheduler_runs(lease_owner, status, lease_expires_at)
                    WHERE lease_owner IS NOT NULL;
                CREATE UNIQUE INDEX IF NOT EXISTS idx_recursive_scheduler_runs_live_idempotency
                    ON recursive_scheduler_runs(idempotency_key)
                    WHERE idempotency_key IS NOT NULL;

                COMMIT;
                PRAGMA foreign_keys=ON;",
            )?;
            tracing::info!("V65 migration complete: recursive DAG live scheduler store primitives");
            self.conn.pragma_update(None, "user_version", 65)?;
        }

        // V66: Recursive DAG typed inspector materialization foundation.
        //
        // These tables are store-only read-model foundations for future typed
        // inspector RPCs. The migration does not register routes or flip typed
        // inspector capabilities; write paths materialize daemon-owned rows in
        // their existing transactions.
        if version < 66 {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS recursive_typed_artifact_roles (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    graph_id TEXT NOT NULL REFERENCES recursive_task_graphs(id) ON DELETE CASCADE,
                    task_id TEXT REFERENCES recursive_task_nodes(id) ON DELETE CASCADE,
                    attempt_id TEXT REFERENCES recursive_task_attempts(id) ON DELETE CASCADE,
                    live_attempt_id TEXT REFERENCES recursive_live_attempts(id) ON DELETE CASCADE,
                    scheduler_run_id TEXT REFERENCES recursive_scheduler_runs(id) ON DELETE CASCADE,
                    validation_id TEXT REFERENCES recursive_live_output_validations(id) ON DELETE CASCADE,
                    artifact_id INTEGER NOT NULL REFERENCES recursive_execution_artifacts(id) ON DELETE CASCADE,
                    role TEXT NOT NULL CHECK (role IN (
                        'raw_output', 'normalized_output', 'validation_report',
                        'produced_artifact', 'test_summary', 'diff_summary',
                        'scheduler_report', 'topology_recursive_graph_creation'
                    )),
                    provenance TEXT NOT NULL CHECK (provenance IN (
                        'legacy', 'daemon_recorded', 'live_output_validation',
                        'scheduler_report', 'topology_creation'
                    )),
                    source TEXT NOT NULL CHECK (source IN (
                        'daemon_collected', 'live_validation', 'scheduler_report',
                        'legacy_backfill', 'model_claimed'
                    )),
                    trust_level TEXT NOT NULL CHECK (trust_level IN (
                        'verified', 'normalized', 'scheduler_owned',
                        'model_claimed', 'legacy_unverified'
                    )),
                    metadata_state TEXT NOT NULL CHECK (
                        metadata_state IN ('valid', 'malformed', 'legacy', 'unavailable')
                    ),
                    source_artifact_id INTEGER REFERENCES recursive_execution_artifacts(id),
                    source_digest TEXT,
                    schema_version INTEGER CHECK (schema_version IS NULL OR schema_version >= 0),
                    metadata_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(metadata_json)),
                    created_at TEXT NOT NULL,
                    UNIQUE (artifact_id, role, source)
                );

                CREATE INDEX IF NOT EXISTS idx_recursive_typed_artifact_roles_graph
                    ON recursive_typed_artifact_roles(graph_id, created_at DESC, id DESC);
                CREATE INDEX IF NOT EXISTS idx_recursive_typed_artifact_roles_artifact
                    ON recursive_typed_artifact_roles(artifact_id, source);
                CREATE INDEX IF NOT EXISTS idx_recursive_typed_artifact_roles_validation
                    ON recursive_typed_artifact_roles(validation_id, role)
                    WHERE validation_id IS NOT NULL;
                CREATE INDEX IF NOT EXISTS idx_recursive_typed_artifact_roles_run
                    ON recursive_typed_artifact_roles(scheduler_run_id, role)
                    WHERE scheduler_run_id IS NOT NULL;

                CREATE TABLE IF NOT EXISTS recursive_test_results (
                    test_id TEXT PRIMARY KEY,
                    graph_id TEXT NOT NULL REFERENCES recursive_task_graphs(id) ON DELETE CASCADE,
                    task_id TEXT REFERENCES recursive_task_nodes(id) ON DELETE CASCADE,
                    attempt_id TEXT REFERENCES recursive_task_attempts(id) ON DELETE CASCADE,
                    live_attempt_id TEXT REFERENCES recursive_live_attempts(id) ON DELETE CASCADE,
                    scheduler_run_id TEXT REFERENCES recursive_scheduler_runs(id) ON DELETE CASCADE,
                    validation_id TEXT REFERENCES recursive_live_output_validations(id) ON DELETE CASCADE,
                    artifact_id INTEGER REFERENCES recursive_execution_artifacts(id),
                    test_index INTEGER NOT NULL DEFAULT 0 CHECK (test_index >= 0),
                    status TEXT NOT NULL CHECK (status IN (
                        'passed', 'failed', 'skipped', 'not_run', 'unknown'
                    )),
                    required INTEGER NOT NULL DEFAULT 0 CHECK (required IN (0, 1)),
                    name TEXT,
                    command TEXT,
                    display_label TEXT NOT NULL CHECK (length(trim(display_label)) > 0),
                    duration_ms INTEGER CHECK (duration_ms IS NULL OR duration_ms >= 0),
                    exit_code INTEGER,
                    failure_summary TEXT,
                    stdout_artifact_id INTEGER REFERENCES recursive_execution_artifacts(id),
                    stderr_artifact_id INTEGER REFERENCES recursive_execution_artifacts(id),
                    log_artifact_id INTEGER REFERENCES recursive_execution_artifacts(id),
                    output_artifact_ids_json TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(output_artifact_ids_json)),
                    related_validation_issue_ids_json TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(related_validation_issue_ids_json)),
                    retry_decision_json TEXT CHECK (retry_decision_json IS NULL OR json_valid(retry_decision_json)),
                    suggested_next_action TEXT,
                    source TEXT NOT NULL CHECK (source IN (
                        'daemon_collected', 'live_validation', 'scheduler_report',
                        'legacy_backfill', 'model_claimed'
                    )),
                    trust_level TEXT NOT NULL CHECK (trust_level IN (
                        'verified', 'normalized', 'scheduler_owned',
                        'model_claimed', 'legacy_unverified'
                    )),
                    metadata_state TEXT NOT NULL CHECK (
                        metadata_state IN ('valid', 'malformed', 'legacy', 'unavailable')
                    ),
                    source_artifact_id INTEGER REFERENCES recursive_execution_artifacts(id),
                    source_digest TEXT,
                    schema_version INTEGER CHECK (schema_version IS NULL OR schema_version >= 0),
                    metadata_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(metadata_json)),
                    created_at TEXT NOT NULL
                );

                CREATE UNIQUE INDEX IF NOT EXISTS idx_recursive_test_results_validation_index
                    ON recursive_test_results(validation_id, test_index)
                    WHERE validation_id IS NOT NULL;
                CREATE INDEX IF NOT EXISTS idx_recursive_test_results_graph
                    ON recursive_test_results(graph_id, created_at DESC, test_id DESC);
                CREATE INDEX IF NOT EXISTS idx_recursive_test_results_run
                    ON recursive_test_results(scheduler_run_id, created_at DESC, test_id DESC)
                    WHERE scheduler_run_id IS NOT NULL;
                CREATE INDEX IF NOT EXISTS idx_recursive_test_results_artifact
                    ON recursive_test_results(artifact_id)
                    WHERE artifact_id IS NOT NULL;

                CREATE TABLE IF NOT EXISTS recursive_diff_summaries (
                    diff_id TEXT PRIMARY KEY,
                    graph_id TEXT NOT NULL REFERENCES recursive_task_graphs(id) ON DELETE CASCADE,
                    task_id TEXT REFERENCES recursive_task_nodes(id) ON DELETE CASCADE,
                    attempt_id TEXT REFERENCES recursive_task_attempts(id) ON DELETE CASCADE,
                    live_attempt_id TEXT REFERENCES recursive_live_attempts(id) ON DELETE CASCADE,
                    scheduler_run_id TEXT REFERENCES recursive_scheduler_runs(id) ON DELETE CASCADE,
                    validation_id TEXT REFERENCES recursive_live_output_validations(id) ON DELETE CASCADE,
                    artifact_id INTEGER REFERENCES recursive_execution_artifacts(id),
                    diff_index INTEGER NOT NULL DEFAULT 0 CHECK (diff_index >= 0),
                    source TEXT NOT NULL CHECK (source IN (
                        'daemon_collected', 'live_validation', 'scheduler_report',
                        'legacy_backfill', 'model_claimed'
                    )),
                    trust_level TEXT NOT NULL CHECK (trust_level IN (
                        'verified', 'normalized', 'scheduler_owned',
                        'model_claimed', 'legacy_unverified'
                    )),
                    file_count INTEGER NOT NULL CHECK (file_count >= 0),
                    binary_file_count INTEGER NOT NULL DEFAULT 0 CHECK (binary_file_count >= 0),
                    truncated_file_count INTEGER NOT NULL DEFAULT 0 CHECK (truncated_file_count >= 0),
                    additions INTEGER CHECK (additions IS NULL OR additions >= 0),
                    deletions INTEGER CHECK (deletions IS NULL OR deletions >= 0),
                    hunk_count INTEGER CHECK (hunk_count IS NULL OR hunk_count >= 0),
                    metadata_state TEXT NOT NULL CHECK (
                        metadata_state IN ('valid', 'malformed', 'legacy', 'unavailable')
                    ),
                    source_artifact_id INTEGER REFERENCES recursive_execution_artifacts(id),
                    source_digest TEXT,
                    schema_version INTEGER CHECK (schema_version IS NULL OR schema_version >= 0),
                    metadata_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(metadata_json)),
                    created_at TEXT NOT NULL
                );

                CREATE UNIQUE INDEX IF NOT EXISTS idx_recursive_diff_summaries_validation_index
                    ON recursive_diff_summaries(validation_id, diff_index)
                    WHERE validation_id IS NOT NULL;
                CREATE INDEX IF NOT EXISTS idx_recursive_diff_summaries_graph
                    ON recursive_diff_summaries(graph_id, created_at DESC, diff_id DESC);
                CREATE INDEX IF NOT EXISTS idx_recursive_diff_summaries_run
                    ON recursive_diff_summaries(scheduler_run_id, created_at DESC, diff_id DESC)
                    WHERE scheduler_run_id IS NOT NULL;
                CREATE INDEX IF NOT EXISTS idx_recursive_diff_summaries_artifact
                    ON recursive_diff_summaries(artifact_id)
                    WHERE artifact_id IS NOT NULL;

                CREATE TABLE IF NOT EXISTS recursive_diff_files (
                    file_id TEXT PRIMARY KEY,
                    diff_id TEXT NOT NULL REFERENCES recursive_diff_summaries(diff_id) ON DELETE CASCADE,
                    graph_id TEXT NOT NULL REFERENCES recursive_task_graphs(id) ON DELETE CASCADE,
                    file_index INTEGER NOT NULL CHECK (file_index >= 0),
                    display_path TEXT NOT NULL CHECK (length(trim(display_path)) > 0),
                    previous_display_path TEXT,
                    status TEXT NOT NULL CHECK (status IN (
                        'added', 'modified', 'deleted', 'renamed',
                        'copied', 'unchanged', 'unknown'
                    )),
                    additions INTEGER CHECK (additions IS NULL OR additions >= 0),
                    deletions INTEGER CHECK (deletions IS NULL OR deletions >= 0),
                    hunk_count INTEGER CHECK (hunk_count IS NULL OR hunk_count >= 0),
                    binary INTEGER NOT NULL DEFAULT 0 CHECK (binary IN (0, 1)),
                    inside_allowed_root INTEGER CHECK (
                        inside_allowed_root IS NULL OR inside_allowed_root IN (0, 1)
                    ),
                    validation_issue_count INTEGER NOT NULL DEFAULT 0 CHECK (validation_issue_count >= 0),
                    metadata_state TEXT NOT NULL CHECK (
                        metadata_state IN ('valid', 'malformed', 'legacy', 'unavailable')
                    ),
                    metadata_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(metadata_json)),
                    created_at TEXT NOT NULL,
                    UNIQUE (diff_id, file_index)
                );

                CREATE INDEX IF NOT EXISTS idx_recursive_diff_files_diff
                    ON recursive_diff_files(diff_id, file_index);
                CREATE INDEX IF NOT EXISTS idx_recursive_diff_files_graph
                    ON recursive_diff_files(graph_id, display_path);

                CREATE TABLE IF NOT EXISTS recursive_scheduler_reports (
                    report_id TEXT PRIMARY KEY,
                    run_id TEXT NOT NULL REFERENCES recursive_scheduler_runs(id) ON DELETE CASCADE,
                    graph_id TEXT NOT NULL REFERENCES recursive_task_graphs(id) ON DELETE CASCADE,
                    source TEXT NOT NULL CHECK (source IN (
                        'daemon_collected', 'live_validation', 'scheduler_report',
                        'legacy_backfill', 'model_claimed'
                    )),
                    trust_level TEXT NOT NULL CHECK (trust_level IN (
                        'verified', 'normalized', 'scheduler_owned',
                        'model_claimed', 'legacy_unverified'
                    )),
                    scheduler_source TEXT NOT NULL CHECK (scheduler_source IN (
                        'test_harness', 'manual_rpc', 'startup_recovery', 'future_daemon_loop'
                    )),
                    operator TEXT,
                    execution_mode TEXT NOT NULL CHECK (execution_mode IN ('fake', 'live_session')),
                    run_status TEXT NOT NULL CHECK (run_status IN (
                        'running', 'cancelling', 'completed', 'cancelled',
                        'failed', 'rejected', 'lease_expired'
                    )),
                    started_at TEXT NOT NULL,
                    completed_at TEXT,
                    stop_reason TEXT CHECK (
                        stop_reason IS NULL OR stop_reason IN (
                            'graph_terminal', 'idle_no_runnable', 'step_limit_exceeded',
                            'partial_failure', 'quarantined', 'cancellation_requested',
                            'lease_expired', 'executor_error', 'recovery_deferred'
                        )
                    ),
                    failure_reason TEXT,
                    step_count INTEGER NOT NULL CHECK (step_count >= 0),
                    max_steps INTEGER NOT NULL CHECK (max_steps > 0),
                    selected_task_count INTEGER NOT NULL DEFAULT 0 CHECK (selected_task_count >= 0),
                    live_attempt_count INTEGER NOT NULL DEFAULT 0 CHECK (live_attempt_count >= 0),
                    validation_count INTEGER NOT NULL DEFAULT 0 CHECK (validation_count >= 0),
                    emitted_artifact_count INTEGER NOT NULL DEFAULT 0 CHECK (emitted_artifact_count >= 0),
                    cancellation_observed INTEGER NOT NULL DEFAULT 0 CHECK (cancellation_observed IN (0, 1)),
                    recovery_observed INTEGER NOT NULL DEFAULT 0 CHECK (recovery_observed IN (0, 1)),
                    report_artifact_id INTEGER REFERENCES recursive_execution_artifacts(id),
                    metadata_state TEXT NOT NULL CHECK (
                        metadata_state IN ('valid', 'malformed', 'legacy', 'unavailable')
                    ),
                    metadata_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(metadata_json)),
                    created_at TEXT NOT NULL,
                    UNIQUE (run_id)
                );

                CREATE INDEX IF NOT EXISTS idx_recursive_scheduler_reports_graph
                    ON recursive_scheduler_reports(graph_id, started_at DESC, report_id DESC);
                CREATE INDEX IF NOT EXISTS idx_recursive_scheduler_reports_artifact
                    ON recursive_scheduler_reports(report_artifact_id)
                    WHERE report_artifact_id IS NOT NULL;

                CREATE TABLE IF NOT EXISTS recursive_scheduler_report_steps (
                    report_id TEXT NOT NULL REFERENCES recursive_scheduler_reports(report_id) ON DELETE CASCADE,
                    run_id TEXT NOT NULL REFERENCES recursive_scheduler_runs(id) ON DELETE CASCADE,
                    graph_id TEXT NOT NULL REFERENCES recursive_task_graphs(id) ON DELETE CASCADE,
                    step_index INTEGER NOT NULL CHECK (step_index >= 0),
                    task_id TEXT NOT NULL REFERENCES recursive_task_nodes(id) ON DELETE CASCADE,
                    phase TEXT NOT NULL CHECK (phase IN ('execute', 'integrate')),
                    attempt_id TEXT NOT NULL REFERENCES recursive_task_attempts(id) ON DELETE CASCADE,
                    live_attempt_id TEXT REFERENCES recursive_live_attempts(id) ON DELETE CASCADE,
                    validation_id TEXT REFERENCES recursive_live_output_validations(id) ON DELETE CASCADE,
                    outcome TEXT NOT NULL CHECK (outcome IN (
                        'succeeded', 'decomposed', 'failed',
                        'blocked', 'cancelled', 'stopped', 'unknown'
                    )),
                    message TEXT,
                    final_task_status TEXT NOT NULL CHECK (final_task_status IN (
                        'pending', 'planning', 'ready', 'running', 'decomposed',
                        'blocked_on_children', 'integrating', 'verifying',
                        'succeeded', 'failed', 'blocked', 'cancelled'
                    )),
                    emitted_artifact_ids_json TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(emitted_artifact_ids_json)),
                    metadata_state TEXT NOT NULL CHECK (
                        metadata_state IN ('valid', 'malformed', 'legacy', 'unavailable')
                    ),
                    metadata_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(metadata_json)),
                    created_at TEXT NOT NULL,
                    PRIMARY KEY (report_id, step_index)
                );

                CREATE INDEX IF NOT EXISTS idx_recursive_scheduler_report_steps_run
                    ON recursive_scheduler_report_steps(run_id, step_index);
                CREATE INDEX IF NOT EXISTS idx_recursive_scheduler_report_steps_graph
                    ON recursive_scheduler_report_steps(graph_id, task_id, step_index);",
            )?;
            tracing::info!(
                "V66 migration complete: recursive DAG typed inspector materialization foundation"
            );
            self.conn.pragma_update(None, "user_version", 66)?;
        }

        if version < 67 {
            self.add_column_if_not_exists("sessions", "pending_question_json", "TEXT")?;
            tracing::info!("V67 migration complete: sessions.pending_question_json");
            self.conn.pragma_update(None, "user_version", 67)?;
        }

        if version < 68 {
            // scheduled_jobs was created in V35. Guard against artificial test
            // scenarios (e.g. blank DB forced to user_version=38) where V35 was
            // bypassed and the table doesn't exist yet. In production every
            // V38+ database already has scheduled_jobs.
            let table_exists: bool = self
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='scheduled_jobs'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap_or(0)
                > 0;
            if table_exists {
                self.add_column_if_not_exists(
                    "scheduled_jobs",
                    "wake_mode",
                    "TEXT NOT NULL DEFAULT 'fresh'",
                )?;
                self.add_column_if_not_exists("scheduled_jobs", "wake_session_id", "TEXT")?;
            }
            tracing::info!("V68 migration complete: scheduled_jobs.wake_mode + wake_session_id");
            self.conn.pragma_update(None, "user_version", 68)?;
        }

        if version < 69 {
            // S10 / D6 content-staleness: graph_cache gains a content_hash column
            // so cache keys invalidate on the RESOLVED input CONTENT changing, not
            // just topology_version.
            //
            // Why ALTER (strict superset) rather than DROP+rebuild: graph_cache is
            // created via `CREATE TABLE IF NOT EXISTS` in
            // `graph_cache::init_cache_table` (not a numbered block), so it may not
            // exist yet on a given DB — hence the table_exists guard, mirroring V68.
            // Cache entries are disposable, so DROP+rebuild would be acceptable, but
            // a defaulted ADD COLUMN keeps pre-existing DBs readable and lets legacy
            // rows load with a blank content_hash instead of being silently dropped
            // (Reliability lens). The `TEXT NOT NULL DEFAULT ''` matches the fresh
            // schema in `init_cache_table`, so both creation paths converge.
            let table_exists: bool = self
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='graph_cache'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap_or(0)
                > 0;
            if table_exists {
                self.add_column_if_not_exists(
                    "graph_cache",
                    "content_hash",
                    "TEXT NOT NULL DEFAULT ''",
                )?;
            }
            tracing::info!("V69 migration complete: graph_cache.content_hash");
            self.conn.pragma_update(None, "user_version", 69)?;
        }

        // V70: accumulated active-work milliseconds per session (TD1 / TUI redesign).
        // "Active work" = wall-clock Running interval MINUS AskUserQuestion wait, folded
        // on the monitor snapshot tick and at finalize. Nullable for symmetry with V41
        // approval_wait_ms: NULL = unmeasured (pre-V70 row, or container that never
        // entered the monitor loop), Some(0) = measured-but-no-work-yet. Add-only floor —
        // never recomputed from wall-clock timestamps, so it stays monotonic across daemon
        // restart (restore flips Running→Failed WITHOUT finalize; the last-flushed floor
        // stands, the crash tail is accepted bounded loss).
        if version < 70 {
            self.add_column_if_not_exists("sessions", "work_time_ms", "INTEGER")?;
            tracing::info!("V70 migration complete: work_time_ms column");
            self.conn.pragma_update(None, "user_version", 70)?;
        }

        // V72: local issue tracker foundation (C1) — `issues` + `issue_deps`.
        //
        // V71 HOLE: V71 is HELD for the unrelated S11 program. Any DB that
        // reaches 72 here will NEVER execute a later-inserted `if version < 71`
        // block — S11 must write idempotent DDL that also applies correctly
        // when first run at user_version > 71, or take a fresh number.
        //
        // Net-new tables, disjoint from V33 `issue_tracker_dispatches` and the
        // session issue columns. `status` carries NO SQL CHECK on purpose: a
        // CHECK cannot be altered without a table rebuild and C2/C5 may add
        // variants; validation lives in Rust (serde-exact strings, unknown
        // string => store error). "Blocked" is derived from `issue_deps`
        // edges, never stored. FK enforcement relies on the
        // `PRAGMA foreign_keys=ON` set in both `open` and `open_in_memory`.
        // All DDL is IF NOT EXISTS-guarded and the version bump is last, so a
        // crash mid-block re-runs cleanly on next boot.
        if version < 72 {
            self.conn.execute_batch(
                "
                CREATE TABLE IF NOT EXISTS issues (
                    id TEXT PRIMARY KEY,                     -- lowercase canonical UUID
                    display_number INTEGER NOT NULL UNIQUE,  -- monotonic human number
                    title TEXT NOT NULL,
                    body TEXT NOT NULL DEFAULT '',
                    status TEXT NOT NULL DEFAULT 'Open',     -- serde variants: Open|InProgress|Closed|Cancelled
                    priority INTEGER,                        -- 1=urgent..4=low; NULL=unset
                    labels TEXT NOT NULL DEFAULT '[]',       -- JSON array of strings
                    created_by_session_id TEXT,              -- lowercase UUID; NULL=operator
                    assignee TEXT,
                    created_at TEXT NOT NULL,                -- RFC3339 nanos
                    updated_at TEXT NOT NULL,
                    closed_at TEXT
                );
                CREATE INDEX IF NOT EXISTS idx_issues_status ON issues(status);
                CREATE INDEX IF NOT EXISTS idx_issues_created_by ON issues(created_by_session_id);

                CREATE TABLE IF NOT EXISTS issue_deps (
                    issue_id TEXT NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
                    depends_on_id TEXT NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
                    created_at TEXT NOT NULL,
                    PRIMARY KEY (issue_id, depends_on_id),
                    CHECK (issue_id <> depends_on_id)
                );
                CREATE INDEX IF NOT EXISTS idx_issue_deps_depends_on ON issue_deps(depends_on_id);
                ",
            )?;
            tracing::info!("V72 migration complete: issues + issue_deps tables");
            self.conn.pragma_update(None, "user_version", 72)?;
        }

        if version < 73 {
            let tx = self.conn.unchecked_transaction()?;
            add_column_if_not_exists_tx(&tx, "sessions", "model_invocation_id", "TEXT")?;
            tx.execute_batch(
                "
                CREATE TABLE IF NOT EXISTS model_invocations (
                    id TEXT PRIMARY KEY,
                    purpose TEXT NOT NULL,
                    invocation_kind TEXT NOT NULL,
                    foreground TEXT NOT NULL,
                    paid_risk TEXT NOT NULL,
                    admission_status TEXT NOT NULL,
                    status TEXT NOT NULL,
                    provider TEXT,
                    model TEXT,
                    backend TEXT,
                    model_tier TEXT,
                    effort TEXT,
                    trigger_source TEXT NOT NULL,
                    session_id TEXT,
                    project_id TEXT,
                    workflow_id TEXT,
                    scheduled_job_id TEXT,
                    issue_tracker_id TEXT,
                    issue_identifier TEXT,
                    topology_node_id TEXT,
                    recursive_graph_id TEXT,
                    recursive_task_id TEXT,
                    recursive_attempt_id TEXT,
                    operator TEXT,
                    parent_invocation_id TEXT,
                    retry_of_invocation_id TEXT,
                    dedup_key TEXT,
                    request_fingerprint TEXT,
                    policy_snapshot_json TEXT NOT NULL DEFAULT '{}',
                    error_class TEXT,
                    reserved_input_tokens INTEGER NOT NULL DEFAULT 0,
                    reserved_output_tokens INTEGER NOT NULL DEFAULT 0,
                    reserved_cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
                    reserved_cache_read_tokens INTEGER NOT NULL DEFAULT 0,
                    reserved_reasoning_tokens INTEGER NOT NULL DEFAULT 0,
                    reserved_embedding_input_count INTEGER NOT NULL DEFAULT 0,
                    reserved_wall_time_ms INTEGER NOT NULL DEFAULT 0,
                    input_tokens INTEGER,
                    output_tokens INTEGER,
                    cache_creation_tokens INTEGER,
                    cache_read_tokens INTEGER,
                    reasoning_tokens INTEGER,
                    embedding_input_count INTEGER,
                    wall_time_ms INTEGER,
                    estimated_cost_usd REAL,
                    usage_confidence TEXT NOT NULL DEFAULT 'unavailable',
                    baseline_input_tokens INTEGER NOT NULL DEFAULT 0,
                    baseline_output_tokens INTEGER NOT NULL DEFAULT 0,
                    baseline_cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
                    baseline_cache_read_tokens INTEGER NOT NULL DEFAULT 0,
                    baseline_reasoning_tokens INTEGER NOT NULL DEFAULT 0,
                    baseline_embedding_input_count INTEGER NOT NULL DEFAULT 0,
                    baseline_wall_time_ms INTEGER NOT NULL DEFAULT 0,
                    created_at TEXT NOT NULL,
                    started_at TEXT,
                    completed_at TEXT
                );
                CREATE UNIQUE INDEX IF NOT EXISTS idx_model_invocations_dedup
                    ON model_invocations(dedup_key) WHERE dedup_key IS NOT NULL;
                CREATE INDEX IF NOT EXISTS idx_model_invocations_session_id
                    ON model_invocations(session_id);
                CREATE INDEX IF NOT EXISTS idx_model_invocations_purpose
                    ON model_invocations(purpose);

                CREATE TABLE IF NOT EXISTS model_budget_policies (
                    policy_key TEXT PRIMARY KEY,
                    scope_kind TEXT NOT NULL,
                    scope_id TEXT NOT NULL,
                    purpose TEXT,
                    model_tier TEXT,
                    effort TEXT,
                    ceiling_model_tier TEXT,
                    ceiling_effort TEXT,
                    max_calls INTEGER,
                    max_total_tokens INTEGER,
                    max_input_tokens INTEGER,
                    max_output_tokens INTEGER,
                    max_cache_creation_tokens INTEGER,
                    max_cache_read_tokens INTEGER,
                    max_reasoning_tokens INTEGER,
                    max_embedding_inputs INTEGER,
                    max_wall_time_ms INTEGER,
                    max_concurrency INTEGER,
                    max_retries INTEGER,
                    max_calls_per_window INTEGER,
                    rate_window_seconds INTEGER,
                    updated_at TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS model_budget_counters (
                    counter_key TEXT PRIMARY KEY,
                    scope_kind TEXT NOT NULL,
                    scope_id TEXT NOT NULL,
                    purpose TEXT NOT NULL,
                    model_tier TEXT NOT NULL,
                    effort TEXT,
                    call_count INTEGER NOT NULL DEFAULT 0,
                    active_count INTEGER NOT NULL DEFAULT 0,
                    input_tokens INTEGER NOT NULL DEFAULT 0,
                    output_tokens INTEGER NOT NULL DEFAULT 0,
                    cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
                    cache_read_tokens INTEGER NOT NULL DEFAULT 0,
                    reasoning_tokens INTEGER NOT NULL DEFAULT 0,
                    embedding_inputs INTEGER NOT NULL DEFAULT 0,
                    wall_time_ms INTEGER NOT NULL DEFAULT 0,
                    rate_window_started_at TEXT,
                    rate_window_call_count INTEGER NOT NULL DEFAULT 0,
                    updated_at TEXT NOT NULL
                );
                ",
            )?;

            for (column, col_type) in [
                ("reserved_input_tokens", "INTEGER NOT NULL DEFAULT 0"),
                ("reserved_output_tokens", "INTEGER NOT NULL DEFAULT 0"),
                (
                    "reserved_cache_creation_tokens",
                    "INTEGER NOT NULL DEFAULT 0",
                ),
                ("reserved_cache_read_tokens", "INTEGER NOT NULL DEFAULT 0"),
                ("reserved_reasoning_tokens", "INTEGER NOT NULL DEFAULT 0"),
                (
                    "reserved_embedding_input_count",
                    "INTEGER NOT NULL DEFAULT 0",
                ),
                ("reserved_wall_time_ms", "INTEGER NOT NULL DEFAULT 0"),
                ("baseline_input_tokens", "INTEGER NOT NULL DEFAULT 0"),
                ("baseline_output_tokens", "INTEGER NOT NULL DEFAULT 0"),
                (
                    "baseline_cache_creation_tokens",
                    "INTEGER NOT NULL DEFAULT 0",
                ),
                ("baseline_cache_read_tokens", "INTEGER NOT NULL DEFAULT 0"),
                ("baseline_reasoning_tokens", "INTEGER NOT NULL DEFAULT 0"),
                (
                    "baseline_embedding_input_count",
                    "INTEGER NOT NULL DEFAULT 0",
                ),
                ("baseline_wall_time_ms", "INTEGER NOT NULL DEFAULT 0"),
            ] {
                add_column_if_not_exists_tx(&tx, "model_invocations", column, col_type)?;
            }

            if sqlite_table_exists_tx(&tx, "sessions")? {
                let exprs = v73_session_exprs(&tx)?;
                let backfill_sql = format!(
                    "
                    INSERT INTO model_invocations (
                        id, purpose, invocation_kind, foreground, paid_risk, admission_status, status,
                        provider, model, backend, model_tier, effort, trigger_source,
                        session_id, project_id, workflow_id, scheduled_job_id,
                        issue_tracker_id, issue_identifier, policy_snapshot_json,
                        input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
                        wall_time_ms, estimated_cost_usd, usage_confidence, dedup_key, created_at, started_at, completed_at
                    )
                    SELECT
                        lower(substr(hex(randomblob(16)),1,8) || '-' ||
                              substr(hex(randomblob(16)),1,4) || '-' ||
                              substr(hex(randomblob(16)),1,4) || '-' ||
                              substr(hex(randomblob(16)),1,4) || '-' ||
                              substr(hex(randomblob(16)),1,12)),
                        'session.launch.fresh',
                        'session_lifecycle',
                        'foreground',
                        'paid_capable',
                        'admitted',
                        CASE
                            WHEN {status_expr} = 'Failed' THEN 'failed'
                            WHEN {status_expr} IN ('Completed', 'Archived', 'Interrupted') THEN 'completed'
                            ELSE 'completed'
                        END,
                        {provider_expr},
                        {model_expr},
                        {backend_expr},
                        {model_tier_expr},
                        {effort_expr},
                        'legacy_backfill',
                        id,
                        {project_id_expr},
                        {workflow_id_expr},
                        {scheduled_job_id_expr},
                        {issue_tracker_id_expr},
                        {issue_identifier_expr},
                        json('{{\"source\":\"v73_backfill\"}}'),
                        {total_input_tokens_expr},
                        {total_output_tokens_expr},
                        {total_cache_creation_tokens_expr},
                        {total_cache_read_tokens_expr},
                        {work_time_ms_expr},
                        {cost_usd_expr},
                        CASE
                            WHEN {total_input_tokens_expr} IS NOT NULL OR {total_output_tokens_expr} IS NOT NULL THEN 'measured'
                            ELSE 'unavailable'
                        END,
                        'legacy-session:' || id,
                        {created_at_expr},
                        {created_at_expr},
                        {updated_at_expr}
                    FROM sessions
                    WHERE {session_kind_filter}
                      AND model_invocation_id IS NULL
                      AND NOT EXISTS (
                          SELECT 1
                          FROM model_invocations
                          WHERE dedup_key = 'legacy-session:' || sessions.id
                      );

                    UPDATE sessions
                    SET model_invocation_id = (
                        SELECT id FROM model_invocations
                        WHERE model_invocations.session_id = sessions.id
                          AND model_invocations.dedup_key = 'legacy-session:' || sessions.id
                        LIMIT 1
                    )
                    WHERE model_invocation_id IS NULL
                      AND {session_kind_filter};
                    ",
                    status_expr = exprs.status_expr,
                    provider_expr = exprs.provider_expr,
                    model_expr = exprs.model_expr,
                    backend_expr = exprs.backend_expr,
                    model_tier_expr = exprs.model_tier_expr,
                    effort_expr = exprs.effort_expr,
                    project_id_expr = exprs.project_id_expr,
                    workflow_id_expr = exprs.workflow_id_expr,
                    scheduled_job_id_expr = exprs.scheduled_job_id_expr,
                    issue_tracker_id_expr = exprs.issue_tracker_id_expr,
                    issue_identifier_expr = exprs.issue_identifier_expr,
                    total_input_tokens_expr = exprs.total_input_tokens_expr,
                    total_output_tokens_expr = exprs.total_output_tokens_expr,
                    total_cache_creation_tokens_expr = exprs.total_cache_creation_tokens_expr,
                    total_cache_read_tokens_expr = exprs.total_cache_read_tokens_expr,
                    work_time_ms_expr = exprs.work_time_ms_expr,
                    cost_usd_expr = exprs.cost_usd_expr,
                    created_at_expr = exprs.created_at_expr,
                    updated_at_expr = exprs.updated_at_expr,
                    session_kind_filter = exprs.session_kind_filter,
                );
                tx.execute_batch(&backfill_sql)?;
            }

            tx.execute(
                "INSERT INTO daemon_settings (key, value, updated_at)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(key) DO NOTHING",
                params![
                    crate::store::model_control::KEY_MODEL_CONTROL_MODE,
                    "normal",
                    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
                ],
            )?;
            tx.execute("PRAGMA user_version = 73", [])?;
            tx.commit()?;
        }

        self.repair_v73_model_control_schema()?;

        if version < 74 {
            let tx = self.conn.unchecked_transaction()?;
            add_column_if_not_exists_tx(
                &tx,
                "model_budget_policies",
                "alert_threshold_ratio",
                "REAL",
            )?;
            add_column_if_not_exists_tx(
                &tx,
                "model_invocations",
                "cancellation_requested_at",
                "TEXT",
            )?;
            add_column_if_not_exists_tx(&tx, "model_invocations", "cancellation_reason", "TEXT")?;
            add_column_if_not_exists_tx(
                &tx,
                "model_invocations",
                "cancellation_mechanism",
                "TEXT",
            )?;
            tx.execute_batch(
                "
                CREATE TABLE IF NOT EXISTS model_budget_alert_events (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    invocation_id TEXT NOT NULL,
                    scope_kind TEXT NOT NULL,
                    scope_id TEXT NOT NULL,
                    purpose TEXT NOT NULL,
                    metric TEXT NOT NULL,
                    remaining INTEGER NOT NULL,
                    limit_value INTEGER NOT NULL,
                    threshold INTEGER NOT NULL,
                    emitted_at TEXT NOT NULL,
                    UNIQUE(invocation_id, scope_kind, scope_id, purpose, metric, threshold)
                );
                CREATE INDEX IF NOT EXISTS idx_model_budget_alert_events_emitted_at
                    ON model_budget_alert_events(emitted_at DESC, id DESC);
                ",
            )?;
            tx.execute("PRAGMA user_version = 74", [])?;
            tx.commit()?;
        }

        // V75: additive Idea identity and compatibility kernel (D01).
        if version < 75 {
            let tx = self.conn.unchecked_transaction()?;
            tx.execute_batch(
                "
                CREATE TABLE IF NOT EXISTS captures (
                    id TEXT PRIMARY KEY CHECK(length(id) = 36 AND id = lower(id)),
                    project_id TEXT NOT NULL
                        CHECK(length(project_id) = 36 AND project_id = lower(project_id)),
                    creator_kind TEXT NOT NULL
                        CHECK(creator_kind IN ('operator', 'session', 'system')),
                    creator_id TEXT NOT NULL CHECK(length(trim(creator_id)) > 0),
                    captured_at TEXT NOT NULL CHECK(length(trim(captured_at)) > 0),
                    source_kind TEXT NOT NULL CHECK(source_kind IN (
                        'operator_input', 'session_artifact',
                        'imported_artifact', 'legacy_reference'
                    )),
                    raw_content_digest TEXT NOT NULL CHECK(
                        length(raw_content_digest) = 71
                        AND substr(raw_content_digest, 1, 7) = 'sha256:'
                        AND substr(raw_content_digest, 8) NOT GLOB '*[^0-9a-f]*'
                    ),
                    storage_policy_id TEXT NOT NULL
                        CHECK(length(trim(storage_policy_id)) > 0),
                    content_ref TEXT NOT NULL CHECK(
                        length(content_ref) = 77
                        AND substr(content_ref, 1, 13) = 'cas://sha256/'
                        AND substr(content_ref, 14) NOT GLOB '*[^0-9a-f]*'
                        AND content_ref = 'cas://sha256/' || substr(raw_content_digest, 8)
                    ),
                    UNIQUE(id, project_id),
                    FOREIGN KEY(project_id) REFERENCES projects(id) ON DELETE RESTRICT
                );
                CREATE INDEX IF NOT EXISTS idx_captures_project_captured_at
                    ON captures(project_id, captured_at, id);
                CREATE TRIGGER IF NOT EXISTS captures_immutable_update
                BEFORE UPDATE ON captures BEGIN
                    SELECT RAISE(ABORT, 'captures are immutable');
                END;
                CREATE TRIGGER IF NOT EXISTS captures_no_delete
                BEFORE DELETE ON captures BEGIN
                    SELECT RAISE(ABORT, 'captures cannot be deleted');
                END;

                CREATE TABLE IF NOT EXISTS ideas (
                    id TEXT PRIMARY KEY CHECK(length(id) = 36 AND id = lower(id)),
                    project_id TEXT NOT NULL
                        CHECK(length(project_id) = 36 AND project_id = lower(project_id)),
                    slug TEXT NOT NULL CHECK(
                        length(trim(slug)) > 0
                        AND length(CAST(slug AS BLOB)) <= 128
                    ),
                    sigil TEXT CHECK(
                        sigil IS NULL
                        OR (length(trim(sigil)) > 0
                            AND length(CAST(sigil AS BLOB)) <= 64)
                    ),
                    genesis_capture_id TEXT NOT NULL CHECK(
                        length(genesis_capture_id) = 36
                        AND genesis_capture_id = lower(genesis_capture_id)
                    ),
                    genesis_span_start INTEGER,
                    genesis_span_end INTEGER,
                    genesis_span_digest TEXT,
                    title TEXT NOT NULL CHECK(
                        length(trim(title)) > 0
                        AND length(CAST(title AS BLOB)) <= 512
                    ),
                    description TEXT NOT NULL
                        CHECK(length(CAST(description AS BLOB)) <= 65536),
                    portfolio_summary TEXT NOT NULL
                        CHECK(length(CAST(portfolio_summary AS BLOB)) <= 2048),
                    lifecycle TEXT NOT NULL CHECK(lifecycle IN (
                        'Open', 'Parked', 'Completed', 'Abandoned', 'Superseded'
                    )),
                    stage TEXT NOT NULL CHECK(stage IN (
                        'Captured', 'Shaping', 'Researching', 'Planned',
                        'Implementing', 'Integrating', 'Verifying', 'Released'
                    )),
                    priority INTEGER NOT NULL,
                    autonomy_policy TEXT NOT NULL CHECK(autonomy_policy IN (
                        'CaptureOnly', 'Research', 'PlanAndWait', 'Sandbox',
                        'IntegrateIdeaBranch', 'PromoteProjectTarget', 'ExternalEffects'
                    )),
                    integration_target_ref TEXT NOT NULL
                        CHECK(length(trim(integration_target_ref)) > 0),
                    program_template_policy_id TEXT CHECK(
                        program_template_policy_id IS NULL
                        OR length(trim(program_template_policy_id)) > 0
                    ),
                    current_controller_session_id TEXT CHECK(
                        current_controller_session_id IS NULL
                        OR (length(current_controller_session_id) = 36
                            AND current_controller_session_id = lower(current_controller_session_id))
                    ),
                    controller_epoch INTEGER NOT NULL CHECK(controller_epoch >= 0),
                    row_version INTEGER NOT NULL CHECK(row_version >= 0),
                    next_event_sequence INTEGER NOT NULL CHECK(next_event_sequence > 0),
                    created_at TEXT NOT NULL CHECK(length(trim(created_at)) > 0),
                    updated_at TEXT NOT NULL CHECK(length(trim(updated_at)) > 0),
                    terminal_at TEXT,
                    superseded_at TEXT,
                    UNIQUE(id, project_id),
                    UNIQUE(project_id, slug),
                    CHECK(
                        (genesis_span_start IS NULL
                         AND genesis_span_end IS NULL
                         AND genesis_span_digest IS NULL)
                        OR
                        (genesis_span_start IS NOT NULL
                         AND genesis_span_end IS NOT NULL
                         AND genesis_span_digest IS NOT NULL
                         AND genesis_span_start >= 0
                         AND genesis_span_end > genesis_span_start
                         AND length(genesis_span_digest) = 71
                         AND substr(genesis_span_digest, 1, 7) = 'sha256:'
                         AND substr(genesis_span_digest, 8) NOT GLOB '*[^0-9a-f]*')
                    ),
                    FOREIGN KEY(project_id) REFERENCES projects(id) ON DELETE RESTRICT,
                    FOREIGN KEY(genesis_capture_id, project_id)
                        REFERENCES captures(id, project_id) ON DELETE RESTRICT,
                    FOREIGN KEY(current_controller_session_id)
                        REFERENCES sessions(id) ON DELETE RESTRICT
                );
                CREATE INDEX IF NOT EXISTS idx_ideas_project_state
                    ON ideas(project_id, lifecycle, stage, priority, id);
                CREATE INDEX IF NOT EXISTS idx_ideas_controller_session
                    ON ideas(current_controller_session_id);
                CREATE TRIGGER IF NOT EXISTS ideas_genesis_immutable
                BEFORE UPDATE ON ideas
                WHEN NEW.id != OLD.id
                  OR NEW.project_id != OLD.project_id
                  OR NEW.genesis_capture_id != OLD.genesis_capture_id
                  OR NEW.genesis_span_start IS NOT OLD.genesis_span_start
                  OR NEW.genesis_span_end IS NOT OLD.genesis_span_end
                  OR NEW.genesis_span_digest IS NOT OLD.genesis_span_digest
                BEGIN
                    SELECT RAISE(ABORT, 'idea identity and genesis are immutable');
                END;
                CREATE TRIGGER IF NOT EXISTS ideas_no_delete
                BEFORE DELETE ON ideas BEGIN
                    SELECT RAISE(ABORT, 'ideas cannot be deleted');
                END;

                CREATE TABLE IF NOT EXISTS idea_events (
                    id TEXT PRIMARY KEY CHECK(length(id) = 36 AND id = lower(id)),
                    project_id TEXT NOT NULL
                        CHECK(length(project_id) = 36 AND project_id = lower(project_id)),
                    idea_id TEXT NOT NULL
                        CHECK(length(idea_id) = 36 AND idea_id = lower(idea_id)),
                    sequence INTEGER NOT NULL CHECK(sequence > 0),
                    event_type TEXT NOT NULL CHECK(event_type IN (
                        'created', 'decision_recorded', 'projection_changed',
                        'lifecycle_transitioned', 'stage_transitioned',
                        'autonomy_changed', 'scope_changed', 'controller_reserved',
                        'controller_assigned', 'controller_released', 'question_asked',
                        'question_answered', 'issue_linked', 'artifact_sealed',
                        'finding_recorded', 'verdict_recorded', 'program_transitioned',
                        'gate_transitioned', 'integration_recorded', 'release_recorded',
                        'relationship_changed', 'split', 'superseded', 'failed', 'abandoned'
                    )),
                    actor_kind TEXT NOT NULL
                        CHECK(actor_kind IN ('operator', 'session', 'system')),
                    actor_id TEXT NOT NULL CHECK(length(trim(actor_id)) > 0),
                    controller_session_id TEXT CHECK(
                        controller_session_id IS NULL
                        OR (length(controller_session_id) = 36
                            AND controller_session_id = lower(controller_session_id))
                    ),
                    controller_epoch INTEGER CHECK(
                        controller_epoch IS NULL OR controller_epoch >= 0
                    ),
                    expected_row_version INTEGER NOT NULL
                        CHECK(expected_row_version >= 0),
                    resulting_row_version INTEGER NOT NULL
                        CHECK(resulting_row_version >= 0),
                    idempotency_key TEXT NOT NULL
                        CHECK(length(trim(idempotency_key)) > 0),
                    occurred_at TEXT NOT NULL CHECK(length(trim(occurred_at)) > 0),
                    payload_json TEXT NOT NULL CHECK(json_valid(payload_json)),
                    artifact_digests_json TEXT NOT NULL
                        CHECK(json_valid(artifact_digests_json)),
                    evidence_digests_json TEXT NOT NULL
                        CHECK(json_valid(evidence_digests_json)),
                    UNIQUE(id, project_id),
                    UNIQUE(id, idea_id, project_id),
                    UNIQUE(idea_id, sequence),
                    UNIQUE(idea_id, idempotency_key),
                    FOREIGN KEY(idea_id, project_id)
                        REFERENCES ideas(id, project_id) ON DELETE RESTRICT,
                    FOREIGN KEY(controller_session_id)
                        REFERENCES sessions(id) ON DELETE RESTRICT
                );
                CREATE INDEX IF NOT EXISTS idx_idea_events_idea_sequence
                    ON idea_events(idea_id, sequence);
                CREATE TRIGGER IF NOT EXISTS idea_events_append_only_update
                BEFORE UPDATE ON idea_events BEGIN
                    SELECT RAISE(ABORT, 'idea events are append-only');
                END;
                CREATE TRIGGER IF NOT EXISTS idea_events_no_delete
                BEFORE DELETE ON idea_events BEGIN
                    SELECT RAISE(ABORT, 'idea events cannot be deleted');
                END;

                CREATE TABLE IF NOT EXISTS idea_relationships (
                    id TEXT PRIMARY KEY CHECK(length(id) = 36 AND id = lower(id)),
                    project_id TEXT NOT NULL
                        CHECK(length(project_id) = 36 AND project_id = lower(project_id)),
                    source_idea_id TEXT NOT NULL,
                    target_idea_id TEXT NOT NULL,
                    kind TEXT NOT NULL
                        CHECK(kind IN ('depends_on', 'supersedes', 'derived_from')),
                    created_event_id TEXT NOT NULL,
                    created_at TEXT NOT NULL CHECK(length(trim(created_at)) > 0),
                    removed_event_id TEXT,
                    removed_at TEXT,
                    CHECK(source_idea_id != target_idea_id),
                    CHECK(
                        (removed_event_id IS NULL AND removed_at IS NULL)
                        OR (removed_event_id IS NOT NULL AND removed_at IS NOT NULL)
                    ),
                    FOREIGN KEY(project_id) REFERENCES projects(id) ON DELETE RESTRICT,
                    FOREIGN KEY(source_idea_id, project_id)
                        REFERENCES ideas(id, project_id) ON DELETE RESTRICT,
                    FOREIGN KEY(target_idea_id, project_id)
                        REFERENCES ideas(id, project_id) ON DELETE RESTRICT,
                    FOREIGN KEY(created_event_id, source_idea_id, project_id)
                        REFERENCES idea_events(id, idea_id, project_id) ON DELETE RESTRICT,
                    FOREIGN KEY(removed_event_id, source_idea_id, project_id)
                        REFERENCES idea_events(id, idea_id, project_id) ON DELETE RESTRICT
                );
                CREATE INDEX IF NOT EXISTS idx_idea_relationships_source
                    ON idea_relationships(source_idea_id, kind, target_idea_id);
                CREATE INDEX IF NOT EXISTS idx_idea_relationships_target
                    ON idea_relationships(target_idea_id, kind, source_idea_id);
                CREATE UNIQUE INDEX IF NOT EXISTS idx_idea_relationships_active
                    ON idea_relationships(source_idea_id, target_idea_id, kind)
                    WHERE removed_at IS NULL;
                CREATE TRIGGER IF NOT EXISTS idea_relationships_identity_immutable
                BEFORE UPDATE ON idea_relationships
                WHEN NEW.id != OLD.id
                  OR NEW.project_id != OLD.project_id
                  OR NEW.source_idea_id != OLD.source_idea_id
                  OR NEW.target_idea_id != OLD.target_idea_id
                  OR NEW.kind != OLD.kind
                  OR NEW.created_event_id != OLD.created_event_id
                  OR NEW.created_at != OLD.created_at
                  OR (OLD.removed_event_id IS NOT NULL
                      AND (NEW.removed_event_id IS NOT OLD.removed_event_id
                           OR NEW.removed_at IS NOT OLD.removed_at))
                BEGIN
                    SELECT RAISE(ABORT, 'relationship identity and creation are immutable');
                END;
                CREATE TRIGGER IF NOT EXISTS idea_relationships_no_delete
                BEFORE DELETE ON idea_relationships BEGIN
                    SELECT RAISE(ABORT, 'idea relationships cannot be deleted');
                END;

                CREATE TABLE IF NOT EXISTS idea_collections (
                    id TEXT PRIMARY KEY CHECK(length(id) = 36 AND id = lower(id)),
                    project_id TEXT NOT NULL
                        CHECK(length(project_id) = 36 AND project_id = lower(project_id)),
                    slug TEXT NOT NULL CHECK(
                        length(trim(slug)) > 0
                        AND length(CAST(slug AS BLOB)) <= 128
                    ),
                    name TEXT NOT NULL CHECK(
                        length(trim(name)) > 0
                        AND length(CAST(name AS BLOB)) <= 512
                    ),
                    description TEXT CHECK(
                        description IS NULL
                        OR length(CAST(description AS BLOB)) <= 65536
                    ),
                    created_at TEXT NOT NULL CHECK(length(trim(created_at)) > 0),
                    updated_at TEXT NOT NULL CHECK(length(trim(updated_at)) > 0),
                    retired_at TEXT,
                    UNIQUE(id, project_id),
                    UNIQUE(project_id, slug),
                    FOREIGN KEY(project_id) REFERENCES projects(id) ON DELETE RESTRICT
                );
                CREATE INDEX IF NOT EXISTS idx_idea_collections_project
                    ON idea_collections(project_id, retired_at, slug, id);
                CREATE TRIGGER IF NOT EXISTS idea_collections_no_delete
                BEFORE DELETE ON idea_collections BEGIN
                    SELECT RAISE(ABORT, 'idea collections cannot be deleted');
                END;

                CREATE TABLE IF NOT EXISTS idea_collection_memberships (
                    id TEXT PRIMARY KEY CHECK(length(id) = 36 AND id = lower(id)),
                    project_id TEXT NOT NULL
                        CHECK(length(project_id) = 36 AND project_id = lower(project_id)),
                    collection_id TEXT NOT NULL,
                    idea_id TEXT NOT NULL,
                    added_at TEXT NOT NULL CHECK(length(trim(added_at)) > 0),
                    removed_at TEXT,
                    FOREIGN KEY(project_id) REFERENCES projects(id) ON DELETE RESTRICT,
                    FOREIGN KEY(collection_id, project_id)
                        REFERENCES idea_collections(id, project_id) ON DELETE RESTRICT,
                    FOREIGN KEY(idea_id, project_id)
                        REFERENCES ideas(id, project_id) ON DELETE RESTRICT
                );
                CREATE INDEX IF NOT EXISTS idx_idea_memberships_collection
                    ON idea_collection_memberships(collection_id, removed_at, idea_id);
                CREATE INDEX IF NOT EXISTS idx_idea_memberships_idea
                    ON idea_collection_memberships(idea_id, removed_at, collection_id);
                CREATE UNIQUE INDEX IF NOT EXISTS idx_idea_memberships_active
                    ON idea_collection_memberships(collection_id, idea_id)
                    WHERE removed_at IS NULL;
                CREATE TRIGGER IF NOT EXISTS idea_memberships_identity_immutable
                BEFORE UPDATE ON idea_collection_memberships
                WHEN NEW.id != OLD.id
                  OR NEW.project_id != OLD.project_id
                  OR NEW.collection_id != OLD.collection_id
                  OR NEW.idea_id != OLD.idea_id
                  OR NEW.added_at != OLD.added_at
                  OR (OLD.removed_at IS NOT NULL
                      AND NEW.removed_at IS NOT OLD.removed_at)
                BEGIN
                    SELECT RAISE(ABORT, 'membership identity and creation are immutable');
                END;
                CREATE TRIGGER IF NOT EXISTS idea_memberships_no_delete
                BEFORE DELETE ON idea_collection_memberships BEGIN
                    SELECT RAISE(ABORT, 'idea collection memberships cannot be deleted');
                END;

                CREATE TABLE IF NOT EXISTS idea_compatibility_mappings (
                    id TEXT PRIMARY KEY CHECK(length(id) = 36 AND id = lower(id)),
                    project_id TEXT NOT NULL
                        CHECK(length(project_id) = 36 AND project_id = lower(project_id)),
                    legacy_source_kind TEXT NOT NULL CHECK(legacy_source_kind IN (
                        'session_group', 'session_epic', 'session_label', 'session_child'
                    )),
                    legacy_source_id TEXT NOT NULL CHECK(
                        length(legacy_source_id) = 36
                        AND legacy_source_id = lower(legacy_source_id)
                    ),
                    idea_id TEXT,
                    collection_id TEXT,
                    status TEXT NOT NULL
                        CHECK(status IN ('pending', 'mapped', 'blocked', 'excluded')),
                    provenance_json TEXT NOT NULL CHECK(json_valid(provenance_json)),
                    disposition TEXT,
                    created_at TEXT NOT NULL CHECK(length(trim(created_at)) > 0),
                    updated_at TEXT NOT NULL CHECK(length(trim(updated_at)) > 0),
                    mapped_at TEXT,
                    UNIQUE(legacy_source_kind, legacy_source_id),
                    CHECK(idea_id IS NULL OR collection_id IS NULL),
                    CHECK(
                        (status = 'mapped'
                         AND (idea_id IS NOT NULL OR collection_id IS NOT NULL)
                         AND mapped_at IS NOT NULL)
                        OR
                        (status != 'mapped'
                         AND idea_id IS NULL
                         AND collection_id IS NULL
                         AND mapped_at IS NULL)
                    ),
                    FOREIGN KEY(project_id) REFERENCES projects(id) ON DELETE RESTRICT,
                    FOREIGN KEY(idea_id, project_id)
                        REFERENCES ideas(id, project_id) ON DELETE RESTRICT,
                    FOREIGN KEY(collection_id, project_id)
                        REFERENCES idea_collections(id, project_id) ON DELETE RESTRICT
                );
                CREATE INDEX IF NOT EXISTS idx_idea_compatibility_project_status
                    ON idea_compatibility_mappings(project_id, status, id);
                CREATE INDEX IF NOT EXISTS idx_idea_compatibility_idea
                    ON idea_compatibility_mappings(idea_id);
                CREATE INDEX IF NOT EXISTS idx_idea_compatibility_collection
                    ON idea_compatibility_mappings(collection_id);
                CREATE TRIGGER IF NOT EXISTS idea_compatibility_source_immutable
                BEFORE UPDATE ON idea_compatibility_mappings
                WHEN NEW.id != OLD.id
                  OR NEW.project_id != OLD.project_id
                  OR NEW.legacy_source_kind != OLD.legacy_source_kind
                  OR NEW.legacy_source_id != OLD.legacy_source_id
                  OR NEW.created_at != OLD.created_at
                BEGIN
                    SELECT RAISE(ABORT, 'compatibility source identity is immutable');
                END;
                CREATE TRIGGER IF NOT EXISTS idea_compatibility_no_delete
                BEFORE DELETE ON idea_compatibility_mappings BEGIN
                    SELECT RAISE(ABORT, 'idea compatibility mappings cannot be deleted');
                END;
                ",
            )?;
            tx.execute("PRAGMA user_version = 75", [])?;
            tx.commit()?;
        }

        // V76: bounded D03 controller-tail lookup.
        if version < 76 {
            let tx = self.conn.unchecked_transaction()?;
            tx.execute_batch(
                "
                CREATE INDEX IF NOT EXISTS idx_idea_events_controller_tail
                    ON idea_events(idea_id, sequence DESC)
                    WHERE event_type IN (
                        'controller_reserved',
                        'controller_assigned',
                        'controller_released'
                    );
                ",
            )?;
            tx.execute("PRAGMA user_version = 76", [])?;
            tx.commit()?;
        }

        // V77: Issues are immutable project-owned obligations.  This is a
        // rebuild rather than an ALTER because the ownership and provenance
        // constraints must be true for every historical row.
        if version < 77 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            if active_version != 76 {
                return Err(crate::error::DaemonError::Store(format!(
                    "V77 requires exact V76 source, found V{active_version}"
                )));
            }
            let legacy_issue_columns = tx
                .prepare("SELECT name FROM pragma_table_info('issues') ORDER BY cid")?
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let legacy_dep_columns = tx
                .prepare("SELECT name FROM pragma_table_info('issue_deps') ORDER BY cid")?
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            if legacy_issue_columns
                != [
                    "id",
                    "display_number",
                    "title",
                    "body",
                    "status",
                    "priority",
                    "labels",
                    "created_by_session_id",
                    "assignee",
                    "created_at",
                    "updated_at",
                    "closed_at",
                ]
                || legacy_dep_columns != ["issue_id", "depends_on_id", "created_at"]
            {
                return Err(crate::error::DaemonError::Store(
                    "V77 legacy issues/issue_deps schema fingerprint mismatch".to_string(),
                ));
            }
            if !d04_v76_issue_catalog_matches(&tx)? {
                return Err(crate::error::DaemonError::Store(
                    "V77 legacy issues/issue_deps constraints or indexes fingerprint mismatch"
                        .to_string(),
                ));
            }

            // Provenance that names a session the database cannot resolve to a
            // project is referential damage: the row asserts an origin nothing
            // corroborates. That is never repaired by guessing an owner.
            let unresolved: Option<String> = tx
                .query_row(
                    "SELECT i.id
                     FROM issues i
                     LEFT JOIN sessions s ON s.id = i.created_by_session_id
                     LEFT JOIN projects p ON p.id = s.project_id
                     WHERE i.created_by_session_id IS NOT NULL
                       AND (s.id IS NULL OR s.project_id IS NULL OR p.id IS NULL)
                     LIMIT 1",
                    [],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(issue_id) = unresolved {
                return Err(crate::error::DaemonError::Store(format!(
                    "V77 issue project resolver failed for issue {issue_id}: \
                     created_by_session_id names a session with no resolvable project"
                )));
            }

            // Absent provenance is a different fact from damaged provenance.
            // Operator-created issues never had a session, so their owner is
            // unknowable from the V76 schema and must be declared, not inferred.
            // The migration refuses rather than mis-attributing, and names every
            // affected row so the declaration is a single informed decision.
            let unowned: Vec<String> = tx
                .prepare(
                    "SELECT '#' || i.display_number || ' ' || i.id
                     FROM issues i
                     WHERE i.created_by_session_id IS NULL
                     ORDER BY i.display_number",
                )?
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let unowned_project: Option<String> = if unowned.is_empty() {
                None
            } else {
                let declared = std::env::var(V77_UNOWNED_ISSUE_PROJECT_ENV)
                    .ok()
                    .map(|value| value.trim().to_lowercase())
                    .filter(|value| !value.is_empty());
                let Some(project_id) = declared else {
                    return Err(crate::error::DaemonError::Store(format!(
                        "V77 cannot attribute {} issue(s) that have no creating session. \
                         Set {}=<project-uuid> to the owning project and restart. \
                         Affected: {}",
                        unowned.len(),
                        V77_UNOWNED_ISSUE_PROJECT_ENV,
                        unowned.join(", ")
                    )));
                };
                let known: i64 = tx.query_row(
                    "SELECT count(*) FROM projects WHERE id = ?1",
                    [&project_id],
                    |row| row.get(0),
                )?;
                if known != 1 {
                    return Err(crate::error::DaemonError::Store(format!(
                        "V77 {V77_UNOWNED_ISSUE_PROJECT_ENV}={project_id} is not an existing project"
                    )));
                }
                Some(project_id)
            };

            let cross_project_dependency: Option<String> = tx
                .query_row(
                    "SELECT d.issue_id
                     FROM issue_deps d
                     LEFT JOIN issues i ON i.id = d.issue_id
                     LEFT JOIN sessions si ON si.id = i.created_by_session_id
                     LEFT JOIN issues b ON b.id = d.depends_on_id
                     LEFT JOIN sessions sb ON sb.id = b.created_by_session_id
                     WHERE i.id IS NULL OR b.id IS NULL
                        OR COALESCE(si.project_id, ?1) IS NULL
                        OR COALESCE(sb.project_id, ?1) IS NULL
                        OR COALESCE(si.project_id, ?1) != COALESCE(sb.project_id, ?1)
                     LIMIT 1",
                    params![unowned_project],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(issue_id) = cross_project_dependency {
                return Err(crate::error::DaemonError::Store(format!(
                    "V77 cross-project dependency resolver failed for issue {issue_id}"
                )));
            }
            #[cfg(test)]
            d04_migration_fault(D04MigrationFault::AfterPreflight)?;

            tx.execute_batch(
                "CREATE TABLE issues_v77 (
                    id TEXT PRIMARY KEY CHECK(length(id) = 36 AND id = lower(id)),
                    project_id TEXT NOT NULL CHECK(length(project_id) = 36 AND project_id = lower(project_id)),
                    display_number INTEGER NOT NULL UNIQUE,
                    title TEXT NOT NULL,
                    body TEXT NOT NULL DEFAULT '',
                    status TEXT NOT NULL DEFAULT 'Open'
                        CHECK(status IN ('Open','InProgress','Closed','Cancelled')),
                    priority INTEGER CHECK(priority IS NULL OR priority BETWEEN 1 AND 4),
                    labels TEXT NOT NULL DEFAULT '[]' CHECK(json_valid(labels)),
                    created_by_session_id TEXT,
                    assignee TEXT,
                    idea_id TEXT CHECK(idea_id IS NULL OR (length(idea_id) = 36 AND idea_id = lower(idea_id))),
                    source_event_id TEXT CHECK(source_event_id IS NULL OR (length(source_event_id) = 36 AND source_event_id = lower(source_event_id))),
                    source_finding_ref TEXT CHECK(source_finding_ref IS NULL OR (
                        length(CAST(source_finding_ref AS BLOB)) BETWEEN 84 AND 211
                        AND substr(source_finding_ref, 1, 18) = 'finding:v1:sha256:'
                        AND length(substr(source_finding_ref, 19, 64)) = 64
                        AND substr(source_finding_ref, 19, 64) NOT GLOB '*[^0-9a-f]*'
                        AND substr(source_finding_ref, 83, 1) = ':'
                        AND length(substr(source_finding_ref, 84)) BETWEEN 1 AND 128
                        AND substr(source_finding_ref, 84) NOT GLOB '*[^A-Za-z0-9._-]*'
                    )),
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    closed_at TEXT,
                    UNIQUE(id, project_id),
                    CHECK(source_event_id IS NULL OR idea_id IS NOT NULL),
                    CHECK(source_finding_ref IS NULL OR idea_id IS NOT NULL),
                    FOREIGN KEY(project_id) REFERENCES projects(id) ON DELETE RESTRICT,
                    FOREIGN KEY(idea_id, project_id) REFERENCES ideas(id, project_id) ON DELETE RESTRICT,
                    FOREIGN KEY(source_event_id, idea_id, project_id)
                        REFERENCES idea_events(id, idea_id, project_id) ON DELETE RESTRICT
                );",
            )?;
            #[cfg(test)]
            d04_migration_fault(D04MigrationFault::AfterCreateIssues)?;
            tx.execute_batch(
                "CREATE TABLE issue_deps_v77 (
                    project_id TEXT NOT NULL CHECK(length(project_id) = 36 AND project_id = lower(project_id)),
                    issue_id TEXT NOT NULL,
                    depends_on_id TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    PRIMARY KEY(project_id, issue_id, depends_on_id),
                    CHECK(issue_id <> depends_on_id),
                    FOREIGN KEY(issue_id, project_id) REFERENCES issues_v77(id, project_id) ON DELETE CASCADE,
                    FOREIGN KEY(depends_on_id, project_id) REFERENCES issues_v77(id, project_id) ON DELETE CASCADE
                );"
            )?;
            #[cfg(test)]
            d04_migration_fault(D04MigrationFault::AfterCreateIssueDeps)?;
            // LEFT JOIN, not JOIN: an issue with no creating session still owes
            // its row. `created_by_session_id` stays NULL because that is the
            // true provenance; only `project_id` is supplied by declaration.
            tx.execute(
                "INSERT INTO issues_v77 (
                    id, project_id, display_number, title, body, status, priority, labels,
                    created_by_session_id, assignee, idea_id, source_event_id, source_finding_ref,
                    created_at, updated_at, closed_at
                  )
                  SELECT i.id, COALESCE(s.project_id, ?1), i.display_number, i.title, i.body,
                         i.status, i.priority, i.labels, i.created_by_session_id, i.assignee,
                         NULL, NULL, NULL, i.created_at, i.updated_at, i.closed_at
                  FROM issues i LEFT JOIN sessions s ON s.id = i.created_by_session_id",
                params![unowned_project],
            )?;
            #[cfg(test)]
            d04_migration_fault(D04MigrationFault::AfterCopyIssues)?;
            tx.execute(
                "INSERT INTO issue_deps_v77 (project_id, issue_id, depends_on_id, created_at)
                  SELECT COALESCE(si.project_id, ?1), d.issue_id, d.depends_on_id, d.created_at
                  FROM issue_deps d
                  JOIN issues i ON i.id = d.issue_id
                  LEFT JOIN sessions si ON si.id = i.created_by_session_id",
                params![unowned_project],
            )?;
            #[cfg(test)]
            d04_migration_fault(D04MigrationFault::AfterCopyIssueDeps)?;
            let old_issues: i64 =
                tx.query_row("SELECT count(*) FROM issues", [], |row| row.get(0))?;
            let new_issues: i64 =
                tx.query_row("SELECT count(*) FROM issues_v77", [], |row| row.get(0))?;
            let old_deps: i64 =
                tx.query_row("SELECT count(*) FROM issue_deps", [], |row| row.get(0))?;
            let new_deps: i64 =
                tx.query_row("SELECT count(*) FROM issue_deps_v77", [], |row| row.get(0))?;
            if old_issues != new_issues || old_deps != new_deps {
                return Err(crate::error::DaemonError::Store(
                    "V77 issue copy parity failed".to_string(),
                ));
            }
            let issue_copy_delta: i64 = tx.query_row(
                "SELECT count(*) FROM (
                    SELECT i.id, COALESCE(s.project_id, ?1) AS project_id, i.display_number,
                           i.title, i.body, i.status,
                           i.priority, i.labels, i.created_by_session_id, i.assignee,
                           NULL AS idea_id, NULL AS source_event_id, NULL AS source_finding_ref,
                           i.created_at, i.updated_at, i.closed_at
                    FROM issues i LEFT JOIN sessions s ON s.id = i.created_by_session_id
                    EXCEPT
                    SELECT id, project_id, display_number, title, body, status, priority, labels,
                           created_by_session_id, assignee, idea_id, source_event_id,
                           source_finding_ref, created_at, updated_at, closed_at
                    FROM issues_v77
                )",
                params![unowned_project],
                |row| row.get(0),
            )?;
            let issue_copy_reverse_delta: i64 = tx.query_row(
                "SELECT count(*) FROM (
                    SELECT id, project_id, display_number, title, body, status, priority, labels,
                           created_by_session_id, assignee, idea_id, source_event_id,
                           source_finding_ref, created_at, updated_at, closed_at
                    FROM issues_v77
                    EXCEPT
                    SELECT i.id, COALESCE(s.project_id, ?1) AS project_id, i.display_number,
                           i.title, i.body, i.status,
                           i.priority, i.labels, i.created_by_session_id, i.assignee,
                           NULL AS idea_id, NULL AS source_event_id, NULL AS source_finding_ref,
                           i.created_at, i.updated_at, i.closed_at
                    FROM issues i LEFT JOIN sessions s ON s.id = i.created_by_session_id
                )",
                params![unowned_project],
                |row| row.get(0),
            )?;
            let dep_copy_delta: i64 = tx.query_row(
                "SELECT count(*) FROM (
                    SELECT COALESCE(s.project_id, ?1) AS project_id, d.issue_id, d.depends_on_id,
                           d.created_at
                    FROM issue_deps d
                    JOIN issues i ON i.id = d.issue_id
                    LEFT JOIN sessions s ON s.id = i.created_by_session_id
                    EXCEPT
                    SELECT project_id, issue_id, depends_on_id, created_at FROM issue_deps_v77
                )",
                params![unowned_project],
                |row| row.get(0),
            )?;
            let dep_copy_reverse_delta: i64 = tx.query_row(
                "SELECT count(*) FROM (
                    SELECT project_id, issue_id, depends_on_id, created_at FROM issue_deps_v77
                    EXCEPT
                    SELECT COALESCE(s.project_id, ?1) AS project_id, d.issue_id, d.depends_on_id,
                           d.created_at
                    FROM issue_deps d
                    JOIN issues i ON i.id = d.issue_id
                    LEFT JOIN sessions s ON s.id = i.created_by_session_id
                )",
                params![unowned_project],
                |row| row.get(0),
            )?;
            if issue_copy_delta != 0
                || issue_copy_reverse_delta != 0
                || dep_copy_delta != 0
                || dep_copy_reverse_delta != 0
            {
                return Err(crate::error::DaemonError::Store(
                    "V77 issue copy EXCEPT parity failed".to_string(),
                ));
            }
            #[cfg(test)]
            d04_migration_fault(D04MigrationFault::AfterParityChecks)?;
            tx.execute("DROP TABLE issue_deps", [])?;
            #[cfg(test)]
            d04_migration_fault(D04MigrationFault::AfterDropIssueDeps)?;
            tx.execute("DROP TABLE issues", [])?;
            #[cfg(test)]
            d04_migration_fault(D04MigrationFault::AfterDropIssues)?;
            tx.execute("ALTER TABLE issues_v77 RENAME TO issues", [])?;
            #[cfg(test)]
            d04_migration_fault(D04MigrationFault::AfterRenameIssues)?;
            tx.execute("ALTER TABLE issue_deps_v77 RENAME TO issue_deps", [])?;
            #[cfg(test)]
            d04_migration_fault(D04MigrationFault::AfterRenameIssueDeps)?;

            tx.execute(
                "CREATE INDEX idx_issues_status ON issues(status, display_number)",
                [],
            )?;
            #[cfg(test)]
            d04_migration_fault(D04MigrationFault::AfterIndexIssuesStatus)?;
            tx.execute("CREATE INDEX idx_issues_created_by ON issues(created_by_session_id, display_number)", [])?;
            #[cfg(test)]
            d04_migration_fault(D04MigrationFault::AfterIndexIssuesCreatedBy)?;
            tx.execute(
                "CREATE INDEX idx_issues_project_display ON issues(project_id, display_number)",
                [],
            )?;
            #[cfg(test)]
            d04_migration_fault(D04MigrationFault::AfterIndexIssuesProjectDisplay)?;
            tx.execute("CREATE INDEX idx_issues_project_ready ON issues(project_id, status, (priority IS NULL), priority, created_at, id)", [])?;
            #[cfg(test)]
            d04_migration_fault(D04MigrationFault::AfterIndexIssuesProjectReady)?;
            tx.execute("CREATE INDEX idx_issues_project_creator ON issues(project_id, created_by_session_id, display_number)", [])?;
            #[cfg(test)]
            d04_migration_fault(D04MigrationFault::AfterIndexIssuesProjectCreator)?;
            tx.execute("CREATE INDEX idx_issues_project_idea ON issues(project_id, idea_id, display_number) WHERE idea_id IS NOT NULL", [])?;
            #[cfg(test)]
            d04_migration_fault(D04MigrationFault::AfterIndexIssuesProjectIdea)?;
            tx.execute("CREATE INDEX idx_issues_source_event ON issues(project_id, idea_id, source_event_id) WHERE source_event_id IS NOT NULL", [])?;
            #[cfg(test)]
            d04_migration_fault(D04MigrationFault::AfterIndexIssuesSourceEvent)?;
            tx.execute("CREATE INDEX idx_issues_source_finding ON issues(project_id, source_finding_ref) WHERE source_finding_ref IS NOT NULL", [])?;
            #[cfg(test)]
            d04_migration_fault(D04MigrationFault::AfterIndexIssuesSourceFinding)?;
            tx.execute("CREATE INDEX idx_issue_deps_blocker ON issue_deps(project_id, depends_on_id, issue_id)", [])?;
            #[cfg(test)]
            d04_migration_fault(D04MigrationFault::AfterIndexIssueDepsBlocker)?;
            tx.execute(
                "CREATE INDEX idx_issue_dispatches_active_tracker_session
                   ON issue_tracker_dispatches(tracker, issue_id, session_id)
                   WHERE terminal_state IS NULL",
                [],
            )?;
            #[cfg(test)]
            d04_migration_fault(D04MigrationFault::AfterIndexActiveDispatch)?;
            let foreign_key_errors: i64 =
                tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                    row.get(0)
                })?;
            if foreign_key_errors != 0 {
                return Err(crate::error::DaemonError::Store(
                    "V77 foreign-key check failed".to_string(),
                ));
            }
            #[cfg(test)]
            d04_migration_fault(D04MigrationFault::AfterForeignKeyCheck)?;
            tx.execute("PRAGMA user_version = 77", [])?;
            #[cfg(test)]
            d04_migration_fault(D04MigrationFault::AfterUserVersion)?;
            #[cfg(test)]
            d04_migration_fault(D04MigrationFault::BeforeCommit)?;
            tx.commit()?;
            tracing::info!("V77 migration complete: project-owned issue linkage schema");
        }

        // V78: D05 durable serial ProgramRun kernel. The exact-V77 guard is
        // temporal evidence: schema drift requires a new reviewed migration,
        // never an opportunistic renumber or partial repair.
        if version < 78 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            if active_version != 77 {
                return Err(crate::error::DaemonError::Store(format!(
                    "V78 requires exact V77 source, found V{active_version}"
                )));
            }
            #[cfg(test)]
            d05_migration_fault(D05MigrationFault::AfterExactSource)?;

            tx.execute_batch(
                "CREATE TABLE idea_program_runs (
                    id TEXT PRIMARY KEY CHECK(length(id)=36 AND id=lower(id)),
                    project_id TEXT NOT NULL CHECK(length(project_id)=36 AND project_id=lower(project_id)),
                    idea_id TEXT NOT NULL CHECK(length(idea_id)=36 AND idea_id=lower(idea_id)),
                    template_key TEXT NOT NULL CHECK(length(CAST(template_key AS BLOB)) BETWEEN 1 AND 64),
                    template_version INTEGER NOT NULL CHECK(template_version > 0),
                    template_digest TEXT NOT NULL CHECK(
                        length(template_digest)=71 AND substr(template_digest,1,7)='sha256:'
                        AND substr(template_digest,8) NOT GLOB '*[^0-9a-f]*'),
                    template_json TEXT NOT NULL CHECK(json_valid(template_json) AND length(CAST(template_json AS BLOB)) <= 1048576),
                    status TEXT NOT NULL CHECK(status IN (
                        'pending','ready','running','awaiting_gate','retry_pending',
                        'blocked','settled','cancelled','failed')),
                    cursor_ordinal INTEGER,
                    cursor_key TEXT,
                    cursor_phase TEXT,
                    revision_no INTEGER NOT NULL CHECK(revision_no >= 0),
                    controller_session_id TEXT NOT NULL CHECK(length(controller_session_id)=36 AND controller_session_id=lower(controller_session_id)),
                    controller_epoch INTEGER NOT NULL CHECK(controller_epoch > 0),
                    idea_row_version INTEGER NOT NULL CHECK(idea_row_version >= 0),
                    row_version INTEGER NOT NULL CHECK(row_version >= 0),
                    next_transition_sequence INTEGER NOT NULL CHECK(next_transition_sequence > 0),
                    creation_idempotency_key TEXT NOT NULL UNIQUE CHECK(length(CAST(creation_idempotency_key AS BLOB)) BETWEEN 1 AND 256),
                    creation_request_fingerprint TEXT NOT NULL CHECK(
                        length(creation_request_fingerprint)=71 AND substr(creation_request_fingerprint,1,7)='sha256:'
                        AND substr(creation_request_fingerprint,8) NOT GLOB '*[^0-9a-f]*'),
                    creation_request_json TEXT NOT NULL CHECK(json_valid(creation_request_json) AND length(CAST(creation_request_json AS BLOB)) <= 1048576),
                    created_at TEXT NOT NULL CHECK(length(created_at)=30 AND substr(created_at,30,1)='Z'),
                    updated_at TEXT NOT NULL CHECK(length(updated_at)=30 AND substr(updated_at,30,1)='Z'),
                    settled_at TEXT CHECK(settled_at IS NULL OR (length(settled_at)=30 AND substr(settled_at,30,1)='Z')),
                    cancelled_at TEXT CHECK(cancelled_at IS NULL OR (length(cancelled_at)=30 AND substr(cancelled_at,30,1)='Z')),
                    failed_at TEXT CHECK(failed_at IS NULL OR (length(failed_at)=30 AND substr(failed_at,30,1)='Z')),
                    UNIQUE(id, project_id),
                    CHECK((cursor_ordinal IS NULL AND cursor_key IS NULL AND cursor_phase IS NULL)
                       OR (cursor_ordinal >= 0 AND cursor_key IS NOT NULL AND cursor_phase IS NOT NULL
                           AND length(CAST(cursor_key AS BLOB)) BETWEEN 1 AND 64
                           AND length(CAST(cursor_phase AS BLOB)) BETWEEN 1 AND 64)),
                    CHECK((status='settled' AND settled_at IS NOT NULL AND cancelled_at IS NULL AND failed_at IS NULL)
                       OR (status='cancelled' AND cancelled_at IS NOT NULL AND settled_at IS NULL AND failed_at IS NULL)
                       OR (status='failed' AND failed_at IS NOT NULL AND settled_at IS NULL AND cancelled_at IS NULL)
                       OR (status NOT IN ('settled','cancelled','failed') AND settled_at IS NULL AND cancelled_at IS NULL AND failed_at IS NULL)),
                    FOREIGN KEY(project_id) REFERENCES projects(id) ON DELETE RESTRICT,
                    FOREIGN KEY(idea_id,project_id) REFERENCES ideas(id,project_id) ON DELETE RESTRICT,
                    FOREIGN KEY(controller_session_id) REFERENCES sessions(id) ON DELETE RESTRICT
                );",
            )?;
            #[cfg(test)]
            d05_migration_fault(D05MigrationFault::AfterRuns)?;

            tx.execute_batch(
                "CREATE TABLE idea_program_run_transitions (
                    id TEXT PRIMARY KEY CHECK(length(id)=36 AND id=lower(id)),
                    program_run_id TEXT NOT NULL CHECK(length(program_run_id)=36 AND program_run_id=lower(program_run_id)),
                    sequence INTEGER NOT NULL CHECK(sequence > 0),
                    operation TEXT NOT NULL CHECK(operation IN (
                        'create','locks_granted','action_claimed','attempt_terminal_observed',
                        'attempt_output_committed','gate_evaluated','retry_scheduled',
                        'wake_acknowledged','operator_unblocked','operator_cancelled',
                        'budget_exhausted','controller_rebound','reconciled_quarantine')),
                    from_status TEXT CHECK(from_status IS NULL OR from_status IN (
                        'pending','ready','running','awaiting_gate','retry_pending','blocked')),
                    to_status TEXT NOT NULL CHECK(to_status IN (
                        'pending','ready','running','awaiting_gate','retry_pending',
                        'blocked','settled','cancelled','failed')),
                    old_cursor_ordinal INTEGER,
                    old_cursor_key TEXT,
                    old_cursor_phase TEXT,
                    new_cursor_ordinal INTEGER,
                    new_cursor_key TEXT,
                    new_cursor_phase TEXT,
                    old_revision_no INTEGER NOT NULL CHECK(old_revision_no >= 0),
                    new_revision_no INTEGER NOT NULL CHECK(new_revision_no >= 0),
                    actor_kind TEXT NOT NULL CHECK(actor_kind IN ('operator','controller','scheduler','system')),
                    actor_session_id TEXT CHECK(actor_session_id IS NULL OR (length(actor_session_id)=36 AND actor_session_id=lower(actor_session_id))),
                    controller_epoch INTEGER NOT NULL CHECK(controller_epoch > 0),
                    expected_run_version INTEGER NOT NULL CHECK(expected_run_version >= 0),
                    resulting_run_version INTEGER NOT NULL CHECK(resulting_run_version = expected_run_version + 1),
                    expected_idea_version INTEGER NOT NULL CHECK(expected_idea_version >= 0),
                    resulting_idea_version INTEGER NOT NULL CHECK(resulting_idea_version = expected_idea_version + 1),
                    idea_event_id TEXT NOT NULL UNIQUE CHECK(length(idea_event_id)=36 AND idea_event_id=lower(idea_event_id)),
                    idempotency_key TEXT NOT NULL CHECK(length(CAST(idempotency_key AS BLOB)) BETWEEN 1 AND 256),
                    request_json TEXT NOT NULL CHECK(json_valid(request_json) AND length(CAST(request_json AS BLOB)) <= 1048576),
                    request_fingerprint TEXT NOT NULL CHECK(
                        length(request_fingerprint)=71 AND substr(request_fingerprint,1,7)='sha256:'
                        AND substr(request_fingerprint,8) NOT GLOB '*[^0-9a-f]*'),
                    created_at TEXT NOT NULL CHECK(length(created_at)=30 AND substr(created_at,30,1)='Z'),
                    UNIQUE(program_run_id,sequence),
                    UNIQUE(program_run_id,idempotency_key),
                    CHECK((operation='create' AND from_status IS NULL) OR (operation!='create' AND from_status IS NOT NULL)),
                    CHECK((old_cursor_ordinal IS NULL AND old_cursor_key IS NULL AND old_cursor_phase IS NULL)
                       OR (old_cursor_ordinal >= 0 AND old_cursor_key IS NOT NULL AND old_cursor_phase IS NOT NULL)),
                    CHECK((new_cursor_ordinal IS NULL AND new_cursor_key IS NULL AND new_cursor_phase IS NULL)
                       OR (new_cursor_ordinal >= 0 AND new_cursor_key IS NOT NULL AND new_cursor_phase IS NOT NULL)),
                    FOREIGN KEY(program_run_id) REFERENCES idea_program_runs(id) ON DELETE RESTRICT,
                    FOREIGN KEY(actor_session_id) REFERENCES sessions(id) ON DELETE RESTRICT,
                    FOREIGN KEY(idea_event_id) REFERENCES idea_events(id) ON DELETE RESTRICT
                );",
            )?;
            #[cfg(test)]
            d05_migration_fault(D05MigrationFault::AfterTransitions)?;

            tx.execute_batch(D05_V78_GATES_TABLE_DDL)?;
            #[cfg(test)]
            d05_migration_fault(D05MigrationFault::AfterGates)?;

            tx.execute_batch(
                "CREATE TABLE idea_program_run_budgets (
                    program_run_id TEXT NOT NULL,
                    dimension TEXT NOT NULL CHECK(dimension IN (
                        'productive_transitions','work_attempts','launch_retries',
                        'revisions','wake_reservations','action_publication_retries')),
                    limit_value INTEGER NOT NULL CHECK(limit_value > 0),
                    reserved_value INTEGER NOT NULL CHECK(reserved_value >= 0),
                    used_value INTEGER NOT NULL CHECK(used_value >= 0),
                    row_version INTEGER NOT NULL CHECK(row_version >= 0),
                    updated_at TEXT NOT NULL CHECK(length(updated_at)=30 AND substr(updated_at,30,1)='Z'),
                    PRIMARY KEY(program_run_id,dimension),
                    CHECK(reserved_value + used_value <= limit_value),
                    FOREIGN KEY(program_run_id) REFERENCES idea_program_runs(id) ON DELETE RESTRICT
                );",
            )?;
            #[cfg(test)]
            d05_migration_fault(D05MigrationFault::AfterBudgets)?;

            tx.execute_batch(
                "CREATE TABLE idea_program_run_locks (
                    queue_sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                    id TEXT NOT NULL UNIQUE CHECK(length(id)=36 AND id=lower(id)),
                    project_id TEXT NOT NULL CHECK(length(project_id)=36 AND project_id=lower(project_id)),
                    lock_key TEXT NOT NULL CHECK(length(CAST(lock_key AS BLOB)) BETWEEN 1 AND 128),
                    conflict_domain TEXT NOT NULL CHECK(conflict_domain IN ('idea_controller','dangerous_mutation')),
                    program_run_id TEXT NOT NULL,
                    requesting_transition_id TEXT NOT NULL,
                    state TEXT NOT NULL CHECK(state IN ('requested','held','released','expired','cancelled')),
                    controller_session_id TEXT NOT NULL,
                    controller_epoch INTEGER NOT NULL CHECK(controller_epoch > 0),
                    lease_generation INTEGER NOT NULL CHECK(lease_generation > 0),
                    owner_boot_id TEXT CHECK(owner_boot_id IS NULL OR (length(owner_boot_id)=36 AND owner_boot_id=lower(owner_boot_id))),
                    requested_at TEXT NOT NULL CHECK(length(requested_at)=30 AND substr(requested_at,30,1)='Z'),
                    acquired_at TEXT,
                    heartbeat_at TEXT,
                    expires_at TEXT,
                    released_at TEXT,
                    release_reason TEXT CHECK(release_reason IS NULL OR length(CAST(release_reason AS BLOB)) <= 2048),
                    idempotency_key TEXT NOT NULL CHECK(length(CAST(idempotency_key AS BLOB)) BETWEEN 1 AND 256),
                    request_fingerprint TEXT NOT NULL CHECK(
                        length(request_fingerprint)=71 AND substr(request_fingerprint,1,7)='sha256:'
                        AND substr(request_fingerprint,8) NOT GLOB '*[^0-9a-f]*'),
                    UNIQUE(program_run_id,conflict_domain),
                    CHECK((state='requested' AND owner_boot_id IS NULL AND acquired_at IS NULL AND heartbeat_at IS NULL AND expires_at IS NULL AND released_at IS NULL)
                       OR (state='held' AND owner_boot_id IS NOT NULL AND acquired_at IS NOT NULL AND heartbeat_at IS NOT NULL AND expires_at IS NOT NULL AND released_at IS NULL)
                       OR (state IN ('released','expired','cancelled') AND released_at IS NOT NULL)),
                    FOREIGN KEY(project_id) REFERENCES projects(id) ON DELETE RESTRICT,
                    FOREIGN KEY(program_run_id) REFERENCES idea_program_runs(id) ON DELETE RESTRICT,
                    FOREIGN KEY(requesting_transition_id) REFERENCES idea_program_run_transitions(id) ON DELETE RESTRICT,
                    FOREIGN KEY(controller_session_id) REFERENCES sessions(id) ON DELETE RESTRICT
                );",
            )?;
            #[cfg(test)]
            d05_migration_fault(D05MigrationFault::AfterLocks)?;

            tx.execute_batch(
                "CREATE TABLE idea_program_run_actions (
                    id TEXT PRIMARY KEY CHECK(length(id)=36 AND id=lower(id)),
                    program_run_id TEXT NOT NULL,
                    creating_transition_id TEXT NOT NULL UNIQUE,
                    action_kind TEXT NOT NULL CHECK(action_kind IN ('work','wake')),
                    purpose TEXT NOT NULL CHECK(purpose IN ('execute_cursor','evaluate_gates','retry_wake')),
                    payload_json TEXT NOT NULL CHECK(json_valid(payload_json) AND length(CAST(payload_json AS BLOB)) <= 1048576),
                    request_fingerprint TEXT NOT NULL CHECK(
                        length(request_fingerprint)=71 AND substr(request_fingerprint,1,7)='sha256:'
                        AND substr(request_fingerprint,8) NOT GLOB '*[^0-9a-f]*'),
                    downstream_dedup_key TEXT NOT NULL UNIQUE CHECK(length(CAST(downstream_dedup_key AS BLOB)) BETWEEN 1 AND 256),
                    controller_session_id TEXT NOT NULL,
                    controller_epoch INTEGER NOT NULL CHECK(controller_epoch > 0),
                    not_before TEXT NOT NULL CHECK(length(not_before)=30 AND substr(not_before,30,1)='Z'),
                    state TEXT NOT NULL CHECK(state IN ('reserved','claimed','published','acknowledged','failed','cancelled')),
                    claim_boot_id TEXT CHECK(claim_boot_id IS NULL OR (length(claim_boot_id)=36 AND claim_boot_id=lower(claim_boot_id))),
                    claim_generation INTEGER NOT NULL CHECK(claim_generation > 0),
                    claimed_at TEXT,
                    claim_expires_at TEXT,
                    publication_attempts INTEGER NOT NULL CHECK(publication_attempts >= 0),
                    max_publication_attempts INTEGER NOT NULL CHECK(max_publication_attempts > 0),
                    external_model_invocation_id TEXT CHECK(external_model_invocation_id IS NULL OR length(external_model_invocation_id)=36),
                    external_session_id TEXT CHECK(external_session_id IS NULL OR length(external_session_id)=36),
                    scheduled_job_id TEXT CHECK(scheduled_job_id IS NULL OR length(scheduled_job_id)=36),
                    last_error_class TEXT CHECK(last_error_class IS NULL OR length(CAST(last_error_class AS BLOB)) <= 64),
                    last_error_message TEXT CHECK(last_error_message IS NULL OR length(CAST(last_error_message AS BLOB)) <= 512),
                    created_at TEXT NOT NULL CHECK(length(created_at)=30 AND substr(created_at,30,1)='Z'),
                    updated_at TEXT NOT NULL CHECK(length(updated_at)=30 AND substr(updated_at,30,1)='Z'),
                    published_at TEXT,
                    acknowledged_at TEXT,
                    CHECK((state='reserved' AND claim_boot_id IS NULL AND claimed_at IS NULL AND claim_expires_at IS NULL AND published_at IS NULL AND acknowledged_at IS NULL)
                       OR (state='claimed' AND claim_boot_id IS NOT NULL AND claimed_at IS NOT NULL AND claim_expires_at IS NOT NULL AND published_at IS NULL AND acknowledged_at IS NULL)
                       OR (state='published' AND claim_boot_id IS NOT NULL AND claimed_at IS NOT NULL AND published_at IS NOT NULL AND acknowledged_at IS NULL)
                       OR (state='acknowledged' AND published_at IS NOT NULL AND acknowledged_at IS NOT NULL)
                       OR state IN ('failed','cancelled')),
                    FOREIGN KEY(program_run_id) REFERENCES idea_program_runs(id) ON DELETE RESTRICT,
                    FOREIGN KEY(creating_transition_id) REFERENCES idea_program_run_transitions(id) ON DELETE RESTRICT,
                    FOREIGN KEY(controller_session_id) REFERENCES sessions(id) ON DELETE RESTRICT,
                    FOREIGN KEY(external_session_id) REFERENCES sessions(id) ON DELETE RESTRICT,
                    FOREIGN KEY(scheduled_job_id) REFERENCES scheduled_jobs(id) ON DELETE RESTRICT
                );",
            )?;
            #[cfg(test)]
            d05_migration_fault(D05MigrationFault::AfterActions)?;

            tx.execute_batch(D05_V78_ATTEMPT_REFS_TABLE_DDL)?;
            #[cfg(test)]
            d05_migration_fault(D05MigrationFault::AfterAttemptRefs)?;

            tx.execute_batch(
                "CREATE INDEX idx_idea_program_runs_project_status_updated ON idea_program_runs(project_id,status,updated_at,id);
                 CREATE INDEX idx_idea_program_runs_idea_created ON idea_program_runs(idea_id,created_at,id);
                 CREATE INDEX idx_idea_program_run_transitions_run_sequence ON idea_program_run_transitions(program_run_id,sequence DESC);
                 CREATE INDEX idx_idea_program_run_transitions_idea_event ON idea_program_run_transitions(idea_event_id);
                 CREATE INDEX idx_idea_program_run_transitions_created ON idea_program_run_transitions(created_at,id);
                 CREATE INDEX idx_idea_program_run_gates_latest ON idea_program_run_gates(program_run_id,cursor_ordinal,revision_no,gate_key,evaluation_no DESC);
                 CREATE INDEX idx_idea_program_run_locks_requested ON idea_program_run_locks(project_id,lock_key,queue_sequence) WHERE state='requested';
                 CREATE INDEX idx_idea_program_run_locks_run_state ON idea_program_run_locks(program_run_id,state);
                 CREATE INDEX idx_idea_program_run_actions_due ON idea_program_run_actions(state,not_before,id);
                 CREATE INDEX idx_idea_program_run_actions_expired_claim ON idea_program_run_actions(state,claim_expires_at,id);
                 CREATE INDEX idx_idea_program_run_attempts_run_cursor ON idea_program_run_attempt_refs(program_run_id,cursor_ordinal,revision_no,attempt_no DESC);",
            )?;
            #[cfg(test)]
            d05_migration_fault(D05MigrationFault::AfterLookupIndexes)?;

            tx.execute_batch(
                "CREATE UNIQUE INDEX uidx_idea_program_runs_one_nonterminal_idea ON idea_program_runs(idea_id)
                    WHERE status NOT IN ('settled','cancelled','failed');
                 CREATE UNIQUE INDEX uidx_idea_program_run_locks_held ON idea_program_run_locks(project_id,lock_key) WHERE state='held';
                 CREATE UNIQUE INDEX uidx_idea_program_run_actions_one_active ON idea_program_run_actions(program_run_id)
                    WHERE state IN ('reserved','claimed','published');
                 CREATE UNIQUE INDEX uidx_idea_program_run_attempts_session ON idea_program_run_attempt_refs(session_id) WHERE session_id IS NOT NULL;
                 CREATE UNIQUE INDEX uidx_idea_program_run_attempts_model_invocation ON idea_program_run_attempt_refs(model_invocation_id) WHERE model_invocation_id IS NOT NULL;",
            )?;
            #[cfg(test)]
            d05_migration_fault(D05MigrationFault::AfterPartialUniqueIndexes)?;

            tx.execute_batch(
                "CREATE TRIGGER idea_program_runs_no_delete BEFORE DELETE ON idea_program_runs BEGIN SELECT RAISE(ABORT,'ProgramRun rows cannot be deleted'); END;
                 CREATE TRIGGER idea_program_run_transitions_no_delete BEFORE DELETE ON idea_program_run_transitions BEGIN SELECT RAISE(ABORT,'ProgramRun transitions cannot be deleted'); END;
                 CREATE TRIGGER idea_program_run_gates_no_delete BEFORE DELETE ON idea_program_run_gates BEGIN SELECT RAISE(ABORT,'ProgramRun gates cannot be deleted'); END;
                 CREATE TRIGGER idea_program_run_budgets_no_delete BEFORE DELETE ON idea_program_run_budgets BEGIN SELECT RAISE(ABORT,'ProgramRun budgets cannot be deleted'); END;
                 CREATE TRIGGER idea_program_run_locks_no_delete BEFORE DELETE ON idea_program_run_locks BEGIN SELECT RAISE(ABORT,'ProgramRun locks cannot be deleted'); END;
                 CREATE TRIGGER idea_program_run_actions_no_delete BEFORE DELETE ON idea_program_run_actions BEGIN SELECT RAISE(ABORT,'ProgramRun actions cannot be deleted'); END;
                 CREATE TRIGGER idea_program_run_attempt_refs_no_delete BEFORE DELETE ON idea_program_run_attempt_refs BEGIN SELECT RAISE(ABORT,'ProgramRun attempts cannot be deleted'); END;",
            )?;
            #[cfg(test)]
            d05_migration_fault(D05MigrationFault::AfterNoDeleteTriggers)?;

            tx.execute_batch(
                "CREATE TRIGGER idea_program_run_transitions_immutable BEFORE UPDATE ON idea_program_run_transitions BEGIN SELECT RAISE(ABORT,'ProgramRun transitions are immutable'); END;
                 CREATE TRIGGER idea_program_run_gates_immutable BEFORE UPDATE ON idea_program_run_gates BEGIN SELECT RAISE(ABORT,'ProgramRun gates are immutable'); END;",
            )?;
            #[cfg(test)]
            d05_migration_fault(D05MigrationFault::AfterImmutableTriggers)?;

            let foreign_key_errors: i64 =
                tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                    row.get(0)
                })?;
            if foreign_key_errors != 0 {
                return Err(crate::error::DaemonError::Store(
                    "V78 foreign-key check failed".to_string(),
                ));
            }
            program_runs::validate_d05_catalog(&tx)?;
            let schema_fingerprint = program_runs::d05_v78_schema_fingerprint(&tx)?;
            if schema_fingerprint.len() != 71 || !schema_fingerprint.starts_with("sha256:") {
                return Err(crate::error::DaemonError::Store(
                    "V78 semantic schema fingerprint failed".to_string(),
                ));
            }
            #[cfg(test)]
            d05_migration_fault(D05MigrationFault::AfterForeignKeyCheck)?;
            tx.execute("PRAGMA user_version = 78", [])?;
            #[cfg(test)]
            d05_migration_fault(D05MigrationFault::AfterUserVersion)?;
            #[cfg(test)]
            d05_migration_fault(D05MigrationFault::BeforeCommit)?;
            tx.commit()?;
            tracing::info!(%schema_fingerprint, "V78 migration complete: ProgramRun kernel");
        }

        // The V78 DDL was amended after V78 had already shipped: three
        // transition foreign keys on `idea_program_run_gates` and
        // `idea_program_run_attempt_refs` gained `DEFERRABLE INITIALLY
        // DEFERRED`. A database that reached V78 before that amendment carries
        // a baseline no later gate can accept — V79 pins the post-amendment
        // fingerprint, so such a database fails the V79 gate on every boot,
        // rolls back, and takes the daemon down with it.
        //
        // Normalize those databases forward to the canonical V78 baseline here,
        // before V79 runs, so V79 still sees the exact source it was reviewed
        // against and stays byte-for-byte the reviewed migration. This is a
        // convergent repair, not a second target shape: it asserts the canonical
        // V78 fingerprint as its post-condition, so every database that survives
        // it holds precisely the schema a fresh V78 produces. Databases already
        // on the canonical baseline are left untouched, and databases already at
        // V79 never enter this branch.
        if version == 78 {
            self.repair_legacy_d05_v78_baseline()?;
        }

        // V79 is a forward-only custody repair for D05. It deliberately does
        // not edit the reviewed V78 DDL. Instead it validates every existing
        // value, adds composed claim witnesses, and installs strict guards for
        // all future ProgramRun identity/time writes.
        if version < 79 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            if active_version != 78 {
                return Err(DaemonError::Store(format!(
                    "V79 requires exact V78 source, found V{active_version}"
                )));
            }
            #[cfg(test)]
            d05_v79_migration_fault(D05V79MigrationFault::AfterExactSource)?;

            program_runs::validate_d05_v79_table_rows(&tx, "idea_program_runs")?;
            #[cfg(test)]
            d05_v79_migration_fault(D05V79MigrationFault::AfterRunRowsValidated)?;
            program_runs::validate_d05_v79_table_rows(&tx, "idea_program_run_transitions")?;
            #[cfg(test)]
            d05_v79_migration_fault(D05V79MigrationFault::AfterTransitionRowsValidated)?;
            program_runs::validate_d05_v79_table_rows(&tx, "idea_program_run_gates")?;
            #[cfg(test)]
            d05_v79_migration_fault(D05V79MigrationFault::AfterGateRowsValidated)?;
            program_runs::validate_d05_v79_table_rows(&tx, "idea_program_run_budgets")?;
            #[cfg(test)]
            d05_v79_migration_fault(D05V79MigrationFault::AfterBudgetRowsValidated)?;
            program_runs::validate_d05_v79_table_rows(&tx, "idea_program_run_locks")?;
            #[cfg(test)]
            d05_v79_migration_fault(D05V79MigrationFault::AfterLockRowsValidated)?;
            program_runs::validate_d05_v79_table_rows(&tx, "idea_program_run_actions")?;
            #[cfg(test)]
            d05_v79_migration_fault(D05V79MigrationFault::AfterActionRowsValidated)?;
            program_runs::validate_d05_v79_table_rows(&tx, "idea_program_run_attempt_refs")?;
            #[cfg(test)]
            d05_v79_migration_fault(D05V79MigrationFault::AfterAttemptRowsValidated)?;

            tx.execute(
                "ALTER TABLE idea_program_run_actions ADD COLUMN claim_run_version INTEGER
                 CHECK(claim_run_version IS NULL OR claim_run_version >= 0)",
                [],
            )?;
            #[cfg(test)]
            d05_v79_migration_fault(D05V79MigrationFault::AfterClaimRunColumn)?;
            tx.execute(
                "ALTER TABLE idea_program_run_actions ADD COLUMN claim_lease_generation INTEGER
                 CHECK(claim_lease_generation IS NULL OR claim_lease_generation >= 0)",
                [],
            )?;
            #[cfg(test)]
            d05_v79_migration_fault(D05V79MigrationFault::AfterClaimLeaseColumn)?;

            install_d05_v79_validation_triggers(&tx)?;
            #[cfg(test)]
            d05_v79_migration_fault(D05V79MigrationFault::AfterValidationTriggers)?;

            let foreign_key_errors: i64 =
                tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                    row.get(0)
                })?;
            if foreign_key_errors != 0 {
                return Err(DaemonError::Store(
                    "V79 ProgramRun foreign-key check failed".into(),
                ));
            }
            let schema_fingerprint = program_runs::d05_schema_fingerprint(&tx)?;
            if schema_fingerprint != program_runs::D05_V79_SCHEMA_FINGERPRINT {
                return Err(DaemonError::Store(format!(
                    "V79 ProgramRun semantic fingerprint mismatch: {schema_fingerprint}"
                )));
            }
            #[cfg(test)]
            d05_v79_migration_fault(D05V79MigrationFault::AfterFingerprint)?;
            #[cfg(test)]
            d05_v79_migration_fault(D05V79MigrationFault::AfterForeignKeyCheck)?;
            tx.execute("PRAGMA user_version = 79", [])?;
            #[cfg(test)]
            d05_v79_migration_fault(D05V79MigrationFault::AfterUserVersion)?;
            #[cfg(test)]
            d05_v79_migration_fault(D05V79MigrationFault::BeforeCommit)?;
            tx.commit()?;
            tracing::info!(%schema_fingerprint, "V79 migration complete: ProgramRun custody repair");
        }

        // V80 adds the durable coordination aggregate used by agent spawn,
        // progress, automatic terminal-watch repair, and Phase 2 messaging.
        // It is forward-only and accepts exactly the audited V79 catalog.
        if version < 80 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            if active_version != 79 {
                return Err(DaemonError::Store(format!(
                    "V80 requires exact V79 source, found V{active_version}"
                )));
            }
            let v79_fingerprint = program_runs::d05_schema_fingerprint(&tx)?;
            if v79_fingerprint != program_runs::D05_V79_SCHEMA_FINGERPRINT {
                return Err(DaemonError::Store(format!(
                    "V80 requires exact V79 ProgramRun catalog, found {v79_fingerprint}"
                )));
            }
            #[cfg(test)]
            agent_coordination_v80_migration_fault(
                AgentCoordinationV80MigrationFault::AfterExactSource,
            )?;

            agent_coordination::install_v80_schema(&tx)?;
            #[cfg(test)]
            agent_coordination_v80_migration_fault(
                AgentCoordinationV80MigrationFault::AfterSchema,
            )?;

            let schema_fingerprint = agent_coordination::v80_schema_fingerprint(&tx)?;
            if schema_fingerprint != agent_coordination::AGENT_COORDINATION_V80_SCHEMA_FINGERPRINT {
                return Err(DaemonError::Store(format!(
                    "V80 agent coordination semantic fingerprint mismatch: {schema_fingerprint}"
                )));
            }
            #[cfg(test)]
            agent_coordination_v80_migration_fault(
                AgentCoordinationV80MigrationFault::AfterFingerprint,
            )?;

            let foreign_key_errors: i64 =
                tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                    row.get(0)
                })?;
            if foreign_key_errors != 0 {
                return Err(DaemonError::Store(
                    "V80 agent coordination foreign-key check failed".into(),
                ));
            }
            #[cfg(test)]
            agent_coordination_v80_migration_fault(
                AgentCoordinationV80MigrationFault::AfterForeignKeyCheck,
            )?;

            tx.execute("PRAGMA user_version = 80", [])?;
            #[cfg(test)]
            agent_coordination_v80_migration_fault(
                AgentCoordinationV80MigrationFault::AfterUserVersion,
            )?;
            #[cfg(test)]
            agent_coordination_v80_migration_fault(
                AgentCoordinationV80MigrationFault::BeforeCommit,
            )?;
            tx.commit()?;
            tracing::info!(%schema_fingerprint, "V80 migration complete: agent coordination");
        }

        if version < 81 {
            // Phase 2 exclusively owns V81 after an exact accepted-V80
            // semantic-fingerprint preflight (C-P2-02). The numbering is legal
            // only because the rejected V81 source was never integrated,
            // deployed, or opened against a real database.
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            if active_version != 80 {
                return Err(DaemonError::Store(format!(
                    "V81 requires exact V80 source, found V{active_version}"
                )));
            }
            #[cfg(test)]
            agent_message_v81_migration_fault(AgentMessageV81MigrationFault::AfterExactSource)?;

            // The accepted V80 coordination catalog must be byte-exact before
            // the first DDL statement; a mismatch aborts without changing data
            // or `user_version`.
            let v80_fingerprint = agent_coordination::v80_schema_fingerprint(&tx)?;
            if v80_fingerprint != agent_coordination::AGENT_COORDINATION_V80_SCHEMA_FINGERPRINT {
                return Err(DaemonError::Store(format!(
                    "V81 requires the exact accepted V80 coordination catalog, found {v80_fingerprint}"
                )));
            }
            #[cfg(test)]
            agent_message_v81_migration_fault(AgentMessageV81MigrationFault::AfterV80Fingerprint)?;

            agent_coordination::install_v81_schema(&tx)?;
            #[cfg(test)]
            agent_message_v81_migration_fault(AgentMessageV81MigrationFault::AfterSchema)?;

            // `install_v81_schema` performs the copy and its row-count parity
            // check; this failpoint sits on the far side of both.
            #[cfg(test)]
            agent_message_v81_migration_fault(AgentMessageV81MigrationFault::AfterCopy)?;

            // Deferred foreign keys must be fully drained inside the
            // transaction, not merely counted.
            let foreign_key_errors: i64 =
                tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                    row.get(0)
                })?;
            if foreign_key_errors != 0 {
                return Err(DaemonError::Store(
                    "V81 agent message foreign-key check failed".into(),
                ));
            }
            #[cfg(test)]
            agent_message_v81_migration_fault(AgentMessageV81MigrationFault::AfterForeignKeyCheck)?;

            tx.execute(
                &format!(
                    "PRAGMA user_version = {}",
                    agent_coordination::AGENT_MESSAGE_V81_USER_VERSION
                ),
                [],
            )?;
            #[cfg(test)]
            agent_message_v81_migration_fault(AgentMessageV81MigrationFault::AfterUserVersion)?;

            // Fingerprinted AFTER the version write, deliberately (C-P2-17,
            // H21-P2-INT-REV-005). `v81_schema_fingerprint` absorbs the LIVE
            // `PRAGMA user_version`, so computing it here — rather than before
            // the write — is what makes the migrating connection and every
            // later reopen agree on one digest for one identical catalog,
            // while still giving the digest real catalog/version coverage.
            // A mismatch still aborts the whole transaction, so nothing is
            // durable: the version write rolls back with the DDL.
            let schema_fingerprint = agent_coordination::v81_schema_fingerprint(&tx)?;
            if schema_fingerprint
                == agent_coordination::AGENT_MESSAGE_REJECTED_V81_SCHEMA_FINGERPRINT
            {
                return Err(DaemonError::Store(
                    "V81 catalog matches the REJECTED V81 fingerprint and is forbidden".into(),
                ));
            }
            if schema_fingerprint != agent_coordination::AGENT_MESSAGE_V81_SCHEMA_FINGERPRINT {
                return Err(DaemonError::Store(format!(
                    "V81 agent message semantic fingerprint mismatch: {schema_fingerprint}"
                )));
            }
            #[cfg(test)]
            agent_message_v81_migration_fault(AgentMessageV81MigrationFault::AfterFingerprint)?;
            #[cfg(test)]
            agent_message_v81_migration_fault(AgentMessageV81MigrationFault::BeforeCommit)?;
            tx.commit()?;
            tracing::info!(%schema_fingerprint, "V81 migration complete: agent message mailbox");
        }

        if version < 82 {
            // V82 REBUILDS `agent_message_delivery_attempts`, which four other
            // relations hold `ON DELETE RESTRICT` foreign keys into, and a
            // RESTRICT action fires immediately even when its constraint is
            // `DEFERRABLE INITIALLY DEFERRED`. `DROP TABLE` under
            // `foreign_keys=ON` performs an implicit `DELETE FROM`, so the
            // rebuild would abort against any live attempt row. `PRAGMA
            // foreign_keys` is a no-op INSIDE a transaction, so the toggle has
            // to bracket the migration from outside it — the V65 precedent.
            //
            // The pragma is restored on BOTH paths before the error propagates,
            // so a failed migration can never leave this connection with
            // enforcement silently disabled.
            self.conn.execute_batch("PRAGMA foreign_keys=OFF;")?;
            let outcome = self.apply_agent_message_v82_migration();
            self.conn.execute_batch("PRAGMA foreign_keys=ON;")?;
            outcome?;
        }

        if version < 83 {
            // V83 is deliberately additive. The V80--V82 catalog is accepted
            // history and remains untouched; every legacy session insert is
            // covered by the projection trigger below.
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            if active_version != 82 {
                return Err(DaemonError::Store(format!(
                    "V83 requires exact V82 source, found V{active_version}"
                )));
            }
            let v82_fingerprint = agent_coordination::v81_schema_fingerprint(&tx)?;
            if v82_fingerprint != agent_coordination::AGENT_MESSAGE_V82_PINNED_FINGERPRINT {
                return Err(DaemonError::Store(format!(
                    "V83 requires the exact accepted V82 catalog, found {v82_fingerprint}"
                )));
            }
            tx.execute_batch(
                "
                CREATE TABLE sandbox_custody_roots (
                    custody_id TEXT PRIMARY KEY,
                    canonical_repo_dir TEXT NOT NULL,
                    sandbox_root TEXT NOT NULL UNIQUE,
                    sandbox_branch TEXT NOT NULL,
                    repository_identity TEXT NOT NULL,
                    source_commit TEXT NOT NULL,
                    state TEXT NOT NULL CHECK (state IN ('live','purged','failed','quarantined')),
                    owner_session_id TEXT UNIQUE REFERENCES sessions(id) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
                    generation INTEGER NOT NULL CHECK (generation > 0),
                    event_sequence INTEGER NOT NULL CHECK (event_sequence > 0),
                    validation_state TEXT NOT NULL CHECK (validation_state IN ('verified','unverified','invalid')),
                    validated_generation INTEGER,
                    validated_at TEXT,
                    validation_error_code TEXT,
                    effect_boot_id TEXT,
                    reserved_effects INTEGER NOT NULL DEFAULT 0 CHECK (reserved_effects >= 0),
                    active_effects INTEGER NOT NULL DEFAULT 0 CHECK (active_effects >= 0),
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    tombstoned_at TEXT,
                    UNIQUE (custody_id, generation),
                    CHECK ((state='live' AND owner_session_id IS NOT NULL AND tombstoned_at IS NULL)
                        OR (state IN ('purged','failed','quarantined') AND owner_session_id IS NULL)),
                    CHECK ((validation_state='verified' AND validated_generation=generation AND validated_at IS NOT NULL AND validation_error_code IS NULL)
                        OR (validation_state='unverified' AND validated_generation IS NULL AND validated_at IS NULL AND validation_error_code IS NULL)
                        OR (validation_state='invalid' AND validated_generation=generation AND validated_at IS NOT NULL AND validation_error_code IS NOT NULL)),
                    CHECK ((reserved_effects=0 AND active_effects=0 AND effect_boot_id IS NULL)
                        OR ((reserved_effects>0 OR active_effects>0) AND effect_boot_id IS NOT NULL)),
                    CHECK (state='live' OR (reserved_effects=0 AND active_effects=0))
                );
                ALTER TABLE sessions ADD COLUMN sandbox_custody_id TEXT REFERENCES sandbox_custody_roots(custody_id) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;
                CREATE TABLE sandbox_custody_events (
                    event_id TEXT PRIMARY KEY,
                    custody_id TEXT NOT NULL REFERENCES sandbox_custody_roots(custody_id) ON DELETE RESTRICT,
                    sequence INTEGER NOT NULL CHECK (sequence > 0),
                    event_kind TEXT NOT NULL CHECK (event_kind IN ('allocated','transferred','validation_failed','tombstoned','failed','quarantined')),
                    cause TEXT NOT NULL CHECK (cause IN ('fresh_launch','agent_spawn_child','rotation','retry','agent_fresh','codex_app_server_replacement','recursive_live','startup_reconciliation','purge','cleanup_failure','effect_revalidation')),
                    from_generation INTEGER,
                    to_generation INTEGER NOT NULL CHECK (to_generation > 0),
                    from_owner_session_id TEXT REFERENCES sessions(id) ON DELETE RESTRICT,
                    to_owner_session_id TEXT REFERENCES sessions(id) ON DELETE RESTRICT,
                    origin_session_id TEXT REFERENCES sessions(id) ON DELETE RESTRICT,
                    scheduled_job_id TEXT REFERENCES scheduled_jobs(id) ON DELETE RESTRICT,
                    prior_state TEXT,
                    next_state TEXT NOT NULL CHECK (next_state IN ('live','purged','failed','quarantined')),
                    error_code TEXT,
                    occurred_at TEXT NOT NULL,
                    UNIQUE (custody_id, sequence),
                    CHECK (prior_state IS NULL OR prior_state IN ('live','purged','failed','quarantined')),
                    CHECK ((event_kind='allocated' AND from_generation IS NULL AND to_generation=1)
                        OR (event_kind='validation_failed' AND from_generation=to_generation)
                        OR (event_kind NOT IN ('allocated','validation_failed') AND from_generation IS NOT NULL AND to_generation=from_generation+1)),
                    CHECK ((event_kind='allocated' AND from_owner_session_id IS NULL AND to_owner_session_id IS NOT NULL AND next_state='live')
                        OR (event_kind='transferred' AND from_owner_session_id IS NOT NULL AND to_owner_session_id IS NOT NULL AND from_owner_session_id!=to_owner_session_id AND next_state='live')
                        OR (event_kind='validation_failed' AND from_owner_session_id IS NOT NULL AND to_owner_session_id=from_owner_session_id AND from_generation=to_generation AND error_code IS NOT NULL)
                        OR (event_kind IN ('tombstoned','failed','quarantined') AND to_owner_session_id IS NULL AND next_state!='live')),
                    CHECK (cause!='agent_fresh' OR (origin_session_id IS NOT NULL AND scheduled_job_id IS NOT NULL))
                );
                CREATE TABLE session_execution_projections (
                    session_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE RESTRICT,
                    schema_version INTEGER NOT NULL CHECK (schema_version=1),
                    projection_version INTEGER NOT NULL CHECK (projection_version=1),
                    execution_state TEXT NOT NULL CHECK (execution_state IN ('ordinary_unsandboxed','live_sandboxed','historical_purged','historical_transferred','historical_cleanup_failed','quarantined','invalid')),
                    freshness TEXT NOT NULL CHECK (freshness IN ('verified','unverified','invalid')),
                    canonical_repo_dir TEXT NOT NULL,
                    effective_cwd TEXT,
                    custody_id TEXT REFERENCES sandbox_custody_roots(custody_id) ON DELETE RESTRICT,
                    custody_generation INTEGER,
                    validated_at TEXT,
                    error_code TEXT,
                    updated_at TEXT NOT NULL,
                    CHECK ((effective_cwd IS NOT NULL AND freshness='verified' AND execution_state IN ('ordinary_unsandboxed','live_sandboxed'))
                        OR (effective_cwd IS NULL AND NOT (freshness='verified' AND execution_state IN ('ordinary_unsandboxed','live_sandboxed')))),
                    CHECK ((freshness='verified' AND validated_at IS NOT NULL AND error_code IS NULL)
                        OR (freshness='unverified' AND validated_at IS NULL AND error_code IS NULL AND effective_cwd IS NULL)
                        OR (freshness='invalid' AND validated_at IS NOT NULL AND error_code IS NOT NULL AND effective_cwd IS NULL))
                );
                CREATE INDEX idx_sandbox_custody_roots_owner ON sandbox_custody_roots(owner_session_id);
                CREATE INDEX idx_sandbox_custody_roots_state ON sandbox_custody_roots(state, validation_state);
                CREATE INDEX idx_sandbox_custody_events_order ON sandbox_custody_events(custody_id, sequence);
                CREATE INDEX idx_sandbox_custody_events_to_owner ON sandbox_custody_events(custody_id, to_owner_session_id);
                CREATE INDEX idx_sessions_sandbox_custody_id_id ON sessions(sandbox_custody_id, id);
                CREATE INDEX idx_sessions_sandbox_root_id ON sessions(sandbox_root, id);
                CREATE INDEX idx_sessions_startup_unlinked_root
                    ON sessions(sandbox_root)
                    WHERE sandbox_root IS NOT NULL AND sandbox_custody_id IS NULL;
                CREATE INDEX idx_sessions_startup_unlinked_rootless
                    ON sessions(id)
                    WHERE sandbox_root IS NULL AND sandbox_custody_id IS NULL;
                CREATE INDEX idx_session_execution_projections_custody ON session_execution_projections(custody_id, session_id);

                CREATE TRIGGER sandbox_custody_roots_no_delete BEFORE DELETE ON sandbox_custody_roots BEGIN SELECT RAISE(ABORT, 'sandbox custody roots are immutable history'); END;
                CREATE TRIGGER sandbox_custody_events_no_delete BEFORE DELETE ON sandbox_custody_events BEGIN SELECT RAISE(ABORT, 'sandbox custody events are immutable history'); END;
                CREATE TRIGGER session_execution_projections_no_delete BEFORE DELETE ON session_execution_projections BEGIN SELECT RAISE(ABORT, 'sandbox custody projections are retained history'); END;
                CREATE TRIGGER sandbox_custody_events_immutable BEFORE UPDATE ON sandbox_custody_events BEGIN SELECT RAISE(ABORT, 'sandbox custody events are immutable'); END;
                CREATE TRIGGER sandbox_custody_roots_identity_immutable BEFORE UPDATE ON sandbox_custody_roots
                WHEN NEW.custody_id != OLD.custody_id OR NEW.canonical_repo_dir != OLD.canonical_repo_dir OR NEW.sandbox_root != OLD.sandbox_root OR NEW.sandbox_branch != OLD.sandbox_branch OR NEW.repository_identity != OLD.repository_identity OR NEW.source_commit != OLD.source_commit OR NEW.created_at != OLD.created_at
                BEGIN SELECT RAISE(ABORT, 'sandbox custody root identity is immutable'); END;
                CREATE TRIGGER sandbox_custody_roots_monotonic BEFORE UPDATE ON sandbox_custody_roots
                WHEN NEW.generation < OLD.generation OR NEW.event_sequence < OLD.event_sequence
                BEGIN SELECT RAISE(ABORT, 'sandbox custody generation and event sequence are forward only'); END;
                CREATE TRIGGER sandbox_custody_roots_transition_event BEFORE UPDATE ON sandbox_custody_roots
                WHEN (NEW.owner_session_id IS NOT OLD.owner_session_id OR NEW.state != OLD.state) AND (NEW.generation != OLD.generation + 1 OR NEW.event_sequence != OLD.event_sequence + 1 OR NOT EXISTS (SELECT 1 FROM sandbox_custody_events e WHERE e.custody_id=OLD.custody_id AND e.sequence=NEW.event_sequence AND e.to_generation=NEW.generation))
                BEGIN SELECT RAISE(ABORT, 'sandbox custody transition requires next immutable event'); END;
                CREATE TRIGGER sessions_execution_projection_after_insert AFTER INSERT ON sessions BEGIN
                    INSERT INTO session_execution_projections (session_id,schema_version,projection_version,execution_state,freshness,canonical_repo_dir,effective_cwd,custody_id,custody_generation,validated_at,error_code,updated_at)
                    VALUES (NEW.id,1,1,CASE WHEN NEW.sandbox_kind IS NULL AND NEW.sandbox_root IS NULL AND NEW.sandbox_branch IS NULL AND NEW.sandbox_cleanup_state IS NULL THEN 'ordinary_unsandboxed' WHEN NEW.sandbox_cleanup_state='Purged' THEN 'historical_purged' WHEN NEW.sandbox_cleanup_state='Failed' THEN 'historical_cleanup_failed' WHEN NEW.sandbox_kind IS NOT NULL AND NEW.sandbox_root IS NOT NULL AND NEW.sandbox_branch IS NOT NULL AND NEW.sandbox_cleanup_state='Live' THEN 'live_sandboxed' ELSE 'invalid' END,'unverified',NEW.working_dir,NULL,NULL,NULL,NULL,NULL,NEW.updated_at);
                END;
                INSERT INTO session_execution_projections (session_id,schema_version,projection_version,execution_state,freshness,canonical_repo_dir,effective_cwd,custody_id,custody_generation,validated_at,error_code,updated_at)
                SELECT id,1,1,CASE WHEN sandbox_kind IS NULL AND sandbox_root IS NULL AND sandbox_branch IS NULL AND sandbox_cleanup_state IS NULL THEN 'ordinary_unsandboxed' WHEN sandbox_cleanup_state='Purged' THEN 'historical_purged' WHEN sandbox_cleanup_state='Failed' THEN 'historical_cleanup_failed' WHEN sandbox_kind IS NOT NULL AND sandbox_root IS NOT NULL AND sandbox_branch IS NOT NULL AND sandbox_cleanup_state='Live' THEN 'live_sandboxed' ELSE 'invalid' END,'unverified',working_dir,NULL,NULL,NULL,NULL,NULL,updated_at FROM sessions;
                ",
            )?;
            tx.execute("PRAGMA user_version = 83", [])?;
            tx.commit()?;
            tracing::info!(
                "V83 migration complete: sandbox custody roots and execution projections"
            );
        }

        if version < 84 {
            // K1 owns one complete additive Closure catalog. K2/K3 consume
            // these predeclared journals but their effect routes remain
            // intentionally absent until their own slices pass review.
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            if active_version != 83 {
                return Err(DaemonError::Store(format!(
                    "V84 requires exact V83 source, found V{active_version}"
                )));
            }
            tx.execute_batch(
                "
                CREATE TABLE closure_programs (
                    id TEXT PRIMARY KEY CHECK(length(id)=36 AND id=lower(id) AND substr(id,9,1)='-' AND substr(id,14,1)='-' AND substr(id,19,1)='-' AND substr(id,24,1)='-' AND length(replace(id,'-',''))=32 AND replace(id,'-','') NOT GLOB '*[^0-9a-f]*'),
                    version INTEGER NOT NULL CHECK(version>0),
                    repository_root TEXT NOT NULL,
                    repository_identity TEXT NOT NULL,
                    base_ref TEXT NOT NULL CHECK(substr(base_ref,1,11)='refs/heads/' AND length(base_ref)>11),
                    base_sha TEXT NOT NULL CHECK(length(base_sha) IN (40,64) AND base_sha=lower(base_sha) AND base_sha NOT GLOB '*[^0-9a-f]*'),
                    destination_ref TEXT NOT NULL CHECK(substr(destination_ref,1,11)='refs/heads/' AND length(destination_ref)>11),
                    destination_pre_head TEXT NOT NULL CHECK(length(destination_pre_head) IN (40,64) AND destination_pre_head=lower(destination_pre_head) AND destination_pre_head NOT GLOB '*[^0-9a-f]*'),
                    state TEXT NOT NULL CHECK(state IN ('draft','configured','working','outcome_blocked','awaiting_evidence','eligible','awaiting_integration','integrating','conflict','gate_failed','integrated','quarantined_cleanup_pending','cleanup_pending','cleanup_in_progress','cleanup_failed','closed','quarantined_closed')),
                    destination_claim_state TEXT NOT NULL CHECK(destination_claim_state IN ('held','released')),
                    review_policy_json TEXT NOT NULL,
                    review_policy_digest TEXT NOT NULL CHECK(length(review_policy_digest)=71 AND substr(review_policy_digest,1,7)='sha256:' AND substr(review_policy_digest,8)=lower(substr(review_policy_digest,8)) AND substr(review_policy_digest,8) NOT GLOB '*[^0-9a-f]*'),
                    verification_policy_json TEXT NOT NULL,
                    verification_policy_digest TEXT NOT NULL CHECK(length(verification_policy_digest)=71 AND substr(verification_policy_digest,1,7)='sha256:' AND substr(verification_policy_digest,8)=lower(substr(verification_policy_digest,8)) AND substr(verification_policy_digest,8) NOT GLOB '*[^0-9a-f]*'),
                    verifier_policy_json TEXT NOT NULL,
                    verifier_policy_digest TEXT NOT NULL CHECK(length(verifier_policy_digest)=71 AND substr(verifier_policy_digest,1,7)='sha256:' AND substr(verifier_policy_digest,8)=lower(substr(verifier_policy_digest,8)) AND substr(verifier_policy_digest,8) NOT GLOB '*[^0-9a-f]*'),
                    checkout_remediation_policy TEXT NOT NULL CHECK(checkout_remediation_policy IN ('refuse','managed_detach_reattach')),
                    creation_idempotency_key TEXT NOT NULL UNIQUE CHECK(length(creation_idempotency_key)=36 AND creation_idempotency_key=lower(creation_idempotency_key) AND substr(creation_idempotency_key,9,1)='-' AND substr(creation_idempotency_key,14,1)='-' AND substr(creation_idempotency_key,19,1)='-' AND substr(creation_idempotency_key,24,1)='-' AND length(replace(creation_idempotency_key,'-',''))=32 AND replace(creation_idempotency_key,'-','') NOT GLOB '*[^0-9a-f]*'),
                    creation_request_fingerprint TEXT NOT NULL CHECK(length(creation_request_fingerprint)=71 AND substr(creation_request_fingerprint,1,7)='sha256:' AND substr(creation_request_fingerprint,8)=lower(substr(creation_request_fingerprint,8)) AND substr(creation_request_fingerprint,8) NOT GLOB '*[^0-9a-f]*'),
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );
                CREATE UNIQUE INDEX idx_closure_programs_held_destination
                    ON closure_programs(repository_identity,destination_ref)
                    WHERE destination_claim_state='held';
                CREATE INDEX idx_closure_programs_state_updated
                    ON closure_programs(state,updated_at,id);

                CREATE TABLE closure_source_launch_reservations (
                    id TEXT PRIMARY KEY CHECK(length(id)=36 AND id=lower(id) AND substr(id,9,1)='-' AND substr(id,14,1)='-' AND substr(id,19,1)='-' AND substr(id,24,1)='-' AND length(replace(id,'-',''))=32 AND replace(id,'-','') NOT GLOB '*[^0-9a-f]*'),
                    program_id TEXT NOT NULL UNIQUE CHECK(length(program_id)=36 AND program_id=lower(program_id) AND substr(program_id,9,1)='-' AND substr(program_id,14,1)='-' AND substr(program_id,19,1)='-' AND substr(program_id,24,1)='-' AND length(replace(program_id,'-',''))=32 AND replace(program_id,'-','') NOT GLOB '*[^0-9a-f]*') REFERENCES closure_programs(id) ON DELETE RESTRICT,
                    idempotency_key TEXT NOT NULL UNIQUE CHECK(length(idempotency_key)=36 AND idempotency_key=lower(idempotency_key) AND substr(idempotency_key,9,1)='-' AND substr(idempotency_key,14,1)='-' AND substr(idempotency_key,19,1)='-' AND substr(idempotency_key,24,1)='-' AND length(replace(idempotency_key,'-',''))=32 AND replace(idempotency_key,'-','') NOT GLOB '*[^0-9a-f]*'),
                    request_fingerprint TEXT NOT NULL CHECK(length(request_fingerprint)=71 AND substr(request_fingerprint,1,7)='sha256:' AND substr(request_fingerprint,8)=lower(substr(request_fingerprint,8)) AND substr(request_fingerprint,8) NOT GLOB '*[^0-9a-f]*'),
                    request_json TEXT NOT NULL,
                    source_id TEXT NOT NULL UNIQUE CHECK(length(source_id)=36 AND source_id=lower(source_id) AND substr(source_id,9,1)='-' AND substr(source_id,14,1)='-' AND substr(source_id,19,1)='-' AND substr(source_id,24,1)='-' AND length(replace(source_id,'-',''))=32 AND replace(source_id,'-','') NOT GLOB '*[^0-9a-f]*'),
                    root_session_id TEXT NOT NULL UNIQUE CHECK(length(root_session_id)=36 AND root_session_id=lower(root_session_id) AND substr(root_session_id,9,1)='-' AND substr(root_session_id,14,1)='-' AND substr(root_session_id,19,1)='-' AND substr(root_session_id,24,1)='-' AND length(replace(root_session_id,'-',''))=32 AND replace(root_session_id,'-','') NOT GLOB '*[^0-9a-f]*'),
                    custody_id TEXT NOT NULL UNIQUE CHECK(length(custody_id)=36 AND custody_id=lower(custody_id) AND substr(custody_id,9,1)='-' AND substr(custody_id,14,1)='-' AND substr(custody_id,19,1)='-' AND substr(custody_id,24,1)='-' AND length(replace(custody_id,'-',''))=32 AND replace(custody_id,'-','') NOT GLOB '*[^0-9a-f]*'),
                    custody_generation INTEGER NOT NULL CHECK(custody_generation=1),
                    branch_name TEXT NOT NULL UNIQUE,
                    repository_identity TEXT NOT NULL,
                    source_ref TEXT NOT NULL UNIQUE CHECK(substr(source_ref,1,11)='refs/heads/' AND length(source_ref)>11),
                    source_base_sha TEXT NOT NULL CHECK(length(source_base_sha) IN (40,64) AND source_base_sha=lower(source_base_sha) AND source_base_sha NOT GLOB '*[^0-9a-f]*'),
                    destination_ref TEXT NOT NULL CHECK(substr(destination_ref,1,11)='refs/heads/' AND length(destination_ref)>11),
                    destination_pre_head TEXT NOT NULL CHECK(length(destination_pre_head) IN (40,64) AND destination_pre_head=lower(destination_pre_head) AND destination_pre_head NOT GLOB '*[^0-9a-f]*'),
                    staging_ref TEXT NOT NULL UNIQUE CHECK(substr(staging_ref,1,29)='refs/heads/rsi/closure-stage/' AND length(staging_ref)>29),
                    created_at TEXT NOT NULL,
                    completed_at TEXT
                );

                CREATE TABLE closure_sources (
                    id TEXT PRIMARY KEY CHECK(length(id)=36 AND id=lower(id) AND substr(id,9,1)='-' AND substr(id,14,1)='-' AND substr(id,19,1)='-' AND substr(id,24,1)='-' AND length(replace(id,'-',''))=32 AND replace(id,'-','') NOT GLOB '*[^0-9a-f]*'),
                    program_id TEXT NOT NULL UNIQUE CHECK(length(program_id)=36 AND program_id=lower(program_id) AND substr(program_id,9,1)='-' AND substr(program_id,14,1)='-' AND substr(program_id,19,1)='-' AND substr(program_id,24,1)='-' AND length(replace(program_id,'-',''))=32 AND replace(program_id,'-','') NOT GLOB '*[^0-9a-f]*') REFERENCES closure_programs(id) ON DELETE RESTRICT,
                    custody_id TEXT NOT NULL CHECK(length(custody_id)=36 AND custody_id=lower(custody_id) AND substr(custody_id,9,1)='-' AND substr(custody_id,14,1)='-' AND substr(custody_id,19,1)='-' AND substr(custody_id,24,1)='-' AND length(replace(custody_id,'-',''))=32 AND replace(custody_id,'-','') NOT GLOB '*[^0-9a-f]*') REFERENCES sandbox_custody_roots(custody_id) ON DELETE RESTRICT,
                    custody_generation INTEGER NOT NULL CHECK(custody_generation>0),
                    root_session_id TEXT NOT NULL UNIQUE CHECK(length(root_session_id)=36 AND root_session_id=lower(root_session_id) AND substr(root_session_id,9,1)='-' AND substr(root_session_id,14,1)='-' AND substr(root_session_id,19,1)='-' AND substr(root_session_id,24,1)='-' AND length(replace(root_session_id,'-',''))=32 AND replace(root_session_id,'-','') NOT GLOB '*[^0-9a-f]*') REFERENCES sessions(id) ON DELETE RESTRICT,
                    source_ref TEXT NOT NULL UNIQUE CHECK(substr(source_ref,1,11)='refs/heads/' AND length(source_ref)>11),
                    source_base_sha TEXT NOT NULL CHECK(length(source_base_sha) IN (40,64) AND source_base_sha=lower(source_base_sha) AND source_base_sha NOT GLOB '*[^0-9a-f]*'),
                    source_head TEXT CHECK(source_head IS NULL OR (length(source_head) IN (40,64) AND source_head=lower(source_head) AND source_head NOT GLOB '*[^0-9a-f]*')),
                    destination_ref TEXT NOT NULL CHECK(substr(destination_ref,1,11)='refs/heads/' AND length(destination_ref)>11),
                    destination_pre_head TEXT NOT NULL CHECK(length(destination_pre_head) IN (40,64) AND destination_pre_head=lower(destination_pre_head) AND destination_pre_head NOT GLOB '*[^0-9a-f]*'),
                    staging_ref TEXT NOT NULL UNIQUE CHECK(substr(staging_ref,1,29)='refs/heads/rsi/closure-stage/' AND length(staging_ref)>29),
                    state TEXT NOT NULL CHECK(state IN ('working','outcome_committed','outcome_no_change','outcome_blocker','outcome_blocked','awaiting_evidence','eligible','integration_queued','integrated','retained','cleanup_complete')),
                    launch_idempotency_key TEXT NOT NULL UNIQUE CHECK(length(launch_idempotency_key)=36 AND launch_idempotency_key=lower(launch_idempotency_key) AND substr(launch_idempotency_key,9,1)='-' AND substr(launch_idempotency_key,14,1)='-' AND substr(launch_idempotency_key,19,1)='-' AND substr(launch_idempotency_key,24,1)='-' AND length(replace(launch_idempotency_key,'-',''))=32 AND replace(launch_idempotency_key,'-','') NOT GLOB '*[^0-9a-f]*'),
                    launch_request_fingerprint TEXT NOT NULL CHECK(length(launch_request_fingerprint)=71 AND substr(launch_request_fingerprint,1,7)='sha256:' AND substr(launch_request_fingerprint,8)=lower(substr(launch_request_fingerprint,8)) AND substr(launch_request_fingerprint,8) NOT GLOB '*[^0-9a-f]*'),
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    UNIQUE(custody_id,custody_generation)
                );
                CREATE INDEX idx_closure_sources_recovery
                    ON closure_sources(created_at,id);
                CREATE INDEX idx_closure_sources_state
                    ON closure_sources(state,created_at,id);

                CREATE TABLE closure_source_sessions (
                    source_id TEXT NOT NULL CHECK(length(source_id)=36 AND source_id=lower(source_id) AND substr(source_id,9,1)='-' AND substr(source_id,14,1)='-' AND substr(source_id,19,1)='-' AND substr(source_id,24,1)='-' AND length(replace(source_id,'-',''))=32 AND replace(source_id,'-','') NOT GLOB '*[^0-9a-f]*') REFERENCES closure_sources(id) ON DELETE RESTRICT,
                    session_id TEXT NOT NULL UNIQUE CHECK(length(session_id)=36 AND session_id=lower(session_id) AND substr(session_id,9,1)='-' AND substr(session_id,14,1)='-' AND substr(session_id,19,1)='-' AND substr(session_id,24,1)='-' AND length(replace(session_id,'-',''))=32 AND replace(session_id,'-','') NOT GLOB '*[^0-9a-f]*') REFERENCES sessions(id) ON DELETE RESTRICT,
                    rotation_depth INTEGER NOT NULL CHECK(rotation_depth>=0),
                    custody_id TEXT NOT NULL CHECK(length(custody_id)=36 AND custody_id=lower(custody_id) AND substr(custody_id,9,1)='-' AND substr(custody_id,14,1)='-' AND substr(custody_id,19,1)='-' AND substr(custody_id,24,1)='-' AND length(replace(custody_id,'-',''))=32 AND replace(custody_id,'-','') NOT GLOB '*[^0-9a-f]*') REFERENCES sandbox_custody_roots(custody_id) ON DELETE RESTRICT,
                    custody_generation INTEGER NOT NULL CHECK(custody_generation>0),
                    continued_from_session_id TEXT CHECK(continued_from_session_id IS NULL OR (length(continued_from_session_id)=36 AND continued_from_session_id=lower(continued_from_session_id) AND substr(continued_from_session_id,9,1)='-' AND substr(continued_from_session_id,14,1)='-' AND substr(continued_from_session_id,19,1)='-' AND substr(continued_from_session_id,24,1)='-' AND length(replace(continued_from_session_id,'-',''))=32 AND replace(continued_from_session_id,'-','') NOT GLOB '*[^0-9a-f]*')) REFERENCES sessions(id) ON DELETE RESTRICT,
                    bound_at TEXT NOT NULL,
                    PRIMARY KEY(source_id,rotation_depth),
                    UNIQUE(source_id,session_id),
                    CHECK((rotation_depth=0 AND continued_from_session_id IS NULL) OR (rotation_depth>0 AND continued_from_session_id IS NOT NULL))
                );

                CREATE TABLE conversation_event_provenance (
                    conversation_event_id INTEGER PRIMARY KEY REFERENCES conversation_events(id) ON DELETE RESTRICT,
                    producer_kind TEXT NOT NULL CHECK(producer_kind IN ('provider_assistant_output','daemon_provider_diagnostic','provider_other')),
                    model_invocation_id TEXT NOT NULL CHECK(length(model_invocation_id)=36 AND model_invocation_id=lower(model_invocation_id) AND substr(model_invocation_id,9,1)='-' AND substr(model_invocation_id,14,1)='-' AND substr(model_invocation_id,19,1)='-' AND substr(model_invocation_id,24,1)='-' AND length(replace(model_invocation_id,'-',''))=32 AND replace(model_invocation_id,'-','') NOT GLOB '*[^0-9a-f]*') REFERENCES model_invocations(id) ON DELETE RESTRICT,
                    provider_event_type TEXT NOT NULL CHECK(length(provider_event_type) BETWEEN 1 AND 128),
                    created_at TEXT NOT NULL
                );
                CREATE INDEX idx_conversation_event_provenance_invocation
                    ON conversation_event_provenance(model_invocation_id,producer_kind,conversation_event_id);
                CREATE TRIGGER conversation_event_provenance_identity_guard BEFORE INSERT ON conversation_event_provenance
                WHEN NOT EXISTS (
                    SELECT 1 FROM conversation_events e
                    JOIN model_invocations mi ON mi.id=NEW.model_invocation_id
                    WHERE e.id=NEW.conversation_event_id AND mi.session_id=e.session_id
                      AND (NEW.producer_kind='provider_other'
                           OR (e.event_type='Message' AND e.role='Assistant'))
                )
                BEGIN SELECT RAISE(ABORT,'conversation event provenance must match event/invocation identity'); END;

                CREATE TABLE closure_output_validations (
                    id TEXT PRIMARY KEY CHECK(length(id)=36 AND id=lower(id) AND substr(id,9,1)='-' AND substr(id,14,1)='-' AND substr(id,19,1)='-' AND substr(id,24,1)='-' AND length(replace(id,'-',''))=32 AND replace(id,'-','') NOT GLOB '*[^0-9a-f]*'),
                    source_id TEXT NOT NULL CHECK(length(source_id)=36 AND source_id=lower(source_id) AND substr(source_id,9,1)='-' AND substr(source_id,14,1)='-' AND substr(source_id,19,1)='-' AND substr(source_id,24,1)='-' AND length(replace(source_id,'-',''))=32 AND replace(source_id,'-','') NOT GLOB '*[^0-9a-f]*') REFERENCES closure_sources(id) ON DELETE RESTRICT,
                    tip_session_id TEXT NOT NULL CHECK(length(tip_session_id)=36 AND tip_session_id=lower(tip_session_id) AND substr(tip_session_id,9,1)='-' AND substr(tip_session_id,14,1)='-' AND substr(tip_session_id,19,1)='-' AND substr(tip_session_id,24,1)='-' AND length(replace(tip_session_id,'-',''))=32 AND replace(tip_session_id,'-','') NOT GLOB '*[^0-9a-f]*') REFERENCES sessions(id) ON DELETE RESTRICT,
                    model_invocation_id TEXT NOT NULL CHECK(length(model_invocation_id)=36 AND model_invocation_id=lower(model_invocation_id) AND substr(model_invocation_id,9,1)='-' AND substr(model_invocation_id,14,1)='-' AND substr(model_invocation_id,19,1)='-' AND substr(model_invocation_id,24,1)='-' AND length(replace(model_invocation_id,'-',''))=32 AND replace(model_invocation_id,'-','') NOT GLOB '*[^0-9a-f]*') REFERENCES model_invocations(id) ON DELETE RESTRICT,
                    conversation_event_id INTEGER REFERENCES conversation_events(id) ON DELETE RESTRICT,
                    conversation_sequence INTEGER,
                    producer_kind TEXT CHECK(producer_kind IS NULL OR producer_kind='provider_assistant_output'),
                    provider_event_type TEXT,
                    parser_source TEXT NOT NULL CHECK(parser_source IN ('closure_handoff_field_v1','missing_provider_output')),
                    raw_handoff TEXT,
                    raw_handoff_digest TEXT CHECK(raw_handoff_digest IS NULL OR (length(raw_handoff_digest)=71 AND substr(raw_handoff_digest,1,7)='sha256:' AND substr(raw_handoff_digest,8)=lower(substr(raw_handoff_digest,8)) AND substr(raw_handoff_digest,8) NOT GLOB '*[^0-9a-f]*')),
                    normalized_envelope_json TEXT,
                    normalized_envelope_digest TEXT CHECK(normalized_envelope_digest IS NULL OR (length(normalized_envelope_digest)=71 AND substr(normalized_envelope_digest,1,7)='sha256:' AND substr(normalized_envelope_digest,8)=lower(substr(normalized_envelope_digest,8)) AND substr(normalized_envelope_digest,8) NOT GLOB '*[^0-9a-f]*')),
                    disposition TEXT NOT NULL CHECK(disposition IN ('accepted_committed','accepted_no_change','accepted_blocker','missing_provider_output','malformed_output','correlation_mismatch','blocked_ambiguous_lineage','blocked_outcome_git_mismatch')),
                    validation_issues_json TEXT NOT NULL,
                    source_state TEXT NOT NULL CHECK(source_state IN ('outcome_committed','outcome_no_change','outcome_blocker','outcome_blocked')),
                    observed_source_ref_head TEXT CHECK(observed_source_ref_head IS NULL OR (length(observed_source_ref_head) IN (40,64) AND observed_source_ref_head=lower(observed_source_ref_head) AND observed_source_ref_head NOT GLOB '*[^0-9a-f]*')),
                    observed_worktree_head TEXT CHECK(observed_worktree_head IS NULL OR (length(observed_worktree_head) IN (40,64) AND observed_worktree_head=lower(observed_worktree_head) AND observed_worktree_head NOT GLOB '*[^0-9a-f]*')),
                    observed_worktree_clean INTEGER CHECK(observed_worktree_clean IN (0,1) OR observed_worktree_clean IS NULL),
                    created_at TEXT NOT NULL,
                    UNIQUE(source_id,tip_session_id,model_invocation_id),
                    CHECK((parser_source='missing_provider_output' AND conversation_event_id IS NULL AND conversation_sequence IS NULL AND producer_kind IS NULL AND raw_handoff IS NULL)
                       OR (parser_source='closure_handoff_field_v1' AND conversation_event_id IS NOT NULL AND conversation_sequence IS NOT NULL AND producer_kind='provider_assistant_output' AND raw_handoff IS NOT NULL)),
                    CHECK((disposition IN ('accepted_committed','accepted_no_change','accepted_blocker') AND normalized_envelope_json IS NOT NULL AND normalized_envelope_digest IS NOT NULL)
                       OR (disposition NOT IN ('accepted_committed','accepted_no_change','accepted_blocker')))
                );
                CREATE INDEX idx_closure_output_validations_source
                    ON closure_output_validations(source_id,created_at,id);

                CREATE TABLE closure_evidence (
                    id TEXT PRIMARY KEY CHECK(length(id)=36 AND id=lower(id) AND substr(id,9,1)='-' AND substr(id,14,1)='-' AND substr(id,19,1)='-' AND substr(id,24,1)='-' AND length(replace(id,'-',''))=32 AND replace(id,'-','') NOT GLOB '*[^0-9a-f]*'),
                    source_id TEXT NOT NULL UNIQUE CHECK(length(source_id)=36 AND source_id=lower(source_id) AND substr(source_id,9,1)='-' AND substr(source_id,14,1)='-' AND substr(source_id,19,1)='-' AND substr(source_id,24,1)='-' AND length(replace(source_id,'-',''))=32 AND replace(source_id,'-','') NOT GLOB '*[^0-9a-f]*') REFERENCES closure_sources(id) ON DELETE RESTRICT,
                    disposition TEXT NOT NULL CHECK(disposition IN ('required_independent','not_required_tier0_deterministic','not_required_proven_no_change')),
                    reviewed_source_head TEXT NOT NULL CHECK(length(reviewed_source_head) IN (40,64) AND reviewed_source_head=lower(reviewed_source_head) AND reviewed_source_head NOT GLOB '*[^0-9a-f]*'),
                    review_schema_version INTEGER,
                    review_raw_bytes BLOB,
                    review_digest TEXT CHECK(review_digest IS NULL OR (length(review_digest)=71 AND substr(review_digest,1,7)='sha256:' AND substr(review_digest,8)=lower(substr(review_digest,8)) AND substr(review_digest,8) NOT GLOB '*[^0-9a-f]*')),
                    normalized_review_json TEXT,
                    finding_set_digest TEXT CHECK(finding_set_digest IS NULL OR (length(finding_set_digest)=71 AND substr(finding_set_digest,1,7)='sha256:' AND substr(finding_set_digest,8)=lower(substr(finding_set_digest,8)) AND substr(finding_set_digest,8) NOT GLOB '*[^0-9a-f]*')),
                    reviewer_session_id TEXT CHECK(reviewer_session_id IS NULL OR (length(reviewer_session_id)=36 AND reviewer_session_id=lower(reviewer_session_id) AND substr(reviewer_session_id,9,1)='-' AND substr(reviewer_session_id,14,1)='-' AND substr(reviewer_session_id,19,1)='-' AND substr(reviewer_session_id,24,1)='-' AND length(replace(reviewer_session_id,'-',''))=32 AND replace(reviewer_session_id,'-','') NOT GLOB '*[^0-9a-f]*')) REFERENCES sessions(id) ON DELETE RESTRICT,
                    reviewer_model_invocation_id TEXT CHECK(reviewer_model_invocation_id IS NULL OR (length(reviewer_model_invocation_id)=36 AND reviewer_model_invocation_id=lower(reviewer_model_invocation_id) AND substr(reviewer_model_invocation_id,9,1)='-' AND substr(reviewer_model_invocation_id,14,1)='-' AND substr(reviewer_model_invocation_id,19,1)='-' AND substr(reviewer_model_invocation_id,24,1)='-' AND length(replace(reviewer_model_invocation_id,'-',''))=32 AND replace(reviewer_model_invocation_id,'-','') NOT GLOB '*[^0-9a-f]*')) REFERENCES model_invocations(id) ON DELETE RESTRICT,
                    reviewer_provider TEXT,
                    reviewer_model TEXT,
                    reviewer_custody_id TEXT CHECK(reviewer_custody_id IS NULL OR (length(reviewer_custody_id)=36 AND reviewer_custody_id=lower(reviewer_custody_id) AND substr(reviewer_custody_id,9,1)='-' AND substr(reviewer_custody_id,14,1)='-' AND substr(reviewer_custody_id,19,1)='-' AND substr(reviewer_custody_id,24,1)='-' AND length(replace(reviewer_custody_id,'-',''))=32 AND replace(reviewer_custody_id,'-','') NOT GLOB '*[^0-9a-f]*')) REFERENCES sandbox_custody_roots(custody_id) ON DELETE RESTRICT,
                    reviewer_custody_generation INTEGER,
                    evidence_commit_sha TEXT CHECK(evidence_commit_sha IS NULL OR (length(evidence_commit_sha) IN (40,64) AND evidence_commit_sha=lower(evidence_commit_sha) AND evidence_commit_sha NOT GLOB '*[^0-9a-f]*')),
                    evidence_parent_sha TEXT CHECK(evidence_parent_sha IS NULL OR (length(evidence_parent_sha) IN (40,64) AND evidence_parent_sha=lower(evidence_parent_sha) AND evidence_parent_sha NOT GLOB '*[^0-9a-f]*')),
                    review_json_path TEXT,
                    manifest_v2_path TEXT,
                    review_handoff_event_id INTEGER REFERENCES conversation_events(id) ON DELETE RESTRICT,
                    review_handoff_digest TEXT CHECK(review_handoff_digest IS NULL OR (length(review_handoff_digest)=71 AND substr(review_handoff_digest,1,7)='sha256:' AND substr(review_handoff_digest,8)=lower(substr(review_handoff_digest,8)) AND substr(review_handoff_digest,8) NOT GLOB '*[^0-9a-f]*')),
                    manifest_schema_version INTEGER CHECK(manifest_schema_version IS NULL OR manifest_schema_version=2),
                    manifest_raw_bytes BLOB,
                    manifest_digest TEXT CHECK(manifest_digest IS NULL OR (length(manifest_digest)=71 AND substr(manifest_digest,1,7)='sha256:' AND substr(manifest_digest,8)=lower(substr(manifest_digest,8)) AND substr(manifest_digest,8) NOT GLOB '*[^0-9a-f]*')),
                    manifest_source_head TEXT CHECK(manifest_source_head IS NULL OR (length(manifest_source_head) IN (40,64) AND manifest_source_head=lower(manifest_source_head) AND manifest_source_head NOT GLOB '*[^0-9a-f]*')),
                    source_ref_before TEXT NOT NULL CHECK(length(source_ref_before) IN (40,64) AND source_ref_before=lower(source_ref_before) AND source_ref_before NOT GLOB '*[^0-9a-f]*'),
                    source_ref_after TEXT NOT NULL CHECK(length(source_ref_after) IN (40,64) AND source_ref_after=lower(source_ref_after) AND source_ref_after NOT GLOB '*[^0-9a-f]*'),
                    source_worktree_head_before TEXT NOT NULL CHECK(length(source_worktree_head_before) IN (40,64) AND source_worktree_head_before=lower(source_worktree_head_before) AND source_worktree_head_before NOT GLOB '*[^0-9a-f]*'),
                    source_worktree_head_after TEXT NOT NULL CHECK(length(source_worktree_head_after) IN (40,64) AND source_worktree_head_after=lower(source_worktree_head_after) AND source_worktree_head_after NOT GLOB '*[^0-9a-f]*'),
                    review_policy_digest TEXT NOT NULL CHECK(length(review_policy_digest)=71 AND substr(review_policy_digest,1,7)='sha256:' AND substr(review_policy_digest,8)=lower(substr(review_policy_digest,8)) AND substr(review_policy_digest,8) NOT GLOB '*[^0-9a-f]*'),
                    verification_policy_digest TEXT NOT NULL CHECK(length(verification_policy_digest)=71 AND substr(verification_policy_digest,1,7)='sha256:' AND substr(verification_policy_digest,8)=lower(substr(verification_policy_digest,8)) AND substr(verification_policy_digest,8) NOT GLOB '*[^0-9a-f]*'),
                    verifier_policy_digest TEXT NOT NULL CHECK(length(verifier_policy_digest)=71 AND substr(verifier_policy_digest,1,7)='sha256:' AND substr(verifier_policy_digest,8)=lower(substr(verifier_policy_digest,8)) AND substr(verifier_policy_digest,8) NOT GLOB '*[^0-9a-f]*'),
                    accepted_by TEXT NOT NULL,
                    accepted_at TEXT NOT NULL,
                    CHECK(source_ref_before=reviewed_source_head AND source_ref_after=reviewed_source_head AND source_worktree_head_before=reviewed_source_head AND source_worktree_head_after=reviewed_source_head),
                    CHECK((disposition='required_independent' AND review_schema_version=1 AND review_raw_bytes IS NOT NULL AND review_digest IS NOT NULL AND normalized_review_json IS NOT NULL AND finding_set_digest IS NOT NULL AND reviewer_session_id IS NOT NULL AND reviewer_model_invocation_id IS NOT NULL AND reviewer_provider IS NOT NULL AND reviewer_custody_id IS NOT NULL AND reviewer_custody_generation IS NOT NULL AND evidence_commit_sha IS NOT NULL AND evidence_parent_sha=reviewed_source_head AND review_json_path IS NOT NULL AND manifest_v2_path IS NOT NULL AND review_handoff_event_id IS NOT NULL AND review_handoff_digest IS NOT NULL AND manifest_schema_version=2 AND manifest_raw_bytes IS NOT NULL AND manifest_digest IS NOT NULL AND manifest_source_head=reviewed_source_head)
                       OR (disposition!='required_independent' AND review_schema_version IS NULL AND review_raw_bytes IS NULL AND review_digest IS NULL AND normalized_review_json IS NULL AND finding_set_digest IS NULL AND reviewer_session_id IS NULL AND reviewer_model_invocation_id IS NULL AND reviewer_provider IS NULL AND reviewer_model IS NULL AND reviewer_custody_id IS NULL AND reviewer_custody_generation IS NULL AND evidence_commit_sha IS NULL AND evidence_parent_sha IS NULL AND review_json_path IS NULL AND manifest_v2_path IS NULL AND review_handoff_event_id IS NULL AND review_handoff_digest IS NULL AND manifest_schema_version IS NULL AND manifest_raw_bytes IS NULL AND manifest_digest IS NULL AND manifest_source_head IS NULL))
                );

                CREATE TABLE closure_integration_queue (
                    id TEXT PRIMARY KEY CHECK(length(id)=36 AND id=lower(id) AND substr(id,9,1)='-' AND substr(id,14,1)='-' AND substr(id,19,1)='-' AND substr(id,24,1)='-' AND length(replace(id,'-',''))=32 AND replace(id,'-','') NOT GLOB '*[^0-9a-f]*'),
                    source_id TEXT NOT NULL UNIQUE CHECK(length(source_id)=36 AND source_id=lower(source_id) AND substr(source_id,9,1)='-' AND substr(source_id,14,1)='-' AND substr(source_id,19,1)='-' AND substr(source_id,24,1)='-' AND length(replace(source_id,'-',''))=32 AND replace(source_id,'-','') NOT GLOB '*[^0-9a-f]*') REFERENCES closure_sources(id) ON DELETE RESTRICT,
                    evidence_id TEXT NOT NULL UNIQUE CHECK(length(evidence_id)=36 AND evidence_id=lower(evidence_id) AND substr(evidence_id,9,1)='-' AND substr(evidence_id,14,1)='-' AND substr(evidence_id,19,1)='-' AND substr(evidence_id,24,1)='-' AND length(replace(evidence_id,'-',''))=32 AND replace(evidence_id,'-','') NOT GLOB '*[^0-9a-f]*') REFERENCES closure_evidence(id) ON DELETE RESTRICT,
                    source_head TEXT NOT NULL CHECK(length(source_head) IN (40,64) AND source_head=lower(source_head) AND source_head NOT GLOB '*[^0-9a-f]*'),
                    state TEXT NOT NULL CHECK(state IN ('eligible_k2','claimed','completed','blocked')),
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );

                CREATE TABLE closure_target_leases (
                    id TEXT PRIMARY KEY CHECK(length(id)=36 AND id=lower(id) AND substr(id,9,1)='-' AND substr(id,14,1)='-' AND substr(id,19,1)='-' AND substr(id,24,1)='-' AND length(replace(id,'-',''))=32 AND replace(id,'-','') NOT GLOB '*[^0-9a-f]*'),
                    repository_identity TEXT NOT NULL,
                    target_ref TEXT NOT NULL CHECK(substr(target_ref,1,11)='refs/heads/' AND length(target_ref)>11),
                    program_id TEXT NOT NULL CHECK(length(program_id)=36 AND program_id=lower(program_id) AND substr(program_id,9,1)='-' AND substr(program_id,14,1)='-' AND substr(program_id,19,1)='-' AND substr(program_id,24,1)='-' AND length(replace(program_id,'-',''))=32 AND replace(program_id,'-','') NOT GLOB '*[^0-9a-f]*') REFERENCES closure_programs(id) ON DELETE RESTRICT,
                    generation INTEGER NOT NULL CHECK(generation>0),
                    state TEXT NOT NULL CHECK(state IN ('held','released','expired')),
                    holder_boot_id TEXT NOT NULL CHECK(length(holder_boot_id)=36 AND holder_boot_id=lower(holder_boot_id) AND substr(holder_boot_id,9,1)='-' AND substr(holder_boot_id,14,1)='-' AND substr(holder_boot_id,19,1)='-' AND substr(holder_boot_id,24,1)='-' AND length(replace(holder_boot_id,'-',''))=32 AND replace(holder_boot_id,'-','') NOT GLOB '*[^0-9a-f]*'),
                    expires_at TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );
                CREATE UNIQUE INDEX idx_closure_target_leases_held
                    ON closure_target_leases(repository_identity,target_ref)
                    WHERE state='held';

                CREATE TABLE closure_integration_attempts (
                    id TEXT PRIMARY KEY CHECK(length(id)=36 AND id=lower(id) AND substr(id,9,1)='-' AND substr(id,14,1)='-' AND substr(id,19,1)='-' AND substr(id,24,1)='-' AND length(replace(id,'-',''))=32 AND replace(id,'-','') NOT GLOB '*[^0-9a-f]*'),
                    source_id TEXT NOT NULL CHECK(length(source_id)=36 AND source_id=lower(source_id) AND substr(source_id,9,1)='-' AND substr(source_id,14,1)='-' AND substr(source_id,19,1)='-' AND substr(source_id,24,1)='-' AND length(replace(source_id,'-',''))=32 AND replace(source_id,'-','') NOT GLOB '*[^0-9a-f]*') REFERENCES closure_sources(id) ON DELETE RESTRICT,
                    lease_id TEXT CHECK(lease_id IS NULL OR (length(lease_id)=36 AND lease_id=lower(lease_id) AND substr(lease_id,9,1)='-' AND substr(lease_id,14,1)='-' AND substr(lease_id,19,1)='-' AND substr(lease_id,24,1)='-' AND length(replace(lease_id,'-',''))=32 AND replace(lease_id,'-','') NOT GLOB '*[^0-9a-f]*')) REFERENCES closure_target_leases(id) ON DELETE RESTRICT,
                    candidate_receipt_id TEXT CHECK(candidate_receipt_id IS NULL OR (length(candidate_receipt_id)=36 AND candidate_receipt_id=lower(candidate_receipt_id) AND substr(candidate_receipt_id,9,1)='-' AND substr(candidate_receipt_id,14,1)='-' AND substr(candidate_receipt_id,19,1)='-' AND substr(candidate_receipt_id,24,1)='-' AND length(replace(candidate_receipt_id,'-',''))=32 AND replace(candidate_receipt_id,'-','') NOT GLOB '*[^0-9a-f]*')),
                    phase TEXT NOT NULL CHECK(phase IN ('reserved','staging_cas_pending','candidate_gate_pending','candidate_verified','destination_preflight','detach_pending','detached','destination_cas_pending','destination_applied','final_gate_pending','reattach_pending','reattached','complete','conflict','reconciliation_blocked')),
                    staging_old_head TEXT CHECK(staging_old_head IS NULL OR (length(staging_old_head) IN (40,64) AND staging_old_head=lower(staging_old_head) AND staging_old_head NOT GLOB '*[^0-9a-f]*')),
                    staging_new_head TEXT CHECK(staging_new_head IS NULL OR (length(staging_new_head) IN (40,64) AND staging_new_head=lower(staging_new_head) AND staging_new_head NOT GLOB '*[^0-9a-f]*')),
                    destination_old_head TEXT CHECK(destination_old_head IS NULL OR (length(destination_old_head) IN (40,64) AND destination_old_head=lower(destination_old_head) AND destination_old_head NOT GLOB '*[^0-9a-f]*')),
                    destination_new_head TEXT CHECK(destination_new_head IS NULL OR (length(destination_new_head) IN (40,64) AND destination_new_head=lower(destination_new_head) AND destination_new_head NOT GLOB '*[^0-9a-f]*')),
                    checkout_path TEXT,
                    request_id TEXT CHECK(request_id IS NULL OR (length(request_id)=36 AND request_id=lower(request_id) AND substr(request_id,9,1)='-' AND substr(request_id,14,1)='-' AND substr(request_id,19,1)='-' AND substr(request_id,24,1)='-' AND length(replace(request_id,'-',''))=32 AND replace(request_id,'-','') NOT GLOB '*[^0-9a-f]*')),
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );

                CREATE TABLE integration_receipts (
                    id TEXT PRIMARY KEY CHECK(length(id)=36 AND id=lower(id) AND substr(id,9,1)='-' AND substr(id,14,1)='-' AND substr(id,19,1)='-' AND substr(id,24,1)='-' AND length(replace(id,'-',''))=32 AND replace(id,'-','') NOT GLOB '*[^0-9a-f]*'),
                    parent_receipt_id TEXT CHECK(parent_receipt_id IS NULL OR (length(parent_receipt_id)=36 AND parent_receipt_id=lower(parent_receipt_id) AND substr(parent_receipt_id,9,1)='-' AND substr(parent_receipt_id,14,1)='-' AND substr(parent_receipt_id,19,1)='-' AND substr(parent_receipt_id,24,1)='-' AND length(replace(parent_receipt_id,'-',''))=32 AND replace(parent_receipt_id,'-','') NOT GLOB '*[^0-9a-f]*')) REFERENCES integration_receipts(id) ON DELETE RESTRICT,
                    program_id TEXT NOT NULL CHECK(length(program_id)=36 AND program_id=lower(program_id) AND substr(program_id,9,1)='-' AND substr(program_id,14,1)='-' AND substr(program_id,19,1)='-' AND substr(program_id,24,1)='-' AND length(replace(program_id,'-',''))=32 AND replace(program_id,'-','') NOT GLOB '*[^0-9a-f]*') REFERENCES closure_programs(id) ON DELETE RESTRICT,
                    source_id TEXT NOT NULL CHECK(length(source_id)=36 AND source_id=lower(source_id) AND substr(source_id,9,1)='-' AND substr(source_id,14,1)='-' AND substr(source_id,19,1)='-' AND substr(source_id,24,1)='-' AND length(replace(source_id,'-',''))=32 AND replace(source_id,'-','') NOT GLOB '*[^0-9a-f]*') REFERENCES closure_sources(id) ON DELETE RESTRICT,
                    attempt_id TEXT CHECK(attempt_id IS NULL OR (length(attempt_id)=36 AND attempt_id=lower(attempt_id) AND substr(attempt_id,9,1)='-' AND substr(attempt_id,14,1)='-' AND substr(attempt_id,19,1)='-' AND substr(attempt_id,24,1)='-' AND length(replace(attempt_id,'-',''))=32 AND replace(attempt_id,'-','') NOT GLOB '*[^0-9a-f]*')) REFERENCES closure_integration_attempts(id) ON DELETE RESTRICT,
                    receipt_kind TEXT NOT NULL CHECK(receipt_kind IN ('candidate_verified','integrated','gate_failed','conflict','accepted_no_change','integrated_after_recheck','quarantined_for_forward_repair','discarded')),
                    method TEXT NOT NULL CHECK(method='fast_forward'),
                    source_base_sha TEXT NOT NULL CHECK(length(source_base_sha) IN (40,64) AND source_base_sha=lower(source_base_sha) AND source_base_sha NOT GLOB '*[^0-9a-f]*'),
                    source_head TEXT NOT NULL CHECK(length(source_head) IN (40,64) AND source_head=lower(source_head) AND source_head NOT GLOB '*[^0-9a-f]*'),
                    staging_pre_head TEXT CHECK(staging_pre_head IS NULL OR (length(staging_pre_head) IN (40,64) AND staging_pre_head=lower(staging_pre_head) AND staging_pre_head NOT GLOB '*[^0-9a-f]*')),
                    staging_post_head TEXT CHECK(staging_post_head IS NULL OR (length(staging_post_head) IN (40,64) AND staging_post_head=lower(staging_post_head) AND staging_post_head NOT GLOB '*[^0-9a-f]*')),
                    expected_destination_pre_head TEXT CHECK(expected_destination_pre_head IS NULL OR (length(expected_destination_pre_head) IN (40,64) AND expected_destination_pre_head=lower(expected_destination_pre_head) AND expected_destination_pre_head NOT GLOB '*[^0-9a-f]*')),
                    destination_pre_head TEXT CHECK(destination_pre_head IS NULL OR (length(destination_pre_head) IN (40,64) AND destination_pre_head=lower(destination_pre_head) AND destination_pre_head NOT GLOB '*[^0-9a-f]*')),
                    destination_post_head TEXT CHECK(destination_post_head IS NULL OR (length(destination_post_head) IN (40,64) AND destination_post_head=lower(destination_post_head) AND destination_post_head NOT GLOB '*[^0-9a-f]*')),
                    evidence_id TEXT CHECK(evidence_id IS NULL OR (length(evidence_id)=36 AND evidence_id=lower(evidence_id) AND substr(evidence_id,9,1)='-' AND substr(evidence_id,14,1)='-' AND substr(evidence_id,19,1)='-' AND substr(evidence_id,24,1)='-' AND length(replace(evidence_id,'-',''))=32 AND replace(evidence_id,'-','') NOT GLOB '*[^0-9a-f]*')) REFERENCES closure_evidence(id) ON DELETE RESTRICT,
                    review_schema_version INTEGER,
                    review_digest TEXT CHECK(review_digest IS NULL OR (length(review_digest)=71 AND substr(review_digest,1,7)='sha256:' AND substr(review_digest,8)=lower(substr(review_digest,8)) AND substr(review_digest,8) NOT GLOB '*[^0-9a-f]*')),
                    review_embedded_source_head TEXT CHECK(review_embedded_source_head IS NULL OR (length(review_embedded_source_head) IN (40,64) AND review_embedded_source_head=lower(review_embedded_source_head) AND review_embedded_source_head NOT GLOB '*[^0-9a-f]*')),
                    manifest_schema_version INTEGER,
                    manifest_digest TEXT CHECK(manifest_digest IS NULL OR (length(manifest_digest)=71 AND substr(manifest_digest,1,7)='sha256:' AND substr(manifest_digest,8)=lower(substr(manifest_digest,8)) AND substr(manifest_digest,8) NOT GLOB '*[^0-9a-f]*')),
                    manifest_embedded_source_head TEXT CHECK(manifest_embedded_source_head IS NULL OR (length(manifest_embedded_source_head) IN (40,64) AND manifest_embedded_source_head=lower(manifest_embedded_source_head) AND manifest_embedded_source_head NOT GLOB '*[^0-9a-f]*')),
                    review_policy_digest TEXT NOT NULL CHECK(length(review_policy_digest)=71 AND substr(review_policy_digest,1,7)='sha256:' AND substr(review_policy_digest,8)=lower(substr(review_policy_digest,8)) AND substr(review_policy_digest,8) NOT GLOB '*[^0-9a-f]*'),
                    verification_policy_digest TEXT NOT NULL CHECK(length(verification_policy_digest)=71 AND substr(verification_policy_digest,1,7)='sha256:' AND substr(verification_policy_digest,8)=lower(substr(verification_policy_digest,8)) AND substr(verification_policy_digest,8) NOT GLOB '*[^0-9a-f]*'),
                    verifier_policy_digest TEXT NOT NULL CHECK(length(verifier_policy_digest)=71 AND substr(verifier_policy_digest,1,7)='sha256:' AND substr(verifier_policy_digest,8)=lower(substr(verifier_policy_digest,8)) AND substr(verifier_policy_digest,8) NOT GLOB '*[^0-9a-f]*'),
                    gate_evidence_json TEXT,
                    disposition TEXT NOT NULL,
                    actor TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    CHECK((receipt_kind='candidate_verified' AND staging_pre_head IS NOT NULL AND staging_post_head IS NOT NULL AND expected_destination_pre_head IS NOT NULL)
                       OR receipt_kind!='candidate_verified'),
                    CHECK((receipt_kind IN ('integrated','gate_failed','conflict','integrated_after_recheck') AND destination_pre_head IS NOT NULL)
                       OR receipt_kind NOT IN ('integrated','gate_failed','conflict','integrated_after_recheck'))
                );

                CREATE TABLE closure_receipt_consumptions (
                    id TEXT PRIMARY KEY CHECK(length(id)=36 AND id=lower(id) AND substr(id,9,1)='-' AND substr(id,14,1)='-' AND substr(id,19,1)='-' AND substr(id,24,1)='-' AND length(replace(id,'-',''))=32 AND replace(id,'-','') NOT GLOB '*[^0-9a-f]*'),
                    candidate_receipt_id TEXT NOT NULL UNIQUE CHECK(length(candidate_receipt_id)=36 AND candidate_receipt_id=lower(candidate_receipt_id) AND substr(candidate_receipt_id,9,1)='-' AND substr(candidate_receipt_id,14,1)='-' AND substr(candidate_receipt_id,19,1)='-' AND substr(candidate_receipt_id,24,1)='-' AND length(replace(candidate_receipt_id,'-',''))=32 AND replace(candidate_receipt_id,'-','') NOT GLOB '*[^0-9a-f]*') REFERENCES integration_receipts(id) ON DELETE RESTRICT,
                    integration_attempt_id TEXT NOT NULL UNIQUE CHECK(length(integration_attempt_id)=36 AND integration_attempt_id=lower(integration_attempt_id) AND substr(integration_attempt_id,9,1)='-' AND substr(integration_attempt_id,14,1)='-' AND substr(integration_attempt_id,19,1)='-' AND substr(integration_attempt_id,24,1)='-' AND length(replace(integration_attempt_id,'-',''))=32 AND replace(integration_attempt_id,'-','') NOT GLOB '*[^0-9a-f]*') REFERENCES closure_integration_attempts(id) ON DELETE RESTRICT,
                    consumed_at TEXT NOT NULL
                );

                CREATE TABLE closure_final_gate_attempt_receipts (
                    id TEXT PRIMARY KEY CHECK(length(id)=36 AND id=lower(id) AND substr(id,9,1)='-' AND substr(id,14,1)='-' AND substr(id,19,1)='-' AND substr(id,24,1)='-' AND length(replace(id,'-',''))=32 AND replace(id,'-','') NOT GLOB '*[^0-9a-f]*'),
                    integration_attempt_id TEXT NOT NULL CHECK(length(integration_attempt_id)=36 AND integration_attempt_id=lower(integration_attempt_id) AND substr(integration_attempt_id,9,1)='-' AND substr(integration_attempt_id,14,1)='-' AND substr(integration_attempt_id,19,1)='-' AND substr(integration_attempt_id,24,1)='-' AND length(replace(integration_attempt_id,'-',''))=32 AND replace(integration_attempt_id,'-','') NOT GLOB '*[^0-9a-f]*') REFERENCES closure_integration_attempts(id) ON DELETE RESTRICT,
                    ordinal INTEGER NOT NULL CHECK(ordinal BETWEEN 1 AND 3),
                    receipt_phase TEXT NOT NULL CHECK(receipt_phase IN ('started','terminal')),
                    failed_destination_receipt_id TEXT CHECK(failed_destination_receipt_id IS NULL OR (length(failed_destination_receipt_id)=36 AND failed_destination_receipt_id=lower(failed_destination_receipt_id) AND substr(failed_destination_receipt_id,9,1)='-' AND substr(failed_destination_receipt_id,14,1)='-' AND substr(failed_destination_receipt_id,19,1)='-' AND substr(failed_destination_receipt_id,24,1)='-' AND length(replace(failed_destination_receipt_id,'-',''))=32 AND replace(failed_destination_receipt_id,'-','') NOT GLOB '*[^0-9a-f]*')) REFERENCES integration_receipts(id) ON DELETE RESTRICT,
                    gated_head TEXT NOT NULL CHECK(length(gated_head) IN (40,64) AND gated_head=lower(gated_head) AND gated_head NOT GLOB '*[^0-9a-f]*'),
                    verifier_policy_digest TEXT NOT NULL CHECK(length(verifier_policy_digest)=71 AND substr(verifier_policy_digest,1,7)='sha256:' AND substr(verifier_policy_digest,8)=lower(substr(verifier_policy_digest,8)) AND substr(verifier_policy_digest,8) NOT GLOB '*[^0-9a-f]*'),
                    reason TEXT CHECK(reason IS NULL OR reason IN ('initial','environmental','flaky')),
                    rationale TEXT,
                    runner_result_json TEXT,
                    disposition TEXT CHECK(disposition IS NULL OR disposition IN ('passed','failed','interrupted_by_restart')),
                    actor TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    UNIQUE(integration_attempt_id,ordinal,receipt_phase),
                    CHECK((receipt_phase='started' AND runner_result_json IS NULL AND disposition IS NULL) OR (receipt_phase='terminal' AND runner_result_json IS NOT NULL AND disposition IS NOT NULL))
                );

                CREATE TABLE closure_gate_failure_settlements (
                    id TEXT PRIMARY KEY CHECK(length(id)=36 AND id=lower(id) AND substr(id,9,1)='-' AND substr(id,14,1)='-' AND substr(id,19,1)='-' AND substr(id,24,1)='-' AND length(replace(id,'-',''))=32 AND replace(id,'-','') NOT GLOB '*[^0-9a-f]*'),
                    source_id TEXT NOT NULL UNIQUE CHECK(length(source_id)=36 AND source_id=lower(source_id) AND substr(source_id,9,1)='-' AND substr(source_id,14,1)='-' AND substr(source_id,19,1)='-' AND substr(source_id,24,1)='-' AND length(replace(source_id,'-',''))=32 AND replace(source_id,'-','') NOT GLOB '*[^0-9a-f]*') REFERENCES closure_sources(id) ON DELETE RESTRICT,
                    failed_destination_receipt_id TEXT NOT NULL UNIQUE CHECK(length(failed_destination_receipt_id)=36 AND failed_destination_receipt_id=lower(failed_destination_receipt_id) AND substr(failed_destination_receipt_id,9,1)='-' AND substr(failed_destination_receipt_id,14,1)='-' AND substr(failed_destination_receipt_id,19,1)='-' AND substr(failed_destination_receipt_id,24,1)='-' AND length(replace(failed_destination_receipt_id,'-',''))=32 AND replace(failed_destination_receipt_id,'-','') NOT GLOB '*[^0-9a-f]*') REFERENCES integration_receipts(id) ON DELETE RESTRICT,
                    expected_current_destination_head TEXT NOT NULL CHECK(length(expected_current_destination_head) IN (40,64) AND expected_current_destination_head=lower(expected_current_destination_head) AND expected_current_destination_head NOT GLOB '*[^0-9a-f]*'),
                    observed_current_destination_head TEXT CHECK(observed_current_destination_head IS NULL OR (length(observed_current_destination_head) IN (40,64) AND observed_current_destination_head=lower(observed_current_destination_head) AND observed_current_destination_head NOT GLOB '*[^0-9a-f]*')),
                    quarantine_ref TEXT NOT NULL UNIQUE CHECK(substr(quarantine_ref,1,28)='refs/rsi/closure-quarantine/' AND length(quarantine_ref)>28),
                    quarantine_head TEXT NOT NULL CHECK(length(quarantine_head) IN (40,64) AND quarantine_head=lower(quarantine_head) AND quarantine_head NOT GLOB '*[^0-9a-f]*'),
                    reason TEXT NOT NULL CHECK(reason IN ('candidate_bad','policy_incompatible','forward_repair_required')),
                    rationale TEXT NOT NULL,
                    confirmation TEXT NOT NULL CHECK(confirmation='HUMAN-QUARANTINE'),
                    journal_phase TEXT NOT NULL CHECK(journal_phase IN ('reserved','quarantine_ref_pending','quarantine_ref_verified','receipt_appended','claim_released','complete','reconciliation_blocked')),
                    settlement_receipt_id TEXT CHECK(settlement_receipt_id IS NULL OR (length(settlement_receipt_id)=36 AND settlement_receipt_id=lower(settlement_receipt_id) AND substr(settlement_receipt_id,9,1)='-' AND substr(settlement_receipt_id,14,1)='-' AND substr(settlement_receipt_id,19,1)='-' AND substr(settlement_receipt_id,24,1)='-' AND length(replace(settlement_receipt_id,'-',''))=32 AND replace(settlement_receipt_id,'-','') NOT GLOB '*[^0-9a-f]*')) REFERENCES integration_receipts(id) ON DELETE RESTRICT,
                    destination_claim_released_at TEXT,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );

                CREATE TABLE closure_cleanup_proofs (
                    id TEXT PRIMARY KEY CHECK(length(id)=36 AND id=lower(id) AND substr(id,9,1)='-' AND substr(id,14,1)='-' AND substr(id,19,1)='-' AND substr(id,24,1)='-' AND length(replace(id,'-',''))=32 AND replace(id,'-','') NOT GLOB '*[^0-9a-f]*'),
                    source_id TEXT NOT NULL CHECK(length(source_id)=36 AND source_id=lower(source_id) AND substr(source_id,9,1)='-' AND substr(source_id,14,1)='-' AND substr(source_id,19,1)='-' AND substr(source_id,24,1)='-' AND length(replace(source_id,'-',''))=32 AND replace(source_id,'-','') NOT GLOB '*[^0-9a-f]*') REFERENCES closure_sources(id) ON DELETE RESTRICT,
                    proof_kind TEXT NOT NULL CHECK(proof_kind IN ('accepted_receipt','discard','quarantine_settlement')),
                    receipt_id TEXT CHECK(receipt_id IS NULL OR (length(receipt_id)=36 AND receipt_id=lower(receipt_id) AND substr(receipt_id,9,1)='-' AND substr(receipt_id,14,1)='-' AND substr(receipt_id,19,1)='-' AND substr(receipt_id,24,1)='-' AND length(replace(receipt_id,'-',''))=32 AND replace(receipt_id,'-','') NOT GLOB '*[^0-9a-f]*')) REFERENCES integration_receipts(id) ON DELETE RESTRICT,
                    settlement_id TEXT CHECK(settlement_id IS NULL OR (length(settlement_id)=36 AND settlement_id=lower(settlement_id) AND substr(settlement_id,9,1)='-' AND substr(settlement_id,14,1)='-' AND substr(settlement_id,19,1)='-' AND substr(settlement_id,24,1)='-' AND length(replace(settlement_id,'-',''))=32 AND replace(settlement_id,'-','') NOT GLOB '*[^0-9a-f]*')) REFERENCES closure_gate_failure_settlements(id) ON DELETE RESTRICT,
                    observed_destination_head TEXT CHECK(observed_destination_head IS NULL OR (length(observed_destination_head) IN (40,64) AND observed_destination_head=lower(observed_destination_head) AND observed_destination_head NOT GLOB '*[^0-9a-f]*')),
                    destination_reachable INTEGER NOT NULL CHECK(destination_reachable IN (0,1)),
                    observed_source_head TEXT NOT NULL CHECK(length(observed_source_head) IN (40,64) AND observed_source_head=lower(observed_source_head) AND observed_source_head NOT GLOB '*[^0-9a-f]*'),
                    observed_staging_head TEXT NOT NULL CHECK(length(observed_staging_head) IN (40,64) AND observed_staging_head=lower(observed_staging_head) AND observed_staging_head NOT GLOB '*[^0-9a-f]*'),
                    quarantine_ref TEXT CHECK(quarantine_ref IS NULL OR (substr(quarantine_ref,1,28)='refs/rsi/closure-quarantine/' AND length(quarantine_ref)>28)),
                    quarantine_head TEXT CHECK(quarantine_head IS NULL OR (length(quarantine_head) IN (40,64) AND quarantine_head=lower(quarantine_head) AND quarantine_head NOT GLOB '*[^0-9a-f]*')),
                    preview_id TEXT NOT NULL UNIQUE CHECK(length(preview_id)=36 AND preview_id=lower(preview_id) AND substr(preview_id,9,1)='-' AND substr(preview_id,14,1)='-' AND substr(preview_id,19,1)='-' AND substr(preview_id,24,1)='-' AND length(replace(preview_id,'-',''))=32 AND replace(preview_id,'-','') NOT GLOB '*[^0-9a-f]*'),
                    inventory_json TEXT NOT NULL,
                    inventory_digest TEXT NOT NULL CHECK(length(inventory_digest)=71 AND substr(inventory_digest,1,7)='sha256:' AND substr(inventory_digest,8)=lower(substr(inventory_digest,8)) AND substr(inventory_digest,8) NOT GLOB '*[^0-9a-f]*'),
                    expires_at TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    CHECK((proof_kind='accepted_receipt' AND receipt_id IS NOT NULL AND settlement_id IS NULL)
                       OR (proof_kind='discard' AND receipt_id IS NOT NULL AND settlement_id IS NULL)
                       OR (proof_kind='quarantine_settlement' AND receipt_id IS NOT NULL AND settlement_id IS NOT NULL AND quarantine_ref IS NOT NULL AND quarantine_head IS NOT NULL))
                );

                CREATE TABLE closure_cleanup_actions (
                    id TEXT PRIMARY KEY CHECK(length(id)=36 AND id=lower(id) AND substr(id,9,1)='-' AND substr(id,14,1)='-' AND substr(id,19,1)='-' AND substr(id,24,1)='-' AND length(replace(id,'-',''))=32 AND replace(id,'-','') NOT GLOB '*[^0-9a-f]*'),
                    source_id TEXT NOT NULL UNIQUE CHECK(length(source_id)=36 AND source_id=lower(source_id) AND substr(source_id,9,1)='-' AND substr(source_id,14,1)='-' AND substr(source_id,19,1)='-' AND substr(source_id,24,1)='-' AND length(replace(source_id,'-',''))=32 AND replace(source_id,'-','') NOT GLOB '*[^0-9a-f]*') REFERENCES closure_sources(id) ON DELETE RESTRICT,
                    proof_id TEXT NOT NULL UNIQUE CHECK(length(proof_id)=36 AND proof_id=lower(proof_id) AND substr(proof_id,9,1)='-' AND substr(proof_id,14,1)='-' AND substr(proof_id,19,1)='-' AND substr(proof_id,24,1)='-' AND length(replace(proof_id,'-',''))=32 AND replace(proof_id,'-','') NOT GLOB '*[^0-9a-f]*') REFERENCES closure_cleanup_proofs(id) ON DELETE RESTRICT,
                    preview_id TEXT NOT NULL CHECK(length(preview_id)=36 AND preview_id=lower(preview_id) AND substr(preview_id,9,1)='-' AND substr(preview_id,14,1)='-' AND substr(preview_id,19,1)='-' AND substr(preview_id,24,1)='-' AND length(replace(preview_id,'-',''))=32 AND replace(preview_id,'-','') NOT GLOB '*[^0-9a-f]*'),
                    preview_digest TEXT NOT NULL CHECK(length(preview_digest)=71 AND substr(preview_digest,1,7)='sha256:' AND substr(preview_digest,8)=lower(substr(preview_digest,8)) AND substr(preview_digest,8) NOT GLOB '*[^0-9a-f]*'),
                    confirmation TEXT NOT NULL CHECK(confirmation='HUMAN-CLEANUP'),
                    expected_custody_generation INTEGER NOT NULL CHECK(expected_custody_generation>0),
                    worktree_root TEXT NOT NULL,
                    source_ref TEXT NOT NULL CHECK(substr(source_ref,1,11)='refs/heads/' AND length(source_ref)>11),
                    source_expected_sha TEXT NOT NULL CHECK(length(source_expected_sha) IN (40,64) AND source_expected_sha=lower(source_expected_sha) AND source_expected_sha NOT GLOB '*[^0-9a-f]*'),
                    staging_ref TEXT NOT NULL CHECK(substr(staging_ref,1,29)='refs/heads/rsi/closure-stage/' AND length(staging_ref)>29),
                    staging_expected_sha TEXT NOT NULL CHECK(length(staging_expected_sha) IN (40,64) AND staging_expected_sha=lower(staging_expected_sha) AND staging_expected_sha NOT GLOB '*[^0-9a-f]*'),
                    session_ids_json TEXT NOT NULL,
                    phase TEXT NOT NULL CHECK(phase IN ('worktree_remove_pending','worktree_removed','source_ref_delete_pending','source_ref_deleted','staging_ref_delete_pending','staging_ref_deleted','custody_settlement_pending','cleanup_complete','retryable_failure','blocked_partial_worktree','blocked_partial_ref','blocked_partial_custody')),
                    partial_failure_json TEXT,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );

                CREATE TABLE closure_operator_requests (
                    id TEXT PRIMARY KEY CHECK(length(id)=36 AND id=lower(id) AND substr(id,9,1)='-' AND substr(id,14,1)='-' AND substr(id,19,1)='-' AND substr(id,24,1)='-' AND length(replace(id,'-',''))=32 AND replace(id,'-','') NOT GLOB '*[^0-9a-f]*'),
                    method TEXT NOT NULL CHECK(method IN ('CreateClosureProgram','UpdateClosureProgram','LaunchClosureSource','RecordClosureEvidence','ResumeClosureFinalization','ApproveClosurePromotion','RecheckClosureFinalGate','SupersedeClosureGateFailure','RecordClosureDiscard','ExecuteClosureCleanup')),
                    idempotency_key TEXT NOT NULL CHECK(length(idempotency_key)=36 AND idempotency_key=lower(idempotency_key) AND substr(idempotency_key,9,1)='-' AND substr(idempotency_key,14,1)='-' AND substr(idempotency_key,19,1)='-' AND substr(idempotency_key,24,1)='-' AND length(replace(idempotency_key,'-',''))=32 AND replace(idempotency_key,'-','') NOT GLOB '*[^0-9a-f]*'),
                    request_fingerprint TEXT NOT NULL CHECK(length(request_fingerprint)=71 AND substr(request_fingerprint,1,7)='sha256:' AND substr(request_fingerprint,8)=lower(substr(request_fingerprint,8)) AND substr(request_fingerprint,8) NOT GLOB '*[^0-9a-f]*'),
                    request_json TEXT NOT NULL,
                    result_json TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    UNIQUE(method,idempotency_key)
                );

                CREATE TABLE closure_events (
                    id TEXT PRIMARY KEY CHECK(length(id)=36 AND id=lower(id) AND substr(id,9,1)='-' AND substr(id,14,1)='-' AND substr(id,19,1)='-' AND substr(id,24,1)='-' AND length(replace(id,'-',''))=32 AND replace(id,'-','') NOT GLOB '*[^0-9a-f]*'),
                    program_id TEXT NOT NULL CHECK(length(program_id)=36 AND program_id=lower(program_id) AND substr(program_id,9,1)='-' AND substr(program_id,14,1)='-' AND substr(program_id,19,1)='-' AND substr(program_id,24,1)='-' AND length(replace(program_id,'-',''))=32 AND replace(program_id,'-','') NOT GLOB '*[^0-9a-f]*') REFERENCES closure_programs(id) ON DELETE RESTRICT,
                    source_id TEXT CHECK(source_id IS NULL OR (length(source_id)=36 AND source_id=lower(source_id) AND substr(source_id,9,1)='-' AND substr(source_id,14,1)='-' AND substr(source_id,19,1)='-' AND substr(source_id,24,1)='-' AND length(replace(source_id,'-',''))=32 AND replace(source_id,'-','') NOT GLOB '*[^0-9a-f]*')) REFERENCES closure_sources(id) ON DELETE RESTRICT,
                    sequence INTEGER NOT NULL CHECK(sequence>0),
                    event_kind TEXT NOT NULL,
                    prior_program_state TEXT,
                    next_program_state TEXT NOT NULL,
                    prior_source_state TEXT,
                    next_source_state TEXT,
                    correlation_key TEXT CHECK(correlation_key IS NULL OR length(correlation_key)>0),
                    payload_json TEXT NOT NULL,
                    occurred_at TEXT NOT NULL,
                    UNIQUE(program_id,sequence),
                    UNIQUE(source_id,event_kind,correlation_key)
                );

                CREATE TRIGGER closure_programs_identity_immutable BEFORE UPDATE ON closure_programs
                WHEN (NEW.id!=OLD.id OR NEW.repository_root!=OLD.repository_root OR NEW.repository_identity!=OLD.repository_identity OR NEW.base_ref!=OLD.base_ref OR NEW.base_sha!=OLD.base_sha OR NEW.destination_ref!=OLD.destination_ref OR NEW.destination_pre_head!=OLD.destination_pre_head OR NEW.creation_idempotency_key!=OLD.creation_idempotency_key OR NEW.creation_request_fingerprint!=OLD.creation_request_fingerprint OR NEW.created_at!=OLD.created_at)
                 AND EXISTS (SELECT 1 FROM closure_sources WHERE program_id=OLD.id)
                BEGIN SELECT RAISE(ABORT,'Closure program identity is immutable'); END;
                CREATE TRIGGER closure_destination_release_guard BEFORE UPDATE OF destination_claim_state ON closure_programs
                WHEN OLD.destination_claim_state='held' AND NEW.destination_claim_state='released'
                 AND NOT (
                    (NEW.state='closed' AND EXISTS (
                        SELECT 1 FROM closure_sources s
                        JOIN closure_cleanup_actions a ON a.source_id=s.id
                        WHERE s.program_id=OLD.id AND a.phase='cleanup_complete'
                    ))
                    OR
                    (NEW.state IN ('quarantined_cleanup_pending','quarantined_closed') AND EXISTS (
                        SELECT 1 FROM closure_sources s
                        JOIN closure_gate_failure_settlements q ON q.source_id=s.id
                        JOIN integration_receipts r ON r.id=q.settlement_receipt_id
                        WHERE s.program_id=OLD.id
                          AND q.journal_phase IN ('claim_released','complete')
                          AND q.destination_claim_released_at IS NOT NULL
                          AND r.receipt_kind='quarantined_for_forward_repair'
                          AND r.source_id=s.id
                    ))
                 )
                BEGIN SELECT RAISE(ABORT,'Closure destination release requires cleanup or quarantine settlement'); END;
                CREATE TRIGGER closure_destination_claim_no_reacquire BEFORE UPDATE OF destination_claim_state ON closure_programs
                WHEN OLD.destination_claim_state='released' AND NEW.destination_claim_state='held'
                BEGIN SELECT RAISE(ABORT,'Closure destination claims cannot be reacquired'); END;
                CREATE TRIGGER closure_sources_identity_immutable BEFORE UPDATE ON closure_sources
                WHEN NEW.id!=OLD.id OR NEW.program_id!=OLD.program_id OR NEW.custody_id!=OLD.custody_id OR NEW.root_session_id!=OLD.root_session_id OR NEW.source_ref!=OLD.source_ref OR NEW.source_base_sha!=OLD.source_base_sha OR NEW.destination_ref!=OLD.destination_ref OR NEW.destination_pre_head!=OLD.destination_pre_head OR NEW.staging_ref!=OLD.staging_ref OR NEW.created_at!=OLD.created_at
                BEGIN SELECT RAISE(ABORT,'Closure source identity is immutable'); END;
                CREATE TRIGGER closure_source_launch_reservations_identity_immutable BEFORE UPDATE ON closure_source_launch_reservations
                WHEN NEW.id!=OLD.id OR NEW.program_id!=OLD.program_id OR NEW.idempotency_key!=OLD.idempotency_key OR NEW.request_fingerprint!=OLD.request_fingerprint OR NEW.request_json!=OLD.request_json OR NEW.source_id!=OLD.source_id OR NEW.root_session_id!=OLD.root_session_id OR NEW.custody_id!=OLD.custody_id OR NEW.custody_generation!=OLD.custody_generation OR NEW.branch_name!=OLD.branch_name OR NEW.repository_identity!=OLD.repository_identity OR NEW.source_ref!=OLD.source_ref OR NEW.source_base_sha!=OLD.source_base_sha OR NEW.destination_ref!=OLD.destination_ref OR NEW.destination_pre_head!=OLD.destination_pre_head OR NEW.staging_ref!=OLD.staging_ref OR NEW.created_at!=OLD.created_at OR OLD.completed_at IS NOT NULL OR NEW.completed_at IS NULL
                BEGIN SELECT RAISE(ABORT,'Closure source launch reservation identity is immutable'); END;
                CREATE TRIGGER closure_source_launch_reservations_no_delete BEFORE DELETE ON closure_source_launch_reservations BEGIN SELECT RAISE(ABORT,'Closure source launch reservations are retained'); END;
                CREATE TRIGGER closure_sources_custody_generation_forward BEFORE UPDATE OF custody_generation ON closure_sources
                WHEN NEW.custody_generation!=OLD.custody_generation
                 AND (NEW.custody_generation!=OLD.custody_generation+1 OR NOT EXISTS (
                    SELECT 1 FROM closure_source_sessions css
                    WHERE css.source_id=OLD.id AND css.custody_id=OLD.custody_id
                      AND css.custody_generation=NEW.custody_generation
                 ))
                BEGIN SELECT RAISE(ABORT,'Closure source custody generation requires the next immutable rotation binding'); END;

                CREATE TRIGGER closure_source_sessions_no_update BEFORE UPDATE ON closure_source_sessions BEGIN SELECT RAISE(ABORT,'Closure source session bindings are immutable'); END;
                CREATE TRIGGER closure_source_sessions_no_delete BEFORE DELETE ON closure_source_sessions BEGIN SELECT RAISE(ABORT,'Closure source session bindings are retained'); END;
                CREATE TRIGGER conversation_event_provenance_no_update BEFORE UPDATE ON conversation_event_provenance BEGIN SELECT RAISE(ABORT,'conversation event provenance is immutable'); END;
                CREATE TRIGGER conversation_event_provenance_no_delete BEFORE DELETE ON conversation_event_provenance BEGIN SELECT RAISE(ABORT,'conversation event provenance is retained'); END;
                CREATE TRIGGER closure_output_validations_no_update BEFORE UPDATE ON closure_output_validations BEGIN SELECT RAISE(ABORT,'Closure output validations are immutable'); END;
                CREATE TRIGGER closure_output_validations_no_delete BEFORE DELETE ON closure_output_validations BEGIN SELECT RAISE(ABORT,'Closure output validations are retained'); END;
                CREATE TRIGGER closure_evidence_no_update BEFORE UPDATE ON closure_evidence BEGIN SELECT RAISE(ABORT,'Closure evidence is immutable'); END;
                CREATE TRIGGER closure_evidence_no_delete BEFORE DELETE ON closure_evidence BEGIN SELECT RAISE(ABORT,'Closure evidence is retained'); END;
                CREATE TRIGGER integration_receipts_no_update BEFORE UPDATE ON integration_receipts BEGIN SELECT RAISE(ABORT,'Closure receipts are immutable'); END;
                CREATE TRIGGER integration_receipts_no_delete BEFORE DELETE ON integration_receipts BEGIN SELECT RAISE(ABORT,'Closure receipts are retained'); END;
                CREATE TRIGGER closure_receipt_consumptions_no_update BEFORE UPDATE ON closure_receipt_consumptions BEGIN SELECT RAISE(ABORT,'Closure receipt consumptions are immutable'); END;
                CREATE TRIGGER closure_receipt_consumptions_no_delete BEFORE DELETE ON closure_receipt_consumptions BEGIN SELECT RAISE(ABORT,'Closure receipt consumptions are retained'); END;
                CREATE TRIGGER closure_final_gate_attempt_receipts_no_update BEFORE UPDATE ON closure_final_gate_attempt_receipts BEGIN SELECT RAISE(ABORT,'Closure gate receipts are immutable'); END;
                CREATE TRIGGER closure_final_gate_attempt_receipts_no_delete BEFORE DELETE ON closure_final_gate_attempt_receipts BEGIN SELECT RAISE(ABORT,'Closure gate receipts are retained'); END;
                CREATE TRIGGER closure_cleanup_proofs_no_update BEFORE UPDATE ON closure_cleanup_proofs BEGIN SELECT RAISE(ABORT,'Closure cleanup proofs are immutable'); END;
                CREATE TRIGGER closure_cleanup_proofs_no_delete BEFORE DELETE ON closure_cleanup_proofs BEGIN SELECT RAISE(ABORT,'Closure cleanup proofs are retained'); END;
                CREATE TRIGGER closure_operator_requests_no_update BEFORE UPDATE ON closure_operator_requests BEGIN SELECT RAISE(ABORT,'Closure operator request receipts are immutable'); END;
                CREATE TRIGGER closure_operator_requests_no_delete BEFORE DELETE ON closure_operator_requests BEGIN SELECT RAISE(ABORT,'Closure operator request receipts are retained'); END;
                CREATE TRIGGER closure_events_no_update BEFORE UPDATE ON closure_events BEGIN SELECT RAISE(ABORT,'Closure events are immutable'); END;
                CREATE TRIGGER closure_events_no_delete BEFORE DELETE ON closure_events BEGIN SELECT RAISE(ABORT,'Closure events are retained'); END;
                ",
            )?;
            let foreign_key_errors: i64 =
                tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                    row.get(0)
                })?;
            if foreign_key_errors != 0 {
                return Err(DaemonError::Store(
                    "V84 Closure catalog foreign-key check failed".into(),
                ));
            }
            tx.execute("PRAGMA user_version = 84", [])?;
            tx.commit()?;
            tracing::info!("V84 migration complete: Closure Kernel catalog and event provenance");
        }

        if stop_after_v84 {
            debug_assert_eq!(
                self.conn
                    .query_row("PRAGMA user_version", [], |row| row.get::<_, i32>(0))
                    .unwrap_or_default(),
                84
            );
            return Ok(());
        }

        if version < 85 {
            // V85 is intentionally private to the daemon.  The two sessions
            // columns are transaction fences, not part of the Session wire
            // contract; all durable protocol state lives in the catalog below.
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            if active_version != 84 {
                return Err(DaemonError::Store(format!(
                    "V85 requires exact V84 source, found V{active_version}"
                )));
            }
            origin_authority::validate_v84_catalog(&tx)?;
            h1_v85_migration_fault(H1V85MigrationFault::AfterPreflight)?;
            tx.execute_batch(&origin_authority::v85_table_sql())?;
            h1_v85_migration_fault(H1V85MigrationFault::AfterTables)?;
            origin_authority::seed_v84_authorities(&tx)?;
            h1_v85_migration_fault(H1V85MigrationFault::AfterBackfill)?;
            tx.execute_batch(origin_authority::V85_INDEX_SQL)?;
            h1_v85_migration_fault(H1V85MigrationFault::AfterIndexes)?;
            tx.execute_batch(origin_authority::V85_TRIGGER_SQL)?;
            h1_v85_migration_fault(H1V85MigrationFault::AfterTriggers)?;
            let foreign_key_errors: i64 =
                tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                    row.get(0)
                })?;
            if foreign_key_errors != 0 {
                return Err(DaemonError::Store(
                    "V85 execution-origin catalog foreign-key check failed".into(),
                ));
            }
            h1_v85_migration_fault(H1V85MigrationFault::AfterForeignKeyCheck)?;
            tx.execute("PRAGMA user_version = 85", [])?;
            h1_v85_migration_fault(H1V85MigrationFault::AfterUserVersion)?;
            h1_v85_migration_fault(H1V85MigrationFault::BeforeCommit)?;
            tx.commit()?;
            tracing::info!("V85 migration complete: execution-origin catalog foundation");
        }

        if version < 86 {
            // V86 is a real upgrade, not an amendment to the V85 creation
            // block.  A V85 database can already contain immutable origin
            // history, so rebuild the five mutually-referencing relations in
            // one deferred-FK transaction and copy every column byte-for-byte.
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            if active_version != 85 {
                return Err(DaemonError::Store(format!(
                    "V86 requires exact V85 source, found V{active_version}"
                )));
            }
            h1_v86_migration_fault(H1V86MigrationFault::AfterPreflight)?;
            tx.execute_batch("PRAGMA defer_foreign_keys=ON;")?;
            tx.execute_batch(origin_authority::V86_REBUILD_RENAME_SQL)?;
            h1_v86_migration_fault(H1V86MigrationFault::AfterRename)?;
            tx.execute_batch(&origin_authority::v86_catalog_table_sql())?;
            tx.execute_batch(origin_authority::V86_REBUILD_COPY_AND_DROP_SQL)?;
            h1_v86_migration_fault(H1V86MigrationFault::AfterCopy)?;
            tx.execute_batch(origin_authority::V85_INDEX_SQL)?;
            h1_v86_migration_fault(H1V86MigrationFault::AfterIndexes)?;
            tx.execute_batch(origin_authority::V85_TRIGGER_SQL)?;
            tx.execute_batch(origin_authority::V86_TRIGGER_SQL)?;
            h1_v86_migration_fault(H1V86MigrationFault::AfterTriggers)?;
            let foreign_key_errors: i64 =
                tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                    row.get(0)
                })?;
            if foreign_key_errors != 0 {
                return Err(DaemonError::Store(
                    "V86 execution-origin catalog foreign-key check failed".into(),
                ));
            }
            h1_v86_migration_fault(H1V86MigrationFault::AfterForeignKeyCheck)?;
            tx.execute("PRAGMA user_version = 86", [])?;
            h1_v86_migration_fault(H1V86MigrationFault::AfterUserVersion)?;
            h1_v86_migration_fault(H1V86MigrationFault::BeforeCommit)?;
            tx.commit()?;
            tracing::info!("V86 migration complete: request-key provenance rebuild");
        }

        if version < 87 {
            // V87 is a trigger-only replacement over the exact deployed V86
            // catalog. Historical V85/V86 reconstruction stays byte-stable.
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            if active_version != 86 {
                return Err(DaemonError::Store(format!(
                    "V87 requires exact V86 source, found V{active_version}"
                )));
            }
            origin_authority::validate_v86_session_trigger(&tx)?;
            h1_v87_migration_fault(H1V87MigrationFault::AfterPreflight)?;
            tx.execute_batch("DROP TRIGGER sessions_execution_origin_write_guard;")?;
            h1_v87_migration_fault(H1V87MigrationFault::AfterDrop)?;
            tx.execute_batch(origin_authority::V87_SESSION_TRIGGER_SQL)?;
            h1_v87_migration_fault(H1V87MigrationFault::AfterCreate)?;
            let integrity: String = tx.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
            if integrity != "ok" {
                return Err(DaemonError::Store(format!(
                    "V87 requires integrity_check=ok, got {integrity}"
                )));
            }
            let foreign_key_errors: i64 =
                tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                    row.get(0)
                })?;
            if foreign_key_errors != 0 {
                return Err(DaemonError::Store(
                    "V87 Session-fence catalog foreign-key check failed".into(),
                ));
            }
            h1_v87_migration_fault(H1V87MigrationFault::AfterChecks)?;
            tx.execute("PRAGMA user_version = 87", [])?;
            h1_v87_migration_fault(H1V87MigrationFault::AfterUserVersion)?;
            h1_v87_migration_fault(H1V87MigrationFault::BeforeCommit)?;
            tx.commit()?;
            tracing::info!("V87 migration complete: Session fence replacement");
        }

        if version < 88 {
            // V88 changes claim CHECKs and the general claim/authority state
            // guards. Authenticate the complete deployed V87 origin catalog,
            // then rebuild all five mutually-referencing relations without
            // rewriting any historical V85/V86/V87 literal.
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            if active_version != 87 {
                return Err(DaemonError::Store(format!(
                    "V88 requires exact V87 source, found V{active_version}"
                )));
            }
            origin_authority::validate_v87_catalog(&tx)?;
            let source_integrity: String =
                tx.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
            if source_integrity != "ok" {
                return Err(DaemonError::Store(format!(
                    "V88 requires integrity_check=ok for exact V87 source, got {source_integrity}"
                )));
            }
            let source_foreign_key_errors: i64 =
                tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                    row.get(0)
                })?;
            if source_foreign_key_errors != 0 {
                return Err(DaemonError::Store(format!(
                    "V88 requires clean V87 foreign keys, found {source_foreign_key_errors} violation(s)"
                )));
            }
            let incoherent_session_bindings: i64 = tx.query_row(
                "SELECT count(*)
                 FROM sessions s
                 WHERE s.execution_origin_claim_id IS NOT NULL
                   AND NOT (
                     EXISTS (
                       SELECT 1
                       FROM execution_origin_claims c
                       JOIN execution_origin_authorities a
                         ON a.authority_kind=c.authority_kind
                        AND a.authority_uuid=c.authority_uuid
                       JOIN execution_origin_events e
                         ON e.authority_kind=a.authority_kind
                        AND e.authority_uuid=a.authority_uuid
                        AND e.sequence=a.event_sequence
                       WHERE c.claim_id=s.execution_origin_claim_id
                         AND c.claimant_session_id=s.id
                         AND c.phase IN ('claimed','launch_ready','launching','provider_live','recovery_quarantined','settling','settled','failed','abandoned','quarantined')
                         AND a.owner_session_id=s.id
                         AND a.active_claim_id=c.claim_id
                         AND a.phase=CASE
                           WHEN c.phase='recovery_quarantined' THEN 'quarantined'
                           WHEN c.phase IN ('settled','failed','abandoned','quarantined') THEN 'settling'
                           ELSE c.phase
                         END
                         AND a.claim_generation=c.expected_claim_generation+1
                         AND a.owner_generation=c.expected_owner_generation+
                           CASE WHEN c.claimant_kind IN ('agent_fresh','rotation','automatic_retry') THEN 1 ELSE 0 END
                         AND a.boot_id=c.boot_id
                         AND e.claim_id=c.claim_id
                         AND e.to_phase=a.phase
                         AND e.to_owner_generation=a.owner_generation
                         AND e.provider_absence_evidence IS c.absence_evidence
                         AND e.event_kind=CASE a.phase
                           WHEN 'claimed' THEN 'claimed'
                           WHEN 'launch_ready' THEN 'launch_ready'
                           WHEN 'launching' THEN 'launching'
                           WHEN 'provider_live' THEN 'provider_live'
                           WHEN 'quarantined' THEN 'quarantined'
                           WHEN 'settling' THEN 'settlement_prepared'
                         END
                         AND e.from_owner_generation=CASE
                           WHEN a.phase='claimed' THEN c.expected_owner_generation
                           ELSE a.owner_generation
                         END
                         AND (
                           (a.phase='claimed' AND e.from_phase='idle')
                           OR (a.phase='launch_ready' AND e.from_phase='claimed')
                           OR (a.phase='launching' AND e.from_phase='launch_ready')
                           OR (a.phase='provider_live' AND e.from_phase='launching')
                           OR (a.phase='quarantined' AND e.from_phase IN ('claimed','launch_ready','launching','provider_live'))
                           OR (a.phase='settling' AND e.from_phase IN ('claimed','launch_ready','launching','provider_live','quarantined'))
                         )
                         AND (
                           (s.status=c.authorized_session_status
                            AND s.execution_origin_write_seq=c.authorized_session_write_seq)
                           OR
                           (c.phase='provider_live'
                            AND c.authorized_session_status IN ('Running','WaitingApproval')
                            AND c.authorized_session_write_seq=s.execution_origin_write_seq+1
                            AND s.status IN ('Starting','Running','WaitingApproval'))
                         )
                         AND (
                           (a.authority_kind='ordinary' AND s.sandbox_custody_id IS NULL)
                           OR
                           (a.authority_kind='sandbox'
                            AND s.sandbox_custody_id=a.authority_uuid
                            AND EXISTS (
                              SELECT 1
                              FROM sandbox_custody_roots root
                              WHERE root.custody_id=a.authority_uuid
                                AND root.owner_session_id=s.id
                                AND root.generation=a.owner_generation
                                AND root.state='live'
                                AND root.validation_state='verified'
                                AND root.validated_generation=root.generation
                                AND root.effect_boot_id=c.boot_id
                                AND root.reserved_effects=0
                                AND root.active_effects=1
                            ))
                         )
                     )
                     OR EXISTS (
                       SELECT 1
                       FROM execution_origin_claims c
                       JOIN execution_origin_authorities a
                         ON a.authority_kind=c.authority_kind
                        AND a.authority_uuid=c.authority_uuid
                       JOIN execution_origin_receipts rcp
                         ON rcp.claim_id=c.claim_id
                        AND rcp.request_key=c.request_key
                        AND rcp.authority_kind=c.authority_kind
                        AND rcp.authority_uuid=c.authority_uuid
                        AND rcp.requested_origin_session_id=c.requested_origin_session_id
                        AND rcp.source_session_id IS c.source_session_id
                        AND rcp.claimant_session_id IS c.claimant_session_id
                        AND rcp.scheduled_job_id IS c.scheduled_job_id
                        AND rcp.scheduled_fire_at IS c.scheduled_fire_at
                        AND rcp.outcome=c.phase
                       JOIN execution_origin_events settled
                         ON settled.authority_kind=a.authority_kind
                        AND settled.authority_uuid=a.authority_uuid
                        AND settled.sequence=a.event_sequence
                       JOIN execution_origin_events prepared
                         ON prepared.authority_kind=a.authority_kind
                        AND prepared.authority_uuid=a.authority_uuid
                        AND prepared.sequence=a.event_sequence-1
                       JOIN model_invocations terminal
                         ON terminal.id=c.terminal_model_invocation_id
                        AND terminal.session_id=s.id
                        AND terminal.status IN ('completed','failed','cancelled','denied')
                       WHERE c.claim_id=s.execution_origin_claim_id
                         AND c.claimant_session_id=s.id
                         AND c.phase IN ('settled','failed','abandoned','quarantined')
                         AND ((c.phase='settled' AND c.prepared_terminal_status='Completed')
                           OR (c.phase='failed' AND c.prepared_terminal_status='Failed')
                           OR (c.phase IN ('abandoned','quarantined') AND c.prepared_terminal_status='Interrupted'))
                         AND c.prepared_stop_reason IS NOT NULL
                         AND c.prepared_session_write_seq=s.execution_origin_write_seq+1
                         AND c.prepared_provider_evidence IS NOT NULL
                         AND c.authorized_session_status=s.status
                         AND c.authorized_session_write_seq=s.execution_origin_write_seq
                         AND a.owner_session_id=s.id
                         AND a.active_claim_id IS NULL
                         AND a.phase='idle'
                         AND a.claim_generation=c.expected_claim_generation+1
                         AND a.owner_generation=c.expected_owner_generation+
                           CASE WHEN c.claimant_kind IN ('agent_fresh','rotation','automatic_retry') THEN 1 ELSE 0 END
                         AND a.boot_id IS NULL
                         AND settled.claim_id=c.claim_id
                         AND settled.from_phase='settling'
                         AND settled.to_phase='idle'
                         AND settled.from_owner_generation=a.owner_generation
                         AND settled.to_owner_generation=a.owner_generation
                         AND settled.provider_absence_evidence=c.prepared_provider_evidence
                         AND settled.event_kind='settled'
                         AND prepared.claim_id=c.claim_id
                         AND prepared.to_phase='settling'
                         AND prepared.to_owner_generation=a.owner_generation
                         AND prepared.provider_absence_evidence=c.prepared_provider_evidence
                         AND prepared.event_kind='settlement_prepared'
                         AND (c.prepared_c5_key IS NULL OR EXISTS (
                           SELECT 1
                           FROM daemon_settings ds
                           WHERE ds.key=c.prepared_c5_key
                             AND ds.value=c.prepared_c5_value
                             AND ds.updated_at=c.prepared_c5_at
                         ))
                         AND (
                           (a.authority_kind='ordinary' AND s.sandbox_custody_id IS NULL)
                           OR
                           (a.authority_kind='sandbox'
                            AND s.sandbox_custody_id=a.authority_uuid
                            AND EXISTS (
                              SELECT 1
                              FROM sandbox_custody_roots root
                              WHERE root.custody_id=a.authority_uuid
                                AND root.owner_session_id=s.id
                                AND root.generation=a.owner_generation
                                AND root.state='live'
                                AND root.validation_state='verified'
                                AND root.validated_generation=root.generation
                                AND root.effect_boot_id IS NULL
                                AND root.reserved_effects=0
                                AND root.active_effects=0
                            ))
                         )
                     )
                   )",
                [],
                |row| row.get(0),
            )?;
            let incoherent_active_authorities: i64 = tx.query_row(
                "SELECT count(*)
                 FROM execution_origin_authorities a
                 WHERE a.active_claim_id IS NOT NULL
                   AND NOT EXISTS (
                     SELECT 1
                     FROM execution_origin_claims c
                     WHERE c.claim_id=a.active_claim_id
                       AND c.authority_kind=a.authority_kind
                       AND c.authority_uuid=a.authority_uuid
                       AND c.claimant_session_id=a.owner_session_id
                       AND c.expected_owner_generation
                           + CASE WHEN c.claimant_kind IN ('agent_fresh','rotation','automatic_retry') THEN 1 ELSE 0 END
                           = a.owner_generation
                       AND c.expected_claim_generation+1=a.claim_generation
                       AND c.boot_id=a.boot_id
                       AND a.phase=CASE
                           WHEN c.phase='recovery_quarantined' THEN 'quarantined'
                           WHEN c.phase IN ('settled','failed','abandoned','quarantined') THEN 'settling'
                           ELSE c.phase
                       END
                       AND c.phase IN ('claimed','launch_ready','launching','provider_live','recovery_quarantined','settling','settled','failed','abandoned','quarantined')
                   )",
                [],
                |row| row.get(0),
            )?;
            let incoherent_active_predecessors: i64 = tx.query_row(
                "SELECT count(*)
                 FROM execution_origin_authorities a
                 WHERE a.active_claim_id IS NOT NULL
                   AND NOT EXISTS (
                     SELECT 1
                     FROM execution_origin_claims c
                     JOIN sessions s
                       ON s.id=a.owner_session_id
                     JOIN execution_origin_events e
                       ON e.authority_kind=a.authority_kind
                      AND e.authority_uuid=a.authority_uuid
                      AND e.sequence=a.event_sequence
                     WHERE c.claim_id=a.active_claim_id
                       AND c.authority_kind=a.authority_kind
                       AND c.authority_uuid=a.authority_uuid
                       AND c.claimant_session_id=a.owner_session_id
                       AND c.expected_owner_generation
                           + CASE WHEN c.claimant_kind IN ('agent_fresh','rotation','automatic_retry') THEN 1 ELSE 0 END
                           = a.owner_generation
                       AND c.expected_claim_generation+1=a.claim_generation
                       AND c.boot_id=a.boot_id
                       AND a.phase=CASE
                           WHEN c.phase='recovery_quarantined' THEN 'quarantined'
                           WHEN c.phase IN ('settled','failed','abandoned','quarantined') THEN 'settling'
                           ELSE c.phase
                       END
                       AND c.phase IN ('claimed','launch_ready','launching','provider_live','recovery_quarantined','settling','settled','failed','abandoned','quarantined')
                       AND e.claim_id=c.claim_id
                       AND e.to_phase=a.phase
                       AND e.to_owner_generation=a.owner_generation
                       AND e.provider_absence_evidence IS c.absence_evidence
                       AND e.event_kind=CASE a.phase
                           WHEN 'claimed' THEN 'claimed'
                           WHEN 'launch_ready' THEN 'launch_ready'
                           WHEN 'launching' THEN 'launching'
                           WHEN 'provider_live' THEN 'provider_live'
                           WHEN 'quarantined' THEN 'quarantined'
                           WHEN 'settling' THEN 'settlement_prepared'
                       END
                       AND e.from_owner_generation=CASE
                           WHEN a.phase='claimed' THEN c.expected_owner_generation
                           ELSE a.owner_generation
                       END
                       AND (
                           (a.phase='claimed' AND e.from_phase='idle')
                           OR (a.phase='launch_ready' AND e.from_phase='claimed')
                           OR (a.phase='launching' AND e.from_phase='launch_ready')
                           OR (a.phase='provider_live' AND e.from_phase='launching')
                           OR (a.phase='quarantined' AND e.from_phase IN ('claimed','launch_ready','launching','provider_live'))
                           OR (a.phase='settling' AND e.from_phase IN ('claimed','launch_ready','launching','provider_live','quarantined'))
                       )
                       AND (
                           (s.execution_origin_claim_id IS NULL
                            AND s.status='Starting'
                            AND c.phase='claimed'
                            AND c.authorized_session_status='Starting'
                            AND c.authorized_session_write_seq=s.execution_origin_write_seq+1)
                           OR
                           (s.execution_origin_claim_id=c.claim_id
                            AND (
                                (s.status=c.authorized_session_status
                                 AND s.execution_origin_write_seq=c.authorized_session_write_seq)
                                OR
                                (c.phase='provider_live'
                                 AND c.authorized_session_status IN ('Running','WaitingApproval')
                                 AND c.authorized_session_write_seq=s.execution_origin_write_seq+1
                                 AND s.status IN ('Starting','Running','WaitingApproval'))
                            ))
                       )
                       AND (
                           (a.authority_kind='ordinary'
                            AND s.sandbox_custody_id IS NULL
                            AND s.sandbox_kind IS NULL
                            AND s.sandbox_root IS NULL
                            AND s.sandbox_branch IS NULL
                            AND s.sandbox_cleanup_state IS NULL)
                           OR
                           (a.authority_kind='sandbox'
                            AND s.sandbox_custody_id=a.authority_uuid
                            AND s.sandbox_kind='GitWorktree'
                            AND s.sandbox_cleanup_state='Live'
                            AND EXISTS (
                              SELECT 1
                              FROM sandbox_custody_roots root
                              WHERE root.custody_id=a.authority_uuid
                                AND root.owner_session_id=s.id
                                AND root.sandbox_root=s.sandbox_root
                                AND root.sandbox_branch=s.sandbox_branch
                                AND root.generation=a.owner_generation
                                AND root.state='live'
                                AND root.validation_state='verified'
                                AND root.validated_generation=root.generation
                                AND root.effect_boot_id=c.boot_id
                                AND root.reserved_effects=0
                                AND root.active_effects=1
                            ))
                       )
                   )",
                [],
                |row| row.get(0),
            )?;
            if incoherent_active_authorities != 0 {
                return Err(DaemonError::Store(format!(
                    "V88 cannot classify {incoherent_active_authorities} active V87 authority/claim binding(s)"
                )));
            }
            // Preserve the fourth-repair diagnostic precedence: a malformed
            // non-null Session binding is owned by its sealed classifier once
            // the legacy authority/claim tuple itself is coherent.
            if incoherent_session_bindings != 0 {
                return Err(DaemonError::Store(format!(
                    "V88 cannot classify {incoherent_session_bindings} V87 Session origin binding(s)"
                )));
            }
            // The complementary scan owns only the remaining active-authority
            // negative space, including the exact unbound pre-bind shape.
            if incoherent_active_predecessors != 0 {
                return Err(DaemonError::Store(format!(
                    "V88 cannot classify {incoherent_active_predecessors} active V87 authority/claim binding(s)"
                )));
            }
            h1_validate_exact_active_predecessors(&tx, 88, 87)?;
            let source_relation_counts = [
                tx.query_row(
                    "SELECT count(*) FROM execution_origin_authorities",
                    [],
                    |row| row.get::<_, i64>(0),
                )?,
                tx.query_row("SELECT count(*) FROM execution_origin_claims", [], |row| {
                    row.get::<_, i64>(0)
                })?,
                tx.query_row("SELECT count(*) FROM execution_origin_members", [], |row| {
                    row.get::<_, i64>(0)
                })?,
                tx.query_row("SELECT count(*) FROM execution_origin_events", [], |row| {
                    row.get::<_, i64>(0)
                })?,
                tx.query_row(
                    "SELECT count(*) FROM execution_origin_receipts",
                    [],
                    |row| row.get::<_, i64>(0),
                )?,
            ];
            h1_v88_migration_fault(H1V88MigrationFault::AfterPreflight)?;
            tx.execute_batch("PRAGMA defer_foreign_keys=ON;")?;
            tx.execute_batch(origin_authority::V88_REBUILD_RENAME_SQL)?;
            h1_v88_migration_fault(H1V88MigrationFault::AfterRename)?;
            tx.execute_batch(&origin_authority::v88_catalog_table_sql())?;
            tx.execute_batch(origin_authority::V88_REBUILD_UPGRADE_COPY_SQL)?;
            let destination_relation_counts = [
                tx.query_row(
                    "SELECT count(*) FROM execution_origin_authorities",
                    [],
                    |row| row.get::<_, i64>(0),
                )?,
                tx.query_row("SELECT count(*) FROM execution_origin_claims", [], |row| {
                    row.get::<_, i64>(0)
                })?,
                tx.query_row("SELECT count(*) FROM execution_origin_members", [], |row| {
                    row.get::<_, i64>(0)
                })?,
                tx.query_row("SELECT count(*) FROM execution_origin_events", [], |row| {
                    row.get::<_, i64>(0)
                })?,
                tx.query_row(
                    "SELECT count(*) FROM execution_origin_receipts",
                    [],
                    |row| row.get::<_, i64>(0),
                )?,
            ];
            if destination_relation_counts != source_relation_counts {
                return Err(DaemonError::Store(format!(
                    "V88 five-relation copy count mismatch: source={source_relation_counts:?}, destination={destination_relation_counts:?}"
                )));
            }
            tx.execute_batch(origin_authority::V88_REBUILD_DROP_V87_SQL)?;
            h1_v88_migration_fault(H1V88MigrationFault::AfterCopy)?;
            tx.execute_batch(origin_authority::V85_INDEX_SQL)?;
            h1_v88_migration_fault(H1V88MigrationFault::AfterIndexes)?;
            tx.execute_batch(origin_authority::V85_TRIGGER_SQL)?;
            tx.execute_batch(
                "DROP TRIGGER execution_origin_authority_transition;
                 DROP TRIGGER execution_origin_claim_rank;
                 DROP TRIGGER execution_origin_claim_prepared_immutable;
                 DROP TRIGGER sessions_execution_origin_write_guard;",
            )?;
            tx.execute_batch(origin_authority::V88_STATE_TRIGGER_SQL)?;
            tx.execute_batch(origin_authority::V87_SESSION_TRIGGER_SQL)?;
            tx.execute_batch(origin_authority::V86_TRIGGER_SQL)?;
            h1_v88_migration_fault(H1V88MigrationFault::AfterTriggers)?;
            let integrity: String = tx.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
            if integrity != "ok" {
                return Err(DaemonError::Store(format!(
                    "V88 requires integrity_check=ok, got {integrity}"
                )));
            }
            let foreign_key_errors: i64 =
                tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                    row.get(0)
                })?;
            if foreign_key_errors != 0 {
                return Err(DaemonError::Store(
                    "V88 static-state-machine catalog foreign-key check failed".into(),
                ));
            }
            h1_v88_migration_fault(H1V88MigrationFault::AfterChecks)?;
            tx.execute("PRAGMA user_version = 88", [])?;
            h1_v88_migration_fault(H1V88MigrationFault::AfterUserVersion)?;
            h1_v88_migration_fault(H1V88MigrationFault::BeforeCommit)?;
            tx.commit()?;
            tracing::info!("V88 migration complete: static execution-origin state machine");
        }

        if version < 89 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            if active_version != 88 {
                return Err(DaemonError::Store(format!(
                    "V89 requires exact V88 source, found V{active_version}"
                )));
            }
            capacity_recovery::validate_v88_source(&tx)?;
            let source_integrity: String =
                tx.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
            if source_integrity != "ok" {
                return Err(DaemonError::Store(format!(
                    "V89 requires integrity_check=ok for exact V88 source, got {source_integrity}"
                )));
            }
            let source_foreign_key_errors: i64 =
                tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                    row.get(0)
                })?;
            if source_foreign_key_errors != 0 {
                return Err(DaemonError::Store(format!(
                    "V89 requires clean V88 foreign keys, found {source_foreign_key_errors} violation(s)"
                )));
            }
            v89_migration_fault(V89MigrationFault::AfterPreflight)?;
            tx.execute_batch(capacity_recovery::V89_TABLE_SQL)?;
            v89_migration_fault(V89MigrationFault::AfterTables)?;
            tx.execute_batch(capacity_recovery::V89_INDEX_SQL)?;
            v89_migration_fault(V89MigrationFault::AfterIndexes)?;
            tx.execute_batch(capacity_recovery::V89_TRIGGER_SQL)?;
            v89_migration_fault(V89MigrationFault::AfterTriggers)?;
            let integrity: String = tx.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
            if integrity != "ok" {
                return Err(DaemonError::Store(format!(
                    "V89 requires integrity_check=ok, got {integrity}"
                )));
            }
            let foreign_key_errors: i64 =
                tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                    row.get(0)
                })?;
            if foreign_key_errors != 0 {
                return Err(DaemonError::Store(
                    "V89 provider-capacity catalog foreign-key check failed".into(),
                ));
            }
            v89_migration_fault(V89MigrationFault::AfterChecks)?;
            tx.execute("PRAGMA user_version = 89", [])?;
            v89_migration_fault(V89MigrationFault::AfterUserVersion)?;
            v89_migration_fault(V89MigrationFault::BeforeCommit)?;
            tx.commit()?;
            tracing::info!("V89 migration complete: provider-capacity recovery ledger");
        }

        if version < 90 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            if active_version != 89 {
                return Err(DaemonError::Store(format!(
                    "V90 requires exact V89 source, found V{active_version}"
                )));
            }
            if !session_execution_projection_catalog_matches(&tx, false)?
                || !session_execution_projection_foreign_keys_match(&tx, false)?
            {
                return Err(DaemonError::Store(
                    "V90 requires exact V89 execution-projection catalog".into(),
                ));
            }
            let missing_projections: i64 = tx.query_row(
                "SELECT count(*) FROM sessions s
                 LEFT JOIN session_execution_projections p ON p.session_id=s.id
                 WHERE p.session_id IS NULL",
                [],
                |row| row.get(0),
            )?;
            if missing_projections != 0 {
                return Err(DaemonError::Store(format!(
                    "V90 requires exactly one execution projection per Session, found {missing_projections} missing"
                )));
            }
            let source_integrity: String =
                tx.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
            if source_integrity != "ok" {
                return Err(DaemonError::Store(format!(
                    "V90 requires integrity_check=ok for exact V89 source, got {source_integrity}"
                )));
            }
            let source_foreign_key_errors: i64 =
                tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                    row.get(0)
                })?;
            if source_foreign_key_errors != 0 {
                return Err(DaemonError::Store(format!(
                    "V90 requires clean V89 foreign keys, found {source_foreign_key_errors} violation(s)"
                )));
            }
            v90_migration_fault(V90MigrationFault::AfterPreflight)?;

            tx.execute_batch(
                "PRAGMA defer_foreign_keys=ON;
                 DROP TRIGGER sessions_execution_projection_after_insert;
                 DROP TRIGGER session_execution_projections_no_delete;
                 ALTER TABLE session_execution_projections
                     RENAME TO session_execution_projections_v89;",
            )?;
            v90_migration_fault(V90MigrationFault::AfterRename)?;
            tx.execute_batch(SESSION_EXECUTION_PROJECTION_V90_TABLE_SQL)?;
            v90_migration_fault(V90MigrationFault::AfterCreate)?;

            let source_count: i64 = tx.query_row(
                "SELECT count(*) FROM session_execution_projections_v89",
                [],
                |row| row.get(0),
            )?;
            tx.execute_batch(
                "INSERT INTO session_execution_projections
                    (session_id,schema_version,projection_version,execution_state,freshness,
                     canonical_repo_dir,effective_cwd,custody_id,custody_generation,
                     validated_at,error_code,updated_at)
                 SELECT session_id,schema_version,projection_version,execution_state,freshness,
                        canonical_repo_dir,effective_cwd,custody_id,custody_generation,
                        validated_at,error_code,updated_at
                 FROM session_execution_projections_v89;",
            )?;
            v90_corrupt_copy_for_test(&tx)?;
            let destination_count: i64 = tx.query_row(
                "SELECT count(*) FROM session_execution_projections",
                [],
                |row| row.get(0),
            )?;
            let source_minus_destination: i64 = tx.query_row(
                "SELECT count(*) FROM (
                    SELECT session_id,schema_version,projection_version,execution_state,freshness,
                           canonical_repo_dir,effective_cwd,custody_id,custody_generation,
                           validated_at,error_code,updated_at
                    FROM session_execution_projections_v89
                    EXCEPT
                    SELECT session_id,schema_version,projection_version,execution_state,freshness,
                           canonical_repo_dir,effective_cwd,custody_id,custody_generation,
                           validated_at,error_code,updated_at
                    FROM session_execution_projections
                 )",
                [],
                |row| row.get(0),
            )?;
            let destination_minus_source: i64 = tx.query_row(
                "SELECT count(*) FROM (
                    SELECT session_id,schema_version,projection_version,execution_state,freshness,
                           canonical_repo_dir,effective_cwd,custody_id,custody_generation,
                           validated_at,error_code,updated_at
                    FROM session_execution_projections
                    EXCEPT
                    SELECT session_id,schema_version,projection_version,execution_state,freshness,
                           canonical_repo_dir,effective_cwd,custody_id,custody_generation,
                           validated_at,error_code,updated_at
                    FROM session_execution_projections_v89
                 )",
                [],
                |row| row.get(0),
            )?;
            if source_count != destination_count
                || source_minus_destination != 0
                || destination_minus_source != 0
            {
                return Err(DaemonError::Store(format!(
                    "V90 execution-projection copy mismatch: source={source_count}, destination={destination_count}, source-minus-destination={source_minus_destination}, destination-minus-source={destination_minus_source}"
                )));
            }
            v90_migration_fault(V90MigrationFault::AfterCopy)?;

            tx.execute_batch("DROP TABLE session_execution_projections_v89;")?;
            v90_migration_fault(V90MigrationFault::AfterOldTableDrop)?;
            tx.execute_batch(SESSION_EXECUTION_PROJECTION_INDEX_SQL)?;
            v90_migration_fault(V90MigrationFault::AfterIndexes)?;
            tx.execute_batch(SESSION_EXECUTION_PROJECTION_NO_DELETE_SQL)?;
            tx.execute_batch(SESSION_EXECUTION_PROJECTION_AFTER_INSERT_SQL)?;
            tx.execute_batch(SESSION_EXECUTION_PROJECTION_PURGE_GUARD_SQL)?;
            v90_migration_fault(V90MigrationFault::AfterTriggers)?;

            if !session_execution_projection_catalog_matches(&tx, true)?
                || !session_execution_projection_foreign_keys_match(&tx, true)?
            {
                return Err(DaemonError::Store(
                    "V90 execution-projection result catalog mismatch".into(),
                ));
            }
            let integrity: String = tx.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
            if integrity != "ok" {
                return Err(DaemonError::Store(format!(
                    "V90 requires integrity_check=ok, got {integrity}"
                )));
            }
            let foreign_key_errors: i64 =
                tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                    row.get(0)
                })?;
            if foreign_key_errors != 0 {
                return Err(DaemonError::Store(format!(
                    "V90 execution-projection foreign-key check found {foreign_key_errors} violation(s)"
                )));
            }
            v90_migration_fault(V90MigrationFault::AfterChecks)?;
            tx.execute("PRAGMA user_version = 90", [])?;
            v90_migration_fault(V90MigrationFault::AfterUserVersion)?;
            v90_migration_fault(V90MigrationFault::BeforeCommit)?;
            tx.commit()?;
            tracing::info!(
                "V90 migration complete: detached retained execution projections with guarded purge"
            );
        }

        if version < 91 {
            // V91 changes no durable DDL. It authenticates the exact two-shape
            // V90 whole catalog and refuses any active predecessor that cannot
            // complete its unchanged V87 Session-fence sequence.
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            if active_version != 90 {
                return Err(DaemonError::Store(format!(
                    "V91 requires exact V90 source, found V{active_version}"
                )));
            }
            h1_v91_validate_v90_catalog(&tx)?;
            h1_validate_exact_active_predecessors(&tx, 91, 90)?;
            h1_v91_migration_fault(H1V91MigrationFault::AfterPreflight)?;
            let integrity: String = tx.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
            if integrity != "ok" {
                return Err(DaemonError::Store(format!(
                    "V91 requires integrity_check=ok, got {integrity}"
                )));
            }
            let foreign_key_errors: i64 =
                tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                    row.get(0)
                })?;
            if foreign_key_errors != 0 {
                return Err(DaemonError::Store(format!(
                    "V91 exact-predecessor foreign-key check found {foreign_key_errors} violation(s)"
                )));
            }
            h1_v91_migration_fault(H1V91MigrationFault::AfterChecks)?;
            tx.execute("PRAGMA user_version = 91", [])?;
            h1_v91_migration_fault(H1V91MigrationFault::AfterUserVersion)?;
            h1_v91_migration_fault(H1V91MigrationFault::BeforeCommit)?;
            tx.commit()?;
            tracing::info!("V91 migration complete: exact execution-origin predecessors");
        }

        if version < 92 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            if active_version != 91 {
                return Err(DaemonError::Store(format!(
                    "V92 requires exact V91 source, found V{active_version}"
                )));
            }
            // V91 changes no DDL, so its authenticated source catalog is the
            // exact accepted V90 catalog plus the V91 version marker.
            h1_v91_validate_v90_catalog(&tx)?;
            h1_validate_exact_active_predecessors(&tx, 92, 91)?;
            h1_v92_migration_fault(H1V92MigrationFault::AfterPreflight)?;

            tx.execute_batch(
                "CREATE TABLE epic_lead_generations (
                    epic_id TEXT PRIMARY KEY CHECK(rsi_uuid_is_canonical(epic_id)) REFERENCES sessions(id) ON DELETE RESTRICT,
                    generation INTEGER NOT NULL CHECK(generation >= 1),
                    updated_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(updated_at))
                );
                CREATE TABLE agent_successor_reservations (
                    reservation_id TEXT PRIMARY KEY CHECK(rsi_uuid_is_canonical(reservation_id)),
                    predecessor_session_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(predecessor_session_id)) REFERENCES sessions(id) ON DELETE RESTRICT,
                    epic_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(epic_id)) REFERENCES sessions(id) ON DELETE RESTRICT,
                    candidate_session_id TEXT NOT NULL UNIQUE CHECK(rsi_uuid_is_canonical(candidate_session_id)),
                    caller_key_digest TEXT NOT NULL CHECK(rsi_sha256_digest_is_canonical(caller_key_digest)),
                    request_json TEXT NOT NULL CHECK(json_valid(request_json)),
                    request_fingerprint TEXT NOT NULL CHECK(rsi_sha256_digest_is_canonical(request_fingerprint)),
                    candidate_kind TEXT NOT NULL CHECK(candidate_kind IN ('Story','Task','Bug','Feature','Refactor','Research')),
                    inherited_launch_json TEXT NOT NULL CHECK(json_valid(inherited_launch_json)),
                    expected_lead_session_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(expected_lead_session_id) AND expected_lead_session_id=predecessor_session_id),
                    expected_lead_generation INTEGER NOT NULL CHECK(expected_lead_generation >= 1),
                    state TEXT NOT NULL CHECK(state IN ('reserved','launching','committed','failed','uncertain')),
                    state_version INTEGER NOT NULL CHECK(state_version >= 1),
                    launch_attempt_id TEXT CHECK(launch_attempt_id IS NULL OR rsi_uuid_is_canonical(launch_attempt_id)),
                    model_invocation_id TEXT CHECK(model_invocation_id IS NULL OR rsi_uuid_is_canonical(model_invocation_id)),
                    establishment_evidence_json TEXT CHECK(establishment_evidence_json IS NULL OR json_valid(establishment_evidence_json)),
                    establishment_digest TEXT CHECK(establishment_digest IS NULL OR rsi_sha256_digest_is_canonical(establishment_digest)),
                    published_at TEXT CHECK(published_at IS NULL OR rsi_rfc3339_nanos_is_canonical(published_at)),
                    terminal_reason TEXT CHECK(terminal_reason IS NULL OR length(terminal_reason)<=256),
                    safe_error_class TEXT CHECK(safe_error_class IS NULL OR length(safe_error_class)<=128),
                    reserved_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(reserved_at)),
                    updated_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(updated_at)),
                    UNIQUE(predecessor_session_id,caller_key_digest),
                    FOREIGN KEY(reservation_id,state_version)
                        REFERENCES agent_successor_transitions(reservation_id,state_version)
                        DEFERRABLE INITIALLY DEFERRED,
                    CHECK((launch_attempt_id IS NULL)=(model_invocation_id IS NULL)),
                    CHECK((establishment_evidence_json IS NULL)=(establishment_digest IS NULL)),
                    CHECK((terminal_reason IS NULL)=(safe_error_class IS NULL)),
                    CHECK(updated_at>=reserved_at),
                    CHECK(published_at IS NULL OR published_at>=reserved_at),
                    CHECK(
                        (state='reserved' AND launch_attempt_id IS NULL AND establishment_evidence_json IS NULL AND terminal_reason IS NULL)
                        OR (state='launching' AND launch_attempt_id IS NOT NULL AND establishment_evidence_json IS NULL AND terminal_reason IS NULL)
                        OR (state='uncertain' AND launch_attempt_id IS NOT NULL AND establishment_evidence_json IS NULL AND terminal_reason IS NOT NULL)
                        OR (state='committed' AND launch_attempt_id IS NOT NULL AND establishment_evidence_json IS NOT NULL AND terminal_reason IS NULL)
                        OR (state='failed' AND establishment_evidence_json IS NULL AND terminal_reason IS NOT NULL)
                    )
                );
                CREATE TABLE agent_successor_transitions (
                    transition_id TEXT PRIMARY KEY CHECK(rsi_uuid_is_canonical(transition_id)),
                    reservation_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(reservation_id)) REFERENCES agent_successor_reservations(reservation_id) ON DELETE RESTRICT,
                    state_version INTEGER NOT NULL CHECK(state_version >= 1),
                    from_state TEXT CHECK(from_state IS NULL OR from_state IN ('reserved','launching','uncertain')),
                    to_state TEXT NOT NULL CHECK(to_state IN ('reserved','launching','committed','failed','uncertain')),
                    authority_digest TEXT NOT NULL CHECK(rsi_sha256_digest_is_canonical(authority_digest)),
                    launch_attempt_id TEXT CHECK(launch_attempt_id IS NULL OR rsi_uuid_is_canonical(launch_attempt_id)),
                    model_invocation_id TEXT CHECK(model_invocation_id IS NULL OR rsi_uuid_is_canonical(model_invocation_id)),
                    reason TEXT CHECK(reason IS NULL OR length(reason)<=256),
                    created_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(created_at)),
                    UNIQUE(reservation_id,state_version),
                    CHECK((launch_attempt_id IS NULL)=(model_invocation_id IS NULL)),
                    CHECK(
                        (to_state='reserved' AND from_state IS NULL AND state_version=1 AND launch_attempt_id IS NULL AND reason IS NULL)
                        OR (to_state='launching' AND from_state='reserved' AND launch_attempt_id IS NOT NULL AND reason IS NULL)
                        OR (to_state='uncertain' AND from_state='launching' AND launch_attempt_id IS NOT NULL AND reason IS NOT NULL)
                        OR (to_state='committed' AND from_state IN ('launching','uncertain') AND launch_attempt_id IS NOT NULL AND reason IS NULL)
                        OR (to_state='failed' AND from_state IN ('reserved','launching','uncertain') AND reason IS NOT NULL)
                    )
                );
                CREATE INDEX idx_agent_successor_reconcile ON agent_successor_reservations(state,updated_at,reservation_id);
                CREATE INDEX idx_agent_successor_predecessor ON agent_successor_reservations(predecessor_session_id,reserved_at,reservation_id);
                CREATE INDEX idx_agent_successor_transitions_order ON agent_successor_transitions(reservation_id,state_version);

                CREATE TRIGGER epic_lead_generations_no_delete BEFORE DELETE ON epic_lead_generations BEGIN
                    SELECT RAISE(ABORT,'epic_lead_generation_no_delete');
                END;
                CREATE TRIGGER epic_lead_generations_epic_only_insert BEFORE INSERT ON epic_lead_generations
                WHEN NOT EXISTS(SELECT 1 FROM sessions WHERE id=NEW.epic_id AND session_kind='Epic') BEGIN
                    SELECT RAISE(ABORT,'epic_lead_generation_requires_epic');
                END;
                CREATE TRIGGER epic_lead_generations_forward_only BEFORE UPDATE ON epic_lead_generations
                WHEN NEW.epic_id!=OLD.epic_id OR NEW.generation!=OLD.generation+1 BEGIN
                    SELECT RAISE(ABORT,'epic_lead_generation_must_increment_once');
                END;
                CREATE TRIGGER sessions_epic_lead_generation_insert AFTER INSERT ON sessions WHEN NEW.session_kind='Epic' BEGIN
                    INSERT INTO epic_lead_generations(epic_id,generation,updated_at)
                    VALUES(NEW.id,1,strftime('%Y-%m-%dT%H:%M:%f000000Z','now'));
                END;
                CREATE TRIGGER sessions_epic_lead_generation_change AFTER UPDATE OF lead_session_id ON sessions
                WHEN OLD.session_kind='Epic' AND NEW.lead_session_id IS NOT OLD.lead_session_id BEGIN
                    UPDATE epic_lead_generations SET generation=generation+1,
                        updated_at=strftime('%Y-%m-%dT%H:%M:%f000000Z','now') WHERE epic_id=NEW.id;
                END;

                CREATE TRIGGER agent_successor_reservations_no_delete BEFORE DELETE ON agent_successor_reservations BEGIN
                    SELECT RAISE(ABORT,'agent_successor_reservation_no_delete');
                END;
                CREATE TRIGGER agent_successor_reservations_identity_immutable BEFORE UPDATE ON agent_successor_reservations
                WHEN NEW.reservation_id!=OLD.reservation_id OR NEW.predecessor_session_id!=OLD.predecessor_session_id
                  OR NEW.epic_id!=OLD.epic_id OR NEW.candidate_session_id!=OLD.candidate_session_id
                  OR NEW.caller_key_digest!=OLD.caller_key_digest OR NEW.request_json!=OLD.request_json
                  OR NEW.request_fingerprint!=OLD.request_fingerprint OR NEW.candidate_kind!=OLD.candidate_kind
                  OR NEW.inherited_launch_json!=OLD.inherited_launch_json OR NEW.expected_lead_session_id!=OLD.expected_lead_session_id
                  OR NEW.expected_lead_generation!=OLD.expected_lead_generation OR NEW.reserved_at!=OLD.reserved_at BEGIN
                    SELECT RAISE(ABORT,'agent_successor_reservation_identity_immutable');
                END;
                CREATE TRIGGER agent_successor_reservations_forward_state BEFORE UPDATE ON agent_successor_reservations
                WHEN NEW.state!=OLD.state AND (NEW.state_version!=OLD.state_version+1 OR NOT (
                    (OLD.state='reserved' AND NEW.state IN ('launching','failed')) OR
                    (OLD.state='launching' AND NEW.state IN ('committed','failed','uncertain')) OR
                    (OLD.state='uncertain' AND NEW.state IN ('committed','failed'))
                )) BEGIN SELECT RAISE(ABORT,'agent_successor_reservation_invalid_transition'); END;
                CREATE TRIGGER agent_successor_reservations_version_guard BEFORE UPDATE ON agent_successor_reservations
                WHEN NEW.state=OLD.state AND (
                    NEW.state_version!=OLD.state_version
                    OR NEW.launch_attempt_id IS NOT OLD.launch_attempt_id
                    OR NEW.model_invocation_id IS NOT OLD.model_invocation_id
                    OR NEW.terminal_reason IS NOT OLD.terminal_reason
                ) BEGIN
                    SELECT RAISE(ABORT,'agent_successor_reservation_version_without_transition');
                END;
                CREATE TRIGGER agent_successor_reservations_terminal BEFORE UPDATE ON agent_successor_reservations
                WHEN OLD.state IN ('committed','failed') AND (NEW.state!=OLD.state OR NEW.state_version!=OLD.state_version) BEGIN
                    SELECT RAISE(ABORT,'agent_successor_reservation_terminal');
                END;
                CREATE TRIGGER agent_successor_transitions_no_update BEFORE UPDATE ON agent_successor_transitions BEGIN
                    SELECT RAISE(ABORT,'agent_successor_transition_immutable');
                END;
                CREATE TRIGGER agent_successor_transitions_no_delete BEFORE DELETE ON agent_successor_transitions BEGIN
                    SELECT RAISE(ABORT,'agent_successor_transition_no_delete');
                END;
                CREATE TRIGGER agent_successor_transitions_match_current BEFORE INSERT ON agent_successor_transitions
                WHEN NOT EXISTS(
                    SELECT 1 FROM agent_successor_reservations reservation
                    WHERE reservation.reservation_id=NEW.reservation_id
                      AND reservation.state_version=NEW.state_version
                      AND reservation.state=NEW.to_state
                      AND reservation.launch_attempt_id IS NEW.launch_attempt_id
                      AND reservation.model_invocation_id IS NEW.model_invocation_id
                      AND reservation.terminal_reason IS NEW.reason
                      AND (
                          (NEW.state_version=1 AND NEW.from_state IS NULL)
                          OR EXISTS(
                              SELECT 1 FROM agent_successor_transitions previous
                              WHERE previous.reservation_id=NEW.reservation_id
                                AND previous.state_version=NEW.state_version-1
                                AND previous.to_state=NEW.from_state
                          )
                      )
                ) BEGIN
                    SELECT RAISE(ABORT,'agent_successor_transition_aggregate_mismatch');
                END;",
            )?;
            h1_v92_migration_fault(H1V92MigrationFault::AfterSchema)?;
            tx.execute(
                "INSERT INTO epic_lead_generations(epic_id,generation,updated_at)
                 SELECT id,1,strftime('%Y-%m-%dT%H:%M:%f000000Z','now') FROM sessions WHERE session_kind='Epic'",
                [],
            )?;
            h1_v92_migration_fault(H1V92MigrationFault::AfterLeadSeed)?;

            let missing_epics: i64 = tx.query_row(
                "SELECT count(*) FROM sessions s LEFT JOIN epic_lead_generations g ON g.epic_id=s.id
                 WHERE s.session_kind='Epic' AND g.epic_id IS NULL",
                [],
                |row| row.get(0),
            )?;
            if missing_epics != 0 {
                return Err(DaemonError::Store(format!(
                    "V92 lead-generation seed missed {missing_epics} Epic row(s)"
                )));
            }
            h1_v92_validate_catalog(&tx)?;
            let integrity: String = tx.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
            if integrity != "ok" {
                return Err(DaemonError::Store(format!(
                    "V92 requires integrity_check=ok, got {integrity}"
                )));
            }
            let foreign_key_errors: i64 =
                tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                    row.get(0)
                })?;
            if foreign_key_errors != 0 {
                return Err(DaemonError::Store(format!(
                    "V92 successor-ledger foreign-key check found {foreign_key_errors} violation(s)"
                )));
            }
            h1_v92_migration_fault(H1V92MigrationFault::AfterChecks)?;
            tx.execute("PRAGMA user_version = 92", [])?;
            h1_v92_migration_fault(H1V92MigrationFault::AfterUserVersion)?;
            h1_v92_migration_fault(H1V92MigrationFault::BeforeCommit)?;
            tx.commit()?;
            tracing::info!("V92 migration complete: master-successor ledger and lead fence");
        }

        if stop_after_v92 {
            return Ok(());
        }

        if version < 93 {
            // Pioneer is a new stable SessionProvider string. V55 put a closed
            // provider CHECK on recursive_live_attempts, so existing databases
            // need a bounded rebuild before recursive live launches can persist
            // Pioneer without violating that historical constraint.
            self.conn.execute_batch("PRAGMA foreign_keys=OFF;")?;
            let outcome = self.apply_pioneer_v93_migration();
            self.conn.execute_batch("PRAGMA foreign_keys=ON;")?;
            outcome?;
        }

        {
            let tx = self.conn.unchecked_transaction()?;
            add_column_if_not_exists_tx(
                &tx,
                "model_invocations",
                "cancellation_requested_at",
                "TEXT",
            )?;
            add_column_if_not_exists_tx(&tx, "model_invocations", "cancellation_reason", "TEXT")?;
            add_column_if_not_exists_tx(
                &tx,
                "model_invocations",
                "cancellation_mechanism",
                "TEXT",
            )?;
            tx.execute_batch(
                "
                CREATE TABLE IF NOT EXISTS model_budget_alert_events (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    invocation_id TEXT NOT NULL,
                    scope_kind TEXT NOT NULL,
                    scope_id TEXT NOT NULL,
                    purpose TEXT NOT NULL,
                    metric TEXT NOT NULL,
                    remaining INTEGER NOT NULL,
                    limit_value INTEGER NOT NULL,
                    threshold INTEGER NOT NULL,
                    emitted_at TEXT NOT NULL,
                    UNIQUE(invocation_id, scope_kind, scope_id, purpose, metric, threshold)
                );
                CREATE INDEX IF NOT EXISTS idx_model_budget_alert_events_emitted_at
                    ON model_budget_alert_events(emitted_at DESC, id DESC);
                ",
            )?;
            tx.commit()?;
        }

        // This compatibility repair is part of the shipped V93 source
        // catalog. Converge it before V94 so blank upgrades and databases
        // already at V93 enter the retained journal migration identically.
        if version < 94 {
            self.apply_source_worktree_settlement_v94_migration()?;
        }

        if version < 95 {
            self.apply_source_worktree_settlement_v95_migration()?;
        } else {
            let tx = self.conn.unchecked_transaction()?;
            cohort_settlement::validate_v95_catalog(&tx)?;
            tx.commit()?;
        }

        // V96: tool calls are asynchronous transactions, so their provider
        // correlation id is durable data rather than a display-only hint. Old
        // rows remain NULL: adjacency is not authoritative enough to backfill.
        if version < 96 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            add_column_if_not_exists_tx(&tx, "conversation_events", "tool_use_id", "TEXT")?;
            add_column_if_not_exists_tx(&tx, "conversation_events", "metadata", "TEXT")?;
            tx.execute(
                "CREATE INDEX IF NOT EXISTS idx_events_tool_use_id
                 ON conversation_events(tool_use_id)",
                [],
            )?;
            tx.execute("PRAGMA user_version = 96", [])?;
            tx.commit()?;
            tracing::info!("V96 migration complete: conversation event tool correlation");
        }

        // V97: guarded Issue CRUD needs durable optimistic versions plus an
        // append-only receipt/audit stream. This is an exact-source,
        // version-last migration: a failure at any boundary leaves a usable
        // V96 catalog and the old daemon may reopen it safely.
        if version < 97 {
            self.apply_issue_v97_migration()?;
        }

        // V98: restored sandbox sessions receive a fresh immutable allocation
        // identity, so their recreated worktree cannot collide with a purged
        // historical custody root for the same Session id.
        if version < 98 {
            self.apply_sandbox_allocation_identity_v98_migration()?;
        }

        // V99: consume what the provider CLI already sends. The Claude CLI
        // emits a rich `system/init` handshake, a `rate_limit_event` stream,
        // and a ~12-counter `result.usage` object; RSI read two init fields,
        // dropped rate limits on the floor, and persisted four usage counters.
        //
        // Additive only — nullable session/turn columns plus one new table. No
        // released DDL is touched and no spawn behaviour changes; this is a
        // receive-side migration. Existing rows stay NULL: none of these facts
        // is recoverable for a session that already ran.
        if version < 99 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;

            // P1-A: what the CLI told us it is, and what it told us it supports.
            add_column_if_not_exists_tx(&tx, "sessions", "provider_cli_version", "TEXT")?;
            add_column_if_not_exists_tx(&tx, "sessions", "provider_capabilities", "TEXT")?;

            // P1-C: richer `result.usage` capture, session-level.
            add_column_if_not_exists_tx(&tx, "sessions", "thinking_tokens", "INTEGER")?;
            add_column_if_not_exists_tx(&tx, "sessions", "service_tier", "TEXT")?;
            add_column_if_not_exists_tx(&tx, "sessions", "cache_creation_1h_tokens", "INTEGER")?;
            add_column_if_not_exists_tx(&tx, "sessions", "cache_creation_5m_tokens", "INTEGER")?;
            add_column_if_not_exists_tx(&tx, "sessions", "permission_denial_count", "INTEGER")?;
            add_column_if_not_exists_tx(&tx, "sessions", "subagent_stats_json", "TEXT")?;
            add_column_if_not_exists_tx(&tx, "sessions", "queued_turn_count", "INTEGER")?;
            add_column_if_not_exists_tx(&tx, "sessions", "terminal_reason", "TEXT")?;

            // P1-C: the same signals attributed per turn.
            add_column_if_not_exists_tx(
                &tx,
                "turn_metrics",
                "thinking_tokens",
                "INTEGER NOT NULL DEFAULT 0",
            )?;
            add_column_if_not_exists_tx(
                &tx,
                "turn_metrics",
                "cache_creation_1h_tokens",
                "INTEGER NOT NULL DEFAULT 0",
            )?;
            add_column_if_not_exists_tx(
                &tx,
                "turn_metrics",
                "cache_creation_5m_tokens",
                "INTEGER NOT NULL DEFAULT 0",
            )?;
            add_column_if_not_exists_tx(&tx, "turn_metrics", "service_tier", "TEXT")?;

            // P1-B: plan-window utilization is an ACCOUNT-level fact — every
            // concurrent session reports the same windows — so it is a
            // daemon-wide latest-wins snapshot keyed by provider and window,
            // not a set of session columns.
            //
            // `provider` is the serde string of `SessionProvider`, so a second
            // provider reporting windows needs no schema change.
            // `resets_at_epoch` holds the provider's raw unix seconds verbatim;
            // `observed_at` is the RFC3339-with-nanoseconds timestamp the
            // repo's timestamp rule requires.
            tx.execute(
                "CREATE TABLE IF NOT EXISTS provider_rate_limit_windows (
                     provider            TEXT    NOT NULL,
                     window_key          TEXT    NOT NULL,
                     utilization         REAL    NOT NULL,
                     resets_at_epoch     INTEGER,
                     status              TEXT,
                     rate_limit_type     TEXT,
                     overage_status      TEXT,
                     is_using_overage    INTEGER NOT NULL DEFAULT 0,
                     observed_at         TEXT    NOT NULL,
                     observed_session_id TEXT    REFERENCES sessions(id),
                     PRIMARY KEY (provider, window_key)
                 )",
                [],
            )?;

            tx.execute("PRAGMA user_version = 99", [])?;
            tx.commit()?;
            tracing::info!(
                "V99 migration complete: provider handshake, rate-limit windows, richer usage"
            );
        }

        // V100: persist the active context-window evidence without inventing
        // provenance for historical numeric values.
        if version < 100 {
            self.apply_context_window_provenance_v100_migration()?;
        }

        // V101: preserve the raw configured window independently from the
        // provider's effective active denominator.
        if version < 101 {
            self.apply_configured_context_window_v101_migration()?;
        }

        // V102: operator-appointed project manager and durable, correlated inbox.
        // Provider transport remains owned by existing terminal watches.
        if version < 102 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            tx.execute_batch(
                "CREATE TABLE harness_manager_scopes (
                    project_id TEXT PRIMARY KEY NOT NULL REFERENCES projects(id),
                    manager_session_id TEXT NOT NULL REFERENCES sessions(id),
                    epic_ids_json TEXT NOT NULL CHECK(json_valid(epic_ids_json)
                        AND json_type(epic_ids_json)='array' AND json_array_length(epic_ids_json)<=32),
                    row_version INTEGER NOT NULL CHECK(row_version>0),
                    updated_at TEXT NOT NULL
                 );
                 CREATE TABLE harness_manager_messages (
                    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                    id TEXT NOT NULL UNIQUE,
                    project_id TEXT NOT NULL REFERENCES projects(id),
                    manager_session_id TEXT NOT NULL REFERENCES sessions(id),
                    epic_id TEXT NOT NULL REFERENCES sessions(id),
                    scope_version INTEGER NOT NULL CHECK(scope_version>0),
                    sender_session_id TEXT NOT NULL REFERENCES sessions(id),
                    recipient_session_id TEXT NOT NULL REFERENCES sessions(id),
                    request_id TEXT REFERENCES harness_manager_messages(id),
                    idempotency_key TEXT NOT NULL CHECK(length(CAST(idempotency_key AS BLOB)) BETWEEN 1 AND 128),
                    request_fingerprint TEXT NOT NULL,
                    message TEXT NOT NULL CHECK(length(CAST(message AS BLOB)) BETWEEN 1 AND 8192),
                    created_at TEXT NOT NULL,
                    UNIQUE(sender_session_id,idempotency_key)
                 );
                 CREATE INDEX harness_manager_messages_inbox
                    ON harness_manager_messages(project_id,scope_version,sequence);
                 CREATE INDEX harness_manager_messages_replies
                    ON harness_manager_messages(request_id,sequence);
                 CREATE TRIGGER harness_manager_messages_no_update
                    BEFORE UPDATE ON harness_manager_messages
                    BEGIN SELECT RAISE(ABORT,'manager messages are immutable'); END;
                 CREATE TRIGGER harness_manager_messages_no_delete
                    BEFORE DELETE ON harness_manager_messages
                    BEGIN SELECT RAISE(ABORT,'manager messages are retained for audit'); END;
                 CREATE TABLE harness_manager_watches (
                    job_id TEXT PRIMARY KEY NOT NULL REFERENCES scheduled_jobs(id),
                    project_id TEXT NOT NULL REFERENCES projects(id),
                    epic_id TEXT NOT NULL REFERENCES sessions(id),
                    scope_version INTEGER NOT NULL CHECK(scope_version>0),
                    direction TEXT NOT NULL CHECK(direction IN ('to_manager','to_lead')),
                    source_session_id TEXT NOT NULL REFERENCES sessions(id),
                    target_session_id TEXT NOT NULL REFERENCES sessions(id),
                    attention_signature TEXT NOT NULL
                 );
                 CREATE INDEX harness_manager_watches_scope
                    ON harness_manager_watches(project_id,scope_version);",
            )?;
            tx.execute("PRAGMA user_version = 102", [])?;
            tx.commit()?;
        }

        // V103: successful manager-lineage rotation receipts and notice CAS.
        if version < 103 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            tx.execute_batch(
                "CREATE TABLE harness_manager_rotation_edges (
                    predecessor_session_id TEXT NOT NULL REFERENCES sessions(id),
                    successor_session_id TEXT NOT NULL REFERENCES sessions(id),
                    committed_at TEXT NOT NULL,
                    retired_at TEXT,
                    PRIMARY KEY(predecessor_session_id,successor_session_id),
                    CHECK(predecessor_session_id<>successor_session_id)
                 );
                 CREATE UNIQUE INDEX harness_manager_rotation_current
                    ON harness_manager_rotation_edges(predecessor_session_id) WHERE retired_at IS NULL;
                 CREATE TRIGGER harness_manager_rotation_retire_on_restore
                    AFTER UPDATE OF status ON sessions
                    WHEN OLD.status='Archived' AND NEW.status<>'Archived'
                    BEGIN UPDATE harness_manager_rotation_edges SET retired_at=NEW.updated_at
                      WHERE predecessor_session_id=NEW.id AND retired_at IS NULL; END;
                 ALTER TABLE harness_manager_watches ADD COLUMN notice_generation INTEGER NOT NULL DEFAULT 1
                    CHECK(typeof(notice_generation)='integer' AND notice_generation>0);
                 CREATE TRIGGER harness_manager_watch_generation_after_update
                    AFTER UPDATE ON scheduled_jobs
                    BEGIN UPDATE harness_manager_watches SET notice_generation=notice_generation+1
                      WHERE job_id=NEW.id; END;",
            )?;
            tx.execute("PRAGMA user_version = 103", [])?;
            tx.commit()?;
        }

        // V104: explicit manager capabilities, typed coordination records and
        // durable action publication. V1 appointments receive no implicit grant.
        if version < 104 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            tx.execute_batch(
                "CREATE TABLE harness_manager_v2_policies (
                    project_id TEXT PRIMARY KEY NOT NULL REFERENCES projects(id),
                    manager_session_id TEXT NOT NULL REFERENCES sessions(id),
                    scope_version INTEGER NOT NULL CHECK(scope_version>0),
                    row_version INTEGER NOT NULL CHECK(row_version>0),
                    policy_json TEXT NOT NULL CHECK(json_valid(policy_json) AND length(policy_json)<=32768),
                    updated_at TEXT NOT NULL
                 );
                 CREATE TABLE harness_manager_v2_operations (
                    id TEXT PRIMARY KEY NOT NULL,
                    project_id TEXT NOT NULL REFERENCES projects(id),
                    manager_session_id TEXT NOT NULL REFERENCES sessions(id),
                    scope_version INTEGER NOT NULL CHECK(scope_version>0),
                    policy_version INTEGER NOT NULL CHECK(policy_version>=0),
                    actor_session_id TEXT REFERENCES sessions(id),
                    idempotency_key TEXT NOT NULL CHECK(length(CAST(idempotency_key AS BLOB)) BETWEEN 1 AND 128),
                    fingerprint TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    payload_json TEXT NOT NULL CHECK(json_valid(payload_json) AND length(payload_json)<=65536),
                    state TEXT NOT NULL CHECK(state IN ('queued','running','succeeded','failed','blocked','uncertain','revoked')),
                    row_version INTEGER NOT NULL DEFAULT 1 CHECK(row_version>0),
                    target_session_id TEXT,
                    outcome_json TEXT CHECK(outcome_json IS NULL OR json_valid(outcome_json)),
                    attempts INTEGER NOT NULL DEFAULT 0 CHECK(attempts>=0),
                    not_before TEXT NOT NULL,
                    claim_boot_id TEXT,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    UNIQUE(project_id,manager_session_id,scope_version,idempotency_key)
                 );
                 CREATE INDEX harness_manager_v2_operation_due
                    ON harness_manager_v2_operations(state,not_before,id);
                 CREATE INDEX harness_manager_v2_operation_scope
                    ON harness_manager_v2_operations(project_id,manager_session_id,scope_version,state,id);
                 CREATE TABLE harness_manager_v2_records (
                    project_id TEXT NOT NULL REFERENCES projects(id),
                    manager_session_id TEXT NOT NULL REFERENCES sessions(id),
                    scope_version INTEGER NOT NULL CHECK(scope_version>0),
                    kind TEXT NOT NULL,
                    record_key TEXT NOT NULL CHECK(length(CAST(record_key AS BLOB)) BETWEEN 1 AND 256),
                    epic_id TEXT REFERENCES sessions(id),
                    row_version INTEGER NOT NULL CHECK(row_version>0),
                    payload_json TEXT NOT NULL CHECK(json_valid(payload_json) AND length(payload_json)<=65536),
                    archived INTEGER NOT NULL DEFAULT 0 CHECK(archived IN (0,1)),
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    PRIMARY KEY(project_id,manager_session_id,scope_version,kind,record_key)
                 );
                 CREATE INDEX harness_manager_v2_record_epic
                    ON harness_manager_v2_records(project_id,scope_version,epic_id,kind,record_key);
                 CREATE TABLE harness_manager_v2_events (
                    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                    project_id TEXT NOT NULL REFERENCES projects(id),
                    manager_session_id TEXT NOT NULL REFERENCES sessions(id),
                    scope_version INTEGER NOT NULL CHECK(scope_version>0),
                    actor_session_id TEXT REFERENCES sessions(id),
                    kind TEXT NOT NULL,
                    record_key TEXT NOT NULL,
                    row_version INTEGER NOT NULL CHECK(row_version>=0),
                    payload_json TEXT NOT NULL CHECK(json_valid(payload_json) AND length(payload_json)<=65536),
                    created_at TEXT NOT NULL
                 );
                 CREATE INDEX harness_manager_v2_event_scope
                    ON harness_manager_v2_events(project_id,manager_session_id,scope_version,sequence);
                 CREATE TRIGGER harness_manager_v2_events_no_update
                    BEFORE UPDATE ON harness_manager_v2_events
                    BEGIN SELECT RAISE(ABORT,'immutable manager event'); END;
                 CREATE TRIGGER harness_manager_v2_events_no_delete
                    BEFORE DELETE ON harness_manager_v2_events
                    BEGIN SELECT RAISE(ABORT,'immutable manager event'); END;
                 CREATE TABLE harness_manager_v2_entities (
                    session_id TEXT PRIMARY KEY NOT NULL REFERENCES sessions(id),
                    operation_id TEXT NOT NULL UNIQUE REFERENCES harness_manager_v2_operations(id),
                    project_id TEXT NOT NULL REFERENCES projects(id),
                    manager_session_id TEXT NOT NULL REFERENCES sessions(id),
                    scope_version INTEGER NOT NULL CHECK(scope_version>0),
                    policy_version INTEGER NOT NULL CHECK(policy_version>0),
                    kind TEXT NOT NULL,
                    created_at TEXT NOT NULL
                 );
                 CREATE INDEX harness_manager_v2_entity_scope
                    ON harness_manager_v2_entities(project_id,manager_session_id,scope_version,kind);",
            )?;
            tx.execute("PRAGMA user_version = 104", [])?;
            tx.commit()?;
        }

        // V105: producer-bound pending-question publication. A reserved epoch
        // is an unresolved gate until its event and identity commit together.
        // Existing pending snapshots deliberately receive no inferred identity.
        if version < 105 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            tx.execute_batch(
                "CREATE TABLE pending_question_publications (
                    session_id TEXT PRIMARY KEY NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                    publication_id TEXT NOT NULL UNIQUE,
                    epoch INTEGER NOT NULL CHECK(epoch>0),
                    state TEXT NOT NULL CHECK(state IN ('unresolved','published','cleared')),
                    question_json TEXT,
                    conversation_event_id INTEGER REFERENCES conversation_events(id) ON DELETE CASCADE,
                    event_sequence INTEGER,
                    tool_use_id TEXT,
                    model_invocation_id TEXT REFERENCES model_invocations(id) ON DELETE CASCADE,
                    updated_at TEXT NOT NULL,
                    CHECK(state<>'published' OR
                        (question_json IS NOT NULL AND json_valid(question_json)
                         AND conversation_event_id IS NOT NULL AND event_sequence IS NOT NULL
                         AND tool_use_id IS NOT NULL AND length(tool_use_id)>0))
                 );",
            )?;
            tx.execute("PRAGMA user_version = 105", [])?;
            tx.commit()?;
        }

        // V106: exact native AppServer approval publication. Existing approvals
        // remain displayable, but receive no inferred provider reply identity.
        if version < 106 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            tx.execute_batch(
                "CREATE TABLE pending_appserver_approvals (
                    session_id TEXT PRIMARY KEY NOT NULL REFERENCES sessions(id),
                    publication_id TEXT NOT NULL UNIQUE,
                    incarnation_id TEXT NOT NULL,
                    approval_id TEXT NOT NULL REFERENCES approvals(id),
                    state TEXT NOT NULL CHECK(state IN ('unresolved','published','enqueued','expired')),
                    target_json TEXT NOT NULL CHECK(json_valid(target_json)),
                    outcome TEXT,
                    updated_at TEXT NOT NULL
                 );
                 CREATE INDEX pending_appserver_approvals_live ON pending_appserver_approvals(state,updated_at);",
            )?;
            tx.execute("PRAGMA user_version = 106", [])?;
            tx.commit()?;
        }

        // V107: independent native approval occurrences. V106 remains intact as
        // historical evidence; migration cannot recover a live writer lease.
        // Request closure is separate from answer enqueue/consumption evidence.
        if version < 107 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            tx.execute_batch(
                "CREATE TABLE appserver_approval_publications (
                    publication_id TEXT PRIMARY KEY NOT NULL,
                    session_id TEXT NOT NULL REFERENCES sessions(id),
                    incarnation_id TEXT NOT NULL,
                    request_id_json TEXT NOT NULL CHECK(json_valid(request_id_json)),
                    thread_id TEXT,
                    approval_id TEXT NOT NULL REFERENCES approvals(id),
                    state TEXT NOT NULL CHECK(state IN ('unresolved','published','enqueued','expired','superseded')),
                    closure_state TEXT NOT NULL DEFAULT 'open' CHECK(closure_state IN ('open','closed','ambiguous')),
                    target_json TEXT NOT NULL CHECK(json_valid(target_json)),
                    closure_json TEXT CHECK(closure_json IS NULL OR json_valid(closure_json)),
                    outcome TEXT,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    closed_at TEXT
                 );
                 CREATE INDEX appserver_approval_publications_session
                    ON appserver_approval_publications(session_id,publication_id);
                 CREATE INDEX appserver_approval_publications_request
                    ON appserver_approval_publications(session_id,incarnation_id,request_id_json,created_at);
                 CREATE UNIQUE INDEX appserver_approval_publications_current
                    ON appserver_approval_publications(session_id,incarnation_id,request_id_json)
                    WHERE state IN ('unresolved','published','enqueued') AND closure_state <> 'closed';
                 CREATE INDEX appserver_approval_publications_live
                    ON appserver_approval_publications(state,closure_state,updated_at);
                 INSERT INTO appserver_approval_publications
                    (publication_id,session_id,incarnation_id,request_id_json,thread_id,
                     approval_id,state,closure_state,target_json,outcome,created_at,updated_at)
                    SELECT publication_id,session_id,incarnation_id,
                        COALESCE(target_json -> '$.request_id','null'),
                        json_extract(target_json,'$.params.threadId'),approval_id,'expired','open',
                        target_json,'V106 historical publication; live writer unavailable; prior state: ' || state,
                        updated_at,updated_at
                    FROM pending_appserver_approvals;",
            )?;
            tx.execute("PRAGMA user_version = 107", [])?;
            tx.commit()?;
        }

        // V108: current answer queues and metadata accounting remain indexed
        // as retained native decision history grows. No historical row is removed.
        if version < 108 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            tx.execute_batch(
                "CREATE INDEX harness_manager_v2_metadata_budget
                    ON harness_manager_v2_records(project_id,manager_session_id,scope_version)
                    WHERE NOT (kind='decision_generation' OR (kind IN ('decision','decision_target') AND substr(record_key,1,9)='approval:') OR (kind='decision_delivery' AND json_extract(payload_json,'$.target.kind') IS 'appserver_approval') OR (kind='decision_retrieval' AND (substr(record_key,1,17)='manager:approval:' OR substr(record_key,1,14)='lead:approval:')));
                 CREATE INDEX harness_manager_v2_queued_answers
                    ON harness_manager_v2_records(project_id,manager_session_id,scope_version,record_key)
                    WHERE kind='decision_delivery' AND json_extract(payload_json,'$.state')='queued';
                 CREATE INDEX harness_manager_v2_running_answers
                    ON harness_manager_v2_records(project_id,record_key)
                    WHERE kind='decision_delivery' AND json_extract(payload_json,'$.state')='running';
                 CREATE INDEX harness_manager_v2_operator_inbox
                    ON harness_manager_v2_records(project_id,manager_session_id,scope_version,epic_id,record_key)
                    WHERE kind='decision' AND json_extract(payload_json,'$.delivery.state')='available_in_scoped_inbox';",
            )?;
            tx.execute("PRAGMA user_version = 108", [])?;
            tx.commit()?;
        }

        // V109: bounded legacy approval candidates and exact native mirror
        // exclusion. Both indexes preserve all historical approval evidence.
        if version < 109 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            tx.execute_batch(
                "CREATE INDEX manager_pending_approval_scan ON approvals(session_id,id) WHERE status='Pending';
                 CREATE INDEX manager_native_approval_mirror ON appserver_approval_publications(approval_id);",
            )?;
            tx.execute("PRAGMA user_version = 109", [])?;
            tx.commit()?;
        }

        // V110: persist operator-selected Groups and project-wide scope. Legacy
        // appointments remain explicit, including empty/revoked appointments.
        if version < 110 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            tx.execute_batch(
                "ALTER TABLE harness_manager_scopes ADD COLUMN scope_mode TEXT NOT NULL DEFAULT 'selected' CHECK(scope_mode IN ('selected','project'));
                 ALTER TABLE harness_manager_scopes ADD COLUMN group_ids_json TEXT NOT NULL DEFAULT '[]' CHECK(json_valid(group_ids_json) AND json_type(group_ids_json)='array');
                 CREATE INDEX manager_project_scope_candidates ON sessions(project_id,session_kind,id);",
            )?;
            tx.execute("PRAGMA user_version = 110", [])?;
            tx.commit()?;
        }

        // V111: root-manager succession has a durable occurrence, independent
        // authority epoch and retained accounting. Existing receipt resolution
        // remains the sole current-manager authority pointer.
        if version < 111 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            tx.execute_batch(
                "CREATE TABLE manager_authority_epochs (
                    project_id TEXT PRIMARY KEY REFERENCES projects(id),
                    epoch INTEGER NOT NULL CHECK(typeof(epoch)='integer' AND epoch>0)
                 );
                 INSERT INTO manager_authority_epochs SELECT id,1 FROM projects;
                 CREATE TRIGGER manager_epoch_project AFTER INSERT ON projects BEGIN
                    INSERT INTO manager_authority_epochs VALUES(NEW.id,1);
                 END;
                 CREATE TRIGGER manager_epoch_scope AFTER UPDATE ON harness_manager_scopes BEGIN
                    UPDATE manager_authority_epochs SET epoch=epoch+1 WHERE project_id=NEW.project_id;
                 END;
                 CREATE TRIGGER manager_epoch_scope_insert AFTER INSERT ON harness_manager_scopes BEGIN
                    UPDATE manager_authority_epochs SET epoch=epoch+1 WHERE project_id=NEW.project_id;
                 END;
                 CREATE TRIGGER manager_epoch_edge AFTER INSERT ON harness_manager_rotation_edges BEGIN
                    UPDATE manager_authority_epochs SET epoch=epoch+1 WHERE project_id=(SELECT project_id FROM sessions WHERE id=NEW.predecessor_session_id);
                 END;
                 CREATE TRIGGER manager_epoch_retire AFTER UPDATE OF retired_at ON harness_manager_rotation_edges
                 WHEN OLD.retired_at IS NULL AND NEW.retired_at IS NOT NULL BEGIN
                    UPDATE manager_authority_epochs SET epoch=epoch+1 WHERE project_id=(SELECT project_id FROM sessions WHERE id=NEW.predecessor_session_id);
                 END;
                 CREATE TRIGGER manager_epoch_restore AFTER UPDATE OF status ON sessions
                 WHEN OLD.status='Archived' AND NEW.status<>'Archived' BEGIN
                    UPDATE manager_authority_epochs SET epoch=epoch+1 WHERE project_id=NEW.project_id;
                 END;
                 CREATE TABLE manager_root_successions (
                    operation_id TEXT PRIMARY KEY REFERENCES harness_manager_v2_operations(id),
                    project_id TEXT NOT NULL REFERENCES projects(id),
                    manager_session_id TEXT NOT NULL REFERENCES sessions(id),
                    predecessor_session_id TEXT NOT NULL REFERENCES sessions(id),
                    candidate_session_id TEXT NOT NULL UNIQUE,
                    launch_attempt_id TEXT NOT NULL UNIQUE,
                    model_invocation_id TEXT NOT NULL UNIQUE,
                    scope_version INTEGER NOT NULL CHECK(scope_version>0),
                    policy_version INTEGER NOT NULL CHECK(policy_version>0),
                    authority_epoch INTEGER NOT NULL CHECK(authority_epoch>0),
                    creation_quantity INTEGER NOT NULL DEFAULT 1 CHECK(creation_quantity=1),
                    recovery_quantity INTEGER NOT NULL DEFAULT 0 CHECK(recovery_quantity=0),
                    frozen_json TEXT NOT NULL CHECK(json_valid(frozen_json) AND length(frozen_json)<=65536),
                    state TEXT NOT NULL CHECK(state IN ('reserved','executing','established','cleanup_required','committed','failed','blocked','revoked')),
                    row_version INTEGER NOT NULL CHECK(typeof(row_version)='integer' AND row_version>0),
                    claim_boot_id TEXT,
                    settled_json TEXT CHECK(settled_json IS NULL OR json_valid(settled_json)),
                    candidate_json TEXT CHECK(candidate_json IS NULL OR json_valid(candidate_json)),
                    admission_recorded INTEGER NOT NULL DEFAULT 0 CHECK(admission_recorded IN (0,1)),
                    effect_claimed INTEGER NOT NULL DEFAULT 0 CHECK(effect_claimed IN (0,1)),
                    establishment_json TEXT CHECK(establishment_json IS NULL OR json_valid(establishment_json)),
                    published_epoch INTEGER CHECK(published_epoch>0),
                    reason TEXT,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    CHECK(candidate_session_id<>predecessor_session_id)
                 );
                 CREATE UNIQUE INDEX manager_root_unresolved ON manager_root_successions(project_id,manager_session_id)
                    WHERE state IN ('reserved','executing','established','cleanup_required');
                 CREATE INDEX manager_root_recovery ON manager_root_successions(state,operation_id);
                 CREATE TRIGGER manager_root_immutable BEFORE UPDATE ON manager_root_successions
                 WHEN NEW.operation_id IS NOT OLD.operation_id OR NEW.project_id IS NOT OLD.project_id
                    OR NEW.manager_session_id IS NOT OLD.manager_session_id OR NEW.predecessor_session_id IS NOT OLD.predecessor_session_id
                    OR NEW.candidate_session_id IS NOT OLD.candidate_session_id OR NEW.launch_attempt_id IS NOT OLD.launch_attempt_id
                    OR NEW.model_invocation_id IS NOT OLD.model_invocation_id OR NEW.scope_version IS NOT OLD.scope_version
                    OR NEW.policy_version IS NOT OLD.policy_version OR NEW.authority_epoch IS NOT OLD.authority_epoch
                    OR NEW.creation_quantity IS NOT OLD.creation_quantity OR NEW.recovery_quantity IS NOT OLD.recovery_quantity
                    OR NEW.frozen_json IS NOT OLD.frozen_json OR NEW.created_at IS NOT OLD.created_at
                    OR NEW.admission_recorded<OLD.admission_recorded OR NEW.effect_claimed<OLD.effect_claimed
                    OR (OLD.settled_json IS NOT NULL AND NEW.settled_json IS NOT OLD.settled_json)
                    OR (OLD.candidate_json IS NOT NULL AND NEW.candidate_json IS NOT OLD.candidate_json)
                    OR (OLD.establishment_json IS NOT NULL AND NEW.establishment_json IS NOT OLD.establishment_json)
                    OR (OLD.published_epoch IS NOT NULL AND NEW.published_epoch IS NOT OLD.published_epoch)
                    OR NEW.row_version<>OLD.row_version+1
                 BEGIN SELECT RAISE(ABORT,'manager_root_immutable'); END;
                 CREATE TRIGGER manager_root_state BEFORE UPDATE OF state ON manager_root_successions
                 WHEN NOT (NEW.state=OLD.state OR
                    (OLD.state='reserved' AND NEW.state IN ('executing','blocked','revoked','failed')) OR
                    (OLD.state='executing' AND NEW.state='reserved'
                     AND OLD.claim_boot_id IS NOT NULL AND NEW.claim_boot_id IS NULL
                     AND OLD.admission_recorded=0 AND NEW.admission_recorded=0
                     AND OLD.effect_claimed=0 AND NEW.effect_claimed=0
                     AND OLD.settled_json IS NULL AND NEW.settled_json IS NULL
                     AND OLD.candidate_json IS NULL AND NEW.candidate_json IS NULL
                     AND OLD.establishment_json IS NULL AND NEW.establishment_json IS NULL
                     AND OLD.published_epoch IS NULL AND NEW.published_epoch IS NULL) OR
                    (OLD.state='executing' AND NEW.state IN ('established','cleanup_required','blocked','revoked','failed')) OR
                    (OLD.state='established' AND NEW.state IN ('committed','cleanup_required')) OR
                    (OLD.state='cleanup_required' AND NEW.state IN ('failed','revoked')))
                 BEGIN SELECT RAISE(ABORT,'manager_root_state'); END;
                 CREATE TRIGGER manager_root_no_delete BEFORE DELETE ON manager_root_successions
                 BEGIN SELECT RAISE(ABORT,'manager_root_retained'); END;
                 CREATE TABLE manager_root_transitions (
                    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                    operation_id TEXT NOT NULL REFERENCES manager_root_successions(operation_id),
                    row_version INTEGER NOT NULL,
                    state TEXT NOT NULL,
                    reason TEXT,
                    created_at TEXT NOT NULL,
                    UNIQUE(operation_id,row_version)
                 );
                 CREATE TRIGGER manager_root_transition_insert AFTER INSERT ON manager_root_successions BEGIN
                    INSERT INTO manager_root_transitions(operation_id,row_version,state,reason,created_at)
                    VALUES(NEW.operation_id,NEW.row_version,NEW.state,NEW.reason,NEW.updated_at);
                 END;
                 CREATE TRIGGER manager_root_transition_update AFTER UPDATE ON manager_root_successions BEGIN
                    INSERT INTO manager_root_transitions(operation_id,row_version,state,reason,created_at)
                    VALUES(NEW.operation_id,NEW.row_version,NEW.state,NEW.reason,NEW.updated_at);
                 END;
                 CREATE TRIGGER manager_root_transition_no_update BEFORE UPDATE ON manager_root_transitions
                 BEGIN SELECT RAISE(ABORT,'manager_root_audit_retained'); END;
                 CREATE TRIGGER manager_root_transition_no_delete BEFORE DELETE ON manager_root_transitions
                 BEGIN SELECT RAISE(ABORT,'manager_root_audit_retained'); END;
                 CREATE TABLE manager_root_resource_origins (
                    session_id TEXT PRIMARY KEY,
                    project_id TEXT NOT NULL REFERENCES projects(id),
                    manager_session_id TEXT NOT NULL REFERENCES sessions(id),
                    scope_version INTEGER NOT NULL CHECK(scope_version>0),
                    operation_id TEXT UNIQUE REFERENCES manager_root_successions(operation_id),
                    predecessor_session_id TEXT REFERENCES sessions(id),
                    model_invocation_id TEXT UNIQUE,
                    provider TEXT NOT NULL,
                    zero_origin INTEGER NOT NULL CHECK(zero_origin IN (0,1)),
                    known_floor_usd REAL NOT NULL CHECK(known_floor_usd>=0),
                    created_at TEXT NOT NULL,
                    CHECK((operation_id IS NULL AND zero_origin=0) OR (operation_id IS NOT NULL AND model_invocation_id IS NOT NULL))
                 );
                 CREATE INDEX manager_root_resource_project ON manager_root_resource_origins(project_id,session_id);
                 CREATE TRIGGER manager_root_resource_immutable BEFORE UPDATE ON manager_root_resource_origins
                 WHEN NEW.session_id IS NOT OLD.session_id OR NEW.project_id IS NOT OLD.project_id
                    OR NEW.manager_session_id IS NOT OLD.manager_session_id OR NEW.scope_version IS NOT OLD.scope_version
                    OR NEW.operation_id IS NOT OLD.operation_id OR NEW.predecessor_session_id IS NOT OLD.predecessor_session_id
                    OR NEW.model_invocation_id IS NOT OLD.model_invocation_id OR NEW.provider IS NOT OLD.provider
                    OR NEW.zero_origin IS NOT OLD.zero_origin OR NEW.known_floor_usd<OLD.known_floor_usd OR NEW.created_at IS NOT OLD.created_at
                 BEGIN SELECT RAISE(ABORT,'manager_root_resource_immutable'); END;
                 CREATE TRIGGER manager_root_resource_no_delete BEFORE DELETE ON manager_root_resource_origins
                 BEGIN SELECT RAISE(ABORT,'manager_root_resource_retained'); END;",
            )?;
            tx.execute("PRAGMA user_version = 111", [])?;
            tx.commit()?;
        }

        // V111 -> V112 precondition. The pinned V112 identity backfill models
        // an Epic child lineage as linear and aborts on a branch. Committed
        // receipts prove branches are real (two logical spawn reservations
        // sharing a predecessor), so converge them here, at V111, before V112
        // reads them. Data-only: this creates no schema object and never
        // writes PRAGMA user_version, so the V112 catalog fingerprint gate
        // still authenticates the source it was reviewed against.
        if version == 111 {
            self.normalize_v111_branched_epic_lineage()?;
        }

        // V112: converge the exact released target V111 catalog and the
        // historical Slice 2A data-bearing V111 catalog without rewriting
        // any released V0-V111 migration.
        if version < 112 {
            self.apply_operator_views_v112_migration()?;
        }

        // V113: add the durable stable-identity archive success projection
        // after both V111 lineages have converged.
        if version < 113 {
            self.apply_archive_projection_v113_migration()?;
        }

        // V114: durable, receipt-validated detachment receipts for the Epic
        // lineages the V111 branch normalization re-rooted, so manager
        // authority can still prove the original attribution from a
        // constrained relation rather than from an erased column.
        if version < 114 {
            self.apply_lineage_detachment_v114_migration()?;
        }

        // V114 -> V115 precondition. Every migration from V115 onward
        // authenticates its source with an exact full-sqlite_master
        // fingerprint, which hashes each object's stored SQL text. A database
        // deployed before `sessions.provider` and `workflows.definition_json`
        // were folded into their base CREATE TABLE carries them as trailing
        // ALTER TABLE columns: same schema, different text, different
        // fingerprint. Rebuild those two tables onto the canonical catalog here
        // so the pinned V115/V116 drivers authenticate the database instead of
        // refusing it. No-op when the catalog is already canonical.
        if version == 114 {
            self.converge_v114_deployed_additive_catalog()?;
        }

        // V115: retained, authority-bound prepared manager actions. Transport
        // and admission APIs are introduced separately from this journal.
        if version < 115 {
            self.apply_manager_prepared_actions_v115_migration()?;
        }

        // V116: preserve the V115 journal while allowing an authority-
        // preserving manager lineage tip to remain the attributed caller.
        if version < 116 {
            self.apply_manager_prepared_actions_v116_migration()?;
        }

        // V117: exact, retained harness-manager notice subjects and explicit
        // recorded/delivered/retrieved/settled lifecycle evidence.
        if version < 117 {
            self.apply_manager_notices_v117_migration()?;
        }

        // V118: bound obsolete-route retirement by job and route before sequence.
        if version < 118 {
            self.apply_manager_notice_retirement_v118_migration()?;
        }

        // V119: durable finite target-reclaim sweep and bounded selection indexes.
        if version < 119 {
            self.apply_target_reclaim_sweep_v119_migration()?;
        }

        // V120: bounded source-worktree batch/dependency substrate. This is
        // additive; V1 settlement receipts and all released V119 catalog
        // objects remain historical inputs.
        if version < 120 {
            self.apply_source_worktree_v120_migration()?;
        }

        // V121: exact-source manager review assignments and immutable,
        // daemon-attributed reviewer receipts. Legacy evidence stays readable.
        if version < 121 {
            self.apply_manager_review_v121_migration()?;
        }

        // V122: durable manager ledger facts keyed by the work, never the seat
        // (decision D19). Carries the newest row per identity forward; the
        // seat-scoped V2 records stay as historical input and in-flight state.
        if version < 122 {
            manager_ledger::apply_work_facts_migration(self)?;
        }

        // V123: bound terminal-watch restart repair's historical owner/child
        // lookup by its natural key while preserving every V122 catalog pin.
        if version < 123 {
            agent_coordination::watch_repair_v123::apply_v123_migration(self)?;
        }

        // V124: live-scope manager record indexes so bookkeeping sweeps and
        // budget counts never range over archived history (Issue #643). The
        // literal must equal
        // harness_manager_v2::MANAGER_V2_LIVE_BOOKKEEPING_INDEX_VERSION.
        if version < 124 {
            harness_manager_v2::apply_live_bookkeeping_index_migration(self)?;
        }

        // V125: rebuild the V97 `issue_events` audit table so an
        // IssueCoordinate manager mutation is recorded with a truthful
        // `manager` actor and no owning Epic (Issue #639). The literal must
        // equal issues::manager_actor_migration::MANAGER_ACTOR_VERSION.
        if version < 125 {
            issues::manager_actor_migration::apply_manager_actor_migration(self)?;
        }

        // V126: admit Bedrock and previously omitted OpenRouter in recursive
        // live attempt provider snapshots. Keep V93 DDL immutable.
        if version < 126 {
            self.conn.execute_batch("PRAGMA foreign_keys=OFF;")?;
            let outcome = self.apply_bedrock_v126_migration();
            self.conn.execute_batch("PRAGMA foreign_keys=ON;")?;
            outcome?;
        }

        // V127: persist bounded session-attributed daemon diagnostics.
        if version < 127 {
            session_diagnostics::apply_v127_migration(self)?;
        }

        // V128: immutable, durable evidence of watchdog-triggered restarts.
        if version < 128 {
            crate::daemon_restart_persistence::apply_v128_migration(self)?;
        }

        // V129: durable topology executions, node attempts and events (#634).
        // Additive only.
        if version < 129 {
            topology_v129::apply_v129_migration(self)?;
        }

        // V130: append-only exact-request journal for agent child fresh relaunch.
        if version < 130 {
            agent_child_relaunch_intents::apply_v130_migration(self)?;
        }

        // V131: durable absent-root sandbox reclaim journal (Epic R R-a1).
        // RSI-RELEASED-MIGRATION-BEGIN: v131-sandbox-reclaim-journal-driver
        if version < 131 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            sandbox_reclaim::apply_sandbox_reclaim_journal_migration(&tx)?;
            tx.execute("PRAGMA user_version = 131", [])?;
            tx.commit()?;
        }
        // RSI-RELEASED-MIGRATION-END: v131-sandbox-reclaim-journal-driver

        // V132: persist operator model/effort selections for the next turn.
        if version < 132 {
            session_model_updates::apply_v132_migration(self)?;
        }
        // V120 definitions and the internal cursor key are authenticated on
        // every reopen, before any projection reconciliation can write.
        source_worktree_v120::validate_v120_catalog(&self.conn)?;
        manager_review_v121::validate_v121_catalog(&self.conn)?;
        agent_coordination::watch_repair_v123::validate_v123_catalog(&self.conn)?;
        crate::daemon_restart_persistence::validate_v128_catalog(&self.conn)?;
        topology_v129::validate_v129_catalog(&self.conn)?;

        // Path evidence is never assumed from the raw scheduled-job field.
        // Reconciliation is bounded and leaves malformed/missing rows
        // unverified for later dependency proof to retain rather than ignore.
        source_worktree_v120::reconcile_scheduled_job_path_projections(&self.conn)?;

        Ok(())
    }

    // RSI-RELEASED-MIGRATION-BEGIN: v94-migration-driver
    fn apply_source_worktree_settlement_v94_migration(&self) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if active_version != 93 {
            return Err(DaemonError::Store(format!(
                "V94 requires exact V93 source, found V{active_version}"
            )));
        }
        h1_v94_validate_v93_catalog(&tx)?;
        cohort_settlement::v94_migration_fault(
            cohort_settlement::SourceWorktreeSettlementV94MigrationFault::AfterPreflight,
        )?;
        cohort_settlement::install_v94_schema(&tx)?;
        cohort_settlement::v94_migration_fault(
            cohort_settlement::SourceWorktreeSettlementV94MigrationFault::AfterSchema,
        )?;
        cohort_settlement::validate_v94_catalog(&tx)?;
        let integrity: String = tx.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        if integrity != "ok" {
            return Err(DaemonError::Store(format!(
                "V94 settlement migration requires integrity_check=ok, got {integrity}"
            )));
        }
        let foreign_key_errors: i64 =
            tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })?;
        if foreign_key_errors != 0 {
            return Err(DaemonError::Store(format!(
                "V94 settlement migration found {foreign_key_errors} foreign-key violation(s)"
            )));
        }
        cohort_settlement::v94_migration_fault(
            cohort_settlement::SourceWorktreeSettlementV94MigrationFault::AfterChecks,
        )?;
        tx.execute("PRAGMA user_version = 94", [])?;
        cohort_settlement::v94_migration_fault(
            cohort_settlement::SourceWorktreeSettlementV94MigrationFault::AfterUserVersion,
        )?;
        cohort_settlement::v94_migration_fault(
            cohort_settlement::SourceWorktreeSettlementV94MigrationFault::BeforeCommit,
        )?;
        tx.commit()?;
        tracing::info!("V94 migration complete: source-worktree settlement journal");
        Ok(())
    }
    // RSI-RELEASED-MIGRATION-END: v94-migration-driver

    // RSI-RELEASED-MIGRATION-BEGIN: v95-migration-driver
    fn apply_source_worktree_settlement_v95_migration(&self) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if active_version != 94 {
            return Err(DaemonError::Store(format!(
                "V95 requires exact V94 source, found V{active_version}"
            )));
        }
        let v94_catalog = cohort_settlement::classify_v94_settlement_catalog(&tx)?;
        if v94_catalog == cohort_settlement::V94SettlementCatalog::Deployed {
            h1_v95_validate_v94_catalog(&tx)?;
        }
        let integrity: String = tx.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        if integrity != "ok" {
            return Err(DaemonError::Store(format!(
                "V95 requires integrity_check=ok before hardening, got {integrity}"
            )));
        }
        let foreign_key_errors: i64 =
            tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })?;
        if foreign_key_errors != 0 {
            return Err(DaemonError::Store(format!(
                "V95 preflight found {foreign_key_errors} foreign-key violation(s)"
            )));
        }
        cohort_settlement::validate_v95_source_rows(&tx)?;
        if v94_catalog == cohort_settlement::V94SettlementCatalog::Deployed {
            cohort_settlement::validate_deployed_v94_rows_for_current_target(&tx)?;
            cohort_settlement::bridge_deployed_v94_catalog(&tx)?;
            h1_v95_validate_v94_catalog(&tx)?;
        } else {
            h1_v95_validate_v94_catalog(&tx)?;
        }
        cohort_settlement::v95_migration_fault(
            cohort_settlement::SourceWorktreeSettlementV95MigrationFault::AfterPreflight,
        )?;
        cohort_settlement::drop_v95_replaceable_triggers(&tx)?;
        cohort_settlement::v95_migration_fault(
            cohort_settlement::SourceWorktreeSettlementV95MigrationFault::AfterTriggerDrop,
        )?;
        cohort_settlement::normalize_v95_runs(&tx)?;
        cohort_settlement::v95_migration_fault(
            cohort_settlement::SourceWorktreeSettlementV95MigrationFault::AfterNormalize,
        )?;
        cohort_settlement::install_v95_run_order(&tx)?;
        cohort_settlement::v95_migration_fault(
            cohort_settlement::SourceWorktreeSettlementV95MigrationFault::AfterRunOrder,
        )?;
        cohort_settlement::install_v95_latest_run_pointer(&tx)?;
        cohort_settlement::v95_migration_fault(
            cohort_settlement::SourceWorktreeSettlementV95MigrationFault::AfterLatestRunPointer,
        )?;
        cohort_settlement::install_v95_indexes(&tx)?;
        cohort_settlement::v95_migration_fault(
            cohort_settlement::SourceWorktreeSettlementV95MigrationFault::AfterIndexes,
        )?;
        cohort_settlement::install_v95_triggers(&tx)?;
        cohort_settlement::v95_migration_fault(
            cohort_settlement::SourceWorktreeSettlementV95MigrationFault::AfterTriggers,
        )?;
        cohort_settlement::validate_v95_catalog(&tx)?;
        let integrity: String = tx.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        if integrity != "ok" {
            return Err(DaemonError::Store(format!(
                "V95 settlement hardening requires integrity_check=ok, got {integrity}"
            )));
        }
        let foreign_key_errors: i64 =
            tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })?;
        if foreign_key_errors != 0 {
            return Err(DaemonError::Store(format!(
                "V95 settlement hardening found {foreign_key_errors} foreign-key violation(s)"
            )));
        }
        cohort_settlement::v95_migration_fault(
            cohort_settlement::SourceWorktreeSettlementV95MigrationFault::AfterChecks,
        )?;
        tx.execute("PRAGMA user_version = 95", [])?;
        cohort_settlement::v95_migration_fault(
            cohort_settlement::SourceWorktreeSettlementV95MigrationFault::AfterUserVersion,
        )?;
        cohort_settlement::v95_migration_fault(
            cohort_settlement::SourceWorktreeSettlementV95MigrationFault::BeforeCommit,
        )?;
        tx.commit()?;
        tracing::info!("V95 migration complete: source-worktree settlement hardening");
        Ok(())
    }
    // RSI-RELEASED-MIGRATION-END: v95-migration-driver

    fn apply_issue_v97_migration(&self) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if active_version != 96 {
            return Err(DaemonError::Store(format!(
                "V97 requires exact V96 source, found V{active_version}"
            )));
        }
        cohort_settlement::validate_v95_catalog(&tx)?;
        validate_v96_tool_correlation_catalog(&tx)?;
        let witness = issues::validate_v96_issue_source_catalog(&tx)?;
        issue_v97_migration_fault(IssueV97MigrationFault::AfterPreflight)?;
        issues::add_v97_issue_columns(&tx)?;
        issue_v97_migration_fault(IssueV97MigrationFault::AfterIssueColumns)?;
        issues::create_v97_issue_event_table(&tx)?;
        issue_v97_migration_fault(IssueV97MigrationFault::AfterEventTable)?;
        issues::backfill_v97_issue_events(&tx)?;
        issue_v97_migration_fault(IssueV97MigrationFault::AfterBaselineCopy)?;
        issues::create_v97_issue_indexes(&tx)?;
        issue_v97_migration_fault(IssueV97MigrationFault::AfterIndexes)?;
        issues::create_v97_issue_triggers(&tx)?;
        issue_v97_migration_fault(IssueV97MigrationFault::AfterTriggers)?;
        issues::validate_v97_catalog(&tx, &witness)?;
        issue_v97_migration_fault(IssueV97MigrationFault::AfterParity)?;
        let integrity: String = tx.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        if integrity != "ok" {
            return Err(DaemonError::Store(format!(
                "V97 Issue migration requires integrity_check=ok, got {integrity}"
            )));
        }
        let foreign_key_errors: i64 =
            tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })?;
        if foreign_key_errors != 0 {
            return Err(DaemonError::Store(format!(
                "V97 Issue migration found {foreign_key_errors} foreign-key violation(s)"
            )));
        }
        issue_v97_migration_fault(IssueV97MigrationFault::AfterChecks)?;
        tx.execute("PRAGMA user_version = 97", [])?;
        issue_v97_migration_fault(IssueV97MigrationFault::AfterUserVersion)?;
        issue_v97_migration_fault(IssueV97MigrationFault::BeforeCommit)?;
        tx.commit()?;
        tracing::info!("V97 migration complete: guarded Issue audit ledger");
        Ok(())
    }

    /// Preserve a unique filesystem allocation identity for each custody root.
    /// Historical roots are immutable, so restoring a purged session must be
    /// able to allocate a distinct path while retaining the same Session id.
    fn apply_sandbox_allocation_identity_v98_migration(&self) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if active_version != 97 {
            return Err(DaemonError::Store(format!(
                "V98 requires exact V97 source, found V{active_version}"
            )));
        }
        add_column_if_not_exists_tx(&tx, "sandbox_custody_roots", "allocation_id", "TEXT")?;
        tx.execute(
            "UPDATE sandbox_custody_roots
             SET allocation_id=(
                 SELECT e.to_owner_session_id
                 FROM sandbox_custody_events e
                 WHERE e.custody_id=sandbox_custody_roots.custody_id
                   AND e.sequence=1
                   AND e.event_kind='allocated'
             )
             WHERE allocation_id IS NULL",
            [],
        )?;
        let missing: i64 = tx.query_row(
            "SELECT count(*) FROM sandbox_custody_roots
             WHERE allocation_id IS NULL OR length(trim(allocation_id))=0",
            [],
            |row| row.get(0),
        )?;
        if missing != 0 {
            return Err(DaemonError::Store(format!(
                "V98 allocation identity migration found {missing} root(s) without an allocation id"
            )));
        }
        tx.execute_batch(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_sandbox_custody_roots_allocation_id
                 ON sandbox_custody_roots(allocation_id);",
        )?;
        let integrity: String = tx.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        if integrity != "ok" {
            return Err(DaemonError::Store(format!(
                "V98 allocation identity migration requires integrity_check=ok, got {integrity}"
            )));
        }
        let foreign_key_errors: i64 =
            tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })?;
        if foreign_key_errors != 0 {
            return Err(DaemonError::Store(format!(
                "V98 allocation identity migration found {foreign_key_errors} foreign-key violation(s)"
            )));
        }
        tx.execute("PRAGMA user_version = 98", [])?;
        tx.commit()?;
        tracing::info!("V98 migration complete: sandbox allocation identities");
        Ok(())
    }

    // RSI-RELEASED-MIGRATION-BEGIN: v100-context-window-provenance
    fn apply_context_window_provenance_v100_migration(&self) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if active_version != 99 {
            return Err(DaemonError::Store(format!(
                "V100 requires exact V99 source, found V{active_version}"
            )));
        }
        tx.execute_batch(
            "ALTER TABLE sessions ADD COLUMN context_window_source TEXT
                 CHECK (context_window_source IS NULL OR context_window_source IN (
                     'official_documentation', 'provider_catalog', 'configured',
                     'runtime_telemetry', 'repository_fallback', 'legacy_unverified'
                 ));
             ALTER TABLE sessions ADD COLUMN context_window_source_version TEXT;
             ALTER TABLE sessions ADD COLUMN context_window_source_digest TEXT;
             ALTER TABLE sessions ADD COLUMN context_window_observed_at TEXT;
             UPDATE sessions
                SET context_window_source='legacy_unverified'
              WHERE context_window IS NOT NULL;",
        )?;
        let integrity: String = tx.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        if integrity != "ok" {
            return Err(DaemonError::Store(format!(
                "V100 context-window provenance migration requires integrity_check=ok, got {integrity}"
            )));
        }
        let foreign_key_errors: i64 =
            tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })?;
        if foreign_key_errors != 0 {
            return Err(DaemonError::Store(format!(
                "V100 context-window provenance migration found {foreign_key_errors} foreign-key violation(s)"
            )));
        }
        tx.execute("PRAGMA user_version = 100", [])?;
        tx.commit()?;
        tracing::info!("V100 migration complete: context-window provenance");
        Ok(())
    }
    // RSI-RELEASED-MIGRATION-END: v100-context-window-provenance

    // RSI-RELEASED-MIGRATION-BEGIN: v101-configured-context-window
    fn apply_configured_context_window_v101_migration(&self) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if active_version != 100 {
            return Err(DaemonError::Store(format!(
                "V101 requires exact V100 source, found V{active_version}"
            )));
        }
        tx.execute_batch(
            "ALTER TABLE sessions ADD COLUMN context_window_configured_tokens INTEGER
                 CHECK (context_window_configured_tokens IS NULL OR
                        context_window_configured_tokens > 0);",
        )?;
        let integrity: String = tx.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        if integrity != "ok" {
            return Err(DaemonError::Store(format!(
                "V101 configured context-window migration requires integrity_check=ok, got {integrity}"
            )));
        }
        let foreign_key_errors: i64 =
            tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })?;
        if foreign_key_errors != 0 {
            return Err(DaemonError::Store(format!(
                "V101 configured context-window migration found {foreign_key_errors} foreign-key violation(s)"
            )));
        }
        tx.execute("PRAGMA user_version = 101", [])?;
        tx.commit()?;
        tracing::info!("V101 migration complete: configured context-window intent");
        Ok(())
    }
    // RSI-RELEASED-MIGRATION-END: v101-configured-context-window

    // RSI-RELEASED-MIGRATION-BEGIN: v112-operator-views-convergence-driver
    fn apply_operator_views_v112_migration(&self) -> Result<()> {
        #[derive(Clone)]
        struct LegacyChild {
            id: String,
            epic_id: String,
            continued_from: Option<String>,
            created_at: chrono::DateTime<chrono::Utc>,
        }

        #[derive(Clone)]
        struct LegacyRequest {
            id: String,
            child_id: String,
            epic_id: String,
            reserved_at: chrono::DateTime<chrono::Utc>,
        }

        struct IdentityEntity {
            request_id: Option<String>,
            tie_session_id: String,
            members: Vec<String>,
            sort_at: chrono::DateTime<chrono::Utc>,
        }

        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if active_version != 111 {
            return Err(DaemonError::Store(format!(
                "V112 requires exact V111 source, found V{active_version}"
            )));
        }
        let source_catalog = classify_v112_source_catalog(&tx)?;
        identity_v112_migration_fault(IdentityV112MigrationFault::AfterPreflight)?;

        let preserved_identity = if source_catalog == V112SourceCatalog::HistoricalOperatorViewsV111
        {
            let sessions = {
                let mut stmt = tx
                    .prepare("SELECT id,agent_role,epic_spawn_ordinal FROM sessions ORDER BY id")?;
                stmt.query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                    ))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?
            };
            let requests = {
                let mut stmt = tx.prepare(
                    "SELECT spawn_request_id,epic_spawn_ordinal
                     FROM agent_spawn_requests ORDER BY spawn_request_id",
                )?;
                stmt.query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?
            };
            let counters = {
                let mut stmt = tx.prepare(
                    "SELECT epic_id,next_ordinal FROM epic_spawn_counters ORDER BY epic_id",
                )?;
                stmt.query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?
            };
            tx.execute_batch(
                "DROP TRIGGER sessions_v100_identity_validate_insert;
                 DROP TRIGGER sessions_v100_identity_validate_update;
                 DROP TRIGGER sessions_v100_identity_immutable;
                 DROP TRIGGER agent_spawn_requests_v100_ordinal_validate_insert;
                 DROP TRIGGER agent_spawn_requests_v100_ordinal_validate_update;
                 DROP TRIGGER agent_spawn_requests_v100_ordinal_immutable;
                 DROP TRIGGER epic_spawn_counters_v100_validate_insert;
                 DROP TRIGGER epic_spawn_counters_v100_validate_update;
                 DROP TRIGGER epic_spawn_counters_v100_no_delete;
                 DROP INDEX idx_agent_spawn_requests_epic_ordinal;
                 DROP INDEX idx_sessions_parent_epic_ordinal;
                 DROP TABLE epic_spawn_counters;
                 ALTER TABLE agent_spawn_requests DROP COLUMN epic_spawn_ordinal;
                 ALTER TABLE sessions DROP COLUMN agent_role;
                 ALTER TABLE sessions DROP COLUMN epic_spawn_ordinal;",
            )?;
            install_v112_capability_columns(&tx)?;
            Some((sessions, requests, counters))
        } else {
            None
        };

        add_column_if_not_exists_tx(&tx, "sessions", "agent_role", "TEXT")?;
        add_column_if_not_exists_tx(&tx, "sessions", "epic_spawn_ordinal", "INTEGER")?;
        add_column_if_not_exists_tx(&tx, "agent_spawn_requests", "epic_spawn_ordinal", "INTEGER")?;
        tx.execute_batch(
            "CREATE TABLE epic_spawn_counters (
                 epic_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE RESTRICT,
                 next_ordinal INTEGER NOT NULL
                     CHECK(next_ordinal >= 1 AND next_ordinal <= 4294967296)
             );",
        )?;
        identity_v112_migration_fault(IdentityV112MigrationFault::AfterColumns)?;

        let epic_ids = {
            let mut stmt =
                tx.prepare("SELECT id FROM sessions WHERE session_kind='Epic' ORDER BY id")?;
            stmt.query_map([], |row| row.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        let epic_set = epic_ids
            .iter()
            .cloned()
            .collect::<std::collections::HashSet<_>>();

        let children = {
            let mut stmt = tx.prepare(
                "SELECT child.id,child.parent_id,child.continued_from,child.created_at
                 FROM sessions child
                 JOIN sessions epic ON epic.id=child.parent_id AND epic.session_kind='Epic'
                 ORDER BY child.parent_id,child.id",
            )?;
            stmt.query_map([], |row| {
                let created_raw: String = row.get(3)?;
                let created_at = parse_timestamp(&created_raw).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        3,
                        rusqlite::types::Type::Text,
                        Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, error)),
                    )
                })?;
                Ok(LegacyChild {
                    id: row.get(0)?,
                    epic_id: row.get(1)?,
                    continued_from: row.get(2)?,
                    created_at,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
        };
        let child_by_id = children
            .iter()
            .cloned()
            .map(|child| (child.id.clone(), child))
            .collect::<HashMap<_, _>>();

        let mut successor_counts = HashMap::<String, usize>::new();
        for child in &children {
            if let Some(predecessor) = &child.continued_from {
                let count = successor_counts.entry(predecessor.clone()).or_default();
                *count += 1;
                if *count > 1 {
                    return Err(DaemonError::Store(
                        "V99 identity backfill found a branched Epic lineage".into(),
                    ));
                }
            }
        }

        let mut root_by_member = HashMap::<String, String>::new();
        let mut members_by_root = HashMap::<String, Vec<String>>::new();
        for child in &children {
            let mut cursor = child.id.clone();
            let mut seen = std::collections::HashSet::new();
            let root = loop {
                if !seen.insert(cursor.clone()) {
                    return Err(DaemonError::Store(
                        "V99 identity backfill found an Epic lineage cycle".into(),
                    ));
                }
                let node = child_by_id.get(&cursor).ok_or_else(|| {
                    DaemonError::Store(
                        "V99 identity backfill found a missing Epic lineage member".into(),
                    )
                })?;
                if node.epic_id != child.epic_id {
                    return Err(DaemonError::Store(
                        "V99 identity backfill found a cross-Epic lineage".into(),
                    ));
                }
                match &node.continued_from {
                    Some(predecessor) => {
                        if !child_by_id.contains_key(predecessor) {
                            return Err(DaemonError::Store(
                                "V99 identity backfill found a missing Epic lineage predecessor"
                                    .into(),
                            ));
                        }
                        cursor = predecessor.clone();
                    }
                    None => break node.id.clone(),
                }
            };
            root_by_member.insert(child.id.clone(), root.clone());
            members_by_root
                .entry(root)
                .or_default()
                .push(child.id.clone());
        }

        let requests = {
            let mut stmt = tx.prepare(
                "SELECT spawn_request_id,child_session_id,epic_id,reserved_at
                 FROM agent_spawn_requests ORDER BY epic_id,spawn_request_id",
            )?;
            stmt.query_map([], |row| {
                let reserved_raw: String = row.get(3)?;
                let reserved_at = parse_timestamp(&reserved_raw).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        3,
                        rusqlite::types::Type::Text,
                        Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, error)),
                    )
                })?;
                Ok(LegacyRequest {
                    id: row.get(0)?,
                    child_id: row.get(1)?,
                    epic_id: row.get(2)?,
                    reserved_at,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
        };
        let mut request_by_root = HashMap::<String, LegacyRequest>::new();
        let mut request_only = Vec::new();
        for request in requests {
            if !epic_set.contains(&request.epic_id) {
                return Err(DaemonError::Store(
                    "V99 identity backfill found a reservation without an Epic".into(),
                ));
            }
            if let Some(root) = root_by_member.get(&request.child_id) {
                let child = child_by_id
                    .get(&request.child_id)
                    .expect("root member exists");
                if child.epic_id != request.epic_id {
                    return Err(DaemonError::Store(
                        "V99 identity backfill found a reservation in the wrong Epic".into(),
                    ));
                }
                if request_by_root.insert(root.clone(), request).is_some() {
                    return Err(DaemonError::Store(
                        "V99 identity backfill found multiple reservations for one lineage".into(),
                    ));
                }
            } else {
                let existing_child: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1)",
                    [&request.child_id],
                    |row| row.get(0),
                )?;
                if existing_child {
                    return Err(DaemonError::Store(
                        "V99 identity backfill found a reserved child outside its Epic".into(),
                    ));
                }
                request_only.push(request);
            }
        }

        let mut entities_by_epic = HashMap::<String, Vec<IdentityEntity>>::new();
        for (root, mut members) in members_by_root {
            members.sort();
            let root_row = child_by_id.get(&root).expect("lineage root exists");
            let request = request_by_root.remove(&root);
            entities_by_epic
                .entry(root_row.epic_id.clone())
                .or_default()
                .push(IdentityEntity {
                    sort_at: request
                        .as_ref()
                        .map_or(root_row.created_at, |item| item.reserved_at),
                    request_id: request.as_ref().map(|item| item.id.clone()),
                    tie_session_id: root.clone(),
                    members,
                });
        }
        if !request_by_root.is_empty() {
            return Err(DaemonError::Store(
                "V99 identity backfill retained an unmapped lineage reservation".into(),
            ));
        }
        for request in request_only {
            entities_by_epic
                .entry(request.epic_id.clone())
                .or_default()
                .push(IdentityEntity {
                    sort_at: request.reserved_at,
                    request_id: Some(request.id),
                    tie_session_id: request.child_id,
                    members: Vec::new(),
                });
        }

        for epic_id in &epic_ids {
            let entities = entities_by_epic.entry(epic_id.clone()).or_default();
            entities.sort_by(|left, right| {
                left.sort_at
                    .cmp(&right.sort_at)
                    .then_with(|| left.request_id.cmp(&right.request_id))
                    .then_with(|| left.tie_session_id.cmp(&right.tie_session_id))
            });
            for (index, entity) in entities.iter().enumerate() {
                let ordinal = i64::try_from(index + 1).map_err(|_| {
                    DaemonError::Store("V99 identity backfill ordinal overflow".into())
                })?;
                if ordinal > i64::from(u32::MAX) {
                    return Err(DaemonError::Store(
                        "V99 identity backfill exhausted Epic ordinals".into(),
                    ));
                }
                if let Some(request_id) = &entity.request_id {
                    let changed = tx.execute(
                        "UPDATE agent_spawn_requests SET epic_spawn_ordinal=?1
                         WHERE spawn_request_id=?2 AND epic_spawn_ordinal IS NULL",
                        params![ordinal, request_id],
                    )?;
                    if changed != 1 {
                        return Err(DaemonError::Store(
                            "V99 identity backfill could not assign a reservation ordinal".into(),
                        ));
                    }
                }
                for member_id in &entity.members {
                    let changed = tx.execute(
                        "UPDATE sessions SET epic_spawn_ordinal=?1
                         WHERE id=?2 AND epic_spawn_ordinal IS NULL",
                        params![ordinal, member_id],
                    )?;
                    if changed != 1 {
                        return Err(DaemonError::Store(
                            "V99 identity backfill could not assign a lineage ordinal".into(),
                        ));
                    }
                }
            }
            let next_ordinal = i64::try_from(entities.len() + 1)
                .map_err(|_| DaemonError::Store("V99 identity counter seed overflow".into()))?;
            tx.execute(
                "INSERT INTO epic_spawn_counters(epic_id,next_ordinal) VALUES(?1,?2)",
                params![epic_id, next_ordinal],
            )?;
        }
        if let Some((sessions, requests, counters)) = preserved_identity {
            for (session_id, agent_role, ordinal) in sessions {
                let changed = tx.execute(
                    "UPDATE sessions SET agent_role=?2,epic_spawn_ordinal=?3 WHERE id=?1",
                    params![session_id, agent_role, ordinal],
                )?;
                if changed != 1 {
                    return Err(DaemonError::Store(
                        "V100 could not restore one authenticated Session identity".into(),
                    ));
                }
            }
            for (request_id, ordinal) in requests {
                let changed = tx.execute(
                    "UPDATE agent_spawn_requests SET epic_spawn_ordinal=?2
                     WHERE spawn_request_id=?1",
                    params![request_id, ordinal],
                )?;
                if changed != 1 {
                    return Err(DaemonError::Store(
                        "V100 could not restore one authenticated spawn ordinal".into(),
                    ));
                }
            }
            tx.execute("DELETE FROM epic_spawn_counters", [])?;
            for (epic_id, next_ordinal) in counters {
                tx.execute(
                    "INSERT INTO epic_spawn_counters(epic_id,next_ordinal) VALUES(?1,?2)",
                    params![epic_id, next_ordinal],
                )?;
            }
        }
        identity_v112_migration_fault(IdentityV112MigrationFault::AfterBackfill)?;

        tx.execute_batch(
            "CREATE UNIQUE INDEX idx_agent_spawn_requests_epic_ordinal
                 ON agent_spawn_requests(epic_id,epic_spawn_ordinal);
             CREATE INDEX idx_sessions_parent_epic_ordinal
                 ON sessions(parent_id,epic_spawn_ordinal);",
        )?;
        identity_v112_migration_fault(IdentityV112MigrationFault::AfterIndexes)?;

        tx.execute_batch(
            "CREATE TRIGGER sessions_v100_identity_validate_insert BEFORE INSERT ON sessions
             WHEN NOT (
                 (NEW.agent_role IS NULL OR (
                     length(CAST(NEW.agent_role AS BLOB)) BETWEEN 1 AND 64
                     AND NEW.agent_role=trim(NEW.agent_role)
                     AND instr(NEW.agent_role,'  ')=0
                     AND instr(NEW.agent_role,char(0))=0
                     AND instr(NEW.agent_role,char(9))=0
                     AND instr(NEW.agent_role,char(10))=0
                     AND instr(NEW.agent_role,char(13))=0
                 ))
                 AND (NEW.epic_spawn_ordinal IS NULL OR
                      NEW.epic_spawn_ordinal BETWEEN 1 AND 4294967295)
             ) BEGIN SELECT RAISE(ABORT,'V100 invalid session display identity'); END;
             CREATE TRIGGER sessions_v100_identity_validate_update BEFORE UPDATE ON sessions
             WHEN NOT (
                 (NEW.agent_role IS NULL OR (
                     length(CAST(NEW.agent_role AS BLOB)) BETWEEN 1 AND 64
                     AND NEW.agent_role=trim(NEW.agent_role)
                     AND instr(NEW.agent_role,'  ')=0
                     AND instr(NEW.agent_role,char(0))=0
                     AND instr(NEW.agent_role,char(9))=0
                     AND instr(NEW.agent_role,char(10))=0
                     AND instr(NEW.agent_role,char(13))=0
                 ))
                 AND (NEW.epic_spawn_ordinal IS NULL OR
                      NEW.epic_spawn_ordinal BETWEEN 1 AND 4294967295)
             ) BEGIN SELECT RAISE(ABORT,'V100 invalid session display identity'); END;
             CREATE TRIGGER sessions_v100_identity_immutable
             BEFORE UPDATE OF agent_role,epic_spawn_ordinal ON sessions
             WHEN NEW.agent_role IS NOT OLD.agent_role
               OR NEW.epic_spawn_ordinal IS NOT OLD.epic_spawn_ordinal
             BEGIN SELECT RAISE(ABORT,'V100 session display identity is immutable'); END;
             CREATE TRIGGER agent_spawn_requests_v100_ordinal_validate_insert
             BEFORE INSERT ON agent_spawn_requests
             WHEN NEW.epic_spawn_ordinal IS NULL
               OR NEW.epic_spawn_ordinal NOT BETWEEN 1 AND 4294967295
             BEGIN SELECT RAISE(ABORT,'V100 invalid agent spawn ordinal'); END;
             CREATE TRIGGER agent_spawn_requests_v100_ordinal_validate_update
             BEFORE UPDATE ON agent_spawn_requests
             WHEN NEW.epic_spawn_ordinal IS NULL
               OR NEW.epic_spawn_ordinal NOT BETWEEN 1 AND 4294967295
             BEGIN SELECT RAISE(ABORT,'V100 invalid agent spawn ordinal'); END;
             CREATE TRIGGER agent_spawn_requests_v100_ordinal_immutable
             BEFORE UPDATE OF epic_spawn_ordinal ON agent_spawn_requests
             WHEN NEW.epic_spawn_ordinal IS NOT OLD.epic_spawn_ordinal
             BEGIN SELECT RAISE(ABORT,'V100 agent spawn ordinal is immutable'); END;
             CREATE TRIGGER epic_spawn_counters_v100_validate_insert
             BEFORE INSERT ON epic_spawn_counters
             WHEN NEW.next_ordinal NOT BETWEEN 1 AND 4294967296
               OR NOT EXISTS(SELECT 1 FROM sessions
                             WHERE id=NEW.epic_id AND session_kind='Epic')
             BEGIN SELECT RAISE(ABORT,'V100 invalid Epic spawn counter'); END;
             CREATE TRIGGER epic_spawn_counters_v100_validate_update
             BEFORE UPDATE ON epic_spawn_counters
             WHEN NEW.epic_id!=OLD.epic_id
               OR NEW.next_ordinal NOT BETWEEN 1 AND 4294967296
               OR NEW.next_ordinal<OLD.next_ordinal
             BEGIN SELECT RAISE(ABORT,'V100 Epic spawn counter cannot move backward'); END;
             CREATE TRIGGER epic_spawn_counters_v100_no_delete
             BEFORE DELETE ON epic_spawn_counters
             BEGIN SELECT RAISE(ABORT,'V100 Epic spawn counters cannot be deleted'); END;",
        )?;
        identity_v112_migration_fault(IdentityV112MigrationFault::AfterTriggers)?;

        let incomplete_requests: i64 = tx.query_row(
            "SELECT count(*) FROM agent_spawn_requests WHERE epic_spawn_ordinal IS NULL",
            [],
            |row| row.get(0),
        )?;
        let incomplete_children: i64 = tx.query_row(
            "SELECT count(*) FROM sessions child
             JOIN sessions epic ON epic.id=child.parent_id AND epic.session_kind='Epic'
             WHERE child.epic_spawn_ordinal IS NULL",
            [],
            |row| row.get(0),
        )?;
        let invalid_counters: i64 = tx.query_row(
            "SELECT count(*) FROM sessions epic
             LEFT JOIN epic_spawn_counters counter ON counter.epic_id=epic.id
             WHERE epic.session_kind='Epic'
               AND (counter.epic_id IS NULL
                    OR counter.next_ordinal <= COALESCE((
                        SELECT max(request.epic_spawn_ordinal)
                        FROM agent_spawn_requests request WHERE request.epic_id=epic.id
                    ),0)
                    OR counter.next_ordinal <= COALESCE((
                        SELECT max(child.epic_spawn_ordinal)
                        FROM sessions child WHERE child.parent_id=epic.id
                    ),0))",
            [],
            |row| row.get(0),
        )?;
        if incomplete_requests != 0 || incomplete_children != 0 || invalid_counters != 0 {
            return Err(DaemonError::Store(format!(
                "V112 identity validation failed: requests={incomplete_requests}, children={incomplete_children}, counters={invalid_counters}"
            )));
        }

        Self::install_archive_cleanup_v112_schema(&tx)?;
        archive_cleanup::validate_v102_catalog(&tx)?;
        let integrity: String = tx.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        if integrity != "ok" {
            return Err(DaemonError::Store(format!(
                "V112 convergence requires integrity_check=ok, got {integrity}"
            )));
        }
        let foreign_key_errors: i64 =
            tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })?;
        if foreign_key_errors != 0 {
            return Err(DaemonError::Store(format!(
                "V112 convergence found {foreign_key_errors} foreign-key violation(s)"
            )));
        }
        identity_v112_migration_fault(IdentityV112MigrationFault::AfterChecks)?;
        archive_cleanup_v112_migration_fault(ArchiveCleanupV112MigrationFault::AfterChecks)?;
        tx.execute("PRAGMA user_version = 112", [])?;
        identity_v112_migration_fault(IdentityV112MigrationFault::AfterUserVersion)?;
        archive_cleanup_v112_migration_fault(ArchiveCleanupV112MigrationFault::AfterUserVersion)?;
        identity_v112_migration_fault(IdentityV112MigrationFault::BeforeCommit)?;
        archive_cleanup_v112_migration_fault(ArchiveCleanupV112MigrationFault::BeforeCommit)?;
        tx.commit()?;
        tracing::info!("V112 migration complete: target and Operator Views catalogs converged");
        Ok(())
    }
    // RSI-RELEASED-MIGRATION-END: v112-operator-views-convergence-driver

    fn install_archive_cleanup_v112_schema(tx: &Transaction<'_>) -> Result<()> {
        archive_cleanup_v112_migration_fault(ArchiveCleanupV112MigrationFault::AfterPreflight)?;
        let existing: i64 = tx.query_row(
            "SELECT count(*) FROM sqlite_master WHERE name LIKE 'archive_cleanup_%'",
            [],
            |row| row.get(0),
        )?;
        if existing != 0 {
            archive_cleanup::validate_v102_catalog(tx)?;
            archive_cleanup_v112_migration_fault(ArchiveCleanupV112MigrationFault::AfterTables)?;
            archive_cleanup_v112_migration_fault(ArchiveCleanupV112MigrationFault::AfterIndexes)?;
            archive_cleanup_v112_migration_fault(ArchiveCleanupV112MigrationFault::AfterTriggers)?;
            return Ok(());
        }

        tx.execute_batch(
            "CREATE TABLE archive_cleanup_runs (
                run_id TEXT PRIMARY KEY
                    CHECK(length(run_id)=36 AND run_id=lower(run_id)),
                session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE RESTRICT,
                custody_id TEXT NOT NULL REFERENCES sandbox_custody_roots(custody_id) ON DELETE RESTRICT,
                custody_generation INTEGER NOT NULL CHECK(custody_generation>0),
                schema_version INTEGER NOT NULL CHECK(schema_version=1),
                marker_version INTEGER NOT NULL CHECK(marker_version=1),
                session_kind TEXT NOT NULL CHECK(session_kind IN ('Standard','TaskRabbit','Bug','Story','Task','Feature','Refactor','Research')),
                session_status TEXT NOT NULL CHECK(session_status IN ('Completed','Failed','Interrupted')),
                session_updated_at TEXT NOT NULL CHECK(length(session_updated_at) BETWEEN 20 AND 40),
                parent_id TEXT,
                continued_from TEXT,
                topology_digest TEXT NOT NULL CHECK(length(topology_digest)=71 AND topology_digest GLOB 'sha256:[0-9a-f]*'),
                repository_identity TEXT NOT NULL CHECK(length(repository_identity) BETWEEN 1 AND 4096),
                repository_identity_digest TEXT NOT NULL CHECK(length(repository_identity_digest)=71 AND repository_identity_digest GLOB 'sha256:[0-9a-f]*'),
                canonical_repo_dir TEXT NOT NULL CHECK(length(canonical_repo_dir) BETWEEN 1 AND 4096),
                canonical_repo_dir_digest TEXT NOT NULL CHECK(length(canonical_repo_dir_digest)=71 AND canonical_repo_dir_digest GLOB 'sha256:[0-9a-f]*'),
                original_root TEXT NOT NULL CHECK(length(original_root) BETWEEN 1 AND 4096),
                original_root_digest TEXT NOT NULL CHECK(length(original_root_digest)=71 AND original_root_digest GLOB 'sha256:[0-9a-f]*'),
                quarantine_root TEXT NOT NULL CHECK(length(quarantine_root) BETWEEN 1 AND 4096),
                quarantine_root_digest TEXT NOT NULL CHECK(length(quarantine_root_digest)=71 AND quarantine_root_digest GLOB 'sha256:[0-9a-f]*'),
                root_device INTEGER NOT NULL CHECK(root_device>=0),
                root_inode INTEGER NOT NULL CHECK(root_inode>0),
                git_common_dir_digest TEXT NOT NULL CHECK(length(git_common_dir_digest)=71 AND git_common_dir_digest GLOB 'sha256:[0-9a-f]*'),
                git_admin_dir_digest TEXT NOT NULL CHECK(length(git_admin_dir_digest)=71 AND git_admin_dir_digest GLOB 'sha256:[0-9a-f]*'),
                git_admin_id TEXT NOT NULL CHECK(length(git_admin_id) BETWEEN 1 AND 255),
                source_ref TEXT NOT NULL CHECK(source_ref GLOB 'refs/heads/*' AND length(source_ref)<=1024),
                source_oid TEXT NOT NULL CHECK(length(source_oid) IN (40,64) AND source_oid=lower(source_oid)),
                preservation_class TEXT NOT NULL CHECK(preservation_class IN ('no_output','integrated_ancestor')),
                target_ref TEXT,
                target_oid TEXT,
                clean_state_digest TEXT NOT NULL CHECK(length(clean_state_digest)=71 AND clean_state_digest GLOB 'sha256:[0-9a-f]*'),
                tree_digest TEXT NOT NULL CHECK(length(tree_digest)=71 AND tree_digest GLOB 'sha256:[0-9a-f]*'),
                dependency_digest TEXT NOT NULL CHECK(length(dependency_digest)=71 AND dependency_digest GLOB 'sha256:[0-9a-f]*'),
                holder_digest TEXT NOT NULL CHECK(length(holder_digest)=71 AND holder_digest GLOB 'sha256:[0-9a-f]*'),
                evidence_digest TEXT NOT NULL CHECK(length(evidence_digest)=71 AND evidence_digest GLOB 'sha256:[0-9a-f]*'),
                phase TEXT NOT NULL CHECK(phase IN ('intent_committed','quarantined','removal_authorized','worktree_removed','settled','refused','recovery_required')),
                phase_ordinal INTEGER NOT NULL CHECK(phase_ordinal IN (1,2,3,4,5,100)),
                row_version INTEGER NOT NULL CHECK(row_version>0),
                safe_code TEXT,
                recovery_detail_code TEXT,
                branch_preserved INTEGER NOT NULL DEFAULT 0 CHECK(branch_preserved IN (0,1)),
                removal_authority_json TEXT,
                removal_authority_digest TEXT,
                receipt_json TEXT,
                receipt_digest TEXT,
                intent_at TEXT NOT NULL CHECK(length(intent_at) BETWEEN 20 AND 40),
                quarantined_at TEXT,
                authorized_at TEXT,
                removed_at TEXT,
                refused_at TEXT,
                recovery_at TEXT,
                settled_at TEXT,
                last_attempt_at TEXT NOT NULL CHECK(length(last_attempt_at) BETWEEN 20 AND 40),
                created_at TEXT NOT NULL CHECK(length(created_at) BETWEEN 20 AND 40),
                updated_at TEXT NOT NULL CHECK(length(updated_at) BETWEEN 20 AND 40),
                CHECK((preservation_class='no_output' AND target_ref IS NULL AND target_oid IS NULL)
                   OR (preservation_class='integrated_ancestor' AND target_ref GLOB 'refs/heads/*'
                       AND length(target_oid) IN (40,64) AND target_oid=lower(target_oid))),
                CHECK((phase_ordinal=1 AND phase='intent_committed') OR
                      (phase_ordinal=2 AND phase='quarantined') OR
                      (phase_ordinal=3 AND phase='removal_authorized') OR
                      (phase_ordinal=4 AND phase='worktree_removed') OR
                      (phase_ordinal=5 AND phase='settled') OR
                      (phase_ordinal=100 AND phase IN ('refused','recovery_required'))),
                CHECK((phase IN ('intent_committed','quarantined','refused','recovery_required')) OR branch_preserved=1),
                CHECK((phase IN ('intent_committed','quarantined','refused','recovery_required')) OR
                      (removal_authority_json IS NOT NULL AND removal_authority_digest IS NOT NULL)),
                CHECK((phase!='settled') OR
                      (safe_code='settled' AND branch_preserved=1 AND receipt_json IS NOT NULL
                       AND receipt_digest IS NOT NULL AND settled_at IS NOT NULL)),
                CHECK((phase!='refused') OR (safe_code IS NOT NULL AND refused_at IS NOT NULL)),
                CHECK((phase!='recovery_required') OR (safe_code IS NOT NULL AND recovery_at IS NOT NULL))
            );
            CREATE TABLE archive_cleanup_events (
                run_id TEXT NOT NULL REFERENCES archive_cleanup_runs(run_id) ON DELETE RESTRICT,
                sequence INTEGER NOT NULL CHECK(sequence>0),
                from_phase TEXT,
                to_phase TEXT NOT NULL CHECK(to_phase IN ('intent_committed','quarantined','removal_authorized','worktree_removed','settled','refused','recovery_required')),
                safe_event_code TEXT NOT NULL CHECK(length(safe_event_code) BETWEEN 1 AND 96),
                evidence_digest TEXT CHECK(evidence_digest IS NULL OR (length(evidence_digest)=71 AND evidence_digest GLOB 'sha256:[0-9a-f]*')),
                marker_digest TEXT CHECK(marker_digest IS NULL OR (length(marker_digest)=71 AND marker_digest GLOB 'sha256:[0-9a-f]*')),
                receipt_digest TEXT CHECK(receipt_digest IS NULL OR (length(receipt_digest)=71 AND receipt_digest GLOB 'sha256:[0-9a-f]*')),
                occurred_at TEXT NOT NULL CHECK(length(occurred_at) BETWEEN 20 AND 40),
                PRIMARY KEY(run_id,sequence)
            );",
        )?;
        archive_cleanup_v112_migration_fault(ArchiveCleanupV112MigrationFault::AfterTables)?;

        tx.execute_batch(
            "CREATE UNIQUE INDEX archive_cleanup_runs_one_open_generation
                 ON archive_cleanup_runs(session_id,custody_id,custody_generation)
                 WHERE phase NOT IN ('refused','settled');
             CREATE INDEX archive_cleanup_runs_recovery_scan
                 ON archive_cleanup_runs(updated_at,run_id)
                 WHERE phase NOT IN ('settled','refused','recovery_required');
             CREATE INDEX archive_cleanup_runs_session_readback
                 ON archive_cleanup_runs(session_id,created_at DESC,run_id DESC);",
        )?;
        archive_cleanup_v112_migration_fault(ArchiveCleanupV112MigrationFault::AfterIndexes)?;

        tx.execute_batch(
            "CREATE TRIGGER archive_cleanup_runs_v101_identity_immutable
             BEFORE UPDATE ON archive_cleanup_runs
             WHEN NEW.run_id!=OLD.run_id OR NEW.session_id!=OLD.session_id
               OR NEW.custody_id!=OLD.custody_id OR NEW.custody_generation!=OLD.custody_generation
               OR NEW.schema_version!=OLD.schema_version OR NEW.marker_version!=OLD.marker_version
               OR NEW.session_kind!=OLD.session_kind OR NEW.session_status!=OLD.session_status
               OR NEW.session_updated_at!=OLD.session_updated_at
               OR NEW.parent_id IS NOT OLD.parent_id OR NEW.continued_from IS NOT OLD.continued_from
               OR NEW.topology_digest!=OLD.topology_digest
               OR NEW.repository_identity!=OLD.repository_identity
               OR NEW.repository_identity_digest!=OLD.repository_identity_digest
               OR NEW.canonical_repo_dir!=OLD.canonical_repo_dir
               OR NEW.canonical_repo_dir_digest!=OLD.canonical_repo_dir_digest
               OR NEW.original_root!=OLD.original_root OR NEW.original_root_digest!=OLD.original_root_digest
               OR NEW.quarantine_root!=OLD.quarantine_root OR NEW.quarantine_root_digest!=OLD.quarantine_root_digest
               OR NEW.root_device!=OLD.root_device OR NEW.root_inode!=OLD.root_inode
               OR NEW.git_common_dir_digest!=OLD.git_common_dir_digest
               OR NEW.git_admin_dir_digest!=OLD.git_admin_dir_digest OR NEW.git_admin_id!=OLD.git_admin_id
               OR NEW.source_ref!=OLD.source_ref OR NEW.source_oid!=OLD.source_oid
               OR NEW.preservation_class!=OLD.preservation_class
               OR NEW.target_ref IS NOT OLD.target_ref OR NEW.target_oid IS NOT OLD.target_oid
               OR NEW.clean_state_digest!=OLD.clean_state_digest OR NEW.tree_digest!=OLD.tree_digest
               OR NEW.dependency_digest!=OLD.dependency_digest OR NEW.holder_digest!=OLD.holder_digest
               OR NEW.evidence_digest!=OLD.evidence_digest OR NEW.intent_at!=OLD.intent_at
               OR NEW.created_at!=OLD.created_at
             BEGIN SELECT RAISE(ABORT,'V101 archive cleanup identity is immutable'); END;
             CREATE TRIGGER archive_cleanup_runs_v101_phase_forward
             BEFORE UPDATE ON archive_cleanup_runs
             WHEN NOT (
               (OLD.phase='intent_committed' AND NEW.phase IN ('quarantined','refused','recovery_required')) OR
               (OLD.phase='quarantined' AND NEW.phase IN ('removal_authorized','recovery_required')) OR
               (OLD.phase='removal_authorized' AND NEW.phase IN ('worktree_removed','recovery_required')) OR
               (OLD.phase='worktree_removed' AND NEW.phase IN ('settled','recovery_required')))
             BEGIN SELECT RAISE(ABORT,'V101 archive cleanup phase transition refused'); END;
             CREATE TRIGGER archive_cleanup_runs_v101_no_delete
             BEFORE DELETE ON archive_cleanup_runs
             BEGIN SELECT RAISE(ABORT,'V101 archive cleanup runs are immutable history'); END;
             CREATE TRIGGER archive_cleanup_events_v101_no_update
             BEFORE UPDATE ON archive_cleanup_events
             BEGIN SELECT RAISE(ABORT,'V101 archive cleanup events are immutable'); END;
             CREATE TRIGGER archive_cleanup_events_v101_no_delete
             BEFORE DELETE ON archive_cleanup_events
             BEGIN SELECT RAISE(ABORT,'V101 archive cleanup events are immutable'); END;",
        )?;
        archive_cleanup_v112_migration_fault(ArchiveCleanupV112MigrationFault::AfterTriggers)?;
        archive_cleanup::validate_v102_catalog(tx)?;
        Ok(())
    }

    // RSI-RELEASED-MIGRATION-BEGIN: v113-archive-projection-driver
    fn apply_archive_projection_v113_migration(&self) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if active_version != 112 {
            return Err(DaemonError::Store(format!(
                "V113 requires exact V112 source, found V{active_version}"
            )));
        }
        archive_cleanup::validate_v102_catalog(&tx)?;
        archive_projection_v113_migration_fault(
            ArchiveProjectionV113MigrationFault::AfterPreflight,
        )?;

        tx.execute_batch(
            "CREATE TABLE archive_cleanup_success_projections (
                projection_id TEXT PRIMARY KEY
                    CHECK(length(projection_id)=36 AND projection_id=lower(projection_id)),
                run_id TEXT NOT NULL UNIQUE
                    REFERENCES archive_cleanup_runs(run_id) ON DELETE RESTRICT,
                session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE RESTRICT,
                custody_id TEXT NOT NULL
                    REFERENCES sandbox_custody_roots(custody_id) ON DELETE RESTRICT,
                custody_generation INTEGER NOT NULL CHECK(custody_generation>0),
                projection_kind TEXT NOT NULL CHECK(projection_kind='session_archived'),
                schema_version INTEGER NOT NULL CHECK(schema_version=1),
                canonical_json TEXT NOT NULL CHECK(length(canonical_json)>2),
                projection_digest TEXT NOT NULL
                    CHECK(length(projection_digest)=71 AND projection_digest GLOB 'sha256:[0-9a-f]*'),
                receipt_digest TEXT NOT NULL
                    CHECK(length(receipt_digest)=71 AND receipt_digest GLOB 'sha256:[0-9a-f]*'),
                delivery_state TEXT NOT NULL CHECK(delivery_state IN ('pending','delivering','delivered')),
                row_version INTEGER NOT NULL CHECK(row_version>0),
                attempt_count INTEGER NOT NULL CHECK(attempt_count>=0),
                first_attempt_at TEXT,
                last_attempt_at TEXT,
                delivered_at TEXT,
                created_at TEXT NOT NULL CHECK(length(created_at) BETWEEN 20 AND 40),
                updated_at TEXT NOT NULL CHECK(length(updated_at) BETWEEN 20 AND 40),
                CHECK((delivery_state='pending' AND attempt_count=0 AND first_attempt_at IS NULL
                       AND last_attempt_at IS NULL AND delivered_at IS NULL)
                   OR (delivery_state='delivering' AND attempt_count>0 AND first_attempt_at IS NOT NULL
                       AND last_attempt_at IS NOT NULL AND delivered_at IS NULL)
                   OR (delivery_state='delivered' AND delivered_at IS NOT NULL))
            );
            CREATE TABLE archive_cleanup_projection_consumers (
                projection_id TEXT NOT NULL
                    REFERENCES archive_cleanup_success_projections(projection_id) ON DELETE RESTRICT,
                consumer_kind TEXT NOT NULL CHECK(consumer_kind IN ('bus','watch','memory')),
                delivery_state TEXT NOT NULL CHECK(delivery_state IN ('pending','delivering','delivered')),
                row_version INTEGER NOT NULL CHECK(row_version>0),
                attempt_count INTEGER NOT NULL CHECK(attempt_count>=0),
                first_attempt_at TEXT,
                last_attempt_at TEXT,
                delivered_at TEXT,
                created_at TEXT NOT NULL CHECK(length(created_at) BETWEEN 20 AND 40),
                updated_at TEXT NOT NULL CHECK(length(updated_at) BETWEEN 20 AND 40),
                PRIMARY KEY(projection_id,consumer_kind),
                CHECK((delivery_state='pending' AND attempt_count=0 AND first_attempt_at IS NULL
                       AND last_attempt_at IS NULL AND delivered_at IS NULL)
                   OR (delivery_state='delivering' AND attempt_count>0 AND first_attempt_at IS NOT NULL
                       AND last_attempt_at IS NOT NULL AND delivered_at IS NULL)
                   OR (delivery_state='delivered' AND delivered_at IS NOT NULL))
            );",
        )?;
        archive_projection_v113_migration_fault(ArchiveProjectionV113MigrationFault::AfterTables)?;

        tx.execute_batch(
            "CREATE INDEX archive_cleanup_success_projections_pending
                 ON archive_cleanup_success_projections(updated_at,projection_id)
                 WHERE delivery_state!='delivered';
             CREATE INDEX archive_cleanup_projection_consumers_pending
                 ON archive_cleanup_projection_consumers(projection_id,consumer_kind)
                 WHERE delivery_state!='delivered';",
        )?;
        archive_projection_v113_migration_fault(ArchiveProjectionV113MigrationFault::AfterIndexes)?;

        tx.execute_batch(
            "CREATE TRIGGER archive_cleanup_success_projections_v103_association_insert
             BEFORE INSERT ON archive_cleanup_success_projections
             WHEN NOT EXISTS (
               SELECT 1 FROM archive_cleanup_runs run
               WHERE run.run_id=NEW.run_id AND run.phase='settled'
                 AND run.session_id=NEW.session_id AND run.custody_id=NEW.custody_id
                 AND run.custody_generation=NEW.custody_generation
                 AND run.receipt_digest=NEW.receipt_digest)
             BEGIN SELECT RAISE(ABORT,'V103 archive projection requires its settled run'); END;
             CREATE TRIGGER archive_cleanup_success_projections_v103_identity_immutable
             BEFORE UPDATE ON archive_cleanup_success_projections
             WHEN NEW.projection_id!=OLD.projection_id OR NEW.run_id!=OLD.run_id
               OR NEW.session_id!=OLD.session_id OR NEW.custody_id!=OLD.custody_id
               OR NEW.custody_generation!=OLD.custody_generation
               OR NEW.projection_kind!=OLD.projection_kind OR NEW.schema_version!=OLD.schema_version
               OR NEW.canonical_json!=OLD.canonical_json OR NEW.projection_digest!=OLD.projection_digest
               OR NEW.receipt_digest!=OLD.receipt_digest OR NEW.created_at!=OLD.created_at
             BEGIN SELECT RAISE(ABORT,'V103 archive projection identity is immutable'); END;
             CREATE TRIGGER archive_cleanup_success_projections_v103_state_forward
             BEFORE UPDATE ON archive_cleanup_success_projections
             WHEN NOT ((OLD.delivery_state='pending' AND NEW.delivery_state='delivering')
                    OR (OLD.delivery_state='delivering' AND NEW.delivery_state IN ('delivering','delivered')))
             BEGIN SELECT RAISE(ABORT,'V103 archive projection transition refused'); END;
             CREATE TRIGGER archive_cleanup_success_projections_v103_no_delete
             BEFORE DELETE ON archive_cleanup_success_projections
             BEGIN SELECT RAISE(ABORT,'V103 archive projections are immutable history'); END;
             CREATE TRIGGER archive_cleanup_projection_consumers_v103_forward
             BEFORE UPDATE ON archive_cleanup_projection_consumers
             WHEN NEW.projection_id!=OLD.projection_id OR NEW.consumer_kind!=OLD.consumer_kind
               OR NEW.created_at!=OLD.created_at
               OR NOT ((OLD.delivery_state='pending' AND NEW.delivery_state='delivering')
                    OR (OLD.delivery_state='delivering' AND NEW.delivery_state IN ('delivering','delivered')))
             BEGIN SELECT RAISE(ABORT,'V103 archive projection consumer transition refused'); END;
             CREATE TRIGGER archive_cleanup_projection_consumers_v103_no_delete
             BEFORE DELETE ON archive_cleanup_projection_consumers
             BEGIN SELECT RAISE(ABORT,'V103 archive projection consumers are immutable history'); END;",
        )?;
        archive_projection_v113_migration_fault(
            ArchiveProjectionV113MigrationFault::AfterTriggers,
        )?;

        archive_cleanup::backfill_v103_settled_projections(&tx)?;
        archive_projection_v113_migration_fault(
            ArchiveProjectionV113MigrationFault::AfterBackfill,
        )?;
        archive_cleanup::validate_v103_catalog(&tx)?;
        archive_cleanup::validate_v103_projection_rows(&tx)?;
        let integrity: String = tx.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        if integrity != "ok" {
            return Err(DaemonError::Store(format!(
                "V113 archive projection migration requires integrity_check=ok, got {integrity}"
            )));
        }
        let foreign_key_errors: i64 =
            tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })?;
        if foreign_key_errors != 0 {
            return Err(DaemonError::Store(format!(
                "V113 archive projection migration found {foreign_key_errors} foreign-key violation(s)"
            )));
        }
        archive_projection_v113_migration_fault(ArchiveProjectionV113MigrationFault::AfterChecks)?;
        tx.execute("PRAGMA user_version = 113", [])?;
        archive_projection_v113_migration_fault(
            ArchiveProjectionV113MigrationFault::AfterUserVersion,
        )?;
        archive_projection_v113_migration_fault(ArchiveProjectionV113MigrationFault::BeforeCommit)?;
        tx.commit()?;
        tracing::info!("V113 migration complete: archive success projection installed");
        Ok(())
    }
    // RSI-RELEASED-MIGRATION-END: v113-archive-projection-driver

    // RSI-RELEASED-MIGRATION-BEGIN: v114-lineage-detachment-driver
    /// V114: durable, receipt-validated detachment receipts for the Epic
    /// lineages the V111 branch normalization re-rooted.
    ///
    /// The records are reconstructed from the receipts themselves, never from
    /// a hard-coded identity list: a session whose `continued_from` is NULL yet
    /// which a committed reservation or a manager rotation edge names as a
    /// successor can only exist because something nulled the column, and the
    /// normalization is the only writer in the tree that does. Both proofs are
    /// required: the receipt proves the edge, and the `rotation_events` journal
    /// row proves this tree performed the detachment. A disagreement or a
    /// missing journal row aborts, so a column nulled by anything else never
    /// earns a receipt.
    fn apply_lineage_detachment_v114_migration(&self) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if active_version != 113 {
            return Err(DaemonError::Store(format!(
                "V114 requires exact V113 source, found V{active_version}"
            )));
        }
        lineage_detachment_v114_migration_fault(
            LineageDetachmentV114MigrationFault::AfterPreflight,
        )?;

        tx.execute_batch(
            "CREATE TABLE session_lineage_detachments (
                session_id TEXT PRIMARY KEY
                    REFERENCES sessions(id) ON DELETE RESTRICT
                    CHECK(length(session_id)=36 AND session_id=lower(session_id)),
                detached_from_session_id TEXT NOT NULL
                    REFERENCES sessions(id) ON DELETE RESTRICT
                    CHECK(length(detached_from_session_id)=36
                          AND detached_from_session_id=lower(detached_from_session_id)),
                receipt_class TEXT NOT NULL
                    CHECK(receipt_class IN ('harness_manager_rotation_edge','committed_reservation')),
                normalization_schema TEXT NOT NULL
                    CHECK(normalization_schema='v111-branch-normalization/1'),
                recorded_at TEXT NOT NULL CHECK(length(recorded_at) BETWEEN 20 AND 40),
                CHECK(session_id <> detached_from_session_id)
            );
            CREATE INDEX session_lineage_detachments_predecessor
                ON session_lineage_detachments(detached_from_session_id, session_id);
            CREATE TRIGGER session_lineage_detachments_v114_validate_insert
            BEFORE INSERT ON session_lineage_detachments
            WHEN NOT (
                (NEW.receipt_class='harness_manager_rotation_edge' AND EXISTS(
                    SELECT 1 FROM harness_manager_rotation_edges
                     WHERE predecessor_session_id=NEW.detached_from_session_id
                       AND successor_session_id=NEW.session_id))
             OR (NEW.receipt_class='committed_reservation' AND EXISTS(
                    SELECT 1 FROM agent_successor_reservations
                     WHERE predecessor_session_id=NEW.detached_from_session_id
                       AND candidate_session_id=NEW.session_id
                       AND state='committed'))
            ) BEGIN SELECT RAISE(ABORT,'V114 lineage detachment requires a matching receipt'); END;
            CREATE TRIGGER session_lineage_detachments_v114_no_update
            BEFORE UPDATE ON session_lineage_detachments
            BEGIN SELECT RAISE(ABORT,'V114 lineage detachment receipts are immutable'); END;
            CREATE TRIGGER session_lineage_detachments_v114_no_delete
            BEFORE DELETE ON session_lineage_detachments
            BEGIN SELECT RAISE(ABORT,'V114 lineage detachment receipts are retained'); END;",
        )?;
        lineage_convergence::validate_v114_catalog(&tx)?;
        lineage_detachment_v114_migration_fault(LineageDetachmentV114MigrationFault::AfterCatalog)?;

        let inserted = lineage_convergence::reconstruct_v114_detachment_rows(&tx)?;
        let foreign_key_errors: i64 = tx.query_row(
            "SELECT count(*) FROM pragma_foreign_key_check('session_lineage_detachments')",
            [],
            |row| row.get(0),
        )?;
        if foreign_key_errors != 0 {
            return Err(DaemonError::Store(format!(
                "V114 lineage detachment migration found {foreign_key_errors} foreign-key violation(s)"
            )));
        }
        lineage_detachment_v114_migration_fault(LineageDetachmentV114MigrationFault::AfterRows)?;

        tx.execute("PRAGMA user_version = 114", [])?;
        tx.commit()?;
        tracing::info!(
            reconstructed = inserted,
            "V114 migration complete: lineage detachment receipts installed"
        );
        Ok(())
    }
    // RSI-RELEASED-MIGRATION-END: v114-lineage-detachment-driver

    // RSI-RELEASED-MIGRATION-BEGIN: v117-manager-notices-driver
    fn apply_manager_notices_v117_migration(&self) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if active_version != 116 {
            return Err(DaemonError::Store(format!(
                "V117 requires exact V116 source, found V{active_version}"
            )));
        }
        tx.execute_batch(
            "ALTER TABLE harness_manager_watches ADD COLUMN route_kind TEXT NOT NULL DEFAULT 'epic'
                CHECK(route_kind IN ('epic','manager_action'));
            CREATE TABLE harness_manager_action_notice_queue (
                operation_id TEXT NOT NULL
                    REFERENCES harness_manager_v2_operations(id),
                project_id TEXT NOT NULL REFERENCES projects(id),
                manager_session_id TEXT NOT NULL REFERENCES sessions(id),
                scope_version INTEGER NOT NULL CHECK(scope_version > 0),
                operation_row_version INTEGER NOT NULL CHECK(operation_row_version > 0),
                receipt_json TEXT NOT NULL
                    CHECK(json_valid(receipt_json) AND json_type(receipt_json)='object'
                          AND length(CAST(receipt_json AS BLOB))<=65536),
                queued_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(queued_at)),
                reconciled_at TEXT
                    CHECK(reconciled_at IS NULL OR rsi_rfc3339_nanos_is_canonical(reconciled_at)),
                retired_at TEXT
                    CHECK(retired_at IS NULL OR rsi_rfc3339_nanos_is_canonical(retired_at)),
                CHECK(reconciled_at IS NULL OR retired_at IS NULL),
                PRIMARY KEY(operation_id,operation_row_version)
            );
            CREATE INDEX harness_manager_action_notice_pending
                ON harness_manager_action_notice_queue(
                    project_id,manager_session_id,scope_version,queued_at,operation_id,
                    operation_row_version)
                WHERE reconciled_at IS NULL AND retired_at IS NULL;
            CREATE TRIGGER harness_manager_action_notice_queue_after_insert
            AFTER INSERT ON harness_manager_v2_operations
            WHEN NEW.kind='lifecycle_action' AND NEW.state NOT IN ('queued','running')
            BEGIN
                INSERT OR IGNORE INTO harness_manager_action_notice_queue(
                    operation_id,project_id,manager_session_id,scope_version,
                    operation_row_version,receipt_json,queued_at,retired_at)
                VALUES(NEW.id,NEW.project_id,NEW.manager_session_id,NEW.scope_version,
                    NEW.row_version,NEW.outcome_json,NEW.updated_at,
                    CASE WHEN EXISTS(
                        SELECT 1 FROM harness_manager_scopes scope
                        WHERE scope.project_id=NEW.project_id
                          AND scope.manager_session_id=NEW.manager_session_id
                          AND scope.row_version=NEW.scope_version
                    ) THEN NULL ELSE NEW.updated_at END);
            END;
            CREATE TRIGGER harness_manager_action_notice_queue_after_update
            AFTER UPDATE OF state,row_version ON harness_manager_v2_operations
            WHEN NEW.kind='lifecycle_action' AND NEW.state NOT IN ('queued','running')
            BEGIN
                INSERT OR IGNORE INTO harness_manager_action_notice_queue(
                    operation_id,project_id,manager_session_id,scope_version,
                    operation_row_version,receipt_json,queued_at,retired_at)
                VALUES(NEW.id,NEW.project_id,NEW.manager_session_id,NEW.scope_version,
                    NEW.row_version,NEW.outcome_json,NEW.updated_at,
                    CASE WHEN EXISTS(
                        SELECT 1 FROM harness_manager_scopes scope
                        WHERE scope.project_id=NEW.project_id
                          AND scope.manager_session_id=NEW.manager_session_id
                          AND scope.row_version=NEW.scope_version
                    ) THEN NULL ELSE NEW.updated_at END);
            END;
            CREATE TRIGGER harness_manager_action_notice_queue_forward
            BEFORE UPDATE ON harness_manager_action_notice_queue
            WHEN NEW.operation_id IS NOT OLD.operation_id
              OR NEW.project_id IS NOT OLD.project_id
              OR NEW.manager_session_id IS NOT OLD.manager_session_id
              OR NEW.scope_version IS NOT OLD.scope_version
              OR NEW.operation_row_version IS NOT OLD.operation_row_version
              OR NEW.receipt_json IS NOT OLD.receipt_json
              OR NEW.queued_at IS NOT OLD.queued_at
              OR (OLD.reconciled_at IS NOT NULL AND NEW.reconciled_at IS NOT OLD.reconciled_at)
              OR (OLD.retired_at IS NOT NULL AND NEW.retired_at IS NOT OLD.retired_at)
              OR (NEW.reconciled_at IS NOT NULL AND NEW.retired_at IS NOT NULL)
            BEGIN SELECT RAISE(ABORT,'manager action notice reconciliation is forward-only'); END;
            CREATE TRIGGER harness_manager_action_notice_queue_no_delete
            BEFORE DELETE ON harness_manager_action_notice_queue
            BEGIN SELECT RAISE(ABORT,'manager action notice reconciliation is retained'); END;
            INSERT INTO harness_manager_action_notice_queue(
                operation_id,project_id,manager_session_id,scope_version,
                operation_row_version,receipt_json,queued_at,retired_at)
            SELECT id,project_id,manager_session_id,scope_version,row_version,
                outcome_json,updated_at,
                CASE WHEN EXISTS(
                    SELECT 1 FROM harness_manager_scopes scope
                    WHERE scope.project_id=harness_manager_v2_operations.project_id
                      AND scope.manager_session_id=harness_manager_v2_operations.manager_session_id
                      AND scope.row_version=harness_manager_v2_operations.scope_version
                ) THEN NULL ELSE updated_at END
            FROM harness_manager_v2_operations
            WHERE kind='lifecycle_action' AND state NOT IN ('queued','running');
            CREATE TABLE harness_manager_notices (
                sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                id TEXT NOT NULL UNIQUE
                    CHECK(rsi_uuid_is_canonical(id)),
                job_id TEXT NOT NULL
                    CHECK(rsi_uuid_is_canonical(job_id)),
                project_id TEXT NOT NULL
                    CHECK(rsi_uuid_is_canonical(project_id)),
                manager_session_id TEXT NOT NULL
                    CHECK(rsi_uuid_is_canonical(manager_session_id)),
                scope_version INTEGER NOT NULL CHECK(scope_version > 0),
                epic_id TEXT
                    CHECK(epic_id IS NULL OR rsi_uuid_is_canonical(epic_id)),
                direction TEXT NOT NULL CHECK(direction IN ('to_manager','to_lead')),
                source_session_id TEXT
                    CHECK(source_session_id IS NULL OR rsi_uuid_is_canonical(source_session_id)),
                recipient_session_id TEXT NOT NULL
                    CHECK(rsi_uuid_is_canonical(recipient_session_id)),
                kind TEXT NOT NULL CHECK(kind IN ('session_state','message','action_result','operator_answer','ledger_change')),
                subject_id TEXT NOT NULL CHECK(length(subject_id) BETWEEN 1 AND 512),
                subject_version TEXT NOT NULL CHECK(length(subject_version) BETWEEN 1 AND 256),
                state_json TEXT NOT NULL
                    CHECK(json_valid(state_json) AND json_type(state_json)='object'
                          AND length(CAST(state_json AS BLOB))<=65536),
                recorded_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(recorded_at)),
                queued_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(queued_at)),
                delivered_at TEXT CHECK(delivered_at IS NULL OR rsi_rfc3339_nanos_is_canonical(delivered_at)),
                retrieved_at TEXT CHECK(retrieved_at IS NULL OR rsi_rfc3339_nanos_is_canonical(retrieved_at)),
                settled_at TEXT CHECK(settled_at IS NULL OR rsi_rfc3339_nanos_is_canonical(settled_at)),
                retired_at TEXT CHECK(retired_at IS NULL OR rsi_rfc3339_nanos_is_canonical(retired_at)),
                UNIQUE(job_id,kind,subject_id,subject_version),
                CHECK((settled_at IS NULL) = (retrieved_at IS NULL)),
                CHECK(retired_at IS NULL OR (retrieved_at IS NULL AND settled_at IS NULL))
            );
            CREATE INDEX harness_manager_notices_live_recipient
                ON harness_manager_notices(project_id,manager_session_id,scope_version,
                    recipient_session_id,retired_at,settled_at,sequence);
            CREATE INDEX harness_manager_notices_pending_scope_sequence
                ON harness_manager_notices(project_id,manager_session_id,scope_version,sequence)
                WHERE retired_at IS NULL AND settled_at IS NULL;
            CREATE INDEX harness_manager_notices_pending_direction_sequence
                ON harness_manager_notices(project_id,manager_session_id,scope_version,
                    direction,sequence)
                WHERE retired_at IS NULL AND settled_at IS NULL;
            CREATE INDEX harness_manager_notices_pending_epic_sequence
                ON harness_manager_notices(project_id,manager_session_id,scope_version,
                    direction,epic_id,sequence)
                WHERE retired_at IS NULL AND settled_at IS NULL;
            CREATE INDEX harness_manager_notices_pending_subject_sequence
                ON harness_manager_notices(project_id,manager_session_id,scope_version,
                    direction,subject_id,sequence)
                WHERE kind='message' AND retired_at IS NULL AND settled_at IS NULL;
            CREATE INDEX harness_manager_notices_pending_request_sequence
                ON harness_manager_notices(project_id,manager_session_id,scope_version,
                    direction,json_extract(state_json,'$.request_id'),sequence)
                WHERE kind='message' AND retired_at IS NULL AND settled_at IS NULL;
            CREATE INDEX harness_manager_notices_undelivered_direction_sequence
                ON harness_manager_notices(project_id,manager_session_id,scope_version,
                    direction,sequence)
                WHERE retired_at IS NULL AND settled_at IS NULL AND delivered_at IS NULL;
            CREATE INDEX harness_manager_notices_subject_lookup
                ON harness_manager_notices(kind,subject_id,subject_version,
                    project_id,manager_session_id,scope_version,epic_id,direction);
            CREATE INDEX harness_manager_notices_unsettled_job
                ON harness_manager_notices(job_id,retired_at,settled_at,delivered_at,sequence);
            CREATE TABLE harness_manager_notice_transport_candidates (
                job_id TEXT PRIMARY KEY NOT NULL
                    CHECK(rsi_uuid_is_canonical(job_id)),
                project_id TEXT NOT NULL
                    CHECK(rsi_uuid_is_canonical(project_id)),
                manager_session_id TEXT NOT NULL
                    CHECK(rsi_uuid_is_canonical(manager_session_id)),
                scope_version INTEGER NOT NULL CHECK(scope_version > 0),
                epic_id TEXT
                    CHECK(epic_id IS NULL OR rsi_uuid_is_canonical(epic_id)),
                first_sequence INTEGER NOT NULL CHECK(first_sequence > 0),
                queued_at TEXT NOT NULL
                    CHECK(rsi_rfc3339_nanos_is_canonical(queued_at))
            );
            CREATE INDEX harness_manager_notice_transport_candidate_page
                ON harness_manager_notice_transport_candidates(
                    project_id,manager_session_id,scope_version,first_sequence,job_id);
            CREATE TRIGGER harness_manager_notice_transport_candidate_after_insert
            AFTER INSERT ON harness_manager_notices
            WHEN NEW.direction='to_manager' AND NEW.retired_at IS NULL
              AND NEW.settled_at IS NULL AND NEW.delivered_at IS NULL
            BEGIN
                INSERT OR IGNORE INTO harness_manager_notice_transport_candidates(
                    job_id,project_id,manager_session_id,scope_version,epic_id,
                    first_sequence,queued_at)
                VALUES(NEW.job_id,NEW.project_id,NEW.manager_session_id,NEW.scope_version,
                    NEW.epic_id,NEW.sequence,NEW.queued_at);
            END;
            CREATE TRIGGER harness_manager_notice_transport_candidate_after_update
            AFTER UPDATE OF delivered_at,settled_at,retired_at ON harness_manager_notices
            WHEN NOT EXISTS(
                SELECT 1 FROM harness_manager_notices pending
                     INDEXED BY harness_manager_notices_unsettled_job
                WHERE pending.job_id=NEW.job_id AND pending.retired_at IS NULL
                  AND pending.direction='to_manager' AND pending.settled_at IS NULL
                  AND pending.delivered_at IS NULL)
            BEGIN
                DELETE FROM harness_manager_notice_transport_candidates
                WHERE job_id=NEW.job_id;
            END;
            CREATE TRIGGER harness_manager_notice_transport_candidates_no_update
            BEFORE UPDATE ON harness_manager_notice_transport_candidates
            BEGIN SELECT RAISE(ABORT,'manager notice transport candidates are immutable'); END;
            CREATE TABLE harness_manager_notice_reconcile_cursors (
                project_id TEXT NOT NULL
                    CHECK(rsi_uuid_is_canonical(project_id)),
                manager_session_id TEXT NOT NULL
                    CHECK(rsi_uuid_is_canonical(manager_session_id)),
                scope_version INTEGER NOT NULL CHECK(scope_version > 0),
                lane TEXT NOT NULL CHECK(length(lane) BETWEEN 1 AND 64),
                owner_id TEXT NOT NULL DEFAULT ''
                    CHECK(owner_id='' OR rsi_uuid_is_canonical(owner_id)),
                after_key TEXT NOT NULL DEFAULT '' CHECK(length(after_key) <= 512),
                cycle INTEGER NOT NULL DEFAULT 0 CHECK(cycle >= 0),
                updated_at TEXT NOT NULL
                    CHECK(rsi_rfc3339_nanos_is_canonical(updated_at)),
                PRIMARY KEY(project_id,manager_session_id,scope_version,lane,owner_id)
            );
            CREATE INDEX harness_manager_notice_group_epic_page
                ON sessions(parent_id,session_kind,id);
            CREATE INDEX harness_manager_notice_entity_epic_page
                ON harness_manager_v2_entities(
                    project_id,manager_session_id,scope_version,kind,session_id);
            CREATE INDEX harness_manager_notice_record_page
                ON harness_manager_v2_records(
                    project_id,manager_session_id,scope_version,epic_id,kind,
                    archived,record_key);
            CREATE TABLE harness_manager_notice_scope_retirements (
                project_id TEXT NOT NULL
                    CHECK(rsi_uuid_is_canonical(project_id)),
                manager_session_id TEXT NOT NULL
                    CHECK(rsi_uuid_is_canonical(manager_session_id)),
                scope_version INTEGER NOT NULL CHECK(scope_version > 0),
                retired_at TEXT NOT NULL
                    CHECK(rsi_rfc3339_nanos_is_canonical(retired_at)),
                completed_at TEXT
                    CHECK(completed_at IS NULL OR rsi_rfc3339_nanos_is_canonical(completed_at)),
                PRIMARY KEY(project_id,manager_session_id,scope_version)
            );
            CREATE INDEX harness_manager_notice_scope_retirements_pending
                ON harness_manager_notice_scope_retirements(
                    retired_at,project_id,manager_session_id,scope_version)
                WHERE completed_at IS NULL;
            CREATE TRIGGER harness_manager_notices_validate_update
            BEFORE UPDATE ON harness_manager_notices
            WHEN NEW.id IS NOT OLD.id OR NEW.job_id IS NOT OLD.job_id
              OR NEW.project_id IS NOT OLD.project_id
              OR NEW.manager_session_id IS NOT OLD.manager_session_id
              OR NEW.scope_version IS NOT OLD.scope_version
              OR NEW.epic_id IS NOT OLD.epic_id OR NEW.direction IS NOT OLD.direction
              OR NEW.source_session_id IS NOT OLD.source_session_id
              OR NEW.recipient_session_id IS NOT OLD.recipient_session_id
              OR NEW.kind IS NOT OLD.kind OR NEW.subject_id IS NOT OLD.subject_id
              OR NEW.subject_version IS NOT OLD.subject_version
              OR NEW.state_json IS NOT OLD.state_json
              OR NEW.recorded_at IS NOT OLD.recorded_at OR NEW.queued_at IS NOT OLD.queued_at
              OR (OLD.delivered_at IS NOT NULL AND NEW.delivered_at IS NOT OLD.delivered_at)
              OR (OLD.retrieved_at IS NOT NULL AND NEW.retrieved_at IS NOT OLD.retrieved_at)
              OR (OLD.settled_at IS NOT NULL AND NEW.settled_at IS NOT OLD.settled_at)
              OR (OLD.retired_at IS NOT NULL AND NEW.retired_at IS NOT OLD.retired_at)
              OR NEW.retrieved_at IS NOT NEW.settled_at
              OR (NEW.retired_at IS NOT NULL
                  AND (NEW.retrieved_at IS NOT NULL OR NEW.settled_at IS NOT NULL))
            BEGIN SELECT RAISE(ABORT,'manager notice identity and lifecycle are forward-only'); END;
            CREATE TRIGGER harness_manager_notices_no_delete
            BEFORE DELETE ON harness_manager_notices
            BEGIN SELECT RAISE(ABORT,'manager notices are retained'); END;
            CREATE TRIGGER harness_manager_notice_scope_retirements_forward
            BEFORE UPDATE ON harness_manager_notice_scope_retirements
            WHEN NEW.project_id IS NOT OLD.project_id
              OR NEW.manager_session_id IS NOT OLD.manager_session_id
              OR NEW.scope_version IS NOT OLD.scope_version
              OR NEW.retired_at IS NOT OLD.retired_at
              OR OLD.completed_at IS NOT NULL OR NEW.completed_at IS NULL
            BEGIN SELECT RAISE(ABORT,'manager notice scope retirement is forward-only'); END;
            CREATE TRIGGER harness_manager_notice_scope_retirements_no_delete
            BEFORE DELETE ON harness_manager_notice_scope_retirements
            BEGIN SELECT RAISE(ABORT,'manager notice scope retirements are retained'); END;",
        )?;
        let foreign_key_errors: i64 =
            tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })?;
        if foreign_key_errors != 0 {
            return Err(DaemonError::Store(format!(
                "V117 manager notice migration found {foreign_key_errors} foreign-key violation(s)"
            )));
        }
        tx.execute("PRAGMA user_version = 117", [])?;
        tx.commit()?;
        tracing::info!("V117 migration complete: durable manager notices installed");
        Ok(())
    }
    // RSI-RELEASED-MIGRATION-END: v117-manager-notices-driver

    // RSI-RELEASED-MIGRATION-BEGIN: v118-manager-notice-retirement-driver
    fn apply_manager_notice_retirement_v118_migration(&self) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if active_version != 117 {
            return Err(DaemonError::Store(format!(
                "V118 requires exact V117 source, found V{active_version}"
            )));
        }
        tx.execute_batch(
            "CREATE INDEX harness_manager_notices_pending_job_route_sequence
             ON harness_manager_notices(job_id,project_id,manager_session_id,
                 scope_version,direction,sequence)
             WHERE retired_at IS NULL AND settled_at IS NULL;",
        )?;
        tx.execute("PRAGMA user_version = 118", [])?;
        tx.commit()?;
        tracing::info!("V118 migration complete: bounded manager notice retirement installed");
        Ok(())
    }
    // RSI-RELEASED-MIGRATION-END: v118-manager-notice-retirement-driver

    // RSI-RELEASED-MIGRATION-BEGIN: v119-target-reclaim-sweep-driver
    fn apply_target_reclaim_sweep_v119_migration(&self) -> Result<()> {
        use target_reclaim_sweep::TargetReclaimSweepV119MigrationFault as Fault;

        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if active_version != 118 {
            return Err(DaemonError::Store(format!(
                "V119 requires exact V118 source, found V{active_version}"
            )));
        }
        let source_fingerprint = capacity_recovery::v88_full_catalog_fingerprint(&tx)?;
        if source_fingerprint != target_reclaim_sweep::V118_FULL_CATALOG_FINGERPRINT {
            return Err(DaemonError::Store(format!(
                "V119 requires exact V118 catalog, found {source_fingerprint}"
            )));
        }
        target_reclaim_sweep::migration_fault(Fault::AfterPreflight)?;

        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        target_reclaim_sweep::install_v119_table_and_row(&tx, &now)?;
        target_reclaim_sweep::migration_fault(Fault::AfterTableAndRow)?;
        target_reclaim_sweep::install_v119_indexes(&tx)?;
        target_reclaim_sweep::migration_fault(Fault::AfterIndexes)?;
        target_reclaim_sweep::install_v119_triggers(&tx)?;
        target_reclaim_sweep::migration_fault(Fault::AfterTriggers)?;
        target_reclaim_sweep::validate_v119_catalog(&tx)?;
        let integrity: String = tx.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        if integrity != "ok" {
            return Err(DaemonError::Store(format!(
                "V119 target reclaim migration requires integrity_check=ok, got {integrity}"
            )));
        }
        let foreign_key_errors: i64 =
            tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })?;
        if foreign_key_errors != 0 {
            return Err(DaemonError::Store(format!(
                "V119 target reclaim migration found {foreign_key_errors} foreign-key violation(s)"
            )));
        }
        target_reclaim_sweep::migration_fault(Fault::AfterChecks)?;
        tx.execute("PRAGMA user_version = 119", [])?;
        target_reclaim_sweep::migration_fault(Fault::AfterUserVersion)?;
        let result_fingerprint = capacity_recovery::v88_full_catalog_fingerprint(&tx)?;
        if result_fingerprint != target_reclaim_sweep::V119_FULL_CATALOG_FINGERPRINT {
            return Err(DaemonError::Store(format!(
                "V119 target reclaim catalog fingerprint mismatch: {result_fingerprint}"
            )));
        }
        target_reclaim_sweep::migration_fault(Fault::BeforeCommit)?;
        tx.commit()?;
        tracing::info!(%result_fingerprint, "V119 migration complete: durable target reclaim sweep installed");
        Ok(())
    }
    // RSI-RELEASED-MIGRATION-END: v119-target-reclaim-sweep-driver

    // RSI-RELEASED-MIGRATION-BEGIN: v120-source-worktree-batch-driver
    fn apply_source_worktree_v120_migration(&self) -> Result<()> {
        source_worktree_v120::apply_v120_migration(self)
    }
    // RSI-RELEASED-MIGRATION-END: v120-source-worktree-batch-driver

    // RSI-RELEASED-MIGRATION-BEGIN: v121-manager-review-driver
    fn apply_manager_review_v121_migration(&self) -> Result<()> {
        manager_review_v121::apply_v121_migration(self)
    }
    // RSI-RELEASED-MIGRATION-END: v121-manager-review-driver

    fn apply_pioneer_v93_migration(&self) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if active_version != 92 {
            return Err(DaemonError::Store(format!(
                "V93 requires exact V92 source, found V{active_version}"
            )));
        }
        h1_v92_validate_catalog(&tx)?;

        tx.execute_batch(
            "CREATE TABLE recursive_live_attempts_v93_new (
                id TEXT PRIMARY KEY,
                graph_id TEXT NOT NULL REFERENCES recursive_task_graphs(id) ON DELETE CASCADE,
                task_id TEXT NOT NULL REFERENCES recursive_task_nodes(id) ON DELETE CASCADE,
                scheduler_run_id TEXT NOT NULL REFERENCES recursive_scheduler_runs(id) ON DELETE CASCADE,
                attempt_id TEXT NOT NULL UNIQUE REFERENCES recursive_task_attempts(id) ON DELETE CASCADE,
                phase TEXT NOT NULL CHECK (phase IN ('execute', 'integrate')),
                attempt_no INTEGER NOT NULL CHECK (attempt_no > 0),
                session_id TEXT REFERENCES sessions(id),
                provider TEXT CHECK (
                    provider IS NULL OR provider IN (
                        'Claude', 'Codex', 'Pioneer', 'Local', 'Antigravity', 'Gemini', 'CodexAppServer', 'Harness'
                    )
                ),
                model TEXT CHECK (model IS NULL OR length(trim(model)) > 0),
                sandbox_kind TEXT CHECK (
                    sandbox_kind IS NULL OR sandbox_kind IN ('None', 'GitWorktree')
                ),
                sandbox_root TEXT,
                sandbox_branch TEXT,
                sandbox_worktree_id TEXT,
                workflow_id TEXT REFERENCES workflows(id),
                topology_id TEXT REFERENCES topologies(id),
                workflow_execution_id TEXT,
                topology_workflow_id TEXT REFERENCES workflows(id),
                execution_mode TEXT NOT NULL CHECK (execution_mode IN ('live_session')),
                status TEXT NOT NULL CHECK (status IN (
                    'created', 'launching', 'running', 'waiting_approval',
                    'succeeded', 'decomposed', 'failed', 'blocked',
                    'cancelled', 'interrupted', 'lost', 'recovery_pending'
                )),
                recovery_status TEXT NOT NULL DEFAULT 'none' CHECK (
                    recovery_status IN ('none', 'pending', 'recovered', 'lost', 'quarantined')
                ),
                prompt_artifact_id INTEGER REFERENCES recursive_execution_artifacts(id),
                output_artifact_id INTEGER REFERENCES recursive_execution_artifacts(id),
                diff_artifact_id INTEGER REFERENCES recursive_execution_artifacts(id),
                test_artifact_id INTEGER REFERENCES recursive_execution_artifacts(id),
                cancellation_request_id TEXT REFERENCES recursive_cancellation_requests(id),
                lease_owner TEXT,
                lease_token TEXT,
                heartbeat_at TEXT,
                lease_expires_at TEXT,
                max_wall_time_ms INTEGER CHECK (
                    max_wall_time_ms IS NULL OR max_wall_time_ms > 0
                ),
                created_at TEXT NOT NULL,
                started_at TEXT,
                launched_at TEXT,
                completed_at TEXT,
                recovery_checked_at TEXT,
                recovered_at TEXT,
                failure_reason TEXT,
                interruption_reason TEXT,
                cancellation_reason TEXT,
                recovery_reason TEXT,
                error TEXT,
                updated_at TEXT NOT NULL,
                CHECK (
                    (
                        status IN (
                            'succeeded', 'decomposed', 'failed', 'blocked',
                            'cancelled', 'interrupted', 'lost'
                        )
                        AND completed_at IS NOT NULL
                    )
                    OR (
                        status NOT IN (
                            'succeeded', 'decomposed', 'failed', 'blocked',
                            'cancelled', 'interrupted', 'lost'
                        )
                        AND completed_at IS NULL
                    )
                ),
                UNIQUE (graph_id, task_id, phase, attempt_no)
            );
            INSERT INTO recursive_live_attempts_v93_new (
                id, graph_id, task_id, scheduler_run_id, attempt_id, phase,
                attempt_no, session_id, provider, model, sandbox_kind, sandbox_root,
                sandbox_branch, sandbox_worktree_id, workflow_id, topology_id,
                workflow_execution_id, topology_workflow_id, execution_mode, status,
                recovery_status, prompt_artifact_id, output_artifact_id, diff_artifact_id,
                test_artifact_id, cancellation_request_id, lease_owner, lease_token,
                heartbeat_at, lease_expires_at, max_wall_time_ms, created_at, started_at,
                launched_at, completed_at, recovery_checked_at, recovered_at,
                failure_reason, interruption_reason, cancellation_reason, recovery_reason,
                error, updated_at
            )
            SELECT
                id, graph_id, task_id, scheduler_run_id, attempt_id, phase,
                attempt_no, session_id, provider, model, sandbox_kind, sandbox_root,
                sandbox_branch, sandbox_worktree_id, workflow_id, topology_id,
                workflow_execution_id, topology_workflow_id, execution_mode, status,
                recovery_status, prompt_artifact_id, output_artifact_id, diff_artifact_id,
                test_artifact_id, cancellation_request_id, lease_owner, lease_token,
                heartbeat_at, lease_expires_at, max_wall_time_ms, created_at, started_at,
                launched_at, completed_at, recovery_checked_at, recovered_at,
                failure_reason, interruption_reason, cancellation_reason, recovery_reason,
                error, updated_at
            FROM recursive_live_attempts;
            DROP TABLE recursive_live_attempts;
            ALTER TABLE recursive_live_attempts_v93_new RENAME TO recursive_live_attempts;
            CREATE INDEX idx_recursive_live_attempts_graph
                ON recursive_live_attempts(graph_id, created_at DESC, id);
            CREATE INDEX idx_recursive_live_attempts_task
                ON recursive_live_attempts(task_id, created_at DESC, id);
            CREATE INDEX idx_recursive_live_attempts_run
                ON recursive_live_attempts(scheduler_run_id, created_at DESC, id);
            CREATE INDEX idx_recursive_live_attempts_status
                ON recursive_live_attempts(status, updated_at DESC, id);
            CREATE UNIQUE INDEX idx_recursive_live_attempts_session
                ON recursive_live_attempts(session_id) WHERE session_id IS NOT NULL;
            CREATE INDEX idx_recursive_live_attempts_heartbeat_expiry
                ON recursive_live_attempts(status, lease_expires_at, heartbeat_at, id)
                WHERE lease_token IS NOT NULL
                  AND heartbeat_at IS NOT NULL
                  AND lease_expires_at IS NOT NULL;",
        )?;

        let integrity: String = tx.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        if integrity != "ok" {
            return Err(DaemonError::Store(format!(
                "V93 requires integrity_check=ok, got {integrity}"
            )));
        }
        let foreign_key_errors: i64 =
            tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })?;
        if foreign_key_errors != 0 {
            return Err(DaemonError::Store(format!(
                "V93 Pioneer provider migration found {foreign_key_errors} foreign-key violation(s)"
            )));
        }
        tx.execute("PRAGMA user_version = 93", [])?;
        tx.commit()?;
        tracing::info!("V93 migration complete: Pioneer recursive-live provider admission");
        Ok(())
    }

    // RSI-RELEASED-MIGRATION-BEGIN: v126-bedrock-provider-driver
    fn apply_bedrock_v126_migration(&self) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if active_version != 125 {
            return Err(DaemonError::Store(format!(
                "V126 requires exact V125 source, found V{active_version}"
            )));
        }

        tx.execute_batch(
            "CREATE TABLE recursive_live_attempts_v126_new (
                id TEXT PRIMARY KEY,
                graph_id TEXT NOT NULL REFERENCES recursive_task_graphs(id) ON DELETE CASCADE,
                task_id TEXT NOT NULL REFERENCES recursive_task_nodes(id) ON DELETE CASCADE,
                scheduler_run_id TEXT NOT NULL REFERENCES recursive_scheduler_runs(id) ON DELETE CASCADE,
                attempt_id TEXT NOT NULL UNIQUE REFERENCES recursive_task_attempts(id) ON DELETE CASCADE,
                phase TEXT NOT NULL CHECK (phase IN ('execute', 'integrate')),
                attempt_no INTEGER NOT NULL CHECK (attempt_no > 0),
                session_id TEXT REFERENCES sessions(id),
                provider TEXT CHECK (
                    provider IS NULL OR provider IN (
                        'Claude', 'Codex', 'Pioneer', 'OpenRouter', 'Bedrock', 'Local', 'Antigravity', 'Gemini', 'CodexAppServer', 'Harness'
                    )
                ),
                model TEXT CHECK (model IS NULL OR length(trim(model)) > 0),
                sandbox_kind TEXT CHECK (
                    sandbox_kind IS NULL OR sandbox_kind IN ('None', 'GitWorktree')
                ),
                sandbox_root TEXT,
                sandbox_branch TEXT,
                sandbox_worktree_id TEXT,
                workflow_id TEXT REFERENCES workflows(id),
                topology_id TEXT REFERENCES topologies(id),
                workflow_execution_id TEXT,
                topology_workflow_id TEXT REFERENCES workflows(id),
                execution_mode TEXT NOT NULL CHECK (execution_mode IN ('live_session')),
                status TEXT NOT NULL CHECK (status IN (
                    'created', 'launching', 'running', 'waiting_approval',
                    'succeeded', 'decomposed', 'failed', 'blocked',
                    'cancelled', 'interrupted', 'lost', 'recovery_pending'
                )),
                recovery_status TEXT NOT NULL DEFAULT 'none' CHECK (
                    recovery_status IN ('none', 'pending', 'recovered', 'lost', 'quarantined')
                ),
                prompt_artifact_id INTEGER REFERENCES recursive_execution_artifacts(id),
                output_artifact_id INTEGER REFERENCES recursive_execution_artifacts(id),
                diff_artifact_id INTEGER REFERENCES recursive_execution_artifacts(id),
                test_artifact_id INTEGER REFERENCES recursive_execution_artifacts(id),
                cancellation_request_id TEXT REFERENCES recursive_cancellation_requests(id),
                lease_owner TEXT,
                lease_token TEXT,
                heartbeat_at TEXT,
                lease_expires_at TEXT,
                max_wall_time_ms INTEGER CHECK (
                    max_wall_time_ms IS NULL OR max_wall_time_ms > 0
                ),
                created_at TEXT NOT NULL,
                started_at TEXT,
                launched_at TEXT,
                completed_at TEXT,
                recovery_checked_at TEXT,
                recovered_at TEXT,
                failure_reason TEXT,
                interruption_reason TEXT,
                cancellation_reason TEXT,
                recovery_reason TEXT,
                error TEXT,
                updated_at TEXT NOT NULL,
                CHECK (
                    (
                        status IN (
                            'succeeded', 'decomposed', 'failed', 'blocked',
                            'cancelled', 'interrupted', 'lost'
                        )
                        AND completed_at IS NOT NULL
                    )
                    OR (
                        status NOT IN (
                            'succeeded', 'decomposed', 'failed', 'blocked',
                            'cancelled', 'interrupted', 'lost'
                        )
                        AND completed_at IS NULL
                    )
                ),
                UNIQUE (graph_id, task_id, phase, attempt_no)
            );
            INSERT INTO recursive_live_attempts_v126_new (
                id, graph_id, task_id, scheduler_run_id, attempt_id, phase,
                attempt_no, session_id, provider, model, sandbox_kind, sandbox_root,
                sandbox_branch, sandbox_worktree_id, workflow_id, topology_id,
                workflow_execution_id, topology_workflow_id, execution_mode, status,
                recovery_status, prompt_artifact_id, output_artifact_id, diff_artifact_id,
                test_artifact_id, cancellation_request_id, lease_owner, lease_token,
                heartbeat_at, lease_expires_at, max_wall_time_ms, created_at, started_at,
                launched_at, completed_at, recovery_checked_at, recovered_at,
                failure_reason, interruption_reason, cancellation_reason, recovery_reason,
                error, updated_at
            )
            SELECT
                id, graph_id, task_id, scheduler_run_id, attempt_id, phase,
                attempt_no, session_id, provider, model, sandbox_kind, sandbox_root,
                sandbox_branch, sandbox_worktree_id, workflow_id, topology_id,
                workflow_execution_id, topology_workflow_id, execution_mode, status,
                recovery_status, prompt_artifact_id, output_artifact_id, diff_artifact_id,
                test_artifact_id, cancellation_request_id, lease_owner, lease_token,
                heartbeat_at, lease_expires_at, max_wall_time_ms, created_at, started_at,
                launched_at, completed_at, recovery_checked_at, recovered_at,
                failure_reason, interruption_reason, cancellation_reason, recovery_reason,
                error, updated_at
            FROM recursive_live_attempts;
            DROP TABLE recursive_live_attempts;
            ALTER TABLE recursive_live_attempts_v126_new RENAME TO recursive_live_attempts;
            CREATE INDEX idx_recursive_live_attempts_graph
                ON recursive_live_attempts(graph_id, created_at DESC, id);
            CREATE INDEX idx_recursive_live_attempts_task
                ON recursive_live_attempts(task_id, created_at DESC, id);
            CREATE INDEX idx_recursive_live_attempts_run
                ON recursive_live_attempts(scheduler_run_id, created_at DESC, id);
            CREATE INDEX idx_recursive_live_attempts_status
                ON recursive_live_attempts(status, updated_at DESC, id);
            CREATE UNIQUE INDEX idx_recursive_live_attempts_session
                ON recursive_live_attempts(session_id) WHERE session_id IS NOT NULL;
            CREATE INDEX idx_recursive_live_attempts_heartbeat_expiry
                ON recursive_live_attempts(status, lease_expires_at, heartbeat_at, id)
                WHERE lease_token IS NOT NULL
                  AND heartbeat_at IS NOT NULL
                  AND lease_expires_at IS NOT NULL;",
        )?;

        let integrity: String = tx.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        if integrity != "ok" {
            return Err(DaemonError::Store(format!(
                "V126 requires integrity_check=ok, got {integrity}"
            )));
        }
        let foreign_key_errors: i64 =
            tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })?;
        if foreign_key_errors != 0 {
            return Err(DaemonError::Store(format!(
                "V126 Bedrock provider migration found {foreign_key_errors} foreign-key violation(s)"
            )));
        }
        tx.execute("PRAGMA user_version = 126", [])?;
        tx.commit()?;
        tracing::info!("V126 migration complete: Bedrock recursive-live provider admission");
        Ok(())
    }

    // RSI-RELEASED-MIGRATION-END: v126-bedrock-provider-driver

    /// The V82 mailbox amendment (P2-06d), factored out of `init_schema` so the
    /// caller can restore `PRAGMA foreign_keys` on every exit path — including
    /// the early `return Err(...)`s below, which an inline block could not.
    ///
    /// Ordering mirrors the V81 block deliberately: exact-source check, exact
    /// SOURCE-catalog fingerprint, DDL, drained `PRAGMA foreign_key_check`,
    /// version write, then the RESULT fingerprint. The result fingerprint is
    /// computed AFTER the version write because `v81_schema_fingerprint`
    /// absorbs the live `PRAGMA user_version`; computing it before would pin a
    /// digest no later reopen could ever reproduce. A mismatch still aborts the
    /// whole transaction, so nothing is durable.
    fn apply_agent_message_v82_migration(&self) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if active_version != 81 {
            return Err(DaemonError::Store(format!(
                "V82 requires exact V81 source, found V{active_version}"
            )));
        }
        #[cfg(test)]
        agent_message_v82_migration_fault(AgentMessageV82MigrationFault::AfterExactSource)?;

        // The SHIPPED V81 catalog must be byte-exact before the first DDL
        // statement. This is also what keeps the V81 pin honest work rather
        // than dead history: it is re-proved on every fresh open, and on the
        // operator's own database at the moment V82 runs.
        let v81_fingerprint = agent_coordination::v81_schema_fingerprint(&tx)?;
        if v81_fingerprint != agent_coordination::AGENT_MESSAGE_V81_PINNED_FINGERPRINT {
            return Err(DaemonError::Store(format!(
                "V82 requires the exact shipped V81 mailbox catalog, found {v81_fingerprint}"
            )));
        }
        #[cfg(test)]
        agent_message_v82_migration_fault(AgentMessageV82MigrationFault::AfterV81Fingerprint)?;

        agent_coordination::install_v82_amendments(&tx)?;
        #[cfg(test)]
        agent_message_v82_migration_fault(AgentMessageV82MigrationFault::AfterSchema)?;

        // `install_v82_amendments` performs the rebuild copy and its row-count
        // parity check; this failpoint sits on the far side of both.
        #[cfg(test)]
        agent_message_v82_migration_fault(AgentMessageV82MigrationFault::AfterCopy)?;

        // Deferred foreign keys must be fully drained inside the transaction,
        // not merely counted. `pragma_foreign_key_check` is an explicit sweep
        // and is unaffected by `foreign_keys=OFF`, so this is a real referential
        // proof over the rebuilt relation and its four referrers.
        let foreign_key_errors: i64 =
            tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })?;
        if foreign_key_errors != 0 {
            return Err(DaemonError::Store(
                "V82 agent message foreign-key check failed".into(),
            ));
        }
        #[cfg(test)]
        agent_message_v82_migration_fault(AgentMessageV82MigrationFault::AfterForeignKeyCheck)?;

        tx.execute(
            &format!(
                "PRAGMA user_version = {}",
                agent_coordination::AGENT_MESSAGE_V82_USER_VERSION
            ),
            [],
        )?;
        #[cfg(test)]
        agent_message_v82_migration_fault(AgentMessageV82MigrationFault::AfterUserVersion)?;

        let schema_fingerprint = agent_coordination::v81_schema_fingerprint(&tx)?;
        if schema_fingerprint != agent_coordination::AGENT_MESSAGE_V82_PINNED_FINGERPRINT {
            return Err(DaemonError::Store(format!(
                "V82 agent message semantic fingerprint mismatch: {schema_fingerprint}"
            )));
        }
        #[cfg(test)]
        agent_message_v82_migration_fault(AgentMessageV82MigrationFault::AfterFingerprint)?;
        #[cfg(test)]
        agent_message_v82_migration_fault(AgentMessageV82MigrationFault::BeforeCommit)?;
        tx.commit()?;
        tracing::info!(
            %schema_fingerprint,
            "V82 migration complete: reconciler no-effect backstop and sealed-uncertain acknowledgement guard"
        );
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

    fn repair_v73_model_control_schema(&self) -> Result<()> {
        // IMMEDIATE, not DEFERRED. This repair runs unconditionally on every
        // `Store::open()` (outside every `if version < N` gate), and its first
        // statement is a read (`pragma_table_info` via
        // `add_column_if_not_exists_tx`) followed by DDL writes. A DEFERRED
        // transaction therefore takes a WAL read snapshot and then tries to
        // upgrade read -> write, which SQLite reports as SQLITE_BUSY_SNAPSHOT
        // (extended code 517) — and it deliberately does NOT invoke the busy
        // handler for that upgrade, because sleeping while holding a read
        // snapshot could deadlock. That made `PRAGMA busy_timeout` inert and
        // let two concurrent opens of the same database fail instantly.
        // BEGIN IMMEDIATE takes the write lock up front, which DOES honor
        // busy_timeout, so concurrent openers serialize instead of racing.
        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        add_column_if_not_exists_tx(&tx, "sessions", "model_invocation_id", "TEXT")?;
        tx.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS daemon_settings (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS model_invocations (
                id TEXT PRIMARY KEY,
                purpose TEXT NOT NULL,
                invocation_kind TEXT NOT NULL,
                foreground TEXT NOT NULL,
                paid_risk TEXT NOT NULL,
                admission_status TEXT NOT NULL,
                status TEXT NOT NULL,
                provider TEXT,
                model TEXT,
                backend TEXT,
                model_tier TEXT,
                effort TEXT,
                trigger_source TEXT NOT NULL,
                session_id TEXT,
                project_id TEXT,
                workflow_id TEXT,
                scheduled_job_id TEXT,
                issue_tracker_id TEXT,
                issue_identifier TEXT,
                topology_node_id TEXT,
                recursive_graph_id TEXT,
                recursive_task_id TEXT,
                recursive_attempt_id TEXT,
                operator TEXT,
                parent_invocation_id TEXT,
                retry_of_invocation_id TEXT,
                dedup_key TEXT,
                request_fingerprint TEXT,
                policy_snapshot_json TEXT NOT NULL DEFAULT '{}',
                error_class TEXT,
                reserved_input_tokens INTEGER NOT NULL DEFAULT 0,
                reserved_output_tokens INTEGER NOT NULL DEFAULT 0,
                reserved_cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
                reserved_cache_read_tokens INTEGER NOT NULL DEFAULT 0,
                reserved_reasoning_tokens INTEGER NOT NULL DEFAULT 0,
                reserved_embedding_input_count INTEGER NOT NULL DEFAULT 0,
                reserved_wall_time_ms INTEGER NOT NULL DEFAULT 0,
                input_tokens INTEGER,
                output_tokens INTEGER,
                cache_creation_tokens INTEGER,
                cache_read_tokens INTEGER,
                reasoning_tokens INTEGER,
                embedding_input_count INTEGER,
                wall_time_ms INTEGER,
                estimated_cost_usd REAL,
                usage_confidence TEXT NOT NULL DEFAULT 'unavailable',
                baseline_input_tokens INTEGER NOT NULL DEFAULT 0,
                baseline_output_tokens INTEGER NOT NULL DEFAULT 0,
                baseline_cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
                baseline_cache_read_tokens INTEGER NOT NULL DEFAULT 0,
                baseline_reasoning_tokens INTEGER NOT NULL DEFAULT 0,
                baseline_embedding_input_count INTEGER NOT NULL DEFAULT 0,
                baseline_wall_time_ms INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL,
                started_at TEXT,
                completed_at TEXT
            );
            CREATE UNIQUE INDEX IF NOT EXISTS idx_model_invocations_dedup
                ON model_invocations(dedup_key) WHERE dedup_key IS NOT NULL;
            CREATE INDEX IF NOT EXISTS idx_model_invocations_session_id
                ON model_invocations(session_id);
            CREATE INDEX IF NOT EXISTS idx_model_invocations_purpose
                ON model_invocations(purpose);

            CREATE TABLE IF NOT EXISTS model_budget_policies (
                policy_key TEXT PRIMARY KEY,
                scope_kind TEXT NOT NULL,
                scope_id TEXT NOT NULL,
                purpose TEXT,
                model_tier TEXT,
                effort TEXT,
                ceiling_model_tier TEXT,
                ceiling_effort TEXT,
                max_calls INTEGER,
                max_total_tokens INTEGER,
                max_input_tokens INTEGER,
                max_output_tokens INTEGER,
                max_cache_creation_tokens INTEGER,
                max_cache_read_tokens INTEGER,
                max_reasoning_tokens INTEGER,
                max_embedding_inputs INTEGER,
                max_wall_time_ms INTEGER,
                max_concurrency INTEGER,
                max_retries INTEGER,
                max_calls_per_window INTEGER,
                rate_window_seconds INTEGER,
                updated_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS model_budget_counters (
                counter_key TEXT PRIMARY KEY,
                scope_kind TEXT NOT NULL,
                scope_id TEXT NOT NULL,
                purpose TEXT NOT NULL,
                model_tier TEXT NOT NULL,
                effort TEXT,
                call_count INTEGER NOT NULL DEFAULT 0,
                active_count INTEGER NOT NULL DEFAULT 0,
                input_tokens INTEGER NOT NULL DEFAULT 0,
                output_tokens INTEGER NOT NULL DEFAULT 0,
                cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
                cache_read_tokens INTEGER NOT NULL DEFAULT 0,
                reasoning_tokens INTEGER NOT NULL DEFAULT 0,
                embedding_inputs INTEGER NOT NULL DEFAULT 0,
                wall_time_ms INTEGER NOT NULL DEFAULT 0,
                rate_window_started_at TEXT,
                rate_window_call_count INTEGER NOT NULL DEFAULT 0,
                updated_at TEXT NOT NULL
            );
            ",
        )?;
        for (column, col_type) in [
            ("reserved_input_tokens", "INTEGER NOT NULL DEFAULT 0"),
            ("reserved_output_tokens", "INTEGER NOT NULL DEFAULT 0"),
            (
                "reserved_cache_creation_tokens",
                "INTEGER NOT NULL DEFAULT 0",
            ),
            ("reserved_cache_read_tokens", "INTEGER NOT NULL DEFAULT 0"),
            ("reserved_reasoning_tokens", "INTEGER NOT NULL DEFAULT 0"),
            (
                "reserved_embedding_input_count",
                "INTEGER NOT NULL DEFAULT 0",
            ),
            ("reserved_wall_time_ms", "INTEGER NOT NULL DEFAULT 0"),
            ("baseline_input_tokens", "INTEGER NOT NULL DEFAULT 0"),
            ("baseline_output_tokens", "INTEGER NOT NULL DEFAULT 0"),
            (
                "baseline_cache_creation_tokens",
                "INTEGER NOT NULL DEFAULT 0",
            ),
            ("baseline_cache_read_tokens", "INTEGER NOT NULL DEFAULT 0"),
            ("baseline_reasoning_tokens", "INTEGER NOT NULL DEFAULT 0"),
            (
                "baseline_embedding_input_count",
                "INTEGER NOT NULL DEFAULT 0",
            ),
            ("baseline_wall_time_ms", "INTEGER NOT NULL DEFAULT 0"),
        ] {
            add_column_if_not_exists_tx(&tx, "model_invocations", column, col_type)?;
        }
        for (column, col_type) in [
            ("ceiling_model_tier", "TEXT"),
            ("ceiling_effort", "TEXT"),
            ("max_cache_creation_tokens", "INTEGER"),
            ("max_cache_read_tokens", "INTEGER"),
            ("max_reasoning_tokens", "INTEGER"),
            ("max_calls_per_window", "INTEGER"),
            ("rate_window_seconds", "INTEGER"),
        ] {
            add_column_if_not_exists_tx(&tx, "model_budget_policies", column, col_type)?;
        }
        for (column, col_type) in [
            ("cache_creation_tokens", "INTEGER NOT NULL DEFAULT 0"),
            ("cache_read_tokens", "INTEGER NOT NULL DEFAULT 0"),
            ("reasoning_tokens", "INTEGER NOT NULL DEFAULT 0"),
            ("rate_window_started_at", "TEXT"),
            ("rate_window_call_count", "INTEGER NOT NULL DEFAULT 0"),
        ] {
            add_column_if_not_exists_tx(&tx, "model_budget_counters", column, col_type)?;
        }
        if sqlite_table_exists_tx(&tx, "sessions")? {
            let exprs = v73_session_exprs(&tx)?;
            let backfill_sql = format!(
                "
                INSERT INTO model_invocations (
                    id, purpose, invocation_kind, foreground, paid_risk, admission_status, status,
                    provider, model, backend, model_tier, effort, trigger_source,
                    session_id, project_id, workflow_id, scheduled_job_id,
                    issue_tracker_id, issue_identifier, policy_snapshot_json,
                    input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
                    wall_time_ms, estimated_cost_usd, usage_confidence, dedup_key, created_at, started_at, completed_at
                )
                SELECT
                    lower(substr(hex(randomblob(16)),1,8) || '-' ||
                          substr(hex(randomblob(16)),1,4) || '-' ||
                          substr(hex(randomblob(16)),1,4) || '-' ||
                          substr(hex(randomblob(16)),1,4) || '-' ||
                          substr(hex(randomblob(16)),1,12)),
                    'session.launch.fresh',
                    'session_lifecycle',
                    'foreground',
                    'paid_capable',
                    'admitted',
                    CASE
                        WHEN {status_expr} = 'Failed' THEN 'failed'
                        WHEN {status_expr} IN ('Completed', 'Archived', 'Interrupted') THEN 'completed'
                        ELSE 'completed'
                    END,
                    {provider_expr},
                    {model_expr},
                    {backend_expr},
                    {model_tier_expr},
                    {effort_expr},
                    'legacy_backfill',
                    id,
                    {project_id_expr},
                    {workflow_id_expr},
                    {scheduled_job_id_expr},
                    {issue_tracker_id_expr},
                    {issue_identifier_expr},
                    json('{{\"source\":\"v73_backfill\",\"confidence\":\"stale\"}}'),
                    {total_input_tokens_expr},
                    {total_output_tokens_expr},
                    {total_cache_creation_tokens_expr},
                    {total_cache_read_tokens_expr},
                    {work_time_ms_expr},
                    {cost_usd_expr},
                    CASE
                        WHEN {total_input_tokens_expr} IS NOT NULL
                          OR {total_output_tokens_expr} IS NOT NULL
                          OR {total_cache_creation_tokens_expr} IS NOT NULL
                          OR {total_cache_read_tokens_expr} IS NOT NULL
                        THEN 'stale'
                        ELSE 'unavailable'
                    END,
                    'legacy-session:' || id,
                    {created_at_expr},
                    {created_at_expr},
                    {updated_at_expr}
                FROM sessions
                WHERE {session_kind_filter}
                  AND model_invocation_id IS NULL
                  AND NOT EXISTS (
                      SELECT 1
                      FROM model_invocations
                      WHERE dedup_key = 'legacy-session:' || sessions.id
                  );

                UPDATE sessions
                SET model_invocation_id = (
                    SELECT id FROM model_invocations
                    WHERE model_invocations.session_id = sessions.id
                      AND model_invocations.dedup_key = 'legacy-session:' || sessions.id
                    LIMIT 1
                )
                WHERE model_invocation_id IS NULL
                  AND {session_kind_filter};
                ",
                status_expr = exprs.status_expr,
                provider_expr = exprs.provider_expr,
                model_expr = exprs.model_expr,
                backend_expr = exprs.backend_expr,
                model_tier_expr = exprs.model_tier_expr,
                effort_expr = exprs.effort_expr,
                project_id_expr = exprs.project_id_expr,
                workflow_id_expr = exprs.workflow_id_expr,
                scheduled_job_id_expr = exprs.scheduled_job_id_expr,
                issue_tracker_id_expr = exprs.issue_tracker_id_expr,
                issue_identifier_expr = exprs.issue_identifier_expr,
                total_input_tokens_expr = exprs.total_input_tokens_expr,
                total_output_tokens_expr = exprs.total_output_tokens_expr,
                total_cache_creation_tokens_expr = exprs.total_cache_creation_tokens_expr,
                total_cache_read_tokens_expr = exprs.total_cache_read_tokens_expr,
                work_time_ms_expr = exprs.work_time_ms_expr,
                cost_usd_expr = exprs.cost_usd_expr,
                created_at_expr = exprs.created_at_expr,
                updated_at_expr = exprs.updated_at_expr,
                session_kind_filter = exprs.session_kind_filter,
            );
            tx.execute_batch(&backfill_sql)?;
        }
        tx.execute(
            "INSERT INTO daemon_settings (key, value, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO NOTHING",
            params![
                crate::store::model_control::KEY_MODEL_CONTROL_MODE,
                "normal",
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
            ],
        )?;
        tx.commit()?;
        Ok(())
    }
}

fn d05_v79_uuid_guard(column: &str, optional: bool) -> String {
    let guard = format!(
        "length(NEW.{column})=36 AND NEW.{column}!='00000000-0000-0000-0000-000000000000' \
         AND NEW.{column}=lower(NEW.{column}) \
         AND substr(NEW.{column},9,1)='-' AND substr(NEW.{column},14,1)='-' \
         AND substr(NEW.{column},19,1)='-' AND substr(NEW.{column},24,1)='-' \
         AND replace(NEW.{column},'-','') NOT GLOB '*[^0-9a-f]*'"
    );
    if optional {
        format!("(NEW.{column} IS NULL OR ({guard}))")
    } else {
        format!("({guard})")
    }
}

fn d05_v79_timestamp_guard(column: &str, optional: bool) -> String {
    let guard = format!(
        "length(NEW.{column})=30 \
         AND strftime('%Y-%m-%dT%H:%M:%S',NEW.{column})=substr(NEW.{column},1,19) \
         AND substr(NEW.{column},20,1)='.' \
         AND substr(NEW.{column},21,9) NOT GLOB '*[^0-9]*' \
         AND substr(NEW.{column},30,1)='Z'"
    );
    if optional {
        format!("(NEW.{column} IS NULL OR ({guard}))")
    } else {
        format!("({guard})")
    }
}

fn install_d05_v79_validation_triggers(tx: &Transaction<'_>) -> Result<()> {
    struct GuardSpec {
        table: &'static str,
        required_uuids: &'static [&'static str],
        optional_uuids: &'static [&'static str],
        required_times: &'static [&'static str],
        optional_times: &'static [&'static str],
        mutable: bool,
        extra: Option<&'static str>,
    }

    let specs = [
        GuardSpec {
            table: "idea_program_runs",
            required_uuids: &["id", "project_id", "idea_id", "controller_session_id"],
            optional_uuids: &[],
            required_times: &["created_at", "updated_at"],
            optional_times: &["settled_at", "cancelled_at", "failed_at"],
            mutable: true,
            extra: None,
        },
        GuardSpec {
            table: "idea_program_run_transitions",
            required_uuids: &["id", "program_run_id", "idea_event_id"],
            optional_uuids: &["actor_session_id"],
            required_times: &["created_at"],
            optional_times: &[],
            mutable: false,
            extra: None,
        },
        GuardSpec {
            table: "idea_program_run_gates",
            required_uuids: &["id", "program_run_id", "transition_id"],
            optional_uuids: &["evaluator_session_id"],
            required_times: &["created_at"],
            optional_times: &[],
            mutable: false,
            extra: None,
        },
        GuardSpec {
            table: "idea_program_run_budgets",
            required_uuids: &["program_run_id"],
            optional_uuids: &[],
            required_times: &["updated_at"],
            optional_times: &[],
            mutable: true,
            extra: None,
        },
        GuardSpec {
            table: "idea_program_run_locks",
            required_uuids: &[
                "id",
                "project_id",
                "program_run_id",
                "requesting_transition_id",
                "controller_session_id",
            ],
            optional_uuids: &["owner_boot_id"],
            required_times: &["requested_at"],
            optional_times: &["acquired_at", "heartbeat_at", "expires_at", "released_at"],
            mutable: true,
            extra: None,
        },
        GuardSpec {
            table: "idea_program_run_actions",
            required_uuids: &[
                "id",
                "program_run_id",
                "creating_transition_id",
                "controller_session_id",
            ],
            optional_uuids: &[
                "claim_boot_id",
                "external_model_invocation_id",
                "external_session_id",
                "scheduled_job_id",
            ],
            required_times: &["not_before", "created_at", "updated_at"],
            optional_times: &[
                "claimed_at",
                "claim_expires_at",
                "published_at",
                "acknowledged_at",
            ],
            mutable: true,
            extra: Some(
                "((NEW.state='reserved' AND NEW.claim_run_version IS NULL AND NEW.claim_lease_generation IS NULL) \
                  OR (NEW.state IN ('claimed','published') AND NEW.claim_run_version IS NOT NULL AND NEW.claim_lease_generation IS NOT NULL) \
                  OR (NEW.state='acknowledged' AND ((NEW.claim_run_version IS NULL AND NEW.claim_lease_generation IS NULL) \
                    OR (NEW.claim_run_version IS NOT NULL AND NEW.claim_lease_generation IS NOT NULL))) \
                  OR NEW.state IN ('failed','cancelled'))",
            ),
        },
        GuardSpec {
            table: "idea_program_run_attempt_refs",
            required_uuids: &[
                "id",
                "program_run_id",
                "action_id",
                "creating_transition_id",
            ],
            optional_uuids: &[
                "session_id",
                "model_invocation_id",
                "completing_transition_id",
            ],
            required_times: &["created_at", "updated_at"],
            optional_times: &["observed_at"],
            mutable: true,
            extra: None,
        },
    ];

    for spec in specs {
        let mut guards = Vec::new();
        guards.extend(
            spec.required_uuids
                .iter()
                .map(|column| d05_v79_uuid_guard(column, false)),
        );
        guards.extend(
            spec.optional_uuids
                .iter()
                .map(|column| d05_v79_uuid_guard(column, true)),
        );
        guards.extend(
            spec.required_times
                .iter()
                .map(|column| d05_v79_timestamp_guard(column, false)),
        );
        guards.extend(
            spec.optional_times
                .iter()
                .map(|column| d05_v79_timestamp_guard(column, true)),
        );
        if let Some(extra) = spec.extra {
            guards.push(extra.to_string());
        }
        let predicate = guards.join(" AND ");
        for operation in if spec.mutable {
            &["INSERT", "UPDATE"][..]
        } else {
            &["INSERT"][..]
        } {
            let suffix = operation.to_ascii_lowercase();
            tx.execute_batch(&format!(
                "CREATE TRIGGER {table}_v79_validate_{suffix} BEFORE {operation} ON {table} \
                 WHEN NOT ({predicate}) BEGIN SELECT RAISE(ABORT,'V79 invalid ProgramRun identity, timestamp, or state'); END;",
                table = spec.table,
            ))?;
        }
    }
    Ok(())
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

struct V73SessionExprs {
    status_expr: String,
    provider_expr: String,
    model_expr: String,
    backend_expr: String,
    model_tier_expr: String,
    effort_expr: String,
    project_id_expr: String,
    workflow_id_expr: String,
    scheduled_job_id_expr: String,
    issue_tracker_id_expr: String,
    issue_identifier_expr: String,
    total_input_tokens_expr: String,
    total_output_tokens_expr: String,
    total_cache_creation_tokens_expr: String,
    total_cache_read_tokens_expr: String,
    work_time_ms_expr: String,
    cost_usd_expr: String,
    created_at_expr: String,
    updated_at_expr: String,
    session_kind_filter: String,
}

fn v73_session_exprs(tx: &rusqlite::Transaction<'_>) -> Result<V73SessionExprs> {
    let has = |column: &str| column_exists_tx(tx, "sessions", column);
    let provider_expr = if has("provider")? {
        "lower(provider)".to_string()
    } else {
        "NULL".to_string()
    };
    let model_expr = if has("model")? {
        "model".to_string()
    } else {
        "NULL".to_string()
    };
    let effort_expr = if has("effort")? {
        "effort".to_string()
    } else {
        "NULL".to_string()
    };
    let backend_expr = if has("provider")? {
        "lower(provider)".to_string()
    } else {
        "NULL".to_string()
    };
    let model_tier_expr = if has("provider")? && has("model")? {
        "CASE
            WHEN lower(provider) = 'local' THEN 'local'
            WHEN model LIKE 'gpt-5%' OR model LIKE 'claude-%' OR model LIKE 'gemini-%' THEN 'premium'
            ELSE 'standard'
         END"
        .to_string()
    } else if has("provider")? {
        "CASE WHEN lower(provider) = 'local' THEN 'local' ELSE 'standard' END".to_string()
    } else {
        "'standard'".to_string()
    };
    Ok(V73SessionExprs {
        status_expr: if has("status")? {
            "status".to_string()
        } else {
            "'Completed'".to_string()
        },
        provider_expr,
        model_expr,
        backend_expr,
        model_tier_expr,
        effort_expr,
        project_id_expr: if has("project_id")? {
            "project_id".to_string()
        } else {
            "NULL".to_string()
        },
        workflow_id_expr: if has("workflow_id")? {
            "workflow_id".to_string()
        } else {
            "NULL".to_string()
        },
        scheduled_job_id_expr: if has("scheduled_job_id")? {
            "scheduled_job_id".to_string()
        } else {
            "NULL".to_string()
        },
        issue_tracker_id_expr: if has("issue_tracker_id")? {
            "issue_tracker_id".to_string()
        } else {
            "NULL".to_string()
        },
        issue_identifier_expr: if has("issue_identifier")? {
            "issue_identifier".to_string()
        } else {
            "NULL".to_string()
        },
        total_input_tokens_expr: if has("total_input_tokens")? {
            "total_input_tokens".to_string()
        } else {
            "NULL".to_string()
        },
        total_output_tokens_expr: if has("total_output_tokens")? {
            "total_output_tokens".to_string()
        } else {
            "NULL".to_string()
        },
        total_cache_creation_tokens_expr: if has("total_cache_creation_tokens")? {
            "total_cache_creation_tokens".to_string()
        } else {
            "NULL".to_string()
        },
        total_cache_read_tokens_expr: if has("total_cache_read_tokens")? {
            "total_cache_read_tokens".to_string()
        } else {
            "NULL".to_string()
        },
        work_time_ms_expr: if has("work_time_ms")? {
            "work_time_ms".to_string()
        } else if has("duration_ms")? {
            "duration_ms".to_string()
        } else {
            "NULL".to_string()
        },
        cost_usd_expr: if has("cost_usd")? {
            "cost_usd".to_string()
        } else {
            "NULL".to_string()
        },
        created_at_expr: if has("created_at")? {
            "created_at".to_string()
        } else {
            "CURRENT_TIMESTAMP".to_string()
        },
        updated_at_expr: if has("updated_at")? {
            "updated_at".to_string()
        } else if has("created_at")? {
            "created_at".to_string()
        } else {
            "CURRENT_TIMESTAMP".to_string()
        },
        session_kind_filter: if has("session_kind")? {
            "session_kind NOT IN ('Group', 'Epic')".to_string()
        } else {
            "1 = 1".to_string()
        },
    })
}
