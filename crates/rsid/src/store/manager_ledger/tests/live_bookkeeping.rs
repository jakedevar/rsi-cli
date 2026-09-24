//! Issue #643: live bookkeeping sweeps and budget counts range over live rows
//! only, and the Inspector shows the limit enforcement uses.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use crate::store::harness_manager_v2::{
    BOOKKEEPING_LIMIT, COORDINATION_BUDGET_COUNT_SQL, LIVE_BOOKKEEPING_COUNTS_SQL,
    MANAGER_V2_LIVE_BOOKKEEPING_INDEX_VERSION, RETIRE_BOOKKEEPING_SQL, bookkeeping_class,
    live_class_count_sql,
};
use crate::store::manager_coordinator::LIVE_KIND_KEYS_SQL;
use rusqlite::{Connection, StatementStatus};

const LIVE_INDEX: &str = "harness_manager_v2_live_records";
const COORDINATION_INDEX: &str = "harness_manager_v2_coordination_budget";
const CLASS_KINDS: [&str; 7] = [
    "retrieval",
    "resource_spend",
    "lifecycle_context",
    "request_released",
    "request_settle",
    "request_rollover",
    "request_unsettled",
];

fn plan(conn: &Connection, sql: &str) -> String {
    let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
    let unbound = vec![rusqlite::types::Null; stmt.parameter_count()];
    stmt.query_map(rusqlite::params_from_iter(unbound), |r| {
        r.get::<_, String>(3)
    })
    .unwrap()
    .collect::<rusqlite::Result<Vec<_>>>()
    .unwrap()
    .join("\n")
}

fn insert(f: &Fixture, config: &HarnessManagerConfigV1, kind: &str, key: &str, archived: bool) {
    f.store
        .conn
        .execute(
            "INSERT INTO harness_manager_v2_records(project_id,manager_session_id,scope_version,
                 kind,record_key,row_version,payload_json,archived,created_at,updated_at)
             VALUES(?1,?2,?3,?4,?5,1,'{}',?6,?7,?7)",
            params![
                config.project_id.to_string(),
                config.manager_session_id.to_string(),
                config.row_version,
                kind,
                key,
                archived,
                now()
            ],
        )
        .unwrap();
}

/// VM steps one execution of `sql` costs for this scope (deterministic work).
fn steps(f: &Fixture, config: &HarnessManagerConfigV1, sql: &str, kind: Option<&str>) -> i32 {
    let mut stmt = f.store.conn.prepare(sql).unwrap();
    let (project, manager) = (
        config.project_id.to_string(),
        config.manager_session_id.to_string(),
    );
    if sql.starts_with("UPDATE") {
        stmt.execute(params![project, manager, config.row_version, now()])
            .unwrap();
    } else if let Some(kind) = kind {
        stmt.query_map(params![project, manager, config.row_version, kind], |_| {
            Ok(())
        })
        .unwrap()
        .for_each(drop);
    } else {
        stmt.query_map(params![project, manager, config.row_version], |_| Ok(()))
            .unwrap()
            .for_each(drop);
    }
    stmt.get_status(StatementStatus::VmStep)
}

fn record_budget(f: &Fixture) -> Value {
    let mut query = AgentManagerInspectRequestV2::default();
    loop {
        let page = f.store.manager_v2_inspect(f.manager, &query).unwrap();
        if let Some(row) = page.rows.iter().find(|r| r["type"] == "record_budget") {
            return row.clone();
        }
        query.cursor = page.next_cursor;
        assert!(
            query.cursor.is_some(),
            "overview carries a record_budget row"
        );
    }
}

fn true_live(f: &Fixture, config: &HarnessManagerConfigV1, kinds: &str) -> i64 {
    f.store
        .conn
        .query_row(
            &format!(
                "SELECT count(*) FROM harness_manager_v2_records NOT INDEXED
                 WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3
                   AND archived=0 AND kind IN ({kinds})"
            ),
            params![
                config.project_id.to_string(),
                config.manager_session_id.to_string(),
                config.row_version
            ],
            |r| r.get(0),
        )
        .unwrap()
}

#[test]
fn live_counts_sweep_and_kind_reads_use_the_live_scope_indexes() {
    let f = fixture();
    let conn = &f.store.conn;
    for kind in CLASS_KINDS {
        let (predicate, _) = bookkeeping_class(kind).unwrap();
        let detail = plan(conn, &live_class_count_sql(predicate));
        assert!(detail.contains(LIVE_INDEX), "{kind}: {detail}");
    }
    for sql in [
        LIVE_BOOKKEEPING_COUNTS_SQL,
        RETIRE_BOOKKEEPING_SQL,
        LIVE_KIND_KEYS_SQL,
    ] {
        let detail = plan(conn, sql);
        assert!(detail.contains(LIVE_INDEX), "{sql}: {detail}");
    }
    let detail = plan(conn, COORDINATION_BUDGET_COUNT_SQL);
    assert!(detail.contains(COORDINATION_INDEX), "{detail}");
}

#[test]
fn archived_history_leaves_live_counts_and_scan_work_unchanged() {
    let f = fixture();
    let config = f.store.get_harness_manager(f.project).unwrap().unwrap();
    // Live rows the sweep keeps: no operation, no admitted invocation, and
    // holds never retire. Spend rows are left out: the resource cohort reads
    // every spend identity by design (spend floor), which is not this bound.
    for n in 0..2 {
        let origin = Uuid::new_v4().to_string();
        insert(&f, &config, "resource_launch_origin", &origin, false);
        insert(
            &f,
            &config,
            "lifecycle_context",
            &format!("live-ctx-{n}"),
            false,
        );
    }
    for n in 0..4 {
        insert(
            &f,
            &config,
            "lifecycle_hold",
            &format!("live-hold-{n}"),
            false,
        );
    }
    for n in 0..5 {
        insert(&f, &config, "intent", &format!("live-intent-{n}"), false);
    }
    let measure = |f: &Fixture| {
        let mut out = Vec::new();
        for kind in CLASS_KINDS {
            let (predicate, _) = bookkeeping_class(kind).unwrap();
            out.push(steps(f, &config, &live_class_count_sql(predicate), None));
            out.push(steps(f, &config, LIVE_KIND_KEYS_SQL, Some(kind)));
        }
        out.push(steps(f, &config, LIVE_BOOKKEEPING_COUNTS_SQL, None));
        out.push(steps(f, &config, COORDINATION_BUDGET_COUNT_SQL, None));
        out.push(steps(f, &config, RETIRE_BOOKKEEPING_SQL, None));
        out
    };
    let before = measure(&f);

    let tx = f.store.conn.unchecked_transaction().unwrap();
    for (kind, rows) in [
        ("retrieval", 7_000),
        ("resource_launch_origin", 2_000),
        ("lifecycle_context", 1_000),
        ("lifecycle_hold", 500),
    ] {
        for n in 0..rows {
            insert(&f, &config, kind, &format!("archived-{n}"), true);
        }
    }
    tx.commit().unwrap();

    assert_eq!(
        measure(&f),
        before,
        "10,500 archived rows must not add scan work"
    );
    // The sweep retired nothing live: every measured pass saw the same rows.
    assert_eq!(true_live(&f, &config, "'resource_launch_origin'"), 2);
    let budget = record_budget(&f);
    let lifecycle = "'lifecycle_context','lifecycle_execution','lifecycle_hold'";
    let resource = "'resource_spend','resource_launch_origin'";
    assert_eq!(budget["resource"]["used"], true_live(&f, &config, resource));
    assert_eq!(budget["resource"]["used"], 2);
    assert_eq!(
        budget["lifecycle"]["used"],
        true_live(&f, &config, lifecycle)
    );
    assert_eq!(budget["lifecycle"]["used"], 6);
    assert_eq!(budget["retrieval"]["used"], 0);
    assert_eq!(
        budget["coordination"]["used"],
        true_live(&f, &config, "'intent'")
    );

    f.store
        .manager_v2_put_record(&config, "retrieval", "fresh", None, 0, &json!({}))
        .unwrap();
    assert_eq!(record_budget(&f)["retrieval"]["used"], 1);
    assert_eq!(
        f.store
            .manager_v2_records_of_kind(&config, "retrieval")
            .unwrap()
            .into_iter()
            .map(|r| r.key)
            .collect::<Vec<_>>(),
        vec!["fresh".to_string()]
    );
}

#[test]
fn displayed_bookkeeping_limit_is_the_enforced_limit() {
    for (class, kind, code) in [
        ("retrieval", "retrieval", "manager_v2_retrieval_limit"),
        (
            "resource",
            "resource_launch_origin",
            "manager_v2_resource_record_limit",
        ),
        ("lifecycle", "lifecycle_hold", "manager_v2_lifecycle_limit"),
        (
            "request_released",
            "request_released",
            "manager_v2_request_release_limit",
        ),
    ] {
        let f = fixture();
        let config = f.store.get_harness_manager(f.project).unwrap().unwrap();
        let displayed = record_budget(&f)[class]["limit"].as_i64().unwrap();
        assert_eq!(displayed, BOOKKEEPING_LIMIT, "{class}");
        let tx = f.store.conn.unchecked_transaction().unwrap();
        for n in 0..displayed - 1 {
            insert(&f, &config, kind, &format!("live:{n}"), false);
        }
        tx.commit().unwrap();
        f.store
            .manager_v2_put_record(&config, kind, "last", None, 0, &json!({}))
            .unwrap();
        assert_eq!(record_budget(&f)[class]["used"], displayed, "{class}");
        let error = f
            .store
            .manager_v2_put_record(&config, kind, "over", None, 0, &json!({}))
            .unwrap_err();
        assert!(error.to_string().contains(code), "{class}: {error}");
    }
}

/// Reviews 1cdaa85f and 1273507c: the rollover (one chain record per rolled
/// standing request), settle and unsettle (one row per request id) classes
/// are unmetered, so the Inspector displays `limit: null` and a new record
/// still lands past `BOOKKEEPING_LIMIT` live rows.
#[test]
fn per_request_marker_classes_are_unmetered_and_displayed_as_such() {
    for kind in ["request_rollover", "request_settle", "request_unsettled"] {
        let f = fixture();
        let config = f.store.get_harness_manager(f.project).unwrap().unwrap();
        let tx = f.store.conn.unchecked_transaction().unwrap();
        for n in 0..BOOKKEEPING_LIMIT {
            insert(&f, &config, kind, &format!("live:{n}"), false);
        }
        tx.commit().unwrap();
        f.store
            .manager_v2_put_record(&config, kind, "next", None, 0, &json!({}))
            .unwrap();
        let budget = record_budget(&f);
        assert_eq!(budget[kind]["used"], BOOKKEEPING_LIMIT + 1, "{kind}");
        assert_eq!(budget[kind]["limit"], Value::Null, "{kind}");
        // The released class keeps its displayed, enforced limit.
        assert_eq!(budget["request_released"]["limit"], BOOKKEEPING_LIMIT);
    }
}

#[test]
fn previous_schema_upgrades_to_the_live_bookkeeping_indexes() {
    // V124 is the index migration under test; later migrations sit above it,
    // so the upgrade lands at the
    // live schema head rather than at V124.
    let head = MANAGER_V2_LIVE_BOOKKEEPING_INDEX_VERSION;
    assert_eq!(head, 124);
    assert!(head < crate::store::LATEST_SCHEMA_VERSION);
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("live-bookkeeping.sqlite");
    let rows = {
        let f = fixture_using(Store::open(&database).unwrap());
        let config = f.store.get_harness_manager(f.project).unwrap().unwrap();
        insert(&f, &config, "retrieval", "archived", true);
        insert(&f, &config, "lifecycle_hold", "live", false);
        crate::store::tests::rewind_post_v121_tail_to(&f.store.conn, head - 1);
        assert_eq!(
            f.store
                .conn
                .query_row("PRAGMA user_version", [], |r| r.get::<_, i32>(0))
                .unwrap(),
            head - 1
        );
        let index_count = |conn: &Connection| -> i64 {
            conn.query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='index' AND name IN (?1,?2)",
                [LIVE_INDEX, COORDINATION_INDEX],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(index_count(&f.store.conn), 0, "rewound to V{}", head - 1);
        f.store
            .conn
            .query_row("SELECT count(*) FROM harness_manager_v2_records", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap()
    };
    let store = Store::open(&database).unwrap();
    assert_eq!(
        store
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get::<_, i32>(0))
            .unwrap(),
        crate::store::LATEST_SCHEMA_VERSION
    );
    for name in [LIVE_INDEX, COORDINATION_INDEX] {
        let found: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='index' AND name=?1",
                [name],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(found, 1, "{name}");
    }
    let after: i64 = store
        .conn
        .query_row("SELECT count(*) FROM harness_manager_v2_records", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(after, rows, "the index migration changes no rows");
    assert!(plan(&store.conn, LIVE_BOOKKEEPING_COUNTS_SQL).contains(LIVE_INDEX));
}
