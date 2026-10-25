//! Bounded Git for the runner's own repository operations: every call runs on
//! the shared owned process pump with a deadline and the run's cancel flag.
use super::process;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io;
use std::path::Path;
use std::sync::atomic::AtomicUsize;
use std::time::{Duration, Instant};

pub const STEP_SECONDS: u64 = 120;
const MAX_OUTPUT: usize = 1 << 20;

#[derive(Debug)]
pub struct Out {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}
impl Out {
    pub fn ok(&self) -> bool {
        self.code == 0
    }
    pub fn first_line(&self) -> &str {
        self.stdout.lines().next().unwrap_or("").trim()
    }
}

pub struct Git<'a> {
    pub repo: &'a Path,
    pub env: &'a BTreeMap<String, String>,
    pub cancel: &'a AtomicUsize,
}
impl Git<'_> {
    fn collect(
        &self,
        run: impl FnOnce(&mut dyn FnMut(usize, &[u8])) -> io::Result<process::Outcome>,
    ) -> io::Result<Out> {
        let mut streams = [Vec::new(), Vec::new()];
        let mut oversized = false;
        let outcome = run(&mut |pipe, bytes| {
            if streams[pipe].len() + bytes.len() <= MAX_OUTPUT {
                streams[pipe].extend_from_slice(bytes);
            } else {
                oversized = true;
            }
        })?;
        if oversized || !outcome.output_complete {
            return Err(io::Error::other("Git output was incomplete or oversized"));
        }
        let [stdout, stderr] = streams;
        Ok(Out {
            code: outcome.code,
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
            timed_out: outcome.timed_out,
        })
    }
    /// Object/history builtins only (`process::stream_git_bytes` refuses others).
    pub fn builtin(&self, args: &[&str], extra: &[(&str, &str)]) -> io::Result<Out> {
        let mut env: BTreeMap<OsString, OsString> =
            self.env.iter().map(|(k, v)| (k.into(), v.into())).collect();
        for (key, value) in extra {
            env.insert((*key).into(), (*value).into());
        }
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        let deadline = Instant::now() + Duration::from_secs(STEP_SECONDS);
        self.collect(|emit| {
            process::stream_git_bytes(self.repo, &args, &env, deadline, self.cancel, emit)
        })
    }
    /// Other Git subcommands (`worktree`, `status`) with the same bounds.
    pub fn plain(&self, args: &[&str]) -> io::Result<Out> {
        let mut argv: Vec<String> = vec![
            "git".into(),
            "-C".into(),
            self.repo.to_string_lossy().into_owned(),
            "-c".into(),
            "core.fsmonitor=false".into(),
        ];
        argv.extend(args.iter().map(|a| (*a).into()));
        let deadline = Instant::now() + Duration::from_secs(STEP_SECONDS);
        self.collect(|emit| {
            process::stream_bytes(&argv, self.repo, self.env, deadline, self.cancel, emit)
        })
    }
    /// Resolve `reference` to one commit id, or None.
    pub fn commit(&self, reference: &str) -> io::Result<Option<String>> {
        if reference.starts_with('-') || reference.contains(['\0', '\n', '\r']) {
            return Ok(None);
        }
        let out = self.builtin(
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{reference}^{{commit}}"),
            ],
            &[],
        )?;
        Ok(out
            .ok()
            .then(|| out.first_line().to_string())
            .filter(|s| !s.is_empty()))
    }
}

#[cfg(test)]
pub(crate) fn test_environment() -> BTreeMap<String, String> {
    ["PATH", "HOME"]
        .iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| ((*k).to_string(), v)))
        .collect()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::testutil;
    #[test]
    fn builtin_and_plain_run_bounded_and_refuse_non_builtins() {
        let dir = tempfile::tempdir().unwrap();
        testutil::git(dir.path(), &["init", "-q"]);
        testutil::write(dir.path(), "a.txt", "one\n");
        testutil::git(dir.path(), &["add", "a.txt"]);
        testutil::git(
            dir.path(),
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                "one",
            ],
        );
        let env = test_environment();
        let cancel = AtomicUsize::new(0);
        let git = Git {
            repo: dir.path(),
            env: &env,
            cancel: &cancel,
        };
        let head = git.commit("HEAD").unwrap().unwrap();
        assert_eq!(head.len(), 40);
        assert_eq!(git.commit("missing-ref").unwrap(), None);
        assert_eq!(git.commit("--upload-pack=x").unwrap(), None);
        assert!(git.plain(&["status", "--porcelain"]).unwrap().ok());
        assert!(git.builtin(&["status"], &[]).is_err());
        assert!(!git.plain(&["rev-parse", "--verify", "nope"]).unwrap().ok());
    }
}
