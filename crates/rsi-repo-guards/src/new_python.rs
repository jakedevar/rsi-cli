//! Refuse newly tracked Python paths (#407, operator directive #358).
//!
//! Existing modifications/deletions remain available to the conversion slices.
//! Only non-executable, shebang-free evidence under thoughts/ is exempt. Disabling
//! rename detection makes every new destination subject to the same policy.
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::process::Stdio;

use crate::Ctx;
use crate::cli::{self, Opt};

const GUIDANCE: &str = "operator directive #358 (2026-10-22): Python is forbidden; use Rust for new guards via scripts/repo-guard; see crates/rsi-repo-guards/README.md";

/// Run `new-python [BASE [HEAD]]` or `new-python --staged`.
pub fn main(args: &[String], ctx: &Ctx, _out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let parsed = match cli::parse(
        "new-python",
        "[BASE [HEAD]] | --staged",
        args,
        &[Opt {
            name: "--staged",
            value: false,
        }],
        &[],
        Some(2),
        err,
    ) {
        Ok(parsed) => parsed,
        Err(exit) => return exit.code(),
    };
    if parsed.flag("--staged") && !parsed.positionals.is_empty() {
        let _ = writeln!(
            err,
            "new-python: --staged cannot be combined with revisions"
        );
        return 2;
    }
    // Pin the candidate tree once. In staged mode this also rejects unresolved
    // entries; neither contents nor symlink targets come from the working tree.
    let staged = parsed.flag("--staged");
    let snapshot = if staged {
        git(ctx, &["write-tree"])
    } else {
        let head = parsed.positionals.get(1).map_or("HEAD", String::as_str);
        git(
            ctx,
            &[
                "rev-parse",
                "--verify",
                "--end-of-options",
                &format!("{head}^{{tree}}"),
            ],
        )
    };
    let snapshot = match snapshot.and_then(|bytes| {
        let oid = String::from_utf8(bytes).map_err(|_| "invalid tree id".to_string())?;
        let oid = oid.trim();
        if valid_oid(oid.as_bytes()) {
            Ok(oid.to_string())
        } else {
            Err("invalid tree id".into())
        }
    }) {
        Ok(tree) => tree,
        Err(detail) => return inspection_error(err, &detail),
    };
    let mut command = ctx.git();
    command.arg("--no-replace-objects");
    command.args([
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--no-renames",
        "--raw",
        "--no-abbrev",
        "-z",
        "--diff-filter=A",
    ]);
    if staged {
        // An unborn index is compared with an empty tree, in either Git hash format.
        let base = if git(ctx, &["rev-parse", "--verify", "HEAD"]).is_ok() {
            "HEAD".to_string()
        } else {
            match git(ctx, &["mktree"]) {
                Ok(bytes) => String::from_utf8_lossy(&bytes).trim().to_string(),
                Err(detail) => return inspection_error(err, &detail),
            }
        };
        command.args([&base, &snapshot]);
    } else {
        let base = parsed
            .positionals
            .first()
            .map_or("origin/rolling", String::as_str);
        command.args([base, &snapshot]);
    }
    command.arg("--");
    let output = match command.output() {
        Ok(output) if output.status.success() => output,
        result => {
            let detail = match result {
                Ok(output) => String::from_utf8_lossy(&output.stderr).trim().to_string(),
                Err(error) => error.to_string(),
            };
            let _ = writeln!(
                err,
                "new-python: cannot inspect tracked additions: {detail}; {GUIDANCE}"
            );
            return 2;
        }
    };
    let mut fields = output.stdout.split(|b| *b == 0);
    let mut entries = None;
    let mut refused = false;
    while let Some(header) = fields.next().filter(|header| !header.is_empty()) {
        let Some(path) = fields.next() else {
            let _ = writeln!(err, "new-python: malformed Git diff; {GUIDANCE}");
            return 2;
        };
        let metadata: Vec<_> = header.split(|b| *b == b' ').collect();
        if metadata.len() != 5
            || metadata[4] != b"A"
            || path.is_empty()
            || metadata[0] != b":000000"
            || !valid_oid(metadata[3])
        {
            let _ = writeln!(err, "new-python: malformed Git addition; {GUIDANCE}");
            return 2;
        }
        let is_python = classify(ctx, &snapshot, path, metadata[1], metadata[3], &mut entries);
        let is_python = match is_python {
            Ok(value) => value,
            Err(detail) => {
                return inspection_error(
                    err,
                    &format!("path {:?}: {detail}", String::from_utf8_lossy(path)),
                );
            }
        };
        if is_python {
            // Debug quoting keeps whitespace/newlines legible. Matching above
            // uses raw bytes, including paths that are not valid UTF-8.
            let _ = writeln!(
                err,
                "new-python: refuses added tracked path {:?}; {GUIDANCE}",
                String::from_utf8_lossy(path)
            );
            refused = true;
        }
    }
    i32::from(refused)
}

fn inspection_error(err: &mut dyn Write, detail: &str) -> i32 {
    let _ = writeln!(
        err,
        "new-python: cannot inspect tracked additions: {detail}; {GUIDANCE}"
    );
    2
}

fn git(ctx: &Ctx, args: &[&str]) -> Result<Vec<u8>, String> {
    let output = ctx
        .git()
        .arg("--no-replace-objects")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(format!(
            "git {}: {}",
            args[0],
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn valid_oid(oid: &[u8]) -> bool {
    matches!(oid.len(), 40 | 64)
        && oid.iter().all(u8::is_ascii_hexdigit)
        && oid.iter().any(|byte| *byte != b'0')
}

fn python_suffix(path: &[u8]) -> bool {
    [b".py".as_slice(), b".pyw", b".pyi"].iter().any(|suffix| {
        path.len() >= suffix.len() && path[path.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
    })
}

// Read a bounded prefix directly from the object database. A batch header proves
// this is an available blob before we read; large binaries need not be buffered.
fn blob_prefix(ctx: &Ctx, oid: &[u8]) -> Result<Vec<u8>, String> {
    let oid = std::str::from_utf8(oid).map_err(|_| "invalid blob id")?;
    let mut child = ctx
        .git()
        .args(["--no-replace-objects", "cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    let result = (|| {
        use std::io::BufRead;
        writeln!(child.stdin.take().ok_or("missing Git stdin")?, "{oid}")
            .map_err(|e| e.to_string())?;
        let mut reader = std::io::BufReader::new(child.stdout.take().ok_or("missing Git stdout")?);
        let mut header = String::new();
        reader.read_line(&mut header).map_err(|e| e.to_string())?;
        let fields: Vec<_> = header.split_ascii_whitespace().collect();
        if fields.len() != 3 || fields[0] != oid || fields[1] != "blob" {
            return Err("unavailable or non-blob object".into());
        }
        let size: u64 = fields[2].parse().map_err(|_| "invalid blob size")?;
        let mut prefix = vec![0; size.min(4096) as usize];
        reader.read_exact(&mut prefix).map_err(|e| e.to_string())?;
        if prefix.starts_with(b"#!") && size > 4096 && !prefix.contains(&b'\n') {
            return Err("shebang exceeds inspection limit".into());
        }
        Ok(prefix)
    })();
    let _ = child.kill();
    let _ = child.wait();
    result
}

type Entries = BTreeMap<Vec<u8>, (Vec<u8>, Vec<u8>)>;

fn tree_entries(ctx: &Ctx, snapshot: &str) -> Result<Entries, String> {
    let output = git(ctx, &["ls-tree", "-r", "-z", snapshot])?;
    let mut entries = BTreeMap::new();
    for record in output.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let tab = record
            .iter()
            .position(|b| *b == b'\t')
            .ok_or("malformed tree entry")?;
        let fields: Vec<_> = record[..tab].split(|b| *b == b' ').collect();
        if fields.len() != 3 || !valid_oid(fields[2]) {
            return Err("malformed tree metadata".into());
        }
        entries.insert(
            record[tab + 1..].to_vec(),
            (fields[0].to_vec(), fields[2].to_vec()),
        );
    }
    Ok(entries)
}

fn classify(
    ctx: &Ctx,
    snapshot: &str,
    path: &[u8],
    mode: &[u8],
    oid: &[u8],
    entries: &mut Option<Entries>,
) -> Result<bool, String> {
    let archive = path.starts_with(b"thoughts/") && mode == b"100644";
    if python_suffix(path) && !archive {
        return Ok(true);
    }
    match mode {
        b"100644" | b"100755" => Ok(python_shebang(&blob_prefix(ctx, oid)?)),
        b"120000" => {
            if entries.is_none() {
                *entries = Some(tree_entries(ctx, snapshot)?);
            }
            let entries = entries.as_ref().ok_or("missing candidate tree")?;
            let mut current = path.to_vec();
            let mut seen = BTreeSet::new();
            // Resolve every component in Git, never through host filesystem links.
            // External/dangling/cyclic links cannot prove a non-Python destination.
            for _ in 0..40 {
                let components: Vec<_> = current.split(|b| *b == b'/').collect();
                let mut prefix = Vec::new();
                let mut followed = false;
                for (index, component) in components.iter().enumerate() {
                    if !prefix.is_empty() {
                        prefix.push(b'/');
                    }
                    prefix.extend_from_slice(component);
                    if python_suffix(&prefix) {
                        return Ok(true);
                    }
                    if let Some((mode, oid)) = entries.get(&prefix) {
                        if mode == b"120000" {
                            if !seen.insert(prefix.clone()) {
                                return Err("cyclic symlink".into());
                            }
                            let target = blob_prefix(ctx, oid)?;
                            if target.len() == 4096 || target.contains(&0) {
                                return Err("invalid symlink target".into());
                            }
                            let parent =
                                prefix.rsplitn(2, |b| *b == b'/').nth(1).unwrap_or_default();
                            current = link_path(parent, &target, &components[index + 1..])?;
                            followed = true;
                            break;
                        }
                    }
                }
                if followed {
                    continue;
                }
                let Some((mode, oid)) = entries.get(&current) else {
                    // A directory alias is not a source blob; links through it
                    // still resolve every component above before classification.
                    let mut directory = current.clone();
                    directory.push(b'/');
                    if entries.keys().any(|path| path.starts_with(&directory)) {
                        return Ok(false);
                    }
                    return Err("unavailable symlink destination in candidate tree".into());
                };
                return match mode.as_slice() {
                    b"100644" | b"100755" => Ok(python_shebang(&blob_prefix(ctx, oid)?)),
                    _ => Err("non-blob symlink destination".into()),
                };
            }
            Err("symlink resolution limit exceeded".into())
        }
        b"160000" => Ok(false), // Submodules do not add a source blob here.
        _ => Err("unsupported Git entry mode".into()),
    }
}

fn link_path(parent: &[u8], target: &[u8], tail: &[&[u8]]) -> Result<Vec<u8>, String> {
    if target.is_empty()
        || target.starts_with(b"/")
        || target.contains(&b'\\')
        || target.contains(&b':')
    {
        return Err("external or non-portable symlink target".into());
    }
    let mut parts: Vec<&[u8]> = parent
        .split(|b| *b == b'/')
        .filter(|p| !p.is_empty())
        .collect();
    for part in target.split(|b| *b == b'/').chain(tail.iter().copied()) {
        match part {
            b"" | b"." => {}
            b".." => {
                parts.pop().ok_or("symlink escapes candidate tree")?;
            }
            _ => parts.push(part),
        }
    }
    Ok(parts.join(&b'/'))
}

fn python_interpreter(word: &[u8]) -> bool {
    let name = word.rsplit(|b| *b == b'/').next().unwrap_or_default();
    let name = name.strip_suffix(b".exe").unwrap_or(name);
    [b"python".as_slice(), b"pythonw", b"pypy"]
        .iter()
        .any(|prefix| {
            name.strip_prefix(*prefix).is_some_and(|version| {
                if version.is_empty() {
                    return true;
                }
                // CPython build/ABI letters follow the numeric version, e.g.
                // python3.13t (free-threaded), python3.13d or python3.7m.
                let version = version
                    .strip_suffix(b"t")
                    .or_else(|| version.strip_suffix(b"d"))
                    .or_else(|| version.strip_suffix(b"m"))
                    .unwrap_or(version);
                version.first().is_some_and(u8::is_ascii_digit)
                    && version.iter().all(|b| b.is_ascii_digit() || *b == b'.')
            })
        })
}

fn python_shebang(blob: &[u8]) -> bool {
    let Some(line) = blob.strip_prefix(b"#!") else {
        return false;
    };
    let line = line.split(|b| *b == b'\n').next().unwrap_or_default();
    let words: Vec<_> = line
        .split(u8::is_ascii_whitespace)
        .filter(|w| !w.is_empty())
        .collect();
    let Some(interpreter) = words.first() else {
        return false;
    };
    if python_interpreter(interpreter) {
        return true;
    }
    if interpreter.rsplit(|b| *b == b'/').next() != Some(b"env") {
        return false;
    }
    // env -S splits the kernel's single shebang argument. Quotes around that
    // split string and option=value spellings must not conceal the interpreter.
    let rest = &line[line
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(0)
        + interpreter.len()..];
    let mut words = split_words(rest).into_iter();
    while let Some(word) = words.next() {
        let word = word.as_slice();
        if matches!(
            word,
            b"-u" | b"--unset" | b"-C" | b"--chdir" | b"-a" | b"--argv0"
        ) {
            words.next();
            continue;
        }
        if matches!(word, b"-S" | b"--split-string") {
            // A quoted split-string may itself contain multiple words.
            if let Some(value) = words.next() {
                words = split_words(&value)
                    .into_iter()
                    .chain(words)
                    .collect::<Vec<_>>()
                    .into_iter();
            }
            continue;
        }
        if let Some(value) = word
            .strip_prefix(b"--split-string=")
            .or_else(|| word.strip_prefix(b"-S").filter(|w| !w.is_empty()))
        {
            words = split_words(value)
                .into_iter()
                .chain(words)
                .collect::<Vec<_>>()
                .into_iter();
            continue;
        }
        if word.starts_with(b"-") || word.contains(&b'=') {
            continue;
        }
        return python_interpreter(word);
    }
    false
}

// Lex only classification data, with no shell expansion or execution. Accepting
// quotes/escapes conservatively also covers env's split-string spellings.
fn split_words(text: &[u8]) -> Vec<Vec<u8>> {
    let mut words = Vec::new();
    let mut word = Vec::new();
    let mut quote = None;
    let mut escaped = false;
    for &byte in text {
        if escaped {
            word.push(byte);
            escaped = false;
        } else if byte == b'\\' && quote != Some(b'\'') {
            escaped = true;
        } else if quote == Some(byte) {
            quote = None;
        } else if quote.is_none() && matches!(byte, b'\'' | b'"') {
            quote = Some(byte);
        } else if quote.is_none() && byte.is_ascii_whitespace() {
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
        } else {
            word.push(byte);
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}
