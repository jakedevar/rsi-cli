impl Store {
    fn migrate_v077(&self, version: i32) -> Result<()> {
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

        Ok(())
    }
}
