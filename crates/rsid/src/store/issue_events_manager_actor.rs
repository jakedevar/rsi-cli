//! Forward migration that admits a truthful `manager` actor into the released
//! V97 `issue_events` audit table (Issue #639).
//!
//! V97 ties every session-actor mutation event to a real owning Epic, so an
//! appointed manager holding `IssueCoordinate` could not record a mutation
//! without writing a false immutable `owning_epic_id`. This migration rebuilds
//! `issue_events` (create-new / copy / drop / rename) with exactly one change:
//! `actor_kind='manager'` joins the actor enum with its own provenance
//! disjunct (session id and idempotency key required, no label, no owning
//! Epic). Every other column, CHECK, index and immutability trigger is the
//! V97 definition verbatim and every row is copied verbatim.
//!
//! Issued as V125 at landing. The number appears only in
//! [`MANAGER_ACTOR_VERSION`], [`MANAGER_ACTOR_SOURCE_VERSION`], the
//! `if version < 125` block in `store/mod.rs`, and the released-migration
//! manifest (plus `LATEST_SCHEMA_VERSION` and the rewind watermark while it is
//! the schema head).

use super::{
    ColumnCatalogSpec, ForeignKeyCatalogSpec, IndexCatalogSpec, V97_EVENT_TABLE_SQL, V97_INDEX_SQL,
    V97_TRIGGER_SQL, require_catalog_object_sql, require_column_catalog,
    require_foreign_key_catalog, require_index_catalog,
};
use crate::error::{DaemonError, Result};
use crate::store::Store;
use rusqlite::{Transaction, TransactionBehavior};

// RSI-RELEASED-MIGRATION-BEGIN: issue-events-manager-actor-catalog
/// Schema version this migration produces.
pub(in crate::store) const MANAGER_ACTOR_VERSION: i32 = 125;
/// Exact schema version this migration requires as its source (V124, the
/// live-bookkeeping index migration).
pub(in crate::store) const MANAGER_ACTOR_SOURCE_VERSION: i32 = 124;

const MANAGER_ACTOR_STAGING_TABLE: &str = "issue_events_manager_actor_new";

/// Column list shared by the copy and the verbatim-row comparison.
const ISSUE_EVENT_ALL_COLUMNS: &str = "id,project_id,issue_id,sequence,operation,actor_kind,\
     actor_session_id,owning_epic_id,actor_label,expected_row_version,resulting_row_version,\
     idempotency_key,request_fingerprint,request_json,result_json,occurred_at";

/// The rebuilt table body. Identical to the V97 body except the `actor_kind`
/// enum gains `'manager'` and the provenance CHECK gains the manager disjunct.
const MANAGER_ACTOR_EVENT_TABLE_BODY: &str = "(
             id TEXT PRIMARY KEY CHECK(length(id)=36 AND id=lower(id)),
             project_id TEXT NOT NULL CHECK(length(project_id)=36 AND project_id=lower(project_id)),
             issue_id TEXT NOT NULL CHECK(length(issue_id)=36 AND issue_id=lower(issue_id)),
             sequence INTEGER NOT NULL CHECK(sequence>=1),
             operation TEXT NOT NULL CHECK(operation IN ('baseline_imported','created','content_updated','status_updated','archived','restored','idea_linked','legacy_create_adopted')),
             actor_kind TEXT NOT NULL CHECK(actor_kind IN ('operator','session','system','manager')),
             actor_session_id TEXT CHECK(actor_session_id IS NULL OR (length(actor_session_id)=36 AND actor_session_id=lower(actor_session_id))),
             owning_epic_id TEXT CHECK(owning_epic_id IS NULL OR (length(owning_epic_id)=36 AND owning_epic_id=lower(owning_epic_id))),
             actor_label TEXT CHECK(actor_label IS NULL OR (length(CAST(actor_label AS BLOB)) BETWEEN 1 AND 256 AND instr(actor_label,char(0))=0)),
             expected_row_version INTEGER NOT NULL CHECK(expected_row_version>=0),
             resulting_row_version INTEGER NOT NULL CHECK(resulting_row_version>=1),
             idempotency_key TEXT CHECK(idempotency_key IS NULL OR (length(CAST(idempotency_key AS BLOB)) BETWEEN 1 AND 128 AND instr(idempotency_key,char(0))=0)),
             request_fingerprint TEXT NOT NULL CHECK(length(request_fingerprint)=71 AND substr(request_fingerprint,1,7)='sha256:' AND substr(request_fingerprint,8) NOT GLOB '*[^0-9a-f]*'),
             request_json TEXT NOT NULL CHECK(json_valid(request_json) AND json_extract(request_json,'$.domain')='rsi.issue.request/v1'),
             result_json TEXT NOT NULL CHECK(json_valid(result_json) AND json_extract(result_json,'$.id')=issue_id AND json_extract(result_json,'$.project_id')=project_id AND json_extract(result_json,'$.row_version')=resulting_row_version),
             occurred_at TEXT NOT NULL CHECK(length(occurred_at)=30 AND occurred_at GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9].[0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9]Z'),
             UNIQUE(issue_id,sequence),
             FOREIGN KEY(issue_id,project_id) REFERENCES issues(id,project_id) ON DELETE RESTRICT,
             CHECK(
                 (actor_kind='session' AND actor_session_id IS NOT NULL AND actor_label IS NULL AND idempotency_key IS NOT NULL)
                 OR (actor_kind IN ('operator','system') AND actor_session_id IS NULL AND owning_epic_id IS NULL AND actor_label IS NOT NULL AND idempotency_key IS NULL)
                 OR (actor_kind='manager' AND actor_session_id IS NOT NULL AND actor_label IS NULL AND idempotency_key IS NOT NULL AND owning_epic_id IS NULL)
             ),
             CHECK(owning_epic_id IS NULL OR actor_kind='session'),
             CHECK(
                 actor_kind!='session'
                 OR operation IN ('created','legacy_create_adopted')
                 OR owning_epic_id IS NOT NULL
             ),
             CHECK(
                 actor_kind!='session'
                 OR operation NOT IN ('created','legacy_create_adopted')
                 OR owning_epic_id IS NULL
             ),
             CHECK(
                 (operation IN ('baseline_imported','created') AND expected_row_version=0 AND resulting_row_version=1)
                 OR (operation='legacy_create_adopted' AND expected_row_version=1 AND resulting_row_version=2)
                 OR (operation NOT IN ('baseline_imported','created','legacy_create_adopted') AND resulting_row_version=expected_row_version+1)
             ),
             CHECK(sequence=resulting_row_version),
             CHECK(
                 json_extract(request_json,'$.operation')=operation
                 OR (operation='legacy_create_adopted' AND json_extract(request_json,'$.operation')='created')
             )
         )";

/// Index definitions recreated unchanged from V97.
const MANAGER_ACTOR_INDEX_SQL: &[(&str, &str)] = &[
    (
        "idx_issue_events_actor_key",
        "CREATE UNIQUE INDEX idx_issue_events_actor_key ON issue_events(actor_session_id,idempotency_key) WHERE actor_session_id IS NOT NULL AND idempotency_key IS NOT NULL",
    ),
    (
        "idx_issue_events_project_time",
        "CREATE INDEX idx_issue_events_project_time ON issue_events(project_id,occurred_at,id)",
    ),
    (
        "idx_issue_events_issue_sequence",
        "CREATE INDEX idx_issue_events_issue_sequence ON issue_events(issue_id,sequence)",
    ),
];

/// Immutability triggers recreated unchanged from V97 (`issues_no_delete`
/// lives on `issues` and is untouched by the rebuild).
const MANAGER_ACTOR_TRIGGER_SQL: &[(&str, &str)] = &[
    (
        "issue_events_no_update",
        "CREATE TRIGGER issue_events_no_update BEFORE UPDATE ON issue_events BEGIN SELECT RAISE(ABORT,'issue_events_are_immutable'); END",
    ),
    (
        "issue_events_no_delete",
        "CREATE TRIGGER issue_events_no_delete BEFORE DELETE ON issue_events BEGIN SELECT RAISE(ABORT,'issue_events_are_immutable'); END",
    ),
];

/// Catalog objects owned by `issue_events` after the rebuild: the table, three
/// explicit indexes, two triggers, and the PK and UNIQUE autoindexes (which
/// have no SQL text).
const MANAGER_ACTOR_EVENT_OBJECTS_WITH_SQL: i64 = 6;

const MANAGER_ACTOR_EVENT_COLUMN_CATALOG: &[ColumnCatalogSpec] = &[
    ColumnCatalogSpec {
        name: "id",
        declared_type: "TEXT",
        not_null: 0,
        default_value: None,
        primary_key_position: 1,
        hidden: 0,
    },
    ColumnCatalogSpec {
        name: "project_id",
        declared_type: "TEXT",
        not_null: 1,
        default_value: None,
        primary_key_position: 0,
        hidden: 0,
    },
    ColumnCatalogSpec {
        name: "issue_id",
        declared_type: "TEXT",
        not_null: 1,
        default_value: None,
        primary_key_position: 0,
        hidden: 0,
    },
    ColumnCatalogSpec {
        name: "sequence",
        declared_type: "INTEGER",
        not_null: 1,
        default_value: None,
        primary_key_position: 0,
        hidden: 0,
    },
    ColumnCatalogSpec {
        name: "operation",
        declared_type: "TEXT",
        not_null: 1,
        default_value: None,
        primary_key_position: 0,
        hidden: 0,
    },
    ColumnCatalogSpec {
        name: "actor_kind",
        declared_type: "TEXT",
        not_null: 1,
        default_value: None,
        primary_key_position: 0,
        hidden: 0,
    },
    ColumnCatalogSpec {
        name: "actor_session_id",
        declared_type: "TEXT",
        not_null: 0,
        default_value: None,
        primary_key_position: 0,
        hidden: 0,
    },
    ColumnCatalogSpec {
        name: "owning_epic_id",
        declared_type: "TEXT",
        not_null: 0,
        default_value: None,
        primary_key_position: 0,
        hidden: 0,
    },
    ColumnCatalogSpec {
        name: "actor_label",
        declared_type: "TEXT",
        not_null: 0,
        default_value: None,
        primary_key_position: 0,
        hidden: 0,
    },
    ColumnCatalogSpec {
        name: "expected_row_version",
        declared_type: "INTEGER",
        not_null: 1,
        default_value: None,
        primary_key_position: 0,
        hidden: 0,
    },
    ColumnCatalogSpec {
        name: "resulting_row_version",
        declared_type: "INTEGER",
        not_null: 1,
        default_value: None,
        primary_key_position: 0,
        hidden: 0,
    },
    ColumnCatalogSpec {
        name: "idempotency_key",
        declared_type: "TEXT",
        not_null: 0,
        default_value: None,
        primary_key_position: 0,
        hidden: 0,
    },
    ColumnCatalogSpec {
        name: "request_fingerprint",
        declared_type: "TEXT",
        not_null: 1,
        default_value: None,
        primary_key_position: 0,
        hidden: 0,
    },
    ColumnCatalogSpec {
        name: "request_json",
        declared_type: "TEXT",
        not_null: 1,
        default_value: None,
        primary_key_position: 0,
        hidden: 0,
    },
    ColumnCatalogSpec {
        name: "result_json",
        declared_type: "TEXT",
        not_null: 1,
        default_value: None,
        primary_key_position: 0,
        hidden: 0,
    },
    ColumnCatalogSpec {
        name: "occurred_at",
        declared_type: "TEXT",
        not_null: 1,
        default_value: None,
        primary_key_position: 0,
        hidden: 0,
    },
];

const MANAGER_ACTOR_EVENT_INDEX_CATALOG: &[IndexCatalogSpec] = &[
    IndexCatalogSpec {
        name: "idx_issue_events_actor_key",
        unique: 1,
        origin: "c",
        partial: 1,
    },
    IndexCatalogSpec {
        name: "idx_issue_events_issue_sequence",
        unique: 0,
        origin: "c",
        partial: 0,
    },
    IndexCatalogSpec {
        name: "idx_issue_events_project_time",
        unique: 0,
        origin: "c",
        partial: 0,
    },
    IndexCatalogSpec {
        name: "sqlite_autoindex_issue_events_1",
        unique: 1,
        origin: "pk",
        partial: 0,
    },
    IndexCatalogSpec {
        name: "sqlite_autoindex_issue_events_2",
        unique: 1,
        origin: "u",
        partial: 0,
    },
];

const MANAGER_ACTOR_EVENT_FOREIGN_KEY_CATALOG: &[ForeignKeyCatalogSpec] = &[
    ForeignKeyCatalogSpec {
        id: 0,
        sequence: 0,
        target_table: "issues",
        source_column: "issue_id",
        target_column: "id",
        on_update: "NO ACTION",
        on_delete: "RESTRICT",
        match_kind: "NONE",
    },
    ForeignKeyCatalogSpec {
        id: 0,
        sequence: 1,
        target_table: "issues",
        source_column: "project_id",
        target_column: "project_id",
        on_update: "NO ACTION",
        on_delete: "RESTRICT",
        match_kind: "NONE",
    },
];

/// The exact post-rename table SQL. The engine quotes the new name when
/// `ALTER TABLE ... RENAME TO` rewrites the stored definition.
fn manager_actor_event_table_sql() -> String {
    format!("CREATE TABLE \"issue_events\" {MANAGER_ACTOR_EVENT_TABLE_BODY}")
}

/// Authenticate the rebuilt `issue_events` catalog: table SQL, columns,
/// indexes, foreign keys, index/trigger SQL, and object membership.
pub(in crate::store) fn validate_manager_actor_catalog(tx: &Transaction<'_>) -> Result<()> {
    require_catalog_object_sql(
        tx,
        "table",
        "issue_events",
        "issue_events",
        &manager_actor_event_table_sql(),
    )?;
    require_column_catalog(tx, "issue_events", MANAGER_ACTOR_EVENT_COLUMN_CATALOG)?;
    require_index_catalog(tx, "issue_events", MANAGER_ACTOR_EVENT_INDEX_CATALOG)?;
    require_foreign_key_catalog(tx, "issue_events", MANAGER_ACTOR_EVENT_FOREIGN_KEY_CATALOG)?;
    for (name, sql) in MANAGER_ACTOR_INDEX_SQL {
        require_catalog_object_sql(tx, "index", name, "issue_events", sql)?;
    }
    for (name, sql) in MANAGER_ACTOR_TRIGGER_SQL {
        require_catalog_object_sql(tx, "trigger", name, "issue_events", sql)?;
    }
    let owned: i64 = tx.query_row(
        "SELECT count(*) FROM sqlite_master WHERE tbl_name='issue_events' AND sql IS NOT NULL",
        [],
        |row| row.get(0),
    )?;
    if owned != MANAGER_ACTOR_EVENT_OBJECTS_WITH_SQL {
        return Err(DaemonError::Store(format!(
            "Issue manager-actor catalog membership mismatch: expected \
             {MANAGER_ACTOR_EVENT_OBJECTS_WITH_SQL}, found {owned}"
        )));
    }
    Ok(())
}
// RSI-RELEASED-MIGRATION-END: issue-events-manager-actor-catalog

// RSI-RELEASED-MIGRATION-BEGIN: issue-events-manager-actor-migration
/// Require the exact released V97 `issue_events` catalog as the source.
fn require_v97_issue_events_source(tx: &Transaction<'_>) -> Result<()> {
    require_catalog_object_sql(
        tx,
        "table",
        "issue_events",
        "issue_events",
        V97_EVENT_TABLE_SQL,
    )?;
    for (name, sql) in V97_INDEX_SQL {
        require_catalog_object_sql(tx, "index", name, "issue_events", sql)?;
    }
    for (name, sql) in V97_TRIGGER_SQL
        .iter()
        .filter(|(_, sql)| sql.contains(" ON issue_events "))
    {
        require_catalog_object_sql(tx, "trigger", name, "issue_events", sql)?;
    }
    Ok(())
}

pub(in crate::store) fn apply_manager_actor_migration(store: &Store) -> Result<()> {
    let tx = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate)?;
    let version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version != MANAGER_ACTOR_SOURCE_VERSION {
        return Err(DaemonError::Store(format!(
            "V{MANAGER_ACTOR_VERSION} requires exact V{MANAGER_ACTOR_SOURCE_VERSION} source, \
             found V{version}"
        )));
    }
    require_v97_issue_events_source(&tx)?;
    let source_rows: i64 =
        tx.query_row("SELECT count(*) FROM issue_events", [], |row| row.get(0))?;

    tx.execute_batch(&format!(
        "CREATE TABLE {MANAGER_ACTOR_STAGING_TABLE} {MANAGER_ACTOR_EVENT_TABLE_BODY};
         INSERT INTO {MANAGER_ACTOR_STAGING_TABLE} ({ISSUE_EVENT_ALL_COLUMNS})
             SELECT {ISSUE_EVENT_ALL_COLUMNS} FROM issue_events;"
    ))?;
    // Rows are copied verbatim: equal count and an empty symmetric difference
    // over every column (EXCEPT compares values with their storage class).
    let copied: i64 = tx.query_row(
        &format!("SELECT count(*) FROM {MANAGER_ACTOR_STAGING_TABLE}"),
        [],
        |row| row.get(0),
    )?;
    let differing: i64 = tx.query_row(
        &format!(
            "SELECT (SELECT count(*) FROM (
                        SELECT {ISSUE_EVENT_ALL_COLUMNS} FROM issue_events
                        EXCEPT SELECT {ISSUE_EVENT_ALL_COLUMNS} FROM {MANAGER_ACTOR_STAGING_TABLE}))
                  + (SELECT count(*) FROM (
                        SELECT {ISSUE_EVENT_ALL_COLUMNS} FROM {MANAGER_ACTOR_STAGING_TABLE}
                        EXCEPT SELECT {ISSUE_EVENT_ALL_COLUMNS} FROM issue_events))"
        ),
        [],
        |row| row.get(0),
    )?;
    if copied != source_rows || differing != 0 {
        return Err(DaemonError::Store(format!(
            "V{MANAGER_ACTOR_VERSION} issue_events copy mismatch: source {source_rows}, \
             copied {copied}, differing {differing}"
        )));
    }

    // DROP TABLE removes the V97 indexes and triggers with the table; the
    // implicit foreign-key DELETE it performs fires no triggers.
    tx.execute_batch(&format!(
        "DROP TABLE issue_events;
         ALTER TABLE {MANAGER_ACTOR_STAGING_TABLE} RENAME TO issue_events;"
    ))?;
    for (_, sql) in MANAGER_ACTOR_INDEX_SQL
        .iter()
        .chain(MANAGER_ACTOR_TRIGGER_SQL)
    {
        tx.execute_batch(sql)?;
    }
    validate_manager_actor_catalog(&tx)?;

    let integrity: String = tx.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        return Err(DaemonError::Store(format!(
            "V{MANAGER_ACTOR_VERSION} issue_events rebuild requires integrity_check=ok, \
             got {integrity}"
        )));
    }
    let foreign_key_errors: i64 =
        tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })?;
    if foreign_key_errors != 0 {
        return Err(DaemonError::Store(format!(
            "V{MANAGER_ACTOR_VERSION} issue_events rebuild found {foreign_key_errors} \
             foreign-key violations"
        )));
    }

    tx.pragma_update(None, "user_version", MANAGER_ACTOR_VERSION)?;
    tx.commit()?;
    Ok(())
}
// RSI-RELEASED-MIGRATION-END: issue-events-manager-actor-migration

/// Whether the live `issue_events` definition admits the manager actor. Used
/// by the migration-chain fixtures to check a rewound store's claimed version.
#[cfg(test)]
pub(in crate::store) fn issue_events_admit_manager_actor(
    connection: &rusqlite::Connection,
) -> bool {
    connection
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='issue_events'",
            [],
            |row| row.get::<_, String>(0),
        )
        .is_ok_and(|sql| sql.contains("actor_kind='manager'"))
}

/// Fixture teardown: rebuild the exact released V97 `issue_events` catalog
/// (unquoted table SQL, V97 indexes and triggers) with every row copied back,
/// and pin the source version. Panics if a manager-actor row exists, because
/// V97 cannot represent it.
#[cfg(test)]
#[allow(clippy::expect_used)]
pub(in crate::store) fn rewind_manager_actor_fixture_to_source(connection: &rusqlite::Connection) {
    let version: i32 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("read user_version before manager-actor rewind");
    if version == MANAGER_ACTOR_SOURCE_VERSION {
        return;
    }
    assert_eq!(
        version, MANAGER_ACTOR_VERSION,
        "manager-actor rewind requires V{MANAGER_ACTOR_VERSION}"
    );
    let tx = Transaction::new_unchecked(connection, TransactionBehavior::Immediate)
        .expect("begin manager-actor rewind");
    tx.execute_batch(
        "DROP TRIGGER issue_events_no_update;
         DROP TRIGGER issue_events_no_delete;
         DROP INDEX idx_issue_events_actor_key;
         DROP INDEX idx_issue_events_project_time;
         DROP INDEX idx_issue_events_issue_sequence;
         ALTER TABLE issue_events RENAME TO issue_events_manager_actor_rewind;",
    )
    .expect("detach the manager-actor issue_events table");
    tx.execute_batch(V97_EVENT_TABLE_SQL)
        .expect("recreate the V97 issue_events table");
    tx.execute_batch(&format!(
        "INSERT INTO issue_events ({ISSUE_EVENT_ALL_COLUMNS})
             SELECT {ISSUE_EVENT_ALL_COLUMNS} FROM issue_events_manager_actor_rewind;
         DROP TABLE issue_events_manager_actor_rewind;"
    ))
    .expect("copy issue_events rows back into the V97 shape");
    for (_, sql) in V97_INDEX_SQL.iter().chain(
        V97_TRIGGER_SQL
            .iter()
            .filter(|(_, sql)| sql.contains(" ON issue_events ")),
    ) {
        tx.execute_batch(sql)
            .expect("recreate V97 issue_events index/trigger");
    }
    require_v97_issue_events_source(&tx).expect("rewound issue_events matches released V97");
    tx.pragma_update(None, "user_version", MANAGER_ACTOR_SOURCE_VERSION)
        .expect("pin the manager-actor source version");
    tx.commit().expect("commit manager-actor rewind");
}

#[cfg(test)]
mod tests {
    use super::super::normalized_sql;
    use super::*;

    /// The rebuilt table is the released V97 table with exactly two edits: the
    /// actor enum gains `'manager'` and the provenance CHECK gains the manager
    /// disjunct. Indexes and immutability triggers are the V97 text verbatim.
    #[test]
    fn issue_events_manager_actor_ddl_is_released_v97_plus_only_the_manager_actor() {
        let released = normalized_sql(V97_EVENT_TABLE_SQL);
        let enum_before = "CHECK(actor_kind IN ('operator','session','system'))";
        let enum_after = "CHECK(actor_kind IN ('operator','session','system','manager'))";
        let disjunct_before = "actor_label IS NOT NULL AND idempotency_key IS NULL) ),";
        let disjunct_after = "actor_label IS NOT NULL AND idempotency_key IS NULL) \
             OR (actor_kind='manager' AND actor_session_id IS NOT NULL AND actor_label IS NULL \
             AND idempotency_key IS NOT NULL AND owning_epic_id IS NULL) ),";
        assert_eq!(released.matches(enum_before).count(), 1);
        assert_eq!(released.matches(disjunct_before).count(), 1);
        let expected = released
            .replacen(enum_before, enum_after, 1)
            .replacen(disjunct_before, disjunct_after, 1)
            .replacen(
                "CREATE TABLE issue_events (",
                "CREATE TABLE \"issue_events\" (",
                1,
            );
        assert_eq!(normalized_sql(&manager_actor_event_table_sql()), expected);

        assert_eq!(MANAGER_ACTOR_INDEX_SQL, V97_INDEX_SQL);
        let released_triggers = V97_TRIGGER_SQL
            .iter()
            .filter(|(_, sql)| sql.contains(" ON issue_events "))
            .copied()
            .collect::<Vec<_>>();
        assert_eq!(MANAGER_ACTOR_TRIGGER_SQL, released_triggers.as_slice());
    }
}
