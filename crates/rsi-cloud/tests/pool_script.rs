//! The warm spot pool reserves every lease before apply, renews only through a
//! fresh reservation, destroys at expiry or after an idle half hour, and
//! settles on down (ported from scripts/tests/test_cloud_pool.py, #3767).
//!
//! No test calls AWS: `scripts/cloud-pool.sh` runs against fake terraform, aws
//! and ssh binaries on PATH (`rsi-cloud-fakes`, one executable symlinked under
//! each tool's name) that log every call to `$FAKE_LOG` and keep a fake
//! terraform state.
#![cfg(unix)]

mod common;

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;

use common::repo_root;
use regex::Regex;
use serde_json::{Value, json};

const HEADER: &str = "# AWS satellite spend ledger\n\n\
Operator grant: $2000 starting 2026-09-27 22:48:22 UTC (direct operator message).\n\
Configured compute estimate: $0.7813/hour. Stop and report by $1500 cumulative under this grant.\n";
const T0: &str = "2030-01-01T00:00:00Z";
/// 720 min at the $0.7813/h worst case.
const LEASE: &str = "9.3756";

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

struct Fixture {
    tmp: tempfile::TempDir,
    bin: PathBuf,
    cloud: PathBuf,
    home: PathBuf,
    log: PathBuf,
}

fn flock_available() -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join("flock").is_file()))
}

fn script(dir: &Path, name: &str, body: &str) {
    let path = dir.join(name);
    fs::write(&path, body).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

impl Fixture {
    /// `None` when the script's own tool (flock) is not installed: the Python
    /// suite needed it too, so the run reports it instead of passing silently.
    fn new() -> Option<Self> {
        if !flock_available() {
            eprintln!("runner_unavailable: flock is not installed; cloud-pool.sh cannot run");
            return None;
        }
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        fs::create_dir(&bin).unwrap();
        for tool in ["terraform", "rsi-cloud", "ssh"] {
            symlink(env!("CARGO_BIN_EXE_rsi-cloud-fakes"), bin.join(tool)).unwrap();
        }
        script(
            &bin,
            "ssh-keyscan",
            "#!/bin/sh\necho \"192.0.2.1 ssh-ed25519 AAAAFAKEKEY\"\n",
        );
        script(
            &bin,
            "ssh-keygen",
            "#!/bin/sh\nfor a in \"$@\"; do [ \"$a\" = \"-R\" ] && exit 0; done\ncat >/dev/null\necho \"256 SHA256:fakefingerprint host (ED25519)\"\n",
        );
        let home = tmp.path().join("home");
        let cloud = home.join(".rsi/cloud");
        fs::create_dir_all(&cloud).unwrap();
        fs::write(cloud.join("spend.md"), HEADER).unwrap();
        fs::write(cloud.join("gate.tfvars"), "ssh_key_name = \"k\"\n").unwrap();
        fs::write(home.join("identity"), "key").unwrap();
        let log = tmp.path().join("calls.jsonl");
        let fixture = Self {
            tmp,
            bin,
            cloud,
            home,
            log,
        };
        fixture.caps(1000, 1500);
        Some(fixture)
    }

    fn caps(&self, daily: u32, stop: u32) {
        fs::write(
            self.cloud.join("spend-caps.json"),
            json!({"stop_line_usd": stop, "daily_cap_usd": daily}).to_string(),
        )
        .unwrap();
    }

    fn ledger_path(&self) -> PathBuf {
        self.cloud.join("spend.md")
    }

    fn ledger(&self) -> String {
        fs::read_to_string(self.ledger_path()).unwrap()
    }

    fn run_with(&self, args: &[&str], now: &str, extra: &[(&str, &str)]) -> Out {
        let path = format!(
            "{}:{}",
            self.bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let output = Command::new(repo_root().join("scripts/cloud-pool.sh"))
            .args(args)
            .env_clear()
            .env("PATH", path)
            .env("HOME", &self.home)
            .env("RSI_CLOUD_DIR", &self.cloud)
            .env("RSI_GATE_IDENTITY", self.home.join("identity"))
            .env("RSI_CLOUD_BIN", self.bin.join("rsi-cloud"))
            .env("RSI_CLOUD_REAL", env!("CARGO_BIN_EXE_rsi-cloud"))
            .env("RSI_POOL_POLL_SECONDS", "0")
            .env("RSI_POOL_NOW", now)
            .env("FAKE_LOG", &self.log)
            .env("FAKE_LEDGER", self.ledger_path())
            .env("FAKE_AWS_STATES", self.tmp.path().join("aws-states.json"))
            .envs(extra.iter().copied())
            .output()
            .expect("run cloud-pool.sh");
        Out {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }

    fn pool(&self, args: &[&str], now: &str) -> Out {
        self.run_with(args, now, &[])
    }

    fn calls(&self, tool: Option<&str>) -> Vec<Value> {
        let Ok(text) = fs::read_to_string(&self.log) else {
            return Vec::new();
        };
        text.lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|row| tool.is_none_or(|tool| row["tool"] == tool))
            .collect()
    }

    /// Terraform calls of `sub`; every command shares the one pool state.
    fn terraform(&self, sub: &str) -> Vec<Value> {
        let calls: Vec<Value> = self
            .calls(Some("terraform"))
            .into_iter()
            .filter(|call| call["sub"] == sub)
            .collect();
        for call in &calls {
            assert_eq!(
                call["state"],
                self.cloud.join("pool.tfstate").display().to_string()
            );
        }
        calls
    }

    fn leases(&self) -> serde_json::Map<String, Value> {
        fs::read_to_string(self.cloud.join("pool.json"))
            .ok()
            .map(|text| {
                serde_json::from_str::<Value>(&text).unwrap()["leases"]
                    .as_object()
                    .unwrap()
                    .clone()
            })
            .unwrap_or_default()
    }

    fn lease_keys(&self) -> Vec<String> {
        self.leases().keys().cloned().collect()
    }

    /// A job was placed on every host just now (S4c/S4d call `cloud-pool.sh touch`).
    fn busy(&self, now: &str) {
        assert_eq!(self.pool(&["touch"], now).code, 0);
    }

    fn up(&self, count: u32) -> Out {
        self.up_at(count, T0)
    }

    fn up_at(&self, count: u32, now: &str) -> Out {
        let out = self.pool(&["up", &count.to_string()], now);
        assert_eq!(out.code, 0, "{}", out.stderr);
        out
    }

    fn shutdowns(&self) -> Vec<String> {
        self.calls(Some("ssh"))
            .into_iter()
            .filter(|c| {
                c["args"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|a| a.as_str().unwrap().contains("shutdown"))
            })
            .map(|c| {
                c["args"]
                    .as_array()
                    .unwrap()
                    .last()
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect()
    }
}

macro_rules! fixture {
    () => {
        match Fixture::new() {
            Some(fixture) => fixture,
            None => return,
        }
    };
}

fn count(pattern: &str, text: &str) -> usize {
    Regex::new(pattern).unwrap().find_iter(text).count()
}

#[test]
fn the_cloud_scripts_parse_and_need_no_python() {
    for name in [
        "cloud-pool.sh",
        "cloud-gate.sh",
        "cloud-sweep.sh",
        "install-cloud-watchdog.sh",
    ] {
        let path = repo_root().join("scripts").join(name);
        assert!(
            Command::new("bash")
                .arg("-n")
                .arg(&path)
                .status()
                .unwrap()
                .success(),
            "{name}"
        );
    }
    for name in [
        "cloud-pool.sh",
        "cloud-gate.sh",
        "install-cloud-watchdog.sh",
        "cloud-watchdog.service",
    ] {
        let text = fs::read_to_string(repo_root().join("scripts").join(name)).unwrap();
        assert!(!text.contains("python"), "{name} still names Python");
        assert!(!text.contains(".py"), "{name} still names a .py file");
    }
}

#[test]
fn the_watchdog_unit_runs_the_rust_binary() {
    let unit = fs::read_to_string(repo_root().join("scripts/cloud-watchdog.service")).unwrap();
    assert!(
        unit.contains("ExecStart=%h/.rsi/bin/rsi-cloud watchdog $RSI_CLOUD_WATCHDOG_ARGS"),
        "{unit}"
    );
}

#[test]
fn up_reserves_each_lease_before_apply() {
    let f = fixture!();
    f.up(2);
    let applies = f.terraform("apply");
    assert_eq!(applies.len(), 1);
    // Both 12 h reservations at the worst-case rate sit in the ledger when apply runs.
    let snapshot = applies[0]["ledger"].as_str().unwrap();
    assert_eq!(
        count(
            &format!(r"reserved \${LEASE} until 2030-01-01T12:00:00Z"),
            snapshot
        ),
        2,
        "{snapshot}"
    );
    let hosts = applies[0]["hosts"].as_object().unwrap();
    assert_eq!(hosts.keys().collect::<Vec<_>>(), ["pool-1", "pool-2"]);
    assert!(
        hosts
            .values()
            .all(|expiry| expiry == "2030-01-01T12:00:00Z")
    );
    let leases = f.leases();
    assert_eq!(leases.keys().collect::<Vec<_>>(), ["pool-1", "pool-2"]);
    for (slot, lease) in &leases {
        let reservation = lease["reservation_id"].as_str().unwrap();
        assert!(
            reservation.starts_with(&format!("res-{slot}-")),
            "{reservation}"
        );
        assert!(f.ledger().contains(&format!("reservation {reservation}")));
        assert_eq!(lease["lease_start"], T0);
        assert_eq!(lease["lease_expires_at"], "2030-01-01T12:00:00Z");
        assert_eq!(lease["state"], "ready");
        assert_eq!(
            lease["host"],
            format!("ec2-user@{}", lease["public_ip"].as_str().unwrap())
        );
        assert!(lease["instance_id"].as_str().unwrap().starts_with("i-"));
    }
    assert_eq!(f.ledger().matches("spot pool pool-").count(), 2);
}

#[test]
fn up_is_idempotent_for_the_same_total() {
    let f = fixture!();
    f.up(2);
    let before = f.ledger();
    f.up(2);
    assert_eq!(f.ledger(), before);
    assert_eq!(f.terraform("apply").len(), 1);
    f.up(3);
    assert_eq!(f.lease_keys(), ["pool-1", "pool-2", "pool-3"]);
}

#[test]
fn more_than_six_hosts_is_refused_before_anything_is_reserved_or_applied() {
    let f = fixture!();
    for want in ["7", "12", "0"] {
        let out = f.pool(&["up", want], T0);
        assert_eq!(out.code, 2, "{}", out.stderr);
        assert!(out.stderr.contains("1 to 6 hosts"), "{}", out.stderr);
    }
    assert_eq!(f.ledger(), HEADER);
    assert!(f.calls(None).is_empty());
    assert!(f.leases().is_empty());
}

#[test]
fn up_refused_at_the_daily_cap_applies_nothing_and_releases_what_it_took() {
    let f = fixture!();
    f.caps(10, 1500); // one 12 h lease fits ($9.38), two do not
    let out = f.pool(&["up", "2"], T0);
    assert_eq!(out.code, 4, "{}", out.stderr);
    assert!(out.stderr.contains("daily cap"), "{}", out.stderr);
    assert!(f.terraform("apply").is_empty());
    assert!(f.leases().is_empty());
    let text = f.ledger();
    assert_eq!(text.matches("reserved $").count(), 1);
    assert!(text.contains("released, no host started"), "{text}");
    // The released reservation no longer counts: one host still fits.
    f.up(1);
}

#[test]
fn up_refused_at_the_stop_line() {
    let f = fixture!();
    f.caps(1000, 5);
    let out = f.pool(&["up", "1"], T0);
    assert_eq!(out.code, 4, "{}", out.stderr);
    assert!(f.terraform("apply").is_empty());
}

#[test]
fn a_failed_apply_rolls_back_and_settles_or_releases() {
    let f = fixture!();
    let out = f.run_with(&["up", "2"], T0, &[("FAKE_TF_FAIL_FIRST_APPLY", "partial")]);
    assert_eq!(out.code, 1, "{}", out.stderr);
    assert!(f.leases().is_empty());
    let text = f.ledger();
    // The hosts the failed apply did create are settled with a stop line ...
    assert_eq!(
        count(r"Gate window i-0\w+: stop .*apply_failed", &text),
        2,
        "{text}"
    );
    // ... and the last apply left the pool empty.
    assert_eq!(f.terraform("apply").last().unwrap()["hosts"], json!({}));
    f.pool(&["up", "1"], T0); // the next try works
}

#[test]
fn a_failed_apply_without_hosts_releases_the_reservations() {
    let f = fixture!();
    let out = f.run_with(&["up", "1"], T0, &[("FAKE_TF_FAIL_FIRST_APPLY", "1")]);
    assert_eq!(out.code, 1, "{}", out.stderr);
    assert!(
        f.ledger()
            .contains("released, no host started (terraform apply failed)"),
        "{}",
        f.ledger()
    );
    assert!(f.leases().is_empty());
}

#[test]
fn renew_refused_at_the_cap_leaves_the_host_to_be_destroyed_at_expiry() {
    let f = fixture!();
    f.up(2);
    f.caps(20, 1500); // 2 x 9.38 reserved; a third lease would pass $20
    let before = f.ledger();
    f.busy("2030-01-01T11:30:00Z");
    let out = f.pool(&["reap"], "2030-01-01T11:30:00Z");
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(out.stderr.contains("not renewed"), "{}", out.stderr);
    assert!(out.stderr.contains("daily cap"), "{}", out.stderr);
    assert_eq!(f.ledger(), before, "a refusal writes nothing");
    assert_eq!(f.lease_keys(), ["pool-1", "pool-2"]);
    for lease in f.leases().values() {
        assert_eq!(lease["renewal"], "refused");
        assert_eq!(lease["lease_expires_at"], "2030-01-01T12:00:00Z");
    }
    assert_eq!(
        f.terraform("apply").len(),
        1,
        "no tag move, nothing destroyed yet"
    );
    assert!(f.shutdowns().is_empty());
    // A later pass inside the window does not retry the renewal ...
    f.busy("2030-01-01T11:50:00Z");
    f.pool(&["reap"], "2030-01-01T11:50:00Z");
    assert_eq!(f.ledger(), before);
    // ... and the lease end destroys both hosts and settles their reservations.
    let out = f.pool(&["reap"], "2030-01-01T12:00:00Z");
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(f.leases().is_empty());
    assert_eq!(f.terraform("apply").last().unwrap()["hosts"], json!({}));
    let text = f.ledger();
    assert_eq!(
        count(
            r"stop 2030-01-01T12:00:00Z, elapsed 43200 s = 12\.0000 h, est compute \$9\.3756 at \$0\.7813/h; terraform destroy ok; pool pool-\d \(lease_expired\)",
            &text
        ),
        2,
        "{text}"
    );
}

#[test]
fn renew_reserves_the_next_lease_and_settles_the_ended_one() {
    let f = fixture!();
    f.up(1);
    let old = f.leases()["pool-1"].clone();
    f.busy("2030-01-01T11:30:00Z");
    let out = f.pool(&["reap"], "2030-01-01T11:30:00Z");
    assert_eq!(out.code, 0, "{}", out.stderr);
    let lease = f.leases()["pool-1"].clone();
    assert_eq!(lease["lease_no"], json!(2));
    assert_eq!(lease["lease_start"], "2030-01-01T12:00:00Z");
    assert_eq!(lease["lease_expires_at"], "2030-01-02T00:00:00Z");
    assert_ne!(lease["reservation_id"], old["reservation_id"]);
    assert_eq!(lease["renewal"], "scheduled");
    let text = f.ledger();
    let new_id = lease["reservation_id"].as_str().unwrap();
    let old_id = old["reservation_id"].as_str().unwrap();
    // The next lease is reserved from the old expiry at the worst-case rate ...
    assert!(
        Regex::new(&format!(
            r"Gate reservation {new_id}: reserved \${LEASE} until 2030-01-02T00:00:00Z \(start 2030-01-01T12:00:00Z"
        ))
        .unwrap()
        .is_match(&text),
        "{text}"
    );
    // ... the ended lease is settled in full by a line that names its reservation ...
    assert!(
        Regex::new(&format!(
            r"Pool lease 2030-01-01T12:00:00Z: {} ended, .*est compute \${LEASE} at \$0\.7813/h; .*reservation {old_id}\.",
            old["instance_id"].as_str().unwrap()
        ))
        .unwrap()
        .is_match(&text),
        "{text}"
    );
    // ... the host's tags and guest timer both move to the new lease end.
    assert_eq!(
        f.terraform("apply").last().unwrap()["hosts"],
        json!({"pool-1": "2030-01-02T00:00:00Z"})
    );
    let shutdowns = f.shutdowns();
    assert_eq!(shutdowns.len(), 1);
    assert!(
        shutdowns[0].contains("shutdown -h +750"),
        "{}",
        shutdowns[0]
    ); // 11:30 -> 00:00 next day
    // Only the new reservation is still open: the guard sees one lease held.
    let probe = common::run(
        &[
            "spend",
            "reserve",
            "--ledger",
            &f.ledger_path().display().to_string(),
            "--minutes",
            "60",
            "--rate",
            "1",
            "--token",
            "res-probe",
            "--now",
            "2030-01-02T01:00:00Z",
        ],
        "",
    );
    assert_eq!(probe.code, 0, "{}", probe.stderr);
    assert!(
        probe
            .stderr
            .contains(&format!("+ ${:.2} reserved of stop line", 9.3756)),
        "{}",
        probe.stderr
    );
}

#[test]
fn a_busy_lease_far_from_its_end_is_left_alone() {
    let f = fixture!();
    f.up(1);
    f.busy("2030-01-01T05:00:00Z");
    let before = f.ledger();
    let out = f.pool(&["reap"], "2030-01-01T05:00:00Z");
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(f.ledger(), before);
    assert_eq!(f.lease_keys(), ["pool-1"]);
    assert_eq!(f.terraform("apply").len(), 1);
}

#[test]
fn idle_host_exits_after_thirty_minutes_without_a_placed_job() {
    let f = fixture!();
    f.up(2);
    assert_eq!(f.pool(&["touch", "pool-1"], "2030-01-01T00:10:00Z").code, 0);
    let out = f.pool(&["reap"], "2030-01-01T00:31:00Z");
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(f.lease_keys(), ["pool-1"], "pool-2 never had a job");
    assert_eq!(
        f.terraform("apply").last().unwrap()["hosts"],
        json!({"pool-1": "2030-01-01T12:00:00Z"})
    );
    let text = f.ledger();
    assert!(
        Regex::new(r"stop 2030-01-01T00:31:00Z, elapsed 1860 s = 0\.5167 h, est compute \$0\.4037 at \$0\.7813/h; terraform destroy ok; pool pool-2 \(idle_exit\); reservation res-pool-2-")
            .unwrap()
            .is_match(&text),
        "{text}"
    );
    // The busy host exits 30 minutes after its last job.
    f.pool(&["reap"], "2030-01-01T00:39:00Z");
    assert_eq!(f.lease_keys(), ["pool-1"]);
    f.pool(&["reap"], "2030-01-01T00:41:00Z");
    assert!(f.leases().is_empty());
}

#[test]
fn down_destroys_everything_and_settles_every_reservation() {
    let f = fixture!();
    f.up(3);
    let tokens: Vec<String> = f
        .leases()
        .values()
        .map(|lease| lease["reservation_id"].as_str().unwrap().to_string())
        .collect();
    let out = f.pool(&["down"], "2030-01-01T02:00:00Z");
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(f.terraform("destroy").len(), 1);
    assert!(f.leases().is_empty());
    let text = f.ledger();
    for token in &tokens {
        assert!(
            Regex::new(&format!(
                r"est compute \$1\.5626 at \$0\.7813/h; terraform destroy ok; pool pool-\d \(down\); reservation {token}\."
            ))
            .unwrap()
            .is_match(&text),
            "{token}: {text}"
        );
    }
    // Settled reservations free the daily cap again: the whole pool fits once more.
    f.caps(40, 1500);
    f.up_at(3, "2030-01-01T03:00:00Z");
}

#[test]
fn down_keeps_leases_and_reservations_until_the_leftover_check_succeeds() {
    let f = fixture!();
    f.up(2);
    let leases = f.leases();
    let ledger = f.ledger();
    // Even a failed API call that emitted partial output is an unknown result.
    let failed_bin = f.tmp.path().join("failed-check-bin");
    fs::create_dir(&failed_bin).unwrap();
    script(
        &failed_bin,
        "rsi-cloud",
        "#!/bin/sh\n\
if [ \"$1\" = aws ]; then shift; fi\n\
if [ \"$1\" = ec2 ]; then\n\
  printf '%s\\n' \"$FAKE_AWS_FAILURE_OUTPUT\"\n\
  exit 1\n\
fi\n\
exec \"$RSI_CLOUD_REAL\" \"$@\"\n",
    );
    // Exercise both the current aws entry point and #3952's rsi-cloud aws entry point.
    symlink("rsi-cloud", failed_bin.join("aws")).unwrap();
    let path = format!(
        "{}:{}:{}",
        failed_bin.display(),
        f.bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let cloud_bin = failed_bin.join("rsi-cloud");
    let cloud_bin = cloud_bin.to_str().unwrap();
    for partial in ["", "None", "i-0partial"] {
        let out = f.run_with(
            &["down"],
            "2030-01-01T02:00:00Z",
            &[
                ("FAKE_AWS_FAILURE_OUTPUT", partial),
                ("PATH", &path),
                ("RSI_CLOUD_BIN", cloud_bin),
                ("RSI_CLOUD_REAL", env!("CARGO_BIN_EXE_rsi-cloud")),
            ],
        );
        assert_eq!(out.code, 1, "{}", out.stderr);
        assert!(
            out.stderr
                .contains("check unknown; leases kept (reservations stand)"),
            "{}",
            out.stderr
        );
        assert_eq!(f.leases(), leases);
        assert_eq!(f.ledger(), ledger);
    }
    assert_eq!(f.terraform("destroy").len(), 3);

    // A later successful empty check resolves the retained leases and reservations.
    let out = f.run_with(
        &["down"],
        "2030-01-01T03:00:00Z",
        &[("FAKE_AWS_LEFTOVER", "")],
    );
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(f.leases().is_empty());
    assert_eq!(f.terraform("destroy").len(), 4);
    let settled = f.ledger();
    assert_eq!(settled.matches(": stop ").count(), 2, "{settled}");
    for lease in leases.values() {
        assert!(
            settled.contains(&format!(
                "(down); reservation {}.",
                lease["reservation_id"].as_str().unwrap()
            )),
            "{settled}"
        );
    }
}

#[test]
fn down_keeps_leases_and_reservations_when_destroy_fails() {
    let f = fixture!();
    f.up(2);
    let leases = f.leases();
    let ledger = f.ledger();
    fs::remove_file(f.bin.join("terraform")).unwrap();
    script(
        &f.bin,
        "terraform",
        "#!/bin/sh\ncase \"$2\" in init) exit 0;; destroy) exit 1;; *) exit 2;; esac\n",
    );
    let out = f.pool(&["down"], "2030-01-01T02:00:00Z");
    assert_eq!(out.code, 1, "{}", out.stderr);
    assert!(
        out.stderr.contains("terraform destroy failed; leases kept"),
        "{}",
        out.stderr
    );
    assert_eq!(f.leases(), leases);
    assert_eq!(f.ledger(), ledger);
}

#[test]
fn down_kills_a_leftover_pool_instance_and_fails() {
    let f = fixture!();
    f.up(1);
    let out = f.run_with(&["down"], T0, &[("FAKE_AWS_LEFTOVER", "i-0leftover")]);
    assert_eq!(out.code, 1, "{}", out.stderr);
    assert!(
        out.stderr
            .contains("LEFTOVER pool instances after destroy: i-0leftover"),
        "{}",
        out.stderr
    );
    assert!(
        f.calls(Some("aws"))
            .iter()
            .any(|c| c["args"][0] == "ec2" && c["args"][1] == "terminate-instances")
    );
    assert!(f.leases().is_empty());
    assert_eq!(f.ledger().matches(": stop ").count(), 1);
}

#[test]
fn a_spot_interrupted_host_is_settled_and_dropped() {
    let f = fixture!();
    f.up(2);
    let victim = f.leases()["pool-1"]["instance_id"]
        .as_str()
        .unwrap()
        .to_string();
    fs::write(
        f.tmp.path().join("aws-states.json"),
        json!({victim.clone(): {"state": "terminated", "reason": "Server.SpotInstanceTermination"}}).to_string(),
    )
    .unwrap();
    f.pool(&["touch"], T0);
    let out = f.pool(&["reap"], "2030-01-01T00:05:00Z");
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(out.stderr.contains("spot_interrupted"), "{}", out.stderr);
    assert_eq!(f.lease_keys(), ["pool-2"]);
    assert_eq!(f.terraform("apply").len(), 2);
    assert!(
        Regex::new(&format!(
            r"Gate window {victim}: stop 2030-01-01T00:05:00Z.*pool pool-1 \(spot_interrupted\)"
        ))
        .unwrap()
        .is_match(&f.ledger()),
        "{}",
        f.ledger()
    );
}

#[test]
fn status_shows_leases_without_calling_aws() {
    let f = fixture!();
    f.up(1);
    fs::remove_file(&f.log).unwrap();
    let out = f.pool(&["status"], "2030-01-01T01:00:00Z");
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(out.stdout.contains("pool: 1 host(s)"), "{}", out.stdout);
    assert!(out.stdout.contains("pool-1 i-"), "{}", out.stdout);
    assert!(out.stdout.contains("(660 min left)"), "{}", out.stdout);
    assert!(out.stdout.contains("idle 60 min"), "{}", out.stdout);
    assert!(f.calls(None).is_empty());
    let data: Value = serde_json::from_str(&f.pool(&["status", "--json"], T0).stdout).unwrap();
    assert_eq!(
        data["leases"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["pool-1"]
    );
}

#[test]
fn a_failed_tag_move_releases_the_renewal_and_keeps_the_old_expiry() {
    let f = fixture!();
    f.up(1);
    f.busy("2030-01-01T11:30:00Z");
    let out = f.run_with(
        &["reap"],
        "2030-01-01T11:30:00Z",
        &[("FAKE_SSH_FAIL_SHUTDOWN", "1")],
    );
    assert_eq!(out.code, 0, "{}", out.stderr);
    let lease = f.leases()["pool-1"].clone();
    assert_eq!(lease["lease_expires_at"], "2030-01-01T12:00:00Z");
    assert_eq!(lease["renewal"], "refused");
    assert!(
        f.ledger()
            .contains("released, no host started (renewal of pool-1 failed"),
        "{}",
        f.ledger()
    );
}

#[test]
fn a_missing_rsi_cloud_stops_the_script_before_any_spend() {
    let f = fixture!();
    let path = format!(
        "{}:{}",
        f.bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let output = Command::new(repo_root().join("scripts/cloud-pool.sh"))
        .args(["up", "1"])
        .env_clear()
        .env("PATH", path)
        .env("HOME", &f.home)
        .env("RSI_CLOUD_DIR", &f.cloud)
        .env("RSI_GATE_IDENTITY", f.home.join("identity"))
        .env("RSI_CLOUD_BIN", f.home.join("no-such-rsi-cloud"))
        .env("RSI_POOL_NOW", T0)
        .env("FAKE_LOG", &f.log)
        .output()
        .unwrap();
    assert_ne!(output.status.code(), Some(0));
    assert_eq!(f.ledger(), HEADER);
    assert!(f.calls(None).is_empty());
}

/// The Terraform module formats, validates and holds its own limits. These need
/// the terraform CLI (and for validate a provider download); without them the
/// run says so rather than passing silently.
mod module {
    use super::*;

    fn terraform(args: &[&str], cwd: &Path, data: &Path) -> Option<(bool, String)> {
        let output = Command::new("terraform")
            .args(args)
            .current_dir(cwd)
            .env("TF_IN_AUTOMATION", "1")
            .env("TF_DATA_DIR", data)
            .output()
            .ok()?;
        Some((
            output.status.success(),
            format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ),
        ))
    }

    fn copy_dir(from: &Path, to: &Path) {
        fs::create_dir_all(to).unwrap();
        for entry in fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let target = to.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy_dir(&entry.path(), &target);
            } else {
                fs::copy(entry.path(), target).unwrap();
            }
        }
    }

    #[test]
    #[ignore = "requires explicit Terraform/tool-network validation; ordinary cloud tests are offline"]
    fn terraform_fmt_check() {
        let tmp = tempfile::tempdir().unwrap();
        let module = repo_root().join("infra/aws/pool");
        let Some((ok, output)) = terraform(
            &[
                "fmt",
                "-check",
                "-recursive",
                "-no-color",
                &module.display().to_string(),
            ],
            tmp.path(),
            &tmp.path().join("data"),
        ) else {
            eprintln!("runner_unavailable: terraform is not installed");
            return;
        };
        assert!(ok, "{output}");
    }

    #[test]
    #[ignore = "requires explicit Terraform/tool-network validation; ordinary cloud tests are offline"]
    fn terraform_validate_and_module_limits() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let module = tmp.path().join("pool");
        copy_dir(&repo_root().join("infra/aws/pool"), &module);
        let Some((init_ok, _)) = terraform(
            &["init", "-backend=false", "-input=false", "-no-color"],
            &module,
            &data,
        ) else {
            eprintln!("runner_unavailable: terraform is not installed");
            return;
        };
        if !init_ok {
            eprintln!("runner_unavailable: terraform init cannot fetch the aws provider here");
            return;
        }
        let (ok, output) = terraform(&["validate", "-no-color"], &module, &data).unwrap();
        assert!(ok, "{output}");
        // pool.tftest.hcl plans with a mocked provider: 6 hosts plan with spot and the
        // expiry tag on every instance and volume; 7 hosts, another zone, a spot price
        // above the reserved $0.75 cap and a bigger volume are refused.
        let (ok, output) = terraform(&["test", "-no-color"], &module, &data).unwrap();
        assert!(ok, "{output}");
        assert!(output.contains("5 passed"), "{output}");
    }
}

#[test]
fn down_marks_api_error_unknown_and_does_not_terminate_partial_output() {
    let f = fixture!();
    f.up(1);
    let out = f.run_with(&["down"], T0, &[("FAKE_AWS_ERROR", "1")]);
    assert_eq!(out.code, 1, "{}", out.stderr);
    assert!(
        out.stderr
            .contains("LEFTOVER pool instances after destroy: unknown"),
        "{}",
        out.stderr
    );
    let calls = f.calls(Some("aws"));
    assert_eq!(calls.len(), 2); // boot console, then failed leftover describe
    assert_eq!(calls[1]["args"][1], "describe-instances");
}

#[test]
fn gate_marks_api_error_unknown_in_ledger_after_failed_apply() {
    let f = fixture!();
    let path = format!(
        "{}:{}",
        f.bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let output = Command::new(repo_root().join("scripts/cloud-gate.sh"))
        .args(["--run", "--", "true"])
        .env_clear()
        .env("PATH", path)
        .env("HOME", &f.home)
        .env("RSI_CLOUD_DIR", &f.cloud)
        .env("RSI_GATE_IDENTITY", f.home.join("identity"))
        .env("RSI_GATE_CARGO_SLOT", "/usr/bin/env")
        .env("RSI_CLOUD_BIN", f.bin.join("rsi-cloud"))
        .env("RSI_CLOUD_REAL", env!("CARGO_BIN_EXE_rsi-cloud"))
        .env("FAKE_LOG", &f.log)
        .env("FAKE_LEDGER", f.ledger_path())
        .env("FAKE_TF_FAIL_FIRST_APPLY", "partial")
        .env("FAKE_AWS_ERROR", "1")
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("LEFTOVER gate instances after destroy: unknown")
    );
    assert!(
        f.ledger().contains("leftover gate instances: unknown"),
        "{}",
        f.ledger()
    );
    let calls = f.calls(Some("aws"));
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["args"][1], "describe-instances");
}
