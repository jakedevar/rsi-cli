//! The cloud BUILD lane's spend net and scripts, run before any real launch
//! (#3844). No test calls AWS or reaches a host: `scripts/cloud-gate.sh --run`
//! and `scripts/cloud-build.sh` run against fake rsi-cloud AWS, terraform, ssh and scp
//! (`rsi-cloud-fakes`, one executable symlinked under each name) that log every
//! call to `$FAKE_LOG`. A `python`/`python3` trap on PATH proves no Python runs.
//!
//! Spend net: a reservation over either cap refuses before any terraform or aws
//! call; the EXIT trap destroys on success, failure and SIGTERM and checks for
//! leftovers; the host gets `max_lifetime_minutes` and an expiry tag.
#![cfg(unix)]

mod common;

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::repo_root;
use serde_json::{Value, json};

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
const HEADER: &str = "# AWS satellite spend ledger\n\n\
Operator grant: $2000 starting 2026-09-27 22:48:22 UTC (direct operator message).\n\
Configured compute estimate: $0.7813/hour. Stop and report by $1500 cumulative under this grant.\n";

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

fn executable(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, body).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn find_tool(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
}

impl Fixture {
    /// `None` (reported, not silent) when a tool the scripts themselves need is
    /// not installed.
    fn new() -> Option<Self> {
        for tool in ["flock", "git", "timeout", "bash"] {
            if find_tool(tool).is_none() {
                eprintln!(
                    "runner_unavailable: {tool} is not installed; the cloud scripts cannot run"
                );
                return None;
            }
        }
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        fs::create_dir(&bin).unwrap();
        for tool in ["rsi-cloud", "terraform", "aws", "ssh", "scp"] {
            symlink(env!("CARGO_BIN_EXE_rsi-cloud-fakes"), bin.join(tool)).unwrap();
        }
        executable(
            &bin,
            "ssh-keyscan",
            "#!/bin/sh\necho \"192.0.2.1 ssh-ed25519 AAAAFAKEKEY\"\n",
        );
        executable(
            &bin,
            "ssh-keygen",
            "#!/bin/sh\nfor a in \"$@\"; do [ \"$a\" = \"-R\" ] && exit 0; done\ncat >/dev/null\necho \"256 SHA256:fakefingerprint host (ED25519)\"\n",
        );
        // Any Python the lane ran would leave this file.
        let trap = format!(
            "#!/bin/sh\necho \"$0 $*\" >> '{}'\nexit 99\n",
            tmp.path().join("python-called").display()
        );
        for name in ["python", "python3", "python3.11"] {
            executable(&bin, name, &trap);
        }
        let home = tmp.path().join("home");
        let cloud = home.join(".rsi/cloud");
        fs::create_dir_all(&cloud).unwrap();
        fs::create_dir_all(home.join(".ssh")).unwrap();
        fs::create_dir_all(home.join(".aws")).unwrap();
        fs::write(
            home.join(".aws/config"),
            "[profile rsi-cloud-terraform]\nregion = us-west-1\n",
        )
        .unwrap();
        fs::write(cloud.join("spend.md"), HEADER).unwrap();
        fs::write(cloud.join("gate.tfvars"), "ssh_key_name = \"k\"\n").unwrap();
        fs::write(home.join("identity"), "key").unwrap();
        let fixture = Self {
            log: tmp.path().join("calls.jsonl"),
            tmp,
            bin,
            cloud,
            home,
        };
        fixture.caps(1000.0, 1500.0);
        Some(fixture)
    }

    fn caps(&self, daily: f64, stop: f64) {
        fs::write(
            self.cloud.join("spend-caps.json"),
            json!({"stop_line_usd": stop, "daily_cap_usd": daily}).to_string(),
        )
        .unwrap();
    }

    fn ledger(&self) -> String {
        fs::read_to_string(self.cloud.join("spend.md")).unwrap()
    }

    fn command(&self, program: &Path) -> Command {
        let path = format!(
            "{}:{}",
            self.bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut command = Command::new(program);
        command
            .env_clear()
            .env("PATH", path)
            .env("HOME", &self.home)
            .env("RSI_CLOUD_DIR", &self.cloud)
            .env("RSI_GATE_IDENTITY", self.home.join("identity"))
            // AWS now runs inside rsi-cloud, so faking `aws` on PATH alone
            // leaves the gate polling the real client's console output. The
            // wrapper fakes AWS and delegates spend to the real binary.
            .env("RSI_CLOUD_BIN", self.bin.join("rsi-cloud"))
            .env("RSI_CLOUD_REAL", env!("CARGO_BIN_EXE_rsi-cloud"))
            .env("FAKE_LOG", &self.log)
            .env("FAKE_LEDGER", self.cloud.join("spend.md"))
            .env("FAKE_AWS_STATES", self.tmp.path().join("aws-states.json"));
        command
    }

    fn run(&self, mut command: Command) -> Out {
        let output = command.output().expect("run script");
        Out {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }

    /// `cloud-gate.sh --max-lifetime-minutes 60 --run -- COMMAND...`.
    fn gate_command(&self, run: &[&str]) -> Command {
        let mut command = self.command(&repo_root().join("scripts/cloud-gate.sh"));
        command
            .args(["--max-lifetime-minutes", "60", "--run", "--"])
            .args(run);
        command
    }

    fn calls(&self, tool: &str) -> Vec<Value> {
        let Ok(text) = fs::read_to_string(&self.log) else {
            return Vec::new();
        };
        text.lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|row| row["tool"] == tool)
            .collect()
    }

    fn terraform_subs(&self) -> Vec<String> {
        self.calls("terraform")
            .iter()
            .map(|call| call["sub"].as_str().unwrap_or("").to_string())
            .collect()
    }

    fn assert_no_python(&self) {
        assert!(
            !self.tmp.path().join("python-called").exists(),
            "the lane ran Python: {:?}",
            fs::read_to_string(self.tmp.path().join("python-called"))
        );
    }
}

fn args_of(call: &Value) -> Vec<String> {
    call["args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|arg| arg.as_str().unwrap().to_string())
        .collect()
}

// --- the spend net ---------------------------------------------------------

#[test]
fn a_reservation_over_the_daily_cap_refuses_before_any_terraform_or_aws_call() {
    let Some(f) = Fixture::new() else { return };
    // One hour at $0.7813 reserves $0.7813: over a $0.50 daily cap.
    f.caps(0.5, 1500.0);
    let out = f.run(f.gate_command(&["sh", "-c", "touch ran"]));
    assert_eq!(out.code, 4, "{}{}", out.stdout, out.stderr);
    assert!(
        out.stderr.contains("would pass the daily cap"),
        "{}",
        out.stderr
    );
    assert!(
        out.stderr.contains("refusing to start a remote run"),
        "{}",
        out.stderr
    );
    assert!(f.calls("terraform").is_empty(), "no terraform call");
    assert!(f.calls("aws").is_empty(), "no aws call");
    assert_eq!(f.ledger(), HEADER, "a refusal writes no reservation");
    f.assert_no_python();
}

#[test]
fn a_reservation_over_the_stop_line_refuses_before_any_terraform_or_aws_call() {
    let Some(f) = Fixture::new() else { return };
    f.caps(1000.0, 0.5);
    let out = f.run(f.gate_command(&["true"]));
    assert_eq!(out.code, 4, "{}{}", out.stdout, out.stderr);
    assert!(
        out.stderr.contains("would pass the stop line"),
        "{}",
        out.stderr
    );
    assert!(f.calls("terraform").is_empty() && f.calls("aws").is_empty());
    assert_eq!(f.ledger(), HEADER);
}

#[test]
fn a_ledger_already_at_the_stop_line_refuses_the_start() {
    let Some(f) = Fixture::new() else { return };
    f.caps(1000.0, 0.5);
    fs::write(
        f.cloud.join("spend.md"),
        format!("{HEADER}Gate window i-0old: stop 2026-10-01T00:00:00Z, elapsed 3600 s = 1.0000 h, est compute $0.9000 at $0.7813/h; terraform destroy ok; leftover gate instances: none; x; reservation res-0-0.\n"),
    )
    .unwrap();
    let out = f.run(f.gate_command(&["true"]));
    assert_eq!(out.code, 4, "{}{}", out.stdout, out.stderr);
    assert!(
        out.stderr.contains("at or above the stop line"),
        "{}",
        out.stderr
    );
    assert!(f.calls("terraform").is_empty() && f.calls("aws").is_empty());
}

#[test]
fn success_destroys_the_host_sets_its_lifetime_and_closes_the_ledger() {
    let Some(f) = Fixture::new() else { return };
    let out = f.run(f.gate_command(&[
        "sh",
        "-c",
        "echo \"host=$RSI_GATE_HOST as=$RSI_GATE_RUN_AS id=$RSI_GATE_INSTANCE_ID\"",
    ]));
    assert_eq!(out.code, 0, "{}{}", out.stdout, out.stderr);
    assert!(
        out.stdout
            .contains("host=ec2-user@192.0.2.1 as=rsi id=i-0gatefake000001"),
        "{}",
        out.stdout
    );
    let subs = f.terraform_subs();
    let apply = subs.iter().position(|s| s == "apply").expect("apply");
    let destroy = subs.iter().position(|s| s == "destroy").expect("destroy");
    assert!(apply < destroy, "{subs:?}");
    // The guest lifetime and the expiry tag reach the apply.
    let apply_args = args_of(&f.calls("terraform")[apply]);
    assert!(
        apply_args.contains(&"max_lifetime_minutes=60".to_string()),
        "{apply_args:?}"
    );
    assert!(
        apply_args.iter().any(|a| a.starts_with("expires_at=")),
        "{apply_args:?}"
    );
    // Reserved before the host started, closed by the stop line.
    let ledger = f.ledger();
    let reserved = ledger.find("Gate reservation res-").expect("reservation");
    let started = ledger
        .find("Gate window i-0gatefake000001: start")
        .expect("start line");
    let stopped = ledger
        .find("Gate window i-0gatefake000001: stop")
        .expect("stop line");
    assert!(reserved < started && started < stopped, "{ledger}");
    assert!(ledger.contains("reserved $0.7813 until"), "{ledger}");
    assert!(ledger.contains("lifetime 60 min"), "{ledger}");
    assert!(
        ledger.contains("terraform destroy ok; leftover gate instances: None"),
        "{ledger}"
    );
    assert!(
        f.calls("aws")
            .iter()
            .any(|c| args_of(c).contains(&"get-console-output".to_string())),
        "host key verified through the fake AWS console"
    );
    // The leftover check is an aws call after the destroy.
    assert!(
        f.calls("aws")
            .iter()
            .any(|c| args_of(c).iter().any(|a| a == "tag:Service")
                || args_of(c).join(" ").contains("tag:Service")),
        "no leftover check"
    );
    f.assert_no_python();
}

#[test]
fn a_failing_run_still_destroys_the_host_and_keeps_its_exit_code() {
    let Some(f) = Fixture::new() else { return };
    let out = f.run(f.gate_command(&["sh", "-c", "exit 7"]));
    assert_eq!(out.code, 7, "{}{}", out.stdout, out.stderr);
    assert!(f.terraform_subs().contains(&"destroy".to_string()));
    assert!(
        f.ledger().contains("terraform destroy ok"),
        "{}",
        f.ledger()
    );
}

#[test]
fn sigterm_during_the_run_destroys_the_host_and_closes_the_ledger() {
    let Some(f) = Fixture::new() else { return };
    let started = f.tmp.path().join("started");
    let mut command = f.gate_command(&[
        "sh",
        "-c",
        &format!("touch '{}'; sleep 60", started.display()),
    ]);
    // Its own process group: a stop kills the whole group, as systemd does.
    let mut child = command
        .process_group(0)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while !started.exists() {
        assert!(Instant::now() < deadline, "the run never started");
        std::thread::sleep(Duration::from_millis(100));
    }
    let pid = child.id().to_string();
    let killed = Command::new("sh")
        .args(["-c", "kill -s TERM -- \"-$0\"", &pid])
        .status()
        .unwrap();
    assert!(killed.success());
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(143));
    assert!(
        f.terraform_subs().contains(&"destroy".to_string()),
        "{:?}",
        f.terraform_subs()
    );
    let ledger = f.ledger();
    assert!(
        ledger.contains("Gate window i-0gatefake000001: stop"),
        "{ledger}"
    );
    assert!(ledger.contains("terraform destroy ok"), "{ledger}");
}

#[test]
fn a_leftover_gate_instance_fails_the_run_and_is_terminated() {
    let Some(f) = Fixture::new() else { return };
    let mut command = f.gate_command(&["true"]);
    command.env("FAKE_AWS_LEFTOVER", "i-0leaked");
    let out = f.run(command);
    assert_eq!(out.code, 1, "{}{}", out.stdout, out.stderr);
    assert!(
        out.stderr
            .contains("LEFTOVER gate instances after destroy: i-0leaked"),
        "{}",
        out.stderr
    );
    assert!(
        f.ledger().contains("leftover gate instances: i-0leaked"),
        "{}",
        f.ledger()
    );
    assert!(
        f.calls("aws")
            .iter()
            .any(|c| args_of(c).contains(&"terminate-instances".to_string())),
        "the leftover is terminated"
    );
}

#[test]
fn a_reclaimed_spot_host_is_incomplete_exit_75() {
    let Some(f) = Fixture::new() else { return };
    fs::write(
        f.tmp.path().join("aws-states.json"),
        json!({"i-0gatefake000001": {"state": "terminated", "reason": "Server.SpotInstanceTermination"}}).to_string(),
    )
    .unwrap();
    let out = f.run(f.gate_command(&["sh", "-c", "exit 255"]));
    assert_eq!(out.code, 75, "{}{}", out.stdout, out.stderr);
    assert!(
        out.stdout
            .lines()
            .any(|l| l == "cloud-gate: spot_interrupted"),
        "{}",
        out.stdout
    );
}

#[test]
fn the_guest_terminates_itself_at_its_lifetime_in_the_checked_in_terraform() {
    let root = repo_root();
    let main = fs::read_to_string(root.join("infra/aws/gate/main.tf")).unwrap();
    // A one-time spot instance terminates when its guest shuts down. AWS
    // refuses to modify instance_initiated_shutdown_behavior on a spot
    // instance (UnsupportedOperation), so the attribute must stay out (#3977).
    let spot = main
        .find(r#"market_type = "spot""#)
        .expect("the gate host is a spot instance");
    let spot_block = &main[spot..];
    assert!(spot_block.contains(r#"spot_instance_type             = "one-time""#));
    assert!(spot_block.contains(r#"instance_interruption_behavior = "terminate""#));
    let attribute = main.lines().find(|l| {
        l.trim_start()
            .starts_with("instance_initiated_shutdown_behavior")
    });
    assert_eq!(attribute, None, "spot hosts must not set it (#3977)");
    assert!(main.contains("max_lifetime_minutes = var.max_lifetime_minutes"));
    assert!(main.contains(r#""rsi:expires-at""#));
    let boot = fs::read_to_string(root.join("infra/aws/gate/bootstrap.sh.tftpl")).unwrap();
    let shutdown = boot
        .find("shutdown -h +${max_lifetime_minutes}")
        .expect("lifetime backstop");
    // First thing, before any download can fail.
    assert!(shutdown < boot.find("dnf install").unwrap());
    let gate = fs::read_to_string(root.join("scripts/cloud-gate.sh")).unwrap();
    assert!(gate.contains("max_lifetime_minutes=$max_lifetime_minutes"));
}

// --- the build lane's scripts ----------------------------------------------

fn build_script() -> PathBuf {
    repo_root().join("scripts/cloud-build.sh")
}

#[test]
fn the_build_lane_runs_no_python_anywhere() {
    let root = repo_root();
    for name in ["cloud-build.sh", "cloud-build-remote.sh"] {
        let text = fs::read_to_string(root.join("scripts").join(name)).unwrap();
        for line in text.lines().filter(|l| !l.trim_start().starts_with('#')) {
            let lower = line.to_ascii_lowercase();
            assert!(
                !lower.contains("python") && !lower.contains(".py"),
                "{name}: {line}"
            );
        }
    }
    // The hub side reaches cloud-sweep.sh only for its python-free bundle mode.
    let hub = fs::read_to_string(root.join("scripts/cloud-build.sh")).unwrap();
    for line in hub.lines().filter(|l| l.contains("cloud-sweep.sh")) {
        let line = line.trim_start();
        assert!(
            line.starts_with('#') || line.contains("cloud-sweep.sh\" bundle "),
            "{line}"
        );
    }
}

#[test]
fn the_lane_refuses_with_a_typed_line_before_any_spend_when_a_prerequisite_is_missing() {
    let Some(f) = Fixture::new() else { return };
    let cloud = |f: &Fixture| {
        let mut command = f.command(&build_script());
        command.args(["cloud", SHA]);
        command
    };
    // The AWS profile.
    fs::remove_file(f.home.join(".aws/config")).unwrap();
    let out = f.run(cloud(&f));
    assert_eq!(out.code, 2, "{}{}", out.stdout, out.stderr);
    assert!(
        out.stderr
            .contains("cloud-build: missing aws-profile rsi-cloud-terraform"),
        "{}",
        out.stderr
    );
    // rsi-cloud.
    fs::write(
        f.home.join(".aws/config"),
        "[profile rsi-cloud-terraform]\n",
    )
    .unwrap();
    let mut command = cloud(&f);
    command.env("RSI_CLOUD_BIN", "/nonexistent/rsi-cloud");
    let out = f.run(command);
    assert_eq!(out.code, 2);
    assert!(
        out.stderr.contains("cloud-build: missing rsi-cloud"),
        "{}",
        out.stderr
    );
    // The tfvars file.
    fs::remove_file(f.cloud.join("gate.tfvars")).unwrap();
    let out = f.run(cloud(&f));
    assert_eq!(out.code, 2);
    assert!(
        out.stderr.contains("cloud-build: missing gate.tfvars"),
        "{}",
        out.stderr
    );
    // terraform and aws: a PATH that has neither.
    for (keep, missing) in [(None, "terraform"), (Some("terraform"), "aws")] {
        let dir = f.tmp.path().join(format!("only-{missing}"));
        fs::create_dir(&dir).unwrap();
        for tool in ["bash", "dirname", "env"] {
            symlink(find_tool(tool).unwrap(), dir.join(tool)).unwrap();
        }
        if let Some(keep) = keep {
            symlink(env!("CARGO_BIN_EXE_rsi-cloud-fakes"), dir.join(keep)).unwrap();
        }
        let mut command = Command::new(find_tool("bash").unwrap());
        command
            .arg(build_script())
            .args(["cloud", SHA])
            .env_clear()
            .env("PATH", &dir)
            .env("HOME", &f.home);
        let out = f.run(command);
        assert_eq!(out.code, 2, "{}{}", out.stdout, out.stderr);
        assert!(
            out.stderr
                .contains(&format!("cloud-build: missing {missing}")),
            "{}",
            out.stderr
        );
    }
    assert!(f.calls("terraform").is_empty() && f.calls("aws").is_empty());
    assert_eq!(f.ledger(), HEADER);
    f.assert_no_python();
}

#[test]
fn a_sha_that_cannot_be_bundled_spends_nothing() {
    let Some(f) = Fixture::new() else { return };
    // A repository whose origin refuses connections at once.
    let repo = f.tmp.path().join("repo");
    fs::create_dir_all(repo.join(".git")).unwrap();
    fs::write(
        repo.join(".git/config"),
        "[remote \"origin\"]\n\turl = https://127.0.0.1:1/rsi.git\n",
    )
    .unwrap();
    let mut command = f.command(&build_script());
    command
        .args(["cloud", SHA, "--repo"])
        .arg(&repo)
        .arg("--mirror")
        .arg(f.tmp.path().join("mirror.git"));
    let out = f.run(command);
    assert_ne!(out.code, 0, "{}{}", out.stdout, out.stderr);
    assert!(
        f.calls("terraform").is_empty(),
        "no host for a SHA that cannot be bundled"
    );
    assert!(f.calls("aws").is_empty());
    assert_eq!(f.ledger(), HEADER, "and no reservation");
    f.assert_no_python();
}

fn hosted(f: &Fixture, remote_output: &str, remote_exit: &str) -> Out {
    let bundle = f.tmp.path().join("source.bundle");
    fs::write(&bundle, "bundle").unwrap();
    let canned = f.tmp.path().join("remote-output");
    fs::write(&canned, remote_output).unwrap();
    let mut command = f.command(&build_script());
    command
        .args(["hosted", SHA])
        .env("RSI_GATE_HOST", "ec2-user@192.0.2.1")
        .env("RSI_GATE_RUN_AS", "rsi")
        .env("RSI_GATE_INSTANCE_ID", "i-0gatefake000001")
        .env("RSI_BUILD_BUNDLE", &bundle)
        .env("FAKE_SSH_OUTPUT", &canned)
        .env("FAKE_SSH_EXIT", remote_exit);
    f.run(command)
}

#[test]
fn hosted_ships_the_bundle_runs_the_remote_script_as_the_toolchain_owner_and_keeps_the_output() {
    let Some(f) = Fixture::new() else { return };
    let canned = format!("BUILD_STEP checkout 0\nBUILD_STEPS_DONE {SHA}\n");
    let out = hosted(&f, &canned, "0");
    assert_eq!(out.code, 0, "{}{}", out.stdout, out.stderr);
    assert!(
        out.stdout
            .contains(&format!("BUILD_HOST instance=i-0gatefake000001 sha={SHA}"))
    );
    assert!(out.stdout.contains(&canned), "{}", out.stdout);
    assert!(out.stdout.contains("BUILD_REMOTE_EXIT 0"));
    let kept = f
        .home
        .join(format!(".rsi/cloud/results/build-{SHA}/remote.out"));
    assert_eq!(fs::read_to_string(kept).unwrap(), canned);
    let scp = args_of(&f.calls("scp")[0]);
    assert!(
        scp.contains(&format!(
            "ec2-user@192.0.2.1:/tmp/rsi-cloud-build-{SHA}.bundle"
        )),
        "{scp:?}"
    );
    let ssh = args_of(&f.calls("ssh")[0]);
    let joined = ssh.join(" ");
    // The host key was pinned by cloud-gate.sh; ssh must verify it.
    assert!(joined.contains("StrictHostKeyChecking=yes"), "{joined}");
    assert!(
        joined.contains("BatchMode=yes") && joined.contains("IdentitiesOnly=yes"),
        "{joined}"
    );
    assert!(
        joined.contains(&format!(
            "exec sudo -n -H -u rsi bash -s -- {SHA} /tmp/rsi-cloud-build-{SHA}.bundle"
        )),
        "{joined}"
    );
    f.assert_no_python();
}

#[test]
fn hosted_exits_with_the_remote_status_and_refuses_a_malformed_host() {
    let Some(f) = Fixture::new() else { return };
    let out = hosted(&f, "BUILD_STEP checkout 0\n", "101");
    assert_eq!(out.code, 101, "{}{}", out.stdout, out.stderr);
    assert!(out.stdout.contains("BUILD_REMOTE_EXIT 101"));
    let before = f.calls("ssh").len();
    let mut command = f.command(&build_script());
    command
        .args(["hosted", SHA])
        .env("RSI_GATE_HOST", "evil@192.0.2.1; rm -rf /")
        .env("RSI_BUILD_BUNDLE", f.home.join("identity"));
    let out = f.run(command);
    assert_eq!(out.code, 2);
    assert_eq!(f.calls("ssh").len(), before, "no ssh for a malformed host");
}

// --- the remote script, against stub cargo/rustup and a real bundle ---------

struct Remote {
    tmp: tempfile::TempDir,
    stubs: PathBuf,
    bundle: PathBuf,
    sha: String,
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

impl Remote {
    fn new() -> Option<Self> {
        find_tool("git")?;
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        fs::write(
            repo.join("rust-toolchain.toml"),
            "[toolchain]\nchannel = \"1.94.1\"\n",
        )
        .unwrap();
        git(&repo, &["add", "rust-toolchain.toml"]);
        git(&repo, &["commit", "-q", "-m", "x"]);
        let sha = git(&repo, &["rev-parse", "HEAD"]);
        git(&repo, &["update-ref", "refs/remotes/origin/rolling", &sha]);
        let bundle = tmp.path().join("source.bundle");
        git(
            &repo,
            &[
                "bundle",
                "create",
                bundle.to_str().unwrap(),
                "refs/remotes/origin/rolling",
            ],
        );
        let stubs = tmp.path().join("stubs");
        fs::create_dir(&stubs).unwrap();
        executable(
            &stubs,
            "cargo",
            "#!/bin/sh\n\
echo \"cargo $* [toolchain=$RUSTUP_TOOLCHAIN target=$CARGO_TARGET_DIR]\" >> \"$STUB_LOG\"\n\
case \"$*\" in\n\
  \"nextest --version\") [ -z \"$STUB_NO_NEXTEST\" ] && exit 0; exit 1 ;;\n\
  \"--version\") echo cargo 1.94.1; exit 0 ;;\n\
  build*) echo \"Compiling stub\"; exit ${STUB_BUILD_EXIT:-0} ;;\n\
  nextest*|test*) echo \"running tests\"; exit ${STUB_TEST_EXIT:-0} ;;\n\
esac\nexit 0\n",
        );
        executable(&stubs, "rustc", "#!/bin/sh\necho rustc 1.94.1\n");
        executable(
            &stubs,
            "rustup",
            "#!/bin/sh\necho \"rustup $*\" >> \"$STUB_LOG\"\n\
[ \"$1 $2\" = \"toolchain list\" ] && echo \"1.94.1-x86_64-unknown-linux-gnu (default)\"\nexit 0\n",
        );
        Some(Self {
            tmp,
            stubs,
            bundle,
            sha,
        })
    }

    fn run(&self, sha: &str, env: &[(&str, &str)]) -> Out {
        let home = self.tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let path = format!(
            "{}:{}",
            self.stubs.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut command = Command::new("bash");
        command
            .arg(repo_root().join("scripts/cloud-build-remote.sh"))
            .arg(sha)
            .arg(&self.bundle)
            .env_clear()
            .env("PATH", path)
            .env("HOME", &home)
            .env("RSI_BUILD_ROOT", self.tmp.path().join("root"))
            .env("STUB_LOG", self.tmp.path().join("stub.log"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .envs(env.iter().copied());
        let output = command.output().unwrap();
        Out {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }

    fn stub_log(&self) -> String {
        fs::read_to_string(self.tmp.path().join("stub.log")).unwrap_or_default()
    }
}

fn step_lines(out: &str) -> Vec<&str> {
    out.lines()
        .filter(|l| l.starts_with("BUILD_STEP "))
        .collect()
}

#[test]
fn the_remote_script_builds_the_workspace_locked_then_tests_the_small_set() {
    let Some(r) = Remote::new() else { return };
    let out = r.run(&r.sha, &[]);
    assert_eq!(out.code, 0, "{}{}", out.stdout, out.stderr);
    assert_eq!(
        step_lines(&out.stdout),
        [
            "BUILD_STEP checkout 0",
            "BUILD_STEP toolchain 0",
            "BUILD_STEP build 0",
            "BUILD_STEP test 0"
        ]
    );
    assert!(out.stdout.contains("BUILD_TEST_RUNNER nextest"));
    assert_eq!(
        out.stdout.lines().last(),
        Some(format!("BUILD_STEPS_DONE {}", r.sha).as_str())
    );
    let log = r.stub_log();
    assert!(
        log.contains("cargo build --workspace --locked [toolchain=1.94.1"),
        "{log}"
    );
    assert!(
        log.contains("cargo nextest run --locked --no-fail-fast -p rsi-common -p rsid-core --lib"),
        "{log}"
    );
    assert!(log.contains("target="), "{log}");
}

#[test]
fn a_failed_build_exits_with_its_code_skips_the_tests_and_prefixes_the_tail() {
    let Some(r) = Remote::new() else { return };
    let out = r.run(&r.sha, &[("STUB_BUILD_EXIT", "101")]);
    assert_eq!(out.code, 101, "{}{}", out.stdout, out.stderr);
    assert_eq!(
        step_lines(&out.stdout),
        [
            "BUILD_STEP checkout 0",
            "BUILD_STEP toolchain 0",
            "BUILD_STEP build 101",
            "BUILD_STEP test skipped"
        ]
    );
    assert!(out.stdout.contains("| Compiling stub"), "{}", out.stdout);
    assert!(!r.stub_log().contains("nextest run"));
    assert!(out.stdout.contains(&format!("BUILD_STEPS_DONE {}", r.sha)));
}

#[test]
fn without_nextest_the_small_set_runs_under_cargo_test() {
    let Some(r) = Remote::new() else { return };
    let out = r.run(
        &r.sha,
        &[("STUB_NO_NEXTEST", "1"), ("STUB_TEST_EXIT", "100")],
    );
    assert_eq!(out.code, 100, "{}{}", out.stdout, out.stderr);
    assert!(out.stdout.contains("BUILD_STEP test 100"));
    assert!(out.stdout.contains("BUILD_TEST_RUNNER cargo-test"));
    assert!(
        r.stub_log()
            .contains("cargo test --locked -p rsi-common -p rsid-core --lib")
    );
}

#[test]
fn a_bundle_that_is_not_the_requested_sha_fails_the_checkout_and_skips_the_rest() {
    let Some(r) = Remote::new() else { return };
    let other = "f".repeat(40);
    let out = r.run(&other, &[]);
    assert_ne!(out.code, 0, "{}{}", out.stdout, out.stderr);
    let steps = step_lines(&out.stdout);
    assert!(
        steps[0].starts_with("BUILD_STEP checkout ") && steps[0] != "BUILD_STEP checkout 0",
        "{steps:?}"
    );
    assert!(out.stdout.contains("BUILD_STEP toolchain skipped"));
    assert!(out.stdout.contains("BUILD_STEP build skipped"));
    assert!(!r.stub_log().contains("cargo build"));
}
