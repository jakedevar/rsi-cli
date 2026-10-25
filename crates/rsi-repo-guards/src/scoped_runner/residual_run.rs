//! Execution of the selected, ported residual suites (#366).
//!
//! A ported suite is a bash script `test_<stem>.sh` that speaks a small TAP
//! subset on stdout: one `ok N - name` or `not ok N - name` line per test case
//! (see `scripts/tests/lib/tap.sh`). The runner owns the verdict, not the
//! script's say-so: the suite is red when it exits non-zero, when any case
//! reports `not ok`, when it times out, and when it executes zero cases (an
//! exit 0 with no TAP line is no evidence). A host without bash is the typed
//! `runner_unavailable`; nothing substitutes for it.
//!
//! Every suite runs as an owned child in its own process group, under the
//! shared PID isolation, with the run's cancel flag and a hard deadline, in the
//! candidate checkout. Entries the selector marked `incomplete_contract` (a
//! suite that is still a Python file) pass through untouched and stay red.
use super::process;
use crate::{desktop_execution, pid_isolation::Isolation};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::AtomicUsize;
use std::time::{Duration, Instant};

pub const RUNNER: &str = "bash";

/// What the TAP output of one suite holds: the cases (`ok` / `not ok`), the plan and every
/// protocol violation. The judge trusts none of it: it counts only what the output proves.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Tally {
    pub executed: usize,
    pub failed: usize,
    /// Cases the suite reported as skipped (`ok N - name # SKIP reason`): not executed,
    /// not failed, and counted separately so they are never mistaken for evidence.
    pub skipped: usize,
    /// The `1..N` plan the suite printed, when it printed one.
    pub plan: Option<usize>,
    /// Malformed or incomplete TAP: a second plan, a case out of sequence, `Bail out!`,
    /// an unnumbered `not ok`. Any one is red, whatever the exit code says.
    pub violations: Vec<String>,
    /// The number the next numbered case line must carry.
    next: usize,
}
/// Whether a case line's text carries a TAP `SKIP` directive: a `#`, optional whitespace, then
/// `skip` in ANY letter case (TAP directives are case-insensitive; `# Skip` and `# sKiP` skip as
/// surely as `# SKIP`). Matching two exact spellings let a skip-only stream count as executed
/// evidence (#483). Over-matching only ever discards evidence, which is the safe direction.
fn has_skip_directive(rest: &str) -> bool {
    rest.match_indices('#').any(|(at, _)| {
        let after = rest[at + 1..].trim_start();
        after
            .get(..4)
            .is_some_and(|word| word.eq_ignore_ascii_case("skip"))
    })
}

impl Tally {
    pub fn observe(&mut self, line: &str) {
        if line.starts_with("Bail out!") {
            self.violations.push(format!("bail out: {line}"));
            return;
        }
        if let Some(plan) = line.strip_prefix("1..") {
            let digits = plan.chars().take_while(char::is_ascii_digit).count();
            let tail = &plan[digits..];
            if digits > 0 && (tail.is_empty() || tail.starts_with([' ', '\t', '#'])) {
                match plan[..digits].parse() {
                    Ok(count) if self.plan.is_none() => self.plan = Some(count),
                    _ => self
                        .violations
                        .push(format!("a second or unreadable plan: {line}")),
                }
                return;
            }
        }
        let (failed, rest) = match line.strip_prefix("not ok") {
            Some(rest) if rest.is_empty() || rest.starts_with([' ', '\t']) => {
                (true, rest.trim_start())
            }
            Some(_) => return,
            None => match line.strip_prefix("ok ") {
                Some(rest) => (false, rest),
                None => return,
            },
        };
        // `ok 3 - name` or `ok 3`; anything else (`ok to proceed`) is not a case.
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        let numbered =
            digits > 0 && matches!(rest[digits..].chars().next(), None | Some(' ') | Some('\t'));
        if !numbered {
            if failed {
                // A failure the suite did not number is still a failure.
                self.failed += 1;
                self.violations
                    .push(format!("an unnumbered failure: {line}"));
            }
            return;
        }
        let number: usize = rest[..digits].parse().unwrap_or(usize::MAX);
        self.next += 1;
        if number != self.next {
            self.violations.push(format!(
                "case numbered {number} where {} was expected: {line}",
                self.next
            ));
        }
        // A `# SKIP` case is not an executed test: skipping is not passing.
        if !failed && has_skip_directive(rest) {
            self.skipped += 1;
            return;
        }
        self.executed += 1;
        if failed {
            self.failed += 1;
        }
    }

    /// Why the plan line contradicts the case lines, once the output is complete.
    pub fn plan_problem(&self) -> Option<String> {
        match self.plan {
            None => Some("no `1..N` plan line: the suite did not run to its end".into()),
            Some(plan) if plan != self.executed + self.skipped => Some(format!(
                "plan 1..{plan} but {} case lines (executed {}, skipped {})",
                self.executed + self.skipped,
                self.executed,
                self.skipped
            )),
            Some(_) => None,
        }
    }
}

/// The receipt entry for a selected shell suite before it runs (the dry-run shape).
pub fn planned(test: &str) -> Value {
    json!({"lane":"residual","test_file":test,"status":"planned","completed":false,"runner":RUNNER,"executed_tests":null})
}

fn missing_runner(test: &str) -> Value {
    json!({
        "lane":"residual","test_file":test,"status":"runner_unavailable","completed":false,
        "runner":RUNNER,"executed_tests":0,
        "reason":"bash is not installed (the ported residual suites are bash scripts); nothing substitutes for it",
    })
}

/// Whether an entry is a ported suite the runner executes (not a red contract gap).
fn is_shell(entry: &Value) -> bool {
    entry["status"] == "planned" && entry["runner"] == RUNNER
}

/// Everything one residual execution needs.
pub struct Run<'a> {
    pub work: &'a Path,
    pub env: &'a BTreeMap<String, String>,
    pub isolation: &'a Isolation,
    pub cancel: &'a AtomicUsize,
    /// Per-suite ceiling.
    pub per_suite: Duration,
    /// The run's remaining total budget (`None`: unbounded beyond `per_suite`).
    pub total_left: Option<Duration>,
    pub log_dir: &'a Path,
}

/// Run (or, on a dry run, preview) the selected suites; one entry per suite, in order.
/// The first red suite stops the lane, as the Python phase stopped at its first failure.
pub fn execute(
    entries: Vec<Value>,
    dry_run: bool,
    run: &Run<'_>,
    say: &mut dyn FnMut(&str),
) -> io::Result<Vec<Value>> {
    let started = Instant::now();
    let mut done: Vec<Value> = Vec::with_capacity(entries.len());
    let bash = desktop_execution::executable(RUNNER, run.env, run.work);
    for (index, entry) in entries.into_iter().enumerate() {
        if !is_shell(&entry) {
            done.push(entry);
            continue;
        }
        let test = entry["test_file"].as_str().unwrap_or_default().to_string();
        say(&format!("residual suite: {RUNNER} {test}"));
        let Some(bash) = bash.as_ref() else {
            say(&format!(
                "scoped-test: runner_unavailable runner={RUNNER} executed_tests=0 test_file={test}"
            ));
            done.push(missing_runner(&test));
            if dry_run {
                continue;
            }
            return Ok(done);
        };
        if dry_run {
            done.push(entry);
            continue;
        }
        let budget = match run.total_left {
            Some(total) => run.per_suite.min(total.saturating_sub(started.elapsed())),
            None => run.per_suite,
        };
        let Some(deadline) = Instant::now()
            .checked_add(budget)
            .filter(|_| !budget.is_zero())
        else {
            say("scoped_test_timeout: total budget exhausted before the residual suites");
            done.push(json!({
                "lane":"residual","test_file":test,"status":"timed_out","completed":false,
                "runner":RUNNER,"executed_tests":null,
                "reason":"total budget exhausted before the residual suites",
            }));
            return Ok(done);
        };
        let log_path = run.log_dir.join(format!("residual-{index}.log"));
        let mut log = std::fs::File::create(&log_path)?;
        let argv = run
            .isolation
            .wrap(&[bash.to_string_lossy().into_owned(), test.clone()]);
        let mut tally = Tally::default();
        let mut write_error = None;
        let outcome = process::stream(
            &argv,
            run.work,
            run.env,
            deadline,
            run.cancel,
            |line| {
                tally.observe(line.trim_end_matches(['\n', '\r']));
                say(line.trim_end_matches('\n'));
                if let Err(error) = log.write_all(line.as_bytes()) {
                    write_error = Some(error);
                }
            },
            |_| {},
        )?;
        if let Some(error) = write_error {
            return Err(error);
        }
        if outcome.cancelled.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "residual lane cancelled",
            ));
        }
        let (status, reason) = if outcome.timed_out {
            say(&format!("scoped_test_timeout: {RUNNER} {test}"));
            (
                "timed_out",
                Some("the suite outran its deadline and was killed"),
            )
        } else if outcome.code != 0 || tally.failed > 0 {
            ("failed", None)
        } else if let Some(violation) = tally.violations.first() {
            say(&format!(
                "scoped-test: malformed TAP from {test}: {violation}"
            ));
            (
                "failed",
                Some("the suite's TAP output is malformed (see the log)"),
            )
        } else if tally.executed == 0 {
            // Skipping is not passing: a suite whose every case skipped proved nothing here.
            say(&format!(
                "scoped-test: residual suite executed 0 tests (skipped {}): {test}",
                tally.skipped
            ));
            (
                "no_tests_executed",
                Some(
                    "no executed (non-SKIP) case: an all-skipped or case-less suite is no evidence",
                ),
            )
        } else if let Some(problem) = tally.plan_problem() {
            say(&format!(
                "scoped-test: incomplete TAP from {test}: {problem}"
            ));
            (
                "failed",
                Some("the TAP plan contradicts the case lines (see the log)"),
            )
        } else if !outcome.output_complete {
            (
                "failed",
                Some("the suite's output could not be read completely"),
            )
        } else {
            ("passed", None)
        };
        let mut result = json!({
            "lane":"residual","test_file":test,"status":status,"completed":!outcome.timed_out,
            "runner":RUNNER,"executed_tests":tally.executed,"failed_tests":tally.failed,"skipped_tests":tally.skipped,"tap_violations":tally.violations.len(),
            "exit_code":outcome.code,"log":log_path,
        });
        if let Some(reason) = reason {
            result["reason"] = json!(reason);
        }
        if status != "passed" {
            say(&format!(
                "scoped-test: residual suite {status}: {RUNNER} {test}"
            ));
        }
        let stop = status != "passed";
        done.push(result);
        if stop {
            return Ok(done);
        }
    }
    Ok(done)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::testutil;

    fn tally(text: &str) -> Tally {
        let mut t = Tally::default();
        text.lines().for_each(|l| t.observe(l));
        t
    }

    #[test]
    fn only_numbered_ok_and_not_ok_lines_are_cases() {
        let t = tally(
            "ok 1 - a\nnot ok 2 - b\nok 3\nok to proceed\nok 4x - c\n# ok 5 - d\n  ok 6 - e\n# not ok 7 - f\n  not ok 8 - g\n1..3\n",
        );
        assert_eq!((t.executed, t.failed, t.skipped), (3, 1, 0));
        assert_eq!(t.plan, Some(3));
        assert!(t.violations.is_empty(), "{:?}", t.violations);
        assert_eq!(t.plan_problem(), None);
    }

    #[test]
    fn a_skipped_case_is_not_an_executed_test() {
        let t = tally("ok 1 - a # SKIP no docker\nok 2 - b\n1..2\n");
        assert_eq!((t.executed, t.failed, t.skipped), (1, 0, 1));
        assert_eq!(t.plan_problem(), None);
    }

    #[test]
    fn an_unnumbered_not_ok_is_a_failure_and_a_violation() {
        let t = tally("ok 1 - good\nnot ok - bad\n1..1\n");
        assert_eq!(t.failed, 1);
        assert_eq!(t.violations.len(), 1, "{:?}", t.violations);
        let t = tally("not ok\n");
        assert_eq!(t.failed, 1);
        // `not okay` and other words are prose, not a verdict.
        assert_eq!(tally("not okay\nnot ok-ish\n").failed, 0);
    }

    #[test]
    fn plan_bail_out_and_numbering_problems_are_violations() {
        assert!(tally("ok 1 - good\nBail out! abort\n1..1\n").violations[0].contains("bail out"));
        // A plan that promises more than the output holds.
        assert!(
            tally("1..2\nok 1 - partial\n")
                .plan_problem()
                .unwrap()
                .contains("plan 1..2")
        );
        // A plan that promises less (a case beyond it).
        assert!(tally("ok 1 - a\nok 2 - b\n1..1\n").plan_problem().is_some());
        // No plan at all: the suite never reached its end.
        assert!(
            tally("ok 1 - a\n")
                .plan_problem()
                .unwrap()
                .contains("no `1..N` plan")
        );
        // A repeated or skipped number.
        assert_eq!(tally("ok 1 - a\nok 1 - a\n1..2\n").violations.len(), 1);
        assert_eq!(tally("ok 1 - a\nok 3 - c\n1..2\n").violations.len(), 1);
        // Two plans.
        assert_eq!(tally("1..1\nok 1 - a\n1..1\n").violations.len(), 1);
        // An unreadable plan is a violation, not silently ignored.
        assert_eq!(tally("1..99999999999999999999999999\n").violations.len(), 1);
    }

    fn fixture() -> (tempfile::TempDir, BTreeMap<String, String>) {
        let dir = tempfile::tempdir().unwrap();
        let env = BTreeMap::from([("PATH".to_string(), std::env::var("PATH").unwrap())]);
        (dir, env)
    }

    fn go(
        dir: &Path,
        env: &BTreeMap<String, String>,
        scripts: &[(&str, &str)],
        dry: bool,
        per: u64,
    ) -> (Vec<Value>, String) {
        let mut entries = vec![];
        for (name, body) in scripts {
            testutil::write(dir, name, body);
            entries.push(planned(name));
        }
        let isolation = Isolation::unavailable("test");
        let cancel = AtomicUsize::new(0);
        let logs = dir.join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        let run = Run {
            work: dir,
            env,
            isolation: &isolation,
            cancel: &cancel,
            per_suite: Duration::from_secs(per),
            total_left: None,
            log_dir: &logs,
        };
        let mut said = String::new();
        let out = execute(entries, dry, &run, &mut |l| {
            said.push_str(l);
            said.push('\n')
        })
        .unwrap();
        (out, said)
    }

    /// The status of one suite body run through the production `execute`.
    fn verdict(body: &str) -> Value {
        let (dir, env) = fixture();
        let (out, _) = go(dir.path(), &env, &[("t/test_v.sh", body)], false, 30);
        out[0].clone()
    }

    #[test]
    fn a_passing_suite_reports_its_case_count_and_a_failing_case_is_red_even_at_exit_zero() {
        let out = verdict("echo 'ok 1 - a'; echo 'ok 2 - b'; echo '1..2'\n");
        assert_eq!(out["status"], "passed");
        assert_eq!(out["executed_tests"], 2);
        let out = verdict("echo 'ok 1 - a'; echo 'not ok 2 - b'; echo '1..2'\n");
        assert_eq!(out["status"], "failed");
        assert_eq!(out["failed_tests"], 1);
        let out = verdict("echo 'ok 1 - a'; echo '1..1'; exit 3\n");
        assert_eq!(out["status"], "failed");
        assert_eq!(out["exit_code"], 3);
    }

    #[test]
    fn an_all_skipped_suite_is_red_and_never_a_pass() {
        let out = verdict("echo 'ok 1 - a # SKIP needs linux'; echo '1..1'\n");
        assert_eq!(out["status"], "no_tests_executed", "{out}");
        assert_eq!(out["executed_tests"], 0);
        assert_eq!(out["skipped_tests"], 1);
        // Several skips, still no executed case.
        let out = verdict("echo 'ok 1 - a # SKIP x'; echo 'ok 2 - b # skip y'; echo '1..2'\n");
        assert_eq!(out["status"], "no_tests_executed");
        // One executed case beside the skips is evidence.
        let out = verdict("echo 'ok 1 - a # SKIP x'; echo 'ok 2 - b'; echo '1..2'\n");
        assert_eq!(out["status"], "passed", "{out}");
        assert_eq!(out["executed_tests"], 1);
        assert_eq!(out["skipped_tests"], 1);
    }

    #[test]
    fn malformed_or_incomplete_tap_is_red_at_exit_zero() {
        for (what, body) in [
            (
                "an unnumbered failure",
                "echo 'ok 1 - good'; echo 'not ok - bad'; echo '1..1'\n",
            ),
            (
                "a plan longer than the cases",
                "echo '1..2'; echo 'ok 1 - partial'\n",
            ),
            (
                "a bail out",
                "echo 'ok 1 - good'; echo 'Bail out! abort'; echo '1..1'\n",
            ),
            ("no plan (the suite stopped early)", "echo 'ok 1 - good'\n"),
            (
                "a duplicated case number",
                "printf 'ok 1 - a\\nok 1 - a\\n1..2\\n'\n",
            ),
            (
                "a case beyond the plan",
                "printf 'ok 1 - a\\nok 2 - b\\n1..1\\n'\n",
            ),
        ] {
            let out = verdict(body);
            assert_eq!(out["status"], "failed", "{what}: {out}");
            assert!(
                out["reason"].is_string() || out["failed_tests"].as_u64() > Some(0),
                "{what}: {out}"
            );
        }
    }

    #[test]
    fn exit_zero_without_a_case_line_is_no_evidence() {
        let (dir, env) = fixture();
        let (out, said) = go(
            dir.path(),
            &env,
            &[("t/test_a.sh", "echo hello\n")],
            false,
            30,
        );
        assert_eq!(out[0]["status"], "no_tests_executed");
        assert_eq!(out[0]["executed_tests"], 0);
        assert!(said.contains("executed 0 tests"));
    }

    #[test]
    fn the_first_red_suite_stops_the_lane_and_later_suites_never_run() {
        let (dir, env) = fixture();
        let (out, _) = go(
            dir.path(),
            &env,
            &[
                ("t/test_a.sh", "echo 'not ok 1 - a'; echo '1..1'\n"),
                ("t/test_b.sh", "touch ran-b; echo 'ok 1 - b'; echo '1..1'\n"),
            ],
            false,
            30,
        );
        assert_eq!(out.len(), 1);
        assert!(!dir.path().join("ran-b").exists());
    }

    #[test]
    fn a_suite_is_killed_at_its_deadline_with_its_children() {
        let (dir, env) = fixture();
        let start = Instant::now();
        let (out, _) = go(
            dir.path(),
            &env,
            &[("t/test_a.sh", "sleep 600 & echo 'ok 1 - a'; sleep 600\n")],
            false,
            1,
        );
        assert_eq!(out[0]["status"], "timed_out");
        assert!(start.elapsed() < Duration::from_secs(30));
    }

    #[test]
    fn a_dry_run_executes_nothing_and_keeps_the_planned_entry() {
        let (dir, env) = fixture();
        let (out, _) = go(
            dir.path(),
            &env,
            &[("t/test_a.sh", "touch ran; echo 'ok 1 - a'\n")],
            true,
            30,
        );
        assert_eq!(out[0], planned("t/test_a.sh"));
        assert!(!dir.path().join("ran").exists());
    }

    #[test]
    fn a_contract_gap_entry_passes_through_red_and_a_missing_bash_is_typed() {
        let (dir, _) = fixture();
        let empty = BTreeMap::from([("PATH".to_string(), "/nonexistent".to_string())]);
        let gap = json!({"status":"incomplete_contract","test_file":"scripts/tests/test_x.py"});
        let isolation = Isolation::unavailable("test");
        let cancel = AtomicUsize::new(0);
        let run = Run {
            work: dir.path(),
            env: &empty,
            isolation: &isolation,
            cancel: &cancel,
            per_suite: Duration::from_secs(5),
            total_left: None,
            log_dir: dir.path(),
        };
        let out = execute(
            vec![gap.clone(), planned("t/test_a.sh")],
            false,
            &run,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(out[0], gap);
        assert_eq!(out[1]["status"], "runner_unavailable");
        assert_eq!(out[1]["executed_tests"], 0);
    }

    /// A suite body that sources the production `tap.sh` and runs `cases` before `tap_done`.
    fn tap_suite(cases: &str) -> String {
        let lib = crate::testutil::repo_root().join("scripts/tests/lib/tap.sh");
        format!("source '{}'\n{cases}\ntap_done\n", lib.display())
    }

    /// The runner's verdict for a suite written on the real `tap.sh`.
    fn tap_verdict(cases: &str) -> Value {
        verdict(&tap_suite(cases))
    }

    // The harness lets a case fail in a place `exit` cannot reach: inside `$(...)`, in a helper
    // whose caller then returns 0, and under the OR-lists that switch errexit off in Bash 3.2.
    // Each shape below must still be a red suite (#468).
    #[test]
    fn the_real_harness_turns_every_hidden_failure_shape_into_a_red_case() {
        for (what, cases) in [
            (
                "tap_fail in a command substitution whose value is ignored",
                "c() { x=\"$(tap_fail boom)\"; return 0; }\nrun_case 'a' c\n",
            ),
            (
                "tap_fail in a subshell, then return 0",
                "c() { (tap_fail broken); return 0; }\nrun_case 'a' c\n",
            ),
            (
                "an empty value from a failed assertion helper in $(...) compared numerically",
                "idx() { tap_fail missing; }\nc() { [[ \"$(idx)\" -lt 5 ]]; }\nrun_case 'a' c\n",
            ),
            (
                "assert_eq inside a pipeline subshell",
                "c() { printf 'x\\n' | { read -r v; assert_eq y \"$v\" 'piped'; }; return 0; }\nrun_case 'a' c\n",
            ),
            (
                "a plainly failing assertion",
                "c() { assert_eq a b 'plain'; }\nrun_case 'a' c\n",
            ),
            (
                "a failing assertion run from an `||` list",
                "c() { assert_eq a b 'or' || true; return 0; }\nrun_case 'a' c\n",
            ),
        ] {
            let out = tap_verdict(cases);
            assert_eq!(out["status"], "failed", "{what}: {out}");
            assert!(out["failed_tests"].as_u64() >= Some(1), "{what}: {out}");
        }
    }

    #[test]
    fn the_real_harness_passes_a_clean_case_and_fails_an_all_skipped_or_empty_suite() {
        let out = tap_verdict("c() { assert_eq a a 'same'; }\nrun_case 'a' c\n");
        assert_eq!(out["status"], "passed", "{out}");
        assert_eq!(out["executed_tests"], 1);
        // tap_done itself refuses an all-skipped suite, whatever the runner would say.
        let out = tap_verdict("skip_case 'x' 'needs linux'\n");
        assert_ne!(out["status"], "passed", "{out}");
        assert_eq!(out["executed_tests"], 0);
        let out = tap_verdict("\n");
        assert_ne!(out["status"], "passed", "{out}");
        // A skip beside a real case is fine.
        let out = tap_verdict(
            "c() { assert_eq a a 'same'; }\nskip_case 'x' 'needs linux'\nrun_case 'a' c\n",
        );
        assert_eq!(out["status"], "passed", "{out}");
        assert_eq!(out["skipped_tests"], 1);
    }
}
