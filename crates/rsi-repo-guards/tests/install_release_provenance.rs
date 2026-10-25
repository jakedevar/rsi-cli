//! #938: a release build never reuses another worktree's workspace crates; a build
//! job's receipt is the provenance of the binaries it links (#157, #282, #3513).
//!
//! Two git worktrees of one throwaway repository build into one shared
//! CARGO_TARGET_DIR, as a global `build.target-dir` makes them. The fake `cargo`
//! (tests/fixtures/install_release_fake_cargo.sh) models the hazard. No real
//! build, daemon or user file is touched (HOME is a temp directory and every
//! install runs with --no-restart). The shared fail-closed lifecycle boundary
//! also isolates plist refreshes, which --no-restart still performs (#632).
//!
//! Ported from scripts/tests/test_install_release_provenance.py (#613). The
//! receipt-target tests run the real `rsi-repo-guard release-target` guard: the
//! fixture's `scripts/repo-guard` is a thin stub that names the guard binary Cargo
//! built for this test crate, so the fixture needs neither the crate nor a build.
#![cfg(unix)]

mod install_release_support;

use install_release_support::isolation;
use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{Map, Value, json};

const FAKE_BINS: [&str; 11] = [
    "rsi",
    "rsid",
    "rsi-rpc",
    "rsi-agent-mcp",
    "rsi-build-rustc",
    "rsi-contract-validate",
    "rsi-known-failure",
    "rsi-rolling-land",
    "rsi-remote",
    "rsi-turn-shim",
    "rsi-socket-hold",
];
const MEMBERS: [&str; 2] = ["stub", "stub-tools"];
const FINGERPRINT: &str = ".fingerprint/stub-0123456789abcdef";
const FAKE_CARGO: &str = include_str!("fixtures/install_release_fake_cargo.sh");

type Edit = fn(&mut Map<String, Value>);

/// The outcome of one script run.
struct Run {
    code: i32,
    out: String,
    err: String,
}

impl Run {
    fn ok(&self) -> bool {
        self.code == 0
    }
}

struct Fixture {
    _temp: tempfile::TempDir,
    base: PathBuf,
    home: PathBuf,
    /// The registered release target (~/.rsi/staging/release-target): the one a
    /// receipt may name besides the caller's own cargo target (#3513).
    target: PathBuf,
    log: PathBuf,
    fakes: PathBuf,
    env: HashMap<String, String>,
    a: PathBuf,
    b: PathBuf,
}

fn text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn executable(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn is_empty_dir(path: &Path) -> bool {
    fs::read_dir(path).unwrap().next().is_none()
}

fn names(path: &Path) -> Vec<String> {
    let mut found: Vec<String> = fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    found.sort();
    found
}

/// The repository's scripts/ without the tests, which the fixture does not need.
fn copy_scripts(from: &Path, to: &Path, top: bool) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        let source = entry.path();
        let dest = to.join(&name);
        let kind = entry.file_type().unwrap();
        if kind.is_symlink() {
            symlink(fs::read_link(&source).unwrap(), &dest).unwrap();
        } else if kind.is_dir() {
            if (top && name == "tests") || name == "__pycache__" {
                continue;
            }
            copy_scripts(&source, &dest, false);
        } else {
            fs::copy(&source, &dest).unwrap();
        }
    }
}

fn no_edit(_: &mut Map<String, Value>) {}

fn without_target_dir(receipt: &mut Map<String, Value>) {
    receipt.remove("target_dir");
}

fn set_target_dir(receipt: &mut Map<String, Value>, value: Value) {
    receipt.insert("target_dir".into(), value);
}

impl Fixture {
    fn new() -> Fixture {
        let temp = tempfile::Builder::new()
            .prefix("rsi-install-provenance-")
            // Outside this checkout: the fixture's git must never find it.
            .tempdir_in("/tmp")
            .unwrap();
        let base = fs::canonicalize(temp.path()).unwrap();
        let home = base.join("home");
        fs::create_dir(&home).unwrap();
        let target = home.join(".rsi/staging/release-target");
        let log = base.join("cargo.log");
        let fakes = base.join("fakes");
        fs::create_dir(&fakes).unwrap();
        executable(&fakes.join("cargo"), FAKE_CARGO);
        executable(&fakes.join("rustup"), "#!/bin/sh\nexit 99\n");
        isolation::install(&base, &fakes);

        let mut env: HashMap<String, String> = HashMap::new();
        for (key, value) in [
            ("HOME", text(&home)),
            ("PATH", text(&fakes)),
            ("CARGO_TARGET_DIR", text(&target)),
            ("FAKE_CARGO_LOG", text(&log)),
            ("GIT_AUTHOR_NAME", "t".into()),
            ("GIT_AUTHOR_EMAIL", "t@example.invalid".into()),
            ("GIT_COMMITTER_NAME", "t".into()),
            ("GIT_COMMITTER_EMAIL", "t@example.invalid".into()),
            ("GIT_CONFIG_NOSYSTEM", "1".into()),
        ] {
            env.insert(key.to_string(), value);
        }

        // Worktree A: the repository; worktree B: a linked worktree one commit ahead.
        let a = base.join("a");
        let scripts = Path::new(rsi_common::manifest_dir!()).join("../../scripts");
        copy_scripts(&scripts, &a.join("scripts"), true);
        // The guard is the binary Cargo built for this test crate.
        executable(
            &a.join("scripts/repo-guard"),
            &format!(
                "#!/bin/sh\nguard='{}'\n[ \"${{1:-}}\" = --bin ] && {{ echo \"$guard\"; exit 0; }}\nexec \"$guard\" \"$@\"\n",
                env!("CARGO_BIN_EXE_rsi-repo-guard")
            ),
        );
        fs::write(a.join("Cargo.toml"), "[workspace]\nmembers = []\n").unwrap();
        fs::write(a.join(".gitignore"), "target/\n").unwrap();
        fs::create_dir(a.join("src")).unwrap();
        fs::write(a.join("src/lib.rs"), "pub fn built() {}\n").unwrap();
        let b = base.join("b");
        let fixture = Fixture {
            _temp: temp,
            base,
            home,
            target,
            log,
            fakes,
            env,
            a,
            b,
        };
        fixture.git(&fixture.a, &["init", "-q", "-b", "main"]);
        fixture.git(&fixture.a, &["add", "-A"]);
        fixture.git(&fixture.a, &["commit", "-q", "-m", "base"]);
        fixture.git(
            &fixture.a,
            &["worktree", "add", "-q", "-b", "other", &text(&fixture.b)],
        );
        fs::write(fixture.b.join("change.txt"), "b\n").unwrap();
        fixture.git(&fixture.b, &["add", "change.txt"]);
        fixture.git(&fixture.b, &["commit", "-q", "-m", "b ahead"]);
        fixture
    }

    fn command(&self, program: impl AsRef<std::ffi::OsStr>, extra: &[(&str, &str)]) -> Command {
        let mut command = Command::new(program);
        command.env_clear().envs(&self.env).current_dir(&self.base);
        for (key, value) in extra {
            command.env(key, value);
        }
        isolation::pin(&mut command, &self.base, &self.fakes, &self.home);
        command
    }

    fn git(&self, cwd: &Path, args: &[&str]) -> String {
        assert!(
            cwd.starts_with(&self.base),
            "fixture git outside the scratch dir: {cwd:?}"
        );
        let output = self
            .command("git", &[])
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
                "-C",
            ])
            .arg(cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn head(&self, worktree: &Path) -> String {
        self.git(worktree, &["rev-parse", "HEAD"])
    }

    fn collect(output: std::process::Output) -> Run {
        Run {
            code: output.status.code().unwrap_or(-1),
            out: String::from_utf8_lossy(&output.stdout).into_owned(),
            err: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }

    /// `install-release.sh --no-restart <flags>` of `worktree`; never asserts.
    fn try_install(&self, worktree: &Path, flags: &[&str], extra: &[(&str, &str)]) -> Run {
        let output = self
            .command("bash", extra)
            .arg(worktree.join("scripts/install-release.sh"))
            .arg("--no-restart")
            .args(flags)
            .output()
            .unwrap();
        Self::collect(output)
    }

    fn install(&self, worktree: &Path, flags: &[&str]) -> Run {
        self.install_with(worktree, flags, &[])
    }

    fn install_with(&self, worktree: &Path, flags: &[&str], extra: &[(&str, &str)]) -> Run {
        let run = self.try_install(worktree, flags, extra);
        assert!(
            run.ok(),
            "install failed ({}):\n{}\n{}",
            run.code,
            run.out,
            run.err
        );
        run
    }

    /// A `--link-only --build-receipt` install; never asserts.
    fn try_receipt(&self, worktree: &Path, receipt: &Path, more: &[&str]) -> Run {
        let receipt = text(receipt);
        let mut flags = vec!["--link-only", "--build-receipt", receipt.as_str()];
        flags.extend(more);
        self.try_install(worktree, &flags, &[])
    }

    fn cargo_calls(&self, command: &str) -> Vec<Vec<String>> {
        let Ok(log) = fs::read_to_string(&self.log) else {
            return Vec::new();
        };
        log.lines()
            .map(|line| line.split(' ').map(str::to_string).collect::<Vec<_>>())
            .filter(|call| call.first().map(String::as_str) == Some(command))
            .collect()
    }

    fn cargo_commands(&self) -> Vec<String> {
        let log = fs::read_to_string(&self.log).unwrap();
        log.lines()
            .filter_map(|line| line.split(' ').next())
            .map(str::to_string)
            .collect()
    }

    /// Run the fake cargo directly, as a build job or a hand build would.
    fn cargo(&self, args: &[&str]) {
        let output = self
            .command(self.fakes.join("cargo"), &[])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn marker(&self) -> HashMap<String, String> {
        let raw = fs::read_to_string(self.target.join("release/.rsi-release-source")).unwrap();
        raw.lines()
            .filter_map(|line| line.split_once('='))
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    fn installed_rsid_build(&self) -> String {
        let output = Command::new(self.home.join(".rsi/install/rsid"))
            .arg("--build-info")
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .next()
            .unwrap()
            .to_string()
    }

    fn commit_next(&self, worktree: &Path) {
        fs::write(worktree.join("next.txt"), "a2\n").unwrap();
        self.git(worktree, &["add", "next.txt"]);
        self.git(worktree, &["commit", "-q", "-m", "a moves"]);
    }

    /// Build `worktree`'s rsid by hand into the shared target (no builder record).
    fn hand_build(&self, worktree: &Path) {
        fs::remove_dir_all(self.target.join("release").join(FINGERPRINT)).unwrap();
        let manifest = text(&worktree.join("Cargo.toml"));
        self.cargo(&[
            "build",
            "--release",
            "--manifest-path",
            &manifest,
            "--bin",
            "rsid",
        ]);
    }

    // --- #157: a build job's receipt is the provenance of its binaries -----------

    /// Build as the daemon's build job does (cargo only, no builder record) and
    /// write `receipt.json` from the default receipt as `edit` changes it.
    fn build_job_receipt(
        &self,
        worktree: &Path,
        edit: impl FnOnce(&mut Map<String, Value>),
    ) -> PathBuf {
        let manifest = text(&worktree.join("Cargo.toml"));
        let mut args = vec!["build", "--release", "--manifest-path", manifest.as_str()];
        for bin in FAKE_BINS {
            args.extend(["--bin", bin]);
        }
        self.cargo(&args);
        assert!(!self.target.join("release/.rsi-release-source").exists());
        let mut receipt = Map::new();
        receipt.insert("exit_code".into(), json!(0));
        receipt.insert("source_head".into(), json!(self.head(worktree)));
        receipt.insert("source_dirty".into(), json!(false));
        receipt.insert("target_dir".into(), json!(text(&self.target)));
        edit(&mut receipt);
        self.write_receipt("receipt.json", &Value::Object(receipt))
    }

    fn write_receipt(&self, name: &str, receipt: &Value) -> PathBuf {
        let path = self.base.join(name);
        fs::write(&path, receipt.to_string()).unwrap();
        path
    }

    /// Wrap the receipt's whole object under `result`, as the daemon's job result does.
    fn wrap_under_result(receipt: &Path) {
        let inner: Value = serde_json::from_str(&fs::read_to_string(receipt).unwrap()).unwrap();
        fs::write(receipt, json!({ "result": inner }).to_string()).unwrap();
    }

    // --- #282: the receipt names the target the job built in ---------------------

    /// Stage as a PM shell does: its stamped CARGO_TARGET_DIR is its own sandbox target.
    fn stage_from_other_shell_target(&self, receipt: &Path) -> (Run, PathBuf, PathBuf) {
        let other = self.base.join("pm-sandbox-target");
        fs::create_dir_all(&other).unwrap();
        let stage = self.base.join("stage");
        let run = self.try_install(
            &self.a,
            &[
                "--stage-dir",
                &text(&stage),
                "--link-only",
                "--build-receipt",
                &text(receipt),
            ],
            &[("CARGO_TARGET_DIR", &text(&other))],
        );
        (run, stage, other)
    }

    /// A directory outside every allowed target that holds a same-SHA rsid.
    fn foreign_target(&self) -> PathBuf {
        let foreign = self.base.join("foreign-target");
        fs::create_dir_all(foreign.join("release")).unwrap();
        for name in FAKE_BINS {
            let body = if name == "rsid" {
                format!("echo \"{} 999\"", self.head(&self.a))
            } else {
                format!("echo \"foreign-{name}\"")
            };
            executable(
                &foreign.join("release").join(name),
                &format!("#!/bin/sh\n{body}\n"),
            );
        }
        foreign
    }

    /// The refusal comes before any lock, binary run or copy (#3513).
    fn assert_target_refused(&self, receipt: &Path, message: &str) -> Run {
        let (run, stage, other) = self.stage_from_other_shell_target(receipt);
        let shown = format!("{}\n{}", run.out, run.err);
        assert!(!run.ok(), "{shown}");
        assert!(run.err.contains(message), "{message:?} not in {shown}");
        assert!(run.err.contains("target_dir"), "{shown}");
        assert!(!run.out.contains("Linked:"), "{shown}");
        assert!(!stage.exists());
        assert!(
            is_empty_dir(&other),
            "no lock or file was written into a target"
        );
        run
    }

    // --- #2188: kept copies live exactly as long as a link resolves to them ------

    fn kept_copies(&self) -> Vec<String> {
        let keep = self.home.join(".rsi/install-kept");
        if !keep.is_dir() {
            return Vec::new();
        }
        names(&keep)
            .into_iter()
            .filter(|name| !name.starts_with('.'))
            .collect()
    }

    fn link_into_release(&self, link: &Path) {
        let _ = fs::remove_file(link);
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        symlink(self.target.join("release/rsi"), link).unwrap();
    }

    fn run_binary(path: &Path) -> String {
        let output = Command::new(path).output().unwrap();
        assert!(output.status.success());
        String::from_utf8_lossy(&output.stdout).into_owned()
    }
}

// --- #938: the shared target never serves another worktree's crates --------------

#[test]
fn building_from_another_worktree_cleans_only_the_workspace_crates() {
    let f = Fixture::new();
    f.install(&f.a, &[]);
    assert!(f.cargo_calls("clean").is_empty());
    assert_eq!(f.installed_rsid_build(), f.head(&f.a));

    fs::remove_file(&f.log).unwrap();
    let run = f.install(&f.b, &[]);
    assert!(
        run.err
            .contains(&format!("was last built from {}", f.a.display())),
        "{}",
        run.err
    );
    // The guard cleans before the build (the workspace call plus the separate
    // rsi-repo-guard call, #451); the embedded-commit backstop never has to rebuild.
    let commands: Vec<String> = f
        .cargo_commands()
        .into_iter()
        .filter(|command| command == "clean" || command == "build")
        .collect();
    assert_eq!(commands, ["clean", "build", "build"]);
    let cleans = f.cargo_calls("clean");
    assert!(cleans[0].contains(&"--release".to_string()));
    let packages: Vec<&str> = cleans[0]
        .windows(2)
        .filter(|pair| pair[0] == "-p")
        .map(|pair| pair[1].as_str())
        .collect();
    assert_eq!(packages, MEMBERS);
    // Third-party artifacts stay shared; B's binaries carry B's commit.
    assert!(
        f.target
            .join("release/.fingerprint/serde-fedcba9876543210")
            .is_dir()
    );
    assert_eq!(f.installed_rsid_build(), f.head(&f.b));
    let marker = f.marker();
    assert_eq!(marker["worktree"], text(&f.b));
    assert_eq!(marker["state"], "built");
    assert_eq!(marker["head"], f.head(&f.b));
    assert_eq!(marker["dirty"], "false");
}

#[test]
fn rebuilding_from_the_same_worktree_keeps_its_artifacts() {
    let f = Fixture::new();
    f.install(&f.a, &[]);
    f.commit_next(&f.a);
    f.install(&f.a, &[]);
    assert!(f.cargo_calls("clean").is_empty());
    assert_eq!(f.installed_rsid_build(), f.head(&f.a));
}

#[test]
fn a_target_with_no_builder_record_is_cleaned_before_the_build() {
    let f = Fixture::new();
    let crate_dir = f.target.join("release").join(FINGERPRINT);
    fs::create_dir_all(&crate_dir).unwrap();
    fs::write(
        crate_dir.join("source"),
        format!(
            "root={}\nsha={}\n",
            f.base.join("elsewhere").display(),
            "0".repeat(40)
        ),
    )
    .unwrap();
    let run = f.install(&f.a, &[]);
    assert!(
        run.err.contains("no record of the worktree that built it"),
        "{}",
        run.err
    );
    assert_eq!(f.cargo_calls("clean").len(), 1);
    assert_eq!(f.installed_rsid_build(), f.head(&f.a));
}

#[test]
fn tui_only_build_from_another_worktree_also_cleans() {
    let f = Fixture::new();
    f.install(&f.a, &[]);
    f.install(&f.b, &["--tui-only"]);
    assert_eq!(f.cargo_calls("clean").len(), 1);
    assert_eq!(f.marker()["worktree"], text(&f.b));
    let rsi = fs::read_to_string(f.target.join("release/rsi")).unwrap();
    assert!(rsi.contains(&format!("from {}", f.b.display())), "{rsi}");
}

#[test]
fn an_out_of_band_build_is_caught_by_the_embedded_commit_and_rebuilt() {
    let f = Fixture::new();
    f.install(&f.b, &[]);
    // Someone builds worktree A into the shared target by hand: the record still
    // names B, but the workspace crates are A's.
    f.hand_build(&f.a);
    let run = f.install(&f.b, &[]);
    assert!(
        run.err.contains(&format!("is at {}", f.head(&f.b))),
        "{}",
        run.err
    );
    assert!(
        run.err
            .contains("Rebuilding this workspace's release crates"),
        "{}",
        run.err
    );
    assert_eq!(f.cargo_calls("clean").len(), 1);
    assert_eq!(f.installed_rsid_build(), f.head(&f.b));
}

#[test]
fn a_binary_that_stays_foreign_is_never_installed() {
    let f = Fixture::new();
    f.install(&f.b, &[]);
    let installed_before = f.installed_rsid_build();
    f.hand_build(&f.a);
    let run = f.try_install(&f.b, &[], &[("FAKE_CLEAN_IGNORED", "1")]);
    assert!(!run.ok());
    assert!(run.err.contains("Refusing to install"), "{}", run.err);
    assert_eq!(f.installed_rsid_build(), installed_before);
    assert_eq!(f.marker()["state"], "building");
}

#[test]
fn link_only_links_this_worktrees_build() {
    let f = Fixture::new();
    f.install(&f.a, &[]);
    let run = f.install(&f.a, &["--link-only"]);
    assert!(run.out.contains("Linked:"), "{}", run.out);
    assert_eq!(f.installed_rsid_build(), f.head(&f.a));
}

#[test]
fn link_only_from_another_worktree_refuses_unless_allowed() {
    let f = Fixture::new();
    f.install(&f.a, &[]);
    let refused = f.try_install(&f.b, &["--link-only"], &[]);
    assert!(!refused.ok());
    let message = format!("were built from {}, not {}", f.a.display(), f.b.display());
    assert!(refused.err.contains(&message), "{}", refused.err);
    assert!(
        refused.err.contains("--allow-foreign-build"),
        "{}",
        refused.err
    );

    let allowed = f.install(&f.b, &["--link-only", "--allow-foreign-build"]);
    assert!(
        allowed.err.contains("--allow-foreign-build"),
        "{}",
        allowed.err
    );
    assert_eq!(f.installed_rsid_build(), f.head(&f.a));
}

#[test]
fn link_only_after_head_moved_refuses() {
    let f = Fixture::new();
    f.install(&f.a, &[]);
    let built = f.head(&f.a);
    f.commit_next(&f.a);
    let refused = f.try_install(&f.a, &["--link-only"], &[]);
    assert!(!refused.ok());
    let message = format!(
        "built at {built}, but {} is now at {}",
        f.a.display(),
        f.head(&f.a)
    );
    assert!(refused.err.contains(&message), "{}", refused.err);
}

#[test]
fn link_only_without_a_builder_record_refuses() {
    let f = Fixture::new();
    f.install(&f.a, &[]);
    fs::remove_file(f.target.join("release/.rsi-release-source")).unwrap();
    let refused = f.try_install(&f.a, &["--link-only"], &[]);
    assert!(!refused.ok());
    assert!(
        refused.err.contains("No release build from"),
        "{}",
        refused.err
    );
}

// --- #157: a build job's receipt is the provenance of its binaries ---------------

#[test]
fn link_only_accepts_a_build_jobs_receipt_without_allow_foreign_build() {
    let f = Fixture::new();
    let receipt = f.build_job_receipt(&f.a, no_edit);
    let refused = f.try_install(&f.a, &["--link-only"], &[]);
    assert!(
        refused.err.contains("No release build from"),
        "{}",
        refused.err
    );
    let run = f.try_receipt(&f.a, &receipt, &[]);
    assert!(run.ok(), "{}\n{}", run.out, run.err);
    assert!(run.out.contains("Linked:"), "{}", run.out);
    assert!(!run.err.contains("--allow-foreign-build"), "{}", run.err);
    assert_eq!(f.installed_rsid_build(), f.head(&f.a));
}

#[test]
fn a_receipt_wrapped_under_result_is_read() {
    let f = Fixture::new();
    let receipt = f.build_job_receipt(&f.a, no_edit);
    Fixture::wrap_under_result(&receipt);
    let run = f.try_receipt(&f.a, &receipt, &[]);
    assert!(run.out.contains("Linked:"), "{}\n{}", run.out, run.err);
}

#[test]
fn a_receipt_for_another_commit_is_refused() {
    let f = Fixture::new();
    let receipt = f.build_job_receipt(&f.a, no_edit);
    let refused = f.try_receipt(&f.b, &receipt, &[]);
    assert!(!refused.ok());
    let message = format!(
        "compiled {}, but {} is at {}",
        f.head(&f.a),
        f.b.display(),
        f.head(&f.b)
    );
    assert!(refused.err.contains(&message), "{}", refused.err);
}

#[test]
fn a_receipt_does_not_vouch_for_binaries_of_another_commit() {
    let f = Fixture::new();
    // The job claims HEAD of b, but the binaries in the target came from a.
    f.build_job_receipt(&f.a, no_edit);
    let lying = f.write_receipt(
        "lying.json",
        &json!({"exit_code": 0, "source_head": f.head(&f.b), "source_dirty": false,
                "target_dir": text(&f.target)}),
    );
    let refused = f.try_receipt(&f.b, &lying, &[]);
    assert!(!refused.ok());
    assert!(
        refused
            .err
            .contains(&format!("embeds commit {}", f.head(&f.a))),
        "{}",
        refused.err
    );
}

#[test]
fn a_failed_refused_dirty_or_headless_receipt_is_refused() {
    let cases: [(Edit, &str); 4] = [
        (
            |r| {
                r.insert("exit_code".into(), json!(101));
            },
            "did not succeed",
        ),
        (
            |r| {
                r.insert("refusal".into(), json!("job_source_moved"));
            },
            "was refused",
        ),
        (
            |r| {
                r.insert("source_dirty".into(), json!(true));
            },
            "compiled a dirty tree",
        ),
        (
            |r| {
                r.insert("source_head".into(), Value::Null);
            },
            "carries no source_head",
        ),
    ];
    for (edit, message) in cases {
        let f = Fixture::new();
        let receipt = f.build_job_receipt(&f.a, edit);
        let refused = f.try_receipt(&f.a, &receipt, &[]);
        assert!(!refused.ok(), "{message}");
        assert!(
            refused.err.contains(message),
            "{message:?} not in {}",
            refused.err
        );
    }
}

#[test]
fn a_receipt_is_refused_after_the_sources_were_edited() {
    let f = Fixture::new();
    let receipt = f.build_job_receipt(&f.a, no_edit);
    fs::write(f.a.join("src/lib.rs"), "pub fn edited() {}\n").unwrap();
    let refused = f.try_receipt(&f.a, &receipt, &[]);
    assert!(!refused.ok());
    assert!(
        refused.err.contains("uncommitted edits or new files"),
        "{}",
        refused.err
    );
}

#[test]
fn a_missing_or_unreadable_receipt_is_refused() {
    let f = Fixture::new();
    f.build_job_receipt(&f.a, no_edit);
    let missing = f.try_receipt(&f.a, &f.base.join("none.json"), &[]);
    assert!(missing.err.contains("does not exist"), "{}", missing.err);
    let junk = f.base.join("junk.json");
    fs::write(&junk, "not json").unwrap();
    let unreadable = f.try_receipt(&f.a, &junk, &[]);
    assert!(!unreadable.ok());
    assert!(
        unreadable.err.contains("Cannot read build receipt"),
        "{}",
        unreadable.err
    );
}

#[test]
fn build_receipt_needs_link_only_and_excludes_allow_foreign_build() {
    let f = Fixture::new();
    let receipt = f.build_job_receipt(&f.a, no_edit);
    let no_link = f.try_install(&f.a, &["--build-receipt", &text(&receipt)], &[]);
    assert!(!no_link.ok());
    assert!(
        no_link.err.contains("only applies with --link-only"),
        "{}",
        no_link.err
    );
    let both = f.try_receipt(&f.a, &receipt, &["--allow-foreign-build"]);
    assert!(!both.ok());
    assert!(both.err.contains("are alternatives"), "{}", both.err);
}

#[test]
fn a_staged_build_job_release_needs_no_allow_foreign_build() {
    let f = Fixture::new();
    let receipt = f.build_job_receipt(&f.a, no_edit);
    let stage = f.base.join("stage");
    let run = f.try_receipt(&f.a, &receipt, &["--stage-dir", &text(&stage)]);
    assert!(stage.join("rsid").is_file(), "{}\n{}", run.out, run.err);
}

// --- #282: the receipt names the target the job built in -------------------------

#[test]
fn a_release_job_receipt_binds_staging_to_clean_source_and_selected_target() {
    let f = Fixture::new();
    let mut bins = FAKE_BINS.to_vec();
    bins.extend(["rsi-repo-guard", "rsi-cloud", "rsi-eval"]);
    let receipt = f.build_job_receipt(&f.a, |receipt| {
        receipt.insert(
            "release_members".into(),
            json!([{"package":"fixture", "bins":bins}]),
        );
    });
    for name in ["rsi-repo-guard", "rsi-cloud", "rsi-eval"] {
        executable(
            &f.target.join("release").join(name),
            "#!/bin/sh\necho isolated-member\n",
        );
    }
    let (run, stage, _) = f.stage_from_other_shell_target(&receipt);
    assert!(run.ok(), "{}\n{}", run.out, run.err);
    for name in ["rsid", "rsi", "rsi-repo-guard"] {
        assert_eq!(
            fs::read(stage.join(name)).unwrap(),
            fs::read(f.target.join("release").join(name)).unwrap()
        );
    }
    let clean: Value = serde_json::from_str(&fs::read_to_string(&receipt).unwrap()).unwrap();
    for (field, value) in [
        ("exit_code", json!(17)),
        ("source_dirty", json!(true)),
        ("source_head", json!("0".repeat(40))),
        ("refusal", json!("job_source_moved")),
        (
            "release_members",
            json!([{"package":"core", "bins":FAKE_BINS}]),
        ),
    ] {
        let mut invalid = clean.clone();
        invalid[field] = value;
        let bad = f.write_receipt(&format!("{field}.json"), &invalid);
        let refused = f.try_receipt(
            &f.a,
            &bad,
            &["--stage-dir", &text(&f.base.join(format!("stage-{field}")))],
        );
        assert_ne!(refused.code, 0, "{field}: {}\n{}", refused.out, refused.err);
    }
    fs::remove_file(f.target.join("release/rsi-eval")).unwrap();
    let missing = f.try_receipt(
        &f.a,
        &receipt,
        &["--stage-dir", &text(&f.base.join("stage-missing"))],
    );
    assert_ne!(missing.code, 0, "{}\n{}", missing.out, missing.err);
    assert!(missing.err.contains("rsi-eval"), "{}", missing.err);
}

#[test]
fn a_receipt_target_dir_beats_the_callers_cargo_target_dir() {
    let f = Fixture::new();
    let receipt = f.build_job_receipt(&f.a, no_edit);
    let (run, stage, other) = f.stage_from_other_shell_target(&receipt);
    assert!(run.ok(), "{}\n{}", run.out, run.err);
    assert!(stage.join("rsid").is_file(), "{}", run.out);
    let info = Command::new(stage.join("rsid"))
        .arg("--build-info")
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&info.stdout).contains(&f.head(&f.a)));
    assert!(
        is_empty_dir(&other),
        "nothing was built or written into the caller's target"
    );
}

#[test]
fn a_result_wrapped_receipt_target_dir_is_read() {
    let f = Fixture::new();
    let receipt = f.build_job_receipt(&f.a, no_edit);
    Fixture::wrap_under_result(&receipt);
    let (run, stage, _) = f.stage_from_other_shell_target(&receipt);
    assert!(run.ok(), "{}\n{}", run.out, run.err);
    assert!(stage.join("rsid").is_file());
}

#[test]
fn a_receipt_without_target_dir_needs_an_explicit_target_dir_flag() {
    // #3514: no silent fallback to the caller's cargo target.
    let f = Fixture::new();
    let receipt = f.build_job_receipt(&f.a, without_target_dir);
    let data: Value = serde_json::from_str(&fs::read_to_string(&receipt).unwrap()).unwrap();
    assert!(data.get("target_dir").is_none());
    let (run, stage, other) = f.stage_from_other_shell_target(&receipt);
    assert!(!run.ok());
    assert!(run.err.contains("names no target_dir"), "{}", run.err);
    assert!(run.err.contains("--target-dir"), "{}", run.err);
    assert!(!run.err.contains("embeds commit"), "{}", run.err);
    assert!(!stage.exists());
    assert!(is_empty_dir(&other));
}

#[test]
fn a_legacy_receipt_deploys_with_a_validated_target_dir_flag() {
    let f = Fixture::new();
    let receipt = f.build_job_receipt(&f.a, without_target_dir);
    let other = f.base.join("pm-sandbox-target");
    fs::create_dir_all(&other).unwrap();
    let stage = f.base.join("stage");
    let run = f.install_with(
        &f.a,
        &[
            "--stage-dir",
            &text(&stage),
            "--link-only",
            "--build-receipt",
            &text(&receipt),
            "--target-dir",
            &text(&f.target),
        ],
        &[("CARGO_TARGET_DIR", &text(&other))],
    );
    assert!(stage.join("rsid").is_file(), "{}\n{}", run.out, run.err);
}

#[test]
fn the_target_dir_flag_must_be_canonical_absolute_and_an_existing_directory() {
    let f = Fixture::new();
    let receipt = f.build_job_receipt(&f.a, without_target_dir);
    let link = f.base.join("link-to-target");
    symlink(&f.target, &link).unwrap();
    let bad_dirs = [
        "relative/target".to_string(),
        text(&f.target.join("release/..")),
        text(&link),
        text(&f.base.join("nope")),
    ];
    for bad in &bad_dirs {
        let run = f.try_receipt(&f.a, &receipt, &["--target-dir", bad]);
        assert!(!run.ok(), "{bad}");
        assert!(run.err.contains("--target-dir"), "{bad}: {}", run.err);
    }
    let alone = f.try_install(
        &f.a,
        &["--link-only", "--target-dir", &text(&f.target)],
        &[],
    );
    assert!(!alone.ok());
    assert!(
        alone
            .err
            .contains("--target-dir only applies with --build-receipt"),
        "{}",
        alone.err
    );
}

#[test]
fn the_target_dir_flag_must_agree_with_the_receipts_target_dir() {
    let f = Fixture::new();
    // The caller's own cargo target is allowed too, so both spellings are valid
    // targets and only their disagreement is wrong.
    let other = f.base.join("pm-sandbox-target");
    fs::create_dir_all(&other).unwrap();
    let receipt = f.build_job_receipt(&f.a, |r| set_target_dir(r, json!(text(&other))));
    let run = f.try_install(
        &f.a,
        &[
            "--link-only",
            "--build-receipt",
            &text(&receipt),
            "--target-dir",
            &text(&f.target),
        ],
        &[("CARGO_TARGET_DIR", &text(&other))],
    );
    assert!(!run.ok());
    assert!(run.err.contains("differs from --target-dir"), "{}", run.err);
}

#[test]
fn a_target_dir_flag_outside_the_allowed_targets_is_refused_even_when_it_agrees() {
    // hub #3715: --target-dir never widens the allowed set.
    let f = Fixture::new();
    let foreign = f.foreign_target();
    let receipt = f.build_job_receipt(&f.a, |r| set_target_dir(r, json!(text(&foreign))));
    for more in [vec!["--target-dir", &text(&foreign)], vec![]] {
        let run = f.try_receipt(&f.a, &receipt, &more);
        assert!(!run.ok(), "{more:?}");
        assert!(
            run.err.contains("is not an allowed deploy target"),
            "{}",
            run.err
        );
        assert!(!run.out.contains("Linked:"));
    }
}

#[test]
fn a_foreign_target_dir_flag_is_refused_for_a_legacy_receipt() {
    // hub #3715: a receipt that names no target_dir and an unregistered --target-dir.
    let f = Fixture::new();
    let foreign = f.foreign_target();
    let receipt = f.build_job_receipt(&f.a, without_target_dir);
    let run = f.try_receipt(&f.a, &receipt, &["--target-dir", &text(&foreign)]);
    assert!(!run.ok());
    assert!(
        run.err.contains("is not an allowed deploy target"),
        "{}",
        run.err
    );
    assert!(!run.out.contains("Linked:"));
}

#[test]
fn a_receipt_target_dir_with_no_binaries_names_that_path() {
    let f = Fixture::new();
    // An allowed target (the caller's own) that holds no build.
    let empty = f.base.join("pm-sandbox-target");
    let receipt = f.build_job_receipt(&f.a, |r| set_target_dir(r, json!(text(&empty))));
    let (run, stage, _) = f.stage_from_other_shell_target(&receipt);
    assert!(!run.ok());
    assert!(
        run.err.contains(&text(&empty.join("release/rsid"))),
        "{}",
        run.err
    );
    assert!(run.err.contains("Rebuild with a build job"), "{}", run.err);
    assert!(!run.err.contains("embeds commit <none>"), "{}", run.err);
    assert!(!stage.exists());
}

#[test]
fn a_foreign_absolute_target_dir_holding_a_same_sha_rsid_is_refused() {
    let f = Fixture::new();
    let foreign = f.foreign_target();
    let receipt = f.build_job_receipt(&f.a, |r| set_target_dir(r, json!(text(&foreign))));
    f.assert_target_refused(&receipt, "is not an allowed deploy target");
    assert_eq!(names(&foreign), ["release"]);
    let mut expected: Vec<String> = FAKE_BINS.iter().map(|name| name.to_string()).collect();
    expected.sort();
    assert_eq!(names(&foreign.join("release")), expected);
}

#[test]
fn a_target_dir_escaping_through_dot_dot_is_refused() {
    let f = Fixture::new();
    let foreign = f.foreign_target();
    let escaping = format!(
        "{}/../../../{}",
        f.target.display(),
        foreign.strip_prefix("/").unwrap().display()
    );
    let receipt = f.build_job_receipt(&f.a, |r| set_target_dir(r, json!(escaping)));
    f.assert_target_refused(&receipt, "is not an allowed deploy target");
}

#[test]
fn a_target_dir_escaping_through_a_symlink_is_refused() {
    let f = Fixture::new();
    let foreign = f.foreign_target();
    let link = f.base.join("looks-allowed");
    symlink(&foreign, &link).unwrap();
    let receipt = f.build_job_receipt(&f.a, |r| set_target_dir(r, json!(text(&link))));
    f.assert_target_refused(&receipt, "is not an allowed deploy target");
    let inside = f.target.join("debug");
    symlink(&foreign, &inside).unwrap();
    let receipt = f.build_job_receipt(&f.a, |r| set_target_dir(r, json!(text(&inside))));
    f.assert_target_refused(&receipt, "is not an allowed deploy target");
}

#[test]
fn a_subdirectory_of_the_allowed_target_is_not_the_allowed_target() {
    let f = Fixture::new();
    let release = text(&f.target.join("release"));
    let receipt = f.build_job_receipt(&f.a, |r| set_target_dir(r, json!(release)));
    f.assert_target_refused(&receipt, "is not an allowed deploy target");
}

#[test]
fn a_dot_dot_or_symlink_that_resolves_to_the_allowed_target_uses_the_resolved_path() {
    let f = Fixture::new();
    symlink(&f.target, f.base.join("alias")).unwrap();
    let spelled_paths = [
        format!("{}/alias", f.base.display()),
        format!(
            "{}/foreign-target/../shared/../home/.rsi/staging/release-target",
            f.base.display()
        ),
    ];
    for spelled in spelled_paths {
        let receipt = f.build_job_receipt(&f.a, |r| set_target_dir(r, json!(spelled)));
        let (run, stage, _) = f.stage_from_other_shell_target(&receipt);
        assert!(run.ok(), "{spelled}: {}\n{}", run.out, run.err);
        assert!(stage.join("rsid").is_file());
        fs::remove_dir_all(&stage).unwrap();
    }
}

#[test]
fn a_relative_non_string_or_empty_target_dir_fails_closed() {
    let f = Fixture::new();
    let bad_values = [
        json!("release-target"),
        json!(7),
        json!(""),
        json!(["/abs"]),
        json!({"path": "/abs"}),
        json!(true),
    ];
    for bad in bad_values {
        let receipt = f.build_job_receipt(&f.a, |r| set_target_dir(r, bad.clone()));
        let run = f.assert_target_refused(&receipt, "the receipt's target_dir");
        assert!(
            !run.err.contains("names no target_dir"),
            "{bad}: {}",
            run.err
        );
    }
}

#[test]
fn a_receipt_target_dir_of_null_is_refused_not_read_as_missing() {
    // hub #3716: the key being present decides, whatever its value. This replaces
    // the python test that read a null as a missing target_dir.
    let f = Fixture::new();
    let receipt = f.build_job_receipt(&f.a, |r| set_target_dir(r, Value::Null));
    let run = f.assert_target_refused(&receipt, "the receipt's target_dir");
    assert!(!run.err.contains("names no target_dir"), "{}", run.err);

    // A null under `result` is refused the same way, and an explicit --target-dir
    // does not turn it into a legacy receipt.
    Fixture::wrap_under_result(&receipt);
    let run = f.assert_target_refused(&receipt, "the receipt's target_dir");
    assert!(!run.err.contains("names no target_dir"), "{}", run.err);
    let with_override = f.try_receipt(&f.a, &receipt, &["--target-dir", &text(&f.target)]);
    assert!(!with_override.ok());
    assert!(
        with_override.err.contains("the receipt's target_dir"),
        "{}",
        with_override.err
    );
    assert!(!with_override.out.contains("Linked:"));
}

#[test]
fn a_missing_guard_binary_refuses_the_deploy_naming_the_build_command() {
    let f = Fixture::new();
    let receipt = f.build_job_receipt(&f.a, no_edit);
    // scripts/repo-guard exits 2 when the guard cannot be built; that exit must
    // never be read as "the receipt names no target_dir".
    executable(&f.a.join("scripts/repo-guard"), "#!/bin/sh\nexit 2\n");
    let (run, stage, other) = f.stage_from_other_shell_target(&receipt);
    assert!(!run.ok());
    assert!(
        run.err.contains("Cannot run the rsi-repo-guard binary"),
        "{}",
        run.err
    );
    assert!(
        run.err.contains("cargo build --release -p rsi-repo-guards"),
        "{}",
        run.err
    );
    assert!(!run.err.contains("names no target_dir"), "{}", run.err);
    assert!(!stage.exists());
    assert!(is_empty_dir(&other));
}

#[test]
fn the_callers_own_cargo_target_is_an_allowed_receipt_target() {
    let f = Fixture::new();
    let receipt = f.build_job_receipt(&f.a, no_edit);
    let run = f.install_with(
        &f.a,
        &["--link-only", "--build-receipt", &text(&receipt)],
        &[("CARGO_TARGET_DIR", &text(&f.target))],
    );
    assert!(run.out.contains("Linked:"), "{}", run.out);
}

#[test]
fn the_commit_check_still_applies_to_the_receipts_target() {
    let f = Fixture::new();
    // The job's target holds b's build; the receipt claims a's commit.
    f.build_job_receipt(&f.b, no_edit);
    let lying = f.write_receipt(
        "lying.json",
        &json!({"exit_code": 0, "source_head": f.head(&f.a), "source_dirty": false,
                "target_dir": text(&f.target)}),
    );
    let (run, stage, _) = f.stage_from_other_shell_target(&lying);
    assert!(!run.ok());
    assert!(run.err.contains("embeds commit"), "{}", run.err);
    assert!(!stage.exists());
}

// --- #2179: --link-only compares the sources, not just HEAD ----------------------

#[test]
fn link_only_refuses_source_edits_that_leave_head_unchanged() {
    let f = Fixture::new();
    f.install(&f.a, &[]);
    fs::write(
        f.a.join("src/lib.rs"),
        "pub fn edited_after_the_build() {}\n",
    )
    .unwrap();
    let refused = f.try_install(&f.a, &["--link-only"], &[]);
    assert!(!refused.ok());
    let message = format!(
        "The sources in {} changed since the release build",
        f.a.display()
    );
    assert!(refused.err.contains(&message), "{}", refused.err);
    // The fingerprint is the content: restoring the file makes it linkable.
    f.git(&f.a, &["checkout", "--", "src/lib.rs"]);
    assert!(f.install(&f.a, &["--link-only"]).out.contains("Linked:"));
}

#[test]
fn a_dirty_build_links_until_its_sources_change() {
    let f = Fixture::new();
    fs::write(f.a.join("src/lib.rs"), "pub fn uncommitted() {}\n").unwrap();
    f.install(&f.a, &[]);
    assert_eq!(f.marker()["dirty"], "true");
    assert!(f.install(&f.a, &["--link-only"]).out.contains("Linked:"));
    // A new, untracked source file is a source change too.
    fs::write(f.a.join("src/new_module.rs"), "pub fn added() {}\n").unwrap();
    let refused = f.try_install(&f.a, &["--link-only"], &[]);
    assert!(!refused.ok());
    assert!(
        refused.err.contains("uncommitted edits or new files"),
        "{}",
        refused.err
    );
}

#[test]
fn sources_changed_during_the_build_are_never_linked() {
    let f = Fixture::new();
    let touched = text(&f.a.join("src/lib.rs"));
    let run = f.install_with(&f.a, &[], &[("FAKE_TOUCH_DURING_BUILD", &touched)]);
    assert!(
        run.err.contains("changed while the release build ran"),
        "{}",
        run.err
    );
    assert_eq!(f.marker()["source_tree"], "changed-during-build");
    let refused = f.try_install(&f.a, &["--link-only"], &[]);
    assert!(!refused.ok());
    assert!(
        refused
            .err
            .contains("changed while the last release build ran"),
        "{}",
        refused.err
    );
}

// --- #2180: no cleanup leaves an installed link dangling -------------------------

#[test]
fn tui_only_links_a_stable_copy_that_survives_another_worktrees_clean() {
    let f = Fixture::new();
    f.install(&f.a, &["--tui-only"]);
    let link = f.home.join(".local/bin/rsi");
    assert_eq!(
        fs::canonicalize(&link).unwrap(),
        fs::canonicalize(f.home.join(".rsi/install/rsi")).unwrap()
    );
    assert_eq!(
        fs::canonicalize(f.a.join("target/release/rsi")).unwrap(),
        fs::canonicalize(&link).unwrap()
    );
    // Worktree B cleans the shared target, then its build fails.
    let failed = f.try_install(&f.b, &[], &[("FAKE_BUILD_FAILS", "1")]);
    assert!(!failed.ok());
    assert_eq!(f.cargo_calls("clean").len(), 1);
    assert_eq!(
        Fixture::run_binary(&link),
        format!("fake-rsi from {}\n", f.a.display())
    );
}

#[test]
fn a_cleanup_keeps_every_installed_link_into_the_target() {
    let f = Fixture::new();
    f.install(&f.a, &[]);
    let release = f.target.join("release");
    let bin_dir = f.home.join(".local/bin");
    // Links an older --tui-only install made: into Cargo's output directly.
    fs::remove_file(bin_dir.join("rsi")).unwrap();
    symlink(release.join("rsi"), bin_dir.join("rsi")).unwrap();
    let compat = f.a.join("target/release/rsi");
    fs::remove_file(&compat).unwrap();
    symlink(release.join("rsi"), &compat).unwrap();
    let expected = fs::read(release.join("rsi")).unwrap();
    let failed = f.try_install(&f.b, &[], &[("FAKE_BUILD_FAILS", "1")]);
    assert!(!failed.ok());
    assert!(!release.join("rsi").exists(), "the clean ran");
    let kept = fs::canonicalize(f.home.join(".rsi/install-kept")).unwrap();
    for link in [bin_dir.join("rsi"), compat] {
        let resolved = fs::canonicalize(&link).unwrap();
        assert_eq!(resolved.parent().unwrap(), kept, "{link:?}");
        assert_eq!(fs::read(&resolved).unwrap(), expected, "{link:?}");
        let message = format!("Kept installed link {}", link.display());
        assert!(failed.err.contains(&message), "{}", failed.err);
    }
}

// --- #2188: kept copies live exactly as long as a link resolves to them ----------

#[test]
fn an_install_prunes_kept_copies_its_links_no_longer_use() {
    let f = Fixture::new();
    f.install(&f.a, &[]);
    let bin_rsi = f.home.join(".local/bin/rsi");
    f.link_into_release(&bin_rsi);
    f.try_install(&f.b, &[], &[("FAKE_BUILD_FAILS", "1")]); // cleans, keeps one copy
    assert_eq!(f.kept_copies().len(), 1);
    let run = f.install(&f.a, &[]); // relinks at ~/.rsi/install
    assert!(f.kept_copies().is_empty());
    assert!(run.err.contains("Pruned kept copy"), "{}", run.err);
    assert_eq!(
        fs::canonicalize(&bin_rsi).unwrap(),
        fs::canonicalize(f.home.join(".rsi/install/rsi")).unwrap()
    );
}

#[test]
fn repeated_distinct_binaries_keep_only_the_linked_copy() {
    let f = Fixture::new();
    f.install(&f.a, &[]);
    let release = f.target.join("release");
    let bin_rsi = f.home.join(".local/bin/rsi");
    for generation in 1..4 {
        executable(
            &release.join("rsi"),
            &format!("#!/bin/sh\necho generation-{generation}\n"),
        );
        f.link_into_release(&bin_rsi);
        // Another worktree last built here, so this build cleans first.
        fs::write(
            release.join(".rsi-release-source"),
            "worktree=/elsewhere\nstate=built\n",
        )
        .unwrap();
        f.try_install(&f.a, &[], &[("FAKE_BUILD_FAILS", "1")]);
        assert_eq!(
            f.kept_copies().len(),
            1,
            "{generation}: {:?}",
            f.kept_copies()
        );
        assert_eq!(
            Fixture::run_binary(&bin_rsi),
            format!("generation-{generation}\n")
        );
    }
}

#[test]
fn a_copy_another_clones_link_resolves_to_is_never_pruned() {
    let f = Fixture::new();
    let clone = f.base.join("clone");
    f.git(&f.base, &["clone", "-q", &text(&f.a), &text(&clone)]);
    f.install(&f.a, &[]);
    let expected = fs::read(f.target.join("release/rsi")).unwrap();
    let clone_link = clone.join("target/release/rsi");
    f.link_into_release(&clone_link);
    f.try_install(&clone, &[], &[("FAKE_BUILD_FAILS", "1")]); // keeps the clone's link
    assert_eq!(f.kept_copies().len(), 1);
    // Worktree A's runs never scan the clone; the record protects its copy.
    f.install(&f.a, &[]);
    assert_eq!(
        fs::read(fs::canonicalize(&clone_link).unwrap()).unwrap(),
        expected
    );
    assert_eq!(f.kept_copies().len(), 1);
    // Once that link is gone, the next run prunes the copy.
    fs::remove_file(&clone_link).unwrap();
    f.install(&f.a, &["--link-only"]);
    assert!(f.kept_copies().is_empty());
    let links = fs::read_to_string(f.home.join(".rsi/install-kept/.links")).unwrap();
    assert_eq!(links, "");
}

#[test]
fn a_concurrent_release_build_into_the_same_target_waits_for_the_lock() {
    let f = Fixture::new();
    fs::create_dir_all(&f.target).unwrap();
    let lock = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(f.target.join(".rsi-release.lock"))
        .unwrap();
    lock.lock().unwrap();
    let mut install = f
        .command("bash", &[])
        .arg(f.b.join("scripts/install-release.sh"))
        .arg("--no-restart")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    let waiting = install.try_wait().unwrap();
    let builds = f.cargo_calls("build");
    if waiting.is_some() || !builds.is_empty() {
        let _ = install.kill();
    }
    assert!(waiting.is_none(), "the install did not wait for the lock");
    assert!(builds.is_empty());
    lock.unlock().unwrap();
    let run = Fixture::collect(install.wait_with_output().unwrap());
    assert!(run.ok(), "{}\n{}", run.out, run.err);
    assert!(
        run.err
            .contains("Waiting for another release build into this target"),
        "{}",
        run.err
    );
    // One install is the workspace build plus the separate guard build (#451).
    assert_eq!(f.cargo_calls("build").len(), 2);
}
