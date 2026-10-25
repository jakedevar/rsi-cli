//! The AWS orphan sweeper terminates expired/untagged RSI hosts and reconciles
//! reservations (ported from scripts/tests/test_cloud_watchdog.py, #3767).
//!
//! AWS sits behind the `Aws` trait: an in-memory fake stands in for the CLI, and
//! one unix test drives the real CLI runner against a shell shim. No test
//! touches real AWS.

mod common;

use std::cell::RefCell;
use std::fs;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use common::{HEADER, run};
use rsi_cloud::util::{fnmatchcase, parse_time};
use rsi_cloud::watchdog::{Aws, AwsError, Options, pass};
use serde_json::{Value, json};

const NOW: &str = "2026-10-21T12:00:00Z";

fn at(text: &str) -> DateTime<Utc> {
    parse_time(text).expect("timestamp")
}

fn tags(resource: &Value) -> Vec<(String, String)> {
    resource["Tags"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|tag| {
            (
                tag["Key"].as_str().unwrap().to_string(),
                tag["Value"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn instance(id: &str, service: &str, expires: Option<&str>, state: &str, launch: &str) -> Value {
    let mut tags = vec![json!({"Key": "Service", "Value": service})];
    if let Some(expires) = expires {
        tags.push(json!({"Key": "rsi:expires-at", "Value": expires}));
    }
    json!({"InstanceId": id, "State": {"Name": state}, "LaunchTime": launch, "Tags": tags})
}

fn host(id: &str, expires: Option<&str>) -> Value {
    instance(
        id,
        "rsi-remote-gate",
        expires,
        "running",
        "2026-10-21T10:00:00+00:00",
    )
}

fn volume(id: &str, expires: Option<&str>, state: &str) -> Value {
    let mut tags = vec![json!({"Key": "Service", "Value": "rsi-remote-gate"})];
    if let Some(expires) = expires {
        tags.push(json!({"Key": "rsi:expires-at", "Value": expires}));
    }
    json!({"VolumeId": id, "State": state, "Tags": tags})
}

/// An in-memory AWS: describe with the tag/state filters the watchdog sends,
/// terminate and delete mutate the state, and every call is logged.
#[derive(Default)]
struct FakeAws {
    instances: RefCell<Vec<Value>>,
    volumes: RefCell<Vec<Value>>,
    calls: RefCell<Vec<String>>,
    /// Answer every call with `{}` (the Issue's repro of an unhelpful CLI).
    empty: bool,
}

impl FakeAws {
    fn matches(resource: &Value, filters: &[String], state_of: impl Fn(&Value) -> String) -> bool {
        filters.iter().all(|filter| {
            let name = filter
                .split_once("Name=")
                .unwrap()
                .1
                .split_once(',')
                .unwrap()
                .0;
            let values: Vec<&str> = filter.split_once("Values=").unwrap().1.split(',').collect();
            if let Some(tag) = name.strip_prefix("tag:") {
                let have = tags(resource)
                    .into_iter()
                    .find(|(key, _)| key == tag)
                    .map_or(String::new(), |(_, value)| value);
                values.iter().any(|pattern| fnmatchcase(&have, pattern))
            } else if name == "instance-state-name" || name == "status" {
                values.contains(&state_of(resource).as_str())
            } else {
                true
            }
        })
    }
}

fn values_after(command: &[String], flag: &str) -> Vec<String> {
    command
        .iter()
        .position(|arg| arg == flag)
        .map(|at| {
            command[at + 1..]
                .iter()
                .take_while(|arg| !arg.starts_with("--"))
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

impl Aws for FakeAws {
    fn call(&self, command: &[String]) -> Result<Value, AwsError> {
        self.calls.borrow_mut().push(command.join(" "));
        if self.empty {
            return Ok(json!({}));
        }
        match command[1].as_str() {
            "describe-instances" => {
                let ids = values_after(command, "--instance-ids");
                let instances = self.instances.borrow();
                let found: Vec<Value> = if ids.is_empty() {
                    let filters = values_after(command, "--filters");
                    instances
                        .iter()
                        .filter(|i| {
                            Self::matches(i, &filters, |r| {
                                r["State"]["Name"].as_str().unwrap().to_string()
                            })
                        })
                        .cloned()
                        .collect()
                } else {
                    instances
                        .iter()
                        .filter(|i| ids.contains(&i["InstanceId"].as_str().unwrap().to_string()))
                        .cloned()
                        .collect()
                };
                if !ids.is_empty() && found.is_empty() {
                    return Err(AwsError("InvalidInstanceID.NotFound".into()));
                }
                Ok(json!({"Reservations": [{"Instances": found}]}))
            }
            "describe-volumes" => {
                let filters = values_after(command, "--filters");
                let found: Vec<Value> = self
                    .volumes
                    .borrow()
                    .iter()
                    .filter(|v| {
                        Self::matches(v, &filters, |r| r["State"].as_str().unwrap().to_string())
                    })
                    .cloned()
                    .collect();
                Ok(json!({"Volumes": found}))
            }
            "terminate-instances" => {
                let ids = values_after(command, "--instance-ids");
                for i in self.instances.borrow_mut().iter_mut() {
                    if ids.contains(&i["InstanceId"].as_str().unwrap().to_string()) {
                        i["State"]["Name"] = json!("shutting-down");
                    }
                }
                Ok(json!({}))
            }
            "delete-volume" => {
                let id = values_after(command, "--volume-id").remove(0);
                self.volumes
                    .borrow_mut()
                    .retain(|v| v["VolumeId"] != id.as_str());
                Ok(json!({}))
            }
            other => Err(AwsError(format!("fake aws: unsupported {other}"))),
        }
    }
}

struct Sweep {
    dir: tempfile::TempDir,
    aws: FakeAws,
}

impl Sweep {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("spend.md"), HEADER).unwrap();
        Self {
            dir,
            aws: FakeAws::default(),
        }
    }

    fn ledger_path(&self) -> PathBuf {
        self.dir.path().join("spend.md")
    }

    fn ledger(&self) -> String {
        fs::read_to_string(self.ledger_path()).unwrap()
    }

    fn append(&self, text: &str) {
        let mut ledger = self.ledger();
        ledger.push_str(text);
        fs::write(self.ledger_path(), ledger).unwrap();
    }

    fn sweep_at(&self, dry_run: bool, now: &str) -> String {
        let options = Options {
            dry_run,
            ledger: self.ledger_path(),
            service_glob: "rsi-*".into(),
            exempt_service: "*-rsid".into(),
            grace_minutes: 10.0,
        };
        pass(&self.aws, &options, at(now)).expect("sweep succeeds")
    }

    /// Replace the AWS state, optionally the ledger body after the header, and sweep.
    fn run_sweep(
        &self,
        instances: Vec<Value>,
        volumes: Vec<Value>,
        ledger: Option<&str>,
    ) -> String {
        *self.aws.instances.borrow_mut() = instances;
        *self.aws.volumes.borrow_mut() = volumes;
        if let Some(body) = ledger {
            fs::write(self.ledger_path(), format!("{HEADER}{body}")).unwrap();
        }
        self.sweep_at(false, NOW)
    }

    fn sweep_now(&self, now: &str) -> String {
        self.sweep_at(false, now)
    }

    fn mutating(&self) -> Vec<String> {
        self.aws
            .calls
            .borrow()
            .iter()
            .filter(|c| c.contains("terminate-instances") || c.contains("delete-volume"))
            .cloned()
            .collect()
    }

    fn state_of(&self, id: &str) -> String {
        self.aws
            .instances
            .borrow()
            .iter()
            .find(|i| i["InstanceId"] == id)
            .unwrap()["State"]["Name"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// An S1 reservation written by the production writer, with its start/rate suffix.
    fn reserve_s1(&mut self, token: &str) {
        let result = run(
            &[
                "spend",
                "reserve",
                "--ledger",
                &self.ledger_path().display().to_string(),
                "--minutes",
                "60",
                "--rate",
                "1",
                "--token",
                token,
                "--now",
                "2020-01-01T00:00:00Z",
            ],
            "",
        );
        assert_eq!(result.code, 0, "{}", result.stderr);
        assert_eq!(result.stdout.trim(), format!("reservation_id={token}"));
        // The Issue's repro has an aws CLI that always returns an empty object.
        self.aws = FakeAws {
            empty: true,
            ..FakeAws::default()
        };
    }
}

#[test]
fn settled_s1_reservation_keeps_only_its_actual_charge() {
    let mut sweep = Sweep::new();
    sweep.reserve_s1("res-t1");
    sweep.append("Gate window i-1: stop 2020-01-01T01:00:00Z, elapsed 1800 s = 0.5000 h, est compute $0.5 at $1.0000/h; reservation res-t1.\n");
    let settled = sweep.ledger();
    for _ in 0..2 {
        let out = sweep.sweep_now("2020-01-01T03:00:00Z");
        assert!(out.contains("reconciled=- open_reserved_usd=0.00"), "{out}");
        assert!(
            out.contains("ledger_cumulative_usd=0.50 ledger_today_usd=0.50"),
            "{out}"
        );
        assert_eq!(sweep.ledger(), settled);
    }
}

#[test]
fn released_s1_reservation_is_settled() {
    let mut sweep = Sweep::new();
    sweep.reserve_s1("res-t1");
    sweep.append("Gate reservation res-t1: released, no host started.\n");
    let released = sweep.ledger();
    for _ in 0..2 {
        let out = sweep.sweep_now("2020-01-01T03:00:00Z");
        assert!(out.contains("reconciled=- open_reserved_usd=0.00"), "{out}");
        assert!(out.contains("ledger_cumulative_usd=0.00"), "{out}");
        assert_eq!(sweep.ledger(), released);
    }
}

#[test]
fn open_s1_reservation_is_charged_once_after_grace() {
    let mut sweep = Sweep::new();
    sweep.reserve_s1("res-t1");
    let reserved = sweep.ledger();
    let out = sweep.sweep_now("2020-01-01T01:10:00Z");
    assert!(out.contains("reconciled=- open_reserved_usd=1.00"), "{out}");
    assert_eq!(
        sweep.ledger(),
        reserved,
        "inside the grace the reservation stays open"
    );
    let out = sweep.sweep_now("2020-01-01T03:00:00Z");
    assert!(out.contains("reconciled=res-t1:$1.0000"), "{out}");
    assert!(out.contains("ledger_cumulative_usd=1.00"), "{out}");
    let charged = sweep.ledger();
    assert_eq!(charged.matches("Gate window res-t1: stop").count(), 1);
    assert!(charged.contains("reservation res-t1."));
    let out = sweep.sweep_now("2020-01-01T04:00:00Z");
    assert!(out.contains("reconciled=- open_reserved_usd=0.00"), "{out}");
    assert_eq!(sweep.ledger(), charged);
    // The spend guard must also see the watchdog's token-keyed settlement.
    let probe = run(
        &[
            "spend",
            "reserve",
            "--ledger",
            &sweep.ledger_path().display().to_string(),
            "--minutes",
            "60",
            "--rate",
            "1",
            "--token",
            "res-next",
            "--now",
            "2020-01-01T04:00:00Z",
        ],
        "",
    );
    assert_eq!(probe.code, 0, "{}", probe.stderr);
    assert!(
        probe.stderr.contains("cumulative $1.00 + $0.00 reserved"),
        "{}",
        probe.stderr
    );
}

#[test]
fn duplicate_s1_reservation_token_closes_once() {
    let mut sweep = Sweep::new();
    sweep.reserve_s1("res-t1");
    let ledger = sweep.ledger();
    let line = ledger.lines().last().unwrap().to_string();
    sweep.append(&format!("{line}\n"));
    let out = sweep.sweep_now("2020-01-01T03:00:00Z");
    assert!(out.contains("ledger_cumulative_usd=1.00"), "{out}");
    assert_eq!(
        sweep.ledger().matches("Gate window res-t1: stop").count(),
        1
    );
}

#[test]
fn duplicate_s2_reservation_window_closes_once() {
    let sweep = Sweep::new();
    let line = "Gate window i-0111: reserved $2.0000 until 2026-10-21T10:00:00Z.\n";
    let out = sweep.run_sweep(vec![], vec![], Some(&line.repeat(2)));
    assert!(out.contains("reconciled=i-0111:$2.0000"), "{out}");
    assert!(out.contains("ledger_cumulative_usd=2.00"), "{out}");
    let charged = sweep.ledger();
    assert_eq!(charged.matches("Gate window i-0111: stop").count(), 1);
    sweep.run_sweep(vec![], vec![], None);
    assert_eq!(sweep.ledger(), charged);
}

#[test]
fn expired_instance_is_terminated_and_ledgered() {
    let sweep = Sweep::new();
    let out = sweep.run_sweep(
        vec![host("i-0aa1", Some("2026-10-21T11:00:00Z"))],
        vec![],
        None,
    );
    assert!(out.contains("terminated=i-0aa1:expired"), "{out}");
    assert_eq!(sweep.state_of("i-0aa1"), "shutting-down");
    let ledger = sweep.ledger();
    assert!(
        ledger.contains("Gate window i-0aa1: stop 2026-10-21T12:00:00Z, elapsed 7200 s = 2.0000 h"),
        "{ledger}"
    );
    assert!(
        ledger.contains("est compute $1.5626 at $0.7813/h; terminated by orphan sweep (expired)"),
        "{ledger}"
    );
}

#[test]
fn missing_expiry_tag_is_terminated() {
    let sweep = Sweep::new();
    let out = sweep.run_sweep(vec![host("i-0bb2", None)], vec![], None);
    assert!(out.contains("terminated=i-0bb2:missing_expiry"), "{out}");
    assert_eq!(sweep.state_of("i-0bb2"), "shutting-down");
}

#[test]
fn unparsable_expiry_counts_as_missing() {
    let sweep = Sweep::new();
    let out = sweep.run_sweep(vec![host("i-0bb3", Some("tomorrow-ish"))], vec![], None);
    assert!(out.contains("terminated=i-0bb3:missing_expiry"), "{out}");
    let out = sweep.run_sweep(vec![host("i-0bb4", Some("   "))], vec![], None);
    assert!(out.contains("terminated=i-0bb4:missing_expiry"), "{out}");
}

#[test]
fn live_host_is_left_alone() {
    let sweep = Sweep::new();
    let out = sweep.run_sweep(
        vec![host("i-0cc3", Some("2026-10-21T13:00:00Z"))],
        vec![],
        None,
    );
    assert!(out.contains("terminated=-"), "{out}");
    assert!(sweep.mutating().is_empty());
    assert_eq!(sweep.ledger(), HEADER);
}

#[test]
fn just_expired_host_gets_the_grace_period() {
    let sweep = Sweep::new();
    let out = sweep.run_sweep(
        vec![host("i-0cc4", Some("2026-10-21T11:55:00Z"))],
        vec![],
        None,
    );
    assert!(out.contains("terminated=-"), "{out}");
    assert!(sweep.mutating().is_empty());
}

#[test]
fn persistent_rsid_host_and_unrelated_services_are_never_swept() {
    let sweep = Sweep::new();
    let out = sweep.run_sweep(
        vec![
            instance(
                "i-0dd4",
                "rsi-cloud-rsid",
                None,
                "running",
                "2026-10-21T10:00:00+00:00",
            ),
            instance(
                "i-0dd5",
                "other-app",
                None,
                "running",
                "2026-10-21T10:00:00+00:00",
            ),
        ],
        vec![],
        None,
    );
    assert!(out.contains("terminated=-"), "{out}");
    assert!(sweep.mutating().is_empty());
}

#[test]
fn unattached_expired_volume_is_deleted_but_attached_is_not() {
    let sweep = Sweep::new();
    let out = sweep.run_sweep(
        vec![],
        vec![
            volume("vol-1", Some("2026-10-21T09:00:00Z"), "available"),
            volume("vol-2", None, "available"),
            volume("vol-3", Some("2026-10-21T20:00:00Z"), "available"),
            volume("vol-4", Some("2026-10-21T09:00:00Z"), "in-use"),
        ],
        None,
    );
    assert!(
        out.contains("deleted_volumes=vol-1:expired,vol-2:missing_expiry"),
        "{out}"
    );
    let left: Vec<String> = sweep
        .aws
        .volumes
        .borrow()
        .iter()
        .map(|v| v["VolumeId"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(left, ["vol-3", "vol-4"]);
}

#[test]
fn dry_run_describes_only() {
    let sweep = Sweep::new();
    *sweep.aws.instances.borrow_mut() = vec![host("i-0ee5", None)];
    *sweep.aws.volumes.borrow_mut() = vec![volume("vol-9", None, "available")];
    let out = sweep.sweep_at(true, NOW);
    assert!(
        out.contains("dry_run=True terminated=i-0ee5:missing_expiry"),
        "{out}"
    );
    assert!(sweep.mutating().is_empty());
    assert_eq!(sweep.ledger(), HEADER);
}

#[test]
fn second_pass_does_not_double_count() {
    let sweep = Sweep::new();
    sweep.run_sweep(vec![host("i-0ff6", None)], vec![], None);
    let once = sweep.ledger();
    // The instance is now shutting-down, so it no longer matches; a stale describe still must not re-log.
    sweep.run_sweep(vec![host("i-0ff6", None)], vec![], None);
    assert_eq!(sweep.ledger(), once);
}

#[test]
fn stale_reservation_is_reconciled_with_real_launch_and_end() {
    let sweep = Sweep::new();
    let mut gone = instance(
        "i-0111",
        "rsi-remote-gate",
        Some("2026-10-21T10:00:00Z"),
        "terminated",
        "2026-10-21T08:00:00+00:00",
    );
    gone["StateTransitionReason"] = json!("User initiated (2026-10-21 09:00:00 GMT)");
    let reservation = "Gate window i-0111: reserved $2.0000 until 2026-10-21T10:00:00Z.\n";
    let out = sweep.run_sweep(vec![gone.clone()], vec![], Some(reservation));
    assert!(out.contains("reconciled=i-0111:$0.7813"), "{out}");
    let ledger = sweep.ledger();
    assert!(ledger.contains("Gate window i-0111: stop 2026-10-21T09:00:00Z, elapsed 3600 s = 1.0000 h, est compute $0.7813"), "{ledger}");
    assert!(ledger.contains("reconciled by orphan sweep"));
    // Reconciling again is a no-op once the stop line exists.
    sweep.run_sweep(vec![gone], vec![], None);
    assert_eq!(sweep.ledger(), ledger);
}

#[test]
fn reservation_without_instance_is_charged_in_full() {
    let sweep = Sweep::new();
    let out = sweep.run_sweep(
        vec![],
        vec![],
        Some("reserved $1.5000 until 2026-10-21T09:00:00Z\n"),
    );
    assert!(out.contains("reconciled=unmatched:$1.5000"), "{out}");
    assert!(sweep.ledger().contains("est compute $1.5000"));
}

#[test]
fn future_reservation_stays_open() {
    let sweep = Sweep::new();
    let out = sweep.run_sweep(
        vec![],
        vec![],
        Some("Gate window i-0222: reserved $3.0000 until 2026-10-21T14:00:00Z.\n"),
    );
    assert!(out.contains("reconciled=-"), "{out}");
    assert!(out.contains("open_reserved_usd=3.00"), "{out}");
}

#[test]
fn reservation_for_an_expired_running_host_closes_in_one_pass() {
    let sweep = Sweep::new();
    let running = instance(
        "i-0333",
        "rsi-remote-gate",
        Some("2026-10-21T10:30:00Z"),
        "running",
        "2026-10-21T10:00:00+00:00",
    );
    let out = sweep.run_sweep(
        vec![running],
        vec![],
        Some("Gate window i-0333: reserved $2.0000 until 2026-10-21T10:30:00Z.\n"),
    );
    assert!(out.contains("terminated=i-0333:expired"), "{out}");
    assert!(out.contains("reconciled=-"), "{out}");
    assert_eq!(
        sweep.ledger().matches("Gate window i-0333: stop").count(),
        1
    );
}

#[test]
fn summary_includes_the_spend_ledger_totals() {
    let sweep = Sweep::new();
    let out = sweep.run_sweep(
        vec![],
        vec![],
        Some("Gate window i-0444: stop 2026-10-21T01:00:00Z, elapsed 1 s, est compute $2.5000 at $1.7808/h; ok.\n"),
    );
    assert!(
        out.contains("ledger_cumulative_usd=2.50 ledger_today_usd=2.50"),
        "{out}"
    );
}

#[test]
fn an_unreadable_ledger_header_is_reported_in_the_summary_not_hidden() {
    let sweep = Sweep::new();
    fs::write(sweep.ledger_path(), "no header\n").unwrap();
    let out = sweep.sweep_at(false, NOW);
    assert!(out.contains("spend=unreadable("), "{out}");
}

#[test]
fn a_terminated_hosts_cost_is_ledgered_even_if_a_volume_delete_fails() {
    // The instance is already terminating when delete-volume fails: its stop
    // line must be on the ledger, or the next pass (which no longer sees it)
    // would never charge it.
    struct FailingDelete(FakeAws);
    impl Aws for FailingDelete {
        fn call(&self, command: &[String]) -> Result<Value, AwsError> {
            if command[1] == "delete-volume" {
                return Err(AwsError("aws ec2 delete-volume failed: boom".into()));
            }
            self.0.call(command)
        }
    }
    let sweep = Sweep::new();
    let aws = FailingDelete(FakeAws::default());
    *aws.0.instances.borrow_mut() = vec![host("i-0a99", None)];
    *aws.0.volumes.borrow_mut() = vec![volume("vol-1", None, "available")];
    let options = Options {
        dry_run: false,
        ledger: sweep.ledger_path(),
        service_glob: "rsi-*".into(),
        exempt_service: "*-rsid".into(),
        grace_minutes: 10.0,
    };
    let error = pass(&aws, &options, at(NOW)).unwrap_err();
    assert!(error.contains("boom"), "{error}");
    assert!(sweep.ledger().contains("Gate window i-0a99: stop"));
}

#[cfg(unix)]
mod cli {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// The real `rsi-cloud watchdog` against an `aws` shell shim that logs its
    /// argv and answers from fixed JSON.
    #[test]
    fn the_cli_runs_the_aws_binary_with_region_profile_and_json_output() {
        let dir = tempfile::tempdir().unwrap();
        let calls = dir.path().join("calls");
        let shim = dir.path().join("aws");
        let script = format!(
            "#!/bin/sh\necho \"$*\" >> {calls}\ncase \"$2\" in\n\
describe-instances) echo '{{\"Reservations\":[{{\"Instances\":[{{\"InstanceId\":\"i-0abc\",\"State\":{{\"Name\":\"running\"}},\"LaunchTime\":\"2026-10-21T11:00:00+00:00\",\"Tags\":[{{\"Key\":\"Service\",\"Value\":\"rsi-remote-gate\"}}]}}]}}]}}' ;;\n\
describe-volumes) echo '{{\"Volumes\":[]}}' ;;\n\
*) echo '{{}}' ;;\nesac\n",
            calls = calls.display()
        );
        fs::write(&shim, script).unwrap();
        fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
        let ledger = dir.path().join("spend.md");
        fs::write(&ledger, HEADER).unwrap();
        let ran = run(
            &[
                "watchdog",
                "--aws",
                &shim.display().to_string(),
                "--ledger",
                &ledger.display().to_string(),
                "--now",
                NOW,
            ],
            "",
        );
        assert_eq!(ran.code, 0, "{}", ran.stderr);
        assert!(
            ran.stdout
                .starts_with("2026-10-21T12:00:00Z dry_run=False terminated=i-0abc:missing_expiry"),
            "{}",
            ran.stdout
        );
        let log = fs::read_to_string(&calls).unwrap();
        assert!(
            log.contains("ec2 describe-instances --filters Name=tag:Service,Values=rsi-* Name=instance-state-name,Values=pending,running,stopping,stopped --region us-west-1 --profile rsi-cloud-watchdog --output json"),
            "{log}"
        );
        assert!(
            log.contains("ec2 terminate-instances --instance-ids i-0abc --region us-west-1"),
            "{log}"
        );
        assert!(
            fs::read_to_string(&ledger)
                .unwrap()
                .contains("Gate window i-0abc: stop 2026-10-21T12:00:00Z, elapsed 3600 s")
        );
        assert!(dir.path().join("watchdog.lock").exists());
    }

    #[test]
    fn a_failing_aws_cli_is_a_failed_pass_that_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let shim = dir.path().join("aws");
        fs::write(&shim, "#!/bin/sh\necho 'AccessDenied' >&2\nexit 254\n").unwrap();
        fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
        let ledger = dir.path().join("spend.md");
        fs::write(&ledger, HEADER).unwrap();
        let ran = run(
            &[
                "watchdog",
                "--aws",
                &shim.display().to_string(),
                "--ledger",
                &ledger.display().to_string(),
                "--now",
                NOW,
            ],
            "",
        );
        assert_eq!(ran.code, 1);
        assert!(
            ran.stderr
                .contains("cloud-watchdog failed: aws ec2 describe-instances failed: AccessDenied"),
            "{}",
            ran.stderr
        );
        assert_eq!(fs::read_to_string(&ledger).unwrap(), HEADER);
    }

    #[test]
    fn bad_options_exit_two() {
        assert_eq!(run(&["watchdog", "--grace-minutes", "-1"], "").code, 2);
        assert_eq!(run(&["watchdog", "--now", "never"], "").code, 2);
        assert_eq!(run(&["watchdog", "--bogus"], "").code, 2);
    }
}
