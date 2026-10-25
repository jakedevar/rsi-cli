//! Fixture candidates for #3194: a valid one, an unknown field, a digest mismatch, a missing notes
//! heading, a held-out leak hit, a generality hit in `instructions.md` (and none in `notes.md`), a
//! read-audit hit, and the deterministic index.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use serde_json::{Value, json};
use tempfile::TempDir;

use super::scaffold::{NewSpec, new_candidate, rename};
use super::verify::{VerifyReport, verify};
use super::*;

const NOTES: &str = "# notes\n\n## Parent\n{parent}\n\n## Hypothesis\nThe model stops early.\n\n\
## Single change\nRaise the iteration cap.\n\n## Expected effect\nMore tasks finish; cost rises.\n\n\
## Prior candidates cited\nnone\n";

struct Fixture {
    _tmp: TempDir,
    repo: Repo,
    split: Split,
}

fn real_tb2() -> PathBuf {
    Path::new(rsi_common::manifest_dir!()).join("../../eval/bench/tb2")
}

/// A scratch repository holding the committed pin files.
fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let tb2 = tmp.path().join("eval/bench/tb2");
    std::fs::create_dir_all(&tb2).unwrap();
    for name in ["split.json", "dataset.toml"] {
        std::fs::copy(real_tb2().join(name), tb2.join(name)).unwrap();
    }
    let repo = Repo {
        root: tmp.path().to_path_buf(),
    };
    let split = repo.split().unwrap();
    Fixture {
        _tmp: tmp,
        repo,
        split,
    }
}

fn spec(parent: Option<&str>) -> NewSpec {
    NewSpec {
        parent: parent.map(str::to_string),
        seed: parent.is_none(),
        ordinal: None,
        records: Vec::new(),
        host: "ssh://arch-laptop".into(),
        model: "deepseek/deepseek-v4.1-flash".into(),
        effort: "high".into(),
    }
}

fn notes_for(parent: &str) -> String {
    NOTES.replace("{parent}", parent)
}

fn seed(fx: &Fixture) -> String {
    let created = new_candidate(&fx.repo, &spec(None)).unwrap();
    std::fs::write(
        fx.repo.dir(&created.candidate).join(NOTES_FILE),
        notes_for("none"),
    )
    .unwrap();
    created.candidate
}

/// A candidate child of `parent`, edited to set `[loop] max_iterations = 200` and an
/// instructions file, renamed to its digest id, with filled notes.
fn child(fx: &Fixture, parent: &str, instructions: &str) -> String {
    let created = new_candidate(&fx.repo, &spec(Some(parent))).unwrap();
    let dir = fx.repo.dir(&created.candidate);
    std::fs::write(
        dir.join(OVERLAY_FILE_NAME),
        "instructions = \"instructions.md\"\n[loop]\nmax_iterations = 200\n",
    )
    .unwrap();
    std::fs::write(dir.join("instructions.md"), instructions).unwrap();
    let renamed = rename(&fx.repo, &created.candidate).unwrap();
    std::fs::write(
        fx.repo.dir(&renamed.candidate).join(NOTES_FILE),
        notes_for(parent),
    )
    .unwrap();
    renamed.candidate
}

fn check<'a>(report: &'a VerifyReport, name: &str) -> &'a verify::Check {
    report.checks.iter().find(|c| c.name == name).unwrap()
}

fn verified(fx: &Fixture, id: &str) -> VerifyReport {
    verify(&fx.repo, id, None, None).unwrap()
}

#[test]
fn a_valid_candidate_passes_every_check_and_writes_verify_json() {
    let fx = fixture();
    let seed = seed(&fx);
    assert!(seed.starts_with("c00-"));
    let id = child(&fx, &seed, "Survey the environment first, then work.\n");
    let report = verified(&fx, &id);
    assert!(report.ok, "{:?}", report.checks);
    assert!(report.checks.iter().all(|c| c.ok));
    assert_eq!(
        report
            .checks
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        [
            "schema",
            "digest",
            "run_config",
            "notes",
            "parent",
            "records",
            "leak_audit",
            "read_audit"
        ]
    );
    assert!(!report.read_audit_ran, "no proposer export was given");
    let digest = report.overlay_digest.clone().unwrap();
    assert!(id.ends_with(&digest["sha256:".len().."sha256:".len() + 12]));
    let written: VerifyReport =
        serde_json::from_str(&std::fs::read_to_string(fx.repo.dir(&id).join(VERIFY_FILE)).unwrap())
            .unwrap();
    assert!(written.ok && written.candidate == id);
    // The seed verifies too: an empty overlay has its own digest.
    assert!(verified(&fx, &seed).ok);
}

#[test]
fn an_unknown_overlay_field_fails_the_schema_check() {
    let fx = fixture();
    let seed = seed(&fx);
    let id = child(&fx, &seed, "Work carefully.\n");
    std::fs::write(
        fx.repo.dir(&id).join(OVERLAY_FILE_NAME),
        "instructions = \"instructions.md\"\nsurprise = 1\n",
    )
    .unwrap();
    let report = verified(&fx, &id);
    assert!(!report.ok);
    let schema = check(&report, "schema");
    assert!(!schema.ok && schema.findings[0].starts_with("overlay_parse_failed"));
    // Nothing to compare the id with once the overlay does not resolve.
    assert!(!check(&report, "digest").ok && !check(&report, "digest").ran);
}

#[test]
fn an_id_that_does_not_match_the_overlay_digest_fails() {
    let fx = fixture();
    let seed = seed(&fx);
    let id = child(&fx, &seed, "Work carefully.\n");
    let wrong = "c01-000000000000";
    std::fs::rename(fx.repo.dir(&id), fx.repo.dir(wrong)).unwrap();
    let report = verified(&fx, wrong);
    let digest = check(&report, "digest");
    assert!(!report.ok && !digest.ok);
    assert!(digest.findings[0].starts_with("id_digest_mismatch"));
    assert!(digest.findings[0].contains(&id), "names the expected id");
    // Editing the overlay without renaming is the usual way to get here; `rename` repairs it.
    let fixed = rename(&fx.repo, wrong).unwrap();
    assert!(verified(&fx, &fixed.candidate).ok);
}

#[test]
fn a_candidate_that_repeats_its_parents_overlay_is_a_duplicate() {
    let fx = fixture();
    let seed = seed(&fx);
    let twin = new_candidate(&fx.repo, &spec(Some(&seed))).unwrap();
    std::fs::write(
        fx.repo.dir(&twin.candidate).join(NOTES_FILE),
        notes_for(&seed),
    )
    .unwrap();
    let report = verified(&fx, &twin.candidate);
    assert!(
        check(&report, "digest")
            .findings
            .iter()
            .any(|f| f.contains("same overlay digest"))
    );
}

#[test]
fn a_missing_or_empty_notes_section_fails() {
    let fx = fixture();
    let seed = seed(&fx);
    let id = child(&fx, &seed, "Work carefully.\n");
    let notes_path = fx.repo.dir(&id).join(NOTES_FILE);
    let full = std::fs::read_to_string(&notes_path).unwrap();
    std::fs::write(
        &notes_path,
        full.replace("## Expected effect", "## Outlook"),
    )
    .unwrap();
    let notes = verified(&fx, &id)
        .checks
        .into_iter()
        .find(|c| c.name == "notes")
        .unwrap();
    assert_eq!(notes.findings, ["missing heading: Expected effect"]);

    std::fs::write(
        &notes_path,
        full.replace("The model stops early.", "<placeholder>"),
    )
    .unwrap();
    let notes = verified(&fx, &id)
        .checks
        .into_iter()
        .find(|c| c.name == "notes")
        .unwrap();
    assert_eq!(notes.findings, ["empty section: Hypothesis"]);
}

#[test]
fn a_parent_must_be_in_the_store() {
    let fx = fixture();
    let seed = seed(&fx);
    let id = child(&fx, &seed, "Work carefully.\n");
    let notes_path = fx.repo.dir(&id).join(NOTES_FILE);
    std::fs::write(&notes_path, notes_for("c09-0123456789ab")).unwrap();
    let report = verified(&fx, &id);
    assert!(check(&report, "parent").findings[0].contains("not in the candidate store"));
}

#[test]
fn a_run_config_must_name_this_candidates_overlay_on_the_search_split() {
    let fx = fixture();
    let seed = seed(&fx);
    let id = child(&fx, &seed, "Work carefully.\n");
    let run_path = fx.repo.dir(&id).join(RUN_FILE);
    let run = std::fs::read_to_string(&run_path).unwrap();
    assert!(verified(&fx, &id).ok);

    std::fs::write(
        &run_path,
        run.replace("split = \"search\"", "split = \"held_out\""),
    )
    .unwrap();
    let report = verified(&fx, &id);
    assert!(check(&report, "run_config").findings[0].contains("search"));

    std::fs::write(&run_path, run.replace(&id, "c01-ffffffffffff")).unwrap();
    assert!(check(&verified(&fx, &id), "run_config").findings[0].starts_with("overlay must be"));

    std::fs::write(&run_path, format!("{run}\nsurprise = 1\n")).unwrap();
    assert!(check(&verified(&fx, &id), "run_config").findings[0].starts_with("run_parse_failed"));
}

#[test]
fn the_run_config_mirror_parses_a_shipped_run_config() {
    // `verify` parses run.toml with a mirror of the daemon's type: it must accept a real config
    // (the daemon's own tests parse the same files). The candidate rules still apply to it.
    let text = std::fs::read_to_string(real_tb2().join("runs/calibration-flash.toml")).unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let id = "c01-000000000000";
    let run = format!("overlay = \"{}\"\n{text}", Repo::overlay_rel(id));
    std::fs::write(tmp.path().join(RUN_FILE), run).unwrap();
    let findings = verify::check_run_config(tmp.path(), id);
    assert_eq!(
        findings,
        ["split must be \"search\" with no explicit tasks"]
    );
}

#[test]
fn a_held_out_term_in_any_candidate_file_is_a_hit_without_the_term() {
    let fx = fixture();
    let seed = seed(&fx);
    let id = child(&fx, &seed, "Work carefully.\n");
    let held = &fx.split.held_out[0];
    let notes_path = fx.repo.dir(&id).join(NOTES_FILE);
    let notes = std::fs::read_to_string(&notes_path).unwrap();
    std::fs::write(
        &notes_path,
        notes.replace("The model stops early.", &format!("It fails {held}.")),
    )
    .unwrap();
    let report = verified(&fx, &id);
    let leak = check(&report, "leak_audit");
    assert!(!report.ok && !leak.ok);
    assert!(
        leak.findings[0].starts_with("HIT ") && leak.findings[0].contains("notes.md:"),
        "{:?}",
        leak.findings
    );
    let json = serde_json::to_string(&report).unwrap();
    assert!(
        !json.contains(held.as_str()),
        "the held-out name must not be echoed"
    );

    // The same name in the instructions (a file the model reads) is a hit there.
    std::fs::write(&notes_path, notes).unwrap();
    std::fs::write(
        fx.repo.dir(&id).join("instructions.md"),
        format!("Do not stall on {held}.\n"),
    )
    .unwrap();
    let report = verified(&fx, &id);
    assert!(
        check(&report, "leak_audit")
            .findings
            .iter()
            .any(|f| f.starts_with("HIT ") && f.contains("instructions.md:"))
    );
}

#[test]
fn a_search_term_is_a_generality_hit_in_instructions_but_not_in_notes() {
    let fx = fixture();
    let seed = seed(&fx);
    let id = child(&fx, &seed, "Work carefully.\n");
    let search = &fx.split.search[0];
    let dir = fx.repo.dir(&id);

    // notes.md may cite search tasks.
    let notes = std::fs::read_to_string(dir.join(NOTES_FILE)).unwrap();
    std::fs::write(
        dir.join(NOTES_FILE),
        notes.replace(
            "The model stops early.",
            &format!("It stops early on {search}."),
        ),
    )
    .unwrap();
    let report = verified(&fx, &id);
    assert!(
        report.ok,
        "notes may cite a search task: {:?}",
        report.checks
    );

    // The model-facing instructions may not.
    std::fs::write(
        dir.join("instructions.md"),
        format!("On {search}, run the build first.\n"),
    )
    .unwrap();
    let report = verified(&fx, &id);
    let leak = check(&report, "leak_audit");
    assert!(!report.ok);
    assert_eq!(leak.findings.len(), 1, "{:?}", leak.findings);
    assert!(
        leak.findings[0].starts_with("GENERALITY ")
            && leak.findings[0].contains("instructions.md:1")
    );
}

fn tool_call(sequence: i64, input: &Value) -> Value {
    json!({"sequence": sequence, "event_type": "ToolUse", "tool_name": "Bash", "tool_input": input, "content": "", "truncated": false})
}

fn export(events: Vec<Value>) -> Value {
    json!({"session_id": "s", "events": events, "has_more": false})
}

fn write_export(dir: &Path, value: &Value) -> PathBuf {
    let path = dir.join("proposer.json");
    std::fs::write(&path, value.to_string()).unwrap();
    path
}

#[test]
fn the_read_audit_flags_records_unlisted_runs_and_held_out_tasks() {
    let fx = fixture();
    let seed = seed(&fx);
    let id = child(&fx, &seed, "Work carefully.\n");
    std::fs::write(
        fx.repo.dir(&seed).join(RECORDS_FILE),
        r#"["eval/bench/tb2/records/run-1.json"]"#,
    )
    .unwrap();
    let held = &fx.split.held_out[1];
    let search = &fx.split.search[0];
    let session = tempfile::tempdir().unwrap();

    // Clean: the candidate store, a listed run, a search task and an assistant message that
    // mentions a held-out task (only tool calls are audited).
    let clean = export(vec![
        tool_call(
            1,
            &json!({"command": "cat eval/bench/tb2/candidates/index.json"}),
        ),
        tool_call(
            2,
            &json!({"command": format!("grep -n {search} ~/.rsi/bench/run-1/trace.jsonl")}),
        ),
        json!({"sequence": 3, "event_type": "Message", "role": "Assistant", "content": format!("Held-out {held} is off limits")}),
    ]);
    let report = verify(
        &fx.repo,
        &id,
        Some(&write_export(session.path(), &clean)),
        None,
    )
    .unwrap();
    assert!(report.ok, "{:?}", report.checks);
    assert!(report.read_audit_ran && report.tool_calls_audited == 2);

    let dirty = export(vec![
        tool_call(
            1,
            &json!({"command": "cat eval/bench/tb2/records/run-1.json"}),
        ),
        tool_call(2, &json!({"command": "cat ~/.rsi/bench/run-9/trace.jsonl"})),
        tool_call(3, &json!({"command": format!("grep -rn {held} .")})),
        tool_call(4, &json!({"command": "ls ~/.rsi/bench"})),
    ]);
    let report = verify(
        &fx.repo,
        &id,
        Some(&write_export(session.path(), &dirty)),
        None,
    )
    .unwrap();
    let audit = check(&report, "read_audit");
    assert!(!report.ok && !audit.ok);
    assert_eq!(
        audit.findings,
        [
            "READ seq=1 tool=Bash kind=records_path",
            "READ seq=2 tool=Bash kind=bench_run run=run-9",
            "READ seq=3 tool=Bash kind=held_out_task",
            "READ seq=4 tool=Bash kind=bench_dir_listing",
        ]
    );
    assert!(
        !serde_json::to_string(&report)
            .unwrap()
            .contains(held.as_str())
    );

    // An export that is not a session export is a failed audit, not a pass.
    let report = verify(
        &fx.repo,
        &id,
        Some(&write_export(session.path(), &json!({"x": 1}))),
        None,
    )
    .unwrap();
    assert!(check(&report, "read_audit").findings[0].starts_with("audit_failed"));
}

#[test]
fn the_read_audit_fails_an_incomplete_export() {
    let fx = fixture();
    let seed = seed(&fx);
    let id = child(&fx, &seed, "Work carefully.\n");
    let session = tempfile::tempdir().unwrap();
    let clean_call = || {
        tool_call(
            1,
            &json!({"command": "cat eval/bench/tb2/candidates/index.json"}),
        )
    };

    let mut partial_page = export(vec![clean_call()]);
    partial_page["has_more"] = json!(true);
    let mut truncated_result = export(vec![clean_call()]);
    truncated_result["truncated"] = json!(true);
    let mut clipped = clean_call();
    clipped["truncated"] = json!(true);
    let clipped_call = export(vec![clipped]);

    for (bad, cause) in [
        (partial_page, "cause=has_more"),
        (truncated_result, "cause=truncated"),
        (clipped_call, "cause=truncated_tool_call"),
    ] {
        let report = verify(
            &fx.repo,
            &id,
            Some(&write_export(session.path(), &bad)),
            None,
        )
        .unwrap();
        let audit = check(&report, "read_audit");
        assert!(!report.ok && !audit.ok, "{cause}: {:?}", report.checks);
        assert!(
            audit
                .findings
                .iter()
                .any(|f| f.contains("kind=incomplete_export") && f.contains(cause)),
            "{cause}: {:?}",
            audit.findings
        );
    }

    // A complete export with none of the flags still passes.
    let report = verify(
        &fx.repo,
        &id,
        Some(&write_export(session.path(), &export(vec![clean_call()]))),
        None,
    )
    .unwrap();
    assert!(report.ok, "{:?}", report.checks);
}

#[test]
fn the_read_audit_accepts_an_rpc_envelope_and_a_bare_event_array() {
    let held = vec!["held-task".to_string()];
    let runs = std::collections::BTreeSet::new();
    let call = tool_call(7, &json!({"path": "/x/held-task/tests"}));
    for shape in [
        json!([call.clone()]),
        json!({"result": {"events": [call.clone()]}}),
    ] {
        let report = audit::read_audit(&shape, &held, &runs).unwrap();
        assert_eq!(report.findings, ["READ seq=7 tool=Bash kind=held_out_task"]);
    }
}

#[test]
fn the_read_audit_accepts_the_proposer_fixture_digest_and_trace_paths() {
    // #3197: the dry-run fixture lives outside the committed records, and its digests name
    // repository-relative traces, so reading them is clean.
    let held = vec!["held-task".to_string()];
    let runs = std::collections::BTreeSet::new();
    let dir = "eval/bench/tb2/proposer/fixtures";
    let export = json!([
        tool_call(1, &json!({"command": format!("ls {dir}/digest")})),
        tool_call(
            2,
            &json!({"command": format!("grep -n 127 {dir}/bench/fixture-baseline/regex-chess__a0/trace.jsonl")})
        ),
    ]);
    let report = audit::read_audit(&export, &held, &runs).unwrap();
    assert!(report.findings.is_empty(), "{:?}", report.findings);
    assert_eq!(report.tool_calls, 2);
}

#[test]
fn index_is_deterministic_and_carries_gain_cost_and_eligibility() {
    let fx = fixture();
    let seed = seed(&fx);
    let id = child(&fx, &seed, "Work carefully.\n");
    let records = fx.repo.root.join("eval/bench/tb2/records");
    std::fs::create_dir_all(&records).unwrap();
    let base = crate::bench::tests::record(&[("t1", false), ("t2", true), ("t3", false)], "m");
    let cand = crate::bench::tests::record(&[("t1", true), ("t2", true), ("t3", false)], "m");
    std::fs::write(records.join("base.json"), base.to_json().unwrap()).unwrap();
    std::fs::write(records.join("screen.json"), cand.to_json().unwrap()).unwrap();
    std::fs::write(
        fx.repo.dir(&seed).join(RECORDS_FILE),
        r#"["eval/bench/tb2/records/base.json"]"#,
    )
    .unwrap();
    std::fs::write(
        fx.repo.dir(&id).join(RECORDS_FILE),
        r#"["eval/bench/tb2/records/screen.json", "eval/bench/tb2/records/gone.json"]"#,
    )
    .unwrap();
    assert!(verified(&fx, &id).ok);

    let path = index::write_index(&fx.repo).unwrap();
    let first = std::fs::read(&path).unwrap();
    index::write_index(&fx.repo).unwrap();
    assert_eq!(
        first,
        std::fs::read(&path).unwrap(),
        "no timestamps, stable order"
    );

    let index: Value = serde_json::from_slice(&first).unwrap();
    let entries = index["candidates"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["id"], json!(seed));
    assert_eq!(entries[0]["status"], "seed");
    assert_eq!(entries[0]["gain"], Value::Null);
    let entry = &entries[1];
    assert_eq!(entry["id"], json!(id));
    assert_eq!(entry["parent"], json!(seed));
    assert_eq!(entry["status"], "extended");
    assert_eq!(entry["records"].as_array().unwrap().len(), 2);
    assert!((entry["gain"].as_f64().unwrap() - 1.0 / 3.0).abs() < 1e-5);
    assert!((entry["cost_per_trial_usd"].as_f64().unwrap() - 0.5).abs() < 1e-9);
    assert_eq!(entry["eligibility"]["verify"], "clean");
    assert_eq!(entry["eligibility"]["records_loaded"], 1);
    assert_eq!(entry["eligibility"]["records_missing"], 1);
    assert_eq!(entry["eligibility"]["invalid_route_trials"], 0);
    assert_eq!(entry["decision"], Value::Null);
    assert!(
        entry["overlay_digest"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );

    // Editing a verified candidate makes its verification stale.
    let dir = fx.repo.dir(&id);
    let notes = std::fs::read_to_string(dir.join(NOTES_FILE)).unwrap();
    std::fs::write(dir.join(NOTES_FILE), notes.replace("early", "late")).unwrap();
    index::write_index(&fx.repo).unwrap();
    let index: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(index["candidates"][1]["eligibility"]["verify"], "stale");
}

#[test]
fn ids_and_pointers_are_checked_before_any_path_is_built() {
    assert_eq!(parse_id("c03-1a2b3c4d5e6f"), Some((3, "1a2b3c4d5e6f")));
    assert_eq!(parse_id("c3-1a2b3c4d5e6f"), None);
    assert_eq!(parse_id("c03-1A2B3C4D5E6F"), None);
    assert_eq!(parse_id("c03-1a2b3c4d5e6"), None);
    let fx = fixture();
    for bad in ["../x", "a/b", ".hidden", ""] {
        assert!(verify(&fx.repo, bad, None, None).is_err(), "{bad:?}");
    }
    let mut bad = spec(None);
    bad.records = vec!["eval/bench/tb2/records/../../secret.json".into()];
    assert!(new_candidate(&fx.repo, &bad).is_err());
}

#[test]
fn ordinals_are_unique_and_default_to_the_next_free_one() {
    let fx = fixture();
    let seed = seed(&fx);
    let first = new_candidate(&fx.repo, &spec(Some(&seed))).unwrap();
    assert!(first.candidate.starts_with("c01-"));
    assert!(
        new_candidate(&fx.repo, &spec(None)).is_err(),
        "c00 is taken"
    );
    let mut explicit = spec(Some(&seed));
    explicit.ordinal = Some(1);
    assert!(new_candidate(&fx.repo, &explicit).is_err());
    explicit.ordinal = Some(7);
    assert!(
        new_candidate(&fx.repo, &explicit)
            .unwrap()
            .candidate
            .starts_with("c07-")
    );
}
