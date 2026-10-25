use super::*;
use crate::qa_cache::SHARDS;

const TIP: &str = "1111111111111111111111111111111111111111";
const TREE: &str = "2222222222222222222222222222222222222222";
const SUMMARY: &str = "     Summary [   1.0s] 3 tests run: 3 passed, 0 skipped\n";

struct Fake {
    events: Vec<String>,
    head_tree: String,
    dirty: bool,
    fingerprint: Value,
    /// Lanes whose row is red (exit 1) or has no summary (so counts are incomplete).
    red: Vec<&'static str>,
    no_summary: Vec<&'static str>,
    interrupt_at: Option<&'static str>,
    ran: Vec<String>,
}
impl Fake {
    fn new() -> Self {
        Self {
            events: Vec::new(),
            head_tree: TREE.into(),
            dirty: false,
            fingerprint: json!({"runner":"fixture"}),
            red: Vec::new(),
            no_summary: Vec::new(),
            interrupt_at: None,
            ran: Vec::new(),
        }
    }
}
impl Services for Fake {
    fn fetch(&mut self) -> io::Result<()> {
        self.events.push("fetch".into());
        Ok(())
    }
    fn rev_parse(&mut self, revision: &str) -> io::Result<String> {
        Ok(match revision {
            "origin/rolling" => TIP.into(),
            "origin/rolling^{tree}" => TREE.into(),
            "HEAD^{tree}" => self.head_tree.clone(),
            other => panic!("unexpected revision {other}"),
        })
    }
    fn tracked_dirty(&mut self) -> io::Result<bool> {
        Ok(self.dirty)
    }
    fn fingerprint(&mut self, _: &str, shard: &str, jobs: u32) -> io::Result<Value> {
        let mut value = self.fingerprint.clone();
        value["probe"] = json!(format!("{shard}/{jobs}"));
        Ok(value)
    }
    fn shard_names(&mut self) -> io::Result<Vec<String>> {
        Ok(SHARDS.iter().map(|s| (*s).to_owned()).collect())
    }
    fn integration_targets(&mut self) -> io::Result<Vec<String>> {
        Ok(vec!["b_test".into(), "a_test".into()])
    }
    fn signature_matches(&mut self, rows: &[Value]) -> Value {
        let mut found = serde_json::Map::new();
        for row in rows {
            if row["exit"] != 0 {
                found.insert(
                    row["label"].as_str().unwrap().into(),
                    json!({"t": {"state":"new"}}),
                );
            }
        }
        Value::Object(found)
    }
    fn closed_issues(&mut self, since: Option<&str>) -> io::Result<Value> {
        Ok(json!([{"since": since}]))
    }
    fn host_name(&self) -> String {
        "fixture-host".into()
    }
    fn now(&self) -> String {
        "2026-10-22T00:00:00.000000+00:00".into()
    }
    fn say(&mut self, text: &str) {
        self.events.push(format!("say:{text}"));
    }
    fn warn(&mut self, text: &str) {
        self.events.push(format!("warn:{text}"));
    }
}
impl Backend for Fake {
    fn verify_source(&mut self, state: &State) -> io::Result<()> {
        if self.head_tree != state.tree {
            return Err(fail("source moved"));
        }
        self.events.push("verify".into());
        Ok(())
    }
    fn run_lane(&mut self, lane: &Lane, _jobs: u32) -> io::Result<Value> {
        let label = lane.label();
        if self.interrupt_at == Some(label) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "signal"));
        }
        self.ran.push(label.to_owned());
        let code = i32::from(self.red.contains(&label));
        let output = if self.no_summary.contains(&label) {
            ""
        } else {
            SUMMARY
        };
        let environment = SHARDS
            .contains(&label)
            .then(|| rolling_qa::shard_environment("disk").unwrap());
        rolling_qa::result(
            label,
            code,
            1.0,
            &format!("target/run/{label}.log"),
            output,
            environment,
        )
    }
}

struct Workspace {
    _temp: tempfile::TempDir,
    repo: PathBuf,
    directory: PathBuf,
}
fn workspace() -> Workspace {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().canonicalize().unwrap();
    let directory = repo.join("target/run");
    Workspace {
        _temp: temp,
        repo,
        directory,
    }
}
fn args(resume: bool) -> Args {
    Args {
        evidence_dir: PathBuf::from("target/run"),
        jobs: 2,
        closed_since: Some("2026-10-01T00:00:00Z".into()),
        resume,
    }
}
fn read(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn arguments_follow_the_legacy_parser() {
    let parse = |items: &[&str]| {
        let items: Vec<String> = items.iter().map(|s| (*s).to_owned()).collect();
        let mut err = Vec::new();
        let parsed = parse_args(&items, &mut err);
        (parsed, String::from_utf8(err).unwrap())
    };
    let (parsed, _) = parse(&["--evidence-dir", "target/x"]);
    assert_eq!(
        parsed.unwrap(),
        Args {
            evidence_dir: "target/x".into(),
            jobs: 4,
            closed_since: None,
            resume: false
        }
    );
    let (parsed, _) = parse(&[
        "--evidence-dir=target/x",
        "--jobs",
        "1",
        "--resume",
        "--closed-since",
        "2026-01-01T00:00:00Z",
    ]);
    let parsed = parsed.unwrap();
    assert!(parsed.resume && parsed.jobs == 1);
    assert_eq!(parsed.closed_since.as_deref(), Some("2026-01-01T00:00:00Z"));
    for (items, message) in [
        (
            &["--jobs", "2"][..],
            "the following arguments are required: --evidence-dir",
        ),
        (
            &["--evidence-dir", "t", "--jobs", "5"],
            "--jobs must be 1..4",
        ),
        (
            &["--evidence-dir", "t", "--jobs", "0"],
            "--jobs must be 1..4",
        ),
        (
            &["--evidence-dir", "t", "--jobs", "x"],
            "argument --jobs: invalid int value: 'x'",
        ),
    ] {
        let (parsed, err) = parse(items);
        assert_eq!(parsed.unwrap_err(), 2, "{items:?}");
        assert!(err.contains(message), "{err}");
    }
}

#[test]
fn a_complete_green_sweep_writes_state_and_both_reports_in_lane_order() {
    let w = workspace();
    let mut fake = Fake::new();
    let code = orchestrate(&args(false), &w.repo, &w.directory, &mut fake).unwrap();
    assert_eq!(code, 0, "{:?}", fake.events);
    assert_eq!(fake.ran.len(), 22);
    assert_eq!(fake.ran[0], "static");
    assert_eq!(
        &fake.ran[17..],
        ["integrations", "bins", "rsid-doc", "rsi", "rsi-common"]
    );
    let state = read(&w.directory.join("state.json"));
    assert_eq!(state["tip"], TIP);
    assert_eq!(state["results"].as_array().unwrap().len(), 22);
    assert_eq!(state["fingerprint"]["probe"], "store-01/2");
    let sweep = read(&w.directory.join("sweep-report.json"));
    assert_eq!(sweep["complete"], true);
    assert_eq!(sweep["swept_sha"], TIP);
    assert_eq!(sweep["provenance"]["runner"], RUNNER);
    assert_eq!(sweep["provenance"]["host"], "fixture-host");
    assert_eq!(
        sweep["provenance"]["report_ref"],
        "target/run/sweep-report.json"
    );
    assert_eq!(sweep["shards"].as_array().unwrap().len(), 16);
    assert_eq!(sweep["lanes"].as_array().unwrap().len(), 5);
    assert_eq!(
        sweep["shards"][0]["fingerprint"]["probe"],
        format!("{}/2", SHARDS[0])
    );
    let report = read(&w.directory.join("report.json"));
    assert_eq!(report["tip_stable"], true);
    assert_eq!(report["closed_issues"][0]["since"], "2026-10-01T00:00:00Z");
    assert!(
        fake.events
            .contains(&"say:known=0 known?=0 new=0".to_owned())
    );
    // The source is verified before every lane and once more at the end.
    assert_eq!(fake.events.iter().filter(|e| *e == "verify").count(), 23);
}

#[test]
fn a_red_static_gate_stops_before_any_shard_and_is_never_complete() {
    let w = workspace();
    let mut fake = Fake::new();
    fake.red = vec!["static"];
    let code = orchestrate(&args(false), &w.repo, &w.directory, &mut fake).unwrap();
    assert_eq!(code, 1);
    assert_eq!(fake.ran, ["static"]);
    assert!(
        fake.events
            .contains(&"warn:static gate failed; shard execution is unsafe".to_owned())
    );
    let sweep = read(&w.directory.join("sweep-report.json"));
    assert_eq!(sweep["complete"], false);
    assert!(sweep["shards"].as_array().unwrap().is_empty());
    assert!(
        fake.events
            .contains(&"say:known=0 known?=0 new=1".to_owned())
    );
}

#[test]
fn zero_exit_rows_without_required_counts_are_not_a_green_sweep() {
    let w = workspace();
    let mut fake = Fake::new();
    fake.no_summary = vec!["store-01"];
    let code = orchestrate(&args(false), &w.repo, &w.directory, &mut fake).unwrap();
    assert_eq!(code, 1);
    assert_eq!(
        read(&w.directory.join("sweep-report.json"))["complete"],
        false
    );
}

#[test]
fn an_interrupted_lane_is_not_recorded_and_resume_skips_only_completed_lanes() {
    let w = workspace();
    let mut fake = Fake::new();
    fake.interrupt_at = Some(SHARDS[2]);
    let error = orchestrate(&args(false), &w.repo, &w.directory, &mut fake).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    let state = read(&w.directory.join("state.json"));
    let labels: Vec<_> = state["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["label"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(labels, ["static", SHARDS[0], SHARDS[1]]);

    let mut resumed = Fake::new();
    let code = orchestrate(&args(true), &w.repo, &w.directory, &mut resumed).unwrap();
    assert_eq!(code, 0);
    assert_eq!(resumed.ran.len(), 19);
    assert_eq!(resumed.ran[0], SHARDS[2]);
}

#[test]
fn resume_refuses_changed_arguments_fingerprint_tree_or_tracked_changes() {
    let w = workspace();
    let mut first = Fake::new();
    first.interrupt_at = Some(SHARDS[0]);
    orchestrate(&args(false), &w.repo, &w.directory, &mut first).unwrap_err();
    let refused = |fake: &mut Fake, args: &Args, text: &str| {
        let error = orchestrate(args, &w.repo, &w.directory, fake).unwrap_err();
        assert!(error.to_string().contains(text), "{error}");
        assert!(fake.ran.is_empty());
    };
    let mut changed_jobs = args(true);
    changed_jobs.jobs = 3;
    refused(&mut Fake::new(), &changed_jobs, "resume arguments differ");
    let mut changed_since = args(true);
    changed_since.closed_since = None;
    refused(&mut Fake::new(), &changed_since, "resume arguments differ");
    let mut other = Fake::new();
    other.fingerprint = json!({"runner":"other"});
    refused(&mut other, &args(true), "fingerprint differs");
    let mut moved = Fake::new();
    moved.head_tree = "3333333333333333333333333333333333333333".into();
    refused(&mut moved, &args(true), "HEAD tree differs");
    let mut dirty = Fake::new();
    dirty.dirty = true;
    refused(&mut dirty, &args(true), "tracked worktree changes");
}

#[test]
fn a_fresh_sweep_refuses_a_moved_tree_dirty_tracked_files_or_an_existing_directory() {
    let w = workspace();
    let mut moved = Fake::new();
    moved.head_tree = "3333333333333333333333333333333333333333".into();
    let error = orchestrate(&args(false), &w.repo, &w.directory, &mut moved).unwrap_err();
    assert!(error.to_string().contains("integrate first"));
    assert!(!w.directory.exists());
    let mut dirty = Fake::new();
    dirty.dirty = true;
    let error = orchestrate(&args(false), &w.repo, &w.directory, &mut dirty).unwrap_err();
    assert!(error.to_string().contains("tracked worktree changes"));
    assert!(!w.directory.exists());
    fs::create_dir_all(&w.directory).unwrap();
    let error = orchestrate(&args(false), &w.repo, &w.directory, &mut Fake::new()).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
}

#[test]
fn the_shard_lanes_refuse_a_script_that_still_invokes_python() {
    let w = workspace();
    fs::create_dir_all(w.repo.join("scripts")).unwrap();
    fs::write(
        w.repo.join("scripts/run-rsid-test-shards.sh"),
        "#!/bin/bash\nshard_output=\"$(python3 check.py --list-shards)\"\n",
    )
    .unwrap();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let host = Host::new(
        &Ctx::new(&w.repo, &[]),
        w.repo.clone(),
        w.directory.clone(),
        "/nonexistent-runner".into(),
        Arc::new(AtomicUsize::new(0)),
        &mut out,
        &mut err,
    );
    let error = host.shard_script_is_python_free().unwrap_err();
    let text = error.to_string();
    assert!(text.starts_with("incomplete_contract: "), "{text}");
    assert!(text.contains("#362"), "{text}");
    fs::write(
        w.repo.join("scripts/run-rsid-test-shards.sh"),
        "#!/bin/bash\nexec scripts/repo-guard rsid-test-shards \"$@\"\n",
    )
    .unwrap();
    host.shard_script_is_python_free().unwrap();
}

#[test]
fn the_shard_lane_command_keeps_the_legacy_order_and_the_slow_tier_flag_last() {
    let dir = Path::new("/evidence/store-01");
    assert_eq!(
        shard_argv("store-01", dir, 3, true),
        [
            "scripts/run-rsid-test-shards.sh",
            "shard",
            "store-01",
            "--jobs",
            "3",
            "--evidence-dir",
            "/evidence/store-01",
            "--slow-tier"
        ]
    );
    assert_eq!(shard_argv("store-01", dir, 3, false).len(), 7);
}

#[cfg(unix)]
mod lane_commands {
    use super::*;

    fn host<'a>(
        w: &Workspace,
        cancel: Arc<AtomicUsize>,
        out: &'a mut Vec<u8>,
        err: &'a mut Vec<u8>,
    ) -> Host<'a> {
        let path = std::env::var("PATH").unwrap();
        Host::new(
            &Ctx::new(&w.repo, &[("PATH", &path), ("CARGO_BUILD_JOBS", "16")]),
            w.repo.clone(),
            w.directory.clone(),
            "/nonexistent-runner".into(),
            cancel,
            out,
            err,
        )
    }
    fn shell(script: &str) -> Vec<String> {
        vec!["sh".into(), "-c".into(), script.into()]
    }

    #[test]
    fn a_shard_command_runs_under_the_gate_spec_environment_and_logs_every_line() {
        let w = workspace();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut host = host(&w, Arc::new(AtomicUsize::new(0)), &mut out, &mut err);
        let script = "echo $CARGO_BUILD_JOBS $CARGO_PROFILE_DEV_DEBUG $CARGO_INCREMENTAL; printf tail; exit 3";
        for (shard, expected) in [(true, "4 line-tables-only 0\ntail"), (false, "16 0\ntail")] {
            let environment = rolling_qa::lane_environment(&host.environment, shard);
            let mut log = fs::File::create(w.repo.join("lane.log")).unwrap();
            let mut output = String::new();
            let code = host
                .command(
                    &shell(script),
                    &environment,
                    &mut log,
                    &mut output,
                    Duration::from_secs(30),
                )
                .unwrap();
            assert_eq!(code, 3);
            assert_eq!(output, expected);
            assert_eq!(
                fs::read_to_string(w.repo.join("lane.log")).unwrap(),
                expected
            );
        }
        drop(host);
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("4 line-tables-only 0\n")
        );
    }

    #[test]
    fn a_deadline_is_exit_124_a_signal_is_an_unrecorded_interruption_and_python_is_refused() {
        let w = workspace();
        let cancel = Arc::new(AtomicUsize::new(0));
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut host = host(&w, cancel.clone(), &mut out, &mut err);
        let environment = host.environment.clone();
        let mut log = fs::File::create(w.repo.join("lane.log")).unwrap();
        let mut output = String::new();
        let code = host
            .command(
                &shell("sleep 30"),
                &environment,
                &mut log,
                &mut output,
                Duration::from_millis(300),
            )
            .unwrap();
        assert_eq!(code, 124);
        let refused = host
            .command(
                &["python3".into(), "-c".into(), "pass".into()],
                &environment,
                &mut log,
                &mut output,
                Duration::from_secs(5),
            )
            .unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied);
        cancel.store(15, std::sync::atomic::Ordering::Relaxed);
        let interrupted = host
            .command(
                &shell("sleep 30"),
                &environment,
                &mut log,
                &mut output,
                Duration::from_secs(30),
            )
            .unwrap_err();
        assert_eq!(interrupted.kind(), io::ErrorKind::Interrupted);
        assert!(interrupted.to_string().contains("not recorded"));
    }
}
