//! The warm target pool against real Cargo, on a tiny offline workspace (#453).
//!
//! These are the real-Cargo cases of the legacy `scripts/tests/test_target_pool.py` (deleted)
//! (#1741): they need `cargo` and, for the wrapper variants, the `rsi-build-rustc`
//! this package builds. The pool tool is `rsi-repo-guard target-pool` (built by
//! `scripts/repo-guard --bin` and run as a program with exactly the environment each case states). The full legacy-to-Rust case map is in the
//! header of `crates/rsi-repo-guards/src/target_pool_case_map.rs`.
//!
//! The clobber tests are the acceptance check: two sandboxes on different commits,
//! seeded from one pool, build concurrently; each gets its own commit's program and the
//! pool's files keep their bytes.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const LIB_BUILD: &str = r#"fn main() {
    let out = std::env::var("OUT_DIR").unwrap();
    let value = std::fs::read_to_string("gen.txt").unwrap();
    std::fs::write(format!("{out}/gen.rs"), format!("pub const GEN: &str = {:?};", value.trim())).unwrap();
    println!("cargo:rerun-if-changed=gen.txt");
    // A watched directory: Cargo compares the directory's own mtime.
    println!("cargo:rerun-if-changed=data");
}
"#;
const LIB_SRC: &str = r#"include!(concat!(env!("OUT_DIR"), "/gen.rs"));
pub fn value() -> String { format!("{GEN}-{}", VALUE) }
pub const VALUE: &str = "VALUE_PLACEHOLDER";
"#;
const APP_SRC: &str = "fn main() { println!(\"{} {}\", lib1::value(), baked::DIR); }\n";
const BAKED_SRC: &str = "pub const DIR: &str = env!(\"CARGO_MANIFEST_DIR\");\n";

type Env = BTreeMap<String, String>;

struct Rig {
    root: tempfile::TempDir,
    origin: PathBuf,
    one: String,
    two: String,
    env: Env,
}

/// The pool tool, `rsi-repo-guard`: `scripts/repo-guard --bin` builds it with the caller's own
/// Cargo setup (once) and prints its path. It is run as a program, never linked into this
/// package: linking would change `serde_json`'s feature set for every `rsid` test build (and
/// with it the topology starters' canonical digests).
fn guard_binary() -> &'static Path {
    static GUARD: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    GUARD.get_or_init(|| {
        let script = Path::new(rsi_common::manifest_dir!()).join("../../scripts/repo-guard");
        let output = Command::new("sh")
            .arg(script)
            .arg("--bin")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        PathBuf::from(String::from_utf8(output.stdout).unwrap().trim())
    })
}

fn wrapper() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_rsi-build-rustc"))
}

/// Every file under `root` with the SHA-256 of its bytes (symlinks and Cargo's lock
/// file skipped), to prove the pool keeps its bytes.
fn digest_tree(root: &Path) -> BTreeMap<String, String> {
    use sha2::{Digest, Sha256};
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                walk(root, &path, out);
            } else if kind.is_file() && path.file_name().is_some_and(|n| n != ".cargo-lock") {
                let digest = Sha256::digest(fs::read(&path).unwrap());
                out.insert(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    format!("{digest:x}"),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

fn compiled(log: &str) -> Vec<String> {
    let mut crates: Vec<String> = log
        .lines()
        .filter(|line| line.contains("Compiling"))
        .filter_map(|line| line.split_whitespace().nth(1).map(str::to_string))
        .collect();
    crates.sort();
    crates
}

impl Rig {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let mut env: Env = std::env::vars()
            .filter(|(name, _)| {
                !["CARGO_", "RSI_", "SCCACHE_", "RUSTC_"]
                    .iter()
                    .any(|prefix| name.starts_with(prefix))
            })
            .collect();
        let cargo_home = std::env::var("CARGO_HOME")
            .unwrap_or_else(|_| format!("{}/.cargo", std::env::var("HOME").unwrap_or_default()));
        for (name, value) in [
            (
                "RSI_TARGET_POOL_ROOT",
                root.path().join("pools").to_string_lossy().into_owned(),
            ),
            (
                "RSI_CARGO_SLOT",
                root.path()
                    .join("no-cargo-slot")
                    .to_string_lossy()
                    .into_owned(),
            ),
            ("RSI_TARGET_POOL_MIN_FREE_GIB", "0".into()),
            ("CARGO_INCREMENTAL", "0".into()),
            ("CARGO_PROFILE_DEV_DEBUG", "line-tables-only".into()),
            ("CARGO_HOME", cargo_home),
            ("CARGO_NET_OFFLINE", "true".into()),
        ] {
            env.insert(name.into(), value);
        }
        let origin = root.path().join("origin");
        fs::create_dir(&origin).unwrap();
        let mut rig = Self {
            root,
            origin,
            one: String::new(),
            two: String::new(),
            env,
        };
        rig.git(&rig.origin.clone(), &["init", "-q", "-b", "main"]);
        rig.write(
            "Cargo.toml",
            "[workspace]\nmembers = [\"lib1\", \"baked\", \"app\", \"stable\"]\nresolver = \"2\"\n",
        );
        rig.write(".gitignore", "target/\n");
        for (name, extra) in [
            ("lib1", "build = \"build.rs\"\n"),
            ("baked", ""),
            ("app", ""),
            ("stable", ""),
        ] {
            let deps = if name == "app" {
                "[dependencies]\nlib1 = { path = \"../lib1\" }\nbaked = { path = \"../baked\" }\n"
            } else {
                ""
            };
            rig.write(
                &format!("{name}/Cargo.toml"),
                &format!(
                    "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n{extra}{deps}"
                ),
            );
        }
        rig.write("lib1/build.rs", LIB_BUILD);
        rig.write("lib1/gen.txt", "gen\n");
        rig.write("lib1/data/notes.txt", "data\n");
        rig.write(
            "lib1/src/lib.rs",
            &LIB_SRC.replace("VALUE_PLACEHOLDER", "one"),
        );
        rig.write("baked/src/lib.rs", BAKED_SRC);
        rig.write("stable/src/lib.rs", "pub const VALUE: &str = \"stable\";\n");
        rig.write("app/src/main.rs", APP_SRC);
        rig.write(
            "app/tests/it.rs",
            "#[test]\nfn t() { assert!(!lib1::value().is_empty()); }\n",
        );
        let origin = rig.origin.clone();
        rig.git(&origin, &["add", "-A"]);
        rig.git(&origin, &["commit", "-qm", "one"]);
        rig.one = rig.git(&origin, &["rev-parse", "HEAD"]);
        rig.write(
            "lib1/src/lib.rs",
            &LIB_SRC.replace("VALUE_PLACEHOLDER", "two"),
        );
        rig.git(&origin, &["commit", "-qam", "two"]);
        rig.two = rig.git(&origin, &["rev-parse", "HEAD"]);
        rig
    }

    fn git(&self, cwd: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
            .args(args)
            .current_dir(cwd)
            .env_clear()
            .envs(&self.env)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn write(&self, relative: &str, text: &str) {
        let path = self.origin.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn pools(&self) -> PathBuf {
        self.root.path().join("pools")
    }

    fn sandbox(&self, name: &str, sha: &str) -> PathBuf {
        let path = self.root.path().join(name);
        self.git(
            &self.origin,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                path.to_str().unwrap(),
                sha,
            ],
        );
        fs::create_dir(path.join("target")).unwrap();
        path
    }

    /// One `target-pool` command: `(exit code, stdout, stderr)`, with exactly `env`.
    fn tool(&self, env: &Env, args: &[&str]) -> (i32, String, String) {
        let output = Command::new(guard_binary())
            .arg("target-pool")
            .args(args)
            .current_dir(&self.origin)
            .env_clear()
            .envs(env)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        (
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    /// A pool of commit `one`, built with `env`; its target directory.
    fn pool(&self, env: &Env) -> PathBuf {
        let (code, _, err) = self.tool(
            env,
            &[
                "build",
                "--repo",
                self.origin.to_str().unwrap(),
                "--sha",
                &self.one,
                "--min-free-gib",
                "0",
            ],
        );
        assert_eq!(code, 0, "{err}");
        let markers = markers(&self.pools());
        assert_eq!(markers.len(), 1, "{markers:?}");
        assert_eq!(markers[0]["state"], "ready");
        PathBuf::from(markers[0]["target"].as_str().unwrap())
    }

    fn seed(&self, sandbox: &Path, env: &Env) -> Value {
        let (code, out, err) = self.tool(env, &["seed", "--sandbox", sandbox.to_str().unwrap()]);
        assert_eq!(code, 0, "{err}");
        serde_json::from_str(out.trim()).unwrap()
    }

    fn build_descendant(&self, sha: &str, env: &Env, extra: &[&str]) -> Value {
        let mut args = vec![
            "build",
            "--repo",
            self.origin.to_str().unwrap(),
            "--sha",
            sha,
            "--min-free-gib",
            "0",
            "--keep",
            "4",
        ];
        args.extend_from_slice(extra);
        let (code, _, err) = self.tool(env, &args);
        assert_eq!(code, 0, "{err}");
        let directory = fs::read_dir(self.pools())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let marker = fs::read_dir(&directory)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(&format!("{sha}-"))
                    && p.extension().is_some_and(|e| e == "json")
            })
            .unwrap();
        let record: Value = serde_json::from_str(&fs::read_to_string(marker).unwrap()).unwrap();
        assert_eq!(record["state"], "ready");
        record
    }

    fn cargo(&self, sandbox: &Path, args: &[&str], env: &Env) -> std::process::Child {
        Command::new("cargo")
            .args(args)
            .current_dir(sandbox)
            .env_clear()
            .envs(env)
            .env("CARGO_TARGET_DIR", sandbox.join("target"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    fn finish(child: std::process::Child) -> (i32, String) {
        let output = child.wait_with_output().unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        (output.status.code().unwrap_or(-1), text)
    }

    fn run_app(&self, sandbox: &Path) -> String {
        let output = Command::new(sandbox.join("target/debug/app"))
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn with_wrapper(&self) -> Env {
        let mut env = self.env.clone();
        env.insert(
            "RUSTC_WRAPPER".into(),
            wrapper().to_string_lossy().into_owned(),
        );
        env
    }

    fn incremental_pool_build(&self, env: &Env) {
        let ancestor = self.pool(env);
        let before = digest_tree(&ancestor);
        let record = self.build_descendant(&self.two, env, &[]);
        assert_eq!(record["seed"]["pool_sha"], self.one.as_str());
        assert_eq!(record["seed"]["distance"], 1);
        assert!(record["seed"]["linked"].as_u64().unwrap() > 0);
        assert!(
            record["seed"]["rebuilt_units"]
                .as_array()
                .unwrap()
                .iter()
                .any(|unit| unit.as_str().is_some_and(|u| u.starts_with("baked-")))
        );
        let log = fs::read_to_string(record["log_path"].as_str().unwrap()).unwrap();
        assert_eq!(compiled(&log), ["app", "baked", "lib1"]);
        let target = PathBuf::from(record["target"].as_str().unwrap());
        let stable = fs::read_dir(ancestor.join("debug/deps"))
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| {
                let n = p.file_name().unwrap().to_string_lossy().into_owned();
                n.starts_with("libstable-") && n.ends_with(".rlib")
            })
            .unwrap();
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(
                fs::metadata(&stable).unwrap().ino(),
                fs::metadata(target.join(stable.strip_prefix(&ancestor).unwrap()))
                    .unwrap()
                    .ino()
            );
        }
        assert_eq!(
            digest_tree(&ancestor),
            before,
            "the ancestor pool keeps its bytes"
        );
        let worktree = PathBuf::from(record["worktree"].as_str().unwrap());
        let (code, output) = Self::finish(self.cargo(&worktree, &["build"], env));
        assert_eq!(code, 0, "{output}");
        assert_eq!(
            self.run_app(&worktree),
            format!("gen-two {}", worktree.join("baked").display())
        );
    }

    fn concurrent_different_commits(&self, env: &Env) -> (PathBuf, PathBuf) {
        let pool_target = self.pool(env);
        let before = digest_tree(&pool_target);
        let first = self.sandbox("s1", &self.one);
        let second = self.sandbox("s2", &self.two);
        assert_eq!(self.seed(&first, env)["distance"], 0);
        assert_eq!(self.seed(&second, env)["distance"], 1);
        for args in [&["test", "--workspace", "--no-run"][..], &["build"][..]] {
            let builds = [&first, &second].map(|sandbox| self.cargo(sandbox, args, env));
            for process in builds {
                let (code, output) = Self::finish(process);
                assert_eq!(code, 0, "{}", &output[output.len().saturating_sub(4000)..]);
            }
        }
        assert_eq!(
            self.run_app(&first),
            format!("gen-one {}", first.join("baked").display())
        );
        assert_eq!(
            self.run_app(&second),
            format!("gen-two {}", second.join("baked").display())
        );
        assert_eq!(
            digest_tree(&pool_target),
            before,
            "the pool's files kept their bytes"
        );
        (first, second)
    }
}

/// Every marker (`*/*.json`) under the pool root, in name order.
fn markers(root: &Path) -> Vec<Value> {
    let mut paths: Vec<PathBuf> = fs::read_dir(root)
        .map(|entries| {
            entries
                .flatten()
                .flat_map(|dir| fs::read_dir(dir.path()).into_iter().flatten().flatten())
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "json"))
                .collect()
        })
        .unwrap_or_default();
    paths.sort();
    paths
        .iter()
        .filter_map(|p| serde_json::from_str(&fs::read_to_string(p).ok()?).ok())
        .collect()
}

#[test]
fn same_commit_seed_is_fresh_and_relocates_baked_paths() {
    let rig = Rig::new();
    let pool_target = rig.pool(&rig.env);
    let sandbox = rig.sandbox("s1", &rig.one);
    let outcome = rig.seed(&sandbox, &rig.env);
    assert_eq!(outcome["result"], "seeded");
    assert_eq!(outcome["distance"], 0);
    let (code, output) = Rig::finish(rig.cargo(&sandbox, &["build"], &rig.env));
    assert_eq!(code, 0, "{output}");
    // Only the crate that bakes CARGO_MANIFEST_DIR (and its dependent) rebuild.
    assert_eq!(compiled(&output), ["app", "baked"], "{output}");
    // The rebuilt program reports this sandbox's own directory.
    assert_eq!(
        rig.run_app(&sandbox),
        format!("gen-one {}", sandbox.join("baked").display())
    );
    assert!(pool_target.is_dir());
}

#[test]
fn pool_build_reuses_ancestor_and_preserves_its_artifacts() {
    let rig = Rig::new();
    rig.incremental_pool_build(&rig.env);
}

#[test]
fn pool_build_reuses_shared_executables_with_wrapper() {
    let rig = Rig::new();
    rig.incremental_pool_build(&rig.with_wrapper());
}

#[test]
fn pool_build_chooses_nearest_ready_ancestor() {
    let rig = Rig::new();
    rig.pool(&rig.env);
    let second = rig.build_descendant(&rig.two, &rig.env, &[]);
    rig.write("notes.txt", "third commit\n");
    rig.git(&rig.origin, &["add", "notes.txt"]);
    rig.git(&rig.origin, &["commit", "-qm", "three"]);
    let head = rig.git(&rig.origin, &["rev-parse", "HEAD"]);
    let third = rig.build_descendant(&head, &rig.env, &[]);
    assert_eq!(third["seed"]["pool"], second["name"]);
    assert_eq!(third["seed"]["distance"], 1);
    let log = fs::read_to_string(third["log_path"].as_str().unwrap()).unwrap();
    assert_eq!(compiled(&log), ["app", "baked"]);
}

#[test]
fn pool_build_is_cold_without_a_compatible_nearby_ancestor() {
    let rig = Rig::new();
    rig.pool(&rig.env);
    let mut full_debug = rig.env.clone();
    full_debug.insert("CARGO_PROFILE_DEV_DEBUG".into(), "full".into());
    // The two builds have distinct keys; each starts without a usable ancestor.
    for (label, env, options) in [
        (
            "max-distance 0",
            rig.env.clone(),
            vec!["--max-distance", "0"],
        ),
        ("different profile", full_debug, vec![]),
    ] {
        let record = rig.build_descendant(&rig.two, &env, &options);
        assert!(record["seed"].is_null(), "{label}: {record}");
        let log = fs::read_to_string(record["log_path"].as_str().unwrap()).unwrap();
        assert_eq!(
            compiled(&log),
            ["app", "baked", "lib1", "stable"],
            "{label}"
        );
    }
}

#[test]
fn pool_build_is_cold_when_ready_pool_is_a_descendant() {
    let rig = Rig::new();
    rig.build_descendant(&rig.two, &rig.env, &[]);
    let record = rig.build_descendant(&rig.one, &rig.env, &[]);
    assert!(record["seed"].is_null(), "{record}");
    let log = fs::read_to_string(record["log_path"].as_str().unwrap()).unwrap();
    assert_eq!(compiled(&log), ["app", "baked", "lib1", "stable"]);
}

#[test]
fn clobber_two_sandboxes_on_different_commits_build_concurrently() {
    let rig = Rig::new();
    rig.concurrent_different_commits(&rig.env);
}

#[test]
fn clobber_with_shared_executables_behind_the_wrapper() {
    use std::os::unix::fs::MetadataExt;
    let rig = Rig::new();
    let (first, _second) = rig.concurrent_different_commits(&rig.with_wrapper());
    // lib1's unchanged test executable is still one inode with the pool's.
    let shared: Vec<PathBuf> = fs::read_dir(first.join("target/debug/deps"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let n = p.file_name().unwrap().to_string_lossy().into_owned();
            n.starts_with("lib1-") && !n.contains('.')
        })
        .collect();
    assert!(!shared.is_empty());
    assert!(
        shared.iter().all(|p| fs::metadata(p).unwrap().nlink() > 1),
        "{shared:?}"
    );
}

#[test]
fn shared_executables_fail_closed_without_the_wrapper() {
    let rig = Rig::new();
    let wrapped = rig.with_wrapper();
    let pool_target = rig.pool(&wrapped);
    let before = digest_tree(&pool_target);
    let second = rig.sandbox("s2", &rig.two);
    rig.seed(&second, &wrapped);
    // The sandbox builds without the wrapper that unlinks outputs (the pool's key does
    // not change that; the build env does).
    let (code, output) =
        Rig::finish(rig.cargo(&second, &["test", "--workspace", "--no-run"], &rig.env));
    // rustc refuses the read-only output ("not writeable"), LLVM cannot open a read-only
    // split-debuginfo file, or (macOS, where the unit's object files stay) rustc cannot
    // write one ("could not write output to ... Permission denied"); either way the build stops and the shared files
    // keep their bytes.
    assert_ne!(code, 0, "{output}");
    assert!(
        output.contains("not writeable")
            || output.contains("IO failure on output stream")
            || output.contains("could not write output to") && output.contains("Permission denied"),
        "{}",
        &output[output.len().saturating_sub(3000)..]
    );
    let after = digest_tree(&pool_target);
    let changed: Vec<&String> = after
        .iter()
        .filter(|(path, digest)| before.get(*path) != Some(*digest))
        .map(|(path, _)| path)
        .chain(before.keys().filter(|path| !after.contains_key(*path)))
        .collect();
    assert!(changed.is_empty(), "the pool's files changed: {changed:?}");
}

#[test]
fn pool_environment_starts_sccache_through_wrapper_with_long_pool_path() {
    let Some(cache) = std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join("sccache"))
            .find(|candidate| candidate.is_file())
    }) else {
        eprintln!("skipped: sccache is not installed");
        return;
    };
    let rig = Rig::new();
    // A long pool root: the pool's scratch directory moves to a short one (the build
    // environment's TMPDIR must leave room for sccache's socket name), and the build
    // starts sccache through the wrapper with a private server socket.
    let long_pool = rig
        .root
        .path()
        .join(format!("long-pool-{}", "x".repeat(100)));
    fs::create_dir(&long_pool).unwrap();
    let private = tempfile::Builder::new()
        .prefix("tp-sccache-")
        .tempdir_in("/tmp")
        .unwrap();
    let mut env = rig.with_wrapper();
    env.insert(
        "RSI_TARGET_POOL_ROOT".into(),
        long_pool.join("pools").to_string_lossy().into_owned(),
    );
    for (name, value) in [
        ("RSI_BUILD_SCCACHE", cache.to_string_lossy().into_owned()),
        ("SCCACHE_CLIENT_SIDE", "1".to_string()),
        (
            "SCCACHE_SERVER_UDS",
            private
                .path()
                .join("cache.sock")
                .to_string_lossy()
                .into_owned(),
        ),
        (
            "SCCACHE_DIR",
            private.path().join("cache").to_string_lossy().into_owned(),
        ),
        (
            "RSI_BUILD_SLOT_ROOT",
            private.path().join("slots").to_string_lossy().into_owned(),
        ),
        ("TMPDIR", long_pool.to_string_lossy().into_owned()),
    ] {
        env.insert(name.into(), value);
    }
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let (code, _, err) = rig.tool(
            &env,
            &[
                "build",
                "--repo",
                rig.origin.to_str().unwrap(),
                "--sha",
                &rig.one,
                "--min-free-gib",
                "0",
            ],
        );
        assert_eq!(code, 0, "{err}");
        let sandbox = rig.sandbox("s1", &rig.one);
        let seeded = rig.seed(&sandbox, &env);
        assert_eq!(seeded["result"], "seeded");
        assert_eq!(seeded["head"], rig.one.as_str());
        assert_eq!(seeded["pool_sha"], rig.one.as_str());
    }));
    // Stop only this test's private cache server.
    let _ = Command::new(&cache)
        .arg("--stop-server")
        .env_clear()
        .envs(&env)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}
