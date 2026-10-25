//! #452: a Rust fixture that builds provisional-migration repositories with the
//! real `rsi-repo-guard` executable, for the daemon tests that must cross the
//! child-process boundary (the lander's stale retry, the queue's enqueue
//! admission).
//!
//! `rsid` does not link `rsi-repo-guards` (see `repo_guard_call`), so the
//! fixture cannot call the guard's own Rust fixture. It writes the same inline
//! layout the guard's tests use, derives the released-migration pin with the
//! guard's own `released-migrations --refresh`, and builds candidates with
//! `rolling-migration-renumber`, all through the executable. No Python.
//!
//! This file is compiled into the library's test build and into the
//! `rsi-rolling-land` test build (`#[path]`), so it names only std, `serde_json`,
//! `sha2`, `tempfile` and `rsi_common`.

#![allow(dead_code)]

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

const STORE: &str = "crates/rsid-store/src/store/mod.rs";
const COHORT: &str = "crates/rsid-store/src/store/cohort_settlement.rs";
const TESTS: &str = "crates/rsid-store/src/store/tests.rs";
const DECLARATIONS: &str = "tools/provisional-migrations/";
/// The version every fixture source declares.
pub const DECLARED: i64 = 130;

const BASE_STORE: &str = "pub const LATEST_SCHEMA_VERSION: i32 = 129;\n// V0: Original schema\n// original\n// V1: Session metadata columns\nif version < 1 {\n    migrate_v1();\n}\nif version < 129 {\n    migrate_v129();\n}\n";
const BASE_TESTS: &str = "// RSI-RELEASED-MIGRATION-BEGIN: test-catalog\n// fixed\n// RSI-RELEASED-MIGRATION-END: test-catalog\nconst REWIND: i32 = 129;\nfn pagination() { assert_eq!(count, 130); }\nfn rewind(active: i32, target: i32) {\n    if active >= 129 && target < 129 {\n        drop_old();\n    }\n}\nfn fixture(target_version: i32) {\n    let scoped = 1;\n}\n";

/// The `rsi-repo-guard` executable the fixtures drive.
///
/// `RSI_TEST_REPO_GUARD` names a prebuilt one (a hub-supplied materialization);
/// otherwise `scripts/repo-guard --bin` builds it in its own Cargo invocation,
/// never together with `rsid`, so `serde_json/preserve_order` stays out of the
/// daemon build. A guard that cannot be had fails the test: a test that needs
/// the real executable never passes without it.
#[must_use]
pub fn real_guard() -> PathBuf {
    static GUARD: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    GUARD
        .get_or_init(locate_guard)
        .clone()
        .unwrap_or_else(|error| panic!("no rsi-repo-guard for the real-guard test: {error}"))
}

fn locate_guard() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("RSI_TEST_REPO_GUARD") {
        let path = PathBuf::from(path);
        return if path.is_file() {
            Ok(path)
        } else {
            Err(format!(
                "RSI_TEST_REPO_GUARD {} is not a file",
                path.display()
            ))
        };
    }
    let root = Path::new(rsi_common::manifest_dir!()).join("../..");
    let output = Command::new(root.join("scripts/repo-guard"))
        .arg("--bin")
        .current_dir(&root)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("cannot run scripts/repo-guard --bin: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "scripts/repo-guard --bin failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let path = PathBuf::from(stdout.lines().last().unwrap_or_default().trim());
    if path.is_file() {
        Ok(path)
    } else {
        Err(format!("scripts/repo-guard --bin printed {stdout:?}"))
    }
}

/// `sha256:<hex>` of `text`, the declaration's `source_blob` form.
fn digest(text: &str) -> String {
    format!("sha256:{:x}", Sha256::digest(text.as_bytes()))
}

fn git_command(cwd: &Path) -> Command {
    let mut command = Command::new("/usr/bin/git");
    command
        .current_dir(cwd)
        .args(["-c", "commit.gpgsign=false"])
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .env("HOME", "/nonexistent")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .stdin(Stdio::null());
    command
}

/// Run git in `cwd` and return trimmed stdout; any failure is a test failure.
pub fn git(cwd: &Path, args: &[&str]) -> String {
    let output = git_command(cwd).args(args).output().expect("git launch");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn write(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).expect("parent directory");
    std::fs::write(path, contents).expect("fixture file");
}

fn commit_all(cwd: &Path, message: &str) -> String {
    git(cwd, &["add", "."]);
    git(cwd, &["commit", "-q", "-m", message]);
    git(cwd, &["rev-parse", "HEAD"])
}

/// A repository in the inline store layout at schema V129 with a derived pin,
/// and the helpers that author provisional V130 sources on it.
pub struct Provisional {
    pub root: tempfile::TempDir,
    pub repo: PathBuf,
    pub base: String,
    pub guard: PathBuf,
}

impl Provisional {
    /// A fresh fixture. `extra` are additional committed files (`path`, text,
    /// executable) of the base, written before the pin is derived.
    #[must_use]
    pub fn new(guard: &Path, extra: &[(&str, &str, bool)]) -> Self {
        let root = tempfile::tempdir().expect("fixture root");
        let repo = root.path().canonicalize().unwrap().join("repo");
        std::fs::create_dir(&repo).expect("repository directory");
        git(&repo, &["init", "-q", "-b", "rolling"]);
        write(&repo, STORE, BASE_STORE);
        write(&repo, COHORT, "");
        write(&repo, TESTS, BASE_TESTS);
        write(
            &repo,
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/rsid\"]\nresolver = \"2\"\n",
        );
        write(
            &repo,
            "crates/rsid/Cargo.toml",
            "[package]\nname = \"rsid\"\nversion = \"0.1.0\"\n",
        );
        write(
            &repo,
            "crates/rsid-store/Cargo.toml",
            "[package]\nname = \"rsid-store\"\nversion = \"0.1.0\"\n",
        );
        for (path, text, executable) in extra {
            write(&repo, path, text);
            if *executable {
                set_executable(&repo.join(path));
            }
        }
        commit_all(&repo, "base files");
        let fixture = Self {
            root,
            repo,
            base: String::new(),
            guard: guard.to_path_buf(),
        };
        // The pin is written next to the tools; a fresh repository has no such directory.
        std::fs::create_dir_all(fixture.repo.join("tools")).expect("tools directory");
        fixture.guard_ok(&fixture.repo, &["released-migrations", "--refresh", "HEAD"]);
        let base = commit_all(&fixture.repo, "pin V129");
        Self { base, ..fixture }
    }

    /// Run `<guard> <args>` in `cwd`; returns `(exit code, stdout, stderr)`.
    pub fn guard_run(&self, cwd: &Path, args: &[&str]) -> (i32, String, String) {
        let output = Command::new(&self.guard)
            .args(args)
            .current_dir(cwd)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env("HOME", "/nonexistent")
            .stdin(Stdio::null())
            .output()
            .expect("guard launch");
        (
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    fn guard_ok(&self, cwd: &Path, args: &[&str]) -> String {
        let (code, stdout, stderr) = self.guard_run(cwd, args);
        assert_eq!(code, 0, "guard {args:?}: {stderr}");
        stdout
    }

    /// The worktree of the source `label`.
    #[must_use]
    pub fn worktree(&self, label: &str) -> PathBuf {
        self.root.path().join(format!("source-{label}"))
    }

    /// A source on the base that declares provisional V130 `label` migration;
    /// `extra` is appended to the store file. Returns its commit.
    pub fn source(&self, label: &str, extra: &str) -> String {
        let wt = self.worktree(label);
        git(
            &self.repo,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                wt.to_str().unwrap(),
                &self.base,
            ],
        );
        let helper = format!("crates/rsid-store/src/store/{label}_v130.rs");
        let helper_text = format!(
            "// RSI-RELEASED-MIGRATION-BEGIN: v130-{label}-migration\npub fn apply_v130() {{}}\n// RSI-RELEASED-MIGRATION-END: v130-{label}-migration\n"
        );
        let store = format!(
            "{}// V130: {label} migration\nif version < 130 {{\n    {label}_v130::apply_v130();\n}}\n{extra}",
            BASE_STORE
                .replace(
                    "LATEST_SCHEMA_VERSION: i32 = 129",
                    "LATEST_SCHEMA_VERSION: i32 = 130"
                )
                .replace(
                    "// V0: Original schema",
                    &format!("mod {label}_v130;\n// V0: Original schema")
                )
        );
        let tests = BASE_TESTS.replace("REWIND: i32 = 129", "REWIND: i32 = 130");
        write(&wt, &helper, &helper_text);
        write(&wt, STORE, &store);
        write(&wt, TESTS, &tests);
        let site = |anchor: String, replacement: &str, scope: &str| json!({"anchor": anchor, "replacement": replacement, "scope": scope});
        let entries = vec![
            json!({"path": STORE, "path_template": STORE, "source_blob": digest(&store), "sites": [
                site("pub const LATEST_SCHEMA_VERSION: i32 = 130;".into(), "pub const LATEST_SCHEMA_VERSION: i32 = ${VERSION};", "head"),
                site(format!("mod {label}_v130;"), &format!("mod {label}_v${{VERSION}};"), "unit"),
                site(format!("// V130: {label} migration"), &format!("// V${{VERSION}}: {label} migration"), "unit"),
                site("if version < 130 {".into(), "if version < ${VERSION} {", "unit"),
                site(format!("{label}_v130::apply_v130();"), &format!("{label}_v${{VERSION}}::apply_v${{VERSION}}();"), "unit")]}),
            json!({"path": TESTS, "path_template": TESTS, "source_blob": digest(&tests), "sites": [
                site("const REWIND: i32 = 130;".into(), "const REWIND: i32 = ${VERSION};", "head")]}),
            json!({"path": helper, "path_template": format!("crates/rsid-store/src/store/{label}_v${{VERSION}}.rs"), "source_blob": digest(&helper_text), "sites": [
                site(format!("// RSI-RELEASED-MIGRATION-BEGIN: v130-{label}-migration"), &format!("// RSI-RELEASED-MIGRATION-BEGIN: v${{VERSION}}-{label}-migration"), "unit"),
                site("pub fn apply_v130() {}".into(), "pub fn apply_v${VERSION}() {}", "unit"),
                site(format!("// RSI-RELEASED-MIGRATION-END: v130-{label}-migration"), &format!("// RSI-RELEASED-MIGRATION-END: v${{VERSION}}-{label}-migration"), "unit")]}),
        ];
        write(
            &wt,
            &format!("{DECLARATIONS}{label}.json"),
            &json!({"schema_version": 1, "version": DECLARED, "files": entries}).to_string(),
        );
        // The documented author step: the pin is refreshed against the base.
        self.guard_ok(&wt, &["released-migrations", "--refresh", &self.base]);
        commit_all(&wt, &format!("provisional V130 {label}"))
    }

    /// Commit one more file on the source `label` and return the new commit.
    pub fn commit_extra(&self, label: &str, name: &str, text: &str) -> String {
        let wt = self.worktree(label);
        write(&wt, name, text);
        git(&wt, &["add", name]);
        git(&wt, &["commit", "-q", "-m", &format!("extra {name}")]);
        git(&wt, &["rev-parse", "HEAD"])
    }

    /// The commit the lander would publish for `source` on `target`: a plain
    /// merge of the source when it already declares head + 1 (`renumber_noop`), else
    /// the renumbered candidate `--transform` and `--build` produce. Returns the
    /// unit the guard printed too.
    #[must_use]
    pub fn candidate(&self, source: &str, target: &str) -> (Value, String) {
        let repo = self.repo.to_str().unwrap();
        let out = self.guard_ok(
            &self.repo,
            &[
                "rolling-migration-renumber",
                "--repo",
                repo,
                "--transform",
                "--base",
                &self.base,
                "--source",
                source,
                "--target",
                target,
            ],
        );
        let unit: Value = serde_json::from_str(out.trim()).expect("unit json");
        if unit["renumber_noop"] == true {
            assert_eq!(
                git(&self.repo, &["merge-base", target, source]),
                target,
                "a no-op candidate must fast-forward its target"
            );
            // The lander merges such a source as is: a merge commit whose second
            // parent is the source (the proof search walks first-parent merges).
            let tree = git(&self.repo, &["rev-parse", &format!("{source}^{{tree}}")]);
            let merge = git(
                &self.repo,
                &[
                    "commit-tree",
                    &tree,
                    "-p",
                    target,
                    "-p",
                    source,
                    "-m",
                    "merge provisional source",
                ],
            );
            return (unit, merge);
        }
        let unit_file = self.root.path().join("unit.json");
        std::fs::write(&unit_file, unit.to_string()).expect("unit file");
        let scratch = self.root.path().join(format!("scratch-{}", &source[..12]));
        std::fs::create_dir_all(&scratch).expect("scratch");
        let built = self.guard_ok(
            &self.repo,
            &[
                "rolling-migration-renumber",
                "--repo",
                repo,
                "--build",
                "--unit-file",
                unit_file.to_str().unwrap(),
                "--scratch",
                scratch.to_str().unwrap(),
            ],
        );
        let built: Value = serde_json::from_str(built.trim()).expect("build json");
        let candidate = built["candidate"].as_str().expect("candidate").to_owned();
        (unit, candidate)
    }
}

#[cfg(unix)]
fn set_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) {}
