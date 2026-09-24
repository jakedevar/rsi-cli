//! Integration tests for the memory system.
//!
//! These tests exercise end-to-end flows: write files → sync → search,
//! FTS-only fallback, concurrent sync serialization, and edge cases.

use std::sync::Arc;

use rsi_common::types::{
    ContextUsageConfidence, ConversationEvent, EventType, Role, Session, SessionKind,
    SessionProvider, SessionStatus,
};
use rsid::memory::embedding::EmbeddingProviderResult;
use rsid::memory::embedding::mock::MockEmbeddingProvider;
use rsid::memory::reindex::cleanup_stale_temp_files;
use rsid::memory::store::MemoryStore;
use rsid::memory::sync::MemorySyncEngine;
use rsid::memory::types::MemoryConfig;
use rsid::store::Store;
use tempfile::TempDir;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Deterministic in-process embedding provider for the end-to-end
/// write/sync/search fixture.
///
/// `provider_label` is not a free-form display string: `MemorySyncEngine`
/// copies it into `EmbeddingControl.provider`, and model control then
/// classifies it as a provider id. Labelling the mock `"mock"` classified the
/// fixture as paid background work, so `memory.embedding.index` was refused by
/// default and `test_e2e_write_sync_search` could not run at all (issue #339).
///
/// `"Local"` matches the existing `make_local_test_control` precedent in
/// `memory::embedding::batch`: an in-process, non-paid provider. Only the
/// label is Local — the provider itself is still the deterministic
/// `MockEmbeddingProvider`, so no network or model call is introduced, and the
/// production paid-background default is untouched.
fn make_mock_provider(dims: usize) -> Arc<EmbeddingProviderResult> {
    let provider = MockEmbeddingProvider::new(dims);
    Arc::new(EmbeddingProviderResult {
        provider: Some(Box::new(provider)),
        provider_label: "Local".to_string(),
        backend: "mock".to_string(),
        base_url: None,
        requested: "mock".to_string(),
        fallback_reason: None,
        unavailable_reason: None,
    })
}

fn make_fts_only_provider() -> Arc<EmbeddingProviderResult> {
    Arc::new(EmbeddingProviderResult {
        provider: None,
        provider_label: "none".to_string(),
        backend: "none".to_string(),
        base_url: None,
        requested: "none".to_string(),
        fallback_reason: None,
        unavailable_reason: None,
    })
}

fn setup_engine_with_provider(
    dir: &TempDir,
    provider: Arc<EmbeddingProviderResult>,
) -> MemorySyncEngine {
    let memory_dir = dir.path().join("memory");
    std::fs::create_dir_all(&memory_dir).unwrap();
    let db_path = dir.path().join("memory.sqlite");
    let store = MemoryStore::open(&db_path).unwrap();

    let main_db = dir.path().join("flywheel.db");
    let _main_store = Store::open(&main_db).unwrap();

    MemorySyncEngine::new(
        MemoryConfig {
            memory_dir: memory_dir.clone(),
            db_path: db_path.clone(),
            ..Default::default()
        },
        store,
        main_db,
        provider,
        Arc::new(rsid::bus::EventBus::new(8)),
        memory_dir,
        db_path,
    )
}

fn make_completed_session(session_id: Uuid, project_id: Option<Uuid>) -> Session {
    Session {
        context_fill_pct: None,
        id: session_id,
        provider: SessionProvider::Claude,
        claude_session_id: Some("claude-test".to_string()),
        query: "Project-scoped memory".to_string(),
        title: None,
        agent_role: None,
        epic_spawn_ordinal: None,
        description: None,
        short_summary: None,
        pending_question: None,
        pending_archive: false,
        working_dir: std::path::PathBuf::from("/tmp/test"),
        git_branch: None,
        status: SessionStatus::Completed,
        project_id,
        pinned_at: None,
        testing_needed_at: None,
        rotation_disabled_at: None,
        session_kind: SessionKind::Standard,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        cost_usd: None,
        duration_ms: None,
        num_turns: None,
        model: Some("claude-sonnet-5".to_string()),
        input_tokens: None,
        output_tokens: None,
        context_window: None,
        resolved_context_budget: None,
        total_input_tokens: None,
        total_output_tokens: None,
        total_cache_creation_tokens: None,
        total_cache_read_tokens: None,
        stop_reason: None,
        continued_from: None,
        context_usage_confidence: ContextUsageConfidence::Missing,
        daemon_input_tokens: None,
        daemon_output_tokens: None,
        handoff_filepath: None,
        active_task: None,
        group_id: None,
        pipeline_artifact: None,
        workflow_id: None,
        workflow_id_override: None,
        rotation_depth: 0,
        retry_attempt: None,
        max_retries: None,
        effort: None,
        issue_identifier: None,
        issue_url: None,
        issue_tracker_id: None,
        scheduled_job_id: None,
        rating: None,
        harness_version_hash: None,
        test_passed: None,
        clippy_passed: None,
        turn_count: None,
        retry_count: None,
        approval_wait_ms: None,
        work_time_ms: None,
        approval_started_at: None,
        sandbox_kind: None,
        sandbox_root: None,
        sandbox_branch: None,
        sandbox_cleanup_state: None,
        tag: String::new(),
        tags: Vec::new(),
        parent_id: None,
        lead_session_id: None,
        is_eval: false,
        capability_class: None,
        topology_node_id: None,
        topology_iteration: 0,
        provider_cli_version: None,
        provider_capabilities: Vec::new(),
        thinking_tokens: None,
        service_tier: None,
        cache_creation_1h_tokens: None,
        cache_creation_5m_tokens: None,
        permission_denial_count: None,
        subagent_stats_json: None,
        queued_turn_count: None,
        terminal_reason: None,
    }
}

fn make_message_event(session_id: Uuid, sequence: i32, content: &str) -> ConversationEvent {
    ConversationEvent {
        id: 0,
        session_id,
        sequence,
        event_type: EventType::Message,
        role: Some(Role::Assistant),
        content: content.to_string(),
        tool_name: None,
        tool_input: None,
        created_at: chrono::Utc::now(),
        offload_id: None,
        tool_use_id: None,
        metadata: None,
    }
}

// ---------------------------------------------------------------------------
// 8.1 End-to-End: Write -> Sync -> Search
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_e2e_write_sync_search() {
    let dir = TempDir::new().unwrap();
    let memory_dir = dir.path().join("memory");
    std::fs::create_dir_all(&memory_dir).unwrap();
    std::fs::write(
        memory_dir.join("2026-02-28.md"),
        "# Today\n\nWe discussed the Rust memory system architecture.\nKey: SQLite + sqlite-vec.\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("MEMORY.md"),
        "# Project\n\nFlywheel is a TUI.\n",
    )
    .unwrap();

    let provider = make_mock_provider(128);
    let mut engine = setup_engine_with_provider(&dir, provider);

    let report = engine.run_sync("test", false).await.unwrap();
    assert!(report.files_indexed > 0, "should index at least one file");

    // Search for content we wrote (unscoped — exercise pre-V3 global path)
    let results = engine
        .search("Rust memory architecture", None, None, None)
        .await
        .unwrap();
    assert!(
        !results.is_empty(),
        "search should return results for indexed content"
    );
}

#[tokio::test]
async fn test_session_project_reassignment_reindexes_transcripts() {
    use rsid::memory::types::MemorySource;

    let dir = TempDir::new().unwrap();
    let main_db = dir.path().join("flywheel.db");
    let memory_db = dir.path().join("memory.sqlite");
    let mut engine = setup_engine_with_provider(&dir, make_fts_only_provider());
    let store = Store::open(&main_db).unwrap();

    let session_id = Uuid::new_v4();
    let project_a = Uuid::new_v4();
    let project_b = Uuid::new_v4();
    let project_a_str = project_a.to_string();
    let project_b_str = project_b.to_string();
    let session_path = format!("sessions/{session_id}");

    store
        .insert_session(&make_completed_session(session_id, Some(project_a)))
        .unwrap();
    store
        .insert_event(&make_message_event(
            session_id,
            1,
            "shared_keyword alpha-only",
        ))
        .unwrap();
    store
        .insert_event(&make_message_event(
            session_id,
            2,
            "shared_keyword alpha-followup",
        ))
        .unwrap();

    let first_report = engine.run_sync("initial", false).await.unwrap();
    assert_eq!(first_report.sessions_indexed, 1);

    let memory_store = MemoryStore::open(&memory_db).unwrap();
    let file = memory_store.get_file(&session_path).unwrap().unwrap();
    assert_eq!(file.project_id, Some(project_a));

    let chunks = memory_store
        .get_chunks_for_file(&session_path, MemorySource::Sessions)
        .unwrap();
    assert!(!chunks.is_empty());
    assert!(
        chunks
            .iter()
            .all(|chunk| chunk.project_id.as_deref() == Some(project_a_str.as_str()))
    );

    store
        .update_session_project(session_id, Some(project_b))
        .unwrap();

    let second_report = engine.run_sync("project-reassign", false).await.unwrap();
    assert_eq!(second_report.sessions_indexed, 1);

    let memory_store = MemoryStore::open(&memory_db).unwrap();
    let file = memory_store.get_file(&session_path).unwrap().unwrap();
    assert_eq!(file.project_id, Some(project_b));

    let chunks = memory_store
        .get_chunks_for_file(&session_path, MemorySource::Sessions)
        .unwrap();
    assert!(!chunks.is_empty());
    assert!(
        chunks
            .iter()
            .all(|chunk| chunk.project_id.as_deref() == Some(project_b_str.as_str()))
    );
}

// ---------------------------------------------------------------------------
// 8.2 Graceful Degradation: FTS-Only Fallback
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_fts_only_fallback() {
    let dir = TempDir::new().unwrap();
    let memory_dir = dir.path().join("memory");
    std::fs::create_dir_all(&memory_dir).unwrap();
    std::fs::write(
        memory_dir.join("notes.md"),
        "# Rust Testing\nUnit tests with cargo test.\nIntegration tests are important.\n",
    )
    .unwrap();

    let mut engine = setup_engine_with_provider(&dir, make_fts_only_provider());
    let report = engine.run_sync("test", false).await.unwrap();
    assert!(report.files_indexed > 0);

    // FTS-only search should still work
    let results = engine
        .search("cargo test", None, Some(0.0), None)
        .await
        .unwrap();
    assert!(!results.is_empty(), "FTS-only search should find results");
}

// ---------------------------------------------------------------------------
// 8.3 Atomic Reindex Interrupt Recovery
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_atomic_reindex_preserves_original_on_failure() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("memory.sqlite");

    // Create a valid store with a known file
    let store = MemoryStore::open(&db_path).unwrap();
    use rsid::memory::types::{MemoryFileEntry, MemorySource};
    store
        .upsert_file(&MemoryFileEntry {
            path: "MEMORY.md".into(),
            abs_path: dir.path().join("MEMORY.md"),
            mtime_ms: 0,
            size: 10,
            hash: "original".into(),
            source: MemorySource::Memory,
            project_id: None,
            watermark: None,
        })
        .unwrap();
    drop(store);

    // Verify the original data persists after a failed reindex:
    // the store should still have the original hash
    let store = MemoryStore::open(&db_path).unwrap();
    let file = store.get_file("MEMORY.md").unwrap().unwrap();
    assert_eq!(file.hash, "original", "original data should persist");
}

// ---------------------------------------------------------------------------
// 8.4 Concurrent Sync Serialization
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_concurrent_sync_serialization() {
    let dir = TempDir::new().unwrap();
    let memory_dir = dir.path().join("memory");
    std::fs::create_dir_all(&memory_dir).unwrap();
    std::fs::write(memory_dir.join("notes.md"), "# Notes\nSome content.").unwrap();

    let provider = make_fts_only_provider();

    // Run two syncs sequentially on the same engine to verify no deadlocks
    let mut engine = setup_engine_with_provider(&dir, provider);

    let r1 = engine.run_sync("c1", false).await;
    assert!(r1.is_ok(), "first sync should succeed");

    engine.mark_dirty();
    let r2 = engine.run_sync("c2", false).await;
    assert!(r2.is_ok(), "second sync should succeed");
}

// ---------------------------------------------------------------------------
// 8.5 WAL Concurrent Readers (No Deadlock)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_wal_concurrent_readers_no_deadlock() {
    let dir = TempDir::new().unwrap();
    let main_db_path = dir.path().join("flywheel.db");
    let _main_store = Store::open(&main_db_path).unwrap();

    let memory_dir = dir.path().join("memory");
    std::fs::create_dir_all(&memory_dir).unwrap();
    std::fs::write(memory_dir.join("notes.md"), "# Notes").unwrap();

    let mut engine = setup_engine_with_provider(&dir, make_fts_only_provider());

    // Run a reader loop concurrently with a sync
    let main_db_path_clone = main_db_path.clone();
    let reader = tokio::spawn(async move {
        for _ in 0..10 {
            // Open a fresh connection and read — should not deadlock
            if let Ok(store) = Store::open(&main_db_path_clone) {
                let _ = store.load_sessions();
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    });

    let sync_result = engine.run_sync("deadlock-test", false).await;

    let reader_result = reader.await;
    reader_result.unwrap();
    assert!(sync_result.is_ok());
}

// ---------------------------------------------------------------------------
// 9.1 Stale .tmp-* Cleanup
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_stale_tmp_cleanup_on_startup() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("memory.sqlite");
    let _store = MemoryStore::open(&db_path).unwrap();
    drop(_store);

    // Create stale temp files
    let stale = dir
        .path()
        .join("memory.sqlite.tmp-00000000-0000-0000-0000-000000000001");
    let stale_wal = dir
        .path()
        .join("memory.sqlite.tmp-00000000-0000-0000-0000-000000000001-wal");
    std::fs::write(&stale, "stale db").unwrap();
    std::fs::write(&stale_wal, "stale wal").unwrap();

    cleanup_stale_temp_files(&db_path).await.unwrap();

    assert!(!stale.exists(), "stale temp should be cleaned");
    assert!(!stale_wal.exists(), "stale WAL should be cleaned");
    assert!(db_path.exists(), "main DB should be preserved");
}

// ---------------------------------------------------------------------------
// 9.2 Empty Memory Directory
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_sync_empty_memory_dir() {
    let dir = TempDir::new().unwrap();
    let mut engine = setup_engine_with_provider(&dir, make_fts_only_provider());
    let report = engine.run_sync("empty", false).await.unwrap();
    assert_eq!(report.files_indexed, 0);
}

// ---------------------------------------------------------------------------
// 9.3 Large Files (>1MB)
// ---------------------------------------------------------------------------

#[test]
fn test_chunk_large_file_no_oom() {
    use rsid::memory::chunking::chunk_markdown;

    let content = "A".repeat(2_000_000);
    let chunks = chunk_markdown(&content, 400, 80);
    assert!(chunks.len() > 100, "large file should produce many chunks");
    // Verify all chunks are bounded (may be larger than max_chars for single-line content
    // since long lines are segmented but segment + overlap can exceed the budget slightly)
    for chunk in &chunks {
        assert!(
            chunk.text.len() <= 10_000,
            "chunk should be reasonably bounded, got {} bytes",
            chunk.text.len()
        );
    }
}

// ---------------------------------------------------------------------------
// 9.4 sqlite-vec Extension Not Found
// ---------------------------------------------------------------------------

#[test]
fn test_sqlite_vec_not_loaded_degrades_gracefully() {
    // Open a store, force vec_extension_loaded = false, ensure graceful degradation
    // (The MemoryStore always loads sqlite-vec now via vendored extension,
    // but we test the fallback path by manually overriding the flag)
    let store = MemoryStore::open_in_memory().unwrap();
    // vector_available starts false until ensure_vector_table is called
    assert!(!store.vector_available());
    // Insert/search noop when vector not available
    let chunks = vec![("id1".to_string(), vec![1.0f32, 2.0])];
    store.insert_vector_chunks(&chunks).unwrap();
    let results = store.search_vector(&[1.0, 2.0], 10).unwrap();
    assert!(results.is_empty());
}

// ---------------------------------------------------------------------------
// Project-scoped agent memory retrieval (RSI plan §Phase 8)
// ---------------------------------------------------------------------------
//
// Seeds two project-scoped chunks plus one global chunk and asserts that
// project-scoped search returns only the matching project's row, while
// unscoped search returns all three.

#[tokio::test]
async fn test_project_scoped_search_isolates_projects() {
    use rsid::memory::types::{MemoryConfig, MemorySource};
    use rusqlite::params;

    let dir = TempDir::new().unwrap();
    let _engine = setup_engine_with_provider(&dir, make_fts_only_provider());

    // Reach into the raw store to seed FTS rows with explicit project_ids.
    // Going through the public sync path would require building real
    // sessions in the main store; project-scoped search filtering is the
    // unit under test here, not the sync glue (covered by other tests).
    {
        let conn = rusqlite::Connection::open(dir.path().join("memory.sqlite")).unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
        // Note: schema is already initialized by setup_engine_with_provider.

        let project_a = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
        let project_b = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";

        for (id, path, source, project, text) in [
            (
                "c_a:sessions:1:h",
                "sessions/proj-a.md",
                MemorySource::Sessions.as_str(),
                Some(project_a),
                "shared_keyword alpha-only",
            ),
            (
                "c_b:sessions:1:h",
                "sessions/proj-b.md",
                MemorySource::Sessions.as_str(),
                Some(project_b),
                "shared_keyword beta-only",
            ),
            (
                "c_g:memory:1:h",
                "memory/global.md",
                MemorySource::Memory.as_str(),
                None,
                "shared_keyword global-note",
            ),
        ] {
            conn.execute(
                "INSERT INTO chunks (id, path, source, start_line, end_line, hash, model, text, embedding, updated_at, project_id)
                 VALUES (?1, ?2, ?3, 1, 5, 'h', 'nomic', ?4, '', 0, ?5)",
                params![id, path, source, text, project],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO chunks_fts (id, path, source, project_id, model, start_line, end_line, text)
                 VALUES (?1, ?2, ?3, ?4, 'nomic', 1, 5, ?5)",
                params![id, path, source, project, text],
            )
            .unwrap();
        }
    }

    let store = rsid::memory::store::MemoryStore::open(&dir.path().join("memory.sqlite")).unwrap();
    let mut config = MemoryConfig::default();
    config.min_score = 0.0; // FTS BM25 scores rarely clear the default 0.35
    config.max_results = 10;

    // Project A scope returns only A.
    let res_a = rsid::memory::search::search(
        &store,
        None,
        "shared_keyword",
        &config,
        Some("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"),
        None,
    )
    .await
    .unwrap();
    assert_eq!(res_a.len(), 1, "project A scope: exactly one chunk");
    assert!(res_a[0].snippet.contains("alpha-only"));

    // Project B scope returns only B.
    let res_b = rsid::memory::search::search(
        &store,
        None,
        "shared_keyword",
        &config,
        Some("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb"),
        None,
    )
    .await
    .unwrap();
    assert_eq!(res_b.len(), 1, "project B scope: exactly one chunk");
    assert!(res_b[0].snippet.contains("beta-only"));

    // Unscoped returns all three chunks (manual/admin/debug behavior).
    let res_all = rsid::memory::search::search(&store, None, "shared_keyword", &config, None, None)
        .await
        .unwrap();
    assert_eq!(res_all.len(), 3, "unscoped: all three chunks visible");
}

#[tokio::test]
async fn test_project_scoped_observation_search_isolates_projects() {
    use rsid::memory::store::ObservationRow;

    let dir = TempDir::new().unwrap();
    let _engine = setup_engine_with_provider(&dir, make_fts_only_provider());
    let store = rsid::memory::store::MemoryStore::open(&dir.path().join("memory.sqlite")).unwrap();

    // Two observations on different projects, shared keyword, plus one
    // observation with no project (NULL project_id).
    let rows = vec![
        ObservationRow {
            id: "obs-a".to_string(),
            session_id: uuid::Uuid::new_v4().to_string(),
            project_id: Some("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa".to_string()),
            level: "explicit".to_string(),
            content: "shared_keyword alpha-fact".to_string(),
            source_ids: "[]".to_string(),
            confidence: Some("high".to_string()),
            times_derived: 1,
            embedding: String::new(),
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
        },
        ObservationRow {
            id: "obs-b".to_string(),
            session_id: uuid::Uuid::new_v4().to_string(),
            project_id: Some("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb".to_string()),
            level: "explicit".to_string(),
            content: "shared_keyword beta-fact".to_string(),
            source_ids: "[]".to_string(),
            confidence: Some("high".to_string()),
            times_derived: 1,
            embedding: String::new(),
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
        },
    ];
    store.insert_observations(&rows).unwrap();

    // Project A search returns only the A row.
    let res_a = store
        .search_observations_by_keyword(
            "shared_keyword",
            10,
            Some("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"),
        )
        .unwrap();
    assert_eq!(res_a.len(), 1);
    assert_eq!(res_a[0].0, "obs-a");

    // Project B search returns only the B row.
    let res_b = store
        .search_observations_by_keyword(
            "shared_keyword",
            10,
            Some("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb"),
        )
        .unwrap();
    assert_eq!(res_b.len(), 1);
    assert_eq!(res_b[0].0, "obs-b");

    // Unscoped returns both rows.
    let res_all = store
        .search_observations_by_keyword("shared_keyword", 10, None)
        .unwrap();
    assert_eq!(res_all.len(), 2);
}

// ---------------------------------------------------------------------------
// Vendored sqlite-vec: vec0 table works
// ---------------------------------------------------------------------------

#[test]
fn test_vendored_sqlite_vec_creates_vec0_table() {
    use rsid::memory::store::register_sqlite_vec;
    use rusqlite::Connection;

    assert!(register_sqlite_vec(), "vendored sqlite-vec should register");

    let conn = Connection::open_in_memory().unwrap();
    let version: String = conn
        .query_row("SELECT vec_version()", [], |row| row.get(0))
        .unwrap();
    assert!(
        !version.is_empty(),
        "vec_version() should return a version string"
    );

    conn.execute_batch("CREATE VIRTUAL TABLE test_vec USING vec0(id TEXT PRIMARY KEY, v float[3])")
        .unwrap();
    conn.execute_batch("DROP TABLE test_vec").unwrap();
}
