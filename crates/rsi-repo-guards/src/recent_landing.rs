//! Bounded informational stale-copy detection over the net trial-merge delta.
use crate::{
    Ctx,
    re::{self, Re},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

pub const LINE_LIMIT: usize = 2000;
pub const FILE_LIMIT: usize = 64;
pub const TIME_LIMIT: u64 = 10;
pub const OPT_OUT: &str = "Allow-Recent-Landing-Removals";
pub const STDERR_LIMIT: usize = 1024;
pub const ARG_LIMIT: usize = 200;

pub fn git_diagnostic(
    argv: &[String],
    step: Option<&str>,
    kind: &str,
    exit_code: Option<i32>,
    stderr: &str,
    timeout: Option<f64>,
) -> Value {
    let hash = Re::new(r"(?<![0-9a-f])[0-9a-f]{40,64}(?![0-9a-f])");
    let display: Vec<String> = argv
        .iter()
        .map(|arg| {
            let arg = hash.sub_with(arg, &|c| format!("{}...", &re::group(c, 0)[..12]));
            let chars: Vec<char> = arg.chars().collect();
            if chars.len() > ARG_LIMIT {
                format!(
                    "{}...<argument shortened>...{}",
                    chars[..80].iter().collect::<String>(),
                    chars[chars.len() - 80..].iter().collect::<String>()
                )
            } else {
                arg
            }
        })
        .collect();
    let lines = crate::py::splitlines(stderr);
    let tail = lines[lines.len().saturating_sub(8)..].join("\n");
    let tail: String = tail
        .chars()
        .rev()
        .take(STDERR_LIMIT)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let mut result = json!({"step":step.unwrap_or_else(|| argv.get(3).map_or("git", String::as_str)), "kind":kind, "argv":display, "argv_shortened":display!=argv, "exit_code":exit_code, "stderr_tail":tail, "stderr_shortened":tail!=stderr});
    if let Some(timeout) = timeout {
        result["timeout_seconds"] = if timeout.fract() == 0.0 {
            json!(timeout as u64)
        } else {
            json!(timeout)
        };
    }
    result
}
fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\"'\"'"))
    }
}
/// The legacy `str(GitStepError(diagnostic))` text a receipt carries as its reason.
pub fn reason(d: &Value) -> String {
    let argv = d["argv"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(shell_quote)
        .collect::<Vec<_>>()
        .join(" ");
    let exit = d["exit_code"]
        .as_i64()
        .map_or_else(|| "None".into(), |n| n.to_string());
    let tail = d["stderr_tail"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or("stderr empty");
    format!(
        "{}: {}; exit_code={exit}; argv={argv}; {tail}",
        d["step"].as_str().unwrap_or("git"),
        d["kind"].as_str().unwrap_or("invalid_output")
    )
}
#[derive(Clone, Copy)]
pub struct Limits {
    pub lines: usize,
    pub files: usize,
    pub time: Duration,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            lines: LINE_LIMIT,
            files: FILE_LIMIT,
            time: Duration::from_secs(TIME_LIMIT),
        }
    }
}
pub struct GitOutput {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}
pub struct GitError {
    pub kind: &'static str,
    pub stderr: Vec<u8>,
}
/// Injected in tests to exercise every Git failure with its exact attempted argv.
pub trait GitRunner {
    fn run(&mut self, argv: &[String], remaining: Duration) -> Result<GitOutput, GitError>;
}
pub struct ProcessGit;
impl GitRunner for ProcessGit {
    fn run(&mut self, argv: &[String], remaining: Duration) -> Result<GitOutput, GitError> {
        let launched = Instant::now();
        let mut child = Command::new(crate::os_escape::arg(&argv[0]))
            .args(argv[1..].iter().map(|a| crate::os_escape::arg(a)))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| GitError {
                kind: "launch_error",
                stderr: e.to_string().into_bytes(),
            })?;
        let mut stdout = child.stdout.take().ok_or_else(|| GitError {
            kind: "launch_error",
            stderr: b"missing stdout pipe".to_vec(),
        })?;
        let mut stderr = child.stderr.take().ok_or_else(|| GitError {
            kind: "launch_error",
            stderr: b"missing stderr pipe".to_vec(),
        })?;
        // Drain while waiting so a full pipe never stalls the deadline check.
        let output = std::thread::spawn(move || {
            let mut b = Vec::new();
            stdout.read_to_end(&mut b).map(|_| b)
        });
        let errors = std::thread::spawn(move || {
            let mut b = Vec::new();
            stderr.read_to_end(&mut b).map(|_| b)
        });
        let mut timed_out = false;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) if launched.elapsed() < remaining => std::thread::sleep(
                    Duration::from_millis(5).min(remaining.saturating_sub(launched.elapsed())),
                ),
                Ok(None) => {
                    timed_out = true;
                    let _ = child.kill();
                    break child.wait();
                }
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break Err(e);
                }
            }
        };
        let stdout = output
            .join()
            .ok()
            .and_then(Result::ok)
            .ok_or_else(|| GitError {
                kind: "invalid_output",
                stderr: b"cannot read git stdout".to_vec(),
            })?;
        let stderr = errors
            .join()
            .ok()
            .and_then(Result::ok)
            .ok_or_else(|| GitError {
                kind: "invalid_output",
                stderr: b"cannot read git stderr".to_vec(),
            })?;
        if timed_out {
            return Err(GitError {
                kind: "timeout_budget",
                stderr,
            });
        }
        let status = status.map_err(|e| GitError {
            kind: "launch_error",
            stderr: e.to_string().into_bytes(),
        })?;
        #[cfg(unix)]
        let code = {
            use std::os::unix::process::ExitStatusExt;
            status
                .code()
                .unwrap_or_else(|| -status.signal().unwrap_or(1))
        };
        #[cfg(not(unix))]
        let code = status.code().unwrap_or(1);
        Ok(GitOutput {
            code,
            stdout,
            stderr,
        })
    }
}
struct Probe<'a> {
    repo: &'a Path,
    deadline: Instant,
    limits: Limits,
    runner: &'a mut dyn GitRunner,
    argv: Vec<String>,
    code: Option<i32>,
}
impl Probe<'_> {
    fn git(&mut self, args: &[String], ancestor: bool) -> Result<GitOutput, Value> {
        self.argv = vec![
            "git".into(),
            "-C".into(),
            crate::os_escape::decode_path(self.repo),
        ];
        self.argv.extend_from_slice(args);
        self.code = None;
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        let timeout = Some(self.limits.time.as_secs_f64());
        if remaining.is_zero() {
            return Err(git_diagnostic(
                &self.argv,
                None,
                "timeout_budget",
                None,
                "",
                timeout,
            ));
        }
        let done = self.runner.run(&self.argv, remaining).map_err(|e| {
            git_diagnostic(
                &self.argv,
                None,
                e.kind,
                None,
                &String::from_utf8_lossy(&e.stderr),
                if e.kind == "timeout_budget" {
                    timeout
                } else {
                    None
                },
            )
        })?;
        self.code = Some(done.code);
        if done.code != 0 && !(ancestor && done.code == 1) {
            return Err(git_diagnostic(
                &self.argv,
                None,
                "git_exit",
                Some(done.code),
                &String::from_utf8_lossy(&done.stderr),
                None,
            ));
        }
        Ok(done)
    }
    fn run(&mut self, args: &[&str]) -> Result<GitOutput, Value> {
        self.git(&args.iter().map(|s| (*s).into()).collect::<Vec<_>>(), false)
    }
    fn invalid(&self, error: &str) -> Value {
        git_diagnostic(&self.argv, None, "invalid_output", self.code, error, None)
    }
}

pub fn recent_landing_deletions(
    repo: &Path,
    source: &str,
    rolling: &str,
    merged_tree: &str,
    limits: Limits,
) -> Value {
    check_with(repo, source, rolling, merged_tree, limits, &mut ProcessGit)
}
pub fn check_with(
    repo: &Path,
    source: &str,
    rolling: &str,
    merged_tree: &str,
    limits: Limits,
    runner: &mut dyn GitRunner,
) -> Value {
    let mut result = json!({"status":"complete", "warnings":[], "rolling":rolling, "lines_blamed":0, "line_limit":limits.lines,"file_limit":limits.files});
    let mut p = Probe {
        repo,
        deadline: Instant::now() + limits.time,
        limits,
        runner,
        argv: Vec::new(),
        code: None,
    };
    let outcome = (|| -> Result<(), Value> {
        let own = p.run(&[
            "rev-list",
            "--first-parent",
            "--reverse",
            &format!("{rolling}..{source}"),
        ])?;
        let Some(first) = own.stdout.split(|b| *b == b'\n').find(|s| !s.is_empty()) else {
            return Ok(());
        };
        let first = std::str::from_utf8(first).map_err(|e| p.invalid(&e.to_string()))?;
        result["first_own_commit"] = json!(first);
        let trailers = p.run(&[
            "log",
            "--no-notes",
            "--no-show-signature",
            &format!("--format=%H%x00%(trailers:key={OPT_OUT},valueonly)%x00"),
            &format!("{rolling}..{source}"),
        ])?;
        let parts: Vec<&[u8]> = trailers.stdout.split(|b| *b == 0).collect();
        for index in (1..parts.len()).step_by(2) {
            if String::from_utf8_lossy(parts[index])
                .lines()
                .any(|s| s.trim().eq_ignore_ascii_case("true"))
            {
                result["status"] = json!("opted_out");
                result["opt_out_commit"] = json!(String::from_utf8_lossy(parts[index - 1]).trim());
                return Ok(());
            }
        }
        let stats = p.run(&[
            "diff",
            "--numstat",
            "-z",
            "--no-renames",
            "--no-ext-diff",
            "--no-textconv",
            rolling,
            merged_tree,
        ])?;
        let mut ancestry = BTreeMap::new();
        let mut files = 0;
        for entry in stats.stdout.split(|b| *b == 0).filter(|e| !e.is_empty()) {
            let parts: Vec<&[u8]> = entry.splitn(3, |b| *b == b'\t').collect();
            if parts.len() != 3 {
                return Err(p.invalid("malformed git numstat"));
            }
            if parts[1] == b"-" || parts[1] == b"0" {
                continue;
            }
            let blamed = result["lines_blamed"].as_u64().unwrap_or(0) as usize;
            if files >= limits.files || blamed >= limits.lines {
                let budget = if files >= limits.files {
                    "file"
                } else {
                    "deleted-line"
                };
                result["status"] = json!("partial");
                result["reason"] = json!(format!("{budget} limit reached"));
                result["diagnostic"] = git_diagnostic(
                    &[],
                    Some(&format!("{budget} budget")),
                    "limit",
                    None,
                    "",
                    None,
                );
                break;
            }
            files += 1;
            let path = crate::os_escape::decode(parts[2])
                .map_err(|e| p.invalid(&format!("unrepresentable Git path: {e}")))?;
            let path = path.as_str();
            let patch = p.run(&[
                "diff",
                "--unified=0",
                "--no-color",
                "--no-renames",
                "--no-ext-diff",
                "--no-textconv",
                rolling,
                merged_tree,
                "--",
                &format!(":(literal){path}"),
            ])?;
            let patch = String::from_utf8_lossy(&patch.stdout);
            let mut ranges = Vec::new();
            let mut remaining = limits.lines - blamed;
            for m in Re::new(r"(?m)^@@ -(\d+)(?:,(\d+))? \+").find_all(&patch) {
                let start: usize = re::group(&m, 1)
                    .parse()
                    .map_err(|_| p.invalid("invalid diff line"))?;
                let count: usize = if re::group(&m, 2).is_empty() {
                    1
                } else {
                    re::group(&m, 2)
                        .parse()
                        .map_err(|_| p.invalid("invalid diff count"))?
                };
                let take = count.min(remaining);
                if take > 0 {
                    ranges.push((start, start + take - 1));
                    remaining -= take;
                }
                if take < count {
                    result["status"] = json!("partial");
                    result["reason"] = json!("deleted-line limit reached");
                    result["diagnostic"] =
                        git_diagnostic(&[], Some("deleted-line budget"), "limit", None, "", None);
                    break;
                }
            }
            if ranges.is_empty() {
                continue;
            }
            let mut args: Vec<String> = ["blame", "--incremental", "--ignore-revs-file", ""]
                .iter()
                .map(|s| (*s).into())
                .collect();
            for (start, end) in &ranges {
                args.extend(["-L".into(), format!("{start},{end}")]);
            }
            args.extend([format!("{first}..{rolling}"), "--".into(), path.into()]);
            let blame = p.git(&args, false)?;
            result["lines_blamed"] =
                json!(blamed + ranges.iter().map(|(s, e)| e - s + 1).sum::<usize>());
            let mut findings: BTreeMap<String, Vec<(usize, usize)>> = BTreeMap::new();
            for m in Re::new(r"(?m)^([0-9a-f]{40,64}) \d+ (\d+) (\d+)$")
                .find_all(&String::from_utf8_lossy(&blame.stdout))
            {
                let commit = re::group(&m, 1);
                let start: usize = re::group(&m, 2)
                    .parse()
                    .map_err(|_| p.invalid("invalid blame line"))?;
                let count: usize = re::group(&m, 3)
                    .parse()
                    .map_err(|_| p.invalid("invalid blame count"))?;
                if !ancestry.contains_key(commit) {
                    let code = p
                        .git(
                            &[
                                "merge-base".into(),
                                "--is-ancestor".into(),
                                commit.into(),
                                first.into(),
                            ],
                            true,
                        )?
                        .code;
                    ancestry.insert(commit.to_string(), code == 0);
                }
                if !ancestry[commit] {
                    findings
                        .entry(commit.to_string())
                        .or_default()
                        .push((start, start + count - 1));
                }
            }
            for (commit, mut lines) in findings {
                let count: usize = lines.iter().map(|(a, b)| b - a + 1).sum();
                lines.sort_unstable();
                if let Some(warnings) = result["warnings"].as_array_mut() {
                    warnings.push(json!({"path":path,"line_count":count,"introducing_commit":commit,"line_ranges":lines}));
                }
            }
        }
        Ok(())
    })();
    if let Err(d) = outcome {
        result["status"] = json!(if result["lines_blamed"].as_u64().unwrap_or(0) > 0 {
            "partial"
        } else {
            "unavailable"
        });
        result["reason"] = json!(reason(&d));
        result["diagnostic"] = d;
    }
    result
}

const RENUMBER_NOTE: &str = "blamed against the lander's renumbered candidate: lines the renumber rewrote from rolling's migration numbers show as removals";

/// The receipt's `reverts_recent_landing` value for a run's trial merge: the
/// bounded blame over `rolling..merged_tree` when the merge produced a tree, the
/// `skipped` shape (with the merge's own reason and diagnostic) when it did not.
/// `renumbered` marks a candidate the lander already renumbered onto rolling.
pub fn advisory(
    repo: &Path,
    head: &str,
    merge: Option<(&str, &str)>,
    skipped_reason: &str,
    skipped_diagnostic: &Value,
    renumbered: bool,
) -> Value {
    let mut recent = match merge {
        Some((rolling, tree)) => {
            recent_landing_deletions(repo, head, rolling, tree, Limits::default())
        }
        None => {
            json!({"status": "skipped", "warnings": [], "reason": skipped_reason, "diagnostic": skipped_diagnostic})
        }
    };
    if renumbered && merge.is_some() {
        recent["note"] = json!(RENUMBER_NOTE);
    }
    recent
}

/// The progress line the legacy runner printed for the advisory.
pub fn advisory_line(recent: &Value) -> String {
    format!(
        "scoped-test: reverts_recent_landing {}",
        crate::os_escape::legacy_compact_json(recent)
    )
}
/// Shell adapter: advisory JSON, including unavailable coverage, always exits 0.
pub fn main(args: &[String], ctx: &Ctx, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    if args.len() != 3 {
        let _ = writeln!(
            err,
            "usage: recent-landing-check SOURCE ROLLING MERGED_TREE"
        );
        return 2;
    }
    // The scoped-test advisory line: `json.dumps(..., separators=(",", ":"))`.
    let result =
        recent_landing_deletions(&ctx.cwd, &args[0], &args[1], &args[2], Limits::default());
    let _ = writeln!(out, "{}", crate::os_escape::legacy_compact_json(&result));
    0
}
#[cfg(test)]
#[path = "recent_landing_tests.rs"]
mod tests;
