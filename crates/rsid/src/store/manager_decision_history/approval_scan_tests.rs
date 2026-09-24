use super::*;
use crate::store::harness_manager_v2::{fingerprint, now};
use rusqlite::StatementStatus;

fn mirror(store: &Store, session: Uuid, id: Uuid, pending: bool) {
    store.conn.execute("INSERT INTO approvals(id,session_id,tool_name,tool_input,status,created_at) VALUES(?1,?2,?3,'{}',?4,?5)",params![id.to_string(),session.to_string(),format!("Exact gate {id}"),if pending {"Pending"} else {"Approved"},now()]).unwrap();
}
fn native(store: &Store, session: Uuid, id: Uuid) -> (Value, String) {
    let target = json!({"kind":"appserver_approval","session_id":session,"publication_id":id});
    let stamp = now();
    store.conn.execute("INSERT INTO appserver_approval_publications(publication_id,session_id,incarnation_id,request_id_json,approval_id,state,closure_state,target_json,created_at,updated_at) VALUES(?1,?2,?1,'1',?1,'enqueued','closed',?3,?4,?4)",params![id.to_string(),session.to_string(),target.to_string(),stamp]).unwrap();
    (target, stamp)
}

#[test]
fn manager_decision_history_legacy_scan_work_stays_bounded_with_real_mirrors() {
    for history in [1030, 10300] {
        let store = Store::open_in_memory().unwrap();
        let (_, lead) = fixture(&store, ManagerPolicyV2::default());
        let tx = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate).unwrap();
        for n in 0..history {
            mirror(&store, lead.id, Uuid::from_u128(n + 1), false);
        }
        tx.commit().unwrap();
        let mut stmt = store.conn.prepare(LEGACY_APPROVAL_CANDIDATES_SQL).unwrap();
        let ids = stmt
            .query_map(params![lead.id.to_string(), ""], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(ids.is_empty());
        assert!(stmt.get_status(StatementStatus::VmStep) < 100);
        assert_eq!(stmt.get_status(StatementStatus::FullscanStep), 0);
        assert_eq!(stmt.get_status(StatementStatus::Sort), 0);
        drop(stmt);
        let tx = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate).unwrap();
        for n in 0..history {
            let id = Uuid::from_u128(100000 + n);
            mirror(&store, lead.id, id, true);
            native(&store, lead.id, id);
        }
        let legacy = Uuid::from_u128(200000);
        mirror(&store, lead.id, legacy, true);
        tx.commit().unwrap();
        let mut stmt = store.conn.prepare(LEGACY_APPROVAL_CANDIDATES_SQL).unwrap();
        let ids = stmt
            .query_map(params![lead.id.to_string(), ""], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(ids.len(), 65);
        assert!(stmt.get_status(StatementStatus::VmStep) < 1000);
        assert_eq!(stmt.get_status(StatementStatus::FullscanStep), 0);
        assert_eq!(stmt.get_status(StatementStatus::Sort), 0);
        let mut lookup = store.conn.prepare(LEGACY_APPROVAL_ROW_SQL).unwrap();
        let visible = lookup
            .query_map([&ids[0]], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(visible.is_empty());
        assert!(lookup.get_status(StatementStatus::VmStep) < 100);
        assert_eq!(lookup.get_status(StatementStatus::FullscanStep), 0);
        let mut after = String::new();
        let mut visits = BTreeSet::new();
        let mut gates = Vec::new();
        let mut empty_pages = 0;
        loop {
            let (candidates, next) = store
                .manager_v2_legacy_approval_candidates(&[lead.id], &after)
                .unwrap();
            assert!(candidates.len() <= 64);
            let before = gates.len();
            for id in candidates {
                assert!(visits.insert(id.clone()));
                if let Some((session, title, _)) =
                    store.manager_v2_legacy_approval_row(&id).unwrap()
                {
                    assert_eq!(session, lead.id);
                    assert_eq!(title, format!("Exact gate {legacy}"));
                    gates.push(id);
                }
            }
            if gates.len() == before {
                empty_pages += 1;
            }
            let Some(next) = next else {
                break;
            };
            assert!(next > after);
            after = next;
        }
        assert!(empty_pages > 0);
        assert_eq!(visits.len(), history as usize + 1);
        assert_eq!(gates, [legacy.to_string()]);
    }
}

#[test]
fn manager_decision_history_pages_native_history_and_sparse_legacy_gates_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mirrors.db");
    let store = Store::open(&path).unwrap();
    let (config, lead) = fixture(&store, ManagerPolicyV2::default());
    let mut worker = lead.clone();
    worker.id = Uuid::new_v4();
    worker.title = Some("Second approval worker".into());
    store.insert_session(&worker).unwrap();
    let mut expected = BTreeSet::new();
    let tx = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate).unwrap();
    for n in 0..1030 {
        let session = if n % 2 == 0 { lead.id } else { worker.id };
        mirror(&store, session, Uuid::from_u128(10000 + n * 3), false);
        let id = Uuid::from_u128(10001 + n * 3);
        mirror(&store, session, id, true);
        let (target, stamp) = native(&store, session, id);
        let key = format!("approval:{id}");
        expected.insert(key.clone());
        store
            .manager_v2_record_changed(&config, "decision_target", &key, lead.parent_id, &target)
            .unwrap();
        store.manager_v2_record_changed(&config,"decision",&key,lead.parent_id,&json!({"key":key,"session_id":session,"question":format!("Retained exact native {id}"),"status":"resolved","provider_request":target,"target_digest":fingerprint(&target).unwrap(),"publication_stamp":stamp})).unwrap();
        if n % 79 == 0 {
            let id = Uuid::from_u128(10002 + n * 3);
            mirror(&store, session, id, true);
            expected.insert(format!("approval:{id}"));
        }
    }
    store
        .manager_v2_record_changed(
            &config,
            "decision",
            "next-step",
            lead.parent_id,
            &json!({"question":"Choose the next step","status":"pending"}),
        )
        .unwrap();
    expected.insert("next-step".into());
    tx.commit().unwrap();
    drop(store);
    let store = Store::open(&path).unwrap();
    let mut q = AgentManagerInspectRequestV2 {
        section: ManagerInspectSectionV2::Decisions,
        epic_id: lead.parent_id,
        limit: 7,
        ..Default::default()
    };
    let mut seen = BTreeSet::new();
    loop {
        let page = store
            .manager_v2_inspect_operator(config.project_id, &q)
            .unwrap();
        assert!(page.rows.len() <= 7);
        for row in &page.rows {
            let key = row["key"].as_str().unwrap();
            assert!(
                seen.insert(key.to_owned()),
                "exact identities appear once: {key}"
            );
            if row["type"] == "operator_gate" {
                assert_eq!(
                    row["question"],
                    format!(
                        "Pending tool approval: Exact gate {}",
                        row["approval_id"].as_str().unwrap()
                    )
                );
                assert_eq!(row["route_state"], "unavailable");
            } else if key == "next-step" {
                assert_eq!(row["question"], "Choose the next step");
            } else {
                assert_eq!(
                    row["question"],
                    format!(
                        "Retained exact native {}",
                        key.strip_prefix("approval:").unwrap()
                    )
                );
                assert_eq!(row["status"], "resolved");
            }
        }
        let Some(next) = page.next_cursor else {
            assert!(page.complete);
            break;
        };
        assert!(next.len() <= 512);
        assert_ne!(q.cursor.as_deref(), Some(next.as_str()));
        q.cursor = Some(next);
        assert!(seen.len() <= expected.len());
    }
    assert_eq!(seen, expected);
}
