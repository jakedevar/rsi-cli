//! The spend guard reads the operator caps (or the ledger header) and fails
//! closed (ported from scripts/tests/test_cloud_spend.py, #3767). The
//! binary runs against temp ledgers; no AWS call is made.

mod common;

use common::{HEADER, repo_root, run};
use std::fs;
use std::path::Path;

fn window(cost: &str) -> String {
    format!(
        "Gate window i-0abc: stop 2026-09-28T09:09:14Z, elapsed 1 s, est compute ${cost} at $1.7808/h; ok.\n"
    )
}

fn reservation(token: &str, amount: &str, start: &str) -> String {
    format!(
        "Gate reservation {token}: reserved ${amount} until 2030-01-01T10:00:00Z (start {start}, 120 min at $0.7813/h worst case).\n"
    )
}

fn stop_line(token: &str, cost: &str) -> String {
    format!(
        "Gate window i-0abc: stop 2030-01-01T09:00:00Z, elapsed 3600 s = 1.0000 h, est compute ${cost} at $0.7813/h; terraform destroy ok; leftover gate instances: none; sccache: off (cold run); reservation {token}.\n"
    )
}

struct Env {
    dir: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn ledger(&self) -> String {
        self.dir.path().join("spend.md").display().to_string()
    }

    fn caps(&self, caps: Option<&str>) {
        if let Some(caps) = caps {
            fs::write(self.dir.path().join("spend-caps.json"), caps).unwrap();
        }
    }

    fn check(&self, text: &str, caps: Option<&str>, today: &str) -> common::Ran {
        fs::write(self.dir.path().join("spend.md"), text).unwrap();
        self.caps(caps);
        run(
            &[
                "spend",
                "check",
                "--ledger",
                &self.ledger(),
                "--today",
                today,
            ],
            "",
        )
    }

    fn reserve(&self, text: &str, caps: Option<&str>, minutes: i64, now: &str) -> common::Ran {
        fs::write(self.dir.path().join("spend.md"), text).unwrap();
        self.caps(caps);
        run(
            &[
                "spend",
                "reserve",
                "--ledger",
                &self.ledger(),
                "--minutes",
                &minutes.to_string(),
                "--rate",
                "0.7813",
                "--now",
                now,
            ],
            "",
        )
    }

    fn reserve_token(&self, text: &str, caps: Option<&str>, token: &str) -> common::Ran {
        fs::write(self.dir.path().join("spend.md"), text).unwrap();
        self.caps(caps);
        run(
            &[
                "spend",
                "reserve",
                "--ledger",
                &self.ledger(),
                "--minutes",
                "120",
                "--rate",
                "0.7813",
                "--now",
                "2030-01-01T12:00:00Z",
                "--token",
                token,
            ],
            "",
        )
    }

    fn text(&self) -> String {
        fs::read_to_string(self.dir.path().join("spend.md")).unwrap()
    }
}

const CAPS_200_25: &str = r#"{"stop_line_usd": 200, "daily_cap_usd": 25}"#;
const NOON: &str = "2030-01-01T12:00:00Z";

#[test]
fn below_stop_line_allows_a_run() {
    let env = Env::new();
    let result = env.check(
        &format!("{HEADER}{}{}", window("40.0000"), window("49.9999")),
        None,
        "2030-01-01",
    );
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert!(result.stderr.contains("of $100.00 grant, stop line $90.00"));
}

#[test]
fn at_the_stop_line_refuses() {
    let env = Env::new();
    let result = env.check(
        &format!("{HEADER}{}{}", window("45.0000"), window("45.0000")),
        None,
        "2030-01-01",
    );
    assert_eq!(result.code, 4);
    assert!(result.stderr.contains("stop line $90.00"));
    assert!(
        result
            .stderr
            .contains("at or above the stop line, refusing to start a remote run")
    );
}

#[test]
fn stop_line_comes_from_the_header_not_the_binary() {
    let env = Env::new();
    let text = format!(
        "{}{}",
        HEADER.replace("by $90 cumulative", "by $5 cumulative"),
        window("5.0000")
    );
    assert_eq!(env.check(&text, None, "2030-01-01").code, 4);
}

#[test]
fn operator_caps_file_overrides_the_header_stop_line() {
    let env = Env::new();
    let caps = r#"{"stop_line_usd": 5, "daily_cap_usd": 100}"#;
    let result = env.check(
        &format!("{HEADER}{}", window("6.0000")),
        Some(caps),
        "2030-01-01",
    );
    assert_eq!(result.code, 4);
    assert!(result.stderr.contains("stop line $5.00"));
    let raised = env.check(
        &format!("{HEADER}{}", window("95.0000")),
        Some(r#"{"stop_line_usd": 200, "daily_cap_usd": 100}"#),
        "2030-01-01",
    );
    assert_eq!(raised.code, 0, "{}", raised.stderr);
}

#[test]
fn daily_cap_refuses_only_for_the_current_utc_day() {
    let env = Env::new();
    let text = format!("{HEADER}{}{}", window("9.0000"), window("7.0000"));
    let caps = r#"{"stop_line_usd": 90, "daily_cap_usd": 15}"#;
    let on_the_day = env.check(&text, Some(caps), "2026-09-28");
    assert_eq!(on_the_day.code, 4);
    assert!(on_the_day.stderr.contains("of $15.00 daily cap"));
    assert!(on_the_day.stderr.contains("at or above the daily cap"));
    assert_eq!(env.check(&text, Some(caps), "2026-09-29").code, 0);
    let raised = env.check(
        &text,
        Some(r#"{"stop_line_usd": 90, "daily_cap_usd": 20}"#),
        "2026-09-28",
    );
    assert_eq!(raised.code, 0, "{}", raised.stderr);
}

#[test]
fn daily_cap_defaults_to_fifteen_without_a_caps_file() {
    let env = Env::new();
    assert_eq!(
        env.check(
            &format!("{HEADER}{}", window("15.0000")),
            None,
            "2026-09-28"
        )
        .code,
        4
    );
    assert_eq!(
        env.check(
            &format!("{HEADER}{}", window("14.9000")),
            None,
            "2026-09-28"
        )
        .code,
        0
    );
}

#[test]
fn unreadable_caps_file_fails_closed() {
    let env = Env::new();
    let text = format!("{HEADER}{}", window("1.0000"));
    assert_eq!(env.check(&text, Some("not json"), "2030-01-01").code, 4);
    // A caps file missing a field, or holding a non-number, is unreadable too.
    assert_eq!(
        env.check(&text, Some(r#"{"stop_line_usd": 90}"#), "2030-01-01")
            .code,
        4
    );
    assert_eq!(
        env.check(
            &text,
            Some(r#"{"stop_line_usd": "nan", "daily_cap_usd": 15}"#),
            "2030-01-01"
        )
        .code,
        4
    );
    assert_eq!(env.check(&text, Some("[1, 2]"), "2030-01-01").code, 4);
}

#[test]
fn unreadable_header_fails_closed() {
    let env = Env::new();
    assert_eq!(
        env.check(
            &format!("no header here\n{}", window("1.0000")),
            None,
            "2030-01-01"
        )
        .code,
        4
    );
    let missing = env.dir.path().join("missing.md").display().to_string();
    assert_eq!(run(&["spend", "check", "--ledger", &missing], "").code, 4);
}

#[test]
fn real_ledger_shape_sums_gate_and_window_lines() {
    let env = Env::new();
    let mut text = HEADER.to_string();
    text.push_str("Window 1 stop confirmed: elapsed 533 seconds, estimated compute $0.5275 at configured $3.5616/h.\n");
    text.push_str(&window("2.0000"));
    let result = env.check(&text, None, "2030-01-01");
    assert_eq!(result.code, 0);
    assert!(
        result
            .stderr
            .contains("cumulative estimate $2.00 of $100.00")
    );
}

#[test]
fn reservation_is_written_and_priced_at_the_spot_reserve_rate() {
    let env = Env::new();
    let result = env.reserve_token(HEADER, Some(CAPS_200_25), "res-a1");
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert_eq!(result.stdout.trim(), "reservation_id=res-a1");
    let text = env.text();
    let line = text.lines().last().unwrap();
    assert!(
        line.contains("Gate reservation res-a1: reserved $1.5626 until 2030-01-01T14:00:00Z"),
        "{line}"
    );
    assert!(
        line.contains("(start 2030-01-01T12:00:00Z, 120 min at $0.7813/h worst case)"),
        "{line}"
    );
    // A reservation is not an "est compute" figure: the daemon's spend sum is unchanged.
    assert!(!line.contains("est compute"));
}

#[test]
fn a_default_token_is_generated_and_valid() {
    let env = Env::new();
    let result = env.reserve(HEADER, Some(CAPS_200_25), 60, NOON);
    assert_eq!(result.code, 0, "{}", result.stderr);
    let id = result
        .stdout
        .trim()
        .strip_prefix("reservation_id=")
        .unwrap()
        .to_string();
    assert!(id.starts_with("res-"), "{id}");
    assert!(
        env.text()
            .contains(&format!("Gate reservation {id}: reserved $0.7813"))
    );
}

#[test]
fn invalid_tokens_are_refused_without_writing() {
    let env = Env::new();
    for token in ["bad", "res-", "res-a b", "res-a\n", "res-a:b"] {
        let result = env.reserve_token(HEADER, Some(CAPS_200_25), token);
        assert_eq!(result.code, 4, "{token:?}: {}", result.stderr);
        assert_eq!(env.text(), HEADER, "{token:?}");
    }
}

#[test]
fn reservation_refused_at_the_daily_cap_and_writes_nothing() {
    let env = Env::new();
    let text = format!(
        "{HEADER}{}",
        window("23.5000").replace("2026-09-28", "2030-01-01")
    );
    let refused = env.reserve(&text, Some(CAPS_200_25), 120, NOON); // 23.50 + 1.5626 > 25
    assert_eq!(refused.code, 4);
    assert!(
        refused
            .stderr
            .contains("would pass the daily cap, refusing to start a remote run")
    );
    assert_eq!(env.text(), text, "a refused reservation appends nothing");
    let ok_text = format!(
        "{HEADER}{}",
        window("23.4000").replace("2026-09-28", "2030-01-01")
    );
    let ok = env.reserve(&ok_text, Some(CAPS_200_25), 120, NOON); // 24.9626 <= 25
    assert_eq!(ok.code, 0, "{}", ok.stderr);
    assert_eq!(
        env.reserve(&text, Some(CAPS_200_25), 120, "2030-01-02T00:00:01Z")
            .code,
        0,
        "yesterday's spend does not count against today's cap"
    );
}

#[test]
fn reservation_refused_by_open_reservations_against_the_daily_cap() {
    let env = Env::new();
    let caps = r#"{"stop_line_usd": 500, "daily_cap_usd": 25}"#;
    let many = |count: usize| {
        let reserved: String = (0..count)
            .map(|n| reservation(&format!("res-{n}"), "1.5626", "2030-01-01T08:00:00Z"))
            .collect();
        format!("{HEADER}{reserved}")
    };
    assert_eq!(env.reserve(&many(15), Some(caps), 120, NOON).code, 4); // 23.439 + 1.5626
    assert_eq!(env.reserve(&many(14), Some(caps), 120, NOON).code, 0);
}

#[test]
fn reservation_refused_at_the_stop_line() {
    let env = Env::new();
    let caps = r#"{"stop_line_usd": 200, "daily_cap_usd": 500}"#;
    let refused = env.reserve(
        &format!("{HEADER}{}", window("198.5000")),
        Some(caps),
        120,
        NOON,
    );
    assert_eq!(refused.code, 4);
    assert!(
        refused
            .stderr
            .contains("would pass the stop line, refusing to start a remote run")
    );
    assert_eq!(
        env.reserve(
            &format!("{HEADER}{}", window("198.4000")),
            Some(caps),
            120,
            NOON
        )
        .code,
        0
    );
    let held = format!(
        "{HEADER}{}{}",
        window("100.0000"),
        reservation("res-x", "99.0000", "2030-01-01T08:00:00Z")
    );
    assert_eq!(
        env.reserve(&held, Some(caps), 120, NOON).code,
        4,
        "an open reservation counts toward the stop line"
    );
}

#[test]
fn a_stop_line_replaces_its_reservation() {
    let env = Env::new();
    let caps = r#"{"stop_line_usd": 200, "daily_cap_usd": 5}"#;
    let open = format!(
        "{HEADER}{}",
        reservation("res-b", "4.0000", "2030-01-01T08:00:00Z")
    );
    assert_eq!(
        env.reserve(&open, Some(caps), 120, NOON).code,
        4,
        "4.00 + 1.5626 > 5"
    );
    let closed = format!("{open}{}", stop_line("res-b", "0.6000"));
    let result = env.reserve(&closed, Some(caps), 120, NOON); // the actual 0.60 replaces the 4.00
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert!(
        result
            .stderr
            .contains("$0.60 + $0.00 reserved of $5.00 daily cap"),
        "{}",
        result.stderr
    );
}

#[test]
fn a_released_reservation_no_longer_counts() {
    let env = Env::new();
    let caps = r#"{"stop_line_usd": 200, "daily_cap_usd": 5}"#;
    let released = format!(
        "{HEADER}{}Gate reservation res-c: released, no host started.\n",
        reservation("res-c", "4.0000", "2030-01-01T08:00:00Z")
    );
    assert_eq!(env.reserve(&released, Some(caps), 120, NOON).code, 0);
}

#[test]
fn a_missing_stop_line_keeps_the_reservation_against_the_stop_line() {
    // A hub crash wrote no stop line and the next day has begun: the day cap
    // forgets it, the cumulative stop line does not.
    let env = Env::new();
    let caps = r#"{"stop_line_usd": 10, "daily_cap_usd": 500}"#;
    let text = format!(
        "{HEADER}{}",
        reservation("res-d", "9.0000", "2029-12-31T08:00:00Z")
    );
    let refused = env.reserve(&text, Some(caps), 120, NOON);
    assert_eq!(refused.code, 4);
    assert!(refused.stderr.contains("would pass the stop line"));
    let daily = env.reserve(
        &format!(
            "{HEADER}{}",
            reservation("res-d", "24.0000", "2029-12-31T08:00:00Z")
        ),
        Some(r#"{"stop_line_usd": 500, "daily_cap_usd": 25}"#),
        120,
        NOON,
    );
    assert_eq!(
        daily.code, 0,
        "the reservation is charged to its start's UTC day"
    );
}

#[test]
fn reserve_fails_closed_on_an_unreadable_ledger_or_caps_file_or_bad_numbers() {
    let env = Env::new();
    assert_eq!(
        env.reserve("no header\n", Some(CAPS_200_25), 120, NOON)
            .code,
        4
    );
    assert_eq!(env.reserve(HEADER, Some("not json"), 120, NOON).code, 4);
    assert_eq!(env.reserve(HEADER, Some(CAPS_200_25), 0, NOON).code, 4);
    assert_eq!(env.reserve(HEADER, Some(CAPS_200_25), -5, NOON).code, 4);
    // A rate of zero, a negative rate and a non-finite rate reserve nothing.
    for rate in ["0", "-1", "nan", "inf"] {
        fs::write(env.dir.path().join("spend.md"), HEADER).unwrap();
        let result = run(
            &[
                "spend",
                "reserve",
                "--ledger",
                &env.ledger(),
                "--minutes",
                "60",
                "--rate",
                rate,
                "--now",
                NOON,
            ],
            "",
        );
        assert_eq!(result.code, 4, "rate {rate}: {}", result.stderr);
        assert_eq!(env.text(), HEADER);
    }
    // A missing ledger is unreadable, not empty.
    let gone = env.dir.path().join("gone/spend.md").display().to_string();
    let result = run(
        &[
            "spend",
            "reserve",
            "--ledger",
            &gone,
            "--minutes",
            "60",
            "--rate",
            "1",
            "--now",
            NOON,
        ],
        "",
    );
    assert_eq!(result.code, 4);
}

#[test]
fn concurrent_reservations_never_pass_the_daily_cap() {
    // Ten racers each ask for a $1.5626 lease against a $5 day: the lock makes
    // check-and-append atomic, so exactly three win.
    let env = Env::new();
    fs::write(env.dir.path().join("spend.md"), HEADER).unwrap();
    env.caps(Some(r#"{"stop_line_usd": 500, "daily_cap_usd": 5}"#));
    let ledger = env.ledger();
    let handles: Vec<_> = (0..10)
        .map(|n| {
            let ledger = ledger.clone();
            std::thread::spawn(move || {
                run(
                    &[
                        "spend",
                        "reserve",
                        "--ledger",
                        &ledger,
                        "--minutes",
                        "120",
                        "--rate",
                        "0.7813",
                        "--now",
                        NOON,
                        "--token",
                        &format!("res-race-{n}"),
                    ],
                    "",
                )
                .code
            })
        })
        .collect();
    let codes: Vec<i32> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(codes.iter().filter(|c| **c == 0).count(), 3, "{codes:?}");
    assert!(codes.iter().all(|c| *c == 0 || *c == 4), "{codes:?}");
    assert_eq!(env.text().matches("Gate reservation res-race-").count(), 3);
}

fn spot(data: &str) -> common::Ran {
    run(&["spend", "spot-reason"], data)
}

fn gone(code: &str) -> String {
    format!(r#"{{"Reservations": [{{"Instances": [{{"StateReason": {{"Code": "{code}"}}}}]}}]}}"#)
}

#[test]
fn spot_reason_names_a_spot_interruption_only() {
    let hit = spot(&gone("Server.SpotInstanceTermination"));
    assert_eq!((hit.code, hit.stdout.trim()), (0, "spot_interrupted"));
    assert_eq!(spot(&gone("Client.UserInitiatedShutdown")).code, 1);
    assert_eq!(spot(r#"{"Reservations": []}"#).code, 1);
    assert_eq!(spot("not json").code, 1);
    assert_eq!(spot("[]").code, 1);
    assert_eq!(spot(r#"{"Reservations": null}"#).code, 1);
}

#[test]
fn gate_wrapper_wires_the_reservation_before_apply_and_the_spot_exit() {
    let gate = fs::read_to_string(repo_root().join("scripts/cloud-gate.sh")).unwrap();
    let reserve = gate
        .find("reservation_id=$(spend reserve")
        .expect("gate reserves");
    assert!(reserve < gate.find("tf apply").expect("gate applies"));
    assert!(gate.contains("hourly_usd=${RSI_GATE_HOURLY_USD:-0.7813}"));
    assert!(gate.contains("max_lifetime_minutes=120"));
    assert!(gate.contains("spend spot-reason"));
    assert!(gate.contains("spend check --ledger"));
    assert!(!gate.contains("python"), "the gate no longer needs Python");
}

fn cache(stats: &str, out: Option<&Path>) -> common::Ran {
    match out {
        Some(out) => run(
            &["spend", "cache", "--json-out", &out.display().to_string()],
            stats,
        ),
        None => run(&["spend", "cache"], stats),
    }
}

#[test]
fn cache_summary_reports_hits_misses_and_rate() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("cache.json");
    let stats = r#"{"stats": {"cache_hits": {"counts": {"Rust": 90, "C/C++": 10}}, "cache_misses": {"counts": {"Rust": 25}}}}"#;
    let result = cache(stats, Some(&out));
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert_eq!(
        result.stdout.trim(),
        "sccache_hits=100 sccache_misses=25 sccache_hit_rate=80.0%"
    );
    let written: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&out).unwrap()).unwrap();
    assert_eq!(
        written,
        serde_json::json!({"hits": 100, "misses": 25, "hit_rate_percent": 80.0})
    );
}

#[test]
fn cache_summary_of_a_cold_run_is_zero_not_an_error() {
    let result = cache(
        r#"{"stats": {"cache_hits": {"counts": {}}, "cache_misses": {"counts": {}}}}"#,
        None,
    );
    assert_eq!(
        result.stdout.trim(),
        "sccache_hits=0 sccache_misses=0 sccache_hit_rate=0.0%"
    );
    assert_eq!(
        cache("{}", None).stdout.trim(),
        "sccache_hits=0 sccache_misses=0 sccache_hit_rate=0.0%"
    );
}

#[test]
fn cache_summary_rejects_garbage() {
    assert_eq!(cache("not json", None).code, 1);
    assert_eq!(cache("[1]", None).code, 1);
    assert_eq!(cache(r#"{"stats": null}"#, None).code, 1);
}

#[test]
fn epoch_stamp_prints_the_expiry_tag_format() {
    let result = run(&["epoch-stamp", "1893456000"], "");
    assert_eq!(
        (result.code, result.stdout.trim()),
        (0, "2030-01-01T00:00:00Z")
    );
    assert_eq!(run(&["epoch-stamp", "soon"], "").code, 2);
}

#[test]
fn usage_errors_exit_two() {
    assert_eq!(run(&[], "").code, 2);
    assert_eq!(run(&["nope"], "").code, 2);
    assert_eq!(run(&["spend"], "").code, 2);
    assert_eq!(run(&["spend", "reserve", "--ledger", "x"], "").code, 2);
    assert_eq!(run(&["spend", "check", "--bogus"], "").code, 2);
}

/// The gate refuses at the stop line before it touches AWS or terraform. It
/// runs the real script, so it needs the gate's own tools (the Python test
/// skipped the same way); the stop-line verdict itself is covered above.
#[cfg(unix)]
#[test]
fn gate_wrapper_refuses_before_touching_aws_at_the_stop_line() {
    let tools = [
        "terraform",
        "aws",
        "ssh",
        "ssh-keyscan",
        "ssh-keygen",
        "flock",
    ];
    let missing: Vec<&str> = tools
        .iter()
        .copied()
        .filter(|tool| {
            std::env::var_os("PATH").is_none_or(|path| {
                !std::env::split_paths(&path).any(|dir| dir.join(tool).is_file())
            })
        })
        .collect();
    if !missing.is_empty() {
        eprintln!("runner_unavailable: gate tooling not installed: {missing:?}");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let cloud = dir.path().join("cloud");
    fs::create_dir(&cloud).unwrap();
    fs::write(cloud.join("gate.tfvars"), "ssh_key_name = \"x\"\n").unwrap();
    fs::write(cloud.join("id"), "k\n").unwrap();
    fs::write(
        cloud.join("spend.md"),
        format!("{HEADER}{}", window("90.0000")),
    )
    .unwrap();
    let output = std::process::Command::new("bash")
        .arg(repo_root().join("scripts/cloud-gate.sh"))
        .args(["--run", "--", "/bin/true"])
        .env("RSI_CLOUD_DIR", &cloud)
        .env("RSI_GATE_IDENTITY", cloud.join("id"))
        .env("RSI_GATE_LANDER", "/bin/true")
        .env("RSI_CLOUD_BIN", env!("CARGO_BIN_EXE_rsi-cloud"))
        .env("AWS_PROFILE", "nonexistent-profile")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(4), "{stderr}");
    assert!(
        stderr.contains("refusing to start a remote run"),
        "{stderr}"
    );
    assert!(
        !cloud.join("logs").exists(),
        "no terraform/apply work may start"
    );
}
