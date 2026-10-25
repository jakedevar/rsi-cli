//! Sweep-report v1 cache importer (#363), staged beside the legacy entry point.
//! Validate every row before writing; exact-key locks and atomic replacement
//! preserve compatibility with running landers and concurrent QA importers.
use crate::Ctx;
use crate::qa_fingerprint::{fingerprint, legacy_json, lowercase_hex, output, sha256};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

pub const SHARDS: &[&str] = &[
    "memory-01",
    "memory-02",
    "other-01",
    "other-02",
    "other-03",
    "other-04",
    "other-05",
    "session-01",
    "session-02",
    "session-03",
    "session-04",
    "session-05",
    "store-01",
    "store-02",
    "store-03",
    "store-04",
];
pub const LANES: &[&str] = &["integrations", "bins", "rsid-doc", "rsi", "rsi-common"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportError {
    Invalid(String),
    CacheConflict(String),
}
impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(s) | Self::CacheConflict(s) => s.fmt(f),
        }
    }
}
impl std::error::Error for ImportError {}
impl From<String> for ImportError {
    fn from(s: String) -> Self {
        Self::Invalid(s)
    }
}
impl From<&str> for ImportError {
    fn from(s: &str) -> Self {
        Self::Invalid(s.into())
    }
}
impl From<std::io::Error> for ImportError {
    fn from(e: std::io::Error) -> Self {
        Self::Invalid(e.to_string())
    }
}
impl From<serde_json::Error> for ImportError {
    fn from(e: serde_json::Error) -> Self {
        Self::Invalid(e.to_string())
    }
}

fn names_match(rows: &Value, expected: &[&str]) -> bool {
    rows.as_array().is_some_and(|rows| {
        rows.len() == expected.len()
            && rows
                .iter()
                .filter_map(|row| row["name"].as_str())
                .collect::<BTreeSet<_>>()
                == expected.iter().copied().collect()
    })
}

pub fn validate_report(report: &Value) -> Result<(), ImportError> {
    if report["schema_version"] != 1 || report["complete"] != true {
        return Err("QA cache import requires a complete sweep-report v1".into());
    }
    if !lowercase_hex(report["swept_sha"].as_str().unwrap_or_default(), 40)
        || !lowercase_hex(report["swept_tree"].as_str().unwrap_or_default(), 40)
    {
        return Err("QA swept SHA/tree must be full lowercase Git OIDs".into());
    }
    if !names_match(&report["shards"], SHARDS) {
        return Err("QA report must contain all 16 distinct shard results".into());
    }
    if !names_match(&report["lanes"], LANES) {
        return Err("QA report must contain all five companion lanes".into());
    }
    let p = &report["provenance"];
    if p["kind"] != "qa_sweep"
        || p["runner"].as_str().unwrap_or_default().is_empty()
        || p["report_ref"].as_str().unwrap_or_default().is_empty()
    {
        return Err("QA report needs sweep provenance".into());
    }
    Ok(())
}

pub fn load_report(path: &Path) -> Result<Value, ImportError> {
    let text = fs::read_to_string(path)?;
    let content = if path.extension().is_some_and(|s| s == "json") {
        text.as_str()
    } else {
        let start = text.find("```qa-sweep-v1").and_then(|i| {
            let tail = &text[i + "```qa-sweep-v1".len()..];
            let newline = tail.find('\n')?;
            tail[..newline]
                .trim()
                .is_empty()
                .then_some(&tail[newline + 1..])
        });
        start
            .and_then(|s| s.split_once("\n```").map(|(json, _)| json))
            .ok_or("Markdown QA report needs a qa-sweep-v1 JSON block")?
    };
    let report = serde_json::from_str(content)?;
    validate_report(&report)?;
    Ok(report)
}

pub fn shard_env_class(spec_env: &serde_json::Map<String, Value>, tmpdir_class: &str) -> String {
    let mut keys: Vec<_> = spec_env.keys().filter(|k| *k != "TMPDIR").collect();
    keys.sort();
    let mut canonical = String::new();
    for key in keys {
        canonical.push_str(&format!(
            "{key}={}\n",
            spec_env[key].as_str().unwrap_or_default()
        ));
    }
    canonical.push_str(&format!("TMPDIR={tmpdir_class}\n"));
    format!("sha256:{}", sha256(canonical))
}

pub fn shard_cache_key(fingerprint: &str, env_class: &str) -> String {
    format!("sha256:{}", sha256(format!("{fingerprint}\n{env_class}")))
}

fn valid_env_key(s: &str) -> bool {
    let mut chars = s.bytes();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

pub fn recorded_env_class(row: &Value, shard: &str) -> Result<Option<String>, ImportError> {
    let env = &row["environment"];
    if env.is_null() {
        return Ok(None);
    }
    let spec = env["spec_env"].as_object();
    let class = env["tmpdir_class"].as_str();
    match (spec, class) {
        (Some(spec), Some(class @ ("disk" | "tmpfs")))
            if spec.iter().all(|(k, v)| {
                valid_env_key(k) && v.as_str().is_some_and(|v| !v.contains('\n'))
            }) =>
        {
            Ok(Some(shard_env_class(spec, class)))
        }
        _ => Err(format!("invalid QA environment for {shard}").into()),
    }
}

/// The injected tree and fingerprint readers make the same trust boundary
/// testable without tools or process-global environment changes.
pub fn import_report_with(
    report: &Value,
    root: &Path,
    tree: &str,
    mut local_fingerprint: impl FnMut(&str, &str, u32) -> Result<Value, String>,
) -> Result<(String, Vec<(String, String)>), ImportError> {
    validate_report(report)?;
    let tip = report["swept_sha"]
        .as_str()
        .ok_or("QA swept SHA is missing")?;
    if tree != report["swept_tree"] {
        return Err("QA swept tree does not match the reported commit".into());
    }
    let provenance = &report["provenance"];
    let source = format!(
        "qa:{}:{}",
        provenance["runner"].as_str().unwrap_or_default(),
        provenance["report_ref"].as_str().unwrap_or_default()
    );
    let mut rows: Vec<_> = report["shards"]
        .as_array()
        .ok_or("QA shards missing")?
        .iter()
        .collect();
    rows.sort_by_key(|row| row["name"].as_str());
    let (mut results, mut prepared) = (Vec::new(), Vec::new());
    for row in rows {
        let shard = row["name"].as_str().ok_or("QA shard name missing")?;
        let fail = |message: &str| ImportError::Invalid(format!("{message} for {shard}"));
        let names = row["failing_tests"]
            .as_array()
            .ok_or_else(|| fail("QA failing tests missing"))?;
        if names.iter().any(|v| !v.is_string()) {
            return Err(fail("QA failing tests missing"));
        }
        let mut names: Vec<String> = names
            .iter()
            .map(|v| {
                v.as_str()
                    .unwrap_or_default()
                    .strip_prefix("rsid ")
                    .unwrap_or(v.as_str().unwrap_or_default())
                    .to_owned()
            })
            .collect();
        names.sort();
        if names.windows(2).any(|pair| pair[0] == pair[1])
            || names.iter().any(|s| {
                s.is_empty()
                    || !s
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"_:.-".contains(&c))
            })
        {
            return Err(fail("QA failure names invalid"));
        }
        let (Some(passed), Some(failed), Some(run)) = (
            row["passed"].as_u64(),
            row["failed"].as_u64(),
            row["run"].as_u64(),
        ) else {
            return Err(fail("QA test counts do not match failures"));
        };
        if passed.checked_add(failed) != Some(run) || failed != names.len() as u64 {
            return Err(fail("QA test counts do not match failures"));
        }
        if (failed == 0 && row["exit_code"] != 0) || (failed > 0 && row["exit_code"] != 100) {
            results.push((shard.to_owned(), "abnormal_skipped".to_owned()));
            continue;
        }
        let fingerprint = &row["fingerprint"];
        let inputs = &fingerprint["inputs"];
        let digest = fingerprint["digest"].as_str().unwrap_or_default();
        if !lowercase_hex(digest, 64) {
            return Err(fail("invalid QA fingerprint"));
        }
        if sha256(legacy_json(inputs, true)) != digest
            || inputs["feature"] != format!("test-shard-{shard}")
        {
            return Err(format!("QA fingerprint inputs do not match {shard}").into());
        }
        let jobs = inputs["jobs"]
            .as_u64()
            .filter(|j| (1..=64).contains(j))
            .ok_or_else(|| fail("invalid QA jobs"))? as u32;
        let local = local_fingerprint(tip, shard, jobs)?;
        if inputs["host_class"] != local["inputs"]["host_class"] {
            return Err(fail("QA host class does not match this host"));
        }
        if &local != fingerprint {
            return Err(fail(
                "QA runner/toolchain fingerprint does not match this host",
            ));
        }
        let Some(env_class) = recorded_env_class(row, shard)? else {
            results.push((shard.to_owned(), "environment_unrecorded".to_owned()));
            continue;
        };
        prepared.push((
            shard.to_owned(),
            shard_cache_key(&format!("sha256:{digest}"), &env_class),
            names,
        ));
    }
    for (shard, key, failures) in prepared {
        let dir = root
            .join(tip)
            .join(key.strip_prefix("sha256:").unwrap_or(&key));
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&dir)?;
        let target = dir.join(format!("{shard}.json"));
        let entry = json!({"schema_version": 1, "rolling_sha": tip, "shard": shard, "fingerprint": key, "failures": failures, "provenance": source});
        let lock = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(dir.join(format!("{shard}.lock")))?;
        // std File::lock uses flock on Unix, sharing the existing Python/Rust
        // lander lock namespace; Windows uses the native portable equivalent.
        lock.lock()?;
        if target.exists() {
            let previous: Value = serde_json::from_slice(&fs::read(&target)?)?;
            if ["rolling_sha", "shard", "fingerprint", "failures"]
                .iter()
                .any(|key| previous[key] != entry[key])
            {
                return Err(ImportError::CacheConflict(format!(
                    "conflicting cached base result for {tip} {shard}"
                )));
            }
            results.push((shard, "existing".into()));
            continue;
        }
        let mut temporary = tempfile::NamedTempFile::new_in(&dir)?;
        temporary.write_all(legacy_json(&entry, false).as_bytes())?;
        temporary.flush()?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(&target)
            .map_err(|e| ImportError::Invalid(e.to_string()))?;
        results.push((shard, "imported".into()));
    }
    Ok((tip.to_owned(), results))
}

pub fn import_report(
    ctx: &Ctx,
    path: &Path,
    root: &Path,
) -> Result<(String, Vec<(String, String)>), ImportError> {
    let report = load_report(path)?;
    let tip = report["swept_sha"]
        .as_str()
        .ok_or("QA swept SHA is missing")?;
    let tree = output(
        &ctx.cwd,
        "git",
        &["rev-parse", "--verify", &format!("{tip}^{{tree}}")],
    )?;
    import_report_with(&report, root, &tree, |sha, shard, jobs| {
        fingerprint(ctx, sha, shard, jobs)
    })
}

pub fn main(args: &[String], ctx: &Ctx, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    if args.len() != 1 {
        let _ = writeln!(err, "usage: import-rolling-qa-cache REPORT");
        return 2;
    }
    let root = ctx.var("RSI_ROLLING_BASE_CACHE_DIR").map_or_else(
        || ctx.home().join(".rsi/cache/rolling-base-shards-v1"),
        |s| ctx.path(s),
    );
    match import_report(ctx, &ctx.path(&args[0]), &root) {
        Ok((tip, rows)) => {
            for (shard, state) in rows {
                let _ = writeln!(out, "qa_cache_{state}={tip}:{shard}");
            }
            0
        }
        Err(e) => {
            let _ = writeln!(err, "import-rolling-qa-cache: {e}");
            1
        }
    }
}

#[cfg(test)]
#[path = "qa_cache_tests.rs"]
mod tests;
