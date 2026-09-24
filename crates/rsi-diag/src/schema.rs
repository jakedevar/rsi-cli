//! `rsi-diag schema` — inspect the live SQLite schema (read-only).
//!
//! The schema's single source of truth is the live database: this command reads
//! `sqlite_master` and the `table_info` / `index_list` / `foreign_key_list`
//! pragmas so it can never drift from `rsid`'s migrations the way a checked-in
//! schema doc would. Read-only by design — opens with `SQLITE_OPEN_READ_ONLY`.
//!
//! Modes:
//! - list:    `rsi-diag schema`                — tables with row counts
//! - detail:  `rsi-diag schema <TABLE>`        — columns, indexes, foreign keys
//! - grep:    `rsi-diag schema --grep <PAT>`   — tables/columns matching a substring
//! - version: `rsi-diag schema --version`      — `PRAGMA user_version` (applied schema)

use rusqlite::{Connection, OpenFlags};
use std::fmt::Write as _;
use std::path::Path;

// ---------------------------------------------------------------------------
// Row shapes
// ---------------------------------------------------------------------------

/// One table in the catalog plus its row count.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TableRow {
    name: String,
    rows: i64,
}

/// One column from `PRAGMA table_info`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ColumnInfo {
    name: String,
    ty: String,
    notnull: bool,
    dflt: Option<String>,
    pk: i64,
}

/// One index from `PRAGMA index_list` + `PRAGMA index_info`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct IndexInfo {
    name: String,
    unique: bool,
    columns: Vec<String>,
}

/// One foreign key from `PRAGMA foreign_key_list`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ForeignKey {
    from: String,
    to_table: String,
    to_col: String,
}

/// Full detail for a single table.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TableDetail {
    name: String,
    columns: Vec<ColumnInfo>,
    indexes: Vec<IndexInfo>,
    foreign_keys: Vec<ForeignKey>,
}

/// One (table, column) hit in grep mode.
#[derive(Debug, Clone, PartialEq, Eq)]
struct GrepMatch {
    table: String,
    column: String,
    ty: String,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Quote an SQLite identifier by wrapping in double quotes and doubling any
/// embedded double quotes. Table/index names come from `sqlite_master` (the
/// catalog) or a user-supplied table argument; quoting keeps PRAGMA/`COUNT`
/// statements well-formed without binding (PRAGMA can't bind parameters).
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Minimal JSON string escaping for catalog names (handles `\` and `"`).
fn json_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Render an optional default value as a JSON value (string or `null`).
fn json_opt(s: Option<&str>) -> String {
    match s {
        Some(v) => format!("\"{}\"", json_escape(v)),
        None => "null".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Collectors (read-only SQL)
// ---------------------------------------------------------------------------

/// List user tables (excluding `sqlite_*` internals) with row counts.
fn collect_table_list(conn: &Connection) -> Result<Vec<TableRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT name FROM sqlite_master \
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%' \
             ORDER BY name",
        )
        .map_err(|e| format!("prepare table list: {}", e))?;
    let names: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| format!("query table list: {}", e))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("row table list: {}", e))?;

    let mut out = Vec::with_capacity(names.len());
    for name in names {
        let count: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM {}", quote_ident(&name)),
                [],
                |r| r.get(0),
            )
            .map_err(|e| format!("count {}: {}", name, e))?;
        out.push(TableRow { name, rows: count });
    }
    Ok(out)
}

/// Column list for one table via `PRAGMA table_info`.
fn collect_columns(conn: &Connection, table: &str) -> Result<Vec<ColumnInfo>, String> {
    let sql = format!("PRAGMA table_info({})", quote_ident(table));
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("prepare table_info {}: {}", table, e))?;
    // table_info columns: cid, name, type, notnull, dflt_value, pk
    let cols = stmt
        .query_map([], |row| {
            Ok(ColumnInfo {
                name: row.get::<_, String>(1)?,
                ty: row.get::<_, String>(2)?,
                notnull: row.get::<_, i64>(3)? != 0,
                dflt: row.get::<_, Option<String>>(4)?,
                pk: row.get::<_, i64>(5)?,
            })
        })
        .map_err(|e| format!("query table_info {}: {}", table, e))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("row table_info {}: {}", table, e))?;
    Ok(cols)
}

/// Index list (with their columns) for one table.
fn collect_indexes(conn: &Connection, table: &str) -> Result<Vec<IndexInfo>, String> {
    let sql = format!("PRAGMA index_list({})", quote_ident(table));
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("prepare index_list {}: {}", table, e))?;
    // index_list columns: seq, name, unique, origin, partial
    let raw: Vec<(String, bool)> = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(1)?, row.get::<_, i64>(2)? != 0))
        })
        .map_err(|e| format!("query index_list {}: {}", table, e))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("row index_list {}: {}", table, e))?;

    let mut out = Vec::with_capacity(raw.len());
    for (name, unique) in raw {
        let isql = format!("PRAGMA index_info({})", quote_ident(&name));
        let mut istmt = conn
            .prepare(&isql)
            .map_err(|e| format!("prepare index_info {}: {}", name, e))?;
        // index_info columns: seqno, cid, name
        let columns: Vec<String> = istmt
            .query_map([], |row| row.get::<_, Option<String>>(2))
            .map_err(|e| format!("query index_info {}: {}", name, e))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("row index_info {}: {}", name, e))?
            .into_iter()
            .flatten()
            .collect();
        out.push(IndexInfo {
            name,
            unique,
            columns,
        });
    }
    Ok(out)
}

/// Foreign keys for one table via `PRAGMA foreign_key_list`.
fn collect_foreign_keys(conn: &Connection, table: &str) -> Result<Vec<ForeignKey>, String> {
    let sql = format!("PRAGMA foreign_key_list({})", quote_ident(table));
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("prepare foreign_key_list {}: {}", table, e))?;
    // foreign_key_list columns: id, seq, table, from, to, on_update, on_delete, match
    let fks = stmt
        .query_map([], |row| {
            Ok(ForeignKey {
                to_table: row.get::<_, String>(2)?,
                from: row.get::<_, String>(3)?,
                to_col: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
            })
        })
        .map_err(|e| format!("query foreign_key_list {}: {}", table, e))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("row foreign_key_list {}: {}", table, e))?;
    Ok(fks)
}

/// Assemble full detail for a table. Returns `None` when the table has no
/// columns (i.e. it does not exist in the catalog).
fn collect_table_detail(conn: &Connection, table: &str) -> Result<Option<TableDetail>, String> {
    let columns = collect_columns(conn, table)?;
    if columns.is_empty() {
        return Ok(None);
    }
    let indexes = collect_indexes(conn, table)?;
    let foreign_keys = collect_foreign_keys(conn, table)?;
    Ok(Some(TableDetail {
        name: table.to_string(),
        columns,
        indexes,
        foreign_keys,
    }))
}

/// Find every (table, column) whose table-name or column-name contains
/// `pattern` (case-insensitive).
fn collect_grep(conn: &Connection, pattern: &str) -> Result<Vec<GrepMatch>, String> {
    let needle = pattern.to_lowercase();
    let tables = collect_table_list(conn)?;
    let mut out = Vec::new();
    for t in tables {
        let table_hit = t.name.to_lowercase().contains(&needle);
        for col in collect_columns(conn, &t.name)? {
            if table_hit || col.name.to_lowercase().contains(&needle) {
                out.push(GrepMatch {
                    table: t.name.clone(),
                    column: col.name,
                    ty: col.ty,
                });
            }
        }
    }
    Ok(out)
}

/// Read `PRAGMA user_version` — the schema version applied to this DB file.
fn read_user_version(conn: &Connection) -> Result<i64, String> {
    conn.query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(|e| format!("read user_version: {}", e))
}

// ---------------------------------------------------------------------------
// Renderers
// ---------------------------------------------------------------------------

fn render_table_list(rows: &[TableRow], json: bool) -> String {
    if json {
        let mut out = String::from("[");
        for (i, r) in rows.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let _ = write!(
                out,
                "{{\"table\":\"{}\",\"rows\":{}}}",
                json_escape(&r.name),
                r.rows
            );
        }
        out.push(']');
        return out;
    }
    let mut out = String::new();
    let _ = writeln!(out, "{:<44} {:>12}", "table", "rows");
    for r in rows {
        let _ = writeln!(out, "{:<44} {:>12}", r.name, r.rows);
    }
    let _ = writeln!(out, "({} tables)", rows.len());
    out
}

fn render_table_detail(d: &TableDetail, json: bool) -> String {
    if json {
        let mut out = String::new();
        let _ = write!(
            out,
            "{{\"table\":\"{}\",\"columns\":[",
            json_escape(&d.name)
        );
        for (i, c) in d.columns.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let _ = write!(
                out,
                "{{\"name\":\"{}\",\"type\":\"{}\",\"notnull\":{},\"default\":{},\"pk\":{}}}",
                json_escape(&c.name),
                json_escape(&c.ty),
                c.notnull,
                json_opt(c.dflt.as_deref()),
                c.pk
            );
        }
        out.push_str("],\"indexes\":[");
        for (i, idx) in d.indexes.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let _ = write!(
                out,
                "{{\"name\":\"{}\",\"unique\":{},\"columns\":[",
                json_escape(&idx.name),
                idx.unique
            );
            for (j, col) in idx.columns.iter().enumerate() {
                if j > 0 {
                    out.push(',');
                }
                let _ = write!(out, "\"{}\"", json_escape(col));
            }
            out.push_str("]}");
        }
        out.push_str("],\"foreign_keys\":[");
        for (i, fk) in d.foreign_keys.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let _ = write!(
                out,
                "{{\"from\":\"{}\",\"table\":\"{}\",\"to\":\"{}\"}}",
                json_escape(&fk.from),
                json_escape(&fk.to_table),
                json_escape(&fk.to_col)
            );
        }
        out.push_str("]}");
        return out;
    }

    let mut out = String::new();
    let _ = writeln!(out, "TABLE {}  ({} columns)", d.name, d.columns.len());
    let _ = writeln!(
        out,
        "  {:<32} {:<12} {:<8} {:<8} default",
        "column", "type", "notnull", "pk"
    );
    for c in &d.columns {
        let _ = writeln!(
            out,
            "  {:<32} {:<12} {:<8} {:<8} {}",
            c.name,
            if c.ty.is_empty() { "-" } else { &c.ty },
            if c.notnull { "yes" } else { "" },
            if c.pk > 0 { "yes" } else { "" },
            c.dflt.as_deref().unwrap_or("")
        );
    }
    if !d.indexes.is_empty() {
        out.push_str("INDEXES\n");
        for idx in &d.indexes {
            let kind = if idx.unique { " (unique)" } else { "" };
            let _ = writeln!(out, "  {}{} [{}]", idx.name, kind, idx.columns.join(", "));
        }
    }
    if !d.foreign_keys.is_empty() {
        out.push_str("FOREIGN KEYS\n");
        for fk in &d.foreign_keys {
            let _ = writeln!(out, "  {} -> {}({})", fk.from, fk.to_table, fk.to_col);
        }
    }
    out
}

fn render_grep(matches: &[GrepMatch], json: bool) -> String {
    if json {
        let mut out = String::from("[");
        for (i, m) in matches.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let _ = write!(
                out,
                "{{\"table\":\"{}\",\"column\":\"{}\",\"type\":\"{}\"}}",
                json_escape(&m.table),
                json_escape(&m.column),
                json_escape(&m.ty)
            );
        }
        out.push(']');
        return out;
    }
    let mut out = String::new();
    let _ = writeln!(out, "{:<44} {:<32} type", "table", "column");
    let mut tables = std::collections::BTreeSet::new();
    for m in matches {
        tables.insert(m.table.clone());
        let _ = writeln!(
            out,
            "{:<44} {:<32} {}",
            m.table,
            m.column,
            if m.ty.is_empty() { "-" } else { &m.ty }
        );
    }
    let _ = writeln!(
        out,
        "({} matches across {} tables)",
        matches.len(),
        tables.len()
    );
    out
}

fn render_version(version: i64, json: bool) -> String {
    if json {
        format!("{{\"user_version\":{}}}", version)
    } else {
        format!("user_version: {}\n", version)
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Run the `schema` subcommand. Opens the DB in `READ_ONLY` mode.
pub fn run(
    db_path: &Path,
    table: Option<&str>,
    grep: Option<&str>,
    version: bool,
    json: bool,
) -> Result<(), String> {
    let conn = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|e| format!("open {}: {}", db_path.display(), e))?;

    let output = if version {
        render_version(read_user_version(&conn)?, json)
    } else if let Some(t) = table {
        match collect_table_detail(&conn, t)? {
            Some(d) => render_table_detail(&d, json),
            None => return Err(format!("table not found: {}", t)),
        }
    } else if let Some(p) = grep {
        render_grep(&collect_grep(&conn, p)?, json)
    } else {
        render_table_list(&collect_table_list(&conn)?, json)
    };

    print!("{}", output);
    if json {
        println!();
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an in-memory DB with a small but representative schema.
    fn fixture() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE projects (id TEXT PRIMARY KEY, name TEXT NOT NULL);
             CREATE TABLE sessions (
                 id TEXT PRIMARY KEY,
                 model TEXT,
                 input_tokens INTEGER,
                 project_id TEXT REFERENCES projects(id)
             );
             CREATE INDEX idx_sessions_project ON sessions(project_id);
             INSERT INTO projects (id, name) VALUES ('p1', 'proj');
             INSERT INTO sessions (id, model, input_tokens, project_id)
                 VALUES ('s1', 'claude-opus-4-5', 10, 'p1'),
                        ('s2', 'claude-sonnet-5', 20, 'p1');
             PRAGMA user_version = 66;",
        )
        .unwrap();
        conn
    }

    #[test]
    fn table_list_counts_rows_and_skips_internals() {
        let conn = fixture();
        let rows = collect_table_list(&conn).unwrap();
        // Alphabetical: projects, sessions. No sqlite_* internal tables.
        assert_eq!(rows.len(), 2);
        let sessions = rows.iter().find(|r| r.name == "sessions").unwrap();
        assert_eq!(sessions.rows, 2);
        let projects = rows.iter().find(|r| r.name == "projects").unwrap();
        assert_eq!(projects.rows, 1);
        assert!(!rows.iter().any(|r| r.name.starts_with("sqlite_")));
    }

    #[test]
    fn table_detail_reports_columns_pk_index_and_fk() {
        let conn = fixture();
        let detail = collect_table_detail(&conn, "sessions").unwrap().unwrap();
        assert_eq!(detail.name, "sessions");

        let id = detail.columns.iter().find(|c| c.name == "id").unwrap();
        assert!(id.pk > 0, "id should be primary key");

        // Explicit index is present (auto PK index may also appear).
        assert!(detail.indexes.iter().any(
            |i| i.name == "idx_sessions_project" && i.columns == vec!["project_id".to_string()]
        ));

        // Foreign key sessions.project_id -> projects.id.
        let fk = detail
            .foreign_keys
            .iter()
            .find(|f| f.from == "project_id")
            .unwrap();
        assert_eq!(fk.to_table, "projects");
        assert_eq!(fk.to_col, "id");
    }

    #[test]
    fn missing_table_returns_none() {
        let conn = fixture();
        assert!(
            collect_table_detail(&conn, "does_not_exist")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn grep_matches_column_names_case_insensitively() {
        let conn = fixture();
        let matches = collect_grep(&conn, "TOKEN").unwrap();
        // Only sessions.input_tokens contains "token".
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].table, "sessions");
        assert_eq!(matches[0].column, "input_tokens");
    }

    #[test]
    fn grep_matches_all_columns_when_table_name_matches() {
        let conn = fixture();
        // "session" matches the sessions table name -> all its columns returned.
        let matches = collect_grep(&conn, "session").unwrap();
        assert_eq!(matches.len(), 4);
        assert!(matches.iter().all(|m| m.table == "sessions"));
    }

    #[test]
    fn user_version_is_read() {
        let conn = fixture();
        assert_eq!(read_user_version(&conn).unwrap(), 66);
    }

    #[test]
    fn render_table_list_json_is_well_formed() {
        let rows = vec![
            TableRow {
                name: "sessions".into(),
                rows: 2,
            },
            TableRow {
                name: "projects".into(),
                rows: 1,
            },
        ];
        let out = render_table_list(&rows, true);
        assert!(out.starts_with('['));
        assert!(out.ends_with(']'));
        assert!(out.contains("\"table\":\"sessions\""));
        assert!(out.contains("\"rows\":2"));
    }

    #[test]
    fn render_table_detail_json_includes_sections() {
        let d = TableDetail {
            name: "sessions".into(),
            columns: vec![ColumnInfo {
                name: "id".into(),
                ty: "TEXT".into(),
                notnull: false,
                dflt: None,
                pk: 1,
            }],
            indexes: vec![IndexInfo {
                name: "idx_sessions_project".into(),
                unique: false,
                columns: vec!["project_id".into()],
            }],
            foreign_keys: vec![ForeignKey {
                from: "project_id".into(),
                to_table: "projects".into(),
                to_col: "id".into(),
            }],
        };
        let out = render_table_detail(&d, true);
        assert!(out.contains("\"name\":\"id\""));
        assert!(out.contains("\"pk\":1"));
        assert!(out.contains("\"default\":null"));
        assert!(out.contains("\"name\":\"idx_sessions_project\""));
        assert!(out.contains("\"from\":\"project_id\""));
        assert!(out.contains("\"table\":\"projects\""));
    }

    #[test]
    fn render_version_json_and_human() {
        assert_eq!(render_version(66, true), "{\"user_version\":66}");
        assert_eq!(render_version(66, false), "user_version: 66\n");
    }

    #[test]
    fn json_escape_handles_quotes_and_backslashes() {
        assert_eq!(json_escape(r#"a"b\c"#), r#"a\"b\\c"#);
    }
}
