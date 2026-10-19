// rsi-migration: contract
/// Cold-storage archive index (#2384, M1). The live database keeps the index of
/// what was archived (`cold_archive_files`, `cold_archive_sessions`,
/// `cold_archive_runs`); the bulky rows themselves live in append-only archive
/// files under the cold-archive directory. This migration moves nothing.
///
/// Two guards come with it. Guard 1 refuses a new conversation event for a
/// session whose rows are archived (`cold`) or being deleted (`deleting`), so a
/// write can never allocate a sequence that collides with an archived one.
/// Guard 2 replaces `conversation_event_provenance_no_delete`: provenance stays
/// retained except inside the archiver's own transaction, where the owning
/// session's index row is in the in-transaction `deleting` state. Guard 3
/// (`sessions_cold_status_guard`, plan review #2392) keeps a `cold` session
/// `Archived`/`Deleted` until it is restored, so no unarchive or resume path can
/// make a session live with its rows in the archive.
impl Store {
    fn migrate_v176(&self, version: i32) -> Result<()> {
        if version < 176 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            tx.execute_batch(
                "CREATE TABLE cold_archive_files(
                    file_id INTEGER PRIMARY KEY,
                    path TEXT NOT NULL UNIQUE,
                    month TEXT NOT NULL,
                    format_version INTEGER NOT NULL,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    bytes INTEGER NOT NULL DEFAULT 0,
                    chunk_count INTEGER NOT NULL DEFAULT 0,
                    last_verified_at TEXT,
                    last_verify_error TEXT
                );
                CREATE TABLE cold_archive_sessions(
                    session_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE RESTRICT,
                    state TEXT NOT NULL CHECK(state IN ('staged','deleting','cold','restoring','restored')),
                    generation INTEGER NOT NULL CHECK(generation >= 1),
                    file_id INTEGER NOT NULL REFERENCES cold_archive_files(file_id),
                    events_chunk_id INTEGER NOT NULL,
                    provenance_chunk_id INTEGER,
                    pre_event_count INTEGER NOT NULL,
                    pre_provenance_count INTEGER NOT NULL,
                    archived_event_count INTEGER NOT NULL,
                    archived_provenance_count INTEGER NOT NULL,
                    archived_content_len INTEGER NOT NULL,
                    archived_min_sequence INTEGER,
                    archived_max_sequence INTEGER,
                    events_sha256 TEXT NOT NULL,
                    provenance_sha256 TEXT,
                    raw_bytes INTEGER NOT NULL,
                    compressed_bytes INTEGER NOT NULL,
                    staged_at TEXT NOT NULL,
                    archived_at TEXT,
                    restored_at TEXT,
                    updated_at TEXT NOT NULL,
                    last_error_code TEXT,
                    verified_at TEXT
                );
                CREATE INDEX cold_archive_sessions_state ON cold_archive_sessions(state, session_id);
                CREATE TABLE cold_archive_runs(
                    run_id TEXT PRIMARY KEY CHECK(rsi_uuid_is_canonical(run_id)),
                    trigger TEXT NOT NULL CHECK(trigger IN ('scheduled','run_now')),
                    mode TEXT NOT NULL CHECK(mode IN ('stage','move','verify','compact')),
                    started_at TEXT NOT NULL,
                    finished_at TEXT,
                    outcome TEXT NOT NULL CHECK(outcome IN ('running','completed','partial','failed','skipped')),
                    sessions_staged INTEGER NOT NULL DEFAULT 0,
                    sessions_moved INTEGER NOT NULL DEFAULT 0,
                    events_moved INTEGER NOT NULL DEFAULT 0,
                    provenance_moved INTEGER NOT NULL DEFAULT 0,
                    raw_bytes INTEGER NOT NULL DEFAULT 0,
                    compressed_bytes INTEGER NOT NULL DEFAULT 0,
                    holds_json TEXT NOT NULL DEFAULT '{}',
                    pre_counts_json TEXT,
                    live_bytes_before INTEGER,
                    live_bytes_after INTEGER,
                    pages_reclaimed INTEGER NOT NULL DEFAULT 0,
                    compact_ms INTEGER,
                    error_code TEXT,
                    error_detail TEXT
                );
                CREATE TRIGGER conversation_events_cold_insert_guard BEFORE INSERT ON conversation_events
                WHEN EXISTS (
                    SELECT 1 FROM cold_archive_sessions c
                    WHERE c.session_id = NEW.session_id AND c.state IN ('cold','deleting')
                )
                BEGIN SELECT RAISE(ABORT,'cold_session_restore_required'); END;
                DROP TRIGGER conversation_event_provenance_no_delete;
                CREATE TRIGGER conversation_event_provenance_no_delete BEFORE DELETE ON conversation_event_provenance
                WHEN NOT EXISTS (
                    SELECT 1 FROM conversation_events e
                    JOIN cold_archive_sessions c ON c.session_id = e.session_id
                    WHERE e.id = OLD.conversation_event_id AND c.state = 'deleting'
                )
                BEGIN SELECT RAISE(ABORT,'conversation event provenance is retained'); END;
                CREATE TRIGGER sessions_cold_status_guard BEFORE UPDATE OF status ON sessions
                WHEN NEW.status NOT IN ('Archived','Deleted')
                 AND EXISTS (
                    SELECT 1 FROM cold_archive_sessions c
                    WHERE c.session_id = NEW.id AND c.state IN ('cold','deleting')
                )
                BEGIN SELECT RAISE(ABORT,'cold_session_restore_required'); END;",
            )?;
            tx.pragma_update(None, "user_version", 176)?;
            tx.commit()?;
        }
        Ok(())
    }
}
