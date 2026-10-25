//! Cold-archive tests, slice S2 (#2384): cold reads, restore and its fences.
//!
//! A session is moved with the test-only move seam (`cold_move_session_for_test`),
//! so the rows really leave `conversation_events`; every read path is then
//! compared, field by field, with what it returned before the move.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    clippy::similar_names
)]

use super::tests::{Fx, force_insert, ts};
use super::*;
use crate::store::Store;
use crate::store_support::spawn_single_flight::try_acquire_spawn_guard;
use rsi_common::types::{ConversationEvent, EventType, Role, SessionStatus};
use rusqlite::params;
use rusqlite::types::Value;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

const EVENT_DUMP_SQL: &str = "SELECT id, session_id, sequence, event_type, role, content,
        tool_name, tool_input, created_at, tool_use_id, metadata
     FROM conversation_events WHERE session_id = ?1 ORDER BY id";
const PROVENANCE_DUMP_SQL: &str = "SELECT p.conversation_event_id, p.producer_kind,
        p.model_invocation_id, p.provider_event_type, p.created_at
     FROM conversation_event_provenance p
     JOIN conversation_events e ON e.id = p.conversation_event_id
     WHERE e.session_id = ?1 ORDER BY p.conversation_event_id";

/// Everything a transcript reader returns for one session.
#[derive(Debug, PartialEq)]
struct Transcript {
    events: serde_json::Value,
    since: Vec<serde_json::Value>,
    pages: Vec<serde_json::Value>,
    watermark: Option<String>,
    last_assistant: serde_json::Value,
    last_provider_output_at: String,
}

fn probe(store: &Store, id: Uuid) -> Transcript {
    let events = serde_json::to_value(store.load_events(id).unwrap()).unwrap();
    let since = [
        None,
        Some(0),
        Some(1),
        Some(2),
        Some(3),
        Some(5),
        Some(8),
        Some(1000),
    ]
    .into_iter()
    .map(|since| serde_json::to_value(store.load_events_since(id, since).unwrap()).unwrap())
    .collect();
    let matrix: Vec<(Option<i32>, u32, Option<Vec<EventType>>)> = vec![
        (None, 3, None),
        (None, 1000, None),
        (Some(0), 2, None),
        (Some(4), 3, None),
        (Some(1000), 5, None),
        (None, 4, Some(vec![EventType::ToolResult])),
        (
            Some(2),
            2,
            Some(vec![EventType::ToolUse, EventType::ToolResult]),
        ),
        (None, 2, Some(vec![EventType::Message])),
        (
            Some(3),
            1,
            Some(vec![EventType::Message, EventType::Thinking]),
        ),
    ];
    let pages = matrix
        .iter()
        .map(|(after, limit, types)| {
            let (events, more, earlier) = store
                .read_session_events_page(id, *after, *limit, types.as_deref())
                .unwrap();
            serde_json::json!({ "events": events, "more": more, "earlier": earlier })
        })
        .collect();
    Transcript {
        events,
        since,
        pages,
        watermark: store.event_watermark(id).unwrap(),
        last_assistant: serde_json::to_value(store.last_assistant_message_event(id).unwrap())
            .unwrap(),
        last_provider_output_at: format!("{:?}", store.last_provider_output_at(id).unwrap()),
    }
}

fn dump(fx: &Fx, id: Uuid) -> (Vec<Vec<Value>>, Vec<Vec<Value>>) {
    (
        fx.live_rows(EVENT_DUMP_SQL, id, 11),
        fx.live_rows(PROVENANCE_DUMP_SQL, id, 5),
    )
}

fn new_event(session_id: Uuid, sequence: i32) -> ConversationEvent {
    ConversationEvent {
        id: 0,
        offload_id: None,
        session_id,
        sequence,
        event_type: EventType::Message,
        role: Some(Role::User),
        content: "after the restore".into(),
        tool_name: None,
        tool_input: None,
        created_at: chrono::Utc::now(),
        tool_use_id: None,
        metadata: None,
    }
}

/// Stages the session (14 days) and moves it with the test seam.
fn stage_and_move(fx: &Fx, id: Uuid) -> MoveReceipt {
    let report = fx.stage(14).unwrap();
    assert!(report.sessions_staged >= 1, "{report:?}");
    match fx.store.cold_move_session_for_test(id).unwrap() {
        MoveOutcome::Moved(receipt) => receipt,
        other => panic!("expected the session to move, got {other:?}"),
    }
}

fn restore(fx: &Fx, id: Uuid) -> RestoreReceipt {
    let guard = try_acquire_spawn_guard(id).expect("no other holder of the guard");
    fx.store.restore_cold_session(&guard, id).unwrap()
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_cold_session_returns_the_identical_transcript_on_every_read_path() {
    let fx = Fx::new();
    let sid = fx.cold_session(SessionStatus::Archived, 40, 4);
    let before = probe(&fx.store, sid);
    let live_before = fx.live_counts();
    let receipt = stage_and_move(&fx, sid);
    assert_eq!((receipt.events, receipt.provenance), (8, 8));

    // The rows really left the live table, so the equality below is the merge.
    let live_after = fx.live_counts();
    assert_eq!(live_after.0, live_before.0 - 8);
    assert_eq!(live_after.1, live_before.1 - 8);
    assert_eq!(
        fx.store
            .cold_session_storage(&sid.to_string())
            .unwrap()
            .tier,
        "cold"
    );

    let after = probe(&fx.store, sid);
    assert_eq!(after, before, "every reader returns the pre-move value");
    assert_eq!(after.events.as_array().unwrap().len(), 12);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_session_with_no_movable_rows_is_never_cold_and_reads_the_same() {
    let fx = Fx::new();
    let sid = fx.session(SessionStatus::Archived, 40);
    fx.event(sid, 1, "Message", Some("User"), "hello", 41);
    fx.event(sid, 2, "Message", Some("Assistant"), "hi", 41);
    let before = probe(&fx.store, sid);
    let report = fx.stage(14).unwrap();
    assert_eq!(report.sessions_staged, 0, "{report:?}");
    assert_eq!(
        fx.store
            .cold_session_storage(&sid.to_string())
            .unwrap()
            .tier,
        "live"
    );
    assert_eq!(probe(&fx.store, sid), before);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn the_program_gate_window_of_a_cold_target_is_the_unmoved_window() {
    use crate::store_support::manager_gates::check_manager_program_gate;
    let fx = Fx::new();
    let sid = fx.session(SessionStatus::Archived, 40);
    fx.event(sid, 1, "Message", Some("User"), "go", 42);
    // 260 assistant-role tool-use rows after the last user message: more than the
    // gate's 256-event window, so its evidence is unknown. Only the newest row
    // stays live, so a gate that read the tail alone would pass.
    for sequence in 2..262 {
        fx.event_at(
            sid,
            sequence,
            "ToolUse",
            Some("Assistant"),
            "",
            Some("Bash"),
            None,
            &ts(41),
        );
    }
    fx.event(sid, 262, "Message", Some("Assistant"), "done", 41);
    let gate = |fx: &Fx| {
        check_manager_program_gate(&fx.store, sid, false, false).map_err(|error| error.to_string())
    };
    let before = gate(&fx);
    assert!(
        before
            .as_ref()
            .is_err_and(|message| message.contains("manager_v2_program_evidence_unknown")),
        "{before:?}"
    );

    let receipt = stage_and_move(&fx, sid);
    assert!(receipt.events >= 255, "{receipt:?}");
    assert_eq!(
        gate(&fx),
        before,
        "the gate sees the archived tool-use rows"
    );

    restore(&fx, sid);
    assert_eq!(gate(&fx), before);
}

// ---------------------------------------------------------------------------
// Unavailable archives
// ---------------------------------------------------------------------------

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_missing_archive_is_cold_archive_unavailable_never_a_short_transcript() {
    let fx = Fx::new();
    let sid = fx.cold_session(SessionStatus::Archived, 40, 3);
    stage_and_move(&fx, sid);
    let file = fx.archive_paths().remove(0);
    std::fs::remove_file(&file).unwrap();

    let error = fx.store.load_events(sid).unwrap_err();
    assert!(is_unavailable_error(&error), "{error}");
    assert!(error.to_string().contains(UNAVAILABLE_CODE), "{error}");
    let crate::error::DaemonError::StructuredRpc { data, .. } = &error else {
        panic!("expected a structured error, got {error:?}");
    };
    assert_eq!(data["reason"], "cold_archive_file_missing");
    let expected_file = file.to_string_lossy().into_owned();
    assert_eq!(data["file"], expected_file.as_str());

    let page = fx
        .store
        .read_session_events_page(sid, None, 5, None)
        .unwrap_err();
    assert!(is_unavailable_error(&page), "{page}");
    let since = fx.store.load_events_since(sid, Some(2)).unwrap_err();
    assert!(is_unavailable_error(&since), "{since}");

    // A restore refuses with the stable code and changes nothing.
    let guard = try_acquire_spawn_guard(sid).unwrap();
    let refused = fx.store.restore_cold_session(&guard, sid).unwrap_err();
    assert_eq!(refused.code, "cold_archive_file_missing");
    assert_eq!(fx.row(sid).unwrap().state, "cold");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_corrupt_chunk_is_unavailable_and_a_foreign_chunk_is_refused() {
    let fx = Fx::new();
    let sid = fx.cold_session(SessionStatus::Archived, 40, 3);
    stage_and_move(&fx, sid);
    let file = fx.archive_paths().remove(0);
    {
        let conn = rusqlite::Connection::open(&file).unwrap();
        conn.execute_batch(
            "DROP TRIGGER chunks_append_only_update;
             UPDATE chunks SET payload = zeroblob(16) WHERE table_name = 'conversation_events';",
        )
        .unwrap();
    }
    let error = fx.store.load_events(sid).unwrap_err();
    assert!(is_unavailable_error(&error), "{error}");
    let crate::error::DaemonError::StructuredRpc { data, .. } = &error else {
        panic!("expected a structured error");
    };
    assert!(
        data["reason"]
            .as_str()
            .is_some_and(|reason| reason.starts_with("cold_archive_verify")),
        "{data}"
    );

    // A chunk that belongs to another session under the recorded id is refused
    // by identity, before anything is decoded.
    let conn = rusqlite::Connection::open(&file).unwrap();
    conn.execute(
        "UPDATE chunks SET session_id = 'another-session' WHERE table_name = 'conversation_events'
           AND session_id = ?1",
        params![sid.to_string()],
    )
    .unwrap();
    drop(conn);
    let cold = fx.store.cold_ref(&sid.to_string()).unwrap().unwrap();
    let identity = decode_cold_session(&cold).unwrap_err();
    assert_eq!(identity.code, "cold_archive_verify_identity", "{identity}");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_chunk_with_unknown_columns_is_refused_not_silently_dropped() {
    let fx = Fx::new();
    let sid = fx.cold_session(SessionStatus::Archived, 40, 2);
    stage_and_move(&fx, sid);
    let file = fx.archive_paths().remove(0);
    let conn = rusqlite::Connection::open(&file).unwrap();
    conn.execute_batch(
        "DROP TRIGGER chunks_append_only_update;
         UPDATE chunks SET columns_json = replace(columns_json, '\"metadata\"', '\"meta_data\"')
          WHERE table_name = 'conversation_events';",
    )
    .unwrap();
    drop(conn);
    let cold = fx.store.cold_ref(&sid.to_string()).unwrap().unwrap();
    let error = decode_cold_session(&cold).unwrap_err();
    assert_eq!(error.code, "cold_archive_schema_drift");
}

// ---------------------------------------------------------------------------
// Writers and the purge refusal
// ---------------------------------------------------------------------------

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn an_insert_into_a_cold_session_is_refused_then_accepted_after_restore() {
    let fx = Fx::new();
    let sid = fx.cold_session(SessionStatus::Archived, 40, 3);
    stage_and_move(&fx, sid);
    let refused = fx
        .store
        .insert_event(&new_event(sid, 500))
        .unwrap_err()
        .to_string();
    assert!(
        refused.contains("cold_session_restore_required"),
        "{refused}"
    );
    let flip = fx.store.conn.execute(
        "UPDATE sessions SET status = 'Completed' WHERE id = ?1",
        [sid.to_string()],
    );
    assert!(
        flip.unwrap_err()
            .to_string()
            .contains("cold_session_restore_required"),
        "the status guard holds while the rows are archived"
    );

    restore(&fx, sid);
    let id = fx.store.insert_event(&new_event(sid, 500)).unwrap();
    assert!(id > 0);
    fx.store
        .conn
        .execute(
            "UPDATE sessions SET status = 'Completed' WHERE id = ?1",
            [sid.to_string()],
        )
        .unwrap();
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn purge_refuses_a_moved_session_and_keeps_its_rows_readable() {
    let fx = Fx::new();
    let sid = fx.cold_session(SessionStatus::Deleted, 60, 2);
    let before = probe(&fx.store, sid);
    stage_and_move(&fx, sid);
    let error = fx.store.purge_session(sid).unwrap_err().to_string();
    assert!(error.contains("session_cold_archived"), "{error}");
    assert!(fx.store.get_session(sid).unwrap().is_some());
    assert_eq!(probe(&fx.store, sid), before);
}

// ---------------------------------------------------------------------------
// Restore
// ---------------------------------------------------------------------------

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn restore_round_trips_exact_rows_and_flips_the_tier() {
    let fx = Fx::new();
    let sid = fx.cold_session(SessionStatus::Archived, 40, 4);
    let before = dump(&fx, sid);
    let transcript_before = probe(&fx.store, sid);
    assert_eq!((before.0.len(), before.1.len()), (12, 10));
    stage_and_move(&fx, sid);
    let during = dump(&fx, sid);
    assert_eq!((during.0.len(), during.1.len()), (4, 2));

    let receipt = restore(&fx, sid);
    assert_eq!(receipt.outcome, "restored");
    assert_eq!((receipt.events, receipt.provenance), (8, 8));
    assert_eq!(receipt.generation, 1);

    // Every row comes back exactly: ids, all columns, and the provenance rows.
    assert_eq!(dump(&fx, sid), before);
    let storage = fx.store.cold_session_storage(&sid.to_string()).unwrap();
    assert_eq!(storage.tier, "restored");
    assert!(storage.restored_at.is_some());
    assert!(fx.store.cold_ref(&sid.to_string()).unwrap().is_none());
    let row = fx.row(sid).unwrap();
    assert_eq!(row.state, "restored");
    assert!(row.restored_at.is_some());
    assert_eq!(probe(&fx.store, sid), transcript_before);
    // The restored session is held live for the look-back window.
    let report = fx.stage(14).unwrap();
    assert_eq!(report.sessions_staged, 0, "{report:?}");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_second_restore_is_already_hot_and_reads_no_archive() {
    let fx = Fx::new();
    let sid = fx.cold_session(SessionStatus::Archived, 40, 3);
    stage_and_move(&fx, sid);
    let guard = try_acquire_spawn_guard(sid).unwrap();
    let first = fx.store.restore_cold_session(&guard, sid).unwrap();
    assert_eq!(first.outcome, "restored");
    let live = fx.live_counts();

    // The archive is gone: only a call that reads it could fail now.
    std::fs::remove_file(fx.archive_paths().remove(0)).unwrap();
    let second = fx.store.restore_cold_session(&guard, sid).unwrap();
    assert_eq!(second.outcome, "already_hot");
    assert_eq!((second.events, second.provenance), (0, 0));
    assert_eq!(second.generation, 1);
    assert_eq!(fx.live_counts(), live, "nothing was inserted");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_stale_reference_hits_the_zero_row_fence_and_inserts_nothing() {
    let fx = Fx::new();
    let sid = fx.cold_session(SessionStatus::Archived, 40, 3);
    stage_and_move(&fx, sid);
    let guard = try_acquire_spawn_guard(sid).unwrap();
    let stale = fx.store.cold_restore_plan(&guard, sid).unwrap().unwrap();
    let decoded = decode_cold_session(&stale).unwrap();

    // Another caller restores while this one holds its decode.
    let winner = fx.store.restore_cold_session(&guard, sid).unwrap();
    assert_eq!(winner.outcome, "restored");
    let live = fx.live_counts();
    let loser = fx
        .store
        .cold_restore_commit(&guard, &stale, &decoded)
        .unwrap();
    assert_eq!(
        loser,
        ColdRestoreCommit::AlreadyHot {
            generation: stale.generation
        }
    );
    assert_eq!(fx.live_counts(), live, "no INSERT was attempted");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_generation_bump_between_decode_and_commit_retries_on_the_new_generation() {
    let fx = Fx::new();
    let sid = fx.cold_session(SessionStatus::Archived, 40, 3);
    stage_and_move(&fx, sid);
    let guard = try_acquire_spawn_guard(sid).unwrap();
    let stale = fx.store.cold_restore_plan(&guard, sid).unwrap().unwrap();
    let stale_decode = decode_cold_session(&stale).unwrap();
    assert_eq!(stale.generation, 1);

    // Generation 1 is restored, one archived body changes, and the session is
    // archived again: a new chunk at generation 2.
    assert_eq!(
        fx.store.restore_cold_session(&guard, sid).unwrap().outcome,
        "restored"
    );
    let changed: i64 = fx
        .store
        .conn
        .query_row(
            "SELECT id FROM conversation_events WHERE session_id = ?1 AND event_type = 'ToolResult'
             ORDER BY sequence LIMIT 1",
            [sid.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    fx.store
        .update_event_content(changed, "a body that changed after the first archive")
        .unwrap();
    fx.store
        .conn
        .execute(
            "UPDATE cold_archive_sessions SET restored_at = ?2 WHERE session_id = ?1",
            params![sid.to_string(), ts(30)],
        )
        .unwrap();
    drop(guard);
    stage_and_move(&fx, sid);
    let row = fx.row(sid).unwrap();
    assert_eq!((row.state.as_str(), row.generation), ("cold", 2));

    let guard = try_acquire_spawn_guard(sid).unwrap();
    let live = fx.live_counts();
    let stale_commit = fx
        .store
        .cold_restore_commit(&guard, &stale, &stale_decode)
        .unwrap();
    assert_eq!(stale_commit, ColdRestoreCommit::Stale);
    assert_eq!(
        fx.live_counts(),
        live,
        "the stale decode was never inserted"
    );
    assert_eq!(fx.row(sid).unwrap().state, "cold");

    // The retry decodes the new generation.
    let receipt = fx.store.restore_cold_session(&guard, sid).unwrap();
    assert_eq!(receipt.outcome, "restored");
    assert_eq!(receipt.generation, 2);
    let body: String = fx
        .store
        .conn
        .query_row(
            "SELECT content FROM conversation_events WHERE id = ?1",
            [changed],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(body, "a body that changed after the first archive");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_primary_key_collision_rolls_back_and_the_session_stays_cold() {
    let fx = Fx::new();
    let sid = fx.cold_session(SessionStatus::Archived, 40, 3);
    let other = fx.session(SessionStatus::Deleted, 60);
    stage_and_move(&fx, sid);
    let guard = try_acquire_spawn_guard(sid).unwrap();
    let cold = fx.store.cold_restore_plan(&guard, sid).unwrap().unwrap();
    let decoded = decode_cold_session(&cold).unwrap();
    let Value::Integer(archived_id) = decoded.events[2][0] else {
        panic!("the first column is the event id");
    };
    // A live row of another session already holds an archived id: a
    // contradiction, not a race.
    fx.store
        .conn
        .execute(
            "INSERT INTO conversation_events(id, session_id, sequence, event_type, content, created_at)
             VALUES (?1, ?2, 1, 'System', 'squatter', ?3)",
            params![archived_id, other.to_string(), ts(1)],
        )
        .unwrap();
    let live = fx.live_counts();

    let error = fx.store.restore_cold_session(&guard, sid).unwrap_err();
    assert_eq!(error.code, "cold_restore_conflict", "{error}");
    assert!(error.detail.contains(&archived_id.to_string()), "{error}");
    assert_eq!(
        fx.row(sid).unwrap().state,
        "cold",
        "the fence rolled back too"
    );
    assert_eq!(fx.live_counts(), live, "no partial insert survived");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_restore_needs_the_guard_of_its_own_session() {
    let fx = Fx::new();
    let sid = fx.cold_session(SessionStatus::Archived, 40, 2);
    let other = fx.session(SessionStatus::Archived, 40);
    stage_and_move(&fx, sid);
    let wrong = try_acquire_spawn_guard(other).unwrap();
    let error = fx.store.restore_cold_session(&wrong, sid).unwrap_err();
    assert_eq!(error.code, "cold_restore_guard_mismatch");
    assert_eq!(fx.row(sid).unwrap().state, "cold");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn the_move_skips_a_session_whose_spawn_guard_is_held() {
    let fx = Fx::new();
    let sid = fx.cold_session(SessionStatus::Archived, 40, 3);
    let live = fx.live_counts();
    fx.stage(14).unwrap();
    let held = try_acquire_spawn_guard(sid).unwrap();
    assert_eq!(
        fx.store.cold_move_session_for_test(sid).unwrap(),
        MoveOutcome::GuardBusy
    );
    assert_eq!(fx.row(sid).unwrap().state, "staged");
    assert_eq!(
        fx.live_counts(),
        live,
        "nothing moved while a restore holds the guard"
    );
    drop(held);
    assert!(matches!(
        fx.store.cold_move_session_for_test(sid).unwrap(),
        MoveOutcome::Moved(_)
    ));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn the_move_refuses_rows_that_changed_since_they_were_staged() {
    let fx = Fx::new();
    let sid = fx.cold_session(SessionStatus::Archived, 40, 3);
    fx.stage(14).unwrap();
    let id: i64 = fx
        .store
        .conn
        .query_row(
            "SELECT id FROM conversation_events WHERE session_id = ?1 AND event_type = 'ToolResult'
             ORDER BY sequence LIMIT 1",
            [sid.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    fx.store
        .update_event_content(id, "edited after staging")
        .unwrap();
    let live = fx.live_counts();
    assert_eq!(
        fx.store.cold_move_session_for_test(sid).unwrap(),
        MoveOutcome::Changed
    );
    assert_eq!(fx.row(sid).unwrap().state, "staged");
    assert_eq!(fx.live_counts(), live);
}

// ---------------------------------------------------------------------------
// Logical aggregates (#2396): the lead-retirement witness
// ---------------------------------------------------------------------------

fn retire_lead(fx: &Fx, lead: Uuid, manager: Uuid) {
    let sequence: i64 = fx
        .store
        .conn
        .query_row(
            "SELECT COALESCE(MAX(sequence),0) FROM conversation_events WHERE session_id=?1",
            [lead.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    let count: i64 = fx
        .store
        .conn
        .query_row(LOGICAL_EVENT_COUNT_SQL, [lead.to_string()], |row| {
            row.get(0)
        })
        .unwrap();
    let payload = serde_json::json!({
        "lead_session_id": lead,
        "event_sequence": sequence,
        "event_count": count,
    })
    .to_string();
    force_insert(
        &fx.store.conn,
        "harness_manager_v2_records",
        &[
            (
                "project_id",
                Value::Text(crate::store::d04_test_project_id().to_string()),
            ),
            ("manager_session_id", Value::Text(manager.to_string())),
            ("scope_version", Value::Integer(1)),
            (
                "kind",
                Value::Text(crate::store::manager_actions::LEAD_RETIREMENT_KIND.to_string()),
            ),
            ("record_key", Value::Text(lead.to_string())),
            ("row_version", Value::Integer(1)),
            ("payload_json", Value::Text(payload)),
            ("archived", Value::Integer(0)),
        ],
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn retirement_witness_survives_cold_move_and_restore() {
    let fx = Fx::new();
    let manager = fx.session(SessionStatus::Completed, 1);
    let superseded = |fx: &Fx, lead: Uuid| {
        fx.store
            .manager_lead_program_outcome_superseded(lead)
            .unwrap()
    };

    // Case 1: the lead is retired while hot.
    let lead = fx.cold_session(SessionStatus::Archived, 40, 3);
    let physical: i64 = fx.count(
        "SELECT COUNT(*) FROM conversation_events WHERE session_id = ?1",
        Some(lead),
    );
    retire_lead(&fx, lead, manager);
    assert!(
        superseded(&fx, lead),
        "the witness is true after the retirement"
    );
    stage_and_move(&fx, lead);
    let live_now = fx.count(
        "SELECT COUNT(*) FROM conversation_events WHERE session_id = ?1",
        Some(lead),
    );
    assert!(live_now < physical, "the rows left the live table");
    assert!(
        superseded(&fx, lead),
        "the witness reads the logical count, so the move does not flip it"
    );
    restore(&fx, lead);
    assert!(superseded(&fx, lead), "nor does the restore");
    fx.store.insert_event(&new_event(lead, 900)).unwrap();
    assert!(!superseded(&fx, lead), "a real new event turns it false");

    // Case 2: the lead is retired while cold.
    let cold_lead = fx.cold_session(SessionStatus::Archived, 40, 3);
    stage_and_move(&fx, cold_lead);
    retire_lead(&fx, cold_lead, manager);
    assert!(superseded(&fx, cold_lead), "retired while cold");
    restore(&fx, cold_lead);
    assert!(
        superseded(&fx, cold_lead),
        "still true once the rows are back"
    );
    fx.store.insert_event(&new_event(cold_lead, 900)).unwrap();
    assert!(!superseded(&fx, cold_lead));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn logical_aggregate_statements_share_one_expression() {
    fn normalized(text: &str) -> String {
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    }
    let root = std::path::PathBuf::from(rsi_common::manifest_dir!()).join("src/store");
    let events = normalized(&std::fs::read_to_string(root.join("events.rs")).unwrap());
    let recovery =
        normalized(&std::fs::read_to_string(root.join("manager_actions/recovery.rs")).unwrap());
    assert!(
        events.contains(&normalized(LOGICAL_ARCHIVED_COUNT_SQL)),
        "event_watermark counts the archived rows"
    );
    assert!(
        events.contains(&normalized(LOGICAL_ARCHIVED_CONTENT_LEN_SQL)),
        "event_watermark adds the archived content length"
    );
    assert!(
        recovery.contains(&normalized(LOGICAL_ARCHIVED_COUNT_SQL)),
        "the retirement predicate counts the archived rows"
    );
    assert!(
        recovery.contains("cold_archive::LOGICAL_EVENT_COUNT_SQL"),
        "the retirement witness writer reads the shared constant"
    );
    assert!(
        LOGICAL_EVENT_COUNT_SQL.ends_with(LOGICAL_ARCHIVED_COUNT_SQL),
        "the shared constant embeds the archived share"
    );
}

// ---------------------------------------------------------------------------
// Invariance (plan §4.1): one probe per classified reader
// ---------------------------------------------------------------------------

/// A session shaped to break a careless tail: timestamps out of order, with a
/// `+10:00` offset and micro-/second precision, a `System` health fact, tool rows
/// with provenance, a `ToolUse` row that is the TEXT `MAX(created_at)` witness and
/// one that another table points at.
fn invariance_session(fx: &Fx) -> Uuid {
    let sid = fx.session(SessionStatus::Archived, 60);
    let at = |text: &str| text.to_string();
    fx.event_at(
        sid,
        1,
        "Message",
        Some("User"),
        "start",
        None,
        None,
        &at("2026-08-01T10:00:00+10:00"),
    );
    let system = fx.event_at(
        sid,
        2,
        "System",
        None,
        "init",
        None,
        None,
        &at("2026-08-01T00:00:00.000001+00:00"),
    );
    fx.store
        .conn
        .execute(
            "UPDATE conversation_events SET metadata = ?2 WHERE id = ?1",
            params![
                system,
                r#"{"health_fact":"delivery_abandoned","job_id":"j-1"}"#
            ],
        )
        .unwrap();
    let rows = [
        (3, "ToolUse", Some("Assistant"), "2026-08-01T00:00:03Z"),
        (4, "ToolResult", None, "2026-08-01T00:00:04Z"),
        // The TEXT maximum of created_at, though not the newest by sequence.
        (
            5,
            "ToolUse",
            Some("Assistant"),
            "2026-09-02T00:00:00.123456+00:00",
        ),
        (6, "ToolResult", None, "2026-08-02T00:00:00Z"),
        // Pointed at by another table.
        (7, "ToolUse", Some("Assistant"), "2026-08-03T00:00:00+00:00"),
        (8, "ToolResult", None, "2026-08-03T00:00:01+00:00"),
    ];
    let mut referenced = 0;
    for (sequence, kind, role, created) in rows {
        // #86: rows 3 and 4 move to the archive naming a pasted image.
        let image = if sequence == 4 {
            " /h/.rsi/paste/rsid/moved.png"
        } else {
            ""
        };
        let input = match (kind, sequence) {
            ("ToolUse", 3) => Some(r#"{"file_path":"/h/moved-input.webp"}"#),
            ("ToolUse", _) => Some(r#"{"cmd":"ls"}"#),
            _ => None,
        };
        let id = fx.event_at(
            sid,
            sequence,
            kind,
            role,
            &format!("{kind} {sequence} {}{image}", "payload ".repeat(50)),
            Some("Bash"),
            input,
            created,
        );
        fx.provenance(sid, id, "provider_other");
        if sequence == 7 {
            referenced = id;
        }
    }
    fx.event_at(
        sid,
        9,
        "Thinking",
        None,
        "hmm",
        None,
        None,
        "2026-08-04T00:00:00Z",
    );
    fx.event_at(
        sid,
        10,
        "Message",
        Some("Assistant"),
        "done",
        None,
        None,
        "2026-08-04T00:00:01Z",
    );
    // The row another table points at must stay in the live tail.
    force_insert(
        &fx.store.conn,
        "pending_question_publications",
        &[
            ("session_id", Value::Text(sid.to_string())),
            ("conversation_event_id", Value::Integer(referenced)),
        ],
    );
    sid
}

fn scalar(store: &Store, sql: &str, session: Uuid) -> String {
    let value: Value = store
        .conn
        .query_row(sql, [session.to_string()], |row| row.get(0))
        .unwrap();
    format!("{value:?}")
}

fn json_of<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap()
}

/// A named probe: a plain function of the store and the session.
macro_rules! probe {
    ($name:literal, |$store:ident, $id:ident| $body:expr) => {{
        fn run($store: &Store, $id: Uuid) -> String {
            $body
        }
        ($name, run as fn(&Store, Uuid) -> String)
    }};
}

fn probes() -> Vec<(&'static str, fn(&Store, Uuid) -> String)> {
    vec![
        probe!("load_events", |store, id| json_of(
            &store.load_events(id).unwrap()
        )),
        probe!("load_events_since", |store, id| {
            [
                None,
                Some(0),
                Some(2),
                Some(5),
                Some(8),
                Some(10),
                Some(100),
            ]
            .into_iter()
            .map(|since| json_of(&store.load_events_since(id, since).unwrap()))
            .collect::<Vec<_>>()
            .join("|")
        }),
        probe!("read_session_events_page", |store, id| {
            let pages: Vec<(Option<i32>, u32, Option<Vec<EventType>>)> = vec![
                (None, 3, None),
                (Some(0), 4, None),
                (
                    Some(4),
                    2,
                    Some(vec![EventType::ToolUse, EventType::ToolResult]),
                ),
                (None, 2, Some(vec![EventType::ToolResult])),
                (Some(100), 5, None),
            ];
            pages
                .iter()
                .map(|(after, limit, types)| {
                    json_of(
                        &store
                            .read_session_events_page(id, *after, *limit, types.as_deref())
                            .unwrap(),
                    )
                })
                .collect::<Vec<_>>()
                .join("|")
        }),
        probe!("event_watermark", |store, id| format!(
            "{:?}",
            store.event_watermark(id).unwrap()
        )),
        probe!("last_assistant_message_event", |store, id| {
            json_of(&store.last_assistant_message_event(id).unwrap())
        }),
        probe!("last_provider_output_at", |store, id| {
            format!("{:?}", store.last_provider_output_at(id).unwrap())
        }),
        probe!("program_gate_window", |store, id| {
            format!(
                "{:?}",
                crate::store_support::manager_gates::check_manager_program_gate(
                    store, id, false, false
                )
                .map_err(|error| error.to_string())
            )
        }),
        probe!("retirement_witness", |store, id| {
            format!(
                "{:?}",
                store.manager_lead_program_outcome_superseded(id).unwrap()
            )
        }),
        probe!("max_sequence", |store, id| {
            scalar(
                store,
                "SELECT COALESCE(MAX(sequence),0) FROM conversation_events WHERE session_id=?1",
                id,
            )
        }),
        probe!("any_event", |store, id| {
            scalar(
                store,
                "SELECT EXISTS(SELECT 1 FROM conversation_events WHERE session_id=?1)",
                id,
            )
        }),
        probe!("sequence_after", |store, id| {
            [0, 4, 9, 10]
                .iter()
                .map(|after| {
                    store
                        .conn
                        .query_row(
                            "SELECT EXISTS(SELECT 1 FROM conversation_events WHERE session_id=?1 AND sequence>?2)",
                            params![id.to_string(), after],
                            |row| row.get::<_, bool>(0),
                        )
                        .unwrap()
                        .to_string()
                })
                .collect::<Vec<_>>()
                .join(",")
        }),
        probe!("provider_after", |store, id| {
            [0, 4, 9]
                .iter()
                .map(|after| {
                    store
                        .conn
                        .query_row(
                            "SELECT EXISTS(SELECT 1 FROM conversation_events WHERE session_id=?1
                               AND sequence>?2 AND (role='Assistant' OR event_type='Thinking'))",
                            params![id.to_string(), after],
                            |row| row.get::<_, bool>(0),
                        )
                        .unwrap()
                        .to_string()
                })
                .collect::<Vec<_>>()
                .join(",")
        }),
        probe!("max_created_at", |store, id| {
            scalar(
                store,
                "SELECT MAX(created_at) FROM conversation_events WHERE session_id=?1",
                id,
            )
        }),
        probe!("max_created_at_non_system", |store, id| {
            scalar(
                store,
                "SELECT MAX(created_at) FROM conversation_events WHERE session_id=?1 AND event_type<>'System'",
                id,
            )
        }),
        probe!("message_facts", |store, id| {
            scalar(
                store,
                "SELECT group_concat(id || ':' || sequence || ':' || COALESCE(role,'') || ':' || content, '|')
                 FROM (SELECT * FROM conversation_events
                       WHERE session_id=?1 AND event_type='Message' ORDER BY sequence)",
                id,
            )
        }),
        probe!("user_role_facts", |store, id| {
            scalar(
                store,
                "SELECT COUNT(*) || ':' || COALESCE(MAX(sequence),-1) FROM conversation_events
                 WHERE session_id=?1 AND role='User'",
                id,
            )
        }),
        probe!("system_facts", |store, id| {
            scalar(
                store,
                "SELECT group_concat(id || ':' || sequence || ':' || content || ':' || COALESCE(metadata,''), '|')
                 FROM (SELECT * FROM conversation_events
                       WHERE session_id=?1 AND event_type='System' ORDER BY sequence)",
                id,
            )
        }),
        probe!("fk_rows", |store, id| {
            scalar(
                store,
                "SELECT group_concat(e.id || ':' || e.sequence || ':' || e.event_type, '|')
                 FROM (SELECT * FROM conversation_events WHERE session_id=?1 ORDER BY sequence) e
                 WHERE EXISTS (SELECT 1 FROM pending_question_publications x
                               WHERE x.conversation_event_id = e.id)",
                id,
            )
        }),
        // The portable-bundle manifest counts the table's rows as the logical
        // total (live plus archived), whichever session moved (slice S3).
        probe!("bundle_event_count", |store, _id| {
            let bundle = store.export_portable_bundle(&[]).unwrap();
            format!("{:?}", bundle.left_behind.get("conversation_events"))
        }),
        // #86: the texts the paste retention scan reads from conversation
        // events, live and archived, of every session.
        probe!("paste_reference_texts", |store, _id| {
            use crate::store::PasteReferenceSource;
            let mut texts = Vec::new();
            for source in [
                PasteReferenceSource::ConversationEvents,
                PasteReferenceSource::ArchivedConversationEvents,
            ] {
                let mut after = 0;
                while let Some(next) = store
                    .paste_reference_scan_chunk(source, after, 2, &mut |text: &str| {
                        texts.push(text.to_string());
                    })
                    .unwrap()
                {
                    after = next;
                }
            }
            texts.sort();
            texts.join("|")
        }),
        // #86 (hub #3322): the event high-water mark of the paste reference
        // scan, the newest live row of any session.
        probe!("paste_reference_event_mark", |store, _id| {
            store
                .conn
                .query_row(
                    "SELECT COALESCE(MAX(id), 0) FROM conversation_events",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap()
                .to_string()
        }),
    ]
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn cold_archive_reader_invariance() {
    use super::read_classes::PROBE_NAMES;
    let probes = probes();
    let names: Vec<&str> = probes.iter().map(|(name, _)| *name).collect();
    assert_eq!(
        names, PROBE_NAMES,
        "the probes run are exactly the probes the classification names"
    );

    let fx = Fx::new();
    let sid = invariance_session(&fx);
    let manager = fx.session(SessionStatus::Completed, 1);
    retire_lead(&fx, sid, manager);
    let run = |fx: &Fx| -> Vec<String> {
        probes
            .iter()
            .map(|(_, probe)| probe(&fx.store, sid))
            .collect()
    };

    let before = run(&fx);
    let live_before = fx.live_counts();
    let receipt = stage_and_move(&fx, sid);
    // Rows 3, 4, 6 and 8 move; 5 (the TEXT-max witness) and 7 (pointed at) stay.
    assert_eq!((receipt.events, receipt.provenance), (4, 4), "{receipt:?}");
    let live_after = fx.live_counts();
    assert_eq!(live_after.0, live_before.0 - 4);
    let fk_probe_index = names.iter().position(|name| *name == "fk_rows").unwrap();
    assert!(
        before[fk_probe_index].contains(":7:ToolUse"),
        "the row another table points at is in the tail: {}",
        before[fk_probe_index]
    );

    let paste_index = names
        .iter()
        .position(|name| *name == "paste_reference_texts")
        .unwrap();
    assert!(
        before[paste_index].contains("/h/.rsi/paste/rsid/moved.png")
            && before[paste_index].contains("/h/moved-input.webp"),
        "the rows that move name pasted images: {}",
        before[paste_index]
    );

    let moved = run(&fx);
    for (index, name) in names.iter().enumerate() {
        assert_eq!(
            moved[index], before[index],
            "probe {name} changed by the move"
        );
    }
    restore(&fx, sid);
    let restored = run(&fx);
    for (index, name) in names.iter().enumerate() {
        assert_eq!(
            restored[index], before[index],
            "probe {name} changed by the restore"
        );
    }
    // The rows are back exactly where they were.
    assert_eq!(fx.live_counts().0, live_before.0);
}

/// #86 (hub #3322 finding 3): the paste reference scan reads a moved session
/// under its bound, and refuses one whose archive record is over it (even when
/// the index row understates the size) before it is decompressed, naming the
/// session.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn the_paste_reference_scan_bounds_an_archive_before_decompressing_it() {
    let fx = Fx::new();
    let sid = fx.cold_session(SessionStatus::Archived, 40, 4);
    stage_and_move(&fx, sid);
    super::reader::cache_forget(&sid.to_string());
    let mut seen = 0;
    fx.store
        .cold_reference_scan_session(0, PASTE_REFERENCE_ARCHIVE_MAX_RAW_BYTES, &mut |_| seen += 1)
        .unwrap();
    assert_eq!(seen, 8, "every archived row under the bound");

    fx.store
        .conn
        .execute(
            "UPDATE cold_archive_sessions SET raw_bytes=0, compressed_bytes=0 WHERE session_id=?1",
            [sid.to_string()],
        )
        .unwrap();
    let mut seen = 0;
    let refused = fx
        .store
        .cold_reference_scan_session(0, 1, &mut |_| seen += 1)
        .unwrap_err()
        .to_string();
    assert!(refused.contains(OVER_BOUND_CODE), "{refused}");
    assert!(refused.contains(&sid.to_string()), "{refused}");
    assert_eq!(seen, 0);
}

/// #86 (hub #3332 finding F3b): when the index row and the archive record both
/// understate a large stored payload, the bounded read refuses it from the
/// payload's `LENGTH()` alone, before the blob is selected: the read returns
/// only sizes, and the paste reference scan fails closed naming the session.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn an_understated_payload_is_refused_before_the_blob_is_read() {
    const BOUND: i64 = 64 * 1024;
    const STORED: i64 = 1024 * 1024;
    let fx = Fx::new();
    let sid = fx.cold_session(SessionStatus::Archived, 40, 4);
    stage_and_move(&fx, sid);
    super::reader::cache_forget(&sid.to_string());
    let file = fx.archive_paths().remove(0);
    {
        let conn = rusqlite::Connection::open(&file).unwrap();
        conn.execute_batch(&format!(
            "DROP TRIGGER chunks_append_only_update;
             UPDATE chunks SET payload = zeroblob({STORED}), raw_bytes = 1
             WHERE table_name = 'conversation_events';"
        ))
        .unwrap();
    }
    fx.store
        .conn
        .execute(
            "UPDATE cold_archive_sessions SET raw_bytes=0, compressed_bytes=0 WHERE session_id=?1",
            [sid.to_string()],
        )
        .unwrap();

    let conn = super::writer::open_archive_readonly(&file).unwrap();
    let chunk_id: i64 = conn
        .query_row(
            "SELECT chunk_id FROM chunks WHERE table_name = 'conversation_events'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    match super::writer::read_chunk_bounded(&conn, chunk_id, BOUND).unwrap() {
        Some(super::writer::BoundedChunk::Over {
            raw_bytes,
            payload_bytes,
        }) => assert_eq!((raw_bytes, payload_bytes), (1, STORED)),
        other => panic!("expected the sizes alone, got {other:?}"),
    }
    drop(conn);

    let mut seen = 0;
    let refused = fx
        .store
        .cold_reference_scan_session(0, BOUND, &mut |_| seen += 1)
        .unwrap_err()
        .to_string();
    assert!(refused.contains(OVER_BOUND_CODE), "{refused}");
    assert!(refused.contains(&sid.to_string()), "{refused}");
    assert_eq!(seen, 0);
}

/// #178: text zstd cannot shrink much (hex of a fixed pseudo-random sequence),
/// so a chunk of a few such rows has a payload of several stream buffers.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
fn noise(seed: u64, bytes: usize) -> String {
    let mut state = seed;
    let mut out = String::with_capacity(bytes + 16);
    while out.len() < bytes {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        out.push_str(&format!("{:016x}", state ^ (state >> 29)));
    }
    out.truncate(bytes);
    out
}

/// #178: an archived session of eight 40 KiB tool results (about 320 KiB raw),
/// the fourth naming the pasted image `named`.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
fn large_cold_session(fx: &Fx, named: &str) -> Uuid {
    let sid = fx.session(SessionStatus::Archived, 40);
    fx.event(sid, 1, "Message", Some("User"), "start", 41);
    for sequence in 2..10 {
        let mut content = noise(u64::try_from(sequence).unwrap(), 40 * 1024);
        if sequence == 5 {
            content.push_str(&format!(" see @/home/op/.rsi/paste/rsid/{named}"));
        }
        fx.event(sid, sequence, "ToolResult", None, &content, 41);
    }
    fx.event(sid, 10, "Message", Some("Assistant"), "done", 41);
    sid
}

/// #178: every archived text the paste reference scan passes on for the first
/// archived session, under the current bound.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
fn archived_reference_texts(store: &Store) -> crate::error::Result<Vec<String>> {
    let mut texts = Vec::new();
    store.paste_reference_scan_chunk(
        crate::store::PasteReferenceSource::ArchivedConversationEvents,
        0,
        1,
        &mut |text: &str| texts.push(text.to_string()),
    )?;
    Ok(texts)
}

/// #178: an archived session over the paste scan's bound is no longer refused:
/// it is streamed (the payload read a buffer at a time, one row held), and the
/// scan passes on exactly the texts the whole-chunk read does, so the image it
/// names is found and an image it does not name is not.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn the_paste_reference_scan_streams_an_archive_over_its_bound_in_bounded_pieces() {
    const BOUND: i64 = 64 * 1024;
    let named = format!("{}.png", "a".repeat(64));
    let unnamed = format!("{}.png", "b".repeat(64));
    let fx = Fx::new();
    let sid = large_cold_session(&fx, &named);
    assert_eq!(stage_and_move(&fx, sid).events, 8);
    super::reader::cache_forget(&sid.to_string());
    let row = fx.row(sid).unwrap();
    assert!(
        row.raw_bytes > 4 * BOUND && row.compressed_bytes > 2 * BOUND,
        "{} raw, {} compressed",
        row.raw_bytes,
        row.compressed_bytes
    );

    let whole = archived_reference_texts(&fx.store).unwrap();
    let _ = stream_stats::take();
    let streamed = {
        let _bound = override_paste_reference_archive_bound_for_test(BOUND);
        archived_reference_texts(&fx.store).unwrap()
    };
    let stats = stream_stats::take();

    assert_eq!(
        streamed, whole,
        "the streamed scan sees what the whole read sees"
    );
    assert_eq!(streamed.len(), 1, "{streamed:?}");
    assert!(streamed[0].contains(&named), "the named image is found");
    assert!(!streamed[0].contains(&unnamed));
    assert_eq!(stats.rows, 8, "{stats:?}");
    assert!(
        stats.payload_reads >= 2,
        "the payload is read in pieces: {stats:?}"
    );
    assert!(
        stats.largest_payload_read <= super::format::STREAM_BUFFER_BYTES,
        "{stats:?}"
    );
    assert!(
        i64::try_from(stats.largest_row).unwrap() <= BOUND,
        "one row at a time: {stats:?}"
    );
}

/// #178 (hub review of ca787eab7): the unbounded read of an oversized archive
/// is not done by the chunk read that runs under the store lock. The step
/// returns the stream instead, reads nothing itself, and the stream, run
/// with no store in hand, passes on the same texts.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn an_oversized_archive_is_returned_as_a_stream_to_run_without_the_store() {
    let named = format!("{}.png", "a".repeat(64));
    let fx = Fx::new();
    let sid = large_cold_session(&fx, &named);
    stage_and_move(&fx, sid);
    super::reader::cache_forget(&sid.to_string());
    let _bound = override_paste_reference_archive_bound_for_test(64 * 1024);
    let _ = stream_stats::take();

    let mut during_step = Vec::new();
    let step = fx
        .store
        .paste_reference_scan_step(
            crate::store::PasteReferenceSource::ArchivedConversationEvents,
            0,
            1,
            &mut |text: &str| during_step.push(text.to_string()),
        )
        .unwrap();

    assert!(step.next.is_some());
    assert!(during_step.is_empty(), "the step read no archived row");
    assert_eq!(
        stream_stats::take().rows,
        0,
        "the step opened no archive stream"
    );
    let stream = step.stream.expect("the oversized archive is deferred");
    let mut streamed = Vec::new();
    stream
        .run(&mut |text: &str| streamed.push(text.to_string()))
        .unwrap();
    assert_eq!(streamed.len(), 1, "{streamed:?}");
    assert!(streamed[0].contains(&named));
}

/// #178: a single streamed row over the bound still fails the scan closed,
/// naming the session.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_streamed_row_over_the_paste_scan_bound_fails_closed_naming_the_session() {
    let fx = Fx::new();
    let sid = large_cold_session(&fx, &format!("{}.png", "a".repeat(64)));
    stage_and_move(&fx, sid);
    let _bound = override_paste_reference_archive_bound_for_test(16 * 1024);

    let refused = archived_reference_texts(&fx.store).unwrap_err().to_string();

    assert!(refused.contains(OVER_BOUND_CODE), "{refused}");
    assert!(refused.contains(&sid.to_string()), "{refused}");
}

/// #178: the streamed rows are checked against the chunk's length and digest
/// once the stream ends; a stream whose digest differs from the one the index
/// and the record agree on fails the scan as unavailable.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_streamed_archive_whose_digest_differs_fails_the_scan_closed() {
    let wrong = "f".repeat(64);
    let fx = Fx::new();
    let sid = large_cold_session(&fx, &format!("{}.png", "a".repeat(64)));
    stage_and_move(&fx, sid);
    let file = fx.archive_paths().remove(0);
    {
        let conn = rusqlite::Connection::open(&file).unwrap();
        conn.execute_batch(&format!(
            "DROP TRIGGER chunks_append_only_update;
             UPDATE chunks SET raw_sha256 = '{wrong}' WHERE table_name = 'conversation_events';"
        ))
        .unwrap();
    }
    fx.store
        .conn
        .execute(
            "UPDATE cold_archive_sessions SET events_sha256 = ?1 WHERE session_id = ?2",
            params![wrong, sid.to_string()],
        )
        .unwrap();
    let _bound = override_paste_reference_archive_bound_for_test(64 * 1024);

    let refused = archived_reference_texts(&fx.store).unwrap_err().to_string();

    assert!(refused.contains(UNAVAILABLE_CODE), "{refused}");
    assert!(
        refused.contains("cold_archive_verify_sha_mismatch"),
        "{refused}"
    );
}
