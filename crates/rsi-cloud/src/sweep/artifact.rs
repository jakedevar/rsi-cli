use super::wire::{host_class, legacy_json, output, sha256};
use super::{failures, read_lossy, status_rows, summary};
use regex::Regex;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;
use std::sync::LazyLock;

/// Probe boundary: tests supply the exact old fixture versions, blobs and host
/// without mutating the process environment or invoking a toolchain.
pub trait JsonSource {
    fn command(&self, program: &str, args: &[&str]) -> Result<String, String>;
    fn hostname(&self) -> Result<String, String>;
    fn host_class(&self) -> Result<String, String>;
    fn shards(&self) -> Result<Vec<String>, String>;
}
struct ProcessSource(std::path::PathBuf);
impl JsonSource for ProcessSource {
    fn command(&self, program: &str, args: &[&str]) -> Result<String, String> {
        output(&self.0, program, args)
    }
    fn hostname(&self) -> Result<String, String> {
        self.command("hostname", &[])
    }
    fn host_class(&self) -> Result<String, String> {
        host_class(&self.0)
    }
    fn shards(&self) -> Result<Vec<String>, String> {
        // The swept checkout's own `scripts/repo-guard` builds or reuses the
        // `rsi-repo-guard` binary (the same path the shard runner uses), so the
        // source validation and ordered inventory stay the guard's.
        let guard = self.0.join("scripts/repo-guard");
        if !guard.is_file() {
            return Err(format!(
                "{} is missing; run sweep-json from the swept checkout",
                guard.display()
            ));
        }
        let done = Command::new(&guard)
            .args(["rsid-test-shards", "--list-shards"])
            .current_dir(&self.0)
            .output()
            .map_err(|e| format!("cannot run {}: {e}", guard.display()))?;
        if !done.status.success() {
            return Err(String::from_utf8_lossy(&done.stderr).trim().into());
        }
        Ok(String::from_utf8(done.stdout)
            .map_err(|e| e.to_string())?
            .lines()
            .map(str::to_owned)
            .collect())
    }
}

pub(super) fn from_process(args: &[String]) -> Result<String, String> {
    json_report(
        &args[0],
        &args[1],
        &args[2],
        Path::new(&args[3]),
        &args[4],
        &ProcessSource(std::env::current_dir().map_err(|e| e.to_string())?),
    )
    .map(|value| pretty_json(&value))
}

/// `json.dumps(indent=2, sort_keys=True)` with ASCII escapes and the original
/// single-line empty containers. The compact serializer also defines digests.
pub fn pretty_json(value: &Value) -> String {
    fn render(value: &Value, depth: usize) -> String {
        let pad = "  ".repeat(depth);
        let next = "  ".repeat(depth + 1);
        match value {
            Value::Array(items) if !items.is_empty() => format!(
                "[\n{}\n{pad}]",
                items
                    .iter()
                    .map(|v| format!("{next}{}", render(v, depth + 1)))
                    .collect::<Vec<_>>()
                    .join(",\n")
            ),
            Value::Object(items) if !items.is_empty() => {
                let mut keys: Vec<_> = items.keys().collect();
                keys.sort();
                format!(
                    "{{\n{}\n{pad}}}",
                    keys.into_iter()
                        .map(|k| format!(
                            "{next}{}: {}",
                            legacy_json(&json!(k), true),
                            render(&items[k], depth + 1)
                        ))
                        .collect::<Vec<_>>()
                        .join(",\n")
                )
            }
            _ => legacy_json(value, true),
        }
    }
    render(value, 0)
}

pub fn json_report(
    sha: &str,
    started: &str,
    finished: &str,
    result: &Path,
    runner: &str,
    source: &dyn JsonSource,
) -> Result<Value, String> {
    if !["nextest", "cargo-test"].contains(&runner) {
        return Err("runner must be nextest or cargo-test".into());
    }
    if runner == "nextest" && source.command("git", &["rev-parse", "HEAD"])? != sha {
        return Err("report source HEAD differs from swept SHA".into());
    }
    let mut report = json!({
        "schema_version": 1, "swept_sha": sha,
        "swept_tree": source.command("git", &["rev-parse", &format!("{sha}^{{tree}}")])?,
        "complete": false,
        "provenance": {
            "kind": "qa_sweep", "runner": if runner == "nextest" { "scripts/cloud-sweep.sh -> scripts/run-rsid-test-shards.sh shard --jobs 4".into() } else { format!("scripts/cloud-sweep.sh ({runner})") },
            "host": source.hostname()?, "started_at": started, "finished_at": finished,
            "report_ref": result.join("report.json").to_string_lossy(),
        }, "shards": [], "lanes": [],
    });
    if runner == "cargo-test" {
        return Ok(report);
    }
    let rustc = source.command("rustc", &["-Vv"])?;
    let cargo = source.command("cargo", &["-V"])?;
    let nextest = source.command("cargo", &["nextest", "--version"])?;
    let target = rustc
        .lines()
        .find_map(|s| s.strip_prefix("host: "))
        .ok_or("rustc -Vv did not report a host target triple")?;
    let rows = status_rows(result)?;
    let mut shards = Vec::new();
    for shard in source.shards()? {
        let label = format!("rsid-{shard}");
        let Some(code) = rows.get(&label) else {
            continue;
        };
        let log = read_lossy(&result.join("logs").join(format!("{label}.log")))?;
        let (run, passed, failed, skipped) = summary(&log).unwrap_or((0, 0, 0, 0));
        let mut inputs = json!({"rustc": rustc, "cargo": cargo, "nextest": nextest,
            "target_triple": target, "host_class": source.host_class()?, "feature": format!("test-shard-{shard}"),
            "jobs": 4, "test_threads": 4});
        for (key, path) in [
            ("runner_blob", "scripts/run-rsid-test-shards.sh"),
            ("checker_blob", "scripts/check-rsid-test-shards.py"),
            ("nextest_config_blob", ".config/nextest.toml"),
        ] {
            inputs[key] = json!(source.command("git", &["rev-parse", &format!("HEAD:{path}")])?);
        }
        shards.push(json!({"name": shard, "run": run, "passed": passed, "failed": failed, "skipped": skipped,
            "exit_code": code.parse::<i64>().map_err(|e| e.to_string())?, "failing_tests": failures(&log),
            "fingerprint": {"digest": sha256(legacy_json(&inputs, true)), "inputs": inputs},
            "environment": {"spec_env": {"CARGO_BUILD_JOBS": "4"}, "tmpdir_class": "disk"}}));
    }
    let mut lanes = Vec::new();
    let lane_labels = [
        ("integrations", "rsid-integrations"),
        ("bins", "rsid-bins"),
        ("rsid-doc", "rsid-doctests"),
        ("rsi", "rsi"),
        ("rsi-common", "rsi-common"),
    ];
    for (name, label) in lane_labels {
        if let Some(code) = rows.get(label) {
            lanes.push(lane(result, name, label, code, name == "rsid-doc")?);
        }
    }
    let mut extra = Vec::new();
    for label in ["other-workspace", "other-doctests", "rolling-library-qa"] {
        if let Some(code) = rows.get(label) {
            extra.push(lane(result, label, label, code, label == "other-doctests")?);
        }
    }
    let expected: BTreeSet<_> = shards
        .iter()
        .map(|s| format!("rsid-{}", s["name"].as_str().unwrap()))
        .chain(lane_labels.into_iter().map(|(_, l)| l.to_owned()))
        .chain(["other-workspace".into(), "other-doctests".into()])
        .collect();
    report["complete"] = json!(
        shards.len() == 16
            && lanes.len() == 5
            && extra.len() == 2
            && rows.keys().cloned().collect::<BTreeSet<_>>() == expected
            && shards.iter().all(|s| s["run"].as_i64().unwrap() > 0
                && s["run"].as_i64().unwrap()
                    == s["passed"].as_i64().unwrap() + s["failed"].as_i64().unwrap())
    );
    report["shards"] = json!(shards);
    report["lanes"] = json!(lanes);
    report["extra_lanes"] = json!(extra);
    Ok(report)
}

fn lane(
    result: &Path,
    name: &str,
    label: &str,
    code: &str,
    doctest: bool,
) -> Result<Value, String> {
    let log = read_lossy(&result.join("logs").join(format!("{label}.log")))?;
    let mut names: BTreeSet<_> = failures(&log).into_iter().collect();
    if doctest {
        static DOC: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"(?m)^---- ([^\n]+) stdout ----$").unwrap());
        names.extend(DOC.captures_iter(&log).map(|m| m[1].to_owned()));
    }
    Ok(
        json!({"name": name, "exit_code": code.parse::<i64>().map_err(|e| e.to_string())?, "failing_tests": names}),
    )
}
