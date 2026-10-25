use super::{files, read_lossy};
use regex::Regex;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;

type Matches = BTreeMap<String, (String, String)>;
#[derive(Debug)]
pub enum Classification {
    Ok(Matches),
    Unavailable(String),
}
static NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^- `([^`]+)`$").unwrap());
static HARNESS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^- `([^`]+):build_or_harness_failure`$").unwrap());
static CLASS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^- `([^`]+)` \[(crash|timeout)\]$").unwrap());

pub fn build_verdict(qa: &str, sha: &str, classification: Classification) -> String {
    static LANES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^Lanes: (\d+)").unwrap());
    static BLOCK: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?ms)^Unclassified failing names[^\n]*\n(.*?)^FLAKE lines:").unwrap()
    });
    let lanes = LANES
        .captures(qa)
        .and_then(|m| m[1].parse::<u64>().ok())
        .unwrap_or(0);
    let harness: Vec<_> = HARNESS.captures_iter(qa).map(|m| m[1].to_owned()).collect();
    let block = BLOCK
        .captures(qa)
        .map(|m| m[1].to_owned())
        .unwrap_or_default();
    let names: Vec<_> = NAME
        .captures_iter(&block)
        .map(|m| m[1].to_owned())
        .filter(|s| !s.ends_with(":build_or_harness_failure") && s != "none")
        .collect();
    let incomplete = lanes == 0 || !harness.is_empty();
    let mut lines = vec![String::new(), "## Verdict".into(), String::new()];
    if incomplete {
        let mut reasons = Vec::new();
        if lanes == 0 {
            reasons.push("Lanes: 0".into());
        }
        reasons.extend(
            harness
                .iter()
                .map(|l| format!("{l}:build_or_harness_failure")),
        );
        lines.push(format!("Incomplete: {}", reasons.join(", ")));
    }
    let matches = match classification {
        Classification::Ok(matches) => matches,
        Classification::Unavailable(reason) => {
            lines.push(format!(
                "Classification unavailable: {reason}; reds are unclassified."
            ));
            BTreeMap::new()
        }
    };
    let classes: BTreeMap<_, _> = CLASS
        .captures_iter(qa)
        .map(|m| (m[1].to_owned(), m[2].to_owned()))
        .collect();
    let mut known = BTreeMap::new();
    let mut new = Vec::new();
    for name in &names {
        let tag = classes
            .get(name)
            .map(|c| format!(" [{c}]"))
            .unwrap_or_default();
        let (kind, issue) = matches
            .get(name)
            .map(|(k, i)| (k.as_str(), i.as_str()))
            .unwrap_or(("new", ""));
        if kind == "known" {
            known.insert(name.clone(), issue.to_owned());
            lines.push(format!("- KNOWN {issue} `{name}`{tag}"));
        } else {
            new.push(name);
            let note = if kind == "name-only" {
                format!(" (name-only match {issue}; signature unverified)")
            } else {
                String::new()
            };
            lines.push(format!("- NEW `{name}`{tag}{note}"));
        }
    }
    if names.is_empty() {
        lines.push("- no failing test names".into());
    }
    let crashed = new.iter().filter(|n| classes.contains_key(**n)).count();
    if crashed > 0 {
        lines.push(format!(
            "Crashed or timed out (NEW): {crashed}; the verdict is RED, not INCOMPLETE."
        ));
    }
    let state = if incomplete {
        "INCOMPLETE"
    } else if !new.is_empty() {
        "RED"
    } else {
        let mut issues: Vec<_> = known
            .values()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        issues.sort_by_key(|v| v.trim_start_matches('#').parse::<u64>().unwrap_or(0));
        if !issues.is_empty() {
            lines.push(format!(
                "Known only: {}",
                issues
                    .into_iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        "GREEN"
    };
    format!(
        "{}\n\nVERDICT {state} {sha} new={}\n",
        lines.join("\n"),
        new.len()
    )
}

fn find_classifier(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = explicit.filter(|p| !p.as_os_str().is_empty()) {
        return path.is_file().then(|| path.to_owned());
    }
    if let Some(path) = std::env::var_os("RSI_KNOWN_FAILURE_BIN").filter(|s| !s.is_empty()) {
        let path = PathBuf::from(path);
        return path.is_file().then_some(path);
    }
    let name = if cfg!(windows) {
        "rsi-known-failure.exe"
    } else {
        "rsi-known-failure"
    };
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let path = dir.join(name);
            #[cfg(unix)]
            let executable = {
                use std::os::unix::fs::PermissionsExt;
                path.metadata()
                    .is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
            };
            #[cfg(not(unix))]
            let executable = path.is_file();
            if path.is_file() && executable {
                return Some(path);
            }
        }
    }
    let mut targets: Vec<_> = std::env::var_os("CARGO_TARGET_DIR")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .into_iter()
        .collect();
    targets.push(crate::util::home_dir().join(".cargo/shared-target"));
    targets
        .into_iter()
        .map(|p| p.join("debug").join(name))
        .find(|p| p.is_file())
}

fn parse_classification(text: &str) -> Matches {
    let mut matches = BTreeMap::new();
    for line in text.lines().map(str::trim) {
        if line.starts_with("KNOWN?") {
            let parts = fields(line, 3);
            if parts.len() == 3 {
                matches.insert(parts[2].to_owned(), ("name-only".into(), parts[1].into()));
            }
        } else if line.starts_with("KNOWN ") {
            let parts = fields(line, 4);
            if parts.len() == 4 {
                matches.insert(parts[3].to_owned(), ("known".into(), parts[1].into()));
            }
        } else if let Some(name) = line.strip_prefix("NEW ") {
            matches.insert(name.trim().into(), ("new".into(), String::new()));
        }
    }
    matches
}

// split(None, maxsplit) keeps whitespace inside the final test name.
fn fields(mut line: &str, count: usize) -> Vec<&str> {
    let mut parts = Vec::new();
    while parts.len() + 1 < count {
        let Some(end) = line.find(char::is_whitespace) else {
            break;
        };
        parts.push(&line[..end]);
        line = line[end..].trim_start();
    }
    if !line.is_empty() {
        parts.push(line);
    }
    parts
}

fn classify_logs(classifier: &Path, snapshot: &Path, result: &Path) -> Result<Matches, String> {
    let mut logs = Vec::new();
    let mut seen = BTreeSet::new();
    for path in files(&result.join("failure-logs"), ".log.gz")? {
        let lane = path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .strip_suffix(".log.gz")
            .unwrap()
            .to_owned();
        seen.insert(lane.clone());
        logs.push((lane, path));
    }
    for path in files(&result.join("failure-logs"), ".txt")? {
        let lane = path.file_stem().unwrap().to_string_lossy().into_owned();
        if seen.insert(lane.clone()) {
            logs.push((lane, path));
        }
    }
    let scratch = tempfile::tempdir().map_err(|e| e.to_string())?;
    let mut matches = BTreeMap::new();
    for (lane, path) in logs {
        let log = scratch.path().join(format!("{lane}.log"));
        if path.extension().is_some_and(|s| s == "gz") {
            let source = std::fs::File::open(&path).map_err(|e| e.to_string())?;
            let mut bytes = Vec::new();
            flate2::read::MultiGzDecoder::new(source)
                .read_to_end(&mut bytes)
                .map_err(|e| e.to_string())?;
            std::fs::write(&log, bytes).map_err(|e| e.to_string())?;
        } else {
            std::fs::copy(&path, &log).map_err(|e| e.to_string())?;
        }
        let output = Command::new(classifier)
            .arg("classify")
            .arg("--log")
            .arg(&log)
            .arg("--snapshot")
            .arg(snapshot)
            .output()
            .map_err(|e| e.to_string())?;
        if !output.status.success() {
            return Err(format!(
                "Command '{}' returned non-zero exit status {}.",
                classifier.display(),
                output.status.code().unwrap_or(-1)
            ));
        }
        let output = String::from_utf8(output.stdout).map_err(|e| e.to_string())?;
        matches.extend(parse_classification(&output));
    }
    Ok(matches)
}

/// Append and return exactly the section printed to stdout, including newline.
pub fn verdict(
    result: &Path,
    snapshot: &Path,
    classifier: Option<&Path>,
) -> Result<String, String> {
    let qa = result.join("QA.md");
    let text = if qa.exists() {
        read_lossy(&qa)?
    } else {
        String::new()
    };
    static HEAD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"Tip SHA: `([^`]+)`").unwrap());
    let sha = HEAD
        .captures(&text)
        .map(|m| m[1].to_owned())
        .unwrap_or_else(|| {
            result
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        });
    let classification = match find_classifier(classifier) {
        None => Classification::Unavailable("rsi-known-failure CLI not found".into()),
        Some(_) if !snapshot.is_file() => {
            Classification::Unavailable(format!("snapshot {} missing", snapshot.display()))
        }
        Some(classifier) => match classify_logs(&classifier, snapshot, result) {
            Ok(matches) => Classification::Ok(matches),
            Err(e) => Classification::Unavailable(format!("classifier failed: {e}")),
        },
    };
    let section = build_verdict(&text, &sha, classification);
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&qa)
        .and_then(|mut f| f.write_all(section.as_bytes()))
        .map_err(|e| e.to_string())?;
    Ok(section)
}

pub(super) fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    if args
        .iter()
        .take_while(|s| s.as_str() != "--")
        .any(|s| s == "--help" || s == "-h")
    {
        let _ = writeln!(
            out,
            "usage: rsi-cloud sweep-verdict RESULT_DIR [--snapshot FILE] [--classifier BIN]"
        );
        return 0;
    }
    let parsed = match crate::args::parse(
        args,
        &[
            ("--snapshot", crate::args::Kind::Value),
            ("--classifier", crate::args::Kind::Value),
        ],
    ) {
        Ok(parsed) => parsed,
        Err(e) => {
            let _ = writeln!(err, "{e}");
            return 2;
        }
    };
    let [result] = parsed.positionals.as_slice() else {
        let _ = writeln!(
            err,
            "usage: rsi-cloud sweep-verdict RESULT_DIR [--snapshot FILE] [--classifier BIN]"
        );
        return 2;
    };
    let snapshot = parsed
        .value("--snapshot")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::util::home_dir().join(".rsi/qa/known-failures.v1.json"));
    match verdict(
        Path::new(result),
        &snapshot,
        parsed.value("--classifier").map(Path::new),
    ) {
        Ok(section) => match out.write_all(section.as_bytes()) {
            Ok(()) => 0,
            Err(e) => {
                let _ = writeln!(err, "{e}");
                1
            }
        },
        Err(e) => {
            let _ = writeln!(err, "{e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn classifier_grammar_preserves_name_spaces_and_last_match() {
        let matches = parse_classification(
            "  KNOWN   #9   flake   crate test with spaces  \nKNOWN?   #7,#8   name with spaces\nNEW ignored\nKNOWN #2 crash ignored\n",
        );
        assert_eq!(
            matches,
            BTreeMap::from([
                (
                    "crate test with spaces".into(),
                    ("known".into(), "#9".into())
                ),
                (
                    "name with spaces".into(),
                    ("name-only".into(), "#7,#8".into())
                ),
                ("ignored".into(), ("known".into(), "#2".into())),
            ])
        );
    }
}
