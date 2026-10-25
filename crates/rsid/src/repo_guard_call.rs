//! #361: run the Python-free repository guards (`rsi-repo-guard`) as a bounded
//! child process.
//!
//! The lander and the queue probe used to start `python3
//! tools/check-released-migrations.py` and `tools/rolling-migration-renumber.py`.
//! Their replacements are guards of the `rsi-repo-guards` crate, run through its
//! `rsi-repo-guard` executable: `<exe> <guard> <args...>`.
//!
//! This crate does NOT link `rsi-repo-guards`. That crate enables
//! `serde_json/preserve_order`, and Cargo unifies a dependency's features across
//! one build: linking it would reorder every `serde_json::Map` in the daemon,
//! the lander and their tests and move the canonical digests and stored JSON
//! that depend on sorted keys (the bundled starter digests changed). The guard
//! stays a separate executable built in its own invocation.
//!
//! Trust: the executable is never a file of the repository or of a candidate.
//! [`trusted_tool`] answers for a repository and the `target` commit the guard
//! judges against with the `rsi-repo-guard` installed beside the running daemon
//! or lander binary; none installed means no guard can run and a caller must
//! not guess (fail closed). An identity record beside the executable
//! (`<exe>.identity.json`, [`check_identity`]) binds its digest and the complete
//! tree it was built from: when one exists the tool is trusted only for a target
//! with that tree, and any mismatch refuses. Hub #3621's materializer writes the
//! record and replaces [`trusted_tool`]'s answer, not its callers.

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The guard executable's file name (beside the daemon and lander binaries).
pub const GUARD_BIN: &str = "rsi-repo-guard";
/// The most stdout or stderr one guard run may return; more is a failed run.
pub const MAX_GUARD_OUTPUT: u64 = 8 << 20;

/// What one guard run did.
#[derive(Debug)]
pub enum GuardRun {
    /// It exited with `code`; `stderr` is lossy UTF-8.
    Exited {
        code: i32,
        stdout: Vec<u8>,
        stderr: String,
    },
    /// The deadline passed; the child's process group was killed and reaped.
    TimedOut,
    /// It could not run: the executable would not start, was killed by a
    /// signal, or answered more than [`MAX_GUARD_OUTPUT`].
    Failed(String),
}

impl GuardRun {
    /// Exit code 0.
    #[must_use]
    pub const fn success(&self) -> bool {
        matches!(self, Self::Exited { code: 0, .. })
    }

    /// The text a refusal carries: stderr of an exited run, else the reason it
    /// did not finish.
    #[must_use]
    pub fn reason(&self) -> String {
        match self {
            Self::Exited { stderr, .. } => stderr.trim().to_owned(),
            Self::TimedOut => "the guard did not finish before its deadline".to_owned(),
            Self::Failed(reason) => reason.clone(),
        }
    }
}

#[cfg(test)]
static TEST_TOOLS: std::sync::Mutex<Vec<(PathBuf, PathBuf)>> = std::sync::Mutex::new(Vec::new());

/// Test seam: answer [`trusted_tool`] for `repo` (compared by canonical path).
#[cfg(test)]
pub fn register_test_tool(repo: &Path, exe: PathBuf) {
    let key = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());
    let mut tools = TEST_TOOLS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    tools.retain(|(path, _)| path != &key);
    tools.push((key, exe));
}

/// The suffix of the identity record a materializer writes beside a guard it
/// built for one target: `<exe>.identity.json`.
pub const IDENTITY_SUFFIX: &str = ".identity.json";

/// What an executable's identity record says about it (#361, hub #3621).
#[derive(Debug, PartialEq, Eq)]
pub enum Identity {
    /// No record: only the interim installed-beside-the-binary trust applies.
    Absent,
    /// The record matches the binary and, when asked, the target's tree.
    Verified,
    /// A record exists and does not authenticate this executable for this use.
    Refused(String),
}

/// The record's path for `exe`.
#[must_use]
pub fn identity_path(exe: &Path) -> PathBuf {
    let mut path = exe.as_os_str().to_owned();
    path.push(IDENTITY_SUFFIX);
    PathBuf::from(path)
}

fn is_hex(text: &str, lengths: &[usize]) -> bool {
    lengths.contains(&text.len())
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Check `exe` against its identity record `{"schema":1,"commit","tree",
/// "binary_sha256"}`: the digest must be the executable's own, and, when
/// `target_tree` is given, the record's `tree` (the complete tree the guard was
/// built from, which binds every input blob) must be the target commit's tree.
/// A record that exists but cannot be read, parsed or matched is
/// [`Identity::Refused`]: an authenticated tool never degrades to an
/// unauthenticated one.
#[must_use]
pub fn check_identity(exe: &Path, target_tree: Option<&str>) -> Identity {
    use sha2::{Digest, Sha256};
    let text = match std::fs::read_to_string(identity_path(exe)) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Identity::Absent,
        Err(error) => return Identity::Refused(format!("identity record unreadable: {error}")),
    };
    let record: serde_json::Value = match serde_json::from_str(&text) {
        Ok(record) => record,
        Err(error) => return Identity::Refused(format!("identity record is not JSON: {error}")),
    };
    let field = |name: &str| record[name].as_str().unwrap_or_default().to_owned();
    let (commit, tree, digest) = (field("commit"), field("tree"), field("binary_sha256"));
    if record["schema"] != 1
        || !is_hex(&commit, &[40, 64])
        || !is_hex(&tree, &[40, 64])
        || !is_hex(&digest, &[64])
    {
        return Identity::Refused("identity record is malformed".to_owned());
    }
    let mut hasher = Sha256::new();
    let hashed =
        std::fs::File::open(exe).and_then(|mut file| std::io::copy(&mut file, &mut hasher));
    if let Err(error) = hashed {
        return Identity::Refused(format!("cannot read the executable: {error}"));
    }
    if format!("{:x}", hasher.finalize()) != digest {
        return Identity::Refused("the executable does not match its recorded digest".to_owned());
    }
    match target_tree {
        Some(target) if target != tree => Identity::Refused(format!(
            "built from tree {tree}, not the target's tree {target}"
        )),
        _ => Identity::Verified,
    }
}

/// The tree object id of `commit` in `repo`, read with plain git.
fn tree_of(repo: &Path, commit: &str) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{commit}^{{tree}}"),
        ])
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8(output.stdout).ok()?;
    (output.status.success() && is_hex(text.trim(), &[40, 64])).then(|| text.trim().to_owned())
}

/// `exe` if it may judge work on `target` in `repo`. With no identity record
/// (the interim installed tool) it is trusted as installed; with one it must
/// authenticate for the target's tree, and a target that has no readable tree
/// is refused rather than guessed.
fn authenticated(exe: PathBuf, repo: &Path, target: &str) -> Option<PathBuf> {
    if !identity_path(&exe).exists() {
        return Some(exe);
    }
    let tree = tree_of(repo, target)?;
    (check_identity(&exe, Some(&tree)) == Identity::Verified).then_some(exe)
}

/// The executable that runs the guards for work on `repo` judged against
/// `target` (a commit of `repo`), if one is trusted. The target is what hub
/// #3621's materializer builds the guard for: it replaces this answer, not its
/// callers.
#[must_use]
pub fn trusted_tool(repo: &Path, target: &str) -> Option<PathBuf> {
    #[cfg(test)]
    {
        let _ = target;
        let key = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());
        let tools = TEST_TOOLS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        return tools
            .iter()
            .find(|(path, _)| path == &key)
            .map(|(_, exe)| exe.clone());
    }
    #[cfg(not(test))]
    {
        authenticated(installed_tool()?, repo, target)
    }
}

/// The `rsi-repo-guard` installed beside the running executable.
#[must_use]
pub fn installed_tool() -> Option<PathBuf> {
    installed_tool_in(std::env::current_exe().ok()?.parent()?).ok()
}

/// Installed-tool lookup failures. A missing guard and an untrusted location
/// are distinct, named refusals; deploy preserves these in its failure reason.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum InstalledToolError {
    #[error("guard_home_unresolvable: cannot resolve the operator's home directory")]
    HomeUnresolvable,
    #[error("guard_unresolvable: cannot resolve a regular installed file at {}", .0.display())]
    Unresolvable(PathBuf),
    #[error("guard_untrusted_path: {} is outside the operator install locations", .0.display())]
    UntrustedPath(PathBuf),
}

/// The `rsi-repo-guard` installed in `dir`, the directory of a daemon or lander
/// binary: what [`installed_tool`] answers for a binary that runs from `dir`. A
/// deploy asks this of the install directory of the build it is putting live
/// (hub #3886), so its verdict and the running binary's cannot differ.
///
/// Only the exact canonical guard paths in `~/.rsi/install` and `~/.local/bin`
/// qualify (#3911). This permits the hub's transitional hand symlink between
/// those directories, but never a sandbox/target/PATH executable. The allowed
/// directories themselves are NOT canonicalized into new trust roots: a
/// redirected install directory must fail closed too. No identity record can
/// grant an executable outside these locations installed-tool trust.
///
/// # Errors
/// A named [`InstalledToolError`] when the location is untrusted or missing.
#[must_use]
pub fn installed_tool_in(dir: &Path) -> Result<PathBuf, InstalledToolError> {
    let home = dirs::home_dir().ok_or(InstalledToolError::HomeUnresolvable)?;
    installed_tool_in_home(dir, &home)
}

/// Validate the directory even when staging will replace the guard itself.
/// Resolve the home anchor (e.g. macOS `/var` -> `/private/var`), but require
/// the install directories below it to retain their exact protected paths.
pub(crate) fn installed_guard_dir_in_home(
    dir: &Path,
    home: &Path,
) -> Result<PathBuf, InstalledToolError> {
    let home = home
        .canonicalize()
        .map_err(|_| InstalledToolError::HomeUnresolvable)?;
    let resolved = dir
        .canonicalize()
        .map_err(|_| InstalledToolError::Unresolvable(dir.to_path_buf()))?;
    if resolved != home.join(".rsi/install") && resolved != home.join(".local/bin") {
        return Err(InstalledToolError::UntrustedPath(resolved));
    }
    Ok(resolved)
}

/// Explicit home anchor for hermetic deploy/lookup fixtures; production uses
/// the daemon's home, never the deploy source, target or requested install dir.
pub(crate) fn installed_tool_in_home(
    dir: &Path,
    home: &Path,
) -> Result<PathBuf, InstalledToolError> {
    installed_guard_dir_in_home(dir, home)?;
    let sibling = dir.join(format!("{GUARD_BIN}{}", std::env::consts::EXE_SUFFIX));
    let resolved = sibling
        .canonicalize()
        .map_err(|_| InstalledToolError::Unresolvable(sibling.clone()))?;
    installed_guard_dir_in_home(
        resolved
            .parent()
            .ok_or_else(|| InstalledToolError::UntrustedPath(resolved.clone()))?,
        home,
    )?;
    if resolved.file_name() != sibling.file_name() {
        return Err(InstalledToolError::UntrustedPath(resolved));
    }
    if !resolved.is_file() {
        return Err(InstalledToolError::Unresolvable(resolved));
    }
    // Identity is optional, but an agent-writable record must not authenticate
    // the operator's binary for a tree selected by the agent.
    let record = identity_path(&resolved);
    match record.canonicalize() {
        Ok(path) if path == record => {}
        Ok(path) => return Err(InstalledToolError::UntrustedPath(path)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if std::fs::symlink_metadata(&record).is_ok() {
                return Err(InstalledToolError::Unresolvable(record));
            }
        }
        Err(_) => return Err(InstalledToolError::Unresolvable(record)),
    }
    Ok(resolved)
}

/// Run `<exe> <guard> <args...>` in `cwd` with exactly the environment `env`,
/// until it exits or `timeout` passes. On a timeout the child's whole process
/// group is killed and reaped. Output is bounded by [`MAX_GUARD_OUTPUT`].
#[must_use]
pub fn run_guard(
    exe: &Path,
    guard: &str,
    args: &[&str],
    cwd: &Path,
    env: impl IntoIterator<Item = (OsString, OsString)>,
    timeout: Duration,
) -> GuardRun {
    let mut command = Command::new(exe);
    command
        .arg(guard)
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return GuardRun::Failed(format!("cannot start {}: {error}", exe.display())),
    };
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => break None,
        }
    };
    if status.is_none() {
        kill_group(&mut child);
    }
    let (stdout, stderr) = (
        stdout.join().unwrap_or_default(),
        stderr.join().unwrap_or_default(),
    );
    let Some(status) = status else {
        return GuardRun::TimedOut;
    };
    if stdout.len() as u64 > MAX_GUARD_OUTPUT || stderr.len() as u64 > MAX_GUARD_OUTPUT {
        return GuardRun::Failed("the guard printed more than the output bound".to_owned());
    }
    match status.code() {
        Some(code) => GuardRun::Exited {
            code,
            stdout,
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        },
        None => GuardRun::Failed(format!("the guard was killed by a signal ({status})")),
    }
}

/// Read at most the output bound from `pipe` on its own thread.
fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(pipe) = pipe {
            let _ = pipe.take(MAX_GUARD_OUTPUT + 1).read_to_end(&mut bytes);
        }
        bytes
    })
}

#[cfg(unix)]
fn kill_group(child: &mut std::process::Child) {
    // SAFETY: `kill` takes a pid and a signal; the group id is the child's pid
    // because it was started with `process_group(0)`.
    unsafe {
        nix::libc::kill(-(child.id() as i32), nix::libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(not(unix))]
fn kill_group(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn script(dir: &Path, body: &str) -> PathBuf {
        let exe = dir.join("rsi-repo-guard");
        std::fs::write(&exe, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        exe
    }

    fn env() -> Vec<(OsString, OsString)> {
        vec![("PATH".into(), std::env::var_os("PATH").unwrap_or_default())]
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_guard_run_returns_its_exit_code_stdout_and_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let exe = script(dir.path(), "echo \"$1 $2\"; echo refused >&2; exit 2");
        let run = run_guard(
            &exe,
            "released-migrations",
            &["a"],
            dir.path(),
            env(),
            Duration::from_secs(10),
        );
        match run {
            GuardRun::Exited {
                code,
                stdout,
                stderr,
            } => {
                assert_eq!(code, 2);
                assert_eq!(stdout, b"released-migrations a\n");
                assert_eq!(stderr.trim(), "refused");
            }
            other => panic!("{other:?}"),
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_guard_that_outruns_its_deadline_is_killed_with_its_children() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("grandchild-survived");
        let exe = script(
            dir.path(),
            &format!("(sleep 3; : > {}) &\nsleep 30", marker.display()),
        );
        let started = Instant::now();
        let run = run_guard(
            &exe,
            "x",
            &[],
            dir.path(),
            env(),
            Duration::from_millis(300),
        );
        assert!(matches!(run, GuardRun::TimedOut), "{run:?}");
        assert!(started.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(3500));
        assert!(!marker.exists(), "a descendant outlived the kill");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_missing_executable_is_a_failed_run_not_a_refusal() {
        let run = run_guard(
            Path::new("/nonexistent/rsi-repo-guard"),
            "x",
            &[],
            Path::new("/"),
            env(),
            Duration::from_secs(1),
        );
        assert!(matches!(run, GuardRun::Failed(_)), "{run:?}");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_child_sees_only_the_environment_it_is_given() {
        let dir = tempfile::tempdir().unwrap();
        let exe = script(dir.path(), "printf '%s' \"${RSI_SESSION_TOKEN:-none}\"");
        let run = run_guard(&exe, "x", &[], dir.path(), env(), Duration::from_secs(10));
        let GuardRun::Exited { stdout, .. } = run else {
            panic!("{run:?}")
        };
        assert_eq!(stdout, b"none");
    }

    /// Whether `pid` names a running process. Linux can briefly retain a killed
    /// orphan as a zombie until its adopter reaps it; that process is already
    /// stopped and cannot outlive the guard deadline.
    fn process_running(pid: &str) -> bool {
        #[cfg(target_os = "linux")]
        {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"));
            stat.is_ok_and(|stat| {
                stat.rsplit_once(") ")
                    .is_some_and(|(_, rest)| !rest.starts_with('Z'))
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            Command::new("kill")
                .args(["-0", pid])
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_timed_out_guard_leaves_neither_it_nor_its_descendants_running() {
        let dir = tempfile::tempdir().unwrap();
        let exe = script(
            dir.path(),
            "echo $$ > guard.pid\nsleep 300 &\necho $! > child.pid\nwait",
        );
        let run = run_guard(
            &exe,
            "x",
            &[],
            dir.path(),
            env(),
            Duration::from_millis(500),
        );
        assert!(matches!(run, GuardRun::TimedOut), "{run:?}");
        for name in ["guard.pid", "child.pid"] {
            let pid = std::fs::read_to_string(dir.path().join(name)).unwrap();
            let pid = pid.trim();
            // The guard process is waited by run_guard. Descendants are killed
            // as a group; Linux may expose an already-dead orphan as a zombie
            // briefly while its adopting init reaps it.
            let gone = (0..50).any(|_| {
                if process_running(pid) {
                    std::thread::sleep(Duration::from_millis(100));
                    false
                } else {
                    true
                }
            });
            assert!(gone, "{name} ({pid}) outlived the deadline kill");
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_guard_that_prints_more_than_the_output_bound_is_a_failed_run() {
        let dir = tempfile::tempdir().unwrap();
        let more = MAX_GUARD_OUTPUT + (1 << 20);
        for redirect in ["", ">&2"] {
            let exe = script(
                dir.path(),
                &format!("head -c {more} /dev/zero | tr '\\0' x {redirect}"),
            );
            let run = run_guard(&exe, "x", &[], dir.path(), env(), Duration::from_secs(60));
            let GuardRun::Failed(reason) = run else {
                panic!("{redirect:?}: {run:?}")
            };
            assert!(reason.contains("output bound"), "{reason}");
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_guard_output_within_the_bound_is_returned_whole() {
        let dir = tempfile::tempdir().unwrap();
        let exe = script(dir.path(), "head -c 1048576 /dev/zero | tr '\\0' x");
        let run = run_guard(&exe, "x", &[], dir.path(), env(), Duration::from_secs(60));
        let GuardRun::Exited { code, stdout, .. } = run else {
            panic!("{run:?}")
        };
        assert_eq!((code, stdout.len()), (0, 1 << 20));
    }

    fn record(exe: &Path, tree: &str, digest: &str) {
        std::fs::write(
            identity_path(exe),
            format!(
                "{{\"schema\":1,\"commit\":\"{}\",\"tree\":\"{tree}\",\"binary_sha256\":\"{digest}\"}}",
                "c".repeat(40)
            ),
        )
        .unwrap();
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        format!("{:x}", Sha256::digest(bytes))
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn installed_guard_accepts_operator_files_and_the_transitional_hand_symlink_3911() {
        let dir = crate::test_support::disk_backed_tempdir("guard-operator-layout");
        let home = dir.path().canonicalize().unwrap();
        let install = home.join(".rsi/install");
        let bin = home.join(".local/bin");
        for path in [&install, &bin] {
            std::fs::create_dir_all(path).unwrap();
        }
        let exe = script(&install, "exit 0");
        assert_eq!(installed_tool_in_home(&install, &home), Ok(exe.clone()));
        assert_eq!(check_identity(&exe, None), Identity::Absent);
        std::fs::remove_file(&exe).unwrap();
        let target = script(&bin, "exit 0");
        // The current hub layout, including a relative link, remains trusted.
        std::os::unix::fs::symlink("../../.local/bin/rsi-repo-guard", &exe).unwrap();
        assert_eq!(installed_tool_in_home(&install, &home), Ok(target.clone()));
        assert_eq!(installed_tool_in_home(&bin, &home), Ok(target.clone()));
        record(
            &target,
            &"a".repeat(40),
            &sha256_hex(&std::fs::read(&target).unwrap()),
        );
        assert_eq!(installed_tool_in_home(&install, &home), Ok(target.clone()));
        assert_eq!(check_identity(&target, None), Identity::Verified);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn installed_guard_refuses_writable_targets_even_with_a_matching_identity_3911() {
        let dir = crate::test_support::disk_backed_tempdir("guard-writable-target");
        let home = dir.path().canonicalize().unwrap();
        let install = home.join(".rsi/install");
        std::fs::create_dir_all(&install).unwrap();
        for location in [
            ".rsi/sandboxes/worker/target/release",
            "target/release",
            "other",
        ] {
            let writable = home.join(location);
            std::fs::create_dir_all(&writable).unwrap();
            let exe = script(&writable, "exit 0");
            let sibling = install.join(GUARD_BIN);
            std::os::unix::fs::symlink(&exe, &sibling).unwrap();
            let refusal = InstalledToolError::UntrustedPath(writable.clone());
            assert_eq!(installed_tool_in_home(&install, &home), Err(refusal));
            // A self-authored digest/tree record cannot grant installed trust.
            record(
                &exe,
                &"a".repeat(40),
                &sha256_hex(&std::fs::read(&exe).unwrap()),
            );
            assert_eq!(
                installed_tool_in_home(&install, &home),
                Err(InstalledToolError::UntrustedPath(writable.clone()))
            );
            assert_eq!(
                installed_tool_in_home(&writable, &home),
                Err(InstalledToolError::UntrustedPath(writable))
            );
            std::fs::remove_file(&sibling).unwrap();
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn installed_guard_refuses_redirected_install_directories_and_identity_records_3911() {
        let dir = crate::test_support::disk_backed_tempdir("guard-redirected-install");
        let home = dir.path().canonicalize().unwrap();
        let writable = home.join(".rsi/sandboxes/worker/target/release");
        std::fs::create_dir_all(&writable).unwrap();
        script(&writable, "exit 0");
        let install = home.join(".rsi/install");
        std::os::unix::fs::symlink(&writable, &install).unwrap();
        assert_eq!(
            installed_tool_in_home(&install, &home),
            Err(InstalledToolError::UntrustedPath(writable.clone()))
        );
        std::fs::remove_file(&install).unwrap();
        std::fs::create_dir(&install).unwrap();
        let exe = script(&install, "exit 0");
        let foreign_record = writable.join("identity.json");
        std::fs::write(&foreign_record, "{}").unwrap();
        std::os::unix::fs::symlink(&foreign_record, identity_path(&exe)).unwrap();
        assert_eq!(
            installed_tool_in_home(&install, &home),
            Err(InstalledToolError::UntrustedPath(foreign_record))
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn an_identity_record_binds_the_binary_and_the_target_tree() {
        let dir = tempfile::tempdir().unwrap();
        let exe = script(dir.path(), "exit 0");
        assert_eq!(check_identity(&exe, Some("a")), Identity::Absent);

        let tree = "a".repeat(40);
        let digest = sha256_hex(&std::fs::read(&exe).unwrap());
        record(&exe, &tree, &digest);
        assert_eq!(check_identity(&exe, Some(&tree)), Identity::Verified);
        assert_eq!(check_identity(&exe, None), Identity::Verified);
        let Identity::Refused(reason) = check_identity(&exe, Some(&"b".repeat(40))) else {
            panic!("another target's tree must be refused")
        };
        assert!(reason.contains("not the target's tree"), "{reason}");

        // The binary changed after it was recorded.
        std::fs::write(&exe, "#!/bin/sh\nexit 1\n").unwrap();
        let Identity::Refused(reason) = check_identity(&exe, Some(&tree)) else {
            panic!("a changed binary must be refused")
        };
        assert!(reason.contains("recorded digest"), "{reason}");

        // A record that cannot be parsed never degrades to "no record".
        std::fs::write(identity_path(&exe), "not json").unwrap();
        assert!(matches!(
            check_identity(&exe, Some(&tree)),
            Identity::Refused(_)
        ));
        record(&exe, "short", &digest);
        assert!(matches!(check_identity(&exe, None), Identity::Refused(_)));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_recorded_tool_is_trusted_only_for_the_tree_of_its_target() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
                .args(args)
                .output()
                .unwrap();
            assert!(output.status.success(), "{args:?}: {output:?}");
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        git(&["init", "-q"]);
        std::fs::write(repo.join("file"), "x\n").unwrap();
        git(&["add", "file"]);
        git(&["commit", "-q", "-m", "one"]);
        let target = git(&["rev-parse", "HEAD"]);
        let tree = git(&["rev-parse", "HEAD^{tree}"]);
        std::fs::write(repo.join("file"), "y\n").unwrap();
        git(&["commit", "-qam", "two"]);
        let other = git(&["rev-parse", "HEAD"]);

        let exe = script(dir.path(), "exit 0");
        // No record: the interim installed tool is trusted for any target.
        assert_eq!(
            authenticated(exe.clone(), &repo, &target),
            Some(exe.clone())
        );
        record(&exe, &tree, &sha256_hex(&std::fs::read(&exe).unwrap()));
        assert_eq!(
            authenticated(exe.clone(), &repo, &target),
            Some(exe.clone())
        );
        assert_eq!(authenticated(exe.clone(), &repo, &other), None);
        // A target the repository cannot read has no tree: refused, not guessed.
        assert_eq!(authenticated(exe.clone(), &repo, &"9".repeat(40)), None);
        assert_eq!(authenticated(exe, &repo, ""), None);
    }
}

/// The portable path (#361): off Unix there is no process group, so only the
/// direct child is killed on a deadline; a guard that cannot start is still a
/// failed run, never a refusal and never a pass.
#[cfg(all(test, not(unix)))]
mod portable_tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_missing_executable_is_a_failed_run_on_every_platform() {
        let run = run_guard(
            Path::new("nonexistent-rsi-repo-guard"),
            "x",
            &[],
            &std::env::temp_dir(),
            Vec::<(OsString, OsString)>::new(),
            Duration::from_secs(1),
        );
        assert!(matches!(run, GuardRun::Failed(_)), "{run:?}");
    }
}
