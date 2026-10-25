//! Shared fixture of the `install-release.sh` stage and link tests (#3771).
//!
//! A throwaway checkout (a copy of `scripts/` plus a stub workspace), a fake
//! `cargo`, a pre-built fake release directory and a temp HOME: no real build,
//! daemon, launchd or systemd is touched. See `isolation` for the fail-closed
//! lifecycle boundary; HOME alone does not isolate the host's service manager.
#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

pub mod isolation;

/// Members a cargo build produces and the fixture pre-builds. `rsi-cloud` is a
/// `DEPLOY_BINARIES` member (#3771), so a staged directory must carry it.
pub const RELEASE_BINS: [&str; 10] = [
    "rsi",
    "rsid",
    "rsi-rpc",
    "rsi-agent-mcp",
    "rsi-build-rustc",
    "rsi-contract-validate",
    "rsi-known-failure",
    "rsi-turn-shim",
    "rsi-socket-hold",
    "rsi-cloud",
];

/// The `DEPLOY_BINARIES` members that are repo scripts; the rest come from cargo.
pub const DEPLOY_SCRIPTS: [&str; 4] = [
    "rsid-supervisor.sh",
    "cargo-slot",
    "target-pool",
    "rsi-spill",
];

pub fn repo_root() -> PathBuf {
    Path::new(rsi_common::manifest_dir!()).join("../..")
}

pub fn text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

pub fn write_executable(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

pub fn is_executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|meta| meta.permissions().mode() & 0o111 != 0)
}

/// A regular file (not a symlink), as the deploy consumer requires of a member.
pub fn is_plain_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_file())
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn names(path: &Path) -> Vec<String> {
    let mut found: Vec<String> = fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    found.sort();
    found
}

/// Names under `dir` that start with `prefix` (leftover atomic-copy temp files).
pub fn dotted(dir: &Path, prefix: &str) -> Vec<String> {
    names(dir)
        .into_iter()
        .filter(|name| name.starts_with(prefix))
        .collect()
}

/// Every path under a root with its kind, mode, link target and content.
#[derive(Debug, PartialEq)]
pub enum Entry {
    Missing,
    Link(PathBuf),
    File {
        bytes: Vec<u8>,
        mtime_ns: i64,
        mode: u32,
    },
    Dir(u32),
}

pub fn snapshot(roots: &[&Path]) -> BTreeMap<String, Entry> {
    fn walk(path: &Path, state: &mut BTreeMap<String, Entry>) {
        let meta = fs::symlink_metadata(path).unwrap();
        let entry = if meta.file_type().is_symlink() {
            Entry::Link(fs::read_link(path).unwrap())
        } else if meta.is_file() {
            Entry::File {
                bytes: fs::read(path).unwrap(),
                mtime_ns: meta.mtime() * 1_000_000_000 + meta.mtime_nsec(),
                mode: meta.mode(),
            }
        } else {
            Entry::Dir(meta.mode())
        };
        let is_dir = matches!(entry, Entry::Dir(_));
        state.insert(text(path), entry);
        if is_dir {
            let mut children: Vec<PathBuf> = fs::read_dir(path)
                .unwrap()
                .map(|child| child.unwrap().path())
                .collect();
            children.sort();
            for child in children {
                walk(&child, state);
            }
        }
    }
    let mut state = BTreeMap::new();
    for root in roots {
        if fs::symlink_metadata(root).is_err() {
            state.insert(text(root), Entry::Missing);
        } else {
            walk(root, &mut state);
        }
    }
    state
}

/// The repository's `scripts/` without its test directory.
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

/// The outcome of one script run.
pub struct Run {
    pub code: i32,
    pub out: String,
    pub err: String,
}

impl Run {
    pub fn ok(&self) -> bool {
        self.code == 0
    }
}

/// Run a command to completion within `limit`; a hang kills it and fails the test.
pub fn run_bounded(mut command: Command, limit: Duration) -> Run {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let out = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let err = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf);
        buf
    });
    let deadline = Instant::now() + limit;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "command timed out after {limit:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    Run {
        code: status.code().unwrap_or(-1),
        out: String::from_utf8_lossy(&out.join().unwrap()).into_owned(),
        err: String::from_utf8_lossy(&err.join().unwrap()).into_owned(),
    }
}

pub struct Fixture {
    _temp: tempfile::TempDir,
    pub base: PathBuf,
    pub home: PathBuf,
    pub rsi_home: PathBuf,
    pub bin_dir: PathBuf,
    pub root: PathBuf,
    pub target: PathBuf,
    pub fakes: PathBuf,
    /// A fake restart or supervisor command records its name here; it stays empty.
    pub called: PathBuf,
    /// `cargo build` is recorded here (not in `called`, which must stay empty).
    pub cargo_builds: PathBuf,
    pub stage: PathBuf,
    env: HashMap<String, String>,
}

impl Fixture {
    pub fn new(prefix: &str) -> Fixture {
        let temp = tempfile::Builder::new()
            .prefix(prefix)
            // Outside this checkout: a fixture git must never find it.
            .tempdir_in("/tmp")
            .unwrap();
        let base = fs::canonicalize(temp.path()).unwrap();
        let home = base.join("home");
        let rsi_home = home.join(".rsi");
        let bin_dir = home.join(".local/bin");
        let root = base.join("repo");
        let target = base.join("target");
        let fakes = base.join("fakes");
        let called = base.join("called");
        let cargo_builds = base.join("cargo-builds");
        let stage = base.join("stage");
        fs::create_dir(&called).unwrap();
        fs::create_dir(&cargo_builds).unwrap();

        write_executable(
            &fakes.join("cargo"),
            &format!(
                "#!/bin/sh\n[ \"$1\" = build ] && touch \"{}/build\"\n\
                 printf '{{\"target_directory\":\"%s\"}}\\n' \"$CARGO_TARGET_DIR\"\n",
                cargo_builds.display()
            ),
        );
        write_executable(&fakes.join("rustup"), "#!/bin/sh\nexit 99\n");
        isolation::install(&base, &fakes);
        copy_scripts(&repo_root().join("scripts"), &root.join("scripts"), true);
        fs::write(
            root.join("Cargo.toml"),
            "[workspace]\n[package]\nname = \"stub\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        write_executable(&root.join("src/lib.rs"), "");
        for name in RELEASE_BINS {
            write_executable(
                &target.join("release").join(name),
                &format!("#!/bin/sh\necho \"fake-{name} $*\"\n"),
            );
        }
        // #938: --link-only links only what this checkout built.
        fs::write(
            target.join("release/.rsi-release-source"),
            format!(
                "worktree={}\nstate=built\nhead=unknown\n",
                fs::canonicalize(&root).unwrap().display()
            ),
        )
        .unwrap();

        let mut env: HashMap<String, String> = HashMap::new();
        env.insert("HOME".into(), text(&home));
        env.insert("CARGO_TARGET_DIR".into(), text(&target));
        // The guard executable is the binary built for this integration test.
        // Tests must not depend on an installed ~/.rsi/bin copy or PATH state.
        env.insert(
            "RSI_REPO_GUARD_BIN".into(),
            env!("CARGO_BIN_EXE_rsi-repo-guard").into(),
        );
        Fixture {
            _temp: temp,
            base,
            home,
            rsi_home,
            bin_dir,
            root,
            target,
            fakes,
            called,
            cargo_builds,
            stage,
            env,
        }
    }

    /// A pre-existing live install that a staging run must leave alone.
    pub fn plant_live_install(&self) {
        write_executable(
            &self.rsi_home.join("install/rsid"),
            "#!/bin/sh\necho live-rsid\n",
        );
        write_executable(
            &self.rsi_home.join("install/rsid-supervisor.sh"),
            "#!/bin/sh\necho live-supervisor\n",
        );
        write_executable(
            &self.rsi_home.join("bin/cargo-slot"),
            "#!/bin/sh\necho live-wrapper\n",
        );
        write_executable(
            &self.rsi_home.join("bin/target-pool"),
            "#!/bin/sh\necho live-pool\n",
        );
        write_executable(
            &self.rsi_home.join("lib/cargo-slot"),
            "#!/bin/sh\necho live-slot\n",
        );
        fs::create_dir_all(&self.bin_dir).unwrap();
        symlink(
            self.rsi_home.join("install/rsid"),
            self.bin_dir.join("rsid"),
        )
        .unwrap();
    }

    pub fn live_state(&self) -> BTreeMap<String, Entry> {
        snapshot(&[&self.rsi_home, &self.bin_dir, &self.root.join("target")])
    }

    pub fn release(&self, name: &str) -> PathBuf {
        self.target.join("release").join(name)
    }

    pub fn script(&self, name: &str) -> PathBuf {
        self.root.join("scripts").join(name)
    }

    /// `install-release.sh <args>` with HOME and the cargo target in the fixture.
    pub fn run_script(&self, args: &[&str], extra_env: &[(&str, &str)]) -> Run {
        self.run_bash(
            &[&text(&self.script("install-release.sh"))],
            args,
            extra_env,
        )
    }

    /// Also used by inert boundary regressions; the same entry checks apply.
    pub fn run_bash(&self, prefix: &[&str], args: &[&str], extra_env: &[(&str, &str)]) -> Run {
        let mut command = Command::new("/bin/bash");
        command
            .args(prefix)
            .args(args)
            .env_clear()
            .envs(&self.env)
            .envs(extra_env.iter().copied())
            .current_dir(&self.base);
        isolation::pin(&mut command, &self.base, &self.fakes, &self.home);
        run_bounded(command, Duration::from_secs(120))
    }

    /// Like [`Fixture::run_script`], failing the test on a non-zero exit.
    pub fn run_ok(&self, args: &[&str], extra_env: &[(&str, &str)]) -> Run {
        let run = self.run_script(args, extra_env);
        assert!(
            run.ok(),
            "install-release.sh {args:?} exited {}:\n{}\n{}",
            run.code,
            run.out,
            run.err
        );
        run
    }
}
