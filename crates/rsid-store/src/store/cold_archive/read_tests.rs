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
        let id = fx.event_at(
            sid,
            sequence,
            kind,
            role,
            &format!("{kind} {sequence} {}", "payload ".repeat(50)),
            Some("Bash"),
            (kind == "ToolUse").then_some(r#"{"cmd":"ls"}"#),
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
