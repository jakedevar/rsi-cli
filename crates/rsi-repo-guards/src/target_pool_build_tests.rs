//! Pool build, the detached build request and `seed-from` (#453), on real Git and
//! a stub toolchain: a stub `cargo` fabricates the target tree, so no test here
//! runs a real build. The cases are those of the legacy `TargetPool` class (build, degraded, failed
//! diagnostics, disk-floor abort, `seed-from`); the real-Cargo ones run in
//! `crates/rsid/tests/target_pool_cargo.rs`, and every legacy case is mapped in
//! `target_pool_case_map.rs`.
use super::*;
use crate::target_pool::build_key;
use crate::target_pool_ops::{markers, read_events, test_disk::Disk};
use crate::testutil;
use std::path::Path;

struct Fx {
    stub: tempfile::TempDir,
    _pools: tempfile::TempDir,
    repo: tempfile::TempDir,
    env: Env,
    commits: Vec<String>,
}

fn script(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

const STUB_CARGO: &str = r#"#!/bin/sh
echo "$*" >> "${STUB_CARGO_LOG:?}"
mkdir -p "$CARGO_TARGET_DIR/debug/deps"
case "$STUB_CARGO_MODE:$*" in
  tests_fail:"test --workspace --no-run") echo "error: tests do not compile" >&2; exit 101;;
  fail:*) echo "error: build broke" >&2; exit 101;;
  all_fail:*) echo "error: nothing builds" >&2; exit 101;;
  sleep:*) sleep 30;;
esac
printf lib > "$CARGO_TARGET_DIR/debug/deps/libapp-aaa.rlib"
printf dep > "$CARGO_TARGET_DIR/debug/deps/app-aaa.d"
case "$*" in
  "build --workspace"|*--keep-going*) ;;
  *) printf exe > "$CARGO_TARGET_DIR/debug/deps/app-aaa";;
esac
exit 0
"#;

fn fx(extra: &[(&str, &str)]) -> Fx {
    let stub = tempfile::tempdir().unwrap();
    script(
        &stub.path().join("rustc"),
        "#!/bin/sh\nprintf 'rustc 1.0.0\\n'\n",
    );
    script(&stub.path().join("cargo"), STUB_CARGO);
    let pools = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    testutil::git(repo.path(), &["init", "-q", "-b", "main"]);
    let mut commits = vec![];
    for (name, text) in [
        ("Cargo.toml", "[workspace]\n"),
        ("a.txt", "a\n"),
        ("b.txt", "b\n"),
    ] {
        testutil::write(repo.path(), name, text);
        testutil::git(repo.path(), &["add", name]);
        testutil::git(
            repo.path(),
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                name,
            ],
        );
        commits.push(
            testutil::git(repo.path(), &["rev-parse", "HEAD"])
                .trim()
                .to_string(),
        );
    }
    let mut env = Env::new();
    env.insert(
        "PATH".into(),
        format!(
            "{}:{}",
            stub.path().display(),
            std::env::var("PATH").unwrap_or_default()
        ),
    );
    env.insert("HOME".into(), std::env::var("HOME").unwrap_or_default());
    env.insert(
        "RSI_TARGET_POOL_ROOT".into(),
        pools.path().to_string_lossy().into_owned(),
    );
    env.insert(
        "RSI_CARGO_SLOT".into(),
        pools
            .path()
            .join("no-cargo-slot")
            .to_string_lossy()
            .into_owned(),
    );
    env.insert("RSI_TARGET_POOL_MIN_FREE_GIB".into(), "0".into());
    env.insert(
        "STUB_CARGO_LOG".into(),
        stub.path().join("cargo.log").to_string_lossy().into_owned(),
    );
    env.insert("STUB_CARGO_MODE".into(), "ok".into());
    for (name, value) in extra {
        env.insert((*name).into(), (*value).into());
    }
    Fx {
        stub,
        _pools: pools,
        repo,
        env,
        commits,
    }
}

impl Fx {
    fn git(&self) -> Git<'_> {
        Git { env: &self.env }
    }
    fn directory(&self) -> PathBuf {
        repo_dir(&self.git(), &self.env, self.repo.path()).unwrap()
    }
    fn key(&self) -> String {
        build_key(self.repo.path(), &self.env).unwrap()
    }
    fn options(&self, index: usize) -> BuildOptions {
        BuildOptions {
            repo: self.repo.path().to_path_buf(),
            sha: Some(self.commits[index].clone()),
            keep: 4,
            min_free_gib: 0.0,
            retry_after: 3600.0,
            max_distance: 200,
        }
    }
    fn build(&self, options: &BuildOptions) -> (i32, String) {
        let mut err = vec![];
        let code = build(options, &self.env, &mut err).unwrap();
        (code, String::from_utf8(err).unwrap())
    }
    fn marker(&self, index: usize) -> Value {
        let path = self
            .directory()
            .join(format!("{}-{}.json", self.commits[index], self.key()));
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }
    fn cargo_calls(&self) -> Vec<String> {
        fs::read_to_string(self.stub.path().join("cargo.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }
}

// ---- build ----

#[test]
fn a_build_publishes_a_ready_pool_with_the_python_tools_marker_fields() {
    let fx = fx(&[]);
    let (code, err) = fx.build(&fx.options(1));
    assert_eq!(code, 0, "{err}");
    assert!(
        err.contains(&format!(
            "target-pool: building pool {}-{}: cargo test --workspace --no-run",
            fx.commits[1],
            fx.key()
        )),
        "{err}"
    );
    let record = fx.marker(1);
    let keys: Vec<&String> = record.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        [
            "name",
            "sha",
            "key",
            "target",
            "worktree",
            "built_at",
            "build_seconds",
            "command",
            "log_path",
            "seed",
            "size_bytes",
            "state"
        ]
    );
    assert_eq!(record["state"], "ready");
    assert_eq!(
        record["command"],
        json!(["cargo", "test", "--workspace", "--no-run"])
    );
    assert_eq!(record["seed"], Value::Null);
    assert_eq!(record["size_bytes"], 3 + 3 + 3, "lib + dep + exe");
    let target = PathBuf::from(record["target"].as_str().unwrap());
    assert!(target.join("debug/deps/libapp-aaa.rlib").is_file());
    assert!(Path::new(record["log_path"].as_str().unwrap()).is_file());
    // The in-place executable is read-only; the rlib and dep-info stay writable.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |name: &str| {
            fs::metadata(target.join("debug/deps").join(name))
                .unwrap()
                .permissions()
                .mode()
        };
        assert_eq!(mode("app-aaa") & 0o222, 0);
        assert_ne!(mode("libapp-aaa.rlib") & 0o200, 0);
        assert_ne!(mode("app-aaa.d") & 0o200, 0);
    }
    let events = read_events(&fx.directory());
    assert_eq!(events["last_pool"]["size_bytes"], 9);
    assert!(events.get("skip").is_none());
    assert_eq!(fx.cargo_calls(), ["test --workspace --no-run"]);
}

#[test]
fn a_ready_pool_or_a_recent_failure_is_left_alone_and_an_old_failure_is_retried() {
    let mut fx = fx(&[]);
    assert_eq!(fx.build(&fx.options(1)).0, 0);
    let (code, err) = fx.build(&fx.options(1));
    assert_eq!(code, 0);
    assert!(err.contains("is ready; nothing to do"), "{err}");
    assert_eq!(fx.cargo_calls().len(), 1);
    // A failed build inside the retry window is not retried; outside it is.
    fx.env.insert("STUB_CARGO_MODE".into(), "all_fail".into());
    let (code, _) = fx.build(&fx.options(2));
    assert_eq!(code, 101);
    fx.env.insert("STUB_CARGO_MODE".into(), "ok".into());
    let failed = fx.marker(2);
    assert_eq!(failed["state"], "failed");
    let (code, err) = fx.build(&fx.options(2));
    assert_eq!(code, 0);
    assert!(err.contains("is failed; nothing to do"), "{err}");
    let mut retry = fx.options(2);
    retry.retry_after = 0.0;
    let (code, _) = fx.build(&retry);
    assert_eq!(
        code, 0,
        "an old failure is rebuilt, and the stub builds now"
    );
    assert_eq!(fx.marker(2)["state"], "ready");
}

#[test]
fn a_descendant_build_is_seeded_from_the_nearest_ready_ancestor_by_hard_links() {
    let fx = fx(&[]);
    assert_eq!(fx.build(&fx.options(0)).0, 0);
    let (code, err) = fx.build(&fx.options(2));
    assert_eq!(code, 0, "{err}");
    assert!(err.contains("seeding pool"), "{err}");
    let record = fx.marker(2);
    let seed = &record["seed"];
    assert_eq!(seed["pool_sha"], fx.commits[0]);
    assert_eq!(seed["distance"], 2);
    assert!(seed["linked"].as_u64().unwrap() >= 1);
    assert!(seed["seconds"].is_number());
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let ancestor = fx.marker(0);
        let rlib = |r: &Value| {
            fs::metadata(
                Path::new(r["target"].as_str().unwrap()).join("debug/deps/libapp-aaa.rlib"),
            )
            .unwrap()
        };
        // The stub rewrites the rlib in the new pool (as the build would); the old pool's bytes survive.
        assert_eq!(
            fs::read(
                Path::new(ancestor["target"].as_str().unwrap()).join("debug/deps/libapp-aaa.rlib")
            )
            .unwrap(),
            b"lib"
        );
        let _ = (rlib(&record).ino(), rlib(&ancestor).ino());
    }
}

#[test]
fn a_tip_whose_tests_do_not_compile_still_gets_a_degraded_pool() {
    let fx = fx(&[("STUB_CARGO_MODE", "tests_fail")]);
    let (code, err) = fx.build(&fx.options(1));
    assert_eq!(code, 0, "{err}");
    assert!(
        err.contains(
            "tests do not compile (exit 101); keeping the libraries and binaries that built"
        ),
        "{err}"
    );
    let record = fx.marker(1);
    assert_eq!(record["state"], "ready");
    assert_eq!(record["degraded"], true);
    assert_eq!(record["test_exit_code"], 101);
    assert_eq!(record["scope"], "libraries");
    assert!(
        record["test_error_tail"]
            .as_str()
            .unwrap()
            .contains("tests do not compile")
    );
    assert_eq!(
        fx.cargo_calls(),
        [
            "test --workspace --no-run",
            "test --workspace --no-run --keep-going",
            "build --workspace"
        ]
    );
    let log = fs::read_to_string(record["log_path"].as_str().unwrap()).unwrap();
    assert!(log.contains("target-pool: tests do not compile; building what does"));
}

#[test]
fn a_real_build_break_fails_the_pool_with_its_diagnostics_in_the_marker() {
    let fx = fx(&[("STUB_CARGO_MODE", "all_fail")]);
    let (code, err) = fx.build(&fx.options(1));
    assert_eq!(code, 101);
    assert!(err.contains("pool build failed (exit 101); log:"), "{err}");
    let record = fx.marker(1);
    assert_eq!(record["state"], "failed");
    assert_eq!(record["exit_code"], 101);
    assert!(
        record["error_tail"]
            .as_str()
            .unwrap()
            .contains("nothing builds")
    );
    let directory = fx.directory();
    let name = format!("{}-{}", fx.commits[1], fx.key());
    assert!(!directory.join(&name).exists(), "the worktree is removed");
    assert!(
        directory.join(format!("{name}.log")).is_file(),
        "the log is kept"
    );
    // The next seed surfaces the durable failure.
    assert_eq!(
        crate::target_pool::failed_pool(&directory, &fx.commits[1], &fx.key()).unwrap()["exit_code"],
        101
    );
}

#[test]
fn a_build_below_the_free_space_floor_is_skipped_with_the_typed_reason() {
    let fx = fx(&[("RSI_TARGET_POOL_WORKER_FLOOR_GIB", "5")]);
    let _disk = Disk::new(Some(1.0));
    let mut options = fx.options(1);
    options.min_free_gib = 40.0;
    let (code, err) = fx.build(&options);
    assert_eq!(code, 0);
    assert!(
        err.contains("pool_skipped_disk_headroom: only 1.0 GiB free; needs"),
        "{err}"
    );
    assert!(fx.cargo_calls().is_empty(), "nothing was built");
    let events = read_events(&fx.directory());
    assert_eq!(events["skip"]["reason"], "pool_skipped_disk_headroom");
    assert!(markers(&fx.directory()).is_empty());
}

#[cfg(unix)]
#[test]
fn a_guarded_build_is_stopped_the_moment_free_space_crosses_the_floor_and_leaves_no_partial_pool() {
    let fx = fx(&[
        ("STUB_CARGO_MODE", "sleep"),
        ("RSI_TARGET_POOL_WORKER_FLOOR_GIB", "5"),
        ("RSI_TARGET_POOL_WATCH_SEC", "0.1"),
    ]);
    let _disk = Disk::new(Some(500.0));
    let flipper = std::thread::spawn(|| {
        std::thread::sleep(Duration::from_millis(700));
        Disk::set(Some(1.0));
    });
    let mut options = fx.options(1);
    options.min_free_gib = 20.0;
    let started = Instant::now();
    let (code, err) = fx.build(&options);
    flipper.join().unwrap();
    assert_eq!(code, 1, "{err}");
    assert!(
        started.elapsed() < Duration::from_secs(25),
        "the 30 s stub was stopped early"
    );
    assert!(
        err.contains("pool_aborted_disk_floor: stopped at 1.0 GiB free; the workers' floor is"),
        "{err}"
    );
    assert!(err.contains("partial pool removed"));
    let record = fx.marker(1);
    assert_eq!(record["state"], "failed");
    assert_eq!(record["reason"], "pool_aborted_disk_floor");
    assert!(
        !fx.directory()
            .join(format!("{}-{}", fx.commits[1], fx.key()))
            .exists()
    );
}

#[test]
fn the_pool_build_environment_drops_launch_scratch_and_keeps_cache_settings() {
    let mut env = Env::new();
    for (name, value) in [
        ("PATH", "/bin"),
        ("TMPDIR", "/scratch"),
        ("RSI_SESSION_TOKEN", "secret"),
        ("RSI_CARGO_SLOT_HELD", "1"),
        ("RSI_PROCESS_OWNERSHIP_NAMESPACE", "ns"),
        ("SCCACHE_DIR", "/cache"),
        ("RUSTC_WRAPPER", "sccache"),
        ("GIT_DIR", "/x/.git"),
    ] {
        env.insert(name.into(), value.into());
    }
    let temporary = Path::new("/tmp/t");
    let built = pool_build_environment(&env, Path::new("/pool/target"), temporary).unwrap();
    for gone in [
        "RSI_SESSION_TOKEN",
        "RSI_CARGO_SLOT_HELD",
        "RSI_PROCESS_OWNERSHIP_NAMESPACE",
        "GIT_DIR",
    ] {
        assert!(!built.contains_key(gone), "{gone}");
    }
    assert_eq!(built["SCCACHE_DIR"], "/cache");
    assert_eq!(built["RUSTC_WRAPPER"], "sccache");
    assert_eq!(built["CARGO_TARGET_DIR"], "/pool/target");
    for name in ["TMPDIR", "TMP", "TEMP"] {
        assert_eq!(built[name], "/tmp/t");
    }
    assert_eq!(built["RSI_CARGO_NO_POOL_RECLAIM"], "1");
    let long = PathBuf::from(format!("/tmp/{}", "x".repeat(90)));
    assert!(
        pool_build_environment(&env, Path::new("/pool/target"), &long).is_err(),
        "sccache socket path too long"
    );
}

// ---- the detached request ----

#[test]
fn pool_unit_limits_follow_the_slice_budget_and_settings() {
    let fx = fx(&[]);
    let mut env = fx.env.clone();
    script(
        &fx.stub.path().join("systemctl"),
        "#!/bin/sh\necho \"${STUB_QUOTA:-infinity}\"\n",
    );
    assert_eq!(
        pool_unit_limits(&env, "rsi-build.slice"),
        (400.0, 7200.0),
        "no quota on the slice"
    );
    env.insert("STUB_QUOTA".into(), "2s".into());
    assert_eq!(
        pool_unit_limits(&env, "rsi-build.slice").0,
        200.0,
        "never above the slice's own budget"
    );
    env.insert("STUB_QUOTA".into(), "30ms".into());
    assert_eq!(
        pool_unit_limits(&env, "rsi-build.slice").0,
        100.0,
        "a floor of one CPU"
    );
    env.insert("RSI_TARGET_POOL_CPU_PERCENT".into(), "250".into());
    env.insert("RSI_TARGET_POOL_MAX_SEC".into(), "60".into());
    env.insert("STUB_QUOTA".into(), "1min".into());
    assert_eq!(pool_unit_limits(&env, "rsi-build.slice"), (250.0, 60.0));
    assert_eq!(
        pool_unit_limits(&env, ""),
        (250.0, 60.0),
        "an empty slice name opts out"
    );
    assert_eq!(build_slice(&env), "rsi-build.slice");
    env.insert("RSI_BUILD_SLICE".into(), String::new());
    assert_eq!(build_slice(&env), "");
}

#[test]
fn the_quota_text_parses_like_the_python_regex() {
    assert_eq!(parse_quota_seconds("4s"), Some(4.0));
    assert_eq!(parse_quota_seconds("2.5"), Some(2.5));
    assert_eq!(parse_quota_seconds("500ms"), Some(0.5));
    assert_eq!(parse_quota_seconds("2min"), Some(120.0));
    assert!((parse_quota_seconds("100us").unwrap() - 0.0001).abs() < 1e-12);
    for bad in ["", "infinity", "4h", "s", ".5", "1.", "1.2.3"] {
        assert_eq!(parse_quota_seconds(bad), None, "{bad:?}");
    }
    assert_eq!(g(400.0), "400");
    assert_eq!(g(7200.0), "7200");
    assert_eq!(g(150.5), "150.5");
}

#[cfg(target_os = "linux")]
#[test]
fn a_build_request_runs_the_detached_unit_in_the_slice_with_only_the_allowed_environment() {
    let fx = fx(&[
        ("RSI_TARGET_POOL_CPU_PERCENT", "300"),
        ("RSI_SESSION_TOKEN", "secret"),
        ("TMPDIR", "/scratch"),
    ]);
    script(
        &fx.stub.path().join("systemd-run"),
        "#!/bin/sh\nfor a in \"$@\"; do echo \"$a\"; done > \"$STUB_CARGO_LOG.systemd\"\n",
    );
    script(
        &fx.stub.path().join("systemctl"),
        "#!/bin/sh\necho infinity\n",
    );
    let program = vec!["/opt/rsi-repo-guard".to_string(), "target-pool".to_string()];
    let _disk = Disk::new(Some(1000.0));
    let result = request_build(&fx.git(), fx.repo.path(), &fx.commits[2], &program).unwrap();
    assert!(result.starts_with("started rsi-target-pool-"), "{result}");
    let args = fs::read_to_string(format!("{}.systemd", fx.env["STUB_CARGO_LOG"])).unwrap();
    let args: Vec<&str> = args.lines().collect();
    assert!(args.contains(&"--slice=rsi-build.slice"));
    assert!(args.contains(&"--property=CPUQuota=300%"));
    assert!(args.contains(&"--property=RuntimeMaxSec=7200"));
    assert!(args.contains(&"--property=Nice=19"));
    assert!(
        !args
            .iter()
            .any(|a| a.contains("RSI_SESSION_TOKEN") || a.contains("TMPDIR")),
        "{args:?}"
    );
    assert!(args.iter().any(|a| a.starts_with("--setenv=PATH=")));
    let tail: Vec<&&str> = args.iter().skip_while(|a| **a != "--").skip(1).collect();
    assert_eq!(
        tail,
        [
            &"/opt/rsi-repo-guard",
            &"target-pool",
            &"build",
            &"--repo",
            &fx.repo.path().to_string_lossy().as_ref(),
            &"--sha",
            &fx.commits[2].as_str()
        ]
    );
}

#[cfg(all(unix, not(target_os = "linux")))]
#[test]
fn a_build_request_without_systemd_starts_a_detached_niced_process() {
    let fx = fx(&[]);
    let record = fx.stub.path().join("started");
    let program = fx.stub.path().join("program");
    script(
        &program,
        &format!("#!/bin/sh\necho \"$@\" > {}\n", record.display()),
    );
    let _disk = Disk::new(Some(1000.0));
    let result = request_build(
        &fx.git(),
        fx.repo.path(),
        &fx.commits[2],
        &[program.to_string_lossy().into_owned()],
    )
    .unwrap();
    assert_eq!(result, "started");
    for _ in 0..100 {
        // The shell creates the file before `echo` writes it: wait for the line itself.
        if fs::read_to_string(&record).is_ok_and(|text| text.ends_with('\n')) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let text = fs::read_to_string(&record).unwrap();
    assert!(text.starts_with("build --repo "), "{text}");
    assert!(text.contains(&format!("--sha {}", fx.commits[2])));
}

#[test]
fn a_build_request_below_the_free_space_floor_says_so_and_starts_nothing() {
    let fx = fx(&[
        ("RSI_TARGET_POOL_MIN_FREE_GIB", "40"),
        ("RSI_TARGET_POOL_WORKER_FLOOR_GIB", "5"),
    ]);
    let marker = fx.stub.path().join("started");
    let program = fx.stub.path().join("program");
    script(
        &program,
        &format!("#!/bin/sh\ntouch {}\n", marker.display()),
    );
    let _disk = Disk::new(Some(2.0));
    let result = request_build(
        &fx.git(),
        fx.repo.path(),
        &fx.commits[2],
        &[program.to_string_lossy().into_owned()],
    )
    .unwrap();
    assert!(
        result.starts_with("skipped: pool_skipped_disk_headroom: only 2.0 GiB free; needs"),
        "{result}"
    );
    std::thread::sleep(Duration::from_millis(200));
    assert!(!marker.exists());
    let events = read_events(&fx.directory());
    assert_eq!(events["skip"]["reason"], "pool_skipped_disk_headroom");
}

// ---- seed-from ----

#[test]
fn shlex_matches_the_python_module() {
    assert_eq!(
        shlex_split("ssh -o BatchMode=yes"),
        ["ssh", "-o", "BatchMode=yes"]
    );
    assert_eq!(
        shlex_split("a 'b c' \"d e\" f\\ g"),
        ["a", "b c", "d e", "f g"]
    );
    assert_eq!(shlex_split("  \"\"  x"), ["", "x"]);
    assert_eq!(shlex_quote("abc/def-1.2"), "abc/def-1.2");
    assert_eq!(shlex_quote("a b"), "'a b'");
    assert_eq!(shlex_quote("it's"), "'it'\"'\"'s'");
    assert_eq!(shlex_quote(""), "''");
}

struct Remote {
    shim: PathBuf,
    root: PathBuf,
    tool: PathBuf,
}

/// A "peer": its pool markers live under `root`; the ssh shim runs the remote command
/// locally with that pool root, `du` is a stub that, like BSD `du`, refuses every flag but `-sk`.
fn remote(fx: &Fx, name: &str, sha: &str, key: &str) -> Remote {
    let root = fx.stub.path().join("remote-pools");
    let directory = root.join("peer");
    let target = directory.join(name).join("target");
    fs::create_dir_all(target.join("debug/deps")).unwrap();
    fs::write(target.join("debug/deps/libapp-aaa.rlib"), "remote-lib").unwrap();
    fs::write(target.join("debug/deps/app-aaa"), "remote-exe").unwrap();
    let record = json!({
        "name": name, "sha": sha, "key": key, "state": "ready", "target": target,
        "worktree": directory.join(name), "built_at": 5.0, "command": BUILD_COMMAND,
    });
    fs::write(
        directory.join(format!("{name}.json")),
        format!("{}\n", dumps(&record, None)),
    )
    .unwrap();
    let shim = fx.stub.path().join("fake-ssh");
    script(&shim, "#!/bin/sh\nshift\nexec sh -c \"$1\"\n");
    script(
        &fx.stub.path().join("du"),
        "#!/bin/sh\ncase \"$1\" in -sk) ;; *) echo \"du: invalid option -- $1\" >&2; exit 64;; esac\nprintf '4\\t%s\\n' \"$2\"\n",
    );
    let tool = fx.stub.path().join("remote-tool");
    script(
        &tool,
        &format!(
            "#!/bin/sh\ncase \"$1\" in status) cat {}/*/*.json;; esac\n",
            root.display()
        ),
    );
    Remote { shim, root, tool }
}

fn have_zstd() -> bool {
    Command::new("zstd")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn seed_options(fx: &Fx, remote: &Remote) -> SeedFromOptions {
    SeedFromOptions {
        host: "peer".into(),
        repo: fx.repo.path().to_path_buf(),
        sha: None,
        dry_run: false,
        keep: Some(5),
        level: 3,
        remote_tool: remote.tool.to_string_lossy().into_owned(),
        min_free_gib: 0.0,
        min_headroom_gib: 0.0,
    }
}

#[test]
fn seed_from_another_host_copies_the_newest_matching_pool() {
    if !have_zstd() {
        eprintln!("skipped: zstd is not installed");
        return;
    }
    let mut fx = fx(&[]);
    let key = fx.key();
    let name = format!("{}-{}", fx.commits[0], key);
    let remote = remote(&fx, &name, &fx.commits[0], &key);
    fx.env.insert(
        "RSI_TARGET_POOL_SSH".into(),
        remote.shim.to_string_lossy().into_owned(),
    );
    let local = tempfile::tempdir().unwrap();
    fx.env.insert(
        "RSI_TARGET_POOL_ROOT".into(),
        local.path().to_string_lossy().into_owned(),
    );
    let _disk = Disk::new(Some(1000.0));
    let run = |env: &Env, options: &SeedFromOptions| -> (i32, String, String) {
        let (mut out, mut err) = (vec![], vec![]);
        let code = seed_from(options, env, &mut out, &mut err).unwrap();
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    };
    let mut dry = seed_options(&fx, &remote);
    dry.dry_run = true;
    let (code, out, err) = run(&fx.env.clone(), &dry);
    assert_eq!(code, 0, "{err}");
    let printed: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(printed["result"], "dry run");
    assert_eq!(printed["host"], "peer");
    assert_eq!(printed["pool"], name);
    assert!(
        markers(&fx.directory()).is_empty(),
        "a dry run copies nothing"
    );
    let (code, out, err) = run(&fx.env.clone(), &seed_options(&fx, &remote));
    assert_eq!(code, 0, "{err}");
    let printed: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(printed["result"], "seeded");
    assert_eq!(printed["sha"], fx.commits[0]);
    let record = fx.marker_named(&name);
    assert_eq!(record["state"], "ready");
    assert_eq!(record["seeded_from"]["host"], "peer");
    assert!(record["size_bytes"].as_u64().unwrap() > 0);
    let target = PathBuf::from(record["target"].as_str().unwrap());
    assert_eq!(
        fs::read(target.join("debug/deps/libapp-aaa.rlib")).unwrap(),
        b"remote-lib"
    );
    let directory = fx.directory();
    let strays: Vec<_> = fs::read_dir(&directory)
        .unwrap()
        .flatten()
        .filter(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.ends_with(".tmp") || n.starts_with(".seed-")
        })
        .collect();
    assert!(strays.is_empty());
    assert!(
        !directory.join(&name).join(".git").exists(),
        "the detached worktree is not copied"
    );
    let (_, out, _) = run(&fx.env.clone(), &seed_options(&fx, &remote));
    assert_eq!(
        serde_json::from_str::<Value>(&out).unwrap()["result"],
        "already present"
    );
    // A host that built differently offers nothing usable.
    let mut other = fx.env.clone();
    other.insert("CARGO_PROFILE_DEV_DEBUG".into(), "full".into());
    let (code, _, err) = run(&other, &seed_options(&fx, &remote));
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("has no ready pool with key"));
}

impl Fx {
    fn marker_named(&self, name: &str) -> Value {
        serde_json::from_str(
            &fs::read_to_string(self.directory().join(format!("{name}.json"))).unwrap(),
        )
        .unwrap()
    }
}

#[test]
fn seed_from_leaves_the_workers_their_floor_too() {
    if !have_zstd() {
        eprintln!("skipped: zstd is not installed");
        return;
    }
    let mut fx = fx(&[("RSI_TARGET_POOL_WORKER_FLOOR_GIB", "5")]);
    let key = fx.key();
    let name = format!("{}-{}", fx.commits[0], key);
    let remote = remote(&fx, &name, &fx.commits[0], &key);
    fx.env.insert(
        "RSI_TARGET_POOL_SSH".into(),
        remote.shim.to_string_lossy().into_owned(),
    );
    let local = tempfile::tempdir().unwrap();
    fx.env.insert(
        "RSI_TARGET_POOL_ROOT".into(),
        local.path().to_string_lossy().into_owned(),
    );
    let (total_gib, _) = (
        crate::target_pool_ops::scaled_floor_gib(local.path(), 1.0e9).unwrap() * 4.0,
        0,
    );
    if total_gib < 40.0 {
        return; // the 5 GiB worker floor would be capped on a tiny disk
    }
    let _disk = Disk::new(Some(3.0));
    let mut options = seed_options(&fx, &remote);
    options.min_free_gib = 1.0;
    let (mut out, mut err) = (vec![], vec![]);
    let code = seed_from(&options, &fx.env, &mut out, &mut err).unwrap();
    assert_eq!(code, 3);
    assert!(
        String::from_utf8(err)
            .unwrap()
            .contains("pool_skipped_disk_headroom")
    );
    let events = read_events(&repo_dir(&fx.git(), &fx.env, fx.repo.path()).unwrap());
    assert_eq!(events["skip"]["expected_source"], "remote pool");
    assert!(markers(&fx.directory()).is_empty());
    let _ = remote.root;
}

// ---- cases the legacy suite ran that the tests above do not (#453, case map in
// `target_pool_case_map.rs`) ----

#[test]
fn failed_build_diagnostics_are_visible_in_status_and_next_seed() {
    let fx = fx(&[]);
    let failing = fx.stub.path().join("failing-slot");
    script(
        &failing,
        "#!/bin/sh\necho controlled-pool-build-failure\nexit 9\n",
    );
    let mut env = fx.env.clone();
    env.insert(
        "RSI_CARGO_SLOT".into(),
        failing.to_string_lossy().into_owned(),
    );
    let mut err = vec![];
    let code = build(&fx.options(1), &env, &mut err).unwrap();
    assert_eq!(code, 9, "{}", String::from_utf8_lossy(&err));
    // `status` prints the failed marker.
    let mut out = vec![];
    crate::target_pool_ops::status_command(Some(fx.repo.path()), &env, &mut out).unwrap();
    let record: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(record["state"], "failed");
    assert_eq!(record["exit_code"], 9);
    assert!(
        record["error_tail"]
            .as_str()
            .unwrap()
            .contains("controlled-pool-build-failure")
    );
    assert!(Path::new(record["log_path"].as_str().unwrap()).is_file());
    // The next seed of a sandbox at that commit reports the durable failure.
    let sandbox = fx.repo.path();
    testutil::git(sandbox, &["checkout", "-q", &fx.commits[1]]);
    let env_vars: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let (mut out, mut err) = (vec![], vec![]);
    let code = crate::target_pool::main(
        &[
            "seed".into(),
            "--sandbox".into(),
            sandbox.to_string_lossy().into_owned(),
        ],
        &crate::Ctx::new(sandbox, &env_vars),
        &mut out,
        &mut err,
    );
    assert_eq!(code, 0, "{}", String::from_utf8_lossy(&err));
    let outcome: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(outcome["result"], "no pool");
    assert_eq!(outcome["build_failure"], record);
}

#[test]
fn seed_from_records_the_checkout_of_the_pool_it_imports() {
    if !have_zstd() {
        eprintln!("skipped: zstd is not installed");
        return;
    }
    let mut fx = fx(&[]);
    let key = fx.key();
    let name = format!("{}-{}", fx.commits[0], key);
    let remote = remote(&fx, &name, &fx.commits[0], &key);
    fx.env.insert(
        "RSI_TARGET_POOL_SSH".into(),
        remote.shim.to_string_lossy().into_owned(),
    );
    let local = tempfile::tempdir().unwrap();
    fx.env.insert(
        "RSI_TARGET_POOL_ROOT".into(),
        local.path().to_string_lossy().into_owned(),
    );
    let _disk = Disk::new(Some(1000.0));
    let directory = fx.directory();
    assert!(crate::target_pool::known_repo(&directory).is_none());
    let (mut out, mut err) = (vec![], vec![]);
    let code = seed_from(&seed_options(&fx, &remote), &fx.env, &mut out, &mut err).unwrap();
    assert_eq!(code, 0, "{}", String::from_utf8_lossy(&err));
    // The import itself writes the association; no local build is needed.
    assert_eq!(
        crate::target_pool::known_repo(&directory).map(|p| fs::canonicalize(p).unwrap()),
        Some(fs::canonicalize(fx.repo.path()).unwrap())
    );
    assert!(!directory.join(&name).join(".git").exists());
}

#[test]
fn gc_racing_a_seed_from_cannot_remove_the_half_copied_pool() {
    if !have_zstd() {
        eprintln!("skipped: zstd is not installed");
        return;
    }
    let mut fx = fx(&[]);
    let key = fx.key();
    let name = format!("{}-{}", fx.commits[0], key);
    let remote = remote(&fx, &name, &fx.commits[0], &key);
    // The peer's pool is newer than the local `newer-pool` marker, so the seeded pool is
    // the one a later gc keeps.
    let remote_marker = remote.root.join("peer").join(format!("{name}.json"));
    let mut remote_record: Value =
        serde_json::from_str(&fs::read_to_string(&remote_marker).unwrap()).unwrap();
    remote_record["built_at"] = json!(2000.0);
    fs::write(
        &remote_marker,
        serde_json::to_string(&remote_record).unwrap(),
    )
    .unwrap();
    // The ssh shim parks the transfer (`tar -C ...`) until the test has run its gc.
    let gate = fx.stub.path().join("gate");
    fs::create_dir(&gate).unwrap();
    let shim = fx.stub.path().join("gating-ssh");
    script(
        &shim,
        &format!(
            "#!/bin/sh\nshift\ncase \"$1\" in \"tar -C\"*) touch {g}/started; i=0; \
             while [ ! -f {g}/go ] && [ $i -lt 400 ]; do sleep 0.05; i=$((i+1)); done;; esac\n\
             exec sh -c \"$1\"\n",
            g = gate.display()
        ),
    );
    fx.env.insert(
        "RSI_TARGET_POOL_SSH".into(),
        shim.to_string_lossy().into_owned(),
    );
    let local = tempfile::tempdir().unwrap();
    fx.env.insert(
        "RSI_TARGET_POOL_ROOT".into(),
        local.path().to_string_lossy().into_owned(),
    );
    let _disk = Disk::new(Some(1000.0));
    let directory = fx.directory();
    fs::create_dir_all(&directory).unwrap();
    // A stale ready marker of the very pool being seeded, and a newer one of another.
    let stale = json!({"name": name, "sha": fx.commits[0], "key": key, "state": "ready",
        "target": directory.join(&name).join("target"), "built_at": 1});
    let newer = json!({"name": "newer-pool", "sha": fx.commits[0], "key": key, "state": "ready",
        "target": directory.join("newer").join("t"), "built_at": 999});
    crate::target_pool_ops::write_marker(&directory.join(format!("{name}.json")), &stale).unwrap();
    crate::target_pool_ops::write_marker(&directory.join("newer-pool.json"), &newer).unwrap();
    let git = Git { env: &fx.env };
    let options = crate::target_pool_ops::GcOptions {
        keep: 1,
        repo: None,
        pressure: false,
        reference: None,
        max_distance: 200,
        filesystem: None,
    };
    let (during, seeded) = std::thread::scope(|scope| {
        let transfer = scope.spawn(|| {
            let (mut out, mut err) = (vec![], vec![]);
            let code = seed_from(&seed_options(&fx, &remote), &fx.env, &mut out, &mut err).unwrap();
            (
                code,
                String::from_utf8(out).unwrap(),
                String::from_utf8(err).unwrap(),
            )
        });
        for _ in 0..400 {
            if gate.join("started").exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(gate.join("started").exists(), "the transfer never started");
        // The transfer is in flight: gc plans to drop the stale marker's pool.
        let mut skipped = vec![];
        let removed = crate::target_pool_ops::gc(&git, &directory, &options, &mut skipped);
        fs::write(gate.join("go"), "").unwrap();
        ((removed, skipped), transfer.join().unwrap())
    });
    assert_eq!(seeded.0, 0, "{}", seeded.2);
    assert_eq!(
        serde_json::from_str::<Value>(&seeded.1).unwrap()["result"],
        "seeded"
    );
    assert_eq!(
        during.1,
        [name.clone()],
        "the seed's use lock makes gc skip the pool"
    );
    assert!(during.0.is_empty(), "{:?}", during.0);
    let record = fx.marker_named(&name);
    assert_eq!(record["seeded_from"]["host"], "peer");
    let target = PathBuf::from(record["target"].as_str().unwrap());
    assert_eq!(
        fs::read(target.join("debug/deps/libapp-aaa.rlib")).unwrap(),
        b"remote-lib"
    );
    assert!(record["size_bytes"].as_u64().unwrap() > 0);
    let strays: Vec<_> = fs::read_dir(&directory)
        .unwrap()
        .flatten()
        .filter(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.ends_with(".tmp") || n.starts_with(".seed-")
        })
        .collect();
    assert!(strays.is_empty(), "{strays:?}");
    // A later gc counts the seeded pool like any other.
    let mut skipped = vec![];
    let removed = crate::target_pool_ops::gc(&git, &directory, &options, &mut skipped);
    assert_eq!((removed, skipped), (vec!["newer-pool".to_string()], vec![]));
    assert!(target.is_dir());
}
