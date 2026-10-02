//! Bounded SQLite source reads for Remote V1. The RPC owner supplies a read
//! transaction, project authorization, runtime observations and transport
//! lifetime guards. No function here exposes an agent control method.

mod cursor;
mod cursor_pages;
mod decisions_cursor;
mod detail;
mod history;
mod owner;
mod pending;
mod permit;
mod projection;
mod projects;
mod responses;
mod rpc_owner;
mod selected_session;
mod sessions;
mod store_budget;

#[allow(unused_imports)] // The RPC owner retains this signer across Remote reads.
pub use cursor::RemoteCursorSigner;
#[allow(unused_imports)] // The RPC owner signs positions after bounded source selection.
pub use decisions_cursor::DecisionsPositionV1;
#[allow(unused_imports)] // The RPC owner consumes this detail read after dispatch integration.
pub use detail::{SessionDetailSource, session_detail_source};
#[allow(unused_imports)] // The RPC owner consumes these exports after dispatch integration.
pub use history::{
    HistoryObservedPage, HistoryRange, HistoryRow, history_page, observed_history_page,
    tool_pair_ambiguous,
};
#[allow(unused_imports)]
// The RPC owner connects trusted scope to this worker after dispatch admission.
pub use owner::{
    InitialSavedDecisionsSources, ResumedSavedDecisionsSources, spawn_decisions_read,
    spawn_history_read, spawn_info_read, spawn_initial_saved_decisions_page,
    spawn_initial_saved_decisions_sources, spawn_projects_read, spawn_resumed_saved_decisions_page,
    spawn_resumed_saved_decisions_sources, spawn_runtime_only_decisions_page,
    spawn_selected_session_read, spawn_sessions_read,
};
#[allow(unused_imports)]
pub use pending::{
    DurablePendingSelection, NativeRuntimeApprovalListRecheckObservation,
    NativeRuntimeApprovalListSnapshot, NativeRuntimeApprovalListState,
    NativeRuntimeApprovalRecheck, NativeRuntimeApprovalRecheckObservation,
    NativeRuntimeApprovalRow, NativeRuntimeApprovalSnapshot, NativeRuntimeApprovalState,
    PendingCandidate, PendingKey, PendingSource, PendingSourceRow, PendingUnionCandidate,
    PendingUnionSelection, QuestionSlotGeneration, QuestionSlotMirror, SelectedPendingSource,
    durable_question_fallback, pending_candidate_hydrate, pending_source_exact,
    pending_source_key_page, pending_source_page, select_durable_pending_keys,
    select_pending_union_keys, selected_pending_source,
};
#[allow(unused_imports)] // The RPC owner carries completed work through its socket boundary.
pub use permit::{RemoteReadBudget, RemoteReadCompleted, RemoteReadLimiter};
#[allow(unused_imports)] // The RPC owner consumes this projection after dispatch integration.
pub use projection::{
    PendingAcquiredSources, PendingCoverageInputs, PendingFinishedPage, PendingPageCandidate,
    PendingPageInputs, PendingPagePosition, PendingPageSelection, PendingPreparedPage, PendingRead,
    PendingResponseInputs, PendingResponseStatus, PendingStoreSources, PendingUnionProjection,
    QuestionProjection, RuntimeOnlyPendingCapture, RuntimeOnlyPendingSources,
    RuntimeOnlyPreparedPage, RuntimeQuestionSlotListEntry,
    RuntimeQuestionSlotListRecheckObservation, RuntimeQuestionSlotListSnapshot,
    RuntimeQuestionSlotRecheck, RuntimeQuestionSlotRecheckObservation, RuntimeQuestionSlotSnapshot,
    RuntimeQuestionSlotState, SavedPendingRuntimeCapture, approval_display,
    capture_runtime_only_pending_sources, capture_saved_pending_runtime_sources,
    durable_approval_summary, durable_question_summary, finish_runtime_only_pending_sources,
    finish_saved_pending_sources, history_event, missing_selected_decision, native_approval_union,
    pending_decisions_response, pending_response_status, pending_source_coverage,
    pending_store_sources, prepare_pending_page, project, project_pending_page_candidates,
    project_pending_union_keys, project_questions, runtime_native_approval_summary,
    runtime_question_slot_summary, select_pending_page_keys, selected_runtime_only_decision,
    selected_saved_decision, selected_saved_pending_sources, session_attention, session_detail,
    session_summary,
};
#[allow(unused_imports)] // The RPC owner consumes this source after dispatch integration.
pub use projects::{ProjectRow, list_projects};
#[allow(unused_imports)] // The RPC owner signs cursors and forwards this DTO.
pub use responses::{
    SessionsResponseSources, info_response, observed_history_response, projects_response,
    selected_session_response, sessions_response,
};
pub(crate) use rpc_owner::{is_operator_read_method, send_operator_read};
pub(crate) use selected_session::runtime_summary_row;
#[allow(unused_imports)]
// The RPC owner drives capture, Store read and reread around this seam.
pub use selected_session::{
    SelectedRuntimeOnlySession, SelectedRuntimeSession, SelectedSavedSession,
    SelectedSessionSources, finish_runtime_only_session_response, finish_selected_session_response,
    selected_runtime_store_miss, selected_saved_session,
};
#[allow(unused_imports)]
pub use sessions::{
    RuntimeSessionCandidate, RuntimeSessionCandidateSnapshot, RuntimeSessionObservation,
    RuntimeSessionRecheck, RuntimeSessionRecheckObservation, SessionCandidateOrigin,
    SessionCandidateSelection, SessionRow, get_session, list_merged_session_candidates,
    list_sessions, selected_session_coverage,
};
#[allow(unused_imports)]
// The RPC owner supplies an acquired Store lock in its blocking worker.
pub use store_budget::with_store_budget;

use rusqlite::{Connection, DatabaseName};
use std::io::Read;
use uuid::Uuid;

pub const MAX_TEXT_BYTES: usize = 8192;

#[derive(Debug)]
pub enum ReadError {
    NotFound,
    InvalidSource,
    StaleCursor,
    Admission,
    Busy,
    SourceUnavailable,
    ResourceLimit,
    Sql(rusqlite::Error),
    Io(std::io::Error),
}

impl From<rusqlite::Error> for ReadError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sql(error)
    }
}

impl From<std::io::Error> for ReadError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

pub type Result<T> = std::result::Result<T, ReadError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundedText {
    pub text: String,
    pub observed_bytes: u64,
    pub truncated: bool,
}

/// SQLite `substr` caps characters, so the intermediate can hold at most
/// 4*(cap+1) bytes. The caller must pass a fixed cap, never a request value.
pub(crate) fn bounded_text(prefix: String, observed_bytes: i64, cap: usize) -> Result<BoundedText> {
    let observed_bytes = u64::try_from(observed_bytes).map_err(|_| ReadError::InvalidSource)?;
    let mut end = cap.min(prefix.len());
    while !prefix.is_char_boundary(end) {
        end -= 1;
    }
    let text = prefix[..end].to_owned();
    Ok(BoundedText {
        truncated: observed_bytes > text.len() as u64,
        text,
        observed_bytes,
    })
}

/// Read a selected TEXT/BLOB column through SQLite's incremental BLOB API.
/// Table and column names are fixed at call sites; no SQL expression may be
/// substituted here. The handle is closed before the read transaction ends.
pub(crate) fn bounded_column(
    conn: &Connection,
    table: &'static str,
    column: &'static str,
    rowid: i64,
    cap: usize,
) -> Result<BoundedText> {
    let mut blob = conn.blob_open(DatabaseName::Main, table, column, rowid, true)?;
    let observed = blob.len();
    let mut bytes = vec![0_u8; observed.min(cap)];
    blob.read_exact(&mut bytes)?;
    blob.close()?;
    let valid = match std::str::from_utf8(&bytes) {
        Ok(_) => bytes.len(),
        Err(error) if error.error_len().is_none() && observed > bytes.len() => error.valid_up_to(),
        Err(_) => return Err(ReadError::InvalidSource),
    };
    let text = String::from_utf8(bytes[..valid].to_vec()).map_err(|_| ReadError::InvalidSource)?;
    Ok(BoundedText {
        text,
        observed_bytes: observed as u64,
        truncated: observed > valid,
    })
}

pub(crate) fn source_uuid(raw: &str) -> Result<Uuid> {
    let id = Uuid::parse_str(raw).map_err(|_| ReadError::InvalidSource)?;
    if id.hyphenated().to_string() != raw {
        return Err(ReadError::InvalidSource);
    }
    Ok(id)
}

/// Check ownership at the source, even when a caller has already checked it.
pub fn require_session_project(conn: &Connection, project: Uuid, session: Uuid) -> Result<()> {
    let owner: Option<String> = conn
        .query_row(
            "SELECT project_id FROM sessions WHERE id=?1 AND status NOT IN ('Archived','Deleted') AND COALESCE(is_eval,0)=0",
            [session.to_string()],
            |row| row.get(0),
        )
        .map_err(|error| match error {
            rusqlite::Error::QueryReturnedNoRows => ReadError::NotFound,
            other => ReadError::Sql(other),
        })?;
    if owner.as_deref() != Some(project.to_string().as_str()) {
        return Err(ReadError::NotFound);
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourcePage<T, K> {
    pub items: Vec<T>,
    pub next: Option<K>,
    /// More means an observed additional candidate, not a frozen snapshot.
    pub has_more: bool,
}

#[cfg(all(
    test,
    any(not(feature = "test-shard-mode"), feature = "test-shard-store-04")
))]
mod tests {
    use super::*;
    use rusqlite::params;

    fn fixture() -> (Connection, Uuid, Uuid) {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE projects(id TEXT PRIMARY KEY);
             CREATE TABLE sessions(id TEXT PRIMARY KEY,project_id TEXT,status TEXT,is_eval INTEGER,
               parent_id TEXT,continued_from TEXT,session_kind TEXT,provider TEXT,updated_at TEXT,
               title TEXT,query TEXT,pending_question_json TEXT);
             CREATE INDEX manager_project_scope_candidates ON sessions(project_id,session_kind,id);
             CREATE TABLE conversation_events(id INTEGER PRIMARY KEY,session_id TEXT,sequence INTEGER,
               event_type TEXT,role TEXT,created_at TEXT,content TEXT,tool_name TEXT,tool_use_id TEXT);
             CREATE INDEX idx_events_session_sequence ON conversation_events(session_id,sequence);
             CREATE TABLE offloaded_content(session_id TEXT,event_sequence INTEGER);
             CREATE INDEX idx_offload_session_seq ON offloaded_content(session_id,event_sequence);
             CREATE TABLE pending_question_publications(publication_id TEXT PRIMARY KEY,session_id TEXT,
               state TEXT,epoch INTEGER,question_json TEXT);
             CREATE TABLE appserver_approval_publications(publication_id TEXT PRIMARY KEY,session_id TEXT,
               state TEXT,closure_state TEXT,incarnation_id TEXT,target_json TEXT,approval_id TEXT);
             CREATE INDEX appserver_approval_publications_session ON appserver_approval_publications(session_id,publication_id);
             CREATE TABLE pending_appserver_approvals(publication_id TEXT PRIMARY KEY,session_id TEXT,
               state TEXT,incarnation_id TEXT,target_json TEXT,approval_id TEXT);
             CREATE TABLE approvals(id TEXT PRIMARY KEY,session_id TEXT,status TEXT,tool_name TEXT,tool_input TEXT);
             CREATE INDEX idx_approvals_session_id ON approvals(session_id);",
        ).unwrap();
        let project = Uuid::new_v4();
        let session = Uuid::new_v4();
        conn.execute("INSERT INTO projects(id) VALUES(?1)", [project.to_string()])
            .unwrap();
        conn.execute("INSERT INTO sessions(id,project_id,status,is_eval,session_kind,provider,updated_at,title,query) VALUES(?1,?2,'Running',0,'Standard','Codex','2026-09-26T00:00:00.000000000Z',?3,'query')",
            params![session.to_string(),project.to_string(),"é🙂".repeat(300)]).unwrap();
        (conn, project, session)
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn unicode_prefix_is_byte_bounded() {
        let projected = bounded_text("é🙂abc".into(), 9, 5).unwrap();
        assert_eq!(projected.text, "é");
        assert_eq!(projected.observed_bytes, 9);
        assert!(projected.truncated);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn session_page_preserves_identity_and_project_scope() {
        let (conn, project, session) = fixture();
        let page = list_sessions(&conn, project, None, 1).unwrap();
        assert_eq!(page.items[0].id, session);
        assert!(page.items[0].own_title.truncated);
        assert!(
            page.items[0]
                .own_title
                .text
                .is_char_boundary(page.items[0].own_title.text.len())
        );
        assert!(matches!(
            require_session_project(&conn, Uuid::new_v4(), session),
            Err(ReadError::NotFound)
        ));
        conn.execute(
            "UPDATE sessions SET title='',query='' WHERE id=?1",
            [session.to_string()],
        )
        .unwrap();
        let fallback = list_sessions(&conn, project, None, 1).unwrap();
        assert_eq!(fallback.items[0].own_title.text, "Untitled session");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn exact_session_read_keeps_scope_and_unknown_kind() {
        let (conn, project, session) = fixture();
        conn.execute(
            "UPDATE sessions SET session_kind='FutureKind' WHERE id=?1",
            [session.to_string()],
        )
        .unwrap();
        let exact = get_session(&conn, project, session).unwrap();
        assert_eq!(exact.id, session);
        assert_eq!(exact.kind, "FutureKind");
        let listed = list_sessions(&conn, project, None, 1).unwrap();
        assert_eq!(listed.items[0].kind, "FutureKind");
        assert!(exact.own_title.truncated);
        assert!(matches!(
            get_session(&conn, Uuid::new_v4(), session),
            Err(ReadError::NotFound)
        ));
        conn.execute(
            "UPDATE sessions SET status='Archived' WHERE id=?1",
            [session.to_string()],
        )
        .unwrap();
        assert!(matches!(
            get_session(&conn, project, session),
            Err(ReadError::NotFound)
        ));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    #[allow(clippy::unwrap_used)] // Local SQLite fixture follows the module's existing test style.
    fn session_kind_discovery_keeps_future_kinds_and_bounds_the_source() {
        let (conn, _, _) = fixture();
        let project = Uuid::new_v4();
        conn.execute("INSERT INTO projects(id) VALUES(?1)", [project.to_string()])
            .unwrap();
        let id = |n: u8| Uuid::parse_str(&format!("00000000-0000-0000-0000-{n:012x}")).unwrap();
        let insert = |project: Uuid, n: u8, kind: &str| {
            conn.execute(
                "INSERT INTO sessions(id,project_id,status,is_eval,session_kind,provider,updated_at,title,query)
                 VALUES(?1,?2,'Running',0,?3,'Codex','2026-09-26T00:00:00.000000000Z','title','query')",
                params![id(n).to_string(), project.to_string(), kind],
            )
            .unwrap();
        };
        insert(project, 1, "Standard");
        insert(project, 3, "FutureKind");
        let first = list_sessions(&conn, project, None, 1).unwrap();
        assert_eq!(first.items[0].id, id(1));
        assert!(first.has_more);
        // A new kind inserted past the examined cursor appears on the next
        // page. This is keyset traversal, not a frozen snapshot.
        insert(project, 2, "LaterKind");
        let second = list_sessions(&conn, project, first.next, 1).unwrap();
        assert_eq!(second.items[0].id, id(2));
        let third = list_sessions(&conn, project, second.next, 1).unwrap();
        assert_eq!(third.items[0].id, id(3));
        assert_eq!(third.items[0].kind, "FutureKind");
        assert!(!third.has_more);

        let cap_project = Uuid::new_v4();
        conn.execute(
            "INSERT INTO projects(id) VALUES(?1)",
            [cap_project.to_string()],
        )
        .unwrap();
        for n in 0..64 {
            insert(cap_project, n + 100, &format!("Kind{n:02}"));
        }
        assert!(list_sessions(&conn, cap_project, None, 1).is_ok());
        insert(cap_project, 164, "Kind64");
        assert!(matches!(
            list_sessions(&conn, cap_project, None, 1),
            Err(ReadError::ResourceLimit)
        ));
        let long_kind_project = Uuid::new_v4();
        conn.execute(
            "INSERT INTO projects(id) VALUES(?1)",
            [long_kind_project.to_string()],
        )
        .unwrap();
        insert(long_kind_project, 200, &"é".repeat(70));
        assert!(matches!(
            list_sessions(&conn, long_kind_project, None, 1),
            Err(ReadError::ResourceLimit)
        ));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn session_candidate_union_keeps_precedence_and_empty_continuation() {
        use rsi_common::types::SessionStatus;
        let (conn, project, _) = fixture();
        conn.execute("DELETE FROM sessions", []).unwrap();
        let id = |n: u8| Uuid::parse_str(&format!("00000000-0000-0000-0000-{n:012x}")).unwrap();
        for (n, status) in [(2, "Archived"), (3, "Running"), (5, "Running")] {
            conn.execute("INSERT INTO sessions(id,project_id,status,is_eval,session_kind,provider,updated_at,title,query) VALUES(?1,?2,?3,0,'Standard','Codex','2026-09-26T00:00:00.000000000Z','title','query')",
                params![id(n).to_string(),project.to_string(),status]).unwrap();
        }
        let runtime = |n, status| RuntimeSessionCandidate {
            id: id(n),
            project_id: Some(project),
            status,
            is_eval: false,
            updated_at_seconds: 0,
            updated_at_nanosecond: 0,
            spawn_generation: None,
        };
        let active = [
            runtime(1, SessionStatus::Running),
            runtime(3, SessionStatus::Running),
        ];
        let completed = [
            runtime(3, SessionStatus::Completed),
            runtime(4, SessionStatus::Completed),
        ];
        let expected = [
            (Some((id(1), SessionCandidateOrigin::Active)), id(1)),
            (None, id(2)),
            (Some((id(3), SessionCandidateOrigin::Active)), id(3)),
            (Some((id(4), SessionCandidateOrigin::Completed)), id(4)),
            (Some((id(5), SessionCandidateOrigin::Store)), id(5)),
        ];
        let mut after = None;
        for (index, (emitted, key)) in expected.into_iter().enumerate() {
            let page =
                list_merged_session_candidates(&conn, project, after, 1, &active, &completed)
                    .unwrap();
            assert_eq!(
                page.items.first().map(|item| (item.id, item.origin)),
                emitted
            );
            assert_eq!(page.has_more, index < 4);
            assert_eq!(page.next, (index < 4).then_some(key));
            after = Some(key);
        }
        let mut foreign = active;
        foreign[1].project_id = Some(Uuid::new_v4());
        assert!(matches!(
            list_merged_session_candidates(&conn, project, None, 3, &foreign, &completed),
            Err(ReadError::SourceUnavailable)
        ));
        let over_cap = vec![runtime(1, SessionStatus::Running); 10_001];
        assert!(matches!(
            list_merged_session_candidates(&conn, project, None, 1, &over_cap, &[]),
            Err(ReadError::ResourceLimit)
        ));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn history_uses_signed_keyset_and_never_pairs_an_id_prefix() {
        let (conn, project, session) = fixture();
        for (id, sequence, tool_id) in [
            (i64::MAX - 1, -2, "x".repeat(257)),
            (i64::MAX, 7, "short".into()),
        ] {
            conn.execute("INSERT INTO conversation_events(id,session_id,sequence,event_type,role,created_at,content,tool_name,tool_use_id) VALUES(?1,?2,?3,'ToolUse','Assistant','2026-09-26T00:00:00.000000000Z',?4,'tool',?5)",
                params![id,session.to_string(),sequence,"🙂".repeat(3000),tool_id]).unwrap();
        }
        conn.execute(
            "INSERT INTO offloaded_content(session_id,event_sequence) VALUES(?1,-2)",
            [session.to_string()],
        )
        .unwrap();
        let latest = history_page(&conn, project, session, HistoryRange::Latest, 1).unwrap();
        assert_eq!(latest.items[0].id, i64::MAX);
        assert!(latest.has_more);
        let older = history_page(
            &conn,
            project,
            session,
            HistoryRange::Older {
                anchor: latest.next.unwrap(),
            },
            1,
        )
        .unwrap();
        assert_eq!(older.items[0].id, i64::MAX - 1);
        assert_eq!(older.items[0].tool_pair_key, None);
        assert!(older.items[0].tool_id_display.as_ref().unwrap().truncated);
        assert!(older.items[0].content.truncated);
        assert!(older.items[0].offloaded);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn history_keyset_uses_index_across_100k_tied_events() {
        let (conn, project, session) = fixture();
        conn.execute(
            "WITH RECURSIVE n(x) AS (
                SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<100000
             )
             INSERT INTO conversation_events(id,session_id,sequence,event_type,role,created_at,content)
             SELECT x,?1,(x-1)/2,'Message','Assistant','2026-09-26T00:00:00.000000000Z','v'
             FROM n",
            [session.to_string()],
        )
        .unwrap();
        let plan: Vec<String> = conn
            .prepare(
                "EXPLAIN QUERY PLAN SELECT id,sequence FROM conversation_events
                 WHERE session_id=?1 ORDER BY sequence DESC,id DESC LIMIT 3",
            )
            .unwrap()
            .query_map([session.to_string()], |row| row.get(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(
            plan.iter()
                .any(|step| step.contains("idx_events_session_sequence"))
        );

        let latest = history_page(&conn, project, session, HistoryRange::Latest, 2).unwrap();
        assert_eq!(
            latest.items.iter().map(|row| row.id).collect::<Vec<_>>(),
            [99999, 100000]
        );
        assert_eq!(latest.next, Some((49999, 99999)));
        let older = history_page(
            &conn,
            project,
            session,
            HistoryRange::Older {
                anchor: latest.next.unwrap(),
            },
            2,
        )
        .unwrap();
        assert_eq!(
            older.items.iter().map(|row| row.id).collect::<Vec<_>>(),
            [99997, 99998]
        );
        assert_eq!(older.next, Some((49998, 99997)));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn legacy_pending_is_visible_without_conversation_history() {
        let (conn, project, session) = fixture();
        let approval = Uuid::new_v4();
        conn.execute("INSERT INTO approvals(id,session_id,status,tool_name,tool_input) VALUES(?1,?2,'Pending','Bash','{\"secret\":1}')",
            params![approval.to_string(),session.to_string()]).unwrap();
        let page = pending_source_page(
            &conn,
            project,
            session,
            PendingSource::LegacyApprovals,
            None,
            16,
            false,
        )
        .unwrap();
        assert_eq!(page.items[0].id, approval);
        assert_eq!(page.items[0].tool_name.as_ref().unwrap().text, "Bash");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn filtered_legacy_page_continues_after_examined_rowid() {
        let (conn, project, session) = fixture();
        let closed = Uuid::new_v4();
        let open = Uuid::new_v4();
        for (id, status) in [(closed, "Approved"), (open, "Pending")] {
            conn.execute("INSERT INTO approvals(id,session_id,status,tool_name,tool_input) VALUES(?1,?2,?3,'Bash','{}')",
                params![id.to_string(),session.to_string(),status]).unwrap();
        }
        let first = pending_source_page(
            &conn,
            project,
            session,
            PendingSource::LegacyApprovals,
            None,
            1,
            false,
        )
        .unwrap();
        assert!(first.items.is_empty());
        assert!(first.has_more);
        let second = pending_source_page(
            &conn,
            project,
            session,
            PendingSource::LegacyApprovals,
            first.next,
            1,
            false,
        )
        .unwrap();
        assert_eq!(second.items[0].id, open);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn pending_sources_keep_native_occurrences_and_question_slot() {
        let (conn, project, session) = fixture();
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let incarnation = Uuid::new_v4();
        for (id, state) in [(first, "enqueued"), (second, "expired")] {
            conn.execute("INSERT INTO appserver_approval_publications(publication_id,session_id,state,closure_state,incarnation_id,target_json,approval_id) VALUES(?1,?2,?3,'open',?4,?5,?6)",
                params![id.to_string(),session.to_string(),state,incarnation.to_string(),
                    r#"{"method":"exec","description":"Review this","params":{"private":"value"}}"#,id.to_string()]).unwrap();
        }
        let page = pending_source_page(
            &conn,
            project,
            session,
            PendingSource::NativePublications,
            None,
            32,
            false,
        )
        .unwrap();
        assert_eq!(page.items.len(), 2);
        assert_ne!(page.items[0].id, page.items[1].id);
        assert!(
            page.items
                .iter()
                .all(|item| item.method.as_ref().unwrap().text == "exec")
        );
        assert!(
            page.items
                .iter()
                .all(|item| item.closure_state.as_deref() == Some("open"))
        );
        conn.execute("UPDATE sessions SET pending_question_json=?1 WHERE id=?2", params![
            r#"{"questions":[{"question":"Deploy?","header":"Action","options":[{"label":"Yes","description":"Proceed"}],"multiSelect":false}]}"#,
            session.to_string()
        ]).unwrap();
        let fallback = durable_question_fallback(&conn, project, session)
            .unwrap()
            .unwrap();
        assert_eq!(fallback.id, session);
        assert!(fallback.question_json.unwrap().contains("multiSelect"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn exact_pending_read_survives_filter_and_suppresses_native_mirrors() {
        let (conn, project, session) = fixture();
        let before = Uuid::parse_str("00000000-0000-4000-8000-000000000001").unwrap();
        let question = Uuid::parse_str("ffffffff-ffff-4fff-8fff-ffffffffffff").unwrap();
        let publication = Uuid::new_v4();
        let approval = Uuid::new_v4();
        let incarnation = Uuid::new_v4();
        let historical_only = Uuid::new_v4();
        let legacy_only = Uuid::new_v4();
        conn.execute("INSERT INTO pending_question_publications(publication_id,session_id,state,epoch,question_json) VALUES(?1,?2,'unresolved',1,?3)",
            params![before.to_string(),session.to_string(),r#"{"questions":[{"question":"First?","header":"Choice","options":[],"multiSelect":false}]}"#]).unwrap();
        conn.execute("INSERT INTO pending_question_publications(publication_id,session_id,state,epoch,question_json) VALUES(?1,?2,'cleared',1,?3)",
            params![question.to_string(),session.to_string(),r#"{"questions":[{"question":"Continue?","header":"Choice","options":[],"multiSelect":false}]}"#]).unwrap();
        conn.execute("INSERT INTO appserver_approval_publications(publication_id,session_id,state,closure_state,incarnation_id,target_json,approval_id) VALUES(?1,?2,'superseded','closed',?3,?5,?4)",
            params![publication.to_string(),session.to_string(),incarnation.to_string(),approval.to_string(),r#"{"method":"exec"}"#]).unwrap();
        conn.execute("INSERT INTO pending_appserver_approvals(publication_id,session_id,state,incarnation_id,target_json,approval_id) VALUES(?1,?2,'published',?3,?5,?4)",
            params![publication.to_string(),session.to_string(),incarnation.to_string(),approval.to_string(),r#"{"method":"exec"}"#]).unwrap();
        conn.execute("INSERT INTO approvals(id,session_id,status,tool_name,tool_input) VALUES(?1,?2,'Approved','Bash','{}')",
            params![approval.to_string(),session.to_string()]).unwrap();
        conn.execute("INSERT INTO pending_appserver_approvals(publication_id,session_id,state,incarnation_id,target_json,approval_id) VALUES(?1,?2,'published',?3,?4,?5)",
            params![historical_only.to_string(),session.to_string(),incarnation.to_string(),r#"{"method":"historical"}"#,Uuid::new_v4().to_string()]).unwrap();
        conn.execute("INSERT INTO approvals(id,session_id,status,tool_name,tool_input) VALUES(?1,?2,'Approved','Read','{}')",
            params![legacy_only.to_string(),session.to_string()]).unwrap();

        let attention = pending_source_page(
            &conn,
            project,
            session,
            PendingSource::Questions,
            None,
            1,
            false,
        )
        .unwrap();
        assert_eq!(attention.items[0].id, before);
        assert!(attention.has_more);
        let selected_question =
            pending_source_exact(&conn, project, session, PendingSource::Questions, question)
                .unwrap()
                .unwrap();
        assert_eq!(selected_question.state, "cleared");
        assert!(selected_question.question_json.is_some());
        let selected_native = pending_source_exact(
            &conn,
            project,
            session,
            PendingSource::NativePublications,
            publication,
        )
        .unwrap()
        .unwrap();
        assert_eq!(selected_native.closure_state.as_deref(), Some("closed"));
        assert!(
            pending_source_exact(
                &conn,
                project,
                session,
                PendingSource::NativeHistorical,
                publication
            )
            .unwrap()
            .is_none()
        );
        assert!(
            pending_source_exact(
                &conn,
                project,
                session,
                PendingSource::LegacyApprovals,
                approval
            )
            .unwrap()
            .is_none()
        );
        assert_eq!(
            pending_source_exact(
                &conn,
                project,
                session,
                PendingSource::NativeHistorical,
                historical_only
            )
            .unwrap()
            .unwrap()
            .method
            .unwrap()
            .text,
            "historical"
        );
        assert_eq!(
            pending_source_exact(
                &conn,
                project,
                session,
                PendingSource::LegacyApprovals,
                legacy_only
            )
            .unwrap()
            .unwrap()
            .tool_name
            .unwrap()
            .text,
            "Read"
        );
        assert!(
            pending_source_exact(
                &conn,
                Uuid::new_v4(),
                session,
                PendingSource::Questions,
                question
            )
            .is_err()
        );
        assert!(
            pending_source_exact(
                &conn,
                project,
                session,
                PendingSource::Questions,
                Uuid::new_v4()
            )
            .unwrap()
            .is_none()
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn selected_pending_id_routes_exact_kind_and_scope() {
        use rsi_common::remote_read::DecisionId;
        let (conn, project, session) = fixture();
        let id = Uuid::new_v4();
        let historical = Uuid::new_v4();
        let legacy = Uuid::new_v4();
        let incarnation = Uuid::new_v4();
        conn.execute("INSERT INTO pending_question_publications(publication_id,session_id,state,epoch,question_json) VALUES(?1,?2,'cleared',1,NULL)",
            params![id.to_string(),session.to_string()]).unwrap();
        for native in [id, historical] {
            conn.execute("INSERT INTO pending_appserver_approvals(publication_id,session_id,state,incarnation_id,target_json,approval_id) VALUES(?1,?2,'published',?3,NULL,?4)",
                params![native.to_string(),session.to_string(),incarnation.to_string(),Uuid::new_v4().to_string()]).unwrap();
        }
        conn.execute("INSERT INTO appserver_approval_publications(publication_id,session_id,state,closure_state,incarnation_id,target_json,approval_id) VALUES(?1,?2,'enqueued','open',?3,NULL,?4)",
            params![id.to_string(),session.to_string(),incarnation.to_string(),Uuid::new_v4().to_string()]).unwrap();
        conn.execute("INSERT INTO approvals(id,session_id,status,tool_name,tool_input) VALUES(?1,?2,'Approved','Read','{}')",
            params![legacy.to_string(),session.to_string()]).unwrap();
        let foreign_session = Uuid::new_v4();
        conn.execute("INSERT INTO appserver_approval_publications(publication_id,session_id,state,closure_state,incarnation_id,target_json,approval_id) VALUES(?1,?2,'published','open',?3,NULL,?4)",
            params![historical.to_string(),foreign_session.to_string(),incarnation.to_string(),legacy.to_string()]).unwrap();
        conn.execute("UPDATE sessions SET pending_question_json=?1 WHERE id=?2",
            params![r#"{"questions":[{"question":"Fallback?","header":"Choice","options":[],"multiSelect":false}]}"#,session.to_string()]).unwrap();
        let selected = |text: String| DecisionId::new(text).unwrap();
        assert!(
            matches!(selected_pending_source(&conn, project, session, &selected(format!("question:{id}"))).unwrap(),
            SelectedPendingSource::Durable { source: PendingSource::Questions, row } if row.id == id)
        );
        assert!(
            matches!(selected_pending_source(&conn, project, session, &selected(format!("native:{id}"))).unwrap(),
            SelectedPendingSource::Durable { source: PendingSource::NativePublications, row } if row.id == id)
        );
        assert!(matches!(
            selected_pending_source(
                &conn,
                project,
                session,
                &selected(format!("native:{historical}"))
            )
            .unwrap(),
            SelectedPendingSource::Durable {
                source: PendingSource::NativeHistorical,
                ..
            }
        ));
        assert!(matches!(
            selected_pending_source(
                &conn,
                project,
                session,
                &selected(format!("legacy:{legacy}"))
            )
            .unwrap(),
            SelectedPendingSource::Durable {
                source: PendingSource::LegacyApprovals,
                ..
            }
        ));
        assert!(
            matches!(selected_pending_source(&conn, project, session, &selected(format!("question-fallback:{session}"))).unwrap(),
            SelectedPendingSource::QuestionFallback(row) if row.id == session)
        );
        assert!(matches!(
            selected_pending_source(
                &conn,
                project,
                session,
                &selected(format!("question-slot:{session}:7:tracked"))
            )
            .unwrap(),
            SelectedPendingSource::RuntimeSlot {
                generation: QuestionSlotGeneration::Spawn(7),
                mirror: QuestionSlotMirror::Tracked
            }
        ));
        assert!(matches!(
            selected_pending_source(
                &conn,
                project,
                session,
                &selected(format!("question-slot:{session}:completed:session"))
            )
            .unwrap(),
            SelectedPendingSource::RuntimeSlot {
                generation: QuestionSlotGeneration::Completed,
                mirror: QuestionSlotMirror::Session
            }
        ));
        assert!(matches!(
            selected_pending_source(
                &conn,
                project,
                session,
                &selected(format!("question:{}", Uuid::new_v4()))
            )
            .unwrap(),
            SelectedPendingSource::Missing
        ));
        assert!(matches!(
            selected_pending_source(
                &conn,
                project,
                session,
                &selected(format!("question-fallback:{}", Uuid::new_v4()))
            ),
            Err(ReadError::NotFound)
        ));
        assert!(matches!(
            selected_pending_source(
                &conn,
                Uuid::new_v4(),
                session,
                &selected(format!("question:{id}"))
            ),
            Err(ReadError::NotFound)
        ));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn unreadable_question_carrier_keeps_a_visible_publication() {
        let (conn, project, session) = fixture();
        let publication = Uuid::new_v4();
        conn.execute("INSERT INTO pending_question_publications(publication_id,session_id,state,epoch,question_json) VALUES(?1,?2,'unresolved',1,?3)",
            params![publication.to_string(),session.to_string(),"x".repeat(65_537)]).unwrap();
        let page = pending_source_page(
            &conn,
            project,
            session,
            PendingSource::Questions,
            None,
            16,
            false,
        )
        .unwrap();
        assert_eq!(page.items[0].id, publication);
        assert!(page.items[0].details_unavailable);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn pending_key_scan_skips_large_payload_until_chosen_and_rechecks_identity() {
        let (conn, project, session) = fixture();
        let first = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
        let second = Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap();
        for (id, state) in [
            (first, "x".repeat(32 * 1024 * 1024)),
            (second, "published".into()),
        ] {
            conn.execute(
                "INSERT INTO pending_question_publications(publication_id,session_id,state,epoch,question_json) VALUES(?1,?2,?3,1,NULL)",
                params![id.to_string(), session.to_string(), state],
            )
            .unwrap();
        }
        let first_page =
            pending_source_key_page(&conn, project, session, PendingSource::Questions, None, 1)
                .unwrap();
        assert_eq!(first_page.items[0].id, first);
        assert!(first_page.has_more);
        assert_eq!(first_page.next, Some(PendingKey::Id(first)));
        assert!(matches!(
            pending_candidate_hydrate(&conn, project, session, first_page.items[0], false),
            Err(ReadError::InvalidSource)
        ));
        let second_page = pending_source_key_page(
            &conn,
            project,
            session,
            PendingSource::Questions,
            first_page.next,
            1,
        )
        .unwrap();
        assert_eq!(second_page.items[0].id, second);
        assert!(!second_page.has_more);
        assert_eq!(
            pending_candidate_hydrate(&conn, project, session, second_page.items[0], false)
                .unwrap()
                .unwrap()
                .id,
            second
        );
        let mut stale = second_page.items[0];
        stale.id = first;
        assert!(matches!(
            pending_candidate_hydrate(&conn, project, session, stale, false),
            Err(ReadError::SourceUnavailable)
        ));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn durable_pending_key_union_round_robins_and_consumes_exact_native_mirror() {
        let id = |n: u8| Uuid::parse_str(&format!("00000000-0000-0000-0000-{n:012x}")).unwrap();
        let candidate = |source, n, rowid| PendingCandidate {
            source,
            id: id(n),
            rowid,
        };
        let questions = [
            candidate(PendingSource::Questions, 1, 1),
            candidate(PendingSource::Questions, 4, 4),
        ];
        let publications = [
            candidate(PendingSource::NativePublications, 2, 2),
            candidate(PendingSource::NativePublications, 5, 5),
        ];
        let historical = [
            candidate(PendingSource::NativeHistorical, 2, 20),
            candidate(PendingSource::NativeHistorical, 3, 30),
        ];
        let legacy = [
            candidate(PendingSource::LegacyApprovals, 6, 1),
            candidate(PendingSource::LegacyApprovals, 7, 2),
        ];
        let first =
            select_durable_pending_keys(&questions, &publications, &historical, &legacy, 0, 4)
                .unwrap();
        assert_eq!(
            first.items.iter().map(|item| item.id).collect::<Vec<_>>(),
            vec![id(1), id(2), id(6), id(4)]
        );
        assert_eq!(first.items[1].source, PendingSource::NativePublications);
        assert_eq!(first.examined, [2, 1, 1, 1]);
        assert_eq!(first.next_bucket, 1);
        assert!(first.remaining_in_inputs);
        let second = select_durable_pending_keys(
            &questions[first.examined[0]..],
            &publications[first.examined[1]..],
            &historical[first.examined[2]..],
            &legacy[first.examined[3]..],
            first.next_bucket,
            4,
        )
        .unwrap();
        assert_eq!(
            second.items.iter().map(|item| item.id).collect::<Vec<_>>(),
            vec![id(3), id(7), id(5)]
        );
        assert!(!second.remaining_in_inputs);
        assert!(matches!(
            select_durable_pending_keys(&questions[..0], &publications, &historical, &legacy, 3, 1),
            Err(ReadError::InvalidSource)
        ));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn pending_key_union_joins_runtime_and_durable_native_mirrors_without_claiming_source_tails() {
        let id = |n: u8| Uuid::parse_str(&format!("00000000-0000-0000-0000-{n:012x}")).unwrap();
        let incarnation = Uuid::new_v4();
        let candidate = |source, n, rowid| PendingCandidate {
            source,
            id: id(n),
            rowid,
        };
        let runtime_row = |n| NativeRuntimeApprovalRow {
            id: id(n),
            incarnation_id: incarnation,
            spawn_generation: 3,
            method: None,
            description: None,
            resolution_observed: false,
            resolution_persisted: false,
            writer_live: true,
            writer_capacity: 61,
        };
        let runtime_rows = vec![runtime_row(2), runtime_row(3), runtime_row(8)];
        let snapshot = |rows| NativeRuntimeApprovalListSnapshot {
            session: Uuid::new_v4(),
            observed_at: chrono::Utc::now(),
            state: NativeRuntimeApprovalListState::Present(rows),
        };
        let runtime = snapshot(runtime_rows.clone());
        let questions = [
            candidate(PendingSource::Questions, 1, 1),
            candidate(PendingSource::Questions, 4, 4),
        ];
        let publications = [
            candidate(PendingSource::NativePublications, 2, 2),
            candidate(PendingSource::NativePublications, 5, 5),
        ];
        let historical = [
            candidate(PendingSource::NativeHistorical, 2, 20),
            candidate(PendingSource::NativeHistorical, 3, 30),
            candidate(PendingSource::NativeHistorical, 7, 70),
        ];
        let legacy = [
            candidate(PendingSource::LegacyApprovals, 6, 1),
            candidate(PendingSource::LegacyApprovals, 9, 2),
        ];
        let first = select_pending_union_keys(
            &runtime,
            NativeRuntimeApprovalRecheck::NoObservedChange,
            0,
            &questions,
            &publications,
            &historical,
            &legacy,
            0,
            4,
        )
        .unwrap();
        assert_eq!(
            first.items.iter().map(|item| item.id()).collect::<Vec<_>>(),
            vec![id(1), id(2), id(6), id(4)]
        );
        assert_eq!(
            first.items[1],
            PendingUnionCandidate::Native {
                id: id(2),
                runtime: true,
                durable: Some(publications[0]),
            }
        );
        assert_eq!(first.examined, [2, 1, 1, 1, 1]);
        assert!(first.remaining_in_inputs);
        let second = select_pending_union_keys(
            &runtime,
            NativeRuntimeApprovalRecheck::NoObservedChange,
            first.examined[1],
            &questions[first.examined[0]..],
            &publications[first.examined[2]..],
            &historical[first.examined[3]..],
            &legacy[first.examined[4]..],
            first.next_bucket,
            8,
        )
        .unwrap();
        assert_eq!(
            second
                .items
                .iter()
                .map(|item| item.id())
                .collect::<Vec<_>>(),
            vec![id(3), id(9), id(5), id(7), id(8)]
        );
        assert_eq!(
            second.items[0],
            PendingUnionCandidate::Native {
                id: id(3),
                runtime: true,
                durable: Some(historical[1]),
            }
        );
        assert_eq!(
            second.items[4],
            PendingUnionCandidate::Native {
                id: id(8),
                runtime: true,
                durable: None,
            }
        );
        assert!(!second.remaining_in_inputs);
        assert!(matches!(
            select_pending_union_keys(
                &runtime,
                NativeRuntimeApprovalRecheck::Changed,
                0,
                &questions,
                &publications,
                &historical,
                &legacy,
                0,
                1,
            ),
            Err(ReadError::SourceUnavailable)
        ));
        assert!(matches!(
            select_pending_union_keys(
                &snapshot(vec![runtime_row(3), runtime_row(2)]),
                NativeRuntimeApprovalRecheck::NoObservedChange,
                0,
                &questions,
                &publications,
                &historical,
                &legacy,
                0,
                1,
            ),
            Err(ReadError::InvalidSource)
        ));
        assert!(matches!(
            select_pending_union_keys(
                &runtime,
                NativeRuntimeApprovalRecheck::NoObservedChange,
                runtime_rows.len() + 1,
                &questions,
                &publications,
                &historical,
                &legacy,
                0,
                1,
            ),
            Err(ReadError::InvalidSource)
        ));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn large_text_is_read_through_bounded_blob_prefixes() {
        let (conn, project, session) = fixture();
        let huge = "🙂".repeat(8 * 1024 * 1024);
        let bytes = huge.len() as u64;
        conn.execute(
            "UPDATE sessions SET title=?1,pending_question_json=?1 WHERE id=?2",
            params![&huge, session.to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO conversation_events(session_id,sequence,event_type,role,created_at,content)
             VALUES(?1,1,'Assistant','Assistant','2026-09-26T00:00:00.000000000Z',?2)",
            params![session.to_string(), &huge],
        )
        .unwrap();
        let sessions = list_sessions(&conn, project, None, 1).unwrap();
        assert_eq!(sessions.items[0].own_title.observed_bytes, bytes);
        assert!(sessions.items[0].own_title.truncated);
        assert!(sessions.items[0].own_title.text.len() <= 512);
        let history = history_page(&conn, project, session, HistoryRange::Latest, 1).unwrap();
        assert_eq!(history.items[0].content.observed_bytes, bytes);
        assert!(history.items[0].content.truncated);
        assert!(history.items[0].content.text.len() <= 8192);
        let question = durable_question_fallback(&conn, project, session)
            .unwrap()
            .unwrap();
        assert!(question.details_unavailable);

        let publication = Uuid::new_v4();
        conn.execute(
            "INSERT INTO pending_question_publications(publication_id,session_id,state,epoch) VALUES(?1,?2,?3,1)",
            params![publication.to_string(), session.to_string(), &huge],
        )
        .unwrap();
        assert!(matches!(
            pending_source_page(
                &conn,
                project,
                session,
                PendingSource::Questions,
                None,
                1,
                false
            ),
            Err(ReadError::InvalidSource)
        ));
        assert!(matches!(
            pending_source_exact(
                &conn,
                project,
                session,
                PendingSource::Questions,
                publication
            ),
            Err(ReadError::InvalidSource)
        ));

        let native = Uuid::new_v4();
        conn.execute(
            "INSERT INTO appserver_approval_publications(publication_id,session_id,state,closure_state,incarnation_id) VALUES(?1,?2,'unresolved',?3,?4)",
            params![native.to_string(), session.to_string(), "x".repeat(129), Uuid::new_v4().to_string()],
        )
        .unwrap();
        assert!(matches!(
            pending_source_exact(
                &conn,
                project,
                session,
                PendingSource::NativePublications,
                native,
            ),
            Err(ReadError::InvalidSource)
        ));
    }
}
