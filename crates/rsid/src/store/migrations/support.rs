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
