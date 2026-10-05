//! #975: an accepted-content integration refusal must stop counting as ready
//! work so the persisted Execute intent stops re-prompting the Epic lead.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    clippy::type_complexity
)]
use super::*;

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
fn config(f: &Fixture) -> HarnessManagerConfigV1 {
    f.store.get_harness_manager(f.project).unwrap().unwrap()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
const SOURCE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// Bind an accepted source onto a work without integrating it. A refusal is
/// only meaningful for an accepted-but-unintegrated work.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
fn accept_work(f: &Fixture, key: &str, source: &str) -> WorkRecord {
    let c = config(f);
    let (row, mut work) = f.store.manager_v2_work(&c, key).unwrap();
    work.source_commit = Some(source.into());
    work.acceptance = Some(Acceptance {
        source_commit: source.into(),
        spec_revision: work.spec_revision,
        evidence_digest: format!("sha256:{}", "b".repeat(64)),
        method: "independent_evidence".into(),
        accepted_at: now(),
    });
    f.store
        .manager_v2_put_record(
            &c,
            "work",
            key,
            Some(work.epic_id),
            row.row_version,
            &json!(work),
        )
        .unwrap();
    work
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
fn work_row(f: &Fixture, key: &str) -> Value {
    let c = config(f);
    f.store
        .manager_v2_work_rows(&c, Some(f.epic))
        .unwrap()
        .into_iter()
        .find(|row| row["key"] == key)
        .unwrap()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
fn refusal(f: &Fixture, key: &str, code: &str, source: &str, target: &str) -> ManagerRecordV2 {
    let c = config(f);
    f.store
        .manager_v2_record_integration_refusal(&c, key, code, source, target, f.lead)
        .unwrap()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn current_source_refusal_blocks_an_accepted_work() {
    let f = fixture();
    work(&f, "slice");
    accept_work(&f, "slice", SOURCE);

    let before = work_row(&f, "slice");
    assert_eq!(before["source_accepted"], true);
    assert_eq!(before["ready"], true);

    refusal(
        &f,
        "slice",
        "manager_v2_accepted_content_lost",
        SOURCE,
        &"c".repeat(40),
    );

    let after = work_row(&f, "slice");
    assert_eq!(after["source_accepted"], true);
    assert_eq!(after["integrated"], false);
    assert_eq!(after["ready"], false);
    assert_eq!(after["integration_ready"], false);
    assert!(
        after["blockers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b == "integration_refused:manager_v2_accepted_content_lost"),
        "{after}"
    );

    let page = inspect(&f, ManagerInspectSectionV2::Work);
    let row = page.rows.iter().find(|row| row["key"] == "slice").unwrap();
    assert_eq!(
        row["integration_refusal"]["code"],
        "manager_v2_accepted_content_lost"
    );
    assert_eq!(row["integration_refusal"]["source_commit"], SOURCE);
    assert_eq!(row["integration_refusal"]["target_commit"], "c".repeat(40));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn refused_work_closes_the_intent_work_gate() {
    let f = fixture();
    work(&f, "only");
    let c = config(&f);
    accept_work(&f, "only", SOURCE);
    assert!(f.store.manager_v2_intent_work_gate(&c, f.epic).is_ok());

    refusal(
        &f,
        "only",
        "manager_v2_accepted_content_lost",
        SOURCE,
        &"c".repeat(40),
    );

    let error = f.store.manager_v2_intent_work_gate(&c, f.epic).unwrap_err();
    assert!(
        error.to_string().contains("manager_v2_no_ready_work"),
        "{error}"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn rebinding_to_a_new_source_ignores_the_stale_refusal() {
    let f = fixture();
    work(&f, "slice");
    accept_work(&f, "slice", SOURCE);
    refusal(
        &f,
        "slice",
        "manager_v2_accepted_content_lost",
        SOURCE,
        &"c".repeat(40),
    );
    assert_eq!(work_row(&f, "slice")["ready"], false);

    accept_work(&f, "slice", &"d".repeat(40));

    let rebound = work_row(&f, "slice");
    assert_eq!(rebound["source_accepted"], true);
    assert_eq!(rebound["ready"], true);
    assert_eq!(rebound["integration_ready"], true);
}
