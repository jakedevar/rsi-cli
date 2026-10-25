//! Slow-tier decisions; ambiguous edits retain the full tier.
use super::git::{Repository, changed_line_ranges};
use super::source::rust_functions;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::io;

pub const CONFIG: &str = ".config/slow-tier.toml";
pub const NEXTEST_CONFIG: &str = ".config/nextest.toml";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct SlowTier {
    pub package: String,
    pub trigger_paths: Vec<String>,
    pub tests: Vec<String>,
}
fn valid_name(s: &str, extra: char) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == extra)
}
pub fn parse_slow_tier(text: &str) -> Result<SlowTier, String> {
    let data: toml::Value = toml::from_str(text).map_err(|e| e.to_string())?;
    let package = data
        .get("package")
        .and_then(toml::Value::as_str)
        .filter(|s| valid_name(s, '-'))
        .ok_or("slow-tier package must be a package name")?;
    fn list(data: &toml::Value, key: &str, error: &str) -> Result<Vec<String>, String> {
        let array = data.get(key).and_then(toml::Value::as_array).ok_or(error)?;
        array
            .iter()
            .map(|v| {
                v.as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .ok_or_else(|| error.to_owned())
            })
            .collect()
    }
    let trigger_paths = list(
        &data,
        "trigger_paths",
        "slow-tier trigger_paths must be a list of paths",
    )?;
    let tests = list(
        &data,
        "tests",
        "slow-tier tests must be a list of test names ([A-Za-z0-9_:])",
    )?;
    if tests.iter().any(|s| !valid_name(s, ':')) {
        return Err("slow-tier tests must be a list of test names ([A-Za-z0-9_:])".into());
    }
    if tests.iter().collect::<BTreeSet<_>>().len() != tests.len() {
        return Err("slow-tier tests lists a name twice".into());
    }
    Ok(SlowTier {
        package: package.into(),
        trigger_paths,
        tests,
    })
}
pub fn slow_tier_filter(config: &SlowTier) -> String {
    let mut names = config.tests.clone();
    names.sort();
    format!(
        "not (package({}) & test(/^(?:{})$/))",
        config.package,
        names.join("|")
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestSpan {
    pub first: usize,
    pub last: usize,
    pub name: String,
}
/// Slow-tier span grammar intentionally differs from the ordinary function
/// scanner: it accepts multiline attributes and rejects malformed test bodies.
pub fn test_fn_spans(text: &str) -> Option<Vec<TestSpan>> {
    let rows: Vec<_> = text.lines().collect();
    let pattern =
        crate::re::Re::new(r"^(\s*)(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:unsafe\s+)?fn\s+(\w+)");
    let test_attr = crate::re::Re::new(r"#\[(?:test|tokio::test|test_case)\b");
    let mut spans = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        let Some(c) = pattern.search(row) else {
            continue;
        };
        let indent = crate::re::group(&c, 1);
        let mut first = index;
        let mut is_test = false;
        let mut probe = index;
        while probe > 0 && index - (probe - 1) <= 24 {
            probe -= 1;
            let stripped = rows[probe].trim();
            let deeper = rows[probe].len() - rows[probe].trim_start().len() > indent.len();
            if stripped.is_empty()
                || !(deeper
                    || stripped.starts_with("#[")
                    || stripped.starts_with("//")
                    || stripped.starts_with(')')
                    || stripped.starts_with(']'))
            {
                break;
            }
            is_test |= test_attr.is_match(stripped);
            first = probe;
        }
        if !is_test {
            continue;
        }
        let last = if row.trim_end().ends_with('}')
            && row.matches('{').count() == row.matches('}').count()
        {
            index
        } else {
            (index + 1..rows.len()).find(|i| rows[*i].trim_end() == format!("{indent}}}"))?
        };
        if (index + 1..last).any(|i| {
            pattern
                .search(rows[i])
                .is_some_and(|c| crate::re::group(&c, 1) == indent)
        }) {
            return None;
        }
        spans.push(TestSpan {
            first: first + 1,
            last: last + 1,
            name: crate::re::group(&c, 2).into(),
        });
    }
    Some(spans)
}

pub fn slow_test_source_change(
    before: Option<&str>,
    after: Option<&str>,
    diff: &str,
    slow_names: &BTreeSet<String>,
) -> Option<usize> {
    let declares = slow_names.iter().any(|name| {
        let pattern = crate::re::Re::new(&format!(r"\bfn\s+{}\b", crate::re::escape(name)));
        [before, after]
            .into_iter()
            .flatten()
            .any(|text| pattern.is_match(text))
    });
    if !declares {
        return None;
    }
    let (Some(before), Some(after)) = (before, after) else {
        return Some(1);
    };
    let (Some(old_spans), Some(new_spans)) = (test_fn_spans(before), test_fn_spans(after)) else {
        return Some(1);
    };
    let old_rows: Vec<_> = before.lines().collect();
    let new_rows: Vec<_> = after.lines().collect();
    for hunk in changed_line_ranges(diff) {
        for (first, count, rows, spans) in [
            (hunk.old_first, hunk.old_count, &old_rows, &old_spans),
            (hunk.new_first, hunk.new_count, &new_rows, &new_spans),
        ] {
            for number in first..first + count {
                if number == 0 || rows.get(number - 1).is_none_or(|r| r.trim().is_empty()) {
                    continue;
                }
                if !spans
                    .iter()
                    .any(|s| s.first <= number && number <= s.last && !slow_names.contains(&s.name))
                {
                    return Some(number);
                }
            }
        }
    }
    None
}

fn blob(repo: &Repository, revision: &str, path: &str) -> io::Result<Option<String>> {
    let files = repo.files(revision)?;
    if !files.iter().any(|p| p == path) {
        return Ok(None);
    }
    Ok(repo.blobs(revision, &[path.into()])?.remove(path))
}

pub fn decision(
    repo: &Repository,
    base: &str,
    head: &str,
    changed: &[String],
) -> serde_json::Value {
    let run = || -> Result<serde_json::Value, String> {
        let Some(text) = blob(repo, head, CONFIG).map_err(|e| e.to_string())? else {
            return Ok(serde_json::json!({"mode":"full","reason":"no_slow_tier_config","tests":0}));
        };
        let config = parse_slow_tier(&text)?;
        let count = config.tests.len();
        for path in changed {
            if config
                .trigger_paths
                .iter()
                .any(|t| path == t || (t.ends_with('/') && path.starts_with(t)))
            {
                return Ok(
                    serde_json::json!({"mode":"full","reason":"trigger_path","path":path,"tests":count}),
                );
            }
        }
        let slow_names: BTreeSet<String> = config
            .tests
            .iter()
            .map(|s| s.rsplit("::").next().unwrap_or(s).into())
            .collect();
        let prefix = format!("crates/{}/src/", config.package);
        for path in changed
            .iter()
            .filter(|p| p.starts_with(&prefix) && p.ends_with(".rs"))
        {
            let before = blob(repo, base, path).map_err(|e| e.to_string())?;
            let after = blob(repo, head, path).map_err(|e| e.to_string())?;
            let diff = repo.diff(base, head, path).map_err(|e| e.to_string())?;
            if let Some(line) =
                slow_test_source_change(before.as_deref(), after.as_deref(), &diff, &slow_names)
            {
                return Ok(
                    serde_json::json!({"mode":"full","reason":"slow_test_source","path":path,"line":line,"tests":count}),
                );
            }
        }
        Ok(serde_json::json!({"mode":"excluded","reason":"no_trigger","tests":count}))
    };
    run().unwrap_or_else(
        |e| serde_json::json!({"mode":"full","reason":format!("decision_error: {e}"),"tests":0}),
    )
}

/// Exact generator contract, without a subprocess or an interpreter.
pub fn write_slow_tier_filter(root: &std::path::Path) -> Result<String, String> {
    let config =
        parse_slow_tier(&std::fs::read_to_string(root.join(CONFIG)).map_err(|e| e.to_string())?)?;
    let text = std::fs::read_to_string(root.join(NEXTEST_CONFIG)).map_err(|e| e.to_string())?;
    let re = crate::re::Re::new(r"(?m)^default-filter = 'not \(package\(.*$");
    if re.find_all(&text).len() != 1 {
        return Err(format!(
            "{NEXTEST_CONFIG} must hold exactly one generated `default-filter = 'not (package(` line"
        ));
    }
    let generated = re.sub_with(&text, &|_| {
        format!("default-filter = '{}'", slow_tier_filter(&config))
    });
    std::fs::write(root.join(NEXTEST_CONFIG), generated).map_err(|e| e.to_string())?;
    Ok(format!(
        "check-touched-shards: wrote the slow-tier default-filter ({} tests) to {NEXTEST_CONFIG}",
        config.tests.len()
    ))
}

/// Used by downstream API classification without dropping host-inert sources.
pub fn only_test_lines(text: &str, lines: &BTreeSet<usize>, host: &str) -> bool {
    let rows: Vec<_> = text.lines().collect();
    let functions = rust_functions(text, host);
    let scopes = super::source::inline_scopes("", text).unwrap_or_default();
    lines.iter().all(|n| {
        let row = n
            .checked_sub(1)
            .and_then(|i| rows.get(i))
            .map_or("", |s| s.trim());
        row.is_empty()
            || row.starts_with("//")
            || n.checked_sub(1)
                .and_then(|i| scopes.get(i))
                .is_some_and(|(m, _)| super::source::is_test_module(m))
            || functions.iter().any(|f| {
                (super::source::is_test(&f.attrs) || f.attrs.contains("#[cfg(test)]"))
                    && f.attr_start < *n
                    && *n <= f.end
            })
    })
}
