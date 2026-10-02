//! Known-failure signature CLI: export, classify, query and block (#1016).

use rsi_common::failure_signature::{
    Classification, DEFAULT_SNAPSHOT_RELATIVE, FENCE_INFO, Matcher, SCHEMA_VERSION, SignatureClass,
    SignatureRecord, Snapshot, build_snapshot, classify, digest, failing_tests, failure_text_for,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("export") => cmd_export(&args[1..]),
        Some("classify") => cmd_classify(&args[1..]),
        Some("query") => cmd_query(&args[1..]),
        Some("block") => cmd_block(&args[1..]),
        other => {
            eprintln!("usage: rsi-known-failure <export|classify|query|block> ...");
            if let Some(other) = other {
                eprintln!("unknown subcommand: {other}");
            }
            ExitCode::from(2)
        }
    }
}

#[derive(Default)]
struct Flags {
    positional: Vec<String>,
    options: BTreeMap<String, Vec<String>>,
}

impl Flags {
    fn parse(args: &[String]) -> Self {
        let mut flags = Self::default();
        let mut index = 0;
        while index < args.len() {
            let arg = &args[index];
            let Some(name) = arg.strip_prefix("--") else {
                flags.positional.push(arg.clone());
                index += 1;
                continue;
            };
            if let Some((key, value)) = name.split_once('=') {
                flags
                    .options
                    .entry(key.to_string())
                    .or_default()
                    .push(value.to_string());
                index += 1;
                continue;
            }
            if index + 1 < args.len() && !args[index + 1].starts_with("--") {
                flags
                    .options
                    .entry(name.to_string())
                    .or_default()
                    .push(args[index + 1].clone());
                index += 2;
            } else {
                flags
                    .options
                    .entry(name.to_string())
                    .or_default()
                    .push(String::new());
                index += 1;
            }
        }
        flags
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.options
            .get(name)
            .and_then(|values| values.last())
            .map(String::as_str)
    }

    fn all(&self, name: &str) -> &[String] {
        self.options.get(name).map_or(&[], Vec::as_slice)
    }

    fn required(&self, name: &str) -> Result<&str, String> {
        self.get(name)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| format!("missing required --{name}"))
    }
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from)
}

fn default_snapshot() -> PathBuf {
    home_dir().join(DEFAULT_SNAPSHOT_RELATIVE)
}

fn read_snapshot(path: &Path) -> Result<Snapshot, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read snapshot {}: {error}", path.display()))?;
    serde_json::from_str(&text)
        .map_err(|error| format!("cannot parse snapshot {}: {error}", path.display()))
}

fn warn_if_stale(snapshot: &Snapshot) {
    if let Some(warning) = snapshot.stale_warning(chrono::Utc::now()) {
        eprintln!("warning: {warning}");
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }
    let temp = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&temp, bytes)
        .map_err(|error| format!("cannot write {}: {error}", temp.display()))?;
    std::fs::rename(&temp, path).map_err(|error| {
        format!(
            "cannot rename {} to {}: {error}",
            temp.display(),
            path.display()
        )
    })
}

fn cmd_export(args: &[String]) -> ExitCode {
    let flags = Flags::parse(args);
    let issues_json = match flags.required("issues-json") {
        Ok(value) => value.to_string(),
        Err(error) => {
            eprintln!("{error}");
            eprintln!(
                "usage: rsi-known-failure export --issues-json <file> --out <file> [--source-rolling <sha>]"
            );
            return ExitCode::from(2);
        }
    };
    let out = match flags.required("out") {
        Ok(value) => PathBuf::from(value),
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(2);
        }
    };
    let raw = match std::fs::read_to_string(&issues_json) {
        Ok(raw) => raw,
        Err(error) => {
            eprintln!("cannot read {issues_json}: {error}");
            return ExitCode::from(2);
        }
    };
    let rows: Vec<serde_json::Value> = match serde_json::from_str(&raw) {
        Ok(rows) => rows,
        Err(error) => {
            eprintln!("invalid issues JSON {issues_json}: {error}");
            return ExitCode::from(2);
        }
    };
    let mut issues = Vec::with_capacity(rows.len());
    for row in &rows {
        let number = row
            .get("display_number")
            .and_then(serde_json::Value::as_u64);
        // An archived Issue keeps its status (often Open); it must not export.
        let archived = row.get("archived_at").is_some_and(|value| !value.is_null());
        if archived {
            continue;
        }
        let status = row.get("status").and_then(serde_json::Value::as_str);
        let body = row.get("body").and_then(serde_json::Value::as_str);
        match (number, status, body) {
            (Some(number), Some(status), Some(body)) => {
                issues.push((number, status.to_string(), body.to_string()));
            }
            _ => {
                eprintln!("skipping row without display_number/status/body: {row}");
            }
        }
    }
    let built = build_snapshot(&issues, flags.get("source-rolling").map(str::to_string));
    let json = match serde_json::to_string_pretty(&built.snapshot) {
        Ok(json) => json,
        Err(error) => {
            eprintln!("cannot serialize snapshot: {error}");
            return ExitCode::from(2);
        }
    };
    if let Err(error) = write_atomic(&out, format!("{json}\n").as_bytes()) {
        eprintln!("{error}");
        return ExitCode::from(2);
    }
    println!(
        "issues={} records={} errors={} out={}",
        issues.len(),
        built.snapshot.records.len(),
        built.errors.len(),
        out.display()
    );
    for error in &built.errors {
        eprintln!("block error: {error}");
    }
    ExitCode::SUCCESS
}

fn cmd_classify(args: &[String]) -> ExitCode {
    let flags = Flags::parse(args);
    let log_path = match flags.required("log") {
        Ok(value) => value.to_string(),
        Err(error) => {
            eprintln!("{error}");
            eprintln!(
                "usage: rsi-known-failure classify --log <nextest-log> [--snapshot <file>] [--host <h>]"
            );
            return ExitCode::from(2);
        }
    };
    let snapshot_path = flags
        .get("snapshot")
        .map_or_else(default_snapshot, PathBuf::from);
    let snapshot = match read_snapshot(&snapshot_path) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(2);
        }
    };
    let log = match std::fs::read_to_string(&log_path) {
        Ok(log) => log,
        Err(error) => {
            eprintln!("cannot read log {log_path}: {error}");
            return ExitCode::from(2);
        }
    };
    warn_if_stale(&snapshot);
    let host = flags.get("host");
    for test in failing_tests(&log) {
        let text = failure_text_for(&log, &test);
        match classify(&snapshot, &test, &text, host) {
            Classification::Known { issue, class, .. } => {
                println!("KNOWN #{issue} {} {test}", class.as_str());
            }
            Classification::NameOnly { issues } => {
                let joined = issues
                    .iter()
                    .map(|issue| format!("#{issue}"))
                    .collect::<Vec<_>>()
                    .join(",");
                println!("KNOWN? {joined} {test}");
            }
            Classification::New => println!("NEW {test}"),
        }
    }
    ExitCode::SUCCESS
}

fn cmd_query(args: &[String]) -> ExitCode {
    let flags = Flags::parse(args);
    let Some(test_id) = flags.positional.first() else {
        eprintln!("usage: rsi-known-failure query [--snapshot <file>] <test-id>");
        return ExitCode::from(2);
    };
    let test_id = test_id.clone();
    let snapshot_path = flags
        .get("snapshot")
        .map_or_else(default_snapshot, PathBuf::from);
    let snapshot = match read_snapshot(&snapshot_path) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(2);
        }
    };
    warn_if_stale(&snapshot);
    for entry in snapshot.live_records() {
        if entry.record.test_id == test_id {
            match serde_json::to_string(&entry.record) {
                Ok(json) => println!("{json}"),
                Err(error) => {
                    eprintln!("cannot serialize record: {error}");
                    return ExitCode::from(2);
                }
            }
        }
    }
    ExitCode::SUCCESS
}

fn cmd_block(args: &[String]) -> ExitCode {
    let flags = Flags::parse(args);
    let test_id = match flags.required("test") {
        Ok(value) => value.to_string(),
        Err(error) => {
            eprintln!("{error}");
            eprintln!(
                "usage: rsi-known-failure block --test <id> --issue <n> --class <c> [--log <file>] [--contains <s>]..."
            );
            return ExitCode::from(2);
        }
    };
    let issue: u64 = match flags.required("issue").and_then(|value| {
        value
            .parse()
            .map_err(|_| format!("--issue must be a number, got {value}"))
    }) {
        Ok(issue) => issue,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(2);
        }
    };
    let class = match flags.required("class").and_then(parse_class) {
        Ok(class) => class,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(2);
        }
    };
    let mut matcher = Matcher {
        digest: None,
        contains: flags
            .all("contains")
            .iter()
            .filter(|value| !value.is_empty())
            .cloned()
            .collect(),
    };
    if let Some(log_path) = flags.get("log") {
        let log = match std::fs::read_to_string(log_path) {
            Ok(log) => log,
            Err(error) => {
                eprintln!("cannot read log {log_path}: {error}");
                return ExitCode::from(2);
            }
        };
        matcher.digest = Some(digest(&failure_text_for(&log, &test_id)));
    }
    let record = SignatureRecord {
        schema_version: SCHEMA_VERSION,
        test_id,
        matcher,
        issue,
        class,
        host_scope: flags.get("host").map(str::to_string),
        note: flags.get("note").map(str::to_string),
    };
    if let Err(error) = record.validate() {
        eprintln!("refusing to print invalid record: {error}");
        return ExitCode::from(2);
    }
    let json = serde_json::to_string_pretty(&record).unwrap_or_else(|_| "{}".to_string());
    println!("```{FENCE_INFO}");
    println!("{json}");
    println!("```");
    ExitCode::SUCCESS
}

fn parse_class(value: &str) -> Result<SignatureClass, String> {
    match value {
        "regression" => Ok(SignatureClass::Regression),
        "flake" => Ok(SignatureClass::Flake),
        "env" => Ok(SignatureClass::Env),
        "seed" => Ok(SignatureClass::Seed),
        other => Err(format!(
            "--class must be regression|flake|env|seed, got {other:?}"
        )),
    }
}
