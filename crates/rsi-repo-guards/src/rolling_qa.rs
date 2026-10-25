//! Full rolling QA planning, resume/state and v1 reports (#363).
//!
//! Staged orchestration library. The production backend must supply authenticated
//! inventory/runner tools, host admission, one parent Isolation/cancel flag and
//! bounded lane processes. No legacy wrapper or canonical CLI is called here.
use crate::qa_cache::{LANES, SHARDS};
use crate::qa_fingerprint::lowercase_hex;
use crate::re::{Re, group};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path};
use std::sync::LazyLock;

fn error(message: impl Into<String>) -> io::Error {
    io::Error::other(message.into())
}
static NEXTEST_FAILURE: LazyLock<Re> =
    LazyLock::new(|| Re::new(r"(?m)^\s*(?:FAIL|SIG[A-Z0-9]+) \[[^]]+\] \([^)]*\) (.+)$"));
static CARGO_FAILURE: LazyLock<Re> = LazyLock::new(|| Re::new(r"(?m)^test (.+) \.\.\. FAILED$"));
static SUMMARY: LazyLock<Re> = LazyLock::new(|| Re::new(r"(?m)^\s*Summary \[[^]]+\] .+$"));
static SUMMARY_COUNTS: LazyLock<Re> =
    LazyLock::new(|| Re::new(r"(?m)^\s*Summary \[[^]]+\] (\d+) tests run: (.+)$"));

pub fn failures(output: &str) -> Vec<String> {
    NEXTEST_FAILURE
        .findall1(output)
        .into_iter()
        .chain(CARGO_FAILURE.findall1(output))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}
pub fn summaries(output: &str) -> Vec<String> {
    SUMMARY
        .find_all(output)
        .last()
        .map(|m| vec![group(m, 0).to_owned()])
        .unwrap_or_default()
}
pub fn shard_counts(summary: &[String]) -> Option<Value> {
    let joined = summary.join("\n");
    let found = SUMMARY_COUNTS.search(&joined)?;
    let run = group(&found, 1).parse::<u64>().ok()?;
    let detail = group(&found, 2);
    let mut counts = BTreeMap::new();
    for word in ["passed", "failed", "skipped"] {
        let pattern = Re::new(&format!(r"(\d+) {word}"));
        let count = match pattern.search(detail) {
            Some(m) => group(&m, 1).parse::<u64>().ok()?,
            None => 0,
        };
        counts.insert(word, count);
    }
    if run != counts["passed"].checked_add(counts["failed"])? {
        return None;
    }
    Some(
        json!({"run":run,"passed":counts["passed"],"failed":counts["failed"],"skipped":counts["skipped"]}),
    )
}

pub fn parse_signature_matches(lines: &[String]) -> Value {
    fn field(text: &str) -> Option<(&str, &str)> {
        let text = text.trim_start();
        let end = text.find(char::is_whitespace)?;
        Some((&text[..end], text[end..].trim_start()))
    }
    let mut matches = serde_json::Map::new();
    for line in lines {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("KNOWN?") {
            let Some((issues, test)) = field(rest) else {
                continue;
            };
            let issues = issues
                .split(',')
                .filter(|s| !s.is_empty())
                .map(|s| s.trim_start_matches('#').parse::<u64>())
                .collect::<Result<Vec<_>, _>>();
            if let Ok(issues) = issues {
                matches.insert(
                    test.trim_start().into(),
                    json!({"state":"known?","issues":issues}),
                );
            }
        } else if let Some(rest) = line.strip_prefix("KNOWN ") {
            let Some((issue, rest)) = field(rest) else {
                continue;
            };
            let Some((class, test)) = field(rest) else {
                continue;
            };
            if let Ok(issue) = issue.trim_start_matches('#').parse::<u64>() {
                matches.insert(
                    test.trim_start().into(),
                    json!({"state":"known","issues":[issue],"class":class}),
                );
            }
        } else if let Some(test) = line.strip_prefix("NEW ") {
            matches.insert(test.trim().into(), json!({"state":"new"}));
        }
    }
    Value::Object(matches)
}

pub fn spec_environment() -> BTreeMap<String, String> {
    [
        ("CARGO_BUILD_JOBS".into(), "4".into()),
        ("CARGO_PROFILE_DEV_DEBUG".into(), "line-tables-only".into()),
    ]
    .into()
}
pub fn lane_environment(base: &BTreeMap<String, String>, shard: bool) -> BTreeMap<String, String> {
    let mut env = base.clone();
    env.entry("CARGO_INCREMENTAL".into()).or_insert("0".into());
    if shard {
        env.extend(spec_environment());
    }
    env
}
pub fn shard_environment(tmpdir_class: &str) -> io::Result<Value> {
    if !matches!(tmpdir_class, "tmpfs" | "disk") {
        return Err(error("unknown QA scratch filesystem class"));
    }
    Ok(json!({"spec_env":spec_environment(),"tmpdir_class":tmpdir_class}))
}
/// Match the lander's filesystem class without a GNU-only stat subprocess.
pub fn tmpdir_class(path: &Path) -> io::Result<&'static str> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(path.as_os_str().as_bytes())
            .map_err(|e| error(e.to_string()))?;
        // SAFETY: an initialized output record and a NUL-terminated input path.
        let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statfs(path.as_ptr(), &raw mut stat) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(if matches!(stat.f_type as u64, 0x0102_1994 | 0x8584_58f6) {
            "tmpfs"
        } else {
            "disk"
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        fs::metadata(path)?;
        Ok("disk")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Lane {
    Static,
    Shard { name: String, slow_tier: bool },
    Integrations { targets: Vec<String> },
    Bins,
    Doc,
    Rsi,
    Common,
}
impl Lane {
    pub fn label(&self) -> &str {
        match self {
            Self::Static => "static",
            Self::Shard { name, .. } => name,
            Self::Integrations { .. } => "integrations",
            Self::Bins => "bins",
            Self::Doc => "rsid-doc",
            Self::Rsi => "rsi",
            Self::Common => "rsi-common",
        }
    }
    /// Companion argv only; static/shard operations need the native owner APIs,
    /// not a guessed inventory or a Python-backed shell entry.
    pub fn cargo_command(&self, jobs: u32) -> Option<Vec<String>> {
        let mut args = vec!["cargo".to_owned()];
        match self {
            Self::Static | Self::Shard { .. } => return None,
            Self::Integrations { targets } => {
                args.extend(
                    ["nextest", "run", "--profile", "rsid-fast", "-p", "rsid"].map(str::to_owned),
                );
                for target in targets {
                    args.extend(["--test".into(), target.clone()]);
                }
            }
            Self::Bins => args.extend(
                [
                    "nextest",
                    "run",
                    "--profile",
                    "rsid-fast",
                    "-p",
                    "rsid",
                    "--bins",
                ]
                .map(str::to_owned),
            ),
            Self::Doc => args.extend(["test", "-p", "rsid", "--doc", "--"].map(str::to_owned)),
            Self::Rsi => args.extend(["test", "-p", "rsi", "--lib", "--"].map(str::to_owned)),
            Self::Common => args.extend(
                ["test", "-p", "rsi-common", "-p", "rsid-remote-read", "--"].map(str::to_owned),
            ),
        }
        if matches!(self, Self::Integrations { .. } | Self::Bins) {
            args.extend([
                "--status-level".into(),
                "all".into(),
                "--final-status-level".into(),
                "all".into(),
                "-j".into(),
                jobs.to_string(),
            ]);
        } else {
            args.push(format!("--test-threads={jobs}"));
        }
        Some(args)
    }
}

pub fn plan(shards: &[String], integrations: &[String], jobs: u32) -> io::Result<Vec<Lane>> {
    if !(1..=4).contains(&jobs) {
        return Err(error("--jobs must be 1..4"));
    }
    let expected: BTreeSet<_> = SHARDS.iter().copied().collect();
    if shards.len() != 16 || shards.iter().map(String::as_str).collect::<BTreeSet<_>>() != expected
    {
        return Err(error(
            "QA requires all 16 distinct authenticated shard names",
        ));
    }
    let mut targets = integrations.to_vec();
    targets.sort();
    targets.dedup();
    if targets.is_empty()
        || targets
            .iter()
            .any(|s| s.is_empty() || s.starts_with('-') || s.contains(['/', '\\']))
    {
        return Err(error("integration target inventory is missing or invalid"));
    }
    let mut lanes = vec![Lane::Static];
    lanes.extend(shards.iter().map(|name| Lane::Shard {
        name: name.clone(),
        slow_tier: true,
    }));
    lanes.extend([
        Lane::Integrations { targets },
        Lane::Bins,
        Lane::Doc,
        Lane::Rsi,
        Lane::Common,
    ]);
    Ok(lanes)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct State {
    pub tip: String,
    pub tree: String,
    pub jobs: u32,
    pub started_at: String,
    pub fingerprint: Value,
    pub closed_since: Option<String>,
    pub results: Vec<Value>,
}
impl State {
    pub fn validate(&self) -> io::Result<()> {
        if !lowercase_hex(&self.tip, 40)
            || !lowercase_hex(&self.tree, 40)
            || !(1..=4).contains(&self.jobs)
        {
            return Err(error("invalid QA state source or jobs"));
        }
        if self.started_at.is_empty() || !self.fingerprint.is_object() {
            return Err(error("QA state lacks time/fingerprint provenance"));
        }
        let mut labels = BTreeSet::new();
        for row in &self.results {
            validate_row(row)?;
            if !labels.insert(row["label"].as_str().unwrap_or_default()) {
                return Err(error("duplicate completed QA lane"));
            }
        }
        Ok(())
    }
    pub fn resume(
        &self,
        jobs: u32,
        closed_since: Option<&str>,
        fingerprint: &Value,
        head_tree: &str,
        tracked_dirty: bool,
    ) -> io::Result<()> {
        self.validate()?;
        if self.jobs != jobs || self.closed_since.as_deref() != closed_since {
            return Err(error("resume arguments differ from recorded sweep"));
        }
        if &self.fingerprint != fingerprint {
            return Err(error(
                "runner/toolchain fingerprint differs from recorded sweep",
            ));
        }
        if self.tree != head_tree {
            return Err(error(
                "HEAD tree differs from recorded sweep; restore that exact tree to resume",
            ));
        }
        if tracked_dirty {
            return Err(error(
                "tracked worktree changes would contaminate the QA sweep",
            ));
        }
        Ok(())
    }
    pub fn record(&mut self, row: Value) -> io::Result<()> {
        validate_row(&row)?;
        if self.results.iter().any(|old| old["label"] == row["label"]) {
            return Err(error("QA lane already completed"));
        }
        self.results.push(row);
        Ok(())
    }
    pub fn save(&self, directory: &Path) -> io::Result<()> {
        self.validate()?;
        atomic_json(
            &directory.join("state.json"),
            &serde_json::to_value(self).map_err(|e| error(e.to_string()))?,
        )
    }
}
fn valid_label(label: &str) -> bool {
    label == "static" || SHARDS.contains(&label) || LANES.contains(&label)
}
fn safe_log(log: &str) -> bool {
    let path = Path::new(log);
    !path.is_absolute()
        && path.components().all(|c| matches!(c, Component::Normal(_)))
        && path.starts_with("target")
        && path.components().count() > 1
}
fn validate_row(row: &Value) -> io::Result<()> {
    let label = row["label"]
        .as_str()
        .ok_or_else(|| error("QA result lacks label"))?;
    if !valid_label(label)
        || row["exit"].as_i64().is_none()
        || !row["seconds"]
            .as_f64()
            .is_some_and(|s| s.is_finite() && s >= 0.)
        || !row["log"].as_str().is_some_and(safe_log)
    {
        return Err(error("invalid completed QA result"));
    }
    for key in ["summary", "failures"] {
        if !row[key]
            .as_array()
            .is_some_and(|v| v.iter().all(Value::is_string))
        {
            return Err(error(format!("invalid QA result {key}")));
        }
    }
    if SHARDS.contains(&label)
        && crate::qa_cache::recorded_env_class(row, label)
            .map_err(|e| error(e.to_string()))?
            .is_none()
    {
        return Err(error("QA shard result lacks its executed environment"));
    }
    Ok(())
}
pub fn result(
    label: &str,
    code: i32,
    seconds: f64,
    log: &str,
    output: &str,
    environment: Option<Value>,
) -> io::Result<Value> {
    let mut row = json!({"label":label,"exit":code,"seconds":(seconds*1000.).round()/1000.,"log":log,"summary":summaries(output),"failures":failures(output)});
    if let Some(env) = environment {
        row["environment"] = env;
    }
    validate_row(&row)?;
    Ok(row)
}
pub fn atomic_json(path: &Path, value: &Value) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| error("QA output has no parent"))?;
    let mut pending = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(pending.as_file_mut(), value).map_err(|e| error(e.to_string()))?;
    pending.write_all(b"\n")?;
    pending.as_file_mut().sync_all()?;
    pending.persist(path).map_err(|e| e.error)?;
    Ok(())
}

/// No successful defaults: the integrating parent retains its admission holder,
/// immutable Isolation, signal flag and source monitor while every lane runs.
pub trait Backend {
    fn verify_source(&mut self, state: &State) -> io::Result<()>;
    fn run_lane(&mut self, lane: &Lane, jobs: u32) -> io::Result<Value>;
}
pub fn execute_pending(
    state: &mut State,
    lanes: &[Lane],
    backend: &mut impl Backend,
    mut checkpoint: impl FnMut(&State) -> io::Result<()>,
) -> io::Result<()> {
    state.validate()?;
    if lanes.len() != 22
        || !matches!(lanes.first(), Some(Lane::Static))
        || lanes.iter().map(Lane::label).collect::<BTreeSet<_>>().len() != 22
    {
        return Err(error("incomplete QA plan"));
    }
    // Validate the inventory and slow-tier requirement even for reused lanes.
    let shards: Vec<_> = lanes
        .iter()
        .filter_map(|l| {
            if let Lane::Shard {
                name,
                slow_tier: true,
            } = l
            {
                Some(name.clone())
            } else {
                None
            }
        })
        .collect();
    let integrations = lanes
        .iter()
        .find_map(|l| {
            if let Lane::Integrations { targets } = l {
                Some(targets.clone())
            } else {
                None
            }
        })
        .ok_or_else(|| error("missing integration lane"))?;
    if &plan(&shards, &integrations, state.jobs)? != lanes {
        return Err(error("QA plan omits or reorders a mandatory lane"));
    }
    for lane in lanes {
        backend.verify_source(state)?;
        if let Some(prior) = state
            .results
            .iter()
            .find(|row| row["label"] == lane.label())
        {
            if lane.label() == "static" && prior["exit"] != 0 {
                break;
            }
            continue;
        }
        let row = backend.run_lane(lane, state.jobs)?;
        if row["label"] != lane.label() {
            return Err(error("QA backend returned a different lane"));
        }
        let failed_static = lane.label() == "static" && row["exit"] != 0;
        state.record(row)?;
        checkpoint(state)?;
        if failed_static {
            break;
        }
    }
    backend.verify_source(state)
}

pub struct ReportContext<'a> {
    pub final_tip: &'a str,
    pub finished_at: &'a str,
    pub host: &'a str,
    pub runner: &'a str,
    pub report_ref: &'a str,
    pub closed_issues: Value,
    pub signature_matches: Value,
}
/// Compatibility reports only, never a daemon-attested scoped receipt. Fingerprint
/// probes are supplied by the same authenticated runner as the actual lanes.
pub fn reports(
    state: &State,
    ctx: ReportContext<'_>,
    mut fingerprint: impl FnMut(&str) -> io::Result<Value>,
) -> io::Result<(i32, Value, Value)> {
    state.validate()?;
    if !lowercase_hex(ctx.final_tip, 40)
        || ctx.finished_at.is_empty()
        || ctx.runner.is_empty()
        || !safe_log(ctx.report_ref)
        || !ctx.closed_issues.is_array()
    {
        return Err(error("invalid QA report provenance"));
    }
    let signatures = ctx
        .signature_matches
        .as_object()
        .ok_or_else(|| error("invalid known failure annotations"))?;
    let mut shards = Vec::new();
    let mut lanes = Vec::new();
    let mut counts_complete = true;
    for row in &state.results {
        let name = row["label"].as_str().unwrap_or_default();
        let mut item = json!({"name":name,"exit_code":row["exit"],"failing_tests":row["failures"]});
        if let Some(matches) = signatures.get(name) {
            item["known_failures"] = matches.clone();
        }
        if SHARDS.contains(&name) {
            let summary = row["summary"]
                .as_array()
                .unwrap()
                .iter()
                .map(|s| s.as_str().unwrap().to_owned())
                .collect::<Vec<_>>();
            let counts = shard_counts(&summary);
            counts_complete &= counts.is_some();
            for (key, value) in counts
                .unwrap_or_else(|| json!({"run":0,"passed":0,"failed":0,"skipped":0}))
                .as_object()
                .unwrap()
            {
                item[key] = value.clone();
            }
            item["fingerprint"] = fingerprint(name)?;
            item["environment"] = row["environment"].clone();
            shards.push(item);
        } else if LANES.contains(&name) {
            lanes.push(item);
        }
    }
    let complete = state
        .results
        .iter()
        .any(|row| row["label"] == "static" && row["exit"] == 0)
        && shards.len() == 16
        && lanes.len() == 5
        && counts_complete;
    let report = json!({"tip":state.tip,"final_rolling":ctx.final_tip,"tip_stable":state.tip==ctx.final_tip,"jobs":state.jobs,"fingerprint":state.fingerprint,
        "sweep_finished_at":ctx.finished_at,"closed_since":state.closed_since,"closed_issues":ctx.closed_issues,"signature_matches":ctx.signature_matches,"results":state.results});
    let sweep = json!({"schema_version":1,"swept_sha":state.tip,"swept_tree":state.tree,"complete":complete,
        "provenance":{"kind":"qa_sweep","runner":ctx.runner,"host":ctx.host,"started_at":state.started_at,"finished_at":ctx.finished_at,"report_ref":ctx.report_ref},"shards":shards,"lanes":lanes});
    // Missing mandatory coverage cannot become a successful sweep from exit 0
    // rows alone. Cache import already refuses such an incomplete report.
    let code = i32::from(!complete || state.results.iter().any(|row| row["exit"] != 0));
    Ok((code, report, sweep))
}

#[cfg(test)]
#[path = "rolling_qa_tests.rs"]
mod tests;

/// Physical evidence path inside this sandbox's target, with no target-top-level
/// files and no symlink escape. New nested directories need not exist yet.
pub fn evidence_directory(repo: &Path, requested: &Path) -> io::Result<std::path::PathBuf> {
    let repo = repo.canonicalize()?;
    let target = repo.join("target").canonicalize()?;
    let absolute = if requested.is_absolute() {
        requested.to_owned()
    } else {
        repo.join(requested)
    };
    if absolute
        .components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        return Err(error("QA evidence path contains parent traversal"));
    }
    let mut ancestor = absolute.as_path();
    let mut tail = Vec::new();
    let mut resolved = loop {
        match fs::canonicalize(ancestor) {
            Ok(path) => break path,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                tail.push(
                    ancestor
                        .file_name()
                        .ok_or_else(|| error("invalid QA evidence path"))?
                        .to_owned(),
                );
                ancestor = ancestor
                    .parent()
                    .ok_or_else(|| error("invalid QA evidence parent"))?;
            }
            Err(e) => return Err(e),
        }
    };
    for part in tail.iter().rev() {
        resolved.push(part);
    }
    if resolved == target || !resolved.starts_with(&target) {
        return Err(error(
            "evidence directory must be under this sandbox's target/",
        ));
    }
    Ok(resolved)
}
