//! Rust port of the local-origin sweep fixtures (already shell-ported on
//! rolling). Git fixtures live outside the checkout; no network or AWS calls.
#![cfg(unix)]
mod common;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    allowed: PathBuf,
    origin: PathBuf,
    caller: PathBuf,
    sha: String,
}
fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "{args:?}: {output:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}
impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::Builder::new()
            .prefix("rsi-sweep-origin-")
            .tempdir_in("/tmp")
            .unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let allowed = root.join("allowed");
        let origin = allowed.join("origin.git");
        let caller = root.join("caller");
        fs::create_dir_all(&origin).unwrap();
        fs::create_dir(&caller).unwrap();
        git(&origin, &["init", "-q", "--bare"]);
        git(&caller, &["init", "-q", "--initial-branch=rolling"]);
        git(&caller, &["commit", "-q", "--allow-empty", "-m", "tip"]);
        let sha = git(&caller, &["rev-parse", "HEAD"]);
        git(
            &caller,
            &["push", "-q", origin.to_str().unwrap(), "rolling"],
        );
        git(
            &caller,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        Self {
            _tmp: tmp,
            root,
            allowed,
            origin,
            caller,
            sha,
        }
    }
    fn bundle(&self, origin: &str, roots: &[&Path]) -> Output {
        git(&self.caller, &["config", "remote.origin.url", origin]);
        let bundle = self.root.join("tip.bundle");
        if bundle.exists() {
            fs::remove_file(&bundle).unwrap();
        }
        let mut command = Command::new("bash");
        command
            .arg(common::repo_root().join("scripts/cloud-sweep.sh"))
            .arg("bundle")
            .arg(&self.sha)
            .arg(&bundle)
            .arg("--repo")
            .arg(&self.caller)
            .arg("--mirror")
            .arg(self.root.join("mirror.git"))
            .env("CDPATH", &self.root);
        for root in roots {
            command.arg("--allow-local-root").arg(root);
        }
        command.output().unwrap()
    }
    fn accepted(&self, origin: &str, roots: &[&Path]) {
        let output = self.bundle(origin, roots);
        assert!(output.status.success(), "{origin}: {output:?}");
        assert_eq!(
            git(&self.root, &["bundle", "list-heads", "tip.bundle"]),
            format!("{} refs/remotes/origin/rolling", self.sha)
        );
    }
    fn refused(&self, origin: &str, roots: &[&Path]) {
        let output = self.bundle(origin, roots);
        assert!(!output.status.success(), "{origin}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("cloud-sweep: refusing origin URL"),
            "{output:?}"
        );
    }
}
#[test]
fn existing_local_path_file_url_and_physical_symlinks_bundle_exact_tip() {
    let f = Fixture::new();
    for origin in [
        f.origin.display().to_string(),
        format!("file://{}", f.origin.display()),
        f.allowed
            .join("../allowed/origin.git")
            .display()
            .to_string(),
    ] {
        f.accepted(&origin, &[&f.allowed]);
    }
    symlink(&f.allowed, f.root.join("alias")).unwrap();
    symlink(&f.origin, f.allowed.join("link.git")).unwrap();
    f.accepted(
        f.allowed.join("link.git").to_str().unwrap(),
        &[&f.root.join("alias")],
    );
    f.accepted(
        f.root.join("alias/origin.git").to_str().unwrap(),
        &[&f.allowed],
    );
}
#[test]
fn local_origins_need_explicit_existing_allowed_roots() {
    let f = Fixture::new();
    f.refused(f.origin.to_str().unwrap(), &[]);
    f.refused(&format!("file://{}", f.origin.display()), &[]);
    fs::write(f.allowed.join("regular"), "not a repository").unwrap();
    symlink(f.allowed.join("missing"), f.allowed.join("dangling")).unwrap();
    for origin in ["missing", "dangling", "regular", "missing/../origin.git"] {
        f.refused(f.allowed.join(origin).to_str().unwrap(), &[&f.allowed]);
    }
    f.refused(f.origin.to_str().unwrap(), &[&f.root.join("missing")]);
    f.refused(f.origin.to_str().unwrap(), &[&f.allowed.join("regular")]);
    f.accepted(
        f.origin.to_str().unwrap(),
        &[&f.root.join("missing"), &f.allowed],
    );
}
#[test]
fn sibling_traversal_symlink_escape_and_hostile_transports_are_refused() {
    let f = Fixture::new();
    fs::create_dir(f.root.join("allowed-other")).unwrap();
    symlink(f.root.join("allowed-other"), f.allowed.join("escaped")).unwrap();
    for origin in [
        f.root.join("allowed-other"),
        f.allowed.join("escaped"),
        f.allowed.join("../allowed-other"),
        f.allowed.clone(),
    ] {
        f.refused(origin.to_str().unwrap(), &[&f.allowed]);
    }
    for origin in [
        "ext::sh -c true",
        "evil::repo",
        "-oProxyCommand=x",
        "http://example.com/x.git",
        "git://example.com/x.git",
        "ssh://-oProxyCommand=x/y",
        "https://-host/x",
        "git@github.com:-x",
        "file://localhost/etc",
        "file://relative",
        "https://example.com/a b",
        "ssh://host/a\nb",
    ] {
        f.refused(origin, &[&f.allowed]);
    }
}
