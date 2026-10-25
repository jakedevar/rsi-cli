//! The `cloud_sweep` job's `build` lane (#3844): the verdict of one Python-free
//! cloud build, classified here in Rust from the exit codes
//! `scripts/cloud-build-remote.sh` printed (`BUILD_STEP <step> <code>`) and the
//! spend ledger lines `scripts/cloud-gate.sh` wrote. Nothing in the log is
//! trusted beyond that grammar: the remote tails are `| `-prefixed, a marker
//! must be an exact whole line, and only lines before the first tail count.
//!
//! GREEN needs every step at 0, the gate's own exit 0 (its EXIT trap destroyed
//! the host and found no leftover) and a clean ledger closing line. RED names
//! the failing step (`build` or `test`). Everything else is INCOMPLETE, never
//! RED: a spot interruption, a failed setup step (`checkout`, `toolchain`), a
//! missing prerequisite, a refused reservation, a missing marker.

use super::{AgentJobResultV1, JobState, filtered_log_tail};
use rsi_common::agent_jobs::{
    CloudBuildReceiptV1, CloudBuildStepV1, CloudSweepResultV1, SweepVerdict,
};
use std::path::{Path, PathBuf};

/// The steps `cloud-build-remote.sh` runs, in order.
const STEPS: [&str; 4] = ["checkout", "toolchain", "build", "test"];
/// Setup steps: their failure is the host's or network's, not the commit's.
const SETUP_STEPS: [&str; 2] = ["checkout", "toolchain"];

/// Where `cloud-build.sh` keeps one run's output (`~/.rsi/cloud/results/build-<sha>`).
pub(super) fn results_dir(sha: &str) -> PathBuf {
    std::env::var_os("HOME")
        .map_or_else(|| PathBuf::from("."), PathBuf::from)
        .join(".rsi/cloud/results")
        .join(format!("build-{sha}"))
}

/// The spend ledger `cloud-gate.sh` appends to (`~/.rsi/cloud/spend.md`), when
/// readable and of a sane size.
pub(super) fn read_ledger() -> Option<String> {
    let path = std::env::var_os("HOME")
        .map_or_else(|| PathBuf::from("."), PathBuf::from)
        .join(".rsi/cloud/spend.md");
    let len = std::fs::metadata(&path).ok()?.len();
    (len <= 8 * 1024 * 1024)
        .then(|| std::fs::read_to_string(path).ok())
        .flatten()
}

/// Steps and the done marker, parsed from the lines before the first tail.
#[derive(Debug, PartialEq, Eq)]
struct Steps {
    steps: Vec<(String, String)>,
}

impl Steps {
    fn exit(&self, step: &str) -> Option<&str> {
        self.steps
            .iter()
            .find(|(name, _)| name == step)
            .map(|(_, exit)| exit.as_str())
    }
}

fn is_exit_word(word: &str) -> bool {
    word == "skipped"
        || (!word.is_empty() && word.len() <= 3 && word.bytes().all(|b| b.is_ascii_digit()))
}

/// The four step results and the `BUILD_STEPS_DONE <sha>` marker, or `None`
/// when the run is not complete for exactly this SHA (a missing, duplicated,
/// unknown or malformed marker; a marker for another SHA).
fn parse_steps(log: &str, sha: &str) -> Option<Steps> {
    let mut steps: Vec<(String, String)> = Vec::new();
    let mut done = false;
    for line in log
        .lines()
        .map(|line| line.trim_end_matches(['\r', ' ', '\t']))
    {
        if line.starts_with("BUILD_TAIL_BEGIN ") {
            break;
        }
        if let Some(rest) = line.strip_prefix("BUILD_STEP ") {
            let (name, exit) = rest.split_once(' ')?;
            if !STEPS.contains(&name) || !is_exit_word(exit) || steps.iter().any(|(n, _)| n == name)
            {
                return None;
            }
            steps.push((name.to_string(), exit.to_string()));
        }
    }
    // The done marker follows the tails; it is a whole line naming this SHA.
    for line in log
        .lines()
        .map(|line| line.trim_end_matches(['\r', ' ', '\t']))
    {
        if let Some(marker) = line.strip_prefix("BUILD_STEPS_DONE ") {
            done = marker == sha;
        }
    }
    (done
        && STEPS
            .iter()
            .all(|step| steps.iter().any(|(n, _)| n == step)))
    .then_some(Steps { steps })
}

/// The instance id `cloud-build.sh hosted` printed (`BUILD_HOST instance=<id> sha=<sha>`).
fn host_instance(log: &str, sha: &str) -> Option<String> {
    log.lines().find_map(|line| {
        let rest = line.trim_end().strip_prefix("BUILD_HOST instance=")?;
        let (id, tail) = rest.split_once(' ')?;
        (tail == format!("sha={sha}")
            && !id.is_empty()
            && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
        .then(|| id.to_string())
    })
}

/// The text between `start` and the first `end` after it.
fn between<'a>(text: &'a str, start: &str, end: &[char]) -> Option<&'a str> {
    let from = text.find(start)? + start.len();
    let rest = &text[from..];
    Some(&rest[..rest.find(end).unwrap_or(rest.len())])
}

/// What the ledger says about one host: the reservation, the closing line.
pub(super) fn ledger_receipt(ledger: &str, instance_id: &str) -> CloudBuildReceiptV1 {
    let mut receipt = CloudBuildReceiptV1 {
        instance_id: Some(instance_id.to_string()),
        ..CloudBuildReceiptV1::default()
    };
    let start = format!("Gate window {instance_id}: start ");
    let stop = format!("Gate window {instance_id}: stop ");
    let mut reservation = None;
    for line in ledger.lines() {
        if line.starts_with(&start) {
            receipt.instance_type = line
                .split(", ")
                .find_map(|part| part.strip_suffix(" spot"))
                .map(str::to_string);
            reservation = line
                .rsplit_once("reservation ")
                .map(|(_, id)| id.trim_end_matches(['.', ')']).to_string());
        } else if line.starts_with(&stop) {
            receipt.elapsed_secs = between(line, ", elapsed ", &[' ']).and_then(|s| s.parse().ok());
            receipt.actual_usd = between(line, "est compute $", &[' ']).map(str::to_string);
            receipt.destroy = between(line, "terraform destroy ", &[';']).map(str::to_string);
            receipt.leftover =
                between(line, "leftover gate instances: ", &[';']).map(str::to_string);
        }
    }
    if let Some(id) = reservation {
        let prefix = format!("Gate reservation {id}: reserved $");
        receipt.reserved_usd = ledger
            .lines()
            .find_map(|line| line.strip_prefix(&prefix))
            .and_then(|rest| rest.split(' ').next())
            .map(str::to_string);
    }
    receipt
}

/// A refusal code for a run that never reached its markers: the first line the
/// scripts print for a known early stop.
fn early_refusal(log: &str) -> String {
    for line in log.lines() {
        let line = line.trim_end();
        if let Some(what) = line
            .strip_prefix("cloud-build: missing ")
            .or_else(|| line.strip_prefix("cloud-gate: missing "))
        {
            let slug: String = what
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() {
                        c.to_ascii_lowercase()
                    } else {
                        '_'
                    }
                })
                .take(48)
                .collect();
            return format!("cloud_prereq_missing_{}", slug.trim_matches('_'));
        }
    }
    [
        ("refusing to start a remote run", "cloud_spend_refused"),
        ("another gate host is running", "cloud_gate_busy"),
        (
            "requested SHA is not the current rolling tip",
            "sweep_sha_not_rolling_tip",
        ),
    ]
    .iter()
    .find_map(|(needle, code)| log.contains(needle).then(|| (*code).to_string()))
    .unwrap_or_else(|| "cloud_build_incomplete".into())
}

fn build_detail(
    receipt: &CloudBuildReceiptV1,
    wall_secs: Option<i64>,
    incomplete_tail: Option<String>,
) -> String {
    let steps = receipt
        .steps
        .iter()
        .map(|s| format!("{}:{}", s.step, s.exit))
        .collect::<Vec<_>>()
        .join(",");
    let mut text = format!(
        "lane=build; steps={}",
        if steps.is_empty() { "none" } else { &steps }
    );
    if let Some(step) = &receipt.failing_step {
        text.push_str(&format!("; failing_step={step}"));
    }
    if let Some(kind) = &receipt.instance_type {
        text.push_str(&format!("; instance_type={kind}"));
    }
    if let Some(secs) = receipt.elapsed_secs {
        text.push_str(&format!("; host_secs={secs}"));
    }
    if let Some(usd) = &receipt.reserved_usd {
        text.push_str(&format!("; reserved_usd={usd}"));
    }
    if let Some(usd) = &receipt.actual_usd {
        text.push_str(&format!("; actual_usd={usd}"));
    }
    if let Some(leftover) = &receipt.leftover {
        text.push_str(&format!("; leftover={leftover}"));
    }
    if let Some(destroy) = &receipt.destroy {
        text.push_str(&format!("; destroy={destroy}"));
    }
    if let Some(secs) = wall_secs {
        text.push_str(&format!("; wall={}m{:02}s", secs / 60, secs % 60));
    }
    if let Some(tail) = incomplete_tail.filter(|tail| !tail.is_empty()) {
        text.push_str(&format!("; log_tail={tail}"));
    }
    text
}

/// The typed outcome of a finished `cloud_sweep` job in the `build` lane.
pub(super) fn classify(
    sha: &str,
    exit_code: Option<i32>,
    log: &str,
    ledger: Option<&str>,
    mut result: AgentJobResultV1,
    dir: &Path,
    wall_secs: Option<i64>,
) -> (JobState, AgentJobResultV1) {
    let mut receipt = host_instance(log, sha)
        .zip(ledger)
        .map_or_else(CloudBuildReceiptV1::default, |(id, text)| {
            ledger_receipt(text, &id)
        });
    if receipt.instance_id.is_none() {
        receipt.instance_id = host_instance(log, sha);
    }
    let spot = exit_code == Some(75)
        || log
            .lines()
            .any(|l| l.trim_end() == "cloud-gate: spot_interrupted");
    let parsed = parse_steps(log, sha);
    if let Some(parsed) = &parsed {
        receipt.steps = parsed
            .steps
            .iter()
            .map(|(step, exit)| CloudBuildStepV1 {
                step: step.clone(),
                exit: exit.clone(),
            })
            .collect();
    }
    let failed = |parsed: &Steps, step: &str| {
        parsed
            .exit(step)
            .is_some_and(|e| e != "0" && e != "skipped")
    };
    let verdict = if spot {
        // AWS took the host back: no verdict, safe to rerun.
        result.refusal = Some("spot_interrupted".into());
        SweepVerdict::Incomplete
    } else if let Some(parsed) = &parsed {
        if let Some(step) = SETUP_STEPS.iter().find(|s| failed(parsed, s)) {
            receipt.failing_step = Some((*step).to_string());
            result.refusal = Some("cloud_build_setup_failed".into());
            SweepVerdict::Incomplete
        } else if let Some(step) = ["build", "test"].iter().find(|s| failed(parsed, s)) {
            receipt.failing_step = Some((*step).to_string());
            SweepVerdict::Red
        } else if STEPS.iter().any(|step| parsed.exit(step) != Some("0")) {
            // Skipped steps describe work after a failure, never success.
            result.refusal = Some("cloud_build_incomplete".into());
            SweepVerdict::Incomplete
        } else if exit_code != Some(0) {
            result.refusal = Some("cloud_build_exit_nonzero".into());
            SweepVerdict::Incomplete
        } else if receipt.destroy.as_deref() != Some("ok")
            || !receipt
                .leftover
                .as_deref()
                .is_some_and(|l| l.eq_ignore_ascii_case("none"))
        {
            // Both fields must be present on this host's ledger closing line.
            // An unreadable ledger or a missing closing line proves nothing.
            result.refusal = Some("cloud_build_cleanup_unverified".into());
            SweepVerdict::Incomplete
        } else {
            SweepVerdict::Green
        }
    } else {
        result.refusal = Some(early_refusal(log));
        SweepVerdict::Incomplete
    };
    let tail = (verdict == SweepVerdict::Incomplete).then(|| filtered_log_tail(log));
    result.failing_tests = Vec::new();
    result.failing_tests_omitted = 0;
    result.detail = Some(build_detail(&receipt, wall_secs, tail));
    result.sweep = Some(CloudSweepResultV1 {
        verdict,
        sha: sha.to_string(),
        results_dir: dir.display().to_string(),
        new_failures: Vec::new(),
        known_failures: Vec::new(),
        build: Some(receipt),
    });
    let state = if verdict == SweepVerdict::Green {
        JobState::Succeeded
    } else {
        JobState::Failed
    };
    (state, result)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
    const LEDGER: &str = "\
Gate reservation res-1-2: reserved $0.7813 until 2030-01-01T01:00:00Z (start 2030-01-01T00:00:00Z, 60 min at $0.7813/h worst case).
Gate window i-0abc: start 2030-01-01T00:01:00Z, c7i.8xlarge spot, public IPv4 192.0.2.1, est $0.7813/h (apply began 00:01:00Z, lifetime 60 min, reservation res-1-2).
Gate window i-0abc: stop 2030-01-01T00:31:00Z, elapsed 1800 s = 0.5000 h, est compute $0.3907 at $0.7813/h; terraform destroy ok; leftover gate instances: none; sccache: off (cold run); reservation res-1-2.
";

    fn log(build: &str, test: &str, extra: &str) -> String {
        format!(
            "cloud-gate: i-0abc (c7i.8xlarge) at 192.0.2.1\nBUILD_HOST instance=i-0abc sha={SHA}\n\
BUILD_STEP checkout 0\nBUILD_STEP toolchain 0\nBUILD_STEP build {build}\nBUILD_STEP test {test}\n\
BUILD_TEST_RUNNER nextest\nBUILD_TAIL_BEGIN build\n| compiling\nBUILD_TAIL_END build\n\
BUILD_STEPS_DONE {SHA}\n{extra}"
        )
    }

    fn run(log: &str, exit: Option<i32>, ledger: Option<&str>) -> (JobState, AgentJobResultV1) {
        classify(
            SHA,
            exit,
            log,
            ledger,
            AgentJobResultV1::default(),
            Path::new("/r"),
            Some(125),
        )
    }

    fn sweep(result: &AgentJobResultV1) -> &CloudSweepResultV1 {
        result.sweep.as_ref().expect("a sweep result")
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn all_steps_zero_and_a_clean_destroy_is_green_with_the_ledger_receipt() {
        let (state, result) = run(&log("0", "0", ""), Some(0), Some(LEDGER));
        assert_eq!(state, JobState::Succeeded);
        let sweep = sweep(&result);
        assert_eq!(sweep.verdict, SweepVerdict::Green);
        assert_eq!(result.refusal, None);
        let build = sweep.build.as_ref().unwrap();
        assert_eq!(build.instance_id.as_deref(), Some("i-0abc"));
        assert_eq!(build.instance_type.as_deref(), Some("c7i.8xlarge"));
        assert_eq!(build.elapsed_secs, Some(1800));
        assert_eq!(build.reserved_usd.as_deref(), Some("0.7813"));
        assert_eq!(build.actual_usd.as_deref(), Some("0.3907"));
        assert_eq!(build.destroy.as_deref(), Some("ok"));
        assert_eq!(build.leftover.as_deref(), Some("none"));
        assert_eq!(build.failing_step, None);
        assert_eq!(build.steps.len(), 4);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_failing_build_or_test_step_is_red_and_names_the_step() {
        let (state, result) = run(&log("101", "skipped", ""), Some(101), Some(LEDGER));
        assert_eq!(state, JobState::Failed);
        assert_eq!(sweep(&result).verdict, SweepVerdict::Red);
        assert_eq!(
            sweep(&result)
                .build
                .as_ref()
                .unwrap()
                .failing_step
                .as_deref(),
            Some("build")
        );
        let (_, result) = run(&log("0", "100", ""), Some(100), Some(LEDGER));
        assert_eq!(sweep(&result).verdict, SweepVerdict::Red);
        assert_eq!(
            sweep(&result)
                .build
                .as_ref()
                .unwrap()
                .failing_step
                .as_deref(),
            Some("test")
        );
        assert!(
            result
                .detail
                .as_deref()
                .unwrap()
                .contains("failing_step=test")
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_spot_interruption_is_incomplete_never_red_even_with_a_failed_step() {
        let reclaimed = log("101", "skipped", "cloud-gate: spot_interrupted\n");
        for exit in [Some(75), Some(101)] {
            let (state, result) = run(&reclaimed, exit, Some(LEDGER));
            assert_eq!(state, JobState::Failed);
            assert_eq!(sweep(&result).verdict, SweepVerdict::Incomplete);
            assert_eq!(result.refusal.as_deref(), Some("spot_interrupted"));
        }
        // Exit 75 alone (no step lines at all) is the same.
        let (_, result) = run("cloud-gate: spot_interrupted\n", Some(75), None);
        assert_eq!(sweep(&result).verdict, SweepVerdict::Incomplete);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_failed_setup_step_is_incomplete_not_red() {
        let mut text = log("skipped", "skipped", "");
        text = text.replace("BUILD_STEP toolchain 0", "BUILD_STEP toolchain 1");
        let (_, result) = run(&text, Some(1), Some(LEDGER));
        assert_eq!(sweep(&result).verdict, SweepVerdict::Incomplete);
        assert_eq!(result.refusal.as_deref(), Some("cloud_build_setup_failed"));
        assert_eq!(
            sweep(&result)
                .build
                .as_ref()
                .unwrap()
                .failing_step
                .as_deref(),
            Some("toolchain")
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_ledger_text_the_gate_really_writes_for_no_leftovers_is_green() {
        // `aws ... --output text` prints `None` for an empty list, and
        // cloud-gate.sh writes that word into the closing line.
        let real = LEDGER.replace("instances: none", "instances: None");
        let (state, result) = run(&log("0", "0", ""), Some(0), Some(&real));
        assert_eq!(state, JobState::Succeeded);
        assert_eq!(sweep(&result).verdict, SweepVerdict::Green);
        // An unreadable leftover check is not clean.
        let unknown = LEDGER.replace("instances: none", "instances: unknown");
        let (_, result) = run(&log("0", "0", ""), Some(0), Some(&unknown));
        assert_eq!(sweep(&result).verdict, SweepVerdict::Incomplete);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn green_needs_the_gates_own_clean_exit_and_a_clean_ledger() {
        // Every step passed but the gate exited nonzero (leftover instance or a
        // failed destroy): INCOMPLETE.
        let (_, result) = run(&log("0", "0", ""), Some(1), Some(LEDGER));
        assert_eq!(sweep(&result).verdict, SweepVerdict::Incomplete);
        assert_eq!(result.refusal.as_deref(), Some("cloud_build_exit_nonzero"));
        for dirty in [
            LEDGER.replace("destroy ok", "destroy FAILED"),
            LEDGER.replace("instances: none", "instances: i-0dead"),
        ] {
            let (_, result) = run(&log("0", "0", ""), Some(0), Some(&dirty));
            assert_eq!(sweep(&result).verdict, SweepVerdict::Incomplete);
            assert_eq!(
                result.refusal.as_deref(),
                Some("cloud_build_cleanup_unverified")
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_missing_or_unreadable_ledger_is_incomplete() {
        // read_ledger() returns None for both missing and unreadable files.
        let (state, result) = run(&log("0", "0", ""), Some(0), None);
        assert_eq!(state, JobState::Failed);
        assert_eq!(sweep(&result).verdict, SweepVerdict::Incomplete);
        assert_eq!(
            result.refusal.as_deref(),
            Some("cloud_build_cleanup_unverified")
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_ledger_without_this_hosts_closing_line_is_incomplete() {
        let no_close = LEDGER
            .lines()
            .filter(|line| !line.starts_with("Gate window i-0abc: stop "))
            .collect::<Vec<_>>()
            .join("\n");
        let other_close = LEDGER.replace("Gate window i-0abc: stop ", "Gate window i-0def: stop ");
        for ledger in ["", no_close.as_str(), other_close.as_str()] {
            let (state, result) = run(&log("0", "0", ""), Some(0), Some(ledger));
            assert_eq!(state, JobState::Failed, "{ledger}");
            assert_eq!(sweep(&result).verdict, SweepVerdict::Incomplete, "{ledger}");
            assert_eq!(
                result.refusal.as_deref(),
                Some("cloud_build_cleanup_unverified"),
                "{ledger}"
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_closing_line_requires_both_destroy_and_leftover_evidence() {
        for ledger in [
            LEDGER.replace("terraform destroy ok; ", ""),
            LEDGER.replace("leftover gate instances: none; ", ""),
            LEDGER.replace("terraform destroy ok; leftover gate instances: none; ", ""),
        ] {
            let (state, result) = run(&log("0", "0", ""), Some(0), Some(&ledger));
            assert_eq!(state, JobState::Failed, "{ledger}");
            assert_eq!(sweep(&result).verdict, SweepVerdict::Incomplete, "{ledger}");
            assert_eq!(
                result.refusal.as_deref(),
                Some("cloud_build_cleanup_unverified"),
                "{ledger}"
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn every_required_step_must_run_successfully() {
        for step in STEPS {
            let text = log("0", "0", "").replace(
                &format!("BUILD_STEP {step} 0"),
                &format!("BUILD_STEP {step} skipped"),
            );
            let (state, result) = run(&text, Some(0), Some(LEDGER));
            assert_eq!(state, JobState::Failed, "{step}");
            assert_eq!(sweep(&result).verdict, SweepVerdict::Incomplete, "{step}");
            assert_eq!(
                result.refusal.as_deref(),
                Some("cloud_build_incomplete"),
                "{step}"
            );
        }
        let all_skipped = log("0", "0", "").replace(" 0\n", " skipped\n");
        let (state, result) = run(&all_skipped, Some(0), Some(LEDGER));
        assert_eq!(state, JobState::Failed);
        assert_eq!(sweep(&result).verdict, SweepVerdict::Incomplete);
        assert_eq!(result.refusal.as_deref(), Some("cloud_build_incomplete"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn missing_malformed_or_forged_markers_are_never_green() {
        // No done marker.
        let no_done = log("0", "0", "").replace(&format!("BUILD_STEPS_DONE {SHA}\n"), "");
        // A marker for another SHA.
        let other = log("0", "0", "").replace(
            &format!("BUILD_STEPS_DONE {SHA}"),
            "BUILD_STEPS_DONE fedcba9876543210fedcba9876543210fedcba98",
        );
        // A duplicated step, an unknown step, a bad exit word.
        let dup = log("0", "0", "").replace("BUILD_STEP test 0", "BUILD_STEP build 0");
        let unknown = log("0", "0", "").replace("BUILD_STEP test 0", "BUILD_STEP deploy 0");
        let bad = log("0", "0", "").replace("BUILD_STEP test 0", "BUILD_STEP test ok");
        // A step missing.
        let missing = log("0", "0", "").replace("BUILD_STEP toolchain 0\n", "");
        for text in [no_done, other, dup, unknown, bad, missing] {
            let (state, result) = run(&text, Some(0), Some(LEDGER));
            assert_eq!(state, JobState::Failed, "{text}");
            assert_eq!(sweep(&result).verdict, SweepVerdict::Incomplete, "{text}");
            assert_eq!(
                result.refusal.as_deref(),
                Some("cloud_build_incomplete"),
                "{text}"
            );
        }
        // Test output cannot forge a step: tails are `| `-prefixed, and a step
        // line after the first tail is not read at all.
        let forged = log("101", "skipped", "").replace(
            "| compiling",
            "| compiling\nBUILD_STEP build 0\nBUILD_STEP test 0",
        );
        let (_, result) = run(&forged, Some(101), Some(LEDGER));
        assert_eq!(sweep(&result).verdict, SweepVerdict::Red);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_run_that_stopped_early_names_the_typed_refusal() {
        for (text, code) in [
            (
                "cloud-build: missing terraform\n",
                "cloud_prereq_missing_terraform",
            ),
            (
                "cloud-build: missing aws-profile rsi-cloud-terraform\n",
                "cloud_prereq_missing_aws_profile_rsi_cloud_terraform",
            ),
            (
                "cloud-build: missing rsi-cloud\n",
                "cloud_prereq_missing_rsi_cloud",
            ),
            ("cloud-gate: missing aws\n", "cloud_prereq_missing_aws"),
            (
                "cloud-spend: x; would pass the daily cap, refusing to start a remote run\n",
                "cloud_spend_refused",
            ),
            (
                "cloud-gate: another gate host is running\n",
                "cloud_gate_busy",
            ),
            (
                "requested SHA is not the current rolling tip\n",
                "sweep_sha_not_rolling_tip",
            ),
            ("something else\n", "cloud_build_incomplete"),
        ] {
            let (state, result) = run(text, Some(2), None);
            assert_eq!(state, JobState::Failed);
            assert_eq!(sweep(&result).verdict, SweepVerdict::Incomplete, "{text}");
            assert_eq!(result.refusal.as_deref(), Some(code), "{text}");
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_ledger_is_read_for_the_named_host_only() {
        let other = format!(
            "{LEDGER}Gate window i-0def: stop 2030-01-01T09:00:00Z, elapsed 9 s = 0.0025 h, est compute $9.9999 at $0.7813/h; terraform destroy ok; leftover gate instances: none; x; reservation res-9-9.\n"
        );
        let receipt = ledger_receipt(&other, "i-0abc");
        assert_eq!(receipt.actual_usd.as_deref(), Some("0.3907"));
        assert_eq!(ledger_receipt(&other, "i-0nope").actual_usd, None);
        assert_eq!(ledger_receipt(&other, "i-0nope").reserved_usd, None);
    }
}
