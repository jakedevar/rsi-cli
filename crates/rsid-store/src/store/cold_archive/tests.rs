//! Cold-archive tests (#2384, slice S1): the row codec, the archive file, the cold
//! set and its holds, the live-tail rules, the stage and verify passes with a
//! crash injected at every step, the V176 guards and the migration.
//!
//! Every database here is a file fixture (`Store::open_test_fixture`) in a
//! temporary directory; nothing touches `~/.rsi`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    clippy::similar_names
)]

use super::candidates::{
    EVENT_FK_REFERENCES, HOLD_CODES, HOLD_STATE_CLASSES, HOLDS, HoldContext, MOVABLE_COUNT_SQL,
    MOVABLE_EVENTS_SQL, MOVABLE_PROVENANCE_SQL, PROVENANCE_COLUMNS, StateClass, candidate_page,
    check_hold_classification, ensure_expected_columns, prove_age, recently_restored, session_hold,
    session_status_names,
};
use super::format::{
    ColumnRoles, RowEncoder, columns_json, compress, decode_rows, decompress_checked, sha256_hex,
    sqlite_length_of_value, table_columns,
};
use super::writer::{ArchiveFile, ChunkInput, open_archive_readonly, read_chunk};
use super::*;
use crate::store::Store;
use crate::test_support::make_test_session;
use chrono::{Duration, Utc};
use rsi_common::types::{ConversationEvent, EventType, Role, SessionStatus};
use rusqlite::types::Value;
use rusqlite::{Connection, params};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

pub(super) fn ts(days_ago: i64) -> String {
    (Utc::now() - Duration::days(days_ago)).to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

fn fast_limits() -> PassLimits {
    PassLimits {
        batch_max_sessions: 32,
        batch_max_raw_bytes: 64 * 1024 * 1024,
        wall_budget: std::time::Duration::from_secs(120),
        batch_sleep: std::time::Duration::ZERO,
        gate_poll: std::time::Duration::ZERO,
    }
}

#[allow(dead_code)]
pub(super) struct Fx {
    pub(super) _dir: tempfile::TempDir,
    pub(super) db: PathBuf,
    pub(super) archive: PathBuf,
    pub(super) store: Store,
    pub(super) read: Connection,
}

impl Fx {
    pub(super) fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("rsi.db");
        let store = Store::open_test_fixture(&db).unwrap();
        let read = open_read_connection(&db).unwrap();
        let archive = dir.path().join("archive");
        Self {
            _dir: dir,
            db,
            archive,
            store,
            read,
        }
    }

    pub(super) fn settings(&self, after_days: u32) -> ColdSettings {
        ColdSettings {
            enabled: true,
            after_days,
            dir: self.archive.clone(),
            forbidden_roots: Vec::new(),
        }
    }

    pub(super) fn stage(&self, after_days: u32) -> ColdResult<StageReport> {
        let settings = self.settings(after_days);
        let run_id = self
            .store
            .cold_begin_run("run_now", ColdMode::Stage)
            .unwrap();
        let params = PassParams {
            run_id,
            settings: &settings,
            limits: fast_limits(),
            batch_gate: None,
            cancel: None,
        };
        run_stage_pass(&DirectStoreAccess(&self.store), &self.read, &params)
    }

    pub(super) fn verify(&self) -> ColdResult<VerifyReport> {
        let settings = self.settings(30);
        let run_id = self
            .store
            .cold_begin_run("run_now", ColdMode::Verify)
            .unwrap();
        let params = PassParams {
            run_id,
            settings: &settings,
            limits: fast_limits(),
            batch_gate: None,
            cancel: None,
        };
        run_verify_pass(&DirectStoreAccess(&self.store), &params)
    }

    /// A session created `age_days` ago (its `updated_at` is now, as after a
    /// sandbox-maintenance bump).
    pub(super) fn session(&self, status: SessionStatus, age_days: i64) -> Uuid {
        let mut session = make_test_session();
        session.status = status;
        session.created_at = Utc::now() - Duration::days(age_days);
        session.updated_at = Utc::now();
        self.store.insert_session(&session).unwrap();
        session.id
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn event_at(
        &self,
        session_id: Uuid,
        sequence: i32,
        event_type: &str,
        role: Option<&str>,
        content: &str,
        tool_name: Option<&str>,
        tool_input: Option<&str>,
        created_at: &str,
    ) -> i64 {
        self.store
            .conn
            .execute(
                "INSERT INTO conversation_events(session_id, sequence, event_type, role, content,
                    tool_name, tool_input, created_at, tool_use_id, metadata)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    session_id.to_string(),
                    sequence,
                    event_type,
                    role,
                    content,
                    tool_name,
                    tool_input,
                    created_at,
                    (event_type == "ToolResult" || event_type == "ToolUse")
                        .then(|| format!("toolu_{sequence}")),
                    (event_type == "ToolResult").then(|| "{\"exit_code\":0}".to_string()),
                ],
            )
            .unwrap();
        self.store.conn.last_insert_rowid()
    }

    pub(super) fn event(
        &self,
        session_id: Uuid,
        sequence: i32,
        event_type: &str,
        role: Option<&str>,
        content: &str,
        age_days: i64,
    ) -> i64 {
        self.event_at(
            session_id,
            sequence,
            event_type,
            role,
            content,
            None,
            None,
            &ts(age_days),
        )
    }

    pub(super) fn invocation(&self, session_id: Uuid) -> String {
        if let Ok(id) = self.store.conn.query_row(
            "SELECT id FROM model_invocations WHERE session_id = ?1 LIMIT 1",
            [session_id.to_string()],
            |row| row.get::<_, String>(0),
        ) {
            return id;
        }
        let id = Uuid::new_v4().to_string();
        let now = ts(0);
        self.store
            .conn
            .execute(
                "INSERT INTO model_invocations(
                    id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,
                    provider,model,trigger_source,session_id,policy_snapshot_json,
                    usage_confidence,created_at,completed_at
                 ) VALUES(?1,'session_launch_fresh','session_lifecycle','foreground',
                          'paid_capable','admitted','completed','Codex','gpt-5','cold-archive-test',
                          ?2,'{}','unavailable',?3,?3)",
                params![id, session_id.to_string(), now],
            )
            .unwrap();
        id
    }

    pub(super) fn provenance(&self, session_id: Uuid, event_id: i64, kind: &str) {
        let invocation = self.invocation(session_id);
        self.store
            .conn
            .execute(
                "INSERT INTO conversation_event_provenance(
                    conversation_event_id, producer_kind, model_invocation_id,
                    provider_event_type, created_at)
                 VALUES (?1, ?2, ?3, 'provider/event', ?4)",
                params![event_id, kind, invocation, ts(0)],
            )
            .unwrap();
    }

    /// An old session shaped like a real transcript: user message, system row,
    /// tool use/result pairs with provenance, thinking, final assistant message.
    /// Movable rows: sequences 2 and 3..=(2 + 2 * pairs) and thinking.
    pub(super) fn cold_session(&self, status: SessionStatus, age_days: i64, pairs: i32) -> Uuid {
        let sid = self.session(status, age_days);
        let base = age_days + 1;
        self.event(
            sid,
            1,
            "Message",
            Some("User"),
            "start — résumé 日本語 ✓",
            base,
        );
        self.event(sid, 2, "System", None, "init", base);
        let mut sequence = 3;
        for pair in 0..pairs {
            let input = format!("{{\"cmd\":\"ls {pair}\"}}");
            let tool_use = self.event_at(
                sid,
                sequence,
                "ToolUse",
                Some("Assistant"),
                "",
                Some("Bash"),
                Some(input.as_str()),
                &ts(base),
            );
            self.provenance(sid, tool_use, "provider_other");
            sequence += 1;
            let result = self.event(
                sid,
                sequence,
                "ToolResult",
                None,
                &format!("output {pair}: {}", "ünïcode line\n".repeat(400)),
                base,
            );
            self.provenance(sid, result, "provider_other");
            sequence += 1;
        }
        let thinking = self.event(sid, sequence, "Thinking", None, "hmm", base);
        self.provenance(sid, thinking, "provider_other");
        sequence += 1;
        let last = self.event(sid, sequence, "Message", Some("Assistant"), "done", base);
        self.provenance(sid, last, "provider_assistant_output");
        sid
    }

    pub(super) fn count(&self, sql: &str, session_id: Option<Uuid>) -> i64 {
        match session_id {
            Some(id) => self
                .store
                .conn
                .query_row(sql, [id.to_string()], |row| row.get(0))
                .unwrap(),
            None => self
                .store
                .conn
                .query_row(sql, [], |row| row.get(0))
                .unwrap(),
        }
    }

    pub(super) fn live_counts(&self) -> (i64, i64, i64) {
        (
            self.count("SELECT COUNT(*) FROM conversation_events", None),
            self.count("SELECT COUNT(*) FROM conversation_event_provenance", None),
            self.count("SELECT COUNT(*) FROM sessions", None),
        )
    }

    pub(super) fn row(&self, session_id: Uuid) -> Option<ColdSessionRow> {
        self.store
            .cold_session_row(&session_id.to_string())
            .unwrap()
    }

    pub(super) fn archive_paths(&self) -> Vec<PathBuf> {
        self.store
            .cold_archive_files()
            .unwrap()
            .into_iter()
            .map(|file| PathBuf::from(file.path))
            .collect()
    }

    pub(super) fn archive_chunk_count(&self) -> i64 {
        self.archive_paths()
            .iter()
            .filter(|path| path.is_file())
            .map(|path| {
                let conn = open_archive_readonly(path).unwrap();
                conn.query_row("SELECT COUNT(*) FROM chunks", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap()
            })
            .sum()
    }

    /// Rows of `sql` for one session as owned values.
    pub(super) fn live_rows(&self, sql: &str, session_id: Uuid, columns: usize) -> Vec<Vec<Value>> {
        let mut stmt = self.read.prepare(sql).unwrap();
        let mut rows = stmt.query([session_id.to_string()]).unwrap();
        let mut out = Vec::new();
        while let Some(row) = rows.next().unwrap() {
            out.push(
                (0..columns)
                    .map(|index| row.get::<_, Value>(index).unwrap())
                    .collect(),
            );
        }
        out
    }

    /// Decodes the rows of a session's archived chunk.
    pub(super) fn archived_rows(
        &self,
        chunk_id: i64,
        file_id: i64,
        columns: usize,
    ) -> Vec<Vec<Value>> {
        let file = self
            .store
            .cold_archive_files()
            .unwrap()
            .into_iter()
            .find(|file| file.file_id == file_id)
            .unwrap();
        let conn = open_archive_readonly(Path::new(&file.path)).unwrap();
        let chunk = read_chunk(&conn, chunk_id).unwrap().unwrap();
        let raw = decompress_checked(&chunk.payload, chunk.raw_bytes, &chunk.raw_sha256).unwrap();
        decode_rows(&raw, columns).unwrap()
    }
}

/// Inserts one row into any table for a hold fixture: the listed columns get the
/// listed values, every other NOT NULL column without a default a dummy. Foreign
/// keys and CHECK constraints are off and the table's triggers are dropped (the
/// fixture database is a throwaway); the holds read only the listed columns.
pub(super) fn force_insert(conn: &Connection, table: &str, set: &[(&str, Value)]) {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    conn.execute_batch("PRAGMA foreign_keys=OFF; PRAGMA ignore_check_constraints=ON;")
        .unwrap();
    drop_triggers(conn, table);
    let info: Vec<(String, String, bool, bool, i64)> = conn
        .prepare(
            "SELECT name, type, \"notnull\", dflt_value IS NOT NULL, pk
             FROM pragma_table_info(?1) ORDER BY cid",
        )
        .unwrap()
        .query_map([table], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get::<_, i64>(2)? != 0,
                row.get::<_, i64>(3)? != 0,
                row.get(4)?,
            ))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let mut columns: Vec<(String, Value)> = Vec::new();
    for (name, declared, notnull, has_default, pk) in info {
        if let Some((_, value)) = set.iter().find(|(candidate, _)| *candidate == name) {
            columns.push((name, value.clone()));
            continue;
        }
        let upper = declared.to_ascii_uppercase();
        if (notnull && !has_default) || (pk > 0 && upper.contains("TEXT")) {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let value = if upper.contains("INT") {
                // Unique, because some tables keep a UNIQUE integer (a grant version).
                Value::Integer(i64::try_from(n).unwrap())
            } else if upper.contains("REAL") {
                Value::Real(0.0)
            } else if upper.contains("BLOB") {
                Value::Blob(vec![0])
            } else {
                Value::Text(format!("x-{n}"))
            };
            columns.push((name, value));
        }
    }
    let names: Vec<String> = columns
        .iter()
        .map(|(name, _)| format!("\"{name}\""))
        .collect();
    let marks: Vec<String> = (1..=columns.len()).map(|n| format!("?{n}")).collect();
    let sql = format!(
        "INSERT INTO \"{table}\" ({}) VALUES ({})",
        names.join(","),
        marks.join(",")
    );
    conn.execute(
        &sql,
        rusqlite::params_from_iter(columns.iter().map(|(_, value)| value)),
    )
    .unwrap();
}

fn drop_triggers(conn: &Connection, table: &str) {
    let names: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='trigger' AND tbl_name=?1")
        .unwrap()
        .query_map([table], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    for name in names {
        conn.execute_batch(&format!("DROP TRIGGER \"{name}\""))
            .unwrap();
    }
}

fn text(value: &str) -> Value {
    Value::Text(value.to_string())
}

/// A canonical `cold_archive_sessions` row in `state` for a real session.
fn index_row(store: &Store, session_id: Uuid, state: &str, restored_at: Option<&str>) {
    let file_id = store
        .cold_register_archive_file("/tmp/cold-fixture/rsi-cold-2026-01.sqlite", "2026-01", 1)
        .unwrap();
    store
        .conn
        .execute(
            "INSERT OR REPLACE INTO cold_archive_sessions(
                session_id, state, generation, file_id, events_chunk_id, provenance_chunk_id,
                pre_event_count, pre_provenance_count, archived_event_count,
                archived_provenance_count, archived_content_len, archived_min_sequence,
                archived_max_sequence, events_sha256, provenance_sha256, raw_bytes,
                compressed_bytes, staged_at, restored_at, updated_at)
             VALUES (?1, ?2, 1, ?3, 1, NULL, 1, 0, 1, 0, 0, 1, 1, ?4, NULL, 10, 5, ?5, ?6, ?5)",
            params![
                session_id.to_string(),
                state,
                file_id,
                "0".repeat(64),
                ts(0),
                restored_at,
            ],
        )
        .unwrap();
}

fn set_state(store: &Store, session_id: Uuid, state: &str) {
    store
        .conn
        .execute(
            "UPDATE cold_archive_sessions SET state = ?2 WHERE session_id = ?1",
            params![session_id.to_string(), state],
        )
        .unwrap();
}

// ---------------------------------------------------------------------------
// Row codec and digests
// ---------------------------------------------------------------------------

const CODEC_ROLES: ColumnRoles = ColumnRoles {
    id: Some(0),
    sequence: Some(1),
    content: Some(5),
};

fn codec_table() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE t(id INTEGER PRIMARY KEY, seq INTEGER, a, b, c, content TEXT)",
    )
    .unwrap();
    let rows: Vec<(i64, i64, Value, Value, Value, Value)> = vec![
        (
            1,
            1,
            Value::Null,
            Value::Integer(i64::MIN),
            Value::Real(1.5e300),
            text("plain"),
        ),
        (
            2,
            2,
            Value::Integer(i64::MAX),
            Value::Real(-2.25),
            Value::Blob(Vec::new()),
            text(""),
        ),
        (
            3,
            3,
            text("日本語 ✓ ñ \u{1F600}"),
            Value::Blob((0..=255).collect()),
            text(""),
            text("résumé — naïve"),
        ),
        (
            4,
            4,
            text("a\u{0}b"),
            Value::Null,
            Value::Real(0.0),
            text("ab\u{0}cd"),
        ),
        (
            5,
            5,
            Value::Null,
            Value::Null,
            Value::Null,
            text(&"x".repeat(1 << 20)),
        ),
    ];
    for (id, seq, a, b, c, content) in rows {
        conn.execute(
            "INSERT INTO t(id, seq, a, b, c, content) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, seq, a, b, c, content],
        )
        .unwrap();
    }
    conn
}

fn encode_table(conn: &Connection) -> (EncodedRows, Vec<Vec<Value>>) {
    let mut encoder = RowEncoder::new(6, CODEC_ROLES);
    let mut originals: Vec<Vec<Value>> = Vec::new();
    let mut stmt = conn
        .prepare("SELECT id, seq, a, b, c, content FROM t ORDER BY id")
        .unwrap();
    let mut rows = stmt.query([]).unwrap();
    while let Some(row) = rows.next().unwrap() {
        encoder.push_row(row).unwrap();
        originals.push((0..6).map(|i| row.get::<_, Value>(i).unwrap()).collect());
    }
    (encoder.finish(), originals)
}

use super::format::EncodedRows;

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn codec_round_trips_every_storage_class() {
    let conn = codec_table();
    let (encoded, originals) = encode_table(&conn);
    assert_eq!(encoded.row_count, 5);
    assert_eq!(encoded.min_id, Some(1));
    assert_eq!(encoded.max_id, Some(5));
    assert_eq!(encoded.min_sequence, Some(1));
    assert_eq!(encoded.max_sequence, Some(5));
    let sqlite_sum: i64 = conn
        .query_row("SELECT SUM(LENGTH(content)) FROM t", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        encoded.content_len, sqlite_sum,
        "the codec counts content exactly as SQLite's LENGTH does (non-ASCII, NUL, empty)"
    );

    let payload = compress(&encoded.raw).unwrap();
    assert!(
        payload.len() < encoded.raw.len(),
        "a repetitive megabyte compresses"
    );
    let raw = decompress_checked(
        &payload,
        i64::try_from(encoded.raw.len()).unwrap(),
        &encoded.sha256(),
    )
    .unwrap();
    assert_eq!(raw, encoded.raw);
    let decoded = decode_rows(&raw, 6).unwrap();
    assert_eq!(
        decoded, originals,
        "every storage class round-trips exactly"
    );
    let decoded_len: i64 = decoded
        .iter()
        .map(|row| sqlite_length_of_value(&row[5]))
        .sum();
    assert_eq!(decoded_len, sqlite_sum);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn codec_digest_is_stable_and_sensitive() {
    assert_eq!(
        sha256_hex(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    let conn = codec_table();
    let (first, _) = encode_table(&conn);
    let (second, _) = encode_table(&conn);
    assert_eq!(first.sha256(), second.sha256());
    assert_eq!(first.raw, second.raw);
    conn.execute("UPDATE t SET content = content || 'z' WHERE id = 1", [])
        .unwrap();
    let (third, _) = encode_table(&conn);
    assert_ne!(first.sha256(), third.sha256());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn empty_session_encodes_zero_rows() {
    let conn = codec_table();
    conn.execute("DELETE FROM t", []).unwrap();
    let (encoded, _) = encode_table(&conn);
    assert_eq!(encoded.row_count, 0);
    assert!(encoded.raw.is_empty());
    assert_eq!(encoded.min_id, None);
    assert_eq!(encoded.content_len, 0);
    assert!(decode_rows(&encoded.raw, 6).unwrap().is_empty());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn decoding_rejects_truncated_corrupt_and_mismatched_streams() {
    let conn = codec_table();
    conn.execute("DELETE FROM t WHERE id > 3", []).unwrap();
    let (encoded, _) = encode_table(&conn);
    let full = decode_rows(&encoded.raw, 6).unwrap();
    assert_eq!(full.len(), 3);
    for cut in 1..encoded.raw.len() {
        match decode_rows(&encoded.raw[..cut], 6) {
            Err(error) => assert_eq!(error.code, "cold_archive_verify_decode", "cut at {cut}"),
            Ok(rows) => assert!(rows.len() < 3, "a truncated stream never decodes whole"),
        }
    }
    assert_eq!(
        decode_rows(&[9], 1).unwrap_err().code,
        "cold_archive_verify_decode",
        "an unknown tag"
    );
    assert_eq!(
        decode_rows(&[3, 1, 0, 0, 0, 0xff], 1).unwrap_err().code,
        "cold_archive_verify_decode",
        "invalid UTF-8 in a TEXT value"
    );
    assert!(decode_rows(&[], 0).is_err(), "a chunk needs columns");

    let payload = compress(&encoded.raw).unwrap();
    let length = i64::try_from(encoded.raw.len()).unwrap();
    assert_eq!(
        decompress_checked(&payload, length, &"0".repeat(64))
            .unwrap_err()
            .code,
        "cold_archive_verify_sha_mismatch"
    );
    assert_eq!(
        decompress_checked(&payload, length + 1, &encoded.sha256())
            .unwrap_err()
            .code,
        "cold_archive_verify_raw_len"
    );
    assert_eq!(
        decompress_checked(&[0, 1, 2, 3], length, &encoded.sha256())
            .unwrap_err()
            .code,
        "cold_archive_verify_decompress"
    );
}

// ---------------------------------------------------------------------------
// The archive file
// ---------------------------------------------------------------------------

fn sample_chunk(content: &str) -> (EncodedRows, String, String) {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE s(id INTEGER PRIMARY KEY, sequence INTEGER, content TEXT);
         INSERT INTO s VALUES (1, 1, 'first');",
    )
    .unwrap();
    conn.execute("INSERT INTO s VALUES (2, 2, ?1)", [content])
        .unwrap();
    let mut encoder = RowEncoder::new(
        3,
        ColumnRoles {
            id: Some(0),
            sequence: Some(1),
            content: Some(2),
        },
    );
    let mut stmt = conn
        .prepare("SELECT id, sequence, content FROM s ORDER BY id")
        .unwrap();
    let mut rows = stmt.query([]).unwrap();
    while let Some(row) = rows.next().unwrap() {
        encoder.push_row(row).unwrap();
    }
    let encoded = encoder.finish();
    let sha = encoded.sha256();
    let columns = columns_json(&table_columns(&conn, "s").unwrap()).unwrap();
    (encoded, sha, columns)
}

fn input<'a>(table: &'static str, chunk: &'a (EncodedRows, String, String)) -> ChunkInput<'a> {
    ChunkInput {
        table,
        columns_json: &chunk.2,
        encoded: &chunk.0,
        sha256: &chunk.1,
        source_schema_version: 176,
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn archive_file_is_created_with_its_pragmas_and_refuses_foreign_files() {
    let dir = tempfile::tempdir().unwrap();
    let (file, created) = ArchiveFile::open_or_create(dir.path(), "2026-03", 176).unwrap();
    assert!(created);
    drop(file);
    let (_, created_again) = ArchiveFile::open_or_create(dir.path(), "2026-03", 176).unwrap();
    assert!(
        !created_again,
        "an existing archive is opened, not recreated"
    );

    let path = dir.path().join(archive_file_name("2026-03"));
    assert_eq!(path.file_name().unwrap(), "rsi-cold-2026-03.sqlite");
    let conn = Connection::open(&path).unwrap();
    let pragma = |name: &str| -> String {
        conn.query_row(&format!("PRAGMA {name}"), [], |row| {
            Ok(row.get::<_, Value>(0)?)
        })
        .map(|value| match value {
            Value::Integer(number) => number.to_string(),
            Value::Text(text) => text,
            other => format!("{other:?}"),
        })
        .unwrap()
    };
    assert_eq!(pragma("page_size"), "65536");
    assert_eq!(pragma("journal_mode"), "delete");
    assert_eq!(pragma("application_id"), ARCHIVE_APPLICATION_ID.to_string());
    assert_eq!(ARCHIVE_APPLICATION_ID, 0x5253_4943);
    assert_eq!(ARCHIVE_APPLICATION_ID, 1_381_189_955);
    assert_eq!(pragma("user_version"), ARCHIVE_FORMAT_VERSION.to_string());
    let meta: BTreeSet<String> = conn
        .prepare("SELECT key FROM archive_meta")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    for key in [
        "format_version",
        "month",
        "created_at",
        "writer_build",
        "codec",
        "row_codec",
        "source_schema_version_at_creation",
    ] {
        assert!(meta.contains(key), "archive_meta has {key}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "archives are owner-only");
    }

    let foreign = dir.path().join("rsi-cold-2026-04.sqlite");
    Connection::open(&foreign)
        .unwrap()
        .execute_batch("CREATE TABLE other(x)")
        .unwrap();
    let error = ArchiveFile::open_or_create(dir.path(), "2026-04", 176)
        .err()
        .unwrap();
    assert_eq!(error.code, "cold_archive_bad_format");
    let error = ArchiveFile::open_or_create(dir.path(), "2026-4", 176)
        .err()
        .unwrap();
    assert_eq!(error.code, "cold_archive_bad_month");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn archive_chunks_and_snapshots_are_append_only() {
    let dir = tempfile::tempdir().unwrap();
    let (mut file, _) = ArchiveFile::open_or_create(dir.path(), "2026-03", 176).unwrap();
    let chunk = sample_chunk("second");
    file.write_session(
        "11111111-1111-4111-8111-111111111111",
        1,
        &input("conversation_events", &chunk),
        None,
        "{\"id\":\"1\"}",
    )
    .unwrap();
    let conn = Connection::open(dir.path().join(archive_file_name("2026-03"))).unwrap();
    for sql in [
        "UPDATE chunks SET codec = 'zstd'",
        "DELETE FROM chunks",
        "UPDATE session_snapshots SET session_json = '{}'",
        "DELETE FROM session_snapshots",
    ] {
        let error = conn.execute(sql, []).unwrap_err();
        assert!(
            error.to_string().contains("append-only"),
            "{sql} must be refused: {error}"
        );
    }
    let rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM chunks", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 1);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn writing_a_session_twice_reuses_the_chunk_and_changed_data_takes_a_new_generation() {
    let dir = tempfile::tempdir().unwrap();
    let (mut file, _) = ArchiveFile::open_or_create(dir.path(), "2026-03", 176).unwrap();
    let session = "22222222-2222-4222-8222-222222222222";
    let first = sample_chunk("second");
    let provenance = sample_chunk("prov");
    let written = file
        .write_session(
            session,
            1,
            &input("conversation_events", &first),
            Some(&input("conversation_event_provenance", &provenance)),
            "{}",
        )
        .unwrap();
    assert_eq!(written.generation, 1);
    assert!(!written.reused);
    assert!(written.provenance_chunk_id.is_some());
    assert!(written.compressed_bytes > 0);

    let again = file
        .write_session(
            session,
            1,
            &input("conversation_events", &first),
            Some(&input("conversation_event_provenance", &provenance)),
            "{}",
        )
        .unwrap();
    assert!(again.reused, "identical data is not written twice");
    assert_eq!(again.events_chunk_id, written.events_chunk_id);
    assert_eq!(again.provenance_chunk_id, written.provenance_chunk_id);
    assert_eq!(file.stats().unwrap().0, 2);

    let changed = sample_chunk("a different second row");
    let moved = file
        .write_session(
            session,
            1,
            &input("conversation_events", &changed),
            Some(&input("conversation_event_provenance", &provenance)),
            "{}",
        )
        .unwrap();
    assert_eq!(
        moved.generation, 2,
        "a changed session takes the next generation"
    );
    assert!(!moved.reused);
    let replay = file
        .write_session(
            session,
            1,
            &input("conversation_events", &changed),
            Some(&input("conversation_event_provenance", &provenance)),
            "{}",
        )
        .unwrap();
    assert!(replay.reused);
    assert_eq!(replay.generation, 2);
    assert_eq!(file.stats().unwrap().0, 4);

    // A session without provenance never reuses a generation that has one.
    let no_provenance = file
        .write_session(
            session,
            1,
            &input("conversation_events", &changed),
            None,
            "{}",
        )
        .unwrap();
    assert_eq!(no_provenance.generation, 3);
    assert_eq!(no_provenance.provenance_chunk_id, None);
}

// ---------------------------------------------------------------------------
// The cold set
// ---------------------------------------------------------------------------

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn age_is_the_newest_event_not_updated_at_and_only_archived_or_deleted_qualify() {
    let fx = Fx::new();
    let cold_archived = fx.cold_session(SessionStatus::Archived, 60, 1);
    let cold_deleted = fx.cold_session(SessionStatus::Deleted, 90, 1);
    let _failed = fx.cold_session(SessionStatus::Failed, 60, 1);
    let recent = fx.session(SessionStatus::Archived, 60);
    fx.event(recent, 1, "System", None, "still talking", 1);
    let empty = fx.session(SessionStatus::Archived, 60);
    let _ = empty;

    let cutoff = Utc::now() - Duration::days(30);
    let page = candidate_page(&fx.read, None, 256, cutoff).unwrap();
    let ids: BTreeSet<String> = page
        .candidates
        .iter()
        .map(|candidate| candidate.session_id.clone())
        .collect();
    assert_eq!(
        ids,
        BTreeSet::from([cold_archived.to_string(), cold_deleted.to_string()])
    );
    assert_eq!(page.examined, 4, "the Failed session is not even read");
    assert!(page.next_after.is_none());
    for candidate in &page.candidates {
        assert!(candidate.last_event_at <= cutoff);
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn timestamps_with_offsets_and_legacy_forms_compare_as_instants() {
    let fx = Fx::new();
    // Cutoff: 2026-09-01T00:00:00Z.
    let cutoff = chrono::DateTime::parse_from_rfc3339("2026-09-01T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let mut expected = BTreeSet::new();
    for (created_at, before_cutoff) in [
        // 2026-08-31T20:00Z: before, though its text sorts after the cutoff's.
        ("2026-09-01T01:00:00+05:00", true),
        // 2026-09-01T04:00Z: after, though its text sorts before the cutoff's.
        ("2026-08-31T23:00:00-05:00", false),
        // Legacy comma fraction: 2026-08-02T23:15:11Z.
        ("2026-08-02T16:15:11,769711900-07:00", true),
        // Sub-second precision just after the cutoff.
        ("2026-09-01T00:00:00.5Z", false),
        ("2026-09-01T00:00:00Z", true),
    ] {
        let sid = fx.session(SessionStatus::Archived, 90);
        fx.event_at(sid, 1, "System", None, "x", None, None, created_at);
        if before_cutoff {
            expected.insert(sid.to_string());
        }
    }
    let page = candidate_page(&fx.read, None, 256, cutoff).unwrap();
    let ids: BTreeSet<String> = page
        .candidates
        .iter()
        .map(|candidate| candidate.session_id.clone())
        .collect();
    assert_eq!(ids, expected);
    assert_eq!(page.unreadable_timestamp, 0);

    let bad = fx.session(SessionStatus::Archived, 90);
    fx.event_at(bad, 1, "System", None, "x", None, None, "not a time");
    let page = candidate_page(&fx.read, None, 256, cutoff).unwrap();
    assert_eq!(
        page.unreadable_timestamp, 1,
        "an unreadable stamp is skipped, counted"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn candidates_are_walked_by_primary_key_in_pages() {
    let fx = Fx::new();
    let mut all = BTreeSet::new();
    for _ in 0..5 {
        let sid = fx.session(SessionStatus::Archived, 90);
        fx.event(sid, 1, "System", None, "x", 80);
        all.insert(sid.to_string());
    }
    let cutoff = Utc::now() - Duration::days(30);
    let mut seen = Vec::new();
    let mut after: Option<String> = None;
    let mut pages = 0;
    loop {
        let page = candidate_page(&fx.read, after.as_deref(), 2, cutoff).unwrap();
        pages += 1;
        seen.extend(page.candidates.iter().map(|c| c.session_id.clone()));
        match page.next_after {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    assert_eq!(pages, 3);
    assert_eq!(seen.iter().cloned().collect::<BTreeSet<_>>(), all);
    let mut sorted = seen.clone();
    sorted.sort();
    assert_eq!(seen, sorted, "pages walk ascending ids");
}

fn hold_context() -> HoldContext {
    HoldContext { now: Utc::now() }
}

fn held(fx: &Fx, session_id: &str) -> Option<&'static str> {
    session_hold(&fx.read, session_id, hold_context()).unwrap()
}

/// The column of each classified table that names the session in the hold.
fn naming_column(table: &str) -> &'static str {
    match table {
        "agent_jobs" => "owner_session_id",
        "rolling_queue_entries" => "source_session_id",
        "topology_executions" => "parent_session_id",
        "manager_nodes" => "seat_root_session_id",
        "global_manager_grants" => "seat_session_id",
        "agent_successor_reservations" => "predecessor_session_id",
        "manager_root_successions" => "manager_session_id",
        "manager_review_assignments" => "reviewer_session_id",
        "harness_manager_v2_operations" | "agent_messages" => "target_session_id",
        "agent_spawn_requests" => "child_session_id",
        "daemon_restart_intents"
        | "provider_turn_custody"
        | "source_worktree_settlement_items"
        | "archive_cleanup_runs" => "session_id",
        "agent_child_watch_witness" | "sandbox_custody_roots" => "owner_session_id",
        "execution_origin_claims" => "claimant_session_id",
        other => panic!("no naming column for {other}"),
    }
}

/// One row that references `session_id` and is in `state`, for a state class.
fn reference_row(conn: &Connection, class: &StateClass, session_id: &str, state: &str) {
    if class.table == "closure_sources" {
        let source = format!("closure-{}", Uuid::new_v4());
        force_insert(
            conn,
            "closure_sources",
            &[("id", text(&source)), ("state", text(state))],
        );
        force_insert(
            conn,
            "closure_source_sessions",
            &[
                ("source_id", text(&source)),
                ("session_id", text(session_id)),
            ],
        );
        return;
    }
    force_insert(
        conn,
        class.table,
        &[
            (naming_column(class.table), text(session_id)),
            (class.column, text(state)),
        ],
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn hold_tables_agree_with_each_other() {
    assert_eq!(HOLD_CODES.len(), HOLDS.len() + 3);
    for (index, (code, _)) in HOLDS.iter().enumerate() {
        assert_eq!(HOLD_CODES[index], *code, "hold {index}");
    }
    assert_eq!(
        HOLD_CODES[HOLDS.len()..],
        [
            "pending_question",
            "usage_limit_window",
            "recently_restored"
        ]
    );
    let distinct: BTreeSet<&str> = HOLD_CODES.iter().copied().collect();
    assert_eq!(distinct.len(), HOLD_CODES.len());
    for class in &HOLD_STATE_CLASSES {
        assert!(
            HOLDS.iter().any(|(code, _)| *code == class.hold),
            "{} is a hold",
            class.hold
        );
        assert!(!class.live.is_empty() && !class.settled.is_empty());
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn cold_archive_hold_state_coverage() {
    let fx = Fx::new();
    check_hold_classification(&fx.read).unwrap();
    assert_eq!(
        session_status_names().len(),
        8,
        "SessionStatus has eight known variants"
    );

    // A migration that adds a state nobody classified: the check fails, naming it,
    // and a pass refuses to run before it archives anything.
    let drifted = Fx::new();
    drifted.cold_session(SessionStatus::Archived, 60, 1);
    drifted
        .store
        .conn
        .execute_batch("PRAGMA writable_schema = ON")
        .unwrap();
    drifted
        .store
        .conn
        .execute(
            "UPDATE sqlite_master SET sql = replace(sql, '''lost''', '''lost'', ''mystery''')
             WHERE type = 'table' AND name = 'agent_jobs'",
            [],
        )
        .unwrap();
    drifted
        .store
        .conn
        .execute_batch("PRAGMA writable_schema = OFF")
        .unwrap();
    let error = check_hold_classification(&drifted.store.conn).unwrap_err();
    assert_eq!(error.code, "cold_archive_hold_schema_unclassified");
    assert!(error.detail.contains("agent_jobs"), "{error}");
    assert!(error.detail.contains("mystery"), "{error}");
    let error = drifted.stage(30).unwrap_err();
    assert_eq!(error.code, "cold_archive_hold_schema_unclassified");
    assert_eq!(
        drifted.count("SELECT COUNT(*) FROM cold_archive_sessions", None),
        0
    );
    assert_eq!(drifted.archive_chunk_count(), 0);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn every_state_class_holds_live_and_unknown_values_and_releases_settled_ones() {
    let fx = Fx::new();
    for class in HOLD_STATE_CLASSES
        .iter()
        .filter(|class| class.table != "sessions")
    {
        for state in class.live {
            let sid = fx.session(SessionStatus::Archived, 60).to_string();
            assert_eq!(held(&fx, &sid), None);
            reference_row(&fx.store.conn, class, &sid, state);
            assert_eq!(
                held(&fx, &sid),
                Some(class.hold),
                "{}.{} = {state} is live and keeps the session",
                class.table,
                class.column
            );
        }
        for state in class.settled {
            let sid = fx.session(SessionStatus::Archived, 60).to_string();
            reference_row(&fx.store.conn, class, &sid, state);
            assert_eq!(
                held(&fx, &sid),
                None,
                "{}.{} = {state} is settled and lets the session go",
                class.table,
                class.column
            );
        }
        let sid = fx.session(SessionStatus::Archived, 60).to_string();
        reference_row(&fx.store.conn, class, &sid, "never_classified_state");
        assert_eq!(
            held(&fx, &sid),
            Some(class.hold),
            "{}.{}: an unknown state holds the session",
            class.table,
            class.column
        );
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn rotation_successor_status_holds_unless_settled() {
    let fx = Fx::new();
    let class = HOLD_STATE_CLASSES
        .iter()
        .find(|class| class.table == "sessions")
        .unwrap();
    let mut planned = Vec::new();
    let statuses: Vec<&str> = session_status_names()
        .into_iter()
        .chain(["Mystery"])
        .collect();
    for status in &statuses {
        let parent = fx.session(SessionStatus::Archived, 60).to_string();
        let child = fx.session(SessionStatus::Running, 1).to_string();
        planned.push((parent, child, *status));
    }
    drop_triggers(&fx.store.conn, "sessions");
    for (parent, child, status) in &planned {
        fx.store
            .conn
            .execute(
                "UPDATE sessions SET continued_from = ?1, status = ?2 WHERE id = ?3",
                params![parent, status, child],
            )
            .unwrap();
    }
    for (parent, _, status) in &planned {
        let expected = if class.settled.contains(status) {
            None
        } else {
            Some("lineage")
        };
        assert_eq!(
            held(&fx, parent),
            expected,
            "a rotation successor in {status} state"
        );
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn presence_holds_keep_a_session_for_as_long_as_the_row_exists() {
    let fx = Fx::new();
    let cases: [(&str, &str, &str); 7] = [
        (
            "manager_seat",
            "harness_manager_scopes",
            "manager_session_id",
        ),
        ("manager_seat", "global_manager_appointments", "session_id"),
        (
            "manager_seat",
            "manager_portfolio_appointments",
            "session_id",
        ),
        ("watch", "harness_manager_watches", "source_session_id"),
        ("watch", "harness_manager_watches", "target_session_id"),
        ("tracker_dispatch", "issue_tracker_dispatches", "session_id"),
        (
            "spawn_restart_watch",
            "agent_child_relaunch_intents",
            "tip_session_id",
        ),
    ];
    for (code, table, column) in cases {
        let sid = fx.session(SessionStatus::Archived, 60).to_string();
        assert_eq!(held(&fx, &sid), None);
        force_insert(&fx.store.conn, table, &[(column, text(&sid))]);
        assert_eq!(held(&fx, &sid), Some(code), "{table}.{column}");
    }
    // Rows that name a terminal outcome release the session.
    let done = fx.session(SessionStatus::Archived, 60).to_string();
    force_insert(
        &fx.store.conn,
        "issue_tracker_dispatches",
        &[
            ("session_id", text(&done)),
            ("terminal_state", text("done")),
        ],
    );
    force_insert(
        &fx.store.conn,
        "agent_child_relaunch_intents",
        &[
            ("tip_session_id", text(&done)),
            ("settled_at", text(&ts(1))),
        ],
    );
    assert_eq!(held(&fx, &done), None);

    let asking = fx.session(SessionStatus::Archived, 60).to_string();
    drop_triggers(&fx.store.conn, "sessions");
    fx.store
        .conn
        .execute(
            "UPDATE sessions SET pending_question_json = '{\"questions\":[]}' WHERE id = ?1",
            [&asking],
        )
        .unwrap();
    assert_eq!(held(&fx, &asking), Some("pending_question"));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn wake_and_queue_references_by_either_column_and_enabled_flag() {
    let fx = Fx::new();
    let by_job = |enabled: i64, set_column: &str| {
        let sid = fx.session(SessionStatus::Archived, 60).to_string();
        let value = if set_column == "wake_mode" {
            format!("on_terminal:{sid}")
        } else {
            sid.clone()
        };
        force_insert(
            &fx.store.conn,
            "scheduled_jobs",
            &[
                ("enabled", Value::Integer(enabled)),
                (set_column, text(&value)),
            ],
        );
        held(&fx, &sid)
    };
    assert_eq!(by_job(1, "wake_session_id"), Some("wake"));
    assert_eq!(by_job(1, "wake_mode"), Some("wake"));
    assert_eq!(
        by_job(2, "wake_session_id"),
        Some("wake"),
        "any nonzero flag is enabled"
    );
    assert_eq!(
        by_job(0, "wake_session_id"),
        None,
        "a disabled wake releases it"
    );
    assert_eq!(by_job(0, "wake_mode"), None);

    let queued = fx.session(SessionStatus::Archived, 60).to_string();
    force_insert(
        &fx.store.conn,
        "rolling_queue_entries",
        &[
            ("wake_session_id", text(&queued)),
            ("state", text("gating")),
        ],
    );
    assert_eq!(held(&fx, &queued), Some("queue_entry"));

    let attempt = fx.session(SessionStatus::Archived, 60).to_string();
    force_insert(
        &fx.store.conn,
        "topology_executions",
        &[("id", text("exec-attempt")), ("status", text("blocked"))],
    );
    force_insert(
        &fx.store.conn,
        "topology_node_attempts",
        &[
            ("session_id", text(&attempt)),
            ("execution_id", text("exec-attempt")),
        ],
    );
    assert_eq!(held(&fx, &attempt), Some("topology"));
    let finished = fx.session(SessionStatus::Archived, 60).to_string();
    force_insert(
        &fx.store.conn,
        "topology_executions",
        &[("id", text("exec-finished")), ("status", text("succeeded"))],
    );
    force_insert(
        &fx.store.conn,
        "topology_node_attempts",
        &[
            ("session_id", text(&finished)),
            ("execution_id", text("exec-finished")),
        ],
    );
    assert_eq!(held(&fx, &finished), None);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_descendant_of_a_wake_target_is_held_and_a_runaway_lineage_holds() {
    let fx = Fx::new();
    let chain: Vec<String> = (0..4)
        .map(|_| fx.session(SessionStatus::Archived, 60).to_string())
        .collect();
    let long: Vec<String> = (0..66)
        .map(|_| fx.session(SessionStatus::Archived, 60).to_string())
        .collect();
    drop_triggers(&fx.store.conn, "sessions");
    for pair in chain.windows(2).chain(long.windows(2)) {
        fx.store
            .conn
            .execute(
                "UPDATE sessions SET continued_from = ?1 WHERE id = ?2",
                params![pair[0], pair[1]],
            )
            .unwrap();
    }
    for session in &chain {
        assert_eq!(held(&fx, session), None, "no wake yet");
    }
    force_insert(
        &fx.store.conn,
        "scheduled_jobs",
        &[
            ("enabled", Value::Integer(1)),
            ("wake_session_id", text(&chain[0])),
        ],
    );
    assert_eq!(held(&fx, &chain[0]), Some("wake"));
    for descendant in &chain[1..] {
        assert_eq!(
            held(&fx, descendant),
            Some("wake_lineage"),
            "a delivery to the target resolves to its lineage tip"
        );
    }
    fx.store
        .conn
        .execute("UPDATE scheduled_jobs SET enabled = 0", [])
        .unwrap();
    for session in &chain {
        assert_eq!(
            held(&fx, session),
            None,
            "the disabled wake releases the lineage"
        );
    }

    // A chain longer than the depth cap holds past the cap, wake or no wake.
    assert_eq!(held(&fx, &long[10]), None);
    assert_eq!(held(&fx, &long[65]), Some("wake_lineage"));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_container_lead_is_held_whatever_the_container_status() {
    let fx = Fx::new();
    let lead = fx.session(SessionStatus::Archived, 60).to_string();
    let other = fx.session(SessionStatus::Archived, 60).to_string();
    let epic = fx.session(SessionStatus::Archived, 60).to_string();
    drop_triggers(&fx.store.conn, "sessions");
    fx.store
        .conn
        .execute(
            "UPDATE sessions SET lead_session_id = ?1 WHERE id = ?2",
            params![lead, epic],
        )
        .unwrap();
    assert_eq!(held(&fx, &lead), Some("container_lead"));
    assert_eq!(held(&fx, &other), None);
}

fn rotation_event(
    fx: &Fx,
    session: &str,
    rotation: &str,
    phase: &str,
    event_type: &str,
    metadata: Option<&str>,
) {
    force_insert(
        &fx.store.conn,
        "rotation_events",
        &[
            ("session_id", text(session)),
            ("rotation_id", text(rotation)),
            ("phase", text(phase)),
            ("event_type", text(event_type)),
            (
                "metadata",
                metadata.map_or(Value::Null, |value| text(value)),
            ),
            ("created_at", text(&ts(0))),
        ],
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn an_open_rotation_intent_holds_its_predecessor_and_reserved_successor() {
    let fx = Fx::new();
    let predecessor = fx.session(SessionStatus::Archived, 60).to_string();
    let successor = fx.session(SessionStatus::Archived, 60).to_string();
    let bystander = fx.session(SessionStatus::Archived, 60).to_string();
    drop_triggers(&fx.store.conn, "sessions");
    fx.store
        .conn
        .execute(
            "UPDATE sessions SET continued_from = ?1, status = 'Completed' WHERE id = ?2",
            params![predecessor, successor],
        )
        .unwrap();
    assert_eq!(held(&fx, &predecessor), None);

    // Entered, not decided: open.
    rotation_event(
        &fx,
        &predecessor,
        "rot-1",
        "writing_handoff",
        "entered",
        None,
    );
    assert_eq!(held(&fx, &predecessor), Some("open_rotation_intent"));
    assert_eq!(
        held(&fx, &successor),
        None,
        "no reserved successor is named yet (and its own status settles the lineage hold)"
    );
    rotation_event(
        &fx,
        &predecessor,
        "rot-1",
        "writing_handoff",
        "successor_reserved",
        Some(&format!("{{\"successor_id\":\"{successor}\"}}")),
    );
    assert_eq!(held(&fx, &predecessor), Some("open_rotation_intent"));
    assert_eq!(held(&fx, &successor), Some("open_rotation_intent"));
    assert_eq!(held(&fx, &bystander), None);

    // Each closing event settles the intent.
    for closing in [
        "completed",
        "suppressed_final_handoff",
        "aborted_empty_session",
        "refused:budget",
    ] {
        rotation_event(&fx, &predecessor, "rot-1", "done", closing, None);
        assert_eq!(held(&fx, &predecessor), None, "{closing} closes the intent");
        assert_eq!(
            held(&fx, &successor),
            None,
            "{closing} releases the successor"
        );
        fx.store
            .conn
            .execute(
                "DELETE FROM rotation_events WHERE session_id = ?1 AND event_type = ?2",
                params![predecessor, closing],
            )
            .unwrap();
        assert_eq!(held(&fx, &predecessor), Some("open_rotation_intent"));
    }
    rotation_event(
        &fx,
        &predecessor,
        "rot-1",
        "depth_limit_hit",
        "stopped",
        None,
    );
    assert_eq!(
        held(&fx, &predecessor),
        None,
        "a depth-limit stop closes it"
    );

    // Only the session's latest rotation counts: a newer, closed rotation hides an
    // older entered one.
    rotation_event(&fx, &bystander, "old", "pending_interrupt", "entered", None);
    assert_eq!(held(&fx, &bystander), Some("open_rotation_intent"));
    rotation_event(&fx, &bystander, "new", "done", "completed", None);
    assert_eq!(held(&fx, &bystander), None);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn the_usage_limit_window_holds_recent_codex_sessions() {
    use crate::store_support::provider_defaults::CODEX_USAGE_LIMIT_STOP_REASON;
    let fx = Fx::new();
    let cases = [
        ("recent codex", "Codex", 1, true),
        ("recent app-server", "CodexAppServer", 2, true),
        ("recent claude", "Claude", 1, false),
        ("old codex", "Codex", 30, false),
    ];
    let mut planned = Vec::new();
    for (label, provider, age, expected) in cases {
        planned.push((
            fx.session(SessionStatus::Archived, 60).to_string(),
            label,
            provider,
            age,
            expected,
        ));
    }
    let wrong_reason = fx.session(SessionStatus::Archived, 60).to_string();
    drop_triggers(&fx.store.conn, "sessions");
    for (sid, _, provider, age, _) in &planned {
        fx.store
            .conn
            .execute(
                "UPDATE sessions SET provider = ?2, stop_reason = ?3, updated_at = ?4 WHERE id = ?1",
                params![sid, provider, CODEX_USAGE_LIMIT_STOP_REASON, ts(*age)],
            )
            .unwrap();
    }
    fx.store
        .conn
        .execute(
            "UPDATE sessions SET provider = 'Codex', stop_reason = 'completed', updated_at = ?2 WHERE id = ?1",
            params![wrong_reason, ts(1)],
        )
        .unwrap();
    for (sid, label, _, _, expected) in &planned {
        assert_eq!(
            held(&fx, sid),
            expected.then_some("usage_limit_window"),
            "{label}"
        );
    }
    assert_eq!(held(&fx, &wrong_reason), None, "another stop reason");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_recent_restore_holds_the_session_and_an_old_one_does_not() {
    let fx = Fx::new();
    let recent = fx.cold_session(SessionStatus::Archived, 60, 1);
    let old = fx.cold_session(SessionStatus::Archived, 60, 1);
    let cutoff = Utc::now() - Duration::days(14);
    assert!(!recently_restored(&fx.read, &recent.to_string(), cutoff).unwrap());
    index_row(&fx.store, recent, "restored", Some(&ts(2)));
    index_row(&fx.store, old, "restored", Some(&ts(40)));
    assert!(recently_restored(&fx.read, &recent.to_string(), cutoff).unwrap());
    assert!(!recently_restored(&fx.read, &old.to_string(), cutoff).unwrap());

    let report = fx.stage(14).unwrap();
    assert_eq!(report.holds.get("recently_restored"), Some(&1));
    assert_eq!(
        fx.row(recent).unwrap().state,
        "restored",
        "the held session is left exactly as it was"
    );
    assert_eq!(report.sessions_staged, 1);
    assert_eq!(
        fx.row(old).unwrap().state,
        "staged",
        "an old restore is archived again"
    );
}

// ---------------------------------------------------------------------------
// The live tail
// ---------------------------------------------------------------------------

fn movable_sequences(fx: &Fx, session_id: Uuid) -> Vec<i64> {
    fx.live_rows(MOVABLE_EVENTS_SQL, session_id, 11)
        .iter()
        .map(|row| match &row[2] {
            Value::Integer(sequence) => *sequence,
            other => panic!("sequence is {other:?}"),
        })
        .collect()
}

/// An event with an explicit instant, in a session created long ago.
fn tail_event(
    fx: &Fx,
    sid: Uuid,
    sequence: i32,
    event_type: &str,
    role: Option<&str>,
    created_at: &str,
) -> i64 {
    fx.event_at(
        sid, sequence, event_type, role, "body", None, None, created_at,
    )
}

fn stamp(second: u32) -> String {
    format!("2026-01-01T00:00:{second:02}.000000000Z")
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn only_tool_rows_move_and_every_other_row_the_newest_rows_and_referenced_rows_stay() {
    let fx = Fx::new();
    let sid = fx.session(SessionStatus::Archived, 60);
    tail_event(&fx, sid, 1, "Message", Some("User"), &stamp(1));
    tail_event(&fx, sid, 2, "System", None, &stamp(2));
    let first_tool_use = tail_event(&fx, sid, 3, "ToolUse", Some("Assistant"), &stamp(3));
    tail_event(&fx, sid, 4, "ToolResult", None, &stamp(4));
    tail_event(&fx, sid, 5, "Thinking", None, &stamp(5));
    tail_event(&fx, sid, 6, "Plan", None, &stamp(6));
    tail_event(&fx, sid, 7, "Compressed", None, &stamp(7));
    // The newest provider row (role Assistant): stays (T3).
    tail_event(&fx, sid, 8, "ToolUse", Some("Assistant"), &stamp(8));
    // Referenced by a pending-question publication: stays (T4).
    let referenced = tail_event(&fx, sid, 9, "ToolResult", None, &stamp(9));
    // The newest non-System row by TEXT created_at: stays (T6).
    tail_event(&fx, sid, 10, "ToolResult", None, &stamp(10));
    // The newest row overall and the TEXT-max row: stays (T2, T5).
    tail_event(&fx, sid, 11, "System", None, &stamp(11));
    fx.provenance(sid, first_tool_use, "provider_other");
    force_insert(
        &fx.store.conn,
        "pending_question_publications",
        &[
            ("session_id", text(&sid.to_string())),
            ("conversation_event_id", Value::Integer(referenced)),
        ],
    );
    assert_eq!(
        movable_sequences(&fx, sid),
        vec![3, 4],
        "T1 only tool rows move; T2 newest, T3 newest provider, T4 referenced and T6 witness rows stay"
    );
    let provenance_rows = fx.live_rows(MOVABLE_PROVENANCE_SQL, sid, PROVENANCE_COLUMNS.len());
    assert_eq!(provenance_rows.len(), 1);
    assert_eq!(provenance_rows[0][0], Value::Integer(first_tool_use));
    let (count, min, max): (i64, Option<i64>, Option<i64>) = fx
        .read
        .query_row(MOVABLE_COUNT_SQL, [sid.to_string()], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .unwrap();
    assert_eq!((count, min, max), (2, Some(3), Some(4)));

    // A closure-evidence reference keeps its row live too (T4).
    force_insert(
        &fx.store.conn,
        "closure_evidence",
        &[("review_handoff_event_id", Value::Integer(first_tool_use))],
    );
    assert_eq!(movable_sequences(&fx, sid), vec![4]);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn the_text_max_created_at_rows_stay_with_ties_to_the_newest_row() {
    let fx = Fx::new();
    // T5: the row that attains TEXT MAX(created_at) (a +10:00 stamp sorts after the Z
    // stamps although its instant is the earliest) stays.
    let offset = "2026-01-01T10:00:00.000000000+10:00";
    let attains_max = fx.session(SessionStatus::Archived, 60);
    tail_event(&fx, attains_max, 1, "Message", Some("User"), &stamp(1));
    tail_event(&fx, attains_max, 2, "ToolResult", None, &stamp(2));
    tail_event(&fx, attains_max, 3, "ToolResult", None, offset);
    tail_event(&fx, attains_max, 4, "ToolResult", None, &stamp(4));
    tail_event(&fx, attains_max, 5, "Message", Some("Assistant"), &stamp(5));
    assert_eq!(movable_sequences(&fx, attains_max), vec![2, 4]);

    // A tie goes to the newest (sequence, id): row 4 stays, row 3 moves.
    let tie = fx.session(SessionStatus::Archived, 60);
    tail_event(&fx, tie, 1, "Message", Some("User"), &stamp(1));
    tail_event(&fx, tie, 2, "ToolResult", None, &stamp(2));
    tail_event(&fx, tie, 3, "ToolResult", None, offset);
    tail_event(&fx, tie, 4, "ToolResult", None, offset);
    tail_event(&fx, tie, 5, "Message", Some("Assistant"), &stamp(5));
    assert_eq!(movable_sequences(&fx, tie), vec![2, 3]);

    // Variable precision: "...01Z" sorts after "...01.999999Z" ('Z' > '.'), so the
    // whole-second stamp is the TEXT max though its instant is earlier.
    let precision = fx.session(SessionStatus::Archived, 60);
    tail_event(
        &fx,
        precision,
        1,
        "Message",
        Some("User"),
        "2026-01-01T00:00:00.5Z",
    );
    tail_event(
        &fx,
        precision,
        2,
        "ToolResult",
        None,
        "2026-01-01T00:00:01.999999Z",
    );
    tail_event(
        &fx,
        precision,
        3,
        "ToolResult",
        None,
        "2026-01-01T00:00:01Z",
    );
    tail_event(
        &fx,
        precision,
        4,
        "Message",
        Some("Assistant"),
        "2026-01-01T00:00:00.5Z",
    );
    assert_eq!(movable_sequences(&fx, precision), vec![2]);

    // T6: a System row holds the overall TEXT max; the newest non-System row by
    // TEXT stays as well.
    let system_max = fx.session(SessionStatus::Archived, 60);
    tail_event(&fx, system_max, 1, "Message", Some("User"), &stamp(1));
    tail_event(&fx, system_max, 2, "ToolResult", None, &stamp(2));
    tail_event(&fx, system_max, 3, "ToolResult", None, &stamp(3));
    tail_event(
        &fx,
        system_max,
        4,
        "System",
        None,
        "2026-01-01T09:00:00.000000000Z",
    );
    tail_event(&fx, system_max, 5, "Message", Some("Assistant"), &stamp(0));
    assert_eq!(movable_sequences(&fx, system_max), vec![2]);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_session_with_only_stay_rows_or_one_tool_row_has_nothing_to_move() {
    let fx = Fx::new();
    let messages = fx.session(SessionStatus::Archived, 60);
    fx.event(messages, 1, "Message", Some("User"), "a", 61);
    fx.event(messages, 2, "System", None, "b", 61);
    fx.event(messages, 3, "Thinking", None, "c", 61);
    fx.event(messages, 4, "Message", Some("Assistant"), "d", 61);
    let single = fx.session(SessionStatus::Archived, 60);
    fx.event(single, 1, "ToolResult", None, "lonely", 61);
    assert!(movable_sequences(&fx, messages).is_empty());
    assert!(
        movable_sequences(&fx, single).is_empty(),
        "the only row is the newest row"
    );
    let report = fx.stage(30).unwrap();
    assert_eq!(report.sessions_empty, 2);
    assert_eq!(report.sessions_staged, 0);
    assert!(fx.row(messages).is_none() && fx.row(single).is_none());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn every_foreign_key_into_conversation_events_is_in_the_tail_rule() {
    let fx = Fx::new();
    let found: BTreeSet<(String, String)> = fx
        .store
        .conn
        .prepare(
            "SELECT m.name, f.\"from\" FROM sqlite_master m, pragma_foreign_key_list(m.name) f
             WHERE m.type = 'table' AND f.\"table\" = 'conversation_events'",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let mut expected: BTreeSet<(String, String)> = EVENT_FK_REFERENCES
        .iter()
        .map(|(table, column)| ((*table).to_string(), (*column).to_string()))
        .collect();
    expected.insert((
        "conversation_event_provenance".to_string(),
        "conversation_event_id".to_string(),
    ));
    assert_eq!(
        found, expected,
        "a new reference to conversation_events(id) must be added to EVENT_FK_REFERENCES \
         (and to the tail rule) before rows can move"
    );
    ensure_expected_columns(&fx.read).unwrap();
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn the_three_movable_statements_share_one_filter() {
    let filter_of = |sql: &str| -> String {
        let start = sql.find("WHERE e.session_id = ?1").unwrap() + "WHERE ".len();
        let rest = &sql[start..];
        let end = rest.find("\n ORDER BY").unwrap_or(rest.len());
        rest[..end].trim_end().to_string()
    };
    let events = filter_of(MOVABLE_EVENTS_SQL);
    let count = filter_of(MOVABLE_COUNT_SQL);
    let provenance = filter_of(MOVABLE_PROVENANCE_SQL);
    assert_eq!(events, count);
    assert_eq!(provenance.strip_suffix(')').unwrap(), events);
    // The production move (slice S4) deletes exactly the rows the filter names.
    for delete in [
        super::mover::DELETE_MOVABLE_EVENTS_SQL,
        super::mover::DELETE_MOVABLE_PROVENANCE_SQL,
    ] {
        assert_eq!(
            filter_of(delete).strip_suffix(')').unwrap(),
            events,
            "{delete}"
        );
    }
}

// ---------------------------------------------------------------------------
// Stage and verify passes
// ---------------------------------------------------------------------------

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_stage_pass_archives_and_verifies_the_cold_set_and_deletes_nothing() {
    let fx = Fx::new();
    let first = fx.cold_session(SessionStatus::Archived, 60, 3);
    let second = fx.cold_session(SessionStatus::Deleted, 120, 2);
    let _recent = fx.cold_session(SessionStatus::Archived, 1, 2);
    let _failed = fx.cold_session(SessionStatus::Failed, 60, 2);
    let held = fx.cold_session(SessionStatus::Archived, 60, 2);
    drop_triggers(&fx.store.conn, "sessions");
    fx.store
        .conn
        .execute(
            "UPDATE sessions SET pending_question_json = '{\"questions\":[]}' WHERE id = ?1",
            [held.to_string()],
        )
        .unwrap();
    let before = fx.live_counts();
    let run_id = fx
        .store
        .cold_begin_run("scheduled", ColdMode::Stage)
        .unwrap();
    let settings = fx.settings(30);
    let report = run_stage_pass(
        &DirectStoreAccess(&fx.store),
        &fx.read,
        &PassParams {
            run_id,
            settings: &settings,
            limits: fast_limits(),
            batch_gate: None,
            cancel: None,
        },
    )
    .unwrap();

    assert_eq!(
        report.sessions_examined, 4,
        "first, second, recent and held qualify by status"
    );
    assert_eq!(report.sessions_staged, 2);
    assert_eq!(report.sessions_failed, 0);
    assert_eq!(report.verify_errors, 0);
    assert_eq!(report.holds.get("pending_question"), Some(&1));
    assert!(report.raw_bytes > 0 && report.compressed_bytes > 0);
    assert!(
        report.compressed_bytes < report.raw_bytes,
        "tool output compresses"
    );
    assert_eq!(
        report.chunks_written, 4,
        "an events and a provenance chunk per session"
    );
    assert_eq!(
        fx.live_counts(),
        before,
        "slice S1 deletes nothing from the live database"
    );

    for (sid, pairs) in [(first, 3_i64), (second, 2)] {
        let row = fx.row(sid).unwrap();
        assert_eq!(row.state, "staged");
        assert_eq!(row.generation, 1);
        assert!(row.events_chunk_id > 0);
        assert!(row.provenance_chunk_id.is_some());
        assert!(row.verified_at.is_some());
        assert!(row.last_error_code.is_none());
        let movable = 2 * pairs;
        assert_eq!(
            row.archived_event_count, movable,
            "only the {pairs} ToolUse/ToolResult pairs move"
        );
        assert_eq!(row.pre_event_count, 1 + 1 + 2 * pairs + 1 + 1);
        assert_eq!(
            row.archived_provenance_count,
            2 * pairs,
            "the provenance of the tool rows"
        );
        assert_eq!(
            row.pre_provenance_count,
            2 * pairs + 2,
            "plus the Thinking row's and the final message's, which stay"
        );
        assert!(row.compressed_bytes > 0 && row.compressed_bytes < row.raw_bytes);
        assert_eq!(row.archived_min_sequence, Some(3), "the first ToolUse");

        // The archive holds exactly the live rows that would move.
        let live_events = fx.live_rows(MOVABLE_EVENTS_SQL, sid, 11);
        assert_eq!(
            live_events.len(),
            usize::try_from(row.archived_event_count).unwrap()
        );
        assert_eq!(
            fx.archived_rows(row.events_chunk_id, row.file_id, 11),
            live_events
        );
        let live_provenance = fx.live_rows(MOVABLE_PROVENANCE_SQL, sid, PROVENANCE_COLUMNS.len());
        assert_eq!(
            fx.archived_rows(row.provenance_chunk_id.unwrap(), row.file_id, 5),
            live_provenance
        );
        let content_len: i64 = live_events
            .iter()
            .map(|row| sqlite_length_of_value(&row[5]))
            .sum();
        assert_eq!(content_len, row.archived_content_len);
    }
    assert!(fx.row(held).is_none(), "a held session is not staged");
    assert_eq!(
        fx.count("SELECT COUNT(*) FROM cold_archive_sessions", None),
        2
    );
    assert_eq!(fx.archive_chunk_count(), 4);
    for file in fx.store.cold_archive_files().unwrap() {
        assert!(Path::new(&file.path).is_file());
        assert!(file.bytes > 0 && file.chunk_count > 0);
        assert!(file.last_verified_at.is_some());
        assert!(file.last_verify_error.is_none());
    }

    // The run row and the status read agree with the report.
    fx.store
        .cold_finish_run(
            run_id,
            &report.run_summary(fx.store.cold_live_db_bytes().ok()),
        )
        .unwrap();
    let status = fx.store.cold_archive_status(&settings, false).unwrap();
    assert_eq!(status.totals.staged_sessions, 2);
    assert_eq!(status.totals.verified_sessions, 2);
    assert_eq!(status.totals.cold_sessions, 0);
    assert_eq!(status.totals.sessions_with_error, 0);
    assert_eq!(
        status.totals.archived_events,
        i64::try_from(report.events_staged).unwrap()
    );
    assert!(status.totals.compressed_bytes < status.totals.raw_bytes);
    assert!(!status.archives.is_empty());
    assert!(status.errors.is_empty(), "{:?}", status.errors);
    assert!(status.live_db.file_bytes.unwrap() > 0);
    let last_run = status.last_run.unwrap();
    assert_eq!(last_run.outcome, "completed");
    assert_eq!(last_run.run_id, run_id.to_string());
    assert_eq!(last_run.holds["pending_question"], 1);
    assert_eq!(last_run.sessions_staged, 2);
    assert_eq!(last_run.sessions_moved, 0);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_second_pass_over_unchanged_data_writes_no_chunk_and_changes_nothing() {
    let fx = Fx::new();
    let first = fx.cold_session(SessionStatus::Archived, 60, 2);
    let second = fx.cold_session(SessionStatus::Deleted, 80, 1);
    fx.stage(30).unwrap();
    let before_rows = (fx.row(first).unwrap(), fx.row(second).unwrap());
    let before_chunks = fx.archive_chunk_count();
    let before_live = fx.live_counts();

    let report = fx.stage(30).unwrap();
    assert_eq!(report.sessions_unchanged, 2);
    assert_eq!(report.sessions_staged, 0);
    assert_eq!(report.chunks_written, 0);
    assert_eq!(fx.archive_chunk_count(), before_chunks);
    assert_eq!(fx.live_counts(), before_live);
    assert_eq!(
        (fx.row(first).unwrap(), fx.row(second).unwrap()),
        before_rows
    );

    // A session whose verification is forgotten is re-checked against the same
    // chunk: nothing new is written.
    fx.store
        .conn
        .execute("UPDATE cold_archive_sessions SET verified_at = NULL", [])
        .unwrap();
    let report = fx.stage(30).unwrap();
    assert_eq!(report.sessions_staged, 2);
    assert_eq!(
        report.sessions_resumed, 2,
        "the existing chunks were reused"
    );
    assert_eq!(report.chunks_written, 0);
    assert_eq!(fx.archive_chunk_count(), before_chunks);
    assert!(fx.row(first).unwrap().verified_at.is_some());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_changed_session_is_staged_again_under_a_new_generation() {
    let fx = Fx::new();
    let sid = fx.cold_session(SessionStatus::Archived, 60, 1);
    fx.stage(30).unwrap();
    let first = fx.row(sid).unwrap();
    assert_eq!(first.generation, 1);

    // Two late rows with older instants: the ToolResult moves (it is neither the
    // newest row nor a TEXT-max witness), the System row is the new newest row.
    let next = i32::try_from(first.pre_event_count).unwrap() + 1;
    fx.event_at(
        sid,
        next,
        "ToolResult",
        None,
        "late output",
        None,
        None,
        &ts(70),
    );
    fx.event_at(
        sid,
        next + 1,
        "System",
        None,
        "late system",
        None,
        None,
        &ts(69),
    );
    let report = fx.stage(30).unwrap();
    assert_eq!(report.sessions_staged, 1);
    assert_eq!(report.sessions_unchanged, 0);
    let second = fx.row(sid).unwrap();
    assert_eq!(second.generation, 2);
    assert!(second.archived_event_count > first.archived_event_count);
    assert_ne!(second.events_sha256, first.events_sha256);
    assert_eq!(
        fx.archive_chunk_count(),
        4,
        "generation 1 stays; append-only"
    );

    let verify = fx.verify().unwrap();
    assert_eq!(verify.sessions_checked, 1);
    assert_eq!(verify.sessions_failed, 0);
    assert_eq!(
        verify.orphan_chunks, 2,
        "the superseded generation is reported, not deleted"
    );
    assert!(verify.orphan_bytes > 0);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_crash_at_every_step_loses_nothing_and_the_next_pass_resumes() {
    for point in failpoint::POINTS {
        let fx = Fx::new();
        let sid = fx.cold_session(SessionStatus::Archived, 60, 2);
        let before = fx.live_counts();

        failpoint::arm(point);
        let error = fx.stage(30).unwrap_err();
        failpoint::disarm();
        assert_eq!(error.code, "cold_archive_injected_failure", "{point}");
        assert_eq!(
            fx.live_counts(),
            before,
            "{point}: the live database is untouched"
        );

        let row = fx
            .row(sid)
            .unwrap_or_else(|| panic!("{point}: the stage is recorded"));
        assert_eq!(row.state, "staged", "{point}");
        assert!(row.verified_at.is_none(), "{point}: nothing is trusted yet");
        match point {
            "after_stage" => {
                assert_eq!(row.events_chunk_id, 0);
                assert_eq!(fx.archive_chunk_count(), 0, "nothing was written yet");
            }
            "write_before_commit" => {
                assert_eq!(row.events_chunk_id, 0);
                assert_eq!(
                    fx.archive_chunk_count(),
                    0,
                    "the partial archive transaction rolled back"
                );
            }
            "after_write" => {
                assert_eq!(row.events_chunk_id, 0, "the index was not told");
                assert_eq!(fx.archive_chunk_count(), 2, "the chunks are durable");
            }
            "after_verify" => {
                assert!(row.events_chunk_id > 0);
                assert_eq!(fx.archive_chunk_count(), 2);
            }
            other => panic!("unexpected failpoint {other}"),
        }

        let report = fx.stage(30).unwrap();
        assert_eq!(report.sessions_failed, 0, "{point}");
        assert_eq!(report.sessions_staged, 1, "{point}");
        let reused = matches!(point, "after_write" | "after_verify");
        assert_eq!(report.sessions_resumed, u64::from(reused), "{point}");
        assert_eq!(report.chunks_written, if reused { 0 } else { 2 }, "{point}");
        let row = fx.row(sid).unwrap();
        assert!(row.verified_at.is_some(), "{point}");
        assert!(row.last_error_code.is_none(), "{point}");
        assert!(row.events_chunk_id > 0, "{point}");
        assert_eq!(
            row.generation, 1,
            "{point}: a crash never costs a generation"
        );
        assert_eq!(fx.archive_chunk_count(), 2, "{point}: no duplicate chunks");
        assert_eq!(fx.live_counts(), before, "{point}");
        // The interrupted run is settled as failed when the next one starts.
        let interrupted: i64 = fx.count(
            "SELECT COUNT(*) FROM cold_archive_runs WHERE outcome = 'failed'
               AND error_code = 'cold_archive_interrupted'",
            None,
        );
        assert_eq!(interrupted, 1, "{point}");
        let verify = fx.verify().unwrap();
        assert_eq!(verify.sessions_failed, 0, "{point}");
        assert_eq!(verify.orphan_chunks, 0, "{point}");
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_held_session_stays_live_and_is_counted_under_its_reason() {
    let fx = Fx::new();
    let held = fx.cold_session(SessionStatus::Archived, 60, 1);
    let free = fx.cold_session(SessionStatus::Archived, 60, 1);
    force_insert(
        &fx.store.conn,
        "scheduled_jobs",
        &[
            ("enabled", Value::Integer(1)),
            ("wake_session_id", text(&held.to_string())),
        ],
    );
    let before = fx.live_counts();
    let report = fx.stage(30).unwrap();
    assert_eq!(report.holds.get("wake"), Some(&1));
    assert_eq!(report.sessions_staged, 1);
    assert!(
        fx.row(held).is_none(),
        "no index row, no chunk for a held session"
    );
    assert!(fx.row(free).is_some());
    assert_eq!(fx.live_counts(), before);
    assert_eq!(
        fx.count("SELECT COUNT(*) FROM cold_archive_sessions", None),
        1
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn sessions_another_actor_is_moving_are_skipped() {
    let fx = Fx::new();
    let busy = fx.cold_session(SessionStatus::Archived, 60, 1);
    let cold = fx.cold_session(SessionStatus::Archived, 60, 1);
    index_row(&fx.store, busy, "restoring", None);
    index_row(&fx.store, cold, "cold", None);
    let report = fx.stage(30).unwrap();
    assert_eq!(report.sessions_staged, 0);
    assert_eq!(
        report.sessions_examined, 0,
        "a session another actor is moving is not even a candidate"
    );
    assert_eq!(fx.row(busy).unwrap().state, "restoring");
    assert_eq!(fx.row(cold).unwrap().state, "cold");
    assert_eq!(fx.archive_chunk_count(), 0);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn verify_detects_a_corrupt_chunk_and_the_next_stage_pass_writes_a_new_generation() {
    let fx = Fx::new();
    let sid = fx.cold_session(SessionStatus::Archived, 60, 2);
    fx.stage(30).unwrap();
    let row = fx.row(sid).unwrap();
    let path = fx.archive_paths().remove(0);

    // Corrupt the events chunk's payload (the append-only trigger is dropped on a
    // private connection, as a bad disk or a careless edit would bypass it).
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("DROP TRIGGER chunks_append_only_update")
            .unwrap();
        conn.execute(
            "UPDATE chunks SET payload = zeroblob(length(payload)) WHERE chunk_id = ?1",
            [row.events_chunk_id],
        )
        .unwrap();
    }
    let report = fx.verify().unwrap();
    assert_eq!(report.sessions_checked, 1);
    assert_eq!(report.sessions_failed, 1);
    assert_eq!(report.errors[0].session_id, sid.to_string());
    assert!(
        report.errors[0].code.starts_with("cold_archive_verify"),
        "{:?}",
        report.errors
    );
    let failed = fx.row(sid).unwrap();
    assert!(failed.last_error_code.is_some());
    assert!(
        failed.verified_at.is_none(),
        "a failed chunk is no longer trusted"
    );
    let file = fx.store.cold_archive_files().unwrap().remove(0);
    assert!(file.last_verify_error.is_some());
    let status = fx
        .store
        .cold_archive_status(&fx.settings(30), false)
        .unwrap();
    assert!(!status.errors.is_empty());
    assert_eq!(status.totals.sessions_with_error, 1);
    assert_eq!(status.totals.verified_sessions, 0);

    // While the live rows exist, the next stage pass does not trust the chunk it
    // finds: it writes the session again under the next generation.
    let report = fx.stage(30).unwrap();
    assert_eq!(report.sessions_failed, 0);
    assert_eq!(report.sessions_staged, 1);
    let healed = fx.row(sid).unwrap();
    assert_eq!(healed.generation, 2);
    assert!(healed.verified_at.is_some());
    assert!(healed.last_error_code.is_none());
    assert_ne!(healed.events_chunk_id, row.events_chunk_id);
    let verify = fx.verify().unwrap();
    assert_eq!(verify.sessions_failed, 0);
    assert_eq!(
        verify.orphan_chunks, 2,
        "the corrupt generation stays, unreferenced"
    );
    assert!(
        fx.store.cold_archive_files().unwrap()[0]
            .last_verify_error
            .is_none()
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn verify_reports_a_missing_archive_file_per_session_and_per_file() {
    let fx = Fx::new();
    let sid = fx.cold_session(SessionStatus::Archived, 60, 2);
    fx.stage(30).unwrap();
    let path = fx.archive_paths().remove(0);
    let moved = path.with_extension("moved");
    std::fs::rename(&path, &moved).unwrap();
    let report = fx.verify().unwrap();
    assert_eq!(report.files_missing, 1);
    assert_eq!(report.sessions_failed, 1);
    assert_eq!(report.errors[0].code, "cold_archive_file_missing");
    assert_eq!(
        fx.row(sid).unwrap().last_error_code.as_deref(),
        Some("cold_archive_file_missing")
    );
    assert!(
        fx.store.cold_archive_files().unwrap()[0]
            .last_verify_error
            .is_some()
    );
    std::fs::rename(&moved, &path).unwrap();
    let report = fx.verify().unwrap();
    assert_eq!(report.sessions_failed, 0);
    assert_eq!(report.sessions_ok, 1);
    assert!(fx.row(sid).unwrap().last_error_code.is_none());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_stage_pass_through_the_shared_store_mutex_matches_the_direct_pass() {
    let fx = Fx::new();
    let sid = fx.cold_session(SessionStatus::Archived, 60, 2);
    let settings = fx.settings(30);
    let Fx {
        _dir,
        db: _,
        archive: _,
        store,
        read,
    } = fx;
    let run_id = store.cold_begin_run("scheduled", ColdMode::Stage).unwrap();
    let shared = std::sync::Arc::new(rsid_core::store_lock::Mutex::new(store));
    let report = run_stage_pass(
        &shared,
        &read,
        &PassParams {
            run_id,
            settings: &settings,
            limits: fast_limits(),
            batch_gate: None,
            cancel: None,
        },
    )
    .unwrap();
    assert_eq!(report.sessions_staged, 1);
    let guard = shared.blocking_lock();
    assert!(
        guard
            .cold_session_row(&sid.to_string())
            .unwrap()
            .unwrap()
            .verified_at
            .is_some()
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_closed_batch_gate_holds_the_pass_until_the_budget_runs_out() {
    let fx = Fx::new();
    fx.cold_session(SessionStatus::Archived, 60, 1);
    let settings = fx.settings(30);
    let run_id = fx
        .store
        .cold_begin_run("scheduled", ColdMode::Stage)
        .unwrap();
    let closed = || false;
    let report = run_stage_pass(
        &DirectStoreAccess(&fx.store),
        &fx.read,
        &PassParams {
            run_id,
            settings: &settings,
            limits: PassLimits {
                wall_budget: std::time::Duration::from_millis(20),
                gate_poll: std::time::Duration::from_millis(5),
                ..fast_limits()
            },
            batch_gate: Some(&closed),
            cancel: None,
        },
    )
    .unwrap();
    assert!(report.budget_exhausted);
    assert_eq!(report.sessions_staged, 0);
    assert_eq!(report.run_summary(None).outcome, "partial");
}

// ---------------------------------------------------------------------------
// The verify pass obeys the stage pass's throttle (#2437)
// ---------------------------------------------------------------------------

/// A verify pass over `sessions` staged sessions with `gate` and `limits`.
fn verify_with(
    fx: &Fx,
    limits: PassLimits,
    gate: Option<&dyn Fn() -> bool>,
) -> ColdResult<VerifyReport> {
    let settings = fx.settings(30);
    let run_id = fx
        .store
        .cold_begin_run("scheduled", ColdMode::Verify)
        .unwrap();
    run_verify_pass(
        &DirectStoreAccess(&fx.store),
        &PassParams {
            run_id,
            settings: &settings,
            limits,
            batch_gate: gate,
            cancel: None,
        },
    )
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_closed_batch_gate_holds_the_verify_pass_until_the_budget_runs_out() {
    let fx = Fx::new();
    fx.cold_session(SessionStatus::Archived, 60, 1);
    assert_eq!(fx.stage(30).unwrap().sessions_staged, 1);
    let closed = || false;
    let report = verify_with(
        &fx,
        PassLimits {
            wall_budget: std::time::Duration::from_millis(30),
            gate_poll: std::time::Duration::from_millis(5),
            ..fast_limits()
        },
        Some(&closed),
    )
    .unwrap();
    assert!(report.budget_exhausted, "a closed gate holds the pass");
    assert_eq!(
        report.sessions_checked, 0,
        "nothing is read while it is held"
    );
    assert_eq!(report.run_summary(None).outcome, "partial");

    // Open again, the same pass verifies the session.
    let open = || true;
    let report = verify_with(&fx, fast_limits(), Some(&open)).unwrap();
    assert!(!report.budget_exhausted);
    assert_eq!(report.sessions_checked, 1);
    assert_eq!(report.sessions_ok, 1);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn the_verify_pass_yields_between_batches_and_asks_the_gate_each_time() {
    let fx = Fx::new();
    for _ in 0..5 {
        fx.cold_session(SessionStatus::Archived, 60, 1);
    }
    assert_eq!(fx.stage(30).unwrap().sessions_staged, 5);
    let asked = std::cell::Cell::new(0u32);
    let counting = || {
        asked.set(asked.get() + 1);
        true
    };
    let sleep = std::time::Duration::from_millis(25);
    let begun = std::time::Instant::now();
    let report = verify_with(
        &fx,
        PassLimits {
            batch_max_sessions: 2,
            batch_sleep: sleep,
            ..fast_limits()
        },
        Some(&counting),
    )
    .unwrap();
    let elapsed = begun.elapsed();
    assert_eq!(report.sessions_checked, 5);
    assert_eq!(report.sessions_ok, 5);
    // 5 sessions at 2 per batch: two full batches and a trailing one.
    assert_eq!(report.batches, 3);
    // Once before the pass starts, then once after each full batch.
    assert_eq!(asked.get(), 3);
    assert!(
        elapsed >= sleep * 2,
        "two sleeps between batches, took {elapsed:?}"
    );

    // The raw-byte ceiling closes a batch too: every session is over 1 byte.
    asked.set(0);
    let report = verify_with(
        &fx,
        PassLimits {
            batch_max_raw_bytes: 1,
            ..fast_limits()
        },
        Some(&counting),
    )
    .unwrap();
    assert_eq!(report.batches, 5, "one batch per session");
    assert_eq!(asked.get(), 6, "the start plus one after each batch");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_gate_that_closes_mid_pass_stops_the_verify_pass_at_the_batch_boundary() {
    let fx = Fx::new();
    for _ in 0..4 {
        fx.cold_session(SessionStatus::Archived, 60, 1);
    }
    assert_eq!(fx.stage(30).unwrap().sessions_staged, 4);
    let asked = std::cell::Cell::new(0u32);
    // Open for the start and the first boundary, then closed for good.
    let closing = || {
        asked.set(asked.get() + 1);
        asked.get() <= 2
    };
    let report = verify_with(
        &fx,
        PassLimits {
            batch_max_sessions: 2,
            wall_budget: std::time::Duration::from_millis(200),
            gate_poll: std::time::Duration::from_millis(5),
            ..fast_limits()
        },
        Some(&closing),
    )
    .unwrap();
    assert!(report.budget_exhausted);
    assert_eq!(
        report.sessions_checked, 4,
        "the second batch ran before the gate closed at its boundary"
    );
}

// ---------------------------------------------------------------------------
// The restart-proof schedule (#2436)
// ---------------------------------------------------------------------------

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn the_last_stage_start_is_durable_across_a_store_restart() {
    let fx = Fx::new();
    assert_eq!(fx.store.cold_last_stage_started_at().unwrap(), None);

    // A verify run is not a stage run: it never moves the schedule.
    fx.store
        .cold_begin_run("run_now", ColdMode::Verify)
        .unwrap();
    assert_eq!(fx.store.cold_last_stage_started_at().unwrap(), None);

    let before = Utc::now();
    fx.store
        .cold_begin_run("scheduled", ColdMode::Stage)
        .unwrap();
    let recorded = fx
        .store
        .cold_last_stage_started_at()
        .unwrap()
        .expect("a stage run was recorded");
    assert!(recorded >= before - Duration::seconds(1) && recorded <= Utc::now());

    let Fx {
        _dir, db, store, ..
    } = fx;
    drop(store);
    let reopened = Store::open_test_fixture(&db).unwrap();
    assert_eq!(
        reopened.cold_last_stage_started_at().unwrap(),
        Some(recorded),
        "the new daemon incarnation reads the previous start from the database"
    );

    // A failed or interrupted run still started: it counts.
    let stale = reopened.cold_begin_run("run_now", ColdMode::Stage).unwrap();
    reopened.cold_fail_stale_runs(Uuid::new_v4()).unwrap();
    assert!(
        reopened
            .cold_last_stage_started_at()
            .unwrap()
            .is_some_and(|started| started >= recorded)
    );
    let _ = stale;
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_future_stamped_stage_run_does_not_outrank_a_later_start_2449() {
    let fx = Fx::new();
    // A run stamped by a clock that was later corrected: a day ahead.
    let future = Utc::now() + Duration::hours(24);
    fx.store
        .conn
        .execute(
            "INSERT INTO cold_archive_runs(run_id, trigger, mode, started_at, outcome)
             VALUES (?1, 'scheduled', 'stage', ?2, 'completed')",
            params![
                Uuid::new_v4().to_string(),
                future.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
            ],
        )
        .unwrap();
    assert!(
        fx.store
            .cold_last_stage_started_at()
            .unwrap()
            .is_some_and(|started| started > Utc::now() + Duration::hours(23)),
        "with only the future row, that row is the last start"
    );

    // The replacement pass starts at the corrected time and is now the last start.
    fx.store
        .cold_begin_run("scheduled", ColdMode::Stage)
        .unwrap();
    let replacement = fx
        .store
        .cold_last_stage_started_at()
        .unwrap()
        .expect("a stage run was recorded");
    assert!(
        replacement <= Utc::now() + Duration::seconds(1),
        "the replacement start outranks the older future-stamped row"
    );

    let Fx {
        _dir, db, store, ..
    } = fx;
    drop(store);
    let reopened = Store::open_test_fixture(&db).unwrap();
    assert_eq!(
        reopened.cold_last_stage_started_at().unwrap(),
        Some(replacement),
        "a new daemon incarnation reads the replacement start, not the future row"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn an_unparsable_stage_start_is_treated_as_never_started() {
    let fx = Fx::new();
    fx.store
        .conn
        .execute(
            "INSERT INTO cold_archive_runs(run_id, trigger, mode, started_at, outcome)
             VALUES (?1, 'scheduled', 'stage', 'not a time', 'completed')",
            [Uuid::new_v4().to_string()],
        )
        .unwrap();
    assert_eq!(fx.store.cold_last_stage_started_at().unwrap(), None);
}

// ---------------------------------------------------------------------------
// Archive-directory containment (#2435)
// ---------------------------------------------------------------------------

/// `<root>/sandboxes/s1` (a sandbox root; `<root>/sandboxes` is the forbidden
/// root) and `<root>/outside` (an allowed place), under a canonical temp root.
struct Tree {
    _dir: tempfile::TempDir,
    root: PathBuf,
    sandboxes: PathBuf,
    outside: PathBuf,
}

fn tree() -> Tree {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let sandboxes = root.join("sandboxes");
    let outside = root.join("outside");
    std::fs::create_dir_all(sandboxes.join("s1")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    Tree {
        _dir: dir,
        root,
        sandboxes,
        outside,
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn archive_dir_setting_refuses_a_sandbox_path_however_it_is_spelled() {
    let t = tree();
    let roots = vec![t.sandboxes.clone()];
    let check = |raw: &str| validate_archive_dir_setting_with(raw, &roots);
    let text = |path: &Path| path.display().to_string();

    // An ordinary place, existing or not, is accepted as typed.
    let fine = text(&t.outside.join("archive"));
    assert_eq!(check(&fine), Ok(fine.clone()));
    assert_eq!(check(&text(&t.outside)), Ok(text(&t.outside)));

    // Inside the sandbox root, and the root itself.
    let inside = check(&text(&t.sandboxes.join("s1").join("archive"))).unwrap_err();
    assert!(inside.contains("sandbox root"), "{inside}");
    assert!(check(&text(&t.sandboxes)).is_err());
    assert!(check(&text(&t.sandboxes.join("not-yet-created"))).is_err());

    // `..` reaches the same place by another spelling: refused outright.
    let dotdot = format!("{}/outside/../sandboxes/s1/archive", t.root.display());
    assert!(check(&dotdot).unwrap_err().contains(".."), "{dotdot}");
    let harmless = format!("{}/outside/../outside/archive", t.root.display());
    assert!(check(&harmless).is_err(), "no . or .. segment is accepted");
    assert!(check(&format!("{}/outside/./archive", t.root.display())).is_err());
    assert!(check(&format!("{}/outside/..", t.root.display())).is_err());

    // `.git` directly.
    assert!(check(&text(&t.outside.join(".git").join("archive"))).is_err());
}

#[cfg(unix)]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn archive_dir_setting_follows_symlinks_before_it_compares() {
    use std::os::unix::fs::symlink;
    let t = tree();
    let roots = vec![t.sandboxes.clone()];
    let check =
        |path: &Path| validate_archive_dir_setting_with(&path.display().to_string(), &roots);

    // A link into a sandbox, then a not-yet-created child: still inside it.
    let link = t.root.join("link");
    symlink(t.sandboxes.join("s1"), &link).unwrap();
    let error = check(&link.join("archive")).unwrap_err();
    assert!(error.contains("sandbox root"), "{error}");
    assert!(check(&link).is_err());

    // A chain of links.
    let chain = t.root.join("chain");
    symlink(&link, &chain).unwrap();
    assert!(check(&chain.join("deep").join("archive")).is_err());

    // A link into a `.git` directory.
    let git = t.root.join("repo").join(".git");
    std::fs::create_dir_all(&git).unwrap();
    let to_git = t.root.join("to-git");
    symlink(&git, &to_git).unwrap();
    let error = check(&to_git.join("archive")).unwrap_err();
    assert!(error.contains(".git"), "{error}");

    // A dangling link has no place the pass could be asked about.
    let dangling = t.root.join("dangling");
    symlink(t.root.join("missing"), &dangling).unwrap();
    let error = check(&dangling.join("archive")).unwrap_err();
    assert!(error.contains("dangling"), "{error}");

    // A link to an allowed place stays allowed (an archive on another disk).
    let good = t.root.join("good");
    symlink(&t.outside, &good).unwrap();
    assert!(check(&good.join("archive")).is_ok());

    // The forbidden root may itself be reached through a link.
    let aliased = vec![t.root.join("root-alias")];
    symlink(&t.sandboxes, &aliased[0]).unwrap();
    let error = validate_archive_dir_setting_with(
        &t.sandboxes.join("s1").join("archive").display().to_string(),
        &aliased,
    )
    .unwrap_err();
    assert!(error.contains("sandbox root"), "{error}");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn check_archive_dir_resolves_dotdot_and_refuses_what_lands_in_a_sandbox() {
    let t = tree();
    let roots = vec![t.sandboxes.clone()];
    // A pass receives resolved paths, but a default directory can still carry
    // `..`; resolution applies it exactly as the operating system would.
    let through = t
        .outside
        .join("..")
        .join("sandboxes")
        .join("s1")
        .join("archive");
    let error = check_archive_dir(&through, &roots).unwrap_err();
    assert!(error.contains("sandbox root"), "{error}");
    let fine = t.outside.join("..").join("outside").join("archive");
    assert_eq!(
        check_archive_dir(&fine, &roots).unwrap(),
        t.outside.join("archive")
    );
    // A `..` after a directory that does not exist cannot be resolved.
    let unresolvable = t.outside.join("missing").join("..").join("archive");
    assert!(check_archive_dir(&unresolvable, &roots).is_err());
    assert!(check_archive_dir(Path::new("relative/archive"), &roots).is_err());
}

impl Fx {
    /// A stage pass writing under `dir`, with `forbidden_roots`.
    fn stage_in(&self, dir: &Path, forbidden_roots: Vec<PathBuf>) -> ColdResult<StageReport> {
        let settings = ColdSettings {
            enabled: true,
            after_days: 30,
            dir: dir.to_path_buf(),
            forbidden_roots,
        };
        let run_id = self
            .store
            .cold_begin_run("run_now", ColdMode::Stage)
            .unwrap();
        run_stage_pass(
            &DirectStoreAccess(&self.store),
            &self.read,
            &PassParams {
                run_id,
                settings: &settings,
                limits: fast_limits(),
                batch_gate: None,
                cancel: None,
            },
        )
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_pass_refuses_an_archive_dir_inside_a_sandbox_root_and_creates_nothing() {
    let fx = Fx::new();
    let sid = fx.cold_session(SessionStatus::Archived, 60, 1);
    let root = fx._dir.path().canonicalize().unwrap();
    let sandboxes = root.join("sandboxes");
    std::fs::create_dir_all(sandboxes.join("s1")).unwrap();
    std::fs::create_dir_all(root.join("outside")).unwrap();
    let roots = vec![sandboxes.clone()];

    let mut refused = vec![
        // The plain path.
        sandboxes.join("s1").join("archive"),
        // `..` through a directory that exists.
        root.join("outside")
            .join("..")
            .join("sandboxes")
            .join("s1")
            .join("archive"),
    ];
    #[cfg(unix)]
    {
        let link = root.join("link");
        std::os::unix::fs::symlink(sandboxes.join("s1"), &link).unwrap();
        refused.push(link.join("archive"));
    }
    for dir in &refused {
        let error = fx.stage_in(dir, roots.clone()).unwrap_err();
        assert_eq!(error.code, "cold_archive_dir_forbidden", "{dir:?}");
        assert!(
            !sandboxes.join("s1").join("archive").exists(),
            "{dir:?} created a directory inside the sandbox root"
        );
    }
    assert!(fx.row(sid).is_none(), "no index row was written");
    assert!(fx.archive_paths().is_empty());

    // The same pass over an allowed directory works.
    let report = fx
        .stage_in(&root.join("outside").join("archive"), roots)
        .unwrap();
    assert_eq!(report.sessions_staged, 1);
    assert!(fx.row(sid).unwrap().verified_at.is_some());
}

#[cfg(unix)]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn an_archive_file_that_is_a_symlink_is_never_written_through() {
    let fx = Fx::new();
    let sid = fx.cold_session(SessionStatus::Archived, 60, 1);
    let root = fx._dir.path().canonicalize().unwrap();
    std::fs::create_dir_all(&fx.archive).unwrap();
    let elsewhere = root.join("elsewhere.sqlite");
    let month = archive_month(Utc::now() - Duration::days(61));
    std::os::unix::fs::symlink(&elsewhere, fx.archive.join(archive_file_name(&month))).unwrap();
    let report = fx.stage_in(&fx.archive, Vec::new()).unwrap();
    assert_eq!(report.sessions_staged, 0);
    assert_eq!(report.sessions_failed, 1);
    assert_eq!(report.errors[0].code, "cold_archive_dir_forbidden");
    assert!(!elsewhere.exists(), "nothing was created through the link");
    assert_eq!(
        fx.row(sid).and_then(|row| row.last_error_code).as_deref(),
        Some("cold_archive_dir_forbidden"),
        "the session's index row records why it never reached its file"
    );
    assert_eq!(fx.archive_chunk_count(), 0);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn verify_refuses_a_recorded_file_whose_directory_is_now_a_sandbox_root() {
    let fx = Fx::new();
    fx.cold_session(SessionStatus::Archived, 60, 1);
    assert_eq!(fx.stage(30).unwrap().sessions_staged, 1);
    // The directory the files were written to becomes a sandbox root (the
    // operator moved the sandbox base): the pass will not read through it.
    let settings = ColdSettings {
        forbidden_roots: vec![fx.archive.clone()],
        ..fx.settings(30)
    };
    let run_id = fx
        .store
        .cold_begin_run("run_now", ColdMode::Verify)
        .unwrap();
    let report = run_verify_pass(
        &DirectStoreAccess(&fx.store),
        &PassParams {
            run_id,
            settings: &settings,
            limits: fast_limits(),
            batch_gate: None,
            cancel: None,
        },
    )
    .unwrap();
    assert_eq!(report.files_missing, 1);
    assert_eq!(report.sessions_ok, 0);
    assert!(
        fx.store.cold_archive_files().unwrap()[0]
            .last_verify_error
            .as_deref()
            .is_some_and(|error| error.contains("cold_archive_dir_forbidden"))
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_schema_change_to_the_archived_tables_stops_the_pass_before_anything_is_written() {
    let fx = Fx::new();
    fx.cold_session(SessionStatus::Archived, 60, 1);
    fx.store
        .conn
        .execute_batch("ALTER TABLE conversation_events ADD COLUMN extra TEXT")
        .unwrap();
    let error = fx.stage(30).unwrap_err();
    assert_eq!(error.code, "cold_archive_schema_drift");
    assert_eq!(
        fx.count("SELECT COUNT(*) FROM cold_archive_sessions", None),
        0
    );
}

// ---------------------------------------------------------------------------
// Settings resolution
// ---------------------------------------------------------------------------

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn archive_dir_resolution_and_validation() {
    assert_eq!(
        resolve_archive_dir("", Some(Path::new("/home/u/.rsi/rsi.db"))),
        PathBuf::from("/home/u/.rsi/archive")
    );
    assert_eq!(
        resolve_archive_dir("  /mnt/cold ", Some(Path::new("/home/u/.rsi/rsi.db"))),
        PathBuf::from("/mnt/cold")
    );
    assert_eq!(validate_archive_dir_setting(""), Ok(String::new()));
    assert_eq!(
        validate_archive_dir_setting(" /mnt/cold "),
        Ok("/mnt/cold".to_string())
    );
    for bad in ["relative/dir", "/a/.git/b", "/a\0b"] {
        assert!(validate_archive_dir_setting(bad).is_err(), "{bad:?}");
    }
    let sandbox = rsi_common::identity::data_dir()
        .join("sandboxes")
        .join("s1");
    assert!(validate_archive_dir_setting(&sandbox.display().to_string()).is_err());
    assert_eq!(ColdMode::parse("stage"), Some(ColdMode::Stage));
    assert_eq!(ColdMode::parse("verify"), Some(ColdMode::Verify));
    assert_eq!(ColdMode::parse("move"), Some(ColdMode::Move));
    assert_eq!(ColdMode::parse("compact"), Some(ColdMode::Compact));
    assert_eq!(ColdMode::parse("everything"), None);
}

#[cfg(target_os = "linux")]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn the_pass_thread_lowers_its_own_priority() {
    let handle = std::thread::spawn(|| {
        lower_current_thread_priority();
        // SAFETY: getpriority takes plain integers; who 0 is the calling thread.
        unsafe { nix::libc::getpriority(nix::libc::PRIO_PROCESS, 0) }
    });
    assert_eq!(handle.join().unwrap(), 19);
}

// ---------------------------------------------------------------------------
// V176 guards and migration
// ---------------------------------------------------------------------------

fn next_event(session_id: Uuid, sequence: i32) -> ConversationEvent {
    ConversationEvent {
        id: 0,
        session_id,
        sequence,
        event_type: EventType::Message,
        role: Some(Role::Assistant),
        created_at: Utc::now(),
        content: format!("later {sequence}"),
        tool_name: None,
        tool_input: None,
        offload_id: None,
        tool_use_id: None,
        metadata: None,
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn guard_three_keeps_a_cold_session_archived_until_it_is_restored() {
    let fx = Fx::new();
    let sid = fx.session(SessionStatus::Archived, 60);
    let set_status = |status: &str| {
        fx.store.conn.execute(
            "UPDATE sessions SET status = ?2 WHERE id = ?1",
            params![sid.to_string(), status],
        )
    };
    let status = || -> String {
        fx.store
            .conn
            .query_row(
                "SELECT status FROM sessions WHERE id = ?1",
                [sid.to_string()],
                |row| row.get(0),
            )
            .unwrap()
    };
    // No index row: status changes are untouched by the guard.
    set_status("Completed").unwrap();
    set_status("Archived").unwrap();
    index_row(&fx.store, sid, "staged", None);
    for state in ["staged", "restored", "restoring"] {
        set_state(&fx.store, sid, state);
        set_status("Completed").unwrap_or_else(|error| panic!("{state} admits unarchive: {error}"));
        set_status("Archived").unwrap();
    }
    for state in ["cold", "deleting"] {
        set_state(&fx.store, sid, state);
        let error = set_status("Completed").unwrap_err().to_string();
        assert!(
            error.contains("cold_session_restore_required"),
            "{state}: {error}"
        );
        assert_eq!(
            status(),
            "Archived",
            "{state}: the refused update changed nothing"
        );
        // Archived <-> Deleted never leaves the cold statuses.
        set_status("Deleted").unwrap();
        assert_eq!(status(), "Deleted");
        set_status("Archived").unwrap();
        let error = set_status("Running").unwrap_err().to_string();
        assert!(
            error.contains("cold_session_restore_required"),
            "{state}: {error}"
        );
    }
    // Other columns of a cold session stay writable.
    fx.store
        .conn
        .execute(
            "UPDATE sessions SET updated_at = ?2 WHERE id = ?1",
            params![sid.to_string(), ts(0)],
        )
        .unwrap();
    // Another session is not affected by this one's state.
    let other = fx.session(SessionStatus::Archived, 60);
    fx.store
        .conn
        .execute(
            "UPDATE sessions SET status = 'Completed' WHERE id = ?1",
            [other.to_string()],
        )
        .unwrap();
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_session_whose_age_cannot_be_proven_is_skipped_as_age_unproven() {
    let fx = Fx::new();
    let ok = fx.cold_session(SessionStatus::Archived, 60, 1);
    let too_new = fx.cold_session(SessionStatus::Archived, 60, 1);
    let unparseable = fx.cold_session(SessionStatus::Archived, 60, 1);
    let legacy = fx.cold_session(SessionStatus::Archived, 60, 1);
    // A row below the newest-by-sequence row: the cheap prefilter (newest event is
    // old) passes all four, the age proof reads every row.
    fx.event_at(too_new, 0, "System", None, "x", None, None, &ts(1));
    fx.event_at(
        unparseable,
        0,
        "System",
        None,
        "x",
        None,
        None,
        "not a time",
    );
    fx.event_at(
        legacy,
        0,
        "System",
        None,
        "x",
        None,
        None,
        "2026-08-02T16:15:11,769711900-07:00",
    );
    let cutoff = Utc::now() - Duration::days(30);
    let page = candidate_page(&fx.read, None, 256, cutoff).unwrap();
    assert_eq!(page.candidates.len(), 4, "the prefilter admits all four");
    assert!(prove_age(&fx.read, &ok.to_string(), cutoff).unwrap());
    for session in [too_new, unparseable, legacy] {
        assert!(
            !prove_age(&fx.read, &session.to_string(), cutoff).unwrap(),
            "{session}"
        );
    }
    let report = fx.stage(30).unwrap();
    assert_eq!(report.sessions_staged, 1);
    assert_eq!(report.holds.get("age_unproven"), Some(&3));
    assert!(fx.row(ok).is_some());
    for session in [too_new, unparseable, legacy] {
        assert!(fx.row(session).is_none(), "{session} stays live");
    }
    assert_eq!(
        fx.count("SELECT COUNT(*) FROM cold_archive_sessions", None),
        1
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn guard_one_refuses_writes_to_cold_sessions_and_admits_every_other_state() {
    let fx = Fx::new();
    let sid = fx.cold_session(SessionStatus::Archived, 60, 1);
    let mut sequence = 100;
    // No index row: ordinary sessions are untouched by the guard.
    fx.store.insert_event(&next_event(sid, sequence)).unwrap();
    index_row(&fx.store, sid, "staged", None);
    for state in ["staged", "restored", "restoring"] {
        set_state(&fx.store, sid, state);
        sequence += 1;
        fx.store
            .insert_event(&next_event(sid, sequence))
            .unwrap_or_else(|error| panic!("{state} must admit inserts: {error}"));
    }
    for state in ["cold", "deleting"] {
        set_state(&fx.store, sid, state);
        sequence += 1;
        let error = fx
            .store
            .insert_event(&next_event(sid, sequence))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("cold_session_restore_required"),
            "{state} must refuse the insert: {error}"
        );
    }
    // Another session is not affected by this one's state.
    let other = fx.session(SessionStatus::Archived, 60);
    fx.store.insert_event(&next_event(other, 1)).unwrap();
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn guard_two_keeps_provenance_retained_except_inside_the_archiver_transaction() {
    let fx = Fx::new();
    let sid = fx.cold_session(SessionStatus::Archived, 60, 1);
    let event_id: i64 = fx
        .store
        .conn
        .query_row(
            "SELECT conversation_event_id FROM conversation_event_provenance p
             JOIN conversation_events e ON e.id = p.conversation_event_id
             WHERE e.session_id = ?1 ORDER BY p.conversation_event_id LIMIT 1",
            [sid.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    let delete = || {
        fx.store.conn.execute(
            "DELETE FROM conversation_event_provenance WHERE conversation_event_id = ?1",
            [event_id],
        )
    };
    let refused = |label: &str| {
        let error = delete().unwrap_err().to_string();
        assert!(
            error.contains("conversation event provenance is retained"),
            "{label}: {error}"
        );
    };
    refused("no index row");
    index_row(&fx.store, sid, "staged", None);
    for state in ["staged", "cold", "restoring", "restored"] {
        set_state(&fx.store, sid, state);
        refused(state);
    }
    // Another session's `deleting` state does not open this session's rows.
    let other = fx.cold_session(SessionStatus::Archived, 60, 1);
    index_row(&fx.store, other, "deleting", None);
    refused("another session deleting");
    set_state(&fx.store, sid, "deleting");
    assert_eq!(
        delete().unwrap(),
        1,
        "inside the archiver's own state the delete is allowed"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn purge_drops_a_staged_index_row_and_refuses_a_cold_session() {
    let fx = Fx::new();
    let staged = fx.session(SessionStatus::Deleted, 60);
    index_row(&fx.store, staged, "staged", None);
    fx.store.purge_session(staged).unwrap();
    assert!(fx.store.get_session(staged).unwrap().is_none());
    assert!(
        fx.row(staged).is_none(),
        "a purged session leaves no index row"
    );

    let cold = fx.session(SessionStatus::Deleted, 60);
    index_row(&fx.store, cold, "cold", None);
    let error = fx.store.purge_session(cold).unwrap_err().to_string();
    assert!(error.contains("session_cold_archived"), "{error}");
    assert!(fx.store.get_session(cold).unwrap().is_some());
    assert_eq!(fx.row(cold).unwrap().state, "cold");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn v176_tables_enforce_their_documented_checks() {
    let fx = Fx::new();
    let sid = fx.session(SessionStatus::Archived, 60);
    let file_id = fx
        .store
        .cold_register_archive_file("/tmp/x/rsi-cold-2026-02.sqlite", "2026-02", 1)
        .unwrap();
    assert_eq!(
        file_id,
        fx.store
            .cold_register_archive_file("/tmp/x/rsi-cold-2026-02.sqlite", "2026-02", 1)
            .unwrap(),
        "registering a path twice is idempotent"
    );
    let insert = |state: &str, generation: i64| {
        fx.store.conn.execute(
            "INSERT OR REPLACE INTO cold_archive_sessions(
                session_id, state, generation, file_id, events_chunk_id, pre_event_count,
                pre_provenance_count, archived_event_count, archived_provenance_count,
                archived_content_len, events_sha256, raw_bytes, compressed_bytes, staged_at,
                updated_at)
             VALUES (?1, ?2, ?3, ?4, 0, 0, 0, 0, 0, 0, '', 0, 0, ?5, ?5)",
            params![sid.to_string(), state, generation, file_id, ts(0)],
        )
    };
    assert!(insert("staged", 1).is_ok());
    assert!(insert("bogus", 1).is_err(), "unknown state");
    assert!(insert("staged", 0).is_err(), "generation starts at 1");
    let run = |mode: &str, outcome: &str, id: &str| {
        fx.store.conn.execute(
            "INSERT INTO cold_archive_runs(run_id, trigger, mode, started_at, outcome)
             VALUES (?1, 'scheduled', ?2, ?3, ?4)",
            params![id, mode, ts(0), outcome],
        )
    };
    assert!(run("stage", "running", &Uuid::new_v4().to_string()).is_ok());
    assert!(run("move", "partial", &Uuid::new_v4().to_string()).is_ok());
    assert!(run("bogus", "running", &Uuid::new_v4().to_string()).is_err());
    assert!(run("stage", "bogus", &Uuid::new_v4().to_string()).is_err());
    assert!(
        run("stage", "running", "NOT-A-UUID").is_err(),
        "run ids are canonical UUIDs"
    );
    // The session row cannot be deleted while its index row exists.
    drop_triggers(&fx.store.conn, "sessions");
    assert!(
        fx.store
            .conn
            .execute("DELETE FROM sessions WHERE id = ?1", [sid.to_string()])
            .is_err()
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn v176_replays_from_v175_keeps_rows_and_restores_the_original_provenance_guard() {
    use crate::store::{LATEST_SCHEMA_VERSION, tests::rewind_post_v121_tail_to};
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("rsi.db");
    let sid = {
        let fx_store = Store::open_test_fixture(&db).unwrap();
        assert_eq!(fx_store.schema_version().unwrap(), LATEST_SCHEMA_VERSION);
        assert!(LATEST_SCHEMA_VERSION >= 176);
        let mut session = make_test_session();
        session.status = SessionStatus::Archived;
        fx_store.insert_session(&session).unwrap();
        fx_store
            .conn
            .execute(
                "INSERT INTO conversation_events(session_id, sequence, event_type, role, content,
                    created_at) VALUES (?1, 1, 'System', NULL, 'kept', ?2)",
                params![session.id.to_string(), ts(1)],
            )
            .unwrap();
        rewind_post_v121_tail_to(&fx_store.conn, 175);
        let objects: i64 = fx_store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name LIKE 'cold_archive_%'
                    OR name = 'conversation_events_cold_insert_guard'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(objects, 0, "the rewind removes the V176 objects");
        let guard: String = fx_store
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name = 'conversation_event_provenance_no_delete'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            !guard.contains("cold_archive"),
            "the V84 guard is back: {guard}"
        );
        assert_eq!(fx_store.schema_version().unwrap(), 175);
        session.id
    };
    let reopened = Store::open(&db).unwrap();
    assert_eq!(reopened.schema_version().unwrap(), LATEST_SCHEMA_VERSION);
    let objects: Vec<String> = reopened
        .conn
        .prepare(
            "SELECT name FROM sqlite_master
             WHERE name IN ('cold_archive_files', 'cold_archive_sessions', 'cold_archive_runs',
                            'cold_archive_sessions_state', 'conversation_events_cold_insert_guard')
             ORDER BY name",
        )
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(objects.len(), 5, "{objects:?}");
    let guard: String = reopened
        .conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name = 'conversation_event_provenance_no_delete'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(guard.contains("cold_archive_sessions"));
    let kept: i64 = reopened
        .conn
        .query_row(
            "SELECT COUNT(*) FROM conversation_events WHERE session_id = ?1",
            [sid.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(kept, 1, "the migration moves nothing");
    let runs: i64 = reopened
        .conn
        .query_row("SELECT COUNT(*) FROM cold_archive_runs", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(runs, 0);
}
