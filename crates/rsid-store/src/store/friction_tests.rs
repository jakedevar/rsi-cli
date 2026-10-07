//! Store tests for friction telemetry and the andon sweep (#1333, V157).

use super::*;
use crate::test_support::test_session;
use rsi_common::friction::{FrictionKind, UNCLASSIFIED};
use rsi_common::types::Project;
use std::path::PathBuf;

fn project(store: &Store) -> Uuid {
    let now = Utc::now();
    let project = Project {
        id: Uuid::new_v4(),
        name: format!("friction-{}", Uuid::new_v4()),
        path: None,
        description: None,
        color: Project::DEFAULT_COLOR.to_string(),
        context_files: None,
        created_at: now,
        updated_at: now,
    };
    store.insert_project(&project).expect("insert project");
    project.id
}

fn session(store: &Store, project_id: Option<Uuid>) -> Uuid {
    let id = Uuid::new_v4();
    let mut session = test_session(id, PathBuf::from("/tmp/friction"));
    session.project_id = project_id;
    store.insert_session(&session).expect("insert session");
    id
}

fn refusal(code: &str) -> NewFrictionEventV1 {
    NewFrictionEventV1::new(FrictionKind::AgentRefusal, &["AgentGetIssue", code])
}

fn record(store: &Store, event: NewFrictionEventV1, session: Uuid, at: DateTime<Utc>) {
    store
        .record_friction_event_at(&event.session(Some(session)), at)
        .expect("record friction");
}

fn andon_issue_count(store: &Store) -> i64 {
    store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM issues WHERE id IN (SELECT issue_id FROM andon_filings)",
            [],
            |row| row.get(0),
        )
        .unwrap()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn friction_events_infer_the_project_and_are_append_only() {
    let store = Store::open_in_memory().unwrap();
    let project_id = project(&store);
    let worker = session(&store, Some(project_id));
    let event = NewFrictionEventV1::new(FrictionKind::DeployTimeout, &["worker_mid_turn"])
        .session(Some(worker))
        .evidence("deploy", Uuid::nil());
    store.record_friction_event(&event).unwrap();

    let (signature, project, evidence): (String, Option<String>, Option<String>) = store
        .conn
        .query_row(
            "SELECT signature, project_id, evidence_ref FROM friction_events",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(signature, "deploy_timeout:worker_mid_turn");
    assert_eq!(project, Some(project_id.to_string()));
    assert_eq!(
        evidence.as_deref(),
        Some("deploy:00000000-0000-0000-0000-000000000000")
    );

    assert!(
        store
            .conn
            .execute("UPDATE friction_events SET signature='terminal:x'", [])
            .is_err()
    );
    assert!(
        store
            .conn
            .execute("DELETE FROM friction_events", [])
            .is_err()
    );
    // The schema refuses prose even past the Rust checks.
    assert!(
        store
            .conn
            .execute(
                "INSERT INTO friction_events(signature, recorded_at) VALUES (?1, ?2)",
                params!["terminal:a secret prompt", stamp(Utc::now())],
            )
            .is_err()
    );
    let refused = store.record_friction_event(&NewFrictionEventV1 {
        signature: "no prose here".into(),
        session_id: None,
        project_id: None,
        evidence_ref: None,
    });
    assert!(refused.is_err());
    // A message that is not a code is recorded as unclassified, never as text.
    let prose = NewFrictionEventV1::new(
        FrictionKind::AgentRefusal,
        &["AgentSendMessage", "the token was sk-live-123"],
    );
    assert_eq!(
        prose.signature,
        format!("agent_refusal:AgentSendMessage:{UNCLASSIFIED}")
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn friction_tables_refuse_replace_and_upsert_and_keep_the_original_row() {
    // #1396: `INSERT OR REPLACE` deletes a conflicting row without firing the
    // DELETE trigger (recursive_triggers is off); the V157 BEFORE INSERT
    // guards refuse the collision, so the retained row survives unchanged.
    let store = Store::open_in_memory().unwrap();
    let at = stamp(Utc::now());
    store
        .conn
        .execute(
            "INSERT INTO friction_events(id, signature, recorded_at) VALUES (2, 'terminal:code', ?1)",
            params![at],
        )
        .unwrap();
    for sql in [
        "INSERT OR REPLACE INTO friction_events(id, signature, recorded_at) VALUES (2, 'terminal:replaced', ?1)",
        "REPLACE INTO friction_events(id, signature, recorded_at) VALUES (2, 'terminal:replaced', ?1)",
        "INSERT INTO friction_events(id, signature, recorded_at) VALUES (2, 'terminal:replaced', ?1)
         ON CONFLICT(id) DO UPDATE SET signature=excluded.signature",
    ] {
        let refused = store.conn.execute(sql, params![at]);
        assert!(refused.is_err(), "{sql} overwrote a friction event");
    }
    let signature: String = store
        .conn
        .query_row(
            "SELECT signature FROM friction_events WHERE id=2",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(signature, "terminal:code");
    // A fresh row still appends.
    store
        .conn
        .execute(
            "INSERT INTO friction_events(signature, recorded_at) VALUES ('terminal:next', ?1)",
            params![at],
        )
        .unwrap();
    let rows: i64 = store
        .conn
        .query_row("SELECT COUNT(*) FROM friction_events", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 2);

    let project_id = Uuid::new_v4().to_string();
    let issue_id = Uuid::new_v4().to_string();
    let insert_filing = |verb: &str, project: &str, signature: &str, issue: &str| {
        store.conn.execute(
            &format!(
                "{verb} INTO andon_filings(project_id, signature, issue_id, occurrences, sessions, filed_at)
                 VALUES (?1, ?2, ?3, 9, 9, ?4)"
            ),
            params![project, signature, issue, at],
        )
    };
    insert_filing("INSERT", &project_id, "terminal:code", &issue_id).unwrap();
    let other_issue = Uuid::new_v4().to_string();
    let other_project = Uuid::new_v4().to_string();
    for verb in ["INSERT OR REPLACE", "REPLACE"] {
        // Same (project, signature), new Issue: would rewrite the dedupe ledger.
        assert!(insert_filing(verb, &project_id, "terminal:code", &other_issue).is_err());
        // Same Issue under another key: would delete the original filing.
        assert!(insert_filing(verb, &other_project, "terminal:other", &issue_id).is_err());
    }
    assert!(
        store
            .conn
            .execute(
                "INSERT INTO andon_filings(project_id, signature, issue_id, occurrences, sessions, filed_at)
                 VALUES (?1, 'terminal:code', ?2, 9, 9, ?3)
                 ON CONFLICT(project_id, signature) DO UPDATE SET issue_id=excluded.issue_id",
                params![project_id, other_issue, at],
            )
            .is_err()
    );
    let filings: Vec<(String, String, String)> = {
        let mut stmt = store
            .conn
            .prepare("SELECT project_id, signature, issue_id FROM andon_filings")
            .unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap()
    };
    assert_eq!(
        filings,
        vec![(project_id, "terminal:code".to_string(), issue_id)]
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn andon_files_one_issue_when_a_signature_repeats_across_sessions() {
    let store = Store::open_in_memory().unwrap();
    let project_id = project(&store);
    let first = session(&store, Some(project_id));
    let second = session(&store, Some(project_id));
    let now = Utc::now();

    // Three occurrences from one session: a loop, not a structural signal.
    for minutes in [30, 20, 10] {
        record(
            &store,
            refusal("stale_continuation"),
            first,
            now - Duration::minutes(minutes),
        );
    }
    // Occurrences older than the window do not count.
    record(
        &store,
        refusal("stale_continuation"),
        second,
        now - Duration::hours(25),
    );
    assert!(store.andon_sweep(now).unwrap().is_empty());
    let rollup = store
        .friction_rollup(&ListFrictionRollupRequestV1::default(), now)
        .unwrap();
    assert_eq!(rollup.rows.len(), 1);
    assert_eq!(rollup.rows[0].occurrences, 3);
    assert_eq!(rollup.rows[0].sessions, 1);
    assert!(!rollup.rows[0].due);

    record(
        &store,
        refusal("stale_continuation").evidence("session", second),
        second,
        now - Duration::minutes(5),
    );
    let filed = store.andon_sweep(now).unwrap();
    assert_eq!(filed.len(), 1);
    let filing = &filed[0];
    assert_eq!(
        filing.signature,
        "agent_refusal:AgentGetIssue:stale_continuation"
    );
    assert_eq!((filing.occurrences, filing.sessions), (4, 2));
    assert_eq!(
        filing.issue_id,
        andon_issue_id(project_id, &filing.signature)
    );
    let issue = store
        .get_issue(filing.issue_id)
        .unwrap()
        .expect("filed Issue");
    assert_eq!(issue.project_id, project_id);
    assert_eq!(
        issue.labels,
        vec!["kaizen".to_string(), "andon".to_string()]
    );
    assert!(issue.body.contains(&format!("`session:{second}`")));
    assert_eq!(count(issue.display_number), filing.display_number);

    // Dedupe: more occurrences and later sweeps never file a second Issue.
    for _ in 0..3 {
        record(&store, refusal("stale_continuation"), first, now);
    }
    assert!(store.andon_sweep(now).unwrap().is_empty());
    assert!(
        store
            .andon_sweep(now + Duration::hours(30))
            .unwrap()
            .is_empty()
    );
    assert_eq!(andon_issue_count(&store), 1);
    let rollup = store
        .friction_rollup(
            &ListFrictionRollupRequestV1 {
                project_id: Some(project_id),
                ..Default::default()
            },
            now,
        )
        .unwrap();
    assert_eq!(rollup.rows[0].filed_issue_id, Some(filing.issue_id));
    assert_eq!(
        rollup.rows[0].filed_display_number,
        Some(filing.display_number)
    );
    assert!(!rollup.rows[0].due);
    assert_eq!(rollup.filings_last_24h, 1);
    assert!(store.conn.execute("DELETE FROM andon_filings", []).is_err());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn andon_filing_is_capped_per_rolling_day() {
    let store = Store::open_in_memory().unwrap();
    let project_id = project(&store);
    let sessions = [
        session(&store, Some(project_id)),
        session(&store, Some(project_id)),
    ];
    let now = Utc::now();
    let codes: Vec<String> = (0..7).map(|n| format!("code_{n}")).collect();
    for code in &codes {
        for (index, at) in [3, 2, 1].into_iter().enumerate() {
            record(
                &store,
                refusal(code),
                sessions[index % 2],
                now - Duration::minutes(at),
            );
        }
    }
    let filed = store.andon_sweep(now).unwrap();
    assert_eq!(filed.len() as u64, ANDON_DAILY_FILING_CAP);
    assert!(store.andon_sweep(now).unwrap().is_empty());
    let rollup = store
        .friction_rollup(&ListFrictionRollupRequestV1::default(), now)
        .unwrap();
    assert_eq!(rollup.filings_last_24h, ANDON_DAILY_FILING_CAP);
    assert_eq!(rollup.rows.iter().filter(|row| row.due).count(), 2);

    // A day later the cap has room again; the two waiting signatures file
    // once they recur within the new window.
    let later = now + Duration::hours(25);
    for code in &codes {
        for (index, at) in [3, 2, 1].into_iter().enumerate() {
            record(
                &store,
                refusal(code),
                sessions[index % 2],
                later - Duration::minutes(at),
            );
        }
    }
    let refiled = store.andon_sweep(later).unwrap();
    assert_eq!(refiled.len(), 2);
    assert!(
        refiled
            .iter()
            .all(|filing| !filed.iter().any(|old| old.signature == filing.signature))
    );
    assert_eq!(andon_issue_count(&store), 7);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn friction_rollup_validates_its_window_and_never_files_projectless_rows() {
    let store = Store::open_in_memory().unwrap();
    let now = Utc::now();
    let unowned = [session(&store, None), session(&store, None)];
    for index in 0..4 {
        record(
            &store,
            NewFrictionEventV1::new(FrictionKind::DeployTimeout, &["worker_mid_turn"]),
            unowned[index % 2],
            now,
        );
    }
    assert!(store.andon_sweep(now).unwrap().is_empty());
    let rollup = store
        .friction_rollup(
            &ListFrictionRollupRequestV1 {
                limit: Some(1),
                ..Default::default()
            },
            now,
        )
        .unwrap();
    assert_eq!(rollup.rows[0].project_id, None);
    assert_eq!(rollup.rows[0].kind, "deploy_timeout");
    assert!(!rollup.rows[0].due);
    assert!(!rollup.truncated);
    for (window_hours, limit) in [(Some(0), None), (Some(721), None), (None, Some(0))] {
        assert!(
            store
                .friction_rollup(
                    &ListFrictionRollupRequestV1 {
                        project_id: None,
                        window_hours,
                        limit,
                    },
                    now,
                )
                .is_err()
        );
    }
}

fn tool_event(
    session_id: Uuid,
    sequence: i32,
    tool: &str,
    content: &str,
    is_error: bool,
) -> rsi_common::types::ConversationEvent {
    use rsi_common::types::{ConversationEvent, EventType};
    ConversationEvent {
        id: 0,
        offload_id: None,
        session_id,
        sequence,
        event_type: EventType::ToolResult,
        role: None,
        content: content.into(),
        tool_name: Some(tool.into()),
        tool_input: None,
        created_at: Utc::now(),
        tool_use_id: Some(format!("tool-{sequence}")),
        metadata: Some(Box::new(json!({"is_error": is_error}))),
    }
}

fn ingest_tool_result(
    store: &Store,
    worker: Uuid,
    sequence: i32,
    tool: &str,
    content: &str,
    is_error: bool,
) {
    let mut event = tool_event(worker, sequence, tool, content, is_error);
    event.event_type = rsi_common::types::EventType::ToolUse;
    event.content.clear();
    store.insert_event(&event).unwrap();
    event.event_type = rsi_common::types::EventType::ToolResult;
    event.tool_name = None; // result must resolve the paired call, within this session
    event.content = content.into();
    store.insert_event(&event).unwrap();
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn repeated_tool_error_friction_is_once_per_session_and_fingerprint() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("friction.db");
    let store = Store::open(&path).unwrap();
    let worker = session(&store, None);
    for sequence in 0..2 {
        ingest_tool_result(
            &store,
            worker,
            sequence,
            "Bash",
            "private failing command",
            true,
        );
    }
    assert!(
        store
            .friction_rollup(&Default::default(), Utc::now())
            .unwrap()
            .rows
            .is_empty()
    );
    drop(store);
    let store = Store::open(&path).unwrap();
    for sequence in 2..5 {
        ingest_tool_result(
            &store,
            worker,
            sequence,
            "Bash",
            "private failing command",
            true,
        );
    }
    let rows = store
        .friction_rollup(&Default::default(), Utc::now())
        .unwrap()
        .rows;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].signature, "tool_error:Bash:repeated");
    assert_eq!(rows[0].occurrences, 1);
    assert_eq!(rows[0].evidence_refs, vec![format!("session:{worker}")]);
    // A second fingerprint under the same sanitized name gets its own event.
    for sequence in 5..8 {
        ingest_tool_result(
            &store,
            worker,
            sequence,
            "Bash",
            "another private error",
            true,
        );
    }
    let rows = store
        .friction_rollup(&Default::default(), Utc::now())
        .unwrap()
        .rows;
    assert_eq!(rows[0].occurrences, 2);
    let encoded: String = store.conn.query_row(
        "SELECT json_group_array(json_object('signature',signature,'session',session_id,'project',project_id,'evidence',evidence_ref,'at',recorded_at)) FROM friction_events",
        [], |row| row.get(0)).unwrap();
    assert!(!encoded.contains("private"));
    let fingerprint: String = store.conn.query_row(
        "SELECT json_extract(metadata, '$.friction_tool_error_sha256') FROM conversation_events WHERE event_type='ToolResult' LIMIT 1",
        [], |row| row.get(0)).unwrap();
    assert_eq!(fingerprint.len(), 64);
    assert!(fingerprint.bytes().all(|b| b.is_ascii_hexdigit()));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn repeated_tool_error_friction_ignores_distinct_successful_and_other_session_results() {
    let store = Store::open_in_memory().unwrap();
    let worker = session(&store, None);
    let other = session(&store, None);
    for sequence in 0..3 {
        ingest_tool_result(
            &store,
            worker,
            sequence,
            "Bash",
            &format!("error-{sequence}"),
            true,
        );
        ingest_tool_result(&store, worker, sequence + 3, "Bash", "success", false);
    }
    for sequence in 6..8 {
        ingest_tool_result(&store, worker, sequence, "Bash", "same", true);
    }
    ingest_tool_result(&store, other, 0, "Bash", "same", true);
    assert!(
        store
            .friction_rollup(&Default::default(), Utc::now())
            .unwrap()
            .rows
            .is_empty()
    );
    for sequence in 8..11 {
        ingest_tool_result(&store, worker, sequence, "/private/tool", "same", true);
    }
    let rows = store
        .friction_rollup(&Default::default(), Utc::now())
        .unwrap()
        .rows;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].signature, "tool_error:unclassified:repeated");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
#[test]
fn repeated_tool_error_friction_insert_failure_preserves_ingestion() {
    let store = Store::open_in_memory().unwrap();
    let worker = session(&store, None);
    store.conn.execute_batch("CREATE TRIGGER refuse_test_friction BEFORE INSERT ON friction_events BEGIN SELECT RAISE(ROLLBACK, 'test refusal'); END;").unwrap();
    for sequence in 0..3 {
        ingest_tool_result(&store, worker, sequence, "Bash", "error", true);
    }
    assert_eq!(store.load_events(worker).unwrap().len(), 6);
}
