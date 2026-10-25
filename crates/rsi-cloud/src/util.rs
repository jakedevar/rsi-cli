//! Helpers shared by the three commands: UTC timestamps, advisory file locks,
//! Python-compatible line splitting and glob matching, and value printing.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Utc};
use serde_json::Value;

/// The timestamp format of every ledger, lease and tag value.
pub const STAMP_FORMAT: &str = "%Y-%m-%dT%H:%M:%SZ";

#[must_use]
pub fn stamp(moment: DateTime<Utc>) -> String {
    moment.format(STAMP_FORMAT).to_string()
}

/// The current time with the sub-second part dropped.
#[must_use]
pub fn now_whole_seconds() -> DateTime<Utc> {
    Utc.timestamp_opt(Utc::now().timestamp(), 0)
        .single()
        .unwrap_or_else(Utc::now)
}

/// Parse `YYYY-MM-DDTHH:MM:SSZ` exactly (the `--now` and lease format).
pub fn parse_stamp(text: &str) -> Result<DateTime<Utc>, String> {
    NaiveDateTime::parse_from_str(text, STAMP_FORMAT)
        .map(|naive| Utc.from_utc_datetime(&naive))
        .map_err(|_| format!("time data '{text}' does not match format '{STAMP_FORMAT}'"))
}

/// Parse an ISO-8601 timestamp or epoch seconds into UTC, else `None`: the
/// watchdog's reading of AWS `LaunchTime` and the `rsi:expires-at` tag. A naive
/// timestamp is taken as UTC.
#[must_use]
pub fn parse_time(value: &str) -> Option<DateTime<Utc>> {
    let text = value.trim();
    if (9..=11).contains(&text.len()) && text.bytes().all(|byte| byte.is_ascii_digit()) {
        return text
            .parse::<i64>()
            .ok()
            .and_then(|secs| Utc.timestamp_opt(secs, 0).single());
    }
    let text = text.replace('Z', "+00:00");
    for format in [
        "%Y-%m-%dT%H:%M:%S%.f%:z",
        "%Y-%m-%d %H:%M:%S%.f%:z",
        "%Y-%m-%dT%H:%M:%S%.f%z",
        "%Y-%m-%d %H:%M:%S%.f%z",
        "%Y-%m-%dT%H:%M%:z",
        "%Y-%m-%dT%H:%M%z",
    ] {
        if let Ok(parsed) = DateTime::parse_from_str(&text, format) {
            return Some(parsed.with_timezone(&Utc));
        }
    }
    for format in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M",
    ] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(&text, format) {
            return Some(Utc.from_utc_datetime(&naive));
        }
    }
    NaiveDate::parse_from_str(&text, "%Y-%m-%d")
        .ok()
        .and_then(|date| date.and_hms_opt(0, 0, 0))
        .map(|naive| Utc.from_utc_datetime(&naive))
}

/// `rsi-cloud epoch-stamp N`: print epoch seconds `N` as `YYYY-MM-DDTHH:MM:SSZ`.
pub fn run_epoch_stamp(args: &[String], stdout: &mut dyn Write, stderr: &mut dyn Write) -> i32 {
    let moment = match args {
        [epoch] => epoch
            .trim()
            .parse::<i64>()
            .ok()
            .and_then(|secs| Utc.timestamp_opt(secs, 0).single()),
        _ => None,
    };
    match moment {
        Some(moment) => {
            let _ = writeln!(stdout, "{}", stamp(moment));
            0
        }
        None => {
            let _ = writeln!(stderr, "usage: rsi-cloud epoch-stamp EPOCH_SECONDS");
            2
        }
    }
}

/// The user's home directory (`$HOME`, else `%USERPROFILE%`, else `.`).
#[must_use]
pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map_or_else(|| PathBuf::from("."), PathBuf::from)
}

/// `<home>/.rsi/cloud/<name>`.
#[must_use]
pub fn default_cloud_file(name: &str) -> PathBuf {
    home_dir().join(".rsi").join("cloud").join(name)
}

/// An absolute form of `path` without resolving symlinks (Python `abspath`).
#[must_use]
pub fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// `<path>` with `suffix` appended to the file name (`spend.md` -> `spend.md.lock`).
#[must_use]
pub fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut text = path.as_os_str().to_os_string();
    text.push(suffix);
    PathBuf::from(text)
}

/// An exclusive advisory lock on a file, held until dropped. The lock file is
/// created if missing; the lock blocks until it is granted.
pub struct FileLock {
    _file: File,
}

impl FileLock {
    pub fn acquire(path: &Path) -> std::io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        file.lock()?;
        Ok(Self { _file: file })
    }
}

/// Python's `str.splitlines()`: splits on `\n`, `\r`, `\r\n` and the other
/// Unicode line boundaries, without keeping the terminators.
#[must_use]
pub fn splitlines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((index, ch)) = chars.next() {
        let is_break = matches!(
            ch,
            '\n' | '\r'
                | '\u{0b}'
                | '\u{0c}'
                | '\u{1c}'
                | '\u{1d}'
                | '\u{1e}'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        );
        if !is_break {
            continue;
        }
        lines.push(&text[start..index]);
        let mut end = index + ch.len_utf8();
        if ch == '\r' && matches!(chars.peek(), Some((_, '\n'))) {
            chars.next();
            end += 1;
        }
        start = end;
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    lines
}

/// Case-sensitive shell-style match (`*`, `?`, `[seq]`, `[!seq]`), like
/// Python's `fnmatch.fnmatchcase`.
#[must_use]
pub fn fnmatchcase(name: &str, pattern: &str) -> bool {
    fn matches(name: &[char], pattern: &[char]) -> bool {
        let Some((&first, rest)) = pattern.split_first() else {
            return name.is_empty();
        };
        match first {
            '*' => (0..=name.len()).any(|skip| matches(&name[skip..], rest)),
            '?' => !name.is_empty() && matches(&name[1..], rest),
            '[' => {
                let Some(close) = rest
                    .iter()
                    .enumerate()
                    .skip(1)
                    .find(|(_, ch)| **ch == ']')
                    .map(|(index, _)| index)
                else {
                    return !name.is_empty() && name[0] == '[' && matches(&name[1..], rest);
                };
                let set = &rest[..close];
                let (negate, set) = match set.split_first() {
                    Some(('!', tail)) => (true, tail),
                    _ => (false, set),
                };
                let Some(&ch) = name.first() else {
                    return false;
                };
                let mut hit = false;
                let mut index = 0;
                while index < set.len() {
                    if index + 2 < set.len() && set[index + 1] == '-' {
                        hit |= set[index] <= ch && ch <= set[index + 2];
                        index += 3;
                    } else {
                        hit |= set[index] == ch;
                        index += 1;
                    }
                }
                hit != negate && matches(&name[1..], &rest[close + 1..])
            }
            literal => name.first() == Some(&literal) && matches(&name[1..], rest),
        }
    }
    let name: Vec<char> = name.chars().collect();
    let pattern: Vec<char> = pattern.chars().collect();
    matches(&name, &pattern)
}

/// Print a JSON value the way Python's `print()` shows the value it decoded.
#[must_use]
pub fn py_display(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Number(number) => number.to_string(),
        other => other.to_string(),
    }
}

/// A finite number from a JSON number or numeric string, like `float(value)`.
/// Booleans, non-finite values and other types are refused.
#[must_use]
pub fn json_float(value: &Value) -> Option<f64> {
    let number = match value {
        Value::Number(number) => number.as_f64()?,
        Value::String(text) => text.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    number.is_finite().then_some(number)
}

/// `value` with every object's keys in sorted order (Python `sort_keys=True`).
#[must_use]
pub fn sorted(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            Value::Object(
                keys.into_iter()
                    .map(|key| (key.clone(), sorted(&map[key])))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
        other => other.clone(),
    }
}

/// Two-space indented JSON with sorted keys and no trailing newline.
#[must_use]
pub fn pretty_sorted(value: &Value) -> String {
    serde_json::to_string_pretty(&sorted(value)).unwrap_or_default()
}

/// `{value:.digits}` rounded the way Python's `round(value, digits)` is: the
/// float nearest the correctly rounded decimal.
#[must_use]
pub fn round_to(value: f64, digits: usize) -> f64 {
    format!("{value:.digits$}").parse().unwrap_or(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splitlines_matches_python() {
        assert_eq!(splitlines("a\nb\r\nc\rd"), ["a", "b", "c", "d"]);
        assert_eq!(splitlines("a\n"), ["a"]);
        assert_eq!(splitlines("a\n\nb"), ["a", "", "b"]);
        assert_eq!(splitlines("a\u{0c}b\u{2028}c"), ["a", "b", "c"]);
        assert!(splitlines("").is_empty());
    }

    #[test]
    fn fnmatch_handles_the_service_globs() {
        assert!(fnmatchcase("rsi-remote-gate", "rsi-*"));
        assert!(fnmatchcase("rsi-cloud-rsid", "*-rsid"));
        assert!(!fnmatchcase("other-app", "rsi-*"));
        assert!(!fnmatchcase("RSI-x", "rsi-*"));
        assert!(fnmatchcase("a1", "a[0-9]"));
        assert!(fnmatchcase("ab", "a[!0-9]"));
        assert!(fnmatchcase("ab", "a?"));
    }

    #[test]
    fn parse_time_reads_aws_and_tag_formats() {
        let expected = Utc.with_ymd_and_hms(2026, 10, 21, 10, 0, 0).unwrap();
        for text in [
            "2026-10-21T10:00:00Z",
            "2026-10-21T10:00:00+00:00",
            "2026-10-21T10:00:00.000Z",
            "2026-10-21T10:00:00",
            "2026-10-21 10:00:00",
            "2026-10-21T12:00:00+02:00",
        ] {
            assert_eq!(parse_time(text), Some(expected), "{text}");
        }
        assert_eq!(parse_time("tomorrow-ish"), None);
        assert_eq!(parse_time(""), None);
        assert_eq!(
            parse_time("1792576800"),
            Utc.timestamp_opt(1_792_576_800, 0).single()
        );
    }

    #[test]
    fn round_to_matches_python_round() {
        assert!((round_to(1.562_6, 4) - 1.5626).abs() < f64::EPSILON);
        assert!((round_to(2.0 / 3.0, 1) - 0.7).abs() < f64::EPSILON);
    }
}
