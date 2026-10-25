//! `target-pool seed` and `key` against real Git and a stub toolchain. The
//! expected pool keys were computed independently (`shasum -a 256` over the
//! material the Python tool hashed), not by this module.
use super::*;
use crate::testutil;
use serde_json::json;

fn stub_toolchain() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let rustc = dir.path().join("rustc");
    fs::write(&rustc, "#!/bin/sh\nprintf 'rustc 1.0.0\\n'\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&rustc, fs::Permissions::from_mode(0o755)).unwrap();
    }
    dir
}

fn environment(stub: &Path, pool_root: &Path, extra: &[(&str, &str)]) -> Env {
    let mut env = Env::new();
    env.insert(
        "PATH".into(),
        format!(
            "{}:{}",
            stub.display(),
            std::env::var("PATH").unwrap_or_default()
        ),
    );
    env.insert("HOME".into(), std::env::var("HOME").unwrap_or_default());
    env.insert(
        "RSI_TARGET_POOL_ROOT".into(),
        pool_root.to_string_lossy().into_owned(),
    );
    for (name, value) in extra {
        env.insert((*name).into(), (*value).into());
    }
    env
}

fn commit(dir: &Path, name: &str, text: &str) -> String {
    testutil::write(dir, name, text);
    testutil::git(dir, &["add", name]);
    testutil::git(
        dir,
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
    testutil::git(dir, &["rev-parse", "HEAD"])
        .trim()
        .to_string()
}

#[test]
fn the_pool_key_hashes_the_toolchain_and_build_settings_like_the_python_tool() {
    let stub = stub_toolchain();
    let pools = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let plain = environment(stub.path(), pools.path(), &[]);
    assert_eq!(build_key(repo.path(), &plain).unwrap(), "2e4875dd6456");
    let tuned = environment(
        stub.path(),
        pools.path(),
        &[
            ("RUSTFLAGS", "-Cdebuginfo=0"),
            ("CARGO_PROFILE_DEV_DEBUG", "0"),
            ("UNRELATED", "ignored"),
        ],
    );
    assert_eq!(build_key(repo.path(), &tuned).unwrap(), "4430e40de2c0");
}

#[test]
fn python_json_formatting_is_reproduced_for_the_printed_outcome_and_the_marker() {
    let value = json!({"a": [1, 2], "b": {}, "c": [], "d": {"é": "x\n"}});
    assert_eq!(
        dumps(&value, None),
        "{\"a\": [1, 2], \"b\": {}, \"c\": [], \"d\": {\"\\u00e9\": \"x\\n\"}}"
    );
    assert_eq!(
        dumps(&json!({"a": [1, 2], "b": {"c": null}}), Some(1)),
        "{\n \"a\": [\n  1,\n  2\n ],\n \"b\": {\n  \"c\": null\n }\n}"
    );
}

#[test]
fn every_subcommand_is_native_and_an_unknown_one_is_a_usage_error() {
    let ctx = Ctx::new(".", &[]);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    assert_eq!(main(&[], &ctx, &mut out, &mut err), 2);
    assert_eq!(main(&["frobnicate".into()], &ctx, &mut out, &mut err), 2);
    assert!(String::from_utf8(err).unwrap().contains("unknown command"));
    for command in [
        "build",
        "seed-from",
        "gc",
        "reclaim-idle",
        "refresh",
        "status",
        "summary",
    ] {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        main(&[command.into(), "--help".into()], &ctx, &mut out, &mut err);
        let text = String::from_utf8(err).unwrap();
        assert!(!text.contains("not ported"), "{command}: {text}");
        assert!(
            text.contains(&format!("target-pool {command}")),
            "{command}: {text}"
        );
    }
}

#[cfg(unix)]
mod seeding {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    struct Fixture {
        sandbox: tempfile::TempDir,
        pools: tempfile::TempDir,
        stub: tempfile::TempDir,
        pool_sha: String,
        head: String,
        directory: PathBuf,
        key: String,
    }

    fn fixture() -> Fixture {
        let sandbox = tempfile::tempdir().unwrap();
        let pools = tempfile::tempdir().unwrap();
        let stub = stub_toolchain();
        testutil::git(sandbox.path(), &["init", "-q", "-b", "main"]);
        commit(sandbox.path(), "Cargo.toml", "[workspace]\n");
        commit(sandbox.path(), "src/unchanged.rs", "pub fn a() {}\n");
        let pool_sha = commit(sandbox.path(), "src/changed.rs", "pub fn b() {}\n");
        let head = commit(
            sandbox.path(),
            "src/changed.rs",
            "pub fn b() { /* edit */ }\n",
        );
        let env = environment(stub.path(), pools.path(), &[]);
        let git = Git { env: &env };
        let directory = repo_dir(&git, &env, sandbox.path()).unwrap();
        let key = build_key(sandbox.path(), &env).unwrap();
        fs::create_dir_all(&directory).unwrap();
        // One ready pool at the parent commit, built "elsewhere".
        let worktree = directory.join("pool-a");
        let target = worktree.join("target");
        let deps = target.join("debug/deps");
        fs::create_dir_all(&deps).unwrap();
        fs::create_dir_all(target.join("debug/.fingerprint/foo")).unwrap();
        fs::write(deps.join("libfoo-abc123.rlib"), "rlib").unwrap();
        fs::write(deps.join("libfoo-abc123.rmeta"), "rmeta").unwrap();
        fs::write(deps.join("foo-abc123"), "exe").unwrap();
        fs::write(deps.join("foo-abc123.d"), "dep-info").unwrap();
        fs::write(
            deps.join("baked-def456.d"),
            "x\n# env-dep:CARGO_MANIFEST_DIR\n",
        )
        .unwrap();
        fs::write(deps.join("libbaked-def456.rlib"), "baked").unwrap();
        fs::write(deps.join(".cargo-lock"), "").unwrap();
        fs::write(target.join("debug/.fingerprint/foo/lock"), "fp").unwrap();
        fs::hard_link(deps.join("foo-abc123"), target.join("debug/foo")).unwrap();
        fs::write(target.join("CACHEDIR.TAG"), "tag").unwrap();
        fs::write(
            directory.join(format!("{pool_sha}-{key}.json")),
            json!({"name": "pool-a", "sha": pool_sha, "state": "ready", "built_at": 5,
                   "target": target})
            .to_string(),
        )
        .unwrap();
        Fixture {
            sandbox,
            pools,
            stub,
            pool_sha,
            head,
            directory,
            key,
        }
    }

    fn run(fx: &Fixture, extra: &[(&str, &str)]) -> (Value, PathBuf) {
        let env = environment(fx.stub.path(), fx.pools.path(), extra);
        let target = fx.sandbox.path().join("target");
        let outcome = seed(
            &SeedOptions {
                sandbox: fx.sandbox.path().to_path_buf(),
                target: None,
                max_distance: DEFAULT_MAX_DISTANCE,
                build_missing: false,
                build_program: vec![],
            },
            &env,
        )
        .unwrap();
        (outcome, target)
    }

    /// `seed --build-missing`: the detached build program is a stub that records its start.
    fn run_missing(fx: &Fixture, max_distance: u64) -> (Value, PathBuf) {
        run_missing_with(fx, max_distance, &[])
    }

    fn run_missing_with(
        fx: &Fixture,
        max_distance: u64,
        extra: &[(&str, &str)],
    ) -> (Value, PathBuf) {
        let started = fx.stub.path().join("build-started");
        let program = fx.stub.path().join("build-program");
        fs::write(
            &program,
            format!("#!/bin/sh\necho \"$@\" > {}\n", started.display()),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        // A systemd-run on the path must not start a real unit on a Linux host.
        let sd = fx.stub.path().join("systemd-run");
        fs::write(
            &sd,
            "#!/bin/sh\nwhile [ \"$#\" -gt 0 ] && [ \"$1\" != -- ]; do shift; done\n[ \"$#\" -gt 0 ] && shift\nexec \"$@\"\n",
        )
        .unwrap();
        fs::set_permissions(&sd, fs::Permissions::from_mode(0o755)).unwrap();
        let systemctl = fx.stub.path().join("systemctl");
        fs::write(&systemctl, "#!/bin/sh\necho infinity\n").unwrap();
        fs::set_permissions(&systemctl, fs::Permissions::from_mode(0o755)).unwrap();
        let mut settings = vec![("RSI_TARGET_POOL_MIN_FREE_GIB", "0")];
        settings.extend_from_slice(extra);
        let env = environment(fx.stub.path(), fx.pools.path(), &settings);
        let outcome = seed(
            &SeedOptions {
                sandbox: fx.sandbox.path().to_path_buf(),
                target: None,
                max_distance,
                build_missing: true,
                build_program: vec![program.to_string_lossy().into_owned()],
            },
            &env,
        )
        .unwrap();
        (outcome, started)
    }

    fn set_rolling(fx: &Fixture, sha: &str) {
        testutil::git(
            fx.sandbox.path(),
            &["update-ref", "refs/remotes/origin/rolling", sha],
        );
    }

    #[test]
    fn seed_with_a_nearby_pool_skips_the_one_off_build_and_records_the_reason() {
        let fx = fixture();
        set_rolling(&fx, &fx.pool_sha); // the tip is not this head
        let (outcome, started) = run_missing(&fx, DEFAULT_MAX_DISTANCE);
        assert_eq!(outcome["result"], "seeded");
        assert_eq!(
            outcome["build"],
            "skipped: nearby pool seeded at distance 1; not building a one-off pool"
        );
        assert!(!started.exists());
        let events = target_pool_ops::read_events(&fx.directory);
        assert_eq!(events["skip"]["reason"], "pool_skipped_nearby");
        assert_eq!(events["skip"]["distance"], 1);
        assert_eq!(events["skip"]["head"], fx.head.as_str());
        assert_eq!(events["skip"]["pool_sha"], fx.pool_sha.as_str());
    }

    #[test]
    fn seed_without_origin_rolling_uses_the_nearby_pool_and_records_the_skip() {
        let fx = fixture();
        let (outcome, _) = run_missing(&fx, DEFAULT_MAX_DISTANCE);
        assert_eq!(outcome["result"], "seeded");
        assert!(
            outcome["build"]
                .as_str()
                .unwrap()
                .starts_with("skipped: nearby pool")
        );
    }

    #[test]
    fn seed_at_the_rolling_tip_with_a_nearby_pool_requests_a_build() {
        let fx = fixture();
        set_rolling(&fx, &fx.head);
        let (outcome, _) = run_missing(&fx, DEFAULT_MAX_DISTANCE);
        assert_eq!(outcome["result"], "seeded", "the nearby pool still seeds");
        assert!(
            outcome["build"].as_str().unwrap().starts_with("started"),
            "{outcome}"
        );
    }

    #[test]
    fn seed_without_a_pool_within_the_distance_limit_requests_a_build() {
        let fx = fixture();
        let (outcome, _) = run_missing(&fx, 0);
        assert_eq!(outcome["result"], "no pool");
        assert!(
            outcome["build"].as_str().unwrap().starts_with("started"),
            "{outcome}"
        );
        assert_eq!(
            outcome["reason"],
            "nearest ancestor pool is 1 commits behind (limit 0)"
        );
    }

    #[test]
    fn seed_at_the_rolling_tip_with_an_exact_pool_uses_it_and_builds_nothing() {
        let fx = fixture();
        set_rolling(&fx, &fx.head);
        let exact = format!("{}-{}", fx.head, fx.key);
        let pool = fx.directory.join("pool-b/target");
        fs::create_dir_all(pool.join("debug/deps")).unwrap();
        fs::write(pool.join("debug/deps/libfoo-abc123.rlib"), "rlib").unwrap();
        fs::write(
            fx.directory.join(format!("{exact}.json")),
            json!({"name": "pool-b", "sha": fx.head, "state": "ready", "built_at": 6, "target": pool}).to_string(),
        )
        .unwrap();
        let (outcome, started) = run_missing(&fx, DEFAULT_MAX_DISTANCE);
        assert_eq!(outcome["result"], "seeded");
        assert_eq!(outcome["distance"], 0);
        assert!(outcome.get("build").is_none(), "{outcome}");
        assert!(!started.exists());
    }
    #[test]
    fn a_sandbox_is_seeded_from_the_nearest_ancestor_pool_and_only_what_differs_rebuilds() {
        let fx = fixture();
        let (outcome, target) = run(&fx, &[]);
        assert_eq!(outcome["result"], "seeded", "{outcome}");
        assert_eq!(outcome["pool_sha"], fx.pool_sha.as_str());
        assert_eq!(outcome["distance"], 1);
        assert_eq!(outcome["key"], fx.key.as_str());
        assert_eq!(outcome["head"], fx.head.as_str());
        assert_eq!(outcome["degraded"], false);
        let pool_deps = fx.directory.join("pool-a/target/debug/deps");
        let deps = target.join("debug/deps");
        // Libraries are hard-linked; dep-info and (without the wrapper) executables are copies.
        let inode = |p: &Path| fs::metadata(p).unwrap().ino();
        assert_eq!(
            inode(&deps.join("libfoo-abc123.rlib")),
            inode(&pool_deps.join("libfoo-abc123.rlib"))
        );
        assert_eq!(
            inode(&deps.join("libfoo-abc123.rmeta")),
            inode(&pool_deps.join("libfoo-abc123.rmeta"))
        );
        assert_ne!(
            inode(&deps.join("foo-abc123.d")),
            inode(&pool_deps.join("foo-abc123.d"))
        );
        assert_ne!(
            inode(&deps.join("foo-abc123")),
            inode(&pool_deps.join("foo-abc123"))
        );
        assert_eq!(fs::read_to_string(deps.join("foo-abc123")).unwrap(), "exe");
        // A unit that read the manifest path is rebuilt, never seeded.
        assert!(!deps.join("libbaked-def456.rlib").exists());
        assert!(!deps.join("baked-def456.d").exists());
        assert_eq!(outcome["skipped_outputs"], 2);
        assert_eq!(outcome["rebuilt_units"], json!(["baked-def456"]));
        assert!(!deps.join(".cargo-lock").exists());
        assert!(target.join("debug/.fingerprint/foo/lock").is_file());
        assert!(target.join("CACHEDIR.TAG").is_file());
        // The marker is what the Python tool wrote (indent=1).
        let marker = fs::read_to_string(target.join(SEED_MARKER)).unwrap();
        assert!(marker.starts_with("{\n \"sandbox\": "), "{marker}");
        assert!(marker.contains("\n \"result\": \"seeded\""));
        assert!(
            !target
                .join(".rsi-tmp")
                .join(format!("target-pool-seed-{}", std::process::id()))
                .exists()
        );
        // Tracked files the pool commit already had get an old mtime; the edited one is fresh.
        let seconds = |name: &str| fs::metadata(fx.sandbox.path().join(name)).unwrap().mtime();
        assert_eq!(seconds("src/unchanged.rs"), OLD_MTIME);
        assert_eq!(seconds("Cargo.toml"), OLD_MTIME);
        assert!(seconds("src/changed.rs") > OLD_MTIME + 1_000_000);
        assert!(
            seconds("src") > OLD_MTIME + 1_000_000,
            "the directory holding the edit is fresh"
        );
        assert_eq!(outcome["changed_files"], 1);
        assert_eq!(outcome["unchanged_files"], 2);
        // The repository checkout is recorded for later refreshes.
        assert!(
            fs::read_to_string(fx.directory.join(REPO_FILE))
                .unwrap()
                .contains(
                    &fx.sandbox
                        .path()
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .to_string()
                )
        );
        // A target that is no longer empty is left alone.
        let (again, _) = run(&fx, &[]);
        assert_eq!(again["result"], "skipped: target is not empty");
    }

    #[test]
    fn the_wrapper_that_unlinks_outputs_lets_executables_be_linked() {
        let fx = fixture();
        let wrapper = fx.stub.path().join("rsi-build-rustc");
        fs::write(&wrapper, "#!/bin/sh\nprintf 'unlink-outputs\\n'\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
        let (outcome, target) = run(&fx, &[("RUSTC_WRAPPER", wrapper.to_str().unwrap())]);
        assert_eq!(outcome["result"], "seeded", "{outcome}");
        let pool = fx.directory.join("pool-a/target/debug");
        let inode = |p: &Path| fs::metadata(p).unwrap().ino();
        assert_eq!(
            inode(&target.join("debug/deps/foo-abc123")),
            inode(&pool.join("deps/foo-abc123"))
        );
        // Cargo's uplifted copy stays one inode with its deps/ original.
        assert_eq!(
            inode(&target.join("debug/foo")),
            inode(&target.join("debug/deps/foo-abc123"))
        );
    }

    #[test]
    fn no_ready_pool_with_this_key_or_ancestry_is_a_named_reason_not_an_error() {
        let fx = fixture();
        let tuned = [("RUSTFLAGS", "-Cdebuginfo=0")];
        let (outcome, target) = run(&fx, &tuned);
        assert_eq!(outcome["result"], "no pool");
        assert_eq!(outcome["reason"], "no ready pool has this build key");
        assert!(!target.join(SEED_MARKER).exists());
        // The pool exists but is not an ancestor of HEAD.
        testutil::git(fx.sandbox.path(), &["checkout", "-q", "--orphan", "other"]);
        commit(fx.sandbox.path(), "Cargo.toml", "[workspace]\n# other\n");
        let (outcome, _) = run(&fx, &[]);
        assert_eq!(outcome["result"], "no pool");
        assert!(
            outcome["reason"]
                .as_str()
                .unwrap()
                .contains("is an ancestor of HEAD"),
            "{outcome}"
        );
    }

    #[test]
    fn a_pool_that_is_too_far_behind_or_gone_from_disk_is_not_offered() {
        let fx = fixture();
        let env = environment(fx.stub.path(), fx.pools.path(), &[]);
        let git = Git { env: &env };
        let pools = ready_pools(&fx.directory, &fx.key);
        assert_eq!(pools.len(), 1);
        assert!(choose_pool(&git, fx.sandbox.path(), &fx.head, &pools, 1).is_some());
        assert!(choose_pool(&git, fx.sandbox.path(), &fx.head, &pools, 0).is_none());
        // A marker whose pool directory vanished is not ready.
        fs::remove_dir_all(fx.directory.join("pool-a")).unwrap();
        assert!(ready_pools(&fx.directory, &fx.key).is_empty());
    }

    #[test]
    fn a_pool_gc_holds_exclusively_is_skipped() {
        let fx = fixture();
        let env = environment(fx.stub.path(), fx.pools.path(), &[]);
        let git = Git { env: &env };
        let pools = ready_pools(&fx.directory, &fx.key);
        let exclusive = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(fx.directory.join("pool-a.use"))
            .unwrap();
        use std::os::fd::AsRawFd;
        assert_eq!(
            unsafe { libc::flock(exclusive.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        let claimed = claim_pool(&fx.directory, &pools, |left| {
            choose_pool(&git, fx.sandbox.path(), &fx.head, left, 200)
        });
        assert!(claimed.is_none(), "a pool being removed is never offered");
        drop(exclusive);
        let claimed = claim_pool(&fx.directory, &pools, |left| {
            choose_pool(&git, fx.sandbox.path(), &fx.head, left, 200)
        });
        assert!(claimed.is_some());
    }

    #[test]
    fn the_nearest_ancestor_wins_and_an_exact_pool_is_distance_zero() {
        let fx = fixture();
        let env = environment(fx.stub.path(), fx.pools.path(), &[]);
        let git = Git { env: &env };
        let sha_at = |back: usize| {
            testutil::git(fx.sandbox.path(), &["rev-parse", &format!("HEAD~{back}")])
                .trim()
                .to_string()
        };
        let mut pools = vec![json!({"sha": sha_at(2)}), json!({"sha": sha_at(1)})];
        let chosen = choose_pool(&git, fx.sandbox.path(), &fx.head, &pools, 200).unwrap();
        assert_eq!(
            (chosen["sha"].as_str().unwrap(), chosen["distance"].as_u64()),
            (sha_at(1).as_str(), Some(1))
        );
        pools.push(json!({"sha": fx.head}));
        let chosen = choose_pool(&git, fx.sandbox.path(), &fx.head, &pools, 200).unwrap();
        assert_eq!(chosen["distance"], 0);
        // A pool of a commit that is not an ancestor is never chosen.
        let side = testutil::git(
            fx.sandbox.path(),
            &[
                "commit-tree",
                &format!("{}^{{tree}}", sha_at(2)),
                "-p",
                &sha_at(2),
                "-m",
                "side",
            ],
        )
        .trim()
        .to_string();
        assert!(
            choose_pool(
                &git,
                fx.sandbox.path(),
                &fx.head,
                &[json!({"sha": side})],
                200
            )
            .is_none()
        );
    }

    #[test]
    fn seed_reports_the_nearest_pool_next_to_no_pool() {
        let fx = fixture();
        let env = environment(fx.stub.path(), fx.pools.path(), &[]);
        let git = Git { env: &env };
        let pools = ready_pools(&fx.directory, &fx.key);
        assert_eq!(
            explain_no_pool(&git, fx.sandbox.path(), &fx.head, &pools, 0),
            json!({
                "reason": "nearest ancestor pool is 1 commits behind (limit 0)",
                "nearest_pool": {"sha": fx.pool_sha, "distance": 1}
            })
        );
        let (outcome, _) = run_missing(&fx, 0);
        assert_eq!(outcome["result"], "no pool");
        assert_eq!(
            outcome["nearest_pool"],
            json!({"sha": fx.pool_sha, "distance": 1})
        );
    }

    #[test]
    fn a_key_mismatch_a_non_empty_target_and_a_non_workspace_are_not_seeded() {
        let fx = fixture();
        let target = fx.sandbox.path().join("target");
        fs::create_dir(&target).unwrap();
        let (outcome, _) = run(&fx, &[("CARGO_PROFILE_DEV_DEBUG", "full")]);
        assert_eq!(
            outcome["result"], "no pool",
            "a pool built another way is not offered"
        );
        fs::create_dir(target.join("debug")).unwrap();
        let (outcome, _) = run(&fx, &[]);
        assert_eq!(outcome["result"], "skipped: target is not empty");
        fs::remove_file(fx.sandbox.path().join("Cargo.toml")).unwrap();
        let (outcome, _) = run(&fx, &[]);
        assert_eq!(outcome["result"], "skipped: not a Cargo workspace");
        assert!(
            fs::read_dir(target.join("debug")).unwrap().next().is_none(),
            "nothing was copied"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn seed_keys_and_requests_the_sandbox_head_despite_inherited_git_context() {
        let fx = fixture();
        // The shared checkout (another repository, another commit) is what an inherited
        // GIT_DIR names; the worker's pinned HEAD is the sandbox's own.
        let elsewhere = tempfile::tempdir().unwrap();
        testutil::git(elsewhere.path(), &["init", "-q", "-b", "main"]);
        let other = commit(elsewhere.path(), "Cargo.toml", "[workspace]\n# elsewhere\n");
        assert_ne!(other, fx.head);
        let git_dir = elsewhere.path().join(".git");
        let index = git_dir.join("index").to_string_lossy().into_owned();
        let inherited = [
            ("GIT_DIR", git_dir.to_str().unwrap()),
            ("GIT_WORK_TREE", elsewhere.path().to_str().unwrap()),
            ("GIT_INDEX_FILE", index.as_str()),
        ];
        let (outcome, _) = run(&fx, &inherited);
        assert_eq!(outcome["head"], fx.head.as_str(), "{outcome}");
        assert_eq!(outcome["result"], "seeded");
        // The detached build is asked for the sandbox's head, with the same inherited context.
        let fx = fixture();
        let (outcome, started) = run_missing_with(&fx, 0, &inherited);
        assert_eq!(outcome["head"], fx.head.as_str(), "{outcome}");
        assert!(
            outcome["build"].as_str().unwrap().starts_with("started"),
            "{outcome}"
        );
        let args = fs::read_to_string(&started).expect("the fake systemd-run waits for its child");
        assert!(args.contains(&format!("--sha {}", fx.head)), "{args}");
        assert!(!args.contains(&other), "{args}");
    }
}

#[cfg(unix)]
#[test]
fn a_read_only_pool_output_is_copied_writable_with_its_times_kept() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("exe");
    fs::write(&source, b"binary").unwrap();
    set_times(&source, 1_000_000.0, true).unwrap();
    fs::set_permissions(&source, fs::Permissions::from_mode(0o555)).unwrap();
    let destination = dir.path().join("copy");
    copy_file(&source, &destination).unwrap();
    let meta = fs::metadata(&destination).unwrap();
    assert_eq!(fs::read(&destination).unwrap(), b"binary");
    assert_eq!(
        meta.permissions().mode() & 0o777,
        0o755,
        "owner-writable, the rest kept"
    );
    use std::os::unix::fs::MetadataExt;
    assert_eq!(meta.mtime(), 1_000_000);
}
