//! Repository static guards (#360, umbrella #358: no Python in the repo).
//!
//! One crate, one binary (`rsi-repo-guard`), one subcommand per guard. The git
//! hooks, `scripts/scoped-test`, the `scripts/check-*.sh` wrappers and CI call it
//! through the shell wrapper `scripts/repo-guard`, which builds this crate on
//! demand with the caller's own Cargo target (the stamped `CARGO_TARGET_DIR` and
//! `RUSTC_WRAPPER`, never an ad-hoc one) and fails closed when the build fails.
//! See `README.md` beside this file for the pattern later slices follow.
//!
//! Each guard is a module with `pub fn main(args, ctx, out, err) -> i32`:
//! `args` excludes the guard name, [`Ctx`] carries the working directory and
//! the environment the guard may read (tests pass their own, so no test writes
//! the process environment), and the guard prints to `out`/`err` exactly what
//! the Python guard it replaced printed, with the same exit codes. The module's
//! tests are the ported Python test cases. [`py`] holds the small pieces of
//! Python semantics the ports rely on (line splitting, `repr`, path display).
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

pub mod agents_md_size;
pub mod bash32_arrays;
pub mod bounded_run;
pub mod cargo_slot;
pub mod cli;
pub mod desktop_execution;
pub mod desktop_gate_cli;
pub mod desktop_selection;
pub mod export_includes;
pub mod fixture_git_py;
#[cfg(test)]
mod fixture_git_py_bypass_tests;
pub mod guard_regressions;
pub mod identity_assertions;
pub mod landing_guard;
pub mod landing_guard_cli;

pub mod migration_declaration;
pub mod migration_renumber;
pub mod new_python;
pub mod operator_identifiers;
pub mod os_escape;
pub mod personal_paths;
pub mod pid_init;
pub mod pid_isolation;
pub mod portable_shell;
pub mod py;
pub mod qa_cache;
pub mod qa_fingerprint;
pub mod re;
pub mod release_guard;
pub mod release_target;
pub mod residual_lane_cli;
pub mod rolling_qa;
pub mod rolling_qa_cli;
pub mod rsid_health_probe;
pub mod rsid_quiet_restart;

pub mod scoped_runner;
pub mod scratch_git;
pub mod tar_extract;

pub mod recent_landing;
pub mod released_migrations;
pub mod rsid_test_shards;
pub mod rust_source;
pub mod scoped_test_failure;
pub mod settings_preflight;
pub mod source_stamp;
pub mod starter_pins;
pub mod store_boundary;
pub mod target_pool;
pub mod target_pool_build;
pub mod target_pool_cli;
pub mod target_pool_ops;
pub mod target_reclaim;
pub mod test_env_writes;
pub mod theme_test_guards;
pub mod touched_shards;
pub mod wire_json;

#[cfg(all(test, unix))]
mod target_pool_case_map;

#[cfg(all(test, unix))]
#[path = "ported_suites_tests.rs"]
mod ported_suites_tests;

/// A guard entry point: the arguments after the guard name, the context, stdout
/// and stderr; returns the process exit code.
pub type GuardMain = fn(&[String], &Ctx, &mut dyn Write, &mut dyn Write) -> i32;

/// Every guard: subcommand name, entry point, and the module source a caller
/// checks for before running it (a tree that predates a guard skips it, the
/// way callers used to check for the guard's `.py` file).
pub const GUARDS: &[(&str, GuardMain, &str)] = &[
    (
        "residual-lane",
        residual_lane_cli::main,
        "crates/rsi-repo-guards/src/residual_lane_cli.rs",
    ),
    (
        "new-python",
        new_python::main,
        "crates/rsi-repo-guards/src/new_python.rs",
    ),
    (
        "run-rolling-qa",
        rolling_qa_cli::main,
        "crates/rsi-repo-guards/src/rolling_qa_cli.rs",
    ),
    (
        "desktop-test-gate",
        desktop_gate_cli::main,
        "crates/rsi-repo-guards/src/desktop_gate_cli.rs",
    ),
    (
        "rolling-landing-guard",
        landing_guard_cli::main,
        "crates/rsi-repo-guards/src/landing_guard_cli.rs",
    ),
    (
        "check-touched-shards",
        touched_shards::main,
        "crates/rsi-repo-guards/src/touched_shards/mod.rs",
    ),
    (
        "rolling-shard-fingerprint",
        qa_fingerprint::main,
        "crates/rsi-repo-guards/src/qa_fingerprint.rs",
    ),
    (
        "import-rolling-qa-cache",
        qa_cache::main,
        "crates/rsi-repo-guards/src/qa_cache.rs",
    ),
    (
        "pid-isolate",
        pid_isolation::main,
        "crates/rsi-repo-guards/src/pid_isolation.rs",
    ),
    (
        "cargo-slot",
        cargo_slot::main,
        "crates/rsi-repo-guards/src/cargo_slot.rs",
    ),
    (
        "guard-regressions",
        guard_regressions::main,
        "crates/rsi-repo-guards/src/guard_regressions.rs",
    ),
    (
        "rsid-test-shards",
        rsid_test_shards::main,
        "crates/rsi-repo-guards/src/rsid_test_shards/mod.rs",
    ),
    (
        "recent-landing-check",
        recent_landing::main,
        "crates/rsi-repo-guards/src/recent_landing.rs",
    ),
    (
        "target-reclaim",
        target_reclaim::main,
        "crates/rsi-repo-guards/src/target_reclaim.rs",
    ),
    (
        "store-boundary",
        store_boundary::main,
        "crates/rsi-repo-guards/src/store_boundary.rs",
    ),
    (
        "scoped-test-failure",
        scoped_test_failure::main,
        "crates/rsi-repo-guards/src/scoped_test_failure.rs",
    ),
    (
        "rolling-migration-renumber",
        migration_renumber::main,
        "crates/rsi-repo-guards/src/migration_renumber.rs",
    ),
    (
        "refresh-migration-declaration-digests",
        migration_declaration::main,
        "crates/rsi-repo-guards/src/migration_declaration.rs",
    ),
    (
        "released-migrations",
        released_migrations::main,
        "crates/rsi-repo-guards/src/released_migrations.rs",
    ),
    (
        "agents-md-size",
        agents_md_size::main,
        "crates/rsi-repo-guards/src/agents_md_size.rs",
    ),
    (
        "bash32-arrays",
        bash32_arrays::main,
        "crates/rsi-repo-guards/src/bash32_arrays.rs",
    ),
    (
        "export-includes",
        export_includes::main,
        "crates/rsi-repo-guards/src/export_includes.rs",
    ),
    (
        "identity-assertions",
        identity_assertions::main,
        "crates/rsi-repo-guards/src/identity_assertions.rs",
    ),
    (
        "operator-identifiers",
        operator_identifiers::main,
        "crates/rsi-repo-guards/src/operator_identifiers.rs",
    ),
    (
        "personal-paths",
        personal_paths::main,
        "crates/rsi-repo-guards/src/personal_paths.rs",
    ),
    (
        "portable-shell",
        portable_shell::main,
        "crates/rsi-repo-guards/src/portable_shell.rs",
    ),
    (
        "release-guard",
        release_guard::main,
        "crates/rsi-repo-guards/src/release_guard.rs",
    ),
    (
        "release-target",
        release_target::main,
        "crates/rsi-repo-guards/src/release_target.rs",
    ),
    (
        "rsid-health-probe",
        rsid_health_probe::main,
        "crates/rsi-repo-guards/src/rsid_health_probe.rs",
    ),
    (
        "rsid-quiet-restart",
        rsid_quiet_restart::main,
        "crates/rsi-repo-guards/src/rsid_quiet_restart.rs",
    ),
    (
        "settings-preflight",
        settings_preflight::main,
        "crates/rsi-repo-guards/src/settings_preflight.rs",
    ),
    (
        "starter-pins",
        starter_pins::main,
        "crates/rsi-repo-guards/src/starter_pins.rs",
    ),
    (
        "target-pool",
        target_pool::main,
        "crates/rsi-repo-guards/src/target_pool.rs",
    ),
    (
        "test-env-writes",
        test_env_writes::main,
        "crates/rsi-repo-guards/src/test_env_writes.rs",
    ),
    (
        "theme-test-guards",
        theme_test_guards::main,
        "crates/rsi-repo-guards/src/theme_test_guards.rs",
    ),
];

/// What a guard may read from its surroundings.
#[derive(Debug, Clone)]
pub struct Ctx {
    /// The directory the guard runs in (Python's `os.getcwd()`).
    pub cwd: PathBuf,
    vars: BTreeMap<String, String>,
}

impl Ctx {
    /// The real process: its working directory and environment.
    #[must_use]
    pub fn from_process() -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let vars = std::env::vars_os()
            .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
            .collect();
        Self { cwd, vars }
    }

    /// A context rooted at `cwd` with only the given variables (tests).
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>, vars: &[(&str, &str)]) -> Self {
        Self {
            cwd: cwd.into(),
            vars: vars
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        }
    }

    /// An environment variable, as the guard sees it.
    #[must_use]
    pub fn var(&self, key: &str) -> Option<&str> {
        self.vars.get(key).map(String::as_str)
    }

    /// `path` resolved against the working directory (an absolute path as is).
    #[must_use]
    pub fn path(&self, path: impl AsRef<Path>) -> PathBuf {
        self.cwd.join(path)
    }

    /// Every variable the guard may read.
    pub fn vars(&self) -> impl Iterator<Item = (&str, &str)> {
        self.vars.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// The user's home directory (Python's `Path.home()`).
    #[must_use]
    pub fn home(&self) -> PathBuf {
        self.var("HOME")
            .or_else(|| self.var("USERPROFILE"))
            .map_or_else(|| PathBuf::from("~"), PathBuf::from)
    }

    /// A `git` command that runs in the working directory.
    #[must_use]
    pub fn git(&self) -> Command {
        let mut command = Command::new("git");
        command.current_dir(&self.cwd);
        command
    }
}

/// Run `rsi-repo-guard <guard> [args...]`; returns the exit code.
pub fn dispatch(args: &[String], ctx: &Ctx, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let names: Vec<&str> = GUARDS.iter().map(|(name, _, _)| *name).collect();
    let Some(first) = args.first() else {
        let _ = writeln!(
            err,
            "usage: rsi-repo-guard <guard> [args...]   guards: {}",
            names.join(", ")
        );
        return 2;
    };
    match first.as_str() {
        "--list" => {
            for (name, _, source) in GUARDS {
                let _ = writeln!(out, "{name}\t{source}");
            }
            0
        }
        "-h" | "--help" => {
            let _ = writeln!(
                out,
                "usage: rsi-repo-guard <guard> [args...]   guards: {}",
                names.join(", ")
            );
            0
        }
        name => match GUARDS.iter().find(|(guard, _, _)| *guard == name) {
            Some((_, run, _)) => run(&args[1..], ctx, out, err),
            None => {
                let _ = writeln!(
                    err,
                    "rsi-repo-guard: unknown guard '{name}'; known guards: {}",
                    names.join(", ")
                );
                2
            }
        },
    }
}

#[cfg(test)]
pub(crate) mod testutil {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use super::Ctx;

    /// A scratch directory under `/tmp`: a Unix socket path must fit `sun_path` (104 bytes on macOS),
    /// which the long per-user `TMPDIR` there does not.
    pub fn short_tempdir() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("rg")
            .tempdir_in("/tmp")
            .unwrap()
    }

    /// Run a guard entry point and return (exit code, stdout, stderr).
    pub fn run(main: super::GuardMain, args: &[&str], ctx: &Ctx) -> (i32, String, String) {
        let args: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = main(&args, ctx, &mut out, &mut err);
        (
            code,
            String::from_utf8_lossy(&out).into_owned(),
            String::from_utf8_lossy(&err).into_owned(),
        )
    }

    /// Write `text` to `root/rel`, creating parent directories.
    pub fn write(root: &Path, rel: &str, text: &str) -> PathBuf {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, text).unwrap();
        path
    }

    /// Run git in `dir` isolated from the user's and system configuration; panics on failure.
    pub fn git(dir: &Path, args: &[&str]) -> String {
        let done = git_status(dir, args);
        assert!(
            done.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&done.stderr)
        );
        String::from_utf8_lossy(&done.stdout).into_owned()
    }

    /// Run git in `dir` (isolated configuration) and return its output whatever the status.
    pub fn git_status(dir: &Path, args: &[&str]) -> std::process::Output {
        let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_OBJECT_DIRECTORY")
            .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
            .env("GIT_CONFIG_GLOBAL", null)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.invalid",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args([
                "-c",
                "init.defaultBranch=main",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap()
    }

    /// Git fixtures must be outside the checkout, including its stamped TMPDIR.
    pub fn scratch_repository() -> tempfile::TempDir {
        let root = repo_root();
        for candidate in [
            PathBuf::from("/var/tmp"),
            PathBuf::from("/tmp"),
            std::env::temp_dir(),
        ] {
            let Ok(candidate) = candidate.canonicalize() else {
                continue;
            };
            if candidate.starts_with(&root) {
                continue;
            }
            if let Ok(dir) = tempfile::Builder::new()
                .prefix("rsi-guard-git-")
                .tempdir_in(candidate)
            {
                return dir;
            }
        }
        panic!("no disk-backed scratch outside the checkout for Git fixture");
    }

    /// Git trees and commits built with plumbing and no worktree, so a path may
    /// be any bytes (no filesystem here can hold invalid UTF-8; Git objects can).
    #[cfg(unix)]
    pub mod raw {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        use std::path::Path;
        use std::process::Command;

        fn plumb(root: &Path, args: Vec<OsString>) -> String {
            let done = Command::new("git")
                .arg("-C")
                .arg(root)
                .env("GIT_INDEX_FILE", root.join(".git").join("raw-index"))
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
                .args(args)
                .output()
                .unwrap();
            assert!(
                done.status.success(),
                "{}",
                String::from_utf8_lossy(&done.stderr)
            );
            String::from_utf8_lossy(&done.stdout).trim().to_string()
        }

        /// The tree holding exactly `files` (path bytes, text).
        pub fn tree(root: &Path, files: &[(&[u8], &str)]) -> String {
            let _ = std::fs::remove_file(root.join(".git").join("raw-index"));
            for (path, text) in files {
                let scratch = root.join(".git").join("raw-blob");
                std::fs::write(&scratch, text).unwrap();
                let blob = plumb(
                    root,
                    vec!["hash-object".into(), "-w".into(), scratch.into_os_string()],
                );
                let mut spec = format!("100644,{blob},").into_bytes();
                spec.extend_from_slice(path);
                plumb(
                    root,
                    vec![
                        "update-index".into(),
                        "--add".into(),
                        "--cacheinfo".into(),
                        OsString::from_vec(spec),
                    ],
                );
            }
            plumb(root, vec!["write-tree".into()])
        }

        pub fn commit(root: &Path, tree: &str, parents: &[&str]) -> String {
            let mut args: Vec<OsString> = vec!["commit-tree".into(), tree.into()];
            for parent in parents {
                args.extend(["-p".into(), (*parent).into()]);
            }
            args.extend(["-m".into(), "raw".into()]);
            plumb(root, args)
        }
    }

    /// The repository root (two levels above this crate's manifest).
    pub fn repo_root() -> PathBuf {
        Path::new(rsi_common::manifest_dir!())
            .join("../..")
            .canonicalize()
            .unwrap()
    }
}
