//! D19: ledger record identity is the work, never the manager seat
//! (Issues bf09774c / 559be8f1 / 8e05cffa).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    clippy::type_complexity
)]
use super::*;

fn fenced(
    change: ManagerUpdateV2,
    key: &str,
    config: &HarnessManagerConfigV1,
    policy: i64,
) -> AgentManagerUpdateRequestV2 {
    AgentManagerUpdateRequestV2 {
        fence: ManagerFenceV2 {
            scope_version: config.row_version,
            policy_version: policy,
        },
        idempotency_key: key.into(),
        change,
    }
}

fn product(f: &Fixture, key: &str, expected: i64) -> ManagerUpdateV2 {
    ManagerUpdateV2::Work {
        key: key.into(),
        expected_row_version: expected,
        epic_id: f.epic,
        title: format!("{key} feature"),
        kind: ManagerWorkKindV2::Product,
        priority: 1,
        weight: 2,
        required_gates: vec![
            ManagerWorkStageV2::Implementation,
            ManagerWorkStageV2::Review,
            ManagerWorkStageV2::Verification,
        ],
    }
}

fn config(f: &Fixture) -> HarnessManagerConfigV1 {
    f.store.get_harness_manager(f.project).unwrap().unwrap()
}

fn policy_version(f: &Fixture) -> i64 {
    f.store
        .get_harness_manager_policy(f.project)
        .unwrap()
        .unwrap()
        .row_version
}

/// Re-grant the current policy under the current scope (a policy refresh).
fn regrant(f: &Fixture, key: &str) -> i64 {
    let current = f
        .store
        .get_harness_manager_policy(f.project)
        .unwrap()
        .unwrap();
    f.store
        .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
            project_id: f.project,
            expected_scope_version: config(f).row_version,
            expected_policy_version: current.row_version,
            idempotency_key: key.into(),
            policy: current.policy,
        })
        .unwrap()
        .row_version
}

/// Save the scope with `seat` (the same or a new manager) over `epics`.
fn save_scope(f: &Fixture, seat: Uuid, epics: Vec<Uuid>) -> HarnessManagerConfigV1 {
    f.store
        .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
            group_ids: Vec::new(),
            project_id: f.project,
            session_id: seat,
            epic_ids: Some(epics),
            expected_row_version: config(f).row_version,
        })
        .unwrap()
}

fn new_seat(f: &Fixture) -> Uuid {
    let mut seat = f.store.get_session(f.manager).unwrap().unwrap();
    seat.id = Uuid::new_v4();
    f.store.insert_session(&seat).unwrap();
    seat.id
}

type Snapshot = Vec<(String, String, i64, String)>;

fn snapshot(f: &Fixture, config: &HarnessManagerConfigV1) -> Snapshot {
    let mut rows = Vec::new();
    for kind in ["work", "dependency", "ownership", "migration"] {
        for record in f.store.manager_v2_records(config, kind).unwrap() {
            rows.push((
                kind.to_string(),
                record.key,
                record.row_version,
                serde_json::to_string(&record.payload).unwrap(),
            ));
        }
    }
    rows
}

/// Two works, an enabled dependency edge and an exclusive ownership claim.
fn plan(f: &Fixture) -> Snapshot {
    let c = config(f);
    let p = policy_version(f);
    for key in ["alpha", "beta"] {
        f.store
            .manager_v2_commit_update(
                f.manager,
                &fenced(product(f, key, 0), key, &c, p),
                &LedgerObservation::default(),
            )
            .unwrap();
    }
    for (i, change) in [
        ManagerUpdateV2::Dependency {
            key: "alpha".into(),
            expected_row_version: 0,
            prerequisite: "beta".into(),
            require_integrated: false,
            enabled: true,
        },
        ManagerUpdateV2::Ownership {
            key: "alpha".into(),
            expected_row_version: 0,
            domain: "crates/rsid/src/store/mod.rs".into(),
            mode: ManagerOwnershipModeV2::Exclusive,
            files: vec!["crates/rsid/src/store/mod.rs".into()],
            active: true,
        },
    ]
    .into_iter()
    .enumerate()
    {
        f.store
            .manager_v2_commit_update(
                f.manager,
                &fenced(change, &format!("plan-{i}"), &c, p),
                &LedgerObservation::default(),
            )
            .unwrap();
    }
    let before = snapshot(f, &c);
    assert_eq!(before.len(), 4, "two works, one edge and one claim");
    before
}

/// Simulate delivered work: accepted and integrated at an exact source.
fn land(f: &Fixture, key: &str) -> WorkRecord {
    let c = config(f);
    let (row, mut work) = f.store.manager_v2_work(&c, key).unwrap();
    let source = "a".repeat(40);
    work.source_commit = Some(source.clone());
    work.acceptance = Some(Acceptance {
        source_commit: source.clone(),
        spec_revision: work.spec_revision,
        evidence_digest: format!("sha256:{}", "b".repeat(64)),
        method: "independent_evidence".into(),
        accepted_at: now(),
    });
    work.integration = Some(Integration {
        source_commit: source,
        target_commit: "c".repeat(40),
        verification: None,
        integrated_at: now(),
    });
    f.store
        .manager_v2_put_record(
            &c,
            "work",
            key,
            Some(work.epic_id),
            row.row_version,
            &serde_json::to_value(&work).unwrap(),
        )
        .unwrap();
    work
}

#[test]
fn policy_refresh_and_scope_save_keep_every_live_work_fact() {
    let f = fixture();
    let before = plan(&f);

    let refreshed_policy = regrant(&f, "refresh");
    assert_eq!(snapshot(&f, &config(&f)), before);

    // A genuine scope change (identical re-saves are no-ops since #450).
    let mut widened = f.store.get_session(f.epic).unwrap().unwrap();
    widened.id = Uuid::new_v4();
    widened.lead_session_id = None;
    f.store.insert_session(&widened).unwrap();
    let saved = save_scope(&f, f.manager, vec![f.epic, widened.id]);
    assert_eq!(saved.row_version, 2);
    let saved_policy = regrant(&f, "after-save");
    assert!(saved_policy > refreshed_policy);
    assert_eq!(snapshot(&f, &config(&f)), before);

    // The carried facts stay live: gates still see the edge and new fences write.
    assert_eq!(
        f.store
            .manager_v2_dependency_blockers(&config(&f), "alpha")
            .unwrap(),
        vec!["beta".to_string()]
    );
    let receipt = f
        .store
        .manager_v2_commit_update(
            f.manager,
            &fenced(product(&f, "alpha", 1), "revise", &config(&f), saved_policy),
            &LedgerObservation::default(),
        )
        .unwrap();
    assert_eq!(receipt.row_version, 2);
}

#[test]
fn seat_move_returns_records_and_accepted_source_with_identical_digests() {
    let f = fixture();
    let before = plan(&f);
    let landed = land(&f, "beta");
    let seat_a = config(&f);
    let digest_a = policy_digest(&landed, landed.source_commit.as_deref().unwrap()).unwrap();

    let seat_b = new_seat(&f);
    let config_b = save_scope(&f, seat_b, vec![f.epic]);
    assert_eq!(config_b.manager_session_id, seat_b);
    assert_eq!(config_b.row_version, seat_a.row_version + 1);
    let policy_b = regrant(&f, "seat-b");

    let (row, work) = f.store.manager_v2_work(&config_b, "beta").unwrap();
    assert_eq!(row.row_version, 2);
    assert_eq!(
        serde_json::to_string(&work).unwrap(),
        serde_json::to_string(&landed).unwrap()
    );
    let accepted = f
        .store
        .manager_v2_accepted_source(&config_b, &work, &"a".repeat(40))
        .unwrap()
        .expect("acceptance recorded under seat A resolves under seat B");
    assert_eq!(
        accepted.evidence_digest,
        format!("sha256:{}", "b".repeat(64))
    );
    assert_eq!(
        policy_digest(&work, &"a".repeat(40)).unwrap(),
        digest_a,
        "the policy digest never binds the seat"
    );
    // Live facts carry over unchanged; delivered beta is history, read by key above.
    let after = snapshot(&f, &config_b);
    assert_eq!(
        after,
        before
            .into_iter()
            .filter(|r| !(r.0 == "work" && r.1 == "beta"))
            .collect::<Vec<_>>()
    );
    let rows = f
        .store
        .manager_v2_inspect(
            seat_b,
            &AgentManagerInspectRequestV2 {
                section: ManagerInspectSectionV2::Work,
                ..Default::default()
            },
        )
        .unwrap()
        .rows;
    let beta = rows.iter().find(|r| r["key"] == "beta").unwrap();
    assert_eq!(beta["source_accepted"], true);
    assert_eq!(beta["integrated"], true);

    // Seat B writes under its own fence; provenance names B, identity is unchanged.
    f.store
        .manager_v2_commit_update(
            seat_b,
            &fenced(
                ManagerUpdateV2::Ownership {
                    key: "beta".into(),
                    expected_row_version: 0,
                    domain: "docs/keybindings.md".into(),
                    mode: ManagerOwnershipModeV2::Shared,
                    files: vec![],
                    active: true,
                },
                "seat-b-claim",
                &config_b,
                policy_b,
            ),
            &LedgerObservation::default(),
        )
        .unwrap();
    let provenance: (String, i64, i64) = f
        .store
        .conn
        .query_row(
            "SELECT manager_session_id,scope_version,policy_version
               FROM harness_manager_v2_work_facts
              WHERE project_id=?1 AND kind='ownership' AND work_key='beta'",
            [f.project.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        provenance,
        (seat_b.to_string(), config_b.row_version, policy_b)
    );
}

#[test]
fn landed_work_is_not_readmitted_by_a_successor_seat() {
    let f = fixture();
    plan(&f);
    let landed = land(&f, "beta");
    let seat_b = new_seat(&f);
    let config_b = save_scope(&f, seat_b, vec![f.epic]);
    let policy_b = regrant(&f, "seat-b");

    let error = f
        .store
        .manager_v2_commit_update(
            seat_b,
            &fenced(product(&f, "beta", 0), "readmit", &config_b, policy_b),
            &LedgerObservation::default(),
        )
        .unwrap_err();
    assert!(
        error.to_string().contains("manager_v2_record_changed"),
        "{error}"
    );
    let (row, work) = f.store.manager_v2_work(&config_b, "beta").unwrap();
    assert_eq!(row.row_version, 2);
    assert_eq!(
        work.integration.as_ref().map(|i| i.target_commit.clone()),
        landed.integration.as_ref().map(|i| i.target_commit.clone())
    );
    assert!(work.acceptance.is_some());
}

#[test]
fn stale_revoked_or_out_of_scope_seats_are_refused_exactly() {
    let f = fixture();
    plan(&f);
    let seat_a = config(&f);
    let policy_a = policy_version(&f);

    // A second Epic that the successor's scope covers instead of the original.
    let mut other = f.store.get_session(f.epic).unwrap().unwrap();
    other.id = Uuid::new_v4();
    other.lead_session_id = None;
    f.store.insert_session(&other).unwrap();
    let seat_b = new_seat(&f);
    let config_b = save_scope(&f, seat_b, vec![other.id]);

    // Before any re-grant the new scope has no live policy: writes are revoked.
    let alpha = f.store.manager_v2_work(&seat_a, "alpha").unwrap();
    let error = f
        .store
        .manager_v2_put_record(
            &config_b,
            "work",
            "alpha",
            Some(f.epic),
            alpha.0.row_version,
            &alpha.0.payload,
        )
        .unwrap_err();
    assert!(
        error.to_string().contains("manager_v2_epic_out_of_scope"),
        "{error}"
    );
    let fresh = serde_json::to_value(&alpha.1).unwrap();
    let error = f
        .store
        .manager_v2_put_record(&config_b, "work", "gamma", Some(other.id), 0, &{
            let mut v = fresh.clone();
            v["key"] = json!("gamma");
            v["epic_id"] = json!(other.id);
            v
        })
        .unwrap_err();
    assert!(
        error.to_string().contains("manager_v2_policy_changed"),
        "{error}"
    );

    // The predecessor's stale config can no longer write durable facts.
    let error = f
        .store
        .manager_v2_put_record(
            &seat_a,
            "work",
            "alpha",
            Some(f.epic),
            alpha.0.row_version,
            &fresh,
        )
        .unwrap_err();
    assert!(
        error.to_string().contains("manager_v2_scope_changed"),
        "{error}"
    );
    let error = f
        .store
        .manager_v2_commit_update(
            f.manager,
            &fenced(product(&f, "alpha", 1), "stale-seat", &seat_a, policy_a),
            &LedgerObservation::default(),
        )
        .unwrap_err();
    assert!(
        error.to_string().contains("manager_scope_denied"),
        "{error}"
    );

    // Granted but out of scope: the original Epic's work cannot be read or taken.
    let policy_b = regrant(&f, "seat-b");
    let error = f
        .store
        .manager_v2_commit_update(
            seat_b,
            &fenced(
                ManagerUpdateV2::Stage {
                    key: "alpha".into(),
                    expected_row_version: 1,
                    stage: ManagerWorkStageV2::Implementation,
                    state: ManagerStageStateV2::Partial,
                    note: "not mine".into(),
                    evidence: None,
                },
                "foreign-stage",
                &config_b,
                policy_b,
            ),
            &LedgerObservation::default(),
        )
        .unwrap_err();
    assert!(
        error.to_string().contains("manager_v2_epic_out_of_scope"),
        "{error}"
    );
    let mut hijack = product(&f, "alpha", 0);
    if let ManagerUpdateV2::Work { epic_id, .. } = &mut hijack {
        *epic_id = other.id;
    }
    let error = f
        .store
        .manager_v2_commit_update(
            seat_b,
            &fenced(hijack, "hijack", &config_b, policy_b),
            &LedgerObservation::default(),
        )
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("manager_v2_work_identity_changed"),
        "{error}"
    );
    // The successor's scoped plan is exactly its own Epic's work, while the
    // project-wide gate still sees the original exclusive claim.
    let mut gamma = product(&f, "gamma", 0);
    if let ManagerUpdateV2::Work { epic_id, .. } = &mut gamma {
        *epic_id = other.id;
    }
    f.store
        .manager_v2_commit_update(
            seat_b,
            &fenced(gamma, "gamma", &config_b, policy_b),
            &LedgerObservation::default(),
        )
        .unwrap();
    assert_eq!(
        f.store
            .manager_v2_records(&config_b, "work")
            .unwrap()
            .iter()
            .map(|r| (r.key.as_str(), r.epic_id))
            .collect::<Vec<_>>(),
        vec![("gamma", Some(other.id))]
    );
    let claims = f
        .store
        .manager_v2_facts(&config_b, "ownership", FactReach::Project)
        .unwrap();
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0].payload["work_key"], "alpha");
}

#[test]
fn fresh_store_opens_at_head_with_the_work_facts_catalog() {
    let store = Store::open_in_memory().unwrap();
    assert_eq!(
        store
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get::<_, i32>(0))
            .unwrap(),
        crate::store::LATEST_SCHEMA_VERSION
    );
    for (kind, name) in super::super::V122_CATALOG_OBJECTS {
        let found: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type=?1 AND name=?2",
                [kind, name],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(found, 1, "{kind} {name}");
    }
}

/// Opens a real V121 database holding multi-seat history and upgrades it
/// through the production `if version < 122` block.
#[test]
fn v121_database_upgrade_carries_forward_the_newest_row_per_identity() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("manager-ledger-v121.sqlite");
    let (project, manager, seat_b, source_rows) = {
        let f = fixture_using(Store::open(&database).unwrap());
        let seat_b = new_seat(&f);
        let conn = &f.store.conn;
        crate::store::tests::rewind_post_v121_tail_to(conn, 121);
        assert_eq!(
            conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i32>(0))
                .unwrap(),
            121
        );
        let insert = |seat: Uuid,
                      scope: i64,
                      kind: &str,
                      key: &str,
                      version: i64,
                      payload: Value,
                      at: &str| {
            conn.execute(
            "INSERT INTO harness_manager_v2_records(project_id,manager_session_id,scope_version,kind,
                 record_key,epic_id,row_version,payload_json,created_at,updated_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?9)",
            params![
                f.project.to_string(),
                seat.to_string(),
                scope,
                kind,
                key,
                f.epic.to_string(),
                version,
                payload.to_string(),
                at
            ],
        )
        .unwrap();
        };
        let t1 = "2026-09-20T00:00:00.000000000Z";
        let t2 = "2026-09-21T00:00:00.000000000Z";
        // Seat A scope 21 wrote alpha at v5; seat B scope 23 rebuilt it at v2.
        insert(
            f.manager,
            21,
            "work",
            "alpha",
            5,
            json!({"key":"alpha","rev":"old"}),
            t2,
        );
        insert(
            seat_b,
            23,
            "work",
            "alpha",
            2,
            json!({"key":"alpha","rev":"new"}),
            t1,
        );
        // Only seat A ever knew beta and its claim.
        insert(f.manager, 22, "work", "beta", 3, json!({"key":"beta"}), t1);
        insert(
            f.manager,
            22,
            "ownership",
            "claim",
            1,
            json!({"work_key":"beta"}),
            t1,
        );
        // Same scope twice: the later update wins deterministically.
        insert(
            f.manager,
            22,
            "dependency",
            "edge",
            1,
            json!({"work_key":"alpha","v":1}),
            t1,
        );
        insert(
            seat_b,
            22,
            "dependency",
            "edge",
            1,
            json!({"work_key":"alpha","v":2}),
            t2,
        );
        // In-flight authority is not carried.
        insert(f.manager, 22, "request", "r", 1, json!({}), t1);
        let source_rows: i64 = conn
            .query_row("SELECT count(*) FROM harness_manager_v2_records", [], |r| {
                r.get(0)
            })
            .unwrap();
        (f.project, f.manager, seat_b, source_rows)
    };

    let store = Store::open(&database).expect("V121 database upgrades through V122");
    let conn = &store.conn;

    let rows: Vec<(
        String,
        String,
        String,
        i64,
        String,
        String,
        i64,
        Option<i64>,
    )> = conn
        .prepare(
            "SELECT kind,record_key,work_key,row_version,payload_json,manager_session_id,
                    scope_version,policy_version
               FROM harness_manager_v2_work_facts WHERE project_id=?1 ORDER BY kind,record_key",
        )
        .unwrap()
        .query_map([project.to_string()], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let a = manager.to_string();
    let b = seat_b.to_string();
    assert_eq!(
        rows,
        vec![
            (
                "dependency".into(),
                "edge".into(),
                "alpha".into(),
                1,
                json!({"work_key":"alpha","v":2}).to_string(),
                b.clone(),
                22,
                None
            ),
            (
                "ownership".into(),
                "claim".into(),
                "beta".into(),
                1,
                json!({"work_key":"beta"}).to_string(),
                a.clone(),
                22,
                None
            ),
            (
                "work".into(),
                "alpha".into(),
                "alpha".into(),
                2,
                json!({"key":"alpha","rev":"new"}).to_string(),
                b,
                23,
                None
            ),
            (
                "work".into(),
                "beta".into(),
                "beta".into(),
                3,
                json!({"key":"beta"}).to_string(),
                a,
                22,
                None
            ),
        ]
    );
    let after_rows: i64 = conn
        .query_row("SELECT count(*) FROM harness_manager_v2_records", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(after_rows, source_rows, "historical rows are retained");
    assert_eq!(
        conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i32>(0))
            .unwrap(),
        crate::store::LATEST_SCHEMA_VERSION
    );
}

/// P-001: project-wide gates never judge a truncated fact set. Over budget they
/// fail closed with the typed budget refusal; `record_key` order is total.
#[test]
fn project_gates_fail_closed_rather_than_read_a_truncated_fact_set() {
    let f = fixture();
    plan(&f);
    let stamp = now();
    // Carried rows can exceed the write budget; each gate's kind is over it.
    for kind in ["work", "dependency", "ownership"] {
        for ordinal in 0..MANAGER_V2_MAX_RECORDS {
            f.store
                .conn
                .execute(
                    "INSERT INTO harness_manager_v2_work_facts(project_id,kind,record_key,epic_id,
                         work_key,row_version,payload_json,manager_session_id,scope_version,
                         created_at,updated_at)
                     VALUES(?1,?2,?3,?4,'beta',1,'{}',?5,1,?6,?6)",
                    params![
                        f.project.to_string(),
                        kind,
                        format!("carried-{ordinal:05}"),
                        f.epic.to_string(),
                        f.manager.to_string(),
                        stamp
                    ],
                )
                .unwrap();
        }
    }
    let c = config(&f);
    // More than the budget of LIVE facts: a brand-new work is refused too.
    let error = f
        .store
        .manager_v2_commit_update(
            f.manager,
            &fenced(
                product(&f, "gamma", 0),
                "over-budget-work",
                &c,
                policy_version(&f),
            ),
            &LedgerObservation::default(),
        )
        .unwrap_err();
    assert!(
        error.to_string().contains("manager_v2_record_budget"),
        "{error}"
    );
    for gate in [
        f.store
            .manager_v2_facts(&c, "ownership", FactReach::Project)
            .map(|_| ()),
        f.store
            .manager_v2_dependency_blockers(&c, "alpha")
            .map(|_| ()),
        f.store
            .manager_v2_commit_update(
                f.manager,
                &fenced(
                    ManagerUpdateV2::Ownership {
                        key: "alpha".into(),
                        expected_row_version: 0,
                        domain: "docs/keybindings.md".into(),
                        mode: ManagerOwnershipModeV2::Exclusive,
                        files: vec![],
                        active: true,
                    },
                    "over-budget-claim",
                    &c,
                    policy_version(&f),
                ),
                &LedgerObservation::default(),
            )
            .map(|_| ()),
        f.store
            .manager_v2_commit_update(
                f.manager,
                &fenced(
                    ManagerUpdateV2::Dependency {
                        key: "beta".into(),
                        expected_row_version: 0,
                        prerequisite: "alpha".into(),
                        require_integrated: false,
                        enabled: true,
                    },
                    "over-budget-edge",
                    &c,
                    policy_version(&f),
                ),
                &LedgerObservation::default(),
            )
            .map(|_| ()),
    ] {
        let error = gate.unwrap_err();
        assert!(
            error.to_string().contains("manager_v2_record_budget"),
            "{error}"
        );
    }
}

/// Builds a V121 database file with the fixture's project, seats and Epic.
fn v121_database(directory: &tempfile::TempDir, name: &str) -> (std::path::PathBuf, Fixture) {
    let database = directory.path().join(name);
    let f = fixture_using(Store::open(&database).unwrap());
    crate::store::tests::rewind_post_v121_tail_to(&f.store.conn, 121);
    (database, f)
}

fn insert_v121_record(f: &Fixture, epic: Option<Uuid>, kind: &str, key: &str, payload: &Value) {
    f.store
        .conn
        .execute(
            "INSERT INTO harness_manager_v2_records(project_id,manager_session_id,scope_version,
                 kind,record_key,epic_id,row_version,payload_json,created_at,updated_at)
             VALUES(?1,?2,1,?3,?4,?5,1,?6,?7,?7)",
            params![
                f.project.to_string(),
                f.manager.to_string(),
                kind,
                key,
                epic.map(|e| e.to_string()),
                payload.to_string(),
                "2026-09-21T00:00:00.000000000Z"
            ],
        )
        .unwrap();
}

/// P-006 / P-010: a defective newest row fails the upgrade with a typed error
/// and leaves the database at V121; nothing is dropped silently.
#[test]
fn v121_upgrade_refuses_defective_carry_forward_instead_of_dropping_rows() {
    let directory = tempfile::tempdir().unwrap();
    let foreign_epic = Uuid::new_v4();
    let cases: [(&str, &str, &str, Value, bool); 6] = [
        (
            "work_key_missing",
            "ownership",
            "claim",
            json!({"domain":"d"}),
            true,
        ),
        ("key_mismatch", "work", "alpha", json!({"key":"beta"}), true),
        (
            "epic_mismatch",
            "work",
            "alpha",
            json!({"key":"alpha","epic_id":foreign_epic}),
            true,
        ),
        (
            "epic_missing",
            "work",
            "alpha",
            json!({"key":"alpha"}),
            false,
        ),
        (
            "work_key_invalid",
            "ownership",
            "claim",
            json!({"work_key":"","domain":"d"}),
            true,
        ),
        (
            "work_key_invalid",
            "dependency",
            "edge",
            json!({"work_key":"w".repeat(257),"prerequisite":"alpha"}),
            true,
        ),
    ];
    for (case, (defect, kind, key, payload, with_epic)) in cases.into_iter().enumerate() {
        let (database, f) = v121_database(&directory, &format!("{case}-{defect}.sqlite"));
        insert_v121_record(&f, with_epic.then_some(f.epic), kind, key, &payload);
        drop(f);
        let error = Store::open(&database)
            .err()
            .expect("defective carry-forward refuses");
        let message = error.to_string();
        assert!(
            message.contains("manager_v2_fact_carry_forward_invalid") && message.contains(defect),
            "{defect}: {message}"
        );
        let raw = rusqlite::Connection::open(&database).unwrap();
        assert_eq!(
            raw.query_row("PRAGMA user_version", [], |r| r.get::<_, i32>(0))
                .unwrap(),
            121,
            "{defect}: the failed upgrade rolls back to exact V121"
        );
        let retained: i64 = raw
            .query_row(
                "SELECT count(*) FROM harness_manager_v2_records WHERE kind=?1 AND record_key=?2",
                [kind, key],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(retained, 1, "{defect}: the source row is retained");
    }
    // A defective row that is not the newest for its identity is history only.
    let (database, f) = v121_database(&directory, "older-defect.sqlite");
    insert_v121_record(
        &f,
        Some(f.epic),
        "ownership",
        "claim",
        &json!({"domain":"d"}),
    );
    f.store
        .conn
        .execute(
            "INSERT INTO harness_manager_v2_records(project_id,manager_session_id,scope_version,
                 kind,record_key,epic_id,row_version,payload_json,created_at,updated_at)
             VALUES(?1,?2,2,'ownership','claim',?3,1,?4,?5,?5)",
            params![
                f.project.to_string(),
                f.manager.to_string(),
                f.epic.to_string(),
                json!({"work_key":"alpha","domain":"d"}).to_string(),
                "2026-09-22T00:00:00.000000000Z"
            ],
        )
        .unwrap();
    drop(f);
    let store = Store::open(&database).unwrap();
    let carried: (String, i64) = store
        .conn
        .query_row(
            "SELECT work_key,scope_version FROM harness_manager_v2_work_facts
              WHERE kind='ownership' AND record_key='claim'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(carried, ("alpha".into(), 2));
}

/// P-009: NULL policy provenance requires a project that never had a grant;
/// a missing policy projection after a grant is refused.
#[test]
fn null_policy_provenance_requires_a_project_that_was_never_granted() {
    let f = fixture();
    plan(&f);
    let c = config(&f);
    f.store
        .conn
        .execute(
            "DELETE FROM harness_manager_v2_policies WHERE project_id=?1",
            [f.project.to_string()],
        )
        .unwrap();
    assert!(
        f.store
            .get_harness_manager_policy(f.project)
            .unwrap()
            .is_none()
    );
    let (row, work) = f.store.manager_v2_work(&c, "alpha").unwrap();
    let error = f
        .store
        .manager_v2_put_record(
            &c,
            "work",
            "alpha",
            Some(f.epic),
            row.row_version,
            &serde_json::to_value(&work).unwrap(),
        )
        .unwrap_err();
    assert!(
        error.to_string().contains("manager_v2_policy_changed"),
        "{error}"
    );

    // A project that never had a V2 grant records unknown policy provenance.
    let store = Store::open_in_memory().unwrap();
    let project = Uuid::new_v4();
    store
        .insert_project(&Project {
            id: project,
            name: "Ungranted".into(),
            path: None,
            description: None,
            color: Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        })
        .unwrap();
    let mut manager = test_session(Uuid::new_v4(), PathBuf::from("/var/tmp/ham-v2-fd23a414"));
    manager.session_kind = SessionKind::Standard;
    manager.project_id = Some(project);
    manager.status = SessionStatus::Completed;
    store.insert_session(&manager).unwrap();
    let mut group = manager.clone();
    group.id = Uuid::new_v4();
    group.session_kind = SessionKind::Group;
    store.insert_session(&group).unwrap();
    let mut epic = manager.clone();
    epic.id = Uuid::new_v4();
    epic.session_kind = SessionKind::Epic;
    epic.parent_id = Some(group.id);
    store.insert_session(&epic).unwrap();
    let ungranted = store
        .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
            group_ids: Vec::new(),
            project_id: project,
            session_id: manager.id,
            epic_ids: Some(vec![epic.id]),
            expected_row_version: 0,
        })
        .unwrap();
    store
        .manager_v2_put_record(
            &ungranted,
            "work",
            "solo",
            Some(epic.id),
            0,
            &json!({"key":"solo","epic_id":epic.id}),
        )
        .unwrap();
    let provenance: Option<i64> = store
        .conn
        .query_row(
            "SELECT policy_version FROM harness_manager_v2_work_facts WHERE project_id=?1",
            [project.to_string()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(provenance, None);
}

/// P-007: a successor pages every carried fact into lead notices, including a
/// lower key written after its cursor has already advanced.
#[test]
fn successor_notice_paging_visits_carried_and_later_lower_key_facts() {
    let f = fixture();
    plan(&f);
    let seat_b = new_seat(&f);
    let config_b = save_scope(&f, seat_b, vec![f.epic]);
    let policy_b = regrant(&f, "seat-b");
    let subjects = |store: &Store| -> Vec<String> {
        let mut statement = store
            .conn
            .prepare(
                "SELECT subject_id FROM harness_manager_notices
                  WHERE manager_session_id=?1 AND scope_version=?2 AND kind='ledger_change'
                  ORDER BY subject_id",
            )
            .unwrap();
        statement
            .query_map(params![seat_b.to_string(), config_b.row_version], |r| {
                r.get::<_, String>(0)
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    f.store.manager_v2_reconcile_notices(&config_b).unwrap();
    assert_eq!(subjects(&f.store), vec!["work:alpha", "work:beta"]);

    f.store
        .manager_v2_commit_update(
            seat_b,
            &fenced(product(&f, "aaa", 0), "lower-key", &config_b, policy_b),
            &LedgerObservation::default(),
        )
        .unwrap();
    f.store.manager_v2_reconcile_notices(&config_b).unwrap();
    f.store.manager_v2_reconcile_notices(&config_b).unwrap();
    assert_eq!(
        subjects(&f.store),
        vec!["work:aaa", "work:alpha", "work:beta"]
    );
}

/// Delivered history beyond the budget: integrated works (and their claims and
/// edges) are terminal, so writes, gates and the work view stay live, while
/// every landed work stays readable by key and is never re-admitted.
#[test]
fn delivered_history_beyond_the_budget_leaves_writes_and_gates_live() {
    let f = fixture();
    plan(&f);
    let c = config(&f);
    let p = policy_version(&f);
    let (_, template) = f.store.manager_v2_work(&c, "beta").unwrap();
    let source = "a".repeat(40);
    let stamp = now();
    let history = MANAGER_V2_MAX_RECORDS + 100;
    for ordinal in 0..history {
        let key = format!("done-{ordinal:05}");
        let mut work = template.clone();
        work.key.clone_from(&key);
        work.source_commit = Some(source.clone());
        work.acceptance = Some(Acceptance {
            source_commit: source.clone(),
            spec_revision: work.spec_revision,
            evidence_digest: format!("sha256:{}", "b".repeat(64)),
            method: "independent_evidence".into(),
            accepted_at: stamp.clone(),
        });
        work.integration = Some(Integration {
            source_commit: source.clone(),
            target_commit: "c".repeat(40),
            verification: None,
            integrated_at: stamp.clone(),
        });
        let facts = [
            ("work", key.clone(), serde_json::to_value(&work).unwrap()),
            (
                "ownership",
                format!("{key}:claim"),
                json!({"work_key":key,"domain":format!("d{ordinal}"),
                       "mode":"exclusive","files":[],"active":true}),
            ),
            (
                "dependency",
                format!("{key}:edge"),
                json!({"work_key":key,"prerequisite":"alpha",
                       "require_integrated":true,"enabled":true}),
            ),
        ];
        for (kind, record_key, payload) in facts {
            f.store
                .conn
                .execute(
                    "INSERT INTO harness_manager_v2_work_facts(project_id,kind,record_key,
                         epic_id,work_key,row_version,payload_json,manager_session_id,
                         scope_version,created_at,updated_at)
                     VALUES(?1,?2,?3,?4,?5,2,?6,?7,1,?8,?8)",
                    params![
                        f.project.to_string(),
                        kind,
                        record_key,
                        f.epic.to_string(),
                        key,
                        payload.to_string(),
                        f.manager.to_string(),
                        stamp
                    ],
                )
                .unwrap();
        }
    }

    // Writes and every project-wide gate still succeed.
    f.store
        .manager_v2_commit_update(
            f.manager,
            &fenced(product(&f, "gamma", 0), "gamma", &c, p),
            &LedgerObservation::default(),
        )
        .unwrap();
    for (i, change) in [
        // A delivered work's exclusive domain is released.
        ManagerUpdateV2::Ownership {
            key: "gamma".into(),
            expected_row_version: 0,
            domain: "d0".into(),
            mode: ManagerOwnershipModeV2::Exclusive,
            files: vec![],
            active: true,
        },
        // A delivered prerequisite satisfies the edge; its old edges cause no cycle.
        ManagerUpdateV2::Dependency {
            key: "gamma".into(),
            expected_row_version: 0,
            prerequisite: "done-00000".into(),
            require_integrated: true,
            enabled: true,
        },
    ]
    .into_iter()
    .enumerate()
    {
        f.store
            .manager_v2_commit_update(
                f.manager,
                &fenced(change, &format!("gamma-{i}"), &c, p),
                &LedgerObservation::default(),
            )
            .unwrap();
    }
    assert_eq!(
        f.store.manager_v2_dependency_blockers(&c, "gamma").unwrap(),
        Vec::<String>::new()
    );
    assert_eq!(
        f.store.manager_v2_dependency_blockers(&c, "alpha").unwrap(),
        vec!["beta".to_string()]
    );
    // The live exclusive claim on an active domain still conflicts.
    let error = f
        .store
        .manager_v2_commit_update(
            f.manager,
            &fenced(
                ManagerUpdateV2::Ownership {
                    key: "gamma".into(),
                    expected_row_version: 0,
                    domain: "crates/rsid/src/store/mod.rs".into(),
                    mode: ManagerOwnershipModeV2::Exclusive,
                    files: vec![],
                    active: true,
                },
                "gamma-conflict",
                &c,
                p,
            ),
            &LedgerObservation::default(),
        )
        .unwrap_err();
    assert!(
        error.to_string().contains("manager_v2_domain_conflict"),
        "{error}"
    );

    // The work view shows every live work plus bounded delivered history.
    let rows = f.store.manager_v2_work_rows(&c, None).unwrap();
    assert_eq!(rows.len(), MANAGER_V2_MAX_WORK);
    for key in ["alpha", "beta", "gamma"] {
        assert!(rows.iter().any(|row| row["key"] == key), "{key}");
    }
    assert!(
        rows.iter()
            .any(|row| row["key"] == "done-00000" && row["integrated"] == true)
    );

    // Landed work stays readable by key, accepted, and is never re-admitted.
    let (row, landed) = f.store.manager_v2_work(&c, "done-01099").unwrap();
    assert_eq!(row.row_version, 2);
    assert!(landed.integration.is_some());
    assert!(
        f.store
            .manager_v2_accepted_source(&c, &landed, &source)
            .unwrap()
            .is_some()
    );
    let error = f
        .store
        .manager_v2_commit_update(
            f.manager,
            &fenced(product(&f, "done-01099", 0), "readmit", &c, p),
            &LedgerObservation::default(),
        )
        .unwrap_err();
    assert!(
        error.to_string().contains("manager_v2_record_changed"),
        "{error}"
    );
}

/// Review finding 3: a key reused by another Epic (or, for a claim, edge or
/// reservation, by another work) across seats is a collision, not history: the
/// upgrade fails typed and stays at V121 instead of dropping one of them.
#[test]
fn v121_upgrade_refuses_cross_epic_or_cross_work_key_collisions() {
    let directory = tempfile::tempdir().unwrap();
    for (defect, kind, rows) in [
        (
            "identity_conflict_epic",
            "work",
            [
                (1, false, json!({"key":"alpha"})),
                (2, true, json!({"key":"alpha"})),
            ],
        ),
        (
            "identity_conflict_work_key",
            "ownership",
            [
                (1, false, json!({"work_key":"alpha","domain":"d"})),
                (2, false, json!({"work_key":"beta","domain":"d"})),
            ],
        ),
    ] {
        let (database, f) = v121_database(&directory, &format!("{defect}.sqlite"));
        let mut other = f.store.get_session(f.epic).unwrap().unwrap();
        other.id = Uuid::new_v4();
        other.lead_session_id = None;
        f.store.insert_session(&other).unwrap();
        let key = if kind == "work" { "alpha" } else { "claim" };
        for (scope, in_other_epic, payload) in rows {
            f.store
                .conn
                .execute(
                    "INSERT INTO harness_manager_v2_records(project_id,manager_session_id,
                         scope_version,kind,record_key,epic_id,row_version,payload_json,
                         created_at,updated_at)
                     VALUES(?1,?2,?3,?4,?5,?6,1,?7,?8,?8)",
                    params![
                        f.project.to_string(),
                        f.manager.to_string(),
                        scope,
                        kind,
                        key,
                        if in_other_epic { other.id } else { f.epic }.to_string(),
                        payload.to_string(),
                        "2026-09-21T00:00:00.000000000Z"
                    ],
                )
                .unwrap();
        }
        drop(f);
        let message = Store::open(&database)
            .err()
            .expect("colliding identities refuse the upgrade")
            .to_string();
        assert!(
            message.contains("manager_v2_fact_carry_forward_invalid") && message.contains(defect),
            "{defect}: {message}"
        );
        let raw = rusqlite::Connection::open(&database).unwrap();
        assert_eq!(
            raw.query_row("PRAGMA user_version", [], |r| r.get::<_, i32>(0))
                .unwrap(),
            121
        );
        let retained: i64 = raw
            .query_row(
                "SELECT count(*) FROM harness_manager_v2_records WHERE kind=?1 AND record_key=?2",
                [kind, key],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(retained, 2, "{defect}: both colliding rows are retained");
    }
}

/// Review finding 1: delivered history with many claims per work keeps the
/// Work and Overview views within budget; the window is the recency prefix
/// whose works and associations fit, and live work is always shown.
#[test]
fn delivered_history_with_many_claims_keeps_work_and_overview_within_budget() {
    let f = fixture();
    plan(&f);
    let c = config(&f);
    let (_, template) = f.store.manager_v2_work(&c, "beta").unwrap();
    let source = "a".repeat(40);
    for ordinal in 0..MANAGER_V2_MAX_WORK {
        let key = format!("done-{ordinal:05}");
        let stamp = format!("2026-09-{:02}T00:00:00.{ordinal:09}Z", 1 + ordinal / 100);
        let mut work = template.clone();
        work.key.clone_from(&key);
        work.source_commit = Some(source.clone());
        work.integration = Some(Integration {
            source_commit: source.clone(),
            target_commit: "c".repeat(40),
            verification: None,
            integrated_at: stamp.clone(),
        });
        let mut facts = vec![("work", key.clone(), serde_json::to_value(&work).unwrap())];
        for claim in 0..5 {
            facts.push((
                "ownership",
                format!("{key}:claim-{claim}"),
                json!({"work_key":key,"domain":format!("{key}/d{claim}"),
                       "mode":"shared","files":[],"active":true}),
            ));
        }
        for (kind, record_key, payload) in facts {
            f.store
                .conn
                .execute(
                    "INSERT INTO harness_manager_v2_work_facts(project_id,kind,record_key,
                         epic_id,work_key,row_version,payload_json,manager_session_id,
                         scope_version,created_at,updated_at)
                     VALUES(?1,?2,?3,?4,?5,2,?6,?7,1,?8,?8)",
                    params![
                        f.project.to_string(),
                        kind,
                        record_key,
                        f.epic.to_string(),
                        key,
                        payload.to_string(),
                        f.manager.to_string(),
                        stamp
                    ],
                )
                .unwrap();
        }
    }
    let rows = f.store.manager_v2_work_rows(&c, None).unwrap();
    for key in ["alpha", "beta"] {
        assert!(rows.iter().any(|row| row["key"] == key), "{key}");
    }
    // 1024 / (1 work + 5 claims) = 170 delivered works, newest first.
    let delivered: Vec<_> = rows
        .iter()
        .filter(|row| row["key"].as_str().is_some_and(|k| k.starts_with("done-")))
        .collect();
    assert_eq!(delivered.len(), MANAGER_V2_MAX_RECORDS / 6);
    assert!(rows.iter().any(|row| row["key"] == "done-00255"));
    assert!(
        delivered
            .iter()
            .all(|row| row["ownership"].as_array().is_some_and(|c| c.len() == 5))
    );
    for section in [
        ManagerInspectSectionV2::Overview,
        ManagerInspectSectionV2::Work,
    ] {
        f.store
            .manager_v2_inspect(
                f.manager,
                &AgentManagerInspectRequestV2 {
                    section,
                    ..Default::default()
                },
            )
            .unwrap();
    }
}
