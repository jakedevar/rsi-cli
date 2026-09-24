//! Converge a deployed-additive V114 catalog onto the canonical V114 catalog.
//!
//! Every released migration from V115 onward authenticates its source with an
//! exact full-`sqlite_master` fingerprint. That fingerprint hashes each object's
//! stored SQL *text*, so two databases with semantically identical schemas
//! disagree whenever a column reached a table by a late `ALTER TABLE ADD COLUMN`
//! in one lineage and inline in the `CREATE TABLE` of another.
//!
//! Exactly that happened. A database deployed before those columns were folded
//! into the base schema carries:
//!
//! - `sessions.provider` as the last column instead of the third, and
//! - `workflows.definition_json` as the last column instead of the fifth.
//!
//! Identical column sets, identical types and defaults, different text — and so
//! a different fingerprint, which V115 refuses. V112's convergence did not cover
//! this lineage: it authenticates three V111 catalogs and reconciles what V112
//! itself installs, but the two column positions predate it and survived.
//!
//! This runs at V114, before V115 reads the catalog, and rebuilds only those two
//! tables so the database rejoins the canonical lineage permanently. It is
//! deliberately all-or-nothing: the transaction commits only if the resulting
//! full catalog equals the exact fingerprint V115 demands, so a partial or
//! mistaken rebuild can never be persisted.
//!
//! It is data-preserving. Rows are copied by explicit column name, never by
//! position, and the row count is asserted on both sides.

use rusqlite::{Transaction, TransactionBehavior};

use crate::error::{DaemonError, Result};
use crate::store::{Store, capacity_recovery, manager_prepared_actions};

/// The canonical `sessions` table text a fresh migration chain produces.
const CANONICAL_SESSIONS_SQL: &str = r"CREATE TABLE sessions (
                id              TEXT PRIMARY KEY,
                claude_session_id TEXT,
                provider        TEXT NOT NULL DEFAULT 'Claude',
                query           TEXT NOT NULL,
                working_dir     TEXT NOT NULL,
                status          TEXT NOT NULL,
                created_at      TEXT NOT NULL,
                updated_at      TEXT NOT NULL
            , cost_usd REAL, duration_ms INTEGER, num_turns INTEGER, model TEXT, input_tokens INTEGER, output_tokens INTEGER, context_window INTEGER, total_input_tokens INTEGER, total_output_tokens INTEGER, total_cache_creation_tokens INTEGER, total_cache_read_tokens INTEGER, stop_reason TEXT, project_id TEXT, pinned_at TEXT, session_kind TEXT NOT NULL DEFAULT 'Standard', continued_from TEXT, handoff_filepath TEXT, rotation_depth INTEGER NOT NULL DEFAULT 0, daemon_input_tokens INTEGER, daemon_output_tokens INTEGER, title TEXT, pipeline_artifact TEXT, workflow_id TEXT, description TEXT, git_branch TEXT, active_task TEXT, group_id TEXT, pending_archive INTEGER NOT NULL DEFAULT 0, testing_needed_at TEXT, rotation_disabled_at TEXT, effort TEXT, retry_attempt INTEGER, max_retries INTEGER, issue_identifier TEXT, issue_url TEXT, issue_tracker_id TEXT, scheduled_job_id TEXT, rating INTEGER, harness_version_hash TEXT, test_passed INTEGER, clippy_passed INTEGER, turn_count INTEGER, retry_count INTEGER, sandbox_kind TEXT, sandbox_root TEXT, sandbox_branch TEXT, sandbox_cleanup_state TEXT, parent_id TEXT, approval_wait_ms INTEGER, lead_session_id TEXT, tag TEXT NOT NULL DEFAULT '', is_eval INTEGER NOT NULL DEFAULT 0, capability_class TEXT, topology_node_id TEXT, topology_iteration INTEGER NOT NULL DEFAULT 0, pending_question_json TEXT, work_time_ms INTEGER, model_invocation_id TEXT, sandbox_custody_id TEXT REFERENCES sandbox_custody_roots(custody_id) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED, execution_origin_claim_id TEXT, execution_origin_write_seq INTEGER NOT NULL DEFAULT 0 CHECK(execution_origin_write_seq >= 0), provider_cli_version TEXT, provider_capabilities TEXT, thinking_tokens INTEGER, service_tier TEXT, cache_creation_1h_tokens INTEGER, cache_creation_5m_tokens INTEGER, permission_denial_count INTEGER, subagent_stats_json TEXT, queued_turn_count INTEGER, terminal_reason TEXT, context_window_source TEXT
                 CHECK (context_window_source IS NULL OR context_window_source IN (
                     'official_documentation', 'provider_catalog', 'configured',
                     'runtime_telemetry', 'repository_fallback', 'legacy_unverified'
                 )), context_window_source_version TEXT, context_window_source_digest TEXT, context_window_observed_at TEXT, context_window_configured_tokens INTEGER
                 CHECK (context_window_configured_tokens IS NULL OR
                        context_window_configured_tokens > 0), agent_role TEXT, epic_spawn_ordinal INTEGER)";

/// The canonical `workflows` table text a fresh migration chain produces.
const CANONICAL_WORKFLOWS_SQL: &str = r"CREATE TABLE workflows (
                    id          TEXT PRIMARY KEY,
                    title       TEXT NOT NULL,
                    stage       TEXT NOT NULL DEFAULT 'Research',
                    artifact_path TEXT,
                    definition_json TEXT,
                    project_id  TEXT REFERENCES projects(id),
                    created_at  TEXT NOT NULL,
                    updated_at  TEXT NOT NULL
                )";

/// Tables this convergence knows how to rebuild, in dependency-free order.
const CONVERGED_TABLES: [(&str, &str); 2] = [
    ("sessions", CANONICAL_SESSIONS_SQL),
    ("workflows", CANONICAL_WORKFLOWS_SQL),
];

impl Store {
    /// Bring a deployed-additive V114 catalog to the exact canonical V114
    /// catalog, so the pinned V115 and V116 drivers authenticate it.
    ///
    /// A no-op on a database that is already canonical, and a no-op on one this
    /// code does not recognise — in that case V115 raises its own catalog
    /// refusal against an untouched database rather than this leaving a
    /// half-converged one behind.
    pub(crate) fn converge_v114_deployed_additive_catalog(&self) -> Result<()> {
        let current = {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
            let value = capacity_recovery::v88_full_catalog_fingerprint(&tx)?;
            tx.commit()?;
            value
        };
        if current == manager_prepared_actions::V114_SOURCE_FINGERPRINT {
            return Ok(());
        }

        let divergent = {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
            let mut names = Vec::new();
            for (table, canonical) in CONVERGED_TABLES {
                if stored_table_sql(&tx, table)?.as_deref() != Some(canonical) {
                    names.push(table);
                }
            }
            tx.commit()?;
            names
        };
        if divergent.is_empty() {
            // The catalog differs somewhere this code does not model. Changing
            // nothing keeps V115's refusal accurate and the database pristine.
            tracing::warn!(
                fingerprint = %current,
                "V114 catalog is not canonical, and not in the shape this convergence repairs"
            );
            return Ok(());
        }

        // `PRAGMA foreign_keys` is ignored inside a transaction, so it has to be
        // cleared first; the rebuild drops and recreates a table that 55 others
        // reference by name. `legacy_alter_table` is what stops ALTER TABLE
        // RENAME from rewriting those 55 references to point at the scratch name.
        self.conn
            .execute_batch("PRAGMA foreign_keys=OFF; PRAGMA legacy_alter_table=ON;")?;
        let outcome = self.converge_v114_tables(&divergent);
        let restored = self
            .conn
            .execute_batch("PRAGMA legacy_alter_table=OFF; PRAGMA foreign_keys=ON;");
        outcome?;
        restored?;
        Ok(())
    }

    fn converge_v114_tables(&self, divergent: &[&str]) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        for (table, canonical) in CONVERGED_TABLES {
            if divergent.contains(&table) {
                rebuild_table(&tx, table, canonical)?;
            }
        }

        // All-or-nothing. Anything short of the exact catalog V115 demands is a
        // failure, and failing here rolls the whole rebuild back.
        let reached = capacity_recovery::v88_full_catalog_fingerprint(&tx)?;
        if reached != manager_prepared_actions::V114_SOURCE_FINGERPRINT {
            return Err(DaemonError::Store(format!(
                "V114 catalog convergence did not reach the canonical catalog: {reached}"
            )));
        }
        let violations: i64 =
            tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })?;
        if violations != 0 {
            return Err(DaemonError::Store(format!(
                "V114 catalog convergence found {violations} foreign-key violation(s)"
            )));
        }
        let integrity: String = tx.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        if integrity != "ok" {
            return Err(DaemonError::Store(format!(
                "V114 catalog convergence found integrity_check={integrity}"
            )));
        }
        tx.commit()?;
        tracing::info!(
            tables = ?divergent,
            "V114 catalog convergence rebuilt deployed-additive tables onto the canonical catalog"
        );
        Ok(())
    }
}

fn stored_table_sql(tx: &Transaction<'_>, table: &str) -> Result<Option<String>> {
    let mut statement =
        tx.prepare("SELECT sql FROM sqlite_master WHERE type='table' AND name=?1")?;
    let mut rows = statement.query([table])?;
    Ok(match rows.next()? {
        Some(row) => Some(row.get::<_, String>(0)?),
        None => None,
    })
}

/// Rebuild one table so its stored text is exactly `canonical`, preserving every
/// row and every attached index and trigger.
///
/// The old table is renamed out of the way rather than the new one renamed into
/// place, because `ALTER TABLE RENAME` rewrites the stored text into its own
/// quoted form — which would defeat the whole point.
fn rebuild_table(tx: &Transaction<'_>, table: &str, canonical: &str) -> Result<()> {
    let attached = {
        let mut statement = tx.prepare(
            "SELECT type,sql FROM sqlite_master
              WHERE tbl_name=?1 AND type IN ('index','trigger') AND sql IS NOT NULL
              ORDER BY type,name",
        )?;
        statement
            .query_map([table], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
    };
    let columns = {
        let mut statement = tx.prepare("SELECT name FROM pragma_table_info(?1) ORDER BY cid")?;
        statement
            .query_map([table], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?
    };
    if columns.is_empty() {
        return Err(DaemonError::Store(format!(
            "V114 catalog convergence found no columns on {table}"
        )));
    }
    let before: i64 = tx.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
        row.get(0)
    })?;

    let scratch = format!("{table}_v114_legacy_catalog");
    for (kind, sql) in &attached {
        let name = object_name(sql).ok_or_else(|| {
            DaemonError::Store(format!(
                "V114 catalog convergence could not name an attached {kind} on {table}"
            ))
        })?;
        tx.execute_batch(&format!("DROP {kind} {name};"))?;
    }
    tx.execute_batch(&format!("ALTER TABLE {table} RENAME TO {scratch};"))?;
    tx.execute_batch(canonical)?;
    let column_list = columns.join(",");
    tx.execute_batch(&format!(
        "INSERT INTO {table} ({column_list}) SELECT {column_list} FROM {scratch};"
    ))?;
    tx.execute_batch(&format!("DROP TABLE {scratch};"))?;
    for (_, sql) in &attached {
        tx.execute_batch(&format!("{sql};"))?;
    }

    let after: i64 = tx.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
        row.get(0)
    })?;
    if after != before {
        return Err(DaemonError::Store(format!(
            "V114 catalog convergence changed {table} from {before} to {after} row(s)"
        )));
    }
    Ok(())
}

/// The object name in a `CREATE INDEX`/`CREATE TRIGGER` statement, quoted forms
/// included. Names in this schema are bare identifiers.
fn object_name(sql: &str) -> Option<String> {
    let lowered = sql.to_ascii_lowercase();
    let start = ["create index ", "create unique index ", "create trigger "]
        .iter()
        .find_map(|prefix| lowered.find(prefix).map(|at| at + prefix.len()))?;
    let rest = sql[start..].trim_start();
    let end = rest.find(|c: char| c.is_whitespace() || c == '(')?;
    Some(rest[..end].to_string())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
pub(crate) mod tests {
    use super::*;
    use rusqlite::Connection;

    /// Rebuild a table into the "deployed additive" shape: the named column is
    /// moved to the end, exactly as a historical `ALTER TABLE ADD COLUMN` left
    /// it. This is the inverse of what the convergence does, and it is how the
    /// tests manufacture a divergent catalog from a canonical one.
    pub(crate) fn decanonicalize(connection: &Connection, table: &str, column: &str) {
        let canonical: String = connection
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name=?1",
                [table],
                |row| row.get(0),
            )
            .expect("read canonical table sql");
        let attached: Vec<String> = {
            let mut statement = connection
                .prepare(
                    "SELECT sql FROM sqlite_master
                      WHERE tbl_name=?1 AND type IN ('index','trigger') AND sql IS NOT NULL
                      ORDER BY type,name",
                )
                .unwrap();
            statement
                .query_map([table], |row| row.get::<_, String>(0))
                .unwrap()
                .map(|row| row.unwrap())
                .collect()
        };
        let columns: Vec<String> = {
            let mut statement = connection
                .prepare("SELECT name FROM pragma_table_info(?1) ORDER BY cid")
                .unwrap();
            statement
                .query_map([table], |row| row.get::<_, String>(0))
                .unwrap()
                .map(|row| row.unwrap())
                .collect()
        };
        // Lift the column's declaration out of the body and re-append it,
        // whatever the surrounding whitespace looks like.
        let mut kept = Vec::new();
        let mut moved = None;
        for line in canonical.lines() {
            let trimmed = line.trim();
            let is_declaration = trimmed
                .strip_prefix(column)
                .is_some_and(|rest| rest.starts_with(char::is_whitespace));
            if is_declaration && moved.is_none() {
                moved = Some(trimmed.trim_end_matches(',').to_string());
            } else {
                kept.push(line);
            }
        }
        let moved = moved.unwrap_or_else(|| panic!("no declaration for {column}"));
        let body = kept.join("\n");
        let closing = body.rfind(')').expect("table body closes");
        let additive = format!("{}, {moved}{}", &body[..closing], &body[closing..]);

        connection
            .execute_batch("PRAGMA foreign_keys=OFF; PRAGMA legacy_alter_table=ON;")
            .unwrap();
        for sql in &attached {
            let name = object_name(sql).expect("name the attached object");
            let kind = if sql.to_ascii_lowercase().contains("trigger") {
                "TRIGGER"
            } else {
                "INDEX"
            };
            connection
                .execute_batch(&format!("DROP {kind} {name};"))
                .unwrap();
        }
        let list = columns.join(",");
        connection
            .execute_batch(&format!("ALTER TABLE {table} RENAME TO {table}_orig;"))
            .unwrap();
        connection.execute_batch(&additive).unwrap();
        connection
            .execute_batch(&format!(
                "INSERT INTO {table} ({list}) SELECT {list} FROM {table}_orig;"
            ))
            .unwrap();
        connection
            .execute_batch(&format!("DROP TABLE {table}_orig;"))
            .unwrap();
        for sql in &attached {
            connection.execute_batch(&format!("{sql};")).unwrap();
        }
        connection
            .execute_batch("PRAGMA legacy_alter_table=OFF; PRAGMA foreign_keys=ON;")
            .unwrap();
    }

    #[test]
    fn object_name_reads_indexes_and_triggers() {
        assert_eq!(
            object_name("CREATE INDEX idx_sessions_status ON sessions(status)").as_deref(),
            Some("idx_sessions_status")
        );
        assert_eq!(
            object_name("CREATE UNIQUE INDEX uq_x ON t(a)").as_deref(),
            Some("uq_x")
        );
        assert_eq!(
            object_name("CREATE TRIGGER t_guard BEFORE DELETE ON t BEGIN SELECT 1; END").as_deref(),
            Some("t_guard")
        );
        assert_eq!(object_name("SELECT 1").as_deref(), None);
    }

    #[test]
    fn rebuild_restores_canonical_text_rows_and_attached_objects() {
        let connection = Connection::open_in_memory().unwrap();
        let canonical = "CREATE TABLE t (\n  id TEXT PRIMARY KEY,\n  moved TEXT,\n  kept TEXT\n)";
        connection.execute_batch(canonical).unwrap();
        connection
            .execute_batch(
                "CREATE INDEX t_kept ON t(kept);
                 CREATE TRIGGER t_no_delete BEFORE DELETE ON t
                 BEGIN SELECT RAISE(ABORT,'no'); END;
                 INSERT INTO t(id,moved,kept) VALUES('a','m1','k1'),('b','m2','k2');",
            )
            .unwrap();
        decanonicalize(&connection, "t", "moved");
        let divergent: String = connection
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='t'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_ne!(divergent, canonical, "the fixture must actually diverge");

        connection
            .execute_batch("PRAGMA foreign_keys=OFF; PRAGMA legacy_alter_table=ON;")
            .unwrap();
        let tx = connection.unchecked_transaction().unwrap();
        rebuild_table(&tx, "t", canonical).expect("rebuild");
        tx.commit().unwrap();

        let restored: String = connection
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='t'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(restored, canonical, "the rebuild must restore exact text");
        let rows: Vec<(String, String, String)> = {
            let mut statement = connection
                .prepare("SELECT id,moved,kept FROM t ORDER BY id")
                .unwrap();
            statement
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
                .unwrap()
                .map(|row| row.unwrap())
                .collect()
        };
        assert_eq!(
            rows,
            vec![
                ("a".to_string(), "m1".to_string(), "k1".to_string()),
                ("b".to_string(), "m2".to_string(), "k2".to_string()),
            ],
            "every row must survive, mapped by column name"
        );
        let attached: Vec<String> = {
            let mut statement = connection
                .prepare(
                    "SELECT name FROM sqlite_master
                      WHERE tbl_name='t' AND type IN ('index','trigger') ORDER BY name",
                )
                .unwrap();
            statement
                .query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .map(|row| row.unwrap())
                .collect()
        };
        assert_eq!(
            attached,
            vec![
                // The implicit PRIMARY KEY index is recreated with the table.
                "sqlite_autoindex_t_1".to_string(),
                "t_kept".to_string(),
                "t_no_delete".to_string()
            ]
        );
    }
}
