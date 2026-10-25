//! SQLite ownership boundary: production writers belong behind rsid-store accessors.
// guard-regressions: stdout
use crate::{
    Ctx,
    cli::{self, Opt},
    re::{self, Re},
    rust_source::{self, closing},
};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

pub const FIX: &str = "add an rsid-store accessor or derive the state from existing records";
pub const RSID: &str = "crates/rsid/src/";

/// Mask comments, strings and test items, retaining the test ranges for SQL checks.
pub fn production(source: &str, path: &str) -> Result<(String, Vec<(usize, usize)>), String> {
    let code = rust_source::mask(source, path)?;
    let name = path.rsplit('/').next().unwrap_or(path);
    if path.split('/').any(|p| p == "tests") || name == "tests.rs" || name.ends_with("_tests.rs") {
        return Ok((" ".repeat(source.len()), vec![(0, source.len())]));
    }
    let mut hidden = Vec::new();
    let attr =
        Re::new(r"#!?\[\s*cfg\(\s*test\s*\)\s*\]|#\[\s*(?:test|tokio::test)(?:\([^]]*\))?\s*\]");
    for m in attr.find_all(&code) {
        if re::group(&m, 0).starts_with("#!") {
            return Ok((" ".repeat(source.len()), vec![(0, source.len())]));
        }
        let mut at = re::end(&m);
        while at < code.len() {
            if code.as_bytes()[at].is_ascii_whitespace() {
                at += 1;
            } else if code[at..].starts_with("#[") {
                at = closing(&code, at + 1)?;
            } else {
                break;
            }
        }
        if let Some(n) = code[at..].find([';', '{']) {
            let start = at + n;
            let end = if code.as_bytes()[start] == b'{' {
                closing(&code, start)?
            } else {
                start + 1
            };
            hidden.push((re::start(&m), end));
        }
    }
    let mut bytes = code.into_bytes();
    for (start, end) in &hidden {
        for b in &mut bytes[*start..*end] {
            if *b != b'\n' {
                *b = b' ';
            }
        }
    }
    Ok((String::from_utf8(bytes).map_err(|e| e.to_string())?, hidden))
}

pub fn scan(path: &str, source: &str) -> Result<Vec<String>, String> {
    let (code, test_ranges) = production(source, path)?;
    let mut findings = Vec::new();
    let mut report = |at: usize, rule: &str| {
        findings.push(format!(
            "{path}:{}: store boundary rule: {rule}; {FIX}",
            rust_source::line(source, at)
        ))
    };
    if path.starts_with(RSID) && path != "crates/rsid/src/memory/store.rs" {
        let mut aliases = vec!["Connection".to_string()];
        aliases.extend(Re::new(r"\bConnection\s+as\s+(\w+)").findall1(&code));
        let pattern = Re::new(&format!(
            r"\b(?:rusqlite\s*::\s*)?(?:{})\s*::\s*open\w*\s*\(",
            aliases.join("|")
        ));
        for call in pattern.find_all(&code) {
            closing(&code, re::end(&call) - 1)?;
            report(
                re::start(&call),
                "rsid must not open raw SQLite connections to rsi.db",
            );
        }
    }
    if path.starts_with("crates/rsid-store/") {
        return Ok(findings);
    }
    let constant = Re::new(r#"(?s)^&?\s*(?:"(?:[^"\\]|\\.)*"|(?:[\w]+::)*[A-Z][A-Z0-9_]*)$"#);
    for call in Re::new(r"\bset_daemon_setting\s*\(").find_all(&code) {
        let end = closing(&code, re::end(&call) - 1)?;
        let mut at = re::end(&call);
        while at < end - 1 {
            match code.as_bytes()[at] {
                b'(' | b'[' | b'{' => at = closing(&code, at)?,
                b',' => break,
                _ => at += 1,
            }
        }
        if !constant.is_match(source[re::end(&call)..at].trim()) {
            report(
                re::start(&call),
                "daemon_settings keys must not be built at runtime outside rsid-store",
            );
        }
    }
    let writer = Re::new(
        r"(?i)\b(?:INSERT\s+(?:OR\s+\w+\s+)?INTO|REPLACE\s+INTO|UPDATE)\s+daemon_settings\b",
    );
    for (at, end, string) in rust_source::literals(source, path)? {
        if string
            && !test_ranges
                .iter()
                .any(|(start, end)| *start <= at && at < *end)
            && writer.is_match(&source[at..end])
        {
            report(at, "daemon_settings writes must use an rsid-store accessor");
        }
    }
    Ok(findings)
}

pub(crate) fn rust_files(root: &Path) -> Result<Vec<PathBuf>, String> {
    fn visit(root: &Path, paths: &mut Vec<PathBuf>) -> Result<(), String> {
        if !root.exists() {
            return Ok(());
        }
        let mut children = fs::read_dir(root)
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        children.sort_by_key(|e| e.path());
        for child in children {
            if child.file_type().map_err(|e| e.to_string())?.is_dir() {
                visit(&child.path(), paths)?;
            } else if child.path().extension().is_some_and(|e| e == "rs") {
                paths.push(child.path());
            }
        }
        Ok(())
    }
    let mut paths = Vec::new();
    visit(root, &mut paths)?;
    Ok(paths)
}

pub fn check(root: &Path) -> Result<Vec<String>, String> {
    let paths = if root.join(".git").exists() {
        let result = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args([
                "grep",
                "-l",
                "-z",
                "-E",
                "Connection|daemon_setting",
                "--",
                "crates/**/*.rs",
            ])
            .output()
            .map_err(|e| e.to_string())?;
        if !result.status.success() && result.status.code() != Some(1) {
            return Err(String::from_utf8_lossy(&result.stderr).into_owned());
        }
        result
            .stdout
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| root.join(crate::os_escape::path_from_bytes(s)))
            .collect()
    } else {
        rust_files(&root.join("crates"))?
    };
    let mut findings = Vec::new();
    for path in paths {
        let source = fs::read_to_string(&path).map_err(|e| e.to_string())?;
        if source.contains("Connection") || source.contains("daemon_setting") {
            let relative =
                crate::os_escape::decode_path(path.strip_prefix(root).map_err(|e| e.to_string())?)
                    .replace('\\', "/");
            findings.extend(scan(&relative, &source)?);
        }
    }
    Ok(findings)
}

pub fn main(args: &[String], ctx: &Ctx, _out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let p = match cli::parse(
        "check-store-boundary",
        "[--root ROOT]",
        args,
        &[Opt {
            name: "--root",
            value: true,
        }],
        &[],
        Some(0),
        err,
    ) {
        Ok(p) => p,
        Err(e) => return e.code(),
    };
    let root = ctx.path(p.value("--root").unwrap_or("."));
    match check(&root) {
        Ok(findings) => {
            for f in &findings {
                let _ = writeln!(err, "{}", crate::os_escape::display(f));
            }
            i32::from(!findings.is_empty())
        }
        Err(e) => {
            let _ = writeln!(err, "store boundary guard: {e}");
            2
        }
    }
}

#[cfg(test)]
#[path = "store_boundary_tests.rs"]
mod tests;
