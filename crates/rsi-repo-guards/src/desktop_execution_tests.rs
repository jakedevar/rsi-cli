use super::*;
use crate::testutil::{repo_root, write};
use std::process::Command;
use std::sync::atomic::Ordering;

fn external_temp() -> tempfile::TempDir {
    Policy::from_process(&repo_root())
        .unwrap()
        .tempdir("rsi-desktop-rust-")
        .unwrap()
}
fn env() -> Environment {
    ["PATH", "SystemRoot", "SYSTEMROOT", "PATHEXT"]
        .into_iter()
        .filter_map(|key| std::env::var(key).ok().map(|v| (key.to_owned(), v)))
        .collect()
}
fn git(root: &Path, args: &[&str]) -> String {
    let mut environment: crate::scratch_git::Environment = env()
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect();
    environment.insert("GIT_CONFIG_NOSYSTEM".into(), "1".into());
    environment.insert(
        "GIT_CONFIG_GLOBAL".into(),
        if cfg!(windows) { "NUL" } else { "/dev/null" }.into(),
    );
    let policy = Policy::new(
        &repo_root(),
        &std::env::current_dir().unwrap(),
        environment,
        &[],
    )
    .unwrap();
    let mut pinned = vec![
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "commit.gpgsign=false",
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.invalid",
    ];
    pinned.extend_from_slice(args);
    let mut command = policy.command(root, &pinned).unwrap();
    let done = command.output().unwrap();
    assert!(
        done.status.success(),
        "{}",
        String::from_utf8_lossy(&done.stderr)
    );
    String::from_utf8_lossy(&done.stdout).into_owned()
}
fn fixture() -> (tempfile::TempDir, PathBuf) {
    let temp = external_temp();
    let repo = temp.path().join("repo");
    write(&repo, "desktop/package.json", "{}");
    write(
        &repo,
        "desktop/src-tauri/Cargo.toml",
        "[package]\nname='fixture'",
    );
    (temp, repo)
}
fn staged(repo: &Path, parent: &Path) -> tempfile::TempDir {
    let stage = tempfile::Builder::new()
        .prefix("stage-")
        .tempdir_in(parent)
        .unwrap();
    stage_tree(repo, stage.path(), &candidate_files(repo, &env()).unwrap()).unwrap();
    stage
}
fn output(code: i32, text: &str) -> StepOutput {
    StepOutput {
        code,
        output: text.into(),
        timed_out: false,
        cancelled: None,
        output_complete: true,
    }
}
struct Fake {
    outcomes: Vec<StepOutput>,
    calls: Vec<(Step, PathBuf)>,
    missing: Option<Value>,
    fail_io: bool,
}
impl Fake {
    fn new(values: Vec<StepOutput>) -> Self {
        Self {
            outcomes: values,
            calls: Vec::new(),
            missing: None,
            fail_io: false,
        }
    }
    fn passing() -> Self {
        Self::new(vec![
            output(0, ""),
            output(0, "# tests 31\n"),
            output(0, "test result: ok. 31 passed; 0 failed;\n"),
        ])
    }
}
impl Backend for Fake {
    fn preflight(&mut self) -> io::Result<Option<Value>> {
        Ok(self.missing.take())
    }
    fn step(
        &mut self,
        step: Step,
        cwd: &Path,
        environment: &Environment,
    ) -> io::Result<StepOutput> {
        assert!(cwd.exists());
        assert!(environment.contains_key("npm_config_cache"));
        self.calls.push((step, cwd.to_path_buf()));
        if self.fail_io {
            return Err(io::Error::other("fixture spawn failed"));
        }
        Ok(self.outcomes.remove(0))
    }
}
fn lane(fake: &mut Fake) -> (i32, Value) {
    let (temp, repo) = fixture();
    run_lane(&repo, &env(), temp.path(), fake).unwrap()
}

#[test]
fn whole_tree_preserves_workspace_and_include_bytes() {
    let (temp, repo) = fixture();
    let paths = [
        "Cargo.toml",
        "Cargo.lock",
        ".cargo/config.toml",
        "rust-toolchain.toml",
        "crates/unrelated/src/lib.rs",
        "crates/shared/Cargo.toml",
        "docs/include.bin",
        "docs/keybindings.md",
    ];
    for path in paths {
        write(&repo, path, &format!("unchanged {path}\n"));
    }
    fs::write(repo.join("docs/include.bin"), [0, 255, 1, 128]).unwrap();
    let stage = staged(&repo, temp.path());
    for path in paths {
        assert_eq!(
            fs::read(stage.path().join(path)).unwrap(),
            fs::read(repo.join(path)).unwrap()
        );
    }
}
#[test]
fn rebuilt_and_machine_local_paths_are_excluded() {
    let (temp, repo) = fixture();
    for path in [
        "desktop/node_modules/a/index.js",
        "desktop/dist/main.js",
        "desktop/src-tauri/gen/schema.json",
        "desktop/src-tauri/target/a",
        "crates/a/target/b",
        ".git/HEAD",
    ] {
        write(&repo, path, "generated");
    }
    write(&repo, "desktop/src/gen/keep.ts", "keep");
    let stage = staged(&repo, temp.path());
    let copied = candidate_files(stage.path(), &env()).unwrap();
    assert_eq!(
        copied,
        vec![
            PathBuf::from("desktop/package.json"),
            PathBuf::from("desktop/src/gen/keep.ts"),
            PathBuf::from("desktop/src-tauri/Cargo.toml")
        ]
    );
    assert_eq!(
        fs::read_to_string(repo.join("desktop/dist/main.js")).unwrap(),
        "generated"
    );
}
#[test]
fn git_adapter_copies_tracked_new_and_ignored_tracked_but_skips_deleted_and_ignored_new() {
    let (temp, repo) = fixture();
    git(&repo, &["init", "-q"]);
    write(&repo, "docs/deleted.md", "delete");
    write(&repo, "keep-ignored.bin", "tracked");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "base"]);
    write(&repo, ".gitignore", "*.bin\n");
    write(&repo, "ignored.bin", "ignore");
    write(&repo, "docs/new\nline.md", "new");
    fs::remove_file(repo.join("docs/deleted.md")).unwrap();
    let stage = staged(&repo, temp.path());
    let files = candidate_files(stage.path(), &env()).unwrap();
    assert_eq!(
        files,
        [
            ".gitignore",
            "desktop/package.json",
            "desktop/src-tauri/Cargo.toml",
            "docs/new\nline.md",
            "keep-ignored.bin"
        ]
        .map(PathBuf::from)
    );
    assert_eq!(
        fs::read_to_string(stage.path().join("keep-ignored.bin")).unwrap(),
        "tracked"
    );
    assert_eq!(git(&repo, &["log", "-1", "--format=%s"]).trim(), "base");
}
#[test]
fn candidate_git_location_environment_is_isolated() {
    let (temp, repo) = fixture();
    git(&repo, &["init", "-q"]);
    let foreign = temp.path().join("foreign");
    write(&foreign, "foreign-file", "foreign");
    git(&foreign, &["init", "-q"]);
    git(&foreign, &["add", "."]);
    let mut environment = env();
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_COMMON_DIR",
    ] {
        environment.insert(key.into(), foreign.to_string_lossy().into_owned());
    }
    assert_eq!(
        candidate_files(&repo, &environment).unwrap(),
        ["desktop/package.json", "desktop/src-tauri/Cargo.toml"].map(PathBuf::from)
    );
}
#[cfg(unix)]
#[test]
fn symlinks_are_preserved_without_walking_directory_links() {
    use std::os::unix::fs::symlink;
    let (temp, repo) = fixture();
    symlink("package.json", repo.join("desktop/link.json")).unwrap();
    symlink("missing", repo.join("desktop/dangling")).unwrap();
    symlink("src-tauri", repo.join("desktop/link-dir")).unwrap();
    let stage = staged(&repo, temp.path());
    for (path, target) in [
        ("desktop/link.json", "package.json"),
        ("desktop/dangling", "missing"),
        ("desktop/link-dir", "src-tauri"),
    ] {
        assert_eq!(
            fs::read_link(stage.path().join(path)).unwrap(),
            PathBuf::from(target)
        );
    }
}
#[cfg(unix)]
#[test]
fn executable_permissions_survive_staging() {
    use std::os::unix::fs::PermissionsExt;
    let (temp, repo) = fixture();
    write(&repo, "run.sh", "exit 0\n");
    fs::set_permissions(repo.join("run.sh"), fs::Permissions::from_mode(0o751)).unwrap();
    let stage = staged(&repo, temp.path());
    assert_eq!(
        fs::metadata(stage.path().join("run.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o751
    );
}
#[test]
fn unsafe_stage_and_relative_paths_are_refused() {
    let (temp, repo) = fixture();
    let inside = repo.join("stage");
    fs::create_dir(&inside).unwrap();
    assert!(stage_tree(&repo, &inside, &[]).is_err());
    assert!(stage_tree(&repo, temp.path(), &[]).is_err());
    let stage = tempfile::tempdir_in(temp.path()).unwrap();
    for path in [
        PathBuf::from("../escape"),
        repo.join("absolute"),
        PathBuf::new(),
    ] {
        assert!(stage_tree(&repo, stage.path(), &[path]).is_err());
    }
}
#[cfg(unix)]
#[test]
fn symlink_parents_cannot_redirect_stage_writes() {
    use std::os::unix::fs::symlink;
    let (temp, repo) = fixture();
    let outside = temp.path().join("outside");
    write(&outside, "file", "untouched");
    symlink(&outside, repo.join("redirect")).unwrap();
    let stage = tempfile::tempdir_in(temp.path()).unwrap();
    assert!(stage_tree(&repo, stage.path(), &[PathBuf::from("redirect/file")]).is_err());
    assert_eq!(
        fs::read_to_string(outside.join("file")).unwrap(),
        "untouched"
    );
}
#[test]
fn environment_preserves_wrapper_and_uses_private_or_explicit_caches() {
    let base = Environment::from([
        ("RUSTC_WRAPPER".into(), "wrapper".into()),
        ("CARGO_TARGET_DIR".into(), "job/target".into()),
        ("RSI_PROCESS_OWNERSHIP_NAMESPACE".into(), "ns".into()),
    ]);
    let done = lane_env(&base, Path::new("scratch"));
    assert_eq!(
        done["CARGO_TARGET_DIR"],
        Path::new("job/target/desktop-tauri").to_string_lossy()
    );
    assert_eq!(done["RUSTC_WRAPPER"], "wrapper");
    assert_eq!(done["CARGO_INCREMENTAL"], "0");
    assert_eq!(
        done["npm_config_cache"],
        Path::new("scratch/npm-cache").to_string_lossy()
    );
    assert!(done.get("RSI_PROCESS_OWNERSHIP_NAMESPACE").is_none());
    let explicit = Environment::from([
        ("DESKTOP_LANE_NPM_CACHE".into(), "cache".into()),
        ("DESKTOP_LANE_CARGO_TARGET".into(), "target".into()),
        ("CARGO_INCREMENTAL".into(), "1".into()),
    ]);
    let done = lane_env(&explicit, Path::new("scratch"));
    assert_eq!(done["npm_config_cache"], "cache");
    assert_eq!(done["CARGO_TARGET_DIR"], "target");
    assert_eq!(done["CARGO_INCREMENTAL"], "1");
    assert_eq!(
        lane_env(&Environment::new(), Path::new("scratch"))["CARGO_TARGET_DIR"],
        Path::new("scratch/cargo-target").to_string_lossy()
    );
}
#[test]
fn preflight_requires_every_runner_and_linux_webkit() {
    for missing in ["node", "npm", "cargo", "pkg-config"] {
        let done = runner_status(
            "linux",
            |tool| tool != missing,
            || panic!("no probe until all tools exist"),
        )
        .unwrap();
        assert_eq!(done["runner"], missing);
        assert_eq!(done["status"], "runner_unavailable");
        assert_eq!(done["executed_tests"], 0);
    }
    assert_eq!(
        runner_status("linux", |_| true, || Ok(1)).unwrap()["runner"],
        "webkit2gtk-4.1"
    );
    assert!(
        runner_status("linux", |_| true, || Err(io::Error::other("timed out"))).unwrap()["reason"]
            .as_str()
            .unwrap()
            .contains("timed out")
    );
    assert_eq!(runner_status("linux", |_| true, || Ok(0)), None);
    for platform in ["darwin", "win32"] {
        assert_eq!(
            runner_status(
                platform,
                |_| true,
                || panic!("platform needs no webkit probe")
            ),
            None
        );
    }
}
#[test]
fn mandatory_commands_and_default_bounds_match_legacy() {
    assert_eq!(Step::Install.command(), ["npm", "ci"]);
    assert_eq!(Step::Frontend.command(), ["npm", "test"]);
    assert_eq!(Step::Cargo.command(), ["cargo", "test", "--locked"]);
    for (step, seconds) in [
        (Step::Install, 900),
        (Step::Frontend, 600),
        (Step::Cargo, 1800),
    ] {
        assert_eq!(
            step.timeout(&Environment::new()).unwrap(),
            Duration::from_secs(seconds)
        );
    }
}
#[test]
fn invalid_bounds_cannot_disable_deadlines() {
    for key in [
        "DESKTOP_LANE_NPM_CI_TIMEOUT_SECONDS",
        "DESKTOP_LANE_NPM_TEST_TIMEOUT_SECONDS",
        "DESKTOP_LANE_CARGO_TEST_TIMEOUT_SECONDS",
    ] {
        for invalid in ["0", "-1", "NaN", "inf", "garbage", "1e300"] {
            let environment = Environment::from([(key.into(), invalid.into())]);
            let step = if key.contains("NPM_CI") {
                Step::Install
            } else if key.contains("NPM_TEST") {
                Step::Frontend
            } else {
                Step::Cargo
            };
            assert!(step.timeout(&environment).is_err(), "{key}={invalid}");
        }
    }
}
#[test]
fn both_sets_run_in_order_and_positive_counts_are_required() {
    let mut fake = Fake::passing();
    let (code, done) = lane(&mut fake);
    assert_eq!(code, 0);
    assert_eq!(done["status"], "passed");
    assert_eq!(
        (
            done["npm_tests"].as_u64(),
            done["cargo_tests"].as_u64(),
            done["executed_tests"].as_u64()
        ),
        (Some(31), Some(31), Some(62))
    );
    assert_eq!(
        fake.calls.iter().map(|v| v.0).collect::<Vec<_>>(),
        [Step::Install, Step::Frontend, Step::Cargo]
    );
    assert_eq!(fake.calls[2].1.file_name().unwrap(), "src-tauri");
    assert!(fake.calls.iter().all(|v| !v.1.exists()));
}
#[test]
fn spec_and_multiple_cargo_summaries_are_summed() {
    let mut fake = Fake::new(vec![
        output(0, ""),
        output(0, "ℹ tests 7\r\n"),
        output(
            0,
            "test result: ok. 31 passed;\ntest result: ok. 4 passed; 0 failed;\n",
        ),
    ]);
    let (_, done) = lane(&mut fake);
    assert_eq!(done["executed_tests"], 42);
    assert_eq!(
        count_node("prefix # tests 5\n# tests 7 trailing\n# tests 3\n"),
        3
    );
}
#[test]
fn ordinary_failures_stop_at_the_failed_step() {
    for index in 0..3 {
        let mut fake = Fake::passing();
        fake.outcomes[index].code = 101;
        let (code, done) = lane(&mut fake);
        assert_eq!(code, 1);
        assert_eq!(done["status"], "failed");
        assert_eq!(done["exit_code"], 101);
        assert_eq!(fake.calls.len(), index + 1);
        assert_eq!(done["executed_tests"], 0);
        assert!(fake.calls.iter().all(|v| !v.1.exists()));
    }
}
#[test]
fn only_locked_update_diagnostics_are_stale_lock() {
    for diagnostic in [
        "error: cannot update the lock file /copy/Cargo.lock because --locked was passed",
        "error: the lock file C:\\copy\\Cargo.lock needs to be updated but --locked was passed",
    ] {
        let mut fake = Fake::passing();
        fake.outcomes[2] = output(101, diagnostic);
        let (_, done) = lane(&mut fake);
        assert_eq!(done["status"], "desktop_lock_stale");
        assert_eq!(done["reason"], LOCK_FIX);
        assert_eq!(done["executed_tests"], 31);
    }
    for diagnostic in [
        "error: failed to download dependency\n--offline was specified",
        "error[E0308]: mismatched types",
        "lock file X needs to be updated\nbut --locked was passed",
    ] {
        let mut fake = Fake::passing();
        fake.outcomes[2] = output(101, diagnostic);
        assert_eq!(lane(&mut fake).1["status"], "failed");
    }
}
#[test]
fn empty_test_sets_are_red_even_with_zero_exit() {
    for index in [1, 2] {
        let mut fake = Fake::passing();
        fake.outcomes[index].output.clear();
        let (code, done) = lane(&mut fake);
        assert_eq!(code, 1);
        assert_eq!(done["status"], "no_tests_executed");
        assert_eq!(fake.calls.len(), 3);
    }
}
#[test]
fn unavailable_preflight_runs_nothing_and_allocates_nothing() {
    let (temp, repo) = fixture();
    let mut fake = Fake::passing();
    fake.missing = Some(unavailable("node", "node missing"));
    let (code, done) = run_lane(
        &repo,
        &env(),
        &temp.path().join("nonexistent-parent"),
        &mut fake,
    )
    .unwrap();
    assert_eq!(code, 1);
    assert_eq!(done["status"], "runner_unavailable");
    assert_eq!(fake.calls.len(), 0);
}
#[test]
fn timeout_cancellation_and_output_loss_remain_red_and_cleanup_stage() {
    for kind in 0..3 {
        let mut fake = Fake::passing();
        let step = &mut fake.outcomes[1];
        let status = match kind {
            0 => {
                step.timed_out = true;
                step.code = 124;
                "desktop_lane_timed_out"
            }
            1 => {
                step.cancelled = Some(15);
                step.code = 143;
                "cancelled"
            }
            _ => {
                step.output_complete = false;
                "output_incomplete"
            }
        };
        let (code, done) = lane(&mut fake);
        assert_eq!(code, 1);
        assert_eq!(done["status"], status);
        assert_eq!(done["executed_tests"], 31);
        assert_eq!(fake.calls.len(), 2);
        assert!(fake.calls.iter().all(|v| !v.1.exists()));
    }
}
#[test]
fn spawn_errors_cleanup_stage_without_touching_candidate() {
    let (temp, repo) = fixture();
    let mut fake = Fake::passing();
    fake.fail_io = true;
    assert!(run_lane(&repo, &env(), temp.path(), &mut fake).is_err());
    assert!(fake.calls.iter().all(|v| !v.1.exists()));
    assert_eq!(
        fs::read_to_string(repo.join("desktop/package.json")).unwrap(),
        "{}"
    );
}
#[cfg(unix)]
#[test]
fn shared_process_adapter_keeps_output_and_exit_code() {
    let temp = external_temp();
    let mut text = String::new();
    let done = run_step(
        &[
            "/bin/sh".into(),
            "-c".into(),
            "printf '# tests 7\\ntail'; printf 'error\\n' >&2; exit 7".into(),
        ],
        temp.path(),
        &env(),
        Step::Frontend,
        &AtomicUsize::new(0),
        |line| text.push_str(line),
    )
    .unwrap();
    assert_eq!(done.code, 7);
    assert!(done.output_complete);
    assert!(done.output.contains("tail"));
    assert!(text.contains("error\n"));
}
#[cfg(unix)]
#[test]
fn silent_and_closed_stdout_steps_keep_the_deadline() {
    let temp = external_temp();
    let mut environment = env();
    environment.insert(
        "DESKTOP_LANE_NPM_TEST_TIMEOUT_SECONDS".into(),
        "0.08".into(),
    );
    for command in ["sleep 30 & wait", "exec 1>&- 2>&-; sleep 30"] {
        let start = Instant::now();
        let done = run_step(
            &["/bin/sh".into(), "-c".into(), command.into()],
            temp.path(),
            &environment,
            Step::Frontend,
            &AtomicUsize::new(0),
            |_| {},
        )
        .unwrap();
        assert!(done.timed_out);
        assert_eq!(done.code, 124);
        assert!(start.elapsed() < Duration::from_secs(3));
    }
}
#[cfg(unix)]
#[test]
fn shared_process_adapter_observes_cancellation() {
    let temp = external_temp();
    let signal = AtomicUsize::new(0);
    signal.store(15, Ordering::Relaxed);
    let done = run_step(
        &["/bin/sh".into(), "-c".into(), "sleep 30".into()],
        temp.path(),
        &env(),
        Step::Frontend,
        &signal,
        |_| {},
    )
    .unwrap();
    assert_eq!(done.cancelled, Some(15));
    assert_eq!(done.code, 143);
    assert!(!done.timed_out);
}

// APFS rejects invalid UTF-8 filenames before Git can observe them. Keep the
// actual filesystem path in the Linux lane; test parser bytes on every Unix.
#[cfg(target_os = "linux")]
#[test]
fn bounded_git_preserves_non_utf8_filenames() {
    use std::os::unix::ffi::OsStrExt;
    let (temp, repo) = fixture();
    git(&repo, &["init", "-q"]);
    let name = PathBuf::from(std::ffi::OsStr::from_bytes(b"docs/name-\xff.bin"));
    fs::create_dir_all(repo.join("docs")).unwrap();
    fs::write(repo.join(&name), b"binary name").unwrap();
    let listed = candidate_files(&repo, &env()).unwrap();
    assert!(listed.contains(&name));
    let stage = tempfile::tempdir_in(temp.path()).unwrap();
    stage_tree(&repo, stage.path(), &listed).unwrap();
    assert_eq!(fs::read(stage.path().join(name)).unwrap(), b"binary name");
}

#[cfg(unix)]
#[test]
fn git_path_bytes_preserve_non_utf8_nuls_and_newlines() {
    use std::os::unix::ffi::OsStrExt;
    let files =
        candidate_paths(b"docs/name-\xff.bin\0docs/line\nname\0docs/name-\xff.bin\0").unwrap();
    assert_eq!(
        files
            .iter()
            .map(|path| path.as_os_str().as_bytes())
            .collect::<Vec<_>>(),
        vec![
            b"docs/line\nname".as_slice(),
            b"docs/name-\xff.bin".as_slice()
        ]
    );
    assert!(candidate_paths(b"../escape\0").is_err());
}

#[cfg(unix)]
#[test]
fn metadata_capture_timeout_cancellation_and_oversize_are_fail_closed() {
    let temp = external_temp();
    let command = vec!["/bin/sh".into(), "-c".into(), "exec sleep 30".into()];
    let error = capture(
        &command,
        temp.path(),
        &env(),
        Duration::from_millis(30),
        &AtomicUsize::new(0),
    )
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    let error = candidate_files_with(temp.path(), &env(), &AtomicUsize::new(15)).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    // Emit >8MiB from a controlled external utility, with no line boundaries.
    let command = vec![
        "/bin/sh".into(),
        "-c".into(),
        "head -c 9437184 /dev/zero".into(),
    ];
    assert!(
        capture(
            &command,
            temp.path(),
            &env(),
            Duration::from_secs(5),
            &AtomicUsize::new(0)
        )
        .unwrap_err()
        .to_string()
        .contains("oversized")
    );
}

#[cfg(unix)]
fn executable_fixture(root: &Path, name: &str, source: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = write(root, name, source);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}
#[cfg(unix)]
#[test]
fn real_backend_runs_both_stub_test_sets_in_the_private_tree() {
    let (temp, repo) = fixture();
    let bin = temp.path().join("bin");
    for name in ["node", "pkg-config"] {
        executable_fixture(&bin, name, "#!/bin/sh\nexit 0\n");
    }
    executable_fixture(
        &bin,
        "npm",
        "#!/bin/sh\nprintf '%s|%s|%s|%s\\n' \"$*\" \"$PWD\" \"$CARGO_TARGET_DIR\" \"$RUSTC_WRAPPER\" >> \"$TRACE\"\nif [ \"$1\" = test ]; then printf '# tests 3\\n'; fi\n",
    );
    executable_fixture(
        &bin,
        "cargo",
        "#!/bin/sh\nprintf '%s|%s|%s|%s\\n' \"$*\" \"$PWD\" \"$CARGO_TARGET_DIR\" \"$RUSTC_WRAPPER\" >> \"$TRACE\"\nprintf 'test result: ok. 4 passed; 0 failed;\\n'\n",
    );
    let mut environment = env();
    environment.insert(
        "PATH".into(),
        std::env::join_paths([bin, PathBuf::from("/usr/bin"), PathBuf::from("/bin")])
            .unwrap()
            .to_string_lossy()
            .into_owned(),
    );
    environment.insert(
        "TRACE".into(),
        temp.path().join("trace").to_string_lossy().into_owned(),
    );
    environment.insert("RUSTC_WRAPPER".into(), "preserved-wrapper".into());
    let cancel = AtomicUsize::new(0);
    let isolation = Isolation::unavailable("test_non_linux_or_unavailable");
    let mut log = String::new();
    let mut emit = |line: &str| log.push_str(line);
    let mut backend = ProcessBackend {
        environment: &environment,
        repo: &repo,
        isolation: &isolation,
        cancel: &cancel,
        emit: &mut emit,
        deadline: None,
    };
    let (code, result) =
        run_lane_with_cancel(&repo, &environment, temp.path(), &cancel, &mut backend).unwrap();
    assert_eq!(code, 0);
    assert_eq!(result["executed_tests"], 7);
    assert!(log.contains("# tests 3"));
    let trace = fs::read_to_string(temp.path().join("trace")).unwrap();
    let lines = trace.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 3);
    assert!(lines[0].starts_with("ci|"));
    assert!(lines[1].starts_with("test|"));
    assert!(lines[2].starts_with("test --locked|"));
    for line in lines {
        let fields = line.split('|').collect::<Vec<_>>();
        let cwd = Path::new(fields[1]);
        assert_ne!(cwd, &repo);
        assert!(!cwd.exists());
        assert!(fields[2].ends_with("cargo-target"));
        assert_eq!(fields[3], "preserved-wrapper");
    }
    assert_eq!(
        fs::read_to_string(repo.join("desktop/package.json")).unwrap(),
        "{}"
    );
}

#[cfg(unix)]
#[test]
fn production_preflight_checks_executable_permissions() {
    let (temp, repo) = fixture();
    let bin = temp.path().join("bin");
    for name in ["node", "npm", "cargo", "pkg-config"] {
        executable_fixture(&bin, name, "#!/bin/sh\nexit 0\n");
    }
    let mut environment = env();
    environment.insert("PATH".into(), bin.to_string_lossy().into_owned());
    let cancel = AtomicUsize::new(0);
    let isolation = Isolation::unavailable("test");
    let mut emit = |_: &str| {};
    let mut backend = ProcessBackend {
        environment: &environment,
        repo: &repo,
        isolation: &isolation,
        cancel: &cancel,
        emit: &mut emit,
        deadline: None,
    };
    assert_eq!(backend.preflight().unwrap(), None);
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(bin.join("npm"), fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(backend.preflight().unwrap().unwrap()["runner"], "npm");
}

#[cfg(unix)]
#[test]
fn each_real_step_timeout_stops_the_lane_and_owned_descendants() {
    let (temp, repo) = fixture();
    struct Wedged<'a> {
        step: Step,
        cancel: &'a AtomicUsize,
        child_file: PathBuf,
    }
    impl Backend for Wedged<'_> {
        fn preflight(&mut self) -> io::Result<Option<Value>> {
            Ok(None)
        }
        fn step(
            &mut self,
            step: Step,
            cwd: &Path,
            environment: &Environment,
        ) -> io::Result<StepOutput> {
            if step != self.step {
                return Ok(output(
                    0,
                    if step == Step::Frontend {
                        "# tests 1\n"
                    } else {
                        "test result: ok. 1 passed;\n"
                    },
                ));
            }
            let mut environment = environment.clone();
            environment.insert(
                "CHILD_FILE".into(),
                self.child_file.to_string_lossy().into_owned(),
            );
            environment.insert("RSI_DESKTOP_OWNED_FIXTURE".into(), "1".into());
            run_step(
                &[
                    std::env::current_exe()?.to_string_lossy().into_owned(),
                    "--exact".into(),
                    "desktop_execution::tests::owned_process_fixture".into(),
                    "--nocapture".into(),
                ],
                cwd,
                &environment,
                step,
                self.cancel,
                |_| {},
            )
        }
    }
    let mut environment = env();
    for key in [
        "DESKTOP_LANE_NPM_CI_TIMEOUT_SECONDS",
        "DESKTOP_LANE_NPM_TEST_TIMEOUT_SECONDS",
        "DESKTOP_LANE_CARGO_TEST_TIMEOUT_SECONDS",
    ] {
        environment.insert(key.into(), "1".into());
    }
    let cancel = AtomicUsize::new(0);
    for step in [Step::Install, Step::Frontend, Step::Cargo] {
        let mut backend = Wedged {
            step,
            cancel: &cancel,
            child_file: temp.path().join(format!("{step:?}.pid")),
        };
        let start = Instant::now();
        let (code, result) =
            run_lane_with_cancel(&repo, &environment, temp.path(), &cancel, &mut backend).unwrap();
        assert_eq!(code, 1);
        assert_eq!(result["status"], "desktop_lane_timed_out");
        assert_eq!(result["failed_step"], step.name());
        assert!(start.elapsed() < Duration::from_secs(4));
        let pid = fs::read_to_string(&backend.child_file)
            .unwrap()
            .parse::<i32>()
            .unwrap();
        // SAFETY: signal 0 only observes the exact child PID this fixture made.
        assert_eq!(
            unsafe { libc::kill(pid, 0) },
            -1,
            "owned child {pid} survived {step:?}"
        );
    }
}

/// Invoked only in a child test harness by the bounded acceptance above. Its
/// signal dispositions never affect the daemon's parent test process.
#[cfg(unix)]
#[test]
fn owned_process_fixture() {
    if std::env::var("RSI_DESKTOP_OWNED_FIXTURE").ok().as_deref() != Some("1") {
        return;
    }
    use std::os::unix::process::CommandExt;
    // SAFETY: this isolated child ignores TERM and auto-reaps its descendants.
    unsafe {
        libc::signal(libc::SIGTERM, libc::SIG_IGN);
        libc::signal(libc::SIGCHLD, libc::SIG_IGN);
    }
    let mut command = Command::new("/bin/sleep");
    command.arg("30");
    // SAFETY: only async-signal-safe libc signal runs between fork and exec.
    unsafe {
        command.pre_exec(|| {
            libc::signal(libc::SIGTERM, libc::SIG_DFL);
            Ok(())
        });
    }
    let child = command.spawn().unwrap();
    fs::write(
        std::env::var_os("CHILD_FILE").unwrap(),
        child.id().to_string(),
    )
    .unwrap();
    loop {
        // SAFETY: wait for a signal; the owned supervisor eventually kills us.
        unsafe {
            libc::pause();
        }
    }
}

#[test]
fn parent_cancellation_interrupts_copy_at_chunk_boundaries() {
    struct CancelAfterWrite<'a> {
        bytes: Vec<u8>,
        cancel: &'a AtomicUsize,
    }
    impl Write for CancelAfterWrite<'_> {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.bytes.extend_from_slice(bytes);
            self.cancel.store(15, Ordering::Relaxed);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let temp = external_temp();
    let source = temp.path().join("large-include.bin");
    let content = vec![0x5a; 3 * 64 * 1024];
    fs::write(&source, &content).unwrap();
    let cancel = AtomicUsize::new(0);
    let mut writer = CancelAfterWrite {
        bytes: Vec::new(),
        cancel: &cancel,
    };
    let error =
        copy_with_cancel(&mut fs::File::open(source).unwrap(), &mut writer, &cancel).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    assert_eq!(writer.bytes, content[..64 * 1024]);

    let (temp, repo) = fixture();
    let stage = temp.path().join("cancelled-stage");
    fs::create_dir(&stage).unwrap();
    let error = stage_tree_with_cancel(&repo, &stage, &["desktop/package.json".into()], &cancel)
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    assert_eq!(fs::read_dir(stage).unwrap().count(), 0);
}

#[test]
fn lane_refuses_sandbox_scratch_and_cleans_after_preflight_cancellation() {
    let (temp, repo) = fixture();
    let mut fake = Fake::passing();
    let error = run_lane(&repo, &env(), &repo, &mut fake).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert_eq!(fake.calls.len(), 0);

    struct CancelPreflight<'a>(&'a AtomicUsize);
    impl Backend for CancelPreflight<'_> {
        fn preflight(&mut self) -> io::Result<Option<Value>> {
            self.0.store(15, Ordering::Relaxed);
            Ok(None)
        }
        fn step(&mut self, _: Step, _: &Path, _: &Environment) -> io::Result<StepOutput> {
            panic!("cancelled staging reached a test command")
        }
    }
    let cancel = AtomicUsize::new(0);
    let before = fs::read_dir(temp.path()).unwrap().count();
    let error = run_lane_with_cancel(
        &repo,
        &env(),
        temp.path(),
        &cancel,
        &mut CancelPreflight(&cancel),
    )
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), before);
}

#[cfg(unix)]
#[test]
fn real_workspace_and_locked_fixture_execute_from_the_private_copy() {
    // No registries or external dependencies: actual npm ci/test and locked
    // Cargo run on a tiny workspace, under the daemon job's existing build cap.
    // This fixture has no Tauri/WebKit dependency; product preflight remains in
    // ProcessBackend and is independently tested above.
    struct WorkspaceBackend<'a>(ProcessBackend<'a>);
    impl Backend for WorkspaceBackend<'_> {
        fn preflight(&mut self) -> io::Result<Option<Value>> {
            Ok(runner_status(
                "workspace_fixture",
                |name| executable(name, self.0.environment, self.0.repo).is_some(),
                || unreachable!(),
            ))
        }
        fn step(&mut self, step: Step, cwd: &Path, env: &Environment) -> io::Result<StepOutput> {
            self.0.step(step, cwd, env)
        }
    }
    let build_env = process::build_env(&crate::Ctx::from_process());
    for stale in [false, true] {
        let (temp, repo) = fixture();
        write(
            &repo,
            "Cargo.toml",
            "[workspace]\nmembers=['crates/fixture-helper','desktop/src-tauri']\nresolver='2'\n[workspace.package]\nversion='0.1.0'\nedition='2021'\n[workspace.dependencies]\nfixture-helper={path='crates/fixture-helper'}\n",
        );
        write(
            &repo,
            ".cargo/config.toml",
            "[env]\nDESKTOP_FIXTURE_ROOT={value='from-staged-root',force=true}\n",
        );
        fs::copy(
            repo_root().join("rust-toolchain.toml"),
            repo.join("rust-toolchain.toml"),
        )
        .unwrap();
        write(&repo, "shared.bin", "bits");
        write(
            &repo,
            "crates/fixture-helper/Cargo.toml",
            "[package]\nname='fixture-helper'\nversion.workspace=true\nedition.workspace=true\n",
        );
        write(
            &repo,
            "crates/fixture-helper/src/lib.rs",
            "pub fn length()->usize { include_bytes!(\"../../../shared.bin\").len() }\n",
        );
        write(
            &repo,
            "desktop/src-tauri/Cargo.toml",
            "[package]\nname='rsi-desktop-fixture'\nversion.workspace=true\nedition.workspace=true\n[dependencies]\nfixture-helper.workspace=true\n",
        );
        write(
            &repo,
            "desktop/src-tauri/src/lib.rs",
            "#[test] fn inherited_workspace_and_root_include() { assert_eq!(fixture_helper::length(),4); assert_eq!(env!(\"DESKTOP_FIXTURE_ROOT\"),\"from-staged-root\"); }\n",
        );
        let dependency = if stale {
            ""
        } else {
            "dependencies = [\"fixture-helper\"]\n"
        };
        write(
            &repo,
            "Cargo.lock",
            &format!(
                "version = 4\n\n[[package]]\nname = \"fixture-helper\"\nversion = \"0.1.0\"\n\n[[package]]\nname = \"rsi-desktop-fixture\"\nversion = \"0.1.0\"\n{dependency}"
            ),
        );
        write(
            &repo,
            "desktop/package.json",
            r#"{"name":"rsi-desktop-fixture","version":"0.1.0","scripts":{"test":"node --test fixture.test.cjs"}}"#,
        );
        write(
            &repo,
            "desktop/package-lock.json",
            r#"{"name":"rsi-desktop-fixture","version":"0.1.0","lockfileVersion":3,"requires":true,"packages":{"":{"name":"rsi-desktop-fixture","version":"0.1.0"}}}"#,
        );
        write(
            &repo,
            "desktop/fixture.test.cjs",
            "const test = require('node:test'); const assert = require('node:assert/strict'); const fs = require('node:fs'); test('outside asset in private copy', () => assert.equal(fs.readFileSync('../shared.bin').length,4));\n",
        );
        let lock_before = fs::read(repo.join("Cargo.lock")).unwrap();
        let mut environment = build_env.clone();
        environment.insert("CARGO_NET_OFFLINE".into(), "true".into());
        environment.insert("npm_config_offline".into(), "true".into());
        for key in [
            "DESKTOP_LANE_NPM_CI_TIMEOUT_SECONDS",
            "DESKTOP_LANE_NPM_TEST_TIMEOUT_SECONDS",
            "DESKTOP_LANE_CARGO_TEST_TIMEOUT_SECONDS",
        ] {
            environment.insert(key.into(), "120".into());
        }
        let cancel = AtomicUsize::new(0);
        let isolation = Isolation::unavailable("real_workspace_fixture_host_supervision");
        let mut log = String::new();
        let mut emit = |line: &str| log.push_str(line);
        let backend = ProcessBackend {
            environment: &environment,
            repo: &repo,
            isolation: &isolation,
            cancel: &cancel,
            emit: &mut emit,
            deadline: None,
        };
        let mut backend = WorkspaceBackend(backend);
        let (code, result) =
            run_lane_with_cancel(&repo, &environment, temp.path(), &cancel, &mut backend).unwrap();
        assert_eq!(result["npm_tests"], 1, "{result}\n{log}");
        if stale {
            assert_eq!(code, 1, "{result}\n{log}");
            assert_eq!(result["status"], "desktop_lock_stale", "{result}\n{log}");
            assert_eq!(result["reason"], LOCK_FIX);
            assert_eq!(result["failed_step"], "cargo test");
        } else {
            assert_eq!(code, 0, "{result}\n{log}");
            assert_eq!(result["status"], "passed", "{result}\n{log}");
            assert_eq!(result["cargo_tests"], 1, "{result}\n{log}");
            assert_eq!(result["executed_tests"], 2);
        }
        assert_eq!(fs::read(repo.join("Cargo.lock")).unwrap(), lock_before);
        assert_eq!(
            fs::read_dir(temp.path()).unwrap().count(),
            1,
            "private stage should be cleaned"
        );
    }
}

#[test]
fn only_the_documented_lane_inputs_are_forwarded_to_the_lane() {
    let mut environment = Environment::new();
    let offered: BTreeMap<&str, &str> = [
        ("DESKTOP_LANE_NPM_CI_TIMEOUT_SECONDS", "11"),
        ("DESKTOP_LANE_NPM_TEST_TIMEOUT_SECONDS", "12"),
        ("DESKTOP_LANE_CARGO_TEST_TIMEOUT_SECONDS", "13"),
        ("DESKTOP_LANE_NPM_CACHE", "/trusted/npm"),
        ("DESKTOP_LANE_CARGO_TARGET", "/trusted/cargo"),
        ("DESKTOP_LANE_UNKNOWN", "ignored"),
        ("RSI_SESSION_TOKEN", "never-forwarded"),
    ]
    .into();
    forward_lane_inputs(|name| offered.get(name).copied(), &mut environment);
    assert_eq!(environment.len(), 5);
    assert_eq!(environment["DESKTOP_LANE_NPM_CACHE"], "/trusted/npm");
    assert_eq!(environment["DESKTOP_LANE_CARGO_TARGET"], "/trusted/cargo");
    assert!(!environment.contains_key("RSI_SESSION_TOKEN"));
    let env = lane_env(&environment, Path::new("/scratch"));
    assert_eq!(env["npm_config_cache"], "/trusted/npm");
    assert_eq!(env["CARGO_TARGET_DIR"], "/trusted/cargo");
}

#[test]
fn receipt_entries_keep_the_legacy_fields_and_only_a_zero_exit_passed_lane_is_green() {
    let entry = |status: &str, code: i32| {
        receipt_entry(
            json!({"lane":"desktop","status":status,"executed_tests":2}),
            code,
        )
    };
    let table = [
        ("passed", 0, "passed", true),
        ("passed", 1, "failed", true),
        ("failed", 1, "failed", true),
        ("desktop_lock_stale", 1, "failed", true),
        ("output_incomplete", 1, "failed", true),
        ("desktop_lane_timed_out", 1, "timed_out", true),
        ("no_tests_executed", 1, "no_tests_executed", true),
        ("runner_unavailable", 1, "runner_unavailable", false),
        ("cancelled", 143, "failed", false),
    ];
    for (lane_status, code, status, completed) in table {
        let entry = entry(lane_status, code);
        assert_eq!(entry["status"], status, "{lane_status}");
        assert_eq!(entry["lane_status"], lane_status);
        assert_eq!(entry["completed"], completed, "{lane_status}");
        assert_eq!(entry["executed_tests"], 2);
    }
}

#[cfg(unix)]
#[test]
fn the_run_budget_shortens_a_step_and_a_zero_budget_never_starts_one() {
    let temp = tempfile::tempdir().unwrap();
    let env = Environment::from([("PATH".to_owned(), std::env::var("PATH").unwrap())]);
    let cancel = AtomicUsize::new(0);
    let argv = vec!["sh".to_owned(), "-c".into(), "sleep 30".into()];
    let started = Instant::now();
    let done = run_step_until(
        &argv,
        temp.path(),
        &env,
        Step::Install,
        &cancel,
        Some(Instant::now() + Duration::from_millis(300)),
        |_| {},
    )
    .unwrap();
    assert!(done.timed_out);
    assert!(started.elapsed() < Duration::from_secs(20));
    // Without a run limit the step's own (900 s default) bound still applies.
    let quick = run_step_until(
        &["sh".to_owned(), "-c".into(), "echo done".into()],
        temp.path(),
        &env,
        Step::Install,
        &cancel,
        None,
        |_| {},
    )
    .unwrap();
    assert_eq!(
        (quick.code, quick.timed_out, quick.output.as_str()),
        (0, false, "done\n")
    );
}
