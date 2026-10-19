use super::*;

fn turn(store: &Store) -> NewProviderTurnCustody {
    let mut session = crate::test_support::make_test_session();
    session.id = Uuid::new_v4();
    session.project_id = None;
    store.insert_session(&session).unwrap();
    let invocation_id = invocation(store, session.id);
    NewProviderTurnCustody {
        worker_scope: None,
        invocation_id,
        session_id: session.id,
        spool_dir: std::env::temp_dir()
            .join("rsi-custody-test")
            .join(invocation_id.to_string()),
        pid: 12345,
        start_time: Some(98765),
        boot_id: Uuid::new_v4(),
    }
}

fn invocation(store: &Store, session_id: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    store.conn.execute(
        "INSERT INTO model_invocations
         (id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,trigger_source,session_id,created_at)
         VALUES(?1,'session.launch','session','foreground','paid','admitted','running','launch_session',?2,?3)",
        params![id.to_string(),session_id.to_string(),now()],
    ).unwrap();
    id
}

fn get(store: &Store, turn: &NewProviderTurnCustody) -> ProviderTurnCustody {
    store
        .get_provider_turn_custody(turn.invocation_id)
        .unwrap()
        .unwrap()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn dead_boot_cleanup_frees_the_next_detached_turn_slot_but_preserves_live_locks() {
    let store = Store::open_in_memory().unwrap();
    let dir = tempfile::tempdir().unwrap(); // tmpfs-fixture-ok: only a turn lock, no sandbox
    let mut old = turn(&store);
    old.spool_dir = dir.path().to_path_buf();
    store.insert_provider_turn_custody(&old).unwrap();
    let boot = Uuid::new_v4();
    let lock = std::fs::File::create(dir.path().join("alive.lock")).unwrap();
    lock.lock().unwrap();
    let refusal = store
        .abandon_unlocked_provider_turns_from_prior_boot(old.session_id, boot)
        .unwrap_err();
    assert!(refusal.to_string().contains("live_detached_turn"));
    assert_eq!(get(&store, &old).state, ProviderTurnCustodyState::Live);
    drop(lock);
    store
        .abandon_unlocked_provider_turns_from_prior_boot(old.session_id, boot)
        .unwrap();
    assert_eq!(get(&store, &old).state, ProviderTurnCustodyState::Abandoned);
    let next = NewProviderTurnCustody {
        invocation_id: invocation(&store, old.session_id),
        spool_dir: dir.path().join("next"),
        boot_id: boot,
        ..old.clone()
    };
    store.insert_provider_turn_custody(&next).unwrap();
    store
        .abandon_unlocked_provider_turns_from_prior_boot(next.session_id, boot)
        .unwrap();
    assert_eq!(get(&store, &next).state, ProviderTurnCustodyState::Live);
    // A missing lock file also means the old boot no longer owns a shim.
    store
        .abandon_unlocked_provider_turns_from_prior_boot(next.session_id, Uuid::new_v4())
        .unwrap();
    assert_eq!(
        get(&store, &next).state,
        ProviderTurnCustodyState::Abandoned
    );
    assert!(
        !store
            .abandon_provider_turn_custody(next.invocation_id, old.boot_id)
            .unwrap()
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn insert_roundtrips_canonical_identity_and_optional_platform_start_time() {
    let store = Store::open_in_memory().unwrap();
    let turn = turn(&store);
    store.insert_provider_turn_custody(&turn).unwrap();
    let record = get(&store, &turn);
    assert_eq!(record.invocation_id, turn.invocation_id);
    assert_eq!(record.session_id, turn.session_id);
    assert_eq!(record.spool_dir, turn.spool_dir);
    assert_eq!(record.pid, turn.pid);
    assert_eq!(record.start_time, turn.start_time);
    assert_eq!(record.boot_id, turn.boot_id);
    assert_eq!(record.stdout_offset, 0);
    assert_eq!(record.state, ProviderTurnCustodyState::Live);
    assert_eq!(record.created_at, record.updated_at);
    let (uuid, timestamp): (String, String) = store
        .conn
        .query_row(
            "SELECT invocation_id,created_at FROM provider_turn_custody WHERE invocation_id=?1",
            [turn.invocation_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(uuid, turn.invocation_id.to_string());
    assert_eq!(
        timestamp,
        record
            .created_at
            .to_rfc3339_opts(SecondsFormat::Nanos, true)
    );
    for state in [
        ProviderTurnCustodyState::Live,
        ProviderTurnCustodyState::Adopted,
        ProviderTurnCustodyState::Finished,
        ProviderTurnCustodyState::Abandoned,
    ] {
        assert_eq!(serde_json::to_value(state).unwrap(), state.as_str());
    }
    let mut portable = self::turn(&store);
    portable.start_time = None;
    store.insert_provider_turn_custody(&portable).unwrap();
    assert_eq!(get(&store, &portable).start_time, None);
    assert_eq!(store.list_active_provider_turn_custody().unwrap().len(), 2);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn cursor_progress_requires_current_boot_and_exact_previous_offset() {
    let store = Store::open_in_memory().unwrap();
    let turn = turn(&store);
    store.insert_provider_turn_custody(&turn).unwrap();
    assert!(
        store
            .advance_provider_turn_stdout_offset(turn.invocation_id, turn.boot_id, 0, 80)
            .unwrap()
    );
    assert!(
        !store
            .advance_provider_turn_stdout_offset(turn.invocation_id, turn.boot_id, 0, 90)
            .unwrap()
    );
    assert!(
        !store
            .advance_provider_turn_stdout_offset(turn.invocation_id, Uuid::new_v4(), 80, 90)
            .unwrap()
    );
    assert!(
        !store
            .advance_provider_turn_stdout_offset(turn.invocation_id, turn.boot_id, 80, 80)
            .unwrap()
    );
    assert!(
        store
            .advance_provider_turn_stdout_offset(turn.invocation_id, turn.boot_id, 80, 79)
            .is_err()
    );
    assert!(
        store
            .advance_provider_turn_stdout_offset(turn.invocation_id, turn.boot_id, 80, u64::MAX)
            .is_err()
    );
    assert_eq!(get(&store, &turn).stdout_offset, 80);
    assert!(
        store
            .advance_provider_turn_stdout_offset(turn.invocation_id, turn.boot_id, 80, 120)
            .unwrap()
    );
    assert_eq!(get(&store, &turn).stdout_offset, 120);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn restart_claim_is_atomic_across_connections_and_fences_old_readers() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("custody.sqlite");
    let store = Store::open_test_fixture(&path).unwrap();
    let turn = turn(&store);
    store.insert_provider_turn_custody(&turn).unwrap();
    store
        .advance_provider_turn_stdout_offset(turn.invocation_id, turn.boot_id, 0, 123)
        .unwrap();
    let snapshot = get(&store, &turn);
    drop(store);

    let winner = Store::open(&path).unwrap();
    let loser = Store::open(&path).unwrap();
    assert_eq!(get(&winner, &turn), snapshot);
    let next_boot = Uuid::new_v4();
    assert!(
        winner
            .claim_provider_turn_custody(turn.invocation_id, turn.boot_id, next_boot)
            .unwrap()
    );
    assert!(
        !loser
            .claim_provider_turn_custody(turn.invocation_id, turn.boot_id, Uuid::new_v4())
            .unwrap()
    );
    assert!(
        !winner
            .claim_provider_turn_custody(turn.invocation_id, turn.boot_id, next_boot)
            .unwrap()
    );
    assert!(
        !loser
            .advance_provider_turn_stdout_offset(turn.invocation_id, turn.boot_id, 123, 200)
            .unwrap()
    );
    assert!(
        !loser
            .finish_provider_turn_custody(turn.invocation_id, turn.boot_id)
            .unwrap()
    );
    assert!(
        !loser
            .abandon_provider_turn_custody(turn.invocation_id, turn.boot_id)
            .unwrap()
    );
    let adopted = get(&winner, &turn);
    assert_eq!(adopted.state, ProviderTurnCustodyState::Adopted);
    assert_eq!(adopted.boot_id, next_boot);
    assert_eq!(adopted.stdout_offset, 123);
    assert_eq!(adopted.created_at, snapshot.created_at);
    assert!(
        winner
            .advance_provider_turn_stdout_offset(turn.invocation_id, next_boot, 123, 200)
            .unwrap()
    );
    let third_boot = Uuid::new_v4();
    assert!(
        winner
            .claim_provider_turn_custody(turn.invocation_id, next_boot, third_boot)
            .unwrap()
    );
    assert_eq!(get(&winner, &turn).boot_id, third_boot);
    assert!(
        winner
            .finish_provider_turn_custody(turn.invocation_id, third_boot)
            .unwrap()
    );
    drop(winner);
    drop(loser);
    let reopened = Store::open(&path).unwrap();
    assert_eq!(
        get(&reopened, &turn).state,
        ProviderTurnCustodyState::Finished
    );
    assert_eq!(get(&reopened, &turn).stdout_offset, 200);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn terminal_custody_retains_identity_and_releases_the_active_session_slot() {
    let store = Store::open_in_memory().unwrap();
    for state in [
        ProviderTurnCustodyState::Finished,
        ProviderTurnCustodyState::Abandoned,
    ] {
        let turn = turn(&store);
        store.insert_provider_turn_custody(&turn).unwrap();
        let settled = match state {
            ProviderTurnCustodyState::Finished => {
                store.finish_provider_turn_custody(turn.invocation_id, turn.boot_id)
            }
            _ => store.abandon_provider_turn_custody(turn.invocation_id, turn.boot_id),
        };
        assert!(settled.unwrap());
        let terminal = get(&store, &turn);
        assert_eq!(terminal.state, state);
        assert!(
            !store
                .finish_provider_turn_custody(turn.invocation_id, turn.boot_id)
                .unwrap()
        );
        assert!(
            !store
                .abandon_provider_turn_custody(turn.invocation_id, turn.boot_id)
                .unwrap()
        );
        assert!(
            !store
                .claim_provider_turn_custody(turn.invocation_id, turn.boot_id, Uuid::new_v4())
                .unwrap()
        );
        assert!(
            !store
                .advance_provider_turn_stdout_offset(turn.invocation_id, turn.boot_id, 0, 100)
                .unwrap()
        );
        assert_eq!(get(&store, &turn), terminal);
        assert!(
            store
                .list_active_provider_turn_custody()
                .unwrap()
                .is_empty()
        );
        let next_invocation = invocation(&store, turn.session_id);
        let next = NewProviderTurnCustody {
            invocation_id: next_invocation,
            spool_dir: turn.spool_dir.with_file_name(next_invocation.to_string()),
            ..turn.clone()
        };
        store.insert_provider_turn_custody(&next).unwrap();
        assert_eq!(get(&store, &next).state, ProviderTurnCustodyState::Live);
        store
            .finish_provider_turn_custody(next.invocation_id, next.boot_id)
            .unwrap();
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn refuses_conflicting_identity_spools_and_invalid_input_without_mutation() {
    let store = Store::open_in_memory().unwrap();
    let turn = turn(&store);
    store.insert_provider_turn_custody(&turn).unwrap();
    let original = get(&store, &turn);
    assert!(store.insert_provider_turn_custody(&turn).is_err());
    let other = self::turn(&store);
    let invalid = [
        NewProviderTurnCustody {
            invocation_id: other.invocation_id,
            ..turn.clone()
        },
        NewProviderTurnCustody {
            invocation_id: invocation(&store, turn.session_id),
            spool_dir: other.spool_dir.clone(),
            ..turn.clone()
        },
        NewProviderTurnCustody {
            spool_dir: turn.spool_dir.clone(),
            ..other.clone()
        },
        NewProviderTurnCustody {
            invocation_id: Uuid::new_v4(),
            ..other.clone()
        },
        NewProviderTurnCustody {
            boot_id: Uuid::nil(),
            ..other.clone()
        },
        NewProviderTurnCustody {
            pid: 0,
            ..other.clone()
        },
        NewProviderTurnCustody {
            pid: u32::MAX,
            ..other.clone()
        },
        NewProviderTurnCustody {
            start_time: Some(u64::MAX),
            ..other.clone()
        },
        NewProviderTurnCustody {
            spool_dir: PathBuf::from("relative"),
            ..other.clone()
        },
        NewProviderTurnCustody {
            spool_dir: other.spool_dir.join("../elsewhere"),
            ..other.clone()
        },
    ];
    for invalid in invalid {
        assert!(store.insert_provider_turn_custody(&invalid).is_err());
    }
    assert_eq!(get(&store, &turn), original);
    assert_eq!(
        store.list_active_provider_turn_custody().unwrap(),
        vec![original]
    );
    assert_eq!(
        store
            .get_provider_turn_custody(other.invocation_id)
            .unwrap(),
        None
    );
    assert!(
        !store
            .finish_provider_turn_custody(Uuid::new_v4(), turn.boot_id)
            .unwrap()
    );
    assert!(
        store
            .claim_provider_turn_custody(turn.invocation_id, turn.boot_id, Uuid::nil())
            .is_err()
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn migration_replays_from_previous_head_without_changing_existing_rows() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("upgrade.sqlite");
    let store = Store::open_test_fixture(&path).unwrap();
    crate::store::tests::rewind_post_v121_tail_to(&store.conn, 160);
    let turn = turn(&store);
    let session_before = store.get_session(turn.session_id).unwrap().unwrap();
    let old_catalog = crate::store::catalog_sql_by_name(&store.conn);
    drop(store);
    let store = Store::open(&path).unwrap();
    assert_eq!(
        store
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get::<_, i32>(0))
            .unwrap(),
        crate::store::LATEST_SCHEMA_VERSION
    );
    let session_after = store.get_session(turn.session_id).unwrap().unwrap();
    assert_eq!(
        serde_json::to_value(session_after).unwrap(),
        serde_json::to_value(session_before).unwrap()
    );
    crate::store::assert_catalog_preserved_across_replay(&old_catalog, &store.conn, 160);
    store.insert_provider_turn_custody(&turn).unwrap();
    assert_eq!(get(&store, &turn).state, ProviderTurnCustodyState::Live);
    let violations: i64 = store
        .conn
        .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(violations, 0);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn sql_constraints_enforce_forward_custody_and_canonical_values() {
    let store = Store::open_in_memory().unwrap();
    let turn = turn(&store);
    store.insert_provider_turn_custody(&turn).unwrap();
    for assignment in [
        "stdout_offset=-1",
        "stdout_offset=1.5",
        "pid=0",
        "state='unknown'",
        "boot_id='BAD'",
        "updated_at='2026-10-07T00:00:00Z'",
        "spool_dir='/different'",
        "start_time=NULL",
    ] {
        assert!(
            store
                .conn
                .execute(
                    &format!(
                        // sql-dynamic-ok: assignments are fixed test fixture literals above.
                        "UPDATE provider_turn_custody SET {assignment} WHERE invocation_id=?1"
                    ),
                    [turn.invocation_id.to_string()]
                )
                .is_err()
        );
    }
    store
        .advance_provider_turn_stdout_offset(turn.invocation_id, turn.boot_id, 0, 50)
        .unwrap();
    assert!(
        store
            .conn
            .execute(
                "UPDATE provider_turn_custody SET stdout_offset=49 WHERE invocation_id=?1",
                [turn.invocation_id.to_string()]
            )
            .is_err()
    );
    let next_boot = Uuid::new_v4();
    store
        .claim_provider_turn_custody(turn.invocation_id, turn.boot_id, next_boot)
        .unwrap();
    assert!(
        store
            .conn
            .execute(
                "UPDATE provider_turn_custody SET state='live' WHERE invocation_id=?1",
                [turn.invocation_id.to_string()]
            )
            .is_err()
    );
    store
        .abandon_provider_turn_custody(turn.invocation_id, next_boot)
        .unwrap();
    assert_eq!(
        get(&store, &turn).state,
        ProviderTurnCustodyState::Abandoned
    );
    assert!(
        store
            .conn
            .execute(
                "UPDATE provider_turn_custody SET state='adopted' WHERE invocation_id=?1",
                [turn.invocation_id.to_string()]
            )
            .is_err()
    );
}

fn line_event(turn: &NewProviderTurnCustody, sequence: i32) -> ProviderTurnEvent {
    ProviderTurnEvent {
        event: ConversationEvent {
            id: 0,
            session_id: turn.session_id,
            sequence,
            event_type: rsi_common::types::EventType::Message,
            role: Some(rsi_common::types::Role::Assistant),
            content: format!("block {sequence}"),
            tool_name: None,
            tool_input: None,
            created_at: Utc::now(),
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        },
        provenance: Some(ConversationEventProvenanceV1 {
            producer_kind:
                rsi_common::closure_kernel::ConversationEventProducerKindV1::ProviderAssistantOutput,
            model_invocation_id: turn.invocation_id,
            provider_event_type: "assistant".into(),
        }),
        question: None,
    }
}

fn cursor(
    turn: &NewProviderTurnCustody,
    expected_offset: u64,
    next_offset: u64,
) -> ProviderTurnCursor {
    ProviderTurnCursor {
        invocation_id: turn.invocation_id,
        boot_id: turn.boot_id,
        expected_offset,
        next_offset,
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn line_batch_and_cursor_commit_together_and_stale_readers_write_nothing() {
    let store = Store::open_in_memory().unwrap();
    let turn = turn(&store);
    store.insert_provider_turn_custody(&turn).unwrap();
    let batch = [line_event(&turn, 1), line_event(&turn, 2)];
    let ids = store
        .consume_provider_turn_line(cursor(&turn, 0, 100), &batch)
        .unwrap()
        .unwrap();
    assert_eq!(ids.len(), 2);
    assert_eq!(store.load_events(turn.session_id).unwrap().len(), 2);
    assert_eq!(get(&store, &turn).stdout_offset, 100);
    assert!(
        store
            .consume_provider_turn_line(cursor(&turn, 0, 100), &batch)
            .unwrap()
            .is_none()
    );
    let other_boot = Uuid::new_v4();
    assert!(
        store
            .claim_provider_turn_custody(turn.invocation_id, turn.boot_id, other_boot)
            .unwrap()
    );
    assert!(
        store
            .consume_provider_turn_line(cursor(&turn, 100, 200), &batch)
            .unwrap()
            .is_none()
    );
    assert_eq!(store.load_events(turn.session_id).unwrap().len(), 2);
    assert_eq!(get(&store, &turn).stdout_offset, 100);
    let provenance_count: i64 = store
        .conn
        .query_row(
            "SELECT count(*) FROM conversation_event_provenance",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(provenance_count, 2);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn failed_second_event_rolls_back_every_block_provenance_and_cursor() {
    let store = Store::open_in_memory().unwrap();
    let turn = turn(&store);
    store.insert_provider_turn_custody(&turn).unwrap();
    store
        .conn
        .execute_batch(
            "CREATE TRIGGER refuse_second_line_block BEFORE INSERT ON conversation_events
        WHEN NEW.sequence=2 BEGIN SELECT RAISE(ABORT,'fixture insert failure'); END;",
        )
        .unwrap();
    let batch = [line_event(&turn, 1), line_event(&turn, 2)];
    assert!(
        store
            .consume_provider_turn_line(cursor(&turn, 0, 100), &batch)
            .is_err()
    );
    assert_eq!(store.load_events(turn.session_id).unwrap().len(), 0);
    assert_eq!(get(&store, &turn).stdout_offset, 0);
    store
        .conn
        .execute_batch("DROP TRIGGER refuse_second_line_block")
        .unwrap();
    assert_eq!(
        store
            .consume_provider_turn_line(cursor(&turn, 0, 100), &batch)
            .unwrap()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(get(&store, &turn).stdout_offset, 100);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn empty_lines_progress_but_cross_session_and_nonadvancing_batches_refuse() {
    let store = Store::open_in_memory().unwrap();
    let turn = turn(&store);
    store.insert_provider_turn_custody(&turn).unwrap();
    assert_eq!(
        store
            .consume_provider_turn_line(cursor(&turn, 0, 1), &[])
            .unwrap(),
        Some(vec![])
    );
    assert!(
        store
            .consume_provider_turn_line(cursor(&turn, 1, 1), &[])
            .is_err()
    );
    let mut wrong = line_event(&turn, 1);
    wrong.event.session_id = self::turn(&store).session_id;
    assert!(
        store
            .consume_provider_turn_line(cursor(&turn, 1, 100), &[wrong])
            .is_err()
    );
    assert_eq!(get(&store, &turn).stdout_offset, 1);
    assert_eq!(store.load_events(turn.session_id).unwrap().len(), 0);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn question_line_binds_exact_publication_and_failed_frame_keeps_it_unresolved() {
    let store = Store::open_in_memory().unwrap();
    let turn = turn(&store);
    store.insert_provider_turn_custody(&turn).unwrap();
    let question: PendingQuestion = serde_json::from_value(serde_json::json!({"questions":[{
        "question":"Choose a route", "header":"Route", "options":[], "multiSelect":false
    }]}))
    .unwrap();
    let mut item = line_event(&turn, 1);
    item.event.event_type = rsi_common::types::EventType::ToolUse;
    item.provenance.as_mut().unwrap().producer_kind =
        rsi_common::closure_kernel::ConversationEventProducerKindV1::ProviderOther;
    item.event.tool_name = Some("AskUserQuestion".into());
    item.event.tool_use_id = Some("question-1".into());
    item.event.tool_input = Some(Box::new(serde_json::to_value(&question).unwrap()));
    item.question = Some(question);
    let ids = store
        .consume_provider_turn_line(cursor(&turn, 0, 80), &[item])
        .unwrap()
        .unwrap();
    let target = store
        .pending_question_target(turn.session_id)
        .unwrap()
        .unwrap();
    assert_eq!(target["event_id"], ids[0]);
    let mut next = line_event(&turn, 2);
    next.event.event_type = rsi_common::types::EventType::ToolUse;
    next.provenance.as_mut().unwrap().producer_kind =
        rsi_common::closure_kernel::ConversationEventProducerKindV1::ProviderOther;
    next.event.tool_name = Some("AskUserQuestion".into());
    next.event.tool_use_id = Some("question-2".into());
    next.event.tool_input = Some(Box::new(serde_json::json!({"questions":[]})));
    next.question = Some(serde_json::from_value(serde_json::json!({"questions":[]})).unwrap());
    store
        .conn
        .execute_batch(
            "CREATE TRIGGER refuse_new_question BEFORE INSERT ON conversation_events
        WHEN NEW.sequence=2 BEGIN SELECT RAISE(ABORT,'fixture failure'); END",
        )
        .unwrap();
    assert!(
        store
            .consume_provider_turn_line(cursor(&turn, 80, 160), &[next])
            .is_err()
    );
    assert_eq!(get(&store, &turn).stdout_offset, 80);
    assert_eq!(store.load_events(turn.session_id).unwrap().len(), 1);
    let state: String = store
        .conn
        .query_row(
            "SELECT state FROM pending_question_publications WHERE session_id=?1",
            [turn.session_id.to_string()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(state, "unresolved");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn retained_turn_history_clearly_refuses_permanent_purge_and_preserves_soft_delete() {
    let store = Store::open_in_memory().unwrap();
    let turn = turn(&store);
    store.insert_provider_turn_custody(&turn).unwrap();
    assert!(
        store
            .finish_provider_turn_custody(turn.invocation_id, turn.boot_id)
            .unwrap()
    );
    for error in [
        store.purge_session(turn.session_id).unwrap_err(),
        store.delete_session(turn.session_id).unwrap_err(),
    ] {
        assert!(
            error.to_string().contains("session_turn_custody_retained"),
            "{error}"
        );
    }
    store.soft_delete_session(turn.session_id).unwrap();
    assert_eq!(
        store.get_session(turn.session_id).unwrap().unwrap().status,
        rsi_common::types::SessionStatus::Deleted
    );
    assert_eq!(get(&store, &turn).state, ProviderTurnCustodyState::Finished);
}

/// Every column of one stored custody row, to prove an attack changed nothing.
fn custody_snapshot(store: &Store) -> Vec<Vec<String>> {
    let mut statement = store
        .conn
        .prepare(
            "SELECT invocation_id,session_id,spool_dir,pid,COALESCE(start_time,-1),boot_id,
                    stdout_offset,state,created_at,updated_at,COALESCE(worker_scope,'')
             FROM provider_turn_custody ORDER BY invocation_id",
        )
        .unwrap();
    statement
        .query_map([], |row| {
            (0..11)
                .map(|index| {
                    row.get::<_, rusqlite::types::Value>(index)
                        .map(|v| format!("{v:?}"))
                })
                .collect()
        })
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn raw_sql_cannot_delete_or_replace_custody_identity_or_terminal_history_1671() {
    let store = Store::open_in_memory().unwrap();
    let live = turn(&store);
    store.insert_provider_turn_custody(&live).unwrap();
    let finished = turn(&store);
    store.insert_provider_turn_custody(&finished).unwrap();
    assert!(
        store
            .finish_provider_turn_custody(finished.invocation_id, finished.boot_id)
            .unwrap()
    );
    let abandoned = turn(&store);
    store.insert_provider_turn_custody(&abandoned).unwrap();
    store
        .abandon_provider_turn_custody(abandoned.invocation_id, abandoned.boot_id)
        .unwrap();
    let before = custody_snapshot(&store);
    assert_eq!(before.len(), 3);

    // Deleting a live, finished or abandoned row is refused, singly and in bulk.
    for turn in [&live, &finished, &abandoned] {
        let error = store
            .conn
            .execute(
                "DELETE FROM provider_turn_custody WHERE invocation_id=?1",
                [turn.invocation_id.to_string()],
            )
            .unwrap_err();
        assert!(error.to_string().contains("append only"), "{error}");
    }
    assert!(
        store
            .conn
            .execute("DELETE FROM provider_turn_custody", [])
            .is_err()
    );

    // Replacing a row by any of its unique identities is refused: the same invocation,
    // the same spool directory under a new invocation, and a second active turn for
    // the live row's session (which the partial unique index would otherwise replace).
    let other_session_invocation = invocation(&store, live.session_id);
    for (invocation, session, spool, state) in [
        (
            finished.invocation_id,
            finished.session_id,
            finished.spool_dir.clone(),
            "live",
        ),
        (
            finished.invocation_id,
            finished.session_id,
            finished.spool_dir.clone(),
            "finished",
        ),
        (
            abandoned.invocation_id,
            abandoned.session_id,
            "/other-spool".into(),
            "abandoned",
        ),
        (
            other_session_invocation,
            live.session_id,
            "/second-active".into(),
            "live",
        ),
        (
            other_session_invocation,
            live.session_id,
            live.spool_dir.clone(),
            "adopted",
        ),
        (
            other_session_invocation,
            live.session_id,
            finished.spool_dir.clone(),
            "finished",
        ),
    ] {
        for verb in ["INSERT OR REPLACE", "REPLACE", "INSERT OR IGNORE", "INSERT"] {
            let result = store.conn.execute(
                // sql-dynamic-ok: verbs are fixed test fixture literals above.
                &format!(
                    "{verb} INTO provider_turn_custody
                     (invocation_id,session_id,spool_dir,pid,start_time,boot_id,stdout_offset,state,created_at,updated_at)
                     VALUES(?1,?2,?3,999,1,?4,0,?5,?6,?6)"
                ),
                params![
                    invocation.to_string(),
                    session.to_string(),
                    spool.to_string_lossy(),
                    Uuid::new_v4().to_string(),
                    state,
                    now(),
                ],
            );
            assert!(
                result.is_err_and(|e| e.to_string().contains("cannot be replaced")),
                "{verb} {state} must be refused"
            );
        }
    }
    // An upsert collision cannot rewrite identity either.
    assert!(
        store
            .conn
            .execute(
                "INSERT INTO provider_turn_custody
                 (invocation_id,session_id,spool_dir,pid,start_time,boot_id,stdout_offset,state,created_at,updated_at)
                 VALUES(?1,?2,'/x',999,1,?3,0,'live',?4,?4)
                 ON CONFLICT(invocation_id) DO UPDATE SET pid=999,state='live'",
                params![
                    finished.invocation_id.to_string(),
                    finished.session_id.to_string(),
                    Uuid::new_v4().to_string(),
                    now()
                ],
            )
            .is_err()
    );
    // Replacement stays refused when recursive triggers would also fire the delete guard.
    store
        .conn
        .execute_batch("PRAGMA recursive_triggers=ON")
        .unwrap();
    assert!(
        store
            .conn
            .execute("DELETE FROM provider_turn_custody", [])
            .is_err()
    );

    assert_eq!(custody_snapshot(&store), before);
    assert_eq!(get(&store, &live).state, ProviderTurnCustodyState::Live);
    assert_eq!(
        get(&store, &finished).state,
        ProviderTurnCustodyState::Finished
    );
    assert_eq!(
        get(&store, &abandoned).state,
        ProviderTurnCustodyState::Abandoned
    );
    // A legitimate new turn for a session with only terminal history still inserts.
    let next = invocation(&store, finished.session_id);
    store
        .insert_provider_turn_custody(&NewProviderTurnCustody {
            invocation_id: next,
            spool_dir: finished.spool_dir.with_file_name(next.to_string()),
            boot_id: Uuid::new_v4(),
            ..finished.clone()
        })
        .unwrap();
    assert_eq!(custody_snapshot(&store).len(), 4);
}

/// #1671: the table's primary key is TEXT, so the implicit rowid is a unique key too. A
/// REPLACE that collides only on the rowid, or an UPDATE that moves a row onto another
/// row's rowid, deletes the occupant without firing the delete guard.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn raw_sql_cannot_replace_custody_through_the_implicit_rowid_1671() {
    let store = Store::open_in_memory().unwrap();
    let finished = turn(&store);
    store.insert_provider_turn_custody(&finished).unwrap();
    assert!(
        store
            .finish_provider_turn_custody(finished.invocation_id, finished.boot_id)
            .unwrap()
    );
    let live = turn(&store);
    store.insert_provider_turn_custody(&live).unwrap();
    let terminal_rowid: i64 = store
        .conn
        .query_row(
            "SELECT rowid FROM provider_turn_custody WHERE invocation_id=?1",
            [finished.invocation_id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    let before = custody_snapshot(&store);
    assert_eq!(before.len(), 2);

    // A fresh, valid invocation with fresh spool and every other key distinct, aimed at
    // the terminal row's rowid through each alias and each replacing verb.
    let fresh_session = turn(&store);
    for column in ["rowid", "_rowid_", "oid"] {
        for verb in ["INSERT OR REPLACE", "REPLACE"] {
            let result = store.conn.execute(
                // sql-dynamic-ok: verbs and aliases are fixed test fixture literals above.
                &format!(
                    "{verb} INTO provider_turn_custody
                     ({column},invocation_id,session_id,spool_dir,pid,start_time,boot_id,stdout_offset,state,created_at,updated_at)
                     VALUES(?1,?2,?3,?4,999,1,?5,0,'live',?6,?6)"
                ),
                params![
                    terminal_rowid,
                    fresh_session.invocation_id.to_string(),
                    fresh_session.session_id.to_string(),
                    fresh_session.spool_dir.to_string_lossy(),
                    Uuid::new_v4().to_string(),
                    now(),
                ],
            );
            assert!(
                result.is_err_and(|e| e.to_string().contains("cannot be replaced")),
                "{verb} through {column} must be refused"
            );
        }
    }

    // Moving a live row onto the terminal row's rowid is refused for every conflict mode.
    for mode in ["", "OR REPLACE", "OR IGNORE", "OR FAIL"] {
        let result = store.conn.execute(
            // sql-dynamic-ok: modes are fixed test fixture literals above.
            &format!("UPDATE {mode} provider_turn_custody SET rowid=?1 WHERE invocation_id=?2"),
            params![terminal_rowid, live.invocation_id.to_string()],
        );
        assert!(
            result.is_err_and(|e| e.to_string().contains("rowid is immutable")),
            "UPDATE {mode} SET rowid must be refused"
        );
    }
    // So is any rowid change, onto an unused value too.
    assert!(
        store
            .conn
            .execute(
                "UPDATE provider_turn_custody SET rowid=rowid+1000 WHERE invocation_id=?1",
                [live.invocation_id.to_string()],
            )
            .is_err()
    );

    assert_eq!(custody_snapshot(&store), before);
    assert_eq!(
        get(&store, &finished).state,
        ProviderTurnCustodyState::Finished
    );
    assert_eq!(get(&store, &live).state, ProviderTurnCustodyState::Live);
    // The legitimate lifecycle still updates the live row (rowid unchanged).
    assert!(
        store
            .finish_provider_turn_custody(live.invocation_id, live.boot_id)
            .unwrap()
    );
    assert_eq!(get(&store, &live).state, ProviderTurnCustodyState::Finished);
}

#[cfg(target_os = "linux")]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn adoptable_turn_requires_lock_incarnation_and_exact_environment() {
    let store = Store::open_in_memory().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut input = turn(&store);
    input.spool_dir = dir.path().join("spool");
    std::fs::create_dir(&input.spool_dir).unwrap();
    let lock = std::fs::File::create(input.spool_dir.join("alive.lock")).unwrap();
    store.insert_provider_turn_custody(&input).unwrap();
    let row = get(&store, &input);
    let root = dir.path().join("proc");
    std::fs::create_dir_all(root.join("self")).unwrap();
    let process = root.join(input.pid.to_string());
    std::fs::create_dir(&process).unwrap();
    let stat = format!(
        "{} (shim) S {} {}",
        input.pid,
        "0 ".repeat(18),
        input.start_time.unwrap()
    );
    std::fs::write(process.join("stat"), stat).unwrap();
    let env = format!(
        "RSI_SESSION_ID={}\0RSI_MODEL_INVOCATION_ID={}\0RSI_PROCESS_OWNERSHIP_NAMESPACE=test-domain\0",
        input.session_id, input.invocation_id
    );
    std::fs::write(process.join("environ"), &env).unwrap();
    assert!(!row.is_adoptable_at(&root, b"test-domain").unwrap());
    lock.lock().unwrap();
    assert!(row.is_adoptable_at(&root, b"test-domain").unwrap());
    assert!(!row.is_adoptable_at(&root, b"other-domain").unwrap());
    let mut reused = row.clone();
    reused.start_time = Some(input.start_time.unwrap() + 1);
    assert!(!reused.is_adoptable_at(&root, b"test-domain").unwrap());
    reused.start_time = None;
    assert!(!reused.is_adoptable_at(&root, b"test-domain").unwrap());
    reused = row.clone();
    reused.state = ProviderTurnCustodyState::Finished;
    assert!(!reused.is_adoptable_at(&root, b"test-domain").unwrap());
    std::fs::write(
        process.join("environ"),
        format!("{env}RSI_SESSION_ID={}\0", input.session_id),
    )
    .unwrap();
    assert!(!row.is_adoptable_at(&root, b"test-domain").unwrap());
    std::fs::write(
        process.join("environ"),
        env.replace(
            &input.invocation_id.to_string(),
            &Uuid::new_v4().to_string(),
        ),
    )
    .unwrap();
    assert!(!row.is_adoptable_at(&root, b"test-domain").unwrap());
    lock.unlock().unwrap();
    assert!(!row.is_adoptable_at(&root, b"test-domain").unwrap());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn startup_reconciliation_preserves_only_current_boot_exact_detached_invocation() {
    let store = Store::open_in_memory().unwrap();
    let old = turn(&store);
    store.insert_provider_turn_custody(&old).unwrap();
    store
        .set_session_model_invocation(old.session_id, Some(old.invocation_id))
        .unwrap();
    let boot = store.program_run_boot_id();
    assert!(
        store
            .claim_provider_turn_custody(old.invocation_id, old.boot_id, boot)
            .unwrap()
    );
    store.conn.execute("UPDATE model_invocations SET purpose='session.launch.fresh',invocation_kind='session_lifecycle',paid_risk='paid_capable' WHERE id=?1", [old.invocation_id.to_string()]).unwrap();
    assert_eq!(store.reconcile_running_model_invocations().unwrap(), 0);
    let other = invocation(&store, old.session_id);
    store.conn.execute("UPDATE model_invocations SET purpose='session.launch.fresh',invocation_kind='session_lifecycle',paid_risk='paid_capable' WHERE id=?1", [other.to_string()]).unwrap();
    assert_eq!(store.reconcile_running_model_invocations().unwrap(), 1);
    assert_eq!(
        store
            .conn
            .query_row(
                "SELECT status FROM model_invocations WHERE id=?1",
                [other.to_string()],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
        "failed"
    );
    assert_eq!(
        store
            .conn
            .query_row(
                "SELECT status FROM model_invocations WHERE id=?1",
                [old.invocation_id.to_string()],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
        "running"
    );
    store.set_program_run_boot_id(Uuid::new_v4()).unwrap();
    assert_eq!(store.reconcile_running_model_invocations().unwrap(), 1);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn finished_spool_cleanup_preserves_live_adopted_and_stale_boot_custody() {
    let store = Store::open_in_memory().unwrap();
    let dir = tempfile::tempdir().unwrap(); // tmpfs-fixture-ok: spool only
    let mut turn = turn(&store);
    turn.spool_dir = dir.path().join(turn.invocation_id.to_string());
    std::fs::create_dir(&turn.spool_dir).unwrap();
    std::fs::write(turn.spool_dir.join("stdin"), "private prompt").unwrap();
    std::fs::write(turn.spool_dir.join("stdout"), "output").unwrap();
    store.insert_provider_turn_custody(&turn).unwrap();
    let lock = std::fs::File::create(turn.spool_dir.join("alive.lock")).unwrap();
    lock.lock().unwrap();
    assert!(
        !store
            .finish_provider_turn_custody(turn.invocation_id, turn.boot_id)
            .unwrap()
    );
    store.cleanup_ended_provider_turn_spools().unwrap();
    assert_eq!(get(&store, &turn).state, ProviderTurnCustodyState::Live);
    assert!(turn.spool_dir.join("stdin").is_file());
    let boot = Uuid::new_v4();
    assert!(
        store
            .claim_provider_turn_custody(turn.invocation_id, turn.boot_id, boot)
            .unwrap()
    );
    store.cleanup_ended_provider_turn_spools().unwrap();
    assert!(turn.spool_dir.join("stdout").is_file());
    assert!(
        !store
            .finish_provider_turn_custody(turn.invocation_id, boot)
            .unwrap()
    );
    assert_eq!(get(&store, &turn).state, ProviderTurnCustodyState::Adopted);
    drop(lock);
    assert!(
        !store
            .finish_provider_turn_custody(turn.invocation_id, turn.boot_id)
            .unwrap()
    );
    assert!(turn.spool_dir.exists());
    assert!(
        store
            .finish_provider_turn_custody(turn.invocation_id, boot)
            .unwrap()
    );
    assert_eq!(get(&store, &turn).state, ProviderTurnCustodyState::Finished);
    assert!(!turn.spool_dir.exists());
    assert!(
        !store
            .finish_provider_turn_custody(turn.invocation_id, boot)
            .unwrap()
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn startup_cleanup_removes_finished_and_abandoned_spools_once_the_shim_is_gone() {
    let store = Store::open_in_memory().unwrap();
    let dir = tempfile::tempdir().unwrap(); // tmpfs-fixture-ok: spool only
    for abandoned in [false, true] {
        let mut turn = turn(&store);
        turn.spool_dir = dir.path().join(turn.invocation_id.to_string());
        std::fs::create_dir(&turn.spool_dir).unwrap();
        std::fs::write(turn.spool_dir.join("output.json"), "decoder metadata").unwrap();
        std::fs::write(turn.spool_dir.join("stderr"), "failure evidence").unwrap();
        store.insert_provider_turn_custody(&turn).unwrap();
        // Simulate a crash between the state commit and filesystem cleanup.
        let state = if abandoned {
            ProviderTurnCustodyState::Abandoned
        } else {
            ProviderTurnCustodyState::Finished
        };
        store
            .settle_provider_turn_custody(turn.invocation_id, turn.boot_id, state)
            .unwrap();
        let lock = std::fs::File::create(turn.spool_dir.join("alive.lock")).unwrap();
        lock.lock().unwrap();
        store.cleanup_ended_provider_turn_spools().unwrap();
        assert!(turn.spool_dir.join("output.json").is_file());
        drop(lock);
        store.cleanup_ended_provider_turn_spools().unwrap();
        // An abandoned turn's evidence is swept like a finished one's.
        assert!(!turn.spool_dir.exists());
        assert_eq!(get(&store, &turn).state, state);
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn abandoned_ended_turn_removes_prompt_and_retains_output_evidence() {
    let store = Store::open_in_memory().unwrap();
    let dir = tempfile::tempdir().unwrap(); // tmpfs-fixture-ok: spool only
    let mut turn = turn(&store);
    turn.spool_dir = dir.path().join(turn.invocation_id.to_string());
    std::fs::create_dir(&turn.spool_dir).unwrap();
    std::fs::write(turn.spool_dir.join("stdin"), "private prompt").unwrap();
    std::fs::write(turn.spool_dir.join("stderr"), "failure evidence").unwrap();
    store.insert_provider_turn_custody(&turn).unwrap();
    assert!(
        store
            .abandon_provider_turn_custody(turn.invocation_id, turn.boot_id)
            .unwrap()
    );
    assert!(!turn.spool_dir.join("stdin").exists());
    assert_eq!(
        std::fs::read_to_string(turn.spool_dir.join("stderr")).unwrap(),
        "failure evidence"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn launch_scope_is_immutable_and_survives_adoption() {
    let store = Store::open_in_memory().unwrap();
    let mut turn = turn(&store);
    turn.worker_scope = Some(format!("rsi-worker-{}-owned.scope", turn.invocation_id));
    store.insert_provider_turn_custody(&turn).unwrap();
    assert!(store.conn.execute(
        "UPDATE provider_turn_custody SET worker_scope='rsi-worker-forged.scope' WHERE invocation_id=?1",
        [turn.invocation_id.to_string()],
    ).is_err());
    assert!(
        store
            .claim_provider_turn_custody(turn.invocation_id, turn.boot_id, Uuid::new_v4())
            .unwrap()
    );
    assert_eq!(get(&store, &turn).worker_scope, turn.worker_scope);
}
