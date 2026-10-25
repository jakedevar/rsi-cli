use super::*;
use crate::scratch_git::Policy;
use crate::testutil::repo_root;

fn all_shards() -> Vec<String> {
    SHARDS.iter().map(|s| (*s).into()).collect()
}
fn all_lanes() -> Vec<Lane> {
    plan(&all_shards(), &["rpc".into(), "smoke".into()], 4).unwrap()
}
fn state() -> State {
    State {
        tip: "a".repeat(40),
        tree: "b".repeat(40),
        jobs: 4,
        started_at: "2026-10-22T00:00:00Z".into(),
        fingerprint: json!({"digest":"fixture","inputs":{"jobs":4}}),
        closed_since: None,
        results: vec![],
    }
}
fn lane_result(lane: &Lane, code: i32) -> Value {
    result(
        lane.label(),
        code,
        1.23456,
        &format!("target/qa/{}.log", lane.label()),
        "Summary [ 1.0s] 10 tests run: 10 passed, 0 skipped\n",
        matches!(lane, Lane::Shard { .. }).then(|| shard_environment("disk").unwrap()),
    )
    .unwrap()
}
#[derive(Default)]
struct Fake {
    calls: Vec<String>,
    checks: usize,
    static_red: bool,
    fail_lane: Option<String>,
}
impl Backend for Fake {
    fn verify_source(&mut self, _: &State) -> io::Result<()> {
        self.checks += 1;
        Ok(())
    }
    fn run_lane(&mut self, lane: &Lane, _: u32) -> io::Result<Value> {
        self.calls.push(lane.label().into());
        if self.fail_lane.as_deref() == Some(lane.label()) {
            return Err(error("owned lane interrupted"));
        }
        Ok(lane_result(
            lane,
            if self.static_red && matches!(lane, Lane::Static) {
                1
            } else {
                0
            },
        ))
    }
}
fn context<'a>(tip: &'a str) -> ReportContext<'a> {
    ReportContext {
        final_tip: tip,
        finished_at: "2026-10-22T01:00:00Z",
        host: "fixture-host",
        runner: "native-test-fixture",
        report_ref: "target/qa/sweep-report.json",
        closed_issues: json!([]),
        signature_matches: json!({}),
    }
}
#[test]
fn captures_nextest_failures_and_signal_aborts() {
    let output = "        FAIL [   1.950s] (110/161) rsid::start_chained_workflow_rpc valid_master_improve_params_register_chain\n     SIGABRT [   3.352s] (214/302) rsid rpc::tests::recursive_dag_second_live_dogfood_two_task_graph_runs_one_at_a_time\n     SIGABRT [   3.352s] (214/302) rsid rpc::tests::recursive_dag_second_live_dogfood_two_task_graph_runs_one_at_a_time\n";
    assert_eq!(
        failures(output),
        vec![
            "rsid rpc::tests::recursive_dag_second_live_dogfood_two_task_graph_runs_one_at_a_time",
            "rsid::start_chained_workflow_rpc valid_master_improve_params_register_chain"
        ]
    );
}
#[test]
fn captures_cargo_test_failures() {
    assert_eq!(
        failures(
            "test manual::tests::html_is_current ... FAILED\ntest every_research_json_sidecar_validates ... FAILED\ntest result: FAILED. 0 passed; 1 failed; 0 ignored\n"
        ),
        vec![
            "every_research_json_sidecar_validates",
            "manual::tests::html_is_current"
        ]
    );
}
#[test]
fn shard_spec_env_is_the_gate_default() {
    assert_eq!(
        spec_environment(),
        BTreeMap::from([
            ("CARGO_BUILD_JOBS".into(), "4".into()),
            ("CARGO_PROFILE_DEV_DEBUG".into(), "line-tables-only".into())
        ])
    );
    let env = lane_environment(&BTreeMap::new(), true);
    assert_eq!(env["CARGO_INCREMENTAL"], "0");
}
#[test]
fn tmpdir_class_separates_tmpfs_from_disk() {
    // statfs needs no write, so classify the checkout itself: it is on disk on
    // every supported host, while the system temp root is tmpfs on the hub and a
    // directory under target/ would block build-cache reclaim (#1429).
    let disk_root = repo_root();
    assert_eq!(tmpdir_class(&disk_root).unwrap(), "disk");
    #[cfg(target_os = "linux")]
    if Path::new("/dev/shm").is_dir() {
        assert_eq!(tmpdir_class(Path::new("/dev/shm")).unwrap(), "tmpfs");
    }
    assert!(tmpdir_class(&disk_root.join("rsi-qa-class-missing")).is_err());
}
#[test]
fn run_passes_and_records_the_shard_environment() {
    let base = BTreeMap::from([
        ("CARGO_BUILD_JOBS".into(), "16".into()),
        ("CARGO_INCREMENTAL".into(), "1".into()),
    ]);
    let env = lane_environment(&base, true);
    assert_eq!(env["CARGO_BUILD_JOBS"], "4");
    assert_eq!(env["CARGO_PROFILE_DEV_DEBUG"], "line-tables-only");
    assert_eq!(env["CARGO_INCREMENTAL"], "1");
    assert_eq!(base["CARGO_BUILD_JOBS"], "16");
    let lane = Lane::Shard {
        name: "store-01".into(),
        slow_tier: true,
    };
    let row = lane_result(&lane, 0);
    assert_eq!(
        row["environment"],
        json!({"spec_env":{"CARGO_BUILD_JOBS":"4","CARGO_PROFILE_DEV_DEBUG":"line-tables-only"},"tmpdir_class":"disk"})
    );
    assert_eq!(row["seconds"], 1.235);
}
#[test]
fn lanes_without_a_spec_env_record_none() {
    let row = result("rsi", 0, 0.1, "target/qa/rsi.log", "", None).unwrap();
    assert!(row.get("environment").is_none());
    assert_eq!(
        lane_environment(
            &BTreeMap::from([("CARGO_BUILD_JOBS".into(), "16".into())]),
            false
        )["CARGO_BUILD_JOBS"],
        "16"
    );
}
#[test]
fn the_sweep_runs_the_slow_tier_of_every_shard() {
    let lanes = all_lanes();
    assert_eq!(lanes.len(), 22);
    let shards = lanes
        .iter()
        .filter(|l| {
            matches!(
                l,
                Lane::Shard {
                    slow_tier: true,
                    ..
                }
            )
        })
        .count();
    assert_eq!(shards, 16);
    assert!(plan(&all_shards()[..15], &["rpc".into()], 4).is_err());
    let mut duplicate = all_shards();
    duplicate[0] = duplicate[1].clone();
    assert!(plan(&duplicate, &["rpc".into()], 4).is_err());
    for jobs in [0, 5] {
        assert!(plan(&all_shards(), &["rpc".into()], jobs).is_err());
    }
}
#[test]
fn mandatory_companions_include_remote_read_and_exact_targets() {
    let lanes = all_lanes();
    assert_eq!(
        lanes
            .iter()
            .filter(|l| !matches!(l, Lane::Static | Lane::Shard { .. }))
            .count(),
        5
    );
    let shared = Lane::Common.cargo_command(4).unwrap();
    assert_eq!(
        shared,
        vec![
            "cargo",
            "test",
            "-p",
            "rsi-common",
            "-p",
            "rsid-remote-read",
            "--",
            "--test-threads=4"
        ]
    );
    let targets = Lane::Integrations {
        targets: vec!["rpc".into(), "smoke".into()],
    }
    .cargo_command(2)
    .unwrap();
    assert_eq!(
        targets,
        vec![
            "cargo",
            "nextest",
            "run",
            "--profile",
            "rsid-fast",
            "-p",
            "rsid",
            "--test",
            "rpc",
            "--test",
            "smoke",
            "--status-level",
            "all",
            "--final-status-level",
            "all",
            "-j",
            "2"
        ]
    );
    assert!(Lane::Static.cargo_command(4).is_none());
    assert!(plan(&all_shards(), &[], 4).is_err());
}
#[test]
fn summaries_require_consistent_counts_and_keep_the_last_summary() {
    let summary = summaries(
        "Summary [ 1s] 2 tests run: 2 passed, 0 skipped\nnoise\nSummary [ 2s] 3 tests run: 2 passed, 1 failed, 4 skipped\n",
    );
    assert_eq!(summary.len(), 1);
    assert_eq!(
        shard_counts(&summary),
        Some(json!({"run":3,"passed":2,"failed":1,"skipped":4}))
    );
    assert_eq!(
        shard_counts(&["Summary [ 1s] 9 tests run: 2 passed, 1 failed".into()]),
        None
    );
    assert_eq!(shard_counts(&[]), None);
}
#[test]
fn known_failure_annotations_keep_states_and_issue_lists() {
    let lines = [
        "KNOWN #12 flake pkg::known",
        "KNOWN? #13,#14 pkg::maybe",
        "NEW pkg::new",
        "unknown text",
        "KNOWN #bad flake ignored",
    ]
    .map(str::to_owned);
    assert_eq!(
        parse_signature_matches(&lines),
        json!({"pkg::known":{"state":"known","issues":[12],"class":"flake"},"pkg::maybe":{"state":"known?","issues":[13,14]},"pkg::new":{"state":"new"}})
    );
}
#[test]
fn static_failure_prevents_shards_and_is_reused_on_resume() {
    let mut state = state();
    let mut backend = Fake {
        static_red: true,
        ..Fake::default()
    };
    execute_pending(&mut state, &all_lanes(), &mut backend, |_| Ok(())).unwrap();
    assert_eq!(backend.calls, vec!["static"]);
    let mut resumed = Fake::default();
    execute_pending(&mut state, &all_lanes(), &mut resumed, |_| Ok(())).unwrap();
    assert_eq!(resumed.calls.len(), 0);
    let (code, _, sweep) = reports(&state, context(&state.tip), |_| {
        panic!("no completed shard")
    })
    .unwrap();
    assert_eq!(code, 1);
    assert_eq!(sweep["complete"], false);
}
#[test]
fn resume_binds_arguments_fingerprint_tree_and_clean_source() {
    let state = state();
    state
        .resume(4, None, &state.fingerprint, &state.tree, false)
        .unwrap();
    for result in [
        state.resume(3, None, &state.fingerprint, &state.tree, false),
        state.resume(4, Some("older"), &state.fingerprint, &state.tree, false),
        state.resume(4, None, &json!({}), &state.tree, false),
        state.resume(4, None, &state.fingerprint, &"c".repeat(40), false),
        state.resume(4, None, &state.fingerprint, &state.tree, true),
    ] {
        assert!(result.is_err());
    }
}
#[test]
fn interrupted_lane_is_not_completed_and_resumes_only_missing_work() {
    let mut state = state();
    let lanes = all_lanes();
    let mut backend = Fake {
        fail_lane: Some(lanes[3].label().into()),
        ..Fake::default()
    };
    let mut checkpoints = 0;
    assert!(
        execute_pending(&mut state, &lanes, &mut backend, |_| {
            checkpoints += 1;
            Ok(())
        })
        .is_err()
    );
    assert_eq!(state.results.len(), 3);
    assert_eq!(checkpoints, 3);
    let mut resumed = Fake::default();
    execute_pending(&mut state, &lanes, &mut resumed, |_| Ok(())).unwrap();
    assert_eq!(resumed.calls.len(), 19);
    assert_eq!(resumed.calls[0], lanes[3].label());
    assert_eq!(resumed.checks, 23);
    assert_eq!(state.results.len(), 22);
}
#[test]
fn reports_preserve_v1_schema_failure_annotations_and_complete_coverage() {
    let mut state = state();
    execute_pending(&mut state, &all_lanes(), &mut Fake::default(), |_| Ok(())).unwrap();
    let mut ctx = context(&state.tip);
    ctx.signature_matches =
        json!({"store-01":{"a":{"state":"known","issues":[1],"class":"flake"}}});
    let (code, report, sweep) =
        reports(&state, ctx, |name| Ok(json!({"digest":name,"inputs":{}}))).unwrap();
    assert_eq!(code, 0);
    assert_eq!(sweep["schema_version"], 1);
    assert_eq!(sweep["complete"], true);
    assert_eq!(sweep["shards"].as_array().unwrap().len(), 16);
    assert_eq!(sweep["lanes"].as_array().unwrap().len(), 5);
    assert_eq!(report["tip_stable"], true);
    crate::qa_cache::validate_report(&sweep).unwrap();
    let shard = sweep["shards"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "store-01")
        .unwrap();
    assert!(shard.get("known_failures").is_some());
}
#[test]
fn zero_exit_without_required_counts_is_incomplete_and_red() {
    let mut state = state();
    execute_pending(&mut state, &all_lanes(), &mut Fake::default(), |_| Ok(())).unwrap();
    state.results[1]["summary"] = json!([]);
    let (code, _, sweep) = reports(&state, context(&state.tip), |_| Ok(json!({}))).unwrap();
    assert_eq!(code, 1);
    assert_eq!(sweep["complete"], false);
    assert!(crate::qa_cache::validate_report(&sweep).is_err());
}
#[test]
fn persisted_state_is_atomic_and_bad_rows_are_refused() {
    let policy = Policy::from_process(&repo_root()).unwrap();
    let temp = policy.tempdir("rsi-qa-state-").unwrap();
    let mut state = state();
    state.save(temp.path()).unwrap();
    let first = fs::read(temp.path().join("state.json")).unwrap();
    state.record(lane_result(&Lane::Static, 0)).unwrap();
    state.save(temp.path()).unwrap();
    let decoded: State =
        serde_json::from_slice(&fs::read(temp.path().join("state.json")).unwrap()).unwrap();
    assert_eq!(decoded.results.len(), 1);
    assert_ne!(first, fs::read(temp.path().join("state.json")).unwrap());
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
    assert!(state.record(lane_result(&Lane::Static, 0)).is_err());
    let mut bad = lane_result(&Lane::Bins, 0);
    bad["log"] = "../outside".into();
    assert!(state.record(bad).is_err());
    let mut duplicate = state.clone();
    duplicate.results.push(state.results[0].clone());
    assert!(duplicate.validate().is_err());
}
#[test]
fn evidence_directory_cannot_escape_target_or_write_at_its_top_level() {
    let temp = Policy::from_process(&repo_root())
        .unwrap()
        .tempdir("rsi-qa-path-")
        .unwrap();
    let repo = temp.path().join("repo");
    fs::create_dir_all(repo.join("target")).unwrap();
    assert_eq!(
        evidence_directory(&repo, Path::new("target/qa/new")).unwrap(),
        repo.canonicalize().unwrap().join("target/qa/new")
    );
    for bad in ["target", "outside", "target/../outside"] {
        assert!(evidence_directory(&repo, Path::new(bad)).is_err());
    }
    #[cfg(unix)]
    {
        let outside = temp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, repo.join("target/link")).unwrap();
        assert!(evidence_directory(&repo, Path::new("target/link/new")).is_err());
    }
}
