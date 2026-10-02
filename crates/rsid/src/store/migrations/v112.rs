impl Store {
    fn migrate_v112(&self, version: i32) -> Result<()> {
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

        Ok(())
    }

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
}
