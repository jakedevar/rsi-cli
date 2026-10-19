//! Cold-storage archive, slice S3 (#2384): Remote V1 and the TUI's views.
//!
//! Remote V1 serves only sessions that are not `Archived` or `Deleted`
//! (`require_session_project`), and only an `Archived` or `Deleted` session is
//! ever cold (the archive's candidate query and the V176 status guard). So the
//! Remote history and detail readers can never reach a cold session's rows: they
//! refuse it before they read an event, exactly as they did before it moved. These
//! tests move a session for real (the test-only move seam) and compare what every
//! Remote reader returns before and after, with an unmoved `Running` session as the
//! positive control that the readers do serve a session the gate admits.

use super::*;
use crate::remote_read::{
    HistoryRange, history_page, observed_history_page, session_detail_source, tool_pair_ambiguous,
};
use rsi_common::types::SessionStatus;
use rusqlite::params;

fn test_project() -> Uuid {
    rsid_store::test_support::make_test_session()
        .project_id
        .expect("the test session belongs to a project")
}

fn live_rows(store: &crate::store::Store, session: Uuid) -> i64 {
    store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM conversation_events WHERE session_id = ?1",
            [session.to_string()],
            |row| row.get(0),
        )
        .unwrap()
}

/// Everything the Remote history and detail readers return for `session`, as text
/// (an error is its debug name), read inside the read transaction the owner holds.
fn remote_view(store: &crate::store::Store, project: Uuid, session: Uuid) -> String {
    store.conn.execute_batch("BEGIN").unwrap();
    let observed = observed_history_page(&store.conn, project, session, HistoryRange::Latest, 50)
        .map(|page| {
            let items: Vec<(i64, i32, String, String)> = page
                .page
                .items
                .iter()
                .map(|row| {
                    (
                        row.id,
                        row.sequence,
                        row.event_type.clone(),
                        row.content.text.clone(),
                    )
                })
                .collect();
            format!("head={:?} items={items:?}", page.head)
        })
        .map_err(|error| format!("{error:?}"));
    let plain = history_page(&store.conn, project, session, HistoryRange::Latest, 50)
        .map(|page| page.items.len())
        .map_err(|error| format!("{error:?}"));
    let detail = session_detail_source(&store.conn, project, session)
        .map(|detail| format!("{detail:?}"))
        .map_err(|error| format!("{error:?}"));
    store.conn.execute_batch("ROLLBACK").unwrap();
    format!("{observed:?}|{plain:?}|{detail:?}")
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
#[allow(clippy::expect_used, clippy::unwrap_used)]
async fn remote_v1_refuses_a_cold_session_exactly_as_before_the_move() {
    let project = test_project();
    for status in [SessionStatus::Archived, SessionStatus::Deleted] {
        let fixture = recursive_dag_rpc_fixture();
        let cold = seed_cold_candidate(&fixture, status).await;
        // The control: the same rows under a status the Remote gate admits. It
        // is never a cold candidate, so the stage pass leaves it alone.
        let control = {
            let store = fixture.manager.store().lock().await;
            seed_old_session(&store, SessionStatus::Running)
        };
        let (cold_before, control_before) = {
            let store = fixture.manager.store().lock().await;
            (
                remote_view(&store, project, cold),
                remote_view(&store, project, control),
            )
        };
        assert!(
            control_before.contains("head=Some((7,") && control_before.contains("ToolUse 3"),
            "the readers serve a session the gate admits: {control_before}"
        );
        let refused = "Err(\"NotFound\")|Err(\"NotFound\")|Err(\"NotFound\")";
        assert_eq!(
            cold_before, refused,
            "{status:?} is refused before the move"
        );

        stage_and_move_cold_session(&fixture, cold).await;

        let store = fixture.manager.store().lock().await;
        assert!(
            live_rows(&store, cold) < 7,
            "the rows really left the live table"
        );
        assert_eq!(
            remote_view(&store, project, cold),
            cold_before,
            "{status:?}: the move changes nothing a Remote reader returns"
        );
        assert_eq!(
            remote_view(&store, project, control),
            control_before,
            "the unmoved control is unchanged"
        );
    }
}

/// `tool_pair_ambiguous` counts only same-session rows as duplicates. A session it
/// is asked about is visible to Remote, so none of its rows is cold; another
/// (cold) session's rows only fill the 65-row cap. A move therefore never changes
/// an answer the cap did not hide, and turns a capped `SourceUnavailable` into
/// the exact answer.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::too_many_lines)]
async fn tool_pairing_ignores_another_sessions_rows_and_a_move_only_makes_it_exact() {
    let fixture = recursive_dag_rpc_fixture();
    fixture
        .runtime_config
        .update_field("cold_archive_enabled", &serde_json::json!(true))
        .unwrap();
    let old = (chrono::Utc::now() - chrono::Duration::days(61))
        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let (cold, live) = {
        let store = fixture.manager.store().lock().await;
        let cold = seed_old_session(&store, SessionStatus::Archived);
        // Another session's rows under the same tool ids: enough to fill the cap.
        for sequence in 8..=80_i32 {
            let (event_type, role) = if sequence % 2 == 0 {
                ("ToolUse", Some("Assistant"))
            } else {
                ("ToolResult", None)
            };
            let key = if sequence <= 76 { "crowd" } else { "few" };
            store
                .conn
                .execute(
                    "INSERT INTO conversation_events(session_id, sequence, event_type, role,
                        content, tool_use_id, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        cold.to_string(),
                        sequence,
                        event_type,
                        role,
                        format!("{event_type} {sequence}"),
                        key,
                        old
                    ],
                )
                .unwrap();
        }
        let live = seed_old_session(&store, SessionStatus::Running);
        (cold, live)
    };
    // Rows of the visible session, inserted last so its ids are the highest.
    let insert_live = |store: &crate::store::Store, sequence: i32, key: &str| -> i64 {
        store
            .conn
            .execute(
                "INSERT INTO conversation_events(session_id, sequence, event_type, role,
                    content, tool_use_id, created_at) VALUES (?1, ?2, 'ToolUse', 'Assistant',
                    'paired', ?3, ?4)",
                params![live.to_string(), sequence, key, now],
            )
            .unwrap();
        store.conn.last_insert_rowid()
    };
    let (crowded, few, lonely, twin_a, twin_b) = {
        let store = fixture.manager.store().lock().await;
        let crowded = insert_live(&store, 20, "crowd");
        let few = insert_live(&store, 21, "few");
        let lonely = insert_live(&store, 22, "lonely");
        let twin_a = insert_live(&store, 23, "twin");
        let twin_b = insert_live(&store, 24, "twin");
        (crowded, few, lonely, twin_a, twin_b)
    };
    let ask = |store: &crate::store::Store, id: i64, key: &str| -> String {
        store.conn.execute_batch("BEGIN").unwrap();
        let answer =
            tool_pair_ambiguous(&store.conn, live, id, key).map_err(|error| format!("{error:?}"));
        store.conn.execute_batch("ROLLBACK").unwrap();
        format!("{answer:?}")
    };

    let before = {
        let store = fixture.manager.store().lock().await;
        [
            ask(&store, crowded, "crowd"),
            ask(&store, few, "few"),
            ask(&store, lonely, "lonely"),
            ask(&store, twin_a, "twin"),
            ask(&store, twin_b, "twin"),
        ]
    };
    // The cold session holds 69 rows under `crowd`, more than the 65-row seek
    // returns, and they all sort before the visible session's row: the cap hides
    // the answer. `few` (4 foreign rows), `lonely` (none) and `twin` (a duplicate in
    // the same session) fit under the cap.
    assert_eq!(before[0], "Err(\"SourceUnavailable\")", "{before:?}");
    assert_eq!(before[1], "Ok(false)", "{before:?}");
    assert_eq!(before[2], "Ok(false)", "{before:?}");
    assert_eq!(
        before[3], "Ok(true)",
        "a same-session duplicate is ambiguous"
    );
    assert_eq!(before[4], "Ok(true)");

    stage_and_move_cold_session(&fixture, cold).await;

    let store = fixture.manager.store().lock().await;
    assert!(
        live_rows(&store, cold) < 80,
        "the other session's rows left the live table"
    );
    let after = [
        ask(&store, crowded, "crowd"),
        ask(&store, few, "few"),
        ask(&store, lonely, "lonely"),
        ask(&store, twin_a, "twin"),
        ask(&store, twin_b, "twin"),
    ];
    // Answers the cap did not hide are identical.
    for index in [1, 2, 3, 4] {
        assert_eq!(after[index], before[index], "key {index}");
    }
    // The capped key converges to the exact answer: the cold session's rows no
    // longer fill the cap, and none of them was ever a duplicate (other session).
    assert_eq!(after[0], "Ok(false)", "{after:?}");
}

/// The TUI reads the operator RPCs through `rsi_common::cold_storage`'s types. A
/// rename on either side must fail here, not show a blank panel.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
#[allow(clippy::expect_used, clippy::unwrap_used)]
async fn the_tui_view_types_read_the_daemons_cold_replies() {
    use rsi_common::cold_storage::{
        ColdArchiveStatusV1, ColdRestoreReceiptV1, ColdRunStartedV1, ColdSessionStorageV1, ColdTier,
    };
    let fixture = recursive_dag_rpc_fixture();
    let (session_id, _) = moved_cold_session(&fixture, SessionStatus::Archived).await;
    let ask = serde_json::json!({ "session_id": session_id });

    let storage: ColdSessionStorageV1 = serde_json::from_value(
        call_rpc(&fixture.server, "GetColdSessionStorage", ask.clone())
            .await
            .result
            .expect("storage"),
    )
    .unwrap();
    assert_eq!(storage.tier, ColdTier::Cold);
    assert_eq!(storage.archived_events, 4);
    assert!(
        storage.file.is_some() && storage.generation == Some(1),
        "{storage:?}"
    );

    let status: ColdArchiveStatusV1 = serde_json::from_value(
        call_rpc(
            &fixture.server,
            "GetColdArchiveStatus",
            serde_json::json!({}),
        )
        .await
        .result
        .expect("status"),
    )
    .unwrap();
    assert!(status.settings.enabled, "{status:?}");
    assert_eq!(status.totals.cold_sessions, 1, "{status:?}");
    assert_eq!(status.archives.len(), 1, "{status:?}");
    assert!(status.archives[0].bytes > 0 && status.live_db.page_count > 0);
    let last = status.last_run.expect("the stage pass is the last run");
    assert_eq!(
        (last.mode.as_str(), last.outcome.as_str()),
        ("stage", "completed")
    );

    let receipt: ColdRestoreReceiptV1 = serde_json::from_value(
        call_rpc(&fixture.server, "RestoreColdSession", ask.clone())
            .await
            .result
            .expect("restore"),
    )
    .unwrap();
    assert_eq!((receipt.outcome.as_str(), receipt.events), ("restored", 4));

    let started: ColdRunStartedV1 = serde_json::from_value(
        call_rpc(
            &fixture.server,
            "RunColdArchiveNow",
            serde_json::json!({ "mode": "verify" }),
        )
        .await
        .result
        .expect("run now"),
    )
    .unwrap();
    assert_eq!(started.mode, "verify");
    wait_for_cold_archive_run(&fixture.server, &started.run_id).await;
}
