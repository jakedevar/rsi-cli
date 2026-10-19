//! V175: the Session dependency-health relevance classifier, versioned (#2050).
//!
//! The released V120 helper (`rsi_swc_v120_session_dependency_relevance`) is
//! immutable. It reported every purged root that retains its `root_missing`
//! observation and every ownerless quarantine as `Invalid` (relevance 2), so the
//! health index kept reselecting thousands of settled rows. V175 replaces only
//! the evidence view, which now calls `rsi_swc_v175_session_dependency_relevance`;
//! the health table, its guard and maintenance triggers and every index stay
//! byte-identical and keep reading the view by name. The terminal-shape
//! predicates are shared with the settlement decoder through
//! `terminal_custody_shape`, so a shape the startup publishers write cannot be
//! healthy to one consumer and a contradiction to the other.

use super::*;
use crate::store::terminal_custody_shape::{
    TerminalCustodyFacts, TerminalCustodyShape, classify_terminal_custody_shape,
};

/// The V120 evidence view keeps its name: the released triggers read it by name.
pub(crate) const EVIDENCE_VIEW_NAME: &str = "source_worktree_session_dependency_evidence_v120";

// RSI-RELEASED-MIGRATION-BEGIN: v175-session-dependency-health-catalog
/// Exact stored text of the V175 evidence view.
pub(crate) const EVIDENCE_VIEW_SQL: &str = r#"CREATE VIEW source_worktree_session_dependency_evidence_v120 AS
        SELECT s.id AS session_id,p.custody_id AS projection_custody_id,
               rsi_swc_v175_session_dependency_relevance(
                   s.id,s.status,s.session_kind,s.working_dir,s.sandbox_root,
                   s.sandbox_custody_id,p.session_id IS NOT NULL,p.schema_version,
                   p.projection_version,p.execution_state,p.freshness,
                   p.canonical_repo_dir,p.effective_cwd,p.custody_id,
                   p.custody_generation,p.validated_at,p.error_code,p.updated_at,
                   s.sandbox_kind,s.sandbox_branch,s.sandbox_cleanup_state,
                   r.custody_id,r.state,r.owner_session_id,r.generation,
                   r.canonical_repo_dir,r.sandbox_root,r.sandbox_branch,
                   r.validation_state,r.validated_generation,r.validation_error_code
               ) AS relevance
          FROM sessions s
          LEFT JOIN session_execution_projections p ON p.session_id=s.id
          LEFT JOIN sandbox_custody_roots r ON r.custody_id=p.custody_id"#;

/// Fingerprint (the `v120_catalog_fingerprint` algorithm) of the 48 V120
/// catalog objects V175 leaves untouched, i.e. every one but the evidence view.
const RETAINED_V120_CATALOG_FINGERPRINT: &str =
    "sha256:ecb745f4e633983d8f51f03740056b0cc6f10f8c17030f1e12c2d9fe5f389274";

/// Health rows follow the evidence view: move every stale row to the view's
/// value. Triggers already keep a row per Session, so none is added; the V120
/// guards accept exactly the view's value.
const REBUILD_HEALTH_SQL: &str = r#"
UPDATE source_worktree_session_dependency_health
   SET relevance=(SELECT e.relevance FROM source_worktree_session_dependency_evidence_v120 e
                   WHERE e.session_id=source_worktree_session_dependency_health.session_id)
 WHERE session_id IN (SELECT e.session_id
                        FROM source_worktree_session_dependency_evidence_v120 e
                        JOIN source_worktree_session_dependency_health h
                          ON h.session_id=e.session_id
                       WHERE e.relevance IS NOT h.relevance);
"#;
// RSI-RELEASED-MIGRATION-END: v175-session-dependency-health-catalog

// RSI-RELEASED-MIGRATION-BEGIN: v175-session-dependency-health-migration
/// Swap the evidence view onto the V175 classifier and re-derive the health
/// index. Runs inside the caller's transaction; the caller bumps the version.
pub(crate) fn install(tx: &Transaction<'_>) -> Result<()> {
    let version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version != 174 {
        return Err(DaemonError::Store(format!(
            "V175 requires exact V174 source, found V{version}"
        )));
    }
    // The released V120 catalog is the exact predecessor of this swap.
    validate_v120_catalog(tx)?;
    tx.execute_batch(&format!("DROP VIEW {EVIDENCE_VIEW_NAME}"))?;
    tx.execute_batch(EVIDENCE_VIEW_SQL)?;
    tx.execute_batch(REBUILD_HEALTH_SQL)?;
    validate_v175_catalog(tx)?;
    Ok(())
}
// RSI-RELEASED-MIGRATION-END: v175-session-dependency-health-migration

// RSI-RELEASED-MIGRATION-BEGIN: v175-session-dependency-health-validator
fn retained_v120_catalog_fingerprint(connection: &Connection) -> Result<String> {
    let mut objects: Vec<(&str, &str)> = V120_CATALOG_OBJECTS
        .iter()
        .copied()
        .filter(|(_, name)| *name != EVIDENCE_VIEW_NAME)
        .collect();
    objects.sort_unstable();
    let mut hash = Sha256::new();
    for (kind, name) in objects {
        let row: (String, String, String, String) = connection
            .query_row(
                "SELECT type,name,tbl_name,coalesce(sql,'') FROM sqlite_master WHERE type=?1 AND name=?2",
                params![kind, name],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .map_err(|_| DaemonError::Store(format!("V175 catalog missing {kind} {name}")))?;
        for (index, field) in [row.0, row.1, row.2, row.3].into_iter().enumerate() {
            hash.update(field.as_bytes());
            if index != 3 {
                hash.update(b"\0");
            }
        }
        hash.update(b"\n");
    }
    Ok(format!("sha256:{:x}", hash.finalize()))
}

/// Authenticate the V120 catalog as V175 left it: every V120 object present,
/// the evidence view exactly the V175 successor, and the other 48 objects
/// byte-identical to the released V120 text.
pub(crate) fn validate_v175_catalog(connection: &Connection) -> Result<()> {
    for (kind, name) in V120_CATALOG_OBJECTS {
        let found: i64 = connection.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type=?1 AND name=?2",
            params![kind, name],
            |row| row.get(0),
        )?;
        if found != 1 {
            return Err(DaemonError::Store(format!(
                "V175 catalog missing {kind} {name}"
            )));
        }
    }
    let view_sql: String = connection.query_row(
        "SELECT sql FROM sqlite_master WHERE type='view' AND name=?1",
        [EVIDENCE_VIEW_NAME],
        |row| row.get(0),
    )?;
    if view_sql != EVIDENCE_VIEW_SQL {
        return Err(DaemonError::Store(format!(
            "V175 catalog changed {EVIDENCE_VIEW_NAME}"
        )));
    }
    let fingerprint = retained_v120_catalog_fingerprint(connection)?;
    if fingerprint != RETAINED_V120_CATALOG_FINGERPRINT {
        return Err(DaemonError::Store(format!(
            "V175 retained V120 catalog fingerprint mismatch: expected {RETAINED_V120_CATALOG_FINGERPRINT}, got {fingerprint}"
        )));
    }
    let key_count: i64 = connection.query_row(
        "SELECT count(*) FROM source_worktree_batch_cursor_keys WHERE key_id=1 AND length(cursor_key)=32",
        [],
        |row| row.get(0),
    )?;
    if key_count != 1 {
        return Err(DaemonError::Store(
            "V175 cursor key singleton is malformed".into(),
        ));
    }
    Ok(())
}
// RSI-RELEASED-MIGRATION-END: v175-session-dependency-health-validator

/// The V120 catalog check that fits the store's schema version: the released
/// V120 text up to V174, the V175 successor from V175 on.
pub(crate) fn validate_current_catalog(connection: &Connection) -> Result<()> {
    let version: i32 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version >= 175 {
        validate_v175_catalog(connection)
    } else {
        validate_v120_catalog(connection)
    }
}

const LEAF_KINDS: [&str; 8] = [
    "Standard",
    "TaskRabbit",
    "Bug",
    "Story",
    "Task",
    "Feature",
    "Refactor",
    "Research",
];
const STATUSES: [&str; 8] = [
    "Starting",
    "Running",
    "WaitingApproval",
    "Completed",
    "Failed",
    "Interrupted",
    "Archived",
    "Deleted",
];

/// How a terminal custody shape reads in the health index.
pub(super) const fn relevance_of_shape(shape: TerminalCustodyShape) -> SessionDependencyRelevance {
    match shape {
        TerminalCustodyShape::SettledHistory => SessionDependencyRelevance::ProvenIrrelevant,
        // Cannot execute, but its retained root still protects its own path:
        // relevant, never `Invalid` (#2050).
        TerminalCustodyShape::RetainedRootProtection => SessionDependencyRelevance::Relevant,
        TerminalCustodyShape::Unproven | TerminalCustodyShape::Contradiction(_) => {
            SessionDependencyRelevance::Invalid
        }
    }
}

impl SessionDependency {
    /// The V175 classifier. Everything but the terminal custody shapes is the
    /// V120 rule; those shapes go through the one classifier the settlement
    /// decoder also uses.
    pub(super) fn relevance_v175(&self) -> SessionDependencyRelevance {
        use SessionDependencyRelevance::{Invalid, ProvenIrrelevant, Relevant};

        if matches!(self.session_kind.as_str(), "Group" | "Epic") {
            return ProvenIrrelevant;
        }
        if self.malformed
            || !LEAF_KINDS.contains(&self.session_kind.as_str())
            || !STATUSES.contains(&self.status.as_str())
        {
            return Invalid;
        }
        if matches!(
            self.status.as_str(),
            "Starting" | "Running" | "WaitingApproval"
        ) {
            return Relevant;
        }
        if !self.projection_is_authenticated() {
            return Invalid;
        }
        match self.execution_state.as_deref() {
            Some("ordinary_unsandboxed" | "live_sandboxed") => {
                if self.status == "Deleted" {
                    Invalid
                } else {
                    Relevant
                }
            }
            Some(_) => classify_terminal_custody_shape(&self.terminal_custody_facts())
                .map_or(Invalid, relevance_of_shape),
            None => Invalid,
        }
    }

    pub(super) fn terminal_custody_facts(&self) -> TerminalCustodyFacts<'_> {
        let generation = |value: Option<i64>| value.and_then(|value| u64::try_from(value).ok());
        TerminalCustodyFacts {
            authenticated_projection: self.projection_is_authenticated(),
            session_id: Some(self.id.as_str()),
            working_dir: Some(self.working_dir.as_str()),
            session_sandbox_root: self.sandbox_root.as_deref(),
            session_sandbox_branch: self.sandbox_branch.as_deref(),
            session_custody_id: self.sandbox_custody_id.as_deref(),
            sandbox_kind: self.sandbox_kind.as_deref(),
            cleanup_state: self.sandbox_cleanup_state.as_deref(),
            execution_state: self.execution_state.as_deref(),
            freshness: self.freshness.as_deref(),
            effective_cwd: self.effective_cwd.as_deref(),
            projection_canonical_repo_dir: self.canonical_repo_dir.as_deref(),
            projection_custody_id: self.projection_custody_id.as_deref(),
            projection_generation: generation(self.projection_generation),
            projection_error_code: self.projection_error_code.as_deref(),
            root_custody_id: self.root_custody_id.as_deref(),
            root_state: self.root_state.as_deref(),
            root_owner_session_id: self.root_owner_session_id.as_deref(),
            root_generation: generation(self.root_generation),
            root_canonical_repo_dir: self.root_canonical_repo_dir.as_deref(),
            root_sandbox_root: self.root_sandbox_root.as_deref(),
            root_sandbox_branch: self.root_sandbox_branch.as_deref(),
            root_validation_state: self.root_validation_state.as_deref(),
            root_validated_generation: generation(self.root_validated_generation),
            root_validation_error_code: self.root_validation_error_code.as_deref(),
        }
    }
}

/// The scalar function behind the V175 evidence view.
pub(crate) fn session_dependency_relevance_sql(
    context: &rusqlite::functions::Context<'_>,
) -> rusqlite::Result<i64> {
    let dependency = session_dependency_from_value_refs(|index| Ok(context.get_raw(index)))?;
    Ok(dependency.relevance_v175().database_code())
}

/// Undo V175 for rewound test fixtures: restore the released V120 evidence view
/// and the V120 health values, then claim V174.
#[cfg(test)]
pub(crate) fn rewind_to_v174(connection: &Connection) {
    connection
        .execute_batch(&format!("DROP VIEW {EVIDENCE_VIEW_NAME}"))
        .expect("drop the V175 evidence view");
    connection
        .execute_batch(V120_EVIDENCE_VIEW_SQL)
        .expect("restore the released V120 evidence view");
    connection
        .execute_batch(REBUILD_HEALTH_SQL)
        .expect("re-derive V120 health");
    connection
        .execute_batch("PRAGMA user_version=174;")
        .expect("claim V174");
}

/// Exact stored text of the released V120 evidence view.
#[cfg(test)]
pub(crate) const V120_EVIDENCE_VIEW_SQL: &str = r#"CREATE VIEW source_worktree_session_dependency_evidence_v120 AS
        SELECT s.id AS session_id,p.custody_id AS projection_custody_id,
               rsi_swc_v120_session_dependency_relevance(
                   s.id,s.status,s.session_kind,s.working_dir,s.sandbox_root,
                   s.sandbox_custody_id,p.session_id IS NOT NULL,p.schema_version,
                   p.projection_version,p.execution_state,p.freshness,
                   p.canonical_repo_dir,p.effective_cwd,p.custody_id,
                   p.custody_generation,p.validated_at,p.error_code,p.updated_at,
                   s.sandbox_kind,s.sandbox_branch,s.sandbox_cleanup_state,
                   r.custody_id,r.state,r.owner_session_id,r.generation,
                   r.canonical_repo_dir,r.sandbox_root,r.sandbox_branch,
                   r.validation_state,r.validated_generation,r.validation_error_code
               ) AS relevance
          FROM sessions s
          LEFT JOIN session_execution_projections p ON p.session_id=s.id
          LEFT JOIN sandbox_custody_roots r ON r.custody_id=p.custody_id"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::sandbox_custody::{CustodyCause, NewCustodyRoot, SessionCustodyBinding};
    use crate::store::tests::rewind_store_to_schema_version;
    use crate::test_support::make_test_session;
    use rsi_common::types::{
        SandboxCleanupState, SandboxCustodyErrorCodeV1, SandboxKind, SessionStatus,
    };
    use std::fs;

    const SETTLED: i64 = 0;
    const PROTECTED: i64 = 1;
    const CONTRADICTION: i64 = 2;

    struct Fixture {
        _directory: tempfile::TempDir,
        repository: PathBuf,
        sandboxes: PathBuf,
        store: Store,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().expect("create V175 fixture directory");
            let repository = directory.path().join("repository");
            let sandboxes = directory.path().join("sandboxes");
            fs::create_dir_all(&repository).expect("create V175 repository directory");
            fs::create_dir_all(&sandboxes).expect("create V175 sandbox base");
            let store = Store::open_test_fixture(&directory.path().join("v175.sqlite"))
                .expect("open V175 store");
            Self {
                _directory: directory,
                repository,
                sandboxes,
                store,
            }
        }

        /// One completed Session that owns a fresh live custody root.
        fn custody_session(&mut self, label: &str) -> (Uuid, Uuid) {
            let session_id = Uuid::new_v4();
            let custody_id = Uuid::new_v4();
            let root = self.sandboxes.join(session_id.to_string());
            let branch = format!("rsi/{label}/{session_id}");
            let repository = self.repository.to_string_lossy().into_owned();
            let mut session = make_test_session();
            session.id = session_id;
            session.status = SessionStatus::Completed;
            session.working_dir = self.repository.clone();
            session.sandbox_kind = Some(SandboxKind::GitWorktree);
            session.sandbox_root = Some(root.clone());
            session.sandbox_branch = Some(branch.clone());
            session.sandbox_cleanup_state = Some(SandboxCleanupState::Live);
            self.store
                .insert_session_with_custody(
                    &session,
                    SessionCustodyBinding::New(NewCustodyRoot {
                        custody_id,
                        canonical_repo_dir: repository,
                        sandbox_root: root.to_string_lossy().into_owned(),
                        sandbox_branch: branch,
                        repository_identity: format!("repo:{label}"),
                        source_commit: "a".repeat(40),
                        cause: CustodyCause::FreshLaunch,
                    }),
                )
                .expect("insert V175 custody session");
            (session_id, custody_id)
        }

        fn publish(&mut self, custody_id: Uuid) {
            self.store
                .publish_startup_terminal_root(custody_id)
                .expect("publish terminal projection");
        }

        /// Startup found the root already removed: purged, keeping the
        /// root_missing observation (the hub's dominant history shape).
        fn purged_root_missing(&mut self, label: &str) -> (Uuid, Uuid) {
            let (session_id, custody_id) = self.custody_session(label);
            self.store
                .tombstone_custody_root(custody_id, 1, CustodyCause::Purge)
                .expect("purge V175 root");
            self.store
                .conn
                .execute(
                    "UPDATE sandbox_custody_roots
                        SET validation_state='invalid',validation_error_code='root_missing'
                      WHERE custody_id=?1",
                    [custody_id.to_string()],
                )
                .expect("retain the missing-root observation");
            self.publish(custody_id);
            (session_id, custody_id)
        }

        fn quarantined(&mut self, label: &str, paths_cleared: bool) -> (Uuid, Uuid) {
            let (session_id, custody_id) = self.custody_session(label);
            self.store
                .record_custody_cleanup_failure(
                    custody_id,
                    1,
                    SandboxCustodyErrorCodeV1::CleanupFailed,
                )
                .expect("fail V175 cleanup");
            self.store
                .quarantine_startup_terminal_root(
                    custody_id,
                    SandboxCustodyErrorCodeV1::OwnershipConflict,
                )
                .expect("quarantine V175 root");
            if paths_cleared {
                self.store
                    .conn
                    .execute(
                        "UPDATE sessions SET sandbox_root=NULL,sandbox_branch=NULL WHERE id=?1",
                        [session_id.to_string()],
                    )
                    .expect("clear the Session paths");
            }
            self.publish(custody_id);
            (session_id, custody_id)
        }

        /// A rootless Session written the way `publish_startup_rootless_*` and
        /// `invalidate_startup_session` leave it.
        fn rootless(&self, label: &str, state: &str) -> Uuid {
            let session_id = Uuid::new_v4();
            insert_session(&self.store.conn, session_id, &self.repository, "Archived");
            let (kind, cleanup, freshness, error) = match state {
                "historical_purged" => (Some("GitWorktree"), Some("Purged"), "verified", None),
                "historical_cleanup_failed" => {
                    (Some("GitWorktree"), Some("Failed"), "verified", None)
                }
                _ => (None, None, "invalid", Some("worktree_mismatch")),
            };
            self.store
                .conn
                .execute(
                    "UPDATE sessions SET sandbox_kind=?2,sandbox_cleanup_state=?3 WHERE id=?1",
                    params![session_id.to_string(), kind, cleanup],
                )
                .unwrap_or_else(|error| panic!("{label}: set rootless Session tuple: {error}"));
            self.store
                .conn
                .execute(
                    "UPDATE session_execution_projections
                        SET execution_state=?2,freshness=?3,canonical_repo_dir=?4,effective_cwd=NULL,
                            custody_id=NULL,custody_generation=NULL,validated_at=?5,error_code=?6,
                            updated_at=?5
                      WHERE session_id=?1",
                    params![
                        session_id.to_string(),
                        state,
                        freshness,
                        self.repository.to_string_lossy().into_owned(),
                        TEST_NOW,
                        error,
                    ],
                )
                .unwrap_or_else(|error| panic!("{label}: publish rootless projection: {error}"));
            session_id
        }

        fn stored(&self, session_id: Uuid) -> i64 {
            self.store
                .conn
                .query_row(
                    "SELECT relevance FROM source_worktree_session_dependency_health
                      WHERE session_id=?1",
                    [session_id.to_string()],
                    |row| row.get(0),
                )
                .expect("read stored Session relevance")
        }

        /// The health classifier reading the Session as the V175 view does.
        fn recomputed(&self, session_id: Uuid) -> i64 {
            load_session_dependency(&self.store.conn, &session_id.to_string())
                .expect("load Session dependency")
                .expect("Session dependency exists")
                .relevance_v175()
                .database_code()
        }

        /// The settlement decoder reading the same Session through its own
        /// column list.
        fn settlement(&self, session_id: Uuid) -> i64 {
            let fields: Vec<Option<String>> = self
                .store
                .conn
                .query_row(
                    &format!(
                        "{} WHERE s.id=?1",
                        crate::store::cohort_settlement::SETTLEMENT_SESSION_FIELDS_SELECT
                    ),
                    [session_id.to_string()],
                    |row| {
                        assert!(!row.get::<_, bool>(30)?, "decoder flags the row malformed");
                        (0..30).map(|column| row.get(column)).collect()
                    },
                )
                .expect("read settlement decoder columns");
            let facts = TerminalCustodyFacts::from_settlement_fields(&fields);
            let shape = classify_terminal_custody_shape(&facts)
                .expect("a terminal projection state classifies");
            relevance_of_shape(shape).database_code()
        }

        /// Health index, health classifier and settlement decoder agree.
        fn assert_parity(&self, label: &str, session_id: Uuid, expected: i64) {
            assert_eq!(self.stored(session_id), expected, "{label}: stored health");
            assert_eq!(
                self.recomputed(session_id),
                expected,
                "{label}: health classifier"
            );
            assert_eq!(
                self.settlement(session_id),
                expected,
                "{label}: settlement decoder"
            );
        }
    }

    const TEST_NOW: &str = "2099-01-01T00:00:00.000000000Z";

    fn insert_session(connection: &Connection, session_id: Uuid, working_dir: &Path, status: &str) {
        connection
            .execute(
                "INSERT INTO sessions(id,query,working_dir,status,created_at,updated_at)
                 VALUES(?1,'V175 fixture',?2,?3,?4,?4)",
                params![
                    session_id.to_string(),
                    working_dir.to_string_lossy().into_owned(),
                    status,
                    TEST_NOW,
                ],
            )
            .expect("insert V175 fixture Session");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn publisher_shapes_classify_identically_for_health_and_settlement() {
        let mut fixture = Fixture::new();
        let (purged, _) = fixture.purged_root_missing("purged-missing");
        fixture.assert_parity("purged root_missing", purged, SETTLED);
        let (intact, _) = fixture.quarantined("quarantine-intact", false);
        fixture.assert_parity("quarantine with intact paths", intact, SETTLED);
        let (cleared, _) = fixture.quarantined("quarantine-cleared", true);
        fixture.assert_parity(
            "ownerless quarantine with cleared paths",
            cleared,
            PROTECTED,
        );
        for state in ["historical_purged", "historical_cleanup_failed", "invalid"] {
            let session_id = fixture.rootless(state, state);
            fixture.assert_parity(&format!("rootless {state}"), session_id, SETTLED);
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn custody_contradictions_stay_unhealthy_for_health_and_settlement() {
        let mut fixture = Fixture::new();
        let (drifted, _) = fixture.purged_root_missing("purged-drift");
        fixture
            .store
            .conn
            .execute(
                "UPDATE session_execution_projections
                    SET custody_generation=custody_generation+1 WHERE session_id=?1",
                [drifted.to_string()],
            )
            .expect("drift the projection generation");
        fixture.assert_parity("purged generation drift", drifted, CONTRADICTION);

        let (conflict, custody) = fixture.purged_root_missing("purged-conflict");
        fixture
            .store
            .conn
            .execute(
                "UPDATE sandbox_custody_roots SET validation_error_code='ownership_conflict'
                  WHERE custody_id=?1",
                [custody.to_string()],
            )
            .expect("replace the missing-root observation");
        fixture.assert_parity("purged ownership conflict", conflict, CONTRADICTION);

        let (generation, _) = fixture.quarantined("quarantine-drift", true);
        fixture
            .store
            .conn
            .execute(
                "UPDATE session_execution_projections
                    SET custody_generation=custody_generation+1 WHERE session_id=?1",
                [generation.to_string()],
            )
            .expect("drift the quarantine generation");
        fixture.assert_parity("quarantine generation drift", generation, CONTRADICTION);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn v175_replaces_the_reported_terminal_shapes_and_keeps_contradictions() {
        let mut fixture = Fixture::new();
        let (purged, _) = fixture.purged_root_missing("upgrade-purged");
        let (cleared, _) = fixture.quarantined("upgrade-cleared", true);
        let (drifted, _) = fixture.purged_root_missing("upgrade-drift");
        fixture
            .store
            .conn
            .execute(
                "UPDATE session_execution_projections
                    SET custody_generation=custody_generation+1 WHERE session_id=?1",
                [drifted.to_string()],
            )
            .expect("drift the projection generation");
        let running = Uuid::new_v4();
        insert_session(&fixture.store.conn, running, &fixture.repository, "Running");

        // The hub before V175: released V120 reports both publisher shapes Invalid.
        rewind_store_to_schema_version(&fixture.store.conn, 174);
        validate_current_catalog(&fixture.store.conn).expect("rewound V120 catalog");
        assert_eq!(fixture.stored(purged), CONTRADICTION);
        assert_eq!(fixture.stored(cleared), CONTRADICTION);
        assert_eq!(fixture.stored(drifted), CONTRADICTION);
        assert_eq!(fixture.stored(running), 1);

        fixture.store.init_schema().expect("upgrade to V175");
        let version: i32 = fixture
            .store
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("read upgraded version");
        // The upgrade runs every migration above V174, so a later head is the version
        // it ends at (V176 adds the cold-archive index, #2384).
        assert_eq!(version, crate::store::LATEST_SCHEMA_VERSION);
        assert!(version >= 175);
        validate_v175_catalog(&fixture.store.conn).expect("V175 successor catalog");
        validate_current_catalog(&fixture.store.conn).expect("current catalog is V175");
        assert_eq!(fixture.stored(purged), SETTLED);
        assert_eq!(fixture.stored(cleared), PROTECTED);
        assert_eq!(fixture.stored(drifted), CONTRADICTION);
        assert_eq!(fixture.stored(running), 1);
        let reselected: i64 = fixture
            .store
            .conn
            .query_row(
                "SELECT count(*) FROM source_worktree_session_dependency_health
                  WHERE relevance=2",
                [],
                |row| row.get(0),
            )
            .expect("count reselected history");
        assert_eq!(
            reselected, 1,
            "only the custody contradiction stays in the Invalid set"
        );

        // A second open keeps the upgraded classification and catalog.
        fixture.store.init_schema().expect("reopen V175");
        assert_eq!(fixture.stored(purged), SETTLED);
        assert_eq!(fixture.stored(cleared), PROTECTED);
        assert_eq!(fixture.stored(drifted), CONTRADICTION);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn v175_catalog_validation_rejects_a_tampered_successor() {
        let fixture = Fixture::new();
        validate_v175_catalog(&fixture.store.conn).expect("head catalog");
        assert!(
            validate_v120_catalog(&fixture.store.conn).is_err(),
            "the released V120 text no longer describes a V175 store"
        );

        let tampered = |sql: &str| {
            let store = Store::open_in_memory().expect("open tampered V175 store");
            store.conn.execute_batch(sql).expect("tamper with catalog");
            validate_v175_catalog(&store.conn).is_err()
        };
        assert!(tampered(
            "DROP INDEX idx_swc_v120_sessions_dependency_health"
        ));
        assert!(tampered(
            "DROP TRIGGER source_worktree_session_health_v120_delete_guard"
        ));
        assert!(tampered(&format!(
            "DROP VIEW {EVIDENCE_VIEW_NAME};{}",
            V120_EVIDENCE_VIEW_SQL
        )));
    }
}
