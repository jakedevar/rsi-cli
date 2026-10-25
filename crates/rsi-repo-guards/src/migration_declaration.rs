//! Author-side declaration digest refresh, preserving every other input byte (#361).
use crate::{Ctx, released_migrations as guard};
use guard::{Result, TOKEN};
use serde_json::Value;
use std::{
    collections::BTreeSet,
    io::{Read, Write},
    ops::Range,
    path::Path,
};

pub const MAX_DECLARATION_BYTES: usize = 512 * 1024;
pub const MAX_CHANGED_PATHS: usize = 2048;
pub const MAX_DECLARED_FILES: usize = 128;
pub const MAX_SITES: usize = 4096;
pub const MAX_SOURCE_BLOB_BYTES: usize = 8 * 1024 * 1024;
pub const DECLARATIONS: &str = "tools/provisional-migrations/";

pub fn path(value: &str) -> Result<&str> {
    if value.is_empty()
        || value.starts_with('/')
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_./-".contains(&b))
        || value.split('/').any(|p| matches!(p, "" | "." | ".."))
    {
        return Err("unsafe declaration path".into());
    }
    Ok(value)
}
pub fn version_spans(data: &[u8], version: i64) -> Vec<Range<usize>> {
    let digits = version.to_string();
    let bytes = digits.as_bytes();
    data.windows(bytes.len())
        .enumerate()
        .filter(|(i, w)| {
            *w == bytes
                && (*i == 0 || !data[*i - 1].is_ascii_digit())
                && (*i + bytes.len() == data.len() || !data[*i + bytes.len()].is_ascii_digit())
        })
        .map(|(i, _)| i..i + bytes.len())
        .collect()
}
pub fn occurrences(data: &[u8], needle: &[u8]) -> Vec<usize> {
    if needle.is_empty() {
        return Vec::new();
    }
    // str/bytes.count counts non-overlapping matches.
    let mut matches = Vec::new();
    let mut offset = 0;
    while offset + needle.len() <= data.len() {
        if let Some(i) = data[offset..]
            .windows(needle.len())
            .position(|w| w == needle)
        {
            matches.push(offset + i);
            offset += i + needle.len();
        } else {
            break;
        }
    }
    matches
}
pub fn read_file(repo: &Path, name: &str, limit: usize) -> Result<Vec<u8>> {
    let mut current = repo.to_path_buf();
    for part in path(name)?.split('/') {
        current.push(part);
        if std::fs::symlink_metadata(&current).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(format!("symlink in declaration path: {name}"));
        }
    }
    if !current.is_file() {
        return Err(format!("declared file is missing or not regular: {name}"));
    }
    let mut data = Vec::new();
    std::fs::File::open(current)
        .map_err(|e| e.to_string())?
        .take(limit as u64 + 1)
        .read_to_end(&mut data)
        .map_err(|e| e.to_string())?;
    if data.len() > limit || data.contains(&0) {
        return Err(format!("declared file is too large or binary: {name}"));
    }
    std::str::from_utf8(&data).map_err(|e| e.to_string())?;
    Ok(data)
}

/// Locate complete JSON string tokens and reject duplicate decoded object keys.
/// Parsing first ensures that escape sequences, nesting and colons are valid.
fn string_tokens(raw: &[u8]) -> Result<Vec<Range<usize>>> {
    let mut tokens = Vec::new();
    let mut stack: Vec<Option<BTreeSet<String>>> = Vec::new();
    let mut i = 0;
    while i < raw.len() {
        match raw[i] {
            b'{' => stack.push(Some(BTreeSet::new())),
            b'[' => stack.push(None),
            b'}' | b']' => {
                stack.pop();
            }
            b'"' => {
                let start = i;
                i += 1;
                while i < raw.len() && raw[i] != b'"' {
                    if raw[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                if i >= raw.len() {
                    return Err("unterminated declaration JSON string".into());
                }
                let end = i + 1;
                let mut next = end;
                while next < raw.len() && raw[next].is_ascii_whitespace() {
                    next += 1;
                }
                if raw.get(next) == Some(&b':') {
                    let key: String =
                        serde_json::from_slice(&raw[start..end]).map_err(|e| e.to_string())?;
                    if let Some(Some(keys)) = stack.last_mut() {
                        if !keys.insert(key.clone()) {
                            return Err(format!("duplicate declaration key: {key}"));
                        }
                    }
                }
                tokens.push(start..end);
            }
            _ => {}
        }
        i += 1;
    }
    Ok(tokens)
}
fn exact_keys(value: &Value, keys: &[&str], optional: &[&str]) -> bool {
    value.as_object().is_some_and(|m| {
        keys.iter().all(|k| m.contains_key(*k))
            && m.keys()
                .all(|k| keys.contains(&k.as_str()) || optional.contains(&k.as_str()))
    })
}

pub fn refreshed_bytes(repo: &Path, raw: &[u8]) -> Result<(Vec<u8>, Vec<String>)> {
    if raw.len() > MAX_DECLARATION_BYTES {
        return Err("provisional declaration is too large".into());
    }
    let declaration: Value = serde_json::from_slice(raw).map_err(|e| e.to_string())?;
    let tokens = string_tokens(raw)?;
    if !exact_keys(
        &declaration,
        &["schema_version", "version", "files"],
        &["pending_predecessors"],
    ) || declaration["schema_version"].as_i64() != Some(1)
        || declaration["version"]
            .as_i64()
            .is_none_or(|n| n <= 0 || n >= i32::MAX as i64)
        || declaration["files"]
            .as_array()
            .is_none_or(|f| f.is_empty() || f.len() > MAX_DECLARED_FILES)
    {
        return Err("invalid provisional declaration shape or version".into());
    }
    let raw_text = std::str::from_utf8(raw).map_err(|e| e.to_string())?;
    guard::parse_pending_predecessors(&[raw_text.into()], 0)?;
    let version = declaration["version"].as_i64().unwrap_or_default();
    let mut seen = BTreeSet::new();
    let mut digests = Vec::new();
    let mut changed = Vec::new();
    for file in declaration["files"].as_array().into_iter().flatten() {
        if !exact_keys(
            file,
            &["path", "path_template", "source_blob", "sites"],
            &[],
        ) {
            return Err("invalid provisional file declaration".into());
        }
        let name = path(file["path"].as_str().ok_or("unsafe declaration path")?)?;
        let template = file["path_template"]
            .as_str()
            .ok_or("invalid provisional path template")?;
        let rendered = template.replace(TOKEN, "VERSION");
        path(&rendered)?;
        if rendered.matches("VERSION").count() != template.matches(TOKEN).count()
            || template.replace(TOKEN, &version.to_string()) != name
            || (!version_spans(name.as_bytes(), version).is_empty() && !template.contains(TOKEN))
        {
            return Err(format!(
                "provisional path template does not match source: {name}"
            ));
        }
        if !seen.insert(name) {
            return Err(format!("duplicate declared file: {name}"));
        }
        let old = file["source_blob"]
            .as_str()
            .filter(|s| {
                s.len() == 71
                    && s.starts_with("sha256:")
                    && s[7..]
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            })
            .ok_or_else(|| format!("invalid source blob digest: {name}"))?;
        let original = read_file(repo, name, MAX_SOURCE_BLOB_BYTES)?;
        let sites = file["sites"]
            .as_array()
            .filter(|s| s.len() <= MAX_SITES)
            .ok_or("invalid provisional site list")?;
        let mut ranges: Vec<Range<usize>> = Vec::new();
        for site in sites {
            if !exact_keys(site, &["anchor", "replacement", "scope"], &[])
                || !matches!(site["scope"].as_str(), Some("unit" | "head"))
            {
                return Err("invalid provisional site".into());
            }
            let (Some(anchor), Some(replacement)) = (
                site["anchor"].as_str().filter(|s| !s.is_empty()),
                site["replacement"].as_str(),
            ) else {
                return Err("site replacement must be a pure version substitution".into());
            };
            if !replacement.contains(TOKEN)
                || replacement.replace(TOKEN, &version.to_string()) != anchor
            {
                return Err("site replacement must be a pure version substitution".into());
            }
            let locations = occurrences(&original, anchor.as_bytes());
            if locations.len() != 1 || version_spans(anchor.as_bytes(), version).is_empty() {
                return Err(format!(
                    "site anchor is missing, duplicate, or unversioned: {name}"
                ));
            }
            let start = locations[0];
            let end = start + anchor.len();
            if ranges.iter().any(|r| r.start < end && start < r.end) {
                return Err(format!("overlapping provisional site anchors: {name}"));
            }
            ranges.push(start..end);
        }
        let digest = guard::digest(&original);
        if digest != old {
            changed.push(name.to_string());
        }
        digests.push(digest);
    }
    let mut spans = Vec::new();
    for (i, token) in tokens.iter().enumerate() {
        let value: String =
            serde_json::from_slice(&raw[token.clone()]).map_err(|e| e.to_string())?;
        if value == "source_blob"
            && raw[token.end..].iter().find(|b| !b.is_ascii_whitespace()) == Some(&b':')
        {
            spans.push(
                tokens
                    .get(i + 1)
                    .ok_or("cannot locate exact source_blob value spans")?
                    .clone(),
            );
        }
    }
    if spans.len() != digests.len() {
        return Err("cannot locate exact source_blob value spans".into());
    }
    let mut result = raw.to_vec();
    for (span, digest) in spans.into_iter().zip(digests).rev() {
        let existing: String =
            serde_json::from_slice(&raw[span.clone()]).map_err(|e| e.to_string())?;
        if existing != digest {
            result.splice(
                span,
                serde_json::to_vec(&digest).map_err(|e| e.to_string())?,
            );
        }
    }
    Ok((result, changed))
}

pub fn refresh(repo: &Path, name: &str, check: bool) -> Result<Vec<String>> {
    let repo = repo.canonicalize().map_err(|e| e.to_string())?;
    path(name)?;
    if !name.starts_with(DECLARATIONS) || !name.ends_with(".json") {
        return Err("expected tools/provisional-migrations/*.json".into());
    }
    let raw = read_file(&repo, name, MAX_DECLARATION_BYTES)?;
    let (updated, changed) = refreshed_bytes(&repo, &raw)?;
    if check || changed.is_empty() {
        return Ok(changed);
    }
    let destination = repo.join(name);
    let parent = destination.parent().ok_or("declaration has no parent")?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
    temporary.write_all(&updated).map_err(|e| e.to_string())?;
    temporary
        .as_file()
        .set_permissions(
            std::fs::metadata(&destination)
                .map_err(|e| e.to_string())?
                .permissions(),
        )
        .map_err(|e| e.to_string())?;
    if std::fs::read(&destination).map_err(|e| e.to_string())? != raw {
        return Err("declaration changed during refresh; retry".into());
    }
    temporary.persist(destination).map_err(|e| e.to_string())?;
    Ok(changed)
}

pub fn main(args: &[String], ctx: &Ctx, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let mut repo = ctx.cwd.clone();
    let mut check = false;
    let mut name = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--repo" => {
                let Some(p) = it.next() else {
                    let _ = writeln!(err, "--repo requires a path");
                    return 2;
                };
                repo = ctx.path(p);
            }
            "--check" => check = true,
            "-h" | "--help" => {
                let _ = writeln!(
                    out,
                    "usage: refresh-migration-declaration-digests [--repo PATH] [--check] declaration"
                );
                return 0;
            }
            s if s.starts_with('-') || name.is_some() => {
                let _ = writeln!(err, "unrecognized argument: {s}");
                return 2;
            }
            _ => name = Some(arg.as_str()),
        }
    }
    let Some(name) = name else {
        let _ = writeln!(err, "declaration is required");
        return 2;
    };
    match refresh(&repo, name, check) {
        Ok(changed) => {
            let _ = writeln!(
                out,
                "{} source_blob values: {}",
                if check { "stale" } else { "refreshed" },
                changed.len()
            );
            for path in &changed {
                let _ = writeln!(out, "{path}");
            }
            i32::from(check && !changed.is_empty())
        }
        Err(error) => {
            let _ = writeln!(err, "migration declaration refresh refused: {error}");
            2
        }
    }
}

#[cfg(test)]
mod tests;
