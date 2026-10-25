//! One named Rust test for each of the 29 legacy scratch safety tests, plus
//! attached-option regression cases. Every mutating Git command is prepared by
//! the same validated API and only targets owned external fixture directories.
use super::*;
use std::process::Output;

struct Fixture {
    _temp: tempfile::TempDir,
    base: PathBuf,
    assigned: PathBuf,
    plain: Policy,
    environment: Environment,
}
impl Fixture {
    fn new() -> Self {
        Self::with_temp_parent(&std::env::temp_dir(), std::env::vars_os().collect())
    }
    fn with_temp_parent(temp_parent: &Path, environment: Environment) -> Self {
        let checkout = crate::testutil::repo_root();
        let mut environment: Environment = environment
            .into_iter()
            .filter(|(k, _)| {
                !LOCATION_VARIABLES.iter().any(|key| k == OsStr::new(key))
                    && k != "RSI_SESSION_TOKEN"
            })
            .collect();
        let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
        environment.insert("GIT_CONFIG_GLOBAL".into(), null.into());
        environment.insert("GIT_CONFIG_NOSYSTEM".into(), "1".into());
        // Keep test hooks and user signing out of every disposable repository.
        environment.insert("GIT_CONFIG_COUNT".into(), "4".into());
        for (i, (key, value)) in [
            ("core.hooksPath", null),
            ("commit.gpgsign", "false"),
            ("user.name", "scratch-fixture"),
            ("user.email", "fixture@example.invalid"),
        ]
        .into_iter()
        .enumerate()
        {
            environment.insert(format!("GIT_CONFIG_KEY_{i}").into(), key.into());
            environment.insert(format!("GIT_CONFIG_VALUE_{i}").into(), value.into());
        }
        // These tests deliberately remove Git ceilings. Discover the repository
        // enclosing the temp parent with that same environment before allocating:
        // a job TMPDIR may be under the home repository (#3888).
        let host = Policy::new(&checkout, &std::env::temp_dir(), environment.clone(), &[]).unwrap();
        let policy = Policy::new(
            &checkout,
            temp_parent,
            environment.clone(),
            &host.protected_roots().unwrap(),
        )
        .unwrap();
        let temp = policy.tempdir("rsi-scratch-git-test-").unwrap();
        let base = temp.path().canonicalize().unwrap();
        let assigned = base.join("assigned");
        fs::create_dir(&assigned).unwrap();
        let plain = Policy::new(&checkout, &checkout, environment.clone(), &[]).unwrap();
        let f = Self {
            _temp: temp,
            base,
            assigned,
            plain,
            environment,
        };
        f.git(&f.assigned, &["init", "-q", "-b", "assigned-branch"]);
        fs::write(f.assigned.join("tracked.txt"), "base\n").unwrap();
        f.git(&f.assigned, &["add", "tracked.txt"]);
        f.git(&f.assigned, &["commit", "-q", "-m", "base"]);
        f
    }
    fn policy(&self, cwd: &Path, environment: Environment, extra: &[PathBuf]) -> Policy {
        Policy::new(&self.plain.checkout, cwd, environment, extra).unwrap()
    }
    fn assigned_policy(&self) -> Policy {
        self.policy(&self.assigned, self.environment.clone(), &[])
    }
    fn scratch(&self, name: &str) -> PathBuf {
        let path = self.base.join(name);
        fs::create_dir_all(&path).unwrap();
        path
    }
    fn git(&self, directory: &Path, args: &[&str]) -> String {
        output(&self.plain, directory, args)
    }
    fn snapshot(&self) -> Vec<String> {
        [
            vec!["symbolic-ref", "HEAD"],
            vec!["rev-parse", "HEAD"],
            vec!["rev-parse", "HEAD^{tree}"],
            vec!["for-each-ref"],
            vec!["status", "--porcelain"],
            vec!["rev-list", "--all", "--count"],
        ]
        .iter()
        .map(|args| self.git(&self.assigned, args))
        .collect()
    }
    fn assert_enclosing_unchanged(&self, mut before: Vec<String>) {
        let mut after = self.snapshot();
        let status = after.remove(4);
        before.remove(4);
        assert_eq!(after, before);
        assert!(status.lines().all(|line| line.starts_with("?? ")));
    }
    fn job(&self) -> (PathBuf, PathBuf, Environment) {
        let job_tmp = self.assigned.join("jobs/job-1.tmp");
        let process_dir = job_tmp.join("process");
        fs::create_dir_all(process_dir.join("repo")).unwrap();
        let mut environment = self.environment.clone();
        environment.insert("TMPDIR".into(), job_tmp.as_os_str().to_owned());
        (job_tmp, process_dir, environment)
    }
}
fn run(policy: &Policy, directory: &Path, args: &[&str]) -> Output {
    policy.command(directory, args).unwrap().output().unwrap()
}
fn output(policy: &Policy, directory: &Path, args: &[&str]) -> String {
    let done = run(policy, directory, args);
    assert!(
        done.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&done.stderr)
    );
    String::from_utf8(done.stdout).unwrap().trim().to_owned()
}
fn ceiling(environment: &Environment, paths: &[&Path]) -> Environment {
    let mut env = environment.clone();
    env.insert(
        "GIT_CEILING_DIRECTORIES".into(),
        std::env::join_paths(paths).unwrap(),
    );
    env
}
fn assert_denied(policy: &Policy, directory: &Path) {
    for args in [
        vec!["init", "-q"],
        vec!["commit", "--allow-empty", "-m", "junk"],
        vec!["reset", "--mixed", "HEAD"],
        vec!["update-ref", "refs/heads/stray", "HEAD"],
    ] {
        assert!(
            policy.command(directory, &args).is_err(),
            "unsafe command was prepared: {directory:?}"
        );
    }
}
#[cfg(unix)]
fn link(target: &Path, path: &Path) {
    std::os::unix::fs::symlink(target, path).unwrap();
}
#[cfg(windows)]
fn link(target: &Path, path: &Path) {
    std::os::windows::fs::symlink_dir(target, path).unwrap();
}

#[test]
fn missing_directory_fails_before_git_and_leaves_the_assigned_repo_unchanged() {
    let f = Fixture::new();
    let before = f.snapshot();
    assert_denied(&f.assigned_policy(), &f.base.join("never-created"));
    assert_eq!(f.snapshot(), before);
}
#[test]
fn unsafe_directory_arguments_are_rejected_without_starting_git() {
    let f = Fixture::new();
    let before = f.snapshot();
    let file = f.base.join("file");
    fs::write(&file, "x").unwrap();
    let root = f.base.ancestors().last().unwrap();
    for path in [
        Path::new(""),
        Path::new("."),
        Path::new("scratch"),
        Path::new("../assigned"),
        &file,
        root,
    ] {
        assert_denied(&f.assigned_policy(), path);
    }
    assert_eq!(f.snapshot(), before);
}
#[test]
fn the_old_unvalidated_cd_chain_is_what_this_prevents() {
    let f = Fixture::new();
    let before = f.snapshot();
    assert_denied(&f.assigned_policy(), &f.base.join("missing"));
    assert_eq!(f.snapshot(), before);
}
#[test]
fn directory_inside_the_current_worktree_is_rejected() {
    let f = Fixture::new();
    let inner = f.assigned.join("nested");
    fs::create_dir(&inner).unwrap();
    let before = f.snapshot();
    for target in [&f.assigned, &inner] {
        assert_denied(&f.assigned_policy(), target);
    }
    assert_eq!(f.snapshot(), before);
}
#[test]
fn symlink_into_the_assigned_repo_is_rejected() {
    let f = Fixture::new();
    let path = f.base.join("looks-like-scratch");
    link(&f.assigned, &path);
    let before = f.snapshot();
    assert_denied(&f.assigned_policy(), &path);
    assert_denied(
        &f.policy(&f.base, f.environment.clone(), &[f.assigned.clone()]),
        &path,
    );
    assert_eq!(f.snapshot(), before);
}
#[test]
fn symlink_to_a_real_scratch_directory_is_followed_and_resolved() {
    let f = Fixture::new();
    let scratch = f.scratch("scratch");
    let path = f.base.join("link");
    link(&scratch, &path);
    assert_eq!(
        f.assigned_policy().validate(&path).unwrap(),
        scratch.canonicalize().unwrap()
    );
    output(&f.assigned_policy(), &path, &["init", "-q"]);
    assert!(scratch.join(".git").is_dir());
}
#[test]
fn this_checkout_is_always_protected() {
    let f = Fixture::new();
    assert_denied(&f.plain, &f.plain.checkout);
    assert_denied(&f.plain, &f.plain.checkout.join("scripts"));
}
#[test]
fn location_override_paths_are_protected_roots() {
    let f = Fixture::new();
    let scratch = f.scratch("override/nested");
    for key in ["GIT_DIR", "GIT_WORK_TREE", "GIT_COMMON_DIR"] {
        let mut environment = f.environment.clone();
        environment.insert(key.into(), f.base.join("override").into_os_string());
        assert_denied(&f.policy(&f.base, environment, &[]), &scratch);
    }
}
#[test]
fn a_worktree_gitdir_pointer_is_protected() {
    let f = Fixture::new();
    let worktree = f.scratch("linked");
    let gitdir = f.scratch("shared-gitdir");
    // Relative pointer exercises resolution against the worktree, not process cwd.
    fs::write(worktree.join(".git"), "gitdir: ../shared-gitdir\n").unwrap();
    fs::create_dir(gitdir.join("worktrees")).unwrap();
    assert_denied(
        &f.policy(&worktree, f.environment.clone(), &[]),
        &gitdir.join("worktrees"),
    );
    // Permanent checkout gitdir stays protected even when cwd discovery is hidden.
    let policy = Policy::new(&worktree, &f.base, f.environment.clone(), &[]).unwrap();
    assert_denied(&policy, &gitdir);
}
#[test]
fn the_sandbox_tmpdir_is_rejected_before_git_and_nothing_changes() {
    let f = Fixture::new();
    let inner = f.assigned.join("target/.rsi-tmp/fixture");
    fs::create_dir_all(&inner).unwrap();
    let before = f.snapshot();
    assert_denied(&f.assigned_policy(), &inner);
    assert_denied(&f.assigned_policy(), inner.parent().unwrap());
    assert_eq!(f.snapshot(), before);
}
#[test]
fn fixtures_allocate_outside_the_sandbox_without_changing_tmpdir() {
    let f = Fixture::new();
    let inside = f.assigned.join("target/.rsi-tmp");
    fs::create_dir_all(&inside).unwrap();
    let mut env = f.environment.clone();
    env.insert("TMPDIR".into(), inside.clone().into_os_string());
    let policy = f.policy(&f.assigned, env.clone(), &[]);
    let before = f.snapshot();
    let temp = policy.tempdir("rsi-scratch-allocated-").unwrap();
    let path = temp.path().to_owned();
    assert!(policy.validate(&path).is_ok());
    output(&policy, &path, &["init", "-q"]);
    output(
        &policy,
        &path,
        &["commit", "-q", "--allow-empty", "-m", "ok"],
    );
    assert_eq!(output(&policy, &path, &["log", "-1", "--format=%s"]), "ok");
    assert_eq!(policy.environment, env);
    assert_eq!(f.snapshot(), before);
    drop(temp);
    assert_eq!(path.try_exists().unwrap(), false);
}
#[test]
fn no_allocation_when_every_candidate_is_inside_the_sandbox() {
    let f = Fixture::new();
    let inside = f.assigned.join("target/.rsi-tmp");
    fs::create_dir_all(&inside).unwrap();
    let policy = f.assigned_policy();
    assert!(policy.host_temp_root_from(&[inside.clone()]).is_err());
    assert!(policy.tempdir_in("scratch-", &inside).is_err());
}
#[test]
fn without_a_ceiling_the_enclosing_repository_is_still_protected() {
    let f = Fixture::new();
    let (_, cwd, env) = f.job();
    let before = f.snapshot();
    assert_denied(&f.policy(&cwd, env, &[]), &cwd.join("repo"));
    assert_eq!(f.snapshot(), before);
}
#[test]
fn a_job_ceiling_above_the_scratch_directory_makes_it_usable() {
    let f = Fixture::new();
    let (job, cwd, env) = f.job();
    let before = f.snapshot();
    let policy = f.policy(&cwd, ceiling(&env, &[&job]), &[]);
    let repo = cwd.join("repo");
    output(&policy, &repo, &["init", "-q", "-b", "main"]);
    output(
        &policy,
        &repo,
        &["commit", "-q", "--allow-empty", "-m", "job"],
    );
    assert_eq!(output(&policy, &repo, &["log", "-1", "--format=%s"]), "job");
    // Empty fixture directories become visible as untracked content when the
    // scratch repository is initialized. Refs and tracked state stay exact.
    f.assert_enclosing_unchanged(before);
}
#[test]
fn the_job_ceiling_also_stops_plain_git_reaching_the_enclosing_repository() {
    let f = Fixture::new();
    let (job, cwd, env) = f.job();
    // Intentionally plain READ, absolute pinned directory; no mutation here.
    let plain = |env: &Environment| {
        Command::new("git")
            .arg("-C")
            .arg(cwd.join("repo"))
            .args(["rev-parse", "--show-toplevel"])
            .env_clear()
            .envs(env)
            .output()
            .unwrap()
    };
    let done = plain(&ceiling(&env, &[&job]));
    assert!(!done.status.success());
    assert!(String::from_utf8_lossy(&done.stderr).contains("not a git repository"));
    let done = plain(&env);
    assert!(done.status.success());
    assert_eq!(
        Path::new(String::from_utf8(done.stdout).unwrap().trim())
            .canonicalize()
            .unwrap(),
        f.assigned
    );
}
#[test]
fn fixtures_allocate_in_the_job_tmpdir_under_the_ceiling() {
    let f = Fixture::new();
    let (job, cwd, env) = f.job();
    let before = f.snapshot();
    let policy = f.policy(&cwd, ceiling(&env, &[&job]), &[]);
    let temp = policy.tempdir("rsi-scratch-job-").unwrap();
    assert_eq!(
        temp.path().canonicalize().unwrap().parent().unwrap(),
        job.canonicalize().unwrap()
    );
    output(&policy, temp.path(), &["init", "-q"]);
    output(
        &policy,
        temp.path(),
        &["commit", "-q", "--allow-empty", "-m", "ok"],
    );
    drop(temp);
    assert_eq!(f.snapshot(), before);
}
#[test]
fn a_ceiling_does_not_unprotect_the_checkout_overrides_or_extra_roots() {
    let f = Fixture::new();
    let (job, cwd, env) = f.job();
    let env = ceiling(&env, &[&job]);
    let mut overridden = env.clone();
    overridden.insert("GIT_WORK_TREE".into(), f.assigned.as_os_str().to_owned());
    let policy = f.policy(&cwd, overridden, &[]);
    assert_denied(&policy, &cwd.join("repo"));
    assert_denied(&policy, &f.plain.checkout.join("scripts"));
    assert_denied(&f.policy(&cwd, env, &[cwd.clone()]), &cwd.join("repo"));
}
#[test]
fn a_ceiling_above_the_repository_hides_nothing() {
    let f = Fixture::new();
    let (_, cwd, env) = f.job();
    assert_denied(
        &f.policy(&cwd, ceiling(&env, &[&f.base]), &[]),
        &cwd.join("repo"),
    );
}
#[test]
fn a_ceiling_equal_to_the_current_directory_hides_nothing() {
    let f = Fixture::new();
    let (_, cwd, env) = f.job();
    assert_denied(
        &f.policy(&cwd, ceiling(&env, &[&cwd]), &[]),
        &cwd.join("repo"),
    );
}
#[test]
fn relative_and_empty_ceiling_entries_are_ignored_like_git_does() {
    let f = Fixture::new();
    let (job, cwd, env) = f.job();
    let env = ceiling(&env, &[Path::new(""), Path::new("relative"), &job, &f.base]);
    assert_eq!(
        ceiling_directories(&env, &cwd),
        vec![lexical(&job), lexical(&f.base)]
    );
    let env = ceiling(
        &f.environment,
        &[Path::new(""), Path::new("relative/job.tmp")],
    );
    assert_eq!(ceiling_directories(&env, &cwd), Vec::<PathBuf>::new());
    assert_denied(&f.policy(&cwd, env, &[]), &cwd.join("repo"));
}
#[test]
fn a_symlinked_ceiling_entry_is_resolved() {
    let f = Fixture::new();
    let (job, cwd, env) = f.job();
    let path = f.base.join("job-link");
    link(&job, &path);
    let policy = f.policy(&cwd, ceiling(&env, &[&path]), &[]);
    assert_eq!(
        policy.validate(&cwd.join("repo")).unwrap(),
        cwd.join("repo").canonicalize().unwrap()
    );
}
#[test]
fn entries_after_an_empty_entry_are_not_resolved_like_git() {
    let f = Fixture::new();
    let (job, cwd, env) = f.job();
    let path = f.base.join("job-link");
    link(&job, &path);
    let dotted = f.base.join("x/../job-link");
    let env = ceiling(&env, &[&path, Path::new(""), &dotted]);
    assert_eq!(
        ceiling_directories(&env, &cwd),
        vec![job.canonicalize().unwrap(), path]
    );
}
#[test]
fn a_symlinked_ceiling_after_an_empty_entry_keeps_the_caller_repository_protected() {
    let f = Fixture::new();
    let caller = f.scratch("caller");
    f.git(&caller, &["init", "-q"]);
    let cwd = caller.join("sub");
    fs::create_dir(&cwd).unwrap();
    let path = f.base.join("caller-link");
    link(&caller, &path);
    let env = ceiling(&f.environment, &[Path::new(""), &path]);
    let done = Command::new("git")
        .arg("-C")
        .arg(&cwd)
        .args(["rev-parse", "--show-toplevel"])
        .env_clear()
        .envs(&env)
        .output()
        .unwrap();
    assert!(done.status.success());
    assert_eq!(
        Path::new(String::from_utf8(done.stdout).unwrap().trim())
            .canonicalize()
            .unwrap(),
        caller.canonicalize().unwrap()
    );
    assert_denied(&f.policy(&cwd, env, &[]), &caller);
}
#[test]
fn location_environment_cannot_target_the_assigned_repo() {
    let f = Fixture::new();
    let scratch = f.scratch("scratch");
    let before = f.snapshot();
    let mut env = f.environment.clone();
    for key in LOCATION_VARIABLES {
        env.insert((*key).into(), f.assigned.join(".git").into_os_string());
    }
    env.insert("GIT_WORK_TREE".into(), f.assigned.as_os_str().to_owned());
    let policy = f.policy(&f.assigned, env, &[]);
    output(&policy, &scratch, &["init", "-q", "-b", "scratch-main"]);
    fs::write(scratch.join("f.txt"), "scratch\n").unwrap();
    output(&policy, &scratch, &["add", "f.txt"]);
    output(&policy, &scratch, &["commit", "-q", "-m", "scratch commit"]);
    output(&policy, &scratch, &["branch", "stray", "HEAD"]);
    assert_eq!(
        output(&policy, &scratch, &["symbolic-ref", "HEAD"]),
        "refs/heads/scratch-main"
    );
    assert_eq!(f.snapshot(), before);
}
#[test]
fn an_explicit_env_argument_is_cleaned_too() {
    let f = Fixture::new();
    let scratch = f.scratch("scratch");
    let before = f.snapshot();
    let mut env = f.environment.clone();
    env.insert("GIT_DIR".into(), f.assigned.join(".git").into_os_string());
    let policy = f.policy(&f.base, env, &[]);
    output(&policy, &scratch, &["init", "-q"]);
    assert!(scratch.join(".git").is_dir());
    assert_eq!(f.snapshot(), before);
    let clean = clean_environment(&scratch, &policy.environment).unwrap();
    for key in LOCATION_VARIABLES
        .iter()
        .filter(|key| **key != "GIT_CEILING_DIRECTORIES")
    {
        assert_eq!(clean.get(OsStr::new(key)), None);
    }
    assert_eq!(
        clean.get(OsStr::new("GIT_CEILING_DIRECTORIES")),
        Some(&scratch.parent().unwrap().as_os_str().to_owned())
    );
}
#[test]
fn explicit_env_fixture_works_with_a_job_tmpdir_inside_the_home_repository() {
    let home = Fixture::new();
    let (job, _, env) = home.job();
    let before = home.snapshot();
    let fixture = Fixture::with_temp_parent(&job, ceiling(&env, &[&job]));
    let policy = home.policy(&job, home.environment.clone(), &[]);
    assert!(policy.validate(&fixture.base).is_ok());
    let scratch = fixture.scratch("scratch");
    let mut env = fixture.environment.clone();
    env.insert(
        "GIT_DIR".into(),
        fixture.assigned.join(".git").into_os_string(),
    );
    let policy = fixture.policy(&fixture.base, env, &[]);
    output(&policy, &scratch, &["init", "-q"]);
    output(
        &policy,
        &scratch,
        &["commit", "-q", "--allow-empty", "-m", "job fixture"],
    );
    assert_eq!(
        output(&policy, &scratch, &["log", "-1", "--format=%s"]),
        "job fixture"
    );
    assert_eq!(home.snapshot(), before);
}
#[test]
fn override_options_before_the_subcommand_are_rejected() {
    let f = Fixture::new();
    let scratch = f.scratch("scratch");
    let before = f.snapshot();
    for args in [
        vec!["-C", "elsewhere", "status"],
        vec!["--git-dir=elsewhere", "status"],
        vec!["--work-tree", "elsewhere", "status"],
        vec!["-c", "core.x=1", "-C", "elsewhere", "status"],
        vec!["-Celsewhere", "status"],
        vec!["--namespace=other", "status"],
        vec!["--super-prefix=x", "status"],
        vec!["--config-env=core.x=VALUE", "-Celsewhere", "status"],
        vec!["--config-env", "core.x=VALUE", "--git-dir=x", "status"],
    ] {
        assert!(f.assigned_policy().command(&scratch, &args).is_err());
    }
    assert_eq!(f.snapshot(), before);
}
#[test]
fn a_subcommand_option_named_dash_c_is_not_an_override() {
    let f = Fixture::new();
    let scratch = f.scratch("scratch");
    let policy = f.assigned_policy();
    output(&policy, &scratch, &["init", "-q"]);
    output(
        &policy,
        &scratch,
        &["commit", "-q", "--allow-empty", "-m", "first"],
    );
    output(
        &policy,
        &scratch,
        &["commit", "-q", "--allow-empty", "-C", "HEAD"],
    );
    assert_eq!(
        output(&policy, &scratch, &["rev-list", "--count", "HEAD"]),
        "2"
    );
}
#[test]
fn a_scratch_directory_that_is_not_a_repo_does_not_find_an_enclosing_repo() {
    let f = Fixture::new();
    let outer = f.scratch("outer");
    f.git(&outer, &["init", "-q"]);
    let inner = outer.join("inner");
    fs::create_dir(&inner).unwrap();
    let before = f.git(&outer, &["status", "--porcelain=v2", "--branch"]);
    let done = run(
        &f.assigned_policy(),
        &inner,
        &["commit", "--allow-empty", "-m", "x"],
    );
    assert!(!done.status.success());
    assert!(String::from_utf8_lossy(&done.stderr).contains("not a git repository"));
    assert_eq!(
        f.git(&outer, &["status", "--porcelain=v2", "--branch"]),
        before
    );
    assert_eq!(f.git(&outer, &["rev-list", "--all", "--count"]), "0");
}
#[test]
fn valid_isolated_fixture_repo_supports_ordinary_git() {
    let f = Fixture::new();
    let scratch = f.scratch("scratch");
    let before = f.snapshot();
    let policy = f.assigned_policy();
    output(&policy, &scratch, &["init", "-q", "-b", "main"]);
    fs::write(scratch.join("a.txt"), "a\n").unwrap();
    output(&policy, &scratch, &["add", "a.txt"]);
    output(&policy, &scratch, &["commit", "-q", "-m", "first"]);
    output(&policy, &scratch, &["branch", "side"]);
    output(&policy, &scratch, &["checkout", "-q", "side"]);
    output(
        &policy,
        &scratch,
        &["commit", "-q", "--allow-empty", "-m", "second"],
    );
    output(&policy, &scratch, &["reset", "--mixed", "HEAD~1"]);
    assert_eq!(
        output(&policy, &scratch, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "side"
    );
    assert_eq!(output(&policy, &scratch, &["log", "--format=%s"]), "first");
    assert!(
        !run(&policy, &scratch, &["rev-parse", "--verify", "no-such-ref"])
            .status
            .success()
    );
    assert_eq!(f.snapshot(), before);
}
