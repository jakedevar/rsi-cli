//! Lease state for the warm spot pool: the `pool-state` commands behind
//! `scripts/cloud-pool.sh` (#3767). Each test drives the binary against a temp
//! pool file; nothing touches AWS, the ledger or terraform.

mod common;

use common::{Ran, run};
use serde_json::{Value, json};
use std::fs;

struct Pool {
    dir: tempfile::TempDir,
}

impl Pool {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn file(&self) -> String {
        self.dir.path().join("pool.json").display().to_string()
    }

    fn st(&self, now: Option<&str>, args: &[&str]) -> Ran {
        self.st_in(now, args, "")
    }

    fn st_in(&self, now: Option<&str>, args: &[&str], stdin: &str) -> Ran {
        let file = self.file();
        let mut full = vec!["pool-state", "--file", &file];
        if let Some(now) = now {
            full.extend(["--now", now]);
        }
        full.extend(args);
        run(&full, stdin)
    }

    fn put(&self, slot: &str, pairs: &[&str]) {
        let mut args = vec!["put", slot];
        args.extend(pairs);
        let ran = self.st(None, &args);
        assert_eq!(ran.code, 0, "{}", ran.stderr);
    }

    fn data(&self) -> Value {
        serde_json::from_str(&fs::read_to_string(self.file()).unwrap()).unwrap()
    }

    fn lease(&self, slot: &str) -> Value {
        self.data()["leases"][slot].clone()
    }

    fn standard(&self, slot: &str, start: &str, expires: &str) {
        self.put(
            slot,
            &[
                "state=ready",
                "lease_no=1",
                &format!("lease_start={start}"),
                &format!("lease_expires_at={expires}"),
                "hourly_usd=0.7813",
                "reservation_id=res-x",
                "instance_id=i-01",
                "host=ec2-user@192.0.2.1",
            ],
        );
    }
}

const T0: &str = "2030-01-01T00:00:00Z";
const T12: &str = "2030-01-01T12:00:00Z";

#[test]
fn put_creates_a_lease_and_types_numbers() {
    let pool = Pool::new();
    pool.put(
        "pool-2",
        &[
            "lease_no=3",
            "hourly_usd=0.7813",
            "host=ec2-user@192.0.2.2",
            "note=a=b",
            "empty=",
        ],
    );
    let lease = pool.lease("pool-2");
    assert_eq!(lease["slot"], "pool-2");
    assert_eq!(lease["lease_no"], json!(3));
    assert_eq!(lease["hourly_usd"], json!(0.7813));
    assert_eq!(lease["host"], "ec2-user@192.0.2.2");
    assert_eq!(lease["note"], "a=b", "only the first = splits");
    assert_eq!(lease["empty"], "");
    // Updating keeps the other fields.
    pool.put("pool-2", &["lease_no=4"]);
    assert_eq!(pool.lease("pool-2")["host"], "ec2-user@192.0.2.2");
    assert_eq!(pool.lease("pool-2")["lease_no"], json!(4));
    assert_eq!(pool.data()["version"], json!(1));
    // The file is sorted two-space JSON with a trailing newline, and no temp file is left.
    let text = fs::read_to_string(pool.file()).unwrap();
    assert!(
        text.ends_with("}\n") && text.contains("\n  \"leases\""),
        "{text}"
    );
    let leftovers: Vec<_> = fs::read_dir(pool.dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".tmp."))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[test]
fn an_unknown_slot_is_refused() {
    let pool = Pool::new();
    let ran = pool.st(None, &["put", "pool-7", "a=b"]);
    assert_eq!(ran.code, 2);
    assert!(ran.stderr.contains("unknown slot pool-7"));
    assert!(!std::path::Path::new(&pool.file()).exists());
}

#[test]
fn get_prints_a_field_and_exits_one_when_absent() {
    let pool = Pool::new();
    pool.put(
        "pool-1",
        &["lease_no=2", "hourly_usd=0.7813", "instance_id=i-0abc"],
    );
    let ran = pool.st(None, &["get", "pool-1", "instance_id"]);
    assert_eq!((ran.code, ran.stdout.as_str()), (0, "i-0abc\n"));
    assert_eq!(pool.st(None, &["get", "pool-1", "lease_no"]).stdout, "2\n");
    assert_eq!(
        pool.st(None, &["get", "pool-1", "hourly_usd"]).stdout,
        "0.7813\n"
    );
    assert_eq!(pool.st(None, &["get", "pool-1", "nope"]).code, 1);
    assert_eq!(pool.st(None, &["get", "pool-2", "instance_id"]).code, 1);
}

#[test]
fn slots_hands_out_the_free_names_and_enforces_the_pool_limit() {
    let pool = Pool::new();
    let ran = pool.st(None, &["slots", "--want", "2"]);
    assert_eq!(
        (ran.code, ran.stdout.as_str()),
        (0, "have=0\nnew=pool-1 pool-2\n")
    );
    pool.put("pool-1", &["state=ready"]);
    pool.put("pool-3", &["state=ready"]);
    // Two held, three wanted: the one free name that fills the gap is pool-2.
    assert_eq!(
        pool.st(None, &["slots", "--want", "3"]).stdout,
        "have=2\nnew=pool-2\n"
    );
    assert_eq!(
        pool.st(None, &["slots", "--want", "2"]).stdout,
        "have=2\nnew=\n"
    );
    assert_eq!(
        pool.st(None, &["slots", "--want", "1"]).stdout,
        "have=2\nnew=\n"
    );
    assert_eq!(
        pool.st(None, &["slots", "--want", "6"]).stdout,
        "have=2\nnew=pool-2 pool-4 pool-5 pool-6\n"
    );
    for want in ["0", "7", "-1", "12"] {
        let ran = pool.st(None, &["slots", "--want", want]);
        assert_eq!(ran.code, 2, "{want}");
        assert!(
            ran.stderr.contains("the pool holds 1 to 6 hosts"),
            "{}",
            ran.stderr
        );
    }
}

#[test]
fn list_and_drop_follow_slot_order() {
    let pool = Pool::new();
    pool.put("pool-3", &["a=1"]);
    pool.put("pool-1", &["a=1"]);
    pool.put("pool-2", &["a=1"]);
    assert_eq!(pool.st(None, &["list"]).stdout, "pool-1\npool-2\npool-3\n");
    assert_eq!(pool.st(None, &["drop", "pool-2", "pool-9"]).code, 0);
    assert_eq!(pool.st(None, &["list"]).stdout, "pool-1\npool-3\n");
    assert_eq!(pool.st(None, &["drop", "pool-1", "pool-3"]).code, 0);
    assert_eq!(pool.st(None, &["list"]).stdout, "");
}

#[test]
fn touch_records_job_activity_on_one_or_every_lease() {
    let pool = Pool::new();
    pool.standard("pool-1", T0, T12);
    pool.standard("pool-2", T0, T12);
    assert_eq!(
        pool.st(Some("2030-01-01T01:00:00Z"), &["touch", "pool-1"])
            .code,
        0
    );
    assert_eq!(pool.lease("pool-1")["last_job_at"], "2030-01-01T01:00:00Z");
    assert!(pool.lease("pool-2").get("last_job_at").is_none());
    assert_eq!(pool.st(Some("2030-01-01T02:00:00Z"), &["touch"]).code, 0);
    assert_eq!(pool.lease("pool-1")["last_job_at"], "2030-01-01T02:00:00Z");
    assert_eq!(pool.lease("pool-2")["last_job_at"], "2030-01-01T02:00:00Z");
    let before = fs::read_to_string(pool.file()).unwrap();
    let ran = pool.st(Some("2030-01-01T03:00:00Z"), &["touch", "pool-4"]);
    assert_eq!(ran.code, 1);
    assert!(ran.stderr.contains("no lease for pool-4"));
    assert_eq!(
        fs::read_to_string(pool.file()).unwrap(),
        before,
        "a refused touch writes nothing"
    );
}

#[test]
fn due_names_what_expires_idles_or_needs_renewal() {
    let pool = Pool::new();
    pool.standard("pool-1", T0, T12); // busy
    pool.standard("pool-2", T0, T12); // never gets a job
    pool.standard("pool-3", T0, T12); // refused renewal
    pool.put("pool-3", &["renewal=refused"]);
    // Early on a fresh lease nothing is due; the idle clock counts from the lease start.
    assert_eq!(pool.st(Some("2030-01-01T00:29:00Z"), &["due"]).stdout, "");
    assert_eq!(
        pool.st(Some("2030-01-01T00:30:00Z"), &["due"]).stdout,
        "pool-1 idle\npool-2 idle\npool-3 idle\n"
    );
    // The thresholds are options.
    assert_eq!(
        pool.st(
            Some("2030-01-01T00:30:00Z"),
            &["due", "--idle-minutes", "45"]
        )
        .stdout,
        ""
    );
    assert_eq!(
        pool.st(
            Some("2030-01-01T09:30:00Z"),
            &["due", "--idle-minutes", "9999", "--renew-window", "180"]
        )
        .stdout,
        "pool-1 renew\npool-2 renew\n",
        "pool-3 refused its renewal"
    );
    pool.st(Some("2030-01-01T11:10:00Z"), &["touch", "pool-1"]);
    pool.st(Some("2030-01-01T11:20:00Z"), &["touch", "pool-3"]);
    // 11:30: pool-2 idle (no job all day), pool-1 inside the 60 min renew window, pool-3 refused.
    let ran = pool.st(Some("2030-01-01T11:30:00Z"), &["due"]);
    assert_eq!(ran.stdout, "pool-1 renew\npool-2 idle\n", "{}", ran.stdout);
    // At the lease end everything expires, whatever else is true.
    let ran = pool.st(Some(T12), &["due"]);
    assert_eq!(ran.stdout, "pool-1 expire\npool-2 expire\npool-3 expire\n");
}

#[test]
fn status_lists_leases_with_remaining_and_idle_time() {
    let pool = Pool::new();
    assert_eq!(pool.st(Some(T0), &["status"]).stdout, "pool: no hosts\n");
    pool.standard("pool-1", T0, T12);
    pool.put(
        "pool-2",
        &[
            "lease_start=2030-01-01T00:00:00Z",
            "lease_expires_at=2030-01-01T12:00:00Z",
            "renewal=refused",
            "renewal_note=daily cap",
            "lease_no=2",
        ],
    );
    let ran = pool.st(Some("2030-01-01T01:00:00Z"), &["status"]);
    assert_eq!(ran.code, 0, "{}", ran.stderr);
    let lines: Vec<&str> = ran.stdout.lines().collect();
    assert_eq!(lines[0], "pool: 2 host(s) at 2030-01-01T01:00:00Z");
    assert_eq!(
        lines[1],
        "pool-1 i-01 ec2-user@192.0.2.1 ready lease 1 until 2030-01-01T12:00:00Z (660 min left) idle 60 min renewal scheduled reservation res-x"
    );
    assert_eq!(
        lines[2],
        "pool-2 - - ready lease 2 until 2030-01-01T12:00:00Z (660 min left) idle 60 min renewal refused (daily cap) reservation -"
    );
    let json: Value = serde_json::from_str(&pool.st(None, &["status", "--json"]).stdout).unwrap();
    assert_eq!(json["leases"].as_object().unwrap().len(), 2);
}

#[test]
fn shift_adds_minutes_and_until_rounds_up() {
    let pool = Pool::new();
    assert_eq!(
        pool.st(None, &["shift", T0, "720"]).stdout,
        "2030-01-01T12:00:00Z\n"
    );
    assert_eq!(
        pool.st(None, &["shift", T12, "720"]).stdout,
        "2030-01-02T00:00:00Z\n"
    );
    assert_eq!(
        pool.st(
            Some("2030-01-01T11:30:00Z"),
            &["until", "2030-01-02T00:00:00Z"]
        )
        .stdout,
        "750\n"
    );
    assert_eq!(
        pool.st(
            Some("2030-01-01T11:30:00Z"),
            &["until", "2030-01-01T11:30:01Z"]
        )
        .stdout,
        "1\n"
    );
    assert_eq!(
        pool.st(
            Some("2030-01-01T11:30:00Z"),
            &["until", "2030-01-01T11:31:01Z"]
        )
        .stdout,
        "2\n"
    );
    assert_eq!(
        pool.st(
            Some("2030-01-01T11:30:00Z"),
            &["until", "2030-01-01T11:00:00Z"]
        )
        .stdout,
        "1\n",
        "past times still wait a minute"
    );
    assert_eq!(pool.st(None, &["shift", "garbage", "5"]).code, 1);
}

#[test]
fn hosts_json_maps_slots_to_expiry_with_drops_and_overrides() {
    let pool = Pool::new();
    pool.standard("pool-1", T0, T12);
    pool.standard("pool-2", T0, "2030-01-01T06:00:00Z");
    let out = pool.dir.path().join("hosts.json").display().to_string();
    assert_eq!(pool.st(None, &["hosts-json", "--out", &out]).code, 0);
    let read =
        |path: &str| -> Value { serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap() };
    assert_eq!(
        read(&out),
        json!({"hosts": {"pool-1": T12, "pool-2": "2030-01-01T06:00:00Z"}})
    );
    assert_eq!(
        pool.st(None, &["hosts-json", "--out", &out, "--drop", "pool-2"])
            .code,
        0
    );
    assert_eq!(read(&out), json!({"hosts": {"pool-1": T12}}));
    let ran = pool.st(
        None,
        &[
            "hosts-json",
            "--out",
            &out,
            "--set",
            "pool-1=2030-01-02T00:00:00Z",
            "pool-3=2030-01-02T00:00:00Z",
            "--drop",
            "pool-2",
        ],
    );
    assert_eq!(ran.code, 0, "{}", ran.stderr);
    assert_eq!(
        read(&out),
        json!({"hosts": {"pool-1": "2030-01-02T00:00:00Z", "pool-3": "2030-01-02T00:00:00Z"}})
    );
    // The pool file itself is untouched by hosts-json.
    assert_eq!(pool.lease("pool-1")["lease_expires_at"], T12);
}

#[test]
fn cost_prices_the_elapsed_lease_time() {
    let pool = Pool::new();
    pool.standard("pool-1", T0, T12);
    assert_eq!(
        pool.st(None, &["cost", "pool-1", T12]).stdout,
        "43200 9.3756\n"
    );
    assert_eq!(
        pool.st(None, &["cost", "pool-1", "2030-01-01T00:31:00Z"])
            .stdout,
        "1860 0.4037\n"
    );
    assert_eq!(
        pool.st(None, &["cost", "pool-1", "2029-12-31T00:00:00Z"])
            .stdout,
        "0 0.0000\n",
        "never negative"
    );
    assert_eq!(pool.st(None, &["cost", "pool-5", T12]).code, 1);
}

#[test]
fn terraform_and_aws_json_helpers() {
    let pool = Pool::new();
    let hosts = r#"{"pool-1": {"instance_id": "i-0abc", "public_ip": "192.0.2.1", "n": 3}}"#;
    let field = |slot: &str, name: &str| pool.st_in(None, &["output-field", slot, name], hosts);
    assert_eq!(field("pool-1", "instance_id").stdout, "i-0abc\n");
    assert_eq!(field("pool-1", "n").stdout, "3\n");
    assert_eq!(field("pool-1", "missing").code, 1);
    assert_eq!(field("pool-2", "instance_id").code, 1);
    assert_eq!(
        pool.st_in(None, &["output-field", "pool-1", "x"], "not json")
            .code,
        1
    );
    let state = |json: &str| pool.st_in(None, &["instance-state"], json);
    assert_eq!(
        state(r#"{"Reservations": [{"Instances": [{"State": {"Name": "running"}}]}]}"#).stdout,
        "running\n"
    );
    for bad in [
        "{}",
        "not json",
        r#"{"Reservations": []}"#,
        "[]",
        r#"{"Reservations": [{"Instances": [{}]}]}"#,
    ] {
        let ran = state(bad);
        assert_eq!((ran.code, ran.stdout.as_str()), (0, "unknown\n"), "{bad}");
    }
}

#[test]
fn a_file_that_is_not_a_pool_file_is_refused_and_left_alone() {
    let pool = Pool::new();
    fs::write(pool.file(), r#"{"leases": []}"#).unwrap();
    for args in [
        &["list"][..],
        &["status"][..],
        &["put", "pool-1", "a=b"][..],
    ] {
        let ran = pool.st(None, args);
        assert_eq!(ran.code, 1, "{args:?}");
        assert!(ran.stderr.contains("is not a pool file"), "{}", ran.stderr);
    }
    assert_eq!(
        fs::read_to_string(pool.file()).unwrap(),
        r#"{"leases": []}"#
    );
    fs::write(pool.file(), "garbage").unwrap();
    assert_eq!(pool.st(None, &["list"]).code, 1);
}

#[test]
fn concurrent_puts_never_lose_an_update() {
    let pool = Pool::new();
    let file = pool.file();
    let handles: Vec<_> = (1..=6)
        .map(|n| {
            let file = file.clone();
            std::thread::spawn(move || {
                let slot = format!("pool-{n}");
                run(
                    &["pool-state", "--file", &file, "put", &slot, "state=ready"],
                    "",
                )
                .code
            })
        })
        .collect();
    for handle in handles {
        assert_eq!(handle.join().unwrap(), 0);
    }
    assert_eq!(pool.st(None, &["list"]).stdout.lines().count(), 6);
}

#[test]
fn global_options_must_precede_the_command_and_usage_errors_exit_two() {
    let pool = Pool::new();
    assert_eq!(run(&["pool-state"], "").code, 2);
    assert_eq!(run(&["pool-state", "--file", &pool.file()], "").code, 2);
    assert_eq!(
        run(&["pool-state", "--file", &pool.file(), "frobnicate"], "").code,
        2
    );
    assert_eq!(run(&["pool-state", "--file"], "").code, 2);
    assert_eq!(
        run(
            &[
                "pool-state",
                "--file",
                &pool.file(),
                "--now",
                "yesterday",
                "list"
            ],
            ""
        )
        .code,
        1
    );
}
