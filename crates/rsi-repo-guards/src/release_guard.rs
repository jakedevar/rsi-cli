//! `release-guard`: the helpers `scripts/release-target-guard.sh` used to run as inline
//! interpreter heredocs (#465, umbrella #358). The shell keeps every decision about the
//! build; these are the parts shell cannot do portably: a held `flock`, `realpath`
//! comparisons, hashing and the atomic keep/prune of copied binaries, and parsing a
//! build receipt's JSON.
//!
//! ```text
//! release-guard lock-fd <fd>
//! release-guard links-into <release-dir> <link-dir>...
//! release-guard kept <preserve|prune> <release-dir> <keep-dir> <link-dir>...
//! release-guard receipt-status <receipt>
//! release-guard receipt-members <receipt> <bin>...
//! ```
//!
//! * `lock-fd`: take an exclusive `flock` on the inherited descriptor (the shell's fd
//!   8). The lock belongs to the open file, so it stays held by the shell after this
//!   process exits. Waits, saying so on stderr, when another build holds it.
//! * `links-into`: print every symlink in the link directories whose target lies in
//!   the release directory (read-only).
//! * `kept`: under an exclusive lock on `<keep>/.lock`, `preserve` repoints every such
//!   link at a byte-identical copy named `<binary>.<sha256 prefix>` in `<keep>`, then
//!   both modes prune the copies no link resolves to and rewrite `<keep>/.links`.
//! (The receipt target check is the `release-target` guard, hub #3715; not repeated here.)
//! * `receipt-status`: print `status|source_head|detail` for a build receipt, the
//!   status being `ok`, `dirty`, `nohead`, `failed`, `refused` or `unreadable`.
//! * `receipt-members`: require aggregate coverage and executable Cargo members;
//!   older per-package receipts retain their existing checks.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::Ctx;
use crate::py;

/// `release-guard <operation> ...`.
pub fn main(args: &[String], _ctx: &Ctx, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let rest: Vec<&str> = args.iter().skip(1).map(String::as_str).collect();
    match (args.first().map(String::as_str), rest.as_slice()) {
        (Some("lock-fd"), [fd]) => lock_fd(fd, err),
        (Some("links-into"), [release, dirs @ ..]) => links_into(release, dirs, out),
        (Some("kept"), [mode, release, keep, dirs @ ..]) => kept(mode, release, keep, dirs, err),
        (Some("receipt-status"), [receipt]) => receipt_status(receipt, out),
        (Some("receipt-members"), [receipt, bins @ ..]) => receipt_members(receipt, bins, err),
        _ => {
            let _ = writeln!(
                err,
                "usage: rsi-repo-guard release-guard lock-fd <fd> | links-into <release> <link-dir>... | \
                 kept <preserve|prune> <release> <keep> <link-dir>... | \
                 receipt-status <receipt> | receipt-members <receipt> <bin>..."
            );
            64
        }
    }
}

// ---- lock-fd ----

fn flock(fd: i32, operation: i32) -> std::io::Result<()> {
    loop {
        // SAFETY: flock takes a descriptor number and flags; a bad descriptor is an error return.
        if unsafe { libc::flock(fd, operation) } == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn lock_fd(fd: &str, err: &mut dyn Write) -> i32 {
    let Ok(fd) = fd.parse::<i32>() else {
        let _ = writeln!(err, "lock-fd: {fd:?} is not a descriptor number");
        return 64;
    };
    match flock(fd, libc::LOCK_EX | libc::LOCK_NB) {
        Ok(()) => 0,
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            let _ = writeln!(
                err,
                "Waiting for another release build into this target to finish (#938) ..."
            );
            match flock(fd, libc::LOCK_EX) {
                Ok(()) => 0,
                Err(error) => {
                    let _ = writeln!(err, "cannot lock descriptor {fd}: {error}");
                    1
                }
            }
        }
        Err(error) => {
            let _ = writeln!(err, "cannot lock descriptor {fd}: {error}");
            1
        }
    }
}

// ---- path helpers ----

/// Python's `os.path.realpath`: absolute, every symlink resolved, tolerant of a
/// missing tail (and of a dangling link, which resolves to its target).
fn realpath(path: &Path) -> PathBuf {
    let mut pending: Vec<std::ffi::OsString> = {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir().unwrap_or_default().join(path)
        };
        absolute
            .components()
            .rev()
            .map(|c| c.as_os_str().to_os_string())
            .collect()
    };
    let mut resolved = PathBuf::from("/");
    let mut links = 0;
    while let Some(part) = pending.pop() {
        if part == "/" || part == "." || part.is_empty() {
            continue;
        }
        if part == ".." {
            resolved.pop();
            continue;
        }
        let candidate = resolved.join(&part);
        match fs::symlink_metadata(&candidate) {
            Ok(meta) if meta.file_type().is_symlink() => {
                links += 1;
                if links > 40 {
                    // A loop: Python returns the path unresolved from here on.
                    resolved.push(&part);
                    continue;
                }
                let Ok(target) = fs::read_link(&candidate) else {
                    resolved.push(&part);
                    continue;
                };
                if target.is_absolute() {
                    resolved = PathBuf::from("/");
                }
                for component in target.components().rev() {
                    pending.push(component.as_os_str().to_os_string());
                }
            }
            _ => resolved.push(&part),
        }
    }
    resolved
}

fn starts_within(path: &Path, directory: &Path) -> bool {
    let mut prefix = directory.as_os_str().to_os_string();
    if !prefix.to_string_lossy().ends_with('/') {
        prefix.push("/");
    }
    path.as_os_str()
        .to_string_lossy()
        .starts_with(prefix.to_string_lossy().as_ref())
}

fn dedupe<'a>(items: &[&'a str]) -> Vec<&'a str> {
    let mut seen = BTreeSet::new();
    items
        .iter()
        .copied()
        .filter(|item| seen.insert(*item))
        .collect()
}

/// The symlinks directly inside `directory` (dangling ones included), unsorted.
fn symlinks_in(directory: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_symlink()))
        .map(|entry| entry.path())
        .collect()
}

fn is_file(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file())
}

// ---- links-into ----

fn links_into(release: &str, dirs: &[&str], out: &mut dyn Write) -> i32 {
    let release = realpath(Path::new(release));
    for directory in dedupe(dirs) {
        let mut links = symlinks_in(Path::new(directory));
        links.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
        for link in links {
            if starts_within(&realpath(&link), &release) {
                let _ = writeln!(out, "{}", link.display());
            }
        }
    }
    0
}

// ---- kept ----

fn sha256_hex(path: &Path) -> std::io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        digest.update(&buffer[..n]);
    }
    Ok(digest
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

fn temp_name(directory: &Path, prefix: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    directory.join(format!("{prefix}{}-{nanos}", std::process::id()))
}

fn kept(mode: &str, release: &str, keep: &str, dirs: &[&str], err: &mut dyn Write) -> i32 {
    if mode != "preserve" && mode != "prune" {
        let _ = writeln!(err, "kept: mode must be preserve or prune, got {mode:?}");
        return 64;
    }
    match kept_inner(mode, release, keep, dirs, err) {
        Ok(()) => 0,
        Err(error) => {
            let _ = writeln!(err, "release-guard kept: {error}");
            1
        }
    }
}

fn kept_inner(
    mode: &str,
    release: &str,
    keep: &str,
    dirs: &[&str],
    err: &mut dyn Write,
) -> std::io::Result<()> {
    let link_dirs = dedupe(dirs);
    let keep_dir = Path::new(keep);
    fs::create_dir_all(keep_dir)?;
    let keep_real = realpath(keep_dir);
    let manifest = keep_dir.join(".links");

    let lock = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(keep_dir.join(".lock"))?;
    {
        use std::os::fd::AsRawFd;
        flock(lock.as_raw_fd(), libc::LOCK_EX)?;
    }

    let mut recorded: BTreeSet<PathBuf> = match fs::read_to_string(&manifest) {
        Ok(text) => text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(PathBuf::from)
            .collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeSet::new(),
        Err(error) => return Err(error),
    };

    if mode == "preserve" {
        let release = realpath(Path::new(release));
        for directory in &link_dirs {
            for path in symlinks_in(Path::new(directory)) {
                let real = realpath(&path);
                if !starts_within(&real, &release) || !is_file(&real) {
                    continue;
                }
                let base = real
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let digest = sha256_hex(&real)?;
                let kept_path = keep_dir.join(format!("{base}.{}", &digest[..16]));
                if !is_file(&kept_path) {
                    let temp = temp_name(keep_dir, ".keep-");
                    fs::copy(&real, &temp)?;
                    fs::set_permissions(&temp, fs::Permissions::from_mode(0o755))?;
                    fs::rename(&temp, &kept_path)?;
                }
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let link_temp =
                    Path::new(directory).join(format!(".{name}.keep-{}", std::process::id()));
                match fs::remove_file(&link_temp) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
                symlink(&kept_path, &link_temp)?;
                fs::rename(&link_temp, &path)?;
                let _ = writeln!(
                    err,
                    "Kept installed link {} -> {} before cleaning {} (#938).",
                    path.display(),
                    kept_path.display(),
                    release.display()
                );
                recorded.insert(path);
            }
        }
    }

    // Prune (#2188): keep exactly the copies a live link resolves to.
    let mut candidates = recorded;
    for directory in &link_dirs {
        candidates.extend(symlinks_in(Path::new(directory)));
    }
    let mut live: BTreeMap<PathBuf, PathBuf> = BTreeMap::new();
    for path in candidates {
        if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
            let real = realpath(&path);
            if real.parent() == Some(keep_real.as_path()) && is_file(&real) {
                live.insert(path, real);
            }
        }
    }
    let referenced: BTreeSet<&PathBuf> = live.values().collect();
    for entry in fs::read_dir(keep_dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".lock" || name == ".links" || !entry.file_type()?.is_file() {
            continue;
        }
        // `.keep-*` are copies a run died writing; every writer holds the lock.
        if name.starts_with(".keep-") || !referenced.contains(&realpath(&entry.path())) {
            if let Err(error) = fs::remove_file(entry.path()) {
                // A failed prune never blocks a cleanup or install.
                let _ = writeln!(
                    err,
                    "warning: cannot prune kept copy {}: {error}",
                    entry.path().display()
                );
                continue;
            }
            if !name.starts_with('.') {
                let _ = writeln!(
                    err,
                    "Pruned kept copy {}: no installed link resolves to it (#2188).",
                    entry.path().display()
                );
            }
        }
    }
    let temp = temp_name(keep_dir, ".links-");
    let mut text = String::new();
    for path in live.keys() {
        text.push_str(&path.to_string_lossy());
        text.push('\n');
    }
    fs::write(&temp, text)?;
    fs::rename(&temp, &manifest)?;
    drop(lock);
    Ok(())
}

// ---- receipts ----

/// Python `repr()` of a JSON value (what the old messages interpolated with `%r`).
fn py_repr(value: &Value) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Number(number) => number.to_string(),
        Value::String(text) => py::repr(text),
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(py_repr).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(map) => format!(
            "{{{}}}",
            map.iter()
                .map(|(key, value)| format!("{}: {}", py::repr(key), py_repr(value)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Python `str()` of a JSON value.
fn py_str(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => py_repr(other),
    }
}

fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_none_or(|n| n != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

fn load_receipt(path: &str) -> Result<Value, String> {
    let text = fs::read(path).map_err(|e| e.to_string())?;
    serde_json::from_slice(&text).map_err(|e| e.to_string())
}

/// The receipt's own fields: the document itself, or its `result` object when the
/// document has no `key` (a build job result wraps the receipt, #282).
fn receipt_fields<'a>(data: &'a Value, key: &str) -> Option<&'a serde_json::Map<String, Value>> {
    let top = data.as_object();
    match top {
        Some(map) if map.contains_key(key) => Some(map),
        Some(map) => match map.get("result").and_then(Value::as_object) {
            Some(inner) => Some(inner),
            None => Some(map),
        },
        None => None,
    }
}

fn receipt_status(receipt: &str, out: &mut dyn Write) -> i32 {
    let line = match load_receipt(receipt) {
        Err(error) => format!("unreadable||{}", error.replace(['\t', '\n'], " ")),
        Ok(data) => {
            let empty = serde_json::Map::new();
            let result = receipt_fields(&data, "source_head").unwrap_or(&empty);
            let head = result.get("source_head");
            if let Some(refusal) = result.get("refusal").filter(|v| is_truthy(v)) {
                format!("refused||{}", py_str(refusal))
            } else if result.get("exit_code").and_then(Value::as_f64) != Some(0.0) {
                let code = result
                    .get("exit_code")
                    .map_or_else(|| "None".to_string(), py_str);
                format!("failed||exit_code {code}")
            } else if let Some(head) = head.and_then(Value::as_str).filter(|h| !h.is_empty()) {
                if result.get("source_dirty") == Some(&Value::Bool(false)) {
                    format!("ok|{head}|")
                } else {
                    format!("dirty|{head}|")
                }
            } else {
                "nohead||".to_string()
            }
        }
    };
    let _ = writeln!(out, "{line}");
    0
}

/// Legacy per-package receipts keep their existing provenance checks. An
/// aggregate receipt must cover the complete requested staging inventory;
/// leftovers in a warm target cannot fill a gap in its build proof.
fn receipt_members(receipt: &str, bins: &[&str], err: &mut dyn Write) -> i32 {
    let checked = (|| -> Result<(), String> {
        let data = load_receipt(receipt)?;
        let fields = receipt_fields(&data, "source_head").ok_or("invalid build receipt")?;
        let Some(members) = fields.get("release_members") else {
            return Ok(());
        };
        let members = members
            .as_array()
            .ok_or("release_members must be an array")?;
        if members.is_empty() {
            return Err("release_members must be nonempty".into());
        }
        let mut built = BTreeSet::new();
        for member in members {
            member
                .get("package")
                .and_then(Value::as_str)
                .filter(|package| !package.is_empty())
                .ok_or("release member needs a package")?;
            let bins = member
                .get("bins")
                .and_then(Value::as_array)
                .filter(|bins| !bins.is_empty())
                .ok_or("release member needs nonempty bins")?;
            for bin in bins {
                built.insert(
                    bin.as_str()
                        .filter(|bin| !bin.is_empty())
                        .ok_or("release bin must be a nonempty string")?,
                );
            }
        }
        let missing: Vec<&str> = bins
            .iter()
            .copied()
            .filter(|bin| !built.contains(bin))
            .collect();
        if !missing.is_empty() {
            return Err(format!(
                "release receipt does not cover {}; build every deploy member at the same clean SHA and target",
                missing.join(", ")
            ));
        }
        let target = fields
            .get("target_dir")
            .and_then(Value::as_str)
            .ok_or("aggregate receipt needs target_dir")?;
        for bin in bins {
            let path = Path::new(target).join("release").join(bin);
            let metadata = fs::metadata(&path)
                .map_err(|error| format!("release member {}: {error}", path.display()))?;
            if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
                return Err(format!(
                    "release member {} is not executable",
                    path.display()
                ));
            }
        }
        Ok(())
    })();
    match checked {
        Ok(()) => 0,
        Err(reason) => {
            let _ = writeln!(err, "Cannot stage build receipt {receipt}: {reason}");
            1
        }
    }
}

#[cfg(test)]
#[path = "release_guard_tests.rs"]
mod tests;
