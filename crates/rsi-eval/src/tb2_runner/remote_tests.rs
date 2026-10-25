//! Tests of the remote Docker transport (#3679 R2) with fake command executors: argv and quoting,
//! host and path validation, memory and disk refusal, pin, label and digest mismatch, readiness,
//! scoped teardown, a synthetic $0 oracle run, and the CLI exit codes. No Docker daemon, no SSH
//! host, no Harbor or uv, no model.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};

use rsi_common::bench_runner::{
    PlannedTrial, RemotePlanV1, RunnerMode, RunnerPlanV1, TrialExceptionKind, TrialIdentity,
    TrialOutcomeV1,
};
use serde_json::{Value, json};

use super::{
    cli,
    docker::{
        LABEL_PROJECT, LABEL_RETRY, LABEL_RUN, LABEL_SERVICE, LABEL_SLOT, LABEL_TASK,
        SETTLE_SCRIPT, TrialNames, compose_config, pinned_ref, short_hash, verify_container,
    },
    reward::{REWARD_JSON_PATH, REWARD_TEXT_PATH},
    runner::{self, REMOTE_STATE_DIR},
    transport::{
        APPROVED_EXECUTABLES, CommandExecutor, CommandOutput, CommandSpec, ExecError, OpKind,
        Remote, SystemExecutor, parse_probe, posix_quote, remote_sh, validate_hosts,
        validate_remote_path, validate_ssh_host,
    },
};
use crate::tb2::{
    dataset::{self, Task},
    split::Shard,
};

#[cfg(unix)]
#[path = "harness_tests.rs"]
mod harness_tests;

const GIB: u64 = 1 << 30;
const HOME: &str = "/home/op";
const DOCKER_HOST: &str = "ssh://arch-laptop";

fn digest(c: char) -> String {
    format!("sha256:{}", c.to_string().repeat(64))
}

// ---- the fake world ------------------------------------------------------------------------

/// A scripted Docker host and laptop. Every command is recorded.
struct World {
    log: Mutex<Vec<CommandSpec>>,
    plan: RunnerPlanV1,
    disk_kb: Option<u64>,
    mem_kb: Option<u64>,
    uname_m: &'static str,
    compose_version: &'static str,
    docker_down: bool,
    ssh_down: bool,
    pull_fails: bool,
    /// Local `RepoDigests` of a pulled reference; `None` uses the pinned digest.
    local_digests: Option<Vec<String>>,
    /// `RepoDigests` of the running container's image; `None` uses the pinned digest.
    container_digests: Option<Vec<String>>,
    label_run: Option<&'static str>,
    memory_limit_mb: Option<u64>,
    drop_bind: bool,
    two_containers: bool,
    no_container: bool,
    solve_times_out: bool,
    solve_code: i32,
    test_code: i32,
    reward: Option<&'static str>,
    collect_fails: bool,
    settle_fails: bool,
    down_times_out: bool,
    force_remove_fails: bool,
    keepalive_is_pid_one: Mutex<bool>,
    owned: Mutex<BTreeMap<String, [bool; 3]>>,
}

impl World {
    fn new(plan: &RunnerPlanV1) -> Self {
        Self {
            log: Mutex::new(Vec::new()),
            plan: plan.clone(),
            disk_kb: Some(200 * GIB / 1024),
            mem_kb: Some(64 * GIB / 1024),
            uname_m: "x86_64",
            compose_version: "2.29.1",
            docker_down: false,
            ssh_down: false,
            pull_fails: false,
            local_digests: None,
            container_digests: None,
            label_run: None,
            memory_limit_mb: None,
            drop_bind: false,
            two_containers: false,
            no_container: false,
            solve_times_out: false,
            solve_code: 0,
            test_code: 0,
            reward: Some(r#"{"reward": 1.0}"#),
            collect_fails: false,
            settle_fails: false,
            down_times_out: false,
            force_remove_fails: false,
            // Adopted live-trial fixtures use the same safe keepalive; `up` reads and
            // replaces this from the actual generated Compose command.
            keepalive_is_pid_one: Mutex::new(true),
            owned: Mutex::new(BTreeMap::new()),
        }
    }

    fn log(&self) -> Vec<CommandSpec> {
        self.log.lock().unwrap().clone()
    }

    fn joined(&self) -> Vec<String> {
        self.log()
            .iter()
            .map(|c| format!("{} {}", c.program, c.args.join(" ")))
            .collect()
    }

    /// The planned trial or its one infrastructure replacement (R5) that owns `project`.
    fn trial_for_project(&self, project: &str) -> PlannedTrial {
        let root = format!("{HOME}/{REMOTE_STATE_DIR}/x");
        self.plan
            .trials
            .iter()
            .flat_map(|t| {
                let mut replacement = t.clone();
                replacement.identity.retry += 1;
                [t.clone(), replacement]
            })
            .find(|t| TrialNames::new(&t.identity, &root).unwrap().project == project)
            .unwrap_or_else(|| panic!("no trial for project {project}"))
    }

    fn names_for(&self, trial: &PlannedTrial) -> TrialNames {
        let root = format!(
            "{HOME}/{REMOTE_STATE_DIR}/{}",
            short_hash(&[&self.plan.run_id])
        );
        TrialNames::new(&trial.identity, &root).unwrap()
    }

    fn inspect_json(&self, trial: &PlannedTrial) -> Value {
        let names = self.names_for(trial);
        let id = &trial.identity;
        let mut mounts = vec![
            json!({"Type": "bind", "Source": names.verifier_dir, "Destination": "/logs/verifier", "RW": true}),
            json!({"Type": "bind", "Source": names.agent_dir, "Destination": "/logs/agent", "RW": true}),
            json!({"Type": "bind", "Source": names.artifacts_dir, "Destination": "/logs/artifacts", "RW": true}),
        ];
        if self.drop_bind {
            mounts.pop();
        }
        json!({
            "Id": container_id(&names.project),
            "Image": "sha256:imageid",
            "State": {"Running": true},
            "Config": {"Labels": {
                LABEL_PROJECT: names.project,
                LABEL_SERVICE: "main",
                LABEL_RUN: self.label_run.unwrap_or(&id.run_id),
                LABEL_TASK: id.task,
                LABEL_SLOT: id.slot.to_string(),
                LABEL_RETRY: id.retry.to_string(),
            }},
            "HostConfig": {
                "NanoCpus": trial.cpus * 1_000_000_000,
                "Memory": self.memory_limit_mb.unwrap_or(trial.memory_mb) * (1 << 20),
            },
            "Mounts": mounts,
        })
    }

    fn respond_ssh(&self, command: &str) -> CommandOutput {
        if self.ssh_down {
            return failed(
                255,
                "ssh: connect to host arch-laptop port 22: Connection refused",
            );
        }
        if command.contains("uname -m") {
            let mut text = format!("home={HOME}\nos=Linux\narch={}\n", self.uname_m);
            if let Some(kb) = self.disk_kb {
                text += &format!("disk_kb={kb}\n");
            }
            if let Some(kb) = self.mem_kb {
                text += &format!("mem_kb={kb}\n");
            }
            return ok(&text);
        }
        ok("")
    }

    fn respond_exec(&self, args: &[String]) -> Result<CommandOutput, ExecError> {
        let joined = args.join(" ");
        if joined.contains("pwd > /logs/agent/rsi-workdir.txt") {
            return Ok(ok(""));
        }
        if joined.contains("me=$$") {
            // Model the actual PID-1 failure: killing a waiting shell's sleep child tears
            // down the container namespace, including this exec, with SIGKILL (#3894).
            return Ok(
                if self.settle_fails || !*self.keepalive_is_pid_one.lock().unwrap() {
                    failed(137, "")
                } else {
                    ok("")
                },
            );
        }
        if joined.contains("cat -- \"$1\"") {
            let path = args.last().unwrap().as_str();
            return Ok(match (path, self.reward) {
                (super::trial::WORKDIR_FILE, _) => ok("/app\n"),
                (REWARD_JSON_PATH, Some(text)) => ok(text),
                _ => {
                    assert!(
                        path == REWARD_JSON_PATH || path == REWARD_TEXT_PATH,
                        "{path}"
                    );
                    failed(3, "")
                }
            });
        }
        if joined.contains("find /logs") {
            return Ok(ok(""));
        }
        if joined.contains("chmod +x") {
            return Ok(ok(""));
        }
        if joined.contains("/solution/solve.sh") {
            if self.solve_times_out {
                return Err(ExecError::Timeout);
            }
            return Ok(code(self.solve_code));
        }
        if joined.contains("/tests/test.sh") {
            return Ok(code(self.test_code));
        }
        panic!("unexpected exec {joined}");
    }

    fn respond_docker(&self, rest: &[String]) -> Result<CommandOutput, ExecError> {
        let word = |i: usize| rest.get(i).map_or("", String::as_str);
        let joined = rest.join(" ");
        if self.docker_down {
            return Ok(failed(
                1,
                "error during connect: Get \"http://docker.example.com/v1.24/info\": command [ssh -l op -- arch-laptop docker system dial-stdio] has exited with exit status 255",
            ));
        }
        Ok(match word(0) {
            "info" => ok("linux x86_64 27.3.1 /var/lib/docker\n"),
            "compose" if word(1) == "version" => ok(&format!("{}\n", self.compose_version)),
            "compose" => {
                let project = rest[rest.iter().position(|a| a == "-p").unwrap() + 1].as_str();
                let _ = self.trial_for_project(project);
                if rest.iter().any(|a| a == "up") {
                    let file = &rest[rest.iter().position(|a| a == "-f").unwrap() + 1];
                    let config: Value =
                        serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap();
                    *self.keepalive_is_pid_one.lock().unwrap() = config["services"]["main"]["command"]
                        == json!(["sh", "-c", "exec sleep infinity"]);
                    self.owned
                        .lock()
                        .unwrap()
                        .insert(project.to_string(), [true; 3]);
                } else if rest.iter().any(|a| a == "down") {
                    if self.down_times_out {
                        return Err(ExecError::Timeout);
                    }
                    self.owned
                        .lock()
                        .unwrap()
                        .insert(project.to_string(), [false; 3]);
                }
                ok("")
            }
            "pull" if self.pull_fails => failed(1, "manifest unknown"),
            "pull" => ok(""),
            "image" => {
                let reference = rest.last().unwrap();
                if joined.contains("RepoDigests") {
                    let pinned: Vec<String> = self
                        .plan
                        .trials
                        .iter()
                        .map(|t| {
                            format!(
                                "{}@{}",
                                pinned_ref(&t.image, &t.image_digest)
                                    .unwrap()
                                    .split('@')
                                    .next()
                                    .unwrap(),
                                t.image_digest
                            )
                        })
                        .collect();
                    let list = if reference == "sha256:imageid" {
                        self.container_digests.clone().unwrap_or(pinned)
                    } else {
                        self.local_digests.clone().unwrap_or(pinned)
                    };
                    ok(&serde_json::to_string(&list).unwrap())
                } else {
                    ok("linux/amd64\n")
                }
            }
            "ps" if joined.contains("status=running") => {
                if self.no_container {
                    return Ok(ok(""));
                }
                let filter = rest
                    .iter()
                    .find_map(|a| a.strip_prefix(&format!("label={LABEL_PROJECT}=")))
                    .unwrap();
                let id = container_id(filter);
                ok(&if self.two_containers {
                    format!("{id}\n{id}\n")
                } else {
                    format!("{id}\n")
                })
            }
            "ps" | "network" | "volume" if word(0) == "ps" || word(1) == "ls" => {
                let kind = match word(0) {
                    "ps" => 0,
                    "network" => 1,
                    _ => 2,
                };
                let project = rest
                    .iter()
                    .find_map(|a| a.strip_prefix(&format!("label={LABEL_PROJECT}=")))
                    .unwrap();
                let present = self
                    .owned
                    .lock()
                    .unwrap()
                    .get(project)
                    .is_some_and(|v| v[kind]);
                ok(&if present {
                    owned_id(project, kind)
                } else {
                    String::new()
                })
            }
            "rm" | "network" | "volume" => {
                if self.force_remove_fails {
                    return Ok(failed(1, "resource busy"));
                }
                let kind = match word(0) {
                    "rm" => 0,
                    "network" => 1,
                    _ => 2,
                };
                for (project, objects) in self.owned.lock().unwrap().iter_mut() {
                    if rest.contains(&owned_id(project, kind)) {
                        objects[kind] = false;
                    }
                }
                ok("")
            }
            "inspect" => {
                let id = rest.last().unwrap();
                let trial = self
                    .plan
                    .trials
                    .iter()
                    .flat_map(|t| {
                        let mut replacement = t.clone();
                        replacement.identity.retry += 1;
                        [t.clone(), replacement]
                    })
                    .find(|t| container_id(&self.names_for(t).project) == *id)
                    .unwrap_or_else(|| panic!("no container {id}"));
                ok(&self.inspect_json(&trial).to_string())
            }
            "cp" if self.collect_fails && joined.contains(":/logs/.") => {
                failed(1, "No such container:path")
            }
            "cp" => ok(""),
            "exec" => return self.respond_exec(rest),
            other => panic!("unexpected docker {other}: {joined}"),
        })
    }
}

fn container_id(project: &str) -> String {
    short_hash(&[project, "container"])
}

fn owned_id(project: &str, kind: usize) -> String {
    match kind {
        0 => container_id(project),
        1 => short_hash(&[project, "network"]),
        _ => format!("{project}_data"),
    }
}

fn ok(stdout: &str) -> CommandOutput {
    CommandOutput {
        code: Some(0),
        stdout: stdout.as_bytes().to_vec(),
        ..CommandOutput::default()
    }
}

const fn code(code: i32) -> CommandOutput {
    CommandOutput {
        code: Some(code),
        stdout: Vec::new(),
        stderr: Vec::new(),
        truncated: false,
    }
}

fn failed(status: i32, stderr: &str) -> CommandOutput {
    CommandOutput {
        code: Some(status),
        stderr: stderr.as_bytes().to_vec(),
        ..CommandOutput::default()
    }
}

impl CommandExecutor for World {
    fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, ExecError> {
        self.log.lock().unwrap().push(spec.clone());
        match spec.program.as_str() {
            "ssh" => Ok(self.respond_ssh(spec.args.last().unwrap())),
            "docker" => {
                assert_eq!(&spec.args[..2], ["-H", DOCKER_HOST], "{:?}", spec.args);
                self.respond_docker(&spec.args[2..])
            }
            other => panic!("the runner may only run docker and ssh, not {other}"),
        }
    }
}

// ---- fixtures ------------------------------------------------------------------------------

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    plan: RunnerPlanV1,
}

fn row(name: &str, c: char, bytes: u64) -> Task {
    Task {
        name: name.to_string(),
        difficulty: "medium".into(),
        category: "software-engineering".into(),
        cpus: 1,
        memory_mb: 2048,
        agent_timeout_sec: 900.0,
        verifier_timeout_sec: 600.0,
        image: format!("registry.example/tb2/{name}:20251031"),
        image_digest: digest(c),
        image_bytes: bytes,
        task_toml_sha256: String::new(),
    }
}

fn planned(task: &Task, slot: u32) -> PlannedTrial {
    super::planned_trial(
        task,
        TrialIdentity {
            run_id: "run-1".into(),
            task: task.name.clone(),
            slot,
            retry: 0,
        },
    )
    .unwrap()
}

/// A checkout, a dataset.toml and a plan for `tasks` (two shards when there are two tasks).
fn fixture(tasks: &[(&str, char)], concurrency: u32) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let checkout = root.join("checkout");
    // Read-only Git marker/HEAD fixture for the mandatory commit proof. No mutation command
    // can reach the assigned sandbox, even when TMPDIR itself is inside that sandbox.
    let git = checkout.join(".git");
    std::fs::create_dir_all(git.join("objects")).unwrap();
    std::fs::create_dir_all(git.join("refs/heads")).unwrap();
    std::fs::write(git.join("HEAD"), "ref: refs/heads/pinned\n").unwrap();
    std::fs::write(
        git.join("refs/heads/pinned"),
        format!("{}\n", dataset::COMMIT),
    )
    .unwrap();
    let mut rows = Vec::new();
    for (name, c) in tasks {
        let task_dir = checkout.join(name);
        std::fs::create_dir_all(task_dir.join("solution")).unwrap();
        std::fs::create_dir_all(task_dir.join("tests")).unwrap();
        std::fs::write(task_dir.join("task.toml"), format!("# {name}\n")).unwrap();
        std::fs::write(task_dir.join("solution/solve.sh"), "#!/bin/sh\n").unwrap();
        std::fs::write(task_dir.join("tests/test.sh"), "#!/bin/sh\n").unwrap();
        let mut r = row(name, *c, 3 * GIB);
        r.task_toml_sha256 = dataset::task_hash(&task_dir.join("task.toml")).unwrap();
        rows.push(r);
    }
    let shards: Vec<Shard> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| Shard {
            name: format!("search-{:02}", i + 1),
            split: "search".into(),
            heavy: false,
            bytes: r.image_bytes,
            tasks: vec![r.name.clone()],
        })
        .collect();
    std::fs::write(root.join("dataset.toml"), dataset::render(&rows, &shards)).unwrap();
    let trials = rows.iter().map(|r| planned(r, 0)).collect();
    let mut plan = RunnerPlanV1::new("run-1", RunnerMode::Oracle, trials);
    plan.remote = Some(RemotePlanV1 {
        docker_host: DOCKER_HOST.into(),
        laptop_host: "arch-laptop".into(),
        allowed_docker_hosts: vec![DOCKER_HOST.into()],
        concurrency,
        max_concurrency: 4,
        checkout_dir: checkout.display().to_string(),
        dataset_path: root.join("dataset.toml").display().to_string(),
        output_root: root.join("out").display().to_string(),
        socket_path: None,
        stop_file: None,
    });
    Fixture {
        _dir: dir,
        root,
        plan,
    }
}

fn kinds(outcome: &TrialOutcomeV1) -> Vec<TrialExceptionKind> {
    outcome.exceptions.iter().map(|e| e.kind).collect()
}

fn outcome_of(fx: &Fixture, key: &str) -> TrialOutcomeV1 {
    let path = fx
        .root
        .join("out/run-1/trials")
        .join(key)
        .join("outcome.json");
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

// ---- quoting, hosts, paths -----------------------------------------------------------------

#[cfg(unix)]
#[test]
fn remote_sh_and_posix_quote_survive_a_real_shell_for_hostile_text() {
    let hostile = [
        "plain",
        "",
        "it's",
        "back\\slash",
        "a b  c",
        "$HOME `id` $(id)",
        "semi; colon && pipe | star * q?",
        "quote\"double",
        "'; rm -rf / #",
        "new\nline",
        "ünïcode ✓",
        r"\'\\'",
    ];
    for text in hostile {
        // posix_quote alone, then the whole command through remote_sh (as sshd's shell does).
        let direct = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("printf %s {}", posix_quote(text)))
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&direct.stdout), text, "{text:?}");
        let wrapped = std::process::Command::new("sh")
            .arg("-c")
            .arg(remote_sh(&format!("printf %s {}", posix_quote(text))))
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&wrapped.stdout), text, "{text:?}");
    }
}

#[test]
fn remote_sh_has_the_fish_safe_shape() {
    assert_eq!(remote_sh("echo hi"), "sh -c 'echo hi'");
    assert_eq!(remote_sh("a'b"), "sh -c 'a'\"'\"'b'");
    assert_eq!(remote_sh("a\\b"), "sh -c 'a'\"\\\\\"'b'");
    assert_eq!(remote_sh(""), "sh -c ''");
    assert_eq!(posix_quote("/home/op/x-1.2"), "/home/op/x-1.2");
    assert_eq!(posix_quote("a b"), "'a b'");
}

#[test]
fn hosts_must_be_allowlisted_and_match_the_laptop_host() {
    let allowed = vec![DOCKER_HOST.to_string()];
    validate_hosts(DOCKER_HOST, "arch-laptop", &allowed).unwrap();
    for (docker, laptop, why) in [
        ("ssh://other", "other", "bench.docker_hosts"),
        (DOCKER_HOST, "other-host", "laptop_host"),
        (
            "tcp://arch-laptop:2375",
            "arch-laptop",
            "bench.docker_hosts",
        ),
        (DOCKER_HOST, "-oProxyCommand=x", "plain"),
        (DOCKER_HOST, "", "plain"),
        (DOCKER_HOST, "a b", "plain"),
    ] {
        let error = validate_hosts(docker, laptop, &allowed).unwrap_err();
        assert_eq!(error.kind, OpKind::HostRefused, "{docker} {laptop}");
        assert!(error.detail.contains(why), "{docker} {laptop}: {error}");
    }
    assert!(validate_ssh_host("op@arch-laptop").is_ok());
    assert!(validate_ssh_host("op@-bad").is_err());
}

#[test]
fn remote_paths_are_absolute_and_conservative() {
    assert!(validate_remote_path("/home/op/.rsi-bench/tb2-runner/abc").is_ok());
    for bad in [
        "relative/path",
        "/",
        "/home/op/../etc",
        "/home/op/a b",
        "/home/op/$(x)",
        "/home/op/it's",
        "/home/op/new\nline",
        "",
    ] {
        assert_eq!(
            validate_remote_path(bad).unwrap_err().kind,
            OpKind::UnsafePath,
            "{bad:?}"
        );
    }
    let identity = TrialIdentity {
        run_id: "r".into(),
        task: "t".into(),
        slot: 0,
        retry: 0,
    };
    assert!(TrialNames::new(&identity, "/home/op/../x").is_err());
}

#[test]
fn probe_parsing_keeps_unmeasured_capacity_unknown() {
    let facts = parse_probe("home=/home/op\nos=Linux\narch=x86_64\ndisk_kb=1024\nmem_kb=\n");
    assert_eq!(facts.disk_free_bytes, Some(1024 * 1024));
    assert_eq!(facts.mem_available_bytes, None);
    assert_eq!(parse_probe("garbage\ndisk_kb=abc\n").disk_free_bytes, None);
}

#[test]
fn ssh_and_docker_argv_are_exact() {
    let world = World::new(&fixture(&[("alpha", 'a')], 1).plan);
    let remote = Remote {
        exec: &world,
        host: "arch-laptop",
    };
    let spec = remote.spec("echo 'x'", Duration::from_secs(9));
    assert_eq!(spec.program, "ssh");
    assert_eq!(
        spec.args,
        [
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=15",
            "-o",
            "ServerAliveInterval=30",
            "-o",
            "ServerAliveCountMax=6",
            "--",
            "arch-laptop",
            "sh -c 'echo '\"'\"'x'\"'\"",
        ]
    );
    assert_eq!(spec.timeout, Duration::from_secs(9));
    let docker = super::docker::DockerClient {
        exec: &world,
        host: DOCKER_HOST,
    };
    let spec = docker.spec(&["ps", "-q"], Duration::from_secs(3));
    assert_eq!(spec.program, "docker");
    assert_eq!(spec.args, ["-H", DOCKER_HOST, "ps", "-q"]);
}

#[test]
fn ssh_failures_are_typed_and_an_exit_255_is_an_ssh_failure_not_a_command_result() {
    let mut world = World::new(&fixture(&[("alpha", 'a')], 1).plan);
    world.ssh_down = true;
    let remote = Remote {
        exec: &world,
        host: "arch-laptop",
    };
    let error = remote.sh("true", Duration::from_secs(1)).unwrap_err();
    assert_eq!(error.kind, OpKind::SshFailed);
    assert!(error.detail.contains("Connection refused"), "{error}");
}

// ---- the real executor ---------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn the_system_executor_clears_ambient_docker_variables_and_bounds_time_and_output() {
    use std::os::unix::fs::PermissionsExt;

    // Exercise the real allowlisted entrypoints with local shims; no remote host.
    let bin = tempfile::tempdir().unwrap();
    for program in APPROVED_EXECUTABLES {
        let shim = bin.path().join(program);
        std::fs::write(&shim, "#!/bin/sh\nexec /bin/sh \"$@\"\n").unwrap();
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut paths = vec![bin.path().to_path_buf()];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let path = std::env::join_paths(paths).unwrap();
    temp_env::with_vars(
        [
            ("PATH", Some(path)),
            ("DOCKER_HOST", Some("tcp://evil:2375".into())),
            ("DOCKER_CONTEXT", Some("evil".into())),
            ("COMPOSE_FILE", Some("/evil.yml".into())),
        ],
        || {
            let exec = SystemExecutor;
            let probe = concat!(
                "printf '%s|%s|%s' ",
                "\"${DOCKER_HOST-unset}\" \"${DOCKER_CONTEXT-unset}\" \"${COMPOSE_FILE-unset}\""
            );
            let mut spec = CommandSpec::new(
                "docker",
                vec!["-c".into(), probe.into()],
                Duration::from_secs(10),
            );
            for program in APPROVED_EXECUTABLES {
                spec.program = program.into();
                assert_eq!(exec.run(&spec).unwrap().stdout_text(), "unset|unset|unset");
            }

            spec.args = vec!["-c".into(), "exec sleep 5".into()];
            spec.timeout = Duration::from_millis(100);
            let started = std::time::Instant::now();
            assert_eq!(exec.run(&spec), Err(ExecError::Timeout));
            assert!(started.elapsed() < Duration::from_secs(4));

            spec.args = vec!["-c".into(), "head -c 100000 /dev/zero; exit 7".into()];
            spec.timeout = Duration::from_secs(10);
            spec.max_output = 1000;
            let out = exec.run(&spec).unwrap();
            assert_eq!(
                (out.code, out.stdout.len(), out.truncated),
                (Some(7), 1000, true)
            );

            spec.args = vec!["-c".into(), "cat".into()];
            spec.stdin = Some(b"fed".to_vec());
            assert_eq!(exec.run(&spec).unwrap().stdout, b"fed");

            let missing =
                CommandSpec::new("definitely-not-a-program", vec![], Duration::from_secs(1));
            assert!(matches!(exec.run(&missing), Err(ExecError::Spawn(_))));
        },
    );
}

#[test]
fn the_executor_accepts_only_the_approved_transport_programs() {
    assert_eq!(APPROVED_EXECUTABLES, ["docker", "ssh"]);
    for program in [
        "python",
        "python3",
        "uv",
        "pip",
        "harbor",
        "sh",
        "/usr/bin/docker",
        "./ssh",
    ] {
        let spec = CommandSpec::new(program, vec![], Duration::from_secs(1));
        assert_eq!(
            SystemExecutor.run(&spec),
            Err(ExecError::Spawn(format!(
                "runner executable {program:?} is not approved (expected docker or ssh)"
            )))
        );
    }
}

// ---- compose and images --------------------------------------------------------------------

#[test]
fn pinned_references_drop_the_tag_and_keep_only_the_digest() {
    let d = digest('a');
    assert_eq!(
        pinned_ref("alexgshaw/foo:20251031", &d).unwrap(),
        format!("alexgshaw/foo@{d}")
    );
    assert_eq!(
        pinned_ref("localhost:5000/foo", &d).unwrap(),
        format!("localhost:5000/foo@{d}")
    );
    assert_eq!(
        pinned_ref("localhost:5000/foo:1", &d).unwrap(),
        format!("localhost:5000/foo@{d}")
    );
    assert_eq!(pinned_ref("foo", &d).unwrap(), format!("foo@{d}"));
    assert_eq!(
        pinned_ref(&format!("foo:1@{}", digest('b')), &d).unwrap(),
        format!("foo@{d}")
    );
    for bad in ["sha256:abc", "md5:abc", &digest('A'), ""] {
        assert_eq!(
            pinned_ref("foo", bad).unwrap_err().kind,
            OpKind::InvalidPlan,
            "{bad}"
        );
    }
}

#[test]
fn compose_pins_the_digest_sets_exact_limits_and_binds_remote_log_directories() {
    let fx = fixture(&[("alpha", 'a')], 1);
    let trial = &fx.plan.trials[0];
    let names = TrialNames::new(&trial.identity, "/home/op/.rsi-bench/tb2-runner/r").unwrap();
    let config = compose_config(trial, &names).unwrap();
    let service = &config["services"]["main"];
    assert_eq!(config["name"], names.project.as_str());
    assert_eq!(
        service["image"],
        format!("registry.example/tb2/alpha@{}", digest('a'))
    );
    assert_eq!(service["platform"], "linux/amd64");
    assert_eq!(service["pull_policy"], "never");
    assert_eq!(
        service["command"],
        json!(["sh", "-c", "exec sleep infinity"])
    );
    assert!(service.get("build").is_none() && service.get("entrypoint").is_none());
    assert_eq!(
        service["deploy"]["resources"]["limits"],
        json!({"cpus": "1", "memory": "2048M"})
    );
    assert_eq!(service["labels"][LABEL_RUN], "run-1");
    assert_eq!(service["labels"][LABEL_TASK], "alpha");
    assert_eq!(service["labels"][LABEL_SLOT], "0");
    assert_eq!(service["labels"][LABEL_RETRY], "0");
    let binds: Vec<(String, String)> = service["volumes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| {
            assert_eq!(v["type"], "bind");
            (
                v["source"].as_str().unwrap().into(),
                v["target"].as_str().unwrap().into(),
            )
        })
        .collect();
    assert_eq!(
        binds,
        [
            (
                "/home/op/.rsi-bench/tb2-runner/r/alpha-s0-r0/verifier".to_string(),
                "/logs/verifier".to_string()
            ),
            (
                "/home/op/.rsi-bench/tb2-runner/r/alpha-s0-r0/agent".to_string(),
                "/logs/agent".to_string()
            ),
            (
                "/home/op/.rsi-bench/tb2-runner/r/alpha-s0-r0/artifacts".to_string(),
                "/logs/artifacts".to_string()
            ),
        ]
    );
    // Heavier limits are carried exactly.
    let mut big = trial.clone();
    (big.cpus, big.memory_mb) = (4, 8192);
    let limits =
        compose_config(&big, &names).unwrap()["services"]["main"]["deploy"]["resources"]["limits"]
            .clone();
    assert_eq!(limits, json!({"cpus": "4", "memory": "8192M"}));
}

#[test]
fn project_names_are_unique_per_run_task_slot_and_retry() {
    let id = |run: &str, task: &str, slot, retry| TrialIdentity {
        run_id: run.into(),
        task: task.into(),
        slot,
        retry,
    };
    let root = "/home/op/x";
    let names = [
        TrialNames::new(&id("r", "a", 0, 0), root).unwrap(),
        TrialNames::new(&id("r", "a", 1, 0), root).unwrap(),
        TrialNames::new(&id("r", "a", 0, 1), root).unwrap(),
        TrialNames::new(&id("r", "b", 0, 0), root).unwrap(),
        TrialNames::new(&id("q", "a", 0, 0), root).unwrap(),
    ];
    let projects: std::collections::BTreeSet<_> = names.iter().map(|n| n.project.clone()).collect();
    assert_eq!(projects.len(), names.len());
    assert!(
        names[0]
            .project
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    );
}

// ---- container verification ----------------------------------------------------------------

fn good_inspect(world: &World) -> (Value, PlannedTrial, TrialNames) {
    let trial = world.plan.trials[0].clone();
    (
        world.inspect_json(&trial),
        trial.clone(),
        world.names_for(&trial),
    )
}

#[test]
fn a_matching_container_verifies_and_every_mismatch_is_typed() {
    let world = World::new(&fixture(&[("alpha", 'a')], 1).plan);
    let (good, trial, names) = good_inspect(&world);
    verify_container(&good, &trial, &names).unwrap();

    let edit = |f: &dyn Fn(&mut Value)| {
        let mut v = good.clone();
        f(&mut v);
        verify_container(&v, &trial, &names).unwrap_err()
    };
    assert_eq!(
        edit(&|v| v["State"]["Running"] = json!(false)).kind,
        OpKind::NotReady
    );
    for label in [
        LABEL_PROJECT,
        LABEL_SERVICE,
        LABEL_RUN,
        LABEL_TASK,
        LABEL_SLOT,
        LABEL_RETRY,
    ] {
        let error = edit(&|v| v["Config"]["Labels"][label] = json!("other"));
        assert_eq!(error.kind, OpKind::LabelMismatch, "{label}");
        assert!(error.detail.contains(label), "{error}");
    }
    assert_eq!(
        edit(&|v| {
            v["Config"]["Labels"]
                .as_object_mut()
                .unwrap()
                .remove(LABEL_RUN);
        })
        .kind,
        OpKind::LabelMismatch
    );
    assert_eq!(
        edit(&|v| v["HostConfig"]["Memory"] = json!(1)).kind,
        OpKind::LimitMismatch
    );
    assert_eq!(
        edit(&|v| v["HostConfig"]["NanoCpus"] = json!(2_000_000_000u64)).kind,
        OpKind::LimitMismatch
    );
    assert_eq!(
        edit(&|v| {
            v["Mounts"].as_array_mut().unwrap().pop();
        })
        .kind,
        OpKind::LimitMismatch
    );
    assert_eq!(
        edit(&|v| v["Mounts"][0]["RW"] = json!(false)).kind,
        OpKind::LimitMismatch
    );
    assert_eq!(
        edit(&|v| v["Mounts"][0]["Source"] = json!("/elsewhere")).kind,
        OpKind::LimitMismatch
    );
}

// ---- preflight -----------------------------------------------------------------------------

fn preflight(
    world: &World,
    plan: &RunnerPlanV1,
) -> Result<runner::Preflighted, super::transport::OpError> {
    runner::preflight(plan, world)
}

fn expect_refusal(mutate: impl FnOnce(&mut World, &mut RunnerPlanV1), kind: OpKind) {
    let mut fx = fixture(&[("alpha", 'a'), ("beta", 'b')], 2);
    let mut world = World::new(&fx.plan);
    mutate(&mut world, &mut fx.plan);
    world.plan = fx.plan.clone();
    let error = preflight(&world, &fx.plan)
        .err()
        .unwrap_or_else(|| panic!("{kind:?} was not refused"));
    assert_eq!(error.kind, kind, "{error}");
}

#[test]
fn preflight_passes_and_uses_only_docker_and_ssh() {
    let fx = fixture(&[("alpha", 'a'), ("beta", 'b')], 2);
    let world = World::new(&fx.plan);
    let ready = preflight(&world, &fx.plan).unwrap();
    let report = &ready.report;
    assert_eq!((report.tasks, report.trials, report.concurrency), (2, 2, 2));
    // Largest selected shard is 3 GiB: disk floor 3 + 10 GiB, memory floor 2 x 4 + 4 GiB.
    assert_eq!(report.largest_shard_bytes, 3 * GIB);
    assert_eq!(report.need_disk_bytes, 13 * GIB);
    assert_eq!(report.need_mem_bytes, 12 * GIB);
    assert_eq!(report.docker.compose_version, "2.29.1");
    assert_eq!(report.remote.home, HOME);
    for command in world.log() {
        assert!(
            ["docker", "ssh"].contains(&command.program.as_str()),
            "{}",
            command.program
        );
    }
    // The probe measured the Docker root the daemon reported.
    let joined = world.joined().join("\n");
    assert!(joined.contains("d=/var/lib/docker"), "{joined}");
    // Read-only: nothing was pulled, created or started.
    for forbidden in ["pull", "mkdir", " up ", " down ", "rm -rf", "cp "] {
        assert!(!joined.contains(forbidden), "{forbidden}: {joined}");
    }
}

#[test]
fn preflight_refuses_low_disk_with_the_largest_shard_plus_ten_gib() {
    expect_refusal(
        |w, _| w.disk_kb = Some((13 * GIB - 1) / 1024),
        OpKind::DiskInsufficient,
    );
    // Exactly the floor passes.
    let fx = fixture(&[("alpha", 'a'), ("beta", 'b')], 2);
    let mut world = World::new(&fx.plan);
    world.disk_kb = Some(13 * GIB / 1024);
    assert!(preflight(&world, &fx.plan).is_ok());
}

#[test]
fn preflight_refuses_low_memory_by_concurrency_times_four_gib_plus_four() {
    expect_refusal(
        |w, _| w.mem_kb = Some((12 * GIB - 1024) / 1024),
        OpKind::MemoryInsufficient,
    );
    let fx = fixture(&[("alpha", 'a'), ("beta", 'b')], 2);
    let mut world = World::new(&fx.plan);
    world.mem_kb = Some(12 * GIB / 1024);
    assert!(preflight(&world, &fx.plan).is_ok());
    // One concurrent trial lowers the floor to 8 GiB.
    let mut one = fx.plan.clone();
    one.remote.as_mut().unwrap().concurrency = 1;
    world.mem_kb = Some(8 * GIB / 1024);
    assert_eq!(
        preflight(&world, &one).unwrap().report.need_mem_bytes,
        8 * GIB
    );
}

#[test]
fn the_effective_concurrency_is_the_smaller_of_the_job_and_the_operator_cap() {
    let mut fx = fixture(&[("alpha", 'a'), ("beta", 'b')], 8);
    fx.plan.remote.as_mut().unwrap().max_concurrency = 1;
    let world = World::new(&fx.plan);
    assert_eq!(preflight(&world, &fx.plan).unwrap().report.concurrency, 1);
}

#[test]
fn a_task_memory_limit_larger_than_the_available_memory_is_refused() {
    let mut fx = fixture(&[("alpha", 'a')], 1);
    // A 8 GiB task needs its own limit plus the 4 GiB reserve, more than the 4 x 1 + 4 floor.
    let big = &mut fx.plan.trials[0];
    big.memory_mb = 8192;
    let text = std::fs::read_to_string(fx.root.join("dataset.toml")).unwrap();
    std::fs::write(
        fx.root.join("dataset.toml"),
        text.replace("memory_mb = 2048", "memory_mb = 8192"),
    )
    .unwrap();
    let mut world = World::new(&fx.plan);
    world.mem_kb = Some(11 * GIB / 1024);
    assert_eq!(
        preflight(&world, &fx.plan).err().unwrap().kind,
        OpKind::MemoryInsufficient
    );
    world.mem_kb = Some(12 * GIB / 1024);
    assert!(preflight(&world, &fx.plan).is_ok());
}

#[test]
fn capacity_that_cannot_be_measured_fails_closed() {
    expect_refusal(|w, _| w.disk_kb = None, OpKind::CapacityUnmeasured);
    expect_refusal(|w, _| w.mem_kb = None, OpKind::CapacityUnmeasured);
}

#[test]
fn preflight_refuses_a_foreign_platform_a_missing_compose_v2_and_an_unreachable_daemon() {
    expect_refusal(|w, _| w.uname_m = "aarch64", OpKind::UnsupportedPlatform);
    expect_refusal(
        |w, _| w.compose_version = "1.29.2",
        OpKind::ComposeUnavailable,
    );
    expect_refusal(|w, _| w.docker_down = true, OpKind::DockerUnreachable);
    expect_refusal(|w, _| w.ssh_down = true, OpKind::SshFailed);
}

#[test]
fn preflight_refuses_hosts_that_are_not_the_allowlisted_laptop() {
    expect_refusal(
        |_, p| p.remote.as_mut().unwrap().allowed_docker_hosts.clear(),
        OpKind::HostRefused,
    );
    expect_refusal(
        |_, p| p.remote.as_mut().unwrap().laptop_host = "elsewhere".into(),
        OpKind::HostRefused,
    );
}

#[test]
fn preflight_refuses_a_plan_that_differs_from_the_pin_before_any_remote_action() {
    let mutations: [(&str, fn(&mut RunnerPlanV1)); 5] = [
        ("digest", |p| p.trials[0].image_digest = digest('9')),
        ("image", |p| p.trials[0].image = "other/image:1".into()),
        ("memory", |p| p.trials[0].memory_mb = 1024),
        ("cpus", |p| p.trials[0].cpus = 8),
        ("agent deadline", |p| p.trials[0].agent_timeout_sec = 1),
    ];
    for (what, mutate) in mutations {
        let mut fx = fixture(&[("alpha", 'a'), ("beta", 'b')], 2);
        mutate(&mut fx.plan);
        let world = World::new(&fx.plan);
        let error = preflight(&world, &fx.plan)
            .err()
            .unwrap_or_else(|| panic!("{what}"));
        assert_eq!(error.kind, OpKind::PinDrift, "{what}: {error}");
        assert!(
            world.log().is_empty(),
            "{what}: nothing remote may run before the pin holds"
        );
    }
}

#[test]
fn preflight_refuses_checkout_drift_unknown_tasks_and_unsupported_layouts() {
    let fx = fixture(&[("alpha", 'a')], 1);
    let world = World::new(&fx.plan);
    std::fs::write(fx.root.join("checkout/alpha/task.toml"), "# changed\n").unwrap();
    assert_eq!(
        preflight(&world, &fx.plan).err().unwrap().kind,
        OpKind::PinDrift
    );

    let fx = fixture(&[("alpha", 'a')], 1);
    let mut unknown = fx.plan.clone();
    unknown.trials[0].identity.task = "ghost".into();
    assert_eq!(
        preflight(&World::new(&unknown), &unknown)
            .err()
            .unwrap()
            .kind,
        OpKind::PinDrift
    );

    let fx = fixture(&[("alpha", 'a')], 1);
    std::fs::create_dir_all(fx.root.join("checkout/alpha/environment")).unwrap();
    std::fs::write(
        fx.root
            .join("checkout/alpha/environment/docker-compose.yaml"),
        "services: {}\n",
    )
    .unwrap();
    let error = preflight(&World::new(&fx.plan), &fx.plan).err().unwrap();
    assert_eq!(error.kind, OpKind::Unsupported);
    assert!(error.detail.contains("unsupported_task_layout"), "{error}");

    let fx = fixture(&[("alpha", 'a')], 1);
    std::fs::remove_file(fx.root.join("checkout/alpha/tests/test.sh")).unwrap();
    assert_eq!(
        preflight(&World::new(&fx.plan), &fx.plan)
            .err()
            .unwrap()
            .kind,
        OpKind::PinDrift
    );
}

#[test]
fn preflight_refuses_an_invalid_harness_socket_and_a_plan_without_a_remote() {
    let mut fx = fixture(&[("alpha", 'a')], 1);
    fx.plan.mode = RunnerMode::Harness;
    // Harness never falls back to the daemon's general socket or an unchecked path.
    fx.plan.remote.as_mut().unwrap().socket_path = Some("/o/run-1/adapter.sock".into());
    assert_eq!(
        preflight(&World::new(&fx.plan), &fx.plan)
            .err()
            .unwrap()
            .kind,
        OpKind::Unsupported
    );
    let mut fx = fixture(&[("alpha", 'a')], 1);
    fx.plan.remote = None;
    assert_eq!(
        preflight(&World::new(&fx.plan), &fx.plan)
            .err()
            .unwrap()
            .kind,
        OpKind::InvalidPlan
    );
}

// ---- runs ----------------------------------------------------------------------------------

fn run(
    world: &World,
    plan: &RunnerPlanV1,
) -> Result<runner::RunSummary, super::transport::OpError> {
    runner::run(plan, world)
}

fn index_of(joined: &[String], needle: &str) -> usize {
    joined
        .iter()
        .position(|c| c.contains(needle))
        .unwrap_or_else(|| panic!("no command containing {needle:?}"))
}

/// Bound the caller's wait even if the runner regresses to joining a stuck watcher/worker.
fn run_with_panicking_executor(command: &'static str) -> (super::transport::OpError, usize) {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc,
    };

    struct PanickingExecutor {
        world: World,
        command: &'static str,
        panics: AtomicUsize,
    }

    impl CommandExecutor for PanickingExecutor {
        fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, ExecError> {
            if spec.program == "docker" && spec.args.get(2).is_some_and(|arg| arg == self.command) {
                self.panics.fetch_add(1, Ordering::SeqCst);
                panic!("fake Docker {} panic", self.command);
            }
            self.world.run(spec)
        }
    }

    let (sender, receiver) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let mut fx = fixture(&[("alpha", 'a')], 2);
        // Only one reservation fits. A panic must give memory back to the other worker.
        fx.plan.trials[0].memory_mb = 8192;
        let mut second = fx.plan.trials[0].clone();
        second.identity.slot = 1;
        fx.plan.trials.push(second);
        let dataset_path = fx.root.join("dataset.toml");
        let dataset = std::fs::read_to_string(&dataset_path).unwrap();
        std::fs::write(
            dataset_path,
            dataset.replace("memory_mb = 2048", "memory_mb = 8192"),
        )
        .unwrap();
        let mut world = World::new(&fx.plan);
        world.mem_kb = Some(12 * GIB / 1024);
        let exec = PanickingExecutor {
            world,
            command,
            panics: AtomicUsize::new(0),
        };
        let result = runner::run(&fx.plan, &exec);
        let _ = sender.send((result, exec.panics.load(Ordering::SeqCst)));
    });
    let (result, panics) = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("a panicking runner must return within five seconds");
    worker.join().unwrap();
    (result.unwrap_err(), panics)
}

#[test]
fn trial_worker_panic_returns_a_typed_failure_without_hanging() {
    let (error, panics) = run_with_panicking_executor("inspect");
    assert_eq!(
        panics, 2,
        "both workers must regain their memory reservation"
    );
    assert_eq!(error.kind, OpKind::RunnerPanicked);
    assert!(error.detail.contains("trial alpha-"), "{error}");
    assert!(
        error.detail.contains("fake Docker inspect panic"),
        "{error}"
    );
    assert_eq!(error.kind.as_str(), "runner_panicked");
    assert_eq!(error.kind.exit_code(), 22);
}

#[test]
fn shard_panic_wakes_the_cancellation_watcher_on_unwind() {
    let (error, panics) = run_with_panicking_executor("pull");
    assert_eq!(panics, 1);
    assert_eq!(error.kind, OpKind::RunnerPanicked);
    assert!(
        error
            .detail
            .contains("run panicked: fake Docker pull panic"),
        "{error}"
    );
}

#[test]
fn a_synthetic_remote_oracle_run_passes_without_models_harbor_or_uv() {
    let fx = fixture(&[("alpha", 'a'), ("beta", 'b')], 2);
    let world = World::new(&fx.plan);
    let summary = run(&world, &fx.plan).unwrap();
    assert_eq!(
        (summary.total, summary.passed, summary.model_sessions),
        (2, 2, 0)
    );
    assert!(summary.all_passed_and_clean());
    for t in &summary.trials {
        assert_eq!(
            (t.reward, t.reward_valid, t.collected, t.cleaned),
            (Some(1.0), true, true, true)
        );
        assert_eq!(t.kept_evidence, None);
    }
    // Only docker and ssh ran; nothing reached for harbor, uv, python or a model.
    let log = world.log();
    let programs: std::collections::BTreeSet<_> = log.iter().map(|c| c.program.as_str()).collect();
    assert_eq!(
        programs,
        std::collections::BTreeSet::from(APPROVED_EXECUTABLES)
    );
    for command in &log {
        assert!(
            APPROVED_EXECUTABLES.contains(&command.program.as_str()),
            "{}",
            command.program
        );
    }
    let joined = world.joined();
    for forbidden in ["harbor", "uv ", "python", "prune", "rmi ", "system "] {
        assert!(!joined.iter().any(|c| c.contains(forbidden)), "{forbidden}");
    }
    // Evidence on the hub: one outcome per trial and the summary.
    assert_eq!(outcome_of(&fx, "alpha-s0-r0").reward, Some(1.0));
    assert_eq!(outcome_of(&fx, "beta-s0-r0").model_sessions, 0);
    let summary_file: Value =
        serde_json::from_slice(&std::fs::read(fx.root.join("out/run-1/summary.json")).unwrap())
            .unwrap();
    assert_eq!(summary_file["passed"], 2);
    assert!(
        fx.root
            .join("out/run-1/trials/alpha-s0-r0/compose.json")
            .is_file()
    );
}

#[test]
fn one_trial_runs_pull_then_up_then_exec_then_collect_then_scoped_down() {
    let fx = fixture(&[("alpha", 'a')], 1);
    let world = World::new(&fx.plan);
    run(&world, &fx.plan).unwrap();
    let joined = world.joined();
    let names = world.names_for(&fx.plan.trials[0]);
    let order = [
        "docker -H ssh://arch-laptop pull --quiet --platform linux/amd64 registry.example/tb2/alpha@sha256:",
        "mkdir -p --",
        "compose -p",
        " up --detach --wait --wait-timeout ",
        "cp ",
        "/solution/solve.sh",
        "/tests/test.sh",
        ":/logs/. ",
        " down --volumes --remove-orphans --timeout 10",
        "rm -rf --",
    ];
    let positions: Vec<usize> = order.iter().map(|n| index_of(&joined, n)).collect();
    assert!(
        positions.windows(2).all(|w| w[0] <= w[1]),
        "{positions:?}\n{}",
        joined.join("\n")
    );

    // up: no build, never a pull, the generated file, the trial's own project.
    let up = &joined[index_of(&joined, " up --detach")];
    for fragment in [
        "-p ".to_string() + &names.project,
        "--no-build".into(),
        "--pull never".into(),
        "compose.json".into(),
    ] {
        assert!(up.contains(&fragment), "{fragment}: {up}");
    }
    // down: scoped to the project, with volumes and orphans, and nothing global.
    let down = &joined[index_of(&joined, " down ")];
    assert!(down.contains(&format!("-p {}", names.project)), "{down}");
    assert_eq!(joined.iter().filter(|c| c.contains(" down ")).count(), 1);
    // The laptop directory removed is exactly the trial's own.
    let rm = &joined[index_of(&joined, "rm -rf --")];
    assert!(rm.contains(&names.trial_dir), "{rm}");
    assert!(names.trial_dir.starts_with(&names.run_root));
}

#[test]
fn execs_use_bash_c_the_declared_users_and_the_noninteractive_default() {
    let fx = fixture(&[("alpha", 'a')], 1);
    let world = World::new(&fx.plan);
    run(&world, &fx.plan).unwrap();
    let execs: Vec<Vec<String>> = world
        .log()
        .into_iter()
        .filter(|c| c.program == "docker" && c.args.get(2).is_some_and(|a| a == "exec"))
        .map(|c| c.args)
        .collect();
    let find = |needle: &str| {
        execs
            .iter()
            .find(|a| a.iter().any(|x| x.contains(needle)))
            .unwrap()
    };
    let solve = find("solve.sh) >");
    assert!(solve.windows(2).any(|w| w == ["bash", "-c"]), "{solve:?}");
    assert!(
        solve
            .windows(2)
            .any(|w| w == ["-e", "DEBIAN_FRONTEND=noninteractive"]),
        "{solve:?}"
    );
    assert!(
        !solve.contains(&"-u".to_string()),
        "image USER, not forced root: {solve:?}"
    );
    assert!(solve.last().unwrap().contains("/logs/agent/oracle.txt"));
    let verify = find("test.sh) >");
    assert!(
        !verify.contains(&"-u".to_string()) && !verify.iter().any(|x| x.starts_with("DEBIAN")),
        "{verify:?}"
    );
    assert!(
        verify
            .last()
            .unwrap()
            .contains("/logs/verifier/test-stdout.txt")
    );
    // Permission changes are root's.
    let chmod = find("chmod +x /solution/solve.sh");
    assert!(chmod.windows(2).any(|w| w == ["-u", "root"]), "{chmod:?}");
    // Uploads copy a directory's contents into the container; nothing is mounted from the hub.
    let uploads: Vec<String> = world
        .joined()
        .into_iter()
        .filter(|c| c.contains(" cp "))
        .collect();
    assert_eq!(
        uploads.len(),
        2 + 1,
        "solution, tests and the log collection: {uploads:?}"
    );
    assert!(uploads.iter().any(|c| c.ends_with(":/solution")));
    assert!(uploads.iter().any(|c| c.ends_with(":/tests")));
}

#[test]
fn settle_keeps_the_container_alive_and_collects_every_trials_logs() {
    let fx = fixture(&[("alpha", 'a'), ("beta", 'b')], 2);
    let world = World::new(&fx.plan);
    let summary = run(&world, &fx.plan).unwrap();
    assert!(summary.trials.iter().all(|t| t.collected && t.cleaned));
    let joined = world.joined();
    for trial in &fx.plan.trials {
        let names = world.names_for(trial);
        let id = container_id(&names.project);
        let settle = joined
            .iter()
            .position(|c| c.contains(&id) && c.contains(SETTLE_SCRIPT))
            .unwrap();
        let copy = index_of(&joined, &format!("{id}:/logs/."));
        assert!(settle < copy, "settle before evidence copy");
    }
}

#[cfg(unix)]
#[test]
fn settle_shell_preserves_its_ancestors_and_kills_children_and_siblings() {
    // Execute the real shell body against a fake /proc and a shell-builtin kill double.
    // No host process can be signalled, and no namespace privileges or Docker are needed.
    let dir = tempfile::tempdir().unwrap();
    let proc = dir.path().join("proc");
    std::fs::create_dir(&proc).unwrap();
    for (pid, parent, state) in [
        (1, 0, "S"),
        (21, 1, "S"),
        (20, 21, "S"),
        (100, 20, "S"),
        (30, 20, "S"),
        (40, 100, "S"),
        (50, 1, "Z"),
    ] {
        let path = proc.join(pid.to_string());
        std::fs::create_dir(&path).unwrap();
        std::fs::write(
            path.join("status"),
            format!("State:\t{state}\nPPid:\t{parent}\n"),
        )
        .unwrap();
    }
    let script = SETTLE_SCRIPT
        .replace("me=$$", "me=100")
        .replace("/proc/", "\"$FAKE_PROC\"/");
    let harness = format!(
        r#"
kill() {{
  printf '%s\n' "$2" >> "$KILLED"
  printf 'State:\tZ\n' > "$FAKE_PROC/$2/status"
}}
sleep() {{ :; }}
{script}
"#
    );
    let killed = dir.path().join("killed");
    let output = std::process::Command::new("sh")
        .args(["-c", &harness])
        .env("FAKE_PROC", &proc)
        .env("KILLED", &killed)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let mut victims: Vec<String> = std::fs::read_to_string(killed)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    victims.sort();
    assert_eq!(victims, ["30", "40"]);
}

#[test]
fn failed_settle_and_compose_timeout_force_remove_only_owned_resources() {
    let fx = fixture(&[("alpha", 'a')], 1);
    let mut world = World::new(&fx.plan);
    world.settle_fails = true;
    world.down_times_out = true;
    world
        .owned
        .lock()
        .unwrap()
        .insert("unrelated-project".into(), [true; 3]);
    let summary = run(&world, &fx.plan).unwrap();
    let outcome = outcome_of(&fx, "alpha-s0-r0");
    assert!(outcome.collection.error.as_deref().unwrap().contains("137"));
    assert!(!summary.trials[0].collected);
    assert!(summary.trials[0].cleaned, "{:?}", outcome.cleanup);
    let names = world.names_for(&fx.plan.trials[0]);
    let resources = world.owned.lock().unwrap();
    assert_eq!(resources[&names.project], [false; 3]);
    assert_eq!(resources["unrelated-project"], [true; 3]);
    let joined = world.joined();
    for (kind, needle) in [
        (0, " rm -f --volumes "),
        (1, " network rm "),
        (2, " volume rm -f "),
    ] {
        let command = &joined[index_of(&joined, needle)];
        assert!(
            command.ends_with(&owned_id(&names.project, kind)),
            "{command}"
        );
    }
    assert!(index_of(&joined, " rm -f --volumes ") < index_of(&joined, " network rm "));
    let log = world.log();
    let down = log
        .iter()
        .find(|c| c.args.iter().any(|a| a == "down"))
        .unwrap();
    assert!(down.timeout <= Duration::from_secs(30));
    for command in log
        .iter()
        .filter(|c| c.args.iter().any(|a| a == "rm" || a == "ls"))
    {
        assert!(command.timeout <= Duration::from_secs(10));
    }
}

#[test]
fn cleanup_cannot_claim_success_when_force_removal_leaves_owned_objects() {
    let fx = fixture(&[("alpha", 'a')], 1);
    let mut world = World::new(&fx.plan);
    world.settle_fails = true;
    world.down_times_out = true;
    world.force_remove_fails = true;
    let summary = run(&world, &fx.plan).unwrap();
    assert!(!summary.trials[0].cleaned);
    let names = world.names_for(&fx.plan.trials[0]);
    assert_eq!(world.owned.lock().unwrap()[&names.project], [true; 3]);
    let outcome = outcome_of(&fx, "alpha-s0-r0");
    for kind in ["containers", "networks", "volumes"] {
        assert!(outcome.cleanup.error.as_deref().unwrap().contains(kind));
    }
}

#[test]
fn an_agent_timeout_settles_the_container_then_still_verifies() {
    let fx = fixture(&[("alpha", 'a')], 1);
    let mut world = World::new(&fx.plan);
    world.solve_times_out = true;
    let summary = run(&world, &fx.plan).unwrap();
    let outcome = outcome_of(&fx, "alpha-s0-r0");
    assert_eq!(kinds(&outcome), [TrialExceptionKind::AgentTimeout]);
    assert!(outcome.reward_valid, "the verifier still ran");
    assert!(summary.trials[0].cleaned);
    let joined = world.joined();
    assert!(
        joined.iter().filter(|c| c.contains("me=$$")).count() >= 1,
        "execs were settled as root"
    );
    assert!(index_of(&joined, "me=$$") < index_of(&joined, "/tests/test.sh"));
    assert!(joined[index_of(&joined, "me=$$")].contains("-u root"));
    assert!(SETTLE_SCRIPT.contains("kill -9"));
}

#[test]
fn a_nonzero_solution_exit_is_recorded_and_the_reward_decides_the_grade() {
    let fx = fixture(&[("alpha", 'a')], 1);
    let mut world = World::new(&fx.plan);
    world.solve_code = 7;
    world.test_code = 1;
    world.reward = Some(r#"{"reward": 0.0}"#);
    let summary = run(&world, &fx.plan).unwrap();
    let outcome = outcome_of(&fx, "alpha-s0-r0");
    assert_eq!(
        (outcome.oracle_exit_code, outcome.verifier_exit_code),
        (Some(7), Some(1))
    );
    assert!(outcome.reward_valid && !outcome.passed());
    assert_eq!(summary.passed, 0);
    assert!(!summary.all_passed_and_clean());
}

#[test]
fn a_missing_reward_is_invalid_not_a_zero() {
    let fx = fixture(&[("alpha", 'a')], 1);
    let mut world = World::new(&fx.plan);
    world.reward = None;
    let summary = run(&world, &fx.plan).unwrap();
    assert!(!summary.trials[0].reward_valid && summary.trials[0].reward.is_none());
    assert_eq!(
        summary.trials[0].invalid_cause.as_deref(),
        Some("reward_missing")
    );
}

#[test]
fn a_failed_pull_is_fatal_before_any_container_starts() {
    let fx = fixture(&[("alpha", 'a')], 1);
    let mut world = World::new(&fx.plan);
    world.pull_fails = true;
    let error = run(&world, &fx.plan).unwrap_err();
    assert_eq!(error.kind, OpKind::ImagePullFailed);
    assert_eq!(error.kind.exit_code(), 22);
    let joined = world.joined();
    assert!(
        !joined
            .iter()
            .any(|c| c.contains(" up ") || c.contains("mkdir")),
        "{joined:?}"
    );
}

#[test]
fn a_pulled_image_with_another_digest_is_a_pin_failure_before_model_spend() {
    let fx = fixture(&[("alpha", 'a')], 1);
    let mut world = World::new(&fx.plan);
    world.local_digests = Some(vec![format!("registry.example/tb2/alpha@{}", digest('f'))]);
    let error = run(&world, &fx.plan).unwrap_err();
    assert_eq!(error.kind, OpKind::DigestMismatch);
    assert_eq!(error.kind.exit_code(), 21);
    assert!(!world.joined().iter().any(|c| c.contains(" up ")));
}

#[test]
fn a_running_container_with_the_wrong_digest_or_labels_never_runs_the_oracle() {
    let cases: [(&str, fn(&mut World), TrialExceptionKind, &str); 6] = [
        (
            "digest",
            |w| w.container_digests = Some(vec![format!("x@{}", digest('f'))]),
            TrialExceptionKind::EnvironmentStartFailed,
            "digest_mismatch",
        ),
        (
            "label",
            |w| w.label_run = Some("someone-elses-run"),
            TrialExceptionKind::EnvironmentStartFailed,
            "label_mismatch",
        ),
        (
            "memory limit",
            |w| w.memory_limit_mb = Some(1024),
            TrialExceptionKind::EnvironmentStartFailed,
            "limit_mismatch",
        ),
        (
            "log bind",
            |w| w.drop_bind = true,
            TrialExceptionKind::EnvironmentStartFailed,
            "limit_mismatch",
        ),
        (
            "two containers",
            |w| w.two_containers = true,
            TrialExceptionKind::EnvironmentStartFailed,
            "label_mismatch",
        ),
        (
            "not ready",
            |w| w.no_container = true,
            TrialExceptionKind::EnvironmentStartFailed,
            "not_ready",
        ),
    ];
    for (what, mutate, kind, slug) in cases {
        let fx = fixture(&[("alpha", 'a')], 1);
        let mut world = World::new(&fx.plan);
        mutate(&mut world);
        let summary = run(&world, &fx.plan).unwrap();
        let outcome = outcome_of(&fx, "alpha-s0-r0");
        assert_eq!(kinds(&outcome), [kind], "{what}");
        assert!(
            outcome.exceptions[0].message.contains(slug),
            "{what}: {:?}",
            outcome.exceptions
        );
        assert!(!summary.trials[0].reward_valid, "{what}");
        let joined = world.joined();
        assert!(
            !joined
                .iter()
                .any(|c| c.contains("solve.sh") || c.contains("test.sh")),
            "{what}: no exec before readiness"
        );
        // Teardown still happened, scoped to the project, and the cleanup succeeded.
        assert!(
            joined
                .iter()
                .any(|c| c.contains(" down --volumes --remove-orphans")),
            "{what}"
        );
        assert!(
            outcome.cleanup.attempted && outcome.cleanup.cleaned,
            "{what}"
        );
    }
}

#[test]
fn a_lost_docker_tunnel_during_the_run_is_an_infrastructure_interruption() {
    let fx = fixture(&[("alpha", 'a')], 1);
    let ready = World::new(&fx.plan);
    // Preflight and pull succeed, then the tunnel dies at `compose up`.
    struct Flaky<'a>(&'a World);
    impl CommandExecutor for Flaky<'_> {
        fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, ExecError> {
            if spec.args.iter().any(|a| a == "up") {
                self.0.log.lock().unwrap().push(spec.clone());
                return Ok(failed(1, "error during connect: EOF"));
            }
            self.0.run(spec)
        }
    }
    let summary = runner::run(&fx.plan, &Flaky(&ready)).unwrap();
    let outcome = outcome_of(&fx, "alpha-s0-r0");
    assert_eq!(kinds(&outcome), [TrialExceptionKind::InfraInterrupted]);
    assert!(outcome.exceptions[0].kind.retryable());
    // R5: one fresh replacement in the same slot, with its own evidence; the tunnel is still
    // down, so it is interrupted too and the slot ends invalid. Both rows are kept.
    let replacement = outcome_of(&fx, "alpha-s0-r1");
    assert_eq!(kinds(&replacement), [TrialExceptionKind::InfraInterrupted]);
    assert_eq!(replacement.identity.retry, 1);
    assert_eq!(
        (summary.total, summary.passed, summary.replacements),
        (1, 0, 1)
    );
    let rows: Vec<(u32, bool)> = summary
        .trials
        .iter()
        .map(|t| (t.retry, t.final_attempt))
        .collect();
    assert_eq!(rows, [(0, false), (1, true)]);
    assert!(
        summary.trials.iter().all(|t| t.cleaned),
        "both containers removed"
    );
}

#[test]
fn a_second_run_over_the_same_output_resumes_and_never_reruns_a_valid_slot() {
    let fx = fixture(&[("alpha", 'a'), ("beta", 'b')], 2);
    let first = run(&World::new(&fx.plan), &fx.plan).unwrap();
    assert_eq!((first.total, first.passed), (2, 2));
    // The same plan again, in a world whose verifier would now fail: nothing is replayed.
    let mut failing = World::new(&fx.plan);
    failing.reward = Some(r#"{"reward": 0.0}"#);
    let again = run(&failing, &fx.plan).unwrap();
    assert_eq!(
        (again.total, again.passed),
        (2, 2),
        "the durable passes are reused"
    );
    assert!(
        !failing.joined().iter().any(|c| c.contains(" up ")),
        "no container was started on resume"
    );
    let journal: Value =
        serde_json::from_slice(&std::fs::read(fx.root.join("out/run-1/journal.json")).unwrap())
            .unwrap();
    assert_eq!(journal["records"].as_array().unwrap().len(), 2);
}

#[test]
fn a_journal_of_another_plan_is_refused_before_anything_runs() {
    let fx = fixture(&[("alpha", 'a')], 1);
    run(&World::new(&fx.plan), &fx.plan).unwrap();
    let mut other = fx.plan.clone();
    other.timeouts.cleanup_sec += 1;
    let world = World::new(&other);
    let error = match runner::run(&other, &world) {
        Err(error) => error,
        Ok(_) => panic!("a changed plan must not resume the journal"),
    };
    assert_eq!(error.kind, OpKind::Journal, "{error:?}");
    assert!(!world.joined().iter().any(|c| c.contains(" up ")));
}

#[test]
fn failed_collection_keeps_the_laptop_evidence_and_never_removes_it() {
    let fx = fixture(&[("alpha", 'a')], 1);
    let mut world = World::new(&fx.plan);
    world.collect_fails = true;
    let summary = run(&world, &fx.plan).unwrap();
    let outcome = outcome_of(&fx, "alpha-s0-r0");
    assert!(
        outcome.reward_valid,
        "a collection failure never changes the grade"
    );
    assert!(!outcome.collection.collected && outcome.collection.error.is_some());
    assert!(
        outcome.cleanup.cleaned,
        "docker resources are still removed"
    );
    let names = world.names_for(&fx.plan.trials[0]);
    assert_eq!(
        summary.trials[0].kept_evidence.as_deref(),
        Some(names.trial_dir.as_str())
    );
    assert!(!world.joined().iter().any(|c| c.contains("rm -rf")));
}

#[test]
fn trials_run_at_most_the_effective_concurrency_and_every_one_is_torn_down_by_its_own_project() {
    let mut fx = fixture(&[("alpha", 'a'), ("beta", 'b')], 4);
    fx.plan.remote.as_mut().unwrap().max_concurrency = 1;
    let world = World::new(&fx.plan);
    let summary = run(&world, &fx.plan).unwrap();
    assert_eq!(summary.concurrency, 1);
    let joined = world.joined();
    let downs: Vec<&String> = joined.iter().filter(|c| c.contains(" down ")).collect();
    assert_eq!(downs.len(), 2);
    for trial in &fx.plan.trials {
        let project = world.names_for(trial).project;
        assert_eq!(
            downs
                .iter()
                .filter(|c| c.contains(&format!("-p {project} ")))
                .count(),
            1
        );
    }
    // Serial: the first trial is gone before the second one is created.
    let first_down = index_of(&joined, " down ");
    let second_up = joined
        .iter()
        .enumerate()
        .filter(|(_, c)| c.contains(" up --detach"))
        .nth(1)
        .unwrap()
        .0;
    assert!(first_down < second_up);
}

// ---- the memory budget ---------------------------------------------------------------------

#[test]
fn a_trial_that_does_not_fit_waits_for_memory_without_lowering_its_limit() {
    use std::sync::{Arc, mpsc};
    let budget = Arc::new(runner::MemoryBudget::new(10));
    budget.acquire(6);
    let (tx, rx) = mpsc::channel();
    let waiter = {
        let budget = Arc::clone(&budget);
        std::thread::spawn(move || {
            budget.acquire(6);
            tx.send(()).unwrap();
        })
    };
    assert!(
        rx.recv_timeout(Duration::from_millis(150)).is_err(),
        "6 + 6 does not fit in 10"
    );
    budget.release(6);
    rx.recv_timeout(Duration::from_secs(5)).unwrap();
    waiter.join().unwrap();
}

// ---- the CLI -------------------------------------------------------------------------------

fn cli_run(
    f: fn(
        &RunnerPlanV1,
        &dyn CommandExecutor,
        &mut dyn std::io::Write,
        &mut dyn std::io::Write,
    ) -> u8,
    world: &World,
    plan: &RunnerPlanV1,
) -> (u8, String, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let status = f(plan, world, &mut out, &mut err);
    (
        status,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

#[test]
fn the_cli_prints_json_and_maps_failures_to_the_transport_exit_family() {
    let fx = fixture(&[("alpha", 'a'), ("beta", 'b')], 2);
    let (status, out, _) = cli_run(cli::preflight_with, &World::new(&fx.plan), &fx.plan);
    assert_eq!(status, 0);
    let report: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(report["trials"], 2);
    assert_eq!(report["docker_host"], DOCKER_HOST);

    let (status, out, _) = cli_run(cli::run_with, &World::new(&fx.plan), &fx.plan);
    assert_eq!(status, 0, "{out}");
    assert_eq!(serde_json::from_str::<Value>(&out).unwrap()["passed"], 2);

    let mut low = World::new(&fx.plan);
    low.disk_kb = Some(1);
    let (status, out, err) = cli_run(cli::preflight_with, &low, &fx.plan);
    assert_eq!((status, out.as_str()), (20, ""));
    assert!(
        err.contains("tb2_runner error [disk_insufficient]"),
        "{err}"
    );

    let mut drift = fx.plan.clone();
    drift.trials[0].image_digest = digest('9');
    let (status, _, err) = cli_run(cli::preflight_with, &World::new(&drift), &drift);
    assert_eq!(status, 21, "{err}");

    let mut down = World::new(&fx.plan);
    down.ssh_down = true;
    assert_eq!(cli_run(cli::preflight_with, &down, &fx.plan).0, 23);

    // A second run over the same output root would resume the first (R5) and reuse its
    // passes, so the failing world gets a fresh output root.
    let fx_failing = fixture(&[("alpha", 'a'), ("beta", 'b')], 2);
    let mut failing = World::new(&fx_failing.plan);
    failing.reward = Some(r#"{"reward": 0.0}"#);
    let (status, _, err) = cli_run(cli::run_with, &failing, &fx_failing.plan);
    assert_eq!(status, 2);
    assert!(err.contains("0 of 2 trials passed"), "{err}");
}

#[test]
fn a_missing_or_malformed_plan_file_is_an_invalid_plan() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nope.json");
    let (mut out, mut err) = (Vec::new(), Vec::new());
    assert_eq!(cli::preflight_command(&missing, &mut out, &mut err), 20);
    assert!(String::from_utf8(err).unwrap().contains("[invalid_plan]"));
    let bad = dir.path().join("bad.json");
    std::fs::write(&bad, "{not json").unwrap();
    assert_eq!(cli::run_command(&bad, &mut Vec::new(), &mut Vec::new()), 20);
    // A plan round-trips through the file the CLI reads.
    let fx = fixture(&[("alpha", 'a')], 1);
    let path: &Path = &dir.path().join("plan.json");
    std::fs::write(path, serde_json::to_vec(&fx.plan).unwrap()).unwrap();
    assert_eq!(cli::load_plan(path).unwrap(), fx.plan);
}
